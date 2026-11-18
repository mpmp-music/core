//! Lyrics and waveform data.

use super::*;

/// A track's lyrics, parsed from plain text, LRC or enhanced (word-timed) LRC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Lyrics {
    pub source: LyricsSource,
    pub format: LyricsFormat,
    /// By time (stable); in the text's order when `format` is `plain`.
    pub lines: Vec<LyricsLine>,
    /// The decoded text the lyrics were parsed from, as found (at most 1 MB).
    pub raw: String,
}

/// Where lyrics were found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LyricsSource {
    /// A `.lrc` file with the track's name, in its folder.
    File,
    /// The track's own tags (`©lyr`, `LYRICS`, `USLT`).
    Embedded,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// What the lyrics' text looked like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LyricsFormat {
    /// No timestamps: every line has `start_ms: null`.
    Plain,
    /// Lines with timestamps (`[mm:ss.xx]`).
    Lrc,
    /// LRC with word timestamps (`<mm:ss.xx>`): some lines have `words`.
    EnhancedLrc,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// One line of lyrics. A line with several timestamps in the text is one line for each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LyricsLine {
    /// Milliseconds from the start of the audio file (also for CUE), with the text's `[offset:]` applied
    /// (never below 0); `null` when `format` is `plain`.
    pub start_ms: Option<u64>,
    /// The line without timestamps; for a line with `words`, the words put together.
    pub text: String,
    /// Timing of each word (or syllable) of the line; left out when the line has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<LyricsWord>,
    /// A second line written at the same timestamp (a translation, by the convention of
    /// Chinese players); several such lines are joined with `\n`. Left out when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translation: Option<String>,
}

/// A word of an enhanced LRC line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LyricsWord {
    /// Milliseconds from the start of the audio file (also for CUE), `[offset:]` applied.
    pub start_ms: u64,
    /// The word as written, with the space after it if the text has one.
    pub text: String,
}

/// A track's waveform on its playback timeline (encoder delay and padding trimmed; no
/// resampling, no ReplayGain, no volume): the track is cut into `buckets` stretches of equal
/// length (2048 in this version), from its first frame to its last. `channels` has one
/// entry for a mono file and two otherwise (files with more channels are mixed down to
/// left and right).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Waveform {
    pub buckets: u32,
    pub channels: Vec<WaveformChannel>,
}

/// One channel of a `Waveform`; each array has `buckets` values. A track shorter than
/// `buckets` frames leaves some buckets empty: 0, 0, 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WaveformChannel {
    /// The smallest sample of each bucket: -128…127 for -1.0…1.0 (`round(x * 128)`).
    pub min: Vec<i8>,
    /// The largest sample of each bucket, on the same scale.
    pub max: Vec<i8>,
    /// The RMS of each bucket: 0…255 for 0…1.0.
    pub rms: Vec<u8>,
}
