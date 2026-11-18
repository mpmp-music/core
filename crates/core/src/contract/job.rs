//! Background job progress, outcomes, reports, and scan errors.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum JobKind {
    CoverEdit,
    /// A library scan.
    Scan,
    /// Writing tags into files: `EditTags`, or the undo of a tag edit.
    TagEdit,
    /// Decoding audio to derive EBU R128 / ReplayGain 2.0 values. Automatic jobs carry
    /// no command origin (null) and use total:null; completed per-track caches survive cancellation.
    LoudnessScan,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// Authoritative state retained by core, independent of event delivery and host caches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobInfo {
    pub id: JobId,
    pub kind: JobKind,
    pub state: JobState,
    /// Observed progress, absent until the first progress event. Completed jobs set done
    /// equal to total (an unknown total becomes the final observed done); cancelled
    /// and failed jobs keep the last observed progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<JobProgressInfo>,
    /// Full report or failure; absent while running. Cancellation has no report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<JobOutcome>,
}

/// Small job-list row. Full reports and failure messages are available only through Job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobSummary {
    pub id: JobId,
    pub kind: JobKind,
    pub state: JobState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<JobProgressInfo>,
    /// Terminal report counts, absent while running or when cancelled/failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<JobCounts>,
    /// Terminal failure classification; no message or diagnostic paths in job lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JobError>,
}

/// Compact failure classification, using the same kind/code as ContractError.
/// Future optional fields may be added; full diagnostics are available through Job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobError {
    pub kind: ErrorKind,
    pub code: ErrorCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum JobCounts {
    /// Compact terminal cover counts; full file arrays require Job.
    CoverEdit {
        /// Successfully written files.
        written_count: u32,
        /// Files with no change required.
        unchanged_count: u32,
        /// Excluded files.
        skipped_count: u32,
        /// Failed files.
        failed_count: u32,
    },
    Scan {
        added_count: u32,
        updated_count: u32,
        restored_count: u32,
        removed_count: u32,
        error_count: u32,
    },
    TagEdit {
        written_count: u32,
        skipped_count: u32,
        failed_count: u32,
    },
    LoudnessScan {
        measured_count: u32,
        album_count: u32,
        silent_count: u32,
        failed_count: u32,
    },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobProgressInfo {
    pub done: u32,
    pub total: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum JobState {
    Running,
    Completed,
    Cancelled,
    Failed,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum JobOutcome {
    Completed {
        report: JobReport,
    },
    Cancelled,
    Failed {
        error: ContractError,
    },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum JobReport {
    /// Terminal per-file cover edit results. Failures may retain durable recovery facts.
    CoverEdit {
        /// Successfully written target IDs.
        written: Vec<TrackId>,
        /// Targets requiring no cover change.
        unchanged: Vec<TrackId>,
        /// Targets excluded before writing, including read-only files.
        skipped: Vec<CoverFileResult>,
        /// Targets whose attempted operation failed.
        failed: Vec<CoverFileResult>,
        /// Original undoable action, or null if nothing was written.
        action: Option<ActionId>,
    },
    Scan {
        /// Files indexed for the first time.
        added_count: u32,
        /// Files whose tags or properties changed.
        updated_count: u32,
        /// Unchanged files that were unavailable and are available again (a re-added
        /// folder, a returned file).
        #[serde(default)]
        restored_count: u32,
        /// Files that disappeared and became unavailable.
        removed_count: u32,
        /// Files and folders that could not be read.
        error_count: u32,
        /// The first errors (at most 20).
        first_errors: Vec<ScanErrorInfo>,
    },
    LoudnessScan {
        /// Tracks whose current measurements were saved, including silent tracks.
        measured_count: u32,
        /// Complete albums whose gated statistics were saved.
        album_count: u32,
        /// Saved tracks with no finite integrated LUFS.
        silent_count: u32,
        /// Tracks not saved and their reason; at most 1000 (automatic jobs keep the
        /// first 1000 failures). Automatic progress counts visited tracks, including cache hits.
        failed: Vec<TagFileResult>,
    },
    /// What a tag job did (`EditTags`, or an undo of a tag edit).
    TagEdit {
        /// Written files whose legacy ID3 text was converted to standard Unicode.
        /// May be absent in retained job data; empty for jobs that converted no files.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encoding_converted: Option<Vec<TrackId>>,
        /// Actual files written. Never infer this list from
        /// a preview minus failures: unchanged files need not have been written.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        written: Option<Vec<TrackId>>,
        /// Completed targets, including a legacy undo whose values were already present.
        /// Use `written` for physical writes. Its length can be smaller.
        written_count: u32,
        /// The action that records them (for an undo: the undo, of kind `undo` or `redo`);
        /// `null` when no target completed.
        action: Option<ActionId>,
        /// Files not written: `track_unavailable`, `cue_track`, `file_missing`, `file_changed`,
        /// `rescan_needed`, `file_in_use`, `unsupported_format`.
        skipped: Vec<TagFileResult>,
        /// Files that could not be written: `open_failed`, `write_failed`, `invalid_tag`
        /// (values the format cannot represent), `undo_too_large` (undo data exceeds
        /// the per-file, job or retained snapshot budget), `not_undoable` (an edit whose
        /// prior file bytes cannot be safely reconstructed), `location_changed` (a file
        /// that no longer is where
        /// it was checked). For an undo, the files skipped or failed were not written back:
        /// the action it undid stays undoable for them, and is not undone (`undone_by`
        /// stays `null`) until they are.
        failed: Vec<TagFileResult>,
    },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScanErrorInfo {
    /// Machine-readable cause (feature `scan_error_codes`), may be absent in retained job data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<ScanErrorCode>,
    /// The file or folder that could not be read, as a location string (desktop: its
    /// absolute path).
    pub path: String,
    pub message: String,
}

/// Scan-specific causes, assigned at the failure site without parsing English text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ScanErrorCode {
    RootUnreachable,
    /// The folder exists but no audio files were found where indexed files existed.
    NoAudioFiles,
    /// Enumerating or reading a directory failed; not an audio or CUE read failure.
    DirectoryReadFailed,
    InvalidPath,
    /// Reading file attributes (stat), not audio tags, failed.
    StatFailed,
    AudioReadFailed,
    CueReadFailed,
    ReaderPanicked,
    CueMissingFiles,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}
