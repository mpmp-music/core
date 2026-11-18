//! Conversions between library models and contract models.

use crate::catalog::Recorded;
use crate::contract::{
    self, ActionId, ActionKind, ActionSummary, AlbumSummary, ArtistSummary, CompareOp,
    ContractError, ErrorCode, Field, Filter, GrantId, Origin, PlayContext, QueryResult, RootId,
    ScanErrorCode, SortKey, TextOp, TrackCue, TrackId, TrackInfo, TrackQuery, Value,
};
use mpmp_library::ActionMeta;

use super::errors::library_error;

/// Version of the play context stored with each history row (`play_events.context`).
pub(crate) const HISTORY_CONTEXT_VERSION: u32 = 1;

/// What a history row keeps of the play context: its kind and key, not the track list
/// (a `Tracks` context can hold thousands of ids and would be copied into every row).
/// Versioned (`"v"`), so readers can tell formats apart.
pub(crate) fn context_summary(context: &PlayContext) -> String {
    let v = HISTORY_CONTEXT_VERSION;
    let value = match context {
        PlayContext::Album { key } => serde_json::json!({"v": v, "type": "album", "key": key}),
        PlayContext::Search { text } => {
            serde_json::json!({"v": v, "type": "search", "text": text})
        }
        PlayContext::Tracks { tracks } => {
            serde_json::json!({"v": v, "type": "tracks", "count": tracks.len()})
        }
        // The query itself is small and is what reproduces the context.
        PlayContext::Query { query } => {
            serde_json::json!({"v": v, "type": "query", "query": query})
        }
        PlayContext::Playlist { playlist, .. } => {
            serde_json::json!({"v": v, "type": "playlist", "id": playlist.0})
        }
        _ => serde_json::json!({"v": v, "type": "unknown"}),
    };
    value.to_string()
}

/// Who asks: an agent's locations were checked by core, and the library checks them once
/// more right before it acts on them (`location_changed`).
pub(super) fn action_meta<'a>(
    label: &'a str,
    grant: Option<GrantId>,
    origin: &Origin,
) -> ActionMeta<'a> {
    ActionMeta::new(label)
        .with_grant(grant.map(|g| g.0))
        .with_checked(matches!(origin, Origin::Agent { .. }))
}

pub(super) fn recorded<T>(done: mpmp_library::Done<T>) -> Recorded<T> {
    Recorded::new(done.value, done.action.map(action_id))
}

pub(super) fn action_id(id: mpmp_library::ActionId) -> ActionId {
    ActionId(id.0)
}

pub(super) fn contract_ids(ids: Vec<mpmp_library::TrackId>) -> Vec<TrackId> {
    ids.into_iter().map(|id| TrackId(id.0)).collect()
}

pub(super) fn scan_error_code(code: mpmp_library::ScanErrorCode) -> ScanErrorCode {
    match code {
        mpmp_library::ScanErrorCode::RootUnreachable => ScanErrorCode::RootUnreachable,
        mpmp_library::ScanErrorCode::NoAudioFiles => ScanErrorCode::NoAudioFiles,
        mpmp_library::ScanErrorCode::DirectoryReadFailed => ScanErrorCode::DirectoryReadFailed,
        mpmp_library::ScanErrorCode::InvalidPath => ScanErrorCode::InvalidPath,
        mpmp_library::ScanErrorCode::StatFailed => ScanErrorCode::StatFailed,
        mpmp_library::ScanErrorCode::AudioReadFailed => ScanErrorCode::AudioReadFailed,
        mpmp_library::ScanErrorCode::CueReadFailed => ScanErrorCode::CueReadFailed,
        mpmp_library::ScanErrorCode::ReaderPanicked => ScanErrorCode::ReaderPanicked,
        mpmp_library::ScanErrorCode::CueMissingFiles => ScanErrorCode::CueMissingFiles,
        mpmp_library::ScanErrorCode::Unknown => ScanErrorCode::Unknown,
    }
}

pub(super) fn action_summary(a: &mpmp_library::ActionSummary) -> ActionSummary {
    ActionSummary {
        id: action_id(a.id),
        recorded_at_ms: a.at_ms,
        origin: Origin::from_label(&a.origin),
        kind: ActionKind::parse(&a.kind),
        summary: a.summary.clone(),
        target_count: sat_u32(a.target_count),
        undone_by: a.undone_by.map(action_id),
        undo_of: a.undo_of.map(action_id),
        undoable: a.undoable,
        grant: a.grant.map(GrantId),
        details: a
            .details
            .clone()
            .and_then(|v| serde_json::from_value(v).ok())
            .filter(|details| match details {
                contract::ActionDetails::Unknown
                | contract::ActionDetails::Undo {
                    original_kind: ActionKind::Unknown,
                } => false,
                contract::ActionDetails::TagEdit { changes } => changes
                    .iter()
                    .all(|change| !matches!(change, contract::TagKeyChange::Unknown)),
                _ => true,
            }),
    }
}

pub(crate) fn track_info(row: &mpmp_library::TrackRow) -> TrackInfo {
    let t = &row.track;
    TrackInfo {
        id: TrackId(t.id.0),
        title: t.display_title().to_owned(),
        title_from_file: t.title.is_none(),
        artist: t.artist.clone(),
        artists: t
            .artists
            .iter()
            .map(|a| contract::ArtistRef {
                key: a.key.clone(),
                name: a.name.clone(),
            })
            .collect(),
        artists_source: t.artists_source.and_then(|source| {
            Some(match source {
                mpmp_library::ArtistListSource::ArtistsTag => {
                    contract::ArtistListSource::ArtistsTag
                }
                mpmp_library::ArtistListSource::ArtistValues => {
                    contract::ArtistListSource::ArtistValues
                }
                mpmp_library::ArtistListSource::CuePerformer => {
                    contract::ArtistListSource::CuePerformer
                }
                mpmp_library::ArtistListSource::LegacyDisplay => {
                    contract::ArtistListSource::LegacyDisplay
                }
                mpmp_library::ArtistListSource::None => contract::ArtistListSource::None,
                mpmp_library::ArtistListSource::Unknown => return None,
            })
        }),
        artists_complete: t.artists_complete
            && t.artists_source
                .is_some_and(|source| source != mpmp_library::ArtistListSource::Unknown),
        cover_revision: t.cover_revision.clone(),
        album: t.album.clone(),
        album_artist: t.album_artist.clone(),
        group_artist: t.group_artist.clone(),
        album_key: t.album_key.0.clone(),
        track_no: t.track_no,
        track_total: t.track_total,
        disc_no: t.disc_no,
        disc_total: t.disc_total,
        year: t.year,
        genre: t.genre.clone(),
        compilation: t.compilation,
        duration_ms: t.duration_ms,
        codec: t.codec.as_ref().map(|c| c.to_lowercase()),
        sample_rate: t.sample_rate,
        channels: t.channels.map(u16::from),
        bits_per_sample: t.bits_per_sample.map(u32::from),
        bitrate_kbps: t.bitrate_kbps,
        lossless: t.lossless.unwrap_or(false),
        root: RootId(t.root_id.0),
        path: t.rel_path.clone(),
        size_bytes: t.size,
        modified_at_ms: t.mtime_ms,
        added_at_ms: t.added_at,
        available: t.available,
        has_cover: t.has_embedded_art || t.has_folder_art,
        rating: row.rating,
        loved: row.loved,
        play_count: sat_u32(row.play_count),
        skip_count: sat_u32(row.skip_count),
        last_played_at_ms: row.last_played_at,
        first_played_at_ms: row.first_played_at,
        replay_gain: replay_gain(&t.replay_gain),
        tags: row.tags.clone(),
        cue: t.cue.as_ref().map(|cue| TrackCue {
            sheet: cue.sheet.clone(),
            number: cue.number,
            start_ms: cue.start_ms(),
            // The last track of a file ends with it.
            end_ms: cue
                .end_ms()
                .or_else(|| t.duration_ms.map(|d| cue.start_ms() + d)),
        }),
    }
}

/// A track the library does not know (any more): listed so containers keep every row.
pub(crate) fn placeholder(id: TrackId) -> TrackInfo {
    TrackInfo {
        id,
        available: false,
        ..TrackInfo::default()
    }
}

pub(super) fn album_summary(a: &mpmp_library::AlbumSummary) -> AlbumSummary {
    AlbumSummary {
        key: a.key.0.clone(),
        title: a.title.clone(),
        artist: a.artist.clone(),
        year: a.year,
        track_count: a.track_count,
        disc_count: a.disc_count,
        duration_ms: a.duration_ms,
        art_track: a.art_track.map(|t| TrackId(t.0)),
        added_at_ms: a.added_at,
    }
}

pub(super) fn artist_summary(a: &mpmp_library::ArtistSummary) -> ArtistSummary {
    ArtistSummary {
        key: a.key.clone(),
        name: a.name.clone(),
        album_count: a.album_count,
        track_count: a.track_count,
    }
}

pub(super) fn library_ids(ids: &[TrackId]) -> Vec<mpmp_library::TrackId> {
    ids.iter().map(|id| mpmp_library::TrackId(id.0)).collect()
}

pub(super) fn sat_u32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

// The contract's query types mirror the library's one to one; the contract keeps its own
// copies so the library can change without changing the API. A variant the contract
// decoded as `Unknown` has no library counterpart: `unsupported` / `unknown_variant`.

// Each conversion takes the JSON path of what it converts (from the request's root), for
// `details.field`.

pub(super) fn track_query(
    q: &TrackQuery,
    path: &str,
) -> Result<mpmp_library::TrackQuery, ContractError> {
    Ok(mpmp_library::TrackQuery {
        filter: lib_filter(&q.filter, &format!("{path}.filter"))?,
        sort: q
            .sort
            .iter()
            .enumerate()
            .map(|(i, SortKey { field, descending })| {
                Ok(mpmp_library::SortKey {
                    field: lib_field(field, &format!("{path}.sort[{i}].field"))?,
                    descending: *descending,
                })
            })
            .collect::<Result<_, ContractError>>()?,
    })
}

pub(super) fn lib_filter(f: &Filter, path: &str) -> Result<mpmp_library::Filter, ContractError> {
    use mpmp_library::Filter as L;
    let many = |fs: &[Filter], key: &str| {
        fs.iter()
            .enumerate()
            .map(|(i, f)| lib_filter(f, &format!("{path}.{key}[{i}]")))
            .collect::<Result<Vec<_>, _>>()
    };
    let field = |f: &Field| lib_field(f, &format!("{path}.field"));
    let converted = match f {
        Filter::All => L::All,
        Filter::And { all } => L::And {
            all: many(all, "all")?,
        },
        Filter::Or { any } => L::Or {
            any: many(any, "any")?,
        },
        Filter::Not { filter } => L::Not {
            filter: Box::new(lib_filter(filter, &format!("{path}.filter"))?),
        },
        Filter::Compare {
            field: f,
            op,
            value,
        } => L::Compare {
            field: field(f)?,
            op: match op {
                CompareOp::Eq => mpmp_library::CompareOp::Eq,
                CompareOp::Ne => mpmp_library::CompareOp::Ne,
                CompareOp::Lt => mpmp_library::CompareOp::Lt,
                CompareOp::Le => mpmp_library::CompareOp::Le,
                CompareOp::Gt => mpmp_library::CompareOp::Gt,
                CompareOp::Ge => mpmp_library::CompareOp::Ge,
                _ => {
                    return Err(ContractError::unknown_variant("comparison")
                        .with_field(format!("{path}.op")));
                }
            },
            value: lib_value(value, &format!("{path}.value"))?,
        },
        Filter::Text { field: f, op, text } => L::Text {
            field: field(f)?,
            op: match op {
                TextOp::Equals => mpmp_library::TextOp::Equals,
                TextOp::Contains => mpmp_library::TextOp::Contains,
                TextOp::StartsWith => mpmp_library::TextOp::StartsWith,
                _ => {
                    return Err(ContractError::unknown_variant("text match")
                        .with_field(format!("{path}.op")));
                }
            },
            text: text.clone(),
        },
        Filter::ArtistPerson { name } => L::ArtistPerson { name: name.clone() },
        Filter::Search { text } => L::Search { text: text.clone() },
        Filter::Missing { field: f } => L::Missing { field: field(f)? },
        _ => return Err(ContractError::unknown_variant("filter").with_field(path)),
    };
    // Reuse the library's type checker on leaves, where the request location is still
    // known. Group/whole-query limits remain the responsibility of the normal compiler.
    if !matches!(
        f,
        Filter::And { .. } | Filter::Or { .. } | Filter::Not { .. }
    ) {
        mpmp_library::TrackQuery {
            filter: converted.clone(),
            sort: Vec::new(),
        }
        .validate()
        .map_err(|error| library_error(error).with_field(path))?;
    }
    Ok(converted)
}

pub(super) fn lib_field(f: &Field, path: &str) -> Result<mpmp_library::Field, ContractError> {
    use mpmp_library::Field as L;
    Ok(match f {
        Field::Title => L::Title,
        Field::Artist => L::Artist,
        Field::Album => L::Album,
        Field::AlbumArtist => L::AlbumArtist,
        Field::GroupArtist => L::GroupArtist,
        Field::Genre => L::Genre,
        Field::Year => L::Year,
        Field::TrackNo => L::TrackNo,
        Field::DiscNo => L::DiscNo,
        Field::Compilation => L::Compilation,
        Field::Tag { key } => L::Tag { key: key.clone() },
        Field::DurationMs => L::DurationMs,
        Field::Codec => L::Codec,
        Field::SampleRate => L::SampleRate,
        Field::BitsPerSample => L::BitsPerSample,
        Field::Channels => L::Channels,
        Field::Lossless => L::Lossless,
        Field::BitrateKbps => L::BitrateKbps,
        Field::Path => L::Path,
        Field::Root => L::Root,
        Field::SizeBytes => L::SizeBytes,
        Field::ModifiedAt => L::ModifiedAt,
        Field::AddedAt => L::AddedAt,
        Field::Available => L::Available,
        Field::HasCover => L::HasCover,
        Field::PlayCount => L::PlayCount,
        Field::SkipCount => L::SkipCount,
        Field::LastPlayedAt => L::LastPlayedAt,
        Field::FirstPlayedAt => L::FirstPlayedAt,
        Field::Rating => L::Rating,
        Field::Loved => L::Loved,
        Field::CueSheet => L::CueSheet,
        _ => return Err(ContractError::unknown_variant("field").with_field(path)),
    })
}

fn lib_value(v: &Value, path: &str) -> Result<mpmp_library::Value, ContractError> {
    Ok(match v {
        Value::Int(n) => mpmp_library::Value::Int(*n),
        Value::Bool(b) => mpmp_library::Value::Bool(*b),
        Value::Text(t) => mpmp_library::Value::Text(t.clone()),
        Value::Timestamp(ms) => mpmp_library::Value::Timestamp(*ms),
        Value::DaysAgo(days) => mpmp_library::Value::DaysAgo(*days),
        _ => return Err(ContractError::unknown_variant("value kind").with_field(path)),
    })
}

pub(super) fn contract_value(v: mpmp_library::Value) -> Value {
    match v {
        mpmp_library::Value::Int(n) => Value::Int(n),
        mpmp_library::Value::Bool(b) => Value::Bool(b),
        mpmp_library::Value::Text(t) => Value::Text(t),
        mpmp_library::Value::Timestamp(ms) => Value::Timestamp(ms),
        mpmp_library::Value::DaysAgo(days) => Value::DaysAgo(days),
        // A kind added to the library but not yet to the contract.
        _ => Value::Unknown,
    }
}

pub(super) fn lyrics_info(lyrics: mpmp_library::Lyrics) -> contract::Lyrics {
    use mpmp_library::{LyricsFormat as F, LyricsSource as S};
    contract::Lyrics {
        source: match lyrics.source {
            S::File => contract::LyricsSource::File,
            S::Embedded => contract::LyricsSource::Embedded,
        },
        format: match lyrics.format {
            F::Plain => contract::LyricsFormat::Plain,
            F::Lrc => contract::LyricsFormat::Lrc,
            F::EnhancedLrc => contract::LyricsFormat::EnhancedLrc,
        },
        lines: lyrics
            .lines
            .into_iter()
            .map(|line| contract::LyricsLine {
                start_ms: line.start_ms,
                text: line.text,
                words: line
                    .words
                    .into_iter()
                    .map(|word| contract::LyricsWord {
                        start_ms: word.start_ms,
                        text: word.text,
                    })
                    .collect(),
                translation: line.translation,
            })
            .collect(),
        raw: lyrics.raw,
    }
}

/// Library query → contract query. The two have the same serde shape by design (the
/// contract keeps its own copy), so going through JSON is exact; a kind the contract
/// lacks decodes as `Unknown` and is reported.
pub(super) fn contract_query(q: &mpmp_library::TrackQuery) -> Result<TrackQuery, String> {
    let query: TrackQuery = serde_json::to_value(q)
        .and_then(serde_json::from_value)
        .map_err(|e| format!("query not expressible in this API version: {e}"))?;
    track_query(&query, "query")
        .map(|_| query)
        .map_err(|e| format!("query not expressible in this API version: {}", e.message))
}

fn replay_gain(rg: &mpmp_library::ReplayGain) -> Option<crate::contract::ReplayGainInfo> {
    (rg.track_gain_db.is_some() || rg.album_gain_db.is_some()).then_some(
        crate::contract::ReplayGainInfo {
            track_gain_db: rg.track_gain_db,
            track_peak: rg.track_peak,
            album_gain_db: rg.album_gain_db,
            album_peak: rg.album_peak,
            track_source: if rg.track_measured {
                contract::GainDataSource::Automatic
            } else {
                contract::GainDataSource::Tags
            },
            album_source: if rg.album_measured {
                contract::GainDataSource::Automatic
            } else {
                contract::GainDataSource::Tags
            },
        },
    )
}

pub(super) fn artist_view_value(
    view: Option<contract::ArtistView>,
    path: &str,
) -> Result<mpmp_library::ArtistView, ContractError> {
    match view.unwrap_or_default() {
        contract::ArtistView::AlbumGroup => Ok(mpmp_library::ArtistView::AlbumGroup),
        contract::ArtistView::Person => Ok(mpmp_library::ArtistView::Person),
        contract::ArtistView::Unknown => {
            Err(ContractError::unknown_variant("artist view").with_field(path))
        }
    }
}

pub(crate) fn cover_change(
    change: &contract::CoverChange,
) -> Result<mpmp_library::CoverChange, ContractError> {
    match change {
        contract::CoverChange::Set { image } => mpmp_library::CoverChange::from_base64(
            &image.mime,
            &image.data_base64,
        )
        .map_err(|code| {
            let code = ErrorCode::parse(code);
            ContractError::new(code.kind(), code, "invalid cover image").with_field("change.image")
        }),
        contract::CoverChange::Remove => Ok(mpmp_library::CoverChange::Remove),
        _ => Err(ContractError::unknown_variant("cover change").with_field("change")),
    }
}

fn image_info(i: mpmp_library::CoverImageInfo) -> contract::CoverImageInfo {
    contract::CoverImageInfo {
        mime: i.mime,
        bytes: i.bytes,
        width: i.width,
        height: i.height,
        sha256: i.sha256,
    }
}

pub(super) fn cover_preview(preview: mpmp_library::CoverPreview) -> QueryResult {
    QueryResult::CoverEditPreview {
        files: preview
            .files
            .into_iter()
            .map(|f| contract::CoverFilePreview {
                track: TrackId(f.track.0),
                before: f.before.map(image_info),
                after: f.after.map(image_info),
                changed: f.changed,
                undo_bytes: f.undo_bytes,
                fallback: f.fallback,
                scope: if f.scope == "all_covr" {
                    contract::CoverScope::AllCovr
                } else {
                    contract::CoverScope::FrontCover
                },
            })
            .collect(),
        skipped: cover_results(&preview.skipped),
    }
}

pub(super) fn cover_results(
    files: &[(mpmp_library::TrackId, &'static str)],
) -> Vec<contract::CoverFileResult> {
    files
        .iter()
        .map(|(id, code)| {
            let code = ErrorCode::parse(code);
            contract::CoverFileResult {
                track: TrackId(id.0),
                code,
                message: if code == ErrorCode::CoverEditUnsupported {
                    concat!(
                        "Cannot safely edit embedded covers in this file or filesystem; ",
                        "filesystems with hard links but no atomic exchange (NFS and some CIFS) ",
                        "are unsupported",
                    )
                    .into()
                } else {
                    code.as_str().replace('_', " ")
                },
            }
        })
        .collect()
}
