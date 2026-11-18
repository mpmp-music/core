use super::*;

impl Loudness for Library {
    fn automatic_loudness_tracks(
        &self,
        after: i64,
    ) -> Result<Vec<mpmp_library::AutomaticLoudnessTrack>, ContractError> {
        Library::automatic_loudness_tracks(self, after, 128).map_err(library_error)
    }

    fn loudness_statistics(&self, track: TrackId) -> Result<Option<Vec<u8>>, ContractError> {
        Library::loudness_statistics(self, mpmp_library::TrackId(track.0)).map_err(library_error)
    }

    fn pending_album_loudness(
        &self,
        album: &AlbumKey,
    ) -> Result<mpmp_library::AlbumLoudnessStatistics, ContractError> {
        Library::pending_album_loudness(self, album).map_err(library_error)
    }

    fn store_automatic_album_loudness(
        &self,
        album: mpmp_library::AlbumLoudnessUpdate,
    ) -> Result<bool, ContractError> {
        Library::store_automatic_album_loudness(self, album).map_err(library_error)
    }

    fn store_automatic_loudness(
        &self,
        tracks: Vec<mpmp_library::LoudnessUpdate>,
    ) -> Result<mpmp_library::LoudnessCommit, ContractError> {
        Library::store_automatic_loudness(self, tracks).map_err(library_error)
    }

    fn supports_loudness_scan(&self) -> bool {
        true
    }

    fn loudness_source(
        &self,
        track: TrackId,
    ) -> Result<Option<mpmp_library::LoudnessSource>, ContractError> {
        Library::loudness_source(self, mpmp_library::TrackId(track.0)).map_err(library_error)
    }

    fn loudness(&self, track: TrackId) -> Result<Option<contract::LoudnessInfo>, ContractError> {
        if Library::track(self, mpmp_library::TrackId(track.0))
            .map_err(library_error)?
            .is_none()
        {
            return Err(track_not_found(track));
        }
        Library::loudness(self, mpmp_library::TrackId(track.0))
            .map_err(library_error)
            .map(|m| {
                m.map(|m| contract::LoudnessInfo {
                    integrated_lufs: m.integrated_lufs,
                    sample_peak: m.sample_peak,
                    album_lufs: m.album_lufs,
                    album_peak: m.album_peak,
                    measured_at_ms: m.measured_at_ms,
                })
            })
    }
}
