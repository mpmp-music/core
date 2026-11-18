//! Agent permissions, grants, scopes, and expiry.

use super::*;

/// Kinds of operations, for grants (`Operation`) and confirmations. Each command has one
/// (the schema's `x-class`). Open: more classes may come.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OperationClass {
    /// Playback, the queue and audio settings.
    Playback,
    /// Library data that can be undone: ratings, loved flags, playlists (also the trash),
    /// undoing actions, rescans.
    LibraryEdit,
    /// Local locations: adding and removing library folders (and undoing that), importing
    /// M3U files.
    LocalPath,
    /// Writing the user's files: exporting an M3U file (which may replace one), editing
    /// tags (and undoing that).
    FileWrite,
    /// Permanently deleting library data (`PurgePlaylists`). Never allowed by a grant
    /// (only forbidden).
    Purge,
    /// The user's own commands (confirmations, agents, grants). UI only: no grant names
    /// them.
    Permissions,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// What a grant covers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Operation {
    /// One command, by its `type` (`add_root`).
    Command { command: String },
    /// Every command of a class.
    Class { class: OperationClass },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// Where a grant applies: what a command acts on must be inside it (see each command's
/// target: a library folder, or a location).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Scope {
    /// Everything.
    All,
    /// A library folder, or a part of it: what is under `prefix`, a `/`-separated location
    /// relative to the root (as `TrackInfo.path`; parts that are empty, `.`, `..` or hold
    /// a backslash are `invalid_request`; at most 1024 characters; stored without leading and
    /// trailing `/`; it must be a place the platform can have in the folder), and any
    /// location inside that place in the folder's location (for `allow`, where the place
    /// really is, which must be inside the folder); `null` (or omitted): the whole folder
    /// and any location inside the folder's location.
    /// Letter case counts as on the folder's file system (where that cannot be told: for
    /// `allow` it counts, for `forbid` it does not).
    Library {
        root: RootId,
        #[serde(default)]
        prefix: Option<String>,
    },
    /// A location (a folder or a file, as `AddRoot.path`; at most 4096 characters) and
    /// everything inside it, as the platform layer decides. Stored in the platform layer's
    /// normal form (desktop: canonical paths, symlinks resolved). `GrantPermission` must
    /// submit this reviewed normal form: if normalization would change it, the grant is
    /// refused with `location_not_normal` (including session grants); one it cannot read is `invalid_location`.
    Location { location: String },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// How long a grant lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Expiry {
    /// Until core stops (the app quits); with the grant's `session`, for that session.
    /// Kept in memory only.
    Session,
    /// Until this Unix time (ms); must be in the future.
    At { at_ms: i64 },
    /// Until revoked.
    Forever,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// What a grant does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Effect {
    /// Run without asking the user.
    Allow,
    /// Refuse (`forbidden` / `permission_denied`) without asking the user.
    Forbid,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// A permission grant (`GrantPermission`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Grant {
    pub id: GrantId,
    /// The agent's name.
    pub agent: String,
    /// Only this session of the agent; `null`: every session.
    pub session: Option<String>,
    pub operation: Operation,
    pub scope: Scope,
    pub until: Expiry,
    pub effect: Effect,
    /// Unix ms.
    pub created_at_ms: i64,
}

/// A registered agent (`RegisterAgent`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentInfo {
    pub name: String,
    /// Unix ms.
    pub created_at_ms: i64,
    /// Unix ms of the last time its adapter presented its token; `null`: never.
    pub last_seen_at_ms: Option<i64>,
}
