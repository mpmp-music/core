//! Edits scenarios.
use super::*;

#[test]
fn the_playing_file_is_skipped() {
    let f = fixture();
    let core = &f.core;
    let playing = f.id("01.flac");
    ok(run(
        core,
        Command::Play {
            context: PlayContext::Tracks {
                tracks: vec![playing],
            },
            start: 0,
            start_track: None,
        },
        Origin::Ui,
    ));
    let receipt = ok(run(
        core,
        edit(f.all(), vec![set("ARTIST", &["New"])]),
        Origin::Ui,
    ));
    let (written, _, skipped, _) = report(&f.job_events(receipt.job.unwrap()));
    assert_eq!(written, 2);
    assert_eq!(
        skipped,
        [TagFileResult {
            track: playing,
            code: ErrorCode::FileInUse
        }]
    );
    assert_eq!(f.tag("ARTIST")[0], ["Old"]);
}

/// The codes core lists files with are the contract's, never `unknown`: `rescan_needed` (a folder's encoding changed since the
/// last scan) and `undo_too_large`, in the preview and in the job's report.
#[test]
fn tag_file_codes_decode_as_what_they_are() {
    let f = fixture();
    let core = &f.core;
    let changes = vec![set("ARTIST", &["New"])];
    let codes =
        |skipped: &[TagFileResult]| -> Vec<ErrorCode> { skipped.iter().map(|s| s.code).collect() };
    let rescan = |f: &Fixture| {
        let job = ok(run(&f.core, Command::Rescan, Origin::Ui)).job.unwrap();
        wait_event(
            &f.sub,
            |e| matches!(&e.event, Event::JobFinished { job: j, .. } if *j == job),
        );
    };

    f.library
        .set_root_encoding(mpmp_library::RootId(f.root.0), Some("gbk"))
        .unwrap();
    let QueryResult::TagEditPreview { files, skipped } = core.query(Query::TagEditPreview {
        tracks: f.all(),
        changes: changes.clone(),
    }) else {
        panic!()
    };
    assert!(files.is_empty());
    assert_eq!(codes(&skipped), [ErrorCode::RescanNeeded; 3]);
    let json = serde_json::to_value(&skipped[0]).unwrap();
    assert_eq!(json["code"], "rescan_needed");
    let receipt = ok(run(core, edit(f.all(), changes.clone()), Origin::Ui));
    let (written, _, skipped, _) = report(&f.job_events(receipt.job.unwrap()));
    assert_eq!(written, 0);
    assert_eq!(codes(&skipped), [ErrorCode::RescanNeeded; 3]);
    rescan(&f);

    let path = f.file("01.flac");
    let mut file = lofty::read_from_path(&path).unwrap();
    file.primary_tag_mut()
        .unwrap()
        .set_comment("x".repeat(8 * 1024 * 1024));
    file.save_to_path(&path, WriteOptions::default()).unwrap();
    rescan(&f);
    let big = vec![set("COMMENT", &["new"])];
    let QueryResult::TagEditPreview { skipped, .. } = core.query(Query::TagEditPreview {
        tracks: vec![f.id("01.flac")],
        changes: big.clone(),
    }) else {
        panic!()
    };
    assert_eq!(codes(&skipped), [ErrorCode::UndoTooLarge]);
    let receipt = ok(run(core, edit(vec![f.id("01.flac")], big), Origin::Ui));
    let events = f.job_events(receipt.job.unwrap());
    let (_, _, _, failed) = report(&events);
    assert_eq!(codes(&failed), [ErrorCode::UndoTooLarge]);
    let finished = events
        .iter()
        .find(|e| matches!(e.event, Event::JobFinished { .. }))
        .unwrap();
    let json = serde_json::to_string(finished).unwrap();
    assert!(json.contains(r#""code":"undo_too_large""#), "{json}");
    assert!(!json.contains("unknown"), "{json}");
}

/// A file of an agent's held edit replaced by a symlink while the user is asked: the edit
/// is refused when approved (`location_changed`), and nothing is written.
#[cfg(unix)]
#[test]
fn a_file_moved_while_the_user_is_asked_is_not_written() {
    let f = fixture();
    let core = &f.core;
    let e = err(run(
        core,
        edit(f.all(), vec![set("ARTIST", &["New"])]),
        agent(),
    ));
    let token = e.details.confirmation.unwrap().token;
    let file = f.file("03.flac");
    let elsewhere = f.music.parent().unwrap().join("elsewhere.flac");
    std::fs::rename(&file, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &file).unwrap();
    let bytes = std::fs::read(&elsewhere).unwrap();
    let outcome = ok(run(core, Command::Confirm { token }, Origin::Ui))
        .confirmed
        .unwrap();
    let CommandResult::Err { error } = *outcome.result.unwrap() else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::LocationChanged);
    assert_eq!(std::fs::read(&elsewhere).unwrap(), bytes);
    assert_eq!(f.tag("ARTIST")[0], ["Old"]);
}

#[test]
fn playing_and_preloaded_tag_edits_require_an_explicit_retry() {
    let (entered_tx, entered) = crossbeam_channel::unbounded();
    let (release, gate) = crossbeam_channel::bounded(1);
    let f = fixture_with_resolver(Arc::new(PrefetchGate {
        entered: entered_tx,
        gates: Mutex::new(BTreeMap::from([("02.flac".into(), gate)])),
    }));
    let selected = vec![f.id("01.flac"), f.id("02.flac")];
    ok(run(
        &f.core,
        Command::Play {
            context: PlayContext::Tracks {
                tracks: selected.clone(),
            },
            start: 0,
            start_track: None,
        },
        Origin::Ui,
    ));
    // Wait for an actual prefetch open task, not merely an actor queue candidate.
    assert_eq!(entered.recv_timeout(TIMEOUT).unwrap(), "02.flac");
    let changes = vec![set("ARTIST", &["Retried"])];
    let receipt = ok(run(
        &f.core,
        edit(selected.clone(), changes.clone()),
        Origin::Ui,
    ));
    let (written, _, skipped, failed) = report(&f.job_events(receipt.job.unwrap()));
    assert_eq!(written, 0);
    assert!(failed.is_empty());
    assert_eq!(
        skipped
            .iter()
            .map(|r| (r.track, r.code))
            .collect::<Vec<_>>(),
        selected
            .iter()
            .map(|t| (*t, ErrorCode::FileInUse))
            .collect::<Vec<_>>()
    );
    release.send(()).unwrap();
    ok(run(&f.core, Command::Stop, Origin::Ui));
    // A scan is a deterministic post-playback barrier; no deferred write occurs.
    let scan = ok(run(&f.core, Command::Rescan, Origin::Ui)).job.unwrap();
    wait_event(
        &f.sub,
        |e| matches!(e.event, Event::JobFinished {job,..} if job==scan),
    );
    assert_eq!(
        &f.tag("ARTIST")[..2],
        &[vec!["Old".to_owned()], vec!["Old".to_owned()]]
    );
    let receipt = ok(run(&f.core, edit(selected, changes), Origin::Ui));
    let (written, _, skipped, failed) = report(&f.job_events(receipt.job.unwrap()));
    assert_eq!(written, 2);
    assert!(skipped.is_empty() && failed.is_empty());
}

#[test]
fn stopped_selection_does_not_hold_tag_files_but_paused_decoders_do() {
    let f = fixture();
    let selected = vec![f.id("01.flac"), f.id("02.flac")];
    let edit_job = ok(run(
        &f.core,
        edit(selected.clone(), vec![set("ARTIST", &["Edited"])]),
        Origin::Ui,
    ))
    .job
    .unwrap();
    let (_, action, _, _) = report(&f.job_events(edit_job));
    let action = action.unwrap();
    ok(run(
        &f.core,
        Command::Play {
            context: PlayContext::Tracks {
                tracks: selected.clone(),
            },
            start: 0,
            start_track: None,
        },
        Origin::Ui,
    ));
    ok(run(&f.core, Command::Pause, Origin::Ui));
    let job = ok(run(
        &f.core,
        edit(vec![selected[0]], vec![set("GENRE", &["Busy"])]),
        Origin::Ui,
    ))
    .job
    .unwrap();
    let (written, _, skipped, failed) = report(&f.job_events(job));
    assert_eq!(written, 0);
    assert_eq!(skipped[0].code, ErrorCode::FileInUse);
    assert!(failed.is_empty());
    let (preview, occupancy) = f.core.preview_undo_with_occupancy(action).unwrap();
    assert_eq!(occupancy, Some(mpmp_core::FileUndoOccupancy::Playback));
    let QueryResult::TagEditPreview { skipped, .. } = preview else {
        panic!("expected a tag undo preview");
    };
    assert!(
        skipped
            .iter()
            .any(|row| row.track == selected[0] && row.code == ErrorCode::FileInUse)
    );
    let job = ok(run(&f.core, Command::UndoAction { action }, Origin::Ui))
        .job
        .unwrap();
    let events = f.job_events(job);
    let error = events
        .iter()
        .find_map(|envelope| match &envelope.event {
            Event::JobFinished {
                outcome: JobOutcome::Failed { error },
                ..
            } => Some(error),
            _ => None,
        })
        .expect("undo failed while playback held a file");
    assert_eq!(error.code, ErrorCode::FileInUse);
    assert_eq!(error.message, "an undo target is in use by playback");

    assert_eq!(
        &f.tag("ARTIST")[..2],
        &[vec!["Edited".to_owned()], vec!["Edited".to_owned()]]
    );
    ok(run(&f.core, Command::Stop, Origin::Ui));
    // Stopped navigation retains a current Playing record without opening the engine.
    ok(run(&f.core, Command::Next, Origin::Ui));
    let QueryResult::Snapshot { snapshot } = f.core.query(Query::Snapshot) else {
        panic!()
    };
    assert_eq!(snapshot.state, PlayState::Stopped);
    let job = ok(run(&f.core, Command::UndoAction { action }, Origin::Ui))
        .job
        .unwrap();
    let (written, _, skipped, failed) = report(&f.job_events(job));
    assert_eq!(written, 2);
    assert!(skipped.is_empty() && failed.is_empty());
    ok(run(
        &f.core,
        Command::Clear {
            expected_revision: None,
        },
        Origin::Ui,
    ));
    let job = ok(run(
        &f.core,
        edit(selected, vec![set("GENRE", &["Free"])]),
        Origin::Ui,
    ))
    .job
    .unwrap();
    let (written, _, skipped, failed) = report(&f.job_events(job));
    assert_eq!(written, 2);
    assert!(skipped.is_empty() && failed.is_empty());
}

#[test]
fn tag_action_details_only_list_keys_changed_in_written_files_without_values() {
    let f = fixture();
    let ticket = ok(run(
        &f.core,
        edit(
            f.all(),
            vec![
                set("ALBUM", &["Album"]),
                set("ARTIST", &["Private new artist"]),
                TagChange::Remove {
                    key: "COMMENT".into(),
                },
                TagChange::Remove {
                    key: "COMPOSER".into(),
                },
            ],
        ),
        Origin::Ui,
    ));
    let events = f.job_events(ticket.job.unwrap());
    let (_, id, _, _) = report(&events);
    let id = id.unwrap();
    let facts = f.action(id).details.unwrap();
    assert_eq!(
        facts,
        ActionDetails::TagEdit {
            changes: vec![
                TagKeyChange::Set {
                    key: "ARTIST".into()
                },
                TagKeyChange::Remove {
                    key: "COMMENT".into()
                },
            ]
        }
    );
    assert!(!serde_json::to_string(&facts).unwrap().contains("Private"));
    let later = ok(run(
        &f.core,
        edit(f.all(), vec![set("ARTIST", &["Another artist"])]),
        Origin::Ui,
    ));
    f.job_events(later.job.unwrap());
    assert_eq!(f.action(id).details, Some(facts));
}
