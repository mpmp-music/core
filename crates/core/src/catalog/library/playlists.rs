use super::CONTEXT_LIMIT;
use crate::catalog::convert::{action_id, action_meta, library_ids, sat_u32, track_query};
use crate::catalog::errors::{is_path_error, library_error, playlist_not_found};
use crate::catalog::playlists::playlist_error;
use crate::catalog::{Browse, PlaylistOutcome, Playlists};
use crate::contract::{
    Command, ContractError, ErrorCode, ErrorKind, GrantId, M3uReport, ObjectRef, Origin,
    PlayContext, PlaylistId, PlaylistItemId, PlaylistKind, PlaylistReceipt, PlaylistRevision,
    TrackId,
};
use mpmp_library::{Library, Page};
use std::path::Path;

impl Playlists for Library {
    fn playlist_playback(
        &self,
        context: &PlayContext,
    ) -> Result<(Vec<TrackId>, Option<usize>), ContractError> {
        let PlayContext::Playlist {
            playlist,
            start_item,
            expected_revision,
        } = context
        else {
            return self.context_tracks(context).map(|tracks| (tracks, None));
        };
        Library::playlist_playback(
            self,
            mpmp_library::PlaylistId(playlist.0),
            CONTEXT_LIMIT,
            start_item.map(|item| mpmp_library::PlaylistItemId(item.0)),
            expected_revision.map(|revision| revision.0),
        )
        .map(|(tracks, index)| (tracks.into_iter().map(|id| TrackId(id.0)).collect(), index))
        .map_err(|error| {
            let error = playlist_error(error, *playlist);
            if error.code == ErrorCode::ItemNotFound {
                ContractError::new(
                    ErrorKind::Conflict,
                    ErrorCode::PlaylistConflict,
                    "the starting playlist item is no longer playable in this context",
                )
                .with_field("command.context.start_item")
                .with_missing(
                    start_item
                        .iter()
                        .map(|id| ObjectRef::PlaylistItem { id: *id })
                        .collect(),
                )
            } else if error.code == ErrorCode::PlaylistConflict {
                error.with_field("command.context.expected_revision")
            } else {
                error
            }
        })
    }

    fn describe_purge(
        &self,
        ids: Option<&[PlaylistId]>,
    ) -> Result<mpmp_library::PurgeGuard, ContractError> {
        let ids = ids.map(|ids| {
            ids.iter()
                .map(|p| mpmp_library::PlaylistId(p.0))
                .collect::<Vec<_>>()
        });
        Library::describe_purge(self, ids.as_deref()).map_err(library_error)
    }

    fn purge_guarded(
        &self,
        guard: mpmp_library::PurgeGuard,
        origin: &Origin,
    ) -> Result<PlaylistOutcome, ContractError> {
        let playlists = guard.playlists.iter().map(|p| PlaylistId(p.id.0)).collect();
        let label = origin.label();
        let result = self
            .purge_playlists_guarded(guard, action_meta(&label, None, origin))
            .map_err(library_error)?;
        Ok(PlaylistOutcome {
            receipt: None,
            playlists,
            changed: result.value > 0,
            action: None,
            audit: result.action.map(action_id),
        })
    }

    fn playlist_command(
        &self,
        command: &Command,
        origin: &Origin,
        grant: Option<GrantId>,
    ) -> Result<PlaylistOutcome, ContractError> {
        let label = origin.label();
        let meta = action_meta(&label, grant, origin);
        let at = |id: mpmp_library::PlaylistId, revision: u64| PlaylistReceipt {
            playlist: PlaylistId(id.0),
            revision: PlaylistRevision(revision),
            created_items: Vec::new(),
            removed_items: Vec::new(),
            import: None,
        };
        let id = |p: &PlaylistId| mpmp_library::PlaylistId(p.0);
        let rev = |r: &Option<PlaylistRevision>| r.map(|r| r.0);
        let revision_of = |p: PlaylistId| -> Result<u64, ContractError> {
            match self.playlist(id(&p)).map_err(library_error)? {
                Some(summary) => Ok(summary.revision),
                None => Err(playlist_not_found(p)),
            }
        };
        // A recorded command changed something exactly when it recorded an action.
        let one =
            |receipt: PlaylistReceipt, action: Option<mpmp_library::ActionId>| PlaylistOutcome {
                playlists: vec![receipt.playlist],
                receipt: Some(receipt),
                changed: action.is_some(),
                action: action.map(action_id),
                audit: None,
            };
        let outcome = match command {
            Command::CreatePlaylist { name, kind } => {
                let done = self
                    .create_playlist(name, lib_kind(kind, "command.kind")?, meta)
                    .map_err(library_error)?;
                one(at(done.value, 0), done.action)
            }
            Command::RenamePlaylist {
                playlist,
                name,
                expected_revision,
            } => {
                let done = self
                    .rename_playlist(id(playlist), name, rev(expected_revision), meta)
                    .map_err(|e| playlist_error(e, *playlist))?;
                one(at(id(playlist), done.value), done.action)
            }
            Command::DeletePlaylist { playlist } => {
                let done = self
                    .delete_playlist(id(playlist), meta)
                    .map_err(|e| playlist_error(e, *playlist))?;
                one(at(id(playlist), revision_of(*playlist)?), done.action)
            }
            Command::RestorePlaylist { playlist } => {
                let done = self
                    .restore_playlist(id(playlist), meta)
                    .map_err(|e| playlist_error(e, *playlist))?;
                one(at(id(playlist), revision_of(*playlist)?), done.action)
            }
            Command::PurgePlaylists { playlists } => {
                let ids: Option<Vec<_>> = playlists.as_ref().map(|ps| ps.iter().map(id).collect());
                let purged = self
                    .purge_playlists(ids.as_deref(), meta)
                    .map_err(library_error)?;
                PlaylistOutcome {
                    receipt: None,
                    // All or nothing: on success every listed playlist is gone.
                    playlists: playlists.clone().unwrap_or_default(),
                    changed: purged.value > 0,
                    action: None,
                    audit: purged.action.map(action_id),
                }
            }
            Command::AddToPlaylist {
                playlist,
                tracks,
                index,
                expected_revision,
            } => {
                let done = self
                    .add_to_playlist(
                        id(playlist),
                        &library_ids(tracks),
                        *index,
                        rev(expected_revision),
                        meta,
                    )
                    .map_err(|e| playlist_error(e, *playlist))?;
                let (items, revision) = done.value;
                one(
                    PlaylistReceipt {
                        created_items: items.into_iter().map(|i| PlaylistItemId(i.0)).collect(),
                        ..at(id(playlist), revision)
                    },
                    done.action,
                )
            }
            Command::RemoveFromPlaylist {
                playlist,
                items,
                expected_revision,
            } => {
                let items: Vec<_> = items
                    .iter()
                    .map(|i| mpmp_library::PlaylistItemId(i.0))
                    .collect();
                let done = self
                    .remove_from_playlist(id(playlist), &items, rev(expected_revision), meta)
                    .map_err(|e| playlist_error(e, *playlist))?;
                let (removed, revision) = done.value;
                one(
                    PlaylistReceipt {
                        removed_items: removed.into_iter().map(|i| PlaylistItemId(i.0)).collect(),
                        ..at(id(playlist), revision)
                    },
                    done.action,
                )
            }
            Command::MovePlaylistItem {
                playlist,
                item,
                to,
                expected_revision,
            } => {
                let done = self
                    .move_playlist_item(
                        id(playlist),
                        mpmp_library::PlaylistItemId(item.0),
                        *to,
                        rev(expected_revision),
                        meta,
                    )
                    .map_err(|e| {
                        let missing = match e.code() {
                            "item_not_found" => vec![ObjectRef::PlaylistItem { id: *item }],
                            _ => Vec::new(),
                        };
                        let error = playlist_error(e, *playlist);
                        if missing.is_empty() {
                            error
                        } else {
                            error.with_missing(missing)
                        }
                    })?;
                one(at(id(playlist), done.value), done.action)
            }
            Command::SetSmartQuery {
                playlist,
                query,
                max_tracks,
                expected_revision,
            } => {
                let done = self
                    .set_smart_query(
                        id(playlist),
                        track_query(query, "command.query")?,
                        *max_tracks,
                        rev(expected_revision),
                        meta,
                    )
                    .map_err(|e| playlist_error(e, *playlist))?;
                one(at(id(playlist), done.value), done.action)
            }
            Command::UndoPlaylist { playlist, revision } => {
                let done = self
                    .undo_playlist(id(playlist), revision.0, meta)
                    .map_err(|e| playlist_error(e, *playlist))?;
                one(at(id(playlist), done.value), done.action)
            }
            Command::ImportM3u { path, name } => {
                let done = self
                    .import_m3u(Path::new(path), name.as_deref(), meta)
                    .map_err(|e| library_error(e).with_field("command.path"))?;
                let (p, report) = done.value;
                // The new playlist's items, for the receipt.
                let (summary, entries) = self
                    .playlist_page(p, Page::new(0, u32::MAX), &[])
                    .map_err(library_error)?
                    .ok_or_else(|| playlist_not_found(PlaylistId(p.0)))?;
                one(
                    PlaylistReceipt {
                        created_items: entries
                            .iter()
                            .filter_map(|e| e.item.map(|i| PlaylistItemId(i.0)))
                            .collect(),
                        import: Some(M3uReport {
                            matched_count: sat_u32(report.matched),
                            missing: report.missing,
                            missing_count: sat_u32(report.missing_total),
                        }),
                        ..at(p, summary.revision)
                    },
                    done.action,
                )
            }
            Command::ExportM3u { playlist, path } => {
                // Core asks agents before they name a path (the command's policy); the UI
                // asks the user itself. It writes a file, not user data: replacing a file,
                // and any file an agent writes, leaves an audit entry.
                let agent = matches!(origin, Origin::Agent { .. });
                let written = self
                    .export_m3u(id(playlist), Path::new(path), true, agent, meta)
                    // Every error about the file names the path (a folder there:
                    // `not_a_regular_file`; checked local file access), as for an import.
                    .map_err(|e| match playlist_error(e, *playlist) {
                        e if is_path_error(e.code) => e.with_field("command.path"),
                        e => e,
                    })?;
                PlaylistOutcome {
                    changed: true,
                    audit: written.action.map(action_id),
                    ..one(at(id(playlist), revision_of(*playlist)?), None)
                }
            }
            _ => {
                return Err(ContractError::invalid(
                    ErrorCode::InvalidRequest,
                    "not a playlist command",
                ));
            }
        };
        Ok(outcome)
    }
}

fn lib_kind(kind: &PlaylistKind, path: &str) -> Result<mpmp_library::PlaylistKind, ContractError> {
    Ok(match kind {
        PlaylistKind::Manual => mpmp_library::PlaylistKind::Manual,
        PlaylistKind::Smart { query, max_tracks } => mpmp_library::PlaylistKind::Smart {
            query: track_query(query, &format!("{path}.query"))?,
            max_tracks: *max_tracks,
        },
        _ => return Err(ContractError::unknown_variant("playlist kind").with_field(path)),
    })
}
