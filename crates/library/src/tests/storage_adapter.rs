//! Test-only desktop transaction adapter. Faults stay inside the original `_with`
//! primitives; ordinary calls always delegate to the injected transaction.
use crate::handle::TestHooks;
use mpmp_storage::*;
use std::{fs::File, io, path::Path, sync::Arc};

pub(crate) struct HookedReplacements {
    pub backend: Arc<dyn DurableReplace>,
    pub hooks: Arc<TestHooks>,
}

struct HookedTransaction {
    transaction: Box<dyn ReplacementTransaction>,
    paths: ReplacementPaths,
    hooks: Arc<TestHooks>,
    from_begin: bool,
}

impl DurableReplace for HookedReplacements {
    fn supported(&self) -> bool {
        self.backend.supported()
    }
    fn provides_tag_publish(&self) -> bool {
        self.backend.provides_tag_publish()
    }
    fn supports_tag_publish(&self, path: &Path) -> io::Result<TagPublish> {
        if let Some(mode) = *self.hooks.tag_publish_mode.lock().unwrap() {
            return Ok(mode);
        }
        self.backend.supports_tag_publish(path)
    }
    fn invalidate_tag_publish(&self, parent: &Path) {
        self.backend.invalidate_tag_publish(parent);
    }
    fn clean_tag_probes(&self, parent: &Path) -> io::Result<()> {
        self.backend.clean_tag_probes(parent)
    }
    fn begin(&self, paths: ReplacementPaths) -> StorageResult<Box<dyn ReplacementTransaction>> {
        Ok(Box::new(HookedTransaction {
            transaction: self.backend.begin(paths.clone())?,
            from_begin: true,
            paths,
            hooks: self.hooks.clone(),
        }))
    }
    fn resume(&self, paths: ReplacementPaths) -> StorageResult<Box<dyn ReplacementTransaction>> {
        Ok(Box::new(HookedTransaction {
            transaction: self.backend.resume(paths.clone())?,
            from_begin: false,
            paths,
            hooks: self.hooks.clone(),
        }))
    }
}

impl ReplacementTransaction for HookedTransaction {
    fn open_source(&self) -> io::Result<File> {
        self.transaction.open_source()
    }
    fn prepare_attributes(&self, staged: &Path, original: &Path) -> io::Result<()> {
        self.transaction.prepare_attributes(staged, original)
    }
    fn hidden(&self, path: &Path) -> io::Result<bool> {
        self.transaction.hidden(path)
    }
    fn set_hidden(&self, path: &Path, value: bool) -> io::Result<()> {
        self.transaction.set_hidden(path, value)
    }
    fn hide_candidate(&self, path: &Path) -> io::Result<()> {
        self.transaction.hide_candidate(path)
    }
    fn publish_tag(&self, mode: TagPublish) -> io::Result<()> {
        self.transaction.publish_tag(mode)
    }
    fn displace_tag(&self, source: &Path, backup: &Path, mode: TagPublish) -> io::Result<()> {
        self.transaction.displace_tag(source, backup, mode)
    }
    fn restore_tag(&self, source: &Path, target: &Path, mode: TagPublish) -> io::Result<()> {
        self.transaction.restore_tag(source, target, mode)
    }
    fn rollback_tag(&self, mode: TagPublish) -> io::Result<()> {
        self.transaction.rollback_tag(mode)
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        self.transaction.sync(path)
    }
    fn verify_attributes(&self) -> io::Result<()> {
        self.transaction.verify_attributes()
    }
    fn move_missing(&self, source: &Path, destination: &Path) -> io::Result<()> {
        self.transaction.move_missing(source, destination)
    }
    fn publish_missing(&self, source: &Path, destination: &Path) -> io::Result<()> {
        self.transaction.publish_missing(source, destination)
    }
    fn prepare_backup(&self, register: &mut RegisterBackup<'_>) -> io::Result<BackupPreparation> {
        #[cfg(unix)]
        if let Some(mode) = self
            .hooks
            .cover_fault
            .lock()
            .unwrap()
            .filter(|s| s.starts_with("backup_sync_fail"))
        {
            return mpmp_fs_desktop::paths::prepare_backup_with(
                &self.paths.destination.0,
                &self.paths.backup.as_ref().unwrap().0,
                |a, b| {
                    if mode.ends_with("copy") {
                        Err(io::Error::from_raw_os_error(1))
                    } else {
                        std::fs::hard_link(a, b)
                    }
                },
                |_| Err(io::Error::other("injected first backup fsync failure")),
                |id, copied| register(FileIdentity(id.parts()), copied),
            )
            .map(|copied| BackupPreparation::Registered { copied });
        }
        self.transaction.prepare_backup(register)
    }
    fn publish(&self, preparation: BackupPreparation) -> io::Result<()> {
        if let Some(hook) = self.hooks.cover_replace.lock().unwrap().as_ref() {
            return hook(
                &self.paths.staged.as_ref().unwrap().0,
                &self.paths.destination.0,
                &self.paths.backup.as_ref().unwrap().0,
            );
        }
        self.transaction.publish(preparation)
    }
    fn seal(&self) -> io::Result<()> {
        if self.from_begin && *self.hooks.cover_fault.lock().unwrap() == Some("seal_interrupted") {
            self.transaction.move_missing(
                &self.paths.destination.0,
                &self.paths.staged.as_ref().unwrap().0,
            )?;
            return Err(io::Error::other(
                "injected interruption during write-through seal",
            ));
        }
        self.transaction.seal()
    }
    fn rollback(&self) -> io::Result<()> {
        #[cfg(unix)]
        if *self.hooks.cover_fault.lock().unwrap() == Some("rollback_no_exchange_sync_fail") {
            return mpmp_fs_desktop::paths::rollback_with(
                &self.paths.destination.0,
                &self.paths.backup.as_ref().unwrap().0,
                &self.paths.staged.as_ref().unwrap().0,
                &self.paths.saved_after.as_ref().unwrap().0,
                |_, _| {
                    Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "exchange unavailable",
                    ))
                },
                |a, b| std::fs::rename(a, b),
                |p| {
                    if p == self.paths.destination.0 {
                        Err(io::Error::other("post-rename fsync failed"))
                    } else {
                        self.transaction.sync(p)
                    }
                },
            );
        }
        self.transaction.rollback()
    }
}
