//! Deterministic playback deadlines and real rendered samples at guarded boundaries.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
struct Clock(AtomicU64);

impl mpmp_engine::SuspendClock for Clock {
    fn includes_suspend(&self) -> bool {
        true
    }
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

fn setup() -> (Actor, Arc<Mutex<Device>>, Arc<CatalogStub>, Arc<Clock>) {
    let clock = Arc::new(Clock::default());
    let (actor, device, catalog) = actor_with_clock(Arc::new(FileResolver), clock.clone());
    (actor, device, catalog, clock)
}

fn command(actor: &mut Actor, command: Command) -> Receipt {
    actor
        .apply_command(command, &Origin::Ui, None, None)
        .unwrap()
}

fn arm(actor: &mut Actor) {
    let entry = actor.current_entry();
    command(
        actor,
        Command::SetStopAfterCurrent {
            enabled: true,
            expected_entry: entry,
        },
    );
}

fn timer(actor: &mut Actor, ms: u64) {
    command(
        actor,
        Command::SetSleepTimer {
            duration_ms: Some(ms),
        },
    );
}

fn subscribe(actor: &mut Actor) -> Receiver<EventEnvelope> {
    let (tx, rx) = crossbeam_channel::unbounded();
    actor.publication.subscribers.push(Subscriber {
        tx,
        lagging: false,
        ui: true,
        positions: true,
    });
    rx
}

fn effects(
    events: &Receiver<EventEnvelope>,
) -> Vec<(PlaybackLimitReason, PlaybackLimitAction, Option<EntryId>)> {
    events
        .try_iter()
        .filter_map(|e| match e.event {
            Event::PlaybackLimitReached {
                reason,
                action,
                entry,
            } => {
                assert_eq!(e.origin, None);
                Some((reason, action, entry))
            }
            _ => None,
        })
        .collect()
}

fn output(device: &Mutex<Device>) -> NullOutputHandle {
    device.lock().unwrap().streams.last().unwrap().1.clone()
}

fn settle(actor: &mut Actor) {
    while let Ok(event) = actor.engine_events.try_recv() {
        actor.on_engine(event);
    }
}

fn constant(value: i16) -> tempfile::NamedTempFile {
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
    for _ in 0..44_100 * 2 {
        wav.write_sample(value).unwrap();
    }
    wav.finalize().unwrap();
    file
}

#[test]
fn rendered_successor_is_discarded_and_repeat_one_cannot_cross_the_boundary() {
    for repeat in [RepeatMode::Off, RepeatMode::One] {
        let (mut actor, device, catalog, _) = setup();
        let first_file = constant(8192);
        let next_file = constant(-16384);
        catalog
            .uris
            .lock()
            .unwrap()
            .insert(TrackId(1), MediaUri::from_path(first_file.path()).unwrap());
        catalog
            .uris
            .lock()
            .unwrap()
            .insert(TrackId(2), MediaUri::from_path(next_file.path()).unwrap());
        catalog.short_first.store(true, Ordering::Release);
        let first = start(&mut actor, &device);
        let next = actor.playback.pending_next.unwrap();
        // A is only 4096 frames. Buffered frames past its remaining tail prove B
        // has already entered the renderer's sample ring before the command.
        ready(&actor);
        assert!(actor.engine.buffer_status().unwrap().buffered_frames > 4096 - 2205 - 256);
        assert!(actor.engine.heard(mpmp_engine::EntryId(next)).is_none());
        command(&mut actor, Command::SetRepeat { mode: repeat });
        let events = subscribe(&mut actor);
        arm(&mut actor);
        timer(&mut actor, 10_000);
        let entry = actor.current_entry().unwrap();
        assert_eq!(actor.limits.stop_after_current, Some(first));
        assert!(actor.playback.pending_next.is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut nonzero_frames = 0;
        while actor.playback.state != PlayState::Stopped {
            assert!(Instant::now() < deadline, "guarded track did not finish");
            if actor.engine.is_active() {
                ready(&actor);
                let samples = output(&device).render_f32(128);
                assert!(
                    samples.iter().all(|s| *s >= 0.0),
                    "audible successor sample"
                );
                nonzero_frames += samples
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .filter(|s| s[0] != 0.0)
                    .count();
            }
            settle(&mut actor);
            std::thread::yield_now();
        }
        assert!(nonzero_frames > 0 && nonzero_frames <= 4096 - 2205 - 256);
        assert_eq!(
            actor
                .engine
                .heard(mpmp_engine::EntryId(first))
                .unwrap()
                .frames,
            4096 - 2205
        );
        assert!(actor.engine.heard(mpmp_engine::EntryId(next)).is_none());
        assert_eq!(actor.playback.queue.repeat(), repeat);
        assert_eq!(actor.playback.resume_at, Some((entry, 0)));
        assert_eq!(actor.playback_limits_info(), PlaybackLimitsInfo::default());
        assert_eq!(
            effects(&events),
            vec![(
                PlaybackLimitReason::EndOfCurrent,
                PlaybackLimitAction::Stop,
                Some(entry)
            )]
        );
        assert_eq!(catalog.plays.lock().unwrap()[0].ended, EndReason::Completed);
    }
}

#[test]
fn actual_rendered_entry_wins_expected_entry_check() {
    let (mut actor, device, _, _) = setup();
    let first = start(&mut actor, &device);
    let old_entry = actor.current_entry().unwrap();
    let second = actor.playback.pending_next.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while actor.engine.heard(mpmp_engine::EntryId(second)).is_none() {
        assert!(Instant::now() < deadline);
        ready(&actor);
        output(&device).render_f32(256);
    }
    assert_eq!(actor.playback.playing.as_ref().unwrap().instance, first);
    let error = actor
        .set_stop_after_current(true, Some(old_entry))
        .unwrap_err();
    assert_eq!(
        (error.code, error.kind),
        (ErrorCode::EntryNotCurrent, ErrorKind::Conflict)
    );
    // Rejection reads the submitted entry without draining or mutating output.
    assert_eq!(
        error.details.actual,
        Some(actor.playback.instances[&second].entry.0)
    );
    assert_eq!(actor.playback.playing.as_ref().unwrap().instance, first);
    // Normal engine delivery, independent of the rejected request, advances core.
    while actor.playback.playing.as_ref().unwrap().instance != second {
        let event = actor
            .engine_events
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        actor.on_engine(event);
    }
    assert_eq!(actor.playback.playing.as_ref().unwrap().instance, second);
    assert!(actor.limits.stop_after_current.is_none());
    arm(&mut actor);
    assert_eq!(actor.limits.stop_after_current, Some(second));
}

#[test]
fn seek_to_end_stops_even_while_paused_and_consumes_both_conditions() {
    for paused in [true, false] {
        let (mut actor, device, catalog, _) = setup();
        start(&mut actor, &device);
        if paused {
            actor.pause();
        }
        arm(&mut actor);
        timer(&mut actor, 5000);
        let entry = actor.current_entry().unwrap();
        let duration = actor.current_track().unwrap().duration_ms.unwrap();
        let events = subscribe(&mut actor);
        actor.seek_to(entry, duration).unwrap();
        assert_eq!(actor.playback.state, PlayState::Stopped);
        assert!(!actor.engine.is_active());
        assert_eq!(actor.playback_limits_info(), PlaybackLimitsInfo::default());
        assert_eq!(
            effects(&events),
            vec![(
                PlaybackLimitReason::EndOfCurrent,
                PlaybackLimitAction::Stop,
                Some(entry)
            )]
        );
        assert_eq!(catalog.plays.lock().unwrap().len(), 1);
    }
}

#[test]
fn pause_seek_resume_and_output_rebuild_keep_the_same_instance_guard() {
    let (mut actor, device, _, _) = setup();
    let instance = start(&mut actor, &device);
    arm(&mut actor);
    timer(&mut actor, 5000);
    actor.pause();
    let entry = actor.current_entry().unwrap();
    actor.seek_to(entry, 100).unwrap();
    actor.close_output();
    assert_eq!(actor.rebuild_output(), RebuildResult::Ready);
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert_eq!(actor.limits.stop_after_current, Some(instance));
    assert!(actor.limits.sleep_deadline.is_some());
    assert!(actor.playback.pending_next.is_none());
    actor.resume(&Origin::Ui);
    assert_eq!(actor.limits.stop_after_current, Some(instance));
    assert!(actor.playback.pending_next.is_none());
}

#[test]
fn manual_replacement_cancels_only_the_boundary_and_failed_navigation_keeps_it() {
    for navigation in [
        Command::Next,
        Command::Previous,
        Command::PlayEntry { entry: EntryId(0) },
    ] {
        let (mut actor, device, _, _) = setup();
        start(&mut actor, &device);
        arm(&mut actor);
        timer(&mut actor, 5000);
        let failed = Command::PlayEntry { entry: EntryId(0) };
        assert_eq!(
            actor
                .apply_command(failed, &Origin::Ui, None, None)
                .unwrap_err()
                .code,
            ErrorCode::EntryNotFound
        );
        assert!(actor.limits.stop_after_current.is_some());
        let navigation = if matches!(navigation, Command::PlayEntry { .. }) {
            Command::PlayEntry {
                entry: actor.playback.queue.entries()[1].id,
            }
        } else {
            navigation
        };
        command(&mut actor, navigation);
        assert!(actor.limits.stop_after_current.is_none());
        assert!(actor.limits.sleep_deadline.is_some());
    }
}

#[test]
fn queue_removal_and_clear_cancel_only_when_the_target_leaves() {
    for clear in [false, true] {
        let (mut actor, device, _, _) = setup();
        start(&mut actor, &device);
        arm(&mut actor);
        timer(&mut actor, 5000);
        let target = actor.current_entry().unwrap();
        let other = actor.playback.queue.entries()[1].id;
        command(
            &mut actor,
            Command::Remove {
                entries: vec![other],
                expected_revision: None,
            },
        );
        assert!(actor.limits.stop_after_current.is_some());
        if clear {
            command(
                &mut actor,
                Command::Clear {
                    expected_revision: None,
                },
            );
        } else {
            command(
                &mut actor,
                Command::Remove {
                    entries: vec![target],
                    expected_revision: None,
                },
            );
        }
        assert!(actor.limits.stop_after_current.is_none());
        assert!(actor.limits.sleep_deadline.is_some());
    }
}

#[test]
fn suspend_expiry_closes_first_wake_callback_before_actor_runs() {
    let (mut actor, device, _, clock) = setup();
    let instance = start(&mut actor, &device);
    arm(&mut actor);
    timer(&mut actor, 1000);
    ready(&actor);
    let before = actor
        .engine
        .heard(mpmp_engine::EntryId(instance))
        .unwrap()
        .frames;
    let wake_output = output(&device);
    let events = subscribe(&mut actor);
    // Simulate suspend, advancing only elapsed time, before actor is scheduled.
    clock.0.store(5000, Ordering::Release);
    assert!(wake_output.render_f32(4096).iter().all(|s| *s == 0.0));
    assert_eq!(
        actor
            .engine
            .heard(mpmp_engine::EntryId(instance))
            .unwrap()
            .frames,
        before
    );
    actor.process_playback_limits();
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(!actor.engine.is_active());
    assert!(actor.output.output_at.is_some());
    assert_eq!(actor.playback_limits_info(), PlaybackLimitsInfo::default());
    let entry = actor.current_entry().unwrap();
    assert_eq!(
        effects(&events),
        vec![(
            PlaybackLimitReason::SleepTimer,
            PlaybackLimitAction::Pause,
            Some(entry)
        )]
    );
    actor.process_playback_limits();
    assert!(effects(&events).is_empty());
    actor.resume(&Origin::Ui);
    assert_eq!(actor.playback.state, PlayState::Playing);
    ready(&actor);
    assert!(output(&device).render_f32(128).iter().any(|s| *s != 0.0));
}

#[test]
fn expiry_in_paused_and_stopped_states_never_starts_output() {
    for stopped in [false, true] {
        let (mut actor, device, _, clock) = setup();
        if !stopped {
            start(&mut actor, &device);
            actor.pause();
            arm(&mut actor);
        }
        timer(&mut actor, 500);
        let events = subscribe(&mut actor);
        let opens = device.lock().unwrap().open_attempts;
        clock.0.store(500, Ordering::Release);
        actor.process_playback_limits();
        assert_eq!(
            actor.playback.state,
            if stopped {
                PlayState::Stopped
            } else {
                PlayState::Paused
            }
        );
        assert_eq!(device.lock().unwrap().open_attempts, opens);
        assert_eq!(effects(&events).len(), 1);
        assert_eq!(actor.playback_limits_info(), PlaybackLimitsInfo::default());
    }
}

#[test]
fn natural_end_at_expiry_wins_and_publishes_updated_state_before_reason() {
    let (mut actor, device, catalog, clock) = setup();
    catalog.short_first.store(true, Ordering::Release);
    start(&mut actor, &device);
    arm(&mut actor);
    timer(&mut actor, 1000);
    let events = subscribe(&mut actor);
    ready(&actor);
    // Finish A's rendered tail, then expire the clock before reducing its EOF.
    output(&device).render_f32(4096);
    clock.0.store(1000, Ordering::Release);
    actor.process_playback_limits();
    assert_eq!(actor.playback.state, PlayState::Stopped);
    let events: Vec<_> = events.try_iter().collect();
    let index = events
        .iter()
        .position(|e| matches!(e.event, Event::PlaybackLimitReached { .. }))
        .unwrap();
    assert!(events[..index].iter().any(|e| matches!(
        &e.event,
        Event::StateChanged { snapshot }
            if snapshot.state == PlayState::Stopped
                && snapshot.playback_limits == Some(PlaybackLimitsInfo::default()),
    )));

    assert!(matches!(
        events[index].event,
        Event::PlaybackLimitReached {
            reason: PlaybackLimitReason::EndOfCurrent,
            action: PlaybackLimitAction::Stop,
            ..
        }
    ));
    assert_eq!(events[index].origin, None);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, Event::PlaybackLimitReached { .. }))
            .count(),
        1
    );
}

#[test]
fn current_decode_failure_stops_without_successor_and_device_loss_preserves_limits() {
    let (mut actor, device, catalog, _) = setup();
    let instance = start(&mut actor, &device);
    arm(&mut actor);
    timer(&mut actor, 5000);
    sink(&device).report(OutputEvent::DeviceUnplugged);
    actor.on_output();
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert_eq!(actor.limits.stop_after_current, Some(instance));
    assert!(actor.limits.sleep_deadline.is_some());
    let entry = actor.current_entry().unwrap();
    let events = subscribe(&mut actor);
    actor.on_engine(EngineEvent::TrackFailed {
        entry: mpmp_engine::EntryId(instance),
        reason: "decode error: injected current failure".into(),
    });
    assert_eq!(actor.playback.state, PlayState::Stopped);
    assert!(!actor.engine.is_active());
    assert_eq!(catalog.plays.lock().unwrap().len(), 1);
    assert_eq!(catalog.plays.lock().unwrap()[0].ended, EndReason::Failed);
    assert_eq!(
        effects(&events),
        vec![(
            PlaybackLimitReason::CurrentFailed,
            PlaybackLimitAction::Stop,
            Some(entry)
        )]
    );
}

#[test]
fn timer_replacement_cancellation_bounds_and_clock_domains() {
    let (mut actor, _, _, clock) = setup();
    for duration_ms in [0, MAX_SLEEP_TIMER_MS + 1, u64::MAX] {
        let error = actor
            .apply_command(
                Command::SetSleepTimer {
                    duration_ms: Some(duration_ms),
                },
                &Origin::Ui,
                None,
                None,
            )
            .unwrap_err();
        assert_eq!(
            (error.code, error.kind),
            (ErrorCode::OutOfRange, ErrorKind::Invalid)
        );
    }
    timer(&mut actor, MAX_SLEEP_TIMER_MS);
    // Snapshot/position time and civil time have no role in the timer's deadline.
    actor.epoch = Instant::now() - Duration::from_secs(100_000);
    assert_eq!(
        actor
            .playback_limits_info()
            .sleep_timer
            .unwrap()
            .remaining_ms,
        MAX_SLEEP_TIMER_MS
    );
    clock.0.store(100, Ordering::Release);
    timer(&mut actor, 500);
    assert_eq!(actor.limits.sleep_deadline, Some(600));
    assert!(command(&mut actor, Command::SetSleepTimer { duration_ms: None }).changed);
    assert!(!command(&mut actor, Command::SetSleepTimer { duration_ms: None }).changed);
    clock.0.store(10_000, Ordering::Release);
    actor.process_playback_limits();
    assert_eq!(actor.playback.state, PlayState::Stopped);
}

#[test]
fn nothing_playing_expected_cancel_and_explicit_stop() {
    let (mut actor, device, _, _) = setup();
    assert_eq!(
        actor
            .set_stop_after_current(true, Some(EntryId(u64::MAX)))
            .unwrap_err()
            .code,
        ErrorCode::OutOfRange
    );
    assert_eq!(
        actor.set_stop_after_current(true, None).unwrap_err().code,
        ErrorCode::NothingPlaying
    );
    assert!(!actor.set_stop_after_current(false, None).unwrap());
    start(&mut actor, &device);
    arm(&mut actor);
    let instance = actor.limits.stop_after_current;
    assert_eq!(
        actor
            .set_stop_after_current(false, Some(EntryId(0)))
            .unwrap_err()
            .code,
        ErrorCode::EntryNotCurrent
    );
    assert_eq!(actor.limits.stop_after_current, instance);
    timer(&mut actor, 1000);
    actor.stop(&Origin::Ui);
    assert_eq!(actor.playback_limits_info(), PlaybackLimitsInfo::default());
    timer(&mut actor, 1000);
    assert!(actor.stop(&Origin::Ui));
    assert_eq!(actor.playback_limits_info(), PlaybackLimitsInfo::default());
}

#[test]
fn real_decode_failure_on_output_recovery_never_starts_a_successor() {
    let (mut actor, device, catalog, _) = setup();
    let file = constant(8192);
    catalog
        .uris
        .lock()
        .unwrap()
        .insert(TrackId(1), MediaUri::from_path(file.path()).unwrap());
    start(&mut actor, &device);
    arm(&mut actor);
    timer(&mut actor, 1000);
    actor.close_output();
    let broken = paused_navigation_broken_wav();
    std::fs::copy(broken.path(), file.path()).unwrap();
    let events = subscribe(&mut actor);
    let entry = actor.current_entry().unwrap();
    actor.rebuild_output();
    let deadline = Instant::now() + Duration::from_secs(5);
    while actor.playback.state != PlayState::Stopped {
        assert!(
            Instant::now() < deadline,
            "real decoder did not report its failure"
        );
        settle(&mut actor);
        std::thread::yield_now();
    }
    assert!(!actor.engine.is_active());
    assert_eq!(catalog.plays.lock().unwrap().len(), 1);
    assert_eq!(catalog.plays.lock().unwrap()[0].ended, EndReason::Failed);
    assert_eq!(
        effects(&events),
        vec![(
            PlaybackLimitReason::CurrentFailed,
            PlaybackLimitAction::Stop,
            Some(entry)
        )]
    );
}

#[test]
fn setting_a_guard_during_device_loss_does_not_retry_or_open_output() {
    let (mut actor, device, _, _) = setup();
    let instance = start(&mut actor, &device);
    sink(&device).report(OutputEvent::DeviceUnplugged);
    actor.on_output();
    device.lock().unwrap().fail_open = true;
    let opens = device.lock().unwrap().open_attempts;
    arm(&mut actor);
    assert_eq!(actor.limits.stop_after_current, Some(instance));
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert!(!actor.engine.is_active());
    assert_eq!(device.lock().unwrap().open_attempts, opens);
}

#[test]
fn boundary_setting_keeps_nonzero_audio_and_never_opens_or_closes_the_device() {
    let (mut actor, device, catalog, _) = setup();
    let file = constant(8192);
    catalog
        .uris
        .lock()
        .unwrap()
        .insert(TrackId(1), MediaUri::from_path(file.path()).unwrap());
    start(&mut actor, &device);
    let live = output(&device);
    ready(&actor);
    live.render_f32(1024); // Finish the initial playback ramp at a nonzero constant.
    let opens = device.lock().unwrap().open_attempts;
    let generation = actor.engine.output_generation();
    let callbacks = actor.engine.callback_count();
    // The backend's close path supplies no callbacks. Any close would cut the last
    // nonzero sample immediately; a delayed open would make that interruption longer.
    assert!(device.lock().unwrap().close_frames.is_empty());
    let (notice, notices) = crossbeam_channel::unbounded();
    let (release, released) = crossbeam_channel::bounded(1);
    device.lock().unwrap().before_open = Some(Box::new({
        let notice = notice.clone();
        move || {
            notice.send(false).unwrap();
            released.recv().unwrap();
        }
    }));
    let task = std::thread::spawn(move || {
        arm(&mut actor);
        let entry = actor.current_entry();
        assert!(
            !command(
                &mut actor,
                Command::SetStopAfterCurrent {
                    enabled: true,
                    expected_entry: entry
                }
            )
            .changed
        );
        assert_eq!(
            actor
                .set_stop_after_current(true, Some(EntryId(0)))
                .unwrap_err()
                .code,
            ErrorCode::EntryNotCurrent
        );
        notice.send(true).unwrap();
        actor
    });
    let completed = notices.recv_timeout(Duration::from_secs(5)).unwrap();
    if !completed {
        release.send(()).unwrap();
    }
    let actor = task.join().unwrap();
    assert!(
        completed,
        "arming or a no-op attempted a delayed device open"
    );
    assert!(
        live.is_open(),
        "setting or rejecting a boundary closed the stream"
    );
    assert_eq!(device.lock().unwrap().open_attempts, opens);
    assert_eq!(actor.engine.output_generation(), generation);
    assert_eq!(actor.engine.callback_count(), callbacks);
    ready(&actor);
    assert!(
        live.render_f32(1024).iter().all(|sample| *sample == 0.25),
        "current audio was reset or faded"
    );
    assert!(device.lock().unwrap().close_samples.is_empty());
}

fn pause_with_delay(actor: &mut Actor, device: &Mutex<Device>) -> (EntryId, u64) {
    let live = output(device);
    ready(actor);
    live.render_with_info(
        mpmp_engine::output::SampleBufMut::F32(&mut [0.0; 2048]),
        &mpmp_engine::output::RenderInfo {
            playback_delay: Some(Duration::from_millis(300)),
        },
    );
    actor.pause();
    let before = actor.snapshot_position().unwrap();
    ready(actor);
    live.render_with_info(
        mpmp_engine::output::SampleBufMut::F32(&mut [0.0; 2048]),
        &mpmp_engine::output::RenderInfo {
            playback_delay: Some(Duration::from_millis(300)),
        },
    );
    assert!(actor.engine.is_silent(), "pause ramp did not finish");
    assert_eq!(
        actor.snapshot_position().unwrap().position_ms,
        before.position_ms
    );
    (before.entry, before.position_ms)
}

#[test]
fn paused_anchor_survives_arming_freezing_and_rebuilding_after_the_full_ramp() {
    let (mut actor, device, _, _) = setup();
    let instance = start(&mut actor, &device);
    let (entry, position) = pause_with_delay(&mut actor, &device);
    let submitted = actor.engine.rendered_position().unwrap();
    assert!(submitted.frame * 1000 / u64::from(submitted.sample_rate) > position);
    arm(&mut actor);
    assert_eq!(
        (
            actor.position().unwrap().entry,
            actor.position().unwrap().position_ms
        ),
        (entry, position)
    );
    actor.close_output();
    assert_eq!(actor.output.output_at, Some((instance, submitted.frame)));
    assert_eq!(actor.position().unwrap().position_ms, position);
    assert_eq!(actor.rebuild_output(), RebuildResult::Ready);
    assert_eq!(actor.position().unwrap().position_ms, position);
    assert_eq!(actor.limits.stop_after_current, Some(instance));
    // Closing again before the rebuilt stream has rendered must retain both anchors.
    actor.freeze_for_shutdown();
    assert_eq!(actor.output.output_at, Some((instance, submitted.frame)));
    assert_eq!(actor.position().unwrap().position_ms, position);
}

#[test]
fn sleep_expiry_on_a_fully_paused_or_stopped_transport_never_touches_output_or_position() {
    for stopped in [false, true] {
        let (mut actor, device, _, clock) = setup();
        start(&mut actor, &device);
        if stopped {
            actor.stop(&Origin::Ui);
            let entry = actor.current_entry().unwrap();
            actor.seek_to(entry, 123).unwrap();
        } else {
            pause_with_delay(&mut actor, &device);
            arm(&mut actor);
        }
        timer(&mut actor, 500);
        let position = actor.snapshot_position().unwrap();
        let live = output(&device);
        let open = live.is_open();
        let generation = actor.engine.output_generation();
        let callbacks = actor.engine.callback_count();
        let opens = device.lock().unwrap().open_attempts;
        let events = subscribe(&mut actor);
        clock.0.store(500, Ordering::Release);
        actor.process_playback_limits();
        let after = actor.snapshot_position().unwrap();
        assert_eq!(
            (after.entry, after.position_ms, after.rate),
            (position.entry, position.position_ms, position.rate)
        );
        assert_eq!(live.is_open(), open);
        assert_eq!(actor.engine.output_generation(), generation);
        assert_eq!(actor.engine.callback_count(), callbacks);
        assert_eq!(device.lock().unwrap().open_attempts, opens);
        assert_eq!(actor.playback_limits_info(), PlaybackLimitsInfo::default());
        assert_eq!(effects(&events).len(), 1);
    }
}

fn request(actor: &mut Actor, command: Command) -> Response {
    let (reply, response) = crossbeam_channel::bounded(1);
    actor.handle(Msg::Request {
        request: Request::new(1, command),
        origin: Origin::Ui,
        registered: None,
        undo_guard: None,
        purge_guard: None,
        reply,
    });
    response.recv_timeout(Duration::from_secs(5)).unwrap()
}

#[test]
fn output_preparation_failures_return_errors_and_keep_the_accepted_instance_and_limits() {
    for navigation in ["play", "play_entry", "next", "previous"] {
        let (mut actor, device, catalog, _) = setup();
        start(&mut actor, &device);
        if navigation == "previous" {
            let second = actor.playback.queue.entries()[1].id;
            actor.skip_to(second, &Origin::Ui);
            ready(&actor);
            output(&device).render_f32(256);
            settle(&mut actor);
        }
        actor.pause();
        arm(&mut actor);
        timer(&mut actor, 5000);
        if matches!(navigation, "play" | "play_entry") {
            // Play preserves an existing stream, so exercise its real output-open
            // failure by starting from an accepted but disconnected paused instance.
            actor.close_output();
        }
        let entry = actor.current_entry().unwrap();
        let instance = actor.playback.playing.as_ref().unwrap().instance;
        let revision = actor.playback.queue.revision();
        let before = actor.snapshot_position().unwrap();
        let accepted: Vec<_> = actor.playback.queue.entries().to_vec();
        let live = output(&device);
        let live_was_open = live.is_open();
        let heard = catalog.plays.lock().unwrap().len();
        let attempts = device.lock().unwrap().open_attempts;
        device.lock().unwrap().fail_open = true;
        device.lock().unwrap().keep_caps = true;
        *catalog.context.lock().unwrap() = vec![TrackId(3), TrackId(4)];
        let target = actor.playback.queue.entries()[1].id;
        let command = match navigation {
            "play" => Command::Play {
                context: PlayContext::Tracks {
                    tracks: vec![TrackId(3), TrackId(4)],
                },
                start: 0,
                start_track: None,
            },
            "play_entry" => Command::PlayEntry { entry: target },
            "next" => Command::Next,
            _ => Command::Previous,
        };
        let response = request(&mut actor, command.clone());
        assert!(
            matches!(
                response.result,
                CommandResult::Err { error } if error.code == ErrorCode::OutputFailed,
            ),
            "{navigation}: expected output_failed"
        );
        assert_eq!(actor.current_entry(), Some(entry), "{navigation}");
        assert_eq!(
            actor.playback.playing.as_ref().unwrap().instance,
            instance,
            "{navigation}"
        );
        assert_eq!(
            actor.limits.stop_after_current,
            Some(instance),
            "{navigation}"
        );
        assert!(actor.limits.sleep_deadline.is_some());
        assert_eq!(actor.playback.queue.entries(), accepted.as_slice());
        assert_eq!(actor.playback.queue.revision(), revision);
        assert_eq!(actor.playback.state, PlayState::Paused);
        assert_eq!(
            actor.snapshot_position().unwrap().position_ms,
            before.position_ms
        );
        assert_eq!(live.is_open(), live_was_open);
        assert_eq!(catalog.plays.lock().unwrap().len(), heard);
        assert_eq!(device.lock().unwrap().open_attempts, attempts + 1);
        // A subsequent successful commit cancels only the instance boundary.
        device.lock().unwrap().fail_open = false;
        let response = request(&mut actor, command);
        assert!(matches!(response.result, CommandResult::Ok(_)));
        assert!(actor.limits.stop_after_current.is_none());
        assert!(actor.limits.sleep_deadline.is_some());
    }
}

#[test]
fn a_guarded_seek_after_a_fully_prefetched_successor_keeps_the_current_track() {
    let (mut actor, device, catalog, _) = setup();
    catalog.short_first.store(true, Ordering::Release);
    start(&mut actor, &device);
    ready(&actor);
    arm(&mut actor);
    let entry = actor.current_entry().unwrap();
    actor.seek_to(entry, 1).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        ready(&actor);
        output(&device).render_f32(128);
        settle(&mut actor);
        if actor.playback.state == PlayState::Stopped {
            break;
        }
        assert!(Instant::now() < deadline, "guarded seek never ended");
    }
    assert_eq!(catalog.plays.lock().unwrap().len(), 1);
    assert_eq!(catalog.plays.lock().unwrap()[0].track, TrackId(1));
    assert_eq!(catalog.plays.lock().unwrap()[0].ended, EndReason::Completed);
}

#[test]
fn a_prefetched_successor_is_truncated_on_a_resampled_stream() {
    let (mut actor, device, catalog, _) = setup();
    device.lock().unwrap().format = Some(StreamFormat {
        sample_rate: 48_000,
        ..format()
    });
    let first = constant(8192);
    let next = constant(-16384);
    catalog
        .uris
        .lock()
        .unwrap()
        .insert(TrackId(1), MediaUri::from_path(first.path()).unwrap());
    catalog
        .uris
        .lock()
        .unwrap()
        .insert(TrackId(2), MediaUri::from_path(next.path()).unwrap());
    catalog.short_first.store(true, Ordering::Release);
    start(&mut actor, &device);
    ready(&actor);
    let target = actor.playback.playing.as_ref().unwrap().instance;
    let successor = actor.playback.pending_next.unwrap();
    let generation = actor.engine.output_generation();
    arm(&mut actor);
    let mut submitted = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while actor.playback.state != PlayState::Stopped {
        ready(&actor);
        let before = actor.engine.buffer_status().unwrap().played_frames;
        let samples = output(&device).render_f32(128);
        let content = (actor.engine.buffer_status().unwrap().played_frames - before) as usize;
        submitted += content;
        assert!(samples[content * 2..].iter().all(|s| *s == 0.0));
        settle(&mut actor);
        assert!(Instant::now() < deadline);
    }
    assert_eq!(actor.engine.output_generation(), generation);
    assert!(submitted > 0);
    let expected_frames = ((4096_u64 - 2205) * 48_000).div_ceil(44_100);
    assert_eq!(
        actor
            .engine
            .heard(mpmp_engine::EntryId(target))
            .unwrap()
            .frames,
        expected_frames
    );
    assert_eq!(submitted as u64 + 256, expected_frames);
    assert!(
        actor
            .engine
            .heard(mpmp_engine::EntryId(successor))
            .is_none()
    );
    assert_eq!(catalog.plays.lock().unwrap().len(), 1);
}

fn start_single(actor: &mut Actor, device: &Mutex<Device>) -> u64 {
    actor.playback.queue = Queue::with_seeds(100, 10);
    let entry = actor.playback.queue.replace(&[TrackId(1)], None)[0];
    actor.play_entry(entry, 50).unwrap();
    ready(actor);
    output(device).render_f32(256);
    let event = actor
        .engine_events
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(matches!(event, EngineEvent::TrackStarted { .. }));
    actor.on_engine(event);
    actor.playback.playing.as_ref().unwrap().instance
}

struct NavigationGate {
    target: MediaUri,
    gate: Mutex<Option<ReopenGate>>,
}

impl SourceResolver for NavigationGate {
    fn resolve(
        &self,
        uri: &MediaUri,
    ) -> Result<mpmp_media_io::ResolvedSource, mpmp_media_io::Error> {
        if uri == &self.target {
            let gate = self.gate.lock().unwrap().take();
            if let Some((entered, release)) = gate {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        }
        FileResolver.resolve(uri)
    }
}

#[test]
fn eof_during_navigation_preparation_cannot_end_the_committed_instance() {
    for navigation in ["next", "play_entry", "play"] {
        let first_file = constant(8192);
        let next_file = constant(-8192);
        let target = MediaUri::from_path(next_file.path()).unwrap();
        let resolver = Arc::new(NavigationGate {
            target: target.clone(),
            gate: Mutex::new(None),
        });
        let (mut actor, device, catalog) = actor_with_source(resolver.clone());
        catalog.short_first.store(true, Ordering::Release);
        catalog
            .uris
            .lock()
            .unwrap()
            .insert(TrackId(1), MediaUri::from_path(first_file.path()).unwrap());
        catalog.uris.lock().unwrap().insert(TrackId(2), target);
        let first = start_single(&mut actor, &device);
        let second_entry = actor
            .playback
            .queue
            .enqueue(&[TrackId(2)], InsertAt::End, None)
            .unwrap()[0];
        arm(&mut actor);
        let (entered, waiting) = crossbeam_channel::bounded(1);
        let (release, released) = crossbeam_channel::bounded(1);
        *resolver.gate.lock().unwrap() = Some((entered, released));
        let live = output(&device);
        let queued = actor.engine_events.clone();
        *catalog.context.lock().unwrap() = vec![TrackId(2)];
        let nav = match navigation {
            "next" => Command::Next,
            "play_entry" => Command::PlayEntry {
                entry: second_entry,
            },
            _ => Command::Play {
                context: PlayContext::Tracks {
                    tracks: vec![TrackId(2)],
                },
                start: 0,
                start_track: None,
            },
        };
        std::thread::scope(|scope| {
            scope.spawn(move || {
                waiting.recv_timeout(Duration::from_secs(5)).unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                // Core is held in B's resolver while real output drains A. Leave the
                // terminal fact queued for the successful navigation to settle.
                while queued.is_empty() {
                    assert!(
                        Instant::now() < deadline,
                        "A did not publish EOF at the gate"
                    );
                    assert!(live.render_f32(128).iter().all(|s| *s >= 0.0));
                    std::thread::yield_now();
                }
                release.send(()).unwrap();
            });
            assert!(matches!(
                request(&mut actor, nav).result,
                CommandResult::Ok { .. }
            ));
        });
        let second = actor.playback.playing.as_ref().unwrap().instance;
        assert_ne!(first, second);
        assert_eq!(actor.playback.state, PlayState::Playing);
        assert_eq!(actor.current_track().unwrap().id, TrackId(2));
        assert!(actor.limits.stop_after_current.is_none());
        assert_eq!(
            catalog
                .plays
                .lock()
                .unwrap()
                .iter()
                .map(|p| (p.track, p.ended))
                .collect::<Vec<_>>(),
            vec![(TrackId(1), EndReason::Completed)]
        );
        // A terminal fact delivered after the handoff must be harmless as well. This
        // assertion specifically rules out relying solely on a pre-commit drain.
        actor.on_engine(EngineEvent::EndOfQueue {
            entry: mpmp_engine::EntryId(first),
        });
        assert_eq!(actor.playback.playing.as_ref().unwrap().instance, second);
        assert_eq!(actor.playback.state, PlayState::Playing);
        let deadline = Instant::now() + Duration::from_secs(5);
        while actor.playback.state != PlayState::Stopped {
            assert!(Instant::now() < deadline, "B did not reach its own EOF");
            ready(&actor);
            output(&device).render_f32(512);
            settle(&mut actor);
            std::thread::yield_now();
        }
        assert!(
            actor
                .engine
                .heard(mpmp_engine::EntryId(second))
                .unwrap()
                .frames
                > 0
        );
        assert_eq!(
            catalog
                .plays
                .lock()
                .unwrap()
                .iter()
                .map(|p| (p.track, p.ended))
                .collect::<Vec<_>>(),
            vec![
                (TrackId(1), EndReason::Completed),
                (TrackId(2), EndReason::Completed)
            ]
        );
    }
}

#[test]
fn guarded_and_unlimited_navigation_skip_bad_candidates_with_identical_facts() {
    for navigation in ["next", "play_entry", "previous", "play"] {
        for paused in [false, true] {
            for missing in [false, true] {
                let mut outcomes = Vec::new();
                let bad = tempfile::NamedTempFile::with_suffix(".flac").unwrap();
                for guarded in [false, true] {
                    let (mut actor, device, catalog, _) = setup();
                    let first = start_single(&mut actor, &device);
                    let entries = actor
                        .playback
                        .queue
                        .enqueue(&[TrackId(2), TrackId(3)], InsertAt::End, None)
                        .unwrap();
                    // Previous selects B, then retries forward to A on media failure.
                    if navigation == "previous" {
                        actor
                            .playback
                            .queue
                            .move_entry(entries[0], 0, None)
                            .unwrap();
                    }
                    let bad_uri = if missing {
                        MediaUri::from_path(bad.path().with_extension("missing")).unwrap()
                    } else {
                        MediaUri::from_path(bad.path()).unwrap()
                    };
                    catalog.uris.lock().unwrap().insert(TrackId(2), bad_uri);
                    if paused {
                        actor.pause();
                    }
                    if guarded {
                        arm(&mut actor);
                    }
                    *catalog.context.lock().unwrap() = vec![TrackId(2), TrackId(3)];
                    let events = subscribe(&mut actor);
                    let nav = match navigation {
                        "next" => Command::Next,
                        "play_entry" => Command::PlayEntry { entry: entries[0] },
                        "previous" => Command::Previous,
                        _ => Command::Play {
                            context: PlayContext::Tracks {
                                tracks: vec![TrackId(2), TrackId(3)],
                            },
                            start: 0,
                            start_track: None,
                        },
                    };
                    let response = request(&mut actor, nav);
                    assert!(
                        matches!(response.result, CommandResult::Ok { .. }),
                        concat!(
                            "{navigation} paused={paused} missing={missing} guarded={guarded}: ",
                            "{response:?}",
                        ),
                        navigation = navigation,
                        paused = paused,
                        missing = missing,
                        guarded = guarded,
                        response = response,
                    );
                    // Previous retries forward from B to A in this queue order.
                    let expected_track = if navigation == "previous" {
                        TrackId(1)
                    } else {
                        TrackId(3)
                    };
                    assert_eq!(actor.current_track().unwrap().id, expected_track);
                    assert_ne!(actor.playback.playing.as_ref().unwrap().instance, first);
                    assert!(actor.limits.stop_after_current.is_none());
                    let expected_state = if paused && matches!(navigation, "next" | "previous") {
                        PlayState::Paused
                    } else {
                        PlayState::Playing
                    };
                    assert_eq!(actor.playback.state, expected_state);
                    let failures: Vec<_> = events
                        .try_iter()
                        .filter_map(|e| match e.event {
                            Event::TrackFailed {
                                entry,
                                code,
                                message,
                                upcoming,
                            } => Some((entry, code, upcoming, message)),
                            _ => None,
                        })
                        .collect();
                    assert_eq!(failures.len(), 1);
                    assert!(!failures[0].2);
                    let history: Vec<_> = catalog
                        .plays
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|p| {
                            (
                                p.track,
                                p.ended,
                                p.context.clone(),
                                p.origin.clone(),
                                p.played_ms,
                            )
                        })
                        .collect();
                    assert_eq!(history[0].0, TrackId(1));
                    assert_eq!(history[0].1, EndReason::Skipped);
                    if expected_state == PlayState::Paused {
                        assert_eq!(
                            history.len(),
                            1,
                            "unheard paused failures have no play record"
                        );
                    } else {
                        assert_eq!(history.len(), 2);
                        assert_eq!(
                            (history[1].0, history[1].1, history[1].4),
                            (TrackId(2), EndReason::Failed, 0)
                        );
                    }
                    outcomes.push((
                        failures,
                        history,
                        actor.playback.state,
                        actor.playback.queue.entries().to_vec(),
                        actor.playback.queue.current(),
                        actor.playback.queue.revision(),
                    ));
                }
                assert_eq!(
                    outcomes[0], outcomes[1],
                    "{navigation} paused={paused} missing={missing}"
                );
            }
        }
    }
}

#[test]
fn guarded_and_unlimited_media_exhaustion_use_the_same_failure_budget() {
    for count in [1, MAX_CONSECUTIVE_FAILURES + 1] {
        for repeat in [RepeatMode::Off, RepeatMode::One] {
            let mut outcomes = Vec::new();
            for guarded in [false, true] {
                let (mut actor, device, catalog, _) = setup();
                start_single(&mut actor, &device);
                let bad = tempfile::NamedTempFile::with_suffix(".flac").unwrap();
                let tracks: Vec<_> = (2..count as i64 + 2).map(TrackId).collect();
                for track in &tracks {
                    catalog
                        .uris
                        .lock()
                        .unwrap()
                        .insert(*track, MediaUri::from_path(bad.path()).unwrap());
                }
                *catalog.context.lock().unwrap() = tracks.clone();
                actor.playback.queue.set_repeat(repeat);
                if guarded {
                    arm(&mut actor);
                }
                let events = subscribe(&mut actor);
                assert!(matches!(
                    request(
                        &mut actor,
                        Command::Play {
                            context: PlayContext::Tracks { tracks },
                            start: 0,
                            start_track: None
                        }
                    )
                    .result,
                    CommandResult::Ok { .. }
                ));
                assert_eq!(actor.playback.state, PlayState::Stopped);
                assert!(actor.limits.stop_after_current.is_none());
                let failures: Vec<_> = events
                    .try_iter()
                    .filter_map(|e| match e.event {
                        Event::TrackFailed {
                            entry,
                            code,
                            upcoming,
                            ..
                        } => Some((entry, code, upcoming)),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    failures.len(),
                    if repeat == RepeatMode::One {
                        1
                    } else {
                        count.min(MAX_CONSECUTIVE_FAILURES)
                    }
                );
                let history: Vec<_> = catalog
                    .plays
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|p| (p.track, p.ended, p.context.clone(), p.played_ms))
                    .collect();
                outcomes.push((
                    failures,
                    history,
                    actor.playback.queue.entries().to_vec(),
                    actor.playback.queue.current(),
                    actor.playback.resume_at,
                ));
            }
            assert_eq!(outcomes[0], outcomes[1], "count={count} repeat={repeat:?}");
        }
    }
}

#[test]
fn media_failure_facts_survive_a_later_output_failure_without_committing_navigation() {
    let (mut actor, device, catalog, _) = setup();
    let first = start_single(&mut actor, &device);
    let entry = actor.current_entry();
    actor.pause();
    arm(&mut actor);
    actor.close_output();
    let bad = tempfile::NamedTempFile::with_suffix(".flac").unwrap();
    catalog
        .uris
        .lock()
        .unwrap()
        .insert(TrackId(2), MediaUri::from_path(bad.path()).unwrap());
    *catalog.context.lock().unwrap() = vec![TrackId(2), TrackId(3)];
    device.lock().unwrap().fail_open = true;
    device.lock().unwrap().keep_caps = true;
    let events = subscribe(&mut actor);
    let context = PlayContext::Tracks {
        tracks: vec![TrackId(2), TrackId(3)],
    };
    let response = request(
        &mut actor,
        Command::Play {
            context: context.clone(),
            start: 0,
            start_track: None,
        },
    );
    assert!(matches!(
        response.result,
        CommandResult::Err { error } if error.code == ErrorCode::OutputFailed,
    ));
    assert_eq!(actor.playback.playing.as_ref().unwrap().instance, first);
    assert_eq!(actor.current_entry(), entry);
    assert_eq!(actor.limits.stop_after_current, Some(first));
    assert_eq!(actor.playback.state, PlayState::Paused);
    assert_eq!(actor.playback.queue.context(), None);
    let failures: Vec<_> = events
        .try_iter()
        .filter_map(|e| match e.event {
            Event::TrackFailed { code, upcoming, .. } => Some((code, upcoming)),
            _ => None,
        })
        .collect();
    assert_eq!(failures, vec![(ErrorCode::UnsupportedFormat, false)]);
    let history = catalog.plays.lock().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(
        (history[0].track, history[0].ended, history[0].played_ms),
        (TrackId(2), EndReason::Failed, 0)
    );
    assert_eq!(history[0].context, Some(context));
}

#[test]
fn cancelled_boundary_continues_like_unlimited_nonzero_gapless_playback() {
    // Identical nonzero files make any inserted silence, fade or truncated tail
    // observable. Compare an ordinary transition with early and late cancellation.
    let mut baseline: Option<Vec<f32>> = None;
    for cancel_at in [None, Some(4096usize), Some(82_000usize)] {
        let (mut actor, device, catalog, _) = setup();
        let first = constant(8192);
        let second = constant(8192);
        for (id, file) in [(TrackId(1), &first), (TrackId(2), &second)] {
            catalog
                .uris
                .lock()
                .unwrap()
                .insert(id, MediaUri::from_path(file.path()).unwrap());
        }
        start(&mut actor, &device);
        let live = output(&device);
        ready(&actor);
        live.render_f32(1024);
        let generation = actor.engine.output_generation();
        let opens = device.lock().unwrap().open_attempts;
        let mut cancelled = false;
        let mut rendered = Vec::new();
        for consumed in (0..100_000).step_by(256) {
            if cancel_at.is_some_and(|at| consumed >= at) && !cancelled {
                arm(&mut actor);
                let expected_entry = actor.current_entry();
                command(
                    &mut actor,
                    Command::SetStopAfterCurrent {
                        enabled: false,
                        expected_entry,
                    },
                );
                assert!(actor.limits.stop_after_current.is_none());
                cancelled = true;
            }
            ready(&actor);
            let samples = live.render_f32(256);
            rendered.extend_from_slice(&samples);
            settle(&mut actor);
        }
        if let Some(baseline) = &baseline {
            let difference = rendered
                .iter()
                .zip(baseline)
                .position(|(actual, expected)| (*actual - *expected).abs() > 0.00001);
            assert!(
                difference.is_none(),
                "cancel_at={cancel_at:?}: audible difference at {difference:?}"
            );
        } else {
            baseline = Some(rendered);
        }
        assert_eq!(actor.engine.output_generation(), generation);
        assert_eq!(device.lock().unwrap().open_attempts, opens);
        assert!(device.lock().unwrap().close_samples.is_empty());
        let playing = actor.playback.playing.as_ref().unwrap().instance;
        assert_eq!(actor.playback.instances[&playing].track.id, TrackId(2));
        assert!(
            actor
                .engine
                .heard(mpmp_engine::EntryId(playing))
                .unwrap()
                .frames
                > 0
        );
    }
}

#[test]
fn limited_clock_reports_precision_without_disabling_core_or_sleep_timer() {
    let (mut actor, _, _) = actor_with_clock(
        Arc::new(FileResolver),
        Arc::new(mpmp_engine::MonotonicPlaybackClock::default()),
    );
    timer(&mut actor, 1000);
    assert_eq!(
        actor
            .playback_limits_info()
            .sleep_timer
            .unwrap()
            .includes_suspend,
        Some(false)
    );
    assert_eq!(actor.playback.state, PlayState::Stopped);
}
