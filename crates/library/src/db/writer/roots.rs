use super::*;

/// Adds a root, or revives the removed root with the same path, and records it; an active
/// root with the same path is returned unchanged (no action).
pub(crate) fn add_root(
    storage: &dyn mpmp_storage::LibraryStorage,
    conn: &mut Connection,
    path: &Path,
    meta: ActionMeta<'_>,
) -> Result<Done<RootId>> {
    let text = path_text(path)?;
    let tx = conn.transaction()?;
    let existing = {
        let mut stmt = tx.prepare_cached("SELECT id, path, removed FROM roots")?;
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                PathBuf::from(row.get::<_, String>(1)?),
                row.get::<_, bool>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    // `path` is canonical; stored paths are compared in canonical form where reachable.
    // Reviving a removed root is checked against the active roots like a new one.
    let mut same = None;
    for (id, stored, removed) in existing {
        let other = storage_access::comparable_with(storage, &stored);
        if storage.same_dir(&other, path) {
            same = Some((id, removed));
        } else if !removed && storage.overlap(&other, path) {
            return Err(LibraryError::OverlappingRoot {
                path: path.to_owned(),
                existing: stored,
            });
        }
    }
    let now = now_ms();
    let id = match same {
        Some((id, false)) => return Ok(Done::unchanged(RootId(id))),
        // Revive a removed root: its tracks keep their ids and become available again as
        // the next scan finds them.
        Some((id, true)) => {
            tx.execute("UPDATE roots SET removed = 0 WHERE id = ?1", [id])?;
            RootId(id)
        }
        None => {
            tx.execute(
                "INSERT INTO roots (path, added_at) VALUES (?1, ?2)",
                params![text, now],
            )?;
            RootId(tx.last_insert_rowid())
        }
    };
    let target = journal::bump(&tx, Kind::Root, id.0, now)?;
    let action = journal::record(
        &tx,
        meta,
        NewAction {
            kind: "root_add",
            summary: format!("add library folder {text}"),
            inverse: Inverse::Root(id, RootOp::Remove),
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

/// Soft-removes a root and records it; a root removed already is left as it is
/// (no action).
pub(crate) fn remove_root(
    conn: &mut Connection,
    id: RootId,
    meta: ActionMeta<'_>,
) -> Result<Done<()>> {
    let tx = conn.transaction()?;
    let (path, removed): (String, bool) = tx
        .query_row(
            "SELECT path, removed FROM roots WHERE id = ?1",
            [id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| root_not_found(id))?;
    // Removing a removed root again changes nothing (idempotent, like moving a playlist
    // to the trash twice).
    if removed {
        return Ok(Done::unchanged(()));
    }
    deactivate_root(&tx, id)?;
    let now = now_ms();
    let target = journal::bump(&tx, Kind::Root, id.0, now)?;
    let action = journal::record(
        &tx,
        meta,
        NewAction {
            kind: "root_remove",
            summary: format!("remove library folder {path}"),
            inverse: Inverse::Root(id, RootOp::Restore),
            targets: vec![target],
        },
        now,
    )?;
    tx.commit()?;
    Ok(Done {
        value: (),
        action: Some(action),
    })
}

fn root_not_found(id: RootId) -> LibraryError {
    LibraryError::not_found(NotFound::Root, format!("library root {id}"))
}

/// Soft removal: rows stay so that re-adding the folder keeps track ids and history
/// links; the tracks just stop being available.
fn deactivate_root(tx: &Connection, id: RootId) -> Result<()> {
    tx.execute("UPDATE roots SET removed = 1 WHERE id = ?1", [id.0])?;
    tx.execute(
        "UPDATE tracks SET available = 0 WHERE root_id = ?1 AND available = 1",
        [id.0],
    )?;
    schema::refresh_aggregates(tx)?;
    Ok(())
}

/// Applies an undo to a root (the journal checked its state and bumps its version): the
/// opposite operation, to redo. A restored root's tracks become available with the next
/// scan; one that now overlaps an active root is [`LibraryError::OverlappingRoot`].
pub(crate) fn apply_root_op(
    storage: &dyn mpmp_storage::LibraryStorage,
    tx: &Connection,
    id: RootId,
    op: RootOp,
) -> Result<RootOp> {
    match op {
        RootOp::Remove => {
            deactivate_root(tx, id)?;
            Ok(RootOp::Restore)
        }
        RootOp::Restore => {
            let path: String = tx
                .query_row("SELECT path FROM roots WHERE id = ?1", [id.0], |row| {
                    row.get(0)
                })
                .optional()?
                .ok_or_else(|| root_not_found(id))?;
            let path = storage_access::comparable_with(storage, Path::new(&path));
            let mut stmt = tx.prepare_cached("SELECT path FROM roots WHERE removed = 0")?;
            let active = stmt
                .query_map([], |row| Ok(PathBuf::from(row.get::<_, String>(0)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for stored in active {
                if storage.overlap(&storage_access::comparable_with(storage, &stored), &path) {
                    return Err(LibraryError::OverlappingRoot {
                        path,
                        existing: stored,
                    });
                }
            }
            tx.execute("UPDATE roots SET removed = 0 WHERE id = ?1", [id.0])?;
            Ok(RootOp::Remove)
        }
    }
}

pub(super) fn set_root_encoding(
    conn: &mut Connection,
    id: RootId,
    label: Option<String>,
) -> Result<()> {
    const UPDATE_TRACKS: &str = r#"
UPDATE tracks SET mtime_ms = -1, metadata_revision = metadata_revision
+ 1 WHERE root_id = ?1
"#;

    let tx = conn.transaction()?;
    let current: Option<Option<String>> = tx
        .query_row(
            "SELECT forced_encoding FROM roots WHERE id = ?1 AND removed = 0",
            [id.0],
            |row| row.get(0),
        )
        .optional()?;
    let Some(current) = current else {
        return Err(LibraryError::not_found(
            NotFound::Root,
            format!("library root {id}"),
        ));
    };
    if current == label {
        return Ok(());
    }
    tx.execute(
        "UPDATE roots SET forced_encoding = ?2 WHERE id = ?1",
        params![id.0, label],
    )?;
    // Force the next scan to re-read every file of the root with the new setting.
    tx.execute(UPDATE_TRACKS, [id.0])?;
    schema::request_rescan(&tx)?;
    schema::refresh_aggregates(&tx)?;
    tx.commit()?;
    Ok(())
}
