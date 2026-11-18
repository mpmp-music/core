use super::*;

// ---- tag edits (crate::tags::job) ---

/// Records a tag edit of `files` (each with what to write back to undo it): the tracks'
/// tag versions are bumped (the caller has updated their rows).
pub(crate) fn record_tag_edit(
    tx: &Connection,
    meta: ActionMeta<'_>,
    summary: String,
    files: Vec<FileTags>,
    changes: serde_json::Value,
    now: i64,
) -> Result<ActionId> {
    let digests = tag_digests(&files);
    let targets = files
        .iter()
        .map(|f| bump(tx, Kind::Tags, f.track, now))
        .collect::<Result<Vec<_>>>()?;
    let id = record(
        tx,
        meta,
        NewAction {
            kind: "tag_edit",
            summary,
            inverse: Inverse::Tags(files),
            targets,
        },
        now,
    )?;
    let details = serde_json::json!({
        "v":1,"details":{"type":"tag_edit","changes":changes},"tag_digests":digests
    });
    tx.execute(
        "UPDATE actions SET details_json=?2 WHERE id=?1",
        params![id.0, details.to_string()],
    )?;
    Ok(id)
}

fn tag_digests(files: &[FileTags]) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    for file in files {
        if let Some(exact) = &file.exact {
            object.insert(
                file.track.to_string(),
                serde_json::Value::String(crate::tags::edit::snapshot::hex(exact.after)),
            );
        }
    }
    if object.len() == files.len() {
        serde_json::Value::Object(object)
    } else {
        serde_json::Value::Null
    }
}

/// What undoing action `id` writes back into files, checked as any undo is (unknown,
/// undone, expired, a later change in the journal: [`LibraryError::ActionConflict`]).
/// `None`: the action does not write files back (not a tag edit, nor an undo of one).
pub(crate) fn tag_undo(conn: &Connection, id: ActionId, now: i64) -> Result<Option<Vec<FileTags>>> {
    if tag_tracks(conn, id)?.is_empty() {
        return Ok(None);
    }
    let (_, inverse, _) = undoable(conn, id, now)?;
    match Inverse::decode(&inverse) {
        Some(Inverse::Tags(files)) => Ok(Some(files)),
        _ => Err(LibraryError::Corrupt(format!("the inverse of action {id}"))),
    }
}

/// Records the undo of tag edit `id` (checked again as in [`tag_undo`]) for the files
/// written back (`redo`, each with what to write to redo it): those tracks' tag versions
/// go back to the state before the edit. The files not written back (an error while
/// writing them) stay with the edit: it is undone only once every file is; until then it is
/// not undone (`undone_by` is `NULL`), and its targets, inverse and count are those of the
/// files left, which undoing it again writes back. The undo counts the files it wrote back
pub(crate) fn record_tag_undo(
    tx: &Connection,
    id: ActionId,
    meta: ActionMeta<'_>,
    redo: Vec<FileTags>,
    now: i64,
    legacy: bool,
) -> Result<ActionId> {
    let (action, inverse, targets) = undoable(tx, id, now)?;
    let written: HashSet<i64> = redo.iter().map(|f| f.track).collect();
    let Some(Inverse::Tags(files)) = Inverse::decode(&inverse) else {
        return Err(LibraryError::Corrupt(format!("the inverse of action {id}")));
    };
    let left: Vec<FileTags> = files
        .into_iter()
        .filter(|f| !written.contains(&f.track))
        .collect();
    let mut new_targets = Vec::with_capacity(written.len());
    for t in targets
        .iter()
        .filter(|t| t.kind == Kind::Tags && written.contains(&t.id))
    {
        let mut bumped = bump(tx, t.kind, t.id, now)?;
        bumped.state_after = t.state_before;
        new_targets.push(bumped);
    }
    let (kind, mut summary) = undo_summary(&action);
    if legacy {
        summary.push_str(" (legacy value restoration)");
    }
    let stamps: Vec<(i64, u64, i64)> = redo.iter().map(|f| (f.track, f.size, f.mtime_ms)).collect();
    let states: Vec<(i64, i64)> = new_targets.iter().map(|t| (t.id, t.state_after)).collect();
    let digests = tag_digests(&redo);
    let undo = insert(
        tx,
        meta,
        NewAction {
            kind,
            summary,
            inverse: Inverse::Tags(redo),
            targets: new_targets,
        },
        Some(id),
        !action.applies,
        now,
    )?;
    if !digests.is_null() {
        tx.execute(
            "UPDATE actions SET details_json=json_set(details_json,'$.tag_digests',json(?2)) WHERE id=?1",
            params![undo.0, digests.to_string()],
        )?;
    }
    // The files are in an earlier state again, written anew: the actions that left them
    // in it (and can be undone from it) now find them as this undo left them.
    for (track, state) in states {
        if let Some(&(_, size, mtime_ms)) = stamps.iter().find(|s| s.0 == track) {
            restamp(tx, track, state, (size, mtime_ms), undo)?;
        }
    }
    if left.is_empty() {
        // Its replacement inverse is on the undo action. Keep operation facts and
        // chain links while releasing the historical snapshot.
        tx.execute("UPDATE actions SET inverse=NULL WHERE id=?1", [id.0])?;
    } else {
        for track in &written {
            tx.prepare_cached(
                "DELETE FROM action_targets
                 WHERE action_id = ?1 AND kind = 'track_tags' AND object_id = ?2",
            )?
            .execute(params![id.0, track])?;
        }
        let count = left.len() as i64;
        tx.prepare_cached(
            "UPDATE actions SET undone_by = NULL, inverse = ?2, target_count = ?3 WHERE id = ?1",
        )?
        .execute(params![id.0, Inverse::Tags(left).encode(), count])?;
    }
    Ok(undo)
}

/// Sets the size and modification time that the tag actions which left `track` in `state`
/// (but `except`) expect its file to have.
fn restamp(
    tx: &Connection,
    track: i64,
    state: i64,
    (size, mtime_ms): (u64, i64),
    except: ActionId,
) -> Result<()> {
    let actions: Vec<(i64, Vec<u8>)> = tx
        .prepare_cached(
            "SELECT a.id, a.inverse FROM action_targets t JOIN actions a ON a.id = t.action_id
             WHERE t.kind = 'track_tags' AND t.object_id = ?1 AND t.state_after = ?2
               AND a.id != ?3 AND a.inverse IS NOT NULL",
        )?
        .query_map(params![track, state, except.0], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (id, data) in actions {
        let Some(Inverse::Tags(mut files)) = Inverse::decode(&data) else {
            continue;
        };
        for file in files.iter_mut().filter(|f| f.track == track) {
            (file.size, file.mtime_ms) = (size, mtime_ms);
        }
        tx.prepare_cached("UPDATE actions SET inverse = ?2 WHERE id = ?1")?
            .execute(params![id, Inverse::Tags(files).encode()])?;
    }
    Ok(())
}

/// The tracks whose files undoing action `id` writes (a tag edit, or an undo or redo of
/// one); empty for any other action.
pub(crate) fn tag_tracks(conn: &Connection, id: ActionId) -> Result<Vec<TrackId>> {
    Ok(conn
        .prepare_cached(
            "SELECT object_id FROM action_targets WHERE action_id = ?1 AND kind = 'track_tags'
             ORDER BY object_id",
        )?
        .query_map([id.0], |row| Ok(TrackId(row.get(0)?)))?
        .collect::<rusqlite::Result<_>>()?)
}
