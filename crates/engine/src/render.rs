//! The realtime side: the [`Renderer`] the output backend calls.
//!
//! Only copies, gain, rounding and atomics happen here: no allocation, no locks, no I/O.
//!
//! Gain: every frame is multiplied by `pause ramp × level`, where `level` follows
//! `segment gain (ReplayGain) × volume` with a linear ~10 ms ramp whenever that target
//! changes. A total of exactly 1.0 skips the multiply, so unity stays bit-perfect.
//! Integer 16-bit output gets TPDF dither for any other multiplier, and also at unity for
//! segments marked [`flags::NEEDS_DITHER`] (deeper than 16 bits, lossy, resampled).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use rtrb::{Consumer, Producer};

use crate::delay_guard::DelayGuard;
use crate::output::{RenderInfo, Renderer, SampleBufMut};
use crate::segment::{ClockSample, ClockSeqlock, FlushWord, Segment, SegmentReport, flags};

/// State shared between the control thread, the decode thread and the renderer.
pub(crate) struct Shared {
    pub playback_deadline: Option<Arc<crate::PlaybackDeadline>>,
    pub channels: usize,
    pub sample_rate: u32,
    /// Requested transport state; the renderer fades towards it.
    pub paused: AtomicBool,
    /// Set only by close_session: a real rendered EOF may drain during its pause fade.
    pub closing: AtomicBool,
    /// Audible cursor captured by control code before publishing a pause request.
    pub pause_index: AtomicU64,
    /// One pending notification per continuous invalid-timestamp episode.
    pub clock_unreliable: AtomicBool,
    pub flush: FlushWord,
    pub clock: ClockSeqlock,
    /// Zero point of `ClockSample::playback_at_ns`.
    pub epoch: Instant,
    #[cfg(test)]
    pub test_now_ns: AtomicU64,
    #[cfg(test)]
    pub test_observed_ns: AtomicU64,
    pub underruns: AtomicU64,
    pub reports_dropped: AtomicU64,
    /// Callbacks that came much later than the previous buffer's duration: the device
    /// may have run dry even though our ring had data (a system scheduling stall, which
    /// `underruns` cannot see), and the longest gap seen, in microseconds.
    pub late_callbacks: AtomicU64,
    pub callbacks: AtomicU64,
    pub max_callback_gap_us: AtomicU64,
    /// Frames written to the sample ring (decode thread) and taken out of it, played or
    /// discarded (renderer); their difference is what is buffered.
    pub frames_written: AtomicU64,
    pub frames_consumed: AtomicU64,
    /// The renderer's content index, stored after each buffer (after the reports of the
    /// segments it played): what per-entry heard time is computed from.
    pub frames_played: AtomicU64,
    /// Set by the decode thread to its current stream epoch when nothing more will be
    /// written until a new command; 0 while content may still come.
    pub end_of_stream: AtomicU64,
    /// The `end_of_stream` epoch the renderer saw run dry, and the content index at that
    /// moment. Epochs make a stale "drained" from before a new command harmless.
    pub drained: AtomicU64,
    pub drained_at: AtomicU64,
    /// Conservative device-tail deadline when callback timestamps are unreliable.
    pub tail_at_ns: AtomicU64,
    /// True while a pause has fully faded out and only silence is being rendered.
    pub silent: AtomicBool,
    /// Master volume as a linear multiplier (f32 bits), set by the control thread.
    pub volume: AtomicU32,
    /// The track gain × volume multiplier the renderer applied last (f32 bits), for
    /// diagnostics; excludes the pause ramp and the declick.
    pub applied_gain: AtomicU32,
}

impl Shared {
    pub fn new(channels: usize, sample_rate: u32) -> Self {
        Self {
            playback_deadline: None,
            channels,
            sample_rate,
            paused: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            pause_index: AtomicU64::new(0),
            clock_unreliable: AtomicBool::new(false),
            flush: FlushWord::new(),
            clock: ClockSeqlock::new(),
            epoch: Instant::now(),
            #[cfg(test)]
            test_now_ns: AtomicU64::new(u64::MAX),
            #[cfg(test)]
            test_observed_ns: AtomicU64::new(u64::MAX),
            underruns: AtomicU64::new(0),
            reports_dropped: AtomicU64::new(0),
            late_callbacks: AtomicU64::new(0),
            callbacks: AtomicU64::new(0),
            max_callback_gap_us: AtomicU64::new(0),
            frames_written: AtomicU64::new(0),
            frames_consumed: AtomicU64::new(0),
            frames_played: AtomicU64::new(0),
            end_of_stream: AtomicU64::new(0),
            drained: AtomicU64::new(0),
            drained_at: AtomicU64::new(0),
            tail_at_ns: AtomicU64::new(0),
            silent: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
            applied_gain: AtomicU32::new(1.0f32.to_bits()),
        }
    }

    pub fn now_ns(&self) -> u64 {
        #[cfg(test)]
        {
            let now = self.test_now_ns.load(Ordering::Acquire);
            if now != u64::MAX {
                return now;
            }
        }
        self.epoch.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
    }
}

pub(crate) struct EngineRenderer {
    samples: Consumer<f32>,
    segments: Consumer<Segment>,
    reports: Producer<SegmentReport>,
    shared: Arc<Shared>,
    /// Segment being played and how many of its frames are left.
    current: Option<(Segment, u32)>,
    /// Content frames played so far: the audible content index.
    content_index: u64,
    delay_guard: DelayGuard,
    /// Current gain of the pause/resume ramp, 0.0..=1.0.
    gain: f32,
    /// When the previous callback ran and how long its buffer lasts.
    last_callback: Option<(std::time::Duration, std::time::Duration)>,
    /// Playback (re)starts only once `prime_frames` are buffered or the stream has
    /// ended, so a cold start or a seek does not begin with a stutter.
    primed: bool,
    prime_frames: usize,
    /// Set when flushed data was dropped during the current callback: running dry right
    /// after a flush is the expected gap of a seek, not an underrun.
    flushed: bool,
    /// Gain change per frame while ramping.
    ramp_step: f32,
    /// Frames left of a declick: when a seek or a new track cuts audio that is playing,
    /// the cut-off audio fades out over a few ms instead of stopping dead, and the new
    /// audio fades in from the resulting zero gain.
    declick: u32,
    declick_frames: u32,
    /// Generation of the last segment accepted, to notice crossing a full flush.
    last_generation: Option<u32>,
    /// Applied track gain × volume.
    level: Level,
    dither: Dither,
}

impl EngineRenderer {
    pub fn new(
        samples: Consumer<f32>,
        segments: Consumer<Segment>,
        reports: Producer<SegmentReport>,
        shared: Arc<Shared>,
    ) -> Self {
        const FADE_MS: f32 = 10.0;
        let ramp_step = 1000.0 / (FADE_MS * shared.sample_rate as f32);
        let gain = if shared.paused.load(Ordering::Acquire) {
            0.0
        } else {
            1.0
        };
        // 50 ms, but never more than half the ring.
        let ring_frames = samples.buffer().capacity() / shared.channels;
        // 5 ms.
        let declick_frames = (shared.sample_rate / 200).max(1);
        // 10 ms, like the pause ramp.
        let level_ramp_frames = (shared.sample_rate / 100).max(1);
        let prime_frames = (shared.sample_rate as usize / 20).min(ring_frames / 2);
        Self {
            samples,
            segments,
            reports,
            shared,
            current: None,
            content_index: 0,
            delay_guard: DelayGuard::default(),
            gain,
            last_callback: None,
            primed: false,
            prime_frames,
            flushed: false,
            ramp_step,
            declick: 0,
            last_generation: None,
            declick_frames,
            level: Level::new(level_ramp_frames),
            dither: Dither::new(),
        }
    }

    /// Drops `frames` frames of the sample ring unplayed.
    fn discard(&mut self, frames: u32) {
        if frames == 0 {
            return;
        }
        self.flushed = true;
        let n = frames as usize * self.shared.channels;
        if let Ok(chunk) = self.samples.read_chunk(n) {
            chunk.commit_all();
            self.shared
                .frames_consumed
                .fetch_add(u64::from(frames), Ordering::AcqRel);
        }
    }

    /// Makes `current` a playable segment, skipping flushed ones. False if none is queued.
    ///
    /// A full flush (seek, new track) cuts what is playing: the cut-off audio is played
    /// for a short fade-out instead of stopping dead. Content dropped by a partial flush
    /// (a replaced next track) is never heard at all; it is told apart by its generation
    /// being newer than what plays, rather than older than the flush.
    fn next_segment(&mut self) -> bool {
        loop {
            if let Some((segment, remaining)) = self.current {
                if remaining > 0 {
                    if self.declick > 0 {
                        return true;
                    }
                    let state = self.shared.flush.load();
                    if state.allows(segment.generation) {
                        return true;
                    }
                    if self.start_declick(segment.generation, remaining) {
                        return true;
                    }
                }
                self.discard(remaining);
                self.current = None;
            }
            let Ok(segment) = self.segments.pop() else {
                return false;
            };
            if !self.shared.flush.commit(segment.generation) {
                if self.start_declick(segment.generation, segment.frames) {
                    // Its fade-out is played content: report it, so the content index
                    // stays covered by reports (heard time, position), but never as a
                    // track start, since the rest of it is dropped.
                    let mut reported = segment;
                    reported.flags &= !flags::TRACK_START;
                    self.report(reported);
                    self.current = Some((segment, segment.frames));
                    return true;
                }
                self.discard(segment.frames);
                continue;
            }
            // Crossing a full flush (seek, new track) cuts the waveform even when the old
            // audio had already run out: the new audio fades in from silence.
            let lo = self.shared.flush.load().lo;
            if self
                .last_generation
                .is_some_and(|g| g < lo && segment.generation >= lo)
            {
                self.gain = 0.0;
                // Behind the dropped audio there may be only the first few frames of the
                // new audio (whatever fitted when the flush came); start once enough is
                // buffered, or it runs dry right away.
                self.primed = false;
            }
            self.last_generation = Some(segment.generation);
            self.report(segment);
            self.current = Some((segment, segment.frames));
        }
    }

    /// Tells the decode/control side where `segment` begins in the content index.
    fn report(&mut self, segment: Segment) {
        let report = SegmentReport {
            content_index: self.content_index,
            segment,
        };
        if self.reports.push(report).is_err() {
            self.shared.reports_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Starts fading out flushed audio of `generation` if it was cut off by a full flush
    /// while audible. Returns whether a declick is now running.
    fn start_declick(&mut self, generation: u32, available: u32) -> bool {
        let cut_off = generation < self.shared.flush.load().lo;
        if !cut_off || self.gain == 0.0 || !self.primed {
            return false;
        }
        self.declick = available.min(self.declick_frames);
        true
    }

    /// While paused, flushed data must still leave the ring so the producer is not stuck
    /// behind it. Valid segments stay put.
    fn discard_flushed(&mut self) {
        if let Some((segment, remaining)) = self.current
            && !self.shared.flush.load().allows(segment.generation)
        {
            self.discard(remaining);
            self.current = None;
        }
        while let Ok(segment) = self.segments.peek().copied() {
            if self.shared.flush.load().allows(segment.generation) {
                break;
            }
            let _ = self.segments.pop();
            self.discard(segment.frames);
        }
    }

    /// Writes up to `frames` content frames through `write(sample_index, value, dither)`
    /// and returns how many were written; they are always the first ones of the buffer.
    /// `dither`: a 16-bit integer output should dither the sample, because it was scaled
    /// by something other than exactly 1.0 or 0.0, or its segment is not clean for 16
    /// bits ([`flags::NEEDS_DITHER`]). True silence (a multiplier of 0.0) never is.
    fn fill(&mut self, frames: usize, mut write: impl FnMut(usize, f32, bool)) -> usize {
        let channels = self.shared.channels;
        let volume = f32::from_bits(self.shared.volume.load(Ordering::Relaxed));
        let target = if self.shared.paused.load(Ordering::Acquire) {
            0.0
        } else {
            1.0
        };
        if target == 0.0 && self.content_index == 0 {
            // Paused before anything was heard (e.g. restoring a session): nothing to
            // fade out, and fading would use up the first frames.
            self.gain = 0.0;
        }
        if target == 0.0 && self.gain == 0.0 {
            self.shared.silent.store(true, Ordering::Release);
            self.discard_flushed();
            return 0;
        }
        self.shared.silent.store(false, Ordering::Release);
        if !self.primed {
            self.discard_flushed();
            if !self.try_prime() {
                return 0;
            }
        }

        let mut written = 0;
        while written < frames && self.next_segment() {
            // Crossing a full flush un-primes (see `next_segment`): wait until enough of
            // the new audio is buffered rather than start on its first frames.
            if !self.primed && !self.try_prime() {
                break;
            }
            let Some((segment, remaining)) = self.current else {
                break;
            };
            // A track's gain is only ever applied to that track: a loud track after a
            // boosted quiet one must not start at the boost (it would clip). So a
            // lower gain ahead at the next track's start is ramped to over this track's
            // last frames, landing exactly on the boundary; a higher one is ramped to
            // over the next track's first frames, as are volume and `set_gain` changes
            // within a track (the same music on both sides).
            let own = segment.gain * volume;
            let lower_ahead = self
                .next_track_gain(remaining, volume)
                .filter(|&(next, _)| next < own);
            let ramp = self.level.ramp_frames;
            // Nothing audible (start-up, fading in from silence after a seek or a
            // pause): start right at the gain.
            let inaudible = self.content_index == 0 || self.gain == 0.0;
            let track_start =
                remaining == segment.frames && segment.flags & flags::TRACK_START != 0;
            match lower_ahead {
                Some((next, until)) if until <= ramp && !inaudible => {
                    self.level.ramp_within(next, until);
                }
                // Reached without a ramp (the next track was not queued yet when this
                // one's tail played): never start a track louder than its own gain.
                _ if track_start && own < self.level.value => self.level.retarget(own, true),
                _ => self.level.retarget(own, inaudible),
            }
            let mut n = (remaining as usize).min(frames - written);
            if let Some((_, until)) = lower_ahead
                && until > ramp
            {
                // Stop where the tail ramp must begin.
                n = n.min((until - ramp) as usize);
            }
            let declicking = self.declick > 0;
            if declicking {
                n = n.min(self.declick as usize);
            }
            let declick_step = 1.0 / self.declick_frames as f32;
            let unclean = segment.flags & flags::NEEDS_DITHER != 0;
            let Ok(chunk) = self.samples.read_chunk(n * channels) else {
                break;
            };
            let (first, second) = chunk.as_slices();
            let mut taken = 0;
            let mut index = written * channels;
            'frames: for part in [first, second] {
                for frame in part.chunks_exact(channels) {
                    if declicking {
                        self.gain = (self.gain - declick_step).max(0.0);
                    } else if self.gain != target {
                        self.gain = if target > self.gain {
                            (self.gain + self.ramp_step).min(1.0)
                        } else {
                            (self.gain - self.ramp_step).max(0.0)
                        };
                    }
                    let multiplier = self.gain * self.level.step();
                    // A multiplier of exactly 1.0 must be a no-op: skip the
                    // multiply and the dither. Exactly 0.0 is true silence, undithered.
                    let dither = multiplier != 0.0 && (multiplier != 1.0 || unclean);
                    for &sample in frame {
                        let value = if multiplier == 1.0 {
                            sample
                        } else if multiplier == 0.0 {
                            0.0
                        } else {
                            sample * multiplier
                        };
                        write(index, value, dither);
                        index += 1;
                    }
                    taken += 1;
                    if self.gain == 0.0 && target == 0.0 {
                        break 'frames;
                    }
                }
            }
            chunk.commit(taken * channels);
            self.shared
                .frames_consumed
                .fetch_add(taken as u64, Ordering::AcqRel);
            self.current = Some((segment, remaining - taken as u32));
            written += taken;
            self.content_index += taken as u64;
            if declicking {
                self.declick -= taken as u32;
                if self.declick == 0 {
                    // Faded out: drop the rest of the cut-off audio; what follows
                    // fades in from silence.
                    self.discard(remaining - taken as u32);
                    self.current = None;
                    self.gain = 0.0;
                    // A flush happened even if nothing was left to discard: running dry
                    // now is the seek's gap, and the new audio must prime first.
                    self.flushed = true;
                    self.primed = false;
                }
            }
            if self.gain == 0.0 && target == 0.0 {
                self.shared.silent.store(true, Ordering::Release);
                break;
            }
        }
        if target == 0.0 && written == 0 {
            // Paused with nothing to fade out (start-up, underrun, end of queue): be
            // silent now, so the next content does not start with a fade-out blip.
            self.gain = 0.0;
            self.shared.silent.store(true, Ordering::Release);
            self.discard_flushed();
        }
        written
    }

    /// The gain × volume at the start of the next track, if that is queued already, and
    /// how many frames play before it (`remaining` of the current segment plus the
    /// segments in between). Looks without consuming: an uncommitted read chunk.
    fn next_track_gain(&mut self, remaining: u32, volume: f32) -> Option<(f32, u32)> {
        let flush = self.shared.flush.load();
        let chunk = self.segments.read_chunk(self.segments.slots()).ok()?;
        let (first, second) = chunk.as_slices();
        let mut until = remaining;
        for next in first.iter().chain(second) {
            if !flush.allows(next.generation) {
                return None;
            }
            if next.flags & flags::TRACK_START != 0 {
                return Some((next.gain * volume, until));
            }
            until = until.saturating_add(next.frames);
            if until >= remaining.saturating_add(self.level.ramp_frames) {
                // Too far ahead to matter for this segment; looked at again later.
                return None;
            }
        }
        None
    }

    /// Primed once `prime_frames` are buffered (or nothing more will come).
    fn try_prime(&mut self) -> bool {
        let buffered = self.samples.slots() / self.shared.channels;
        let ending = self.shared.end_of_stream.load(Ordering::Acquire) != 0;
        if buffered >= self.prime_frames || ending {
            self.primed = true;
        }
        self.primed
    }

    /// Counts callbacks that arrive later than the previous buffer could cover.
    fn check_timing(&mut self, frames: usize) {
        let now = self.shared.epoch.elapsed();
        let period =
            std::time::Duration::from_secs_f64(frames as f64 / f64::from(self.shared.sample_rate));
        if let Some((last, last_period)) = self.last_callback {
            let gap = now.saturating_sub(last);
            self.shared
                .max_callback_gap_us
                .fetch_max(gap.as_micros() as u64, Ordering::Relaxed);
            // Backends may batch or jitter by a period; well past two periods the
            // device has most likely played silence.
            if gap > last_period * 2 + std::time::Duration::from_millis(5) {
                self.shared.late_callbacks.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.last_callback = Some((now, period));
    }

    fn queue_is_empty(&self) -> bool {
        self.current.is_none_or(|(_, remaining)| remaining == 0) && self.segments.is_empty()
    }
}

impl Renderer for EngineRenderer {
    fn render(&mut self, out: SampleBufMut<'_>, info: &RenderInfo) {
        self.shared.callbacks.fetch_add(1, Ordering::Release);
        // Check before consuming any samples, including the first callback after wake.
        if self
            .shared
            .playback_deadline
            .as_ref()
            .is_some_and(|d| d.expired())
        {
            match out {
                SampleBufMut::F32(buf) => buf.fill(0.0),
                SampleBufMut::I16(buf) => buf.fill(0),
                SampleBufMut::I32(buf) => buf.fill(0),
            }
            return;
        }
        let channels = self.shared.channels;
        let frames = out.len() / channels;
        let (delay, untrusted) = self.delay_guard.check(info.playback_delay);
        if untrusted {
            self.shared.clock_unreliable.store(true, Ordering::Release);
        }
        self.check_timing(frames);
        let content_start = self.content_index;
        self.flushed = false;
        let filled = match out {
            SampleBufMut::F32(buf) => {
                let filled = self.fill(frames, |i, s, _| buf[i] = s);
                buf[filled * channels..].fill(0.0);
                filled
            }
            SampleBufMut::I16(buf) => {
                // A copy, so the closure can use it while `fill` borrows `self`.
                let mut dither = self.dither;
                let filled = self.fill(frames, |i, s, dithered| {
                    buf[i] = if dithered {
                        dither.quantize_i16(s)
                    } else {
                        quantize_i16(s)
                    };
                });
                self.dither = dither;
                buf[filled * channels..].fill(0);
                filled
            }
            // At 24 bits and more the rounding error is far below audibility.
            SampleBufMut::I32(buf) => {
                let filled = self.fill(frames, |i, s, _| buf[i] = quantize_i32(s));
                buf[filled * channels..].fill(0);
                filled
            }
        };
        self.shared
            .applied_gain
            .store(self.level.value.to_bits(), Ordering::Relaxed);

        self.shared
            .frames_played
            .store(self.content_index, Ordering::Release);
        // Only buffers with content move the clock. Publishing silent ones would make
        // the audible index fall back by the output latency during pauses and at the end.
        if filled > 0 {
            let tail_delay = self.delay_guard.completion_delay();
            if !tail_delay.is_zero() {
                let tail = tail_delay
                    + std::time::Duration::from_secs_f64(
                        filled as f64 / f64::from(self.shared.sample_rate),
                    );
                self.shared.tail_at_ns.fetch_max(
                    self.shared.now_ns().saturating_add(tail.as_nanos() as u64),
                    Ordering::Release,
                );
            }
            let playback_at_ns = self
                .shared
                .now_ns()
                .saturating_add(delay.as_nanos().min(u128::from(u64::MAX)) as u64);
            self.shared.clock.publish(ClockSample {
                content_start,
                content_frames: filled as u64,
                playback_at_ns,
            });
        }

        let paused = self.shared.paused.load(Ordering::Acquire);
        let closing = self.shared.closing.load(Ordering::Acquire);
        if filled < frames && (!paused || closing) {
            // Load the flag before looking at the queue: the decode thread writes the
            // last segment before it raises the flag.
            let end = self.shared.end_of_stream.load(Ordering::Acquire);
            // A full flush can empty the ring without rendering the new epoch. Only
            // an accepted tail still valid under the flush may complete while paused;
            // this also works when the last content buffer was exactly full.
            let rendered_tail = !closing
                || self
                    .last_generation
                    .is_some_and(|g| self.shared.flush.load().allows(g));
            if end != 0 && self.queue_is_empty() && rendered_tail {
                self.shared
                    .drained_at
                    .store(self.content_index, Ordering::Release);
                self.shared.drained.store(end, Ordering::Release);
            } else if !paused && !closing && self.primed {
                // Ran dry while playing. Right after a flush this is the expected gap
                // of a seek or a new track, not a glitch.
                if !self.flushed {
                    self.shared.underruns.fetch_add(1, Ordering::Relaxed);
                }
                self.primed = false;
            }
        }
    }
}

/// Rounds to the nearest integer sample. symphonia converts i16 to f32 by dividing by
/// 2^15, so this is the exact inverse on an untouched path. Samples the renderer
/// scaled (volume, ReplayGain, ramps), and samples of segments that are not exact 16-bit
/// values, go through [`Dither::quantize_i16`] instead.
fn quantize_i16(sample: f32) -> i16 {
    (sample * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16
}

/// A multiplier that moves linearly to a new target over a fixed number of frames. The
/// ramp is counted, so it always ends exactly on the target (1.0 included, which keeps
/// unity bit-perfect), however small the step.
#[derive(Debug, Clone, Copy)]
struct Level {
    value: f32,
    target: f32,
    delta: f32,
    left: u32,
    ramp_frames: u32,
}

impl Level {
    fn new(ramp_frames: u32) -> Self {
        Self {
            value: 1.0,
            target: 1.0,
            delta: 0.0,
            left: 0,
            ramp_frames,
        }
    }

    /// Starts ramping to `target`, or jumps there if `jump`.
    fn retarget(&mut self, target: f32, jump: bool) {
        if jump {
            self.value = target;
            self.target = target;
            self.left = 0;
        } else if target != self.target {
            self.target = target;
            self.left = self.ramp_frames;
            self.delta = (target - self.value) / self.ramp_frames as f32;
        }
    }

    /// Ramps to `target` so that it is reached after exactly `frames` more frames (the
    /// end of the current segment). Keeps a ramp already heading there.
    fn ramp_within(&mut self, target: f32, frames: u32) {
        if target == self.target && (self.left == frames || self.value == target) {
            return;
        }
        self.target = target;
        self.left = frames.max(1);
        self.delta = (target - self.value) / self.left as f32;
    }

    /// Advances one frame and returns the multiplier for it.
    fn step(&mut self) -> f32 {
        if self.left > 0 {
            self.left -= 1;
            self.value = if self.left == 0 {
                self.target
            } else {
                self.value + self.delta
            };
        }
        self.value
    }
}

/// TPDF dither for 16-bit output: triangular noise of ±1 LSB, the difference of
/// two uniform randoms, added before rounding. It turns the rounding error of a scaled
/// signal into benign, signal-independent noise. A deterministic xorshift32; no
/// allocation, so fine on the realtime thread.
#[derive(Debug, Clone, Copy)]
struct Dither(u32);

impl Dither {
    fn new() -> Self {
        Self(0x9E37_79B9)
    }

    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Triangular noise in (-1, 1), in LSB.
    fn tpdf(&mut self) -> f32 {
        // 24-bit uniforms convert to f32 exactly.
        let a = (self.next() >> 8) as f32;
        let b = (self.next() >> 8) as f32;
        (a - b) * (1.0 / 16_777_216.0)
    }

    fn quantize_i16(&mut self, sample: f32) -> i16 {
        (sample * 32_768.0 + self.tpdf())
            .round()
            .clamp(-32_768.0, 32_767.0) as i16
    }
}

/// i32 needs f64: f32 cannot hold 2^31-scaled values exactly.
fn quantize_i32(sample: f32) -> i32 {
    (f64::from(sample) * 2_147_483_648.0)
        .round()
        .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_callbacks_advance_while_paused_or_starved() {
        let (_samples, sample_rx) = rtrb::RingBuffer::new(128);
        let (_segments, segment_rx) = rtrb::RingBuffer::new(16);
        let (reports, _report_rx) = rtrb::RingBuffer::new(16);
        let shared = Arc::new(Shared::new(2, 44_100));
        let mut renderer = EngineRenderer::new(sample_rx, segment_rx, reports, shared.clone());
        let mut buffer = [1.0; 128];
        shared.paused.store(true, Ordering::Release);
        renderer.render(SampleBufMut::F32(&mut buffer), &RenderInfo::default());
        assert_eq!(shared.callbacks.load(Ordering::Acquire), 1);
        assert_eq!(shared.frames_played.load(Ordering::Acquire), 0);
        shared.paused.store(false, Ordering::Release);
        renderer.render(SampleBufMut::F32(&mut buffer), &RenderInfo::default());
        assert_eq!(shared.callbacks.load(Ordering::Acquire), 2);
        assert_eq!(shared.frames_played.load(Ordering::Acquire), 0);
        assert!(buffer.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn quantization_inverts_symphonia_scaling() {
        for v in [i16::MIN, -12_345, -1, 0, 1, 12_345, i16::MAX] {
            assert_eq!(quantize_i16(f32::from(v) / 32_768.0), v);
        }
        for v in [-8_388_608i32, -1, 0, 1, 8_388_607] {
            // 24-bit content, left-justified in i32.
            let left_justified = v << 8;
            let as_f32 = (f64::from(left_justified) / 2_147_483_648.0) as f32;
            assert_eq!(quantize_i32(as_f32), left_justified);
        }
        assert_eq!(quantize_i16(1.5), i16::MAX);
        assert_eq!(quantize_i16(-1.5), i16::MIN);
    }

    #[test]
    fn level_ramps_linearly_and_lands_exactly() {
        let mut level = Level::new(100);
        level.retarget(0.5, false);
        let ramp: Vec<f32> = (0..100).map(|_| level.step()).collect();
        assert!((ramp[49] - 0.75).abs() < 1e-6, "{}", ramp[49]);
        assert_eq!(ramp[99], 0.5);
        assert_eq!(level.step(), 0.5);
        // Back to unity, even by a step too small to change an f32 near 1.0.
        level.retarget(1.0 - f32::EPSILON / 2.0, true);
        level.retarget(1.0, false);
        let last = (0..100).map(|_| level.step()).last().unwrap();
        assert_eq!(last, 1.0);
        // A new target mid-ramp starts from where the ramp is.
        level.retarget(0.0, false);
        (0..50).for_each(|_| {
            level.step();
        });
        level.retarget(1.0, false);
        assert!((level.step() - 0.505).abs() < 1e-3);
    }

    #[test]
    fn dither_is_triangular_and_within_one_lsb() {
        let mut dither = Dither::new();
        let n = 200_000;
        let noise: Vec<f32> = (0..n).map(|_| dither.tpdf()).collect();
        assert!(noise.iter().all(|v| v.abs() < 1.0));
        let mean = noise.iter().sum::<f32>() / n as f32;
        assert!(mean.abs() < 0.01, "mean {mean}");
        // Triangular on (-1, 1): variance 1/6. Uniform would be 1/3.
        let variance = noise.iter().map(|v| v * v).sum::<f32>() / n as f32;
        assert!((variance - 1.0 / 6.0).abs() < 0.01, "variance {variance}");
        // Within (-0.5, 0.5): 3/4 of a triangle's mass, but only 1/2 of a uniform's.
        let middle = noise.iter().filter(|v| v.abs() < 0.5).count() as f32 / n as f32;
        assert!((middle - 0.75).abs() < 0.01, "middle {middle}");
    }

    #[test]
    fn dithered_quantization_stays_within_one_and_a_half_lsb() {
        let mut dither = Dither::new();
        for i in 0..10_000 {
            let exact = (i as f32 - 5_000.0) * 3.7 + 0.31;
            let q = dither.quantize_i16(exact / 32_768.0);
            assert!((f32::from(q) - exact).abs() <= 1.5, "{exact} -> {q}");
        }
    }
}
