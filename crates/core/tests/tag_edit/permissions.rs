//! Permissions scenarios.
use super::*;
use mpmp_core::catalog::Browse;

/// An agent's tag edit (class `file_write`, its targets the tracks' files) needs the
/// user's confirmation by default, which shows the files' folder and the change; a grant
/// for the folder lets it run without asking; a forbid of a part of it refuses it, also
/// one given while the user is asked. Undoing an agent's tag edit is a file write too.
#[test]
fn agents_ask_before_rewriting_files() {
    let f = fixture();
    let core = &f.core;
    let ui = |command| ok(run(core, command, Origin::Ui));
    let changes = vec![set("ARTIST", &["New"])];
    // The preview is a query: anyone may ask.
    assert!(matches!(
        core.query(Query::TagEditPreview {
            tracks: f.all(),
            changes: changes.clone(),
        }),
        QueryResult::TagEditPreview { .. }
    ));
    let e = err(run(core, edit(f.all(), changes.clone()), agent()));
    assert_eq!(e.kind, ErrorKind::NeedsConfirmation);
    let token = e.details.confirmation.unwrap().token;
    let asked = wait_event(&f.sub, |e| {
        matches!(e.event, Event::ConfirmationRequested { .. })
    });
    let Event::ConfirmationRequested { confirmation } = asked.event else {
        panic!()
    };
    assert_eq!(
        confirmation.operation,
        Some(OperationInfo {
            class: OperationClass::FileWrite,
            command: "edit_tags".into(),
        })
    );
    assert_eq!(
        confirmation.scope,
        Some(Scope::Library {
            root: f.root,
            prefix: Some(ALBUM.into()),
        })
    );
    assert_eq!(
        confirmation.display_location.as_deref(),
        Some(f.music.join("Artist").join("Album").to_str().unwrap())
    );
    assert_eq!(
        confirmation.summary,
        "rewrite the tags of 3 files: set ARTIST to \"New\" (9 bytes of keys and values per file)"
    );
    // Approved: core runs it as the agent; the job writes the files it checked.
    let receipt = ui(Command::Confirm { token });
    let result = receipt.confirmed.unwrap().result.unwrap();
    let CommandResult::Ok(held) = *result else {
        panic!("{result:?}")
    };
    let events = f.job_events(held.job.unwrap());
    let of_the_job = events
        .iter()
        .filter(|e| !matches!(e.event, Event::ConfirmationResolved { .. }));
    assert!(of_the_job.clone().count() >= 4);
    assert!(of_the_job.into_iter().all(|e| e.origin == Some(agent())));
    let (written, action, _, _) = report(&events);
    assert_eq!(written, 3);
    let action = action.unwrap();
    assert_eq!(f.action(action).origin, agent());

    // Undoing it is a file write as well.
    let e = err(run(core, Command::UndoAction { action }, agent()));
    assert_eq!(e.kind, ErrorKind::NeedsConfirmation);
    let asked = wait_event(&f.sub, |e| {
        matches!(e.event, Event::ConfirmationRequested { .. })
    });
    let Event::ConfirmationRequested { confirmation } = asked.event else {
        panic!()
    };
    assert_eq!(
        confirmation.operation.unwrap().class,
        OperationClass::FileWrite
    );
    assert_eq!(
        confirmation.summary,
        format!(
            "undo action {}, writing the old tags back into 3 files",
            action.0
        )
    );
    ui(Command::Reject {
        token: e.details.confirmation.unwrap().token,
    });

    // A grant for the library folder: no asking; the action names the grant.
    let allow = ui(grant(
        Operation::Class {
            class: OperationClass::FileWrite,
        },
        Scope::Library {
            root: f.root,
            prefix: None,
        },
        Effect::Allow,
    ))
    .grant
    .unwrap();
    let receipt = ok(run(
        core,
        edit(f.all(), vec![set("ARTIST", &["Granted"])]),
        agent(),
    ));
    let (written, action, _, _) = report(&f.job_events(receipt.job.unwrap()));
    assert_eq!(written, 3);
    assert_eq!(f.action(action.unwrap()).grant, Some(allow));
    // … also for undoing it (grants for `edit_tags` apply to its undo).
    let receipt = ok(run(
        core,
        Command::UndoAction {
            action: action.unwrap(),
        },
        agent(),
    ));
    assert!(receipt.job.is_some());
    f.job_events(receipt.job.unwrap());
    assert_eq!(f.tag("ARTIST")[0], ["New"]);

    // A forbid of a part of it refuses the whole edit.
    let forbid = ui(grant(
        Operation::Command {
            command: "edit_tags".into(),
        },
        Scope::Library {
            root: f.root,
            prefix: Some(ALBUM.into()),
        },
        Effect::Forbid,
    ))
    .grant
    .unwrap();
    let e = err(run(
        core,
        edit(vec![f.id("02.flac")], changes.clone()),
        agent(),
    ));
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::Forbidden, ErrorCode::PermissionDenied)
    );
    assert_eq!(e.details.forbidden_by[0].grant, forbid);

    // A forbid given while the user is asked refuses it when approved.
    ui(Command::RevokePermission { grant: forbid });
    ui(Command::RevokePermission { grant: allow });
    let e = err(run(core, edit(f.all(), changes.clone()), agent()));
    let token = e.details.confirmation.unwrap().token;
    ui(grant(
        Operation::Class {
            class: OperationClass::FileWrite,
        },
        Scope::Library {
            root: f.root,
            prefix: Some(ALBUM.into()),
        },
        Effect::Forbid,
    ));
    let outcome = ui(Command::Confirm { token }).confirmed.unwrap();
    assert_eq!(outcome.reason, Some(ConfirmationReason::Approved));
    let CommandResult::Err { error } = *outcome.result.unwrap() else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    assert_eq!(f.tag("ARTIST")[0], ["New"]);
}

/// An agent's tag edit is checked for what it names (how many tracks, the changes) before
/// its files are looked for.
#[test]
fn an_agents_tag_edit_is_checked_before_its_files_are_looked_for() {
    let f = fixture();
    let many: Vec<TrackId> = (1..=1001).map(|i| TrackId(1_000_000 + i)).collect();
    let e = err(run(
        &f.core,
        edit(many, vec![set("ARTIST", &["x"])]),
        agent(),
    ));
    assert_eq!(
        (e.code, e.details.field.as_deref()),
        (ErrorCode::OutOfRange, Some("command.tracks"))
    );
    let e = err(run(
        &f.core,
        edit(vec![TrackId(999)], vec![set("TPE1", &["x"])]),
        agent(),
    ));
    assert_eq!(e.code, ErrorCode::InvalidTag, "{e:?}");
}

/// An agent's tag edit of files that are not where their paths in the music folder lead:
/// one a symlink leads out of the folder is shown, and
/// scoped, where it really is, so that allowing that scope lets the edit run; with files
/// in the folder no single scope holds them. A file that cannot be read now: the edit of
/// it alone, and an undo writing it back, cannot run (`invalid_location`, nobody asked);
/// with others it can, the confirmation says it is skipped, and the job skips it.
#[cfg(unix)]
#[test]
fn agents_are_shown_files_where_they_really_are() {
    let f = fixture();
    let core = &f.core;
    let ui = |command| ok(run(core, command, Origin::Ui));
    let changes = vec![set("ARTIST", &["New"])];
    let (one, two, three) = (f.id("01.flac"), f.id("02.flac"), f.id("03.flac"));
    let receipt = ui(edit(vec![one, two], vec![set("ARTIST", &["UI"])]));
    let (_, ui_edit, _, _) = report(&f.job_events(receipt.job.unwrap()));

    // 03.flac: a link out of the music folder (same size and time: no rescan needed).
    let outside = f.music.parent().unwrap().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let master = outside.join("master.flac");
    std::fs::rename(f.file("03.flac"), &master).unwrap();
    std::os::unix::fs::symlink(&master, f.file("03.flac")).unwrap();
    let real = std::fs::canonicalize(&master)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let QueryResult::TagEditPreview { files, skipped } = core.query(Query::TagEditPreview {
        tracks: vec![three],
        changes: changes.clone(),
    }) else {
        panic!("tag preview")
    };
    assert!(skipped.is_empty());
    assert_eq!(files[0].display_location.as_deref(), Some(real.as_str()));
    let e = err(run(core, edit(vec![three], changes.clone()), agent()));
    assert_eq!(e.kind, ErrorKind::NeedsConfirmation);
    let shown = asked(&f);
    assert_eq!(
        shown.scope,
        Some(Scope::Location {
            location: real.clone()
        })
    );
    assert_eq!(shown.display_location.as_deref(), Some(real.as_str()));
    assert!(
        shown.summary.ends_with(&format!(
            "; the file is not in the music folder but where a link from it leads: {real}"
        )),
        "{}",
        shown.summary
    );
    ui(Command::Reject { token: shown.token });
    // A grant for the scope offered covers the command.
    let allow = ui(grant(
        Operation::Class {
            class: OperationClass::FileWrite,
        },
        shown.scope.unwrap(),
        Effect::Allow,
    ))
    .grant
    .unwrap();
    let receipt = ok(run(core, edit(vec![three], changes.clone()), agent()));
    let (written, _, _, _) = report(&f.job_events(receipt.job.unwrap()));
    assert_eq!(written, 1);
    ui(Command::RevokePermission { grant: allow });
    // With a file in the folder: no single scope.
    let e = err(run(core, edit(vec![one, three], changes.clone()), agent()));
    assert_eq!(e.kind, ErrorKind::NeedsConfirmation);
    let shown = asked(&f);
    assert_eq!((shown.scope, shown.display_location), (None, None));
    assert!(
        shown.summary.contains(&format!(
            "; 1 of the files is not in the music folder but where a link from it leads: {real}"
        )),
        "{}",
        shown.summary
    );
    ui(Command::Reject { token: shown.token });

    // 02.flac: a link to nothing (a file on a drive that is not there reads the same).
    let gone = outside.join("gone.flac");
    std::fs::rename(f.file("02.flac"), &gone).unwrap();
    std::os::unix::fs::symlink(&gone, f.file("02.flac")).unwrap();
    std::fs::remove_file(&gone).unwrap();
    let e = err(run(core, edit(vec![two], changes.clone()), agent()));
    assert_eq!(
        (e.code, e.details.field.as_deref()),
        (ErrorCode::InvalidLocation, Some("command.tracks")),
        "{e:?}"
    );
    assert!(e.message.contains("Artist/Album/02.flac"), "{e:?}");
    let e = err(run(
        core,
        Command::UndoAction {
            action: ui_edit.unwrap(),
        },
        agent(),
    ));
    assert_eq!(
        (e.code, e.details.field.as_deref()),
        (ErrorCode::InvalidLocation, Some("command.action")),
        "{e:?}"
    );
    let e = err(run(core, edit(vec![one, two], changes.clone()), agent()));
    assert_eq!(e.kind, ErrorKind::NeedsConfirmation, "{e:?}");
    let shown = asked(&f);
    assert_eq!(shown.scope, None);
    assert!(
        shown.summary.contains(
            "; 1 of the files is not there now (gone, or on a drive that is not plugged in) \
             and skipped"
        ),
        "{}",
        shown.summary
    );
    assert!(
        shown
            .summary
            .contains(&format!("#{} (file_missing)", two.0))
    );
    let outcome = ui(Command::Confirm { token: shown.token })
        .confirmed
        .unwrap();
    let CommandResult::Ok(held) = *outcome.result.unwrap() else {
        panic!()
    };
    let (written, _, skipped, _) = report(&f.job_events(held.job.unwrap()));
    assert_eq!(written, 1);
    assert_eq!(
        skipped,
        [TagFileResult {
            track: two,
            code: ErrorCode::FileMissing
        }]
    );
}

#[test]
fn encoding_conversion_is_shown_in_preview_confirmation_and_job_report() {
    let f = fixture();
    let id = add_mp3(&f, true);
    let changes = vec![set("GENRE", &["Pop"])];
    let QueryResult::TagEditPreview { files, skipped } = f.core.query(Query::TagEditPreview {
        tracks: vec![id],
        changes: changes.clone(),
    }) else {
        panic!()
    };
    assert!(skipped.is_empty());
    assert_eq!(files[0].encoding_conversion, Some(true));
    let error = err(run(&f.core, edit(vec![id], changes.clone()), agent()));
    assert_eq!(error.kind, ErrorKind::NeedsConfirmation);
    let token = error.details.confirmation.unwrap().token;
    let asked = wait_event(&f.sub, |e| {
        matches!(e.event, Event::ConfirmationRequested { .. })
    });
    let Event::ConfirmationRequested { confirmation } = asked.event else {
        panic!()
    };
    assert!(
        confirmation
            .summary
            .contains("1 file will have legacy text converted to standard Unicode encoding"),
        "{}",
        confirmation.summary
    );
    ok(run(&f.core, Command::Reject { token }, Origin::Ui));
    ok(run(
        &f.core,
        grant(
            Operation::Class {
                class: OperationClass::FileWrite,
            },
            Scope::All,
            Effect::Allow,
        ),
        Origin::Ui,
    ));
    let receipt = ok(run(&f.core, edit(vec![id], changes), agent()));
    let events = f.job_events(receipt.job.unwrap());
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.event, Event::JobFinished {
        outcome: JobOutcome::Completed {
         report: JobReport::TagEdit {
         encoding_converted: Some(ids),
         .. }
         }
        ,
         ..
    } if ids == &[id]))
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.event, Event::ConfirmationRequested { .. }))
    );
}

#[test]
fn agent_is_not_asked_to_confirm_an_edit_whose_preview_skips_every_file() {
    let f = fixture();
    use lofty::file::TaggedFileExt;
    use lofty::tag::Accessor;
    let path = f.file("01.flac");
    let mut audio = lofty::probe::Probe::open(&path).unwrap().read().unwrap();
    audio
        .primary_tag_mut()
        .unwrap()
        .set_comment("x".repeat(8 * 1024 * 1024));
    audio
        .save_to_path(&path, lofty::config::WriteOptions::new())
        .unwrap();
    let scan = ok(run(&f.core, Command::Rescan, Origin::Ui)).job.unwrap();
    f.job_events(scan);
    let id = f.id("01.flac");
    let changes = vec![set("COMMENT", &["Small"])];
    let QueryResult::TagEditPreview { files, skipped } = f.core.query(Query::TagEditPreview {
        tracks: vec![id],
        changes: changes.clone(),
    }) else {
        panic!()
    };
    assert!(files.is_empty());
    assert_eq!(skipped[0].code, ErrorCode::UndoTooLarge);
    let error = err(run(&f.core, edit(vec![id], changes), agent()));
    assert_eq!(error.code, ErrorCode::UndoTooLarge);
    assert_eq!(error.kind, ErrorKind::Invalid);
    assert!(error.details.confirmation.is_none());
    assert_eq!(error.details.field.as_deref(), Some("command.tracks"));
    assert!(
        !f.sub
            .events
            .try_iter()
            .any(|e| matches!(e.event, Event::ConfirmationRequested { .. }))
    );
}

#[test]
fn agent_all_file_changed_preview_skips_return_a_consistent_error_without_hanging() {
    let f = fixture();
    for name in f.tracks.keys() {
        touch(&f.file(name));
    }
    assert_agent_all_skipped_finishes(&f, f.all(), ErrorCode::FileChanged);
    f.core.shutdown();
}

#[test]
fn agent_all_cue_track_preview_skips_return_a_consistent_error_without_hanging() {
    let f = fixture();
    std::fs::write(
        f.file("album.cue"),
        concat!(
            "PERFORMER \"Artist\"\nTITLE \"Album\"\nFILE \"01.flac\" WAVE\nTRACK 01 ",
            "AUDIO\nTITLE \"Cue song\"\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 ",
            "00:01:00\n",
        ),
    )
    .unwrap();
    let job = ok(run(&f.core, Command::Rescan, Origin::Ui)).job.unwrap();
    f.job_events(job);
    let QueryResult::Tracks { tracks, .. } = f.core.query(Query::Tracks {
        query: TrackQuery::default(),
        offset: 0,
        limit: 100,
        tags: Vec::new(),
    }) else {
        panic!("tracks")
    };
    let cue_tracks: Vec<_> = tracks
        .into_iter()
        .filter(|t| t.cue.is_some())
        .map(|t| t.id)
        .collect();
    assert!(
        !cue_tracks.is_empty(),
        "the real scan must import the CUE track"
    );
    assert_agent_all_skipped_finishes(&f, cue_tracks, ErrorCode::CueTrack);
    f.core.shutdown();
}

#[test]
fn agent_root_access_requests_are_audited_with_grants_and_survive_reopen() {
    let f = fixture();
    let allow = ok(run(
        &f.core,
        grant(
            Operation::Class {
                class: OperationClass::LocalPath,
            },
            Scope::All,
            Effect::Allow,
        ),
        Origin::Ui,
    ))
    .grant
    .unwrap();
    let mut audit_ids = Vec::new();
    for (command, kind) in [
        (
            Command::ReleaseRoot { root: f.root },
            ActionKind::RootRelease,
        ),
        (
            Command::ReleaseRoot { root: f.root },
            ActionKind::RootRelease,
        ),
        (Command::ResumeRoot { root: f.root }, ActionKind::RootResume),
    ] {
        let receipt = ok(run(&f.core, command, agent()));
        assert!(receipt.action.is_none());
        let id = receipt.audit.unwrap();
        let audit = f.action(id);
        let QueryResult::Roots { roots } = f.core.query(Query::Roots) else {
            panic!()
        };
        assert_eq!(
            audit.details,
            Some(ActionDetails::Root {
                root: f.root,
                path: roots[0].path.clone()
            })
        );
        assert_eq!(
            (audit.kind, audit.origin, audit.grant, audit.undoable),
            (kind, agent(), Some(allow), false)
        );
        assert_eq!(
            err(run(&f.core, Command::UndoAction { action: id }, Origin::Ui)).code,
            ErrorCode::ActionNotFound
        );
        audit_ids.push(id);
    }
    let ui = ok(run(
        &f.core,
        Command::ReleaseRoot { root: f.root },
        Origin::Ui,
    ));
    assert!(ui.audit.is_none());
    assert!(f.core.shutdown_and_wait(Duration::from_secs(2)));
    let reopened = mpmp_library::Library::open(&f._dir.path().join("library.db")).unwrap();
    let QueryResult::Actions { actions, .. } = reopened.query(&Query::Actions {
        origin: None,
        offset: 0,
        limit: 100,
    }) else {
        panic!()
    };
    for id in audit_ids {
        assert!(actions.iter().any(|action| action.id == id
            && !action.undoable
            && matches!(&action.details,
                 Some(ActionDetails::Root {
                 root,
                 .. }
                )
                if *root == f.root)));
    }
}

/// The official SDK mock and the real core read this corpus. Refresh only from real
/// FLAC/ID3 files, not from the mock's implementation.
#[test]
fn sdk_file_workflows_match_mock_corpus() {
    use serde_json::{Value, json};
    let path = client_parity::core_crate_path(env!("CARGO_MANIFEST_DIR"), "mock-file-cases.json");
    let mut corpus: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let update = std::env::var_os("UPDATE_MOCK_FILE_CASES").is_some();
    let f = fixture();
    let id = |n: i64| {
        if n > 3 {
            TrackId(n)
        } else {
            f.id(&format!("{n:02}.flac"))
        }
    };
    let logical = |track: TrackId| {
        f.all()
            .iter()
            .position(|t| *t == track)
            .map_or(track.0, |i| i as i64 + 1)
    };
    let preview_value = |result: QueryResult| match result {
        QueryResult::TagEditPreview { files, skipped } => json!({
            "files":files.into_iter().map(|mut file| {
            file.track=TrackId(logical(file.track));
            file.display_location=None;
            file}
            ).collect::<Vec<_>>(),

            "skipped":skipped.into_iter().map(|mut file|{
            file.track=TrackId(logical(file.track));
            file}
            ).collect::<Vec<_>>()
        }),
        QueryResult::Err { error } => json!({"error":{"kind":error.kind,"code":error.code}}),
        other => panic!("{other:?}"),
    };
    let check = |slot: &mut Value, actual: Value| {
        if update {
            *slot = actual;
        } else {
            assert_eq!(actual, *slot);
        }
    };
    check(
        &mut corpus["seed"],
        preview_value(f.core.query(Query::TagEditPreview {
            tracks: f.all(),
            changes: vec![],
        })),
    );
    for case in corpus["preview_cases"].as_array_mut().unwrap() {
        let ids = case["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| id(n.as_i64().unwrap()))
            .collect();
        let changes = serde_json::from_value(case["changes"].clone()).unwrap();
        let actual = preview_value(f.core.query(Query::TagEditPreview {
            tracks: ids,
            changes,
        }));
        check(&mut case["expected"], actual);
    }
    let cases = [
        ("empty tracks", vec![], vec![set("TITLE", &["x"])]),
        (
            "over 1000 tracks",
            vec![1; 1001],
            vec![set("TITLE", &["x"])],
        ),
        (
            "unknown track preview",
            vec![999999],
            vec![set("TITLE", &["x"])],
        ),
        (
            "invalid changes preview",
            vec![1],
            vec![set("APIC", &["x"])],
        ),
    ];
    let mut command_cases = Vec::new();
    for (name, logical_ids, changes) in cases {
        let core_error = if logical_ids.is_empty() || logical_ids.len() > 1000 {
            Value::Null
        } else {
            let QueryResult::Err { error } = f.core.query(Query::TagEditPreview {
                tracks: logical_ids.iter().map(|n| id(*n)).collect(),
                changes: changes.clone(),
            }) else {
                panic!("expected preflight error")
            };
            serde_json::to_value(error).unwrap()
        };
        let expected = if core_error.is_null() {
            json!({
            "source":"host",
            "kind":null,
            "code":"invalid",
            "reason":"before_submission",
            "outcome":"not_sent",
            "retryable":false}
            )
        } else {
            json!({
            "source":"core",
            "kind":core_error["kind"],
            "code":core_error["code"],
            "reason":"core_precondition",
            "outcome":"not_sent",
            "retryable":false}
            )
        };
        let repeat = (logical_ids.len() > 1000).then_some(logical_ids.len());
        command_cases.push(json!({
        "name":name,
        "command":{
        "type":"edit_tags",
        "tracks":
        if repeat.is_some() {
         vec![1] }
         else {
         logical_ids }
        ,
        "changes":changes}
        ,
        "repeat":repeat,
        "core_error":core_error,
        "expected":expected}
        ));
    }
    check(&mut corpus["command_preconditions"], json!(command_cases));
    let noop_receipt = ok(run(&f.core, edit(vec![id(1)], vec![]), Origin::Ui));
    check(
        &mut corpus["noop"],
        json!({
        "changed":noop_receipt.changed,
        "job":noop_receipt.job.is_some(),
        "action":noop_receipt.action.is_some(),
        "written":[]}
        ),
    );
    let edited = ok(run(
        &f.core,
        edit(vec![id(1), id(2)], vec![set("TITLE", &["Song 1"])]),
        Origin::Ui,
    ));
    let events = f.job_events(edited.job.unwrap());
    let (written, action, skipped, failed) = report(&events);
    assert_eq!(written, 1);
    assert!(skipped.is_empty() && failed.is_empty());
    let action = f.action(action.unwrap());
    check(&mut corpus["minor_2"]["tag_details"], json!(action.details));
    let QueryResult::Job { job } = f.core.query(Query::Job {
        job: edited.job.unwrap(),
    }) else {
        panic!()
    };
    check(
        &mut corpus["minor_2"]["tag_job"],
        json!({
        "kind":job.kind,
        "state":job.state,
        "outcome":job.outcome.as_ref().map(|o|matches!(o,
        JobOutcome::Completed {
         .. }
        ))}
        ),
    );
    let terminal = events.iter().find_map(|e| match &e.event {
        Event::JobFinished { outcome, .. } => Some(outcome.clone()),
        _ => None,
    });
    assert_eq!(job.outcome, terminal);
    let QueryResult::Err { error } = f.core.query(Query::Job { job: JobId(0) }) else {
        panic!()
    };
    check(
        &mut corpus["minor_2"]["missing_job"],
        json!({"kind":error.kind,"code":error.code}),
    );
    let noop = json!({
    "written_count":written,
    "kind":action.kind,
    "target_count":action.target_count,
    "undoable":action.undoable}
    );

    let changed = ok(run(
        &f.core,
        edit(vec![id(2)], vec![set("DATE", &["2025-03-01"])]),
        Origin::Ui,
    ));
    let (_, date_action, _, _) = report(&f.job_events(changed.job.unwrap()));
    let date_action = date_action.unwrap();
    let QueryResult::TrackRows { tracks: rows } = f.core.query(Query::TrackRows {
        tracks: vec![id(2)],
        tags: vec!["DATE".into()],
    }) else {
        panic!()
    };
    let date = json!({"year":rows[0].year,"tags":rows[0].tags});
    let QueryResult::TrackRows { tracks: rows } = f.core.query(Query::TrackRows {
        tracks: vec![id(2)],
        tags: vec![],
    }) else {
        panic!()
    };
    let no_tags = json!(rows[0].tags);
    ok(run(
        &f.core,
        Command::Play {
            context: PlayContext::Tracks {
                tracks: vec![id(2)],
            },
            start: 0,
            start_track: None,
        },
        Origin::Ui,
    ));
    // Preview excludes the decoder's open file, matching execution admission.
    let open_preview = preview_value(f.core.query(Query::TagEditPreview {
        tracks: vec![id(2)],
        changes: vec![],
    }));
    let undone = ok(run(
        &f.core,
        Command::UndoAction {
            action: date_action,
        },
        Origin::Ui,
    ));
    let finished = wait_event(
        &f.sub,
        |e| matches!(e.event,Event::JobFinished{job,..} if Some(job)==undone.job),
    );
    let Event::JobFinished {
        outcome: JobOutcome::Failed { error },
        ..
    } = finished.event
    else {
        panic!("{finished:?}")
    };
    let QueryResult::Job { job: failed_job } = f.core.query(Query::Job {
        job: undone.job.unwrap(),
    }) else {
        panic!()
    };
    check(
        &mut corpus["minor_2"]["failed_job"],
        json!({"kind":failed_job.kind,"state":failed_job.state}),
    );
    assert_eq!(
        failed_job.outcome,
        Some(JobOutcome::Failed {
            error: error.clone()
        })
    );
    let busy = json!({"kind":error.kind,"code":error.code,"retryable":error.details.retryable});
    ok(run(&f.core, Command::Stop, Origin::Ui));
    let undone = ok(run(
        &f.core,
        Command::UndoAction {
            action: date_action,
        },
        Origin::Ui,
    ));
    let (undo_written, undo_action, _, _) = report(&f.job_events(undone.job.unwrap()));
    let undo_summary = f.action(undo_action.unwrap());
    check(
        &mut corpus["minor_2"]["undo_details"],
        json!(undo_summary.details),
    );
    let undo = json!({
    "written_count":undo_written,
    "kind":undo_summary.kind,
    "target_count":undo_summary.target_count,
    "undo_of":undo_summary.undo_of.is_some()}
    );

    let QueryResult::Actions {
        actions,
        total,
        next_offset,
        ..
    } = f.core.query(Query::Actions {
        origin: Some(OriginFilter::Agent { name: None }),
        offset: 0,
        limit: 1,
    })
    else {
        panic!()
    };
    let filtered = json!({"length":actions.len(),"total":total,"next_offset":next_offset});
    ok(run(
        &f.core,
        Command::ReleaseRoot { root: f.root },
        Origin::Ui,
    ));
    let released = preview_value(f.core.query(Query::TagEditPreview {
        tracks: vec![id(1)],
        changes: vec![],
    }));
    let removed = ok(run(
        &f.core,
        Command::RemoveRoot { root: f.root },
        Origin::Ui,
    ));
    let QueryResult::Roots { roots } = f.core.query(Query::Roots) else {
        panic!()
    };
    let repeated = ok(run(
        &f.core,
        Command::RemoveRoot { root: f.root },
        Origin::Ui,
    ));
    check(
        &mut corpus["remove_repeat"],
        json!({"changed":repeated.changed,"action":repeated.action}),
    );
    let removed_count = roots.len();
    let details = f.action(removed.action.unwrap()).details.unwrap();
    let ActionDetails::Root {
        root,
        path: root_path,
    } = details
    else {
        panic!()
    };
    check(
        &mut corpus["minor_2"]["root_details"],
        json!({"root_matches":root==f.root,"path_matches":root_path==f.music.to_string_lossy()}),
    );
    let restore = ok(run(
        &f.core,
        Command::UndoAction {
            action: removed.action.unwrap(),
        },
        Origin::Ui,
    ));
    f.job_events(restore.job.unwrap());
    let QueryResult::Roots { roots } = f.core.query(Query::Roots) else {
        panic!()
    };
    let restored = json!({"length":roots.len(),"released":roots[0].released});
    let scan = ok(run(&f.core, Command::Rescan, Origin::Ui));
    let event = wait_event(
        &f.sub,
        |e| matches!(e.event,Event::JobFinished{job,..} if Some(job)==scan.job),
    );
    let Event::JobFinished {
        outcome: JobOutcome::Completed {
            report: JobReport::Scan { .. },
        },
        ..
    } = event.event
    else {
        panic!("{event:?}")
    };
    check(
        &mut corpus["workflow"],
        json!({
        "noop":noop,
        "date":date,
        "no_tags":no_tags,
        "open_preview":open_preview,
        "busy":busy,
        "undo":undo,
        "filtered":filtered,
        "released":released,
        "removed_count":removed_count,
        "restored":restored,
        "rescan":"completed"}
        ),
    );
    // This immutable corpus retains its historical metadata. Read its title rather
    // than duplicating a real-world song name in the synthetic test fixtures.
    let frozen_title = corpus["legacy"]["files"][0]["before"]["TITLE"][0]
        .as_str()
        .unwrap();
    let mp3 = add_mp3_with_title(&f, true, frozen_title);
    let mut legacy = preview_value(f.core.query(Query::TagEditPreview {
        tracks: vec![mp3],
        changes: vec![set("GENRE", &["Pop"])],
    }));
    legacy["files"][0]["track"] = json!(4);
    check(&mut corpus["legacy"], legacy);
    let second = f.music.parent().unwrap().join("other");
    std::fs::create_dir_all(&second).unwrap();
    let added = ok(run(
        &f.core,
        Command::AddRoot {
            path: second.display().to_string(),
        },
        Origin::Ui,
    ));
    f.job_events(added.job.unwrap());
    let other_root = added.root.unwrap();
    for root in [f.root, other_root] {
        ok(run(&f.core, Command::ReleaseRoot { root }, Origin::Ui));
    }
    let source = f.music.join(ALBUM).join("01.flac");
    std::fs::copy(&source, f.music.join("new.flac")).unwrap();
    std::fs::copy(&source, second.join("new.flac")).unwrap();
    let mut scoped = vec![];
    for root in [f.root, other_root] {
        let resumed = ok(run(&f.core, Command::ResumeRoot { root }, Origin::Ui));
        let events = f.job_events(resumed.job.unwrap());
        let count = events
            .iter()
            .find_map(|e| match &e.event {
                Event::JobFinished {
                    outcome:
                        JobOutcome::Completed {
                            report: JobReport::Scan { added_count, .. },
                        },
                    ..
                } => Some(*added_count),
                _ => None,
            })
            .unwrap();
        scoped.push(count);
    }
    check(&mut corpus["scoped_scan"], json!(scoped));
    ok(run(
        &f.core,
        Command::SetRating {
            tracks: vec![id(1)],
            rating: Some(4),
        },
        Origin::Ui,
    ));
    let cleared = ok(run(
        &f.core,
        Command::SetRating {
            tracks: vec![id(1)],
            rating: None,
        },
        Origin::Ui,
    ));
    check(
        &mut corpus["minor_2"]["cleared_rating_details"],
        json!(f.action(cleared.action.unwrap()).details),
    );
    if update {
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&corpus).unwrap()),
        )
        .unwrap();
    }
    f.core.shutdown();
}

#[test]
fn a_mixed_confirmation_names_the_tracks_and_their_undo_refusals() {
    use lofty::file::TaggedFileExt;
    use lofty::tag::Accessor;
    let f = fixture();
    let path = f.file("01.flac");
    let mut audio = lofty::probe::Probe::open(&path).unwrap().read().unwrap();
    audio
        .primary_tag_mut()
        .unwrap()
        .set_comment("x".repeat(8 * 1024 * 1024));
    audio
        .save_to_path(&path, lofty::config::WriteOptions::new())
        .unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../library/tests/tag_edit/data/tone-faststart.m4a");
    let mut m4a = std::fs::read(source).unwrap();
    m4a.extend(b"\0\0\0\x08moof");
    std::fs::write(f.file("fragment.m4a"), m4a).unwrap();
    let scan = ok(run(&f.core, Command::Rescan, Origin::Ui)).job.unwrap();
    f.job_events(scan);
    let QueryResult::Tracks { tracks, .. } = f.core.query(Query::Tracks {
        query: Default::default(),
        tags: vec![],
        offset: 0,
        limit: 20,
    }) else {
        panic!("tracks query");
    };
    let fragmented = tracks
        .iter()
        .find(|track| track.path.ends_with("fragment.m4a"))
        .unwrap()
        .id;
    let large = f.id("01.flac");
    let tracks = vec![large, f.id("02.flac"), fragmented];
    let changes = vec![set("COMMENT", &["Changed"])];
    let e = err(run(&f.core, edit(tracks, changes), agent()));
    assert_eq!(e.kind, ErrorKind::NeedsConfirmation, "{e:?}");
    let shown = wait_event(&f.sub, |e| {
        matches!(e.event, Event::ConfirmationRequested { .. })
    });
    let Event::ConfirmationRequested {
        confirmation: shown,
    } = shown.event
    else {
        panic!()
    };
    assert!(
        shown
            .summary
            .contains(&format!("#{} (undo_too_large)", large.0)),
        "{shown:?}"
    );
    assert!(
        shown
            .summary
            .contains(&format!("#{} (not_undoable)", fragmented.0)),
        "{shown:?}"
    );
}
