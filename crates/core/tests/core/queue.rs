//! Queue scenarios.
use super::*;

#[test]
fn enqueue_next_changes_what_plays_next() {
    let mut h = Harness::new(None);
    let receipt = h.ok(play(tracks(&[1, 2])));
    let added = h.ok(Command::Enqueue {
        tracks: vec![TrackId(3)],
        at: InsertAt::Next,
        expected_revision: Some(receipt.queue_revision),
        expected_current: None,
    });
    h.until(is_stopped);
    let created = receipt.created_entries;
    assert_eq!(
        h.started(),
        [created[0], added.created_entries[0], created[1]]
    );
}

#[test]
fn queue_pages_reflect_the_latest_command() {
    let mut h = Harness::new(None);
    h.ok(play(tracks(&[1, 2])));
    let receipt = h.ok(Command::Enqueue {
        tracks: vec![TrackId(3)],
        at: InsertAt::End,
        expected_revision: None,
        expected_current: None,
    });
    // Read your own write: no waiting for anything.
    let (revision, total, entries) = h.queue_page();
    assert_eq!(revision, receipt.queue_revision);
    assert_eq!(total, 3);
    assert_eq!(entries.last().unwrap().entry, receipt.created_entries[0]);
    assert_eq!(
        entries.iter().map(|e| e.track.id.0).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let QueryResult::QueuePage {
        offset,
        total,
        next_offset,
        entries,
        ..
    } = h.core.query(Query::QueuePage {
        offset: 1,
        limit: 1,
    })
    else {
        panic!()
    };
    assert_eq!(
        (offset, total, next_offset, entries.len()),
        (1, 3, Some(2), 1)
    );
}

#[test]
fn unavailable_tracks_in_the_queue_are_skipped() {
    // The file exists, but the library says the track is unavailable (its folder was
    // removed): it must not play.
    let mut catalog = MemoryCatalog::standard();
    catalog.tracks.get_mut(&TrackId(2)).unwrap().0.available = false;
    let mut h = Harness::with_catalog(Arc::new(catalog), None);
    let created = h.ok(play(album())).created_entries;
    h.until(is_stopped);
    assert_eq!(h.started(), [created[0], created[2]]);
    assert!(h.seen.iter().any(|e| matches!(
        e.event,
        Event::TrackFailed {
            code: ErrorCode::TrackUnavailable,
            ..
        }
    )));
}

#[test]
fn c1_shuffled_play_plays_every_track() {
    let mut h = Harness::new(None);
    h.ok(Command::SetShuffle { shuffle: true });
    // That change announces the (stopped) state; start watching after it.
    h.until(|e| matches!(e, Event::StateChanged { .. }));
    h.seen.clear();
    let created = h.ok(play_at(album(), 1)).created_entries;
    h.until(is_stopped);
    let started = h.started();
    assert_eq!(started.first(), Some(&created[1]), "starts where asked");
    let mut sorted = started.clone();
    sorted.sort();
    assert_eq!(sorted, created, "every track exactly once: {started:?}");
}

#[test]
fn c2_clear_while_playing_lets_the_current_track_finish_and_stops() {
    let mut h = Harness::new(None);
    h.ok(play(album()));
    h.until(|e| matches!(e, Event::TrackStarted { .. }));
    h.ok(clear());
    h.until(is_stopped);
    assert_eq!(
        heard_tracks(&h),
        [1],
        "nothing from the cleared queue plays"
    );
}

#[test]
fn c3_a_failing_first_track_is_skipped_and_the_queue_still_changes() {
    let mut h = Harness::new(None);
    let created = h.ok(play(tracks(&[9, 1]))).created_entries;
    h.until(is_stopped);
    assert!(
        h.seen
            .iter()
            .any(|e| matches!(&e.event, Event::TrackFailed { entry, .. } if *entry == created[0]))
    );
    assert!(
        h.seen
            .iter()
            .any(|e| matches!(e.event, Event::QueueChanged { .. }))
    );
    assert_eq!(heard_tracks(&h), [1]);
    let plays = h.catalog.plays.lock().unwrap().clone();
    assert!(
        plays
            .iter()
            .any(|p| p.track == TrackId(9) && p.ended == EndReason::Failed),
        "{plays:?}"
    );
}

#[test]
fn c5_resume_after_the_stopped_entry_left_the_queue() {
    let mut h = Harness::new(None);
    h.ok(play(album()));
    h.until(|e| matches!(e, Event::TrackStarted { .. }));
    h.ok(Command::Stop);
    h.ok(clear());
    h.ok(Command::Enqueue {
        tracks: vec![TrackId(3)],
        at: InsertAt::End,
        expected_revision: None,
        expected_current: None,
    });
    h.ok(Command::Resume);
    h.until(|e| matches!(e, Event::TrackStarted { track, .. } if track.id == TrackId(3)));
}

#[test]
fn c6_enqueueing_unknown_tracks_is_refused() {
    let mut h = Harness::new(None);
    let error = h.err(Command::Enqueue {
        tracks: vec![TrackId(1), TrackId(12345)],
        at: InsertAt::End,
        expected_revision: None,
        expected_current: None,
    });
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::NotFound, ErrorCode::TrackNotFound)
    );
    assert_eq!(
        error.details.missing,
        [ObjectRef::Track { id: TrackId(12345) }]
    );
    assert_eq!(h.snapshot().queue.entry_count, 0);
}

#[test]
fn queue_changed_follows_the_summary() {
    let mut h = Harness::new(None);
    let receipt = h.ok(play(album()));
    let first = h.until_envelope(|e| matches!(e.event, Event::QueueChanged { .. }));
    assert_eq!(first.origin, Some(Origin::Ui));
    let Event::QueueChanged { queue } = first.event else {
        unreachable!()
    };
    assert_eq!(
        queue,
        QueueSummary {
            revision: receipt.queue_revision,
            entry_count: 3,
            current_index: Some(0),
            next_entry: Some(receipt.created_entries[1]),
            natural_next_entry: Some(receipt.created_entries[1]),
        }
    );
    // The next track: only the current index changes, and it is announced (no origin:
    // no command caused it).
    let next = h.until_envelope(
        |e| matches!(&e.event, Event::QueueChanged { queue } if queue.current_index == Some(1)),
    );
    assert_eq!(next.origin, None);
    let Event::QueueChanged { queue } = next.event else {
        unreachable!()
    };
    assert_eq!(queue.revision, receipt.queue_revision);
    assert_eq!(queue.next_entry, Some(receipt.created_entries[2]));
    assert_eq!(queue.natural_next_entry, queue.next_entry);
}

/// With repeat one, `Next` (also a headset's button) moves on; repeat one holds only at a
/// track's natural end. `Previous` goes back likewise.
#[test]
fn next_and_previous_move_on_with_repeat_one() {
    let mut h = Harness::new(None);
    h.ok(Command::SetRepeat {
        mode: RepeatMode::One,
    });
    let e = h.ok(play(album())).created_entries;
    h.until(|ev| matches!(ev, Event::TrackStarted { entry, .. } if *entry == e[0]));
    h.ok(Command::Next);
    h.until(|ev| matches!(ev, Event::TrackStarted { entry, .. } if *entry == e[1]));
    h.ok(Command::Previous);
    h.until(|ev| matches!(ev, Event::TrackStarted { entry, .. } if *entry == e[0]));
    assert_eq!(h.snapshot().repeat, RepeatMode::One);
    // At its natural end the track plays again.
    h.drain();
    h.until(|ev| matches!(ev, Event::TrackStarted { entry, .. } if *entry == e[0]));
}

/// An unknown insert position is refused also when there is nothing to insert.
#[test]
fn enqueue_checks_its_insert_position_also_with_no_tracks() {
    let h = Harness::new(None);
    let r: serde_json::Value = serde_json::from_str(&h.core.execute_json(
        r#"{"request_id":1,"command":{"type":"enqueue","tracks":[],"at":"after_album"}}"#,
        Origin::Ui,
    ))
    .unwrap();
    assert_eq!(r["result"]["error"]["code"], "unknown_variant", "{r}");
    assert_eq!(
        r["result"]["error"]["details"]["field"], "command.at",
        "{r}"
    );
}

/// Found on Windows by ear: repeat one switched on while a track plays must repeat that
/// track, although the engine was already given the next one.
#[test]
fn repeat_one_switched_on_mid_track_repeats_it() {
    let mut h = Harness::new(None);
    let created = h.ok(Command::Play {
        context: PlayContext::Album { key: "sine".into() },
        start: 0,
        start_track: None,
    });
    h.until(|e| matches!(e, Event::TrackStarted { .. }));
    h.ok(Command::SetRepeat {
        mode: RepeatMode::One,
    });
    let again = h.until(|e| matches!(e, Event::TrackStarted { .. }));
    let Event::TrackStarted { entry, .. } = again else {
        unreachable!()
    };
    assert_eq!(entry, created.created_entries[0]);
    h.ok(Command::SetRepeat {
        mode: RepeatMode::Off,
    });
    h.until(is_stopped);
}

#[test]
fn mock_queue_command_traces_match_core() {
    let path = client_parity::core_crate_path(env!("CARGO_MANIFEST_DIR"), "mock-state-cases.json");
    let mut corpus: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let update = std::env::var_os("UPDATE_MOCK_STATE_CASES").is_some();
    for case in corpus["queue"].as_array_mut().unwrap() {
        let name = case["name"].clone();
        let mut h = Harness::new(None);
        for step in case["steps"].as_array_mut().unwrap() {
            let (revision, _, entries) = h.queue_page();
            let mut command = step["command"].clone();
            if let Some(indices) = step["entries"].as_array() {
                command["entries"] = serde_json::json!(
                    indices
                        .iter()
                        .map(|i| entries[i.as_u64().unwrap() as usize].entry)
                        .collect::<Vec<_>>()
                );
            }
            if step["revision"] == true {
                command["revision"] = serde_json::json!(revision);
            }
            h.ok(serde_json::from_value(command).unwrap());
            let s = h.snapshot();
            let actual = serde_json::json!({
                        "state":s.state,
                        "current":s.current.map(|c| c.track.id),
                        "current_index":s.queue.current_index,
                        "shuffle":s.shuffle,
                        "repeat":s.repeat,
                        "next":h.queue_page().2
            .iter().find(|e| Some(e.entry)==s.queue.next_entry).map(|e| e.track.id),
                        "natural_next":h.queue_page().2
            .iter().find(|e| Some(e.entry)==s.queue.natural_next_entry).map(|e| e.track.id),
                        "queue":h.queue_page().2.iter().map(|e| e.track.id).collect::<Vec<_>>()}
                        );

            if update {
                step["expected"] = actual;
            } else {
                assert_eq!(actual, step["expected"], "{}: {}", name, step["command"]);
            }
        }
    }
    if update {
        std::fs::write(path, serde_json::to_string_pretty(&corpus).unwrap() + "\n").unwrap();
    }
}

#[test]
fn enqueue_anchor_rejects_natural_track_change_without_changing_revision_or_undo() {
    let mut h = Harness::new(None);
    let played = h.ok(play(tracks(&[1, 2])));
    let anchor = played.created_entries[0];
    let next = played.created_entries[1];
    h.until(|event| matches!(event, Event::TrackStarted { entry, .. } if *entry == next));
    h.ok(Command::Pause);
    let before = h.queue_page();
    assert_eq!(
        before.0, played.queue_revision,
        "natural transition is not a queue edit"
    );
    let error = h.err(Command::Enqueue {
        tracks: vec![TrackId(3)],
        at: InsertAt::Next,
        expected_revision: Some(played.queue_revision),
        expected_current: Some(anchor),
    });
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Conflict, ErrorCode::EntryNotCurrent)
    );
    assert_eq!(
        error.details.field.as_deref(),
        Some("command.expected_current")
    );
    assert_eq!(h.queue_page(), before);
    // The rejected edit must not replace the undo saved by Play.
    h.ok(Command::Undo {
        revision: played.queue_revision,
    });
    assert!(h.queue_page().2.is_empty());
}

#[test]
fn clear_undo_restores_fourth_of_36_entries_without_restarting_playback() {
    for state in [PlayState::Playing, PlayState::Paused, PlayState::Stopped] {
        let mut h = Harness::new(None);
        let receipt = h.ok(Command::Play {
            context: PlayContext::Tracks {
                tracks: (0..36).map(|i| TrackId(i % 3 + 1)).collect(),
            },
            start: 3,
            start_track: None,
        });
        let entry = receipt.created_entries[3];
        h.until(|e| matches!(e,Event::TrackStarted {entry:started,..} if *started==entry));
        match state {
            PlayState::Paused => {
                h.ok(Command::Pause);
            }
            PlayState::Stopped => {
                h.ok(Command::Stop);
            }
            _ => {}
        }
        let clear = h.ok(Command::Clear {
            expected_revision: None,
        });
        assert_eq!(h.snapshot().queue.current_index, None);
        h.ok(Command::Undo {
            revision: clear.queue_revision,
        });
        let s = h.snapshot();
        assert_eq!((s.queue.current_index, s.queue.entry_count), (Some(3), 36));
        assert_eq!(s.state, state);
        assert_eq!(s.current.unwrap().entry, entry);
        assert_eq!(
            h.queue_page().2.iter().map(|e| e.entry).collect::<Vec<_>>(),
            receipt.created_entries
        );
        let next = h.ok(Command::Enqueue {
            tracks: vec![TrackId(1)],
            at: InsertAt::Next,
            expected_revision: Some(s.queue.revision),
            expected_current: Some(entry),
        });
        assert_eq!(h.queue_page().2[4].entry, next.created_entries[0]);
    }
}

#[test]
fn queue_candidates_follow_shuffle_and_notify_without_editing_revision() {
    let mut h = Harness::new(None);
    let ids = h.ok(play(album())).created_entries;
    h.ok(Command::Pause);
    h.ok(Command::SetShuffle { shuffle: true });
    let revision = h.snapshot().queue.revision;
    let next = h.snapshot().queue.next_entry.unwrap();
    assert_ne!(next, ids[0]);
    assert_eq!(h.snapshot().queue.natural_next_entry, Some(next));
    h.drain();
    h.ok(Command::SetRepeat {
        mode: RepeatMode::One,
    });
    let one = h.snapshot().queue;
    assert_eq!(
        (one.next_entry, one.natural_next_entry),
        (Some(next), Some(ids[0]))
    );
    assert_eq!(one.revision, revision);
    assert!(
        h.drain()
            .iter()
            .any(|e| matches!(e.event, Event::QueueChanged { queue } if queue == one))
    );
    h.ok(Command::Remove {
        entries: vec![ids[0]],
        expected_revision: None,
    });
    let detached = h.snapshot().queue;
    assert_eq!(
        (detached.next_entry, detached.natural_next_entry),
        (Some(next), Some(next))
    );
    h.ok(Command::Next);
    assert_eq!(h.snapshot().current.unwrap().entry, next);
    h.ok(Command::Stop);
    assert_eq!(h.snapshot().queue.natural_next_entry, Some(next));
}
