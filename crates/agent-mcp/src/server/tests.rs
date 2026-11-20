use mpmp_core::contract::{Command, CommandResult, Query, QueryResult, Request};
use mpmp_core::{Catalog, CoreConfig};

use super::*;
use crate::confirm::{Status, unix_ms};

/// A core with an empty library, and agent `claude`'s token.
fn core() -> (mpmp_test_support::TempDir, CoreHandle, String) {
    core_with_config(CoreConfig::default())
}

fn core_with_config(config: CoreConfig) -> (mpmp_test_support::TempDir, CoreHandle, String) {
    use mpmp_engine::output::{SampleType, StreamFormat};
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let library = mpmp_library::Library::open(&dir.path().join("library.db")).unwrap();
    let output = mpmp_engine::NullOutput::new(StreamFormat {
        sample_rate: 44_100,
        channels: 2,
        sample_type: SampleType::F32,
    });
    let core = CoreHandle::start(
        Box::new(output),
        Arc::new(mpmp_media_io::FileResolver),
        Arc::new(library) as Arc<dyn Catalog>,
        config,
    )
    .unwrap();
    let register = Command::RegisterAgent {
        name: "claude".into(),
    };
    let CommandResult::Ok(receipt) = core.execute(Request::new(1, register), Origin::Ui).result
    else {
        panic!("cannot register")
    };
    (dir, core, receipt.agent_token.unwrap())
}

/// Lost events (`Resync`) may have hidden an `AgentsChanged`: every connection is
/// checked again (closed when its token no longer works), and confirmations still
/// waited for are marked, so one past its time is `unknown`.
#[tokio::test]
async fn a_resync_checks_connections_and_marks_confirmations() {
    let (_dir, core, token) = core();
    let shared = Shared {
        core: core.clone(),
        options: ServerOptions::default(),
        confirmations: Arc::new(Confirmations::new()),
        connections: Mutex::new(HashMap::new()),
        next_connection: AtomicU64::new(1),
        active: AtomicUsize::new(0),
        shutdown: watch::channel(false).0,
        ending: Mutex::new(Vec::new()),
    };
    let owner = |session: &str| Owner {
        agent: "claude".into(),
        session: session.into(),
    };
    let agent = core.authenticate_agent_connection(&token).unwrap();
    let mut closed = Vec::new();
    for (id, token) in [(1, token.as_str()), (2, "pmpa_renewed_meanwhile")] {
        let (close, watch) = watch::channel(false);
        closed.push(watch);
        let connection = Connection {
            agent: agent.clone(),
            token: Arc::from(token),
            session: format!("s{id}"),
            close,
        };
        shared.connections().insert(id, connection);
    }
    let s1 = owner("s1");
    shared.confirmations.claim("t", &s1, unix_ms() - 10);
    let status = || {
        shared
            .confirmations
            .wait("t", &s1, Duration::ZERO, std::future::pending())
    };
    assert_eq!(status().await, Status::Pending);

    let QueryResult::Snapshot { snapshot } = core.query(Query::Snapshot) else {
        panic!("no snapshot")
    };
    shared.on_event(Event::Resync { snapshot });
    assert!(!*closed[0].borrow(), "a working token stays");
    assert!(
        *closed[1].borrow(),
        "a token that no longer works is closed"
    );
    assert_eq!(status().await, Status::Unknown);
    core.shutdown();
}

/// A call past the MCP gate still carries the registration authenticated before removal.
#[tokio::test]
async fn a_call_past_the_gate_cannot_borrow_a_new_registration() {
    let (_dir, core, token) = core();
    let agent = core.authenticate_agent_connection(&token).unwrap();
    let gate = Arc::new(Gate::new(true));
    let open = gate.read_owned().await;
    for again in [false, true] {
        if !again {
            let result = core
                .execute(
                    Request::new(
                        2,
                        Command::RemoveAgent {
                            agent: "claude".into(),
                        },
                    ),
                    Origin::Ui,
                )
                .result;
            assert!(matches!(result, CommandResult::Ok(_)), "{result:?}");
        } else {
            let result = core
                .execute(
                    Request::new(
                        3,
                        Command::RegisterAgent {
                            name: "claude".into(),
                        },
                    ),
                    Origin::Ui,
                )
                .result;
            assert!(matches!(result, CommandResult::Ok(_)), "{result:?}");
        }
        let result = agent
            .execute(Request::new(4, Command::SetMuted { muted: true }), "old")
            .result;
        let CommandResult::Err { error } = result else {
            panic!("old connection executed: {result:?}")
        };
        assert_eq!(
            error.code,
            mpmp_core::contract::ErrorCode::ConfirmationRejected
        );
        let QueryResult::Snapshot { snapshot } = core.query(Query::Snapshot) else {
            panic!()
        };
        assert!(!snapshot.audio.muted);
    }
    drop(open);
    core.shutdown();
}

/// Cleanup must notify core before it waits for an in-flight call holding the gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_cleanup_withdraws_before_waiting_for_the_gate() {
    use mpmp_core::contract::{Effect, ErrorCode, Expiry, Operation, Scope};
    struct Blocked {
        entered: std::sync::mpsc::Sender<()>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl mpmp_core::Locations for Blocked {
        fn normalize(&self, location: &str) -> Option<String> {
            self.entered.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            mpmp_fs_desktop::DesktopLocations.normalize(location)
        }
        fn contains(&self, scope: &str, location: &str, doubt: mpmp_core::Doubt) -> bool {
            mpmp_fs_desktop::DesktopLocations.contains(
                scope,
                location,
                doubt == mpmp_core::Doubt::Include,
            )
        }
        fn child(&self, location: &str, relative: &str) -> Option<String> {
            mpmp_fs_desktop::DesktopLocations.child(location, relative)
        }
        fn ignores_case(&self, location: &str) -> Option<bool> {
            mpmp_fs_desktop::DesktopLocations.ignores_case(location)
        }
        fn display(&self, location: &str) -> String {
            mpmp_fs_desktop::DesktopLocations.display(location)
        }
    }
    for abort in [false, true] {
        let (entered, arrived) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (dir, core, token) = core_with_config(CoreConfig {
            locations: Some(Arc::new(Blocked {
                entered,
                release: Mutex::new(released),
            })),
            automatic_loudness_scan: Some(false),
            ..Default::default()
        });
        let sub = core
            .subscribe(Origin::Ui, SubscribeOptions::default())
            .unwrap();
        let shared = Arc::new(Shared {
            core: core.clone(),
            options: ServerOptions::default(),
            confirmations: Arc::new(Confirmations::new()),
            connections: Mutex::new(HashMap::new()),
            next_connection: AtomicU64::new(1),
            active: AtomicUsize::new(0),
            shutdown: watch::channel(false).0,
            ending: Mutex::new(Vec::new()),
        });
        let owner = Owner {
            agent: "claude".into(),
            session: "disconnect".into(),
        };
        let grant = core
            .execute(
                Request::new(
                    1,
                    Command::GrantPermission {
                        agent: owner.agent.clone(),
                        session: Some(owner.session.clone()),
                        operation: Operation::Command {
                            command: "add_root".into(),
                        },
                        scope: Scope::All,
                        until: Expiry::Session,
                        effect: Effect::Allow,
                    },
                ),
                Origin::Ui,
            )
            .result;
        assert!(matches!(grant, CommandResult::Ok(_)), "{grant:?}");
        sub.events.try_iter().for_each(drop);
        let agent = core.authenticate_agent_connection(&token).unwrap();
        let (close, closed) = watch::channel(false);
        shared.connections().insert(
            1,
            Connection {
                agent: agent.clone(),
                token: Arc::from(token),
                session: owner.session.clone(),
                close,
            },
        );
        let gate = Arc::new(Gate::new(true));
        let session = Session::new(
            core.clone(),
            agent,
            owner.clone(),
            Arc::clone(&shared.confirmations),
            MAX_WAIT,
            closed,
            Arc::clone(&gate),
        );
        let command = Command::AddRoot {
            path: dir.path().display().to_string(),
        };
        let pending = tokio::spawn(async move { session.run(command).await });
        tokio::task::block_in_place(|| arrived.recv_timeout(Duration::from_secs(5)).unwrap());
        let mut guard = SessionGuard {
            shared: Arc::clone(&shared),
            id: 1,
            owner,
            gate,
            ended: false,
        };
        let ending = if abort {
            drop(guard);
            None
        } else {
            Some(tokio::spawn(async move { guard.end().await }))
        };
        tokio::task::block_in_place(|| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let event = sub
                    .events
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    .unwrap();
                if matches!(event.event, Event::PermissionsChanged) {
                    break;
                }
                assert!(
                    matches!(
                        event.event,
                        Event::OutputDevicesChanged { .. }
                            | Event::OutputChanged { .. }
                            | Event::StateChanged { .. }
                    ),
                    "{event:?}"
                );
            }
        });
        release.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap()
            .unwrap();
        let CommandResult::Err { error } = result else {
            panic!("disconnected command executed: {result:?}")
        };
        assert_eq!(error.code, ErrorCode::ConfirmationRejected);
        if let Some(ending) = ending {
            tokio::time::timeout(Duration::from_secs(5), ending)
                .await
                .unwrap()
                .unwrap();
        }
        let threads = std::mem::take(&mut *shared.ending());
        tokio::task::block_in_place(|| {
            for thread in threads {
                thread.join().unwrap();
            }
        });
        assert!(
            !sub.events
                .try_iter()
                .any(|e| matches!(e.event, Event::ConfirmationRequested { .. }))
        );
        let QueryResult::Roots { roots } = core.query(Query::Roots) else {
            panic!()
        };
        assert!(roots.is_empty());
        core.shutdown();
    }
}

/// The command has the gate, but its closure cannot enter core until the pool frees.
#[test]
fn saturated_blocking_pool_cannot_delay_session_invalidation() {
    for ending_kind in 0..3 {
        for json in [false, true] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(1)
                .build()
                .unwrap();
            runtime.block_on(async {
                let (_dir, core, token) = core();
                let agent = core.authenticate_agent_connection(&token).unwrap();
                let owner = Owner {
                    agent: "claude".into(),
                    session: "queued".into(),
                };
                let (close, mut closed) = watch::channel(false);
                let shared = Arc::new(Shared {
                    core: core.clone(),
                    options: ServerOptions::default(),
                    confirmations: Arc::new(Confirmations::new()),
                    connections: Mutex::new(HashMap::new()),
                    next_connection: AtomicU64::new(2),
                    active: AtomicUsize::new(0),
                    shutdown: watch::channel(false).0,
                    ending: Mutex::new(Vec::new()),
                });
                shared.connections().insert(
                    1,
                    Connection {
                        agent: agent.clone(),
                        token: Arc::from(token),
                        session: owner.session.clone(),
                        close,
                    },
                );
                let gate = Arc::new(Gate::new(true));
                let session = Session::new(
                    core.clone(),
                    agent,
                    owner.clone(),
                    Arc::clone(&shared.confirmations),
                    MAX_WAIT,
                    closed.clone(),
                    Arc::clone(&gate),
                );
                let (entered, arrived) = std::sync::mpsc::channel();
                let (release, released) = std::sync::mpsc::channel();
                let blocker = tokio::task::spawn_blocking(move || {
                    entered.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(5)).unwrap();
                });
                arrived.recv_timeout(Duration::from_secs(5)).unwrap();
                let pending = tokio::spawn(async move {
                    let command = Command::SetMuted { muted: true };
                    if json {
                        session
                            .run_json(serde_json::to_string(&Request::new(1, command)).unwrap())
                            .await
                    } else {
                        session.run(command).await
                    }
                });
                tokio::time::timeout(Duration::from_secs(5), async {
                    while gate.try_write().is_ok() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                let guard = SessionGuard {
                    shared: Arc::clone(&shared),
                    id: 1,
                    owner,
                    gate,
                    ended: false,
                };
                let mut guard = Some(guard);
                let ending = match ending_kind {
                    1 => {
                        stop_thread(&shared, None);
                        None
                    }
                    2 => {
                        drop(guard.take());
                        None
                    }
                    _ => {
                        let mut guard = guard.take().unwrap();
                        let ending = tokio::spawn(async move { guard.end().await });
                        tokio::time::timeout(Duration::from_secs(5), closed.changed())
                            .await
                            .unwrap()
                            .unwrap();
                        Some(ending)
                    }
                };
                // No asynchronous cleanup is needed for this marker to be visible.
                let probe = core.execute(
                    Request::new(2, Command::SetMuted { muted: true }),
                    Origin::Agent {
                        name: "claude".into(),
                        session: Some("queued".into()),
                    },
                );
                let CommandResult::Err { error } = probe.result else {
                    panic!("session is still valid")
                };
                assert_eq!(
                    error.code,
                    mpmp_core::contract::ErrorCode::ConfirmationRejected
                );
                release.send(()).unwrap();
                blocker.await.unwrap();
                let result = tokio::time::timeout(Duration::from_secs(5), pending)
                    .await
                    .unwrap()
                    .unwrap();
                let CommandResult::Err { error } = result else {
                    panic!("queued call executed: {result:?}")
                };
                assert_eq!(
                    error.code,
                    mpmp_core::contract::ErrorCode::ConfirmationRejected
                );
                if let Some(ending) = ending {
                    tokio::time::timeout(Duration::from_secs(5), ending)
                        .await
                        .unwrap()
                        .unwrap();
                }
                if let Some(mut guard) = guard {
                    tokio::time::timeout(Duration::from_secs(5), guard.end())
                        .await
                        .unwrap();
                }
                for thread in std::mem::take(&mut *shared.ending()) {
                    thread.join().unwrap();
                }
                let QueryResult::Snapshot { snapshot } = core.query(Query::Snapshot) else {
                    panic!()
                };
                assert!(!snapshot.audio.muted);
                core.shutdown();
            });
        }
    }
}

#[test]
fn sessions_are_random_uuids() {
    let a = super::new_session_id().unwrap();
    let b = super::new_session_id().unwrap();
    assert_ne!(a, b);
    assert_eq!(a.len(), 36);
    assert_eq!(&a[14..15], "4");
    assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
}
