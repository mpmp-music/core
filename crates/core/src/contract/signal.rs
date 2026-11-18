//! Signal path formats, stages, gain provenance, and loudness.

use super::*;

/// What happens to the samples between the file and the device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SignalPathInfo {
    /// Lower-case codec id.
    pub codec: String,
    pub lossless: bool,
    pub source_rate: u32,
    pub source_channels: u16,
    pub source_bits_per_sample: Option<u32>,
    pub stages: Vec<StageInfo>,
    pub output_rate: u32,
    pub output_channels: u16,
    pub output_format: SampleFormat,
    pub bit_perfect: BitPerfectInfo,
    /// Stable causes of the verdict; empty for Yes. Independent of English text.
    /// Feature `structured_signal_path`.
    #[serde(default)]
    pub reason_codes: Vec<SignalReasonCode>,
    /// Observed active access mode; exclusive alone is not a bit-perfect verdict.
    #[serde(default)]
    pub share_mode: Option<OutputShareMode>,
    /// Opaque active endpoint identity, or null when unknown.
    #[serde(default)]
    pub output_device_id: Option<String>,
    /// Active backend identity, or null on older messages.
    #[serde(default)]
    pub backend: Option<OutputBackend>,
    /// Rendered-sample integrity at the endpoint driver boundary only. Driver/firmware/DAC internals are outside this evidence; use bit_perfect as the sole conclusion.
    #[serde(default)]
    pub sample_integrity: Option<SampleIntegrity>,
}

/// The output's sample format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SampleFormat {
    F32,
    I16,
    /// Packed signed 24-bit PCM in three bytes. For 24 valid bits in a 32-bit container use I32 and valid_bits_per_sample=24.
    I24,
    I32,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StageInfo {
    /// For display (an open set).
    pub name: String,
    pub detail: String,
    pub alters_samples: bool,
    /// Stable translation key. Unknown on messages predating structured_signal_path.
    #[serde(default)]
    pub kind: SignalStageKind,
    /// Final applied dB, including preamp and clipping protection. Null for silence
    /// or stages without gain; volume silence is distinguished by `silent`.
    #[serde(default)]
    pub gain_db: Option<f32>,
    #[serde(default)]
    pub gain_source: Option<GainDataSource>,
    #[serde(default)]
    pub gain_scope: Option<GainScope>,
    #[serde(default)]
    pub input_format: Option<SignalFormat>,
    #[serde(default)]
    pub output_format: Option<SignalFormat>,
    #[serde(default)]
    pub silent: Option<bool>,
}

/// Stable identifier (structured_signal_path); unknown is a decode-only fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SignalStageKind {
    Resample,
    Channels,
    WordLength,
    ReplayGain,
    Volume,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[default]
    #[serde(other)]
    Unknown,
}

/// Stable identifier (structured_signal_path); unknown is a decode-only fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SignalReasonCode {
    LossySource,
    Resample,
    Channels,
    /// Output conversion narrows source word length, including its legacy quantization/dither path.
    WordLength,
    ReplayGain,
    Volume,
    SharedMixer,
    /// Known loss of effective PCM precision, even when container width is unchanged; may accompany word_length and dither.
    PrecisionLoss,
    /// Dither was applied to samples. Legacy conversion paths may report the coarser word_length reason instead; codes may coexist.
    Dither,
    /// The backend performs known sample-altering processing.
    BackendProcessing,
    /// Evidence for unchanged transport is incomplete; an uncertainty reason, not proof of an alteration.
    PathUnverified,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[default]
    #[serde(other)]
    Unknown,
}

/// Stable identifier (structured_signal_path); unknown is a decode-only fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GainDataSource {
    Tags,
    Automatic,
    Fallback,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[default]
    #[serde(other)]
    Unknown,
}

/// Stable identifier (structured_signal_path); unknown is a decode-only fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GainScope {
    Track,
    Album,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[default]
    #[serde(other)]
    Unknown,
}

/// Source and device formats for a format-changing stage (overall conversion,
/// not the intermediate DSP buffer representation). Source word length can be unknown;
/// sample_format is null for encoded input, set for the device format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SignalFormat {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// PCM container bits on output; source word length on encoded input. Null means unknown. Effective integer precision is reported separately by valid_bits_per_sample.
    pub bits_per_sample: Option<u32>,
    /// Container representation, or null for encoded input/unknown representation.
    pub sample_format: Option<SampleFormat>,
    /// Valid integer PCM bits within the container; null for unknown precision or floating
    /// point. I32 with 24 valid bits represents 24-in-32; I24 is packed three-byte PCM.
    #[serde(default)]
    pub valid_bits_per_sample: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BitPerfectInfo {
    Yes,
    No {
        reasons: Vec<String>,
    },
    /// Core cannot tell (e.g. the system mixer may resample).
    Uncertain {
        reason: String,
    },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// Computed EBU R128 values, before playback DSP. The source is decoded audio,
/// target -18 LUFS; peaks are linear sample peaks, not true peaks. Null LUFS means
/// silence, below the gate, or too short for a valid 400 ms measurement block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LoudnessInfo {
    pub integrated_lufs: Option<f64>,
    pub sample_peak: f64,
    pub album_lufs: Option<f64>,
    pub album_peak: Option<f64>,
    pub measured_at_ms: i64,
}
