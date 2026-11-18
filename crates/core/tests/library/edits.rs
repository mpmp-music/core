//! Edits scenarios.
use super::*;

#[test]
fn playlists_edit_undo_trash_m3u_and_play() {
    let (core, sub, audio, ids, dir) = scanned();

    // A manual playlist: the third track, then the first two before it.
    let created = receipt(execute(
        &core,
        Command::CreatePlaylist {
            name: "Mix".into(),
            kind: PlaylistKind::Manual,
        },
        Origin::Ui,
    ));
    let mix = created.playlist;
    let added = receipt(execute(
        &core,
        Command::AddToPlaylist {
            playlist: mix,
            tracks: vec![ids[2]],
            index: None,
            expected_revision: Some(created.revision),
        },
        Origin::Ui,
    ));
    let added = receipt(execute(
        &core,
        Command::AddToPlaylist {
            playlist: mix,
            tracks: vec![ids[0], ids[1]],
            index: Some(0),
            expected_revision: Some(added.revision),
        },
        Origin::agent("test"),
    ));
    assert_eq!(added.created_items.len(), 2);
    let items = |core: &CoreHandle| match core.query(Query::PlaylistItems {
        playlist: mix,
        offset: 0,
        limit: 10,
        tags: Vec::new(),
    }) {
        QueryResult::PlaylistItems { items, .. } => {
            items.iter().map(|i| i.track.id).collect::<Vec<_>>()
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(items(&core), [ids[0], ids[1], ids[2]]);

    // A stale edit conflicts; undo takes the last edit back.
    let stale = error(execute(
        &core,
        Command::RenamePlaylist {
            playlist: mix,
            name: "Old".into(),
            expected_revision: Some(created.revision),
        },
        Origin::Ui,
    ));
    assert_eq!(
        (stale.kind, stale.code),
        (ErrorKind::Conflict, ErrorCode::PlaylistConflict)
    );
    assert_eq!(stale.details.expected, Some(created.revision.0));
    let undone = receipt(execute(
        &core,
        Command::UndoPlaylist {
            playlist: mix,
            revision: added.revision,
        },
        Origin::Ui,
    ));
    assert_eq!(items(&core), [ids[2]]);
    // Undo is multi-step: the edit before goes too, then there is nothing left.
    receipt(execute(
        &core,
        Command::UndoPlaylist {
            playlist: mix,
            revision: undone.revision,
        },
        Origin::Ui,
    ));
    assert_eq!(items(&core), []);
    let current = match core.query(Query::Playlists {
        deleted: false,
        offset: 0,
        limit: 10,
    }) {
        QueryResult::Playlists { playlists, .. } => playlists[0].revision,
        other => panic!("{other:?}"),
    };
    let nothing = error(execute(
        &core,
        Command::UndoPlaylist {
            playlist: mix,
            revision: current,
        },
        Origin::Ui,
    ));
    assert_eq!(nothing.code, ErrorCode::NothingToUndo);
    let readded = receipt(execute(
        &core,
        Command::AddToPlaylist {
            playlist: mix,
            tracks: vec![ids[2]],
            index: None,
            expected_revision: None,
        },
        Origin::Ui,
    ));

    // Export, import back: the same tracks, and the import lists its items.
    let m3u = dir.path().join("music").join("mix.m3u8");
    receipt(execute(
        &core,
        Command::ExportM3u {
            playlist: mix,
            path: m3u.display().to_string(),
        },
        Origin::Ui,
    ));
    let imported = receipt(execute(
        &core,
        Command::ImportM3u {
            path: m3u.display().to_string(),
            name: None,
        },
        Origin::Ui,
    ));
    let report = imported.import.clone().unwrap();
    assert_eq!((report.matched_count, report.missing.len()), (1, 0));
    assert_eq!(imported.created_items.len(), 1);
    assert_ne!(imported.created_items, readded.created_items);

    // Playing a playlist copies it into the queue.
    let queued = match execute(
        &core,
        Command::Play {
            context: PlayContext::Playlist {
                playlist: imported.playlist,
                start_item: None,
                expected_revision: None,
            },
            start: 0,
            start_track: None,
        },
        Origin::Ui,
    ) {
        CommandResult::Ok(receipt) => receipt.created_entries,
        other => panic!("{other:?}"),
    };
    assert_eq!(queued.len(), 1);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        assert!(Instant::now() < deadline, "the playlist never played");
        if audio.is_open() {
            audio.render_f32(512);
        }
        if sub
            .events
            .try_iter()
            .any(|e| matches!(&e.event, Event::TrackStarted { track, .. } if track.id == ids[2]))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(3));
    }

    // A smart playlist with a limit.
    let smart = receipt(execute(
        &core,
        Command::CreatePlaylist {
            name: "Two".into(),
            kind: PlaylistKind::Smart {
                query: TrackQuery::default(),
                max_tracks: Some(2),
            },
        },
        Origin::Ui,
    ));
    let QueryResult::PlaylistItems {
        items,
        total,
        playlist,
        ..
    } = core.query(Query::PlaylistItems {
        playlist: smart.playlist,
        offset: 0,
        limit: 10,
        tags: Vec::new(),
    })
    else {
        panic!()
    };
    assert_eq!((items.len(), total), (2, 2));
    assert!(items.iter().all(|i| i.item.is_none()));
    assert!(matches!(
        playlist.kind,
        PlaylistKind::Smart {
            max_tracks: Some(2),
            ..
        }
    ));

    // An agent may move a playlist to the trash; purging waits for the user.
    let agent = Origin::agent("test");
    receipt(execute(
        &core,
        Command::DeletePlaylist { playlist: mix },
        agent.clone(),
    ));
    let trash = |core: &CoreHandle| match core.query(Query::Playlists {
        deleted: true,
        offset: 0,
        limit: 10,
    }) {
        QueryResult::Playlists { total, .. } => total,
        other => panic!("{other:?}"),
    };
    assert_eq!(trash(&core), 1);
    let ask = |core: &CoreHandle| {
        let error = error(execute(
            core,
            Command::PurgePlaylists { playlists: None },
            agent.clone(),
        ));
        assert_eq!(error.kind, ErrorKind::NeedsConfirmation);
        let ticket = error.details.confirmation.unwrap();
        assert!(ticket.summary.contains("trash"), "{ticket:?}");
        ticket.token
    };
    let wait_resolved = |token: &str| {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "never resolved");
            if let Some(outcome) = sub.events.try_iter().find_map(|e| match e.event {
                Event::ConfirmationResolved { outcome } if outcome.token == token => Some(outcome),
                _ => None,
            }) {
                return (outcome.approved, outcome.result);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    };

    // Rejected: nothing happens.
    let token = ask(&core);
    assert_eq!(trash(&core), 1);
    // Only the UI may answer.
    let refused = error(execute(
        &core,
        Command::Confirm {
            token: token.clone(),
        },
        agent.clone(),
    ));
    assert_eq!(
        (refused.kind, refused.code),
        (ErrorKind::Forbidden, ErrorCode::OriginNotAllowed)
    );
    execute(
        &core,
        Command::Reject {
            token: token.clone(),
        },
        Origin::Ui,
    );
    assert_eq!(wait_resolved(&token), (false, None));
    assert_eq!(trash(&core), 1);

    // Confirmed: core runs it, and the receipt has the purge's own result; the token
    // cannot be used twice.
    let token = ask(&core);
    let CommandResult::Ok(confirmed) = execute(
        &core,
        Command::Confirm {
            token: token.clone(),
        },
        Origin::Ui,
    ) else {
        panic!()
    };
    let outcome = confirmed.confirmed.unwrap();
    assert!(outcome.approved);
    assert!(
        matches!(outcome.result.as_deref(), Some(CommandResult::Ok(r)) if r.changed),
        "{outcome:?}"
    );
    let (approved, result) = wait_resolved(&token);
    assert!(approved);
    assert!(
        matches!(result.as_deref(), Some(CommandResult::Ok(_))),
        "{result:?}"
    );
    assert_eq!(trash(&core), 0);
    let again = error(execute(&core, Command::Confirm { token }, Origin::Ui));
    assert_eq!(
        (again.kind, again.code),
        (ErrorKind::NotFound, ErrorCode::ConfirmationNotFound)
    );
}

#[test]
fn playlist_edits_are_idempotent_and_checked() {
    let (core, _sub, _audio, ids, _dir) = scanned();
    let p = receipt(execute(
        &core,
        Command::CreatePlaylist {
            name: "P".into(),
            kind: PlaylistKind::Manual,
        },
        Origin::Ui,
    ))
    .playlist;
    let added = receipt(execute(
        &core,
        Command::AddToPlaylist {
            playlist: p,
            tracks: ids.clone(),
            index: None,
            expected_revision: None,
        },
        Origin::Ui,
    ));
    let items = added.created_items.clone();
    assert_eq!(items.len(), 3);

    // Past the end is out of range; at the end appends.
    for (index, ok) in [(4, false), (3, true)] {
        let result = execute(
            &core,
            Command::AddToPlaylist {
                playlist: p,
                tracks: vec![ids[0]],
                index: Some(index),
                expected_revision: None,
            },
            Origin::Ui,
        );
        match result {
            CommandResult::Ok(_) => assert!(ok),
            CommandResult::Err { error } => {
                assert!(!ok);
                assert_eq!(error.code, ErrorCode::OutOfRange);
            }
            other => panic!("{other:?}"),
        }
    }
    let moved = error(execute(
        &core,
        Command::MovePlaylistItem {
            playlist: p,
            item: items[0],
            to: 4,
            expected_revision: None,
        },
        Origin::Ui,
    ));
    assert_eq!(moved.code, ErrorCode::OutOfRange);
    let missing = error(execute(
        &core,
        Command::MovePlaylistItem {
            playlist: p,
            item: PlaylistItemId(999_999),
            to: 0,
            expected_revision: None,
        },
        Origin::Ui,
    ));
    assert_eq!(missing.code, ErrorCode::ItemNotFound);
    assert_eq!(
        missing.details.missing,
        [ObjectRef::PlaylistItem {
            id: PlaylistItemId(999_999)
        }]
    );
    let unknown_track = error(execute(
        &core,
        Command::AddToPlaylist {
            playlist: p,
            tracks: vec![TrackId(424_242)],
            index: None,
            expected_revision: None,
        },
        Origin::Ui,
    ));
    assert_eq!(unknown_track.code, ErrorCode::TrackNotFound);
    assert_eq!(
        unknown_track.details.missing,
        [ObjectRef::Track {
            id: TrackId(424_242)
        }]
    );

    // Removing reports what it removed; ids already gone are not an error.
    let CommandResult::Ok(first) = execute(
        &core,
        Command::RemoveFromPlaylist {
            playlist: p,
            items: vec![items[1], PlaylistItemId(999_999)],
            expected_revision: None,
        },
        Origin::Ui,
    ) else {
        panic!()
    };
    assert!(first.changed);
    let first = first.playlist.unwrap();
    assert_eq!(first.removed_items, [items[1]]);
    let CommandResult::Ok(second) = execute(
        &core,
        Command::RemoveFromPlaylist {
            playlist: p,
            items: vec![items[1]],
            expected_revision: Some(first.revision),
        },
        Origin::Ui,
    ) else {
        panic!()
    };
    assert!(!second.changed);
    let second = second.playlist.unwrap();
    assert!(second.removed_items.is_empty());
    assert_eq!(second.revision, first.revision, "no edit, no new revision");

    // An unknown playlist is not found, by id.
    let QueryResult::Err { error: e } = core.query(Query::PlaylistItems {
        playlist: PlaylistId(999),
        offset: 0,
        limit: 10,
        tags: Vec::new(),
    }) else {
        panic!()
    };
    assert_eq!(e.code, ErrorCode::PlaylistNotFound);
    assert_eq!(
        e.details.missing,
        [ObjectRef::Playlist {
            id: PlaylistId(999)
        }]
    );

    // Paths: an agent needs the user's confirmation, a remote may not.
    let path = Command::ExportM3u {
        playlist: p,
        // Absolute on every platform (agents must name absolute paths).
        path: std::env::temp_dir().join("x.m3u").display().to_string(),
    };
    let e = error(execute(&core, path.clone(), Origin::agent("a")));
    assert_eq!(e.kind, ErrorKind::NeedsConfirmation);
    let e = error(execute(
        &core,
        path,
        Origin::Remote {
            device: "phone".into(),
        },
    ));
    assert_eq!(e.code, ErrorCode::PathNotAllowed);

    // Ratings are 1–5 and show on every track.
    let CommandResult::Ok(_) = execute(
        &core,
        Command::SetRating {
            tracks: vec![ids[1]],
            rating: Some(5),
        },
        Origin::Ui,
    ) else {
        panic!()
    };
    let QueryResult::Tracks {
        tracks,
        total,
        next_offset,
        offset,
    } = core.query(Query::Tracks {
        query: TrackQuery {
            filter: Filter::Compare {
                field: Field::Rating,
                op: CompareOp::Eq,
                value: Value::Int(5),
            },
            sort: Vec::new(),
        },
        offset: 0,
        limit: 10,
        tags: Vec::new(),
    })
    else {
        panic!()
    };
    assert_eq!((offset, total, next_offset), (0, 1, None));
    assert_eq!((tracks[0].id, tracks[0].rating), (ids[1], Some(5)));
    // Pages say where the next one starts.
    let QueryResult::Tracks { next_offset, .. } = core.query(Query::Tracks {
        query: TrackQuery::default(),
        offset: 0,
        limit: 2,
        tags: Vec::new(),
    }) else {
        panic!()
    };
    assert_eq!(next_offset, Some(2));
}

/// Ratings, playlists and folders are recorded as actions (docs/design/queries-playlists-undo.md):
/// receipts name them, `Actions` lists them with their origin, `UndoAction` undoes one
/// unless someone changed its objects since, and undoing an undo redoes it.
#[test]
fn actions_are_recorded_and_undone() {
    let (core, sub, _audio, ids, _dir) = scanned();
    let agent = Origin::agent("helper");

    // An agent rates two tracks, without being asked to confirm.
    let rated = ok(execute(
        &core,
        Command::SetRating {
            tracks: vec![ids[0], ids[1]],
            rating: Some(5),
        },
        agent.clone(),
    ));
    let rate_action = rated.action.expect("recorded");
    assert!(rated.changed);
    let changed = wait_event(&sub, |e| matches!(e.event, Event::TracksChanged { .. }));
    assert_eq!(
        changed.event,
        Event::TracksChanged {
            tracks: vec![ids[0], ids[1]]
        }
    );
    let announced = wait_event(&sub, |e| matches!(e.event, Event::ActionsChanged));
    assert_eq!(announced.origin, Some(agent.clone()));
    // The same rating again changes nothing: no action, no event.
    let again = ok(execute(
        &core,
        Command::SetRating {
            tracks: vec![ids[0]],
            rating: Some(5),
        },
        agent.clone(),
    ));
    assert_eq!((again.changed, again.action), (false, None));
    let noise: Vec<_> = sub.events.try_iter().map(|e| e.event).collect();
    assert!(noise.is_empty(), "a rating no-op emitted {noise:?}");

    let (listed, total) = actions(&core, Some(OriginFilter::Agent { name: None }));
    assert_eq!(total, 1);
    let first = &listed[0];
    assert_eq!(
        (first.id, first.kind, first.target_count, first.undoable),
        (rate_action, ActionKind::SetRating, 2, true)
    );
    assert_eq!(first.origin, agent);
    assert!(first.summary.contains('5'), "{first:?}");
    assert_eq!(
        actions(
            &core,
            Some(OriginFilter::Agent {
                name: Some("someone else".into())
            })
        )
        .1,
        0
    );
    // `limit: 0` only counts.
    let QueryResult::Actions {
        actions: none,
        total,
        next_offset,
        ..
    } = core.query(Query::Actions {
        origin: None,
        offset: 0,
        limit: 0,
    })
    else {
        panic!()
    };
    // The agent's rating and the folder `scanned` added.
    assert_eq!((none.len(), total, next_offset), (0, 2, None));

    // The UI changes one of the tracks since: undoing the agent's action would overwrite
    // that, so it conflicts and names the track.
    let ui_rating = ok(execute(
        &core,
        Command::SetRating {
            tracks: vec![ids[0]],
            rating: Some(2),
        },
        Origin::Ui,
    ))
    .action
    .unwrap();
    let conflict = error(execute(
        &core,
        Command::UndoAction {
            action: rate_action,
        },
        agent.clone(),
    ));
    assert_eq!(
        (conflict.kind, conflict.code),
        (ErrorKind::Conflict, ErrorCode::ActionConflict)
    );
    assert_eq!(conflict.details.changed, [ObjectRef::Track { id: ids[0] }]);
    assert!(
        conflict.details.missing.is_empty(),
        "missing means not found only"
    );
    assert_eq!(ratings(&core, &ids[..2]), [Some(2), Some(5)]);

    // Undo the UI's change first, then the agent's: both tracks are unrated again.
    ok(execute(
        &core,
        Command::UndoAction { action: ui_rating },
        Origin::Ui,
    ));
    let undo = ok(execute(
        &core,
        Command::UndoAction {
            action: rate_action,
        },
        agent.clone(),
    ));
    assert!(undo.changed);
    let undo_action = undo.action.unwrap();
    assert_eq!(ratings(&core, &ids[..2]), [None, None]);
    wait_event(
        &sub,
        |e| matches!(&e.event, Event::TracksChanged { tracks } if tracks.len() == 2),
    );
    let already = error(execute(
        &core,
        Command::UndoAction {
            action: rate_action,
        },
        Origin::Ui,
    ));
    assert_eq!(
        (already.kind, already.code),
        (ErrorKind::Invalid, ErrorCode::AlreadyUndone)
    );
    let unknown = error(execute(
        &core,
        Command::UndoAction {
            action: ActionId(987_654),
        },
        Origin::Ui,
    ));
    assert_eq!(
        (unknown.kind, unknown.code),
        (ErrorKind::NotFound, ErrorCode::ActionNotFound)
    );
    assert_eq!(
        unknown.details.missing,
        [ObjectRef::Action {
            id: ActionId(987_654)
        }]
    );
    // Undoing the undo redoes the rating.
    let redo = ok(execute(
        &core,
        Command::UndoAction {
            action: undo_action,
        },
        Origin::Ui,
    ))
    .action
    .unwrap();
    assert_eq!(ratings(&core, &ids[..2]), [Some(5), Some(5)]);
    let (listed, _) = actions(&core, None);
    let by_id = |id: ActionId| listed.iter().find(|a| a.id == id).unwrap().clone();
    assert_eq!(by_id(redo).kind, ActionKind::Redo);
    assert_eq!(by_id(redo).undo_of, Some(undo_action));
    assert_eq!(by_id(undo_action).kind, ActionKind::Undo);
    assert_eq!(by_id(undo_action).undone_by, Some(redo));
    assert_eq!(by_id(rate_action).undone_by, Some(undo_action));
    assert!(!by_id(rate_action).undoable);
    let (ui_only, _) = actions(&core, Some(OriginFilter::Ui));
    assert!(ui_only.iter().all(|a| a.origin == Origin::Ui));
    // The folder, the rating, its undo, the redo.
    assert_eq!(ui_only.len(), 4);
    assert_eq!(ui_only[3].kind, ActionKind::RootAdd);

    // Playlists: creating one is undone by moving it to the trash; a playlist in the
    // trash cannot be edited, and deleting it again changes nothing.
    let created = ok(execute(
        &core,
        Command::CreatePlaylist {
            name: "Undo me".into(),
            kind: PlaylistKind::Manual,
        },
        agent.clone(),
    ));
    let playlist = created.playlist.unwrap().playlist;
    ok(execute(
        &core,
        Command::UndoAction {
            action: created.action.unwrap(),
        },
        Origin::Ui,
    ));
    wait_event(
        &sub,
        |e| matches!(&e.event, Event::PlaylistsChanged { playlists } if playlists == &[playlist]),
    );
    let deleted = ok(execute(
        &core,
        Command::DeletePlaylist { playlist },
        Origin::Ui,
    ));
    assert_eq!((deleted.changed, deleted.action), (false, None));
    let in_trash = error(execute(
        &core,
        Command::RenamePlaylist {
            playlist,
            name: "x".into(),
            expected_revision: None,
        },
        Origin::Ui,
    ));
    assert_eq!(
        (in_trash.kind, in_trash.code),
        (ErrorKind::Invalid, ErrorCode::PlaylistInTrash)
    );
    let restored = ok(execute(
        &core,
        Command::RestorePlaylist { playlist },
        Origin::Ui,
    ));
    assert!(restored.action.is_some());
    // UndoPlaylist is a shortcut that records its undo too.
    let added = receipt(execute(
        &core,
        Command::AddToPlaylist {
            playlist,
            tracks: vec![ids[2]],
            index: None,
            expected_revision: None,
        },
        Origin::Ui,
    ));
    let undone = ok(execute(
        &core,
        Command::UndoPlaylist {
            playlist,
            revision: added.revision,
        },
        Origin::Ui,
    ));
    let undone_action = undone.action.unwrap();
    let (listed, _) = actions(&core, None);
    assert_eq!(listed[0].id, undone_action);
    assert_eq!(listed[0].kind, ActionKind::Undo);

    // Folders: undoing a removal brings the folder back and rescans it.
    let QueryResult::Roots { roots } = core.query(Query::Roots) else {
        panic!()
    };
    let root = roots[0].id;
    let removed = ok(execute(&core, Command::RemoveRoot { root }, Origin::Ui));
    assert_eq!(removed.root, Some(root));
    let back = ok(execute(
        &core,
        Command::UndoAction {
            action: removed.action.unwrap(),
        },
        Origin::Ui,
    ));
    let job = back.job.expect("a rescan");
    wait_event(
        &sub,
        |e| matches!(&e.event, Event::JobFinished { job: j, .. } if *j == job),
    );
    let QueryResult::Roots { roots } = core.query(Query::Roots) else {
        panic!()
    };
    assert_eq!((roots.len(), roots[0].track_count), (1, 3));
    let QueryResult::Tracks { total, .. } = core.query(Query::Tracks {
        query: TrackQuery::default(),
        offset: 0,
        limit: 10,
        tags: Vec::new(),
    }) else {
        panic!()
    };
    assert_eq!(total, 3, "the folder's tracks are available again");
}

#[test]
fn undoing_a_folder_change_follows_the_local_path_rules() {
    let (core, _sub, _audio, _ids, _dir) = scanned();
    // The UI's AddRoot in `scanned` is the newest (and only) folder action.
    let (listed, _) = actions(&core, Some(OriginFilter::Ui));
    let add = listed
        .iter()
        .find(|a| a.kind == ActionKind::RootAdd)
        .expect("AddRoot is recorded")
        .id;

    // An agent undoing it would remove a folder: the user is asked first.
    let held = error(execute(
        &core,
        Command::UndoAction { action: add },
        Origin::agent("helper"),
    ));
    assert_eq!(held.kind, ErrorKind::NeedsConfirmation, "{held:?}");
    // A remote may not touch library folders at all.
    let refused = error(execute(
        &core,
        Command::UndoAction { action: add },
        Origin::Remote {
            device: "phone".into(),
        },
    ));
    assert_eq!(
        (refused.kind, refused.code),
        (ErrorKind::Forbidden, ErrorCode::PathNotAllowed),
        "{refused:?}"
    );
    // The UI may.
    assert!(
        ok(execute(
            &core,
            Command::UndoAction { action: add },
            Origin::Ui
        ))
        .changed
    );
}

/// Soft deletes answer a repeat alike: removing a removed folder, deleting a playlist in
/// the trash and restoring one that is not in it all succeed with `changed: false`, record
/// nothing and emit nothing. An unknown folder is still `not_found`.
#[test]
fn repeated_soft_deletes_and_restores_change_nothing() {
    let (core, sub, _audio, _ids, _dir) = scanned();
    let QueryResult::Roots { roots } = core.query(Query::Roots) else {
        panic!()
    };
    let root = roots[0].id;
    let removed = ok(execute(&core, Command::RemoveRoot { root }, Origin::Ui));
    assert!(removed.changed && removed.action.is_some(), "{removed:?}");
    let created = receipt(execute(
        &core,
        Command::CreatePlaylist {
            name: "p".into(),
            kind: PlaylistKind::Manual,
        },
        Origin::Ui,
    ));
    let playlist = created.playlist;
    let (_, journal) = actions(&core, None);
    sub.events.try_iter().for_each(drop);

    let agent = Origin::agent("helper");
    for (command, origin) in [
        // The agent is not asked to confirm what changes nothing.
        (Command::RemoveRoot { root }, agent.clone()),
        (Command::RemoveRoot { root }, Origin::Ui),
        (Command::RestorePlaylist { playlist }, agent.clone()),
    ] {
        let again = ok(execute(&core, command.clone(), origin));
        assert!(!again.changed, "{command:?}: {again:?}");
        assert_eq!(again.action, None, "{command:?}");
    }
    let noise: Vec<_> = sub.events.try_iter().map(|e| e.event).collect();
    assert!(noise.is_empty(), "a no-op emitted {noise:?}");
    assert_eq!(actions(&core, None).1, journal, "no-ops record nothing");
    assert!(
        ok(execute(
            &core,
            Command::DeletePlaylist { playlist },
            Origin::Ui
        ))
        .changed
    );
    let (_, after_delete) = actions(&core, None);
    sub.events.try_iter().for_each(drop);
    let again = ok(execute(&core, Command::DeletePlaylist { playlist }, agent));
    assert!(!again.changed && again.action.is_none(), "{again:?}");
    assert_eq!(
        after_delete,
        journal + 1,
        "only the real delete is recorded"
    );
    let noise: Vec<_> = sub.events.try_iter().map(|e| e.event).collect();
    assert!(noise.is_empty(), "a no-op emitted {noise:?}");

    let unknown = error(execute(
        &core,
        Command::RemoveRoot {
            root: RootId(424_242),
        },
        Origin::Ui,
    ));
    assert_eq!(
        (unknown.kind, unknown.code, unknown.details.field.as_deref()),
        (
            ErrorKind::NotFound,
            ErrorCode::RootNotFound,
            Some("command.root")
        )
    );
}

/// `ActionSummary.recorded_at_ms` is Unix time; `Snapshot.now_ms` and
/// `PositionInfo.at_ms` are core's monotonic clock (the action's field used to be called `at_ms` too).
#[test]
fn the_action_time_is_unix_time_and_named_so() {
    let (core, _sub, _audio, _ids, _dir) = scanned();
    let actions: serde_json::Value =
        serde_json::from_str(&core.query_json(r#"{"type":"actions","limit":10}"#)).unwrap();
    let action = &actions["actions"][0];
    assert!(action.get("at_ms").is_none(), "{action}");
    let recorded = action["recorded_at_ms"].as_i64().unwrap();
    assert!(recorded > 1_700_000_000_000, "Unix ms: {recorded}");
    let snapshot: serde_json::Value =
        serde_json::from_str(&core.query_json(r#"{"type":"snapshot"}"#)).unwrap();
    let now_ms = snapshot["snapshot"]["now_ms"].as_u64().unwrap();
    assert!(
        now_ms < 1_000_000_000,
        "monotonic ms since core started: {now_ms}"
    );
}

/// `SetRating.rating` is required: a request that leaves it out is refused, naming the
/// field, instead of clearing the ratings; `null` clears.
#[test]
fn set_rating_needs_an_explicit_rating() {
    let (core, _sub, _audio, ids, _dir) = scanned();
    let send = |command: serde_json::Value| -> serde_json::Value {
        let request = serde_json::json!({"request_id": 1, "command": command});
        let answer = core.execute_json(&request.to_string(), Origin::agent("a"));
        serde_json::from_str(&answer).unwrap()
    };
    let rated = send(serde_json::json!({"type": "set_rating", "tracks": [ids[0].0], "rating": 4}));
    assert_eq!(rated["result"]["changed"], true, "{rated}");
    let forgot = send(serde_json::json!({"type": "set_rating", "tracks": [ids[0].0]}));
    let error = &forgot["result"]["error"];
    assert_eq!(
        (&error["kind"], &error["code"], &error["details"]["field"]),
        (
            &serde_json::json!("invalid"),
            &serde_json::json!("invalid_request"),
            &serde_json::json!("command.rating")
        ),
        "{forgot}"
    );
    assert_eq!(ratings(&core, &ids[..1]), [Some(4)], "nothing was cleared");
    let cleared =
        send(serde_json::json!({"type": "set_rating", "tracks": [ids[0].0], "rating": null}));
    assert_eq!(cleared["result"]["changed"], true, "{cleared}");
    assert_eq!(ratings(&core, &ids[..1]), [None]);
    // The schema says so too.
    let schema = json_schema();
    let set_rating = schema["$defs"]["Command"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| variant_tag(v) == Some("set_rating"))
        .unwrap();
    assert!(
        set_rating["required"]
            .as_array()
            .unwrap()
            .contains(&"rating".into())
    );
}

/// An undo is judged by what it does (undoing a folder change is a folder change), which
/// the static `x-origins` cannot show: the schema says the default (`allow`) and both the
/// command's description and the guarantees say that core may require more.
/// The behaviour itself is `undoing_a_folder_change_follows_the_local_path_rules`.
#[test]
fn the_schema_says_undo_policies_are_defaults() {
    let schema = json_schema();
    let undo = schema["$defs"]["Command"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| variant_tag(v) == Some("undo_action"))
        .unwrap();
    assert_eq!(undo["x-origins"]["agent"], "allow");
    let description = undo["description"].as_str().unwrap();
    assert!(
        description.contains("An undo is judged by what it does"),
        "{description}"
    );
    // Compared with whitespace collapsed: the guarantees are wrapped Markdown.
    let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let guarantees = words(schema["description"].as_str().unwrap());
    for needle in [
        "**default** policy",
        "core may require more than the default",
        "An undo is judged by what the undo itself does",
        "handle `needs_confirmation` and `forbidden` for any command",
        "never granted",
    ] {
        assert!(guarantees.contains(needle), "{needle}");
    }
}
