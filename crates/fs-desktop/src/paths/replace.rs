use super::*;

/// Copy normal attributes before publication; temporary-file caching must not survive it.
pub fn prepare_replacement(staged: &Path, original: &Path) -> io::Result<()> {
    std::fs::set_permissions(staged, std::fs::metadata(original)?.permissions())?;
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TEMPORARY, GetFileAttributesW,
            INVALID_FILE_ATTRIBUTES, SetFileAttributesW,
        };
        let old = wide(original);
        let new = wide(staged);
        // SAFETY: both buffers are live NUL-terminated paths.
        let attributes = unsafe { GetFileAttributesW(old.as_ptr()) };
        if attributes == INVALID_FILE_ATTRIBUTES {
            return Err(io::Error::last_os_error());
        }
        let attributes = attributes & !FILE_ATTRIBUTE_TEMPORARY;
        let attributes = if attributes == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            attributes
        };
        // SAFETY: new is a live NUL-terminated path.
        if unsafe { SetFileAttributesW(new.as_ptr(), attributes) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Private candidates are hidden on Windows without changing normal-file attributes.
pub fn hidden(path: &Path) -> io::Result<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Ok(std::fs::metadata(path)?.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN
            != 0)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(false)
    }
}

pub fn set_hidden(path: &Path, hidden: bool) -> io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::*;
        let path = wide(path);
        // SAFETY: the buffer is a live NUL-terminated path.
        let attributes = unsafe { GetFileAttributesW(path.as_ptr()) };
        if attributes == INVALID_FILE_ATTRIBUTES {
            return Err(io::Error::last_os_error());
        }
        if (attributes & FILE_ATTRIBUTE_HIDDEN != 0) == hidden {
            return Ok(());
        }
        let attributes = if hidden {
            (attributes & !FILE_ATTRIBUTE_NORMAL) | FILE_ATTRIBUTE_HIDDEN
        } else {
            attributes & !FILE_ATTRIBUTE_HIDDEN
        };
        let attributes = if attributes == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            attributes
        };
        if unsafe { SetFileAttributesW(path.as_ptr(), attributes) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(windows))]
    let _ = (path, hidden);
    Ok(())
}

pub fn hide_candidate(path: &Path) -> io::Result<()> {
    set_hidden(path, true)
}

/// Verify copied Windows file attributes after replacement, as well as TEMPORARY.
/// NORMAL is an encoding of no other attributes and is ignored in the comparison.
pub fn verify_replacement_attributes(destination: &Path, backup: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TEMPORARY,
        };
        let actual = std::fs::metadata(destination)?.file_attributes();
        let expected = std::fs::metadata(backup)?.file_attributes() & !FILE_ATTRIBUTE_TEMPORARY;
        if actual & FILE_ATTRIBUTE_TEMPORARY != 0
            || actual & !(FILE_ATTRIBUTE_NORMAL | FILE_ATTRIBUTE_HIDDEN)
                != expected & !(FILE_ATTRIBUTE_NORMAL | FILE_ATTRIBUTE_HIDDEN)
        {
            return Err(io::Error::other("replacement file attributes differ"));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (destination, backup);
    }
    Ok(())
}

/// Create a complete backup while the original name remains available. The caller
/// persists its resulting identity and verifies its digest before publishing anything.
/// Returns true for a full copy, false for a hard link.
pub fn prepare_backup(destination: &Path, backup: &Path) -> io::Result<bool> {
    prepare_backup_registered(destination, backup, |_, _| Ok(()))
}

/// Register the newly created identity before copying bytes or synchronizing it.
pub fn prepare_backup_registered(
    destination: &Path,
    backup: &Path,
    registered: impl FnMut(FileIdentity, bool) -> io::Result<()>,
) -> io::Result<bool> {
    prepare_backup_with(
        destination,
        backup,
        |a, b| std::fs::hard_link(a, b),
        sync_replacement,
        registered,
    )
}

/// Injectable filesystem primitives; registration must be durable before it returns.
pub fn prepare_backup_with(
    destination: &Path,
    backup: &Path,
    link: impl FnOnce(&Path, &Path) -> io::Result<()>,
    sync: impl FnOnce(&Path) -> io::Result<()>,
    mut registered: impl FnMut(FileIdentity, bool) -> io::Result<()>,
) -> io::Result<bool> {
    let copied = match link(destination, backup) {
        Ok(()) => {
            registered(FileIdentity::of_file(&File::open(backup)?)?, false)?;
            false
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::Unsupported
                    | io::ErrorKind::CrossesDevices
            ) || matches!(e.raw_os_error(), Some(libc_code::NOT_SUPPORTED)) =>
        {
            let mut source = File::open(destination)?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut saved = options.open(backup)?;
            registered(FileIdentity::of_file(&saved)?, true)?;
            std::io::copy(&mut source, &mut saved)?;
            std::fs::set_permissions(backup, source.metadata()?.permissions())?;
            true
        }
        Err(e) => return Err(e),
    };
    sync(backup)?;
    Ok(copied)
}

#[cfg(unix)]
mod libc_code {
    pub const NOT_SUPPORTED: i32 = libc::EOPNOTSUPP;
}

#[cfg(not(unix))]
mod libc_code {
    pub const NOT_SUPPORTED: i32 = 50;
}

#[cfg(unix)]
pub(super) fn unsupported_exchange_code(code: i32, codes: [i32; 4]) -> bool {
    codes.contains(&code)
}

#[cfg(unix)]
pub(super) fn exchange_unsupported(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported
        || error.raw_os_error().is_some_and(|code| {
            unsupported_exchange_code(
                code,
                [libc::ENOSYS, libc::EINVAL, libc::ENOTSUP, libc::EOPNOTSUPP],
            )
        })
}

/// Publish atomically, retaining the old complete file. Unix first prepares a hard-link
/// or full-copy backup; exchange additionally captures concurrent path replacements.
/// On filesystems without exchange, rename is atomic but the copy-backup race boundary
/// documented in the contract applies. No candidate is deleted on an uncertain error.
pub fn replace_with_backup(staged: &Path, destination: &Path, backup: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        if std::fs::symlink_metadata(backup).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "backup already exists",
            ));
        }
        let (old, new, saved) = (wide(destination), wide(staged), wide(backup));
        // SAFETY: all paths are live NUL-terminated buffers; ACL errors remain fatal.
        if unsafe {
            windows_sys::Win32::Storage::FileSystem::ReplaceFileW(
                old.as_ptr(),
                new.as_ptr(),
                saved.as_ptr(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        let copied = if !backup.exists() {
            prepare_backup(destination, backup)?
        } else {
            false
        };
        replace_prepared_with_backup(staged, destination, backup, copied)
    }
}

/// The caller durably binds whether the backup is a copy before any replacement.
#[cfg(unix)]
pub fn replace_prepared_with_backup(
    staged: &Path,
    destination: &Path,
    backup: &Path,
    copied: bool,
) -> io::Result<()> {
    publish_prepared_with(
        staged,
        destination,
        backup,
        copied,
        exchange,
        |a, b| std::fs::rename(a, b),
        sync_replacement,
    )
}

#[cfg(unix)]
pub(super) fn publish_prepared_with(
    staged: &Path,
    destination: &Path,
    backup: &Path,
    copied: bool,
    swap: impl FnOnce(&Path, &Path) -> io::Result<()>,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
    sync: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    match swap(staged, destination) {
        Ok(()) => {
            sync(destination)?;
            // Only discard a displaced name when the backup preserves that inode.
            // A concurrently replaced inode remains registered at staged instead.
            if FileIdentity::of_file(&File::open(staged)?)?
                != FileIdentity::of_file(&File::open(backup)?)?
                && !same_contents(staged, backup)?
            {
                return Err(io::Error::other(
                    "displaced file differs from backup; preserve candidates",
                ));
            }
            std::fs::remove_file(staged)?;
            sync_parent(destination)
        }
        Err(e) if exchange_unsupported(&e) => {
            // Only the explicitly documented copy-backup path permits this race
            // boundary. A hard-link filesystem without atomic exchange is refused.
            if !copied {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "atomic exchange unavailable for linked backup",
                ));
            }
            rename(staged, destination)?;
            sync(destination)
        }
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
pub(crate) fn exchange(source: &Path, destination: &Path) -> io::Result<()> {
    rename_unix(source, destination, true)
}

#[cfg(unix)]
fn rename_unix(source: &Path, destination: &Path, swap: bool) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source.as_os_str().as_bytes())?;
    let destination = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
    rename_at(libc::AT_FDCWD, &source, &destination, swap)
}

#[cfg(unix)]
pub(super) fn rename_at(
    dir: libc::c_int,
    source: &std::ffi::CStr,
    destination: &std::ffi::CStr,
    swap: bool,
) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: the caller keeps the directory open and both paths are terminated.
    let result = unsafe {
        let flags = if swap {
            libc::RENAME_EXCHANGE
        } else {
            libc::RENAME_NOREPLACE
        };
        libc::renameat2(dir, source.as_ptr(), dir, destination.as_ptr(), flags)
    };
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    // SAFETY: the caller keeps the directory open and both paths are terminated.
    let result = unsafe {
        let flags = if swap {
            libc::RENAME_SWAP
        } else {
            libc::RENAME_EXCL
        };
        libc::renameatx_np(dir, source.as_ptr(), dir, destination.as_ptr(), flags)
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn same_contents(a: &Path, b: &Path) -> io::Result<bool> {
    use std::io::Read;
    let (mut a, mut b) = (File::open(a)?, File::open(b)?);
    if a.metadata()?.len() != b.metadata()?.len() {
        return Ok(false);
    }
    let (mut x, mut y) = ([0; 65536], [0; 65536]);
    loop {
        let n = a.read(&mut x)?;
        if n == 0 {
            return Ok(true);
        }
        b.read_exact(&mut y[..n])?;
        if x[..n] != y[..n] {
            return Ok(false);
        }
    }
}

/// Restore before without a missing-name window, keeping the failed result at staged
/// or the independently registered, durable saved_after path.
/// An unexpected current file or occupied staging name is never overwritten.
pub fn rollback_replacement(
    destination: &Path,
    backup: &Path,
    staged: &Path,
    saved_after: &Path,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        rollback_with(
            destination,
            backup,
            staged,
            saved_after,
            exchange,
            |a, b| std::fs::rename(a, b),
            sync_replacement,
        )
    }
    #[cfg(windows)]
    {
        let _ = saved_after;
        if !destination.exists() {
            return publish_missing(backup, destination);
        }
        if staged.exists() {
            return Err(io::Error::other("rollback staging name occupied"));
        }
        // Restore before in one ReplaceFileW call, retaining the rejected after file
        // at the already registered staged name. Partial errors preserve all names.
        replace_with_backup(backup, destination, staged)?;
        sync_replacement(staged)?;
        seal_replacement(destination, backup)
    }
}

#[cfg(unix)]
pub fn rollback_with(
    destination: &Path,
    backup: &Path,
    staged: &Path,
    saved_after: &Path,
    swap: impl FnOnce(&Path, &Path) -> io::Result<()>,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
    sync: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    if staged.exists() {
        if !same_contents(staged, backup)? {
            return Err(io::Error::other("rollback staging name occupied"));
        }
    } else {
        let mut source = File::open(backup)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        let mut staged_file = options.open(staged)?;
        std::io::copy(&mut source, &mut staged_file)?;
        std::fs::set_permissions(staged, source.metadata()?.permissions())?;
        staged_file.sync_all()?;
    }
    match swap(staged, destination) {
        Ok(()) => {}
        Err(e) if exchange_unsupported(&e) => {
            // The caller registered this independent complete copy before rollback.
            // Never discard the only after file if namespace persistence then fails.
            if !same_contents(saved_after, destination)? {
                return Err(io::Error::other("durable after copy unavailable"));
            }
            sync(saved_after)?;
            rename(staged, destination)?;
        }
        Err(e) => return Err(e),
    }
    sync(destination)
}

/// Move a name without replacing an existing destination. Unknown Unix platforms
/// refuse the write rather than emulate a racy check followed by an overwriting rename.
pub fn move_missing(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        publish_missing(source, destination)
    }
    #[cfg(unix)]
    {
        rename_unix(source, destination, false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (source, destination);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "exclusive rename unavailable",
        ))
    }
}

/// Publish only if the destination is absent. Used for both normal Unix publication
/// and recovery; it must never overwrite an external file created during recovery.
pub fn publish_missing(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
        let (source, destination) = (wide(source), wide(destination));
        // SAFETY: live NUL-terminated paths, deliberately no REPLACE_EXISTING flag.
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(windows))]
    {
        move_missing(source, destination)?;
    }
    sync_replacement(destination)
}

/// Flush file contents and the namespace before the database may finalize a replacement.
pub fn sync_replacement(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TEMPORARY;
        if std::fs::metadata(path)?.file_attributes() & FILE_ATTRIBUTE_TEMPORARY != 0 {
            return Err(io::Error::other(
                "replacement still has TEMPORARY attribute",
            ));
        }
        // sync_all maps to FlushFileBuffers. A writable handle is required on Windows.
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?
            .sync_all()?;
        // Persist the directory entry using the documented write-through rename path.
        // Renaming to the same name does not promise persistence, so the actual move is
        // performed through MoveFileExW by seal_replacement below.
    }
    #[cfg(not(windows))]
    File::open(path)?.sync_all()?;
    sync_parent(path)
}

pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(
            path.parent()
                .ok_or_else(|| io::Error::other("missing parent"))?,
        )?
        .sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// ReplaceFileW does not support write-through. A same-name MoveFileExW is not a
/// documented durability barrier, so retain two write-through moves with immediate
/// rollback of the first move if the second fails. No candidates are cleaned on error.
pub fn seal_replacement(destination: &Path, seal: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        seal_with(destination, seal, publish_missing)
    }
    #[cfg(not(windows))]
    {
        let _ = seal;
        sync_replacement(destination)
    }
}

#[cfg(any(windows, test))]
pub(super) fn seal_with(
    destination: &Path,
    seal: &Path,
    mut publish: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    if let Err(e) = publish(destination, seal) {
        if !destination.exists() && seal.exists() {
            let _ = publish(seal, destination);
        }
        return Err(e);
    }
    if let Err(e) = publish(seal, destination) {
        if !destination.exists() && seal.exists() {
            let _ = publish(seal, destination);
        }
        return Err(e);
    }
    Ok(())
}
