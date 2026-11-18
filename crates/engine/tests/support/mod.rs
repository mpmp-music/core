//! Test harness: drives the engine on the null output. Time only advances when a test
//! renders, so results are deterministic.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use mpmp_engine::output::{SampleType, StreamFormat};
use mpmp_engine::{
    Engine, EngineConfig, EngineEvent, EntryId, NullOutput, NullOutputHandle, TrackRef, wait_until,
};
use mpmp_media_io::{FileResolver, MediaUri};

pub const TIMEOUT: Duration = Duration::from_secs(20);

pub const CHUNK: usize = 512;

pub fn format(rate: u32, channels: u16, sample_type: SampleType) -> StreamFormat {
    StreamFormat {
        sample_rate: rate,
        channels,
        sample_type,
    }
}

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

pub fn track(entry: u64, path: &Path) -> TrackRef {
    TrackRef::new(EntryId(entry), MediaUri::from_path(path).unwrap())
}

pub struct Harness {
    pub engine: Engine,
    pub output: NullOutputHandle,
    pub events: Receiver<EngineEvent>,
    pub format: StreamFormat,
    /// Every event seen so far, in order.
    pub seen: Vec<EngineEvent>,
    /// Frames `render` waits to have buffered before each chunk (or the end of the
    /// stream). A real device plays from a ring that is well ahead of it; tests that
    /// depend on the renderer seeing what comes next raise this.
    pub min_buffered: u64,
}

impl Harness {
    pub fn new(format: StreamFormat) -> Self {
        let output = NullOutput::new(format);
        let handle = output.handle();
        let engine = Engine::new(
            Box::new(output),
            Arc::new(FileResolver),
            EngineConfig::default(),
        );
        let events = engine.events();
        Self {
            engine,
            output: handle,
            events,
            format,
            seen: Vec::new(),
            min_buffered: CHUNK as u64,
        }
    }

    pub fn channels(&self) -> usize {
        usize::from(self.format.channels)
    }

    /// Waits until a full chunk is buffered (or the stream has ended), so that rendering
    /// never underruns.
    fn wait_for_data(&self) {
        assert!(
            wait_until(TIMEOUT, || {
                let status = self.engine.buffer_status().unwrap();
                status.buffered_frames >= self.min_buffered || status.end_of_stream
            }),
            "decode thread stalled: {:?}",
            self.engine.buffer_status()
        );
    }

    /// Renders one chunk and returns only its content frames (as f32), in order.
    pub fn render(&mut self) -> Vec<f32> {
        self.wait_for_data();
        let before = self.engine.buffer_status().unwrap().played_frames;
        let samples: Vec<f32> = match self.format.sample_type {
            SampleType::F32 => self.output.render_f32(CHUNK),
            SampleType::I16 => self
                .output
                .render_i16(CHUNK)
                .into_iter()
                .map(|s| f32::from(s) / 32_768.0)
                .collect(),
            SampleType::I32 => self
                .output
                .render_i32(CHUNK)
                .into_iter()
                .map(|s| (f64::from(s) / 2_147_483_648.0) as f32)
                .collect(),
        };
        let played = (self.engine.buffer_status().unwrap().played_frames - before) as usize;
        samples[..played * self.channels()].to_vec()
    }

    pub fn drain_events(&mut self) -> Vec<EngineEvent> {
        let new: Vec<_> = self.events.try_iter().collect();
        self.seen.extend(new.iter().cloned());
        new
    }

    /// Plays `tracks` back to back the way core would: the next track is set right
    /// away, then again after each `TrackStarted`. Returns everything rendered.
    pub fn play_queue(&mut self, tracks: &[TrackRef]) -> Vec<f32> {
        self.engine.play(tracks[0].clone()).unwrap();
        self.engine.set_next(tracks.get(1).cloned());
        self.run_queue(tracks)
    }

    /// The rendering part of [`play_queue`](Self::play_queue), for a queue the test has
    /// already started with `play` and `set_next`.
    pub fn run_queue(&mut self, tracks: &[TrackRef]) -> Vec<f32> {
        self.drive_queue(tracks, false)
    }

    /// Like [`run_queue`](Self::run_queue), for queues of tracks so short that the test,
    /// rendering much faster than real time, can outrun the decode thread at a boundary.
    pub fn run_queue_allowing_underruns(&mut self, tracks: &[TrackRef]) -> Vec<f32> {
        self.drive_queue(tracks, true)
    }

    fn drive_queue(&mut self, tracks: &[TrackRef], allow_underruns: bool) -> Vec<f32> {
        let mut out = Vec::new();
        loop {
            out.extend(self.render());
            for event in self.drain_events() {
                match event {
                    EngineEvent::TrackStarted { entry, .. } => {
                        let at = tracks.iter().position(|t| t.entry == entry).unwrap();
                        if at > 0 {
                            self.engine.set_next(tracks.get(at + 1).cloned());
                        }
                    }
                    EngineEvent::EndOfQueue { .. } => {
                        if !allow_underruns {
                            assert_eq!(self.engine.buffer_status().unwrap().underruns, 0);
                        }
                        return out;
                    }
                    EngineEvent::TrackFailed { entry, reason } => {
                        panic!("{entry:?} failed: {reason}")
                    }
                    EngineEvent::NextAlreadyStarted { .. } => {}
                }
            }
        }
    }

    /// Renders until the end of the queue, returning what was rendered.
    pub fn play_to_end(&mut self) -> Vec<f32> {
        let mut out = Vec::new();
        loop {
            out.extend(self.render());
            if self
                .drain_events()
                .iter()
                .any(|e| matches!(e, EngineEvent::EndOfQueue { .. }))
            {
                return out;
            }
            let status = self.engine.buffer_status().unwrap();
            if status.end_of_stream && status.buffered_frames == 0 {
                // Keep pulling, as a device does, until the renderer's "drained" is seen
                // for the stream's latest epoch (a failed next track can end the stream
                // again after the first drain was noticed).
                let deadline = std::time::Instant::now() + TIMEOUT;
                loop {
                    out.extend(self.render());
                    if self
                        .drain_events()
                        .iter()
                        .any(|e| matches!(e, EngineEvent::EndOfQueue { .. }))
                    {
                        return out;
                    }
                    assert!(std::time::Instant::now() < deadline, "no EndOfQueue");
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
    }

    /// Waits for an event matching `pred`, rendering meanwhile.
    pub fn render_until(&mut self, mut pred: impl FnMut(&EngineEvent) -> bool) -> Vec<f32> {
        let mut out = Vec::new();
        for _ in 0..100_000 {
            out.extend(self.render());
            // Events are emitted once the frames are "heard"; give the clock a moment.
            std::thread::sleep(Duration::from_millis(1));
            if self.drain_events().iter().any(&mut pred) {
                return out;
            }
        }
        panic!("event never arrived; seen: {:?}", self.seen);
    }
}

/// The fixture signal (scripts/gen_signal.py), as decoded f32: frames `[start, end)`.
pub fn sine(rate: u32, start: usize, end: usize) -> Vec<f32> {
    (start..end)
        .flat_map(|n| {
            let t = n as f64 / f64::from(rate);
            let l = (0.5 * (2.0 * std::f64::consts::PI * 997.0 * t).sin() * 32_767.0).round();
            let r =
                (0.5 * (2.0 * std::f64::consts::PI * 1499.0 * t + 0.3).sin() * 32_767.0).round();
            [(l / 32_768.0) as f32, (r / 32_768.0) as f32]
        })
        .collect()
}

/// Largest absolute sample difference.
pub fn max_error(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "lengths differ");
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f32::max)
}

/// Waits until at least `frames` frames are buffered (the decode thread fills the ring).
pub fn wait_until_buffered(h: &Harness, frames: u64) -> bool {
    wait_until(TIMEOUT, || {
        h.engine.buffer_status().unwrap().buffered_frames >= frames
    })
}

/// Like `assert_eq!` on sample vectors, but reports what differs instead of dumping
/// both vectors: lengths, the first differing frame and a few frames around it.
#[track_caller]
pub fn assert_samples_eq(actual: &[f32], expected: &[f32], context: &str) {
    let first = actual
        .iter()
        .zip(expected)
        .position(|(a, b)| a != b)
        .map(|i| i / 2);
    if actual.len() == expected.len() && first.is_none() {
        return;
    }
    let around = |v: &[f32], f: usize| -> Vec<f32> {
        v.get(2 * f.saturating_sub(2)..(2 * (f + 3)).min(v.len()))
            .unwrap_or_default()
            .to_vec()
    };
    let detail = first.map_or(String::new(), |f| {
        format!(
            "; first difference at frame {f}: actual {:?} vs expected {:?}",
            around(actual, f),
            around(expected, f)
        )
    });
    panic!(
        "{context}: {} frames vs {} expected{detail}",
        actual.len() / 2,
        expected.len() / 2
    );
}

/// Lets the decode thread handle a command just sent. In real time it has a whole
/// buffer's duration before the next audio is due; tests render much faster, and on a
/// loaded machine (or Windows, with its coarse wake-ups) it may not have run at all yet.
pub fn settle() {
    std::thread::sleep(std::time::Duration::from_millis(30));
}

pub struct TempFile(pub PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("engine-test-{}-{name}", std::process::id()))
}

/// Deterministic noise, so that any reordering or rounding shows up.
pub fn noise(n: usize, bits: u32) -> Vec<i32> {
    let mut state = 0x1234_5678u32;
    let max = (1i64 << (bits - 1)) - 1;
    (0..n)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (i64::from(state >> 8) % (2 * max + 1) - max) as i32
        })
        .collect()
}

pub fn write_wav(name: &str, rate: u32, channels: u16, bits: u16, samples: &[i32]) -> TempFile {
    let path = temp_path(name);
    let spec = hound::WavSpec {
        channels,
        sample_rate: rate,
        bits_per_sample: bits,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).unwrap();
    for &s in samples {
        writer.write_sample(s).unwrap();
    }
    writer.finalize().unwrap();
    TempFile(path)
}
