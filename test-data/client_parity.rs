//! Shared client parity inputs. Paths resolve here, independently of each test file.
//! Desktop will consume this helper and the JSON files from a pinned vendor snapshot.
#![allow(dead_code)]

pub const QUERY: &str = include_str!("client-parity/mock-query-cases.json");
pub const STATE: &str = include_str!("client-parity/mock-state-cases.json");
pub const FILE: &str = include_str!("client-parity/mock-file-cases.json");
pub const SIGNAL: &str = include_str!("client-parity/mock-signal-cases.json");
pub const TRUSTED: &str = include_str!("client-parity/trusted-payloads.json");

/// Runtime reads / explicit corpus refreshes from a core workspace crate.
pub fn core_crate_path(manifest_dir: &str, name: &str) -> std::path::PathBuf {
    std::path::Path::new(manifest_dir)
        .join("../../test-data/client-parity")
        .join(name)
}
