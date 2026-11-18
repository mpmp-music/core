use super::*;

/// The canonical form of an existing directory: absolute, `.`/`..` and symlinks resolved,
/// and on Windows the real letter case, without the `\\?\` prefix `canonicalize` adds.
pub fn canonical_dir(path: &Path) -> io::Result<PathBuf> {
    Ok(strip_verbatim(std::fs::canonicalize(path)?))
}

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\server\share` → `\\server\share`; other paths unchanged.
pub fn strip_verbatim(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\")
        && rest.as_bytes().get(1) == Some(&b':')
    {
        return PathBuf::from(rest);
    }
    path
}

/// A stored root path in comparable form: canonical if it is reachable, else as stored.
pub fn comparable(path: &Path) -> PathBuf {
    canonical_dir(path).unwrap_or_else(|_| path.to_owned())
}

fn lower(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

/// Whether letter case is ignored for names in the file system that holds the existing file
/// or folder `path` (inside it, for a folder that is a mount point; macOS and Windows
/// defaults): `None` when that cannot be told. Probed by looking up a name with its ASCII
/// case flipped and comparing what is found with the original (the same file: case is
/// ignored). The name probed is the first one with ASCII letters among `path` and the
/// folders above it that are held by the same file system (numeric or non-Latin names have none: their
/// parent is probed instead); failing that, for a folder, a name inside it. A folder that
/// is a mount point is probed inside first: its own name belongs to the file system above.
pub fn case_insensitive_fs(path: &Path) -> Option<bool> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let mount = meta.is_dir() && !holder_is(path, &meta);
    if mount && let Some(ignored) = probe_inside(path) {
        return Some(ignored);
    }
    path.ancestors()
        .take_while(|p| holder_is(p, &meta))
        .find_map(|p| probe_name(p).flatten())
        .or_else(|| {
            if meta.is_dir() && !mount {
                probe_inside(path)
            } else {
                None
            }
        })
}

/// Whether letter case is ignored for the names inside the existing folder `dir` (a
/// library root, for M3U matching): probed by a name inside it first (Windows can make a
/// single folder case-sensitive, and its own name is then still looked up without regard
/// to case in the folder above), else as [`case_insensitive_fs`].
pub fn case_insensitive_inside(dir: &Path) -> Option<bool> {
    let meta = std::fs::symlink_metadata(dir).ok()?;
    if meta.is_dir()
        && let Some(ignored) = probe_inside(dir)
    {
        return Some(ignored);
    }
    case_insensitive_fs(dir)
}

/// Whether the folder that holds `path` (its parent) is on the file system of `meta`.
/// Elsewhere than on Unix, every folder of a canonical path is taken to be (a volume
/// mounted in a folder is not told apart).
fn holder_is(path: &Path, meta: &std::fs::Metadata) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(parent).is_ok_and(|p| p.dev() == meta.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = (parent, meta);
        true
    }
}

/// Probes the last name of `path` (an existing file or folder): `None` when it has no
/// ASCII letter to flip (or none at all); `Some(None)` when the probe cannot tell.
/// Nothing is opened on Unix (a FIFO would block) and no symlink is followed: only the
/// entries themselves are compared. A regular file with hard links is not probed.
pub(super) fn probe_name(path: &Path) -> Option<Option<bool>> {
    let name = path.file_name()?.to_str()?;
    let flipped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    if flipped == name {
        return None;
    }
    // Only folders and regular files: a symlink would stand for where it leads (two links
    // to one folder are not one name), a FIFO or device could block when compared.
    let plain = |meta: &Metadata| meta.file_type().is_dir() || meta.file_type().is_file();
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !plain(&meta) || has_other_names(&meta) {
        return None;
    }
    let other = path.with_file_name(flipped);
    Some(match std::fs::symlink_metadata(&other) {
        // Where case is ignored the flipped name finds this very entry, a folder or a
        // regular file: anything else there (a symlink, a FIFO …) is another entry, so
        // case counts here.
        Ok(found) if !plain(&found) => Some(false),
        Ok(found) => match (
            FileId::of_entry(path, &meta),
            FileId::of_entry(&other, &found),
        ) {
            (Ok(a), Ok(b)) => Some(a == b),
            _ => None,
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    })
}

/// Whether a regular file has other names (hard links): one of them could be the
/// flipped name, the same file without case being ignored, so such a name is not probed
/// (the next one is). Folders cannot have them. Told on Unix only (std does not give the
/// link count elsewhere; the platform may not expose a stable link count).
fn has_other_names(meta: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.is_file() && meta.nlink() != 1
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

/// Probes a name inside the folder `dir` (the first few entries are tried).
pub(super) fn probe_inside(dir: &Path) -> Option<bool> {
    std::fs::read_dir(dir)
        .ok()?
        .take(64)
        .filter_map(|entry| entry.ok())
        .find_map(|entry| probe_name(&entry.path()).flatten())
}

/// Why [`ensure_canonical`] refused a path.
#[derive(Debug)]
pub enum Uncanonical {
    /// Nothing is there at all (as for anyone: nothing changed, it never existed).
    Missing(io::Error),
    /// It no longer leads where it did: a symlink or another spelling, or it cannot be
    /// resolved for another reason.
    Changed,
}

/// For a location the caller checked before: `path` (absolute) must still be in
/// canonical form, so that no symlink or other spelling leads elsewhere; else
/// [`Uncanonical::Changed`] (also when it cannot be resolved for another reason than that
/// nothing is there). When nothing is there at all, [`Uncanonical::Missing`].
pub fn ensure_canonical(path: &Path) -> Result<(), Uncanonical> {
    match canonical_dir(path) {
        Ok(canonical) if canonical == path => Ok(()),
        Err(err)
            if err.kind() == io::ErrorKind::NotFound
                && std::fs::symlink_metadata(path)
                    .is_err_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            Err(Uncanonical::Missing(err))
        }
        _ => Err(Uncanonical::Changed),
    }
}

/// Whether two canonical paths name the same directory.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || (lower(a) == lower(b) && case_insensitive_fs(a) == Some(true))
}

/// Whether one canonical path is inside the other (or both are the same).
pub fn overlap(a: &Path, b: &Path) -> bool {
    if a.starts_with(b) || b.starts_with(a) {
        return true;
    }
    let (la, lb) = (lower(a), lower(b));
    (la.starts_with(&lb) || lb.starts_with(&la)) && case_insensitive_fs(a) == Some(true)
}
