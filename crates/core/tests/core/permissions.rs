//! Permissions scenarios.
use super::*;

#[test]
fn events_carry_the_origin_of_the_command_that_caused_them() {
    // Ticks (positions) and the engine (a track starting) cause events of their own,
    // without an origin; every event a command caused carries the command's.
    let caused_by = |events: &[EventEnvelope], origin: &Origin| {
        assert!(
            events
                .iter()
                .all(|e| e.origin.is_none() || e.origin.as_ref() == Some(origin)),
            "{events:#?}"
        );
        events
            .iter()
            .filter(|e| e.origin.as_ref() == Some(origin))
            .map(|e| e.event.clone())
            .collect::<Vec<_>>()
    };
    let mut h = Harness::new(None);
    let agent = Origin::agent("dj");
    let CommandResult::Ok(_) = h.run_as(play(album()), agent.clone()) else {
        panic!()
    };
    let caused = caused_by(&h.drain(), &agent);
    assert!(
        caused
            .iter()
            .any(|e| matches!(e, Event::StateChanged { .. }))
    );
    assert!(
        caused
            .iter()
            .any(|e| matches!(e, Event::QueueChanged { .. }))
    );
    h.until(|e| matches!(e, Event::TrackStarted { .. }));
    h.drain();
    let CommandResult::Ok(_) = h.run_as(Command::Next, Origin::MediaControls) else {
        panic!()
    };
    let caused = caused_by(&h.drain(), &Origin::MediaControls);
    assert!(
        caused.iter().any(|e| matches!(e, Event::HistoryChanged)),
        "{caused:#?}"
    );
    assert!(
        caused
            .iter()
            .any(|e| matches!(e, Event::StateChanged { .. }))
    );
    h.until(|e| matches!(e, Event::TrackStarted { .. }));
    h.drain();
    h.ok(Command::Pause);
    let caused = caused_by(&h.drain(), &Origin::Ui);
    assert!(
        caused
            .iter()
            .any(|e| matches!(e, Event::StateChanged { .. }))
    );
    assert!(caused.iter().any(|e| matches!(e, Event::Position { .. })));
}

#[test]
fn origins_are_held_to_their_policy() {
    let mut h = Harness::new(None);
    let agent = Origin::agent("dj");
    let remote = Origin::Remote {
        device: "phone".into(),
    };
    let agent_sub = h
        .core
        .subscribe(agent.clone(), SubscribeOptions::default())
        .unwrap();

    // Media controls: transport only.
    let error = h.err_as(play(album()), Origin::MediaControls);
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Forbidden, ErrorCode::OriginNotAllowed)
    );
    let CommandResult::Ok(_) = h.run_as(Command::Pause, Origin::MediaControls) else {
        panic!()
    };

    // Remote: no local paths.
    for command in [
        Command::AddRoot { path: "/".into() },
        Command::ImportM3u {
            path: "/x.m3u".into(),
            name: None,
        },
    ] {
        let error = h.err_as(command, remote.clone());
        assert_eq!(
            (error.kind, error.code),
            (ErrorKind::Forbidden, ErrorCode::PathNotAllowed)
        );
    }

    // Agent: local paths wait for the user, who alone sees the request.
    let error = h.err_as(
        Command::AddRoot {
            path: absent("music"),
        },
        agent.clone(),
    );
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::NeedsConfirmation, ErrorCode::NeedsConfirmation)
    );
    let ticket = error.details.confirmation.clone().unwrap();
    assert!(ticket.summary.contains("music"), "{ticket:?}");
    let requested = h.until_envelope(|e| matches!(e.event, Event::ConfirmationRequested { .. }));
    let Event::ConfirmationRequested { confirmation } = requested.event else {
        unreachable!()
    };
    assert_eq!(confirmation.token, ticket.token);
    assert_eq!(confirmation.requested_by, agent);
    assert!(
        !agent_sub
            .events
            .try_iter()
            .any(|e| matches!(e.event, Event::ConfirmationRequested { .. })),
        "only UI subscribers see confirmation requests"
    );

    // Only the UI may answer; the answer carries the command's own result.
    let error = h.err_as(
        Command::Confirm {
            token: ticket.token.clone(),
        },
        agent.clone(),
    );
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Forbidden, ErrorCode::OriginNotAllowed)
    );
    let receipt = h.ok(Command::Confirm {
        token: ticket.token.clone(),
    });
    let confirmed = receipt.confirmed.unwrap();
    assert!(confirmed.approved);
    // Approved, even though the command then fails.
    assert_eq!(confirmed.reason, Some(ConfirmationReason::Approved));
    // The test catalog has no roots: the held command itself fails.
    let Some(result) = confirmed.result else {
        panic!()
    };
    let CommandResult::Err { error } = *result else {
        panic!("{result:?}")
    };
    assert_eq!(error.code, ErrorCode::NotImplemented);
    let error = h.err(Command::Confirm {
        token: ticket.token,
    });
    assert_eq!(error.code, ErrorCode::ConfirmationNotFound);
}

/// A confirmation the user does not answer in time expires: `ConfirmationResolved` with
/// `reason: expired`, and a retry with the request's key gets `confirmation_expired`
#[test]
fn unanswered_confirmations_expire() {
    const TTL: Duration = Duration::from_millis(300);
    let mut h = Harness::with_config(
        Arc::new(MemoryCatalog::standard()),
        CoreConfig {
            confirmation_ttl: Some(TTL),
            ..Default::default()
        },
    );
    let agent = Origin::agent("a");
    let purge = |id| Request {
        request_id: id,
        idempotency_key: Some("purge".into()),
        command: Command::PurgePlaylists { playlists: None },
    };
    let unix_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    };
    let asked = unix_ms();
    let CommandResult::Err { error } = h.core.execute(purge(1), agent.clone()).result else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::NeedsConfirmation);
    let ticket = error.details.confirmation.unwrap();
    let ttl = TTL.as_millis() as i64;
    assert!(
        (ttl..ttl + 5_000).contains(&(ticket.expires_at_ms - asked)),
        "{ticket:?}, asked at {asked}"
    );
    let resolved = h.until(
        |e| matches!(e, Event::ConfirmationResolved { outcome } if outcome.token == ticket.token),
    );
    let Event::ConfirmationResolved { outcome } = resolved else {
        unreachable!()
    };
    assert_eq!(
        (outcome.approved, outcome.reason, outcome.result),
        (false, Some(ConfirmationReason::Expired), None)
    );
    let CommandResult::Err { error } = h.core.execute(purge(2), agent).result else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::ConfirmationExpired);
    let error = h.err(Command::Confirm {
        token: ticket.token,
    });
    assert_eq!(error.code, ErrorCode::ConfirmationNotFound);
}

/// A command that cannot run is refused before the user is asked to approve it; the library checks ids (tests/library.rs), core the M3U file name.
#[test]
fn no_confirmation_is_asked_for_an_m3u_path_that_is_not_one() {
    let mut h = Harness::new(None);
    let agent = Origin::agent("a");
    let error = h.err_as(
        Command::ExportM3u {
            playlist: PlaylistId(1),
            path: absent("list.txt"),
        },
        agent.clone(),
    );
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Invalid, ErrorCode::NotAnM3u)
    );
    h.drain();
    assert!(
        !h.seen
            .iter()
            .any(|e| matches!(e.event, Event::ConfirmationRequested { .. }))
    );
    // A valid name is held as before.
    let error = h.err_as(
        Command::ExportM3u {
            playlist: PlaylistId(1),
            path: absent("list.M3U8"),
        },
        agent,
    );
    assert_eq!(error.kind, ErrorKind::NeedsConfirmation);
}

/// A catalog that stores no permissions: core keeps agents and grants in memory (lost
/// when it stops), with grant ids from the clock.
#[test]
fn permissions_work_without_a_catalog_that_stores_them() {
    let mut h = Harness::new(None);
    let token = h
        .ok(Command::RegisterAgent { name: "dj".into() })
        .agent_token
        .unwrap();
    assert_eq!(h.core.authenticate_agent(&token).as_deref(), Some("dj"));
    let grant = h
        .ok(Command::GrantPermission {
            agent: "dj".into(),
            session: None,
            operation: Operation::Class {
                class: OperationClass::LocalPath,
            },
            scope: Scope::All,
            until: Expiry::Forever,
            effect: Effect::Forbid,
        })
        .grant
        .unwrap();
    assert!(grant.0 >= unix_ms() as i64 - 60_000, "{grant:?}");
    let e = h.err_as(
        Command::AddRoot {
            path: absent("music"),
        },
        Origin::agent("dj"),
    );
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    // Another agent is asked as before.
    let e = h.err_as(
        Command::AddRoot {
            path: absent("music"),
        },
        Origin::agent("other"),
    );
    assert_eq!(e.kind, ErrorKind::NeedsConfirmation);
    // A library scope needs a folder the catalog knows.
    let e = h.err(Command::GrantPermission {
        agent: "dj".into(),
        session: None,
        operation: Operation::Class {
            class: OperationClass::LocalPath,
        },
        scope: Scope::Library {
            root: RootId(1),
            prefix: None,
        },
        until: Expiry::Forever,
        effect: Effect::Allow,
    });
    assert_eq!(e.code, ErrorCode::RootNotFound);
}

#[test]
fn playback_limits_required_null_and_origin_policy_are_enforced() {
    let mut h = Harness::new(None);
    let response: Response = serde_json::from_str(&h.core.execute_json(
        r#"{"request_id":100,"command":{"type":"set_sleep_timer"}}"#,
        Origin::Ui,
    ))
    .unwrap();
    assert!(matches!(
        response.result,
        CommandResult::Err { error } if error.code == ErrorCode::InvalidRequest,
    ));
    let error = h.err_as(
        Command::SetSleepTimer {
            duration_ms: Some(1000),
        },
        Origin::MediaControls,
    );
    assert_eq!(error.code, ErrorCode::OriginNotAllowed);
    h.ok(play(album()));
    assert!(matches!(
        h.run_as(
            Command::SetStopAfterCurrent {
                enabled: true,
                expected_entry: None
            },
            Origin::MediaControls
        ),
        CommandResult::Ok(_)
    ));
    for origin in [
        Origin::Ui,
        Origin::System,
        Origin::agent("dj"),
        Origin::Remote {
            device: "remote".into(),
        },
    ] {
        assert_eq!(
            Command::SetSleepTimer { duration_ms: None }.policy(&origin),
            Policy::Allow
        );
    }
    assert_eq!(
        Command::SetSleepTimer { duration_ms: None }.risk(),
        Risk::Write
    );
    assert_eq!(
        Command::SetStopAfterCurrent {
            enabled: false,
            expected_entry: None
        }
        .risk(),
        Risk::Control
    );
}

#[test]
fn playback_limit_navigation_and_agent_forbid_keep_existing_permission_semantics() {
    let mut h = Harness::new(None);
    h.ok(play(album()));
    h.ok(Command::Pause);
    h.ok(Command::SetStopAfterCurrent {
        enabled: true,
        expected_entry: None,
    });
    h.ok(Command::SetSleepTimer {
        duration_ms: Some(MAX_SLEEP_TIMER_MS),
    });
    let before = h.snapshot().playback_limits.unwrap().stop_after_current;
    assert_eq!(
        h.err(play(PlayContext::Tracks { tracks: vec![] })).code,
        ErrorCode::EmptyContext
    );
    assert_eq!(
        h.snapshot().playback_limits.unwrap().stop_after_current,
        before
    );
    h.ok(play(album()));
    let limits = h.snapshot().playback_limits.unwrap();
    assert!(limits.stop_after_current.is_none() && limits.sleep_timer.is_some());
    h.ok(Command::RegisterAgent { name: "dj".into() });
    h.ok(Command::GrantPermission {
        agent: "dj".into(),
        session: None,
        operation: Operation::Class {
            class: OperationClass::Playback,
        },
        scope: Scope::All,
        until: Expiry::Forever,
        effect: Effect::Forbid,
    });
    for command in [
        Command::SetStopAfterCurrent {
            enabled: true,
            expected_entry: None,
        },
        Command::SetSleepTimer { duration_ms: None },
    ] {
        assert_eq!(
            h.err_as(command, Origin::agent("dj")).code,
            ErrorCode::PermissionDenied
        );
    }
}
