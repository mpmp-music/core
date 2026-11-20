//! The engine API and its decode thread.
//!
//! The engine plays the current track and, gaplessly, the one set with
//! [`Engine::set_next`]. Queues and shuffle live in core; core sets the next track again
//! after every [`EngineEvent::TrackStarted`].

mod worker;
use worker::{Command, Diagnostics, Next, Session, SessionNotifications, Worker};

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, select};
use mpmp_media_io::{MediaUri, SourceResolver};
use rtrb::{Producer, RingBuffer};

use crate::EngineError;
use crate::decode::{Decoder, Seek, SourceFormat};
use crate::dsp::channel_map_alters;
use crate::output::{
    DeviceId, OutputBackend, OutputEventSink, OutputFacts, OutputStream, ShareMode, StreamFormat,
    StreamRequest,
};
use crate::pipeline::{Pipeline, TrackStart};
use crate::render::{EngineRenderer, Shared};
use crate::segment::{EntryId, Generation, MAX_GENERATION, Segment};
use crate::signal::SignalPath;
use crate::timeline::{Heard, HeardLog, Position, Timeline};

const SEGMENT_SLOTS: usize = 4096;

const REPORT_SLOTS: usize = 4096;

const RUNNING: u32 = 0;

const STOPPED: u32 = 1;

const FROZEN: u32 = 2;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Ring buffer length (~200 ms on desktop, 1–2 s on mobile).
    pub buffer: Duration,
    /// `None` uses the system default output device.
    pub device: Option<DeviceId>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            buffer: Duration::from_millis(200),
            device: None,
        }
    }
}

/// A part of a file in source frames, e.g. one track of a CUE sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRange {
    pub start: u64,
    pub end: Option<u64>,
}

/// One playable queue entry.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackRef {
    pub entry: EntryId,
    pub uri: MediaUri,
    pub range: Option<FrameRange>,
    /// Linear gain for this track (ReplayGain, computed by core), applied on top of the
    /// volume. Exactly 1.0 leaves the samples untouched. Change it later with
    /// [`Engine::set_gain`].
    pub gain: f32,
}

impl TrackRef {
    pub fn new(entry: EntryId, uri: MediaUri) -> Self {
        Self {
            entry,
            uri,
            range: None,
            gain: 1.0,
        }
    }

    /// Sets the track's linear gain (see [`gain`](Self::gain)).
    pub fn with_gain(mut self, gain: f32) -> Self {
        self.gain = gain;
        self
    }

    fn range_start(&self) -> u64 {
        self.range.map_or(0, |r| r.start)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// The first frame of the entry is being heard (not merely decoded).
    TrackStarted {
        entry: EntryId,
        signal_path: SignalPath,
    },
    /// A `set_next` came too late: the previously set next track is already audible and
    /// was kept. Core decides what to do (usually skip to the new choice).
    NextAlreadyStarted { entry: EntryId },
    /// The entry could not be opened or stopped decoding part-way; playback moved on.
    TrackFailed { entry: EntryId, reason: String },
    /// The last frame of this playback instance has been heard. Its identity remains
    /// authoritative even if a later Play has already been accepted by the owner.
    EndOfQueue { entry: EntryId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferStatus {
    /// Frames in the ring, including flushed ones not yet discarded.
    pub buffered_frames: u64,
    /// Content frames rendered so far (the content index).
    pub played_frames: u64,
    /// Nothing more will be written until a new command.
    pub end_of_stream: bool,
    pub underruns: u64,
    /// Audio callbacks that came late enough for the device to run dry (see
    /// `underruns` for our own buffer running dry), and the longest callback gap.
    pub late_callbacks: u64,
    pub max_callback_gap: Duration,
}

/// A closed stream's final rendered boundary, shared by recovery and history.
#[derive(Debug)]
pub struct FrozenOutput {
    pub position: Option<Position>,
    pub rendered_frames: u64,
    /// The latest seek target if its generation has not rendered any content.
    pub pending_seek: Option<(EntryId, u64)>,
    pub events: Vec<EngineEvent>,
}

/// Optional owner-supplied access barrier, acquired before resolving any media URI.
pub type FileAccess = dyn Fn(&MediaUri) -> Result<Box<dyn Send>, EngineError> + Send + Sync;

/// Fully prepared decoder and, when needed, a silent replacement stream. Dropping
/// this value leaves the accepted playback session and its event authority intact.
pub struct PreparedPlayback {
    track: TrackRef,
    decoder: Box<Decoder>,
    frame: u64,
    session: Option<(Session, OutputEventSink, u64)>,
    path: SignalPath,
}

pub struct Engine {
    backend: Box<dyn OutputBackend>,
    resolver: Arc<dyn SourceResolver>,
    file_access: Option<Arc<FileAccess>>,
    config: EngineConfig,
    events: (Sender<EngineEvent>, Receiver<EngineEvent>),
    output_events: (Sender<()>, Receiver<()>),
    output_sink: Option<OutputEventSink>,
    device_sink: OutputEventSink,
    output_generation: u64,
    session: Option<Session>,
    // Opens can outlive the session that requested them, even after Stop.
    prefetch_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// Kept here too, so a pause before the first `play` is not lost.
    paused: bool,
    playback_deadline: Option<Arc<crate::PlaybackDeadline>>,
    /// Master volume (f32 bits), kept here too for the same reason.
    volume: AtomicU32,
    /// What earlier output streams played, per entry (see [`heard`](Self::heard)).
    heard_history: HeardLog,
}

impl Engine {
    pub fn new(
        mut backend: Box<dyn OutputBackend>,
        resolver: Arc<dyn SourceResolver>,
        config: EngineConfig,
    ) -> Self {
        let output_events = crossbeam_channel::bounded(1);
        let output_sink = OutputEventSink::new(0, output_events.0.clone());
        if let Err(error) = backend.watch_output(output_sink.clone()) {
            tracing::warn!(%error, "output device notifications unavailable");
        }
        Self {
            backend,
            resolver,
            file_access: None,
            config,
            events: crossbeam_channel::unbounded(),
            output_events,
            output_sink: None,
            device_sink: output_sink,
            output_generation: 0,
            session: None,
            prefetch_threads: Arc::default(),
            paused: false,
            playback_deadline: None,
            volume: AtomicU32::new(1.0f32.to_bits()),
            heard_history: HeardLog::new(0),
        }
    }

    /// Set before playback begins. Leases live until the decoder has closed its source.
    pub fn set_file_access(&mut self, access: Arc<FileAccess>) {
        self.file_access = Some(access);
    }

    pub fn events(&self) -> Receiver<EngineEvent> {
        self.events.1.clone()
    }

    /// Device events, for core to act on.
    pub fn output_events(&self) -> Receiver<()> {
        self.output_events.1.clone()
    }

    pub fn take_output_facts(&self) -> Option<OutputFacts> {
        let mut facts = self
            .device_sink
            .take()
            .or_else(|| self.output_sink.as_ref().and_then(OutputEventSink::take));
        if self.session.as_ref().is_some_and(|session| {
            session
                .shared
                .clock_unreliable
                .swap(false, Ordering::AcqRel)
        }) {
            facts
                .get_or_insert_with(|| OutputFacts::clock_unreliable(self.output_generation))
                .mark_clock_unreliable();
        }
        if self.device_sink.take_disconnect() {
            facts
                .get_or_insert_with(|| OutputFacts::disconnect(self.output_generation))
                .route
                .headphones_disconnected = true;
        }
        if let Some(f) = &mut facts
            && f.generation == 0
        {
            f.generation = self.output_generation;
        }
        facts
    }

    pub fn take_output_disconnect(&self) -> bool {
        self.device_sink.take_disconnect()
    }

    pub fn output_generation(&self) -> u64 {
        self.output_generation
    }

    pub fn callback_count(&self) -> u64 {
        self.session
            .as_ref()
            .map_or(0, |s| s.shared.callbacks.load(Ordering::Acquire))
    }

    pub fn default_output_format(&self) -> Result<StreamFormat, EngineError> {
        Ok(self
            .backend
            .capabilities(self.config.device.as_ref())?
            .mix_format)
    }

    pub fn selected_output_device(&self) -> Option<DeviceId> {
        self.config.device.clone()
    }

    pub fn output_identity_persistent(&self, device: &DeviceId) -> bool {
        self.backend.identity_persistent(device)
    }

    pub fn commit_output_discovery(&mut self, result: crate::output::DiscoveredDevices) {
        self.backend.commit_discovery(result);
    }
    pub fn output_capability_query(
        &self,
        device: Option<&DeviceId>,
    ) -> Result<crate::output::CapabilityQuery, EngineError> {
        Ok(self.backend.capability_query(device)?)
    }
    pub fn device_discovery(&self) -> Option<crate::output::DeviceDiscovery> {
        self.backend.device_discovery()
    }
    pub fn has_native_output_notifications(&self) -> bool {
        self.backend.has_native_notifications()
    }

    pub fn output_capabilities(
        &self,
        device: Option<&DeviceId>,
    ) -> Result<crate::output::OutputCaps, EngineError> {
        Ok(self.backend.capabilities(device)?)
    }

    /// Call only after freezing the old output. Device observations retain a distinct
    /// generation even when the replacement fails before opening a stream.
    pub fn set_output_device(&mut self, device: Option<DeviceId>) {
        self.output_generation += 1;
        if let Some(old) = self.output_sink.take() {
            old.retire();
        }
        self.device_sink.retire();
        self.device_sink = OutputEventSink::new(0, self.output_events.0.clone());
        // Configure the new identity while the earlier observer is retired. Native
        // workers serialize selection and watch changes, so queued old-route facts
        // cannot inherit the new configuration's authority or disconnect latch.
        let _ = self.backend.select_device(device.as_ref());
        let _ = self.backend.watch_output(self.device_sink.clone());
        self.config.device = device;
    }

    pub fn output_device(&self) -> Option<crate::output::DeviceInfo> {
        self.session.as_ref()?.stream.as_ref()?.device_info()
    }

    /// Reopens at a frozen source frame, preserving volume, pause and heard history.
    /// Core resends the prepared successor after this succeeds.
    pub fn rebuild_output(
        &mut self,
        track: TrackRef,
        frame: u64,
    ) -> Result<SignalPath, EngineError> {
        self.stop();
        self.start_from(track, frame)
    }

    /// Starts `track` now, dropping whatever was playing or queued as next.
    pub fn play(&mut self, track: TrackRef) -> Result<(), EngineError> {
        self.play_from(track, 0)
    }

    /// Like [`play`](Self::play), starting `frame` source frames into the track.
    pub fn play_from(&mut self, track: TrackRef, frame: u64) -> Result<(), EngineError> {
        self.start_from(track, frame).map(|_| ())
    }

    /// Prepare navigation without retiring the old stream, sending Play, or changing
    /// pause/volume/event authority. Replacement streams contain no playable content.
    pub fn prepare_playback(
        &mut self,
        track: TrackRef,
        frame: u64,
        fresh: bool,
    ) -> Result<PreparedPlayback, EngineError> {
        let decoder = Box::new(open_decoder(
            self.resolver.as_ref(),
            self.file_access.as_deref(),
            &track.uri,
        )?);
        let session = if fresh || self.session.is_none() {
            let format = self
                .backend
                .capabilities(self.config.device.as_ref())?
                .mix_format;
            let generation = self.output_generation + 1;
            let sink = self.device_sink.stream(generation);
            let session = match self.build_session(format, true, sink.clone()) {
                Ok(session) => session,
                Err(error) => {
                    sink.retire();
                    return Err(error);
                }
            };
            Some((session, sink, generation))
        } else {
            None
        };
        let format = session
            .as_ref()
            .map(|(s, _, _)| s.stream.as_ref().unwrap().format())
            .or_else(|| self.output_format())
            .expect("prepared output");
        let path = SignalPath::new(decoder.format(), format, ShareMode::Shared);
        Ok(PreparedPlayback {
            track,
            decoder,
            frame,
            session,
            path,
        })
    }

    /// Commit is infallible after preparation; the owner settles the old session before
    /// committing a fresh stream. No decoder or device opens occur at this boundary.
    pub fn commit_playback(&mut self, prepared: PreparedPlayback) -> SignalPath {
        let PreparedPlayback {
            track,
            decoder,
            frame,
            session,
            path,
        } = prepared;
        if let Some((session, sink, generation)) = session {
            assert!(
                self.session.is_none(),
                "settle the old stream before commit"
            );
            if let Some(old) = self.output_sink.replace(sink) {
                old.retire();
            }
            self.output_generation = generation;
            self.session = Some(session);
        }
        let session = self.session.as_ref().expect("prepared output");
        lock(&session.timeline).register(track.entry);
        self.send(Command::Play {
            track,
            decoder,
            frame,
        });
        path
    }

    fn start_from(&mut self, track: TrackRef, frame: u64) -> Result<SignalPath, EngineError> {
        let decoder = open_decoder(
            self.resolver.as_ref(),
            self.file_access.as_deref(),
            &track.uri,
        )?;
        if self.session.is_none() {
            self.session = Some(self.open_session()?);
        }
        if let Some(session) = &self.session {
            lock(&session.timeline).register(track.entry);
        }
        let path = SignalPath::new(
            decoder.format(),
            self.output_format().expect("session is open"),
            ShareMode::Shared,
        );
        self.send(Command::Play {
            track,
            decoder: Box::new(decoder),
            frame,
        });
        Ok(path)
    }

    /// Supplies a deadline gate before opening any output stream.
    pub fn set_playback_deadline(&mut self, deadline: Arc<crate::PlaybackDeadline>) {
        assert!(self.session.is_none());
        self.playback_deadline = Some(deadline);
    }

    /// The last submitted source position, independent of the audible pause anchor.
    pub fn rendered_position(&self) -> Option<Position> {
        let session = self.session.as_ref()?;
        let index = session.shared.frames_played.load(Ordering::Acquire);
        let mut timeline = lock(&session.timeline);
        timeline.sync();
        timeline.position_at(index)
    }

    /// Settle submitted starts without closing, pausing or flushing the output. This
    /// also lets a caller reject stale UI expectations before issuing any mutation.
    pub fn settle_rendered_starts(&self) {
        let Some(session) = &self.session else {
            return;
        };
        let mut notifications = lock(&session.notifications);
        let index = session.shared.frames_played.load(Ordering::Acquire);
        let mut timeline = lock(&session.timeline);
        timeline.sync();
        for report in timeline.take_due_starts(index) {
            let entry = report.segment.entry;
            if let Some(signal_path) = notifications.paths.remove(&entry) {
                let _ = self
                    .events
                    .0
                    .send(EngineEvent::TrackStarted { entry, signal_path });
            }
        }
    }

    /// Confirm that the current stream cannot submit a successor. The worker's partial
    /// flush CAS serializes with the renderer's generation commitment: a lost race
    /// returns false without changing output. No stream, ramp or current samples reset.
    pub fn truncate_after(&self, track: TrackRef) -> Result<bool, EngineError> {
        let Some(session) = &self.session else {
            return Ok(true);
        };
        let (reply, result) = crossbeam_channel::bounded(1);
        session
            .commands
            .send(Command::TruncateAfter { track, reply })
            .map_err(|_| {
                EngineError::Decode("decode worker stopped before boundary confirmation".into())
            })?;
        result
            .recv()
            .map_err(|_| EngineError::Decode("boundary confirmation disconnected".into()))?
            .map_err(EngineError::Decode)
    }

    /// Sets the track to follow the one being heard, or clears it (partial flush).
    pub fn set_next(&self, track: Option<TrackRef>) {
        self.send(Command::SetNext(track));
    }

    /// Seeks `entry`, which should be the track being heard, to a source frame.
    pub fn seek(&self, entry: EntryId, frame: u64) {
        if let Some(session) = &self.session {
            let token = lock(&session.timeline).request_seek(entry, frame);
            self.send(Command::Seek {
                entry,
                frame,
                token,
            });
        }
    }

    /// Fades out and keeps the stream running, emitting silence.
    pub fn pause(&mut self) {
        self.set_paused(true);
    }

    pub fn resume(&mut self) {
        self.set_paused(false);
    }

    fn set_paused(&mut self, paused: bool) {
        if let Some(session) = &self.session {
            let mut timeline = lock(&session.timeline);
            if paused && !self.paused {
                timeline.sync();
                let now = session.shared.now_ns();
                let index = timeline.audible_index(session.shared.clock.read(), now);
                session.shared.pause_index.store(index, Ordering::Release);
            }
            session.shared.paused.store(paused, Ordering::Release);
        }
        self.paused = paused;
    }

    /// Sets the master volume as a linear multiplier (core maps the slider to it). Takes
    /// effect at the next audio callback, with a ~10 ms ramp. Exactly 1.0 (with a track
    /// gain of 1.0) leaves the samples untouched.
    pub fn set_volume(&self, gain: f32) {
        let bits = sanitize_gain(gain).to_bits();
        self.volume.store(bits, Ordering::Relaxed);
        if let Some(session) = &self.session {
            session.shared.volume.store(bits, Ordering::Relaxed);
        }
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    /// Changes the linear gain of an entry already given to [`play`](Self::play) or
    /// [`set_next`](Self::set_next) (the current or the next track, even while the next
    /// is still opening). It applies to audio decoded from now on; what is already
    /// buffered (up to [`EngineConfig::buffer`]) keeps the old gain, and the change is
    /// ramped. Unknown entries are ignored.
    pub fn set_gain(&self, entry: EntryId, gain: f32) {
        self.send(Command::SetGain {
            entry,
            gain: sanitize_gain(gain),
        });
    }

    /// The track gain × volume the output applies right now (excluding pause fades);
    /// during a ramp, somewhere between the old and the new value. For diagnostics.
    pub fn applied_gain(&self) -> Option<f32> {
        let shared = &self.session.as_ref()?.shared;
        Some(f32::from_bits(shared.applied_gain.load(Ordering::Relaxed)))
    }

    /// Stops playback and closes the output stream. What was heard stays readable
    /// through [`heard`](Self::heard).
    pub fn stop(&mut self) {
        self.close_session(false);
    }

    /// Stops playback and waits for prefetch opens from all sessions to release their
    /// sources. Join any threads returned by [`Self::freeze_output_detached`] first.
    pub fn stop_and_wait(&mut self) {
        self.stop();
        let threads = std::mem::take(&mut *lock(&self.prefetch_threads));
        for thread in threads {
            let _ = thread.join();
        }
    }

    /// Closes the output before joining decoding. No old callback can advance the
    /// returned position. Recovery uses rendered frames, exactly like heard history;
    /// device-queued latency is committed once rather than replayed after rebuilding.
    pub fn freeze_output(&mut self) -> FrozenOutput {
        let mut frozen = self.close_session(true);
        frozen.events = self.events.1.try_iter().collect();
        frozen
    }

    /// Revoke output/session authority immediately. The owner must retire the returned
    /// decode thread off its control loop; its decoder keeps its file lease until exit.
    /// Rotate the event channel so late events cannot authorize a subsequent session.
    pub fn freeze_output_detached(&mut self) -> (FrozenOutput, Option<JoinHandle<()>>) {
        let active = self.session.is_some();
        let (mut frozen, thread) = self.close_session_inner(true, false);
        if !active {
            frozen.events = self.events.1.try_iter().collect();
        }
        self.events = crossbeam_channel::unbounded();
        (frozen, thread)
    }

    fn close_session(&mut self, freeze: bool) -> FrozenOutput {
        self.close_session_inner(freeze, true).0
    }
    fn close_session_inner(
        &mut self,
        freeze: bool,
        join: bool,
    ) -> (FrozenOutput, Option<JoinHandle<()>>) {
        let Some(mut session) = self.session.take() else {
            return (
                FrozenOutput {
                    position: None,
                    rendered_frames: 0,
                    pending_seek: None,
                    events: Vec::new(),
                },
                None,
            );
        };
        // Let callbacks fade out while the backend is still closing this stream.
        session.shared.closing.store(true, Ordering::Release);
        session.shared.paused.store(true, Ordering::Release);
        drop(session.stream.take());
        if let Some(sink) = &self.output_sink {
            sink.retire();
        }
        session
            .stop
            .store(if freeze { FROZEN } else { STOPPED }, Ordering::Release);
        let mut retired = session.thread.take();
        if join && let Some(thread) = retired.take() {
            let _ = thread.join();
        }
        let played = session.shared.frames_played.load(Ordering::Acquire);
        // Match the worker's lock order. Snapshot all already-rendered starts without
        // waiting for a blocked read, and serialize event publication with observe_at.
        let mut notifications = lock(&session.notifications);
        let mut timeline = lock(&session.timeline);
        timeline.sync();
        let mut events = Vec::new();
        if !join {
            events.extend(self.events.1.try_iter());
            for report in timeline.take_due_starts(played) {
                let entry = report.segment.entry;
                if let Some(signal_path) = notifications.paths.remove(&entry) {
                    events.push(EngineEvent::TrackStarted { entry, signal_path });
                }
            }
            let epoch = session.shared.end_of_stream.load(Ordering::Acquire);
            if epoch != 0
                && session.shared.drained.load(Ordering::Acquire) == epoch
                && played >= session.shared.drained_at.load(Ordering::Acquire)
                && notifications.end_reported != epoch
            {
                notifications.end_reported = epoch;
                if let Some(entry) = notifications.end_entry {
                    events.push(EngineEvent::EndOfQueue { entry });
                }
            }
        }
        let position = timeline.position_at(played);
        let pending_seek = timeline.pending_seek_at(played);
        let log = timeline.finish(played);
        self.heard_history.merge(&log);
        (
            FrozenOutput {
                position,
                rendered_frames: played,
                pending_seek,
                events,
            },
            retired,
        )
    }

    /// How much of `entry` the output has played so far: every content frame the
    /// renderer output of it, across seeks, pauses, flushes and stream restarts.
    ///
    /// - Counted: frames actually rendered, including the short fades the renderer
    ///   plays itself (pause/resume ramps, the ~5 ms fade-out of audio cut by a seek or
    ///   a new track). At a gapless boundary each frame counts for the entry it belongs
    ///   to.
    /// - Not counted: paused or underrun silence, and buffered audio that a seek, a new
    ///   track or a replaced next track flushed before it was played.
    /// - "Rendered" is when the renderer wrote the frames to the device buffer, which is
    ///   up to the output latency before they are heard.
    ///
    /// Readable while the entry plays and afterwards, including after [`stop`](Self::stop)
    /// (the last ~16 entries are kept; an entry replayed in a later stream adds up).
    /// `Some` (possibly zero frames) once the entry was given to [`play`](Self::play) or
    /// any of its audio was rendered; `None` for entries the engine never started or no
    /// longer remembers.
    pub fn heard(&self, entry: EntryId) -> Option<Heard> {
        let past = self.heard_history.get(entry);
        let Some(session) = &self.session else {
            return past;
        };
        let mut timeline = lock(&session.timeline);
        // Before taking in the reports: every report the index covers is then seen.
        let played = session.shared.frames_played.load(Ordering::Acquire);
        timeline.sync();
        let live = timeline.heard(entry, played);
        match (past, live) {
            (Some(past), Some(live)) => Some(Heard {
                frames: live.frames
                    + crate::timeline::rescale(past.frames, past.sample_rate, live.sample_rate),
                sample_rate: live.sample_rate,
            }),
            (past, live) => live.or(past),
        }
    }

    pub fn is_active(&self) -> bool {
        self.session.is_some()
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// True once a pause has fully faded out.
    pub fn is_silent(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.shared.silent.load(Ordering::Acquire))
    }

    /// What is being heard right now.
    pub fn position(&self) -> Option<Position> {
        let session = self.session.as_ref()?;
        let mut timeline = lock(&session.timeline);
        timeline.sync();
        let now = session.shared.now_ns();
        let index = if session.shared.paused.load(Ordering::Acquire) {
            session.shared.pause_index.load(Ordering::Acquire)
        } else {
            timeline.audible_index(session.shared.clock.read(), now)
        };
        timeline.position_at(index)
    }

    pub fn output_format(&self) -> Option<StreamFormat> {
        Some(self.session.as_ref()?.stream.as_ref()?.format())
    }

    pub fn buffer_status(&self) -> Option<BufferStatus> {
        let shared = &self.session.as_ref()?.shared;
        let written = shared.frames_written.load(Ordering::Acquire);
        let consumed = shared.frames_consumed.load(Ordering::Acquire);
        Some(BufferStatus {
            buffered_frames: written.saturating_sub(consumed),
            played_frames: shared.frames_played.load(Ordering::Acquire),
            end_of_stream: shared.end_of_stream.load(Ordering::Acquire) != 0,
            underruns: shared.underruns.load(Ordering::Relaxed),
            late_callbacks: shared.late_callbacks.load(Ordering::Relaxed),
            max_callback_gap: Duration::from_micros(
                shared.max_callback_gap_us.load(Ordering::Relaxed),
            ),
        })
    }

    fn send(&self, command: Command) {
        if let Some(session) = &self.session {
            let _ = session.commands.send(command);
        }
    }

    fn open_session(&mut self) -> Result<Session, EngineError> {
        let format = self
            .backend
            .capabilities(self.config.device.as_ref())?
            .mix_format;
        self.output_generation += 1;
        let sink = self.device_sink.stream(self.output_generation);
        if let Some(old) = self.output_sink.replace(sink.clone()) {
            old.retire();
        }
        self.build_session(format, self.paused, sink)
    }

    fn build_session(
        &mut self,
        format: StreamFormat,
        paused: bool,
        sink: OutputEventSink,
    ) -> Result<Session, EngineError> {
        let channels = usize::from(format.channels);
        let ring_frames =
            (self.config.buffer.as_secs_f64() * f64::from(format.sample_rate)).ceil() as usize;
        let (samples_tx, samples_rx) = RingBuffer::new(ring_frames.max(256) * channels);
        let (segments_tx, segments_rx) = RingBuffer::new(SEGMENT_SLOTS);
        let (reports_tx, reports_rx) = RingBuffer::new(REPORT_SLOTS);
        let mut shared = Shared::new(channels, format.sample_rate);
        shared.playback_deadline = self.playback_deadline.clone();
        let shared = Arc::new(shared);
        shared.paused.store(paused, Ordering::Release);
        shared
            .volume
            .store(self.volume.load(Ordering::Relaxed), Ordering::Relaxed);
        let renderer =
            EngineRenderer::new(samples_rx, segments_rx, reports_tx, Arc::clone(&shared));

        let stream = self.backend.open(
            StreamRequest {
                device: self.config.device.clone(),
                format,
                share_mode: ShareMode::Shared,
                events: sink,
            },
            Box::new(renderer),
        )?;
        let output = stream.format();
        let timeline = Arc::new(Mutex::new(Timeline::new(
            output.sample_rate,
            Some(reports_rx),
        )));
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicU32::new(RUNNING));
        let (opened_tx, opened_rx) = crossbeam_channel::unbounded();

        let notifications = Arc::new(Mutex::new(SessionNotifications::default()));
        let worker = Worker {
            commands: commands_rx,
            opened_tx,
            opened_rx,
            prefetch_threads: Arc::clone(&self.prefetch_threads),
            resolver: Arc::clone(&self.resolver),
            file_access: self.file_access.clone(),
            stop: Arc::clone(&stop),
            shared: Arc::clone(&shared),
            samples: samples_tx,
            segments: segments_tx,
            timeline: Arc::clone(&timeline),
            events: self.events.0.clone(),
            pipeline: Pipeline::new(output.sample_rate, channels),
            output,
            share_mode: ShareMode::Shared,
            current: None,
            next: Next::None,
            previous: None,
            replaceable: None,
            generation: 0,
            open_token: 0,
            notifications: Arc::clone(&notifications),
            gains: HashMap::new(),
            finishing: false,
            epoch: 1,

            decoded: Vec::new(),
            poll: (self.config.buffer / 8)
                .clamp(Duration::from_millis(2), Duration::from_millis(10)),
            diag: Diagnostics::new(),
        };
        let thread = thread::Builder::new()
            .name("decode".into())
            .spawn(move || worker.run())
            .map_err(|e| EngineError::Unsupported(format!("cannot spawn decode thread: {e}")))?;

        Ok(Session {
            notifications,
            shared,
            stop,
            commands: commands_tx,
            timeline,
            thread: Some(thread),
            stream: Some(stream),
        })
    }
}

/// Whether the track's output may carry values that are not exact 16-bit samples, so that
/// 16-bit integer output must dither it even at unity gain: anything but a lossless
/// source of at most 16 bits, played at the device rate, through a copying channel map.
fn needs_dither(source: &SourceFormat, output: StreamFormat) -> bool {
    !source.lossless
        || source.bits_per_sample.is_none_or(|bits| bits > 16)
        || source.sample_rate != output.sample_rate
        || channel_map_alters(usize::from(source.channels), usize::from(output.channels))
}

/// A usable linear gain: NaN or infinity would never let the renderer's ramp settle.
fn sanitize_gain(gain: f32) -> f32 {
    if gain.is_finite() { gain.max(0.0) } else { 1.0 }
}

/// The timeline stays usable even if a holder panicked.
fn lock<T>(timeline: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    timeline.lock().unwrap_or_else(|e| e.into_inner())
}

fn open_decoder(
    resolver: &dyn SourceResolver,
    access: Option<&FileAccess>,
    uri: &MediaUri,
) -> Result<Decoder, EngineError> {
    let lease = access.map(|acquire| acquire(uri)).transpose()?;
    let source = mpmp_media_io::open_uri(resolver, uri)?;
    let mut decoder = Decoder::open(source, uri.extension().as_deref())?;
    decoder.file_lease = lease;
    Ok(decoder)
}

#[cfg(test)]
mod tests;

/// Polls `condition` until it holds or `timeout` passes.
#[doc(hidden)]
pub fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(1));
    }
    condition()
}
