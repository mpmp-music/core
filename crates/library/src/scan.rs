//! Incremental folder scanning: walk every root, re-read files whose `(size, mtime)`
//! changed, mark vanished ones unavailable. Runs on its own thread; rows reach the database
//! through the writer thread in batches.
//!
//! CUE sheets (cue.rs) are read on every pass (they are small): each audio file a sheet
//! cuts into tracks is hidden, and each of its tracks is a row keyed by sheet and number,
//! rebuilt when the sheet or the audio file changed. A FLAC file's embedded sheet is read
//! with its tags, so only when the file changed (or was hidden, or a sidecar sheet stopped
//! cutting it); a sidecar sheet wins over an embedded one.

pub(crate) mod diagnostics;

use std::collections::{HashMap, HashSet};

use std::io::Read;

use std::panic::{self, AssertUnwindSafe};

use std::path::{Component, Path, PathBuf};

use std::sync::Arc;

use std::sync::atomic::{AtomicBool, Ordering};

use std::thread::JoinHandle;

use std::time::{Duration, Instant, UNIX_EPOCH};

use encoding_rs::Encoding;

use sha2::{Digest, Sha256};

use walkdir::WalkDir;

use crate::cover::cover_rank;

use crate::cue::{Cut, MAX_SHEET_BYTES, Sheet, cd_frames_to_ms, cue_meta, decode_sheet};

use crate::db::reads::{KnownCue, KnownRows};

use crate::db::writer::{CueRecord, ScanBatch, TrackRecord, WriteCmd};

use crate::handle::Inner;

use crate::tags::encoding::encoding_for_label;

use crate::tags::read::{TagError, TrackMeta, read_track};

use crate::types::{
    Root, RootId, SCAN_ERROR_LIMIT, ScanError, ScanErrorCode, ScanId, ScanProgress, ScanReport,
    TrackId, file_stem,
};

use crate::{LibraryError, Result, db::reads, db::schema};

/// File extensions (lower case) that are indexed. Limited to what the engine can decode.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "ogg", "oga", "m4a", "m4b", "mp4", "aac", "wav", "aif", "aiff", "aifc",
];

/// Rows per write transaction.
const BATCH_ROWS: usize = 500;

/// Minimum interval between progress callbacks.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// What [`crate::Library::scan`] did.
#[must_use]
#[derive(Debug)]
pub enum ScanStart {
    /// A new scan started.
    Started(ScanHandle),
    /// A scan was already running: the request joined it. The running scan re-reads the
    /// roots and makes another pass before it finishes, so changes made before this call
    /// (e.g. an added root) are covered, and its final [`ScanReport`] includes that pass
    /// and counts this request in [`ScanReport::coalesced`]. The progress callback given
    /// with this request is not used; the running scan's handle reports the result.
    Coalesced(ScanId),
}

/// A running scan. Dropping the handle lets the scan finish in the background.
#[derive(Debug)]
pub struct ScanHandle {
    id: ScanId,
    cancel: Arc<AtomicBool>,
    thread: JoinHandle<Result<ScanReport>>,
}

impl ScanHandle {
    /// This scan's id, as [`ScanStart::Coalesced`] names it for requests that join it.
    pub fn id(&self) -> ScanId {
        self.id
    }

    /// Asks the scan to stop after the current file, including any coalesced passes. Rows
    /// written so far stay; files are not marked unavailable by a cancelled scan.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// Waits for the scan, including coalesced passes. Its results are visible to reads when
    /// this returns. The scanned counter counts distinct root-relative audio paths over all passes; `errors` counts unreadable files
    /// and unreachable roots, each path once (also when several passes meet it). A
    /// cancelled scan returns its report with [`ScanReport::cancelled`] set; errors are
    /// database failures, a stopped writer or a panic ([`LibraryError::ScanPanicked`]).
    pub fn wait(self) -> Result<ScanReport> {
        self.thread.join().map_err(|_| LibraryError::ScanPanicked)?
    }
}

/// Internal scan scope. Directory retries never acknowledge an upgrade scan.
#[derive(Debug, Clone, Default)]
struct ScanScope {
    roots: Option<HashSet<RootId>>,
    directories: Option<HashSet<PathBuf>>,
}

impl ScanScope {
    fn full(&self) -> bool {
        self.roots.is_none() && self.directories.is_none()
    }

    fn directory_only(&self, root: &Root) -> bool {
        self.directories.is_some()
            && self
                .roots
                .as_ref()
                .is_none_or(|ids| !ids.contains(&root.id))
    }

    fn covers(&self, root: &Root) -> bool {
        self.full()
            || self
                .roots
                .as_ref()
                .is_some_and(|ids| ids.contains(&root.id))
            || self
                .directories
                .as_ref()
                .is_some_and(|dirs| dirs.iter().any(|dir| dir.starts_with(&root.path)))
    }

    fn merge(&mut self, other: Self) {
        if self.full() || other.full() {
            *self = Self::default();
            return;
        }
        if let Some(roots) = other.roots {
            self.roots.get_or_insert_with(HashSet::new).extend(roots);
        }
        if let Some(dirs) = other.directories {
            self.directories
                .get_or_insert_with(HashSet::new)
                .extend(dirs);
        }
    }
}

/// Whether a scan runs, and whether another pass was requested meanwhile.
#[derive(Debug, Default)]
pub(crate) struct ScanState {
    /// The running scan.
    running: Option<ScanId>,
    rerun: bool,
    /// Requests coalesced into the running scan.
    coalesced: u32,
    /// The last id handed out.
    last_id: u64,
    scope: ScanScope,
}

impl ScanState {
    /// Marks the scan finished (the id counter stays).
    fn finish(&mut self) {
        self.running = None;
        self.rerun = false;
        self.coalesced = 0;
    }
}

pub(crate) fn start(
    inner: Arc<Inner>,
    progress: impl Fn(ScanProgress) + Send + 'static,
) -> Result<ScanStart> {
    start_scoped(inner, None, progress)
}

pub(crate) fn start_scoped(
    inner: Arc<Inner>,
    roots: Option<HashSet<RootId>>,
    progress: impl Fn(ScanProgress) + Send + 'static,
) -> Result<ScanStart> {
    start_scope(
        inner,
        ScanScope {
            roots,
            directories: None,
        },
        progress,
    )
}

pub(crate) fn start_directories(
    inner: Arc<Inner>,
    directories: Vec<PathBuf>,
    progress: impl Fn(ScanProgress) + Send + 'static,
) -> Result<ScanStart> {
    start_scope(
        inner,
        ScanScope {
            roots: None,
            directories: Some(directories.into_iter().collect()),
        },
        progress,
    )
}

fn start_scope(
    inner: Arc<Inner>,
    scope: ScanScope,
    progress: impl Fn(ScanProgress) + Send + 'static,
) -> Result<ScanStart> {
    let id = {
        let mut state = inner.scan_state.lock().unwrap_or_else(|e| e.into_inner());
        inner.check_open()?;
        if let Some(running) = state.running {
            if state.rerun {
                state.scope.merge(scope);
            } else {
                state.scope = scope;
            }
            state.rerun = true;
            state.coalesced += 1;
            return Ok(ScanStart::Coalesced(running));
        }
        state.last_id += 1;
        let id = ScanId(state.last_id);
        state.running = Some(id);
        state.scope = ScanScope::default();
        id
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let thread_inner = inner.clone();
    let spawned = spawn_scan_thread(&inner, move || {
        let inner = thread_inner;
        let mut claim = ScanClaim {
            inner: &inner,
            released: false,
        };
        let mut scanner = Scanner {
            inner: &inner,
            cancel: &flag,
            report: Box::new(progress),
            progress: ScanProgress::default(),
            first_errors: Vec::new(),
            diagnostics: crate::ScanDiagnostics::default(),
            audio_paths: HashMap::new(),
            observed: Arc::default(),
            passes: 0,
            last_report: Instant::now(),
            batch: ScanBatch::default(),
            batch_root: None,
            rescan_request: None,
            root_skipped: false,
            scope,
        };
        scanner.run(id, &mut claim)
    });
    match spawned {
        Ok(thread) => Ok(ScanStart::Started(ScanHandle { id, cancel, thread })),
        Err(err) => {
            inner
                .scan_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .finish();
            Err(err.into())
        }
    }
}

fn spawn_scan_thread(
    inner: &Inner,
    body: impl FnOnce() -> Result<ScanReport> + Send + 'static,
) -> std::io::Result<JoinHandle<Result<ScanReport>>> {
    #[cfg(all(test, feature = "desktop-fs"))]
    if inner.hooks.fail_spawn.load(Ordering::Relaxed) {
        return Err(std::io::Error::other("injected spawn failure"));
    }
    let _ = inner;
    std::thread::Builder::new()
        .name("library-scan".into())
        .spawn(body)
}

/// The running scan's hold on [`ScanState`]. Released either by
/// [`ScanClaim::finish_or_rerun`] or, on error, cancel or panic, when dropped.
struct ScanClaim<'a> {
    inner: &'a Inner,
    released: bool,
}

impl ScanClaim<'_> {
    /// At the end of a pass: `Err(coalesced)` if another pass was requested meanwhile (and
    /// clears the request); otherwise marks the scan finished, so later requests start a
    /// new scan, and returns how many requests were coalesced into it.
    fn finish_or_rerun(&mut self) -> std::result::Result<u32, ScanScope> {
        let mut state = self
            .inner
            .scan_state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if state.rerun {
            state.rerun = false;
            Err(std::mem::take(&mut state.scope))
        } else {
            let coalesced = state.coalesced;
            state.finish();
            self.released = true;
            Ok(coalesced)
        }
    }

    /// Requests coalesced so far (for a scan that ends early).
    fn coalesced(&self) -> u32 {
        self.inner
            .scan_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .coalesced
    }
}

impl Drop for ScanClaim<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.inner
                .scan_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .finish();
        }
    }
}

fn is_sheet(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cue"))
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            AUDIO_EXTENSIONS
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
}

/// The folder part of a `/`-separated relative path ("" for files directly in the root).
fn parent_dir(rel_path: &str) -> &str {
    rel_path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// `path` relative to `root`, `/`-separated. `None` for non-Unicode names.
pub(crate) fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut out = String::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return None;
        };
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(part.to_str()?);
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests;

mod scanner;
use self::scanner::audio_error_code;
#[cfg(all(test, feature = "desktop-fs"))]
use self::scanner::diagnostic;
use self::scanner::missing_diagnostic;
use self::scanner::read_audio;

mod walk;

mod cue_plan;
use self::cue_plan::usable_cuts;

pub(crate) use self::cue_plan::SupplementStamp;
use self::scanner::Scanner;
#[cfg(all(test, feature = "desktop-fs"))]
use self::walk::WalkCompleteness;
use self::walk::{Found, Walked};
