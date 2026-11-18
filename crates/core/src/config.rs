//! Startup configuration and event subscription options.

use crate::Locations;
use mpmp_engine::EngineConfig;
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(Clone, Default)]
pub struct CoreConfig {
    /// A host with a watcher owns the startup baseline, after registering notifications.
    /// The host must start that baseline (including upgrade rescans).
    pub startup_scan_by_host: bool,
    /// Default true: background measurement after startup and library changes.
    /// Hosts/tests may opt out; this does not change the user's RG playback mode.
    pub automatic_loudness_scan: Option<bool>,
    /// How long background measurement waits before trying a track again that could not
    /// be measured (doubling with each further failure of the same file version, up to 30
    /// minutes); `None`: 30 seconds. For tests.
    pub automatic_loudness_retry: Option<Duration>,
    pub engine: EngineConfig,
    /// Suspend-aware elapsed clock; injectable for deterministic deadline tests.
    pub playback_clock: Option<Arc<dyn mpmp_engine::SuspendClock>>,
    /// Where queue and position survive restarts ; `None` keeps nothing (ids and
    /// revisions still start from the current Unix time, so they are not reused).
    pub state_file: Option<PathBuf>,
    /// The platform layer's rules for location strings (permission scopes); `None`: the
    /// desktop rules when `desktop-fs` is enabled; otherwise startup returns InvalidInput.
    pub locations: Option<Arc<dyn Locations>>,
    /// How long one agent command may wait on the file system (`locations`, and core's
    /// checks of the paths an agent names) before core goes on as if it could not tell;
    /// `None`: [`crate::AGENT_FILE_SYSTEM_TIMEOUT`]. For tests; the UI's commands never wait on a
    /// limit.
    pub agent_file_system_timeout: Option<Duration>,
    /// How long a confirmation request waits for the user's answer before it expires;
    /// `None`: five minutes. For tests.
    pub confirmation_ttl: Option<Duration>,
}

impl std::fmt::Debug for CoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreConfig")
            .field("startup_scan_by_host", &self.startup_scan_by_host)
            .field("automatic_loudness_scan", &self.automatic_loudness_scan)
            .field("automatic_loudness_retry", &self.automatic_loudness_retry)
            .field("engine", &self.engine)
            .field("state_file", &self.state_file)
            .field("locations", &self.locations.as_ref().map(|_| "platform"))
            .field("agent_file_system_timeout", &self.agent_file_system_timeout)
            .field("confirmation_ttl", &self.confirmation_ttl)
            .finish()
    }
}

/// What a subscriber wants to receive. More options may be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SubscribeOptions {
    /// `Position` events (on by default). Without them a client still gets positions in
    /// every snapshot and interpolates.
    pub positions: bool,
}

impl Default for SubscribeOptions {
    fn default() -> Self {
        Self { positions: true }
    }
}

impl SubscribeOptions {
    pub fn with_positions(mut self, positions: bool) -> Self {
        self.positions = positions;
        self
    }
}
