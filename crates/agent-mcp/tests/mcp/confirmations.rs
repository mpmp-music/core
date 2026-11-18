//! Confirmations scenarios.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirmations_are_waited_for() {
    let h = Harness::new(ServerOptions::default());
    within(async {
        let client = h.client().await;
        let extra = folder(h.dir.path(), "extra");
        let add = |path: &Path| json!({"action": "add", "path": path.display().to_string()});

        // Approved while waiting: the waiter gets the command's receipt.
        let asked = ok(&client, "library_folders", add(&extra)).await;
        assert_eq!(asked["status"], "needs_confirmation");
        let token = asked["token"].as_str().unwrap().to_owned();
        let request = h.confirmation_request();
        assert_eq!(request.token, token);
        let Origin::Agent { name, session } = &request.requested_by else {
            panic!("{:?}", request.requested_by)
        };
        assert_eq!(name, "claude");
        let session = session.clone().expect("a session");
        assert_eq!(session.len(), 36);
        let waiter = {
            let client = client.peer().clone();
            let token = token.clone();
            tokio::spawn(async move {
                call(&client, "wait_for_confirmation", json!({"token": token})).await
            })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        h.ui_ok(Command::Confirm {
            token: token.clone(),
        });
        let (failed, answer) = waiter.await.unwrap();
        assert!(!failed, "{answer}");
        assert_eq!(answer["status"], "approved");
        assert!(answer["receipt"]["root"].is_i64(), "{answer}");
        // Asked again later: the same answer.
        let again = ok(&client, "wait_for_confirmation", json!({"token": token})).await;
        assert_eq!(again["status"], "approved");

        // Rejected (before the agent waits: the answer is kept).
        h.wait_ui(|e| matches!(e, Event::JobFinished { .. }));
        let asked = ok(
            &client,
            "library_folders",
            json!({"action": "remove", "root": answer["receipt"]["root"]}),
        )
        .await;
        let token = asked["token"].as_str().unwrap().to_owned();
        h.confirmation_request();
        h.ui_ok(Command::Reject {
            token: token.clone(),
        });
        let answer = ok(&client, "wait_for_confirmation", json!({"token": token})).await;
        assert_eq!(answer["status"], "rejected");

        // Not answered: pending when the wait is up.
        let other = folder(h.dir.path(), "other");
        let asked = ok(&client, "library_folders", add(&other)).await;
        let token = asked["token"].as_str().unwrap().to_owned();
        h.confirmation_request();
        let started = Instant::now();
        let answer = ok(
            &client,
            "wait_for_confirmation",
            json!({"token": token, "timeout_s": 1}),
        )
        .await;
        assert_eq!(answer["status"], "pending");
        assert!(started.elapsed() >= Duration::from_millis(900));

        // Another session of the same agent, or another agent, cannot wait for it.
        let second = h.client().await;
        assert_eq!(
            error(&second, "wait_for_confirmation", json!({"token": token})).await,
            pair("not_found", "confirmation_not_found")
        );
        let codex = h.register("codex");
        let third = h.client_with(&codex).await;
        assert_eq!(
            error(&third, "wait_for_confirmation", json!({"token": token})).await,
            pair("not_found", "confirmation_not_found")
        );
        second.cancel().await.unwrap();
        third.cancel().await.unwrap();

        // The connection closes while it waits: core rejects it.
        client.cancel().await.unwrap();
        let resolved = h.wait_ui(
            |e| matches!(e, Event::ConfirmationResolved { outcome } if outcome.token == token),
        );
        let Event::ConfirmationResolved { outcome } = resolved.event else {
            unreachable!()
        };
        assert!(!outcome.approved);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waits_are_capped() {
    let h = Harness::new(ServerOptions::default().with_max_wait(Duration::from_millis(300)));
    within(async {
        let client = h.client().await;
        let extra = folder(h.dir.path(), "extra");
        let asked = ok(
            &client,
            "library_folders",
            json!({"action": "add", "path": extra.display().to_string()}),
        )
        .await;
        h.confirmation_request();
        let started = Instant::now();
        let answer = ok(
            &client,
            "wait_for_confirmation",
            json!({"token": asked["token"], "timeout_s": 50}),
        )
        .await;
        assert_eq!(answer["status"], "pending");
        assert!(started.elapsed() < Duration::from_secs(10));
        client.cancel().await.unwrap();
    })
    .await;
}

/// Approved, but refused when checked again: a failure like any other, saying it was
/// approved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approved_command_that_fails_is_a_failure() {
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
        // The user forbids it meanwhile, then approves the request.
        h.ui_ok(Command::GrantPermission {
            agent: "claude".into(),
            session: None,
            operation: Operation::Class {
                class: OperationClass::LocalPath,
            },
            scope: Scope::All,
            until: Expiry::Forever,
            effect: Effect::Forbid,
        });
        h.ui_ok(Command::Confirm {
            token: token.clone(),
        });
        let (failed, value) = call(&client, "wait_for_confirmation", json!({"token": token})).await;
        assert!(failed, "{value}");
        assert_eq!(value["kind"], "forbidden");
        assert_eq!(value["code"], "permission_denied");
        assert!(value["message"].is_string(), "{value}");
        assert_eq!(value["status"], "approved");
        assert_eq!(value["token"], token.as_str());
        client.cancel().await.unwrap();
    })
    .await;
}

/// Closing a connection ends its session before waiting for a check still in core.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_withdraws_commands_still_being_checked_in_core() {
    let slow = Arc::new(SlowLocations::default());
    let config = CoreConfig {
        locations: Some(slow.clone()),
        agent_file_system_timeout: Some(Duration::from_secs(30)),
        automatic_loudness_scan: Some(false),
        ..Default::default()
    };
    let mut h = Harness::with_config(ServerOptions::default(), config);
    within(async {
        for shutdown in [false, true] {
            let client = h.client().await;
            let extra = folder(
                h.dir.path(),
                if shutdown { "shutdown" } else { "disconnect" },
            );
            let add = json!({"action": "add", "path": extra.display().to_string()});
            ok(&client, "library_folders", add.clone()).await;
            let request = h.confirmation_request();
            let Origin::Agent {
                session: Some(session),
                ..
            } = request.requested_by
            else {
                panic!()
            };
            h.ui_ok(Command::Reject {
                token: request.token,
            });
            h.ui_ok(Command::GrantPermission {
                agent: "claude".into(),
                session: Some(session),
                operation: Operation::Command {
                    command: "add_root".into(),
                },
                scope: Scope::All,
                until: Expiry::Session,
                effect: Effect::Allow,
            });
            h.ui.events.try_iter().for_each(drop);
            slow.block();
            let pending = {
                let client = client.peer().clone();
                tokio::spawn(async move { call(&client, "library_folders", add).await })
            };
            slow.wait_until_entered(if shutdown { 2 } else { 1 }).await;
            let stopping = if shutdown {
                let server = h.server.take().unwrap();
                Some(tokio::task::spawn_blocking(move || server.shutdown()))
            } else {
                client.cancel().await.unwrap();
                None
            };
            // The session grant is revoked while the command is still blocked.
            h.wait_ui(|event| matches!(event, Event::PermissionsChanged));
            slow.release();
            if let Some(stopping) = stopping {
                stopping.await.unwrap();
            } else {
                tokio::task::block_in_place(|| h.server.take().unwrap().shutdown());
            }
            pending.abort();
            let QueryResult::Roots { roots } = h.core.query(Query::Roots) else {
                panic!()
            };
            assert!(
                !roots
                    .iter()
                    .any(|root| root.path == extra.display().to_string())
            );
            assert!(
                !h.ui
                    .events
                    .try_iter()
                    .any(|e| matches!(e.event, Event::ConfirmationRequested { .. }))
            );
            if !shutdown {
                h.server = Some(
                    Server::start(
                        h.core.clone(),
                        ServerOptions::default().with_endpoint(h.endpoint.clone()),
                    )
                    .unwrap(),
                );
            }
        }
    })
    .await;
}
