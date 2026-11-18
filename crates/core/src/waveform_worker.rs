//! Track waveforms (docs/design/media-resources.md): one background thread decodes
//! tracks with the engine's [`mpmp_engine::compute_waveform`] and keeps the results in the
//! catalog's cache. The tracks being heard and coming next go first, then the tracks
//! queries asked for, in the order asked. A track is queued once however often it is asked
//! for; one whose file cannot be decoded is not tried again while core runs.
//!
//! Only the actor emits events: a finished waveform goes to it as [`Msg::WaveformReady`],
//! and it announces `WaveformReady` (origin `null`).

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use mpmp_engine::EngineError;
use mpmp_media_io::SourceResolver;

use crate::actor::Msg;
use crate::catalog::Catalog;
use crate::contract::{TrackId, Waveform, WaveformChannel};
use crate::loudness_scan::Analysis;

enum Job {
    /// The tracks being heard (or shown as current) and coming next, in that order; they
    /// replace the previous ones.
    Playing(Vec<TrackId>),
    /// A query found no waveform for this track.
    Asked(TrackId),
    Stop,
}

/// Where the handle and the actor send work; cheap to clone.
#[derive(Clone)]
pub(crate) struct WaveformQueue {
    tx: Sender<Job>,
}

impl WaveformQueue {
    /// The tracks to compute first: the current one, then the next.
    pub fn playing(&self, tracks: Vec<TrackId>) {
        let _ = self.tx.send(Job::Playing(tracks));
    }

    /// Queues a track a query asked for (once, however often it is asked for).
    pub fn ask(&self, track: TrackId) {
        let _ = self.tx.send(Job::Asked(track));
    }
}

/// The waveform thread. Dropping it cancels the waveform being computed and waits for the
/// thread to end.
pub(crate) struct WaveformWorker {
    queue: WaveformQueue,
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WaveformWorker {
    /// Starts the thread. Finished waveforms are reported to the actor through `actor`.
    pub fn start(
        resolver: Arc<dyn SourceResolver>,
        catalog: Arc<dyn Catalog>,
        actor: Sender<Msg>,
        analysis: Arc<Analysis>,
        automatic_loudness: bool,
    ) -> std::io::Result<Self> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            rx,
            cancel: Arc::clone(&cancel),
            resolver,
            catalog,
            actor,
            analysis,
            automatic_loudness,
            urgent: VecDeque::new(),
            asked: VecDeque::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
        };
        let thread = std::thread::Builder::new()
            .name("core-waveforms".into())
            .spawn(move || worker.run())?;
        Ok(Self {
            queue: WaveformQueue { tx },
            cancel,
            thread: Some(thread),
        })
    }

    pub fn queue(&self) -> WaveformQueue {
        self.queue.clone()
    }
}

impl Drop for WaveformWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.queue.tx.send(Job::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Worker {
    rx: Receiver<Job>,
    cancel: Arc<AtomicBool>,
    resolver: Arc<dyn SourceResolver>,
    catalog: Arc<dyn Catalog>,
    actor: Sender<Msg>,
    analysis: Arc<Analysis>,
    automatic_loudness: bool,
    /// The current and next tracks still to look at.
    urgent: VecDeque<TrackId>,
    /// Tracks queries asked for, in order; only those still in `pending` count (a track
    /// taken earlier as urgent stays here, skipped).
    asked: VecDeque<TrackId>,
    pending: HashSet<TrackId>,
    /// Tracks whose waveform could not be made or kept: not tried again.
    failed: HashSet<TrackId>,
}

impl Worker {
    fn run(mut self) {
        loop {
            loop {
                match self.rx.try_recv() {
                    Ok(job) => {
                        if !self.take(job) {
                            return;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            match self.next() {
                Some((track, asked)) => self.process(track, asked),
                None => match self.rx.recv() {
                    Ok(job) => {
                        if !self.take(job) {
                            return;
                        }
                    }
                    Err(_) => return,
                },
            }
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
        }
    }

    /// Takes a job in; false to stop.
    fn take(&mut self, job: Job) -> bool {
        match job {
            Job::Stop => return false,
            Job::Playing(tracks) => {
                self.urgent = tracks
                    .into_iter()
                    .filter(|t| !self.failed.contains(t))
                    .collect();
            }
            Job::Asked(track) => {
                if !self.failed.contains(&track) && self.pending.insert(track) {
                    self.asked.push_back(track);
                }
            }
        }
        true
    }

    /// The next track to look at, and whether a query asked for it.
    fn next(&mut self) -> Option<(TrackId, bool)> {
        if let Some(track) = self.urgent.pop_front() {
            let asked = self.pending.remove(&track);
            return Some((track, asked));
        }
        while let Some(track) = self.asked.pop_front() {
            if self.pending.remove(&track) {
                return Some((track, true));
            }
        }
        None
    }

    fn process(&mut self, track: TrackId, asked: bool) {
        // Twice at most: once more when the file was rescanned as another version while
        // its waveform was computed (that one is not kept).
        for _ in 0..2 {
            if !self.process_once(track, asked) {
                return;
            }
        }
    }

    /// Computes and keeps a track's waveform unless it is kept already or failed this run;
    /// `true`: the file changed while it was computed, so it was not kept.
    fn process_once(&mut self, track: TrackId, asked: bool) -> bool {
        if self.failed.contains(&track) {
            return false;
        }
        // A waveform kept already is all there is to do: the track's RG, if missing, comes
        // with the background passes (one that cannot be measured is not decoded for it).
        if let Ok(Some(_)) = self.catalog.waveform(track) {
            if asked {
                self.ready(track);
            }
            return false;
        }
        // Otherwise one decode makes both, when its RG is missing too.
        if self.automatic_loudness {
            crate::loudness_scan::warm_automatic_track(
                Arc::clone(&self.resolver),
                Arc::clone(&self.catalog),
                self.actor.clone(),
                Arc::clone(&self.analysis),
                Arc::clone(&self.cancel),
                track,
            );
        }
        let analysis = Arc::clone(&self.analysis);
        let _analysis = analysis.gate.lock().unwrap_or_else(|p| p.into_inner());
        match self.catalog.waveform(track) {
            // Made meanwhile (or kept from an earlier run): whoever asked is told.
            Ok(Some(_)) => {
                if asked {
                    self.ready(track);
                }
                return false;
            }
            Ok(None) => {}
            Err(err) => {
                tracing::debug!(track = track.0, "no waveform for this track: {err}");
                return false;
            }
        }
        // The version computed from (read first: a rescan meanwhile makes it another).
        let version = match self.catalog.file_version(track) {
            Ok(Some(version)) => version,
            Ok(None) => {
                // A catalog that keeps no waveforms: none is computed.
                self.failed.insert(track);
                return false;
            }
            Err(err) => {
                tracing::debug!(track = track.0, "no waveform for this track: {err}");
                return false;
            }
        };
        let Ok(_access) = self.catalog.file_lease(track) else {
            return false;
        };
        let uri = match self.catalog.uri_of(track) {
            Ok(Some(uri)) => uri,
            Ok(None) => return false,
            Err(err) => {
                tracing::debug!(track = track.0, "no waveform for this track: {err}");
                return false;
            }
        };
        // A track of a CUE sheet: only its part of the file.
        let range = match self.catalog.frame_range(track) {
            Ok(range) => range,
            Err(err) => {
                tracing::debug!(track = track.0, "no waveform for this track: {err}");
                return false;
            }
        };
        let started = Instant::now();
        let computed = mpmp_media_io::open_uri(self.resolver.as_ref(), &uri)
            .map_err(EngineError::from)
            .and_then(|source| {
                mpmp_engine::compute_waveform(
                    source,
                    uri.extension().as_deref(),
                    range,
                    &self.cancel,
                )
            });
        let waveform = match computed {
            Ok(Some(waveform)) => waveform,
            // Cancelled: core is stopping.
            Ok(None) => return false,
            Err(err) => {
                tracing::debug!(track = track.0, %uri, "cannot compute the waveform: {err}");
                self.failed.insert(track);
                return false;
            }
        };
        let waveform = contract_waveform(waveform);
        match self.catalog.store_waveform(track, version, &waveform) {
            Ok(true) => {
                tracing::debug!(track = track.0, took = ?started.elapsed(), "waveform computed");
                self.ready(track);
                false
            }
            Ok(false) => {
                tracing::debug!(
                    track = track.0,
                    "the file changed while its waveform was computed"
                );
                true
            }
            Err(err) => {
                tracing::debug!(track = track.0, "cannot keep the waveform: {err}");
                self.failed.insert(track);
                false
            }
        }
    }

    fn ready(&self, track: TrackId) {
        let _ = self.actor.send(Msg::WaveformReady(track));
    }
}

pub(crate) fn contract_waveform(waveform: mpmp_engine::Waveform) -> Waveform {
    Waveform {
        buckets: u32::try_from(waveform.buckets()).unwrap_or(u32::MAX),
        channels: waveform
            .channels
            .into_iter()
            .map(|c| WaveformChannel {
                min: c.min,
                max: c.max,
                rms: c.rms,
            })
            .collect(),
    }
}
