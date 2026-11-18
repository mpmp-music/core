//! Cover editing inputs, results, and recovery selections.

use super::*;

/// Explicit UI resolution of a cover recovery conflict. More choices may be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CoverRecoveryResolution {
    /// Accept a readable current file; retain registered candidates for confirmed cleanup.
    AcceptCurrent,
    /// Restore a verified before backup to a missing original name; UI confirmation only.
    RestoreBefore,
    /// Restore a verified staged after file to a missing original name; UI confirmation only.
    RestoreAfter,
    /// Delete registered, digest-bound candidates after separate UI confirmation.
    CleanupCandidates,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

impl CoverRecoveryResolution {
    pub(crate) fn library_resolution(self) -> Option<mpmp_library::CoverRecoveryResolution> {
        use mpmp_library::CoverRecoveryResolution as R;
        Some(match self {
            Self::AcceptCurrent => R::AcceptCurrent,
            Self::RestoreBefore => R::RestoreBefore,
            Self::RestoreAfter => R::RestoreAfter,
            Self::CleanupCandidates => R::CleanupCandidates,
            Self::Unknown => return None,
        })
    }
}

/// A content-bound choice from the UI-only recovery inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoverRecoverySelection {
    /// Opaque registration ID supplied only by the trusted host recovery inventory; third-party clients must not infer it from summary text.
    pub intent: i64,
    /// The registered copy to publish.
    pub role: CoverCandidateRole,
    /// Complete lowercase 64-digit SHA-256 from the UI recovery inventory.
    pub sha256: String,
}

/// Registered full-file recovery copies, available only to UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CoverCandidateRole {
    /// Complete before version.
    Backup,
    /// Staged edited result.
    Staged,
    /// Complete edited result saved before rollback.
    RollbackAfter,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

impl CoverRecoverySelection {
    pub(crate) fn library_selection(&self) -> Option<mpmp_library::CoverRecoverySelection> {
        Some(mpmp_library::CoverRecoverySelection {
            intent: self.intent,
            sha256: self.sha256.clone(),
            role: match self.role {
                CoverCandidateRole::Backup => mpmp_library::CoverCandidateRole::Backup,
                CoverCandidateRole::Staged => mpmp_library::CoverCandidateRole::Staged,
                CoverCandidateRole::RollbackAfter => {
                    mpmp_library::CoverCandidateRole::RollbackAfter
                }
                CoverCandidateRole::Unknown => return None,
            },
        })
    }
}

/// JPEG/PNG original bytes, standard padded base64. No path or URL is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoverImageInput {
    /// Declared image MIME, image/jpeg or image/png; content must match.
    pub mime: String,
    /// Canonical standard padded base64; decoded bytes are bounded by max_cover_image_bytes.
    pub data_base64: String,
}

/// Primary-picture edit; unrelated pictures and metadata are preserved or the file is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CoverChange {
    /// Replace the primary front-cover representation with a validated original JPEG/PNG.
    Set { image: CoverImageInput },
    /// Remove the primary cover representation; another embedded or folder image may become visible.
    Remove,
    /// Decode-only fallback for values this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// Validated encoded image description, without image contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoverImageInfo {
    /// Detected image MIME type.
    pub mime: String,
    /// Encoded image byte count.
    pub bytes: u64,
    /// Decoded width in pixels.
    pub width: u32,
    /// Decoded height in pixels.
    pub height: u32,
    /// Lowercase SHA-256 of the encoded image bytes.
    pub sha256: String,
}

/// Primary picture-container representation affected by a cover edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CoverScope {
    /// Only the front-cover pictures in the primary container.
    FrontCover,
    /// All MP4 covr entries, whose roles cannot be distinguished.
    AllCovr,
    /// Recorded multi-file scope that may include both representations.
    FrontCoverOrAllCovr,
    /// Decode-only fallback for values this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// Cover edit operation recorded without image contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CoverOperation {
    /// Set or replace primary cover data.
    Set,
    /// Remove primary cover data.
    Remove,
    /// Decode-only fallback for values this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
/// Read-only cover preview. A cover undo uses a whole-file digest: any later file
/// change makes it conflict. Clients must show this limitation before confirmation.
pub struct CoverFilePreview {
    /// Explicitly selected track.
    pub track: TrackId,
    /// Current primary image metadata, or null when absent or not established.
    pub before: Option<CoverImageInfo>,
    /// Expected visible image metadata, including a known fallback, or null when absent or not established.
    pub after: Option<CoverImageInfo>,
    /// Whether this operation would change the primary cover representation.
    pub changed: bool,
    /// Estimated retained bytes required for undo, subject to limits.max_cover_undo_bytes; not a reservation.
    pub undo_bytes: u64,
    /// True when removal may expose another embedded or folder picture; inspect after and re-read the cover after completion.
    pub fallback: bool,
    /// Picture-container representation replaced by this operation.
    pub scope: CoverScope,
}

/// One excluded or failed cover target, classified by the open error code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CoverFileResult {
    /// Selected track associated with the skip or failure.
    pub track: TrackId,
    /// Open error code classified by its frozen x-kind.
    pub code: ErrorCode,
    /// English diagnostic text for logs; localize by code.
    pub message: String,
}
