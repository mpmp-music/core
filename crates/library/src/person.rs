//! Ordered name identities, separate from display credits and album grouping.

use std::collections::HashSet;

use rusqlite::{Connection, Transaction, params};
use sha2::{Digest, Sha256};

use crate::text::normalize;
use crate::{ArtistListSource, ArtistRef, Result};

// v2 recognizes Picard's slash-joined explicit ARTISTS in ID3v2.3. The stored
// raw tags lack the ID3 version, so existing memberships need a format-aware read.
pub(crate) const PARSER_VERSION: i64 = 2;

/// This prefix pins the identity normalization and hash domain independently of the
/// metadata parser version. Never reinterpret existing v1 identities with a new rule.
pub(crate) fn person_key(normalized: &str) -> String {
    format!("person:v1:{:x}", Sha256::digest(normalized.as_bytes()))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ArtistList {
    pub artists: Vec<ArtistRef>,
    pub source: ArtistListSource,
}

impl ArtistList {
    pub fn new<'a>(names: impl IntoIterator<Item = &'a str>, source: ArtistListSource) -> Self {
        let mut seen = HashSet::new();
        let artists = names
            .into_iter()
            .filter_map(|name| {
                let name = name.trim();
                let mut end = name.len().min(crate::tags::read::MAX_TAG_VALUE);
                while !name.is_char_boundary(end) {
                    end -= 1;
                }
                let name = &name[..end];
                let norm = normalize(name);
                if norm.is_empty() || !seen.insert(norm.clone()) {
                    return None;
                }
                let key = person_key(&norm);
                Some(ArtistRef {
                    key,
                    name: name.to_owned(),
                })
            })
            .collect();
        Self { artists, source }
    }

    pub fn from_tags(tags: &[(String, String)], display: Option<&str>) -> Self {
        for (key, source) in [
            ("ARTISTS", ArtistListSource::ArtistsTag),
            ("ARTIST", ArtistListSource::ArtistValues),
        ] {
            let list = Self::new(
                tags.iter()
                    .filter(|(k, _)| k == key)
                    .map(|(_, v)| v.as_str()),
                source,
            );
            if !list.artists.is_empty() {
                return list;
            }
        }
        let list = Self::new(display, ArtistListSource::LegacyDisplay);
        if list.artists.is_empty() {
            Self::new([], ArtistListSource::None)
        } else {
            list
        }
    }

    fn source_json(&self) -> Result<String> {
        serde_json::to_string(&self.source).map_err(|e| crate::LibraryError::Corrupt(e.to_string()))
    }
}

pub(crate) fn matches_stored(tx: &Transaction<'_>, track: i64, list: &ArtistList) -> Result<bool> {
    const SELECT_SOURCE: &str =
        "SELECT artists_source IS ?2 AND artists_parser_version=?3 FROM tracks WHERE id=?1";
    let source = list.source_json()?;
    let matches = tx.prepare_cached(SELECT_SOURCE)?.query_row(
        params![track, source.trim_matches('"'), PARSER_VERSION],
        |row| row.get::<_, bool>(0),
    )?;
    if !matches {
        return Ok(false);
    }
    let mut stmt = tx.prepare_cached(
        "SELECT ordinal,artist_key,name FROM track_artists WHERE track_id=?1 ORDER BY ordinal",
    )?;
    let mut rows = stmt.query([track])?;
    for (ordinal, artist) in list.artists.iter().enumerate() {
        let Some(row) = rows.next()? else {
            return Ok(false);
        };
        if row.get::<_, i64>(0)? != ordinal as i64
            || row.get::<_, String>(1)? != artist.key
            || row.get::<_, String>(2)? != artist.name
        {
            return Ok(false);
        }
    }
    Ok(rows.next()?.is_none())
}

pub(crate) fn replace(
    tx: &Transaction<'_>,
    track: i64,
    list: &ArtistList,
    version: i64,
) -> Result<()> {
    const INSERT_PERSON_ARTISTS: &str = r#"
INSERT INTO person_artists (key, name_norm) VALUES (?1, ?2) ON CONFLICT(key)
DO NOTHING
"#;

    const INSERT_TRACK_ARTISTS: &str = r#"
INSERT INTO track_artists (track_id, artist_key, ordinal, name) VALUES
(?1, ?2, ?3, ?4)
"#;

    const DELETE_PERSON_ARTISTS: &str = r#"
DELETE FROM person_artists WHERE key=?1 AND NOT EXISTS(SELECT 1 FROM
track_artists WHERE artist_key=?1)
"#;

    let old_keys = tx
        .prepare_cached("SELECT artist_key FROM track_artists WHERE track_id=?1")?
        .query_map([track], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tx.prepare_cached("DELETE FROM track_artists WHERE track_id=?1")?
        .execute([track])?;
    for (ordinal, artist) in list.artists.iter().enumerate() {
        tx.prepare_cached(INSERT_PERSON_ARTISTS)?
            .execute(params![artist.key, normalize(&artist.name)])?;
        tx.prepare_cached(INSERT_TRACK_ARTISTS)?.execute(params![
            track,
            artist.key,
            ordinal as i64,
            artist.name
        ])?;
    }
    for key in old_keys {
        tx.prepare_cached(DELETE_PERSON_ARTISTS)?.execute([key])?;
    }
    let source = list.source_json()?;
    tx.prepare_cached(
        "UPDATE tracks SET artists_source=?2, artists_parser_version=?3 WHERE id=?1",
    )?
    .execute(params![track, source.trim_matches('"'), version])?;
    Ok(())
}

pub(crate) fn indexing(conn: &Connection) -> Result<bool> {
    const SELECT_TRACKS: &str = r#"
SELECT EXISTS(SELECT 1 FROM tracks t JOIN roots r ON r.id=t.root_id
WHERE (t.artists_parser_version < ?1 OR COALESCE(t.artists_source IN
('artists_tag','artist_values','cue_performer','legacy_display','none'),0)=0)
AND t.hidden=0 AND t.available=1 AND r.removed=0)
"#;

    Ok(conn.query_row(SELECT_TRACKS, [PARSER_VERSION], |row| row.get(0))?)
}
