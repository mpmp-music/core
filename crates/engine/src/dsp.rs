//! Decode-thread signal processing between the decoder and the ring buffer.

use std::collections::VecDeque;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler as _};

use crate::EngineError;

const CHUNK_FRAMES: usize = 1024;

/// Call boundaries retained for deterministic resampler rebuilds.
const CALL_LOG: usize = 64;

/// A streaming resampler that is transparent in frame count: the warm-up (`output_delay()` frames) is dropped at the start, and
/// [`flush_to`](Self::flush_to) drains the tail to an exact frame count.
///
/// It remembers where each processing call began, so that a fresh copy started at one of
/// those boundaries and fed the same input reproduces the original output exactly once
/// its warm-up has passed.
pub(crate) struct StreamResampler {
    inner: Fft<f32>,
    channels: usize,
    rate_in: u32,
    rate_out: u32,
    /// Input not yet consumed by a processing call.
    pending: Vec<f32>,
    /// Input frames consumed by processing calls so far.
    consumed: u64,
    scratch: Vec<f32>,
    /// Raw output frames produced so far, warm-up included.
    raw_out: u64,
    delay: u64,
    /// `(input frames consumed, raw output frames)` at the start of recent calls.
    calls: VecDeque<(u64, u64)>,
}

impl StreamResampler {
    pub fn new(rate_in: u32, rate_out: u32, channels: usize) -> Result<Self, EngineError> {
        let inner = Fft::<f32>::new(
            rate_in as usize,
            rate_out as usize,
            CHUNK_FRAMES,
            channels,
            // One FFT block per call and no frames carried between calls, so every call
            // boundary is a block boundary: a restart there replays the same blocks.
            FixedSync::Both,
        )
        .map_err(|e| EngineError::Unsupported(format!("resampler: {e}")))?;
        let delay = inner.output_delay() as u64;
        let scratch = vec![0.0; inner.output_frames_max() * channels];
        Ok(Self {
            pending: Vec::with_capacity(inner.input_frames_max() * channels * 2),
            inner,
            channels,
            rate_in,
            rate_out,
            consumed: 0,
            scratch,
            raw_out: 0,
            delay,
            calls: VecDeque::with_capacity(CALL_LOG),
        })
    }

    /// Input frames per processing call.
    pub fn chunk_frames(&self) -> usize {
        self.inner.input_frames_next()
    }

    /// Output frames (after warm-up trimming) delivered so far.
    pub fn produced(&self) -> u64 {
        self.raw_out.saturating_sub(self.delay)
    }

    /// Output frames that `input` input frames correspond to, rounded up.
    pub fn out_frames_for(&self, input: u64) -> u64 {
        (input * u64::from(self.rate_out)).div_ceil(u64::from(self.rate_in))
    }

    /// Feeds interleaved input; appends whatever output is ready to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<(), EngineError> {
        self.pending.extend_from_slice(input);
        while self.pending.len() / self.channels >= self.inner.input_frames_next() {
            self.process(None, out)?;
        }
        Ok(())
    }

    /// Consumes the remaining input and pumps silence until exactly `total` output frames
    /// (counted from this resampler's start, after warm-up) have been delivered.
    pub fn flush_to(&mut self, total: u64, out: &mut Vec<f32>) -> Result<(), EngineError> {
        let before = out.len();
        let pending = self.pending.len() / self.channels;
        if pending > 0 {
            self.process(Some(pending), out)?;
        }
        while self.produced() < total {
            self.process(Some(0), out)?;
        }
        let excess = (self.produced() - total) as usize * self.channels;
        let keep = (out.len() - before).saturating_sub(excess);
        out.truncate(before + keep);
        self.raw_out = total + self.delay;
        Ok(())
    }

    /// The most recent call boundary at or before input frame `at`, as
    /// `(input frames consumed, raw output frames)` at that point.
    pub fn call_boundary_before(&self, at: u64) -> Option<(u64, u64)> {
        self.calls
            .iter()
            .rev()
            .find(|(input, _)| *input <= at)
            .copied()
    }

    /// Output frames between a fresh resampler's first raw output and its first valid one.
    #[cfg(test)]
    pub fn delay(&self) -> u64 {
        self.delay
    }

    fn process(&mut self, partial: Option<usize>, out: &mut Vec<f32>) -> Result<(), EngineError> {
        if self.calls.len() == CALL_LOG {
            self.calls.pop_front();
        }
        self.calls.push_back((self.consumed, self.raw_out));

        let needed = self.inner.input_frames_next();
        // A partial chunk still needs a full-size input buffer; the rest reads as silence.
        if self.pending.len() < needed * self.channels {
            self.pending.resize(needed * self.channels, 0.0);
        }
        let input = InterleavedSlice::new(&self.pending[..], self.channels, needed)
            .map_err(|e| EngineError::Decode(format!("resampler input: {e}")))?;
        let out_frames = self.scratch.len() / self.channels;
        let mut output =
            InterleavedSlice::new_mut(&mut self.scratch[..], self.channels, out_frames)
                .map_err(|e| EngineError::Decode(format!("resampler output: {e}")))?;
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: partial,
            active_channels_mask: None,
        };
        let (consumed, produced) = self
            .inner
            .process_into_buffer(&input, &mut output, Some(&indexing))
            .map_err(|e| EngineError::Decode(format!("resampler: {e}")))?;

        let real = partial.unwrap_or(consumed);
        self.pending.drain(..real * self.channels);
        if partial.is_some() {
            self.pending.clear();
        }
        self.consumed += real as u64;

        let skip = self.delay.saturating_sub(self.raw_out).min(produced as u64) as usize;
        out.extend_from_slice(&self.scratch[skip * self.channels..produced * self.channels]);
        self.raw_out += produced as u64;
        Ok(())
    }
}

/// Maps interleaved `input` with `from` channels onto `to` channels, appending to `out`.
///
/// Mono is duplicated to the first two outputs. Otherwise channels are copied in order and
/// missing outputs are silent; a proper down-mix matrix comes later.
pub(crate) fn map_channels(input: &[f32], from: usize, to: usize, out: &mut Vec<f32>) {
    if from == to {
        out.extend_from_slice(input);
        return;
    }
    for frame in input.chunks_exact(from) {
        for ch in 0..to {
            let sample = match (from, ch) {
                (1, 0 | 1) => frame[0],
                _ if ch < from => frame[ch],
                _ => 0.0,
            };
            out.push(sample);
        }
    }
}

/// Whether [`map_channels`] changes sample values rather than only copying them.
pub(crate) fn channel_map_alters(from: usize, to: usize) -> bool {
    !(from == to || (from == 1 && to == 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resample_all(
        rate_in: u32,
        rate_out: u32,
        input: &[f32],
        channels: usize,
        push: usize,
    ) -> Vec<f32> {
        let mut resampler = StreamResampler::new(rate_in, rate_out, channels).unwrap();
        let mut out = Vec::new();
        for chunk in input.chunks(push * channels) {
            resampler.push(chunk, &mut out).unwrap();
        }
        let total = resampler.out_frames_for((input.len() / channels) as u64);
        resampler.flush_to(total, &mut out).unwrap();
        out
    }

    #[test]
    fn output_length_is_exact() {
        for &(rate_in, rate_out, frames, push) in &[
            (44_100, 48_000, 44_100, 1152),
            (48_000, 44_100, 10_000, 4096),
            (44_100, 96_000, 300, 100),
            (96_000, 44_100, 1, 1),
        ] {
            let input = vec![0.25; frames * 2];
            let out = resample_all(rate_in, rate_out, &input, 2, push);
            let expected =
                (frames as u64 * u64::from(rate_out)).div_ceil(u64::from(rate_in)) as usize;
            assert_eq!(
                out.len(),
                expected * 2,
                "{rate_in}->{rate_out}, {frames} frames"
            );
        }
    }

    #[test]
    fn warm_up_is_trimmed() {
        // A step at input frame 5000 must cross half level at 5000 * ratio in the output;
        // an untrimmed warm-up would shift it late by `output_delay()` frames.
        for &(rate_in, rate_out) in &[(48_000, 44_100), (44_100, 48_000)] {
            let step_at = 5000;
            let input: Vec<f32> = (0..20_000)
                .map(|i| if i < step_at { 0.0 } else { 1.0 })
                .collect();
            let out = resample_all(rate_in, rate_out, &input, 1, 4096);
            let crossing = out.iter().position(|&s| s >= 0.5).unwrap() as f64;
            let expected = step_at as f64 * f64::from(rate_out) / f64::from(rate_in);
            assert!(
                (crossing - expected).abs() <= 1.0,
                "{rate_in}->{rate_out}: crossing {crossing}, expected {expected}"
            );
        }
    }

    #[test]
    fn restart_at_call_boundary_reproduces_output() {
        // The basis of the boundary checkpoint: a fresh resampler started at a recorded
        // call boundary and fed the same input matches the original after its warm-up.
        let input: Vec<f32> = (0..40_000).map(|i| ((i as f32) * 0.013).sin()).collect();
        let mut original = StreamResampler::new(44_100, 48_000, 1).unwrap();
        let mut reference = Vec::new();
        original.push(&input, &mut reference).unwrap();

        let (start_in, start_raw) = original.call_boundary_before(20_000).unwrap();
        let mut copy = StreamResampler::new(44_100, 48_000, 1).unwrap();
        let mut replayed = Vec::new();
        copy.push(&input[start_in as usize..], &mut replayed)
            .unwrap();

        // The copy's trimmed output k is the original's trimmed output start_raw + k.
        let settle = 2 * copy.chunk_frames() + copy.delay() as usize;
        let base = start_raw as usize;
        let mut compared = 0;
        for k in settle..replayed.len().min(reference.len() - base) {
            assert_eq!(replayed[k], reference[base + k], "frame {k}");
            compared += 1;
        }
        assert!(compared > 10_000, "only {compared} frames compared");
    }

    #[test]
    fn channel_mapping() {
        let mut out = Vec::new();
        map_channels(&[0.1, 0.2], 1, 2, &mut out);
        assert_eq!(out, [0.1, 0.1, 0.2, 0.2]);

        out.clear();
        map_channels(&[0.1, 0.2], 2, 4, &mut out);
        assert_eq!(out, [0.1, 0.2, 0.0, 0.0]);

        out.clear();
        map_channels(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6], 6, 2, &mut out);
        assert_eq!(out, [0.1, 0.2]);

        assert!(!channel_map_alters(1, 2));
        assert!(!channel_map_alters(2, 2));
        assert!(channel_map_alters(6, 2));
    }
}
