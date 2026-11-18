//! Undo scenarios.
use super::*;

#[test]
fn accepted_retained_copies_do_not_block_a_valid_cover_undo() {
    let f = Fixture::new();
    let before = std::fs::read(&f.path).unwrap();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_action");
    assert!(f.edit().is_err());
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = None;
    let action = f
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
        .find(|a| a.kind == "cover_edit")
        .unwrap()
        .id;
    let guard = f.lib.describe_cover_recovery(f.id).unwrap();
    f.lib.accept_cover_recovery(&guard, UI).unwrap();
    assert_eq!(targets(&f).len(), 1);
    let undo = f
        .lib
        .undo_cover_edit(action, UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(undo.written, [f.id]);
    assert_eq!(std::fs::read(&f.path).unwrap(), before);
    let t = targets(&f).pop().unwrap();
    assert!(t.backup.unwrap().path.exists());
}

#[test]
fn corrupted_undo_regions_cannot_publish_a_result_with_the_wrong_before_digest() {
    let f = Fixture::new();
    let action = f.edit().unwrap().action.unwrap();
    let after = std::fs::read(&f.path).unwrap();
    f.lib
        .inner
        .writer
        .run(move |conn| {
            let bytes: Vec<u8> =
                conn.query_row("SELECT inverse FROM actions WHERE id=?1", [action.0], |r| {
                    r.get(0)
                })?;
            let Some(journal::Inverse::Covers(mut files)) = journal::Inverse::decode(&bytes) else {
                panic!()
            };
            files[0].regions[0].bytes[0] ^= 1;
            conn.execute(
                "UPDATE actions SET inverse=?2 WHERE id=?1",
                params![action.0, journal::Inverse::Covers(files).encode()],
            )?;
            Ok(())
        })
        .unwrap();
    let r = f
        .lib
        .undo_cover_edit(action, UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert!(r.written.is_empty());
    assert_eq!(std::fs::read(&f.path).unwrap(), after);
}

#[test]
fn undo_redo_occupancy_and_conflict_preflight() {
    let f = Fixture::new();
    let original = std::fs::read(&f.path).unwrap();
    let original_cover = f.lib.cover_art(f.id).unwrap();
    let options = TagJobOptions {
        in_use: Arc::new(|_| true),
        ..Default::default()
    };
    let skipped = f
        .lib
        .edit_cover(&[f.id], &f.change(), UI, options)
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(skipped.skipped, [(f.id, "file_in_use")]);
    assert_eq!(f.cover_actions(), 0);
    let r = f.edit().unwrap();
    assert_eq!(r.written, [f.id]);
    let changed = std::fs::read(&f.path).unwrap();
    let action = r.action.unwrap();
    assert!(f.lib.tracks(&[f.id]).unwrap()[0].cover_revision.is_some());
    let changed_cover = f.lib.cover_art(f.id).unwrap();
    assert_eq!(
        f.lib.cover_art_after_undo(action, f.id).unwrap(),
        original_cover
    );
    assert_eq!(std::fs::read(&f.path).unwrap(), changed);
    let undo = f
        .lib
        .undo_cover_edit(action, UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(undo.written, [f.id]);
    assert_eq!(std::fs::read(&f.path).unwrap(), original);
    assert_eq!(
        f.lib
            .cover_art_after_undo(undo.action.unwrap(), f.id)
            .unwrap(),
        changed_cover
    );
    let redo = f
        .lib
        .undo_cover_edit(undo.action.unwrap(), UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(std::fs::read(&f.path).unwrap(), changed);
    let options = TagJobOptions {
        in_use: Arc::new(|_| true),
        ..Default::default()
    };
    assert!(matches!(
        f.lib
            .undo_cover_edit(redo.action.unwrap(), UI, options)
            .unwrap()
            .unwrap()
            .wait(),
        Err(LibraryError::InUse(_))
    ));
    let mut edited = changed.clone();
    *edited.last_mut().unwrap() ^= 1;
    std::fs::write(&f.path, &edited).unwrap();
    assert!(matches!(
        f.lib
            .undo_cover_edit(redo.action.unwrap(), UI, TagJobOptions::default())
            .unwrap()
            .unwrap()
            .wait(),
        Err(LibraryError::ActionConflict { .. })
    ));
    assert_eq!(std::fs::read(&f.path).unwrap(), edited);
}

#[test]
fn multi_file_cover_and_tag_undo_preserve_original_journal_targets() {
    const SELECT_ACTION_TARGETS: &str = r#"
SELECT kind,object_id,version_after,state_before,state_after FROM action_targets
WHERE action_id=?1 ORDER BY kind,object_id
"#;

    for cover in [true, false] {
        let f = Fixture::new();
        for number in 2..=15 {
            tagged_copy(
                "sine44_a.mp3",
                &f.path.with_file_name(format!("{number}.mp3")),
            );
        }
        scan(&f);
        let ids = f.lib.track_ids(&crate::TrackQuery::default(), 100).unwrap();
        assert_eq!(ids.len(), 15);
        let action = if cover {
            f.lib
                .edit_cover(&ids, &f.change(), UI, TagJobOptions::default())
                .unwrap()
                .wait()
                .unwrap()
                .action
                .unwrap()
        } else {
            f.lib
                .edit_tags(
                    &ids,
                    &[crate::TagChange::Set {
                        key: "TITLE".into(),
                        values: vec!["Updated title".into()],
                    }],
                    UI,
                    TagJobOptions::default(),
                )
                .unwrap()
                .wait()
                .unwrap()
                .action
                .unwrap()
        };
        let facts = || {
            f.lib
                .inner
                .pool
                .read(|conn| {
                    Ok(conn
                        .prepare(SELECT_ACTION_TARGETS)?
                        .query_map([action.0], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, i64>(4)?,
                            ))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?)
                })
                .unwrap()
        };
        let before = facts();
        let original = f.lib.action(action).unwrap().unwrap();
        let undo = if cover {
            let report = f
                .lib
                .undo_cover_edit(action, UI, TagJobOptions::default())
                .unwrap()
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(report.written.len(), 15);
            assert!(report.failed.is_empty());
            report.action.unwrap()
        } else {
            let report = f
                .lib
                .undo_tag_edit(action, UI, TagJobOptions::default())
                .unwrap()
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(report.written.len(), 15);
            report.action.unwrap()
        };
        let after = f.lib.action(action).unwrap().unwrap();
        assert_eq!(after.target_count, 15);
        assert_eq!(after.summary, original.summary);
        assert_eq!(after.undone_by, Some(undo));
        assert_eq!(
            facts(),
            before,
            "operation-time targets must remain unchanged, cover={cover}"
        );
        assert_eq!(f.lib.action(undo).unwrap().unwrap().target_count, 15);
    }
}

#[test]
fn partial_cover_undo_keeps_failed_targets_retryable() {
    let f = Fixture::new();
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
    let ids = [f.id, other_id];
    let edit = f
        .lib
        .edit_cover(&ids, &f.change(), UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(edit.written, ids);
    let action = edit.action.unwrap();
    let permissions = std::fs::metadata(&other).unwrap().permissions();
    let blocked = other.clone();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let options = TagJobOptions {
        in_use: Arc::new(move |_| {
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 3 {
                let mut p = std::fs::metadata(&blocked).unwrap().permissions();
                p.set_readonly(true);
                std::fs::set_permissions(&blocked, p).unwrap();
            }
            false
        }),
        ..Default::default()
    };
    let undo = f
        .lib
        .undo_cover_edit(action, UI, options)
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(undo.written, [f.id]);
    assert_eq!(undo.failed, [(other_id, "file_read_only")]);
    assert_eq!(f.lib.action(action).unwrap().unwrap().target_count, 2);
    assert_eq!(f.lib.action(action).unwrap().unwrap().undone_by, None);
    assert_eq!(f.lib.cover_edit_tracks(action).unwrap(), [other_id]);
    let remaining = f.lib.describe_undo(action).unwrap();
    assert_eq!(
        remaining.targets,
        [journal::ActionTarget::Track { id: other_id }]
    );
    std::fs::set_permissions(&other, permissions).unwrap();
    let retry = f
        .lib
        .undo_cover_edit(action, UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(retry.written, [other_id]);
    assert_eq!(
        f.lib.action(action).unwrap().unwrap().undone_by,
        retry.action
    );
}

#[test]
fn tag_undo_requires_the_cover_write_to_be_undone_first() {
    let f = Fixture::new();
    let original = std::fs::read(&f.path).unwrap();
    let edit = f
        .lib
        .edit_tags(
            &[f.id],
            &[crate::TagChange::Set {
                key: "ARTIST".into(),
                values: vec!["new artist".into()],
            }],
            UI,
            TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    let action = edit.action.unwrap();
    let cover = f.edit().unwrap().action.unwrap();
    let covered = std::fs::read(&f.path).unwrap();
    let outcome = f
        .lib
        .undo_tag_edit(action, UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait();
    match outcome {
        Err(LibraryError::ActionConflict { .. }) => {}
        Ok(r) => {
            assert!(r.written.is_empty());
            assert_eq!(r.skipped, [(f.id, "file_changed")]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(std::fs::read(&f.path).unwrap(), covered);
    f.lib.undo_action(cover, UI).unwrap();
    f.lib.undo_action(action, UI).unwrap();
    assert_eq!(std::fs::read(&f.path).unwrap(), original);
}
