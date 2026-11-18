use super::*;

// ---- recording ---

/// An action to record: what it was, how to undo it and the objects it changed (whose
/// versions the caller bumped with [`bump`]).
pub(crate) struct NewAction {
    pub kind: &'static str,
    pub summary: String,
    pub inverse: Inverse,
    pub targets: Vec<Target>,
}

/// Records an original action (not an undo).
pub(crate) fn record(
    tx: &Connection,
    meta: ActionMeta<'_>,
    action: NewAction,
    now: i64,
) -> Result<ActionId> {
    insert(tx, meta, action, None, false, now)
}

pub(super) fn insert(
    tx: &Connection,
    meta: ActionMeta<'_>,
    action: NewAction,
    undo_of: Option<ActionId>,
    redo: bool,
    now: i64,
) -> Result<ActionId> {
    let details = operation_details(tx, &action, undo_of)?;
    tx.prepare_cached(
        "INSERT INTO actions (at_ms, origin, kind, summary, target_count, undo_of, redo,
             inverse, grant_id, details_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?
    .execute(params![
        now,
        meta.origin,
        action.kind,
        action.summary,
        action.targets.len() as i64,
        undo_of.map(|a| a.0),
        redo,
        action.inverse.encode(),
        meta.grant,
        details.map(|details| serde_json::json!({"v":1,"details":details}).to_string()),
    ])?;
    let id = ActionId(tx.last_insert_rowid());
    if let Some(undone) = undo_of {
        // Before the cap below: the undone action no longer counts.
        tx.prepare_cached("UPDATE actions SET undone_by = ?2 WHERE id = ?1")?
            .execute(params![undone.0, id.0])?;
    }
    {
        let mut target = tx.prepare_cached(
            "INSERT INTO action_targets
                 (kind, object_id, version_after, action_id, state_before, state_after)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for t in &action.targets {
            target.execute(params![
                t.kind.as_str(),
                t.id,
                t.version_after,
                id.0,
                t.state_before,
                t.state_after
            ])?;
        }
    }
    for t in &action.targets {
        if t.kind == Kind::Playlist {
            cap_playlist(tx, t.id)?;
        }
    }
    prune(tx, now)?;
    Ok(id)
}

/// Snapshot facts inside the same transaction as the edit. Never derive historical
/// details at read time from mutable objects, English summaries, or expirable inverses.
fn operation_details(
    tx: &Connection,
    action: &NewAction,
    undo: Option<ActionId>,
) -> Result<Option<serde_json::Value>> {
    use serde_json::json;
    if let Some(id) = undo {
        let kind: Option<String> = tx
            .query_row(
                "WITH RECURSIVE chain(id, kind, undo_of, details_json, depth) AS (
                SELECT id, kind, undo_of, details_json, 0 FROM actions WHERE id=?1
                UNION ALL SELECT a.id, a.kind, a.undo_of, a.details_json, c.depth+1 FROM actions a
                JOIN chain c ON a.id=c.undo_of)
             SELECT kind, details_json FROM chain
             WHERE kind NOT IN ('undo','redo') OR details_json IS NOT NULL ORDER BY depth LIMIT 1",
                [id.0],
                |r| {
                    let kind: String = r.get(0)?;
                    let stored: Option<String> = r.get(1)?;
                    Ok(if matches!(kind.as_str(), "undo" | "redo") {
                        stored
                            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                            .filter(|v| v["v"] == 1)
                            .and_then(|v| v["details"]["original_kind"].as_str().map(str::to_owned))
                    } else {
                        Some(kind)
                    })
                },
            )
            .optional()?
            .flatten()
            .filter(|kind| kind != "unknown");
        return Ok(kind.map(|kind| json!({"type":"undo","original_kind":kind})));
    }
    Ok(match &action.inverse {
        Inverse::Ratings(rows) => match rows.first() {
            Some((id, _)) => {
                let rating: Option<u8> = tx.query_row(
                    "SELECT rating FROM track_user WHERE track_id=?1",
                    [id],
                    |r| r.get(0),
                )?;
                Some(json!({"type":"set_rating","rating":rating}))
            }
            None => None,
        },
        Inverse::Loved(rows) => match rows.first() {
            Some((id, _)) => {
                let loved: bool = tx.query_row(
                    "SELECT loved FROM track_user WHERE track_id=?1",
                    [id],
                    |r| r.get(0),
                )?;
                Some(json!({"type":"set_loved","loved":loved}))
            }
            None => None,
        },
        Inverse::Playlist(id, _) => {
            let name: String =
                tx.query_row("SELECT name FROM playlists WHERE id=?1", [id.0], |r| {
                    r.get(0)
                })?;
            Some(json!({"type":"playlist","playlist":id.0,"name":name}))
        }
        Inverse::Root(id, _) => {
            let path: String =
                tx.query_row("SELECT path FROM roots WHERE id=?1", [id.0], |r| r.get(0))?;
            Some(json!({"type":"root","root":id.0,"path":path}))
        }
        Inverse::Covers(_) => None,
        Inverse::Tags(_) => None, // Filled from the actual key operations by record_tag_edit.
    })
}

/// Records an audit entry: a change that cannot be undone (`purge`, `file_write`), listed
/// with the actions but never undoable (no inverse, no targets). `target_count` counts
/// what it changed.
pub(crate) fn record_audit(
    tx: &Connection,
    meta: ActionMeta<'_>,
    kind: &'static str,
    summary: String,
    target_count: u64,
    now: i64,
) -> Result<ActionId> {
    tx.prepare_cached(
        "INSERT INTO actions (at_ms, origin, kind, summary, target_count, inverse, grant_id)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)",
    )?
    .execute(params![
        now,
        meta.origin,
        kind,
        summary,
        i64::try_from(target_count).unwrap_or(i64::MAX),
        meta.grant,
    ])?;
    let id = ActionId(tx.last_insert_rowid());
    prune(tx, now)?;
    Ok(id)
}

/// The library roots an action changed (a `root_add` / `root_remove`, or an undo of one).
pub(crate) fn roots_of(conn: &Connection, id: ActionId) -> Result<Vec<RootId>> {
    Ok(conn
        .prepare_cached(
            "SELECT object_id FROM action_targets WHERE action_id = ?1 AND kind = 'root'
             ORDER BY object_id",
        )?
        .query_map([id.0], |row| Ok(RootId(row.get(0)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Keeps at most [`crate::PLAYLIST_UNDO_STEPS`] undoable actions per playlist: older ones
/// lose their inverse.
fn cap_playlist(tx: &Connection, id: i64) -> Result<()> {
    tx.prepare_cached(
        "UPDATE actions SET inverse = NULL WHERE id IN (
             SELECT a.id FROM action_targets t JOIN actions a ON a.id = t.action_id
             WHERE t.kind = 'playlist' AND t.object_id = ?1
               AND a.inverse IS NOT NULL AND a.undone_by IS NULL
             ORDER BY a.id DESC LIMIT -1 OFFSET ?2)",
    )?
    .execute(params![id, playlist::PLAYLIST_UNDO_STEPS as i64])?;
    Ok(())
}

/// Drops actions older than [`ACTION_RETENTION_MS`] at `now`, with every older one by id
/// (what remains is a contiguous history even if the clock went back), and all but the
/// newest [`ACTION_LIMIT`]. Audit entries are kept apart: one goes when it is older than
/// [`AUDIT_RETENTION_MS`] and not among the newest [`AUDIT_LIMIT`] of them.
pub(crate) fn prune(conn: &Connection, now: i64) -> Result<()> {
    let cutoff = now.saturating_sub(ACTION_RETENTION_MS);
    conn.prepare_cached(&format!(
        "WITH RECURSIVE boundary(id) AS (
           SELECT max(
             coalesce((SELECT max(id) FROM actions
                       WHERE kind NOT IN {AUDIT_KINDS_SQL} AND at_ms < ?1), 0),
             coalesce((SELECT id FROM actions WHERE kind NOT IN {AUDIT_KINDS_SQL}
                       ORDER BY id DESC LIMIT 1 OFFSET ?2), 0)
           )
         ), keep(id) AS (
           SELECT undo_of FROM actions
           WHERE id > (SELECT id FROM boundary)
             AND inverse IS NOT NULL AND undone_by IS NULL AND undo_of IS NOT NULL
           UNION
           SELECT json_extract(iif(json_valid(target_json), target_json, NULL),'$.undo_of') FROM file_write_intents
           WHERE stage NOT IN ('finalized','retained')
             AND json_extract(iif(json_valid(target_json), target_json, NULL),'$.undo_of') IS NOT NULL
           UNION
           SELECT a.id FROM actions a JOIN file_write_intents i
             ON json_extract(a.details_json,'$.cover_group')=json_extract(iif(json_valid(i.target_json), i.target_json, NULL),'$.group')
           WHERE i.stage NOT IN ('finalized','retained')
           UNION
           SELECT a.undo_of FROM actions a JOIN keep k ON a.id=k.id WHERE a.undo_of IS NOT NULL
         )
         DELETE FROM actions WHERE kind NOT IN {AUDIT_KINDS_SQL}
           AND id <= (SELECT id FROM boundary) AND id NOT IN (SELECT id FROM keep)"
    ))?
    .execute(params![cutoff, ACTION_LIMIT as i64])?;
    conn.prepare_cached(&format!(
        "DELETE FROM actions WHERE kind IN {AUDIT_KINDS_SQL} AND at_ms < ?1 AND id <= (
             SELECT id FROM actions WHERE kind IN {AUDIT_KINDS_SQL}
             ORDER BY id DESC LIMIT 1 OFFSET ?2)"
    ))?
    .execute(params![
        now.saturating_sub(AUDIT_RETENTION_MS),
        AUDIT_LIMIT as i64
    ])?;
    Ok(())
}

/// Drops the actions of playlists that are being purged (they cannot be undone any more).
pub(crate) fn forget_playlist(conn: &Connection, id: PlaylistId) -> Result<()> {
    conn.prepare_cached(
        "DELETE FROM actions WHERE id IN (
             SELECT action_id FROM action_targets
             WHERE kind IN ('playlist', 'playlist_trash') AND object_id = ?1)",
    )?
    .execute([id.0])?;
    Ok(())
}
