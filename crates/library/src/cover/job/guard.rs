use super::*;

pub(super) fn recovery_guard(
    storage: &dyn mpmp_storage::LibraryStorage,
    conn: &Connection,
    track: TrackId,
) -> Result<CoverRecoveryGuard> {
    recovery_guard_for(
        storage,
        conn,
        track,
        CoverRecoveryResolution::AcceptCurrent,
        None,
    )
}

pub(super) fn recovery_guard_for(
    storage: &dyn mpmp_storage::LibraryStorage,
    conn: &Connection,
    track: TrackId,
    resolution: CoverRecoveryResolution,
    selection: Option<&CoverRecoverySelection>,
) -> Result<CoverRecoveryGuard> {
    const SELECT_FILE_WRITE_INTENTS: &str = r#"
SELECT id,target_json,before_digest,after_digest,stage FROM file_write_intents
WHERE family='cover_edit' ORDER BY id
"#;
    // Cache within this inventory only: approval must hash afresh, even if timestamps
    // were restored. Repeated intents/hard links do not reread the same path here.
    let mut digests = std::collections::BTreeMap::new();
    let mut cached_digest = |path: &Path| {
        digests
            .entry(storage_access::comparable_with(storage, path))
            .or_insert_with(|| digest_at(storage, path))
            .clone()
    };
    let path = track_path(conn, track)?;
    let path = storage_access::comparable_with(storage, &path);
    let ident = identity(storage, &path);
    let current = cached_digest(&path);
    let mut facts = Vec::new();
    let mut ids = Vec::new();
    let mut intents = Vec::new();
    let mut summaries = Vec::new();
    let mut candidate_facts = Vec::new();
    let mut deletable = Vec::new();
    let mut candidates = Vec::new();
    for row in conn
        .prepare(SELECT_FILE_WRITE_INTENTS)?
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
    {
        let Ok((id, t, b, a, stage)) = row else {
            continue;
        };
        let Ok(target) = parse::<Target>(&t) else {
            continue;
        };
        if target.track == track.0
            || storage_access::comparable_with(storage, &target.path) == path
            || ident.is_some_and(|i| protects_identity(storage, &target, i))
        {
            summaries.push(format!(
                "intent {id}: before {}, after {}",
                b.chars().take(12).collect::<String>(),
                a.chars().take(12).collect::<String>()
            ));
            for (role, candidate, expected) in [
                (CoverCandidateRole::Backup, &target.backup, &b),
                (CoverCandidateRole::Staged, &target.staged, &a),
                (
                    CoverCandidateRole::RollbackAfter,
                    &target.rollback_after,
                    &a,
                ),
            ] {
                if let Some(c) = candidate {
                    let digest = cached_digest(&c.path);
                    let ident = identity(storage, &c.path);
                    let bytes = std::fs::metadata(&c.path).ok().map(|m| m.len());
                    summaries.push(format!("intent {id} {}: {} ({} bytes, SHA-256 {}, {})",
                     match role {
                    CoverCandidateRole::Backup=>"backup",
                    CoverCandidateRole::Staged=>"staged",
                    CoverCandidateRole::RollbackAfter=>"rollback_after"}
                    ,
                     c.path.display(),
                     bytes.map(|n|n.to_string()).unwrap_or_else(
                    ||"unreadable".into()),
                     digest.as_deref().unwrap_or("missing/unreadable"),

                    if digest.as_deref()==Some(expected.as_str())
                    && !expected.is_empty() {
                     "verified expected content" }
                     else {
                     concat!(
"unverified expected content; explicit cleanup deletes this registered ",
"file if its identity and current full digest still match",
) }));
                    if digest.as_deref() == Some(expected.as_str())
                        && !expected.is_empty()
                        && ident == Some(c.identity)
                    {
                        candidates.push(CoverRecoverySelection {
                            intent: id,
                            role,
                            sha256: expected.clone(),
                        });
                    }
                    // A crash can precede persistence of the newly created stage identity.
                    // Explicit confirmation binds its current identity AND complete digest.
                    if ident.is_some()
                        && (ident == Some(c.identity)
                            || (stage == "building" && c.identity == [0, 0]))
                        && digest.as_ref().is_some_and(|digest| {
                            c.retained_digest
                                .as_ref()
                                .is_none_or(|retained| retained == digest)
                        })
                    {
                        deletable.push((id, role));
                    }
                    candidate_facts.push((id, role, ident, digest, bytes, c.path.clone()));
                }
            }
            ids.push(id);
            intents.push(t.clone());
            facts.push((id, t, b, a, stage));
        }
    }
    if ids.is_empty() {
        return Err(LibraryError::CoverRecoveryNotFound);
    }
    let restoring = matches!(
        resolution,
        CoverRecoveryResolution::RestoreBefore | CoverRecoveryResolution::RestoreAfter
    );
    let selected = if restoring {
        let choices: Vec<_> = candidates
            .iter()
            .filter(|c| {
                if resolution == CoverRecoveryResolution::RestoreBefore {
                    c.role == CoverCandidateRole::Backup
                } else {
                    c.role != CoverCandidateRole::Backup
                }
            })
            .collect();
        let chosen = if let Some(requested) = selection {
            choices.into_iter().find(|c| *c == requested)
        } else {
            let hashes: HashSet<_> = choices.iter().map(|c| &c.sha256).collect();
            if hashes.len() != 1 {
                return Err(LibraryError::CoverRecoveryChanged);
            }
            choices.into_iter().next()
        }
        .ok_or(LibraryError::CoverRecoveryChanged)?;
        summaries.push(format!(
            "SELECTED intent {}, {:?}, SHA-256 {}",
            chosen.intent, chosen.role, chosen.sha256
        ));
        Some(chosen.clone())
    } else {
        if selection.is_some() {
            return Err(LibraryError::CoverRecoveryChanged);
        }
        None
    };
    let will_delete = |intent, role, digest: &Option<String>, bytes| {
        deletable.contains(&(intent, role))
            && (resolution == CoverRecoveryResolution::CleanupCandidates
                || (resolution == CoverRecoveryResolution::AcceptCurrent
                    && (bytes == Some(0) || (digest.is_some() && *digest == current))))
    };
    let files: Vec<_> = candidate_facts
        .iter()
        .map(|(intent, role, _ident, digest, bytes, location)| {
            let verified = candidates
                .iter()
                .any(|c| c.intent == *intent && c.role == *role);
            let deleting = will_delete(*intent, *role, digest, *bytes);
            let copies = candidate_facts
                .iter()
                .filter(|(_, _, _, d, _, _)| digest.is_some() && d == digest)
                .map(|(_, _, _, _, _, p)| p)
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            let retained_copy = candidate_facts.iter().any(|(id, r, _, d, n, _)| {
                digest.is_some() && d == digest && !will_delete(*id, *r, d, *n)
            });
            let deletes_last_copies =
                deleting && bytes.is_some_and(|n| n > 0) && current != *digest && !retained_copy;
            if deletes_last_copies {
                summaries.push(format!(
                    "WARNING: {} — {}",
                    if copies == 1 {
                        "ONLY REMAINING COPY"
                    } else {
                        "ALL REMAINING COPIES"
                    },
                    location.display()
                ));
            }
            CoverRecoveryFile {
                intent: *intent,
                role: *role,
                location: location.display().to_string(),
                sha256: digest.clone(),
                bytes: *bytes,
                verified,
                exists: !std::fs::symlink_metadata(location)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
                will_delete: deleting,
                only_copy: deletes_last_copies && copies == 1,
                deletes_last_copies,
            }
        })
        .collect();
    let candidate_identities = candidate_facts
        .iter()
        .filter_map(|(_, _, identity, _, _, path)| identity.map(|id| (path.clone(), id)))
        .collect();
    summaries.sort();
    let fingerprint = cover_edit::digest(
        json(&(
            facts,
            &candidate_facts,
            &path,
            ident,
            &current,
            resolution,
            &selected,
        ))?
        .as_bytes(),
    );
    let operation = match resolution {
        CoverRecoveryResolution::AcceptCurrent => {
            concat!(
                "Accept readable current file; discard inverse data and delete confirmed ",
                "empty or current-content duplicate candidates; retain other candidates ",
                "for separately confirmed cleanup",
            )
        }
        CoverRecoveryResolution::RestoreBefore => {
            "Restore verified backup (before) to the missing original name; retain other candidates"
        }
        CoverRecoveryResolution::RestoreAfter => {
            concat!(
                "Restore verified staged result (after) to the missing original name; ",
                "retain other candidates",
            )
        }
        CoverRecoveryResolution::CleanupCandidates => {
            "Delete ONLY registered candidates whose identity and full digest still match this confirmation; keep current file"
        }
    };
    let summary = format!(
        "{operation}; track {} (current SHA-256 {}); {} intents. {}",
        track.0,
        current
            .as_deref()
            .map(|d| &d[..12])
            .unwrap_or("missing/unreadable"),
        ids.len(),
        summaries.join("; ")
    );
    Ok(CoverRecoveryGuard {
        track,
        summary,
        inventory: summaries.join("; "),
        candidates,
        files,
        can_accept_current: current.is_some(),
        can_restore: std::fs::symlink_metadata(&path)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        can_cleanup: current.is_some()
            && candidate_facts
                .iter()
                .all(|(id, role, ident, digest, bytes, candidate)| {
                    ident.is_none()
                        && digest.is_none()
                        && bytes.is_none()
                        && std::fs::symlink_metadata(candidate)
                            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                        || deletable.contains(&(*id, *role))
                }),
        current_location: path.display().to_string(),
        current_sha256: current.clone(),
        selected,
        resolution,
        fingerprint,
        path,
        ids,
        intents,
        candidate_identities,
    })
}

/// An inaccessible candidate is still registered. Only NotFound proves absence.
pub(super) fn candidates_absent(target: &Target) -> Result<bool> {
    candidates_absent_with(target, |path| std::fs::symlink_metadata(path).map(|_| ()))
}

pub(super) fn candidates_absent_with(
    target: &Target,
    mut inspect: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<bool> {
    for candidate in [&target.backup, &target.staged, &target.rollback_after]
        .into_iter()
        .flatten()
    {
        match inspect(&candidate.path) {
            Ok(()) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(LibraryError::CoverRecoveryRequired),
        }
    }
    Ok(true)
}
