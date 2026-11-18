//! Turns a sequence of (already trimmed) tracks into device-rate output pieces with exact
//! segment metadata.
//!
//! Tracks of the same format share one resampler stream, so a seam is filtered exactly as
//! continuous audio would be. A format change drains the old stream and starts a new one.
//! The last track boundary can be rewound for a partial flush; the resampler is then
//! rebuilt from recent input history at a call boundary, which reproduces the original
//! output exactly (`dsp::tests::restart_at_call_boundary_reproduces_output`).

use std::collections::VecDeque;

use crate::EngineError;
use crate::dsp::{StreamResampler, map_channels};
use crate::segment::{EntryId, Generation, Segment, flags};

/// Calls of input history kept before a boundary, so that a rebuilt resampler has settled
/// (one block of overlap plus its delay) before the first frame it must reproduce.
const REBUILD_MARGIN_CALLS: u64 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrackStart {
    pub generation: Generation,
    pub entry: EntryId,
    pub source_rate: u32,
    pub source_channels: usize,
    /// Track frame of the first input frame that will follow.
    pub source_start: u64,
    /// False when restarting inside a track (seek): no `TrackStarted` event then.
    pub track_start: bool,
    /// The track's output may not be exact 16-bit values ([`flags::NEEDS_DITHER`]).
    pub needs_dither: bool,
}

#[derive(Debug, Clone, Copy)]
struct Mark {
    in_index: u64,
    out_index: u64,
    start: TrackStart,
}

impl Mark {
    fn source_at(&self, out_index: u64, out_rate: u32) -> u64 {
        self.start.source_start
            + (out_index - self.out_index) * u64::from(self.start.source_rate) / u64::from(out_rate)
    }
}

struct Piece {
    /// Interleaved, at the output channel count.
    samples: Vec<f32>,
    out_index: u64,
    mark: Mark,
}

struct Stream {
    rate: u32,
    channels: usize,
    resampler: Option<StreamResampler>,
    /// Stream input/output index of the resampler's first input / first trimmed output.
    base_in: u64,
    base_out: u64,
    in_total: u64,
    out_total: u64,
    /// Outputs below this index are dropped (already produced before a rebuild).
    discard_until: u64,
    marks: Vec<Mark>,
    /// The most recent input frames, from stream index `history_start`.
    history: VecDeque<f32>,
    history_start: u64,
    history_cap: usize,
}

impl Stream {
    fn new(rate: u32, channels: usize, out_rate: u32) -> Result<Self, EngineError> {
        let resampler = if rate == out_rate {
            None
        } else {
            Some(StreamResampler::new(rate, out_rate, channels)?)
        };
        let chunk = resampler.as_ref().map_or(0, |r| r.chunk_frames());
        Ok(Self {
            rate,
            channels,
            history_cap: chunk * (REBUILD_MARGIN_CALLS as usize + 2),
            resampler,
            base_in: 0,
            base_out: 0,
            in_total: 0,
            out_total: 0,
            discard_until: 0,
            marks: Vec::new(),
            history: VecDeque::new(),
            history_start: 0,
        })
    }

    /// Output index corresponding to input index `input`.
    fn out_for(&self, input: u64) -> u64 {
        match &self.resampler {
            Some(r) => self.base_out + r.out_frames_for(input - self.base_in),
            None => input,
        }
    }

    /// Where to restart a resampler so that it reproduces this stream's output from
    /// input `boundary` on: a recorded call boundary at least `REBUILD_MARGIN_CALLS` calls
    /// earlier and still inside the history, as (stream input index, stream output index).
    fn restart_point(&self, boundary: u64) -> Option<(u64, u64)> {
        let resampler = self.resampler.as_ref()?;
        let margin = REBUILD_MARGIN_CALLS * resampler.chunk_frames() as u64;
        let wanted = boundary.saturating_sub(margin).max(self.history_start);
        match resampler.call_boundary_before(wanted - self.base_in) {
            Some((consumed, raw)) if self.base_in + consumed >= self.history_start => {
                Some((self.base_in + consumed, self.base_out + raw))
            }
            _ if self.history_start == self.base_in => Some((self.base_in, self.base_out)),
            _ => None,
        }
    }

    fn remember(&mut self, input: &[f32]) {
        if self.history_cap == 0 {
            return;
        }
        self.history.extend(input.iter().copied());
        let excess = (self.history.len() / self.channels).saturating_sub(self.history_cap);
        self.history.drain(..excess * self.channels);
        self.history_start += excess as u64;
    }
}

enum Boundary {
    /// The track started inside a running stream. `clean_until`: outputs below it were
    /// produced before any of the track's input entered the resampler, so they do not
    /// depend on it and survive a rewind. `history`: the stream's input history at the
    /// boundary (the resampler checkpoint), since later input pushes it out.
    Continuous {
        mark: Mark,
        clean_until: u64,
        history: Vec<f32>,
        history_start: u64,
        /// The resampler call boundary to rebuild from, chosen now: by the time of a
        /// rewind the replaced track may have pushed it out of the call log.
        restart: Option<(u64, u64)>,
    },
    /// The track started a new stream; the previous one had been drained.
    Fresh(Generation),
}

pub(crate) struct Pipeline {
    out_rate: u32,
    out_channels: usize,
    stream: Option<Stream>,
    pending: VecDeque<Piece>,
    /// The last track boundary, while it can still be rewound.
    boundary: Option<Boundary>,
    resampled: Vec<f32>,
}

impl Pipeline {
    pub fn new(out_rate: u32, out_channels: usize) -> Self {
        Self {
            out_rate,
            out_channels,
            stream: None,
            pending: VecDeque::new(),
            boundary: None,
            resampled: Vec::new(),
        }
    }

    /// Starts a track (or a restart inside one) at the current position.
    pub fn begin(&mut self, start: TrackStart) -> Result<(), EngineError> {
        let continues = self
            .stream
            .as_ref()
            .is_some_and(|s| s.rate == start.source_rate && s.channels == start.source_channels);
        if continues {
            let stream = self.stream.as_mut().expect("checked above");
            let mark = Mark {
                in_index: stream.in_total,
                out_index: stream.out_for(stream.in_total),
                start,
            };
            stream.marks.push(mark);
            if stream.marks.len() > 8 {
                stream.marks.remove(0);
            }
            self.boundary = start.track_start.then(|| Boundary::Continuous {
                mark,
                clean_until: stream.out_total,
                history: stream.history.iter().copied().collect(),
                history_start: stream.history_start,
                restart: stream.restart_point(mark.in_index),
            });
        } else {
            self.finish()?;
            let mut stream = Stream::new(start.source_rate, start.source_channels, self.out_rate)?;
            stream.marks.push(Mark {
                in_index: 0,
                out_index: 0,
                start,
            });
            self.stream = Some(stream);
            self.boundary = start
                .track_start
                .then_some(Boundary::Fresh(start.generation));
        }
        Ok(())
    }

    /// Feeds interleaved input of the current track at its source channel count.
    pub fn push(&mut self, input: &[f32]) -> Result<(), EngineError> {
        let stream = self.stream.as_mut().expect("begin() must precede push()");
        stream.remember(input);
        stream.in_total += (input.len() / stream.channels) as u64;
        match &mut stream.resampler {
            Some(resampler) => {
                let before = stream.base_out + resampler.produced();
                self.resampled.clear();
                resampler.push(input, &mut self.resampled)?;
                let resampled = std::mem::take(&mut self.resampled);
                self.emit(before, &resampled);
                self.resampled = resampled;
            }
            None => {
                let before = stream.out_total;
                self.emit(before, input);
            }
        }
        Ok(())
    }

    /// Drains the current stream: every input frame now has its output. Called at the end
    /// of the queue and before a format change.
    pub fn finish(&mut self) -> Result<(), EngineError> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(());
        };
        let target = stream.out_for(stream.in_total);
        if let Some(resampler) = &mut stream.resampler {
            let before = stream.base_out + resampler.produced();
            self.resampled.clear();
            resampler.flush_to(target - stream.base_out, &mut self.resampled)?;
            let resampled = std::mem::take(&mut self.resampled);
            self.emit(before, &resampled);
            self.resampled = resampled;
        }
        self.stream = None;
        Ok(())
    }

    /// Drops everything: queued output, stream state, the rewindable boundary.
    pub fn reset(&mut self) {
        self.stream = None;
        self.pending.clear();
        self.boundary = None;
    }

    /// Undoes the boundary at which the track with `generation` began, for a partial
    /// flush: its output and later is dropped, and the stream is restored to
    /// the state it had right before, so the replacement continues exactly where the
    /// previous track ends. Returns false (and changes nothing) if the last boundary is
    /// not that track's.
    pub fn rewind(&mut self, generation: Generation) -> Result<bool, EngineError> {
        let matches = match &self.boundary {
            Some(Boundary::Fresh(g)) => *g == generation,
            Some(Boundary::Continuous { mark, .. }) => mark.start.generation == generation,
            None => false,
        };
        if !matches {
            return Ok(false);
        }
        match self.boundary.take() {
            Some(Boundary::Fresh(generation)) => {
                self.pending
                    .retain(|p| p.mark.start.generation < generation);
                self.stream = None;
            }
            Some(Boundary::Continuous {
                mark,
                clean_until,
                history,
                history_start,
                restart,
            }) => {
                let stream = self
                    .stream
                    .as_mut()
                    .expect("a continuous boundary keeps its stream");
                stream.history = history.into();
                stream.history_start = history_start;
                self.rebuild_before(mark, clean_until, restart)?;
            }
            None => unreachable!("checked above"),
        }
        Ok(true)
    }

    fn rebuild_before(
        &mut self,
        mark: Mark,
        clean_until: u64,
        restart: Option<(u64, u64)>,
    ) -> Result<(), EngineError> {
        let out_rate = self.out_rate;
        let stream = self
            .stream
            .as_mut()
            .expect("a continuous boundary has a stream");
        // Output still queued here can be regenerated; what already left for the ring
        // cannot. Keep queued output only while it is clean, regenerate the rest. If the
        // tail of the previous track was already written with the replaced track in the
        // resampler's lookahead, that short stretch stays as it was (the rare
        // approximate case).
        let written_end = self
            .pending
            .front()
            .map_or(stream.out_total, |p| p.out_index);
        // Never past the boundary: output of the replaced track that already reached the
        // ring is flushed there, and the replacement starts exactly at the boundary.
        let produced = written_end.max(clean_until).min(mark.out_index);
        let channels = self.out_channels;
        self.pending.retain_mut(|p| {
            if p.out_index >= produced {
                return false;
            }
            let keep = (produced - p.out_index) as usize * channels;
            p.samples.truncate(keep);
            true
        });
        stream.marks.retain(|m| m.in_index < mark.in_index);
        stream.in_total = mark.in_index;
        stream.out_total = produced;

        if stream.resampler.is_none() {
            // Passthrough: every frame before the boundary is already out.
            return Ok(());
        }
        let (start_in, start_out) = match restart {
            Some(point) => point,
            None => {
                tracing::warn!("resampler history too short; the seam will not be exact");
                (stream.history_start, stream.out_for(stream.history_start))
            }
        };

        let mut rebuilt = StreamResampler::new(stream.rate, out_rate, stream.channels)?;
        let replay: Vec<f32> = stream
            .history
            .iter()
            .skip((start_in - stream.history_start) as usize * stream.channels)
            .copied()
            .collect();
        stream.base_in = start_in;
        stream.base_out = start_out;
        stream.discard_until = produced;
        self.resampled.clear();
        rebuilt.push(&replay, &mut self.resampled)?;
        stream.resampler = Some(rebuilt);
        let resampled = std::mem::take(&mut self.resampled);
        self.emit(start_out, &resampled);
        self.resampled = resampled;
        Ok(())
    }

    /// Splits output frames starting at stream index `first` into pieces by mark.
    fn emit(&mut self, first: u64, samples: &[f32]) {
        let stream = self.stream.as_mut().expect("emit needs a stream");
        let channels = stream.channels;
        let frames = (samples.len() / channels) as u64;
        let mut o = first.max(stream.discard_until);
        let end = first + frames;
        while o < end {
            let index = stream.marks.partition_point(|m| m.out_index <= o);
            let mark = stream.marks[index.saturating_sub(1)];
            let piece_end = stream
                .marks
                .get(index)
                .map_or(end, |next| next.out_index.min(end));
            let range = (o - first) as usize * channels..(piece_end - first) as usize * channels;
            let mut mapped = Vec::with_capacity((piece_end - o) as usize * self.out_channels);
            map_channels(&samples[range], channels, self.out_channels, &mut mapped);
            self.pending.push_back(Piece {
                samples: mapped,
                out_index: o,
                mark,
            });
            o = piece_end;
        }
        stream.out_total = stream.out_total.max(end);
    }

    /// The next output to write: up to `max_frames` frames and the segment describing them.
    pub fn peek(&self, max_frames: usize) -> Option<(&[f32], Segment)> {
        let piece = self.pending.front()?;
        let frames = (piece.samples.len() / self.out_channels).min(max_frames);
        if frames == 0 {
            return None;
        }
        let mark = piece.mark;
        let source_start = mark.source_at(piece.out_index, self.out_rate);
        let source_end = mark.source_at(piece.out_index + frames as u64, self.out_rate);
        let segment = Segment {
            generation: mark.start.generation,
            entry: mark.start.entry,
            frames: frames as u32,
            source_start,
            source_frames: (source_end - source_start) as u32,
            source_rate: mark.start.source_rate,
            flags: if mark.start.track_start && piece.out_index == mark.out_index {
                flags::TRACK_START
            } else {
                0
            } | if mark.start.needs_dither {
                flags::NEEDS_DITHER
            } else {
                0
            },
            // The decode thread sets the track's gain when it writes the segment.
            gain: 1.0,
        };
        Some((&piece.samples[..frames * self.out_channels], segment))
    }

    /// Marks `frames` frames from [`peek`](Self::peek) as written.
    pub fn advance(&mut self, frames: usize) {
        let Some(piece) = self.pending.front_mut() else {
            return;
        };
        let samples = frames * self.out_channels;
        if samples >= piece.samples.len() {
            self.pending.pop_front();
        } else {
            piece.samples.drain(..samples);
            piece.out_index += frames as u64;
        }
    }

    pub fn has_output(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether more output of `entry` may still come out of the pipeline: queued pieces,
    /// or input still inside the resampler (its mark is in the running stream).
    pub fn holds(&self, entry: EntryId) -> bool {
        self.pending.iter().any(|p| p.mark.start.entry == entry)
            || self
                .stream
                .as_ref()
                .is_some_and(|s| s.marks.iter().any(|m| m.start.entry == entry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(generation: Generation, entry: u64, rate: u32, channels: usize) -> TrackStart {
        TrackStart {
            generation,
            entry: EntryId(entry),
            source_rate: rate,
            source_channels: channels,
            source_start: 0,
            track_start: true,
            needs_dither: false,
        }
    }

    fn signal(frames: usize, phase: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let t = i as f32 + phase;
                [(t * 0.031).sin() * 0.5, (t * 0.017).cos() * 0.5]
            })
            .collect()
    }

    /// Drains all output, returning samples and the segments describing them.
    fn drain(p: &mut Pipeline, max: usize) -> (Vec<f32>, Vec<Segment>) {
        let mut samples = Vec::new();
        let mut segments = Vec::new();
        while let Some((s, seg)) = p.peek(max) {
            samples.extend_from_slice(s);
            segments.push(seg);
            let frames = seg.frames as usize;
            p.advance(frames);
        }
        (samples, segments)
    }

    #[test]
    fn passthrough_tracks_are_segmented_at_the_boundary() {
        let mut p = Pipeline::new(44_100, 2);
        p.begin(start(1, 1, 44_100, 2)).unwrap();
        p.push(&signal(1000, 0.0)).unwrap();
        p.begin(start(2, 2, 44_100, 2)).unwrap();
        p.push(&signal(500, 0.0)).unwrap();
        let (samples, segments) = drain(&mut p, 300);
        assert_eq!(samples.len(), 1500 * 2);
        let firsts: Vec<_> = segments
            .iter()
            .filter(|s| s.flags & flags::TRACK_START != 0)
            .map(|s| (s.entry, s.source_start))
            .collect();
        assert_eq!(firsts, [(EntryId(1), 0), (EntryId(2), 0)]);
        let b: u32 = segments
            .iter()
            .filter(|s| s.entry == EntryId(2))
            .map(|s| s.frames)
            .sum();
        assert_eq!(b, 500);
        let last = segments.last().unwrap();
        assert_eq!(last.source_start + u64::from(last.source_frames), 500);
    }

    #[test]
    fn dither_need_is_marked_per_track() {
        let mut p = Pipeline::new(44_100, 2);
        p.begin(start(1, 1, 44_100, 2)).unwrap();
        p.push(&signal(1000, 0.0)).unwrap();
        p.begin(TrackStart {
            needs_dither: true,
            ..start(2, 2, 44_100, 2)
        })
        .unwrap();
        p.push(&signal(500, 0.0)).unwrap();
        let (_, segments) = drain(&mut p, 300);
        for s in &segments {
            assert_eq!(
                s.flags & flags::NEEDS_DITHER != 0,
                s.entry == EntryId(2),
                "{s:?}"
            );
        }
        assert!(p.holds(EntryId(2)), "its mark is in the running stream");
        assert!(!p.holds(EntryId(3)));
    }

    #[test]
    fn same_rate_tracks_share_the_resampler() {
        // A gapless seam through a resampler must equal resampling the joined signal.
        let whole = signal(30_000, 0.0);
        let (a, b) = whole.split_at(12_345 * 2);

        let mut joined = Pipeline::new(48_000, 2);
        joined.begin(start(1, 1, 44_100, 2)).unwrap();
        joined.push(&whole).unwrap();
        joined.finish().unwrap();
        let (reference, _) = drain(&mut joined, usize::MAX);

        let mut split = Pipeline::new(48_000, 2);
        split.begin(start(1, 1, 44_100, 2)).unwrap();
        split.push(a).unwrap();
        split.begin(start(2, 2, 44_100, 2)).unwrap();
        split.push(b).unwrap();
        split.finish().unwrap();
        let (output, segments) = drain(&mut split, 700);

        assert_eq!(output, reference);
        let a_frames: u64 = segments
            .iter()
            .filter(|s| s.entry == EntryId(1))
            .map(|s| u64::from(s.frames))
            .sum();
        assert_eq!(a_frames, (12_345u64 * 48_000).div_ceil(44_100));
        let b_frames: u64 = segments
            .iter()
            .filter(|s| s.entry == EntryId(2))
            .map(|s| u64::from(s.frames))
            .sum();
        assert_eq!(a_frames + b_frames, (30_000u64 * 48_000).div_ceil(44_100));
    }

    #[test]
    fn format_change_drains_and_restarts() {
        let mut p = Pipeline::new(48_000, 2);
        p.begin(start(1, 1, 44_100, 2)).unwrap();
        p.push(&signal(10_000, 0.0)).unwrap();
        p.begin(start(2, 2, 48_000, 1)).unwrap();
        p.push(&vec![0.25; 4_800]).unwrap();
        p.finish().unwrap();
        let (samples, segments) = drain(&mut p, usize::MAX);
        let a: u64 = segments
            .iter()
            .filter(|s| s.entry == EntryId(1))
            .map(|s| u64::from(s.frames))
            .sum();
        assert_eq!(a, (10_000u64 * 48_000).div_ceil(44_100));
        assert_eq!(samples.len() as u64, (a + 4_800) * 2);
        // Mono is duplicated, untouched, onto both output channels.
        assert!(samples[a as usize * 2..].iter().all(|&s| s == 0.25));
    }

    #[test]
    fn rewind_reproduces_the_continuous_seam() {
        // A, then B partly decoded, then B is replaced by C: the result must equal A
        // followed directly by C, bit for bit, as if B had never been there. The one
        // exception is A output that had already been written while B was in the
        // resampler's lookahead; it cannot be recalled.
        let whole = signal(40_000, 0.0);
        let (a, c) = whole.split_at(20_000 * 2);
        let b = signal(6_000, 777.0);

        let mut direct = Pipeline::new(48_000, 2);
        direct.begin(start(1, 1, 44_100, 2)).unwrap();
        direct.push(a).unwrap();
        direct.begin(start(3, 3, 44_100, 2)).unwrap();
        direct.push(c).unwrap();
        direct.finish().unwrap();
        let (reference, _) = drain(&mut direct, usize::MAX);

        // `b_written`: frames of B that also reached the ring before the flush (the
        // renderer discards them; they are not part of the result).
        for (already_written, b_written) in [
            (0usize, 0usize),
            (15_000, 0),
            (21_000, 0),
            (usize::MAX, 0),
            (usize::MAX, 3_000),
        ] {
            let mut p = Pipeline::new(48_000, 2);
            p.begin(start(1, 1, 44_100, 2)).unwrap();
            p.push(a).unwrap();
            p.begin(start(2, 2, 44_100, 2)).unwrap();
            p.push(&b).unwrap();
            // Part of the output may already have reached the ring before the flush.
            let mut written = Vec::new();
            let mut b_out = 0;
            while written.len() / 2 < already_written {
                let Some((s, seg)) = p.peek(500) else { break };
                let n = seg.frames as usize;
                if seg.entry == EntryId(2) {
                    if b_out >= b_written {
                        break;
                    }
                    b_out += n;
                } else {
                    written.extend_from_slice(s);
                }
                p.advance(n);
            }
            assert!(b_out >= b_written, "only {b_out} frames of B were produced");
            assert!(p.rewind(2).unwrap());
            p.begin(start(3, 3, 44_100, 2)).unwrap();
            p.push(c).unwrap();
            p.finish().unwrap();
            let (rest, segments) = drain(&mut p, usize::MAX);
            assert!(segments.iter().all(|s| s.entry != EntryId(2)));
            let written_frames = written.len() / 2;
            written.extend_from_slice(&rest);
            assert_eq!(written.len(), reference.len(), "written {already_written}");
            let mismatch: Vec<usize> = (0..written.len() / 2)
                .filter(|&f| written[2 * f..2 * f + 2] != reference[2 * f..2 * f + 2])
                .collect();
            if already_written <= 15_000 {
                assert!(
                    mismatch.is_empty(),
                    "written {already_written}: {} frames differ",
                    mismatch.len()
                );
            } else {
                // Only output written before the flush, with B in the lookahead, differs.
                assert!(
                    mismatch
                        .iter()
                        .all(|&f| (19_000..written_frames).contains(&f)),
                    "written {already_written}: differences outside the written tail: {:?}",
                    mismatch.first()
                );
            }
        }
    }

    #[test]
    fn rewind_of_a_fresh_stream_drops_it() {
        let mut p = Pipeline::new(48_000, 2);
        p.begin(start(1, 1, 44_100, 2)).unwrap();
        p.push(&signal(5_000, 0.0)).unwrap();
        p.begin(start(2, 2, 48_000, 2)).unwrap();
        p.push(&signal(1_000, 0.0)).unwrap();
        assert!(
            !p.rewind(1).unwrap(),
            "only the last boundary can be rewound"
        );
        assert!(p.rewind(2).unwrap());
        assert!(!p.rewind(2).unwrap());
        p.begin(start(3, 3, 48_000, 2)).unwrap();
        p.push(&signal(2_000, 0.0)).unwrap();
        let (_, segments) = drain(&mut p, usize::MAX);
        assert!(segments.iter().all(|s| s.entry != EntryId(2)));
        let c: u32 = segments
            .iter()
            .filter(|s| s.entry == EntryId(3))
            .map(|s| s.frames)
            .sum();
        assert_eq!(c, 2_000);
    }
}
