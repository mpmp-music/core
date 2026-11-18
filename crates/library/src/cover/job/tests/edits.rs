//! Edits scenarios.
use super::*;

#[test]
fn largest_cover_writes_probe_and_fully_decode_with_the_playback_engine() {
    let data = crate::cover::edit::tests::max_png();
    for name in [
        "sine44_a.mp3",
        "sine44_a.flac",
        "sine44_a.m4a",
        "sine44_a.ogg",
    ] {
        let f = Fixture::new_format(name);
        let decode = || {
            mpmp_engine::compute_waveform(
                Arc::new(mpmp_media_io::FileSource::open(&f.path).unwrap()),
                f.path.extension().and_then(|s| s.to_str()),
                None,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap()
            .unwrap()
        };
        let before = (!name.ends_with("opus")).then(&decode);
        let r = f
            .lib
            .edit_cover(
                &[f.id],
                &CoverChange::Set {
                    mime: "image/png".into(),
                    data: data.clone(),
                },
                UI,
                TagJobOptions::default(),
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(r.written, [f.id], "{name}: {r:?}");
        if let Some(before) = &before {
            assert_eq!(&decode(), before, "{name}");
        }
        let r = f
            .lib
            .edit_cover(&[f.id], &CoverChange::Remove, UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(r.written, [f.id], "{name}: {r:?}");
        if let Some(before) = &before {
            assert_eq!(&decode(), before, "remove {name}");
        }
    }
}

#[test]
fn cleanup_confirmation_warns_when_deleting_the_only_registered_version() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_action");
    assert!(f.edit().is_err());
    let before = targets(&f).pop().unwrap().backup.unwrap();
    let accepted = f.lib.describe_cover_recovery(f.id).unwrap();
    f.lib.accept_cover_recovery(&accepted, UI).unwrap();
    let guard = f
        .lib
        .describe_cover_recovery_resolution(f.id, CoverRecoveryResolution::CleanupCandidates)
        .unwrap();
    assert!(guard.summary.contains("ONLY REMAINING COPY"));
    let file = guard
        .files
        .iter()
        .find(|file| file.location == before.path.display().to_string())
        .unwrap();
    assert!(file.will_delete && file.only_copy && file.deletes_last_copies && file.verified);
    assert!(!guard.can_restore);
    assert!(guard.can_accept_current);
    assert!(guard.summary.contains(before.path.to_str().unwrap()));
    assert!(
        guard
            .summary
            .contains(&digest_at(&mpmp_fs_desktop::DesktopStorage, &before.path).unwrap()[..12])
    );
    f.lib.accept_cover_recovery(&guard, UI).unwrap();
    assert!(!before.path.exists());
}

#[test]
fn cleanup_is_unavailable_when_the_original_is_missing() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_action");
    assert!(f.edit().is_err());
    std::fs::remove_file(&f.path).unwrap();
    let guard = f
        .lib
        .describe_cover_recovery_resolution(f.id, CoverRecoveryResolution::CleanupCandidates)
        .unwrap();
    assert!(!guard.can_cleanup);
    assert!(!guard.can_accept_current);
    assert!(guard.can_restore);
    assert!(f.lib.accept_cover_recovery(&guard, UI).is_err());
}

#[test]
fn cleanup_inventory_does_not_call_a_duplicate_current_image_the_only_copy() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_action");
    assert!(f.edit().is_err());
    let target = targets(&f).pop().unwrap();
    let transaction = f
        .lib
        .inner
        .storage
        .replacements
        .resume(replacement_paths(&target))
        .unwrap();
    let backup = target.backup.unwrap();
    // CopyFileW cannot overwrite a hidden destination. Keep its registered identity.
    transaction.set_hidden(&backup.path, false).unwrap();
    std::fs::copy(&f.path, &backup.path).unwrap();
    transaction.hide_candidate(&backup.path).unwrap();
    let guard = f
        .lib
        .describe_cover_recovery_resolution(f.id, CoverRecoveryResolution::CleanupCandidates)
        .unwrap();
    let candidate = guard
        .files
        .iter()
        .find(|file| file.location == backup.path.display().to_string())
        .unwrap();
    assert!(candidate.will_delete);
    assert!(!candidate.only_copy && !candidate.deletes_last_copies);
    assert!(!guard.summary.contains("ONLY REMAINING COPY"));
}

#[test]
fn accepting_current_cleans_empty_stage_without_a_last_copy_warning() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(f.edit().is_err());
    let staged = targets(&f).pop().unwrap().staged.unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&staged.path)
        .unwrap()
        .set_len(0)
        .unwrap();
    assert_eq!(std::fs::metadata(&staged.path).unwrap().len(), 0);
    let guard = f.lib.describe_cover_recovery(f.id).unwrap();
    let file = guard
        .files
        .iter()
        .find(|file| file.location == staged.path.display().to_string())
        .unwrap();
    assert!(file.exists && file.will_delete);
    assert!(!file.only_copy && !file.deletes_last_copies);
    f.lib.accept_cover_recovery(&guard, UI).unwrap();
    assert!(!staged.path.exists());
    assert!(f.path.exists());
}

#[test]
fn unverified_registered_content_has_truthful_cleanup_and_last_copy_facts() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(f.edit().is_err());
    let target = targets(&f).pop().unwrap();
    let staged = target.staged.unwrap();
    std::fs::write(&staged.path, b"unique unverified registered candidate").unwrap();
    let guard = f
        .lib
        .describe_cover_recovery_resolution(f.id, CoverRecoveryResolution::CleanupCandidates)
        .unwrap();
    let candidate = guard
        .files
        .iter()
        .find(|file| file.location == staged.path.display().to_string())
        .unwrap();
    assert!(!candidate.verified);
    assert!(candidate.will_delete && candidate.only_copy && candidate.deletes_last_copies);
    assert!(guard.can_cleanup && guard.can_accept_current && !guard.can_restore);
    assert!(!guard.summary.contains("unverified content; retained"));
    f.lib.accept_cover_recovery(&guard, UI).unwrap();
    assert!(!staged.path.exists());
    assert!(f.path.exists());
}

#[cfg(unix)]
#[test]
fn walk_target_identity_protects_alias_in_another_batch() {
    alias_scan_batches(true);
}

#[cfg(unix)]
#[test]
fn walk_target_identity_protects_alias_when_target_has_no_upsert() {
    alias_scan_batches(false);
}

#[cfg(unix)]
#[test]
fn symlink_music_root_keeps_replaced_target_and_hardlink_alias_protected() {
    use lofty::config::WriteOptions;
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::Accessor;
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real-music");
    let linked = dir.path().join("linked-music");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &linked).unwrap();
    let path = linked.join("a.mp3");
    let alias = linked.join("alias.mp3");
    tagged_copy("sine44_a.mp3", &path);
    std::fs::hard_link(&path, &alias).unwrap();
    let lib = Library::open(&dir.path().join("library.db")).unwrap();
    let root = lib.add_root(&linked, UI).unwrap().value;
    let canonical = storage_access::canonical_dir(&real).unwrap();
    assert_eq!(lib.roots().unwrap()[0].path, canonical);
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    let rows = lib
        .tracks_query(&crate::TrackQuery::default(), crate::Page::new(0, 10), &[])
        .unwrap()
        .0;
    assert_eq!(rows.len(), 2);
    let target = rows
        .iter()
        .find(|row| row.track.rel_path == "a.mp3")
        .unwrap()
        .track
        .id;
    let old = identity(&mpmp_fs_desktop::DesktopStorage, &path);
    *lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(
        lib.edit_cover(
            &[target],
            &CoverChange::Set {
                mime: "image/png".into(),
                data: png()
            },
            UI,
            TagJobOptions::default()
        )
        .unwrap()
        .wait()
        .is_err()
    );
    let registered: Target = lib
        .inner
        .pool
        .read(|conn| {
            let json: String = conn.query_row(
                "SELECT target_json FROM file_write_intents WHERE family='cover_edit'",
                [],
                |row| row.get(0),
            )?;
            parse(&json)
        })
        .unwrap();
    assert_eq!(registered.path, canonical.join("a.mp3"));
    let replacement = linked.join("replacement.mp3");
    std::fs::copy(&path, &replacement).unwrap();
    let mut file = lofty::read_from_path(&replacement).unwrap();
    file.primary_tag_mut()
        .unwrap()
        .set_title("SHOULD BE BLOCKED".into());
    file.save_to_path(&replacement, WriteOptions::default())
        .unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::fs::hard_link(&path, &alias).unwrap();
    assert_ne!(old, identity(&mpmp_fs_desktop::DesktopStorage, &path));
    assert_eq!(
        identity(&mpmp_fs_desktop::DesktopStorage, &path),
        identity(&mpmp_fs_desktop::DesktopStorage, &alias)
    );
    // Exercise the public scanner, not synthetic observations or writer batches.
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    for row in &rows {
        let current = lib.track(row.track.id).unwrap().unwrap();
        assert_eq!(current.root_id, root);
        assert_eq!(current.title, row.track.title);
        assert_ne!(current.title.as_deref(), Some("SHOULD BE BLOCKED"));
    }
    assert_eq!(lib.cover_recovery_required().unwrap(), [target]);
    let guard = lib.describe_cover_recovery(target).unwrap();
    lib.accept_cover_recovery(&guard, UI).unwrap();
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    for row in &rows {
        assert_eq!(
            lib.track(row.track.id).unwrap().unwrap().title.as_deref(),
            Some("SHOULD BE BLOCKED")
        );
    }
}

#[cfg(unix)]
#[test]
fn earlier_root_observations_protect_hardlink_alias_in_a_later_root() {
    cross_root_alias_protection(false);
}

#[cfg(unix)]
#[test]
fn pending_target_in_a_later_root_protects_alias_in_the_first_root() {
    cross_root_alias_protection(true);
}

#[test]
fn empty_pending_prewarm_never_reads_metadata_or_constructs_paths() {
    let f = Fixture::new();
    let observed = ScanObservations::default();
    for retained in [false, true] {
        if retained {
            *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
            assert!(f.edit().is_err());
            let guard = f.lib.describe_cover_recovery(f.id).unwrap();
            f.lib.accept_cover_recovery(&guard, UI).unwrap();
        }
        PATH_NORMALIZATIONS.with(|n| n.set(0));
        observed
            .prewarm_pending_with(&f.lib.inner, |_| {
                panic!("empty pending must not stat files")
            })
            .unwrap();
        assert_eq!(PATH_NORMALIZATIONS.with(|n| n.get()), 0);
        assert!(observed.roots.lock().unwrap().is_empty());
    }
}

#[test]
fn failed_or_released_prewarm_targets_stay_isolated_and_pending() {
    let f = Fixture::new();
    let before = std::fs::read(&f.path).unwrap();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(f.edit().is_err());
    let observed = ScanObservations::default();
    let mut attempted = HashSet::new();
    observed
        .prewarm_pending_with(&f.lib.inner, |path| {
            assert!(attempted.insert(path.to_owned()));
            None
        })
        .unwrap();
    assert_eq!(attempted.len(), 4);
    assert!(attempted.contains(&f.path));
    assert!(observed.roots.lock().unwrap().is_empty());
    assert_eq!(targets(&f).len(), 1);
    assert_eq!(f.cover_actions(), 0);
    assert_eq!(std::fs::read(&f.path).unwrap(), before);
    let root = f.lib.track(f.id).unwrap().unwrap().root_id;
    f.lib.release_root(root).unwrap();
    observed
        .prewarm_pending_with(&f.lib.inner, |_| panic!("released root must not be opened"))
        .unwrap();
    assert_eq!(targets(&f).len(), 1);
    assert!(observed.roots.lock().unwrap().is_empty());
}

#[test]
fn large_import_identity_work_is_linear_in_files_and_pending_targets_per_batch() {
    let small = counted_scan_import(2000, true);
    let large = counted_scan_import(4000, true);
    assert_eq!(small.0, 2000);
    assert_eq!(large.0, 4000);
    // Four registered paths for one intent, queried once per 500-row batch.
    assert!(small.1 <= 4 * 4 && large.1 <= 4 * 8, "{small:?}, {large:?}");
    assert!(
        small.2 <= 2000 + 4 * 4 && large.2 <= 4000 + 4 * 8,
        "{small:?}, {large:?}"
    );
    assert!(
        large.1 <= 2 * small.1 && large.2 <= 2 * small.2,
        "{small:?}, {large:?}"
    );
}

#[test]
fn empty_pending_import_never_builds_guard_paths_or_queries_observations() {
    let work = counted_scan_import(4000, false);
    assert_eq!(work, (4000, 0, 0));
}

#[test]
fn scan_index_merges_a_new_pending_snapshot_after_an_empty_snapshot() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(f.edit().is_err());
    let observed = ScanObservations::default();
    observed.record_root(
        f.path.parent().unwrap(),
        [("a.mp3", Some([123, 456]))].into_iter(),
    );
    let target = f.path.clone();
    f.lib
        .inner
        .writer
        .run(move |conn| {
            conn.execute("UPDATE file_write_intents SET stage='retained'", [])?;
            let mut empty = PendingTargets::load(conn)?;
            empty.merge_observed(&observed);
            assert!(empty.is_empty());
            assert_eq!(
                observed.lookups.load(std::sync::atomic::Ordering::Relaxed),
                0
            );
            conn.execute("UPDATE file_write_intents SET stage='prepared'", [])?;
            let mut latest = PendingTargets::load(conn)?;
            latest.merge_observed(&observed);
            assert!(latest.protects(Path::new("unrelated-hardlink"), Some([123, 456])));
            assert!(latest.protects(&target, None));
            Ok(())
        })
        .unwrap();
}

#[test]
fn interrupted_seal_restores_before_and_reports_no_committed_write() {
    let f = Fixture::new();
    let before = std::fs::read(&f.path).unwrap();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("seal_interrupted");
    let r = f.edit().unwrap();
    assert!(r.written.is_empty());
    assert_eq!(r.failed, [(f.id, "write_failed")]);
    assert!(r.action.is_none());
    assert!(targets(&f).is_empty());
    assert_eq!(std::fs::read(&f.path).unwrap(), before);
}

#[test]
fn completed_registered_files_finalize_and_cleanup_on_restart() {
    for point in ["after_rename", "after_action"] {
        let mut f = Fixture::new();
        *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some(point);
        assert!(f.edit().is_err());
        let target = targets(&f).pop().unwrap();
        let backup = target.backup.unwrap().path;
        assert!(backup.exists());
        let db = f.dir.path().join("library.db");
        drop(f.lib);
        f.lib = Library::open(&db).unwrap();
        assert!(!backup.exists());
        assert!(targets(&f).is_empty());
        assert!(f.lib.cover_recovery_required().unwrap().is_empty());
        assert_eq!(f.cover_actions(), 1);
    }
}

#[test]
fn readonly_files_are_skipped_in_cover_and_tag_previews() {
    let f = Fixture::new();
    let before = std::fs::metadata(&f.path).unwrap().permissions();
    let mut readonly = before.clone();
    readonly.set_readonly(true);
    std::fs::set_permissions(&f.path, readonly).unwrap();
    let covers = f.lib.preview_cover_edit(&[f.id], &f.change()).unwrap();
    let tags = f
        .lib
        .preview_tag_edit(
            &[f.id],
            &[crate::TagChange::Set {
                key: "TITLE".into(),
                values: vec!["New title".into()],
            }],
        )
        .unwrap();
    std::fs::set_permissions(&f.path, before).unwrap();
    assert!(covers.files.is_empty());
    assert_eq!(covers.skipped, [(f.id, "file_read_only")]);
    assert!(tags.files.is_empty());
    assert_eq!(tags.skipped, [(f.id, "file_read_only")]);
}

#[test]
fn one_conflicting_finalize_does_not_block_healthy_siblings() {
    let f = Fixture::new();
    tagged_copy("sine44_a.mp3", &f.path.with_file_name("b.mp3"));
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
    let corrupt = f.path.clone();
    let progress = Box::new(move |done, total| {
        if done == total {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&corrupt)
                .unwrap()
                .write_all(b"external after replacement")
                .unwrap();
        }
    });
    let r = f
        .lib
        .edit_cover(
            &ids,
            &f.change(),
            UI,
            TagJobOptions {
                progress,
                ..Default::default()
            },
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(r.written.len(), 2);
    assert_eq!(r.failed, [(f.id, "cover_recovery_required")]);
    assert_eq!(targets(&f).len(), 1);
    assert_eq!(f.cover_actions(), 1);
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
        .find(|a| Some(a.id) == r.action)
        .unwrap();
    assert_eq!(a.target_count, 1);
}

#[test]
fn cue_targets_are_skipped_and_removed_front_reveals_back_cover() {
    const UPDATE_TRACKS: &str = r#"
UPDATE tracks SET cue_sheet='a.cue',cue_number=1,cue_start=0,cue_size=0,cue_mtime_ms=0
WHERE id=?1
"#;

    let f = Fixture::new();
    let before = std::fs::read(&f.path).unwrap();
    let preview = f
        .lib
        .preview_cover_edit(&[f.id], &CoverChange::Remove)
        .unwrap();
    assert!(!preview.files[0].changed);
    assert!(preview.files[0].fallback);
    assert!(preview.files[0].after.is_some());
    assert_eq!(std::fs::read(&f.path).unwrap(), before);
    let id = f.id;
    f.lib
        .inner
        .writer
        .run(move |conn| {
            conn.execute(UPDATE_TRACKS, [id.0])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(f.edit().unwrap().skipped, [(f.id, "cue_track")]);
    assert_eq!(std::fs::read(&f.path).unwrap(), before);
}

#[test]
fn approved_sources_are_rechecked_inside_the_serial_scheduler() {
    let f = Fixture::new();
    let sources = f.lib.cover_sources(&[f.id]).unwrap();
    let modified = std::fs::metadata(&f.path).unwrap().modified().unwrap();
    let mut bytes = std::fs::read(&f.path).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    std::fs::write(&f.path, &bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&f.path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let report = f
        .lib
        .edit_cover(
            &[f.id],
            &f.change(),
            UI,
            TagJobOptions {
                cover_sources: Some(sources),
                ..Default::default()
            },
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.skipped, [(f.id, "file_changed")]);
    assert_eq!(std::fs::read(&f.path).unwrap(), bytes);
}
