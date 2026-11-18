//! Playlists (docs/design/queries-playlists-undo.md): manual lists of tracks and saved smart queries,
//! with revisions, undo through the action journal (crate::journal), and a trash that
//! keeps deleted playlists until the user confirms permanent deletion.
//!
//! Write functions run on the writer thread (one transaction each); read functions on a
//! pooled read-only connection.

pub(crate) mod m3u;

use std::collections::{HashMap, HashSet};

use std::fmt;

use std::sync::atomic::{AtomicU64, Ordering};

use std::sync::{Arc, Mutex, OnceLock};

use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use serde::{Deserialize, Serialize};

use crate::db::writer::now_ms;

use crate::journal::{self, ActionId, ActionMeta, Done, Inverse, Kind, NewAction};

use crate::text::normalize;

use crate::track_query::{self, CompareOp, Field, Filter, TrackQuery, TrackRow, Value};

use crate::types::{Page, TrackId};

use crate::{Invalid, LibraryError, NotFound, Result, db::reads};

/// Age threshold for the trusted window's manual cleanup selection: 30 days.
/// This is never a deletion deadline; older playlists remain restorable.
pub const PLAYLIST_TRASH_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// How many edits of one playlist can be undone, newest first: older actions on the
/// playlist's contents stop being undoable.
pub const PLAYLIST_UNDO_STEPS: usize = 50;

/// How long an edit stays undoable: 30 days (the action journal's retention,
/// [`crate::ACTION_RETENTION_MS`]).
pub const PLAYLIST_UNDO_RETENTION_MS: i64 = journal::ACTION_RETENTION_MS;

/// Version of the stored smart query format: `playlists.query_json` (and the journal's
/// inverses that restore one) hold `{"v": 1, "query": <TrackQuery>}` since schema v5.
pub(crate) const QUERY_FORMAT: u32 = 1;

/// Stable playlist id; ids of purged playlists are never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlaylistId(pub i64);

/// Stable id of one entry of a manual playlist: the same track added twice is two items.
/// Never reused, also across playlists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlaylistItemId(pub i64);

impl fmt::Display for PlaylistId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for PlaylistItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlaylistKind {
    /// Tracks in a user-defined order; duplicates allowed.
    Manual,
    /// The first `max_tracks` tracks (all without a limit) matching `query`, recomputed
    /// whenever the playlist is read or played.
    Smart {
        query: TrackQuery,
        #[serde(default)]
        max_tracks: Option<u32>,
    },
}

/// A playlist as listed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistSummary {
    pub id: PlaylistId,
    pub name: String,
    /// For a smart playlist whose stored query no longer parses, `Smart` with an empty query
    /// (all tracks) and [`PlaylistSummary::query_error`] set.
    pub kind: PlaylistKind,
    /// Manual: all items, including unavailable tracks. Smart: tracks the query gives now
    /// (respecting the limit); 0 if the query is invalid.
    pub track_count: u64,
    /// Total duration of the same tracks (unknown durations count as 0).
    pub duration_ms: u64,
    /// Bumped by every edit (not by delete or restore).
    pub revision: u64,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds of the last edit.
    pub updated_at: i64,
    /// Unix milliseconds when it was moved to the trash; `None` if not in the trash.
    pub deleted_at: Option<i64>,
    /// Smart playlists whose stored query no longer parses or type-checks (e.g. after a
    /// field was removed): the reason. They list no tracks (and cannot be played or
    /// exported) until the query is replaced with [`crate::Library::set_smart_query`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_error: Option<String>,
}

/// A trusted confirmation's immutable trash selection, rechecked in the delete transaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PurgeGuard {
    pub all: bool,
    pub playlists: Vec<PlaylistSummary>,
    /// For manual expiry cleanup, also freeze the entire trash. Private Rust/host
    /// binding only; this is not part of the versioned message contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trash: Option<Vec<PlaylistSummary>>,
    /// Process-local review clock; never part of a versioned message or persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry_clock: Option<PurgeReviewClock>,
}

/// Wall time bracketed by process-local monotonic samples. The bracket accounts for
/// sampling/scheduling delay; comparisons also allow precision error and gradual
/// wall-clock correction relative to the monotonic clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PurgeReviewClock {
    wall_ms: i64,
    before_ms: u64,
    after_ms: u64,
}

// A platform-neutral precision budget plus 1000 ppm (0.1%) covers the review's
// 500–1000 ppm slew examples without equating ordinary correction with a step.
// These are engineering allowances, not claims about an OS clock's maximum error.
const PURGE_CLOCK_BASE_TOLERANCE_MS: i128 = 20;

const PURGE_CLOCK_MAX_SLEW_PPM: i128 = 1_000;

impl PurgeReviewClock {
    fn now() -> Self {
        static START: OnceLock<Instant> = OnceLock::new();
        let start = START.get_or_init(Instant::now);
        let before_ms = start.elapsed().as_millis() as u64;
        let wall_ms = now_ms();
        let after_ms = start.elapsed().as_millis() as u64;
        Self {
            wall_ms,
            before_ms,
            after_ms,
        }
    }

    fn follows(&self, reviewed: &Self) -> bool {
        let wall_elapsed = i128::from(self.wall_ms) - i128::from(reviewed.wall_ms);
        // Use the widest monotonic elapsed interval; round its slew allowance up to
        // a whole millisecond. i128 keeps multiplication safe for every u64 sample.
        let elapsed = (i128::from(self.after_ms) - i128::from(reviewed.before_ms)).max(0);
        let slew_allowance = (elapsed * PURGE_CLOCK_MAX_SLEW_PPM + 999_999) / 1_000_000;
        let tolerance = PURGE_CLOCK_BASE_TOLERANCE_MS + slew_allowance;
        let earliest = i128::from(self.before_ms) - i128::from(reviewed.after_ms) - tolerance;
        let latest = i128::from(self.after_ms) - i128::from(reviewed.before_ms) + tolerance;
        self.before_ms <= self.after_ms
            && reviewed.before_ms <= reviewed.after_ms
            && self.before_ms >= reviewed.before_ms
            && (earliest..=latest).contains(&wall_elapsed)
    }

    #[cfg(all(test, feature = "desktop-fs"))]
    pub(crate) fn at(wall_ms: i64, monotonic_ms: u64) -> Self {
        Self {
            wall_ms,
            before_ms: monotonic_ms,
            after_ms: monotonic_ms,
        }
    }
}

impl PurgeGuard {
    /// Selects only playlists older than 30 days from an all-trash description.
    /// Retains the complete snapshot so any trash change invalidates the review.
    pub fn expired_selection(self) -> Self {
        self.expired_selection_at(PurgeReviewClock::now())
    }

    pub(crate) fn expired_selection_at(mut self, clock: PurgeReviewClock) -> Self {
        let cutoff = clock.wall_ms.saturating_sub(PLAYLIST_TRASH_RETENTION_MS);
        self.trash = Some(self.playlists.clone());
        self.playlists
            .retain(|p| p.deleted_at.is_some_and(|at| at < cutoff));
        self.all = false;
        self.expiry_clock = Some(clock);
        self
    }

    /// Expiry cleanup needs a fresh complete trash description. Recompute eligibility
    /// and reject either a changed set/content or a wall-clock discontinuity. Ordinary
    /// selected/all purges keep their existing comparison semantics.
    pub fn matches_current(&self, current: Self) -> bool {
        self.matches_current_at(current, PurgeReviewClock::now())
    }

    pub(crate) fn matches_current_at(&self, current: Self, clock: PurgeReviewClock) -> bool {
        if let Some(reviewed) = &self.expiry_clock {
            if !current.all || !clock.follows(reviewed) {
                return false;
            }
            let mut current = current.expired_selection_at(clock);
            current.expiry_clock = self.expiry_clock.clone();
            current == *self
        } else {
            // Older expiry bindings without a clock fail closed.
            self.trash.is_none() && current == *self
        }
    }
}

/// One entry of a playlist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistEntry {
    /// The item id of a manual playlist entry; `None` for smart playlists.
    pub item: Option<PlaylistItemId>,
    pub row: TrackRow,
}

/// Most entries [`M3uReport::missing`] lists.
pub const M3U_MISSING_LIMIT: usize = 200;

/// Result of an M3U import.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct M3uReport {
    /// Entries that matched a library track (and were added).
    pub matched: u64,
    /// The first [`M3U_MISSING_LIMIT`] entries (as written in the file) that matched no
    /// library track, in file order.
    pub missing: Vec<String>,
    /// How many entries matched no library track, listed in `missing` or not.
    #[serde(default)]
    pub missing_total: u64,
}

fn playlist_not_found(id: PlaylistId) -> LibraryError {
    LibraryError::not_found(NotFound::Playlist, format!("playlist {id}"))
}

fn item_not_found(id: PlaylistId, item: i64) -> LibraryError {
    LibraryError::not_found(NotFound::Item, format!("item {item} in playlist {id}"))
}

/// The JSON stored for a smart query: `{"v": 1, "query":...}`.
pub(crate) fn encode_query(query: &TrackQuery) -> Result<String> {
    #[derive(Serialize)]
    struct Stored<'a> {
        v: u32,
        query: &'a TrackQuery,
    }
    serde_json::to_string(&Stored {
        v: QUERY_FORMAT,
        query,
    })
    .map_err(|err| LibraryError::InvalidQuery(err.to_string()))
}

/// Reads [`encode_query`]'s JSON, or why it cannot.
pub(crate) fn decode_query(json: &str) -> std::result::Result<TrackQuery, String> {
    #[derive(Deserialize)]
    struct Stored {
        v: u32,
        query: serde_json::Value,
    }
    let stored: Stored = serde_json::from_str(json)
        .map_err(|err| format!("stored smart query no longer parses: {err}"))?;
    if stored.v != QUERY_FORMAT {
        return Err(format!(
            "stored smart query has format version {}; this build reads version {QUERY_FORMAT}",
            stored.v
        ));
    }
    TrackQuery::deserialize(stored.query)
        .map_err(|err| format!("stored smart query no longer parses: {err}"))
}

// ---- stored rows ---

const COLUMNS: &str =
    "id, name, kind, query_json, item_limit, revision, created_at, updated_at, deleted_at";

struct Stored {
    id: PlaylistId,
    name: String,
    smart: bool,
    query_json: Option<String>,
    limit: Option<u32>,
    revision: u64,
    created_at: i64,
    updated_at: i64,
    deleted_at: Option<i64>,
}

fn stored_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Stored> {
    Ok(Stored {
        id: PlaylistId(row.get(0)?),
        name: row.get(1)?,
        smart: row.get::<_, String>(2)? == "smart",
        query_json: row.get(3)?,
        limit: row.get(4)?,
        revision: row.get::<_, i64>(5)?.max(0) as u64,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        deleted_at: row.get(8)?,
    })
}

fn stored(conn: &Connection, id: PlaylistId) -> Result<Option<Stored>> {
    Ok(conn
        .prepare_cached(&format!("SELECT {COLUMNS} FROM playlists WHERE id = ?1"))?
        .query_row([id.0], stored_from_row)
        .optional()?)
}

impl Stored {
    /// The smart query, or why it is unusable.
    fn query(&self) -> std::result::Result<TrackQuery, String> {
        let query = decode_query(self.query_json.as_deref().unwrap_or(""))?;
        match track_query::validate(&query) {
            Ok(()) => Ok(query),
            Err(LibraryError::InvalidQuery(message)) => Err(message),
            Err(err) => Err(err.to_string()),
        }
    }

    fn query_or_error(&self) -> Result<TrackQuery> {
        self.query().map_err(LibraryError::InvalidQuery)
    }
}

fn in_trash(id: PlaylistId) -> LibraryError {
    LibraryError::invalid(
        Invalid::PlaylistInTrash,
        format!("playlist {id} is in the trash; restore it first"),
    )
}

/// A playlist that may be edited: exists and is not in the trash.
fn live(conn: &Connection, id: PlaylistId) -> Result<Stored> {
    match stored(conn, id)? {
        None => Err(playlist_not_found(id)),
        Some(p) if p.deleted_at.is_some() => Err(in_trash(id)),
        Some(p) => Ok(p),
    }
}

fn check_revision(p: &Stored, expected: Option<u64>) -> Result<()> {
    match expected {
        Some(expected) if expected != p.revision => Err(LibraryError::PlaylistConflict {
            expected,
            actual: p.revision,
        }),
        _ => Ok(()),
    }
}

fn require_manual(p: &Stored) -> Result<()> {
    if p.smart {
        return Err(LibraryError::invalid(
            Invalid::PlaylistKindMismatch,
            format!(
                "playlist {} is a smart playlist; its tracks come from its query",
                p.id
            ),
        ));
    }
    Ok(())
}

fn check_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        return Err(LibraryError::invalid(
            Invalid::BlankName,
            "playlist name is empty",
        ));
    }
    Ok(name)
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::*;

    #[test]
    fn smart_page_respects_limit() {
        assert_eq!(limited(Page::new(0, 50), Some(20)), Page::new(0, 20));
        assert_eq!(limited(Page::new(10, 50), Some(20)), Page::new(10, 10));
        assert_eq!(limited(Page::new(30, 50), Some(20)), Page::new(30, 0));
        assert_eq!(limited(Page::new(30, 50), None), Page::new(30, 50));
    }
}

mod purge;
pub(crate) use self::purge::purge;
pub(crate) use self::purge::purge_guarded;
#[cfg(all(test, feature = "desktop-fs"))]
pub(crate) use self::purge::purge_guarded_with_clock;
pub(crate) use self::purge::purge_selection;

mod browse;
pub(crate) use self::browse::Counts;
pub(crate) use self::browse::ExportTrack;
pub(crate) use self::browse::SmartCounts;
pub(crate) use self::browse::export_tracks;
pub(crate) use self::browse::items;
pub(crate) use self::browse::playback;
pub(crate) use self::browse::summaries;
pub(crate) use self::browse::summary;
pub(crate) use self::browse::summary_and_items;
pub(crate) use self::browse::track_ids;

use self::browse::summarize;

#[cfg(all(test, feature = "desktop-fs"))]
use self::browse::limited;

mod commands;
pub(crate) use self::commands::{
    PlaylistOp, add, apply_op, create, delete, editable_revision, move_item, remove, rename,
    restore, set_query,
};

use self::commands::item_count;
