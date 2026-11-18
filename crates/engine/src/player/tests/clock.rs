//! Real engine/NullOutput tests with a manually advanced monotonic clock.
use super::*;
use crate::NullOutput;
use crate::output::{OutputEvent, RenderInfo, RouteFacts, SampleBufMut, SampleType};
use std::path::Path;

fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "decoder/observer did not reach the condition"
        );
        thread::yield_now();
    }
}

struct Harness {
    engine: Engine,
    output: crate::NullOutputHandle,
    shared: Arc<Shared>,
    events: Receiver<EngineEvent>,
    started: Vec<EntryId>,
    ended: usize,
}

impl Harness {
    fn new() -> Self {
        Self::with_successor(true)
    }

    fn with_successor(successor: bool) -> Self {
        let output = NullOutput::new(StreamFormat {
            sample_rate: 44100,
            channels: 2,
            sample_type: SampleType::F32,
        });
        let handle = output.handle();
        let mut engine = Engine::new(
            Box::new(output),
            Arc::new(mpmp_media_io::FileResolver),
            EngineConfig::default(),
        );
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sine44_a.flac");
        let uri = MediaUri::from_path(path).unwrap();
        let mut first = TrackRef::new(EntryId(1), uri.clone());
        first.range = Some(FrameRange {
            start: 0,
            end: Some(4410),
        });
        engine.play(first).unwrap();
        let shared = Arc::clone(&engine.session.as_ref().unwrap().shared);
        shared.test_now_ns.store(0, Ordering::Release);
        if successor {
            engine.set_next(Some(TrackRef::new(EntryId(2), uri)));
        }
        let events = engine.events();
        Self {
            engine,
            output: handle,
            shared,
            events,
            started: Vec::new(),
            ended: 0,
        }
    }

    fn at(&mut self, ns: u64) {
        self.shared.test_now_ns.store(ns, Ordering::Release);
        until(|| self.shared.test_observed_ns.load(Ordering::Acquire) == ns);
        for event in self.events.try_iter() {
            match event {
                EngineEvent::TrackStarted { entry, .. } => self.started.push(entry),
                EngineEvent::EndOfQueue { .. } => self.ended += 1,
                _ => {}
            }
        }
    }

    fn render(&mut self, ms: u64, delay: Duration) {
        until(|| self.engine.buffer_status().unwrap().buffered_frames >= 4410);
        self.shared
            .test_now_ns
            .store(ms * 1_000_000, Ordering::Release);
        let mut samples = [0.0; 882];
        self.output.render_with_info(
            SampleBufMut::F32(&mut samples),
            &RenderInfo {
                playback_delay: Some(delay),
            },
        );
        self.at(ms * 1_000_000 + 1);
    }
}

#[test]
fn clock_reset_cannot_rewind_position_delay_track_start_or_change_heard_frames() {
    for default_switch in [false, true] {
        let mut h = Harness::new();
        let mut previous = 0;
        for callback in 0..=14 {
            if callback == 5 && default_switch {
                h.engine.device_sink.report_route(RouteFacts {
                    default_changed: true,
                    old_route_available: Some(true),
                    ..Default::default()
                });
            }
            let delay = if callback < 5 {
                30
            } else {
                [11216, 20181, 24352][(callback % 3) as usize]
            };
            h.render(callback * 10, Duration::from_millis(delay));
            let position = h.engine.position().expect("position must remain available");
            let absolute = position.frame
                + if position.entry == EntryId(2) {
                    4410
                } else {
                    0
                };
            assert!(absolute >= previous);
            // 30 ms device lead, one ns only for the observer acknowledgement.
            assert_eq!(absolute, (callback * 10).saturating_sub(30) * 441 / 10);
            previous = absolute;
            if callback < 13 {
                assert_eq!(h.started, [EntryId(1)]);
            } else {
                assert_eq!(h.started, [EntryId(1), EntryId(2)]);
            }
            if callback == 5 {
                assert!(
                    h.engine
                        .take_output_facts()
                        .unwrap()
                        .contains(OutputEvent::ClockUnreliable)
                );
            } else if callback > 5 {
                assert!(
                    h.engine.take_output_facts().is_none(),
                    "one notification per continuous fault"
                );
            }
        }
        assert_eq!(h.engine.heard(EntryId(1)).unwrap().frames, 4410);
        assert_eq!(h.engine.heard(EntryId(2)).unwrap().frames, 2205);
        let frozen = h.engine.freeze_output();
        assert_eq!(frozen.rendered_frames, 6615);
        assert_eq!(h.engine.heard(EntryId(1)).unwrap().frames, 4410);
        assert_eq!(h.engine.heard(EntryId(2)).unwrap().frames, 2205);
    }
}

#[test]
fn pause_freezes_position_and_pending_starts_despite_clock_reset_and_wall_time() {
    let mut h = Harness::new();
    for callback in 0..=11 {
        h.render(
            callback * 10,
            Duration::from_millis(if callback < 5 { 30 } else { 24352 }),
        );
    }
    assert_eq!(h.started, [EntryId(1)]); // B is rendered, but still in device latency.
    h.engine.pause();
    let paused = h.engine.position().unwrap();
    assert_eq!(paused.entry, EntryId(1));
    // Finish the pause fade, then advance twenty seconds with a broken timestamp.
    h.render(120, Duration::from_millis(24352));
    let heard = h.engine.heard(EntryId(2)).unwrap().frames;
    for ms in [1000, 10000, 20000] {
        h.render(ms, Duration::from_millis(24352));
        assert_eq!(h.engine.position(), Some(paused));
        assert_eq!(h.started, [EntryId(1)]);
        assert_eq!(h.engine.heard(EntryId(2)).unwrap().frames, heard);
    }
    h.engine.resume();
    h.at(20_000_000_002);
    assert_eq!(h.started, [EntryId(1), EntryId(2)]);
}

#[test]
fn stable_large_delay_and_degraded_clock_wait_for_device_tail_before_completion() {
    for (first_delay, later_delay, tail_ms, degraded) in [
        (300, 300, 300, false),
        (1000, 1000, 1000, false),
        (3000, 3000, 2000, true),
        (1000, 3000, 2000, true),
        // A jump exactly at the absolute ceiling must still be rejected.
        (30, 2000, 2000, true),
    ] {
        let mut h = Harness::with_successor(false);
        h.render(0, Duration::from_millis(first_delay));
        until(|| h.engine.buffer_status().unwrap().end_of_stream);
        h.shared.test_now_ns.store(10_000_000, Ordering::Release);
        // The remaining 90 ms followed by silence drains the renderer in one call.
        let mut samples = [0.0; 8820];
        h.output.render_with_info(
            SampleBufMut::F32(&mut samples),
            &RenderInfo {
                playback_delay: Some(Duration::from_millis(later_delay)),
            },
        );
        until(|| h.shared.drained.load(Ordering::Acquire) != 0);
        h.at(100_000_001);
        let fault = h
            .engine
            .take_output_facts()
            .is_some_and(|f| f.contains(OutputEvent::ClockUnreliable));
        assert_eq!(fault, degraded);
        let end_ns = (100 + tail_ms) * 1_000_000;
        h.at(end_ns - 1);
        assert_eq!(
            h.ended, 0,
            "premature end at {first_delay}/{later_delay} ms"
        );
        h.at(end_ns + 1);
        assert_eq!(h.ended, 1);
        h.at(end_ns + 2);
        assert_eq!(h.ended, 1);
        assert!(h.output.is_open());
        h.engine.stop();
        assert!(!h.output.is_open());
    }
}
