//! Output preferences, device capabilities, and runtime status.

use super::*;

/// Requested preferences, separate from the verified active stream.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct OutputSettings {
    #[serde(default)]
    /// Requested identity; defaults to following the system default route.
    pub selection: OutputDeviceSelection,
    #[serde(default)]
    /// Optimistic preference revision shared by device and mode updates; no-ops do not increment it.
    pub settings_revision: u64,
    #[serde(default)]
    /// Requested access mode, not proof of an exclusive stream.
    pub mode: OutputShareMode,
    /// Stored on cpal, but inactive: unsupported modes are refused before commit.
    #[serde(default)]
    /// Explicit runtime fallback policy. Stored but inactive on the current cpal build.
    pub fallback: OutputFallbackPolicy,
    #[serde(default)]
    /// Requested sample-rate policy; a supported policy can still fail on a particular endpoint.
    pub rate_policy: OutputRatePolicy,
}

/// Requested routing identity. Device names are never used to resolve an ID.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputDeviceSelection {
    /// Follow the system default route.
    #[default]
    SystemDefault,
    /// Use the opaque id even when absent; the preference may remain unavailable until changed.
    Device { id: String },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// Cached endpoint description; availability does not promise that opening succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OutputDeviceInfo {
    /// Opaque endpoint identity; compare for equality, never reconstruct from a display name.
    pub id: String,
    /// Display name; an unseen ID is displayed as the ID until discovery supplies a name.
    pub name: String,
    /// Whether the latest discovery identifies this endpoint as the current system default.
    pub is_default: bool,
    /// Whether the cached discovery currently sees this endpoint; it is not proof that opening it will succeed.
    pub available: bool,
    /// Whether this identity can be persisted; session identities expire when core restarts.
    pub id_stability: OutputDeviceIdStability,
}

/// Capability evidence for an endpoint, separate from active stream facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OutputDeviceCapabilities {
    /// Backend supplying this capability evidence.
    pub backend: OutputBackend,
    /// Whether the backend/endpoint reports exclusive access support, not whether it is acquired.
    pub exclusive_access: CapabilitySupport,
    /// Whether the backend/endpoint can attempt source-rate switching.
    pub source_rate_switch: CapabilitySupport,
    /// Capability to deliver rendered samples unchanged to the endpoint driver boundary. It excludes driver/firmware/DAC processing and is not a bit-perfect verdict.
    pub bit_exact_transport: CapabilitySupport,
    /// Whether the physical endpoint route is known, independent of bit-perfect evidence.
    pub physical_route_known: bool,
    /// Discovered shared mix format, or null when not established.
    pub mix_format: Option<SignalFormat>,
    #[serde(default)]
    /// Bounded supported exclusive ranges, not an active format.
    pub exclusive_formats: Vec<OutputFormatRange>,
    /// True only when exclusive_formats is a complete inventory; false also covers unknown or unqueried ranges.
    pub formats_complete: bool,
}

/// Supported PCM container and rate range, with independent effective precision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OutputFormatRange {
    /// Number of channels supported by this range.
    pub channels: u16,
    /// PCM container representation; I24 is packed three-byte signed PCM.
    pub sample_format: SampleFormat,
    /// Effective integer PCM precision within the container; null means unknown or floating point.
    pub valid_bits_per_sample: Option<u32>,
    /// Inclusive minimum rate in Hz.
    pub sample_rate_min: u32,
    /// Inclusive maximum rate in Hz.
    pub sample_rate_max: u32,
}

/// Authoritative output application state, separate from requested preferences.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct OutputStateInfo {
    /// Preference revision these active facts are associated with.
    pub settings_revision: u64,
    /// Output/discovery state generation used to reject stale results.
    pub generation: u64,
    /// Stream readiness, independent of transport play/pause: an open paused stream is ready.
    pub status: OutputStatus,
    /// Verified active stream facts, or null when no stream is established.
    pub actual: Option<OutputActualInfo>,
    /// Latest output application failure, separate from a successful preference receipt; null when cleared.
    pub failure: Option<OutputFailureInfo>,
    /// Reported downgrade or expired session identity; null when none applies.
    pub fallback: Option<OutputFallbackInfo>,
}

/// Active endpoint facts verified only up to the endpoint driver boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OutputActualInfo {
    /// Opaque identity of the active endpoint.
    pub device_id: String,
    /// Active endpoint display name.
    pub name: String,
    /// Active access mode. Exclusive with exclusive_verified=false is unverified and cannot establish bit-perfect Yes.
    pub share_mode: OutputShareMode,
    /// Backend owning the active stream.
    pub backend: OutputBackend,
    /// Active container format and independently known effective precision.
    pub format: SignalFormat,
    /// Known physical-route identity, or null when not established.
    pub physical_device_id: Option<String>,
    /// True only with evidence of exclusive ownership at the endpoint driver boundary; says nothing about processing inside the driver, firmware or DAC.
    pub exclusive_verified: bool,
    /// Evidence that rendered samples reach the endpoint driver boundary unchanged; excludes driver/firmware/DAC internals. Clients use SignalPathInfo.bit_perfect as the sole verdict.
    pub sample_integrity: SampleIntegrity,
}

/// Runtime failure to apply a committed output preference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OutputFailureInfo {
    /// Open error code for the runtime application failure, normally of kind failed.
    pub code: ErrorCode,
    /// English diagnostic text; clients localize by code.
    pub message: String,
    /// Whether a later explicit retry may succeed; never authorizes automatic playback.
    pub retryable: bool,
}

/// Reported downgrade or expired session identity; not a complete format-negotiation record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OutputFallbackInfo {
    /// Why actual output differs, including session identity expiry before a stream exists.
    pub reason: OutputFallbackReason,
    /// Requested access mode at the time of fallback. For device_identity_expired it is contextual only, not a failed exclusive request.
    pub requested_mode: OutputShareMode,
    /// Actual fallback access mode; null when no stream exists, including device_identity_expired. Rate-policy downgrades are not separately represented by this record; inspect the actual format.
    pub actual_mode: Option<OutputShareMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputDeviceIdStability {
    /// The backend reports this identity as restorable across core runs.
    Persistent,
    /// Valid only in the current run; the conservative default.
    #[default]
    Session,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputShareMode {
    /// Use the platform shared mixer.
    #[default]
    Shared,
    /// Request exclusive ownership; only exclusive_verified establishes acquisition evidence.
    Exclusive,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputFallbackPolicy {
    /// Pause rather than silently substitute an access mode.
    #[default]
    Pause,
    /// Allow an explicitly reported shared fallback when a supported backend cannot acquire the requested mode.
    Shared,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputRatePolicy {
    /// Request the backend mix format.
    #[default]
    Mix,
    /// Request source rate/format; supported implementations may still fail at runtime.
    Source,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputBackend {
    /// Current shared-mode cpal backend.
    #[default]
    Cpal,
    /// Windows WASAPI backend identity; this vocabulary does not promise implementation.
    Wasapi,
    /// Apple Core Audio backend identity; this vocabulary does not promise implementation.
    CoreAudio,
    /// ALSA backend identity; this vocabulary does not promise implementation.
    Alsa,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// Evidence-based capability support; default is undetermined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CapabilitySupport {
    /// Evidence indicates this capability can be attempted; runtime success is not promised.
    Supported,
    /// This capability is known not to be available in the reported context.
    Unsupported,
    #[default]
    /// Support has not been established; the conservative default.
    Undetermined,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// Evidence about unchanged transport to the endpoint driver boundary, never a complete bit-perfect verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SampleIntegrity {
    /// Backend evidence shows rendered samples delivered unchanged to the endpoint driver boundary only; it is not a complete bit-perfect verdict.
    Verified,
    #[default]
    /// Unchanged transport to the endpoint driver boundary has not been established.
    Unverified,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// Output readiness independent of transport play/pause state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputStatus {
    #[default]
    /// No active stream is established and no current rebuild is in progress.
    Idle,
    /// An active stream is established; transport may be playing or paused.
    Ready,
    /// Output reconstruction or initial opening is in progress.
    Rebuilding,
    /// The selected output could not be established; inspect failure and retain the preference.
    Unavailable,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// Cause of a reported preference downgrade or identity reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputFallbackReason {
    /// Runtime exclusive acquisition failed, for example because another process owns the endpoint.
    ExclusiveUnavailable,
    /// The current endpoint cannot satisfy the committed mode; not a build-wide command refusal.
    ModeUnsupported,
    /// No active format satisfies the committed policy on this endpoint.
    FormatUnsupported,
    /// A saved session-only identity expired; selection reset to system_default, with actual_mode null until output opens.
    DeviceIdentityExpired,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}
