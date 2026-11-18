//! One bounded, cancellable background scan. Only the actor emits public events.
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::actor::Msg;
use crate::catalog::Catalog;
use crate::contract::{
    ContractError, ErrorCode, JobId, JobOutcome, JobReport, TagFileResult, TrackId,
};
use crossbeam_channel::Sender;
use mpmp_media_io::SourceResolver;

/// How often an automatic pass reports progress and newly saved values (as a library
/// scan reports its progress); the last of each is always sent.
const EVERY: Duration = Duration::from_millis(100);

/// The longest wait before a track that could not be measured is tried again.
const MAX_RETRY: Duration = Duration::from_secs(30 * 60);

/// The first wait (doubling with each further failure of the same file version).
pub(crate) const RETRY: Duration = Duration::from_secs(30);

/// Shared by the loudness and waveform threads: one analysis decode at a time, and the
/// tracks that could not be measured, by file version, so that a file that does not
/// change is not decoded again before its retry is due. Kept in memory only: after a
/// restart each is tried once more.
pub(crate) struct Analysis {
    pub gate: Mutex<()>,
    failures: Mutex<Failures>,
    retry: Duration,
}

#[derive(Default)]
struct Failures {
    tracks: HashMap<TrackId, Failure>,
    passes: u64,
}

struct Failure {
    version: String,
    code: ErrorCode,
    attempts: u32,
    retry_at: Instant,
    /// The last automatic pass that came to the track.
    pass: u64,
}

impl Analysis {
    pub fn new(retry: Duration) -> Self {
        Self {
            gate: Mutex::new(()),
            failures: Mutex::default(),
            retry,
        }
    }

    fn failures(&self) -> MutexGuard<'_, Failures> {
        self.failures.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn next_pass(&self) -> u64 {
        let mut failures = self.failures();
        failures.passes += 1;
        failures.passes
    }

    /// The code of this version's last failure while its retry is not due: not decoded
    /// again. `pass`: the automatic pass looking at it (it still needs one).
    fn deferred(&self, track: TrackId, version: &str, pass: Option<u64>) -> Option<ErrorCode> {
        let mut failures = self.failures();
        let failure = failures.tracks.get_mut(&track)?;
        if failure.version != version {
            // Another version of the file: a fresh start.
            failures.tracks.remove(&track);
            return None;
        }
        if let Some(pass) = pass {
            failure.pass = pass;
        }
        (Instant::now() < failure.retry_at).then_some(failure.code)
    }

    /// Records a failed attempt; true for the first failure of this version.
    fn failed(&self, track: TrackId, version: &str, code: ErrorCode, pass: u64) -> bool {
        let mut failures = self.failures();
        let (attempts, pass) = match failures.tracks.get(&track) {
            Some(f) if f.version == version => (
                f.attempts.saturating_add(1),
                if pass == 0 { f.pass } else { pass },
            ),
            // Waveform warming belongs to the latest pass for cleanup purposes too.
            _ => (1, if pass == 0 { failures.passes } else { pass }),
        };
        let wait = self
            .retry
            .saturating_mul(1u32 << (attempts - 1).min(16))
            .min(MAX_RETRY.max(self.retry));
        failures.tracks.insert(
            track,
            Failure {
                version: version.to_owned(),
                code,
                attempts,
                retry_at: Instant::now() + wait,
                pass,
            },
        );
        attempts == 1
    }

    fn measured(&self, track: TrackId) {
        self.failures().tracks.remove(&track);
    }

    /// When the earliest failure is due again. After a complete pass, a failure it did not
    /// come to is forgotten (the track is gone, or was measured some other way).
    fn finish_pass(&self, pass: u64, complete: bool) -> Option<Instant> {
        let mut failures = self.failures();
        if complete {
            failures.tracks.retain(|_, f| f.pass == pass);
        }
        failures.tracks.values().map(|f| f.retry_at).min()
    }
}

/// What an automatic pass tells the actor beyond its report.
#[derive(Debug, Default)]
pub(crate) struct Followup {
    /// When a track that failed is due again.
    pub retry_at: Option<Instant>,
    /// A file was found changed or missing for the first time: the library should read
    /// the folders again before that retry.
    pub rescan: bool,
}

/// What automatic measurement did with one track.
enum Automatic {
    Saved {
        silent: bool,
    },
    /// Nothing to do: measured meanwhile, no longer current, or cancelled.
    Skipped,
    /// It failed before, and that version of its file is not due again: not decoded.
    Deferred(ErrorCode),
    /// It failed now; `first`: the first failure of this version of its file.
    Failed {
        code: ErrorCode,
        first: bool,
    },
}

/// An automatic pass's counts, and what it saved that is not announced yet.
struct Tally {
    measured: u32,
    silent: u32,
    albums: u32,
    failed: Vec<TagFileResult>,
    rescan: bool,
    changed: Vec<TrackId>,
    announced: Instant,
}

impl Default for Tally {
    fn default() -> Self {
        Self {
            measured: 0,
            silent: 0,
            albums: 0,
            failed: Vec::new(),
            rescan: false,
            changed: Vec::new(),
            announced: Instant::now(),
        }
    }
}

impl Tally {
    fn note(&mut self, track: TrackId, result: Automatic) {
        let code = match result {
            Automatic::Saved { silent } => {
                self.measured = self.measured.saturating_add(1);
                self.silent += u32::from(silent);
                self.changed.push(track);
                return;
            }
            Automatic::Skipped => return,
            Automatic::Deferred(code) => code,
            Automatic::Failed { code, first } => {
                self.rescan |=
                    first && matches!(code, ErrorCode::FileChanged | ErrorCode::FileMissing);
                code
            }
        };
        if self.failed.len() < 1000 {
            self.failed.push(TagFileResult { track, code });
        }
    }

    /// At most one message per `EVERY` (the rest with `last`), not one per track or album:
    /// each makes the actor refresh loaded gains and announce the tracks.
    fn announce(&mut self, actor: &Sender<Msg>, last: bool) {
        if !self.changed.is_empty() && (last || self.announced.elapsed() >= EVERY) {
            let _ = actor.send(Msg::AutomaticLoudnessChanged {
                tracks: std::mem::take(&mut self.changed),
                warmed: false,
            });
            self.announced = Instant::now();
        }
    }
}

/// An automatic pass's progress, sent at most once per `EVERY` (and with `last`).
struct Progress {
    done: u32,
    sent: u32,
    at: Instant,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            done: 0,
            sent: 0,
            at: Instant::now(),
        }
    }
}

impl Progress {
    fn send(&mut self, ctx: &Context, last: bool) {
        if self.done != self.sent && (last || self.at.elapsed() >= EVERY) {
            ctx.send(LoudnessEvent::Progress {
                done: self.done,
                total: None,
            });
            self.sent = self.done;
            self.at = Instant::now();
        }
    }
}

#[derive(Default)]
struct Control {
    cancel: Arc<AtomicBool>,
}

impl Control {
    fn cancel(&self) -> bool {
        !self.cancel.swap(true, Ordering::Relaxed)
    }
}

pub(crate) enum LoudnessEvent {
    Progress {
        done: u32,
        total: Option<u32>,
    },
    Finished {
        outcome: JobOutcome,
        tracks: Vec<TrackId>,
        /// Automatic passes only.
        followup: Followup,
    },
}

/// At most one worker and one job. A completed worker is joined before replacement.
/// Drop requests cancellation and joins; workers never wait for the actor to reply.
pub(crate) struct LoudnessWorker {
    resolver: Arc<dyn SourceResolver>,
    catalog: Arc<dyn Catalog>,
    actor: Sender<Msg>,
    analysis: Arc<Analysis>,
    control: Option<Arc<Control>>,
    thread: Option<JoinHandle<()>>,
}

impl LoudnessWorker {
    pub fn new(
        resolver: Arc<dyn SourceResolver>,
        catalog: Arc<dyn Catalog>,
        actor: Sender<Msg>,
        analysis: Arc<Analysis>,
    ) -> Self {
        Self {
            resolver,
            catalog,
            actor,
            analysis,
            control: None,
            thread: None,
        }
    }
    pub fn start_automatic(&mut self, job: JobId) -> Result<(), ContractError> {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let control = Arc::new(Control::default());
        let ctx = Context {
            resolver: Arc::clone(&self.resolver),
            catalog: Arc::clone(&self.catalog),
            actor: self.actor.clone(),
            control: Arc::clone(&control),
            job,
            pass: self.analysis.next_pass(),
            analysis: Arc::clone(&self.analysis),
        };
        self.thread = Some(
            std::thread::Builder::new()
                .name("core-loudness".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        ctx.run_automatic()
                    }));
                    let (outcome, tracks, followup) = match result {
                        Ok(Ok(result)) => result,
                        Ok(Err(error)) => (
                            JobOutcome::Failed { error },
                            Vec::new(),
                            Followup::default(),
                        ),
                        Err(_) => (
                            JobOutcome::Failed {
                                error: ContractError::failed(
                                    ErrorCode::ScanPanicked,
                                    "loudness worker panicked",
                                ),
                            },
                            Vec::new(),
                            Followup::default(),
                        ),
                    };
                    ctx.send(LoudnessEvent::Finished {
                        outcome,
                        tracks,
                        followup,
                    });
                })
                .map_err(|e| {
                    ContractError::failed(
                        ErrorCode::Io,
                        format!("cannot start loudness worker: {e}"),
                    )
                })?,
        );
        self.control = Some(control);
        Ok(())
    }
    pub fn cancel(&self) -> bool {
        self.control.as_ref().is_some_and(|c| c.cancel())
    }
}

impl Drop for LoudnessWorker {
    fn drop(&mut self) {
        self.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Context {
    resolver: Arc<dyn SourceResolver>,
    catalog: Arc<dyn Catalog>,
    actor: Sender<Msg>,
    control: Arc<Control>,
    job: JobId,
    analysis: Arc<Analysis>,
    /// The automatic pass this is (0: warming for a waveform).
    pass: u64,
}

#[derive(Clone, PartialEq)]
struct Stamp {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

impl Stamp {
    fn read(path: &Path) -> Result<Self, ErrorCode> {
        let path = std::fs::canonicalize(path).map_err(|_| ErrorCode::FileMissing)?;
        let metadata = std::fs::metadata(&path).map_err(|_| ErrorCode::FileMissing)?;
        if !metadata.is_file() {
            return Err(ErrorCode::NotARegularFile);
        }
        Ok(Self {
            path,
            size: metadata.len(),
            modified: metadata.modified().map_err(|_| ErrorCode::Io)?,
        })
    }
    fn checked(path: &Path, expected: mpmp_library::FileVersion) -> Result<Self, ErrorCode> {
        let stamp = Self::read(path)?;
        // As the library's scan stamps files: a time before 1970 is 0.
        let millis = stamp
            .modified
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        if stamp.size != expected.size || i64::try_from(millis).ok() != Some(expected.mtime_ms) {
            return Err(ErrorCode::FileChanged);
        }
        Ok(stamp)
    }
}

struct Measured {
    track: TrackId,
    source: mpmp_library::LoudnessSource,
    loudness: mpmp_engine::Loudness,
    /// Original paths plus resolved stamps detect a changed symlink as well as mtime.
    files: Vec<(PathBuf, Stamp)>,
    waveform: Option<mpmp_engine::Waveform>,
}

impl Measured {
    fn current(&self) -> bool {
        self.files
            .iter()
            .all(|(path, before)| Stamp::read(path).is_ok_and(|after| &after == before))
    }
}

impl Context {
    fn send(&self, event: LoudnessEvent) {
        let _ = self.actor.send(Msg::Loudness {
            job: self.job,
            event,
        });
    }
    fn measure(&self, track: TrackId) -> Result<Option<Measured>, ErrorCode> {
        let _access = self.catalog.file_lease(track).map_err(|e| e.code)?;
        let source = self
            .catalog
            .loudness_source(track)
            .map_err(|e| e.code)?
            .ok_or(ErrorCode::TrackUnavailable)?;
        let uri = self
            .catalog
            .uri_of(track)
            .map_err(|e| e.code)?
            .ok_or(ErrorCode::TrackNotFound)?;
        let path = uri.to_file_path().ok_or(ErrorCode::UnsupportedFormat)?;
        let mut files = vec![(path.clone(), Stamp::checked(&path, source.audio)?)];
        if let Some((path, version)) = &source.sidecar {
            files.push((path.clone(), Stamp::checked(path, *version)?));
        }
        let range = self.catalog.frame_range(track).map_err(|e| e.code)?;
        let audio = mpmp_media_io::open_uri(self.resolver.as_ref(), &uri)
            .map_err(|_| ErrorCode::OpenFailed)?;
        // Waveform and loudness share this decode; analysis uses native PCM for the meter.
        let Some(analysis) = mpmp_engine::analyze_audio(
            audio,
            uri.extension().as_deref(),
            range,
            &self.control.cancel,
            true,
            true,
        )
        .map_err(|e| {
            tracing::debug!(track = track.0, "loudness decode failed: {e}");
            match e {
                mpmp_engine::EngineError::Unsupported(_) => ErrorCode::UnsupportedFormat,
                _ => ErrorCode::DecodeError,
            }
        })?
        else {
            return Ok(None);
        };
        let measured = Measured {
            track,
            source,
            loudness: analysis.loudness.expect("loudness requested"),
            files,
            waveform: analysis.waveform,
        };
        if !measured.current() {
            return Err(ErrorCode::FileChanged);
        }
        Ok(Some(measured))
    }
    /// Measures a track that has no statistics yet, unless it failed before and the same
    /// version of its file is not due for another try.
    fn automatic_track(&self, track: TrackId) -> Automatic {
        let version = match self.catalog.loudness_source(track) {
            Ok(Some(source)) => source.version,
            // Unavailable, hidden, or to be read again by a scan first.
            Ok(None) => return Automatic::Skipped,
            // The library could not tell: recorded under no version, tried again when due.
            Err(_) => String::new(),
        };
        let pass = (self.pass != 0).then_some(self.pass);
        if let Some(code) = self.analysis.deferred(track, &version, pass) {
            return Automatic::Deferred(code);
        }
        match self.measure_automatic(track) {
            Ok(Some(silent)) => {
                self.analysis.measured(track);
                Automatic::Saved { silent }
            }
            Ok(None) => Automatic::Skipped,
            Err(ErrorCode::RootReleased) => Automatic::Skipped,
            Err(code) => {
                // A tag job may already have catalogued the replacement while this
                // decode was reading the old file. Its follow-up pass handles it.
                if code == ErrorCode::FileChanged
                    && !version.is_empty()
                    && self
                        .catalog
                        .loudness_source(track)
                        .is_ok_and(|source| source.is_some_and(|s| s.version != version))
                {
                    return Automatic::Skipped;
                }
                Automatic::Failed {
                    code,
                    first: self.analysis.failed(track, &version, code, self.pass),
                }
            }
        }
    }
    fn measure_automatic(&self, track: TrackId) -> Result<Option<bool>, ErrorCode> {
        let _analysis = self.analysis.gate.lock().unwrap_or_else(|p| p.into_inner());
        if self.control.cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        // Rechecked under the gate: a concurrent waveform request may have measured it.
        if let Some(statistics) = self
            .catalog
            .loudness_statistics(track)
            .map_err(|e| e.code)?
            && let Some(info) = self.catalog.loudness(track).map_err(|e| e.code)?
            && mpmp_engine::Loudness::from_statistics(&statistics, info.sample_peak).is_some()
        {
            return Ok(None);
        }
        let Some(m) = self.measure(track)? else {
            return Ok(None);
        };
        if self.control.cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let silent = m.loudness.integrated_lufs.is_none();
        let committed = self
            .catalog
            .store_automatic_loudness(vec![mpmp_library::LoudnessUpdate {
                track: mpmp_library::TrackId(track.0),
                version: m.source.version.clone(),
                integrated_lufs: m.loudness.integrated_lufs,
                sample_peak: m.loudness.sample_peak,
                statistics: Some(m.loudness.statistics()),
            }])
            .map_err(|e| e.code)?;
        if committed.tracks.is_empty() {
            return Ok(None);
        }
        self.keep_waveform(m);
        Ok(Some(silent))
    }
    fn keep_waveform(&self, m: Measured) {
        if let Some(waveform) = m.waveform
            && let Ok(Some(current)) = self.catalog.loudness_source(m.track)
            && current.version == m.source.version
            && let Ok(Some(version)) = self.catalog.file_version(m.track)
        {
            let mtime = m
                .source
                .sidecar
                .as_ref()
                .map_or(m.source.audio.mtime_ms, |(_, v)| {
                    v.mtime_ms.max(m.source.audio.mtime_ms)
                });
            if version.size == m.source.audio.size && version.mtime_ms == mtime {
                let waveform = crate::waveform_worker::contract_waveform(waveform);
                if let Ok(true) = self.catalog.store_waveform(m.track, version, &waveform) {
                    let _ = self.actor.send(Msg::WaveformReady(m.track));
                }
            }
        }
    }
    /// Pages through the library: measures the tracks without statistics, then stores the
    /// aggregate of every album that lacks one and whose members are all measured. Saved
    /// values are announced in batches (`EVERY`), progress likewise.
    fn run_automatic(&self) -> Result<(JobOutcome, Vec<TrackId>, Followup), ContractError> {
        let mut after = 0;
        let mut tally = Tally::default();
        let mut albums = BTreeSet::new();
        let mut progress = Progress::default();
        self.send(LoudnessEvent::Progress {
            done: 0,
            total: None,
        });
        loop {
            if self.control.cancel.load(Ordering::Relaxed) {
                return Ok(self.cancelled(&mut tally));
            }
            let page = self.catalog.automatic_loudness_tracks(after)?;
            if page.is_empty() {
                break;
            }
            for candidate in page {
                after = candidate.track.0;
                if self.control.cancel.load(Ordering::Relaxed) {
                    break;
                }
                let track = TrackId(candidate.track.0);
                if (candidate.album_pending || !candidate.measured)
                    && !candidate.album.is_unknown_album()
                {
                    albums.insert(candidate.album.0);
                }
                if !candidate.measured {
                    tally.note(track, self.automatic_track(track));
                }
                progress.done = progress.done.saturating_add(1);
                progress.send(self, false);
                tally.announce(&self.actor, false);
            }
        }
        progress.send(self, true);
        for key in albums {
            if self.control.cancel.load(Ordering::Relaxed) {
                return Ok(self.cancelled(&mut tally));
            }
            self.aggregate(&mpmp_library::AlbumKey(key), &mut tally)?;
            tally.announce(&self.actor, false);
        }
        tally.announce(&self.actor, true);
        let followup = Followup {
            retry_at: self.analysis.finish_pass(self.pass, true),
            rescan: tally.rescan,
        };
        Ok((
            JobOutcome::Completed {
                report: JobReport::LoudnessScan {
                    measured_count: tally.measured,
                    album_count: tally.albums,
                    silent_count: tally.silent,
                    failed: tally.failed,
                },
            },
            vec![],
            followup,
        ))
    }
    /// What was saved before a cancellation stays saved, and is announced.
    fn cancelled(&self, tally: &mut Tally) -> (JobOutcome, Vec<TrackId>, Followup) {
        tally.announce(&self.actor, true);
        let followup = Followup {
            retry_at: self.analysis.finish_pass(self.pass, false),
            rescan: tally.rescan,
        };
        (JobOutcome::Cancelled, vec![], followup)
    }
    /// Stores an album's aggregate from its members' saved statistics when every current
    /// member has them. A member whose statistics do not read back is measured again
    /// (only it is decoded), once.
    fn aggregate(
        &self,
        album: &mpmp_library::AlbumKey,
        tally: &mut Tally,
    ) -> Result<(), ContractError> {
        for attempt in 0..2 {
            let members = self.catalog.pending_album_loudness(album)?;
            if members.is_empty() {
                return Ok(());
            }
            let mut merged: Option<mpmp_engine::Loudness> = None;
            let mut unreadable = None;
            for (id, _, peak) in &members {
                let stats = self.catalog.loudness_statistics(TrackId(id.0))?;
                let Some(stats) =
                    stats.and_then(|s| mpmp_engine::Loudness::from_statistics(&s, *peak))
                else {
                    unreadable = Some(TrackId(id.0));
                    break;
                };
                if let Some(combined) = &mut merged {
                    combined.merge_statistics(&stats).map_err(|e| {
                        ContractError::failed(ErrorCode::DecodeError, e.to_string())
                    })?;
                } else {
                    merged = Some(stats);
                }
            }
            if let Some(track) = unreadable {
                let measured = self.automatic_track(track);
                let saved = matches!(measured, Automatic::Saved { .. });
                tally.note(track, measured);
                if saved && attempt == 0 {
                    continue;
                }
                return Ok(());
            }
            let Some(merged) = merged else {
                return Ok(());
            };
            let Some(integrated_lufs) = merged.integrated_lufs else {
                return Ok(());
            };
            let ids: Vec<_> = members.iter().map(|(id, _, _)| TrackId(id.0)).collect();
            let update = mpmp_library::AlbumLoudnessUpdate {
                album: album.clone(),
                members: members.into_iter().map(|(id, v, _)| (id, v)).collect(),
                integrated_lufs,
                sample_peak: merged.sample_peak,
            };
            if self.catalog.store_automatic_album_loudness(update)? {
                tally.albums += 1;
                tally.changed.extend(ids);
            }
            return Ok(());
        }
        Ok(())
    }
}

/// Warm missing automatic RG with a waveform request under the same decode gate.
/// A concurrent automatic pass rechecks the cache under this gate and skips its decode;
/// a track that failed before is not decoded again until its retry is due.
pub(crate) fn warm_automatic_track(
    resolver: Arc<dyn SourceResolver>,
    catalog: Arc<dyn Catalog>,
    actor: Sender<Msg>,
    analysis: Arc<Analysis>,
    cancel: Arc<AtomicBool>,
    track: TrackId,
) {
    let ctx = Context {
        resolver,
        catalog,
        actor,
        analysis,
        pass: 0,
        job: JobId(0),
        control: Arc::new(Control { cancel }),
    };
    match ctx.automatic_track(track) {
        Automatic::Saved { .. } => {
            // Its album's aggregate may now be complete: the actor schedules a pass.
            let _ = ctx.actor.send(Msg::AutomaticLoudnessChanged {
                tracks: vec![track],
                warmed: true,
            });
        }
        Automatic::Failed {
            code: ErrorCode::FileChanged | ErrorCode::FileMissing,
            first: true,
        } => {
            let _ = ctx.actor.send(Msg::AutomaticRescan);
        }
        _ => {}
    }
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::*;
    use crate::catalog::PlayRecord;
    use crate::contract::{PlayContext, Query, QueryResult, TrackInfo};
    use crossbeam_channel::unbounded;

    #[test]
    fn waveform_failure_keeps_the_pass_and_its_retry_after_a_complete_traversal() {
        let analysis = Analysis::new(RETRY);
        let track = TrackId(1);
        let gone = TrackId(2);
        let old_pass = analysis.next_pass();
        analysis.failed(track, "v1", ErrorCode::OpenFailed, old_pass);
        analysis.failed(gone, "gone", ErrorCode::OpenFailed, old_pass);
        let pass = analysis.next_pass();
        analysis.failures().tracks.get_mut(&track).unwrap().retry_at = Instant::now();
        assert_eq!(analysis.deferred(track, "v1", Some(pass)), None);
        // The traversal visited this track, then waveform warming retried it before
        // the traversal finished the rest of the library.
        assert!(!analysis.failed(track, "v1", ErrorCode::DecodeError, 0));
        let retry = analysis.finish_pass(pass, true).unwrap();
        let failures = analysis.failures();
        let failure = &failures.tracks[&track];
        assert_eq!(failure.attempts, 2);
        assert_eq!(failure.code, ErrorCode::DecodeError);
        assert_eq!(failure.retry_at, retry);
        assert!(retry > Instant::now() + RETRY);
        assert!(!failures.tracks.contains_key(&gone));
    }

    #[test]
    fn first_waveform_failure_during_a_pass_survives_until_the_next_pass() {
        let analysis = Analysis::new(RETRY);
        let pass = analysis.next_pass();
        assert!(analysis.failed(TrackId(1), "v1", ErrorCode::OpenFailed, 0));
        assert!(analysis.finish_pass(pass, true).is_some());
        // If the next traversal never visits it, it has gone away.
        let next = analysis.next_pass();
        assert!(analysis.finish_pass(next, true).is_none());
    }

    struct Unreadable(ErrorCode);
    impl crate::catalog::Browse for Unreadable {
        fn tracks(&self, _: &[TrackId]) -> Result<Vec<TrackInfo>, ContractError> {
            Ok(vec![])
        }

        fn uri_of(&self, _: TrackId) -> Result<Option<mpmp_media_io::MediaUri>, ContractError> {
            Ok(None)
        }

        fn context_tracks(&self, _: &PlayContext) -> Result<Vec<TrackId>, ContractError> {
            Ok(vec![])
        }

        fn query(&self, _: &Query) -> QueryResult {
            QueryResult::Unknown
        }
    }

    impl crate::catalog::Playlists for Unreadable {}

    impl crate::catalog::LibraryRoots for Unreadable {}

    impl crate::catalog::Actions for Unreadable {}

    impl crate::catalog::CatalogChecks for Unreadable {}

    impl crate::catalog::AgentPermissions for Unreadable {}

    impl crate::catalog::MediaResources for Unreadable {}

    impl crate::catalog::Loudness for Unreadable {
        fn loudness_source(
            &self,
            _: TrackId,
        ) -> Result<Option<mpmp_library::LoudnessSource>, ContractError> {
            Err(ContractError::new(
                self.0.kind(),
                self.0,
                "file no longer matches the catalog",
            ))
        }
    }

    impl crate::catalog::TagsAndCovers for Unreadable {}

    impl crate::catalog::PlaybackHistory for Unreadable {
        fn record_play(&self, _: PlayRecord) {}
    }

    #[test]
    fn first_waveform_file_failure_requests_a_rescan_once() {
        for code in [
            ErrorCode::FileChanged,
            ErrorCode::FileMissing,
            ErrorCode::DecodeError,
        ] {
            let (actor, events) = unbounded();
            let analysis = Arc::new(Analysis::new(RETRY));
            let warm = || {
                warm_automatic_track(
                    Arc::new(mpmp_media_io::FileResolver),
                    Arc::new(Unreadable(code)),
                    actor.clone(),
                    Arc::clone(&analysis),
                    Arc::new(AtomicBool::new(false)),
                    TrackId(1),
                )
            };
            warm();
            if matches!(code, ErrorCode::FileChanged | ErrorCode::FileMissing) {
                assert!(matches!(events.try_recv(), Ok(Msg::AutomaticRescan)));
            }
            assert!(events.is_empty());
            warm();
            assert!(
                events.is_empty(),
                "deferred failure must not schedule repeated rescans"
            );
            assert_eq!(analysis.failures().tracks[&TrackId(1)].attempts, 1);
        }
    }
}
