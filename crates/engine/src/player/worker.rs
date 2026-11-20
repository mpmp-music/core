use super::*;

#[derive(Default)]
pub(super) struct SessionNotifications {
    pub(super) paths: HashMap<EntryId, SignalPath>,
    pub(super) end_reported: u64,
    /// Terminal playback instance for the published end-of-stream epoch.
    pub(super) end_entry: Option<EntryId>,
}

pub(super) struct Session {
    pub(super) notifications: Arc<Mutex<SessionNotifications>>,
    pub(super) shared: Arc<Shared>,
    pub(super) stop: Arc<AtomicU32>,
    pub(super) commands: Sender<Command>,
    pub(super) timeline: Arc<Mutex<Timeline>>,
    pub(super) thread: Option<JoinHandle<()>>,
    pub(super) stream: Option<Box<dyn OutputStream>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        drop(self.stream.take());
        self.stop.store(STOPPED, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(super) enum Command {
    Play {
        track: TrackRef,
        decoder: Box<Decoder>,
        frame: u64,
    },
    SetNext(Option<TrackRef>),
    TruncateAfter {
        track: TrackRef,
        reply: Sender<Result<bool, String>>,
    },
    Seek {
        entry: EntryId,
        frame: u64,
        token: u64,
    },
    SetGain {
        entry: EntryId,
        gain: f32,
    },
}

pub(super) struct Opened {
    token: u64,
    track: TrackRef,
    result: Result<Decoder, EngineError>,
}

pub(super) enum Next {
    None,
    Opening {
        token: u64,
        entry: EntryId,
    },
    Ready {
        track: TrackRef,
        decoder: Box<Decoder>,
    },
}

/// The track being decoded.
pub(super) struct Active {
    track: TrackRef,
    decoder: Box<Decoder>,
    generation: Generation,
    /// Absolute decoder frames: drop everything before `skip_until` and from `end` on.
    skip_until: u64,
    end: Option<u64>,
    /// Absolute frame of the next decoded frame, when the container gives no timestamps.
    next_frame: i64,
    track_start: bool,
    began: bool,
}

pub(super) struct Worker {
    pub(super) commands: Receiver<Command>,
    pub(super) opened_tx: Sender<Opened>,
    pub(super) opened_rx: Receiver<Opened>,
    pub(super) prefetch_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
    pub(super) resolver: Arc<dyn SourceResolver>,
    pub(super) file_access: Option<Arc<FileAccess>>,
    pub(super) stop: Arc<AtomicU32>,
    pub(super) shared: Arc<Shared>,
    pub(super) samples: Producer<f32>,
    pub(super) segments: Producer<Segment>,
    pub(super) timeline: Arc<Mutex<Timeline>>,
    pub(super) events: Sender<EngineEvent>,
    pub(super) pipeline: Pipeline,
    pub(super) output: StreamFormat,
    pub(super) share_mode: ShareMode,
    pub(super) current: Option<Active>,
    pub(super) next: Next,
    /// The track before `current`, while it may still be the one heard (a seek target).
    pub(super) previous: Option<TrackRef>,
    /// `(entry, generation)` of `current` while it began gaplessly and may not be heard
    /// yet, so a new `set_next` replaces it with a partial flush.
    pub(super) replaceable: Option<(EntryId, Generation)>,
    pub(super) generation: Generation,
    pub(super) open_token: u64,
    pub(super) notifications: Arc<Mutex<SessionNotifications>>,
    /// Current linear gain of each entry that may still have output to write; stamped on
    /// each segment as it is written, so `set_gain` reaches everything not yet in the
    /// ring. Pruned when a track is heard, keeping every entry the pipeline still holds
    /// output of.
    pub(super) gains: HashMap<EntryId, f32>,
    /// The queue ran out: once the pipeline's output is written, signal end of stream.
    pub(super) finishing: bool,
    /// Bumped whenever new content is coming, so a stale end-of-stream is ignored.
    pub(super) epoch: u64,
    pub(super) decoded: Vec<f32>,
    pub(super) poll: Duration,
    pub(super) diag: Diagnostics,
}

/// Debug-level timing around flushes and underruns (`RUST_LOG=mpmp_engine=debug`), for
/// diagnosing dropouts on real devices.
pub(super) struct Diagnostics {
    flushed_at: Option<Instant>,
    first_write_logged: bool,
    last_write: Instant,
    underruns: u64,
}

impl Diagnostics {
    pub(super) fn new() -> Self {
        Self {
            flushed_at: None,
            first_write_logged: true,
            last_write: Instant::now(),
            underruns: 0,
        }
    }
}

impl Worker {
    pub(super) fn run(mut self) {
        while self.stop.load(Ordering::Acquire) == RUNNING {
            let mut progressed = false;
            while let Ok(command) = self.commands.try_recv() {
                self.handle(command);
                progressed = true;
            }
            while let Ok(opened) = self.opened_rx.try_recv() {
                self.handle_opened(opened);
                progressed = true;
            }
            self.observe();
            progressed |= self.write_output();
            if !self.pipeline.has_output() {
                progressed |= self.produce();
            }
            self.log_underruns();
            if !progressed {
                let waiting = Instant::now();
                select! {
                    recv(self.commands) -> command => {
                        if let Ok(command) = command {
                            self.handle(command);
                        }
                    }
                    recv(self.opened_rx) -> opened => {
                        if let Ok(opened) = opened {
                            self.handle_opened(opened);
                        }
                    }
                    default(self.poll) => {}
                }
                let waited = waiting.elapsed();
                if waited > self.poll * 2 {
                    tracing::debug!(?waited, "decode thread overslept");
                }
            }
        }
        if self.stop.load(Ordering::Acquire) == FROZEN {
            self.observe_at(Some(self.shared.frames_played.load(Ordering::Acquire)));
        }
    }

    fn buffered(&self) -> u64 {
        self.shared
            .frames_written
            .load(Ordering::Acquire)
            .saturating_sub(self.shared.frames_consumed.load(Ordering::Acquire))
    }

    fn log_underruns(&mut self) {
        let underruns = self.shared.underruns.load(Ordering::Relaxed);
        if underruns != self.diag.underruns {
            tracing::debug!(
                underruns,
                buffered = self.buffered(),
                pending_output = self.pipeline.has_output(),
                since_last_write = ?self.diag.last_write.elapsed(),
                since_flush = ?self.diag.flushed_at.map(|t| t.elapsed()),
                "underrun"
            );
            self.diag.underruns = underruns;
        }
    }

    fn next_generation(&mut self) -> Generation {
        // 21-bit generations: about two million tracks and seeks per output
        // stream; stop() + play() starts a new stream. Saturate rather than corrupt the
        // flush word; past this point flushes stop separating old from new content.
        if self.generation + 1 >= MAX_GENERATION {
            tracing::error!("generation space exhausted; restart the output stream");
            return self.generation;
        }
        self.generation += 1;
        self.generation
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Play {
                track,
                decoder,
                frame,
            } => {
                self.flush_all();
                self.next = Next::None;
                self.previous = None;
                self.gains.clear();
                self.gains.insert(track.entry, sanitize_gain(track.gain));
                self.start_track(track, decoder, frame, true);
                self.ensure_finishing();
            }
            Command::SetNext(track) => self.set_next(track),
            Command::TruncateAfter { track, reply } => {
                let result = self.truncate_after(track);
                let _ = reply.send(result);
            }
            Command::Seek {
                entry,
                frame,
                token,
            } => {
                let before = self.generation;
                self.seek(entry, frame);
                if self.generation != before {
                    lock(&self.timeline).apply_seek(token, self.generation);
                }
            }
            Command::SetGain { entry, gain } => self.set_gain(entry, gain),
        }
    }

    /// Applies to output not yet written to the ring, and to any later restart of the
    /// track (a seek reopens it from its `TrackRef`).
    fn set_gain(&mut self, entry: EntryId, gain: f32) {
        let Some(current) = self.gains.get_mut(&entry) else {
            tracing::debug!("set_gain: {entry:?} is not open");
            return;
        };
        *current = gain;
        let tracks = [
            self.current.as_mut().map(|a| &mut a.track),
            match &mut self.next {
                Next::Ready { track, .. } => Some(track),
                _ => None,
            },
            self.previous.as_mut(),
        ];
        for track in tracks.into_iter().flatten() {
            if track.entry == entry {
                track.gain = gain;
            }
        }
    }

    /// New content is coming: any earlier end-of-stream no longer holds.
    fn new_content(&mut self) {
        self.epoch += 1;
        self.finishing = false;
        self.shared.end_of_stream.store(0, Ordering::Release);
    }

    /// Drops everything queued for playback; content written from now on plays.
    fn flush_all(&mut self) {
        tracing::debug!(buffered = self.buffered(), "flush");
        self.diag.flushed_at = Some(Instant::now());
        self.diag.first_write_logged = false;
        let generation = self.next_generation();
        self.shared.flush.flush_all(generation);
        self.pipeline.reset();
        self.current = None;
        self.replaceable = None;
        self.new_content();
    }

    /// Once nothing is decoding, nothing is queued and no boundary can still be rewound,
    /// drain the pipeline and let the stream end (EndOfQueue follows when it is heard).
    fn ensure_finishing(&mut self) {
        let idle =
            self.current.is_none() && matches!(self.next, Next::None) && self.replaceable.is_none();
        if !idle || self.finishing || self.shared.end_of_stream.load(Ordering::Acquire) != 0 {
            return;
        }
        if let Err(err) = self.pipeline.finish() {
            tracing::error!("finishing the stream: {err}");
        }
        self.finishing = true;
    }

    fn start_track(
        &mut self,
        track: TrackRef,
        decoder: Box<Decoder>,
        frame: u64,
        track_start: bool,
    ) {
        self.start_track_reusing(track, decoder, frame, track_start, false);
    }

    /// `reused`: the decoder has already been read from, so it must seek even to the
    /// very start (a fresh one is already there).
    fn start_track_reusing(
        &mut self,
        track: TrackRef,
        mut decoder: Box<Decoder>,
        frame: u64,
        track_start: bool,
        reused: bool,
    ) {
        let skip_until = track.range_start() + frame;
        let mut next_frame = 0;
        let mut past_end = track
            .range
            .and_then(|r| r.end)
            .is_some_and(|end| skip_until >= end);
        if !past_end && (skip_until > 0 || reused) {
            let started = Instant::now();
            let seeked = decoder.seek(skip_until);
            tracing::debug!(frame = skip_until, took = ?started.elapsed(), "decoder seek");
            match seeked {
                Ok(Seek::Landed(landed)) => next_frame = landed as i64,
                Ok(Seek::PastEnd) => past_end = true,
                Err(err) => {
                    self.fail(&track, err.to_string());
                    return;
                }
            }
        }
        if past_end {
            // Seeking at or beyond the end is reaching the end: go on to the next track,
            // or end the queue (not a failure).
            self.previous = Some(track);
            if !self.start_next_if_ready() {
                self.ensure_finishing();
            }
            return;
        }
        lock(&self.notifications).paths.insert(
            track.entry,
            SignalPath::new(decoder.format(), self.output, self.share_mode),
        );
        self.gains
            .entry(track.entry)
            .or_insert_with(|| sanitize_gain(track.gain));
        let generation = self.next_generation();
        self.current = Some(Active {
            end: track.range.and_then(|r| r.end),
            track,
            decoder,
            generation,
            skip_until,
            next_frame,
            track_start,
            began: false,
        });
    }

    fn truncate_after(&mut self, mut track: TrackRef) -> Result<bool, String> {
        let target = track.entry;
        let index = self.shared.frames_played.load(Ordering::Acquire);
        let actual = {
            let mut timeline = lock(&self.timeline);
            timeline.sync();
            timeline.position_at(index).map(|p| p.entry)
        };
        // A manual Play/seek may still be finishing the flushed predecessor's declick.
        // It must not prevent guarding the newly accepted, not-yet-rendered instance.
        let selected = self.replaceable.is_none()
            && (self
                .current
                .as_ref()
                .is_some_and(|a| a.track.entry == target)
                || self
                    .previous
                    .as_ref()
                    .is_some_and(|track| track.entry == target));
        if actual.is_some_and(|entry| entry != target) && !selected {
            return Ok(false);
        }
        if let Some((entry, generation)) = self.replaceable.filter(|(entry, _)| *entry != target) {
            // Do not take replaceable or change any pipeline state until the renderer
            // agrees that none of the successor's samples has been committed.
            let replacement = self.next_generation();
            if self
                .shared
                .flush
                .flush_from(generation, replacement)
                .is_err()
            {
                return Ok(false);
            }
            let began = self
                .current
                .as_ref()
                .is_none_or(|a| a.track.entry != entry || a.began);
            if began
                && !self
                    .pipeline
                    .rewind(generation)
                    .map_err(|e| e.to_string())?
            {
                return Err("successor boundary no longer rewindable".into());
            }
            if self
                .current
                .as_ref()
                .is_some_and(|a| a.track.entry == entry)
            {
                self.current = None;
            }
            lock(&self.notifications).paths.remove(&entry);
            self.gains.remove(&entry);
            self.replaceable = None;
            track.gain = self.gains.get(&target).copied().unwrap_or(track.gain);
            self.previous = Some(track);
            self.new_content();
        } else if self
            .current
            .as_ref()
            .is_some_and(|a| a.track.entry != target)
        {
            return Ok(false);
        }
        self.next = Next::None;
        // A committed target is no longer a replaceable successor itself.
        if self.replaceable.is_some_and(|(entry, _)| entry == target) {
            self.replaceable = None;
        }
        self.ensure_finishing();
        Ok(true)
    }

    fn set_next(&mut self, track: Option<TrackRef>) {
        if let Some((entry, generation)) = self.replaceable.take() {
            // The previous set_next's track has started decoding but is not heard yet:
            // replace it (partial flush).
            let began = self
                .current
                .as_ref()
                .is_none_or(|active| active.track.entry != entry || active.began);
            if began {
                let replacement = self.next_generation();
                if self
                    .shared
                    .flush
                    .flush_from(generation, replacement)
                    .is_err()
                {
                    let _ = self.events.send(EngineEvent::NextAlreadyStarted { entry });
                    return;
                }
                match self.pipeline.rewind(generation) {
                    Ok(true) => {}
                    Ok(false) => tracing::error!("partial flush: no boundary for {entry:?}"),
                    Err(err) => tracing::error!("partial flush: {err}"),
                }
            }
            // Nothing of it reached the pipeline otherwise (condition 3): just drop it.
            if self
                .current
                .as_ref()
                .is_some_and(|a| a.track.entry == entry)
            {
                self.current = None;
            }
            lock(&self.notifications).paths.remove(&entry);
            self.gains.remove(&entry);
            self.new_content();
        }
        match track {
            Some(track) => {
                self.gains.insert(track.entry, sanitize_gain(track.gain));
                self.new_content();
                self.open_token += 1;
                let token = self.open_token;
                let resolver = Arc::clone(&self.resolver);
                let access = self.file_access.clone();
                let opened = self.opened_tx.clone();
                let uri = track.uri.clone();
                self.next = Next::Opening {
                    token,
                    entry: track.entry,
                };
                // Opening can block on slow storage (prefetch thread).
                let spawned = thread::Builder::new()
                    .name("prefetch".into())
                    .spawn(move || {
                        let result = open_decoder(resolver.as_ref(), access.as_deref(), &uri);
                        let _ = opened.send(Opened {
                            token,
                            track,
                            result,
                        });
                    });
                match spawned {
                    Ok(thread) => {
                        let mut threads = lock(&self.prefetch_threads);
                        for finished in threads.extract_if(.., |thread| thread.is_finished()) {
                            let _ = finished.join();
                        }
                        threads.push(thread);
                    }
                    Err(err) => {
                        tracing::error!("cannot spawn prefetch thread: {err}");
                        self.next = Next::None;
                    }
                }
            }
            None => self.next = Next::None,
        }
        self.ensure_finishing();
    }

    fn handle_opened(&mut self, opened: Opened) {
        let Next::Opening { token, .. } = &self.next else {
            return;
        };
        if *token != opened.token {
            return;
        }
        let mut track = opened.track;
        // A set_gain may have come while it was opening.
        if let Some(&gain) = self.gains.get(&track.entry) {
            track.gain = gain;
        }
        match opened.result {
            Ok(decoder) => {
                self.next = Next::Ready {
                    track,
                    decoder: Box::new(decoder),
                }
            }
            Err(err) => {
                self.next = Next::None;
                self.fail(&track, err.to_string());
                self.ensure_finishing();
            }
        }
    }

    fn seek(&mut self, entry: EntryId, frame: u64) {
        let seeking_current = self
            .current
            .as_ref()
            .is_some_and(|a| a.track.entry == entry);
        if seeking_current {
            let active = self.current.take().expect("checked above");
            self.flush_all();
            self.start_track_reusing(active.track, active.decoder, frame, false, true);
            self.ensure_finishing();
            return;
        }
        // The track still heard while its successor decodes: reopen it first, and only
        // then change any state.
        let Some(previous) = self.previous.clone().filter(|t| t.entry == entry) else {
            tracing::warn!("seek: {entry:?} is not playing");
            return;
        };
        let decoder = match open_decoder(
            self.resolver.as_ref(),
            self.file_access.as_deref(),
            &previous.uri,
        ) {
            Ok(decoder) => Box::new(decoder),
            Err(err) => {
                self.fail(&previous, err.to_string());
                return;
            }
        };
        // Its successor goes back in line, from its start. A next set after it is
        // dropped; core sets it again after the successor's TrackStarted.
        let mut reopen = None;
        if let Some(active) = self.current.take() {
            let mut successor = active.decoder;
            match successor.seek(active.track.range_start()) {
                Ok(_) => {
                    self.next = Next::Ready {
                        track: active.track,
                        decoder: successor,
                    }
                }
                Err(_) => reopen = Some(active.track),
            }
        }
        self.flush_all();
        self.start_track(previous, decoder, frame, false);
        if let Some(track) = reopen {
            self.set_next(Some(track));
        }
        self.ensure_finishing();
    }

    fn fail(&mut self, track: &TrackRef, reason: String) {
        tracing::warn!("{}: {reason}", track.uri);
        let _ = self.events.send(EngineEvent::TrackFailed {
            entry: track.entry,
            reason,
        });
    }

    /// Decodes one packet of the current track, or moves on to the next track.
    fn produce(&mut self) -> bool {
        let Some(active) = self.current.as_mut() else {
            return self.start_next_if_ready();
        };
        let chunk = match active.decoder.next_chunk(&mut self.decoded) {
            Ok(Some(chunk)) => chunk,
            Ok(None) => {
                self.track_ended();
                return true;
            }
            Err(err) => {
                let track = active.track.clone();
                self.fail(&track, err.to_string());
                self.track_ended();
                return true;
            }
        };

        // Frame trimmer: seek target and range end, in absolute decoder frames.
        // `first` can be negative for codec delay that was not trimmed already.
        let channels = usize::from(active.decoder.format().channels);
        let frames = (self.decoded.len() / channels) as i64;
        let first = chunk.start_frame.unwrap_or(active.next_frame);
        active.next_frame = first + frames;
        let from = (active.skip_until as i64 - first).clamp(0, frames);
        let to = active
            .end
            .map_or(frames, |end| (end as i64 - first).clamp(0, frames));
        let reached_end = active.end.is_some_and(|end| first + frames >= end as i64);
        if from < to {
            if !active.began {
                active.began = true;
                let format = active.decoder.format();
                let needs_dither = needs_dither(format, self.output);
                let start = TrackStart {
                    generation: active.generation,
                    entry: active.track.entry,
                    source_rate: format.sample_rate,
                    source_channels: channels,
                    source_start: (first + from) as u64 - active.track.range_start(),
                    track_start: active.track_start,
                    needs_dither,
                };
                if let Err(err) = self.pipeline.begin(start) {
                    let track = active.track.clone();
                    self.fail(&track, err.to_string());
                    self.track_ended();
                    return true;
                }
            }
            let samples = &self.decoded[from as usize * channels..to as usize * channels];
            if let Err(err) = self.pipeline.push(samples) {
                let track = active.track.clone();
                self.fail(&track, err.to_string());
                self.track_ended();
                return true;
            }
        }
        if reached_end {
            self.track_ended();
        }
        true
    }

    fn track_ended(&mut self) {
        if let Some(active) = self.current.take() {
            self.previous = Some(active.track);
        }
        // `replaceable` stays: a track shorter than the ring can end decoding before it
        // is heard, and must still be replaceable until then.
        if !self.start_next_if_ready() {
            self.ensure_finishing();
        }
    }

    /// Starts the next track if it is open and nothing is decoding; gapless when the
    /// pipeline still runs the previous track's stream.
    fn start_next_if_ready(&mut self) -> bool {
        if self.current.is_some() || !matches!(self.next, Next::Ready { .. }) {
            return false;
        }
        let Next::Ready { track, decoder } = std::mem::replace(&mut self.next, Next::None) else {
            unreachable!()
        };
        let entry = track.entry;
        self.start_track(track, decoder, 0, true);
        self.replaceable = self.current.as_ref().map(|a| (entry, a.generation));
        if self.current.is_none() {
            self.ensure_finishing();
        }
        true
    }

    /// Moves pipeline output into the rings, as far as they have room.
    fn write_output(&mut self) -> bool {
        let channels = usize::from(self.output.channels);
        let mut progressed = false;
        loop {
            let room = self.samples.slots() / channels;
            if room == 0 || self.segments.is_full() {
                break;
            }
            let Some((samples, mut segment)) = self.pipeline.peek(room) else {
                break;
            };
            segment.gain = self.gains.get(&segment.entry).copied().unwrap_or(1.0);
            self.samples
                .push_entire_slice(samples)
                .expect("room was checked");
            // Samples first, then their descriptor, then the count: only frames
            // with a descriptor are playable, so `buffered` must not count them before.
            let _ = self.segments.push(segment);
            self.shared
                .frames_written
                .fetch_add(u64::from(segment.frames), Ordering::AcqRel);
            self.pipeline.advance(segment.frames as usize);
            progressed = true;
            self.diag.last_write = Instant::now();
            if !self.diag.first_write_logged {
                self.diag.first_write_logged = true;
                tracing::debug!(
                    frames = segment.frames,
                    buffered = self.buffered(),
                    since_flush = ?self.diag.flushed_at.map(|t| t.elapsed()),
                    "first write after flush"
                );
            }
        }
        if self.finishing && !self.pipeline.has_output() {
            self.finishing = false;
            // Publish identity and epoch together under the notification lock. Closing
            // and observation must never associate an old drain with a new instance.
            let mut notifications = lock(&self.notifications);
            notifications.end_entry = self.previous.as_ref().map(|track| track.entry);
            self.shared
                .end_of_stream
                .store(self.epoch, Ordering::Release);
        }
        progressed
    }

    /// Turns renderer reports and the clock into events.
    fn observe(&mut self) {
        self.observe_at(None);
    }

    fn observe_at(&mut self, final_index: Option<u64>) {
        let notifications = Arc::clone(&self.notifications);
        let mut notifications = lock(&notifications);
        let now = self.shared.now_ns();
        let (index, started) = {
            let mut timeline = lock(&self.timeline);
            timeline.sync();
            let paused = self.shared.paused.load(Ordering::Acquire);
            let index = final_index.unwrap_or_else(|| {
                if paused {
                    self.shared.pause_index.load(Ordering::Acquire)
                } else {
                    timeline.audible_index(self.shared.clock.read(), now)
                }
            });
            let started: Vec<EntryId> = if paused && final_index.is_none() {
                Vec::new()
            } else {
                timeline
                    .take_due_starts(index)
                    .map(|r| r.segment.entry)
                    .collect()
            };
            timeline.prune(index);
            (index, started)
        };
        for entry in started {
            // Forget the gains of entries no more audio of can be produced. Not simply
            // everything before this track: a track shorter than the ring can have
            // finished decoding while its tail is still in the pipeline (queued, or
            // inside the resampler), and that tail must keep the track's gain.
            let next = match &self.next {
                Next::Ready { track, .. } => Some(track.entry),
                Next::Opening { entry, .. } => Some(*entry),
                Next::None => None,
            };
            let current = self.current.as_ref().map(|a| a.track.entry);
            let previous = self.previous.as_ref().map(|t| t.entry);
            let pipeline = &self.pipeline;
            self.gains.retain(|e, _| {
                *e == entry
                    || Some(*e) == current
                    || Some(*e) == next
                    || Some(*e) == previous
                    || pipeline.holds(*e)
            });
            if self.replaceable.is_some_and(|(e, _)| e == entry) {
                self.replaceable = None;
                self.ensure_finishing();
            }
            if let Some(signal_path) = notifications.paths.remove(&entry) {
                let _ = self
                    .events
                    .send(EngineEvent::TrackStarted { entry, signal_path });
            }
        }
        // The end is reported once the last frame is heard, not merely rendered.
        let drained = self.shared.drained.load(Ordering::Acquire) == self.epoch;
        if notifications.end_reported != self.epoch
            && drained
            && (!self.shared.paused.load(Ordering::Acquire) || final_index.is_some())
            && index >= self.shared.drained_at.load(Ordering::Acquire)
            && (final_index.is_some() || now >= self.shared.tail_at_ns.load(Ordering::Acquire))
        {
            notifications.end_reported = self.epoch;
            if let Some(entry) = notifications.end_entry {
                let _ = self.events.send(EngineEvent::EndOfQueue { entry });
            }
        }
        #[cfg(test)]
        self.shared.test_observed_ns.store(now, Ordering::Release);
    }
}
