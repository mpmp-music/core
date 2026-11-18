//! Read queries, run on pooled read-only connections.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, Row, ToSql, params_from_iter};

use crate::Result;
use crate::text::{SearchQuery, normalize};
use crate::track_query::TrackRow;
use crate::types::{
    AlbumKey, AlbumSort, AlbumSummary, ArtistSummary, CueTrack, EndedReason, HistoryEntry, Page,
    Paged, PlayEvent, PlayEventId, ReplayGain, Root, RootId, Track, TrackId,
};

/// A track's gain and peak come as a pair from one source: its measurement when there is
/// one (no gain when it found no integrated loudness: silent or too short), else its
/// tags. Mixing a tag's gain with a measured peak would defeat clipping prevention.
const TRACK_COLUMNS: &str = "t.id, t.root_id, t.rel_path, t.available, t.size, t.mtime_ms,
    t.title, t.artist, t.album, t.album_artist, t.genre, t.year,
    t.track_no, t.track_total, t.disc_no, t.disc_total, t.compilation, t.album_key,
    t.duration_ms, t.sample_rate, t.channels, t.bits_per_sample, t.bitrate_kbps, t.codec,
    t.lossless,
    CASE WHEN EXISTS(SELECT 1 FROM track_loudness l WHERE l.track_id=t.id)
        THEN (SELECT -18.0 - l.integrated_lufs FROM track_loudness l WHERE l.track_id=t.id)
        ELSE t.rg_track_gain END,
    CASE WHEN EXISTS(SELECT 1 FROM track_loudness l WHERE l.track_id=t.id)
        THEN (SELECT l.sample_peak FROM track_loudness l WHERE l.track_id=t.id)
        ELSE t.rg_track_peak END,
    COALESCE((SELECT -18.0 - l.integrated_lufs FROM album_loudness l WHERE l.album_key=t.album_key), t.rg_album_gain),
    COALESCE((SELECT l.sample_peak FROM album_loudness l WHERE l.album_key=t.album_key), t.rg_album_peak),
    t.has_embedded_art, t.tag_encoding, t.added_at, t.updated_at, t.has_folder_art,
    t.cue_sheet, t.cue_number, t.cue_start, t.cue_end, t.album_group_artist,
    EXISTS(SELECT 1 FROM track_loudness l WHERE l.track_id=t.id),
    EXISTS(SELECT 1 FROM album_loudness l WHERE l.album_key=t.album_key AND l.integrated_lufs IS NOT NULL), t.artists_source, t.artists_parser_version, t.cover_revision"
;

fn track_from_row(row: &Row<'_>) -> rusqlite::Result<Track> {
    let source = row
        .get::<_, Option<String>>(41)?
        .and_then(|s| {
            serde_json::from_value::<crate::ArtistListSource>(serde_json::Value::String(s)).ok()
        })
        .filter(|s| *s != crate::ArtistListSource::Unknown);
    Ok(Track {
        id: TrackId(row.get(0)?),
        root_id: RootId(row.get(1)?),
        rel_path: row.get(2)?,
        available: row.get(3)?,
        size: row.get::<_, i64>(4)? as u64,
        mtime_ms: row.get(5)?,
        title: row.get(6)?,
        artist: row.get(7)?,
        artists: Vec::new(),
        artists_source: source,
        artists_complete: source.is_some()
            && row.get::<_, i64>(42)? >= crate::person::PARSER_VERSION,
        cover_revision: row.get(43)?,
        album: row.get(8)?,
        album_artist: row.get(9)?,
        group_artist: row.get(38)?,
        genre: row.get(10)?,
        year: row.get(11)?,
        track_no: row.get(12)?,
        track_total: row.get(13)?,
        disc_no: row.get(14)?,
        disc_total: row.get(15)?,
        compilation: row.get(16)?,
        album_key: AlbumKey(row.get(17)?),
        duration_ms: row.get::<_, Option<i64>>(18)?.map(|d| d as u64),
        sample_rate: row.get(19)?,
        channels: row.get(20)?,
        bits_per_sample: row.get(21)?,
        bitrate_kbps: row.get(22)?,
        codec: row.get(23)?,
        lossless: row.get(24)?,
        replay_gain: ReplayGain {
            track_gain_db: row.get(25)?,
            track_peak: row.get(26)?,
            album_gain_db: row.get(27)?,
            album_peak: row.get(28)?,
            track_measured: row.get(39)?,
            album_measured: row.get(40)?,
        },
        has_embedded_art: row.get(29)?,
        tag_encoding: row.get(30)?,
        added_at: row.get(31)?,
        updated_at: row.get(32)?,
        has_folder_art: row.get(33)?,
        cue: match row.get::<_, Option<String>>(34)? {
            Some(sheet) => Some(CueTrack {
                sheet,
                number: row.get::<_, Option<u32>>(35)?.unwrap_or(0),
                start: row.get::<_, Option<i64>>(36)?.unwrap_or(0).max(0) as u64,
                end: row.get::<_, Option<i64>>(37)?.map(|e| e.max(0) as u64),
            }),
            None => None,
        },
    })
}

const ALBUM_COLUMNS: &str = "album_key, title, artist, year, track_count, disc_count,
    duration_ms, art_track, added_at";

fn album_from_row(row: &Row<'_>) -> rusqlite::Result<AlbumSummary> {
    Ok(AlbumSummary {
        key: AlbumKey(row.get(0)?),
        title: row.get(1)?,
        artist: row.get(2)?,
        year: row.get(3)?,
        track_count: row.get(4)?,
        disc_count: row.get(5)?,
        duration_ms: row.get::<_, i64>(6)? as u64,
        art_track: row.get::<_, Option<i64>>(7)?.map(TrackId),
        added_at: row.get(8)?,
    })
}

const ARTIST_COLUMNS: &str = "name, album_count, track_count";

fn artist_from_row(row: &Row<'_>) -> rusqlite::Result<ArtistSummary> {
    Ok(ArtistSummary {
        key: None,
        name: row.get(0)?,
        album_count: row.get(1)?,
        track_count: row.get(2)?,
    })
}

pub(crate) fn roots(conn: &Connection) -> Result<Vec<Root>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, path, forced_encoding, added_at, last_scan_at,
            (SELECT COUNT(*) FROM tracks t WHERE t.root_id = r.id AND t.available = 1)
         FROM roots r WHERE removed = 0 ORDER BY id",
    )?;
    let roots = stmt
        .query_map([], |row| {
            Ok(Root {
                id: RootId(row.get(0)?),
                path: PathBuf::from(row.get::<_, String>(1)?),
                forced_encoding: row.get(2)?,
                added_at: row.get(3)?,
                last_scan_at: row.get(4)?,
                track_count: row.get::<_, i64>(5)? as u64,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(roots)
}

/// Include soft-removed roots: a delayed playback open still belongs to its original
/// root and must not escape that root's session-local release barrier.
pub(crate) fn root_paths(conn: &Connection) -> Result<Vec<(RootId, PathBuf)>> {
    let mut stmt = conn.prepare_cached("SELECT id, path FROM roots ORDER BY id")?;
    Ok(stmt
        .query_map([], |row| {
            Ok((RootId(row.get(0)?), PathBuf::from(row.get::<_, String>(1)?)))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn root_removed(conn: &Connection, id: RootId) -> Result<Option<bool>> {
    Ok(conn
        .prepare_cached("SELECT removed FROM roots WHERE id = ?1")?
        .query_row([id.0], |row| row.get(0))
        .optional()?)
}

pub(crate) fn track(conn: &Connection, id: TrackId) -> Result<Option<Track>> {
    Ok(tracks(conn, &[id])?.pop())
}

/// Tracks in the order of `ids`; unknown ids are skipped.
pub(crate) fn tracks(conn: &Connection, ids: &[TrackId]) -> Result<Vec<Track>> {
    const CHUNK: usize = 500;
    let mut found = HashMap::with_capacity(ids.len());
    for chunk in ids.chunks(CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!("SELECT {TRACK_COLUMNS} FROM tracks t WHERE t.id IN ({placeholders})");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params_from_iter(chunk.iter().map(|id| id.0)),
            track_from_row,
        )?;
        for track in rows {
            let track = track?;
            found.insert(track.id, track);
        }
    }
    // Batch each unique id once, including duplicate queue entries across chunks.
    let unique: Vec<_> = found.keys().copied().collect();
    for chunk in unique.chunks(CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            concat!(
                "SELECT a.track_id, a.artist_key, a.name FROM track_artists a WHERE ",
                "a.track_id IN ({placeholders}) ORDER BY a.track_id, a.ordinal",
            ),
            placeholders = placeholders,
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(chunk.iter().map(|id| id.0)))?;
        while let Some(row) = rows.next()? {
            if let Some(track) = found.get_mut(&TrackId(row.get(0)?)) {
                track.artists.push(crate::ArtistRef {
                    key: row.get(1)?,
                    name: row.get(2)?,
                });
            }
        }
    }
    Ok(ids.iter().filter_map(|id| found.get(id).cloned()).collect())
}

/// Statistics and user data of one track: plays, skips, last and first played, rating, loved.
type RowExtra = (i64, i64, Option<i64>, Option<i64>, Option<u8>, bool);

/// [`TrackRow`]s for `ids` in that order (unknown ids skipped), with the tags `keys`
/// (upper case).
pub(crate) fn track_rows(
    conn: &Connection,
    ids: &[TrackId],
    keys: &[String],
) -> Result<Vec<TrackRow>> {
    const CHUNK: usize = 500;
    let tracks = tracks(conn, ids)?;
    // Each id once: a repeated id is one set of rows, shared by its occurrences.
    let mut seen = HashSet::with_capacity(tracks.len());
    let unique: Vec<i64> = tracks
        .iter()
        .map(|t| t.id.0)
        .filter(|id| seen.insert(*id))
        .collect();
    let mut extra: HashMap<i64, RowExtra> = HashMap::with_capacity(unique.len());
    let mut tags: HashMap<i64, BTreeMap<String, Vec<String>>> = HashMap::new();
    for chunk in unique.chunks(CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let ids = chunk.iter().copied();
        let sql = format!(
            "SELECT t.id, COALESCE(s.play_count, 0), COALESCE(s.skip_count, 0),
                    s.last_played_at, s.first_played_at, u.rating, COALESCE(u.loved, 0)
             FROM tracks t
             LEFT JOIN track_stats s ON s.track_id = t.id
             LEFT JOIN track_user u ON u.track_id = t.id
             WHERE t.id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(ids.clone()))?;
        while let Some(row) = rows.next()? {
            extra.insert(
                row.get(0)?,
                (
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ),
            );
        }
        if keys.is_empty() {
            continue;
        }
        let key_placeholders = vec!["?"; keys.len()].join(",");
        let sql = format!(
            "SELECT track_id, key, value FROM track_tags
             WHERE track_id IN ({placeholders}) AND key IN ({key_placeholders})
             ORDER BY track_id, rowid"
        );
        let params: Vec<rusqlite::types::Value> = ids
            .map(rusqlite::types::Value::from)
            .chain(keys.iter().map(|k| k.clone().into()))
            .collect();
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(params.iter()))?;
        while let Some(row) = rows.next()? {
            tags.entry(row.get(0)?)
                .or_default()
                .entry(row.get(1)?)
                .or_default()
                .push(row.get(2)?);
        }
    }
    Ok(tracks
        .into_iter()
        .map(|track| {
            let (plays, skips, last, first, rating, loved) =
                extra.get(&track.id.0).copied().unwrap_or_default();
            TrackRow {
                play_count: plays.max(0) as u64,
                skip_count: skips.max(0) as u64,
                last_played_at: last,
                first_played_at: first,
                rating,
                loved,
                tags: tags.get(&track.id.0).cloned().unwrap_or_default(),
                track,
            }
        })
        .collect())
}

/// Root path and relative path of a track.
pub(crate) fn location(conn: &Connection, id: TrackId) -> Result<Option<(PathBuf, String)>> {
    Ok(conn
        .prepare_cached(
            "SELECT r.path, t.rel_path FROM tracks t JOIN roots r ON r.id = t.root_id
             WHERE t.id = ?1",
        )?
        .query_row([id.0], |row| {
            Ok((PathBuf::from(row.get::<_, String>(0)?), row.get(1)?))
        })
        .optional()?)
}

/// What a track of a CUE sheet is cut from, as stored: CD frames and the file's sample
/// rate. `None` for a track that is a whole file (or an unknown one).
pub(crate) fn cue_frames(conn: &Connection, id: TrackId) -> Result<Option<CueFrames>> {
    let row: Option<(Option<i64>, Option<i64>, Option<u32>)> = conn
        .prepare_cached(
            "SELECT cue_start, cue_end, sample_rate FROM tracks
             WHERE id = ?1 AND cue_sheet IS NOT NULL",
        )?
        .query_row([id.0], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .optional()?;
    Ok(row.map(|(start, end, sample_rate)| CueFrames {
        start: start.unwrap_or(0).max(0) as u64,
        end: end.map(|e| e.max(0) as u64),
        sample_rate,
    }))
}

/// See [`cue_frames`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct CueFrames {
    pub start: u64,
    pub end: Option<u64>,
    pub sample_rate: Option<u32>,
}

/// A track's file size and modification time (Unix ms) as last scanned. For a track of a
/// CUE sheet, the later of its audio file's and its sheet's modification times: editing the
/// sheet makes it another version too.
pub(crate) fn file_stamp(conn: &Connection, id: TrackId) -> Result<Option<(u64, i64)>> {
    Ok(conn
        .prepare_cached(
            "SELECT size, MAX(mtime_ms, COALESCE(cue_mtime_ms, mtime_ms)) FROM tracks
             WHERE id = ?1",
        )?
        .query_row([id.0], |row| {
            Ok((row.get::<_, i64>(0)?.max(0) as u64, row.get(1)?))
        })
        .optional()?)
}

/// Rows of the `albums` or `artists` table, optionally only albums of one artist.
pub(crate) fn count(conn: &Connection, table: &str, artist: Option<&str>) -> Result<u64> {
    let n: i64 = match artist {
        None => conn
            .prepare_cached(&format!("SELECT COUNT(*) FROM {table}"))?
            .query_row([], |row| row.get(0))?,
        Some(name) => conn
            .prepare_cached(&format!(
                "SELECT COUNT(*) FROM {table} WHERE artist_norm = ?1"
            ))?
            .query_row([normalize(name)], |row| row.get(0))?,
    };
    Ok(n as u64)
}

/// `ORDER BY` terms; unknown albums (empty title) come after an artist's named albums.
fn album_order(sort: AlbumSort) -> &'static str {
    match sort {
        // Matches the `albums_title` index.
        AlbumSort::Title => "title_norm = '', title_norm, album_key",
        AlbumSort::Artist => "artist_norm, title_norm = '', title_norm, album_key",
        AlbumSort::Year => "year IS NULL, year, artist_norm, title_norm, album_key",
        AlbumSort::TitleDesc => "title_norm = '', title_norm DESC, album_key",
        AlbumSort::ArtistDesc => "artist_norm DESC, title_norm = '', title_norm, album_key",
        AlbumSort::YearDesc => "year IS NULL, year DESC, artist_norm, title_norm, album_key",
        AlbumSort::OldestAdded => "added_at, album_key",
        AlbumSort::RecentlyAdded => "added_at DESC, album_key DESC",
    }
}

pub(crate) fn albums(conn: &Connection, sort: AlbumSort, page: Page) -> Result<Vec<AlbumSummary>> {
    let sql = format!(
        "SELECT {ALBUM_COLUMNS} FROM albums ORDER BY {} LIMIT ?1 OFFSET ?2",
        album_order(sort)
    );
    let albums = conn
        .prepare_cached(&sql)?
        .query_map([page.limit, page.offset], album_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(albums)
}

/// Albums whose album artist normalizes to the same text as `name` (an
/// [`ArtistSummary::name`](crate::ArtistSummary)).
pub(crate) fn albums_by_artist(
    conn: &Connection,
    name: &str,
    sort: AlbumSort,
    page: Page,
) -> Result<Vec<AlbumSummary>> {
    let sql = format!(
        "SELECT {ALBUM_COLUMNS} FROM albums WHERE artist_norm = ?1
         ORDER BY {} LIMIT ?2 OFFSET ?3",
        album_order(sort)
    );
    let albums = conn
        .prepare_cached(&sql)?
        .query_map(
            rusqlite::params![normalize(name), page.limit, page.offset],
            album_from_row,
        )?
        .collect::<rusqlite::Result<_>>()?;
    Ok(albums)
}

pub(crate) fn album_summary(conn: &Connection, key: &AlbumKey) -> Result<Option<AlbumSummary>> {
    let sql = format!("SELECT {ALBUM_COLUMNS} FROM albums WHERE album_key = ?1");
    Ok(conn
        .prepare_cached(&sql)?
        .query_row([key.as_str()], album_from_row)
        .optional()?)
}

pub(crate) fn album_tracks(conn: &Connection, key: &AlbumKey, page: Page) -> Result<Vec<Track>> {
    let sql =
        "SELECT t.id FROM tracks t
         WHERE t.album_key = ?1 AND t.available = 1
         ORDER BY COALESCE(t.disc_no, 1), t.track_no IS NULL, t.track_no, t.rel_path, t.id LIMIT ?2 OFFSET ?3"
;

    let ids: Vec<TrackId> = conn
        .prepare_cached(sql)?
        .query_map(
            rusqlite::params![key.as_str(), page.limit, page.offset],
            |row| Ok(TrackId(row.get(0)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
    tracks(conn, &ids)
}

pub(crate) fn artists(conn: &Connection, page: Page) -> Result<Vec<ArtistSummary>> {
    let sql =
        format!("SELECT {ARTIST_COLUMNS} FROM artists ORDER BY artist_norm LIMIT ?1 OFFSET ?2");
    let artists = conn
        .prepare_cached(&sql)?
        .query_map([page.limit, page.offset], artist_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(artists)
}

/// A page of the history, newest first, and the number of events. Run in a read
/// transaction, so both come from one snapshot.
pub(crate) fn history(conn: &Connection, page: Page) -> Result<Paged<HistoryEntry>> {
    let items = conn
        .prepare_cached(
            "SELECT id, track_id, context, started_at, played_ms, ended_reason, origin
             FROM play_events ORDER BY started_at DESC, id DESC LIMIT ?1 OFFSET ?2",
        )?
        .query_map([page.limit, page.offset], |row| {
            Ok(HistoryEntry {
                id: PlayEventId(row.get(0)?),
                event: PlayEvent {
                    track_id: TrackId(row.get(1)?),
                    context: row.get(2)?,
                    started_at: row.get(3)?,
                    played_ms: row.get::<_, i64>(4)?.max(0) as u64,
                    ended_reason: EndedReason::parse(&row.get::<_, String>(5)?),
                    origin: row.get(6)?,
                },
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let total: i64 = conn
        .prepare_cached("SELECT COUNT(*) FROM play_events")?
        .query_row([], |row| row.get(0))?;
    Ok(Paged {
        items,
        total: total.max(0) as u64,
    })
}

pub(crate) const TRACK_SEARCH_COLUMNS: [&str; 5] = [
    "title_norm",
    "artist_norm",
    "album_norm",
    "album_artist_norm",
    "genre_norm",
];

/// SQL condition: every term (parameters `first..first + terms`) occurs in one of `columns`.
/// `instr` on normalized text is the `LIKE '%term%'` fallback without the wildcard
/// escaping and ASCII case folding, both unnecessary here.
fn contains_all(columns: &[&str], first: usize, terms: usize) -> String {
    let mut conditions = Vec::with_capacity(terms);
    for n in first..first + terms {
        let any: Vec<String> = columns
            .iter()
            .map(|column| format!("instr({column}, ?{n}) > 0"))
            .collect();
        conditions.push(format!("({})", any.join(" OR ")));
    }
    if conditions.is_empty() {
        "1".to_owned()
    } else {
        conditions.join(" AND ")
    }
}

fn query_ids(conn: &Connection, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<i64>> {
    let ids = conn
        .prepare_cached(sql)?
        .query_map(params, |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Relevance order is total: title matches first, then score, then immutable track ID.
pub(crate) fn search_tracks(
    conn: &Connection,
    query: &SearchQuery,
    page: Page,
) -> Result<Vec<Track>> {
    let short: Vec<&dyn ToSql> = query.like_terms.iter().map(|t| t as &dyn ToSql).collect();
    let limit = i64::from(page.limit);
    let offset = i64::from(page.offset);
    let ids = match query.fts_expression(&[]) {
        None => {
            let in_title = contains_all(&["title_norm"], 1, short.len());
            let anywhere = contains_all(&TRACK_SEARCH_COLUMNS, 1, short.len());
            let n = short.len() + 1;
            let mut params = short;
            params.extend([&limit as &dyn ToSql, &offset]);
            query_ids(
                conn,
                &format!(
                    concat!(
                        "SELECT id FROM tracks INDEXED BY tracks_search WHERE available=1 AND ",
                        "{anywhere} ORDER BY NOT ({in_title}), title_norm, id LIMIT ?{n} OFFSET ?{}",
                    ),
                    n + 1,
                    anywhere = anywhere,
                    in_title = in_title,
                    n = n,
                ),
                &params,
            )?
        }
        Some(anywhere_match) => {
            let title_match = query.fts_expression(&["title_norm"]).unwrap_or_default();
            let columns = TRACK_SEARCH_COLUMNS.map(|c| format!("t.{c}"));
            let columns: Vec<&str> = columns.iter().map(String::as_str).collect();
            let anywhere = contains_all(&columns, 3, short.len());
            let in_title = contains_all(&["t.title_norm"], 3, short.len());
            let n = short.len() + 3;
            let mut params: Vec<&dyn ToSql> = vec![&anywhere_match, &title_match];
            params.extend(short);
            params.extend([&limit as &dyn ToSql, &offset]);
            query_ids(
                conn,
                &format!(
                    concat!(
                        "WITH title_hits AS MATERIALIZED (SELECT rowid AS id, rank AS score FROM ",
                        "tracks_fts WHERE tracks_fts MATCH ?2) SELECT t.id FROM tracks_fts f JOIN ",
                        "tracks t ON t.id=f.rowid LEFT JOIN title_hits h ON h.id=t.id WHERE ",
                        "tracks_fts MATCH ?1 AND t.available=1 AND {anywhere} ORDER BY NOT (h.id ",
                        "IS NOT NULL AND {in_title}), CASE WHEN h.id IS NOT NULL AND {in_title} ",
                        "THEN h.score ELSE f.rank END, t.id LIMIT ?{n} OFFSET ?{}",
                    ),
                    n + 1,
                    anywhere = anywhere,
                    in_title = in_title,
                    n = n,
                ),
                &params,
            )?
        }
    };
    tracks(conn, &ids.into_iter().map(TrackId).collect::<Vec<_>>())
}

/// How many available tracks, albums and artists match a search, counted the way
/// [`search_tracks`], [`search_albums`] and [`search_artists`] match.
pub(crate) fn search_totals(conn: &Connection, query: &SearchQuery) -> Result<(u64, u64, u64)> {
    let short: Vec<&dyn ToSql> = query.like_terms.iter().map(|t| t as &dyn ToSql).collect();
    let count = |sql: &str, params: &[&dyn ToSql]| -> Result<u64> {
        let n: i64 = conn
            .prepare_cached(sql)?
            .query_row(params, |row| row.get(0))?;
        Ok(n.max(0) as u64)
    };
    let tracks = match query.fts_expression(&[]) {
        None => {
            let anywhere = contains_all(&TRACK_SEARCH_COLUMNS, 1, short.len());
            count(
                &format!(
                    "SELECT COUNT(*) FROM tracks INDEXED BY tracks_search
                     WHERE available = 1 AND {anywhere}"
                ),
                &short,
            )?
        }
        Some(anywhere_match) => {
            let columns = TRACK_SEARCH_COLUMNS.map(|c| format!("t.{c}"));
            let columns: Vec<&str> = columns.iter().map(String::as_str).collect();
            let anywhere = contains_all(&columns, 2, short.len());
            let mut params: Vec<&dyn ToSql> = vec![&anywhere_match];
            params.extend(short.iter().copied());
            count(
                &format!(
                    "SELECT COUNT(*) FROM tracks_fts f JOIN tracks t ON t.id = f.rowid
                     WHERE tracks_fts MATCH ?1 AND t.available = 1 AND {anywhere}"
                ),
                &params,
            )?
        }
    };
    let terms = query.all_terms();
    let params: Vec<&dyn ToSql> = terms.iter().map(|t| t as &dyn ToSql).collect();
    let in_albums = contains_all(&["title_norm", "artist_norm"], 1, terms.len());
    let albums = count(
        &format!("SELECT COUNT(*) FROM albums WHERE {in_albums}"),
        &params,
    )?;
    let in_artists = contains_all(&["artist_norm"], 1, terms.len());
    let artists = count(
        &format!("SELECT COUNT(*) FROM artists WHERE {in_artists}"),
        &params,
    )?;
    Ok((tracks, albums, artists))
}

/// Albums whose title or album artist contain every term. The albums table is small (one
/// row per album), so a scan is cheap and needs no trigram index.
pub(crate) fn search_albums(
    conn: &Connection,
    query: &SearchQuery,
    page: Page,
) -> Result<Vec<AlbumSummary>> {
    let terms = query.all_terms();
    let condition = contains_all(&["title_norm", "artist_norm"], 1, terms.len());
    let in_title = contains_all(&["title_norm"], 1, terms.len());
    let limit_param = terms.len() + 1;
    let sql = format!(
        "SELECT {ALBUM_COLUMNS} FROM albums WHERE {condition}
         ORDER BY NOT ({in_title}), title_norm, album_key LIMIT ?{limit_param} OFFSET ?{}",
        limit_param + 1
    );
    let limit = i64::from(page.limit);
    let offset = i64::from(page.offset);
    let mut params: Vec<&dyn ToSql> = terms.iter().map(|t| t as &dyn ToSql).collect();
    params.push(&limit);
    params.push(&offset);
    let albums = conn
        .prepare_cached(&sql)?
        .query_map(params.as_slice(), album_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(albums)
}

/// Album artists (the [`crate::ArtistSummary`] grouping) whose name contains every term,
/// most tracks first.
pub(crate) fn search_artists(
    conn: &Connection,
    query: &SearchQuery,
    page: Page,
) -> Result<Vec<ArtistSummary>> {
    let terms = query.all_terms();
    let condition = contains_all(&["artist_norm"], 1, terms.len());
    let limit_param = terms.len() + 1;
    let sql = format!(
        "SELECT {ARTIST_COLUMNS} FROM artists WHERE {condition}
         ORDER BY track_count DESC, artist_norm LIMIT ?{limit_param} OFFSET ?{}",
        limit_param + 1
    );
    let limit = i64::from(page.limit);
    let offset = i64::from(page.offset);
    let mut params: Vec<&dyn ToSql> = terms.iter().map(|t| t as &dyn ToSql).collect();
    params.push(&limit);
    params.push(&offset);
    let artists = conn
        .prepare_cached(&sql)?
        .query_map(params.as_slice(), artist_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(artists)
}

/// What the database knows about a file, for incremental scans.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KnownFile {
    pub revision: i64,
    pub id: TrackId,
    pub size: u64,
    pub mtime_ms: i64,
    pub available: bool,
    pub has_folder_art: bool,
    /// A sheet cuts the file into tracks (`tracks.hidden`).
    pub hidden: bool,
    pub artists_complete: bool,
}

/// What the database knows about a track of a CUE sheet, for incremental scans.
#[derive(Debug, Clone)]
pub(crate) struct KnownCue {
    pub revision: i64,
    pub artists_complete: bool,
    pub id: TrackId,
    /// Its audio file as last scanned.
    pub rel_path: String,
    pub size: u64,
    pub mtime_ms: i64,
    /// Its sheet file as last scanned (the audio file, for an embedded sheet).
    pub cue_size: u64,
    pub cue_mtime_ms: i64,
    pub available: bool,
    pub has_folder_art: bool,
}

/// Every row of a root: files by `rel_path`, tracks of CUE sheets by `(sheet, number)`.
#[derive(Debug, Default)]
pub(crate) struct KnownRows {
    pub files: HashMap<String, KnownFile>,
    pub cues: HashMap<(String, u32), KnownCue>,
}

impl KnownRows {
    pub fn any_available(&self) -> bool {
        self.files.values().any(|k| k.available) || self.cues.values().any(|k| k.available)
    }
}

pub(crate) fn root_files(conn: &Connection, root: RootId) -> Result<KnownRows> {
    let mut known = KnownRows::default();
    let mut stmt = conn.prepare_cached(
        "SELECT rel_path, id, size, mtime_ms, available, has_folder_art, hidden,
                cue_sheet, cue_number, cue_size, cue_mtime_ms, artists_parser_version, metadata_revision, COALESCE(artists_source IN ('artists_tag','artist_values','cue_performer','legacy_display','none'),0)
         FROM tracks WHERE root_id = ?1 ORDER BY (hidden=1 AND available=0) DESC,id",
    )?;
    let mut rows = stmt.query([root.0])?;
    while let Some(row) = rows.next()? {
        let rel_path: String = row.get(0)?;
        let id = TrackId(row.get(1)?);
        let size = row.get::<_, i64>(2)? as u64;
        let mtime_ms = row.get(3)?;
        let available = row.get(4)?;
        let has_folder_art = row.get(5)?;
        match row.get::<_, Option<String>>(7)? {
            None => {
                let hidden = row.get(6)?;
                known.files.insert(
                    rel_path,
                    KnownFile {
                        revision: row.get(12)?,
                        id,
                        size,
                        mtime_ms,
                        available,
                        has_folder_art,
                        hidden,
                        artists_complete: row.get::<_, bool>(13)?
                            && row.get::<_, i64>(11)? >= crate::person::PARSER_VERSION,
                    },
                );
            }
            Some(sheet) => {
                let number = row.get::<_, Option<u32>>(8)?.unwrap_or(0);
                known.cues.insert(
                    (sheet, number),
                    KnownCue {
                        revision: row.get(12)?,
                        artists_complete: row.get::<_, bool>(13)?
                            && row.get::<_, i64>(11)? >= crate::person::PARSER_VERSION,
                        id,
                        rel_path,
                        size,
                        mtime_ms,
                        cue_size: row.get::<_, Option<i64>>(9)?.unwrap_or(-1) as u64,
                        cue_mtime_ms: row.get::<_, Option<i64>>(10)?.unwrap_or(-1),
                        available,
                        has_folder_art,
                    },
                );
            }
        }
    }
    Ok(known)
}

/// Only available, visible tracks contribute to person counts. The relation primary key
/// deduplicates people within a track; album keys are counted independently.
const PERSON_COUNTS: &str = r#"
SELECT p.key, MIN(a.name) AS name, p.name_norm,
COUNT(DISTINCT t.album_key) AS album_count,
COUNT(*) AS track_count
FROM person_artists p
JOIN track_artists a ON a.artist_key=p.key
JOIN tracks t ON t.id=a.track_id
WHERE t.available=1 AND t.hidden=0
"#;

fn person_summary(row: &Row<'_>) -> rusqlite::Result<ArtistSummary> {
    Ok(ArtistSummary {
        key: Some(row.get(0)?),
        name: row.get(1)?,
        album_count: row.get(3)?,
        track_count: row.get(4)?,
    })
}

pub(crate) fn person_artists(
    conn: &Connection,
    page: Page,
    query: Option<&SearchQuery>,
) -> Result<Paged<ArtistSummary>> {
    let terms = query.map(SearchQuery::all_terms).unwrap_or_default();
    let condition = contains_all(&["p.name_norm"], 1, terms.len());
    let base = format!("{PERSON_COUNTS} AND {condition} GROUP BY p.key");
    let mut params: Vec<rusqlite::types::Value> =
        terms.into_iter().map(|s| s.to_owned().into()).collect();
    let base_params = params.clone();
    params.push(page.limit.into());
    params.push(page.offset.into());
    let order = if query.is_some() {
        "track_count DESC, name_norm"
    } else {
        "name_norm"
    };
    let mut total = 0i64;
    let mut items = Vec::new();
    let mut stmt = conn.prepare(&format!(
        "SELECT *, COUNT(*) OVER() FROM ({base}) ORDER BY {order} LIMIT ? OFFSET ?"
    ))?;
    let mut rows = stmt.query(params_from_iter(params.iter()))?;
    while let Some(row) = rows.next()? {
        items.push(person_summary(row)?);
        total = row.get(5)?;
    }
    if items.is_empty() {
        total = conn
            .prepare(&format!("SELECT COUNT(*) FROM ({base})"))?
            .query_row(params_from_iter(base_params.iter()), |row| row.get(0))?;
    }
    Ok(Paged {
        items,
        total: total as u64,
    })
}

pub(crate) fn person_albums(
    conn: &Connection,
    name: &str,
    sort: AlbumSort,
    page: Page,
) -> Result<Paged<AlbumSummary>> {
    let condition = concat!(
        "album_key IN (SELECT t.album_key FROM track_artists a JOIN tracks t ON ",
        "t.id=a.track_id WHERE a.artist_key=?1 AND t.available=1 AND t.hidden=0)",
    );
    let key = crate::person::person_key(&normalize(name));
    let total = conn
        .prepare(&format!("SELECT COUNT(*) FROM albums WHERE {condition}"))?
        .query_row([&key], |row| row.get::<_, i64>(0))?;
    let items = conn
        .prepare(&format!(
            "SELECT {ALBUM_COLUMNS} FROM albums WHERE {condition} ORDER BY {} LIMIT ?2 OFFSET ?3",
            album_order(sort)
        ))?
        .query_map(
            rusqlite::params![key, page.limit, page.offset],
            album_from_row,
        )?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Paged {
        items,
        total: total as u64,
    })
}
