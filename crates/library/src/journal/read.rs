use super::*;

// ---- reads ---

const SUMMARY_COLUMNS: &str = "id, at_ms, origin, kind, summary, target_count, undone_by, undo_of,
     inverse IS NOT NULL, grant_id, details_json";

fn summary_from_row(row: &rusqlite::Row<'_>, now: i64) -> rusqlite::Result<ActionSummary> {
    let at_ms: i64 = row.get(1)?;
    let undone_by: Option<i64> = row.get(6)?;
    let has_inverse: bool = row.get(8)?;
    Ok(ActionSummary {
        id: ActionId(row.get(0)?),
        at_ms,
        origin: row.get(2)?,
        kind: row.get(3)?,
        summary: row.get(4)?,
        target_count: row.get::<_, i64>(5)?.max(0) as u64,
        undone_by: undone_by.map(ActionId),
        undo_of: row.get::<_, Option<i64>>(7)?.map(ActionId),
        undoable: has_inverse
            && undone_by.is_none()
            && at_ms >= now.saturating_sub(ACTION_RETENTION_MS),
        grant: row.get(9)?,
        details: row.get::<_, Option<String>>(10)?.and_then(|text| {
            let value: serde_json::Value = serde_json::from_str(&text).ok()?;
            if value["v"] != 1 {
                return None;
            }
            let mut details = value.get("details")?.clone();
            // The unfrozen minor-2 writer used operation instead of type. Both
            // are version-1 facts; normalize only that legacy tag-key spelling.
            if details["type"] == "tag_edit"
                && let Some(changes) = details["changes"].as_array_mut()
            {
                for change in changes {
                    if let Some(fields) = change.as_object_mut()
                        && !fields.contains_key("type")
                        && let Some(operation) = fields.remove("operation")
                    {
                        fields.insert("type".into(), operation);
                    }
                }
            }
            Some(details)
        }),
    })
}

/// A page of actions, newest first, and how many match, from one snapshot. `origin`
/// matches exactly, or, ending in `:`, as a prefix (`"agent:"`: every agent).
pub(crate) fn list(
    conn: &Connection,
    origin: Option<&str>,
    page: Page,
    now: i64,
) -> Result<Paged<ActionSummary>> {
    const FILTER: &str = "?1 IS NULL OR origin = ?1
        OR (substr(?1, -1) = ':' AND substr(origin, 1, length(?1)) = ?1)";
    let items = conn
        .prepare_cached(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM actions WHERE {FILTER}
             ORDER BY id DESC LIMIT ?2 OFFSET ?3"
        ))?
        .query_map(params![origin, page.limit, page.offset], |row| {
            summary_from_row(row, now)
        })?
        .collect::<rusqlite::Result<_>>()?;
    let total: i64 = conn
        .prepare_cached(&format!("SELECT COUNT(*) FROM actions WHERE {FILTER}"))?
        .query_row([origin], |row| row.get(0))?;
    Ok(Paged {
        items,
        total: total.max(0) as u64,
    })
}

pub(crate) fn summary(conn: &Connection, id: ActionId, now: i64) -> Result<Option<ActionSummary>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM actions WHERE id = ?1"
        ))?
        .query_row([id.0], |row| summary_from_row(row, now))
        .optional()?)
}

/// `n thing(s)`.
pub(crate) fn count(n: usize, thing: &str) -> String {
    if n == 1 {
        format!("1 {thing}")
    } else {
        format!("{n} {thing}s")
    }
}
