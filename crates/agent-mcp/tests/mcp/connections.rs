//! Connections scenarios.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_connection_ends_its_session() {
    let h = Harness::new(ServerOptions::default());
    within(async {
        let client = h.client().await;
        let extra = folder(h.dir.path(), "extra");
        ok(
            &client,
            "library_folders",
            json!({"action": "add", "path": extra.display().to_string()}),
        )
        .await;
        let request = h.confirmation_request();
        let Origin::Agent {
            session: Some(session),
            ..
        } = request.requested_by.clone()
        else {
            panic!("{:?}", request.requested_by)
        };
        // "Allow for this session", as the UI offers it.
        h.ui_ok(Command::GrantPermission {
            agent: "claude".into(),
            session: Some(session.clone()),
            operation: Operation::Class {
                class: OperationClass::LocalPath,
            },
            scope: Scope::All,
            until: Expiry::Session,
            effect: Effect::Allow,
        });
        h.ui_ok(Command::Confirm {
            token: request.token,
        });
        assert_eq!(h.grants().len(), 1);
        // The session's grant lets it add another folder without asking.
        let other = folder(h.dir.path(), "other");
        let added = ok(
            &client,
            "library_folders",
            json!({"action": "add", "path": other.display().to_string()}),
        )
        .await;
        assert!(added["root"].is_i64(), "{added}");

        client.cancel().await.unwrap();
        let deadline = Instant::now() + TIMEOUT;
        while h
            .grants()
            .iter()
            .any(|g| g.session.as_deref() == Some(&session))
        {
            assert!(Instant::now() < deadline, "the session's grant stayed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // A new connection is a new session.
        let client = h.client().await;
        let third = folder(h.dir.path(), "third");
        let asked = ok(
            &client,
            "library_folders",
            json!({"action": "add", "path": third.display().to_string()}),
        )
        .await;
        assert_eq!(asked["status"], "needs_confirmation");
        client.cancel().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renewed_or_removed_agents_are_disconnected() {
    let h = Harness::new(ServerOptions::default());
    within(async {
        let client = h.client().await;
        ok(&client, "player_status", json!({})).await;
        let renewed = h
            .ui_ok(Command::RenewAgentToken {
                agent: "claude".into(),
            })
            .agent_token
            .unwrap();
        tokio::time::timeout(TIMEOUT, client.waiting())
            .await
            .expect("the connection stayed open")
            .unwrap();
        // The old token no longer gets in; the new one does.
        assert!(
            open(&h.endpoint, &h.token)
                .await
                .unwrap_err()
                .starts_with("ERR invalid_token ")
        );
        let client = h.client_with(&renewed).await;
        ok(&client, "player_status", json!({})).await;

        let codex_token = h.register("codex");
        let codex = h.client_with(&codex_token).await;
        ok(&codex, "player_status", json!({})).await;
        h.ui_ok(Command::RemoveAgent {
            agent: "codex".into(),
        });
        tokio::time::timeout(TIMEOUT, codex.waiting())
            .await
            .expect("the connection stayed open")
            .unwrap();
        // Another agent's change leaves this one connected.
        ok(&client, "player_status", json!({})).await;
        client.cancel().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_handshake_refuses_wrong_tokens() {
    let h = Harness::new(ServerOptions::default());
    within(async {
        let wrong = format!("pmpa_{}", "0".repeat(64));
        assert_eq!(
            open(&h.endpoint, &wrong).await.unwrap_err(),
            handshake::err_line(handshake::Refusal::InvalidToken).trim_end()
        );
        assert!(
            open_with(&h.endpoint, "HELLO\n")
                .await
                .unwrap_err()
                .starts_with("ERR bad_handshake ")
        );
        assert!(
            open_with(&h.endpoint, &format!("PLAYER-MCP/9 {}\n", h.token))
                .await
                .unwrap_err()
                .starts_with("ERR unsupported_version ")
        );
        // A good one still gets in.
        let client = h.client().await;
        ok(&client, "player_status", json!({})).await;
        client.cancel().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn too_many_connections_are_refused() {
    let h = Harness::new(ServerOptions::default().with_max_connections(1));
    within(async {
        let client = h.client().await;
        assert!(
            open(&h.endpoint, &h.token)
                .await
                .unwrap_err()
                .starts_with("ERR busy ")
        );
        client.cancel().await.unwrap();
        // Once it is gone, there is room again.
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match open(&h.endpoint, &h.token).await {
                Ok(_) => break,
                Err(line) => assert!(line.starts_with("ERR busy "), "{line}"),
            }
            assert!(Instant::now() < deadline, "no room");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
}

/// The 2026-07-28 protocol has no `initialize`: every request carries its own metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clients_without_initialize_are_served() {
    let h = Harness::new(ServerOptions::default());
    within(async {
        let (read, write) = open(&h.endpoint, &h.token).await.unwrap();
        let client = ()
            .serve_with_lifecycle(
                (read, write),
                ClientLifecycleMode::Discover {
                    preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                },
            )
            .await
            .unwrap();
        let tools = client.list_all_tools().await.unwrap();
        assert_eq!(tools.len(), 13);
        let found = ok(&client, "search_tracks", json!({"text": "river"})).await;
        assert_eq!(found["tracks"][0]["title"], "Red River");
        client.cancel().await.unwrap();
    })
    .await;
}

/// The model's idempotency keys hold on its connection only: another session of the same
/// agent with the same key runs its own command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotency_keys_belong_to_the_connection() {
    let h = Harness::new(ServerOptions::default());
    within(async {
        let a = h.client().await;
        let b = h.client().await;
        let shuffle = |on: bool| {
            json!({"command": {"type": "set_shuffle", "shuffle": on}, "idempotency_key": "k"})
        };
        assert_eq!(ok(&a, "contract_command", shuffle(true)).await["changed"], true);
        // Another connection: it runs (shuffle is on already).
        assert_eq!(ok(&b, "contract_command", shuffle(true)).await["changed"], false);
        // On one connection a key still means one command…
        assert_eq!(
            error(&b, "contract_command", shuffle(false)).await,
            pair("invalid", "idempotency_key_reused")
        );
        // … and a retry gets the first result without running again.
        assert_eq!(ok(&a, "contract_command", shuffle(true)).await["changed"], true);
        a.cancel().await.unwrap();
        b.cancel().await.unwrap();
    })
    .await;
}

/// A connection serves other calls while one waits for the user.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_serves_calls_while_one_waits() {
    let h = Harness::new(ServerOptions::default());
    within(async {
        let client = h.client().await;
        let extra = folder(h.dir.path(), "extra");
        let asked = ok(
            &client,
            "library_folders",
            json!({"action": "add", "path": extra.display().to_string()}),
        )
        .await;
        let token = asked["token"].as_str().unwrap().to_owned();
        h.confirmation_request();
        let waiter = {
            let client = client.peer().clone();
            let token = token.clone();
            tokio::spawn(async move {
                call(&client, "wait_for_confirmation", json!({"token": token})).await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = Instant::now();
        assert_eq!(
            ok(&client, "player_status", json!({})).await["state"],
            "stopped"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!waiter.is_finished());
        h.ui_ok(Command::Confirm { token });
        let (failed, answer) = waiter.await.unwrap();
        assert!(!failed, "{answer}");
        assert_eq!(answer["status"], "approved");
        client.cancel().await.unwrap();
    })
    .await;
}

/// The first line is bounded in length and time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_handshake_is_bounded() {
    let h = Harness::new(ServerOptions::default());
    within(async {
        let long = format!("{} {}\n", handshake::HELLO, "x".repeat(2000));
        let refused = open_with(&h.endpoint, &long).await.unwrap_err();
        assert!(refused.starts_with("ERR bad_handshake "), "{refused}");
        // Silent: closed without a word once the handshake's time is up.
        let stream = endpoint::connect(&h.endpoint).await.unwrap();
        let started = Instant::now();
        let (mut read, _write) = tokio::io::split(stream);
        let mut got = Vec::new();
        let closed = tokio::time::timeout(TIMEOUT, read.read_to_end(&mut got))
            .await
            .expect("the silent connection stayed open");
        assert!(got.is_empty(), "{closed:?}: {got:?}");
        let waited = started.elapsed();
        assert!(
            waited >= handshake::TIMEOUT - Duration::from_millis(500),
            "{waited:?}"
        );
        // A good one still gets in.
        let client = h.client().await;
        ok(&client, "player_status", json!({})).await;
        client.cancel().await.unwrap();
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_socket_is_private_and_removed_at_shutdown() {
    use std::os::unix::fs::PermissionsExt;

    let mut h = Harness::new(ServerOptions::default());
    let socket = h.endpoint.path().to_path_buf();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(socket.parent().unwrap()), 0o700);
    // A second server cannot take a live endpoint.
    let second = Server::start(
        h.core.clone(),
        ServerOptions::default().with_endpoint(h.endpoint.clone()),
    );
    assert_eq!(second.err().unwrap().kind(), std::io::ErrorKind::AddrInUse);
    tokio::task::block_in_place(|| h.server.take().unwrap().shutdown());
    assert!(!socket.exists());
    // A folder that is a symlink is refused.
    let target = folder(h.dir.path(), "elsewhere");
    let link = h.dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let refused = Server::start(
        h.core.clone(),
        ServerOptions::default().with_endpoint(Endpoint::new(link.join("mcp.sock")).unwrap()),
    );
    assert_eq!(
        refused.err().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}
