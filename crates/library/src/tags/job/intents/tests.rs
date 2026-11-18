use super::*;
use crate::{Library, Page, ScanStart, TrackQuery};
use std::fs;
#[cfg(any(unix, windows))]
use std::sync::Mutex;

const UI: ActionMeta<'static> = ActionMeta::new("ui");
const POINTS: &[&str] = &[
    "building",
    "sync_failed",
    "prepared",
    "publishing",
    "after_syscall",
    "after_rename",
    "replaced",
    "before_action",
    "after_action",
];

fn fixture() -> (tempfile::TempDir, Library, PathBuf, PathBuf, TrackId) {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    fs::create_dir(&music).unwrap();
    let path = music.join("sample.flac");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_a.flac"),
        &path,
    )
    .unwrap();
    let db = dir.path().join("library.sqlite");
    let lib = Library::open(&db).unwrap();
    lib.add_root(&music, UI).unwrap();
    scan(&lib);
    let id = lib.track_ids(&TrackQuery::default(), 1).unwrap()[0];
    (dir, lib, db, path, id)
}

fn scan(lib: &Library) {
    let ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() else {
        panic!("expected scan");
    };
    scan.wait().unwrap();
}

fn changes() -> [TagChange; 1] {
    [TagChange::Set {
        key: "GENRE".into(),
        values: vec!["Written".into()],
    }]
}

fn edit(lib: &Library, track: TrackId) -> Result<TagEditReport> {
    lib.edit_tags(&[track], &changes(), UI, TagJobOptions::default())
        .unwrap()
        .wait()
}

fn undo(lib: &Library, id: ActionId) -> Result<TagEditReport> {
    lib.undo_tag_edit(id, UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
}

fn action(lib: &Library, kind: &str) -> Option<ActionId> {
    let entries = lib
        .actions(
            None,
            Page {
                offset: 0,
                limit: 100,
            },
        )
        .unwrap();
    let mut actions = entries.items.iter().filter(|a| a.kind == kind);
    let id = actions.next().map(|a| a.id);
    assert!(actions.next().is_none(), "{kind} was recorded twice");
    id
}

fn assert_clean(lib: &Library, path: &Path) {
    let diagnostics = lib.tag_recovery_diagnostics().unwrap();
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let entries: Vec<_> = path
        .parent()
        .unwrap()
        .read_dir()
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries, [path], "unexpected files in the music directory");
}

#[cfg(windows)]
#[test]
fn exclusive_displacement_reports_sharing_conflict_after_probe() {
    use std::os::windows::fs::OpenOptionsExt;

    let (_dir, lib, _db, path, id) = fixture();
    let before = fs::read(&path).unwrap();
    assert_eq!(
        lib.preview_tag_edit(&[id], &changes()).unwrap().files.len(),
        1
    );
    let hooks = &lib.inner().hooks;
    *hooks.tag_publish_mode.lock().unwrap() = Some(TagPublish::Exclusive { stable_ids: true });
    let held = Arc::new(Mutex::new(None));
    let late = held.clone();
    *hooks.tag_before_publish.lock().unwrap() = Some(Box::new(move |source, _| {
        // The real deny-delete handle arrives after the job's successful probe.
        let reader = fs::File::options()
            .read(true)
            .share_mode(0x1 | 0x2)
            .open(source)
            .unwrap();
        *late.lock().unwrap() = Some(reader);
    }));
    let report = edit(&lib, id).unwrap();
    assert!(held.lock().unwrap().is_some());
    assert_eq!(report.skipped, [(id, "file_in_use")]);
    assert!(report.failed.is_empty());
    assert!(report.action.is_none());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_clean(&lib, &path);

    *hooks.tag_before_publish.lock().unwrap() = None;
    *held.lock().unwrap() = None;
    *hooks.tag_publish_mode.lock().unwrap() = None;
    assert_eq!(edit(&lib, id).unwrap().written, [id]);
}

#[cfg(unix)]
const UNIX_PROTOCOLS: [TagPublish; 3] = [
    TagPublish::Exchange,
    TagPublish::Exclusive { stable_ids: true },
    TagPublish::Link,
];

#[cfg(unix)]
#[test]
fn edit_undo_and_redo_leave_no_copies_or_intents_on_any_unix_protocol() {
    for protocol in UNIX_PROTOCOLS {
        let (_dir, lib, _db, path, id) = fixture();
        let original = fs::read(&path).unwrap();
        assert_eq!(
            lib.preview_tag_edit(&[id], &changes()).unwrap().files.len(),
            1
        );
        assert_clean(&lib, &path);
        *lib.inner().hooks.tag_publish_mode.lock().unwrap() = Some(protocol);

        let edited = edit(&lib, id).unwrap().action.unwrap();
        let after = fs::read(&path).unwrap();
        assert_ne!(after, original, "{protocol:?}");
        assert_clean(&lib, &path);

        let undone = undo(&lib, edited).unwrap().action.unwrap();
        assert_eq!(fs::read(&path).unwrap(), original, "{protocol:?}");
        assert_clean(&lib, &path);

        assert_eq!(undo(&lib, undone).unwrap().written, [id], "{protocol:?}");
        assert_eq!(fs::read(&path).unwrap(), after, "{protocol:?}");
        assert_clean(&lib, &path);
    }
}

#[cfg(unix)]
#[test]
fn restart_cleans_a_verified_displacement_on_any_unix_protocol() {
    for protocol in UNIX_PROTOCOLS {
        let (_dir, lib, db, path, id) = fixture();
        let original = fs::read(&path).unwrap();
        *lib.inner().hooks.tag_publish_mode.lock().unwrap() = Some(protocol);
        *lib.inner().hooks.tag_fault.lock().unwrap() = Some("after_rename");
        assert!(edit(&lib, id).is_err());
        assert_eq!(path.parent().unwrap().read_dir().unwrap().count(), 2);
        drop(lib);

        let lib = Library::open(&db).unwrap();
        assert_clean(&lib, &path);
        let action = action(&lib, "tag_edit").unwrap();
        assert_eq!(undo(&lib, action).unwrap().written, [id]);
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_clean(&lib, &path);
    }
}

#[cfg(unix)]
#[test]
fn cleanup_removes_a_verified_hard_link_left_by_the_installation() {
    let (_dir, lib, _db, path, id) = fixture();
    *lib.inner().hooks.tag_publish_mode.lock().unwrap() = Some(TagPublish::Link);
    *lib.inner().hooks.tag_after_publish.lock().unwrap() = Some(Box::new(|dest, displaced| {
        fs::hard_link(dest, displaced.with_extension("tmp")).unwrap();
    }));
    assert_eq!(edit(&lib, id).unwrap().written, [id]);
    assert_clean(&lib, &path);
}

#[test]
fn restart_at_every_edit_and_restore_boundary() {
    for &point in POINTS {
        for restoring in [false, true] {
            let (_dir, mut lib, db, path, id) = fixture();
            let original = fs::read(&path).unwrap();
            let edit_id = restoring.then(|| {
                let report = edit(&lib, id).unwrap();
                assert_eq!(report.written, [id], "{report:?}");
                report.action.unwrap()
            });
            let source = fs::read(&path).unwrap();
            let run = |lib: &Library| match edit_id {
                Some(edit_id) => undo(lib, edit_id),
                None => edit(lib, id),
            };
            *lib.inner().hooks.tag_fault.lock().unwrap() = Some(point);
            let interrupted = run(&lib);
            assert!(interrupted.is_err(), "{point} {interrupted:?}");
            drop(lib);
            lib = Library::open(&db).unwrap();
            assert_clean(&lib, &path);
            let kind = if restoring { "undo" } else { "tag_edit" };
            if matches!(point, "building" | "prepared" | "publishing") {
                assert!(action(&lib, kind).is_none());
                assert_eq!(fs::read(&path).unwrap(), source);
                let retry = run(&lib).unwrap();
                assert_eq!(retry.written, [id], "{retry:?}");
            }
            let id = action(&lib, kind).unwrap();
            let restored = undo(&lib, id).unwrap();
            assert_eq!(restored.written.len(), 1, "{point} {restored:?}");
            assert_eq!(
                fs::read(&path).unwrap(),
                source,
                "{point}, restoring={restoring}"
            );
            if restoring {
                undo(&lib, restored.action.unwrap()).unwrap();
                assert_eq!(fs::read(&path).unwrap(), original, "{point}");
            }
            assert_clean(&lib, &path);
        }
    }
}

#[test]
fn recovery_keeps_published_targets_when_a_batch_stops_before_the_next_rename() {
    let (_dir, lib, db, path, id) = fixture();
    let other = path.with_file_name("second.flac");
    let original = fs::read(&path).unwrap();
    fs::write(&other, &original).unwrap();
    scan(&lib);
    let second = lib
        .track_ids(&TrackQuery::default(), 10)
        .unwrap()
        .into_iter()
        .find(|track| *track != id)
        .unwrap();
    let hooks = lib.inner().hooks.clone();
    let report = lib
        .edit_tags(
            &[id, second],
            &changes(),
            UI,
            TagJobOptions {
                in_use: Arc::new(move |track| {
                    if track == second {
                        *hooks.tag_fault.lock().unwrap() = Some("prepared");
                    }
                    false
                }),
                ..Default::default()
            },
        )
        .unwrap()
        .wait();
    assert!(report.is_err(), "{report:?}");
    drop(lib);
    let lib = Library::open(&db).unwrap();
    let edit = action(&lib, "tag_edit").unwrap();
    assert_eq!(lib.tag_edit_tracks(edit).unwrap(), &[id]);
    assert_eq!(fs::read(&other).unwrap(), original);
    assert_ne!(fs::read(&path).unwrap(), original);
    assert_eq!(undo(&lib, edit).unwrap().written, [id]);
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn a_development_value_inverse_uses_the_labeled_legacy_path() {
    let (_dir, lib, _db, path, id) = fixture();
    let edit_id = edit(&lib, id).unwrap().action.unwrap();
    let after = fs::read(&path).unwrap();
    lib.inner()
        .writer
        .run(move |conn| {
            let mut files = journal::tag_undo(conn, edit_id, now_ms())?.unwrap();
            files[0].exact = None;
            files[0].values = vec![("GENRE".into(), Vec::new())];
            conn.execute(
                "UPDATE actions SET inverse=?2 WHERE id=?1",
                params![edit_id.0, journal::Inverse::Tags(files).encode()],
            )?;
            Ok(())
        })
        .unwrap();
    let preview = lib.preview_tag_undo(edit_id).unwrap();
    assert!(preview.skipped.is_empty(), "{preview:?}");
    assert!(lib.action(edit_id).unwrap().unwrap().undoable);
    let report = undo(&lib, edit_id).unwrap();
    assert_eq!(report.written, [id]);
    let undo_id = report.action.unwrap();
    assert!(
        lib.action(undo_id)
            .unwrap()
            .unwrap()
            .summary
            .contains("legacy value restoration")
    );
    assert_ne!(fs::read(&path).unwrap(), after);
    assert_eq!(undo(&lib, undo_id).unwrap().written, [id]);
    assert_eq!(fs::read(&path).unwrap(), after);
}

#[test]
fn recovery_keeps_an_external_change_after_publication_and_refuses_the_inverse() {
    let (_dir, lib, db, path, id) = fixture();
    *lib.inner().hooks.tag_fault.lock().unwrap() = Some("after_rename");
    assert!(edit(&lib, id).is_err());
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let mut bytes = fs::read(&path).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    fs::write(&path, &bytes).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    drop(lib);
    let lib = Library::open(&db).unwrap();
    let entries = lib.tag_recovery_diagnostics().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["stage"], "retained");
    assert!(Path::new(entries[0]["displaced"].as_str().unwrap()).exists());
    let id = action(&lib, "tag_edit").unwrap();
    let report = undo(&lib, id).unwrap();
    assert_eq!(report.skipped[0].1, "file_changed");
    assert!(report.action.is_none());
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn recovery_keeps_two_external_versions_and_releases_the_target() {
    let (_dir, lib, db, path, id) = fixture();
    *lib.inner().hooks.tag_fault.lock().unwrap() = Some("after_syscall");
    assert!(edit(&lib, id).is_err());
    let displaced = path
        .parent()
        .unwrap()
        .read_dir()
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|candidate| {
            candidate
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".player-tags-")
        })
        .unwrap();
    let change_external = |path: &Path| {
        let mut bytes = fs::read(path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(path, &bytes).unwrap();
        bytes
    };
    let current = change_external(&path);
    let previous = change_external(&displaced);
    drop(lib);
    let lib = Library::open(&db).unwrap();
    assert!(action(&lib, "tag_edit").is_none());
    assert_eq!(fs::read(&path).unwrap(), current);
    assert_eq!(fs::read(&displaced).unwrap(), previous);
    assert!(!crate::tags::job::intents::pending(lib.inner(), id, &path).unwrap());
}

fn changed_tail(path: &Path) {
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    let mut bytes = fs::read(path).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    fs::write(path, bytes).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
}

#[test]
fn publication_before_first_digest_check_keeps_the_only_complete_original() {
    let (_dir, lib, db, path, id) = fixture();
    let original = fs::read(&path).unwrap();
    *lib.inner().hooks.tag_after_publish.lock().unwrap() =
        Some(Box::new(|dest, _| changed_tail(dest)));
    *lib.inner().hooks.tag_fault.lock().unwrap() = Some("after_syscall");
    assert!(edit(&lib, id).is_err());
    let externally_saved = fs::read(&path).unwrap();
    drop(lib);
    let lib = Library::open(&db).unwrap();
    assert!(action(&lib, "tag_edit").is_none());
    let entries = lib.tag_recovery_diagnostics().unwrap();
    let backup = Path::new(entries[0]["displaced"].as_str().unwrap());
    assert_eq!(entries[0]["stage"], "retained");
    assert_eq!(fs::read(backup).unwrap(), original);
    assert_eq!(fs::read(path).unwrap(), externally_saved);
}

#[test]
fn a_missing_candidate_does_not_prove_publication() {
    let (_dir, lib, db, path, id) = fixture();
    let original = fs::read(&path).unwrap();
    *lib.inner().hooks.tag_fault.lock().unwrap() = Some("publishing");
    assert!(edit(&lib, id).is_err());
    let entries = lib.tag_recovery_diagnostics().unwrap();
    fs::remove_file(entries[0]["staged"].as_str().unwrap()).unwrap();
    drop(lib);
    let lib = Library::open(&db).unwrap();
    assert!(action(&lib, "tag_edit").is_none());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_clean(&lib, &path);
}

#[test]
fn live_external_edits_are_neither_rolled_back_nor_cleaned() {
    let (_dir, lib, _db, path, id) = fixture();
    let original = fs::read(&path).unwrap();
    *lib.inner().hooks.tag_after_publish.lock().unwrap() =
        Some(Box::new(|dest, _| changed_tail(dest)));
    let report = edit(&lib, id).unwrap();
    assert!(report.action.is_none());
    assert_eq!(report.failed, [(id, "write_failed")]);
    let external = fs::read(&path).unwrap();
    assert_ne!(external, original);
    let entries = lib.tag_recovery_diagnostics().unwrap();
    let backup = Path::new(entries[0]["displaced"].as_str().unwrap());
    assert_eq!(fs::read(backup).unwrap(), original);
    assert_eq!(fs::read(path).unwrap(), external);
}

#[cfg(unix)]
#[test]
fn a_source_field_changed_after_parsing_is_never_overwritten() {
    let (_dir, lib, _db, path, id) = fixture();
    let artist = TagChange::Set {
        key: "ARTIST".into(),
        values: vec!["AuthorA".into()],
    };
    lib.edit_tags(&[id], &[artist], UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    let original = fs::read(&path).unwrap();
    let offset = original
        .windows(7)
        .position(|bytes| bytes == b"AuthorA")
        .unwrap();
    *lib.inner().hooks.tag_after_plan.lock().unwrap() = Some(Box::new(move |source| {
        let modified = fs::metadata(source).unwrap().modified().unwrap();
        let mut bytes = fs::read(source).unwrap();
        bytes[offset..offset + 7].copy_from_slice(b"AuthorB");
        fs::write(source, bytes).unwrap();
        fs::File::options()
            .write(true)
            .open(source)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }));
    let report = edit(&lib, id).unwrap();
    assert_eq!(report.skipped, [(id, "file_changed")], "{report:?}");
    assert!(report.action.is_none());
    let mut expected = original;
    expected[offset..offset + 7].copy_from_slice(b"AuthorB");
    assert_eq!(fs::read(path).unwrap(), expected);
}

#[cfg(unix)]
#[test]
fn exclusive_fallback_never_overwrites_a_file_created_in_the_gap() {
    let (_dir, lib, _db, path, id) = fixture();
    *lib.inner().hooks.tag_publish_mode.lock().unwrap() =
        Some(TagPublish::Exclusive { stable_ids: true });
    let original = fs::read(&path).unwrap();
    let initial = edit(&lib, id).unwrap();
    assert_eq!(initial.written, [id]);
    assert_eq!(undo(&lib, initial.action.unwrap()).unwrap().written, [id]);
    assert_eq!(fs::read(&path).unwrap(), original);
    *lib.inner().hooks.tag_exclusive_gap.lock().unwrap() = Some(Box::new(|path| {
        assert!(!path.exists());
        fs::write(path, b"external save").unwrap();
    }));
    let report = edit(&lib, id).unwrap();
    assert!(report.action.is_none());
    assert_eq!(fs::read(path).unwrap(), b"external save");
    let entries = lib.tag_recovery_diagnostics().unwrap();
    assert_eq!(
        fs::read(entries[0]["displaced"].as_str().unwrap()).unwrap(),
        original
    );
}

#[test]
fn recovery_does_not_attribute_an_external_write_to_an_unpublished_intent() {
    let (_dir, lib, db, path, id) = fixture();
    *lib.inner().hooks.tag_fault.lock().unwrap() = Some("prepared");
    assert!(edit(&lib, id).is_err());
    let staged = path
        .parent()
        .unwrap()
        .read_dir()
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|candidate| candidate.extension().is_some_and(|ext| ext == "tmp"))
        .unwrap();
    let bytes = fs::read(staged).unwrap();
    fs::write(&path, &bytes).unwrap();
    drop(lib);
    let lib = Library::open(&db).unwrap();
    assert_clean(&lib, &path);
    assert!(action(&lib, "tag_edit").is_none());
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn source_is_revalidated_inside_the_intent_registration_transaction() {
    let (_dir, lib, _db, path, id) = fixture();
    let edit_id = edit(&lib, id).unwrap().action.unwrap();
    let edited = fs::read(&path).unwrap();
    let library = lib.clone();
    let report = lib
        .undo_tag_edit(
            edit_id,
            UI,
            TagJobOptions {
                in_use: Arc::new(move |_| {
                    library
                        .inner()
                        .writer
                        .run(move |conn| {
                            conn.execute("UPDATE actions SET at_ms=0 WHERE id=?1", [edit_id.0])?;
                            journal::prune(conn, now_ms())
                        })
                        .unwrap();
                    false
                }),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap()
        .wait();
    assert!(report.is_err(), "{report:?}");
    assert_clean(&lib, &path);
    assert_eq!(fs::read(&path).unwrap(), edited);
}

#[test]
fn edits_after_the_last_check_preserve_actual_destination_bytes() {
    for change_candidate in [false, true] {
        let (_dir, lib, _db, path, id) = fixture();
        let original = fs::read(&path).unwrap();
        let mut external = original.clone();
        *external.last_mut().unwrap() ^= 1;
        let to_write = external.clone();
        *lib.inner().hooks.tag_before_publish.lock().unwrap() =
            Some(Box::new(move |dest, staged| {
                if change_candidate {
                    let modified = fs::metadata(staged).unwrap().modified().unwrap();
                    let mut bytes = fs::read(staged).unwrap();
                    *bytes.last_mut().unwrap() ^= 1;
                    fs::write(staged, &bytes).unwrap();
                    fs::File::options()
                        .write(true)
                        .open(staged)
                        .unwrap()
                        .set_times(fs::FileTimes::new().set_modified(modified))
                        .unwrap();
                } else {
                    let replacement = dest.with_file_name("external.tmp");
                    fs::write(&replacement, &to_write).unwrap();
                    #[cfg(not(windows))]
                    fs::rename(replacement, dest).unwrap();
                    #[cfg(windows)]
                    {
                        let saved = dest.with_file_name("external-saved.tmp");
                        mpmp_fs_desktop::paths::replace_with_backup(&replacement, dest, &saved)
                            .unwrap();
                        fs::remove_file(saved).unwrap();
                    }
                }
            }));
        let report = edit(&lib, id).unwrap();
        assert_eq!(report.skipped, [(id, "file_changed")], "{report:?}");
        assert!(report.action.is_none());
        assert_eq!(
            fs::read(&path).unwrap(),
            if change_candidate { original } else { external }
        );
        if change_candidate {
            let entries = lib.tag_recovery_diagnostics().unwrap();
            assert_eq!(entries[0]["stage"], "retained");
            let staged = Path::new(entries[0]["staged"].as_str().unwrap());
            assert!(staged.exists());
        } else {
            assert_clean(&lib, &path);
        }
    }
}

#[cfg(unix)]
#[test]
fn an_unreadable_recovery_member_leaves_the_other_member_undoable() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, lib, db, path, id) = fixture();
    let other = path.with_file_name("other.flac");
    let original = fs::read(&path).unwrap();
    fs::write(&other, &original).unwrap();
    scan(&lib);
    let other_id = lib
        .track_ids(&TrackQuery::default(), 10)
        .unwrap()
        .into_iter()
        .find(|t| *t != id)
        .unwrap();
    *lib.inner().hooks.tag_fault.lock().unwrap() = Some("before_action");
    assert!(
        lib.edit_tags(&[id, other_id], &changes(), UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .is_err()
    );
    drop(lib);
    let permissions = fs::metadata(&path).unwrap().permissions();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
    if fs::File::open(&path).is_ok() {
        fs::set_permissions(&path, permissions).unwrap();
        return; // Superuser bypasses the permission check.
    }
    let lib = Library::open(&db).unwrap();
    let recovered = action(&lib, "tag_edit").unwrap();
    assert_eq!(lib.tag_edit_tracks(recovered).unwrap(), [other_id]);
    assert!(pending(lib.inner(), id, &path).unwrap());
    let report = undo(&lib, recovered).unwrap();
    assert_eq!(report.written, [other_id], "{report:?}");
    assert_eq!(fs::read(other).unwrap(), original);
    fs::set_permissions(&path, permissions).unwrap();
}

#[test]
fn a_deleted_destination_after_publication_has_a_historical_action() {
    let (_dir, lib, db, path, id) = fixture();
    *lib.inner().hooks.tag_fault.lock().unwrap() = Some("after_rename");
    assert!(edit(&lib, id).is_err());
    fs::remove_file(&path).unwrap();
    drop(lib);
    let lib = Library::open(&db).unwrap();
    let recovered = action(&lib, "tag_edit").unwrap();
    assert_eq!(lib.tag_edit_tracks(recovered).unwrap(), [id]);
    assert!(matches!(
        undo(&lib, recovered),
        Err(LibraryError::ActionConflict { .. })
    ));
    assert!(!path.exists());
    assert!(!pending(lib.inner(), id, &path).unwrap());
}

#[test]
fn legacy_undo_that_already_has_the_values_commits_without_a_physical_write() {
    let (_dir, lib, _db, path, id) = fixture();
    let edit_id = edit(&lib, id).unwrap().action.unwrap();
    let after = fs::read(&path).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    lib.inner()
        .writer
        .run(move |conn| {
            let mut files = journal::tag_undo(conn, edit_id, now_ms())?.unwrap();
            files[0].exact = None;
            files[0].values = vec![("GENRE".into(), vec!["Written".into()])];
            conn.execute(
                "UPDATE actions SET inverse=?2 WHERE id=?1",
                params![edit_id.0, journal::Inverse::Tags(files).encode()],
            )?;
            Ok(())
        })
        .unwrap();
    let undo_report = undo(&lib, edit_id).unwrap();
    assert_eq!(undo_report.completed, [id]);
    assert!(undo_report.written.is_empty());
    let redo = undo(&lib, undo_report.action.unwrap()).unwrap();
    assert_eq!(redo.completed, [id]);
    assert!(redo.written.is_empty());
    assert_eq!(fs::read(&path).unwrap(), after);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    assert_clean(&lib, &path);
}

#[test]
fn pruning_preserves_an_active_redos_expired_ancestors() {
    let (_dir, lib, _db, path, id) = fixture();
    let original = fs::read(&path).unwrap();
    let edited = edit(&lib, id).unwrap().action.unwrap();
    let after = fs::read(&path).unwrap();
    let undone = undo(&lib, edited).unwrap().action.unwrap();
    lib.inner()
        .writer
        .run(move |conn| {
            conn.execute("UPDATE actions SET at_ms=0 WHERE id=?1", [edited.0])?;
            journal::prune(conn, now_ms())
        })
        .unwrap();
    let redone = undo(&lib, undone).unwrap().action.unwrap();
    assert_eq!(fs::read(&path).unwrap(), after);
    assert!(lib.action(edited).unwrap().is_some());
    assert_eq!(undo(&lib, redone).unwrap().written, [id]);
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn undo_transfers_a_bidirectional_image_with_no_new_edit_quota() {
    let (_dir, lib, _db, path, id) = fixture();
    let original = fs::read(&path).unwrap();
    let edited = edit(&lib, id).unwrap().action.unwrap();
    lib.inner()
        .writer
        .run(move |conn| {
            conn.execute(
                "INSERT INTO actions(at_ms,origin,kind,summary,target_count,inverse)
             VALUES(?1,'ui','tag_edit','quota fixture',0,zeroblob(?2))",
                params![now_ms(), tag_edit::MAX_TAG_HISTORY_BYTES as i64],
            )?;
            assert_eq!(budget(conn, false)?, 0);
            assert!(budget(conn, true)? > 0);
            Ok(())
        })
        .unwrap();
    assert!(lib.preview_tag_undo(edited).unwrap().skipped.is_empty());
    assert_eq!(undo(&lib, edited).unwrap().written, [id]);
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn failed_publication_restores_an_externally_changed_displaced_file() {
    let (_dir, lib, _db, path, id) = fixture();
    *lib.inner().hooks.tag_publish_mode.lock().unwrap() =
        Some(TagPublish::Exclusive { stable_ids: true });
    *lib.inner().hooks.tag_exclusive_gap.lock().unwrap() = Some(Box::new(|dest| {
        let entries = dest.parent().unwrap().read_dir().unwrap().flatten();
        for entry in entries {
            let path = entry.path();
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("backup") => fs::write(path, b"external save to displaced file").unwrap(),
                Some("tmp") => fs::remove_file(path).unwrap(),
                _ => {}
            }
        }
    }));
    let report = edit(&lib, id).unwrap();
    assert!(report.action.is_none());
    assert_eq!(fs::read(path).unwrap(), b"external save to displaced file");
}

#[cfg(unix)]
#[test]
fn a_new_external_write_before_rollback_is_preserved() {
    let (_dir, lib, _db, path, id) = fixture();
    let original = fs::read(&path).unwrap();
    *lib.inner().hooks.tag_before_publish.lock().unwrap() = Some(Box::new(move |dest, _| {
        let replacement = dest.with_file_name("outside.tmp");
        fs::write(&replacement, &original).unwrap();
        fs::rename(replacement, dest).unwrap();
    }));
    let after = Arc::new(Mutex::new(None));
    let observed = after.clone();
    *lib.inner().hooks.tag_before_rollback.lock().unwrap() = Some(Box::new(move |dest, _| {
        changed_tail(dest);
        *observed.lock().unwrap() = Some(fs::read(dest).unwrap());
    }));
    let report = edit(&lib, id).unwrap();
    assert!(report.action.is_none());
    assert_eq!(
        fs::read(path).unwrap(),
        after.lock().unwrap().take().unwrap()
    );
    assert_eq!(
        lib.tag_recovery_diagnostics().unwrap()[0]["stage"],
        "retained"
    );
}

#[cfg(windows)]
#[test]
fn windows_holds_deny_write_from_planning_through_cleanup() {
    let (_dir, lib, _db, path, id) = fixture();
    *lib.inner().hooks.tag_after_plan.lock().unwrap() = Some(Box::new(|path| {
        assert!(fs::File::options().write(true).open(path).is_err());
    }));
    *lib.inner().hooks.tag_before_cleanup.lock().unwrap() = Some(Box::new(|path| {
        assert!(fs::File::options().write(true).open(path).is_err());
    }));
    assert_eq!(edit(&lib, id).unwrap().written, [id]);
    assert_clean(&lib, &path);
}

#[cfg(unix)]
#[test]
fn corrupt_exclusive_candidate_does_not_record_edit_undo_or_redo() {
    for operation in 0..3 {
        let (_dir, lib, _db, path, id) = fixture();
        *lib.inner().hooks.tag_publish_mode.lock().unwrap() = Some(TagPublish::Link);
        let source = match operation {
            0 => None,
            1 => edit(&lib, id).unwrap().action,
            _ => {
                let edit = edit(&lib, id).unwrap().action.unwrap();
                undo(&lib, edit).unwrap().action
            }
        };
        let before = fs::read(&path).unwrap();
        let hooks = lib.inner().hooks.clone();
        *hooks.tag_exclusive_gap.lock().unwrap() = Some(Box::new(|path| {
            assert!(!path.exists());
            let candidate = path
                .parent()
                .unwrap()
                .read_dir()
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|entry| entry.extension().is_some_and(|s| s == "tmp"))
                .unwrap();
            changed_tail(&candidate);
        }));
        let result = match source {
            Some(action) => undo(&lib, action).unwrap(),
            None => edit(&lib, id).unwrap(),
        };
        assert!(result.written.is_empty(), "{result:?}");
        assert_eq!(result.failed, [(id, "write_failed")]);
        assert!(result.action.is_none());
        if let Some(action) = source {
            assert!(lib.action(action).unwrap().unwrap().undone_by.is_none());
        }
        let entries = lib.tag_recovery_diagnostics().unwrap();
        let row = entries.iter().find(|row| row["action"].is_null()).unwrap();
        assert_eq!(row["stage"], "retained");
        assert_eq!(
            fs::read(row["displaced"].as_str().unwrap()).unwrap(),
            before
        );
    }
}

#[cfg(unix)]
#[test]
fn scan_missing_observation_is_invalidated_across_the_displacement_gap() {
    use std::sync::mpsc;
    use std::time::Duration;

    fn gate() -> (
        impl Fn() + Send + Sync,
        mpsc::Receiver<()>,
        mpsc::Sender<()>,
    ) {
        let (arrived, waiting) = mpsc::channel();
        let (go, released) = mpsc::channel();
        let released = Mutex::new(released);
        (
            move || {
                arrived.send(()).unwrap();
                released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(30))
                    .unwrap();
            },
            waiting,
            go,
        )
    }

    let (_dir, lib, _db, path, id) = fixture();
    fs::copy(&path, path.with_file_name("another.flac")).unwrap();
    scan(&lib);
    let hooks = &lib.inner().hooks;
    *hooks.tag_publish_mode.lock().unwrap() = Some(TagPublish::Link);
    let (before_walk, at_before, walk_now) = gate();
    let (after_walk, at_after, commit_now) = gate();
    let (in_gap, at_gap, publish_now) = gate();
    *hooks.scan_before_walk.lock().unwrap() = Some(Arc::new(before_walk));
    *hooks.scan_after_walk.lock().unwrap() = Some(Arc::new(after_walk));
    *hooks.tag_exclusive_gap.lock().unwrap() = Some(Box::new(move |path| {
        assert!(!path.exists());
        in_gap();
    }));
    let ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() else {
        panic!("scan")
    };
    let timeout = Duration::from_secs(30);
    at_before.recv_timeout(timeout).unwrap();
    let edit = lib
        .edit_tags(&[id], &changes(), UI, TagJobOptions::default())
        .unwrap();
    at_gap.recv_timeout(timeout).unwrap();
    walk_now.send(()).unwrap();
    at_after.recv_timeout(timeout).unwrap();
    publish_now.send(()).unwrap();
    assert_eq!(edit.wait().unwrap().written, [id]);
    commit_now.send(()).unwrap();
    let report = scan.wait().unwrap();
    assert_eq!(report.removed, 0);
    assert!(
        lib.track_ids(&TrackQuery::default(), 10)
            .unwrap()
            .contains(&id)
    );
}

#[test]
fn a_v1_intent_can_be_retained_and_opened_twice() {
    let (_dir, lib, db, path, id) = fixture();
    *lib.inner().hooks.tag_fault.lock().unwrap() = Some("prepared");
    assert!(edit(&lib, id).is_err());
    lib.inner()
        .writer
        .run(|conn| {
            conn.execute(
                "UPDATE file_write_intents SET format_version=1,
             target_json=json_remove(target_json,'$.displaced','$.parent_identity')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    fs::write(&path, b"external version").unwrap();
    drop(lib);
    for _ in 0..2 {
        let lib = Library::open(&db).unwrap();
        assert!(action(&lib, "tag_edit").is_none());
        assert_eq!(
            lib.tag_recovery_diagnostics().unwrap()[0]["stage"],
            "retained"
        );
        assert_eq!(fs::read(&path).unwrap(), b"external version");
    }
}

#[test]
fn unreadable_tag_intents_protect_every_track_until_retained() {
    let (_dir, lib, db, path, id) = fixture();
    let original = fs::read(&path).unwrap();
    let missing = path.with_file_name("missing.flac");
    fs::write(&missing, &original).unwrap();
    scan(&lib);
    let missing_id = lib
        .track_ids(&TrackQuery::default(), 10)
        .unwrap()
        .into_iter()
        .find(|track| *track != id)
        .unwrap();
    let group = Group::new(&UI.owned(), None, &changes()).unwrap();
    let authorization = json(&group).unwrap();
    lib.inner()
        .writer
        .run(move |conn| {
            for (family, target) in [
                ("tag_edit", "{"),
                ("tag_edit", "{}"),
                ("cover_edit", "{"),
                ("unknown-test-family", "{"),
            ] {
                conn.execute(
                    "INSERT INTO file_write_intents(family,format_version,authorization_json,
                 target_json,before_digest,before_image,after_digest,stage,created_at,updated_at)
                 VALUES(?1,1,?2,?3,'','', '', 'prepared',1,1)",
                    params![family, authorization, target],
                )?;
            }
            Ok(())
        })
        .unwrap();
    drop(lib);
    let lib = Library::open(&db).unwrap();
    let entries = lib.tag_recovery_diagnostics().unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|row| row["stage"] == "unreadable" && row["blocks_metadata_writes"] == true)
            .count(),
        2
    );
    assert!(lib.has_cover_recovery_required());
    fs::remove_file(missing).unwrap();
    let added = path.with_file_name("added.flac");
    fs::write(&added, &original).unwrap();
    scan(&lib);
    assert!(lib.track(missing_id).unwrap().unwrap().available);
    assert!(
        lib.track_ids(&TrackQuery::default(), 10)
            .unwrap()
            .into_iter()
            .map(|id| lib.track(id).unwrap().unwrap())
            .any(|track| track.rel_path == "added.flac" && track.available)
    );
    assert!(matches!(
        lib.preview_tag_edit(&[id], &changes()),
        Err(LibraryError::CoverRecoveryRequired)
    ));
    assert!(matches!(
        edit(&lib, id),
        Err(LibraryError::CoverRecoveryRequired)
    ));
    let remove = crate::CoverChange::Remove;
    assert!(matches!(
        lib.preview_cover_edit(&[id], &remove),
        Err(LibraryError::CoverRecoveryRequired)
    ));
    assert!(matches!(
        lib.edit_cover(&[id], &remove, UI, TagJobOptions::default())
            .unwrap()
            .wait(),
        Err(LibraryError::CoverRecoveryRequired)
    ));
    assert_eq!(fs::read(&path).unwrap(), original);
    lib.inner()
        .writer
        .run(|conn| {
            conn.execute(
                "UPDATE file_write_intents SET stage='retained' WHERE family='tag_edit'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    scan(&lib);
    assert!(!lib.track(missing_id).unwrap().unwrap().available);
    assert_eq!(edit(&lib, id).unwrap().written, [id]);
    lib.inner()
        .pool
        .read(|conn| {
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM file_write_intents WHERE target_json IN ('{','{}')",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
                4
            );
            Ok(())
        })
        .unwrap();
}

#[cfg(unix)]
#[test]
fn recovery_in_installing_restores_before_on_both_fallback_protocols() {
    for protocol in &UNIX_PROTOCOLS[1..] {
        let (_dir, lib, db, path, id) = fixture();
        let original = fs::read(&path).unwrap();
        *lib.inner().hooks.tag_publish_mode.lock().unwrap() = Some(*protocol);
        *lib.inner().hooks.tag_fault.lock().unwrap() = Some("installing");
        assert!(edit(&lib, id).is_err());
        assert!(!path.exists());
        let entries = lib.tag_recovery_diagnostics().unwrap();
        assert_eq!(entries[0]["stage"], "installing");
        assert!(Path::new(entries[0]["staged"].as_str().unwrap()).is_file());
        assert!(Path::new(entries[0]["displaced"].as_str().unwrap()).is_file());
        drop(lib);

        let lib = Library::open(&db).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(action(&lib, "tag_edit").is_none());
        assert_clean(&lib, &path);
        scan(&lib);
        assert!(lib.track(id).unwrap().unwrap().available);
    }
}

#[cfg(unix)]
#[test]
fn recovery_in_installing_requires_an_unchanged_private_candidate() {
    for extra_link in [false, true] {
        let (_dir, lib, db, path, id) = fixture();
        let original = fs::read(&path).unwrap();
        *lib.inner().hooks.tag_publish_mode.lock().unwrap() = Some(TagPublish::Link);
        *lib.inner().hooks.tag_fault.lock().unwrap() = Some("installing");
        assert!(edit(&lib, id).is_err());
        let entries = lib.tag_recovery_diagnostics().unwrap();
        let staged = Path::new(entries[0]["staged"].as_str().unwrap());
        if extra_link {
            fs::hard_link(staged, path.with_file_name("external.flac")).unwrap();
        } else {
            fs::write(staged, b"external bytes").unwrap();
        }
        drop(lib);

        let lib = Library::open(&db).unwrap();
        assert!(!path.exists());
        assert!(action(&lib, "tag_edit").is_none());
        let entries = lib.tag_recovery_diagnostics().unwrap();
        assert_eq!(entries[0]["stage"], "retained");
        assert!(Path::new(entries[0]["staged"].as_str().unwrap()).is_file());
        assert_eq!(
            fs::read(entries[0]["displaced"].as_str().unwrap()).unwrap(),
            original
        );
    }
}

#[cfg(unix)]
#[test]
fn recovery_restores_the_public_name_before_decoding_a_corrupt_inverse() {
    for (protocol, point) in [
        (TagPublish::Link, "displace_gap"),
        (TagPublish::Exclusive { stable_ids: true }, "installing"),
        (TagPublish::Link, "installing"),
    ] {
        let (_dir, lib, db, path, id) = fixture();
        let original = fs::read(&path).unwrap();
        *lib.inner().hooks.tag_publish_mode.lock().unwrap() = Some(protocol);
        *lib.inner().hooks.tag_fault.lock().unwrap() = Some(point);
        assert!(edit(&lib, id).is_err());
        assert!(!path.exists());
        lib.inner()
            .writer
            .run(|conn| {
                conn.execute(
                    "UPDATE file_write_intents SET before_image=X'00' WHERE family='tag_edit'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        drop(lib);

        let lib = Library::open(&db).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(action(&lib, "tag_edit").is_none());
        let entries = lib.tag_recovery_diagnostics().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["stage"], "unreadable");
        assert_eq!(entries[0]["blocks_metadata_writes"], true);
        assert!(Path::new(entries[0]["staged"].as_str().unwrap()).exists());
    }
}

#[cfg(unix)]
#[test]
fn recovery_cleans_an_empty_link_backup_reservation_before_displacement() {
    let (_dir, lib, db, path, id) = fixture();
    let original = fs::read(&path).unwrap();
    *lib.inner().hooks.tag_publish_mode.lock().unwrap() = Some(TagPublish::Link);
    *lib.inner().hooks.tag_fault.lock().unwrap() = Some("displacing");
    assert!(edit(&lib, id).is_err());
    let entries = lib.tag_recovery_diagnostics().unwrap();
    assert_eq!(entries[0]["stage"], "displacing");
    fs::File::create_new(entries[0]["displaced"].as_str().unwrap()).unwrap();
    drop(lib);

    let lib = Library::open(&db).unwrap();
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(action(&lib, "tag_edit").is_none());
    assert_clean(&lib, &path);
}

#[cfg(unix)]
#[test]
fn recovery_at_first_move_restores_before_even_with_an_unreadable_candidate() {
    for unreadable in [false, true] {
        let (_dir, lib, db, path, id) = fixture();
        let original = fs::read(&path).unwrap();
        *lib.inner().hooks.tag_publish_mode.lock().unwrap() = Some(TagPublish::Link);
        *lib.inner().hooks.tag_fault.lock().unwrap() = Some("displace_gap");
        assert!(edit(&lib, id).is_err());
        assert!(!path.exists());
        let entries = lib.tag_recovery_diagnostics().unwrap();
        let candidate = PathBuf::from(entries[0]["staged"].as_str().unwrap());
        if unreadable {
            fs::remove_file(&candidate).unwrap();
            fs::create_dir(&candidate).unwrap();
        }
        drop(lib);
        let lib = Library::open(&db).unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(action(&lib, "tag_edit").is_none());
        assert_eq!(candidate.exists(), unreadable);
        assert_eq!(
            lib.tag_recovery_diagnostics().unwrap().len(),
            usize::from(unreadable)
        );
    }
}

#[cfg(unix)]
#[test]
fn recovery_does_not_resurrect_an_external_delete_after_second_move() {
    let (_dir, lib, db, path, id) = fixture();
    let original = fs::read(&path).unwrap();
    *lib.inner().hooks.tag_publish_mode.lock().unwrap() = Some(TagPublish::Link);
    *lib.inner().hooks.tag_fault.lock().unwrap() = Some("after_syscall");
    assert!(edit(&lib, id).is_err());
    fs::remove_file(&path).unwrap();
    drop(lib);
    let lib = Library::open(&db).unwrap();
    assert!(!path.exists());
    assert!(action(&lib, "tag_edit").is_none());
    let entries = lib.tag_recovery_diagnostics().unwrap();
    assert_eq!(
        fs::read(entries[0]["displaced"].as_str().unwrap()).unwrap(),
        original
    );
}

#[cfg(unix)]
#[test]
fn recovery_rejects_a_replacement_directory_even_when_names_are_missing() {
    let (_dir, lib, db, path, id) = fixture();
    let original = fs::read(&path).unwrap();
    *lib.inner().hooks.tag_publish_mode.lock().unwrap() = Some(TagPublish::Link);
    *lib.inner().hooks.tag_fault.lock().unwrap() = Some("displace_gap");
    assert!(edit(&lib, id).is_err());
    let entries = lib.tag_recovery_diagnostics().unwrap();
    let backup = Path::new(entries[0]["displaced"].as_str().unwrap());
    let parent = path.parent().unwrap();
    let moved = parent.with_file_name("moved");
    fs::rename(parent, &moved).unwrap();
    fs::create_dir(parent).unwrap();
    drop(lib);
    let lib = Library::open(&db).unwrap();
    assert!(!path.exists());
    assert_eq!(
        fs::read(moved.join(backup.file_name().unwrap())).unwrap(),
        original
    );
    assert_eq!(lib.tag_recovery_diagnostics().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn a_write_through_an_open_original_handle_after_verification_is_retained() {
    use std::io::Write;

    let (_dir, lib, db, path, id) = fixture();
    let outside = Mutex::new(fs::File::options().append(true).open(&path).unwrap());
    *lib.inner().hooks.tag_before_cleanup.lock().unwrap() = Some(Box::new(move |_| {
        outside
            .lock()
            .unwrap()
            .write_all(b"external write through old handle")
            .unwrap();
    }));
    let report = edit(&lib, id).unwrap();
    assert_eq!(report.written, [id]);
    let entries = lib.tag_recovery_diagnostics().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["stage"], "retained");
    let saved = entries[0]["displaced"].as_str().unwrap();
    assert!(
        fs::read(saved)
            .unwrap()
            .ends_with(b"external write through old handle")
    );
    drop(lib);
    let again = Library::open(&db).unwrap();
    assert_eq!(again.tag_recovery_diagnostics().unwrap().len(), 1);
    assert!(
        fs::read(saved)
            .unwrap()
            .ends_with(b"external write through old handle")
    );
}

#[cfg(unix)]
#[test]
fn a_same_content_replacement_of_the_displaced_name_after_verification_is_retained() {
    let (_dir, lib, _db, path, id) = fixture();
    let before = fs::read(&path).unwrap();
    let original = before.clone();
    *lib.inner().hooks.tag_before_cleanup.lock().unwrap() = Some(Box::new(move |path| {
        let replacement = path.with_extension("external");
        fs::write(&replacement, &original).unwrap();
        fs::rename(replacement, path).unwrap();
    }));
    assert_eq!(edit(&lib, id).unwrap().written, [id]);
    let entries = lib.tag_recovery_diagnostics().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["stage"], "retained");
    assert_eq!(
        fs::read(entries[0]["displaced"].as_str().unwrap()).unwrap(),
        before
    );
}

#[cfg(unix)]
#[test]
fn restart_in_rollback_gap_restores_the_displaced_external_version() {
    let (_dir, lib, db, path, id) = fixture();
    let mut external = fs::read(&path).unwrap();
    *external.last_mut().unwrap() ^= 1;
    *lib.inner().hooks.tag_publish_mode.lock().unwrap() = Some(TagPublish::Link);
    *lib.inner().hooks.tag_before_publish.lock().unwrap() =
        Some(Box::new(|path, _| changed_tail(path)));
    *lib.inner().hooks.tag_fault.lock().unwrap() = Some("rollback_gap");
    assert!(edit(&lib, id).is_err());
    assert!(!path.exists());
    drop(lib);
    let lib = Library::open(&db).unwrap();
    assert_eq!(fs::read(&path).unwrap(), external);
    assert!(action(&lib, "tag_edit").is_none());
    assert_eq!(
        lib.tag_recovery_diagnostics().unwrap()[0]["stage"],
        "retained"
    );
}
