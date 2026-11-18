//! Transactions share the real cover edit and recovery fixtures.
use super::*;
#[derive(Clone, Copy, PartialEq, Eq)]
enum TransactionMode {
    Desktop,
    Copy,
    BackupSyncError,
    PublishError,
    PublishBeforeError,
    SealError,
    SealBeforeError,
    SyncBackupError,
    VerifyError,
    AttributesError,
}

struct TransactionRecorder {
    db: PathBuf,
    trace: std::sync::Mutex<Vec<String>>,
    mode: TransactionMode,
}

impl TransactionRecorder {
    fn record(&self, event: impl Into<String>) {
        self.trace.lock().unwrap().push(event.into());
    }
    fn events(&self) -> Vec<String> {
        self.trace.lock().unwrap().clone()
    }
}

struct RecordedTransaction {
    backend: Arc<TransactionRecorder>,
    paths: mpmp_storage::ReplacementPaths,
    desktop: Box<dyn mpmp_storage::ReplacementTransaction>,
}

struct RecordingReplacements(Arc<TransactionRecorder>);

impl mpmp_storage::DurableReplace for RecordingReplacements {
    fn supported(&self) -> bool {
        true
    }
    fn begin(
        &self,
        paths: mpmp_storage::ReplacementPaths,
    ) -> mpmp_storage::StorageResult<Box<dyn mpmp_storage::ReplacementTransaction>> {
        Ok(Box::new(RecordedTransaction {
            backend: self.0.clone(),
            desktop: mpmp_fs_desktop::DesktopStorage.begin(paths.clone())?,
            paths,
        }))
    }
    fn resume(
        &self,
        paths: mpmp_storage::ReplacementPaths,
    ) -> mpmp_storage::StorageResult<Box<dyn mpmp_storage::ReplacementTransaction>> {
        self.begin(paths)
    }
}

impl RecordedTransaction {
    fn role(&self, path: &Path) -> &'static str {
        if path == self.paths.destination.0 {
            "live"
        } else if self.paths.staged.as_ref().is_some_and(|p| p.0 == path) {
            "stage"
        } else if self.paths.backup.as_ref().is_some_and(|p| p.0 == path) {
            "backup"
        } else if self.paths.saved_after.as_ref().is_some_and(|p| p.0 == path) {
            "after"
        } else {
            panic!("unregistered transaction path: {}", path.display())
        }
    }
}

impl mpmp_storage::ReplacementTransaction for RecordedTransaction {
    fn open_source(&self) -> std::io::Result<std::fs::File> {
        self.backend.record("open_source");
        self.desktop.open_source()
    }
    fn prepare_attributes(&self, staged: &Path, original: &Path) -> std::io::Result<()> {
        self.backend
            .record(format!("attributes:{}", self.role(staged)));
        self.desktop.prepare_attributes(staged, original)?;
        if self.backend.mode == TransactionMode::AttributesError {
            return Err(std::io::Error::from_raw_os_error(123));
        }
        Ok(())
    }
    fn prepare_backup(
        &self,
        register: &mut mpmp_storage::RegisterBackup<'_>,
    ) -> std::io::Result<mpmp_storage::BackupPreparation> {
        self.backend.record("prepare_backup");
        let mut checked_register = |id: mpmp_storage::FileIdentity, copied| {
            // Check the real durable JSON after library's registration callback,
            // while a newly created full-copy candidate still has zero bytes.
            self.backend.record("register");
            register(id, copied)?;
            let conn = rusqlite::Connection::open(&self.backend.db).unwrap();
            let target: String = conn
                .query_row(
                    "SELECT target_json FROM file_write_intents ORDER BY id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let target: Target = parse(&target).unwrap();
            let backup = target.backup.unwrap();
            assert_eq!(backup.identity, id.parts());
            assert_eq!(backup.copied, copied);
            if copied {
                assert_eq!(std::fs::metadata(&backup.path)?.len(), 0);
            }
            Ok(())
        };
        #[cfg(unix)]
        if matches!(
            self.backend.mode,
            TransactionMode::Copy | TransactionMode::BackupSyncError
        ) {
            return mpmp_fs_desktop::paths::prepare_backup_with(
                &self.paths.destination.0,
                &self.paths.backup.as_ref().unwrap().0,
                |_, _| Err(std::io::ErrorKind::Unsupported.into()),
                |path| {
                    self.backend.record("backup_copy_sync");
                    assert_eq!(
                        std::fs::read(path)?,
                        std::fs::read(&self.paths.destination.0)?
                    );
                    if self.backend.mode == TransactionMode::BackupSyncError {
                        return Err(std::io::Error::from_raw_os_error(123));
                    }
                    self.desktop.sync(path)
                },
                |id, copied| checked_register(mpmp_storage::FileIdentity(id.parts()), copied),
            )
            .map(|copied| mpmp_storage::BackupPreparation::Registered { copied });
        }
        self.desktop.prepare_backup(&mut checked_register)
    }
    fn publish(&self, preparation: mpmp_storage::BackupPreparation) -> std::io::Result<()> {
        self.backend.record("publish");
        #[cfg(unix)]
        {
            let conn = rusqlite::Connection::open(&self.backend.db).unwrap();
            let target: String = conn
                .query_row(
                    "SELECT target_json FROM file_write_intents ORDER BY id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let target: Target = parse(&target).unwrap();
            assert_eq!(
                preparation,
                mpmp_storage::BackupPreparation::Registered {
                    copied: target.backup.unwrap().copied
                }
            );
        }
        #[cfg(windows)]
        assert_eq!(preparation, mpmp_storage::BackupPreparation::DuringPublish);
        if self.backend.mode == TransactionMode::PublishBeforeError {
            return Err(std::io::Error::from_raw_os_error(123));
        }
        self.desktop.publish(preparation)?;
        if self.backend.mode == TransactionMode::PublishError {
            return Err(std::io::Error::from_raw_os_error(123));
        }
        Ok(())
    }
    fn sync(&self, path: &Path) -> std::io::Result<()> {
        if path == self.paths.destination.0 {
            let conn = rusqlite::Connection::open(&self.backend.db).unwrap();
            let stage: String = conn
                .query_row(
                    "SELECT stage FROM file_write_intents ORDER BY id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let candidates_present = [
                &self.paths.staged,
                &self.paths.backup,
                &self.paths.saved_after,
            ]
            .into_iter()
            .flatten()
            .any(|p| p.0.exists());
            if stage == "finalized" {
                let actions: i64 = conn
                    .query_row(
                        "SELECT count(*) FROM actions WHERE kind='cover_edit'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert!(actions > 0, "cleanup cannot precede the durable action");
            }
            self.backend.record(format!(
                "sync:live:{stage}:{}",
                if candidates_present {
                    "candidates"
                } else {
                    "clean"
                }
            ));
        } else {
            self.backend.record(format!("sync:{}", self.role(path)));
        }
        self.desktop.sync(path)?;
        if self.role(path) == "backup" && self.backend.mode == TransactionMode::SyncBackupError {
            return Err(std::io::Error::from_raw_os_error(123));
        }
        Ok(())
    }
    fn seal(&self) -> std::io::Result<()> {
        self.backend.record("seal");
        if self.backend.mode == TransactionMode::SealBeforeError {
            return Err(std::io::Error::from_raw_os_error(123));
        }
        self.desktop.seal()?;
        if self.backend.mode == TransactionMode::SealError {
            return Err(std::io::Error::from_raw_os_error(123));
        }
        Ok(())
    }
    fn rollback(&self) -> std::io::Result<()> {
        self.backend.record("rollback");
        self.desktop.rollback()
    }
    fn publish_missing(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
        self.backend.record("publish_missing");
        self.desktop.publish_missing(source, destination)
    }
    fn hidden(&self, path: &Path) -> std::io::Result<bool> {
        self.backend.record(format!("hidden:{}", self.role(path)));
        self.desktop.hidden(path)
    }
    fn set_hidden(&self, path: &Path, value: bool) -> std::io::Result<()> {
        self.backend
            .record(format!("set_hidden:{}", self.role(path)));
        self.desktop.set_hidden(path, value)
    }
    fn hide_candidate(&self, path: &Path) -> std::io::Result<()> {
        self.backend.record(format!("hide:{}", self.role(path)));
        self.desktop.hide_candidate(path)
    }
    fn verify_attributes(&self) -> std::io::Result<()> {
        self.backend.record("verify_attributes");
        self.desktop.verify_attributes()?;
        if self.backend.mode == TransactionMode::VerifyError {
            return Err(std::io::Error::from_raw_os_error(123));
        }
        Ok(())
    }
    fn move_missing(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
        self.backend.record("move_missing");
        self.desktop.move_missing(source, destination)
    }
}

fn recorded_fixture(mode: TransactionMode) -> (Fixture, Arc<TransactionRecorder>) {
    let Fixture { dir, lib, id, path } = Fixture::new();
    drop(lib);
    // TMPDIR can contain symlinks: use canonical paths for checked access.
    let db = dir.path().canonicalize().unwrap().join("library.db");
    let backend = Arc::new(TransactionRecorder {
        db: db.clone(),
        trace: Default::default(),
        mode,
    });
    let mut storage = mpmp_fs_desktop::DesktopStorage::backends();
    storage.replacements = Arc::new(RecordingReplacements(backend.clone()));
    let lib = Library::open_with(
        &db,
        crate::OpenOptions {
            storage: Some(storage),
            migrate: true,
        },
    )
    .unwrap();
    let trace = backend.clone();
    *lib.inner.hooks.cover_temp_sync.lock().unwrap() =
        Some(Box::new(move || trace.record("temp_sync")));
    (Fixture { dir, lib, id, path }, backend)
}

#[test]
fn transaction_trace_preserves_durable_order_and_errors() {
    for mode in [
        TransactionMode::Desktop,
        TransactionMode::Copy,
        TransactionMode::BackupSyncError,
        TransactionMode::PublishError,
        TransactionMode::PublishBeforeError,
        TransactionMode::SealError,
        TransactionMode::SealBeforeError,
        TransactionMode::SyncBackupError,
        TransactionMode::VerifyError,
        TransactionMode::AttributesError,
    ] {
        #[cfg(not(unix))]
        if matches!(
            mode,
            TransactionMode::Copy | TransactionMode::BackupSyncError
        ) {
            continue;
        }
        let (f, backend) = recorded_fixture(mode);
        let before = std::fs::read(&f.path).unwrap();
        let report = f.edit().unwrap();
        let mut expected = vec![
            "hidden:live",
            "hide:stage",
            "attributes:stage",
            "temp_sync",
            "open_source",
        ];
        if mode == TransactionMode::AttributesError {
            assert_eq!(report.failed, [(f.id, "write_failed")]);
            assert_eq!(
                backend.events(),
                ["hidden:live", "hide:stage", "attributes:stage"]
            );
            assert_eq!(std::fs::read(&f.path).unwrap(), before);
            let stage: String = f
                .lib
                .inner
                .pool
                .read(|conn| {
                    Ok(conn.query_row("SELECT stage FROM file_write_intents", [], |r| r.get(0))?)
                })
                .unwrap();
            assert_eq!(stage, "building");
            continue;
        }
        #[cfg(unix)]
        {
            expected.extend(["prepare_backup", "register"]);
        }
        if matches!(
            mode,
            TransactionMode::Copy | TransactionMode::BackupSyncError
        ) {
            expected.push("backup_copy_sync");
        }
        if mode != TransactionMode::BackupSyncError {
            expected.push("publish");
            if mode == TransactionMode::PublishBeforeError {
                expected.push("hide:stage");
            } else {
                expected.push("hide:backup");
            }
            if !matches!(
                mode,
                TransactionMode::PublishError | TransactionMode::PublishBeforeError
            ) {
                expected.push("sync:backup");
                if mode != TransactionMode::SyncBackupError {
                    expected.push("seal");
                    if !matches!(
                        mode,
                        TransactionMode::SealError | TransactionMode::SealBeforeError
                    ) {
                        expected.push("verify_attributes");
                    }
                }
            }
        }
        if matches!(
            mode,
            TransactionMode::PublishError
                | TransactionMode::SealError
                | TransactionMode::SealBeforeError
                | TransactionMode::SyncBackupError
                | TransactionMode::VerifyError
        ) {
            expected.extend([
                "attributes:after",
                "sync:after",
                "rollback",
                "set_hidden:live",
            ]);
        }
        if matches!(mode, TransactionMode::Desktop | TransactionMode::Copy) {
            assert_eq!(report.written, [f.id]);
            assert!(report.failed.is_empty());
            expected.extend([
                "sync:live:finalized:candidates",
                "sync:live:finalized:clean",
            ]);
        } else {
            assert_eq!(report.failed, [(f.id, "write_failed")]);
            assert!(report.written.is_empty());
            assert!(report.action.is_none());
            assert_eq!(std::fs::read(&f.path).unwrap(), before);
            expected.extend(["sync:live:prepared:candidates", "sync:live:prepared:clean"]);
        }
        assert_eq!(backend.events(), expected);
        assert!(targets(&f).is_empty());
    }
}

#[test]
fn injected_transactions_cover_recovery_undo_redo_and_isolate_libraries() {
    let (a, backend_a) = recorded_fixture(TransactionMode::Desktop);
    let (b, backend_b) = recorded_fixture(TransactionMode::Desktop);
    let edit = a.edit().unwrap();
    assert!(backend_b.events().is_empty());
    let events = backend_a.events();
    b.edit().unwrap();
    assert_eq!(backend_a.events(), events);
    let undo = a
        .lib
        .undo_cover_edit(edit.action.unwrap(), UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    a.lib
        .undo_cover_edit(undo.action.unwrap(), UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(
        backend_a
            .events()
            .iter()
            .filter(|e| e.as_str() == "publish")
            .count(),
        3
    );
    assert_eq!(
        backend_b
            .events()
            .iter()
            .filter(|e| e.as_str() == "publish")
            .count(),
        1
    );

    let before_crash = std::fs::read(&a.path).unwrap();
    *a.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_rename");
    assert!(
        a.lib
            .edit_cover(&[a.id], &CoverChange::Remove, UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .is_err()
    );
    let pending = targets(&a).pop().unwrap();
    std::fs::remove_file(&pending.path).unwrap();
    let Fixture { dir, lib, .. } = a;
    drop(lib);
    backend_a.trace.lock().unwrap().clear();
    let mut storage = mpmp_fs_desktop::DesktopStorage::backends();
    storage.replacements = Arc::new(RecordingReplacements(backend_a.clone()));
    let reopened = Library::open_with(
        &backend_a.db,
        crate::OpenOptions {
            storage: Some(storage),
            migrate: true,
        },
    )
    .unwrap();
    assert!(!reopened.has_cover_recovery_required());
    assert_eq!(std::fs::read(&pending.path).unwrap(), before_crash);
    assert_eq!(
        backend_a.events(),
        [
            "hide:backup",
            "publish_missing",
            "set_hidden:live",
            "sync:live:prepared:clean",
            "sync:live:prepared:clean"
        ]
    );
    drop(reopened);
    drop(dir);
}

#[test]
fn injected_transaction_is_used_for_explicit_recovery_and_cleanup() {
    let (f, backend) = recorded_fixture(TransactionMode::Desktop);
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(f.edit().is_err());
    std::fs::remove_file(&f.path).unwrap();
    let guard = f
        .lib
        .describe_cover_recovery_resolution(f.id, CoverRecoveryResolution::RestoreAfter)
        .unwrap();
    backend.trace.lock().unwrap().clear();
    f.lib.accept_cover_recovery(&guard, UI).unwrap();
    assert_eq!(backend.events(), ["publish_missing", "set_hidden:live"]);
    assert!(targets(&f).is_empty());

    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_stage_created");
    assert!(
        f.lib
            .edit_cover(&[f.id], &CoverChange::Remove, UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .is_err()
    );
    let guard = f
        .lib
        .describe_cover_recovery_resolution(f.id, CoverRecoveryResolution::CleanupCandidates)
        .unwrap();
    backend.trace.lock().unwrap().clear();
    f.lib.accept_cover_recovery(&guard, UI).unwrap();
    assert_eq!(backend.events(), ["sync:live:building:clean"]);
    assert!(targets(&f).is_empty());
}

// Consistent read-only capabilities for the globally unsupported backend case.
struct ReadOnlyStorage;
macro_rules! forward_read_only_files {
    ($(fn $name:ident($($argument:ident: $kind:ty),*) -> $result:ty;)+) => {
        $(fn $name(&self, $($argument: $kind),*) -> $result {
            mpmp_storage::LibraryStorage::$name(&mpmp_fs_desktop::DesktopStorage, $($argument),*)
        })+
    };
}

impl mpmp_storage::LibraryStorage for ReadOnlyStorage {
    forward_read_only_files! {
        fn canonical(path: &Path) -> std::io::Result<mpmp_storage::LocalPath>;
        fn strip_verbatim(path: PathBuf) -> PathBuf;
        fn comparable(path: &Path) -> mpmp_storage::LocalPath;
        fn case_insensitive_fs(path: &Path) -> Option<bool>;
        fn case_insensitive_inside(path: &Path) -> Option<bool>;
        fn same_dir(a: &Path, b: &Path) -> bool;
        fn overlap(a: &Path, b: &Path) -> bool;
        fn ensure_canonical(path: &Path) -> mpmp_storage::StorageResult<()>;
        fn open_regular(path: &Path,
         checked: bool) -> mpmp_storage::StorageResult<mpmp_storage::OpenedRegular>;

        fn entry_id(path: &Path) -> std::io::Result<mpmp_storage::EntryId>;
        fn entry_id_from_metadata(path: &Path,
         metadata: &std::fs::Metadata) -> std::io::Result<mpmp_storage::EntryId>;

        fn opened_id(file: &std::fs::File) -> std::io::Result<mpmp_storage::EntryId>;
        fn entry_identity(path: &Path,
         metadata: &std::fs::Metadata) -> std::io::Result<mpmp_storage::FileIdentity>;

        fn opened_identity(file: &std::fs::File) -> std::io::Result<mpmp_storage::FileIdentity>;
    }
    fn write_capabilities(&self) -> mpmp_storage::WriteCapabilities {
        mpmp_storage::WriteCapabilities {
            durable_replace: false,
            legacy_tag_write: false,
        }
    }
}

struct RefusingResume {
    refused: PathBuf,
    unsupported: bool,
    calls: std::sync::Mutex<Vec<PathBuf>>,
}

impl mpmp_storage::DurableReplace for RefusingResume {
    fn supported(&self) -> bool {
        !self.unsupported
    }
    fn begin(
        &self,
        paths: mpmp_storage::ReplacementPaths,
    ) -> mpmp_storage::StorageResult<Box<dyn mpmp_storage::ReplacementTransaction>> {
        if self.unsupported {
            Err(mpmp_storage::StorageError::Unsupported {
                operation: mpmp_storage::Operation::DurableReplace,
            })
        } else if paths.destination.0 != self.refused {
            mpmp_fs_desktop::DesktopStorage.begin(paths)
        } else {
            Err(mpmp_storage::StorageError::Io(
                std::io::Error::from_raw_os_error(123),
            ))
        }
    }
    fn resume(
        &self,
        paths: mpmp_storage::ReplacementPaths,
    ) -> mpmp_storage::StorageResult<Box<dyn mpmp_storage::ReplacementTransaction>> {
        self.calls.lock().unwrap().push(paths.destination.0.clone());
        self.begin(paths)
    }
}

fn intent_rows(lib: &Library) -> Vec<Vec<rusqlite::types::Value>> {
    lib.inner
        .pool
        .read(|conn| {
            let mut statement = conn.prepare("SELECT * FROM file_write_intents ORDER BY id")?;
            let columns = statement.column_count();
            Ok(statement
                .query_map([], |row| {
                    (0..columns).map(|column| row.get(column)).collect()
                })?
                .collect::<rusqlite::Result<_>>()?)
        })
        .unwrap()
}

#[test]
fn storage_io_errors_preserve_kind_raw_os_error_and_source() {
    let f = Fixture::new();
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(f.edit().is_err());
    let rows = intent_rows(&f.lib);
    let backend = Arc::new(RefusingResume {
        refused: f.path.clone(),
        unsupported: false,
        calls: Default::default(),
    });
    use mpmp_storage::DurableReplace;
    let error: LibraryError = backend
        .resume(replacement_paths(&targets(&f).pop().unwrap()))
        .err()
        .unwrap()
        .into();
    let LibraryError::Storage(mpmp_storage::StorageError::Io(actual)) = &error else {
        panic!("{error:?}")
    };
    let expected = std::io::Error::from_raw_os_error(123);
    assert_eq!(actual.kind(), expected.kind());
    assert_eq!(actual.raw_os_error(), expected.raw_os_error());
    assert_eq!(actual.to_string(), expected.to_string());
    use std::error::Error;
    assert_eq!(
        error
            .source()
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .raw_os_error(),
        Some(123)
    );
    let Fixture { dir, lib, id, .. } = f;
    drop(lib);
    let db = dir.path().canonicalize().unwrap().join("library.db");
    let mut backends = mpmp_fs_desktop::DesktopStorage::backends();
    backends.replacements = backend;
    let reopened = Library::open_with(
        &db,
        crate::OpenOptions {
            storage: Some(backends),
            migrate: true,
        },
    )
    .unwrap();
    assert!(reopened.has_cover_recovery_required());
    assert_eq!(reopened.cover_recovery_required().unwrap(), [id]);
    assert_eq!(intent_rows(&reopened), rows);
}

#[test]
fn unsupported_backend_preserves_pending_intents_on_open() {
    for stage in ["building", "retained", "finalized", "prepared"] {
        let f = Fixture::new();
        *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
        assert!(f.edit().is_err());
        let mut target = targets(&f).pop().unwrap();
        if matches!(stage, "building" | "retained") {
            // Nothing to hide or repair: these rows must be filtered without resume.
            target.staged.as_mut().unwrap().identity = [0, 0];
        }
        let saved = json(&target).unwrap();
        f.lib
            .inner
            .writer
            .run(move |conn| {
                conn.execute(
                    "UPDATE file_write_intents SET stage=?1,target_json=?2",
                    params![stage, saved],
                )?;
                Ok(())
            })
            .unwrap();
        let rows = intent_rows(&f.lib);
        let backend = Arc::new(RefusingResume {
            refused: f.path.clone(),
            unsupported: true,
            calls: Default::default(),
        });
        let Fixture { dir, lib, id, .. } = f;
        drop(lib);
        let db = dir.path().canonicalize().unwrap().join("library.db");
        let mut storage = mpmp_fs_desktop::DesktopStorage::backends();
        storage.files = Arc::new(ReadOnlyStorage);
        storage.replacements = backend.clone();
        let reopened = Library::open_with(
            &db,
            crate::OpenOptions {
                storage: Some(storage),
                migrate: true,
            },
        )
        .unwrap();
        assert!(reopened.has_cover_recovery_required());
        assert_eq!(reopened.cover_recovery_required().unwrap(), [id]);
        assert_eq!(intent_rows(&reopened), rows);
        assert_eq!(backend.calls.lock().unwrap().len(), 0);
    }
}

fn reopened_recording(backend: &Arc<TransactionRecorder>) -> Library {
    let mut storage = mpmp_fs_desktop::DesktopStorage::backends();
    storage.replacements = Arc::new(RecordingReplacements(backend.clone()));
    Library::open_with(
        &backend.db,
        crate::OpenOptions {
            storage: Some(storage),
            migrate: true,
        },
    )
    .unwrap()
}

fn completed_recovery_trace() -> Vec<&'static str> {
    vec![
        "hide:backup",
        "sync:live:prepared:candidates",
        "sync:backup",
        "seal",
        "verify_attributes",
        "sync:live:finalized:candidates",
        "sync:live:finalized:clean",
    ]
}

#[test]
fn recovery_after_trace_preserves_barriers_and_finalization() {
    let (f, backend) = recorded_fixture(TransactionMode::Desktop);
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_rename");
    assert!(f.edit().is_err());
    let after = std::fs::read(&f.path).unwrap();
    let Fixture { dir, lib, path, .. } = f;
    drop(lib);
    backend.trace.lock().unwrap().clear();
    let reopened = reopened_recording(&backend);
    assert_eq!(backend.events(), completed_recovery_trace());
    assert_eq!(std::fs::read(&path).unwrap(), after);
    assert!(!reopened.has_cover_recovery_required());
    assert!(intent_rows(&reopened).is_empty());
    let actions: i64 = reopened
        .inner
        .pool
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM actions WHERE kind='cover_edit'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(actions, 1);
    drop(reopened);
    backend.trace.lock().unwrap().clear();
    let reopened = reopened_recording(&backend);
    assert!(backend.events().is_empty());
    assert!(!reopened.has_cover_recovery_required());
    drop(reopened);
    drop(dir);
}

#[test]
fn recovery_error_traces_preserve_prepared_intent_until_retry() {
    for mode in [
        TransactionMode::SyncBackupError,
        TransactionMode::VerifyError,
        TransactionMode::SealBeforeError,
    ] {
        let (f, original) = recorded_fixture(TransactionMode::Desktop);
        *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_rename");
        assert!(f.edit().is_err());
        let rows = intent_rows(&f.lib);
        let target = targets(&f).pop().unwrap();
        let after = std::fs::read(&f.path).unwrap();
        let before = std::fs::read(&target.backup.as_ref().unwrap().path).unwrap();
        let Fixture { dir, lib, path, .. } = f;
        drop(lib);
        let backend = Arc::new(TransactionRecorder {
            db: original.db.clone(),
            trace: Default::default(),
            mode,
        });
        let reopened = reopened_recording(&backend);
        let mut expected = vec![
            "hide:backup",
            "sync:live:prepared:candidates",
            "sync:backup",
        ];
        if mode != TransactionMode::SyncBackupError {
            expected.push("seal");
        }
        if mode == TransactionMode::VerifyError {
            expected.push("verify_attributes");
        }
        assert_eq!(backend.events(), expected);
        assert!(reopened.has_cover_recovery_required());
        assert_eq!(intent_rows(&reopened), rows);
        assert_eq!(std::fs::read(&path).unwrap(), after);
        assert_eq!(std::fs::read(&target.backup.unwrap().path).unwrap(), before);
        let actions: i64 = reopened
            .inner
            .pool
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM actions WHERE kind='cover_edit'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(actions, 0);
        drop(reopened);
        original.trace.lock().unwrap().clear();
        let retried = reopened_recording(&original);
        assert_eq!(original.events(), completed_recovery_trace());
        assert!(!retried.has_cover_recovery_required());
        drop(retried);
        drop(dir);
    }
}

#[test]
fn resumed_transactions_ignore_run_only_seal_fault() {
    let (f, backend) = recorded_fixture(TransactionMode::Desktop);
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_rename");
    assert!(f.edit().is_err());
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("seal_interrupted");
    backend.trace.lock().unwrap().clear();
    recover_inner(&f.lib.inner).unwrap();
    assert_eq!(backend.events(), completed_recovery_trace());
    assert_eq!(f.cover_actions(), 1);
    assert!(targets(&f).is_empty());
    assert!(!f.lib.has_cover_recovery_required());
}

#[test]
fn transaction_trace_distinguishes_all_registered_path_roles() {
    let (f, backend) = recorded_fixture(TransactionMode::Desktop);
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(f.edit().is_err());
    let target = targets(&f).pop().unwrap();
    let staged = &target.staged.as_ref().unwrap().path;
    let backup = &target.backup.as_ref().unwrap().path;
    let after = &target.rollback_after.as_ref().unwrap().path;
    std::fs::copy(&f.path, backup).unwrap();
    std::fs::copy(staged, after).unwrap();
    let transaction = f
        .lib
        .inner
        .storage
        .replacements
        .resume(replacement_paths(&target))
        .unwrap();
    backend.trace.lock().unwrap().clear();
    for path in [&target.path, staged, backup, after] {
        transaction.hidden(path).unwrap();
    }
    for path in [staged, backup, after] {
        transaction.hide_candidate(path).unwrap();
    }
    transaction.sync(&target.path).unwrap();
    transaction.sync(staged).unwrap();
    transaction.sync(backup).unwrap();
    transaction.sync(after).unwrap();
    assert_eq!(
        backend.events(),
        [
            "hidden:live",
            "hidden:stage",
            "hidden:backup",
            "hidden:after",
            "hide:stage",
            "hide:backup",
            "hide:after",
            "sync:live:prepared:candidates",
            "sync:stage",
            "sync:backup",
            "sync:after"
        ]
    );
}

#[test]
fn resume_failure_does_not_block_a_later_healthy_intent() {
    let f = Fixture::new();
    let other_path = f.path.with_file_name("b.mp3");
    tagged_copy("sine44_a.mp3", &other_path);
    scan(&f);
    let other = f
        .lib
        .tracks_query(
            &crate::TrackQuery::default(),
            crate::Page {
                offset: 0,
                limit: 10,
            },
            &[],
        )
        .unwrap()
        .0
        .into_iter()
        .find(|track| track.track.id != f.id)
        .unwrap()
        .track
        .id;
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("before_action");
    assert!(
        f.lib
            .edit_cover(&[f.id, other], &f.change(), UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .is_err()
    );
    let rows = intent_rows(&f.lib);
    assert_eq!(rows.len(), 2);
    let backend = Arc::new(RefusingResume {
        refused: f.path.clone(),
        unsupported: false,
        calls: Default::default(),
    });
    let Fixture { dir, lib, id, .. } = f;
    drop(lib);
    let mut storage = mpmp_fs_desktop::DesktopStorage::backends();
    storage.replacements = backend.clone();
    let reopened = Library::open_with(
        &dir.path().canonicalize().unwrap().join("library.db"),
        crate::OpenOptions {
            storage: Some(storage),
            migrate: true,
        },
    )
    .unwrap();
    assert_eq!(intent_rows(&reopened), rows[..1]);
    assert_eq!(reopened.cover_recovery_required().unwrap(), [id]);
    assert!(backend.calls.lock().unwrap().contains(&other_path));
}

#[test]
fn filtered_rows_preserve_desktop_attribute_maintenance() {
    for stage in ["building", "retained"] {
        let (f, backend) = recorded_fixture(TransactionMode::Desktop);
        *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
        assert!(f.edit().is_err());
        let mut target = targets(&f).pop().unwrap();
        target.baseline.as_mut().unwrap().hidden = None;
        let saved = json(&target).unwrap();
        f.lib
            .inner
            .writer
            .run(move |conn| {
                conn.execute(
                    "UPDATE file_write_intents SET stage=?1,target_json=?2",
                    params![stage, saved],
                )?;
                Ok(())
            })
            .unwrap();
        backend.trace.lock().unwrap().clear();
        recover_inner(&f.lib.inner).unwrap();
        assert_eq!(backend.events(), ["hidden:live", "hide:stage"]);
        assert_eq!(targets(&f).len(), 1);
        assert_eq!(
            targets(&f)[0].baseline.as_ref().unwrap().hidden,
            Some(false)
        );
        assert!(f.lib.has_cover_recovery_required());
    }
}
