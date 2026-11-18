//! Provides the library handle, database access, and file-job entry points.
//!
//! Cloned handles share a writer and read pool. Synchronous calls wait for their database
//! operation; scan and edit entry points return job handles for work on worker threads.

use std::path::{Path, PathBuf};

use std::sync::{Arc, OnceLock};

use std::sync::Mutex;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use mpmp_media_io::MediaUri;

use crate::cover::thumbnail::ThumbnailCache;

use crate::cover::{CoverArt, read_cover};

use crate::db::pool::ReadPool;

use crate::db::writer::{self, WriteCmd, Writer, now_ms};

use crate::journal::{ActionId, ActionMeta, ActionSummary, Done, UndoEffect};

use crate::lyrics::Lyrics;

use crate::lyrics::find::read_lyrics;

use crate::playlist::{
    self, M3uReport, PlaylistEntry, PlaylistId, PlaylistItemId, PlaylistKind, PlaylistSummary,
    SmartCounts,
};

use crate::ratings::{self, Field as UserField};

use crate::scan::{self, ScanStart, ScanState};

use crate::storage_access;

use crate::tags::edit::TagChange;

use crate::tags::encoding::encoding_for_label;

use crate::tags::job::{self as tag_job, TagJob, TagJobOptions, TagPreview};

use crate::text::SearchQuery;

use crate::track_query::{self, FacetValue, Field, Filter, TrackQuery, TrackRow};

use crate::types::{
    Album, AlbumKey, AlbumSort, AlbumSummary, ArtistSummary, HistoryEntry, Page, Paged, PlayEvent,
    Root, RootId, ScanProgress, SearchResults, Track, TrackId,
};

use crate::waveform::{FileVersion, Waveform, WaveformCache};

use crate::{Damage, Invalid, LibraryError, NotFound, Result, db::schema, playlist::m3u};

/// Storage selection and schema migration policy for [`Library::open_with`].
///
/// Defaults to the desktop backend and automatic migration. Without `desktop-fs`,
/// callers must supply `storage`; a missing backend fails before any I/O.
#[derive(Clone)]
pub struct OpenOptions {
    /// Per-instance file storage. `None` selects the default desktop backend.
    pub storage: Option<mpmp_storage::StorageBackends>,
    /// Whether baseline development v13 may be backed up and relabeled. New databases
    /// are always initialized; development 1–12 cannot be migrated by this build.
    pub migrate: bool,
}

impl OpenOptions {
    /// Uses explicit storage with automatic migration enabled.
    pub fn with_storage(storage: mpmp_storage::StorageBackends) -> Self {
        Self {
            storage: Some(storage),
            ..Self::default()
        }
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            storage: None,
            migrate: true,
        }
    }
}

/// The music library. Cheap to clone; all clones share one writer thread and read pool, which
/// stop on explicit close or when the last clone (and any running scan) is gone.
#[derive(Clone)]
pub struct Library {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    closing: Arc<AtomicBool>,
    closed: OnceLock<Result<(), Arc<LibraryError>>>,
    file_jobs: Arc<file_jobs::FileJobs>,
    close_progress: Arc<Mutex<crate::CloseProgress>>,
    pub storage: mpmp_storage::StorageBackends,
    pub pool: ReadPool,
    pub writer: Writer,
    /// Shared by the writer and the pool: whether the database was found damaged.
    pub damage: Damage,
    pub smart_counts: SmartCounts,
    pub scan_state: Mutex<ScanState>,
    /// Observed whole-file CUE supplements and their confirmation by the writer.
    pub cue_supplements: Mutex<std::collections::HashMap<(RootId, String), scan::SupplementStamp>>,
    /// Resolved sidecar ownership, including first imports without any database CUE rows yet.
    pub cue_dependencies:
        Mutex<std::collections::BTreeMap<PathBuf, std::collections::BTreeSet<PathBuf>>>,
    pub file_access: Arc<crate::lease::FileAccess>,
    /// Cover thumbnails, in a `covers` folder next to the database.
    pub thumbnails: ThumbnailCache,
    /// Track waveforms, in a `waveforms` folder next to the database.
    pub waveforms: WaveformCache,
    /// Held by the running tag job: they run one at a time.
    pub tag_jobs: Mutex<()>,
    /// Invalidates directory enumerations across a file replacement or restoration.
    pub file_namespace_epoch: AtomicU64,
    pub recovered_covers: Mutex<Vec<TrackId>>,
    pub cover_recovery: std::sync::atomic::AtomicBool,
    #[cfg(all(test, feature = "desktop-fs"))]
    pub hooks: Arc<TestHooks>,
}

impl Inner {
    pub(crate) fn register_file_job(&self) -> Result<file_jobs::FileJob> {
        self.file_jobs.register()
    }

    pub(crate) fn check_open(&self) -> Result<()> {
        if self.closing.load(Ordering::Acquire) {
            return Err(LibraryError::Closed);
        }
        Ok(())
    }

    fn close(&self) -> Result<(), Arc<LibraryError>> {
        if self.file_jobs.active_here() || self.writer.on_writer_thread() {
            return Err(Arc::new(LibraryError::ReentrantClose));
        }
        self.closed
            .get_or_init(|| {
                use crate::CloseStage;
                self.file_jobs
                    .seal(|| {
                        self.writer.begin_shutdown();
                        Ok(())
                    })
                    .map_err(Arc::new)?;
                let file_failure = self.file_jobs.wait();
                self.close_progress
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .file_jobs = file_failure
                    .clone()
                    .map_or(CloseStage::Succeeded, CloseStage::Failed);
                self.closing.store(true, Ordering::Release);
                let readers = self.pool.close();
                self.close_progress
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .read_connections = CloseStage::from_result(&readers);
                let writer = self.writer.close();
                let database = readers.and(writer);
                if let Some(details) = file_failure {
                    return Err(Arc::new(LibraryError::FileJobShutdown {
                        details,
                        database: database.err().map(Box::new),
                    }));
                }
                database.map_err(Arc::new)
            })
            .clone()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if self.closed.get().is_none()
            && let Err(error) = self.close()
        {
            tracing::warn!(%error, "library close failed during drop");
        }
    }
}

#[cfg(all(test, feature = "desktop-fs"))]
type ScanFileHook = dyn Fn(RootId, &str) + Send + Sync;

#[cfg(all(test, feature = "desktop-fs"))]
type ReplaceHook =
    dyn Fn(&std::path::Path, &std::path::Path, &std::path::Path) -> std::io::Result<()> + Send;

#[cfg(all(test, feature = "desktop-fs"))]
type ExportHook = Box<dyn FnOnce() -> Result<()> + Send>;

#[cfg(all(test, feature = "desktop-fs"))]
type TagPublishHook = dyn Fn(&Path, &Path) + Send;

#[cfg(all(test, feature = "desktop-fs"))]
type TagPlanHook = dyn Fn(&Path) + Send;

/// Fault and timing injection for tests.
#[cfg(all(test, feature = "desktop-fs"))]
#[derive(Default)]
pub(crate) struct TestHooks {
    pub after_m3u_publish: Mutex<Option<ExportHook>>,
    pub tag_fault: Mutex<Option<&'static str>>,
    pub tag_before_publish: Mutex<Option<Box<TagPublishHook>>>,
    pub tag_after_plan: Mutex<Option<Box<TagPlanHook>>>,
    pub tag_after_publish: Mutex<Option<Box<TagPublishHook>>>,
    pub tag_before_cleanup: Mutex<Option<Box<TagPlanHook>>>,
    pub tag_before_rollback: Mutex<Option<Box<TagPublishHook>>>,
    pub tag_publish_mode: Mutex<Option<mpmp_storage::TagPublish>>,
    pub tag_exclusive_gap: Mutex<Option<Box<TagPlanHook>>>,
    pub cover_fault: Mutex<Option<&'static str>>,
    pub cover_temp_sync: Mutex<Option<Box<dyn Fn() + Send>>>,
    pub cover_abort: std::sync::atomic::AtomicBool,
    pub cover_before_replace: Mutex<Option<Box<dyn Fn() + Send>>>,
    pub cover_replace: Mutex<Option<Box<ReplaceHook>>>,
    pub fail_spawn: std::sync::atomic::AtomicBool,
    pub panic_audio_reader: Mutex<Option<PathBuf>>,
    pub fail_audio_read: Mutex<Option<PathBuf>>,
    pub fail_metadata: Mutex<Option<PathBuf>>,
    pub fail_walk: Mutex<Option<PathBuf>>,
    pub fail_walk_after: Mutex<Option<PathBuf>>,
    pub scan_file: Mutex<Option<Arc<ScanFileHook>>>,
    /// Runs after a successful audio read in the whole-file scan loop.
    pub after_scan_audio_read: Mutex<Option<Arc<ScanFileHook>>>,
    pub scan_before_walk: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    pub scan_after_walk: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Runs between the statements of a multi-statement read.
    pub between_reads: Mutex<Option<Box<dyn Fn() + Send>>>,
}

#[cfg(all(test, feature = "desktop-fs"))]
impl Inner {
    fn between_reads(&self) {
        if let Some(hook) = &*self.hooks.between_reads.lock().unwrap() {
            hook();
        }
    }
}

impl Library {
    /// File writes supported by this instance's validated storage backends.
    pub fn storage_write_capabilities(&self) -> mpmp_storage::WriteCapabilities {
        let mut capabilities = self.inner.storage.files.write_capabilities();
        capabilities.legacy_tag_write &= self.inner.storage.replacements.provides_tag_publish();
        capabilities
    }

    pub(crate) fn require_cover_write(&self) -> Result<()> {
        if !self.storage_write_capabilities().durable_replace {
            return Err(LibraryError::CoverInput("cover_edit_unsupported"));
        }
        Ok(())
    }

    /// Opens or creates the database (including parent folders), recognizing published
    /// schema 1 or backing up and relabeling baseline development v13 with the desktop backend. Without `desktop-fs`, use
    /// [`Self::open_with`] to supply storage explicitly.
    pub fn open(db_path: &Path) -> Result<Library> {
        Self::open_with(db_path, OpenOptions::default())
    }

    /// Opens a library with explicit storage and migration options.
    ///
    /// With `migrate: false`, an existing database must already be published schema 1;
    /// a missing database is initialized normally. Development 1–12 and early v13 variants
    /// are always refused. Storage is validated before database access.
    pub fn open_with(db_path: &Path, options: OpenOptions) -> Result<Library> {
        let storage = match options.storage {
            Some(storage) => storage,
            None => {
                #[cfg(feature = "desktop-fs")]
                {
                    mpmp_fs_desktop::DesktopStorage::backends()
                }
                #[cfg(not(feature = "desktop-fs"))]
                {
                    return Err(mpmp_storage::StorageError::Configuration(
                        "missing storage backend; use Library::open_with and OpenOptions::with_storage",
                    )
                    .into());
                }
            }
        };
        Self::validate_storage(&storage)?;
        #[cfg(all(test, feature = "desktop-fs"))]
        let hooks = Arc::new(TestHooks::default());
        #[cfg(all(test, feature = "desktop-fs"))]
        let storage = mpmp_storage::StorageBackends {
            replacements: Arc::new(crate::tests::storage_adapter::HookedReplacements {
                backend: storage.replacements.clone(),
                hooks: hooks.clone(),
            }),
            ..storage
        };
        if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let data_version = Arc::new(AtomicU64::new(0));
        let damage = Damage::default();
        let closing = Arc::new(AtomicBool::new(false));
        let close_progress = Arc::new(Mutex::new(crate::CloseProgress::default()));
        let writer = Writer::start(
            db_path,
            data_version.clone(),
            damage.clone(),
            closing.clone(),
            close_progress.clone(),
            &storage,
            options.migrate,
        )
        .map_err(crate::db::open_error)?;
        let library = Library {
            inner: Arc::new(Inner {
                storage,
                writer,
                pool: ReadPool::new(db_path, damage.clone(), closing.clone()),
                closing,
                closed: OnceLock::new(),
                file_jobs: Arc::default(),
                close_progress,
                damage,
                smart_counts: SmartCounts::new(data_version),
                scan_state: Mutex::default(),
                cue_dependencies: Mutex::default(),
                cue_supplements: Mutex::default(),
                file_access: Arc::default(),
                thumbnails: ThumbnailCache::new(db_path.with_file_name("covers")),
                waveforms: WaveformCache::new(db_path.with_file_name("waveforms")),
                tag_jobs: Mutex::new(()),
                file_namespace_epoch: AtomicU64::new(0),
                recovered_covers: Mutex::default(),
                cover_recovery: std::sync::atomic::AtomicBool::new(false),
                #[cfg(all(test, feature = "desktop-fs"))]
                hooks,
            }),
        };
        crate::tags::job::intents::recover_inner(&library.inner)?;
        crate::tags::job::intents::clean_interrupted_probes(&library.inner);
        crate::cover::job::refresh_recovery_flag(&library.inner);
        Ok(library)
    }

    fn validate_storage(storage: &mpmp_storage::StorageBackends) -> Result<()> {
        let capabilities = storage.files.write_capabilities();
        if capabilities.durable_replace != storage.replacements.supported()
            || (capabilities.legacy_tag_write && !capabilities.durable_replace)
        {
            return Err(mpmp_storage::StorageError::Configuration(
                "inconsistent storage write capabilities",
            )
            .into());
        }
        Ok(())
    }

    /// Runs the exact database recognition checks through a read-only connection.
    /// Does not recover hot journals or create locks, backups, receipts or schemas.
    /// Stop writers first; SQLite may require its own recovery before recognition.
    pub fn inspect_schema(path: &Path) -> Result<&'static str> {
        schema::inspect(path).map_err(crate::db::open_error)
    }

    /// Paths of pending or retained tag-write intents for the trusted Diagnostics window.
    pub fn tag_recovery_diagnostics(&self) -> Result<Vec<serde_json::Value>> {
        self.inner.pool.read(crate::tags::job::intents::diagnostics)
    }

    /// Relabels a stopped, disposable development-v13 copy using production backup
    /// and transaction safeguards, without WAL setup, optimize, recovery or pruning.
    /// Use only for acceptance evidence, before opening a normal Library session.
    pub fn relabel_schema_copy(
        db_path: &Path,
        storage: &mpmp_storage::StorageBackends,
    ) -> Result<()> {
        Self::validate_storage(storage)?;
        schema::relabel_copy(db_path, storage).map_err(crate::db::open_error)
    }

    /// Read and consume the durable schema reset diagnostic, only on a trusted host page.
    /// Historical development notices are read through [`Self::historical_upgrade_notice`].
    pub fn take_upgrade_notice(data_directory: &Path) -> std::io::Result<Option<String>> {
        if !data_directory.exists() {
            return Ok(None);
        }
        let _notice_lock = schema::notice_lock(data_directory)?;
        let path = data_directory.join(schema::RESET_NOTICE);
        match std::fs::read_to_string(&path) {
            Ok(receipt) => {
                let backup = schema::notice_backup(&receipt)?;
                std::fs::remove_file(path)?;
                Ok(Some(backup))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Reads a development upgrade notice as history, without consuming or promoting it.
    /// Only trusted diagnostic pages may display its backup path.
    pub fn historical_upgrade_notice(data_directory: &Path) -> std::io::Result<Option<String>> {
        match std::fs::read_to_string(data_directory.join("library-v13-upgrade-notice")) {
            Ok(path) => Ok(Some(path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whether files should be re-read: a migration or a root's encoding change marked them
    /// so. True from then until the next completed scan (also across restarts).
    pub fn rescan_recommended(&self) -> Result<bool> {
        self.inner
            .pool
            .read(|conn| Ok(schema::rescan_request(conn)?.is_some()))
    }

    /// Whether visible tracks still need a successful current-version person read.
    /// This is independent of the legacy full-rescan request flag.
    pub fn artists_indexing(&self) -> Result<bool> {
        self.inner.pool.read(crate::person::indexing)
    }

    /// Why the database is damaged, if any read or write so far found it so (SQLite: corrupt,
    /// or not a database; [`LibraryError::is_damaged`]), also one whose failure was only
    /// logged (a play event, a scan batch). Stays set until the library is reopened; the
    /// library never deletes or rebuilds the database itself.
    pub fn damaged(&self) -> Option<String> {
        self.inner.damage.get().map(str::to_owned)
    }

    /// Waits until every write queued so far (e.g. [`Library::record_play`]) is committed.
    pub fn flush(&self) -> Result<()> {
        self.inner.writer.call(|reply| WriteCmd::Flush { reply })
    }

    /// Seals file-job admission and waits for registered jobs to finish their commits.
    /// Then seals database work on all clones, waits for readers and queued writes, and
    /// closes the writer after a WAL checkpoint. Concurrent and repeated callers receive
    /// the same result, including asynchronous write failures and errors during the drain.
    /// A callback from this library's file job receives `ReentrantClose`. It can call
    /// [`Self::request_close`] and return so the job is free to finish.
    pub fn close(&self) -> Result<(), Arc<LibraryError>> {
        self.inner.close()
    }

    /// Seals file-job admission and starts background shutdown without waiting.
    /// Call [`Self::close`] outside a file job to collect the final result.
    pub fn request_close(&self) -> Result<()> {
        self.inner.file_jobs.seal(|| {
            let library = self.clone();
            std::thread::Builder::new()
                .name("library-close".into())
                .spawn(move || {
                    if let Err(error) = library.close() {
                        tracing::warn!(%error, "background library close reported errors");
                    }
                })?;
            self.inner.writer.begin_shutdown();
            Ok(())
        })
    }

    /// Returns the observed stage results without waiting for any shutdown work.
    pub fn close_progress(&self) -> crate::CloseProgress {
        self.inner
            .close_progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub(crate) fn inner(&self) -> &Inner {
        &self.inner
    }
}

pub(crate) struct NamespaceChange<'a>(&'a AtomicU64);

impl Inner {
    pub(crate) fn changing_file_names(&self) -> NamespaceChange<'_> {
        self.file_namespace_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        NamespaceChange(&self.file_namespace_epoch)
    }
}

impl Drop for NamespaceChange<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::*;
    use crate::tests::fixtures::{fixture, id_of, meta, record, upsert};
    use mpmp_storage::LibraryStorage;
    use std::time::{Duration, Instant};

    fn track_for_close(fixture: &crate::tests::fixtures::Fixture) -> TrackId {
        upsert(
            &fixture.lib,
            vec![record(fixture.root, "close.flac", meta("Close", "", ""))],
        );
        id_of(&fixture.lib, "Close")
    }

    fn close_play_event(track_id: TrackId) -> PlayEvent {
        PlayEvent {
            track_id,
            context: None,
            started_at: 1_799_999_000_125,
            played_ms: 1_000,
            ended_reason: crate::EndedReason::Completed,
            origin: "ui".into(),
        }
    }

    #[test]
    fn close_rejects_new_work_waits_for_readers_and_drains_play_events() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let track_id = track_for_close(&fixture);
        let db = lib.inner.pool.path();
        let wal = db.with_file_name("library.sqlite-wal");
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);
        let event = close_play_event(track_id);
        std::thread::scope(|scope| {
            let reader = lib.inner.pool.connection().unwrap();
            let idle = lib.inner.pool.connection().unwrap();
            let snapshot = reader.unchecked_transaction().unwrap();
            let reader_count = || -> i64 {
                snapshot
                    .query_row("SELECT count(*) FROM play_events", [], |row| row.get(0))
                    .unwrap()
            };
            assert_eq!(reader_count(), 0);
            drop(idle);
            let (release_writer, blocked) = std::sync::mpsc::channel();
            let (writer_started, started) = std::sync::mpsc::channel();
            let (write_drained, drained) = std::sync::mpsc::channel();
            let (checkpoint_started, checkpoint) = std::sync::mpsc::channel();
            *lib.inner.writer.before_checkpoint.lock().unwrap() = Some(checkpoint_started);
            lib.inner
                .writer
                .send(WriteCmd::Run(Box::new(move |_, _| {
                    writer_started.send(()).unwrap();
                    let _ = blocked.recv();
                })))
                .unwrap();
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            lib.record_play(event.clone()).unwrap();
            lib.inner
                .writer
                .send(WriteCmd::Run(Box::new(move |_, _| {
                    write_drained.send(()).unwrap();
                })))
                .unwrap();
            let (done, result) = std::sync::mpsc::channel();
            scope.spawn(move || done.send(lib.close()).unwrap());
            let deadline = Instant::now() + Duration::from_secs(5);
            while !lib.inner.closing.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            let rejected_read = lib.track(track_id);
            let rejected_write = lib.set_agent_seen("agent", 123);
            let rejected_play = lib.record_play(close_play_event(track_id));
            let rejected_scan = lib.scan(|_| {});
            release_writer.send(()).unwrap();
            drained.recv_timeout(Duration::from_secs(5)).unwrap();
            let premature = checkpoint.recv_timeout(Duration::from_millis(40));
            let count_in_snapshot = reader_count();
            drop(snapshot);
            drop(reader);
            assert!(matches!(rejected_read, Err(LibraryError::Closed)));
            assert!(matches!(rejected_write, Err(LibraryError::Closed)));
            assert!(matches!(rejected_play, Err(LibraryError::Closed)));
            assert!(matches!(rejected_scan, Err(LibraryError::Closed)));
            assert!(matches!(
                premature,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            assert_eq!(count_in_snapshot, 0);
            checkpoint.recv_timeout(Duration::from_secs(5)).unwrap();
            result
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
        });

        lib.close().unwrap();
        assert!(!wal.exists());
        assert!(!db.with_file_name("library.sqlite-shm").exists());
        let copy = tempfile::tempdir().unwrap();
        let copied_db = copy.path().join("library.db");
        std::fs::copy(db, &copied_db).unwrap();
        let recovered = Library::open(&copied_db).unwrap();
        let history = recovered.history(Page::new(0, 10)).unwrap();
        assert_eq!(history.total, 1);
        assert_eq!(history.items[0].event, event);
        let row = &recovered.track_rows(&[track_id], &[]).unwrap()[0];
        assert_eq!(
            (row.play_count, row.last_played_at),
            (1, Some(event.started_at))
        );
    }

    #[test]
    fn close_reports_a_failed_last_play_event() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let track = track_for_close(&fixture);
        lib.inner
            .writer
            .run(|conn| {
                conn.execute_batch(
                    "CREATE TEMP TRIGGER fail_last_play BEFORE INSERT ON play_events
                 WHEN NEW.origin = 'fail-on-close' BEGIN
                    SELECT RAISE(ABORT, 'injected final play failure');
                 END;",
                )?;
                Ok(())
            })
            .unwrap();
        let good = close_play_event(track);
        lib.record_play(good.clone()).unwrap();
        let mut bad = close_play_event(track);
        bad.origin = "fail-on-close".into();
        lib.record_play(bad).unwrap();
        let error = lib.close().unwrap_err();
        assert!(
            matches!(
                &*error,
                LibraryError::WriteShutdown { count: 1, first, database: None }
                    if first.contains("injected final play failure")
            ),
            "{error}"
        );
        assert!(Arc::ptr_eq(&error, &lib.close().unwrap_err()));
        let progress = lib.close_progress();
        assert!(matches!(progress.writes, crate::CloseStage::Failed(_)));
        assert_eq!(progress.checkpoint, crate::CloseStage::Succeeded);
        assert_eq!(progress.read_connections, crate::CloseStage::Succeeded);
        assert_eq!(progress.write_connection, crate::CloseStage::Succeeded);
        let db = lib.inner.pool.path();
        assert!(!db.with_file_name("library.sqlite-wal").exists());
        let reopened = Library::open(db).unwrap();
        let history = reopened.history(Page::new(0, 10)).unwrap();
        assert_eq!(history.total, 1);
        assert_eq!(history.items[0].event, good);
    }

    #[test]
    fn a_tag_callback_can_request_close_but_cannot_wait_for_it() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let path = fixture._dir.path().join("music/close.flac");
        std::fs::write(
            &path,
            include_bytes!("../../engine/tests/fixtures/sine44_a.flac"),
        )
        .unwrap();
        let ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() else {
            panic!("fixture scan already running");
        };
        scan.wait().unwrap();
        let track = lib.track_ids(&TrackQuery::default(), 1).unwrap()[0];
        let callback = lib.clone();
        let (finished, observed) = std::sync::mpsc::channel();
        let options = TagJobOptions {
            progress: Box::new(move |done, total| {
                if done == total {
                    let before = callback.close().unwrap_err();
                    callback.request_close().unwrap();
                    // The running job keeps shutdown in progress after admission closes.
                    assert!(matches!(
                        callback.inner.register_file_job(),
                        Err(LibraryError::Closed)
                    ));
                    let during = callback.close().unwrap_err();
                    finished.send((before, during)).unwrap();
                }
            }),
            ..Default::default()
        };
        let job = lib
            .edit_tags(
                &[track],
                &[TagChange::Set {
                    key: "TITLE".into(),
                    values: vec!["Edited before close".into()],
                }],
                ActionMeta::new("ui"),
                options,
            )
            .unwrap();
        let (before, during) = observed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(&*before, LibraryError::ReentrantClose));
        assert!(matches!(&*during, LibraryError::ReentrantClose));
        assert!(job.wait().unwrap().action.is_some());
        lib.close().unwrap();
        let reopened = Library::open(lib.inner.pool.path()).unwrap();
        assert_eq!(
            reopened.track(track).unwrap().unwrap().title.as_deref(),
            Some("Edited before close")
        );
    }

    #[test]
    fn close_reports_m3u_failure_after_publish_before_the_audit_is_queued() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let meta = ActionMeta::new("ui");
        let playlist = lib
            .create_playlist("Export", PlaylistKind::Manual, meta)
            .unwrap()
            .value;
        let path = fixture._dir.path().join("export.m3u");
        std::thread::scope(|scope| {
            let (published, published_rx) = std::sync::mpsc::channel();
            let (continue_tx, continue_rx) = std::sync::mpsc::channel();
            *lib.inner.hooks.after_m3u_publish.lock().unwrap() = Some(Box::new(move || {
                published.send(()).unwrap();
                continue_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                Err(std::io::Error::other("failed before M3U audit").into())
            }));
            let export = scope.spawn(|| lib.export_m3u(playlist, &path, false, true, meta));
            published_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(path.is_file());
            lib.request_close().unwrap();
            let (done, result) = std::sync::mpsc::channel();
            scope.spawn(move || done.send(lib.close()).unwrap());
            let premature = result.recv_timeout(Duration::from_millis(40));
            continue_tx.send(()).unwrap();
            assert!(matches!(
                premature,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            assert!(matches!(export.join().unwrap(), Err(LibraryError::Io(_))));
            let failure = result
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap_err();
            assert!(matches!(
                &*failure,
                LibraryError::FileJobShutdown { details, database: None }
                    if details.contains("failed before M3U audit")
            ));
            let progress = lib.close_progress();
            assert!(matches!(progress.file_jobs, crate::CloseStage::Failed(_)));
            assert_eq!(progress.writes, crate::CloseStage::Succeeded);
            assert_eq!(progress.checkpoint, crate::CloseStage::Succeeded);
            let reopened = Library::open(lib.inner.pool.path()).unwrap();
            let actions = reopened.actions(None, Page::new(0, 10)).unwrap();
            assert!(
                actions
                    .items
                    .iter()
                    .all(|action| action.kind != "file_write")
            );
        });
    }

    #[test]
    fn close_reports_a_replied_write_failure_during_the_file_job_drain() {
        let fixture = fixture();
        let lib = &fixture.lib;
        std::thread::scope(|scope| {
            let active = lib.inner.register_file_job().unwrap();
            let (done, result) = std::sync::mpsc::channel();
            scope.spawn(move || done.send(lib.close()).unwrap());
            let deadline = Instant::now() + Duration::from_secs(5);
            while lib.inner.register_file_job().is_ok() {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            let write = lib.inner.writer.run(|conn| {
                // SQLite rejects this statement while the existing file-job permit
                // keeps the writer open.
                conn.execute("INSERT INTO no_such_table VALUES (1)", [])?;
                Ok(())
            });
            drop(active);
            assert!(matches!(write, Err(LibraryError::Db(_))));
            let error = result
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap_err();
            assert!(
                matches!(
                    &*error,
                    LibraryError::WriteShutdown { count: 1, first, database: None }
                        if first.contains("no_such_table")
                ),
                "{error}"
            );
        });
    }

    #[test]
    fn close_waits_for_tag_edit_and_undo_after_the_file_was_replaced() {
        for undo in [false, true] {
            let fixture = fixture();
            let lib = &fixture.lib;
            let path = fixture._dir.path().join("music/close.flac");
            std::fs::write(
                &path,
                include_bytes!("../../engine/tests/fixtures/sine44_a.flac"),
            )
            .unwrap();
            let ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() else {
                panic!("fixture scan already running");
            };
            scan.wait().unwrap();
            let track = lib.track_ids(&TrackQuery::default(), 1).unwrap()[0];
            let original = lib.track(track).unwrap().unwrap().title;
            let meta = ActionMeta::new("ui");
            let change = [TagChange::Set {
                key: "TITLE".into(),
                values: vec!["Replaced before close".into()],
            }];
            let first = undo.then(|| {
                lib.edit_tags(&[track], &change, meta, TagJobOptions::default())
                    .unwrap()
                    .wait()
                    .unwrap()
                    .action
                    .unwrap()
            });
            let prior_file = std::fs::read(&path).unwrap();
            let prior_row = lib.track(track).unwrap().unwrap().title;
            let (replaced, did_replace) = std::sync::mpsc::channel();
            let (commit, wait_for_commit) = std::sync::mpsc::channel();
            let options = TagJobOptions {
                progress: Box::new(move |done, total| {
                    if done == total {
                        replaced.send(()).unwrap();
                        wait_for_commit
                            .recv_timeout(Duration::from_secs(10))
                            .unwrap();
                    }
                }),
                ..Default::default()
            };
            let job = match first {
                Some(action) => lib.undo_tag_edit(action, meta, options).unwrap().unwrap(),
                None => lib.edit_tags(&[track], &change, meta, options).unwrap(),
            };
            did_replace.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_ne!(std::fs::read(&path).unwrap(), prior_file);
            assert_eq!(lib.track(track).unwrap().unwrap().title, prior_row);

            std::thread::scope(|scope| {
                let (done, result) = std::sync::mpsc::channel();
                scope.spawn(move || done.send(lib.close()).unwrap());
                let deadline = Instant::now() + Duration::from_secs(5);
                while lib.inner.register_file_job().is_ok() {
                    assert!(Instant::now() < deadline);
                    std::thread::yield_now();
                }
                let rejected = lib.edit_tags(&[track], &change, meta, TagJobOptions::default());
                let premature = result.recv_timeout(Duration::from_millis(40));
                commit.send(()).unwrap();
                assert!(matches!(rejected, Err(LibraryError::Closed)));
                assert!(matches!(
                    premature,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ));
                let action = job.wait().unwrap().action.unwrap();
                result
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();

                let reopened = Library::open(lib.inner.pool.path()).unwrap();
                let expected = if undo {
                    original
                } else {
                    Some("Replaced before close".into())
                };
                assert_eq!(reopened.track(track).unwrap().unwrap().title, expected);
                let actions = reopened.actions(None, Page::new(0, 10)).unwrap().items;
                let completed = actions.iter().find(|item| item.id == action).unwrap();
                assert_eq!(completed.kind, if undo { "undo" } else { "tag_edit" });
                assert_eq!(completed.undo_of, first);
                assert!(completed.undoable);
                if let Some(first) = first {
                    let edit = actions.iter().find(|item| item.id == first).unwrap();
                    assert_eq!(edit.undone_by, Some(action));
                }
                assert!(matches!(lib.search("", 1), Err(LibraryError::Closed)));
                assert!(matches!(
                    lib.wait_root_idle(fixture.root, Duration::MAX),
                    Err(LibraryError::Closed)
                ));
            });
        }
    }

    #[test]
    fn close_reports_busy_checkpoint_and_preserves_wal_for_recovery() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let track_id = track_for_close(&fixture);
        let db = lib.inner.pool.path();
        let wal = db.with_file_name("library.sqlite-wal");
        // Keep the test short while still exercising the real TRUNCATE busy result.
        lib.inner
            .writer
            .run(|conn| Ok(conn.busy_timeout(Duration::from_millis(50))?))
            .unwrap();
        let external =
            rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let snapshot = external.unchecked_transaction().unwrap();
        assert_eq!(
            snapshot
                .query_row("SELECT count(*) FROM play_events", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let event = close_play_event(track_id);
        lib.record_play(event.clone()).unwrap();

        let error = lib.close().unwrap_err();
        assert!(
            matches!(
                &*error,
                LibraryError::CheckpointIncomplete { busy: 1, log, checkpointed }
                if log > checkpointed
            ),
            "{error}"
        );
        assert!(Arc::ptr_eq(&error, &lib.close().unwrap_err()));
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);
        drop(snapshot);
        external.close().unwrap();

        let recovered = Library::open(db).unwrap();
        let history = recovered.history(Page::new(0, 10)).unwrap();
        assert_eq!(history.total, 1);
        assert_eq!(history.items[0].event, event);
        recovered.close().unwrap();
        assert!(!wal.exists());
        assert!(!db.with_file_name("library.sqlite-shm").exists());
    }

    #[test]
    fn dropping_last_library_handle_closes_pooled_readers_before_the_writer() {
        let fixture = fixture();
        fixture.lib.roots().unwrap();
        let db = fixture.lib.inner.pool.path().to_owned();
        assert!(db.with_file_name("library.sqlite-wal").exists());
        drop(fixture.lib);
        assert!(!db.with_file_name("library.sqlite-wal").exists());
        assert!(!db.with_file_name("library.sqlite-shm").exists());
    }

    #[test]
    fn root_undo_postcommit_read_failure_conservatively_resumes_all_affected_roots() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(&dir.path().join("library.db")).unwrap();
        let roots = vec![RootId(1), RootId(2)];
        for root in &roots {
            library.inner.file_access.suspend(*root);
        }
        library
            .inner
            .writer
            .run_data(|conn| {
                conn.execute("DROP TABLE roots", [])?;
                Ok(())
            })
            .unwrap();
        assert!(library.roots().is_err());
        library.finish_root_undo(&UndoEffect {
            undone: ActionId(1),
            tracks: vec![],
            playlists: vec![],
            roots: roots.clone(),
        });
        for root in roots {
            assert!(!library.root_released(root));
        }
    }

    fn temporary_files(dir: &Path) -> Vec<std::ffi::OsString> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().ends_with(".tmp"))
            .collect()
    }

    /// A checked write replaces only the file found at the
    /// path when it began (or still nothing); anything else there now is
    /// `location_changed`, nothing is replaced and no temporary file is left.
    #[test]
    fn checked_writes_replace_only_what_they_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir = storage_access::canonical_dir(dir.path()).unwrap();
        let (path, other) = (dir.join("x.m3u"), dir.join("other.m3u"));
        std::fs::write(&path, "old").unwrap();
        std::fs::write(&other, "other").unwrap();
        let found = |at: &Path| Checked {
            found: Some(mpmp_fs_desktop::DesktopStorage.entry_id(at).unwrap()),
        };
        let changed = |result: Result<()>| match result {
            Err(e) => assert_eq!(e.code(), "location_changed"),
            Ok(()) => panic!("written"),
        };
        // Another file than the one found (the path was swapped meanwhile).
        changed(write_atomically(
            &mpmp_fs_desktop::DesktopStorage,
            &path,
            b"new",
            true,
            Some(found(&other)),
        ));
        // A file where there was none.
        changed(write_atomically(
            &mpmp_fs_desktop::DesktopStorage,
            &path,
            b"new",
            true,
            Some(Checked { found: None }),
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        assert!(temporary_files(&dir).is_empty());
        // The file found: replaced.
        write_atomically(
            &mpmp_fs_desktop::DesktopStorage,
            &path,
            b"new",
            true,
            Some(found(&path)),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        // Still nothing there: written.
        let fresh = dir.join("fresh.m3u");
        write_atomically(
            &mpmp_fs_desktop::DesktopStorage,
            &fresh,
            b"new",
            true,
            Some(Checked { found: None }),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "new");
        assert!(temporary_files(&dir).is_empty());
    }
}

mod roots;

mod reads;

mod journal;

mod playlists;

mod file_jobs;
mod m3u_files;
#[cfg(all(test, feature = "desktop-fs"))]
use self::m3u_files::Checked;
use self::m3u_files::join_relative;
#[cfg(all(test, feature = "desktop-fs"))]
use self::m3u_files::write_atomically;
