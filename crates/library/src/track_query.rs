//! Flexible track queries (docs/design/queries-playlists-undo.md): typed filters and sorts compiled to
//! SQL, a page of tracks or of one field's values (facets).
//!
//! Safety: fields map to SQL through a fixed whitelist ([`column`]); every value from the
//! query — texts, numbers, tag keys — is bound as a parameter. No input reaches SQL text.

use std::collections::BTreeMap;

use rusqlite::types::Value as SqlValue;

use rusqlite::{Connection, params_from_iter};

use serde::{Deserialize, Serialize};

use crate::db::writer::now_ms;

use crate::text::{SearchQuery, normalize};

use crate::types::{Page, Track, TrackId};

use crate::{LibraryError, Result, db::reads};

/// A track attribute usable in filters, sorts and facets. Each has a fixed type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Field {
    // Tags (text unless noted).
    /// Without a title tag, sorts, comparisons and text matches use the file name (without
    /// extension), so untagged files sort among the others; `Missing { Title }` (and the
    /// unknown group of a Title facet) means "no title tag".
    Title,
    Artist,
    Album,
    AlbumArtist,
    /// The persisted album/artist grouping name: album artist, else Various Artists for
    /// compilations, else track artist. Text comparisons use normalized search keys.
    GroupArtist,
    Genre,
    /// Integer.
    Year,
    /// Integer.
    TrackNo,
    /// Integer.
    DiscNo,
    /// Boolean.
    Compilation,
    /// Any tag by key (case-insensitive, e.g. `COMPOSER`, `LABEL`), text. Multi-valued tags
    /// match if any value does.
    Tag {
        key: String,
    },
    // Audio properties.
    /// Integer.
    DurationMs,
    /// Text: `flac`, `mp3`, `aac`, `alac`, `vorbis`, `pcm`,...
    Codec,
    /// Integer.
    SampleRate,
    /// Integer: bits per sample.
    BitsPerSample,
    /// Integer.
    Channels,
    /// Boolean.
    Lossless,
    /// Integer.
    BitrateKbps,
    // File.
    /// Text: path relative to the root, `/`-separated.
    Path,
    /// Integer: the root id.
    Root,
    /// Integer.
    SizeBytes,
    /// Timestamp.
    ModifiedAt,
    /// Timestamp.
    AddedAt,
    /// Boolean. Unless a filter mentions it, queries only see available tracks.
    Available,
    /// Boolean: embedded art or a cover image in the folder.
    HasCover,
    /// Boolean: a track of a CUE sheet ([`crate::Track::cue`]).
    CueSheet,
    // Play statistics.
    /// Integer.
    PlayCount,
    /// Integer.
    SkipCount,
    /// Timestamp of the last counted play.
    LastPlayedAt,
    /// Timestamp of the first counted play.
    FirstPlayedAt,
    // User data (database only).
    /// Integer 1–5; missing when unrated.
    Rating,
    /// Boolean.
    Loved,
}

/// A typed value. Timestamps are Unix milliseconds; `DaysAgo(n)` means "now minus n days"
/// at execution time, so saved queries stay relative.
///
/// JSON: adjacently tagged, `{"type": "int", "value": 5}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Value {
    Int(i64),
    Bool(bool),
    Text(String),
    Timestamp(i64),
    DaysAgo(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompareOp {
    Eq,
    /// Not equal; tracks without a value count as not equal.
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TextOp {
    Equals,
    Contains,
    StartsWith,
}

/// A filter expression. Text comparisons use the normalized, accent-folded search keys
/// (`etude` matches `Étude`). A missing value (no tag, or one that is empty after
/// normalization) satisfies no comparison or text match except `ne`.
///
/// Size limits (else [`crate::LibraryError::InvalidQuery`]): at most 500 parts in one `And`
/// or `Or`, 2000 nodes in all (groups, `Not`s and conditions), 64 levels of nesting.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Filter {
    #[default]
    All,
    And {
        all: Vec<Filter>,
    },
    Or {
        any: Vec<Filter>,
    },
    Not {
        filter: Box<Filter>,
    },
    /// Numeric, timestamp and boolean comparison; on text fields, the value must be text
    /// and compares search keys.
    Compare {
        field: Field,
        op: CompareOp,
        value: Value,
    },
    /// Text match on a text field or tag.
    Text {
        field: Field,
        op: TextOp,
        text: String,
    },
    /// Exact normalized person-name membership.
    ArtistPerson {
        name: String,
    },
    /// Full-text search, as [`crate::Library::search`] does for tracks.
    Search {
        text: String,
    },
    /// The field has no value (or an empty one).
    Missing {
        field: Field,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SortKey {
    pub field: Field,
    #[serde(default)]
    pub descending: bool,
}

/// A filter plus sort keys. Sorting is always stable: album artist, album, disc, track
/// number, path and id follow the given keys. Missing values sort last either way.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrackQuery {
    #[serde(default)]
    pub filter: Filter,
    #[serde(default)]
    pub sort: Vec<SortKey>,
}

impl TrackQuery {
    /// Checks types and size limits without opening a database or running a query.
    /// Adapters can use the same validator when attaching request field locations.
    pub fn validate(&self) -> Result<()> {
        validate(self)
    }
}

/// A track with its statistics, user data and requested tags.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackRow {
    pub track: Track,
    pub play_count: u64,
    pub skip_count: u64,
    pub last_played_at: Option<i64>,
    pub first_played_at: Option<i64>,
    pub rating: Option<u8>,
    pub loved: bool,
    /// Requested tags (upper-case keys) that the track has, all values in file order.
    pub tags: BTreeMap<String, Vec<String>>,
}

/// One value of a facet and how many tracks have it; `None` is the "unknown" group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FacetValue {
    pub value: Option<Value>,
    pub tracks: u64,
}

/// Upper-cased tag key; empty keys are invalid.
pub(crate) fn tag_key(key: &str) -> Result<String> {
    let key = key.trim();
    if key.is_empty() {
        return Err(invalid("tag field with an empty key".to_owned()));
    }
    Ok(key.to_uppercase())
}

/// Most parts of one `And` / `Or`.
pub(crate) const MAX_WIDTH: usize = 500;

/// Most filter nodes (every `And`, `Or`, `Not` and condition counts) in one query.
pub(crate) const MAX_NODES: usize = 2000;

/// Whether the filter mentions [`Field::Available`]; otherwise queries see only available
/// tracks.
pub(crate) fn mentions_available(filter: &Filter) -> bool {
    match filter {
        Filter::And { all: parts } | Filter::Or { any: parts } => {
            parts.iter().any(mentions_available)
        }
        Filter::Not { filter } => mentions_available(filter),
        Filter::Compare { field, .. } | Filter::Text { field, .. } | Filter::Missing { field } => {
            *field == Field::Available
        }
        Filter::All | Filter::Search { .. } | Filter::ArtistPerson { .. } => false,
    }
}

/// Type-checks `query` without running it: [`LibraryError::InvalidQuery`] if it is invalid.
pub(crate) fn validate(query: &TrackQuery) -> Result<()> {
    Compiled::new(query).map(drop)
}

/// [`validate`], then also prepares the statements `query` compiles to on `conn`, so a
/// query that SQLite cannot run is [`LibraryError::InvalidQuery`] now, not a failure each
/// time it is read (for queries that are stored: smart playlists).
pub(crate) fn validate_prepared(conn: &Connection, query: &TrackQuery) -> Result<()> {
    let c = Compiled::new(query)?;
    for text in [
        format!(
            "SELECT t.id FROM tracks t{} WHERE {} ORDER BY {} LIMIT ? OFFSET ?",
            c.joins, c.cond, c.order
        ),
        format!(
            "SELECT COUNT(*) FROM tracks t{} WHERE {}",
            c.count_joins, c.cond
        ),
    ] {
        conn.prepare(&text)
            .map_err(|err| invalid(format!("query cannot run: {err}")))?;
    }
    Ok(())
}

/// Ids of the tracks matching `query`, in order: `(ids, total)`.
pub(crate) fn ids_page(
    conn: &Connection,
    query: &TrackQuery,
    offset: u64,
    limit: u64,
) -> Result<(Vec<TrackId>, u64)> {
    let c = Compiled::new(query)?;
    // A separate count: `COUNT(*) OVER` would make SQLite sort every match instead of
    // keeping only the top `offset + limit`.
    let ids = select_ids(conn, &c, offset, limit)?;
    let total = if offset == 0 && (ids.len() as u64) < limit {
        ids.len() as u64
    } else {
        let text = format!(
            "SELECT COUNT(*) FROM tracks t{} WHERE {}",
            c.count_joins, c.cond
        );
        conn.prepare(&text)?
            .query_row(params_from_iter(c.where_params.iter()), |row| {
                row.get::<_, i64>(0)
            })? as u64
    };
    Ok((ids, total))
}

/// Ids of the tracks matching `query`, in order, without counting them.
pub(crate) fn ids(
    conn: &Connection,
    query: &TrackQuery,
    offset: u64,
    limit: u64,
) -> Result<Vec<TrackId>> {
    select_ids(conn, &Compiled::new(query)?, offset, limit)
}

fn select_ids(conn: &Connection, c: &Compiled, offset: u64, limit: u64) -> Result<Vec<TrackId>> {
    let text = c.select_sql();
    let mut params = c.select_params();
    params.push(SqlValue::Integer(clamp(limit)));
    params.push(SqlValue::Integer(clamp(offset)));
    Ok(conn
        .prepare(&text)?
        .query_map(params_from_iter(params.iter()), |row| {
            Ok(TrackId(row.get(0)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Whether the filter compares with [`Value::DaysAgo`], so its result changes with time.
pub(crate) fn is_relative(filter: &Filter) -> bool {
    match filter {
        Filter::And { all: parts } | Filter::Or { any: parts } => parts.iter().any(is_relative),
        Filter::Not { filter } => is_relative(filter),
        Filter::Compare { value, .. } => matches!(value, Value::DaysAgo(_)),
        _ => false,
    }
}

/// `query` with every [`Value::DaysAgo`] replaced by the timestamp it means at `now`, so
/// statements run one after another (a count and a page) agree.
pub(crate) fn pin_time(query: &TrackQuery, now: i64) -> TrackQuery {
    fn walk(filter: &Filter, now: i64) -> Filter {
        match filter {
            Filter::And { all } => Filter::And {
                all: all.iter().map(|f| walk(f, now)).collect(),
            },
            Filter::Or { any } => Filter::Or {
                any: any.iter().map(|f| walk(f, now)).collect(),
            },
            Filter::Not { filter } => Filter::Not {
                filter: Box::new(walk(filter, now)),
            },
            Filter::Compare {
                field,
                op,
                value: Value::DaysAgo(days),
            } => Filter::Compare {
                field: field.clone(),
                op: *op,
                value: Value::Timestamp(now - i64::from(*days) * 86_400_000),
            },
            other => other.clone(),
        }
    }
    TrackQuery {
        filter: walk(&query.filter, now),
        sort: query.sort.clone(),
    }
}

/// Number and total duration (ms) of the tracks matching `query`; with `limit`, of its first
/// `limit` tracks in query order.
pub(crate) fn count_and_duration(
    conn: &Connection,
    query: &TrackQuery,
    limit: Option<u64>,
) -> Result<(u64, u64)> {
    let c = Compiled::new(query)?;
    let (text, params) = match limit {
        None => (
            format!(
                "SELECT COUNT(*), COALESCE(SUM(t.duration_ms), 0) FROM tracks t{} WHERE {}",
                c.count_joins, c.cond
            ),
            c.where_params.clone(),
        ),
        Some(limit) => {
            let mut params = c.select_params();
            params.push(SqlValue::Integer(clamp(limit)));
            (
                format!(
                    "SELECT COUNT(*), COALESCE(SUM(d), 0) FROM (
                        SELECT t.duration_ms AS d FROM tracks t{} WHERE {}
                        ORDER BY {} LIMIT ?)",
                    c.joins, c.cond, c.order
                ),
                params,
            )
        }
    };
    let (count, duration) = conn
        .prepare(&text)?
        .query_row(params_from_iter(params.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
    Ok((count.max(0) as u64, duration.max(0) as u64))
}

fn clamp(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

pub(crate) fn tracks_page(
    conn: &Connection,
    query: &TrackQuery,
    page: Page,
    tags: &[String],
) -> Result<(Vec<TrackRow>, u64)> {
    let (ids, total) = ids_page(conn, query, u64::from(page.offset), u64::from(page.limit))?;
    let keys: Vec<String> = tags.iter().map(|k| tag_key(k)).collect::<Result<_>>()?;
    Ok((reads::track_rows(conn, &ids, &keys)?, total))
}

#[cfg(all(test, feature = "desktop-fs"))]
pub(crate) fn explain_query(conn: &Connection, query: &TrackQuery) -> Result<Vec<String>> {
    let compiled = Compiled::new(query)?;
    let mut params = compiled.select_params();
    params.extend([SqlValue::Integer(1), SqlValue::Integer(0)]);
    Ok(conn
        .prepare(&format!("EXPLAIN QUERY PLAN {}", compiled.select_sql()))?
        .query_map(params_from_iter(params.iter()), |r| r.get(3))?
        .collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape_matches_the_design() {
        let json = r#"{
          "filter": {"type": "and", "all": [
            {"type": "text", "field": {"type": "genre"}, "op": "contains", "text": "jazz"},
            {"type": "compare", "field": {"type": "lossless"}, "op": "eq", "value": {"type": "bool", "value": true}},
            {"type": "or", "any": [
              {"type": "missing", "field": {"type": "last_played_at"}},
              {"type": "compare", "field": {"type": "last_played_at"}, "op": "lt",
               "value": {"type": "days_ago", "value": 30}}
            ]}
          ]},
          "sort": [{"field": {"type": "play_count"}, "descending": true}]
        }"#;
        let query: TrackQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.sort[0].field, Field::PlayCount);
        let Filter::And { all } = &query.filter else {
            panic!("and")
        };
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn values_are_parameters_not_sql() {
        let mut sql = Sql::new();
        let filter = Filter::And {
            all: vec![
                Filter::Text {
                    field: Field::Tag {
                        key: "x'); DROP TABLE tracks; --".into(),
                    },
                    op: TextOp::Contains,
                    text: "'; DELETE FROM tracks; --".into(),
                },
                Filter::Search {
                    text: "\" OR 1=1 --".into(),
                },
            ],
        };
        sql.filter(&filter, 0).unwrap();
        assert!(!sql.text.contains("DROP"));
        assert!(!sql.text.contains("DELETE"));
        assert!(!sql.text.contains("1=1"));
        assert!(sql.params.len() >= 3);
    }

    #[test]
    fn type_errors_name_field_and_op() {
        let mut sql = Sql::new();
        let err = sql
            .filter(
                &Filter::Compare {
                    field: Field::Title,
                    op: CompareOp::Gt,
                    value: Value::Int(3),
                },
                0,
            )
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Title") && message.contains("Gt"),
            "{message}"
        );
        for bad in [
            Filter::Compare {
                field: Field::Lossless,
                op: CompareOp::Lt,
                value: Value::Bool(true),
            },
            Filter::Compare {
                field: Field::AddedAt,
                op: CompareOp::Lt,
                value: Value::Int(5),
            },
            Filter::Text {
                field: Field::Year,
                op: TextOp::Contains,
                text: "19".into(),
            },
            Filter::Compare {
                field: Field::Tag {
                    key: "LABEL".into(),
                },
                op: CompareOp::Eq,
                value: Value::Int(1),
            },
            Filter::Missing {
                field: Field::Tag { key: " ".into() },
            },
        ] {
            assert!(
                matches!(
                    Sql::new().filter(&bad, 0),
                    Err(LibraryError::InvalidQuery(_))
                ),
                "{bad:?}"
            );
        }
        let mut deep = Filter::All;
        for _ in 0..100 {
            deep = Filter::Not {
                filter: Box::new(deep),
            };
        }
        assert!(Sql::new().filter(&deep, 0).is_err());
    }
}

mod compile;
use self::compile::Compiled;
use self::compile::Kind;
use self::compile::Sql;
use self::compile::column;
use self::compile::invalid;
use self::compile::where_clause;

mod facet;
pub(crate) use self::facet::facet;
