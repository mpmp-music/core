//! The action journal (docs/design/queries-playlists-undo.md): every change to user data (ratings,
//! loved flags, playlists, library roots) is recorded as an action that can be undone on
//! its own, by id. Undoing is itself an action, so undoing an undo is a redo.
//!
//! # Storage (introduced in schema v6)
//!
//! `actions` holds one row per action with its inverse (`inverse`, a compact encoding of
//! [`Inverse`]; `NULL` once the action can no longer be undone). `action_targets` holds one
//! row per object the action changed, keyed by the object's version after the action:
//!
//! | target kind | object | version |
//! |---|---|---|
//! | `track` | a track's rating and loved flag | `track_user.user_version` |
//! | `playlist` | a playlist's name, query and items | `playlists.revision` |
//! | `playlist_trash` | whether a playlist is in the trash | `playlists.trash_version` |
//! | `root` | whether a library root is active | `roots.version` |
//! | `track_tags` | the tags in a track's file, as tag edits wrote them | the newest `version_after` of its `track_tags` rows (0 without any) |
//!
//! Every recorded change bumps the version of each object it touches by one. A track's
//! `track_tags` version has no column of its own (schema v7 has none): it is kept by the
//! target rows, which go only with the actions that wrote them, oldest first, so a version
//! counts again from 0 only once no action names the track's tags any more. What a tag edit
//! cannot see in the database (the file changed since it wrote it) is checked by the tag
//! job itself (`crate::tags::job`), by the size and modification time each file had then.
//!
//! # Conflicts
//!
//! Versions only grow, so an undo cannot compare them directly: after an edit and its undo
//! the object is back in the state before the edit, at a new version. Each target row
//! therefore also records a *state*: the version an original action produced, or for an
//! undo the state before the action it undid. The state of an object at version `v` is the
//! `state_after` of the target row that produced `v`, or `v` itself if no recorded action
//! did. An action can be undone when every target is (in this sense) still in the state the
//! action left it in; otherwise the undo fails with [`LibraryError::ActionConflict`] listing
//! the objects that changed.
//!
//! # Undo order
//!
//! An original action and a redo *apply* a change; an undo *reverts* one. The playlist undo
//! ([`undo_playlist`]) walks back through the applying actions, newest first, like a stack:
//! an undo makes the next older edit current again.

use std::collections::HashSet;

use std::fmt;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use serde::{Deserialize, Serialize};

use crate::playlist::{self, PlaylistId, PlaylistOp};

use crate::types::{Page, Paged, RootId, TrackId};

use crate::{Invalid, LibraryError, NotFound, Result, db::writer, ratings};

/// How long an action stays in the journal (and undoable): 30 days.
pub const ACTION_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Most actions the journal keeps; older ones are dropped first. Audit entries do not
/// count.
pub const ACTION_LIMIT: usize = 10_000;

/// An audit entry (of what cannot be undone: [`AUDIT_KINDS`]) is dropped only once it is
/// both older than this (one year) and not among the newest [`AUDIT_LIMIT`] audit entries.
pub const AUDIT_RETENTION_MS: i64 = 365 * 24 * 60 * 60 * 1000;

/// Audit entries always kept, however old (see [`AUDIT_RETENTION_MS`]).
pub const AUDIT_LIMIT: usize = 10_000;

/// The kinds of audit entries: `purge` (playlists permanently deleted), `file_write` (a file
/// written for an agent, or replaced), and non-UI root access requests.
pub const AUDIT_KINDS: &[&str] = &[
    "purge",
    "file_write",
    "root_release",
    "root_resume",
    "cover_recovery_resolve",
];

/// [`AUDIT_KINDS`] as an SQL list.
const AUDIT_KINDS_SQL: &str =
    "('purge', 'file_write', 'root_release', 'root_resume', 'cover_recovery_resolve')";

/// Id of a recorded action; never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActionId(pub i64);

impl fmt::Display for ActionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Who asks for a change, recorded with its action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActionMeta<'a> {
    /// The origin label (`"ui"`, `"agent:<name>"`, `"remote:<device>"`,...), opaque to the
    /// library; [`crate::Library::actions`] filters on it.
    pub origin: &'a str,
    /// The user's permission grant that let the change run without asking (agent
    /// permissions), opaque to the library: listed with the action
    /// ([`ActionSummary::grant`]).
    pub grant: Option<i64>,
    /// The caller checked the locations the call names, in canonical form, for someone who
    /// may act only where it checked (core sets it for agents). Then the library checks
    /// once more, as late as it can, that they still are canonical and lead where they did
    /// (else [`crate::Invalid::LocationChanged`]: a symlink put in meanwhile), and an
    /// imported file's lines come back in [`crate::M3uReport::missing`] only when they look
    /// like references to media (a path or URL with an audio or cue sheet extension).
    pub checked: bool,
}

impl<'a> ActionMeta<'a> {
    pub const fn new(origin: &'a str) -> Self {
        Self {
            origin,
            grant: None,
            checked: false,
        }
    }

    /// The same, allowed by `grant`.
    pub const fn with_grant(self, grant: Option<i64>) -> Self {
        Self { grant, ..self }
    }

    /// The same, with its locations checked ([`ActionMeta::checked`]).
    pub const fn with_checked(self, checked: bool) -> Self {
        Self { checked, ..self }
    }

    /// An owned copy, to move into a writer closure.
    pub(crate) fn owned(&self) -> OwnedMeta {
        OwnedMeta {
            origin: self.origin.to_owned(),
            grant: self.grant,
            checked: self.checked,
        }
    }
}

/// An [`ActionMeta`] that owns its origin.
pub(crate) struct OwnedMeta {
    origin: String,
    grant: Option<i64>,
    checked: bool,
}

impl OwnedMeta {
    pub(crate) fn meta(&self) -> ActionMeta<'_> {
        ActionMeta::new(&self.origin)
            .with_grant(self.grant)
            .with_checked(self.checked)
    }
}

/// The result of a change, and the action it recorded: `None` when nothing changed (an
/// empty or repeated edit), which is then also not in the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done<T> {
    pub value: T,
    pub action: Option<ActionId>,
}

impl<T> Done<T> {
    /// Whether the call changed anything (recorded an action).
    pub fn changed(&self) -> bool {
        self.action.is_some()
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Done<U> {
        Done {
            value: f(self.value),
            action: self.action,
        }
    }

    pub(crate) fn unchanged(value: T) -> Self {
        Self {
            value,
            action: None,
        }
    }
}

/// An object an action changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionTarget {
    /// A track's rating or loved flag.
    Track { id: TrackId },
    /// A playlist (its contents, or whether it is in the trash).
    Playlist { id: PlaylistId },
    /// A library root folder.
    Root { id: RootId },
}

/// One file of a tag edit's inverse: the values to write back for each key the edit
/// changed (none: the key was not there), and the file's size and modification time
/// right after the edit wrote it (a file that has changed since is not written back).
/// A conversion instead carries the exact primary ID3 block in `raw_id3`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileTags {
    pub track: i64,
    pub size: u64,
    pub mtime_ms: i64,
    pub values: Vec<(String, Vec<String>)>,
    /// Exact primary ID3 block for a conversion undo (or its redo).
    pub raw_id3: Option<Vec<u8>>,
    pub protected_hash: Option<[u8; 32]>,
    pub exact: Option<Box<crate::tags::edit::snapshot::ByteUndo>>,
}

/// A read-only description of the effective inverse, from one journal snapshot.
/// `fingerprint` binds its encoded values and target versions, including partial undos.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoDescription {
    pub action: ActionId,
    pub chain: Vec<ActionId>,
    pub original: ActionId,
    pub effective_kind: String,
    pub writes_files: bool,
    pub targets: Vec<ActionTarget>,
    pub fingerprint: String,
}

/// An action as listed by [`crate::Library::actions`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionSummary {
    pub id: ActionId,
    /// Unix milliseconds.
    pub at_ms: i64,
    pub origin: String,
    /// `set_rating`, `set_loved`, `playlist_create`, `playlist_import`, `playlist_rename`,
    /// `playlist_add`, `playlist_remove`, `playlist_move`, `playlist_set_query`,
    /// `playlist_delete`, `playlist_restore`, `root_add`, `root_remove`; `undo` for an
    /// undo, `redo` for the undo of an undo; `tag_edit` (tags written into files: its undo is
    /// a tag job, [`crate::Library::undo_tag_edit`]); audit entries of what cannot be undone:
    /// `purge` (playlists permanently deleted), `file_write` (a file replaced). Open: more
    /// kinds may come.
    pub kind: String,
    /// One English sentence, e.g. `rate 120 tracks 5 stars`.
    pub summary: String,
    /// Objects the action changed.
    pub target_count: u64,
    /// The action that undid this one.
    pub undone_by: Option<ActionId>,
    /// For `undo` / `redo`: the action it undid.
    pub undo_of: Option<ActionId>,
    /// It can be undone: not undone yet, within the retention and its playlist's last
    /// [`crate::PLAYLIST_UNDO_STEPS`] steps. An undo can still fail on a conflict. Never
    /// for an audit entry.
    pub undoable: bool,
    /// The permission grant that let it run without asking ([`ActionMeta::grant`]).
    pub grant: Option<i64>,
    /// Optional operation-time facts, version 1 JSON; no tag values or inverse data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// What an undo changed, for change events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoEffect {
    /// The action that was undone.
    pub undone: ActionId,
    /// Tracks whose rating or loved flag changed.
    pub tracks: Vec<TrackId>,
    /// Playlists that changed (contents, or moved into or out of the trash).
    pub playlists: Vec<PlaylistId>,
    /// Roots that were removed or restored. A restored root's tracks become available
    /// again with the next scan.
    pub roots: Vec<RootId>,
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::codec::decode_ints;
    use super::*;

    #[test]
    fn int_columns_round_trip() {
        let positions = [0, 1, 2, 50_000, 99_999];
        let items = [7, 3, i64::MAX, i64::MIN, -1];
        let tracks = [100, 100, 5, 0, 1 << 40];
        let data = encode_ints(&[&positions, &items, &tracks]);
        let back = decode_ints(&data, 3).unwrap();
        assert_eq!(
            back,
            vec![positions.to_vec(), items.to_vec(), tracks.to_vec()]
        );
        assert_eq!(
            decode_ints(&encode_ints(&[&[]]), 1).unwrap(),
            vec![Vec::<i64>::new()]
        );
        // Sorted ids are small.
        let ids: Vec<i64> = (1_000_000..1_100_000).collect();
        assert!(encode_ints(&[&ids]).len() < 100_010);
        // Malformed data is rejected, not misread.
        assert!(decode_ints(&data[..data.len() - 1], 3).is_none());
        assert!(decode_ints(&[0xff; 3], 1).is_none());
        assert!(decode_ints(&data, 2).is_none());
    }

    #[test]
    fn inverses_round_trip() {
        let p = PlaylistId(7);
        let all = [
            Inverse::Ratings(vec![(1, Some(5)), (2, None), (9, Some(1))]),
            Inverse::Ratings(vec![]),
            Inverse::Loved(vec![(3, true), (4, false)]),
            Inverse::Playlist(p, PlaylistOp::Name("名前 \"x\"".into())),
            Inverse::Playlist(p, PlaylistOp::Name(String::new())),
            Inverse::Playlist(
                p,
                PlaylistOp::Query {
                    json: "{\"v\":1}".into(),
                    limit: Some(25),
                },
            ),
            Inverse::Playlist(
                p,
                PlaylistOp::Query {
                    json: String::new(),
                    limit: None,
                },
            ),
            Inverse::Playlist(p, PlaylistOp::Remove(vec![5, 6, 100])),
            Inverse::Playlist(p, PlaylistOp::Restore(vec![(0, 5, 1), (3, 9, 1)])),
            Inverse::Playlist(p, PlaylistOp::Move { item: 4, from: 0 }),
            Inverse::Playlist(PlaylistId(i64::MAX), PlaylistOp::Trash),
            Inverse::Playlist(p, PlaylistOp::Untrash),
            Inverse::Root(RootId(3), RootOp::Remove),
            Inverse::Root(RootId(3), RootOp::Restore),
            Inverse::Tags(vec![]),
            Inverse::Tags(vec![
                FileTags {
                    track: 1,
                    size: 10,
                    mtime_ms: 1,
                    values: vec![],
                    raw_id3: Some(vec![1, 2]),
                    protected_hash: Some([0xa5; 32]),
                    exact: None,
                },
                FileTags {
                    track: 2,
                    size: 10,
                    mtime_ms: 1,
                    values: vec![],
                    raw_id3: None,
                    protected_hash: None,
                    exact: None,
                },
            ]),
            Inverse::Tags(vec![
                FileTags {
                    track: 1,
                    size: 10,
                    mtime_ms: -1,
                    values: vec![],
                    raw_id3: Some(vec![0, 255, 128, 0]),
                    protected_hash: None,
                    exact: None,
                },
                FileTags {
                    track: 2,
                    size: 10,
                    mtime_ms: 1,
                    values: vec![],
                    raw_id3: None,
                    protected_hash: None,
                    exact: None,
                },
                FileTags {
                    track: 3,
                    size: 10,
                    mtime_ms: 1,
                    values: vec![],
                    raw_id3: Some(vec![]),
                    protected_hash: None,
                    exact: None,
                },
            ]),
            Inverse::Tags(vec![
                FileTags {
                    raw_id3: None,
                    protected_hash: None,
                    exact: None,
                    track: 9,
                    size: 1 << 40,
                    mtime_ms: -5,
                    values: vec![
                        ("ARTIST".into(), vec!["A".into(), "周砚舟".into()]),
                        ("COMMENT".into(), vec![]),
                        (String::new(), vec![String::new()]),
                    ],
                },
                FileTags {
                    raw_id3: None,
                    protected_hash: None,
                    exact: None,
                    track: 3,
                    size: 0,
                    mtime_ms: 1_700_000_000_000,
                    values: vec![],
                },
            ]),
        ];
        for inverse in all {
            let data = inverse.encode();
            assert_eq!(Inverse::decode(&data), Some(inverse.clone()), "{data:?}");
            // Truncated data never decodes to something else silently, except where the
            // tail is free text.
            if let Some(cut) = Inverse::decode(&data[..data.len() - 1]) {
                assert!(
                    matches!(
                        cut,
                        Inverse::Playlist(_, PlaylistOp::Name(_) | PlaylistOp::Query { .. })
                    ),
                    "{cut:?}"
                );
            }
        }
        assert_eq!(Inverse::decode(&[]), None);
        assert_eq!(Inverse::decode(&[99]), None);
        // Ratings outside 0-5 and loved flags other than 0/1 are corrupt.
        assert_eq!(
            Inverse::decode(&[&[TAG_RATINGS][..], &encode_ints(&[&[1], &[6]])].concat()),
            None
        );
        assert_eq!(
            Inverse::decode(&[&[TAG_LOVED][..], &encode_ints(&[&[1], &[2]])].concat()),
            None
        );
    }
}

mod target;
pub(crate) use self::target::Kind;
pub(crate) use self::target::Target;
pub(crate) use self::target::bump;
pub(crate) use self::target::state_of;
use self::target::version_of;

mod inverse;
pub(crate) use self::inverse::Inverse;
pub(crate) use self::inverse::RootOp;

mod codec;
#[cfg(all(test, feature = "desktop-fs"))]
use self::codec::TAG_LOVED;
#[cfg(all(test, feature = "desktop-fs"))]
use self::codec::TAG_RATINGS;

#[cfg(all(test, feature = "desktop-fs"))]
pub(crate) use self::codec::encode_ints;

mod record;
pub(crate) use self::record::NewAction;
pub(crate) use self::record::forget_playlist;
use self::record::insert;
pub(crate) use self::record::prune;
pub(crate) use self::record::record;
pub(crate) use self::record::record_audit;
pub(crate) use self::record::roots_of;

mod undo;
pub(crate) use self::undo::check_undo_description;
pub(crate) use self::undo::describe_undo;
pub(crate) use self::undo::undo_action;
pub(crate) use self::undo::undo_guarded;
pub(crate) use self::undo::undo_playlist;
use self::undo::undo_summary;
use self::undo::undoable;

mod tags;
pub(crate) use self::tags::record_tag_edit;
pub(crate) use self::tags::record_tag_undo;
pub(crate) use self::tags::tag_tracks;
pub(crate) use self::tags::tag_undo;

mod read;
pub(crate) use self::read::count;
pub(crate) use self::read::list;
pub(crate) use self::read::summary;

mod covers;
pub(crate) use self::covers::cover_restamp;
pub(crate) use self::covers::cover_undo;
pub(crate) use self::covers::record_cover;
