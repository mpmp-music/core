use super::*;

// ---- reads ---

/// How long the counts of a smart playlist whose query is relative to the current time
/// ([`Value::DaysAgo`]) stay cached for listings ([`summaries`], [`summary`]); a page of
/// such a playlist always counts afresh, so its total matches its rows.
const RELATIVE_COUNT_TTL: Duration = Duration::from_secs(60);

/// Cached `(track count, duration)` of smart playlists: running every smart query for each
/// listing costs up to a second at 100k tracks. An entry is valid while the playlist is at
/// the same revision (its query and limit) and the library data is at the same version.
pub(crate) struct SmartCounts {
    /// Bumped by the writer after every committed change to tracks, statistics or user
    /// data.
    data_version: Arc<AtomicU64>,
    entries: Mutex<HashMap<PlaylistId, CachedCount>>,
}

#[derive(Clone, Copy)]
struct CachedCount {
    revision: u64,
    data_version: u64,
    /// Set for queries relative to the current time: when the entry stops being valid.
    expires: Option<Instant>,
    count: u64,
    duration_ms: u64,
}

impl SmartCounts {
    pub fn new(data_version: Arc<AtomicU64>) -> Self {
        Self {
            data_version,
            entries: Mutex::default(),
        }
    }

    /// The cache as of now. Take it before the read transaction starts, so a count is never
    /// filed under a newer data version than the snapshot it was computed from.
    pub fn at_current_version(&self) -> Counts<'_> {
        Counts {
            cache: self,
            data_version: self.data_version.load(Ordering::Acquire),
        }
    }
}

/// [`SmartCounts`] at one data version.
pub(crate) struct Counts<'a> {
    cache: &'a SmartCounts,
    data_version: u64,
}

impl Counts<'_> {
    /// `(count, duration)` of smart playlist `p` with its valid `query`.
    fn get(&self, conn: &Connection, p: &Stored, query: &TrackQuery) -> Result<(u64, u64)> {
        let lock = || self.cache.entries.lock().unwrap_or_else(|e| e.into_inner());
        let cached = lock().get(&p.id).copied();
        if let Some(c) = cached
            && c.revision == p.revision
            && c.data_version == self.data_version
            && c.expires.is_none_or(|at| Instant::now() < at)
        {
            return Ok((c.count, c.duration_ms));
        }
        let expires =
            track_query::is_relative(&query.filter).then(|| Instant::now() + RELATIVE_COUNT_TTL);
        let (count, duration_ms) =
            track_query::count_and_duration(conn, query, p.limit.map(u64::from))?;
        let mut entries = lock();
        if entries.len() >= 4096 {
            // Purged playlists leave entries behind; there are never this many smart lists.
            entries.clear();
        }
        entries.insert(
            p.id,
            CachedCount {
                revision: p.revision,
                data_version: self.data_version,
                expires,
                count,
                duration_ms,
            },
        );
        Ok((count, duration_ms))
    }
}

/// `(count, duration)` of smart playlist `p` with its valid `query`, and the query to read
/// its tracks with. With `fresh`, a query relative to the time is counted now, with its
/// times pinned like the returned query, so a page read with that query agrees with the
/// count; otherwise the count may come from the cache.
fn smart_counts(
    conn: &Connection,
    counts: &Counts<'_>,
    p: &Stored,
    query: TrackQuery,
    fresh: bool,
) -> Result<(u64, u64, TrackQuery)> {
    if fresh && track_query::is_relative(&query.filter) {
        let pinned = track_query::pin_time(&query, now_ms());
        let (count, duration) =
            track_query::count_and_duration(conn, &pinned, p.limit.map(u64::from))?;
        return Ok((count, duration, pinned));
    }
    let (count, duration) = counts.get(conn, p, &query)?;
    Ok((count, duration, query))
}

/// The summary of `p`, and for a smart playlist with a valid query the query to read its
/// tracks with (see [`smart_counts`]).
pub(super) fn summarize(
    conn: &Connection,
    counts: &Counts<'_>,
    p: Stored,
    fresh: bool,
) -> Result<(PlaylistSummary, Option<TrackQuery>)> {
    let (kind, track_count, duration_ms, query_error, read) = if p.smart {
        match p.query() {
            Ok(query) => {
                let (count, duration, read) = smart_counts(conn, counts, &p, query.clone(), fresh)?;
                let kind = PlaylistKind::Smart {
                    query,
                    max_tracks: p.limit,
                };
                (kind, count, duration, None, Some(read))
            }
            Err(message) => {
                let kind = PlaylistKind::Smart {
                    query: TrackQuery::default(),
                    max_tracks: p.limit,
                };
                (kind, 0, 0, Some(message), None)
            }
        }
    } else {
        let (count, duration): (i64, i64) = conn
            .prepare_cached(
                "SELECT COUNT(*), COALESCE(SUM(t.duration_ms), 0)
                 FROM playlist_items i LEFT JOIN tracks t ON t.id = i.track_id
                 WHERE i.playlist_id = ?1",
            )?
            .query_row([p.id.0], |row| Ok((row.get(0)?, row.get(1)?)))?;
        (
            PlaylistKind::Manual,
            count.max(0) as u64,
            duration.max(0) as u64,
            None,
            None,
        )
    };
    let summary = PlaylistSummary {
        id: p.id,
        name: p.name,
        kind,
        track_count,
        duration_ms,
        revision: p.revision,
        created_at: p.created_at,
        updated_at: p.updated_at,
        deleted_at: p.deleted_at,
        query_error,
    };
    Ok((summary, read))
}

pub(crate) fn summaries(
    conn: &Connection,
    counts: &Counts<'_>,
    deleted: bool,
    page: Page,
) -> Result<(Vec<PlaylistSummary>, u64)> {
    let (cond, order) = if deleted {
        ("deleted_at IS NOT NULL", "deleted_at DESC, id DESC")
    } else {
        ("deleted_at IS NULL", "name_norm, id")
    };
    let rows: Vec<Stored> = conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM playlists WHERE {cond} ORDER BY {order} LIMIT ?1 OFFSET ?2"
        ))?
        .query_map(params![page.limit, page.offset], stored_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    let total: i64 = conn
        .prepare_cached(&format!("SELECT COUNT(*) FROM playlists WHERE {cond}"))?
        .query_row([], |row| row.get(0))?;
    let list = rows
        .into_iter()
        .map(|p| Ok(summarize(conn, counts, p, false)?.0))
        .collect::<Result<_>>()?;
    Ok((list, total.max(0) as u64))
}

pub(crate) fn summary(
    conn: &Connection,
    counts: &Counts<'_>,
    id: PlaylistId,
) -> Result<Option<PlaylistSummary>> {
    stored(conn, id)?
        .map(|p| Ok(summarize(conn, counts, p, false)?.0))
        .transpose()
}

/// A playlist's summary and a page of its entries, from one snapshot: the page's total is
/// the summary's `track_count`. `None` if the playlist is unknown or purged. A smart
/// playlist whose stored query is unusable lists no entries (`query_error` says why).
pub(crate) fn summary_and_items(
    conn: &Connection,
    counts: &Counts<'_>,
    id: PlaylistId,
    page: Page,
    tags: &[String],
) -> Result<Option<(PlaylistSummary, Vec<PlaylistEntry>)>> {
    let Some(p) = stored(conn, id)? else {
        return Ok(None);
    };
    let (smart, limit) = (p.smart, p.limit);
    let (summary, read) = summarize(conn, counts, p, true)?;
    let entries = match read {
        Some(query) => smart_entries(conn, limit, &query, page, tags)?,
        None if !smart => manual_entries(conn, id, page, tags)?,
        None => Vec::new(),
    };
    Ok(Some((summary, entries)))
}

/// The effective page of a smart playlist's query under its limit: `(offset, limit)`.
pub(super) fn limited(page: Page, limit: Option<u32>) -> Page {
    match limit {
        None => page,
        Some(limit) => Page::new(
            page.offset,
            page.limit.min(limit.saturating_sub(page.offset)),
        ),
    }
}

pub(crate) fn items(
    conn: &Connection,
    counts: &Counts<'_>,
    id: PlaylistId,
    page: Page,
    tags: &[String],
) -> Result<(u64, Vec<PlaylistEntry>, u64)> {
    let p = stored(conn, id)?.ok_or_else(|| playlist_not_found(id))?;
    if p.smart {
        let Ok(query) = p.query() else {
            // An unusable stored query lists no tracks (its summary says why).
            return Ok((p.revision, Vec::new(), 0));
        };
        // The total is the playlist's count (cached unless relative to the time): the
        // query runs once, for the page.
        let (total, _, query) = smart_counts(conn, counts, &p, query, true)?;
        let entries = smart_entries(conn, p.limit, &query, page, tags)?;
        return Ok((p.revision, entries, total));
    }
    let total = item_count(conn, id)?.max(0) as u64;
    Ok((p.revision, manual_entries(conn, id, page, tags)?, total))
}

/// A page of a smart playlist's tracks under its limit.
fn smart_entries(
    conn: &Connection,
    limit: Option<u32>,
    query: &TrackQuery,
    page: Page,
    tags: &[String],
) -> Result<Vec<PlaylistEntry>> {
    let keys = tag_keys(tags)?;
    let page = limited(page, limit);
    let ids = track_query::ids(conn, query, u64::from(page.offset), u64::from(page.limit))?;
    Ok(reads::track_rows(conn, &ids, &keys)?
        .into_iter()
        .map(|row| PlaylistEntry { item: None, row })
        .collect())
}

fn tag_keys(tags: &[String]) -> Result<Vec<String>> {
    tags.iter().map(|k| track_query::tag_key(k)).collect()
}

/// A page of a manual playlist's items.
fn manual_entries(
    conn: &Connection,
    id: PlaylistId,
    page: Page,
    tags: &[String],
) -> Result<Vec<PlaylistEntry>> {
    let keys = tag_keys(tags)?;
    let page_items: Vec<(i64, i64)> = conn
        .prepare_cached(
            "SELECT id, track_id FROM playlist_items WHERE playlist_id = ?1
             ORDER BY position LIMIT ?2 OFFSET ?3",
        )?
        .query_map(params![id.0, page.limit, page.offset], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut unique = Vec::with_capacity(page_items.len());
    let mut seen = HashSet::with_capacity(page_items.len());
    for &(_, track) in &page_items {
        if seen.insert(track) {
            unique.push(TrackId(track));
        }
    }
    let rows: HashMap<TrackId, TrackRow> = reads::track_rows(conn, &unique, &keys)?
        .into_iter()
        .map(|row| (row.track.id, row))
        .collect();
    Ok(page_items
        .into_iter()
        .filter_map(|(item, track)| {
            // Every item has its track (tracks are never hard-deleted; see schema v3).
            rows.get(&TrackId(track)).map(|row| PlaylistEntry {
                item: Some(PlaylistItemId(item)),
                row: row.clone(),
            })
        })
        .collect())
}

/// Resolve tracks and the optional exact item's playable index in the caller's read
/// transaction. Both the revision and item belong to that same snapshot.
pub(crate) fn playback(
    conn: &Connection,
    id: PlaylistId,
    limit: usize,
    start_item: Option<PlaylistItemId>,
    expected_revision: Option<u64>,
) -> Result<(Vec<TrackId>, Option<usize>)> {
    let p = stored(conn, id)?.ok_or_else(|| playlist_not_found(id))?;
    check_revision(&p, expected_revision)?;
    let Some(item) = start_item else {
        return track_ids(conn, id, limit).map(|tracks| (tracks, None));
    };
    require_manual(&p)?;
    let rows = conn
        .prepare_cached(
            "SELECT i.id, i.track_id FROM playlist_items i JOIN tracks t ON t.id = i.track_id
         WHERE i.playlist_id = ?1 AND t.available = 1 ORDER BY i.position LIMIT ?2",
        )?
        .query_map(
            params![id.0, i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| Ok((PlaylistItemId(row.get(0)?), TrackId(row.get(1)?))),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let index = rows
        .iter()
        .position(|(candidate, _)| *candidate == item)
        .ok_or_else(|| {
            LibraryError::not_found(
                NotFound::Item,
                format!("item {} is not playable in playlist {}", item.0, id.0),
            )
        })?;
    Ok((
        rows.into_iter().map(|(_, track)| track).collect(),
        Some(index),
    ))
}

/// Track ids to play, in order without unavailable ones, at most `limit`.
pub(crate) fn track_ids(conn: &Connection, id: PlaylistId, limit: usize) -> Result<Vec<TrackId>> {
    let p = stored(conn, id)?.ok_or_else(|| playlist_not_found(id))?;
    let limit = u64::try_from(limit).unwrap_or(u64::MAX);
    if !p.smart {
        return Ok(conn
            .prepare_cached(
                "SELECT i.track_id FROM playlist_items i JOIN tracks t ON t.id = i.track_id
                 WHERE i.playlist_id = ?1 AND t.available = 1
                 ORDER BY i.position LIMIT ?2",
            )?
            .query_map(
                params![id.0, i64::try_from(limit).unwrap_or(i64::MAX)],
                |row| Ok(TrackId(row.get(0)?)),
            )?
            .collect::<rusqlite::Result<_>>()?);
    }
    let mut query = p.query_or_error()?;
    if !track_query::mentions_available(&query.filter) {
        // Only available tracks match anyway.
        let limit = p.limit.map_or(limit, |l| limit.min(u64::from(l)));
        return track_query::ids(conn, &query, 0, limit);
    }
    match p.limit {
        // The playlist is the first `l` matches; unavailable ones are left out of those.
        Some(l) => {
            let ids = track_query::ids(conn, &query, 0, u64::from(l))?;
            let available: HashSet<TrackId> = reads::tracks(conn, &ids)?
                .into_iter()
                .filter(|t| t.available)
                .map(|t| t.id)
                .collect();
            Ok(ids
                .into_iter()
                .filter(|id| available.contains(id))
                .take(usize::try_from(limit).unwrap_or(usize::MAX))
                .collect())
        }
        None => {
            query.filter = Filter::And {
                all: vec![
                    Filter::Compare {
                        field: Field::Available,
                        op: CompareOp::Eq,
                        value: Value::Bool(true),
                    },
                    query.filter,
                ],
            };
            track_query::ids(conn, &query, 0, limit)
        }
    }
}

/// One track of a playlist for export: root path, relative path, duration, artist, title.
pub(crate) struct ExportTrack {
    pub root: String,
    pub rel_path: String,
    pub duration_ms: Option<i64>,
    pub artist: Option<String>,
    pub title: Option<String>,
}

/// All tracks of a playlist in order (manual: every item, including unavailable tracks;
/// smart: the query's tracks under its limit).
pub(crate) fn export_tracks(conn: &Connection, id: PlaylistId) -> Result<Vec<ExportTrack>> {
    let p = stored(conn, id)?.ok_or_else(|| playlist_not_found(id))?;
    let read = |row: &rusqlite::Row<'_>| {
        Ok(ExportTrack {
            root: row.get(0)?,
            rel_path: row.get(1)?,
            duration_ms: row.get(2)?,
            artist: row.get(3)?,
            title: row.get(4)?,
        })
    };
    if !p.smart {
        return Ok(conn
            .prepare(
                "SELECT r.path, t.rel_path, t.duration_ms, t.artist, t.title
                 FROM playlist_items i JOIN tracks t ON t.id = i.track_id
                 JOIN roots r ON r.id = t.root_id
                 WHERE i.playlist_id = ?1 ORDER BY i.position",
            )?
            .query_map([id.0], read)?
            .collect::<rusqlite::Result<_>>()?);
    }
    let query = p.query_or_error()?;
    let ids = track_query::ids(conn, &query, 0, p.limit.map_or(u64::MAX, u64::from))?;
    let mut stmt = conn.prepare_cached(
        "SELECT r.path, t.rel_path, t.duration_ms, t.artist, t.title
         FROM tracks t JOIN roots r ON r.id = t.root_id WHERE t.id = ?1",
    )?;
    let mut tracks = Vec::with_capacity(ids.len());
    for track in ids {
        if let Some(row) = stmt.query_row([track.0], read).optional()? {
            tracks.push(row);
        }
    }
    Ok(tracks)
}
