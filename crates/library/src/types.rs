//! Public data types. All of them serialize with serde; core re-exposes them in its contract.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::text::normalize;

/// Stable track id, assigned when a file is first indexed and never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrackId(pub i64);

/// Id of a library root folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RootId(pub i64);

impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for RootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A folder whose audio files make up (part of) the library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Root {
    pub id: RootId,
    /// Absolute path of the folder.
    pub path: PathBuf,
    /// WHATWG encoding label (`"gbk"`, `"big5"`, `"shift_jis"`,...) used for Latin-1 tag
    /// text in this root instead of detection. See [`crate::repair_latin1_text`].
    pub forced_encoding: Option<String>,
    /// Unix time in milliseconds.
    pub added_at: i64,
    /// Unix time in milliseconds of the last completed (not cancelled) scan.
    pub last_scan_at: Option<i64>,
    /// Available tracks under this root.
    pub track_count: u64,
}

/// Identifies an album: normalized `(album artist, album title)`.
///
/// The artist part is the album artist, or "various artists" for compilations without one,
/// or else the track artist (empty if unknown). Tracks without an album tag form one
/// "unknown album" per artist, whose key has an empty title part; no real album has that.
/// Opaque to clients; pass it back as received.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AlbumKey(pub String);

const ALBUM_KEY_SEPARATOR: char = '\u{1f}';

impl AlbumKey {
    /// Builds the key from display strings; both parts are normalized. An empty `album`
    /// gives the artist's unknown album.
    pub fn new(artist: &str, album: &str) -> Self {
        Self(format!(
            "{}{ALBUM_KEY_SEPARATOR}{}",
            normalize(artist),
            normalize(album)
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is an "unknown album" (tracks without an album tag).
    pub fn is_unknown_album(&self) -> bool {
        self.0.ends_with(ALBUM_KEY_SEPARATOR)
    }
}

/// Artist name used for compilations that have no album artist tag.
pub const VARIOUS_ARTISTS: &str = "Various Artists";

/// Effective ReplayGain pairs, with provenance captured by the same read query.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplayGain {
    pub track_gain_db: Option<f32>,
    pub track_peak: Option<f32>,
    pub album_gain_db: Option<f32>,
    pub album_peak: Option<f32>,
    #[serde(default)]
    pub track_measured: bool,
    #[serde(default)]
    pub album_measured: bool,
}

/// An opaque normalized-name identity, independent of album grouping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtistRef {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtistListSource {
    ArtistsTag,
    ArtistValues,
    CuePerformer,
    LegacyDisplay,
    None,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ArtistView {
    #[default]
    AlbumGroup,
    Person,
}

/// One indexed audio file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub root_id: RootId,
    /// Path relative to the root, `/`-separated on every platform.
    pub rel_path: String,
    /// `false` when the file was missing at the last scan; the row is kept for history.
    pub available: bool,
    pub size: u64,
    /// File modification time, Unix milliseconds.
    pub mtime_ms: i64,

    pub title: Option<String>,
    pub artist: Option<String>,
    #[serde(default)]
    pub artists: Vec<ArtistRef>,
    #[serde(default)]
    pub artists_source: Option<ArtistListSource>,
    #[serde(default)]
    pub artists_complete: bool,
    #[serde(default)]
    pub cover_revision: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    /// Persisted album/artist grouping name, derived by the library writer.
    pub group_artist: Option<String>,
    pub genre: Option<String>,
    pub year: Option<i32>,
    pub track_no: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_no: Option<u32>,
    pub disc_total: Option<u32>,
    pub compilation: bool,
    /// The album this track is listed under; for tracks without an album tag, the artist's
    /// unknown album.
    pub album_key: AlbumKey,

    pub duration_ms: Option<u64>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    pub bits_per_sample: Option<u8>,
    pub bitrate_kbps: Option<u32>,
    /// Lower-case codec name: `flac`, `mp3`, `aac`, `alac`, `vorbis`, `opus`, `pcm`,...
    pub codec: Option<String>,
    pub lossless: Option<bool>,
    pub replay_gain: ReplayGain,
    pub has_embedded_art: bool,
    /// A cover image file (`cover`/`folder`/`front` `.jpg`/`.jpeg`/`.png`) is next to the
    /// file, as of the last scan.
    pub has_folder_art: bool,
    /// Legacy encoding that tag text was re-decoded from, if any.
    pub tag_encoding: Option<String>,

    /// Unix milliseconds when the file was first indexed.
    pub added_at: i64,
    /// Unix milliseconds of the last change to this row's file data.
    pub updated_at: i64,
    /// Set for a track of a CUE sheet: `rel_path`, `size` and `mtime_ms` are then its audio
    /// file's (shared with the sheet's other tracks of that file), `duration_ms` is the
    /// track's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cue: Option<CueTrack>,
}

/// Where a track of a CUE sheet is (docs/design/media-resources.md). It starts at
/// its `INDEX 01` and ends where the next track of the same file starts, or at the end of
/// the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CueTrack {
    /// The sheet's path relative to the root, `/`-separated; for a sheet embedded in the
    /// audio file (a FLAC `CUESHEET` tag), the audio file's.
    pub sheet: String,
    /// The `TRACK` number; 0 for the hidden track before track 1 (audio before its
    /// `INDEX 01` that is longer than 10 s).
    pub number: u32,
    /// Start in the file, CD frames (1/75 s).
    pub start: u64,
    /// End in the file, CD frames; `None`: the end of the file.
    pub end: Option<u64>,
}

impl CueTrack {
    /// The start in milliseconds from the start of the file (rounded down).
    pub fn start_ms(&self) -> u64 {
        crate::cue::cd_frames_to_ms(self.start)
    }

    /// The end in milliseconds from the start of the file (rounded down); `None`: the end
    /// of the file.
    pub fn end_ms(&self) -> Option<u64> {
        self.end.map(crate::cue::cd_frames_to_ms)
    }

    /// The track's part of its file in frames at `sample_rate` (the file's): `(start,
    /// end)`, end `None` for the end of the file. Exact for every rate that is a multiple
    /// of 75 Hz (all common ones).
    pub fn frames(&self, sample_rate: u32) -> (u64, Option<u64>) {
        let at = |cd| crate::cue::cd_frames_to_samples(cd, sample_rate);
        (at(self.start), self.end.map(at))
    }
}

impl Track {
    /// The title, or the file name without extension for untagged files.
    pub fn display_title(&self) -> &str {
        match &self.title {
            Some(title) => title,
            None => file_stem(&self.rel_path),
        }
    }
}

/// File name without directory and extension of a `/`-separated relative path.
pub(crate) fn file_stem(rel_path: &str) -> &str {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

/// An album as listed in the album grid or a search result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlbumSummary {
    pub key: AlbumKey,
    /// Empty for an unknown album (tracks without an album tag).
    pub title: String,
    /// Album artist, [`VARIOUS_ARTISTS`] for untagged compilations, or the track artist.
    pub artist: Option<String>,
    pub year: Option<i32>,
    pub track_count: u32,
    pub disc_count: u32,
    pub duration_ms: u64,
    /// A track to pass to [`crate::Library::cover_art`]: one with embedded art if any,
    /// else one with a cover image file in its folder.
    pub art_track: Option<TrackId>,
    /// Unix milliseconds when the album's first track was indexed.
    pub added_at: i64,
}

/// An album with its tracks in disc / track order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Album {
    pub summary: AlbumSummary,
    pub tracks: Vec<Track>,
}

/// An album artist (album artist tag, else "Various Artists" for compilations, else the
/// track artist) with counts over available tracks; matches the artists of the album grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtistSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub name: String,
    pub album_count: u32,
    pub track_count: u32,
}

/// The first entries of each kind that match a search, and how many match in all; lists
/// and totals come from one snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchResults {
    pub tracks: Vec<Track>,
    pub albums: Vec<AlbumSummary>,
    pub artists: Vec<ArtistSummary>,
    /// Available tracks that match (listed in `tracks` or not).
    pub tracks_total: u64,
    pub albums_total: u64,
    pub artists_total: u64,
}

/// A page of a list and the length of the whole list, from one snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub total: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AlbumSort {
    /// By album title.
    Title,
    /// By album artist, then title (the default).
    #[default]
    Artist,
    /// By year (oldest first), then artist.
    Year,
    /// Most recently added first.
    RecentlyAdded,
    /// Title descending; unknown titles still last.
    TitleDesc,
    /// Artist descending, then title ascending.
    ArtistDesc,
    /// Newest year first; unknown years still last.
    YearDesc,
    /// Earliest indexed first.
    OldestAdded,
}

/// A window into a result list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Page {
    pub offset: u32,
    pub limit: u32,
}

impl Page {
    pub const fn new(offset: u32, limit: u32) -> Self {
        Self { offset, limit }
    }
}

/// Counters reported while scanning (see [`ScanReport`] for the final result).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanProgress {
    /// Distinct audio paths looked at across all passes (including unchanged files).
    pub scanned: u64,
    /// Distinct audio paths found across all walks; directories and CUE sheets excluded.
    pub total_estimate: u64,
    pub added: u64,
    pub updated: u64,
    /// Unchanged files that were unavailable and are available again (a re-added folder,
    /// a returned file).
    #[serde(default)]
    pub restored: u64,
    /// Files that disappeared and were marked unavailable.
    pub removed: u64,
    /// Files that could not be read (tags or properties) or had unusable paths; each path
    /// counts once per scan.
    pub errors: u64,
}

/// Most errors a [`ScanReport`] lists.
pub const SCAN_ERROR_LIMIT: usize = 20;

/// Identifies one scan (a [`ScanHandle`](crate::ScanHandle) and the requests coalesced into
/// it) within one [`Library`](crate::Library); counts up from 1, not persisted.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ScanId(pub u64);

impl fmt::Display for ScanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A file or folder a scan could not read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanError {
    pub code: ScanErrorCode,
    /// Absolute path (lossy for non-Unicode names).
    pub path: String,
    /// English description.
    pub message: String,
}

/// The final result of a scan, over all its passes (one per coalesced request).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanReport {
    /// Complete Rust-only error summary and visited scope; never part of JSON reports.
    #[serde(skip)]
    pub diagnostics: crate::ScanDiagnostics,
    pub scan: ScanId,
    /// Distinct root-relative audio paths looked at across all passes, unchanged included.
    pub scanned: u64,
    pub added: u64,
    pub updated: u64,
    /// Unchanged files that were unavailable and are available again (a re-added folder,
    /// a returned file).
    #[serde(default)]
    pub restored: u64,
    /// Files that disappeared and were marked unavailable.
    pub removed: u64,
    /// Files and folders that could not be read, unusable paths, unreachable roots; each
    /// path counts once, also when several passes met it.
    pub errors: u64,
    /// The first [`SCAN_ERROR_LIMIT`] of those errors, in the order met.
    pub first_errors: Vec<ScanError>,
    /// Passes over the roots: one, plus one more after each pass during which requests
    /// were coalesced into this scan.
    pub passes: u32,
    /// Requests coalesced into this scan ([`crate::ScanStart::Coalesced`]).
    pub coalesced: u32,
    /// The scan was cancelled: the counters cover the work done until then; files were not
    /// marked unavailable by the interrupted pass.
    pub cancelled: bool,
}

/// Why a play ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EndedReason {
    Completed,
    Skipped,
    Stopped,
    Failed,
    /// A reason this build does not know (written by a newer one); counted by the time
    /// heard alone.
    #[serde(other)]
    Unknown,
}

impl EndedReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EndedReason::Completed => "completed",
            EndedReason::Skipped => "skipped",
            EndedReason::Stopped => "stopped",
            EndedReason::Failed => "failed",
            EndedReason::Unknown => "unknown",
        }
    }

    pub(crate) fn parse(text: &str) -> Self {
        match text {
            "completed" => EndedReason::Completed,
            "skipped" => EndedReason::Skipped,
            "stopped" => EndedReason::Stopped,
            "failed" => EndedReason::Failed,
            _ => EndedReason::Unknown,
        }
    }
}

/// Id of a stored play event (row of the history); never reused (since schema v6 the
/// table hands out ids with `AUTOINCREMENT`, so not even after rows are deleted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlayEventId(pub i64);

impl fmt::Display for PlayEventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One row of the play history: the event as recorded, with its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: PlayEventId,
    /// `context` and `origin` as stored; `ended_reason` is
    /// [`EndedReason::Unknown`] for reasons this build does not know.
    pub event: PlayEvent,
}

/// One raw play event; whether it "counts" as a play is decided at query time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayEvent {
    pub track_id: TrackId,
    /// The play context as serialized by core (e.g. JSON), opaque to the library.
    pub context: Option<String>,
    /// Unix milliseconds when the track started being heard.
    pub started_at: i64,
    /// How much of the track was heard.
    pub played_ms: u64,
    pub ended_reason: EndedReason,
    /// Who started the play (`"ui"`, `"agent:..."`), stamped by core's adapter.
    pub origin: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn album_key_is_normalized() {
        assert!(AlbumKey::new("x", "").is_unknown_album());
        assert!(!AlbumKey::new("", "x").is_unknown_album());
        assert_eq!(
            AlbumKey::new("ＡＢＣ", " Greatest  Hits "),
            AlbumKey::new("abc", "greatest hits")
        );
        assert_ne!(AlbumKey::new("a", "bc"), AlbumKey::new("ab", "c"));
    }

    #[test]
    fn file_stem_of_rel_path() {
        assert_eq!(file_stem("a/b/01 Song.flac"), "01 Song");
        assert_eq!(file_stem("noext"), "noext");
        assert_eq!(file_stem("dir/.hidden"), ".hidden");
    }
}

/// A single result kind, or use `None` for a mixed search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Tracks,
    Albums,
    Artists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanErrorCode {
    RootUnreachable,
    NoAudioFiles,
    /// Reading or enumerating a directory failed.
    DirectoryReadFailed,
    InvalidPath,
    /// Reading file attributes (stat), not tag metadata, failed.
    StatFailed,
    AudioReadFailed,
    CueReadFailed,
    ReaderPanicked,
    CueMissingFiles,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}
