use super::*;

// ---- undo ---

/// A stored action, as an undo reads it.
pub(super) struct Stored {
    at_ms: i64,
    summary: String,
    undone_by: Option<i64>,
    /// Applies a change (an original action or a redo), as opposed to reverting one.
    pub(super) applies: bool,
    inverse: Option<Vec<u8>>,
}

fn stored(conn: &Connection, id: ActionId) -> Result<Option<Stored>> {
    Ok(conn
        .prepare_cached(
            "SELECT at_ms, summary, undone_by, undo_of IS NULL OR redo, inverse
             FROM actions WHERE id = ?1",
        )?
        .query_row([id.0], |row| {
            Ok(Stored {
                at_ms: row.get(0)?,
                summary: row.get(1)?,
                undone_by: row.get(2)?,
                applies: row.get(3)?,
                inverse: row.get(4)?,
            })
        })
        .optional()?)
}

fn targets(conn: &Connection, id: ActionId) -> Result<Vec<Target>> {
    let rows: Vec<(String, i64, i64, i64, i64)> = conn
        .prepare_cached(
            "SELECT kind, object_id, version_after, state_before, state_after
             FROM action_targets WHERE action_id = ?1",
        )?
        .query_map([id.0], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(kind, id, version_after, state_before, state_after)| {
            let kind = Kind::parse(&kind).ok_or_else(|| {
                LibraryError::Corrupt(format!("action target of unknown kind {kind:?}"))
            })?;
            Ok(Target {
                kind,
                id,
                version_after,
                state_before,
                state_after,
            })
        })
        .collect()
}

fn action_not_found(id: ActionId, why: &str) -> LibraryError {
    LibraryError::not_found(NotFound::Action, format!("action {id} {why}"))
}

/// Describe only a valid, currently undoable inverse. Unknown/corrupt/pruned chains
/// fail closed, even when an inverse alone might still be executable by older APIs.
pub(crate) fn describe_undo(conn: &Connection, id: ActionId, now: i64) -> Result<UndoDescription> {
    use sha2::{Digest, Sha256};
    let (_, bytes, targets) = undoable(conn, id, now)?;
    let inverse = Inverse::decode(&bytes)
        .ok_or_else(|| LibraryError::Corrupt(format!("inverse of action {id}")))?;
    let (kind, writes) = match &inverse {
        Inverse::Ratings(_) => ("set_rating", false),
        Inverse::Loved(_) => ("set_loved", false),
        Inverse::Playlist(_, _) => ("playlist_edit", false),
        Inverse::Root(_, RootOp::Remove) => ("remove_root", false),
        Inverse::Root(_, RootOp::Restore) => ("add_root", false),
        Inverse::Tags(_) => ("edit_tags", true),
        Inverse::Covers(_) => ("edit_cover", true),
    };
    let mut chain = vec![];
    let mut next = id;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(next) || chain.len() >= ACTION_LIMIT {
            return Err(LibraryError::Corrupt(
                "cyclic or oversized undo chain".into(),
            ));
        }
        chain.push(next);
        let row: Option<(String, Option<i64>)> = conn
            .query_row(
                "SELECT kind, undo_of FROM actions WHERE id=?1",
                [next.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (stored_kind, parent) =
            row.ok_or_else(|| action_not_found(next, "undo chain was pruned"))?;
        match parent {
            Some(parent) if matches!(stored_kind.as_str(), "undo" | "redo") => {
                next = ActionId(parent)
            }
            None if matches!(
                (stored_kind.as_str(), &inverse),
                ("set_rating", Inverse::Ratings(_))
                    | ("set_loved", Inverse::Loved(_))
                    | ("root_add" | "root_remove", Inverse::Root(_, _))
                    | ("tag_edit", Inverse::Tags(_))
                    | ("cover_edit", Inverse::Covers(_))
            ) || (matches!(
                stored_kind.as_str(),
                "playlist_create"
                    | "playlist_import"
                    | "playlist_rename"
                    | "playlist_add"
                    | "playlist_remove"
                    | "playlist_move"
                    | "playlist_set_query"
                    | "playlist_delete"
                    | "playlist_restore"
            ) && matches!(&inverse, Inverse::Playlist(_, _))) =>
            {
                break;
            }
            _ => {
                return Err(LibraryError::Corrupt(
                    "unknown or inconsistent undo chain".into(),
                ));
            }
        }
    }
    let mut hash = Sha256::new();
    hash.update(&bytes);
    hash.update(format!("{id:?}{chain:?}{targets:?}"));
    let mut objects: Vec<_> = targets.iter().map(|t| t.kind.object(t.id)).collect();
    objects.sort_unstable();
    objects.dedup();
    let mut effective = match &inverse {
        Inverse::Ratings(rows) => rows
            .iter()
            .map(|(id, _)| ActionTarget::Track { id: TrackId(*id) })
            .collect::<Vec<_>>(),
        Inverse::Loved(rows) => rows
            .iter()
            .map(|(id, _)| ActionTarget::Track { id: TrackId(*id) })
            .collect(),
        Inverse::Playlist(id, _) => vec![ActionTarget::Playlist { id: *id }],
        Inverse::Root(id, _) => vec![ActionTarget::Root { id: *id }],
        Inverse::Covers(files) => files
            .iter()
            .map(|f| ActionTarget::Track {
                id: TrackId(f.track),
            })
            .collect(),
        Inverse::Tags(files) => files
            .iter()
            .map(|f| ActionTarget::Track {
                id: TrackId(f.track),
            })
            .collect(),
    };
    effective.sort_unstable();
    effective.dedup();
    if effective.is_empty() || effective != objects {
        return Err(LibraryError::Corrupt(
            "inverse targets differ from journal targets".into(),
        ));
    }
    Ok(UndoDescription {
        action: id,
        original: next,
        chain,
        effective_kind: kind.into(),
        writes_files: writes,
        targets: objects,
        fingerprint: format!("{:x}", hash.finalize()),
    })
}

pub(crate) fn check_undo_description(
    conn: &Connection,
    expected: &UndoDescription,
    now: i64,
) -> Result<()> {
    if describe_undo(conn, expected.action, now)? != *expected {
        return Err(LibraryError::ActionConflict {
            action: expected.action,
            changed: expected.targets.clone(),
        });
    }
    Ok(())
}

pub(crate) fn undo_guarded(
    storage: &dyn mpmp_storage::LibraryStorage,
    conn: &mut Connection,
    expected: UndoDescription,
    meta: ActionMeta<'_>,
    now: i64,
) -> Result<(ActionId, UndoEffect)> {
    run_undo(
        storage,
        conn,
        now,
        |tx| {
            check_undo_description(tx, &expected, now)?;
            if expected.writes_files {
                return Err(LibraryError::Corrupt("file undo requires a tag job".into()));
            }
            Ok(expected.action)
        },
        meta,
    )
}

/// Undoes action `id`: the undo's id and what changed.
pub(crate) fn undo_action(
    storage: &dyn mpmp_storage::LibraryStorage,
    conn: &mut Connection,
    id: ActionId,
    meta: ActionMeta<'_>,
    now: i64,
) -> Result<(ActionId, UndoEffect)> {
    run_undo(storage, conn, now, |_| Ok(id), meta)
}

/// Undoes the newest edit of a playlist that is at `revision` (see
/// [`crate::Library::undo_playlist`]): the undo's id and the new revision.
pub(crate) fn undo_playlist(
    storage: &dyn mpmp_storage::LibraryStorage,
    conn: &mut Connection,
    id: PlaylistId,
    revision: u64,
    meta: ActionMeta<'_>,
    now: i64,
) -> Result<(ActionId, u64)> {
    let pick = |tx: &Transaction<'_>| -> Result<ActionId> {
        let current = playlist::editable_revision(tx, id)?;
        if current != revision {
            return Err(LibraryError::PlaylistConflict {
                expected: revision,
                actual: current,
            });
        }
        let state = state_of(tx, Kind::Playlist, id.0, current as i64)?;
        // The newest edit (or redo) still in effect.
        let newest: Option<(i64, i64)> = tx
            .prepare_cached(
                "SELECT a.id, t.state_after FROM action_targets t
                 JOIN actions a ON a.id = t.action_id
                 WHERE t.kind = 'playlist' AND t.object_id = ?1
                   AND a.undone_by IS NULL AND a.inverse IS NOT NULL
                   AND (a.undo_of IS NULL OR a.redo) AND a.at_ms >= ?2
                 ORDER BY a.id DESC LIMIT 1",
            )?
            .query_row(
                params![id.0, now.saturating_sub(ACTION_RETENTION_MS)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match newest {
            Some((action, after)) if after == state => Ok(ActionId(action)),
            _ => Err(LibraryError::not_found(
                NotFound::Undo,
                format!("nothing to undo in playlist {id}"),
            )),
        }
    };
    let (action, effect) = run_undo(storage, conn, now, pick, meta)?;
    debug_assert_eq!(effect.playlists, [id]);
    Ok((action, revision + 1))
}

/// Picks the action to undo and undoes it, in one transaction. An inverse that cannot be
/// read is reported once ([`LibraryError::Corrupt`]); the action is then marked as not
/// undoable and transparent, so older actions can be undone past it.
fn run_undo(
    storage: &dyn mpmp_storage::LibraryStorage,
    conn: &mut Connection,
    now: i64,
    pick: impl FnOnce(&Transaction<'_>) -> Result<ActionId>,
    meta: ActionMeta<'_>,
) -> Result<(ActionId, UndoEffect)> {
    // Expired actions go first, also when the undo then fails.
    prune(conn, now)?;
    let tx = conn.transaction()?;
    let target = pick(&tx)?;
    match undo_in(storage, &tx, target, meta, now) {
        Ok(done) => {
            tx.commit()?;
            Ok(done)
        }
        Err(LibraryError::Corrupt(message)) => {
            drop(tx);
            tracing::warn!(action = %target, %message, "dropping an action that cannot be undone");
            let tx = conn.transaction()?;
            mark_corrupt(&tx, target)?;
            tx.commit()?;
            Err(LibraryError::Corrupt(message))
        }
        Err(err) => Err(err),
    }
}

/// Makes a corrupt action not undoable, and transparent for the conflict check: its
/// objects count as still in the state before it.
fn mark_corrupt(tx: &Connection, id: ActionId) -> Result<()> {
    tx.execute("UPDATE actions SET inverse = NULL WHERE id = ?1", [id.0])?;
    tx.execute(
        "UPDATE action_targets SET state_after = state_before WHERE action_id = ?1",
        [id.0],
    )?;
    Ok(())
}

/// An action that can be undone now: it exists, is not undone and not expired, and every
/// object it changed is still in the state it left it in (else
/// [`LibraryError::ActionConflict`]). With its inverse (encoded) and targets.
pub(super) fn undoable(
    tx: &Connection,
    id: ActionId,
    now: i64,
) -> Result<(Stored, Vec<u8>, Vec<Target>)> {
    let action = stored(tx, id)?.ok_or_else(|| action_not_found(id, "is unknown or expired"))?;
    if let Some(by) = action.undone_by {
        return Err(LibraryError::invalid(
            Invalid::AlreadyUndone,
            format!("action {id} was already undone (by action {by})"),
        ));
    }
    let Some(inverse) = action
        .inverse
        .clone()
        .filter(|_| action.at_ms >= now.saturating_sub(ACTION_RETENTION_MS))
    else {
        return Err(action_not_found(id, "can no longer be undone"));
    };
    let mut targets = targets(tx, id)?;
    // Cover targets are immutable operation-time facts. After a partial undo only
    // the remaining inverse participates in conflict checks and another retry.
    if let Some(Inverse::Covers(files)) = Inverse::decode(&inverse) {
        let pending: HashSet<_> = files.iter().map(|file| file.track).collect();
        targets.retain(|target| target.kind != Kind::Cover || pending.contains(&target.id));
    }
    let mut changed = Vec::new();
    for t in &targets {
        let current = match version_of(tx, t.kind, t.id)? {
            Some(version) => state_of(tx, t.kind, t.id, version)?,
            None => i64::MIN,
        };
        if current != t.state_after {
            changed.push(t.kind.object(t.id));
        }
    }
    if !changed.is_empty() {
        changed.sort_unstable();
        changed.dedup();
        return Err(LibraryError::ActionConflict {
            action: id,
            changed,
        });
    }
    Ok((action, inverse, targets))
}

fn undo_in(
    storage: &dyn mpmp_storage::LibraryStorage,
    tx: &Transaction<'_>,
    id: ActionId,
    meta: ActionMeta<'_>,
    now: i64,
) -> Result<(ActionId, UndoEffect)> {
    let (action, inverse, targets) = undoable(tx, id, now)?;
    let inverse = Inverse::decode(&inverse)
        .ok_or_else(|| LibraryError::Corrupt(format!("the inverse of action {id}")))?;
    let redo = apply(storage, tx, &inverse, now)?;
    let mut new_targets = Vec::with_capacity(targets.len());
    for t in &targets {
        let mut bumped = bump(tx, t.kind, t.id, now)?;
        bumped.state_after = t.state_before;
        new_targets.push(bumped);
    }
    let (kind, summary) = undo_summary(&action);
    let effect = effect(id, &new_targets);
    let undo = insert(
        tx,
        meta,
        NewAction {
            kind,
            summary,
            inverse: redo,
            targets: new_targets,
        },
        Some(id),
        !action.applies,
        now,
    )?;
    Ok((undo, effect))
}

/// An undo's kind and summary: `undo` of what applied a change, `redo` of an undo.
pub(super) fn undo_summary(action: &Stored) -> (&'static str, String) {
    let base = action
        .summary
        .strip_prefix("undo: ")
        .or_else(|| action.summary.strip_prefix("redo: "))
        .unwrap_or(&action.summary);
    if action.applies {
        ("undo", format!("undo: {base}"))
    } else {
        ("redo", format!("redo: {base}"))
    }
}

fn effect(undone: ActionId, targets: &[Target]) -> UndoEffect {
    let mut effect = UndoEffect {
        undone,
        tracks: Vec::new(),
        playlists: Vec::new(),
        roots: Vec::new(),
    };
    let mut playlists = HashSet::new();
    for t in targets {
        match t.kind {
            Kind::Track | Kind::Tags | Kind::Cover => effect.tracks.push(TrackId(t.id)),
            Kind::Playlist | Kind::Trash => {
                if playlists.insert(t.id) {
                    effect.playlists.push(PlaylistId(t.id));
                }
            }
            Kind::Root => effect.roots.push(RootId(t.id)),
        }
    }
    effect
}

/// Applies an inverse: the inverse of what it did (to redo it).
fn apply(
    storage: &dyn mpmp_storage::LibraryStorage,
    tx: &Transaction<'_>,
    inverse: &Inverse,
    now: i64,
) -> Result<Inverse> {
    Ok(match inverse {
        Inverse::Ratings(values) => Inverse::Ratings(ratings::apply_ratings(tx, values)?),
        Inverse::Loved(values) => Inverse::Loved(ratings::apply_loved(tx, values)?),
        Inverse::Playlist(id, op) => Inverse::Playlist(*id, playlist::apply_op(tx, *id, op, now)?),
        Inverse::Root(id, op) => Inverse::Root(*id, writer::apply_root_op(storage, tx, *id, *op)?),
        // Files are written by a tag job, outside any transaction (`crate::tags::job`);
        // `Library::undo_action` hands tag edits to one.
        // (Not `Corrupt`: that would make the action not undoable.)
        Inverse::Tags(_) | Inverse::Covers(_) => {
            return Err(LibraryError::Io(std::io::Error::other(
                "a tag edit's undo writes files: a tag job does it",
            )));
        }
    })
}
