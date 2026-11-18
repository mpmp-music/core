use super::*;

fn begin(h: &mut Harness, index: u32) -> Vec<EntryId> {
    let entries = h.ok(play_at(album(), index)).created_entries;
    h.ok(Command::Stop);
    h.drain();
    entries
}

fn selected(h: &Harness, entry: EntryId, index: u32, position_ms: u64) {
    let s = h.snapshot();
    assert_eq!(s.state, PlayState::Stopped);
    let current = s.current.unwrap();
    assert_eq!(current.entry, entry);
    assert_eq!(current.track.id, TrackId(i64::from(index) + 1));
    assert!(current.signal_path.is_none());
    assert_eq!(s.queue.current_index, Some(index));
    let p = s.position.unwrap();
    assert_eq!((p.entry, p.position_ms, p.rate), (entry, position_ms, 0.0));
    assert!(!h.output.is_open());
}

#[test]
fn selection_announces_metadata_for_every_origin_without_starting() {
    for origin in [Origin::Ui, Origin::MediaControls, Origin::agent("dj")] {
        let mut h = Harness::new(None);
        let entries = begin(&mut h, 0);
        let history = h.catalog.plays.lock().unwrap().len();
        h.ok(Command::SetRepeat {
            mode: RepeatMode::One,
        });
        h.drain();
        let CommandResult::Ok(receipt) = h.run_as(Command::Next, origin.clone()) else {
            panic!()
        };
        assert!(receipt.changed);
        selected(&h, entries[1], 1, 0);
        let events = h.drain();
        assert!(events.iter().any(|e| e.origin.as_ref() == Some(&origin)
            && matches!(&e.event, Event::StateChanged { snapshot }
                if snapshot.state == PlayState::Stopped
                && snapshot.current.as_ref().unwrap().entry == entries[1])));
        assert!(events.iter().any(|e| matches!(e.event,
            Event::QueueChanged { queue } if queue.current_index == Some(1))));
        assert!(events.iter().any(|e| matches!(e.event,
            Event::Position { position } if position.entry == entries[1]
                && position.position_ms == 0 && position.rate == 0.0)));
        assert!(!events.iter().any(|e| matches!(
            e.event,
            Event::TrackStarted { .. } | Event::TrackFailed { .. } | Event::HistoryChanged
        )));
        assert_eq!(h.catalog.plays.lock().unwrap().len(), history);
        assert!(matches!(
            h.run_as(Command::Previous, origin),
            CommandResult::Ok(_)
        ));
        selected(&h, entries[0], 0, 0);
    }
}

#[test]
fn previous_uses_remembered_position_at_three_second_boundary() {
    for position_ms in [2999, 3000, 3001] {
        let mut catalog = MemoryCatalog::standard();
        catalog.tracks.get_mut(&TrackId(2)).unwrap().0.duration_ms = Some(10_000);
        let mut h = Harness::with_catalog(Arc::new(catalog), None);
        let entries = begin(&mut h, 1);
        h.ok(seek(position_ms));
        assert!(h.ok(Command::Previous).changed);
        if position_ms >= 3000 {
            selected(&h, entries[1], 1, 0);
            // The restart branch only changes the resume position, as before.
            assert!(!h.drain().iter().any(|e| matches!(
                e.event,
                Event::StateChanged { .. } | Event::QueueChanged { .. }
            )));
            h.ok(Command::Previous);
        }
        selected(&h, entries[0], 0, 0);
        h.ok(Command::Previous);
        selected(&h, entries[0], 0, 0);
    }
}

#[test]
fn repeated_next_resumes_only_last_selection_at_zero() {
    let mut h = Harness::new(None);
    let entries = begin(&mut h, 0);
    let history = h.catalog.plays.lock().unwrap().len();
    h.ok(Command::Next);
    h.ok(seek(500));
    h.ok(Command::Next);
    selected(&h, entries[2], 2, 0);
    assert_eq!(
        h.err(Command::Seek {
            entry: Some(entries[1]),
            position_ms: 0
        })
        .code,
        ErrorCode::EntryNotCurrent
    );
    assert_eq!(h.catalog.plays.lock().unwrap().len(), history);
    assert!(
        !h.drain()
            .iter()
            .any(|e| matches!(e.event, Event::TrackStarted { .. }))
    );
    h.ok(Command::Resume);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if h.drain().iter().any(|e| {
            matches!(e.event,
            Event::TrackStarted { entry, .. } if entry == entries[2])
        }) {
            break;
        }
        assert!(Instant::now() < deadline);
        h.output.render_f32(128);
        std::thread::yield_now();
    }
    assert_eq!(h.snapshot().state, PlayState::Playing);
    h.ok(Command::Stop);
    assert!(
        !h.catalog
            .plays
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.track == TrackId(2))
    );
}

#[test]
fn tail_next_is_unchanged_including_position_and_receipt() {
    let mut h = Harness::new(None);
    let entries = begin(&mut h, 2);
    h.ok(seek(500));
    h.drain();
    for mode in [RepeatMode::Off, RepeatMode::One] {
        h.ok(Command::SetRepeat { mode });
        h.drain();
        let before = h.snapshot();
        assert!(!h.ok(Command::Next).changed);
        selected(&h, entries[2], 2, 500);
        assert_eq!(h.snapshot().queue, before.queue);
        assert!(h.drain().is_empty());
    }
    h.ok(Command::SetRepeat {
        mode: RepeatMode::All,
    });
    assert!(h.ok(Command::Next).changed);
    selected(&h, entries[0], 0, 0);
}

#[test]
fn repeat_all_same_entry_resets_position_without_starting() {
    let mut h = Harness::new(None);
    let entry = h.ok(play(tracks(&[1]))).created_entries[0];
    h.ok(Command::Stop);
    h.ok(Command::SetRepeat {
        mode: RepeatMode::All,
    });
    h.ok(seek(500));
    h.drain();
    for _ in 0..2 {
        assert!(h.ok(Command::Next).changed);
        selected(&h, entry, 0, 0);
        let events = h.drain();
        assert!(
            events
                .iter()
                .any(|e| matches!(e.event, Event::Position { position }
            if position.entry == entry && position.position_ms == 0))
        );
        assert!(!events.iter().any(|e| matches!(
            e.event,
            Event::TrackStarted { .. } | Event::QueueChanged { .. }
        )));
    }
}

#[test]
fn enqueue_only_next_and_previous_have_no_target_and_do_not_start() {
    let mut h = Harness::new(None);
    for enqueued in [false, true] {
        if enqueued {
            h.ok(Command::Enqueue {
                tracks: vec![TrackId(1), TrackId(2)],
                at: InsertAt::End,
                expected_revision: None,
                expected_current: None,
            });
        }
        h.drain();
        assert!(!h.ok(Command::Next).changed);
        assert!(!h.ok(Command::Previous).changed);
        let s = h.snapshot();
        assert_eq!(s.state, PlayState::Stopped);
        assert!(s.current.is_none() && s.queue.current_index.is_none() && s.position.is_none());
        assert!(!h.output.is_open());
        assert!(h.drain().is_empty());
        assert!(h.catalog.plays.lock().unwrap().is_empty());
    }
}

#[test]
fn bad_file_is_selected_without_probe_and_fails_only_on_resume() {
    for (successor, corrupt) in [(true, false), (false, false), (true, true), (false, true)] {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = MemoryCatalog::standard();
        if corrupt {
            let path = dir.path().join("corrupt.flac");
            std::fs::write(&path, b"not a FLAC file").unwrap();
            catalog.tracks.get_mut(&TrackId(9)).unwrap().1 = path;
        }
        let mut h = Harness::with_catalog(Arc::new(catalog), None);
        let ids = if successor { vec![1, 9, 3] } else { vec![1, 9] };
        let entries = h.ok(play(tracks(&ids))).created_entries;
        h.ok(Command::Stop);
        h.drain();
        let history = h.catalog.plays.lock().unwrap().len();
        h.ok(Command::Next);
        let s = h.snapshot();
        assert_eq!(s.current.unwrap().entry, entries[1]);
        assert_eq!(s.position.unwrap().position_ms, 0);
        assert_eq!(s.state, PlayState::Stopped);
        assert!(!h.output.is_open());
        assert!(!h.drain().iter().any(|e| matches!(
            e.event,
            Event::TrackFailed { .. } | Event::TrackStarted { .. }
        )));
        assert_eq!(h.catalog.plays.lock().unwrap().len(), history);
        h.ok(Command::Resume);
        assert!(h.drain().iter().any(|e| matches!(e.event,
            Event::TrackFailed { entry, upcoming: false, .. }
                if entry == entries[1])));
        let s = h.snapshot();
        assert_eq!(
            s.state,
            if successor {
                PlayState::Playing
            } else {
                PlayState::Stopped
            }
        );
        if successor {
            assert_eq!(s.current.unwrap().entry, entries[2]);
        }
    }
}

#[test]
fn exit_preserves_real_position_unless_stop_explicitly_reset_it() {
    for paused in [false, true] {
        for stop in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let state = dir.path().join("state.json");
            let entry;
            {
                let mut h = Harness::new(Some(state.clone()));
                entry = h.ok(play(tracks(&[1]))).created_entries[0];
                h.ok(Command::Pause);
                h.ok(seek(500));
                if !paused {
                    h.ok(Command::Resume);
                    let deadline = Instant::now() + TIMEOUT;
                    while h.output.render_f32(128).iter().all(|s| *s == 0.0) {
                        assert!(Instant::now() < deadline, "resumed audio missing");
                        std::thread::yield_now();
                    }
                }
                if stop {
                    h.ok(Command::Stop);
                }
                assert!(h.core.shutdown_and_wait(TIMEOUT));
            }
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
            let position_ms = saved["queue"]["position_ms"].as_u64().unwrap();
            if stop {
                assert_eq!(position_ms, 0);
            } else if paused {
                assert_eq!(position_ms, 500);
            } else {
                assert!(position_ms >= 500, "{saved}");
            }
            let h = Harness::new(Some(state));
            selected(&h, entry, 0, position_ms);
        }
    }
}

#[test]
fn shutdown_and_restart_restore_last_selection_at_zero_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    let entry;
    {
        let mut h = Harness::new(Some(state.clone()));
        let entries = begin(&mut h, 0);
        h.ok(seek(500));
        h.ok(Command::Next);
        h.ok(Command::Next);
        entry = entries[2];
    }
    let h = Harness::new(Some(state));
    selected(&h, entry, 2, 0);
}
