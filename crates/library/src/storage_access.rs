//! Per-library local file storage, translated into library errors.
//! Desktop conveniences below are only for existing desktop tests.

use std::fs::File;
use std::path::Path;

#[cfg(all(test, feature = "desktop-fs"))]
pub(crate) use mpmp_fs_desktop::paths::{canonical_dir, comparable};

pub(crate) fn canonical(
    storage: &dyn mpmp_storage::LibraryStorage,
    path: &Path,
) -> std::io::Result<std::path::PathBuf> {
    storage.canonical(path).map(|p| p.0)
}

pub(crate) fn comparable_with(
    storage: &dyn mpmp_storage::LibraryStorage,
    path: &Path,
) -> std::path::PathBuf {
    storage.comparable(path).0
}

fn error(path: &Path, why: mpmp_storage::StorageError) -> crate::LibraryError {
    match why {
        mpmp_storage::StorageError::LocationChanged => location_changed(path),
        mpmp_storage::StorageError::NotRegular => not_a_regular_file(path),
        mpmp_storage::StorageError::Io(err) => err.into(),
        other => crate::LibraryError::Storage(other),
    }
}

/// For a location checked by the caller ([`crate::ActionMeta::checked`]): `path` (absolute)
/// must still be in canonical form, so that no symlink or other spelling leads elsewhere;
/// else [`crate::Invalid::LocationChanged`]. When nothing is there at all, the error is the
/// same `NotFound` as for anyone else: nothing changed, it never existed.
pub(crate) fn ensure_canonical(
    storage: &dyn mpmp_storage::LibraryStorage,
    path: &Path,
) -> crate::Result<()> {
    storage
        .ensure_canonical(path)
        .map_err(|why| error(path, why))
}

pub(crate) fn location_changed(path: &Path) -> crate::LibraryError {
    crate::LibraryError::invalid(
        crate::Invalid::LocationChanged,
        format!(
            "{} changed since it was checked: it no longer leads where it did",
            path.display()
        ),
    )
}

pub(crate) fn not_a_regular_file(path: &Path) -> crate::LibraryError {
    crate::LibraryError::invalid(
        crate::Invalid::NotARegularFile,
        format!("{} is not a regular file", path.display()),
    )
}

/// Opens an existing regular file to read it, without waiting on anything else
/// ([`mpmp_fs_desktop::paths::open_regular`]): anything but a regular file is
/// `not_a_regular_file`; with `checked` ([`crate::ActionMeta::checked`]), a symlink there
/// is `location_changed`.
pub(crate) fn open_regular(
    storage: &dyn mpmp_storage::LibraryStorage,
    path: &Path,
    checked: bool,
) -> crate::Result<File> {
    storage
        .open_regular(path, checked)
        .map(|opened| opened.file)
        .map_err(|why| error(path, why))
}

/// Opens a tag source or private copy with the backend's write-sharing guard.
pub(crate) fn open_tag_source(
    storage: &dyn mpmp_storage::LibraryStorage,
    path: &Path,
    checked: bool,
) -> crate::Result<File> {
    storage
        .open_tag_source(path, checked)
        .map(|opened| opened.file)
        .map_err(|why| error(path, why))
}
