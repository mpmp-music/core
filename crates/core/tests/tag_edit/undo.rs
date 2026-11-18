//! Undo scenarios.
use super::*;

/// The preview shows what the edit will write; the edit is a job whose events, in order,
/// carry its origin, after which queries see the new tags; one action records it, and its
/// undo (a job too) writes the old values back.
#[test]
fn tag_edits_run_as_jobs_and_are_undone() {
    let f = fixture();
    let core = &f.core;
    let changes = vec![
        set("ARTIST", &["New A", "New B"]),
        TagChange::Remove {
            key: "COMMENT".into(),
        },
    ];
    let QueryResult::TagEditPreview { files, skipped } = core.query(Query::TagEditPreview {
        tracks: f.all(),
        changes: changes.clone(),
    }) else {
        panic!()
    };
    assert!(skipped.is_empty());
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].before["ARTIST"], ["Old"]);
    assert_eq!(files[0].before["COMMENT"], ["comment"]);
    assert_eq!(files[0].after["ARTIST"], ["New A", "New B"]);
    assert!(!files[0].after.contains_key("COMMENT"));
    assert_eq!(files[0].after["TITLE"], files[0].before["TITLE"]);
    // Bad changes name the field, in the query and in the command.
    let QueryResult::Err { error } = core.query(Query::TagEditPreview {
        tracks: f.all(),
        changes: vec![set("TPE1", &["x"])],
    }) else {
        panic!()
    };
    assert_eq!(
        (error.code, error.details.field.as_deref()),
        (ErrorCode::InvalidTag, Some("changes[0].key"))
    );
    let e = err(run(
        core,
        edit(f.all(), vec![set("ARTIST", &[" "])]),
        Origin::Ui,
    ));
    assert_eq!(
        (e.code, e.details.field.as_deref()),
        (ErrorCode::InvalidTag, Some("command.changes[0].values[0]"))
    );
    let e = err(run(
        core,
        edit(vec![TrackId(999)], changes.clone()),
        Origin::Ui,
    ));
    assert_eq!(e.code, ErrorCode::TrackNotFound);
    // Nothing to change: no job.
    let receipt = ok(run(core, edit(f.all(), Vec::new()), Origin::Ui));
    assert!(!receipt.changed && receipt.job.is_none());

    let receipt = ok(run(core, edit(f.all(), changes.clone()), Origin::Ui));
    assert!(receipt.changed && receipt.action.is_none());
    let events = f.job_events(receipt.job.unwrap());
    let order: Vec<_> = names(&events)
        .into_iter()
        .filter(|n| *n != "job_progress")
        .collect();
    assert_eq!(
        order,
        [
            "tracks_changed",
            "actions_changed",
            "job_finished",
            "library_changed"
        ]
    );
    assert!(events.iter().all(|e| e.origin == Some(Origin::Ui)));
    assert!(events.iter().any(|e| matches!(
        e.event,
        Event::JobProgress {
            kind: JobKind::TagEdit,
            total: Some(3),
            ..
        }
    )));
    let (written, action, skipped, failed) = report(&events);
    assert_eq!(written, 3);
    assert!(events.iter().any(|e| matches!(
        &e.event,
        Event::JobFinished {
            outcome:
                JobOutcome::Completed {
                    report:
                        JobReport::TagEdit {
                            written: Some(ids), ..
                        },
                },
            ..
        } if ids.len() == 3,
    )));

    assert!(skipped.is_empty() && failed.is_empty());
    let action = action.unwrap();
    assert_eq!(
        f.tag("ARTIST"),
        vec![vec!["New A".to_owned(), "New B".into()]; 3]
    );
    assert_eq!(f.tag("COMMENT"), vec![Vec::<String>::new(); 3]);
    let summary = f.action(action);
    assert_eq!(summary.kind, ActionKind::TagEdit);
    assert_eq!(summary.origin, Origin::Ui);
    assert_eq!(summary.target_count, 3);
    assert!(summary.undoable);

    // The undo is a job too: its own action is in its report.
    let description = core.describe_undo(action).unwrap();
    assert!(description.writes_files);
    assert_eq!(description.effective_kind, "edit_tags");
    assert_eq!(description.original.0, action.0);
    let QueryResult::TagEditPreview { files, skipped } = core.preview_undo(action).unwrap() else {
        panic!("undo preview")
    };
    assert!(skipped.is_empty());
    assert!(!files.is_empty());
    assert!(
        files
            .iter()
            .all(|f| f.display_location.is_some() && f.before != f.after)
    );
    let mut forged = description.clone();
    forged.writes_files = false;
    let stale = err(core
        .execute_described_undo(
            Request::new(900, Command::UndoAction { action }),
            Origin::Ui,
            forged,
        )
        .result);
    assert_eq!(
        (stale.kind, stale.code),
        (ErrorKind::Conflict, ErrorCode::ActionConflict)
    );
    assert_eq!(stale.message, "undo description changed");
    assert!(f.action(action).undone_by.is_none());
    let receipt = ok(core
        .execute_described_undo(
            Request::new(901, Command::UndoAction { action }),
            Origin::Ui,
            description,
        )
        .result);
    assert!(receipt.action.is_none());
    let events = f.job_events(receipt.job.unwrap());
    let (written, undo, _, _) = report(&events);
    assert_eq!(written, 3);
    assert_eq!(f.tag("ARTIST"), vec![vec!["Old".to_owned()]; 3]);
    assert_eq!(f.tag("COMMENT"), vec![vec!["comment".to_owned()]; 3]);
    let undo = f.action(undo.unwrap());
    assert_eq!((undo.kind, undo.undo_of), (ActionKind::Undo, Some(action)));
    assert_eq!(f.action(action).undone_by, Some(undo.id));
    let redo_description = core.describe_undo(undo.id).unwrap();
    assert!(redo_description.writes_files);
    assert_eq!(redo_description.original.0, action.0);
    assert_eq!(redo_description.chain.len(), 2);
    let e = err(run(core, Command::UndoAction { action }, Origin::Ui));
    assert_eq!(e.code, ErrorCode::AlreadyUndone);

    // A file changed since the edit: the undo writes nothing back.
    let receipt = ok(run(core, edit(f.all(), changes.clone()), Origin::Ui));
    let (_, action, _, _) = report(&f.job_events(receipt.job.unwrap()));
    touch(&f.file("02.flac"));
    let receipt = ok(run(
        core,
        Command::UndoAction {
            action: action.unwrap(),
        },
        Origin::Ui,
    ));
    let events = f.job_events(receipt.job.unwrap());
    let failed = events.iter().find_map(|e| match &e.event {
        Event::JobFinished {
            outcome: JobOutcome::Failed { error },
            ..
        } => Some(error.clone()),
        _ => None,
    });
    let failed = failed.unwrap();
    assert_eq!(
        (failed.kind, failed.code),
        (ErrorKind::Conflict, ErrorCode::ActionConflict)
    );
    assert_eq!(
        failed.details.changed,
        [ObjectRef::Track {
            id: f.id("02.flac")
        }]
    );
    assert!(!names(&events).contains(&"tracks_changed"));
    assert_eq!(f.tag("ARTIST")[0], ["New A", "New B"]);
    // So is a file changed since the last scan, by the edit itself.
    let receipt = ok(run(
        core,
        edit(f.all(), vec![set("ARTIST", &["X"])]),
        Origin::Ui,
    ));
    let (written, _, skipped, _) = report(&f.job_events(receipt.job.unwrap()));
    assert_eq!(written, 2);
    assert_eq!(
        skipped,
        [TagFileResult {
            track: f.id("02.flac"),
            code: ErrorCode::FileChanged
        }]
    );
}

/// The file the player has open (the track being heard) is not written.
#[test]
fn changed_trusted_undo_description_is_an_action_conflict() {
    let f = fixture();
    let receipt = ok(run(
        &f.core,
        Command::SetRating {
            tracks: f.all(),
            rating: Some(4),
        },
        Origin::Ui,
    ));
    let action = receipt.action.unwrap();
    let mut description = f.core.describe_undo(action).unwrap();
    description.effective_kind = "stale_operation".into();
    let error = err(f
        .core
        .execute_described_undo(
            Request::new(902, Command::UndoAction { action }),
            Origin::Ui,
            description,
        )
        .result);
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Conflict, ErrorCode::ActionConflict)
    );
    assert_eq!(error.message, "undo description changed");
    assert!(f.action(action).undone_by.is_none());
    f.core.shutdown();
}
