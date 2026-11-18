//! Playlist model and error adapters.

use super::convert::{contract_query, sat_u32};
use super::errors::library_error;
use crate::contract::{
    ContractError, ErrorCode, ObjectRef, PlaylistId, PlaylistKind, PlaylistRevision,
    PlaylistSummary, TrackQuery,
};
use mpmp_library::LibraryError;

/// [`library_error`] for a command on `playlist`: an unknown playlist names it.
pub(super) fn playlist_error(e: LibraryError, playlist: PlaylistId) -> ContractError {
    let error = library_error(e);
    if error.code == ErrorCode::PlaylistNotFound {
        error.with_missing(vec![ObjectRef::Playlist { id: playlist }])
    } else {
        error
    }
}

pub(super) fn playlist_summary(p: &mpmp_library::PlaylistSummary) -> PlaylistSummary {
    let mut query_error = p.query_error.clone();
    let kind = match &p.kind {
        mpmp_library::PlaylistKind::Smart { query, max_tracks } => PlaylistKind::Smart {
            query: contract_query(query).unwrap_or_else(|e| {
                query_error.get_or_insert(e);
                TrackQuery::default()
            }),
            max_tracks: *max_tracks,
        },
        _ => PlaylistKind::Manual,
    };
    PlaylistSummary {
        id: PlaylistId(p.id.0),
        name: p.name.clone(),
        kind,
        track_count: sat_u32(p.track_count),
        duration_ms: p.duration_ms,
        revision: PlaylistRevision(p.revision),
        created_at_ms: p.created_at,
        updated_at_ms: p.updated_at,
        deleted_at_ms: p.deleted_at,
        query_error,
    }
}
