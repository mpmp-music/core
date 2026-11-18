// Controllable local test backend. Never links fs-desktop or performs replacements.
use mpmp_storage::*;
use std::{
    fs::{File, Metadata},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub struct LocalStorage {
    pub calls: Mutex<Vec<&'static str>>,
    pub opened_paths: Mutex<Vec<PathBuf>>,
    pub capabilities: Option<WriteCapabilities>,
    pub open_error: Option<i32>,
    pub identities: bool,
}
impl Default for LocalStorage {
    fn default() -> Self {
        Self {
            calls: Mutex::default(),
            opened_paths: Mutex::default(),
            capabilities: None,
            open_error: None,
            identities: true,
        }
    }
}
impl LocalStorage {
    fn record(&self, op: &'static str) {
        self.calls.lock().unwrap().push(op);
    }
    pub fn backends(self: &Arc<Self>) -> StorageBackends {
        StorageBackends {
            files: self.clone(),
            database: self.clone(),
            replacements: self.clone(),
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
struct Identity([u64; 2]);
impl EntryIdentity for Identity {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn same(&self, other: &dyn EntryIdentity) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}
fn identity(file: &File) -> io::Result<[u64; 2]> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        Ok([meta.dev(), meta.ino()])
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: file owns a live handle and info points to writable storage.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok([
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ])
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "test identity unsupported",
        ))
    }
}
impl LibraryStorage for LocalStorage {
    fn canonical(&self, path: &Path) -> io::Result<LocalPath> {
        self.record("canonical");
        std::fs::canonicalize(path).map(LocalPath)
    }
    fn strip_verbatim(&self, path: PathBuf) -> PathBuf {
        path
    }
    fn comparable(&self, path: &Path) -> LocalPath {
        LocalPath(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned()))
    }
    fn case_insensitive_fs(&self, _: &Path) -> Option<bool> {
        None
    }
    fn case_insensitive_inside(&self, _: &Path) -> Option<bool> {
        None
    }
    fn same_dir(&self, a: &Path, b: &Path) -> bool {
        self.comparable(a) == self.comparable(b)
    }
    fn overlap(&self, a: &Path, b: &Path) -> bool {
        a.starts_with(b) || b.starts_with(a)
    }
    fn ensure_canonical(&self, path: &Path) -> StorageResult<()> {
        if self.canonical(path)?.0 == path {
            Ok(())
        } else {
            Err(StorageError::LocationChanged)
        }
    }
    fn open_regular(&self, path: &Path, checked: bool) -> StorageResult<OpenedRegular> {
        self.record("open_regular");
        self.opened_paths.lock().unwrap().push(path.to_owned());
        if let Some(code) = self.open_error {
            return Err(io::Error::from_raw_os_error(code).into());
        }
        if checked {
            self.ensure_canonical(path)?;
        }
        let file = File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(StorageError::NotRegular);
        }
        Ok(OpenedRegular { file })
    }
    fn entry_id(&self, path: &Path) -> io::Result<EntryId> {
        self.opened_id(&File::open(path)?)
    }
    fn entry_id_from_metadata(&self, path: &Path, _: &Metadata) -> io::Result<EntryId> {
        self.entry_id(path)
    }
    fn opened_id(&self, file: &File) -> io::Result<EntryId> {
        Ok(EntryId(Box::new(Identity(identity(file)?))))
    }
    fn entry_identity(&self, path: &Path, meta: &Metadata) -> io::Result<FileIdentity> {
        self.record("entry_identity");
        if !self.identities {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "identity unsupported",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let _ = path;
            Ok(FileIdentity([meta.dev(), meta.ino()]))
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            self.opened_identity(&File::open(path)?)
        }
    }
    fn opened_identity(&self, file: &File) -> io::Result<FileIdentity> {
        identity(file).map(FileIdentity)
    }
    fn write_capabilities(&self) -> WriteCapabilities {
        self.record("capabilities");
        self.capabilities.unwrap_or(WriteCapabilities {
            durable_replace: false,
            legacy_tag_write: false,
        })
    }
}
impl DatabaseStorage for LocalStorage {
    fn available_space(&self, _: &Path) -> io::Result<u64> {
        self.record("available_space");
        Ok(u64::MAX)
    }
}
impl DurableReplace for LocalStorage {
    fn supported(&self) -> bool {
        false
    }
    fn begin(&self, _: ReplacementPaths) -> StorageResult<Box<dyn ReplacementTransaction>> {
        self.record("begin");
        Err(StorageError::Unsupported {
            operation: Operation::DurableReplace,
        })
    }
    fn resume(&self, _: ReplacementPaths) -> StorageResult<Box<dyn ReplacementTransaction>> {
        self.record("resume");
        Err(StorageError::Unsupported {
            operation: Operation::DurableReplace,
        })
    }
}
