//! Playback scenarios.
use super::*;

#[test]
fn plays_an_album_through_and_records_history() {
    let mut h = Harness::new(None);
    let receipt = h.ok(play(album()));
    assert!(receipt.changed);
    let created = receipt.created_entries;
    assert_eq!(created.len(), 3);
    h.until(is_stopped);
    assert_eq!(h.started(), created, "every track heard, in order");
    let plays = h.catalog.plays.lock().unwrap().clone();
    assert_eq!(
        plays.iter().map(|p| p.track.0).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(
        plays.iter().all(|p| p.ended == EndReason::Completed),
        "{plays:?}"
    );
    assert_eq!(h.last_state().state, PlayState::Stopped);

    // Track events carry the signal path; sequence numbers have no gaps.
    let Event::TrackStarted {
        signal_path, track, ..
    } = &h
        .seen
        .iter()
        .find(|e| matches!(e.event, Event::TrackStarted { .. }))
        .unwrap()
        .event
    else {
        unreachable!()
    };
    assert_eq!(track.title, "sine44_a.flac");
    let path = signal_path.as_ref().unwrap();
    assert_eq!(path.codec, "flac");
    assert_eq!(path.output_format, SampleFormat::F32);
    let seqs: Vec<u64> = h.seen.iter().map(|e| e.seq).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
}

#[test]
fn stale_edits_are_refused() {
    let mut h = Harness::new(None);
    let rev = h.ok(play(tracks(&[1]))).queue_revision;
    h.ok(Command::Enqueue {
        tracks: vec![TrackId(2)],
        at: InsertAt::End,
        expected_revision: Some(rev),
        expected_current: None,
    });
    let error = h.err(Command::Enqueue {
        tracks: vec![TrackId(3)],
        at: InsertAt::End,
        expected_revision: Some(rev),
        expected_current: None,
    });
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Conflict, ErrorCode::QueueConflict)
    );
    assert_eq!(error.details.expected, Some(rev.0));
    assert_eq!(error.details.actual, Some(rev.0 + 1));
    // The receipt's revision lets the client undo exactly its own edit.
    let after_add = h
        .ok(Command::Enqueue {
            tracks: vec![TrackId(3)],
            at: InsertAt::End,
            expected_revision: None,
            expected_current: None,
        })
        .queue_revision;
    h.ok(Command::Undo {
        revision: after_add,
    });
    assert_eq!(h.snapshot().queue.entry_count, 2);
    // A stale clear is refused too.
    let error = h.err(Command::Clear {
        expected_revision: Some(rev),
    });
    assert_eq!(error.code, ErrorCode::QueueConflict);
}

#[test]
fn pause_resume_seek_and_next() {
    let mut h = Harness::new(None);
    let created = h.ok(play(album())).created_entries;
    h.until(|e| matches!(e, Event::TrackStarted { .. }));
    h.ok(Command::Pause);
    assert_eq!(h.snapshot().state, PlayState::Paused);
    h.ok(seek(500));
    h.ok(Command::Resume);
    assert_eq!(h.snapshot().state, PlayState::Playing);
    h.ok(Command::Next);
    h.until(|e| matches!(e, Event::TrackStarted { entry, .. } if *entry == created[1]));
    let plays = h.catalog.plays.lock().unwrap().clone();
    assert_eq!(plays[0].ended, EndReason::Skipped);
    assert_eq!(plays[0].origin, Some(Origin::Ui));
}

#[test]
fn a_missing_file_is_skipped() {
    let mut h = Harness::new(None);
    let created = h.ok(play(tracks(&[1, 9, 3]))).created_entries;
    h.until(is_stopped);
    // Found while the first track plays (as the next one is prepared): reported once,
    // as upcoming, and recorded as a failed play.
    let failures: Vec<_> = h
        .seen
        .iter()
        .filter_map(|e| match &e.event {
            Event::TrackFailed {
                entry,
                upcoming,
                code,
                ..
            } => Some((*entry, *upcoming, *code)),
            _ => None,
        })
        .collect();
    assert_eq!(failures, [(created[1], true, ErrorCode::FileMissing)]);
    assert_eq!(h.started(), [created[0], created[2]]);
    let plays = h.catalog.plays.lock().unwrap().clone();
    assert!(
        plays
            .iter()
            .any(|p| p.track == TrackId(9) && p.ended == EndReason::Failed),
        "{plays:?}"
    );
}

#[test]
fn a_slow_subscriber_is_told_to_resync_without_waiting_for_an_event() {
    let h = Harness::new(None);
    let slow = h
        .core
        .subscribe(
            Origin::Ui,
            SubscribeOptions::default().with_positions(false),
        )
        .unwrap();
    let mut h = h;
    // Flood events without reading `slow`: each command emits at least one.
    for _ in 0..300 {
        h.ok(Command::SetShuffle { shuffle: true });
        h.ok(Command::SetShuffle { shuffle: false });
    }
    let events: Vec<_> = slow.events.try_iter().collect();
    assert!(events.len() <= 256 + 1);
    // After draining, a Resync with a fresh snapshot comes on its own (on the next tick).
    let next = slow.events.recv_timeout(TIMEOUT).unwrap();
    let Event::Resync { snapshot } = next.event else {
        panic!("{next:?}")
    };
    assert_eq!(snapshot.seq, next.seq);
    assert_eq!(snapshot.seq, h.snapshot().seq);
}

#[test]
fn c2_removing_the_current_entry_continues_with_its_successor() {
    let mut h = Harness::new(None);
    let e = h.ok(play(album())).created_entries;
    h.until(|ev| matches!(ev, Event::TrackStarted { .. }));
    h.ok(Command::Remove {
        entries: vec![e[0]],
        expected_revision: None,
    });
    // `Next` goes to what followed the removed entry, not to the top of the queue.
    h.ok(Command::Next);
    h.until(|ev| matches!(ev, Event::TrackStarted { entry, .. } if *entry == e[1]));
    // And removing the (new) current plus its successor never plays the removed one.
    h.ok(Command::Remove {
        entries: vec![e[1], e[2]],
        expected_revision: None,
    });
    h.until(is_stopped);
    assert_eq!(heard_tracks(&h), [1, 2]);
}

#[test]
fn c12_state_snapshots_carry_their_own_event_seq() {
    let mut h = Harness::new(None);
    h.ok(Command::SetRepeat {
        mode: RepeatMode::All,
    });
    h.until(|e| matches!(e, Event::StateChanged { .. }));
    let envelope = h
        .seen
        .iter()
        .find(|e| matches!(e.event, Event::StateChanged { .. }))
        .unwrap();
    let Event::StateChanged { snapshot } = &envelope.event else {
        unreachable!()
    };
    assert_eq!(snapshot.seq, envelope.seq);
}

#[test]
fn c10_absurd_seek_targets_do_not_crash_the_core() {
    let mut h = Harness::new(None);
    h.ok(play_at(album(), 2));
    h.until(|e| matches!(e, Event::TrackStarted { .. }));
    // The test catalog knows no durations, so only the contract's number rule applies.
    let error = h.err(seek(u64::MAX));
    assert_eq!(
        (error.code, error.details.field.as_deref()),
        (ErrorCode::OutOfRange, Some("command.position_ms"))
    );
    h.ok(seek((1 << 53) - 1));
    // Positions never wrap or overflow: ticks and Stop read them.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        h.snapshot()
            .position
            .is_none_or(|p| p.position_ms >= 1_000_000)
    );
    h.until(is_stopped);
    assert!(matches!(h.run(Command::Stop), CommandResult::Ok(_)));
}

#[test]
fn ratings_are_checked_before_reaching_the_catalog() {
    let mut h = Harness::new(None);
    for rating in [0, 6] {
        let error = h.err(Command::SetRating {
            tracks: vec![TrackId(1)],
            rating: Some(rating),
        });
        assert_eq!(error.code, ErrorCode::OutOfRange, "{rating}");
        assert_eq!(error.details.field.as_deref(), Some("command.rating"));
    }
    let error = h.err(Command::SetRating {
        tracks: vec![TrackId(12345)],
        rating: Some(3),
    });
    assert_eq!(error.code, ErrorCode::TrackNotFound);
    // This catalog has no ratings.
    let error = h.err(Command::SetRating {
        tracks: vec![TrackId(1)],
        rating: Some(3),
    });
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Unsupported, ErrorCode::NotImplemented)
    );
}

#[test]
fn played_time_is_the_time_heard_not_where_playback_ended() {
    let (_dir, mut h) = with_long_track();

    // Seek to a second before the end and let it run into the next track.
    let created = h.ok(play(tracks(&[20, 2]))).created_entries;
    h.until(|e| matches!(e, Event::TrackStarted { .. }));
    h.ok(seek(19_000));
    h.until(|e| matches!(e, Event::TrackStarted { entry, .. } if *entry == created[1]));
    let played = h.catalog.plays.lock().unwrap()[0].clone();
    assert_eq!(played.track, TrackId(20));
    assert_eq!(played.ended, EndReason::Completed);
    assert!((900..3_000).contains(&played.played_ms), "{played:?}");

    // Seek into the middle, hear a little, stop.
    h.ok(play(tracks(&[20])));
    h.until(|e| matches!(e, Event::TrackStarted { track, .. } if track.id == TrackId(20)));
    h.ok(seek(15_000));
    h.until(|e| matches!(e, Event::Position { position } if position.position_ms >= 15_500));
    h.ok(Command::Stop);
    let plays = h.catalog.plays.lock().unwrap().clone();
    let stopped = plays.iter().rfind(|p| p.track == TrackId(20)).unwrap();
    assert_eq!(stopped.ended, EndReason::Stopped);
    assert!((400..4_000).contains(&stopped.played_ms), "{stopped:?}");
}

/// Quick repeated seeks must not count the same stretch again.
#[test]
fn quick_seeks_do_not_inflate_the_time_heard() {
    let (_dir, mut h) = with_long_track();
    h.ok(play(tracks(&[20])));
    h.until(|e| matches!(e, Event::TrackStarted { .. }));
    h.until(|e| matches!(e, Event::Position { position } if position.position_ms >= 6_000));
    for _ in 0..3 {
        h.ok(seek(0));
    }
    h.ok(Command::Stop);
    let plays = h.catalog.plays.lock().unwrap().clone();
    let stopped = plays.iter().rfind(|p| p.track == TrackId(20)).unwrap();
    assert!((5_500..8_000).contains(&stopped.played_ms), "{stopped:?}");
}

/// A seek right after Play counts from the seek target.
#[test]
fn a_seek_right_after_play_counts_from_its_target() {
    let (_dir, mut h) = with_long_track();
    h.ok(play(tracks(&[20])));
    h.ok(seek(15_000));
    h.until(|e| matches!(e, Event::Position { position } if position.position_ms >= 15_500));
    h.ok(Command::Stop);
    let plays = h.catalog.plays.lock().unwrap().clone();
    let stopped = plays.iter().rfind(|p| p.track == TrackId(20)).unwrap();
    assert_eq!(stopped.ended, EndReason::Stopped);
    assert!(stopped.played_ms < 4_000, "{stopped:?}");
}

#[test]
fn receipts_and_no_op_edits() {
    let mut h = Harness::new(None);
    let played = h.ok(play(tracks(&[1, 2, 3])));
    let e = played.created_entries;
    h.drain();
    let rev = played.queue_revision;

    // Empty edits change nothing: no new revision, no QueueChanged.
    let empty = h.ok(Command::Enqueue {
        tracks: Vec::new(),
        at: InsertAt::End,
        expected_revision: Some(rev),
        expected_current: None,
    });
    assert_eq!((empty.changed, empty.queue_revision), (false, rev));
    let gone = h.ok(Command::Remove {
        entries: vec![EntryId(1)],
        expected_revision: None,
    });
    assert_eq!((gone.changed, gone.queue_revision), (false, rev));
    assert!(gone.removed_entries.is_empty());
    let moved = h.ok(Command::Move {
        entry: e[1],
        to: 1,
        expected_revision: None,
    });
    assert!(!moved.changed);
    assert!(
        !h.drain()
            .iter()
            .any(|e| matches!(e.event, Event::QueueChanged { .. })),
        "no QueueChanged for no-ops"
    );

    // Removing is idempotent and reports what it removed.
    let removed = h.ok(Command::Remove {
        entries: vec![e[2], EntryId(1), e[2]],
        expected_revision: Some(rev),
    });
    assert!(removed.changed);
    assert_eq!(removed.removed_entries, [e[2]]);
    assert_eq!(removed.queue_revision.0, rev.0 + 1);
    let again = h.ok(Command::Remove {
        entries: vec![e[2]],
        expected_revision: None,
    });
    assert!(!again.changed);

    // Out of range and not found.
    let error = h.err(Command::Move {
        entry: e[0],
        to: 2,
        expected_revision: None,
    });
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Invalid, ErrorCode::OutOfRange)
    );
    let error = h.err(Command::Move {
        entry: EntryId(1),
        to: 0,
        expected_revision: None,
    });
    assert_eq!(error.code, ErrorCode::EntryNotFound);
    assert_eq!(error.details.missing, [ObjectRef::Entry { id: EntryId(1) }]);
    let error = h.err(Command::PlayEntry { entry: EntryId(1) });
    assert_eq!(error.code, ErrorCode::EntryNotFound);

    // Clearing an empty queue changes nothing.
    assert!(h.ok(clear()).changed);
    assert!(!h.ok(clear()).changed);
    // Setting shuffle to what it is changes nothing.
    assert!(!h.ok(Command::SetShuffle { shuffle: false }).changed);
}

#[test]
fn play_checks_its_start_and_context() {
    let mut h = Harness::new(None);
    let error = h.err(play_at(album(), 3));
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Invalid, ErrorCode::OutOfRange)
    );
    assert_eq!(error.details.field.as_deref(), Some("command.start"));
    let error = h.err(Command::Play {
        context: album(),
        start: 0,
        start_track: Some(TrackId(9)),
    });
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::NotFound, ErrorCode::TrackNotFound)
    );
    let error = h.err(play(tracks(&[1, 777, 2, 778])));
    assert_eq!(error.code, ErrorCode::TrackNotFound);
    assert_eq!(
        error.details.missing,
        [
            ObjectRef::Track { id: TrackId(777) },
            ObjectRef::Track { id: TrackId(778) }
        ]
    );
    let error = h.err(play(tracks(&[])));
    assert_eq!(error.code, ErrorCode::EmptyContext);
    let error = h.err(play(PlayContext::Album { key: "nope".into() }));
    assert_eq!(error.code, ErrorCode::AlbumNotFound);
    assert_eq!(h.snapshot().queue.entry_count, 0, "nothing changed");

    // `start_track` wins over `start`.
    let receipt = h.ok(Command::Play {
        context: album(),
        start: 0,
        start_track: Some(TrackId(3)),
    });
    let started = h.until(|e| matches!(e, Event::TrackStarted { .. }));
    let Event::TrackStarted { entry, track, .. } = started else {
        unreachable!()
    };
    assert_eq!((entry, track.id), (receipt.created_entries[2], TrackId(3)));
}

#[test]
fn seek_checks_its_entry_and_range_and_positions_follow_every_jump() {
    let (_dir, mut h) = with_long_track();
    let error = h.err(seek(1_000));
    assert_eq!(error.code, ErrorCode::NothingPlaying);
    let entry = h.ok(play(tracks(&[20, 1]))).created_entries[0];
    h.until(|e| matches!(e, Event::TrackStarted { .. }));

    let error = h.err(Command::Seek {
        position_ms: 1_000,
        entry: Some(EntryId(entry.0 + 1)),
    });
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Conflict, ErrorCode::EntryNotCurrent)
    );
    assert_eq!(error.details.actual, Some(entry.0));
    let error = h.err(Command::Seek {
        position_ms: 20_001,
        entry: Some(entry),
    });
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Invalid, ErrorCode::OutOfRange)
    );

    // Pause: a Position at once, rate 0.
    h.drain();
    h.ok(Command::Pause);
    let paused: Vec<PositionInfo> = h
        .drain()
        .iter()
        .filter_map(|e| match e.event {
            Event::Position { position } if position.rate == 0.0 => Some(position),
            _ => None,
        })
        .collect();
    assert_eq!(paused.len(), 1, "{paused:?}");

    // A seek while paused: announced at once, and the snapshot has it.
    h.ok(Command::Seek {
        position_ms: 12_000,
        entry: Some(entry),
    });
    let events = h.drain();
    let last = events.last().unwrap();
    assert_eq!(last.origin, Some(Origin::Ui));
    let Event::Position { position } = last.event else {
        panic!("{events:?}")
    };
    assert_eq!((position.entry, position.position_ms), (entry, 12_000));
    assert_eq!(h.snapshot().position.unwrap().position_ms, 12_000);
    // The snapshot's `seq` is the last non-`Position` event's.
    let last_state = h
        .seen
        .iter()
        .rev()
        .find(|e| !matches!(e.event, Event::Position { .. }))
        .map(|e| e.seq);
    assert_eq!(Some(h.snapshot().seq), last_state);

    // Resume: announced too, and playback continues from the target.
    h.ok(Command::Resume);
    assert!(
        h.drain()
            .iter()
            .any(|e| matches!(e.event, Event::Position { position } if position.rate == 1.0))
    );
    h.until(|e| matches!(e, Event::Position { position } if position.position_ms >= 13_000));
    // Stop: announced at once, rate 0.
    h.ok(Command::Stop);
    assert!(h.drain().iter().any(|e| matches!(
        e.event,
        Event::Position { position } if position.rate == 0.0 && position.position_ms == 0,
    )));
}

#[test]
fn idempotency_keys_make_retries_safe() {
    let mut h = Harness::new(None);
    let request = |id: u64, key: &str, track: i64| Request {
        request_id: id,
        idempotency_key: Some(key.into()),
        command: Command::Enqueue {
            tracks: vec![TrackId(track)],
            at: InsertAt::End,
            expected_revision: None,
            expected_current: None,
        },
    };
    let first = h.core.execute(request(1, "k1", 1), Origin::Ui);
    let retry = h.core.execute(request(2, "k1", 1), Origin::Ui);
    assert_eq!(retry.request_id, 2);
    assert_eq!(
        retry.result, first.result,
        "the first receipt, not a new one"
    );
    assert_eq!(h.snapshot().queue.entry_count, 1, "ran once");
    // The same key from another origin is another request.
    h.core.execute(request(3, "k1", 1), Origin::agent("a"));
    assert_eq!(h.snapshot().queue.entry_count, 2);
    // Reusing a key for another command is an error.
    let CommandResult::Err { error } = h.core.execute(request(4, "k1", 2), Origin::Ui).result
    else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::IdempotencyKeyReused);

    // A retry of a command waiting for confirmation gets the same token.
    let agent = Origin::agent("a");
    let purge = |id| Request {
        request_id: id,
        idempotency_key: Some("purge".into()),
        command: Command::PurgePlaylists { playlists: None },
    };
    let a = h.core.execute(purge(5), agent.clone()).result;
    let b = h.core.execute(purge(6), agent.clone()).result;
    assert_eq!(a, b);
    let CommandResult::Err { error } = a else {
        panic!()
    };
    let token = error.details.confirmation.unwrap().token;
    let rejected = h.ok(Command::Reject { token }).confirmed.unwrap();
    assert_eq!(
        (rejected.approved, rejected.reason),
        (false, Some(ConfirmationReason::Rejected))
    );
    // Once answered, a retry gets the answer.
    let CommandResult::Err { error } = h.core.execute(purge(7), agent).result else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::ConfirmationRejected);
}

#[test]
fn json_adapters_answer_every_request_with_the_contract() {
    let h = Harness::new(None);
    let response = |json: &str| -> serde_json::Value {
        serde_json::from_str(&h.core.execute_json(json, Origin::Ui)).unwrap()
    };
    let r = response(r#"{"request_id":4,"command":{"type":"set_volume","volume":0.5}}"#);
    assert_eq!(r["request_id"], 4);
    assert_eq!(r["result"]["type"], "ok");
    assert_eq!(r["result"]["changed"], true);

    for (json, kind, code, field) in [
        (
            r#"{"request_id":5,"command":{"type":"teleport","to":1}}"#,
            "unsupported",
            "unknown_command",
            None,
        ),
        (
            r#"{"request_id":5,"command":{"type":"seek","position_ms":1,"exact":true}}"#,
            "unsupported",
            "unknown_field",
            Some("command.exact"),
        ),
        (
            r#"{"request_id":5,"command":{"type":"stop","fade_ms":1}}"#,
            "unsupported",
            "unknown_field",
            Some("command.fade_ms"),
        ),
        (
            r#"{"request_id":5,"command":{"type":"play","context":{"type":"radio","station":1}}}"#,
            "unsupported",
            "unknown_variant",
            Some("command.context"),
        ),
        (
            r#"{"request_id":5,"command":{"type":"set_repeat","mode":"spiral"}}"#,
            "unsupported",
            "unknown_variant",
            Some("command.mode"),
        ),
        (
            r#"{"request_id":5,"command":{"type":"seek"}}"#,
            "invalid",
            "invalid_request",
            None,
        ),
        (r#"{"request_id":5,"#, "invalid", "invalid_json", None),
    ] {
        let r = response(json);
        let error = &r["result"]["error"];
        assert_eq!(r["result"]["type"], "err", "{json}: {r}");
        assert_eq!(error["kind"], kind, "{json}: {r}");
        assert_eq!(error["code"], code, "{json}: {r}");
        if let Some(field) = field {
            assert_eq!(error["details"]["field"], field, "{json}: {r}");
        }
        if !json.ends_with(',') {
            assert_eq!(r["request_id"], 5, "{json}");
        }
    }

    let query = |json: &str| -> serde_json::Value {
        serde_json::from_str(&h.core.query_json(json)).unwrap()
    };
    let snapshot = query(r#"{"type":"snapshot"}"#);
    assert_eq!(snapshot["snapshot"]["contract_version"], 1);
    assert_eq!(snapshot["snapshot"]["contract_minor"], CONTRACT_MINOR);
    assert_eq!(
        snapshot["snapshot"]["features"],
        serde_json::json!([
            "multi_artist",
            "cover_edit",
            "playback_limits",
            "output_devices",
            "output_mode_contract",
            "permissions",
            "lyrics",
            "waveform",
            "cue",
            "edit_tags",
            "automatic_loudness",
            "paused_navigation",
            "track_rows",
            "album_paging",
            "search_paging",
            "enqueue_expected_current",
            "playlist_playback_guards",
            "release_root",
            "queue_candidates",
            "structured_signal_path",
            "job_queries",
            "action_details",
            "scan_error_codes"
        ])
    );
    assert_eq!(snapshot["snapshot"]["audio"]["volume"], 0.5);
    let unknown = query(r#"{"type":"no_such_query_in_any_version","track":1}"#);
    assert_eq!(unknown["error"]["code"], "unknown_query");
    let bad = query(r#"{"type":"queue_page","limit":5,"cursor":"x"}"#);
    assert_eq!(bad["error"]["code"], "unknown_field");
}

#[test]
fn subscribers_can_do_without_positions() {
    let (_dir, mut h) = with_long_track();
    let quiet = h
        .core
        .subscribe(
            Origin::Ui,
            SubscribeOptions::default().with_positions(false),
        )
        .unwrap();
    h.ok(play(tracks(&[20])));
    h.until(|e| matches!(e, Event::TrackStarted { .. }));
    h.until(|e| matches!(e, Event::Position { position } if position.position_ms >= 2_000));
    let events: Vec<_> = quiet.events.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, Event::TrackStarted { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.event, Event::Position { .. }))
    );
}

#[test]
fn rescans_are_jobs() {
    let mut h = Harness::new(None);
    let receipt = h.ok(Command::Rescan);
    let job = receipt.job.unwrap();
    let finished = h.until_envelope(|e| matches!(e.event, Event::JobFinished { .. }));
    assert_eq!(finished.origin, Some(Origin::Ui));
    let Event::JobFinished {
        job: done,
        kind,
        outcome,
    } = finished.event
    else {
        unreachable!()
    };
    assert_eq!((done, kind), (job, JobKind::Scan));
    assert!(matches!(outcome, JobOutcome::Completed { .. }));
    h.until(|e| matches!(e, Event::LibraryChanged));
}

/// The heard entry is removed (what is heard plays on), then Stop: the snapshot must not
/// point at the removed entry, and Resume continues with its successor, without a
/// `TrackFailed` for the entry the client removed itself.
#[test]
fn stop_after_removing_the_heard_entry_resumes_with_its_successor() {
    let mut h = Harness::new(None);
    let e = h.ok(play(album())).created_entries;
    h.until(|ev| matches!(ev, Event::TrackStarted { entry, .. } if *entry == e[0]));
    h.ok(Command::Next);
    h.until(|ev| matches!(ev, Event::TrackStarted { entry, .. } if *entry == e[1]));
    let removed = h.ok(Command::Remove {
        entries: vec![e[1]],
        expected_revision: None,
    });
    assert_eq!(removed.removed_entries, [e[1]]);
    h.ok(Command::Stop);
    let snapshot = h.snapshot();
    assert_eq!(snapshot.state, PlayState::Stopped);
    assert!(snapshot.current.is_none());
    assert_eq!(snapshot.position, None, "no position in a removed entry");
    assert_eq!(h.err(seek(100)).code, ErrorCode::NothingPlaying);
    h.drain();
    h.ok(Command::Resume);
    let started = h.until(|ev| matches!(ev, Event::TrackStarted { .. }));
    let Event::TrackStarted { entry, .. } = started else {
        unreachable!()
    };
    assert_eq!(entry, e[2], "the removed entry's successor");
    assert!(
        !h.seen
            .iter()
            .any(|ev| matches!(ev.event, Event::TrackFailed { .. })),
        "{:?}",
        h.seen
    );
}

/// A retryable failure is not kept under its idempotency key: the retry runs.
#[test]
fn idempotency_does_not_keep_failures() {
    let catalog = Arc::new(FlakyCatalog {
        inner: MemoryCatalog::standard(),
        fail_next: std::sync::atomic::AtomicBool::new(true),
    });
    let core = CoreHandle::start(
        Box::new(support::null_output()),
        Arc::new(FileResolver),
        catalog as Arc<dyn Catalog>,
        CoreConfig {
            automatic_loudness_scan: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    let request = |id| Request {
        request_id: id,
        idempotency_key: Some("love-1".into()),
        command: Command::SetLoved {
            tracks: vec![TrackId(1)],
            loved: true,
        },
    };
    let agent = Origin::agent("a");
    let CommandResult::Err { error } = core.execute(request(1), agent.clone()).result else {
        panic!()
    };
    assert!(error.details.retryable);
    let CommandResult::Ok(receipt) = core.execute(request(2), agent.clone()).result else {
        panic!("the retry runs the command again")
    };
    assert_eq!(receipt.action, Some(ActionId(7)));
    // Now it succeeded, the key answers retries without running again.
    assert_eq!(
        core.execute(request(3), agent).result,
        CommandResult::Ok(receipt)
    );
}

/// Unknown track ids are named by their place in the request.
#[test]
fn missing_tracks_point_at_their_index() {
    let mut h = Harness::new(None);
    let error = h.err(Command::Enqueue {
        tracks: vec![
            TrackId(1),
            TrackId(404),
            TrackId(2),
            TrackId(404),
            TrackId(405),
        ],
        at: InsertAt::End,
        expected_revision: None,
        expected_current: None,
    });
    assert_eq!(error.details.field.as_deref(), Some("command.tracks[1]"));
    assert_eq!(
        error.details.missing,
        [
            ObjectRef::Track { id: TrackId(404) },
            ObjectRef::Track { id: TrackId(405) }
        ]
    );
}
