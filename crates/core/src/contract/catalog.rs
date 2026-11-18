//! Track, playlist, album, artist, history, and root records.

use super::*;

/// The one track type, everywhere (queue, albums, search, history, playlists, now
/// playing). Statistics and user data are as of when it was read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TrackInfo {
    pub id: TrackId,
    /// The title tag, or the file name without extension (`title_from_file`).
    pub title: String,
    pub title_from_file: bool,
    /// For display: the artist tag as written (one string).
    pub artist: Option<String>,
    /// Ordered, deduplicated normalized name identities.
    #[serde(default)]
    pub artists: Vec<ArtistRef>,
    /// Declaration source; null means unknown/unrecognized. The none value instead means the reader confirmed no declaration. An unrecognized stored source never emits decode-only unknown and cannot establish artists_complete.
    #[serde(default)]
    pub artists_source: Option<ArtistListSource>,
    /// False for old messages and tracks awaiting parser backfill.
    #[serde(default)]
    pub artists_complete: bool,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    /// The library grouping name: album artist, else Various Artists for compilations,
    /// else track artist; null if none.
    #[serde(default)]
    pub group_artist: Option<String>,
    /// The key of the album it is listed under (`AlbumSummary.key`).
    pub album_key: String,
    pub track_no: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_no: Option<u32>,
    pub disc_total: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub compilation: bool,
    pub duration_ms: Option<u64>,
    /// Lower-case codec id, as the `codec` field of queries compares it (`flac`, `mp3`, `aac` …).
    pub codec: Option<String>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub bits_per_sample: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    pub lossless: bool,
    /// The library folder it is under (`RootInfo`).
    pub root: RootId,
    /// The location relative to the root, `/`-separated on every platform, for display
    /// and sorting. Location strings are defined by the platform layer: never join the
    /// root's location and `path` into a file path.
    pub path: String,
    pub size_bytes: u64,
    /// Unix ms.
    pub modified_at_ms: i64,
    /// Unix ms when it was first indexed.
    pub added_at_ms: i64,
    /// False when the file has gone missing or its root was removed; the track is kept
    /// (for history and playlists).
    pub available: bool,
    pub has_cover: bool,
    /// Opaque equality-only revision token. Null means no verified revision, not no cover; use has_cover for presence. CoversChanged/TracksChanged invalidate resources even when this token is null or unchanged.
    #[serde(default)]
    pub cover_revision: Option<String>,
    /// 1–5; `null` when unrated.
    pub rating: Option<u8>,
    pub loved: bool,
    pub play_count: u32,
    pub skip_count: u32,
    pub last_played_at_ms: Option<i64>,
    pub first_played_at_ms: Option<i64>,
    /// The track's ReplayGain values; left out when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_gain: Option<ReplayGainInfo>,
    /// The tags a query asked for (`tags`), by upper-case key; left out when none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, Vec<String>>,
    /// Set for a track of a CUE sheet; left out for a track that is a whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cue: Option<TrackCue>,
}

/// Where a track of a CUE sheet is: every track of a sheet is a track of its own (its own
/// id, statistics, rating), cut from an audio file that is then not a track itself and is
/// not in browse queries (guarantee 9). Its `path` and `size_bytes` are that file's, shared
/// with the sheet's other tracks of the file; `duration_ms` is the track's own. Tags come
/// from the sheet, with the file's tags for what the sheet does not say. A track runs from
/// its `INDEX 01` to the next track's (its pregap ends the track before it), the last one
/// of a file to the end of the file; audio before track 1 longer than 10 s is track 0.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct TrackCue {
    /// The sheet's location relative to the root, like `TrackInfo.path` and for display
    /// only (guarantee 16); for a sheet embedded in the audio file (a FLAC `CUESHEET` tag),
    /// the same as `path`.
    pub sheet: String,
    /// The `TRACK` number in the sheet; 0 for the hidden track before track 1.
    pub number: u32,
    /// Where the track starts in the file, ms (rounded down from the sheet's 1/75 s).
    pub start_ms: u64,
    /// Where it ends in the file, ms; `null` when the file's length is unknown and the
    /// track is the file's last.
    pub end_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlaylistSummary {
    pub id: PlaylistId,
    pub name: String,
    pub kind: PlaylistKind,
    /// Manual: all items. Smart: the query's current result, within `max_tracks`.
    pub track_count: u32,
    pub duration_ms: u64,
    pub revision: PlaylistRevision,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// Set while the playlist is in the trash.
    pub deleted_at_ms: Option<i64>,
    /// Set when a smart playlist's saved query no longer type-checks; it lists no tracks
    /// until `SetSmartQuery` replaces it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlaylistEntry {
    /// `null` for smart playlists, whose rows are not stored items.
    pub item: Option<PlaylistItemId>,
    pub track: TrackInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AlbumSummary {
    /// Opaque key of the album (the `album` query and play context). It may change when
    /// tags or the grouping rules change: look it up again rather than keeping it long.
    pub key: String,
    /// Empty for the "unknown album" group of tracks without an album tag.
    pub title: String,
    pub artist: Option<String>,
    pub year: Option<i32>,
    pub track_count: u32,
    pub disc_count: u32,
    pub duration_ms: u64,
    /// A track with cover art, for the album's thumbnail.
    pub art_track: Option<TrackId>,
    /// Unix ms when its first track was indexed.
    pub added_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AlbumDetail {
    /// Full available-track count, equal to `summary.track_count`.
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub offset: u32,
    #[serde(default)]
    pub next_offset: Option<u32>,
    pub summary: AlbumSummary,
    /// A page of available tracks in stable disc/track/path/ID order.
    /// The full count is `summary.track_count`.
    pub tracks: Vec<TrackInfo>,
}

/// Name-based identity; opaque key, never a file path or database row number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtistRef {
    /// Opaque equality-only name identity, stable within its identity version. Do not parse or derive names from it. Person queries take name, not key.
    pub key: String,
    /// Per-track spelling, bounded by limits.max_artist_name_bytes UTF-8 bytes; represents a normalized name identity, not a verified person.
    pub name: String,
}

/// Which declared values established a track's person membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ArtistListSource {
    /// Explicit nonempty ARTISTS values.
    ArtistsTag,
    /// All ARTIST values from the selected tag source.
    ArtistValues,
    /// A single CUE PERFORMER override.
    CuePerformer,
    /// Provisional legacy display credit, without splitting.
    LegacyDisplay,
    /// The current parser confirmed no artist declaration.
    None,
    /// Decode-only fallback; never send it.
    #[serde(other)]
    Unknown,
}

/// Artist query semantics; omission preserves the album grouping view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ArtistView {
    /// Existing album grouping artist semantics.
    #[default]
    AlbumGroup,
    /// Ordered name identities independent of album grouping.
    Person,
    /// Decode-only fallback; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ArtistSummary {
    /// Opaque name identity, present in person view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Display name. In person view the current implementation selects the smallest stored per-track spelling; queries use this name, not key.
    pub name: String,
    pub album_count: u32,
    pub track_count: u32,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct SearchResults {
    /// Selected kind's full count; sum of the three counts for a mixed first page.
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub offset: u32,
    /// Next page of the selected kind; null for mixed results, empty or last pages.
    #[serde(default)]
    pub next_offset: Option<u32>,
    pub tracks: Vec<TrackInfo>,
    pub albums: Vec<AlbumSummary>,
    pub artists: Vec<ArtistSummary>,
    /// Every match of each kind, listed or not.
    pub tracks_total: u32,
    pub albums_total: u32,
    pub artists_total: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HistoryItem {
    pub id: PlayEventId,
    pub track: TrackInfo,
    /// Unix time, milliseconds.
    pub started_at_ms: i64,
    /// How long the track was actually heard: pauses and audio skipped by seeking do not
    /// count.
    pub played_ms: u64,
    pub ended: EndReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EndReason {
    Completed,
    Skipped,
    Stopped,
    Failed,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RootInfo {
    pub id: RootId,
    /// The folder's location, an opaque string defined by the platform layer (desktop:
    /// its absolute path; mobile: e.g. a platform folder URI). Clients only display it or
    /// pass it back.
    pub path: String,
    pub track_count: u32,
    /// Session-local file access suspension (`release_root`). Clients treat missing data as unknown.
    #[serde(default)]
    pub released: bool,
}
