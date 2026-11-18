//! Known PCM references measured independently with ffmpeg ebur128=peak=sample.
//! Tests themselves need no ffmpeg and never open an audio device.
mod support;
use mpmp_engine::{AudioAnalysis, FrameRange, album_loudness, analyze_audio, compute_waveform};
use mpmp_media_io::FileSource;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use support::{fixture, write_wav};

fn tone(rate: u32, seconds: f64, amplitude: f64, channels: u16) -> Vec<i32> {
    (0..(f64::from(rate) * seconds) as usize)
        .flat_map(|i| {
            let v = (amplitude
                * 32768.0
                * (std::f64::consts::TAU * 1000.0 * i as f64 / f64::from(rate)).sin())
            .round() as i32;
            std::iter::repeat_n(v, channels as usize)
        })
        .collect()
}

fn analyze(path: &std::path::Path, range: Option<FrameRange>) -> AudioAnalysis {
    analyze_audio(
        Arc::new(FileSource::open(path).unwrap()),
        path.extension().and_then(|e| e.to_str()),
        range,
        &AtomicBool::new(false),
        true,
        true,
    )
    .unwrap()
    .unwrap()
}

#[test]
fn reference_levels_rates_and_single_decode_waveform() {
    for rate in [44_100, 48_000, 96_000] {
        for (amp, expected) in [(0.5, -6.0), (0.25, -12.0)] {
            let file = write_wav(
                "loudness-reference.wav",
                rate,
                2,
                16,
                &tone(rate, 3.0, amp, 2),
            );
            let a = analyze(&file.0, None);
            let l = a.loudness.unwrap();
            assert!(
                (l.integrated_lufs.unwrap() - expected).abs() < 0.15,
                "{}",
                l.integrated_lufs.unwrap()
            );
            assert!((l.sample_peak - amp).abs() < 0.0001);
            assert!((l.gain_db().unwrap() - (-18.0 - expected)).abs() < 0.15);
            let w = compute_waveform(
                Arc::new(FileSource::open(&file.0).unwrap()),
                Some("wav"),
                None,
                &AtomicBool::new(false),
            )
            .unwrap()
            .unwrap();
            assert_eq!(a.waveform.unwrap(), w);
            // Finished meters compact their PCM buffers; album statistics still match.
            assert!(
                (album_loudness(&[&l]).unwrap().unwrap().0 - l.integrated_lufs.unwrap()).abs()
                    < 1e-10
            );
        }
    }
}

#[test]
fn album_merges_energy_and_applies_relative_gate() {
    let make = |amplitude| {
        let file = write_wav(
            "loudness-album.wav",
            48_000,
            2,
            16,
            &tone(48_000, 3.0, amplitude, 2),
        );
        analyze(&file.0, None).loudness.unwrap()
    };
    let loud = make(0.5);
    let quiet = make(0.25);
    let gated = make(0.005);
    let album = album_loudness(&[&loud, &quiet]).unwrap().unwrap();
    assert!((album.0 - (-8.04)).abs() < 0.15, "{}", album.0);
    assert!(
        (album.0 - (loud.integrated_lufs.unwrap() + quiet.integrated_lufs.unwrap()) / 2.0).abs()
            > 0.7
    );
    let album = album_loudness(&[&loud, &gated]).unwrap().unwrap();
    assert!((album.0 - loud.integrated_lufs.unwrap()).abs() < 0.1);
    assert_eq!(album.1, loud.sample_peak);
}

#[test]
fn silence_short_ranges_and_cancellation_are_finite() {
    for samples in [
        vec![0; 96_000],
        tone(48_000, 0.2, 0.5, 2),
        tone(48_000, 1.0, 0.00003, 2),
    ] {
        let file = write_wav("loudness-empty.wav", 48_000, 2, 16, &samples);
        let l = analyze(&file.0, None).loudness.unwrap();
        assert_eq!(l.integrated_lufs, None);
        assert_eq!(l.gain_db(), None);
        assert!(l.sample_peak.is_finite());
        assert_eq!(album_loudness(&[&l]).unwrap(), None);
    }
    let mut samples = tone(48_000, 1.0, 0.5, 2);
    samples.extend(tone(48_000, 1.0, 0.25, 2));
    let file = write_wav("loudness-cue.wav", 48_000, 2, 16, &samples);
    let a = analyze(
        &file.0,
        Some(FrameRange {
            start: 48_000,
            end: Some(96_000),
        }),
    );
    assert_eq!(a.waveform.unwrap().frames, 48_000);
    assert!((a.loudness.unwrap().integrated_lufs.unwrap() + 12.0).abs() < 0.15);
    assert!(
        analyze_audio(
            Arc::new(FileSource::open(&file.0).unwrap()),
            Some("wav"),
            None,
            &AtomicBool::new(true),
            true,
            true
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn lossless_decoding_matches_pcm() {
    let wav = analyze(&fixture("sine44_a.wav"), None);
    let flac = analyze(&fixture("sine44_a.flac"), None);
    let a = wav.loudness.unwrap();
    let b = flac.loudness.unwrap();
    assert!((a.integrated_lufs.unwrap() - b.integrated_lufs.unwrap()).abs() < 0.001);
    assert_eq!(a.sample_peak, b.sample_peak);
    assert_eq!(wav.waveform, flac.waveform);
}
