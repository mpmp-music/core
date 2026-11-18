//! Library query execution and album paging.

use crate::catalog::check_tracks_exist;
use crate::contract::{
    self, AlbumDetail, AlbumSort, ContractError, EndReason, ErrorCode, FacetValue, HistoryItem,
    PlayEventId, PlaylistEntry, PlaylistItemId, Query, QueryResult, RootId, RootInfo,
    SearchResults, TagFilePreview, TrackId, TrackInfo, next_offset,
};
use mpmp_library::{AlbumKey, Library, Page};
use std::collections::HashMap;

use super::convert::{
    action_summary, album_summary, artist_summary, artist_view_value, contract_value, cover_change,
    cover_preview, lib_field, lib_filter, library_ids, placeholder, sat_u32, track_info,
    track_query,
};
use super::errors::{album_not_found, library_error, playlist_not_found};
use super::library::{check_cover_tracks, check_tag_tracks, tag_changes, tag_results};
use super::playlists::{playlist_error, playlist_summary};

pub(super) trait RunQuery {
    fn run_query(&self, query: &Query) -> Result<QueryResult, ContractError>;
    /// Full tracks (statistics, user data, `tags`) for `ids`, in order; ids the library
    /// does not know get a placeholder, so containers never drop rows.
    fn infos(
        &self,
        ids: &[mpmp_library::TrackId],
        tags: &[String],
    ) -> Result<Vec<TrackInfo>, ContractError>;
}

impl RunQuery for Library {
    fn infos(
        &self,
        ids: &[mpmp_library::TrackId],
        tags: &[String],
    ) -> Result<Vec<TrackInfo>, ContractError> {
        let rows = self.track_rows(ids, tags).map_err(library_error)?;
        let by_id: HashMap<_, _> = rows.iter().map(|r| (r.track.id, r)).collect();
        Ok(ids
            .iter()
            .map(|id| match by_id.get(id) {
                Some(row) => track_info(row),
                None => placeholder(TrackId(id.0)),
            })
            .collect())
    }

    fn run_query(&self, query: &Query) -> Result<QueryResult, ContractError> {
        Ok(match query {
            Query::CoverEditPreview { tracks, change } => {
                check_cover_tracks(tracks)?;
                let change = cover_change(change)?;
                let preview = self
                    .preview_cover_edit(&library_ids(tracks), &change)
                    .map_err(library_error)?;
                cover_preview(preview)
            }
            Query::TagEditPreview { tracks, changes } => {
                check_tag_tracks(tracks, "tracks")?;
                let changes = tag_changes(changes, "")?;
                check_tracks_exist(self, tracks, "tracks")?;
                let preview = self
                    .preview_tag_edit(&library_ids(tracks), &changes)
                    .map_err(library_error)?;
                QueryResult::TagEditPreview {
                    files: preview
                        .files
                        .into_iter()
                        .map(|f| TagFilePreview {
                            changed: Some(f.changed),
                            track: TrackId(f.track.0),
                            encoding_conversion: Some(f.encoding_conversion),
                            display_location: Some(f.display_location),
                            before: f.before,
                            after: f.after,
                        })
                        .collect(),
                    skipped: tag_results(&preview.skipped),
                }
            }
            Query::Search {
                artist_view,
                text,
                kind,
                offset,
                limit,
            } => {
                if kind.is_none() && *offset != 0 {
                    return Err(ContractError::invalid(
                        ErrorCode::OutOfRange,
                        "mixed search requires offset zero; select kind to page",
                    )
                    .with_field("query.offset"));
                }
                let kind = match kind {
                    None => None,
                    Some(contract::SearchKind::Tracks) => Some(mpmp_library::SearchKind::Tracks),
                    Some(contract::SearchKind::Albums) => Some(mpmp_library::SearchKind::Albums),
                    Some(contract::SearchKind::Artists) => Some(mpmp_library::SearchKind::Artists),
                    Some(contract::SearchKind::Unknown) => {
                        return Err(
                            ContractError::unknown_variant("search kind").with_field("kind")
                        );
                    }
                };
                let found = self
                    .search_page_view(
                        text,
                        kind,
                        Page::new(*offset, *limit),
                        artist_view_value(*artist_view, "artist_view")?,
                    )
                    .map_err(library_error)?;
                let ids: Vec<_> = found.tracks.iter().map(|t| t.id).collect();
                QueryResult::Search {
                    results: SearchResults {
                        total: sat_u32(match kind {
                            Some(mpmp_library::SearchKind::Tracks) => found.tracks_total,
                            Some(mpmp_library::SearchKind::Albums) => found.albums_total,
                            Some(mpmp_library::SearchKind::Artists) => found.artists_total,
                            None => found
                                .tracks_total
                                .saturating_add(found.albums_total)
                                .saturating_add(found.artists_total),
                        }),
                        offset: *offset,
                        next_offset: match kind {
                            Some(mpmp_library::SearchKind::Tracks) => next_offset(
                                *offset,
                                found.tracks.len(),
                                sat_u32(found.tracks_total),
                            ),
                            Some(mpmp_library::SearchKind::Albums) => next_offset(
                                *offset,
                                found.albums.len(),
                                sat_u32(found.albums_total),
                            ),
                            Some(mpmp_library::SearchKind::Artists) => next_offset(
                                *offset,
                                found.artists.len(),
                                sat_u32(found.artists_total),
                            ),
                            None => None,
                        },
                        tracks: self.infos(&ids, &[])?,
                        albums: found.albums.iter().map(album_summary).collect(),
                        artists: found.artists.iter().map(artist_summary).collect(),
                        tracks_total: sat_u32(found.tracks_total),
                        albums_total: sat_u32(found.albums_total),
                        artists_total: sat_u32(found.artists_total),
                    },
                }
            }
            Query::Albums {
                sort,
                offset,
                limit,
            } => {
                let paged = self
                    .albums(album_sort(*sort, "sort")?, Page::new(*offset, *limit))
                    .map_err(library_error)?;
                albums_page(paged, *offset)
            }
            Query::ArtistAlbums {
                view,
                name,
                sort,
                offset,
                limit,
            } => {
                let sort = album_sort(*sort, "sort")?;
                let page = Page::new(*offset, *limit);
                let paged = match artist_view_value(*view, "view")? {
                    mpmp_library::ArtistView::Person => self.albums_by_person(name, sort, page),
                    mpmp_library::ArtistView::AlbumGroup => self.albums_by_artist(name, sort, page),
                }
                .map_err(library_error)?;
                albums_page(paged, *offset)
            }
            Query::Album { key, offset, limit } => {
                let key = AlbumKey(key.clone());
                let album = if offset.is_none() && limit.is_none() {
                    self.album(&key)
                } else {
                    self.album_page(
                        &key,
                        Page::new(
                            offset.unwrap_or(0),
                            limit.unwrap_or(contract::MAX_TRACK_PAGE),
                        ),
                    )
                }
                .map_err(library_error)?
                .ok_or_else(|| album_not_found(key.as_str()))?;
                let offset = offset.unwrap_or(0);
                let ids: Vec<_> = album.tracks.iter().map(|t| t.id).collect();
                QueryResult::Album {
                    album: AlbumDetail {
                        total: album.summary.track_count,
                        offset,
                        next_offset: next_offset(offset, ids.len(), album.summary.track_count),
                        summary: album_summary(&album.summary),
                        tracks: self.infos(&ids, &[])?,
                    },
                }
            }
            Query::Artists {
                view,
                offset,
                limit,
            } => {
                let page = Page::new(*offset, *limit);
                let (paged, indexing) = match artist_view_value(*view, "view")? {
                    mpmp_library::ArtistView::Person => {
                        let (paged, indexing) = self.person_artists(page).map_err(library_error)?;
                        (paged, Some(indexing))
                    }
                    mpmp_library::ArtistView::AlbumGroup => {
                        (self.artists(page).map_err(library_error)?, None)
                    }
                };
                let total = sat_u32(paged.total);
                QueryResult::Artists {
                    indexing,
                    next_offset: next_offset(*offset, paged.items.len(), total),
                    artists: paged.items.iter().map(artist_summary).collect(),
                    offset: *offset,
                    total,
                }
            }
            Query::History { offset, limit } => {
                let paged = self
                    .history(Page::new(*offset, *limit))
                    .map_err(library_error)?;
                let ids: Vec<_> = paged.items.iter().map(|e| e.event.track_id).collect();
                let tracks = self.infos(&ids, &[])?;
                let total = sat_u32(paged.total);
                QueryResult::History {
                    next_offset: next_offset(*offset, paged.items.len(), total),
                    items: paged
                        .items
                        .iter()
                        .zip(tracks)
                        .map(|(e, track)| HistoryItem {
                            id: PlayEventId(e.id.0),
                            track,
                            started_at_ms: e.event.started_at,
                            played_ms: e.event.played_ms,
                            ended: match e.event.ended_reason {
                                mpmp_library::EndedReason::Completed => EndReason::Completed,
                                mpmp_library::EndedReason::Skipped => EndReason::Skipped,
                                mpmp_library::EndedReason::Stopped => EndReason::Stopped,
                                mpmp_library::EndedReason::Failed => EndReason::Failed,
                                _ => EndReason::Unknown,
                            },
                        })
                        .collect(),
                    offset: *offset,
                    total,
                }
            }
            Query::Roots => QueryResult::Roots {
                roots: self
                    .roots()
                    .map_err(library_error)?
                    .iter()
                    .map(|r| RootInfo {
                        id: RootId(r.id.0),
                        path: r.path.display().to_string(),
                        track_count: sat_u32(r.track_count),
                        released: self.root_released(r.id),
                    })
                    .collect(),
            },
            Query::TrackRows { tracks, tags } => {
                if tracks.len() > crate::contract::MAX_TRACK_PAGE as usize {
                    return Err(ContractError::invalid(
                        ErrorCode::OutOfRange,
                        "too many track IDs",
                    ));
                }
                let rows = self
                    .track_rows(&library_ids(tracks), tags)
                    .map_err(library_error)?;
                QueryResult::TrackRows {
                    tracks: rows.iter().map(track_info).collect(),
                }
            }
            Query::Tracks {
                query,
                offset,
                limit,
                tags,
            } => {
                let (rows, total) = self
                    .tracks_query(
                        &track_query(query, "query")?,
                        Page::new(*offset, *limit),
                        tags,
                    )
                    .map_err(library_error)?;
                let total = sat_u32(total);
                QueryResult::Tracks {
                    next_offset: next_offset(*offset, rows.len(), total),
                    tracks: rows.iter().map(track_info).collect(),
                    offset: *offset,
                    total,
                }
            }
            Query::Facet {
                field,
                filter,
                offset,
                limit,
            } => {
                let (values, total) = self
                    .facet(
                        &lib_field(field, "field")?,
                        &lib_filter(filter, "filter")?,
                        Page::new(*offset, *limit),
                    )
                    .map_err(library_error)?;
                let total = sat_u32(total);
                QueryResult::Facet {
                    next_offset: next_offset(*offset, values.len(), total),
                    values: values
                        .into_iter()
                        .map(|v| FacetValue {
                            value: v.value.map(contract_value),
                            track_count: sat_u32(v.tracks),
                        })
                        .collect(),
                    offset: *offset,
                    total,
                }
            }
            Query::Playlists {
                deleted,
                offset,
                limit,
            } => {
                let (playlists, total) = self
                    .playlists(*deleted, Page::new(*offset, *limit))
                    .map_err(library_error)?;
                let total = sat_u32(total);
                QueryResult::Playlists {
                    next_offset: next_offset(*offset, playlists.len(), total),
                    playlists: playlists.iter().map(playlist_summary).collect(),
                    offset: *offset,
                    total,
                }
            }
            Query::PlaylistItems {
                playlist,
                offset,
                limit,
                tags,
            } => {
                let id = mpmp_library::PlaylistId(playlist.0);
                let (summary, entries) = self
                    .playlist_page(id, Page::new(*offset, *limit), tags)
                    .map_err(|e| playlist_error(e, *playlist))?
                    .ok_or_else(|| playlist_not_found(*playlist))?;
                let total = sat_u32(summary.track_count);
                QueryResult::PlaylistItems {
                    next_offset: next_offset(*offset, entries.len(), total),
                    playlist: playlist_summary(&summary),
                    items: entries
                        .iter()
                        .map(|e| PlaylistEntry {
                            item: e.item.map(|i| PlaylistItemId(i.0)),
                            track: track_info(&e.row),
                        })
                        .collect(),
                    offset: *offset,
                    total,
                }
            }
            Query::Actions {
                origin,
                offset,
                limit,
            } => {
                let prefix = match origin {
                    None => None,
                    Some(filter) => Some(filter.label_prefix().ok_or_else(|| {
                        ContractError::unknown_variant("origin filter").with_field("origin")
                    })?),
                };
                let paged = self
                    .actions(prefix.as_deref(), Page::new(*offset, *limit))
                    .map_err(library_error)?;
                let total = sat_u32(paged.total);
                QueryResult::Actions {
                    next_offset: next_offset(*offset, paged.items.len(), total),
                    actions: paged.items.iter().map(action_summary).collect(),
                    offset: *offset,
                    total,
                }
            }
            Query::Snapshot | Query::QueuePage { .. } => {
                return Err(ContractError::invalid(
                    ErrorCode::InvalidRequest,
                    "state queries are answered by core, not the library",
                ));
            }
            _ => {
                return Err(ContractError::unsupported(
                    ErrorCode::UnknownQuery,
                    "this core does not know that query",
                ));
            }
        })
    }
}

fn albums_page(paged: mpmp_library::Paged<mpmp_library::AlbumSummary>, offset: u32) -> QueryResult {
    let total = sat_u32(paged.total);
    QueryResult::Albums {
        next_offset: next_offset(offset, paged.items.len(), total),
        albums: paged.items.iter().map(album_summary).collect(),
        offset,
        total,
    }
}

fn album_sort(sort: AlbumSort, path: &str) -> Result<mpmp_library::AlbumSort, ContractError> {
    Ok(match sort {
        AlbumSort::Artist => mpmp_library::AlbumSort::Artist,
        AlbumSort::Title => mpmp_library::AlbumSort::Title,
        AlbumSort::Year => mpmp_library::AlbumSort::Year,
        AlbumSort::TitleDesc => mpmp_library::AlbumSort::TitleDesc,
        AlbumSort::ArtistDesc => mpmp_library::AlbumSort::ArtistDesc,
        AlbumSort::YearDesc => mpmp_library::AlbumSort::YearDesc,
        AlbumSort::OldestAdded => mpmp_library::AlbumSort::OldestAdded,
        AlbumSort::RecentlyAdded => mpmp_library::AlbumSort::RecentlyAdded,
        _ => return Err(ContractError::unknown_variant("album sort").with_field(path)),
    })
}
