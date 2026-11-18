//! Playback scenarios.
use super::*;
use mpmp_core::catalog::Browse;

#[test]
fn restored_current_metadata_and_cue_identity_refresh_after_scan() {
    const UPDATE_TRACKS: &str = r#"
UPDATE tracks SET title='CUE track 5',artist='Old artist',cue_sheet=?2,cue_number=1,cue_start=0,cue_end=NULL
WHERE id=?1
"#;

    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    make_album(&music);
    let album = music.join("Test Artist").join("测试专辑");
    let file = album.join("01 a.flac");
    let name = file.file_name().unwrap().to_str().unwrap();
    std::fs::write(
        album.join("EAC.cue"),
        format!("FILE \"{name}\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n"),
    )
    .unwrap();
    let database = dir.path().join("library.db");
    let lib = mpmp_library::Library::open(&database).unwrap();
    lib.add_root(&music, mpmp_library::ActionMeta::new("ui"))
        .unwrap();
    let mpmp_library::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() else {
        panic!()
    };
    scan.wait().unwrap();
    let id = lib
        .tracks(
            &lib.track_ids(&mpmp_library::TrackQuery::default(), 10)
                .unwrap(),
        )
        .unwrap()
        .into_iter()
        .find(|track| track.rel_path.ends_with(name))
        .unwrap()
        .id;
    drop(lib);
    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.execute(
        UPDATE_TRACKS,
        rusqlite::params![id.0, "Test Artist/测试专辑/EAC.cue"],
    )
    .unwrap();
    drop(conn);
    let state = dir.path().join("state.json");
    std::fs::write(
        &state,
        serde_json::json!({
        "version":1,
        "queue":{
        "entries":[{
        "id":42,
        "track":id.0}
        ],
        "current_index":0}
        }
        )
        .to_string(),
    )
    .unwrap();

    let lib = Arc::new(mpmp_library::Library::open(&database).unwrap());
    let core = CoreHandle::start(
        Box::new(NullOutput::new(StreamFormat {
            sample_rate: 44100,
            channels: 2,
            sample_type: SampleType::F32,
        })),
        Arc::new(FileResolver),
        lib.clone(),
        CoreConfig {
            startup_scan_by_host: true,
            automatic_loudness_scan: Some(false),
            state_file: Some(state),
            ..Default::default()
        },
    )
    .unwrap();
    let sub = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    let old = sub.snapshot.current.unwrap();
    assert_eq!(old.track.title, "CUE track 5");
    assert!(old.track.cue.is_some());
    for pass in 0..2 {
        if pass == 1 {
            let mut audio = lofty::read_from_path(&file).unwrap();
            let tag = audio.primary_tag_mut().unwrap();
            tag.set_title("New file title".into());
            tag.set_artist("New artist".into());
            audio.save_to_path(&file, WriteOptions::default()).unwrap();
        }
        let CommandResult::Ok(receipt) = execute(&core, Command::Rescan, Origin::Ui) else {
            panic!()
        };
        let deadline = Instant::now() + TIMEOUT;
        let mut refreshed = false;
        loop {
            let envelope = sub
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("scan did not finish before deadline");
            if let Event::StateChanged { snapshot } = &envelope.event
                && let Some(current) = &snapshot.current
            {
                refreshed |= current.track.id.0 == id.0
                    && current.track.cue.is_none()
                    && current.track.title != "CUE track 5";
            }
            if matches!(envelope.event,Event::JobFinished {job,..} if Some(job)==receipt.job) {
                break;
            }
        }
        assert!(
            refreshed,
            "scan must publish the refreshed current track before JobFinished"
        );
        // Discovery queries are actor barriers; snapshot reads use the published copy.
        let _ = core.query(Query::OutputDevices);
        let QueryResult::Snapshot { snapshot } = core.query(Query::Snapshot) else {
            panic!()
        };
        let current = snapshot.current.unwrap();
        assert_eq!(current.entry, old.entry);
        assert_eq!(current.track.id.0, id.0);
        assert!(current.track.cue.is_none());
        let expected = mpmp_core::catalog::Browse::tracks(lib.as_ref(), &[current.track.id])
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(current.track, expected);
        if pass == 1 {
            assert_eq!(current.track.title, "New file title");
            assert_eq!(current.track.artist.as_deref(), Some("New artist"));
        }
    }
    execute(
        &core,
        Command::SetRating {
            tracks: vec![TrackId(id.0)],
            rating: Some(5),
        },
        Origin::Ui,
    );
    let QueryResult::Snapshot { snapshot } = core.query(Query::Snapshot) else {
        panic!()
    };
    assert_eq!(
        snapshot.current.unwrap().track.rating,
        Some(5),
        "TracksChanged also refreshes current metadata"
    );
    assert!(core.shutdown_and_wait(TIMEOUT));
}

#[test]
fn paused_navigation_does_not_record_selections_or_increment_play_count() {
    paused_navigation_library_scenario(false, PlayState::Paused);
}

#[test]
fn paused_navigation_resume_counts_once_after_threshold_skip_and_sqlite_reopen() {
    paused_navigation_library_scenario(true, PlayState::Paused);
}

#[test]
fn stopped_navigation_does_not_record_selections_or_increment_play_count() {
    paused_navigation_library_scenario(false, PlayState::Stopped);
}

#[test]
fn stopped_navigation_resume_counts_once_after_threshold_skip_and_sqlite_reopen() {
    paused_navigation_library_scenario(true, PlayState::Stopped);
}

#[test]
fn stop_navigation_cue_resume_starts_at_the_subtrack_beginning() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    make_cue_album(&music);
    let library = Arc::new(mpmp_library::Library::open(&dir.path().join("library.db")).unwrap());
    library
        .add_root(&music, mpmp_library::ActionMeta::new("ui"))
        .unwrap();
    let mpmp_library::ScanStart::Started(scan) = library.scan(|_| {}).unwrap() else {
        panic!()
    };
    scan.wait().unwrap();
    let QueryResult::Tracks { tracks, .. } = library.as_ref().query(&Query::Tracks {
        query: TrackQuery::default(),
        offset: 0,
        limit: 10,
        tags: Vec::new(),
    }) else {
        panic!()
    };
    let song = tracks
        .into_iter()
        .find(|t| t.cue.as_ref().is_some_and(|c| c.number == 2))
        .unwrap();
    assert_eq!(song.cue.as_ref().unwrap().start_ms, 1000);
    let output = support::null_output();
    let audio = output.handle();
    let core = CoreHandle::start(
        Box::new(output),
        Arc::new(FileResolver),
        library,
        CoreConfig {
            automatic_loudness_scan: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    let run = |command| match execute(&core, command, Origin::Ui) {
        CommandResult::Ok(receipt) => receipt,
        other => panic!("{other:?}"),
    };
    let render_audio = || {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let samples = audio.render_f32(128);
            if samples.iter().any(|s| *s != 0.0) {
                return samples;
            }
            assert!(Instant::now() < deadline, "no CUE audio rendered");
            std::thread::yield_now();
        }
    };
    let entry = run(Command::Play {
        context: PlayContext::Tracks {
            tracks: vec![song.id],
        },
        start: 0,
        start_track: None,
    })
    .created_entries[0];
    render_audio();
    run(Command::Pause);
    run(Command::Seek {
        entry: Some(entry),
        position_ms: 500,
    });
    run(Command::Stop);
    let QueryResult::Snapshot { snapshot } = core.query(Query::Snapshot) else {
        panic!()
    };
    assert_eq!(snapshot.current.unwrap().entry, entry);
    let p = snapshot.position.unwrap();
    assert_eq!((p.entry, p.position_ms, p.rate), (entry, 0, 0.0));
    assert_eq!(snapshot.state, PlayState::Stopped);
    assert!(!audio.is_open());
    run(Command::Resume);
    let mut samples = render_audio();
    for _ in 0..3 {
        samples.extend(audio.render_f32(128));
    }
    // Compare a known source window beyond the existing 10 ms resume ramp.
    // Consuming exactly 512 frames binds this window to the subtrack's beginning.
    let expected: Vec<f32> = (44_100 + 448..44_100 + 512)
        .flat_map(|i| {
            let sample = f32::from(image_sample(i)) / 32_768.0;
            [sample, -sample]
        })
        .collect();
    assert_eq!(
        samples[448 * 2..],
        expected,
        "resume must start at CUE track 2, not image zero or its old offset"
    );
    assert!(core.shutdown_and_wait(TIMEOUT));
}

#[test]
fn cue_tracks_play_gaplessly_and_have_their_own_waveforms() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    make_cue_album(&music);
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
    execute(
        &core,
        Command::AddRoot {
            path: music.display().to_string(),
        },
        Origin::Ui,
    );
    wait_event(&sub, |e| matches!(e.event, Event::JobFinished { .. }));
    assert!(
        snapshot_features(&core).iter().any(|f| f == "cue"),
        "{:?}",
        snapshot_features(&core)
    );

    // The sheet's tracks, not the image.
    let QueryResult::Tracks { tracks, total, .. } = core.query(Query::Tracks {
        query: TrackQuery::default(),
        offset: 0,
        limit: 10,
        tags: Vec::new(),
    }) else {
        panic!()
    };
    assert_eq!(total, 3);
    let titles: Vec<&str> = tracks.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(titles, ["Intro", "Song", "Outro"]);
    // Their tags come from the sheet: tag editing leaves them alone (for now).
    let QueryResult::TagEditPreview { files, skipped } = core.query(Query::TagEditPreview {
        tracks: vec![tracks[1].id],
        changes: vec![TagChange::Set {
            key: "TITLE".into(),
            values: vec!["Renamed".into()],
        }],
    }) else {
        panic!()
    };
    assert!(files.is_empty(), "{files:?}");
    assert_eq!(
        skipped,
        [TagFileResult {
            track: tracks[1].id,
            code: ErrorCode::CueTrack,
        }]
    );
    for (n, track) in tracks.iter().enumerate() {
        let n = n as u64;
        assert_eq!(track.path, "Band/Live/Live.wav");
        assert_eq!(track.duration_ms, Some(1000));
        let cue = track.cue.as_ref().unwrap();
        assert_eq!(cue.sheet, "Band/Live/Live.cue");
        assert_eq!(
            (cue.number, cue.start_ms, cue.end_ms),
            (n as u32 + 1, n * 1000, Some((n + 1) * 1000))
        );
    }
    let wire: serde_json::Value = serde_json::from_str(&core.query_json(
        r#"{"type":"tracks","query":{"filter":{"type":"compare","field":{"type":"cue_sheet"},
            "op":"eq","value":{"type":"bool","value":false}}},"offset":0,"limit":10}"#,
    ))
    .unwrap();
    assert_eq!(wire["total"], 0, "{wire}");
    let ids: Vec<TrackId> = tracks.iter().map(|t| t.id).collect();

    // Tracks 2 and 3 in a row: the image's samples from 1 s to its end, without a gap
    // or a repeated frame at the seam.
    execute(
        &core,
        Command::Play {
            context: PlayContext::Tracks {
                tracks: vec![ids[1], ids[2]],
            },
            start: 0,
            start_track: None,
        },
        Origin::Ui,
    );
    let mut out: Vec<f32> = Vec::new();
    let mut started = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        assert!(Instant::now() < deadline, "playback never ended");
        // About twice real time: the decoder stays ahead, as it does of a device.
        if audio.is_open() {
            out.extend(audio.render_f32(256));
        }
        std::thread::sleep(Duration::from_millis(3));
        let mut stopped = false;
        for envelope in sub.events.try_iter() {
            match envelope.event {
                Event::TrackStarted { track, .. } => started.push(track.id),
                Event::StateChanged { snapshot } if snapshot.state == PlayState::Stopped => {
                    stopped = !started.is_empty();
                }
                _ => {}
            }
        }
        if stopped {
            break;
        }
    }
    assert_eq!(started, [ids[1], ids[2]]);
    let expected: Vec<f32> = (44_100..IMAGE_FRAMES)
        .flat_map(|i| {
            let s = f32::from(image_sample(i)) / 32_768.0;
            [s, -s]
        })
        .collect();
    // Lined up a little after the start (the first frames may fade in).
    let probe = &expected[2 * 1_000..2 * 1_100];
    let at = out
        .windows(probe.len())
        .position(|w| w == probe)
        .expect("track 2 is in the output")
        - 2 * 1_000;
    let heard = &out[at + 2 * 1_000..];
    let rest = &expected[2 * 1_000..];
    assert!(heard.len() >= rest.len(), "the output ends early");
    let first_wrong = heard.iter().zip(rest).position(|(a, b)| a != b);
    assert_eq!(first_wrong, None, "seam at frame {}", (44_100 - 1_000));
    assert!(
        heard[rest.len()..].iter().all(|&s| s == 0.0),
        "nothing after the end"
    );

    // Each track's waveform is that of its part of the image alone.
    let waveform = |track: TrackId| match core.query(Query::Waveform { track }) {
        QueryResult::Waveform { waveform } => waveform,
        other => panic!("{other:?}"),
    };
    let mut song = waveform(ids[1]);
    let deadline = Instant::now() + TIMEOUT;
    while song.is_none() {
        assert!(Instant::now() < deadline, "no waveform");
        std::thread::sleep(Duration::from_millis(20));
        song = waveform(ids[1]);
    }
    let part = dir.path().join("song.wav");
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = hound::WavWriter::create(&part, spec).unwrap();
    for i in 44_100..2 * 44_100 {
        wav.write_sample(image_sample(i)).unwrap();
        wav.write_sample(-image_sample(i)).unwrap();
    }
    wav.finalize().unwrap();
    let alone = mpmp_engine::compute_waveform(
        Arc::new(mpmp_media_io::FileSource::open(&part).unwrap()),
        Some("wav"),
        None,
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap()
    .unwrap();
    let song = song.unwrap();
    assert_eq!(song.channels.len(), alone.channels.len());
    for (got, want) in song.channels.iter().zip(&alone.channels) {
        assert_eq!(
            (&got.min, &got.max, &got.rms),
            (&want.min, &want.max, &want.rms)
        );
    }

    // The plays are counted per sheet track.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let QueryResult::Tracks { tracks, .. } = core.query(Query::Tracks {
            query: TrackQuery::default(),
            offset: 0,
            limit: 10,
            tags: Vec::new(),
        }) else {
            panic!()
        };
        let counts: Vec<u32> = tracks.iter().map(|t| t.play_count).collect();
        if counts == [0, 1, 1] {
            break;
        }
        assert!(Instant::now() < deadline, "plays: {counts:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Shared commands and observations are also run against the SDK mock. All playlist
/// resolution here goes through the real actor and SQLite, without rendering audio.
#[test]
fn guarded_playback_traces_match_sdk_mock() {
    use serde_json::{Value, json};
    let (core, _sub, _audio, ids, _dir) = scanned();
    let create = |kind| {
        receipt(execute(
            &core,
            Command::CreatePlaylist {
                name: "guard".into(),
                kind,
            },
            Origin::Ui,
        ))
    };
    let playlist = create(PlaylistKind::Manual).playlist;
    let added = receipt(execute(
        &core,
        Command::AddToPlaylist {
            playlist,
            tracks: vec![ids[0], ids[1], ids[0]],
            index: None,
            expected_revision: None,
        },
        Origin::Ui,
    ));
    let other = create(PlaylistKind::Manual).playlist;
    let other_item = receipt(execute(
        &core,
        Command::AddToPlaylist {
            playlist: other,
            tracks: vec![ids[0]],
            index: None,
            expected_revision: None,
        },
        Origin::Ui,
    ))
    .created_items[0];
    let smart = create(PlaylistKind::Smart {
        query: TrackQuery::default(),
        max_tracks: None,
    })
    .playlist;
    let initial = ok(execute(
        &core,
        Command::Play {
            context: PlayContext::Tracks {
                tracks: vec![ids[0], ids[1]],
            },
            start: 0,
            start_track: None,
        },
        Origin::Ui,
    ));
    let QueryResult::Roots { roots } = core.query(Query::Roots) else {
        panic!()
    };
    let mut bindings = json!({"$playlist":playlist,"$playlist_revision":added.revision,
        "$latest_playlist_revision":added.revision,
        "$item0":added.created_items[0],
        "$item2":added.created_items[2],

        "$track0":ids[0],"$track1":ids[1],"$track2":ids[2],"$other_item":other_item,"$smart":smart,
        "$entry0":initial.created_entries[0],"$entry1":initial.created_entries[1],
        "$queue_revision":initial.queue_revision,"$root":roots[0].id});
    fn resolve(value: &mut Value, bindings: &Value) {
        match value {
            Value::String(key) if key.starts_with('$') => *value = bindings[key.as_str()].clone(),
            Value::Object(object) => object.values_mut().for_each(|v| resolve(v, bindings)),
            Value::Array(array) => array.iter_mut().for_each(|v| resolve(v, bindings)),
            _ => {}
        }
    }
    let path = client_parity::core_crate_path(env!("CARGO_MANIFEST_DIR"), "mock-state-cases.json");
    let mut corpus: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let update = std::env::var_os("UPDATE_MOCK_GUARD_CASES").is_some();
    for step in corpus["guards"].as_array_mut().unwrap() {
        let mut command = step["command"].clone();
        resolve(&mut command, &bindings);
        // Guard rejection must preserve queue identity/order/revision. Track metadata
        // is freshly read and may acquire asynchronous skip/play statistics meanwhile.
        let queue_shape = |page: &QueryResult| {
            let QueryResult::QueuePage {
                revision,
                offset,
                total,
                next_offset,
                entries,
            } = page
            else {
                panic!()
            };
            json!({"revision":revision,"offset":offset,"total":total,"next_offset":next_offset,
                "entries":entries.iter().map(|e| (e.entry,e.track.id)).collect::<Vec<_>>()})
        };
        let before = queue_shape(&core.query(Query::QueuePage {
            offset: 0,
            limit: 1000,
        }));
        let result = execute(&core, serde_json::from_value(command).unwrap(), Origin::Ui);
        let error = match result {
            CommandResult::Err { error } => json!({"kind":error.kind,"code":error.code}),
            CommandResult::Ok(receipt) => {
                if let Some(p) = receipt.playlist {
                    bindings["$latest_playlist_revision"] = json!(p.revision);
                }
                Value::Null
            }
            _ => panic!(),
        };
        let page = core.query(Query::QueuePage {
            offset: 0,
            limit: 1000,
        });
        if !error.is_null() {
            assert_eq!(queue_shape(&page), before, "{}", step["name"]);
        }
        let QueryResult::QueuePage { entries, .. } = page else {
            panic!()
        };
        let QueryResult::Snapshot { snapshot } = core.query(Query::Snapshot) else {
            panic!()
        };
        let current_index = snapshot.queue.current_index;
        let actual = json!({"error":error,"current_index":current_index,
                "queue":entries
        .iter().map(|e| ids.iter().position(|id| *id == e.track.id).unwrap()).collect::<Vec<_>>()}
                );

        if update {
            step["expected"] = actual;
        } else {
            assert_eq!(actual, step["expected"], "{}", step["name"]);
        }
    }
    if update {
        std::fs::write(path, serde_json::to_string_pretty(&corpus).unwrap() + "\n").unwrap();
    }
}
