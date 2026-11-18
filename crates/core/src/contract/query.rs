//! Read queries, result variants, and pagination.

use super::*;

/// Reads. Unknown fields are rejected, as for commands. Paged queries take `offset`
/// (default 0) and `limit`, which is cut to the query's page cap; `limit: 0` only counts
/// (guarantee 8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum Query {
    /// The player's state: play state, current track, position, queue summary, settings.
    Snapshot,
    /// Authoritative job state, including its full terminal report or failure. Feature
    /// `job_queries`. Read-only, available to agents and themes. Unknown or evicted ID:
    /// `not_found` / `job_not_found`. Only available during this core run.
    Job { job: JobId },
    /// Job summaries newest ID first: all running jobs and at least the newest 1000
    /// terminal jobs, subject to the retained-history byte budget. Includes automatic loudness jobs. Feature
    /// `job_queries`. No full outcomes; use Job for a complete report. Read-only; history
    /// is not persisted across restarts. Offsets are
    /// live and may shift as jobs start or are evicted; deduplicate by ID when paging.
    Jobs {
        #[serde(default)]
        offset: u32,
        /// Cut to limits.max_page; 0 only counts.
        limit: u32,
    },
    /// The queue in queue order, every entry (unavailable ones marked).
    QueuePage {
        /// Index of the first row to return (default 0).
        #[serde(default)]
        offset: u32,
        /// The most rows to return, cut to `limits.max_track_page`; 0 only counts.
        limit: u32,
    },
    /// Matching tracks, albums and artists with totals. Omit `kind` for the compatible
    /// mixed first page; select a kind to page its complete relevance-ordered results.
    /// Ties use track ID, album key or normalized artist name.
    Search {
        /// Only selects the artists branch; default retains album-group semantics.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artist_view: Option<ArtistView>,
        text: String,
        /// Omit for mixed results. Other lists are empty for a selected kind.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<SearchKind>,
        #[serde(default)]
        /// Nonzero requires kind; mixed search rejects it as invalid/out_of_range,
        /// field query.offset.
        offset: u32,
        /// The most rows of each kind, cut to `limits.max_track_page`; 0 only counts.
        limit: u32,
    },
    /// The album grid, a page at a time (available tracks only).
    Albums {
        #[serde(default)]
        sort: AlbumSort,
        /// Index of the first row to return (default 0).
        #[serde(default)]
        offset: u32,
        /// The most rows to return, cut to `limits.max_page`; 0 only counts.
        limit: u32,
    },
    /// One album with its available tracks. Without offset and limit, returns all tracks.
    /// Explicit pagination is capped at `max_track_page`. Total: `summary.track_count`.
    /// Stable disc, track number, path and ID order. Unknown key: `not_found` /
    /// `album_not_found`.
    Album {
        /// The album's `AlbumSummary.key`.
        key: String,
        #[serde(default)]
        offset: Option<u32>,
        /// Cut to `limits.max_track_page`; 0 only counts. With offset only: the page cap.
        /// Omit both offset and limit for the complete album (v1.0 behavior).
        #[serde(default)]
        limit: Option<u32>,
    },
    /// Playlists by name; `deleted: true` lists the trash, most recently deleted first.
    Playlists {
        #[serde(default)]
        deleted: bool,
        /// Index of the first row to return (default 0).
        #[serde(default)]
        offset: u32,
        /// The most rows to return, cut to `limits.max_page`; 0 only counts.
        limit: u32,
    },
    /// A playlist's tracks in order. Manual playlists include unavailable tracks
    /// (`available: false`); smart playlists are the query's current result (available
    /// tracks only, unless the query mentions `available`; none while the stored query is
    /// invalid, see `PlaylistSummary.query_error`). Unknown playlist: `not_found`.
    PlaylistItems {
        playlist: PlaylistId,
        /// Index of the first row to return (default 0).
        #[serde(default)]
        offset: u32,
        /// The most rows to return, cut to `limits.max_track_page`; 0 only counts.
        limit: u32,
        /// Tags to add to each track (`TrackInfo.tags`).
        #[serde(default)]
        tags: Vec<String>,
    },
    /// Album artists (falling back to track artists), matching the album grid.
    Artists {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<ArtistView>,
        /// Index of the first row to return (default 0).
        #[serde(default)]
        offset: u32,
        /// The most rows to return, cut to `limits.max_page`; 0 only counts.
        limit: u32,
    },
    /// An artist's albums; answered with an `albums` result.
    ArtistAlbums {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<ArtistView>,
        name: String,
        #[serde(default)]
        sort: AlbumSort,
        /// Index of the first row to return (default 0).
        #[serde(default)]
        offset: u32,
        /// The most rows to return, cut to `limits.max_page`; 0 only counts.
        limit: u32,
    },
    /// Plays, newest first; every recorded play (also of tracks now unavailable).
    History {
        /// Index of the first row to return (default 0).
        #[serde(default)]
        offset: u32,
        /// The most rows to return, cut to `limits.max_track_page`; 0 only counts.
        limit: u32,
    },
    /// The library folders (active ones), with their track counts (not paged).
    Roots,
    /// Tracks matching a query, a page at a time.
    Tracks {
        #[serde(default)]
        query: TrackQuery,
        /// Index of the first row to return (default 0).
        #[serde(default)]
        offset: u32,
        /// The most rows to return, cut to `limits.max_track_page`; 0 only counts.
        limit: u32,
        /// Tags to add to each track (`TrackInfo.tags`).
        #[serde(default)]
        tags: Vec<String>,
    },
    /// Read at most `limits.max_track_page` IDs, preserving input order and duplicates.
    /// Missing IDs are omitted; unavailable tracks are returned with `available:false`.
    /// More IDs fail with `out_of_range` (no partial answer). Empty input returns [].
    TrackRows {
        tracks: Vec<TrackId>,
        /// Tags to add to each track (`TrackInfo.tags`).
        #[serde(default)]
        tags: Vec<String>,
    },
    /// The distinct values of `field` among the tracks matching `filter`, with counts:
    /// one level of a library tree.
    Facet {
        field: Field,
        #[serde(default)]
        filter: Filter,
        /// Index of the first row to return (default 0).
        #[serde(default)]
        offset: u32,
        /// The most rows to return, cut to `limits.max_page`; 0 only counts.
        limit: u32,
    },
    /// The action journal, newest first (guarantee 15): who changed what, and whether it
    /// can still be undone. `origin` keeps the actions of one origin (or kind of origin).
    Actions {
        #[serde(default)]
        origin: Option<OriginFilter>,
        /// Index of the first row to return (default 0).
        #[serde(default)]
        offset: u32,
        /// The most rows to return, cut to `limits.max_page`; 0 only counts.
        limit: u32,
    },
    /// The registered agents, by name (not paged). Feature `permissions`.
    Agents,
    /// The grants in effect (expired ones are gone), oldest first; `agent` keeps one
    /// agent's. Feature `permissions`.
    Permissions {
        #[serde(default)]
        agent: Option<String>,
        /// Index of the first row to return (default 0).
        #[serde(default)]
        offset: u32,
        /// The most rows to return, cut to `limits.max_page`; 0 only counts.
        limit: u32,
    },
    /// A track's lyrics, read from a `.lrc` file next to the track, else from its tags
    /// (not paged). The result's `lyrics` is `null` when there are none. Unknown track:
    /// `not_found` / `track_not_found`. Feature `lyrics`.
    Lyrics { track: TrackId },
    /// A track's waveform (not paged): from core's cache, else `null`, and the track is
    /// queued to be computed in the background; `WaveformReady` announces it. A track whose
    /// file cannot be decoded stays `null`. Unknown track: `not_found` / `track_not_found`.
    /// Feature `waveform`.
    Waveform { track: TrackId },
    /// Cached computed loudness (feature `automatic_loudness`); never starts a scan.
    Loudness { track: TrackId },
    /// Preview embedded front-cover edits without modifying music files or reserving them.
    /// Reports actual display fallbacks, format scope and the required undo storage.
    CoverEditPreview {
        tracks: Vec<TrackId>,
        change: CoverChange,
    },
    /// What `EditTags` with these tracks and changes would do (not paged; read from the
    /// files on the caller's thread; writes nothing): each file's tags as the library
    /// shows them (`TrackInfo.tags`) before and after (a file the changes leave as it is
    /// has `before` equal to `after`), and the files it would skip, by code (the player's
    /// open files only when it runs: `file_in_use`). Errors as for `EditTags`. Any origin
    /// may ask (agents too). Feature `edit_tags`.
    TagEditPreview {
        tracks: Vec<TrackId>,
        changes: Vec<TagChange>,
    },
    /// Enumerates at most 256 endpoints; offline fixed selections retain their identity.
    OutputDevices,
    /// Read cached capability evidence for the selected identity. This does not open output or promise exclusive acquisition.
    OutputDeviceCapabilities {
        /// Identity to inspect; system_default resolves the current cached default. Offline or unseen IDs may return output_device_not_found.
        selection: OutputDeviceSelection,
    },
    /// Authoritative requested preferences and verified active output facts.
    OutputState,
    /// Current in-memory playback limits, sampled on the core clock.
    PlaybackLimits,
    /// Decode-only fallback for a query this build does not know; never send it. Core
    /// answers an unknown query with `unsupported` / `unknown_query`.
    #[serde(other)]
    Unknown,
}

/// Which complete search result list to page (`search_paging`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SearchKind {
    /// Complete matching tracks.
    Tracks,
    /// Complete matching albums.
    Albums,
    /// Complete matching album artists.
    Artists,
    /// Decode-only fallback; never send it. Rejected in queries.
    #[serde(other)]
    Unknown,
}

impl Query {
    /// Cuts `limit` to the page maximum of this query.
    pub fn cap_limit(&mut self) {
        let (limit, max) = match self {
            Query::Album { limit, .. } => {
                if let Some(limit) = limit {
                    *limit = (*limit).min(MAX_TRACK_PAGE);
                }
                return;
            }
            Query::QueuePage { limit, .. }
            | Query::Search { limit, .. }
            | Query::PlaylistItems { limit, .. }
            | Query::History { limit, .. }
            | Query::Tracks { limit, .. } => (limit, MAX_TRACK_PAGE),
            Query::Albums { limit, .. }
            | Query::Playlists { limit, .. }
            | Query::Artists { limit, .. }
            | Query::ArtistAlbums { limit, .. }
            | Query::Facet { limit, .. }
            | Query::Jobs { limit, .. }
            | Query::Actions { limit, .. }
            | Query::Permissions { limit, .. } => (limit, MAX_PAGE),
            _ => return,
        };
        *limit = (*limit).min(max);
    }
}

/// Where the next page starts: `None` after the last page, and after an empty one (so a
/// client that follows it always ends, also with `limit: 0`).
pub fn next_offset(offset: u32, rows: usize, total: u32) -> Option<u32> {
    let end = u64::from(offset) + rows as u64;
    (rows > 0 && end < u64::from(total)).then(|| u32::try_from(end).unwrap_or(u32::MAX))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AlbumSort {
    /// Album artist, then title.
    #[default]
    Artist,
    Title,
    /// Oldest first.
    Year,
    RecentlyAdded,
    /// Title descending; unknown titles still last.
    TitleDesc,
    /// Artist descending, then title ascending.
    ArtistDesc,
    /// Newest year first; unknown years still last.
    YearDesc,
    /// Earliest indexed first.
    OldestAdded,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum QueryResult {
    /// Answer to Job: authoritative run-local state and the full retained outcome.
    Job { job: JobInfo },
    /// Answer to Jobs: compact summaries, without failure messages or diagnostic paths.
    Jobs {
        jobs: Vec<JobSummary>,
        offset: u32,
        total: u32,
        next_offset: Option<u32>,
    },
    /// Answer to `Snapshot`.
    Snapshot { snapshot: Box<Snapshot> },
    /// Answer to `QueuePage`.
    QueuePage {
        /// The queue's revision this page was read at.
        revision: QueueRevision,
        /// The `offset` of this page, as asked.
        offset: u32,
        /// Rows in the whole result, not only this page.
        total: u32,
        /// The `offset` of the next page; `null` after the last page or an empty one.
        next_offset: Option<u32>,
        entries: Vec<QueueEntry>,
    },
    /// Answer to `Search`.
    Search { results: SearchResults },
    /// Answer to `TrackRows`, in requested order, excluding missing IDs.
    TrackRows { tracks: Vec<TrackInfo> },
    /// Answer to `Albums` and `ArtistAlbums`.
    Albums {
        albums: Vec<AlbumSummary>,
        /// The `offset` of this page, as asked.
        offset: u32,
        /// Rows in the whole result, not only this page.
        total: u32,
        /// The `offset` of the next page; `null` after the last page or an empty one.
        next_offset: Option<u32>,
    },
    /// Answer to `Album`.
    Album { album: AlbumDetail },
    /// Answer to `Tracks`.
    Tracks {
        tracks: Vec<TrackInfo>,
        /// The `offset` of this page, as asked.
        offset: u32,
        /// Rows in the whole result, not only this page.
        total: u32,
        /// The `offset` of the next page; `null` after the last page or an empty one.
        next_offset: Option<u32>,
    },
    /// Answer to `Facet`.
    Facet {
        values: Vec<FacetValue>,
        /// The `offset` of this page, as asked.
        offset: u32,
        /// Rows in the whole result, not only this page.
        total: u32,
        /// The `offset` of the next page; `null` after the last page or an empty one.
        next_offset: Option<u32>,
    },
    /// Answer to `Playlists`.
    Playlists {
        playlists: Vec<PlaylistSummary>,
        /// The `offset` of this page, as asked.
        offset: u32,
        /// Rows in the whole result, not only this page.
        total: u32,
        /// The `offset` of the next page; `null` after the last page or an empty one.
        next_offset: Option<u32>,
    },
    /// Answer to `PlaylistItems`.
    PlaylistItems {
        playlist: PlaylistSummary,
        items: Vec<PlaylistEntry>,
        /// The `offset` of this page, as asked.
        offset: u32,
        /// Rows in the whole result, not only this page.
        total: u32,
        /// The `offset` of the next page; `null` after the last page or an empty one.
        next_offset: Option<u32>,
    },
    /// Answer to `Artists`.
    Artists {
        /// Whether any track still needs the current artist parser.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        indexing: Option<bool>,
        artists: Vec<ArtistSummary>,
        /// The `offset` of this page, as asked.
        offset: u32,
        /// Rows in the whole result, not only this page.
        total: u32,
        /// The `offset` of the next page; `null` after the last page or an empty one.
        next_offset: Option<u32>,
    },
    /// Answer to `History`.
    History {
        items: Vec<HistoryItem>,
        /// The `offset` of this page, as asked.
        offset: u32,
        /// Rows in the whole result, not only this page.
        total: u32,
        /// The `offset` of the next page; `null` after the last page or an empty one.
        next_offset: Option<u32>,
    },
    /// Answer to `Roots`.
    Roots { roots: Vec<RootInfo> },
    /// Answer to `Actions`.
    Actions {
        actions: Vec<ActionSummary>,
        /// The `offset` of this page, as asked.
        offset: u32,
        /// Rows in the whole result, not only this page.
        total: u32,
        /// The `offset` of the next page; `null` after the last page or an empty one.
        next_offset: Option<u32>,
    },
    /// Answer to `Agents`.
    Agents { agents: Vec<AgentInfo> },
    /// Answer to `Permissions`.
    Permissions {
        grants: Vec<Grant>,
        /// The `offset` of this page, as asked.
        offset: u32,
        /// Rows in the whole result, not only this page.
        total: u32,
        /// The `offset` of the next page; `null` after the last page or an empty one.
        next_offset: Option<u32>,
    },
    /// Answer to `Lyrics`.
    Lyrics {
        /// `null`: the track has no lyrics.
        lyrics: Option<Lyrics>,
    },
    /// Answer to `Waveform`.
    Waveform {
        /// `null`: not computed yet (it is now queued; `WaveformReady` follows), or the
        /// file cannot be decoded.
        waveform: Option<Waveform>,
    },
    /// Answer to `Loudness`; null when not measured or invalidated. File tags remain
    /// available through TrackInfo.replay_gain regardless of this derived cache.
    Loudness { loudness: Option<LoudnessInfo> },
    /// Answer to CoverEditPreview: read-only effects and exclusions; not a file reservation.
    CoverEditPreview {
        /// Inspected selected files, including unchanged files.
        files: Vec<CoverFilePreview>,
        /// Selected files excluded from the operation with stable error codes.
        skipped: Vec<CoverFileResult>,
    },
    /// Answer to `TagEditPreview`.
    TagEditPreview {
        /// The files it would write, in the order asked (each track once).
        files: Vec<TagFilePreview>,
        /// The files it would not write: `track_unavailable`, `cue_track`, `file_missing`,
        /// `file_changed`, `rescan_needed`, `unsupported_format`, `open_failed` (it cannot
        /// be read), `invalid_tag` (values the format cannot represent), `undo_too_large`,
        /// `not_undoable` (the edit could not be undone).
        skipped: Vec<TagFileResult>,
    },
    /// The query failed.
    Err { error: ContractError },
    /// Answer to OutputDevices. Inventory overflow returns invalid/out_of_range; smaller pages cannot remedy it.
    OutputDevices {
        /// Cached discovery generation.
        generation: u64,
        /// Current default identity, or null when not known.
        default_device: Option<String>,
        /// Bounded cached inventory including retained selected/offline descriptions.
        #[serde(default)]
        devices: Vec<OutputDeviceInfo>,
    },
    /// Answer to OutputDeviceCapabilities. Inventory/range overflow returns invalid/out_of_range.
    OutputDeviceCapabilities {
        /// Discovery generation associated with the evidence.
        generation: u64,
        /// Identity actually resolved for the query.
        resolved_device: OutputDeviceInfo,
        /// Capability evidence, independent of an active stream.
        capabilities: OutputDeviceCapabilities,
    },
    /// Answer to OutputState: preferences and independently observed active facts.
    OutputState {
        /// Committed preferences, even when application fails.
        settings: OutputSettings,
        /// Current stream readiness, active format, fallback or application failure.
        state: OutputStateInfo,
    },
    /// Answer to PlaybackLimits, sampled on the Snapshot display clock.
    PlaybackLimits {
        /// Current run-local conditions; both clear when either is consumed.
        playback_limits: PlaybackLimitsInfo,
        /// Snapshot.now_ms clock for display interpolation.
        now_ms: u64,
    },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}
