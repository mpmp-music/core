use super::*;

impl Browse for Library {
    fn tracks(&self, ids: &[TrackId]) -> Result<Vec<TrackInfo>, ContractError> {
        let rows = self
            .track_rows(&library_ids(ids), &[])
            .map_err(library_error)?;
        Ok(rows.iter().map(track_info).collect())
    }

    fn uri_of(&self, id: TrackId) -> Result<Option<MediaUri>, ContractError> {
        Library::uri_of(self, mpmp_library::TrackId(id.0)).map_err(library_error)
    }

    fn frame_range(&self, id: TrackId) -> Result<Option<mpmp_engine::FrameRange>, ContractError> {
        let range =
            Library::frame_range(self, mpmp_library::TrackId(id.0)).map_err(library_error)?;
        Ok(range.map(|(start, end)| mpmp_engine::FrameRange { start, end }))
    }

    fn context_tracks(&self, context: &PlayContext) -> Result<Vec<TrackId>, ContractError> {
        let ids = match context {
            PlayContext::Tracks { tracks } => return Ok(tracks.clone()),
            PlayContext::Album { key } => match self.album(&AlbumKey(key.clone())) {
                Ok(Some(album)) => album
                    .tracks
                    .iter()
                    .filter(|t| t.available)
                    .map(|t| t.id)
                    .collect(),
                Ok(None) => return Err(album_not_found(key)),
                Err(e) => return Err(library_error(e)),
            },
            PlayContext::Search { text } => self
                .search(text, CONTEXT_LIMIT)
                .map_err(library_error)?
                .tracks
                .iter()
                .map(|t| t.id)
                .collect(),
            PlayContext::Query { query } => self
                .track_ids(&track_query(query, "command.context.query")?, CONTEXT_LIMIT)
                .map_err(library_error)?,
            PlayContext::Playlist { playlist, .. } => self
                .playlist_track_ids(mpmp_library::PlaylistId(playlist.0), CONTEXT_LIMIT)
                .map_err(|e| playlist_error(e, *playlist))?,
            _ => {
                return Err(
                    ContractError::unknown_variant("play context").with_field("command.context")
                );
            }
        };
        Ok(ids.into_iter().map(|id| TrackId(id.0)).collect())
    }

    fn query(&self, query: &Query) -> QueryResult {
        self.run_query(query)
            .unwrap_or_else(|error| QueryResult::Err { error })
    }
}
