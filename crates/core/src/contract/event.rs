//! Event envelopes and change notifications.

use super::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventEnvelope {
    /// Core-wide event order (guarantee 2).
    pub seq: u64,
    /// The command that caused the event (guarantee 5).
    pub origin: Option<Origin>,
    pub event: Event,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    /// Trusted UI-only recovery result. Never deliver candidate locations to themes, Agent or Remote.
    CoverRecoveryResolved {
        /// Track whose recovery was resolved.
        track: TrackId,
        /// Human-readable trusted-host summary, not a structured inventory or input to parse.
        summary: String,
    },
    /// Cover resources were invalidated; re-read even when cover_revision is null or unchanged.
    CoversChanged {
        /// Affected track identities; older clients still receive track/library notifications.
        tracks: Vec<TrackId>,
    },
    /// The cached output inventory changed; re-read OutputDevices.
    OutputDevicesChanged {
        /// New discovery generation.
        generation: u64,
    },
    /// Authoritative active output facts, fallback or failure changed.
    OutputChanged {
        /// New output state; independent of preference-command success.
        output: OutputStateInfo,
    },
    /// Play state, current entry, shuffle, repeat or audio settings changed; carries the
    /// new snapshot (whose `seq` is this event's).
    StateChanged { snapshot: Box<Snapshot> },
    /// The entry is being heard (not merely decoded).
    TrackStarted {
        entry: EntryId,
        track: Box<TrackInfo>,
        /// Currently always set.
        signal_path: Option<Box<SignalPathInfo>>,
    },
    /// Where playback is: at once on every jump, periodically while playing (guarantee
    /// 10).
    Position { position: PositionInfo },
    /// The snapshot's queue summary changed (guarantee 11).
    QueueChanged { queue: QueueSummary },
    /// The library changed: a scan finished, or a folder was added or removed (`AddRoot`
    /// emits it at once and again when its scan finishes), or a tag job wrote files (after
    /// its `JobFinished`: albums and artists may have changed). Re-read library views
    /// (folders, albums, tracks).
    LibraryChanged,
    /// A play was recorded in history.
    HistoryChanged,
    /// Playlists were created, edited, deleted, restored or purged; empty when it is not
    /// known which (emptying the trash): re-fetch the list.
    PlaylistsChanged { playlists: Vec<PlaylistId> },
    /// Data of these tracks changed: user data (rating, loved), and tags a tag job wrote
    ///; loudness values and other data in later versions. Re-read them where
    /// shown.
    TracksChanged { tracks: Vec<TrackId> },
    /// The action journal changed: an action was recorded, or one was undone. Re-fetch
    /// `Actions` if you show it.
    ActionsChanged,
    /// A job's progress: `done` of `total` units (files, for a scan and a tag job); `total`
    /// is `null` while not known yet.
    JobProgress {
        job: JobId,
        kind: JobKind,
        done: u32,
        total: Option<u32>,
    },
    /// A job ended; what it did is visible to queries from now on (guarantee 3).
    JobFinished {
        job: JobId,
        kind: JobKind,
        outcome: JobOutcome,
    },
    /// An entry could not be played and is skipped. `upcoming`: it failed while being
    /// prepared as the next track, so what is heard now is fine; it is skipped when
    /// reached. `code`: `file_missing`, `track_unavailable`, `track_not_found`,
    /// `open_failed`, `unsupported_format`, `decode_error`, `output_failed` …
    TrackFailed {
        entry: EntryId,
        code: ErrorCode,
        message: String,
        #[serde(default)]
        upcoming: bool,
    },
    /// A command waits for the user: show `summary` (or render `command`) and answer
    /// with `Confirm` or `Reject`. Delivered to UI subscribers only.
    ConfirmationRequested {
        confirmation: Box<PendingConfirmation>,
    },
    /// A pending confirmation was answered, expired or withdrawn (`outcome.reason`).
    ConfirmationResolved { outcome: ConfirmationOutcome },
    /// This subscriber fell behind and events were dropped: rebuild from `snapshot`
    /// (guarantee 4).
    Resync { snapshot: Box<Snapshot> },
    /// Grants were added, revoked or expired, or an agent's grants removed with it:
    /// re-fetch `Permissions` if you show them.
    PermissionsChanged,
    /// An agent was registered or removed, or got a new token: re-fetch `Agents` if you
    /// show them (seeing an agent does not announce anything).
    AgentsChanged,
    /// The waveform of a track was computed in the background and can now be queried
    /// (`Waveform`). Origin `null`. Feature `waveform`.
    WaveformReady { track: TrackId },
    /// An automatic condition consumed both limits; origin is null and state is published first.
    PlaybackLimitReached {
        /// Frozen reason identifying the condition, including end_of_current for the natural boundary.
        reason: PlaybackLimitReason,
        /// Transport effect; pause never starts stopped playback.
        action: PlaybackLimitAction,
        /// Affected entry, or null if no current entry exists.
        entry: Option<EntryId>,
    },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}
