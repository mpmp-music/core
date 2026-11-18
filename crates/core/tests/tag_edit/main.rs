//! Tag editing through core (docs/design/media-resources.md): the preview query, the
//! command as a job with its events and action, its undo, and the agent permissions of
//! both (class `file_write`, targets the tracks' files).

use mpmp_test_support::client_parity;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::{Accessor, Tag, TagType};
use mpmp_core::contract::*;
use mpmp_core::{Catalog, CoreConfig, CoreHandle, SubscribeOptions, Subscription};
use mpmp_media_io::FileResolver;

use mpmp_test_support::core as support;

const TIMEOUT: Duration = Duration::from_secs(30);

const ALBUM: &str = "Artist/Album";

struct Fixture {
    core: CoreHandle,
    sub: Subscription,
    /// Core's catalog, for what no command does (a folder's text encoding).
    library: Arc<mpmp_library::Library>,
    music: PathBuf,
    root: RootId,
    /// By file name.
    tracks: BTreeMap<String, TrackId>,
    _dir: tempfile::TempDir,
}

/// A core over a library with three tagged FLAC files in `Artist/Album`.
fn fixture() -> Fixture {
    fixture_with_resolver(Arc::new(FileResolver))
}

fn fixture_with_resolver(resolver: Arc<dyn mpmp_media_io::SourceResolver>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    let album = music.join("Artist").join("Album");
    std::fs::create_dir_all(&album).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures");
    for (no, part) in [(1, "a"), (2, "b"), (3, "c")] {
        let path = album.join(format!("{no:02}.flac"));
        std::fs::copy(fixtures.join(format!("sine44_{part}.flac")), &path).unwrap();
        let mut file = lofty::read_from_path(&path).unwrap();
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.set_title(format!("Song {no}"));
        tag.set_artist("Old".into());
        tag.set_album("Album".into());
        tag.set_comment("comment".into());
        tag.set_track(no);
        file.insert_tag(tag);
        file.save_to_path(&path, WriteOptions::default()).unwrap();
    }
    let library = Arc::new(mpmp_library::Library::open(&dir.path().join("library.db")).unwrap());
    let output = support::null_output();
    let core = CoreHandle::start(
        Box::new(output),
        resolver,
        Arc::clone(&library) as Arc<dyn Catalog>,
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
    let music =
        PathBuf::from(mpmp_fs_desktop::DesktopLocations.display(&music.display().to_string()));
    let added = ok(run(
        &core,
        Command::AddRoot {
            path: music.display().to_string(),
        },
        Origin::Ui,
    ));
    let job = added.job.unwrap();
    wait_event(
        &sub,
        |e| matches!(&e.event, Event::JobFinished { job: j, .. } if *j == job),
    );
    wait_event(&sub, |e| matches!(e.event, Event::LibraryChanged));
    let QueryResult::Tracks { tracks, .. } = core.query(Query::Tracks {
        query: TrackQuery::default(),
        offset: 0,
        limit: 100,
        tags: Vec::new(),
    }) else {
        panic!()
    };
    let tracks = tracks
        .into_iter()
        .map(|t| (t.path.rsplit('/').next().unwrap().to_owned(), t.id))
        .collect();
    Fixture {
        core,
        sub,
        library,
        music,
        root: added.root.unwrap(),
        tracks,
        _dir: dir,
    }
}

impl Fixture {
    fn all(&self) -> Vec<TrackId> {
        self.tracks.values().copied().collect()
    }

    fn id(&self, name: &str) -> TrackId {
        self.tracks[name]
    }

    fn file(&self, name: &str) -> PathBuf {
        self.music.join("Artist").join("Album").join(name)
    }

    /// A tag of each track, as the library shows it.
    fn tag(&self, key: &str) -> Vec<Vec<String>> {
        let QueryResult::Tracks { tracks, .. } = self.core.query(Query::Tracks {
            query: TrackQuery::default(),
            offset: 0,
            limit: 100,
            tags: vec![key.into()],
        }) else {
            panic!()
        };
        let mut tracks = tracks;
        tracks.sort_by(|a, b| a.path.cmp(&b.path));
        tracks
            .iter()
            .map(|t| t.tags.get(key).cloned().unwrap_or_default())
            .collect()
    }

    fn action(&self, id: ActionId) -> ActionSummary {
        let QueryResult::Actions { actions, .. } = self.core.query(Query::Actions {
            origin: None,
            offset: 0,
            limit: 100,
        }) else {
            panic!()
        };
        actions.into_iter().find(|a| a.id == id).unwrap()
    }

    /// The events of a job up to and with its `JobFinished`, then the `LibraryChanged`
    /// that follows it when it wrote anything; positions left out.
    fn job_events(&self, job: JobId) -> Vec<EventEnvelope> {
        let mut seen = Vec::new();
        let deadline = Instant::now() + TIMEOUT;
        let mut finished = None;
        while Instant::now() < deadline {
            let Ok(envelope) = self.sub.events.recv_timeout(Duration::from_millis(20)) else {
                if finished.is_some_and(|at: Instant| at.elapsed() > Duration::from_millis(200)) {
                    return seen;
                }
                continue;
            };
            if matches!(envelope.event, Event::Position { .. }) {
                continue;
            }
            let done = matches!(&envelope.event, Event::JobFinished { job: j, .. } if *j == job);
            let changed = matches!(envelope.event, Event::LibraryChanged);
            seen.push(envelope);
            if done {
                let Event::JobFinished { outcome, .. } = &seen.last().unwrap().event else {
                    unreachable!()
                };
                let QueryResult::Job { job: info } = self.core.query(Query::Job { job }) else {
                    panic!("job missing")
                };
                assert_eq!(info.outcome.as_ref(), Some(outcome));
                finished = Some(Instant::now());
            }
            if changed && finished.is_some() {
                return seen;
            }
        }
        panic!("the job never finished: {seen:#?}");
    }
}

use support::run;

use support::ok;

use support::err;

use support::wait_event;

fn set(key: &str, values: &[&str]) -> TagChange {
    TagChange::Set {
        key: key.into(),
        values: values.iter().map(|v| (*v).to_owned()).collect(),
    }
}

fn edit(tracks: Vec<TrackId>, changes: Vec<TagChange>) -> Command {
    Command::EditTags { tracks, changes }
}

/// The report of a tag job's `JobFinished` among `events`.
fn report(
    events: &[EventEnvelope],
) -> (
    u32,
    Option<ActionId>,
    Vec<TagFileResult>,
    Vec<TagFileResult>,
) {
    for e in events {
        if let Event::JobFinished {
            kind: JobKind::TagEdit,
            outcome:
                JobOutcome::Completed {
                    report:
                        JobReport::TagEdit {
                            written_count,
                            action,
                            skipped,
                            failed,
                            ..
                        },
                },
            ..
        } = &e.event
        {
            return (*written_count, *action, skipped.clone(), failed.clone());
        }
    }
    panic!("no tag job report in {events:#?}");
}

fn names(events: &[EventEnvelope]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match &e.event {
            Event::JobProgress { .. } => "job_progress",
            Event::TracksChanged { .. } => "tracks_changed",
            Event::ActionsChanged => "actions_changed",
            Event::JobFinished { .. } => "job_finished",
            Event::LibraryChanged => "library_changed",
            _ => "other",
        })
        .collect()
}

fn touch(path: &Path) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(5))
        .unwrap();
}

fn agent() -> Origin {
    Origin::agent("claude")
}

fn grant(operation: Operation, scope: Scope, effect: Effect) -> Command {
    Command::GrantPermission {
        agent: "claude".into(),
        session: None,
        operation,
        scope,
        until: Expiry::Forever,
        effect,
    }
}

/// The next confirmation asked for.
#[cfg(unix)]
fn asked(f: &Fixture) -> PendingConfirmation {
    let event = wait_event(&f.sub, |e| {
        matches!(e.event, Event::ConfirmationRequested { .. })
    });
    let Event::ConfirmationRequested { confirmation } = event.event else {
        panic!()
    };
    *confirmation
}

fn add_mp3(f: &Fixture, legacy: bool) -> TrackId {
    add_mp3_with_title(f, legacy, "三棱雾")
}

fn add_mp3_with_title(f: &Fixture, legacy: bool, title: &str) -> TrackId {
    use lofty::TextEncoding;
    use lofty::id3::v2::{Frame, FrameId, Id3v2Tag, TextInformationFrame};
    use lofty::tag::TagExt;
    let path = f.music.join("legacy.mp3");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_a.mp3"),
        &path,
    )
    .unwrap();
    let mut tag = Id3v2Tag::new();
    let (id, encoding, value) = if legacy {
        let (bytes, _, errors) = encoding_rs::GBK.encode(title);
        assert!(!errors, "legacy title must be representable in GBK");
        (
            "TIT2",
            TextEncoding::Latin1,
            bytes.iter().map(|b| *b as char).collect::<String>(),
        )
    } else {
        ("TRCK", TextEncoding::UTF8, "3/abc".into())
    };
    tag.insert(Frame::Text(TextInformationFrame::new(
        FrameId::Valid(id.into()),
        encoding,
        value,
    )));
    tag.save_to_path(&path, WriteOptions::new().use_id3v23(true))
        .unwrap();
    let job = ok(run(&f.core, Command::Rescan, Origin::Ui)).job.unwrap();
    wait_event(
        &f.sub,
        |e| matches!(e.event, Event::JobFinished { job: j, .. } if j == job),
    );
    let QueryResult::Tracks { tracks, .. } = f.core.query(Query::Tracks {
        query: TrackQuery::default(),
        offset: 0,
        limit: 100,
        tags: Vec::new(),
    }) else {
        panic!()
    };
    tracks.iter().find(|t| t.path == "legacy.mp3").unwrap().id
}

/// The real library preview supplies the code; the policy worker must answer even
/// when its first skip belongs to a different error kind than invalid.
fn assert_agent_all_skipped_finishes(f: &Fixture, tracks: Vec<TrackId>, code: ErrorCode) {
    let changes = vec![set("ARTIST", &["New"])];
    let QueryResult::TagEditPreview { files, skipped } = f.core.query(Query::TagEditPreview {
        tracks: tracks.clone(),
        changes: changes.clone(),
    }) else {
        panic!("tag preview")
    };
    assert!(files.is_empty());
    assert_eq!(skipped.len(), tracks.len());
    assert!(skipped.iter().all(|file| file.code == code));
    let QueryResult::Permissions { grants, .. } = f.core.query(Query::Permissions {
        agent: None,
        offset: 0,
        limit: 100,
    }) else {
        panic!("grants")
    };
    assert!(grants.is_empty(), "the agent must have no allow grant");
    let core = f.core.clone();
    let (sent, result) = crossbeam_channel::bounded(1);
    let worker = std::thread::spawn(move || {
        let _ = sent.send(run(&core, edit(tracks, changes), agent()));
    });
    let response = result.recv_timeout(TIMEOUT);
    if response.is_err() {
        f.core.shutdown();
    }
    let error = err(response.expect("all-skipped agent request must finish within the deadline"));
    worker.join().unwrap();
    assert_eq!(error.code, code);
    assert_eq!(error.kind, code.kind());
    assert!(error.details.confirmation.is_none());
    assert_eq!(error.details.field.as_deref(), Some("command.tracks"));
    assert!(
        !f.sub
            .events
            .try_iter()
            .any(|e| matches!(e.event, Event::ConfirmationRequested { .. }))
    );
    // The actor remains serviceable after the off-actor policy check.
    ok(run(&f.core, Command::SetMuted { muted: true }, Origin::Ui));
}

/// Only prefetch resolution blocks; scanning/waveforms/current playback stay independent.
struct PrefetchGate {
    entered: crossbeam_channel::Sender<String>,
    gates: Mutex<BTreeMap<String, crossbeam_channel::Receiver<()>>>,
}

impl mpmp_media_io::SourceResolver for PrefetchGate {
    fn resolve(
        &self,
        uri: &mpmp_media_io::MediaUri,
    ) -> Result<mpmp_media_io::ResolvedSource, mpmp_media_io::Error> {
        if std::thread::current().name() == Some("prefetch") {
            let name = uri
                .to_file_path()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let gate = self.gates.lock().unwrap().remove(&name);
            if let Some(gate) = gate {
                self.entered.send(name).unwrap();
                gate.recv_timeout(TIMEOUT).unwrap();
            }
        }
        mpmp_media_io::SourceResolver::resolve(&FileResolver, uri)
    }
}

fn begin_release(
    f: &Fixture,
) -> (
    crossbeam_channel::Receiver<CommandResult>,
    std::thread::JoinHandle<()>,
) {
    let core = f.core.clone();
    let root = f.root;
    let (tx, rx) = crossbeam_channel::bounded(1);
    let thread = std::thread::spawn(move || {
        tx.send(run(&core, Command::ReleaseRoot { root }, Origin::Ui))
            .unwrap();
    });
    let deadline = Instant::now() + TIMEOUT;
    while !f.library.root_released(mpmp_library::RootId(root.0)) {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    (rx, thread)
}

struct MainReadGate {
    entered: crossbeam_channel::Sender<()>,
    gate: crossbeam_channel::Receiver<()>,
    armed: std::sync::atomic::AtomicBool,
    closed: std::sync::atomic::AtomicBool,
}

struct GatedBytes {
    source: Arc<dyn mpmp_media_io::ByteSource>,
    gate: Arc<MainReadGate>,
    held: std::sync::atomic::AtomicBool,
}

impl mpmp_media_io::ByteSource for GatedBytes {
    fn read_at(&self, offset: u64, bytes: &mut [u8]) -> std::io::Result<usize> {
        use std::sync::atomic::Ordering::SeqCst;
        if std::thread::current().name() == Some("decode") && self.gate.armed.swap(false, SeqCst) {
            self.held.store(true, SeqCst);
            self.gate.entered.send(()).unwrap();
            self.gate.gate.recv_timeout(TIMEOUT).unwrap();
        }
        let len = bytes.len().min(512); // probing must not buffer the entire small fixture
        self.source.read_at(offset, &mut bytes[..len])
    }
    fn len(&self) -> Option<u64> {
        self.source.len()
    }
    fn is_seekable(&self) -> bool {
        self.source.is_seekable()
    }
}

impl Drop for GatedBytes {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::SeqCst;
        if self.held.load(SeqCst) {
            self.gate.closed.store(true, SeqCst);
        }
    }
}

struct MainReadResolver(Arc<MainReadGate>);

impl mpmp_media_io::SourceResolver for MainReadResolver {
    fn resolve(
        &self,
        uri: &mpmp_media_io::MediaUri,
    ) -> Result<mpmp_media_io::ResolvedSource, mpmp_media_io::Error> {
        FileResolver.resolve(uri)
    }
    fn open(
        &self,
        uri: &mpmp_media_io::MediaUri,
    ) -> Result<Arc<dyn mpmp_media_io::ByteSource>, mpmp_media_io::Error> {
        Ok(Arc::new(GatedBytes {
            source: mpmp_media_io::open(FileResolver.resolve(uri)?)?,
            gate: Arc::clone(&self.0),
            held: false.into(),
        }))
    }
}

fn main_read_fixture() -> (Fixture, Arc<MainReadGate>, crossbeam_channel::Sender<()>) {
    let (tx, entered) = crossbeam_channel::bounded(1);
    let (open, gate) = crossbeam_channel::bounded(1);
    let state = Arc::new(MainReadGate {
        entered: tx,
        gate,
        armed: true.into(),
        closed: false.into(),
    });
    let f = fixture_with_resolver(Arc::new(MainReadResolver(Arc::clone(&state))));
    ok(run(
        &f.core,
        Command::Play {
            context: PlayContext::Tracks {
                tracks: vec![f.id("01.flac")],
            },
            start: 0,
            start_track: None,
        },
        Origin::Ui,
    ));
    entered
        .recv_timeout(TIMEOUT)
        .expect("main decode read did not block");
    (f, state, open)
}

fn cover_set(tracks: Vec<TrackId>) -> Command {
    Command::EditCover {
    tracks,
    change:CoverChange::Set {
    image:CoverImageInput {
    mime:"image/png".into(),
    data_base64:
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGMQFNf8DwAB3wFRsS+UFAAAAABJRU5ErkJggg=="
    .into()}
    }
    }
}

fn cover_report(
    events: &[EventEnvelope],
) -> (
    Vec<TrackId>,
    Option<ActionId>,
    Vec<CoverFileResult>,
    Vec<CoverFileResult>,
) {
    events
        .iter()
        .find_map(|e| match &e.event {
            Event::JobFinished {
                kind: JobKind::CoverEdit,
                outcome:
                    JobOutcome::Completed {
                        report:
                            JobReport::CoverEdit {
                                written,
                                action,
                                skipped,
                                failed,
                                ..
                            },
                    },
                ..
            } => Some((written.clone(), *action, skipped.clone(), failed.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{events:#?}"))
}

mod undo;

mod edits;

mod permissions;

mod root_access;

mod covers;
