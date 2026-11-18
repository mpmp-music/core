//! Playback snapshots, limits, audio settings, and queue state.

use super::*;

/// Everything a client needs to draw the player, consistent as of `seq` (guarantee 4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    /// `seq` of the last non-`Position` event reflected here (guarantee 4).
    pub seq: u64,
    pub contract_version: u32,
    pub contract_minor: u32,
    /// API capabilities available in this core build.
    pub features: Vec<String>,
    /// Core's clock when the snapshot was taken (the clock of `PositionInfo.at_ms`), so a
    /// client can interpolate positions exactly from the first frame.
    pub now_ms: u64,
    pub state: PlayState,
    pub current: Option<NowPlaying>,
    pub position: Option<PositionInfo>,
    pub queue: QueueSummary,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub audio: AudioSettings,
    /// Run-local transport conditions, or null on older peers.
    #[serde(default)]
    pub playback_limits: Option<PlaybackLimitsInfo>,
    /// Active output state, or null on older peers; requested settings live in audio.output.
    #[serde(default)]
    pub output: Option<OutputStateInfo>,
    /// Problems for the app to show, once per code, oldest first. Data problems
    /// last until core restarts; `output_failed` clears when output opens or playback stops.
    /// Changes are announced by `StateChanged`. Empty when all is well.
    #[serde(default)]
    pub problems: Vec<Problem>,
}

/// Conditions for this core run, cleared together when either is reached.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct PlaybackLimitsInfo {
    /// Bound playback instance, or null when no stop boundary is armed.
    pub stop_after_current: Option<StopAfterCurrentInfo>,
    /// Run-local sleep deadline sample, or null when disarmed.
    pub sleep_timer: Option<SleepTimerInfo>,
}

/// Accepted playback-instance boundary identified by its queue entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StopAfterCurrentInfo {
    /// Queue entry whose accepted playback instance owns the stop boundary.
    pub entry: EntryId,
}

/// Elapsed deadline sample; display interpolation is advisory and must refresh after wake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SleepTimerInfo {
    /// Remaining elapsed milliseconds at sampled_at_ms; clamp display interpolation to zero.
    pub remaining_ms: u64,
    /// Snapshot.now_ms clock for display interpolation only. Re-read PlaybackLimits after wake or becoming visible: this display clock may exclude suspend.
    pub sampled_at_ms: u64,
    /// True when the timer clock includes system suspend. False means limited precision:
    /// suspend time is excluded. Null on older peers or when the property is unknown.
    #[serde(default)]
    pub includes_suspend: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlaybackLimitReason {
    /// The armed instance ended naturally; both conditions are consumed.
    EndOfCurrent,
    /// The elapsed sleep deadline was reached; both conditions are consumed.
    SleepTimer,
    /// The guarded current instance failed; no automatic successor crosses its boundary.
    CurrentFailed,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlaybackLimitAction {
    /// Pause active playback; already stopped playback remains stopped.
    Pause,
    /// Stop at the accepted instance boundary without an automatic successor.
    Stop,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// A current problem the app should show (`Snapshot.problems`). For damaged data, core
/// never deletes or rebuilds it: the app decides. `output_failed` is transient and clears
/// when output opens or playback stops; it does not mark the track as damaged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Problem {
    /// What is wrong (an error code of kind `failed`): `permissions_unavailable` (the
    /// agents and grants could not be read at startup: agents' commands fail with it),
    /// `library_damaged` (the library database is damaged), `output_failed` (no usable
    /// output after an open attempt; retrying playback is allowed). Open: more may come.
    pub code: ErrorCode,
    /// English text for logs (UIs localize by `code`).
    pub message: String,
    /// Unix ms when core noticed it.
    pub since_ms: i64,
}

/// Volume and ReplayGain settings.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct AudioSettings {
    /// Slider position, 0.0–1.0; in `software` mode the gain is `volume³`.
    pub volume: f32,
    /// The volume in dB; `null` when silent (volume 0) or when core cannot tell (e.g. a
    /// system volume mode whose scale is unknown).
    pub volume_db: Option<f32>,
    pub muted: bool,
    pub volume_mode: VolumeMode,
    pub replay_gain: ReplayGainSettings,
    #[serde(default)]
    pub output: Option<OutputSettings>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum VolumeMode {
    /// Gain inside the player; bit-perfect only at 100 %.
    #[default]
    Software,
    /// The system's volume for the output device (not implemented yet).
    System,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReplayGainSettings {
    pub mode: ReplayGainMode,
    /// Added to the track's or album's ReplayGain value.
    pub preamp_db: f32,
    /// Gain for tracks without ReplayGain values.
    pub fallback_preamp_db: f32,
    /// Lower the gain so the known peak does not clip.
    pub prevent_clipping: bool,
}

impl Default for ReplayGainSettings {
    fn default() -> Self {
        Self {
            mode: ReplayGainMode::Smart,
            preamp_db: 0.0,
            fallback_preamp_db: 0.0,
            prevent_clipping: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReplayGainMode {
    Off,
    Track,
    Album,
    /// Album gain while an album plays in order, track gain otherwise.
    #[default]
    Smart,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlayState {
    #[default]
    Stopped,
    Playing,
    Paused,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NowPlaying {
    pub entry: EntryId,
    /// As loaded when the entry became current (statistics may be older: re-read the
    /// track on `TracksChanged` / `HistoryChanged`).
    pub track: TrackInfo,
    /// What happens to the samples; `null` while stopped.
    pub signal_path: Option<SignalPathInfo>,
}

/// Where playback is. Clients interpolate: `position_ms + (now - at_ms) * rate`, with
/// `at_ms` on core's clock (`Snapshot.now_ms` and events carry it; remote clients estimate
/// the offset). `rate` is 0 while paused or stopped.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PositionInfo {
    pub entry: EntryId,
    pub position_ms: u64,
    pub at_ms: u64,
    pub rate: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct QueueSummary {
    pub revision: QueueRevision,
    /// Entries in the queue.
    pub entry_count: u32,
    pub current_index: Option<u32>,
    /// Candidate for Next, even while stopped; null when no successor exists.
    /// Does not skip unavailable tracks. Feature `queue_candidates`.
    #[serde(default)]
    pub next_entry: Option<EntryId>,
    /// Candidate at natural completion (hypothetical while paused/stopped).
    /// Repeat one holds the current entry; Next bypasses repeat one.
    #[serde(default)]
    pub natural_next_entry: Option<EntryId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QueueEntry {
    pub entry: EntryId,
    pub track: TrackInfo,
}

/// A track's ReplayGain values: computed cache values take precedence over file tags.
/// Gains are in dB on the ReplayGain 2.0 scale (the gain that brings the track or album to
/// −18 LUFS; values on other scales, such as Opus R128 tags at −23 LUFS, are converted).
/// Peaks are linear sample peaks, 1.0 = full scale.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct ReplayGainInfo {
    pub track_gain_db: Option<f32>,
    pub track_peak: Option<f32>,
    pub album_gain_db: Option<f32>,
    pub album_peak: Option<f32>,
    /// Provenance of the selected gain/peak pair; Unknown for older catalogs.
    #[serde(default)]
    pub track_source: GainDataSource,
    #[serde(default)]
    pub album_source: GainDataSource,
}
