//! Track filters, values, sorting, and facets.

use super::*;

/// A field of a track that queries can filter, sort and group by. Time fields are named
/// without the `_ms` of the matching `TrackInfo` fields (`added_at` ↔ `added_at_ms`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Field {
    Title,
    /// The artist as one display string (`TrackInfo.artist`). For any single value of a
    /// multi-valued tag use `tag` with key `ARTIST`.
    Artist,
    Album,
    /// The album artist as one display string (`TrackInfo.album_artist`). For any single
    /// value of a multi-valued tag use `tag` with key `ALBUMARTIST`.
    AlbumArtist,
    /// The persisted album/artist grouping name: album artist, else Various Artists for
    /// compilations, else track artist. Text comparisons use normalized search keys.
    GroupArtist,
    /// The genre as one display string (`TrackInfo.genre`). For any single value of a
    /// multi-valued tag use `tag` with key `GENRE`.
    Genre,
    Year,
    TrackNo,
    DiscNo,
    Compilation,
    /// Any tag by its key, case-insensitive (e.g. "COMPOSER", "LABEL"). Multi-valued tags
    /// match if any value does.
    Tag {
        key: String,
    },
    DurationMs,
    /// Lower-case codec id (`flac`, `mp3`, `aac`, `alac`, `vorbis`, `opus`, `pcm` …).
    Codec,
    SampleRate,
    BitsPerSample,
    Channels,
    Lossless,
    BitrateKbps,
    /// The location relative to its library root (`TrackInfo.path`).
    Path,
    Root,
    SizeBytes,
    ModifiedAt,
    AddedAt,
    /// Mentioning it anywhere in a filter turns off the default "available only".
    Available,
    HasCover,
    /// Plays by the Last.fm rule: at least min(half the duration, 4 minutes) actually
    /// heard (seeking past audio does not count it).
    PlayCount,
    SkipCount,
    LastPlayedAt,
    FirstPlayedAt,
    /// 1–5, or missing when unrated.
    Rating,
    Loved,
    /// Boolean: a track of a CUE sheet (`TrackInfo.cue` is set).
    CueSheet,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// A typed value; JSON `{"type":"int","value":5}`. A value kind this build does not know
/// decodes as `Unknown` (whatever its `value`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, JsonSchema)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Value {
    Int(i64),
    Bool(bool),
    Text(String),
    /// Unix time, milliseconds.
    Timestamp(i64),
    /// A time this many days before the query runs; saved queries stay relative to now.
    DaysAgo(u32),
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

impl<'de> Deserialize<'de> for Value {
    // serde's `other` on an adjacently tagged enum only accepts a missing `value`; a newer
    // kind with a value must still decode (as `Unknown`), so the tag is looked at first.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Lenient like every other type (values also travel core → client); the JSON
        // adapters reject fields they do not know.
        #[derive(Deserialize)]
        struct Raw {
            #[serde(rename = "type")]
            kind: String,
            #[serde(default)]
            value: serde_json::Value,
        }
        #[derive(Deserialize)]
        #[serde(tag = "type", content = "value", rename_all = "snake_case")]
        enum Known {
            Int(i64),
            Bool(bool),
            Text(String),
            Timestamp(i64),
            DaysAgo(u32),
        }
        let raw = Raw::deserialize(deserializer)?;
        if !matches!(
            raw.kind.as_str(),
            "int" | "bool" | "text" | "timestamp" | "days_ago"
        ) {
            return Ok(Value::Unknown);
        }
        let known: Known = serde_json::from_value(serde_json::json!({
            "type": raw.kind,
            "value": raw.value,
        }))
        .map_err(serde::de::Error::custom)?;
        Ok(match known {
            Known::Int(n) => Value::Int(n),
            Known::Bool(b) => Value::Bool(b),
            Known::Text(t) => Value::Text(t),
            Known::Timestamp(ms) => Value::Timestamp(ms),
            Known::DaysAgo(days) => Value::DaysAgo(days),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// Text matching is case-insensitive and ignores accents and width (Unicode
/// compatibility decomposition, accents removed, lower-cased).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TextOp {
    Equals,
    Contains,
    StartsWith,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// Size limits: at most 500 parts in one `and`/`or`, 2000 nodes, 64 levels.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
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
    /// Compares a field's value; text fields compare their folded search keys. A track
    /// without a value never satisfies a comparison, except `ne`.
    Compare {
        field: Field,
        op: CompareOp,
        value: Value,
    },
    Text {
        field: Field,
        op: TextOp,
        text: String,
    },
    /// Exact normalized person-name membership.
    ArtistPerson {
        name: String,
    },
    /// Full-text search over title, artist, album, album artist and genre.
    Search {
        text: String,
    },
    /// The field has no value (or an empty one).
    Missing {
        field: Field,
    },
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct SortKey {
    pub field: Field,
    #[serde(default)]
    pub descending: bool,
}

/// Which tracks, in which order. Ties are broken by album artist, album, disc, track
/// and path, so pages never overlap or skip. Missing values sort last either way.
/// Unless the filter mentions `available`, only available tracks match.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct TrackQuery {
    /// Default: all tracks.
    #[serde(default)]
    pub filter: Filter,
    #[serde(default)]
    pub sort: Vec<SortKey>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FacetValue {
    /// `null` is the group of tracks without a value.
    pub value: Option<Value>,
    pub track_count: u32,
}
