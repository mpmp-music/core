//! The core actor: one thread owns the queue, the play state and
//! the engine, handles commands in order and fans events out to subscribers. Queue and
//! state queries read the snapshot it publishes; library queries never reach it.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, TrySendError, select, tick};
use mpmp_engine::output::policy;
use mpmp_engine::{BitPerfect, Engine, EngineError, EngineEvent, SignalPath, TrackRef};

use crate::SubscribeOptions;
use crate::catalog::{Catalog, PlayRecord, Recorded, ScanEvent, ScanResult, TagFiles, TagJobEvent};
use crate::contract::{
    ActionId, AudioSettings, BitPerfectInfo, COMMANDS, CONTRACT_MINOR, CONTRACT_VERSION, Command,
    CommandResult, ConfirmationOutcome, ConfirmationReason, ConfirmationTicket, ContractError,
    Effect, EndReason, EntryId, ErrorCode, ErrorKind, Event, EventEnvelope, Expiry, Grant, GrantId,
    JobId, JobKind, JobOutcome, JobReport, MAX_SAFE_INTEGER, MAX_SLEEP_TIMER_MS,
    MAX_TAG_EDIT_TRACKS, NowPlaying, ObjectRef, Operation, OperationClass, OperationInfo, Origin,
    PendingConfirmation, PlayContext, PlayState, PlaybackLimitAction, PlaybackLimitReason,
    PlaybackLimitsInfo, Policy, PositionInfo, Problem, QueueSummary, Receipt, RepeatMode,
    ReplayGainMode, ReplayGainSettings, Request, Response, RootId, SampleFormat, Scope,
    SignalFormat, SignalPathInfo, SignalReasonCode, SignalStageKind, SleepTimerInfo, Snapshot,
    StageInfo, StopAfterCurrentInfo, TrackId, TrackInfo, VolumeMode,
};
use crate::gain::TrackGain;
use crate::handle::ShutdownStage;
use crate::permissions::checks::{self, Check, CheckContext, Checked, Recheck, View};
use crate::permissions::fs_deadline::Limits;
use crate::permissions::{
    self, Locations, MAX_LOCATION, MAX_PREFIX, MAX_SESSION, Permissions, StoredAgent, Target,
    agent_not_found, grant_not_found,
};
use crate::queue::Queue;
use crate::state_file::{
    SavedAudio, SavedContext, SavedEntry, SavedQueue, SavedRepeat, SavedReplayGain, SavedState,
    StateFile,
};
use crate::waveform_worker::WaveformWorker;

/// Events a slow subscriber may have pending before it is told to resync.
const OUTBOX: usize = 256;

const POSITION_EVERY: Duration = Duration::from_millis(250);

/// How long an automatic pass waits for a library scan that has started.
const AUTOMATIC_SCAN_WAIT: Duration = Duration::from_secs(60);

/// How long an automatic pass that was cancelled waits before it goes on.
const AUTOMATIC_RESUME: Duration = Duration::from_secs(30);

const SAVE_EVERY: Duration = Duration::from_secs(5);

const SAVE_POSITION_EVERY: Duration = Duration::from_secs(10);

/// `Previous` within this much of the start goes to the previous track; later, it
/// restarts the current one.
const RESTART_THRESHOLD_MS: u64 = 3_000;

/// Files that fail in a row before playback gives up.
const MAX_CONSECUTIVE_FAILURES: usize = 5;

/// How long a confirmation token stays valid (unless `CoreConfig::confirmation_ttl`).
pub(crate) const CONFIRMATION_TTL: Duration = Duration::from_secs(5 * 60);

/// Idempotency keys kept, and for how long (contract guarantee 13).
const IDEMPOTENCY_KEYS: usize = 1024;

const IDEMPOTENCY_TTL: Duration = Duration::from_secs(10 * 60);

/// Available API capabilities (`Snapshot::features`).
const FEATURES: &[&str] = &[
    "multi_artist",
    "cover_edit",
    "playback_limits",
    "output_devices",
    "output_mode_contract",
    "permissions",
    "lyrics",
    "waveform",
    "cue",
    "edit_tags",
    "automatic_loudness",
    "paused_navigation",
    "track_rows",
    "album_paging",
    "search_paging",
    "enqueue_expected_current",
    "playlist_playback_guards",
    "release_root",
    "queue_candidates",
    "structured_signal_path",
    "job_queries",
    "action_details",
    "scan_error_codes",
];

pub(crate) enum Msg {
    OutputQuery {
        query: crate::contract::Query,
        reply: Sender<crate::contract::QueryResult>,
    },
    Pending {
        reply: Sender<crate::ConfirmationSnapshot>,
    },
    Request {
        request: Request,
        origin: Origin,
        registered: Option<u64>,
        undo_guard: Option<Box<mpmp_library::UndoDescription>>,
        purge_guard: Option<mpmp_library::PurgeGuard>,
        reply: Sender<Response>,
    },
    Subscribe {
        origin: Origin,
        options: SubscribeOptions,
        reply: Sender<(Snapshot, Receiver<EventEnvelope>)>,
    },
    Scan(ScanEvent),
    /// An agent's session ended (`CoreHandle::end_agent_session`).
    EndSession {
        agent: String,
        session: String,
        reply: Sender<()>,
    },
    /// The handle saw a failure that tells of a problem (`Snapshot.problems`).
    Failure(ContractError),
    /// The checks of an agent's command, run off the actor ([`Waiting`]), are done.
    Checked {
        id: u64,
        checked: Checked,
    },
    /// The check again of an approved agent command, run off the actor, is done.
    Rechecked {
        id: u64,
        result: Result<(), ContractError>,
    },
    /// The waveform thread made (or found) a track's waveform.
    WaveformReady(TrackId),
    /// A tag job (`EditTags`, or the undo of a tag edit) went on or ended.
    TagJob {
        job: JobId,
        event: TagJobEvent,
    },
    Loudness {
        job: JobId,
        event: crate::loudness_scan::LoudnessEvent,
    },
    /// Background measurement saved values of these tracks (`warmed`: one measured for a
    /// waveform, outside an automatic pass, whose album may now be complete).
    AutomaticLoudnessChanged {
        tracks: Vec<TrackId>,
        warmed: bool,
    },
    /// Waveform warming found a changed or missing file for the first time.
    AutomaticRescan,
    ScanRoots {
        roots: Vec<RootId>,
        reply: Sender<Result<JobId, ContractError>>,
    },
    VerifyScan {
        previous: Option<ScanResult>,
        completed: Sender<ScanResult>,
        reply: Sender<Result<JobId, ContractError>>,
    },
    RetryScan {
        directories: Vec<std::path::PathBuf>,
        previous: ScanResult,
        completed: Sender<ScanResult>,
        reply: Sender<Result<JobId, ContractError>>,
    },
    EnsureRootScan {
        root: RootId,
        covered: Arc<crate::root_access::ScanCoverage>,
        reply: Sender<Result<Option<JobId>, ContractError>>,
    },
    RootDrained {
        root: RootId,
        result: Result<(), ContractError>,
    },
    Shutdown,
}

/// What the actor publishes for lock-free reads by queries.
pub(crate) struct Published {
    pub snapshot: Snapshot,
    pub queue: Arc<Vec<(EntryId, TrackId)>>,
}

/// A snapshot before the actor has published anything.
pub(crate) fn empty_snapshot(capabilities: mpmp_storage::WriteCapabilities) -> Snapshot {
    Snapshot {
        seq: 0,
        contract_version: CONTRACT_VERSION,
        contract_minor: CONTRACT_MINOR,
        features: FEATURES
            .iter()
            .filter(|f| match **f {
                "cover_edit" => capabilities.durable_replace,
                "edit_tags" => capabilities.durable_replace && capabilities.legacy_tag_write,
                _ => true,
            })
            .map(|f| (*f).to_owned())
            .collect(),
        now_ms: 0,
        state: PlayState::Stopped,
        current: None,
        position: None,
        queue: QueueSummary::default(),
        shuffle: false,
        repeat: RepeatMode::Off,
        audio: AudioSettings {
            output: Some(crate::contract::OutputSettings::default()),
            ..Default::default()
        },
        playback_limits: Some(PlaybackLimitsInfo::default()),
        output: Some(Default::default()),
        problems: Vec::new(),
    }
}

/// The most agent commands checked off the actor at once; beyond that a command's checks
/// run on the actor with a time limit used up already (they fail at once, as late).
const MAX_CHECKS: usize = 16;

/// How many times checks whose grants or music folders changed meanwhile are run again
/// off the actor; after that once more on the actor (so the answer is up to date).
const MAX_ATTEMPTS: u32 = 8;

/// Lease draining never owns an actor or a response sender. Shutdown drops pending
/// answers immediately; a late worker completion is harmless.
const ROOT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct Actor {
    job_history: Arc<Mutex<crate::jobs::JobHistory>>,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
    engine: Engine,
    limits: LimitState,
    engine_events: Receiver<EngineEvent>,
    output: OutputState,
    catalog: Arc<dyn Catalog>,
    published: Arc<ArcSwap<Published>>,
    playback: PlaybackState,
    publication: PublicationState,
    epoch: Instant,
    persistence: PersistenceState,
    idle_track: Option<(EntryId, TrackInfo)>,
    audio: AudioState,
    confirmations: ConfirmationState,
    scans: ScanState,
    published_queue: (crate::contract::QueueRevision, Arc<Vec<(EntryId, TrackId)>>),
    access: AccessState,
    /// `Snapshot.problems`, oldest first.
    problems: Vec<Problem>,
    requests: RequestState,
    roots: RootState,
    /// Counts the changes of what an agent command's checks depend on: grants, agents and
    /// sessions (each `PermissionsChanged`, `AgentsChanged`), music folders
    /// ([`Actor::folders_changed`]).
    generation: u64,
    workers: WorkerState,
    /// Actual decoder/open-task leases, including paused and retired prefetch sources.
    pub(super) open_files: Arc<Mutex<Vec<mpmp_media_io::MediaUri>>>,
    pub(crate) shutdown_stage: Arc<Mutex<ShutdownStage>>,
    root_access: Arc<crate::root_access::RootAccess>,
}

/// The core's inputs besides the engine: where things are kept and the platform's rules.
pub(crate) struct Parts {
    pub job_history: Arc<Mutex<crate::jobs::JobHistory>>,
    pub root_access: Arc<crate::root_access::RootAccess>,
    pub catalog: Arc<dyn Catalog>,
    pub published: Arc<ArcSwap<Published>>,
    pub state_file: Option<StateFile>,
    pub permissions: Arc<Mutex<Permissions>>,
    pub locations: Arc<dyn Locations>,
    pub agent_fs: Limits,
    pub confirmation_ttl: Duration,
    /// Problems known when core starts.
    pub problems: Vec<Problem>,
    pub waveforms: WaveformWorker,
    pub loudness: crate::loudness_scan::LoudnessWorker,
    pub automatic_loudness: bool,
    pub playback_clock: Arc<dyn mpmp_engine::SuspendClock>,
    pub playback_deadline: Arc<mpmp_engine::PlaybackDeadline>,
}

impl Actor {
    pub fn new(
        rx: Receiver<Msg>,
        tx: Sender<Msg>,
        mut engine: Engine,
        parts: Parts,
        epoch: Instant,
    ) -> Self {
        let Parts {
            job_history,
            root_access,
            catalog,
            published,
            state_file,
            permissions,
            locations,
            agent_fs,
            confirmation_ttl,
            problems,
            waveforms,
            loudness,
            automatic_loudness,
            playback_clock,
            playback_deadline,
        } = parts;
        let open_files = Arc::new(Mutex::new(Vec::new()));
        let playback_files = Arc::clone(&open_files);
        let playback_catalog = Arc::clone(&catalog);
        engine.set_file_access(Arc::new(move |uri| {
            let root = playback_catalog.playback_lease(uri).map_err(|error| {
                mpmp_engine::EngineError::Source(mpmp_media_io::Error::Io(std::io::Error::other(
                    error.to_string(),
                )))
            })?;
            playback_files
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(uri.clone());
            Ok(Box::new(PlaybackFile {
                uri: uri.clone(),
                open: Arc::clone(&playback_files),
                _root: root,
            }))
        }));
        let engine_events = engine.events();
        let output_events = engine.output_events();
        let now = unix_ms().max(0) as u64;
        let initial_selection = engine.selected_output_device().map_or(
            crate::contract::OutputDeviceSelection::SystemDefault,
            |id| crate::contract::OutputDeviceSelection::Device { id: id.0 },
        );
        let mut actor = Self {
            job_history,
            root_access,
            shutdown_stage: Arc::default(),
            rx,
            tx,
            engine,
            limits: LimitState {
                playback_clock,
                playback_deadline,
                stop_after_current: None,
                sleep_deadline: None,
            },
            engine_events,
            output: OutputState {
                output_events,
                output_policy: Default::default(),
                output_at: None,
                unrendered_resume: None,
                output_failure_serial: 0,
                output_error: None,
                output_settings: crate::contract::OutputSettings {
                    selection: initial_selection,
                    ..Default::default()
                },
                output_devices: Vec::new(),
                output_devices_generation: 0,
                output_devices_poll_at: 1000,
                output_discovery_epoch: 0,
                output_discovery_busy: None,
                output_discovery_pending: false,
                output_discovery_task: None,
                output_discovery_retry: None,
                output_capability_tasks: Vec::new(),
                output_devices_error: None,
                output_discovery_lost: false,
                output_discovery_arrived: false,
                output_selected: None,
                output_selection_failure: None,
                output_fallback: None,
                announced_output: None,
            },
            catalog,
            published,
            playback: PlaybackState {
                queue: Queue::with_seeds(now, now),
                state: PlayState::Stopped,
                instances: HashMap::new(),
                next_instance: 0,
                playing: None,
                pending_next: None,
                failed_next: Vec::new(),
                resume_at: None,
                seek_hint: Cell::new(None),
            },
            publication: PublicationState {
                subscribers: Vec::new(),
                seq: 0,
                state_seq: 0,
                cause: None,
                announced_queue: QueueSummary::default(),
            },
            epoch,
            persistence: PersistenceState {
                state_file,
                dirty: false,
                last_save: Instant::now(),
            },
            idle_track: None,
            audio: AudioState {
                volume: 1.0,
                muted: false,
                replay_gain: ReplayGainSettings::default(),
            },
            confirmations: ConfirmationState {
                held: HashMap::new(),
                host_cover_sources: None,
                host_recovery_guard: None,
                host_undo_guard: None,
                host_purge_guard: None,
                pending_read_revision: 0,
                next_token: 0,
                idempotency: IdempotencyCache::default(),
            },
            scans: ScanState {
                jobs: HashMap::new(),
                next_job: now,
            },
            published_queue: (
                crate::contract::QueueRevision(u64::MAX),
                Arc::new(Vec::new()),
            ),
            access: AccessState {
                permissions,
                locations,
                agent_fs,
                confirmation_ttl,
                next_grant: unix_ms(),
            },
            problems,
            requests: RequestState {
                waiting: HashMap::new(),
                next_waiting: 0,
            },
            roots: RootState {
                release_start: None,
                releasing: HashMap::new(),
                retired_decodes: HashMap::new(),
                removed_root_notifications: HashMap::new(),
            },
            generation: 0,
            workers: WorkerState {
                waveforms,
                loudness,
                loudness_job: None,
                automatic_loudness,
                automatic_dirty: automatic_loudness,
                awaiting_startup_scan: false,
                automatic_job: None,
                automatic_retry_at: None,
                automatic_failures: 0,
                automatic_rescan: false,
                waveform_tracks: Vec::new(),
                tag_jobs: HashMap::new(),
                cover_jobs: std::collections::HashSet::new(),
            },
            open_files,
        };
        actor.restore(now);
        actor.engine.set_volume(actor.master_volume());
        actor.publication.announced_queue = actor.queue_summary();
        actor.publish();
        actor
    }

    pub fn run(mut self, startup_scan_by_host: bool) {
        self.workers.awaiting_startup_scan = startup_scan_by_host;
        let ticker = tick(POSITION_EVERY);
        let output_poll = tick(Duration::from_millis(25));
        // After a library upgrade that needs every file re-read (e.g. to collect all
        // tags), do that now rather than waiting for the user to rescan.
        if !startup_scan_by_host {
            match self.startup_scan_required() {
                Ok(true) => {
                    tracing::info!("checking library before background analysis");
                    if let Err(err) = self.start_scan(None) {
                        tracing::warn!("cannot start the library scan: {err}");
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, "cannot determine whether startup scan is needed");
                    self.note(&error);
                }
            }
        }
        if !self.workers.awaiting_startup_scan {
            self.start_automatic_loudness();
        }
        self.tell_waveforms();
        let mut stopping = false;
        let mut shutdown_decode = None;
        loop {
            if stopping && self.workers.tag_jobs.is_empty() && self.scans.jobs.is_empty() {
                break;
            }
            let output_timer = self
                .output
                .output_policy
                .next_due()
                .filter(|_| !stopping)
                .map_or_else(crossbeam_channel::never, |at| {
                    crossbeam_channel::after(Duration::from_millis(
                        at.saturating_sub(self.now_ms()),
                    ))
                });
            select! {
                recv(output_poll) -> _ => {
                    if !stopping {
                        self.process_playback_limits();
                        self.poll_output();
                    }
                },
                recv(output_timer) -> _ => {
                    if !stopping {
                        self.poll_output();
                    }
                },
                recv(self.rx) -> msg => {
                    match msg {
                        Ok(Msg::Shutdown) if !stopping => {
                            stopping = true;
                            self.set_shutdown_stage(ShutdownStage::FreezingOutput);
                            self.cancel_output_reads();
                            shutdown_decode = self.freeze_for_shutdown();
                            self.engine_events = crossbeam_channel::never();
                            self.output.output_events = crossbeam_channel::never();
                            self.workers.waveforms.cancel();
                            self.workers.loudness.cancel();
                            // No asynchronous check may start a command during shutdown.
                            self.requests.waiting.clear();
                            self.roots.releasing.clear();
                            self.set_shutdown_stage(ShutdownStage::WaitingForJobs);
                        }
                        Err(_) => break,
                        Ok(msg) if stopping => match msg {
                            Msg::Scan(_) | Msg::TagJob { .. } | Msg::Loudness { .. } => {
                                self.handle(msg)
                            }

                            // Dropping reply senders returns CoreStopped to their callers.
                            _ => {}
                        },
                        Ok(msg) => self.handle(msg),
                    }
                },
                recv(self.output.output_events) -> _ => {
                    if !stopping {
                        self.on_output();
                    }
                },
                recv(self.engine_events) -> event => {
                    {
                        if !stopping && let Ok(event) = event {
                            self.on_engine(event);
                        }
                    }
                    // Ticks change nothing a query reports: positions are interpolated from
                    // the last snapshot, so there is nothing to publish (but a problem found
                    // meanwhile is).
                },
                recv(ticker) -> _ => {
                    if stopping {
                        continue;
                    }
                    let start = self.on_tick();
                    if !self.check_catalog() && !start {
                        continue;
                    }
                },
            }
            if stopping {
                continue;
            }
            self.check_catalog();
            self.start_automatic_loudness();
            self.publish();
            self.tell_waveforms();
        }
        // Drop the last receiver and every queued reply sender before joining workers.
        // While draining jobs above, their completion notifications still use this queue.
        self.rx = crossbeam_channel::never();
        self.cancel_output_reads();
        self.workers.waveforms.cancel();
        self.workers.loudness.cancel();
        if !stopping {
            self.set_shutdown_stage(ShutdownStage::FreezingOutput);
            shutdown_decode = self.freeze_for_shutdown();
        }
        self.set_shutdown_stage(ShutdownStage::SavingState);
        self.save_now();
        if let Some(file) = &mut self.persistence.state_file {
            file.finish();
        }
        // Close output and save the rendered position before waiting on slow media. Every
        // decoder must exit before we can take the final list of prefetch threads to join.
        if let Some(thread) = shutdown_decode {
            self.set_shutdown_stage(ShutdownStage::JoiningDecoder);
            let _ = thread.join();
        }
        self.set_shutdown_stage(ShutdownStage::JoiningRetiredDecoders);
        self.join_retired_decodes();
        self.set_shutdown_stage(ShutdownStage::JoiningPrefetch);
        self.engine.stop_and_wait();
        self.set_shutdown_stage(ShutdownStage::DroppingActor);
    }

    fn set_shutdown_stage(&self, stage: ShutdownStage) {
        *self
            .shutdown_stage
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = stage;
    }

    fn startup_scan_required(&self) -> Result<bool, ContractError> {
        if self.catalog.rescan_recommended()? || self.catalog.artist_backfill_pending()? {
            return Ok(true);
        }
        if self.workers.automatic_loudness && self.catalog.supports_loudness_scan() {
            return match self.catalog.query(&crate::contract::Query::Roots) {
                crate::contract::QueryResult::Roots { roots } => Ok(!roots.is_empty()),
                crate::contract::QueryResult::Err { error } => Err(error),
                _ => Ok(false),
            };
        }
        Ok(false)
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    // --- messages ---

    fn handle(&mut self, msg: Msg) {
        self.process_playback_limits();
        match msg {
            Msg::OutputQuery { query, reply } => self.handle_output_query(query, reply),
            Msg::Pending { reply } => {
                self.expire_confirmations();
                let mut held: Vec<_> = self.confirmations.held.values().collect();
                held.sort_by_key(|h| h.arrival);
                let pending = held.into_iter().map(|h| h.view.clone()).collect();
                self.confirmations.pending_read_revision += 1;
                let _ = reply.send(crate::ConfirmationSnapshot {
                    revision: self.confirmations.pending_read_revision,
                    pending,
                });
            }
            Msg::Request {
                request,
                origin,
                registered,
                undo_guard,
                purge_guard,
                reply,
            } => {
                self.confirmations.host_undo_guard = undo_guard.map(|guard| *guard);
                self.confirmations.host_purge_guard = purge_guard;
                let Request {
                    request_id,
                    idempotency_key,
                    command,
                } = request;
                let key = idempotency_key.map(|k| (origin.label(), registered, k));
                self.request(request_id, key, command, origin, registered, reply);
                self.confirmations.host_undo_guard = None;
                self.confirmations.host_purge_guard = None;
            }
            Msg::Checked { id, checked } => self.on_checked(id, checked),
            Msg::Rechecked { id, result } => self.on_rechecked(id, result),
            Msg::Subscribe {
                origin,
                options,
                reply,
            } => {
                // Registered and snapshotted in one step, so no event falls in between.
                let (tx, rx) = crossbeam_channel::bounded(OUTBOX);
                self.publication.subscribers.push(Subscriber {
                    tx,
                    lagging: false,
                    ui: origin == Origin::Ui,
                    positions: options.positions,
                });
                let _ = reply.send((self.snapshot(), rx));
            }
            Msg::Scan(event) => self.on_scan(event),
            Msg::EndSession {
                agent,
                session,
                reply,
            } => {
                self.end_session(&agent, &session);
                let _ = reply.send(());
            }
            Msg::Failure(error) => self.note(&error),
            // Computed in the background: no command caused it (guarantee 5).
            Msg::WaveformReady(track) => self.emit_as(None, Event::WaveformReady { track }),
            Msg::TagJob { job, event } => self.on_tag_job(job, event),
            Msg::Loudness { job, event } => self.on_loudness(job, event),
            // Computed in the background: no command caused it (guarantee 5).
            Msg::AutomaticLoudnessChanged { tracks, warmed } => {
                if warmed {
                    self.workers.automatic_dirty = true;
                }
                if self.refresh_loaded_tracks() {
                    self.emit_state_as(None);
                }
                self.emit_as(None, Event::TracksChanged { tracks });
                self.emit_as(None, Event::LibraryChanged);
            }
            Msg::AutomaticRescan => {
                self.workers.automatic_rescan = true;
                self.workers.automatic_dirty = true;
            }
            Msg::ScanRoots { roots, reply } => {
                let result = roots
                    .iter()
                    .try_for_each(|&root| self.catalog.check_root_access(root))
                    .and_then(|()| self.start_scan_scoped(Some(Origin::System), Some(roots)));
                let _ = reply.send(result);
            }
            Msg::VerifyScan {
                previous,
                completed,
                reply,
            } => {
                let result = self.start_scan_scoped_inner(
                    Some(Origin::System),
                    None,
                    Some((previous, completed)),
                );
                let _ = reply.send(result);
            }
            Msg::RetryScan {
                directories,
                previous,
                completed,
                reply,
            } => {
                let tx = self.tx.clone();
                let result = self
                    .catalog
                    .rescan_directories(
                        directories,
                        Box::new(move |event| {
                            let _ = tx.send(Msg::Scan(event));
                        }),
                    )
                    .map(|ticket| {
                        if let Some(job) = self.scans.jobs.get_mut(&ticket.scan) {
                            job.completed.push(completed);
                            job.id
                        } else {
                            let id = self.new_job();
                            self.scans.jobs.insert(
                                ticket.scan,
                                Job {
                                    id,
                                    origin: None,
                                    started: Instant::now(),
                                    background: Some(previous),
                                    completed: vec![completed],
                                    diagnostics: None,
                                },
                            );
                            id
                        }
                    });
                let _ = reply.send(result);
            }
            Msg::EnsureRootScan {
                root,
                covered,
                reply,
            } => {
                // Serialized with AddRoot/ResumeRoot and their preparation callbacks.
                // A baseline queued before preparation must not race that command.
                let result = self.catalog.check_root_access(root).and_then(|()| {
                    if covered.is_covered() {
                        Ok(None)
                    } else {
                        self.start_scan_scoped(Some(Origin::System), Some(vec![root]))
                            .map(Some)
                    }
                });
                let _ = reply.send(result);
            }
            Msg::RootDrained { root, result } => self.root_drained(root, result),
            Msg::Shutdown => {}
        }
    }

    // --- playback ---

    // --- snapshot, events, persistence ---
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests;

mod agents;
mod commands;
mod confirmations;
mod convert;
mod dispatch;
mod engine_events;
mod gain;
mod idempotency;
mod jobs;
mod limits;
mod output;
mod output_settings;
mod persistence;
mod playback;
mod publish;
mod requests;
mod roots;
mod state;

pub(crate) use agents::lock_permissions;
use agents::manages_permissions;
use confirmations::Held;
use convert::{convert_signal_path, engine_error, engine_failure_code, unix_ms};
use dispatch::Flow;
use idempotency::{IdempotencyCache, IdempotencyKey, command_fingerprint, key_reused};
use jobs::Job;
use output::RebuildResult;
use playback::{Instance, PlaybackFile, Playing, SeekHint, entry_not_found};
use publish::Subscriber;
use requests::{Answer, Waiting, Work, withdrawn};
use roots::{ReleaseStart, Releasing, RemovalRecovery, RetiredDecode};

use state::{
    AccessState, AudioState, ConfirmationState, LimitState, OutputState, PersistenceState,
    PlaybackState, PublicationState, RequestState, RootState, ScanState, WorkerState,
};
