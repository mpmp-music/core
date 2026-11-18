//! The single writer thread: every change to the database goes through here, one
//! command at a time, so scans, play events and root edits never contend for the write lock.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use rusqlite::{
    Connection, ErrorCode, OptionalExtension, ToSql, Transaction, params, params_from_iter,
};

use crate::journal::{self, ActionMeta, Done, Inverse, Kind, NewAction, RootOp};
use crate::tags::read::TrackMeta;
use crate::text::normalize;
use crate::types::{AlbumKey, PlayEvent, RootId, TrackId, VARIOUS_ARTISTS, file_stem};
use crate::{Damage, LibraryError, NotFound, Result, db::schema, play_stats, storage_access};

use super::{BUSY_TIMEOUT, with_busy_timeout};

/// A singleton's current parsed evidence. Missing INDEX 01 is not identity evidence.
pub(crate) struct Singleton {
    pub path: String,
    pub identity: Option<[u64; 2]>,
    pub sheet_track: Option<(String, u32, Option<u64>)>,
}

/// Reuse only a sole matching whole-file CUE row. Never merge separate identities.
/// Return active rows whose ownership must be retained when evidence is insufficient.
pub(crate) fn reclassify_singletons(
    conn: &mut Connection,
    root: RootId,
    paths: &[Singleton],
) -> Result<Vec<(String, Vec<TrackId>)>> {
    const SELECT_TRACKS: &str = r#"
SELECT id,cue_start,cue_end,cue_sheet,cue_number,available FROM tracks
WHERE root_id=?1 AND rel_path=?2 AND cue_sheet IS NOT NULL AND cue_sheet<>rel_path
"#;

    const SELECT_TRACKS_2: &str = r#"
SELECT EXISTS(SELECT 1 FROM tracks WHERE root_id=?1 AND rel_path=?2
AND cue_sheet IS NULL AND (hidden=0 OR available=1))
"#;

    const UPDATE_TRACKS: &str = r#"
UPDATE tracks SET cue_sheet=NULL,cue_number=NULL,cue_start=NULL,cue_end=NULL,cue_size=NULL,cue_mtime_ms=NULL,hidden=0,mtime_ms=-1,artists_parser_version=0,metadata_revision=metadata_revision+1
WHERE id=?1
"#;

    struct LegacyCue {
        id: i64,
        start: Option<i64>,
        end: Option<i64>,
        sheet: String,
        number: u32,
        available: bool,
    }
    let tx = conn.transaction()?;
    let pending = crate::cover::job::PendingTargets::load(&tx)?;
    let root_path: String =
        tx.query_row("SELECT path FROM roots WHERE id=?1", [root.0], |r| r.get(0))?;
    let mut retained = Vec::new();
    for candidate in paths {
        let path = &candidate.path;
        let legacy: Vec<LegacyCue> = tx
            .prepare(SELECT_TRACKS)?
            .query_map(params![root.0, path], |r| {
                Ok(LegacyCue {
                    id: r.get(0)?,
                    start: r.get(1)?,
                    end: r.get(2)?,
                    sheet: r.get(3)?,
                    number: r.get(4)?,
                    available: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        if legacy.is_empty() {
            continue;
        }
        let physical: bool = tx.query_row(SELECT_TRACKS_2, params![root.0, path], |r| r.get(0))?;
        let reusable = !physical
            && !pending.protects(&Path::new(&root_path).join(path), candidate.identity)
            && matches!(
                legacy.as_slice(),
                [old]
                    if old.start.unwrap_or(0) == 0
                        && candidate.sheet_track.as_ref().is_some_and(
                            |(sheet, number, gap_end)| {
                                *sheet == old.sheet
                                    && *number == old.number
                                    && (old.end.is_none()
                                        || gap_end
                                            .is_some_and(|end| old.end == Some(end as i64)))
                            },
                        ),
            );

        if reusable {
            let old = legacy[0].id;
            tx.execute(UPDATE_TRACKS, [old])?;
        } else {
            let active: Vec<_> = legacy
                .iter()
                .filter(|row| row.available)
                .map(|row| TrackId(row.id))
                .collect();
            if !active.is_empty() {
                tracing::warn!(%path,

                "retaining external CUE identity: whole-file ownership is not proven"
                );

                retained.push((path.clone(), active));
            }
        }
    }
    tx.commit()?;
    Ok(retained)
}

/// A file's row as produced by the scanner.
#[derive(Debug, Clone)]
pub(crate) struct TrackRecord {
    pub root_id: RootId,
    pub rel_path: String,
    pub size: u64,
    pub mtime_ms: i64,
    pub has_folder_art: bool,
    pub meta: TrackMeta,
    /// A track of a CUE sheet: the row is keyed by sheet and number, not by `rel_path`.
    pub cue: Option<CueRecord>,
    /// The row's `(size, mtime_ms)` when the file was found, for a file the library knew:
    /// the record replaces the row only if it still has them. Else the row changed meanwhile
    /// (a tag edit wrote the file and its row while a scan read it) and the record, which
    /// may hold what the file was before, is dropped (the next scan sees the file as it is).
    /// `None`: whatever the row holds.
    pub based_on: Option<(u64, i64)>,
    pub based_on_revision: Option<i64>,
    pub artists_only: bool,
    /// Identity already acquired during the walk; avoids reopening recovery targets.
    pub file_identity: Option<[u64; 2]>,
}

/// Where a track of a CUE sheet is, as [`TrackRecord::cue`].
#[derive(Debug, Clone)]
pub(crate) struct CueRecord {
    /// The sheet's `rel_path` (the audio file's own for an embedded sheet).
    pub sheet: String,
    pub number: u32,
    /// CD frames.
    pub start: u64,
    pub end: Option<u64>,
    /// The sheet file as scanned.
    pub size: u64,
    pub mtime_ms: i64,
}

/// One scan transaction (about 500 rows each).
#[derive(Debug, Default)]
pub(crate) struct ScanBatch {
    /// Root configuration actually used for parsing, including automatic detection.
    pub forced_encoding: Option<String>,
    /// Indexed identities from all roots observed in this scan, including unchanged files.
    pub observed: Arc<crate::cover::job::ScanObservations>,
    pub upserts: Vec<TrackRecord>,
    /// Whole-file reads to acknowledge for the CUE supplement cache. A track ID means
    /// the read was only for the supplement and its persisted metadata can be compared.
    pub supplement_reads: HashMap<String, Option<TrackId>>,
    /// Unchanged files that were unavailable and are back.
    pub available: Vec<TrackId>,
    /// Files that are gone.
    pub unavailable: Vec<TrackId>,
    /// Unchanged files whose folder gained or lost a cover image.
    pub folder_art: Vec<(TrackId, bool)>,
    /// Files a CUE sheet now cuts into tracks: no longer tracks of their own.
    pub hidden: Vec<TrackId>,
}

impl ScanBatch {
    pub fn len(&self) -> usize {
        self.upserts.len()
            + self.available.len()
            + self.unavailable.len()
            + self.folder_art.len()
            + self.hidden.len()
    }
}

#[derive(Default)]
pub(crate) struct ScanBatchResult {
    pub confirmed_supplements: Vec<(String, TrackId, i64)>,
    pub updated: u64,
    pub restored: u64,
}

type Reply<T> = Sender<Result<T>>;

pub(crate) enum WriteCmd {
    SetRootEncoding {
        id: RootId,
        label: Option<String>,
        reply: Reply<()>,
    },
    /// One batch of scan results for `root_id`; dropped if the root was removed meanwhile.
    Scan {
        root_id: RootId,
        batch: ScanBatch,
        reply: Reply<ScanBatchResult>,
    },
    FinishScan {
        root_id: RootId,
        reply: Reply<()>,
    },
    RecordPlay(PlayEvent),
    RebuildStats {
        reply: Reply<()>,
    },
    /// Replies once every earlier command has been applied.
    Flush {
        reply: Reply<()>,
    },
    /// Any other write: runs on the writer's connection and replies itself; it calls the
    /// second argument (bumps the data version) between its commit and the reply if it may
    /// have changed tracks, user data or roots (playlist edits have revisions instead).
    Run(RunFn),
}

/// A write for [`WriteCmd::Run`]: gets the connection and a function that bumps the data
/// version.
pub(crate) type RunFn = Box<dyn FnOnce(&mut Connection, &dyn Fn()) + Send>;

#[derive(Default)]
struct WriteFailures {
    draining: AtomicBool,
    summary: Mutex<Option<(usize, String)>>,
}

impl WriteFailures {
    fn note(&self, error: String) {
        let mut summary = self.summary.lock().unwrap_or_else(|p| p.into_inner());
        match &mut *summary {
            Some((count, _)) => *count += 1,
            None => *summary = Some((1, error)),
        }
    }

    fn reply<T>(&self, reply: Reply<T>, result: Result<T>) {
        // During ordinary operation the caller handles a returned failure. Also
        // retain errors no caller received, and every failure during shutdown.
        if let Err(error) = &result {
            let draining = self.draining.load(Ordering::Acquire);
            let message = error.to_string();
            if reply.send(result).is_err() || draining {
                self.note(message);
            }
        } else {
            let _ = reply.send(result);
        }
    }

    fn take(&self) -> Option<(usize, String)> {
        self.summary
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }
}

#[cfg(test)]
#[test]
fn write_reply_classifies_the_failure_before_sending_and_retains_undelivered_errors() {
    use std::fmt::{self, Display};

    #[derive(Debug)]
    struct SlowMessage {
        entered: Sender<()>,
        release: Receiver<()>,
    }
    impl Display for SlowMessage {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let _ = self.entered.send(());
            let _ = self.release.recv_timeout(Duration::from_secs(5));
            f.write_str("failed during ordinary operation")
        }
    }
    impl std::error::Error for SlowMessage {}

    let failures = WriteFailures::default();
    thread::scope(|scope| {
        let (reply, answer) = bounded(1);
        let (entered, format_started) = bounded(1);
        let (proceed, release) = bounded(1);
        let err = std::io::Error::other(SlowMessage { entered, release });
        let writer = &failures;
        scope.spawn(move || writer.reply(reply, Err::<(), _>(err.into())));
        format_started.recv_timeout(Duration::from_secs(5)).unwrap();
        failures.draining.store(true, Ordering::Release);
        proceed.send(()).unwrap();
        assert!(
            answer
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_err()
        );
    });
    assert!(failures.take().is_none());

    let (reply, answer) = bounded(1);
    drop(answer);
    failures.draining.store(false, Ordering::Release);
    failures.reply(reply, Err::<(), _>(LibraryError::Cancelled));
    assert_eq!(failures.take().unwrap().0, 1);
}

pub(crate) struct Writer {
    tx: Mutex<Option<Sender<WriteCmd>>>,
    thread: Mutex<Option<JoinHandle<Result<()>>>>,
    thread_id: thread::ThreadId,
    closing: Arc<AtomicBool>,
    failures: Arc<WriteFailures>,
    progress: Arc<Mutex<crate::CloseProgress>>,
    #[cfg(all(test, feature = "desktop-fs"))]
    pub(crate) before_checkpoint: Arc<Mutex<Option<std::sync::mpsc::Sender<()>>>>,
    /// Notes every write's failure that says the database is damaged.
    damage: Damage,
    _maintenance: super::schema::reset::Guard,
}

impl Writer {
    /// Opens (creating if needed) the database, migrates it and starts the writer thread.
    /// Every open runs normal WAL setup, optimization and recovery after preparation.
    ///
    /// `data_version` is bumped after every command that may have changed tracks, play
    /// statistics, user data or roots (not after playlist edits, which have revisions).
    /// `damage` notes every write that finds the database damaged.
    pub fn start(
        db_path: &Path,
        data_version: Arc<AtomicU64>,
        damage: Damage,
        closing: Arc<AtomicBool>,
        progress: Arc<Mutex<crate::CloseProgress>>,
        storage: &mpmp_storage::StorageBackends,
        allow_migrate: bool,
    ) -> Result<Self> {
        let (maintenance, _) = schema::prepare(db_path, storage, allow_migrate)?;
        let mut conn = with_busy_timeout(Connection::open(db_path))?;
        maintenance.check(storage)?;
        enable_wal(&conn)?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        crate::db::pool::tune(&conn)?;
        conn.set_prepared_statement_cache_capacity(32);
        schema::optimize(&conn)?;
        crate::cover::job::recover(
            storage.files.as_ref(),
            storage.replacements.as_ref(),
            &mut conn,
        )?;
        journal::prune(&conn, now_ms())?;

        let (tx, rx) = unbounded();
        let watch = damage.clone();
        let failures = Arc::new(WriteFailures::default());
        let writer_failures = Arc::clone(&failures);
        let writer_progress = Arc::clone(&progress);
        #[cfg(all(test, feature = "desktop-fs"))]
        let before_checkpoint = Arc::new(Mutex::new(None));
        #[cfg(all(test, feature = "desktop-fs"))]
        let checkpoint_hook = Arc::clone(&before_checkpoint);
        let thread = thread::Builder::new()
            .name("library-writer".into())
            .spawn(move || {
                run(
                    conn,
                    rx,
                    data_version,
                    watch,
                    &writer_failures,
                    &writer_progress,
                    #[cfg(all(test, feature = "desktop-fs"))]
                    &checkpoint_hook,
                )
            })?;
        Ok(Self {
            tx: Mutex::new(Some(tx)),
            thread_id: thread.thread().id(),
            thread: Mutex::new(Some(thread)),
            closing,
            failures,
            progress,
            #[cfg(all(test, feature = "desktop-fs"))]
            before_checkpoint,
            damage,
            _maintenance: maintenance,
        })
    }

    pub fn on_writer_thread(&self) -> bool {
        thread::current().id() == self.thread_id
    }

    /// Begin tracking all accepted write failures while finishable jobs still have DB access.
    pub fn begin_shutdown(&self) {
        self.failures.draining.store(true, Ordering::Release);
    }

    pub fn send(&self, cmd: WriteCmd) -> Result<()> {
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        if self.closing.load(Ordering::Acquire) {
            return Err(LibraryError::Closed);
        }
        tx.as_ref()
            .ok_or(LibraryError::WriterStopped)?
            .send(cmd)
            .map_err(|_| LibraryError::WriterStopped)
    }

    /// Called after the read pool has closed. Disconnecting the sender drains the queue.
    pub fn close(&self) -> Result<()> {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        let thread = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        match thread {
            Some(thread) => match thread.join() {
                Ok(result) => result,
                Err(_) => {
                    let error = LibraryError::WriterStopped;
                    self.progress
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .write_connection = crate::CloseStage::Failed(error.to_string());
                    Err(error)
                }
            },
            None => Ok(()),
        }
    }

    /// Sends a command built around a reply channel and waits for the answer.
    pub fn call<T>(&self, make: impl FnOnce(Reply<T>) -> WriteCmd) -> Result<T> {
        let (reply, answer) = bounded(1);
        self.send(make(reply))?;
        answer.recv().map_err(|_| LibraryError::WriterStopped)?
    }

    /// Runs `f` on the writer thread with its connection and waits for the result.
    pub fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.run_with(f, false)
    }

    /// [`Writer::run`] for a write that may change tracks, user data or roots: bumps the
    /// data version afterwards.
    pub fn run_data<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.run_with(f, true)
    }

    fn run_with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        data: bool,
    ) -> Result<T> {
        let damage = self.damage.clone();
        let failures = self.failures.clone();
        self.call(|reply| {
            WriteCmd::Run(Box::new(move |conn, bump| {
                let result = damage.watch(f(conn));
                if data {
                    bump();
                }
                failures.reply(reply, result);
            }))
        })
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static ON_WAL_BUSY: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}

fn enable_wal(conn: &Connection) -> Result<()> {
    // Switching to WAL upgrades a read lock. SQLite can skip the busy handler
    // on this upgrade, so retry the whole PRAGMA within the same timeout budget.
    let deadline = Instant::now() + BUSY_TIMEOUT;
    let mut remaining = BUSY_TIMEOUT;
    let mode: String = loop {
        conn.busy_timeout(remaining)?;
        // Step through completion so a failure while committing is also returned.
        match conn.query_one("PRAGMA journal_mode=WAL", [], |row| row.get(0)) {
            Err(error) if error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) => {
                #[cfg(test)]
                if let Some(on_busy) = ON_WAL_BUSY.take() {
                    on_busy();
                }
                let backoff = deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(10));
                if !backoff.is_zero() {
                    thread::sleep(backoff);
                }
                remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(error.into());
                }
            }
            result => break result?,
        }
    };
    if mode != "wal" {
        return Err(LibraryError::WalUnavailable { mode });
    }
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(())
}

#[cfg(test)]
#[test]
fn enable_wal_rejects_the_actual_non_wal_mode() {
    // SQLite accepts the PRAGMA on an in-memory database and returns "memory".
    let conn = with_busy_timeout(Connection::open_in_memory()).unwrap();
    let error = enable_wal(&conn).unwrap_err();
    assert!(matches!(error, LibraryError::WalUnavailable { ref mode } if mode == "memory"));
    assert_eq!(error.code(), "db");
}

fn run(
    mut conn: Connection,
    rx: Receiver<WriteCmd>,
    data_version: Arc<AtomicU64>,
    damage: Damage,
    failures: &WriteFailures,
    progress: &Mutex<crate::CloseProgress>,
    #[cfg(all(test, feature = "desktop-fs"))] before_checkpoint: &Mutex<
        Option<std::sync::mpsc::Sender<()>>,
    >,
) -> Result<()> {
    while let Ok(cmd) = rx.recv() {
        // Bumped after the commit and before the reply: a reader that loads the version
        // before its snapshot never files old data under the new version (newer data filed
        // under the old one is merely never looked up again), and a caller reading right
        // after its own write never gets a count cached before it.
        let bump = || {
            data_version.fetch_add(1, Ordering::Release);
        };
        match cmd {
            WriteCmd::SetRootEncoding { id, label, reply } => {
                let result = damage.watch(set_root_encoding(&mut conn, id, label));
                bump();
                failures.reply(reply, result);
            }
            WriteCmd::Scan {
                root_id,
                batch,
                reply,
            } => {
                let result = damage.watch(apply_scan_batch(&mut conn, root_id, batch));
                bump();
                failures.reply(reply, result);
            }
            WriteCmd::FinishScan { root_id, reply } => {
                let result = damage.watch(
                    conn.execute(
                        "UPDATE roots SET last_scan_at = ?2 WHERE id = ?1",
                        params![root_id.0, now_ms()],
                    )
                    .map(drop)
                    .map_err(LibraryError::from),
                );
                bump();
                failures.reply(reply, result);
            }
            WriteCmd::RebuildStats { reply } => {
                let result = damage.watch(conn.transaction().map_err(LibraryError::from).and_then(
                    |tx| {
                        play_stats::rebuild(&tx)?;
                        Ok(tx.commit()?)
                    },
                ));
                bump();
                failures.reply(reply, result);
            }
            WriteCmd::RecordPlay(event) => {
                if let Err(err) = damage.watch(record_play(&mut conn, &event)) {
                    tracing::warn!(%err, track = %event.track_id, "failed to record play event");
                    failures.note(format!("play event for track {}: {err}", event.track_id));
                }
                bump();
            }
            WriteCmd::Flush { reply } => {
                let _ = reply.send(Ok(()));
            }
            WriteCmd::Run(f) => f(&mut conn, &bump),
        }
    }
    use crate::CloseStage;
    let summary = failures.take();
    progress.lock().unwrap_or_else(|p| p.into_inner()).writes = match &summary {
        Some((count, first)) => {
            CloseStage::Failed(format!("{count} write(s) failed (first: {first})"))
        }
        None => CloseStage::Succeeded,
    };
    #[cfg(all(test, feature = "desktop-fs"))]
    if let Some(hook) = before_checkpoint.lock().unwrap().take() {
        let _ = hook.send(());
    }
    let checkpoint = damage.watch(checkpoint(&conn));
    progress
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .checkpoint = CloseStage::from_result(&checkpoint);
    let closed = damage.watch(conn.close().map_err(|(_, err)| err.into()));
    progress
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .write_connection = CloseStage::from_result(&closed);
    let database = checkpoint.and(closed);
    match summary {
        Some((count, first)) => Err(LibraryError::WriteShutdown {
            count,
            first,
            database: database.err().map(Box::new),
        }),
        None => database,
    }
}

fn checkpoint(conn: &Connection) -> Result<()> {
    let (busy, log, checkpointed) =
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if (busy, log, checkpointed) != (0, 0, 0) {
        return Err(LibraryError::CheckpointIncomplete {
            busy,
            log,
            checkpointed,
        });
    }
    Ok(())
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| LibraryError::NonUnicodePath(path.to_owned()))
}

fn apply_scan_batch(
    conn: &mut Connection,
    root_id: RootId,
    mut batch: ScanBatch,
) -> Result<ScanBatchResult> {
    const UPDATE_TRACKS: &str = r#"
UPDATE tracks SET has_folder_art = ?2, cover_revision = CASE WHEN has_embedded_art
THEN cover_revision ELSE NULL END WHERE id = ?1
"#;

    let tx = conn.transaction()?;
    let root: Option<(bool, Option<String>)> = tx
        .query_row(
            "SELECT removed = 0, forced_encoding FROM roots WHERE id = ?1",
            [root_id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((true, encoding)) = root else {
        tracing::debug!(root = %root_id, "dropping scan batch of removed root");
        return Ok(ScanBatchResult::default());
    };
    if encoding != batch.forced_encoding {
        tracing::debug!(root = %root_id,
         "dropping scan batch parsed with an obsolete root encoding");

        return Ok(ScanBatchResult::default());
    }
    let now = now_ms();
    let mut pending = crate::cover::job::PendingTargets::load(&tx)?;
    // Empty snapshots do not construct paths or inspect the observation index.
    let root_path = if pending.is_empty() {
        None
    } else {
        pending.merge_observed(&batch.observed);
        Some(PathBuf::from(tx.query_row(
            "SELECT path FROM roots WHERE id=?1",
            [root_id.0],
            |r| r.get::<_, String>(0),
        )?))
    };
    let mut result = ScanBatchResult::default();
    for record in &batch.upserts {
        if root_path.as_ref().is_some_and(|root| {
            pending.protects(&root.join(&record.rel_path), record.file_identity)
        }) {
            continue;
        }
        let supplement = record
            .cue
            .is_none()
            .then(|| batch.supplement_reads.get(&record.rel_path))
            .flatten();
        let compare = supplement.copied().flatten();
        let Some(written) = upsert_track_unchecked(&tx, record, now, compare)? else {
            continue;
        };
        let (id, revision) = match written {
            TrackWrite::Full(id, revision) => {
                if compare.is_some() {
                    result.updated += 1;
                }
                (id, revision)
            }
            TrackWrite::Unchanged(row) => {
                if !row.available {
                    batch.available.push(row.id);
                    result.restored += 1;
                }
                if row.folder_art != record.has_folder_art {
                    batch.folder_art.push((row.id, record.has_folder_art));
                }
                (row.id, row.revision)
            }
            TrackWrite::ArtistsOnly => continue,
        };
        if supplement.is_some() {
            result
                .confirmed_supplements
                .push((record.rel_path.clone(), id, revision));
        }
    }
    {
        let mut stmt = tx.prepare_cached("UPDATE tracks SET available = ?2 WHERE id = ?1")?;
        for id in &batch.available {
            stmt.execute(params![id.0, true])?;
        }
        for id in &batch.unavailable {
            let protected = if pending.blocks_missing() {
                true
            } else if let Some(root) = &root_path {
                let relative: String =
                    tx.query_row("SELECT rel_path FROM tracks WHERE id=?1", [id.0], |r| {
                        r.get(0)
                    })?;
                pending.protects_missing(*id, &root.join(relative))
            } else {
                false
            };
            if !protected {
                stmt.execute(params![id.0, false])?;
            }
        }
        let mut stmt = tx.prepare_cached(UPDATE_TRACKS)?;
        for (id, art) in &batch.folder_art {
            stmt.execute(params![id.0, art])?;
        }
        let mut stmt =
            tx.prepare_cached("UPDATE tracks SET available = 0, hidden = 1 WHERE id = ?1")?;
        for id in &batch.hidden {
            stmt.execute([id.0])?;
        }
    }
    schema::refresh_aggregates(&tx)?;
    tx.commit()?;
    Ok(result)
}

/// Album grouping for a track: `(key, album artist shown for the album)`. Tracks without
/// an album tag get the artist's unknown album (empty title part).
pub(crate) fn album_grouping(meta: &TrackMeta) -> (AlbumKey, Option<String>) {
    let artist = meta
        .album_artist
        .clone()
        .or_else(|| meta.compilation.then(|| VARIOUS_ARTISTS.to_owned()))
        .or_else(|| meta.artist.clone());
    let album = meta.album.as_deref().unwrap_or("");
    let key = AlbumKey::new(artist.as_deref().unwrap_or(""), album);
    (key, artist)
}

fn norm(text: &Option<String>) -> String {
    text.as_deref().map(normalize).unwrap_or_default()
}

/// The columns derived from a track's display text: search keys and grouping. One function
/// for the writer and the v2 migration, so both produce the same keys.
pub(crate) struct Derived {
    pub title_norm: String,
    pub artist_norm: String,
    pub album_norm: String,
    pub album_artist_norm: String,
    pub genre_norm: String,
    pub rel_path_norm: String,
    pub album_key: AlbumKey,
    pub group_artist: Option<String>,
    pub group_artist_norm: String,
}

impl Derived {
    pub fn of(m: &TrackMeta, rel_path: &str) -> Self {
        let (album_key, group_artist) = album_grouping(m);
        Self {
            // Untagged files are found (and sorted) by file name.
            title_norm: Some(norm(&m.title))
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| normalize(file_stem(rel_path))),
            artist_norm: norm(&m.artist),
            album_norm: norm(&m.album),
            album_artist_norm: norm(&m.album_artist),
            genre_norm: norm(&m.genre),
            rel_path_norm: normalize(rel_path),
            group_artist_norm: norm(&group_artist),
            album_key,
            group_artist,
        }
    }
}

// The upsert and the CUE supplement comparison bind exactly these metadata columns.
macro_rules! metadata_columns {
    () => {
        "title, artist, album, album_artist, genre, year,
         track_no, track_total, disc_no, disc_total, compilation,
         duration_ms, sample_rate, channels, bits_per_sample, bitrate_kbps, codec, lossless,
         rg_track_gain, rg_track_peak, rg_album_gain, rg_album_peak,
         has_embedded_art, tag_encoding,
         title_norm, artist_norm, album_norm, album_artist_norm, genre_norm,
         album_key, album_group_artist, group_artist_norm, rel_path_norm"
    };
}

macro_rules! metadata_values {
    () => {
        "?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
         ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33"
    };
}

/// The UPSERT of one row: a file's is keyed by its path (and shows the file again if it
/// was hidden), a CUE sheet track's by its sheet and number (and may move to another
/// audio file, e.g. after the sheet's `FILE` was re-encoded).
macro_rules! upsert_sql {
    ($conflict:literal, $extra:literal) => {
        concat!(
            "INSERT INTO tracks (",
            metadata_columns!(),
            ", id, root_id, rel_path, size, mtime_ms, available,
                added_at, updated_at, has_folder_art,
                cue_sheet, cue_number, cue_start, cue_end, cue_size, cue_mtime_ms
            ) VALUES (",
            metadata_values!(),
            ", ?48, ?34, ?35, ?36, ?37, 1,
                ?38, ?38, ?39,
                ?40, ?41, ?42, ?43, ?44, ?45
            )
            ON CONFLICT ",
            $conflict,
            " DO UPDATE SET
                size = excluded.size, mtime_ms = excluded.mtime_ms, available = 1,
                title = excluded.title, artist = excluded.artist, album = excluded.album,
                album_artist = excluded.album_artist, genre = excluded.genre, year = excluded.year,
                track_no = excluded.track_no, track_total = excluded.track_total,
                disc_no = excluded.disc_no, disc_total = excluded.disc_total,
                compilation = excluded.compilation,
                duration_ms = excluded.duration_ms, sample_rate = excluded.sample_rate,
                channels = excluded.channels, bits_per_sample = excluded.bits_per_sample,
                bitrate_kbps = excluded.bitrate_kbps, codec = excluded.codec,
                lossless = excluded.lossless,
                rg_track_gain = excluded.rg_track_gain, rg_track_peak = excluded.rg_track_peak,
                rg_album_gain = excluded.rg_album_gain, rg_album_peak = excluded.rg_album_peak,
                has_embedded_art = excluded.has_embedded_art, cover_revision = NULL, tag_encoding = excluded.tag_encoding,
                title_norm = excluded.title_norm, artist_norm = excluded.artist_norm,
                album_norm = excluded.album_norm, album_artist_norm = excluded.album_artist_norm,
                genre_norm = excluded.genre_norm,
                album_key = excluded.album_key, album_group_artist = excluded.album_group_artist,
                group_artist_norm = excluded.group_artist_norm,
                has_folder_art = excluded.has_folder_art, rel_path_norm = excluded.rel_path_norm,
                metadata_revision = tracks.metadata_revision + 1, updated_at = excluded.updated_at, ",
            $extra,
            " WHERE ?46 IS NULL OR (tracks.size = ?46 AND tracks.mtime_ms = ?47)
            RETURNING id, metadata_revision"
        )
    };
}

const UPSERT_FILE: &str = upsert_sql!(
    "(id)",
    "hidden = CASE WHEN ?46 IS NULL THEN tracks.hidden ELSE 0 END,
     available = CASE WHEN ?46 IS NULL AND tracks.hidden = 1 THEN 0 ELSE 1 END"
);

const UPSERT_CUE: &str = upsert_sql!(
    "(root_id, cue_sheet, cue_number) WHERE cue_sheet IS NOT NULL",
    "rel_path = excluded.rel_path, cue_start = excluded.cue_start, cue_end = excluded.cue_end,
     cue_size = excluded.cue_size, cue_mtime_ms = excluded.cue_mtime_ms"
);

/// Inserts or updates a track's row from what was read of its file (unless the row changed
/// since: [`TrackRecord::based_on`]).
pub(crate) fn upsert_track(
    storage: &dyn mpmp_storage::LibraryStorage,
    tx: &Transaction<'_>,
    record: &TrackRecord,
    now: i64,
) -> Result<()> {
    let root: String = tx.query_row(
        "SELECT path FROM roots WHERE id=?1",
        [record.root_id.0],
        |r| r.get(0),
    )?;
    if crate::cover::job::pending_path(
        storage,
        tx,
        &std::path::Path::new(&root).join(&record.rel_path),
    )? {
        return Ok(());
    }

    upsert_track_unchecked(tx, record, now, None).map(drop)
}

#[derive(Clone, Copy)]
struct CurrentTrack {
    id: TrackId,
    revision: i64,
    size: u64,
    mtime_ms: i64,
    hidden: bool,
    available: bool,
    folder_art: bool,
}

enum TrackWrite {
    Full(TrackId, i64),
    Unchanged(CurrentTrack),
    ArtistsOnly,
}

fn upsert_track_unchecked(
    tx: &Transaction<'_>,
    record: &TrackRecord,
    now: i64,
    compare: Option<TrackId>,
) -> Result<Option<TrackWrite>> {
    const SELECT_TRACKS: &str = r#"
SELECT id,metadata_revision,size,mtime_ms,hidden,available,has_folder_art FROM tracks WHERE root_id=?1
AND ((?3 IS NULL AND cue_sheet IS NULL AND rel_path=?2) OR (cue_sheet=?3
AND cue_number=?4)) ORDER BY (hidden=1 AND available=0),id DESC LIMIT
1
"#;

    let current =
        if record.cue.is_none() || record.based_on_revision.is_some() || record.artists_only {
            tx.prepare_cached(SELECT_TRACKS)?
                .query_row(
                    params![
                        record.root_id.0,
                        record.rel_path,
                        record.cue.as_ref().map(|c| &c.sheet),
                        record.cue.as_ref().map(|c| c.number)
                    ],
                    |r| {
                        Ok(CurrentTrack {
                            id: TrackId(r.get(0)?),
                            revision: r.get(1)?,
                            size: r.get::<_, i64>(2)? as u64,
                            mtime_ms: r.get(3)?,
                            hidden: r.get(4)?,
                            available: r.get(5)?,
                            folder_art: r.get(6)?,
                        })
                    },
                )
                .optional()?
        } else {
            None
        };
    if let Some(expected) = record.based_on_revision
        && current.is_none_or(|row| row.revision != expected)
    {
        return Ok(None);
    }
    if record.artists_only {
        let Some(row) =
            current.filter(|row| (row.size, row.mtime_ms) == (record.size, record.mtime_ms))
        else {
            return Ok(None);
        };
        crate::person::replace(
            tx,
            row.id.0,
            &track_artists(&record.meta),
            crate::person::PARSER_VERSION,
        )?;
        tx.execute(
            "UPDATE tracks SET metadata_revision=metadata_revision+1 WHERE id=?1",
            [row.id.0],
        )?;
        return Ok(Some(TrackWrite::ArtistsOnly));
    }
    let m = &record.meta;
    let d = Derived::of(m, &record.rel_path);
    let rg = &m.replay_gain;
    let duration_ms = m.duration_ms.map(|d| d as i64);
    let tag_encoding = m.tag_encoding.map(|e| e.name());
    let album_key = d.album_key.as_str();
    let metadata = params![
        m.title,
        m.artist,
        m.album,
        m.album_artist,
        m.genre,
        m.year,
        m.track_no,
        m.track_total,
        m.disc_no,
        m.disc_total,
        m.compilation,
        duration_ms,
        m.sample_rate,
        m.channels,
        m.bits_per_sample,
        m.bitrate_kbps,
        m.codec,
        m.lossless,
        rg.track_gain_db,
        rg.track_peak,
        rg.album_gain_db,
        rg.album_peak,
        m.has_embedded_art,
        tag_encoding,
        d.title_norm,
        d.artist_norm,
        d.album_norm,
        d.album_artist_norm,
        d.genre_norm,
        album_key,
        d.group_artist,
        d.group_artist_norm,
        d.rel_path_norm,
    ];
    let artists = track_artists(m);
    if let Some(id) = compare {
        let Some(row) = current.filter(|row| {
            row.id == id
                && !row.hidden
                && (row.size, row.mtime_ms) == (record.size, record.mtime_ms)
        }) else {
            return Ok(None);
        };
        if metadata_matches(tx, id, metadata)?
            && tags_match(tx, id, &m.tags)?
            && crate::person::matches_stored(tx, id.0, &artists)?
        {
            return Ok(Some(TrackWrite::Unchanged(row)));
        }
    }

    let cue = record.cue.as_ref();
    let mut stmt = tx.prepare_cached(if cue.is_some() {
        UPSERT_CUE
    } else {
        UPSERT_FILE
    })?;
    let (based_size, based_mtime) = match record.based_on {
        Some((size, mtime)) => (Some(size as i64), Some(mtime)),
        None => (None, None),
    };
    let written = stmt
        .query_row(
            params_from_iter(
                metadata.iter().copied().chain(
                    params![
                        record.root_id.0,
                        record.rel_path,
                        record.size as i64,
                        record.mtime_ms,
                        now,
                        record.has_folder_art,
                        cue.map(|c| &c.sheet),
                        cue.map(|c| c.number),
                        cue.map(|c| c.start as i64),
                        cue.and_then(|c| c.end.map(|e| e as i64)),
                        cue.map(|c| c.size as i64),
                        cue.map(|c| c.mtime_ms),
                        based_size,
                        based_mtime,
                        record
                            .cue
                            .is_none()
                            .then(|| current.map(|row| row.id.0))
                            .flatten(),
                    ]
                    .iter()
                    .copied(),
                ),
            ),
            |row| Ok((TrackId(row.get(0)?), row.get(1)?)),
        )
        .optional()?;
    let Some((id, revision)) = written else {
        tracing::debug!(
            path = record.rel_path,
            "dropping a scan record of a row changed meanwhile"
        );
        return Ok(None);
    };
    replace_tags(tx, id.0, &m.tags)?;
    crate::person::replace(tx, id.0, &artists, crate::person::PARSER_VERSION)?;
    Ok(Some(TrackWrite::Full(id, revision)))
}

fn track_artists(meta: &TrackMeta) -> Cow<'_, crate::person::ArtistList> {
    match &meta.artists {
        Some(list) => Cow::Borrowed(list),
        None => Cow::Owned(crate::person::ArtistList::from_tags(
            &meta.tags,
            meta.artist.as_deref(),
        )),
    }
}

fn metadata_matches(tx: &Transaction<'_>, id: TrackId, metadata: &[&dyn ToSql]) -> Result<bool> {
    const SELECT: &str = concat!(
        "SELECT (",
        metadata_columns!(),
        ") IS (",
        metadata_values!(),
        ") FROM tracks WHERE id=?34"
    );
    let matches = tx.prepare_cached(SELECT)?.query_row(
        params_from_iter(
            metadata
                .iter()
                .copied()
                .chain(params![id.0].iter().copied()),
        ),
        |row| row.get(0),
    )?;
    Ok(matches)
}

fn stored_tags(tags: &[(String, String)]) -> impl Iterator<Item = (&str, &str, String)> {
    tags.iter().filter_map(|(key, value)| {
        // A value without a search key (only marks or control characters) counts as missing.
        let value_norm = normalize(value);
        (!value_norm.is_empty()).then_some((key.as_str(), value.as_str(), value_norm))
    })
}

fn tags_match(tx: &Transaction<'_>, id: TrackId, tags: &[(String, String)]) -> Result<bool> {
    let mut stmt = tx.prepare_cached(
        "SELECT key,value,value_norm FROM track_tags WHERE track_id=?1 ORDER BY rowid",
    )?;
    let mut rows = stmt.query([id.0])?;
    for (key, value, value_norm) in stored_tags(tags) {
        let Some(row) = rows.next()? else {
            return Ok(false);
        };
        if row.get::<_, String>(0)? != key
            || row.get::<_, String>(1)? != value
            || row.get::<_, String>(2)? != value_norm
        {
            return Ok(false);
        }
    }
    Ok(rows.next()?.is_none())
}

/// Replaces the `track_tags` rows of one track.
fn replace_tags(tx: &Transaction<'_>, track_id: i64, tags: &[(String, String)]) -> Result<()> {
    tx.prepare_cached("DELETE FROM track_tags WHERE track_id = ?1")?
        .execute([track_id])?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO track_tags (track_id, key, value, value_norm) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (key, value, value_norm) in stored_tags(tags) {
        insert.execute(params![track_id, key, value, value_norm])?;
    }
    Ok(())
}

fn record_play(conn: &mut Connection, event: &PlayEvent) -> Result<()> {
    let tx = conn.transaction()?;
    tx.prepare_cached(
        "INSERT INTO play_events (track_id, context, started_at, played_ms, ended_reason, origin)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        event.track_id.0,
        event.context,
        event.started_at,
        event.played_ms as i64,
        event.ended_reason.as_str(),
        event.origin,
    ])?;
    let duration: Option<i64> = tx
        .prepare_cached("SELECT duration_ms FROM tracks WHERE id = ?1")?
        .query_row([event.track_id.0], |row| row.get(0))
        .optional()?
        .flatten();
    let counted = play_stats::classify(
        event.ended_reason,
        event.played_ms,
        duration.map(|d| d.max(0) as u64),
    );
    play_stats::apply_event(&tx, event.track_id.0, event.started_at, counted)?;
    tx.commit()?;
    Ok(())
}

mod roots;
use self::roots::set_root_encoding;
pub(crate) use self::roots::{add_root, apply_root_op, remove_root};
