//! Contract errors, stable codes, and structured error details.

use super::*;

/// Why a command or query failed. `kind` says what a client should do (closed set);
/// `code` says exactly what happened (open set: new codes may be added in a minor
/// version; clients treat unknown codes by their `kind`); `message` is English text for
/// logs and agents (UIs localize by `code`); `details` carries structured data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContractError {
    pub kind: ErrorKind,
    pub code: ErrorCode,
    pub message: String,
    // Boxed to keep `Result<_, ContractError>` small.
    #[serde(default, skip_serializing_if = "ErrorDetails::is_empty")]
    pub details: Box<ErrorDetails>,
}

/// What a client should do about an error. Closed: no kinds are added after v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorKind {
    /// Someone changed it meanwhile (a revision, the current entry): refresh and retry.
    Conflict,
    /// An id that does not exist (or no longer does).
    NotFound,
    /// The request is wrong: fix it.
    Invalid,
    /// This origin may not do this.
    Forbidden,
    /// This core does not support it (an unknown command, field or variant; not
    /// implemented).
    Unsupported,
    /// It went wrong (I/O, database, decoding); `details.retryable` says whether trying
    /// again may help.
    Failed,
    /// Waits for the user: see `details.confirmation`; do not resend.
    NeedsConfirmation,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// The exact error. Open: codes may be added in minor versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCode {
    /// Cover input is not a valid supported JPEG/PNG, canonical base64, or supported dimensions.
    InvalidImage,
    /// Cover input exceeds the published image byte, pixel or edge limit.
    ImageTooLarge,
    /// This build has no implementation for the requested cover edit representation.
    CoverEditUnsupported,
    /// An unresolved durable cover write needs trusted UI recovery before this file can be edited.
    CoverRecoveryRequired,
    /// No unfinished recovery intent protects this track or its actual file.
    CoverRecoveryNotFound,
    /// The recovery selection or current file changed after the UI confirmation.
    CoverRecoveryChanged,
    // conflict
    /// `expected_revision` (or `Undo.revision`) is not the queue's revision.
    QueueConflict,
    /// A playlist revision differs, or `Play` cannot resolve its guarded playlist item.
    PlaylistConflict,
    /// `Seek.entry` or `Enqueue.expected_current` is not the current entry.
    EntryNotCurrent,
    /// `UndoAction`: another action changed some of the action's objects since
    /// (`details.changed` lists them); nothing was undone. Undoing a tag edit: also a file
    /// changed (its size or modification time) or gone since the edit wrote it.
    ActionConflict,
    /// Tag editing (a file's code in `skipped`): the file changed since the library last
    /// read it (its size or modification time): it is not written, so that nothing
    /// overwrites another program's change. A rescan reads it again.
    FileChanged,
    /// Tag editing (a file's code in `skipped`): the library has to read the file again
    /// before it is written (its music folder's text encoding changed since the last
    /// scan): it is not written. A rescan reads it again.
    RescanNeeded,
    // not_found
    /// No job with this ID is retained in this run (unknown, evicted, or from an earlier run).
    JobNotFound,
    EntryNotFound,
    TrackNotFound,
    PlaylistNotFound,
    /// A playlist item that is not in the playlist.
    ItemNotFound,
    RootNotFound,
    RootReleased,
    AlbumNotFound,
    /// Answered, expired or never issued.
    ConfirmationNotFound,
    /// Nothing (left) to undo for that revision.
    NothingToUndo,
    /// The play context has no playable tracks.
    EmptyContext,
    /// An action that is unknown or can no longer be undone (expired, beyond its
    /// playlist's last 50 edits, its playlist purged).
    ActionNotFound,
    /// A file to read (an M3U to import) that does not exist.
    FileNotFound,
    /// No registered agent of that name.
    AgentNotFound,
    /// A grant that does not exist (revoked, expired, or a session's grant from before
    /// core restarted).
    GrantNotFound,
    // invalid
    InvalidJson,
    /// A missing field, a wrong type or an impossible value in the request.
    InvalidRequest,
    InvalidQuery,
    OutOfRange,
    /// `Seek` with no current entry.
    NothingPlaying,
    NotADirectory,
    RootOverlaps,
    PathNotUnicode,
    PlaylistKindMismatch,
    NotInTrash,
    /// Editing (or undoing an edit of) a playlist in the trash: restore it first.
    PlaylistInTrash,
    /// `UndoAction` of an action that was undone already (undo its undo to redo it).
    AlreadyUndone,
    BlankName,
    NotAnM3u,
    NotARegularFile,
    UnknownEncoding,
    AlreadyExists,
    IdempotencyKeyReused,
    /// `GrantPermission`: a grant that could never apply. `allow`: permanently deleting
    /// library data, the user's own commands, or what an agent may do without asking anyway
    /// (`playback`, `library_edit`, such commands); `forbid`: the user's own commands.
    NotGrantable,
    /// An agent name that is blank, too long or has characters other than letters,
    /// digits, `-`, `_` and `.` (or, in `GrantPermission`, not such a name or
    /// `<platform>:<name>`).
    InvalidAgentName,
    /// A location the platform layer cannot read as one (a relative path, `..` past what
    /// exists, a symlink to nothing …): a grant's `scope.location`, or the path an agent
    /// names (which also may not have a `..` part or a control character).
    InvalidLocation,
    /// `GrantPermission`: scope.location is not already the reviewed platform-normal
    /// location. Applies to every expiry, including session grants. Obtain the normal
    /// form from core or the host platform layer, review it, then submit again.
    /// `details.field` is command.scope.location.
    LocationNotNormal,
    /// A location an agent named no longer leads where it did when it was checked (and
    /// shown to the user): something, such as a symlink, was put in its way while the
    /// command waited for the user, or just before it ran. Nothing was done. (Tag editing,
    /// a file's code in `failed`: that file was not written.)
    LocationChanged,
    /// A tag change that is not one: a key that is blank, longer than 64 characters, not
    /// printable ASCII, has `=` or `:`, or is a picture or rating key (`RATING`,
    /// `METADATA_BLOCK_PICTURE` …), or another name of a key the library shows under one
    /// name (`TPE1`, `aART`: `ARTIST`); a key changed twice; a `set` with no values, more
    /// than 64, or a value that is blank, has NUL or is over 8 KiB; more than 64 changes.
    /// `details.field` names the change, key or value.
    InvalidTag,
    /// Tag editing (a file's code in `failed`; in a preview's `skipped`): the edit's undo
    /// data (the values to write back, and those to write again to redo it) would exceed
    /// 8 MiB for the job: the file is not written, so that every file written can be
    /// undone. Edit fewer files at once.
    UndoTooLarge,
    /// Tag editing (a file's code in `failed`; in a preview's `skipped`): an undo could
    /// not write the file's present values back (its format cannot store them as the
    /// library reads them, a date that is no date, say): it is not written, so that
    /// every file written can be undone.
    NotUndoable,
    /// `GrantPermission` for an agent's session that has ended already (its adapter's
    /// connection closed, `end_agent_session`): the grant could never apply. An app that
    /// offers "allow for this session" should tell the user the session is over.
    SessionEnded,
    // forbidden
    OriginNotAllowed,
    /// Local file system paths are not allowed from this origin.
    PathNotAllowed,
    ConfirmationRejected,
    ConfirmationExpired,
    /// The user forbade this agent this operation (a grant with `effect: forbid`).
    PermissionDenied,
    // unsupported
    UnknownCommand,
    UnknownQuery,
    UnknownField,
    UnknownVariant,
    NotImplemented,
    // needs_confirmation
    NeedsConfirmation,
    // failed
    CoreStopped,
    Db,
    Io,
    SchemaTooNew,
    CorruptData,
    Cancelled,
    WriterStopped,
    ScanPanicked,
    /// A loudness scan already runs; cancel it or wait and retry (`retryable`).
    JobBusy,
    /// The file is gone.
    FileMissing,
    /// Tag editing (a file's code in `skipped`): the player has the file open (the track
    /// being heard, or the one prepared to follow it): it is not written now. An undo of a
    /// tag edit that would write one fails with it, `retryable` (nothing was written).
    FileInUse,
    /// Tag editing (a file's code in `skipped`): a track of a CUE sheet. Its tags come from
    /// the sheet and it shares its audio file with the sheet's other tracks; writing them
    /// to the sheet is not done yet, so it is not written.
    CueTrack,
    /// Tag editing (a file's code in `failed`): the file could not be written (read-only,
    /// no room, its folder not writable …); it is as it was.
    WriteFailed,
    /// The file has its read-only attribute set; cover previews exclude it. Legacy tag editing retains failed/write_failed at execution.
    FileReadOnly,
    /// The library no longer vouches for the track (missing at the last scan, root
    /// removed).
    TrackUnavailable,
    OpenFailed,
    UnsupportedFormat,
    DecodeError,
    OutputFailed,
    /// expected_revision differs from the current output preference revision; re-read before retrying.
    OutputSettingsConflict,
    /// A device capability query could not resolve its requested identity. Selection commands accept unseen IDs.
    OutputDeviceNotFound,
    /// The selected endpoint cannot currently be used; preferences stay committed and OutputState reports the failure.
    OutputDeviceUnavailable,
    /// The build/backend cannot implement this mode or rate policy at all. A setting command refuses it before committing preferences; device-specific failures use OutputState.failure/fallback instead.
    OutputModeUnsupported,
    /// Runtime format negotiation cannot satisfy the committed policy on this endpoint. Kind failed; reported in OutputState.failure, or as format_unsupported fallback. It does not reject SetOutputDevice.
    OutputFormatUnsupported,
    /// Exclusive access could not be acquired at runtime, for example because the endpoint is busy. Report in OutputState.failure/fallback after committing a supported preference, never as a static mode rejection.
    ExclusiveUnavailable,
    /// The agents and grants could not be read when core started: every command from an
    /// agent fails with this until core restarts with readable data (`Snapshot.problems`).
    PermissionsUnavailable,
    /// The library database is damaged (or not a database): retrying will not help. Also
    /// in `Snapshot.problems` from the first time until core restarts.
    LibraryDamaged,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    /// Stable error classification, also exported as x-kind on every Schema code.
    pub fn kind(self) -> ErrorKind {
        match self {
            Self::CoverRecoveryChanged
            | Self::OutputSettingsConflict
            | Self::QueueConflict
            | Self::PlaylistConflict
            | Self::EntryNotCurrent
            | Self::ActionConflict
            | Self::FileChanged
            | Self::RescanNeeded => ErrorKind::Conflict,
            Self::CoverRecoveryNotFound
            | Self::OutputDeviceNotFound
            | Self::JobNotFound
            | Self::EntryNotFound
            | Self::TrackNotFound
            | Self::PlaylistNotFound
            | Self::ItemNotFound
            | Self::RootNotFound
            | Self::AlbumNotFound
            | Self::ConfirmationNotFound
            | Self::NothingToUndo
            | Self::EmptyContext
            | Self::ActionNotFound
            | Self::FileNotFound
            | Self::AgentNotFound
            | Self::GrantNotFound => ErrorKind::NotFound,
            Self::InvalidImage
            | Self::ImageTooLarge
            | Self::InvalidJson
            | Self::InvalidRequest
            | Self::InvalidQuery
            | Self::OutOfRange
            | Self::NothingPlaying
            | Self::NotADirectory
            | Self::RootOverlaps
            | Self::PathNotUnicode
            | Self::PlaylistKindMismatch
            | Self::NotInTrash
            | Self::PlaylistInTrash
            | Self::AlreadyUndone
            | Self::BlankName
            | Self::NotAnM3u
            | Self::NotARegularFile
            | Self::UnknownEncoding
            | Self::AlreadyExists
            | Self::IdempotencyKeyReused
            | Self::NotGrantable
            | Self::InvalidAgentName
            | Self::InvalidLocation
            | Self::LocationNotNormal
            | Self::LocationChanged
            | Self::InvalidTag
            | Self::UndoTooLarge
            | Self::NotUndoable
            | Self::SessionEnded => ErrorKind::Invalid,
            Self::OriginNotAllowed
            | Self::PathNotAllowed
            | Self::ConfirmationRejected
            | Self::ConfirmationExpired
            | Self::PermissionDenied => ErrorKind::Forbidden,
            Self::CoverEditUnsupported
            | Self::OutputModeUnsupported
            | Self::UnknownCommand
            | Self::UnknownQuery
            | Self::UnknownField
            | Self::UnknownVariant
            | Self::NotImplemented => ErrorKind::Unsupported,
            Self::NeedsConfirmation => ErrorKind::NeedsConfirmation,
            Self::CoverRecoveryRequired
            | Self::OutputFormatUnsupported
            | Self::OutputDeviceUnavailable
            | Self::ExclusiveUnavailable
            | Self::RootReleased
            | Self::CoreStopped
            | Self::Db
            | Self::Io
            | Self::SchemaTooNew
            | Self::CorruptData
            | Self::Cancelled
            | Self::WriterStopped
            | Self::ScanPanicked
            | Self::JobBusy
            | Self::FileMissing
            | Self::FileInUse
            | Self::CueTrack
            | Self::FileReadOnly
            | Self::WriteFailed
            | Self::TrackUnavailable
            | Self::OpenFailed
            | Self::UnsupportedFormat
            | Self::DecodeError
            | Self::OutputFailed
            | Self::PermissionsUnavailable
            | Self::LibraryDamaged => ErrorKind::Failed,
            Self::Unknown => ErrorKind::Unknown,
        }
    }

    /// The snake_case JSON name.
    pub fn as_str(self) -> String {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::String(s)) => s,
            _ => "unknown".into(),
        }
    }

    /// The code with this JSON name; `Unknown` if there is none.
    pub fn parse(code: &str) -> Self {
        serde_json::from_value(serde_json::Value::String(code.to_owned()))
            .unwrap_or(ErrorCode::Unknown)
    }
}

/// Structured data of an error; fields that do not apply are left out of the JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorDetails {
    /// `conflict`: the revision (or entry id) the request expected …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<u64>,
    /// … and the one it found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual: Option<u64>,
    /// `not_found`: the objects that do not exist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<ObjectRef>,
    /// `action_conflict`: the objects another action changed since the one being undone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<ObjectRef>,
    /// The request field at fault, as a JSON path (`command.position_ms`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// JSON syntax error: 1-based input line. Absent for errors without a parser position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// JSON syntax error: 1-based input column, paired with line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    /// `failed`: trying again later may succeed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retryable: bool,
    /// A committed directory undo cannot be repeated: use this command to finish
    /// participant recovery for the roots in the action instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_command: Option<String>,
    /// `needs_confirmation`: the token the user answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<ConfirmationTicket>,
    /// `permission_denied`: the user's `forbid` grants that refused the command, by id (at
    /// most 8; the message says how many more there were), and how much of what the command
    /// acts on each forbids; never where they are (the places a user forbids an agent are
    /// not the agent's to learn). Where none forbids all of it, an agent may still ask for
    /// the rest a part at a time (a subfolder, say).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbidden_by: Vec<ForbiddenBy>,
}

/// A `forbid` grant that refused an agent's command (`ErrorDetails.forbidden_by`): what it
/// forbids, and how much of what the command acts on; not its scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ForbiddenBy {
    pub grant: GrantId,
    pub operation: Operation,
    pub covers: Covers,
}

/// How much of what a refused command acts on a `forbid` grant covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Covers {
    /// All of it (the location it names is inside the forbidden place, or the command acts
    /// on nothing in particular): asking again for a part of it is refused too.
    All,
    /// A part of it (the folder it names holds a forbidden place): what lies outside that
    /// part may still be asked for, a part at a time (a subfolder, say).
    Part,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

impl ErrorDetails {
    pub fn is_empty(&self) -> bool {
        self == &ErrorDetails::default()
    }
}

/// A pending confirmation as an error reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConfirmationTicket {
    pub token: String,
    pub summary: String,
    /// Unix ms when the token expires.
    pub expires_at_ms: i64,
}

/// An object an error refers to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ObjectRef {
    Entry {
        id: EntryId,
    },
    Track {
        id: TrackId,
    },
    Playlist {
        id: PlaylistId,
    },
    PlaylistItem {
        id: PlaylistItemId,
    },
    Root {
        id: RootId,
    },
    Album {
        key: String,
    },
    Confirmation {
        token: String,
    },
    Job {
        id: JobId,
    },
    Action {
        id: ActionId,
    },
    Agent {
        name: String,
    },
    Grant {
        id: GrantId,
    },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

impl ContractError {
    pub fn new(kind: ErrorKind, code: ErrorCode, message: impl Into<String>) -> Self {
        if kind != ErrorKind::Unknown && code != ErrorCode::Unknown {
            debug_assert_eq!(kind, code.kind(), "error kind must match its code");
        }
        Self {
            kind,
            code,
            message: message.into(),
            details: Box::default(),
        }
    }

    pub fn invalid(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, code, message)
    }

    pub fn not_found(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, code, message)
    }

    pub fn unsupported(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, code, message)
    }

    pub fn forbidden(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Forbidden, code, message)
    }

    pub fn failed(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Failed, code, message)
    }

    pub fn conflict(
        code: ErrorCode,
        message: impl Into<String>,
        expected: u64,
        actual: u64,
    ) -> Self {
        let mut e = Self::new(ErrorKind::Conflict, code, message);
        e.details.expected = Some(expected);
        e.details.actual = Some(actual);
        e
    }

    /// An `unknown_variant` error for an `Unknown` value found in a request.
    pub fn unknown_variant(what: &str) -> Self {
        Self::unsupported(
            ErrorCode::UnknownVariant,
            format!("this core does not know that {what}"),
        )
    }

    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.details.field = Some(field.into());
        self
    }

    pub fn with_missing(mut self, missing: Vec<ObjectRef>) -> Self {
        self.details.missing = missing;
        self
    }

    pub fn with_forbidden_by(mut self, forbidden_by: Vec<ForbiddenBy>) -> Self {
        self.details.forbidden_by = forbidden_by;
        self
    }

    pub fn with_changed(mut self, changed: Vec<ObjectRef>) -> Self {
        self.details.changed = changed;
        self
    }

    pub fn retryable(mut self) -> Self {
        self.details.retryable = true;
        self
    }
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ContractError {}
