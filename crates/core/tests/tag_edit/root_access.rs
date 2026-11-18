//! Root access scenarios.
use super::*;
use mpmp_core::catalog::MediaResources;

#[test]
fn release_root_stops_playback_preserves_queue_and_requires_explicit_resume() {
    let f = fixture();
    ok(run(
        &f.core,
        Command::Play {
            context: PlayContext::Tracks { tracks: f.all() },
            start: 0,
            start_track: None,
        },
        Origin::Ui,
    ));
    let before = f.core.query(Query::QueuePage {
        offset: 0,
        limit: 100,
    });
    let receipt = ok(run(
        &f.core,
        Command::ReleaseRoot { root: f.root },
        Origin::Ui,
    ));
    assert!(receipt.changed);
    assert_eq!(receipt.root, Some(f.root));
    assert!(receipt.job.is_none() && receipt.action.is_none());
    assert_eq!(
        f.core.query(Query::QueuePage {
            offset: 0,
            limit: 100
        }),
        before
    );
    let QueryResult::Snapshot { snapshot } = f.core.query(Query::Snapshot) else {
        panic!()
    };
    assert_eq!(snapshot.state, PlayState::Stopped);
    let QueryResult::Roots { roots } = f.core.query(Query::Roots) else {
        panic!()
    };
    assert!(roots[0].released);
    assert_eq!(roots[0].track_count, 3);
    assert!(
        !ok(run(
            &f.core,
            Command::ReleaseRoot { root: f.root },
            Origin::Ui
        ))
        .changed
    );
    let released = f
        .library
        .file_lease(mpmp_library::TrackId(f.id("01.flac").0))
        .err()
        .unwrap();
    assert_eq!(released.code(), "root_released");
    assert_eq!(
        f.library
            .cover_art(mpmp_library::TrackId(f.id("01.flac").0))
            .unwrap_err()
            .code(),
        "root_released"
    );
    let QueryResult::Err { error } = f.core.query(Query::Lyrics {
        track: f.id("01.flac"),
    }) else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::RootReleased);
    let job = ok(run(
        &f.core,
        edit(f.all(), vec![set("ARTIST", &["Skip released"])]),
        Origin::Ui,
    ))
    .job
    .unwrap();
    let (written, _, skipped, _) = report(&f.job_events(job));
    assert_eq!(written, 0);
    assert_eq!(skipped.len(), 3);
    assert!(skipped.iter().all(|s| s.code == ErrorCode::RootReleased));
    // Scans cannot read replacements while the root is released.
    let path = f.music.join(ALBUM).join("01.flac");
    std::fs::remove_file(path).unwrap();
    let job = ok(run(&f.core, Command::Rescan, Origin::Ui)).job.unwrap();
    wait_event(
        &f.sub,
        |e| matches!(e.event,Event::JobFinished {job:j,..} if j==job),
    );
    assert!(
        f.library
            .track(mpmp_library::TrackId(f.id("01.flac").0))
            .unwrap()
            .unwrap()
            .available
    );
    let resumed = ok(run(
        &f.core,
        Command::ResumeRoot { root: f.root },
        Origin::Ui,
    ));
    assert!(resumed.changed);
    let job = resumed.job.unwrap();
    wait_event(
        &f.sub,
        |e| matches!(e.event,Event::JobFinished {job:j,..} if j==job),
    );
    assert!(
        !f.library
            .track(mpmp_library::TrackId(f.id("01.flac").0))
            .unwrap()
            .unwrap()
            .available
    );
    let QueryResult::Snapshot { snapshot } = f.core.query(Query::Snapshot) else {
        panic!()
    };
    assert_eq!(snapshot.state, PlayState::Stopped);
    assert!(
        !ok(run(
            &f.core,
            Command::ResumeRoot { root: f.root },
            Origin::Ui
        ))
        .changed
    );
    assert_eq!(
        err(run(
            &f.core,
            Command::ReleaseRoot { root: RootId(999) },
            Origin::Ui
        ))
        .code,
        ErrorCode::RootNotFound
    );
}

#[test]
fn release_root_preloaded_from_another_folder_stops_both_and_unrelated_playback_continues() {
    let corpus: serde_json::Value = serde_json::from_str(client_parity::STATE).unwrap();
    for case in corpus["root_release"].as_array().unwrap() {
        let f = fixture();
        let other = f._dir.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::copy(
            f.music.join(ALBUM).join("01.flac"),
            other.join("other.flac"),
        )
        .unwrap();
        let added = ok(run(
            &f.core,
            Command::AddRoot {
                path: other.display().to_string(),
            },
            Origin::Ui,
        ));
        wait_event(
            &f.sub,
            |e| matches!(e.event,Event::JobFinished {job,..} if Some(job)==added.job),
        );
        let other_id = f
            .library
            .track_ids(
                &mpmp_library::TrackQuery {
                    filter: mpmp_library::Filter::Compare {
                        field: mpmp_library::Field::Root,
                        op: mpmp_library::CompareOp::Eq,
                        value: mpmp_library::Value::Int(added.root.unwrap().0),
                    },
                    sort: vec![],
                },
                100,
            )
            .unwrap()[0];
        let tracks = case["play"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| {
                if id == 1 {
                    f.id("01.flac")
                } else {
                    TrackId(other_id.0)
                }
            })
            .collect();
        ok(run(
            &f.core,
            Command::Play {
                context: PlayContext::Tracks { tracks },
                start: 0,
                start_track: None,
            },
            Origin::Ui,
        ));
        ok(run(
            &f.core,
            Command::ReleaseRoot { root: f.root },
            Origin::Ui,
        ));
        let QueryResult::Snapshot { snapshot } = f.core.query(Query::Snapshot) else {
            panic!()
        };
        assert_eq!(
            snapshot.state,
            if case["state"] == "stopped" {
                PlayState::Stopped
            } else {
                PlayState::Playing
            }
        );
    }
}

#[test]
fn release_root_permissions_follow_local_path_rules() {
    let f = fixture();
    for command in [
        Command::ReleaseRoot { root: f.root },
        Command::ResumeRoot { root: f.root },
    ] {
        if matches!(command, Command::ResumeRoot { .. }) {
            ok(run(
                &f.core,
                Command::ReleaseRoot { root: f.root },
                Origin::Ui,
            ));
        }
        assert_eq!(
            err(run(&f.core, command.clone(), Origin::agent("fixture"))).kind,
            ErrorKind::NeedsConfirmation
        );
        assert_eq!(
            err(run(
                &f.core,
                command.clone(),
                Origin::Remote {
                    device: "remote".into()
                }
            ))
            .code,
            ErrorCode::PathNotAllowed
        );
        assert_eq!(
            err(run(&f.core, command, Origin::MediaControls)).code,
            ErrorCode::OriginNotAllowed
        );
    }
    let QueryResult::Roots { roots } = f.core.query(Query::Roots) else {
        panic!()
    };
    assert!(roots[0].released);
}

#[test]
fn failed_agent_root_release_still_audits_the_suspension_request() {
    let f = fixture();
    ok(run(
        &f.core,
        grant(
            Operation::Class {
                class: OperationClass::LocalPath,
            },
            Scope::All,
            Effect::Allow,
        ),
        Origin::Ui,
    ));
    let _registration = f.core.register_root_access_handler(Arc::new(|_, released| {
        if released {
            Err(ContractError::failed(
                ErrorCode::Io,
                "injected close failure",
            ))
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        err(run(&f.core, Command::ReleaseRoot { root: f.root }, agent())).code,
        ErrorCode::Io
    );
    let QueryResult::Actions { actions, .. } = f.core.query(Query::Actions {
        origin: None,
        offset: 0,
        limit: 100,
    }) else {
        panic!()
    };
    assert_eq!(
        actions
            .iter()
            .filter(|action| action.kind == ActionKind::RootRelease
                && action.origin == agent()
                && !action.undoable)
            .count(),
        1
    );
    assert!(f.library.root_released(mpmp_library::RootId(f.root.0)));
    assert_eq!(
        err(run(
            &f.core,
            Command::ResumeRoot {
                root: RootId(i64::MAX)
            },
            agent()
        ))
        .code,
        ErrorCode::RootNotFound
    );
    let QueryResult::Actions { actions: after, .. } = f.core.query(Query::Actions {
        origin: None,
        offset: 0,
        limit: 100,
    }) else {
        panic!()
    };
    assert_eq!(after.len(), actions.len());
}

#[test]
fn root_release_requires_platform_acknowledgement_and_retries_failed_close() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let f = fixture();
    let failing = Arc::new(AtomicBool::new(true));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let flag = Arc::clone(&failing);
    let seen = Arc::clone(&observed);
    let registration = f
        .core
        .register_root_access_handler(Arc::new(move |path, released| {
            seen.lock().unwrap().push((path.to_owned(), released));
            if released && flag.load(Ordering::Relaxed) {
                Err(ContractError::failed(ErrorCode::Io, "watch handle still open").retryable())
            } else {
                Ok(())
            }
        }));
    assert_eq!(
        err(run(
            &f.core,
            Command::ReleaseRoot { root: f.root },
            Origin::Ui
        ))
        .code,
        ErrorCode::Io
    );
    let QueryResult::Roots { roots } = f.core.query(Query::Roots) else {
        panic!()
    };
    assert!(roots[0].released, "failure must not reopen file access");
    failing.store(false, Ordering::Relaxed);
    assert!(
        !ok(run(
            &f.core,
            Command::ReleaseRoot { root: f.root },
            Origin::Ui
        ))
        .changed
    );
    assert!(
        ok(run(
            &f.core,
            Command::ResumeRoot { root: f.root },
            Origin::Ui
        ))
        .changed
    );
    assert_eq!(
        observed
            .lock()
            .unwrap()
            .iter()
            .map(|(_, r)| *r)
            .collect::<Vec<_>>(),
        [true, true, false]
    );
    drop(registration);
    ok(run(
        &f.core,
        Command::ReleaseRoot { root: f.root },
        Origin::Ui,
    ));
    assert_eq!(observed.lock().unwrap().len(), 3);
}

#[test]
fn release_waits_for_replaced_and_canceled_prefetch_open_tasks() {
    let (entered_tx, entered) = crossbeam_channel::unbounded();
    let (open_b, b) = crossbeam_channel::bounded(1);
    let (open_c, c) = crossbeam_channel::bounded(1);
    let resolver = Arc::new(PrefetchGate {
        entered: entered_tx,
        gates: Mutex::new(BTreeMap::from([
            ("02.flac".into(), b),
            ("03.flac".into(), c),
        ])),
    });
    let f = fixture_with_resolver(resolver);
    let played = ok(run(
        &f.core,
        Command::Play {
            context: PlayContext::Tracks { tracks: f.all() },
            start: 0,
            start_track: None,
        },
        Origin::Ui,
    ));
    assert_eq!(entered.recv_timeout(TIMEOUT).unwrap(), "02.flac");
    let removed = ok(run(
        &f.core,
        Command::Remove {
            entries: vec![played.created_entries[1]],
            expected_revision: Some(played.queue_revision),
        },
        Origin::Ui,
    ));
    assert_eq!(entered.recv_timeout(TIMEOUT).unwrap(), "03.flac");
    ok(run(
        &f.core,
        Command::Remove {
            entries: vec![played.created_entries[2]],
            expected_revision: Some(removed.queue_revision),
        },
        Origin::Ui,
    ));
    // Both tasks have left pending_next; stopping the main decode thread cannot join them.
    let (released, thread) = begin_release(&f);
    assert!(released.recv_timeout(Duration::from_millis(100)).is_err());
    open_b.send(()).unwrap();
    assert!(
        released.recv_timeout(Duration::from_millis(100)).is_err(),
        "second canceled task still owns its lease"
    );
    open_c.send(()).unwrap();
    assert!(ok(released.recv_timeout(TIMEOUT).unwrap()).changed);
    thread.join().unwrap();
    assert!(matches!(
        f.core.query(Query::Snapshot),
        QueryResult::Snapshot { snapshot } if snapshot.state == PlayState::Stopped,
    ));
}

#[test]
fn pending_release_processes_commands_and_events_times_out_and_can_retry() {
    let f = fixture();
    let lease = f
        .library
        .file_lease(mpmp_library::TrackId(f.id("01.flac").0))
        .unwrap();
    let (released, thread) = begin_release(&f);
    ok(run(
        &f.core,
        Command::SetVolume { volume: 0.37 },
        Origin::Ui,
    ));
    wait_event(
        &f.sub,
        |e| matches!(&e.event, Event::StateChanged { snapshot } if snapshot.audio.volume == 0.37),
    );
    assert!(released.try_recv().is_err());
    assert_eq!(
        err(run(
            &f.core,
            Command::ResumeRoot { root: f.root },
            Origin::Ui
        ))
        .code,
        ErrorCode::JobBusy
    );
    assert_eq!(
        err(run(
            &f.core,
            Command::ReleaseRoot { root: f.root },
            Origin::Ui
        ))
        .code,
        ErrorCode::JobBusy
    );
    let corpus: serde_json::Value = serde_json::from_str(client_parity::FILE).unwrap();
    for command in [
        Command::ReleaseRoot { root: f.root },
        Command::ResumeRoot { root: f.root },
        Command::RemoveRoot { root: f.root },
    ] {
        let error = err(run(&f.core, command, Origin::Ui));
        assert_eq!(
            serde_json::json!({
            "kind":error.kind,
            "code":error.code,
            "retryable":error.details.retryable}
            ),
            corpus["root_errors"]["busy"]
        );
    }
    let error = err(released.recv_timeout(Duration::from_secs(7)).unwrap());
    assert_eq!(
        serde_json::json!({
        "kind":error.kind,
        "code":error.code,
        "retryable":error.details.retryable}
        ),
        corpus["root_errors"]["timeout"]
    );
    assert_eq!(error.code, ErrorCode::Io);
    assert!(error.details.retryable);
    assert!(f.library.root_released(mpmp_library::RootId(f.root.0)));
    assert!(
        f.library
            .file_lease(mpmp_library::TrackId(f.id("01.flac").0))
            .is_err()
    );
    thread.join().unwrap();
    drop(lease);
    assert!(
        !ok(run(
            &f.core,
            Command::ReleaseRoot { root: f.root },
            Origin::Ui
        ))
        .changed
    );
}

#[test]
fn shutdown_during_release_does_not_wait_for_held_lease_or_report_success() {
    let f = fixture();
    let lease = f
        .library
        .file_lease(mpmp_library::TrackId(f.id("01.flac").0))
        .unwrap();
    let (released, thread) = begin_release(&f);
    assert!(
        f.core.shutdown_and_wait(Duration::from_secs(2)),
        "shutdown must not join root drain workers"
    );
    assert_eq!(
        err(released.recv_timeout(Duration::from_secs(1)).unwrap()).code,
        ErrorCode::CoreStopped
    );
    thread.join().unwrap();
    assert!(f.library.root_released(mpmp_library::RootId(f.root.0)));
    drop(lease); // Late completion can only send to the now closed actor channel.
}

#[test]
fn release_waits_for_active_prefetch_and_defers_approved_agent_success() {
    let (entered_tx, entered) = crossbeam_channel::bounded(1);
    let (open, gate) = crossbeam_channel::bounded(1);
    let f = fixture_with_resolver(Arc::new(PrefetchGate {
        entered: entered_tx,
        gates: Mutex::new(BTreeMap::from([("02.flac".into(), gate)])),
    }));
    ok(run(
        &f.core,
        Command::Play {
            context: PlayContext::Tracks { tracks: f.all() },
            start: 0,
            start_track: None,
        },
        Origin::Ui,
    ));
    entered.recv_timeout(TIMEOUT).unwrap();
    let requested = err(run(
        &f.core,
        Command::ReleaseRoot { root: f.root },
        Origin::agent("fixture"),
    ));
    let token = requested.details.confirmation.unwrap().token;
    let core = f.core.clone();
    let confirm_token = token.clone();
    let (tx, confirmed) = crossbeam_channel::bounded(1);
    let thread = std::thread::spawn(move || {
        tx.send(run(
            &core,
            Command::Confirm {
                token: confirm_token,
            },
            Origin::Ui,
        ))
        .unwrap();
    });
    let deadline = Instant::now() + TIMEOUT;
    while !f.library.root_released(mpmp_library::RootId(f.root.0)) {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    ok(run(&f.core, Command::SetMuted { muted: true }, Origin::Ui));
    assert!(confirmed.try_recv().is_err());
    assert!(matches!(
        f.core.query(Query::Snapshot),
        QueryResult::Snapshot { snapshot } if snapshot.state == PlayState::Stopped,
    ));
    assert!(
        !f.sub.events.try_iter().any(|e| matches!(e.event,
        Event::ConfirmationResolved { outcome } if outcome.token == token)),
        "approval must not publish success before prefetch exits"
    );
    open.send(()).unwrap();
    let receipt = ok(confirmed.recv_timeout(TIMEOUT).unwrap());
    let outcome = receipt.confirmed.unwrap();
    assert!(outcome.approved);
    assert!(matches!(*outcome.result.unwrap(), CommandResult::Ok(r) if r.root == Some(f.root)));
    wait_event(
        &f.sub,
        |e| matches!(&e.event, Event::ConfirmationResolved { outcome } if outcome.token == token),
    );
    thread.join().unwrap();
}

#[test]
fn released_root_serves_cached_waveform_and_does_not_queue_missing_waveforms() {
    let f = fixture();
    let id = f.id("01.flac");
    let waveform = mpmp_library::Waveform {
        buckets: 2048,
        channels: vec![mpmp_library::WaveformChannel {
            min: vec![-1; 2048],
            max: vec![1; 2048],
            rms: vec![1; 2048],
        }],
    };
    let version = f
        .library
        .file_version(mpmp_library::TrackId(id.0))
        .unwrap()
        .unwrap();
    assert!(
        f.library
            .store_waveform(mpmp_library::TrackId(id.0), version, &waveform)
            .unwrap()
    );
    let cached = f.core.query(Query::Waveform { track: id });
    ok(run(
        &f.core,
        Command::ReleaseRoot { root: f.root },
        Origin::Ui,
    ));
    assert_eq!(f.core.query(Query::Waveform { track: id }), cached);
    let missing = f.id("02.flac");
    assert!(!f.library.as_ref().waveform_accessible(missing));
    assert!(matches!(
        f.core.query(Query::Waveform { track: missing }),
        QueryResult::Waveform { waveform: None }
    ));
}

#[test]
fn remove_released_root_clears_suspension_and_revival_is_enabled() {
    for undo in [false, true] {
        let f = fixture();
        let suspended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = Arc::clone(&suspended);
        let _registration = f
            .core
            .register_root_access_handler(Arc::new(move |_, released| {
                observed.store(released, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }));
        ok(run(
            &f.core,
            Command::ReleaseRoot { root: f.root },
            Origin::Ui,
        ));
        assert!(suspended.load(std::sync::atomic::Ordering::SeqCst));
        let removed = ok(run(
            &f.core,
            Command::RemoveRoot { root: f.root },
            Origin::Ui,
        ));
        assert!(!f.library.root_released(mpmp_library::RootId(f.root.0)));
        assert!(!suspended.load(std::sync::atomic::Ordering::SeqCst));
        let revived = ok(run(
            &f.core,
            if undo {
                Command::UndoAction {
                    action: removed.action.unwrap(),
                }
            } else {
                Command::AddRoot {
                    path: f.music.display().to_string(),
                }
            },
            Origin::Ui,
        ));
        if let Some(job) = revived.job {
            wait_event(
                &f.sub,
                |e| matches!(e.event,Event::JobFinished {job:j,..} if j==job),
            );
        }
        let QueryResult::Roots { roots } = f.core.query(Query::Roots) else {
            panic!()
        };
        assert_eq!(roots.len(), 1);
        assert!(!roots[0].released);
        assert!(
            f.library
                .file_lease(mpmp_library::TrackId(f.id("01.flac").0))
                .is_ok()
        );
    }
}

#[test]
fn shutdown_during_main_decode_release_never_joins_blocked_source() {
    use std::sync::atomic::Ordering::SeqCst;
    let (f, gate, open) = main_read_fixture();
    let (released, thread) = begin_release(&f);
    assert!(f.core.shutdown_and_wait(Duration::from_secs(2)));
    assert_eq!(
        err(released.recv_timeout(Duration::from_secs(1)).unwrap()).code,
        ErrorCode::CoreStopped
    );
    assert!(!gate.closed.load(SeqCst));
    open.send(()).unwrap();
    thread.join().unwrap();
}

#[test]
fn undo_add_root_removal_uses_release_busy_and_clears_suspension_for_redo() {
    for guarded in [false, true] {
        let f = fixture();
        let undo = |action| {
            if guarded {
                f.core
                    .execute_described_undo(
                        Request::new(1, Command::UndoAction { action }),
                        Origin::Ui,
                        f.core.describe_undo(action).unwrap(),
                    )
                    .result
            } else {
                run(&f.core, Command::UndoAction { action }, Origin::Ui)
            }
        };
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
        let lease = f
            .library
            .file_lease(mpmp_library::TrackId(f.id("01.flac").0))
            .unwrap();
        let (released, thread) = begin_release(&f);
        assert_eq!(err(undo(add)).code, ErrorCode::JobBusy);
        drop(lease);
        ok(released.recv_timeout(TIMEOUT).unwrap());
        thread.join().unwrap();
        // A duplicate AddRoot is not ResumeRoot.
        ok(run(
            &f.core,
            Command::AddRoot {
                path: f.music.display().to_string(),
            },
            Origin::Ui,
        ));
        assert!(f.library.root_released(mpmp_library::RootId(f.root.0)));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&calls);
        let _registration = f
            .core
            .register_root_access_handler(Arc::new(move |path, released| {
                log.lock().unwrap().push((path.to_owned(), released));
                Ok(())
            }));
        let removed = ok(undo(add));
        assert!(!f.library.root_released(mpmp_library::RootId(f.root.0)));
        assert_eq!(calls.lock().unwrap().len(), 1);
        let restored = ok(undo(removed.action.unwrap()));
        f.job_events(restored.job.unwrap());
        ok(run(
            &f.core,
            Command::ReleaseRoot { root: f.root },
            Origin::Ui,
        ));
        ok(undo(restored.action.unwrap()));
        assert!(!f.library.root_released(mpmp_library::RootId(f.root.0)));
        f.core.shutdown();
    }
}

#[test]
fn committed_remove_publishes_events_and_retries_failed_participant() {
    let f = fixture();
    ok(run(
        &f.core,
        Command::ReleaseRoot { root: f.root },
        Origin::Ui,
    ));
    while f.sub.events.try_recv().is_ok() {}
    let successful = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&successful);
    let _success = f.core.register_root_access_handler(Arc::new(move |_, _| {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let _registration = f
        .core
        .register_root_access_handler(Arc::new(move |_, released| {
            if !released && seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Err(ContractError::failed(
                    ErrorCode::Io,
                    "injected recovery failure",
                ))
            } else {
                Ok(())
            }
        }));
    let error = err(run(
        &f.core,
        Command::RemoveRoot { root: f.root },
        Origin::Ui,
    ));
    assert_eq!((error.code, error.details.retryable), (ErrorCode::Io, true));
    assert!(matches!(f.core.query(Query::Roots),QueryResult::Roots{roots}if roots.is_empty()));
    assert!(!f.library.root_released(mpmp_library::RootId(f.root.0)));
    assert!(
        f.sub
            .events
            .try_iter()
            .any(|e| matches!(e.event, Event::LibraryChanged))
    );
    let retry = ok(run(
        &f.core,
        Command::RemoveRoot { root: f.root },
        Origin::Ui,
    ));
    assert!(!retry.changed && retry.action.is_none());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        successful.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "successful participants must not be notified again"
    );
    f.core.shutdown();
}

#[test]
fn release_join_and_lease_drain_share_one_total_deadline() {
    let (f, gate, open) = main_read_fixture();
    let lease = f
        .library
        .file_lease(mpmp_library::TrackId(f.id("01.flac").0))
        .unwrap();
    let start = Instant::now();
    let (released, thread) = begin_release(&f);
    std::thread::sleep(Duration::from_secs(2));
    open.send(()).unwrap();
    let error = err(released.recv_timeout(Duration::from_secs(4)).unwrap());
    assert_eq!(error.code, ErrorCode::Io);
    assert!(start.elapsed() < Duration::from_secs(6));
    assert!(gate.closed.load(std::sync::atomic::Ordering::SeqCst));
    drop(lease);
    thread.join().unwrap();
    ok(run(
        &f.core,
        Command::ReleaseRoot { root: f.root },
        Origin::Ui,
    ));
    f.core.shutdown();
}

#[test]
fn native_notification_timeout_retains_identity_serializes_resume_and_shutdown() {
    for shutdown in [false, true] {
        let f = fixture();
        let (entered, started) = crossbeam_channel::bounded(1);
        let (open, blocked) = crossbeam_channel::bounded(1);
        let native = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = Arc::clone(&native);
        let _registration = f
            .core
            .register_root_access_handler(Arc::new(move |_, released| {
                if released {
                    entered.send(()).unwrap();
                    blocked.recv_timeout(TIMEOUT).unwrap();
                }
                seen.store(released, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }));
        let (released, thread) = begin_release(&f);
        started.recv_timeout(TIMEOUT).unwrap();
        ok(run(
            &f.core,
            Command::SetVolume { volume: 0.37 },
            Origin::Ui,
        ));
        let error = err(released.recv_timeout(Duration::from_secs(7)).unwrap());
        assert_eq!(
            (error.kind, error.code, error.details.retryable),
            (ErrorKind::Failed, ErrorCode::Io, true)
        );
        for command in [
            Command::ReleaseRoot { root: f.root },
            Command::ResumeRoot { root: f.root },
            Command::RemoveRoot { root: f.root },
        ] {
            assert_eq!(
                err(run(&f.core, command, Origin::Ui)).code,
                ErrorCode::JobBusy
            );
        }
        assert!(f.library.root_released(mpmp_library::RootId(f.root.0)));
        if shutdown {
            assert!(f.core.shutdown_and_wait(Duration::from_secs(2)));
            open.send(()).unwrap();
        } else {
            open.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match run(&f.core, Command::ResumeRoot { root: f.root }, Origin::Ui) {
                    CommandResult::Ok(_) => break,
                    other => {
                        assert_eq!(err(other).code, ErrorCode::JobBusy);
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }
            assert!(
                !native.load(std::sync::atomic::Ordering::SeqCst),
                "late release cannot override resume"
            );
            assert!(!f.library.root_released(mpmp_library::RootId(f.root.0)));
            f.core.shutdown();
        }
        thread.join().unwrap();
    }
}
