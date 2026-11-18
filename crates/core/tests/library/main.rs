//! Core with the real library: add a folder, scan it, browse it, play an album gaplessly,
//! and find the plays in history.

use mpmp_test_support::client_parity;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::{Accessor, Tag, TagType};
use mpmp_core::contract::*;
use mpmp_core::{Catalog, CoreConfig, CoreHandle, SubscribeOptions};
use mpmp_engine::NullOutput;
use mpmp_engine::output::{SampleType, StreamFormat};
use mpmp_media_io::FileResolver;

use mpmp_test_support::core as support;

const TIMEOUT: Duration = Duration::from_secs(30);

fn paused_navigation_library_scenario(resume_and_count: bool, state: PlayState) {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    make_album(&music);
    let library = Arc::new(mpmp_library::Library::open(&dir.path().join("library.db")).unwrap());
    library
        .add_root(&music, mpmp_library::ActionMeta::new("ui"))
        .unwrap();
    let mpmp_library::ScanStart::Started(scan) = library.scan(|_| {}).unwrap() else {
        panic!()
    };
    scan.wait().unwrap();
    let output = support::null_output();
    let audio = output.handle();
    let core = CoreHandle::start(
        Box::new(output),
        Arc::new(FileResolver),
        library.clone(),
        CoreConfig {
            automatic_loudness_scan: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    let sub = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    let mut id = 0;
    let mut run = |command| {
        id += 1;
        let CommandResult::Ok(receipt) = core.execute(Request::new(id, command), Origin::Ui).result
        else {
            panic!()
        };
        receipt
    };
    let entries = run(Command::Play {
        context: PlayContext::Query {
            query: TrackQuery {
                filter: Filter::All,
                sort: vec![SortKey {
                    field: Field::TrackNo,
                    descending: false,
                }],
            },
        },
        start: 0,
        start_track: None,
    })
    .created_entries;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if sub
            .events
            .try_iter()
            .any(|e| matches!(e.event, Event::TrackStarted { .. }))
        {
            break;
        }
        assert!(Instant::now() < deadline);
        audio.render_f32(128);
        std::thread::yield_now();
    }
    run(if state == PlayState::Stopped {
        Command::Stop
    } else {
        Command::Pause
    });
    run(Command::Next);
    run(Command::Next);
    let QueryResult::Snapshot { snapshot } = core.query(Query::Snapshot) else {
        panic!()
    };
    let selected = snapshot.current.unwrap();
    assert_eq!(selected.entry, entries[2]);
    assert_eq!(snapshot.state, state);
    assert_eq!(selected.track.play_count, 0);
    let threshold = selected.track.duration_ms.unwrap() / 2;
    if resume_and_count {
        run(Command::Resume);
        let deadline = Instant::now() + TIMEOUT;
        let mut started = false;
        loop {
            audio.render_f32(256);
            // A no-op command publishes a fresh engine position before its receipt.
            run(Command::SetMuted { muted: false });
            started |= sub.events.try_iter().any(|e| {
                matches!(e.event,
                Event::TrackStarted { entry, .. } if entry == selected.entry)
            });
            let QueryResult::Snapshot { snapshot } = core.query(Query::Snapshot) else {
                panic!()
            };
            assert_eq!(snapshot.current.unwrap().entry, selected.entry);
            assert_eq!(snapshot.state, PlayState::Playing);
            if started && snapshot.position.unwrap().position_ms >= threshold + 50 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "resumed track did not reach its heard threshold"
            );
            std::thread::yield_now();
        }
        // End the resumed C as Skipped by selecting another entry before shutting down.
        run(Command::Previous);
        let QueryResult::Snapshot { snapshot } = core.query(Query::Snapshot) else {
            panic!()
        };
        assert_eq!(snapshot.current.unwrap().entry, entries[1]);
    }
    run(Command::Stop);
    assert!(core.shutdown_and_wait(TIMEOUT));
    // Closing the writer is a deterministic barrier for queued play records.
    drop(core);
    drop(library);
    let library = mpmp_library::Library::open(&dir.path().join("library.db")).unwrap();
    let history = library
        .history(mpmp_library::Page {
            offset: 0,
            limit: 10,
        })
        .unwrap();
    if resume_and_count {
        let plays: Vec<_> = history
            .items
            .iter()
            .filter(|p| p.event.track_id.0 == selected.track.id.0)
            .collect();
        assert_eq!(plays.len(), 1);
        assert_eq!(
            plays[0].event.ended_reason,
            mpmp_library::EndedReason::Skipped
        );
        assert!(plays[0].event.played_ms >= threshold);
    } else {
        assert_eq!(history.total, 1);
        assert_eq!(
            history.items[0].event.ended_reason,
            if state == PlayState::Stopped {
                mpmp_library::EndedReason::Stopped
            } else {
                mpmp_library::EndedReason::Skipped
            }
        );
    }
    let core_catalog: &dyn Catalog = &library;
    let QueryResult::Tracks { tracks, .. } = core_catalog.query(&Query::Tracks {
        query: TrackQuery::default(),
        offset: 0,
        limit: 10,
        tags: Vec::new(),
    }) else {
        panic!()
    };
    assert_eq!(tracks.len(), 3);
    for track in tracks {
        assert_eq!(
            track.play_count,
            u32::from(resume_and_count && track.id == selected.track.id),
            "{track:?}"
        );
    }
}

/// Copies the engine's three fixture parts into `dir` as a tagged album.
fn make_album(dir: &Path) {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures");
    let album = dir.join("Test Artist").join("测试专辑");
    std::fs::create_dir_all(&album).unwrap();
    for (no, part, title) in [
        (1, "a", "第一首"),
        (2, "b", "Second"),
        (3, "c", "Ｔｈｉｒｄ"),
    ] {
        let path = album.join(format!("{no:02} {part}.flac"));
        std::fs::copy(fixtures.join(format!("sine44_{part}.flac")), &path).unwrap();
        let mut file = lofty::read_from_path(&path).unwrap();
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.set_title(title.into());
        tag.set_artist("Test Artist".into());
        tag.set_album("测试专辑".into());
        tag.set_track(no);
        file.insert_tag(tag);
        file.save_to_path(&path, WriteOptions::default()).unwrap();
    }
}

fn execute(core: &CoreHandle, command: Command, origin: Origin) -> CommandResult {
    core.execute(Request::new(1, command), origin).result
}

fn receipt(result: CommandResult) -> PlaylistReceipt {
    match result {
        CommandResult::Ok(Receipt {
            playlist: Some(receipt),
            ..
        }) => receipt,
        other => panic!("{other:?}"),
    }
}

fn error(result: CommandResult) -> ContractError {
    match result {
        CommandResult::Err { error } => error,
        other => panic!("{other:?}"),
    }
}

/// A core over a scanned library of the test album: (core, subscription, audio, track
/// ids in track order, data dir).
fn scanned() -> (
    CoreHandle,
    mpmp_core::Subscription,
    mpmp_engine::NullOutputHandle,
    Vec<TrackId>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
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
    support::wait_for_output_initialization(&core);
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
    // `AddRoot` emits `LibraryChanged` at once; the tracks are there with `JobFinished`.
    let deadline = Instant::now() + TIMEOUT;
    while !sub
        .events
        .try_iter()
        .any(|e| matches!(e.event, Event::JobFinished { .. }))
    {
        assert!(Instant::now() < deadline, "scan never finished");
        std::thread::sleep(Duration::from_millis(10));
    }
    let QueryResult::Tracks { tracks, .. } = core.query(Query::Tracks {
        query: TrackQuery {
            filter: Filter::All,
            sort: vec![SortKey {
                field: Field::TrackNo,
                descending: false,
            }],
        },
        offset: 0,
        limit: 10,
        tags: Vec::new(),
    }) else {
        panic!()
    };
    let ids: Vec<TrackId> = tracks.iter().map(|t| t.id).collect();
    assert_eq!(ids.len(), 3);
    (core, sub, audio, ids, dir)
}

/// Waits for an event matching `pred` (without rendering audio): its envelope.
fn wait_event(
    sub: &mpmp_core::Subscription,
    mut pred: impl FnMut(&EventEnvelope) -> bool,
) -> EventEnvelope {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        assert!(Instant::now() < deadline, "event never came");
        if let Ok(envelope) = sub.events.recv_timeout(Duration::from_millis(20))
            && pred(&envelope)
        {
            return envelope;
        }
    }
}

use support::ok;

fn actions(core: &CoreHandle, origin: Option<OriginFilter>) -> (Vec<ActionSummary>, u32) {
    match core.query(Query::Actions {
        origin,
        offset: 0,
        limit: 100,
    }) {
        QueryResult::Actions { actions, total, .. } => (actions, total),
        other => panic!("{other:?}"),
    }
}

fn ratings(core: &CoreHandle, ids: &[TrackId]) -> Vec<Option<u8>> {
    let QueryResult::Tracks { tracks, .. } = core.query(Query::Tracks {
        query: TrackQuery::default(),
        offset: 0,
        limit: 100,
        tags: Vec::new(),
    }) else {
        panic!()
    };
    ids.iter()
        .map(|id| tracks.iter().find(|t| t.id == *id).unwrap().rating)
        .collect()
}

// Query validation and unavailable media handling.

// --- CUE sheets ---

/// Sample `i` of the CUE image (both channels, the right one negated): no two windows
/// alike, so an output can be lined up with it exactly.
fn image_sample(i: usize) -> i16 {
    (((i as u32).wrapping_mul(2_654_435_761) >> 17) as i32 - 16_384) as i16
}

const IMAGE_FRAMES: usize = 3 * 44_100;

/// A 3 s WAV image `Live.wav` with a sheet of three 1 s tracks, under `music/`.
fn make_cue_album(music: &Path) {
    let album = music.join("Band").join("Live");
    std::fs::create_dir_all(&album).unwrap();
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = hound::WavWriter::create(album.join("Live.wav"), spec).unwrap();
    for i in 0..IMAGE_FRAMES {
        wav.write_sample(image_sample(i)).unwrap();
        wav.write_sample(-image_sample(i)).unwrap();
    }
    wav.finalize().unwrap();
    std::fs::write(
        album.join("Live.cue"),
        "PERFORMER \"Band\"\r\nTITLE \"Live\"\r\nFILE \"Live.wav\" WAVE\r\n\
         TRACK 01 AUDIO\r\nTITLE \"Intro\"\r\nINDEX 01 00:00:00\r\n\
         TRACK 02 AUDIO\r\nTITLE \"Song\"\r\nINDEX 00 00:00:50\r\nINDEX 01 00:01:00\r\n\
         TRACK 03 AUDIO\r\nTITLE \"Outro\"\r\nINDEX 01 00:02:00\r\n",
    )
    .unwrap();
}

fn snapshot_features(core: &CoreHandle) -> Vec<String> {
    match core.query(Query::Snapshot) {
        QueryResult::Snapshot { snapshot } => snapshot.features,
        other => panic!("{other:?}"),
    }
}

mod playback;

mod catalog;

mod edits;

mod permissions;

mod media;
