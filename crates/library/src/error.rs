//! Library errors and shared database damage tracking.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::{ActionId, ActionTarget, RootId, TrackId};

/// Why a library call failed.
///
/// Every error has a stable snake_case [`crate::LibraryError::code`] that clients can branch on;
/// messages include English for logs and agents. Selected codes (see `code()` for all variants):
///
/// | Code | Variant | Meaning |
/// |---|---|---|
/// | `db` | [`Db`](LibraryError::Db) | SQLite failed |
/// | `db` | [`DatabaseBusy`](LibraryError::DatabaseBusy) | SQLite reported contention during open or inspection |
/// | `db` | [`WalUnavailable`](LibraryError::WalUnavailable) | SQLite could not enable WAL |
/// | `library_damaged` | `Db` | SQLite found the database file damaged, or not a database ([`crate::LibraryError::is_damaged`]) |
/// | `storage` | [`Storage`](LibraryError::Storage) | invalid storage configuration or a backend-specific failure |
/// | `io` | [`Io`](LibraryError::Io) | file system failed |
/// | `schema_too_new` | [`SchemaTooNew`](LibraryError::SchemaTooNew) | the database is from a newer build |
/// | `not_a_directory` | [`NotADirectory`](LibraryError::NotADirectory) | a root that is not a folder |
/// | `root_overlaps` | [`OverlappingRoot`](LibraryError::OverlappingRoot) | a root inside or around an active root |
/// | `path_not_unicode` | [`NonUnicodePath`](LibraryError::NonUnicodePath) | a root path that is not valid Unicode |
/// | `invalid_query` | [`InvalidQuery`](LibraryError::InvalidQuery) | a track query that does not type-check (also a stored smart query) |
/// | `playlist_conflict` | [`PlaylistConflict`](LibraryError::PlaylistConflict) | `expected_revision` is not the playlist's revision |
/// | `action_conflict` | [`ActionConflict`](LibraryError::ActionConflict) | an undo whose objects changed since the action |
/// | `already_exists` | [`AlreadyExists`](LibraryError::AlreadyExists) | an M3U export target exists |
/// | `playlist_not_found` | [`NotFound`](LibraryError::NotFound) | unknown or purged playlist |
/// | `track_not_found` | `NotFound` | unknown track id |
/// | `item_not_found` | `NotFound` | a playlist item that is not in the playlist |
/// | `root_not_found` | `NotFound` | unknown root, or a removed one (except for removing it again) |
/// | `nothing_to_undo` | `NotFound` | the playlist has no (unexpired) edit to undo |
/// | `action_not_found` | `NotFound` | an unknown action, or one that can no longer be undone (expired, beyond its playlist's last 50 steps, unreadable) |
/// | `file_not_found` | `NotFound` | an M3U file to import that does not exist |
/// | `playlist_kind_mismatch` | [`Invalid`](LibraryError::Invalid) | an item edit on a smart playlist, a query on a manual one |
/// | `out_of_range` | `Invalid` | a position past the end, a rating outside 1–5 |
/// | `blank_name` | `Invalid` | an empty playlist name |
/// | `not_in_trash` | `Invalid` | purging a playlist that is not in the trash |
/// | `playlist_in_trash` | `Invalid` | editing (or undoing an edit of) a playlist in the trash |
/// | `already_undone` | `Invalid` | undoing an action that was undone already (undo its undo to redo it) |
/// | `not_an_m3u` | `Invalid` | an M3U path without `.m3u`/`.m3u8` |
/// | `not_a_regular_file` | `Invalid` | an M3U export target that is a folder, symlink or device; an M3U file to import that is a folder, FIFO or device |
/// | `unknown_encoding` | `Invalid` | an unknown text encoding label |
/// | `location_changed` | `Invalid` | a location checked for the caller ([`crate::ActionMeta::checked`]) no longer leads where it did (a symlink put in its way) |
/// | `invalid_tag` | `Invalid` | a tag change that is not one ([`crate::check_changes`]) |
/// | `file_in_use` | [`ExternalInUse`](LibraryError::ExternalInUse) | undo preflight found a sharing conflict with another program |
/// | `file_in_use` | [`InUse`](LibraryError::InUse) | undo preflight found a file held by playback |
/// | `corrupt_data` | [`Corrupt`](LibraryError::Corrupt) | a stored record that cannot be read (an action's undo data: reported once, then the action is no longer undoable) |
/// | `cancelled` | [`Cancelled`](LibraryError::Cancelled) | internal: a scan was cancelled |
/// | `writer_stopped` | [`WriterStopped`](LibraryError::WriterStopped) | the writer thread is gone (shutting down) |
/// | `scan_panicked` | [`ScanPanicked`](LibraryError::ScanPanicked) | the scan thread panicked |
#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error(transparent)]
    Storage(#[from] mpmp_storage::StorageError),
    #[error("cover recovery is required before writing this file")]
    CoverRecoveryRequired,
    #[error("cover recovery intent not found")]
    CoverRecoveryNotFound,
    #[error(
        "cover recovery target or candidate changed; explicitly select intent, role and full SHA-256 when versions differ"
    )]
    CoverRecoveryChanged,
    #[error("cover input: {0}")]
    CoverInput(&'static str),
    #[error("trash selection changed; review permanent deletion again")]
    PurgeChanged,
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("曲库当前正被占用，请稍后重试。\nThe library database is busy. Try again shortly.")]
    DatabaseBusy(#[source] rusqlite::Error),
    #[error(
        "无法为曲库启用 WAL。SQLite 返回的日志模式为 {mode}。\nSQLite could not enable WAL for the library database. Reported journal mode is {mode}."
    )]
    WalUnavailable { mode: String },
    #[error("library database schema version {found} is newer than this build ({supported})")]
    SchemaTooNew { found: i64, supported: i64 },
    #[error("{} is not a directory", .0.display())]
    NotADirectory(PathBuf),
    #[error("{} overlaps the existing library root {}", path.display(), existing.display())]
    OverlappingRoot { path: PathBuf, existing: PathBuf },
    #[error("library root path {} is not valid Unicode", .0.display())]
    NonUnicodePath(PathBuf),
    /// The scan was cancelled (internal: [`crate::ScanHandle::wait`] reports a cancelled scan as
    /// a [`crate::ScanReport`] with `cancelled` set).
    #[error("the library scan was cancelled")]
    Cancelled,
    #[error("the library writer thread has stopped")]
    WriterStopped,
    #[error("the library is closing or closed")]
    Closed,
    #[error("cannot wait for library close from its own file job or writer")]
    ReentrantClose,
    #[error("WAL checkpoint incomplete (busy={busy}, log={log}, checkpointed={checkpointed})")]
    CheckpointIncomplete {
        busy: i64,
        log: i64,
        checkpointed: i64,
    },
    #[error(
        "{count} accepted library write(s) failed (first: {first}); database close: {database:?}"
    )]
    WriteShutdown {
        count: usize,
        first: String,
        database: Option<Box<LibraryError>>,
    },
    #[error("library file job failed during shutdown: {details}; database close: {database:?}")]
    FileJobShutdown {
        details: String,
        database: Option<Box<LibraryError>>,
    },
    /// The user released this folder until explicitly resumed (session-local).
    #[error("library root {0} is released")]
    RootReleased(RootId),
    /// A track query that does not type-check; the message names the field and operation.
    #[error("invalid query: {0}")]
    InvalidQuery(String),
    /// Something the request names does not exist (or can no longer be edited); `what`
    /// says which kind of thing, the message which one.
    #[error("not found: {message}")]
    NotFound { what: NotFound, message: String },
    /// A playlist edit's `expected_revision` (or an undo's revision) is not the playlist's
    /// current revision: someone else changed it meanwhile.
    #[error("playlist changed: expected revision {expected}, it is at {actual}")]
    PlaylistConflict { expected: u64, actual: u64 },
    /// An undo of `action` whose objects were changed since (by later actions): `changed`
    /// lists those, sorted. Nothing was undone.
    #[error("action {action} cannot be undone: {} of its objects changed since", changed.len())]
    ActionConflict {
        action: ActionId,
        changed: Vec<ActionTarget>,
    },
    /// A request that cannot apply to its target; `why` gives the code, the message the
    /// details.
    #[error("invalid request: {message}")]
    Invalid { why: Invalid, message: String },
    /// An M3U export to a file that exists, without `overwrite`.
    #[error("{} already exists", .0.display())]
    AlreadyExists(PathBuf),
    /// A stored record (e.g. the undo data of an action) that this build cannot read.
    #[error("corrupt library data: {0}")]
    Corrupt(String),
    #[error("the library scan thread panicked")]
    ScanPanicked,
    /// Undo preflight found files held by playback.
    #[error("{} of the files to write back are in use by playback", .0.len())]
    InUse(Vec<TrackId>),
    /// Undo preflight found an OS sharing conflict. Also lists any targets held by playback.
    #[error("{} undo targets are in use, including an external sharing conflict", .0.len())]
    ExternalInUse(Vec<TrackId>),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// What [`crate::LibraryError::NotFound`] did not find.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NotFound {
    /// Unknown or purged playlist.
    Playlist,
    Track,
    /// A playlist item that is not in the playlist.
    Item,
    /// Unknown or removed library root.
    Root,
    /// No (unexpired) playlist edit left to undo.
    Undo,
    /// An unknown action, or one that can no longer be undone.
    Action,
    /// A file to read (an M3U to import) that does not exist.
    File,
}

impl NotFound {
    pub const fn code(self) -> &'static str {
        match self {
            NotFound::Playlist => "playlist_not_found",
            NotFound::Track => "track_not_found",
            NotFound::Item => "item_not_found",
            NotFound::Root => "root_not_found",
            NotFound::Undo => "nothing_to_undo",
            NotFound::Action => "action_not_found",
            NotFound::File => "file_not_found",
        }
    }
}

/// Why [`crate::LibraryError::Invalid`] rejected a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Invalid {
    /// Items of a smart playlist, or the query of a manual one.
    PlaylistKindMismatch,
    /// A position past the end, or a rating outside 1–5.
    OutOfRange,
    /// An empty (or blank) playlist name.
    BlankName,
    /// Restoring or purging a playlist that is not in the trash.
    NotInTrash,
    /// An M3U path whose name does not end in `.m3u` / `.m3u8`.
    NotAnM3u,
    /// An M3U export target that exists and is not a regular file, or an M3U file to
    /// import that is not one.
    NotARegularFile,
    /// An unknown text encoding label.
    UnknownEncoding,
    /// An edit of a playlist that is in the trash.
    PlaylistInTrash,
    /// An undo of an action that was undone already.
    AlreadyUndone,
    /// A location checked for the caller ([`crate::ActionMeta::checked`]) that is no longer in
    /// canonical form, or no longer leads to what was opened or checked: something (a
    /// symlink) was put in its way since. Nothing was read or written.
    LocationChanged,
    /// A tag change that is not one ([`crate::check_changes`]).
    InvalidTag,
}

impl Invalid {
    pub const fn code(self) -> &'static str {
        match self {
            Invalid::PlaylistKindMismatch => "playlist_kind_mismatch",
            Invalid::OutOfRange => "out_of_range",
            Invalid::BlankName => "blank_name",
            Invalid::NotInTrash => "not_in_trash",
            Invalid::NotAnM3u => "not_an_m3u",
            Invalid::NotARegularFile => "not_a_regular_file",
            Invalid::UnknownEncoding => "unknown_encoding",
            Invalid::PlaylistInTrash => "playlist_in_trash",
            Invalid::AlreadyUndone => "already_undone",
            Invalid::LocationChanged => "location_changed",
            Invalid::InvalidTag => "invalid_tag",
        }
    }
}

impl LibraryError {
    /// The stable snake_case code of this error (see the table on [`crate::LibraryError`]).
    pub fn code(&self) -> &'static str {
        match self {
            LibraryError::Storage(_) => "storage",
            LibraryError::CoverRecoveryRequired => "cover_recovery_required",
            LibraryError::CoverRecoveryNotFound => "cover_recovery_not_found",
            LibraryError::CoverRecoveryChanged => "cover_recovery_changed",
            LibraryError::CoverInput(code) => code,
            LibraryError::Db(_) if self.is_damaged() => "library_damaged",
            LibraryError::Db(_)
            | LibraryError::DatabaseBusy(_)
            | LibraryError::WalUnavailable { .. } => "db",
            LibraryError::SchemaTooNew { .. } => "schema_too_new",
            LibraryError::NotADirectory(_) => "not_a_directory",
            LibraryError::OverlappingRoot { .. } => "root_overlaps",
            LibraryError::NonUnicodePath(_) => "path_not_unicode",
            LibraryError::Cancelled => "cancelled",
            LibraryError::WriterStopped | LibraryError::Closed | LibraryError::ReentrantClose => {
                "writer_stopped"
            }
            LibraryError::CheckpointIncomplete { .. }
            | LibraryError::WriteShutdown { .. }
            | LibraryError::FileJobShutdown { .. } => "db",
            LibraryError::RootReleased(_) => "root_released",
            LibraryError::InvalidQuery(_) => "invalid_query",
            LibraryError::NotFound { what, .. } => what.code(),
            LibraryError::PlaylistConflict { .. } | LibraryError::PurgeChanged => {
                "playlist_conflict"
            }
            LibraryError::ActionConflict { .. } => "action_conflict",
            LibraryError::Invalid { why, .. } => why.code(),
            LibraryError::AlreadyExists(_) => "already_exists",
            LibraryError::Corrupt(_) => "corrupt_data",
            LibraryError::ScanPanicked => "scan_panicked",
            LibraryError::InUse(_) | LibraryError::ExternalInUse(_) => "file_in_use",
            LibraryError::Io(_) => "io",
        }
    }

    /// Whether SQLite found the database file damaged (`SQLITE_CORRUPT`) or not a database
    /// at all (`SQLITE_NOTADB`): retrying will not help; the database needs replacing. The
    /// library never deletes or rebuilds it itself.
    pub fn is_damaged(&self) -> bool {
        matches!(
            self,
            LibraryError::Db(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                )
        )
    }

    pub(crate) fn not_found(what: NotFound, message: impl Into<String>) -> Self {
        LibraryError::NotFound {
            what,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(why: Invalid, message: impl Into<String>) -> Self {
        LibraryError::Invalid {
            why,
            message: message.into(),
        }
    }
}

pub type Result<T, E = LibraryError> = std::result::Result<T, E>;

/// Whether this library's database was found damaged ([`crate::LibraryError::is_damaged`]), shared
/// by the only two ways into the database: the writer thread (every write, also those whose
/// failures are only logged, such as play events and scan batches) and the read pool
/// (every read). Set once, by the first damage seen; [`crate::Library::damaged`] reports it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Damage(Arc<OnceLock<String>>);

impl Damage {
    /// Notes `result` if it says the database is damaged; returns it unchanged.
    pub fn watch<T>(&self, result: Result<T>) -> Result<T> {
        if let Err(err) = &result
            && err.is_damaged()
            && self.0.set(err.to_string()).is_ok()
        {
            tracing::error!(%err, "the library database is damaged");
        }
        result
    }

    pub fn get(&self) -> Option<&str> {
        self.0.get().map(String::as_str)
    }
}
