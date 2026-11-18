//! Published schema 1 and guarded recognition of development databases.
use rusqlite::{Connection, OptionalExtension};

mod fingerprint;
pub(super) mod reset;
#[cfg(all(test, feature = "desktop-fs"))]
pub(crate) use reset::migrate;
pub(crate) use reset::{RESET_NOTICE, inspect, notice_backup, notice_lock, prepare, relabel_copy};

pub(crate) const LATEST_VERSION: i64 = 1;
pub(crate) const APPLICATION_ID: i64 = i64::from_be_bytes([0, 0, 0, 0, b'M', b'P', b'M', b'P']);
pub(crate) const SCHEMA_SHA256: &str =
    "a0c2e40b19c08cb291dc067e723e1a5bbc82ae09e068a9aaccc95bcfa3438a26";
const V1_SOURCE: &str = include_str!("schema/v1.sql");

/// The schema 1 DDL with LF line endings. The fingerprint covers the SQL text SQLite
/// stores, so a checkout that converted `v1.sql` to CRLF (Git's `core.autocrlf` on
/// Windows) must still create exactly the published schema.
pub(crate) fn v1() -> std::borrow::Cow<'static, str> {
    lf(V1_SOURCE)
}

fn lf(source: &str) -> std::borrow::Cow<'_, str> {
    if source.contains('\r') {
        std::borrow::Cow::Owned(source.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(source)
    }
}

/// Keeps the query planner's statistics current: a full `ANALYZE` when the library has
/// tracks but no statistics yet (a new or migrated database; without them SQLite picks
/// poor plans at scale), else `PRAGMA optimize`, which re-analyzes tables whose size has
/// changed a lot. Run when the database is opened (after migrations) and after each
/// completed scan, on the writer's connection.
pub(crate) fn optimize(conn: &Connection) -> rusqlite::Result<()> {
    let analyzed: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sqlite_stat1')",
        [],
        |row| row.get(0),
    )? && conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_stat1 WHERE tbl = 'tracks')",
        [],
        |row| row.get(0),
    )?;
    if analyzed {
        conn.execute_batch("PRAGMA optimize = 0x10002")
    } else if conn.query_row("SELECT EXISTS (SELECT 1 FROM tracks)", [], |row| row.get(0))? {
        let started = std::time::Instant::now();
        conn.execute_batch("ANALYZE")?;
        tracing::info!(elapsed = ?started.elapsed(), "library statistics computed");
        Ok(())
    } else {
        Ok(())
    }
}

const RESCAN_KEY: &str = "rescan_recommended";

/// Records that files must be re-read (until the next completed scan). Each request bumps
/// the counter, so a scan clears only the requests it has seen begin.
pub(crate) fn request_rescan(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, 1)
         ON CONFLICT (key) DO UPDATE SET value = value + 1",
        [RESCAN_KEY],
    )
    .map(drop)
}

/// The rescan request counter, if a rescan is recommended.
pub(crate) fn rescan_request(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        [RESCAN_KEY],
        |row| row.get(0),
    )
    .optional()
}

/// Clears the rescan request if it is still the one `seen` when the scan pass began.
pub(crate) fn clear_rescan(conn: &Connection, seen: i64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM meta WHERE key = ?1 AND value = ?2",
        rusqlite::params![RESCAN_KEY, seen],
    )
    .map(drop)
}

/// Recomputes the `albums` and `artists` rows whose keys the triggers marked dirty. Called
/// by the writer at the end of every transaction that changes `tracks`, so readers always
/// see aggregates that match the tracks.
pub(crate) fn refresh_aggregates(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DELETE FROM albums WHERE album_key IN (SELECT album_key FROM dirty_albums);
         INSERT INTO albums
         SELECT album_key, COALESCE(MIN(album), ''), MIN(album_group_artist), MIN(year), COUNT(*),
                COUNT(DISTINCT COALESCE(disc_no, 1)), SUM(COALESCE(duration_ms, 0)),
                COALESCE(MIN(CASE WHEN has_embedded_art THEN id END),
                         MIN(CASE WHEN has_folder_art THEN id END)),
                MIN(added_at),
                substr(album_key, instr(album_key, char(31)) + 1),
                substr(album_key, 1, instr(album_key, char(31)) - 1)
         FROM tracks
         WHERE available = 1 AND album_key IN (SELECT album_key FROM dirty_albums)
         GROUP BY album_key;
         DELETE FROM dirty_albums;

         DELETE FROM artists WHERE artist_norm IN (SELECT artist_norm FROM dirty_artists);
         INSERT INTO artists
         SELECT group_artist_norm, MIN(album_group_artist), COUNT(DISTINCT album_key), COUNT(*)
         FROM tracks
         WHERE available = 1 AND group_artist_norm <> ''
           AND group_artist_norm IN (SELECT artist_norm FROM dirty_artists)
         GROUP BY group_artist_norm;
         DELETE FROM dirty_artists;",
    )
}

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "desktop-fs"))]
mod crash_tests;
