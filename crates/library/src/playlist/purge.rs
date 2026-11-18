use super::*;

pub(crate) fn purge_selection(
    conn: &Connection,
    counts: &Counts<'_>,
    ids: Option<&[PlaylistId]>,
) -> Result<PurgeGuard> {
    let mut selected = match ids {
        Some(ids) => ids.to_vec(),
        None => conn
            .prepare("SELECT id FROM playlists WHERE deleted_at IS NOT NULL")?
            .query_map([], |row| Ok(PlaylistId(row.get(0)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    selected.sort_unstable();
    selected.dedup();
    let playlists = selected
        .into_iter()
        .map(|id| {
            let stored = stored(conn, id)?.ok_or(LibraryError::PurgeChanged)?;
            if stored.deleted_at.is_none() {
                return Err(LibraryError::PurgeChanged);
            }
            Ok(summarize(conn, counts, stored, true)?.0)
        })
        .collect::<Result<_>>()?;
    Ok(PurgeGuard {
        all: ids.is_none(),
        playlists,
        trash: None,
        expiry_clock: None,
    })
}

/// Permanently deletes playlists in the trash (`None`: all of them), with their actions.
/// All or nothing: a playlist that is unknown or not in the trash fails the whole call. It
/// cannot be undone: when it deleted anything, it is recorded as an audit entry (kind
/// `purge`, never undoable), the returned action.
pub(crate) fn purge(
    conn: &mut Connection,
    ids: Option<&[PlaylistId]>,
    meta: ActionMeta<'_>,
    now: i64,
) -> Result<Done<u64>> {
    purge_impl(conn, ids, meta, now, None, PurgeReviewClock::now)
}

pub(crate) fn purge_guarded(
    conn: &mut Connection,
    guard: &PurgeGuard,
    counts: &Counts<'_>,
    meta: ActionMeta<'_>,
    now: i64,
) -> Result<Done<u64>> {
    purge_guarded_with_clock(conn, guard, counts, meta, now, PurgeReviewClock::now)
}

pub(crate) fn purge_guarded_with_clock(
    conn: &mut Connection,
    guard: &PurgeGuard,
    counts: &Counts<'_>,
    meta: ActionMeta<'_>,
    now: i64,
    clock: impl FnOnce() -> PurgeReviewClock,
) -> Result<Done<u64>> {
    let ids: Vec<_> = guard.playlists.iter().map(|p| p.id).collect();
    purge_impl(conn, Some(&ids), meta, now, Some((guard, counts)), clock)
}

fn purge_impl(
    conn: &mut Connection,
    ids: Option<&[PlaylistId]>,
    meta: ActionMeta<'_>,
    now: i64,
    guard: Option<(&PurgeGuard, &Counts<'_>)>,
    clock: impl FnOnce() -> PurgeReviewClock,
) -> Result<Done<u64>> {
    let tx = conn.transaction()?;
    if let Some((expected, counts)) = guard {
        let current = purge_selection(
            &tx,
            counts,
            if expected.all || expected.trash.is_some() {
                None
            } else {
                ids
            },
        )?;
        // Sample after reading inside this transaction: no host preflight can replace
        // the final eligibility and clock check immediately before deletion.
        if !expected.matches_current_at(current, clock()) {
            return Err(LibraryError::PurgeChanged);
        }
    }
    let doomed: Vec<PlaylistId> = match ids {
        None => tx
            .prepare("SELECT id FROM playlists WHERE deleted_at IS NOT NULL")?
            .query_map([], |row| Ok(PlaylistId(row.get(0)?)))?
            .collect::<rusqlite::Result<_>>()?,
        Some(ids) => {
            let mut unique = Vec::with_capacity(ids.len());
            let mut seen = HashSet::with_capacity(ids.len());
            for &id in ids {
                if !seen.insert(id) {
                    continue;
                }
                match stored(&tx, id)? {
                    None => return Err(playlist_not_found(id)),
                    Some(p) if p.deleted_at.is_none() => {
                        return Err(LibraryError::invalid(
                            Invalid::NotInTrash,
                            format!("playlist {id} is not in the trash; delete it first"),
                        ));
                    }
                    Some(_) => unique.push(id),
                }
            }
            unique
        }
    };
    let mut purged = 0;
    let mut names = Vec::new();
    for id in doomed {
        if let Some(p) = stored(&tx, id)? {
            names.push(p.name);
        }
        journal::forget_playlist(&tx, id)?;
        purged += tx.execute("DELETE FROM playlists WHERE id = ?1", [id.0])? as u64;
    }
    if purged == 0 {
        return Ok(Done::unchanged(0));
    }
    // It cannot be undone: an audit entry says who did it.
    let action = journal::record_audit(
        &tx,
        meta,
        "purge",
        purge_summary(purged, &names),
        purged,
        now,
    )?;
    tx.commit()?;
    Ok(Done {
        value: purged,
        action: Some(action),
    })
}

/// `permanently delete playlist "A"`, `permanently delete 3 playlists ("A", "B" and 1
/// more)`: the names of the first few.
fn purge_summary(purged: u64, names: &[String]) -> String {
    const SHOWN: usize = 5;
    if purged == 1 && names.len() == 1 {
        return format!("permanently delete playlist \"{}\"", names[0]);
    }
    let mut listed: Vec<String> = names
        .iter()
        .take(SHOWN)
        .map(|n| format!("\"{n}\""))
        .collect();
    let more = purged.saturating_sub(listed.len() as u64);
    if more > 0 && !listed.is_empty() {
        listed.push(format!("{more} more"));
    }
    let what = journal::count(purged as usize, "playlist");
    match listed.len() {
        0 => format!("permanently delete {what}"),
        1 => format!("permanently delete {what} ({})", listed[0]),
        n => format!(
            "permanently delete {what} ({} and {})",
            listed[..n - 1].join(", "),
            listed[n - 1]
        ),
    }
}
