//! Core end to end: the actor driving the real engine on the null output, with an
//! in-memory catalog over the engine's fixture files.

use mpmp_test_support::client_parity;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mpmp_core::contract::*;
use mpmp_core::{
    Catalog, CoreConfig, CoreHandle, PlayRecord, Recorded, ScanEvent, ScanTicket, SubscribeOptions,
    Subscription,
};
use mpmp_media_io::{FileResolver, MediaUri};

use mpmp_test_support::core as support;

const TIMEOUT: Duration = Duration::from_secs(20);

mod output_initialization;

mod paused_navigation;

mod stopped_navigation;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../engine/tests/fixtures")
        .join(name)
}

fn info(id: i64, title: &str, album: &str, no: u32) -> TrackInfo {
    TrackInfo {
        id: TrackId(id),
        title: title.into(),
        artist: Some("Test".into()),
        album: Some(album.into()),
        album_key: album.into(),
        track_no: Some(no),
        codec: Some("flac".into()),
        sample_rate: Some(44_100),
        channels: Some(2),
        bits_per_sample: Some(16),
        lossless: true,
        available: true,
        ..Default::default()
    }
}

#[derive(Default)]
struct MemoryCatalog {
    tracks: HashMap<TrackId, (TrackInfo, PathBuf)>,
    plays: Mutex<Vec<PlayRecord>>,
}

impl MemoryCatalog {
    fn standard() -> Self {
        let mut c = Self::default();
        for (id, file, no) in [
            (1, "sine44_a.flac", 1),
            (2, "sine44_b.flac", 2),
            (3, "sine44_c.flac", 3),
        ] {
            c.tracks
                .insert(TrackId(id), (info(id, file, "sine", no), fixture(file)));
        }
        c.tracks.insert(
            TrackId(9),
            (
                info(9, "missing", "other", 1),
                fixture("does-not-exist.flac"),
            ),
        );
        c
    }
}

impl mpmp_core::catalog::Browse for MemoryCatalog {
    fn tracks(&self, ids: &[TrackId]) -> Result<Vec<TrackInfo>, ContractError> {
        Ok(ids
            .iter()
            .filter_map(|id| self.tracks.get(id).map(|(t, _)| t.clone()))
            .collect())
    }

    fn uri_of(&self, id: TrackId) -> Result<Option<MediaUri>, ContractError> {
        Ok(self
            .tracks
            .get(&id)
            .map(|(_, p)| MediaUri::from_path(p).unwrap()))
    }

    fn context_tracks(&self, context: &PlayContext) -> Result<Vec<TrackId>, ContractError> {
        Ok(match context {
            PlayContext::Tracks { tracks } => tracks.clone(),
            PlayContext::Album { key } => {
                let mut album: Vec<_> = self
                    .tracks
                    .values()
                    .filter(|(t, _)| &t.album_key == key)
                    .map(|(t, _)| (t.track_no, t.id))
                    .collect();
                if album.is_empty() {
                    return Err(ContractError::not_found(
                        ErrorCode::AlbumNotFound,
                        "no such album",
                    ));
                }
                album.sort();
                album.into_iter().map(|(_, id)| id).collect()
            }
            _ => Vec::new(),
        })
    }

    fn query(&self, _query: &Query) -> QueryResult {
        QueryResult::Err {
            error: ContractError::unsupported(ErrorCode::NotImplemented, "not in the test catalog"),
        }
    }
}

impl mpmp_core::catalog::Playlists for MemoryCatalog {
    // Confirmation lifetime/idempotency fixtures have no playlist store. Execution
    // still fails closed through Catalog::purge_guarded's unsupported default.
    fn describe_purge(
        &self,
        ids: Option<&[PlaylistId]>,
    ) -> Result<mpmp_library::PurgeGuard, ContractError> {
        assert!(ids.is_none());
        Ok(mpmp_library::PurgeGuard {
            all: true,
            playlists: vec![],
            trash: None,
            expiry_clock: None,
        })
    }
}

impl mpmp_core::catalog::LibraryRoots for MemoryCatalog {
    fn rescan(
        &self,
        events: Box<dyn Fn(ScanEvent) + Send + Sync>,
    ) -> Result<ScanTicket, ContractError> {
        events(ScanEvent::Finished {
            scan: 1,
            outcome: JobOutcome::Completed {
                report: JobReport::Scan {
                    added_count: 0,
                    updated_count: 0,
                    restored_count: 0,
                    removed_count: 0,
                    error_count: 0,
                    first_errors: Vec::new(),
                },
            },
        });
        Ok(ScanTicket {
            scan: 1,
            joined: false,
        })
    }
}

impl mpmp_core::catalog::Actions for MemoryCatalog {}

impl mpmp_core::catalog::CatalogChecks for MemoryCatalog {
    fn storage_write_capabilities(&self) -> mpmp_storage::WriteCapabilities {
        mpmp_storage::WriteCapabilities {
            durable_replace: true,
            legacy_tag_write: true,
        }
    }
}

impl mpmp_core::catalog::AgentPermissions for MemoryCatalog {}

impl mpmp_core::catalog::MediaResources for MemoryCatalog {}

impl mpmp_core::catalog::Loudness for MemoryCatalog {}

impl mpmp_core::catalog::TagsAndCovers for MemoryCatalog {}

impl mpmp_core::catalog::PlaybackHistory for MemoryCatalog {
    fn record_play(&self, record: PlayRecord) {
        self.plays.lock().unwrap().push(record);
    }
}

struct Harness {
    core: CoreHandle,
    output: mpmp_engine::NullOutputHandle,
    catalog: Arc<MemoryCatalog>,
    sub: Subscription,
    seen: Vec<EventEnvelope>,
    next_request: u64,
}

impl Harness {
    fn new(state_file: Option<PathBuf>) -> Self {
        Self::with_catalog(Arc::new(MemoryCatalog::standard()), state_file)
    }

    fn with_catalog(catalog: Arc<MemoryCatalog>, state_file: Option<PathBuf>) -> Self {
        Self::with_config(
            catalog,
            CoreConfig {
                state_file,
                ..Default::default()
            },
        )
    }

    fn with_config(catalog: Arc<MemoryCatalog>, config: CoreConfig) -> Self {
        let output = support::null_output();
        let handle = output.handle();
        let core = CoreHandle::start(
            Box::new(output),
            Arc::new(FileResolver),
            Arc::clone(&catalog) as Arc<dyn Catalog>,
            config,
        )
        .unwrap();
        support::wait_for_output_initialization(&core);
        let sub = core
            .subscribe(Origin::Ui, SubscribeOptions::default())
            .unwrap();
        Self {
            core,
            output: handle,
            catalog,
            sub,
            seen: Vec::new(),
            next_request: 0,
        }
    }

    fn run_as(&mut self, command: Command, origin: Origin) -> CommandResult {
        self.next_request += 1;
        let response = self
            .core
            .execute(Request::new(self.next_request, command), origin);
        assert_eq!(response.request_id, self.next_request);
        response.result
    }

    fn run(&mut self, command: Command) -> CommandResult {
        self.run_as(command, Origin::Ui)
    }

    fn ok(&mut self, command: Command) -> Receipt {
        match self.run(command) {
            CommandResult::Ok(receipt) => receipt,
            other => panic!("command failed: {other:?}"),
        }
    }

    fn err(&mut self, command: Command) -> ContractError {
        self.err_as(command, Origin::Ui)
    }

    fn err_as(&mut self, command: Command, origin: Origin) -> ContractError {
        match self.run_as(command, origin) {
            CommandResult::Err { error } => error,
            other => panic!("command did not fail: {other:?}"),
        }
    }

    fn snapshot(&self) -> Snapshot {
        match self.core.query(Query::Snapshot) {
            QueryResult::Snapshot { snapshot } => *snapshot,
            other => panic!("{other:?}"),
        }
    }

    /// The snapshot of the last `StateChanged` seen. After waiting for an event, use this
    /// rather than `snapshot()`: events reach subscribers before the published snapshot.
    fn last_state(&self) -> Snapshot {
        self.seen
            .iter()
            .rev()
            .find_map(|e| match &e.event {
                Event::StateChanged { snapshot } => Some((**snapshot).clone()),
                _ => None,
            })
            .expect("no StateChanged seen")
    }

    /// Renders audio (at about 4× real time) until an event matches.
    fn until(&mut self, mut pred: impl FnMut(&Event) -> bool) -> Event {
        self.until_envelope(|e| pred(&e.event)).event
    }

    fn until_envelope(&mut self, mut pred: impl FnMut(&EventEnvelope) -> bool) -> EventEnvelope {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self.output.is_open() {
                self.output.render_f32(512);
            }
            std::thread::sleep(Duration::from_millis(3));
            while let Ok(envelope) = self.sub.events.try_recv() {
                let hit = pred(&envelope);
                self.seen.push(envelope.clone());
                if hit {
                    return envelope;
                }
            }
        }
        panic!(
            "event never came; seen: {:#?}",
            self.seen.iter().map(|e| &e.event).collect::<Vec<_>>()
        );
    }

    /// Events received so far, without rendering.
    fn drain(&mut self) -> Vec<EventEnvelope> {
        let new: Vec<_> = self.sub.events.try_iter().collect();
        self.seen.extend(new.iter().cloned());
        new
    }

    fn started(&self) -> Vec<EntryId> {
        self.seen
            .iter()
            .filter_map(|e| match &e.event {
                Event::TrackStarted { entry, .. } => Some(*entry),
                _ => None,
            })
            .collect()
    }

    fn queue_page(&self) -> (QueueRevision, u32, Vec<QueueEntry>) {
        match self.core.query(Query::QueuePage {
            offset: 0,
            limit: 100,
        }) {
            QueryResult::QueuePage {
                revision,
                total,
                entries,
                ..
            } => (revision, total, entries),
            other => panic!("{other:?}"),
        }
    }
}

fn play(context: PlayContext) -> Command {
    play_at(context, 0)
}

fn play_at(context: PlayContext, start: u32) -> Command {
    Command::Play {
        context,
        start,
        start_track: None,
    }
}

fn album() -> PlayContext {
    PlayContext::Album { key: "sine".into() }
}

fn tracks(ids: &[i64]) -> PlayContext {
    PlayContext::Tracks {
        tracks: ids.iter().map(|&i| TrackId(i)).collect(),
    }
}

fn seek(position_ms: u64) -> Command {
    Command::Seek {
        position_ms,
        entry: None,
    }
}

fn clear() -> Command {
    Command::Clear {
        expected_revision: None,
    }
}

fn is_stopped(e: &Event) -> bool {
    matches!(e, Event::StateChanged { snapshot } if snapshot.state == PlayState::Stopped)
}

/// An absolute path on this platform (agents must name one: `/music` is relative on
/// Windows), in a folder that does not exist.
fn absent(name: &str) -> String {
    std::env::temp_dir()
        .join("pmp-no-such-folder")
        .join(name)
        .display()
        .to_string()
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

// Command validation, queue edits, and event delivery.

fn heard_tracks(h: &Harness) -> Vec<i64> {
    h.seen
        .iter()
        .filter_map(|e| match &e.event {
            Event::TrackStarted { track, .. } => Some(track.id.0),
            _ => None,
        })
        .collect()
}

/// A 20 s silent WAV, long enough to seek around in.
fn long_track(dir: &Path) -> (TrackInfo, PathBuf) {
    let path = dir.join("long.wav");
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = hound::WavWriter::create(&path, spec).unwrap();
    for _ in 0..44_100 * 20 * 2 {
        wav.write_sample(0i16).unwrap();
    }
    wav.finalize().unwrap();
    let mut track = info(20, "long", "long", 1);
    track.duration_ms = Some(20_000);
    (track, path)
}

fn with_long_track() -> (tempfile::TempDir, Harness) {
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = MemoryCatalog::standard();
    catalog.tracks.insert(TrackId(20), long_track(dir.path()));
    let h = Harness::with_catalog(Arc::new(catalog), None);
    (dir, h)
}

fn stage_names(snapshot: &Snapshot) -> Vec<String> {
    snapshot
        .current
        .as_ref()
        .and_then(|c| c.signal_path.as_ref())
        .map(|p| p.stages.iter().map(|s| s.name.clone()).collect())
        .unwrap_or_default()
}

fn replay_gain(mode: Option<ReplayGainMode>, fallback_preamp_db: Option<f32>) -> Command {
    Command::SetReplayGain {
        mode,
        preamp_db: None,
        fallback_preamp_db,
        prevent_clipping: None,
    }
}

// --- API v1 semantics (docs/api-v1-freeze.md) ---

// Playback transitions and idempotent command handling.

/// A catalog whose loved flags fail once with a retryable database error.
struct FlakyCatalog {
    inner: MemoryCatalog,
    fail_next: std::sync::atomic::AtomicBool,
}

impl mpmp_core::catalog::Browse for FlakyCatalog {
    fn tracks(&self, ids: &[TrackId]) -> Result<Vec<TrackInfo>, ContractError> {
        self.inner.tracks(ids)
    }

    fn uri_of(&self, id: TrackId) -> Result<Option<MediaUri>, ContractError> {
        self.inner.uri_of(id)
    }

    fn context_tracks(&self, context: &PlayContext) -> Result<Vec<TrackId>, ContractError> {
        self.inner.context_tracks(context)
    }

    fn query(&self, query: &Query) -> QueryResult {
        self.inner.query(query)
    }
}

impl mpmp_core::catalog::Playlists for FlakyCatalog {}

impl mpmp_core::catalog::LibraryRoots for FlakyCatalog {}

impl mpmp_core::catalog::Actions for FlakyCatalog {
    fn set_loved(
        &self,
        tracks: &[TrackId],
        _loved: bool,
        _origin: &Origin,
    ) -> Result<Recorded<Vec<TrackId>>, ContractError> {
        if self
            .fail_next
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            Err(ContractError::failed(ErrorCode::Db, "database is locked").retryable())
        } else {
            Ok(Recorded::new(tracks.to_vec(), Some(ActionId(7))))
        }
    }
}

impl mpmp_core::catalog::CatalogChecks for FlakyCatalog {
    fn storage_write_capabilities(&self) -> mpmp_storage::WriteCapabilities {
        mpmp_storage::WriteCapabilities {
            durable_replace: true,
            legacy_tag_write: true,
        }
    }
}

impl mpmp_core::catalog::AgentPermissions for FlakyCatalog {}

impl mpmp_core::catalog::MediaResources for FlakyCatalog {}

impl mpmp_core::catalog::Loudness for FlakyCatalog {}

impl mpmp_core::catalog::TagsAndCovers for FlakyCatalog {}

impl mpmp_core::catalog::PlaybackHistory for FlakyCatalog {
    fn record_play(&self, record: PlayRecord) {
        self.inner.record_play(record)
    }
}

mod playback;

mod queue;

mod persistence;

mod permissions;

mod playback_limits;
