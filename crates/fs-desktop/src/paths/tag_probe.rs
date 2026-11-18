//! Tag-volume discovery and the hard-link fallback for volumes without rename flags.
use super::*;
use mpmp_storage::TagPublish;
use std::collections::HashMap;
use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type VolumeCache = HashMap<(PathBuf, [u64; 2]), TagPublish>;
static CACHE: LazyLock<Mutex<VolumeCache>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NONCE: AtomicU64 = AtomicU64::new(0);
const PREFIX: &str = ".player-tags-probe-";
const SUFFIX: &str = ".tags.tmp";

fn name(suffix: char) -> String {
    let ticks = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    format!("{PREFIX}{ticks:x}-{pid:x}-{n:x}-{suffix}{SUFFIX}")
}

pub(crate) fn probe_tag_target(target: &Path) -> io::Result<TagPublish> {
    let parent = target.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let dir = FileIdentity::directory(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        const STICKY: u32 = 0o1000;
        let meta = dir.metadata()?;
        // SAFETY: geteuid has no pointers and always returns the effective user ID.
        let user = unsafe { libc::geteuid() };
        if meta.mode() & STICKY != 0
            && user != 0
            && user != meta.uid()
            && user != std::fs::symlink_metadata(target)?.uid()
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        std::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(target)?;
    }
    probe_open(parent, &dir)
}

#[cfg(test)]
pub(crate) fn probe_tag_publish(parent: &Path) -> io::Result<TagPublish> {
    probe_open(parent, &FileIdentity::directory(parent)?)
}

fn probe_open(parent: &Path, dir: &File) -> io::Result<TagPublish> {
    let id = FileIdentity::of_file(dir)?;
    let key = (parent.to_owned(), id.parts());
    #[cfg(unix)]
    unix::check_writable(dir)?;
    if let Some(mode) = CACHE.lock().unwrap().get(&key).copied() {
        return Ok(mode);
    }
    #[cfg(unix)]
    let mode = unix::probe(dir, replace::rename_at)?;
    #[cfg(windows)]
    let mode = probe_windows(parent)?;
    // Keep the same directory handle open through probing and cleanup. Refuse
    // to cache a result under a name that has since been redirected.
    if FileIdentity::of_file(&FileIdentity::directory(parent)?)? != id {
        return Err(io::ErrorKind::NotFound.into());
    }
    let mut cache = CACHE.lock().unwrap();
    if cache.len() >= 1024 {
        cache.clear();
    }
    cache.insert(key, mode);
    Ok(mode)
}

pub(crate) fn invalidate_tag_probe(parent: &Path) {
    CACHE.lock().unwrap().retain(|(path, _), _| path != parent);
}

pub(crate) fn clean_tag_probes(parent: &Path) -> io::Result<()> {
    let dir = FileIdentity::directory(parent)?;
    let id = FileIdentity::of_file(&dir)?;
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let file = entry.file_name();
        let name = file.to_string_lossy();
        if !name.starts_with(PREFIX) || !name.ends_with(SUFFIX) {
            continue;
        }
        let meta = std::fs::symlink_metadata(entry.path())?;
        if meta.is_file()
            && meta.len() == 0
            && meta.modified()?.elapsed().unwrap_or_default() > Duration::from_secs(86400)
            && FileIdentity::of_file(&FileIdentity::directory(parent)?)? == id
        {
            #[cfg(unix)]
            unix::remove(&dir, &std::ffi::CString::new(name.as_bytes())?)?;
            #[cfg(windows)]
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn probe_windows(parent: &Path) -> io::Result<TagPublish> {
    let create = || {
        // A Windows tempfile has FILE_ATTRIBUTE_TEMPORARY, which the actual
        // music publication clears before any durable flush.
        tempfile::Builder::new()
            .prefix(PREFIX)
            .suffix(SUFFIX)
            .make_in(parent, |path| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
            })
            .map(|file| file.into_temp_path())
    };
    let first = create()?;
    let second = create()?;
    let backup = parent.join(name('c'));
    let source = open_replace_source(&first)?;
    let original = FileIdentity::of_file(&source)?;
    let staged = FileIdentity::of_file(&File::open(&second)?)?;
    let result = replace_with_backup(&second, &first, &backup);
    drop(source);
    let checked = result.and_then(|()| {
        sync_replacement(&first)?;
        Ok(TagPublish::Replace {
            stable_ids: original != staged
                && FileIdentity::of_file(&File::open(&first)?)? == staged
                && FileIdentity::of_file(&File::open(&backup)?)? == original,
        })
    });
    if backup.exists() {
        std::fs::remove_file(&backup)?;
    }
    checked
}

#[cfg(unix)]
pub(crate) fn link_missing(source: &Path, destination: &Path) -> io::Result<()> {
    // The source is a private staging/backup name, never a public music name.
    std::fs::hard_link(source, destination)?;
    let identity = |path: &Path| {
        std::fs::symlink_metadata(path)
            .ok()
            .and_then(|meta| FileIdentity::of_entry(path, &meta).ok())
    };
    if let Some(id) = identity(source)
        && identity(destination) == Some(id)
    {
        // The new link is already installed. Leave the old name registered if its
        // removal fails instead of reporting that installation did not happen.
        let _ = std::fs::remove_file(source);
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn move_to_private(source: &Path, backup: &Path) -> io::Result<()> {
    // A plain rename is safe in this direction: reserve a fresh private name, then
    // move whichever inode actually owns the public name in one syscall.
    let reserved = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(backup)?;
    let result = std::fs::rename(source, backup);
    if result.is_err()
        && std::fs::symlink_metadata(backup)
            .ok()
            .filter(|meta| meta.len() == 0)
            .and_then(|meta| FileIdentity::of_entry(backup, &meta).ok())
            == FileIdentity::of_file(&reserved).ok()
    {
        let _ = std::fs::remove_file(backup);
    }
    result
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::ffi::{CStr, CString};
    use std::os::fd::{AsRawFd, FromRawFd};

    pub(super) fn check_writable(dir: &File) -> io::Result<()> {
        // SAFETY: dir is open and the relative path is terminated.
        if unsafe { libc::faccessat(dir.as_raw_fd(), c".".as_ptr(), libc::W_OK | libc::X_OK, 0) }
            == 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn open(dir: &File, name: &CStr, create: bool) -> io::Result<File> {
        let flags = libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | if create {
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL
            } else {
                libc::O_RDONLY
            };
        // SAFETY: dir is open and the path is terminated and relative.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if fd == -1 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: openat returned an owned descriptor.
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    pub(super) fn remove(dir: &File, name: &CStr) -> io::Result<()> {
        // SAFETY: dir is open and the name is terminated.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn link(dir: &File, a: &CStr, b: &CStr) -> io::Result<()> {
        let fd = dir.as_raw_fd();
        // SAFETY: fd stays open and both relative names are terminated.
        if unsafe { libc::linkat(fd, a.as_ptr(), fd, b.as_ptr(), 0) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    struct Probe<'a>(&'a File, CString);

    impl Drop for Probe<'_> {
        fn drop(&mut self) {
            let _ = remove(self.0, &self.1);
        }
    }

    fn refuses_overwrite(result: io::Result<()>) -> io::Result<()> {
        match result {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
            Ok(()) => Err(io::ErrorKind::Unsupported.into()),
        }
    }

    pub(super) fn probe(
        dir: &File,
        rename: impl Fn(libc::c_int, &CStr, &CStr, bool) -> io::Result<()>,
    ) -> io::Result<TagPublish> {
        let first = CString::new(name('a'))?;
        let source = open(dir, &first, true)?;
        let first = Probe(dir, first);
        let second = CString::new(name('b'))?;
        let other = open(dir, &second, true)?;
        let second = Probe(dir, second);
        let (a, b) = (&first.1, &second.1);
        let fd = dir.as_raw_fd();
        other.sync_all()?;
        dir.sync_all()?;
        match rename(fd, a, b, true) {
            Ok(()) => return Ok(TagPublish::Exchange),
            Err(e) if replace::exchange_unsupported(&e) => {}
            Err(e) => return Err(e),
        }
        match refuses_overwrite(rename(fd, a, b, false)) {
            Ok(()) => {
                remove(dir, b)?;
                rename(fd, a, b, false)?;
                Ok(TagPublish::Exclusive {
                    stable_ids: FileIdentity::of_file(&source)?
                        == FileIdentity::of_file(&open(dir, b, false)?)?,
                })
            }
            Err(e) if replace::exchange_unsupported(&e) => {
                refuses_overwrite(link(dir, a, b))?;
                remove(dir, b)?;
                link(dir, a, b)?;
                remove(dir, a)?;
                Ok(TagPublish::Link)
            }
            Err(e) => Err(e),
        }
    }

    #[test]
    fn no_rename_flags_selects_a_no_clobber_link_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let handle = FileIdentity::directory(dir.path()).unwrap();
        assert_eq!(
            probe(&handle, |_, _, _, _| Err(io::Error::from_raw_os_error(
                libc::EINVAL
            )))
            .unwrap(),
            TagPublish::Link
        );
        assert_eq!(dir.path().read_dir().unwrap().count(), 0);
    }
}
