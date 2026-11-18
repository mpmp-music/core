use super::*;

pub(crate) fn recover_inner(inner: &Inner) -> Result<()> {
    tag_job::intents::recover_inner(inner)?;
    recover_matching(inner, None).map(|_| ())
}

/// A scan retries only targets it actually observed, so an unrelated offline intent
/// cannot introduce filesystem I/O into another directory's scan.
pub(crate) fn recover_observed(
    inner: &Inner,
    observed: &[ObservedFile],
    directories: &HashSet<PathBuf>,
) -> Result<bool> {
    recover_matching(
        inner,
        Some(RecoveryScope {
            observed,
            directories,
        }),
    )
}

pub(super) struct RecoveryScope<'a> {
    observed: &'a [ObservedFile],
    directories: &'a HashSet<PathBuf>,
}

fn recover_matching(inner: &Inner, scope: Option<RecoveryScope<'_>>) -> Result<bool> {
    const SELECT_FILE_WRITE_INTENTS: &str = r#"
SELECT DISTINCT i.id,t.root_id,i.target_json FROM file_write_intents
i JOIN tracks t ON t.id=json_extract(iif(json_valid(i.target_json), i.target_json, NULL),'$.track') WHERE
i.family='cover_edit'
"#;

    let targets: Vec<(i64, crate::RootId, String)> = inner.pool.read(|conn| {
        Ok(conn
            .prepare(SELECT_FILE_WRITE_INTENTS)?
            .query_map([], |r| Ok((r.get(0)?, crate::RootId(r.get(1)?), r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?)
    })?;
    let mut leases = HashMap::new();
    let mut allowed = HashSet::new();
    let observed = scope.as_ref().map(|scope| {
        let files = scope.observed;
        (
            files
                .iter()
                .map(|(p, _)| lexical(p))
                .collect::<HashSet<_>>(),
            files.iter().filter_map(|(_, i)| *i).collect::<HashSet<_>>(),
        )
    });
    let directories: HashSet<_> = scope
        .as_ref()
        .into_iter()
        .flat_map(|scope| scope.directories.iter().map(|p| lexical(p)))
        .collect();
    for (track, root, json) in targets {
        let Ok(target) = parse::<Target>(&json) else {
            continue;
        };
        if let Some((paths, ids)) = &observed {
            let found = target
                .path
                .parent()
                .is_some_and(|parent| directories.contains(&lexical(parent)))
                || paths.contains(&lexical(&target.path))
                || target.before_identity.is_some_and(|i| ids.contains(&i))
                || [&target.staged, &target.backup, &target.rollback_after]
                    .into_iter()
                    .flatten()
                    .any(|c| ids.contains(&c.identity));
            if !found {
                continue;
            }
        }
        let lease = leases
            .entry(root)
            .or_insert_with(|| inner.file_access.acquire(root));
        if lease.is_some() {
            allowed.insert(track);
        }
    }
    let storage = inner.storage.files.clone();
    let replacements = inner.storage.replacements.clone();
    let _namespace = (!allowed.is_empty()).then(|| inner.changing_file_names());
    let changed = inner.writer.run_data(move |conn| {
        recover_selected(
            storage.as_ref(),
            replacements.as_ref(),
            conn,
            Some(&allowed),
        )
    })?;
    let restored = !changed.is_empty();
    inner
        .recovered_covers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(changed);
    refresh_recovery_flag(inner);
    Ok(restored)
}

pub(crate) fn refresh_recovery_flag(inner: &Inner) {
    if let Ok(pending) = inner.pool.read(|conn| {
        if tag_job::intents::blocks_metadata_writes(conn)? {
            return Ok(true);
        }
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM file_write_intents WHERE family='cover_edit')",
            [],
            |r| r.get::<_, bool>(0),
        )?)
    }) {
        inner
            .cover_recovery
            .store(pending, std::sync::atomic::Ordering::Release);
    }
}

pub(super) fn digest_at(storage: &dyn mpmp_storage::LibraryStorage, path: &Path) -> Option<String> {
    storage_access::open_regular(storage, path, true)
        .ok()
        .and_then(|mut f| cover_edit::file_digest(&mut f).ok())
}

pub(super) fn candidate_matches(
    storage: &dyn mpmp_storage::LibraryStorage,
    candidate: &Candidate,
    digest: &str,
) -> bool {
    identity(storage, &candidate.path) == Some(candidate.identity)
        && digest_at(storage, &candidate.path).as_deref() == Some(digest)
}

pub(super) fn backup_safe(
    storage: &dyn mpmp_storage::LibraryStorage,
    target: &Target,
    before: &str,
) -> bool {
    target
        .backup
        .as_ref()
        .is_none_or(|b| candidate_matches(storage, b, before))
}

fn recovery_io(
    step: &'static str,
    operation: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    #[cfg(all(test, feature = "desktop-fs"))]
    if RECOVERY_FAULT.with(|f| f.get()) == Some(step) && step == "remove" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "injected read-only directory",
        ));
    }
    operation()?;
    #[cfg(all(test, feature = "desktop-fs"))]
    if RECOVERY_FAULT.with(|f| f.get()) == Some(step) {
        return Err(std::io::Error::other(
            "injected synchronization failure after namespace mutation",
        ));
    }
    let _ = step;
    Ok(())
}

// Only wrap the paths already registered by the existing intent.
pub(super) fn replacement_paths(target: &Target) -> mpmp_storage::ReplacementPaths {
    mpmp_storage::ReplacementPaths {
        destination: mpmp_storage::LocalPath(target.path.clone()),
        staged: target
            .staged
            .as_ref()
            .map(|c| mpmp_storage::LocalPath(c.path.clone())),
        backup: target
            .backup
            .as_ref()
            .map(|c| mpmp_storage::LocalPath(c.path.clone())),
        saved_after: target
            .rollback_after
            .as_ref()
            .map(|c| mpmp_storage::LocalPath(c.path.clone())),
    }
}

/// Cleanup is identity- and content-bound and touches only registered candidates.
/// Unknown candidates retain their record for explicit UI resolution.
pub(super) fn cleanup(
    storage: &dyn mpmp_storage::LibraryStorage,
    replacements: &dyn mpmp_storage::DurableReplace,
    conn: &Connection,
    id: i64,
    target: &Target,
    before: &str,
    after: &str,
) -> Result<()> {
    // Even a finalized row may be cleaned much later. Keep the complete backup if
    // an external save or deletion made the live file uncertain in the meantime.
    let current = digest_at(storage, &target.path);
    if current.as_deref() != Some(before) && current.as_deref() != Some(after) {
        return Err(LibraryError::CoverRecoveryRequired);
    }
    let transaction = replacements.resume(replacement_paths(target))?;
    recovery_io("sync", || transaction.sync(&target.path))?;
    let candidates = [
        (&target.staged, after),
        (&target.backup, before),
        (&target.rollback_after, after),
    ];
    for (candidate, digest) in candidates {
        if let Some(c) = candidate
            && std::fs::symlink_metadata(&c.path).is_ok()
            && !candidate_matches(storage, c, digest)
        {
            return Err(LibraryError::CoverRecoveryRequired);
        }
    }
    for (candidate, digest) in candidates {
        if let Some(c) = candidate {
            match std::fs::symlink_metadata(&c.path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Ok(_) if candidate_matches(storage, c, digest) => {
                    recovery_io("remove", || std::fs::remove_file(&c.path))?;
                }
                _ => return Err(LibraryError::CoverRecoveryRequired),
            }
        }
    }
    recovery_io("sync", || transaction.sync(&target.path))?;
    conn.execute("DELETE FROM file_write_intents WHERE id=?1", [id])?;
    Ok(())
}

/// Recovery reads one BLOB at a time. Finalized records only clean registered candidates;
/// pending records never overwrite an existing name, including an external save.
pub(crate) fn recover(
    storage: &dyn mpmp_storage::LibraryStorage,
    replacements: &dyn mpmp_storage::DurableReplace,
    conn: &mut Connection,
) -> Result<Vec<TrackId>> {
    recover_selected(storage, replacements, conn, None)
}

// Acquire only when this row actually needs an attribute primitive. In particular,
// skipped building/retained rows with nothing to maintain need no transaction.
fn recovery_transaction<'a>(
    transaction: &'a mut Option<Box<dyn mpmp_storage::ReplacementTransaction>>,
    replacements: &dyn mpmp_storage::DurableReplace,
    paths: &mpmp_storage::ReplacementPaths,
) -> mpmp_storage::StorageResult<&'a dyn mpmp_storage::ReplacementTransaction> {
    if transaction.is_none() {
        *transaction = Some(replacements.resume(paths.clone())?);
    }
    Ok(transaction.as_deref().unwrap())
}

fn recover_selected(
    storage: &dyn mpmp_storage::LibraryStorage,
    replacements: &dyn mpmp_storage::DurableReplace,
    conn: &mut Connection,
    allowed: Option<&HashSet<i64>>,
) -> Result<Vec<TrackId>> {
    const SELECT_FILE_WRITE_INTENTS: &str = r#"
SELECT id,CAST(target_json AS BLOB),CAST(authorization_json AS BLOB),
CAST(before_digest AS BLOB),CAST(after_digest AS BLOB),stage
FROM file_write_intents WHERE family='cover_edit' AND format_version=1
AND id>?1 AND typeof(target_json)='text' AND typeof(authorization_json)='text'
AND typeof(before_digest)='text' AND typeof(after_digest)='text'
AND stage IN ('building','prepared','replaced','retained','finalized')
ORDER BY id LIMIT 1
"#;

    // An unsupported backend must leave every intent and candidate untouched.
    if !storage.write_capabilities().durable_replace
        || !replacements.supported()
        || tag_job::intents::blocks_metadata_writes(conn)?
    {
        return Ok(Vec::new());
    }
    let mut cursor = 0;
    let mut changed = Vec::new();
    loop {
        use rusqlite::OptionalExtension;
        let row = conn
            .query_row(SELECT_FILE_WRITE_INTENTS, [cursor], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                    r.get::<_, Vec<u8>>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .optional()?;
        let Some((id, t, a, before, after, stage)) = row else {
            break;
        };
        cursor = id;
        if allowed.is_some_and(|ids| !ids.contains(&id)) {
            continue;
        }
        let parsed = (
            serde_json::from_slice::<Target>(&t),
            serde_json::from_slice::<Authorization>(&a),
            String::from_utf8(before),
            String::from_utf8(after),
        );
        let (Ok(mut target), Ok(auth), Ok(before), Ok(after)) = parsed else {
            tracing::warn!(
                intent = id,
                "unreadable cover intent retained without recovery"
            );
            continue;
        };
        let paths = replacement_paths(&target);
        let mut transaction = None;
        // Preserve the desktop attribute maintenance that precedes stage filtering,
        // acquiring a transaction only if an actual attribute call needs it.
        let attributes: Result<()> = (|| {
            if let Some(baseline) = target.baseline.as_mut()
                && baseline.hidden.is_none()
            {
                baseline.hidden = recovery_transaction(&mut transaction, replacements, &paths)?
                    .hidden(&target.path)
                    .ok()
                    .or_else(|| {
                        target
                            .backup
                            .as_ref()
                            .and_then(|b| transaction.as_deref().unwrap().hidden(&b.path).ok())
                    });
                conn.execute(
                    "UPDATE file_write_intents SET target_json=?2 WHERE id=?1",
                    params![id, json(&target)?],
                )?;
            }
            for candidate in [&target.backup, &target.staged, &target.rollback_after]
                .into_iter()
                .flatten()
            {
                if identity(storage, &candidate.path) == Some(candidate.identity)
                    && identity(storage, &candidate.path) != identity(storage, &target.path)
                {
                    let _ = recovery_transaction(&mut transaction, replacements, &paths)?
                        .hide_candidate(&candidate.path);
                }
            }
            Ok(())
        })();
        match attributes {
            Ok(()) => {}
            Err(error @ LibraryError::Storage(_)) => {
                tracing::warn!(intent=id, %error, "cover recovery attributes remain pending");
                continue;
            }
            Err(error) => return Err(error),
        }
        if stage == "finalized" {
            if let Err(error) = cleanup(storage, replacements, conn, id, &target, &before, &after) {
                tracing::warn!(intent=id, %error, "cover recovery cleanup remains pending");
            }
            continue;
        }
        if stage == "building" {
            // No durable content digest exists for a partially built candidate. Even
            // the same inode may now contain somebody else's complete file.
            tracing::warn!(
                intent = id,
                "cover recovery requires explicit resolution of building candidate"
            );
            continue;
        }
        if stage == "retained" {
            continue;
        }
        let transaction = match transaction {
            Some(transaction) => transaction,
            None => match replacements.resume(replacement_paths(&target)) {
                Ok(transaction) => transaction,
                Err(error) => {
                    tracing::warn!(intent=id, %error, "cover recovery transaction remains pending");
                    continue;
                }
            },
        };
        let mut current = digest_at(storage, &target.path);
        if current.is_none()
            && std::fs::symlink_metadata(&target.path)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            // Prefer the complete original. If it is unavailable, a complete staged
            // result can restore availability, but a missing original backup stays pending.
            let candidate = target
                .backup
                .as_ref()
                .filter(|c| candidate_matches(storage, c, &before))
                .or_else(|| {
                    target
                        .staged
                        .as_ref()
                        .filter(|c| candidate_matches(storage, c, &after))
                })
                .or_else(|| {
                    target
                        .rollback_after
                        .as_ref()
                        .filter(|c| candidate_matches(storage, c, &after))
                });
            if let Some(c) = candidate {
                if recovery_io("publish", || {
                    transaction.publish_missing(&c.path, &target.path)
                })
                .is_err()
                {
                    tracing::warn!(
                        intent = id,
                        "cover recovery publication or synchronization failed"
                    );
                    continue;
                }
                current = digest_at(storage, &target.path);
                if (current.as_ref() == Some(&before) || current.as_ref() == Some(&after))
                    && let Some(hidden) = target.baseline.as_ref().and_then(|b| b.hidden)
                    && let Err(error) = transaction.set_hidden(&target.path, hidden)
                {
                    tracing::warn!(intent = id, %error, "cover recovery attributes remain pending");
                    continue;
                }
                if current.as_ref() == Some(&before) || current.as_ref() == Some(&after) {
                    changed.push(TrackId(target.track));
                }
            }
        }
        if current.as_ref() == Some(&before) {
            if let Err(error) = cleanup(storage, replacements, conn, id, &target, &before, &after) {
                tracing::warn!(intent=id, %error, "cover recovery cleanup remains pending");
            }
        } else if current.as_ref() == Some(&after) && backup_safe(storage, &target, &before) {
            if let Some(c) = &target.staged
                && c.path.exists()
                && !candidate_matches(storage, c, &after)
            {
                // Exchange may have completed just before a crash. The old inode is
                // then present at BOTH staged and backup. Only this known duplicate
                // can be discarded automatically; unknown displaced files stay put.
                let duplicate = target.backup.as_ref().is_some_and(|b| {
                    (identity(storage, &c.path) == Some(b.identity)
                        || identity(storage, &c.path) == target.before_identity)
                        && digest_at(storage, &c.path).as_ref() == Some(&before)
                });
                if !duplicate
                    || recovery_io("sync", || transaction.sync(&target.path)).is_err()
                    || recovery_io("sync", || {
                        transaction.sync(&target.backup.as_ref().unwrap().path)
                    })
                    .is_err()
                    || recovery_io("remove", || std::fs::remove_file(&c.path)).is_err()
                    || recovery_io("sync", || transaction.sync(&target.path)).is_err()
                {
                    tracing::warn!(intent = id, "displaced/staged candidate remains pending");
                    continue;
                }
            }
            if transaction.sync(&target.path).is_err() {
                continue;
            }
            if let (Some(backup), Some(_staged)) = (&target.backup, &target.staged)
                && (transaction.sync(&backup.path).is_err()
                    || transaction.seal().is_err()
                    || transaction.verify_attributes().is_err())
            {
                continue;
            }
            if probe_staged(&target.path, &target.path).is_err() {
                continue;
            }
            // Bound even old or corrupt databases before allocating their BLOBs.
            let bytes: i64 = conn.query_row(
                "SELECT length(before_image) FROM file_write_intents WHERE id=?1",
                [id],
                |r| r.get(0),
            )?;
            if bytes < 0 || bytes as u64 > MAX_COVER_UNDO_BYTES as u64 {
                continue;
            }
            let blob: Vec<u8> = conn.query_row(
                "SELECT before_image FROM file_write_intents WHERE id=?1",
                [id],
                |r| r.get(0),
            )?;
            let Some(journal::Inverse::Covers(mut files)) = journal::Inverse::decode(&blob) else {
                continue;
            };
            if files.len() != 1 || files[0].track != target.track {
                continue;
            }
            let intent = Intent {
                id,
                target,
                auth,
                before,
                after,
                inverse: files.remove(0).regions,
            };
            // One unreadable or conflicting file must not freeze its healthy siblings.
            if finalize(storage, conn, std::slice::from_ref(&intent)).is_ok() {
                changed.push(TrackId(intent.target.track));
                let _ = cleanup(
                    storage,
                    replacements,
                    conn,
                    id,
                    &intent.target,
                    &intent.before,
                    &intent.after,
                );
            }
        }
    }
    Ok(changed)
}

pub(super) fn finalize(
    storage: &dyn mpmp_storage::LibraryStorage,
    conn: &mut Connection,
    intents: &[Intent],
) -> Result<ActionId> {
    const UPDATE_TRACKS: &str = r#"
UPDATE tracks SET size=?2,mtime_ms=?3,has_embedded_art=?4,cover_revision=?5,has_folder_art=?6,metadata_revision=metadata_revision+1
WHERE id=?1
"#;

    const UPDATE_FILE_WRITE_INTENTS: &str = r#"
UPDATE file_write_intents SET stage='finalized',action_id=?2,updated_at=?3
WHERE id=?1
"#;

    let tx = conn.transaction()?;
    let mut files = Vec::new();
    let first = &intents[0];
    for intent in intents {
        let target = &intent.target;
        let mut file = storage_access::open_regular(storage, &target.path, true)?;
        if cover_edit::file_digest(&mut file)? != intent.after {
            return Err(LibraryError::CoverRecoveryRequired);
        }
        let (size, mtime) = tag_job::stamp(&file.metadata()?);
        if !backup_safe(storage, target, &intent.before) {
            return Err(LibraryError::CoverRecoveryRequired);
        }
        let art = crate::cover::read_cover(&target.path).ok().flatten();
        let embedded = crate::cover::embedded_cover(&target.path)
            .ok()
            .flatten()
            .is_some();
        let folder = target
            .path
            .parent()
            .and_then(|p| crate::cover::folder_cover(p).ok().flatten())
            .is_some();
        let revision = art.as_ref().map(|a| cover_edit::digest(&a.data));
        tx.execute(
            UPDATE_TRACKS,
            params![target.track, size as i64, mtime, embedded, revision, folder],
        )?;
        if let Ok(guard) = crate::cover::picture_guard(&target.path)
            && let Some(baseline) = &target.baseline
        {
            journal::cover_restamp(
                &tx,
                target.track,
                baseline.stamp,
                baseline.picture,
                (size, mtime),
                guard,
                &intent.after,
            )?;
        }
        files.push(FileCover {
            track: target.track,
            regions: intent.inverse.clone(),
            expected: intent.after.clone(),
            restored_digest: intent.before.clone(),
        });
    }
    crate::db::schema::refresh_aggregates(&tx)?;
    let action = journal::record_cover(
        &tx,
        first.auth.meta(),
        files,
        &first.target.operation,
        first.target.undo_of,
        first.target.at,
        &first.target.group,
    )?;
    for intent in intents {
        tx.execute(
            UPDATE_FILE_WRITE_INTENTS,
            params![intent.id, action.0, now_ms()],
        )?;
    }
    tx.commit()?;
    Ok(action)
}
