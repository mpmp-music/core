//! Request envelopes, command receipts, and confirmations.

use super::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Echoed in the response; only correlates the two.
    pub request_id: u64,
    /// Makes retries safe: core runs a request at most once per origin and key, for 10
    /// minutes, and answers a retry with the first result (guarantee 13). Reusing a key
    /// for a different command is `invalid` / `idempotency_key_reused`.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    pub command: Command,
}

impl Request {
    pub fn new(request_id: u64, command: Command) -> Self {
        Self {
            request_id,
            idempotency_key: None,
            command,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Response {
    pub request_id: u64,
    pub result: CommandResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
// One per request and moved rarely: boxing the receipt would only add an allocation.
#[allow(clippy::large_enum_variant)]
pub enum CommandResult {
    /// The command ran; JSON: `{"type":"ok", <receipt fields>}`.
    Ok(Receipt),
    Err {
        error: ContractError,
    },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// What a command did. Fields that do not apply are left out of the JSON.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Receipt {
    /// The audit entry for an irreversible operation, when one was recorded.
    /// Read-only history attribution; unlike `action`, this ID is never undoable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<ActionId>,
    /// `false`: the command succeeded but changed nothing (resume of an empty queue,
    /// a setting set to its value, a remove of ids already gone …).
    pub changed: bool,
    /// The queue's revision after the command (also for commands that do not touch the
    /// queue).
    pub queue_revision: QueueRevision,
    /// Queue entries the command added, in queue order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created_entries: Vec<EntryId>,
    /// Queue entries the command actually removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_entries: Vec<EntryId>,
    /// What a playlist command did to its playlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist: Option<PlaylistReceipt>,
    /// The root `AddRoot` added (or found), or `RemoveRoot`, `ReleaseRoot`, `ResumeRoot` named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<RootId>,
    /// The job the command started, or the running job it joined (`AddRoot`, `Rescan`,
    /// `UndoAction` of a folder's removal; `EditTags` and `UndoAction` of a tag edit, which
    /// then have no `action`): its results are visible with its `JobFinished` (guarantee
    /// 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobId>,
    /// `Confirm` / `Reject`: the answer and, when approved, the held command's result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed: Option<ConfirmationOutcome>,
    /// The action the command recorded in the journal (guarantee 15): set exactly when a
    /// recorded command changed something itself (ratings, loved flags, playlists,
    /// library folders, `UndoAction` / `UndoPlaylist` themselves), so it can be undone.
    /// Never an audit entry (`PurgePlaylists`, `ExportM3u`): see `audit` and `Actions`.
    /// A change a job makes (a tag edit, or its undo) is named by the job's
    /// report when the job finishes instead (the `tag_edit` report's `action`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ActionId>,
    /// `RegisterAgent` / `RenewAgentToken`: the agent's new token. Shown this once: core
    /// keeps only a hash of it. The user gives it to the agent's adapter (for example in
    /// the agent's MCP configuration); never log it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_token: Option<String>,
    /// `GrantPermission`: the new grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<GrantId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlaylistReceipt {
    pub playlist: PlaylistId,
    /// The playlist's revision after the command.
    pub revision: PlaylistRevision,
    /// Items the command added (`AddToPlaylist`, `ImportM3u`), in playlist order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created_items: Vec<PlaylistItemId>,
    /// Items the command actually removed (`RemoveFromPlaylist`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_items: Vec<PlaylistItemId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import: Option<M3uReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct M3uReport {
    /// Lines that matched a track and were added.
    pub matched_count: u32,
    /// Lines that matched no track in the library (at most 200; see `missing_count`). For
    /// an agent, only those that look like references to media (a path or URL with an
    /// audio or cue sheet extension): it never reads other text of a file back this way.
    pub missing: Vec<String>,
    /// Every line that matched no track, listed in `missing` or not.
    #[serde(default)]
    pub missing_count: u32,
}

/// The answer to a confirmation, in `Confirm`/`Reject` receipts and in
/// `ConfirmationResolved`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ConfirmationOutcome {
    pub token: String,
    /// Whether the user approved it (`reason` is `approved`).
    pub approved: bool,
    /// The held command's result when it ran (approved); left out when rejected or
    /// expired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Box<CommandResult>>,
    /// How it ended. Always sent by this core; clients treat missing data as unknown.
    #[serde(default)]
    pub reason: Option<ConfirmationReason>,
}

/// How a pending confirmation ended (`ConfirmationOutcome.reason`). It says who or what
/// answered, not how the command went: an approved command that then failed (for an
/// agent: its location changed meanwhile, `location_changed`, or a `forbid` was given
/// meanwhile, `permission_denied`) is `approved`, with the error in `result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConfirmationReason {
    /// The user approved it (`Confirm`): the command ran, or was refused when checked
    /// again (see `result`).
    Approved,
    /// The user rejected it (`Reject`).
    Rejected,
    /// Nobody answered before `expires_at_ms`.
    Expired,
    /// Nobody will answer it any more: the requester went away before the user answered
    /// (an agent's session ended, `end_agent_session`, or the agent was removed,
    /// `RemoveAgent`); or the user approved it, and the agent was removed (or registered
    /// again) before it ran, while it was checked again. The command did not run.
    Withdrawn,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// A command held for the user's confirmation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PendingConfirmation {
    /// Single use; answer with `Confirm { token }` or `Reject { token }`.
    pub token: String,
    /// English text for the dialog; the UI may render `command` its own way instead.
    pub summary: String,
    pub command: Command,
    pub requested_by: Origin,
    /// Unix ms when the token expires (then `ConfirmationResolved` with `approved: false`).
    pub expires_at_ms: i64,
    /// What kind of operation it is: its class and command, as a grant names them. With
    /// `scope`, enough to offer "allow this agent to do this here" (a `GrantPermission` for
    /// the requester's session, then `Confirm`).
    #[serde(default)]
    pub operation: Option<OperationInfo>,
    /// What the command acts on, as a scope: the smallest single scope that contains every
    /// one of its targets (a library folder or a part of it, or a location such as the file
    /// or folder it names, in the platform layer's normal form: the location core acts
    /// on; a track's file that a link in its library folder leads out of it, or elsewhere
    /// in it, by that location). `null` when there is no such scope short of `all`: the
    /// command names nothing in particular (`PurgePlaylists`), its targets lie apart (in
    /// several library folders, or such a file with others), or a file it writes cannot
    /// be read now (it is skipped, `summary` says so). A grant for this scope covers the
    /// command.
    #[serde(default)]
    pub scope: Option<Scope>,
    /// `scope` for the user to read (a folder's path, the file core will write, where it
    /// really is), from the platform layer; `null` when there is nothing to show.
    #[serde(default)]
    pub display_location: Option<String>,
}

/// The class and command of a held operation (`PendingConfirmation.operation`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OperationInfo {
    /// The class the command counts as here: an undo of a folder change counts as
    /// `local_path` (guarantee 12).
    pub class: OperationClass,
    /// The command's `type`.
    pub command: String,
}
