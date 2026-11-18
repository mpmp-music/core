//! Persistence scenarios.
use super::*;

#[test]
fn state_survives_a_restart() {
    let path = std::env::temp_dir().join(format!("core-restart-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let (entries, revision) = {
        let mut h = Harness::new(Some(path.clone()));
        h.ok(play_at(album(), 1));
        h.until(|e| matches!(e, Event::TrackStarted { .. }));
        h.ok(Command::SetRepeat {
            mode: RepeatMode::All,
        });
        h.ok(Command::Pause);
        let (revision, _, entries) = h.queue_page();
        (entries, revision)
    };
    let mut h = Harness::new(Some(path.clone()));
    let snapshot = h.snapshot();
    assert_eq!(snapshot.state, PlayState::Stopped);
    assert_eq!(snapshot.queue.entry_count, 3);
    assert_eq!(snapshot.queue.current_index, Some(1));
    assert_eq!(snapshot.repeat, RepeatMode::All);
    assert_eq!(snapshot.current.as_ref().unwrap().track.id, TrackId(2));
    // Entry ids are kept; the revision only grows; new ids are new.
    let (after, _, restored) = h.queue_page();
    assert!(after > revision, "{after:?} after {revision:?}");
    assert_eq!(
        restored.iter().map(|e| e.entry).collect::<Vec<_>>(),
        entries.iter().map(|e| e.entry).collect::<Vec<_>>()
    );
    let added = h.ok(Command::Enqueue {
        tracks: vec![TrackId(1)],
        at: InsertAt::End,
        expected_revision: None,
        expected_current: None,
    });
    assert!(entries.iter().all(|e| e.entry < added.created_entries[0]));
    h.ok(Command::Resume);
    let started = h.until(|e| matches!(e, Event::TrackStarted { .. }));
    let Event::TrackStarted { track, .. } = started else {
        unreachable!()
    };
    assert_eq!(track.id, TrackId(2));
    drop(h);
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["version"], 1, "{saved}");
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn ids_and_revisions_start_from_the_clock_without_a_state_file() {
    let before = unix_ms();
    let mut h = Harness::new(None);
    let receipt = h.ok(play(tracks(&[1])));
    assert!(receipt.queue_revision.0 >= before, "{receipt:?}");
    assert!(receipt.created_entries[0].0 >= before, "{receipt:?}");
    assert!(receipt.queue_revision.0 < 1 << 53);
}

#[test]
fn c4_stop_navigation_persists_the_track_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut h = Harness::new(Some(path.clone()));
    let entry = h.ok(play(album())).created_entries[0];
    let deadline = Instant::now() + TIMEOUT;
    let mut audible = 0;
    while audible < 10 {
        if h.output.render_f32(128).iter().any(|s| *s != 0.0) {
            audible += 1;
        }
        assert!(Instant::now() < deadline, "no audio rendered");
        std::thread::yield_now();
    }
    h.ok(Command::Pause);
    // First observe a nonzero checkpoint, so the initial Play checkpoint at zero
    // cannot be mistaken for Stop's immediate checkpoint below.
    let saved_at = |position: fn(u64) -> bool| {
        loop {
            if let Ok(bytes) = std::fs::read(&path)
                && let Ok(saved) = serde_json::from_slice::<serde_json::Value>(&bytes)
                && saved["queue"]["position_ms"].as_u64().is_some_and(position)
            {
                return saved;
            }
            assert!(Instant::now() < deadline, "checkpoint missing");
            std::thread::yield_now();
        }
    };
    saved_at(|ms| ms > 0);
    h.drain();
    h.ok(Command::Stop);
    // Stop submits immediately; disk completion must not block the core actor.
    let saved = saved_at(|ms| ms == 0);
    assert_eq!(saved["queue"]["current_index"], 0);
    let snapshot = h
        .drain()
        .into_iter()
        .find_map(|e| match e.event {
            Event::StateChanged { snapshot } if snapshot.state == PlayState::Stopped => {
                Some(snapshot)
            }
            _ => None,
        })
        .expect("Stop must announce its snapshot");
    let position = snapshot.position.unwrap();
    assert_eq!(
        (position.entry, position.position_ms, position.rate),
        (entry, 0, 0.0)
    );
    // Stopping again changes nothing and retains zero.
    assert!(!h.ok(Command::Stop).changed);
    assert_eq!(h.snapshot().position.unwrap().position_ms, 0);
}

#[test]
fn volume_and_replay_gain_show_in_the_signal_path_and_persist() {
    let path = std::env::temp_dir().join(format!("core-audio-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut h = Harness::new(Some(path.clone()));
        assert_eq!(h.snapshot().audio.volume, 1.0);
        assert_eq!(h.snapshot().audio.replay_gain.mode, ReplayGainMode::Smart);
        for (bad, kind, code) in [
            (
                Command::SetVolume { volume: 1.5 },
                ErrorKind::Invalid,
                ErrorCode::OutOfRange,
            ),
            (
                Command::SetVolumeMode {
                    mode: VolumeMode::System,
                },
                ErrorKind::Unsupported,
                ErrorCode::NotImplemented,
            ),
            (
                Command::SetReplayGain {
                    mode: None,
                    preamp_db: Some(40.0),
                    fallback_preamp_db: None,
                    prevent_clipping: None,
                },
                ErrorKind::Invalid,
                ErrorCode::OutOfRange,
            ),
        ] {
            let error = h.err(bad);
            assert_eq!((error.kind, error.code), (kind, code), "{error:?}");
        }
        // Setting a value it already has changes nothing.
        assert!(!h.ok(Command::SetVolume { volume: 1.0 }).changed);
        assert!(
            !h.ok(Command::SetVolumeMode {
                mode: VolumeMode::Software
            })
            .changed
        );

        // Untagged files with a fallback pre-amp, at half volume: both stages are listed
        // and the path is no longer bit-perfect.
        h.ok(Command::SetVolume { volume: 0.5 });
        h.ok(replay_gain(Some(ReplayGainMode::Track), Some(-6.0)));
        // A partial update keeps the other settings.
        h.ok(Command::SetReplayGain {
            mode: None,
            preamp_db: None,
            fallback_preamp_db: None,
            prevent_clipping: Some(false),
        });
        let rg = h.snapshot().audio.replay_gain;
        assert_eq!(
            (rg.mode, rg.fallback_preamp_db, rg.prevent_clipping),
            (ReplayGainMode::Track, -6.0, false)
        );
        h.ok(play(album()));
        h.until(|e| matches!(e, Event::TrackStarted { .. }));
        // Its `StateChanged` follows it.
        h.until(|e| matches!(e, Event::StateChanged { .. }));
        let snapshot = h.last_state();
        assert_eq!(stage_names(&snapshot)[..], ["replay gain", "volume"][..]);
        let path_info = snapshot.current.unwrap().signal_path.unwrap();
        assert!(
            path_info.stages[0].detail.contains("-6.00 dB"),
            "{path_info:?}"
        );
        assert!(matches!(path_info.bit_perfect, BitPerfectInfo::No { .. }));

        // Back to unity: RG still explains the missing data, without claiming a change.
        h.ok(Command::SetVolume { volume: 1.0 });
        h.ok(replay_gain(Some(ReplayGainMode::Smart), Some(0.0)));
        let unity = h.snapshot().current.unwrap().signal_path.unwrap();
        let stage = unity
            .stages
            .iter()
            .find(|s| s.name == "replay gain")
            .unwrap();
        assert!(stage.detail.contains("no RG data; fallback +0.00 dB"));
        assert!(!stage.alters_samples);
        assert!(!unity.stages.iter().any(|s| s.name == "volume"));

        // A unity information stage preserves the engine's complete bit-perfect verdict.
        h.ok(replay_gain(Some(ReplayGainMode::Off), None));
        let off = h.snapshot().current.unwrap().signal_path.unwrap();
        assert!(!off.stages.iter().any(|s| s.name == "replay gain"));
        assert_eq!(unity.bit_perfect, off.bit_perfect);
        h.ok(replay_gain(Some(ReplayGainMode::Smart), None));

        h.ok(Command::SetVolume { volume: 0.25 });
        h.ok(Command::SetMuted { muted: true });
        h.ok(Command::Stop);
    }
    let h = Harness::new(Some(path.clone()));
    let audio = h.snapshot().audio;
    assert_eq!((audio.volume, audio.muted), (0.25, true));
    assert!((audio.volume_db.unwrap() + 36.12).abs() < 0.01, "{audio:?}");
    assert!(!audio.replay_gain.prevent_clipping);
    std::fs::remove_file(&path).unwrap();
}
