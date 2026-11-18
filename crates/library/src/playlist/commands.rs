use super::*;

/// Bumps the revision and records the edit in the action journal, with its inverse: the
/// new revision and the action.
fn commit_edit(
    tx: &Transaction<'_>,
    p: &Stored,
    op: PlaylistOp,
    kind: &'static str,
    summary: String,
    meta: ActionMeta<'_>,
) -> Result<(u64, ActionId)> {
    let now = now_ms();
    let target = journal::bump(tx, Kind::Playlist, p.id.0, now)?;
    let action = journal::record(
        tx,
        meta,
        NewAction {
            kind,
            summary,
            inverse: Inverse::Playlist(p.id, op),
            targets: vec![target],
        },
        now,
    )?;
    Ok((target.version_after as u64, action))
}

/// Moves a playlist into or out of the trash and records it (`kind` and `summary`), with
/// the opposite move as the inverse.
fn commit_trash(
    tx: &Transaction<'_>,
    id: PlaylistId,
    trash: bool,
    kind: &'static str,
    summary: String,
    meta: ActionMeta<'_>,
    now: i64,
) -> Result<ActionId> {
    set_trashed(tx, id, trash, now)?;
    let target = journal::bump(tx, Kind::Trash, id.0, now)?;
    journal::record(
        tx,
        meta,
        NewAction {
            kind,
            summary,
            inverse: Inverse::Playlist(
                id,
                if trash {
                    PlaylistOp::Untrash
                } else {
                    PlaylistOp::Trash
                },
            ),
            targets: vec![target],
        },
        now,
    )
}

fn set_trashed(tx: &Connection, id: PlaylistId, trash: bool, now: i64) -> Result<()> {
    tx.execute(
        "UPDATE playlists SET deleted_at = ?2 WHERE id = ?1",
        params![id.0, trash.then_some(now)],
    )?;
    Ok(())
}

/// How to undo one change of a playlist: the inverse the action journal stores
/// ([`crate::journal::Inverse::Playlist`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlaylistOp {
    Name(String),
    /// The previous query JSON (as stored) and limit.
    Query {
        json: String,
        limit: Option<u32>,
    },
    /// Items to delete (an add's undo).
    Remove(Vec<i64>),
    /// Items to put back: `(position, item, track)`, by position (a removal's undo).
    Restore(Vec<(i64, i64, i64)>),
    /// An item to move back to position `from`.
    Move {
        item: i64,
        from: i64,
    },
    /// Move the playlist to the trash (the undo of a create or a restore).
    Trash,
    /// Take it out of the trash (the undo of a delete).
    Untrash,
}

/// Applies an undo to a playlist (the journal checked that it is in the state the undone
/// action left; it bumps the version): the opposite operation, to redo.
pub(crate) fn apply_op(
    tx: &Transaction<'_>,
    id: PlaylistId,
    op: &PlaylistOp,
    now: i64,
) -> Result<PlaylistOp> {
    let corrupt = || LibraryError::Corrupt(format!("an undo of playlist {id}"));
    let p = stored(tx, id)?.ok_or_else(|| playlist_not_found(id))?;
    if !matches!(op, PlaylistOp::Trash | PlaylistOp::Untrash) && p.deleted_at.is_some() {
        return Err(in_trash(id));
    }
    Ok(match op {
        PlaylistOp::Name(name) => {
            tx.execute(
                "UPDATE playlists SET name = ?2, name_norm = ?3 WHERE id = ?1",
                params![id.0, name, normalize(name)],
            )?;
            PlaylistOp::Name(p.name)
        }
        PlaylistOp::Query { json, limit } => {
            tx.execute(
                "UPDATE playlists SET query_json = ?2, item_limit = ?3 WHERE id = ?1",
                params![id.0, json, limit],
            )?;
            PlaylistOp::Query {
                json: p.query_json.unwrap_or_default(),
                limit: p.limit,
            }
        }
        PlaylistOp::Remove(items) => PlaylistOp::Restore(delete_items(tx, id, items)?),
        PlaylistOp::Restore(rows) => {
            restore_items(tx, id, rows)?;
            PlaylistOp::Remove(rows.iter().map(|&(_, item, _)| item).collect())
        }
        PlaylistOp::Move { item, from } => {
            let at = item_position(tx, id, PlaylistItemId(*item)).map_err(|_| corrupt())?;
            if *from < 0 || *from >= item_count(tx, id)? {
                return Err(corrupt());
            }
            move_position(tx, id, *item, at, *from)?;
            PlaylistOp::Move {
                item: *item,
                from: at,
            }
        }
        PlaylistOp::Trash => {
            set_trashed(tx, id, true, now)?;
            PlaylistOp::Untrash
        }
        PlaylistOp::Untrash => {
            set_trashed(tx, id, false, now)?;
            PlaylistOp::Trash
        }
    })
}

/// The revision of a playlist that may be edited (for an undo).
pub(crate) fn editable_revision(conn: &Connection, id: PlaylistId) -> Result<u64> {
    Ok(live(conn, id)?.revision)
}

// ---- item positions ---

/// Number of items (positions are dense: `0..len`).
pub(super) fn item_count(conn: &Connection, id: PlaylistId) -> Result<i64> {
    Ok(conn
        .prepare_cached(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM playlist_items WHERE playlist_id = ?1",
        )?
        .query_row([id.0], |row| row.get(0))?)
}

/// Inserts `tracks` at `at` (shifting later items): their new item ids.
fn insert_tracks(
    tx: &Transaction<'_>,
    id: PlaylistId,
    tracks: &[TrackId],
    at: i64,
    len: i64,
) -> Result<Vec<i64>> {
    let k = tracks.len() as i64;
    if at < len {
        tx.prepare_cached(
            "UPDATE playlist_items SET position = position + ?3
             WHERE playlist_id = ?1 AND position >= ?2",
        )?
        .execute(params![id.0, at, k])?;
    }
    let mut insert = tx.prepare_cached(
        "INSERT INTO playlist_items (playlist_id, position, track_id) VALUES (?1, ?2, ?3)",
    )?;
    let mut items = Vec::with_capacity(tracks.len());
    for (i, track) in tracks.iter().enumerate() {
        insert.execute(params![id.0, at + i as i64, track.0])?;
        items.push(tx.last_insert_rowid());
    }
    Ok(items)
}

/// Deletes the items that are in playlist `id` (others are ignored) and closes the gaps:
/// `(position, item, track)` of the deleted items by position.
fn delete_items(
    tx: &Transaction<'_>,
    id: PlaylistId,
    items: &[i64],
) -> Result<Vec<(i64, i64, i64)>> {
    let mut removed = Vec::with_capacity(items.len());
    {
        let mut find = tx.prepare_cached(
            "SELECT position, track_id FROM playlist_items WHERE id = ?1 AND playlist_id = ?2",
        )?;
        let mut seen = HashSet::with_capacity(items.len());
        for &item in items {
            if !seen.insert(item) {
                continue;
            }
            if let Some((position, track)) = find
                .query_row(params![item, id.0], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                })
                .optional()?
            {
                removed.push((position, item, track));
            }
        }
    }
    removed.sort_unstable();
    {
        let mut delete = tx.prepare_cached("DELETE FROM playlist_items WHERE id = ?1")?;
        for &(_, item, _) in &removed {
            delete.execute([item])?;
        }
    }
    if let Some(&(first, _, _)) = removed.first() {
        tx.prepare_cached(
            "UPDATE playlist_items SET position = r.p
             FROM (SELECT id AS rid, ?2 + ROW_NUMBER() OVER (ORDER BY position) - 1 AS p
                   FROM playlist_items WHERE playlist_id = ?1 AND position > ?2) AS r
             WHERE playlist_items.id = r.rid AND playlist_items.position <> r.p",
        )?
        .execute(params![id.0, first])?;
    }
    Ok(removed)
}

/// Puts deleted items back at their old positions (`(position, item, track)` by position,
/// as [`delete_items`] returned them), with their old ids.
fn restore_items(tx: &Transaction<'_>, id: PlaylistId, rows: &[(i64, i64, i64)]) -> Result<()> {
    let Some(&(first, _, _)) = rows.first() else {
        return Ok(());
    };
    // An item now at `q` was at `q + #{i: P_i - i <= q}` before the removal.
    let thresholds: Vec<i64> = rows
        .iter()
        .enumerate()
        .map(|(i, &(p, _, _))| p - i as i64)
        .collect();
    let later: Vec<(i64, i64)> = tx
        .prepare_cached(
            "SELECT id, position FROM playlist_items
             WHERE playlist_id = ?1 AND position >= ?2 ORDER BY position",
        )?
        .query_map(params![id.0, first], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    {
        let mut update =
            tx.prepare_cached("UPDATE playlist_items SET position = ?2 WHERE id = ?1")?;
        let mut below = 0usize;
        for (item, q) in later {
            while below < thresholds.len() && thresholds[below] <= q {
                below += 1;
            }
            update.execute(params![item, q + below as i64])?;
        }
    }
    let mut insert = tx.prepare_cached(
        "INSERT INTO playlist_items (id, playlist_id, position, track_id) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for &(position, item, track) in rows {
        insert.execute(params![item, id.0, position, track])?;
    }
    Ok(())
}

/// Moves an item from `from` to `to` (both valid positions).
fn move_position(
    tx: &Transaction<'_>,
    id: PlaylistId,
    item: i64,
    from: i64,
    to: i64,
) -> Result<()> {
    if from < to {
        tx.prepare_cached(
            "UPDATE playlist_items SET position = position - 1
             WHERE playlist_id = ?1 AND position > ?2 AND position <= ?3",
        )?
        .execute(params![id.0, from, to])?;
    } else {
        tx.prepare_cached(
            "UPDATE playlist_items SET position = position + 1
             WHERE playlist_id = ?1 AND position >= ?3 AND position < ?2",
        )?
        .execute(params![id.0, from, to])?;
    }
    tx.prepare_cached("UPDATE playlist_items SET position = ?2 WHERE id = ?1")?
        .execute(params![item, to])?;
    Ok(())
}

fn item_position(conn: &Connection, id: PlaylistId, item: PlaylistItemId) -> Result<i64> {
    conn.prepare_cached("SELECT position FROM playlist_items WHERE id = ?1 AND playlist_id = ?2")?
        .query_row(params![item.0, id.0], |row| row.get(0))
        .optional()?
        .ok_or_else(|| item_not_found(id, item.0))
}

// ---- writes (writer thread) ---
//
// Every edit that changes something bumps the revision and records an action in the
// journal (crate::journal); an edit that changes nothing returns the current revision and
// no action.

/// Creates a playlist; `tracks` fill a manual playlist (M3U import, recorded as
/// `playlist_import`). Starts at revision 0; undoing the creation moves it to the trash.
pub(crate) fn create(
    conn: &mut Connection,
    name: &str,
    kind: &PlaylistKind,
    tracks: &[TrackId],
    import: bool,
    meta: ActionMeta<'_>,
) -> Result<Done<PlaylistId>> {
    let name = check_name(name)?;
    let (kind_text, query_json, limit) = match kind {
        PlaylistKind::Manual => ("manual", None, None),
        PlaylistKind::Smart { query, max_tracks } => {
            track_query::validate_prepared(conn, query)?;
            ("smart", Some(encode_query(query)?), *max_tracks)
        }
    };
    let tx = conn.transaction()?;
    check_tracks(&tx, tracks)?;
    let now = now_ms();
    tx.execute(
        "INSERT INTO playlists (name, name_norm, kind, query_json, item_limit, created_at,
             updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![name, normalize(name), kind_text, query_json, limit, now],
    )?;
    let id = PlaylistId(tx.last_insert_rowid());
    insert_tracks(&tx, id, tracks, 0, 0)?;
    let target = journal::bump(&tx, Kind::Trash, id.0, now)?;
    let (action_kind, summary) = if import {
        (
            "playlist_import",
            format!(
                "import playlist \"{name}\" with {}",
                journal::count(tracks.len(), "track")
            ),
        )
    } else {
        ("playlist_create", format!("create playlist \"{name}\""))
    };
    let action = journal::record(
        &tx,
        meta,
        NewAction {
            kind: action_kind,
            summary,
            inverse: Inverse::Playlist(id, PlaylistOp::Trash),
            targets: vec![target],
        },
        now,
    )?;
    tx.commit()?;
    Ok(Done {
        value: id,
        action: Some(action),
    })
}

/// Every track must exist (unavailable ones are fine).
fn check_tracks(conn: &Connection, tracks: &[TrackId]) -> Result<()> {
    let mut exists = conn.prepare_cached("SELECT 1 FROM tracks WHERE id = ?1")?;
    let mut checked = HashSet::with_capacity(tracks.len());
    for track in tracks {
        if checked.insert(track.0) && !exists.exists([track.0])? {
            return Err(LibraryError::not_found(
                NotFound::Track,
                format!("track {track}"),
            ));
        }
    }
    Ok(())
}

pub(crate) fn rename(
    conn: &mut Connection,
    id: PlaylistId,
    name: &str,
    expected: Option<u64>,
    meta: ActionMeta<'_>,
) -> Result<Done<u64>> {
    let name = check_name(name)?;
    let tx = conn.transaction()?;
    let p = live(&tx, id)?;
    check_revision(&p, expected)?;
    if p.name == name {
        return Ok(Done::unchanged(p.revision));
    }
    tx.execute(
        "UPDATE playlists SET name = ?2, name_norm = ?3 WHERE id = ?1",
        params![id.0, name, normalize(name)],
    )?;
    let summary = format!("rename playlist \"{}\" to \"{name}\"", p.name);
    let (revision, action) = commit_edit(
        &tx,
        &p,
        PlaylistOp::Name(p.name.clone()),
        "playlist_rename",
        summary,
        meta,
    )?;
    tx.commit()?;
    Ok(Done {
        value: revision,
        action: Some(action),
    })
}

pub(crate) fn add(
    conn: &mut Connection,
    id: PlaylistId,
    tracks: &[TrackId],
    at: Option<u32>,
    expected: Option<u64>,
    meta: ActionMeta<'_>,
) -> Result<Done<(Vec<PlaylistItemId>, u64)>> {
    let tx = conn.transaction()?;
    let p = live(&tx, id)?;
    require_manual(&p)?;
    check_revision(&p, expected)?;
    let len = item_count(&tx, id)?;
    let at = match at {
        None => len,
        Some(at) if i64::from(at) <= len => i64::from(at),
        Some(at) => {
            return Err(LibraryError::invalid(
                Invalid::OutOfRange,
                format!("position {at} is past the end of playlist {id} ({len} items)"),
            ));
        }
    };
    if tracks.is_empty() {
        return Ok(Done::unchanged((Vec::new(), p.revision)));
    }
    check_tracks(&tx, tracks)?;
    let items = insert_tracks(&tx, id, tracks, at, len)?;
    let summary = format!(
        "add {} to playlist \"{}\"",
        journal::count(items.len(), "track"),
        p.name
    );
    let (revision, action) = commit_edit(
        &tx,
        &p,
        PlaylistOp::Remove(items.clone()),
        "playlist_add",
        summary,
        meta,
    )?;
    tx.commit()?;
    Ok(Done {
        value: (items.into_iter().map(PlaylistItemId).collect(), revision),
        action: Some(action),
    })
}

pub(crate) fn remove(
    conn: &mut Connection,
    id: PlaylistId,
    items: &[PlaylistItemId],
    expected: Option<u64>,
    meta: ActionMeta<'_>,
) -> Result<Done<(Vec<PlaylistItemId>, u64)>> {
    let tx = conn.transaction()?;
    let p = live(&tx, id)?;
    require_manual(&p)?;
    check_revision(&p, expected)?;
    let ids: Vec<i64> = items.iter().map(|i| i.0).collect();
    let removed = delete_items(&tx, id, &ids)?;
    if removed.is_empty() {
        return Ok(Done::unchanged((Vec::new(), p.revision)));
    }
    // In request order, each once.
    let mut gone: HashSet<i64> = removed.iter().map(|&(_, item, _)| item).collect();
    let removed_ids = ids
        .iter()
        .filter(|item| gone.remove(item))
        .map(|&item| PlaylistItemId(item))
        .collect();
    let summary = format!(
        "remove {} from playlist \"{}\"",
        journal::count(removed.len(), "item"),
        p.name
    );
    let (revision, action) = commit_edit(
        &tx,
        &p,
        PlaylistOp::Restore(removed),
        "playlist_remove",
        summary,
        meta,
    )?;
    tx.commit()?;
    Ok(Done {
        value: (removed_ids, revision),
        action: Some(action),
    })
}

pub(crate) fn move_item(
    conn: &mut Connection,
    id: PlaylistId,
    item: PlaylistItemId,
    to: u32,
    expected: Option<u64>,
    meta: ActionMeta<'_>,
) -> Result<Done<u64>> {
    let tx = conn.transaction()?;
    let p = live(&tx, id)?;
    require_manual(&p)?;
    check_revision(&p, expected)?;
    let from = item_position(&tx, id, item)?;
    let len = item_count(&tx, id)?;
    if i64::from(to) >= len {
        return Err(LibraryError::invalid(
            Invalid::OutOfRange,
            format!("position {to} is past the end of playlist {id} ({len} items)"),
        ));
    }
    let to = i64::from(to);
    if from == to {
        return Ok(Done::unchanged(p.revision));
    }
    move_position(&tx, id, item.0, from, to)?;
    let summary = format!("move an item in playlist \"{}\"", p.name);
    let (revision, action) = commit_edit(
        &tx,
        &p,
        PlaylistOp::Move { item: item.0, from },
        "playlist_move",
        summary,
        meta,
    )?;
    tx.commit()?;
    Ok(Done {
        value: revision,
        action: Some(action),
    })
}

pub(crate) fn set_query(
    conn: &mut Connection,
    id: PlaylistId,
    query: &TrackQuery,
    limit: Option<u32>,
    expected: Option<u64>,
    meta: ActionMeta<'_>,
) -> Result<Done<u64>> {
    track_query::validate_prepared(conn, query)?;
    let json = encode_query(query)?;
    let tx = conn.transaction()?;
    let p = live(&tx, id)?;
    if !p.smart {
        return Err(LibraryError::invalid(
            Invalid::PlaylistKindMismatch,
            format!("playlist {id} is a manual playlist; it has no query"),
        ));
    }
    check_revision(&p, expected)?;
    // Compared parsed: the same query may be stored with another key order (e.g. by the
    // v5 migration).
    if p.limit == limit && p.query().is_ok_and(|stored| stored == *query) {
        return Ok(Done::unchanged(p.revision));
    }
    tx.execute(
        "UPDATE playlists SET query_json = ?2, item_limit = ?3 WHERE id = ?1",
        params![id.0, json, limit],
    )?;
    let op = PlaylistOp::Query {
        json: p.query_json.clone().unwrap_or_default(),
        limit: p.limit,
    };
    let summary = format!("change the query of playlist \"{}\"", p.name);
    let (revision, action) = commit_edit(&tx, &p, op, "playlist_set_query", summary, meta)?;
    tx.commit()?;
    Ok(Done {
        value: revision,
        action: Some(action),
    })
}

/// Moves a playlist to the trash; one already there is left as it is (no action).
pub(crate) fn delete(
    conn: &mut Connection,
    id: PlaylistId,
    meta: ActionMeta<'_>,
    now: i64,
) -> Result<Done<()>> {
    let tx = conn.transaction()?;
    let p = stored(&tx, id)?.ok_or_else(|| playlist_not_found(id))?;
    if p.deleted_at.is_some() {
        return Ok(Done::unchanged(()));
    }
    let summary = format!("move playlist \"{}\" to the trash", p.name);
    let action = commit_trash(&tx, id, true, "playlist_delete", summary, meta, now)?;
    tx.commit()?;
    Ok(Done {
        value: (),
        action: Some(action),
    })
}

pub(crate) fn restore(
    conn: &mut Connection,
    id: PlaylistId,
    meta: ActionMeta<'_>,
    now: i64,
) -> Result<Done<()>> {
    let tx = conn.transaction()?;
    let p = stored(&tx, id)?.ok_or_else(|| playlist_not_found(id))?;
    // Restoring a playlist that is not in the trash changes nothing (idempotent, like
    // `delete`).
    if p.deleted_at.is_none() {
        return Ok(Done::unchanged(()));
    }
    let summary = format!("restore playlist \"{}\" from the trash", p.name);
    let action = commit_trash(&tx, id, false, "playlist_restore", summary, meta, now)?;
    tx.commit()?;
    Ok(Done {
        value: (),
        action: Some(action),
    })
}
