//! Shared backends and frozen client inputs for tests and external test consumers.

pub mod local_storage;

#[path = "../../../test-data/client_parity.rs"]
pub mod client_parity;

#[cfg(feature = "core")]
pub mod core;
