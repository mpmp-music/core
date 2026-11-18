//! Core: the play queue, play state, history and the contract every client speaks.
//! One actor thread owns the state; clients hold a [`CoreHandle`].
//!
//! Adapters that speak JSON (the desktop WebView, FFI, remote, agents) use
//! [`CoreHandle::execute_json`] / [`CoreHandle::query_json`], which apply the contract's
//! strict parsing rules and answer every malformed request with a contract error.

mod actor;
pub mod catalog;
mod config;
pub mod contract;
mod gain;
mod handle;
mod jobs;
mod json;
mod loudness_scan;
mod permissions;
mod queue;
mod root_access;
mod state_file;
mod waveform_worker;

pub use catalog::{
    Catalog, CoverArt, FileVersion, PlayRecord, PlaylistOutcome, Recorded, ScanEvent, ScanResult,
    ScanTicket, Undone,
};
pub use config::{CoreConfig, SubscribeOptions};
pub use handle::{
    AgentHandle, ConfirmationSnapshot, CoreHandle, FileUndoOccupancy, ShutdownError, Subscription,
};
pub use permissions::fs_deadline::{AGENT_FILE_SYSTEM_CALLS, AGENT_FILE_SYSTEM_TIMEOUT};
pub use permissions::{
    Doubt, Locations, MAX_AGENT_NAME, MAX_LOCATION, MAX_PLATFORM, MAX_PREFIX, MAX_SESSION,
    StoredAgent, StoredPermissions, grantable as permission_grantable,
    takes_scope as permission_takes_scope,
};
pub use root_access::{
    RootAccessHandler, RootAccessRegistration, ScanCoverage, ScanPreparationHandler,
};
