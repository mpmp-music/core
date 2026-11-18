//! Defines the file, identity, capacity, and replacement interfaces used by the library.
//!
//! Backends supply platform policy without introducing platform dependencies here. File
//! identity snapshots keep their two-integer encoding, while live entry identities may
//! retain handles. Replacement failures must leave registered recovery paths available
//! to the caller; an error does not imply that publication had no effect.
//! See docs/design/storage.md for ownership and recovery sequencing.

#![warn(missing_docs)]

use std::{
    any::Any,
    fmt::Debug,
    fs::{File, Metadata},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

/// A local path supplied by a backend; construction alone performs no validation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalPath(
    /// The path in the backend's local namespace.
    pub PathBuf,
);

/// A platform-normal location string used for permission comparisons.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PermissionLocation(
    /// The location text; validation belongs to the platform adapter.
    pub String,
);

/// A handle-free file identity snapshot with a stable two-integer representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity(
    /// Backend identity components; meaningful only for the same backend.
    pub [u64; 2],
);

impl FileIdentity {
    /// Returns both components unchanged; this does not query the file system.
    pub fn parts(self) -> [u64; 2] {
        self.0
    }
}

/// A comparable live identity; implementations may retain handles until dropped.
/// Unlike `FileIdentity`, this must preserve backend-specific resource lifetimes.
pub trait EntryIdentity: Debug + Send + Sync + 'static {
    /// Exposes the concrete identity for backend-specific equality checks.
    fn as_any(&self) -> &dyn Any;
    /// Returns whether both identities name the same entry; incompatible backends return false.
    fn same(&self, other: &dyn EntryIdentity) -> bool;
}

/// A live identity whose equality delegates to the owning backend.
#[derive(Debug)]
pub struct EntryId(
    /// The backend identity, retained for the lifetime of this wrapper.
    pub Box<dyn EntryIdentity>,
);

impl PartialEq for EntryId {
    fn eq(&self, other: &Self) -> bool {
        self.0.same(other.0.as_ref())
    }
}

impl Eq for EntryId {}

/// A storage capability that a backend may reject as unsupported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Durable file publication with retained recovery copies.
    DurableReplace,
    /// Legacy tag writing through local files.
    TagWrite,
    /// Resolving a provider-specific permission location.
    ProviderLocation,
}

/// A storage failure, preserving the backend I/O error when available.
#[derive(Debug)]
pub enum StorageError {
    /// The path no longer matches its previously checked canonical location.
    LocationChanged,
    /// The opened entry is not a regular file.
    NotRegular,
    /// An underlying I/O operation failed.
    Io(io::Error),
    /// Capacity is below the requested minimum.
    InsufficientSpace {
        /// Bytes required by the caller.
        required: u64,
        /// Bytes reported available by the backend.
        available: u64,
    },
    /// The backend cannot provide the requested capability.
    Unsupported {
        /// The capability that was refused.
        operation: Operation,
    },
    /// The supplied backend configuration is invalid.
    Configuration(&'static str),
}

/// A storage operation result; failures do not erase underlying I/O details.
pub type StorageResult<T> = Result<T, StorageError>;

impl From<io::Error> for StorageError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// A regular file opened without forcing an additional identity query.
/// Identity lookup is separate so opening does not gain a new failure mode.
#[derive(Debug)]
pub struct OpenedRegular {
    /// The opened regular file; its lifetime is owned by the caller.
    pub file: File,
}

/// Write guarantees advertised independently of path-specific access failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteCapabilities {
    /// Whether the backend supports the durable replacement protocol.
    pub durable_replace: bool,
    /// Whether existing local-file tag writes are supported.
    pub legacy_tag_write: bool,
}

/// Resolves local paths and identities for library file access.
/// Fallible operations preserve backend errors; boolean probes are conservative.
pub trait LibraryStorage: Send + Sync + 'static {
    /// Returns an existing path in canonical, non-verbatim form, or its resolution error.
    fn canonical(&self, path: &Path) -> io::Result<LocalPath>;
    /// Removes recognized Windows verbatim drive/UNC prefixes without I/O.
    /// Other paths, including non-Unicode paths, are returned unchanged; this cannot fail.
    fn strip_verbatim(&self, path: PathBuf) -> PathBuf;
    /// Returns a canonical path if reachable, otherwise the original spelling.
    /// Resolution failure is deliberately a fallback, not proof that two paths differ.
    fn comparable(&self, path: &Path) -> LocalPath;
    /// Probes case behavior on the containing file system; `None` means unknown.
    fn case_insensitive_fs(&self, path: &Path) -> Option<bool>;
    /// Probes names inside a directory before falling back to its file system.
    /// Returns `None` if case behavior cannot be established.
    fn case_insensitive_inside(&self, dir: &Path) -> Option<bool>;
    /// Compares directory locations using backend path and case rules.
    fn same_dir(&self, a: &Path, b: &Path) -> bool;
    /// Reports whether either directory contains the other under backend path rules.
    fn overlap(&self, a: &Path, b: &Path) -> bool;
    /// Rechecks canonical spelling; returns `LocationChanged` on mismatch or an I/O error.
    fn ensure_canonical(&self, path: &Path) -> StorageResult<()>;
    /// Opens a regular file, refusing non-regular entries. With `checked`, also rechecks
    /// canonical location; reports `LocationChanged`, `NotRegular`, or the I/O error.
    fn open_regular(&self, path: &Path, checked: bool) -> StorageResult<OpenedRegular>;
    /// Opens the source for tag planning. Backends with mandatory sharing controls
    /// deny writers until the handle is dropped.
    fn open_tag_source(&self, path: &Path, checked: bool) -> StorageResult<OpenedRegular> {
        self.open_regular(path, checked)
    }
    /// Obtains a live path identity, retaining backend resources; returns lookup errors.
    fn entry_id(&self, path: &Path) -> io::Result<EntryId>;
    /// Obtains a live identity using existing metadata where supported; errors propagate.
    fn entry_id_from_metadata(&self, path: &Path, meta: &Metadata) -> io::Result<EntryId>;
    /// Obtains a live identity from an open handle, or the backend query error.
    fn opened_id(&self, file: &File) -> io::Result<EntryId>;
    /// Obtains a handle-free identity from an entry and metadata; errors propagate.
    fn entry_identity(&self, path: &Path, meta: &Metadata) -> io::Result<FileIdentity>;
    /// Identifies the containing directory without following its last symlink.
    fn directory_identity(&self, path: &Path) -> io::Result<FileIdentity> {
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.is_dir() {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        self.entry_identity(path, &meta)
    }
    /// Obtains a handle-free identity from an open file; errors propagate.
    fn opened_identity(&self, file: &File) -> io::Result<FileIdentity>;
    /// Returns supported write protocols without checking a particular path.
    fn write_capabilities(&self) -> WriteCapabilities;
}

/// Reports capacity for database placement without choosing a directory.
pub trait DatabaseStorage: Send + Sync + 'static {
    /// Returns available bytes on the volume of an existing parent, or the query error.
    fn available_space(&self, existing_parent: &Path) -> io::Result<u64>;
}

/// The file, database, and replacement backends shared by library handles.
#[derive(Clone)]
pub struct StorageBackends {
    /// Path and identity operations.
    pub files: Arc<dyn LibraryStorage>,
    /// Database capacity queries.
    pub database: Arc<dyn DatabaseStorage>,
    /// Durable replacement transactions.
    pub replacements: Arc<dyn DurableReplace>,
}

/// Caller-selected recovery paths; backends must not invent alternate candidates.
#[derive(Debug, Clone)]
pub struct ReplacementPaths {
    /// The file being replaced.
    pub destination: LocalPath,
    /// The prepared replacement, also used as the sealing path when publishing.
    pub staged: Option<LocalPath>,
    /// The retained original for rollback.
    pub backup: Option<LocalPath>,
    /// A registered copy of the published result retained during rollback.
    pub saved_after: Option<LocalPath>,
}

/// How the original file is preserved before or during publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupPreparation {
    /// The backup was registered before publication.
    Registered {
        /// Whether preparation copied the bytes instead of linking the original.
        copied: bool,
    },
    /// The platform creates the backup during publication (Windows `ReplaceFileW`).
    DuringPublish,
}

/// Records a backup identity and whether it is a copy before publication proceeds.
/// Returning an error prevents preparation from reporting success.
pub type RegisterBackup<'a> = dyn FnMut(FileIdentity, bool) -> io::Result<()> + 'a;

/// Same-directory tag publication protocol, selected by an actual volume probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TagPublish {
    /// Atomically exchanges two paths.
    Exchange,
    /// Displaces and publishes using two no-clobber moves.
    Exclusive {
        /// Whether rename preserves identities on this directory's volume.
        stable_ids: bool,
    },
    /// Uses a hard link for no-clobber installation from a private name.
    Link,
    /// Uses the platform replacement operation with a backup pathname.
    Replace {
        /// Whether replacement preserves identities on this directory's volume.
        stable_ids: bool,
    },
}

/// Constructs replacement transactions over caller-registered paths.
pub trait DurableReplace: Send + Sync + 'static {
    /// Returns whether the backend provides the durable replacement protocol.
    fn supported(&self) -> bool;
    /// Can publish tags while retaining the actual displaced file for verification.
    fn provides_tag_publish(&self) -> bool {
        false
    }
    /// Checks the actual target and probes its directory before planning a tag write.
    fn supports_tag_publish(&self, _target: &Path) -> io::Result<TagPublish> {
        Err(io::ErrorKind::Unsupported.into())
    }
    /// Invalidates a directory probe after an unsupported live operation.
    fn invalidate_tag_publish(&self, _parent: &Path) {}
    /// Removes abandoned empty probe files from a known music directory.
    fn clean_tag_probes(&self, _parent: &Path) -> io::Result<()> {
        Ok(())
    }
    /// Retains the supplied paths for a new transaction, or reports unsupported/configuration
    /// errors. Construction does not publish or choose new candidate names.
    fn begin(&self, paths: ReplacementPaths) -> StorageResult<Box<dyn ReplacementTransaction>>;
    /// Reuses registered paths for recovery without constructing or renaming candidates.
    /// Reports unsupported/configuration errors as for `begin`.
    fn resume(&self, paths: ReplacementPaths) -> StorageResult<Box<dyn ReplacementTransaction>>;
}

/// Executes recoverable replacement steps over the registered paths.
/// I/O failures propagate; callers must recheck paths because a failing step may have
/// changed the namespace. Do not finalize database state until durability is confirmed.
pub trait ReplacementTransaction: Send {
    /// Opens the registered original for replacement, or returns the open error.
    fn open_source(&self) -> io::Result<File>;
    /// Applies the original's required replacement attributes to staged bytes; errors propagate.
    fn prepare_attributes(&self, staged: &Path, original: &Path) -> io::Result<()>;
    /// Reads the platform hidden attribute, or returns the query error.
    fn hidden(&self, path: &Path) -> io::Result<bool>;
    /// Sets the hidden attribute where supported, returning any update error.
    fn set_hidden(&self, path: &Path, value: bool) -> io::Result<()>;
    /// Marks an internal candidate hidden where supported, or returns the update error.
    fn hide_candidate(&self, path: &Path) -> io::Result<()>;
    /// Preserves the original and registers its identity before returning `Registered`.
    /// Windows defers backup creation to publication. Registration/I/O failures propagate.
    fn prepare_backup(&self, register: &mut RegisterBackup<'_>) -> io::Result<BackupPreparation>;
    /// Publishes staged bytes using the matching backup preparation.
    /// Errors may occur after a namespace change; registered copies must remain recoverable.
    fn publish(&self, preparation: BackupPreparation) -> io::Result<()>;
    /// Publishes staged tags. Exclusive publication requires prior displacement.
    fn publish_tag(&self, _mode: TagPublish) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "guarded tag publication unavailable",
        ))
    }
    /// Moves a public name to a reserved private sibling before no-clobber installation.
    fn displace_tag(&self, source: &Path, backup: &Path, _mode: TagPublish) -> io::Result<()> {
        self.move_missing(source, backup)
    }
    /// Restores a private sibling without overwriting a competing public name.
    fn restore_tag(&self, source: &Path, target: &Path, _mode: TagPublish) -> io::Result<()> {
        self.publish_missing(source, target)
    }
    /// Restores the displaced name and retains the rejected result for inspection.
    fn rollback_tag(&self, _mode: TagPublish) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "guarded tag rollback unavailable",
        ))
    }
    /// Flushes file contents and the supported namespace durability barriers; errors propagate.
    fn sync(&self, path: &Path) -> io::Result<()>;
    /// Completes publication durability before the database can finalize the replacement.
    /// Windows uses two write-through moves and attempts rollback if either fails; other
    /// platforms sync the destination. Errors retain recovery candidates for inspection.
    fn seal(&self) -> io::Result<()>;
    /// Checks the published file against required backup attributes; mismatch is an error.
    fn verify_attributes(&self) -> io::Result<()>;
    /// Moves a name exclusively, refusing to overwrite an existing destination.
    /// Unsupported exclusive moves and I/O failures are errors; no racy overwrite fallback.
    fn move_missing(&self, source: &Path, destination: &Path) -> io::Result<()>;
    /// Moves a name only into an absent destination, then flushes publication.
    /// Never overwrites an external file. A sync error may follow a successful move,
    /// so callers must inspect both names before retrying or cleaning recovery data.
    fn publish_missing(&self, source: &Path, destination: &Path) -> io::Result<()>;
    /// Restores the backup while retaining the registered after-copy needed for recovery.
    /// A failure may leave a partial namespace transition; no success or cleanup is implied.
    fn rollback(&self) -> io::Result<()>;
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LocationChanged => f.write_str("location changed"),
            Self::NotRegular => f.write_str("not a regular file"),
            Self::Io(e) => std::fmt::Display::fmt(e, f),
            Self::InsufficientSpace {
                required,
                available,
            } => write!(
                f,
                "insufficient space: required {required}, available {available}"
            ),
            Self::Unsupported { operation } => {
                write!(f, "unsupported storage operation: {operation:?}")
            }
            Self::Configuration(message) => write!(f, "storage configuration: {message}"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
