use super::*;

impl MediaResources for Library {
    fn cover_art(&self, track: TrackId) -> Result<Option<CoverArt>, ContractError> {
        Library::cover_art(self, mpmp_library::TrackId(track.0))
            .map(|art| {
                art.map(|a| CoverArt {
                    mime: a.mime,
                    data: a.data,
                })
            })
            .map_err(library_error)
    }

    fn cover_thumbnail(
        &self,
        track: TrackId,
        max_edge: u32,
    ) -> Result<Option<CoverArt>, ContractError> {
        Library::cover_thumbnail(self, mpmp_library::TrackId(track.0), max_edge)
            .map(|art| {
                art.map(|a| CoverArt {
                    mime: a.mime,
                    data: a.data,
                })
            })
            .map_err(library_error)
    }

    fn lyrics(&self, track: TrackId) -> Result<Option<contract::Lyrics>, ContractError> {
        let id = mpmp_library::TrackId(track.0);
        // The library answers `None` for an unknown track as for one without lyrics.
        if Library::uri_of(self, id).map_err(library_error)?.is_none() {
            return Err(track_not_found(track));
        }
        Ok(Library::lyrics(self, id)
            .map_err(library_error)?
            .map(lyrics_info))
    }

    fn waveform(&self, track: TrackId) -> Result<Option<contract::Waveform>, ContractError> {
        let id = mpmp_library::TrackId(track.0);
        // The library answers `None` for an unknown track as for one not computed yet.
        if Library::track(self, id).map_err(library_error)?.is_none() {
            return Err(track_not_found(track));
        }
        Ok(Library::waveform(self, id)
            .map_err(library_error)?
            .map(|w| contract::Waveform {
                buckets: w.buckets,
                channels: w
                    .channels
                    .into_iter()
                    .map(|c| contract::WaveformChannel {
                        min: c.min,
                        max: c.max,
                        rms: c.rms,
                    })
                    .collect(),
            }))
    }

    fn waveform_accessible(&self, track: TrackId) -> bool {
        Library::track(self, mpmp_library::TrackId(track.0))
            .ok()
            .flatten()
            .is_some_and(|t| !self.root_released(t.root_id))
    }

    fn file_version(&self, track: TrackId) -> Result<Option<FileVersion>, ContractError> {
        Ok(Library::file_version(self, mpmp_library::TrackId(track.0))
            .map_err(library_error)?
            .map(|v| FileVersion {
                size: v.size,
                mtime_ms: v.mtime_ms,
            }))
    }

    fn store_waveform(
        &self,
        track: TrackId,
        from: FileVersion,
        waveform: &contract::Waveform,
    ) -> Result<bool, ContractError> {
        let waveform = mpmp_library::Waveform {
            buckets: waveform.buckets,
            channels: waveform
                .channels
                .iter()
                .map(|c| mpmp_library::WaveformChannel {
                    min: c.min.clone(),
                    max: c.max.clone(),
                    rms: c.rms.clone(),
                })
                .collect(),
        };
        let from = mpmp_library::FileVersion {
            size: from.size,
            mtime_ms: from.mtime_ms,
        };
        Library::store_waveform(self, mpmp_library::TrackId(track.0), from, &waveform)
            .map_err(library_error)
    }
}
