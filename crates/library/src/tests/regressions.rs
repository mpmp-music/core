//! Missing-value query semantics and safe playlist import/export behavior.

use crate::tests::{UI, fixture, meta, record, upsert};
use crate::track_query::{CompareOp, Field, Filter, SortKey, TrackQuery, Value};
use crate::{LibraryError, M3U_MISSING_LIMIT, Page, PlaylistKind, normalize};

const ALL: Page = Page::new(0, 1000);

/// Missing values sort last in either direction. Genre/Artist/Album/AlbumArtist keep
/// '' (not NULL) in their *_norm columns; an untagged track must still sort last.
#[test]
fn review_missing_genre_sorts_last_ascending() {
    let f = fixture();
    let mut jazz = meta("Jazz track", "A", "X");
    jazz.genre = Some("Jazz".into());
    let untagged = meta("No genre", "B", "Y");
    upsert(
        &f.lib,
        vec![
            record(f.root, "a.flac", jazz),
            record(f.root, "b.flac", untagged),
        ],
    );
    for descending in [false, true] {
        let q = TrackQuery {
            filter: Filter::All,
            sort: vec![SortKey {
                field: Field::Genre,
                descending,
            }],
        };
        let (rows, _) = f.lib.tracks_query(&q, ALL, &[]).unwrap();
        let order: Vec<_> = rows
            .iter()
            .map(|r| r.track.title.clone().unwrap())
            .collect();
        assert_eq!(
            order,
            ["Jazz track", "No genre"],
            "missing genre must sort last"
        );
    }
}

/// Missing values fail every comparison except `ne`.
#[test]
fn review_missing_genre_does_not_satisfy_lt() {
    let f = fixture();
    let mut alt = meta("Alt", "A", "X");
    alt.genre = Some("Alternative".into());
    let untagged = meta("No genre", "B", "Y");
    upsert(
        &f.lib,
        vec![
            record(f.root, "a.flac", alt),
            record(f.root, "b.flac", untagged),
        ],
    );
    let titles = |filter: Filter| -> Vec<String> {
        let q = TrackQuery {
            filter,
            sort: vec![],
        };
        let (rows, _) = f.lib.tracks_query(&q, ALL, &[]).unwrap();
        rows.iter()
            .map(|r| r.track.title.clone().unwrap())
            .collect()
    };
    let lt = Filter::Compare {
        field: Field::Genre,
        op: CompareOp::Lt,
        value: Value::Text("b".into()),
    };
    assert_eq!(
        titles(lt.clone()),
        ["Alt"],
        "a track without a genre satisfied genre < 'b'"
    );
    // Two-valued NOT: the missing value is in the complement.
    assert_eq!(
        titles(Filter::Not {
            filter: Box::new(lt)
        }),
        ["No genre"]
    );
    assert_eq!(
        titles(Filter::Compare {
            field: Field::Genre,
            op: CompareOp::Ne,
            value: Value::Text("alternative".into()),
        }),
        ["No genre"]
    );
}

/// `normalize` lower-cased only before NFKD, so compatibility characters that decompose to
/// capitals stayed capital in the search key.
#[test]
fn review_normalize_lowercases_compatibility_decompositions() {
    assert_eq!(normalize("Song™"), normalize("song tm").replace(' ', ""));
    assert_eq!(normalize("№5"), "no5");
    assert_eq!(normalize("20℃"), "20°c");
}

/// A relative path line starting with `#` used to be read back as a comment: the entry
/// silently disappeared on re-import.
#[test]
fn review_m3u_round_trip_of_hash_file_name() {
    let f = fixture();
    let music = crate::storage_access::canonical_dir(&f._dir.path().join("music")).unwrap();
    std::fs::write(music.join("#1 hit.flac"), b"").unwrap();
    upsert(
        &f.lib,
        vec![record(f.root, "#1 hit.flac", meta("Hit", "A", "X"))],
    );
    let id = crate::tests::id_of(&f.lib, "Hit");
    let p = f
        .lib
        .create_playlist("p", PlaylistKind::Manual, UI)
        .unwrap()
        .value;
    f.lib.add_to_playlist(p, &[id], None, None, UI).unwrap();
    let path = music.join("list.m3u8");
    f.lib.export_m3u(p, &path, false, false, UI).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        text.lines().nth(2),
        Some(format!(".{}#1 hit.flac", std::path::MAIN_SEPARATOR).as_str())
    );
    let (_, report) = f.lib.import_m3u(&path, None, UI).unwrap().value;
    assert_eq!(
        (report.matched, report.missing.len(), report.missing_total),
        (1, 0, 0),
        "exported entry lost on re-import: {report:?}\n{text}"
    );
}

/// ImportM3u used to read any file and return every non-'#' line in `missing`: an
/// arbitrary-file read for any client. Only M3U files are read now, and the report lists
/// at most [`M3U_MISSING_LIMIT`] lines.
#[test]
fn review_import_m3u_echoes_any_file() {
    let f = fixture();
    let secret = f._dir.path().join("secret.txt");
    std::fs::write(&secret, "user=me\npassword=hunter2\n").unwrap();
    match f.lib.import_m3u(&secret, None, UI) {
        Err(LibraryError::Invalid { message, .. }) => assert!(!message.contains("hunter2")),
        other => panic!("a non-M3U file was read: {other:?}"),
    }
    assert_eq!(f.lib.playlists(false, ALL).unwrap().1, 0);

    // Letter case of the extension does not matter; the report is capped.
    let list = f._dir.path().join("big.M3U");
    let lines: Vec<String> = (0..M3U_MISSING_LIMIT + 50)
        .map(|i| format!("nowhere/{i}.flac"))
        .collect();
    std::fs::write(&list, lines.join("\n")).unwrap();
    let (_, report) = f.lib.import_m3u(&list, None, UI).unwrap().value;
    assert_eq!(report.matched, 0);
    assert_eq!(report.missing.len(), M3U_MISSING_LIMIT);
    assert_eq!(report.missing_total, (M3U_MISSING_LIMIT + 50) as u64);
    assert_eq!(report.missing[0], "nowhere/0.flac");
}

/// ExportM3u used to overwrite any existing file, including a library track.
#[test]
fn review_export_m3u_overwrites_existing_files() {
    let f = fixture();
    let music = crate::storage_access::canonical_dir(&f._dir.path().join("music")).unwrap();
    std::fs::write(music.join("song.flac"), b"fLaC-audio-bytes").unwrap();
    upsert(
        &f.lib,
        vec![record(f.root, "song.flac", meta("Song", "A", "X"))],
    );
    let p = f
        .lib
        .create_playlist("p", PlaylistKind::Manual, UI)
        .unwrap()
        .value;
    for overwrite in [false, true] {
        let result = f
            .lib
            .export_m3u(p, &music.join("song.flac"), overwrite, false, UI);
        let after = std::fs::read(music.join("song.flac")).unwrap();
        assert!(
            matches!(result, Err(LibraryError::Invalid { .. })) && after == b"fLaC-audio-bytes",
            "export clobbered an existing (music) file: now {:?}",
            String::from_utf8_lossy(&after)
        );
    }
    // A folder or a symlink named like a playlist is not replaced either.
    std::fs::create_dir(music.join("dir.m3u")).unwrap();
    assert!(matches!(
        f.lib.export_m3u(p, &music.join("dir.m3u"), true, false, UI),
        Err(LibraryError::Invalid { .. })
    ));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(music.join("song.flac"), music.join("link.m3u")).unwrap();
        assert!(matches!(
            f.lib
                .export_m3u(p, &music.join("link.m3u"), true, false, UI),
            Err(LibraryError::Invalid { .. })
        ));
        assert_eq!(
            std::fs::read(music.join("song.flac")).unwrap(),
            b"fLaC-audio-bytes"
        );
    }
}

/// `Library::track_rows` with a repeated id: the second row lost its tags.
#[test]
fn review_track_rows_repeated_id_keeps_tags() {
    let f = fixture();
    let mut m = meta("T", "A", "X");
    m.tags = vec![("COMPOSER".into(), "Bavel".into())];
    upsert(&f.lib, vec![record(f.root, "t.flac", m)]);
    let id = crate::tests::id_of(&f.lib, "T");
    let rows = f.lib.track_rows(&[id, id], &["composer".into()]).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].tags, rows[1].tags,
        "second row of a repeated id lost its tags"
    );
    assert_eq!(rows[0].tags["COMPOSER"], ["Bavel"]);
}

/// Perf probe (prints, no assertion): listing playlists and paging a smart playlist at
/// 100k tracks, on a fresh database (no statistics) and after reopening it (statistics
/// computed on open).
/// `cargo test -p mpmp-library --release --lib -- --ignored review_scale --nocapture`.
#[test]
#[ignore = "builds 100k rows"]
fn review_scale_smart_playlist_listing() {
    use crate::tests::{LATIN, Lcg, synthetic};
    use crate::track_query::TextOp;
    use std::time::Instant;
    let f = fixture();
    let mut rng = Lcg(7);
    let artists: Vec<String> = (0..2_000)
        .map(|i| format!("{} {i}", rng.words(LATIN, 2, " ")))
        .collect();
    let mut batch = Vec::with_capacity(500);
    for i in 0..100_000 {
        let mut r = synthetic(&mut rng, f.root, i, &artists);
        r.meta.tags = vec![
            ("COMPOSER".into(), format!("Composer {}", (i * 13) % 500)),
            ("LABEL".into(), format!("Label {}", i % 50)),
        ];
        batch.push(r);
        if batch.len() == 500 {
            upsert(&f.lib, std::mem::take(&mut batch));
        }
    }
    upsert(&f.lib, batch);
    let by_tag = |key: &str| SortKey {
        field: Field::Tag { key: key.into() },
        descending: false,
    };
    let mut ids = Vec::new();
    for i in 0..10 {
        let query = TrackQuery {
            filter: if i % 2 == 0 {
                Filter::All
            } else {
                Filter::Text {
                    field: Field::Genre,
                    op: TextOp::Contains,
                    text: "o".into(),
                }
            },
            sort: vec![by_tag(if i % 2 == 0 { "COMPOSER" } else { "LABEL" })],
        };
        let kind = PlaylistKind::Smart {
            query,
            max_tracks: Some(500 + i),
        };
        ids.push(
            f.lib
                .create_playlist(&format!("smart {i}"), kind, UI)
                .unwrap()
                .value,
        );
    }
    let probe = |lib: &crate::Library, label: &str| {
        let t = Instant::now();
        let (list, _) = lib.playlists(false, Page::new(0, 50)).unwrap();
        eprintln!(
            "{label}: playlists() with {} smart lists: {:.2?}",
            list.len(),
            t.elapsed()
        );
        let t = Instant::now();
        let (list, _) = lib.playlists(false, Page::new(0, 50)).unwrap();
        eprintln!(
            "{label}: playlists() again (cached): {:.2?} ({})",
            t.elapsed(),
            list.len()
        );
        let t = Instant::now();
        lib.playlist(ids[0]).unwrap();
        lib.playlist_items(ids[0], Page::new(0, 50), &[]).unwrap();
        eprintln!("{label}: summary + PlaylistItems page: {:.2?}", t.elapsed());
        let t = Instant::now();
        lib.playlist_page(ids[0], Page::new(50, 50), &[]).unwrap();
        eprintln!(
            "{label}: playlist_page (summary + page): {:.2?}",
            t.elapsed()
        );
        // A write invalidates the counts.
        lib.set_loved(&[crate::TrackId(1)], true, UI).unwrap();
        let t = Instant::now();
        lib.playlists(false, Page::new(0, 50)).unwrap();
        eprintln!("{label}: playlists() after a write: {:.2?}", t.elapsed());
        let t = Instant::now();
        let q = TrackQuery {
            filter: Filter::All,
            sort: vec![by_tag("COMPOSER")],
        };
        lib.tracks_query(&q, Page::new(0, 50), &[]).unwrap();
        eprintln!(
            "{label}: one tag-sorted page of all tracks: {:.2?}",
            t.elapsed()
        );
    };
    probe(&f.lib, "fresh, no statistics");
    let path = f.lib.inner().pool.path().to_owned();
    let crate::tests::Fixture { lib, _dir, .. } = f;
    drop(lib);
    let t = Instant::now();
    let lib = crate::Library::open(&path).unwrap();
    eprintln!("reopen (computes statistics): {:.2?}", t.elapsed());
    probe(&lib, "after reopen");
}

/// The scanner follows symlinks, so a track can live at `root/linked/x.flac` with `linked`
/// a symlink to a folder elsewhere; M3U import must match that path, not only the
/// canonical one (outside the root).
#[cfg(unix)]
#[test]
fn review_m3u_matches_tracks_under_symlinked_folders() {
    let f = fixture();
    let music = crate::storage_access::canonical_dir(&f._dir.path().join("music")).unwrap();
    let elsewhere = f._dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("x.flac"), b"").unwrap();
    std::os::unix::fs::symlink(&elsewhere, music.join("linked")).unwrap();
    upsert(
        &f.lib,
        vec![record(f.root, "linked/x.flac", meta("Linked", "A", "X"))],
    );
    let path = music.join("list.m3u8");
    std::fs::write(&path, "linked/x.flac\n").unwrap();
    let (p, report) = f.lib.import_m3u(&path, None, UI).unwrap().value;
    assert_eq!(report.matched, 1, "{report:?}");
    // Our own export round-trips, also through an absolute path and a symlinked M3U
    // folder.
    let outside = f._dir.path().join("out.m3u");
    f.lib.export_m3u(p, &outside, false, false, UI).unwrap();
    assert_eq!(
        f.lib
            .import_m3u(&outside, None, UI)
            .unwrap()
            .value
            .1
            .matched,
        1
    );
    std::os::unix::fs::symlink(&music, f._dir.path().join("music-link")).unwrap();
    let via_link = f._dir.path().join("music-link/list.m3u8");
    assert_eq!(
        f.lib
            .import_m3u(&via_link, None, UI)
            .unwrap()
            .value
            .1
            .matched,
        1
    );
}

/// Planner statistics exist once the library has tracks: computed after a completed scan
/// and when a database without them is opened (a tag sort without them took minutes at
/// 100k tracks).
#[test]
fn statistics_are_computed_after_scans_and_on_open() {
    let has_stats = |lib: &crate::Library| -> bool {
        let conn = rusqlite::Connection::open(lib.inner().pool.path()).unwrap();
        conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'sqlite_stat1')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .unwrap()
            && conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM sqlite_stat1 WHERE tbl = 'tracks')",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
    };
    let f = fixture();
    assert!(!has_stats(&f.lib), "an empty library needs none");
    upsert(&f.lib, vec![record(f.root, "a.flac", meta("A", "B", "C"))]);
    assert!(!has_stats(&f.lib));
    let crate::ScanStart::Started(scan) = f.lib.scan(|_| {}).unwrap() else {
        panic!("no scan running")
    };
    scan.wait().unwrap();
    assert!(has_stats(&f.lib), "after a completed scan");

    let f = fixture();
    upsert(&f.lib, vec![record(f.root, "a.flac", meta("A", "B", "C"))]);
    let path = f.lib.inner().pool.path().to_owned();
    let crate::tests::Fixture { lib, _dir, .. } = f;
    drop(lib);
    let lib = crate::Library::open(&path).unwrap();
    assert!(has_stats(&lib), "on open");
}

/// Smart playlists' counts are cached, and every kind of library write invalidates them.
#[test]
fn smart_playlist_counts_follow_library_writes() {
    let f = fixture();
    let lib = &f.lib;
    upsert(
        lib,
        vec![
            record(f.root, "a.flac", meta("A", "X", "Y")),
            record(f.root, "b.flac", meta("B", "X", "Y")),
        ],
    );
    let a = crate::tests::id_of(lib, "A");
    let loved = Filter::Compare {
        field: Field::Loved,
        op: CompareOp::Eq,
        value: Value::Bool(true),
    };
    let played = Filter::Compare {
        field: Field::PlayCount,
        op: CompareOp::Ge,
        value: Value::Int(1),
    };
    let smart = |name: &str, filter: Filter| {
        lib.create_playlist(
            name,
            PlaylistKind::Smart {
                query: TrackQuery {
                    filter,
                    sort: vec![],
                },
                max_tracks: None,
            },
            UI,
        )
        .unwrap()
        .value
    };
    let all = smart("all", Filter::All);
    let loved_list = smart("loved", loved);
    let played_list = smart("played", played);
    let count = |id| lib.playlist(id).unwrap().unwrap().track_count;
    let listed = || -> Vec<u64> {
        lib.playlists(false, ALL)
            .unwrap()
            .0
            .iter()
            .map(|p| p.track_count)
            .collect()
    };
    assert_eq!(listed(), [2, 0, 0]);

    lib.set_loved(&[a], true, UI).unwrap();
    assert_eq!(count(loved_list), 1);
    upsert(lib, vec![record(f.root, "c.flac", meta("C", "X", "Y"))]);
    assert_eq!(count(all), 3);
    lib.record_play(crate::PlayEvent {
        track_id: a,
        context: None,
        started_at: 1,
        played_ms: 1_000_000,
        ended_reason: crate::EndedReason::Completed,
        origin: "test".into(),
    })
    .unwrap();
    lib.flush().unwrap();
    assert_eq!(count(played_list), 1);
    assert_eq!(listed(), [3, 1, 1]);
    // Editing the playlist (a new revision) recomputes it too.
    lib.set_smart_query(all, TrackQuery::default(), Some(1), None, UI)
        .unwrap();
    assert_eq!(count(all), 1);
    let (summary, entries) = lib.playlist_page(all, ALL, &[]).unwrap().unwrap();
    assert_eq!((summary.track_count, entries.len()), (1, 1));
    lib.remove_root(f.root, UI).unwrap();
    assert_eq!(listed(), [0, 0, 0]);
    assert!(
        lib.playlist_page(crate::PlaylistId(999), ALL, &[])
            .unwrap()
            .is_none()
    );
}

/// A tag job finishing after a CUE scan must not unhide the original image row.
#[test]
fn tag_job_keeps_a_concurrently_hidden_image_hidden() {
    let f = fixture();
    upsert(
        &f.lib,
        vec![record(f.root, "image.flac", meta("Image", "A", "X"))],
    );
    f.lib
        .inner()
        .writer
        .run_data(|conn| {
            conn.execute("UPDATE tracks SET hidden = 1, available = 0", [])?;
            Ok(())
        })
        .unwrap();
    let mut updated = record(f.root, "image.flac", meta("Edited", "A", "X"));
    updated.size = 2;
    upsert(&f.lib, vec![updated]);
    let visible = f.lib.track_ids(&TrackQuery::default(), 10).unwrap();
    assert!(visible.is_empty());
    let (hidden, title): (bool, String) = f
        .lib
        .inner()
        .pool
        .read(|conn| {
            Ok(conn.query_row("SELECT hidden, title FROM tracks", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?)
        })
        .unwrap();
    assert!(hidden);
    assert_eq!(title, "Edited");
}
