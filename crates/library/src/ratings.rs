//! Ratings and loved flags (`track_user`), recorded in the action journal. Runs on the
//! writer thread.
//!
//! Every change bumps the track's `user_version` (one counter for both fields), which the
//! journal checks before an undo. Rows are never deleted, so a version never goes back.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};

use crate::journal::{self, ActionMeta, Done, Inverse, Kind, NewAction, Target};
use crate::types::{TrackId, file_stem};
use crate::{Result, db::writer::now_ms};

/// A user-data field and the value to give it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    /// 1–5 stars, or unrated.
    Rating(Option<u8>),
    Loved(bool),
}

impl Field {
    fn column(self) -> &'static str {
        match self {
            Field::Rating(_) => "rating",
            Field::Loved(_) => "loved",
        }
    }

    fn value(self) -> Option<i64> {
        match self {
            Field::Rating(rating) => rating.map(i64::from),
            Field::Loved(loved) => Some(i64::from(loved)),
        }
    }
}

/// The track's current value of `field` and its user version; `None` for an unknown track.
fn current(conn: &Connection, id: i64, field: Field) -> Result<Option<(Option<i64>, i64)>> {
    let sql = match field {
        Field::Rating(_) => {
            "SELECT u.rating, COALESCE(u.user_version, 0)
             FROM tracks t LEFT JOIN track_user u ON u.track_id = t.id WHERE t.id = ?1"
        }
        Field::Loved(_) => {
            "SELECT COALESCE(u.loved, 0), COALESCE(u.user_version, 0)
             FROM tracks t LEFT JOIN track_user u ON u.track_id = t.id WHERE t.id = ?1"
        }
    };
    Ok(conn
        .prepare_cached(sql)?
        .query_row([id], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?)
}

/// Writes `field` and the new user version of a track.
fn write(conn: &Connection, id: i64, field: Field, version: i64) -> Result<()> {
    let column = field.column();
    conn.prepare_cached(&format!(
        "INSERT INTO track_user (track_id, {column}, user_version) VALUES (?1, ?2, ?3)
         ON CONFLICT (track_id) DO UPDATE
         SET {column} = excluded.{column}, user_version = excluded.user_version"
    ))?
    .execute(params![id, field.value(), version])?;
    Ok(())
}

/// Sets `field` on tracks (unknown ids and tracks that already hold the value are left
/// out) and records the action: the tracks that changed, in request order.
pub(crate) fn set(
    conn: &mut Connection,
    ids: &[TrackId],
    field: Field,
    meta: ActionMeta<'_>,
) -> Result<Done<Vec<TrackId>>> {
    let now = now_ms();
    let tx = conn.transaction()?;
    let mut seen = HashSet::with_capacity(ids.len());
    let mut changed = Vec::new();
    let mut previous = Vec::new();
    let mut targets = Vec::new();
    for id in ids {
        if !seen.insert(id.0) {
            continue;
        }
        let Some((value, version)) = current(&tx, id.0, field)? else {
            continue;
        };
        if value == field.value() {
            continue;
        }
        let state_before = journal::state_of(&tx, Kind::Track, id.0, version)?;
        write(&tx, id.0, field, version + 1)?;
        changed.push(*id);
        previous.push((id.0, value));
        targets.push(Target {
            kind: Kind::Track,
            id: id.0,
            version_after: version + 1,
            state_before,
            state_after: version + 1,
        });
    }
    if changed.is_empty() {
        return Ok(Done::unchanged(changed));
    }
    let what = match changed.as_slice() {
        [one] => format!("\"{}\"", title_of(&tx, *one)?),
        _ => journal::count(changed.len(), "track"),
    };
    let (kind, summary, inverse) = match field {
        Field::Rating(rating) => (
            "set_rating",
            match rating {
                Some(stars) => format!("rate {what} {}", journal::count(stars.into(), "star")),
                None => format!("clear the rating of {what}"),
            },
            Inverse::Ratings(
                previous
                    .into_iter()
                    .map(|(id, r)| (id, r.and_then(|r| u8::try_from(r).ok())))
                    .collect(),
            ),
        ),
        Field::Loved(loved) => (
            "set_loved",
            format!("{} {what}", if loved { "love" } else { "unlove" }),
            Inverse::Loved(
                previous
                    .into_iter()
                    .map(|(id, l)| (id, l.unwrap_or(0) != 0))
                    .collect(),
            ),
        ),
    };
    let action = journal::record(
        &tx,
        meta,
        NewAction {
            kind,
            summary,
            inverse,
            targets,
        },
        now,
    )?;
    tx.commit()?;
    Ok(Done {
        value: changed,
        action: Some(action),
    })
}

fn title_of(conn: &Connection, id: TrackId) -> Result<String> {
    let (title, rel_path): (Option<String>, String) = conn
        .prepare_cached("SELECT title, rel_path FROM tracks WHERE id = ?1")?
        .query_row([id.0], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(title.unwrap_or_else(|| file_stem(&rel_path).to_owned()))
}

/// Sets each track's rating (an undo; the journal bumps the versions): the previous
/// ratings.
pub(crate) fn apply_ratings(
    conn: &Connection,
    values: &[(i64, Option<u8>)],
) -> Result<Vec<(i64, Option<u8>)>> {
    let mut previous = Vec::with_capacity(values.len());
    for &(id, rating) in values {
        let field = Field::Rating(rating);
        let Some((value, version)) = current(conn, id, field)? else {
            continue;
        };
        write(conn, id, field, version)?;
        previous.push((id, value.and_then(|r| u8::try_from(r).ok())));
    }
    Ok(previous)
}

/// Sets each track's loved flag (an undo): the previous flags.
pub(crate) fn apply_loved(conn: &Connection, values: &[(i64, bool)]) -> Result<Vec<(i64, bool)>> {
    let mut previous = Vec::with_capacity(values.len());
    for &(id, loved) in values {
        let field = Field::Loved(loved);
        let Some((value, version)) = current(conn, id, field)? else {
            continue;
        };
        write(conn, id, field, version)?;
        previous.push((id, value.unwrap_or(0) != 0));
    }
    Ok(previous)
}
