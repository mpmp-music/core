//! Covers scenarios.
use super::*;

#[test]
fn release_main_decode_read_is_off_actor_deadline_covers_join_and_retry() {
    use std::sync::atomic::Ordering::SeqCst;
    let (f, gate, open) = main_read_fixture();
    let start = Instant::now();
    let (released, thread) = begin_release(&f);
    ok(run(
        &f.core,
        Command::SetVolume { volume: 0.41 },
        Origin::Ui,
    ));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(!gate.closed.load(SeqCst));
    assert!(released.try_recv().is_err());
    let error = err(released.recv_timeout(Duration::from_secs(6)).unwrap());
    assert_eq!(
        (error.kind, error.code, error.details.retryable),
        (ErrorKind::Failed, ErrorCode::Io, true)
    );
    assert!(start.elapsed() < Duration::from_secs(7));
    thread.join().unwrap();
    let (retry, thread) = begin_release(&f);
    ok(run(&f.core, Command::SetMuted { muted: true }, Origin::Ui));
    assert!(retry.recv_timeout(Duration::from_millis(50)).is_err());
    open.send(()).unwrap();
    ok(retry.recv_timeout(TIMEOUT).unwrap());
    thread.join().unwrap();
    assert!(gate.closed.load(SeqCst));
    f.core.shutdown();
}

#[test]
fn committed_directory_undo_errors_require_remove_root_recovery_not_undo_retry() {
    let f = fixture();
    let QueryResult::Actions { actions, .. } = f.core.query(Query::Actions {
        origin: None,
        offset: 0,
        limit: 100,
    }) else {
        panic!()
    };
    let add = actions
        .iter()
        .find(|a| a.kind == ActionKind::RootAdd)
        .unwrap()
        .id;
    ok(run(
        &f.core,
        Command::ReleaseRoot { root: f.root },
        Origin::Ui,
    ));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let _registration = f
        .core
        .register_root_access_handler(Arc::new(move |_, released| {
            if !released && seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Err(ContractError::failed(
                    ErrorCode::Io,
                    "injected undo recovery failure",
                ))
            } else {
                Ok(())
            }
        }));
    let error = err(run(
        &f.core,
        Command::UndoAction { action: add },
        Origin::Ui,
    ));
    assert_eq!(error.code, ErrorCode::Io);
    assert!(!error.details.retryable);
    assert_eq!(
        error.details.recovery_command.as_deref(),
        Some("remove_root")
    );
    assert!(!f.library.root_released(mpmp_library::RootId(f.root.0)));
    assert!(matches!(f.core.query(Query::Roots), QueryResult::Roots { roots } if roots.is_empty()));
    assert_eq!(
        err(run(
            &f.core,
            Command::UndoAction { action: add },
            Origin::Ui
        ))
        .code,
        ErrorCode::AlreadyUndone
    );
    let retry = ok(run(
        &f.core,
        Command::RemoveRoot { root: f.root },
        Origin::Ui,
    ));
    assert!(!retry.changed);
    assert!(retry.action.is_none());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    f.core.shutdown();
}

#[test]
fn cover_jobs_publish_authority_events_revisions_and_reuse_undo_grants() {
    let f = fixture();
    let core = &f.core;
    let Command::EditCover { tracks, change } = cover_set(f.all()) else {
        unreachable!()
    };
    assert!(matches!(
        core.query(Query::CoverEditPreview { tracks, change }),
        QueryResult::CoverEditPreview { files, .. } if files.len() == 3,
    ));
    let e = err(run(core, cover_set(f.all()), agent()));
    assert_eq!(e.code, ErrorCode::NeedsConfirmation);
    let token = e.details.confirmation.unwrap().token;
    let asked = wait_event(&f.sub, |e| {
        matches!(e.event, Event::ConfirmationRequested { .. })
    });
    let Event::ConfirmationRequested { confirmation } = asked.event else {
        unreachable!()
    };
    assert!(confirmation.summary.contains("SHA-256"));
    assert!(confirmation.summary.contains("front"));
    assert!(!confirmation.summary.contains("iVBOR"));
    let confirmed = ok(run(core, Command::Confirm { token }, Origin::Ui))
        .confirmed
        .unwrap();
    let CommandResult::Ok(receipt) = *confirmed.result.unwrap() else {
        panic!()
    };
    let events = f.job_events(receipt.job.unwrap());
    let (written, action, skipped, failed) = cover_report(&events);
    assert_eq!(written.len(), 3);
    assert!(skipped.is_empty() && failed.is_empty());
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.event,Event::CoversChanged{tracks} if tracks.len()==3))
    );
    assert!(f.action(action.unwrap()).details.is_some());
    let grant = ok(run(
        core,
        grant(
            Operation::Command {
                command: "edit_cover".into(),
            },
            Scope::Library {
                root: f.root,
                prefix: None,
            },
            Effect::Allow,
        ),
        Origin::Ui,
    ))
    .grant
    .unwrap();
    let receipt = ok(run(
        core,
        Command::UndoAction {
            action: action.unwrap(),
        },
        agent(),
    ));
    let (_, undo, _, failed) = cover_report(&f.job_events(receipt.job.unwrap()));
    assert!(failed.is_empty());
    assert_eq!(f.action(undo.unwrap()).grant, Some(grant));
}

#[test]
fn cover_confirmation_rechecks_sources_and_a_forbid_refuses_the_entire_batch() {
    let f = fixture();
    let core = &f.core;
    let before: Vec<_> = ["01.flac", "02.flac", "03.flac"]
        .iter()
        .map(|name| (*name, std::fs::read(f.file(name)).unwrap()))
        .collect();
    let e = err(run(core, cover_set(f.all()), agent()));
    let token = e.details.confirmation.unwrap().token;
    let forbid = ok(run(
        core,
        grant(
            Operation::Command {
                command: "edit_cover".into(),
            },
            Scope::Library {
                root: f.root,
                prefix: Some(format!("{ALBUM}/02.flac")),
            },
            Effect::Forbid,
        ),
        Origin::Ui,
    ))
    .grant
    .unwrap();
    let r = ok(run(core, Command::Confirm { token }, Origin::Ui))
        .confirmed
        .unwrap();
    let CommandResult::Err { error } = *r.result.unwrap() else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    for (name, bytes) in before {
        assert_eq!(std::fs::read(f.file(name)).unwrap(), bytes);
    }
    ok(run(
        core,
        Command::RevokePermission { grant: forbid },
        Origin::Ui,
    ));
    let e = err(run(core, cover_set(vec![f.id("01.flac")]), agent()));
    let token = e.details.confirmation.unwrap().token;
    let path = f.file("01.flac");
    let mut b = std::fs::read(&path).unwrap();
    *b.last_mut().unwrap() ^= 1;
    std::fs::write(&path, &b).unwrap();
    let r = ok(run(core, Command::Confirm { token }, Origin::Ui))
        .confirmed
        .unwrap();
    let CommandResult::Err { error } = *r.result.unwrap() else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::FileChanged);
    assert_eq!(std::fs::read(&path).unwrap(), b);
}

#[test]
fn cover_paused_playback_and_prefetch_use_the_tag_file_lease_checks() {
    let (entered_tx, entered) = crossbeam_channel::unbounded();
    let (release_prefetch, gate) = crossbeam_channel::bounded(1);
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
    assert_eq!(entered.recv_timeout(TIMEOUT).unwrap(), "02.flac");
    ok(run(&f.core, Command::Pause, Origin::Ui));
    let job = ok(run(&f.core, cover_set(selected.clone()), Origin::Ui))
        .job
        .unwrap();
    let (written, _, skipped, failed) = cover_report(&f.job_events(job));
    assert!(written.is_empty() && failed.is_empty());
    assert_eq!(skipped[0].code, ErrorCode::FileInUse);
    release_prefetch.send(()).unwrap();
    ok(run(&f.core, Command::Stop, Origin::Ui));
    assert!(
        f.library
            .wait_root_idle(mpmp_library::RootId(f.root.0), TIMEOUT)
            .unwrap()
    );
    let job = ok(run(&f.core, cover_set(selected), Origin::Ui))
        .job
        .unwrap();
    let (written, _, skipped, failed) = cover_report(&f.job_events(job));
    assert_eq!(written.len(), 2);
    assert!(skipped.is_empty() && failed.is_empty());
}

#[test]
#[cfg(unix)]
fn cover_confirmation_refuses_replaced_links_and_same_content_new_files() {
    for link in [true, false] {
        let f = fixture();
        let path = f.file("01.flac");
        let bytes = std::fs::read(&path).unwrap();
        let e = err(run(&f.core, cover_set(vec![f.id("01.flac")]), agent()));
        let token = e.details.confirmation.unwrap().token;
        let replacement = f.music.join("replacement.flac");
        std::fs::copy(&path, &replacement).unwrap();
        if link {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&replacement, &path).unwrap();
        } else {
            std::fs::rename(&replacement, &path).unwrap();
        }
        let r = ok(run(&f.core, Command::Confirm { token }, Origin::Ui))
            .confirmed
            .unwrap();
        let CommandResult::Err { error } = *r.result.unwrap() else {
            panic!()
        };
        assert_eq!(
            error.code,
            if link {
                ErrorCode::LocationChanged
            } else {
                ErrorCode::FileChanged
            }
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn ui_cover_recovery_resolution_confirms_binds_current_file_and_audits_acceptance() {
    const INSERT_FILE_WRITE_INTENTS: &str = r#"
INSERT INTO file_write_intents(family,format_version,authorization_json,target_json,before_digest,before_image,after_digest,stage,created_at,updated_at)
VALUES('cover_edit',1,?1,?2,?3,?4,?5,'prepared',1,1)
"#;

    let f = fixture();
    let track = f.id("01.flac");
    let conn = rusqlite::Connection::open(f._dir.path().join("library.db")).unwrap();
    let agent_sub = f
        .core
        .subscribe(agent(), SubscribeOptions::default())
        .unwrap();
    let remote_sub = f
        .core
        .subscribe(
            Origin::Remote {
                device: "privacy-test".into(),
            },
            SubscribeOptions::default(),
        )
        .unwrap();
    let candidate = f.music.join(".cover-backup-ui-secret");
    std::fs::write(&candidate, b"retained complete candidate").unwrap();
    let candidate_identity =
        mpmp_fs_desktop::paths::FileIdentity::of_file(&std::fs::File::open(&candidate).unwrap())
            .unwrap()
            .parts();
    let target = serde_json::json!({
    "track":track.0,
    "path":std::fs::canonicalize(f.file("01.flac")).unwrap(),
    "group":"manual-test-conflict",
    "operation":"set",
    "undo_of":null,
    "at":1,
    "backup":{
    "path":candidate,
    "identity":candidate_identity}
    }
    );

    conn.execute(
        INSERT_FILE_WRITE_INTENTS,
        rusqlite::params![
            r#"{"origin":"ui","grant":null,"checked":false}"#,
            target.to_string(),
            "a".repeat(64),
            Vec::<u8>::new(),
            "b".repeat(64)
        ],
    )
    .unwrap();
    let command = || Command::ResolveCoverRecovery {
        track,
        resolution: CoverRecoveryResolution::AcceptCurrent,
        selection: None,
    };
    for origin in [
        agent(),
        Origin::Remote { device: "r".into() },
        Origin::System,
    ] {
        assert_eq!(
            err(run(&f.core, command(), origin)).code,
            ErrorCode::OriginNotAllowed
        );
    }
    let ticket = err(run(&f.core, command(), Origin::Ui))
        .details
        .confirmation
        .unwrap();
    assert!(ticket.summary.contains("accept") || ticket.summary.contains("Accept"));
    assert!(!ticket.summary.contains("base64"));
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(f.file("01.flac"))
        .unwrap()
        .write_all(b"external change")
        .unwrap();
    let outcome = ok(run(
        &f.core,
        Command::Confirm {
            token: ticket.token,
        },
        Origin::Ui,
    ))
    .confirmed
    .unwrap();
    assert!(matches!(
        *outcome.result.unwrap(),
        CommandResult::Err { error } if error.code == ErrorCode::CoverRecoveryChanged,
    ));
    let ticket = err(run(&f.core, command(), Origin::Ui))
        .details
        .confirmation
        .unwrap();
    let outcome = ok(run(
        &f.core,
        Command::Confirm {
            token: ticket.token,
        },
        Origin::Ui,
    ))
    .confirmed
    .unwrap();
    let CommandResult::Ok(receipt) = *outcome.result.unwrap() else {
        panic!()
    };
    let ui_result = f
        .sub
        .events
        .try_iter()
        .find_map(|e| {
            if let Event::CoverRecoveryResolved { summary, .. } = e.event {
                Some(summary)
            } else {
                None
            }
        })
        .expect("UI recovery result");
    assert!(ui_result.contains("backup:"));
    assert!(ui_result.contains(candidate.to_str().unwrap()));
    for sub in [&agent_sub, &remote_sub] {
        for e in sub.events.try_iter() {
            assert!(!matches!(
                e.event,
                Event::CoverRecoveryResolved { .. } | Event::ConfirmationRequested { .. }
            ));
            assert!(
                !serde_json::to_string(&e)
                    .unwrap()
                    .contains("cover-backup-ui-secret")
            );
        }
    }
    let audit = f.action(receipt.audit.unwrap());
    assert_eq!(audit.kind, ActionKind::CoverRecoveryResolve);
    assert!(!audit.undoable);
    assert_eq!(
        conn.query_row("SELECT count(*) FROM file_write_intents", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let cleanup = Command::ResolveCoverRecovery {
        track,
        resolution: CoverRecoveryResolution::CleanupCandidates,
        selection: None,
    };
    for origin in [
        agent(),
        Origin::Remote { device: "r".into() },
        Origin::System,
    ] {
        assert_eq!(
            err(run(&f.core, cleanup.clone(), origin)).code,
            ErrorCode::OriginNotAllowed
        );
    }
    let ticket = err(run(&f.core, cleanup, Origin::Ui))
        .details
        .confirmation
        .unwrap();
    let outcome = ok(run(
        &f.core,
        Command::Confirm {
            token: ticket.token,
        },
        Origin::Ui,
    ))
    .confirmed
    .unwrap();
    assert!(matches!(*outcome.result.unwrap(), CommandResult::Ok(_)));
    assert!(!candidate.exists());
    assert_eq!(
        err(run(&f.core, command(), Origin::Ui)).code,
        ErrorCode::CoverRecoveryNotFound
    );
}

#[test]
fn selected_cover_recovery_inventory_and_result_are_private_to_ui() {
    const INSERT_FILE_WRITE_INTENTS: &str = r#"
INSERT INTO file_write_intents(family,format_version,authorization_json,target_json,before_digest,before_image,after_digest,stage,created_at,updated_at)
VALUES('cover_edit',1,?1,?2,?3,?4,?5,'prepared',1,1)
"#;

    use sha2::{Digest, Sha256};
    let f = fixture();
    let track = f.id("01.flac");
    let path = std::fs::canonicalize(f.file("01.flac")).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let candidate = path.with_file_name(".cover-backup-selected-private");
    std::fs::copy(&path, &candidate).unwrap();
    let identity =
        mpmp_fs_desktop::paths::FileIdentity::of_file(&std::fs::File::open(&candidate).unwrap())
            .unwrap()
            .parts();
    let staged = path.with_file_name(".cover-stage-selected-private");
    std::fs::copy(f.file("02.flac"), &staged).unwrap();
    let staged_digest = format!("{:x}", Sha256::digest(std::fs::read(&staged).unwrap()));
    let staged_identity =
        mpmp_fs_desktop::paths::FileIdentity::of_file(&std::fs::File::open(&staged).unwrap())
            .unwrap()
            .parts();
    let conn = rusqlite::Connection::open(f._dir.path().join("library.db")).unwrap();
    let target = serde_json::json!({
    "track":track.0,
    "path":path,
    "group":"selected-private",
    "operation":"set",
    "undo_of":null,
    "at":1,
    "backup":{
    "path":candidate,
    "identity":identity}
    ,
    "staged":{
    "path":staged,
    "identity":staged_identity}
    }
    );

    conn.execute(
        INSERT_FILE_WRITE_INTENTS,
        rusqlite::params![
            r#"{"origin":"ui","grant":null,"checked":false}"#,
            target.to_string(),
            digest,
            Vec::<u8>::new(),
            staged_digest
        ],
    )
    .unwrap();
    let intent = conn.last_insert_rowid();
    std::fs::remove_file(&path).unwrap();
    let agent_sub = f
        .core
        .subscribe(agent(), SubscribeOptions::default())
        .unwrap();
    let remote_sub = f
        .core
        .subscribe(
            Origin::Remote {
                device: "recovery-privacy".into(),
            },
            SubscribeOptions::default(),
        )
        .unwrap();
    let command = |role, sha256| Command::ResolveCoverRecovery {
        track,
        resolution: CoverRecoveryResolution::RestoreBefore,
        selection: Some(CoverRecoverySelection {
            intent,
            role,
            sha256,
        }),
    };
    assert_eq!(
        err(run(
            &f.core,
            command(CoverCandidateRole::Unknown, digest.clone()),
            Origin::Ui
        ))
        .code,
        ErrorCode::UnknownVariant
    );
    assert_eq!(
        err(run(
            &f.core,
            command(CoverCandidateRole::Backup, "0123".into()),
            Origin::Ui
        ))
        .code,
        ErrorCode::OutOfRange
    );
    let ticket = err(run(
        &f.core,
        command(CoverCandidateRole::Backup, digest.clone()),
        Origin::Ui,
    ))
    .details
    .confirmation
    .unwrap();
    assert!(ticket.summary.contains(&digest));
    assert!(ticket.summary.contains(candidate.to_str().unwrap()));
    assert!(
        ticket
            .summary
            .contains(&format!("SELECTED intent {intent}"))
    );
    let outcome = ok(run(
        &f.core,
        Command::Confirm {
            token: ticket.token,
        },
        Origin::Ui,
    ))
    .confirmed
    .unwrap();
    let CommandResult::Ok(receipt) = *outcome.result.unwrap() else {
        panic!("restore failed")
    };
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let result = f
        .sub
        .events
        .try_iter()
        .find_map(|e| match e.event {
            Event::CoverRecoveryResolved { summary, .. } => Some(summary),
            _ => None,
        })
        .unwrap();
    assert!(result.contains(staged.to_str().unwrap()));
    assert!(result.contains(&staged_digest));
    for sub in [&agent_sub, &remote_sub] {
        for event in sub.events.try_iter() {
            assert!(!matches!(
                event.event,
                Event::CoverRecoveryResolved { .. } | Event::ConfirmationRequested { .. }
            ));
            assert!(
                !serde_json::to_string(&event)
                    .unwrap()
                    .contains("selected-private")
            );
        }
    }
    assert!(
        !serde_json::to_string(&f.action(receipt.audit.unwrap()))
            .unwrap()
            .contains("selected-private")
    );
}

#[test]
fn playing_and_paused_files_are_excluded_from_cover_and_tag_previews() {
    let f = fixture();
    let id = f.id("01.flac");
    ok(run(
        &f.core,
        Command::Play {
            context: PlayContext::Tracks { tracks: vec![id] },
            start: 0,
            start_track: None,
        },
        Origin::Ui,
    ));
    for paused in [false, true] {
        if paused {
            ok(run(&f.core, Command::Pause, Origin::Ui));
        }
        let QueryResult::CoverEditPreview { files, skipped } =
            f.core.query(Query::CoverEditPreview {
                tracks: vec![id],
                change: CoverChange::Remove,
            })
        else {
            panic!()
        };
        assert!(files.is_empty());
        assert_eq!(skipped[0].code, ErrorCode::FileInUse);
        let QueryResult::TagEditPreview { files, skipped } = f.core.query(Query::TagEditPreview {
            tracks: vec![id],
            changes: vec![set("TITLE", &["New"])],
        }) else {
            panic!()
        };
        assert!(files.is_empty());
        assert_eq!(skipped[0].code, ErrorCode::FileInUse);
    }
}
