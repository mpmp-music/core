//! Playlist tests on synthetic rows (no audio files).

use std::path::{Path, PathBuf};
use std::time::Instant;

use mpmp_test_support::TempDir;

use crate::db::writer::{ScanBatch, now_ms};
use crate::playlist::{self, PLAYLIST_TRASH_RETENTION_MS, PurgeReviewClock};
use crate::tests::{Fixture, UI, fixture, meta, record, upsert, write};
use crate::track_query::{CompareOp, Field, Filter, SortKey, TextOp, TrackQuery, Value};
use crate::{
    Done, Library, LibraryError, Page, PlaylistId, PlaylistItemId, PlaylistKind, RootId, TrackId,
};

const ALL: Page = Page::new(0, 1000);

#[test]
fn guarded_purge_checks_one_transaction_and_never_adds_expired_unreviewed_lists() {
    let f = fixture();
    let lib = &f.lib;
    let a = manual(lib, "reviewed");
    let b = manual(lib, "outside selection");
    lib.delete_playlist(a, UI).unwrap();
    lib.delete_playlist(b, UI).unwrap();
    let expected = lib.describe_purge(Some(&[a])).unwrap();
    let all = lib.describe_purge(None).unwrap();
    let c = manual(lib, "new trash");
    lib.delete_playlist(c, UI).unwrap();
    assert!(matches!(
        lib.purge_playlists_guarded(all, UI),
        Err(LibraryError::PurgeChanged)
    ));
    // A guard checks display data too, even a mutation made outside the core actor.
    let conn = rusqlite::Connection::open(lib.inner().pool.path()).unwrap();
    conn.execute("UPDATE playlists SET name='changed' WHERE id=?1", [a.0])
        .unwrap();
    assert!(matches!(
        lib.purge_playlists_guarded(expected, UI),
        Err(LibraryError::PurgeChanged)
    ));
    conn.execute("UPDATE playlists SET deleted_at=1 WHERE id=?1", [b.0])
        .unwrap();
    let expected = lib.describe_purge(Some(&[a])).unwrap();
    assert_eq!(lib.purge_playlists_guarded(expected, UI).unwrap().value, 1);
    assert!(lib.playlist(a).unwrap().is_none());
    assert!(
        lib.playlist(b).unwrap().is_some(),
        "expired but never approved"
    );
    assert!(lib.playlist(c).unwrap().is_some());
}

fn music(f: &Fixture) -> PathBuf {
    f._dir.path().join("music")
}

/// Five tracks "t0".."t4" (durations 1..5 s) under `a/`, as real (empty) files.
fn five(f: &Fixture) -> Vec<TrackId> {
    let dir = music(f).join("a");
    std::fs::create_dir_all(&dir).unwrap();
    let records = (0..5)
        .map(|i| {
            std::fs::write(dir.join(format!("{i}.flac")), b"").unwrap();
            let mut m = meta(&format!("t{i}"), "Artist", "Album");
            m.duration_ms = Some((i + 1) * 1000);
            m.track_no = Some(i as u32 + 1);
            record(f.root, &format!("a/{i}.flac"), m)
        })
        .collect();
    upsert(&f.lib, records);
    let (ids, _) = f
        .lib
        .tracks_query(&by_track_no(Filter::All), ALL, &[])
        .unwrap();
    ids.into_iter().map(|r| r.track.id).collect()
}

fn by_track_no(filter: Filter) -> TrackQuery {
    TrackQuery {
        filter,
        sort: vec![SortKey {
            field: Field::TrackNo,
            descending: false,
        }],
    }
}

fn manual(lib: &Library, name: &str) -> PlaylistId {
    lib.create_playlist(name, PlaylistKind::Manual, UI)
        .unwrap()
        .value
}

/// Track ids of a playlist's entries, in order.
fn tracks_of(lib: &Library, id: PlaylistId) -> Vec<TrackId> {
    let (_, entries, total) = lib.playlist_items(id, ALL, &[]).unwrap();
    assert_eq!(total as usize, entries.len());
    entries.iter().map(|e| e.row.track.id).collect()
}

fn items_of(lib: &Library, id: PlaylistId) -> Vec<PlaylistItemId> {
    let (_, entries, _) = lib.playlist_items(id, ALL, &[]).unwrap();
    entries.iter().map(|e| e.item.unwrap()).collect()
}

fn revision(lib: &Library, id: PlaylistId) -> u64 {
    lib.playlist(id).unwrap().unwrap().revision
}

/// Positions must stay dense `0..n` in item order.
fn assert_dense(lib: &Library, id: PlaylistId) {
    let conn = lib.inner().pool.connection().unwrap();
    let positions: Vec<i64> = conn
        .prepare("SELECT position FROM playlist_items WHERE playlist_id = ?1 ORDER BY position")
        .unwrap()
        .query_map([id.0], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(positions, (0..positions.len() as i64).collect::<Vec<_>>());
}

fn conflict(result: crate::Result<impl std::fmt::Debug>) -> (u64, u64) {
    match result {
        Err(LibraryError::PlaylistConflict { expected, actual }) => (expected, actual),
        other => panic!("expected a conflict, got {other:?}"),
    }
}

/// The error code of a failed call.
fn code<T: std::fmt::Debug>(result: crate::Result<T>) -> &'static str {
    match result {
        Err(err) => err.code(),
        Ok(value) => panic!("expected an error, got {value:?}"),
    }
}

fn is_not_found<T: std::fmt::Debug>(result: crate::Result<T>) -> bool {
    matches!(result, Err(LibraryError::NotFound { .. }))
}

fn is_invalid<T: std::fmt::Debug>(result: crate::Result<T>) -> bool {
    matches!(result, Err(LibraryError::Invalid { .. }))
}

#[test]
fn create_rename_list_and_revisions() {
    let f = fixture();
    let lib = &f.lib;
    let b = manual(lib, "beta");
    let a = manual(lib, "Älpha");
    let c = manual(lib, "  ALPHA  ");
    assert!(is_invalid(lib.create_playlist(
        "  ",
        PlaylistKind::Manual,
        UI
    )));

    let (list, total) = lib.playlists(false, ALL).unwrap();
    assert_eq!(total, 3);
    // Folded name order, then id; names are trimmed.
    let names: Vec<_> = list.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Älpha", "ALPHA", "beta"]);
    assert_eq!(list[0].id, a);
    assert_eq!(list[1].id, c);
    let (page, total) = lib.playlists(false, Page::new(1, 1)).unwrap();
    assert_eq!((page[0].id, total), (c, 3));

    let s = lib.playlist(b).unwrap().unwrap();
    assert_eq!(s.revision, 0);
    assert_eq!(s.kind, PlaylistKind::Manual);
    assert_eq!((s.track_count, s.duration_ms, s.deleted_at), (0, 0, None));
    assert!(s.created_at > 0 && s.updated_at == s.created_at);
    assert!(lib.playlist(PlaylistId(999)).unwrap().is_none());

    assert_eq!(
        lib.rename_playlist(b, "Gamma", Some(0), UI).unwrap().value,
        1
    );
    assert_eq!(
        lib.rename_playlist(b, "Gamma", None, UI).unwrap().value,
        1,
        "no change"
    );
    assert_eq!(conflict(lib.rename_playlist(b, "x", Some(0), UI)), (0, 1));
    assert!(is_invalid(lib.rename_playlist(b, "", None, UI)));
    assert!(is_not_found(lib.rename_playlist(
        PlaylistId(999),
        "x",
        None,
        UI
    )));
    assert_eq!(lib.playlist(b).unwrap().unwrap().name, "Gamma");
}

#[test]
fn add_remove_move_keep_order_and_positions() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let p = manual(lib, "p");

    let (i01, r) = lib
        .add_to_playlist(p, &[t[0], t[1]], None, Some(0), UI)
        .unwrap()
        .value;
    assert_eq!((i01.len(), r), (2, 1));
    // Insert at the front, at the end (appends), in the middle; duplicates allowed.
    let (i2, r) = lib
        .add_to_playlist(p, &[t[2]], Some(0), Some(1), UI)
        .unwrap()
        .value;
    assert_eq!(r, 2);
    // Past the end is out of range, also with nothing to add.
    assert_eq!(
        code(lib.add_to_playlist(p, &[t[3]], Some(4), None, UI)),
        "out_of_range"
    );
    assert_eq!(
        code(lib.add_to_playlist(p, &[], Some(4), None, UI)),
        "out_of_range"
    );
    let (i3, _) = lib
        .add_to_playlist(p, &[t[3], t[3]], Some(3), None, UI)
        .unwrap()
        .value;
    let (i4, r) = lib
        .add_to_playlist(p, &[t[4]], Some(2), None, UI)
        .unwrap()
        .value;
    assert_eq!(r, 4);
    assert_eq!(tracks_of(lib, p), [t[2], t[0], t[4], t[1], t[3], t[3]]);
    assert_ne!(i3[0], i3[1], "the same track twice is two items");
    assert_eq!(
        items_of(lib, p),
        [i2[0], i01[0], i4[0], i01[1], i3[0], i3[1]]
    );
    assert_dense(lib, p);
    // Nothing to add: no new revision.
    assert_eq!(
        lib.add_to_playlist(p, &[], None, None, UI).unwrap().value.1,
        4
    );
    // An unknown track fails the whole call.
    assert!(is_not_found(lib.add_to_playlist(
        p,
        &[t[0], TrackId(999)],
        None,
        None,
        UI
    )));
    assert_eq!(
        conflict(lib.add_to_playlist(p, &[t[0]], None, Some(3), UI)),
        (3, 4)
    );
    assert_eq!(tracks_of(lib, p).len(), 6);

    // Summary over all items.
    let s = lib.playlist(p).unwrap().unwrap();
    assert_eq!(
        (s.track_count, s.duration_ms),
        (6, 3000 + 1000 + 5000 + 2000 + 8000)
    );

    // Remove a scattered batch (duplicates in the request are fine): the removed ids in
    // request order.
    let (removed, r) = lib
        .remove_from_playlist(p, &[i3[0], i01[0], i01[0]], Some(4), UI)
        .unwrap()
        .value;
    assert_eq!((removed, r), (vec![i3[0], i01[0]], 5));
    assert_eq!(tracks_of(lib, p), [t[2], t[4], t[1], t[3]]);
    assert_dense(lib, p);
    // Items that are not in the playlist are ignored: only the others are removed.
    let other = manual(lib, "other");
    let (foreign, _) = lib
        .add_to_playlist(other, &[t[0]], None, None, UI)
        .unwrap()
        .value;
    let (removed, r) = lib
        .remove_from_playlist(
            p,
            &[i01[0], i2[0], foreign[0], PlaylistItemId(999)],
            Some(5),
            UI,
        )
        .unwrap()
        .value;
    assert_eq!((removed, r), (vec![i2[0]], 6));
    assert_eq!(tracks_of(lib, p), [t[4], t[1], t[3]]);
    assert_eq!(tracks_of(lib, other), [t[0]]);
    // Nothing to remove: no new revision (but still a revision check).
    assert_eq!(
        lib.remove_from_playlist(p, &[i01[0], foreign[0]], Some(6), UI)
            .unwrap()
            .value,
        (vec![], 6)
    );
    assert_eq!(
        lib.remove_from_playlist(p, &[], None, UI).unwrap().value,
        (vec![], 6)
    );
    assert_eq!(
        conflict(lib.remove_from_playlist(p, &[i01[0]], Some(5), UI)),
        (5, 6)
    );
    lib.undo_playlist(p, 6, UI).unwrap();
    assert_eq!(tracks_of(lib, p), [t[2], t[4], t[1], t[3]]);
    let r = revision(lib, p);
    assert_eq!(r, 7);

    // Move forward, backward, to the last position, onto itself.
    assert_eq!(
        lib.move_playlist_item(p, i2[0], 2, Some(r), UI)
            .unwrap()
            .value,
        8
    );
    assert_eq!(tracks_of(lib, p), [t[4], t[1], t[2], t[3]]);
    lib.move_playlist_item(p, i3[1], 0, None, UI).unwrap();
    assert_eq!(tracks_of(lib, p), [t[3], t[4], t[1], t[2]]);
    lib.move_playlist_item(p, i4[0], 3, None, UI).unwrap();
    assert_eq!(tracks_of(lib, p), [t[3], t[1], t[2], t[4]]);
    assert_eq!(
        lib.move_playlist_item(p, i4[0], 3, None, UI).unwrap().value,
        10,
        "no-op"
    );
    // The position is final: at or past the length is out of range.
    assert_eq!(
        code(lib.move_playlist_item(p, i4[0], 4, None, UI)),
        "out_of_range"
    );
    assert_eq!(
        code(lib.move_playlist_item(p, i4[0], u32::MAX, None, UI)),
        "out_of_range"
    );
    assert_eq!(revision(lib, p), 10);
    assert_dense(lib, p);
    assert_eq!(
        code(lib.move_playlist_item(p, i01[0], 0, None, UI)),
        "item_not_found"
    );

    // Pages.
    let (rev, page, total) = lib.playlist_items(p, Page::new(1, 2), &[]).unwrap();
    assert_eq!((rev, total), (10, 4));
    let page: Vec<_> = page.iter().map(|e| e.row.track.id).collect();
    assert_eq!(page, [t[1], t[2]]);
    assert!(is_not_found(lib.playlist_items(PlaylistId(999), ALL, &[])));
}

#[test]
fn undo_each_kind_of_edit() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let p = manual(lib, "p");
    // Nothing to undo at creation.
    assert_eq!(code(lib.undo_playlist(p, 0, UI)), "nothing_to_undo");

    let (_, r) = lib.add_to_playlist(p, &t, None, None, UI).unwrap().value;
    let r = lib
        .add_to_playlist(p, &[t[0]], Some(2), Some(r), UI)
        .unwrap()
        .value
        .1;
    assert_eq!(tracks_of(lib, p), [t[0], t[1], t[0], t[2], t[3], t[4]]);
    // Undo the add.
    let r = lib.undo_playlist(p, r, UI).unwrap().value;
    assert_eq!(r, 3);
    assert_eq!(tracks_of(lib, p), t);
    assert_dense(lib, p);
    // Only at the current revision.
    assert_eq!(conflict(lib.undo_playlist(p, 2, UI)), (2, 3));
    // The undo itself is not undone (no redo): the next undo goes one edit further back.
    assert_eq!(lib.undo_playlist(p, 3, UI).unwrap().value, 4);
    assert!(tracks_of(lib, p).is_empty());
    assert_eq!(code(lib.undo_playlist(p, 4, UI)), "nothing_to_undo");
    let (items, r) = lib.add_to_playlist(p, &t, None, Some(4), UI).unwrap().value;

    // Undo a scattered remove: same items back at the same places.
    let r = lib
        .remove_from_playlist(p, &[items[4], items[0], items[2]], Some(r), UI)
        .unwrap()
        .value
        .1;
    assert_eq!(tracks_of(lib, p), [t[1], t[3]]);
    let r = lib.undo_playlist(p, r, UI).unwrap().value;
    assert_eq!(tracks_of(lib, p), t);
    assert_eq!(items_of(lib, p), items);
    assert_dense(lib, p);

    // Undo a move, both directions.
    let r = lib
        .move_playlist_item(p, items[0], 3, Some(r), UI)
        .unwrap()
        .value;
    let r = lib.undo_playlist(p, r, UI).unwrap().value;
    assert_eq!(items_of(lib, p), items);
    let r = lib
        .move_playlist_item(p, items[4], 1, Some(r), UI)
        .unwrap()
        .value;
    let r = lib.undo_playlist(p, r, UI).unwrap().value;
    assert_eq!(items_of(lib, p), items);
    assert_dense(lib, p);

    // Undo a rename.
    let r = lib
        .rename_playlist(p, "renamed", Some(r), UI)
        .unwrap()
        .value;
    let r = lib.undo_playlist(p, r, UI).unwrap().value;
    assert_eq!(lib.playlist(p).unwrap().unwrap().name, "p");

    // Only while nothing changed since.
    let r1 = lib.rename_playlist(p, "one", Some(r), UI).unwrap().value;
    let r2 = lib
        .add_to_playlist(p, &[t[1]], None, Some(r1), UI)
        .unwrap()
        .value
        .1;
    assert_eq!(conflict(lib.undo_playlist(p, r1, UI)), (r1, r2));
    lib.undo_playlist(p, r2, UI).unwrap();
    assert_eq!(tracks_of(lib, p), t);
    assert_eq!(lib.playlist(p).unwrap().unwrap().name, "one");

    // Undo a smart query change.
    let s = lib
        .create_playlist(
            "s",
            PlaylistKind::Smart {
                query: TrackQuery::default(),
                max_tracks: None,
            },
            UI,
        )
        .unwrap()
        .value;
    let q = by_track_no(Filter::Compare {
        field: Field::TrackNo,
        op: CompareOp::Le,
        value: Value::Int(2),
    });
    let r = lib
        .set_smart_query(s, q.clone(), Some(1), Some(0), UI)
        .unwrap()
        .value;
    assert_eq!(tracks_of(lib, s), [t[0]]);
    lib.undo_playlist(s, r, UI).unwrap();
    assert_eq!(tracks_of(lib, s).len(), 5);
    assert_eq!(
        lib.playlist(s).unwrap().unwrap().kind,
        PlaylistKind::Smart {
            query: TrackQuery::default(),
            max_tracks: None
        }
    );
}

#[test]
fn undo_survives_restart() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("library.sqlite");
    let music = dir.path().join("m");
    std::fs::create_dir(&music).unwrap();
    let (p, items, r) = {
        let lib = Library::open(&db).unwrap();
        let root = lib.add_root(&music, UI).unwrap().value;
        upsert(
            &lib,
            (0..4)
                .map(|i| record(root, &format!("{i}.flac"), meta(&format!("x{i}"), "", "")))
                .collect(),
        );
        let ids: Vec<TrackId> = (1..=4).map(TrackId).collect();
        let p = manual(&lib, "p");
        let (items, r) = lib.add_to_playlist(p, &ids, None, None, UI).unwrap().value;
        let r = lib.rename_playlist(p, "q", Some(r), UI).unwrap().value;
        let (_, r) = lib
            .remove_from_playlist(p, &[items[1], items[2]], Some(r), UI)
            .unwrap()
            .value;
        (p, items, r)
    };
    let lib = Library::open(&db).unwrap();
    assert_eq!(items_of(&lib, p), [items[0], items[3]]);
    // Every step survives, newest first.
    let r = lib.undo_playlist(p, r, UI).unwrap().value;
    assert_eq!(items_of(&lib, p), items);
    drop(lib);
    let lib = Library::open(&db).unwrap();
    let r = lib.undo_playlist(p, r, UI).unwrap().value;
    assert_eq!(lib.playlist(p).unwrap().unwrap().name, "p");
    let r = lib.undo_playlist(p, r, UI).unwrap().value;
    assert!(items_of(&lib, p).is_empty());
    assert_eq!(code(lib.undo_playlist(p, r, UI)), "nothing_to_undo");
}

/// The journal's actions that `undo_playlist` can still walk back through for a playlist
/// (its edits and redos in effect), oldest first.
const STEPS: &str = r#"
SELECT a.id FROM action_targets t JOIN actions a ON a.id = t.action_id
     WHERE t.kind = 'playlist' AND t.object_id = ?1 AND a.undone_by IS NULL
       AND a.inverse IS NOT NULL AND (a.undo_of IS NULL OR a.redo)
     ORDER BY a.id
"#;

/// How many edits of a playlist `undo_playlist` can still undo.
fn undo_steps(lib: &Library, id: PlaylistId) -> i64 {
    let conn = lib.inner().pool.connection().unwrap();
    conn.query_row(&format!("SELECT COUNT(*) FROM ({STEPS})"), [id.0], |r| {
        r.get(0)
    })
    .unwrap()
}

#[test]
fn undo_keeps_the_last_50_steps() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let p = manual(lib, "p");
    let mut r = 0;
    // 60 edits: each adds one track.
    for i in 0..60 {
        r = lib
            .add_to_playlist(p, &[t[i % 5]], None, Some(r), UI)
            .unwrap()
            .value
            .1;
    }
    assert_eq!(tracks_of(lib, p).len(), 60);
    assert_eq!(undo_steps(lib, p), crate::PLAYLIST_UNDO_STEPS as i64);
    // Other playlists' steps are not affected.
    let other = manual(lib, "other");
    lib.rename_playlist(other, "o", None, UI).unwrap();
    for _ in 0..crate::PLAYLIST_UNDO_STEPS {
        r = lib.undo_playlist(p, r, UI).unwrap().value;
    }
    // The 10 oldest edits are beyond the limit.
    assert_eq!(tracks_of(lib, p).len(), 10);
    assert_eq!(code(lib.undo_playlist(p, r, UI)), "nothing_to_undo");
    assert_eq!(undo_steps(lib, other), 1);
    assert_eq!(r, 110);
}

/// Sets the creation time of a playlist's undo steps, oldest first, `days` apart (one
/// entry per step).
fn age_steps(lib: &Library, id: PlaylistId, days_ago: &[i64]) {
    let conn = rusqlite::Connection::open(lib.inner().pool.path()).unwrap();
    let ids: Vec<i64> = conn
        .prepare(STEPS)
        .unwrap()
        .query_map([id.0], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(ids.len(), days_ago.len());
    for (step, days) in ids.iter().zip(days_ago) {
        conn.execute(
            "UPDATE actions SET at_ms = ?2 WHERE id = ?1",
            [*step, now_ms() - days * 24 * 3600 * 1000],
        )
        .unwrap();
    }
}

#[test]
fn undo_steps_expire_after_30_days() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let p = manual(lib, "p");
    let mut r = 0;
    for track in &t[..4] {
        r = lib
            .add_to_playlist(p, &[*track], None, Some(r), UI)
            .unwrap()
            .value
            .1;
    }
    // Two edits are older than 30 days.
    age_steps(lib, p, &[40, 31, 29, 1]);
    r = lib.undo_playlist(p, r, UI).unwrap().value;
    r = lib.undo_playlist(p, r, UI).unwrap().value;
    assert_eq!(tracks_of(lib, p), &t[..2]);
    assert_eq!(code(lib.undo_playlist(p, r, UI)), "nothing_to_undo");
    assert_eq!(undo_steps(lib, p), 0);

    // A step that looks newer than an expired one (the clock went back) goes with it, so
    // what is left stays a contiguous history.
    for track in &t[..3] {
        r = lib
            .add_to_playlist(p, &[*track], None, Some(r), UI)
            .unwrap()
            .value
            .1;
    }
    age_steps(lib, p, &[1, 45, 2]);
    r = lib.undo_playlist(p, r, UI).unwrap().value;
    assert_eq!(code(lib.undo_playlist(p, r, UI)), "nothing_to_undo");
    assert_eq!(tracks_of(lib, p), [t[0], t[1], t[0], t[1]]);

    // Expired steps of every playlist are dropped when the library opens.
    let q = manual(lib, "q");
    lib.rename_playlist(q, "q2", None, UI).unwrap();
    age_steps(lib, q, &[31]);
    let db = lib.inner().pool.path().to_owned();
    let f2 = Library::open(&db).unwrap();
    assert_eq!(undo_steps(&f2, q), 0);
}

#[test]
fn agent_clearing_a_playlist_is_fully_recoverable() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let p = manual(lib, "favourites");
    let (items, r) = lib.add_to_playlist(p, &t, None, None, UI).unwrap().value;
    let r = lib
        .move_playlist_item(p, items[4], 0, Some(r), UI)
        .unwrap()
        .value;
    let before = items_of(lib, p);
    // An agent removes every item one by one, then renames the empty list and adds a
    // track to it.
    let mut r = r;
    for item in &before {
        r = lib
            .remove_from_playlist(p, &[*item], Some(r), UI)
            .unwrap()
            .value
            .1;
    }
    r = lib.rename_playlist(p, "empty", Some(r), UI).unwrap().value;
    r = lib
        .add_to_playlist(p, &[t[2]], None, Some(r), UI)
        .unwrap()
        .value
        .1;
    assert_eq!(tracks_of(lib, p), [t[2]]);
    // The user walks back: the add, the rename, then each remove.
    for _ in 0..2 + before.len() {
        r = lib.undo_playlist(p, r, UI).unwrap().value;
    }
    assert_eq!(items_of(lib, p), before);
    assert_eq!(lib.playlist(p).unwrap().unwrap().name, "favourites");
    assert_dense(lib, p);
    // A new edit after undos keeps the older steps: the move and the add are still there.
    r = lib.rename_playlist(p, "favs", Some(r), UI).unwrap().value;
    r = lib.undo_playlist(p, r, UI).unwrap().value;
    r = lib.undo_playlist(p, r, UI).unwrap().value;
    assert_eq!(items_of(lib, p), items);
    r = lib.undo_playlist(p, r, UI).unwrap().value;
    assert!(items_of(lib, p).is_empty());
    assert_eq!(code(lib.undo_playlist(p, r, UI)), "nothing_to_undo");
}

#[test]
fn empty_edits_never_bump_the_revision() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let p = manual(lib, "p");
    let (items, r) = lib
        .add_to_playlist(p, &t[..2], None, None, UI)
        .unwrap()
        .value;
    assert_eq!(
        lib.add_to_playlist(p, &[], None, Some(r), UI)
            .unwrap()
            .value,
        (vec![], r)
    );
    assert_eq!(
        lib.add_to_playlist(p, &[], Some(2), Some(r), UI)
            .unwrap()
            .value,
        (vec![], r)
    );
    assert_eq!(
        lib.remove_from_playlist(p, &[], Some(r), UI).unwrap().value,
        (vec![], r)
    );
    assert_eq!(
        lib.remove_from_playlist(p, &[PlaylistItemId(12345)], Some(r), UI)
            .unwrap()
            .value,
        (vec![], r)
    );
    assert_eq!(
        lib.move_playlist_item(p, items[1], 1, Some(r), UI)
            .unwrap()
            .value,
        r
    );
    assert_eq!(lib.rename_playlist(p, " p ", Some(r), UI).unwrap().value, r);
    let s = lib
        .create_playlist(
            "s",
            PlaylistKind::Smart {
                query: TrackQuery::default(),
                max_tracks: Some(3),
            },
            UI,
        )
        .unwrap()
        .value;
    assert_eq!(
        lib.set_smart_query(s, TrackQuery::default(), Some(3), Some(0), UI)
            .unwrap()
            .value,
        0
    );
    assert_eq!(revision(lib, p), r);
    // None of them left an undo step: the next undo undoes the add.
    assert_eq!(undo_steps(lib, p), 1);
    assert_eq!(undo_steps(lib, s), 0);
}

#[test]
fn kind_mismatches_are_invalid() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let m = manual(lib, "m");
    let s = lib
        .create_playlist(
            "s",
            PlaylistKind::Smart {
                query: TrackQuery::default(),
                max_tracks: None,
            },
            UI,
        )
        .unwrap()
        .value;
    assert!(is_invalid(lib.add_to_playlist(s, &[t[0]], None, None, UI)));
    assert!(is_invalid(lib.remove_from_playlist(
        s,
        &[PlaylistItemId(1)],
        None,
        UI
    )));
    assert!(is_invalid(lib.move_playlist_item(
        s,
        PlaylistItemId(1),
        0,
        None,
        UI
    )));
    assert!(is_invalid(lib.set_smart_query(
        m,
        TrackQuery::default(),
        None,
        None,
        UI
    )));
    // Invalid queries are rejected on create and on change.
    let bad = TrackQuery {
        filter: Filter::Compare {
            field: Field::Title,
            op: CompareOp::Gt,
            value: Value::Int(3),
        },
        sort: vec![],
    };
    assert!(matches!(
        lib.create_playlist(
            "bad",
            PlaylistKind::Smart {
                query: bad.clone(),
                max_tracks: None
            },
            UI
        ),
        Err(LibraryError::InvalidQuery(_))
    ));
    assert!(matches!(
        lib.set_smart_query(s, bad, None, None, UI),
        Err(LibraryError::InvalidQuery(_))
    ));
    assert_eq!(revision(lib, s), 0);
}

#[test]
fn smart_playlists_with_limit() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let q = TrackQuery {
        filter: Filter::Text {
            field: Field::Artist,
            op: TextOp::Equals,
            text: "artist".into(),
        },
        sort: vec![SortKey {
            field: Field::DurationMs,
            descending: true,
        }],
    };
    let s = lib
        .create_playlist(
            "longest",
            PlaylistKind::Smart {
                query: q.clone(),
                max_tracks: Some(3),
            },
            UI,
        )
        .unwrap()
        .value;
    let summary = lib.playlist(s).unwrap().unwrap();
    assert_eq!((summary.track_count, summary.duration_ms), (3, 12_000));
    assert_eq!(summary.query_error, None);
    let (rev, entries, total) = lib.playlist_items(s, ALL, &[]).unwrap();
    assert_eq!((rev, total), (0, 3));
    assert!(entries.iter().all(|e| e.item.is_none()));
    let ids: Vec<_> = entries.iter().map(|e| e.row.track.id).collect();
    assert_eq!(ids, [t[4], t[3], t[2]]);
    // Pages stop at the limit.
    let (_, entries, total) = lib.playlist_items(s, Page::new(2, 10), &[]).unwrap();
    assert_eq!((entries.len(), total), (1, 3));
    assert_eq!(lib.playlist_track_ids(s, 100).unwrap(), [t[4], t[3], t[2]]);
    assert_eq!(lib.playlist_track_ids(s, 2).unwrap(), [t[4], t[3]]);

    // Without a limit; then content follows the library.
    lib.set_smart_query(s, q.clone(), None, Some(0), UI)
        .unwrap();
    assert_eq!(lib.playlist(s).unwrap().unwrap().track_count, 5);
    lib.remove_root(f.root, UI).unwrap();
    assert_eq!(lib.playlist(s).unwrap().unwrap().track_count, 0);
    assert!(lib.playlist_track_ids(s, 100).unwrap().is_empty());

    // A query that mentions availability lists unavailable tracks, but they are not played.
    let with_unavailable = TrackQuery {
        filter: Filter::Compare {
            field: Field::Available,
            op: CompareOp::Eq,
            value: Value::Bool(false),
        },
        sort: q.sort.clone(),
    };
    lib.set_smart_query(s, with_unavailable.clone(), Some(2), None, UI)
        .unwrap();
    assert_eq!(tracks_of(lib, s), [t[4], t[3]]);
    assert!(lib.playlist_track_ids(s, 100).unwrap().is_empty());
    lib.set_smart_query(s, with_unavailable, None, None, UI)
        .unwrap();
    assert!(lib.playlist_track_ids(s, 100).unwrap().is_empty());
}

#[test]
fn unparsable_smart_query_does_not_break_listing() {
    let f = fixture();
    let lib = &f.lib;
    five(&f);
    let s = lib
        .create_playlist(
            "s",
            PlaylistKind::Smart {
                query: TrackQuery::default(),
                max_tracks: Some(2),
            },
            UI,
        )
        .unwrap()
        .value;
    manual(lib, "m");
    let db = lib.inner().pool.path().to_owned();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE playlists SET query_json = '{\"filter\": {\"type\": \"no_such_filter\"}}'
         WHERE id = ?1",
        [s.0],
    )
    .unwrap();
    drop(conn);

    let (list, total) = lib.playlists(false, ALL).unwrap();
    assert_eq!(total, 2);
    let broken = list.iter().find(|p| p.id == s).unwrap();
    assert!(broken.query_error.as_deref().unwrap().contains("parse"));
    assert_eq!(broken.track_count, 0);
    // Its entries: none (the summary says why); playing it is an error.
    let (revision, entries, total) = lib.playlist_items(s, ALL, &[]).unwrap();
    assert_eq!((revision, entries.len(), total), (0, 0, 0));
    let (summary, entries) = lib.playlist_page(s, ALL, &[]).unwrap().unwrap();
    assert!(summary.query_error.unwrap().contains("parse"));
    assert_eq!((summary.track_count, entries.len()), (0, 0));
    assert!(matches!(
        lib.playlist_track_ids(s, 10),
        Err(LibraryError::InvalidQuery(_))
    ));
    // Replacing the query repairs it.
    lib.set_smart_query(s, TrackQuery::default(), Some(2), Some(0), UI)
        .unwrap();
    assert_eq!(lib.playlist(s).unwrap().unwrap().query_error, None);
    assert_eq!(tracks_of(lib, s).len(), 2);
}

#[test]
fn trash_restore_and_purge() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let a = manual(lib, "a");
    let b = manual(lib, "b");
    let c = manual(lib, "c");
    let (_, r) = lib.add_to_playlist(a, &t, None, None, UI).unwrap().value;

    assert!(lib.delete_playlist(a, UI).unwrap().changed());
    lib.delete_playlist(b, UI).unwrap();
    // Deleting again changes nothing (idempotent); unknown is not found.
    assert_eq!(lib.delete_playlist(a, UI).unwrap().action, None);
    assert!(is_not_found(lib.delete_playlist(PlaylistId(999), UI)));
    let (live, total) = lib.playlists(false, ALL).unwrap();
    assert_eq!((live.len(), total), (1, 1));
    let (trash, total) = lib.playlists(true, ALL).unwrap();
    assert_eq!(total, 2);
    // Most recently deleted first (same millisecond: higher id first).
    assert_eq!(trash[0].id, b);
    assert!(trash.iter().all(|p| p.deleted_at.is_some()));
    // Edits are refused (`playlist_in_trash`), reads work; the revision is unchanged.
    assert_eq!(
        code(lib.rename_playlist(a, "x", None, UI)),
        "playlist_in_trash"
    );
    assert_eq!(
        code(lib.add_to_playlist(a, &t, None, None, UI)),
        "playlist_in_trash"
    );
    assert_eq!(code(lib.undo_playlist(a, r, UI)), "playlist_in_trash");
    assert_eq!(lib.playlist(a).unwrap().unwrap().revision, r);
    assert_eq!(tracks_of(lib, a), t);
    assert_eq!(lib.playlist_track_ids(a, 10).unwrap(), t);

    // Restore: everything back, undo included.
    // Restoring one that is not in the trash changes nothing.
    assert_eq!(lib.restore_playlist(c, UI).unwrap(), Done::unchanged(()));
    assert!(is_not_found(lib.restore_playlist(PlaylistId(999), UI)));
    lib.restore_playlist(a, UI).unwrap();
    let s = lib.playlist(a).unwrap().unwrap();
    assert_eq!((s.deleted_at, s.revision, s.track_count), (None, r, 5));
    lib.undo_playlist(a, r, UI).unwrap();
    assert!(tracks_of(lib, a).is_empty());

    // Purge only what is in the trash, all or nothing.
    assert!(is_invalid(lib.purge_playlists(Some(&[b, c]), UI)));
    assert!(is_not_found(
        lib.purge_playlists(Some(&[PlaylistId(999)]), UI)
    ));
    assert_eq!(lib.purge_playlists(Some(&[b, b]), UI).unwrap().value, 1);
    assert!(lib.playlist(b).unwrap().is_none());
    lib.delete_playlist(a, UI).unwrap();
    lib.delete_playlist(c, UI).unwrap();
    assert_eq!(lib.purge_playlists(None, UI).unwrap().value, 2);
    assert_eq!(lib.playlists(true, ALL).unwrap().1, 0);
    assert_eq!(lib.playlists(false, ALL).unwrap().1, 0);
    // Items and their actions went with them; ids are not reused.
    let conn = rusqlite::Connection::open(lib.inner().pool.path()).unwrap();
    let left: i64 = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM playlist_items)
                 + (SELECT COUNT(*) FROM action_targets WHERE kind LIKE 'playlist%')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0);
    assert!(manual(lib, "new").0 > c.0);
}

/// Sets a trashed playlist's deletion time directly in the database.
fn deleted_at(lib: &Library, id: PlaylistId, at: i64) {
    let conn = rusqlite::Connection::open(lib.inner().pool.path()).unwrap();
    conn.execute(
        "UPDATE playlists SET deleted_at = ?2 WHERE id = ?1",
        [id.0, at],
    )
    .unwrap();
}

#[test]
fn expired_trash_survives_delete_restart_and_can_be_restored() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("library.sqlite");
    let old = {
        let lib = Library::open(&db).unwrap();
        let old = manual(&lib, "old");
        lib.delete_playlist(old, UI).unwrap();
        deleted_at(&lib, old, now_ms() - PLAYLIST_TRASH_RETENTION_MS - 10_000);
        let other = manual(&lib, "other");
        lib.delete_playlist(other, UI).unwrap();
        assert!(lib.playlist(old).unwrap().is_some());
        // Neither an empty selection nor purging another item includes expired trash.
        assert_eq!(lib.purge_playlists(Some(&[]), UI).unwrap().value, 0);
        assert_eq!(lib.purge_playlists(Some(&[other]), UI).unwrap().value, 1);
        assert!(lib.playlist(old).unwrap().is_some());
        old
    };
    let lib = Library::open(&db).unwrap();
    assert_eq!(lib.playlists(true, ALL).unwrap().1, 1);
    lib.restore_playlist(old, UI).unwrap();
    assert!(lib.playlist(old).unwrap().unwrap().deleted_at.is_none());
}

#[test]
fn manual_expiry_cleanup_binds_whole_trash_and_audits_only_expired_selection() {
    let f = fixture();
    let lib = &f.lib;
    let now = now_ms();
    let old = manual(lib, "expired");
    let edge = manual(lib, "not yet 30 days");
    let recent = manual(lib, "recent");
    for id in [old, edge, recent] {
        lib.delete_playlist(id, UI).unwrap();
    }
    deleted_at(lib, old, now - PLAYLIST_TRASH_RETENTION_MS - 1);
    deleted_at(lib, edge, now - PLAYLIST_TRASH_RETENTION_MS + 60_000);
    let describe = || lib.describe_purge(None).unwrap().expired_selection();
    let expected = describe();
    assert_eq!(
        expected.playlists.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![old]
    );
    assert_eq!(expected.trash.as_ref().unwrap().len(), 3);
    // New trash invalidates even though it is recent and outside the deletion set.
    let other = manual(lib, "new trash");
    lib.delete_playlist(other, UI).unwrap();
    assert!(matches!(
        lib.purge_playlists_guarded(expected, UI),
        Err(LibraryError::PurgeChanged)
    ));
    let expected = describe();
    lib.restore_playlist(recent, UI).unwrap();
    assert!(matches!(
        lib.purge_playlists_guarded(expected, UI),
        Err(LibraryError::PurgeChanged)
    ));
    let expected = describe();
    // Recheck inside the writer transaction, including unselected display data.
    let conn = rusqlite::Connection::open(lib.inner().pool.path()).unwrap();
    conn.execute("UPDATE playlists SET name='changed' WHERE id=?1", [edge.0])
        .unwrap();
    assert!(matches!(
        lib.purge_playlists_guarded(expected, UI),
        Err(LibraryError::PurgeChanged)
    ));
    let done = lib.purge_playlists_guarded(describe(), UI).unwrap();
    assert_eq!(done.value, 1);
    let audit = lib.action(done.action.unwrap()).unwrap().unwrap();
    assert_eq!(audit.kind, "purge");
    assert_eq!(audit.target_count, 1);
    assert!(!audit.undoable);
    assert!(audit.summary.contains("expired"));
    assert!(lib.playlist(old).unwrap().is_none());
    for id in [edge, recent, other] {
        assert!(lib.playlist(id).unwrap().is_some());
    }
}

// Exercise the host's shared preflight comparison and the actual delete transaction
// with independent, deterministic wall/monotonic clocks; never change the OS clock.
fn expiry_clock_conflict(old_age: i64, recent_in: i64, wall_elapsed: i64, mono_elapsed: u64) {
    let f = fixture();
    let lib = &f.lib;
    let now = now_ms();
    let old = manual(lib, "expired");
    let recent = manual(lib, "not yet expired");
    for id in [old, recent] {
        lib.delete_playlist(id, UI).unwrap();
    }
    deleted_at(lib, old, now - PLAYLIST_TRASH_RETENTION_MS - old_age);
    deleted_at(lib, recent, now - PLAYLIST_TRASH_RETENTION_MS + recent_in);
    let expected = lib
        .describe_purge(None)
        .unwrap()
        .expired_selection_at(PurgeReviewClock::at(now, 10_000));
    assert_eq!(
        expected.playlists.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![old]
    );
    let current = lib.describe_purge(None).unwrap();
    assert!(expected.matches_current_at(current.clone(), PurgeReviewClock::at(now, 10_000)));
    if wall_elapsed == mono_elapsed as i64 {
        // Exactly 30 days is still ineligible; crossing by one millisecond invalidates.
        assert!(expected.matches_current_at(
            current.clone(),
            PurgeReviewClock::at(now + recent_in, 10_000 + recent_in as u64)
        ));
    }
    let clock = PurgeReviewClock::at(now + wall_elapsed, 10_000 + mono_elapsed);
    assert!(
        !expected.matches_current_at(current, clock.clone()),
        "host preflight must reject"
    );
    let mut conn = rusqlite::Connection::open(lib.inner().pool.path()).unwrap();
    let counts = lib.inner().smart_counts.at_current_version();
    assert!(matches!(
        playlist::purge_guarded_with_clock(
            &mut conn,
            &expected,
            &counts,
            UI,
            now + wall_elapsed,
            || clock.clone()
        ),
        Err(LibraryError::PurgeChanged)
    ));
    assert!(lib.playlist(old).unwrap().is_some());
    assert!(lib.playlist(recent).unwrap().is_some());
    assert!(
        !lib.actions(None, ALL)
            .unwrap()
            .items
            .iter()
            .any(|a| a.kind == "purge")
    );
    // Ordinary explicit-ID purges have no expiry policy or review clock, and do not
    // become invalid just because another playlist crosses the threshold or clocks jump.
    let explicit = lib.describe_purge(Some(&[old])).unwrap();
    let done = playlist::purge_guarded_with_clock(
        &mut conn,
        &explicit,
        &counts,
        UI,
        now + wall_elapsed,
        || clock,
    )
    .unwrap();
    assert_eq!(done.value, 1);
    assert_eq!(
        lib.action(done.action.unwrap()).unwrap().unwrap().kind,
        "purge"
    );
    assert!(lib.playlist(recent).unwrap().is_some());
}

#[test]
fn expiry_cleanup_natural_crossing_conflicts_in_preflight_and_transaction() {
    expiry_clock_conflict(10_000, 1_000, 1_001, 1_001);
}

#[test]
fn expiry_cleanup_forward_clock_jump_conflicts_even_with_same_expired_set() {
    expiry_clock_conflict(10_000, 1_000, 20_000, 2_000);
    expiry_clock_conflict(10_000, 60_000, 20_000, 2_000);
}

#[test]
fn expiry_cleanup_backward_clock_jump_conflicts_even_with_same_expired_set() {
    expiry_clock_conflict(10_000, 1_000, -20_000, 2_000);
    expiry_clock_conflict(60_000, 1_000, -20_000, 2_000);
}

#[test]
fn expiry_cleanup_gradual_clock_corrections_pass_preflight_and_transaction() {
    for elapsed in [10_000_u64, 300_000, 900_000] {
        for ppm in [500, 1_000] {
            for direction in [-1, 1] {
                let f = fixture();
                let lib = &f.lib;
                let now = now_ms();
                let old = manual(lib, "expired");
                let recent = manual(lib, "recent");
                for id in [old, recent] {
                    lib.delete_playlist(id, UI).unwrap();
                }
                let day = 24 * 60 * 60 * 1000;
                deleted_at(lib, old, now - PLAYLIST_TRASH_RETENTION_MS - day);
                deleted_at(lib, recent, now - PLAYLIST_TRASH_RETENTION_MS + day);
                let expected = lib
                    .describe_purge(None)
                    .unwrap()
                    .expired_selection_at(PurgeReviewClock::at(now, 10_000));
                let correction = elapsed as i64 * ppm / 1_000_000 * direction;
                let wall = now + elapsed as i64 + correction;
                let clock = PurgeReviewClock::at(wall, 10_000 + elapsed);
                assert!(
                    expected.matches_current_at(lib.describe_purge(None).unwrap(), clock.clone()),
                    "normal {direction:+} {ppm} ppm correction after {elapsed} ms"
                );
                let mut conn = rusqlite::Connection::open(lib.inner().pool.path()).unwrap();
                let counts = lib.inner().smart_counts.at_current_version();
                let done = playlist::purge_guarded_with_clock(
                    &mut conn,
                    &expected,
                    &counts,
                    UI,
                    wall,
                    || clock,
                )
                .unwrap();
                assert_eq!(done.value, 1);
                assert!(lib.playlist(old).unwrap().is_none());
                assert!(lib.playlist(recent).unwrap().is_some());
                let audit = lib.action(done.action.unwrap()).unwrap().unwrap();
                assert_eq!(audit.kind, "purge");
                assert_eq!(audit.target_count, 1);
                assert!(!audit.undoable);
            }
        }
    }
}

#[test]
fn expiry_cleanup_one_second_clock_steps_still_conflict() {
    // Keep the expired set unchanged so only the time check can reject the step.
    for elapsed in [10_000_i64, 300_000, 900_000] {
        for jump in [-1_000, 1_000] {
            expiry_clock_conflict(86_400_000, 86_400_000, elapsed + jump, elapsed as u64);
        }
    }
}

#[test]
fn expiry_cleanup_allowed_slew_does_not_relax_expired_set_comparison() {
    // +1000 ppm is allowed, but its extra 10 ms makes the unreviewed item expire.
    expiry_clock_conflict(10_000, 10_005, 10_010, 10_000);
}

#[test]
fn items_survive_root_removal_and_rescans() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let p = manual(lib, "p");
    lib.add_to_playlist(p, &[t[0], t[1], t[0]], None, None, UI)
        .unwrap();

    // A file gone at a rescan: the item stays, unavailable, and is not played.
    write(
        lib,
        f.root,
        ScanBatch {
            unavailable: vec![t[1]],
            ..ScanBatch::default()
        },
    );
    let (_, entries, total) = lib.playlist_items(p, ALL, &[]).unwrap();
    assert_eq!(total, 3);
    let available: Vec<_> = entries.iter().map(|e| e.row.track.available).collect();
    assert_eq!(available, [true, false, true]);
    assert_eq!(lib.playlist_track_ids(p, 10).unwrap(), [t[0], t[0]]);
    assert_eq!(lib.playlist_track_ids(p, 1).unwrap(), [t[0]]);
    // Back at the next scan: same track id.
    write(
        lib,
        f.root,
        ScanBatch {
            available: vec![t[1]],
            ..ScanBatch::default()
        },
    );
    assert_eq!(lib.playlist_track_ids(p, 10).unwrap(), [t[0], t[1], t[0]]);

    // Root removed: all items stay, unavailable; re-adding and rescanning brings them back.
    lib.remove_root(f.root, UI).unwrap();
    let (_, entries, _) = lib.playlist_items(p, ALL, &[]).unwrap();
    assert_eq!(entries.len(), 3);
    assert!(entries.iter().all(|e| !e.row.track.available));
    assert!(lib.playlist_track_ids(p, 10).unwrap().is_empty());
    assert_eq!(lib.playlist(p).unwrap().unwrap().track_count, 3);
    assert_eq!(lib.add_root(&music(&f), UI).unwrap().value, f.root);
    upsert(
        lib,
        vec![
            record(f.root, "a/0.flac", meta("t0", "Artist", "Album")),
            record(f.root, "a/1.flac", meta("t1 retagged", "Artist", "Album")),
        ],
    );
    let (_, entries, _) = lib.playlist_items(p, ALL, &[]).unwrap();
    assert_eq!(entries[1].row.track.title.as_deref(), Some("t1 retagged"));
    assert_eq!(lib.playlist_track_ids(p, 10).unwrap(), [t[0], t[1], t[0]]);

    // Tracks are never hard-deleted; if one were, the items would block it rather than
    // vanish.
    let conn = rusqlite::Connection::open(lib.inner().pool.path()).unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    assert!(
        conn.execute("DELETE FROM tracks WHERE id = ?1", [t[1].0])
            .is_err()
    );
}

/// A v4 database file with the persisted formats v5 changes: an unversioned smart query
/// in the old JSON shape (externally tagged values, the `bits` field), one-step undo
/// records (one current, one stale), ratings of 0 and a play event with a reason this
/// build does not know.

#[test]
fn smart_queries_are_stored_versioned() {
    let f = fixture();
    let lib = &f.lib;
    five(&f);
    let query = by_track_no(Filter::Compare {
        field: Field::BitsPerSample,
        op: CompareOp::Ge,
        value: Value::Int(16),
    });
    let s = lib
        .create_playlist(
            "s",
            PlaylistKind::Smart {
                query: query.clone(),
                max_tracks: Some(3),
            },
            UI,
        )
        .unwrap()
        .value;
    let conn = rusqlite::Connection::open(lib.inner().pool.path()).unwrap();
    let stored: String = conn
        .query_row(
            "SELECT query_json FROM playlists WHERE id = ?1",
            [s.0],
            |r| r.get(0),
        )
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(json["v"], 1);
    assert_eq!(
        json["query"]["filter"],
        serde_json::json!({"type": "compare", "field": {"type": "bits_per_sample"},
                           "op": "ge", "value": {"type": "int", "value": 16}})
    );
    // A version this build does not know is reported, not misread.
    conn.execute(
        "UPDATE playlists SET query_json = ?2 WHERE id = ?1",
        rusqlite::params![s.0, stored.replace(r#""v":1"#, r#""v":2"#)],
    )
    .unwrap();
    let summary = lib.playlist(s).unwrap().unwrap();
    assert!(
        summary
            .query_error
            .as_deref()
            .unwrap()
            .contains("version 2"),
        "{summary:?}"
    );
}

fn scan_and_wait(lib: &Library) {
    match lib.scan(|_| {}).unwrap() {
        crate::ScanStart::Started(handle) => {
            handle.wait().unwrap();
        }
        crate::ScanStart::Coalesced(_) => panic!("a scan was already running"),
    }
}

#[test]
fn new_database_needs_no_rescan_but_encoding_change_does() {
    let f = fixture();
    assert!(!f.lib.rescan_recommended().unwrap());
    f.lib.set_root_encoding(f.root, Some("gbk")).unwrap();
    assert!(f.lib.rescan_recommended().unwrap());
    scan_and_wait(&f.lib);
    assert!(!f.lib.rescan_recommended().unwrap());
}

// ---- M3U ---

/// Writes `text` (already encoded) as `name` in `dir`.
fn m3u_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn import_m3u8_relative_absolute_and_uri() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let music = crate::storage_access::canonical_dir(&music(&f)).unwrap();
    // A CJK file name, existing on disk.
    std::fs::create_dir_all(music.join("周砚舟")).unwrap();
    std::fs::write(music.join("周砚舟/晶港.flac"), b"").unwrap();
    upsert(
        lib,
        vec![record(
            f.root,
            "周砚舟/晶港.flac",
            meta("晶港", "周砚舟", "叶窗集"),
        )],
    );
    let sunny = crate::tests::id_of(lib, "晶港");
    let lists = music.join("lists");
    std::fs::create_dir(&lists).unwrap();
    let abs = music.join("a/1.flac");
    let uri_path = music.join("周砚舟/晶港.flac");
    let uri_text: String = uri_path
        .to_str()
        .unwrap()
        .replace('\\', "/")
        .bytes()
        .map(|b| match b {
            b'/' | b':' | b'.' | b'-' | b'_' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    let uri = if uri_text.starts_with('/') {
        format!("file://{uri_text}")
    } else {
        format!("file:///{uri_text}")
    };
    let text = format!(
        "\u{feff}#EXTM3U\n#EXTINF:3,Artist - t2\n../a/2.flac\n{}\r\n{uri}\n# comment\n\
         ../a/missing.flac\n/nowhere/x.mp3\n../a/./0.flac\n",
        abs.display()
    );
    let path = m3u_file(&lists, "Road Trip.m3u8", text.as_bytes());
    let (p, report) = lib.import_m3u(&path, None, UI).unwrap().value;
    assert_eq!(report.matched, 4);
    assert_eq!(report.missing, ["../a/missing.flac", "/nowhere/x.mp3"]);
    let s = lib.playlist(p).unwrap().unwrap();
    assert_eq!((s.name.as_str(), s.revision), ("Road Trip", 0));
    assert_eq!(tracks_of(lib, p), [t[2], t[1], sunny, t[0]]);

    // An explicit name; a missing file is `file_not_found`.
    let (q, _) = lib.import_m3u(&path, Some("Named"), UI).unwrap().value;
    assert_eq!(lib.playlist(q).unwrap().unwrap().name, "Named");
    assert_eq!(
        code(lib.import_m3u(&lists.join("nope.m3u"), None, UI)),
        "file_not_found"
    );
    assert!(matches!(
        lib.import_m3u(&lists.join("nope.m3u"), None, UI),
        Err(LibraryError::NotFound { .. })
    ));
}

#[test]
fn import_gbk_m3u() {
    let f = fixture();
    let lib = &f.lib;
    let music = crate::storage_access::canonical_dir(&music(&f)).unwrap();
    std::fs::create_dir_all(music.join("周砚舟")).unwrap();
    for name in ["晶港", "云盒之声"] {
        std::fs::write(music.join(format!("周砚舟/{name}.mp3")), b"").unwrap();
    }
    upsert(
        lib,
        vec![
            record(f.root, "周砚舟/晶港.mp3", meta("晶港", "周砚舟", "叶窗集")),
            record(
                f.root,
                "周砚舟/云盒之声.mp3",
                meta("云盒之声", "周砚舟", "叶窗集"),
            ),
        ],
    );
    let text = "#EXTM3U\r\n#EXTINF:269,周砚舟 - 晶港\r\n周砚舟\\晶港.mp3\r\n周砚舟/云盒之声.mp3\r\n周砚舟/雾盒帆.mp3\r\n";
    let (bytes, _, unmappable) = encoding_rs::GBK.encode(text);
    assert!(!unmappable);
    let path = m3u_file(&music, "老歌.m3u", &bytes);
    let (p, report) = lib.import_m3u(&path, None, UI).unwrap().value;
    assert_eq!(report.matched, 2);
    assert_eq!(report.missing, ["周砚舟/雾盒帆.mp3"]);
    assert_eq!(lib.playlist(p).unwrap().unwrap().name, "老歌");
    let titles: Vec<_> = lib
        .playlist_items(p, ALL, &[])
        .unwrap()
        .1
        .into_iter()
        .map(|e| e.row.track.title.unwrap())
        .collect();
    assert_eq!(titles, ["晶港", "云盒之声"]);
}

#[test]
fn export_m3u_round_trip() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let music = crate::storage_access::canonical_dir(&music(&f)).unwrap();
    let p = manual(lib, "p");
    lib.add_to_playlist(p, &[t[3], t[0], t[3]], None, None, UI)
        .unwrap();
    // Unavailable tracks are exported at their last known path.
    write(
        lib,
        f.root,
        ScanBatch {
            unavailable: vec![t[0]],
            ..ScanBatch::default()
        },
    );
    let sep = std::path::MAIN_SEPARATOR;

    // In the root: relative paths.
    let in_root = music.join("mine.m3u8");
    lib.export_m3u(p, &in_root, false, false, UI).unwrap();
    let text = std::fs::read_to_string(&in_root).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines[0], "#EXTM3U");
    assert_eq!(lines[1], "#EXTINF:4,Artist - t3");
    assert_eq!(lines[2], format!("a{sep}3.flac"));
    assert_eq!(lines[4], format!("a{sep}0.flac"));
    assert_eq!(lines.len(), 7);
    let (back, report) = lib.import_m3u(&in_root, None, UI).unwrap().value;
    assert_eq!(report.missing, Vec::<String>::new());
    assert_eq!(tracks_of(lib, back), [t[3], t[0], t[3]]);

    // In a sub-folder of the root: `..`.
    std::fs::create_dir(music.join("lists")).unwrap();
    let nested = music.join("lists/mine.m3u");
    lib.export_m3u(p, &nested, false, false, UI).unwrap();
    let text = std::fs::read_to_string(&nested).unwrap();
    assert_eq!(text.lines().nth(2).unwrap(), format!("..{sep}a{sep}3.flac"));
    let (back, _) = lib.import_m3u(&nested, None, UI).unwrap().value;
    assert_eq!(tracks_of(lib, back), [t[3], t[0], t[3]]);

    // Outside the root: absolute paths.
    let outside = f._dir.path().join("out.m3u8");
    lib.export_m3u(p, &outside, false, false, UI).unwrap();
    let text = std::fs::read_to_string(&outside).unwrap();
    assert_eq!(
        PathBuf::from(text.lines().nth(2).unwrap()),
        music.join("a").join("3.flac")
    );
    let (back, _) = lib.import_m3u(&outside, None, UI).unwrap().value;
    assert_eq!(tracks_of(lib, back), [t[3], t[0], t[3]]);

    // Smart playlists export their query result; a missing folder is an error.
    let s = lib
        .create_playlist(
            "s",
            PlaylistKind::Smart {
                query: by_track_no(Filter::All),
                max_tracks: Some(2),
            },
            UI,
        )
        .unwrap()
        .value;
    // The file exists: replaced only on request.
    assert!(matches!(
        lib.export_m3u(s, &outside, false, false, UI),
        Err(LibraryError::AlreadyExists(_))
    ));
    lib.export_m3u(s, &outside, true, false, UI).unwrap();
    let (back, _) = lib.import_m3u(&outside, None, UI).unwrap().value;
    assert_eq!(tracks_of(lib, back), [t[1], t[2]]);
    assert!(
        lib.export_m3u(s, &f._dir.path().join("no/such/dir.m3u"), false, false, UI)
            .is_err()
    );
    assert!(is_not_found(lib.export_m3u(
        PlaylistId(999),
        &outside,
        true,
        false,
        UI
    )));
    // No temporary files are left behind.
    let names: Vec<_> = std::fs::read_dir(f._dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert_eq!(names, Vec::<String>::new());
}

// ---- scale ---

fn playlist_latency(items: usize) {
    let dir = TempDir::new().unwrap();
    let music = dir.path().join("m");
    std::fs::create_dir(&music).unwrap();
    let lib = Library::open(&dir.path().join("library.sqlite")).unwrap();
    let root: RootId = lib.add_root(&music, UI).unwrap().value;
    let tracks = 2_000.min(items);
    upsert(
        &lib,
        (0..tracks)
            .map(|i| record(root, &format!("{i}.flac"), meta(&format!("t{i}"), "a", "b")))
            .collect(),
    );
    let ids: Vec<TrackId> = (0..items)
        .map(|i| TrackId((i % tracks) as i64 + 1))
        .collect();
    let p = manual(&lib, "big");
    let time = |label: &str, run: &mut dyn FnMut()| {
        let start = Instant::now();
        run();
        eprintln!("{label:<34} {:>9.2?}", start.elapsed());
    };
    let mut items_ids = Vec::new();
    let mut r = 0;
    time(&format!("add {items} items"), &mut || {
        (items_ids, r) = lib.add_to_playlist(p, &ids, None, None, UI).unwrap().value;
    });
    time("add 1 at the front", &mut || {
        r = lib
            .add_to_playlist(p, &[ids[0]], Some(0), Some(r), UI)
            .unwrap()
            .value
            .1;
    });
    time("undo (remove it)", &mut || {
        r = lib.undo_playlist(p, r, UI).unwrap().value
    });
    time("move first to last", &mut || {
        r = lib
            .move_playlist_item(p, items_ids[0], items as u32 - 1, Some(r), UI)
            .unwrap()
            .value;
    });
    time("undo (move back)", &mut || {
        r = lib.undo_playlist(p, r, UI).unwrap().value
    });
    let batch: Vec<PlaylistItemId> = items_ids.iter().step_by(100).copied().collect();
    time(&format!("remove {} scattered", batch.len()), &mut || {
        r = lib
            .remove_from_playlist(p, &batch, Some(r), UI)
            .unwrap()
            .value
            .1;
    });
    let conn = rusqlite::Connection::open(lib.inner().pool.path()).unwrap();
    let undo_bytes: i64 = conn
        .query_row(
            "SELECT length(inverse) FROM actions ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    eprintln!("undo record of that remove: {undo_bytes} bytes");
    time("undo (restore them)", &mut || {
        r = lib.undo_playlist(p, r, UI).unwrap().value
    });
    assert_eq!(items_of_page(&lib, p, 0, 3), items_ids[..3]);
    let mid = items / 2;
    time("page of 100 at the middle", &mut || {
        assert_eq!(
            items_of_page(&lib, p, mid as u32, 100),
            items_ids[mid..(mid + 100).min(items)]
        );
    });
    time("summary", &mut || {
        assert_eq!(lib.playlist(p).unwrap().unwrap().track_count, items as u64)
    });
    time("track ids for playing", &mut || {
        assert_eq!(lib.playlist_track_ids(p, usize::MAX).unwrap().len(), items)
    });
    assert_dense(&lib, p);
    time("purge", &mut || {
        lib.delete_playlist(p, UI).unwrap();
        lib.purge_playlists(None, UI).unwrap();
    });
}

fn items_of_page(lib: &Library, p: PlaylistId, offset: u32, limit: u32) -> Vec<PlaylistItemId> {
    let (_, entries, _) = lib
        .playlist_items(p, Page::new(offset, limit), &[])
        .unwrap();
    entries.iter().map(|e| e.item.unwrap()).collect()
}

#[test]
fn playlist_edits_on_10k_items() {
    playlist_latency(10_000);
}

/// 100k-item playlist edit timings: `cargo test -p mpmp-library --release -- --ignored
/// scale_100k_playlist --nocapture`.
#[test]
#[ignore = "builds a 100k-item playlist; run with --ignored, preferably --release"]
fn scale_100k_playlist() {
    playlist_latency(100_000);
}

/// A location core checked for an agent
/// (`ActionMeta::checked`) is checked once more right before it is used: it must still be
/// canonical, so a symlink put in its way since makes the call fail (`location_changed`)
/// without reading or writing anything there. And an agent's import shows only the lines
/// that look like media, so that importing any text file reads nothing of it back.
#[test]
fn checked_locations_are_checked_again_before_use() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let checked = UI.with_checked(true);
    let music = crate::storage_access::canonical_dir(&music(&f)).unwrap();
    let outside = crate::storage_access::canonical_dir(f._dir.path())
        .unwrap()
        .join("outside");
    std::fs::create_dir_all(outside.join("sub")).unwrap();
    let p = manual(lib, "p");
    lib.add_to_playlist(p, &[t[0]], None, None, UI).unwrap();

    // Not canonical (any more): refused, nothing done. (Win32 resolves `..` by name, so
    // there a different letter case makes the path not canonical.)
    #[cfg(unix)]
    let (detour, outside_detour) = (
        music.join("a").join("..").join("lists"),
        outside.join("sub").join(".."),
    );
    #[cfg(windows)]
    let (detour, outside_detour) = (
        music.with_file_name("MUSIC").join("lists"),
        outside.with_file_name("OUTSIDE"),
    );
    std::fs::create_dir(music.join("lists")).unwrap();
    assert_eq!(
        code(lib.export_m3u(p, &detour.join("x.m3u"), false, true, checked)),
        "location_changed"
    );
    assert!(!music.join("lists/x.m3u").exists());
    std::fs::write(music.join("lists/in.m3u"), "a/0.flac\n").unwrap();
    assert_eq!(
        code(lib.import_m3u(&detour.join("in.m3u"), None, checked)),
        "location_changed"
    );
    assert_eq!(
        code(lib.add_root(&outside_detour, checked)),
        "location_changed"
    );
    // Unchecked (the UI's paths), the same is fine.
    lib.export_m3u(p, &detour.join("x.m3u"), false, false, UI)
        .unwrap();
    assert!(music.join("lists/x.m3u").exists());
    // Canonical: as before.
    lib.export_m3u(p, &music.join("lists/y.m3u"), false, true, checked)
        .unwrap();
    let added = lib.add_root(&outside.join("sub"), checked).unwrap();
    assert!(added.changed());

    // An agent's import shows only what looks like media; the count has every line.
    let text = "../a/0.flac\nTOP-SECRET-LINE\nhttp://radio.example/live.mp3?token=1\n\
                gone/x.FLAC\nkey = value\nother/sheet.cue\n.flac\n";
    std::fs::write(music.join("lists/mixed.m3u"), text).unwrap();
    let (_, report) = lib
        .import_m3u(&music.join("lists/mixed.m3u"), None, checked)
        .unwrap()
        .value;
    assert_eq!(report.matched, 1);
    assert_eq!(
        report.missing,
        [
            "http://radio.example/live.mp3?token=1",
            "gone/x.FLAC",
            "other/sheet.cue"
        ]
    );
    assert_eq!(report.missing_total, 6);
    let (_, report) = lib
        .import_m3u(&music.join("lists/mixed.m3u"), None, UI)
        .unwrap()
        .value;
    assert_eq!(report.missing.len(), 6, "the UI sees every line");
}

/// The same with symlinks put in the way: through a link, a checked location is refused;
/// a file that is a link to another (the agent's own file system tools may make one) is
/// not read.
#[cfg(unix)]
#[test]
fn checked_locations_do_not_follow_links_put_in_their_way() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let checked = UI.with_checked(true);
    let music = crate::storage_access::canonical_dir(&music(&f)).unwrap();
    let outside = crate::storage_access::canonical_dir(f._dir.path())
        .unwrap()
        .join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "TOP-SECRET-LINE\n").unwrap();
    let p = manual(lib, "p");
    lib.add_to_playlist(p, &[t[0]], None, None, UI).unwrap();
    // A folder that became a link to elsewhere.
    std::os::unix::fs::symlink(&outside, music.join("new")).unwrap();
    assert_eq!(
        code(lib.export_m3u(p, &music.join("new/x.m3u"), false, true, checked)),
        "location_changed"
    );
    assert!(!outside.join("x.m3u").exists());
    assert_eq!(
        code(lib.add_root(&music.join("new"), checked)),
        "location_changed"
    );
    // A playlist file that became a link to another file.
    std::os::unix::fs::symlink(outside.join("secret.txt"), music.join("x.m3u")).unwrap();
    let e = lib
        .import_m3u(&music.join("x.m3u"), None, checked)
        .unwrap_err();
    assert_eq!(e.code(), "location_changed");
    assert!(!e.to_string().contains("TOP-SECRET"));
}

/// An export to a folder that does not exist fails as
/// it does for anyone (the folder never existed: nothing changed), not `location_changed`.
#[test]
fn checked_exports_to_a_missing_folder_fail_as_any_other() {
    let f = fixture();
    let lib = &f.lib;
    let music = crate::storage_access::canonical_dir(&music(&f)).unwrap();
    let p = manual(lib, "p");
    let path = music.join("nope").join("x.m3u");
    let checked = code(lib.export_m3u(p, &path, true, true, UI.with_checked(true)));
    assert_eq!(checked, "io");
    assert_eq!(code(lib.export_m3u(p, &path, true, true, UI)), checked);
}

/// A checked export replaces only what was there when it
/// began. A symlink at the path (checked in canonical form, so put in since) is
/// `location_changed`, as for the UI it is `not_a_regular_file`.
#[cfg(unix)]
#[test]
fn checked_exports_replace_only_what_they_found() {
    let f = fixture();
    let lib = &f.lib;
    let music = crate::storage_access::canonical_dir(&music(&f)).unwrap();
    let p = manual(lib, "p");
    std::fs::write(music.join("other.txt"), "other").unwrap();
    std::os::unix::fs::symlink(music.join("other.txt"), music.join("x.m3u")).unwrap();
    let path = music.join("x.m3u");
    assert_eq!(
        code(lib.export_m3u(p, &path, true, true, UI.with_checked(true))),
        "location_changed"
    );
    assert_eq!(
        code(lib.export_m3u(p, &path, true, true, UI)),
        "not_a_regular_file"
    );
    assert_eq!(
        std::fs::read_to_string(music.join("other.txt")).unwrap(),
        "other"
    );
    let left: Vec<_> = std::fs::read_dir(&music)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// A FIFO named like a playlist is not opened in a way that
/// waits for a writer: importing it is `not_a_regular_file` right away, checked or not.
/// A checked import opens no symlink (`O_NOFOLLOW`): one to a
/// FIFO is `location_changed` right away (unchecked, it is followed: `not_a_regular_file`).
#[cfg(unix)]
#[test]
fn only_regular_files_are_imported() {
    use std::time::Duration;
    let f = fixture();
    let music = crate::storage_access::canonical_dir(&music(&f)).unwrap();
    let fifo = music.join("x.m3u");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let link = music.join("link.m3u");
    std::os::unix::fs::symlink(&fifo, &link).unwrap();
    let cases = [
        (&fifo, false, "not_a_regular_file"),
        (&fifo, true, "not_a_regular_file"),
        (&link, false, "not_a_regular_file"),
        (&link, true, "location_changed"),
    ];
    for (path, checked, expected) in cases {
        let (lib, at) = (f.lib.clone(), path.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = lib.import_m3u(&at, None, UI.with_checked(checked));
            let _ = tx.send(result.map(|_| ()).map_err(|e| e.code()));
        });
        let result = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the import returns without waiting for a writer");
        assert_eq!(result, Err(expected), "{path:?} {checked}");
    }
}

/// A folder named like a playlist is `not_a_regular_file`,
/// checked or not, on every platform (Windows cannot open a folder as a file at all).
#[test]
fn folders_are_not_imported() {
    let f = fixture();
    let music = crate::storage_access::canonical_dir(&music(&f)).unwrap();
    let folder = music.join("d.m3u");
    std::fs::create_dir(&folder).unwrap();
    for checked in [false, true] {
        let e = f
            .lib
            .import_m3u(&folder, None, UI.with_checked(checked))
            .unwrap_err();
        assert_eq!(e.code(), "not_a_regular_file", "{checked}");
    }
}

#[test]
fn guarded_playback_counts_playable_items_and_rejects_unavailable_or_capped_items() {
    let f = fixture();
    let lib = &f.lib;
    let t = five(&f);
    let p = manual(lib, "guarded");
    lib.add_to_playlist(p, &[t[0], t[1], t[0]], None, None, UI)
        .unwrap();
    let (revision, items, _) = lib.playlist_items(p, ALL, &[]).unwrap();
    write(
        lib,
        f.root,
        ScanBatch {
            unavailable: vec![t[1]],
            ..ScanBatch::default()
        },
    );
    assert_eq!(
        lib.playlist_playback(p, 10, items[2].item, Some(revision))
            .unwrap(),
        (vec![t[0], t[0]], Some(1))
    );
    for (limit, item) in [(10, items[1].item), (1, items[2].item), (0, items[0].item)] {
        assert!(matches!(
            lib.playlist_playback(p, limit, item, Some(revision)),
            Err(LibraryError::NotFound {
                what: crate::NotFound::Item,
                ..
            })
        ));
    }
    assert!(matches!(
        lib.playlist_playback(p, 10, items[2].item, Some(revision + 1)),
        Err(LibraryError::PlaylistConflict { .. })
    ));
}
