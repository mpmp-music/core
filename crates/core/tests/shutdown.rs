//! Shutdown waits for jobs known to the actor even before their first progress event.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mpmp_core::catalog::{TagFiles, TagJobEvent};
use mpmp_core::contract::*;
use mpmp_core::{CoreConfig, CoreHandle, PlayRecord, ScanEvent, ScanTicket, ShutdownError};
use mpmp_engine::NullOutput;
use mpmp_engine::output::{SampleType, StreamFormat};
use mpmp_media_io::{FileResolver, MediaUri};

const WAIT: Duration = Duration::from_secs(10);

type TagEvents = Box<dyn Fn(TagJobEvent) + Send + Sync>;

type ScanEvents = Box<dyn Fn(ScanEvent) + Send + Sync>;

#[derive(Default)]
struct Jobs {
    events: Mutex<Option<TagEvents>>,
    scan: Mutex<Option<ScanEvents>>,
}

impl mpmp_core::catalog::Browse for Jobs {
    fn tracks(&self, ids: &[TrackId]) -> Result<Vec<TrackInfo>, ContractError> {
        Ok(ids
            .iter()
            .map(|id| TrackInfo {
                id: *id,
                ..Default::default()
            })
            .collect())
    }

    fn uri_of(&self, _: TrackId) -> Result<Option<MediaUri>, ContractError> {
        Ok(None)
    }

    fn context_tracks(&self, _: &PlayContext) -> Result<Vec<TrackId>, ContractError> {
        Ok(vec![])
    }

    fn query(&self, _: &Query) -> QueryResult {
        QueryResult::Roots { roots: vec![] }
    }
}

impl mpmp_core::catalog::Playlists for Jobs {}

impl mpmp_core::catalog::LibraryRoots for Jobs {
    fn rescan(&self, events: ScanEvents) -> Result<ScanTicket, ContractError> {
        *self.scan.lock().unwrap() = Some(events);
        Ok(ScanTicket {
            scan: 1,
            joined: false,
        })
    }
}

impl mpmp_core::catalog::Actions for Jobs {}

impl mpmp_core::catalog::CatalogChecks for Jobs {}

impl mpmp_core::catalog::AgentPermissions for Jobs {}

impl mpmp_core::catalog::MediaResources for Jobs {}

impl mpmp_core::catalog::Loudness for Jobs {}

impl mpmp_core::catalog::TagsAndCovers for Jobs {
    fn edit_tags(
        &self,
        _: &[TrackId],
        _: &[TagChange],
        _: TagFiles,
        _: &Origin,
        _: Option<GrantId>,
        events: TagEvents,
    ) -> Result<(), ContractError> {
        *self.events.lock().unwrap() = Some(events);
        Ok(())
    }
}

impl mpmp_core::catalog::PlaybackHistory for Jobs {
    fn record_play(&self, _: PlayRecord) {}
}

fn core(catalog: Arc<impl mpmp_core::Catalog>, state: std::path::PathBuf) -> CoreHandle {
    CoreHandle::start(
        Box::new(NullOutput::new(StreamFormat {
            sample_rate: 44_100,
            channels: 2,
            sample_type: SampleType::F32,
        })),
        Arc::new(FileResolver),
        catalog,
        CoreConfig {
            state_file: Some(state),
            automatic_loudness_scan: Some(false),
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn shutdown_closes_the_library_while_core_and_library_handles_remain_live() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("library.db");
    let library = Arc::new(mpmp_library::Library::open(&db).unwrap());
    let core = core(library.clone(), dir.path().join("state.json"));
    library.roots().unwrap();
    library
        .add_agent(mpmp_library::AgentRecord {
            name: "close-test".into(),
            token_hash: vec![1],
            created_at: 100,
            last_seen_at: None,
        })
        .unwrap();
    assert!(
        std::fs::metadata(db.with_file_name("library.db-wal"))
            .unwrap()
            .len()
            > 0
    );
    assert!(db.with_file_name("library.db-shm").exists());
    core.shutdown_with_result(WAIT).unwrap();
    core.shutdown_with_result(Duration::ZERO).unwrap();
    assert!(matches!(
        library.roots(),
        Err(mpmp_library::LibraryError::Closed)
    ));
    assert!(matches!(
        library.flush(),
        Err(mpmp_library::LibraryError::Closed)
    ));
    assert!(!db.with_file_name("library.db-wal").exists());
    assert!(!db.with_file_name("library.db-shm").exists());
    assert_eq!(
        mpmp_core::catalog::LibraryRoots::rescan_recommended(library.as_ref())
            .unwrap_err()
            .code,
        ErrorCode::WriterStopped
    );
}

#[test]
fn shutdown_keeps_a_failed_write_distinct_from_a_finished_checkpoint_and_close() {
    use mpmp_library::CloseStage;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("library.db");
    let library = Arc::new(mpmp_library::Library::open(&db).unwrap());
    let core = core(library.clone(), dir.path().join("state.json"));
    let fixture = rusqlite::Connection::open(&db).unwrap();
    fixture
        .execute_batch(
            "CREATE TRIGGER reject_test_play BEFORE INSERT ON play_events BEGIN
           SELECT RAISE(ABORT, 'injected playback write failure');
         END;",
        )
        .unwrap();
    fixture.close().unwrap();
    library
        .record_play(mpmp_library::PlayEvent {
            track_id: mpmp_library::TrackId(1),
            context: None,
            started_at: 100,
            played_ms: 10,
            ended_reason: mpmp_library::EndedReason::Completed,
            origin: "ui".into(),
        })
        .unwrap();
    let Err(ShutdownError::Library(error)) = core.shutdown_with_result(WAIT) else {
        panic!("core should report the rejected write");
    };
    let progress = error.progress.unwrap();
    assert!(matches!(&progress.writes, CloseStage::Failed(reason) if reason.contains("injected")));
    assert_eq!(progress.checkpoint, CloseStage::Succeeded);
    assert_eq!(progress.read_connections, CloseStage::Succeeded);
    assert_eq!(progress.write_connection, CloseStage::Succeeded);
    assert!(!db.with_file_name("library.db-wal").exists());
    assert!(!db.with_file_name("library.db-shm").exists());
}

#[test]
fn shutdown_waits_for_a_job_without_progress_then_saves_and_rejects_commands() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    let catalog = Arc::new(Jobs::default());
    let core = core(Arc::clone(&catalog), state.clone());
    assert!(matches!(
        core.execute(
            Request::new(0, Command::SetVolume { volume: 0.37 }),
            Origin::Ui
        )
        .result,
        CommandResult::Ok(_)
    ));
    assert!(matches!(
        core.execute(Request::new(0, Command::Rescan), Origin::Ui)
            .result,
        CommandResult::Ok(_)
    ));
    let response = core.execute(
        Request::new(
            1,
            Command::EditTags {
                tracks: vec![TrackId(1)],
                changes: vec![TagChange::Set {
                    key: "TITLE".into(),
                    values: vec!["new".into()],
                }],
            },
        ),
        Origin::Ui,
    );
    assert!(matches!(
        response.result,
        CommandResult::Ok(Receipt { job: Some(_), .. })
    ));
    assert_eq!(
        core.shutdown_with_result(Duration::from_millis(30)),
        Err(ShutdownError::TimedOut)
    );
    assert!(matches!(
        core.execute(Request::new(2, Command::Rescan), Origin::Ui)
            .result,
        CommandResult::Err {
            error: ContractError {
                code: ErrorCode::CoreStopped,
                ..
            }
        }
    ));
    let events = catalog.events.lock().unwrap().take().unwrap();
    events(TagJobEvent::Finished {
        outcome: JobOutcome::Failed {
            error: ContractError::failed(ErrorCode::Io, "test job ended"),
        },
        tracks: vec![],
    });
    // The tag job finished, but a catalog scan still owns work.
    assert!(!core.shutdown_and_wait(Duration::from_millis(30)));
    catalog.scan.lock().unwrap().take().unwrap()(ScanEvent::Finished {
        scan: 1,
        outcome: JobOutcome::Failed {
            error: ContractError::failed(ErrorCode::Io, "test scan ended"),
        },
    });
    let other = core.clone();
    let waiter = std::thread::spawn(move || other.shutdown_and_wait(WAIT));
    assert!(core.shutdown_and_wait(WAIT));
    assert!(waiter.join().unwrap());
    assert!(state.exists());
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(state).unwrap()).unwrap();
    assert!((saved["audio"]["volume"].as_f64().unwrap() - 0.37).abs() < 0.00001);
    assert!(core.shutdown_and_wait(Duration::ZERO));
}

#[test]
fn shutdown_without_jobs_is_waitable_and_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    let core = core(Arc::new(Jobs::default()), state.clone());
    core.shutdown();
    assert!(core.shutdown_and_wait(WAIT));
    assert!(state.exists());
    assert!(core.shutdown_and_wait(Duration::ZERO));
}
