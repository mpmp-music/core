//! Play statistics from the raw play history (docs/design/queries-playlists-undo.md).
//!
//! Raw events are kept as recorded; `track_stats` is derived from them with the rule below,
//! updated incrementally on every event and rebuilt in full when the rule changes.

use std::collections::HashMap;

use rusqlite::{Connection, params};

use crate::Result;
use crate::types::EndedReason;

/// Last.fm's cap on the listening time a play needs: 4 minutes.
pub(crate) const PLAY_CAP_MS: u64 = 4 * 60 * 1000;

/// How one play event counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Counted {
    /// A play: at least `min(50% of the duration, 4 min)` heard, however the play ended
    /// (Last.fm). With an unknown duration, 4 minutes. Finishing the track is not enough on
    /// its own: seeking near the end and letting it finish does not count.
    pub play: bool,
    /// A skip: ended as skipped with less than 50% heard (any time, if the duration is
    /// unknown), and not already a play: a long track skipped after 4 minutes is a play.
    pub skip: bool,
}

/// `played_ms` is the time actually heard (core's adapter sums the listening time; it is not
/// the end position).
pub(crate) fn classify(ended: EndedReason, played_ms: u64, duration_ms: Option<u64>) -> Counted {
    let duration = duration_ms.filter(|&d| d > 0);
    let threshold = duration.map_or(PLAY_CAP_MS, |d| (d / 2).min(PLAY_CAP_MS));
    let play = played_ms >= threshold;
    let under_half = duration.is_none_or(|d| played_ms.saturating_mul(2) < d);
    let skip = ended == EndedReason::Skipped && under_half && !play;
    Counted { play, skip }
}

/// Adds one event to `track_stats`.
pub(crate) fn apply_event(
    conn: &Connection,
    track_id: i64,
    started_at: i64,
    counted: Counted,
) -> rusqlite::Result<()> {
    if !counted.play && !counted.skip {
        return Ok(());
    }
    let played_at = counted.play.then_some(started_at);
    conn.prepare_cached(
        "INSERT INTO track_stats (track_id, play_count, skip_count, first_played_at, last_played_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT (track_id) DO UPDATE SET
            play_count = play_count + excluded.play_count,
            skip_count = skip_count + excluded.skip_count,
            first_played_at = CASE
                WHEN excluded.first_played_at IS NULL THEN first_played_at
                WHEN first_played_at IS NULL THEN excluded.first_played_at
                ELSE MIN(first_played_at, excluded.first_played_at) END,
            last_played_at = CASE
                WHEN excluded.last_played_at IS NULL THEN last_played_at
                WHEN last_played_at IS NULL THEN excluded.last_played_at
                ELSE MAX(last_played_at, excluded.last_played_at) END",
    )?
    .execute(params![
        track_id,
        i64::from(counted.play),
        i64::from(counted.skip),
        played_at
    ])?;
    Ok(())
}

/// Recomputes `track_stats` from every event in `play_events`, with current durations.
pub(crate) fn rebuild(conn: &Connection) -> Result<()> {
    #[derive(Default)]
    struct Acc {
        plays: i64,
        skips: i64,
        first: Option<i64>,
        last: Option<i64>,
    }
    let mut acc: HashMap<i64, Acc> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT e.track_id, e.started_at, e.played_ms, e.ended_reason, t.duration_ms
             FROM play_events e LEFT JOIN tracks t ON t.id = e.track_id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let track_id: i64 = row.get(0)?;
            let started_at: i64 = row.get(1)?;
            let played_ms = row.get::<_, i64>(2)?.max(0) as u64;
            let ended = EndedReason::parse(&row.get::<_, String>(3)?);
            let duration = row.get::<_, Option<i64>>(4)?.map(|d| d.max(0) as u64);
            let counted = classify(ended, played_ms, duration);
            let entry = acc.entry(track_id).or_default();
            if counted.play {
                entry.plays += 1;
                entry.first = Some(entry.first.map_or(started_at, |f| f.min(started_at)));
                entry.last = Some(entry.last.map_or(started_at, |l| l.max(started_at)));
            }
            if counted.skip {
                entry.skips += 1;
            }
        }
    }
    conn.execute("DELETE FROM track_stats", [])?;
    let mut insert = conn.prepare(
        "INSERT INTO track_stats (track_id, play_count, skip_count, first_played_at, last_played_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (track_id, a) in acc {
        if a.plays > 0 || a.skips > 0 {
            insert.execute(params![track_id, a.plays, a.skips, a.first, a.last])?;
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::*;
    use EndedReason::*;

    fn c(ended: EndedReason, played_s: u64, duration_s: Option<u64>) -> (bool, bool) {
        let r = classify(ended, played_s * 1000, duration_s.map(|d| d * 1000));
        (r.play, r.skip)
    }

    #[test]
    fn last_fm_rule() {
        // Completing is not enough: seeking near the end and letting the track finish
        // does not count; only the time heard does.
        assert_eq!(c(Completed, 1, Some(200)), (false, false));
        assert_eq!(c(Completed, 99, Some(200)), (false, false));
        assert_eq!(c(Completed, 100, Some(200)), (true, false));
        assert_eq!(c(Completed, 239, None), (false, false));
        // Half of a short track.
        assert_eq!(c(Stopped, 100, Some(200)), (true, false));
        assert_eq!(c(Stopped, 99, Some(200)), (false, false));
        // 4 minutes of a long track, though less than half.
        assert_eq!(c(Stopped, 240, Some(1200)), (true, false));
        assert_eq!(c(Stopped, 239, Some(1200)), (false, false));
        // Unknown duration: 4 minutes.
        assert_eq!(c(Stopped, 240, None), (true, false));
        assert_eq!(c(Stopped, 239, None), (false, false));
        assert_eq!(c(Stopped, 239, Some(0)), (false, false));
    }

    #[test]
    fn skips() {
        assert_eq!(c(Skipped, 10, Some(200)), (false, true));
        assert_eq!(c(Skipped, 99, Some(200)), (false, true));
        // Half heard: a play, not a skip.
        assert_eq!(c(Skipped, 100, Some(200)), (true, false));
        // Long track skipped after 4 minutes: a play (exclusive with skip).
        assert_eq!(c(Skipped, 300, Some(1200)), (true, false));
        assert_eq!(c(Skipped, 10, None), (false, true));
        // Failed and stopped plays are never skips.
        assert_eq!(c(Failed, 1, Some(200)), (false, false));
        assert_eq!(c(Stopped, 1, Some(200)), (false, false));
    }
}
