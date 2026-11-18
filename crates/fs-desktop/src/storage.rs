//! Thin adapters over the desktop primitives; transactions only retain supplied paths.
use crate::paths;
use mpmp_storage::*;
use std::{
    any::Any,
    fs::{File, Metadata},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Default)]
pub struct DesktopStorage;

impl DesktopStorage {
    pub fn backends() -> StorageBackends {
        let desktop = Arc::new(Self);
        StorageBackends {
            files: desktop.clone(),
            database: desktop.clone(),
            replacements: desktop,
        }
    }
}

#[derive(Debug)]
struct DesktopEntryId(paths::FileId);

impl EntryIdentity for DesktopEntryId {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn same(&self, other: &dyn EntryIdentity) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|other| self.0 == other.0)
    }
}

fn entry(id: paths::FileId) -> EntryId {
    EntryId(Box::new(DesktopEntryId(id)))
}

impl LibraryStorage for DesktopStorage {
    fn canonical(&self, path: &Path) -> io::Result<LocalPath> {
        paths::canonical_dir(path).map(LocalPath)
    }
    fn strip_verbatim(&self, path: PathBuf) -> PathBuf {
        paths::strip_verbatim(path)
    }
    fn comparable(&self, path: &Path) -> LocalPath {
        LocalPath(paths::comparable(path))
    }
    fn case_insensitive_fs(&self, path: &Path) -> Option<bool> {
        paths::case_insensitive_fs(path)
    }
    fn case_insensitive_inside(&self, path: &Path) -> Option<bool> {
        paths::case_insensitive_inside(path)
    }
    fn same_dir(&self, a: &Path, b: &Path) -> bool {
        paths::same_dir(a, b)
    }
    fn overlap(&self, a: &Path, b: &Path) -> bool {
        paths::overlap(a, b)
    }
    fn ensure_canonical(&self, path: &Path) -> StorageResult<()> {
        paths::ensure_canonical(path).map_err(|e| match e {
            paths::Uncanonical::Missing(e) => StorageError::Io(e),
            paths::Uncanonical::Changed => StorageError::LocationChanged,
        })
    }
    fn open_regular(&self, path: &Path, checked: bool) -> StorageResult<OpenedRegular> {
        paths::open_regular(path, checked)
            .map(|file| OpenedRegular { file })
            .map_err(|e| match e {
                paths::Unopened::Changed => StorageError::LocationChanged,
                paths::Unopened::NotRegular => StorageError::NotRegular,
                paths::Unopened::Io(e) => StorageError::Io(e),
            })
    }
    fn open_tag_source(&self, path: &Path, checked: bool) -> StorageResult<OpenedRegular> {
        #[cfg(windows)]
        {
            if checked {
                self.ensure_canonical(path)?;
            }
            let file = paths::open_replace_source(path)?;
            if !file.metadata()?.is_file() {
                return Err(StorageError::NotRegular);
            }
            Ok(OpenedRegular { file })
        }
        #[cfg(not(windows))]
        self.open_regular(path, checked)
    }
    fn entry_id(&self, path: &Path) -> io::Result<EntryId> {
        paths::FileId::of_path(path).map(entry)
    }
    fn entry_id_from_metadata(&self, path: &Path, meta: &Metadata) -> io::Result<EntryId> {
        paths::FileId::of_entry(path, meta).map(entry)
    }
    fn opened_id(&self, file: &File) -> io::Result<EntryId> {
        paths::FileId::of_file(file).map(entry)
    }
    fn entry_identity(&self, path: &Path, meta: &Metadata) -> io::Result<FileIdentity> {
        paths::FileIdentity::of_entry(path, meta).map(|id| FileIdentity(id.parts()))
    }
    fn directory_identity(&self, path: &Path) -> io::Result<FileIdentity> {
        let dir = paths::FileIdentity::directory(path)?;
        paths::FileIdentity::of_file(&dir).map(|id| FileIdentity(id.parts()))
    }
    fn opened_identity(&self, file: &File) -> io::Result<FileIdentity> {
        paths::FileIdentity::of_file(file).map(|id| FileIdentity(id.parts()))
    }
    fn write_capabilities(&self) -> WriteCapabilities {
        WriteCapabilities {
            durable_replace: true,
            legacy_tag_write: true,
        }
    }
}

impl DatabaseStorage for DesktopStorage {
    fn available_space(&self, path: &Path) -> io::Result<u64> {
        crate::available_space(path)
    }
}

#[derive(Debug)]
pub struct DesktopReplacementTransaction {
    paths: ReplacementPaths,
}

impl DesktopReplacementTransaction {
    #[cfg(unix)]
    fn prepare_backup_using(
        &self,
        register: &mut RegisterBackup<'_>,
        prepare: impl FnOnce(
            &Path,
            &Path,
            &mut dyn FnMut(paths::FileIdentity, bool) -> io::Result<()>,
        ) -> io::Result<bool>,
    ) -> io::Result<BackupPreparation> {
        let copied = prepare(
            &self.paths.destination.0,
            required(&self.paths.backup)?,
            &mut |id, copied| register(FileIdentity(id.parts()), copied),
        )?;
        Ok(BackupPreparation::Registered { copied })
    }
}

impl DurableReplace for DesktopStorage {
    fn supported(&self) -> bool {
        cfg!(any(unix, windows))
    }
    fn provides_tag_publish(&self) -> bool {
        cfg!(any(unix, windows))
    }
    fn supports_tag_publish(&self, target: &Path) -> io::Result<TagPublish> {
        paths::probe_tag_target(target)
    }
    fn invalidate_tag_publish(&self, parent: &Path) {
        paths::invalidate_tag_probe(parent);
    }
    fn clean_tag_probes(&self, parent: &Path) -> io::Result<()> {
        paths::clean_tag_probes(parent)
    }
    fn begin(&self, paths: ReplacementPaths) -> StorageResult<Box<dyn ReplacementTransaction>> {
        if !self.supported() {
            return Err(StorageError::Unsupported {
                operation: Operation::DurableReplace,
            });
        }
        Ok(Box::new(DesktopReplacementTransaction { paths }))
    }
    fn resume(&self, paths: ReplacementPaths) -> StorageResult<Box<dyn ReplacementTransaction>> {
        self.begin(paths)
    }
}

fn required(path: &Option<LocalPath>) -> io::Result<&Path> {
    path.as_ref().map(|p| p.0.as_path()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing registered replacement path",
        )
    })
}

impl ReplacementTransaction for DesktopReplacementTransaction {
    fn open_source(&self) -> io::Result<File> {
        paths::open_replace_source(&self.paths.destination.0)
    }
    fn prepare_attributes(&self, staged: &Path, original: &Path) -> io::Result<()> {
        paths::prepare_replacement(staged, original)
    }
    fn hidden(&self, path: &Path) -> io::Result<bool> {
        paths::hidden(path)
    }
    fn set_hidden(&self, path: &Path, value: bool) -> io::Result<()> {
        paths::set_hidden(path, value)
    }
    fn hide_candidate(&self, path: &Path) -> io::Result<()> {
        paths::hide_candidate(path)
    }
    fn prepare_backup(&self, register: &mut RegisterBackup<'_>) -> io::Result<BackupPreparation> {
        #[cfg(unix)]
        {
            self.prepare_backup_using(register, |destination, backup, register| {
                paths::prepare_backup_registered(destination, backup, register)
            })
        }
        #[cfg(windows)]
        {
            let _ = register;
            Ok(BackupPreparation::DuringPublish)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = register;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "durable replacement unavailable",
            ))
        }
    }
    fn publish(&self, preparation: BackupPreparation) -> io::Result<()> {
        #[cfg(any(unix, windows))]
        let staged = required(&self.paths.staged)?;
        #[cfg(any(unix, windows))]
        let backup = required(&self.paths.backup)?;
        #[cfg(unix)]
        {
            let BackupPreparation::Registered { copied } = preparation else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "backup preparation does not match platform",
                ));
            };
            paths::replace_prepared_with_backup(staged, &self.paths.destination.0, backup, copied)
        }
        #[cfg(windows)]
        {
            if preparation != BackupPreparation::DuringPublish {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "backup preparation does not match platform",
                ));
            }
            paths::replace_with_backup(staged, &self.paths.destination.0, backup)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = preparation;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "durable replacement unavailable",
            ))
        }
    }
    fn publish_tag(&self, mode: TagPublish) -> io::Result<()> {
        let staged = required(&self.paths.staged)?;
        let destination = &self.paths.destination.0;
        match mode {
            #[cfg(unix)]
            TagPublish::Exchange => paths::exchange(staged, destination),
            TagPublish::Exclusive { .. } => paths::move_missing(staged, destination)
                .and_then(|()| paths::sync_parent(destination)),
            #[cfg(unix)]
            TagPublish::Link => paths::link_missing(staged, destination)
                .and_then(|()| paths::sync_parent(destination)),
            #[cfg(windows)]
            TagPublish::Replace { .. } => {
                paths::replace_with_backup(staged, destination, required(&self.paths.backup)?)
            }
            #[allow(unreachable_patterns)]
            _ => Err(io::ErrorKind::Unsupported.into()),
        }
    }
    fn displace_tag(&self, source: &Path, backup: &Path, mode: TagPublish) -> io::Result<()> {
        #[cfg(unix)]
        if mode == TagPublish::Link {
            return paths::move_to_private(source, backup);
        }
        let _ = mode;
        paths::move_missing(source, backup)
    }
    fn restore_tag(&self, source: &Path, target: &Path, mode: TagPublish) -> io::Result<()> {
        #[cfg(unix)]
        if mode == TagPublish::Link {
            return paths::link_missing(source, target);
        }
        #[cfg(unix)]
        if matches!(mode, TagPublish::Exclusive { .. }) {
            return paths::move_missing(source, target);
        }
        let _ = mode;
        paths::publish_missing(source, target)
    }
    fn rollback_tag(&self, mode: TagPublish) -> io::Result<()> {
        let staged = required(&self.paths.staged)?;
        let destination = &self.paths.destination.0;
        match mode {
            #[cfg(unix)]
            TagPublish::Exchange => paths::exchange(staged, destination),
            TagPublish::Exclusive { .. } => {
                paths::move_missing(destination, staged)?;
                paths::publish_missing(required(&self.paths.backup)?, destination)
            }
            #[cfg(windows)]
            TagPublish::Replace { .. } => {
                paths::replace_with_backup(required(&self.paths.backup)?, destination, staged)
            }
            #[allow(unreachable_patterns)]
            _ => Err(io::ErrorKind::Unsupported.into()),
        }
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        paths::sync_replacement(path)
    }
    fn seal(&self) -> io::Result<()> {
        paths::seal_replacement(&self.paths.destination.0, required(&self.paths.staged)?)
    }
    fn verify_attributes(&self) -> io::Result<()> {
        paths::verify_replacement_attributes(
            &self.paths.destination.0,
            required(&self.paths.backup)?,
        )
    }
    fn move_missing(&self, source: &Path, destination: &Path) -> io::Result<()> {
        paths::move_missing(source, destination)
    }
    fn publish_missing(&self, source: &Path, destination: &Path) -> io::Result<()> {
        paths::publish_missing(source, destination)
    }
    fn rollback(&self) -> io::Result<()> {
        paths::rollback_replacement(
            &self.paths.destination.0,
            required(&self.paths.backup)?,
            required(&self.paths.staged)?,
            required(&self.paths.saved_after)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    fn same_error(actual: io::Error, expected: io::Error) {
        assert_eq!(actual.kind(), expected.kind());
        assert_eq!(actual.raw_os_error(), expected.raw_os_error());
        assert_eq!(actual.to_string(), expected.to_string());
    }
    fn paths(dir: &Path) -> ReplacementPaths {
        ReplacementPaths {
            destination: LocalPath(dir.join("original")),
            staged: Some(LocalPath(dir.join("staged"))),
            backup: Some(LocalPath(dir.join("backup"))),
            saved_after: Some(LocalPath(dir.join("saved_after"))),
        }
    }

    #[test]
    fn adapter_forwarding_preserves_errors_and_identity_lifetimes() {
        let dir = tempfile::tempdir().unwrap();
        let desktop = DesktopStorage;
        let missing = dir.path().join("missing");
        same_error(
            desktop.canonical(&missing).unwrap_err(),
            paths::canonical_dir(&missing).unwrap_err(),
        );
        let StorageError::Io(error) = desktop.open_regular(&missing, false).unwrap_err() else {
            panic!("missing file must retain I/O error")
        };
        let paths::Unopened::Io(expected) = paths::open_regular(&missing, false).unwrap_err()
        else {
            panic!("expected I/O error")
        };
        same_error(error, expected);
        let StorageError::Io(error) = desktop.ensure_canonical(&missing).unwrap_err() else {
            panic!("missing canonical must retain I/O error")
        };
        let paths::Uncanonical::Missing(expected) = paths::ensure_canonical(&missing).unwrap_err()
        else {
            panic!("expected missing")
        };
        same_error(error, expected);
        assert!(matches!(
            desktop.open_regular(dir.path(), false),
            Err(StorageError::NotRegular)
        ));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path(), dir.path().join("link")).unwrap();
            assert!(matches!(
                desktop.open_regular(&dir.path().join("link"), true),
                Err(StorageError::LocationChanged)
            ));
            assert!(matches!(
                desktop.ensure_canonical(&dir.path().join("link")),
                Err(StorageError::LocationChanged)
            ));
        }
        let source = StorageError::Io(io::Error::other("original diagnostic"));
        let source = source
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert_eq!(source.get_ref().unwrap().to_string(), "original diagnostic");

        let path = dir.path().join("file");
        std::fs::write(&path, b"contents").unwrap();
        #[cfg(not(windows))]
        let file = File::open(&path).unwrap();
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(3)
                .open(&path)
                .unwrap()
        };
        let opened = desktop.opened_id(&file).unwrap();
        let entry = desktop.entry_id(&path).unwrap();
        assert_eq!(opened, entry);
        assert_eq!(
            opened
                .0
                .as_any()
                .downcast_ref::<DesktopEntryId>()
                .unwrap()
                .0,
            paths::FileId::of_file(&file).unwrap()
        );
        assert_eq!(
            desktop.opened_identity(&file).unwrap().parts(),
            paths::FileIdentity::of_file(&file).unwrap().parts()
        );
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            desktop.entry_id_from_metadata(&path, &metadata).unwrap(),
            entry
        );
        let moved = dir.path().join("moved");
        #[cfg(windows)]
        {
            // The cloned identity keeps the source's share mode after File is dropped.
            drop(file);
            let actual = std::fs::rename(&path, &moved).unwrap_err();
            drop(opened);
            drop(entry);
            use std::os::windows::fs::OpenOptionsExt;
            let direct_file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(3)
                .open(&path)
                .unwrap();
            let direct = paths::FileId::of_file(&direct_file).unwrap();
            drop(direct_file);
            let expected = std::fs::rename(&path, &moved).unwrap_err();
            same_error(actual, expected);
            drop(direct);
        }
        #[cfg(unix)]
        {
            // Metadata-only snapshots still work when their path has disappeared:
            // neither identity construction nor equality may add an open/stat.
            std::fs::rename(&path, &moved).unwrap();
            assert_eq!(
                desktop.entry_id_from_metadata(&missing, &metadata).unwrap(),
                opened
            );
            assert_eq!(
                desktop.entry_identity(&missing, &metadata).unwrap(),
                desktop.opened_identity(&file).unwrap()
            );
            assert_eq!(opened, entry);
            drop(opened);
            drop(entry);
        }
        #[cfg(not(windows))]
        drop(file);
        #[cfg(windows)]
        std::fs::rename(&path, &moved).unwrap();
        std::fs::remove_file(&moved).unwrap();

        // begin/resume and Drop must do no I/O, even for missing paths.
        let absent = dir.path().join("absent");
        drop(desktop.begin(paths(&absent)).unwrap());
        drop(desktop.resume(paths(&absent)).unwrap());
        assert!(!absent.exists());
        let transaction = desktop
            .resume(ReplacementPaths {
                destination: LocalPath(missing),
                staged: None,
                backup: None,
                saved_after: None,
            })
            .unwrap();
        assert_eq!(
            transaction.seal().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            transaction.verify_attributes().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            transaction.rollback().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            transaction
                .publish(BackupPreparation::DuringPublish)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn adapter_backup_registration_precedes_copy_and_sync() {
        let dir = tempfile::tempdir().unwrap();
        let registered_paths = paths(dir.path());
        let destination = &registered_paths.destination.0;
        let backup = &registered_paths.backup.as_ref().unwrap().0;
        std::fs::write(destination, b"before").unwrap();
        let transaction = DesktopReplacementTransaction {
            paths: registered_paths.clone(),
        };
        #[cfg(unix)]
        {
            use std::cell::RefCell;
            let trace = RefCell::new(Vec::new());
            // Exercise the production adapter over the real hard-link path.
            assert_eq!(
                transaction
                    .prepare_backup(&mut |identity, copied| {
                        trace.borrow_mut().push("register");
                        assert!(!copied);
                        assert_eq!(
                            identity.parts(),
                            paths::FileIdentity::of_file(&File::open(backup)?)?.parts()
                        );
                        Ok(())
                    })
                    .unwrap(),
                BackupPreparation::Registered { copied: false }
            );
            assert_eq!(*trace.borrow(), ["register"]);
            std::fs::remove_file(backup).unwrap();
            trace.borrow_mut().clear();
            // Inject only link/sync primitives through the same adapter callback bridge.
            let result = transaction.prepare_backup_using(
                &mut |identity, copied| {
                    trace.borrow_mut().push("register");
                    assert!(copied);
                    assert_eq!(std::fs::metadata(backup)?.len(), 0);
                    assert_eq!(
                        identity.parts(),
                        paths::FileIdentity::of_file(&File::open(backup)?)?.parts()
                    );
                    Ok(())
                },
                |destination, backup, register| {
                    paths::prepare_backup_with(
                        destination,
                        backup,
                        |_, _| Err(io::ErrorKind::Unsupported.into()),
                        |path| {
                            trace.borrow_mut().push("sync");
                            assert_eq!(std::fs::read(path)?, b"before");
                            paths::sync_replacement(path)
                        },
                        register,
                    )
                },
            );
            assert_eq!(
                result.unwrap(),
                BackupPreparation::Registered { copied: true }
            );
            assert_eq!(*trace.borrow(), ["register", "sync"]);
            std::fs::remove_file(backup).unwrap();
            // A failed durable registration must prevent both copying and sync.
            let result = transaction.prepare_backup_using(
                &mut |_, copied| {
                    assert!(copied);
                    Err(io::Error::from_raw_os_error(123))
                },
                |destination, backup, register| {
                    paths::prepare_backup_with(
                        destination,
                        backup,
                        |_, _| Err(io::ErrorKind::Unsupported.into()),
                        |_| panic!("sync after registration failure"),
                        register,
                    )
                },
            );
            assert_eq!(result.unwrap_err().raw_os_error(), Some(123));
            assert_eq!(std::fs::metadata(backup).unwrap().len(), 0);
            assert_eq!(std::fs::read(destination).unwrap(), b"before");
            assert_eq!(
                transaction
                    .publish(BackupPreparation::DuringPublish)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        #[cfg(windows)]
        {
            assert_eq!(
                transaction
                    .prepare_backup(&mut |_, _| panic!("Windows registers during publish"))
                    .unwrap(),
                BackupPreparation::DuringPublish
            );
            assert!(!backup.exists());
            assert_eq!(
                transaction
                    .publish(BackupPreparation::Registered { copied: false })
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
}
