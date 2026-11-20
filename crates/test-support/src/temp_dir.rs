use std::io;
use std::path::Path;

/// A test directory that checks for open files before cleanup.
///
/// Declare it before local handles, or after the handles in a struct, so it drops last.
#[derive(Debug)]
pub struct TempDir {
    dir: Option<tempfile::TempDir>,
    #[cfg(target_os = "linux")]
    canonical_path: std::path::PathBuf,
}

impl TempDir {
    pub fn new() -> io::Result<Self> {
        let dir = tempfile::tempdir()?;
        Ok(Self {
            #[cfg(target_os = "linux")]
            canonical_path: dir.path().canonicalize()?,
            dir: Some(dir),
        })
    }

    pub fn path(&self) -> &Path {
        self.dir.as_ref().unwrap().path()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Let tempfile clean up during unwinding without risking a second panic.
        if std::thread::panicking() {
            return;
        }

        #[cfg(target_os = "linux")]
        {
            let paths = open_paths(&self.canonical_path).unwrap_or_else(|error| {
                panic!("scan /proc/self/fd for {}: {error}", self.path().display())
            });
            assert!(
                paths.is_empty(),
                "open file descriptors under {}: {paths:#?}",
                self.path().display()
            );
        }

        let dir = self.dir.take().unwrap();
        let path = dir.path().to_owned();
        dir.close()
            .unwrap_or_else(|error| panic!("remove test directory {}: {error}", path.display()));
    }
}

#[cfg(target_os = "linux")]
fn open_paths(root: &Path) -> io::Result<Vec<std::path::PathBuf>> {
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    let mut paths = Vec::new();
    for entry in fs::read_dir("/proc/self/fd")? {
        let path = match fs::read_link(entry?.path()) {
            Ok(path) => path,
            // Another test can close a descriptor between read_dir and read_link.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        // Linux appends this suffix when the file was unlinked while still open.
        let target = path.as_os_str().as_bytes();
        let target = Path::new(OsStr::from_bytes(
            target.strip_suffix(b" (deleted)").unwrap_or(target),
        ));
        if target.starts_with(root) {
            paths.push(path);
        }
    }
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs::{self, File};

    #[test]
    #[should_panic(expected = "still-open.txt")]
    fn drop_reports_an_open_unlinked_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("still-open.txt");
        let _file = File::create(&path).unwrap();
        fs::remove_file(path).unwrap();
        drop(dir);
    }

    #[test]
    fn drop_ignores_another_directory_with_an_open_file() {
        let other = TempDir::new().unwrap();
        let _file = File::create(other.path().join("other.txt")).unwrap();
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_owned();
        drop(dir);
        assert!(!path.exists());
    }
}
