//! Action journal summaries and tag edit previews.

use super::*;

/// One file of a `TagEditPreview`: its tags (as `TrackInfo.tags` has them: upper-case keys,
/// values in the file's order) now and after the edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TagFilePreview {
    /// Whether core's edit application actually modifies this file.
    /// Use this instead of comparing display tags, which can omit or truncate values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<bool>,
    /// Canonical display location resolved for this preview. This is not a
    /// write lock or a target-binding promise; files are checked again when executing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_location: Option<String>,
    pub track: TrackId,
    /// Whether this edit converts legacy ID3 text to standard Unicode.
    /// False when the file would remain unchanged; absent data is not proof of no change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding_conversion: Option<bool>,
    pub before: BTreeMap<String, Vec<String>>,
    pub after: BTreeMap<String, Vec<String>>,
}

/// A track a job could not process, and why (`loudness_scan.failed`, `TagEditPreview.skipped`,
/// the `tag_edit` job report).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TagFileResult {
    pub track: TrackId,
    pub code: ErrorCode,
}

/// One action of the journal (guarantee 15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActionSummary {
    pub id: ActionId,
    /// Unix ms when the action was recorded.
    pub recorded_at_ms: i64,
    /// Who did it (`unknown` for actions older than the journal's origins, or an origin
    /// this build does not know).
    pub origin: Origin,
    pub kind: ActionKind,
    /// One English sentence, e.g. `rate 120 tracks 5 stars`.
    pub summary: String,
    /// Objects the action changed (tracks, playlists, folders). A tag edit whose undo
    /// could not write every file back counts those left (`undone_by`).
    pub target_count: u32,
    /// The action that undid this one. `null` also for a tag edit (or an undo or redo of
    /// one) whose undo could not write every file back: that undo (`undo_of` naming this
    /// one) counts the files it wrote back, and this one still undoes those left.
    pub undone_by: Option<ActionId>,
    /// For `undo` / `redo`: the action it undid.
    pub undo_of: Option<ActionId>,
    /// It can be undone now: not undone yet and not expired. An undo can still fail on a
    /// conflict. Never for an audit entry (`purge`, `file_write`, `root_release`, `root_resume`).
    pub undoable: bool,
    /// The user's grant that let an agent do it without asking; `null` when none did.
    #[serde(default)]
    pub grant: Option<GrantId>,
    /// Structured operation-time facts (feature `action_details`). Omitted for older
    /// journal entries. Tag values are deliberately excluded; only keys and operations
    /// are recorded. Independent of the English summary and of later object changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<ActionDetails>,
}

/// Operation-time facts; the enclosing action kind identifies the precise operation.
/// Names describe the fact family (root, playlist), except set_rating/set_loved and
/// tag_edit which retain their operation names; undo covers both undo and redo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ActionDetails {
    /// Audit facts for trusted recovery, without private candidate paths.
    CoverRecoveryResolve {
        /// Resolved track.
        track: TrackId,
        /// Explicit trusted resolution.
        resolution: CoverRecoveryResolution,
        /// Number of reconciled durable write registrations.
        intent_count: u32,
    },
    /// Operation-time cover facts without image bytes or candidate locations.
    CoverEdit {
        /// Set or remove.
        operation: CoverOperation,
        /// Representation affected by the completed operation.
        scope: CoverScope,
        /// Count of successfully written files in the original operation.
        file_count: u32,
    },
    TagEdit {
        changes: Vec<TagKeyChange>,
    },
    Root {
        root: RootId,
        path: String,
    },
    SetRating {
        rating: Option<u8>,
    },
    SetLoved {
        loved: bool,
    },
    Playlist {
        playlist: PlaylistId,
        name: String,
    },
    Undo {
        original_kind: ActionKind,
    },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// A key operation whose primary values changed in at least one written file. No values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TagKeyChange {
    Set {
        key: String,
    },
    Remove {
        key: String,
    },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// What an action did. Open: more kinds may come.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ActionKind {
    /// UI acceptance of a conflicting recovery intent. Audit only, never undoable.
    CoverRecoveryResolve,
    CoverEdit,
    SetRating,
    SetLoved,
    PlaylistCreate,
    PlaylistImport,
    PlaylistRename,
    PlaylistAdd,
    PlaylistRemove,
    PlaylistMove,
    PlaylistSetQuery,
    /// Moved a playlist to the trash.
    PlaylistDelete,
    /// Took a playlist out of the trash.
    PlaylistRestore,
    RootAdd,
    RootRemove,
    /// Audit of a non-UI release request, including retries. Not undoable.
    RootRelease,
    /// Audit of a non-UI resume request, including retries. Not undoable.
    RootResume,
    /// Undid an action (`undo_of`).
    Undo,
    /// Undid an undo, redoing the action it undid.
    Redo,
    /// An audit entry: playlists were permanently deleted (`PurgePlaylists`). Not undoable.
    Purge,
    /// An audit entry: a file was replaced (`ExportM3u` over an existing file). Not
    /// undoable.
    FileWrite,
    /// The tags of files were rewritten (`EditTags`). Undoing it writes the old
    /// values back (a job).
    TagEdit,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

impl ActionKind {
    /// The kind with this JSON name; `Unknown` if there is none.
    pub fn parse(kind: &str) -> Self {
        serde_json::from_value(serde_json::Value::String(kind.to_owned()))
            .unwrap_or(ActionKind::Unknown)
    }
}
