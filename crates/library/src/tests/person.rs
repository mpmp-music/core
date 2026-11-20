//! Person index regression, migration safety and bounded backfill tests.

use rusqlite::{Connection, params};

use crate::person::ArtistList;
use crate::tests::{fixture, meta, record, upsert};
use crate::{ArtistListSource, ArtistView, Filter, Library, Page, TrackQuery, db::schema};

#[test]
fn person_membership_is_ordered_deduplicated_and_independent_of_grouping() {
    let f = fixture();
    let mut a = meta("One", "Display feat. Guest", "Compilation");
    a.compilation = true;
    a.tags = [
        ("ARTIST", "Ignored"),
        ("ARTISTS", "Björven"),
        ("ARTISTS", "BJORVEN"),
        ("ARTISTS", "AX/DZ; feat. X, Y"),
    ]
    .map(|(k, v)| (k.into(), v.into()))
    .to_vec();
    let mut b = meta("Two", "Other", "Compilation");
    b.compilation = true;
    b.tags = vec![("ARTIST".into(), "Björven".into())];
    upsert(
        &f.lib,
        vec![record(f.root, "one.flac", a), record(f.root, "two.flac", b)],
    );
    let old = f.lib.artists(Page::new(0, 500)).unwrap();
    assert_eq!(old.items.len(), 1);
    assert_eq!(old.items[0].name, "Various Artists");
    assert_eq!(old.items[0].key, None);
    let (people, indexing) = f.lib.person_artists(Page::new(0, 500)).unwrap();
    assert!(!indexing);
    assert_eq!(people.total, 2);
    let bjorven = people.items.iter().find(|p| p.name == "Björven").unwrap();
    assert_eq!((bjorven.track_count, bjorven.album_count), (2, 1));
    let found = f
        .lib
        .search_page_view("bjorven", None, Page::new(0, 500), ArtistView::Person)
        .unwrap();
    assert_eq!(found.artists_total, 1);
    assert_eq!(found.artists[0], *bjorven);
    let legacy = f.lib.search("bjorven", 500).unwrap();
    assert_eq!(legacy.artists_total, 0);
    assert_eq!(found.tracks, legacy.tracks);
    assert_eq!(found.albums, legacy.albums);
    assert_eq!(
        f.lib
            .albums_by_person("BJORVEN", crate::AlbumSort::Artist, Page::new(0, 500))
            .unwrap()
            .total,
        1
    );
    assert_eq!(
        f.lib
            .albums_by_artist("BJORVEN", crate::AlbumSort::Artist, Page::new(0, 500))
            .unwrap()
            .total,
        0
    );
    let query = TrackQuery {
        filter: Filter::ArtistPerson {
            name: "BJORVEN".into(),
        },
        ..Default::default()
    };
    let (rows, total) = f.lib.tracks_query(&query, Page::new(0, 500), &[]).unwrap();
    assert_eq!(total, 2);
    assert_eq!(rows[0].track.artist.as_deref(), Some("Display feat. Guest"));
    assert_eq!(rows[0].track.artists.len(), 2);
    assert_eq!(
        rows[0].track.artists_source,
        Some(ArtistListSource::ArtistsTag)
    );
    let before_key = rows[0].track.album_key.clone();
    let id = rows[0].track.id;
    crate::tests::write(
        &f.lib,
        f.root,
        crate::db::writer::ScanBatch {
            unavailable: vec![id],
            ..Default::default()
        },
    );
    let (people, _) = f.lib.person_artists(Page::new(0, 500)).unwrap();
    assert_eq!(people.total, 1);
    assert_eq!(people.items[0].track_count, 1);
    let track = f.lib.track(id).unwrap().unwrap();
    assert_eq!(track.artists.len(), 2);
    assert_eq!(track.album_key, before_key);
    let unavailable = TrackQuery {
        filter: Filter::And {
            all: vec![
                query.filter,
                Filter::Compare {
                    field: crate::Field::Available,
                    op: crate::CompareOp::Eq,
                    value: crate::Value::Bool(false),
                },
            ],
        },
        ..Default::default()
    };
    assert_eq!(
        f.lib
            .tracks_query(&unavailable, Page::new(0, 500), &[])
            .unwrap()
            .1,
        1
    );
    let not = TrackQuery {
        filter: Filter::Not {
            filter: Box::new(Filter::ArtistPerson {
                name: "BJORVEN".into(),
            }),
        },
        ..Default::default()
    };
    assert_eq!(
        f.lib.tracks_query(&not, Page::new(0, 500), &[]).unwrap().1,
        0
    );
}

#[test]
fn pending_parser_versions_resume_across_reopens_without_changing_stamps() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let audio = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../engine/tests/fixtures/sine44_a.flac");
    for name in ["one.flac", "two.flac"] {
        std::fs::copy(&audio, music.join(name)).unwrap();
    }
    let path = dir.path().join("db.sqlite");
    let lib = Library::open(&path).unwrap();
    let root = lib.add_root(&music, crate::tests::UI).unwrap().value;
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    let tracks = lib.search("flac", 20).unwrap().tracks;
    let _ = tracks;
    lib.inner()
        .writer
        .run(|conn| {
            conn.execute("UPDATE tracks SET artists_parser_version=0", [])?;
            Ok(())
        })
        .unwrap();
    let before = lib
        .inner()
        .pool
        .read(|conn| {
            Ok(conn
                .prepare("SELECT id,size,mtime_ms FROM tracks ORDER BY id")?
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap();
    // A successful first page is durable, while a failed second read remains pending.
    *lib.inner().hooks.fail_audio_read.lock().unwrap() =
        Some(crate::storage_access::comparable(&music.join("two.flac")));
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    assert_eq!(
        lib.inner()
            .pool
            .read(|c| Ok(c.query_row(
                "SELECT COUNT(*) FROM tracks WHERE artists_parser_version=?1",
                [crate::person::PARSER_VERSION],
                |r| r.get::<_, i64>(0)
            )?))
            .unwrap(),
        1
    );
    assert!(lib.artists_indexing().unwrap());
    lib.release_root(root).unwrap();
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    assert!(lib.artists_indexing().unwrap());
    drop(lib);
    let lib = Library::open(&path).unwrap();
    assert!(lib.artists_indexing().unwrap());
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    assert!(!lib.artists_indexing().unwrap());
    let after = lib
        .inner()
        .pool
        .read(|conn| {
            Ok(conn
                .prepare("SELECT id,size,mtime_ms FROM tracks ORDER BY id")?
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap();
    assert_eq!(before, after);
}

#[test]
fn person_query_handles_100k_relations_with_reverse_index() {
    const WITH_N: &str = r#"
WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE
x<10000) INSERT INTO tracks(root_id,rel_path,size,mtime_ms,album_key,added_at,updated_at)
SELECT ?1,x||'.flac',1,1,'old-album',1,1 FROM n
"#;

    let f = fixture();
    let root = f.root.0;
    f.lib
        .inner()
        .writer
        .run(move |conn| {
            let tx = conn.transaction()?;
            // Ten thousand tracks, ten names each; the old grouping remains one album.
            tx.execute(WITH_N, [root])?;
            for n in 0..10 {
                tx.execute(
                    "INSERT INTO person_artists VALUES(?1,?2)",
                    params![
                        crate::person::person_key(&format!("person {n}")),
                        format!("person {n}")
                    ],
                )?;
                tx.execute(
                    "INSERT INTO track_artists SELECT id,?1,?2,?3 FROM tracks",
                    params![
                        crate::person::person_key(&format!("person {n}")),
                        n,
                        format!("Person {n}")
                    ],
                )?;
            }
            schema::refresh_aggregates(&tx)?;
            tx.commit()?;
            Ok(())
        })
        .unwrap();
    let start = std::time::Instant::now();
    let (people, _) = f.lib.person_artists(Page::new(0, 500)).unwrap();
    assert_eq!(people.total, 10);
    assert!(
        people
            .items
            .iter()
            .all(|p| p.track_count == 10000 && p.album_count == 1)
    );
    let query = TrackQuery {
        filter: Filter::ArtistPerson {
            name: "Person 5".into(),
        },
        ..Default::default()
    };
    assert_eq!(
        f.lib.tracks_query(&query, Page::new(0, 0), &[]).unwrap().1,
        10000
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "100k relations took {:?}",
        start.elapsed()
    );
    f.lib
        .inner()
        .pool
        .read(|conn| {
            let plan = crate::track_query::explain_query(conn, &query)?;
            assert!(plan.iter().any(|p| p.contains("track_artists_person")));
            Ok(())
        })
        .unwrap();
}

#[test]
fn cue_performer_is_one_name_and_inherited_membership_stays_separate() {
    let mut base = meta("Whole", "Display", "Album");
    base.artists = Some(ArtistList::new(
        ["File One", "File Two"],
        ArtistListSource::ArtistsTag,
    ));
    let sheet = crate::cue::Sheet::parse(concat!(
        "PERFORMER \"Single / feat. Person\"\nFILE \"audio.flac\" WAVE\n TRACK 01 ",
        "AUDIO\n INDEX 01 00:00:00\n",
    ));
    let cuts = sheet.cut(true);
    let cut = &cuts[0];
    let result = crate::cue::cue_meta(&base, cut, None);
    let list = result.artists.unwrap();
    assert_eq!(list.source, ArtistListSource::CuePerformer);
    assert_eq!(list.artists.len(), 1);
    assert_eq!(list.artists[0].name, "Single / feat. Person");
}

fn audio_fixture(name: &str, target: &std::path::Path) {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../engine/tests/fixtures")
        .join(name);
    std::fs::copy(source, target).unwrap();
}

#[test]
fn real_vorbis_flac_opus_repeated_fields_and_artists_priority() {
    use lofty::config::WriteOptions;
    use lofty::ogg::tag::VorbisComments;
    use lofty::tag::TagExt;
    let dir = mpmp_test_support::TempDir::new().unwrap();
    for fixture in ["sine44_a.flac", "sine44_a.ogg", "sine44_a.opus"] {
        let path = dir.path().join(fixture);
        audio_fixture(fixture, &path);
        let mut tag = VorbisComments::default();
        tag.push("ARTIST".into(), "Display / feat. Credit".into());
        tag.push("ARTIST".into(), "Second Artist".into());
        tag.save_to_path(&path, WriteOptions::default()).unwrap();
        let meta = crate::tags::read::read_track(&path, None).unwrap();
        let display = meta.artist.clone();
        let list = meta.artists.unwrap();
        assert_eq!(list.source, ArtistListSource::ArtistValues);
        assert_eq!(
            list.artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            ["Display / feat. Credit", "Second Artist"]
        );
        tag.push("ARTISTS".into(), "  Björven  ".into());
        tag.push("ARTISTS".into(), "BJORVEN".into());
        tag.push("ARTISTS".into(), "AX/DZ; B, feat. C".into());
        tag.save_to_path(&path, WriteOptions::default()).unwrap();
        let meta = crate::tags::read::read_track(&path, None).unwrap();
        assert_eq!(meta.artist, display);
        let list = meta.artists.unwrap();
        assert_eq!(list.source, ArtistListSource::ArtistsTag);
        assert_eq!(
            list.artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            ["Björven", "AX/DZ; B, feat. C"]
        );
    }
}

#[test]
fn real_id3v24_nul_txxx_and_v23_slashes_preserve_display_chain() {
    use lofty::TextEncoding;
    use lofty::config::WriteOptions;
    use lofty::id3::v2::{ExtendedTextFrame, Frame, Id3v2Tag, TextInformationFrame};
    use lofty::tag::TagExt;
    let dir = mpmp_test_support::TempDir::new().unwrap();
    for v23 in [false, true] {
        let path = dir.path().join(if v23 { "v23.mp3" } else { "v24.mp3" });
        audio_fixture("sine44_a.mp3", &path);
        let mut tag = Id3v2Tag::new();
        let value = if v23 {
            "AX/DZ / Guest; feat. Person"
        } else {
            "AX/DZ\0Guest; feat. Person"
        };
        tag.insert(Frame::Text(TextInformationFrame::new(
            lofty::id3::v2::FrameId::Valid("TPE1".into()),
            TextEncoding::UTF8,
            value,
        )));
        tag.save_to_path(&path, WriteOptions::default().use_id3v23(v23))
            .unwrap();
        let meta = crate::tags::read::read_track(&path, None).unwrap();
        let display = meta.artist.clone();
        let list = meta.artists.unwrap();
        assert_eq!(list.source, ArtistListSource::ArtistValues);
        assert_eq!(
            list.artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            if v23 {
                vec!["AX/DZ / Guest; feat. Person"]
            } else {
                vec!["AX/DZ", "Guest; feat. Person"]
            }
        );
        if !v23 {
            tag.insert(Frame::UserText(ExtendedTextFrame::new(
                TextEncoding::UTF8,
                "ARTISTS",
                "One\0Two\0ONE",
            )));
            tag.save_to_path(&path, WriteOptions::default()).unwrap();
            let meta = crate::tags::read::read_track(&path, None).unwrap();
            assert_eq!(meta.artist, display);
            let list = meta.artists.unwrap();
            assert_eq!(list.source, ArtistListSource::ArtistsTag);
            assert_eq!(
                list.artists
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>(),
                ["One", "Two"]
            );
        }
    }
}

#[test]
fn real_id3v23_splits_only_explicit_artists_with_picard_join() {
    use lofty::TextEncoding;
    use lofty::config::WriteOptions;
    use lofty::id3::v2::{ExtendedTextFrame, Frame, FrameId, Id3v2Tag, TextInformationFrame};
    use lofty::tag::TagExt;
    let dir = mpmp_test_support::TempDir::new().unwrap();
    for (v23, explicit, expected) in [
        (true, Some("A/B"), vec!["A", "B"]),
        (true, None, vec!["A/B"]),
        (false, Some("AX/DZ"), vec!["AX/DZ"]),
        (false, Some(" A\0 \0B\0a "), vec!["A", "B"]),
        (
            true,
            Some(" / Björven / / BJORVEN / Guest; feat. X, Y / "),
            vec!["Björven", "Guest; feat. X, Y"],
        ),
        (true, Some(" / // "), vec!["A/B"]),
    ] {
        let path = dir.path().join("tagged.mp3");
        audio_fixture("sine44_a.mp3", &path);
        let mut tag = Id3v2Tag::new();
        tag.insert(Frame::Text(TextInformationFrame::new(
            FrameId::Valid("TPE1".into()),
            TextEncoding::UTF8,
            "A/B",
        )));
        if let Some(explicit) = explicit {
            tag.insert(Frame::UserText(ExtendedTextFrame::new(
                TextEncoding::UTF8,
                "ARTISTS",
                explicit,
            )));
        }
        tag.save_to_path(&path, WriteOptions::default().use_id3v23(v23))
            .unwrap();
        let meta = crate::tags::read::read_track(&path, None).unwrap();
        assert_eq!(meta.artist.as_deref(), Some("A/B"));
        let list = meta.artists.unwrap();
        assert_eq!(
            list.artists
                .iter()
                .map(|person| person.name.as_str())
                .collect::<Vec<_>>(),
            expected,
            "v23={v23}, explicit={explicit:?}"
        );
        assert_eq!(
            list.source,
            if explicit.is_some_and(|s| !s.trim_matches([' ', '/']).is_empty()) {
                ArtistListSource::ArtistsTag
            } else {
                ArtistListSource::ArtistValues
            }
        );
        // Raw stored text is not destructively rewritten as a split list.
        if let Some(explicit) = explicit
            && !explicit.contains('\0')
        {
            assert!(
                meta.tags
                    .iter()
                    .any(|(key, value)| key == "ARTISTS" && value == explicit.trim())
            );
        }
    }
}

#[test]
fn real_mp4_only_picard_namespace_establishes_artists_and_reads_all_text_values() {
    use lofty::config::WriteOptions;
    use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst};
    use lofty::tag::{Accessor, TagExt};
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("a.m4a");
    audio_fixture("sine44_a.m4a", &path);
    let mut tag = Ilst::new();
    tag.set_artist("Display".into());
    tag.insert(
        Atom::from_collection(
            AtomIdent::Freeform {
                mean: "other.namespace".into(),
                name: "ARTISTS".into(),
            },
            vec![AtomData::UTF8("False person".into())],
        )
        .unwrap(),
    );
    tag.save_to_path(&path, WriteOptions::default()).unwrap();
    let meta = crate::tags::read::read_track(&path, None).unwrap();
    assert_eq!(meta.artists.unwrap().artists[0].name, "Display");
    tag.insert(
        Atom::from_collection(
            AtomIdent::Freeform {
                mean: "com.apple.iTunes".into(),
                name: "ARTISTS".into(),
            },
            vec![
                AtomData::UTF8("First".into()),
                AtomData::UTF8("Second / feat. Person".into()),
                AtomData::UTF8("FIRST".into()),
            ],
        )
        .unwrap(),
    );
    tag.save_to_path(&path, WriteOptions::default()).unwrap();
    let meta = crate::tags::read::read_track(&path, None).unwrap();
    assert_eq!(meta.artist.as_deref(), Some("Display"));
    let list = meta.artists.unwrap();
    assert_eq!(list.source, ArtistListSource::ArtistsTag);
    assert_eq!(
        list.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        ["First", "Second / feat. Person"]
    );
}

#[test]
fn edit_undo_redo_update_people_without_replacing_display_or_album_key() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    audio_fixture("sine44_a.flac", &music.join("a.flac"));
    let lib = Library::open(&dir.path().join("db.sqlite")).unwrap();
    lib.add_root(&music, crate::tests::UI).unwrap();
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    let id = lib.track_ids(&TrackQuery::default(), 1).unwrap()[0];
    let before = lib.track(id).unwrap().unwrap();
    let edit = lib
        .edit_tags(
            &[id],
            &[crate::TagChange::Set {
                key: "ARTISTS".into(),
                values: vec!["One".into(), "Two".into()],
            }],
            crate::tests::UI,
            crate::TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(edit.written, [id]);
    let after = lib.track(id).unwrap().unwrap();
    assert_eq!(after.artist, before.artist);
    assert_eq!(after.album_key, before.album_key);
    assert_eq!(
        after
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        ["One", "Two"]
    );
    let undo = lib
        .undo_tag_edit(
            edit.action.unwrap(),
            crate::tests::UI,
            crate::TagJobOptions::default(),
        )
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(lib.track(id).unwrap().unwrap().artists, before.artists);
    let redo = lib
        .undo_tag_edit(
            undo.action.unwrap(),
            crate::tests::UI,
            crate::TagJobOptions::default(),
        )
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(redo.written, [id]);
    assert_eq!(lib.track(id).unwrap().unwrap().artists, after.artists);
}

#[test]
fn repeated_track_ids_across_pages_never_duplicate_membership() {
    let f = fixture();
    let mut m = meta("Track", "Person", "Album");
    m.tags = vec![
        ("ARTISTS".into(), "One".into()),
        ("ARTISTS".into(), "Two".into()),
    ];
    upsert(&f.lib, vec![record(f.root, "a.flac", m)]);
    let id = f.lib.track_ids(&TrackQuery::default(), 1).unwrap()[0];
    let tracks = f.lib.tracks(&vec![id; 1001]).unwrap();
    assert_eq!(tracks.len(), 1001);
    assert!(tracks.iter().all(|t| t.artists.len() == 2));
}

#[test]
fn forced_id3_encoding_applies_to_each_person_value() {
    use lofty::config::WriteOptions;
    use lofty::id3::v2::{Frame, FrameId, Id3v2Tag, TextInformationFrame};
    use lofty::tag::TagExt;
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("a.mp3");
    audio_fixture("sine44_a.mp3", &path);
    let (bytes, _, _) = encoding_rs::GBK.encode("周砚舟\0云笛");
    let text: String = bytes.iter().map(|&b| char::from(b)).collect();
    let mut tag = Id3v2Tag::new();
    tag.insert(Frame::Text(TextInformationFrame::new(
        FrameId::Valid("TPE1".into()),
        lofty::TextEncoding::Latin1,
        text,
    )));
    tag.save_to_path(&path, WriteOptions::default()).unwrap();
    let list = crate::tags::read::read_track(&path, Some(encoding_rs::GBK))
        .unwrap()
        .artists
        .unwrap();
    assert_eq!(
        list.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        ["周砚舟", "云笛"]
    );
}

#[test]
fn backfill_commits_500_row_pages_and_resumes_after_interruption() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    for n in 0..501 {
        audio_fixture("sine44_a.flac", &music.join(format!("{n:04}.flac")));
    }
    let path = dir.path().join("db.sqlite");
    let lib = Library::open(&path).unwrap();
    let root = lib.add_root(&music, crate::tests::UI).unwrap().value;
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    lib.inner()
        .writer
        .run(|conn| {
            conn.execute("UPDATE tracks SET artists_parser_version=0", [])?;
            Ok(())
        })
        .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let interrupted = lib.clone();
    *lib.inner().hooks.scan_file.lock().unwrap() = Some(Arc::new(move |_, _| {
        if count.fetch_add(1, Ordering::SeqCst) == 500 {
            interrupted.suspend_root(root).unwrap();
        }
    }));
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    *lib.inner().hooks.scan_file.lock().unwrap() = None;
    let done = lib
        .inner()
        .pool
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM tracks WHERE artists_parser_version=?1",
                [crate::person::PARSER_VERSION],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .unwrap();
    assert_eq!(done, 500);
    drop(lib);
    // An absent root preserves the final pending row and its previous memberships.
    let away = dir.path().join("away");
    std::fs::rename(&music, &away).unwrap();
    let lib = Library::open(&path).unwrap();
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    assert!(lib.artists_indexing().unwrap());
    std::fs::rename(&away, &music).unwrap();
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    assert!(!lib.artists_indexing().unwrap());
    assert_eq!(
        lib.track_ids(&TrackQuery::default(), 1000).unwrap().len(),
        501
    );
}

#[test]
fn external_rewrite_with_the_same_stamp_does_not_commit_an_undo() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let path = music.join("a.flac");
    audio_fixture("sine44_a.flac", &path);
    use lofty::config::WriteOptions;
    use lofty::tag::TagExt;
    let mut tag = lofty::ogg::tag::VorbisComments::default();
    tag.push("ARTISTS".into(), "Original person".into());
    tag.save_to_path(&path, WriteOptions::default()).unwrap();
    let before = std::fs::read(&path).unwrap();
    let lib = Library::open(&dir.path().join("db.sqlite")).unwrap();
    lib.add_root(&music, crate::tests::UI).unwrap();
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    let id = lib.track_ids(&TrackQuery::default(), 1).unwrap()[0];
    let edit = lib
        .edit_tags(
            &[id],
            &[crate::TagChange::Set {
                key: "ARTISTS".into(),
                values: vec!["Different human".into()],
            }],
            crate::tests::UI,
            crate::TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before.len() as u64);
    std::fs::write(&path, before).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let undo = lib
        .undo_tag_edit(
            edit.action.unwrap(),
            crate::tests::UI,
            crate::TagJobOptions::default(),
        )
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert!(undo.written.is_empty());
    assert!(undo.completed.is_empty());
    assert_eq!(undo.skipped, [(id, "file_changed")]);
    let current = lib.track(id).unwrap().unwrap();
    assert_eq!(current.artists[0].name, "Different human");
}

#[test]
fn unreadable_after_edit_keeps_old_people_pending_for_retry() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let path = music.join("a.flac");
    audio_fixture("sine44_a.flac", &path);
    let lib = Library::open(&dir.path().join("db.sqlite")).unwrap();
    lib.add_root(&music, crate::tests::UI).unwrap();
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    let id = lib.track_ids(&TrackQuery::default(), 1).unwrap()[0];
    let original = lib.track(id).unwrap().unwrap();
    *lib.inner().hooks.fail_audio_read.lock().unwrap() =
        Some(crate::storage_access::comparable(&path));
    let edit = lib
        .edit_tags(
            &[id],
            &[crate::TagChange::Set {
                key: "ARTISTS".into(),
                values: vec!["New person".into()],
            }],
            crate::tests::UI,
            crate::TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(edit.written, [id]);
    let old = lib.track(id).unwrap().unwrap();
    assert_eq!(old.artists, original.artists);
    assert!(!old.artists_complete);
    assert!(old.available);
    *lib.inner().hooks.fail_audio_read.lock().unwrap() = None;
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    let after = lib.track(id).unwrap().unwrap();
    assert!(after.artists_complete);
    assert_eq!(after.artists[0].name, "New person");
}

#[test]
fn delayed_scan_cannot_clear_failed_edit_or_undo_backfill() {
    for undo in [false, true] {
        for artists_only in [false, true] {
            let dir = mpmp_test_support::TempDir::new().unwrap();
            let music = dir.path().join("music");
            std::fs::create_dir(&music).unwrap();
            let path = music.join("a.flac");
            audio_fixture("sine44_a.flac", &path);
            let lib = Library::open(&dir.path().join("db.sqlite")).unwrap();
            let root = lib.add_root(&music, crate::tests::UI).unwrap().value;
            if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
                scan.wait().unwrap();
            }
            let id = lib.track_ids(&TrackQuery::default(), 1).unwrap()[0];
            let action = if undo {
                lib.edit_tags(
                    &[id],
                    &[crate::TagChange::Set {
                        key: "ARTISTS".into(),
                        values: vec!["After".into()],
                    }],
                    crate::tests::UI,
                    crate::TagJobOptions::default(),
                )
                .unwrap()
                .wait()
                .unwrap()
                .action
            } else {
                None
            };
            let known = lib
                .inner()
                .pool
                .read(|c| crate::db::reads::root_files(c, root))
                .unwrap()
                .files["a.flac"];
            // Freeze the exact parsed record before the tag job, and submit it only
            // after the physical write and failed readback have committed.
            let old_meta = crate::tags::read::read_track(&path, None).unwrap();
            let delayed = crate::db::writer::TrackRecord {
                root_id: root,
                rel_path: "a.flac".into(),
                size: known.size,
                mtime_ms: known.mtime_ms,
                has_folder_art: false,
                meta: old_meta,
                cue: None,
                based_on: Some((known.size, known.mtime_ms)),
                based_on_revision: Some(known.revision),
                artists_only,
                file_identity: None,
            };
            let previous = lib.track(id).unwrap().unwrap().artists;
            *lib.inner().hooks.fail_audio_read.lock().unwrap() =
                Some(crate::storage_access::comparable(&path));
            if let Some(action) = action {
                lib.undo_tag_edit(action, crate::tests::UI, crate::TagJobOptions::default())
                    .unwrap()
                    .unwrap()
                    .wait()
                    .unwrap();
            } else {
                lib.edit_tags(
                    &[id],
                    &[crate::TagChange::Set {
                        key: "ARTISTS".into(),
                        values: vec!["After".into()],
                    }],
                    crate::tests::UI,
                    crate::TagJobOptions::default(),
                )
                .unwrap()
                .wait()
                .unwrap();
            }
            crate::tests::upsert(&lib, vec![delayed]);
            let pending = lib.track(id).unwrap().unwrap();
            assert!(!pending.artists_complete);
            assert_eq!(pending.artists, previous);
            *lib.inner().hooks.fail_audio_read.lock().unwrap() = None;
            if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
                scan.wait().unwrap();
            }
            let current = lib.track(id).unwrap().unwrap();
            assert!(current.artists_complete);
            if !undo {
                assert_eq!(current.artists[0].name, "After");
            } else {
                assert_ne!(current.artists, previous);
            }
        }
    }
}

#[test]
fn intents_survive_journal_pruning_and_reopen() {
    const INSERT_ACTIONS: &str = r#"
INSERT INTO actions(id,at_ms,origin,kind,summary,target_count) VALUES(1,1,'ui','tag_edit','old',1);
INSERT INTO file_write_intents(family,format_version,authorization_json,target_json,before_digest,before_image,after_digest,stage,action_id,created_at,updated_at)
VALUES('cover',1,'{}','{}','before',x'010203','after','pending',1,1,1);
"#;

    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("db.sqlite");
    let mut conn = Connection::open(&path).unwrap();
    schema::migrate(&mut conn).unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    conn.execute_batch(INSERT_ACTIONS).unwrap();
    crate::journal::prune(&conn, i64::MAX / 2).unwrap();
    assert_eq!(
        conn.query_row("SELECT action_id FROM file_write_intents", [], |r| r
            .get::<_, Option<i64>>(0))
            .unwrap(),
        None
    );
    drop(conn);
    let lib = Library::open(&path).unwrap();
    lib.inner()
        .pool
        .read(|c| {
            assert_eq!(
                c.query_row("SELECT before_image FROM file_write_intents", [], |r| r
                    .get::<_, Vec<u8>>(0))?,
                [1, 2, 3]
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn indexing_ignores_unavailable_or_removed_but_keeps_offline_and_released() {
    let f = fixture();
    upsert(
        &f.lib,
        vec![record(f.root, "missing.flac", meta("Old", "Name", "Album"))],
    );
    f.lib
        .inner()
        .writer
        .run(|c| {
            c.execute("UPDATE tracks SET artists_parser_version=0", [])?;
            Ok(())
        })
        .unwrap();
    assert!(f.lib.artists_indexing().unwrap());
    assert!(!f.lib.rescan_recommended().unwrap());
    f.lib.release_root(f.root).unwrap();
    assert!(f.lib.artists_indexing().unwrap());
    f.lib.remove_root(f.root, crate::tests::UI).unwrap();
    assert!(!f.lib.artists_indexing().unwrap());
    f.lib
        .inner()
        .pool
        .read(|c| {
            assert_eq!(
                c.query_row("SELECT artists_parser_version FROM tracks", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .unwrap();
    let music = f._dir.path().join("music");
    f.lib.add_root(&music, crate::tests::UI).unwrap();
    f.lib
        .inner()
        .writer
        .run(|c| {
            c.execute("UPDATE tracks SET available=1", [])?;
            Ok(())
        })
        .unwrap();
    assert!(f.lib.artists_indexing().unwrap());
    f.lib
        .inner()
        .writer
        .run(|c| {
            c.execute("UPDATE tracks SET available=0", [])?;
            Ok(())
        })
        .unwrap();
    assert!(!f.lib.artists_indexing().unwrap());
}

#[test]
fn artist_spelling_is_per_track_orphans_are_collected_and_names_are_bounded() {
    let f = fixture();
    let mut first = meta("First", "BJORVEN", "Album");
    first.tags = vec![("ARTISTS".into(), "Björven".into())];
    let mut second = meta("Second", "BJORVEN", "Album");
    second.tags = vec![("ARTISTS".into(), "BJORVEN".into())];
    upsert(
        &f.lib,
        vec![
            record(f.root, "a.flac", first),
            record(f.root, "b.flac", second),
        ],
    );
    let ids = f.lib.track_ids(&TrackQuery::default(), 10).unwrap();
    let tracks = f.lib.tracks(&ids).unwrap();
    assert_eq!(tracks[0].artists[0].name, "Björven");
    assert_eq!(tracks[1].artists[0].name, "BJORVEN");
    assert_eq!(tracks[0].artists[0].key, tracks[1].artists[0].key);
    for name in ["a.flac", "b.flac"] {
        upsert(
            &f.lib,
            vec![record(f.root, name, meta("Updated", "Other", "Album"))],
        );
    }
    f.lib
        .inner()
        .pool
        .read(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM person_artists WHERE key=?1",
                    [crate::person::person_key("bjorven")],
                    |r| r.get::<_, i64>(0)
                )?,
                0
            );
            Ok(())
        })
        .unwrap();
    let list = ArtistList::new(
        ["界".repeat(crate::tags::read::MAX_TAG_VALUE).as_str()],
        ArtistListSource::ArtistsTag,
    );
    assert!(list.artists[0].name.len() <= crate::tags::read::MAX_TAG_VALUE);
    assert_eq!(list.artists[0].name.len() % 3, 0);
}

#[test]
fn unchanged_backfill_preserves_all_metadata_and_does_not_repeat_scan_errors() {
    const UPDATE_TRACKS: &str = r#"
UPDATE tracks SET artists_parser_version=0,title='Preserved',
artist='Preserved credit',album_key='preserved-key',album_group_artist='Preserved group',updated_at=12
"#;

    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    audio_fixture("sine44_a.flac", &music.join("a.flac"));
    let lib = Library::open(&dir.path().join("db.sqlite")).unwrap();
    lib.add_root(&music, crate::tests::UI).unwrap();
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        scan.wait().unwrap();
    }
    lib.inner()
        .writer
        .run(|c| {
            c.execute(UPDATE_TRACKS, [])?;
            Ok(())
        })
        .unwrap();
    let id = lib.track_ids(&TrackQuery::default(), 1).unwrap()[0];
    let before = lib.track(id).unwrap().unwrap();
    *lib.inner().hooks.fail_audio_read.lock().unwrap() = Some(music.join("a.flac"));
    for _ in 0..2 {
        if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
            let report = scan.wait().unwrap();
            assert_eq!(report.errors, 0);
            assert_eq!(report.updated, 0);
        }
    }
    *lib.inner().hooks.fail_audio_read.lock().unwrap() = None;
    if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
        assert_eq!(scan.wait().unwrap().updated, 0);
    }
    let after = lib.track(id).unwrap().unwrap();
    assert!(after.artists_complete);
    assert_eq!(before.artist, after.artist);
    assert_eq!(before.title, after.title);
    assert_eq!(before.album_key, after.album_key);
    assert_eq!(before.group_artist, after.group_artist);
    assert_eq!(before.updated_at, after.updated_at);
}

/// Snapshot every column including inverse BLOBs, action links and detail JSON.
fn table_snapshot(conn: &Connection, table: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
        .unwrap();
    let count = stmt.column_count();
    stmt.query_map([], |r| {
        (0..count)
            .map(|i| r.get(i))
            .collect::<rusqlite::Result<Vec<_>>>()
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

#[test]
fn complete_journal_and_album_cache_survive_and_remain_undoable() {
    const SEED_ALBUM_LOUDNESS: &str =
    "INSERT INTO album_loudness(album_key,integrated_lufs,sample_peak,measured_at) SELECT album_key,-14.75,0.876,123456 FROM tracks;
        INSERT INTO track_loudness(track_id,version,integrated_lufs,sample_peak,measured_at) SELECT id,'fixture',-15.25,0.8,123456 FROM tracks;
        UPDATE actions SET details_json='{\"fixture\":\"complete details\"}' WHERE id=(SELECT MAX(id) FROM actions);
        PRAGMA user_version=13; PRAGMA application_id=0;";

    let f = fixture();
    upsert(
        &f.lib,
        vec![record(f.root, "one.flac", meta("One", "Person", "Album"))],
    );
    let id = f
        .lib
        .tracks_query(&TrackQuery::default(), Page::new(0, 5), &[])
        .unwrap()
        .0[0]
        .track
        .id;
    f.lib.set_loved(&[id], true, crate::tests::UI).unwrap();
    let rate = f
        .lib
        .set_rating(&[id], Some(4), crate::tests::UI)
        .unwrap()
        .action
        .unwrap();
    let undo = f
        .lib
        .undo_action(rate, crate::tests::UI)
        .unwrap()
        .action
        .unwrap();
    let redo = f
        .lib
        .undo_action(undo, crate::tests::UI)
        .unwrap()
        .action
        .unwrap();
    let path = f.lib.inner().pool.path().to_owned();
    drop(f.lib);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(SEED_ALBUM_LOUDNESS).unwrap();
    let tables = [
        "actions",
        "action_targets",
        "album_loudness",
        "track_loudness",
        "track_user",
    ];
    let before = tables.map(|table| table_snapshot(&conn, table));
    drop(conn);
    drop(Library::open(&path).unwrap());
    let conn = Connection::open(&path).unwrap();
    for (table, snapshot) in tables.into_iter().zip(before) {
        assert!(!snapshot.is_empty(), "empty {table} fixture");
        assert_eq!(table_snapshot(&conn, table), snapshot, "{table}");
    }
    drop(conn);
    let lib = Library::open(&path).unwrap();
    assert!(lib.action(redo).unwrap().unwrap().undoable);
    lib.undo_action(redo, crate::tests::UI).unwrap();
    assert_eq!(lib.track_rows(&[id], &[]).unwrap()[0].rating, None);
}

#[test]
fn track_and_album_search_do_not_query_person_aggregates() {
    let f = fixture();
    upsert(
        &f.lib,
        vec![record(f.root, "one.flac", meta("One", "Person", "Album"))],
    );
    let conn = Connection::open(f.lib.inner().pool.path()).unwrap();
    conn.execute_batch("ALTER TABLE person_artists RENAME TO unavailable_person_aggregate;")
        .unwrap();
    for kind in [crate::SearchKind::Tracks, crate::SearchKind::Albums] {
        f.lib
            .search_page_view("one", Some(kind), Page::new(0, 10), ArtistView::Person)
            .unwrap();
    }
    assert!(
        f.lib
            .search_page_view(
                "person",
                Some(crate::SearchKind::Artists),
                Page::new(0, 10),
                ArtistView::Person
            )
            .is_err()
    );
}

#[test]
fn delayed_encoding_batches_are_rejected_before_and_after_track_snapshot() {
    use lofty::{
        TextEncoding,
        config::WriteOptions,
        id3::v2::{Frame, FrameId, Id3v2Tag, TextInformationFrame},
        tag::TagExt,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;
    for after_snapshot in [false, true] {
        let f = fixture();
        let music = f.lib.roots().unwrap()[0].path.clone();
        let path = music.join("old.mp3");
        audio_fixture("sine44_a.mp3", &path);
        let mut tag = Id3v2Tag::new();
        let text: String = [0xa4, 0x40, 0xa4, 0x41]
            .into_iter()
            .map(char::from)
            .collect();
        tag.insert(Frame::Text(TextInformationFrame::new(
            FrameId::Valid("TPE1".into()),
            TextEncoding::Latin1,
            text,
        )));
        tag.save_to_path(&path, WriteOptions::default()).unwrap();
        std::fs::copy(&path, music.join("new.mp3")).unwrap();
        upsert(
            &f.lib,
            vec![record(
                f.root,
                "old.mp3",
                meta("Before", "Before", "Before"),
            )],
        );
        let id = f.lib.track_ids(&TrackQuery::default(), 1).unwrap()[0];
        let a = crate::tags::read::read_track(&path, Some(encoding_rs::GBK)).unwrap();
        let b = crate::tags::read::read_track(&path, Some(encoding_rs::BIG5)).unwrap();
        assert_ne!(a.artist, b.artist);
        f.lib.set_root_encoding(f.root, Some("GBK")).unwrap();
        let before = f.lib.track(id).unwrap().unwrap();
        let revision = || {
            f.lib
                .inner()
                .pool
                .read(|conn| {
                    Ok(conn.query_row(
                        "SELECT metadata_revision FROM tracks WHERE id=?1",
                        [id.0],
                        |r| r.get::<_, i64>(0),
                    )?)
                })
                .unwrap()
        };
        let a_revision = revision();
        let (arrived_tx, arrived_rx) = crossbeam_channel::bounded(1);
        let (resume_tx, resume_rx) = crossbeam_channel::bounded(1);
        let paused = AtomicBool::new(false);
        let pause = Arc::new(move || {
            if !paused.swap(true, Ordering::SeqCst) {
                arrived_tx.send(()).unwrap();
                resume_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            }
        });
        if after_snapshot {
            let pause = pause.clone();
            *f.lib.inner().hooks.scan_file.lock().unwrap() = Some(Arc::new(move |_, _| pause()));
        }
        let crate::ScanStart::Started(scan) = f
            .lib
            .scan(move |_| {
                if !after_snapshot {
                    pause()
                }
            })
            .unwrap()
        else {
            panic!("scan not started")
        };
        arrived_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        // In the first case the scanner has captured A but not the track rows yet;
        // in the second it has captured both A and the old row revisions.
        f.lib.set_root_encoding(f.root, Some("Big5")).unwrap();
        assert_eq!(revision(), a_revision + 1);
        resume_tx.send(()).unwrap();
        scan.wait().unwrap();
        *f.lib.inner().hooks.scan_file.lock().unwrap() = None;
        assert_eq!(f.lib.track(id).unwrap().unwrap(), before);
        assert_eq!(f.lib.track_ids(&TrackQuery::default(), 10).unwrap(), [id]);
        assert_eq!(
            f.lib
                .inner()
                .pool
                .read(|conn| Ok(conn.query_row(
                    "SELECT mtime_ms FROM tracks WHERE id=?1",
                    [id.0],
                    |r| r.get::<_, i64>(0)
                )?))
                .unwrap(),
            -1
        );
        assert!(f.lib.rescan_recommended().unwrap());
        let crate::ScanStart::Started(scan) = f.lib.scan(|_| {}).unwrap() else {
            panic!("scan not started")
        };
        let report = scan.wait().unwrap();
        assert_eq!(report.updated, 1);
        assert_eq!(report.added, 1);
        let ids = f.lib.track_ids(&TrackQuery::default(), 10).unwrap();
        assert_eq!(ids.len(), 2);
        for track in f.lib.tracks(&ids).unwrap() {
            assert_eq!(track.artist, b.artist);
            assert_eq!(track.artists, b.artists.as_ref().unwrap().artists);
            assert_eq!(track.album_key, crate::db::writer::album_grouping(&b).0);
            assert!(track.artists_complete);
        }
        assert!(!f.lib.rescan_recommended().unwrap());
    }
}

#[test]
fn forced_encoding_is_retained_across_scan_batch_pages() {
    let f = fixture();
    let music = f.lib.roots().unwrap()[0].path.clone();
    for n in 0..501 {
        audio_fixture("sine44_a.flac", &music.join(format!("{n:04}.flac")));
    }
    f.lib.set_root_encoding(f.root, Some("Big5")).unwrap();
    let crate::ScanStart::Started(scan) = f.lib.scan(|_| {}).unwrap() else {
        panic!("scan not started")
    };
    assert_eq!(scan.wait().unwrap().added, 501);
    let ids = f.lib.track_ids(&TrackQuery::default(), 1000).unwrap();
    assert_eq!(ids.len(), 501);
    assert!(
        f.lib
            .tracks(&ids)
            .unwrap()
            .iter()
            .all(|t| t.artists_complete)
    );
}

#[test]
fn opaque_identity_keys_are_versioned() {
    let f = fixture();
    upsert(
        &f.lib,
        vec![record(f.root, "one.flac", meta("One", "Björven", "Album"))],
    );
    let track = f
        .lib
        .tracks_query(&TrackQuery::default(), Page::new(0, 10), &[])
        .unwrap()
        .0
        .remove(0)
        .track;
    let key = track.artists[0].key.clone();
    assert_eq!(key, crate::person::person_key("bjorven"));
    assert!(key.starts_with("person:v1:"));
    assert!(!key.contains("bjorven"));
}

#[test]
fn unrecognized_stored_artist_source_does_not_claim_completeness() {
    let f = fixture();
    upsert(
        &f.lib,
        vec![record(f.root, "one.flac", meta("One", "Person", "Album"))],
    );
    f.lib
        .inner()
        .writer
        .run(|c| {
            c.execute("UPDATE tracks SET artists_source='future_source'", [])?;
            Ok(())
        })
        .unwrap();
    let row = f
        .lib
        .tracks_query(&TrackQuery::default(), Page::new(0, 10), &[])
        .unwrap()
        .0
        .remove(0);
    assert_eq!(row.track.artists_source, None);
    assert!(!row.track.artists_complete);
}
