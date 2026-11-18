//! Tokens scenarios.
use super::*;

#[test]
fn agent_tokens_authenticate_until_renewed_or_removed() {
    let dir = tempfile::tempdir().unwrap();
    let (core, sub) = start(dir.path());
    let register = |name: &str| {
        run(
            &core,
            Command::RegisterAgent { name: name.into() },
            Origin::Ui,
        )
    };
    let before = unix_ms();
    let first = ok(register("claude")).agent_token.unwrap();
    assert!(first.starts_with("pmpa_") && first.len() == 69, "{first}");
    let events = drain(&sub);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        (&events[0].event, &events[0].origin),
        (&Event::AgentsChanged, &Some(Origin::Ui))
    );
    let codex = ok(register("codex")).agent_token.unwrap();
    assert_ne!(first, codex);

    // Names are checked and unique.
    let e = err(register("claude"));
    assert_eq!(
        (e.code, e.details.field.as_deref()),
        (ErrorCode::AlreadyExists, Some("command.name"))
    );
    for bad in ["", "two words", "agent:x", &"x".repeat(65)] {
        let e = err(register(bad));
        assert_eq!(e.code, ErrorCode::InvalidAgentName, "{bad:?}");
    }

    drain(&sub);
    // A token names its agent; anything else names none.
    assert_eq!(core.authenticate_agent(&first).as_deref(), Some("claude"));
    assert_eq!(core.authenticate_agent(&codex).as_deref(), Some("codex"));
    for wrong in [
        "",
        "pmpa_",
        &first[..first.len() - 1],
        &first.to_uppercase(),
    ] {
        assert_eq!(core.authenticate_agent(wrong), None, "{wrong:?}");
    }
    let listed = agents(&core);
    assert_eq!(
        listed.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
        ["claude", "codex"]
    );
    assert!(listed[0].created_at_ms >= before);
    assert!(
        listed[0]
            .last_seen_at_ms
            .is_some_and(|t| t >= listed[0].created_at_ms)
    );
    // Seeing an agent announces nothing.
    assert!(drain(&sub).is_empty());

    // A new token replaces the old one at once.
    let renewed = ui(
        &core,
        Command::RenewAgentToken {
            agent: "claude".into(),
        },
    )
    .agent_token
    .unwrap();
    assert_ne!(renewed, first);
    assert_eq!(core.authenticate_agent(&first), None);
    assert_eq!(core.authenticate_agent(&renewed).as_deref(), Some("claude"));
    let e = err(run(
        &core,
        Command::RenewAgentToken {
            agent: "nobody".into(),
        },
        Origin::Ui,
    ));
    assert_eq!(
        (e.kind, e.code, e.details.missing.clone()),
        (
            ErrorKind::NotFound,
            ErrorCode::AgentNotFound,
            vec![ObjectRef::Agent {
                name: "nobody".into()
            }]
        )
    );

    // Tokens and agents outlive core; only the hash is stored.
    stop(core, sub);
    let db = std::fs::read(dir.path().join("library.db")).unwrap();
    assert!(
        !db.windows(renewed.len()).any(|w| w == renewed.as_bytes()),
        "the token itself is never stored"
    );
    let (core, sub) = start(dir.path());
    assert_eq!(core.authenticate_agent(&renewed).as_deref(), Some("claude"));
    assert_eq!(core.authenticate_agent(&first), None);
    assert_eq!(core.authenticate_agent(&codex).as_deref(), Some("codex"));

    // A removed agent's token names no one, and its grants go with it.
    ui(
        &core,
        grant(
            "codex",
            None,
            class(OperationClass::LocalPath),
            Scope::All,
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::LocalPath),
            Scope::All,
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    drain(&sub);
    ui(
        &core,
        Command::RemoveAgent {
            agent: "codex".into(),
        },
    );
    let events: Vec<Event> = drain(&sub).into_iter().map(|e| e.event).collect();
    assert_eq!(events, [Event::AgentsChanged, Event::PermissionsChanged]);
    assert_eq!(core.authenticate_agent(&codex), None);
    assert_eq!(agents(&core).len(), 1);
    assert!(grants(&core, Some("codex")).is_empty());
    assert_eq!(grants(&core, None).len(), 1);
    let e = err(run(
        &core,
        Command::RemoveAgent {
            agent: "codex".into(),
        },
        Origin::Ui,
    ));
    assert_eq!(e.code, ErrorCode::AgentNotFound);
    stop(core, sub);
    let (core, _sub) = start(dir.path());
    assert_eq!(core.authenticate_agent(&codex), None);
    assert!(grants(&core, Some("codex")).is_empty());
    assert_eq!(grants(&core, Some("claude")).len(), 1);
}

/// Guarantee 13: a retry is recognised by the agent's name, whatever its session (a
/// retry after reconnecting).
#[test]
fn idempotency_keys_belong_to_the_agent_not_its_session() {
    let dir = tempfile::tempdir().unwrap();
    let (core, _sub) = start(dir.path());
    let create = Command::CreatePlaylist {
        name: "Mix".into(),
        kind: PlaylistKind::Manual,
    };
    let send = |origin: Origin| {
        core.execute(
            Request {
                request_id: 1,
                idempotency_key: Some("k1".into()),
                command: create.clone(),
            },
            origin,
        )
        .result
    };
    let first = send(session("claude", "s1"));
    let retry = send(session("claude", "s2"));
    assert_eq!(first, retry);
    assert_eq!(send(Origin::agent("claude")), first);
    // Another agent's key is its own.
    assert_ne!(send(session("codex", "s1")), first);
    match core.query(Query::Playlists {
        deleted: false,
        offset: 0,
        limit: 10,
    }) {
        QueryResult::Playlists { total, .. } => assert_eq!(total, 2),
        other => panic!("{other:?}"),
    }
    // The stored origin has no session.
    assert!(
        actions(&core)
            .iter()
            .all(|a| matches!(&a.origin, Origin::Agent { session: None, .. }))
    );
}

/// A token is shown once: a retry with the same idempotency key runs the command again
/// rather than handing the token out a second time. The key still belongs to that command;
/// another command with it is `idempotency_key_reused`.
#[test]
fn tokens_are_not_kept_for_retries() {
    let dir = tempfile::tempdir().unwrap();
    let (core, _sub) = start(dir.path());
    let send = |key: &str, command: Command| {
        core.execute(
            Request {
                request_id: 1,
                idempotency_key: Some(key.into()),
                command,
            },
            Origin::Ui,
        )
        .result
    };
    let register = Command::RegisterAgent {
        name: "claude".into(),
    };
    let token = ok(send("same", register.clone())).agent_token.unwrap();
    let again = err(send("same", register));
    assert_eq!(again.code, ErrorCode::AlreadyExists);
    let renew = Command::RenewAgentToken {
        agent: "claude".into(),
    };
    let reused = err(send("same", renew.clone()));
    assert_eq!(
        (reused.code, reused.details.field.as_deref()),
        (ErrorCode::IdempotencyKeyReused, Some("idempotency_key"))
    );
    assert_eq!(core.authenticate_agent(&token).as_deref(), Some("claude"));
    let first = ok(send("renew", renew.clone())).agent_token.unwrap();
    let second = ok(send("renew", renew)).agent_token.unwrap();
    assert_ne!(first, second);
    assert_eq!(core.authenticate_agent(&token), None);
    assert_eq!(core.authenticate_agent(&first), None);
    assert_eq!(core.authenticate_agent(&second).as_deref(), Some("claude"));
}
