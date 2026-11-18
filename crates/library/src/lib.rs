//! Indexes music files and provides persistent library queries and reversible edits.
//!
//! [`Library`] owns a single database writer, a read pool, and cancellable file jobs.
//! `scan`, `tags`, and `cue` build the catalog; `track_query`, `db::reads`, `person`, and `play_stats`
//! implement reads; `playlist` and `journal` record edits and undo. `tags::job` and
//! `cover::job` coordinate file writes; `cover`, `lyrics`, `waveform`, and `cover::thumbnail`
//! serve media resources. `agent_records` stores the agent records interpreted by core.
//!
//! See docs/design/storage.md for storage boundaries and schema migration policy,
//! docs/design/queries-playlists-undo.md for queries and undo, and
//! docs/design/media-resources.md for file edits and cached resources. Schema versions
//! are defined by `db::schema::LATEST_VERSION`; error codes are given by [`LibraryError::code`].

mod agent_records;
mod close_progress;
mod cover;
mod cue;
mod db;
mod error;
mod handle;
mod journal;
mod lease;
mod loudness;
mod lyrics;
mod person;
mod play_stats;
mod playlist;
mod ratings;
mod scan;
mod storage_access;
mod tags;
mod text;
mod track_query;
mod types;
mod waveform;

pub use agent_records::{AgentRecord, GrantRecord};
pub use close_progress::{CloseProgress, CloseStage};
pub use cover::CoverArt;
pub use cover::edit::{
    CoverChange, CoverImageInfo, MAX_COVER_EDIT_TRACKS, MAX_COVER_IMAGE_BYTES,
    MAX_COVER_IMAGE_EDGE, MAX_COVER_IMAGE_PIXELS, MAX_COVER_UNDO_BYTES, validate_image,
};
pub use cover::job::{
    CoverCandidateRole, CoverEditReport, CoverFilePreview, CoverJob, CoverPreview,
    CoverRecoveryFile, CoverRecoveryGuard, CoverRecoveryResolution, CoverRecoverySelection,
    CoverSource,
};
pub use cover::thumbnail::DEFAULT_COVER_CACHE_BYTES;
pub(crate) use error::Damage;
pub use error::{Invalid, LibraryError, NotFound, Result};
pub use handle::{Library, OpenOptions};
pub use journal::{
    ACTION_LIMIT, ACTION_RETENTION_MS, AUDIT_KINDS, AUDIT_LIMIT, AUDIT_RETENTION_MS, ActionId,
    ActionMeta, ActionSummary, ActionTarget, Done, UndoDescription, UndoEffect,
};
pub use lease::FileLease;
pub use lyrics::{Lyrics, LyricsFormat, LyricsLine, LyricsSource, LyricsWord};
pub use playlist::{
    M3U_MISSING_LIMIT, M3uReport, PLAYLIST_TRASH_RETENTION_MS, PLAYLIST_UNDO_RETENTION_MS,
    PLAYLIST_UNDO_STEPS, PlaylistEntry, PlaylistId, PlaylistItemId, PlaylistKind, PlaylistSummary,
    PurgeGuard, PurgeReviewClock,
};
pub use scan::diagnostics::ScanDiagnostics;
pub use scan::{AUDIO_EXTENSIONS, ScanHandle, ScanStart};
pub use tags::edit::{
    MAX_TAG_CHANGES, MAX_TAG_EDIT_BYTES, MAX_TAG_EDIT_TRACKS, MAX_TAG_KEY, MAX_TAG_VALUE_BYTES,
    MAX_TAG_VALUES, TagChange, TagChangeError, TagValues, check_changes,
    display_key as display_tag_key,
};
pub use tags::encoding::{encoding_for_label, is_latin1_mojibake_candidate, repair_latin1_text};
pub use tags::job::{
    InUse, TAG_FILE_CODES, TagEditReport, TagFilePreview, TagJob, TagJobOptions, TagPreview,
};
pub use text::normalize;
pub use track_query::{
    CompareOp, FacetValue, Field, Filter, SortKey, TextOp, TrackQuery, TrackRow, Value,
};
pub use types::{
    Album, AlbumKey, AlbumSort, AlbumSummary, ArtistListSource, ArtistRef, ArtistSummary,
    ArtistView, CueTrack, EndedReason, HistoryEntry, Page, Paged, PlayEvent, PlayEventId,
    ReplayGain, Root, RootId, SCAN_ERROR_LIMIT, ScanError, ScanErrorCode, ScanId, ScanProgress,
    ScanReport, SearchKind, SearchResults, Track, TrackId, VARIOUS_ARTISTS,
};
pub use waveform::{FileVersion, Waveform, WaveformChannel};

pub use loudness::{
    AlbumLoudnessStatistics, AlbumLoudnessUpdate, AutomaticLoudnessTrack, LoudnessCommit,
    LoudnessMeasurement, LoudnessSource, LoudnessUpdate,
};

#[cfg(test)]
mod tests;
