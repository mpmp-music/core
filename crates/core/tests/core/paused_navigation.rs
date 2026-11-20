use super::*;

// Drive callbacks and wait for an observable condition; never infer progress from sleep.
fn until(h: &mut Harness, mut predicate: impl FnMut(&Event) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        for envelope in h.drain() {
            if predicate(&envelope.event) {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "missing event: {:?}",
            h.snapshot()
        );
        if h.output.is_open() {
            h.output.render_f32(128);
        }
        std::thread::yield_now();
    }
}

fn begin(h: &mut Harness, start: u32) -> Vec<EntryId> {
    let entries = h.ok(play_at(album(), start)).created_entries;
    until(h, |e| matches!(e, Event::TrackStarted { .. }));
    h.ok(Command::Pause);
    // Complete the existing track's intentional pause fade before testing navigation.
    h.output.render_f32(4096);
    h.ok(Command::Pause);
    h.drain();
    entries
}

fn selected(h: &mut Harness, entry: EntryId, index: u32, position: u64) {
    let s = h.snapshot();
    assert_eq!(s.state, PlayState::Paused);
    assert_eq!(s.current.unwrap().entry, entry);
    assert_eq!(s.queue.current_index, Some(index));
    let p = s.position.unwrap();
    assert_eq!((p.entry, p.position_ms, p.rate), (entry, position, 0.0));
    if h.output.is_open() {
        assert!(h.output.render_f32(4096).iter().all(|s| *s == 0.0));
    }
    // A command response is a barrier for the effects of preceding callbacks.
    h.ok(Command::Pause);
    assert!(
        !h.drain()
            .iter()
            .any(|e| matches!(e.event, Event::TrackStarted { .. }))
    );
}

#[test]
fn next_announces_selection_for_every_origin_without_a_false_start() {
    for origin in [Origin::Ui, Origin::MediaControls, Origin::agent("dj")] {
        let mut h = Harness::new(None);
        let entries = begin(&mut h, 0);
        h.ok(Command::SetRepeat {
            mode: RepeatMode::One,
        });
        h.drain();
        assert!(matches!(
            h.run_as(Command::Next, origin.clone()),
            CommandResult::Ok(_)
        ));
        let events = h.drain();
        assert!(events.iter().any(|e| e.origin.as_ref() == Some(&origin)
            && matches!(&e.event, Event::StateChanged { snapshot }
                if snapshot.state == PlayState::Paused
                && snapshot.current.as_ref().unwrap().entry == entries[1])));
        assert!(events.iter().any(
            |e| matches!(e.event, Event::QueueChanged { queue } if queue.current_index == Some(1))
        ));
        assert!(events.iter().any(|e| matches!(
            e.event,
            Event::Position { position }
                if position.entry == entries[1] && position.position_ms == 0,
        )));

        assert!(
            !events
                .iter()
                .any(|e| matches!(e.event, Event::TrackStarted { .. }))
        );
        selected(&mut h, entries[1], 1, 0);
        let plays = h.catalog.plays.lock().unwrap();
        assert_eq!(plays.len(), 1);
        assert_eq!(
            (plays[0].track, plays[0].ended),
            (TrackId(1), EndReason::Skipped)
        );
        assert_eq!(plays[0].origin.as_ref(), Some(&origin));
    }
}

#[test]
fn previous_selects_or_restarts_and_stays_paused() {
    let mut h = Harness::new(None);
    let entries = begin(&mut h, 1);
    h.ok(Command::Previous);
    selected(&mut h, entries[0], 0, 0);
    h.ok(Command::Previous);
    selected(&mut h, entries[0], 0, 0);
    // The heard-track >=3s branch is covered with a deterministic fade-completion
    // gate in actor::output_tests::paused_navigation_previous_restarts_after_the_pause_fade.
}

#[test]
fn repeated_next_then_resume_starts_only_the_last_selection_and_gapless_continues() {
    let mut h = Harness::new(None);
    let entries = begin(&mut h, 0);
    h.ok(Command::Next);
    h.ok(Command::Next);
    selected(&mut h, entries[2], 2, 0);
    assert_eq!(h.catalog.plays.lock().unwrap().len(), 1);
    h.ok(Command::SetRepeat {
        mode: RepeatMode::All,
    });
    h.ok(Command::Resume);
    until(
        &mut h,
        |e| matches!(e, Event::TrackStarted { entry, .. } if *entry == entries[2]),
    );
    assert_eq!(h.snapshot().state, PlayState::Playing);
    until(
        &mut h,
        |e| matches!(e, Event::TrackStarted { entry, .. } if *entry == entries[0]),
    );
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
fn selection_seek_resume_keeps_target_and_announces_a_real_start() {
    let mut h = Harness::new(None);
    let entries = begin(&mut h, 0);
    h.ok(Command::Next);
    let error = h.err(Command::Seek {
        entry: Some(entries[0]),
        position_ms: 0,
    });
    assert_eq!(error.code, ErrorCode::EntryNotCurrent);
    h.ok(Command::Seek {
        entry: Some(entries[1]),
        position_ms: 500,
    });
    selected(&mut h, entries[1], 1, 500);
    h.ok(Command::Resume);
    until(
        &mut h,
        |e| matches!(e, Event::TrackStarted { entry, .. } if *entry == entries[1]),
    );
    h.ok(Command::Pause);
    let p = h.snapshot().position.unwrap();
    assert_eq!(p.entry, entries[1]);
    assert!(p.position_ms >= 500, "{p:?}");
    h.ok(Command::Stop);
    assert_eq!(h.catalog.plays.lock().unwrap().len(), 2);
}

#[test]
fn corrupt_file_is_skipped_silently_and_no_successor_stops() {
    for successor in [true, false] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let corrupt = dir.path().join("corrupt.flac");
        std::fs::write(&corrupt, b"not a FLAC file").unwrap();
        let mut catalog = MemoryCatalog::standard();
        // Start on C so the corrupt file is not prefetched before Pause.
        catalog.tracks.get_mut(&TrackId(1)).unwrap().1 = corrupt;
        let mut h = Harness::with_catalog(Arc::new(catalog), None);
        let ids = if successor { vec![3, 1, 2] } else { vec![3, 1] };
        let entries = h.ok(play(tracks(&ids))).created_entries;
        // Pause before any callback, avoiding an unrelated upcoming failure.
        h.ok(Command::Pause);
        h.drain();
        h.ok(Command::Next);
        let events = h.drain();
        assert!(events.iter().any(|e| matches!(
            e.event,
            Event::TrackFailed {
                entry,
                upcoming: false,
                ..
            } if entry == entries[1],
        )));

        assert!(
            !events
                .iter()
                .any(|e| matches!(e.event, Event::TrackStarted { .. }))
        );
        if successor {
            selected(&mut h, entries[2], 2, 0);
            h.ok(Command::Resume);
            until(
                &mut h,
                |e| matches!(e, Event::TrackStarted { entry, .. } if *entry == entries[2]),
            );
        } else {
            assert_eq!(h.snapshot().state, PlayState::Stopped);
            assert!(!h.output.is_open());
        }
    }
}

#[test]
fn missing_files_and_failure_budget_never_start_playing() {
    for ids in [vec![1, 9, 3], vec![1, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 3]] {
        let mut h = Harness::new(None);
        let entries = h.ok(play(tracks(&ids))).created_entries;
        h.ok(Command::Pause);
        h.drain();
        h.ok(Command::Next);
        let events = h.drain();
        assert!(events.iter().any(|e| matches!(
            e.event,
            Event::TrackFailed {
                upcoming: false,
                code: ErrorCode::FileMissing,
                ..
            }
        )));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.event, Event::TrackStarted { .. }))
        );
        if ids.len() == 3 {
            selected(&mut h, entries[2], 2, 0);
        } else {
            assert_eq!(h.snapshot().state, PlayState::Stopped);
        }
    }
}

#[test]
fn tail_next_stops_and_unheard_selections_do_not_add_history() {
    let mut h = Harness::new(None);
    begin(&mut h, 0);
    h.ok(Command::Next);
    h.ok(Command::Next);
    h.ok(Command::Next);
    let s = h.snapshot();
    assert_eq!(s.state, PlayState::Stopped);
    assert_eq!(s.queue.current_index, Some(2));
    assert!(s.position.is_none());
    assert!(!h.output.is_open());
    assert_eq!(h.catalog.plays.lock().unwrap().len(), 1);
}

#[test]
fn restart_restores_selection_and_seek_without_playing_or_extra_history() {
    for position_ms in [0, 500] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let state = dir.path().join("state.json");
        let catalog;
        let entry;
        {
            let mut h = Harness::new(Some(state.clone()));
            let entries = begin(&mut h, 0);
            catalog = h.catalog.clone();
            h.ok(Command::Next);
            entry = entries[1];
            if position_ms != 0 {
                h.ok(Command::Seek {
                    entry: Some(entry),
                    position_ms,
                });
            }
        }
        assert_eq!(catalog.plays.lock().unwrap().len(), 1);
        let mut h = Harness::new(Some(state));
        let s = h.snapshot();
        assert_eq!(s.state, PlayState::Stopped);
        assert_eq!(s.current.unwrap().entry, entry);
        assert_eq!(s.queue.current_index, Some(1));
        assert_eq!(s.position.unwrap().position_ms, position_ms);
        assert!(!h.output.is_open());
        h.ok(Command::Resume);
        until(
            &mut h,
            |e| matches!(e, Event::TrackStarted { entry: e, .. } if *e == entry),
        );
    }
}

#[test]
fn playing_navigation_keeps_its_existing_behavior() {
    let mut h = Harness::new(None);
    let entries = h.ok(play(album())).created_entries;
    until(&mut h, |e| matches!(e, Event::TrackStarted { .. }));
    h.ok(Command::Next);
    until(
        &mut h,
        |e| matches!(e, Event::TrackStarted { entry, .. } if *entry == entries[1]),
    );
    assert_eq!(h.snapshot().state, PlayState::Playing);
    h.ok(Command::Previous);
    until(
        &mut h,
        |e| matches!(e, Event::TrackStarted { entry, .. } if *entry == entries[0]),
    );
    assert_eq!(h.snapshot().state, PlayState::Playing);
}
