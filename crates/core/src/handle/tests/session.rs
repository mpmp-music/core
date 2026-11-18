//! Deterministic gates for session withdrawal and registration snapshots.
use super::*;
use crate::contract::{ActionId, GrantId, PlayContext, TrackId, TrackInfo};
use crate::{PlayRecord, Undone, permissions};
use crossbeam_channel::{Receiver, Sender, bounded};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const WAIT: Duration = Duration::from_secs(5);

struct Gate {
    armed: AtomicBool,
    entered: Sender<()>,
    release: Receiver<()>,
}

impl Gate {
    fn wait(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.send(()).unwrap();
            self.release.recv_timeout(WAIT).unwrap();
        }
    }
}

struct Database {
    read: Gate,
    write: Gate,
    executed: AtomicUsize,
}

impl crate::catalog::Browse for Database {
    fn tracks(&self, _: &[TrackId]) -> Result<Vec<TrackInfo>, ContractError> {
        Ok(vec![])
    }

    fn uri_of(&self, _: TrackId) -> Result<Option<mpmp_media_io::MediaUri>, ContractError> {
        Ok(None)
    }

    fn context_tracks(&self, _: &PlayContext) -> Result<Vec<TrackId>, ContractError> {
        Ok(vec![])
    }

    fn query(&self, _: &Query) -> QueryResult {
        QueryResult::Unknown
    }
}

impl crate::catalog::Playlists for Database {}

impl crate::catalog::LibraryRoots for Database {}

impl crate::catalog::Actions for Database {
    fn undo_root_command(&self, _: ActionId) -> Result<Option<&'static str>, ContractError> {
        self.read.wait();
        Ok(None)
    }

    fn undo_action(
        &self,
        action: ActionId,
        _: &Origin,
        _: Option<GrantId>,
    ) -> Result<Undone, ContractError> {
        self.write.wait();
        self.executed.fetch_add(1, Ordering::SeqCst);
        Ok(Undone {
            action,
            tracks: vec![],
            playlists: vec![],
            roots: vec![],
            rescan: false,
        })
    }
}

impl crate::catalog::CatalogChecks for Database {}

impl crate::catalog::AgentPermissions for Database {}

impl crate::catalog::MediaResources for Database {}

impl crate::catalog::Loudness for Database {}

impl crate::catalog::TagsAndCovers for Database {}

impl crate::catalog::PlaybackHistory for Database {
    fn record_play(&self, _: PlayRecord) {}
}

fn gate() -> (Gate, Receiver<()>, Sender<()>) {
    let (entered, arrived) = bounded(1);
    let (release, released) = bounded(1);
    (
        Gate {
            armed: AtomicBool::new(false),
            entered,
            release: released,
        },
        arrived,
        release,
    )
}

fn start(database: Arc<Database>) -> CoreHandle {
    CoreHandle::start(
        Box::new(mpmp_engine::NullOutput::new(
            mpmp_engine::output::StreamFormat {
                sample_rate: 44_100,
                channels: 2,
                sample_type: mpmp_engine::output::SampleType::F32,
            },
        )),
        Arc::new(mpmp_media_io::FileResolver),
        database,
        CoreConfig {
            automatic_loudness_scan: Some(false),
            ..Default::default()
        },
    )
    .unwrap()
}

fn ok(response: Response) -> crate::contract::Receipt {
    match response.result {
        CommandResult::Ok(receipt) => receipt,
        other => panic!("{other:?}"),
    }
}

fn rejected(response: Response) {
    let CommandResult::Err { error } = response.result else {
        panic!("{response:?}")
    };
    assert_eq!(error.code, ErrorCode::ConfirmationRejected);
}

fn register(core: &CoreHandle) {
    ok(core.execute(
        Request::new(1, Command::RegisterAgent { name: "reg".into() }),
        Origin::Ui,
    ));
}

fn origin(session: &str) -> Origin {
    Origin::Agent {
        name: "reg".into(),
        session: Some(session.into()),
    }
}

fn undo() -> Request {
    Request {
        request_id: 2,
        idempotency_key: Some("key".into()),
        command: Command::UndoAction {
            action: ActionId(1),
        },
    }
}

fn queued(core: &CoreHandle, count: usize) {
    let deadline = Instant::now() + WAIT;
    while core.inner.tx.len() < count {
        assert!(Instant::now() < deadline, "request was not queued");
        std::thread::yield_now();
    }
}

#[test]
fn session_ending_during_synchronous_policy_read_withdraws_without_caching() {
    let (read, entered, release) = gate();
    let (write, _, _) = gate();
    let database = Arc::new(Database {
        read,
        write,
        executed: AtomicUsize::new(0),
    });
    let core = start(database.clone());
    register(&core);
    database.read.armed.store(true, Ordering::SeqCst);
    let pending = {
        let core = core.clone();
        std::thread::spawn(move || core.execute(undo(), origin("old")))
    };
    entered.recv_timeout(WAIT).unwrap();
    // end_agent_session marks synchronously, then waits for the actor blocked in this read.
    let ending = {
        let core = core.clone();
        std::thread::spawn(move || core.end_agent_session("reg", "old"))
    };
    queued(&core, 1);
    release.send(()).unwrap();
    rejected(pending.join().unwrap());
    ending.join().unwrap();
    assert_eq!(database.executed.load(Ordering::SeqCst), 0);
    ok(core.execute(undo(), origin("new")));
    assert_eq!(database.executed.load(Ordering::SeqCst), 1);
    core.shutdown();
}

#[test]
fn direct_and_json_requests_keep_the_registration_from_before_enqueueing() {
    let (read, _, _) = gate();
    let (write, entered, release) = gate();
    let database = Arc::new(Database {
        read,
        write,
        executed: AtomicUsize::new(0),
    });
    let core = start(database.clone());
    register(&core);
    core.end_agent_session("reg", "ended");
    database.write.armed.store(true, Ordering::SeqCst);
    let blocker = {
        let core = core.clone();
        std::thread::spawn(move || core.execute(undo(), Origin::Ui))
    };
    entered.recv_timeout(WAIT).unwrap();
    let remove = {
        let core = core.clone();
        std::thread::spawn(move || {
            core.execute(
                Request::new(
                    3,
                    Command::RemoveAgent {
                        agent: "reg".into(),
                    },
                ),
                Origin::Ui,
            )
        })
    };
    queued(&core, 1);
    let again = {
        let core = core.clone();
        std::thread::spawn(move || register(&core))
    };
    queued(&core, 2);
    let direct = {
        let core = core.clone();
        std::thread::spawn(move || core.execute(undo(), origin("old")))
    };
    queued(&core, 3);
    let json = {
        let core = core.clone();
        std::thread::spawn(move || {
            core.execute_json(&serde_json::to_string(&undo()).unwrap(), origin("old-json"))
        })
    };
    queued(&core, 4);
    release.send(()).unwrap();
    ok(blocker.join().unwrap());
    ok(remove.join().unwrap());
    again.join().unwrap();
    rejected(direct.join().unwrap());
    rejected(serde_json::from_str(&json.join().unwrap()).unwrap());
    rejected(core.execute(undo(), origin("ended")));
    assert_eq!(database.executed.load(Ordering::SeqCst), 1);
    ok(core.execute(undo(), origin("fresh")));
    assert_eq!(database.executed.load(Ordering::SeqCst), 2);
    core.shutdown();
}

#[test]
fn idempotency_results_belong_to_one_registration() {
    let (read, _, _) = gate();
    let (write, _, _) = gate();
    let database = Arc::new(Database {
        read,
        write,
        executed: AtomicUsize::new(0),
    });
    let core = start(database.clone());
    for _ in 0..2 {
        register(&core);
        ok(core.execute(undo(), origin("session")));
        ok(core.execute(undo(), origin("reconnect")));
        ok(core.execute(
            Request::new(
                3,
                Command::RemoveAgent {
                    agent: "reg".into(),
                },
            ),
            Origin::Ui,
        ));
    }
    assert_eq!(database.executed.load(Ordering::SeqCst), 2);
    core.shutdown();
}

#[test]
fn a_command_already_executing_finishes_and_keeps_its_result() {
    let (read, _, _) = gate();
    let (write, entered, release) = gate();
    let database = Arc::new(Database {
        read,
        write,
        executed: AtomicUsize::new(0),
    });
    let core = start(database.clone());
    register(&core);
    database.write.armed.store(true, Ordering::SeqCst);
    let pending = {
        let core = core.clone();
        std::thread::spawn(move || core.execute(undo(), origin("old")))
    };
    entered.recv_timeout(WAIT).unwrap();
    core.invalidate_agent_session("reg", "old");
    release.send(()).unwrap();
    ok(pending.join().unwrap());
    ok(core.execute(undo(), origin("new")));
    assert_eq!(database.executed.load(Ordering::SeqCst), 1);
    core.shutdown();
}

#[test]
fn a_new_registration_does_not_inherit_cached_denials_or_key_conflicts() {
    let (read, _, _) = gate();
    let (write, _, _) = gate();
    let database = Arc::new(Database {
        read,
        write,
        executed: AtomicUsize::new(0),
    });
    let core = start(database.clone());
    register(&core);
    ok(core.execute(
        Request::new(
            1,
            Command::GrantPermission {
                agent: "reg".into(),
                session: None,
                operation: crate::contract::Operation::Command {
                    command: "undo_action".into(),
                },
                scope: crate::contract::Scope::All,
                until: crate::contract::Expiry::Forever,
                effect: crate::contract::Effect::Forbid,
            },
        ),
        Origin::Ui,
    ));
    let CommandResult::Err { error } = core.execute(undo(), origin("old")).result else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    ok(core.execute(
        Request::new(
            3,
            Command::RemoveAgent {
                agent: "reg".into(),
            },
        ),
        Origin::Ui,
    ));
    register(&core);
    ok(core.execute(undo(), origin("new")));
    assert_eq!(database.executed.load(Ordering::SeqCst), 1);
    // Changing the command with a key used by the old instance is also allowed.
    ok(core.execute(
        Request::new(
            3,
            Command::RemoveAgent {
                agent: "reg".into(),
            },
        ),
        Origin::Ui,
    ));
    register(&core);
    let request = Request {
        command: Command::Pause,
        ..undo()
    };
    ok(core.execute(request, origin("newest")));
    core.shutdown();
}

#[test]
fn queued_session_remains_withdrawn_after_ended_fifo_eviction() {
    let (read, _, _) = gate();
    let (write, entered, release) = gate();
    let database = Arc::new(Database {
        read,
        write,
        executed: AtomicUsize::new(0),
    });
    let core = start(database.clone());
    register(&core);
    database.write.armed.store(true, Ordering::SeqCst);
    let blocker = {
        let core = core.clone();
        std::thread::spawn(move || core.execute(undo(), Origin::Ui))
    };
    entered.recv_timeout(WAIT).unwrap();
    let pending = {
        let core = core.clone();
        std::thread::spawn(move || core.execute(undo(), origin("old")))
    };
    queued(&core, 1);
    core.invalidate_agent_session("reg", "old");
    for i in 0..permissions::MAX_ENDED_SESSIONS {
        core.invalidate_agent_session("reg", &format!("ended-{i}"));
    }
    release.send(()).unwrap();
    ok(blocker.join().unwrap());
    rejected(pending.join().unwrap());
    assert_eq!(database.executed.load(Ordering::SeqCst), 1);
    ok(core.execute(undo(), origin("fresh")));
    assert_eq!(database.executed.load(Ordering::SeqCst), 2);
    core.shutdown();
}
