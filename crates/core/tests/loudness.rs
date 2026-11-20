//! Library-backed scans through core, with NullOutput only. A gated resolver makes
//! cancellation/file-change races deterministic without long media or timing sleeps.
mod support;

use crossbeam_channel::{Receiver, Sender};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::{Accessor, Tag, TagType};
use mpmp_core::contract::*;
use mpmp_core::{CoreConfig, CoreHandle, SubscribeOptions, Subscription};
use mpmp_media_io::{FileResolver, MediaUri, ResolvedSource, SourceResolver};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

struct Gate {
    entered: Sender<()>,
    release: Receiver<()>,
}

/// Analysis decodes: counted (`.1`), by file name (`.3`); `.2`: how many more times a
/// file name fails to open on the analysis threads.
#[derive(Default)]
struct Resolver(
    Mutex<Option<Gate>>,
    std::sync::atomic::AtomicUsize,
    Mutex<std::collections::HashMap<String, usize>>,
    Mutex<std::collections::HashMap<String, usize>>,
);

impl Resolver {
    fn fail(&self, name: &str, times: usize) {
        self.2.lock().unwrap().insert(name.into(), times);
    }
    fn attempts(&self, name: &str) -> usize {
        self.3.lock().unwrap().get(name).copied().unwrap_or(0)
    }
}

impl SourceResolver for Resolver {
    fn resolve(&self, uri: &MediaUri) -> Result<ResolvedSource, mpmp_media_io::Error> {
        if matches!(
            std::thread::current().name(),
            Some("core-loudness" | "core-waveforms")
        ) {
            self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = uri.to_file_path().unwrap();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            *self.3.lock().unwrap().entry(name.clone()).or_default() += 1;
            if let Some(left) = self.2.lock().unwrap().get_mut(&name)
                && *left > 0
            {
                *left -= 1;
                return Err(mpmp_media_io::Error::Unsupported("failing on purpose"));
            }
        }
        if std::thread::current().name() == Some("core-loudness") {
            let gate = self.0.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.entered.send(()).unwrap();
                gate.release.recv_timeout(TIMEOUT).unwrap();
            }
        }
        FileResolver.resolve(uri)
    }
}

struct Fixture {
    audio: mpmp_engine::NullOutputHandle,
    core: CoreHandle,
    sub: Subscription,
    lib: mpmp_library::Library,
    music: PathBuf,
    ids: Vec<TrackId>,
    resolver: Arc<Resolver>,
    _dir: mpmp_test_support::TempDir,
}

use support::run;

fn ok(result: CommandResult) -> Receipt {
    match result {
        CommandResult::Ok(r) => r,
        other => panic!("{other:?}"),
    }
}

fn wait(sub: &Subscription, predicate: impl Fn(&EventEnvelope) -> bool) -> EventEnvelope {
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        assert!(std::time::Instant::now() < deadline, "event missing");
        if let Ok(e) = sub.events.recv_timeout(Duration::from_millis(20))
            && predicate(&e)
        {
            return e;
        }
    }
}

fn finished(f: &Fixture, job: JobId) -> EventEnvelope {
    wait(
        &f.sub,
        |e| matches!(e.event,Event::JobFinished{job:id,..} if id==job),
    )
}

fn fixture() -> Fixture {
    fixture_with_auto(false)
}

fn fixture_with_auto(automatic: bool) -> Fixture {
    fixture_with(automatic, Resolver::default(), None)
}

fn fixture_with(automatic: bool, resolver: Resolver, retry: Option<Duration>) -> Fixture {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures");
    for part in ["a", "b", "c"] {
        let path = music.join(format!("{part}.flac"));
        std::fs::copy(source.join(format!("sine44_{part}.flac")), &path).unwrap();
        let mut file = lofty::read_from_path(&path).unwrap();
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.set_title(part.into());
        tag.set_artist("Artist".into());
        tag.set_album("Album".into());
        file.insert_tag(tag);
        file.save_to_path(&path, Default::default()).unwrap();
    }
    let lib = mpmp_library::Library::open(&dir.path().join("library.db")).unwrap();
    let resolver = Arc::new(resolver);
    let output = support::null_output();
    let audio = output.handle();
    let core = CoreHandle::start(
        Box::new(output),
        resolver.clone(),
        Arc::new(lib.clone()),
        CoreConfig {
            automatic_loudness_scan: (!automatic).then_some(false),
            automatic_loudness_retry: retry,
            ..Default::default()
        },
    )
    .unwrap();
    let sub = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    let root = ok(run(
        &core,
        Command::AddRoot {
            path: music.to_string_lossy().into_owned(),
        },
        Origin::Ui,
    ));
    wait(
        &sub,
        |e| matches!(e.event,Event::JobFinished{job,..} if Some(job)==root.job),
    );
    wait(&sub, |e| matches!(e.event, Event::LibraryChanged));
    let QueryResult::Tracks { tracks, .. } = core.query(Query::Tracks {
        query: TrackQuery::default(),
        offset: 0,
        limit: 100,
        tags: vec![],
    }) else {
        panic!()
    };
    let mut tracks = tracks;
    tracks.sort_by(|a, b| a.path.cmp(&b.path));
    Fixture {
        audio,
        core,
        sub,
        lib,
        music,
        ids: tracks.into_iter().map(|t| t.id).collect(),
        resolver,
        _dir: dir,
    }
}

fn gate(f: &Fixture) -> (Receiver<()>, Sender<()>) {
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    *f.resolver.0.lock().unwrap() = Some(Gate {
        entered: entered_tx,
        release: release_rx,
    });
    (entered_rx, release_tx)
}

fn data(f: &Fixture, id: TrackId) -> Option<LoudnessInfo> {
    match f.core.query(Query::Loudness { track: id }) {
        QueryResult::Loudness { loudness } => loudness,
        other => panic!("{other:?}"),
    }
}

fn automatic_finished(f: &Fixture) -> EventEnvelope {
    wait(&f.sub, |e| {
        e.origin.is_none()
            && matches!(
                e.event,
                Event::JobFinished {
                    kind: JobKind::LoudnessScan,
                    ..
                }
            )
    })
}

#[test]
fn default_automatically_measures_imports_and_reuses_unchanged_album_members() {
    let f = fixture_with_auto(true);
    let event = automatic_finished(&f);
    assert!(
        matches!(
            event.event,
            Event::JobFinished {
                outcome: JobOutcome::Completed {
                    report: JobReport::LoudnessScan {
                        measured_count: 3,
                        album_count: 1,
                        ..
                    }
                },
                ..
            }
        ),
        "{event:?}"
    );
    // The same catalog read carries the provenance of the effective gain pairs.
    let QueryResult::TrackRows { tracks } = f.core.query(Query::TrackRows {
        tracks: f.ids.clone(),
        tags: vec![],
    }) else {
        panic!()
    };
    for track in tracks {
        let rg = track.replay_gain.unwrap();
        assert_eq!(rg.track_source, GainDataSource::Automatic);
        assert_eq!(rg.album_source, GainDataSource::Automatic);
    }
    let before: Vec<_> = f.ids.iter().map(|id| data(&f, *id).unwrap()).collect();
    for value in &before {
        assert!(value.album_lufs.is_some());
    }
    let calls = f.resolver.1.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(calls, 3);
    // A scan that finds everything as it was gives no pass anything to do: none runs (the
    // next pass seen below is the one for the edit, which measures one track).
    let job = ok(run(&f.core, Command::Rescan, Origin::Ui)).job.unwrap();
    finished(&f, job);
    assert_eq!(
        f.resolver.1.load(std::sync::atomic::Ordering::Relaxed),
        calls
    );
    // Editing one valid FLAC changes its version. Its siblings retain their statistics.
    let path = f.music.join("a.flac");
    let mut file = lofty::read_from_path(&path).unwrap();
    file.primary_tag_mut()
        .unwrap()
        .set_title("changed title".into());
    file.save_to_path(&path, Default::default()).unwrap();
    let job = ok(run(&f.core, Command::Rescan, Origin::Ui)).job.unwrap();
    finished(&f, job);
    let event = automatic_finished(&f);
    assert!(
        matches!(
            event.event,
            Event::JobFinished {
                outcome: JobOutcome::Completed {
                    report: JobReport::LoudnessScan {
                        measured_count: 1,
                        album_count: 1,
                        ..
                    }
                },
                ..
            }
        ),
        "{event:?}"
    );
    assert_eq!(
        f.resolver.1.load(std::sync::atomic::Ordering::Relaxed),
        calls + 1
    );
    assert_eq!(
        data(&f, f.ids[1]).unwrap().measured_at_ms,
        before[1].measured_at_ms
    );
    assert_eq!(
        data(&f, f.ids[2]).unwrap().measured_at_ms,
        before[2].measured_at_ms
    );
}

#[test]
fn restart_resumes_from_persistent_statistics_without_decoding_cached_tracks() {
    let f = fixture_with_auto(true);
    automatic_finished(&f);
    let before: Vec<_> = f.ids.iter().map(|id| data(&f, *id)).collect();
    let calls = f.resolver.1.load(std::sync::atomic::Ordering::Relaxed);
    drop(f.core);
    let core = CoreHandle::start(
        Box::new(support::null_output()),
        f.resolver.clone(),
        Arc::new(f.lib.clone()),
        CoreConfig::default(),
    )
    .unwrap();
    let sub = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    // A new file gives a pass work after subscribing, also when the startup pass ended
    // before: only it is decoded.
    // Untagged: in no album, so the others' album values stay as they are.
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures");
    std::fs::copy(source.join("sine44_c.flac"), f.music.join("d.flac")).unwrap();
    let ticket = ok(run(&core, Command::Rescan, Origin::Ui));
    wait(&sub, |e| {
        matches!(
            e.event,
            Event::JobFinished {
                job,
                kind: JobKind::Scan,
                ..
            } if Some(job) == ticket.job,
        )
    });
    wait(&sub, |e| {
        e.origin.is_none()
            && matches!(
                e.event,
                Event::JobFinished {
                    kind: JobKind::LoudnessScan,
                    outcome: JobOutcome::Completed {
                        report: JobReport::LoudnessScan {
                            measured_count: 1,
                            ..
                        }
                    },
                    ..
                }
            )
    });
    for (id, before) in f.ids.iter().zip(before) {
        assert_eq!(
            core.query(Query::Loudness { track: *id }),
            QueryResult::Loudness { loudness: before }
        );
    }
    assert_eq!(
        f.resolver.1.load(std::sync::atomic::Ordering::Relaxed),
        calls + 1
    );
}

#[test]
fn concurrent_waveform_request_shares_automatic_decode() {
    let f = fixture();
    drop(f.core);
    let (entered_tx, entered) = crossbeam_channel::bounded(1);
    let (release, release_rx) = crossbeam_channel::bounded(1);
    *f.resolver.0.lock().unwrap() = Some(Gate {
        entered: entered_tx,
        release: release_rx,
    });
    let core = CoreHandle::start(
        Box::new(support::null_output()),
        f.resolver.clone(),
        Arc::new(f.lib.clone()),
        CoreConfig::default(),
    )
    .unwrap();
    let sub = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    entered.recv_timeout(TIMEOUT).unwrap();
    let target = *f.ids.iter().min().unwrap();
    assert_eq!(
        core.query(Query::Waveform { track: target }),
        QueryResult::Waveform { waveform: None }
    );
    ok(run(&core, Command::Pause, Origin::Ui));
    release.send(()).unwrap();
    let mut ready = 0;
    let mut completed = false;
    let deadline = std::time::Instant::now() + TIMEOUT;
    while ready < 2 || !completed {
        assert!(
            std::time::Instant::now() < deadline,
            "analysis events missing"
        );
        if let Ok(event) = sub.events.recv_timeout(Duration::from_millis(10)) {
            match event.event {
                Event::WaveformReady { track } if track == target => ready += 1,
                Event::JobFinished {
                    kind: JobKind::LoudnessScan,
                    ..
                } => completed = true,
                _ => {}
            }
        }
    }
    assert_eq!(f.resolver.1.load(std::sync::atomic::Ordering::Relaxed), 3);
}

fn report(e: &EventEnvelope) -> (u32, u32, Vec<TagFileResult>) {
    match &e.event {
        Event::JobFinished {
            outcome:
                JobOutcome::Completed {
                    report:
                        JobReport::LoudnessScan {
                            measured_count,
                            album_count,
                            failed,
                            ..
                        },
                },
            ..
        } => (*measured_count, *album_count, failed.clone()),
        other => panic!("{other:?}"),
    }
}

fn engine_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../engine/tests/fixtures")
        .join(name)
}

fn rescan(f: &Fixture) {
    let job = ok(run(&f.core, Command::Rescan, Origin::Ui)).job.unwrap();
    finished(f, job);
}

#[test]
fn a_track_that_cannot_be_measured_is_not_decoded_again_until_its_file_changes() {
    let resolver = Resolver::default();
    resolver.fail("b.flac", usize::MAX);
    let f = fixture_with(true, resolver, None);
    let (measured, albums, failed) = report(&automatic_finished(&f));
    assert_eq!((measured, albums), (2, 0));
    assert_eq!(failed.len(), 1);
    assert_eq!(f.resolver.attempts("b.flac"), 1);
    // Later passes (here for a new file) report it again without decoding it.
    std::fs::copy(engine_fixture("sine44_a.flac"), f.music.join("d.flac")).unwrap();
    rescan(&f);
    let (measured, albums, failed) = report(&automatic_finished(&f));
    assert_eq!((measured, albums), (1, 0));
    assert_eq!(failed[0].track, f.ids[1]);
    assert_eq!(failed[0].code, ErrorCode::OpenFailed);
    assert_eq!(f.resolver.attempts("b.flac"), 1);
    // Another version of the file is tried at once.
    f.resolver.fail("b.flac", 0);
    let file = std::fs::File::options()
        .write(true)
        .open(f.music.join("b.flac"))
        .unwrap();
    file.set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
        .unwrap();
    drop(file);
    rescan(&f);
    let (measured, albums, failed) = report(&automatic_finished(&f));
    assert_eq!((measured, albums, failed.len()), (1, 1, 0));
    assert_eq!(f.resolver.attempts("b.flac"), 2);
    assert!(data(&f, f.ids[0]).unwrap().album_lufs.is_some());
}

#[test]
fn tag_edits_and_removed_folders_keep_measurements_and_rebuild_albums_without_decoding() {
    let f = fixture_with_auto(true);
    automatic_finished(&f);
    let calls = f.resolver.1.load(std::sync::atomic::Ordering::Relaxed);
    let before = data(&f, f.ids[0]).unwrap();
    // Only tags change: the measurement moves to the file's new stamp; the album (whose
    // aggregate an edit drops) comes back from the stored statistics.
    let job = ok(run(
        &f.core,
        Command::EditTags {
            tracks: vec![f.ids[0]],
            changes: vec![TagChange::Set {
                key: "title".into(),
                values: vec!["retitled".into()],
            }],
        },
        Origin::Ui,
    ))
    .job
    .unwrap();
    finished(&f, job);
    let (measured, albums, failed) = report(&automatic_finished(&f));
    assert_eq!((measured, albums, failed.len()), (0, 1, 0));
    let after = data(&f, f.ids[0]).unwrap();
    assert_eq!(after.measured_at_ms, before.measured_at_ms);
    assert_eq!(after.integrated_lufs, before.integrated_lufs);
    // A second folder with a member of the same album: removing the folder drops the
    // album's aggregate; a pass rebuilds it for the members left.
    let other = f.music.parent().unwrap().join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::copy(f.music.join("c.flac"), other.join("c2.flac")).unwrap();
    let added = ok(run(
        &f.core,
        Command::AddRoot {
            path: other.to_string_lossy().into_owned(),
        },
        Origin::Ui,
    ));
    finished(&f, added.job.unwrap());
    let (measured, albums, _) = report(&automatic_finished(&f));
    assert_eq!((measured, albums), (1, 1));
    let with_four = data(&f, f.ids[0]).unwrap().album_lufs;
    ok(run(
        &f.core,
        Command::RemoveRoot {
            root: added.root.unwrap(),
        },
        Origin::Ui,
    ));
    let (measured, albums, _) = report(&automatic_finished(&f));
    assert_eq!((measured, albums), (0, 1));
    assert!(data(&f, f.ids[0]).unwrap().album_lufs.is_some());
    assert!(with_four.is_some());
    assert_eq!(
        f.resolver.1.load(std::sync::atomic::Ordering::Relaxed),
        calls + 1
    );
}

#[test]
fn a_failed_track_is_retried_when_due_also_while_core_is_idle() {
    let resolver = Resolver::default();
    resolver.fail("b.flac", 1);
    let f = fixture_with(true, resolver, Some(Duration::from_millis(200)));
    let (measured, albums, failed) = report(&automatic_finished(&f));
    assert_eq!((measured, albums, failed.len()), (2, 0, 1));
    // No command: the retry falls due on core's tick.
    let (measured, albums, failed) = report(&automatic_finished(&f));
    assert_eq!((measured, albums, failed.len()), (1, 1, 0));
    assert_eq!(f.resolver.attempts("b.flac"), 2);
}

#[test]
fn automatic_progress_and_saved_values_are_announced_in_batches() {
    let f = fixture_with_auto(true);
    automatic_finished(&f);
    let tracks = 120;
    for i in 0..tracks {
        let path = f.music.join(format!("{i:03}.flac"));
        std::fs::copy(engine_fixture("sine44_a.flac"), &path).unwrap();
        let mut file = lofty::read_from_path(&path).unwrap();
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.set_title(format!("{i}"));
        tag.set_artist("Artist".into());
        tag.set_album(format!("Album {}", i / 2));
        file.insert_tag(tag);
        file.save_to_path(&path, Default::default()).unwrap();
    }
    rescan(&f);
    let mut progress = Vec::new();
    let mut changes = 0;
    let mut started = None;
    let deadline = std::time::Instant::now() + TIMEOUT;
    let finished = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "automatic pass missing"
        );
        let Ok(e) = f.sub.events.recv_timeout(Duration::from_millis(20)) else {
            continue;
        };
        assert!(!matches!(e.event, Event::Resync { .. }));
        if e.origin.is_some() {
            continue;
        }
        match &e.event {
            Event::JobProgress {
                kind: JobKind::LoudnessScan,
                done,
                ..
            } => {
                started.get_or_insert_with(std::time::Instant::now);
                progress.push(*done);
            }
            Event::TracksChanged { .. } => changes += 1,
            Event::JobFinished {
                kind: JobKind::LoudnessScan,
                ..
            } => break e,
            _ => {}
        }
    };
    let (measured, albums, _) = report(&finished);
    assert_eq!((measured, albums), (tracks, tracks / 2));
    // At most one of each per 100 ms (and the last), not one per track or album.
    let batches = started.unwrap().elapsed().as_millis() as usize / 100 + 3;
    assert!(progress.len() <= batches, "{} > {batches}", progress.len());
    assert!(changes <= batches, "{changes} > {batches}");
    assert_eq!(progress.last(), Some(&(tracks + 3)));
}

#[test]
fn missing_statistics_backfill_rebuilds_an_existing_album_with_all_members_on_one_page() {
    let f = fixture();
    let mut members: Vec<_> = f
        .ids
        .iter()
        .map(|id| {
            let track = mpmp_library::TrackId(id.0);
            let source = f.lib.loudness_source(track).unwrap().unwrap();
            (track, source)
        })
        .collect();
    members.sort_by_key(|(id, _)| *id);
    let stored = f
        .lib
        .store_loudness(
            members
                .iter()
                .map(|(track, source)| mpmp_library::LoudnessUpdate {
                    track: *track,
                    version: source.version.clone(),
                    integrated_lufs: Some(-10.0),
                    sample_peak: 0.5,
                    statistics: None,
                })
                .collect(),
            vec![mpmp_library::AlbumLoudnessUpdate {
                album: members[0].1.album.clone(),
                members: members
                    .iter()
                    .map(|(id, source)| (*id, source.version.clone()))
                    .collect(),
                integrated_lufs: -10.0,
                sample_peak: 0.5,
            }],
        )
        .unwrap();
    assert_eq!(stored.album_count, 1);
    let Fixture {
        core,
        lib,
        resolver,
        music,
        ids,
        audio,
        _dir,
        sub,
    } = f;
    drop(core);
    drop(sub);
    drop(lib);
    let path = _dir.path().join("library.db");
    // Keep the current schema. Measurements without gating statistics are a valid
    // persistent state and must resume automatic analysis after a restart.
    let lib = mpmp_library::Library::open(&path).unwrap();
    let page = lib.automatic_loudness_tracks(0, 128).unwrap();
    assert_eq!(page.len(), 3);
    assert!(page.iter().all(|c| !c.measured && !c.album_pending));
    assert!(
        lib.loudness(mpmp_library::TrackId(ids[0].0))
            .unwrap()
            .unwrap()
            .album_lufs
            .is_some()
    );
    // Gate startup so the completion cannot be published before subscription.
    let (entered_tx, entered) = crossbeam_channel::bounded(1);
    let (release, release_rx) = crossbeam_channel::bounded(1);
    *resolver.0.lock().unwrap() = Some(Gate {
        entered: entered_tx,
        release: release_rx,
    });
    let core = CoreHandle::start(
        Box::new(support::null_output()),
        resolver.clone(),
        Arc::new(lib.clone()),
        CoreConfig::default(),
    )
    .unwrap();
    let sub = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    let f = Fixture {
        core,
        lib,
        resolver,
        music,
        ids,
        audio,
        _dir,
        sub,
    };
    entered.recv_timeout(TIMEOUT).unwrap();
    release.send(()).unwrap();
    let (measured, albums, failed) = report(&automatic_finished(&f));
    assert_eq!((measured, albums, failed.len()), (3, 1, 0));
    for id in &f.ids {
        assert!(data(&f, *id).unwrap().album_lufs.is_some());
        assert!(
            f.lib
                .loudness_statistics(mpmp_library::TrackId(id.0))
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn a_tag_job_that_catalogues_a_new_version_during_analysis_needs_no_rescan() {
    let f = fixture_with(true, Resolver::default(), Some(Duration::from_millis(100)));
    automatic_finished(&f);
    let (entered, release) = gate(&f);
    let path = f
        .lib
        .uri_of(mpmp_library::TrackId(f.ids[0].0))
        .unwrap()
        .unwrap()
        .to_file_path()
        .unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(1))
        .unwrap();
    rescan(&f);
    entered.recv_timeout(TIMEOUT).unwrap();
    let job = ok(run(
        &f.core,
        Command::EditTags {
            tracks: vec![f.ids[0]],
            changes: vec![TagChange::Set {
                key: "title".into(),
                values: vec!["retagged during analysis".into()],
            }],
        },
        Origin::Ui,
    ))
    .job
    .unwrap();
    finished(&f, job);
    release.send(()).unwrap();
    let (measured, albums, failed) = report(&automatic_finished(&f));
    assert_eq!((measured, albums, failed.len()), (0, 0, 0));
    let (measured, albums, failed) = report(&automatic_finished(&f));
    assert_eq!((measured, albums, failed.len()), (1, 1, 0));
    let until = std::time::Instant::now() + Duration::from_millis(450);
    while let Ok(event) = f
        .sub
        .events
        .recv_timeout(until.saturating_duration_since(std::time::Instant::now()))
    {
        assert!(
            !matches!(
                event.event,
                Event::JobProgress {
                    kind: JobKind::Scan,
                    ..
                }
            ),
            "{event:?}"
        );
        if std::time::Instant::now() >= until {
            break;
        }
    }
}

#[test]
fn removed_manual_loudness_commands_are_unknown_and_automatic_analysis_remains() {
    let f = fixture_with_auto(true);
    automatic_finished(&f);
    assert!(data(&f, f.ids[0]).is_some());
    for name in ["scan_loudness", "clear_loudness", "cancel_loudness_scan"] {
        let json = serde_json::json!({
        "request_id":1,
        "command":{
        "type":name,
        "tracks":[f.ids[0].0],
        "job":1}
        }
        )
        .to_string();

        let response: Response =
            serde_json::from_str(&f.core.execute_json(&json, Origin::Ui)).unwrap();
        assert!(matches!(
            response.result,
            CommandResult::Err { error } if error.code == ErrorCode::UnknownCommand,
        ));
    }
    let QueryResult::Snapshot { snapshot } = f.core.query(Query::Snapshot) else {
        panic!()
    };
    assert!(snapshot.features.iter().any(|s| s == "automatic_loudness"));
    assert!(!snapshot.features.iter().any(|s| s == "loudness_scan"));
}

#[test]
fn root_release_waits_for_analysis_and_resume_reenables_measurement() {
    let f = fixture_with_auto(true);
    automatic_finished(&f);
    let root = f.lib.roots().unwrap()[0].id;
    let (entered, release) = gate(&f);
    let path = f
        .lib
        .uri_of(mpmp_library::TrackId(f.ids[0].0))
        .unwrap()
        .unwrap()
        .to_file_path()
        .unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(1))
        .unwrap();
    rescan(&f);
    entered.recv_timeout(TIMEOUT).unwrap();
    let core = f.core.clone();
    let (tx, rx) = crossbeam_channel::bounded(1);
    let thread = std::thread::spawn(move || {
        tx.send(run(
            &core,
            Command::ReleaseRoot {
                root: RootId(root.0),
            },
            Origin::Ui,
        ))
        .unwrap()
    });
    let deadline = std::time::Instant::now() + TIMEOUT;
    while !f.lib.root_released(root) {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(
        rx.try_recv().is_err(),
        "release cannot acknowledge while analysis holds the root"
    );
    release.send(()).unwrap();
    assert!(ok(rx.recv_timeout(TIMEOUT).unwrap()).changed);
    thread.join().unwrap();
    assert!(f.lib.file_lease(mpmp_library::TrackId(f.ids[0].0)).is_err());
    assert!(
        matches!(
            f.core.query(Query::Loudness { track: f.ids[0] }),
            QueryResult::Loudness { .. }
        ),
        "cached values remain readable without opening the released folder"
    );
    let resumed = ok(run(
        &f.core,
        Command::ResumeRoot {
            root: RootId(root.0),
        },
        Origin::Ui,
    ));
    assert!(resumed.changed);
    assert!(f.lib.file_lease(mpmp_library::TrackId(f.ids[0].0)).is_ok());
}
