use super::*;

/// Why [`open_regular`] did not open a file.
#[derive(Debug)]
pub enum Unopened {
    /// `checked`, and a symlink is there now: put in the path's way.
    Changed,
    /// Not a regular file (a folder, a FIFO, a device).
    NotRegular,
    Io(io::Error),
}

impl From<io::Error> for Unopened {
    fn from(err: io::Error) -> Self {
        Unopened::Io(err)
    }
}

/// Opens an existing regular file to read it, without waiting on anything else: on Unix
/// with `O_NONBLOCK` (opening a FIFO that has no writer would block), then told by the
/// open file's own metadata. Anything but a regular file (a folder, a FIFO, a device) is
/// [`Unopened::NotRegular`], also when it cannot be opened at all (a folder on Windows). A
/// symlink is followed, as by any open, unless `checked` (the path is in canonical form,
/// so a link there was put in its way): then it is [`Unopened::Changed`], and on Unix
/// never opened (`O_NOFOLLOW`; a link to a device is not opened even in a race).
pub fn open_regular(path: &Path, checked: bool) -> Result<File, Unopened> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let nofollow = if checked { libc::O_NOFOLLOW } else { 0 };
        options.custom_flags(libc::O_NONBLOCK | nofollow);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(err) => {
            if checked && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
            {
                return Err(Unopened::Changed);
            }
            if std::fs::metadata(path).is_ok_and(|m| !m.is_file()) {
                return Err(Unopened::NotRegular);
            }
            return Err(Unopened::Io(err));
        }
    };
    if !file.metadata()?.is_file() {
        return Err(Unopened::NotRegular);
    }
    Ok(file)
}

/// Open a source while denying new Windows writers, but allowing its name to move.
/// Existing incompatible writers cause a sharing error before any replacement occurs.
pub fn open_replace_source(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        open_regular(path, true).map_err(|e| io::Error::other(format!("{e:?}")))
    }
}
