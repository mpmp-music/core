use super::*;

// ---- targets and versions ---

/// What a target row versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Track,
    Playlist,
    Trash,
    Root,
    /// The tags in a track's file (tag edits).
    Tags,
    Cover,
}

impl Kind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Kind::Track => "track",
            Kind::Playlist => "playlist",
            Kind::Trash => "playlist_trash",
            Kind::Root => "root",
            Kind::Tags => "track_tags",
            Kind::Cover => "track_cover",
        }
    }

    pub(super) fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "track" => Kind::Track,
            "playlist" => Kind::Playlist,
            "playlist_trash" => Kind::Trash,
            "root" => Kind::Root,
            "track_tags" => Kind::Tags,
            "track_cover" => Kind::Cover,
            _ => return None,
        })
    }

    pub(super) fn object(self, id: i64) -> ActionTarget {
        match self {
            Kind::Track | Kind::Tags | Kind::Cover => ActionTarget::Track { id: TrackId(id) },
            Kind::Playlist | Kind::Trash => ActionTarget::Playlist { id: PlaylistId(id) },
            Kind::Root => ActionTarget::Root { id: RootId(id) },
        }
    }
}

/// One object an action changed: its version after the action, and its state before and
/// after (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Target {
    pub kind: Kind,
    pub id: i64,
    pub version_after: i64,
    pub state_before: i64,
    pub state_after: i64,
}

/// The state of an object at `version`.
pub(crate) fn state_of(conn: &Connection, kind: Kind, id: i64, version: i64) -> Result<i64> {
    Ok(conn
        .prepare_cached(
            "SELECT state_after FROM action_targets
             WHERE kind = ?1 AND object_id = ?2 AND version_after = ?3",
        )?
        .query_row(params![kind.as_str(), id, version], |row| row.get(0))
        .optional()?
        .unwrap_or(version))
}

/// The current version of an object; `None` if it does not exist (any more).
pub(super) fn version_of(conn: &Connection, kind: Kind, id: i64) -> Result<Option<i64>> {
    let sql = match kind {
        Kind::Track => {
            "SELECT COALESCE((SELECT user_version FROM track_user WHERE track_id = ?1), 0)
             FROM tracks WHERE id = ?1"
        }
        Kind::Playlist => "SELECT revision FROM playlists WHERE id = ?1",
        Kind::Trash => "SELECT trash_version FROM playlists WHERE id = ?1",
        Kind::Root => "SELECT version FROM roots WHERE id = ?1",
        Kind::Cover => {
            "SELECT COALESCE((SELECT MAX(version_after) FROM action_targets
                              WHERE kind = 'track_cover' AND object_id = ?1), 0)
             FROM tracks WHERE id = ?1"
        }
        Kind::Tags => {
            "SELECT COALESCE((SELECT MAX(version_after) FROM action_targets
                              WHERE kind = 'track_tags' AND object_id = ?1), 0)
             FROM tracks WHERE id = ?1"
        }
    };
    Ok(conn
        .prepare_cached(sql)?
        .query_row([id], |row| row.get(0))
        .optional()?)
}

/// Bumps the version of an object that the caller is changing (a playlist revision also
/// stamps `updated_at`): the target to record. `state_after` is the new version; an undo
/// replaces it.
pub(crate) fn bump(conn: &Connection, kind: Kind, id: i64, now: i64) -> Result<Target> {
    let version = version_of(conn, kind, id)?
        .ok_or_else(|| LibraryError::Corrupt(format!("{} {id} vanished", kind.as_str())))?;
    let state_before = state_of(conn, kind, id, version)?;
    let after = version + 1;
    match kind {
        Kind::Track => conn
            .prepare_cached(
                "INSERT INTO track_user (track_id, user_version) VALUES (?1, ?2)
                 ON CONFLICT (track_id) DO UPDATE SET user_version = excluded.user_version",
            )?
            .execute(params![id, after])?,
        Kind::Playlist => conn
            .prepare_cached("UPDATE playlists SET revision = ?2, updated_at = ?3 WHERE id = ?1")?
            .execute(params![id, after, now])?,
        Kind::Trash => conn
            .prepare_cached("UPDATE playlists SET trash_version = ?2 WHERE id = ?1")?
            .execute(params![id, after])?,
        Kind::Root => conn
            .prepare_cached("UPDATE roots SET version = ?2 WHERE id = ?1")?
            .execute(params![id, after])?,
        // Kept by the target row the action records.
        Kind::Tags | Kind::Cover => 0,
    };
    Ok(Target {
        kind,
        id,
        version_after: after,
        state_before,
        state_after: after,
    })
}
