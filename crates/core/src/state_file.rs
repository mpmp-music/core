//! Playback state that survives restarts: a small JSON file written
//! atomically, kept apart from the library database so a long scan never delays it.
//!
//! The file has versioned stored sections. The optional output subsection keeps
//! validated preferences and the last device descriptor, never active stream facts:
//! `{"version": 1, "next_entry_id": …, "queue_revision": …, "queue": {…}, "audio": {…}}`.
//! Sections are read independently: a section that does not parse falls back to its
//! default without losing the others. Files from before versioning (no `version`) are
//! read as such.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};

use serde::{Deserialize, Serialize};

use crate::contract::{
    EntryId, PlayContext, PlaylistId, RepeatMode, ReplayGainMode, ReplayGainSettings, TrackId,
    TrackQuery,
};

pub(crate) const STATE_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct SavedState {
    pub queue: SavedQueue,
    pub audio: SavedAudio,
    /// The id the next new queue entry gets.
    pub next_entry_id: u64,
    pub queue_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub(crate) struct SavedQueue {
    #[serde(default)]
    pub entries: Vec<SavedEntry>,
    #[serde(default)]
    pub current_index: Option<u32>,
    #[serde(default)]
    pub position_ms: u64,
    /// A context that does not parse is dropped, not the queue.
    #[serde(default, deserialize_with = "lenient")]
    pub context: Option<SavedContext>,
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default, deserialize_with = "lenient")]
    pub repeat: Option<SavedRepeat>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedEntry {
    /// 0: none (files from before versioning); a new id is given.
    #[serde(default)]
    pub id: u64,
    pub track: i64,
}

/// Where the queue came from, as stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SavedContext {
    Album {
        key: String,
    },
    Tracks {
        tracks: Vec<i64>,
    },
    Search {
        text: String,
    },
    /// The contract (v1) `TrackQuery` JSON.
    Query {
        query: serde_json::Value,
    },
    Playlist {
        playlist: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SavedRepeat {
    Off,
    All,
    One,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SavedAudio {
    pub volume: f32,
    pub muted: bool,
    #[serde(default, deserialize_with = "lenient")]
    pub replay_gain: Option<SavedReplayGain>,
    #[serde(default, deserialize_with = "lenient_output")]
    pub output: Option<SavedOutput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SavedOutput {
    #[serde(flatten)]
    pub settings: crate::contract::OutputSettings,
    #[serde(default)]
    pub device: Option<crate::contract::OutputDeviceInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct SavedReplayGain {
    pub mode: SavedGainMode,
    #[serde(default)]
    pub preamp_db: f32,
    #[serde(default)]
    pub fallback_preamp_db: f32,
    #[serde(default = "yes")]
    pub prevent_clipping: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SavedGainMode {
    Off,
    Track,
    Album,
    Smart,
}

impl Default for SavedAudio {
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
            replay_gain: None,
            output: None,
        }
    }
}

/// `None` instead of an error for a value that does not parse.
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

fn lenient_output<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SavedOutput>, D::Error> {
    use crate::contract::{
        OutputDeviceSelection, OutputFallbackPolicy, OutputRatePolicy, OutputShareMode,
    };
    let output: Option<SavedOutput> = lenient(deserializer)?;
    Ok(output.filter(|o| {
        matches!(
            o.settings.selection,
            OutputDeviceSelection::SystemDefault | OutputDeviceSelection::Device { .. }
        ) && o.settings.mode == OutputShareMode::Shared
            && o.settings.rate_policy == OutputRatePolicy::Mix
            && matches!(
                o.settings.fallback,
                OutputFallbackPolicy::Pause | OutputFallbackPolicy::Shared
            )
            && o.device
                .as_ref()
                .is_none_or(|d| d.id_stability != crate::contract::OutputDeviceIdStability::Unknown)
    }))
}

// --- conversions to and from the contract's types ---

impl SavedContext {
    pub fn from_contract(context: &PlayContext) -> Option<Self> {
        Some(match context {
            PlayContext::Album { key } => SavedContext::Album { key: key.clone() },
            PlayContext::Tracks { tracks } => SavedContext::Tracks {
                tracks: tracks.iter().map(|t| t.0).collect(),
            },
            PlayContext::Search { text } => SavedContext::Search { text: text.clone() },
            PlayContext::Query { query } => SavedContext::Query {
                query: serde_json::to_value(query).ok()?,
            },
            PlayContext::Playlist { playlist, .. } => SavedContext::Playlist {
                playlist: playlist.0,
            },
            _ => return None,
        })
    }

    pub fn to_contract(&self) -> Option<PlayContext> {
        Some(match self {
            SavedContext::Album { key } => PlayContext::Album { key: key.clone() },
            SavedContext::Tracks { tracks } => PlayContext::Tracks {
                tracks: tracks.iter().map(|&t| TrackId(t)).collect(),
            },
            SavedContext::Search { text } => PlayContext::Search { text: text.clone() },
            SavedContext::Query { query } => PlayContext::Query {
                query: serde_json::from_value::<TrackQuery>(query.clone()).ok()?,
            },
            SavedContext::Playlist { playlist } => PlayContext::Playlist {
                playlist: PlaylistId(*playlist),
                start_item: None,
                expected_revision: None,
            },
        })
    }
}

impl SavedRepeat {
    pub fn from_contract(mode: RepeatMode) -> Self {
        match mode {
            RepeatMode::All => SavedRepeat::All,
            RepeatMode::One => SavedRepeat::One,
            _ => SavedRepeat::Off,
        }
    }

    pub fn to_contract(self) -> RepeatMode {
        match self {
            SavedRepeat::Off => RepeatMode::Off,
            SavedRepeat::All => RepeatMode::All,
            SavedRepeat::One => RepeatMode::One,
        }
    }
}

impl SavedReplayGain {
    pub fn from_contract(s: &ReplayGainSettings) -> Self {
        Self {
            mode: match s.mode {
                ReplayGainMode::Off => SavedGainMode::Off,
                ReplayGainMode::Track => SavedGainMode::Track,
                ReplayGainMode::Album => SavedGainMode::Album,
                _ => SavedGainMode::Smart,
            },
            preamp_db: s.preamp_db,
            fallback_preamp_db: s.fallback_preamp_db,
            prevent_clipping: s.prevent_clipping,
        }
    }

    pub fn to_contract(self) -> ReplayGainSettings {
        ReplayGainSettings {
            mode: match self.mode {
                SavedGainMode::Off => ReplayGainMode::Off,
                SavedGainMode::Track => ReplayGainMode::Track,
                SavedGainMode::Album => ReplayGainMode::Album,
                SavedGainMode::Smart => ReplayGainMode::Smart,
            },
            preamp_db: self.preamp_db,
            fallback_preamp_db: self.fallback_preamp_db,
            prevent_clipping: self.prevent_clipping,
        }
    }
}

impl SavedQueue {
    pub fn entries(&self) -> Vec<(EntryId, TrackId)> {
        self.entries
            .iter()
            .map(|e| (EntryId(e.id), TrackId(e.track)))
            .collect()
    }
}

// --- the file ---

pub(crate) struct StateFile {
    path: PathBuf,
    writer: Option<StateWriter>,
}

/// One active write and one replaceable snapshot; the actor never waits for disk.
struct StateWriter {
    tx: Option<Sender<SavedState>>,
    pending: Receiver<SavedState>,
    thread: Option<JoinHandle<()>>,
}

impl StateWriter {
    fn start(mut write: impl FnMut(SavedState) + Send + 'static) -> std::io::Result<Self> {
        let (tx, pending) = bounded(1);
        let jobs = pending.clone();
        let thread = std::thread::Builder::new()
            .name("state-writer".into())
            .spawn(move || {
                while let Ok(state) = jobs.recv() {
                    write(state);
                }
            })?;
        Ok(Self {
            tx: Some(tx),
            pending,
            thread: Some(thread),
        })
    }

    fn submit(&self, state: SavedState) {
        let tx = self.tx.as_ref().unwrap();
        if let Err(TrySendError::Full(state)) = tx.try_send(state) {
            let _ = self.pending.try_recv();
            let _ = tx.try_send(state);
        }
    }
}

impl Drop for StateWriter {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl StateFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            writer: None,
        }
    }

    pub fn submit(&mut self, state: SavedState) -> std::io::Result<()> {
        if self.writer.is_none() {
            let file = Self::new(self.path.clone());
            let mut previous = None;
            self.writer = Some(StateWriter::start(move |state| {
                if previous.as_ref() == Some(&state) {
                    return;
                }
                match file.save(&state) {
                    Ok(()) => previous = Some(state),
                    Err(error) => tracing::warn!(%error, "saving playback state"),
                }
            })?);
        }
        self.writer.as_ref().unwrap().submit(state);
        Ok(())
    }

    /// Shutdown drains the latest pending snapshot before the core exits.
    pub fn finish(&mut self) {
        self.writer.take();
    }

    /// A missing or unreadable file is an empty state, not an error: playback must start.
    pub fn load(&self) -> SavedState {
        let value: serde_json::Value = match std::fs::read(&self.path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(err) => {
                    tracing::warn!("ignoring unreadable {}: {err}", self.path.display());
                    return SavedState::default();
                }
            },
            Err(_) => return SavedState::default(),
        };
        let Some(obj) = value.as_object() else {
            tracing::warn!("ignoring {}: not a JSON object", self.path.display());
            return SavedState::default();
        };
        let section = |name: &str| obj.get(name).filter(|v| !v.is_null()).cloned();
        let warn = |name: &str, err: serde_json::Error| {
            tracing::warn!(
                "{}: ignoring the unreadable {name} section: {err}",
                self.path.display()
            );
        };
        match obj.get("version").and_then(|v| v.as_u64()) {
            None => {
                // Before versioning: the queue fields at the top level, tracks without
                // entry ids, the contract's context and settings shapes of the time.
                let legacy = LegacyState::deserialize(&value).unwrap_or_else(|err| {
                    warn("queue", err);
                    LegacyState::default()
                });
                let audio = section("audio")
                    .map(|v| {
                        serde_json::from_value(v).unwrap_or_else(|err| {
                            warn("audio", err);
                            SavedAudio::default()
                        })
                    })
                    .unwrap_or_default();
                SavedState {
                    queue: SavedQueue {
                        entries: legacy
                            .tracks
                            .iter()
                            .map(|&track| SavedEntry { id: 0, track })
                            .collect(),
                        current_index: legacy.current_index,
                        position_ms: legacy.position_ms,
                        context: legacy.context,
                        shuffle: legacy.shuffle,
                        repeat: legacy.repeat,
                    },
                    audio,
                    next_entry_id: 0,
                    queue_revision: 0,
                }
            }
            Some(version) => {
                if version > STATE_VERSION {
                    tracing::warn!(
                        "{} is version {version}, newer than this build ({STATE_VERSION}); \
                         reading what it can",
                        self.path.display()
                    );
                }
                let queue = section("queue")
                    .map(|v| {
                        serde_json::from_value(v).unwrap_or_else(|err| {
                            warn("queue", err);
                            SavedQueue::default()
                        })
                    })
                    .unwrap_or_default();
                let audio = section("audio")
                    .map(|v| {
                        serde_json::from_value(v).unwrap_or_else(|err| {
                            warn("audio", err);
                            SavedAudio::default()
                        })
                    })
                    .unwrap_or_default();
                SavedState {
                    queue,
                    audio,
                    next_entry_id: obj
                        .get("next_entry_id")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    queue_revision: obj
                        .get("queue_revision")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                }
            }
        }
    }

    /// Writes to a temporary file and renames it over the old one, so a crash or power
    /// loss leaves either the old or the new state, never half of one.
    pub fn save(&self, state: &SavedState) -> std::io::Result<()> {
        let json = serde_json::json!({
            "version": STATE_VERSION,
            "next_entry_id": state.next_entry_id,
            "queue_revision": state.queue_revision,
            "queue": state.queue,
            "audio": state.audio,
        });
        let bytes = serde_json::to_vec(&json).map_err(std::io::Error::other)?;
        let tmp = temp_path(&self.path);
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &self.path)
    }
}

/// The unversioned file of A3 and before.
#[derive(Debug, Default, Deserialize)]
struct LegacyState {
    #[serde(default)]
    tracks: Vec<i64>,
    #[serde(default)]
    current_index: Option<u32>,
    #[serde(default)]
    position_ms: u64,
    #[serde(default, deserialize_with = "lenient")]
    context: Option<SavedContext>,
    #[serde(default)]
    shuffle: bool,
    #[serde(default, deserialize_with = "lenient")]
    repeat: Option<SavedRepeat>,
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::*;

    #[test]
    fn blocked_disk_write_does_not_block_submission_and_only_latest_pending_state_is_written() {
        let (entered, started) = bounded(1);
        let (release, gate) = bounded(1);
        let (written, results) = bounded(2);
        let writer = StateWriter::start(move |state| {
            if state.queue.position_ms == 1 {
                entered.send(()).unwrap();
                gate.recv().unwrap();
            }
            written.send(state.queue.position_ms).unwrap();
        })
        .unwrap();
        let state = |position_ms| SavedState {
            queue: SavedQueue {
                position_ms,
                ..Default::default()
            },
            ..Default::default()
        };
        writer.submit(state(1));
        started
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let (submitted, completed) = bounded(1);
        let producer = std::thread::spawn(move || {
            writer.submit(state(2));
            writer.submit(state(3));
            submitted.send(()).unwrap();
            drop(writer); // Drains the latest state at shutdown.
        });
        let unblocked = completed.recv_timeout(std::time::Duration::from_secs(5));
        release.send(()).unwrap();
        producer.join().unwrap();
        assert!(unblocked.is_ok(), "submitting state waited for disk");
        assert_eq!(results.try_iter().collect::<Vec<_>>(), [1, 3]);
    }

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("core-state-{name}-{}.json", std::process::id()))
    }

    #[test]
    fn round_trip_and_tolerates_garbage() {
        let path = temp("roundtrip");
        let file = StateFile::new(&path);
        assert_eq!(file.load(), SavedState::default());
        let state = SavedState {
            queue: SavedQueue {
                entries: vec![
                    SavedEntry { id: 7, track: 4 },
                    SavedEntry { id: 9, track: 2 },
                ],
                current_index: Some(1),
                position_ms: 61_234,
                context: Some(SavedContext::Album { key: "a|b".into() }),
                shuffle: true,
                repeat: Some(SavedRepeat::All),
            },
            audio: SavedAudio {
                output: None,
                volume: 0.5,
                muted: true,
                replay_gain: Some(SavedReplayGain::from_contract(
                    &ReplayGainSettings::default(),
                )),
            },
            next_entry_id: 10,
            queue_revision: 44,
        };
        file.save(&state).unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(file.load(), state);
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(file.load(), SavedState::default());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_bad_section_does_not_reset_the_others() {
        let path = temp("partial");
        let file = StateFile::new(&path);
        std::fs::write(
            &path,
            r#"{"version":1,"next_entry_id":12,"queue_revision":3,
                "queue":{"entries":[{"id":5,"track":1}],"current_index":0,"position_ms":10,
                         "context":{"type":"radio","station":1},"shuffle":false,"repeat":"spiral"},
                "audio":{"volume":"loud"}}"#,
        )
        .unwrap();
        let state = file.load();
        assert_eq!(state.queue.entries, [SavedEntry { id: 5, track: 1 }]);
        assert_eq!(state.queue.context, None, "an unknown context is dropped");
        assert_eq!(state.queue.repeat, None);
        assert_eq!(state.audio, SavedAudio::default());
        assert_eq!((state.next_entry_id, state.queue_revision), (12, 3));

        std::fs::write(
            &path,
            r#"{"version":1,"queue":[1,2],"audio":{"volume":0.25,"muted":false}}"#,
        )
        .unwrap();
        let state = file.load();
        assert_eq!(state.queue, SavedQueue::default());
        assert_eq!(state.audio.volume, 0.25);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reads_files_from_before_versioning() {
        let path = temp("legacy");
        let file = StateFile::new(&path);
        std::fs::write(
            &path,
            r#"{"tracks":[4,2],"current_index":1,"position_ms":500,
                "context":{"type":"query","query":{"filter":{"type":"compare","field":{"type":"year"},"op":"eq","value":{"int":2000}},"sort":[]}},
                "shuffle":true,"repeat":"one",
                "audio":{"volume":0.5,"muted":false,"replay_gain":{"mode":"track","preamp_db":1.0,"fallback_preamp_db":0.0,"prevent_clipping":true}}}"#,
        )
        .unwrap();
        let state = file.load();
        assert_eq!(
            state.queue.entries,
            [
                SavedEntry { id: 0, track: 4 },
                SavedEntry { id: 0, track: 2 }
            ]
        );
        assert_eq!(state.queue.current_index, Some(1));
        assert_eq!(state.queue.repeat, Some(SavedRepeat::One));
        // The old value shape cannot be read back as a query: the context goes, the
        // queue stays.
        let context = state.queue.context.and_then(|c| c.to_contract());
        assert_eq!(context, None);
        assert_eq!(state.audio.volume, 0.5);
        assert_eq!(
            state.audio.replay_gain.unwrap().to_contract().mode,
            ReplayGainMode::Track
        );
        std::fs::remove_file(&path).unwrap();
    }
}

#[cfg(all(test, feature = "desktop-fs"))]
mod output_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn old_audio_defaults_output_and_unknown_output_enums_only_drop_that_section() {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let file = StateFile::new(dir.path().join("state.json"));
        let original = json!({
        "version":1,
        "queue":{
        "entries":[{
        "id":3,
        "track":7}
        ],
        "position_ms":321}
        ,
        "audio":{
        "volume":0.4,
        "muted":true,
        "replay_gain":{
        "mode":"track",
        "preamp_db":2.0}
        }
        }
        );

        std::fs::write(&file.path, original.to_string()).unwrap();
        let baseline = file.load();
        assert!(baseline.audio.output.is_none());
        assert_eq!(baseline.audio.replay_gain.unwrap().preamp_db, 2.0);
        for (key, value) in [
            ("selection", json!({"type":"future"})),
            ("mode", json!("future")),
            ("fallback", json!("future")),
            ("rate_policy", json!("future")),
        ] {
            let mut saved = original.clone();
            saved["audio"]["output"] = json!({
            "selection":{
            "type":"system_default"}
            ,
            "mode":"shared",
            "fallback":"pause",
            "rate_policy":"mix"}
            );

            saved["audio"]["output"][key] = value;
            std::fs::write(&file.path, saved.to_string()).unwrap();
            assert_eq!(file.load(), baseline, "{key}");
        }
    }

    #[test]
    fn persistent_output_settings_round_trip_in_version_one() {
        use crate::contract::{
            OutputDeviceIdStability, OutputDeviceInfo, OutputDeviceSelection, OutputSettings,
        };
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let file = StateFile::new(dir.path().join("state.json"));
        let mut state = SavedState::default();
        state.audio.output = Some(SavedOutput {
            settings: OutputSettings {
                selection: OutputDeviceSelection::Device {
                    id: "cpal:uid".into(),
                },
                settings_revision: 23,
                ..Default::default()
            },
            device: Some(OutputDeviceInfo {
                id: "cpal:uid".into(),
                name: "Last known name".into(),
                available: false,
                is_default: false,
                id_stability: OutputDeviceIdStability::Persistent,
            }),
        });
        file.save(&state).unwrap();
        assert_eq!(file.load(), state);
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file.path).unwrap()).unwrap();
        assert_eq!(saved["version"], 1);
        assert_eq!(saved["audio"]["output"]["settings_revision"], 23);
    }
}
