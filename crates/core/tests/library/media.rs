//! Media scenarios.
use super::*;

#[test]
fn lyrics_come_from_a_lrc_file_or_the_tags() {
    let (core, _sub, _audio, ids, dir) = scanned();
    let album = dir.path().join("music/Test Artist/测试专辑");
    let lyrics = |track: TrackId| match core.query(Query::Lyrics { track }) {
        QueryResult::Lyrics { lyrics } => lyrics,
        other => panic!("{other:?}"),
    };
    let json = |track: TrackId| -> serde_json::Value {
        let query = format!(r#"{{"type":"lyrics","track":{}}}"#, track.0);
        serde_json::from_str(&core.query_json(&query)).unwrap()
    };

    // None yet.
    assert_eq!(lyrics(ids[0]), None);
    assert_eq!(
        json(ids[0]),
        serde_json::json!({"type": "lyrics", "lyrics": null})
    );

    // A.lrc file next to the first track, with a translation and word timing.
    std::fs::write(
        album.join("01 a.lrc"),
        "\u{feff}[ti:第一首]\n[offset:+100]\n[00:01.00]<00:01.00>你<00:01.50>好\n[00:01.00]Hello\n[00:03.50]再见\n",
    )
    .unwrap();
    let found = lyrics(ids[0]).unwrap();
    assert_eq!(found.source, LyricsSource::File);
    assert_eq!(found.format, LyricsFormat::EnhancedLrc);
    assert_eq!(found.lines.len(), 2);
    assert_eq!(found.lines[0].start_ms, Some(900));
    assert_eq!(found.lines[0].text, "你好");
    assert_eq!(found.lines[0].translation.as_deref(), Some("Hello"));
    assert_eq!(
        found.lines[0].words,
        [
            LyricsWord {
                start_ms: 900,
                text: "你".into()
            },
            LyricsWord {
                start_ms: 1400,
                text: "好".into()
            }
        ]
    );
    assert_eq!(found.lines[1].start_ms, Some(3400));
    assert!(found.raw.starts_with("[ti:第一首]"));
    // On the wire: empty words and a missing translation are left out.
    let wire = json(ids[0]);
    assert_eq!(wire["lyrics"]["source"], "file");
    assert_eq!(wire["lyrics"]["format"], "enhanced_lrc");
    assert_eq!(
        wire["lyrics"]["lines"][1],
        serde_json::json!({"start_ms": 3400, "text": "再见"})
    );

    // Lyrics in the tags of the second track: plain text.
    let path = album.join("02 b.flac");
    let mut file = lofty::read_from_path(&path).unwrap();
    file.primary_tag_mut()
        .unwrap()
        .insert_text(lofty::tag::ItemKey::Lyrics, "la la\nla".into());
    file.save_to_path(&path, WriteOptions::default()).unwrap();
    let found = lyrics(ids[1]).unwrap();
    assert_eq!(found.source, LyricsSource::Embedded);
    assert_eq!(found.format, LyricsFormat::Plain);
    let texts: Vec<_> = found.lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, ["la la", "la"]);
    assert!(found.lines.iter().all(|l| l.start_ms.is_none()));

    // An unknown track is an error, not "no lyrics".
    let QueryResult::Err { error } = core.query(Query::Lyrics {
        track: TrackId(424_242),
    }) else {
        panic!()
    };
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::NotFound, ErrorCode::TrackNotFound)
    );
    assert_eq!(
        error.details.missing,
        [ObjectRef::Track {
            id: TrackId(424_242)
        }]
    );
}

#[test]
fn waveforms_are_computed_in_the_background_and_announced() {
    let (core, sub, _audio, ids, dir) = scanned();
    let album = dir.path().join("music/Test Artist/测试专辑");
    let waveform = |track: TrackId| match core.query(Query::Waveform { track }) {
        QueryResult::Waveform { waveform } => waveform,
        other => panic!("{other:?}"),
    };
    let ready = |envelope: &EventEnvelope| match envelope.event {
        Event::WaveformReady { track } => Some(track),
        _ => None,
    };

    // The second file no longer decodes (the library has not noticed: no rescan).
    std::fs::write(album.join("02 b.flac"), b"not audio".repeat(100)).unwrap();

    // Not computed yet: `null`, and queued; on the wire too.
    assert_eq!(waveform(ids[1]), None);
    let query = format!(r#"{{"type":"waveform","track":{}}}"#, ids[2].0);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&core.query_json(&query)).unwrap(),
        serde_json::json!({"type": "waveform", "waveform": null})
    );
    // Asked for in that order: once the third is announced, the second has failed.
    let envelope = wait_event(&sub, |e| ready(e).is_some());
    assert_eq!(ready(&envelope), Some(ids[2]), "{envelope:?}");
    assert_eq!(
        envelope.origin, None,
        "computed in the background (guarantee 5)"
    );
    let third = waveform(ids[2]).expect("stored");
    assert_eq!(third.buckets, 2048);
    assert_eq!(third.channels.len(), 2);
    for channel in &third.channels {
        assert_eq!(channel.min.len(), 2048);
        assert_eq!(channel.max.len(), 2048);
        assert_eq!(channel.rms.len(), 2048);
        // The fixture signal is a sine at half scale.
        assert_eq!(*channel.max.iter().max().unwrap(), 64);
        assert_eq!(*channel.min.iter().min().unwrap(), -64);
    }
    // The cache is next to the database.
    let cached: Vec<_> = std::fs::read_dir(dir.path().join("waveforms"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(cached.len(), 1, "{cached:?}");
    assert!(cached[0].starts_with(&format!("{}-", ids[2].0)));
    let wire: serde_json::Value = serde_json::from_str(&core.query_json(&query)).unwrap();
    assert_eq!(wire["waveform"]["buckets"], 2048);
    assert_eq!(
        wire["waveform"]["channels"][0]["max"]
            .as_array()
            .unwrap()
            .len(),
        2048
    );
    // The file that does not decode stays `null`; asking again does not retry it.
    assert_eq!(waveform(ids[1]), None);

    // The track being heard is computed without being asked for.
    execute(
        &core,
        Command::Play {
            context: PlayContext::Tracks {
                tracks: vec![ids[0]],
            },
            start: 0,
            start_track: None,
        },
        Origin::Ui,
    );
    let envelope = wait_event(&sub, |e| ready(e).is_some());
    assert_eq!(ready(&envelope), Some(ids[0]), "{envelope:?}");
    assert_eq!(envelope.origin, None);
    assert!(waveform(ids[0]).is_some());
    // Nothing was announced for the broken file.
    std::thread::sleep(Duration::from_millis(200));
    assert!(sub.events.try_iter().all(|e| ready(&e) != Some(ids[1])));

    // An unknown track is an error, not "not yet".
    let QueryResult::Err { error } = core.query(Query::Waveform {
        track: TrackId(424_242),
    }) else {
        panic!()
    };
    assert_eq!(
        (error.kind, error.code),
        (ErrorKind::NotFound, ErrorCode::TrackNotFound)
    );
}
