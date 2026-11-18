//! Job queries recover authoritative state even without consuming completion events.
use mpmp_core::{
    CoreConfig, CoreHandle, PlayRecord, ScanEvent, ScanTicket, SubscribeOptions, contract::*,
};
use mpmp_engine::{
    NullOutput,
    output::{SampleType, StreamFormat},
};
use mpmp_media_io::{FileResolver, MediaUri};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type ScanSink = Box<dyn Fn(ScanEvent) + Send + Sync>;

#[derive(Default)]
struct ControlledScan(Mutex<Option<ScanSink>>);

impl mpmp_core::catalog::Browse for ControlledScan {
    fn tracks(&self, _: &[TrackId]) -> Result<Vec<TrackInfo>, ContractError> {
        Ok(vec![])
    }

    fn uri_of(&self, _: TrackId) -> Result<Option<MediaUri>, ContractError> {
        Ok(None)
    }

    fn context_tracks(&self, _: &PlayContext) -> Result<Vec<TrackId>, ContractError> {
        Ok(vec![])
    }

    fn query(&self, _: &Query) -> QueryResult {
        QueryResult::Unknown
    }
}

impl mpmp_core::catalog::Playlists for ControlledScan {}

impl mpmp_core::catalog::LibraryRoots for ControlledScan {
    fn rescan_directories(
        &self,
        _: Vec<std::path::PathBuf>,
        events: ScanSink,
    ) -> Result<ScanTicket, ContractError> {
        self.rescan(events)
    }

    fn rescan(&self, events: ScanSink) -> Result<ScanTicket, ContractError> {
        let mut sink = self.0.lock().unwrap();
        let joined = sink.is_some();
        if !joined {
            *sink = Some(events);
        }
        Ok(ScanTicket { scan: 1, joined })
    }
}

impl mpmp_core::catalog::Actions for ControlledScan {}

impl mpmp_core::catalog::CatalogChecks for ControlledScan {}

impl mpmp_core::catalog::AgentPermissions for ControlledScan {}

impl mpmp_core::catalog::MediaResources for ControlledScan {}

impl mpmp_core::catalog::Loudness for ControlledScan {}

impl mpmp_core::catalog::TagsAndCovers for ControlledScan {}

impl mpmp_core::catalog::PlaybackHistory for ControlledScan {
    fn record_play(&self, _: PlayRecord) {}
}

fn core(catalog: Arc<ControlledScan>) -> CoreHandle {
    CoreHandle::start(
        Box::new(NullOutput::new(StreamFormat {
            sample_rate: 44100,
            channels: 2,
            sample_type: SampleType::F32,
        })),
        Arc::new(FileResolver),
        catalog,
        CoreConfig {
            automatic_loudness_scan: Some(false),
            ..Default::default()
        },
    )
    .unwrap()
}

fn scan(core: &CoreHandle) -> JobId {
    let response = core.execute(Request::new(1, Command::Rescan), Origin::Ui);
    let CommandResult::Ok(receipt) = response.result else {
        panic!("{response:?}")
    };
    receipt.job.unwrap()
}

#[test]
fn job_queries_are_visible_before_receipt_and_events_and_reset_on_restart() {
    let catalog = Arc::new(ControlledScan::default());
    let core = core(catalog.clone());
    let sub = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    let id = scan(&core);
    assert_eq!(scan(&core), id, "joined scans share authoritative state");
    assert!(matches!(
        core.query(Query::Job { job: id }),
        QueryResult::Job { job }
            if job.state == JobState::Running
                && job.progress.is_none()
                && job.outcome.is_none(),
    ));
    let outcome = JobOutcome::Completed {
        report: JobReport::Scan {
            added_count: 2,
            updated_count: 0,
            restored_count: 0,
            removed_count: 0,
            error_count: 1,
            first_errors: vec![ScanErrorInfo {
                path: "/music/a.flac".into(),
                message: "cannot read".into(),
                code: Some(ScanErrorCode::AudioReadFailed),
            }],
        },
    };
    let sink = catalog.0.lock().unwrap().take().unwrap();
    sink(ScanEvent::Progress {
        scan: 1,
        done: 2,
        total: Some(3),
    });
    sink(ScanEvent::Finished {
        scan: 1,
        outcome: outcome.clone(),
    });
    loop {
        let event = sub
            .events
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .event;
        if matches!(event, Event::JobFinished { .. }) {
            break;
        }
    }
    assert!(matches!(
        core.query(Query::Job { job: id }),
        QueryResult::Job { job }
            if job.state == JobState::Completed
                && job.outcome == Some(outcome)
                && job.progress
                    == Some(JobProgressInfo {
                        done: 3,
                        total: Some(3),
                    }),
    ));
    assert!(matches!(
        core.query(Query::Jobs {
            offset: 0,
            limit: 0,
        }),
        QueryResult::Jobs {
            jobs,
            total: 1,
            next_offset: None,
            ..
        } if jobs.is_empty(),
    ));
    assert!(matches!(
        core.query(Query::Jobs {
            offset: 1,
            limit: 1,
        }),
        QueryResult::Jobs {
            jobs,
            total: 1,
            next_offset: None,
            ..
        } if jobs.is_empty(),
    ));
    assert!(matches!(
        core.query(Query::Job { job: JobId(0) }),
        QueryResult::Err { error }
            if error.kind == ErrorKind::NotFound && error.code == ErrorCode::JobNotFound,
    ));
    let parsed: Query = serde_json::from_str(r#"{"type":"jobs","limit":1}"#).unwrap();
    assert!(
        matches!(core.query(parsed),QueryResult::Jobs {jobs,total:1,offset:0,..} if jobs.len()==1)
    );
    assert!(
        core.query_json(r#"{"type":"job","job":0,"unexpected":true}"#)
            .contains("unknown_field")
    );
    core.shutdown();
    let restarted = self::core(catalog);
    assert!(matches!(
        restarted.query(Query::Jobs {
            offset: 0,
            limit: 5000,
        }),
        QueryResult::Jobs { jobs, total: 0, .. } if jobs.is_empty(),
    ));
    assert!(matches!(
        restarted.query(Query::Job { job: id }),
        QueryResult::Err { error } if error.code == ErrorCode::JobNotFound,
    ));
    restarted.shutdown();
}

#[test]
fn explicit_rescan_promotes_background_retry_and_preserves_its_completion() {
    let catalog = Arc::new(ControlledScan::default());
    let core = core(catalog.clone());
    let sub = core
        .subscribe(
            Origin::Ui,
            SubscribeOptions::default().with_positions(false),
        )
        .unwrap();
    let previous = JobOutcome::Completed {
        report: JobReport::Scan {
            added_count: 0,
            updated_count: 0,
            restored_count: 0,
            removed_count: 0,
            error_count: 1,
            first_errors: vec![ScanErrorInfo {
                path: "/music/broken.flac".into(),
                message: "broken".into(),
                code: Some(ScanErrorCode::AudioReadFailed),
            }],
        },
    };
    let (retry_job, completed) = core
        .retry_scan_directories(vec!["/music".into()], previous.clone().into())
        .unwrap();
    assert!(matches!(
        core.query(Query::Jobs {
            offset: 0,
            limit: 10
        }),
        QueryResult::Jobs { total: 0, .. }
    ));
    let id = scan(&core);
    assert_eq!(retry_job, id);
    assert!(matches!(
        core.query(Query::Job { job: id }),
        QueryResult::Job { job } if job.state == JobState::Running,
    ));
    let sink = catalog.0.lock().unwrap().take().unwrap();
    sink(ScanEvent::Progress {
        scan: 1,
        done: 1,
        total: Some(1),
    });
    sink(ScanEvent::Finished {
        scan: 1,
        outcome: previous.clone(),
    });
    assert_eq!(
        completed
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .outcome,
        previous
    );
    loop {
        if let Event::JobFinished { job, .. } = sub
            .events
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .event
        {
            assert_eq!(job, id);
            break;
        }
    }
    assert!(matches!(
        core.query(Query::Job { job: id }),
        QueryResult::Job { job }
            if job.state == JobState::Completed
                && job.progress
                    == Some(JobProgressInfo {
                        done: 1,
                        total: Some(1),
                    }),
    ));
    assert!(core.shutdown_and_wait(Duration::from_secs(5)));
}

#[test]
fn explicit_rescan_promotes_background_periodic_verification_and_preserves_its_completion() {
    let catalog = Arc::new(ControlledScan::default());
    let core = core(catalog.clone());
    let sub = core
        .subscribe(
            Origin::Ui,
            SubscribeOptions::default().with_positions(false),
        )
        .unwrap();
    let previous = JobOutcome::Completed {
        report: JobReport::Scan {
            added_count: 0,
            updated_count: 0,
            restored_count: 0,
            removed_count: 0,
            error_count: 1,
            first_errors: vec![ScanErrorInfo {
                path: "/music/broken.flac".into(),
                message: "broken".into(),
                code: Some(ScanErrorCode::AudioReadFailed),
            }],
        },
    };
    let (retry_job, completed) = core.verify_library(Some(previous.clone().into())).unwrap();
    assert!(matches!(
        core.query(Query::Jobs {
            offset: 0,
            limit: 10
        }),
        QueryResult::Jobs { total: 0, .. }
    ));
    let id = scan(&core);
    assert_eq!(retry_job, id);
    assert!(matches!(
        core.query(Query::Job { job: id }),
        QueryResult::Job { job } if job.state == JobState::Running,
    ));
    let sink = catalog.0.lock().unwrap().take().unwrap();
    sink(ScanEvent::Progress {
        scan: 1,
        done: 1,
        total: Some(1),
    });
    sink(ScanEvent::Finished {
        scan: 1,
        outcome: previous.clone(),
    });
    assert_eq!(
        completed
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .outcome,
        previous
    );
    loop {
        if let Event::JobFinished { job, .. } = sub
            .events
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .event
        {
            assert_eq!(job, id);
            break;
        }
    }
    assert!(matches!(
        core.query(Query::Job { job: id }),
        QueryResult::Job { job }
            if job.state == JobState::Completed
                && job.progress
                    == Some(JobProgressInfo {
                        done: 1,
                        total: Some(1),
                    }),
    ));
    assert!(core.shutdown_and_wait(Duration::from_secs(5)));
}

#[test]
fn oversized_scan_diagnostics_preserve_public_completion_without_retaining_facts() {
    let catalog = Arc::new(ControlledScan::default());
    let core = core(catalog.clone());
    let sub = core
        .subscribe(
            Origin::Ui,
            SubscribeOptions::default().with_positions(false),
        )
        .unwrap();
    let id = scan(&core);
    let mut diagnostics = mpmp_library::ScanDiagnostics::default();
    diagnostics
        .coverage
        .insert("x".repeat(9 * 1024 * 1024).into());
    let sink = catalog.0.lock().unwrap().take().unwrap();
    sink(ScanEvent::Diagnostics {
        scan: 1,
        diagnostics: Arc::new(diagnostics),
    });
    sink(ScanEvent::Finished {
        scan: 1,
        outcome: JobOutcome::Completed {
            report: JobReport::Scan {
                added_count: 0,
                updated_count: 0,
                restored_count: 0,
                removed_count: 0,
                error_count: 0,
                first_errors: vec![],
            },
        },
    });
    loop {
        if let Event::JobFinished { job, .. } = sub
            .events
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .event
        {
            assert_eq!(job, id);
            break;
        }
    }
    assert!(matches!(
        core.query(Query::Job { job: id }),
        QueryResult::Job { job }
            if job.state == JobState::Completed
                && matches!(job.outcome, Some(JobOutcome::Completed { .. })),
    ));
    assert!(core.scan_diagnostics(id).is_none());
    assert!(core.shutdown_and_wait(Duration::from_secs(5)));
}
