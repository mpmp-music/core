//! Recovery scenarios.
use super::*;

#[test]
fn closed_library_rejects_recovery_confirmation_before_waiting_for_scheduler() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_action");
    assert!(f.edit().is_err());
    let guard = f.lib.describe_cover_recovery(f.id).unwrap();
    f.lib.close().unwrap();
    std::thread::scope(|scope| {
        let held = f.lib.inner.tag_jobs.lock().unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let library = &f.lib;
        scope.spawn(move || {
            done.send(library.accept_cover_recovery(&guard, UI))
                .unwrap()
        });
        let result = finished.recv_timeout(std::time::Duration::from_secs(2));
        drop(held);
        assert!(matches!(result.unwrap(), Err(LibraryError::Closed)));
    });
}

#[test]
fn startup_cover_recovery_uses_injected_file_backend() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(f.edit().is_err());
    let Fixture { dir, lib, path, .. } = f;
    drop(lib);
    let backend = Arc::new(crate::tests::storage::RecordingStorage::default());
    let reopened = Library::open_with(
        &dir.path().join("library.db"),
        crate::OpenOptions {
            storage: Some(backend.backends()),
            migrate: true,
        },
    )
    .unwrap();
    assert!(!reopened.has_cover_recovery_required());
    let calls = backend.calls.lock().unwrap();
    assert!(
        calls
            .iter()
            .any(|(op, at)| *op == "open_regular" && at == &path)
    );
    assert!(calls.iter().any(|(op, _)| *op == "opened_identity"));
}

#[cfg(unix)]
#[test]
fn first_backup_sync_failure_cleans_registered_copy_or_link_without_pending_recovery() {
    for fault in ["backup_sync_fail_copy", "backup_sync_fail_link"] {
        let f = Fixture::new();
        let before = std::fs::read(&f.path).unwrap();
        *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some(fault);
        let r = f.edit().unwrap();
        assert_eq!(r.failed, [(f.id, "write_failed")], "{fault}: {r:?}");
        assert!(r.written.is_empty());
        assert!(r.action.is_none());
        assert_eq!(std::fs::read(&f.path).unwrap(), before);
        assert!(targets(&f).is_empty());
        assert!(!f.lib.has_cover_recovery_required());
        assert!(
            std::fs::read_dir(f.path.parent().unwrap())
                .unwrap()
                .all(|p| !p
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".cover-"))
        );
    }
}

#[cfg(unix)]
#[test]
fn failed_fallback_rollback_preserves_registered_after_and_before_for_recovery() {
    let f = Fixture::new();
    let before = std::fs::read(&f.path).unwrap();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("rollback_no_exchange_sync_fail");
    *f.lib.inner.hooks.cover_replace.lock().unwrap() = Some(Box::new(|staged, path, _backup| {
        std::fs::rename(staged, path)?;
        Err(std::io::Error::other("injected publication sync failure"))
    }));
    let r = f.edit().unwrap();
    assert_eq!(r.failed, [(f.id, "cover_recovery_required")]);
    assert_eq!(std::fs::read(&f.path).unwrap(), before);
    let t = targets(&f).pop().unwrap();
    let copy = t.rollback_after.unwrap();
    assert_eq!(
        identity(&mpmp_fs_desktop::DesktopStorage, &copy.path),
        Some(copy.identity)
    );
    let after = f
        .lib
        .inner
        .pool
        .read(|conn| {
            Ok(
                conn.query_row("SELECT after_digest FROM file_write_intents", [], |r| {
                    r.get::<_, String>(0)
                })?,
            )
        })
        .unwrap();
    assert!(candidate_matches(
        &mpmp_fs_desktop::DesktopStorage,
        &copy,
        &after
    ));
    assert_eq!(std::fs::read(&t.backup.unwrap().path).unwrap(), before);
    assert!(f.lib.has_cover_recovery_required());
    f.lib
        .inner
        .writer
        .run_data({
            let storage = f.lib.inner.storage.clone();
            move |conn| recover(storage.files.as_ref(), storage.replacements.as_ref(), conn)
        })
        .unwrap();
    assert!(!copy.path.exists());
    assert!(targets(&f).is_empty());
}

#[test]
fn restore_binds_explicit_version_with_old_retained_and_new_pending_intents() {
    const UPDATE_FILE_WRITE_INTENTS: &str = r#"
UPDATE file_write_intents SET target_json=json_set(target_json,'$.backup.identity',json(?1),'$.backup.copied',json('true'))
WHERE json_extract(target_json,'$.group')=?2
"#;

    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_action");
    assert!(f.edit().is_err());
    let old = targets(&f).pop().unwrap();
    let accepted = f.lib.describe_cover_recovery(f.id).unwrap();
    f.lib.accept_cover_recovery(&accepted, UI).unwrap();
    let wanted = std::fs::read(&f.path).unwrap();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(
        f.lib
            .edit_cover(&[f.id], &CoverChange::Remove, UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .is_err()
    );
    let new = targets(&f).pop().unwrap();
    assert!(!new.backup.as_ref().unwrap().path.exists());
    std::fs::copy(&f.path, &new.backup.as_ref().unwrap().path).unwrap();
    f.lib
        .inner
        .storage
        .replacements
        .resume(replacement_paths(&new))
        .unwrap()
        .hide_candidate(&new.backup.as_ref().unwrap().path)
        .unwrap();
    let new_backup_id = identity(
        &mpmp_fs_desktop::DesktopStorage,
        &new.backup.as_ref().unwrap().path,
    )
    .unwrap();
    let group = new.group.clone();
    f.lib
        .inner
        .writer
        .run(move |conn| {
            conn.execute(
                UPDATE_FILE_WRITE_INTENTS,
                params![json(&new_backup_id)?, group],
            )?;
            Ok(())
        })
        .unwrap();
    std::fs::remove_file(&f.path).unwrap();
    assert!(matches!(
        f.lib
            .describe_cover_recovery_resolution(f.id, CoverRecoveryResolution::RestoreBefore),
        Err(LibraryError::CoverRecoveryChanged)
    ));
    let inventory = f.lib.describe_cover_recovery(f.id).unwrap();
    let chosen = inventory
        .candidates
        .iter()
        .filter(|c| c.role == CoverCandidateRole::Backup)
        .max_by_key(|c| c.intent)
        .unwrap()
        .clone();
    assert!(
        inventory
            .candidates
            .iter()
            .any(|c| c.role == CoverCandidateRole::Backup && c.sha256 != chosen.sha256)
    );
    let mut invalid = chosen.clone();
    invalid.sha256 = "0".repeat(64);
    assert!(
        f.lib
            .describe_cover_recovery_selected(
                f.id,
                CoverRecoveryResolution::RestoreBefore,
                Some(&invalid)
            )
            .is_err()
    );
    let guard = f
        .lib
        .describe_cover_recovery_selected(
            f.id,
            CoverRecoveryResolution::RestoreBefore,
            Some(&chosen),
        )
        .unwrap();
    assert!(
        guard
            .summary
            .contains(&format!("SELECTED intent {}", chosen.intent))
    );
    assert!(guard.summary.contains(&chosen.sha256));
    f.lib.accept_cover_recovery(&guard, UI).unwrap();
    assert_eq!(std::fs::read(&f.path).unwrap(), wanted);
    assert_eq!(targets(&f)[0].group, old.group);
    assert!(old.backup.as_ref().unwrap().path.exists());
    assert_eq!(targets(&f).len(), 2);
    assert_eq!(f.lib.cover_recovery_required().unwrap(), [f.id]);
}

#[test]
fn accepting_current_cleans_building_candidates_before_identity_was_persisted() {
    for duplicate in [false, true] {
        let f = Fixture::new();
        *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_stage_created");
        assert!(f.edit().is_err());
        let target = targets(&f).pop().unwrap();
        let transaction = f
            .lib
            .inner
            .storage
            .replacements
            .resume(replacement_paths(&target))
            .unwrap();
        let candidate = target.staged.unwrap();
        assert_eq!(candidate.identity, [0, 0]);
        assert_eq!(std::fs::metadata(&candidate.path).unwrap().len(), 0);
        if duplicate {
            // CopyFileW cannot overwrite an existing hidden destination.
            std::fs::remove_file(&candidate.path).unwrap();
            std::fs::copy(&f.path, &candidate.path).unwrap();
            transaction.hide_candidate(&candidate.path).unwrap();
        }
        let guard = f.lib.describe_cover_recovery(f.id).unwrap();
        let file = guard
            .files
            .iter()
            .find(|file| file.location == candidate.path.display().to_string())
            .unwrap();
        assert!(file.exists && file.will_delete && !file.deletes_last_copies);
        assert!(guard.can_cleanup && guard.can_accept_current);
        f.lib.accept_cover_recovery(&guard, UI).unwrap();
        assert!(!candidate.path.exists());
        assert!(targets(&f).is_empty());
    }
}

#[test]
fn accepting_building_candidates_binds_contents_and_retains_distinct_nonempty_copies() {
    use std::io::Write;
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_stage_created");
    assert!(f.edit().is_err());
    let candidate = targets(&f).pop().unwrap().staged.unwrap();
    let guard = f.lib.describe_cover_recovery(f.id).unwrap();
    // The original empty-file approval cannot delete subsequently written contents.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&candidate.path)
        .unwrap()
        .write_all(b"another copy")
        .unwrap();
    assert!(f.lib.accept_cover_recovery(&guard, UI).is_err());
    let guard = f.lib.describe_cover_recovery(f.id).unwrap();
    assert!(
        !guard
            .files
            .iter()
            .find(|file| file.location == candidate.path.display().to_string())
            .unwrap()
            .will_delete
    );
    f.lib.accept_cover_recovery(&guard, UI).unwrap();
    assert_eq!(std::fs::read(&candidate.path).unwrap(), b"another copy");
    assert!(!targets(&f).is_empty());
}

#[test]
fn missing_registered_candidate_is_explicitly_absent() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(f.edit().is_err());
    let staged = targets(&f).pop().unwrap().staged.unwrap();
    std::fs::remove_file(&staged.path).unwrap();
    let guard = f.lib.describe_cover_recovery(f.id).unwrap();
    let file = guard
        .files
        .iter()
        .find(|file| file.location == staged.path.display().to_string())
        .unwrap();
    assert!(!file.exists && !file.will_delete && !file.deletes_last_copies);
}

#[cfg(unix)]
#[test]
fn crash_between_atomic_exchange_and_duplicate_removal_finalizes_once() {
    const UPDATE_FILE_WRITE_INTENTS: &str = r#"
UPDATE file_write_intents SET
target_json=json_set(target_json,'$.backup.identity',json(?1),'$.backup.copied',json('true'))
"#;

    for copied in [false, true] {
        let f = Fixture::new();
        *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
        assert!(f.edit().is_err());
        let t = targets(&f).pop().unwrap();
        let staged = &t.staged.as_ref().unwrap().path;
        let backup = &t.backup.as_ref().unwrap().path;
        if copied {
            std::fs::copy(&f.path, backup).unwrap();
            let backup_id = identity(&mpmp_fs_desktop::DesktopStorage, backup).unwrap();
            f.lib
                .inner
                .writer
                .run(move |conn| {
                    conn.execute(UPDATE_FILE_WRITE_INTENTS, [json(&backup_id)?])?;
                    Ok(())
                })
                .unwrap();
        } else {
            std::fs::hard_link(&f.path, backup).unwrap();
        }
        // Reproduce exchange's persisted namespace without exposing a production hook.
        let saved = f.path.with_file_name("test-exchange");
        std::fs::rename(&f.path, &saved).unwrap();
        std::fs::rename(staged, &f.path).unwrap();
        std::fs::rename(saved, staged).unwrap();
        assert_eq!(
            identity(&mpmp_fs_desktop::DesktopStorage, staged)
                == identity(&mpmp_fs_desktop::DesktopStorage, backup),
            !copied
        );
        f.lib
            .inner
            .writer
            .run_data({
                let storage = f.lib.inner.storage.clone();
                move |conn| recover(storage.files.as_ref(), storage.replacements.as_ref(), conn)
            })
            .unwrap();
        assert_eq!(f.cover_actions(), 1);
        assert!(targets(&f).is_empty());
        f.lib
            .inner
            .writer
            .run_data({
                let storage = f.lib.inner.storage.clone();
                move |conn| recover(storage.files.as_ref(), storage.replacements.as_ref(), conn)
            })
            .unwrap();
        assert_eq!(f.cover_actions(), 1);
    }
}

#[test]
fn recovery_post_move_sync_failure_retains_intent_until_a_durable_retry() {
    for step in ["publish", "sync"] {
        let f = Fixture::new();
        *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_rename");
        assert!(f.edit().is_err());
        let t = targets(&f).pop().unwrap();
        let original = std::fs::read(&t.backup.as_ref().unwrap().path).unwrap();
        std::fs::remove_file(&f.path).unwrap();
        f.lib
            .inner
            .writer
            .run_data({
                let storage = f.lib.inner.storage.clone();
                move |conn| {
                    RECOVERY_FAULT.with(|f| f.set(Some(step)));
                    let result =
                        recover(storage.files.as_ref(), storage.replacements.as_ref(), conn);
                    RECOVERY_FAULT.with(|f| f.set(None));
                    result
                }
            })
            .unwrap();
        assert_eq!(std::fs::read(&f.path).unwrap(), original);
        assert_eq!(targets(&f).len(), 1, "{step}");
        f.lib
            .inner
            .writer
            .run_data({
                let storage = f.lib.inner.storage.clone();
                move |conn| recover(storage.files.as_ref(), storage.replacements.as_ref(), conn)
            })
            .unwrap();
        assert!(targets(&f).is_empty());
        assert_eq!(f.cover_actions(), 0);
    }
}

#[cfg(unix)]
#[test]
fn read_only_cleanup_keeps_library_open_and_continues_other_intents() {
    const INSERT_FILE_WRITE_INTENTS: &str = r#"
INSERT INTO file_write_intents(family,format_version,authorization_json,target_json,before_digest,before_image,after_digest,stage,created_at,updated_at)
SELECT family,format_version,authorization_json,?1,before_digest,before_image,after_digest,stage,created_at,updated_at
FROM file_write_intents LIMIT 1
"#;

    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_action");
    assert!(f.edit().is_err());
    let first = targets(&f).pop().unwrap();
    let second_dir = f.dir.path().join("healthy");
    std::fs::create_dir(&second_dir).unwrap();
    let mut second = first.clone();
    second.path = second_dir.join("music.mp3");
    std::fs::copy(&f.path, &second.path).unwrap();
    let backup = second.backup.as_mut().unwrap();
    backup.path = second_dir.join("backup");
    std::fs::copy(&first.backup.as_ref().unwrap().path, &backup.path).unwrap();
    backup.identity = identity(&mpmp_fs_desktop::DesktopStorage, &backup.path).unwrap();
    second.staged.as_mut().unwrap().path = second_dir.join("staged");
    let saved = json(&second).unwrap();
    f.lib
        .inner
        .writer
        .run(move |conn| {
            conn.execute(INSERT_FILE_WRITE_INTENTS, [saved])?;
            Ok(())
        })
        .unwrap();
    let dir = f.path.parent().unwrap();
    let permissions = std::fs::metadata(dir).unwrap().permissions();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let db = f.dir.path().join("library.db");
    drop(f.lib);
    f.lib = Library::open(&db).unwrap();
    assert_eq!(targets(&f).len(), 1);
    assert_eq!(f.lib.cover_recovery_required().unwrap(), [f.id]);
    assert!(
        !second.backup.as_ref().unwrap().path.exists(),
        "healthy sibling cleanup continued"
    );
    std::fs::set_permissions(dir, permissions).unwrap();
    f.lib
        .inner
        .writer
        .run_data({
            let storage = f.lib.inner.storage.clone();
            move |conn| recover(storage.files.as_ref(), storage.replacements.as_ref(), conn)
        })
        .unwrap();
    assert!(targets(&f).is_empty());
}

#[test]
fn ui_recovery_rejects_missing_accept_restores_verified_candidates_and_cleans_explicitly() {
    for resolution in [
        CoverRecoveryResolution::RestoreBefore,
        CoverRecoveryResolution::RestoreAfter,
    ] {
        let f = Fixture::new();
        let before = std::fs::read(&f.path).unwrap();
        *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
        assert!(f.edit().is_err());
        let t = targets(&f).pop().unwrap();
        // Simulate an interrupted platform replacement with both registered candidates.
        std::fs::rename(&f.path, &t.backup.as_ref().unwrap().path).unwrap();
        let after = std::fs::read(&t.staged.as_ref().unwrap().path).unwrap();
        let guard = f.lib.describe_cover_recovery(f.id).unwrap();
        assert!(matches!(
            f.lib.accept_cover_recovery(&guard, UI),
            Err(LibraryError::CoverRecoveryChanged)
        ));
        assert_eq!(targets(&f).len(), 1);
        let guard = f
            .lib
            .describe_cover_recovery_resolution(f.id, resolution)
            .unwrap();
        assert!(guard.summary.contains("backup:"));
        assert!(guard.summary.contains("staged:"));
        assert!(
            guard
                .summary
                .contains(t.backup.as_ref().unwrap().path.to_str().unwrap())
        );
        f.lib.accept_cover_recovery(&guard, UI).unwrap();
        assert_eq!(
            std::fs::read(&f.path).unwrap(),
            if resolution == CoverRecoveryResolution::RestoreBefore {
                before
            } else {
                after
            }
        );
        let cleanup = f
            .lib
            .describe_cover_recovery_resolution(f.id, CoverRecoveryResolution::CleanupCandidates)
            .unwrap();
        assert!(
            f.lib
                .accept_cover_recovery(&cleanup, ActionMeta::new("remote:test"))
                .is_err()
        );
        f.lib.accept_cover_recovery(&cleanup, UI).unwrap();
        assert!(targets(&f).is_empty());
        assert!(!t.backup.as_ref().unwrap().path.exists());
        assert!(!t.staged.as_ref().unwrap().path.exists());
    }
}

#[cfg(unix)]
#[test]
fn directory_scan_restores_missing_original_with_only_hidden_candidates_and_a_broken_alias() {
    let f = Fixture::new();
    let alias = f.path.with_file_name("alias.mp3");
    std::os::unix::fs::symlink(&f.path, &alias).unwrap();
    scan(&f);
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(f.edit().is_err());
    let t = targets(&f).pop().unwrap();
    std::fs::hard_link(&f.path, &t.backup.unwrap().path).unwrap();
    std::fs::remove_file(&f.path).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let directory = f.path.parent().unwrap();
    let permissions = std::fs::metadata(directory).unwrap().permissions();
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o500)).unwrap();
    scan(&f);
    assert!(
        !f.path.exists(),
        "temporary read-only directory blocks recovery publication"
    );
    assert_eq!(targets(&f).len(), 1);
    std::fs::set_permissions(directory, permissions).unwrap();
    scan(&f);
    let tracks = f
        .lib
        .tracks_query(
            &crate::TrackQuery::default(),
            crate::Page {
                offset: 0,
                limit: 10,
            },
            &[],
        )
        .unwrap()
        .0;
    assert_eq!(tracks.len(), 2);
    assert!(tracks.iter().all(|t| t.track.available));
    assert!(f.path.exists());
    assert!(targets(&f).is_empty());
}

#[cfg(unix)]
#[test]
fn pending_write_guard_detects_new_hardlink_to_an_externally_replaced_target() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(f.edit().is_err());
    let replacement = f.path.with_file_name("external.mp3");
    std::fs::copy(&f.path, &replacement).unwrap();
    std::fs::rename(replacement, &f.path).unwrap();
    let alias = f.path.with_file_name("new-alias.mp3");
    std::fs::hard_link(&f.path, &alias).unwrap();
    f.lib
        .inner
        .pool
        .read(|conn| {
            assert!(pending_path(
                &mpmp_fs_desktop::DesktopStorage,
                conn,
                &alias
            )?);
            Ok(())
        })
        .unwrap();
}

#[test]
fn directory_scan_does_not_retry_an_unobserved_recovery_target() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_rename");
    assert!(f.edit().is_err());
    recover_observed(
        &f.lib.inner,
        &[(f.dir.path().join("another-directory/music.mp3"), None)],
        &HashSet::new(),
    )
    .unwrap();
    assert_eq!(targets(&f).len(), 1);
    assert_eq!(f.cover_actions(), 0);
    recover_observed(
        &f.lib.inner,
        &[(
            f.path.clone(),
            identity(&mpmp_fs_desktop::DesktopStorage, &f.path),
        )],
        &HashSet::new(),
    )
    .unwrap();
    assert!(targets(&f).is_empty());
    assert_eq!(f.cover_actions(), 1);
}

#[test]
fn candidate_content_changes_invalidate_ui_confirmation_and_batch_snapshot_has_no_target_io() {
    const UPDATE_FILE_WRITE_INTENTS: &str = r#"
UPDATE file_write_intents SET
target_json=json_set(target_json,'$.path','/unreachable/offline/music.mp3')
"#;

    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(f.edit().is_err());
    let t = targets(&f).pop().unwrap();
    let staged = t.staged.unwrap().path;
    let guard = f.lib.describe_cover_recovery(f.id).unwrap();
    std::fs::write(&staged, b"external file with same inode").unwrap();
    assert!(matches!(
        f.lib.accept_cover_recovery(&guard, UI),
        Err(LibraryError::CoverRecoveryChanged)
    ));
    let target = f.path.clone();
    f.lib
        .inner
        .writer
        .run_data(move |conn| {
            let cached = PendingTargets::load(conn)?;
            assert!(cached.protects(&target, None));
            let identity = identity(&mpmp_fs_desktop::DesktopStorage, &target);
            assert!(cached.protects(Path::new("unrelated-hardlink-alias"), identity));
            // An unreachable target is data, never opened during snapshot creation.
            conn.execute(UPDATE_FILE_WRITE_INTENTS, [])?;
            let next = PendingTargets::load(conn)?;
            assert!(!next.protects(&target, None));
            assert!(next.protects(Path::new("/unreachable/offline/music.mp3"), None));
            Ok(())
        })
        .unwrap();
}

#[test]
fn inaccessible_registered_candidate_never_counts_as_absent() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_action");
    assert!(f.edit().is_err());
    let target = targets(&f).pop().unwrap();
    assert!(
        candidates_absent_with(&target, |_| Err(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )))
        .is_err()
    );
    assert!(
        candidates_absent_with(&target, |_| Err(std::io::Error::from(
            std::io::ErrorKind::NotFound
        )))
        .unwrap()
    );
    assert!(!candidates_absent(&target).unwrap());
    assert_eq!(f.lib.cover_recovery_required().unwrap(), [f.id]);
}

#[test]
fn finalized_cleanup_preserves_backup_after_a_later_external_save() {
    let mut f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_action");
    assert!(f.edit().is_err());
    let backup = targets(&f).pop().unwrap().backup.unwrap().path;
    let before = std::fs::read(&backup).unwrap();
    let external = b"external complete replacement";
    std::fs::write(&f.path, external).unwrap();
    let db = f.dir.path().join("library.db");
    drop(f.lib);
    f.lib = Library::open(&db).unwrap();
    assert_eq!(f.cover_actions(), 1);
    assert_eq!(std::fs::read(&backup).unwrap(), before);
    assert_eq!(std::fs::read(&f.path).unwrap(), external);
    assert!(f.lib.has_cover_recovery_required());
    assert_eq!(f.lib.cover_recovery_required().unwrap(), [f.id]);
    let guard = f.lib.describe_cover_recovery(f.id).unwrap();
    f.lib.accept_cover_recovery(&guard, UI).unwrap();
    assert_eq!(targets(&f).len(), 1);
    assert!(backup.exists());
}

#[test]
fn final_probe_failure_restores_original_and_cleans_verified_candidates() {
    let f = Fixture::new();
    let before = std::fs::read(&f.path).unwrap();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("probe_failed");
    let r = f.edit().unwrap();
    assert!(r.written.is_empty());
    assert_eq!(r.failed, [(f.id, "cover_edit_unsupported")]);
    assert_eq!(std::fs::read(&f.path).unwrap(), before);
    assert!(targets(&f).is_empty());
    assert_eq!(f.cover_actions(), 0);
}

#[test]
fn crash_while_staging_retains_untrusted_same_inode_contents() {
    let mut f = Fixture::new();
    let original = std::fs::read(&f.path).unwrap();
    let unrelated = f.path.with_file_name(".tmp-unrelated");
    std::fs::write(&unrelated, b"leave me").unwrap();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("during_staging");
    assert!(f.edit().is_err());
    let target = targets(&f).pop().unwrap();
    let staged = target.staged.unwrap().path;
    assert!(staged.exists());
    let original_id = identity(&mpmp_fs_desktop::DesktopStorage, &staged);
    std::fs::write(&staged, b"another complete externally written file").unwrap();
    assert_eq!(
        identity(&mpmp_fs_desktop::DesktopStorage, &staged),
        original_id
    );
    let db = f.dir.path().join("library.db");
    drop(f.lib);
    f.lib = Library::open(&db).unwrap();
    assert_eq!(
        std::fs::read(&staged).unwrap(),
        b"another complete externally written file"
    );
    assert_eq!(targets(&f).len(), 1);
    assert_eq!(std::fs::read(&f.path).unwrap(), original);
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"leave me");
}

#[test]
fn replacement_errors_roll_back_immediately_and_preserve_unknown_candidates() {
    for state in 0..5 {
        let f = Fixture::new();
        let before = std::fs::read(&f.path).unwrap();
        let mutated = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = mutated.clone();
        *f.lib.inner.hooks.cover_replace.lock().unwrap() =
            Some(Box::new(move |staged, path, backup| {
                match state {
                    0 => {}
                    1 => {
                        if !backup.exists() {
                            std::fs::rename(path, backup)?;
                        } else {
                            std::fs::remove_file(path)?;
                        }
                    }
                    2 => {
                        if !backup.exists() {
                            std::fs::rename(path, backup)?;
                        }
                        std::fs::rename(staged, path)?;
                    }
                    3 => {
                        if !backup.exists() {
                            std::fs::rename(path, backup)?;
                        } else {
                            std::fs::remove_file(path)?;
                        }
                    }
                    4 => {
                        if !backup.exists() {
                            std::fs::rename(path, backup)?;
                        } else {
                            std::fs::remove_file(path)?;
                        }
                        // Open the existing candidate: CREATE_ALWAYS has different
                        // attribute/sharing behavior on Windows hidden/temp files.
                        let mut external = std::fs::OpenOptions::new()
                            .write(true)
                            .truncate(true)
                            .open(staged)?;
                        std::io::Write::write_all(&mut external, b"external complete candidate")?;
                    }
                    _ => unreachable!(),
                }
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
                Err(std::io::Error::from_raw_os_error(if state == 1 {
                    1177
                } else {
                    1176
                }))
            }));
        let r = f.edit().unwrap();
        assert!(
            mutated.load(std::sync::atomic::Ordering::SeqCst),
            "state {state}: OS handle prevented the intended partial-replacement fixture"
        );
        assert!(r.written.is_empty(), "state {state}: {r:?}");
        assert_eq!(std::fs::read(&f.path).unwrap(), before, "state {state}");
        assert_eq!(f.cover_actions(), 0);
        if state == 4 {
            let t = targets(&f).pop().unwrap();
            assert_eq!(
                std::fs::read(&t.staged.unwrap().path).unwrap(),
                b"external complete candidate"
            );
            assert_eq!(r.failed, [(f.id, "cover_recovery_required")]);
        } else {
            assert!(targets(&f).is_empty(), "state {state}");
        }
    }
}

#[cfg(unix)]
#[test]
fn concurrent_external_writes_after_final_check_are_preserved_in_displaced_backup() {
    for rename in [false, true] {
        let f = Fixture::new();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        *f.lib.inner.hooks.cover_before_replace.lock().unwrap() = Some(Box::new(move || {
            ready_tx.send(()).unwrap();
            go_rx.recv().unwrap();
        }));
        let job = f
            .lib
            .edit_cover(&[f.id], &f.change(), UI, TagJobOptions::default())
            .unwrap();
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let mut external = std::fs::read(&f.path).unwrap();
        external.extend(b"external audio update");
        if rename {
            let next = f.path.with_file_name("external.mp3");
            std::fs::write(&next, &external).unwrap();
            std::fs::rename(next, &f.path).unwrap();
        } else {
            std::fs::write(&f.path, &external).unwrap();
        }
        go_tx.send(()).unwrap();
        let report = job.wait().unwrap();
        assert!(report.written.is_empty());
        assert_eq!(std::fs::read(&f.path).unwrap(), external);
        assert_eq!(report.failed, [(f.id, "cover_recovery_required")]);
        let target = targets(&f).pop().unwrap();
        assert_eq!(
            std::fs::read(&target.backup.unwrap().path).unwrap(),
            external
        );
        assert!(digest_at(&mpmp_fs_desktop::DesktopStorage, &f.path).is_some());
        assert_eq!(f.cover_actions(), 0);
        f.lib
            .inner
            .writer
            .run_data({
                let storage = f.lib.inner.storage.clone();
                move |conn| recover(storage.files.as_ref(), storage.replacements.as_ref(), conn)
            })
            .unwrap();
        assert_eq!(targets(&f).len(), 1);
    }
}

#[test]
fn external_text_conflict_survives_cover_restamp_and_crash_recovery() {
    for crash in [false, true] {
        let mut f = Fixture::new();
        let action = f
            .lib
            .edit_tags(
                &[f.id],
                &[crate::TagChange::Set {
                    key: "TITLE".into(),
                    values: vec!["our title".into()],
                }],
                UI,
                TagJobOptions::default(),
            )
            .unwrap()
            .wait()
            .unwrap()
            .action
            .unwrap();
        use lofty::file::AudioFile;
        use lofty::file::TaggedFileExt;
        use lofty::tag::Accessor;
        let mut tagged = lofty::probe::Probe::open(&f.path).unwrap().read().unwrap();
        tagged
            .primary_tag_mut()
            .unwrap()
            .set_title("external title with a different length".into());
        tagged
            .save_to_path(&f.path, lofty::config::WriteOptions::default())
            .unwrap();
        // Ensure a changed stamp even on filesystems with coarse timestamps.
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&f.path)
            .unwrap()
            .write_all(b"external trailing bytes")
            .unwrap();
        scan(&f);
        if crash {
            *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_rename");
            assert!(f.edit().is_err());
            let db = f.dir.path().join("library.db");
            drop(f.lib);
            f.lib = Library::open(&db).unwrap();
        } else {
            assert_eq!(f.edit().unwrap().written, [f.id]);
        }
        assert!(matches!(
            f.lib
                .undo_tag_edit(action, UI, TagJobOptions::default())
                .unwrap()
                .unwrap()
                .wait(),
            Err(LibraryError::ActionConflict { .. })
        ));
        let tagged = lofty::probe::Probe::open(&f.path).unwrap().read().unwrap();
        assert_eq!(
            tagged.primary_tag().unwrap().title().as_deref(),
            Some("external title with a different length")
        );
    }
}

#[cfg(unix)]
#[test]
fn aliases_cannot_bypass_recovery_guard_in_cover_tags_or_scans() {
    let f = Fixture::new();
    std::os::unix::fs::symlink(&f.path, f.path.with_file_name("b.mp3")).unwrap();
    scan(&f);
    let alias = f
        .lib
        .tracks_query(
            &crate::TrackQuery::default(),
            crate::Page {
                offset: 0,
                limit: 10,
            },
            &[],
        )
        .unwrap()
        .0
        .into_iter()
        .map(|r| r.track.id)
        .find(|id| *id != f.id)
        .unwrap();
    let old = f.lib.tracks(&[alias]).unwrap()[0].size;
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_rename");
    assert!(f.edit().is_err());
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = None;
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&f.path)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    scan(&f);
    assert_eq!(f.lib.tracks(&[alias]).unwrap()[0].size, old);
    assert!(pending(&f.lib.inner, alias).unwrap());
    let r = f
        .lib
        .edit_cover(&[alias], &f.change(), UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(r.failed, [(alias, "cover_recovery_required")]);
    let r = f
        .lib
        .edit_tags(
            &[alias],
            &[crate::TagChange::Set {
                key: "TITLE".into(),
                values: vec!["no".into()],
            }],
            UI,
            TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(r.skipped, [(alias, "cover_recovery_required")]);
}

#[test]
fn accept_current_is_guarded_audited_and_retains_full_candidates() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_rename");
    assert!(f.edit().is_err());
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = None;
    let target = targets(&f).pop().unwrap();
    let backup = target.backup.unwrap().path;
    let guard = f.lib.describe_cover_recovery(f.id).unwrap();
    assert!(
        f.lib
            .accept_cover_recovery(&guard, ActionMeta::new("agent:test"))
            .is_err()
    );
    let mut external = std::fs::read(&f.path).unwrap();
    external.extend(b"external");
    std::fs::write(&f.path, &external).unwrap();
    assert!(matches!(
        f.lib.accept_cover_recovery(&guard, UI),
        Err(LibraryError::CoverRecoveryChanged)
    ));
    let guard = f.lib.describe_cover_recovery(f.id).unwrap();
    let action = f.lib.accept_cover_recovery(&guard, UI).unwrap();
    assert_eq!(std::fs::read(&f.path).unwrap(), external);
    assert!(backup.exists());
    assert_eq!(targets(&f).len(), 1);
    assert!(f.lib.has_cover_recovery_required());
    let row = f
        .lib
        .actions(
            None,
            crate::Page {
                offset: 0,
                limit: 20,
            },
        )
        .unwrap()
        .items
        .into_iter()
        .find(|a| a.id == action)
        .unwrap();
    assert_eq!(row.kind, "cover_recovery_resolve");
    assert!(!row.undoable);
    assert_eq!(
        f.lib.tracks(&[f.id]).unwrap()[0].size,
        external.len() as u64
    );
}

#[test]
fn successful_partial_finalization_recovers_into_one_group_action() {
    let mut f = Fixture::new();
    let b = f.path.with_file_name("b.mp3");
    tagged_copy("sine44_a.mp3", &b);
    scan(&f);
    let ids: Vec<_> = f
        .lib
        .tracks_query(
            &crate::TrackQuery::default(),
            crate::Page {
                offset: 0,
                limit: 10,
            },
            &[],
        )
        .unwrap()
        .0
        .into_iter()
        .map(|r| r.track.id)
        .collect();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_action");
    assert!(
        f.lib
            .edit_cover(&ids, &f.change(), UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .is_err()
    );
    assert_eq!(f.cover_actions(), 1);
    let db = f.dir.path().join("library.db");
    drop(f.lib);
    f.lib = Library::open(&db).unwrap();
    assert_eq!(f.cover_actions(), 1);
    assert!(targets(&f).is_empty());
    let a = f
        .lib
        .actions(
            None,
            crate::Page {
                offset: 0,
                limit: 10,
            },
        )
        .unwrap()
        .items
        .into_iter()
        .find(|a| a.kind == "cover_edit")
        .unwrap();
    assert_eq!(a.target_count, 2);
    assert!(a.summary.contains("2 files"));
    let r = f
        .lib
        .undo_cover_edit(a.id, UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(r.written.len(), 2);
}

#[test]
fn failed_persisted_intents_remain_charged_to_the_undo_budget() {
    let f = Fixture::new();
    for name in ["a.mp3", "b.mp3", "c.mp3"] {
        let path = f.path.with_file_name(name);
        tagged_copy("sine44_a.mp3", &path);
        let old = std::fs::read(&path).unwrap();
        let n = old[6..10]
            .iter()
            .fold(0usize, |n, b| (n << 7) | usize::from(*b));
        let total = 24 * 1024 * 1024;
        let mut large = old[..10 + n].to_vec();
        large.resize(10 + total, 0);
        large.extend(&old[10 + n..]);
        for (i, shift) in [21, 14, 7, 0].into_iter().enumerate() {
            large[6 + i] = ((total >> shift) & 127) as u8;
        }
        std::fs::write(path, large).unwrap();
    }
    scan(&f);
    let ids: Vec<_> = f
        .lib
        .tracks_query(
            &crate::TrackQuery::default(),
            crate::Page {
                offset: 0,
                limit: 10,
            },
            &[],
        )
        .unwrap()
        .0
        .into_iter()
        .map(|r| r.track.id)
        .collect();
    *f.lib.inner.hooks.cover_replace.lock().unwrap() = Some(Box::new(|staged, _, _| {
        // Rollback cannot clean an externally changed candidate; its committed
        // inverse must remain charged until explicitly resolved.
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(staged)?
            .write_all(b"external candidate change")?;
        Err(std::io::Error::other("sharing violation"))
    }));
    let r = f
        .lib
        .edit_cover(&ids, &f.change(), UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(targets(&f).len(), 2);
    assert!(r.failed.iter().any(|(_, c)| *c == "undo_too_large"));
    let used: i64 = f
        .lib
        .inner
        .pool
        .read(|c| {
            Ok(c.query_row(
                "SELECT sum(length(before_image)) FROM file_write_intents",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert!(used as usize <= MAX_COVER_UNDO_BYTES);
}

#[test]
fn crashes_at_every_durable_boundary_recover_once() {
    for stage in [
        "before_intent",
        "during_staging",
        "after_intent",
        "before_rename",
        "after_rename",
        "before_action",
        "after_action",
    ] {
        let mut f = Fixture::new();
        let old = std::fs::read(&f.path).unwrap();
        *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some(stage);
        assert!(f.edit().is_err(), "{stage}");
        let db = f.dir.path().join("library.db");
        drop(f.lib);
        f.lib = Library::open(&db).unwrap();
        let written = matches!(stage, "after_rename" | "before_action" | "after_action");
        assert_eq!(f.cover_actions(), usize::from(written), "{stage}");
        if !written {
            assert_eq!(std::fs::read(&f.path).unwrap(), old);
        }
        assert_eq!(
            !f.lib.cover_recovery_required().unwrap().is_empty(),
            stage == "during_staging",
            "{stage}"
        );
        drop(f.lib);
        f.lib = Library::open(&db).unwrap();
        assert_eq!(f.cover_actions(), usize::from(written), "repeat {stage}");
        if written {
            let a = f
                .lib
                .actions(
                    None,
                    crate::Page {
                        offset: 0,
                        limit: 100,
                    },
                )
                .unwrap()
                .items
                .into_iter()
                .find(|a| a.kind == "cover_edit")
                .unwrap();
            let report = f
                .lib
                .undo_cover_edit(a.id, UI, TagJobOptions::default())
                .unwrap()
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(report.written, [f.id]);
            assert_eq!(std::fs::read(&f.path).unwrap(), old);
        }
    }
}

#[test]
fn external_edit_after_crash_is_preserved_and_blocks_both_writers() {
    let mut f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_rename");
    assert!(f.edit().is_err());
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&f.path)
        .unwrap();
    file.write_all(b"external change").unwrap();
    drop(file);
    let external = std::fs::read(&f.path).unwrap();
    let db = f.dir.path().join("library.db");
    drop(f.lib);
    f.lib = Library::open(&db).unwrap();
    assert_eq!(f.lib.cover_recovery_required().unwrap(), [f.id]);
    assert_eq!(
        f.edit().unwrap().failed,
        [(f.id, "cover_recovery_required")]
    );
    let tags = f
        .lib
        .edit_tags(
            &[f.id],
            &[crate::TagChange::Set {
                key: "TITLE".into(),
                values: vec!["new".into()],
            }],
            UI,
            TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(tags.skipped, [(f.id, "cover_recovery_required")]);
    assert_eq!(std::fs::read(&f.path).unwrap(), external);
    assert_eq!(f.cover_actions(), 0);
}

#[test]
fn scan_recovers_a_runtime_intent_before_reading_files_and_notifies_once() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_rename");
    assert!(f.edit().is_err());
    assert!(f.lib.has_cover_recovery_required());
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = None;
    if let crate::ScanStart::Started(scan) = f.lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    assert!(f.lib.cover_recovery_required().unwrap().is_empty());
    assert_eq!(f.cover_actions(), 1);
    assert!(!f.lib.has_cover_recovery_required());
    assert_eq!(f.lib.take_recovered_covers(), [f.id]);
    assert!(f.lib.take_recovered_covers().is_empty());
}

#[test]
fn batch_recovery_records_one_action_for_all_replaced_files() {
    let mut f = Fixture::new();
    let other = f.path.with_file_name("b.mp3");
    tagged_copy("sine44_a.mp3", &other);
    if let crate::ScanStart::Started(scan) = f.lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    let other_id = f
        .lib
        .tracks_query(
            &crate::TrackQuery::default(),
            crate::Page {
                offset: 0,
                limit: 10,
            },
            &[],
        )
        .unwrap()
        .0
        .into_iter()
        .find(|r| r.track.rel_path == "b.mp3")
        .unwrap()
        .track
        .id;
    let old = [
        std::fs::read(&f.path).unwrap(),
        std::fs::read(&other).unwrap(),
    ];
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("before_action");
    assert!(
        f.lib
            .edit_cover(&[f.id, other_id], &f.change(), UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .is_err()
    );
    let db = f.dir.path().join("library.db");
    drop(f.lib);
    f.lib = Library::open(&db).unwrap();
    assert_eq!(f.cover_actions(), 1);
    let action = f
        .lib
        .actions(
            None,
            crate::Page {
                offset: 0,
                limit: 100,
            },
        )
        .unwrap()
        .items
        .into_iter()
        .find(|a| a.kind == "cover_edit")
        .unwrap();
    assert_eq!(action.target_count, 2);
    let report = f
        .lib
        .undo_cover_edit(action.id, UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written.len(), 2);
    assert_eq!(std::fs::read(&f.path).unwrap(), old[0]);
    assert_eq!(std::fs::read(&other).unwrap(), old[1]);
}

#[test]
#[ignore = "subprocess helper; executed by abrupt_process_crashes_recover_once"]
#[cfg(unix)]
fn process_crash_child() {
    let dir =
        PathBuf::from(std::env::var_os("MPMP_TEST_COVER_CRASH_DIR").expect("child directory"));
    let stage = std::env::var("MPMP_TEST_COVER_CRASH_STAGE").unwrap();
    let stage = [
        "before_intent",
        "during_staging",
        "after_intent",
        "before_rename",
        "after_rename",
        "before_action",
        "after_action",
    ]
    .into_iter()
    .find(|s| *s == stage)
    .expect("known fault stage");
    let lib = Library::open(&dir.join("library.db")).unwrap();
    let id = lib
        .tracks_query(
            &crate::TrackQuery::default(),
            crate::Page {
                offset: 0,
                limit: 10,
            },
            &[],
        )
        .unwrap()
        .0[0]
        .track
        .id;
    *lib.inner.hooks.cover_fault.lock().unwrap() = Some(stage);
    lib.inner
        .hooks
        .cover_abort
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = lib
        .edit_cover(
            &[id],
            &CoverChange::Set {
                mime: "image/png".into(),
                data: png(),
            },
            UI,
            TagJobOptions::default(),
        )
        .unwrap()
        .wait();
    panic!("fault injection did not terminate the child");
}

#[test]
#[cfg(unix)]
fn abrupt_process_crashes_recover_once() {
    for stage in [
        "before_intent",
        "during_staging",
        "after_intent",
        "before_rename",
        "after_rename",
        "before_action",
        "after_action",
    ] {
        let mut f = Fixture::new();
        let original = std::fs::read(&f.path).unwrap();
        let db = f.dir.path().join("library.db");
        drop(f.lib);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cover::job::tests::recovery::process_crash_child",
                "--ignored",
                "--nocapture",
            ])
            .env("MPMP_TEST_COVER_CRASH_DIR", f.dir.path())
            .env("MPMP_TEST_COVER_CRASH_STAGE", stage)
            .current_dir(f.dir.path())
            .output()
            .unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            output.status.signal(),
            Some(6),
            "{stage}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        f.lib = Library::open(&db).unwrap();
        let written = matches!(stage, "after_rename" | "before_action" | "after_action");
        assert_eq!(f.cover_actions(), usize::from(written), "{stage}");
        assert_eq!(
            !f.lib.cover_recovery_required().unwrap().is_empty(),
            stage == "during_staging",
            "{stage}"
        );
        if !written {
            assert_eq!(std::fs::read(&f.path).unwrap(), original);
        }
        drop(f.lib);
        f.lib = Library::open(&db).unwrap();
        assert_eq!(f.cover_actions(), usize::from(written));
    }
}

#[test]
fn release_barrier_includes_finalization_and_recovery_defers_released_roots() {
    let f = Fixture::new();
    let root = f.lib.track(f.id).unwrap().unwrap().root_id;
    let lib = f.lib.clone();
    let options = TagJobOptions {
        progress: Box::new(move |_, _| {
            lib.suspend_root(root).unwrap();
            assert!(
                !lib.wait_root_idle(root, std::time::Duration::from_millis(10))
                    .unwrap()
            );
        }),
        ..Default::default()
    };
    let report = f
        .lib
        .edit_cover(&[f.id], &f.change(), UI, options)
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, [f.id]);
    assert!(
        f.lib
            .wait_root_idle(root, std::time::Duration::from_secs(1))
            .unwrap()
    );
    f.lib.resume_root(root).unwrap();
    let change = CoverChange::Remove;
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_rename");
    assert!(
        f.lib
            .edit_cover(&[f.id], &change, UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .is_err()
    );
    f.lib.suspend_root(root).unwrap();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = None;
    if let crate::ScanStart::Started(scan) = f.lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    assert_eq!(f.lib.cover_recovery_required().unwrap(), [f.id]);
    f.lib.resume_root(root).unwrap();
    if let crate::ScanStart::Started(scan) = f.lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    assert!(f.lib.cover_recovery_required().unwrap().is_empty());
}
