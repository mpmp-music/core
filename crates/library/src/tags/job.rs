//! Tag edits and their undos as jobs (docs/design/media-resources.md): files are
//! read and written on a thread of their own, one job at a time; what was written is read
//! again. A durable intent precedes each publication. The track rows, action and intent
//! deletion commit in one transaction when the job ends.
//!
//! An edit checks the library's size and modification time, and a byte undo also checks
//! the current whole-file digest. A same-directory candidate is verified before publication.
//! Publication retains the displaced file until both versions are verified. Readers see
//! complete files, and a conflicting save can be put back. Checked callers also require
//! the authorized canonical path ([`ActionMeta::checked`]). File permissions are copied.
//!
//! Per file, the codes a report lists ([`TAG_FILE_CODES`]): skipped
//! `track_unavailable`, `cue_track` (a track of a CUE sheet: its tags come from the sheet,
//! not written yet), `rescan_needed`, `file_missing`, `file_changed`, `file_in_use`,
//! `unsupported_format`; failed `open_failed` (it could not be read), `invalid_tag`,
//! `not_undoable`, `undo_too_large`, `write_failed`, `location_changed`. A file that
//! already has the values asked for is not written, and not listed.
//!
//! An edit proves it can reconstruct the original file from the candidate and its
//! metadata delta (else `not_undoable`). Both directions count toward the per-file and
//! job budgets (else `undo_too_large`). A target left unchanged by a late undo failure
//! remains in the source action's inverse ([`journal::record_tag_undo`]).

use std::collections::{HashMap, HashSet};

use std::path::{Path, PathBuf};

use std::sync::Arc;

use std::thread::JoinHandle;

use std::time::{Duration, Instant, UNIX_EPOCH};

use encoding_rs::Encoding;

use crate::db::writer::{TrackRecord, now_ms};

use crate::handle::Inner;

use crate::journal::{self, ActionId, ActionMeta, FileTags, OwnedMeta};

use crate::storage_access;

use crate::tags::edit::{self as tag_edit, OpenError, TagChange, TagValues};

use crate::tags::encoding::encoding_for_label;

use crate::tags::read;

use crate::types::{RootId, Track, TrackId};

use crate::{LibraryError, NotFound, Result, db::reads, db::schema};

/// Every code a tag job (or a preview) lists a file with (the module docs); each is one of
/// the contract's error codes.
pub const TAG_FILE_CODES: &[&str] = &[
    "cover_recovery_required",
    "track_unavailable",
    "cue_track",
    "rescan_needed",
    "file_missing",
    "file_changed",
    "file_in_use",
    "root_released",
    "unsupported_format",
    "open_failed",
    "invalid_tag",
    "not_undoable",
    "undo_too_large",
    "write_failed",
    "file_read_only",
    "location_changed",
];

/// The files a tag job must not write now (the player's open files: `file_in_use`).
pub type InUse = Arc<dyn Fn(TrackId) -> bool + Send + Sync>;

/// What a tag job needs besides its changes.
pub struct TagJobOptions {
    /// Approved cover sources, checked again after entering the file-write scheduler.
    pub cover_sources: Option<Vec<crate::CoverSource>>,
    /// Files not to write now ([`InUse`]).
    pub in_use: InUse,
    /// A trusted host's frozen undo; checked after acquiring the tag-job gate.
    pub undo_guard: Option<crate::UndoDescription>,
    /// For a caller that acts only where it checked ([`ActionMeta::checked`]): each track's
    /// file in canonical form as checked. A file that is elsewhere now, or not in this map,
    /// is not written (`location_changed`).
    pub checked: Option<HashMap<TrackId, PathBuf>>,
    /// `(done, total)` files: about ten times a second at most, and once at the end.
    pub progress: Box<dyn Fn(u64, u64) + Send>,
}

impl Default for TagJobOptions {
    fn default() -> Self {
        Self {
            cover_sources: None,
            in_use: Arc::new(|_| false),
            undo_guard: None,
            checked: None,
            progress: Box::new(|_, _| {}),
        }
    }
}

/// What a tag job did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagEditReport {
    /// Logically completed targets.
    pub completed: Vec<TrackId>,
    /// Tracks whose files were physically written, in the order asked.
    pub written: Vec<TrackId>,
    /// Written files whose legacy text was converted to Unicode.
    pub encoding_converted: Vec<TrackId>,
    /// The action recording completed targets (the undo, for an undo).
    pub action: Option<ActionId>,
    /// Files not written, by code (the module docs).
    pub skipped: Vec<(TrackId, &'static str)>,
    /// Files that could not be read or written, by code.
    pub failed: Vec<(TrackId, &'static str)>,
}

/// One file of a [`TagPreview`]: its tags as the library shows them, and after the edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagFilePreview {
    pub track: TrackId,
    pub before: TagValues,
    pub after: TagValues,
    pub encoding_conversion: bool,
    pub changed: bool,
    /// Canonical file selected by the platform resolver for this preview.
    pub display_location: String,
}

/// What an edit would do ([`crate::Library::preview_tag_edit`]): the files it would write
/// (also those it would leave as they are: `before == after`), and those it would skip,
/// by code (as a job reports them). Player-owned files are checked when the job runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagPreview {
    pub files: Vec<TagFilePreview>,
    pub skipped: Vec<(TrackId, &'static str)>,
}

/// A running tag job.
#[derive(Debug)]
pub struct TagJob {
    thread: JoinHandle<Result<TagEditReport>>,
}

impl TagJob {
    /// Waits for the job and makes its committed writes visible to reads.
    /// An undo preflight error such as `action_conflict` or `file_in_use` prevents
    /// all writes. Occupancy found during publication appears in the per-file
    /// skipped list. A database error can occur after files have been written.
    pub fn wait(self) -> Result<TagEditReport> {
        self.thread.join().map_err(|_| LibraryError::ScanPanicked)?
    }
}

const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// A track's file as the database knows it.
#[derive(Clone)]
pub(crate) struct FileRef {
    pub(crate) track: Track,
    /// Its root's path and encoding for Latin-1 ID3 text; `None`: the root is removed.
    pub(crate) root: Option<(PathBuf, Option<&'static Encoding>)>,
}

/// The tracks `ids` (each once, in order); an unknown one is `track_not_found`.
pub(crate) fn file_refs(inner: &Inner, ids: &[TrackId]) -> Result<Vec<FileRef>> {
    inner.pool.read(|conn| {
        let tx = conn.unchecked_transaction()?;
        let refs = refs_from(&tx, ids)?;
        tx.commit()?;
        let known: HashSet<_> = refs.iter().map(|f| f.track.id).collect();
        if let Some(unknown) = ids.iter().find(|id| !known.contains(id)) {
            return Err(LibraryError::not_found(
                NotFound::Track,
                format!("track {unknown}"),
            ));
        }
        Ok(refs)
    })
}

fn refs_from(conn: &rusqlite::Connection, ids: &[TrackId]) -> Result<Vec<FileRef>> {
    let tracks = reads::tracks(conn, ids)?;
    let roots = reads::roots(conn)?;
    let roots: HashMap<RootId, (PathBuf, Option<&'static Encoding>)> = roots
        .into_iter()
        .map(|r| {
            let forced = r.forced_encoding.as_deref().and_then(encoding_for_label);
            (r.id, (r.path, forced))
        })
        .collect();
    let mut seen = HashSet::new();
    Ok(tracks
        .into_iter()
        .filter(|t| seen.insert(t.id))
        .map(|track| FileRef {
            root: roots.get(&track.root_id).cloned(),
            track,
        })
        .collect())
}

/// A file's size and modification time (Unix ms, as a scan reads it).
pub(crate) fn stamp(meta: &std::fs::Metadata) -> (u64, i64) {
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    (meta.len(), mtime_ms)
}

/// Where a track's file is now, and how it is.
pub(crate) enum Located {
    File { path: PathBuf, stamp: (u64, i64) },
    Skip(&'static str),
    Fail(&'static str),
}

pub(crate) fn locate(
    storage: &dyn mpmp_storage::LibraryStorage,
    file: &FileRef,
    checked: Option<&HashMap<TrackId, PathBuf>>,
) -> Located {
    // A track of a CUE sheet shares its file with the sheet's other tracks, and its tags
    // come from the sheet: writing them belongs in the sheet (not yet done).
    if file.track.cue.is_some() {
        return Located::Skip("cue_track");
    }
    let Some((root, _)) = file.root.as_ref().filter(|_| file.track.available) else {
        return Located::Skip("track_unavailable");
    };
    if file.track.mtime_ms == -1 {
        return Located::Skip("rescan_needed");
    }
    let mut path = root.clone();
    path.extend(file.track.rel_path.split('/'));
    let path = match storage_access::canonical(storage, &path) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Located::Skip("file_missing"),
        Err(e) if write::is_file_in_use(&e) => return Located::Skip("file_in_use"),
        Err(_) => return Located::Fail("open_failed"),
    };
    if let Some(checked) = checked
        && checked.get(&file.track.id) != Some(&path)
    {
        return Located::Fail("location_changed");
    }
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => Located::File {
            stamp: stamp(&meta),
            path,
        },
        Ok(_) => Located::Fail("open_failed"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Located::Skip("file_missing"),
        Err(e) if write::is_file_in_use(&e) => Located::Skip("file_in_use"),
        Err(_) => Located::Fail("open_failed"),
    }
}

/// A file opened to edit, or why not.
fn open_code(error: OpenError) -> &'static str {
    match error {
        OpenError::Unsupported => "unsupported_format",
        OpenError::Unreadable(message) => {
            tracing::warn!(%message, "cannot read a file to edit its tags");
            "open_failed"
        }
    }
}

/// Refuses file jobs before reads, scheduling or journal validation can mutate state.
pub(crate) fn require_tag_write(inner: &Inner) -> Result<()> {
    let caps = inner.storage.files.write_capabilities();
    if !caps.durable_replace
        || !caps.legacy_tag_write
        || !inner.storage.replacements.provides_tag_publish()
    {
        return Err(mpmp_storage::StorageError::Unsupported {
            operation: mpmp_storage::Operation::TagWrite,
        }
        .into());
    }
    Ok(())
}

/// Shows what an edit would do; reads the files on the caller's thread.
pub(crate) fn preview(
    inner: &Inner,
    tracks: &[TrackId],
    changes: &[TagChange],
) -> Result<TagPreview> {
    require_tag_write(inner)?;
    inner.pool.read(intents::require_readable)?;
    let changes = checked_changes(changes)?;
    let mut out = TagPreview::default();
    let mut undo_left = tag_edit::MAX_TAG_JOB_BYTES;
    let mut storage_left = inner.pool.read(|conn| intents::budget(conn, false))?;
    for file in file_refs(inner, tracks)? {
        let id = file.track.id;
        let Some(_access) = inner.file_access.acquire(file.track.root_id) else {
            out.skipped.push((id, "root_released"));
            continue;
        };
        let path = match locate(inner.storage.files.as_ref(), &file, None) {
            Located::File { path, stamp } if stamp == (file.track.size, file.track.mtime_ms) => {
                path
            }
            Located::File { .. } => {
                out.skipped.push((id, "file_changed"));
                continue;
            }
            Located::Skip(code) | Located::Fail(code) => {
                out.skipped.push((id, code));
                continue;
            }
        };
        if intents::pending(inner, id, &path)? {
            out.skipped.push((id, "file_changed"));
            continue;
        }
        let forced = file.root.as_ref().and_then(|(_, f)| *f);
        match preview_file(
            inner,
            &path,
            forced,
            Mode::Edit(&changes),
            undo_left.min(storage_left),
        ) {
            Ok((before, after, bytes, encoding_conversion)) => {
                undo_left -= bytes;
                storage_left -= bytes;
                out.files.push(TagFilePreview {
                    track: id,
                    before,
                    after,
                    encoding_conversion,
                    changed: bytes != 0,
                    display_location: path.to_string_lossy().into_owned(),
                });
            }
            Err(rejected) => out.skipped.push((id, rejected.code())),
        }
    }
    Ok(out)
}

fn preview_file(
    inner: &Inner,
    path: &Path,
    forced: Option<&'static Encoding>,
    mode: Mode<'_>,
    limit: usize,
) -> std::result::Result<(TagValues, TagValues, usize, bool), Rejected> {
    let mut source = open_source(inner.storage.files.as_ref(), path, false)?;
    mode.verify(&mut source, None)?;
    let plan = plan(&mut source, forced, &mode, limit)?;
    if plan.physical() {
        write::volume(inner, path, &source.metadata()?)?;
    }
    Ok(plan.preview())
}

/// [`tag_edit::check_changes`] as a library error.
fn checked_changes(changes: &[TagChange]) -> Result<Vec<TagChange>> {
    tag_edit::check_changes(changes)
        .map_err(|e| LibraryError::invalid(crate::Invalid::InvalidTag, e.to_string()))
}

/// Checks `tracks` (each must exist, at most [`tag_edit::MAX_TAG_EDIT_TRACKS`]) and
/// `changes`, then edits the files on a thread of its own.
pub(crate) fn start_edit(
    inner: &Arc<Inner>,
    tracks: &[TrackId],
    changes: &[TagChange],
    meta: ActionMeta<'_>,
    options: TagJobOptions,
) -> Result<TagJob> {
    let job = inner.register_file_job()?;
    require_tag_write(inner)?;
    check_count(tracks)?;
    let changes = checked_changes(changes)?;
    let files = file_refs(inner, tracks)?;
    let meta = meta.owned();
    let inner = Arc::clone(inner);
    spawn(move || job.run(|| run_edit(&inner, files, &changes, &meta, options)))
}

pub(crate) fn check_count(tracks: &[TrackId]) -> Result<()> {
    if tracks.len() > tag_edit::MAX_TAG_EDIT_TRACKS {
        return Err(LibraryError::invalid(
            crate::Invalid::OutOfRange,
            format!(
                "at most {} tracks in one tag edit",
                tag_edit::MAX_TAG_EDIT_TRACKS
            ),
        ));
    }
    Ok(())
}

/// Previews the effective tag undo and resolves file locations. May probe a directory
/// with disposable files to determine whether it can publish a replacement.
pub(crate) fn preview_undo(inner: &Inner, id: ActionId) -> Result<TagPreview> {
    require_tag_write(inner)?;
    inner.pool.read(intents::require_readable)?;
    let files = inner
        .pool
        .read(|conn| journal::tag_undo(conn, id, now_ms()))?
        .ok_or_else(|| LibraryError::not_found(NotFound::Action, "not a tag undo"))?;
    let ids: Vec<_> = files.iter().map(|f| TrackId(f.track)).collect();
    let refs: HashMap<_, _> = file_refs(inner, &ids)?
        .into_iter()
        .map(|f| (f.track.id, f))
        .collect();
    let mut preview = TagPreview::default();
    let mut undo_left = tag_edit::MAX_TAG_JOB_BYTES;
    let mut storage_left = inner.pool.read(|conn| intents::budget(conn, true))?;
    for undo in files {
        let track = TrackId(undo.track);
        if crate::cover::job::pending(inner, track)? {
            return Err(LibraryError::CoverRecoveryRequired);
        }
        let file = &refs[&track];
        let Some(_access) = inner.file_access.acquire(file.track.root_id) else {
            preview.skipped.push((track, "root_released"));
            continue;
        };
        let path = match locate(inner.storage.files.as_ref(), file, None) {
            Located::File { path, stamp } if stamp == (undo.size, undo.mtime_ms) => path,
            Located::File { .. } => {
                preview.skipped.push((track, "file_changed"));
                continue;
            }
            Located::Skip(code) | Located::Fail(code) => {
                preview.skipped.push((track, code));
                continue;
            }
        };
        if intents::pending(inner, track, &path)? {
            preview.skipped.push((track, "file_changed"));
            continue;
        }
        let forced = file.root.as_ref().and_then(|(_, f)| *f);
        match preview_file(
            inner,
            &path,
            forced,
            Mode::undo(&undo),
            undo_left.min(storage_left),
        ) {
            Ok((before, after, bytes, encoding_conversion)) => {
                undo_left -= bytes;
                storage_left -= bytes;
                preview.files.push(TagFilePreview {
                    track,
                    before,
                    after,
                    changed: true,
                    display_location: path.to_string_lossy().into_owned(),
                    encoding_conversion,
                });
            }
            Err(rejected) => preview.skipped.push((track, rejected.code())),
        }
    }
    Ok(preview)
}

/// Checks that action `id` can be undone now as far as the journal tells (unknown,
/// undone, expired, a later change: [`LibraryError::ActionConflict`]), then writes its old
/// values back on a thread of its own. `None`: it is not a tag edit (nor an undo of one).
pub(crate) fn start_undo(
    inner: &Arc<Inner>,
    id: ActionId,
    meta: ActionMeta<'_>,
    options: TagJobOptions,
) -> Result<Option<TagJob>> {
    let job = inner.register_file_job()?;
    let capabilities = inner.storage.files.write_capabilities();
    if (!capabilities.durable_replace
        || !capabilities.legacy_tag_write
        || !inner.storage.replacements.provides_tag_publish())
        && !tracks_of(inner, id)?.is_empty()
    {
        require_tag_write(inner)?;
    }
    let now = now_ms();
    if inner
        .pool
        .read(|conn| journal::tag_undo(conn, id, now))?
        .is_none()
    {
        return Ok(None);
    }
    require_tag_write(inner)?;
    let meta = meta.owned();
    let inner = Arc::clone(inner);
    spawn(move || job.run(|| run_undo(&inner, id, &meta, options))).map(Some)
}

fn spawn(body: impl FnOnce() -> Result<TagEditReport> + Send + 'static) -> Result<TagJob> {
    let thread = std::thread::Builder::new()
        .name("library-tags".into())
        .spawn(body)?;
    Ok(TagJob { thread })
}

/// Reconcile successful edits and logical undos with the same format-aware parser.
/// A failed read keeps membership and explicitly leaves the parser version pending.
pub(crate) fn reread_record(
    inner: &Inner,
    file: &FileRef,
    path: &Path,
    (size, mtime_ms): (u64, i64),
) -> Option<Box<TrackRecord>> {
    #[cfg(all(test, feature = "desktop-fs"))]
    if inner.hooks.fail_audio_read.lock().unwrap().as_deref() == Some(path) {
        return None;
    }
    let _ = inner;
    read_record(file, path, (size, mtime_ms))
}

fn read_record(
    file: &FileRef,
    path: &Path,
    (size, mtime_ms): (u64, i64),
) -> Option<Box<TrackRecord>> {
    let forced = file.root.as_ref().and_then(|(_, forced)| *forced);
    match read::read_track(path, forced) {
        Ok(meta) => Some(Box::new(TrackRecord {
            root_id: file.track.root_id,
            rel_path: file.track.rel_path.clone(),
            size,
            mtime_ms,
            has_folder_art: file.track.has_folder_art,
            meta,
            cue: None,
            based_on: None,
            based_on_revision: None,
            artists_only: false,
            file_identity: None,
        })),
        Err(err) => {
            tracing::warn!(path = %path.display(),
             %err,
             "cannot read a file again after editing its tags");

            None
        }
    }
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::*;

    /// Every code this module lists a file with (its snake_case string literals) is in
    /// [`TAG_FILE_CODES`]; core checks that each of those is one of the contract's.
    #[test]
    fn every_file_code_is_listed() {
        // The module and its submodules; tests (after the cfg marker) are not scanned.
        let sources = [
            include_str!("job.rs"),
            include_str!("job/run.rs"),
            include_str!("job/undo.rs"),
            include_str!("job/write.rs"),
        ];
        let used: HashSet<&str> = sources
            .iter()
            .map(|source| {
                source
                    .split(r#"#[cfg(all(test, feature = "desktop-fs"))]"#)
                    .next()
                    .unwrap()
            })
            .flat_map(|code| code.split('"').skip(1).step_by(2))
            .filter(|s| {
                s.contains('_')
                    && s.split('_')
                        .all(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase()))
            })
            .collect();
        let listed: HashSet<&str> = TAG_FILE_CODES.iter().copied().collect();
        assert!(used.is_subset(&listed), "{:?}", used.difference(&listed));
    }

    #[test]
    fn summaries_name_the_changes() {
        let set = |k: &str| TagChange::Set {
            key: k.into(),
            values: vec!["x".into()],
        };
        assert_eq!(
            summary(
                1,
                &[
                    set("ARTIST"),
                    TagChange::Remove {
                        key: "COMMENT".into()
                    }
                ]
            ),
            "edit the tags of 1 file: set ARTIST, remove COMMENT"
        );
        let many: Vec<_> = ["A", "B", "C", "D", "E", "F", "G"].map(set).to_vec();
        assert_eq!(
            summary(3, &many),
            "edit the tags of 3 files: set A, set B, set C, set D, set E, and 2 more"
        );
    }
}

mod write;
use self::write::Outcome;
use self::write::Progress;
use self::write::edit_file;
use self::write::summary;
use self::write::{FileWrite, Mode, Rejected, open_source, plan};

pub(crate) mod intents;

mod run;
use self::run::commit;
use self::run::run_edit;

mod undo;
use self::undo::run_undo;
pub(crate) use self::undo::tracks_of;
