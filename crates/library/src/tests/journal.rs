//! Action journal tests (docs/design/queries-playlists-undo.md), and the library reproductions of the
//! API v1 semantic regressions.

use std::time::{Duration, Instant};

use mpmp_test_support::TempDir;

use crate::db::writer::now_ms;
use crate::tests::{UI, fixture, meta, record, upsert};
use crate::track_query::{CompareOp, Field, Filter, TrackQuery, Value};
use crate::{
    ACTION_LIMIT, AUDIT_LIMIT, ActionId, ActionMeta, ActionTarget, Done, Library, LibraryError,
    Page, PlaylistKind, RootId, TrackId,
};

const ALL: Page = Page::new(0, 1000);

const AGENT: ActionMeta<'static> = ActionMeta::new("agent:helper");

const DAY: i64 = 24 * 60 * 60 * 1000;

fn code<T: std::fmt::Debug>(result: crate::Result<T>) -> &'static str {
    match result {
        Err(err) => err.code(),
        Ok(value) => panic!("expected an error, got {value:?}"),
    }
}

fn action<T>(done: Done<T>) -> ActionId {
    done.action.expect("an action was recorded")
}

/// `n` tracks "t0".. under the fixture's root (no files).
fn tracks(lib: &Library, root: RootId, n: usize) -> Vec<TrackId> {
    upsert(
        lib,
        (0..n)
            .map(|i| record(root, &format!("{i}.flac"), meta(&format!("t{i}"), "", "")))
            .collect(),
    );
    let (rows, total) = lib.tracks_query(&TrackQuery::default(), ALL, &[]).unwrap();
    assert_eq!(total as usize, n);
    let mut ids: Vec<TrackId> = rows.into_iter().map(|r| r.track.id).collect();
    ids.sort();
    ids
}

/// `(rating, loved)` of each track.
fn user(lib: &Library, ids: &[TrackId]) -> Vec<(Option<u8>, bool)> {
    lib.track_rows(ids, &[])
        .unwrap()
        .iter()
        .map(|r| (r.rating, r.loved))
        .collect()
}

fn ratings(lib: &Library, ids: &[TrackId]) -> Vec<Option<u8>> {
    user(lib, ids).into_iter().map(|(r, _)| r).collect()
}

fn conflict<T: std::fmt::Debug>(result: crate::Result<T>) -> Vec<ActionTarget> {
    match result {
        Err(LibraryError::ActionConflict { changed, .. }) => changed,
        other => panic!("expected an action conflict, got {other:?}"),
    }
}

fn db_path(lib: &Library) -> std::path::PathBuf {
    lib.inner().pool.path().to_owned()
}

fn raw(lib: &Library) -> rusqlite::Connection {
    rusqlite::Connection::open(db_path(lib)).unwrap()
}

#[test]
fn ratings_undo_and_redo() {
    let f = fixture();
    let lib = &f.lib;
    let t = tracks(lib, f.root, 4);
    lib.set_rating(&t[..1], Some(2), UI).unwrap();

    let rate = lib.set_rating(&t[..3], Some(4), AGENT).unwrap();
    assert_eq!(rate.value, &t[..3]);
    let rate = action(rate);
    assert_eq!(ratings(lib, &t), [Some(4), Some(4), Some(4), None]);
    // The same rating again (and unknown ids) change nothing: no action.
    let again = lib
        .set_rating(&[t[1], t[2], TrackId(999)], Some(4), AGENT)
        .unwrap();
    assert_eq!(again, Done::unchanged(vec![]));

    let summary = lib.action(rate).unwrap().unwrap();
    assert_eq!(
        (summary.kind.as_str(), summary.summary.as_str()),
        ("set_rating", "rate 3 tracks 4 stars")
    );
    assert_eq!(
        (summary.origin.as_str(), summary.target_count),
        ("agent:helper", 3)
    );
    assert!(summary.undoable);

    // Undo: the previous values come back, for exactly those tracks.
    let undo = lib.undo_action(rate, UI).unwrap();
    assert_eq!(undo.value.undone, rate);
    assert_eq!(undo.value.tracks, &t[..3]);
    assert!(undo.value.playlists.is_empty() && undo.value.roots.is_empty());
    let undo = action(undo);
    assert_eq!(ratings(lib, &t), [Some(2), None, None, None]);
    let undone = lib.action(rate).unwrap().unwrap();
    assert_eq!((undone.undone_by, undone.undoable), (Some(undo), false));
    let summary = lib.action(undo).unwrap().unwrap();
    assert_eq!(
        (summary.kind.as_str(), summary.summary.as_str()),
        ("undo", "undo: rate 3 tracks 4 stars")
    );
    assert_eq!(
        (summary.undo_of, summary.origin.as_str()),
        (Some(rate), "ui")
    );
    assert_eq!(code(lib.undo_action(rate, UI)), "already_undone");
    assert_eq!(
        code(lib.undo_action(ActionId(9999), UI)),
        "action_not_found"
    );

    // Undoing the undo is a redo; undoing that undoes again.
    let redo = action(lib.undo_action(undo, UI).unwrap());
    assert_eq!(ratings(lib, &t), [Some(4), Some(4), Some(4), None]);
    let summary = lib.action(redo).unwrap().unwrap();
    assert_eq!(
        (summary.kind.as_str(), summary.summary.as_str()),
        ("redo", "redo: rate 3 tracks 4 stars")
    );
    let again = action(lib.undo_action(redo, UI).unwrap());
    assert_eq!(ratings(lib, &t), [Some(2), None, None, None]);
    assert_eq!(lib.action(again).unwrap().unwrap().kind, "undo");

    // Clearing, one track: named in the summary.
    let clear = action(lib.set_rating(&t[..1], None, UI).unwrap());
    assert_eq!(
        lib.action(clear).unwrap().unwrap().summary,
        "clear the rating of \"t0\""
    );
    lib.undo_action(clear, UI).unwrap();
    assert_eq!(ratings(lib, &t[..1]), [Some(2)]);
}

#[test]
fn loved_undo_and_redo() {
    let f = fixture();
    let lib = &f.lib;
    let t = tracks(lib, f.root, 3);
    lib.set_loved(&t[..1], true, UI).unwrap();
    let love = lib.set_loved(&t, true, AGENT).unwrap();
    assert_eq!(love.value, &t[1..]);
    let love = action(love);
    assert_eq!(lib.action(love).unwrap().unwrap().summary, "love 2 tracks");
    assert!(user(lib, &t).iter().all(|&(_, loved)| loved));
    let undo = action(lib.undo_action(love, UI).unwrap());
    assert_eq!(user(lib, &t), [(None, true), (None, false), (None, false)]);
    lib.undo_action(undo, UI).unwrap();
    assert!(user(lib, &t).iter().all(|&(_, loved)| loved));
    let unlove = action(lib.set_loved(&t[2..], false, UI).unwrap());
    assert_eq!(
        lib.action(unlove).unwrap().unwrap().summary,
        "unlove \"t2\""
    );
}

#[test]
fn undo_conflicts_with_later_changes_by_others() {
    let f = fixture();
    let lib = &f.lib;
    let t = tracks(lib, f.root, 5);
    // An agent rates everything; the user then changes two of the tracks.
    let agent = action(lib.set_rating(&t, Some(5), AGENT).unwrap());
    let rerate = action(lib.set_rating(&[t[1]], Some(3), UI).unwrap());
    let love = action(lib.set_loved(&[t[3]], true, UI).unwrap());

    // The agent's action cannot be undone over them: only the changed tracks are listed,
    // and nothing changes.
    let changed = conflict(lib.undo_action(agent, UI));
    assert_eq!(
        changed,
        [
            ActionTarget::Track { id: t[1] },
            ActionTarget::Track { id: t[3] }
        ]
    );
    assert_eq!(code(lib.undo_action(agent, UI)), "action_conflict");
    assert_eq!(
        ratings(lib, &t),
        [Some(5), Some(3), Some(5), Some(5), Some(5)]
    );
    // Undoing the later changes clears the way (rating and loved share one version).
    lib.undo_action(love, UI).unwrap();
    assert_eq!(
        conflict(lib.undo_action(agent, UI)),
        [ActionTarget::Track { id: t[1] }]
    );
    lib.undo_action(rerate, UI).unwrap();
    lib.undo_action(agent, UI).unwrap();
    assert_eq!(ratings(lib, &t), [None; 5]);
    // A change made after an undo blocks redoing it.
    let undo = lib.action(agent).unwrap().unwrap().undone_by.unwrap();
    lib.set_rating(&[t[4]], Some(1), UI).unwrap();
    assert_eq!(
        conflict(lib.undo_action(undo, UI)),
        [ActionTarget::Track { id: t[4] }]
    );
}

#[test]
fn playlist_undo_by_action_and_by_playlist() {
    let f = fixture();
    let lib = &f.lib;
    let t = tracks(lib, f.root, 3);
    let p = lib
        .create_playlist("mix", PlaylistKind::Manual, UI)
        .unwrap()
        .value;
    let e1 = lib
        .add_to_playlist(p, &t[..2], None, Some(0), AGENT)
        .unwrap();
    let (items, r1) = e1.value.clone();
    let e1 = action(e1);
    let e2 = lib.rename_playlist(p, "x", Some(r1), AGENT).unwrap();
    let r2 = e2.value;
    let e2 = action(e2);
    let e3 = action(
        lib.add_to_playlist(p, &t[2..], None, Some(r2), AGENT)
            .unwrap(),
    );
    let items_of = |lib: &Library| -> Vec<_> {
        let (_, entries, _) = lib.playlist_items(p, ALL, &[]).unwrap();
        entries.iter().map(|e| e.item.unwrap()).collect()
    };
    let name = |lib: &Library| lib.playlist(p).unwrap().unwrap().name;

    // The rename cannot be undone while the add after it is in effect.
    assert_eq!(
        conflict(lib.undo_action(e2, UI)),
        [ActionTarget::Playlist { id: p }]
    );
    // The playlist undo takes the newest edit...
    let u3 = lib.undo_playlist(p, 3, UI).unwrap();
    assert_eq!(u3.value, 4);
    let u3 = action(u3);
    assert_eq!(lib.action(e3).unwrap().unwrap().undone_by, Some(u3));
    assert_eq!(items_of(lib), items);
    //...after which the rename can be undone by id...
    let u2 = lib.undo_action(e2, UI).unwrap();
    assert_eq!(u2.value.playlists, [p]);
    let u2 = action(u2);
    assert_eq!(name(lib), "mix");
    //...and the playlist undo goes on with the edit before.
    assert_eq!(lib.undo_playlist(p, 5, UI).unwrap().value, 6);
    assert!(items_of(lib).is_empty());
    assert_eq!(code(lib.undo_playlist(p, 6, UI)), "nothing_to_undo");
    assert_eq!(code(lib.undo_playlist(p, 5, UI)), "playlist_conflict");

    // Redo the first add (the same item ids come back); the playlist undo takes it back.
    let u1 = lib.action(e1).unwrap().unwrap().undone_by.unwrap();
    let r = lib.undo_action(u1, UI).unwrap();
    assert_eq!(lib.action(action(r)).unwrap().unwrap().kind, "redo");
    assert_eq!(items_of(lib), items);
    assert_eq!(lib.playlist(p).unwrap().unwrap().revision, 7);
    // Redoing the rename needs the add in effect: fine now.
    lib.undo_action(u2, UI).unwrap();
    assert_eq!(name(lib), "x");
    assert_eq!(lib.undo_playlist(p, 8, UI).unwrap().value, 9);
    assert_eq!(name(lib), "mix");
    assert_eq!(lib.undo_playlist(p, 9, UI).unwrap().value, 10);
    assert!(items_of(lib).is_empty());
    // The third add's redo needs the state before it (items and the new name).
    assert_eq!(
        conflict(lib.undo_action(u3, UI)),
        [ActionTarget::Playlist { id: p }]
    );
    assert_eq!(code(lib.undo_playlist(p, 10, UI)), "nothing_to_undo");

    // The journal lists all of it, newest first.
    let kinds: Vec<String> = lib
        .actions(None, ALL)
        .unwrap()
        .items
        .into_iter()
        .filter(|a| a.kind != "root_add")
        .map(|a| a.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "undo",
            "undo",
            "redo",
            "redo",
            "undo",
            "undo",
            "undo",
            "playlist_add",
            "playlist_rename",
            "playlist_add",
            "playlist_create"
        ]
    );
}

#[test]
fn playlist_create_delete_restore_import_undo() {
    let f = fixture();
    let lib = &f.lib;
    let t = tracks(lib, f.root, 2);
    let create = lib
        .create_playlist("new", PlaylistKind::Manual, UI)
        .unwrap();
    let p = create.value;
    let create = action(create);
    let (_, r) = lib.add_to_playlist(p, &t, None, None, UI).unwrap().value;
    // Undoing the creation moves it to the trash (nothing is lost)...
    let undo = action(lib.undo_action(create, UI).unwrap());
    let s = lib.playlist(p).unwrap().unwrap();
    assert!(s.deleted_at.is_some());
    assert_eq!((s.revision, s.track_count), (r, 2));
    assert_eq!(
        code(lib.add_to_playlist(p, &t, None, None, UI)),
        "playlist_in_trash"
    );
    //...and redoing it takes it out.
    lib.undo_action(undo, UI).unwrap();
    assert!(lib.playlist(p).unwrap().unwrap().deleted_at.is_none());

    let delete = lib.delete_playlist(p, AGENT).unwrap();
    assert!(delete.changed());
    let delete = action(delete);
    assert_eq!(
        lib.action(delete).unwrap().unwrap().summary,
        "move playlist \"new\" to the trash"
    );
    assert_eq!(
        lib.actions(None, Page::new(0, 1)).unwrap().items[0].id,
        delete
    );
    let restore = lib.undo_action(delete, UI).unwrap();
    assert_eq!(restore.value.playlists, [p]);
    assert!(lib.playlist(p).unwrap().unwrap().deleted_at.is_none());
    // Restoring and its undo.
    let delete = action(lib.delete_playlist(p, UI).unwrap());
    let restore = action(lib.restore_playlist(p, UI).unwrap());
    lib.undo_action(restore, UI).unwrap();
    assert!(lib.playlist(p).unwrap().unwrap().deleted_at.is_some());
    // A delete is not undone over a later restore and delete.
    lib.undo_action(lib.action(restore).unwrap().unwrap().undone_by.unwrap(), UI)
        .unwrap();
    lib.delete_playlist(p, UI).unwrap();
    assert_eq!(
        conflict(lib.undo_action(delete, UI)),
        [ActionTarget::Playlist { id: p }]
    );

    // An import is undone like a creation.
    let lists = f._dir.path().join("lists");
    std::fs::create_dir(&lists).unwrap();
    let m3u = lists.join("trip.m3u8");
    std::fs::write(&m3u, "../music/0.flac\n../music/1.flac\n").unwrap();
    let import = lib.import_m3u(&m3u, None, UI).unwrap();
    let (q, report) = import.value.clone();
    assert_eq!(report.matched, 2);
    let import = action(import);
    assert_eq!(
        lib.action(import).unwrap().unwrap().summary,
        "import playlist \"trip\" with 2 tracks"
    );
    lib.undo_action(import, UI).unwrap();
    assert!(lib.playlist(q).unwrap().unwrap().deleted_at.is_some());

    // Purging drops the playlist's actions.
    lib.purge_playlists(None, UI).unwrap();
    assert_eq!(code(lib.undo_action(import, UI)), "action_not_found");
    assert!(lib.action(create).unwrap().is_none());
}

#[test]
fn root_remove_and_add_undo() {
    let f = fixture();
    let lib = &f.lib;
    let t = tracks(lib, f.root, 2);
    let added = lib.actions(None, ALL).unwrap().items;
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].kind, "root_add");
    let add = added[0].id;
    // Adding it again changes nothing.
    let again = lib.add_root(&f._dir.path().join("music"), UI).unwrap();
    assert_eq!(again, Done::unchanged(f.root));

    let remove = lib.remove_root(f.root, AGENT).unwrap();
    let remove = action(remove);
    assert!(lib.roots().unwrap().is_empty());
    assert!(!lib.track(t[0]).unwrap().unwrap().available);
    // Undo: the root is active again; its tracks come back with the next scan.
    let undo = lib.undo_action(remove, UI).unwrap();
    assert_eq!(undo.value.roots, [f.root]);
    let undo = action(undo);
    assert_eq!(lib.roots().unwrap().len(), 1);
    // Redo the removal: the add cannot be undone over it, only once it is undone.
    let redo = action(lib.undo_action(undo, UI).unwrap());
    assert!(lib.roots().unwrap().is_empty());
    assert_eq!(
        conflict(lib.undo_action(add, UI)),
        [ActionTarget::Root { id: f.root }]
    );
    lib.undo_action(redo, UI).unwrap();
    lib.undo_action(add, UI).unwrap();
    assert!(lib.roots().unwrap().is_empty());

    // A restore that would overlap an active root fails and changes nothing.
    let inner = f._dir.path().join("music/sub");
    std::fs::create_dir(&inner).unwrap();
    let sub = lib.add_root(&inner, UI).unwrap().value;
    let undo_add = lib.action(add).unwrap().unwrap().undone_by.unwrap();
    assert_eq!(code(lib.undo_action(undo_add, UI)), "root_overlaps");
    assert_eq!(lib.roots().unwrap()[0].id, sub);
}

#[test]
fn retention_drops_old_actions() {
    let f = fixture();
    let lib = &f.lib;
    let t = tracks(lib, f.root, 2);
    let old = action(lib.set_rating(&t[..1], Some(1), UI).unwrap());
    let recent = action(lib.set_rating(&t[1..], Some(2), UI).unwrap());
    raw(lib)
        .execute(
            "UPDATE actions SET at_ms = ?2 WHERE id = ?1",
            [old.0, now_ms() - 31 * DAY],
        )
        .unwrap();
    // Listed until the journal is pruned, but no longer undoable.
    assert!(!lib.action(old).unwrap().unwrap().undoable);
    assert_eq!(code(lib.undo_action(old, UI)), "action_not_found");
    assert!(lib.action(old).unwrap().is_none());
    assert!(lib.action(recent).unwrap().unwrap().undoable);

    // At most ACTION_LIMIT actions: the oldest go first.
    raw(lib)
        .execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?2)
             INSERT INTO actions (at_ms, origin, kind, summary, target_count)
             SELECT ?1, 'agent:bulk', 'set_loved', 'filler', 0 FROM n",
            [now_ms(), ACTION_LIMIT as i64],
        )
        .unwrap();
    let newest = action(lib.set_loved(&t, true, UI).unwrap());
    let all = lib.actions(None, Page::new(0, 1)).unwrap();
    assert_eq!(all.total, ACTION_LIMIT as u64);
    assert_eq!(all.items[0].id, newest);
    assert_eq!(code(lib.undo_action(recent, UI)), "action_not_found");
    lib.undo_action(newest, UI).unwrap();
}

#[test]
fn playlist_keeps_50_undoable_actions() {
    let f = fixture();
    let lib = &f.lib;
    let t = tracks(lib, f.root, 1);
    let p = lib
        .create_playlist("p", PlaylistKind::Manual, UI)
        .unwrap()
        .value;
    let adds: Vec<ActionId> = (0..60)
        .map(|_| action(lib.add_to_playlist(p, &t, None, None, UI).unwrap()))
        .collect();
    // The 10 oldest edits are beyond the playlist's 50 steps.
    assert!(!lib.action(adds[9]).unwrap().unwrap().undoable);
    assert!(lib.action(adds[10]).unwrap().unwrap().undoable);
    assert_eq!(code(lib.undo_action(adds[9], UI)), "action_not_found");
    // The playlist's creation is not an edit: not capped.
    let create = lib.actions(None, ALL).unwrap().total;
    assert!(create > 60);
    let created = lib.actions(None, Page::new(60, 1)).unwrap().items[0].clone();
    assert_eq!(created.kind, "playlist_create");
    assert!(created.undoable);
}

#[test]
fn journal_survives_restart() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("library.sqlite");
    let music = dir.path().join("m");
    std::fs::create_dir(&music).unwrap();
    let (root, t, p, rate, rename, remove) = {
        let lib = Library::open(&db).unwrap();
        let root = lib.add_root(&music, UI).unwrap().value;
        let t = tracks(&lib, root, 2);
        let rate = action(lib.set_rating(&t, Some(3), AGENT).unwrap());
        let p = lib
            .create_playlist("p", PlaylistKind::Manual, UI)
            .unwrap()
            .value;
        let rename = action(lib.rename_playlist(p, "q", None, UI).unwrap());
        let remove = action(lib.remove_root(root, UI).unwrap());
        (root, t, p, rate, rename, remove)
    };
    let lib = Library::open(&db).unwrap();
    assert_eq!(lib.actions(None, ALL).unwrap().total, 5);
    assert_eq!(lib.actions(Some("agent:"), ALL).unwrap().items[0].id, rate);
    lib.undo_action(remove, UI).unwrap();
    assert_eq!(lib.roots().unwrap()[0].id, root);
    lib.undo_action(rate, UI).unwrap();
    assert_eq!(ratings(&lib, &t), [None, None]);
    let undo = action(lib.undo_action(rename, UI).unwrap());
    assert_eq!(lib.playlist(p).unwrap().unwrap().name, "p");
    drop(lib);
    let lib = Library::open(&db).unwrap();
    // Redo after another restart.
    lib.undo_action(undo, UI).unwrap();
    assert_eq!(lib.playlist(p).unwrap().unwrap().name, "q");
}

#[test]
fn actions_filter_by_origin() {
    let f = fixture();
    let lib = &f.lib;
    let t = tracks(lib, f.root, 1);
    lib.set_loved(&t, true, ActionMeta::new("agent:a")).unwrap();
    lib.set_loved(&t, false, ActionMeta::new("agent:b"))
        .unwrap();
    lib.set_loved(&t, true, ActionMeta::new("agentx")).unwrap();
    lib.set_loved(&t, false, ActionMeta::new("remote:phone"))
        .unwrap();
    let origins = |filter: Option<&str>| -> Vec<String> {
        lib.actions(filter, ALL)
            .unwrap()
            .items
            .into_iter()
            .map(|a| a.origin)
            .collect()
    };
    assert_eq!(origins(Some("agent:")), ["agent:b", "agent:a"]);
    assert_eq!(origins(Some("agent:a")), ["agent:a"]);
    assert_eq!(origins(Some("agent")), Vec::<String>::new());
    assert_eq!(origins(Some("ui")), ["ui"]);
    assert_eq!(origins(None).len(), 5);
    let page = lib.actions(None, Page::new(1, 2)).unwrap();
    assert_eq!(page.total, 5);
    assert_eq!(
        page.items
            .iter()
            .map(|a| a.origin.as_str())
            .collect::<Vec<_>>(),
        ["agentx", "agent:b"]
    );
}

/// An action whose stored inverse cannot be read is reported once, then skipped: older
/// actions can be undone past it.
#[test]
fn corrupt_actions_are_reported_once_then_skipped() {
    let f = fixture();
    let lib = &f.lib;
    let t = tracks(lib, f.root, 2);
    let p = lib
        .create_playlist("m", PlaylistKind::Manual, UI)
        .unwrap()
        .value;
    let (first, r1) = lib
        .add_to_playlist(p, &t[..1], None, Some(0), UI)
        .unwrap()
        .value;
    let second = lib.add_to_playlist(p, &t[1..], None, Some(r1), UI).unwrap();
    let r2 = second.value.1;
    let second = action(second);
    raw(lib)
        .execute(
            "UPDATE actions SET inverse = x'03ff' WHERE id = ?1",
            [second.0],
        )
        .unwrap();
    assert_eq!(code(lib.undo_playlist(p, r2, UI)), "corrupt_data");
    // Nothing changed; next time the older edit is undone.
    assert_eq!(lib.playlist(p).unwrap().unwrap().revision, r2);
    assert_eq!(lib.undo_playlist(p, r2, UI).unwrap().value, r2 + 1);
    let (_, entries, _) = lib.playlist_items(p, ALL, &[]).unwrap();
    assert_eq!(entries.len(), 1);
    assert_ne!(entries[0].item, Some(first[0]));
    assert_eq!(code(lib.undo_action(second, UI)), "action_not_found");
    assert!(!lib.action(second).unwrap().unwrap().undoable);

    // By id: the same.
    let rate = action(lib.set_rating(&t, Some(5), UI).unwrap());
    raw(lib)
        .execute("UPDATE actions SET inverse = x'01' WHERE id = ?1", [rate.0])
        .unwrap();
    assert_eq!(code(lib.undo_action(rate, UI)), "corrupt_data");
    assert_eq!(code(lib.undo_action(rate, UI)), "action_not_found");
    assert_eq!(ratings(lib, &t), [Some(5), Some(5)]);
}

/// Ratings over thousands of tracks are one transaction and one action:
/// `cargo test -p mpmp-library --release -- batch_rating_of_10k_tracks --nocapture`.
#[test]
fn batch_rating_of_10k_tracks() {
    const N: usize = 10_000;
    let f = fixture();
    let lib = &f.lib;
    upsert(
        lib,
        (0..N)
            .map(|i| record(f.root, &format!("{i}.flac"), meta(&format!("t{i}"), "", "")))
            .collect(),
    );
    let ids = lib.track_ids(&TrackQuery::default(), usize::MAX).unwrap();
    assert_eq!(ids.len(), N);
    let time = |label: &str, run: &mut dyn FnMut()| {
        let start = Instant::now();
        run();
        eprintln!("{label:<28} {:>9.2?}", start.elapsed());
    };
    let mut rate = None;
    time("rate 10k tracks", &mut || {
        rate = Some(action(lib.set_rating(&ids, Some(5), AGENT).unwrap()));
    });
    let rate = rate.unwrap();
    let mut undo = None;
    time("undo it", &mut || {
        undo = Some(action(lib.undo_action(rate, UI).unwrap()));
    });
    time("redo it", &mut || {
        lib.undo_action(undo.unwrap(), UI).unwrap();
    });
    time("love 10k tracks", &mut || {
        lib.set_loved(&ids, true, AGENT).unwrap();
    });
    let rated = lib
        .tracks_query(
            &TrackQuery {
                filter: Filter::Compare {
                    field: Field::Rating,
                    op: CompareOp::Eq,
                    value: Value::Int(5),
                },
                sort: vec![],
            },
            Page::new(0, 1),
            &[],
        )
        .unwrap()
        .1;
    assert_eq!(rated, N as u64);
    let bytes: i64 = raw(lib)
        .query_row(
            "SELECT length(inverse) FROM actions WHERE id = ?1",
            [rate.0],
            |r| r.get(0),
        )
        .unwrap();
    eprintln!("inverse of the 10k rating: {bytes} bytes");
}

#[test]
fn relative_smart_playlist_page_total_matches_its_rows() {
    let f = fixture();
    let mut r = record(f.root, "a/0.flac", meta("t0", "Artist", "Album"));
    // Modified "in the future": matches `modified_at >= now` for ~1.5 s.
    r.mtime_ms = now_ms() + 1500;
    upsert(&f.lib, vec![r]);
    let query = TrackQuery {
        filter: Filter::Compare {
            field: Field::ModifiedAt,
            op: CompareOp::Ge,
            value: Value::DaysAgo(0),
        },
        sort: vec![],
    };
    let p = f
        .lib
        .create_playlist(
            "recent",
            PlaylistKind::Smart {
                query,
                max_tracks: None,
            },
            UI,
        )
        .unwrap()
        .value;
    let (summary, entries) = f.lib.playlist_page(p, ALL, &[]).unwrap().unwrap();
    assert_eq!((summary.track_count, entries.len()), (1, 1));
    // The listing caches the count.
    assert_eq!(f.lib.playlists(false, ALL).unwrap().0[0].track_count, 1);
    std::thread::sleep(Duration::from_millis(2000));
    let (summary, entries) = f.lib.playlist_page(p, ALL, &[]).unwrap().unwrap();
    assert_eq!((summary.track_count, entries.len()), (0, 0));
    let (_, entries, total) = f.lib.playlist_items(p, ALL, &[]).unwrap();
    assert_eq!((total, entries.len()), (0, 0));
}

#[test]
fn what_cannot_be_undone_leaves_an_audit_entry() {
    let f = fixture();
    let lib = &f.lib;
    let t = tracks(lib, f.root, 2);
    let mk = |name: &str| {
        lib.create_playlist(name, PlaylistKind::Manual, UI)
            .unwrap()
            .value
    };
    let (a, b, c) = (mk("A"), mk("B"), mk("C"));
    lib.add_to_playlist(a, &t, None, None, UI).unwrap();
    // Purging an empty trash changes nothing and records nothing.
    let empty = lib.purge_playlists(None, UI).unwrap();
    assert_eq!((empty.value, empty.action), (0, None));
    for p in [a, b, c] {
        lib.delete_playlist(p, UI).unwrap();
    }
    let done = lib
        .purge_playlists(Some(&[b, c]), AGENT.with_grant(Some(3)))
        .unwrap();
    assert_eq!(done.value, 2);
    let entry = lib.action(done.action.unwrap()).unwrap().unwrap();
    assert_eq!(
        (
            entry.kind.as_str(),
            entry.summary.as_str(),
            entry.origin.as_str(),
            entry.target_count,
            entry.undoable,
            entry.grant
        ),
        (
            "purge",
            "permanently delete 2 playlists (\"B\" and \"C\")",
            "agent:helper",
            2,
            false,
            Some(3)
        )
    );
    assert_eq!(
        code(lib.undo_action(entry.id, UI)),
        "action_not_found",
        "an audit entry cannot be undone"
    );
    let last = lib.purge_playlists(None, UI).unwrap();
    let entry = lib.action(last.action.unwrap()).unwrap().unwrap();
    assert_eq!(
        (entry.summary.as_str(), entry.origin.as_str(), entry.grant),
        ("permanently delete playlist \"A\"", "ui", None)
    );
    // The purged playlist's own actions are gone; the audit entries stay.
    let kinds: Vec<String> = lib
        .actions(None, ALL)
        .unwrap()
        .items
        .into_iter()
        .map(|a| a.kind)
        .collect();
    assert_eq!(&kinds[..2], ["purge", "purge"]);
    assert!(!kinds.contains(&"playlist_add".to_owned()));
    let empty = lib.purge_playlists(None, UI).unwrap();
    assert_eq!((empty.value, empty.action), (0, None));

    // Exporting to a new file is not an audit entry; replacing a file is.
    let p = mk("Mix");
    lib.add_to_playlist(p, &t, None, None, UI).unwrap();
    let file = f._dir.path().join("mix.m3u");
    let first = lib.export_m3u(p, &file, true, false, UI).unwrap();
    assert_eq!((first.value, first.action), (false, None));
    let again = lib.export_m3u(p, &file, true, false, AGENT).unwrap();
    assert!(again.value);
    let entry = lib.action(again.action.unwrap()).unwrap().unwrap();
    assert_eq!(entry.kind, "file_write");
    assert_eq!(entry.origin, "agent:helper");
    assert!(!entry.undoable);
    assert!(
        entry.summary.starts_with("replace the file "),
        "{}",
        entry.summary
    );
    assert!(
        entry.summary.ends_with("with playlist \"Mix\""),
        "{}",
        entry.summary
    );
    // Asked to (core does for agents), a new file is recorded too, and said to be new.
    let created = lib
        .export_m3u(p, &f._dir.path().join("new.m3u"), true, true, AGENT)
        .unwrap();
    assert!(!created.value, "nothing replaced");
    let entry = lib.action(created.action.unwrap()).unwrap().unwrap();
    assert_eq!((entry.kind.as_str(), entry.undoable), ("file_write", false));
    assert!(
        entry.summary.starts_with("write the new file "),
        "{}",
        entry.summary
    );
}

/// Audit entries are kept apart from actions: they do not use up the journal's 10 000, and
/// go only when both older than a year and beyond the newest 10 000 audit entries.
#[test]
fn audit_entries_outlast_the_journal() {
    let f = fixture();
    let lib = &f.lib;
    let t = tracks(lib, f.root, 1);
    let mk = |name: &str| {
        lib.create_playlist(name, PlaylistKind::Manual, UI)
            .unwrap()
            .value
    };
    let doomed = mk("Old");
    lib.delete_playlist(doomed, UI).unwrap();
    let audit = action(lib.purge_playlists(None, UI).unwrap());
    let age = |id: ActionId, days: i64| {
        raw(lib)
            .execute(
                "UPDATE actions SET at_ms = ?2 WHERE id = ?1",
                [id.0, now_ms() - days * DAY],
            )
            .unwrap();
    };
    // Older than the journal keeps actions, younger than a year: kept.
    age(audit, 200);
    lib.set_rating(&t, Some(3), UI).unwrap();
    assert!(lib.action(audit).unwrap().is_some());
    // Older than a year, but among the newest 10 000 audit entries: kept.
    age(audit, 400);
    lib.set_rating(&t, Some(4), UI).unwrap();
    assert!(lib.action(audit).unwrap().is_some());
    // A full journal of actions does not push it out.
    raw(lib)
        .execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?2)
             INSERT INTO actions (at_ms, origin, kind, summary, target_count)
             SELECT ?1, 'agent:bulk', 'set_loved', 'filler', 0 FROM n",
            [now_ms(), ACTION_LIMIT as i64 + 5],
        )
        .unwrap();
    lib.set_rating(&t, Some(5), UI).unwrap();
    assert!(lib.action(audit).unwrap().is_some());
    let all = lib.actions(None, Page::new(0, 0)).unwrap();
    assert_eq!(
        all.total,
        ACTION_LIMIT as u64 + 1,
        "the actions, and the audit entry"
    );
    // Beyond the newest 10 000 audit entries and older than a year: gone. One beyond the
    // 10 000 that is not a year old yet stays.
    let doomed = mk("Older");
    lib.delete_playlist(doomed, UI).unwrap();
    let young = action(lib.purge_playlists(None, UI).unwrap());
    age(young, 300);
    raw(lib)
        .execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?2)
             INSERT INTO actions (at_ms, origin, kind, summary, target_count)
             SELECT ?1, 'agent:bulk', 'file_write', 'filler', 1 FROM n",
            [now_ms(), AUDIT_LIMIT as i64],
        )
        .unwrap();
    lib.set_rating(&t, Some(1), UI).unwrap();
    assert!(lib.action(audit).unwrap().is_none());
    assert!(lib.action(young).unwrap().is_some());
}

#[test]
fn trusted_undo_descriptions_follow_chains_and_guard_the_writer_transaction() {
    let f = fixture();
    let lib = &f.lib;
    let t = tracks(lib, f.root, 1);
    let first = action(lib.set_rating(&t, Some(3), UI).unwrap());
    let d = lib.describe_undo(first).unwrap();
    assert!(!d.writes_files);
    assert_eq!(d.effective_kind, "set_rating");
    assert_eq!(d.original, first);
    assert_eq!(d.targets, vec![ActionTarget::Track { id: t[0] }]);
    let second = action(lib.undo_action_guarded(d.clone(), UI).unwrap());
    assert!(lib.undo_action_guarded(d, UI).is_err());
    let redo = lib.describe_undo(second).unwrap();
    assert_eq!(redo.chain, vec![second, first]);
    assert_eq!(redo.original, first);
    assert!(!redo.writes_files);
    let third = action(lib.undo_action_guarded(redo, UI).unwrap());
    assert_eq!(
        lib.describe_undo(third).unwrap().chain,
        vec![third, second, first]
    );
    let frozen = lib.describe_undo(third).unwrap();
    lib.set_loved(&t, true, UI).unwrap();
    assert!(lib.undo_action_guarded(frozen, UI).is_err());
    assert!(lib.describe_undo(ActionId(i64::MAX)).is_err());
}

#[test]
fn trusted_undo_description_refuses_corrupt_unknown_and_pruned_chains() {
    let f = fixture();
    let lib = &f.lib;
    let t = tracks(lib, f.root, 1);
    let first = action(lib.set_loved(&t, true, UI).unwrap());
    let second = action(lib.undo_action(first, UI).unwrap());
    raw(lib)
        .execute(
            "UPDATE actions SET undo_of=?1 WHERE id=?2",
            [second.0, second.0],
        )
        .unwrap();
    assert!(lib.describe_undo(second).is_err());
    raw(lib)
        .execute(
            "UPDATE actions SET undo_of=?1 WHERE id=?2",
            [first.0, second.0],
        )
        .unwrap();
    raw(lib)
        .execute(
            "UPDATE actions SET kind='future_unknown' WHERE id=?1",
            [first.0],
        )
        .unwrap();
    assert!(lib.describe_undo(second).is_err());
    raw(lib)
        .execute("DELETE FROM actions WHERE id=?1", [first.0])
        .unwrap();
    assert!(lib.describe_undo(second).is_err());
    raw(lib)
        .execute("UPDATE actions SET inverse=x'ff' WHERE id=?1", [second.0])
        .unwrap();
    assert!(lib.describe_undo(second).is_err());
}

#[test]
fn details_are_operation_time_facts_and_survive_reopen_and_inverse_expiry() {
    const INSERT_ACTIONS: &str = r#"
INSERT INTO actions (at_ms,origin,kind,summary,target_count) VALUES
(?1,'ui','set_loved','old entry',1)
"#;

    use serde_json::json;
    let f = fixture();
    let ids = tracks(&f.lib, f.root, 1);
    let rated = action(f.lib.set_rating(&ids, Some(4), UI).unwrap());
    let cleared = action(f.lib.set_rating(&ids, None, UI).unwrap());
    let loved = action(f.lib.set_loved(&ids, true, UI).unwrap());
    let created = f
        .lib
        .create_playlist("Original", PlaylistKind::Manual, UI)
        .unwrap();
    let playlist = created.value;
    let renamed = action(
        f.lib
            .rename_playlist(playlist, "Renamed", None, UI)
            .unwrap(),
    );
    let undone = action(f.lib.undo_action(renamed, UI).unwrap());
    let redone = action(f.lib.undo_action(undone, UI).unwrap());
    let db = f._dir.path().join("db/library.sqlite");
    // Facts are independent of whether undo payloads are retained. Legacy rows stay absent.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("UPDATE actions SET inverse=NULL WHERE id=?1", [rated.0])
        .unwrap();
    conn.execute(INSERT_ACTIONS, [now_ms()]).unwrap();
    let legacy = ActionId(conn.last_insert_rowid());
    drop(conn);
    drop(f.lib);
    let reopened = Library::open(&db).unwrap();
    let details = |id| reopened.action(id).unwrap().unwrap().details;
    assert_eq!(
        details(rated),
        Some(json!({"type":"set_rating","rating":4}))
    );
    assert_eq!(
        details(cleared),
        Some(json!({"type":"set_rating","rating":null}))
    );
    assert_eq!(
        details(loved),
        Some(json!({"type":"set_loved","loved":true}))
    );
    assert_eq!(
        details(created.action.unwrap()),
        Some(json!({"type":"playlist","playlist":playlist.0,"name":"Original"}))
    );
    assert_eq!(
        details(renamed),
        Some(json!({"type":"playlist","playlist":playlist.0,"name":"Renamed"}))
    );
    for id in [undone, redone] {
        assert_eq!(
            details(id),
            Some(json!({"type":"undo","original_kind":"playlist_rename"}))
        );
    }
    assert_eq!(details(legacy), None);
    let root = reopened
        .actions(None, ALL)
        .unwrap()
        .items
        .into_iter()
        .find(|a| a.kind == "root_add")
        .unwrap();
    assert_eq!(
        root.details,
        Some(json!({
        "type":"root",
        "root":f.root.0,
        "path":crate::storage_access::canonical_dir(&f._dir.path().join("music"))
            .unwrap().to_string_lossy()}
        ))
    );
}

#[test]
fn pruned_legacy_undo_chain_omits_unrecoverable_details() {
    let f = fixture();
    let p = f
        .lib
        .create_playlist("A", PlaylistKind::Manual, UI)
        .unwrap()
        .value;
    let renamed = action(f.lib.rename_playlist(p, "B", None, UI).unwrap());
    let undone = action(f.lib.undo_action(renamed, UI).unwrap());
    let conn = rusqlite::Connection::open(f._dir.path().join("db/library.sqlite")).unwrap();
    conn.execute(
        "UPDATE actions SET details_json=NULL WHERE id=?1",
        [undone.0],
    )
    .unwrap();
    conn.execute("DELETE FROM actions WHERE id=?1", [renamed.0])
        .unwrap();
    let redone = action(f.lib.undo_action(undone, UI).unwrap());
    assert_eq!(f.lib.action(redone).unwrap().unwrap().details, None);
}

#[test]
fn undo_facts_use_nearest_recorded_original_kind_in_depth_order() {
    let f = fixture();
    let p = f
        .lib
        .create_playlist("A", PlaylistKind::Manual, UI)
        .unwrap()
        .value;
    let renamed = action(f.lib.rename_playlist(p, "B", None, UI).unwrap());
    let undone = action(f.lib.undo_action(renamed, UI).unwrap());
    let conn = rusqlite::Connection::open(f._dir.path().join("db/library.sqlite")).unwrap();
    conn.execute(
        "UPDATE actions SET kind='playlist_create' WHERE id=?1",
        [renamed.0],
    )
    .unwrap();
    let redone = action(f.lib.undo_action(undone, UI).unwrap());
    assert_eq!(
        f.lib.action(redone).unwrap().unwrap().details,
        Some(serde_json::json!({"type":"undo","original_kind":"playlist_rename"}))
    );
}

#[test]
fn version_1_tag_facts_accept_legacy_operation_without_rewriting_storage() {
    use serde_json::json;
    let f = fixture();
    let ids = tracks(&f.lib, f.root, 1);
    let id = action(f.lib.set_rating(&ids, Some(4), UI).unwrap());
    let legacy = json!({"v":1,"details":{"type":"tag_edit","changes":[
        {"key":"ARTIST","operation":"set"}, {"key":"COMMENT","operation":"remove"},
        {"key":"TITLE","type":"set"}
    ]}})
    .to_string();
    let conn = raw(&f.lib);
    conn.execute(
        "UPDATE actions SET details_json=?1 WHERE id=?2",
        rusqlite::params![legacy, id.0],
    )
    .unwrap();
    let expected = Some(json!({"type":"tag_edit","changes":[
        {
        "key":"ARTIST",
        "type":"set"}
        ,
         {
        "key":"COMMENT",
        "type":"remove"}
        ,
         {
        "key":"TITLE",
        "type":"set"}

    ]}));
    assert_eq!(f.lib.action(id).unwrap().unwrap().details, expected);
    assert_eq!(f.lib.actions(None, ALL).unwrap().items[0].details, expected);
    let stored: String = conn
        .query_row(
            "SELECT details_json FROM actions WHERE id=?1",
            [id.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, legacy);
}

#[test]
fn playlist_undo_depth_does_not_exempt_edits_from_global_journal_retention() {
    for by_age in [true, false] {
        let f = fixture();
        let lib = &f.lib;
        let ids = tracks(lib, f.root, 1);
        let playlist = lib
            .create_playlist("Before", PlaylistKind::Manual, UI)
            .unwrap()
            .value;
        let edit = action(lib.rename_playlist(playlist, "After", None, UI).unwrap());
        let revision = lib.playlist(playlist).unwrap().unwrap().revision;
        assert!(lib.action(edit).unwrap().unwrap().undoable);
        let conn = raw(lib);
        if by_age {
            conn.execute(
                "UPDATE actions SET at_ms=?1 WHERE id=?2",
                [now_ms() - 31 * DAY, edit.0],
            )
            .unwrap();
            assert!(!lib.action(edit).unwrap().unwrap().undoable);
        } else {
            conn.execute(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<?2)
                INSERT INTO actions(at_ms,origin,kind,summary,target_count)
                SELECT ?1,'ui','set_loved','filler',0 FROM n",
                [now_ms(), ACTION_LIMIT as i64],
            )
            .unwrap();
        }
        // An unrelated write triggers the same global pruning used in production.
        lib.set_loved(&ids, true, UI).unwrap();
        assert!(lib.action(edit).unwrap().is_none(), "by_age={by_age}");
        assert_eq!(
            code(lib.undo_playlist(playlist, revision, UI)),
            "nothing_to_undo"
        );
        assert_eq!(code(lib.undo_action(edit, UI)), "action_not_found");
    }
}
