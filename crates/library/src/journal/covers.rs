use super::*;

pub(crate) fn cover_undo(
    conn: &Connection,
    id: ActionId,
    now: i64,
) -> Result<Option<Vec<crate::cover::job::FileCover>>> {
    const SELECT_FILE_WRITE_INTENTS: &str = r#"
SELECT EXISTS(SELECT 1 FROM file_write_intents i JOIN actions a ON
json_extract(iif(json_valid(i.target_json), i.target_json, NULL),'$.group')=json_extract(a.details_json,'$.cover_group')
WHERE a.id=?1 AND i.stage NOT IN ('finalized','retained'))
"#;

    let description = describe_undo(conn, id, now)?;
    let pending: bool = conn.query_row(SELECT_FILE_WRITE_INTENTS, [id.0], |r| r.get(0))?;
    if pending {
        return Err(LibraryError::CoverRecoveryRequired);
    }
    if description.effective_kind != "edit_cover" {
        return Ok(None);
    }
    let (_, bytes, _) = undoable(conn, id, now)?;
    match Inverse::decode(&bytes) {
        Some(Inverse::Covers(files)) => Ok(Some(files)),
        _ => Err(LibraryError::Corrupt("cover inverse".into())),
    }
}

pub(crate) fn record_cover(
    tx: &Connection,
    meta: ActionMeta<'_>,
    files: Vec<crate::cover::job::FileCover>,
    operation: &str,
    undo_of: Option<ActionId>,
    now: i64,
    group: &str,
) -> Result<ActionId> {
    const SELECT_ACTIONS: &str = r#"
SELECT id FROM actions WHERE json_extract(details_json,'$.cover_group')=?1
AND inverse IS NOT NULL ORDER BY id LIMIT 1
"#;

    const UPDATE_ACTIONS: &str = r#"
UPDATE actions SET details_json=json_set(coalesce(details_json,'{"v":1}'),'$.cover_group',?2)
WHERE id=?1
"#;

    const UPDATE_ACTIONS_2: &str = r#"
UPDATE actions SET inverse=?2,target_count=?3,details_json=CASE WHEN
undo_of IS NULL THEN json_set(details_json,'$.details.file_count',?3)
ELSE details_json END,summary=CASE WHEN undo_of IS NULL THEN ?4 ELSE
summary END WHERE id=?1
"#;

    use rusqlite::OptionalExtension;
    let existing: Option<i64> = tx
        .query_row(SELECT_ACTIONS, [group], |r| r.get(0))
        .optional()?;
    let mut targets = Vec::new();
    for file in &files {
        targets.push(bump(tx, Kind::Cover, file.track, now)?);
    }
    let count = files.len();
    let id = if let Some(id) = undo_of {
        let (action, bytes, old_targets) = undoable(tx, id, now)?;
        let Some(Inverse::Covers(old)) = Inverse::decode(&bytes) else {
            return Err(LibraryError::Corrupt("cover inverse".into()));
        };
        for target in &mut targets {
            let old = old_targets
                .iter()
                .find(|t| t.id == target.id && t.kind == Kind::Cover)
                .ok_or_else(|| LibraryError::Corrupt("cover target".into()))?;
            target.state_after = old.state_before;
        }
        let written: HashSet<_> = files.iter().map(|f| f.track).collect();
        let left: Vec<_> = old
            .into_iter()
            .filter(|f| !written.contains(&f.track))
            .collect();
        let (kind, summary) = undo_summary(&action);
        let undo = insert(
            tx,
            meta,
            NewAction {
                kind,
                summary,
                inverse: Inverse::Covers(files),
                targets,
            },
            Some(id),
            !action.applies,
            now,
        )?;
        if !left.is_empty() {
            tx.execute(
                "UPDATE actions SET undone_by=NULL, inverse=?2 WHERE id=?1",
                params![id.0, Inverse::Covers(left).encode()],
            )?;
        }
        undo
    } else {
        record(
            tx,
            meta,
            NewAction {
                kind: "cover_edit",
                summary: format!(
                    "{operation} embedded front covers of {count} files (MP4: all covr items)"
                ),
                inverse: Inverse::Covers(files),
                targets,
            },
            now,
        )?
    };
    if undo_of.is_none() {
        let details = serde_json::json!({
        "v":1,
        "cover_group":group,
        "details":{
        "type":"cover_edit",
        "operation":operation,
        "scope":"front_cover_or_all_covr",
        "file_count":count}
        }
        );

        tx.execute(
            "UPDATE actions SET details_json=?2 WHERE id=?1",
            params![id.0, details.to_string()],
        )?;
    }
    if undo_of.is_some() {
        tx.execute(UPDATE_ACTIONS, params![id.0, group])?;
    }
    if let Some(existing) = existing {
        let retained: i64 = tx.query_row(
            "SELECT coalesce(sum(length(inverse)),0) FROM actions WHERE id IN (?1,?2)",
            params![existing, id.0],
            |r| r.get(0),
        )?;
        if retained < 0 || retained as u64 > crate::MAX_COVER_UNDO_BYTES as u64 {
            return Err(LibraryError::CoverInput("undo_too_large"));
        }
        let old: Vec<u8> =
            tx.query_row("SELECT inverse FROM actions WHERE id=?1", [existing], |r| {
                r.get(0)
            })?;
        let new: Vec<u8> =
            tx.query_row("SELECT inverse FROM actions WHERE id=?1", [id.0], |r| {
                r.get(0)
            })?;
        let (Some(Inverse::Covers(mut old)), Some(Inverse::Covers(new))) =
            (Inverse::decode(&old), Inverse::decode(&new))
        else {
            return Err(LibraryError::Corrupt("cover group inverse".into()));
        };
        old.extend(new);
        let count = old.len();
        tx.execute(
            "UPDATE action_targets SET action_id=?2 WHERE action_id=?1",
            params![id.0, existing],
        )?;
        tx.execute(
            "UPDATE actions SET undone_by=?2 WHERE undone_by=?1",
            params![id.0, existing],
        )?;
        let summary =
            format!("{operation} embedded front covers of {count} files (MP4: all covr items)");
        tx.execute(
            UPDATE_ACTIONS_2,
            params![
                existing,
                Inverse::Covers(old).encode(),
                count as i64,
                summary
            ],
        )?;
        tx.execute("DELETE FROM actions WHERE id=?1", [id.0])?;
        return Ok(ActionId(existing));
    }
    Ok(id)
}

/// A cover write changes file stamps but preserves normal text-key inverse values.
/// Exact inverses become usable again when a cover undo restores their expected digest.
pub(crate) fn cover_restamp(
    tx: &Connection,
    track: i64,
    before_stamp: (u64, i64),
    before_picture: Option<[u8; 32]>,
    (size, mtime): (u64, i64),
    guard: [u8; 32],
    digest: &str,
) -> Result<()> {
    const SELECT_ACTIONS: &str = r#"
SELECT a.id FROM actions a JOIN action_targets t ON t.action_id=a.id
WHERE t.kind='track_tags' AND t.object_id=?1 AND a.inverse IS NOT NULL
AND a.undone_by IS NULL AND (coalesce(json_type(a.details_json,'$.tag_digests'),'null')='null'
OR json_extract(a.details_json,'$.tag_digests.' || t.object_id)=?2)
"#;

    let ids: Vec<i64> = tx
        .prepare(SELECT_ACTIONS)?
        .query_map(params![track, digest], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in ids {
        let bytes: Vec<u8> =
            tx.query_row("SELECT inverse FROM actions WHERE id=?1", [id], |r| {
                r.get(0)
            })?;
        let Some(Inverse::Tags(mut files)) = Inverse::decode(&bytes) else {
            continue;
        };
        let mut changed = false;
        for f in files.iter_mut().filter(|f| {
            f.track == track
                && match &f.exact {
                    Some(exact) => crate::tags::edit::snapshot::hex(exact.after) == digest,
                    None => {
                        f.raw_id3.is_none()
                            && (f.size, f.mtime_ms) == before_stamp
                            && f.protected_hash.is_none_or(|h| Some(h) == before_picture)
                    }
                }
        }) {
            if (f.size, f.mtime_ms) != (size, mtime)
                || f.exact.is_none() && f.protected_hash != Some(guard)
            {
                f.size = size;
                f.mtime_ms = mtime;
                if f.exact.is_none() {
                    f.protected_hash = Some(guard);
                }
                changed = true;
            }
        }
        if changed {
            tx.execute(
                "UPDATE actions SET inverse=?2 WHERE id=?1",
                params![id, Inverse::Tags(files).encode()],
            )?;
        }
    }
    Ok(())
}
