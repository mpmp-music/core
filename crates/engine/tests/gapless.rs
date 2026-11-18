//! Gapless seams, seek accuracy, partial flush and position on real encoded fixtures.
//! Fixtures: tests/fixtures/README.md. The source signal is split
//! into a=[0,37123), b=[37123,81457), c=[81457,132300) at 44.1 kHz.

mod support;

use mpmp_engine::output::SampleType;
use mpmp_engine::{EngineEvent, EntryId, FrameRange, TrackRef};
use support::{Harness, fixture, format, max_error, sine, track};

const A: usize = 37_123;

const B: usize = 81_457;

const FULL: usize = 132_300;

fn parts(ext: &str) -> Vec<TrackRef> {
    ["a", "b", "c"]
        .iter()
        .enumerate()
        .map(|(i, p)| track(i as u64 + 1, &fixture(&format!("sine44_{p}.{ext}"))))
        .collect()
}

fn play_full(h: &mut Harness, ext: &str) -> Vec<f32> {
    h.engine
        .play(track(100, &fixture(&format!("sine44_full.{ext}"))))
        .unwrap();
    h.play_to_end()
}

// --- seams ---

#[test]
fn lossless_seams_are_sample_exact() {
    for ext in ["flac", "wav"] {
        let mut h = Harness::new(format(44_100, 2, SampleType::F32));
        let out = h.play_queue(&parts(ext));
        assert_eq!(out.len(), FULL * 2, "{ext}");
        support::assert_samples_eq(&out, &sine(44_100, 0, FULL), ext);
    }
}

#[test]
fn resampled_seams_equal_resampling_the_whole() {
    // On a 48 kHz device the three parts share one resampler stream, so the result must
    // be bit-identical to resampling the unsplit file.
    let mut h = Harness::new(format(48_000, 2, SampleType::F32));
    let whole = play_full(&mut h, "flac");
    let mut h = Harness::new(format(48_000, 2, SampleType::F32));
    let joined = h.play_queue(&parts("flac"));
    assert_eq!(
        joined.len(),
        (FULL as u64 * 48_000).div_ceil(44_100) as usize * 2
    );
    support::assert_samples_eq(&joined, &whole, "joined vs whole");
}

#[test]
fn lossy_seams_have_no_gap_or_overlap() {
    // Each part was encoded separately, so near a seam the samples differ from the source
    // by more than elsewhere: the encoder saw a hard cut at the file edge (the unsplit
    // file shows the same at its own start). What must hold is alignment: the best match
    // of the output against the source after each seam is at offset 0. A gap or an
    // overlap would shift it, and push the error towards 1.0.
    for ext in ["mp3", "ogg"] {
        let mut h = Harness::new(format(44_100, 2, SampleType::F32));
        let out = h.play_queue(&parts(ext));
        assert_eq!(out.len(), FULL * 2, "{ext}: total length");
        let reference = sine(44_100, 0, FULL);
        for (name, seam) in [("a|b", A), ("b|c", B)] {
            assert_eq!(
                best_offset(&out, &reference, seam + 200..seam + 1_200),
                0,
                "{ext} seam {name}"
            );
            let window = (seam - 2_000) * 2..(seam + 2_000) * 2;
            let err = max_error(&out[window.clone()], &reference[window]);
            assert!(err < 0.1, "{ext} seam {name}: max error {err}");
        }
    }
}

/// The shift (in frames, within ±50) that best aligns `out` to `reference` over `frames`.
fn best_offset(out: &[f32], reference: &[f32], frames: std::ops::Range<usize>) -> i64 {
    (-50i64..=50)
        .map(|shift| {
            let err: f32 = frames
                .clone()
                .map(|f| {
                    let r = (f as i64 + shift) as usize;
                    (out[2 * f] - reference[2 * r]).abs()
                        + (out[2 * f + 1] - reference[2 * r + 1]).abs()
                })
                .sum();
            (err, shift)
        })
        .fold(
            (f32::MAX, 0),
            |best, cur| if cur.0 < best.0 { cur } else { best },
        )
        .1
}

#[test]
fn cross_rate_seam_keeps_exact_lengths() {
    let tracks = [
        track(1, &fixture("sine44_a.flac")),
        track(2, &fixture("sine48.flac")),
    ];
    let mut h = Harness::new(format(48_000, 2, SampleType::F32));
    let out = h.play_queue(&tracks);
    let a = (A as u64 * 48_000).div_ceil(44_100) as usize;
    assert_eq!(out.len(), (a + 48_000) * 2);
    // The 48 kHz track passes through untouched.
    assert_eq!(&out[a * 2..], &sine(48_000, 0, 48_000)[..]);
}

#[test]
fn ranges_play_part_of_a_file() {
    // CUE-style: the middle part as a range of the full file.
    let mut middle = track(1, &fixture("sine44_full.flac"));
    middle.range = Some(FrameRange {
        start: A as u64,
        end: Some(B as u64),
    });
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(middle).unwrap();
    let out = h.play_to_end();
    support::assert_samples_eq(&out, &sine(44_100, A, B), "range");
}

/// The tracks of a CUE sheet of one image: `[0, A)`, `[A, B)`, `[B, end)`.
fn cue_tracks(path: &std::path::Path) -> Vec<TrackRef> {
    [(0, Some(A)), (A, Some(B)), (B, None)]
        .into_iter()
        .enumerate()
        .map(|(i, (start, end))| {
            let mut t = track(i as u64 + 1, path);
            t.range = Some(FrameRange {
                start: start as u64,
                end: end.map(|e| e as u64),
            });
            t
        })
        .collect()
}

#[test]
fn consecutive_ranges_of_one_file_are_seamless() {
    // Consecutive tracks of one image play as the image does, sample for sample, also
    // for lossy formats: the next range goes on with the decoder of the one before.
    let samples: Vec<i32> = sine(44_100, 0, FULL)
        .iter()
        .map(|&x| (x * 32_767.0).round() as i32)
        .collect();
    let wav = support::write_wav("cue-image.wav", 44_100, 2, 16, &samples);
    let mut files = vec![("wav".to_owned(), wav.0.clone())];
    for ext in ["flac", "mp3", "ogg"] {
        files.push((ext.to_owned(), fixture(&format!("sine44_full.{ext}"))));
    }
    for (ext, path) in files {
        let mut h = Harness::new(format(44_100, 2, SampleType::F32));
        h.engine.play(track(100, &path)).unwrap();
        let whole = h.play_to_end();
        let mut h = Harness::new(format(44_100, 2, SampleType::F32));
        let joined = h.play_queue(&cue_tracks(&path));
        assert_eq!(joined.len(), whole.len(), "{ext}");
        support::assert_samples_eq(&joined, &whole, &ext);
    }
}

#[test]
fn a_range_after_a_seek_still_ends_and_hands_over_exactly() {
    // Seeking within a CUE track, then playing on into the next one.
    let path = fixture("sine44_full.flac");
    let tracks = cue_tracks(&path);
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play_from(tracks[1].clone(), 1_000).unwrap();
    h.engine.set_next(Some(tracks[2].clone()));
    let out = h.run_queue(&tracks);
    support::assert_samples_eq(&out, &sine(44_100, A + 1_000, FULL), "seek then next");
}

// --- events and position ---

#[test]
fn track_started_fires_when_the_track_is_heard() {
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    let tracks = parts("flac");
    h.engine.play(tracks[0].clone()).unwrap();
    h.engine.set_next(Some(tracks[1].clone()));
    let mut heard = 0;
    let mut started_b_at = None;
    while started_b_at.is_none() {
        heard += h.render().len() / 2;
        // Real-time pace: the event is due once the frames are heard, not when rendered.
        std::thread::sleep(std::time::Duration::from_micros(
            support::CHUNK as u64 * 1_000_000 / 44_100,
        ));
        for event in h.drain_events() {
            if matches!(
                event,
                EngineEvent::TrackStarted {
                    entry: EntryId(2),
                    ..
                }
            ) {
                started_b_at = Some(heard);
            }
        }
    }
    let started_b_at = started_b_at.unwrap();
    // Not before the first frame of b was rendered, and within a couple of chunks after
    // it (the decode thread checks the clock every few milliseconds).
    assert!(
        started_b_at > A && started_b_at <= A + 3 * support::CHUNK,
        "{started_b_at}"
    );
    let position = h.engine.position().unwrap();
    assert_eq!(position.entry, EntryId(2));
    let heard_of_b = (started_b_at - A) as u64;
    assert!(
        position.frame <= heard_of_b && position.frame + 2 * support::CHUNK as u64 >= heard_of_b,
        "{position:?} vs {heard_of_b}"
    );
}

// --- seek ---

#[test]
fn seek_is_sample_accurate() {
    for (ext, exact) in [("flac", true), ("mp3", false), ("ogg", false)] {
        let mut h = Harness::new(format(44_100, 2, SampleType::F32));
        let reference = play_full(&mut h, ext);
        for target in [1usize, 4_410, 50_001, 120_000] {
            let mut h = Harness::new(format(44_100, 2, SampleType::F32));
            h.engine
                .play_from(
                    track(1, &fixture(&format!("sine44_full.{ext}"))),
                    target as u64,
                )
                .unwrap();
            let out = h.play_to_end();
            let expected = &reference[target * 2..];
            assert_eq!(out.len(), expected.len(), "{ext} @ {target}: length");
            let err = max_error(&out, expected);
            if exact {
                assert_eq!(err, 0.0, "{ext} @ {target}");
            } else {
                assert!(err < 1e-4, "{ext} @ {target}: max error {err}");
            }
        }
    }
}

#[test]
fn seek_while_playing_restarts_at_the_target() {
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    let t = track(1, &fixture("sine44_full.flac"));
    h.engine.play(t.clone()).unwrap();
    for _ in 0..20 {
        h.render();
    }
    h.engine.seek(t.entry, 100_000);
    // Content may still come from before the seek until the flush is seen; the tail
    // must be exactly the target onward.
    let out = h.play_to_end();
    let tail = &out[out.len() - (FULL - 100_000) * 2..];
    // After the 10 ms fade-in (441 frames) the audio is exact.
    assert_eq!(&tail[2 * 441..], &sine(44_100, 100_000, FULL)[2 * 441..]);
    let before_seek = &out[..out.len() - tail.len()];
    assert!(
        before_seek.len() / 2 < 30 * support::CHUNK,
        "{} frames before the seek",
        before_seek.len() / 2
    );
}

// --- partial flush ---

#[test]
fn replacing_a_prefetched_next_track_is_seamless() {
    // a plays, b is set as next and starts decoding, then b is replaced by c before it
    // is heard: the output must be a followed directly by c. `full_ring`: wait until b's
    // frames fill the ring first (the usual case: b sits in the ring for the last
    // ring-length of a).
    for rate in [44_100, 48_000] {
        for full_ring in [false, true] {
            let mut h = Harness::new(format(rate, 2, SampleType::F32));
            let p = parts("flac");
            h.engine.play(p[0].clone()).unwrap();
            h.engine.set_next(Some(p[1].clone()));
            let mut out = Vec::new();
            // a is 0.84 s and the ring holds 0.2 s: stop rendering while a is still heard.
            while out.len() / 2 < (A * rate as usize / 44_100) - 12_000 {
                out.extend(h.render());
            }
            if full_ring {
                let ring = (rate as u64) / 5;
                assert!(support::wait_until_buffered(
                    &h,
                    ring - 2 * support::CHUNK as u64
                ));
            }
            h.engine.set_next(Some(p[2].clone()));
            support::settle();
            out.extend(h.play_to_end());
            let started: Vec<_> = h
                .seen
                .iter()
                .filter_map(|e| match e {
                    EngineEvent::TrackStarted { entry, .. } => Some(*entry),
                    _ => None,
                })
                .collect();
            assert_eq!(
                started,
                [EntryId(1), EntryId(3)],
                "{rate} Hz, full ring {full_ring}"
            );

            let mut reference_h = Harness::new(format(rate, 2, SampleType::F32));
            let reference = reference_h.play_queue(&[p[0].clone(), p[2].clone()]);
            assert_eq!(
                out.len(),
                reference.len(),
                "{rate} Hz, full ring {full_ring}: length"
            );
            let err = max_error(&out, &reference);
            if rate == 44_100 {
                assert_eq!(
                    err, 0.0,
                    "{rate} Hz, full ring {full_ring}: output differs from a then c"
                );
            } else if !full_ring {
                assert_eq!(err, 0.0, "{rate} Hz: output differs from a then c");
            } else {
                // a's last milliseconds were resampled with b in the filter's lookahead
                // and are already in the ring (the one approximate case). The error
                // stays small and local; c itself must be exact.
                assert!(err < 0.05, "{rate} Hz, full ring: max error {err}");
                let c_start = out.len() - (FULL - B) * 2 * 48_000 / 44_100 + 4_000;
                assert_eq!(max_error(&out[c_start..], &reference[c_start..]), 0.0);
            }
        }
    }
}

#[test]
fn set_next_after_track_started_queues_after_that_track() {
    // Once b is heard, set_next means "after b": b must play to its end, then c.
    // (The race where b becomes audible while core is replacing it is covered by the
    // flush-word unit tests in segment.rs.)
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    let p = parts("flac");
    h.engine.play(p[0].clone()).unwrap();
    h.engine.set_next(Some(p[1].clone()));
    let mut out = h.render_until(|e| {
        matches!(
            e,
            EngineEvent::TrackStarted {
                entry: EntryId(2),
                ..
            }
        )
    });
    h.engine.set_next(Some(p[2].clone()));
    out.extend(h.play_to_end());
    assert!(
        !h.seen
            .iter()
            .any(|e| matches!(e, EngineEvent::NextAlreadyStarted { .. }))
    );
    support::assert_samples_eq(&out, &sine(44_100, 0, FULL), "a, b, c");
}

#[test]
fn seek_fades_out_and_in_instead_of_cutting() {
    // A seek cuts the waveform; without a declick that is an audible click.
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    let t = track(1, &fixture("sine44_full.flac"));
    h.engine.play(t.clone()).unwrap();
    let mut out = Vec::new();
    for _ in 0..20 {
        out.extend(h.render());
    }
    // As in real playback, the ring is full when the seek comes.
    assert!(support::wait_until_buffered(
        &h,
        44_100 / 5 - 2 * support::CHUNK as u64
    ));
    h.engine.seek(t.entry, 100_000);
    // In real time the decode thread handles the seek long before the next buffer is
    // due; the test renders much faster, so give it that moment.
    std::thread::sleep(std::time::Duration::from_millis(30));
    out.extend(h.play_to_end());
    let new_start = out.len() - (FULL - 100_000) * 2;
    let expected = sine(44_100, 100_000, FULL);
    // New audio fades in from silence over 10 ms (441 frames), then is exact.
    assert!(
        out[new_start].abs() < 0.01,
        "first new sample {}",
        out[new_start]
    );
    assert_eq!(&out[new_start + 2 * 441..], &expected[2 * 441..]);
    // The cut-off audio before it faded out: its last frame is near silence, and the
    // fade lasted 5 ms (220 frames) of what was playing.
    assert!(
        out[new_start - 2].abs() < 0.01,
        "last old sample {}",
        out[new_start - 2]
    );
    let fade = &out[new_start - 2 * 220..new_start];
    let reference = sine(44_100, 0, FULL);
    let old_end = new_start / 2;
    assert!(
        fade.iter()
            .zip(&reference[2 * (old_end - 220)..2 * old_end])
            .all(|(a, b)| a.abs() <= b.abs() + 1e-6)
    );
}

#[test]
fn seeking_back_to_the_start_restarts_the_track() {
    // A reused decoder has read ahead; seeking to frame 0 must still go back.
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    let t = track(1, &fixture("sine44_full.flac"));
    h.engine.play(t.clone()).unwrap();
    for _ in 0..40 {
        h.render();
    }
    h.engine.seek(t.entry, 0);
    std::thread::sleep(std::time::Duration::from_millis(30));
    let out = h.play_to_end();
    let tail = &out[out.len() - FULL * 2..];
    assert_eq!(&tail[2 * 441..], &sine(44_100, 0, FULL)[2 * 441..]);
}

#[test]
fn seeking_past_the_end_moves_on_instead_of_failing() {
    // `f` near the end of a track: a target beyond its length is reaching the end.
    let p = parts("flac");
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(p[0].clone()).unwrap();
    h.engine.set_next(Some(p[1].clone()));
    for _ in 0..10 {
        h.render();
    }
    h.engine.seek(p[0].entry, 10_000_000);
    std::thread::sleep(std::time::Duration::from_millis(30));
    let out = h.play_to_end();
    assert!(
        !h.seen
            .iter()
            .any(|e| matches!(e, EngineEvent::TrackFailed { .. })),
        "{:?}",
        h.seen
    );
    assert!(h.seen.iter().any(|e| matches!(
        e,
        EngineEvent::TrackStarted {
            entry: EntryId(2),
            ..
        }
    )));
    let b = sine(44_100, A, B);
    let tail = &out[out.len() - b.len()..];
    assert_eq!(&tail[2 * 441..], &b[2 * 441..]);

    // On the last track it ends the queue.
    let mut h = Harness::new(format(44_100, 2, SampleType::F32));
    h.engine.play(p[2].clone()).unwrap();
    for _ in 0..10 {
        h.render();
    }
    h.engine.seek(p[2].entry, 10_000_000);
    h.play_to_end();
    assert!(
        !h.seen
            .iter()
            .any(|e| matches!(e, EngineEvent::TrackFailed { .. })),
        "{:?}",
        h.seen
    );
}

#[test]
fn seeks_with_a_full_ring_do_not_underrun() {
    // At a seek the ring is full of audio about to be dropped, so little new audio can be
    // written yet. Playback must wait until enough of it is buffered, not start on the
    // first new frames and run dry right after.
    let mut h = Harness::new(format(48_000, 2, SampleType::F32));
    let t = track(1, &fixture("sine44_full.flac"));
    h.engine.play(t.clone()).unwrap();
    for target in [20_000u64, 60_000, 5_000, 90_000, 40_000] {
        for _ in 0..8 {
            h.render();
        }
        assert!(support::wait_until_buffered(
            &h,
            48_000 / 5 - 2 * support::CHUNK as u64
        ));
        h.engine.seek(t.entry, target);
        // The decode thread handles the seek and writes what little fits behind the
        // old audio, then sleeps until its next poll.
        std::thread::sleep(std::time::Duration::from_millis(5));
        for i in 0..30 {
            h.output.render_f32(support::CHUNK);
            // The first callbacks come before the decode thread wakes again (on Windows
            // a 10 ms sleep lasts ~16 ms): those catch a renderer that starts on the
            // first new frames. After that, a little slower than real time, so a busy
            // CI machine stalling the decode thread does not count as a failure.
            if i >= 3 {
                std::thread::sleep(std::time::Duration::from_millis(14));
            }
        }
        assert_eq!(
            h.engine.buffer_status().unwrap().underruns,
            0,
            "after seeking to {target}"
        );
    }
}
