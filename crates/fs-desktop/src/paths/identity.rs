use super::*;

/// What tells one file or folder from another. On Unix its device and inode, read from
/// metadata: nothing is opened, and a path's last symlink is not followed (the link is
/// the entry). Elsewhere a `same_file::Handle` (Windows: volume serial and file index; the
/// file or folder is opened, following symlinks, and held open while this lives: drop it
/// before renaming or removing what it names).
#[derive(Debug, PartialEq, Eq)]
pub struct FileId(#[cfg(unix)] (u64, u64), #[cfg(not(unix))] same_file::Handle);

impl FileId {
    /// The entry at `path`.
    pub fn of_path(path: &Path) -> io::Result<FileId> {
        Self::of_entry(path, &std::fs::symlink_metadata(path)?)
    }

    /// The entry at `path`, whose `symlink_metadata` is `meta` (on Unix read from `meta`
    /// alone; elsewhere `path` is opened).
    pub fn of_entry(path: &Path, meta: &Metadata) -> io::Result<FileId> {
        #[cfg(unix)]
        {
            let _ = path;
            Ok(Self::of_meta(meta))
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            Ok(FileId(same_file::Handle::from_path(path)?))
        }
    }

    /// The open `file` (on Unix by `fstat`).
    pub fn of_file(file: &File) -> io::Result<FileId> {
        #[cfg(unix)]
        {
            Ok(Self::of_meta(&file.metadata()?))
        }
        #[cfg(not(unix))]
        {
            Ok(FileId(same_file::Handle::from_file(file.try_clone()?)?))
        }
    }

    #[cfg(unix)]
    fn of_meta(meta: &Metadata) -> FileId {
        use std::os::unix::fs::MetadataExt;
        FileId((meta.dev(), meta.ino()))
    }
}

/// A file identity snapshot that never retains a handle across a confirmation dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity(u64, u64);

impl FileIdentity {
    pub(crate) fn directory(path: &Path) -> io::Result<File> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
                FILE_SHARE_WRITE,
            };
            options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
            options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = options.open(path)?;
        let meta = file.metadata()?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
            if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(io::ErrorKind::NotADirectory.into());
            }
        }
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        Ok(file)
    }

    /// Reuse scanner metadata on Unix; Windows needs one handle for its file index.
    pub fn of_entry(path: &Path, metadata: &Metadata) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let _ = path;
            Ok(Self(metadata.dev(), metadata.ino()))
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Self::of_file(&File::open(path)?)
        }
    }
    pub fn parts(self) -> [u64; 2] {
        [self.0, self.1]
    }
    pub fn of_file(file: &File) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = file.metadata()?;
            Ok(Self(meta.dev(), meta.ino()))
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
            };
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            // SAFETY: the borrowed file owns a live handle and info is writable.
            if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(
                u64::from(info.dwVolumeSerialNumber),
                (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            ))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = file;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "file identity snapshots unavailable",
            ))
        }
    }
}
