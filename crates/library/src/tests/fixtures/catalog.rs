//! Catalog, storage and scan scenarios over shared synthetic rows.
use super::*;

#[test]
fn search_uses_trigram_like_and_mixed_terms() {
    let f = fixture();
    sample(&f);
    let lib = &f.lib;

    // Three or more characters: trigram index.
    assert_eq!(titles(&lib.search("周砚舟", 10).unwrap()).len(), 3);
    assert_eq!(
        titles(&lib.search("雲箱のア", 10).unwrap()),
        ["静かな雲箱のアーチ"]
    );
    // Full-width text and queries meet in the middle.
    assert_eq!(
        titles(&lib.search("abc song", 10).unwrap()),
        ["ＡＢＣ　Ｓｏｎｇ"]
    );
    assert_eq!(titles(&lib.search("ＳＯＮＧ", 10).unwrap()).len(), 3);
    // One and two characters: LIKE.
    assert_eq!(titles(&lib.search("晶港", 10).unwrap()), ["晶港"]);
    assert_eq!(titles(&lib.search("晶", 10).unwrap()), ["晶港"]);
    // Mixed: both must match.
    assert_eq!(titles(&lib.search("周砚舟 晶", 10).unwrap()), ["晶港"]);
    assert!(lib.search("周砚舟 abc", 10).unwrap().tracks.is_empty());
    // Untagged files are found by file name.
    assert_eq!(titles(&lib.search("lonely", 10).unwrap()), ["Lonely Noise"]);
    // Quotes, FTS operators and LIKE wildcards are literal.
    assert_eq!(titles(&lib.search(r#""hi""#, 10).unwrap()).len(), 1);
    assert_eq!(titles(&lib.search("0%", 10).unwrap()).len(), 1);
    assert_eq!(titles(&lib.search("_d", 10).unwrap()).len(), 1);
    assert_eq!(titles(&lib.search("%", 10).unwrap()).len(), 1);
    assert!(lib.search("x%y", 10).unwrap().tracks.is_empty());
    assert!(lib.search("AND OR NOT", 10).unwrap().tracks.is_empty());
    assert!(lib.search("   ", 10).unwrap().tracks.is_empty());
    // Limits.
    assert_eq!(lib.search("song", 1).unwrap().tracks.len(), 1);

    let results = lib.search("叶窗集", 10).unwrap();
    assert_eq!(results.albums.len(), 1);
    assert_eq!(results.albums[0].title, "叶窗集");
    assert_eq!(results.albums[0].track_count, 3);
    let results = lib.search("周砚", 10).unwrap();
    assert_eq!(results.artists.len(), 1);
    assert_eq!(results.artists[0].name, "周砚舟");
    assert_eq!(results.artists[0].track_count, 3);
    // Totals count every match, whatever the limit (also 0).
    for text in [
        "周砚舟",
        "晶",
        "song",
        "周砚舟 晶",
        "叶窗集",
        "周砚",
        "lonely",
        "%",
        "雲箱のア",
        "o",
    ] {
        let all = lib.search(text, 1000).unwrap();
        for limit in [0, 1] {
            let few = lib.search(text, limit).unwrap();
            assert!(few.tracks.len() <= limit && few.albums.len() <= limit);
            assert_eq!(
                (few.tracks_total, few.albums_total, few.artists_total),
                (
                    all.tracks.len() as u64,
                    all.albums.len() as u64,
                    all.artists.len() as u64
                ),
                "{text:?}"
            );
        }
    }
    assert_eq!(lib.search("song", 1).unwrap().tracks_total, 3);
}

#[test]
fn albums_group_and_order() {
    let f = fixture();
    sample(&f);
    let lib = &f.lib;

    let page = lib.albums(AlbumSort::Title, Page::new(0, 100)).unwrap();
    assert_eq!(page.total, 6);
    let albums = page.items;
    let names: Vec<_> = albums.iter().map(|a| a.title.as_str()).collect();
    // Tracks without an album tag: one unknown album (empty title) per artist, last.
    assert_eq!(names, ["Hits", "叶窗集", "", "", "", ""]);
    // The default order is by artist.
    assert_eq!(AlbumSort::default(), AlbumSort::Artist);
    assert!(albums[2..].iter().all(|a| a.key.is_unknown_album()));
    let unknown = lib.album(&AlbumKey::new("高雲帆子", "")).unwrap().unwrap();
    assert_eq!(unknown.summary.artist.as_deref(), Some("高雲帆子"));
    assert_eq!(unknown.tracks[0].display_title(), "静かな雲箱のアーチ");
    // No tags at all: unknown artist, unknown album.
    let nothing = lib.album(&AlbumKey::new("", "")).unwrap().unwrap();
    assert_eq!(nothing.summary.artist, None);
    assert_eq!(nothing.tracks[0].display_title(), "Lonely Noise");
    let hits = &albums[0];
    assert_eq!(hits.artist.as_deref(), Some(VARIOUS_ARTISTS));
    assert_eq!(hits.track_count, 2);
    assert_eq!(hits.key, AlbumKey::new(VARIOUS_ARTISTS, "hits"));

    let album = lib.album(&albums[1].key).unwrap().unwrap();
    assert_eq!(album.summary.disc_count, 2);
    assert_eq!(album.summary.duration_ms, 3000);
    assert!(album.summary.art_track.is_some());
    let order: Vec<_> = album.tracks.iter().map(|t| t.display_title()).collect();
    assert_eq!(order, ["云盒之声", "晶港", "Second Disc Opener"]);

    let page = lib.albums(AlbumSort::Artist, Page::new(1, 1)).unwrap();
    assert_eq!((page.items.len(), page.total), (1, 6));
    assert!(lib.album(&AlbumKey::new("x", "y")).unwrap().is_none());

    // Artists are album artists, like the album grid.
    let page = lib.artists(Page::new(0, 100)).unwrap();
    assert_eq!(page.total, 5);
    let artists = page.items;
    let names: Vec<_> = artists.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "ＤＪ Ｗｉｄｅ",
            "Quote",
            "Various Artists",
            "周砚舟",
            "高雲帆子"
        ]
    );
    let va = artists.iter().find(|a| a.name == VARIOUS_ARTISTS).unwrap();
    assert_eq!((va.album_count, va.track_count), (1, 2));
    let jay = artists.iter().find(|a| a.name == "周砚舟").unwrap();
    assert_eq!((jay.album_count, jay.track_count), (1, 3));

    let by_jay = lib
        .albums_by_artist("周砚舟", AlbumSort::Title, Page::new(0, 10))
        .unwrap();
    assert_eq!((by_jay.items.len(), by_jay.total), (1, 1));
    assert_eq!(by_jay.items[0].title, "叶窗集");
    let by_va = lib
        .albums_by_artist("VARIOUS artists", AlbumSort::Year, Page::new(0, 10))
        .unwrap();
    assert_eq!(by_va.items[0].title, "Hits");
    let total = |name| {
        lib.albums_by_artist(name, AlbumSort::Title, Page::new(0, 0))
            .unwrap()
            .total
    };
    assert_eq!(total("ＤＪ Ｗｉｄｅ"), 1);
    assert_eq!(total("nobody"), 0);
}

#[test]
fn rescan_batches_keep_ids_and_availability() {
    let f = fixture();
    sample(&f);
    let lib = &f.lib;
    let id = id_of(lib, "晶港");
    let before = lib.track(id).unwrap().unwrap();

    // A re-read under the same path keeps the id and added_at; the FTS index follows.
    let mut changed = meta("晶港 (Live)", "周砚舟", "叶窗集");
    changed.disc_no = Some(1);
    upsert(lib, vec![record(f.root, "jay/1.flac", changed)]);
    let after = lib.track(id).unwrap().unwrap();
    assert_eq!(after.title.as_deref(), Some("晶港 (Live)"));
    assert_eq!(after.added_at, before.added_at);
    assert_eq!(titles(&lib.search("live", 10).unwrap()), ["晶港 (Live)"]);

    write(
        lib,
        f.root,
        ScanBatch {
            unavailable: vec![id],
            ..ScanBatch::default()
        },
    );
    assert!(!lib.track(id).unwrap().unwrap().available);
    assert!(lib.search("live", 10).unwrap().tracks.is_empty());
    let album = lib.album(&after.album_key).unwrap().unwrap();
    assert_eq!(album.tracks.len(), 2);
    assert!(lib.uri_of(id).unwrap().is_some());

    write(
        lib,
        f.root,
        ScanBatch {
            available: vec![id],
            ..ScanBatch::default()
        },
    );
    assert!(lib.track(id).unwrap().unwrap().available);

    let uri = lib.uri_of(id).unwrap().unwrap();
    assert!(uri.as_str().starts_with("file://"));
    assert!(uri.as_str().ends_with("jay/1.flac") || uri.as_str().ends_with("jay\\1.flac"));
    assert!(lib.uri_of(TrackId(999_999)).unwrap().is_none());

    let ids = [id, TrackId(424242), id_of(lib, "云盒之声")];
    let tracks = lib.tracks(&ids).unwrap();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].id, id);
}

#[test]
fn roots_overlap_encoding_and_removal() {
    let f = fixture();
    sample(&f);
    let lib = &f.lib;
    let roots = lib.roots().unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].track_count, 9);
    let music = roots[0].path.clone();
    assert_eq!(lib.add_root(&music, UI).unwrap().value, f.root);

    let nested = music.join("sub");
    std::fs::create_dir(&nested).unwrap();
    assert!(matches!(
        lib.add_root(&nested, UI),
        Err(crate::LibraryError::OverlappingRoot { .. })
    ));
    assert!(matches!(
        lib.add_root(Path::new("/definitely/not/here"), UI),
        Err(crate::LibraryError::NotADirectory(_))
    ));

    lib.set_root_encoding(f.root, Some("GBK")).unwrap();
    assert_eq!(
        lib.roots().unwrap()[0].forced_encoding.as_deref(),
        Some("gbk")
    );
    assert!(matches!(
        lib.set_root_encoding(f.root, Some("klingon")),
        Err(crate::LibraryError::Invalid {
            why: crate::Invalid::UnknownEncoding,
            ..
        })
    ));

    let id = id_of(lib, "晶港");
    lib.record_play(PlayEvent {
        track_id: id,
        context: Some(r#"{"album":"x"}"#.into()),
        started_at: 1000,
        played_ms: 200_000,
        ended_reason: EndedReason::Completed,
        origin: "ui".into(),
    })
    .unwrap();
    lib.record_play(PlayEvent {
        track_id: id,
        context: None,
        started_at: 2000,
        played_ms: 5_000,
        ended_reason: EndedReason::Skipped,
        origin: "agent:test".into(),
    })
    .unwrap();
    lib.flush().unwrap();
    let history = lib.history(Page::new(0, 10)).unwrap();
    assert_eq!((history.items.len(), history.total), (2, 2));
    let (newest, oldest) = (&history.items[0], &history.items[1]);
    assert_eq!(newest.event.ended_reason, EndedReason::Skipped);
    assert_eq!(newest.event.origin, "agent:test");
    assert_eq!(oldest.event.context.as_deref(), Some(r#"{"album":"x"}"#));
    assert!(newest.id > oldest.id);
    let page = lib.history(Page::new(1, 10)).unwrap();
    assert_eq!((page.items.len(), page.total), (1, 2));
    assert_eq!(page.items[0].id, oldest.id);

    // Soft removal: rows stay, tracks become unavailable, history is kept.
    lib.remove_root(f.root, UI).unwrap();
    assert!(lib.roots().unwrap().is_empty());
    assert!(!lib.track(id).unwrap().unwrap().available);
    assert!(lib.search("周砚舟", 10).unwrap().tracks.is_empty());
    assert_eq!(
        lib.albums(AlbumSort::Title, Page::new(0, 1)).unwrap().total,
        0
    );
    assert_eq!(lib.artists(Page::new(0, 1)).unwrap().total, 0);
    assert_eq!(lib.history(Page::new(0, 10)).unwrap().total, 2);
    // Removing it again changes nothing; an unknown root is not found.
    assert_eq!(lib.remove_root(f.root, UI).unwrap(), Done::unchanged(()));
    assert_eq!(lib.root_removed(f.root).unwrap(), Some(true));
    assert_eq!(lib.root_removed(RootId(424_242)).unwrap(), None);
    let err = lib.remove_root(RootId(424_242), UI).unwrap_err();
    assert_eq!(err.code(), "root_not_found", "{err}");
    let err = lib.set_root_encoding(f.root, None).unwrap_err();
    assert_eq!(err.code(), "root_not_found", "{err}");
    // A scan batch for a root removed meanwhile is dropped.
    upsert(lib, vec![record(f.root, "late.flac", meta("Late", "", ""))]);
    assert!(lib.search("late", 10).unwrap().tracks.is_empty());

    // Re-adding revives the same root; the next scan makes its files available again.
    assert_eq!(lib.add_root(&music, UI).unwrap().value, f.root);
    assert_eq!(lib.roots().unwrap()[0].track_count, 0);
    write(
        lib,
        f.root,
        ScanBatch {
            available: vec![id],
            ..ScanBatch::default()
        },
    );
    assert_eq!(lib.roots().unwrap()[0].track_count, 1);
    assert_eq!(id_of(lib, "晶港"), id);

    // Revived, it rejects nested roots again.
    assert!(lib.add_root(&nested, UI).is_err());
}

#[test]
fn root_paths_are_canonical_for_overlap_and_revival() {
    let f = fixture();
    let lib = &f.lib;
    let music = lib.roots().unwrap()[0].path.clone();
    let base = music.parent().unwrap().to_owned();
    std::fs::create_dir_all(music.join("sub")).unwrap();
    std::fs::create_dir(base.join("other")).unwrap();

    // `..` neither hides an overlap nor creates a second root for the same folder.
    assert!(matches!(
        lib.add_root(&base.join("other/../music/sub"), UI),
        Err(crate::LibraryError::OverlappingRoot { .. })
    ));
    assert_eq!(
        lib.add_root(&music.join("sub/.."), UI).unwrap().value,
        f.root
    );
    assert_eq!(lib.roots().unwrap().len(), 1);

    // Reviving a removed root is checked against the active roots too.
    lib.remove_root(f.root, UI).unwrap();
    let sub = lib.add_root(&music.join("sub"), UI).unwrap().value;
    assert!(matches!(
        lib.add_root(&music, UI),
        Err(crate::LibraryError::OverlappingRoot { .. })
    ));
    lib.remove_root(sub, UI).unwrap();
    assert_eq!(
        lib.add_root(&base.join("other/../music"), UI)
            .unwrap()
            .value,
        f.root
    );
}

#[test]
fn scan_thread_spawn_failure_is_an_error() {
    let f = fixture();
    let lib = &f.lib;
    lib.inner()
        .hooks
        .fail_spawn
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(matches!(lib.scan(|_| {}), Err(crate::LibraryError::Io(_))));
    lib.inner()
        .hooks
        .fail_spawn
        .store(false, std::sync::atomic::Ordering::Relaxed);
    // The failed attempt left no scan marked as running.
    match lib.scan(|_| {}).unwrap() {
        crate::ScanStart::Started(handle) => {
            handle.wait().unwrap();
        }
        crate::ScanStart::Coalesced(_) => panic!("no scan was running"),
    }
}

#[test]
fn multi_statement_reads_use_one_snapshot() {
    let f = fixture();
    sample(&f);
    let lib = &f.lib;
    let key = AlbumKey::new("周砚舟", "叶窗集");
    // A scan batch lands between the statements of each read.
    let writer = lib.clone();
    let root = f.root;
    let n = std::sync::atomic::AtomicUsize::new(0);
    *lib.inner().hooks.between_reads.lock().unwrap() = Some(Box::new(move || {
        let i = n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        upsert(
            &writer,
            vec![record(
                root,
                &format!("jay/extra{i}.flac"),
                meta(&format!("Extra {i}"), "周砚舟", "叶窗集"),
            )],
        );
    }));
    let album = lib.album(&key).unwrap().unwrap();
    assert_eq!(album.summary.track_count as usize, album.tracks.len());
    let results = lib.search("叶窗集", 50).unwrap();
    assert_eq!(results.albums[0].track_count as usize, results.tracks.len());
    // Drop the hook (it holds a Library clone).
    *lib.inner().hooks.between_reads.lock().unwrap() = None;
    assert_eq!(lib.album(&key).unwrap().unwrap().tracks.len(), 5);

    // Pages and totals: a new album lands between the page and its count.
    let writer = lib.clone();
    let n = std::sync::atomic::AtomicUsize::new(0);
    *lib.inner().hooks.between_reads.lock().unwrap() = Some(Box::new(move || {
        let i = n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        upsert(
            &writer,
            vec![record(
                root,
                &format!("new/{i}.flac"),
                meta(&format!("Fresh {i}"), "Newcomer", &format!("Debut {i}")),
            )],
        );
    }));
    let page = lib.albums(AlbumSort::Title, Page::new(0, 1000)).unwrap();
    assert_eq!(page.total, page.items.len() as u64);
    let results = lib.search("fresh", 100).unwrap();
    assert_eq!(results.tracks_total, results.tracks.len() as u64);
    *lib.inner().hooks.between_reads.lock().unwrap() = None;
    assert_eq!(lib.search("fresh", 100).unwrap().tracks_total, 2);
}

#[test]
fn reopen_keeps_data_and_schema() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("library.sqlite");
    let music = dir.path().join("m");
    std::fs::create_dir(&music).unwrap();
    {
        let lib = Library::open(&db).unwrap();
        let root = lib.add_root(&music, UI).unwrap().value;
        upsert(&lib, vec![record(root, "a.flac", meta("Kept", "", ""))]);
    }
    let lib = Library::open(&db).unwrap();
    assert_eq!(titles(&lib.search("kept", 10).unwrap()), ["Kept"]);

    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.pragma_update(None, "user_version", 99).unwrap();
    drop(conn);
    drop(lib);
    assert!(matches!(
        Library::open(&db),
        Err(crate::LibraryError::SchemaTooNew { found: 99, .. })
    ));
}

/// A damaged database file says so (`library_damaged`), apart from other database errors:
/// the app replaces it, nothing retries.
#[test]
fn damaged_databases_are_told_apart() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("library.sqlite");
    std::fs::write(&db, vec![0x5a; 8192]).unwrap();
    let err = Library::open(&db).err().expect("not a database");
    assert!(err.is_damaged(), "{err:?}");
    assert_eq!(err.code(), "library_damaged");
    let failure = |code| {
        crate::LibraryError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            None,
        ))
    };
    assert_eq!(
        failure(rusqlite::ffi::SQLITE_CORRUPT).code(),
        "library_damaged"
    );
    assert_eq!(
        failure(rusqlite::ffi::SQLITE_NOTADB).code(),
        "library_damaged"
    );
    assert_eq!(failure(rusqlite::ffi::SQLITE_BUSY).code(), "db");
    assert!(!failure(rusqlite::ffi::SQLITE_BUSY).is_damaged());
}

/// A database that turns out damaged while the library is open is noted by whatever finds
/// it first, also a write whose failure is only logged (a play event), and by reads:
/// [`Library::damaged`] then says so until the library is reopened.
#[test]
fn damage_found_by_any_read_or_write_is_noted() {
    let f = fixture();
    upsert(&f.lib, vec![record(f.root, "a.flac", meta("Kept", "", ""))]);
    let t = id_of(&f.lib, "Kept");
    assert_eq!(f.lib.damaged(), None);
    // Ordinary failures are not damage.
    let busy: crate::Result<()> = f.lib.inner().pool.read(|_| {
        Err(crate::LibraryError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        )))
    });
    assert!(busy.is_err());
    assert_eq!(f.lib.damaged(), None);
    // Damage the schema behind the library's back: every connection that reads it again
    // finds the database malformed.
    let db = f.lib.inner().pool.path().to_owned();
    let raw = rusqlite::Connection::open(&db).unwrap();
    let version: i64 = raw
        .query_row("PRAGMA schema_version", [], |r| r.get(0))
        .unwrap();
    raw.execute_batch(&format!(
        "PRAGMA writable_schema = ON;
         INSERT INTO sqlite_master VALUES ('table', 'bogus', 'bogus', 0, 'CREATE TABLE bogus (');
         PRAGMA schema_version = {};",
        version + 1
    ))
    .unwrap();
    drop(raw);
    // A play event fails on the writer thread, where it is only logged.
    f.lib
        .record_play(PlayEvent {
            track_id: t,
            context: None,
            started_at: 1,
            played_ms: 1,
            ended_reason: EndedReason::Completed,
            origin: "ui".into(),
        })
        .unwrap();
    f.lib.flush().unwrap();
    let damaged = f.lib.damaged().expect("noted by the play event's write");
    assert!(damaged.contains("malformed"), "{damaged}");
    // A read finds it too, and says so by its code.
    let err = f.lib.roots().unwrap_err();
    assert_eq!(err.code(), "library_damaged", "{err}");
}

#[test]
fn reads_do_not_wait_for_a_write_transaction() {
    let f = fixture();
    sample(&f);
    let lib = &f.lib;
    // Hold a write transaction open on another connection, as a long scan batch would.
    let db = lib.inner().pool.path().to_owned();
    let blocker = rusqlite::Connection::open(&db).unwrap();
    blocker
        .execute_batch("BEGIN IMMEDIATE; UPDATE tracks SET size = 2;")
        .unwrap();
    let start = Instant::now();
    assert_eq!(titles(&lib.search("周砚舟", 10).unwrap()).len(), 3);
    assert!(start.elapsed() < Duration::from_secs(1));
    blocker.execute_batch("ROLLBACK").unwrap();
}

/// Exercises the search query shapes on a mid-sized library; cheap enough for every run.
#[test]
fn search_latency_10k() {
    search_latency(10_000);
}

/// Search latency target: search < 50 ms at 100k tracks. Building the rows takes ~12 s in a debug
/// build, so this runs on request: `cargo test -p mpmp-library --release -- --ignored scale`.
#[test]
#[ignore = "builds 100k rows (~12 s in debug); run with --ignored, preferably --release"]
fn scale_100k_search_latency() {
    let worst = search_latency(100_000);
    let budget = if cfg!(debug_assertions) {
        Duration::from_millis(100)
    } else {
        Duration::from_millis(50)
    };
    assert!(worst < budget, "search took {worst:?} (budget {budget:?})");
}

/// A scan record of a file the library knew replaces its row only if the row is still as
/// the scan found it: a row a tag job updated meanwhile keeps what the job wrote.
#[test]
fn scan_records_never_overwrite_a_row_changed_meanwhile() {
    let f = fixture();
    upsert(
        &f.lib,
        vec![record(f.root, "a.flac", meta("Old", "A", "X"))],
    );
    let stale = TrackRecord {
        based_on: Some((7, 7)),
        based_on_revision: None,
        artists_only: false,
        file_identity: None,
        ..record(f.root, "a.flac", meta("Stale", "A", "X"))
    };
    upsert(&f.lib, vec![stale]);
    let id = id_of(&f.lib, "Old");
    let fresh = TrackRecord {
        based_on: Some((1, 1)),
        based_on_revision: None,
        artists_only: false,
        file_identity: None,
        ..record(f.root, "a.flac", meta("New", "A", "X"))
    };
    upsert(&f.lib, vec![fresh]);
    assert_eq!(id_of(&f.lib, "New"), id);
}

#[test]
fn group_artist_queries_use_persisted_album_grouping() {
    use crate::{CompareOp, Field, Filter, TextOp, TrackQuery, Value};
    let f = fixture();
    let mut tagged = meta("Tagged", "Guest", "One");
    tagged.album_artist = Some("Énsemble".into());
    tagged.compilation = true;
    let mut compilation = meta("Compilation", "Guest", "Two");
    compilation.compilation = true;
    upsert(
        &f.lib,
        vec![
            record(f.root, "a.flac", tagged),
            record(f.root, "b.flac", compilation),
            record(f.root, "c.flac", meta("Solo", "Guest", "Three")),
            record(f.root, "d.flac", TrackMeta::default()),
        ],
    );
    for (name, title) in [
        ("ensemble", "Tagged"),
        ("VARIOUS ARTISTS", "Compilation"),
        ("guest", "Solo"),
    ] {
        for filter in [
            Filter::Text {
                field: Field::GroupArtist,
                op: TextOp::Equals,
                text: name.into(),
            },
            Filter::Compare {
                field: Field::GroupArtist,
                op: CompareOp::Eq,
                value: Value::Text(name.into()),
            },
        ] {
            let (rows, total) = f
                .lib
                .tracks_query(
                    &TrackQuery {
                        filter,
                        sort: vec![],
                    },
                    Page::new(0, 10),
                    &[],
                )
                .unwrap();
            assert_eq!(total, 1);
            assert_eq!(rows[0].track.display_title(), title);
            assert!(rows[0].track.group_artist.is_some());
        }
    }
    let (facets, total) = f
        .lib
        .facet(&Field::GroupArtist, &Filter::All, Page::new(0, 10))
        .unwrap();
    assert_eq!(total, 4);
    assert_eq!(facets.last().unwrap().value, None);
    let (rows, total) = f
        .lib
        .tracks_query(
            &TrackQuery {
                filter: Filter::Missing {
                    field: Field::GroupArtist,
                },
                sort: vec![],
            },
            Page::new(0, 10),
            &[],
        )
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(rows[0].track.group_artist, None);
}

#[test]
fn album_descending_orders_keep_missing_last_and_pages_stable() {
    let f = fixture();
    let rows = [
        ("a", "Alpha", "A", Some(1990)),
        ("b", "Beta", "B", Some(2025)),
        ("c", "Gamma", "A", Some(2025)),
        ("d", "", "Z", None),
    ];
    upsert(
        &f.lib,
        rows.into_iter()
            .map(|(path, title, artist, year)| {
                let mut m = meta(path, artist, title);
                m.year = year;
                record(f.root, path, m)
            })
            .collect(),
    );
    for (sort, expected) in [
        (AlbumSort::TitleDesc, vec!["Gamma", "Beta", "Alpha", ""]),
        (AlbumSort::ArtistDesc, vec!["", "Beta", "Alpha", "Gamma"]),
        (AlbumSort::YearDesc, vec!["Gamma", "Beta", "Alpha", ""]),
        (AlbumSort::OldestAdded, vec!["Alpha", "Gamma", "Beta", ""]),
    ] {
        let all = f.lib.albums(sort, Page::new(0, 100)).unwrap();
        assert_eq!(
            all.items
                .iter()
                .map(|a| a.title.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        for (i, album) in all.items.iter().enumerate() {
            let page = f.lib.albums(sort, Page::new(i as u32, 1)).unwrap();
            assert_eq!(page.total, 4);
            assert_eq!(&page.items[0], album);
        }
        let a = f
            .lib
            .albums_by_artist("A", sort, Page::new(0, 100))
            .unwrap();
        assert_eq!(a.total, 2);
        assert_eq!(
            a.items,
            all.items
                .into_iter()
                .filter(|a| a.artist.as_deref() == Some("A"))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn album_and_search_pages_are_complete_stable_and_count_at_zero_limit() {
    use crate::SearchKind;
    let f = fixture();
    let rows = (0..12)
        .map(|i| {
            // Reverse title order relative to IDs, with identical FTS scores in each phase.
            let mut m = meta(
                if i % 2 == 0 { "match zz" } else { "match aa" },
                "Artist",
                "Album",
            );
            m.disc_no = Some(1 + i / 6);
            m.track_no = Some(6 - i % 6);
            record(f.root, &format!("{i:02}.flac"), m)
        })
        .collect();
    upsert(&f.lib, rows);
    let key = AlbumKey::new("Artist", "Album");
    let all = f.lib.album(&key).unwrap().unwrap();
    let mut ids = Vec::new();
    for offset in (0..12).step_by(2) {
        let page = f
            .lib
            .album_page(&key, Page::new(offset, 2))
            .unwrap()
            .unwrap();
        assert_eq!(page.summary.track_count, 12);
        ids.extend(page.tracks.iter().map(|t| t.id));
    }
    assert_eq!(ids, all.tracks.iter().map(|t| t.id).collect::<Vec<_>>());
    assert!(
        f.lib
            .album_page(&key, Page::new(0, 0))
            .unwrap()
            .unwrap()
            .tracks
            .is_empty()
    );
    for text in ["ma", "match", "match ar"] {
        let all = f.lib.search(text, 100).unwrap();
        assert_eq!(all.tracks_total, 12);
        if text == "match" {
            let mut sorted = all.tracks.iter().map(|t| t.id).collect::<Vec<_>>();
            sorted.sort();
            assert_eq!(
                all.tracks.iter().map(|t| t.id).collect::<Vec<_>>(),
                sorted,
                "equal FTS scores use ID, not title or query plan order"
            );
        }
        let mut paged = Vec::new();
        for offset in (0..12).step_by(2) {
            let p = f
                .lib
                .search_page(text, Some(SearchKind::Tracks), Page::new(offset, 2))
                .unwrap();
            assert_eq!(p.tracks_total, 12);
            assert!(p.albums.is_empty() && p.artists.is_empty());
            paged.extend(p.tracks.iter().map(|t| t.id));
        }
        assert_eq!(paged, all.tracks.iter().map(|t| t.id).collect::<Vec<_>>());
        let count = f
            .lib
            .search_page(text, Some(SearchKind::Tracks), Page::new(0, 0))
            .unwrap();
        assert!(count.tracks.is_empty());
        assert_eq!(count.tracks_total, 12);
        assert!(
            f.lib
                .search_page(text, Some(SearchKind::Tracks), Page::new(u32::MAX, 2))
                .unwrap()
                .tracks
                .is_empty()
        );
    }
    for (kind, text, total) in [
        (SearchKind::Albums, "al", 1),
        (SearchKind::Artists, "ar", 1),
    ] {
        let p = f
            .lib
            .search_page(text, Some(kind), Page::new(0, 1))
            .unwrap();
        assert_eq!(
            if kind == SearchKind::Albums {
                p.albums_total
            } else {
                p.artists_total
            },
            total
        );
        assert!(p.tracks.is_empty());
    }
}

#[test]
fn released_roots_are_session_local_and_m3u_access_obeys_the_barrier() {
    let f = fixture();
    let root = f.lib.roots().unwrap()[0].path.clone();
    let playlist = root.join("list.m3u8");
    std::fs::write(&playlist, "#EXTM3U\n").unwrap();
    assert!(f.lib.release_root(f.root).unwrap());
    assert!(!f.lib.release_root(f.root).unwrap());
    assert_eq!(
        f.lib.import_m3u(&playlist, None, UI).unwrap_err().code(),
        "root_released"
    );
    let reopened = Library::open(&f._dir.path().join("db/library.sqlite")).unwrap();
    assert!(!reopened.root_released(f.root));
    assert!(f.lib.resume_root(f.root).unwrap());
    assert!(f.lib.import_m3u(&playlist, None, UI).is_ok());
    f.lib.release_root(f.root).unwrap();
    f.lib.remove_root(f.root, UI).unwrap();
    // Removal explicitly ends the session-local suspension; revival starts enabled.
    assert!(!f.lib.root_released(f.root));
    assert_eq!(
        f.lib.release_root(f.root).unwrap_err().code(),
        "root_not_found"
    );
    assert_eq!(
        f.lib.resume_root(RootId(999)).unwrap_err().code(),
        "root_not_found"
    );
}

#[test]
fn released_scan_abandons_root_without_missing_rows_and_does_not_lease_later_roots() {
    use std::sync::{Arc, Mutex};
    let f = fixture();
    let root = f.lib.roots().unwrap()[0].path.clone();
    let audio =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_a.flac");
    for id in 0..8 {
        std::fs::copy(&audio, root.join(format!("{id}.flac"))).unwrap();
    }
    write(
        &f.lib,
        f.root,
        ScanBatch {
            upserts: vec![record(f.root, "gone.flac", meta("kept", "A", "B"))],
            ..Default::default()
        },
    );
    let known = f.lib.track_ids(&Default::default(), 100).unwrap()[0];
    let later = f._dir.path().join("later");
    std::fs::create_dir(&later).unwrap();
    let later = f.lib.add_root(&later, UI).unwrap().value;
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (resume_tx, resume) = std::sync::mpsc::channel();
    let resume = Mutex::new(resume);
    let target = f.root;
    *f.lib.inner().hooks.scan_file.lock().unwrap() = Some(Arc::new(move |root, _| {
        assert_eq!(root, target);
        entered_tx.send(()).unwrap();
        resume
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
    }));
    let crate::ScanStart::Started(scan) = f.lib.scan(|_| {}).unwrap() else {
        panic!()
    };
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    let at = Instant::now();
    assert!(f.lib.release_root(later).unwrap());
    assert!(
        at.elapsed() < Duration::from_secs(1),
        "later root must not hold a queued lease"
    );
    f.lib.suspend_root(f.root).unwrap();
    resume_tx.send(()).unwrap();
    let report = scan.wait().unwrap();
    assert_eq!(report.removed, 0);
    assert!(!report.cancelled);
    assert!(f.lib.track(known).unwrap().unwrap().available);
    assert!(
        f.lib
            .wait_root_idle(f.root, Duration::from_secs(1))
            .unwrap()
    );
}

#[test]
fn scoped_scans_merge_requested_roots_without_visiting_other_roots() {
    coalesced_scope_scenario(false);
}

#[test]
fn scoped_request_after_full_scan_does_not_repeat_unrelated_roots() {
    coalesced_scope_scenario(true);
}

#[test]
fn notification_during_walk_is_seen_by_scoped_followup_without_inflating_counts() {
    use std::sync::{Arc, Mutex};
    let f = fixture();
    let root = f.lib.roots().unwrap()[0].path.clone();
    let audio =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_a.flac");
    std::fs::copy(&audio, root.join("first.flac")).unwrap();
    std::fs::write(root.join("broken.flac"), b"broken").unwrap();
    // CUE, folders and non-audio files do not add to scanned.
    std::fs::create_dir(root.join("folder")).unwrap();
    std::fs::write(root.join("notes.txt"), b"notes").unwrap();
    std::fs::write(root.join("empty.cue"), b"REM empty").unwrap();
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (resume_tx, resume) = std::sync::mpsc::channel();
    let gate = Mutex::new(Some(resume));
    *f.lib.inner().hooks.scan_file.lock().unwrap() = Some(Arc::new(move |_, _| {
        if let Some(resume) = gate.lock().unwrap().take() {
            entered_tx.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(5)).unwrap();
        }
    }));
    let crate::ScanStart::Started(scan) = f.lib.scan_roots(Some(vec![f.root]), |_| {}).unwrap()
    else {
        panic!()
    };
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    // The first walk already completed, so only a fresh pass can find this write.
    std::fs::copy(audio, root.join("late.flac")).unwrap();
    assert!(matches!(f.lib.scan_roots(Some(vec![f.root]),
         |_| {
        }
        ).unwrap(),
         crate::ScanStart::Coalesced(id)
        if id == scan.id()));
    resume_tx.send(()).unwrap();
    let report = scan.wait().unwrap();
    assert_eq!((report.passes, report.coalesced), (2, 1));
    assert_eq!((report.scanned, report.added), (3, 2));
    assert_eq!(
        report
            .first_errors
            .iter()
            .filter(|e| e.path.ends_with("broken.flac"))
            .count(),
        1
    );
}
