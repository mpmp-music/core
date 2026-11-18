//! Commands and their playback, queue, playlist, and tag inputs.

use super::*;

/// Changes to state; reads are queries. Unknown fields are rejected (`unsupported` /
/// `unknown_field`). Each variant's schema carries `x-risk` and the default policies
/// `x-origins` (guarantee 12).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Command {
    /// Replace the queue with `context` and play from it: `start_track` if given (its
    /// first occurrence; not in the context → `not_found`), else the track at index
    /// `start` (default 0; past the end → `out_of_range`). An empty context is
    /// `not_found` / `empty_context`. A playlist context's `start_item` takes precedence
    /// over both selectors when present.
    Play {
        context: PlayContext,
        #[serde(default)]
        start: u32,
        #[serde(default)]
        start_track: Option<TrackId>,
    },
    /// Jump to an entry already in the queue.
    PlayEntry { entry: EntryId },
    /// Pause playback; no-op (`changed: false`) unless playing.
    Pause,
    /// Continue: paused → playing; stopped → play from the remembered position or
    /// current entry (including stopped navigation), what follows a removed current
    /// entry, or the start of the queue; no-op on an empty queue.
    Resume,
    /// `Pause` while playing, else `Resume`.
    TogglePause,
    /// Stop playback. With `paused_navigation`: remember the current entry at
    /// position zero (also shown in the snapshot); a later `Resume` starts at its
    /// beginning, unless a subsequent Seek changes the resume position. An entry
    /// removed from the queue still resumes with its successor. History retains
    /// the actual time heard and the Stopped end reason. Ordinary shutdown still saves
    /// the real position; only this command resets it.
    Stop,
    /// Skip to the next entry in play order (shuffled order while shuffled; wraps with
    /// repeat `all`). Repeat `one` applies only when a track ends by itself: `Next` still
    /// moves on (past the last entry it stops, as with repeat `off`).
    /// With `paused_navigation`: while paused or stopped, select at position zero
    /// and keep that state; `StateChanged` announces the selection, `TrackStarted`
    /// only when heard. Stopped selection never opens output or probes files; Resume
    /// starts the last selection and handles file failures normally. With no successor
    /// while stopped, nothing changes (`changed: false`), including a never-played,
    /// enqueue-only queue with no current entry. Playing behavior is unchanged.
    Next,
    /// Within the first 3 seconds of a track: go to the previous entry in play order (as
    /// `Next`, repeat `one` does not hold it); later, or on the first entry: restart the
    /// current track.
    /// With `paused_navigation`: while paused or stopped, selection and restart
    /// keep that state. Stopped uses the remembered resume position for the 3-second
    /// threshold; restarting only resets that position. Selection does not record
    /// history or increment play count; it persists the new entry at position zero.
    Previous,
    /// Seek within the current entry. With `entry`, only if it is still the current one
    /// (else `conflict` / `entry_not_current`); past the track's known duration (or past
    /// 2^53 − 1) is `out_of_range`. Paused or stopped, it moves where playback resumes;
    /// the play state does not change.
    Seek {
        position_ms: u64,
        #[serde(default)]
        entry: Option<EntryId>,
    },
    /// Adds tracks (all must exist, else `not_found` with `details.missing`); empty is a
    /// no-op.
    Enqueue {
        tracks: Vec<TrackId>,
        #[serde(default)]
        at: InsertAt,
        /// The queue's revision the client last saw; another one is `conflict` /
        /// `queue_conflict` and nothing changes. Omitted: no check.
        #[serde(default)]
        expected_revision: Option<QueueRevision>,
        /// only with `at: next`. Atomically require this current entry,
        /// even for an empty batch; otherwise `conflict` / `entry_not_current`.
        /// Omitted or null: no anchor check. With another position: `invalid`.
        #[serde(default)]
        expected_current: Option<EntryId>,
    },
    /// Removes entries; ids not in the queue are ignored (the receipt lists what was
    /// removed). Removing the entry being heard lets it play to its end; playback then
    /// continues with what followed it.
    Remove {
        entries: Vec<EntryId>,
        /// The queue's revision the client last saw; another one is `conflict` /
        /// `queue_conflict` and nothing changes. Omitted: no check.
        #[serde(default)]
        expected_revision: Option<QueueRevision>,
    },
    /// Moves an entry to final index `to` (< queue length).
    Move {
        entry: EntryId,
        to: u32,
        /// The queue's revision the client last saw; another one is `conflict` /
        /// `queue_conflict` and nothing changes. Omitted: no check.
        #[serde(default)]
        expected_revision: Option<QueueRevision>,
    },
    /// Empties the queue and its play context. The track being heard plays to its end,
    /// then playback stops.
    Clear {
        /// The queue's revision the client last saw; another one is `conflict` /
        /// `queue_conflict` and nothing changes. Omitted: no check.
        #[serde(default)]
        expected_revision: Option<QueueRevision>,
    },
    /// Shuffle on or off. Turning it on draws a new play order starting at the current
    /// entry; every entry plays once.
    SetShuffle { shuffle: bool },
    /// `off`, `all` (wrap around at the end) or `one` (a track that ends by itself plays
    /// again; `Next` / `Previous` still move).
    SetRepeat { mode: RepeatMode },
    /// Undo the queue change that produced `revision`, only while the queue's revision is
    /// still `revision` (else `conflict` / `queue_conflict`). One step: the undo itself
    /// cannot be undone. The queue is not in the action journal.
    Undo { revision: QueueRevision },
    /// Adds a library folder and scans it (the receipt's `root` and `job`). Emits
    /// `LibraryChanged` at once (the folder list changed) and again when the scan finishes.
    /// Recorded (`root_add`); adding a folder that is already a library folder changes
    /// nothing (`changed: false`, no action, no scan).
    AddRoot {
        /// The folder's location, an opaque string defined by the platform layer (desktop:
        /// its absolute path; mobile: e.g. a platform folder URI).
        path: String,
    },
    /// Removes a library folder; its tracks become unavailable (history is kept) and
    /// `LibraryChanged` follows. Recorded (`root_remove`): undoing it brings the folder
    /// back and rescans. Removing a folder that is removed already changes nothing
    /// (`changed: false`).
    RemoveRoot { root: RootId },
    /// Rescans the library (the receipt's `job`; a running scan is joined).
    Rescan,
    /// Rates tracks. Stored in the library only, not in the files. Recorded (`set_rating`,
    /// undoable); tracks that already have that rating are left out of the action and of
    /// `TracksChanged`.
    SetRating {
        tracks: Vec<TrackId>,
        /// 1–5 stars, or `null` to clear the rating; 0 is `out_of_range`. Required: send
        /// `null` explicitly to clear (a missing `rating` is `invalid_request`).
        #[serde(deserialize_with = "nullable")]
        #[schemars(with = "Nullable<u8>")]
        rating: Option<u8>,
    },
    /// Marks tracks loved or not; like `SetRating` (recorded as `set_loved`).
    SetLoved { tracks: Vec<TrackId>, loved: bool },
    /// The user approves a pending command (`ConfirmationRequested`); UI only. Tokens are
    /// single-use and expire after 5 minutes. The receipt's `confirmed.result` is the
    /// held command's result.
    Confirm { token: String },
    /// The user refuses a pending command; UI only. Its requester's retries get
    /// `forbidden` / `confirmation_rejected`.
    Reject { token: String },
    /// Sets the volume slider, 0.0–1.0 (else `out_of_range`); in `software` mode the gain
    /// is `volume³`.
    SetVolume { volume: f32 },
    /// Mute or unmute; the volume is kept.
    SetMuted { muted: bool },
    /// Chooses how the volume slider acts: `software` gain, or the `system` volume. `system` is not implemented yet: `unsupported` / `not_implemented`.
    SetVolumeMode { mode: VolumeMode },
    /// Changes the given ReplayGain settings; omitted ones stay as they are. Pre-amps
    /// are −24 to 24 dB.
    SetReplayGain {
        #[serde(default)]
        mode: Option<ReplayGainMode>,
        #[serde(default)]
        preamp_db: Option<f32>,
        #[serde(default)]
        fallback_preamp_db: Option<f32>,
        #[serde(default)]
        prevent_clipping: Option<bool>,
    },
    // Playlists (docs/design/queries-playlists-undo.md). Edits take the playlist's own revision.
    /// Creates a playlist (the receipt's `playlist`), at revision 0. A smart query is
    /// type-checked (`invalid_query`); a blank name is `blank_name`. Recorded
    /// (`playlist_create`): its undo moves the playlist to the trash.
    CreatePlaylist { name: String, kind: PlaylistKind },
    /// Renames a playlist; the same name changes nothing. A playlist in the trash is
    /// `playlist_in_trash`.
    RenamePlaylist {
        playlist: PlaylistId,
        name: String,
        /// The playlist's revision the client last saw; another one is `conflict` /
        /// `playlist_conflict` and nothing changes. Omitted: no check.
        #[serde(default)]
        expected_revision: Option<PlaylistRevision>,
    },
    /// Moves the playlist to the trash; `RestorePlaylist` brings it back. One already in
    /// the trash stays there (`changed: false`). Playlists remain restorable regardless
    /// of age, until the user confirms permanent deletion.
    DeletePlaylist { playlist: PlaylistId },
    /// Takes a playlist out of the trash, as it was. One that is not in the trash stays
    /// as it is (`changed: false`).
    RestorePlaylist { playlist: PlaylistId },
    /// Permanently deletes playlists in the trash; `null` (or omitted) empties it. All or
    /// nothing: an id not in the trash fails the whole command (`not_in_trash`). It cannot
    /// be undone: not recorded as an action, but an audit entry (`purge`) lists it in
    /// `Actions` when it deleted anything. From an agent it needs the user's confirmation
    /// every time, unless the user forbade it; no grant can allow it.
    PurgePlaylists {
        #[serde(default)]
        playlists: Option<Vec<PlaylistId>>,
    },
    /// Adds tracks to a manual playlist (not to a smart one). `index`: where to insert (≤ length); `null` (or omitted)
    /// appends. All tracks must exist.
    AddToPlaylist {
        playlist: PlaylistId,
        tracks: Vec<TrackId>,
        #[serde(default)]
        index: Option<u32>,
        /// The playlist's revision the client last saw; another one is `conflict` /
        /// `playlist_conflict` and nothing changes. Omitted: no check.
        #[serde(default)]
        expected_revision: Option<PlaylistRevision>,
    },
    /// Items not in the playlist are ignored (the receipt lists what was removed).
    RemoveFromPlaylist {
        playlist: PlaylistId,
        items: Vec<PlaylistItemId>,
        /// The playlist's revision the client last saw; another one is `conflict` /
        /// `playlist_conflict` and nothing changes. Omitted: no check.
        #[serde(default)]
        expected_revision: Option<PlaylistRevision>,
    },
    /// Moves an item to final index `to` (< length).
    MovePlaylistItem {
        playlist: PlaylistId,
        item: PlaylistItemId,
        to: u32,
        /// The playlist's revision the client last saw; another one is `conflict` /
        /// `playlist_conflict` and nothing changes. Omitted: no check.
        #[serde(default)]
        expected_revision: Option<PlaylistRevision>,
    },
    /// Smart playlists only: sets the query and `max_tracks`. The same query and limit
    /// changes nothing. Options added in later versions keep their current value when
    /// omitted.
    SetSmartQuery {
        playlist: PlaylistId,
        query: TrackQuery,
        /// The most tracks the playlist lists; omitted or `null`: no limit.
        #[serde(default)]
        max_tracks: Option<u32>,
        /// The playlist's revision the client last saw; another one is `conflict` /
        /// `playlist_conflict` and nothing changes. Omitted: no check.
        #[serde(default)]
        expected_revision: Option<PlaylistRevision>,
    },
    /// Undo the playlist's latest edit; `revision` must be its current revision. Repeat
    /// (with the new revision) to undo older edits: the last 50 edits of the last 30 days
    /// can be undone, then `nothing_to_undo`. A shortcut for `UndoAction` of that edit;
    /// the receipt's `action` is the undo.
    UndoPlaylist {
        playlist: PlaylistId,
        revision: PlaylistRevision,
    },
    /// Creates a manual playlist from an M3U/M3U8 file; tracks outside the library are
    /// reported, not added (for an agent, only lines that look like media are listed: see
    /// `M3uReport.missing`). `name` defaults to the file name. A file that does not exist
    /// is `not_found` / `file_not_found`; one that is not a regular file (a folder, a FIFO,
    /// a device) is `invalid` / `not_a_regular_file`. Recorded (`playlist_import`).
    ImportM3u {
        /// The file's location, an opaque string defined by the platform layer (desktop:
        /// its absolute path).
        path: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// Writes a playlist to an M3U/M3U8 file, replacing an existing file. It cannot be
    /// undone: not recorded as an action, but an audit entry (`file_write`) lists it in
    /// `Actions` when it replaced a file, and whenever an agent wrote one.
    ExportM3u {
        playlist: PlaylistId,
        /// The file's location, an opaque string defined by the platform layer (desktop:
        /// its absolute path).
        path: String,
    },
    /// Undoes one recorded action (guarantee 15): its receipt's `action` is the undo, of
    /// kind `undo` (or `redo` when undoing an undo). Fails with `conflict` /
    /// `action_conflict` when another action changed some of the same objects since
    /// (`details.changed` lists them), `invalid` / `already_undone`, or `not_found` /
    /// `action_not_found` (unknown or expired). Undoing a folder's removal rescans
    /// (the receipt's `job`). An undo is judged by what it does, which `x-origins` cannot
    /// show (guarantee 12): undoing a music-folder change (`root_add`, `root_remove`, or
    /// an undo of one) is a folder change, so agents need the user's confirmation and
    /// remotes are refused (`path_not_allowed`), as for `AddRoot`; undoing a tag edit (or
    /// an undo of one) rewrites files, as `EditTags` does (agents and remotes need the
    /// user's confirmation, grants for `edit_tags` apply), and runs as a job (the receipt's
    /// `job`, no `action`; the undo's action is in its `tag_edit` report). It restores the
    /// complete file for new edits. A missing file or changed stamp fails the batch with
    /// `action_conflict`. An unchanged stamp with a different digest reports `file_changed`
    /// and leaves the batch untouched. An open playback file fails it with `file_in_use`
    /// (`retryable`); undoing a change to library data (ratings, playlists, also an import)
    /// is a library data change.
    UndoAction { action: ActionId },
    /// Trusted host UI-only recovery. Always confirms; third-party clients are unsupported. selection is obtained from the host private trusted inventory, never parsed from summary.
    ResolveCoverRecovery {
        /// Track with durable recovery facts.
        track: TrackId,
        /// Reviewed recovery action; cleanup requires separate confirmation.
        resolution: CoverRecoveryResolution,
        /// Optional candidate binding supplied by the trusted host inventory; not available through third-party public queries.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selection: Option<CoverRecoverySelection>,
    },
    /// Start an explicit cover-edit job after a fresh preview and appropriate authorization.
    EditCover {
        /// Explicit target IDs, deduplicated and bounded by max_cover_edit_tracks.
        tracks: Vec<TrackId>,
        /// Set a validated image or remove the primary front-cover representation.
        change: CoverChange,
    },
    /// Rewrites the tags of the tracks' files (feature `edit_tags`; preview it with
    /// `TagEditPreview`). Runs as a job: the receipt has `job` (no `action`), the files are
    /// written one by one with `JobProgress`, and `JobFinished` reports what was written,
    /// skipped and could not be written, with the action that records it
    /// (its `tag_edit` report); what was written is read again, visible to queries from
    /// then on and announced by `TracksChanged` and `LibraryChanged` before and after the
    /// `JobFinished` (guarantee 3). Recorded (`tag_edit`, one action for every file
    /// written, none when none was): `UndoAction` restores the prior file bytes, also
    /// as a job. Conflicts found by undo preflight prevent all writes in the batch.
    /// Conflicts found after publication starts can leave a partially completed undo.
    /// A file is written only if it is as the library last read it (else `skipped`
    /// with `file_changed`); current and preloaded files are skipped (`file_in_use`).
    /// They are never queued for a later write: the caller must retry after playback
    /// releases them. Other skips include
    /// unavailable tracks, missing files and formats whose tags are not written
    /// (`unsupported_format`); one that has the values already is left alone and not
    /// listed. Only the file's primary tag is written (FLAC, Ogg: Vorbis comments; MP3,
    /// WAV, AIFF: ID3v2; MP4: `ilst`), in its own type; other tags, pictures and keys the
    /// changes do not name are kept. Ratings are never written to files. All tracks must
    /// exist (else `not_found`), at most 1000 (`out_of_range`); bad changes are `invalid`
    /// / `invalid_tag`. No tracks or no changes change nothing (`changed: false`, no job).
    /// Agents need the user's confirmation unless a grant allows it (class `file_write`;
    /// its targets are the tracks' files).
    EditTags {
        tracks: Vec<TrackId>,
        changes: Vec<TagChange>,
    },
    /// Release a library folder for safe removal (`release_root`). If current
    /// playback or preloading uses it, stop the entire transport without advancing.
    /// Preserve queue entries, revisions, history and library metadata. Wait for existing
    /// scans, analysis, tag jobs, covers, lyrics and all canceled/replaced prefetch opens
    /// and their decoders to close their files, and block new
    /// physical access to this root until `ResumeRoot`, `RemoveRoot` or core restart. Trusted platform
    /// adapters synchronously close native directory watchers before success. A close
    /// failure returns `failed` / `io` (retryable); the root stays suspended: retry release
    /// to prove closure or explicitly resume. `Roots.released` alone is not an eject ACK.
    /// Lease draining runs off the actor, with a 5-second deadline. Timeout returns
    /// retryable `failed` / `io` and keeps suspension; retry release to prove closure.
    /// Concurrent release/resume of the same root returns retryable `failed` / `job_busy`.
    /// RemoveRoot ends suspension so re-adding/undoing removal does not inherit it.
    /// Scans abandon released roots at file/CUE/batch boundaries without marking missing.
    /// Shutdown discards pending answers (`core_stopped`) without joining drain workers.
    /// Scans skip it;
    /// file reads/writes fail or skip with `root_released`. No filesystem unmount/eject.
    /// Returns `receipt.root`, `changed` (false if already released), no job or action.
    /// Unknown/removed root: `not_found` / `root_not_found`. Only this root's library
    /// access is covered, not other roots (including symlink aliases), apps, or an on-drive
    /// library database/cache. Agent: local_path rules
    /// (confirmation by default, scoped grants apply); remotes/media controls forbidden.
    ReleaseRoot { root: RootId },
    /// Explicitly restore a released folder's file access. Returns `root` and `changed`.
    /// Schedules a scan and automatic analysis when changed; never resumes playback.
    /// A scan failure is reported normally and does not re-release the root.
    /// Idempotent; unknown/removed root and origin rules are as for `ReleaseRoot`.
    ResumeRoot { root: RootId },
    // Agent permissions (feature `permissions`): the user's commands, UI only.
    /// Registers an agent under `name` (unique; letters, digits, `-`, `_`, `.`, so never
    /// `:`; at most 64 characters, else `invalid_agent_name`; taken: `already_exists`).
    /// The receipt's `agent_token` is the agent's token, shown this once; the agent's
    /// adapter presents it to core, which then knows the agent by `name`. Never kept for
    /// retries (guarantee 13): a retry registers again (`already_exists`).
    RegisterAgent { name: String },
    /// Replaces an agent's token (the receipt's `agent_token`); the old one stops working
    /// at once (adapters check their connections again on `AgentsChanged`). Unknown agent:
    /// `not_found` / `agent_not_found`. Never kept for retries (guarantee 13): a retry
    /// hands out another token.
    RenewAgentToken { agent: String },
    /// Removes an agent: its token stops working, all its grants are revoked, and its
    /// commands waiting for confirmation are withdrawn (`ConfirmationResolved`, reason
    /// `withdrawn`).
    RemoveAgent { agent: String },
    /// Grants an agent an operation (or forbids it) without asking each time (guarantee
    /// 12). `forbid` refuses it (`forbidden` / `permission_denied`), also what the agent
    /// may do without asking by default (`x-origins`: `allow`), and wins over `allow`.
    /// `allow` runs without asking what would otherwise need the user's confirmation
    /// (`x-origins`: `confirm`); an `allow` that could never apply is `not_grantable`
    /// (`library_edit`, commands agents run without asking anyway, the user's
    /// own commands; `undo_action` may be allowed, for undoing folder changes and tag
    /// edits), and so is permanently deleting library data. A `forbid` of the user's own
    /// commands is `not_grantable` too. Only `local_path`, `file_write` and the commands
    /// that act on something (`add_root`, `remove_root`, `import_m3u`, `export_m3u`,
    /// `undo_action`, `edit_tags`) take a scope other than `all` (else
    /// `invalid_request`). The receipt's `grant` is the
    /// new grant. A grant does not answer a confirmation already pending: `Confirm` that
    /// too.
    GrantPermission {
        /// The agent's name (`Origin` `agent.name`); it need not be registered. A
        /// registered agent's name, or `<platform>:<name>` for an agent an adapter names
        /// itself (lowercase ASCII letters and digits, a `:`, then such a name); else
        /// `invalid_agent_name`.
        agent: String,
        /// Only this session of the agent (at most 128 characters), and then `until` must
        /// be `session` (a session ends with its connection, at the latest when core
        /// stops); `null` (or omitted): every session. A session that has ended already
        /// (`end_agent_session`; core remembers the last 10 000 since it started) is
        /// `invalid` / `session_ended`.
        #[serde(default)]
        session: Option<String>,
        operation: Operation,
        scope: Scope,
        until: Expiry,
        effect: Effect,
    },
    /// Revokes a grant. Unknown or expired: `not_found` / `grant_not_found`.
    RevokePermission { grant: GrantId },
    /// Commit an identity preference, including unseen/offline IDs. Capabilities never reject this command; failures use OutputState. Agent/Remote confirm by default.
    SetOutputDevice {
        /// Requested opaque identity or system_default. An unseen ID may remain persisted and unavailable; choosing system_default recovers the default route.
        selection: OutputDeviceSelection,
        /// Optional optimistic guard shared by device and mode settings.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<u64>,
    },
    /// Only build/backend-wide unsupported configurations fail before commit with output_mode_unsupported. Device-specific failures use OutputState.failure/fallback. Agent/Remote confirm by default.
    SetOutputMode {
        /// Optional requested access mode; omission retains the preference.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<OutputShareMode>,
        /// Optional explicit runtime fallback policy; omission retains the preference.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fallback: Option<OutputFallbackPolicy>,
        /// Optional requested rate policy; omission retains the preference.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_policy: Option<OutputRatePolicy>,
        /// Optional shared output preference revision guard; empty updates do not increment it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<u64>,
    },
    /// Bind or cancel a run-local stop boundary before acknowledging success.
    SetStopAfterCurrent {
        /// True arms the current accepted playback instance; false cancels its boundary.
        enabled: bool,
        /// Optional guard against arming a different current queue entry.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_entry: Option<EntryId>,
    },
    /// Replace or cancel the elapsed-time deadline; a missing duration_ms is invalid.
    SetSleepTimer {
        /// Required nullable duration: null cancels; otherwise 1..max_sleep_timer_ms. Clock precision is reported in SleepTimerInfo.
        #[serde(deserialize_with = "nullable")]
        #[schemars(with = "Nullable<u64>")]
        duration_ms: Option<u64>,
    },
    /// Decode-only fallback for a command this build does not know; never send it. Core
    /// answers an unknown command with `unsupported` / `unknown_command`.
    #[serde(other)]
    Unknown,
}

/// One change of `EditTags` / `TagEditPreview`. Keys are the library's
/// (`TrackInfo.tags`): upper case (sent in any case), the name the library shows
/// (`ALBUMARTIST`, `TRACKNUMBER`, a format's own key such as a `TXXX` description);
/// values are trimmed. Keys and values together are at most 256 KiB per edit.
/// CUESHEET is not editable; CUE metadata is written through its sheet in a future version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TagChange {
    /// The key gets exactly these values (1 to 64), replacing those it has.
    Set { key: String, values: Vec<String> },
    /// The key is removed.
    Remove { key: String },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlaylistKind {
    Manual,
    /// The first `max_tracks` tracks (all without it) matching `query`, recomputed
    /// whenever it is read or played.
    Smart {
        query: TrackQuery,
        #[serde(default)]
        max_tracks: Option<u32>,
    },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InsertAt {
    #[default]
    End,
    /// Right after the current entry.
    Next,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// Where a queue came from; kept with the queue and in play history.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlayContext {
    /// An album's available tracks; an unknown key is `not_found` / `album_not_found`.
    Album {
        /// The album's `AlbumSummary.key`: opaque, and it may change when tags or the
        /// grouping rules change.
        key: String,
    },
    /// These tracks (all must exist: `not_found` with `details.missing`).
    Tracks {
        tracks: Vec<TrackId>,
    },
    Search {
        text: String,
    },
    /// The tracks matching a query, in its order, as of when playback starts.
    Query {
        query: TrackQuery,
    },
    /// A playlist's available tracks, copied into the queue: later edits to the playlist
    /// do not change the queue.
    Playlist {
        playlist: PlaylistId,
        /// start at this exact manual playlist item, ahead of `start_track`
        /// and `start`. Unavailable, absent, foreign or beyond the context limit:
        /// `conflict` / `playlist_conflict`. Smart playlist: `playlist_kind_mismatch`.
        #[serde(default)]
        start_item: Option<PlaylistItemId>,
        /// check this revision in the same snapshot as item lookup and
        /// queue expansion; mismatch is `conflict` / `playlist_conflict`.
        /// Omitted or null: no revision check.
        #[serde(default)]
        expected_revision: Option<PlaylistRevision>,
    },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}
