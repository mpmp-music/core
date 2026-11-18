//! The desktop's file system rules, in one place: what differs between Unix and Windows.
//!
//! - [`paths`]: canonical folders (without Windows' `\\?\` prefix), whether letter case
//!   counts where a file is, what tells one file from another ([`paths::FileId`]), opening
//!   a regular file without waiting on anything else. The library uses these for its
//!   folders and files.
//! - [`DesktopLocations`]: location strings (contract guarantee 16) as absolute file
//!   system paths, for core's permission scopes and the locations agents name; with a UNC
//!   path back to this computer read as the local path it names.
//!
//! Other platforms (Android, iOS) get rules of their own in crates of their own; the
//! library and core take what they need through their interfaces.

pub mod data;
pub mod paths;

mod storage;
pub use storage::{DesktopReplacementTransaction, DesktopStorage};

mod locations;
mod unc;

pub use locations::DesktopLocations;

/// Free bytes available to this user on the filesystem containing `path`.
pub fn available_space(path: &std::path::Path) -> std::io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: path is NUL terminated and info is writable for statvfs.
        if unsafe { libc::statvfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: successful statvfs initialized the structure.
        let info = unsafe { info.assume_init() };
        Ok(
            u64::try_from(u128::from(info.f_bavail).saturating_mul(u128::from(info.f_frsize)))
                .unwrap_or(u64::MAX),
        )
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut free = 0u64;
        // SAFETY: path is terminated, free is writable, other outputs are optional.
        if unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                path.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(free)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "free-space checks unavailable on this platform",
        ))
    }
}
