//! Real renderer callbacks inside output destruction, with no timing sleeps.
use super::*;
use crate::output::{RenderInfo, SampleBufMut, SampleType};
use crate::{NullOutput, NullOutputHandle};

const RATE: u32 = 44_100;

const TOTAL: u64 = 8192;

const BOUNDARY: u64 = 1024;

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "decode thread stalled");
        thread::yield_now();
    }
}

#[derive(Debug)]
struct Callback {
    paused: bool,
    closing: bool,
    silent: bool,
    played: u64,
    consumed: u64,
    drained: u64,
    samples: Vec<f32>,
}

/// Keeps the native stream alive while its destructor continues requesting buffers.
struct RenderDuringDrop {
    inner: Box<dyn OutputStream>,
    output: NullOutputHandle,
    shared: Arc<Shared>,
    callbacks: Arc<Mutex<Vec<Callback>>>,
    info: RenderInfo,
}

impl OutputStream for RenderDuringDrop {
    fn format(&self) -> StreamFormat {
        self.inner.format()
    }
}

impl Drop for RenderDuringDrop {
    fn drop(&mut self) {
        // Four small callbacks span the ~441-frame fade, followed by two large ones.
        for frames in [128, 128, 128, 128, 1024, 1024] {
            let mut samples = vec![f32::NAN; frames * 2];
            self.output
                .render_with_info(SampleBufMut::F32(&mut samples), &self.info);
            self.callbacks.lock().unwrap().push(Callback {
                paused: self.shared.paused.load(Ordering::Acquire),
                closing: self.shared.closing.load(Ordering::Acquire),
                silent: self.shared.silent.load(Ordering::Acquire),
                played: self.shared.frames_played.load(Ordering::Acquire),
                consumed: self.shared.frames_consumed.load(Ordering::Acquire),
                drained: self.shared.drained.load(Ordering::Acquire),
                samples,
            });
        }
    }
}

struct Harness {
    engine: Engine,
    output: NullOutputHandle,
    shared: Arc<Shared>,
    callbacks: Arc<Mutex<Vec<Callback>>>,
    tracks: Vec<TrackRef>,
    info: RenderInfo,
}

impl Harness {
    fn new(gapless: bool, delay: Duration) -> Self {
        Self::with_length(gapless, delay, TOTAL)
    }

    fn with_length(gapless: bool, delay: Duration, total: u64) -> Self {
        let output = NullOutput::new(StreamFormat {
            sample_rate: RATE,
            channels: 2,
            sample_type: SampleType::F32,
        });
        let handle = output.handle();
        let config = EngineConfig::default();
        let ring_frames = (config.buffer.as_secs_f64() * f64::from(RATE)).ceil() as u64;
        let mut engine = Engine::new(
            Box::new(output),
            Arc::new(mpmp_media_io::FileResolver),
            config,
        );
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/sine44_full.flac");
        let uri = MediaUri::from_path(path).unwrap();
        let mut first = TrackRef::new(EntryId(1), uri.clone());
        first.range = Some(FrameRange {
            start: 0,
            end: Some(if gapless { BOUNDARY } else { total }),
        });
        let mut tracks = vec![first];
        engine.play(tracks[0].clone()).unwrap();
        if gapless {
            let mut next = TrackRef::new(EntryId(2), uri);
            next.range = Some(FrameRange {
                start: BOUNDARY,
                end: Some(total),
            });
            engine.set_next(Some(next.clone()));
            tracks.push(next);
        }
        let session = engine.session.as_mut().unwrap();
        let shared = Arc::clone(&session.shared);
        shared.test_now_ns.store(0, Ordering::Release);
        let callbacks = Arc::new(Mutex::new(Vec::new()));
        let info = RenderInfo {
            playback_delay: Some(delay),
        };
        session.stream = Some(Box::new(RenderDuringDrop {
            inner: session.stream.take().unwrap(),
            output: handle.clone(),
            shared: Arc::clone(&shared),
            callbacks: Arc::clone(&callbacks),
            info,
        }));
        // Short content fits entirely in the ring. Long content instead waits until
        // the ring is full, without requiring the producer to reach EOF.
        until(|| {
            shared.frames_written.load(Ordering::Acquire) == total.min(ring_frames)
                && (gapless
                    || total > ring_frames
                    || shared.end_of_stream.load(Ordering::Acquire) != 0)
        });
        Self {
            engine,
            output: handle,
            shared,
            callbacks,
            tracks,
            info,
        }
    }

    fn render(&self, frames: usize) -> Vec<f32> {
        let mut samples = vec![f32::NAN; frames * 2];
        self.output
            .render_with_info(SampleBufMut::F32(&mut samples), &self.info);
        samples
    }

    fn check_fade(&self, before: u64) -> u64 {
        let callbacks = self.callbacks.lock().unwrap();
        assert_eq!(callbacks.len(), 6);
        let after = callbacks[3].played;
        let fade = after - before;
        // Allow one frame for the f32 ramp's rounding at zero.
        assert!((u64::from(RATE / 100)..=u64::from(RATE / 100) + 1).contains(&fade));
        assert!(callbacks[0].samples.iter().any(|&s| s != 0.0));
        let mut previous = before;
        for callback in callbacks.iter() {
            assert!(callback.paused);
            assert!(callback.closing);
            let rendered = callback.played - previous;
            assert!(
                callback.samples[rendered as usize * 2..]
                    .iter()
                    .all(|&s| s == 0.0)
            );
            previous = callback.played;
        }
        for callback in &callbacks[3..] {
            assert!(callback.silent);
            assert_eq!(callback.played, after);
            assert_eq!(callback.consumed, after, "only fade frames leave the ring");
        }
        assert!(
            callbacks[4..]
                .iter()
                .all(|c| c.samples.iter().all(|&s| s == 0.0))
        );
        assert_eq!(self.shared.frames_written.load(Ordering::Acquire), TOTAL);
        assert!(TOTAL > after, "valid content remains buffered and unplayed");
        assert_eq!(self.shared.underruns.load(Ordering::Acquire), 0);
        assert_eq!(self.shared.drained.load(Ordering::Acquire), 0);
        after
    }

    fn check_recovery(&mut self, position: Position, rebuild: bool) {
        let track = self
            .tracks
            .iter()
            .find(|t| t.entry == position.entry)
            .unwrap()
            .clone();
        let range = track.range.unwrap();
        let frames = 512.min((range.end.unwrap() - range.start - position.frame) as usize);
        if rebuild {
            self.engine.rebuild_output(track, position.frame).unwrap();
        } else {
            self.engine.play_from(track, position.frame).unwrap();
        }
        let session = self.engine.session.as_ref().unwrap();
        assert!(!Arc::ptr_eq(&session.shared, &self.shared));
        assert!(!session.shared.paused.load(Ordering::Acquire));
        assert!(!session.shared.closing.load(Ordering::Acquire));
        assert!(!self.engine.is_paused());
        until(|| {
            let status = self.engine.buffer_status().unwrap();
            status.buffered_frames >= frames as u64 && status.end_of_stream
        });
        let samples = self.render(frames);
        assert!(
            samples.iter().any(|&s| s != 0.0),
            "new Playing session must sound"
        );
        let mut timeline = lock(&session.timeline);
        timeline.sync();
        assert_eq!(
            timeline.position_at(0),
            Some(position),
            "resume at rendered boundary"
        );
        assert_eq!(
            timeline.position_at(frames as u64).unwrap().frame,
            position.frame + frames as u64
        );
    }
}

#[test]
fn close_fades_during_drop_and_stop_or_rebuild_keeps_rendered_boundary() {
    for freeze in [false, true] {
        for delay in [Duration::ZERO, Duration::from_millis(100)] {
            let mut h = Harness::new(false, delay);
            h.render(512);
            let frozen = if freeze {
                Some(h.engine.freeze_output())
            } else {
                h.engine.stop();
                None
            };
            let after = h.check_fade(512);
            assert!(!h.output.is_open());
            assert_eq!(h.engine.heard(EntryId(1)).unwrap().frames, after);
            let position = Position {
                entry: EntryId(1),
                frame: after,
                sample_rate: RATE,
            };
            if let Some(frozen) = frozen {
                assert_eq!(frozen.position, Some(position));
                assert_eq!(frozen.rendered_frames, after);
                assert_eq!(frozen.pending_seek, None);
                assert!(
                    !frozen
                        .events
                        .iter()
                        .any(|e| matches!(e, EngineEvent::EndOfQueue { .. }))
                );
            }
            h.check_recovery(position, freeze);
            assert_eq!(h.engine.heard(EntryId(1)).unwrap().frames, after + 512);
        }
    }
}

#[test]
fn close_near_gapless_boundary_recovers_the_entry_actually_rendered() {
    // In the first case the fade stops within A; in the second it reaches B.
    for before in [512, 768] {
        let mut h = Harness::new(true, Duration::from_millis(100));
        h.render(before);
        let frozen = h.engine.freeze_output();
        let after = h.check_fade(before as u64);
        assert_eq!(frozen.rendered_frames, after);
        let crossed = after > BOUNDARY;
        assert_eq!(crossed, before == 768);
        let position = Position {
            entry: EntryId(if crossed { 2 } else { 1 }),
            frame: if crossed { after - BOUNDARY } else { after },
            sample_rate: RATE,
        };
        assert_eq!(frozen.position, Some(position));
        assert_eq!(
            h.engine.heard(EntryId(1)).unwrap().frames,
            after.min(BOUNDARY)
        );
        assert_eq!(
            h.engine.heard(EntryId(2)).map_or(0, |h| h.frames),
            after.saturating_sub(BOUNDARY)
        );
        assert_eq!(
            frozen
                .events
                .iter()
                .filter(|e| matches!(
                    e,
                    EngineEvent::TrackStarted {
                        entry: EntryId(2),
                        ..
                    }
                ))
                .count(),
            usize::from(crossed)
        );
        assert!(
            !frozen
                .events
                .iter()
                .any(|e| matches!(e, EngineEvent::EndOfQueue { .. }))
        );
        h.check_recovery(position, true);
    }
}

#[test]
fn close_keeps_unrendered_seek_pending_and_flushed_frames_out_of_history() {
    let mut h = Harness::new(false, Duration::from_millis(100));
    h.render(512);
    let generation = h.shared.flush.load().lo;
    h.engine.seek(EntryId(1), 4096);
    until(|| h.shared.flush.load().lo > generation);
    let frozen = h.engine.freeze_output();
    // A full flush uses the existing ~5 ms declick, which can only shorten the
    // close fade. Discarding the flushed ring never counts as rendered content.
    let after = 512 + u64::from(RATE / 200);
    assert_eq!(frozen.rendered_frames, after);
    assert_eq!(frozen.position.unwrap().frame, after);
    assert_eq!(frozen.pending_seek, Some((EntryId(1), 4096)));
    assert_eq!(h.engine.heard(EntryId(1)).unwrap().frames, after);
    let callbacks = h.callbacks.lock().unwrap();
    assert_eq!(callbacks.len(), 6);
    for callback in &callbacks[3..] {
        assert!(callback.paused && callback.silent);
        assert!(callback.samples.iter().all(|&s| s == 0.0));
        assert_eq!(callback.played, after);
        assert_eq!(callback.consumed, callbacks[3].consumed);
    }
    assert!(
        callbacks[3].consumed > after,
        "flushed frames were discarded unplayed"
    );
    assert_eq!(h.shared.underruns.load(Ordering::Acquire), 0);
    assert_eq!(h.shared.drained.load(Ordering::Acquire), 0);
}

#[test]
fn close_reports_real_eof_for_short_exact_and_already_drained_tails() {
    for (before, already_drained) in [
        (TOTAL - 200, false),
        (TOTAL - 128, false),
        (TOTAL, false),
        (TOTAL, true),
    ] {
        let mut h = Harness::new(false, Duration::from_millis(100));
        let epoch = h.shared.end_of_stream.load(Ordering::Acquire);
        h.render(before as usize);
        if already_drained {
            h.render(128);
        }
        assert_eq!(
            h.shared.drained.load(Ordering::Acquire),
            if already_drained { epoch } else { 0 }
        );
        // The manual clock is before the device tail, so only FROZEN's final
        // observation can commit EndOfQueue, even in the already-drained case.
        assert!(
            !h.engine
                .events()
                .try_iter()
                .any(|e| matches!(e, EngineEvent::EndOfQueue { .. }))
        );
        let frozen = h.engine.freeze_output();
        assert_eq!(frozen.rendered_frames, TOTAL);
        assert_eq!(frozen.position.unwrap().frame, TOTAL);
        assert_eq!(h.engine.heard(EntryId(1)).unwrap().frames, TOTAL);
        assert_eq!(h.shared.drained.load(Ordering::Acquire), epoch);
        assert_eq!(h.shared.drained_at.load(Ordering::Acquire), TOTAL);
        assert_eq!(h.shared.underruns.load(Ordering::Acquire), 0);
        assert_eq!(
            frozen
                .events
                .iter()
                .filter(|e| matches!(e, EngineEvent::EndOfQueue { .. }))
                .count(),
            1
        );
        let callbacks = h.callbacks.lock().unwrap();
        assert_eq!(callbacks.len(), 6);
        assert!(callbacks.iter().all(|c| c.closing && c.paused));
        let confirms_at = usize::from(before < TOTAL);
        assert_eq!(callbacks[confirms_at].drained, epoch);
        if before == TOTAL - 128 {
            assert_eq!(callbacks[0].played, TOTAL);
            assert_eq!(
                callbacks[0].drained, 0,
                "exactly full buffer needs an empty callback"
            );
        }
    }
}

#[test]
fn close_does_not_turn_a_flushed_empty_ring_into_rendered_eof() {
    for before in [0, 512] {
        let mut h = Harness::new(false, Duration::from_millis(100));
        if before > 0 {
            h.render(before);
        }
        let epoch = h.shared.end_of_stream.load(Ordering::Acquire);
        h.engine.seek(EntryId(1), TOTAL);
        until(|| {
            let end = h.shared.end_of_stream.load(Ordering::Acquire);
            end != 0 && end != epoch
        });
        let frozen = h.engine.freeze_output();
        assert_eq!(
            h.shared.frames_written.load(Ordering::Acquire),
            h.shared.frames_consumed.load(Ordering::Acquire)
        );
        let rendered = if before == 0 {
            0
        } else {
            before as u64 + u64::from(RATE / 200)
        };
        assert_eq!(frozen.rendered_frames, rendered);
        assert_eq!(h.engine.heard(EntryId(1)).unwrap().frames, rendered);
        assert_eq!(frozen.pending_seek, Some((EntryId(1), TOTAL)));
        assert_eq!(h.shared.drained.load(Ordering::Acquire), 0);
        assert_eq!(h.shared.underruns.load(Ordering::Acquire), 0);
        assert!(
            !frozen
                .events
                .iter()
                .any(|e| matches!(e, EngineEvent::EndOfQueue { .. }))
        );
        let callbacks = h.callbacks.lock().unwrap();
        assert!(
            callbacks[3..]
                .iter()
                .all(|c| c.played == rendered && c.drained == 0 && c.silent)
        );
    }
}

#[test]
fn close_with_a_full_ring_joins_the_worker_without_waiting_for_consumption() {
    for freeze in [false, true] {
        let mut h = Harness::with_length(false, Duration::from_millis(100), 132_300);
        let capacity = h.shared.frames_written.load(Ordering::Acquire);
        assert!(capacity < 132_300);
        h.render(512);
        until(|| h.engine.buffer_status().unwrap().buffered_frames == capacity);
        assert_eq!(h.shared.end_of_stream.load(Ordering::Acquire), 0);
        let (done, completed) = crossbeam_channel::bounded(1);
        let closer = thread::spawn(move || {
            let frozen = if freeze {
                Some(h.engine.freeze_output())
            } else {
                h.engine.stop();
                None
            };
            done.send((h, frozen))
                .unwrap_or_else(|_| panic!("close result receiver dropped"));
        });
        // The timeout only bounds a deadlock failure; ring fullness and callback
        // ordering are established through counters, not elapsed time.
        let (h, frozen) = completed
            .recv_timeout(Duration::from_secs(5))
            .expect("closing a full ring must join the worker");
        closer.join().unwrap();
        assert!(!h.engine.is_active());
        assert!(!h.output.is_open());
        let after = h.shared.frames_played.load(Ordering::Acquire);
        assert!((512 + u64::from(RATE / 100)..=513 + u64::from(RATE / 100)).contains(&after));
        assert!(
            h.shared.frames_written.load(Ordering::Acquire)
                > h.shared.frames_consumed.load(Ordering::Acquire)
        );
        assert_eq!(h.engine.heard(EntryId(1)).unwrap().frames, after);
        assert_eq!(h.shared.underruns.load(Ordering::Acquire), 0);
        assert_eq!(h.shared.drained.load(Ordering::Acquire), 0);
        if let Some(frozen) = frozen {
            assert_eq!(frozen.rendered_frames, after);
            assert!(
                !frozen
                    .events
                    .iter()
                    .any(|e| matches!(e, EngineEvent::EndOfQueue { .. }))
            );
        }
        let callbacks = h.callbacks.lock().unwrap();
        assert_eq!(callbacks.len(), 6);
        assert!(
            callbacks[3..]
                .iter()
                .all(|c| c.played == after && c.consumed == after && c.silent)
        );
        assert!(
            callbacks[4..]
                .iter()
                .all(|c| c.samples.iter().all(|&s| s == 0.0))
        );
    }
}

#[test]
fn close_and_rebuild_preserve_a_finished_or_unfinished_user_pause() {
    for pause_frames in [128, 1024] {
        let mut h = Harness::new(false, Duration::from_millis(100));
        h.render(512);
        h.engine.pause();
        h.render(pause_frames);
        assert_eq!(h.engine.is_silent(), pause_frames == 1024);
        assert!(!h.shared.closing.load(Ordering::Acquire));
        let pause_index = h.shared.pause_index.load(Ordering::Acquire);
        let before = h.shared.frames_played.load(Ordering::Acquire);
        let frozen = h.engine.freeze_output();
        let after = frozen.rendered_frames;
        assert!((512 + u64::from(RATE / 100)..=513 + u64::from(RATE / 100)).contains(&after));
        assert_eq!(after == before, pause_frames == 1024);
        assert_eq!(h.shared.pause_index.load(Ordering::Acquire), pause_index);
        assert_eq!(h.engine.heard(EntryId(1)).unwrap().frames, after);
        assert_eq!(h.shared.drained.load(Ordering::Acquire), 0);
        assert!(
            !frozen
                .events
                .iter()
                .any(|e| matches!(e, EngineEvent::EndOfQueue { .. }))
        );
        let position = frozen.position.unwrap();
        assert_eq!(position.frame, after);
        {
            let callbacks = h.callbacks.lock().unwrap();
            assert!(
                callbacks[3..]
                    .iter()
                    .all(|c| c.silent && c.played == after && c.consumed == after)
            );
            assert!(
                callbacks[4..]
                    .iter()
                    .all(|c| c.samples.iter().all(|&s| s == 0.0))
            );
        }
        h.engine
            .rebuild_output(h.tracks[0].clone(), position.frame)
            .unwrap();
        let shared = Arc::clone(&h.engine.session.as_ref().unwrap().shared);
        assert!(!Arc::ptr_eq(&shared, &h.shared));
        assert!(!shared.closing.load(Ordering::Acquire));
        assert!(h.engine.is_paused());
        assert!(shared.paused.load(Ordering::Acquire));
        until(|| h.engine.buffer_status().unwrap().end_of_stream);
        assert!(h.render(1024).iter().all(|&s| s == 0.0));
        assert!(h.engine.is_silent());
        assert_eq!(shared.frames_played.load(Ordering::Acquire), 0);
        assert_eq!(shared.frames_consumed.load(Ordering::Acquire), 0);
        assert_eq!(shared.drained.load(Ordering::Acquire), 0);
        h.engine.resume();
        assert!(h.render(512).iter().any(|&s| s != 0.0));
        assert_eq!(h.engine.heard(EntryId(1)).unwrap().frames, after + 512);
    }
}

#[test]
fn user_pause_still_defers_eof_until_close() {
    let mut h = Harness::new(false, Duration::from_millis(100));
    h.render((TOTAL - 200) as usize);
    h.engine.pause();
    h.render(128);
    h.render(128);
    h.render(128);
    assert_eq!(h.shared.frames_played.load(Ordering::Acquire), TOTAL);
    assert_eq!(h.shared.drained.load(Ordering::Acquire), 0);
    assert_eq!(h.shared.underruns.load(Ordering::Acquire), 0);
    assert!(
        !h.engine
            .events()
            .try_iter()
            .any(|e| matches!(e, EngineEvent::EndOfQueue { .. }))
    );
    let frozen = h.engine.freeze_output();
    assert_eq!(
        frozen
            .events
            .iter()
            .filter(|e| matches!(e, EngineEvent::EndOfQueue { .. }))
            .count(),
        1
    );
}

#[test]
fn close_fade_samples_follow_the_known_signal_and_linear_envelope() {
    let mut h = Harness::new(false, Duration::from_millis(100));
    h.render(512);
    h.engine.freeze_output();
    h.check_fade(512);
    let callbacks = h.callbacks.lock().unwrap();
    for (index, &sample) in callbacks.iter().flat_map(|c| &c.samples).enumerate() {
        let frame = index / 2;
        let (frequency, phase) = if index % 2 == 0 {
            (997.0, 0.0)
        } else {
            (1499.0, 0.3)
        };
        // Fixture generator: scripts/gen_signal.py. Compare with the ideal linear
        // envelope, allowing only the accumulated rounding of the f32 pause ramp.
        let angle =
            2.0 * std::f64::consts::PI * frequency * (512 + frame) as f64 / f64::from(RATE) + phase;
        let original = (0.5 * angle.sin() * 32_767.0).round() / 32_768.0;
        let gain = (1.0 - (frame + 1) as f64 * 100.0 / f64::from(RATE)).max(0.0);
        let expected = original * gain;
        assert!(
            (f64::from(sample) - expected).abs() < 1e-5,
            "sample {index}: {sample} vs {expected}"
        );
    }
}
