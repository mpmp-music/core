//! Short-lived publication locks and a lifetime lock INSIDE the selected data root.
use super::*;
use sha2::{Digest, Sha256};

pub struct DataLease {
    file: fs::File,
}

impl Drop for DataLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn lock(path: &Path) -> io::Result<DataLease> {
    open_lock(path, false)
}

fn open_lock(path: &Path, temporary: bool) -> io::Result<DataLease> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{
            Foundation::{GENERIC_READ, GENERIC_WRITE},
            Storage::FileSystem::{
                DELETE, FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAG_OPEN_REPARSE_POINT,
            },
        };
        let flags = if temporary {
            // Exclusive opens plus the coordinator prevent pending-delete races.
            options
                .share_mode(0)
                .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE);
            FILE_FLAG_DELETE_ON_CLOSE
        } else {
            0
        };
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | flags);
    }
    #[cfg(not(windows))]
    let _ = temporary; // Unlinking a Unix lock file could create competing inodes.
    let file = options.open(path)?;
    if !file.metadata()?.is_file() || super::linked(&file.metadata()?) {
        return Err(io::Error::other("mpmp: lock is not a regular file"));
    }
    file.try_lock().map_err(|e| {
        io::Error::other(format!(
            concat!(
                "mpmp: another instance is using or publishing this data directory; close ",
                "it before starting: {e}",
            ),
            e = e,
        ))
    })?;
    Ok(DataLease { file })
}

pub(super) fn runtime_lease(root: &Path) -> io::Result<DataLease> {
    let lease = lock(&root.join(".mpmp.lock"))?;
    lease.file.sync_all()?;
    Ok(lease)
}

pub(super) struct PublicationLease {
    // Fields drop in declaration order: delete the sibling before releasing coordination.
    _sibling: Option<DataLease>,
    _coordinator: DataLease,
}

/// Canonicalize the existing prefix; nonexistent trailing components retain their
/// names. Existing symlink aliases therefore cannot select different coordinator locks.
fn identity(root: &Path) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(root)?;
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    let canonical = loop {
        match fs::canonicalize(ancestor) {
            Ok(path) => break path,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                suffix.push(
                    ancestor
                        .file_name()
                        .ok_or_else(|| io::Error::other("mpmp: invalid data path"))?,
                );
                ancestor = ancestor.parent().ok_or(e)?;
            }
            Err(e) => return Err(e),
        }
    };
    let mut canonical = canonical;
    for name in suffix.into_iter().rev() {
        canonical.push(name);
    }
    Ok(canonical)
}

fn coordinator_path(root: &Path) -> io::Result<PathBuf> {
    let directory = std::env::temp_dir().join(format!("mpmp-publication-locks-{}", user_key()));
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(&directory)
    };
    #[cfg(not(unix))]
    let created = fs::create_dir(&directory);
    match created {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    super::regular_directory(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(&directory)?;
        // SAFETY: geteuid has no preconditions.
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(io::Error::other(
                "mpmp: publication coordinator directory is not private",
            ));
        }
    }
    #[cfg(windows)]
    let key = root
        .as_os_str()
        .to_string_lossy()
        .to_lowercase()
        .into_bytes();
    #[cfg(not(windows))]
    let key = root.as_os_str().as_encoded_bytes();
    Ok(directory.join(format!("{:x}.lock", Sha256::digest(key))))
}

#[cfg(unix)]
fn user_key() -> String {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }.to_string()
}

#[cfg(not(unix))]
fn user_key() -> String {
    // Windows' GetTempPath2 selects the user's private temporary directory.
    "user".into()
}

pub(super) fn publication_lease(root: &Path) -> io::Result<PublicationLease> {
    let root = identity(root)?;
    // Always coordinate first: a change in parent permissions cannot split callers
    // between sibling and fallback locks. All consumers also take the internal lease.
    let coordinator = lock(&coordinator_path(&root)?)?;
    let name = root
        .file_name()
        .ok_or_else(|| io::Error::other("mpmp: data root has no name"))?;
    let parent = root
        .parent()
        .ok_or_else(|| io::Error::other("mpmp: data root has no parent"))?;
    fs::create_dir_all(parent)?;
    let sibling = match open_lock(
        &parent.join(format!(".{}.publish.lock", name.to_string_lossy())),
        true,
    ) {
        Ok(lease) => Some(lease),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
            ) =>
        {
            tracing::info!(
                "mpmp: parent is not writable; canonical-path publication coordinator used"
            );
            None
        }
        Err(e) => return Err(e),
    };
    Ok(PublicationLease {
        _coordinator: coordinator,
        _sibling: sibling,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_lock_is_inside_root_and_publication_lock_is_released_after_prepare() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DataPaths {
            root: dir.path().join("mpmp"),
            legacy: None,
        };
        let runtime = prepare(&paths).unwrap();
        assert!(paths.root.join(".mpmp.lock").is_file());
        drop(publication_lease(&paths.root).unwrap());
        assert!(prepare(&paths).is_err());
        drop(runtime);
        prepare(&paths).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn writable_data_root_with_unwritable_parent_uses_fallback_and_internal_lock() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("mpmp");
        fs::create_dir(&root).unwrap();
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        let _restore = Restore(dir.path().to_path_buf());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let publication = publication_lease(&root).unwrap();
        // When run as root, filesystem permissions cannot exercise the denied branch.
        if unsafe { libc::geteuid() } != 0 {
            assert!(publication._sibling.is_none());
        }
        drop(publication);
        let paths = DataPaths {
            root: root.clone(),
            legacy: None,
        };
        let lease = prepare(&paths).unwrap();
        assert!(root.join(".mpmp.lock").exists());
        fs::write(root.join("state.json"), b"writable").unwrap();
        assert!(prepare(&paths).is_err());
        drop(lease);
        assert!(prepare(&paths).is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn canonical_aliases_share_publication_and_runtime_lock_identity() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = dir.path().join("alias");
        symlink(&real, &alias).unwrap();
        assert_eq!(
            identity(&real.join("mpmp")).unwrap(),
            identity(&alias.join("mpmp")).unwrap()
        );
        let publication = publication_lease(&real.join("mpmp")).unwrap();
        assert!(publication_lease(&alias.join("mpmp")).is_err());
        drop(publication);
        let lease = prepare(&DataPaths {
            root: real.join("mpmp"),
            legacy: None,
        })
        .unwrap();
        assert!(
            prepare(&DataPaths {
                root: alias.join("mpmp"),
                legacy: None
            })
            .is_err()
        );
        drop(lease);
        prepare(&DataPaths {
            root: alias.join("mpmp"),
            legacy: None,
        })
        .unwrap();
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn sibling_lock_is_deleted_only_after_its_last_exclusive_handle_closes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("mpmp");
        let sibling = dir.path().join(".mpmp.publish.lock");
        let publication = publication_lease(&root).unwrap();
        assert!(publication._sibling.is_some());
        assert!(publication_lease(&root).is_err());
        assert!(open_lock(&sibling, true).is_err());
        drop(publication);
        assert!(!sibling.exists());
        for _ in 0..2 {
            drop(
                prepare(&DataPaths {
                    root: root.clone(),
                    legacy: None,
                })
                .unwrap(),
            );
            assert!(
                !sibling.exists(),
                "successful startup must not litter AppData"
            );
        }
    }
}
