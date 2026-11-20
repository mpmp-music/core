//! Output recovery with a manually driven renderer and caller-controlled policy time.
use mpmp_test_support::client_parity;

use super::*;
use crate::contract::{ActionId, InsertAt};
use crate::gain;
use mpmp_engine::output::{
    DeviceDiscovery, DeviceId, DeviceInfo, OutputBackend, OutputCaps, OutputError, OutputEvent,
    OutputEventSink, OutputIdentity, OutputStream, Renderer, RouteFacts, SampleType, StreamFormat,
    StreamRequest,
};
use mpmp_engine::{NullOutput, NullOutputHandle};
use mpmp_media_io::{FileResolver, MediaUri, SourceResolver};
use std::path::Path;

#[test]
fn paused_navigation_previous_restarts_after_the_pause_fade() {
    let file = tempfile::NamedTempFile::with_suffix(".wav").unwrap();
    let mut wav = hound::WavWriter::create(
        file.path(),
        hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .unwrap();
    for _ in 0..44_100 * 4 * 2 {
        wav.write_sample(4096_i16).unwrap();
    }
    wav.finalize().unwrap();
    let (mut actor, device, catalog) = actor();
    catalog
        .uris
        .lock()
        .unwrap()
        .insert(TrackId(2), MediaUri::from_path(file.path()).unwrap());
    catalog.durations.lock().unwrap().insert(TrackId(2), 4000);
    let entry = actor
        .playback
        .queue
        .replace(&[TrackId(1), TrackId(2)], None)[1];
    actor.play_entry(entry, 50).unwrap();
    ready(&actor);
    let output = device.lock().unwrap().streams.last().unwrap().1.clone();
    output.render_f32(256);
    let event = actor
        .engine_events
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(matches!(event, EngineEvent::TrackStarted { .. }));
    actor.on_engine(event);
    assert_eq!(actor.current_entry(), Some(entry));
    assert!(actor.pause());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !actor.engine.is_silent() {
        ready(&actor);
        output.render_f32(128);
        assert!(Instant::now() < deadline, "pause fade did not finish");
        std::thread::yield_now();
    }
    let history = catalog.plays.lock().unwrap().len();
    actor.seek_to(entry, 3001).unwrap();
    actor
        .apply_command(Command::Previous, &Origin::Ui, None, None)
        .unwrap();
    assert_eq!(actor.current_entry(), Some(entry));
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert_eq!(actor.snapshot().queue.current_index, Some(1));
    assert_eq!(actor.snapshot().position.unwrap().position_ms, 0);
    assert!(output.render_f32(4096).iter().all(|s| *s == 0.0));
    assert_eq!(catalog.plays.lock().unwrap().len(), history);
    actor
        .apply_command(Command::Previous, &Origin::Ui, None, None)
        .unwrap();
    assert_eq!(
        actor.current_entry(),
        Some(actor.playback.queue.entries()[0].id)
    );
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert_eq!(actor.snapshot().queue.current_index, Some(0));
    assert_eq!(actor.snapshot().position.unwrap().position_ms, 0);
    assert!(
        device
            .lock()
            .unwrap()
            .streams
            .last()
            .unwrap()
            .1
            .render_f32(4096)
            .iter()
            .all(|s| *s == 0.0)
    );
}

#[test]
fn stop_navigation_resumes_from_zero_or_subsequent_seek_and_preserves_heard_history() {
    for paused in [false, true] {
        for seek_ms in [0, 500] {
            let (mut actor, device, catalog) = actor();
            let instance = start(&mut actor, &device);
            let entry = actor.current_entry().unwrap();
            let heard_before = actor.heard_ms(instance);
            assert!(heard_before > 0);
            assert!(actor.position().unwrap().position_ms > 0);
            if paused {
                assert!(actor.pause());
            }
            device.lock().unwrap().close_frames = vec![128, 128];
            assert!(actor.stop(&Origin::Ui));
            let p = actor.snapshot().position.unwrap();
            assert_eq!((p.entry, p.position_ms, p.rate), (entry, 0, 0.0));
            assert_eq!(actor.playback.resume_at, Some((entry, 0)));
            assert_eq!(actor.playback.state, PlayState::Stopped);
            assert!(!actor.engine.is_active());
            let heard = actor.heard_ms(instance);
            assert!(heard >= heard_before);
            let plays = catalog.plays.lock().unwrap();
            assert_eq!(plays.len(), 1);
            assert_eq!(plays[0].ended, EndReason::Stopped);
            assert_eq!(plays[0].played_ms, heard);
            drop(plays);
            if seek_ms != 0 {
                actor.seek_to(entry, seek_ms).unwrap();
                assert!(!actor.stop(&Origin::Ui));
                assert_eq!(actor.snapshot().position.unwrap().position_ms, seek_ms);
            }
            assert!(actor.resume(&Origin::Ui));
            assert_eq!(actor.current_entry(), Some(entry));
            ready(&actor);
            device
                .lock()
                .unwrap()
                .streams
                .last()
                .unwrap()
                .1
                .render_f32(128);
            let p = actor.engine.position().unwrap();
            assert_eq!(p.frame, seek_ms * 44_100 / 1000);
            assert_eq!(catalog.plays.lock().unwrap().len(), 1);
        }
    }
}

#[test]
fn stopped_navigation_never_opens_output_or_resolves_files_and_retires_old_starts() {
    let (mut actor, device, catalog) = actor();
    start(&mut actor, &device);
    let old_next = actor.playback.pending_next.unwrap();
    let old_format = device.lock().unwrap().format.unwrap();
    assert!(actor.stop(&Origin::Ui));
    let opens = device.lock().unwrap().open_attempts;
    let uris = catalog.uri_calls.load(std::sync::atomic::Ordering::Relaxed);
    let history = catalog.plays.lock().unwrap().len();
    for _ in 0..2 {
        actor
            .apply_command(Command::Next, &Origin::MediaControls, None, None)
            .unwrap();
        assert_eq!(actor.playback.state, PlayState::Stopped);
        assert!(actor.playback.playing.is_none() && actor.playback.instances.is_empty());
        assert!(actor.output.output_at.is_none() && actor.playback.pending_next.is_none());
        assert!(!actor.engine.is_active());
        assert_eq!(device.lock().unwrap().open_attempts, opens);
        assert_eq!(
            catalog.uri_calls.load(std::sync::atomic::Ordering::Relaxed),
            uris
        );
        assert_eq!(catalog.plays.lock().unwrap().len(), history);
    }
    let chosen = actor.current_entry().unwrap();
    actor.on_engine(EngineEvent::TrackStarted {
        entry: mpmp_engine::EntryId(old_next),
        signal_path: mpmp_engine::SignalPath::new(
            &mpmp_engine::SourceFormat {
                codec: "flac",
                sample_rate: 44_100,
                channels: 2,
                bits_per_sample: Some(16),
                lossless: true,
                frames: None,
            },
            old_format,
            mpmp_engine::output::ShareMode::Shared,
        ),
    });
    assert_eq!(actor.current_entry(), Some(chosen));
    assert_eq!(actor.playback.resume_at, Some((chosen, 0)));
    assert!(actor.resume(&Origin::Ui));
    assert_eq!(actor.playback.state, PlayState::Playing);
    assert_eq!(device.lock().unwrap().open_attempts, opens + 1);
    ready(&actor);
    device
        .lock()
        .unwrap()
        .streams
        .last()
        .unwrap()
        .1
        .render_f32(128);
    // The first callback publishes its start frame, independent of wall-clock interpolation.
    assert_eq!(actor.engine.position().unwrap().frame, 0);
}

/// A valid WAV header with its declared PCM missing: open succeeds, packet decoding fails.
fn paused_navigation_broken_wav() -> tempfile::NamedTempFile {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_a.wav");
    let mut bytes = std::fs::read(path).unwrap();
    let data = bytes.windows(4).position(|chunk| chunk == b"data").unwrap();
    bytes.truncate(data + 8);
    let file = tempfile::NamedTempFile::with_suffix(".wav").unwrap();
    std::fs::write(file.path(), bytes).unwrap();
    file
}

#[test]
fn paused_navigation_first_seek_rechecks_entry_after_real_queued_decode_failure() {
    for bound in [true, false] {
        let (mut actor, device, catalog) = actor();
        start(&mut actor, &device);
        let c = actor
            .playback
            .queue
            .enqueue(&[TrackId(3)], crate::contract::InsertAt::End, None)
            .unwrap()[0];
        let broken = paused_navigation_broken_wav();
        catalog
            .uris
            .lock()
            .unwrap()
            .insert(TrackId(2), MediaUri::from_path(broken.path()).unwrap());
        catalog
            .durations
            .lock()
            .unwrap()
            .insert(TrackId(2), 120_000);
        actor.pause();
        actor
            .apply_command(Command::Next, &Origin::Ui, None, None)
            .unwrap();
        let b = actor.current_entry().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while actor.engine_events.is_empty() {
            assert!(
                Instant::now() < deadline,
                "real WAV decode failure not queued"
            );
            std::thread::yield_now();
        }
        // Leave the real event in the engine queue for close_output() to reduce.
        assert_eq!(actor.current_entry(), Some(b));
        let (tx, events) = crossbeam_channel::unbounded();
        actor.publication.subscribers.push(Subscriber {
            tx,
            lagging: false,
            ui: false,
            positions: true,
        });
        let error = actor
            .apply_command(
                Command::Seek {
                    entry: bound.then_some(b),
                    position_ms: 60_000,
                },
                &Origin::Ui,
                None,
                None,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::EntryNotCurrent);
        assert_eq!(error.kind, ErrorKind::Conflict);
        assert_eq!(error.details.field.as_deref(), Some("command.entry"));
        assert_eq!(error.details.expected, Some(b.0));
        assert_eq!(error.details.actual, Some(c.0));
        assert_eq!(actor.current_entry(), Some(c));
        assert_eq!(actor.playback.state, PlayState::Paused);
        assert_eq!(actor.position().unwrap().position_ms, 0);
        assert_eq!(actor.output.output_at.unwrap().1, 0);
        assert!(!actor.engine.is_active());
        assert!(events.try_iter().any(
            |e| matches!(e.event, Event::TrackFailed { entry, upcoming: false, .. } if entry == b)
        ));
        // Duration validation applies to the final, unchanged target, never to stale B.
        let error = actor
            .apply_command(
                Command::Seek {
                    entry: Some(c),
                    position_ms: 60_000,
                },
                &Origin::Ui,
                None,
                None,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::OutOfRange);
        assert_eq!(actor.position().unwrap().position_ms, 0);
        actor
            .apply_command(Command::Resume, &Origin::Ui, None, None)
            .unwrap();
        // The Playing display may interpolate; no callback has advanced source frames.
        assert!(
            actor
                .engine
                .position()
                .is_none_or(|position| position.frame == 0)
        );
        assert_eq!(actor.current_entry(), Some(c));
    }
}

#[test]
fn paused_navigation_closed_bad_file_is_decoded_only_on_resume() {
    for successor in [false, true] {
        let (mut actor, device, catalog) = actor();
        start(&mut actor, &device);
        if successor {
            actor
                .playback
                .queue
                .enqueue(&[TrackId(3)], crate::contract::InsertAt::End, None)
                .unwrap();
        }
        let broken = tempfile::NamedTempFile::with_suffix(".flac").unwrap();
        std::fs::write(broken.path(), b"cannot open this decoder").unwrap();
        catalog
            .uris
            .lock()
            .unwrap()
            .insert(TrackId(2), MediaUri::from_path(broken.path()).unwrap());
        actor.output_action(policy::Action::PauseClose);
        let opens = device.lock().unwrap().open_attempts;
        let (tx, events) = crossbeam_channel::unbounded();
        actor.publication.subscribers.push(Subscriber {
            tx,
            lagging: false,
            ui: false,
            positions: true,
        });
        actor
            .apply_command(Command::Next, &Origin::Ui, None, None)
            .unwrap();
        let b = actor.current_entry().unwrap();
        assert_eq!(actor.current_track().unwrap().id, TrackId(2));
        assert_eq!(actor.playback.state, PlayState::Paused);
        assert!(!actor.engine.is_active());
        assert_eq!(device.lock().unwrap().open_attempts, opens);
        assert!(!events.try_iter().any(|e| matches!(
            e.event,
            Event::TrackFailed { .. } | Event::TrackStarted { .. }
        )));
        actor
            .apply_command(Command::Resume, &Origin::Ui, None, None)
            .unwrap();
        let prepared: Vec<_> = events.try_iter().collect();
        assert!(prepared.iter().any(
            |e| matches!(e.event, Event::TrackFailed { entry, upcoming: false, .. } if entry == b)
        ));
        assert!(
            !prepared
                .iter()
                .any(|e| matches!(e.event, Event::TrackStarted { .. }))
        );
        assert_eq!(catalog.plays.lock().unwrap().len(), 1);
        if successor {
            // Only the successfully prepared C may play after the explicit Resume.
            assert_eq!(actor.playback.state, PlayState::Playing);
            assert_eq!(actor.current_track().unwrap().id, TrackId(3));
            assert!(
                actor
                    .engine
                    .position()
                    .is_none_or(|position| position.frame == 0)
            );
            ready(&actor);
            let samples = device
                .lock()
                .unwrap()
                .streams
                .last()
                .unwrap()
                .1
                .render_f32(512);
            assert!(samples.iter().any(|sample| *sample != 0.0));
            let event = actor
                .engine_events
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            let c = actor.playback.playing.as_ref().unwrap().instance;
            assert!(matches!(event, EngineEvent::TrackStarted { entry, .. } if entry.0 == c));
            actor.on_engine(event);
            assert!(
                events
                    .try_iter()
                    .all(|e| !matches!(e.event, Event::TrackStarted { entry, .. } if entry == b))
            );
        } else {
            assert_eq!(actor.playback.state, PlayState::Stopped);
            assert!(!actor.engine.is_active());
            assert_eq!(device.lock().unwrap().open_attempts, opens);
        }
    }
}

#[test]
fn paused_navigation_follow_format_rebuild_and_retry_keep_selection_paused() {
    for closed in [false, true] {
        for format_change in [false, true] {
            for fail_once in [false, true] {
                let (mut actor, device, catalog) = actor();
                start(&mut actor, &device);
                if closed {
                    actor.output_action(policy::Action::PauseClose);
                } else {
                    actor.pause();
                }
                actor
                    .apply_command(Command::Next, &Origin::Ui, None, None)
                    .unwrap();
                let selected = actor.current_entry().unwrap();
                {
                    let mut d = device.lock().unwrap();
                    if format_change {
                        d.format = Some(StreamFormat {
                            sample_rate: 48_000,
                            ..format()
                        });
                    }
                    d.fail_open = fail_once;
                    d.keep_caps = true;
                }
                actor.output_action(if format_change {
                    policy::Action::CheckFormat
                } else {
                    policy::Action::Follow
                });
                assert_eq!(actor.playback.state, PlayState::Paused);
                if fail_once {
                    assert!(!actor.engine.is_active());
                    device.lock().unwrap().fail_open = false;
                    output_timer(&mut actor);
                }
                assert!(actor.engine.is_active());
                assert!(actor.engine.is_paused());
                assert_eq!(actor.playback.state, PlayState::Paused);
                assert_eq!(actor.current_entry(), Some(selected));
                assert_eq!(actor.position().unwrap().position_ms, 0);
                assert!(actor.playback.pending_next.is_none());
                ready(&actor);
                for _ in 0..3 {
                    assert!(
                        device
                            .lock()
                            .unwrap()
                            .streams
                            .last()
                            .unwrap()
                            .1
                            .render_f32(4096)
                            .iter()
                            .all(|s| *s == 0.0)
                    );
                }
                assert!(actor.engine_events.is_empty());
                actor.stop(&Origin::Ui);
                assert_eq!(catalog.plays.lock().unwrap().len(), 1);
            }
        }
    }
}

#[test]
fn paused_navigation_accounts_exact_old_track_tail_during_close() {
    let (mut actor, device, catalog) = actor();
    let first = start(&mut actor, &device);
    device
        .lock()
        .unwrap()
        .streams
        .last()
        .unwrap()
        .1
        .render_f32(512);
    assert_eq!(actor.heard_ms(first), 768 * 1000 / 44_100);
    actor.pause();
    // Leave the fade unfinished: these real callbacks run inside output destruction.
    device.lock().unwrap().close_frames = vec![128, 128];
    actor
        .apply_command(Command::Next, &Origin::Ui, None, None)
        .unwrap();
    assert_eq!(actor.heard_ms(first), 1024 * 1000 / 44_100);
    let d = device.lock().unwrap();
    assert_eq!(d.close_samples.len(), 2);
    assert!(
        d.close_samples
            .iter()
            .all(|samples| samples.iter().any(|s| *s != 0.0))
    );
    drop(d);
    let plays = catalog.plays.lock().unwrap();
    assert_eq!(plays.len(), 1);
    assert_eq!(plays[0].track, TrackId(1));
    assert_eq!(plays[0].ended, EndReason::Skipped);
    assert_eq!(plays[0].played_ms, 1024 * 1000 / 44_100);
    drop(plays);
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert_eq!(actor.position().unwrap().position_ms, 0);
    actor.stop(&Origin::Ui);
    assert_eq!(catalog.plays.lock().unwrap().len(), 1);
}

#[test]
fn paused_navigation_async_failure_keeps_successor_silent_and_out_of_history() {
    for frozen in [false, true] {
        let (mut actor, device, catalog) = actor();
        start(&mut actor, &device);
        // Keep a third candidate after B, without replacing the active entries.
        actor
            .playback
            .queue
            .enqueue(&[TrackId(3)], crate::contract::InsertAt::End, None)
            .unwrap();
        actor.pause();
        actor
            .apply_command(Command::Next, &Origin::Ui, None, None)
            .unwrap();
        let selected = actor.playback.playing.as_ref().unwrap().instance;
        let failure = EngineEvent::TrackFailed {
            entry: mpmp_engine::EntryId(selected),
            reason: "decoder failed after opening".into(),
        };
        if frozen {
            let mut batch = actor.engine.freeze_output();
            batch.events.push(failure);
            actor.reduce_frozen(batch, PlayState::Paused);
        } else {
            actor.on_engine(failure);
        }
        assert_eq!(actor.playback.state, PlayState::Paused);
        assert_eq!(actor.current_track().unwrap().id, TrackId(3));
        assert_eq!(actor.position().unwrap().position_ms, 0);
        assert!(actor.playback.pending_next.is_none());
        if actor.engine.is_active() {
            assert!(
                device
                    .lock()
                    .unwrap()
                    .streams
                    .last()
                    .unwrap()
                    .1
                    .render_f32(4096)
                    .iter()
                    .all(|v| *v == 0.0)
            );
        }
        actor.stop(&Origin::Ui);
        assert_eq!(catalog.plays.lock().unwrap().len(), 1);
    }
}

#[test]
fn paused_navigation_closed_output_seek_resume_and_late_prefetch() {
    for seek_ms in [0, 500] {
        let (mut actor, device, catalog) = actor();
        start(&mut actor, &device);
        let old_next = actor.playback.pending_next.unwrap();
        let old = actor.playback.instances[&old_next].clone();
        actor.output_action(policy::Action::PauseClose);
        assert!(!actor.engine.is_active());
        let opens = device.lock().unwrap().open_attempts;
        actor
            .apply_command(Command::Next, &Origin::MediaControls, None, None)
            .unwrap();
        let chosen = actor.playback.playing.as_ref().unwrap().instance;
        let entry = actor.current_entry().unwrap();
        assert_ne!(chosen, old_next);
        assert_eq!(actor.playback.state, PlayState::Paused);
        assert_eq!(actor.snapshot().position.unwrap().position_ms, 0);
        assert_eq!(actor.snapshot().queue.current_index, Some(1));
        assert!(actor.snapshot().current.unwrap().signal_path.is_none());
        assert_eq!(actor.output.output_at.unwrap().0, chosen);
        assert!(!actor.engine.is_active());
        assert!(actor.playback.pending_next.is_none());
        assert_eq!(device.lock().unwrap().open_attempts, opens);
        // A held event from the retired gapless session cannot select its old instance.
        actor.on_engine(EngineEvent::TrackStarted {
            entry: mpmp_engine::EntryId(old_next),
            signal_path: mpmp_engine::SignalPath::new(
                &mpmp_engine::SourceFormat {
                    codec: "flac",
                    sample_rate: 44_100,
                    channels: 2,
                    bits_per_sample: Some(16),
                    lossless: true,
                    frames: None,
                },
                device.lock().unwrap().format.unwrap(),
                mpmp_engine::output::ShareMode::Shared,
            ),
        });
        assert_eq!(actor.playback.playing.as_ref().unwrap().instance, chosen);
        assert_eq!(actor.current_track().unwrap().id, old.track.id);
        actor.seek_to(entry, seek_ms).unwrap();
        assert_eq!(actor.position().unwrap().position_ms, seek_ms);
        assert_eq!(device.lock().unwrap().open_attempts, opens);
        assert!(actor.resume(&Origin::Ui));
        assert_eq!(actor.playback.state, PlayState::Playing);
        assert!(!actor.engine.is_paused());
        assert_eq!(device.lock().unwrap().open_attempts, opens + 1);
        assert_eq!(catalog.plays.lock().unwrap().len(), 1);
        ready(&actor);
        device
            .lock()
            .unwrap()
            .streams
            .last()
            .unwrap()
            .1
            .render_f32(256);
        let event = actor
            .engine_events
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(matches!(event, EngineEvent::TrackStarted { entry, .. } if entry.0 == chosen));
        actor.on_engine(event);
        assert!(actor.playback.instances[&chosen].start_announced);
    }
}

// Exercise the shared production retry combinator.
use mpmp_engine::output::retry;
mod engine_retry;

#[derive(Default)]
struct Activation {
    default_caps_fail: bool,
    default_open_fail: bool,
    endpoint_fail: bool,
    fresh_success: bool,
    prepared_concrete: bool,
    pinned_stream: bool,
    calls: Vec<&'static str>,
}

impl Activation {
    fn attempt(&mut self, caps: bool) -> Result<bool, OutputError> {
        self.calls
            .push(if caps { "default caps" } else { "default open" });
        let primary = if if caps {
            self.default_caps_fail
        } else {
            self.default_open_fail
        } {
            Err(OutputError::Backend(
                "default async activation failed".into(),
            ))
        } else {
            Ok(false)
        };
        let primary = retry::retry_on_fresh(
            primary,
            || {
                self.calls
                    .push(if caps { "fresh caps" } else { "fresh open" });
                if self.fresh_success {
                    Ok(false)
                } else {
                    Err(OutputError::Backend("fresh activation failed".into()))
                }
            },
            if caps { "capabilities" } else { "open" },
        );
        retry::retry_on_endpoint(
            primary,
            || Ok(((), "endpoint-A".into())),
            |_| self.endpoint(caps),
            if caps { "capabilities" } else { "open" },
        )
    }

    fn endpoint(&mut self, caps: bool) -> Result<bool, OutputError> {
        self.calls.push(if caps {
            "endpoint caps"
        } else {
            "endpoint open"
        });
        if self.endpoint_fail {
            Err(OutputError::NoDevice)
        } else {
            Ok(true)
        }
    }
}

#[derive(Default)]
struct CatalogStub {
    before_tracks: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    plays: Mutex<Vec<PlayRecord>>,
    uri_calls: std::sync::atomic::AtomicUsize,
    short_first: std::sync::atomic::AtomicBool,
    panic_drain: std::sync::atomic::AtomicBool,
    root_undo: std::sync::atomic::AtomicBool,
    uris: Mutex<HashMap<TrackId, MediaUri>>,
    durations: Mutex<HashMap<TrackId, u64>>,
    context: Mutex<Vec<TrackId>>,
}

impl crate::catalog::Browse for CatalogStub {
    fn tracks(&self, ids: &[TrackId]) -> Result<Vec<TrackInfo>, ContractError> {
        let hook = self.before_tracks.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        Ok(ids
            .iter()
            .map(|id| TrackInfo {
                id: *id,
                title: format!("song {}", id.0),
                available: true,
                sample_rate: Some(44_100),
                duration_ms: Some(*self.durations.lock().unwrap().get(id).unwrap_or(&2000)),
                ..Default::default()
            })
            .collect())
    }

    fn frame_range(&self, id: TrackId) -> Result<Option<mpmp_engine::FrameRange>, ContractError> {
        Ok(
            (id == TrackId(1) && self.short_first.load(std::sync::atomic::Ordering::Acquire))
                .then_some(mpmp_engine::FrameRange {
                    start: 0,
                    end: Some(4096),
                }),
        )
    }

    fn uri_of(&self, id: TrackId) -> Result<Option<MediaUri>, ContractError> {
        self.uri_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(uri) = self.uris.lock().unwrap().get(&id) {
            return Ok(Some(uri.clone()));
        }
        Ok(Some(
            MediaUri::from_path(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../engine/tests/fixtures/sine44_a.flac"),
            )
            .unwrap(),
        ))
    }

    fn context_tracks(&self, _: &PlayContext) -> Result<Vec<TrackId>, ContractError> {
        Ok(self.context.lock().unwrap().clone())
    }

    fn query(&self, _: &crate::contract::Query) -> crate::contract::QueryResult {
        crate::contract::QueryResult::Unknown
    }
}

impl crate::catalog::Playlists for CatalogStub {}

impl crate::catalog::LibraryRoots for CatalogStub {
    fn root_location(&self, root: RootId) -> Result<Option<String>, ContractError> {
        Ok(self
            .root_undo
            .load(std::sync::atomic::Ordering::SeqCst)
            .then(|| format!("/root/{}", root.0)))
    }

    fn wait_root_idle(&self, _: RootId, _: Duration) -> Result<bool, ContractError> {
        assert!(
            !self.panic_drain.load(std::sync::atomic::Ordering::SeqCst),
            "injected drain panic"
        );
        Ok(true)
    }
}

impl crate::catalog::Actions for CatalogStub {
    fn action_roots(&self, _: ActionId) -> Result<Vec<RootId>, ContractError> {
        Ok(
            if self.root_undo.load(std::sync::atomic::Ordering::SeqCst) {
                vec![RootId(1), RootId(2)]
            } else {
                vec![]
            },
        )
    }

    fn undo_root_command(&self, _: ActionId) -> Result<Option<&'static str>, ContractError> {
        Ok(self
            .root_undo
            .load(std::sync::atomic::Ordering::SeqCst)
            .then_some("remove_root"))
    }

    fn undo_action(
        &self,
        _: ActionId,
        _: &Origin,
        _: Option<GrantId>,
    ) -> Result<crate::catalog::Undone, ContractError> {
        Ok(crate::catalog::Undone {
            action: ActionId(2),
            tracks: vec![],
            playlists: vec![],
            roots: vec![RootId(1), RootId(2)],
            rescan: false,
        })
    }
}

impl crate::catalog::CatalogChecks for CatalogStub {
    fn storage_write_capabilities(&self) -> mpmp_storage::WriteCapabilities {
        mpmp_storage::WriteCapabilities {
            durable_replace: true,
            legacy_tag_write: true,
        }
    }
}

impl crate::catalog::AgentPermissions for CatalogStub {}

impl crate::catalog::MediaResources for CatalogStub {}

impl crate::catalog::Loudness for CatalogStub {}

impl crate::catalog::TagsAndCovers for CatalogStub {}

impl crate::catalog::PlaybackHistory for CatalogStub {
    fn record_play(&self, record: PlayRecord) {
        self.plays.lock().unwrap().push(record);
    }
}

struct Device {
    discovery_cancel_hook:
        Option<Arc<dyn Fn(mpmp_engine::output_task::Cancellation) + Send + Sync>>,
    capability_hook: Option<Arc<dyn Fn(mpmp_engine::output_task::Cancellation) + Send + Sync>>,
    discovery_failures: usize,
    committed_devices: Vec<DeviceInfo>,
    discovery_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    discovery_calls: usize,
    native_notifications: bool,
    identities: Vec<Option<OutputIdentity>>,
    devices: Option<Vec<DeviceInfo>>,
    selections: Vec<Option<DeviceId>>,
    before_open: Option<Box<dyn FnOnce() + Send>>,
    close_frames: Vec<usize>,
    close_samples: Vec<Vec<f32>>,
    format: Option<StreamFormat>,
    streams: Vec<(OutputEventSink, NullOutputHandle)>,
    watcher: Option<OutputEventSink>,
    fail_open: bool,
    keep_caps: bool,
    open_attempts: usize,
    loss_on_close: bool,
    loss_on_open: bool,
    format_failures: usize,
    format_queries: usize,
    activation: Option<Activation>,
}

struct Backend(Arc<Mutex<Device>>);

impl OutputBackend for Backend {
    fn watch_output(&mut self, sink: OutputEventSink) -> Result<(), OutputError> {
        self.0.lock().unwrap().watcher = Some(sink);
        Ok(())
    }
    fn devices(&self) -> Result<Vec<DeviceInfo>, OutputError> {
        Ok(self.0.lock().unwrap().devices.clone().unwrap_or_default())
    }
    fn commit_discovery(&mut self, result: mpmp_engine::output::DiscoveredDevices) {
        self.0.lock().unwrap().committed_devices = *result
            .handles
            .unwrap()
            .downcast::<Vec<DeviceInfo>>()
            .unwrap();
    }
    fn device_discovery(&self) -> Option<DeviceDiscovery> {
        let device = Arc::downgrade(&self.0);
        Some(Arc::new(move |cancel| {
            let (snapshot, hook, cancel_hook, fail) = {
                let device = device.upgrade().ok_or(OutputError::NoDevice)?;
                let mut device = device.lock().unwrap();
                device.discovery_calls += 1;
                let fail = device.discovery_failures > 0;
                device.discovery_failures = device.discovery_failures.saturating_sub(1);
                (
                    device.devices.clone().unwrap_or_default(),
                    device.discovery_hook.clone(),
                    device.discovery_cancel_hook.clone(),
                    fail,
                )
            };
            if let Some(hook) = hook {
                hook();
            }
            if let Some(hook) = cancel_hook {
                hook(cancel.clone());
            }
            cancel.check()?;
            if fail {
                return Err(OutputError::Backend("injected discovery error".into()));
            }
            Ok(mpmp_engine::output::DiscoveredDevices {
                handles: Some(Box::new(snapshot.clone())),
                devices: snapshot,
            })
        }))
    }
    fn capability_query(
        &self,
        selection: Option<&DeviceId>,
    ) -> Result<mpmp_engine::output::CapabilityQuery, OutputError> {
        let (format, hook) = {
            let mut device = self.0.lock().unwrap();
            if let Some(devices) = &device.devices
                && !devices
                    .iter()
                    .any(|d| selection.map_or(d.is_default, |id| *id == d.id))
            {
                return Err(OutputError::NoDevice);
            }
            device.format_queries += 1;
            (
                device.format.ok_or(OutputError::NoDevice)?,
                device.capability_hook.clone(),
            )
        };
        Ok(Box::new(move |cancel| {
            if let Some(hook) = hook {
                hook(cancel.clone());
            }
            cancel.check()?;
            Ok(OutputCaps {
                mix_format: format,
                supports_exclusive: false,
            })
        }))
    }
    fn has_native_notifications(&self) -> bool {
        self.0.lock().unwrap().native_notifications
    }
    fn capabilities(&self, selection: Option<&DeviceId>) -> Result<OutputCaps, OutputError> {
        let mut device = self.0.lock().unwrap();
        device.format_queries += 1;
        if let Some(devices) = &device.devices
            && !devices
                .iter()
                .any(|d| selection.map_or(d.is_default, |id| *id == d.id))
        {
            return Err(OutputError::NoDevice);
        }
        if let Some(activation) = &mut device.activation {
            activation.prepared_concrete = activation.attempt(true)?;
        }
        if device.format_failures > 0 {
            device.format_failures -= 1;
            return Err(OutputError::Backend("injected format query failure".into()));
        }
        Ok(OutputCaps {
            mix_format: device.format.ok_or(OutputError::NoDevice)?,
            supports_exclusive: false,
        })
    }
    fn open(
        &mut self,
        request: StreamRequest,
        renderer: Box<dyn Renderer>,
    ) -> Result<Box<dyn OutputStream>, OutputError> {
        let hook = self.0.lock().unwrap().before_open.take();
        if let Some(hook) = hook {
            hook();
        }
        {
            let mut device = self.0.lock().unwrap();
            device.open_attempts += 1;
            device.selections.push(request.device.clone());
            if let Some(activation) = &mut device.activation {
                activation.pinned_stream = if activation.prepared_concrete {
                    activation.endpoint(false)?
                } else {
                    activation.attempt(false)?
                };
            }
            if device.fail_open {
                if !device.keep_caps {
                    device.format = None;
                }
                return Err(OutputError::NoDevice);
            }
            if std::mem::take(&mut device.loss_on_open) {
                device.watcher.as_ref().unwrap().report_route(RouteFacts {
                    headphones_disconnected: true,
                    ..Default::default()
                });
            }
        }
        let identity = self
            .0
            .lock()
            .unwrap()
            .devices
            .as_ref()
            .and_then(|devices| {
                devices.iter().find(|d| {
                    request
                        .device
                        .as_ref()
                        .map_or(d.is_default, |id| *id == d.id)
                })
            })
            .cloned()
            .map(OutputIdentity::new);
        self.0.lock().unwrap().identities.push(identity.clone());
        let mut output = NullOutput::new(request.format);
        let handle = output.handle();
        self.0
            .lock()
            .unwrap()
            .streams
            .push((request.events.clone(), output.handle()));
        let events = request.events.clone();
        Ok(Box::new(ClosingStream {
            inner: output.open(request, renderer)?,
            identity,
            output: handle,
            device: Arc::clone(&self.0),
            events,
        }))
    }
}

struct ClosingStream {
    identity: Option<OutputIdentity>,
    inner: Box<dyn OutputStream>,
    output: NullOutputHandle,
    device: Arc<Mutex<Device>>,
    events: OutputEventSink,
}

impl OutputStream for ClosingStream {
    fn device_info(&self) -> Option<DeviceInfo> {
        self.identity.as_ref().and_then(OutputIdentity::get)
    }
    fn format(&self) -> StreamFormat {
        self.inner.format()
    }
}

impl Drop for ClosingStream {
    fn drop(&mut self) {
        let frames = std::mem::take(&mut self.device.lock().unwrap().close_frames);
        for frames in frames {
            let samples = self.output.render_f32(frames);
            self.device.lock().unwrap().close_samples.push(samples);
        }
        if std::mem::take(&mut self.device.lock().unwrap().loss_on_close) {
            self.events.report(OutputEvent::DeviceUnplugged);
        }
    }
}

fn format() -> StreamFormat {
    StreamFormat {
        sample_rate: 44_100,
        channels: 2,
        sample_type: SampleType::F32,
    }
}

fn actor() -> (Actor, Arc<Mutex<Device>>, Arc<CatalogStub>) {
    actor_with_source(Arc::new(FileResolver))
}

fn actor_with_source(
    source: Arc<dyn SourceResolver>,
) -> (Actor, Arc<Mutex<Device>>, Arc<CatalogStub>) {
    actor_with_clock(
        source,
        Arc::new(mpmp_engine::SystemSuspendClock::new().unwrap()),
    )
}

fn actor_with_clock(
    source: Arc<dyn SourceResolver>,
    playback_clock: Arc<dyn mpmp_engine::SuspendClock>,
) -> (Actor, Arc<Mutex<Device>>, Arc<CatalogStub>) {
    let device = Arc::new(Mutex::new(Device {
        discovery_cancel_hook: None,
        capability_hook: None,
        discovery_failures: 0,
        committed_devices: Vec::new(),
        discovery_hook: None,
        discovery_calls: 0,
        native_notifications: false,
        identities: Vec::new(),
        devices: None,
        selections: Vec::new(),
        before_open: None,
        close_frames: Vec::new(),
        close_samples: Vec::new(),
        format: Some(format()),
        streams: Vec::new(),
        watcher: None,
        fail_open: false,
        keep_caps: false,
        open_attempts: 0,
        loss_on_close: false,
        loss_on_open: false,
        format_failures: 0,
        format_queries: 0,
        activation: None,
    }));
    let catalog = Arc::new(CatalogStub::default());
    let catalog_trait: Arc<dyn Catalog> = catalog.clone();
    let (tx, rx) = crossbeam_channel::unbounded();
    let analysis = Arc::new(crate::loudness_scan::Analysis::new(Duration::from_secs(1)));
    let waveforms = WaveformWorker::start(
        source.clone(),
        catalog_trait.clone(),
        tx.clone(),
        analysis.clone(),
        false,
    )
    .unwrap();
    let loudness = crate::loudness_scan::LoudnessWorker::new(
        source.clone(),
        catalog_trait.clone(),
        tx.clone(),
        analysis,
    );
    let mut engine = Engine::new(
        Box::new(Backend(device.clone())),
        source,
        Default::default(),
    );
    let playback_deadline = Arc::new(mpmp_engine::PlaybackDeadline::new(playback_clock.clone()));
    engine.set_playback_deadline(playback_deadline.clone());
    let mut actor = Actor::new(
        rx,
        tx,
        engine,
        Parts {
            job_history: Arc::default(),
            root_access: Arc::default(),
            catalog: catalog_trait,
            published: Arc::new(ArcSwap::from_pointee(Published {
                snapshot: empty_snapshot(mpmp_storage::WriteCapabilities {
                    durable_replace: true,
                    legacy_tag_write: true,
                }),
                queue: Arc::default(),
            })),
            state_file: None,
            permissions: Arc::new(Mutex::new(Permissions::new(Default::default(), 0).0)),
            locations: Arc::new(mpmp_fs_desktop::DesktopLocations),
            agent_fs: Limits::new(Duration::from_secs(1), 4),
            confirmation_ttl: Duration::from_secs(1),
            problems: Vec::new(),
            waveforms,
            loudness,
            automatic_loudness: false,
            playback_clock,
            playback_deadline,
        },
        Instant::now(),
    );
    finish_discovery(&mut actor);
    (actor, device, catalog)
}

fn ready(actor: &Actor) {
    // A condition gate, not a delay: the timeout only diagnoses a hung decoder.
    let deadline = Instant::now() + Duration::from_secs(5);
    while actor.engine.buffer_status().unwrap().buffered_frames < 4096
        && !actor.engine.buffer_status().unwrap().end_of_stream
    {
        assert!(Instant::now() < deadline, "decoder did not fill the ring");
        std::thread::yield_now();
    }
}

fn start(actor: &mut Actor, device: &Mutex<Device>) -> u64 {
    let entry = actor
        .playback
        .queue
        .replace(&[TrackId(1), TrackId(2)], None)[0];
    actor.play_entry(entry, 50).unwrap();
    ready(actor);
    device
        .lock()
        .unwrap()
        .streams
        .last()
        .unwrap()
        .1
        .render_f32(256);
    let event = actor
        .engine_events
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    actor.on_engine(event);
    actor.playback.playing.as_ref().unwrap().instance
}

fn sink(device: &Mutex<Device>) -> OutputEventSink {
    device.lock().unwrap().streams.last().unwrap().0.clone()
}

fn switch() -> RouteFacts {
    RouteFacts {
        route: Some(DeviceId("new".into())),
        default_changed: true,
        old_route_available: Some(true),
        ..Default::default()
    }
}

/// Advance the actor's monotonic clock to its next policy deadline, without sleep.
fn output_timer(actor: &mut Actor) {
    let due = actor
        .output
        .output_policy
        .next_due()
        .expect("output timer missing");
    actor.epoch -= Duration::from_millis(due.saturating_sub(actor.now_ms()));
    actor.poll_output();
}

#[test]
fn playback_checkpoints_position_periodically_without_dirty_or_shutdown() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("state.json");
    let (mut actor, _, _) = actor();
    actor.persistence.state_file = Some(StateFile::new(&path));
    let entry = actor
        .playback
        .queue
        .replace(&vec![TrackId(1); 100_000], None)[0];
    actor.play_entry(entry, 0).unwrap();
    actor.save_now();
    actor.persistence.state_file.as_mut().unwrap().finish();
    let original = std::fs::read(&path).unwrap();
    let instance = actor.playback.playing.as_ref().unwrap().instance;
    actor.playback.seek_hint.set(Some(SeekHint {
        instance,
        position_ms: 42_000,
        at_ms: actor.now_ms(),
        running: false,
    }));
    actor.persistence.last_save = Instant::now() - (SAVE_POSITION_EVERY - Duration::from_secs(1));
    let before = actor.persistence.last_save;
    assert!(!actor.persistence.dirty);
    actor.on_tick();
    assert_eq!(actor.persistence.last_save, before);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    actor.persistence.last_save = Instant::now() - SAVE_POSITION_EVERY;
    actor.on_tick();
    actor.persistence.state_file.as_mut().unwrap().finish();
    let saved = StateFile::new(&path).load();
    assert_eq!(saved.queue.position_ms, 42_000);
    assert_eq!(saved.queue.entries.len(), 100_000);
    // A fresh core restores the checkpoint while the original has not shut down.
    let (mut restored, _, _) = self::actor();
    restored.persistence.state_file = Some(StateFile::new(&path));
    restored.restore(1_000);
    assert_eq!(restored.playback.resume_at.unwrap().1, 42_000);

    assert!(actor.pause());
    actor.persistence.state_file.as_mut().unwrap().finish();
    actor.persistence.last_save = Instant::now() - Duration::from_secs(60);
    let paused_save = actor.persistence.last_save;
    assert!(!actor.pause());
    actor.on_tick();
    assert_eq!(actor.persistence.last_save, paused_save);
    assert!(actor.stop(&Origin::Ui));
    actor.persistence.state_file.as_mut().unwrap().finish();
    actor.persistence.last_save = Instant::now() - Duration::from_secs(60);
    let stopped_save = actor.persistence.last_save;
    assert!(!actor.stop(&Origin::Ui));
    actor.on_tick();
    assert_eq!(actor.persistence.last_save, stopped_save);
}

fn observe_switch(actor: &mut Actor, device: &Mutex<Device>, alive: bool) {
    let events = sink(device);
    let mut facts = switch();
    facts.rebuild_required = device
        .lock()
        .unwrap()
        .activation
        .as_ref()
        .is_some_and(|a| a.pinned_stream);
    events.report_route(facts);
    actor.on_output();
    events.report(OutputEvent::StreamInvalidated);
    actor.on_output();
    output_timer(actor); // Sample the beginning of the final 100 ms.
    if alive {
        ready(actor);
        device
            .lock()
            .unwrap()
            .streams
            .last()
            .unwrap()
            .1
            .render_f32(256);
    }
    output_timer(actor); // Finish the 300 ms observation.
}

#[test]
fn default_activation_failure_falls_back_and_live_same_format_switch_rebuilds() {
    for cap_failure in [false, true] {
        for paused in [false, true] {
            let (mut actor, device, catalog) = actor();
            let instance = start(&mut actor, &device);
            if paused {
                actor.pause();
            }
            device.lock().unwrap().activation = Some(Activation {
                default_caps_fail: cap_failure,
                default_open_fail: !cap_failure,
                ..Default::default()
            });
            actor.output_action(policy::Action::Follow);
            assert_eq!(
                actor.playback.state,
                if paused {
                    PlayState::Paused
                } else {
                    PlayState::Playing
                }
            );
            assert_eq!(actor.engine.is_paused(), paused);
            assert!(actor.engine.is_active());
            assert!(
                device
                    .lock()
                    .unwrap()
                    .activation
                    .as_ref()
                    .unwrap()
                    .pinned_stream
            );
            let expected = if cap_failure {
                vec![
                    "default caps",
                    "fresh caps",
                    "endpoint caps",
                    "endpoint open",
                ]
            } else {
                vec![
                    "default caps",
                    "default open",
                    "fresh open",
                    "endpoint open",
                ]
            };
            assert_eq!(
                device.lock().unwrap().activation.as_ref().unwrap().calls,
                expected
            );
            let generation = actor.engine.output_generation();
            // Default handle recovered; the next rebuild should use it, even though
            // the current endpoint-bound stream is healthy and the format unchanged.
            {
                let mut d = device.lock().unwrap();
                let a = d.activation.as_mut().unwrap();
                a.default_caps_fail = false;
                a.default_open_fail = false;
                a.calls.clear();
            }
            observe_switch(&mut actor, &device, true);
            assert_eq!(actor.engine.output_generation(), generation + 1);
            assert_eq!(
                device.lock().unwrap().activation.as_ref().unwrap().calls,
                ["default caps", "default open"]
            );
            assert!(
                !device
                    .lock()
                    .unwrap()
                    .activation
                    .as_ref()
                    .unwrap()
                    .pinned_stream
            );
            assert_eq!(actor.playback.playing.as_ref().unwrap().instance, instance);
            assert_eq!(
                actor.playback.state,
                if paused {
                    PlayState::Paused
                } else {
                    PlayState::Playing
                }
            );
            assert!(catalog.plays.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn both_activation_paths_share_finite_retries_and_resume_retries_after_exhaustion() {
    for cap_failure in [false, true] {
        let attempt = if cap_failure {
            vec!["default caps", "fresh caps", "endpoint caps"]
        } else {
            vec![
                "default caps",
                "default open",
                "fresh open",
                "endpoint open",
            ]
        };
        for paused in [false, true] {
            for origin in [Origin::Ui, Origin::MediaControls] {
                let (mut actor, device, _) = actor();
                let instance = start(&mut actor, &device);
                if paused {
                    actor.pause();
                }
                device.lock().unwrap().activation = Some(Activation {
                    default_caps_fail: cap_failure,
                    default_open_fail: !cap_failure,
                    endpoint_fail: true,
                    ..Default::default()
                });
                actor.output_action(policy::Action::Follow);
                for _ in 0..3 {
                    output_timer(&mut actor);
                }
                assert!(actor.output.output_policy.next_due().is_some());
                assert!(!actor.engine.is_active());
                assert_eq!(
                    device.lock().unwrap().activation.as_ref().unwrap().calls,
                    attempt.repeat(4)
                );
                // Even an unsuccessful user retry is one new bounded attempt.
                assert!(!actor.resume(&origin));
                assert!(actor.output.output_policy.next_due().is_some());
                assert_eq!(
                    device.lock().unwrap().activation.as_ref().unwrap().calls,
                    attempt.repeat(5)
                );
                {
                    let mut d = device.lock().unwrap();
                    let a = d.activation.as_mut().unwrap();
                    a.endpoint_fail = false;
                    a.calls.clear();
                }
                assert!(actor.resume(&origin));
                assert_eq!(
                    device.lock().unwrap().activation.as_ref().unwrap().calls,
                    if cap_failure {
                        vec![
                            "default caps",
                            "fresh caps",
                            "endpoint caps",
                            "endpoint open",
                        ]
                    } else {
                        vec![
                            "default caps",
                            "default open",
                            "fresh open",
                            "endpoint open",
                        ]
                    }
                );
                assert!(actor.engine.is_active());
                assert_eq!(actor.playback.state, PlayState::Playing);
                assert_eq!(actor.playback.playing.as_ref().unwrap().instance, instance);
            }
        }
    }
}

#[test]
fn genuine_device_change_retries_exhausted_fallback_without_resuming_privacy_pause() {
    let (mut actor, device, _) = actor();
    start(&mut actor, &device);
    device.lock().unwrap().activation = Some(Activation {
        default_caps_fail: true,
        endpoint_fail: true,
        ..Default::default()
    });
    actor.output_action(policy::Action::PauseRebuild);
    for _ in 0..3 {
        output_timer(&mut actor);
    }
    for _ in 0..20 {
        output_timer(&mut actor);
    }
    assert!(actor.output.output_policy.next_due().is_none());
    assert_eq!(actor.playback.state, PlayState::Paused);
    {
        let mut d = device.lock().unwrap();
        d.activation.as_mut().unwrap().endpoint_fail = false;
        d.watcher.as_ref().unwrap().report_route(switch());
    }
    actor.on_output();
    assert!(actor.engine.is_active());
    assert!(
        device
            .lock()
            .unwrap()
            .activation
            .as_ref()
            .unwrap()
            .pinned_stream
    );
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(actor.engine.is_paused());
}

#[test]
fn fallback_resume_cannot_override_disconnect_latched_during_endpoint_open() {
    let (mut actor, device, _) = actor();
    start(&mut actor, &device);
    actor.output_action(policy::Action::PauseClose);
    {
        let mut d = device.lock().unwrap();
        d.activation = Some(Activation {
            default_open_fail: true,
            ..Default::default()
        });
        d.loss_on_open = true;
    }
    assert!(!actor.resume(&Origin::MediaControls));
    assert!(
        device
            .lock()
            .unwrap()
            .activation
            .as_ref()
            .unwrap()
            .pinned_stream
    );
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(actor.engine.is_paused());
    ready(&actor);
    assert!(
        device
            .lock()
            .unwrap()
            .streams
            .last()
            .unwrap()
            .1
            .render_f32(512)
            .iter()
            .all(|sample| *sample == 0.0)
    );
}

#[test]
fn format_property_rebuilds_updates_signal_path_and_preserves_paused_position() {
    for paused in [false, true] {
        let (mut actor, device, catalog) = actor();
        let instance = start(&mut actor, &device);
        if paused {
            actor.pause();
        }
        let position = actor.position().unwrap().position_ms;
        let heard = actor.heard_ms(instance);
        let generation = actor.engine.output_generation();
        device.lock().unwrap().format = Some(StreamFormat {
            sample_rate: 48000,
            ..format()
        });
        device
            .lock()
            .unwrap()
            .watcher
            .as_ref()
            .unwrap()
            .report_route(RouteFacts {
                format_changed: true,
                ..Default::default()
            });
        sink(&device).report(OutputEvent::StreamInvalidated);
        actor.on_output();
        assert_eq!(actor.engine.output_generation(), generation + 1);
        assert_eq!(
            actor.playback.state,
            if paused {
                PlayState::Paused
            } else {
                PlayState::Playing
            }
        );
        assert_eq!(actor.engine.is_paused(), paused);
        assert_eq!(
            actor
                .snapshot()
                .current
                .unwrap()
                .signal_path
                .unwrap()
                .output_rate,
            48000
        );
        assert_eq!(actor.playback.playing.as_ref().unwrap().instance, instance);
        assert_eq!(actor.heard_ms(instance), heard);
        assert!(catalog.plays.lock().unwrap().is_empty());
        if paused {
            assert_eq!(actor.position().unwrap().position_ms, position);
            actor.epoch -= Duration::from_secs(20);
            assert_eq!(actor.position().unwrap().position_ms, position);
        }
    }
}

#[test]
fn clock_fault_checks_format_keeps_position_events_and_uses_finite_query_retries() {
    let (mut actor, device, catalog) = actor();
    let instance = start(&mut actor, &device);
    let generation = actor.engine.output_generation();
    let queries = device.lock().unwrap().format_queries;
    device.lock().unwrap().format_failures = 4;
    let (tx, positions) = crossbeam_channel::unbounded();
    actor.publication.subscribers.push(Subscriber {
        tx,
        lagging: false,
        ui: false,
        positions: true,
    });
    let mut last = actor.position().unwrap().position_ms;
    let output = device.lock().unwrap().streams.last().unwrap().1.clone();
    let mut audio = [0.0; 512];
    for attempt in 0..4 {
        ready(&actor);
        output.render_with_info(
            mpmp_engine::output::SampleBufMut::F32(&mut audio),
            &mpmp_engine::output::RenderInfo {
                playback_delay: Some(Duration::from_secs(24)),
            },
        );
        if attempt == 0 {
            actor.poll_output();
        } else {
            output_timer(&mut actor);
        }
        let current = actor.position().unwrap().position_ms;
        assert!(current >= last);
        last = current;
        actor.emit_position();
        assert!(matches!(
            positions.try_recv().unwrap().event,
            Event::Position { .. }
        ));
        assert_eq!(device.lock().unwrap().format_queries, queries + attempt + 1);
        assert_eq!(actor.playback.state, PlayState::Playing);
    }
    assert_eq!(actor.engine.output_generation(), generation);
    assert_eq!(device.lock().unwrap().open_attempts, 1);
    assert!(actor.output.output_policy.next_due().is_none());
    assert!(actor.heard_ms(instance) > 0);
    assert!(catalog.plays.lock().unwrap().is_empty());
    actor.pause();
    let position = actor.position().unwrap().position_ms;
    positions.try_iter().for_each(drop);
    for _ in 0..4 {
        output.render_with_info(
            mpmp_engine::output::SampleBufMut::F32(&mut audio),
            &mpmp_engine::output::RenderInfo {
                playback_delay: Some(Duration::from_secs(24)),
            },
        );
        actor.epoch -= Duration::from_secs(10);
        actor.emit_position();
        let Event::Position { position: emitted } = positions.try_recv().unwrap().event else {
            panic!("position missing")
        };
        assert_eq!(emitted.position_ms, position);
        assert_eq!(emitted.rate, 0.0);
    }
    let heard = actor.heard_ms(instance);
    actor.finish_playing(EndReason::Stopped, &Origin::Ui);
    let history = catalog.plays.lock().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].played_ms, heard);
}

#[test]
fn clock_fault_rebuilds_changed_format_and_format_storms_share_the_retry_budget() {
    for clock_fault in [false, true] {
        let (mut actor, device, _) = actor();
        start(&mut actor, &device);
        {
            let mut d = device.lock().unwrap();
            d.format = Some(StreamFormat {
                sample_rate: 48000,
                ..format()
            });
            d.fail_open = true;
            d.keep_caps = true;
        }
        if clock_fault {
            let mut audio = [0.0; 512];
            device
                .lock()
                .unwrap()
                .streams
                .last()
                .unwrap()
                .1
                .render_with_info(
                    mpmp_engine::output::SampleBufMut::F32(&mut audio),
                    &mpmp_engine::output::RenderInfo {
                        playback_delay: Some(Duration::MAX),
                    },
                );
        } else {
            sink(&device).report_route(RouteFacts {
                format_changed: true,
                ..Default::default()
            });
        }
        actor.on_output();
        assert_eq!(device.lock().unwrap().open_attempts, 2);
        assert_eq!(actor.playback.state, PlayState::Playing);
        let due = actor.output.output_policy.next_due();
        for _ in 0..10 {
            device
                .lock()
                .unwrap()
                .watcher
                .as_ref()
                .unwrap()
                .report_route(RouteFacts {
                    format_changed: true,
                    ..Default::default()
                });
            actor.on_output();
        }
        assert_eq!(actor.output.output_policy.next_due(), due);
        assert_eq!(device.lock().unwrap().open_attempts, 2);
        device.lock().unwrap().fail_open = false;
        output_timer(&mut actor);
        assert_eq!(actor.playback.state, PlayState::Playing);
        assert_eq!(actor.engine.output_format().unwrap().sample_rate, 48000);
        assert_eq!(
            actor
                .snapshot()
                .current
                .unwrap()
                .signal_path
                .unwrap()
                .output_rate,
            48000
        );
    }
}

#[test]
fn failed_format_query_then_success_retains_live_playing_stream() {
    let (mut actor, device, catalog) = actor();
    let instance = start(&mut actor, &device);
    let generation = actor.engine.output_generation();
    let queries = device.lock().unwrap().format_queries;
    device.lock().unwrap().format_failures = 1;
    observe_switch(&mut actor, &device, true);
    assert_eq!(device.lock().unwrap().format_queries, queries + 1);
    assert_eq!(actor.playback.state, PlayState::Playing);
    assert!(actor.engine.is_active());
    assert!(!actor.engine.is_paused());
    assert!(actor.output.output_at.is_none());
    for _ in 0..10 {
        actor.output_action(policy::Action::CheckFormat);
    }
    assert_eq!(device.lock().unwrap().format_queries, queries + 1);
    let heard = actor.heard_ms(instance);
    ready(&actor);
    let audio = device
        .lock()
        .unwrap()
        .streams
        .last()
        .unwrap()
        .1
        .render_f32(256);
    assert!(audio.iter().any(|s| *s != 0.0));
    assert!(actor.heard_ms(instance) > heard);
    output_timer(&mut actor);
    assert_eq!(device.lock().unwrap().format_queries, queries + 2);
    assert_eq!(device.lock().unwrap().open_attempts, 1);
    assert_eq!(actor.engine.output_generation(), generation);
    assert_eq!(actor.playback.state, PlayState::Playing);
    assert!(actor.output.output_policy.next_due().is_none());
    assert!(catalog.plays.lock().unwrap().is_empty());
}

#[test]
fn format_retry_finding_a_real_change_rebuilds_within_the_same_attempt() {
    let (mut actor, device, _) = actor();
    let instance = start(&mut actor, &device);
    device.lock().unwrap().format_failures = 1;
    observe_switch(&mut actor, &device, true);
    let replacement = StreamFormat {
        sample_rate: 48_000,
        ..format()
    };
    device.lock().unwrap().format = Some(replacement);
    output_timer(&mut actor);
    assert_eq!(actor.engine.output_format(), Some(replacement));
    assert_eq!(device.lock().unwrap().open_attempts, 2);
    assert_eq!(actor.playback.state, PlayState::Playing);
    assert!(!actor.engine.is_paused());
    assert_eq!(actor.playback.playing.as_ref().unwrap().instance, instance);
    assert!(actor.output.output_policy.next_due().is_none());
}

#[test]
fn failed_format_queries_exhaust_budget_and_keep_stream_until_device_notification() {
    let (mut actor, device, _) = actor();
    start(&mut actor, &device);
    let queries = device.lock().unwrap().format_queries;
    device.lock().unwrap().format_failures = 4;
    observe_switch(&mut actor, &device, true);
    for attempt in 1..=4 {
        for _ in 0..10 {
            actor.output_action(policy::Action::CheckFormat);
        }
        assert_eq!(device.lock().unwrap().format_queries, queries + attempt);
        assert_eq!(actor.playback.state, PlayState::Playing);
        assert!(actor.engine.is_active());
        assert!(!actor.engine.is_paused());
        if attempt < 4 {
            output_timer(&mut actor);
        }
    }
    assert!(actor.output.output_policy.next_due().is_none());
    assert_eq!(device.lock().unwrap().open_attempts, 1);
    // A real notification may authorize another finite round.
    observe_switch(&mut actor, &device, true);
    assert_eq!(device.lock().unwrap().format_queries, queries + 5);
    assert_eq!(device.lock().unwrap().open_attempts, 1);
    assert!(actor.output.output_policy.next_due().is_none());
}

#[test]
fn format_check_and_rebuild_failures_share_deadlines_and_budget() {
    let (mut actor, device, _) = actor();
    start(&mut actor, &device);
    device.lock().unwrap().format_failures = 1;
    observe_switch(&mut actor, &device, true);
    {
        let mut d = device.lock().unwrap();
        d.keep_caps = true;
        d.fail_open = true;
    }
    for attempts in 1..=4 {
        let due = actor.output.output_policy.next_due();
        let queries = device.lock().unwrap().format_queries;
        for _ in 0..10 {
            actor.output_action(policy::Action::Follow);
            actor.output_action(policy::Action::CheckFormat);
        }
        assert_eq!(actor.output.output_policy.next_due(), due);
        assert_eq!(device.lock().unwrap().format_queries, queries);
        assert_eq!(device.lock().unwrap().open_attempts, attempts);
        if attempts < 4 {
            output_timer(&mut actor);
        }
    }
    assert!(actor.output.output_policy.next_due().is_some());
}

#[test]
fn switch_rebuild_retry_preserves_transport_unless_pause_or_disconnect_intervenes() {
    // Cover the disconnect latch on both a failed attempt and a successful retry,
    // plus user Pause and the unknown-route safety path during the retry interval.
    for initially_paused in [false, true] {
        for interruption in 0..5 {
            let (mut actor, device, catalog) = actor();
            let instance = start(&mut actor, &device);
            if initially_paused {
                actor.pause();
            }
            {
                let mut d = device.lock().unwrap();
                d.keep_caps = true;
                d.fail_open = true;
                d.loss_on_close = interruption == 1;
            }
            observe_switch(&mut actor, &device, false);
            assert_eq!(device.lock().unwrap().open_attempts, 2);
            assert!(!actor.engine.is_active());
            assert_eq!(
                actor.playback.state,
                if initially_paused || interruption == 1 {
                    PlayState::Paused
                } else {
                    PlayState::Playing
                }
            );
            let frozen_position = actor.position().unwrap().position_ms;
            match interruption {
                2 => device.lock().unwrap().loss_on_open = true,
                3 => {
                    actor.pause();
                }
                4 => {
                    device
                        .lock()
                        .unwrap()
                        .watcher
                        .as_ref()
                        .unwrap()
                        .report(OutputEvent::Rerouted);
                    actor.on_output();
                }
                _ => {}
            }
            device.lock().unwrap().fail_open = false;
            let retried = Instant::now();
            output_timer(&mut actor);
            let paused = initially_paused || interruption != 0;
            assert_eq!(
                actor.playback.state,
                if paused {
                    PlayState::Paused
                } else {
                    PlayState::Playing
                }
            );
            assert_eq!(actor.engine.is_paused(), paused);
            assert_eq!(actor.playback.playing.as_ref().unwrap().instance, instance);
            assert_eq!(device.lock().unwrap().open_attempts, 3);
            // Once playing again the reported position follows the wall clock.
            let moved = if paused {
                0
            } else {
                retried.elapsed().as_millis() as u64 + 1
            };
            let position = actor.position().unwrap().position_ms;
            assert!((frozen_position..=frozen_position + moved).contains(&position));
            assert!(actor.output.output_at.is_none());
            assert!(actor.output.output_policy.next_due().is_none());
            assert!(catalog.plays.lock().unwrap().is_empty());
            ready(&actor);
            let audio = device
                .lock()
                .unwrap()
                .streams
                .last()
                .unwrap()
                .1
                .render_f32(256);
            assert_eq!(audio.iter().all(|s| *s == 0.0), paused);
        }
    }
}

#[test]
fn unplug_retains_entry_position_history_and_never_auto_resumes() {
    let (mut actor, device, catalog) = actor();
    let instance = start(&mut actor, &device);
    let before = actor.engine.buffer_status().unwrap().played_frames + 2205;
    let heard = actor.heard_ms(instance);
    let old = sink(&device);
    old.report_route(RouteFacts {
        headphones_disconnected: true,
        old_route_available: Some(false),
        ..Default::default()
    });
    actor.on_output();
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(!actor.engine.is_active());
    assert_eq!(
        actor.position().unwrap().position_ms,
        before * 1000 / 44_100
    );
    assert_eq!(actor.heard_ms(instance), heard);
    assert!(catalog.plays.lock().unwrap().is_empty());
    device
        .lock()
        .unwrap()
        .watcher
        .as_ref()
        .unwrap()
        .report_route(switch());
    actor.on_output();
    if let Some(action) =
        actor
            .output
            .output_policy
            .poll(u64::MAX / 2, actor.engine.output_generation(), 0, 0, true)
    {
        actor.output_action(action);
    }
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(actor.engine.is_paused());
    old.report(OutputEvent::DeviceUnplugged);
    actor.on_output();
    assert!(
        actor.engine.is_active(),
        "retired stream's notification must be ignored"
    );
    actor.finish_playing(EndReason::Stopped, &Origin::Ui);
    assert_eq!(catalog.plays.lock().unwrap().len(), 1);
}

#[test]
fn format_rebuild_restores_volume_next_pause_and_counts_only_new_audio() {
    let (mut actor, device, catalog) = actor();
    let instance = start(&mut actor, &device);
    actor.audio.volume = 0.5;
    actor.engine.set_volume(actor.master_volume());
    let before = actor
        .engine
        .heard(mpmp_engine::EntryId(instance))
        .unwrap()
        .frames;
    let position = actor.engine.position().unwrap();
    // A different sample layout requires rebuilding even at the same sample rate.
    device.lock().unwrap().format = Some(StreamFormat {
        channels: 1,
        sample_type: SampleType::I16,
        ..format()
    });
    actor.output_action(policy::Action::CheckFormat);
    assert_eq!(actor.playback.state, PlayState::Playing);
    assert!(!actor.engine.is_paused());
    assert_eq!(actor.engine.volume(), 0.125);
    assert!(actor.playback.pending_next.is_some());
    assert_eq!(actor.output.output_at, None);
    assert_eq!(device.lock().unwrap().streams.len(), 2);
    ready(&actor);
    device
        .lock()
        .unwrap()
        .streams
        .last()
        .unwrap()
        .1
        .render_i16(256);
    assert_eq!(
        actor
            .engine
            .heard(mpmp_engine::EntryId(instance))
            .unwrap()
            .frames,
        before + 256
    );
    assert!(actor.engine.position().unwrap().frame >= position.frame);
    assert!(catalog.plays.lock().unwrap().is_empty());
    actor.pause();
    device.lock().unwrap().format = Some(StreamFormat {
        sample_rate: 48_000,
        ..format()
    });
    actor.output_action(policy::Action::Follow);
    assert_eq!(actor.engine.output_format().unwrap().sample_rate, 48_000);
    assert_eq!(
        actor
            .snapshot()
            .current
            .unwrap()
            .signal_path
            .unwrap()
            .output_rate,
        48_000
    );
    assert!(actor.engine.is_paused());
    assert_eq!(actor.playback.state, PlayState::Paused);
}

#[test]
fn missing_device_keeps_track_and_unknown_arrival_waits_for_manual_play() {
    let (mut actor, device, catalog) = actor();
    device.lock().unwrap().format = None;
    let entry = actor
        .playback
        .queue
        .replace(&[TrackId(1), TrackId(2)], None)[0];
    actor.play_or_skip(entry, 75, &Origin::Ui);
    assert_eq!(actor.current_entry(), Some(entry));
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(catalog.plays.lock().unwrap().is_empty());
    let instance = actor.playback.playing.as_ref().unwrap().instance;
    for now in [300, 1300, 4300] {
        if let Some(action) = actor
            .output
            .output_policy
            .poll(now + actor.now_ms(), 0, 0, 0, true)
        {
            actor.output_action(action);
        }
    }
    assert!(actor.output.output_policy.next_due().is_some());
    device.lock().unwrap().format = Some(format());
    device
        .lock()
        .unwrap()
        .watcher
        .as_ref()
        .unwrap()
        .report_route(RouteFacts {
            route: Some(DeviceId("returned".into())),
            default_changed: true,
            ..Default::default()
        });
    actor.on_output();
    assert!(actor.engine.is_active());
    assert!(actor.engine.is_paused());
    assert_eq!(actor.playback.playing.as_ref().unwrap().instance, instance);
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(actor.resume(&Origin::Ui));
    assert_eq!(actor.playback.state, PlayState::Playing);
    assert!(catalog.plays.lock().unwrap().is_empty());
}

#[test]
fn unknown_route_pauses_and_xrun_does_not_change_transport() {
    let (mut actor, device, _) = actor();
    start(&mut actor, &device);
    sink(&device).report(OutputEvent::Xrun);
    actor.on_output();
    assert_eq!(actor.playback.state, PlayState::Playing);
    sink(&device).report(OutputEvent::Rerouted);
    actor.on_output();
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert_eq!(
        device.lock().unwrap().streams.len(),
        1,
        "compatible format retains its stream"
    );
    assert!(actor.engine.is_active());
}

#[test]
fn disconnect_survives_timer_close_and_open_boundaries() {
    for boundary in 0..3 {
        let (mut actor, device, _) = actor();
        start(&mut actor, &device);
        sink(&device).report_route(switch());
        actor.on_output();
        match boundary {
            0 => sink(&device).report(OutputEvent::DeviceUnplugged),
            1 => device.lock().unwrap().loss_on_close = true,
            _ => device.lock().unwrap().loss_on_open = true,
        }
        // Force the timer's decision before it consumes pending facts.
        actor.output_action(policy::Action::Follow);
        assert_eq!(
            actor.playback.state,
            PlayState::Paused,
            "boundary {boundary}"
        );
        assert!(actor.engine.is_paused());
        ready(&actor);
        let output = device.lock().unwrap().streams.last().unwrap().1.clone();
        assert!(output.render_f32(512).iter().all(|s| *s == 0.0));
    }
}

#[test]
fn switch_open_failure_exhausts_retries_and_persistent_arrival_prepares_silently() {
    let (mut actor, device, _) = actor();
    let instance = start(&mut actor, &device);
    device.lock().unwrap().fail_open = true;
    actor.output_action(policy::Action::Follow);
    assert_eq!(actor.playback.state, PlayState::Playing);
    for _ in 0..3 {
        let due = actor.output.output_policy.next_due().unwrap();
        let action = actor
            .output
            .output_policy
            .poll(due, actor.engine.output_generation(), 0, 0, true)
            .unwrap();
        actor.output_action(action);
    }
    assert!(actor.output.output_policy.next_due().is_some());
    {
        let mut d = device.lock().unwrap();
        d.format = Some(format());
        d.fail_open = false;
        d.watcher.as_ref().unwrap().report_route(switch());
    }
    actor.on_output();
    assert!(actor.engine.is_active());
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(actor.engine.is_paused());
    assert_eq!(actor.playback.playing.as_ref().unwrap().instance, instance);
}

#[test]
fn gapless_rebuild_reduces_committed_successor_before_selecting_recovery() {
    let (mut actor, device, catalog) = actor();
    let first = start(&mut actor, &device);
    let second = actor.playback.pending_next.unwrap();
    let output = device.lock().unwrap().streams.last().unwrap().1.clone();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "successor did not render");
        ready(&actor);
        output.render_f32(256);
        if actor.engine.position().is_some_and(|p| p.entry.0 == second) {
            break;
        }
    }
    assert_eq!(actor.playback.playing.as_ref().unwrap().instance, first);
    let second_heard = actor
        .engine
        .heard(mpmp_engine::EntryId(second))
        .unwrap()
        .frames;
    actor.close_output();
    assert_eq!(actor.playback.playing.as_ref().unwrap().instance, second);
    assert_eq!(actor.output.output_at, Some((second, second_heard)));
    let plays = catalog.plays.lock().unwrap();
    assert_eq!(plays.len(), 1);
    assert_eq!(plays[0].track, TrackId(1));
    assert_eq!(plays[0].ended, EndReason::Completed);
    assert_eq!(plays[0].played_ms, actor.heard_ms(first));
    drop(plays);
    assert_eq!(actor.rebuild_output(), RebuildResult::Ready);
    assert_eq!(actor.playback.playing.as_ref().unwrap().instance, second);
}

#[test]
fn resume_cannot_override_a_disconnect_received_while_opening() {
    let (mut actor, device, _) = actor();
    start(&mut actor, &device);
    actor.output_action(policy::Action::PauseClose);
    device.lock().unwrap().loss_on_open = true;
    assert!(!actor.resume(&Origin::Ui));
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(actor.engine.is_paused());
    ready(&actor);
    let output = device.lock().unwrap().streams.last().unwrap().1.clone();
    for _ in 0..4 {
        assert!(output.render_f32(512).iter().all(|s| *s == 0.0));
    }
}

type ReopenGate = (
    crossbeam_channel::Sender<()>,
    crossbeam_channel::Receiver<()>,
);

#[derive(Default)]
struct FailingResolver(std::sync::atomic::AtomicBool, Mutex<Option<ReopenGate>>);

impl SourceResolver for FailingResolver {
    fn resolve(
        &self,
        uri: &MediaUri,
    ) -> Result<mpmp_media_io::ResolvedSource, mpmp_media_io::Error> {
        if self.0.load(std::sync::atomic::Ordering::Acquire) {
            if let Some((entered, release)) = self.1.lock().unwrap().take() {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            return Err(std::io::Error::other("injected reopen failure").into());
        }
        FileResolver.resolve(uri)
    }
}

fn queue_failure_before_delayed_start(
    actor: &mut Actor,
    device: &Mutex<Device>,
    catalog: &CatalogStub,
    resolver: &FailingResolver,
) -> u64 {
    catalog
        .short_first
        .store(true, std::sync::atomic::Ordering::Release);
    let entry = actor
        .playback
        .queue
        .replace(&[TrackId(1), TrackId(2), TrackId(3)], None)[0];
    actor.play_entry(entry, 50).unwrap();
    let first = actor.playback.playing.as_ref().unwrap().instance;
    ready(actor);
    let output = device.lock().unwrap().streams.last().unwrap().1.clone();
    // Hold the decode worker before the failing reopen: renderer reports cannot be
    // observed until we release it. No fabricated latency or scheduling delay.
    let (entered, waiting) = crossbeam_channel::bounded(1);
    let (release, released) = crossbeam_channel::bounded(1);
    *resolver.1.lock().unwrap() = Some((entered, released));
    resolver.0.store(true, std::sync::atomic::Ordering::Release);
    actor.engine.seek(mpmp_engine::EntryId(first), 0);
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    output.render_f32(256);
    assert!(
        actor.engine_events.is_empty(),
        "observer is held at the reopen gate"
    );
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while actor.engine_events.is_empty() {
        assert!(Instant::now() < deadline, "failure was not queued");
        std::thread::yield_now();
    }
    resolver
        .0
        .store(false, std::sync::atomic::Ordering::Release);
    first
}

#[test]
fn frozen_late_start_cannot_revive_a_failed_instance() {
    let resolver = Arc::new(FailingResolver::default());
    let (mut actor, device, catalog) = actor_with_source(resolver.clone());
    let (tx, events) = crossbeam_channel::unbounded();
    actor.publication.subscribers.push(Subscriber {
        tx,
        lagging: false,
        ui: false,
        positions: false,
    });
    let first = queue_failure_before_delayed_start(&mut actor, &device, &catalog, &resolver);
    sink(&device).report(OutputEvent::DeviceUnplugged);
    actor.on_output();
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(!actor.engine.is_active());
    assert_eq!(device.lock().unwrap().streams.len(), 1);
    assert_eq!(actor.current_track().unwrap().id, TrackId(2));
    assert_ne!(actor.output.output_at.unwrap().0, first);
    let facts: Vec<_> = events
        .try_iter()
        .map(|e| e.event)
        .filter(|e| matches!(e, Event::TrackFailed { .. } | Event::TrackStarted { .. }))
        .collect();
    assert!(matches!(
        facts.as_slice(),
        [Event::TrackFailed { .. }, Event::TrackStarted { .. }]
    ));
    let plays = catalog.plays.lock().unwrap();
    assert_eq!(plays.len(), 1);
    assert_eq!(plays[0].track, TrackId(1));
    assert_eq!(plays[0].ended, EndReason::Failed);
    assert!(plays[0].played_ms > 0);
    drop(plays);
    assert_eq!(actor.rebuild_output(), RebuildResult::Ready);
    assert!(actor.engine.is_paused());
    actor.finish_playing(EndReason::Stopped, &Origin::Ui);
    assert_eq!(
        catalog
            .plays
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.track == TrackId(1))
            .count(),
        1
    );
}

#[test]
fn frozen_batch_settles_duplicate_failures_once_and_skips_failed_successors() {
    let resolver = Arc::new(FailingResolver::default());
    let (mut actor, device, catalog) = actor_with_source(resolver.clone());
    let first = queue_failure_before_delayed_start(&mut actor, &device, &catalog, &resolver);
    let second = actor.playback.pending_next.unwrap();
    sink(&device).report(OutputEvent::DeviceUnplugged);
    let mut frozen = actor.engine.freeze_output();
    let Some(EngineEvent::TrackStarted { signal_path, .. }) = frozen.events.last().cloned() else {
        panic!("freezing must commit A's delayed start");
    };
    let first_failure = EngineEvent::TrackFailed {
        entry: mpmp_engine::EntryId(first),
        reason: "decode error: A".into(),
    };
    let second_failure = EngineEvent::TrackFailed {
        entry: mpmp_engine::EntryId(second),
        reason: "decode error: B".into(),
    };
    // Exercise duplicates and a failed pending successor in the same frozen batch.
    frozen.events = vec![
        first_failure.clone(),
        second_failure.clone(),
        EngineEvent::TrackStarted {
            entry: mpmp_engine::EntryId(first),
            signal_path: signal_path.clone(),
        },
        EngineEvent::TrackStarted {
            entry: mpmp_engine::EntryId(second),
            signal_path,
        },
        first_failure,
        second_failure,
    ];
    actor.playback.state = PlayState::Paused;
    actor.reduce_frozen(frozen, PlayState::Paused);
    actor.on_output();
    assert_eq!(actor.current_track().unwrap().id, TrackId(3));
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(!actor.engine.is_active());
    assert_eq!(device.lock().unwrap().open_attempts, 1);
    let plays = catalog.plays.lock().unwrap();
    assert_eq!(
        plays.iter().map(|p| (p.track, p.ended)).collect::<Vec<_>>(),
        vec![
            (TrackId(1), EndReason::Failed),
            (TrackId(2), EndReason::Failed)
        ]
    );
}

#[test]
fn deferred_undecodable_successor_skips_to_c_without_resuming_after_unplug() {
    let resolver = Arc::new(FailingResolver::default());
    let (mut actor, device, catalog) = actor_with_source(resolver.clone());
    queue_failure_before_delayed_start(&mut actor, &device, &catalog, &resolver);
    let bad = tempfile::NamedTempFile::with_suffix(".flac").unwrap();
    std::fs::write(bad.path(), b"this file exists but cannot be decoded").unwrap();
    catalog
        .uris
        .lock()
        .unwrap()
        .insert(TrackId(2), MediaUri::from_path(bad.path()).unwrap());
    let (tx, events) = crossbeam_channel::unbounded();
    actor.publication.subscribers.push(Subscriber {
        tx,
        lagging: false,
        ui: false,
        positions: false,
    });
    sink(&device).report(OutputEvent::DeviceUnplugged);
    actor.on_output();
    assert_eq!(actor.current_track().unwrap().id, TrackId(2));
    assert_eq!(device.lock().unwrap().open_attempts, 1);
    assert!(!actor.engine.is_active());
    device
        .lock()
        .unwrap()
        .watcher
        .as_ref()
        .unwrap()
        .report_route(RouteFacts {
            old_route_available: Some(false),
            ..switch()
        });
    actor.on_output();
    assert_eq!(actor.current_track().unwrap().id, TrackId(3));
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(actor.engine.is_paused());
    assert_eq!(device.lock().unwrap().open_attempts, 2);
    assert!(actor.output.output_policy.next_due().is_none());
    ready(&actor);
    let output = device.lock().unwrap().streams.last().unwrap().1.clone();
    for _ in 0..4 {
        assert!(output.render_f32(512).iter().all(|s| *s == 0.0));
    }
    let failures: Vec<_> = events
        .try_iter()
        .filter_map(|e| match e.event {
            Event::TrackFailed {
                entry, upcoming, ..
            } => {
                assert!(!upcoming);
                Some(entry)
            }
            _ => None,
        })
        .collect();
    assert_eq!(failures.len(), 2);
    assert_ne!(failures[0], failures[1]);
    let plays = catalog.plays.lock().unwrap();
    assert_eq!(
        plays.iter().map(|p| (p.track, p.ended)).collect::<Vec<_>>(),
        vec![
            (TrackId(1), EndReason::Failed),
            (TrackId(2), EndReason::Failed)
        ]
    );
    assert_eq!(plays[1].played_ms, 0);
    drop(plays);
    assert!(actor.resume(&Origin::Ui));
    assert_eq!(actor.current_track().unwrap().id, TrackId(3));
    assert!(output.render_f32(512).iter().any(|s| *s != 0.0));
    actor.stop(&Origin::Ui);
    assert_eq!(catalog.plays.lock().unwrap().len(), 3);
}

#[test]
fn deferred_file_failures_stop_at_normal_limit_without_output_retries() {
    for mixed in [false, true] {
        let (mut actor, device, catalog) = actor();
        let bad = tempfile::NamedTempFile::with_suffix(".flac").unwrap();
        std::fs::write(bad.path(), b"not decodable").unwrap();
        let tracks: Vec<_> = (1..=7).map(TrackId).collect();
        let missing = bad.path().with_extension("missing");
        for track in &tracks {
            catalog.uris.lock().unwrap().insert(
                *track,
                MediaUri::from_path(if mixed && track.0 % 2 == 0 {
                    &missing
                } else {
                    bad.path()
                })
                .unwrap(),
            );
        }
        let entry = actor.playback.queue.replace(&tracks, None)[0];
        actor.play_entry_with_output(entry, 0, false).unwrap();
        actor.playback.state = PlayState::Paused;
        actor.output_action(policy::Action::Rebuild);
        assert_eq!(actor.playback.state, PlayState::Stopped);
        assert!(actor.playback.playing.is_none());
        assert!(
            actor.playback.instances.is_empty()
                && actor.playback.pending_next.is_none()
                && actor.output.output_at.is_none()
        );
        assert!(!actor.engine.is_active());
        assert_eq!(device.lock().unwrap().open_attempts, 0);
        assert!(actor.output.output_policy.next_due().is_none());
        let plays = catalog.plays.lock().unwrap();
        assert_eq!(plays.len(), MAX_CONSECUTIVE_FAILURES);
        assert!(
            plays
                .iter()
                .all(|p| p.ended == EndReason::Failed && p.played_ms == 0)
        );
    }
}

#[test]
fn missing_default_notification_pauses_and_exhausts_finite_retries() {
    let (mut actor, device, catalog) = actor();
    let instance = start(&mut actor, &device);
    device.lock().unwrap().format = None;
    sink(&device).report_route(RouteFacts {
        route: None,
        default_changed: true,
        old_route_available: Some(false),
        ..Default::default()
    });
    actor.on_output();
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(!actor.engine.is_active());
    for _ in 0..3 {
        let due = actor
            .output
            .output_policy
            .next_due()
            .expect("missing default schedules a retry");
        let action = actor
            .output
            .output_policy
            .poll(due, actor.engine.output_generation(), 0, 0, true)
            .unwrap();
        actor.output_action(action);
        assert_eq!(actor.playback.playing.as_ref().unwrap().instance, instance);
        assert_eq!(actor.playback.state, PlayState::Paused);
    }
    for _ in 0..20 {
        output_timer(&mut actor);
        assert_eq!(actor.playback.state, PlayState::Paused);
    }
    assert!(actor.output.output_policy.next_due().is_none());
    assert!(
        actor
            .output
            .output_policy
            .poll(100_000, actor.engine.output_generation(), 0, 0, true)
            .is_none()
    );
    assert!(catalog.plays.lock().unwrap().is_empty());
}

#[test]
fn queued_current_failure_and_unplug_never_open_a_successor_stream() {
    let resolver = Arc::new(FailingResolver::default());
    let (mut actor, device, catalog) = actor_with_source(resolver.clone());
    catalog
        .short_first
        .store(true, std::sync::atomic::Ordering::Release);
    let first = start(&mut actor, &device);
    // ready() requires more frames than remain in A: B is already decoding, while
    // A is still heard. Seeking A now reopens it, producing a real engine failure.
    resolver.0.store(true, std::sync::atomic::Ordering::Release);
    actor.engine.seek(mpmp_engine::EntryId(first), 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while actor.engine_events.is_empty() {
        assert!(Instant::now() < deadline, "failure was not queued");
        std::thread::yield_now();
    }
    sink(&device).report(OutputEvent::DeviceUnplugged);
    actor.on_output();
    assert_eq!(device.lock().unwrap().streams.len(), 1);
    assert!(!actor.engine.is_active());
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert_eq!(actor.current_track().unwrap().id, TrackId(2));
    assert_eq!(catalog.plays.lock().unwrap()[0].ended, EndReason::Failed);
}

#[test]
fn frozen_seek_uses_rendered_generation_or_keeps_the_unrendered_target() {
    for rendered in [false, true] {
        let (mut actor, device, catalog) = actor();
        let instance = start(&mut actor, &device);
        let entry = actor.current_entry().unwrap();
        actor.seek_entry(entry, 500).unwrap();
        let mut lower = 0;
        if rendered {
            let output = device.lock().unwrap().streams.last().unwrap().1.clone();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                assert!(Instant::now() < deadline, "seek did not render");
                ready(&actor);
                output.render_f32(256);
                if let Some(p) = actor.engine.position()
                    && p.frame >= 22_050
                {
                    lower = p.frame;
                    break;
                }
            }
        }
        // A stale hint must not substitute thirty seconds of wall time for content.
        actor.epoch -= Duration::from_secs(30);
        assert!(actor.playback.seek_hint.get().is_some());
        actor.output_action(policy::Action::PauseClose);
        let frame = actor.output.output_at.unwrap().1;
        if rendered {
            assert!((lower..=lower + 256).contains(&frame), "{lower} -> {frame}");
        } else {
            assert_eq!(frame, 22_050);
        }
        let position = actor.position().unwrap().position_ms;
        actor.epoch -= Duration::from_secs(30);
        assert_eq!(actor.position().unwrap().position_ms, position);
        let heard = actor.heard_ms(instance);
        assert!(catalog.plays.lock().unwrap().is_empty());
        assert_eq!(actor.rebuild_output(), RebuildResult::Ready);
        assert_eq!(actor.heard_ms(instance), heard);
    }
}

#[test]
fn persistent_open_failure_with_available_caps_shares_one_retry_budget() {
    let (mut actor, device, _) = actor();
    start(&mut actor, &device);
    {
        let mut d = device.lock().unwrap();
        d.keep_caps = true;
        d.fail_open = true;
    }
    actor.output_action(policy::Action::Follow);
    for expected_attempts in 2..=5 {
        assert_eq!(device.lock().unwrap().open_attempts, expected_attempts);
        // Error storms and other automatic entry points cannot bypass the timer.
        for _ in 0..10 {
            device
                .lock()
                .unwrap()
                .watcher
                .as_ref()
                .unwrap()
                .report(OutputEvent::DeviceUnavailable);
            actor.on_output();
            actor.output_action(policy::Action::Follow);
            actor.output_action(policy::Action::Rebuild);
        }
        assert_eq!(device.lock().unwrap().open_attempts, expected_attempts);
        if expected_attempts < 5 {
            let due = actor.output.output_policy.next_due().unwrap();
            let action = actor
                .output
                .output_policy
                .poll(due, actor.engine.output_generation(), 0, 0, true)
                .unwrap();
            actor.output_action(action);
        }
    }
    assert_eq!(device.lock().unwrap().open_attempts, 5);
    assert!(actor.output.output_policy.next_due().is_some());
    for _ in 0..20 {
        output_timer(&mut actor);
    }
    assert_eq!(device.lock().unwrap().open_attempts, 25);
    assert!(actor.output.output_policy.next_due().is_none());
    assert!(actor.engine.default_output_format().is_ok());
}

#[test]
fn unplug_at_a_gapless_boundary_announces_queue_without_further_events() {
    let (mut actor, device, _) = actor();
    start(&mut actor, &device);
    actor.announce_queue();
    let (tx, events) = crossbeam_channel::unbounded();
    actor.publication.subscribers.push(Subscriber {
        tx,
        lagging: false,
        ui: false,
        positions: false,
    });
    let second = actor.playback.pending_next.unwrap();
    let output = device.lock().unwrap().streams.last().unwrap().1.clone();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "successor did not render");
        ready(&actor);
        output.render_f32(256);
        if actor.engine.position().is_some_and(|p| p.entry.0 == second) {
            break;
        }
    }
    sink(&device).report(OutputEvent::DeviceUnplugged);
    actor.on_output();
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(!actor.engine.is_active());
    let changes: Vec<_> = events
        .try_iter()
        .filter_map(|event| {
            if let Event::QueueChanged { queue } = event.event {
                assert!(event.origin.is_none());
                Some(queue)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(changes, vec![actor.queue_summary()]);
    assert_eq!(changes[0].current_index, Some(1));
}

#[test]
fn failed_user_resume_reports_error_and_gentle_retry_prepares_silent_output() {
    for origin in [Origin::Ui, Origin::MediaControls] {
        let (mut actor, device, catalog) = actor();
        let instance = start(&mut actor, &device);
        device.lock().unwrap().activation = Some(Activation {
            default_caps_fail: true,
            endpoint_fail: true,
            ..Default::default()
        });
        actor.output_action(policy::Action::Follow);
        for _ in 0..3 {
            output_timer(&mut actor);
        }
        let before = device
            .lock()
            .unwrap()
            .activation
            .as_ref()
            .unwrap()
            .calls
            .len();
        let error = actor
            .apply_command(Command::Resume, &origin, None, None)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::OutputFailed);
        assert!(error.details.retryable);
        assert_eq!(
            &device.lock().unwrap().activation.as_ref().unwrap().calls[before..],
            ["default caps", "fresh caps", "endpoint caps"]
        );
        assert!(
            actor
                .problems
                .iter()
                .any(|p| p.code == ErrorCode::OutputFailed)
        );
        assert!(
            actor
                .playback
                .playing
                .as_ref()
                .unwrap()
                .signal_path
                .is_none()
        );
        assert!(!actor.engine.is_active());
        let due = actor.output.output_policy.next_due().unwrap();
        assert!(due > actor.now_ms());
        device
            .lock()
            .unwrap()
            .activation
            .as_mut()
            .unwrap()
            .endpoint_fail = false;
        output_timer(&mut actor);
        assert!(actor.engine.is_active());
        assert!(actor.engine.is_paused());
        assert_eq!(actor.playback.state, PlayState::Paused);
        assert_eq!(actor.playback.playing.as_ref().unwrap().instance, instance);
        assert!(
            actor
                .playback
                .playing
                .as_ref()
                .unwrap()
                .signal_path
                .is_some()
        );
        assert!(
            !actor
                .problems
                .iter()
                .any(|p| p.code == ErrorCode::OutputFailed)
        );
        assert!(actor.output.output_policy.next_due().is_none());
        assert!(catalog.plays.lock().unwrap().is_empty());
        assert!(actor.resume(&origin));
        assert!(!actor.engine.is_paused());
    }
}

#[test]
fn silent_preparation_is_finite_and_pause_never_restarts_playback() {
    let (mut actor, device, _) = actor();
    let entry = actor.playback.queue.replace(&[TrackId(1)], None)[0];
    device.lock().unwrap().format = None;
    actor.play_entry(entry, 0).unwrap();
    for _ in 0..23 {
        output_timer(&mut actor);
        assert_eq!(actor.playback.state, PlayState::Paused);
    }
    assert!(actor.output.output_policy.next_due().is_none());
    actor
        .apply_command(Command::Resume, &Origin::Ui, None, None)
        .unwrap_err();
    assert!(actor.output.output_policy.next_due().is_some());
    assert!(!actor.pause());
    device.lock().unwrap().format = Some(format());
    output_timer(&mut actor);
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(actor.engine.is_paused());
}

#[test]
fn device_arrival_prepares_output_clears_problem_and_requires_one_new_play() {
    for origin in [Origin::Ui, Origin::MediaControls] {
        for with_route_id in [false, true] {
            let (mut actor, device, _) = actor();
            let entry = actor.playback.queue.replace(&[TrackId(1)], None)[0];
            device.lock().unwrap().format = None;
            let error = actor
                .apply_command(Command::PlayEntry { entry }, &origin, None, None)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::OutputFailed);
            assert_eq!(actor.playback.state, PlayState::Paused);
            let instance = actor.playback.playing.as_ref().unwrap().instance;
            for _ in 0..3 {
                output_timer(&mut actor);
            }
            {
                let mut d = device.lock().unwrap();
                d.format = Some(format());
                d.watcher.as_ref().unwrap().report_route(RouteFacts {
                    route: with_route_id.then(|| DeviceId("arrived".into())),
                    default_changed: true,
                    old_route_available: None,
                    ..Default::default()
                });
            }
            actor.on_output();
            assert!(actor.engine.is_active());
            assert!(actor.engine.is_paused());
            assert_eq!(actor.playback.state, PlayState::Paused);
            assert_eq!(actor.playback.playing.as_ref().unwrap().instance, instance);
            assert!(
                !actor
                    .problems
                    .iter()
                    .any(|p| p.code == ErrorCode::OutputFailed)
            );
            ready(&actor);
            let output = device.lock().unwrap().streams.last().unwrap().1.clone();
            assert!(output.render_f32(256).iter().all(|s| *s == 0.0));
            actor
                .apply_command(Command::Resume, &origin, None, None)
                .unwrap();
            assert_eq!(actor.playback.state, PlayState::Playing);
            assert!(output.render_f32(256).iter().any(|s| *s != 0.0));
        }
    }
}

#[test]
fn explicit_switch_that_reaches_low_frequency_preparation_stays_silent() {
    let (mut actor, device, _) = actor();
    start(&mut actor, &device);
    {
        let mut d = device.lock().unwrap();
        d.fail_open = true;
        d.keep_caps = true;
    }
    observe_switch(&mut actor, &device, false);
    for _ in 0..3 {
        output_timer(&mut actor);
    }
    assert_eq!(actor.playback.state, PlayState::Paused);
    device.lock().unwrap().fail_open = false;
    output_timer(&mut actor);
    assert!(actor.engine.is_active());
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(actor.engine.is_paused());
    ready(&actor);
    let output = device.lock().unwrap().streams.last().unwrap().1.clone();
    assert!(output.render_f32(256).iter().all(|sample| *sample == 0.0));
    assert!(
        !actor
            .problems
            .iter()
            .any(|p| p.code == ErrorCode::OutputFailed)
    );
    assert!(actor.resume(&Origin::Ui));
    assert!(output.render_f32(256).iter().any(|sample| *sample != 0.0));
}

#[test]
fn unknown_default_change_with_open_stream_still_pauses() {
    let (mut actor, device, _) = actor();
    start(&mut actor, &device);
    assert!(actor.engine.is_active());
    device
        .lock()
        .unwrap()
        .watcher
        .as_ref()
        .unwrap()
        .report_route(RouteFacts {
            route: Some(DeviceId("unknown".into())),
            default_changed: true,
            old_route_available: None,
            ..Default::default()
        });
    actor.on_output();
    assert_ne!(actor.playback.state, PlayState::Playing);
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(actor.engine.is_paused());
}

#[test]
fn initial_play_failure_and_selection_report_output_error_without_failing_track() {
    for operation in 0..4 {
        let (mut actor, device, catalog) = actor();
        let entry = actor.playback.queue.replace(&[TrackId(1)], None)[0];
        let command = match operation {
            0 => Command::Resume,
            1 => Command::PlayEntry { entry },
            2 => Command::TogglePause,
            _ => Command::Play {
                context: PlayContext::Tracks {
                    tracks: vec![TrackId(1)],
                },
                start: 0,
                start_track: None,
            },
        };
        {
            let mut d = device.lock().unwrap();
            d.fail_open = true;
            d.keep_caps = true;
        }
        let error = actor
            .apply_command(command, &Origin::Ui, None, None)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::OutputFailed);
        assert!(error.details.retryable);
        assert_eq!(actor.playback.state, PlayState::Paused);
        assert_eq!(actor.playback.state, PlayState::Paused);
        assert!(actor.playback.playing.is_some());
        assert!(
            actor
                .problems
                .iter()
                .any(|p| p.code == ErrorCode::OutputFailed)
        );
        assert!(catalog.plays.lock().unwrap().is_empty());
        for _ in 0..3 {
            output_timer(&mut actor);
        }
        device.lock().unwrap().fail_open = false;
        output_timer(&mut actor);
        assert_eq!(actor.playback.state, PlayState::Paused);
        assert!(actor.engine.is_paused());
        assert!(
            !actor
                .problems
                .iter()
                .any(|p| p.code == ErrorCode::OutputFailed)
        );
        assert!(actor.resume(&Origin::Ui));
        assert!(!actor.engine.is_paused());
    }
}

#[test]
fn fresh_default_success_retains_automatic_route_and_transport() {
    for paused in [false, true] {
        let (mut actor, device, _) = actor();
        start(&mut actor, &device);
        if paused {
            actor.pause();
        }
        device.lock().unwrap().activation = Some(Activation {
            default_open_fail: true,
            fresh_success: true,
            ..Default::default()
        });
        actor.output_action(policy::Action::Follow);
        let d = device.lock().unwrap();
        let activation = d.activation.as_ref().unwrap();
        assert_eq!(
            activation.calls,
            ["default caps", "default open", "fresh open"]
        );
        assert!(!activation.pinned_stream);
        assert!(actor.engine.is_active());
        assert_eq!(actor.engine.is_paused(), paused);
        assert!(
            !actor
                .problems
                .iter()
                .any(|p| p.code == ErrorCode::OutputFailed)
        );
    }
}

#[test]
fn queued_failure_after_user_pause_keeps_successor_and_background_preparation_silent() {
    for fail_open in [false, true] {
        let resolver = Arc::new(FailingResolver::default());
        let (mut actor, device, catalog) = actor_with_source(resolver.clone());
        queue_failure_before_delayed_start(&mut actor, &device, &catalog, &resolver);
        actor.pause();
        {
            let mut d = device.lock().unwrap();
            d.fail_open = fail_open;
            d.keep_caps = true;
        }
        let failure = actor.engine_events.try_recv().unwrap();
        assert!(matches!(failure, EngineEvent::TrackFailed { .. }));
        actor.on_engine(failure);
        assert_eq!(actor.current_track().unwrap().id, TrackId(2));
        assert_eq!(actor.playback.state, PlayState::Paused);
        assert_ne!(actor.playback.state, PlayState::Playing);
        assert!(actor.engine.is_paused());
        if fail_open {
            for _ in 0..3 {
                output_timer(&mut actor);
            }
            // Background preparation may continue, but cannot resume the successor.
            assert!(actor.output.output_policy.next_due().is_some());
            device.lock().unwrap().fail_open = false;
            actor.output.output_policy.external_change();
            actor.output_action(policy::Action::Rebuild);
            assert!(actor.engine.is_paused());
            assert_ne!(actor.playback.state, PlayState::Playing);
        }
        ready(&actor);
        let output = device.lock().unwrap().streams.last().unwrap().1.clone();
        for _ in 0..4 {
            assert!(output.render_f32(512).iter().all(|s| *s == 0.0));
        }
        assert!(actor.resume(&Origin::Ui));
        assert!(output.render_f32(512).iter().any(|s| *s != 0.0));
    }
}

#[test]
fn frozen_batch_recovers_after_final_completed_successor_and_settles_it_once() {
    for with_c in [false, true] {
        for transport in [PlayState::Playing, PlayState::Paused] {
            let resolver = Arc::new(FailingResolver::default());
            let (mut actor, device, catalog) = actor_with_source(resolver.clone());
            let first =
                queue_failure_before_delayed_start(&mut actor, &device, &catalog, &resolver);
            let second = actor.playback.pending_next.unwrap();
            if !with_c {
                let third = actor.playback.queue.entries()[2].id;
                actor.playback.queue.remove(&[third], None).unwrap();
            }
            let mut frozen = actor.engine.freeze_output();
            let Some(EngineEvent::TrackStarted { signal_path, .. }) = frozen.events.last().cloned()
            else {
                panic!("freezing must commit A's delayed start");
            };
            frozen.events = vec![
                EngineEvent::TrackFailed {
                    entry: mpmp_engine::EntryId(first),
                    reason: "decode error: A".into(),
                },
                EngineEvent::TrackStarted {
                    entry: mpmp_engine::EntryId(first),
                    signal_path: signal_path.clone(),
                },
                EngineEvent::TrackStarted {
                    entry: mpmp_engine::EntryId(second),
                    signal_path: signal_path.clone(),
                },
                EngineEvent::EndOfQueue {
                    entry: mpmp_engine::EntryId(second),
                },
                // Duplicate terminal/start facts cannot revive B or settle it twice.
                EngineEvent::TrackStarted {
                    entry: mpmp_engine::EntryId(second),
                    signal_path,
                },
                EngineEvent::EndOfQueue {
                    entry: mpmp_engine::EntryId(second),
                },
            ];
            actor.playback.state = transport;
            actor.reduce_frozen(frozen, transport);
            assert!(!actor.engine.is_active());
            assert_eq!(device.lock().unwrap().open_attempts, 1);
            if with_c {
                assert_eq!(actor.current_track().unwrap().id, TrackId(3));
                assert_eq!(actor.playback.state, transport);
                assert_ne!(actor.output.output_at.unwrap().0, second);
            } else {
                assert_eq!(actor.playback.state, PlayState::Stopped);
                assert!(actor.playback.playing.is_none());
                assert!(actor.output.output_at.is_none());
                assert_ne!(actor.playback.state, PlayState::Playing);
            }
            let plays = catalog.plays.lock().unwrap();
            assert_eq!(
                plays.iter().map(|p| (p.track, p.ended)).collect::<Vec<_>>(),
                [
                    (TrackId(1), EndReason::Failed),
                    (TrackId(2), EndReason::Completed)
                ]
            );
        }
    }
}

#[test]
fn stop_closes_live_output_even_after_degraded_logical_completion() {
    let (mut actor, device, catalog) = actor();
    let instance = start(&mut actor, &device);
    let output = device.lock().unwrap().streams.last().unwrap().1.clone();
    output.render_with_info(
        mpmp_engine::output::SampleBufMut::F32(&mut [0.0; 512]),
        &mpmp_engine::output::RenderInfo {
            playback_delay: Some(Duration::from_secs(20)),
        },
    );
    assert!(
        actor
            .engine
            .take_output_facts()
            .unwrap()
            .contains(OutputEvent::ClockUnreliable)
    );
    // The engine tests exercise the clock deadline. Here inject its terminal event
    // to isolate the command behavior after logical completion with a live stream.
    actor.on_engine(EngineEvent::EndOfQueue {
        entry: mpmp_engine::EntryId(instance),
    });
    assert_eq!(actor.playback.state, PlayState::Stopped);
    assert!(output.is_open());
    assert!(actor.stop(&Origin::Ui));
    assert!(!output.is_open());
    assert!(!actor.engine.is_active());
    assert!(!actor.stop(&Origin::Ui));
    let plays = catalog.plays.lock().unwrap();
    assert_eq!(plays.len(), 1);
    assert_eq!(plays[0].played_ms, actor.heard_ms(instance));
}

#[test]
fn stable_bluetooth_latency_from_first_callback_keeps_natural_actor_completion_correct() {
    for delay in [Duration::from_millis(300), Duration::from_secs(1)] {
        let (mut actor, device, catalog) = actor();
        catalog
            .short_first
            .store(true, std::sync::atomic::Ordering::Release);
        let entry = actor.playback.queue.replace(&[TrackId(1)], None)[0];
        actor.play_entry(entry, 0).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !actor.engine.buffer_status().unwrap().end_of_stream {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let output = device.lock().unwrap().streams.last().unwrap().1.clone();
        let queries = device.lock().unwrap().format_queries;
        let submitted = Instant::now();
        output.render_with_info(
            mpmp_engine::output::SampleBufMut::F32(&mut [0.0; 8820]),
            &mpmp_engine::output::RenderInfo {
                playback_delay: Some(delay),
            },
        );
        loop {
            let event = actor
                .engine_events
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            let ended = matches!(event, EngineEvent::EndOfQueue { .. });
            actor.on_engine(event);
            actor.poll_output();
            if ended {
                break;
            }
            assert_eq!(actor.playback.state, PlayState::Playing);
        }
        let elapsed = submitted.elapsed();
        let tail = delay + Duration::from_secs_f64(4096.0 / 44100.0);
        assert!(
            elapsed + Duration::from_millis(2) >= tail,
            "premature completion: {elapsed:?} < {tail:?}"
        );
        assert!(
            elapsed < tail + Duration::from_secs(1),
            "unexpected completion delay: {elapsed:?}"
        );
        assert_eq!(
            device.lock().unwrap().format_queries,
            queries,
            "stable latency must not trigger a clock fault"
        );
        assert_eq!(actor.playback.state, PlayState::Stopped);
        assert_eq!(catalog.plays.lock().unwrap().len(), 1);
        assert!(actor.stop(&Origin::Ui));
        assert!(!output.is_open());
        assert_eq!(catalog.plays.lock().unwrap().len(), 1);
    }
}

// Leave the actor on A while the engine position already belongs to B.
fn drive_gapless_boundary(actor: &mut Actor, device: &Mutex<Device>, catalog: &CatalogStub) {
    catalog
        .short_first
        .store(true, std::sync::atomic::Ordering::Release);
    let first = start(actor, device);
    let second = actor.playback.pending_next.unwrap();
    let output = device.lock().unwrap().streams.last().unwrap().1.clone();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !actor
        .engine
        .position()
        .is_some_and(|p| p.entry.0 == second && p.frame > 0)
    {
        assert!(Instant::now() < deadline, "B did not become audible");
        ready(actor);
        output.render_f32(256);
    }
    assert_eq!(actor.playback.playing.as_ref().unwrap().instance, first);
}

fn hold_gapless_start(
    actor: &mut Actor,
    device: &Mutex<Device>,
    catalog: &CatalogStub,
) -> EngineEvent {
    drive_gapless_boundary(actor, device, catalog);
    let second = actor.playback.pending_next.unwrap();
    loop {
        let event = actor
            .engine_events
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        if matches!(&event, EngineEvent::TrackStarted { entry, .. } if entry.0 == second) {
            return event;
        }
        actor.on_engine(event);
    }
}

#[test]
fn stopped_navigation_selection_retires_residue_without_explicit_stop() {
    let (mut actor, device, catalog) = actor();
    let delayed = hold_gapless_start(&mut actor, &device, &catalog);
    let target = actor.playback.queue.entries()[1].id;
    actor.finish_playing(EndReason::Skipped, &Origin::Ui);
    actor.engine.stop();
    actor.playback.state = PlayState::Stopped;
    // Simulate a Stopped entry point that left ended and prefetched instances behind.
    assert!(!actor.playback.instances.is_empty());
    let opens = device.lock().unwrap().open_attempts;
    let history = catalog.plays.lock().unwrap().len();
    actor.skip_to(target, &Origin::Ui);
    assert!(
        actor.playback.instances.is_empty()
            && actor.playback.playing.is_none()
            && actor.playback.pending_next.is_none()
    );
    let retired = match &delayed {
        EngineEvent::TrackStarted { entry, .. } => *entry,
        _ => unreachable!(),
    };
    actor.on_engine(delayed);
    actor.on_engine(EngineEvent::EndOfQueue { entry: retired });
    assert_eq!(actor.playback.state, PlayState::Stopped);
    assert_eq!(actor.snapshot().current.unwrap().entry, target);
    assert_eq!(actor.playback.resume_at, Some((target, 0)));
    assert_eq!(device.lock().unwrap().open_attempts, opens);
    assert_eq!(catalog.plays.lock().unwrap().len(), history);
    assert!(!actor.engine.is_active());
}

#[test]
fn stopped_navigation_failed_play_budget_commits_audible_prefetch_once() {
    let (mut actor, device, catalog) = actor();
    drive_gapless_boundary(&mut actor, &device, &catalog);
    let instance = actor.playback.pending_next.unwrap();
    let heard = actor.heard_ms(instance);
    assert!(heard > 0);
    let opens = device.lock().unwrap().open_attempts;
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let tracks: Vec<_> = (10..15).map(TrackId).collect();
    for track in &tracks {
        catalog.uris.lock().unwrap().insert(
            *track,
            MediaUri::from_path(dir.path().join(format!("missing-{}.flac", track.0))).unwrap(),
        );
    }
    actor
        .apply_command(
            Command::Play {
                context: PlayContext::Tracks {
                    tracks: tracks.clone(),
                },
                start: 0,
                start_track: None,
            },
            &Origin::Ui,
            None,
            None,
        )
        .unwrap();
    assert_retired_prefetch(&mut actor, &device, &catalog, instance, heard, opens);
    let plays = catalog.plays.lock().unwrap();
    assert_eq!(plays.len(), 7);
    assert_eq!(
        (plays[0].track, plays[0].ended),
        (TrackId(1), EndReason::Skipped)
    );
    for track in tracks {
        let rows: Vec<_> = plays.iter().filter(|p| p.track == track).collect();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].ended, rows[0].played_ms), (EndReason::Failed, 0));
    }
}

#[test]
fn stopped_navigation_failure_without_successor_commits_audible_prefetch_once() {
    let (mut actor, device, catalog) = actor();
    drive_gapless_boundary(&mut actor, &device, &catalog);
    let first = actor.playback.playing.as_ref().unwrap().instance;
    let instance = actor.playback.pending_next.unwrap();
    let heard = actor.heard_ms(instance);
    assert!(heard > 0);
    let opens = device.lock().unwrap().open_attempts;
    actor.playback.queue.clear(None).unwrap();
    // Deliver an older current-instance failure before the queued real B start.
    actor.on_engine(EngineEvent::TrackFailed {
        entry: mpmp_engine::EntryId(first),
        reason: "injected current decode failure".into(),
    });
    assert_retired_prefetch(&mut actor, &device, &catalog, instance, heard, opens);
    let plays = catalog.plays.lock().unwrap();
    assert_eq!(plays.len(), 2);
    assert_eq!(
        (plays[0].track, plays[0].ended),
        (TrackId(1), EndReason::Failed)
    );
}

fn assert_retired_prefetch(
    actor: &mut Actor,
    device: &Mutex<Device>,
    catalog: &CatalogStub,
    instance: u64,
    heard: u64,
    opens: usize,
) {
    assert_eq!(actor.playback.state, PlayState::Stopped);
    assert!(actor.playback.playing.is_none() && actor.playback.instances.is_empty());
    assert!(actor.playback.pending_next.is_none() && actor.output.output_at.is_none());
    assert!(!actor.engine.is_active());
    assert_eq!(actor.heard_ms(instance), heard);
    assert_eq!(device.lock().unwrap().open_attempts, opens);
    assert!(!device.lock().unwrap().streams.last().unwrap().1.is_open());
    {
        let plays = catalog.plays.lock().unwrap();
        let rows: Vec<_> = plays.iter().filter(|p| p.track == TrackId(2)).collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].ended, rows[0].played_ms),
            (EndReason::Skipped, heard)
        );
    }
    let history = catalog.plays.lock().unwrap().len();
    for event in actor.engine_events.try_iter().collect::<Vec<_>>() {
        actor.on_engine(event);
    }
    actor.poll_output();
    assert!(!actor.stop(&Origin::Ui));
    assert_eq!(catalog.plays.lock().unwrap().len(), history);
    assert_eq!(device.lock().unwrap().open_attempts, opens);
    assert_eq!(actor.playback.state, PlayState::Stopped);
}

#[test]
fn stopped_navigation_tail_next_commits_queued_real_start_and_heard_before_retirement() {
    let (mut actor, device, catalog) = actor();
    drive_gapless_boundary(&mut actor, &device, &catalog);
    let b = actor.playback.queue.entries()[1].id;
    let instance = actor.playback.pending_next.unwrap();
    let heard = actor.heard_ms(instance);
    assert!(heard > 0);
    let x = actor
        .playback
        .queue
        .enqueue(&[TrackId(3)], InsertAt::End, None)
        .unwrap()[0];
    actor.playback.queue.move_entry(x, 0, None).unwrap();
    actor
        .apply_command(
            Command::Move {
                entry: b,
                to: 0,
                expected_revision: None,
            },
            &Origin::Ui,
            None,
            None,
        )
        .unwrap();
    actor
        .apply_command(Command::Next, &Origin::Ui, None, None)
        .unwrap();
    assert_eq!(actor.playback.state, PlayState::Stopped);
    assert_eq!(actor.playback.queue.current(), Some(b));
    assert!(actor.playback.instances.is_empty() && actor.playback.playing.is_none());
    let plays = catalog.plays.lock().unwrap();
    assert_eq!(
        plays.iter().map(|p| (p.track, p.ended)).collect::<Vec<_>>(),
        [
            (TrackId(1), EndReason::Completed),
            (TrackId(2), EndReason::Skipped)
        ]
    );
    assert_eq!(plays[1].played_ms, heard);
}

#[test]
fn stopped_navigation_tail_next_retires_delayed_real_start_before_device_follow() {
    let (mut actor, device, catalog) = actor();
    let delayed = hold_gapless_start(&mut actor, &device, &catalog);
    let a = actor.playback.queue.entries()[0].id;
    let b = actor.playback.queue.entries()[1].id;
    let x = actor
        .playback
        .queue
        .enqueue(&[TrackId(3)], InsertAt::End, None)
        .unwrap()[0];
    actor.playback.queue.move_entry(x, 0, None).unwrap();
    actor
        .apply_command(
            Command::Move {
                entry: b,
                to: 0,
                expected_revision: None,
            },
            &Origin::Ui,
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        actor
            .playback
            .queue
            .entries()
            .iter()
            .map(|e| e.id)
            .collect::<Vec<_>>(),
        [b, x, a]
    );
    assert_eq!(actor.playback.queue.current(), Some(a));
    let (tx, events) = crossbeam_channel::unbounded();
    actor.publication.subscribers.push(Subscriber {
        tx,
        lagging: false,
        ui: false,
        positions: true,
    });
    actor
        .apply_command(Command::Next, &Origin::Ui, None, None)
        .unwrap();
    assert_eq!(actor.playback.state, PlayState::Stopped);
    assert!(actor.playback.instances.is_empty());
    actor
        .apply_command(Command::Previous, &Origin::Ui, None, None)
        .unwrap();
    let chosen = actor.snapshot();
    assert_eq!(chosen.current.as_ref().unwrap().entry, x);
    assert_eq!(chosen.current.as_ref().unwrap().track.title, "song 3");
    assert_eq!(chosen.position.unwrap().position_ms, 0);
    assert_eq!(chosen.queue.current_index, Some(1));
    let opens = device.lock().unwrap().open_attempts;
    let history = catalog.plays.lock().unwrap().len();
    events.try_iter().for_each(drop);
    actor.on_engine(delayed);
    let watcher = device.lock().unwrap().watcher.clone().unwrap();
    watcher.report_route(switch());
    actor.on_output();
    for _ in 0..4 {
        if actor.output.output_policy.next_due().is_some() {
            output_timer(&mut actor);
        }
    }
    assert_eq!(actor.playback.state, PlayState::Stopped);
    assert!(
        actor.playback.playing.is_none()
            && actor.playback.instances.is_empty()
            && actor.output.output_at.is_none()
    );
    assert!(!actor.engine.is_active());
    assert_eq!(actor.snapshot().current, chosen.current);
    assert_eq!(actor.snapshot().position.unwrap().position_ms, 0);
    assert_eq!(actor.playback.queue.current(), Some(x));
    assert_eq!(device.lock().unwrap().open_attempts, opens);
    assert_eq!(catalog.plays.lock().unwrap().len(), history);
    assert!(
        !events
            .try_iter()
            .any(|e| matches!(e.event, Event::TrackStarted { .. }))
    );
}

#[test]
fn stopped_navigation_ended_real_start_cannot_publish_old_metadata() {
    let (mut actor, device, _) = actor();
    let entries = actor
        .playback
        .queue
        .replace(&[TrackId(1), TrackId(2)], None);
    actor.play_entry(entries[1], 0).unwrap();
    let instance = actor.playback.playing.as_ref().unwrap().instance;
    ready(&actor);
    device
        .lock()
        .unwrap()
        .streams
        .last()
        .unwrap()
        .1
        .render_f32(256);
    let delayed = actor
        .engine_events
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(matches!(delayed, EngineEvent::TrackStarted { .. }));
    actor.finish_playing(EndReason::Skipped, &Origin::Ui);
    let ended = actor.playback.instances[&instance].clone();
    assert!(ended.ended.is_some() && !ended.start_announced);
    actor.stop_engine();
    actor
        .apply_command(Command::Previous, &Origin::Ui, None, None)
        .unwrap();
    // Exercise the ended-instance guard independently of session retirement.
    actor.playback.instances.insert(instance, ended);
    let (tx, events) = crossbeam_channel::unbounded();
    actor.publication.subscribers.push(Subscriber {
        tx,
        lagging: false,
        ui: false,
        positions: true,
    });
    actor.on_engine(delayed);
    assert_eq!(actor.playback.state, PlayState::Stopped);
    assert_eq!(actor.snapshot().current.unwrap().entry, entries[0]);
    assert!(actor.playback.playing.is_none());
    assert!(
        !events
            .try_iter()
            .any(|e| matches!(e.event, Event::TrackStarted { .. }))
    );
}

#[test]
fn stopped_navigation_stop_freezing_natural_end_keeps_completed_history_and_null_position() {
    let (mut actor, device, catalog) = actor();
    catalog
        .short_first
        .store(true, std::sync::atomic::Ordering::Release);
    let entry = actor.playback.queue.replace(&[TrackId(1)], None)[0];
    actor.play_entry(entry, 0).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !actor.engine.buffer_status().unwrap().end_of_stream {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let output = device.lock().unwrap().streams.last().unwrap().1.clone();
    output.render_f32(4096);
    output.render_f32(128);
    // No engine events have been consumed by actor. Stop must merge real EOF first.
    assert!(actor.stop(&Origin::Ui));
    assert_eq!(actor.playback.state, PlayState::Stopped);
    assert_eq!(actor.playback.queue.current(), Some(entry));
    assert!(actor.snapshot().position.is_none() && actor.playback.resume_at.is_none());
    assert!(actor.playback.instances.is_empty() && actor.playback.playing.is_none());
    assert!(!output.is_open());
    let plays = catalog.plays.lock().unwrap();
    assert_eq!(plays.len(), 1);
    assert_eq!(plays[0].ended, EndReason::Completed);
    assert_eq!(plays[0].played_ms, 4096 * 1000 / 44_100);
}

#[test]
fn stopped_navigation_natural_end_and_queue_clear_retire_without_changing_completion() {
    for clear in [false, true] {
        let (mut actor, device, catalog) = actor();
        catalog
            .short_first
            .store(true, std::sync::atomic::Ordering::Release);
        let entry = actor.playback.queue.replace(&[TrackId(1)], None)[0];
        actor.play_entry(entry, 0).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !actor.engine.buffer_status().unwrap().end_of_stream {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        if clear {
            actor.playback.queue.clear(None).unwrap();
        }
        let output = device.lock().unwrap().streams.last().unwrap().1.clone();
        output.render_f32(4096);
        output.render_f32(128);
        loop {
            let event = actor
                .engine_events
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            let ended = matches!(event, EngineEvent::EndOfQueue { .. });
            actor.on_engine(event);
            if ended {
                break;
            }
        }
        assert_eq!(actor.playback.state, PlayState::Stopped);
        assert!(
            actor.playback.instances.is_empty()
                && actor.playback.playing.is_none()
                && actor.output.output_at.is_none()
        );
        assert!(actor.snapshot().position.is_none());
        assert!(
            output.is_open(),
            "natural completion still leaves physical output policy unchanged"
        );
        let plays = catalog.plays.lock().unwrap();
        assert_eq!(plays.len(), 1);
        assert_eq!(plays[0].ended, EndReason::Completed);
        assert_eq!(plays[0].played_ms, 4096 * 1000 / 44_100);
    }
}

#[test]
fn stopped_navigation_overflow_resync_preserves_silent_selection() {
    let (mut actor, device, catalog) = actor();
    start(&mut actor, &device);
    actor.stop(&Origin::Ui);
    let opens = device.lock().unwrap().open_attempts;
    let history = catalog.plays.lock().unwrap().len();
    let (tx, events) = crossbeam_channel::bounded(1);
    actor.publication.subscribers.push(Subscriber {
        tx,
        lagging: false,
        ui: false,
        positions: true,
    });
    actor
        .apply_command(Command::Next, &Origin::Ui, None, None)
        .unwrap();
    actor.announce_queue();
    assert!(actor.publication.subscribers[0].lagging);
    events.recv().unwrap();
    actor.resync_lagging();
    let envelope = events.recv().unwrap();
    let Event::Resync { snapshot } = envelope.event else {
        panic!("missing Resync")
    };
    assert_eq!(snapshot.state, PlayState::Stopped);
    assert_eq!(snapshot.current.unwrap().track.id, TrackId(2));
    assert_eq!(snapshot.queue.current_index, Some(1));
    let p = snapshot.position.unwrap();
    assert_eq!(
        (p.entry, p.position_ms, p.rate),
        (actor.playback.queue.current().unwrap(), 0, 0.0)
    );
    assert_eq!(snapshot.seq, envelope.seq);
    assert_eq!(device.lock().unwrap().open_attempts, opens);
    assert_eq!(catalog.plays.lock().unwrap().len(), history);
    assert!(!actor.engine.is_active());
}

#[test]
fn stopped_session_rejects_held_successor_start_and_device_notifications() {
    let (mut actor, device, catalog) = actor();
    let event = hold_gapless_start(&mut actor, &device, &catalog);
    let second = actor.playback.pending_next.unwrap();
    assert!(actor.stop(&Origin::Ui));
    actor.on_engine(event);
    assert!(actor.playback.playing.is_none());
    assert!(actor.playback.instances.is_empty());
    assert!(actor.output.output_at.is_none());
    assert_eq!(actor.playback.state, PlayState::Stopped);
    let attempts = device.lock().unwrap().open_attempts;
    let watcher = device.lock().unwrap().watcher.clone().unwrap();
    for facts in [
        RouteFacts {
            format_changed: true,
            ..Default::default()
        },
        switch(),
    ] {
        watcher.report_route(facts);
        actor.on_output();
        actor.poll_output();
        assert_eq!(actor.playback.state, PlayState::Stopped);
        assert!(!actor.engine.is_active());
        assert!(actor.playback.playing.is_none());
        assert!(actor.output.output_policy.next_due().is_none());
    }
    assert_eq!(device.lock().unwrap().open_attempts, attempts);
    assert!(actor.resume(&Origin::Ui));
    assert_eq!(actor.playback.state, PlayState::Playing);
    assert_ne!(actor.playback.playing.as_ref().unwrap().instance, second);
    ready(&actor);
    let output = device.lock().unwrap().streams.last().unwrap().1.clone();
    assert!(output.render_f32(256).iter().any(|s| *s != 0.0));
    let plays = catalog.plays.lock().unwrap();
    assert_eq!(plays.iter().filter(|p| p.track == TrackId(1)).count(), 1);
}

#[test]
fn notifications_before_first_slow_retry_prepare_silently_and_keep_anchor() {
    for format_changed in [false, true] {
        let (mut actor, device, _) = actor();
        start(&mut actor, &device);
        {
            let mut d = device.lock().unwrap();
            d.fail_open = true;
            d.keep_caps = true;
        }
        observe_switch(&mut actor, &device, false);
        for _ in 0..3 {
            output_timer(&mut actor);
        }
        assert_eq!(actor.playback.state, PlayState::Paused);
        let position = actor.position().unwrap().position_ms;
        actor.epoch -= Duration::from_secs(1);
        assert_eq!(actor.position().unwrap().position_ms, position);
        assert!(actor.output.output_policy.next_due().unwrap() > actor.now_ms());
        {
            let mut d = device.lock().unwrap();
            d.fail_open = false;
            d.watcher.as_ref().unwrap().report_route(if format_changed {
                RouteFacts {
                    format_changed: true,
                    ..Default::default()
                }
            } else {
                RouteFacts {
                    route: Some(DeviceId("ready".into())),
                    ..Default::default()
                }
            });
        }
        actor.on_output();
        assert!(actor.engine.is_active());
        assert!(actor.engine.is_paused());
        assert_eq!(actor.playback.state, PlayState::Paused);
        assert_eq!(actor.position().unwrap().position_ms, position);
        assert!(
            !actor
                .problems
                .iter()
                .any(|p| p.code == ErrorCode::OutputFailed)
        );
        ready(&actor);
        let output = device.lock().unwrap().streams.last().unwrap().1.clone();
        assert!(output.render_f32(256).iter().all(|s| *s == 0.0));
        assert!(actor.resume(&Origin::Ui));
        assert!(output.render_f32(256).iter().any(|s| *s != 0.0));
    }
}

#[test]
fn last_fast_retry_success_keeps_explicit_switch_playing() {
    let (mut actor, device, _) = actor();
    start(&mut actor, &device);
    {
        let mut d = device.lock().unwrap();
        d.fail_open = true;
        d.keep_caps = true;
    }
    observe_switch(&mut actor, &device, false);
    for _ in 0..2 {
        output_timer(&mut actor);
    }
    assert_eq!(actor.playback.state, PlayState::Playing);
    device.lock().unwrap().fail_open = false;
    output_timer(&mut actor);
    assert_eq!(actor.playback.state, PlayState::Playing);
    assert!(!actor.engine.is_paused());
    ready(&actor);
    let output = device.lock().unwrap().streams.last().unwrap().1.clone();
    assert!(output.render_f32(256).iter().any(|s| *s != 0.0));
}

#[test]
fn checkpoint_and_shutdown_pair_engine_successor_with_its_own_position() {
    for shutdown in [false, true] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("state.json");
        let (mut actor, device, catalog) = actor();
        actor.persistence.state_file = Some(StateFile::new(&path));
        drive_gapless_boundary(&mut actor, &device, &catalog);
        let b = actor.playback.queue.entries()[1].id;
        let instance = actor.playback.pending_next.unwrap();
        let position = actor.engine.position().unwrap();
        assert_eq!(position.entry.0, instance);
        // Checkpoints sample the audible cursor; closing commits the final rendered
        // boundary, including the device-queued tail, as required by the design.
        let expected_ms = if shutdown {
            actor.heard_ms(instance)
        } else {
            position.frame * 1000 / u64::from(position.sample_rate)
        };
        assert!(expected_ms > 0);
        assert_eq!(actor.playback.queue.current_index(), Some(0));
        let mut latest_ms = expected_ms;
        if shutdown {
            // Force Shutdown to win select without consuming the engine's queued
            // start. freeze_output still drains the real engine event receiver.
            actor.engine_events = crossbeam_channel::never();
            actor.tx.send(Msg::Shutdown).unwrap();
            actor.run(false);
            assert!(!device.lock().unwrap().streams.last().unwrap().1.is_open());
            let plays = catalog.plays.lock().unwrap();
            assert_eq!(plays.len(), 1);
            assert_eq!(
                (plays[0].track, plays[0].ended),
                (TrackId(1), EndReason::Completed)
            );
        } else {
            actor.persistence.dirty = false;
            actor.persistence.last_save = Instant::now() - SAVE_POSITION_EVERY;
            actor.on_tick();
            // The audible cursor follows the wall clock between callbacks, so the
            // checkpoint may land after the first sample.
            let position = actor.engine.position().unwrap();
            latest_ms = position.frame * 1000 / u64::from(position.sample_rate);
            actor.persistence.state_file.as_mut().unwrap().finish();
        }
        let saved = StateFile::new(&path).load();
        assert_eq!(saved.queue.current_index, Some(1));
        assert_eq!(saved.queue.entries[1].id, b.0);
        assert!((expected_ms..=latest_ms).contains(&saved.queue.position_ms));
        let (mut restored, _, _) = self::actor();
        restored.persistence.state_file = Some(StateFile::new(&path));
        restored.restore(1_000);
        assert_eq!(
            restored.playback.resume_at,
            Some((b, saved.queue.position_ms))
        );
    }
}

#[test]
fn stopped_snapshot_keeps_resume_pair_and_removed_offsets_are_not_reassigned() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("state.json");
    let (mut actor, _, _) = actor();
    actor.persistence.state_file = Some(StateFile::new(&path));
    let entries = actor
        .playback
        .queue
        .replace(&[TrackId(1), TrackId(2)], None);
    actor.playback.queue.set_current(Some(entries[0]));
    actor.playback.resume_at = Some((entries[1], 321));
    actor.save_now();
    actor.persistence.state_file.as_mut().unwrap().finish();
    let saved = StateFile::new(&path).load();
    assert_eq!(saved.queue.current_index, Some(1));
    assert_eq!(saved.queue.position_ms, 321);
    actor.playback.queue.remove(&[entries[1]], None).unwrap();
    actor.save_now();
    actor.persistence.state_file.as_mut().unwrap().finish();
    let saved = StateFile::new(&path).load();
    assert_eq!(saved.queue.current_index, None);
    assert_eq!(saved.queue.position_ms, 0);
}

#[test]
fn stop_navigation_commits_queued_successor_and_resets_only_its_resume_position() {
    let (mut actor, device, catalog) = actor();
    drive_gapless_boundary(&mut actor, &device, &catalog);
    let b = actor.playback.queue.entries()[1].id;
    let position = actor.engine.position().unwrap();
    let expected_ms = actor.heard_ms(position.entry.0);
    assert!(actor.stop(&Origin::Ui));
    assert_eq!(actor.playback.resume_at, Some((b, 0)));
    assert_eq!(actor.playback.queue.current(), Some(b));
    assert!(actor.playback.instances.is_empty());
    assert!(actor.playback.pending_next.is_none());
    assert!(actor.engine_events.is_empty());
    let plays = catalog.plays.lock().unwrap();
    assert_eq!(
        plays.iter().map(|p| (p.track, p.ended)).collect::<Vec<_>>(),
        [
            (TrackId(1), EndReason::Completed),
            (TrackId(2), EndReason::Stopped)
        ]
    );
    assert!(expected_ms > 0);
    assert_eq!(plays[1].played_ms, expected_ms);
    drop(plays);
    assert!(actor.resume(&Origin::Ui));
    assert_eq!(actor.current_track().unwrap().id, TrackId(2));
    ready(&actor);
    let output = device.lock().unwrap().streams.last().unwrap().1.clone();
    assert!(output.render_f32(256).iter().any(|s| *s != 0.0));
}

#[test]
fn sdk_mock_structured_signal_corpus_matches_core() {
    let path = client_parity::core_crate_path(env!("CARGO_MANIFEST_DIR"), "mock-signal-cases.json");
    let mut cases: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let update = std::env::var_os("UPDATE_MOCK_SIGNAL_CASES").is_some();
    let (mut actor, _, _) = actor();
    for case in cases.as_array_mut().unwrap() {
        let t = &case["track"];
        let source = mpmp_engine::SourceFormat {
            codec: if t["lossless"] == true { "flac" } else { "mp3" },
            lossless: t["lossless"].as_bool().unwrap(),
            sample_rate: t["sample_rate"].as_u64().unwrap() as u32,
            channels: t["channels"].as_u64().unwrap() as u16,
            bits_per_sample: t["bits_per_sample"].as_u64().map(|n| n as u32),
            frames: None,
        };
        let output = StreamFormat {
            sample_rate: case["output"]["sample_rate"].as_u64().unwrap() as u32,
            channels: case["output"]["channels"].as_u64().unwrap() as u16,
            sample_type: if case["output"]["sample_format"] == "i16" {
                SampleType::I16
            } else {
                SampleType::F32
            },
        };
        let path = SignalPath::new(&source, output, mpmp_engine::output::ShareMode::Shared);
        actor.audio.volume = case["audio"]["volume"].as_f64().unwrap() as f32;
        actor.audio.muted = case["audio"]["muted"].as_bool().unwrap();
        let settings: ReplayGainSettings =
            serde_json::from_value(case["audio"]["replay_gain"].clone()).unwrap();
        let rg: Option<crate::contract::ReplayGainInfo> = t
            .get("replay_gain")
            .map(|v| serde_json::from_value(v.clone()).unwrap());
        let gain = gain::track_gain(
            rg.as_ref(),
            &settings,
            case["album_context"].as_bool().unwrap(),
        );
        let mut actual =
            serde_json::to_value(actor.with_gain_stages(convert_signal_path(&path), gain)).unwrap();
        // SDK 1.7 retains its frozen signal vocabulary until the phase-5 review.
        for field in [
            "share_mode",
            "output_device_id",
            "backend",
            "sample_integrity",
        ] {
            actual.as_object_mut().unwrap().remove(field);
        }
        if update {
            case["expected"] = actual;
        } else {
            assert_eq!(actual, case["expected"], "{}", case["name"]);
        }
    }
    if update {
        std::fs::write(path, serde_json::to_string_pretty(&cases).unwrap() + "\n").unwrap();
    }
}

#[test]
fn detached_freeze_commits_rendered_gapless_tail_before_retiring_authority() {
    let (mut actor, device, catalog) = actor();
    drive_gapless_boundary(&mut actor, &device, &catalog);
    let successor = actor.playback.pending_next.unwrap();
    let (frozen, thread) = actor.engine.freeze_output_detached();
    actor.engine_events = actor.engine.events();
    actor.reduce_frozen_inner(frozen, actor.playback.state, false);
    assert_eq!(actor.playback.playing.as_ref().unwrap().instance, successor);
    let plays = catalog.plays.lock().unwrap();
    assert_eq!(plays.len(), 1);
    assert_eq!(plays[0].ended, EndReason::Completed);
    drop(plays);
    actor.retire_session();
    actor.playback.state = PlayState::Stopped;
    if let Some(thread) = thread {
        thread.join().unwrap();
    }
    assert!(
        actor.engine_events.try_recv().is_err(),
        "retired decoder must not publish into the new event channel"
    );
}

#[test]
fn shutdown_with_running_job_replaces_disconnected_engine_receiver_and_finishes() {
    let (mut actor, device, _) = actor();
    start(&mut actor, &device);
    actor.scans.jobs.insert(
        7,
        Job {
            id: JobId(7),
            origin: None,
            started: Instant::now(),
            background: None,
            completed: Vec::new(),
            diagnostics: None,
        },
    );
    actor.freeze_for_shutdown();
    assert!(matches!(
        actor.engine_events.try_recv(),
        Err(crossbeam_channel::TryRecvError::Empty)
    ));
    let tx = actor.tx.clone();
    let (done, finished) = crossbeam_channel::bounded(1);
    let thread = std::thread::spawn(move || {
        actor.run(false);
        done.send(()).unwrap();
    });
    tx.send(Msg::Shutdown).unwrap();
    assert!(
        finished.recv_timeout(Duration::from_millis(50)).is_err(),
        "job still owns shutdown lifetime"
    );
    tx.send(Msg::Scan(ScanEvent::Progress {
        scan: 7,
        done: 1,
        total: Some(1),
    }))
    .unwrap();
    tx.send(Msg::Scan(ScanEvent::Finished {
        scan: 7,
        outcome: JobOutcome::Cancelled,
    }))
    .unwrap();
    finished.recv_timeout(Duration::from_secs(2)).unwrap();
    thread.join().unwrap();
}

#[test]
fn retired_decodes_are_reaped_without_waiting_for_blocked_sources() {
    let (mut actor, _, _) = actor();
    for root in 1..=3 {
        let (unblock, blocked) = crossbeam_channel::bounded::<()>(1);
        let handle = std::thread::spawn(move || {
            let _ = blocked.recv();
        });
        let finished = std::thread::spawn(|| {});
        while !finished.is_finished() {
            std::thread::yield_now();
        }
        actor.roots.retired_decodes.insert(
            RootId(root),
            vec![
                Arc::new(Mutex::new(Some(handle))),
                Arc::new(Mutex::new(Some(finished))),
            ],
        );
        actor.reap_retired_decodes();
        assert_eq!(actor.roots.retired_decodes[&RootId(root)].len(), 1);
        unblock.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while actor.roots.retired_decodes.contains_key(&RootId(root)) {
            actor.on_tick();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
    assert!(actor.roots.retired_decodes.is_empty());
}

#[test]
fn root_drain_checks_once_at_expired_deadline_and_panic_always_replies() {
    for panic in [false, true] {
        let (mut actor, _, catalog) = actor();
        catalog
            .panic_drain
            .store(panic, std::sync::atomic::Ordering::SeqCst);
        let root = RootId(1);
        actor.roots.release_start = Some(ReleaseStart {
            audit: None,
            root,
            deadline: Instant::now(),
            changed: true,
            origin: Origin::Ui,
            confirmation: None,
        });
        let (reply, response) = crossbeam_channel::bounded(1);
        actor.finish(
            Answer {
                request_id: 1,
                reply,
                key: None,
                command: Command::ReleaseRoot { root },
                origin: Origin::Ui,
                registered: None,
                joined: vec![],
            },
            Ok(Receipt::default()),
        );
        let msg = actor.rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(&msg, Msg::RootDrained { result, .. } if result.is_err() == panic));
        actor.handle(msg);
        let result = response
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .result;
        if panic {
            assert!(matches!(
                result,
                CommandResult::Err { error }
                    if error.code == ErrorCode::Io && error.details.retryable,
            ));
        } else {
            assert!(matches!(result, CommandResult::Ok(_)));
        }
        assert!(actor.roots.releasing.is_empty());
    }
}

#[test]
fn committed_multi_root_undo_registers_all_recoveries_before_first_notification() {
    let (mut actor, _, catalog) = actor();
    catalog
        .root_undo
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    let _registration = actor.root_access.register(Arc::new(move |path, released| {
        assert!(!released);
        let mut calls = seen.lock().unwrap();
        let first = calls.is_empty();
        calls.push(path.to_owned());
        if first {
            Err(ContractError::failed(
                ErrorCode::Io,
                "injected first-root failure",
            ))
        } else {
            Ok(())
        }
    }));
    let error = actor
        .apply_command(
            Command::UndoAction {
                action: ActionId(1),
            },
            &Origin::Ui,
            None,
            None,
        )
        .unwrap_err();
    assert!(!error.details.retryable);
    assert_eq!(actor.roots.removed_root_notifications.len(), 2);
    assert_eq!(*calls.lock().unwrap(), ["/root/1"]);
    actor.retry_removed_root(RootId(2)).unwrap();
    actor.retry_removed_root(RootId(1)).unwrap();
    assert!(actor.roots.removed_root_notifications.is_empty());
    assert_eq!(*calls.lock().unwrap(), ["/root/1", "/root/2", "/root/1"]);
}

#[test]
fn tag_completion_is_authoritative_before_refresh_and_change_events() {
    let (mut actor, _, catalog) = actor();
    let history = actor.job_history.clone();
    crate::jobs::lock(&history).start(JobId(7), JobKind::TagEdit);
    let inspected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = inspected.clone();
    *catalog.before_tracks.lock().unwrap() = Some(Box::new(move || {
        assert!(matches!(
            crate::jobs::get(&history, JobId(7)),
            crate::contract::QueryResult::Job { job }
                if job.state == crate::contract::JobState::Completed,
        ));
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }));
    let (tx, rx) = crossbeam_channel::unbounded();
    actor.publication.subscribers.push(Subscriber {
        tx,
        lagging: false,
        ui: true,
        positions: false,
    });
    actor.on_tag_job(
        JobId(7),
        TagJobEvent::Finished {
            tracks: vec![TrackId(1)],
            outcome: JobOutcome::Completed {
                report: JobReport::TagEdit {
                    encoding_converted: None,
                    written: Some(vec![TrackId(1)]),
                    written_count: 1,
                    action: Some(ActionId(1)),
                    skipped: vec![],
                    failed: vec![],
                },
            },
        },
    );
    assert!(inspected.load(std::sync::atomic::Ordering::SeqCst));
    let events: Vec<_> = rx.try_iter().map(|e| e.event).collect();
    assert!(matches!(
        events.as_slice(),
        [
            Event::TracksChanged { .. },
            Event::ActionsChanged,
            Event::JobFinished { .. },
            Event::LibraryChanged
        ]
    ));
}

mod playback_limits;

fn named_device(id: &str, persistent: bool, is_default: bool) -> DeviceInfo {
    DeviceInfo {
        id: DeviceId(id.into()),
        name: "Identical display name".into(),
        persistent,
        is_default,
    }
}

fn select_device(actor: &mut Actor, id: &str) -> Result<Receipt, ContractError> {
    refresh_devices(actor);
    let result = actor.apply_command(
        Command::SetOutputDevice {
            selection: crate::contract::OutputDeviceSelection::Device { id: id.into() },
            expected_revision: Some(actor.output.output_settings.settings_revision),
        },
        &Origin::Ui,
        None,
        None,
    );
    finish_discovery(actor);
    result
}

#[test]
fn output_devices_use_identity_and_share_atomic_settings_revision_with_mode() {
    use crate::contract::{
        OutputDeviceSelection, OutputFallbackPolicy, OutputRatePolicy, OutputShareMode, Query,
        QueryResult,
    };
    let (mut actor, device, _) = actor();
    device.lock().unwrap().devices = Some(vec![
        named_device("cpal:a", true, true),
        named_device("cpal:b", true, false),
    ]);
    refresh_devices(&mut actor);
    let QueryResult::OutputDevices { devices, .. } = actor.output_query(Query::OutputDevices)
    else {
        panic!()
    };
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0].name, devices[1].name);
    assert!(select_device(&mut actor, "cpal:b").unwrap().changed);
    assert_eq!(
        actor.output.output_settings.selection,
        OutputDeviceSelection::Device {
            id: "cpal:b".into()
        }
    );
    let before = actor.output.output_settings.clone();
    assert!(!select_device(&mut actor, "cpal:b").unwrap().changed);
    for (mode, rate) in [
        (Some(OutputShareMode::Exclusive), None),
        (None, Some(OutputRatePolicy::Source)),
    ] {
        let error = actor
            .set_output_mode(
                mode,
                Some(OutputFallbackPolicy::Shared),
                rate,
                Some(before.settings_revision),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::OutputModeUnsupported);
        assert_eq!(actor.output.output_settings, before);
    }
    assert_eq!(
        actor
            .set_output_mode(None, Some(OutputFallbackPolicy::Shared), None, Some(0))
            .unwrap_err()
            .code,
        ErrorCode::OutputSettingsConflict
    );
    assert!(
        actor
            .set_output_mode(
                None,
                Some(OutputFallbackPolicy::Shared),
                None,
                Some(before.settings_revision)
            )
            .unwrap()
    );
    let error = actor
        .apply_command(
            Command::SetOutputDevice {
                selection: OutputDeviceSelection::SystemDefault,
                expected_revision: Some(before.settings_revision),
            },
            &Origin::Ui,
            None,
            None,
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::OutputSettingsConflict);
    assert_eq!(
        actor.output.output_settings.settings_revision,
        before.settings_revision + 1
    );
    assert!(!actor.set_output_mode(None, None, None, None).unwrap());
    assert_eq!(
        Command::SetOutputDevice {
            selection: OutputDeviceSelection::SystemDefault,
            expected_revision: None
        }
        .policy(&Origin::MediaControls),
        Policy::Forbidden(ErrorCode::OriginNotAllowed)
    );
    let QueryResult::OutputDeviceCapabilities {
        resolved_device,
        capabilities,
        ..
    } = query_output(
        &mut actor,
        Query::OutputDeviceCapabilities {
            selection: OutputDeviceSelection::SystemDefault,
        },
    )
    else {
        panic!()
    };
    assert_eq!(resolved_device.id, "cpal:a");
    assert_eq!(
        capabilities.exclusive_access,
        crate::contract::CapabilitySupport::Unsupported
    );
    assert_eq!(
        capabilities.source_rate_switch,
        crate::contract::CapabilitySupport::Unsupported
    );
    assert!(!capabilities.physical_route_known);
    assert!(capabilities.formats_complete && capabilities.exclusive_formats.is_empty());
    assert!(actor.engine.output_format().is_none());
}

#[test]
fn selecting_output_preserves_final_frame_instance_queue_and_history() {
    let (mut actor, device, catalog) = actor();
    device.lock().unwrap().devices = Some(vec![
        named_device("cpal:a", true, true),
        named_device("cpal:b", true, false),
    ]);
    let instance = start(&mut actor, &device);
    let queue = actor.playback.queue.revision();
    let entry = actor.current_entry();
    let history = catalog.plays.lock().unwrap().len();
    device.lock().unwrap().close_frames = vec![128, 128];
    let old = device.lock().unwrap().streams.last().unwrap().0.clone();
    let old_watcher = device.lock().unwrap().watcher.clone().unwrap();
    assert!(select_device(&mut actor, "cpal:b").unwrap().changed);
    assert_eq!(actor.playback.playing.as_ref().unwrap().instance, instance);
    assert_eq!(actor.playback.queue.revision(), queue);
    assert_eq!(actor.current_entry(), entry);
    assert_eq!(actor.playback.state, PlayState::Playing);
    let frame = actor.output.unrendered_resume.unwrap().1;
    assert!(frame > 0);
    old.report(OutputEvent::DeviceUnplugged);
    old_watcher.report_route(RouteFacts {
        headphones_disconnected: true,
        ..Default::default()
    });
    actor.on_output();
    assert_eq!(actor.playback.state, PlayState::Playing);
    ready(&actor);
    device
        .lock()
        .unwrap()
        .streams
        .last()
        .unwrap()
        .1
        .render_f32(128);
    assert_eq!(actor.engine.position().unwrap().frame, frame);
    assert_eq!(actor.engine.output_device().unwrap().id.0, "cpal:b");
    assert_eq!(catalog.plays.lock().unwrap().len(), history);
    actor.stop(&Origin::Ui);
    assert_eq!(catalog.plays.lock().unwrap().len(), history + 1);
    assert!(actor.output_state().actual.is_none());
}

#[test]
fn fixed_device_offline_first_attempt_and_rearrival_never_authorize_playback() {
    use crate::contract::{
        OutputDeviceIdStability, OutputDeviceInfo, OutputDeviceSelection, OutputSettings,
    };
    let (mut actor, device, catalog) = actor();
    device.lock().unwrap().devices = Some(vec![named_device("cpal:other", true, true)]);
    actor.restore_output(Some(crate::state_file::SavedOutput {
        settings: OutputSettings {
            selection: OutputDeviceSelection::Device {
                id: "cpal:missing".into(),
            },
            ..Default::default()
        },
        device: Some(OutputDeviceInfo {
            id: "cpal:missing".into(),
            name: "Saved name".into(),
            available: true,
            is_default: false,
            id_stability: OutputDeviceIdStability::Persistent,
        }),
    }));
    assert_eq!(
        actor.output_state().status,
        crate::contract::OutputStatus::Unavailable
    );
    refresh_devices(&mut actor);
    assert!(
        !actor
            .output
            .output_devices
            .iter()
            .find(|d| d.id == "cpal:missing")
            .unwrap()
            .available
    );
    let entry = actor.playback.queue.replace(&[TrackId(1)], None)[0];
    actor.playback.state = PlayState::Paused;
    actor.play_entry(entry, 50).unwrap();
    assert!(actor.output.output_at.is_some());
    let history = catalog.plays.lock().unwrap().len();
    device
        .lock()
        .unwrap()
        .devices
        .as_mut()
        .unwrap()
        .push(named_device("cpal:missing", true, false));
    let watcher = device.lock().unwrap().watcher.clone().unwrap();
    watcher.report_route(RouteFacts {
        route: Some(DeviceId("cpal:missing".into())),
        ..Default::default()
    });
    actor.on_output();
    finish_discovery(&mut actor);
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(actor.engine.is_active());
    ready(&actor);
    assert!(
        device
            .lock()
            .unwrap()
            .streams
            .last()
            .unwrap()
            .1
            .render_f32(1024)
            .iter()
            .all(|s| *s == 0.0)
    );
    assert_eq!(catalog.plays.lock().unwrap().len(), history);
    assert!(actor.output_state().failure.is_none());
}

#[test]
fn fixed_device_disconnect_overrides_default_changes_in_both_orders_and_restores_silently() {
    for loss_first in [false, true] {
        let (mut actor, device, _) = actor();
        device.lock().unwrap().devices = Some(vec![named_device("cpal:a", true, true)]);
        select_device(&mut actor, "cpal:a").unwrap();
        start(&mut actor, &device);
        let stream = device.lock().unwrap().streams.last().unwrap().0.clone();
        let switch = RouteFacts {
            default_changed: true,
            old_route_available: Some(true),
            ..Default::default()
        };
        let loss = RouteFacts {
            headphones_disconnected: true,
            old_route_available: Some(false),
            ..Default::default()
        };
        for fact in if loss_first {
            [loss, switch]
        } else {
            [switch, loss]
        } {
            stream.report_route(fact);
        }
        actor.on_output();
        assert_eq!(actor.playback.state, PlayState::Paused);
        assert!(actor.output_state().failure.is_some());
        assert!(matches!(
            actor.output.output_settings.selection,
            crate::contract::OutputDeviceSelection::Device { .. }
        ));
        device
            .lock()
            .unwrap()
            .watcher
            .as_ref()
            .unwrap()
            .report_route(RouteFacts {
                route: Some(DeviceId("cpal:a".into())),
                ..Default::default()
            });
        actor.on_output();
        finish_discovery(&mut actor);
        assert_eq!(actor.playback.state, PlayState::Paused);
        ready(&actor);
        assert!(
            device
                .lock()
                .unwrap()
                .streams
                .last()
                .unwrap()
                .1
                .render_f32(1024)
                .iter()
                .all(|s| *s == 0.0)
        );
    }
}

#[test]
fn session_output_identity_expires_without_matching_an_identical_name() {
    use crate::contract::{
        OutputDeviceIdStability, OutputDeviceInfo, OutputDeviceSelection, OutputFallbackReason,
        OutputSettings,
    };
    let (mut actor, device, _) = actor();
    device.lock().unwrap().devices = Some(vec![named_device("cpal:new", false, true)]);
    actor.restore_output(Some(crate::state_file::SavedOutput {
        settings: OutputSettings {
            selection: OutputDeviceSelection::Device {
                id: "cpal:old".into(),
            },
            settings_revision: 7,
            ..Default::default()
        },
        device: Some(OutputDeviceInfo {
            id: "cpal:old".into(),
            name: "Identical display name".into(),
            available: true,
            is_default: true,
            id_stability: OutputDeviceIdStability::Session,
        }),
    }));
    assert_eq!(
        actor.output.output_settings.selection,
        OutputDeviceSelection::SystemDefault
    );
    assert_eq!(actor.output.output_settings.settings_revision, 8);
    assert_eq!(
        actor.output.output_fallback.as_ref().unwrap().reason,
        OutputFallbackReason::DeviceIdentityExpired
    );
    assert!(!actor.engine.is_active());
}

#[test]
fn output_device_limit_is_not_silently_truncated_and_format_changes_advance_generation() {
    let (mut actor, device, _) = actor();
    device.lock().unwrap().devices = Some(
        (0..257)
            .map(|i| named_device(&format!("cpal:{i}"), true, i == 0))
            .collect(),
    );
    assert_eq!(
        {
            refresh_devices(&mut actor);
            actor.output.output_devices_error.as_ref().unwrap().code
        },
        ErrorCode::OutOfRange
    );
    device.lock().unwrap().devices = Some(vec![named_device("cpal:a", true, true)]);
    refresh_devices(&mut actor);
    let generation = actor.output.output_devices_generation;
    start(&mut actor, &device);
    device.lock().unwrap().format.as_mut().unwrap().sample_rate = 48000;
    device
        .lock()
        .unwrap()
        .streams
        .last()
        .unwrap()
        .0
        .report_route(RouteFacts {
            format_changed: true,
            ..Default::default()
        });
    actor.on_output();
    assert!(actor.output.output_devices_generation > generation);
    assert_eq!(actor.engine.output_format().unwrap().sample_rate, 48000);
}

#[test]
fn fixed_device_ignores_unrelated_default_notifications_without_rebuilding_or_pausing() {
    let (mut actor, device, _) = actor();
    device.lock().unwrap().devices = Some(vec![named_device("cpal:fixed", true, true)]);
    select_device(&mut actor, "cpal:fixed").unwrap();
    start(&mut actor, &device);
    let opens = device.lock().unwrap().open_attempts;
    device
        .lock()
        .unwrap()
        .streams
        .last()
        .unwrap()
        .0
        .report_route(RouteFacts {
            default_changed: true,
            old_route_available: Some(true),
            route: Some(DeviceId("cpal:unrelated".into())),
            rebuild_required: true,
            ..Default::default()
        });
    actor.on_output();
    assert_eq!(actor.playback.state, PlayState::Playing);
    assert_eq!(device.lock().unwrap().open_attempts, opens);
    assert_eq!(actor.output.output_policy.next_due(), None);
}

#[test]
fn committed_offline_device_receipt_is_success_and_repeating_it_does_not_retry() {
    let (mut actor, device, _) = actor();
    device.lock().unwrap().devices = Some(vec![named_device("cpal:default", true, true)]);
    let instance = start(&mut actor, &device);
    let receipt = select_device(&mut actor, "cpal:offline").unwrap();
    assert!(receipt.changed);
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert_eq!(actor.playback.playing.as_ref().unwrap().instance, instance);
    assert_eq!(
        actor.output_state().failure.unwrap().code,
        ErrorCode::OutputDeviceUnavailable
    );
    let opens = device.lock().unwrap().open_attempts;
    let queries = device.lock().unwrap().format_queries;
    assert!(!select_device(&mut actor, "cpal:offline").unwrap().changed);
    assert_eq!(device.lock().unwrap().open_attempts, opens);
    assert_eq!(device.lock().unwrap().format_queries, queries);
}

#[test]
fn fixed_device_polling_recovers_without_native_events_and_queries_cannot_consume_arrival() {
    use crate::contract::{Query, QueryResult};
    let (mut actor, device, _) = actor();
    device.lock().unwrap().devices = Some(vec![named_device("cpal:a", true, true)]);
    select_device(&mut actor, "cpal:a").unwrap();
    start(&mut actor, &device);
    device.lock().unwrap().devices = Some(vec![]);
    actor.output.output_devices_poll_at = 0;
    actor.poll_output_devices();
    finish_discovery(&mut actor);
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(actor.output_state().failure.is_some());
    device.lock().unwrap().devices = Some(vec![named_device("cpal:a", true, true)]);
    assert!(matches!(
        actor.output_query(Query::OutputDevices),
        QueryResult::OutputDevices { .. }
    ));
    actor.output.output_devices_poll_at = 0;
    actor.poll_output_devices();
    finish_discovery(&mut actor);
    assert!(actor.engine.is_active());
    assert_eq!(actor.playback.state, PlayState::Paused);
    ready(&actor);
    assert!(
        device
            .lock()
            .unwrap()
            .streams
            .last()
            .unwrap()
            .1
            .render_f32(1024)
            .iter()
            .all(|s| *s == 0.0)
    );
}

#[test]
fn output_preferences_persist_through_the_real_state_file_and_session_ids_expire() {
    use crate::contract::{OutputDeviceSelection, OutputFallbackPolicy, OutputFallbackReason};
    for persistent in [false, true] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("state.json");
        let (mut first, device, _) = actor();
        first.persistence.state_file = Some(crate::state_file::StateFile::new(&path));
        device.lock().unwrap().devices = Some(vec![named_device("cpal:saved", persistent, true)]);
        first
            .playback
            .queue
            .replace(&[TrackId(1), TrackId(2)], None);
        first.audio.volume = 0.4;
        select_device(&mut first, "cpal:saved").unwrap();
        first
            .set_output_mode(None, Some(OutputFallbackPolicy::Shared), None, None)
            .unwrap();
        let revision = first.output.output_settings.settings_revision;
        first.save_now();
        first.persistence.state_file.as_mut().unwrap().finish();
        let (mut restored, device, _) = actor();
        // Only a different same-named endpoint exists at restart.
        device.lock().unwrap().devices = Some(vec![named_device("cpal:other", persistent, true)]);
        restored.persistence.state_file = Some(crate::state_file::StateFile::new(&path));
        restored.restore(123);
        assert_eq!(restored.audio.volume, 0.4);
        assert_eq!(restored.playback.queue.entries().len(), 2);
        assert_eq!(
            restored.output.output_settings.fallback,
            OutputFallbackPolicy::Shared
        );
        if persistent {
            assert_eq!(
                restored.output.output_settings.selection,
                OutputDeviceSelection::Device {
                    id: "cpal:saved".into()
                }
            );
            assert_eq!(restored.output.output_settings.settings_revision, revision);
            assert!(restored.output_state().failure.is_some());
        } else {
            assert_eq!(
                restored.output.output_settings.selection,
                OutputDeviceSelection::SystemDefault
            );
            assert_eq!(
                restored.output.output_fallback.unwrap().reason,
                OutputFallbackReason::DeviceIdentityExpired
            );
        }
    }
}

// Only explicit test synchronization drives discoveries; query paths never do.
fn finish_discovery(actor: &mut Actor) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while actor.output.output_discovery_busy.is_some() {
        actor.poll_output_reads();
        assert!(Instant::now() < deadline, "discovery did not finish");
        std::thread::yield_now();
    }
}

fn query_output(actor: &mut Actor, query: crate::contract::Query) -> crate::contract::QueryResult {
    let (reply, result) = crossbeam_channel::bounded(1);
    actor.handle_output_query(query, reply);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        actor.poll_output_reads();
        if let Ok(result) = result.try_recv() {
            return result;
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
}

fn refresh_devices(actor: &mut Actor) {
    actor.refresh_output_devices().unwrap();
    finish_discovery(actor);
}

#[test]
fn blocked_discovery_cannot_delay_disconnect_pause_close_or_cached_queries() {
    use crate::contract::{Query, QueryResult};
    for fixed in [false, true] {
        let (mut actor, device, _) = actor();
        device.lock().unwrap().devices = Some(vec![named_device("cpal:a", true, true)]);
        refresh_devices(&mut actor);
        if fixed {
            select_device(&mut actor, "cpal:a").unwrap();
        }
        start(&mut actor, &device);
        let (entered, started) = crossbeam_channel::bounded(1);
        let (release, blocked) = crossbeam_channel::bounded(1);
        device.lock().unwrap().discovery_hook = Some(Arc::new(move || {
            entered.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        }));
        actor.refresh_output_devices().unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        sink(&device).report(OutputEvent::DeviceUnplugged);
        sink(&device).report_route(RouteFacts {
            format_changed: true,
            ..Default::default()
        });
        let generation = actor.output.output_devices_generation;
        let before = Instant::now();
        actor.on_output();
        let elapsed = before.elapsed();
        let paused = actor.playback.state == PlayState::Paused && !actor.engine.is_active();
        assert_eq!(actor.output.output_devices_generation, generation + 1);
        let state = actor.output_query(Query::OutputState);
        let list = actor.output_query(Query::OutputDevices);
        device.lock().unwrap().discovery_hook = None;
        release.send(()).unwrap();
        assert!(
            elapsed < Duration::from_secs(1),
            "disconnect waited for discovery: {elapsed:?}"
        );
        assert!(paused);
        assert!(matches!(state, QueryResult::OutputState { .. }));
        assert!(matches!(list, QueryResult::OutputDevices { .. }));
        finish_discovery(&mut actor);
        assert_eq!(actor.playback.state, PlayState::Paused);
    }
}

#[test]
fn discovery_is_single_flight_coalesces_requests_and_discards_stale_revision_and_epoch() {
    use crate::contract::{OutputFallbackPolicy, Query, QueryResult};
    for change_revision in [false, true] {
        let (mut actor, device, _) = actor();
        device.lock().unwrap().devices = Some(vec![named_device("cpal:a", true, true)]);
        refresh_devices(&mut actor);
        let generation = actor.output.output_devices_generation;
        let calls = device.lock().unwrap().discovery_calls;
        // The blocked observation differs from the accepted cache, so accepting
        // even briefly would be visible before the coalesced fresh result.
        device.lock().unwrap().devices = Some(vec![named_device("cpal:stale", true, true)]);
        let (entered, started) = crossbeam_channel::bounded(1);
        let (release, blocked) = crossbeam_channel::bounded(1);
        device.lock().unwrap().discovery_hook = Some(Arc::new(move || {
            entered.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        }));
        actor.refresh_output_devices().unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        device.lock().unwrap().devices = Some(vec![named_device("cpal:b", true, true)]);
        if change_revision {
            actor
                .set_output_mode(None, Some(OutputFallbackPolicy::Shared), None, None)
                .unwrap();
        } else {
            for _ in 0..100 {
                actor.refresh_output_devices().unwrap();
            }
        }
        for _ in 0..10 {
            assert!(matches!(
                actor.output_query(Query::OutputState),
                QueryResult::OutputState { .. }
            ));
            let QueryResult::OutputDevices { devices, .. } =
                actor.output_query(Query::OutputDevices)
            else {
                panic!()
            };
            assert_eq!(devices[0].id, "cpal:a");
            actor.output.output_devices_poll_at = 0;
            actor.poll_output_devices();
        }
        assert_eq!(device.lock().unwrap().discovery_calls, calls + 1);
        device.lock().unwrap().discovery_hook = None;
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            if let Some(result) = actor
                .output
                .output_discovery_task
                .as_ref()
                .unwrap()
                .try_take()
            {
                break result;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        };
        let (revision, epoch) = actor.output.output_discovery_busy.unwrap();
        actor.output_discovered(revision, epoch, result);
        assert_eq!(
            device.lock().unwrap().committed_devices[0].id.0,
            "cpal:a",
            "stale handles were committed"
        );
        assert_eq!(
            actor.output.output_devices_generation, generation,
            "stale discovery was accepted"
        );
        assert_eq!(actor.output.output_devices[0].id, "cpal:a");
        finish_discovery(&mut actor);
        assert_eq!(device.lock().unwrap().discovery_calls, calls + 2);
        assert_eq!(actor.output.output_devices[0].id, "cpal:b");
    }
}

#[test]
fn native_notifications_drive_discovery_and_fallback_polls_slower_while_idle() {
    let (mut actor, device, _) = actor();
    let baseline = device.lock().unwrap().discovery_calls;
    device.lock().unwrap().native_notifications = true;
    let (tx, rx) = crossbeam_channel::unbounded();
    actor.publication.subscribers.push(Subscriber {
        tx,
        lagging: false,
        ui: true,
        positions: false,
    });
    for state in [PlayState::Stopped, PlayState::Paused, PlayState::Playing] {
        actor.playback.state = state;
        actor.output.output_devices_poll_at = 0;
        actor.poll_output_devices();
        assert!(actor.output.output_discovery_busy.is_none());
        assert_eq!(device.lock().unwrap().discovery_calls, baseline);
    }
    device
        .lock()
        .unwrap()
        .watcher
        .as_ref()
        .unwrap()
        .report_route(RouteFacts {
            devices_changed: true,
            ..Default::default()
        });
    actor.on_output();
    finish_discovery(&mut actor);
    assert_eq!(device.lock().unwrap().discovery_calls, baseline + 1);
    assert!(
        !rx.try_iter()
            .any(|e| matches!(e.event, Event::StateChanged { .. })),
        "inventory refresh is not a transport transition"
    );
    device.lock().unwrap().native_notifications = false;
    for (state, period) in [
        (PlayState::Stopped, 30_000),
        (PlayState::Paused, 30_000),
        (PlayState::Playing, 1000),
    ] {
        actor.playback.state = state;
        actor.output.output_devices_poll_at = 0;
        let before = actor.now_ms();
        actor.poll_output_devices();
        assert!(actor.output.output_devices_poll_at >= before + period);
        finish_discovery(&mut actor);
        let calls = device.lock().unwrap().discovery_calls;
        actor.poll_output_devices();
        assert_eq!(device.lock().unwrap().discovery_calls, calls);
        assert!(actor.output.output_discovery_busy.is_none());
    }
}

#[test]
fn native_confirmed_same_format_migration_updates_actual_signal_path_and_event_without_reopen() {
    use crate::contract::OutputDeviceSelection;
    let (mut actor, device, _) = actor();
    let mut a = named_device("cpal:a", true, true);
    a.name = "Endpoint A".into();
    let mut b = named_device("cpal:b", true, false);
    b.name = "Endpoint B".into();
    device.lock().unwrap().devices = Some(vec![a, b.clone()]);
    refresh_devices(&mut actor);
    start(&mut actor, &device);
    let opens = device.lock().unwrap().open_attempts;
    let (tx, rx) = crossbeam_channel::unbounded();
    actor.publication.subscribers.push(Subscriber {
        tx,
        lagging: false,
        ui: true,
        positions: false,
    });
    let identity = device
        .lock()
        .unwrap()
        .identities
        .last()
        .unwrap()
        .clone()
        .unwrap();
    // Only a native observer can confirm migration. Discovery alone never changes
    // the identity of either an automatic or a Windows endpoint-bound stream.
    device.lock().unwrap().devices = Some(vec![{
        b.is_default = true;
        b.clone()
    }]);
    refresh_devices(&mut actor);
    assert_eq!(actor.output_state().actual.unwrap().device_id, "cpal:a");
    rx.try_iter().for_each(drop);
    assert!(identity.set(Some(b)));
    device
        .lock()
        .unwrap()
        .watcher
        .as_ref()
        .unwrap()
        .report_route(RouteFacts {
            identity_changed: true,
            default_changed: true,
            route: Some(DeviceId("cpal:b".into())),
            old_route_available: Some(true),
            ..Default::default()
        });
    actor.on_output();
    let actual = actor.output_state().actual.unwrap();
    assert_eq!(
        (actual.device_id.as_str(), actual.name.as_str()),
        ("cpal:b", "Endpoint B")
    );
    let path = actor.snapshot().current.unwrap().signal_path.unwrap();
    assert_eq!(path.output_device_id.as_deref(), Some("cpal:b"));
    assert_eq!(device.lock().unwrap().open_attempts, opens);
    assert_eq!(actor.playback.state, PlayState::Playing);
    assert_eq!(
        actor.output.output_settings.selection,
        OutputDeviceSelection::SystemDefault
    );
    assert!(rx.try_iter().any(|e| matches!(
        e.event,
        Event::OutputChanged { output }
            if output
                .actual
                .as_ref()
                .is_some_and(|a| a.device_id == "cpal:b"),
    )));

    finish_discovery(&mut actor);
}

#[test]
fn endpoint_bound_default_identity_changes_only_after_actual_reconstruction() {
    let (mut actor, device, _) = actor();
    device.lock().unwrap().devices = Some(vec![
        named_device("cpal:a", true, true),
        named_device("cpal:b", true, false),
    ]);
    device.lock().unwrap().activation = Some(Activation {
        default_open_fail: true,
        ..Default::default()
    });
    refresh_devices(&mut actor);
    start(&mut actor, &device);
    assert!(
        device
            .lock()
            .unwrap()
            .activation
            .as_ref()
            .unwrap()
            .pinned_stream
    );
    let opens = device.lock().unwrap().open_attempts;
    device.lock().unwrap().devices = Some(vec![
        named_device("cpal:a", true, false),
        named_device("cpal:b", true, true),
    ]);
    device
        .lock()
        .unwrap()
        .watcher
        .as_ref()
        .unwrap()
        .report_route(RouteFacts {
            default_changed: true,
            rebuild_required: true,
            old_route_available: Some(true),
            route: Some(DeviceId("cpal:b".into())),
            ..Default::default()
        });
    actor.on_output();
    finish_discovery(&mut actor);
    assert_eq!(device.lock().unwrap().open_attempts, opens);
    assert_eq!(actor.output_state().actual.unwrap().device_id, "cpal:a");
    assert_eq!(
        actor
            .snapshot()
            .current
            .unwrap()
            .signal_path
            .unwrap()
            .output_device_id
            .as_deref(),
        Some("cpal:a")
    );
    actor.output_action(policy::Action::Follow);
    assert_eq!(device.lock().unwrap().open_attempts, opens + 1);
    assert_eq!(actor.output_state().actual.unwrap().device_id, "cpal:b");
    assert_eq!(
        actor
            .snapshot()
            .current
            .unwrap()
            .signal_path
            .unwrap()
            .output_device_id
            .as_deref(),
        Some("cpal:b")
    );
    assert_eq!(
        actor.output.output_settings.selection,
        crate::contract::OutputDeviceSelection::SystemDefault
    );
}

#[test]
fn native_and_fallback_discovery_errors_retry_with_backoff_then_stop_or_recover() {
    use crate::contract::{OutputDeviceSelection, Query, QueryResult};
    for native in [false, true] {
        for failures in [1, 10] {
            let (mut actor, device, _) = actor();
            device.lock().unwrap().devices = Some(vec![named_device("cpal:a", true, true)]);
            device.lock().unwrap().native_notifications = native;
            device.lock().unwrap().discovery_failures = failures;
            let calls = device.lock().unwrap().discovery_calls;
            refresh_devices(&mut actor);
            assert!(actor.output.output_devices_error.is_some());
            for (attempt, interval) in [300, 1000, 3000].into_iter().enumerate() {
                let (due, index) = actor.output.output_discovery_retry.unwrap();
                assert_eq!(index, attempt);
                assert!(due >= actor.now_ms() + interval - 100);
                actor.epoch = Instant::now() - Duration::from_millis(due - 1);
                actor.poll_output_devices();
                assert!(actor.output.output_discovery_busy.is_none());
                assert_eq!(device.lock().unwrap().discovery_calls, calls + attempt + 1);
                actor.epoch = Instant::now() - Duration::from_millis(due);
                actor.poll_output_devices();
                finish_discovery(&mut actor);
                if failures == 1 {
                    break;
                }
            }
            if failures > 1 {
                assert_eq!(actor.output.output_discovery_retry, Some((u64::MAX, 3)));
                let exhausted = device.lock().unwrap().discovery_calls;
                let due = actor.output.output_devices_poll_at;
                actor.epoch = Instant::now() - Duration::from_millis(due - 1);
                for _ in 0..10 {
                    actor.poll_output_devices();
                }
                assert_eq!(device.lock().unwrap().discovery_calls, exhausted);
                device.lock().unwrap().discovery_failures = 0;
                if native {
                    actor.epoch = Instant::now() - Duration::from_millis(due + 60_000);
                    actor.poll_output_devices();
                    assert!(actor.output.output_discovery_busy.is_none());
                    device
                        .lock()
                        .unwrap()
                        .watcher
                        .as_ref()
                        .unwrap()
                        .report_route(RouteFacts {
                            devices_changed: true,
                            ..Default::default()
                        });
                    actor.on_output();
                } else {
                    actor.epoch = Instant::now() - Duration::from_millis(due);
                    actor.poll_output_devices();
                }
                finish_discovery(&mut actor);
                assert_eq!(device.lock().unwrap().discovery_calls, exhausted + 1);
            }
            assert!(actor.output.output_devices_error.is_none());
            assert!(actor.output.output_discovery_retry.is_none());
            assert!(matches!(
                actor.output_query(Query::OutputDevices),
                QueryResult::OutputDevices { .. }
            ));
            assert!(matches!(
                query_output(
                    &mut actor,
                    Query::OutputDeviceCapabilities {
                        selection: OutputDeviceSelection::SystemDefault
                    }
                ),
                QueryResult::OutputDeviceCapabilities { .. }
            ));
        }
    }
}

#[test]
fn exhausted_discovery_recovers_without_notifications_with_bounded_rounds() {
    use crate::contract::{OutputDeviceSelection, Query, QueryResult};
    for state in [PlayState::Stopped, PlayState::Paused, PlayState::Playing] {
        let (mut actor, device, _) = actor();
        actor.playback.state = state;
        device.lock().unwrap().native_notifications = false;
        device.lock().unwrap().discovery_failures = 8;
        let calls = device.lock().unwrap().discovery_calls;
        refresh_devices(&mut actor);
        for round in 0..2 {
            for attempt in 0..3 {
                let (due, index) = actor.output.output_discovery_retry.unwrap();
                assert_eq!(index, attempt);
                actor.epoch = Instant::now() - Duration::from_millis(due);
                actor.poll_output_devices();
                finish_discovery(&mut actor);
            }
            assert_eq!(actor.output.output_discovery_retry, Some((u64::MAX, 3)));
            let exhausted = device.lock().unwrap().discovery_calls;
            assert_eq!(exhausted, calls + (round + 1) * 4);
            let due = actor.output.output_devices_poll_at;
            assert!(due >= actor.now_ms() + 29_900);
            actor.epoch = Instant::now() - Duration::from_millis(due - 1);
            for _ in 0..10 {
                actor.poll_output_devices();
            }
            assert!(actor.output.output_discovery_busy.is_none());
            assert_eq!(device.lock().unwrap().discovery_calls, exhausted);
            if round == 1 {
                device.lock().unwrap().devices =
                    Some(vec![named_device("cpal:returned", true, true)]);
            }
            actor.epoch = Instant::now() - Duration::from_millis(due);
            actor.poll_output_devices();
            finish_discovery(&mut actor);
            assert_eq!(device.lock().unwrap().discovery_calls, exhausted + 1);
        }
        assert!(actor.output.output_devices_error.is_none());
        assert!(actor.output.output_discovery_retry.is_none());
        assert_eq!(actor.output.output_devices[0].id, "cpal:returned");
        assert!(matches!(
            actor.output_query(Query::OutputDevices),
            QueryResult::OutputDevices { .. }
        ));
        assert_eq!(
            actor.output.output_settings.selection,
            OutputDeviceSelection::SystemDefault
        );
    }
}

#[test]
fn reselecting_after_discovery_exhaustion_retries_without_changing_settings() {
    use crate::contract::OutputDeviceSelection;
    for native in [false, true] {
        let (mut actor, device, _) = actor();
        device.lock().unwrap().native_notifications = native;
        device.lock().unwrap().discovery_failures = 4;
        refresh_devices(&mut actor);
        for _ in 0..3 {
            let (due, _) = actor.output.output_discovery_retry.unwrap();
            actor.epoch = Instant::now() - Duration::from_millis(due);
            actor.poll_output_devices();
            finish_discovery(&mut actor);
        }
        assert_eq!(actor.output.output_discovery_retry, Some((u64::MAX, 3)));
        let revision = actor.output.output_settings.settings_revision;
        let calls = device.lock().unwrap().discovery_calls;
        assert!(
            actor
                .set_output_device(OutputDeviceSelection::SystemDefault, Some(revision + 1))
                .is_err()
        );
        assert!(actor.output.output_discovery_busy.is_none());
        assert!(
            !actor
                .set_output_device(OutputDeviceSelection::SystemDefault, Some(revision))
                .unwrap()
        );
        finish_discovery(&mut actor);
        assert_eq!(device.lock().unwrap().discovery_calls, calls + 1);
        assert!(actor.output.output_devices_error.is_none());
        assert!(actor.output.output_discovery_retry.is_none());
        assert_eq!(actor.output.output_settings.settings_revision, revision);
    }
}

#[test]
fn blocked_reads_do_not_hold_pause_stop_disconnect_or_actor_shutdown() {
    use crate::contract::{OutputDeviceSelection, Query};
    for capabilities in [false, true] {
        for disconnect in [false, true] {
            let (mut actor, device, _) = actor();
            device.lock().unwrap().devices = Some(vec![named_device("cpal:a", true, true)]);
            refresh_devices(&mut actor);
            start(&mut actor, &device);
            let (entered, started) = crossbeam_channel::bounded(1);
            let (cancelled, cancellation_seen) = crossbeam_channel::bounded(1);
            let hook = Arc::new(move |cancel: mpmp_engine::output_task::Cancellation| {
                entered.send(()).unwrap();
                assert!(cancel.wait_cancelled(Duration::from_secs(5)));
                cancelled.send(()).unwrap();
            });
            let (reply, query_result) = crossbeam_channel::bounded(1);
            if capabilities {
                device.lock().unwrap().capability_hook = Some(hook);
                actor.handle_output_query(
                    Query::OutputDeviceCapabilities {
                        selection: OutputDeviceSelection::SystemDefault,
                    },
                    reply,
                );
            } else {
                device.lock().unwrap().discovery_cancel_hook = Some(hook);
                actor.refresh_output_devices().unwrap();
                drop(reply);
            }
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            let tx = actor.tx.clone();
            let published = actor.published.clone();
            let (done, finished) = crossbeam_channel::bounded(1);
            let thread = std::thread::spawn(move || {
                actor.run(false);
                done.send(()).unwrap();
            });
            let command = |command| {
                let (reply, result) = crossbeam_channel::bounded(1);
                tx.send(Msg::Request {
                    request: Request::new(1, command),
                    origin: Origin::Ui,
                    registered: None,
                    undo_guard: None,
                    purge_guard: None,
                    reply,
                })
                .unwrap();
                assert!(matches!(
                    result
                        .recv_timeout(Duration::from_millis(250))
                        .unwrap()
                        .result,
                    CommandResult::Ok(_)
                ));
            };
            command(Command::Pause);
            command(Command::Resume);
            if disconnect {
                device
                    .lock()
                    .unwrap()
                    .watcher
                    .as_ref()
                    .unwrap()
                    .report_route(RouteFacts {
                        headphones_disconnected: true,
                        ..Default::default()
                    });
                let deadline = Instant::now() + Duration::from_secs(1);
                loop {
                    let state = published.load();
                    if state.snapshot.state == PlayState::Paused
                        && state.snapshot.output.as_ref().unwrap().actual.is_none()
                    {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "disconnect waited for driver read"
                    );
                    std::thread::yield_now();
                }
            }
            command(Command::Stop);
            let before = Instant::now();
            tx.send(Msg::Shutdown).unwrap();
            finished.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(before.elapsed() < Duration::from_secs(1));
            cancellation_seen
                .recv_timeout(Duration::from_secs(1))
                .unwrap();
            if capabilities {
                assert!(
                    !matches!(
                        query_result.try_recv(),
                        Ok(crate::contract::QueryResult::OutputDeviceCapabilities { .. })
                    ),
                    "shutdown or stale reads cannot return capabilities"
                );
            }
            thread.join().unwrap();
            drop(tx);
            let weak = Arc::downgrade(&device);
            drop(device);
            assert!(
                weak.upgrade().is_none(),
                "blocked read retained backend cache"
            );
        }
    }
}

#[test]
fn capability_timeout_and_selection_change_reject_late_results_without_preparing_output() {
    use crate::contract::{OutputDeviceSelection, OutputFallbackPolicy, Query, QueryResult};
    for change_revision in [false, true] {
        let (mut actor, device, _) = actor();
        device.lock().unwrap().devices = Some(vec![named_device("cpal:a", true, true)]);
        refresh_devices(&mut actor);
        let opens = device.lock().unwrap().open_attempts;
        let (entered, started) = crossbeam_channel::bounded(1);
        let (release, blocked) = crossbeam_channel::bounded(1);
        device.lock().unwrap().capability_hook = Some(Arc::new(move |_| {
            entered.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        }));
        let (reply, result) = crossbeam_channel::bounded(1);
        actor.handle_output_query(
            Query::OutputDeviceCapabilities {
                selection: OutputDeviceSelection::SystemDefault,
            },
            reply,
        );
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        if change_revision {
            actor
                .set_output_mode(None, Some(OutputFallbackPolicy::Shared), None, None)
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        let value = loop {
            actor.poll_output_reads();
            if let Ok(result) = result.try_recv() {
                break result;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        };
        let QueryResult::Err { error } = value else {
            panic!("stale or timed out read succeeded");
        };
        assert_eq!(error.code, ErrorCode::OutputDeviceUnavailable);
        assert!(error.details.retryable);
        release.send(()).unwrap();
        assert!(actor.output.output_capability_tasks.is_empty());
        actor.poll_output_reads();
        assert_eq!(device.lock().unwrap().open_attempts, opens);
        assert!(actor.engine.output_format().is_none());
    }
}

#[test]
fn fixed_output_keeps_its_stream_while_default_inventory_moves_to_another_device() {
    use crate::contract::{Query, QueryResult};
    let (mut actor, device, _) = actor();
    device.lock().unwrap().devices = Some(vec![
        named_device("cpal:fixed", true, true),
        named_device("cpal:new-default", true, false),
    ]);
    select_device(&mut actor, "cpal:fixed").unwrap();
    start(&mut actor, &device);
    let opens = device.lock().unwrap().open_attempts;
    device.lock().unwrap().devices = Some(vec![
        named_device("cpal:fixed", true, false),
        named_device("cpal:new-default", true, true),
    ]);
    device
        .lock()
        .unwrap()
        .watcher
        .as_ref()
        .unwrap()
        .report_route(RouteFacts {
            devices_changed: true,
            ..Default::default()
        });
    actor.on_output();
    finish_discovery(&mut actor);
    let QueryResult::OutputDevices {
        default_device,
        devices,
        ..
    } = actor.output_query(Query::OutputDevices)
    else {
        panic!()
    };
    assert_eq!(default_device.as_deref(), Some("cpal:new-default"));
    assert!(
        !devices
            .iter()
            .find(|d| d.id == "cpal:fixed")
            .unwrap()
            .is_default
    );
    assert_eq!(actor.output_state().actual.unwrap().device_id, "cpal:fixed");
    assert_eq!(actor.playback.state, PlayState::Playing);
    assert_eq!(device.lock().unwrap().open_attempts, opens);
}

#[test]
fn large_cover_fingerprints_cannot_evict_a_recent_enqueue_key() {
    let (mut actor, _, _) = actor();
    let enqueue = Command::Enqueue {
        tracks: vec![TrackId(1)],
        at: InsertAt::End,
        expected_revision: None,
        expected_current: None,
    };
    let key = (Origin::Ui.label(), None, "ordinary-enqueue".to_owned());
    let (tx, rx) = crossbeam_channel::unbounded();
    actor.request(1, Some(key.clone()), enqueue.clone(), Origin::Ui, None, tx);
    let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(first.result, CommandResult::Ok(_)));
    let count = actor.playback.queue.len();
    assert_eq!(count, 1);
    // More than the old 32 MiB retained-image budget, but fewer than 1024 keys.
    let mut image = "A".repeat(1_398_104);
    for index in 0..32 {
        image.replace_range(0..1, if index % 2 == 0 { "B" } else { "C" });
        let command = Command::EditCover {
            tracks: vec![TrackId(index + 1)],
            change: crate::contract::CoverChange::Set {
                image: crate::contract::CoverImageInput {
                    mime: "image/png".into(),
                    data_base64: image.clone(),
                },
            },
        };
        actor.confirmations.idempotency.insert(
            (Origin::Ui.label(), None, format!("cover-{index}")),
            command.clone(),
            Some(first.result.clone()),
        );
        let cached = actor
            .confirmations
            .idempotency
            .get(&(Origin::Ui.label(), None, format!("cover-{index}")))
            .unwrap();
        assert_eq!(cached.0, command_fingerprint(&command));
        let mut different = command.clone();
        if let Command::EditCover {
            change: crate::contract::CoverChange::Set { image },
            ..
        } = &mut different
        {
            image.data_base64.push('A');
        }
        assert_ne!(cached.0, command_fingerprint(&different));
    }
    assert_eq!(actor.confirmations.idempotency.results.len(), 33);
    let (tx, rx) = crossbeam_channel::unbounded();
    actor.request(2, Some(key.clone()), enqueue, Origin::Ui, None, tx);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap().result,
        first.result
    );
    assert_eq!(
        actor.playback.queue.len(),
        count,
        "retry executed the enqueue twice"
    );
    let (tx, rx) = crossbeam_channel::unbounded();
    actor.request(
        3,
        Some(key),
        Command::Clear {
            expected_revision: None,
        },
        Origin::Ui,
        None,
        tx,
    );
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap().result,
        CommandResult::Err { error } if error.code == ErrorCode::IdempotencyKeyReused,
    ));
    assert_eq!(
        command_fingerprint(&Command::SetVolume { volume: -0.0 }),
        command_fingerprint(&Command::SetVolume { volume: 0.0 })
    );
}

#[test]
fn non_finite_requests_are_invalid_before_idempotency_lookup_and_remain_retryable() {
    fn request(actor: &mut Actor, key: &str, command: Command) -> CommandResult {
        let (tx, rx) = crossbeam_channel::unbounded();
        actor.request(
            1,
            Some((Origin::Ui.label(), None, key.into())),
            command,
            Origin::Ui,
            None,
            tx,
        );
        rx.recv_timeout(Duration::from_secs(5)).unwrap().result
    }
    fn replay(preamp_db: Option<f32>, fallback_preamp_db: Option<f32>) -> Command {
        Command::SetReplayGain {
            mode: None,
            preamp_db,
            fallback_preamp_db,
            prevent_clipping: None,
        }
    }
    let (mut actor, _, _) = actor();
    let seed = replay(None, None);
    let first = request(&mut actor, "cached", seed.clone());
    assert!(matches!(first, CommandResult::Ok(_)));
    let volume = request(&mut actor, "volume", Command::SetVolume { volume: 1.0 });
    assert!(matches!(volume, CommandResult::Ok(_)));
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        for command in [
            replay(Some(value), None),
            replay(None, Some(value)),
            Command::SetVolume { volume: value },
        ] {
            for key in ["cached", "volume", "fresh"] {
                assert!(matches!(
                    request(&mut actor, key, command.clone()),
                    CommandResult::Err { error }
                        if error.code == ErrorCode::OutOfRange
                            && error.kind == crate::contract::ErrorKind::Invalid,
                ));
            }
        }
    }
    assert_eq!(
        actor.confirmations.idempotency.results.len(),
        2,
        "invalid inputs consumed a key"
    );
    assert_eq!(request(&mut actor, "cached", seed.clone()), first);
    assert!(matches!(
        request(&mut actor, "fresh", seed),
        CommandResult::Ok(_)
    ));
}

#[test]
fn known_precision_loss_precedes_unverified_transport() {
    let (mut actor, _, _) = actor();
    actor.audio.volume = 1.0;
    actor.audio.muted = false;
    let source = mpmp_engine::SourceFormat {
        codec: "flac",
        lossless: true,
        sample_rate: 44_100,
        channels: 2,
        bits_per_sample: Some(24),
        frames: None,
    };
    for share_mode in [
        mpmp_engine::output::ShareMode::Exclusive,
        mpmp_engine::output::ShareMode::Shared,
    ] {
        let engine_path = SignalPath::new(
            &source,
            StreamFormat {
                sample_rate: 44_100,
                channels: 2,
                sample_type: SampleType::I32,
            },
            share_mode,
        );
        for valid in [Some(16), None, Some(24)] {
            let mut path = convert_signal_path(&engine_path);
            path.stages.push(StageInfo {
                output_format: Some(SignalFormat {
                    sample_rate: 44_100,
                    channels: 2,
                    bits_per_sample: Some(32),
                    valid_bits_per_sample: valid,
                    sample_format: Some(SampleFormat::I32),
                }),
                ..Default::default()
            });
            let path = actor.with_gain_stages(path, None);
            if valid == Some(16) {
                assert!(matches!(path.bit_perfect, BitPerfectInfo::No { .. }));
                assert!(path.reason_codes.contains(&SignalReasonCode::PrecisionLoss));
                assert!(
                    !path
                        .reason_codes
                        .contains(&SignalReasonCode::PathUnverified)
                );
                assert!(!path.reason_codes.contains(&SignalReasonCode::SharedMixer));
            } else {
                assert!(matches!(path.bit_perfect, BitPerfectInfo::Uncertain { .. }));
                assert!(!path.reason_codes.contains(&SignalReasonCode::PrecisionLoss));
            }
        }
    }
}
