//! Rust-only scan facts. Public reports keep their bounded diagnostic sample.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Exact failed paths with compact cause/message hashes, and the actual covered scope.
/// Not serialized into the library or core contracts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanDiagnostics {
    pub errors: BTreeMap<PathBuf, [u8; 32]>,
    pub coverage: BTreeSet<PathBuf>,
}

impl ScanDiagnostics {
    /// Compare the same scope, including explicitly visited CUE dependency files.
    pub fn same_errors_in(&self, current: &Self) -> bool {
        self.errors
            .iter()
            .filter(|(path, _)| current.covers(path))
            .eq(current.errors.iter())
    }

    pub fn covers(&self, path: &Path) -> bool {
        self.coverage.iter().any(|scope| path.starts_with(scope))
    }

    /// Replace facts only where this scan actually visited; retain other roots and
    /// failures beyond the public sample across directory repairs.
    pub fn update(&mut self, current: &Self) {
        self.errors.retain(|path, _| !current.covers(path));
        self.errors.extend(current.errors.clone());
        for scope in &current.coverage {
            if !self.covers(scope) {
                self.coverage.retain(|old| !old.starts_with(scope));
                self.coverage.insert(scope.clone());
            }
        }
    }

    /// Conservative retention charge for owned tree nodes, paths and digests.
    pub fn retained_bytes(&self) -> usize {
        self.errors
            .keys()
            .map(|p| 192 + p.as_os_str().len() * 4)
            .sum::<usize>()
            + self
                .coverage
                .iter()
                .map(|p| 128 + p.as_os_str().len() * 4)
                .sum::<usize>()
    }
}
