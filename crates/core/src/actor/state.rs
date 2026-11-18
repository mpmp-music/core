//! Actor state groups, in the original field drop order.
use super::*;

pub(super) struct LimitState {
    pub(super) playback_clock: Arc<dyn mpmp_engine::SuspendClock>,
    pub(super) playback_deadline: Arc<mpmp_engine::PlaybackDeadline>,
    pub(super) stop_after_current: Option<u64>,
    pub(super) sleep_deadline: Option<u64>,
}

pub(super) struct OutputState {
    pub(super) output_events: Receiver<()>,
    pub(super) output_policy: policy::OutputPolicy,
    pub(super) output_at: Option<(u64, u64)>,
    /// Submitted resume frame of a rebuilt stream before its first content callback.
    /// Kept separately from the public audible pause anchor in seek_hint.
    pub(super) unrendered_resume: Option<(u64, u64)>,
    pub(super) output_failure_serial: u64,
    pub(super) output_error: Option<ContractError>,
    pub(super) output_settings: crate::contract::OutputSettings,
    pub(super) output_devices: Vec<crate::contract::OutputDeviceInfo>,
    pub(super) output_devices_generation: u64,
    pub(super) output_devices_poll_at: u64,
    pub(super) output_discovery_epoch: u64,
    pub(super) output_discovery_busy: Option<(u64, u64)>,
    pub(super) output_discovery_pending: bool,
    pub(super) output_discovery_task:
        Option<mpmp_engine::output_task::OutputTask<mpmp_engine::output::DiscoveredDevices>>,
    pub(super) output_discovery_retry: Option<(u64, usize)>,
    pub(super) output_capability_tasks: Vec<output_settings::PendingCapabilities>,
    pub(super) output_devices_error: Option<ContractError>,
    pub(super) output_discovery_lost: bool,
    pub(super) output_discovery_arrived: bool,
    pub(super) output_selected: Option<crate::contract::OutputDeviceInfo>,
    pub(super) output_selection_failure: Option<crate::contract::OutputFailureInfo>,
    pub(super) output_fallback: Option<crate::contract::OutputFallbackInfo>,
    pub(super) announced_output: Option<crate::contract::OutputStateInfo>,
}

pub(super) struct PlaybackState {
    pub(super) queue: Queue,
    pub(super) state: PlayState,
    pub(super) instances: HashMap<u64, Instance>,
    pub(super) next_instance: u64,
    /// The instance being heard.
    pub(super) playing: Option<Playing>,
    /// The instance the engine has been told comes next.
    pub(super) pending_next: Option<u64>,
    /// Entries that failed while being prepared as the next track during the current one;
    /// they are skipped, and reported once.
    pub(super) failed_next: Vec<EntryId>,
    /// Restored or stopped position, played from on the next resume.
    pub(super) resume_at: Option<(EntryId, u64)>,
    pub(super) seek_hint: Cell<Option<SeekHint>>,
}

pub(super) struct PublicationState {
    pub(super) subscribers: Vec<Subscriber>,
    /// The last event's `seq`, and the last non-`Position` one's (`Snapshot::seq`).
    pub(super) seq: u64,
    pub(super) state_seq: u64,
    /// The origin of the command being handled: stamped on the events it causes.
    pub(super) cause: Option<Origin>,
    /// The queue summary as last announced (`QueueChanged`).
    pub(super) announced_queue: QueueSummary,
}

pub(super) struct PersistenceState {
    pub(super) state_file: Option<StateFile>,
    pub(super) dirty: bool,
    pub(super) last_save: Instant,
}

pub(super) struct AudioState {
    /// Volume and ReplayGain settings, saved with the state.
    pub(super) volume: f32,
    pub(super) muted: bool,
    pub(super) replay_gain: ReplayGainSettings,
}

pub(super) struct ConfirmationState {
    /// Commands waiting for the user, by token.
    pub(super) held: HashMap<String, Held>,
    pub(super) host_cover_sources: Option<Vec<mpmp_library::CoverSource>>,
    pub(super) host_recovery_guard: Option<mpmp_library::CoverRecoveryGuard>,
    pub(super) host_undo_guard: Option<mpmp_library::UndoDescription>,
    pub(super) host_purge_guard: Option<mpmp_library::PurgeGuard>,
    pub(super) pending_read_revision: u64,
    pub(super) next_token: u64,
    pub(super) idempotency: IdempotencyCache,
}

pub(super) struct ScanState {
    /// Running scan jobs by the catalog's scan id.
    pub(super) jobs: HashMap<u64, Job>,
    pub(super) next_job: u64,
}

pub(super) struct AccessState {
    /// Agents and grants, shared with the handle (queries, authentication).
    pub(super) permissions: Arc<Mutex<Permissions>>,
    pub(super) locations: Arc<dyn Locations>,
    /// The time limit on an agent command's file system calls (`bounded`).
    pub(super) agent_fs: Limits,
    /// How long a confirmation waits for the user.
    pub(super) confirmation_ttl: Duration,
    /// Grant ids when the catalog hands out none (from the clock, like job ids).
    pub(super) next_grant: i64,
}

pub(super) struct RequestState {
    /// Requests whose checks run off the actor, by id.
    pub(super) waiting: HashMap<u64, Waiting>,
    pub(super) next_waiting: u64,
}

pub(super) struct RootState {
    pub(super) release_start: Option<ReleaseStart>,
    pub(super) releasing: HashMap<RootId, Releasing>,
    pub(super) retired_decodes: HashMap<RootId, Vec<RetiredDecode>>,
    pub(super) removed_root_notifications: HashMap<RootId, RemovalRecovery>,
}

pub(super) struct WorkerState {
    /// The waveform thread; stopped (and joined) when the actor ends.
    pub(super) waveforms: WaveformWorker,
    pub(super) loudness: crate::loudness_scan::LoudnessWorker,
    pub(super) loudness_job: Option<(JobId, Origin)>,
    pub(super) automatic_loudness: bool,
    /// Something changed that an automatic pass should look at.
    pub(super) automatic_dirty: bool,
    pub(super) awaiting_startup_scan: bool,
    pub(super) automatic_job: Option<JobId>,
    /// When to run a pass again: a track that failed is due, or a pass failed.
    pub(super) automatic_retry_at: Option<Instant>,
    /// Passes failed in a row (not tracks: those back off by themselves).
    pub(super) automatic_failures: u32,
    /// Rescan before that retry: a file was found changed or missing.
    pub(super) automatic_rescan: bool,
    /// The current and next tracks as last told to the waveform thread.
    pub(super) waveform_tracks: Vec<TrackId>,
    /// Running tag jobs, with the origin of the command that started each (its events
    /// carry it: guarantee 5).
    pub(super) tag_jobs: HashMap<JobId, Origin>,
    pub(super) cover_jobs: std::collections::HashSet<JobId>,
}
