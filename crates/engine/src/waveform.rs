//! Whole-track waveforms (docs/design/media-resources.md): a fixed number of
//! buckets, each with the minimum, maximum and RMS of its stretch of the track, computed on
//! the playback timeline. The track is decoded with the decoder playback uses and trimmed
//! the same way (encoder delay and padding removed, nothing before frame 0), so bucket `b`
//! covers the same frames a position bar shows there. No resampling, no gain, no volume.
//!
//! Needs no [`Engine`](crate::Engine): it runs on whatever thread calls it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mpmp_media_io::ByteSource;

use crate::decode::{Decoder, Seek, Side};
use crate::{EngineError, FrameRange};

/// Buckets in every waveform.
pub const WAVEFORM_BUCKETS: usize = 2048;

/// Fine slots kept while the length of the track is not known; when they are all used,
/// neighbours are merged in pairs (so a slot covers twice the frames). Each final bucket
/// then gathers at least `MAX_SLOTS / 2 / WAVEFORM_BUCKETS` = 16 slots (for tracks of at
/// least `MAX_SLOTS` frames; shorter tracks keep one frame per slot).
const MAX_SLOTS: usize = 32 * WAVEFORM_BUCKETS;

/// ITU-R BS.775 down-mix coefficient of a centre or surround channel (−3 dB).
const MIX_SIDE: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// A track's waveform: one entry per channel (one for mono, two otherwise; files with more
/// than two channels are mixed down to two), each with [`WAVEFORM_BUCKETS`] buckets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waveform {
    /// Sample rate of the track (frames per second).
    pub sample_rate: u32,
    /// Frames on the playback timeline: what was decoded, after trimming.
    pub frames: u64,
    pub channels: Vec<WaveformChannel>,
}

impl Waveform {
    /// Buckets of each channel ([`WAVEFORM_BUCKETS`]).
    pub fn buckets(&self) -> usize {
        self.channels.first().map_or(0, |c| c.min.len())
    }
}

/// One channel of a [`Waveform`]: per bucket, the smallest and largest sample
/// (−128…127 for −1.0…1.0: `round(x * 128)`, clamped) and the RMS (0…255 for 0…1.0:
/// `round(rms * 255)`). A bucket with no frames (a track shorter than the bucket count) is
/// 0, 0, 0.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaveformChannel {
    pub min: Vec<i8>,
    pub max: Vec<i8>,
    pub rms: Vec<u8>,
}

/// Decodes `source` (opened as playback opens it; `extension` is the format hint) and
/// computes its waveform: of the whole track, or with `range` of that part of the file (a
/// track of a CUE sheet; source frames, as [`TrackRef::range`](crate::TrackRef::range)
/// plays it). Checks `cancel` before every packet: `Ok(None)` once it is set. Fails when the
/// source cannot be opened or decoded, also part-way through (corrupt packets are skipped,
/// as in playback).
pub fn compute_waveform(
    source: Arc<dyn ByteSource>,
    extension: Option<&str>,
    range: Option<FrameRange>,
    cancel: &AtomicBool,
) -> Result<Option<Waveform>, EngineError> {
    Ok(analyze_audio(source, extension, range, cancel, true, false)?.map(|a| a.waveform.unwrap()))
}

/// Results of one decode pass, with either or both analyses requested.
#[derive(Debug)]
pub struct AudioAnalysis {
    pub waveform: Option<Waveform>,
    pub loudness: Option<crate::Loudness>,
}

/// Native PCM analysis. Loudness uses all positioned source channels before downmix;
/// waveform uses its existing one/two-channel representation. Cancellation saves neither.
pub fn analyze_audio(
    source: Arc<dyn ByteSource>,
    extension: Option<&str>,
    range: Option<FrameRange>,
    cancel: &AtomicBool,
    waveform: bool,
    loudness: bool,
) -> Result<Option<AudioAnalysis>, EngineError> {
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let mut decoder = Decoder::open(source, extension)?;
    let format = decoder.format().clone();
    let channels = usize::from(format.channels);
    if channels == 0 {
        return Err(EngineError::Unsupported("no channels".into()));
    }
    let (start, end) = range.map_or((0, None), |r| (r.start, r.end));
    let end = match (end, format.frames) {
        (Some(end), Some(total)) => Some(end.min(total)),
        (end, _) => end,
    };
    // Track frame after the last chunk, for chunks without a timestamp (as in playback).
    let mut next_frame = 0i64;
    let mut past_end = end.is_some_and(|end| start >= end);
    if start > 0 && !past_end {
        match decoder.seek(start)? {
            Seek::Landed(landed) => next_frame = landed as i64,
            Seek::PastEnd => past_end = true,
        }
    }
    let length = end
        .or(format.frames)
        .map(|end| end.saturating_sub(start))
        .filter(|&n| n > 0);
    let mix = Mix::new(decoder.sides());
    let mut acc = waveform.then(|| Accumulator::new(mix.outputs(), length));
    let mut meter = if loudness {
        let map = decoder
            .loudness_channels()
            .ok_or_else(|| EngineError::Unsupported("unknown loudness channel layout".into()))?;
        Some(crate::loudness::Meter::new(format.sample_rate, map)?)
    } else {
        None
    };
    let mut measured_frames = 0u64;
    let mut decoded = Vec::new();
    while !past_end {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let chunk = decoder.next_chunk(&mut decoded)?;
        if loudness && decoder.analysis_incomplete {
            return Err(EngineError::Decode(
                "incomplete audio cannot be measured for loudness".into(),
            ));
        }
        let Some(chunk) = chunk else {
            break;
        };
        // The frame trimmer of playback (player.rs `produce`): frames before the start
        // (for a whole track, before track frame 0: codec delay that was not trimmed
        // already) and from the end on are left out.
        let frames = (decoded.len() / channels) as i64;
        let first = chunk.start_frame.unwrap_or(next_frame);
        next_frame = first + frames;
        let from = (start as i64 - first).clamp(0, frames) as usize;
        let to = end.map_or(frames, |end| (end as i64 - first).clamp(0, frames)) as usize;
        let samples = &decoded[from * channels..to.max(from) * channels];
        measured_frames += (samples.len() / channels) as u64;
        if let Some(meter) = &mut meter {
            meter.push(samples)?;
        }
        if let Some(acc) = &mut acc {
            for frame in samples.chunks_exact(channels) {
                acc.push(mix.apply(frame));
            }
        }
        past_end = end.is_some_and(|end| next_frame >= end as i64);
    }
    if let Some(expected) = length
        && expected != measured_frames
    {
        tracing::debug!(
            expected,
            decoded = measured_frames,
            "track length differs from what its container says"
        );
    }
    Ok(Some(AudioAnalysis {
        waveform: acc.map(|acc| Waveform {
            sample_rate: format.sample_rate,
            frames: acc.frames,
            channels: acc.finish(),
        }),
        loudness: meter.map(crate::loudness::Meter::finish).transpose()?,
    }))
}

/// How the stream's channels become the waveform's one or two.
enum Mix {
    Mono,
    Stereo,
    /// More than two channels: weights of each into left and right (BS.775: the front
    /// channel of a side at 1, the side's other channels and the centres at −3 dB, LFE left
    /// out). The sum is clamped to ±1.0.
    Down(Vec<(f32, f32)>),
}

impl Mix {
    fn new(sides: &[Side]) -> Self {
        match sides.len() {
            1 => Mix::Mono,
            2 => Mix::Stereo,
            _ => Mix::Down(
                sides
                    .iter()
                    .map(|side| match side {
                        Side::FrontLeft => (1.0, 0.0),
                        Side::FrontRight => (0.0, 1.0),
                        Side::Left => (MIX_SIDE, 0.0),
                        Side::Right => (0.0, MIX_SIDE),
                        Side::Center => (MIX_SIDE, MIX_SIDE),
                        Side::Lfe => (0.0, 0.0),
                    })
                    .collect(),
            ),
        }
    }

    fn outputs(&self) -> usize {
        match self {
            Mix::Mono => 1,
            _ => 2,
        }
    }

    /// One frame of the stream as the waveform's channels (the second is unused for mono).
    fn apply(&self, frame: &[f32]) -> [f32; 2] {
        match self {
            Mix::Mono => [frame[0], 0.0],
            Mix::Stereo => [frame[0], frame[1]],
            Mix::Down(weights) => {
                let (l, r) = frame
                    .iter()
                    .zip(weights)
                    .fold((0.0, 0.0), |(l, r), (x, (wl, wr))| (l + x * wl, r + x * wr));
                [l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0)]
            }
        }
    }
}

/// Statistics of a stretch of one channel.
#[derive(Debug, Clone, Copy)]
struct Cell {
    min: f32,
    max: f32,
    sum_sq: f64,
    count: u64,
}

impl Cell {
    const EMPTY: Cell = Cell {
        min: f32::INFINITY,
        max: f32::NEG_INFINITY,
        sum_sq: 0.0,
        count: 0,
    };

    fn add(&mut self, x: f32) {
        // NaN never wins a comparison, so it is left out of min and max.
        if x < self.min {
            self.min = x;
        }
        if x > self.max {
            self.max = x;
        }
        if x.is_finite() {
            self.sum_sq += f64::from(x) * f64::from(x);
        }
        self.count += 1;
    }

    fn merge(&mut self, other: &Cell) {
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.sum_sq += other.sum_sq;
        self.count += other.count;
    }
}

/// Collects frames into slots. With the track's length known, the slots are the final
/// buckets: frame `i` goes to bucket `i * BUCKETS / total` (frames past `total`, if the
/// container was wrong, into the last). Without it, slots of `block` frames that double in
/// size as needed, merged into buckets by `finish` once the length is known.
struct Accumulator {
    channels: usize,
    total: Option<u64>,
    /// Frames pushed so far.
    frames: u64,
    /// `channels` cells per slot.
    cells: Vec<Cell>,
    slot: usize,
    /// The frame at which the current slot ends.
    slot_end: u64,
    /// Frames per slot, without a known total.
    block: u64,
}

impl Accumulator {
    fn new(channels: usize, total: Option<u64>) -> Self {
        let slots = if total.is_some() { WAVEFORM_BUCKETS } else { 1 };
        let mut acc = Self {
            channels,
            total,
            frames: 0,
            cells: vec![Cell::EMPTY; slots * channels],
            slot: 0,
            slot_end: 0,
            block: 1,
        };
        acc.slot_end = acc.end_of(0);
        acc
    }

    /// The first frame after `slot`.
    fn end_of(&self, slot: usize) -> u64 {
        match self.total {
            Some(_) if slot + 1 >= WAVEFORM_BUCKETS => u64::MAX,
            Some(total) => {
                ((slot as u128 + 1) * u128::from(total)).div_ceil(WAVEFORM_BUCKETS as u128) as u64
            }
            None => (slot as u64 + 1) * self.block,
        }
    }

    fn push(&mut self, frame: [f32; 2]) {
        while self.frames >= self.slot_end {
            self.next_slot();
        }
        let at = self.slot * self.channels;
        for (cell, &x) in self.cells[at..at + self.channels].iter_mut().zip(&frame) {
            cell.add(x);
        }
        self.frames += 1;
    }

    fn next_slot(&mut self) {
        self.slot += 1;
        if self.total.is_none() {
            if self.slot == MAX_SLOTS {
                self.halve();
            }
            if self.slot * self.channels == self.cells.len() {
                self.cells
                    .extend(std::iter::repeat_n(Cell::EMPTY, self.channels));
            }
        }
        self.slot_end = self.end_of(self.slot);
    }

    /// Merges neighbouring slots in pairs: each slot now covers twice the frames.
    fn halve(&mut self) {
        let channels = self.channels;
        let slots = self.cells.len() / channels;
        for slot in 0..slots.div_ceil(2) {
            for ch in 0..channels {
                let mut cell = self.cells[2 * slot * channels + ch];
                if let Some(next) = self.cells.get((2 * slot + 1) * channels + ch) {
                    cell.merge(next);
                }
                self.cells[slot * channels + ch] = cell;
            }
        }
        self.cells.truncate(slots.div_ceil(2) * channels);
        self.slot /= 2;
        self.block *= 2;
    }

    /// The buckets, quantized, per channel.
    fn finish(self) -> Vec<WaveformChannel> {
        let channels = self.channels;
        let buckets = if self.total.is_some() {
            self.cells
        } else {
            let mut buckets = vec![Cell::EMPTY; WAVEFORM_BUCKETS * channels];
            if self.frames > 0 {
                for (slot, cells) in self.cells.chunks_exact(channels).enumerate() {
                    // A slot goes to the bucket its first frame is in.
                    let start = slot as u128 * u128::from(self.block);
                    let bucket = (start * WAVEFORM_BUCKETS as u128 / u128::from(self.frames))
                        .min(WAVEFORM_BUCKETS as u128 - 1)
                        as usize;
                    for (ch, cell) in cells.iter().enumerate() {
                        buckets[bucket * channels + ch].merge(cell);
                    }
                }
            }
            buckets
        };
        (0..channels)
            .map(|ch| {
                let mut out = WaveformChannel {
                    min: Vec::with_capacity(WAVEFORM_BUCKETS),
                    max: Vec::with_capacity(WAVEFORM_BUCKETS),
                    rms: Vec::with_capacity(WAVEFORM_BUCKETS),
                };
                for cell in buckets.chunks_exact(channels).map(|cells| cells[ch]) {
                    let (min, max, rms) = quantize(&cell);
                    out.min.push(min);
                    out.max.push(max);
                    out.rms.push(rms);
                }
                out
            })
            .collect()
    }
}

fn quantize(cell: &Cell) -> (i8, i8, u8) {
    if cell.count == 0 {
        return (0, 0, 0);
    }
    let level = |x: f32| {
        if x.is_finite() {
            (x * 128.0).round().clamp(-128.0, 127.0) as i8
        } else if x > 0.0 {
            127
        } else {
            -128
        }
    };
    // Only NaN samples: no extremes.
    let (min, max) = if cell.min <= cell.max {
        (level(cell.min), level(cell.max))
    } else {
        (0, 0)
    };
    let rms = (cell.sum_sq / cell.count as f64).sqrt();
    let rms = (rms * 255.0).round().clamp(0.0, 255.0) as u8;
    (min, max, rms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(signal: &[[f32; 2]], channels: usize, total: Option<u64>) -> Vec<WaveformChannel> {
        let mut acc = Accumulator::new(channels, total);
        for &frame in signal {
            acc.push(frame);
        }
        assert_eq!(acc.frames, signal.len() as u64);
        acc.finish()
    }

    fn ramp(frames: usize) -> Vec<[f32; 2]> {
        (0..frames)
            .map(|i| {
                let x = i as f32 / frames as f32;
                [x, -x]
            })
            .collect()
    }

    #[test]
    fn known_length_puts_each_frame_in_its_bucket() {
        // 4 frames per bucket: bucket b holds frames 4b..4b+4.
        let frames = 4 * WAVEFORM_BUCKETS;
        let signal: Vec<[f32; 2]> = (0..frames)
            .map(|i| if i % 4 == 0 { [1.0, -1.0] } else { [0.0, 0.0] })
            .collect();
        let out = run(&signal, 2, Some(frames as u64));
        assert_eq!(out.len(), 2);
        assert!(out[0].max.iter().all(|&m| m == 127));
        assert!(out[0].min.iter().all(|&m| m == 0));
        assert!(out[1].min.iter().all(|&m| m == -128));
        // RMS of one full-scale sample in four: 0.5.
        assert!(
            out[0].rms.iter().all(|&r| r == 128),
            "{:?}",
            &out[0].rms[..4]
        );
    }

    #[test]
    fn unknown_length_merges_fine_slots_into_the_same_buckets() {
        // Long enough for several rounds of halving, and not a multiple of anything.
        for frames in [1000, 2048 * 3 + 7, MAX_SLOTS * 5 + 123] {
            let signal = ramp(frames);
            let known = run(&signal, 2, Some(frames as u64));
            let unknown = run(&signal, 2, None);
            for ch in 0..2 {
                assert_eq!(unknown[ch].max.len(), WAVEFORM_BUCKETS);
                let worst = known[ch]
                    .max
                    .iter()
                    .zip(&unknown[ch].max)
                    .chain(known[ch].min.iter().zip(&unknown[ch].min))
                    .map(|(a, b)| (i16::from(*a) - i16::from(*b)).abs())
                    .max()
                    .unwrap();
                // Slot edges may straddle a bucket edge by less than a slot.
                assert!(worst <= 1, "{frames} frames, channel {ch}: off by {worst}");
            }
            if frames < MAX_SLOTS {
                assert_eq!(
                    known, unknown,
                    "{frames} frames: one frame per slot is exact"
                );
            }
        }
    }

    #[test]
    fn short_tracks_leave_buckets_empty_and_long_containers_fill_the_last() {
        let out = run(&[[0.5, 0.5]; 10], 1, Some(10));
        let filled = out[0].max.iter().filter(|&&m| m != 0).count();
        assert_eq!(filled, 10);
        assert_eq!(out[0].max[0], 64);
        // The container said 10 frames, but 20 came: the rest is in the last bucket.
        let mut signal = vec![[0.0, 0.0]; 10];
        signal.extend([[-1.0, 0.0]; 10]);
        let out = run(&signal, 1, Some(10));
        assert_eq!(out[0].min[WAVEFORM_BUCKETS - 1], -128);
        assert_eq!(run(&[], 2, None)[1].rms, vec![0; WAVEFORM_BUCKETS]);
    }

    #[test]
    fn down_mix_follows_bs775() {
        use Side::*;
        let mix = Mix::new(&[FrontLeft, FrontRight, Center, Lfe, Left, Right]);
        let [l, r] = mix.apply(&[0.0, 0.0, 0.5, 1.0, 0.0, 0.0]);
        assert!((l - 0.5 * MIX_SIDE).abs() < 1e-6 && (r - l).abs() < 1e-6);
        let [l, r] = mix.apply(&[1.0, 0.0, 1.0, 0.0, 1.0, 0.0]);
        assert_eq!((l, r), (1.0, MIX_SIDE), "clamped");
    }
}
