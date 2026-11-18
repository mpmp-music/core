//! Single-track behaviour on the null output: bit-exactness, resampling length, pause,
//! errors.

mod support;

use mpmp_engine::output::SampleType;
use mpmp_engine::{BitPerfect, EngineError, EngineEvent, EntryId, wait_until};
use mpmp_media_io::MediaUri;
use support::{CHUNK, Harness, TIMEOUT, TempFile, format, noise, temp_path, track, write_wav};

fn play_file(h: &mut Harness, file: &TempFile) -> Vec<f32> {
    h.engine.play(track(1, &file.0)).unwrap();
    h.play_to_end()
}

#[test]
fn sixteen_bit_is_bit_exact_through_f32() {
    let samples = noise(2 * 10_000, 16);
    let file = write_wav("exact-f32.wav", 44_100, 2, 16, &samples);
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    let rendered = play_file(&mut h, &file);
    let expected: Vec<f32> = samples.iter().map(|&s| s as f32 / 32_768.0).collect();
    support::assert_samples_eq(&rendered, &expected, "f32 output");
}

#[test]
fn sixteen_bit_is_bit_exact_through_i16() {
    let samples = noise(2 * 10_000, 16);
    let file = write_wav("exact-i16.wav", 48_000, 2, 16, &samples);
    let mut h = Harness::new(format(48_000, 2, SampleType::I16));
    let rendered = play_file(&mut h, &file);
    let rendered: Vec<i16> = rendered.iter().map(|&s| (s * 32_768.0) as i16).collect();
    let expected: Vec<i16> = samples.iter().map(|&s| s as i16).collect();
    assert_eq!(rendered, expected);
}

#[test]
fn twenty_four_bit_is_bit_exact_through_i32() {
    let samples = noise(2 * 5_000, 24);
    let file = write_wav("exact-i32.wav", 96_000, 2, 24, &samples);
    let mut h = Harness::new(format(96_000, 2, SampleType::I32));
    let rendered = play_file(&mut h, &file);
    let rendered: Vec<i32> = rendered
        .iter()
        .map(|&s| (f64::from(s) * 2_147_483_648.0) as i32)
        .collect();
    let expected: Vec<i32> = samples.iter().map(|&s| s << 8).collect();
    assert_eq!(rendered, expected);
}

#[test]
fn resampling_preserves_duration_exactly() {
    let frames = 44_100 + 123;
    let file = write_wav("resample.wav", 44_100, 2, 16, &noise(2 * frames, 16));
    let mut h = Harness::new(format(48_000, 2, SampleType::F32));
    let rendered = play_file(&mut h, &file);
    let expected_frames = (frames as u64 * 48_000).div_ceil(44_100) as usize;
    assert_eq!(rendered.len(), expected_frames * 2);
}

#[test]
fn mono_is_duplicated_to_stereo() {
    let samples = noise(4_000, 16);
    let file = write_wav("mono.wav", 44_100, 1, 16, &samples);
    let mut h = Harness::new(format(44_100, 2, SampleType::I16));
    let rendered = play_file(&mut h, &file);
    let expected: Vec<f32> = samples
        .iter()
        .flat_map(|&s| [s as f32 / 32_768.0, s as f32 / 32_768.0])
        .collect();
    support::assert_samples_eq(&rendered, &expected, "mono to stereo");
}

#[test]
fn track_started_carries_the_signal_path() {
    let file = write_wav("signal.wav", 44_100, 1, 16, &noise(3_000, 16));
    let mut h = Harness::new(format(48_000, 2, SampleType::F32));
    h.engine.play(track(7, &file.0)).unwrap();
    h.render_until(|e| matches!(e, EngineEvent::TrackStarted { .. }));
    let Some(EngineEvent::TrackStarted { entry, signal_path }) = h.seen.first().cloned() else {
        panic!("{:?}", h.seen);
    };
    assert_eq!(entry, EntryId(7));
    assert_eq!(signal_path.source.codec, "PCM");
    assert!(signal_path.source.lossless);
    let BitPerfect::No(reasons) = signal_path.bit_perfect else {
        panic!("{:?}", signal_path.bit_perfect);
    };
    assert_eq!(reasons, ["resample: 44100 Hz → 48000 Hz"]);
}

#[test]
fn pause_fades_to_silence_and_holds_position() {
    let frames = 20_000;
    let file = write_wav("pause.wav", 44_100, 2, 16, &noise(2 * frames, 16));
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(track(1, &file.0)).unwrap();

    let mut played = 0;
    while played < 2_000 {
        played += h.render().len() / 2;
    }
    h.engine.pause();
    let held = h.engine.position().unwrap();
    // 10 ms fade at 44.1 kHz is 441 frames; after that only silence.
    played += h.render().len() / 2;
    assert!(h.engine.is_silent());
    for _ in 0..5 {
        assert!(h.output.render_f32(CHUNK).iter().all(|&s| s == 0.0));
    }
    // The visible cursor freezes at the pause request; the fade still counts as
    // rendered/heard content but cannot advance that cursor.
    assert!(held.frame <= played as u64);
    assert_eq!(h.engine.position().unwrap(), held);
    assert_eq!(h.engine.buffer_status().unwrap().underruns, 0);

    h.engine.resume();
    let rest = h.play_to_end();
    assert_eq!(played + rest.len() / 2, frames);
}

#[test]
fn end_of_queue_is_reported_after_the_last_frame() {
    let file = write_wav("finished.wav", 44_100, 2, 16, &noise(2 * 3_000, 16));
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(track(1, &file.0)).unwrap();
    let mut content = 0;
    while content < 3_000 {
        content += h.render().len() / 2;
        assert!(
            !h.drain_events()
                .contains(&EngineEvent::EndOfQueue { entry: EntryId(1) })
        );
    }
    h.render();
    assert!(wait_until(TIMEOUT, || h
        .events
        .try_iter()
        .any(|e| e == EngineEvent::EndOfQueue { entry: EntryId(1) })));
}

#[test]
fn stop_closes_the_stream() {
    let file = write_wav("stop.wav", 44_100, 2, 16, &noise(2 * 100_000, 16));
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(track(1, &file.0)).unwrap();
    assert!(h.output.is_open());
    // The decode thread is blocked on a full ring; stop must still return promptly.
    assert!(wait_until(TIMEOUT, || h
        .engine
        .buffer_status()
        .unwrap()
        .buffered_frames
        > 0));
    h.engine.stop();
    assert!(!h.output.is_open());
    assert!(h.engine.position().is_none());
}

#[test]
fn unreadable_media_is_rejected() {
    let path = temp_path("garbage.wav");
    std::fs::write(&path, b"this is not audio at all, just some bytes").unwrap();
    let file = TempFile(path);
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    assert!(matches!(
        h.engine.play(track(1, &file.0)),
        Err(EngineError::Unsupported(_))
    ));
    assert!(!h.output.is_open());

    let missing = MediaUri::from_path(temp_path("missing.flac")).unwrap();
    let missing = mpmp_engine::TrackRef::new(EntryId(2), missing);
    assert!(matches!(
        h.engine.play(missing),
        Err(EngineError::Source(_))
    ));
}

// --- queue edge cases (from the M1 code review) ---

fn as_f32(samples: &[i32]) -> Vec<f32> {
    samples.iter().map(|&s| s as f32 / 32_768.0).collect()
}

#[test]
fn clearing_next_after_a_short_last_track_still_ends_the_queue() {
    // Both tracks are shorter than the ring: b has finished decoding before it is heard,
    // and core clears the next track only after b's TrackStarted.
    let a = noise(2 * 3_000, 16);
    let b: Vec<i32> = noise(2 * 5_000, 16).split_off(4_000);
    let (fa, fb) = (
        write_wav("short-a.wav", 44_100, 2, 16, &a),
        write_wav("short-b.wav", 44_100, 2, 16, &b),
    );
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(track(1, &fa.0)).unwrap();
    h.engine.set_next(Some(track(2, &fb.0)));
    let mut out = h.render_until(|e| {
        matches!(
            e,
            EngineEvent::TrackStarted {
                entry: EntryId(2),
                ..
            }
        )
    });
    h.engine.set_next(None);
    out.extend(h.play_to_end());
    let mut expected = as_f32(&a);
    expected.extend(as_f32(&b));
    support::assert_samples_eq(&out, &expected, "a then b");
}

#[test]
fn a_next_track_that_fails_to_open_ends_the_queue() {
    let a = noise(2 * 3_000, 16);
    let fa = write_wav("open-fail-a.wav", 44_100, 2, 16, &a);
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(track(1, &fa.0)).unwrap();
    h.engine
        .set_next(Some(track(2, &temp_path("does-not-exist.flac"))));
    let out = h.play_to_end();
    assert_eq!(out, as_f32(&a));
    assert!(h.seen.iter().any(|e| matches!(
        e,
        EngineEvent::TrackFailed {
            entry: EntryId(2),
            ..
        }
    )));
}

#[test]
fn seeking_an_entry_that_is_not_playing_changes_nothing() {
    let a = noise(2 * 20_000, 16);
    let fa = write_wav("seek-unknown.wav", 44_100, 2, 16, &a);
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(track(1, &fa.0)).unwrap();
    let mut out = h.render();
    h.engine.seek(EntryId(99), 1_000);
    out.extend(h.play_to_end());
    assert_eq!(out, as_f32(&a));
    let starts = h
        .seen
        .iter()
        .filter(|e| matches!(e, EngineEvent::TrackStarted { .. }))
        .count();
    assert_eq!(starts, 1);
}

#[test]
fn pausing_before_any_audio_starts_silently() {
    // e.g. restoring a session: play_from, then pause before anything was rendered. No
    // fade-out blip may consume the first frames.
    let a = noise(2 * 8_000, 16);
    let fa = write_wav("pause-first.wav", 44_100, 2, 16, &a);
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.pause();
    h.engine.play(track(1, &fa.0)).unwrap();
    h.engine.pause();
    assert!(wait_until(TIMEOUT, || h
        .engine
        .buffer_status()
        .unwrap()
        .buffered_frames
        > 0));
    for _ in 0..4 {
        assert!(h.output.render_f32(CHUNK).iter().all(|&s| s == 0.0));
    }
    assert_eq!(h.engine.buffer_status().unwrap().played_frames, 0);
    h.engine.resume();
    let out = h.play_to_end();
    // Fades in from silence: the first 10 ms are ramped, everything after is exact.
    assert_eq!(out.len(), a.len());
    assert_eq!(&out[2 * 441..], &as_f32(&a)[2 * 441..]);
}

#[test]
fn a_short_next_track_can_be_replaced_until_it_is_heard() {
    // b is shorter than the ring, so it finishes decoding while a is still heard; it
    // must still be replaceable.
    let a = noise(2 * 30_000, 16);
    let b: Vec<i32> = noise(2 * 3_000, 16).iter().map(|s| s / 2).collect();
    let c: Vec<i32> = noise(2 * 4_000, 16).iter().map(|s| -s).collect();
    let (fa, fb, fc) = (
        write_wav("replace-a.wav", 44_100, 2, 16, &a),
        write_wav("replace-b.wav", 44_100, 2, 16, &b),
        write_wav("replace-c.wav", 44_100, 2, 16, &c),
    );
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(track(1, &fa.0)).unwrap();
    h.engine.set_next(Some(track(2, &fb.0)));
    let mut out = Vec::new();
    while out.len() / 2 < 30_000 - 6_000 {
        out.extend(h.render());
    }
    // a's tail and all of b are in the ring by now.
    assert!(support::wait_until_buffered(&h, 6_000 + 2_000));
    h.engine.set_next(Some(track(3, &fc.0)));
    support::settle();
    out.extend(h.play_to_end());
    let mut expected = as_f32(&a);
    expected.extend(as_f32(&c));
    support::assert_samples_eq(&out, &expected, "a then c, b replaced");
}

#[test]
fn late_callbacks_are_counted() {
    // The device can run dry while our ring is full, if its callback is starved.
    let a = noise(2 * 20_000, 16);
    let fa = write_wav("late.wav", 44_100, 2, 16, &a);
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(track(1, &fa.0)).unwrap();
    h.render();
    h.render();
    assert_eq!(h.engine.buffer_status().unwrap().late_callbacks, 0);
    // A 512-frame buffer lasts 11.6 ms; 60 ms later the device has played silence.
    std::thread::sleep(std::time::Duration::from_millis(60));
    h.render();
    let status = h.engine.buffer_status().unwrap();
    assert_eq!(status.late_callbacks, 1);
    assert!(status.max_callback_gap >= std::time::Duration::from_millis(60));
    assert_eq!(status.underruns, 0, "our own ring never ran dry");
}

#[test]
fn starting_and_seeking_are_not_underruns() {
    // A cold start renders before the decode thread has produced anything, and a seek
    // drops what was buffered: both are expected gaps, not glitches.
    let a = noise(2 * 40_000, 16);
    let fa = write_wav("prime.wav", 44_100, 2, 16, &a);
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(track(1, &fa.0)).unwrap();
    let mut out = Vec::new();
    for _ in 0..3 {
        // No waiting for data: the renderer must hold back until primed.
        let before = h.engine.buffer_status().unwrap().played_frames;
        let samples = h.output.render_f32(CHUNK);
        let played = (h.engine.buffer_status().unwrap().played_frames - before) as usize;
        out.extend_from_slice(&samples[..played * 2]);
    }
    for _ in 0..10 {
        out.extend(h.render());
    }
    // The three callbacks right after the seek may still play old audio (the decode
    // thread has not seen the seek yet): have enough of it buffered that they cannot
    // run dry, which on a slow machine would be a real underrun, not the seek's gap.
    assert!(support::wait_until_buffered(&h, 4 * CHUNK as u64));
    h.engine.seek(EntryId(1), 20_000);
    // Unwaited callbacks: old audio fading out, nothing, or (on a fast machine) already
    // the new audio. Keep what they played; the check below looks at the tail's end.
    let mut tail = Vec::new();
    for _ in 0..3 {
        let before = h.engine.buffer_status().unwrap().played_frames;
        let samples = h.output.render_f32(CHUNK);
        let played = (h.engine.buffer_status().unwrap().played_frames - before) as usize;
        tail.extend_from_slice(&samples[..played * 2]);
    }
    tail.extend(h.play_to_end());
    assert_eq!(h.engine.buffer_status().unwrap().underruns, 0);
    // Everything before the seek was the start of the file, and the tail is exact.
    assert_eq!(&out[..], &as_f32(&a)[..out.len()]);
    // After the 10 ms fade-in that follows a seek (441 frames), the tail is exact.
    let expected_tail = &as_f32(&a)[2 * 20_000..];
    let tail = &tail[tail.len() - expected_tail.len()..];
    assert_eq!(&tail[2 * 441..], &expected_tail[2 * 441..]);
}

/// Counts how many of `rendered` (16-bit output, as f32) differ from plain rounding of
/// `exact` (in 16-bit LSB), asserting each is within TPDF dither (±1 LSB) of it.
#[track_caller]
fn dithered_samples(rendered: &[f32], exact: &[f64]) -> usize {
    assert_eq!(rendered.len(), exact.len());
    let mut differs = 0;
    for (i, (&got, &exact)) in rendered.iter().zip(exact).enumerate() {
        let got = f64::from(got) * 32_768.0;
        assert!(
            (got - exact).abs() <= 1.5,
            "sample {i}: {got} vs exact {exact}"
        );
        if got != exact.round() {
            differs += 1;
        }
    }
    differs
}

#[test]
fn deeper_sources_are_dithered_on_16_bit_output_and_16_bit_ones_stay_exact() {
    // A 16-bit track, then gaplessly a 24-bit one, at unity gain on a 16-bit device: the
    // first is bit-exact, the second dithered from its first frame.
    let a = noise(2 * 6_000, 16);
    let b = noise(2 * 8_000, 24);
    let fa = write_wav("dither-a16.wav", 44_100, 2, 16, &a);
    let fb = write_wav("dither-b24.wav", 44_100, 2, 24, &b);
    let mut h = Harness::new(format(44_100, 2, SampleType::I16));
    let out = h.play_queue(&[track(1, &fa.0), track(2, &fb.0)]);
    assert_eq!(out.len(), a.len() + b.len());
    support::assert_samples_eq(&out[..a.len()], &as_f32(&a), "16-bit source");
    let exact: Vec<f64> = b.iter().map(|&s| f64::from(s) / 256.0).collect();
    let differs = dithered_samples(&out[a.len()..], &exact);
    assert!(
        differs > b.len() / 5,
        "only {differs} of {} samples differ from plain rounding",
        b.len()
    );
}

#[test]
fn resampled_audio_is_dithered_on_16_bit_output() {
    // 15-bit noise, so that resampling overshoot cannot clip.
    let samples = noise(2 * 10_000, 15);
    let file = write_wav("dither-resampled.wav", 44_100, 2, 16, &samples);
    let mut reference = Harness::new(format(48_000, 2, SampleType::F32));
    let exact: Vec<f64> = play_file(&mut reference, &file)
        .iter()
        .map(|&s| f64::from(s) * 32_768.0)
        .collect();
    let mut h = Harness::new(format(48_000, 2, SampleType::I16));
    let out = play_file(&mut h, &file);
    let differs = dithered_samples(&out, &exact);
    assert!(
        differs > out.len() / 5,
        "only {differs} of {} samples differ from plain rounding",
        out.len()
    );
}

#[test]
fn discarding_a_prepared_stream_preserves_the_accepted_stream_and_generation() {
    let first = write_wav("prepared-old.wav", 44_100, 2, 16, &vec![8192; 2 * 44_100]);
    let next = write_wav(
        "prepared-next.wav",
        44_100,
        2,
        16,
        &vec![-16384; 2 * 44_100],
    );
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(track(1, &first.0)).unwrap();
    assert!(h.render().iter().all(|s| *s == 0.25));
    let generation = h.engine.output_generation();
    let heard = h.engine.heard(EntryId(1)).unwrap().frames;
    let prepared = h
        .engine
        .prepare_playback(track(2, &next.0), 0, true)
        .unwrap();
    assert_eq!(h.engine.output_generation(), generation);
    assert_eq!(h.engine.heard(EntryId(1)).unwrap().frames, heard);
    assert!(h.output.render_f32(512).iter().all(|s| *s == 0.0));
    assert!(h.engine.heard(EntryId(2)).is_none());
    drop(prepared);
    assert!(h.output.is_open());
    assert!(h.render().iter().all(|s| *s == 0.25));
    assert_eq!(h.engine.output_generation(), generation);
    assert_eq!(
        h.engine.heard(EntryId(1)).unwrap().frames,
        heard + CHUNK as u64
    );
}

#[test]
fn committing_a_prepared_stream_stays_silent_until_resume_and_retains_old_history() {
    let first = write_wav("commit-old.wav", 44_100, 2, 16, &vec![8192; 2 * 44_100]);
    let next = write_wav("commit-next.wav", 44_100, 2, 16, &vec![-16384; 2 * 44_100]);
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(track(1, &first.0)).unwrap();
    h.render();
    h.engine.pause();
    h.render();
    let prepared = h
        .engine
        .prepare_playback(track(2, &next.0), 0, true)
        .unwrap();
    h.engine.freeze_output();
    let heard = h.engine.heard(EntryId(1)).unwrap();
    h.engine.commit_playback(prepared);
    assert!(h.output.is_open());
    assert!(h.output.render_f32(512).iter().all(|s| *s == 0.0));
    h.engine.resume();
    let samples = h.render();
    assert!(samples.iter().all(|s| *s <= 0.0));
    assert!(samples.iter().any(|s| *s < 0.0));
    assert_eq!(h.engine.heard(EntryId(1)), Some(heard));
    assert!(h.engine.heard(EntryId(2)).unwrap().frames > 0);
}

#[test]
fn prepared_handoff_keeps_queued_and_new_eof_bound_to_their_playback_instances() {
    let first = write_wav("handoff-old.wav", 44_100, 2, 16, &vec![8192; 2 * 3_000]);
    let next = write_wav("handoff-next.wav", 44_100, 2, 16, &vec![-16384; 2 * 3_000]);
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(track(1, &first.0)).unwrap();
    let prepared = h
        .engine
        .prepare_playback(track(2, &next.0), 0, false)
        .unwrap();
    while h.engine.heard(EntryId(1)).map_or(0, |p| p.frames) < 3_000 {
        assert!(h.render().iter().all(|s| *s == 0.25));
    }
    h.render();
    // Leave both A's start and its real EOF queued across the same-stream commit.
    assert!(wait_until(TIMEOUT, || h.events.len() >= 2));
    h.engine.commit_playback(prepared);
    let old = h.drain_events();
    assert_eq!(
        old.iter()
            .filter(|e| matches!(e, EngineEvent::EndOfQueue { .. }))
            .collect::<Vec<_>>(),
        vec![&EngineEvent::EndOfQueue { entry: EntryId(1) }]
    );
    let samples = h.play_to_end();
    assert!(samples.iter().any(|s| *s < 0.0));
    assert_eq!(h.engine.heard(EntryId(1)).unwrap().frames, 3_000);
    assert_eq!(h.engine.heard(EntryId(2)).unwrap().frames, 3_000);
    assert_eq!(
        h.seen
            .iter()
            .filter_map(|e| match e {
                EngineEvent::EndOfQueue { entry } => Some(*entry),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![EntryId(1), EntryId(2)]
    );
}
