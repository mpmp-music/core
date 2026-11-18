//! Derived loudness data. SQLite triggers invalidate it when audio/CUE versions or
//! album membership change; file RG tags remain untouched as the fallback.
use crate::{AlbumKey, FileVersion, Library, LibraryError, Result, TrackId};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Includes actual audio/CUE versions, ranges and identity; not merely max(mtime).
const VERSION: &str = concat!(
    "json_array(t.size,t.mtime_ms,t.cue_sheet,t.cue_number,t.cue_start,",
    "t.cue_end,t.cue_size,t.cue_mtime_ms,t.rel_path,t.root_id,t.sample_rate,t.channels)"
);

/// `VERSION` without the file's own stamp: what rewriting tags alone leaves as it was.
const AUDIO: &str = concat!(
    "json_array(t.cue_sheet,t.cue_number,t.cue_start,t.cue_end,t.cue_size,",
    "t.cue_mtime_ms,t.rel_path,t.root_id,t.sample_rate,t.channels)"
);

/// The length of the statistics an automatic measurement stores.
const STATISTICS_BYTES: i64 = 8005;

#[derive(Debug, Clone)]
pub struct LoudnessSource {
    pub version: String,
    pub audio: FileVersion,
    pub sidecar: Option<(PathBuf, FileVersion)>,
    pub album: AlbumKey,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoudnessMeasurement {
    pub integrated_lufs: Option<f64>,
    pub sample_peak: f64,
    pub album_lufs: Option<f64>,
    pub album_peak: Option<f64>,
    pub measured_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct LoudnessUpdate {
    pub statistics: Option<Vec<u8>>,
    pub track: TrackId,
    pub version: String,
    pub integrated_lufs: Option<f64>,
    pub sample_peak: f64,
}

#[derive(Debug, Clone)]
pub struct AlbumLoudnessUpdate {
    pub album: AlbumKey,
    /// Every available, visible member and its measured version, sorted by track id.
    pub members: Vec<(TrackId, String)>,
    pub integrated_lufs: f64,
    pub sample_peak: f64,
}

#[derive(Debug, Default)]
pub struct LoudnessCommit {
    pub tracks: Vec<TrackId>,
    pub album_count: u32,
}

/// One track of an automatic pass, with what the cache holds for it (read with it, so a
/// pass over a measured library reads no statistics).
#[derive(Debug, Clone, PartialEq)]
pub struct AutomaticLoudnessTrack {
    pub track: TrackId,
    /// Statistics of its current version are stored (their content is checked where used).
    pub measured: bool,
    pub album: AlbumKey,
    /// Its album has no aggregate stored.
    pub album_pending: bool,
}

fn token(conn: &Connection, id: TrackId) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            &format!(
                concat!(
                    "SELECT {VERSION} FROM tracks t WHERE t.id=?1 AND t.available=1 AND ",
                    "t.hidden=0 AND t.mtime_ms != -1",
                ),
                VERSION = VERSION,
            ),
            [id.0],
            |r| r.get(0),
        )
        .optional()?)
}

fn members(conn: &Connection, album: &AlbumKey) -> Result<Vec<(TrackId, String)>> {
    Ok(conn
        .prepare(&format!(
            concat!(
                "SELECT t.id,{VERSION} FROM tracks t WHERE t.album_key=?1 AND ",
                "t.available=1 AND t.hidden=0 ORDER BY t.id",
            ),
            VERSION = VERSION,
        ))?
        .query_map([album.as_str()], |r| Ok((TrackId(r.get(0)?), r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

fn valid(lufs: Option<f64>, peak: f64) -> bool {
    lufs.is_none_or(f64::is_finite)
        && peak.is_finite()
        && peak >= 0.0
        && peak <= f64::from(f32::MAX)
}

pub type AlbumLoudnessStatistics = Vec<(TrackId, String, f64)>;

impl Library {
    pub fn loudness_source(&self, id: TrackId) -> Result<Option<LoudnessSource>> {
        const SELECT_TRACKS: &str = r#"
SELECT t.size,t.mtime_ms,t.cue_sheet,t.cue_size,t.cue_mtime_ms,t.rel_path,r.path,t.album_key
FROM tracks t JOIN roots r ON r.id=t.root_id WHERE t.id=?1 AND r.removed=0
"#;

        self.inner().pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let Some(version) = token(&tx, id)? else {
                return Ok(None);
            };
            let source = tx
                .query_row(SELECT_TRACKS, [id.0], |r| {
                    let sheet: Option<String> = r.get(2)?;
                    let rel: String = r.get(5)?;
                    let root = PathBuf::from(r.get::<_, String>(6)?);
                    let sidecar = match sheet.filter(|s| *s != rel) {
                        Some(sheet) => {
                            let mut path = root;
                            path.extend(sheet.split('/'));
                            Some((
                                path,
                                FileVersion {
                                    size: r.get::<_, i64>(3)?.max(0) as u64,
                                    mtime_ms: r.get(4)?,
                                },
                            ))
                        }
                        None => None,
                    };
                    Ok(LoudnessSource {
                        version,
                        audio: FileVersion {
                            size: r.get::<_, i64>(0)?.max(0) as u64,
                            mtime_ms: r.get(1)?,
                        },
                        sidecar,
                        album: AlbumKey(r.get(7)?),
                    })
                })
                .optional()?;
            tx.commit()?;
            Ok(source)
        })
    }

    pub fn loudness_members(&self, album: &AlbumKey) -> Result<Vec<(TrackId, String)>> {
        self.inner().pool.read(|conn| members(conn, album))
    }

    pub fn loudness(&self, id: TrackId) -> Result<Option<LoudnessMeasurement>> {
        const SELECT_TRACK_LOUDNESS: &str = r#"
SELECT l.integrated_lufs,l.sample_peak,a.integrated_lufs,a.sample_peak,l.measured_at
FROM track_loudness l JOIN tracks t ON t.id=l.track_id LEFT JOIN album_loudness
a ON a.album_key=t.album_key WHERE l.track_id=?1
"#;

        self.inner().pool.read(|conn| {
            Ok(conn
                .query_row(SELECT_TRACK_LOUDNESS, [id.0], |r| {
                    Ok(LoudnessMeasurement {
                        integrated_lufs: r.get(0)?,
                        sample_peak: r.get(1)?,
                        album_lufs: r.get(2)?,
                        album_peak: r.get(3)?,
                        measured_at_ms: r.get(4)?,
                    })
                })
                .optional()?)
        })
    }

    /// Bounded keyset pages: never load the whole library into the actor's queue.
    pub fn automatic_loudness_tracks(
        &self,
        after: i64,
        limit: usize,
    ) -> Result<Vec<AutomaticLoudnessTrack>> {
        const AUTOMATIC_TRACKS: &str = r#"
SELECT t.id,
            EXISTS(SELECT 1 FROM track_loudness l WHERE l.track_id=t.id AND length(l.statistics)=?3),
            t.album_key, NOT EXISTS(SELECT 1 FROM album_loudness a WHERE a.album_key=t.album_key)
            FROM tracks t JOIN roots r ON r.id=t.root_id WHERE t.id>?1 AND t.available=1 AND t.hidden=0 AND r.removed=0 AND t.mtime_ms != -1 ORDER BY t.id LIMIT ?2
"#;

        self.inner().pool.read(|conn| {
            Ok(conn
                .prepare(AUTOMATIC_TRACKS)?
                .query_map(
                    params![after, limit.min(128) as i64, STATISTICS_BYTES],
                    |r| {
                        Ok(AutomaticLoudnessTrack {
                            track: TrackId(r.get(0)?),
                            measured: r.get(1)?,
                            album: AlbumKey(r.get(2)?),
                            album_pending: r.get(3)?,
                        })
                    },
                )?
                .collect::<rusqlite::Result<_>>()?)
        })
    }
    pub fn loudness_statistics(&self, id: TrackId) -> Result<Option<Vec<u8>>> {
        self.inner().pool.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT statistics FROM track_loudness WHERE track_id=?1",
                    [id.0],
                    |r| r.get(0),
                )
                .optional()?
                .flatten())
        })
    }
    /// A complete current album's cached per-track statistics, or none if incomplete,
    /// unknown, or already aggregated. Read in one snapshot.
    pub fn pending_album_loudness(&self, album: &AlbumKey) -> Result<AlbumLoudnessStatistics> {
        const SELECT_TRACK_LOUDNESS: &str = r#"
SELECT sample_peak FROM track_loudness WHERE track_id=?1 AND statistics
IS NOT NULL
"#;

        if album.is_unknown_album() {
            return Ok(vec![]);
        }
        self.inner().pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM album_loudness WHERE album_key=?1)",
                [album.as_str()],
                |r| r.get(0),
            )?;
            if exists {
                return Ok(vec![]);
            }
            let members = members(&tx, album)?;
            let mut result = Vec::with_capacity(members.len());
            for (id, version) in members {
                let peak: Option<f64> = tx
                    .query_row(SELECT_TRACK_LOUDNESS, [id.0], |r| r.get(0))
                    .optional()?;
                let Some(peak) = peak else {
                    return Ok(vec![]);
                };
                result.push((id, version, peak));
            }
            tx.commit()?;
            Ok(result)
        })
    }
    pub fn store_automatic_album_loudness(&self, album: AlbumLoudnessUpdate) -> Result<bool> {
        const SELECT_TRACK_LOUDNESS: &str = r#"
SELECT EXISTS(SELECT 1 FROM track_loudness WHERE track_id=?1 AND version=?2
AND statistics IS NOT NULL)
"#;

        if !valid(Some(album.integrated_lufs), album.sample_peak) || album.album.is_unknown_album()
        {
            return Ok(false);
        }
        self.inner().writer.run_data(move |conn| {
            let tx = conn.transaction()?;
            if album.members.is_empty() || members(&tx, &album.album)? != album.members {
                return Ok(false);
            }
            for (id, version) in &album.members {
                let exists: bool =
                    tx.query_row(SELECT_TRACK_LOUDNESS, params![id.0, version], |r| r.get(0))?;
                if !exists {
                    return Ok(false);
                }
            }
            tx.execute(
                "INSERT OR REPLACE INTO album_loudness VALUES(?1,?2,?3,?4)",
                params![
                    album.album.as_str(),
                    album.integrated_lufs,
                    album.sample_peak,
                    crate::db::writer::now_ms()
                ],
            )?;
            tx.commit()?;
            Ok(true)
        })
    }
    pub fn store_automatic_loudness(&self, updates: Vec<LoudnessUpdate>) -> Result<LoudnessCommit> {
        self.store_loudness_impl(updates, vec![])
    }
    /// Atomically publish valid measurements. Stale members are refused individually;
    /// an album is published only when every member was measured and is still current.
    pub fn store_loudness(
        &self,
        updates: Vec<LoudnessUpdate>,
        albums: Vec<AlbumLoudnessUpdate>,
    ) -> Result<LoudnessCommit> {
        self.store_loudness_impl(updates, albums)
    }
    fn store_loudness_impl(
        &self,
        updates: Vec<LoudnessUpdate>,
        albums: Vec<AlbumLoudnessUpdate>,
    ) -> Result<LoudnessCommit> {
        const INSERT_TRACK_LOUDNESS: &str = r#"
INSERT OR REPLACE INTO track_loudness(track_id,version,integrated_lufs,sample_peak,measured_at,statistics)
VALUES (?1,?2,?3,?4,?5,?6)
"#;

        const DELETE_ALBUM_LOUDNESS: &str = r#"
DELETE FROM album_loudness WHERE album_key=(SELECT album_key FROM tracks
WHERE id=?1)
"#;

        if updates.len() > 1000
            || albums.len() > 1000
            || updates.iter().any(|u| {
                !valid(u.integrated_lufs, u.sample_peak)
                    || u.statistics
                        .as_ref()
                        .is_some_and(|s| s.len() > STATISTICS_BYTES as usize)
            })
            || updates
                .iter()
                .map(|u| u.track)
                .collect::<BTreeSet<_>>()
                .len()
                != updates.len()
            || albums
                .iter()
                .any(|a| a.members.len() > 1000 || !valid(Some(a.integrated_lufs), a.sample_peak))
        {
            return Err(LibraryError::invalid(
                crate::Invalid::OutOfRange,
                "invalid loudness measurements",
            ));
        }
        self.inner().writer.run_data(move |conn| {
            let tx = conn.transaction()?;
            let now = crate::db::writer::now_ms();
            let mut accepted = BTreeSet::new();
            for update in updates {
                if token(&tx, update.track)?.as_deref() != Some(&update.version) {
                    continue;
                }
                tx.execute(
                    INSERT_TRACK_LOUDNESS,
                    params![
                        update.track.0,
                        update.version,
                        update.integrated_lufs,
                        update.sample_peak,
                        now,
                        update.statistics
                    ],
                )?;
                tx.execute(DELETE_ALBUM_LOUDNESS, [update.track.0])?;
                accepted.insert(update.track);
            }
            let mut album_count = 0;
            for album in albums {
                if album.album.is_unknown_album()
                    || album.members.is_empty()
                    || album.members.iter().any(|(id, _)| !accepted.contains(id))
                {
                    continue;
                }
                if members(&tx, &album.album)? != album.members {
                    continue;
                }
                tx.execute(
                    "INSERT OR REPLACE INTO album_loudness VALUES (?1,?2,?3,?4)",
                    params![
                        album.album.as_str(),
                        album.integrated_lufs,
                        album.sample_peak,
                        now
                    ],
                )?;
                album_count += 1;
            }
            tx.commit()?;
            Ok(LoudnessCommit {
                tracks: accepted.into_iter().collect(),
                album_count,
            })
        })
    }

    /// Clear derived data, retaining file tags. Album invalidation also affects its other
    /// members; return all affected tracks so clients refresh their displayed RG values.
    pub fn clear_loudness(&self, tracks: &[TrackId]) -> Result<Vec<TrackId>> {
        if tracks.len() > 1000 {
            return Err(LibraryError::invalid(
                crate::Invalid::OutOfRange,
                "at most 1000 tracks",
            ));
        }
        let tracks = tracks.to_vec();
        self.inner().writer.run_data(move |conn| {
            let tx = conn.transaction()?;
            let mut affected = BTreeSet::new();
            for id in tracks {
                let album: Option<AlbumKey> = tx
                    .query_row("SELECT album_key FROM tracks WHERE id=?1", [id.0], |r| {
                        Ok(AlbumKey(r.get(0)?))
                    })
                    .optional()?;

                if tx.execute("DELETE FROM track_loudness WHERE track_id=?1", [id.0])? > 0 {
                    affected.insert(id);
                }
                if let Some(album) = album
                    && tx.execute(
                        "DELETE FROM album_loudness WHERE album_key=?1",
                        [album.as_str()],
                    )? > 0
                {
                    for (id, _) in members(&tx, &album)? {
                        affected.insert(id);
                    }
                }
            }
            tx.commit()?;
            Ok(affected.into_iter().collect())
        })
    }
}

/// A measurement a tag job carries over to the file it rewrote.
type Kept = (i64, String, Option<f64>, f64, i64, Option<Vec<u8>>);

/// Commits the row of a file whose tags a tag job rewrote (or an undo wrote back). The job
/// writes a file only when it is the version the library knew, and changes its tags alone:
/// its audio is what was measured, so the measurement moves to the new stamp instead of
/// being dropped by `loudness_track_version` (and the file decoded again). Only when
/// nothing but the stamp changed. The album's aggregate is still dropped (its members or
/// title may have changed); an automatic pass rebuilds it from the stored statistics.
pub(crate) fn upsert_retagged(
    storage: &dyn mpmp_storage::LibraryStorage,
    tx: &rusqlite::Transaction<'_>,
    record: &crate::db::writer::TrackRecord,
    now: i64,
) -> Result<()> {
    const INSERT_TRACK_LOUDNESS: &str = r#"
INSERT OR REPLACE INTO track_loudness(track_id,version,integrated_lufs,sample_peak,measured_at,statistics)
VALUES (?1,?2,?3,?4,?5,?6)
"#;

    let kept: Option<Kept> = if record.cue.is_none() {
        tx.query_row(
            &format!(
                "SELECT t.id, {AUDIO}, l.integrated_lufs, l.sample_peak, l.measured_at, l.statistics
                 FROM tracks t JOIN track_loudness l ON l.track_id=t.id
                 WHERE t.root_id=?1 AND t.rel_path=?2 AND t.cue_sheet IS NULL
                 AND t.mtime_ms != -1 AND l.version={VERSION}"
            ),
            params![record.root_id.0, record.rel_path],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?
    } else {
        None
    };
    crate::db::writer::upsert_track(storage, tx, record, now)?;
    let Some((id, audio, lufs, peak, measured_at, statistics)) = kept else {
        return Ok(());
    };
    let version: Option<String> = tx
        .query_row(
            &format!(
                "SELECT {VERSION} FROM tracks t WHERE t.id=?1 AND t.mtime_ms != -1 AND {AUDIO}=?2"
            ),
            params![id, audio],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(version) = version {
        tx.execute(
            INSERT_TRACK_LOUDNESS,
            params![id, version, lufs, peak, measured_at, statistics],
        )?;
    }
    Ok(())
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests;
