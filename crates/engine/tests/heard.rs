//! Time heard per entry (`Engine::heard`): only frames the output actually played count,
//! however the entry was seeked, paused, flushed or stopped. The fixture is the 3 s sine
//! (tests/fixtures/README.md) at 44.1 kHz, played on a 44.1 kHz device, so heard frames
//! are track frames. A test renders much faster than real time; every `render()` returns
//! exactly the content frames it played.

mod support;

use mpmp_engine::output::SampleType;
use mpmp_engine::{EngineEvent, EntryId, Heard, TrackRef};
use support::{CHUNK, Harness, fixture, format, settle, track};

const RATE: u32 = 44_100;

const FULL: u64 = 132_300;

const A: u64 = 37_123;

const B: u64 = 81_457 - 37_123;

/// The fade-out of audio cut off by a seek: it is played, so it counts.
const DECLICK: u64 = RATE as u64 / 200;

fn full(entry: u64) -> TrackRef {
    track(entry, &fixture("sine44_full.flac"))
}

fn harness() -> Harness {
    Harness::new(format(RATE, 2, SampleType::F32))
}

fn heard(h: &Harness, entry: u64) -> u64 {
    let heard = h.engine.heard(EntryId(entry)).expect("entry unknown");
    assert_eq!(heard.sample_rate, RATE);
    heard.frames
}

/// Renders until at least `frames` content frames have played; returns how many did.
fn render_frames(h: &mut Harness, frames: u64) -> u64 {
    let mut played = 0;
    while played < frames {
        played += (h.render().len() / 2) as u64;
    }
    played
}

#[test]
fn quick_seeks_count_only_what_was_played() {
    let mut h = harness();
    h.engine.play(full(1)).unwrap();
    assert_eq!(heard(&h, 1), 0, "known, nothing heard yet");
    let first = render_frames(&mut h, u64::from(RATE));
    assert_eq!(heard(&h, 1), first);

    // Several seeks before any of their audio plays: only the fade-out of what was
    // playing is heard in between.
    for target in [100_000, 20_000, 120_000, 60_000] {
        h.engine.seek(EntryId(1), target);
    }
    settle();
    let more = render_frames(&mut h, u64::from(RATE) * 3 / 10);
    let total = heard(&h, 1);
    assert_eq!(total, first + more, "heard equals what was rendered");
    let expected = u64::from(RATE) * 13 / 10;
    assert!(
        // Each loop may overshoot by up to a buffer.
        total >= expected && total <= expected + DECLICK + 2 * CHUNK as u64,
        "heard {total} frames, expected about {expected}"
    );
    let seconds = h
        .engine
        .heard(EntryId(1))
        .unwrap()
        .as_duration()
        .as_secs_f64();
    assert!((seconds - 1.3).abs() < 0.03, "{seconds} s");
}

#[test]
fn seeking_near_the_end_counts_the_tail_only() {
    const TAIL: u64 = 22_050;
    // Seeked before anything was heard: exactly the tail.
    let mut h = harness();
    h.engine.play(full(1)).unwrap();
    h.engine.seek(EntryId(1), FULL - TAIL);
    settle();
    let out = h.play_to_end();
    assert_eq!(out.len() as u64 / 2, TAIL);
    assert_eq!(heard(&h, 1), TAIL);

    // Seeked after a little: that little, the fade-out at the seek, and the tail.
    let mut h = harness();
    h.engine.play(full(1)).unwrap();
    let before = render_frames(&mut h, 4 * CHUNK as u64);
    h.engine.seek(EntryId(1), FULL - TAIL);
    // Until the decode thread has flushed, the old audio keeps playing (and counts).
    settle();
    let after = h.play_to_end().len() as u64 / 2;
    let total = heard(&h, 1);
    assert_eq!(total, before + after);
    assert!(
        (before + TAIL..=before + TAIL + DECLICK).contains(&total),
        "heard {total}, before the seek {before}"
    );
}

#[test]
fn gapless_frames_count_for_their_own_track() {
    let tracks = [
        track(1, &fixture("sine44_a.flac")),
        track(2, &fixture("sine44_b.flac")),
    ];
    let mut h = harness();
    h.engine.play(tracks[0].clone()).unwrap();
    h.engine.set_next(Some(tracks[1].clone()));
    // Right after the boundary was rendered: B starts from zero.
    let rendered = render_frames(&mut h, A + 1);
    assert!(rendered - A <= CHUNK as u64);
    assert_eq!(heard(&h, 1), A);
    assert_eq!(heard(&h, 2), rendered - A);
    // Asked once the next track has started, as core does.
    let rendered = rendered
        + h.render_until(
            |e| matches!(e, EngineEvent::TrackStarted { entry, .. } if *entry == EntryId(2)),
        )
        .len() as u64
            / 2;
    assert_eq!(heard(&h, 1), A);
    assert_eq!(heard(&h, 2), rendered - A);
    let more = h.play_to_end().len() as u64 / 2;
    assert!(more > 0);
    assert_eq!(heard(&h, 2), B);
    assert_eq!(heard(&h, 1), A);
}

#[test]
fn paused_time_is_not_counted() {
    let mut h = harness();
    h.engine.play(full(1)).unwrap();
    let mut played = render_frames(&mut h, 10 * CHUNK as u64);
    h.engine.pause();
    // The fade-out is played (and counted); then only silence.
    while !h.engine.is_silent() {
        played += (h.render().len() / 2) as u64;
    }
    assert_eq!(heard(&h, 1), played);
    for _ in 0..50 {
        assert!(h.render().is_empty());
    }
    assert_eq!(heard(&h, 1), played);
    h.engine.resume();
    played += render_frames(&mut h, 10 * CHUNK as u64);
    assert_eq!(heard(&h, 1), played);
}

#[test]
fn heard_stays_readable_after_stop_and_adds_up_across_streams() {
    let mut h = harness();
    h.engine.play(full(1)).unwrap();
    let first = render_frames(&mut h, 8 * CHUNK as u64);
    h.engine.stop();
    assert_eq!(heard(&h, 1), first);
    assert!(h.engine.heard(EntryId(2)).is_none(), "never played");

    // Another entry in a new stream: the old one is still there.
    h.engine.play(full(2)).unwrap();
    let second = render_frames(&mut h, 3 * CHUNK as u64);
    assert_eq!(heard(&h, 1), first);
    assert_eq!(heard(&h, 2), second);
    h.engine.stop();

    // The same entry resumed in a new stream (as after closing an idle output) adds up.
    h.engine.play_from(full(1), first).unwrap();
    let third = render_frames(&mut h, 2 * CHUNK as u64);
    assert_eq!(heard(&h, 1), first + third);
    assert_eq!(heard(&h, 2), second);
    assert_eq!(
        h.engine.heard(EntryId(1)),
        Some(Heard {
            frames: first + third,
            sample_rate: RATE
        })
    );
}

#[test]
fn a_new_track_ends_the_count_of_the_old_one() {
    // `play` of another entry flushes what was buffered of the first: none of it counts.
    let mut h = harness();
    h.engine.play(full(1)).unwrap();
    let first = render_frames(&mut h, 6 * CHUNK as u64);
    h.engine.play(full(2)).unwrap();
    settle();
    let rendered = render_frames(&mut h, 6 * CHUNK as u64);
    let (one, two) = (heard(&h, 1), heard(&h, 2));
    assert_eq!(one + two, first + rendered);
    assert!(
        (first..=first + DECLICK).contains(&one),
        "first entry: {one} heard, {first} rendered before the switch"
    );
}

#[test]
fn freeze_commits_late_callbacks_and_device_delay_at_one_boundary() {
    use mpmp_engine::output::{RenderInfo, SampleBufMut};
    for delay in [
        std::time::Duration::ZERO,
        std::time::Duration::from_millis(100),
    ] {
        let mut h = harness();
        h.engine.play(full(1)).unwrap();
        assert!(support::wait_until_buffered(&h, 4096));
        let mut buffer = vec![0.0; 512 * 2];
        let info = RenderInfo {
            playback_delay: Some(delay),
        };
        h.output
            .render_with_info(SampleBufMut::F32(&mut buffer), &info);
        let captured = h.engine.position().unwrap();
        // A callback after the caller's old position capture must be included once.
        h.output
            .render_with_info(SampleBufMut::F32(&mut buffer), &info);
        let frozen = h.engine.freeze_output();
        let final_position = frozen.position.unwrap();
        assert!(final_position.frame > captured.frame);
        assert_eq!(final_position.entry, EntryId(1));
        assert_eq!(final_position.frame, 1024);
        assert_eq!(frozen.rendered_frames, 1024);
        assert_eq!(heard(&h, 1), 1024);
        assert!(!h.output.is_open());
        assert_eq!(
            frozen
                .events
                .iter()
                .filter(|e| matches!(
                    e,
                    EngineEvent::TrackStarted {
                        entry: EntryId(1),
                        ..
                    }
                ))
                .count(),
            1
        );
        h.engine
            .rebuild_output(full(1), final_position.frame)
            .unwrap();
        assert!(support::wait_until_buffered(&h, 4096));
        let samples = h.output.render_f32(512);
        support::assert_samples_eq(
            &samples,
            &support::sine(RATE, 1024, 1536),
            "recovery has no replay",
        );
        assert_eq!(heard(&h, 1), 1536);
    }
}
