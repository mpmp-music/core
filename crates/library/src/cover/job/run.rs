use super::*;

pub(super) fn check(ids: &[TrackId], change: &CoverChange) -> Result<()> {
    if ids.len() > MAX_COVER_EDIT_TRACKS {
        return Err(LibraryError::invalid(
            crate::Invalid::OutOfRange,
            "too many cover targets",
        ));
    }
    change.validate().map_err(LibraryError::CoverInput)
}

#[cfg(all(test, feature = "desktop-fs"))]
fn fault(inner: &Inner, stage: &'static str) -> Result<()> {
    if *inner.hooks.cover_fault.lock().unwrap() == Some(stage) {
        if inner
            .hooks
            .cover_abort
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            std::process::abort();
        }
        return Err(LibraryError::Io(std::io::Error::other(format!(
            "injected crash: {stage}"
        ))));
    }
    Ok(())
}

#[cfg(not(all(test, feature = "desktop-fs")))]
fn fault(_: &Inner, _: &'static str) -> Result<()> {
    Ok(())
}

fn rollback_before(inner: &Inner, id: i64, target: &Target, before: &str, after: &str) -> bool {
    let Ok(transaction) = inner.storage.replacements.resume(replacement_paths(target)) else {
        return false;
    };
    if digest_at(inner.storage.files.as_ref(), &target.path).as_deref() != Some(before) {
        let Some(backup) = &target.backup else {
            return false;
        };
        if !candidate_matches(inner.storage.files.as_ref(), backup, before) {
            return false;
        }
        if !target.path.exists() {
            if transaction
                .publish_missing(&backup.path, &target.path)
                .is_err()
            {
                return false;
            }
            if let Some(hidden) = target.baseline.as_ref().and_then(|b| b.hidden)
                && transaction.set_hidden(&target.path, hidden).is_err()
            {
                return false;
            }
        } else {
            if digest_at(inner.storage.files.as_ref(), &target.path).as_deref() != Some(after) {
                return false;
            }
            let Some(_staged) = &target.staged else {
                return false;
            };
            let Some(after_copy) = &target.rollback_after else {
                return false;
            };
            let mut saved_target = target.clone();
            if !after_copy.path.exists() {
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let Ok(mut saved) = options.open(&after_copy.path) else {
                    return false;
                };
                let Ok(saved_id) = inner.storage.files.as_ref().opened_identity(&saved) else {
                    return false;
                };
                saved_target.rollback_after.as_mut().unwrap().identity = saved_id.parts();
                let registered = saved_target.clone();
                if inner
                    .writer
                    .run(move |conn| {
                        conn.execute(
                            "UPDATE file_write_intents SET target_json=?2 WHERE id=?1",
                            params![id, json(&registered)?],
                        )?;
                        Ok(())
                    })
                    .is_err()
                {
                    return false;
                }
                let Ok(mut source) =
                    storage_access::open_regular(inner.storage.files.as_ref(), &target.path, true)
                else {
                    return false;
                };
                if std::io::copy(&mut source, &mut saved).is_err()
                    || transaction
                        .prepare_attributes(&after_copy.path, &target.path)
                        .is_err()
                    || saved.sync_all().is_err()
                {
                    return false;
                }
            }
            let registered = saved_target.rollback_after.as_ref().unwrap();
            if !candidate_matches(inner.storage.files.as_ref(), registered, after)
                || transaction.sync(&registered.path).is_err()
                || digest_at(inner.storage.files.as_ref(), &target.path).as_deref() != Some(after)
            {
                return false;
            }
            let rolled_back = transaction.rollback();
            if rolled_back.is_err() {
                return false;
            }
            if let Some(hidden) = target.baseline.as_ref().and_then(|b| b.hidden)
                && transaction.set_hidden(&target.path, hidden).is_err()
            {
                return false;
            }

            // Cleanup must use the durably registered identity of the after copy.
            return rollback_before(inner, id, &saved_target, before, after);
        }
    }
    if digest_at(inner.storage.files.as_ref(), &target.path).as_deref() != Some(before) {
        return false;
    }
    let target = target.clone();
    let before = before.to_owned();
    let after = after.to_owned();
    let storage = inner.storage.files.clone();
    let replacements = inner.storage.replacements.clone();
    inner
        .writer
        .run(move |conn| {
            cleanup(
                storage.as_ref(),
                replacements.as_ref(),
                conn,
                id,
                &target,
                &before,
                &after,
            )
        })
        .is_ok()
}

pub(super) fn probe_staged(
    staged: &Path,
    original: &Path,
) -> std::result::Result<(), &'static str> {
    let source = mpmp_media_io::FileSource::open(staged).map_err(|_| "cover_edit_unsupported")?;
    mpmp_engine::probe_audio(
        Arc::new(source),
        original.extension().and_then(|s| s.to_str()),
    )
    .map(|_| ())
    .map_err(|_| "cover_edit_unsupported")
}

fn pause_before_replace(_inner: &Inner) {
    #[cfg(all(test, feature = "desktop-fs"))]
    if let Some(hook) = _inner.hooks.cover_before_replace.lock().unwrap().as_ref() {
        hook();
    }
}

// Keep the File barrier at its original call site; the test observer records the
// actual sync attempt alongside the injected transaction's attribute call.
fn sync_staged(_inner: &Inner, temp: &std::fs::File) -> std::io::Result<()> {
    #[cfg(all(test, feature = "desktop-fs"))]
    if let Some(observe) = _inner.hooks.cover_temp_sync.lock().unwrap().as_ref() {
        observe();
    }
    temp.sync_all()
}

pub(super) fn run(
    inner: &Inner,
    ids: &[TrackId],
    change: Option<&CoverChange>,
    undo_of: Option<ActionId>,
    meta: &OwnedMeta,
    options: TagJobOptions,
) -> Result<CoverEditReport> {
    const SELECT_FILE_WRITE_INTENTS: &str = r#"
SELECT coalesce(sum(length(before_image)),0)
FROM file_write_intents WHERE family='cover_edit'
"#;

    const INSERT_FILE_WRITE_INTENTS: &str = r#"
INSERT INTO file_write_intents(family,format_version,authorization_json,target_json,before_digest,before_image,after_digest,stage,created_at,updated_at)
VALUES('cover_edit',1,?1,?2,?3,zeroblob(?4),'','building',?5,?5)
"#;

    const UPDATE_FILE_WRITE_INTENTS: &str = r#"
UPDATE file_write_intents SET before_image=?2,after_digest=?3,stage='prepared',updated_at=?4
WHERE id=?1
"#;

    const SELECT_ACTIONS: &str = r#"
SELECT EXISTS(SELECT 1 FROM actions a JOIN action_targets t ON t.action_id=a.id
WHERE json_extract(a.details_json,'$.cover_group')=?1 AND t.kind='track_cover'
AND t.object_id=?2)
"#;

    const SELECT_ACTIONS_2: &str = r#"
SELECT id FROM actions WHERE json_extract(details_json,'$.cover_group')=?1
ORDER BY id LIMIT 1
"#;

    let _serial = inner.tag_jobs.lock().unwrap_or_else(|e| e.into_inner());
    let _recovery_flag = RecoveryFlag(inner);
    recover_inner(inner)?;
    inner.pool.read(tag_job::intents::require_readable)?;
    let authorized_at = now_ms();
    let undo = if let Some(id) = undo_of {
        let expected = options.undo_guard.clone();
        inner
            .writer
            .run(move |conn| {
                if let Some(expected) = expected {
                    journal::check_undo_description(conn, &expected, now_ms())?;
                }
                journal::cover_undo(conn, id, now_ms())
            })?
            .ok_or_else(|| LibraryError::Corrupt("cover undo".into()))?
    } else {
        vec![]
    };
    let ids: Vec<_> = if undo_of.is_some() {
        undo.iter().map(|f| TrackId(f.track)).collect()
    } else {
        ids.to_vec()
    };
    let refs = tag_job::file_refs(inner, &ids)?;
    let checked = meta.meta().checked;
    let empty = HashMap::new();
    let locations = checked.then(|| options.checked.as_ref().unwrap_or(&empty));
    // Undo is preflighted as a complete batch before the first replacement.
    let mut leases = Vec::new();
    let mut conflicts = Vec::new();
    let mut busy = Vec::new();
    if let Some(action) = undo_of {
        for file in &refs {
            leases.push(
                inner
                    .file_access
                    .acquire(file.track.root_id)
                    .ok_or(LibraryError::RootReleased(file.track.root_id))?,
            );
            if pending(inner, file.track.id)? {
                return Err(LibraryError::CoverRecoveryRequired);
            }
            let expected = undo.iter().find(|f| f.track == file.track.id.0).unwrap();
            match tag_job::locate(inner.storage.files.as_ref(), file, locations) {
                Located::File { path, .. } => {
                    if tag_job::intents::pending(inner, file.track.id, &path)?
                        || storage_access::open_regular(inner.storage.files.as_ref(), &path, true)
                            .ok()
                            .and_then(|mut f| cover_edit::file_digest(&mut f).ok())
                            .as_ref()
                            != Some(&expected.expected)
                    {
                        conflicts.push(journal::ActionTarget::Track { id: file.track.id });
                    }
                    if (options.in_use)(file.track.id) {
                        busy.push(file.track.id);
                    }
                }
                Located::Fail("location_changed") => {
                    return Err(storage_access::location_changed(Path::new(
                        &file.track.rel_path,
                    )));
                }
                _ => conflicts.push(journal::ActionTarget::Track { id: file.track.id }),
            }
        }
        if !conflicts.is_empty() {
            return Err(LibraryError::ActionConflict {
                action,
                changed: conflicts,
            });
        }
        if !busy.is_empty() {
            return Err(LibraryError::InUse(busy));
        }
    }
    let require_sources = options.cover_sources.is_some();
    let mut expected_sources: HashMap<_, _> = options
        .cover_sources
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.track, s))
        .collect();
    let auth = Authorization {
        origin: meta.meta().origin.into(),
        grant: meta.meta().grant,
        checked,
    };
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|e| LibraryError::Io(std::io::Error::other(e.to_string())))?;
    let group = format!("{:032x}", u128::from_le_bytes(nonce));
    let mut report = CoverEditReport::default();
    let mut intents = Vec::new();
    let mut write_leases = Vec::new();
    let mut left = inner.pool.read(|conn| {
        let used: i64 = conn.query_row(
            "SELECT coalesce(sum(length(before_image)),0) FROM file_write_intents WHERE family='cover_edit'",
            [],
            |r| r.get(0),
        )?;
        Ok(MAX_COVER_UNDO_BYTES.saturating_sub(used.max(0) as usize))
    })?;
    for (i, file) in refs.iter().enumerate() {
        let id = file.track.id;
        let _lease = inner.file_access.acquire(file.track.root_id);
        let result = (|| -> std::result::Result<Option<Intent>, &'static str> {
            if _lease.is_none() {
                return Err("root_released");
            }
            if pending(inner, id).map_err(|_| "cover_recovery_required")? {
                return Err("cover_recovery_required");
            }
            let (path, stamp) = match tag_job::locate(inner.storage.files.as_ref(), file, locations)
            {
                Located::File { path, stamp } => (path, stamp),
                Located::Skip(c) | Located::Fail(c) => return Err(c),
            };
            if tag_job::intents::pending(inner, id, &path).map_err(|_| "file_changed")?
                || undo_of.is_none() && stamp != (file.track.size, file.track.mtime_ms)
            {
                return Err("file_changed");
            }
            if (options.in_use)(id) {
                return Err("file_in_use");
            }
            let mut source =
                storage_access::open_regular(inner.storage.files.as_ref(), &path, true)
                    .map_err(|_| "location_changed")?;
            let before = cover_edit::file_digest(&mut source).map_err(|_| "open_failed")?;
            if require_sources && !expected_sources.contains_key(&id) {
                return Err("file_changed");
            }
            if let Some(expected) = expected_sources.remove(&id) {
                let same = expected.path == path
                    && expected.digest == before
                    && inner
                        .storage
                        .files
                        .as_ref()
                        .opened_identity(&source)
                        .is_ok_and(|current| expected.identity == current);
                if !same {
                    return Err("file_changed");
                }
            }
            let regions = if let Some(change) = change {
                cover_edit::prepare(&mut source, change)?.regions
            } else {
                let undo = undo.iter().find(|f| f.track == id.0).unwrap();
                if undo.expected != before {
                    return Err("file_changed");
                }
                undo.regions.clone()
            };
            if regions.is_empty() {
                return Ok(None);
            }
            let bytes = regions.iter().fold(256usize, |sum, r| {
                sum.saturating_add((r.replaced as usize).max(r.bytes.len()))
                    .saturating_add(32)
            });
            if bytes > left {
                return Err("undo_too_large");
            }
            if source
                .metadata()
                .map_err(|_| "open_failed")?
                .permissions()
                .readonly()
            {
                return Err("file_read_only");
            }
            let source_id = inner
                .storage
                .files
                .as_ref()
                .opened_id(&source)
                .map_err(|_| "open_failed")?;
            let transaction = inner
                .storage
                .replacements
                .begin(mpmp_storage::ReplacementPaths {
                    destination: mpmp_storage::LocalPath(path.clone()),
                    staged: Some(mpmp_storage::LocalPath(
                        path.with_file_name(format!(".cover-stage-{group}-{i}")),
                    )),
                    backup: Some(mpmp_storage::LocalPath(
                        path.with_file_name(format!(".cover-backup-{group}-{i}")),
                    )),
                    saved_after: Some(mpmp_storage::LocalPath(
                        path.with_file_name(format!(".cover-after-{group}-{i}")),
                    )),
                })
                .map_err(|_| "open_failed")?;
            let baseline = Baseline {
                hidden: transaction.hidden(&path).ok(),
                stamp,
                picture: crate::cover::picture_guard(&path).ok(),
            };
            if fault(inner, "before_intent").is_err() {
                return Err("injected_crash");
            }
            let source_identity = inner
                .storage
                .files
                .as_ref()
                .opened_identity(&source)
                .map_err(|_| "open_failed")?
                .parts();
            let staged_path = path.with_file_name(format!(".cover-stage-{group}-{i}"));
            let mut target = Target {
                track: id.0,
                path: path.clone(),
                group: group.clone(),
                operation: change.map_or("restore", CoverChange::operation).into(),
                undo_of,
                at: authorized_at,
                staged: Some(Candidate {
                    path: staged_path.clone(),
                    identity: [0, 0],
                    retained_digest: None,
                    copied: false,
                }),
                backup: Some(Candidate {
                    path: path.with_file_name(format!(".cover-backup-{group}-{i}")),
                    identity: source_identity,
                    retained_digest: None,
                    copied: false,
                }),
                rollback_after: Some(Candidate {
                    path: path.with_file_name(format!(".cover-after-{group}-{i}")),
                    identity: [0, 0],
                    retained_digest: None,
                    copied: true,
                }),
                baseline: Some(baseline),
                before_identity: Some(source_identity),
            };
            // Register the candidate name and reserve its persistent budget before even
            // creating it. A crash while copying cannot leave an unregistered full file.
            let (a, t, b) = (
                json(&auth).map_err(|_| "write_failed")?,
                json(&target).map_err(|_| "write_failed")?,
                before.clone(),
            );
            let intent_id = inner
                .writer
                .run(move |conn| {
                    let tx = conn.transaction()?;
                    let used: i64 = tx.query_row(SELECT_FILE_WRITE_INTENTS, [], |r| r.get(0))?;
                    if used.max(0) as u64 + bytes as u64 > MAX_COVER_UNDO_BYTES as u64 {
                        return Err(LibraryError::CoverInput("undo_too_large"));
                    }
                    tx.execute(
                        INSERT_FILE_WRITE_INTENTS,
                        params![a, t, b, bytes as i64, now_ms()],
                    )?;
                    let id = tx.last_insert_rowid();
                    tx.commit()?;
                    Ok(id)
                })
                .map_err(|e| {
                    if e.code() == "undo_too_large" {
                        "undo_too_large"
                    } else {
                        "write_failed"
                    }
                })?;
            left -= bytes;
            let mut create = std::fs::OpenOptions::new();
            create.create_new(true).read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                create.mode(0o600);
            }
            let mut temp = create
                .open(&staged_path)
                .map_err(|_| "cover_recovery_required")?;
            transaction
                .hide_candidate(&staged_path)
                .map_err(|_| "write_failed")?;
            if fault(inner, "after_stage_created").is_err() {
                return Err("injected_crash");
            }
            target.staged.as_mut().unwrap().identity = inner
                .storage
                .files
                .as_ref()
                .opened_identity(&temp)
                .map_err(|_| "write_failed")?
                .parts();
            let t = json(&target).map_err(|_| "write_failed")?;
            inner
                .writer
                .run(move |conn| {
                    conn.execute(
                        "UPDATE file_write_intents SET target_json=?2 WHERE id=?1",
                        params![intent_id, t],
                    )?;
                    Ok(())
                })
                .map_err(|_| "cover_recovery_required")?;
            if fault(inner, "during_staging").is_err() {
                return Err("injected_crash");
            }
            let inverse = cover_edit::stage(&mut source, &mut temp, &regions)?;
            if let Some(change) = change {
                let verified = cover_edit::prepare(&mut temp, change)?;
                if !verified.regions.is_empty() {
                    return Err("cover_edit_unsupported");
                }
                if let CoverChange::Set { data, .. } = change {
                    let readable = crate::cover::embedded_cover(&staged_path)
                        .map_err(|_| "cover_edit_unsupported")?
                        .ok_or("cover_edit_unsupported")?;
                    if readable.data != *data {
                        return Err("cover_edit_unsupported");
                    }
                }
            }
            let after = cover_edit::file_digest(&mut temp).map_err(|_| "write_failed")?;
            if cover_edit::patched_digest(&mut temp, &inverse)? != before {
                return Err("file_changed");
            }
            if let Some(undo) = undo.iter().find(|f| f.track == id.0)
                && !undo.restored_digest.is_empty()
                && after != undo.restored_digest
            {
                return Err("file_changed");
            }
            probe_staged(&staged_path, &path)?;
            transaction
                .prepare_attributes(&staged_path, &path)
                .map_err(|_| "write_failed")?;
            sync_staged(inner, &temp).map_err(|_| "write_failed")?;
            drop(temp);
            drop(source);
            let blob = journal::Inverse::Covers(vec![FileCover {
                track: id.0,
                expected: after.clone(),
                restored_digest: before.clone(),
                regions: inverse.clone(),
            }])
            .encode();
            if blob.len() > bytes {
                return Err("undo_too_large");
            }
            let n = after.clone();
            inner
                .writer
                .run(move |conn| {
                    let tx = conn.transaction()?;
                    tx.execute(
                        UPDATE_FILE_WRITE_INTENTS,
                        params![intent_id, blob, n, now_ms()],
                    )?;
                    tx.commit()?;
                    Ok(())
                })
                .map_err(|_| "cover_recovery_required")?;
            if fault(inner, "after_intent").is_err() {
                return Err("injected_crash");
            }
            let mut current = transaction.open_source().map_err(|_| "location_changed")?;
            let same = inner
                .storage
                .files
                .as_ref()
                .opened_id(&current)
                .is_ok_and(move |now| now == source_id);
            if !same
                || tag_job::stamp(&current.metadata().map_err(|_| "file_changed")?) != stamp
                || cover_edit::file_digest(&mut current).map_err(|_| "file_changed")? != before
            {
                return Err("file_changed");
            }
            if current
                .metadata()
                .map_err(|_| "write_failed")?
                .permissions()
                .readonly()
            {
                return Err("file_read_only");
            }
            storage_access::ensure_canonical(inner.storage.files.as_ref(), &path)
                .map_err(|_| "location_changed")?;
            match tag_job::locate(inner.storage.files.as_ref(), file, locations) {
                Located::File { path: now, .. } if now == path => {}
                _ => return Err("location_changed"),
            }
            if fault(inner, "before_rename").is_err() {
                return Err("injected_crash");
            }
            pause_before_replace(inner);
            #[cfg(unix)]
            {
                let mut register =
                    |identity: mpmp_storage::FileIdentity, copied: bool| -> std::io::Result<()> {
                        target.backup.as_mut().unwrap().identity = identity.parts();
                        target.backup.as_mut().unwrap().copied = copied;
                        let saved = json(&target).map_err(std::io::Error::other)?;
                        inner
                            .writer
                            .run(move |conn| {
                                conn.execute(
                                    "UPDATE file_write_intents SET target_json=?2 WHERE id=?1",
                                    params![intent_id, saved],
                                )?;
                                Ok(())
                            })
                            .map_err(std::io::Error::other)
                    };
                let prepared = transaction.prepare_backup(&mut register);
                if prepared.is_err() {
                    return Err(
                        if rollback_before(inner, intent_id, &target, &before, &after) {
                            "write_failed"
                        } else {
                            "cover_recovery_required"
                        },
                    );
                }
                if !backup_safe(inner.storage.files.as_ref(), &target, &before) {
                    return Err("cover_recovery_required");
                }
            }
            #[cfg(unix)]
            let preparation = mpmp_storage::BackupPreparation::Registered {
                copied: target.backup.as_ref().unwrap().copied,
            };
            #[cfg(not(unix))]
            let preparation = mpmp_storage::BackupPreparation::DuringPublish;
            let _namespace = inner.changing_file_names();
            let replaced = transaction.publish(preparation);
            drop(current);
            for candidate in [&target.backup, &target.staged, &target.rollback_after]
                .into_iter()
                .flatten()
            {
                if identity(inner.storage.files.as_ref(), &candidate.path)
                    == Some(candidate.identity)
                    && identity(inner.storage.files.as_ref(), &candidate.path)
                        != identity(inner.storage.files.as_ref(), &target.path)
                {
                    let _ = transaction.hide_candidate(&candidate.path);
                }
            }
            // Inspect actual candidates even when the OS reports a partial failure.
            // A changed displaced file is never cleaned or finalized.
            let published = digest_at(inner.storage.files.as_ref(), &path).as_ref() == Some(&after);
            if replaced.is_err() {
                return Err(
                    if rollback_before(inner, intent_id, &target, &before, &after) {
                        if replaced
                            .as_ref()
                            .is_err_and(|e| e.kind() == std::io::ErrorKind::Unsupported)
                        {
                            "cover_edit_unsupported"
                        } else {
                            "write_failed"
                        }
                    } else if digest_at(inner.storage.files.as_ref(), &path).as_ref()
                        == Some(&after)
                    {
                        "written_pending_recovery"
                    } else {
                        "cover_recovery_required"
                    },
                );
            }
            if !published || !backup_safe(inner.storage.files.as_ref(), &target, &before) {
                return Err(if published || replaced.is_ok() {
                    "written_pending_recovery"
                } else {
                    "cover_recovery_required"
                });
            }
            if fault(inner, "after_rename").is_err() {
                return Err("injected_crash");
            }
            if replaced.is_err()
                || transaction
                    .sync(&target.backup.as_ref().unwrap().path)
                    .is_err()
                || transaction.seal().is_err()
                || transaction.verify_attributes().is_err()
            {
                return Err(
                    if rollback_before(inner, intent_id, &target, &before, &after) {
                        "write_failed"
                    } else if digest_at(inner.storage.files.as_ref(), &path).as_ref()
                        == Some(&after)
                    {
                        "written_pending_recovery"
                    } else {
                        "cover_recovery_required"
                    },
                );
            }
            if probe_staged(&path, &path).is_err() || fault(inner, "probe_failed").is_err() {
                if rollback_before(inner, intent_id, &target, &before, &after) {
                    return Err("cover_edit_unsupported");
                }
                return Err("cover_recovery_required");
            }
            if inner
                .writer
                .run(move |conn| {
                    conn.execute(
                        "UPDATE file_write_intents SET stage='replaced',updated_at=?2 WHERE id=?1",
                        params![intent_id, now_ms()],
                    )?;
                    Ok(())
                })
                .is_err()
            {
                return Err("written_pending_recovery");
            }
            Ok(Some(Intent {
                id: intent_id,
                target,
                auth: auth.clone(),
                before,
                after,
                inverse,
            }))
        })();
        match result {
            Ok(Some(intent)) => {
                report.written.push(id);
                intents.push(intent);
            }
            Ok(None) => report.unchanged.push(id),
            Err("written_pending_recovery") => {
                report.written.push(id);
                report.failed.push((id, "cover_recovery_required"));
            }
            Err("injected_crash") => {
                return Err(LibraryError::Io(std::io::Error::other("injected crash")));
            }
            Err(
                c @ ("cue_track" | "file_changed" | "file_in_use" | "file_missing"
                | "root_released" | "track_unavailable" | "rescan_needed"),
            ) => report.skipped.push((id, c)),
            Err(c) => report.failed.push((id, c)),
        }
        write_leases.extend(_lease);
        (options.progress)(i as u64 + 1, refs.len() as u64);
    }
    if !intents.is_empty() {
        fault(inner, "before_action")?;
        // Readback and commit each file independently. record_cover joins the durable
        // group into a single action, including recovery after a partial finalize.
        for intent in intents {
            let clean_target = intent.target.clone();
            let (id, before, after) = (intent.id, intent.before.clone(), intent.after.clone());
            let storage = inner.storage.files.clone();
            match inner
                .writer
                .run_data(move |conn| finalize(storage.as_ref(), conn, &[intent]))
            {
                Ok(action) => {
                    report.action = Some(action);
                    fault(inner, "after_action")?;
                    let storage = inner.storage.files.clone();
                    let replacements = inner.storage.replacements.clone();
                    let _ = inner.writer.run(move |conn| {
                        cleanup(
                            storage.as_ref(),
                            replacements.as_ref(),
                            conn,
                            id,
                            &clean_target,
                            &before,
                            &after,
                        )
                    });
                }
                Err(_) => report
                    .failed
                    .push((TrackId(clean_target.track), "cover_recovery_required")),
            }
        }
    }
    if report
        .failed
        .iter()
        .any(|(id, code)| *code == "cover_recovery_required" && report.written.contains(id))
    {
        let _ = recover_inner(inner);
        let mut failures = Vec::new();
        for (id, code) in std::mem::take(&mut report.failed) {
            if code != "cover_recovery_required" || pending(inner, id).unwrap_or(true) {
                failures.push((id, code));
                continue;
            }
            let group = group.clone();
            let committed = inner
                .pool
                .read(|conn| {
                    Ok(conn.query_row(SELECT_ACTIONS, params![group, id.0], |r| {
                        r.get::<_, bool>(0)
                    })?)
                })
                .unwrap_or(false);
            if !committed {
                // Recovery restored before instead of completing the new write.
                report.written.retain(|written| *written != id);
                failures.push((id, "write_failed"));
            }
        }
        report.failed = failures;
        use rusqlite::OptionalExtension;
        if let Ok(action) = inner.pool.read(|conn| {
            Ok(conn
                .query_row(SELECT_ACTIONS_2, [&group], |r| r.get::<_, i64>(0))
                .optional()?)
        }) {
            report.action = action.map(ActionId);
        }
    }
    Ok(report)
}
