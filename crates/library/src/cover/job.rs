//! Previews and runs embedded cover edits, undo/redo, and interrupted-write recovery.
//!
//! Each job verifies file identity and content guards before publishing a durable
//! replacement. Registered candidates and backups remain available until recovery can
//! account for them; database records track the result without hiding retained copies.
//! See docs/design/media-resources.md and docs/design/storage.md.

use crate::cover::edit::{
    self as cover_edit, CoverChange, CoverImageInfo, MAX_COVER_EDIT_TRACKS, MAX_COVER_UNDO_BYTES,
    Region,
};

use crate::db::writer::now_ms;

use crate::handle::Inner;

use crate::journal::{self, ActionId, ActionMeta, OwnedMeta};

use crate::tags::job::{self as tag_job, Located, TagJobOptions};

use crate::{Library, LibraryError, Result, TrackId, storage_access};

use rusqlite::{Connection, params};

use serde::{Deserialize, Serialize};

use std::collections::{HashMap, HashSet};

use std::path::{Path, PathBuf};

use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverFilePreview {
    pub track: TrackId,
    pub before: Option<CoverImageInfo>,
    pub after: Option<CoverImageInfo>,
    pub changed: bool,
    pub undo_bytes: u64,
    pub fallback: bool,
    pub scope: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverPreview {
    pub files: Vec<CoverFilePreview>,
    pub skipped: Vec<(TrackId, &'static str)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverEditReport {
    pub written: Vec<TrackId>,
    pub unchanged: Vec<TrackId>,
    pub skipped: Vec<(TrackId, &'static str)>,
    pub failed: Vec<(TrackId, &'static str)>,
    pub action: Option<ActionId>,
}

#[derive(Debug)]
pub struct CoverJob {
    thread: std::thread::JoinHandle<Result<CoverEditReport>>,
}

impl CoverJob {
    pub fn wait(self) -> Result<CoverEditReport> {
        self.thread.join().map_err(|_| LibraryError::ScanPanicked)?
    }
}

/// Cover inverse with a whole-file result guard (binary version 9). The whole-file guard deliberately detects overlapping raw
/// ID3 undo and external edits, including edits which retain size and timestamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileCover {
    pub track: i64,
    pub regions: Vec<Region>,
    pub expected: String,
    pub restored_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Target {
    track: i64,
    path: PathBuf,
    group: String,
    operation: String,
    undo_of: Option<ActionId>,
    at: i64,
    #[serde(default)]
    staged: Option<Candidate>,
    #[serde(default)]
    backup: Option<Candidate>,
    #[serde(default)]
    baseline: Option<Baseline>,
    #[serde(default)]
    before_identity: Option<[u64; 2]>,
    #[serde(default)]
    rollback_after: Option<Candidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Candidate {
    path: PathBuf,
    identity: [u64; 2],
    #[serde(default)]
    retained_digest: Option<String>,
    #[serde(default)]
    copied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Baseline {
    #[serde(default)]
    hidden: Option<bool>,
    stamp: (u64, i64),
    picture: Option<[u8; 32]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Authorization {
    origin: String,
    grant: Option<i64>,
    checked: bool,
}

impl Authorization {
    fn meta(&self) -> ActionMeta<'_> {
        ActionMeta::new(&self.origin)
            .with_grant(self.grant)
            .with_checked(self.checked)
    }
}

struct Intent {
    id: i64,
    target: Target,
    auth: Authorization,
    before: String,
    after: String,
    inverse: Vec<Region>,
}

fn json<T: Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| LibraryError::Corrupt(e.to_string()))
}

fn parse<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| LibraryError::Corrupt(e.to_string()))
}

/// Read-only validation for schema relabeling. Reuse the recovery parser without
/// touching candidates, clearing intents, or invoking recovery.
pub(crate) fn validate_reset_intents(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT target_json,authorization_json FROM file_write_intents WHERE family='cover_edit'",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let _: Target = parse(&row.get::<_, String>(0)?)?;
        let _: Authorization = parse(&row.get::<_, String>(1)?)?;
    }
    Ok(())
}

/// An unreadable row must be visible to the trusted diagnostics without
/// interpreting any path from that row.
pub(crate) fn unreadable_recovery_rows(conn: &Connection) -> Result<Vec<i64>> {
    let mut statement = conn.prepare(
        "SELECT id,format_version,stage,target_json,authorization_json
         FROM file_write_intents WHERE family='cover_edit'",
    )?;
    let mut rows = statement.query([])?;
    let mut unreadable = Vec::new();
    while let Some(row) = rows.next()? {
        let valid = row.get::<_, i64>(1).ok() == Some(1)
            && row.get::<_, String>(2).is_ok_and(|stage| {
                matches!(
                    stage.as_str(),
                    "building" | "prepared" | "replaced" | "retained" | "finalized"
                )
            })
            && row
                .get::<_, String>(3)
                .is_ok_and(|json| parse::<Target>(&json).is_ok())
            && row
                .get::<_, String>(4)
                .is_ok_and(|json| parse::<Authorization>(&json).is_ok());
        if !valid {
            unreadable.push(row.get(0)?);
        }
    }
    Ok(unreadable)
}

pub(crate) type ObservedFile = (PathBuf, Option<[u64; 2]>);

/// A scan-wide canonical-path index, split into root and relative keys so recording
/// observations never constructs an absolute path per file. No filesystem I/O.
#[derive(Debug, Default)]
pub(crate) struct ScanObservations {
    roots: std::sync::Mutex<HashMap<PathBuf, HashMap<String, [u64; 2]>>>,
    #[cfg(all(test, feature = "desktop-fs"))]
    insertions: std::sync::atomic::AtomicUsize,
    #[cfg(all(test, feature = "desktop-fs"))]
    lookups: std::sync::atomic::AtomicUsize,
}

#[cfg(all(test, feature = "desktop-fs"))]
thread_local! {
 static PATH_NORMALIZATIONS: std::cell::Cell<usize> = const {
 std::cell::Cell::new(0) }
;
 }

struct RecoveryFlag<'a>(&'a Inner);

impl Drop for RecoveryFlag<'_> {
    fn drop(&mut self) {
        refresh_recovery_flag(self.0);
    }
}

#[cfg(all(test, feature = "desktop-fs"))]
thread_local! {
 static RECOVERY_FAULT: std::cell::Cell<Option<&'static str>> = const {
 std::cell::Cell::new(None) }
;
 }

/// Read-only confirmation binding; snapshots never keep music files open while waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverSource {
    track: TrackId,
    path: PathBuf,
    digest: String,
    identity: mpmp_storage::FileIdentity,
}

/// Explicit UI-only recovery decisions. Candidate cleanup is separately confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverRecoveryResolution {
    AcceptCurrent,
    RestoreBefore,
    RestoreAfter,
    CleanupCandidates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverCandidateRole {
    Backup,
    Staged,
    RollbackAfter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverRecoverySelection {
    pub intent: i64,
    pub role: CoverCandidateRole,
    pub sha256: String,
}

/// A private confirmation binding. It includes current target identity/digest and all
/// matching intent metadata, but never includes inverse BLOBs or image input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverRecoveryFile {
    pub intent: i64,
    pub role: CoverCandidateRole,
    pub location: String,
    pub sha256: Option<String>,
    pub bytes: Option<u64>,
    pub verified: bool,
    pub exists: bool,
    pub will_delete: bool,
    /// Only among the registered candidates and the current target, never a disk-wide claim.
    pub only_copy: bool,
    pub deletes_last_copies: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverRecoveryGuard {
    pub track: TrackId,
    pub summary: String,
    pub inventory: String,
    pub candidates: Vec<CoverRecoverySelection>,
    pub files: Vec<CoverRecoveryFile>,
    pub can_accept_current: bool,
    pub can_restore: bool,
    pub can_cleanup: bool,
    pub current_location: String,
    pub current_sha256: Option<String>,
    selected: Option<CoverRecoverySelection>,
    resolution: CoverRecoveryResolution,
    fingerprint: String,
    path: PathBuf,
    ids: Vec<i64>,
    intents: Vec<String>,
    candidate_identities: Vec<(PathBuf, [u64; 2])>,
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests;

mod observations;
pub(crate) use self::observations::PendingTargets;
use self::observations::identity;
use self::observations::lexical;
pub(crate) use self::observations::pending;
pub(crate) use self::observations::pending_path;
use self::observations::protects_identity;
use self::observations::track_path;

mod recovery;
use self::recovery::backup_safe;
use self::recovery::candidate_matches;
use self::recovery::cleanup;
use self::recovery::digest_at;
use self::recovery::finalize;
pub(crate) use self::recovery::recover;
pub(crate) use self::recovery::recover_inner;
pub(crate) use self::recovery::recover_observed;
pub(crate) use self::recovery::refresh_recovery_flag;
use self::recovery::replacement_paths;

mod guard;
use self::guard::candidates_absent;
#[cfg(all(test, feature = "desktop-fs"))]
use self::guard::candidates_absent_with;
use self::guard::recovery_guard;
use self::guard::recovery_guard_for;

mod api;

mod run;
use self::run::check;
use self::run::probe_staged;
use self::run::run;
