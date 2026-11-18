//! Volume and per-track gain (docs/design/loudness-volume.md): ramps instead of
//! steps, bit-perfect unity, TPDF dither on 16-bit output. The source signal is the
//! fixture sine (tests/fixtures/README.md), split into a=[0,37123), b=[37123,81457).

mod support;

use mpmp_engine::output::SampleType;
use mpmp_engine::{EngineEvent, EntryId};
use support::{CHUNK, Harness, fixture, format, sine, track};

const A: usize = 37_123;

const B: usize = 81_457;

const FULL: usize = 132_300;

/// 10 ms at 44.1 kHz: the length of every gain ramp.
const RAMP: usize = 441;

/// Enough buffered ahead that the renderer sees the next track before A's tail ramp.
const LOOKAHEAD: u64 = (support::CHUNK + 2 * RAMP) as u64;

fn full() -> mpmp_engine::TrackRef {
    track(1, &fixture("sine44_full.flac"))
}

fn scaled(samples: &[f32], gain: f32) -> Vec<f32> {
    samples.iter().map(|s| s * gain).collect()
}

/// Asserts that from frame `start` on, `out` follows `reference` scaled by a linear ramp
/// from `from` to `to` over `RAMP` frames (the first ramped frame is already one step in).
#[track_caller]
fn assert_ramp(out: &[f32], reference: &[f32], start: usize, from: f32, to: f32) {
    for k in 0..RAMP {
        let gain = from + (to - from) * (k + 1) as f32 / RAMP as f32;
        for c in 0..2 {
            let i = 2 * (start + k) + c;
            let expected = reference[i] * gain;
            assert!(
                (out[i] - expected).abs() < 2e-3,
                "ramp frame {k}, channel {c}: {} vs {expected}",
                out[i]
            );
        }
    }
}

/// No sample-to-sample jump larger than the signal's own steepest slope (a click would
/// be a step of up to the full amplitude).
#[track_caller]
fn assert_no_click(out: &[f32], reference: &[f32]) {
    let max_jump = |v: &[f32], c: usize| {
        let frames = v.as_chunks::<2>().0;
        frames
            .iter()
            .zip(&frames[1..])
            .map(|(x, y)| (y[c] - x[c]).abs())
            .fold(0.0, f32::max)
    };
    for c in 0..2 {
        let (o, r) = (max_jump(out, c), max_jump(reference, c));
        assert!(o <= r + 1e-3, "channel {c}: jump {o} vs the signal's {r}");
    }
}

/// Renders `chunks` buffers and returns them.
fn render_chunks(h: &mut Harness, chunks: usize) -> Vec<f32> {
    (0..chunks).flat_map(|_| h.render()).collect()
}

#[test]
fn explicit_unity_gain_is_bit_exact() {
    for sample_type in [SampleType::F32, SampleType::I16, SampleType::I32] {
        let mut h = Harness::new(format(44_100, 2, sample_type));
        h.engine.set_volume(0.25);
        h.engine.set_volume(1.0);
        h.engine.play(full().with_gain(1.0)).unwrap();
        let out = h.play_to_end();
        support::assert_samples_eq(&out, &sine(44_100, 0, FULL), &format!("{sample_type:?}"));
    }
}

#[test]
fn volume_change_ramps_over_10_ms() {
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(full()).unwrap();
    let mut out = render_chunks(&mut h, 20);
    let changed_at = out.len() / 2;
    h.engine.set_volume(0.5);
    out.extend(h.play_to_end());
    let reference = sine(44_100, 0, FULL);
    assert_eq!(out.len(), reference.len());

    support::assert_samples_eq(
        &out[..2 * changed_at],
        &reference[..2 * changed_at],
        "before",
    );
    assert_ramp(&out, &reference, changed_at, 1.0, 0.5);
    let settled = 2 * (changed_at + RAMP);
    support::assert_samples_eq(
        &out[settled..],
        &scaled(&reference[settled..], 0.5),
        "after the ramp",
    );
    assert_no_click(&out, &reference);
    assert_eq!(h.engine.applied_gain(), Some(0.5));
}

#[test]
fn i16_is_dithered_while_scaled_and_exact_again_at_unity() {
    let mut h = Harness::new(format(44_100, 2, SampleType::I16));
    // Set before playing: the first frame is already at 0.5, no ramp.
    h.engine.set_volume(0.5);
    h.engine.play(full()).unwrap();
    let mut out = render_chunks(&mut h, 20);
    let changed_at = out.len() / 2;
    h.engine.set_volume(1.0);
    out.extend(h.play_to_end());
    let reference = sine(44_100, 0, FULL);
    assert_eq!(out.len(), reference.len());

    // At 0.5: within TPDF dither (±1 LSB) plus rounding of the exact value, and visibly
    // not plain rounding.
    let lsb = |v: f32| f64::from(v) * 32_768.0;
    let mut differs = 0;
    for i in 0..2 * changed_at {
        let exact = lsb(reference[i]) * 0.5;
        let got = lsb(out[i]);
        assert!(
            (got - exact).abs() <= 1.5,
            "sample {i}: {got} vs exact {exact}"
        );
        if got != exact.round() {
            differs += 1;
        }
    }
    assert!(
        differs > changed_at / 2,
        "only {differs} of {} samples differ from plain rounding",
        2 * changed_at
    );

    // Back at unity after the ramp: untouched, so bit-exact and undithered.
    let settled = 2 * (changed_at + RAMP);
    support::assert_samples_eq(&out[settled..], &reference[settled..], "at unity");
    assert_eq!(h.engine.applied_gain(), Some(1.0));
}

#[test]
fn volume_zero_is_true_silence() {
    for sample_type in [SampleType::I16, SampleType::F32] {
        let mut h = Harness::new(format(44_100, 2, sample_type));
        h.engine.set_volume(0.0);
        h.engine.play(full()).unwrap();
        let out = h.play_to_end();
        // Playback still runs (and ends) as usual; only the samples are silent.
        assert_eq!(out.len(), FULL * 2, "{sample_type:?}");
        assert!(
            out.iter().all(|&s| s == 0.0),
            "{sample_type:?}: not silent (dithered?)"
        );
    }
}

#[test]
fn gapless_track_gain_ramps_at_the_boundary() {
    let tracks = [
        track(1, &fixture("sine44_a.flac")),
        track(2, &fixture("sine44_b.flac")).with_gain(0.5),
    ];
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.min_buffered = LOOKAHEAD;
    let out = h.play_queue(&tracks);
    let reference = sine(44_100, 0, B);
    assert_eq!(out.len(), reference.len());

    // A lower gain ahead is reached over A's last frames: B never gets A's gain.
    let ramp_start = A - RAMP;
    support::assert_samples_eq(
        &out[..2 * ramp_start],
        &reference[..2 * ramp_start],
        "a at unity",
    );
    assert_ramp(&out, &reference, ramp_start, 1.0, 0.5);
    support::assert_samples_eq(
        &out[2 * A..],
        &scaled(&reference[2 * A..], 0.5),
        "b at 0.5 from its first frame",
    );
    assert_no_click(&out, &reference);
}

#[test]
fn a_higher_gain_ahead_ramps_over_the_next_track() {
    let tracks = [
        track(1, &fixture("sine44_a.flac")).with_gain(0.5),
        track(2, &fixture("sine44_b.flac")),
    ];
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.min_buffered = LOOKAHEAD;
    let out = h.play_queue(&tracks);
    let reference = sine(44_100, 0, B);
    support::assert_samples_eq(
        &out[..2 * A],
        &scaled(&reference[..2 * A], 0.5),
        "a at 0.5 to its last frame",
    );
    assert_ramp(&out, &reference, A, 0.5, 1.0);
    let settled = 2 * (A + RAMP);
    support::assert_samples_eq(&out[settled..], &reference[settled..], "b at unity");
    assert_no_click(&out, &reference);
}

#[test]
fn a_boosted_quiet_track_does_not_boost_the_next_one() {
    // Found on Windows: quiet track at +15 dB, then a loud one at -12 dB. The loud track's
    // first 10 ms were played at the boost (far past full scale: clipping, a pop).
    let boost = 5.6;
    let cut = 0.25;
    let tracks = [
        track(1, &fixture("sine44_a.flac")).with_gain(boost),
        track(2, &fixture("sine44_b.flac")).with_gain(cut),
    ];
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.min_buffered = LOOKAHEAD;
    let out = h.play_queue(&tracks);
    let reference = sine(44_100, 0, B);
    for (i, (&got, &r)) in out[2 * A..].iter().zip(&reference[2 * A..]).enumerate() {
        assert!(
            got.abs() <= (r * cut).abs() + 1e-6,
            "b frame {}: {got} is louder than its own gain allows ({})",
            i / 2,
            r * cut
        );
    }
    assert_ramp(&out, &reference, A - RAMP, boost, cut);
}

#[test]
fn set_gain_applies_to_audio_decoded_later() {
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    let t = full();
    h.engine.play(t.clone()).unwrap();
    let mut out = render_chunks(&mut h, 10);
    let called_at = out.len() / 2;
    h.engine.set_gain(t.entry, 0.5);
    out.extend(h.play_to_end());
    let reference = sine(44_100, 0, FULL);
    assert_eq!(out.len(), reference.len());

    // Audio already buffered keeps the old gain; the change follows within about the
    // ring's length (200 ms) plus one decoded packet.
    let changed_at = (0..FULL)
        .find(|&f| out[2 * f..2 * f + 2] != reference[2 * f..2 * f + 2])
        .expect("the gain never changed");
    assert!(changed_at >= called_at, "{changed_at} < {called_at}");
    assert!(
        changed_at - called_at < 44_100 / 2,
        "took {} frames",
        changed_at - called_at
    );
    // `changed_at` is the ramp's first frame (or the one after, if that frame was
    // exactly zero in both channels); from one ramp length later it is exactly 0.5x.
    let settled = 2 * (changed_at + RAMP);
    support::assert_samples_eq(
        &out[settled..],
        &scaled(&reference[settled..], 0.5),
        "after set_gain",
    );
    assert_no_click(&out, &reference);
}

#[test]
fn set_gain_reaches_a_next_track_that_is_still_opening() {
    // Sent right after set_next: the next track may not even be open yet.
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.min_buffered = LOOKAHEAD;
    h.engine.play(track(1, &fixture("sine44_a.flac"))).unwrap();
    h.engine.set_next(Some(track(2, &fixture("sine44_b.flac"))));
    h.engine.set_gain(EntryId(2), 0.5);
    let out = h.play_to_end();
    assert!(h.seen.iter().any(|e| matches!(
        e,
        EngineEvent::TrackStarted {
            entry: EntryId(2),
            ..
        }
    )));
    let reference = sine(44_100, 0, B);
    assert_eq!(out.len(), reference.len());
    // B's lower gain is reached over A's tail (see `gapless_track_gain_ramps_...`).
    let ramp_start = A - RAMP;
    support::assert_samples_eq(
        &out[..2 * ramp_start],
        &reference[..2 * ramp_start],
        "a at unity",
    );
    assert_ramp(&out, &reference, ramp_start, 1.0, 0.5);
    support::assert_samples_eq(&out[2 * A..], &scaled(&reference[2 * A..], 0.5), "b at 0.5");
}

#[test]
fn set_gain_of_an_unknown_entry_changes_nothing() {
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(full()).unwrap();
    let mut out = h.render();
    h.engine.set_gain(EntryId(99), 0.5);
    out.extend(h.play_to_end());
    support::assert_samples_eq(&out, &sine(44_100, 0, FULL), "unchanged");
    assert!(out.len() > CHUNK);
}

#[test]
fn a_very_short_track_keeps_its_gain_to_its_last_frame() {
    // A (50 ms) and B (100 ms) fit in the ring (200 ms) except for B's last frames, which
    // wait inside the resampler (44.1 kHz → 48 kHz) for C's input. When A is first heard,
    // B has finished decoding; its gain must still reach that tail.
    const DC: i32 = 8_192;
    let a = support::write_wav("short-a.wav", 44_100, 2, 16, &vec![DC; 2 * 2_205]);
    let b = support::write_wav("short-b.wav", 44_100, 2, 16, &vec![DC; 2 * 4_410]);
    let c = support::write_wav("short-c.wav", 44_100, 2, 16, &vec![DC; 2 * 13_230]);
    let tracks = [
        track(1, &a.0),
        track(2, &b.0).with_gain(0.5),
        track(3, &c.0).with_gain(0.25),
    ];
    let mut h = Harness::new(format(48_000, 2, SampleType::F32));
    h.engine.play(tracks[0].clone()).unwrap();
    h.engine.set_next(Some(tracks[1].clone()));
    // Let A and B be decoded completely before anything is heard.
    assert!(support::wait_until_buffered(&h, 2_400 + 2_400));
    std::thread::sleep(std::time::Duration::from_millis(100));
    // C is set only once B is heard, as core does; rendering faster than real time, the
    // test may then run dry before C arrives (no frames are lost, only time).
    let out = h.run_queue_allowing_underruns(&tracks);
    // 48 kHz frames: A [0, 2400), B [2400, 7200), C [7200, 21600), each boundary within
    // a few frames (the resampler maps it to the nearest output frame).
    assert_eq!(out.len(), 2 * 21_600);
    let level = DC as f32 / 32_768.0;
    let ramp = 480;
    for (name, frames, gain) in [
        // B's tail may already be ramping down to C's lower gain.
        ("b", 2_400 + 20 + ramp..7_200 - 20 - ramp, 0.5),
        ("c", 7_200 + 20 + ramp..21_600 - 100, 0.25),
    ] {
        for f in frames {
            let v = out[2 * f];
            assert!(
                (v - level * gain).abs() < 2e-3,
                "{name}: frame {f} is {v}, expected {}",
                level * gain
            );
        }
    }
}
