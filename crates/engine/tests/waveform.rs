//! Whole-track waveforms on generated WAV files and the encoded fixtures
//! (tests/fixtures/README.md): bucket layout, values for known signals, channel handling,
//! the playback timeline (encoder delay and padding trimmed) and cancellation.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use mpmp_engine::{FrameRange, WAVEFORM_BUCKETS, Waveform, compute_waveform};
use mpmp_media_io::{ByteSource, FileSource, MemorySource};
use support::{fixture, write_wav};

fn waveform_of(path: &Path) -> Waveform {
    let source = Arc::new(FileSource::open(path).unwrap());
    let ext = path.extension().and_then(|e| e.to_str());
    compute_waveform(source, ext, None, &AtomicBool::new(false))
        .unwrap()
        .expect("not cancelled")
}

/// A full-scale square wave, `period` frames long, as 16-bit samples.
fn square(frames: usize, period: usize) -> Vec<i32> {
    (0..frames)
        .map(|i| {
            if i % period < period / 2 {
                i32::from(i16::MAX)
            } else {
                i32::from(i16::MIN)
            }
        })
        .collect()
}

#[test]
fn a_full_scale_square_fills_every_bucket() {
    // 16 frames per bucket, 4 periods each: every bucket sees both extremes.
    let frames = 16 * WAVEFORM_BUCKETS;
    let file = write_wav("wf-square.wav", 44_100, 1, 16, &square(frames, 4));
    let w = waveform_of(&file.0);
    assert_eq!((w.sample_rate, w.frames), (44_100, frames as u64));
    assert_eq!(w.buckets(), WAVEFORM_BUCKETS);
    assert_eq!(w.channels.len(), 1, "mono stays one channel");
    let ch = &w.channels[0];
    assert_eq!(ch.min, vec![-128; WAVEFORM_BUCKETS]);
    assert_eq!(ch.max, vec![127; WAVEFORM_BUCKETS]);
    assert_eq!(ch.rms, vec![255; WAVEFORM_BUCKETS]);
}

#[test]
fn stereo_channels_are_kept_apart() {
    // Left: a square at full scale; right: silence for the first half, then a constant
    // -0.25.
    let frames = 8 * WAVEFORM_BUCKETS;
    let left = square(frames, 2);
    let samples: Vec<i32> = (0..frames)
        .flat_map(|i| [left[i], if i < frames / 2 { 0 } else { -8192 }])
        .collect();
    let file = write_wav("wf-stereo.wav", 48_000, 2, 16, &samples);
    let w = waveform_of(&file.0);
    assert_eq!(w.channels.len(), 2);
    assert_eq!(w.channels[0].rms, vec![255; WAVEFORM_BUCKETS]);
    let right = &w.channels[1];
    let half = WAVEFORM_BUCKETS / 2;
    assert!(right.max[..half].iter().all(|&m| m == 0));
    assert!(right.rms[..half].iter().all(|&r| r == 0));
    assert_eq!(&right.min[half..], &vec![-32; half][..]);
    assert_eq!(&right.max[half..], &vec![-32; half][..]);
    // 0.25 * 255 = 63.75.
    assert_eq!(&right.rms[half..], &vec![64; half][..]);
}

#[test]
fn a_sine_has_the_expected_peaks_and_rms() {
    // The fixture signal: 0.5 * sine on both channels, 37123 frames (18 per bucket).
    let w = waveform_of(&fixture("sine44_a.wav"));
    assert_eq!(w.frames, 37_123);
    for ch in &w.channels {
        // A bucket is less than half a period (44 or 29 frames), so many miss the peaks;
        // none goes beyond them.
        assert_eq!(*ch.max.iter().max().unwrap(), 64);
        assert_eq!(*ch.min.iter().min().unwrap(), -64);
        assert!(ch.min.iter().zip(&ch.max).all(|(lo, hi)| lo <= hi));
        // RMS of a sine: 0.5 / sqrt(2) = 0.354, 90 of 255, over whole periods; buckets of
        // equal length average to it in power.
        let power = ch.rms.iter().map(|&r| f64::from(r).powi(2)).sum::<f64>() / ch.rms.len() as f64;
        assert!((power.sqrt() - 90.2).abs() < 1.5, "rms {}", power.sqrt());
    }
}

#[test]
fn more_than_two_channels_are_mixed_down_to_two() {
    // 5.1 in the WAVE order (FL FR FC LFE BL BR): only the centre and the LFE carry sound.
    let frames = 4 * WAVEFORM_BUCKETS;
    let samples: Vec<i32> = (0..frames)
        .flat_map(|i| {
            let c = if i % 2 == 0 { 16_384 } else { -16_384 };
            [0, 0, c, i32::from(i16::MAX), 0, 0]
        })
        .collect();
    let file = write_wav("wf-surround.wav", 48_000, 6, 16, &samples);
    let w = waveform_of(&file.0);
    assert_eq!(w.channels.len(), 2);
    for ch in &w.channels {
        // 0.5 at -3 dB is 0.354: 45 of 128; the LFE is left out.
        assert_eq!(ch.max, vec![45; WAVEFORM_BUCKETS]);
        assert_eq!(ch.min, vec![-45; WAVEFORM_BUCKETS]);
        assert_eq!(ch.rms, vec![90; WAVEFORM_BUCKETS]);
    }
}

#[test]
fn encoder_delay_and_padding_are_trimmed_as_in_playback() {
    // Each file holds frames 0..37123 of the same signal; MP3 adds 576 frames of encoder
    // delay plus decoder delay at the start and padding at the end, Vorbis a pre-roll.
    let reference = waveform_of(&fixture("sine44_a.flac"));
    assert_eq!(reference.frames, 37_123);
    for name in ["sine44_a.mp3", "sine44_a.ogg"] {
        let w = waveform_of(&fixture(name));
        assert_eq!(w.frames, 37_123, "{name}");
        for (ch, expected) in w.channels.iter().zip(&reference.channels) {
            // Untrimmed, the first buckets would be silent and everything shifted.
            assert!(ch.max[0] > 30, "{name}: first bucket {}", ch.max[0]);
            let worst = ch
                .max
                .iter()
                .zip(&expected.max)
                .map(|(a, b)| (i16::from(*a) - i16::from(*b)).abs())
                .max()
                .unwrap();
            assert!(worst <= 6, "{name}: a bucket differs by {worst}");
        }
    }
}

/// A source whose length is not known and that cannot seek, like a stream.
struct Stream(MemorySource);

impl ByteSource for Stream {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read_at(offset, buf)
    }

    fn len(&self) -> Option<u64> {
        None
    }

    fn is_seekable(&self) -> bool {
        false
    }
}

#[test]
fn a_source_of_unknown_length_gives_the_same_waveform() {
    let path = fixture("sine44_a.ogg");
    let known = waveform_of(&path);
    let stream = Arc::new(Stream(MemorySource::new(std::fs::read(&path).unwrap())));
    let w = compute_waveform(stream, Some("ogg"), None, &AtomicBool::new(false))
        .unwrap()
        .unwrap();
    assert_eq!(w.frames, known.frames);
    for (a, b) in w.channels.iter().zip(&known.channels) {
        let worst = a
            .max
            .iter()
            .zip(&b.max)
            .map(|(x, y)| (i16::from(*x) - i16::from(*y)).abs())
            .max()
            .unwrap();
        assert!(worst <= 2, "a bucket differs by {worst}");
    }
}

#[test]
fn a_cancelled_computation_stops() {
    let source = Arc::new(FileSource::open(&fixture("sine44_full.flac")).unwrap());
    let w = compute_waveform(source, Some("flac"), None, &AtomicBool::new(true)).unwrap();
    assert_eq!(w, None);
}

#[test]
fn a_file_that_is_not_audio_fails() {
    let source = Arc::new(MemorySource::new(b"not audio at all".repeat(100)));
    assert!(compute_waveform(source, Some("mp3"), None, &AtomicBool::new(false)).is_err());
}

#[test]
fn a_track_shorter_than_the_buckets_leaves_some_empty() {
    let file = write_wav("wf-short.wav", 44_100, 1, 16, &[i32::from(i16::MIN); 100]);
    let w = waveform_of(&file.0);
    assert_eq!(w.frames, 100);
    assert_eq!(w.buckets(), WAVEFORM_BUCKETS);
    let filled = w.channels[0].min.iter().filter(|&&m| m == -128).count();
    assert_eq!(filled, 100);
    assert_eq!(w.channels[0].min.iter().filter(|&&m| m == 0).count(), 1948);
}

fn waveform_of_range(path: &Path, range: FrameRange) -> Waveform {
    let source = Arc::new(FileSource::open(path).unwrap());
    let ext = path.extension().and_then(|e| e.to_str());
    compute_waveform(source, ext, Some(range), &AtomicBool::new(false))
        .unwrap()
        .expect("not cancelled")
}

#[test]
fn a_range_covers_only_its_part_of_the_file() {
    // A CUE track: the waveform of a range is that of a file holding just those frames.
    let frames = 40_000;
    let samples: Vec<i32> = (0..frames)
        .flat_map(|i| {
            let s = ((i * 37) % 2000) as i32 * 16 - 16_000;
            [s, -s / 2]
        })
        .collect();
    let (start, end) = (12_345usize, 31_000usize);
    let whole = write_wav("wf-whole.wav", 44_100, 2, 16, &samples);
    let part = write_wav("wf-part.wav", 44_100, 2, 16, &samples[start * 2..end * 2]);
    let range = |start: usize, end: Option<usize>| FrameRange {
        start: start as u64,
        end: end.map(|e| e as u64),
    };
    let ranged = waveform_of_range(&whole.0, range(start, Some(end)));
    assert_eq!(ranged.frames, (end - start) as u64);
    assert_eq!(ranged, waveform_of(&part.0));
    // To the end of the file.
    let tail = write_wav("wf-tail.wav", 44_100, 2, 16, &samples[start * 2..]);
    let ranged = waveform_of_range(&whole.0, range(start, None));
    assert_eq!(ranged, waveform_of(&tail.0));
    // FLAC, seeking into the file: the middle part of the fixture is sine44_b.flac.
    let ranged = waveform_of_range(&fixture("sine44_full.flac"), range(37_123, Some(81_457)));
    assert_eq!(ranged, waveform_of(&fixture("sine44_b.flac")));
    // Nothing of a range past the end.
    let empty = waveform_of_range(&whole.0, range(frames + 10, None));
    assert_eq!(empty.frames, 0);
}
