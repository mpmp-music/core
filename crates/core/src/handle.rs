//! Client handles, subscriptions and core startup and shutdown.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender};
use mpmp_engine::Engine;
use mpmp_engine::output::OutputBackend;
use mpmp_media_io::SourceResolver;

use crate::actor::{Actor, Msg, Parts, Published};
use crate::catalog::{Catalog, CoverArt, ScanResult};
use crate::contract::{
    Command, CommandResult, ContractError, ErrorCode, EventEnvelope, Origin, Problem, Query,
    QueryResult, QueueEntry, Request, Response, Snapshot, next_offset,
};
use crate::permissions::Permissions;
use crate::state_file::StateFile;
use crate::waveform_worker::{WaveformQueue, WaveformWorker};

use crate::json::{parse_request, parse_strict, to_json};
use crate::{
    AGENT_FILE_SYSTEM_CALLS, AGENT_FILE_SYSTEM_TIMEOUT, CoreConfig, RootAccessHandler,
    RootAccessRegistration, ScanCoverage, ScanPreparationHandler, SubscribeOptions, contract,
};
use crate::{actor, jobs, loudness_scan, permissions::fs_deadline, root_access};

/// What a client gets on subscribing: the state now, and every event after it.
pub struct Subscription {
    pub snapshot: Snapshot,
    pub events: Receiver<EventEnvelope>,
}

/// An authoritative trusted-host pending table. `revision` orders reads even when
/// the event sequence does not change (for example during approval rechecks).
#[derive(Debug, Clone)]
pub struct ConfirmationSnapshot {
    pub revision: u64,
    pub pending: Vec<contract::PendingConfirmation>,
}

/// Source of occupancy found during a file undo preview, for native hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileUndoOccupancy {
    Playback,
    External,
}

/// A handle to the running core. Cheap to clone; the core stops when the last handle
/// is dropped (or on [`CoreHandle::shutdown`]).
#[derive(Clone)]
pub struct CoreHandle {
    inner: Arc<Inner>,
}

/// The outcome of waiting for core and catalog shutdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownError {
    TimedOut,
    ActorPanicked,
    Library(crate::catalog::CatalogCloseError),
}

impl std::fmt::Display for ShutdownError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut => f.write_str("core shutdown timed out"),
            Self::ActorPanicked => f.write_str("the core shutdown thread panicked"),
            Self::Library(error) => write!(f, "library close failed: {error}"),
        }
    }
}

impl std::error::Error for ShutdownError {}

/// An authenticated connection's registration identity, bound before any request is queued.
#[derive(Clone)]
pub struct AgentHandle {
    core: CoreHandle,
    name: String,
    registration: u64,
}

impl AgentHandle {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn same_registration(&self, other: &Self) -> bool {
        self.registration == other.registration
    }

    /// Executes as this registration, even if the name has since been registered again.
    pub fn execute(&self, request: Request, session: &str) -> Response {
        self.core.execute_registered(
            request,
            Origin::Agent {
                name: self.name.clone(),
                session: Some(session.to_owned()),
            },
            Some(self.registration),
        )
    }

    pub fn execute_json(&self, request: &str, session: &str) -> String {
        let response = match parse_request(request) {
            Ok(request) => self.execute(request, session),
            Err((request_id, error)) => Response {
                request_id,
                result: CommandResult::Err { error },
            },
        };
        to_json(&response)
    }
}

struct Inner {
    open_files: Arc<Mutex<Vec<mpmp_media_io::MediaUri>>>,
    jobs: Arc<Mutex<jobs::JobHistory>>,
    root_access: Arc<root_access::RootAccess>,
    tx: Sender<Msg>,
    published: Arc<ArcSwap<Published>>,
    catalog: Arc<dyn Catalog>,
    permissions: Arc<Mutex<Permissions>>,
    /// Waveforms a query found missing go here, to be computed in the background.
    waveforms: WaveformQueue,
    epoch: Instant,
    stopping: Arc<AtomicBool>,
    stopped: Receiver<()>,
    shutdown_result: Arc<OnceLock<Result<(), ShutdownError>>>,
    thread: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(thread) = self.thread.lock().ok().and_then(|mut t| t.take()) {
            let _ = thread.join();
        }
    }
}

impl CoreHandle {
    /// Register a trusted platform participant in root release. `released=true` must
    /// synchronously close the folder's handles and prevent reopening until false.
    /// Callbacks must not execute commands or wait for this core's actor. They may
    /// return an error if closure cannot be proven; the root stays suspended.
    pub fn register_root_access_handler(
        &self,
        handler: Arc<RootAccessHandler>,
    ) -> RootAccessRegistration {
        self.inner.root_access.register(handler)
    }

    /// Starts the core on its own thread. The output backend and source resolver come
    /// from the platform's assembly root. Returns an error if a thread cannot start
    /// or Locations is missing without `desktop-fs`. Problems with core's data are
    /// reported in `Snapshot.problems`, for the app to handle.
    pub fn start(
        backend: Box<dyn OutputBackend>,
        resolver: Arc<dyn SourceResolver>,
        catalog: Arc<dyn Catalog>,
        config: CoreConfig,
    ) -> std::io::Result<Self> {
        // Resolve permissions before starting workers, constructing the engine or loading data.
        let locations = match config.locations {
            Some(locations) => locations,
            #[cfg(feature = "desktop-fs")]
            None => Arc::new(mpmp_fs_desktop::DesktopLocations),
            #[cfg(not(feature = "desktop-fs"))]
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "missing Locations backend; set CoreConfig.locations",
                ));
            }
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        let automatic_loudness = config.automatic_loudness_scan.unwrap_or(true);
        let analysis = Arc::new(loudness_scan::Analysis::new(
            config
                .automatic_loudness_retry
                .unwrap_or(loudness_scan::RETRY),
        ));
        let waveforms = WaveformWorker::start(
            Arc::clone(&resolver),
            Arc::clone(&catalog),
            tx.clone(),
            Arc::clone(&analysis),
            automatic_loudness,
        )?;
        let waveform_queue = waveforms.queue();
        let loudness = loudness_scan::LoudnessWorker::new(
            Arc::clone(&resolver),
            Arc::clone(&catalog),
            tx.clone(),
            analysis,
        );
        let mut engine = Engine::new(backend, resolver, config.engine);
        let playback_clock = match config.playback_clock {
            Some(clock) => clock,
            None => match mpmp_engine::SystemSuspendClock::new() {
                Ok(clock) => Arc::new(clock) as Arc<dyn mpmp_engine::SuspendClock>,
                Err(error) => {
                    tracing::warn!(%error,
                     "sleep timer precision limited: suspend time is excluded");

                    Arc::new(mpmp_engine::MonotonicPlaybackClock::default())
                }
            },
        };
        let playback_deadline = Arc::new(mpmp_engine::PlaybackDeadline::new(Arc::clone(
            &playback_clock,
        )));
        engine.set_playback_deadline(Arc::clone(&playback_deadline));
        let epoch = Instant::now();
        let published = Arc::new(ArcSwap::from_pointee(Published {
            snapshot: actor::empty_snapshot(catalog.storage_write_capabilities()),
            queue: Arc::new(Vec::new()),
        }));
        let mut problems = Vec::new();
        let permissions = Arc::new(Mutex::new(load_permissions(
            catalog.as_ref(),
            &mut problems,
        )));
        let root_access = Arc::new(root_access::RootAccess::default());
        let jobs = Arc::new(Mutex::new(jobs::JobHistory::default()));
        let actor = Actor::new(
            rx,
            tx.clone(),
            engine,
            Parts {
                job_history: Arc::clone(&jobs),
                root_access: Arc::clone(&root_access),
                catalog: Arc::clone(&catalog),
                published: Arc::clone(&published),
                state_file: config.state_file.map(StateFile::new),
                permissions: Arc::clone(&permissions),
                locations,
                agent_fs: fs_deadline::Limits::new(
                    config
                        .agent_file_system_timeout
                        .unwrap_or(AGENT_FILE_SYSTEM_TIMEOUT),
                    AGENT_FILE_SYSTEM_CALLS,
                ),
                confirmation_ttl: config.confirmation_ttl.unwrap_or(actor::CONFIRMATION_TTL),
                problems,
                waveforms,
                loudness,
                automatic_loudness,
                playback_clock,
                playback_deadline,
            },
            epoch,
        );
        let open_files = Arc::clone(&actor.open_files);
        let (completion, stopped) = crossbeam_channel::bounded::<()>(0);
        let shutdown_result = Arc::new(OnceLock::new());
        let result = Arc::clone(&shutdown_result);
        let closing_catalog = Arc::clone(&catalog);
        let stopping = Arc::new(AtomicBool::new(false));
        let explicit_shutdown = Arc::clone(&stopping);
        let thread = std::thread::Builder::new()
            .name("core".into())
            .spawn(move || {
                actor.run(config.startup_scan_by_host);
                // Actor::run consumes the actor. Its workers and state writer are joined
                // before closing shared catalog storage.
                let closed = if explicit_shutdown.load(Ordering::Acquire) {
                    closing_catalog.close().map_err(ShutdownError::Library)
                } else {
                    Ok(())
                };
                let _ = result.set(closed);
                drop(completion);
            })?;
        Ok(Self {
            inner: Arc::new(Inner {
                open_files,
                jobs,
                root_access,
                tx,
                published,
                catalog,
                permissions,
                waveforms: waveform_queue,
                epoch,
                stopping,
                stopped,
                shutdown_result,
                thread: std::sync::Mutex::new(Some(thread)),
            }),
        })
    }

    /// Runs a command and returns its receipt. `origin` is stamped by the adapter
    /// the request came through, never by the client. Returns when the command is done
    /// and its effects are visible (contract guarantee 3).
    pub fn execute(&self, request: Request, origin: Origin) -> Response {
        let registered = match &origin {
            Origin::Agent { name, .. } => {
                actor::lock_permissions(&self.inner.permissions).registration(name)
            }
            _ => None,
        };
        self.execute_registered(request, origin, registered)
    }

    /// Trusted-host recovery API. Not a public Query: adapters must never expose this
    /// on agent or theme transports. Only Ui may read approval tickets.
    pub fn pending_confirmations(
        &self,
        origin: Origin,
    ) -> Result<Vec<contract::PendingConfirmation>, ContractError> {
        self.pending_confirmations_snapshot(origin)
            .map(|s| s.pending)
    }
    pub fn pending_confirmations_snapshot(
        &self,
        origin: Origin,
    ) -> Result<ConfirmationSnapshot, ContractError> {
        if origin != Origin::Ui {
            return Err(ContractError::forbidden(
                ErrorCode::OriginNotAllowed,
                "UI only",
            ));
        }
        let (reply, rx) = crossbeam_channel::bounded(1);
        self.inner
            .tx
            .send(Msg::Pending { reply })
            .map_err(|_| core_stopped())?;
        rx.recv().map_err(|_| core_stopped())
    }
    /// Read-only before/after values and resolved locations for a tag undo. No write lock.
    pub fn preview_undo(&self, action: contract::ActionId) -> Result<QueryResult, ContractError> {
        self.preview_undo_with_occupancy(action)
            .map(|(preview, _)| preview)
    }

    /// Return the occupancy observed in the same preview, without changing its wire data.
    pub fn preview_undo_with_occupancy(
        &self,
        action: contract::ActionId,
    ) -> Result<(QueryResult, Option<FileUndoOccupancy>), ContractError> {
        let preview = self.inner.catalog.preview_undo(action)?;
        let external = match &preview {
            QueryResult::TagEditPreview { skipped, .. } => skipped
                .iter()
                .any(|row| row.code == contract::ErrorCode::FileInUse),
            QueryResult::CoverEditPreview { skipped, .. } => skipped
                .iter()
                .any(|row| row.code == contract::ErrorCode::FileInUse),
            _ => false,
        };
        let (preview, playback) = self.available_preview(preview);
        let occupancy = if external {
            Some(FileUndoOccupancy::External)
        } else {
            playback.then_some(FileUndoOccupancy::Playback)
        };
        Ok((preview, occupancy))
    }
    /// Effective inverse and targets from the complete retained chain; grants no authority.
    pub fn describe_undo(
        &self,
        action: contract::ActionId,
    ) -> Result<mpmp_library::UndoDescription, ContractError> {
        self.inner.catalog.describe_undo(action)
    }
    /// Execute a previously described undo, checking it again on the actor and in the
    /// library transaction / serialized tag job. No new wire command or approval token.
    pub fn execute_described_undo(
        &self,
        request: Request,
        origin: Origin,
        expected: mpmp_library::UndoDescription,
    ) -> Response {
        if origin != Origin::Ui
            || !matches!(
                &request.command,
                Command::UndoAction { action } if action.0 == expected.action.0,
            )
        {
            return Response {
                request_id: request.request_id,
                result: CommandResult::Err {
                    error: ContractError::forbidden(ErrorCode::OriginNotAllowed, "UI undo only"),
                },
            };
        }
        self.execute_registered_guarded(request, origin, None, Some(expected), None)
    }
    /// Frozen permanent-deletion preview; no authority is granted by this read.
    pub fn describe_purge(
        &self,
        ids: Option<&[contract::PlaylistId]>,
    ) -> Result<mpmp_library::PurgeGuard, ContractError> {
        self.inner.catalog.describe_purge(ids)
    }
    /// Ui-only execution of exactly a reviewed trash selection, checked atomically.
    /// A null selection is accepted only with an all-trash guard; the transaction still
    /// checks the complete reviewed trash, including items added or removed meanwhile.
    pub fn execute_described_purge(
        &self,
        request: Request,
        origin: Origin,
        expected: mpmp_library::PurgeGuard,
    ) -> Response {
        let matches = match &request.command {
            Command::PurgePlaylists { playlists: None } => expected.all,
            Command::PurgePlaylists {
                playlists: Some(ids),
            } => {
                ids.iter()
                    .map(|p| p.0)
                    .collect::<std::collections::BTreeSet<_>>()
                    == expected.playlists.iter().map(|p| p.id.0).collect()
            }
            _ => false,
        };
        if origin != Origin::Ui || !matches {
            return Response {
                request_id: request.request_id,
                result: CommandResult::Err {
                    error: ContractError::forbidden(
                        ErrorCode::OriginNotAllowed,
                        "UI bound permanent deletion only",
                    ),
                },
            };
        }
        self.execute_registered_guarded(request, origin, None, None, Some(expected))
    }
    fn execute_registered(
        &self,
        request: Request,
        origin: Origin,
        registered: Option<u64>,
    ) -> Response {
        self.execute_registered_guarded(request, origin, registered, None, None)
    }
    fn execute_registered_guarded(
        &self,
        request: Request,
        origin: Origin,
        registered: Option<u64>,
        undo_guard: Option<mpmp_library::UndoDescription>,
        purge_guard: Option<mpmp_library::PurgeGuard>,
    ) -> Response {
        // Keep invalidation irreversible until the actor has answered; confirmation
        // tickets acquire their own pin before this caller can release its pin.
        let _session = actor::lock_permissions(&self.inner.permissions).pin_session(&origin);
        let (reply, rx) = crossbeam_channel::bounded(1);
        let request_id = request.request_id;
        let sent = if self.inner.stopping.load(Ordering::Acquire) {
            None
        } else {
            self.inner
                .tx
                .send(Msg::Request {
                    request,
                    origin,
                    registered,
                    undo_guard: undo_guard.map(Box::new),
                    purge_guard,
                    reply,
                })
                .ok()
        };
        match sent.and_then(|()| rx.recv().ok()) {
            Some(response) => response,
            None => Response {
                request_id,
                result: CommandResult::Err {
                    error: core_stopped(),
                },
            },
        }
    }

    /// [`CoreHandle::execute`] for JSON adapters: a `Request` in, a `Response` out. A
    /// request that does not parse is answered with a contract error (`invalid_json`,
    /// `invalid_request`, `unknown_field` …) and the `request_id` it carries, if any.
    pub fn execute_json(&self, request: &str, origin: Origin) -> String {
        let response = match parse_request(request) {
            Ok(request) => self.execute(request, origin),
            Err((request_id, error)) => Response {
                request_id,
                result: CommandResult::Err { error },
            },
        };
        to_json(&response)
    }

    /// [`CoreHandle::query`] for JSON adapters: a `Query` in, a `QueryResult` out.
    pub fn query_json(&self, query: &str) -> String {
        let result = match parse_strict::<Query>(query) {
            Ok(query) => self.query(query),
            Err(error) => QueryResult::Err { error },
        };
        to_json(&result)
    }

    /// Answers a query. State and queue come from the published snapshot (they include
    /// the client's own latest commands); library queries go to the library's read
    /// connections. These do not wait for the actor. Output discovery and
    /// capability queries serialize with device selection on the actor, without
    /// opening streams, so native backend caches cannot race a reconstruction.
    pub fn query(&self, mut query: Query) -> QueryResult {
        query.cap_limit();
        let result = self.answer(query);
        if let QueryResult::Err { error } = &result {
            self.note(error);
        }
        result
    }

    fn available_preview(&self, mut result: QueryResult) -> (QueryResult, bool) {
        let open = self
            .inner
            .open_files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let in_use = |track| {
            self.inner
                .catalog
                .uri_of(track)
                .ok()
                .flatten()
                .is_some_and(|uri| open.contains(&uri))
        };
        let mut playback = false;
        match &mut result {
            QueryResult::CoverEditPreview { files, skipped } => files.retain(|file| {
                if in_use(file.track) {
                    playback = true;
                    skipped.push(contract::CoverFileResult {
                        track: file.track,
                        code: contract::ErrorCode::FileInUse,
                        message: "file is in use by playback".into(),
                    });
                    false
                } else {
                    true
                }
            }),
            QueryResult::TagEditPreview { files, skipped } => files.retain(|file| {
                if in_use(file.track) {
                    playback = true;
                    skipped.push(contract::TagFileResult {
                        track: file.track,
                        code: contract::ErrorCode::FileInUse,
                    });
                    false
                } else {
                    true
                }
            }),
            _ => {}
        }
        (result, playback)
    }

    fn answer(&self, query: Query) -> QueryResult {
        match query {
            query @ (Query::OutputDevices
            | Query::OutputDeviceCapabilities { .. }
            | Query::OutputState) => {
                let (reply, result) = crossbeam_channel::bounded(1);
                if self
                    .inner
                    .tx
                    .send(Msg::OutputQuery { query, reply })
                    .is_err()
                {
                    return QueryResult::Err {
                        error: core_stopped(),
                    };
                }
                result.recv().unwrap_or_else(|_| QueryResult::Err {
                    error: core_stopped(),
                })
            }
            Query::PlaybackLimits => {
                let snapshot = &self.inner.published.load().snapshot;
                QueryResult::PlaybackLimits {
                    playback_limits: snapshot.playback_limits.clone().unwrap_or_default(),
                    now_ms: snapshot.now_ms,
                }
            }
            query @ (Query::CoverEditPreview { .. } | Query::TagEditPreview { .. }) => {
                self.available_preview(self.inner.catalog.query(&query)).0
            }
            Query::Job { job } => jobs::get(&self.inner.jobs, job),
            Query::Jobs { offset, limit } => jobs::lock(&self.inner.jobs).page(offset, limit),
            Query::Snapshot => QueryResult::Snapshot {
                snapshot: Box::new(self.inner.published.load().snapshot.clone()),
            },
            Query::QueuePage { offset, limit } => {
                let published = self.inner.published.load();
                let slice: Vec<_> = published
                    .queue
                    .iter()
                    .skip(offset as usize)
                    .take(limit as usize)
                    .collect();
                let ids: Vec<_> = slice.iter().map(|(_, track)| *track).collect();
                match self.inner.catalog.tracks(&ids) {
                    Ok(tracks) => {
                        let by_id: HashMap<_, _> = tracks.into_iter().map(|t| (t.id, t)).collect();
                        let total = u32::try_from(published.queue.len()).unwrap_or(u32::MAX);
                        QueryResult::QueuePage {
                            revision: published.snapshot.queue.revision,
                            offset,
                            total,
                            next_offset: next_offset(offset, slice.len(), total),
                            // Every entry, also of tracks the library no longer knows.
                            entries: slice
                                .iter()
                                .map(|(entry, track)| QueueEntry {
                                    entry: *entry,
                                    track: by_id
                                        .get(track)
                                        .cloned()
                                        .unwrap_or_else(|| crate::catalog::placeholder(*track)),
                                })
                                .collect(),
                        }
                    }
                    Err(error) => QueryResult::Err { error },
                }
            }
            Query::Agents | Query::Permissions { .. }
                if !actor::lock_permissions(&self.inner.permissions).available() =>
            {
                QueryResult::Err {
                    error: permissions_unavailable(),
                }
            }
            Query::Agents => QueryResult::Agents {
                agents: actor::lock_permissions(&self.inner.permissions).agents(),
            },
            Query::Permissions {
                agent,
                offset,
                limit,
            } => actor::lock_permissions(&self.inner.permissions).page(
                agent.as_deref(),
                offset,
                limit,
                unix_ms(),
            ),
            // Read from disk on the caller's thread, like the cover.
            Query::Lyrics { track } => match self.inner.catalog.lyrics(track) {
                Ok(lyrics) => QueryResult::Lyrics { lyrics },
                Err(error) => QueryResult::Err { error },
            },
            // The cache is read on the caller's thread; a missing waveform is queued for the
            // waveform thread, and `WaveformReady` follows.
            Query::Waveform { track } => match self.inner.catalog.waveform(track) {
                Ok(Some(waveform)) => QueryResult::Waveform {
                    waveform: Some(waveform),
                },
                Ok(None) => {
                    if self.inner.catalog.waveform_accessible(track) {
                        self.inner.waveforms.ask(track);
                    }
                    QueryResult::Waveform { waveform: None }
                }
                Err(error) => QueryResult::Err { error },
            },
            Query::Loudness { track } => match self.inner.catalog.loudness(track) {
                Ok(loudness) => QueryResult::Loudness { loudness },
                Err(error) => QueryResult::Err { error },
            },
            Query::Unknown => QueryResult::Err {
                error: ContractError::unsupported(
                    ErrorCode::UnknownQuery,
                    "this core does not know that query",
                ),
            },
            other => self.inner.catalog.query(&other),
        }
    }

    /// Subscribes to events, atomically with a snapshot. `origin` is the
    /// subscriber's, stamped by the adapter: only `Ui` subscribers get
    /// `ConfirmationRequested`.
    pub fn subscribe(&self, origin: Origin, options: SubscribeOptions) -> Option<Subscription> {
        let (reply, rx) = crossbeam_channel::bounded(1);
        self.inner
            .tx
            .send(Msg::Subscribe {
                origin,
                options,
                reply,
            })
            .ok()?;
        let (snapshot, events) = rx.recv().ok()?;
        Some(Subscription { snapshot, events })
    }

    /// The registered agent whose token this is (agent permissions), for an adapter that
    /// lets agents in: `None` for a token no agent has (wrong, renewed, or of a removed
    /// agent), and while the permissions are unavailable. The agent is then seen now
    /// (`AgentInfo.last_seen_at_ms`; stored at most once a minute). Not part of the
    /// contract, so that no client can probe tokens through it. Adapters that execute
    /// commands use [`CoreHandle::authenticate_agent_connection`] to bind the registration.
    ///
    /// An adapter authenticates each connection, and again with the token it holds
    /// whenever `AgentsChanged` arrives: a connection whose token no longer names its
    /// agent (renewed, removed) must be closed. Sessions are random (a UUID, say), unique
    /// and never reused; when one ends, the adapter calls [`CoreHandle::end_agent_session`]
    /// (docs/design/agent-permissions.md).
    pub fn authenticate_agent(&self, token: &str) -> Option<String> {
        self.authenticate_agent_connection(token).map(|a| a.name)
    }

    /// Authenticates and binds an adapter connection to this registration instance.
    /// Adapters execute through this handle so removal invalidates even requests that
    /// have not reached the actor yet. The identity is internal, outside the contract.
    pub fn authenticate_agent_connection(&self, token: &str) -> Option<AgentHandle> {
        if self.inner.stopping.load(Ordering::Acquire) {
            return None;
        }
        let now = unix_ms();
        let (name, write, registration) = {
            let mut permissions = actor::lock_permissions(&self.inner.permissions);
            let (name, write) = permissions.authenticate(token, now)?;
            let registration = permissions.registration(&name)?;
            (name, write, registration)
        };
        if write && let Err(err) = self.inner.catalog.set_agent_seen(&name, now) {
            tracing::warn!("cannot record that agent {name} was seen: {err}");
            self.note(&err);
        }
        if self.inner.stopping.load(Ordering::Acquire) {
            return None;
        }
        Some(AgentHandle {
            core: self.clone(),
            name,
            registration,
        })
    }

    /// Immediately prevents this session's commands from starting. Does not wait for the
    /// actor or revoke grants; adapters follow this with `end_agent_session` for cleanup.
    pub fn invalidate_agent_session(&self, agent: &str, session: &str) {
        actor::lock_permissions(&self.inner.permissions).mark_session_ended(agent, session);
    }

    /// An agent's session ended (its adapter's connection closed): its grants for that
    /// session are revoked (`PermissionsChanged`), and its commands not started yet,
    /// including approved ones still being checked, are withdrawn (`ConfirmationResolved`, `approved: false`,
    /// `reason: withdrawn`). Returns when done. Not part of the contract: adapters call it.
    pub fn end_agent_session(&self, agent: &str, session: &str) {
        self.invalidate_agent_session(agent, session);
        let (reply, rx) = crossbeam_channel::bounded(1);
        let sent = self.inner.tx.send(Msg::EndSession {
            agent: agent.to_owned(),
            session: session.to_owned(),
            reply,
        });
        if sent.is_ok() {
            let _ = rx.recv();
        }
    }

    /// Cover image of a track (see [`Catalog::cover_art`](crate::catalog::MediaResources::cover_art)).
    pub fn cover_art(&self, track: contract::TrackId) -> Result<Option<CoverArt>, ContractError> {
        self.inner
            .catalog
            .cover_art(track)
            .inspect_err(|e| self.note(e))
    }

    /// Thumbnail of a track's cover for a longest edge of `max_edge` pixels (see
    /// [`Catalog::cover_thumbnail`](crate::catalog::MediaResources::cover_thumbnail)).
    pub fn cover_thumbnail(
        &self,
        track: contract::TrackId,
        max_edge: u32,
    ) -> Result<Option<CoverArt>, ContractError> {
        self.inner
            .catalog
            .cover_thumbnail(track, max_edge)
            .inspect_err(|e| self.note(e))
    }

    /// Hands a failure that may tell of a problem with core's data to the actor
    /// (`Snapshot.problems`).
    fn note(&self, error: &ContractError) {
        if error.code == ErrorCode::LibraryDamaged {
            let _ = self.inner.tx.send(Msg::Failure(error.clone()));
        }
    }

    /// Milliseconds on core's clock, the clock of `PositionInfo::at_ms`.
    pub fn now_ms(&self) -> u64 {
        self.inner.epoch.elapsed().as_millis() as u64
    }

    /// Trusted host/watcher baseline after registering one root. Never exposed to themes.
    pub fn rescan_root(&self, root: contract::RootId) -> Result<contract::JobId, ContractError> {
        self.rescan_roots(vec![root])
    }

    /// Trusted watcher scope; requests during a scan still schedule a follow-up pass.
    pub fn rescan_roots(
        &self,
        roots: Vec<contract::RootId>,
    ) -> Result<contract::JobId, ContractError> {
        let (reply, rx) = crossbeam_channel::bounded(1);
        self.inner
            .tx
            .send(Msg::ScanRoots { roots, reply })
            .map_err(|_| core_stopped())?;
        rx.recv().map_err(|_| core_stopped())?
    }

    /// Rust-only exact diagnostics, retained under the same job-history budget.
    pub fn scan_diagnostics(
        &self,
        job: contract::JobId,
    ) -> Option<Arc<mpmp_library::ScanDiagnostics>> {
        jobs::lock(&self.inner.jobs).scan_diagnostics(job)
    }

    /// Trusted periodic full verification. Identical errors and unchanged library
    /// contents create no new public job; explicit scans still have normal receipts.
    /// Completion always arrives, including when joined to a visible scan.
    pub fn verify_library(
        &self,
        previous: Option<ScanResult>,
    ) -> Result<(contract::JobId, crossbeam_channel::Receiver<ScanResult>), ContractError> {
        let (reply, rx) = crossbeam_channel::bounded(1);
        let (completed, completion) = crossbeam_channel::bounded(1);
        self.inner
            .tx
            .send(Msg::VerifyScan {
                previous,
                completed,
                reply,
            })
            .map_err(|_| core_stopped())?;
        Ok((rx.recv().map_err(|_| core_stopped())??, completion))
    }

    /// Trusted adapter repair scan, without a public command. Repeated identical
    /// failures do not create history/events; changed errors or applied changes do.
    /// The receiver completes even when the retry joins an ordinary visible scan.
    /// Its id correlates completions; it may have no public history row.
    pub fn retry_scan_directories(
        &self,
        directories: Vec<std::path::PathBuf>,
        previous: ScanResult,
    ) -> Result<(contract::JobId, crossbeam_channel::Receiver<ScanResult>), ContractError> {
        let (reply, rx) = crossbeam_channel::bounded(1);
        let (completed, completion) = crossbeam_channel::bounded(1);
        self.inner
            .tx
            .send(Msg::RetryScan {
                directories,
                previous,
                completed,
                reply,
            })
            .map_err(|_| core_stopped())?;
        let job = rx.recv().map_err(|_| core_stopped())??;
        Ok((job, completion))
    }

    /// Request a post-registration baseline unless this native registration generation
    /// was already covered by a scan preparation. The actor checks the token in command
    /// order, including when AddRoot is between committing its root and preparing it.
    /// The trusted adapter must invalidate the token when it closes/replaces the watch.
    pub fn ensure_root_scan(
        &self,
        root: contract::RootId,
        covered: Arc<ScanCoverage>,
    ) -> Result<Option<contract::JobId>, ContractError> {
        let (reply, rx) = crossbeam_channel::bounded(1);
        self.inner
            .tx
            .send(Msg::EnsureRootScan {
                root,
                covered,
                reply,
            })
            .map_err(|_| core_stopped())?;
        rx.recv().map_err(|_| core_stopped())?
    }

    /// Register native watches synchronously before a scan reads these roots.
    /// The callback must never call core; dropping the registration unregisters it.
    pub fn register_scan_preparation(
        &self,
        handler: Arc<ScanPreparationHandler>,
    ) -> RootAccessRegistration {
        self.inner.root_access.register_scan(handler)
    }

    /// Stops accepting commands, finishes jobs, saves state and closes catalog storage.
    /// Dropping the last core handle alone stops core and releases its catalog reference.
    pub fn shutdown(&self) {
        if !self.inner.stopping.swap(true, Ordering::AcqRel) {
            let _ = self.inner.tx.send(Msg::Shutdown);
        }
    }

    /// Whether core and its catalog completed shutdown within the given timeout.
    pub fn shutdown_and_wait(&self, timeout: Duration) -> bool {
        self.shutdown_with_result(timeout).is_ok()
    }

    /// Waits for running jobs, the final state save and catalog close. A timeout leaves
    /// shutdown in progress. Keep a handle alive if the caller must avoid joining on drop.
    pub fn shutdown_with_result(&self, timeout: Duration) -> Result<(), ShutdownError> {
        self.shutdown();
        if !matches!(
            self.inner.stopped.recv_timeout(timeout),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        ) {
            return Err(ShutdownError::TimedOut);
        }
        if let Some(thread) = self
            .inner
            .thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = thread.join();
        }
        self.inner
            .shutdown_result
            .get()
            .cloned()
            .unwrap_or(Err(ShutdownError::ActorPanicked))
    }
}

/// The catalog's agents and lasting grants; expired grants are deleted. When they cannot
/// be read, agents are refused everything until core restarts (failing closed: a lost
/// `forbid` must not let an agent do what the user forbade), and `problems` says so.
fn load_permissions(catalog: &dyn Catalog, problems: &mut Vec<Problem>) -> Permissions {
    let stored = match catalog.load_permissions() {
        Ok(stored) => stored,
        Err(err) => {
            tracing::error!("cannot read agent permissions: {err}");
            if err.code == ErrorCode::LibraryDamaged {
                problems.push(problem(ErrorCode::LibraryDamaged, err.message.clone()));
            }
            problems.push(problem(
                ErrorCode::PermissionsUnavailable,
                format!("the agents and grants could not be read: {}", err.message),
            ));
            return Permissions::unavailable();
        }
    };
    let (permissions, expired) = Permissions::new(stored, unix_ms());
    let ids: Vec<_> = expired.iter().map(|g| g.id).collect();
    if !ids.is_empty()
        && let Err(err) = catalog.remove_grants(&ids)
    {
        tracing::warn!("cannot delete expired grants: {err}");
    }
    permissions
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn core_stopped() -> ContractError {
    ContractError::failed(ErrorCode::CoreStopped, "core has stopped")
}

fn permissions_unavailable() -> ContractError {
    ContractError::failed(
        ErrorCode::PermissionsUnavailable,
        "the agents and grants could not be read when core started",
    )
}

fn problem(code: ErrorCode, message: String) -> Problem {
    Problem {
        code,
        message,
        since_ms: unix_ms(),
    }
}

#[cfg(test)]
mod tests;
