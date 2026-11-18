use super::*;
use crate::db::reads;

impl Library {
    // ---- reads (read pool; never wait for the writer) ---

    pub fn track(&self, id: TrackId) -> Result<Option<Track>> {
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let track = reads::track(&tx, id)?;
            tx.commit()?;
            Ok(track)
        })
    }

    /// Tracks in the order of `ids`; unknown ids are left out.
    pub fn tracks(&self, ids: &[TrackId]) -> Result<Vec<Track>> {
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let tracks = reads::tracks(&tx, ids)?;
            tx.commit()?;
            Ok(tracks)
        })
    }

    /// Searches titles, artists, albums, album artists and genres of available tracks.
    ///
    /// The text is normalized and split on whitespace; every term must match (in any field
    /// for tracks, in album title or album artist for albums, in the album-artist name for
    /// artists). Each of the three lists holds at most `limit` entries; the `*_total`
    /// fields count every match. Lists and totals come from one snapshot.
    pub fn search(&self, text: &str, limit: usize) -> Result<SearchResults> {
        self.search_page(
            text,
            None,
            Page::new(0, u32::try_from(limit).unwrap_or(u32::MAX)),
        )
    }

    /// Search one kind, or all kinds, in stable relevance order from one snapshot.
    pub fn search_page(
        &self,
        text: &str,
        kind: Option<crate::types::SearchKind>,
        page: Page,
    ) -> Result<SearchResults> {
        self.search_page_view(text, kind, page, crate::ArtistView::AlbumGroup)
    }

    /// Select person-name results without changing track or album search semantics.
    pub fn search_page_view(
        &self,
        text: &str,
        kind: Option<crate::SearchKind>,
        page: Page,
        view: crate::ArtistView,
    ) -> Result<SearchResults> {
        self.inner.check_open()?;
        let Some(query) = SearchQuery::parse(text) else {
            return Ok(SearchResults::default());
        };
        self.inner.pool.read(|conn| {
            // One read transaction: all lists and totals come from the same snapshot.
            let tx = conn.unchecked_transaction()?;
            let tracks = if kind.is_none() || kind == Some(crate::types::SearchKind::Tracks) {
                reads::search_tracks(&tx, &query, page)?
            } else {
                Vec::new()
            };
            #[cfg(all(test, feature = "desktop-fs"))]
            self.inner.between_reads();
            let albums = if kind.is_none() || kind == Some(crate::types::SearchKind::Albums) {
                reads::search_albums(&tx, &query, page)?
            } else {
                Vec::new()
            };
            let person = if view == crate::ArtistView::Person
                && (kind.is_none() || kind == Some(crate::SearchKind::Artists))
            {
                Some(reads::person_artists(&tx, page, Some(&query))?)
            } else {
                None
            };
            let artists = if kind.is_none() || kind == Some(crate::types::SearchKind::Artists) {
                match &person {
                    Some(person) => person.items.clone(),
                    None => reads::search_artists(&tx, &query, page)?,
                }
            } else {
                Vec::new()
            };
            let (tracks_total, albums_total, artists_total) = reads::search_totals(&tx, &query)?;
            let artists_total = person.map_or(artists_total, |p| p.total);
            tx.commit()?;
            Ok(SearchResults {
                tracks,
                albums,
                artists,
                tracks_total,
                albums_total,
                artists_total,
            })
        })
    }

    /// A page of the albums of available tracks and how many there are, from one snapshot.
    /// Tracks without an album tag are listed as one unknown album (empty title) per album
    /// artist.
    pub fn albums(&self, sort: AlbumSort, page: Page) -> Result<Paged<AlbumSummary>> {
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let items = reads::albums(&tx, sort, page)?;
            #[cfg(all(test, feature = "desktop-fs"))]
            self.inner.between_reads();
            let total = reads::count(&tx, "albums", None)?;
            tx.commit()?;
            Ok(Paged { items, total })
        })
    }

    /// A page of the albums of one artist, `name` as in [`ArtistSummary::name`] (compared
    /// normalized), and how many there are, from one snapshot.
    pub fn albums_by_artist(
        &self,
        name: &str,
        sort: AlbumSort,
        page: Page,
    ) -> Result<Paged<AlbumSummary>> {
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let items = reads::albums_by_artist(&tx, name, sort, page)?;
            let total = reads::count(&tx, "albums", Some(name))?;
            tx.commit()?;
            Ok(Paged { items, total })
        })
    }

    /// An album with its available tracks ordered by disc, track number and path.
    pub fn album(&self, key: &AlbumKey) -> Result<Option<Album>> {
        self.album_page(key, Page::new(0, u32::MAX))
    }

    /// Available album tracks in stable disc/track/path/ID order, with the full summary.
    pub fn album_page(&self, key: &AlbumKey, page: Page) -> Result<Option<Album>> {
        self.inner.pool.read(|conn| {
            // One read transaction: the summary and the track list come from the same snapshot.
            let tx = conn.unchecked_transaction()?;
            let Some(summary) = reads::album_summary(&tx, key)? else {
                return Ok(None);
            };
            #[cfg(all(test, feature = "desktop-fs"))]
            self.inner.between_reads();
            let tracks = reads::album_tracks(&tx, key, page)?;
            tx.commit()?;
            Ok(Some(Album { summary, tracks }))
        })
    }

    /// A page of the album artists of available tracks (see [`ArtistSummary`]), by name,
    /// and how many there are, from one snapshot.
    pub fn artists(&self, page: Page) -> Result<Paged<ArtistSummary>> {
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let items = reads::artists(&tx, page)?;
            let total = reads::count(&tx, "artists", None)?;
            tx.commit()?;
            Ok(Paged { items, total })
        })
    }

    /// A person page and its index status from one snapshot.
    pub fn person_artists(&self, page: Page) -> Result<(Paged<ArtistSummary>, bool)> {
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let items = reads::person_artists(&tx, page, None)?;
            let indexing = crate::person::indexing(&tx)?;
            tx.commit()?;
            Ok((items, indexing))
        })
    }

    /// Participating albums; summaries retain their existing whole-album meaning.
    /// Use ArtistPerson together with an album filter to page only this person's tracks.
    pub fn albums_by_person(
        &self,
        name: &str,
        sort: AlbumSort,
        page: Page,
    ) -> Result<Paged<AlbumSummary>> {
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let page = reads::person_albums(&tx, name, sort, page)?;
            tx.commit()?;
            Ok(page)
        })
    }

    /// Where to play a track from. `None` for an unknown id. Unavailable tracks still get
    /// their last known location.
    pub fn uri_of(&self, id: TrackId) -> Result<Option<MediaUri>> {
        if self.track(id)?.is_none() {
            return Ok(None);
        }
        let _access = self.file_lease(id)?;
        let Some((root, rel_path)) = self.inner.pool.read(|conn| reads::location(conn, id))? else {
            return Ok(None);
        };
        Ok(Some(MediaUri::from_path(join_relative(&root, &rel_path))?))
    }

    /// The part of its file a track of a CUE sheet plays ([`Track::cue`]), in frames at the
    /// file's sample rate: `(start, end)`, end `None` for the end of the file. `None` for a
    /// track that is a whole file, or an unknown id. A sheet track whose file's sample rate
    /// is unknown (it was not read) is `corrupt_data`: its frames cannot be told.
    pub fn frame_range(&self, id: TrackId) -> Result<Option<(u64, Option<u64>)>> {
        let Some(cue) = self.inner.pool.read(|conn| reads::cue_frames(conn, id))? else {
            return Ok(None);
        };
        let Some(sample_rate) = cue.sample_rate else {
            return Err(LibraryError::Corrupt(format!(
                "track {id} of a CUE sheet has no known sample rate"
            )));
        };
        let at = |cd| crate::cue::cd_frames_to_samples(cd, sample_rate);
        Ok(Some((at(cue.start), cue.end.map(at))))
    }

    /// Cover image for a track: its embedded front cover, else a `cover`/`folder`/`front`
    /// `.jpg`/`.jpeg`/`.png` file in its folder. `None` for an unknown id or no image. Reads
    /// the disk on the calling thread; use [`AlbumSummary::art_track`] for albums. Images
    /// above 16 MiB and tag parsing above 32 MiB cumulative input are treated as absent.
    pub fn cover_art(&self, id: TrackId) -> Result<Option<CoverArt>> {
        if self.track(id)?.is_none() {
            return Ok(None);
        }
        let _access = self.file_lease(id)?;
        let Some((root, rel_path)) = self.inner.pool.read(|conn| reads::location(conn, id))? else {
            return Ok(None);
        };
        Ok(read_cover(&join_relative(&root, &rel_path))?)
    }

    /// Cover of a track shrunk to fit `max_edge` pixels on its longest edge, rounded up to
    /// 128, 256, 512 or 1024: JPEG (quality 85), or PNG when the image has transparent
    /// pixels. The original, as [`Library::cover_art`] returns it, when `max_edge` is above
    /// 1024, the original is no larger than the bucket (images are never enlarged), it is
    /// not a JPEG, PNG, GIF, WebP or BMP, or it cannot be decoded within limits (16384
    /// pixels a side, 256 MB). `None` as for `cover_art`.
    ///
    /// Thumbnails are made on demand on the calling thread and kept in a `covers` folder
    /// next to the database, one file per original image (by SHA-256) and size, so the
    /// tracks of an album share them. The folder is held under the limit of
    /// [`Library::set_cover_cache_limit`] by deleting the least recently used files.
    pub fn cover_thumbnail(&self, id: TrackId, max_edge: u32) -> Result<Option<CoverArt>> {
        Ok(self
            .cover_art(id)?
            .map(|original| self.inner.thumbnails.thumbnail(original, max_edge)))
    }

    /// Sets the size the thumbnail cache is kept under, in bytes
    /// ([`crate::DEFAULT_COVER_CACHE_BYTES`] until set). It takes effect when the next
    /// thumbnail is cached: if the folder is over the limit then, least recently used files
    /// are deleted until it is under 90% of it.
    pub fn set_cover_cache_limit(&self, bytes: u64) {
        self.inner.thumbnails.set_cap(bytes);
    }

    /// Lyrics for a track: a `.lrc` file with the track's name next to it (extension in any
    /// case), else the lyrics in its tags (`©lyr`, `LYRICS`, `USLT`), parsed as plain text,
    /// LRC or enhanced LRC (docs/design/media-resources.md). `None` for an unknown
    /// id or no lyrics; text over 1 MB is not read. Reads the disk on the calling thread.
    pub fn lyrics(&self, id: TrackId) -> Result<Option<Lyrics>> {
        if self.track(id)?.is_none() {
            return Ok(None);
        }
        let _access = self.file_lease(id)?;
        let Some((root, rel_path)) = self.inner.pool.read(|conn| reads::location(conn, id))? else {
            return Ok(None);
        };
        Ok(read_lyrics(
            self.inner.storage.files.as_ref(),
            &join_relative(&root, &rel_path),
        )?)
    }

    /// The cached waveform of a track (docs/design/media-resources.md), for the
    /// file's size and modification time as last scanned: `None` for an unknown id, when
    /// none was stored, when the file changed since (a rescan saw another size or time), or
    /// when the cache file cannot be read back. Reads the disk on the calling thread.
    pub fn waveform(&self, id: TrackId) -> Result<Option<Waveform>> {
        let Some(stamp) = self.file_version(id)? else {
            return Ok(None);
        };
        Ok(self.inner.waveforms.get(id, stamp))
    }

    /// Stores a track's waveform in the cache, computed from the file at version `from`
    /// ([`Library::file_version`] before it was computed), and removes the waveforms stored
    /// for other versions of the file. Not stored (`Ok(false)`) when the file was scanned
    /// as another version meanwhile: it was computed from a file that is no longer there.
    /// Unknown id: `track_not_found`. A waveform with no channel or more than 8, no bucket
    /// or more than 65536, or vectors not all `buckets` long is not stored (`io`, invalid
    /// input).
    pub fn store_waveform(
        &self,
        id: TrackId,
        from: FileVersion,
        waveform: &Waveform,
    ) -> Result<bool> {
        let Some(version) = self.file_version(id)? else {
            return Err(LibraryError::not_found(
                NotFound::Track,
                format!("track {id}"),
            ));
        };
        if version != from {
            return Ok(false);
        }
        self.inner.waveforms.put(id, version, waveform)?;
        Ok(true)
    }

    /// The version of a track's file as last scanned (its size and modification time):
    /// what a cached waveform belongs to. `None` for an unknown id.
    pub fn file_version(&self, id: TrackId) -> Result<Option<FileVersion>> {
        Ok(self
            .inner
            .pool
            .read(|conn| reads::file_stamp(conn, id))?
            .map(|(size, mtime_ms)| FileVersion { size, mtime_ms }))
    }

    // ---- flexible queries (docs/design/queries-playlists-undo.md) ---

    /// A page of tracks matching `query`, with statistics, user data and the requested
    /// `tags` (keys case-insensitive): `(rows, total matching tracks)`. Unless the filter
    /// mentions [`Field::Available`], only available tracks match. Type errors return
    /// [`LibraryError::InvalidQuery`].
    pub fn tracks_query(
        &self,
        query: &TrackQuery,
        page: Page,
        tags: &[String],
    ) -> Result<(Vec<TrackRow>, u64)> {
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let result = track_query::tracks_page(&tx, query, page, tags)?;
            tx.commit()?;
            Ok(result)
        })
    }

    /// The values of `field` among the tracks matching `filter`, each with its number of
    /// tracks, ordered by value with the unknown group (`value: None`) last:
    /// `(values, total number of values)`. A track with several values of a tag counts once
    /// under each. Tree browsing is a facet per level with the parent values as filters.
    pub fn facet(
        &self,
        field: &Field,
        filter: &Filter,
        page: Page,
    ) -> Result<(Vec<FacetValue>, u64)> {
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let result = track_query::facet(&tx, field, filter, page)?;
            tx.commit()?;
            Ok(result)
        })
    }

    /// [`TrackRow`]s for `ids`, in that order (unknown ids are left out), with the requested
    /// `tags`. With [`Library::track_ids`] this pages a large sorted view without an
    /// `OFFSET` sort per page: fetch the ordered ids once, then rows by window.
    pub fn track_rows(&self, ids: &[TrackId], tags: &[String]) -> Result<Vec<TrackRow>> {
        let keys = tags
            .iter()
            .map(|key| track_query::tag_key(key))
            .collect::<Result<Vec<_>>>()?;
        self.inner
            .pool
            .read(|conn| reads::track_rows(conn, ids, &keys))
    }

    /// The ids of the first `limit` tracks of `query`, in order (e.g. to snapshot a play
    /// context as a queue).
    pub fn track_ids(&self, query: &TrackQuery, limit: usize) -> Result<Vec<TrackId>> {
        self.inner.pool.read(|conn| {
            let limit = u64::try_from(limit).unwrap_or(u64::MAX);
            Ok(track_query::ids_page(conn, query, 0, limit)?.0)
        })
    }

    /// Recomputes play statistics from the raw history (after a rule change, or to repair).
    pub fn rebuild_play_stats(&self) -> Result<()> {
        self.inner
            .writer
            .call(|reply| WriteCmd::RebuildStats { reply })
    }

    // ---- play history ---

    /// Queues a play event and returns immediately. A failed commit is logged and
    /// included in the result of [`Self::close`].
    pub fn record_play(&self, event: PlayEvent) -> Result<()> {
        self.inner.writer.send(WriteCmd::RecordPlay(event))
    }

    /// A page of the play history, newest first, and the number of events, from one
    /// snapshot. Every stored event is listed (an ended reason this build does not know is
    /// [`crate::EndedReason::Unknown`]).
    pub fn history(&self, page: Page) -> Result<Paged<HistoryEntry>> {
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let result = reads::history(&tx, page)?;
            tx.commit()?;
            Ok(result)
        })
    }
}
