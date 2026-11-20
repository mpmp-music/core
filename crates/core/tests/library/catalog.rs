//! Catalog scenarios.
use super::*;
use mpmp_core::catalog::Browse;

#[test]
fn startup_backfills_pending_person_parser_without_manual_rescan_or_metadata_changes() {
    use lofty::TextEncoding;
    use lofty::id3::v2::{ExtendedTextFrame, Frame, FrameId, Id3v2Tag, TextInformationFrame};
    use lofty::tag::TagExt;
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_a.mp3");
    // Match the reported library size and its explicit v2.3/v2.4 tag counts.
    for (name, explicit, v23) in [
        ("v23", true, true),
        ("v24", true, false),
        ("artist", false, true),
    ] {
        let template = dir.path().join(format!("{name}.mp3"));
        std::fs::copy(&source, &template).unwrap();
        let mut tag = Id3v2Tag::new();
        tag.insert(Frame::Text(TextInformationFrame::new(
            FrameId::Valid("TPE1".into()),
            TextEncoding::UTF8,
            "A/B",
        )));
        if explicit {
            tag.insert(Frame::UserText(ExtendedTextFrame::new(
                TextEncoding::UTF8,
                "ARTISTS",
                "A/B",
            )));
        }
        tag.save_to_path(&template, WriteOptions::default().use_id3v23(v23))
            .unwrap();
    }
    for index in 0..875 {
        let name = if index < 420 {
            "v23"
        } else if index < 427 {
            "v24"
        } else {
            "artist"
        };
        std::fs::copy(
            dir.path().join(format!("{name}.mp3")),
            music.join(format!("{index:04}.mp3")),
        )
        .unwrap();
    }
    let database = dir.path().join("library.db");
    let lib = mpmp_library::Library::open(&database).unwrap();
    lib.add_root(&music, mpmp_library::ActionMeta::new("ui"))
        .unwrap();
    let mpmp_library::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() else {
        panic!()
    };
    scan.wait().unwrap();
    let ids = lib
        .track_ids(&mpmp_library::TrackQuery::default(), 1000)
        .unwrap();
    let before = lib.tracks(&ids).unwrap();
    assert_eq!(before.len(), 875);
    drop(lib);
    // A current identity with parser_version=1 still needs a format-aware read.
    // Raw ARTISTS text alone cannot distinguish the two ID3 versions.
    let pending_key = before
        .iter()
        .flat_map(|track| &track.artists)
        .find(|person| person.name == "A/B")
        .unwrap()
        .key
        .clone();
    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.execute_batch("BEGIN; DELETE FROM track_artists; DELETE FROM person_artists;")
        .unwrap();
    conn.execute(
        "INSERT INTO person_artists VALUES(?1,'a/b')",
        [&pending_key],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO track_artists SELECT id,?1,0,'A/B' FROM tracks",
        [&pending_key],
    )
    .unwrap();
    conn.execute_batch("UPDATE tracks SET artists_parser_version=1; COMMIT;")
        .unwrap();
    drop(conn);
    let lib = Arc::new(mpmp_library::Library::open(&database).unwrap());
    assert!(lib.artists_indexing().unwrap());
    assert!(
        lib.tracks(&ids)
            .unwrap()
            .iter()
            .all(|track| track.artists.len() == 1 && !track.artists_complete)
    );
    let started = Instant::now();
    let core = CoreHandle::start(
        Box::new(support::null_output()),
        Arc::new(FileResolver),
        lib.clone(),
        CoreConfig {
            automatic_loudness_scan: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    // No Rescan command or explicit Library::scan: startup owns the backfill.
    while lib.artists_indexing().unwrap() {
        assert!(
            started.elapsed() < TIMEOUT,
            "startup person backfill did not complete"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    println!(
        "ARTISTS_BACKFILL tracks=875 elapsed_ms={}",
        started.elapsed().as_millis()
    );
    assert_eq!(
        lib.tracks(&ids).unwrap(),
        before,
        "backfill must preserve every other stored track field"
    );
    let tracks = lib.tracks(&ids).unwrap();
    assert_eq!(
        tracks
            .iter()
            .filter(|track| track
                .artists
                .iter()
                .map(|person| person.name.as_str())
                .collect::<Vec<_>>()
                == ["A", "B"])
            .count(),
        420
    );
    assert_eq!(
        tracks
            .iter()
            .filter(|track| track.artists.len() == 1 && track.artists[0].name == "A/B")
            .count(),
        455
    );
    assert!(core.shutdown_and_wait(TIMEOUT));
}

#[test]
fn scan_browse_play_and_history() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = dir.path().join("music");
    make_album(&music);
    let library = mpmp_library::Library::open(&dir.path().join("library.db")).unwrap();
    let output = support::null_output();
    let audio = output.handle();
    let core = CoreHandle::start(
        Box::new(output),
        Arc::new(FileResolver),
        Arc::new(library) as Arc<dyn Catalog>,
        CoreConfig {
            automatic_loudness_scan: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    let sub = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    let mut request_id = 0;
    let mut run = |command| {
        request_id += 1;
        match core
            .execute(Request::new(request_id, command), Origin::Ui)
            .result
        {
            CommandResult::Ok(receipt) => receipt,
            other => panic!("{other:?}"),
        }
    };
    let seen = std::cell::RefCell::new(Vec::new());
    let wait_for = |pred: &dyn Fn(&Event) -> bool, render: bool| {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if render && audio.is_open() {
                audio.render_f32(512);
            }
            std::thread::sleep(Duration::from_millis(3));
            for envelope in sub.events.try_iter() {
                let hit = pred(&envelope.event);
                seen.borrow_mut().push(envelope.event);
                if hit {
                    return;
                }
            }
        }
        panic!("event never came: {:#?}", seen.borrow());
    };

    // Add the folder: it is scanned as a job, and LibraryChanged follows.
    let added = run(Command::AddRoot {
        path: music.display().to_string(),
    });
    let (root, job) = (added.root.unwrap(), added.job.unwrap());
    wait_for(
        &|e| {
            matches!(e, Event::JobFinished { job: j, outcome: JobOutcome::Completed {
                report: JobReport::Scan { added_count: 3, error_count: 0, .. } }, .. } if *j == job)
        },
        false,
    );
    wait_for(&|e| matches!(e, Event::LibraryChanged), false);
    // Rescanning returns a job too.
    assert!(run(Command::Rescan).job.is_some());
    let QueryResult::Roots { roots } = core.query(Query::Roots) else {
        panic!()
    };
    assert_eq!((roots[0].id, roots[0].track_count), (root, 3));

    // Browse: the album is there, with CJK and full-width text intact.
    let QueryResult::Albums {
        albums,
        total,
        next_offset,
        ..
    } = core.query(Query::Albums {
        sort: AlbumSort::Artist,
        offset: 0,
        limit: 10,
    })
    else {
        panic!()
    };
    assert_eq!(albums.len(), 1, "{albums:?}");
    assert_eq!((total, next_offset), (1, None));
    assert_eq!(albums[0].title, "测试专辑");
    assert_eq!(albums[0].track_count, 3);
    // An album by key; an unknown key is an error.
    let QueryResult::Album { album } = core.query(Query::Album {
        offset: Some(0),
        limit: Some(1000),
        key: albums[0].key.clone(),
    }) else {
        panic!()
    };
    assert_eq!(album.tracks.len(), 3);
    assert_eq!(album.tracks[0].album_key, albums[0].key);
    assert_eq!(album.tracks[0].group_artist.as_deref(), Some("Test Artist"));
    let QueryResult::Tracks { tracks, total, .. } = core.query(Query::Tracks {
        query: TrackQuery {
            filter: Filter::Text {
                field: Field::GroupArtist,
                op: TextOp::Equals,
                text: "test artist".into(),
            },
            sort: vec![],
        },
        offset: 0,
        limit: 10,
        tags: vec![],
    }) else {
        panic!()
    };
    assert_eq!(total, 3);
    assert!(
        tracks
            .iter()
            .all(|t| t.group_artist.as_deref() == Some("Test Artist"))
    );
    for sort in [
        AlbumSort::TitleDesc,
        AlbumSort::ArtistDesc,
        AlbumSort::YearDesc,
        AlbumSort::OldestAdded,
    ] {
        let QueryResult::Albums { albums: sorted, .. } = core.query(Query::Albums {
            sort,
            offset: 0,
            limit: 10,
        }) else {
            panic!()
        };
        assert_eq!(sorted, albums);
    }
    assert_eq!(album.tracks[0].root, root);
    assert_eq!(album.tracks[0].path, "Test Artist/测试专辑/01 a.flac");
    assert_eq!(album.tracks[0].codec.as_deref(), Some("flac"));
    let QueryResult::Err { error } = core.query(Query::Album {
        offset: Some(0),
        limit: Some(1000),
        key: "nope".into(),
    }) else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::AlbumNotFound);
    // Full-width search text matches half-width tags, and CJK 2-character queries work.
    for text in ["ｔｈｉｒｄ", "测试", "test artist"] {
        let QueryResult::Search { results } = core.query(Query::Search {
            artist_view: None,
            kind: None,
            offset: 0,
            text: text.into(),
            limit: 10,
        }) else {
            panic!()
        };
        assert!(
            !results.tracks.is_empty() || !results.albums.is_empty(),
            "nothing found for {text}"
        );
        assert_eq!(results.tracks_total as usize, results.tracks.len());
        assert_eq!(results.albums_total as usize, results.albums.len());
    }

    // Play the album: three tracks heard in order, gaplessly, then the end.
    let created = run(Command::Play {
        context: PlayContext::Album {
            key: albums[0].key.clone(),
        },
        start: 0,
        start_track: None,
    })
    .created_entries;
    assert_eq!(created.len(), 3);
    wait_for(
        &|e| matches!(e, Event::StateChanged { snapshot } if snapshot.state == PlayState::Stopped),
        true,
    );
    let started: Vec<String> = seen
        .borrow()
        .iter()
        .filter_map(|e| match e {
            Event::TrackStarted { track, .. } => Some(track.title.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(started, ["第一首", "Second", "Ｔｈｉｒｄ"]);

    // History: three completed plays, newest first.
    let mut items = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    while items.len() < 3 && Instant::now() < deadline {
        if let QueryResult::History {
            items: found,
            total,
            ..
        } = core.query(Query::History {
            offset: 0,
            limit: 10,
        }) {
            assert_eq!(total as usize, found.len());
            items = found;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(items.len(), 3, "{items:?}");
    assert!(items.iter().all(|i| i.ended == EndReason::Completed));
    assert_eq!(items[0].track.title, "Ｔｈｉｒｄ");

    // Track queries: every track was played once, by the Last.fm rule; requested tags
    // come back by upper-case key.
    let by_track_no = TrackQuery {
        filter: Filter::All,
        sort: vec![SortKey {
            field: Field::TrackNo,
            descending: true,
        }],
    };
    let QueryResult::Tracks {
        tracks: rows,
        total,
        ..
    } = core.query(Query::Tracks {
        query: by_track_no.clone(),
        offset: 0,
        limit: 10,
        tags: vec!["album".into()],
    })
    else {
        panic!()
    };
    assert_eq!(total, 3);
    let titles: Vec<_> = rows.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles, ["Ｔｈｉｒｄ", "Second", "第一首"]);
    assert!(rows.iter().all(|r| r.play_count == 1), "{rows:?}");
    assert_eq!(
        rows[0].tags.get("ALBUM"),
        Some(&vec!["测试专辑".to_owned()])
    );

    // One level of a tree: the artists, with counts.
    let QueryResult::Facet { values, total, .. } = core.query(Query::Facet {
        field: Field::Artist,
        filter: Filter::All,
        offset: 0,
        limit: 10,
    }) else {
        panic!()
    };
    assert_eq!(total, 1);
    assert_eq!(values[0].track_count, 3);

    // A comparison of the wrong type is rejected as invalid, naming the field.
    let QueryResult::Err { error } = core.query(Query::Tracks {
        query: TrackQuery {
            filter: Filter::Compare {
                field: Field::Rating,
                op: CompareOp::Gt,
                value: Value::Text("x".into()),
            },
            sort: Vec::new(),
        },
        offset: 0,
        limit: 10,
        tags: Vec::new(),
    }) else {
        panic!()
    };
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::Invalid, ErrorCode::InvalidQuery)
    );
    assert!(error.message.to_lowercase().contains("rating"), "{error:?}");

    // Rate one track; it is announced, and a query context plays just the rated ones.
    let second = rows[1].id;
    run(Command::SetRating {
        tracks: vec![second],
        rating: Some(4),
    });
    wait_for(
        &|e| matches!(e, Event::TracksChanged { tracks } if tracks == &[second]),
        false,
    );
    let created = run(Command::Play {
        context: PlayContext::Query {
            query: TrackQuery {
                filter: Filter::Compare {
                    field: Field::Rating,
                    op: CompareOp::Ge,
                    value: Value::Int(4),
                },
                sort: Vec::new(),
            },
        },
        start: 0,
        start_track: None,
    })
    .created_entries;
    assert_eq!(created.len(), 1);
    wait_for(
        &|e| matches!(e, Event::TrackStarted { track, .. } if track.id == second),
        true,
    );
}

/// `AddRoot` emits `LibraryChanged` before its response (the folder list changed), and
/// again after the scan's `JobFinished`.
#[test]
fn adding_a_folder_announces_it_at_once_and_after_its_scan() {
    let (dir, core, sub, _audio, _ids) = scanned();
    let more = dir.path().join("more");
    make_album(&more);
    sub.events.try_iter().for_each(drop);
    let added = ok(execute(
        &core,
        Command::AddRoot {
            path: more.display().to_string(),
        },
        Origin::Ui,
    ));
    let job = added.job.expect("a scan");
    let before: Vec<Event> = sub.events.try_iter().map(|e| e.event).collect();
    assert!(
        before.iter().any(|e| matches!(e, Event::LibraryChanged)),
        "{before:#?}"
    );
    // Guarantee 3: the roots list shows it already.
    let QueryResult::Roots { roots } = core.query(Query::Roots) else {
        panic!()
    };
    assert_eq!(roots.len(), 2);
    wait_event(
        &sub,
        |e| matches!(e.event, Event::JobFinished { job: j, .. } if j == job),
    );
    let after = sub.events.recv_timeout(TIMEOUT).unwrap();
    assert!(matches!(after.event, Event::LibraryChanged), "{after:?}");
}

/// The query regression example uses the current `Tracks` and `Value` shapes.
#[test]
fn the_track_query_design_example_runs() {
    let (_dir, core, _sub, _audio, _ids) = scanned();
    let doc = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/track-query-example.md"),
    )
    .unwrap()
    .replace("\r\n", "\n");
    let start = doc.find("```json\n").expect("a JSON example") + "```json\n".len();
    let example = &doc[start..start + doc[start..].find("```").unwrap()];
    let query: serde_json::Value = serde_json::from_str(example).unwrap();
    let mut tracks = serde_json::json!({"type": "tracks", "limit": 10});
    tracks["query"] = query;
    let answer: serde_json::Value =
        serde_json::from_str(&core.query_json(&tracks.to_string())).unwrap();
    assert_eq!(answer["type"], "tracks", "{answer}");
}

#[test]
fn track_rows_preserves_order_duplicates_unavailable_and_missing_rules() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = dir.path().join("music");
    make_album(&music);
    let catalog = Arc::new(mpmp_library::Library::open(&dir.path().join("library.db")).unwrap());
    catalog
        .add_root(&music, mpmp_library::ActionMeta::new("ui"))
        .unwrap();
    let mpmp_library::ScanStart::Started(scan) = catalog.scan(|_| {}).unwrap() else {
        panic!()
    };
    scan.wait().unwrap();
    let ids = catalog
        .track_ids(&mpmp_library::TrackQuery::default(), 1000)
        .unwrap();
    assert_eq!(ids.len(), 3);
    std::fs::remove_file(music.join("Test Artist/测试专辑/01 a.flac")).unwrap();
    let mpmp_library::ScanStart::Started(scan) = catalog.scan(|_| {}).unwrap() else {
        panic!()
    };
    scan.wait().unwrap();
    let core = CoreHandle::start(
        Box::new(NullOutput::new(StreamFormat {
            sample_rate: 44100,
            channels: 2,
            sample_type: SampleType::F32,
        })),
        Arc::new(FileResolver),
        catalog,
        CoreConfig {
            automatic_loudness_scan: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    let wanted = vec![
        TrackId(ids[2].0),
        TrackId(i64::MAX),
        TrackId(ids[0].0),
        TrackId(ids[2].0),
        TrackId(ids[1].0),
    ];
    let QueryResult::TrackRows { tracks } = core.query(Query::TrackRows {
        tracks: wanted.clone(),
        tags: vec!["TITLE".into()],
    }) else {
        panic!()
    };
    assert_eq!(
        tracks.iter().map(|t| t.id).collect::<Vec<_>>(),
        wanted
            .into_iter()
            .filter(|id| id.0 != i64::MAX)
            .collect::<Vec<_>>()
    );
    assert_eq!(tracks.iter().filter(|t| !t.available).count(), 1);
    assert!(tracks.iter().all(|t| t.tags.contains_key("TITLE")));
    assert!(matches!(
        core.query(Query::TrackRows {
            tracks: vec![],
            tags: vec![],
        }),
        QueryResult::TrackRows { tracks } if tracks.is_empty(),
    ));
    assert!(matches!(
        core.query(Query::TrackRows {
            tracks: vec![TrackId(1); 1001],
            tags: vec![],
        }),
        QueryResult::Err { error } if error.code == ErrorCode::OutOfRange,
    ));
    assert!(matches!(
        core.query(Query::Snapshot),
        QueryResult::Snapshot { snapshot }
            if snapshot.features.iter().any(|f| f == "track_rows"),
    ));
}

#[test]
fn person_view_contract_keeps_album_group_defaults_and_track_search() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = dir.path().join("music");
    make_album(&music);
    let lib = mpmp_library::Library::open(&dir.path().join("library.db")).unwrap();
    lib.add_root(&music, mpmp_library::ActionMeta::new("ui"))
        .unwrap();
    let mpmp_library::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() else {
        panic!()
    };
    scan.wait().unwrap();
    let old = lib.query(&Query::Artists {
        view: None,
        offset: 0,
        limit: 100,
    });
    let id = lib
        .track_ids(&mpmp_library::TrackQuery::default(), 1)
        .unwrap()[0];
    lib.edit_tags(
        &[id],
        &[mpmp_library::TagChange::Set {
            key: "ARTISTS".into(),
            values: vec!["Björven".into(), "Guest".into(), "BJORVEN".into()],
        }],
        mpmp_library::ActionMeta::new("ui"),
        mpmp_library::TagJobOptions::default(),
    )
    .unwrap()
    .wait()
    .unwrap();
    assert_eq!(
        lib.query(&Query::Artists {
            view: None,
            offset: 0,
            limit: 100
        }),
        old
    );
    let QueryResult::Artists {
        artists, indexing, ..
    } = lib.query(&Query::Artists {
        view: Some(ArtistView::Person),
        offset: 0,
        limit: 100,
    })
    else {
        panic!()
    };
    assert_eq!(indexing, Some(false));
    let person = artists.iter().find(|a| a.name == "Björven").unwrap();
    assert!(person.key.as_ref().is_some_and(|k| !k.is_empty()));
    assert_eq!((person.track_count, person.album_count), (1, 1));
    let QueryResult::Tracks { tracks, total, .. } = lib.query(&Query::Tracks {
        query: TrackQuery {
            filter: Filter::ArtistPerson {
                name: "BJORVEN".into(),
            },
            sort: vec![],
        },
        offset: 0,
        limit: 100,
        tags: vec![],
    }) else {
        panic!()
    };
    assert_eq!(total, 1);
    assert_eq!(tracks[0].id, TrackId(id.0));
    assert_eq!(tracks[0].artist.as_deref(), Some("Test Artist"));
    assert_eq!(tracks[0].artists.len(), 2);
    assert_eq!(tracks[0].artists_source, Some(ArtistListSource::ArtistsTag));
    assert!(tracks[0].artists_complete);
    let QueryResult::Albums { total, .. } = lib.query(&Query::ArtistAlbums {
        view: Some(ArtistView::Person),
        name: "BJORVEN".into(),
        sort: AlbumSort::Artist,
        offset: 0,
        limit: 100,
    }) else {
        panic!()
    };
    assert_eq!(total, 1);
    let QueryResult::Search { results: person } = lib.query(&Query::Search {
        artist_view: Some(ArtistView::Person),
        text: "bjorven".into(),
        kind: None,
        offset: 0,
        limit: 100,
    }) else {
        panic!()
    };
    let QueryResult::Search { results: old } = lib.query(&Query::Search {
        artist_view: None,
        text: "bjorven".into(),
        kind: None,
        offset: 0,
        limit: 100,
    }) else {
        panic!()
    };
    assert_eq!(person.artists_total, 1);
    assert_eq!(old.artists_total, 0);
    assert_eq!(person.tracks, old.tracks);
    assert_eq!(person.albums, old.albums);
}
