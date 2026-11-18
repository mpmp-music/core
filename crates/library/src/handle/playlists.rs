use super::*;
use crate::journal;

impl Library {
    // ---- playlists (docs/design/queries-playlists-undo.md) ---
    //
    // Edits take `expected_revision` (`None`: no check) and fail with
    // `LibraryError::PlaylistConflict` when the playlist is at another revision. Each edit
    // that changes something bumps the revision by one, returns the new one and records an
    // action (undo it with `undo_action`, or the newest edits with `undo_playlist`); an
    // edit that changes nothing (no tracks, no item of the playlist, same name, same
    // position, same query) returns the current revision unchanged and no action.
    // Playlists in the trash cannot be edited (`playlist_in_trash`) but can be read.

    /// Creates a playlist at revision 0, recorded as `playlist_create` (its undo moves the
    /// playlist to the trash). A smart query is type-checked (`LibraryError::InvalidQuery`);
    /// a blank name is `blank_name`.
    pub fn create_playlist(
        &self,
        name: &str,
        kind: PlaylistKind,
        meta: ActionMeta<'_>,
    ) -> Result<Done<PlaylistId>> {
        let name = name.to_owned();
        let origin = meta.owned();
        self.inner
            .writer
            .run(move |conn| playlist::create(conn, &name, &kind, &[], false, origin.meta()))
    }

    pub fn rename_playlist(
        &self,
        id: PlaylistId,
        name: &str,
        expected_revision: Option<u64>,
        meta: ActionMeta<'_>,
    ) -> Result<Done<u64>> {
        let name = name.to_owned();
        let origin = meta.owned();
        self.inner
            .writer
            .run(move |conn| playlist::rename(conn, id, &name, expected_revision, origin.meta()))
    }

    /// Inserts `tracks` (duplicates allowed) at position `at` of a manual playlist
    /// (`None` or the length: appends; past the length: `out_of_range`): `(new item ids,
    /// new revision)`. Every track must exist (unavailable is fine), else
    /// `track_not_found` and nothing is added.
    pub fn add_to_playlist(
        &self,
        id: PlaylistId,
        tracks: &[TrackId],
        at: Option<u32>,
        expected_revision: Option<u64>,
        meta: ActionMeta<'_>,
    ) -> Result<Done<(Vec<PlaylistItemId>, u64)>> {
        let tracks = tracks.to_vec();
        let origin = meta.owned();
        self.inner.writer.run(move |conn| {
            let meta = origin.meta();
            playlist::add(conn, id, &tracks, at, expected_revision, meta)
        })
    }

    /// Removes items of a manual playlist; items that are not in it are ignored
    /// (idempotent): `(the item ids actually removed, in request order, revision)`. When
    /// none was removed the revision stays.
    pub fn remove_from_playlist(
        &self,
        id: PlaylistId,
        items: &[PlaylistItemId],
        expected_revision: Option<u64>,
        meta: ActionMeta<'_>,
    ) -> Result<Done<(Vec<PlaylistItemId>, u64)>> {
        let items = items.to_vec();
        let origin = meta.owned();
        self.inner
            .writer
            .run(move |conn| playlist::remove(conn, id, &items, expected_revision, origin.meta()))
    }

    /// Moves an item of a manual playlist to final position `to`; `to` at or past the
    /// length is `out_of_range`, an item that is not in the playlist `item_not_found`.
    pub fn move_playlist_item(
        &self,
        id: PlaylistId,
        item: PlaylistItemId,
        to: u32,
        expected_revision: Option<u64>,
        meta: ActionMeta<'_>,
    ) -> Result<Done<u64>> {
        let origin = meta.owned();
        self.inner.writer.run(move |conn| {
            let meta = origin.meta();
            playlist::move_item(conn, id, item, to, expected_revision, meta)
        })
    }

    /// Replaces the query and `max_tracks` limit of a smart playlist (type-checked like
    /// [`Library::tracks_query`]). The same query (compared parsed, not as stored text) and
    /// limit is no change.
    pub fn set_smart_query(
        &self,
        id: PlaylistId,
        query: TrackQuery,
        max_tracks: Option<u32>,
        expected_revision: Option<u64>,
        meta: ActionMeta<'_>,
    ) -> Result<Done<u64>> {
        let origin = meta.owned();
        self.inner.writer.run(move |conn| {
            let meta = origin.meta();
            playlist::set_query(conn, id, &query, max_tracks, expected_revision, meta)
        })
    }

    /// Undoes the newest edit of a playlist (items, name or smart query) that is still in
    /// effect, if the playlist is at `revision` (its current revision; else
    /// `PlaylistConflict`): the new revision, and the undo's action. Calling it again (with
    /// the new revision) undoes the edit before, and so on, newest first: the last
    /// [`crate::PLAYLIST_UNDO_STEPS`] edits of the last
    /// [`crate::PLAYLIST_UNDO_RETENTION_MS`]; then `nothing_to_undo`. A redo (an undone
    /// undo, see [`Library::undo_action`]) counts as an edit. Moving into and out of the
    /// trash is not an edit (undo those with [`Library::undo_action`]). Survives restarts;
    /// a playlist in the trash is `playlist_in_trash`.
    pub fn undo_playlist(
        &self,
        id: PlaylistId,
        revision: u64,
        meta: ActionMeta<'_>,
    ) -> Result<Done<u64>> {
        let origin = meta.owned();
        let storage = self.inner.storage.files.clone();
        let (action, revision) = self.inner.writer.run(move |conn| {
            journal::undo_playlist(
                storage.as_ref(),
                conn,
                id,
                revision,
                origin.meta(),
                now_ms(),
            )
        })?;
        Ok(Done {
            value: revision,
            action: Some(action),
        })
    }

    /// Moves a playlist to the trash (nothing is lost; see [`Library::restore_playlist`]),
    /// recorded as `playlist_delete`; one already in the trash stays there (no action).
    /// Never permanently deletes other playlists. The revision does not change.
    pub fn delete_playlist(&self, id: PlaylistId, meta: ActionMeta<'_>) -> Result<Done<()>> {
        let origin = meta.owned();
        self.inner
            .writer
            .run(move |conn| playlist::delete(conn, id, origin.meta(), now_ms()))
    }

    /// Takes a playlist out of the trash, as it was (revision and undo included), recorded
    /// as `playlist_restore`. A playlist that is not in the trash is left as it is (no
    /// action).
    pub fn restore_playlist(&self, id: PlaylistId, meta: ActionMeta<'_>) -> Result<Done<()>> {
        let origin = meta.owned();
        self.inner
            .writer
            .run(move |conn| playlist::restore(conn, id, origin.meta(), now_ms()))
    }

    /// Permanently deletes playlists from the trash (`None`: empties it): how many. A
    /// playlist that is not in the trash fails the whole call (`not_in_trash`;
    /// `playlist_not_found` if unknown). Only the selected playlists are deleted, regardless
    /// of age. It cannot be undone: the purged playlists' actions are dropped from the
    /// journal, and the purge is recorded as an audit entry (kind `purge`, never undoable)
    /// when it deleted anything.
    pub fn purge_playlists(
        &self,
        ids: Option<&[PlaylistId]>,
        meta: ActionMeta<'_>,
    ) -> Result<Done<u64>> {
        let ids = ids.map(<[PlaylistId]>::to_vec);
        let origin = meta.owned();
        self.inner.writer.run(move |conn| {
            playlist::purge(
                conn,
                ids.as_deref(),
                origin.meta(),
                crate::db::writer::now_ms(),
            )
        })
    }

    /// Captures exact trash IDs and display data from one read snapshot.
    pub fn describe_purge(&self, ids: Option<&[PlaylistId]>) -> Result<crate::PurgeGuard> {
        let counts = self.inner.smart_counts.at_current_version();
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let guard = playlist::purge_selection(&tx, &counts, ids)?;
            tx.commit()?;
            Ok(guard)
        })
    }

    /// Compares a frozen selection inside the delete transaction, without purging any
    /// other expired playlists. Any set or display-data change refuses the whole call.
    pub fn purge_playlists_guarded(
        &self,
        guard: crate::PurgeGuard,
        meta: ActionMeta<'_>,
    ) -> Result<Done<u64>> {
        let inner = Arc::clone(&self.inner);
        let origin = meta.owned();
        self.inner.writer.run(move |conn| {
            let counts = inner.smart_counts.at_current_version();
            playlist::purge_guarded(conn, &guard, &counts, origin.meta(), now_ms())
        })
    }

    /// Playlists by name (folded like search), or with `deleted` the trash, most recently
    /// deleted first: `(page, total)`.
    ///
    /// Smart playlists' counts are cached until the playlist or the library's tracks,
    /// statistics or user data change; for queries relative to the current time
    /// (`days_ago`) at most a minute, so a listed count may be that old (a page from
    /// [`Library::playlist_page`] or [`Library::playlist_items`] always counts afresh).
    pub fn playlists(&self, deleted: bool, page: Page) -> Result<(Vec<PlaylistSummary>, u64)> {
        let counts = self.inner.smart_counts.at_current_version();
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let result = playlist::summaries(&tx, &counts, deleted, page)?;
            tx.commit()?;
            Ok(result)
        })
    }

    /// One playlist (also in the trash); `None` if unknown or purged.
    pub fn playlist(&self, id: PlaylistId) -> Result<Option<PlaylistSummary>> {
        let counts = self.inner.smart_counts.at_current_version();
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let result = playlist::summary(&tx, &counts, id)?;
            tx.commit()?;
            Ok(result)
        })
    }

    /// [`Library::playlist`] and a page of [`Library::playlist_items`] from one snapshot:
    /// `(summary, entries)`; the page's total is `summary.track_count` and its revision
    /// `summary.revision`. A smart playlist's query runs once (for the page) when its count
    /// is cached. `None` if the playlist is unknown or purged. A smart playlist whose stored
    /// query is invalid lists no entries (its summary's `query_error` says why).
    pub fn playlist_page(
        &self,
        id: PlaylistId,
        page: Page,
        tags: &[String],
    ) -> Result<Option<(PlaylistSummary, Vec<PlaylistEntry>)>> {
        let counts = self.inner.smart_counts.at_current_version();
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let result = playlist::summary_and_items(&tx, &counts, id, page, tags)?;
            tx.commit()?;
            Ok(result)
        })
    }

    /// A page of a playlist's entries with the requested `tags`: `(revision, entries,
    /// total)`. Manual: items in order, including unavailable tracks (`available: false`).
    /// Smart: the query's tracks (available only, unless the query mentions availability)
    /// up to its limit; an invalid stored query lists nothing (see
    /// [`PlaylistSummary::query_error`]). Unknown: `playlist_not_found`.
    pub fn playlist_items(
        &self,
        id: PlaylistId,
        page: Page,
        tags: &[String],
    ) -> Result<(u64, Vec<PlaylistEntry>, u64)> {
        let counts = self.inner.smart_counts.at_current_version();
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let result = playlist::items(&tx, &counts, id, page, tags)?;
            tx.commit()?;
            Ok(result)
        })
    }

    /// The tracks to play for a playlist, in order, at most `limit`: unavailable tracks are
    /// left out (manual items, and smart queries that mention availability).
    pub fn playlist_track_ids(&self, id: PlaylistId, limit: usize) -> Result<Vec<TrackId>> {
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let result = playlist::track_ids(&tx, id, limit)?;
            tx.commit()?;
            Ok(result)
        })
    }

    /// Resolve playback from one read snapshot, including the optional revision guard
    /// and the exact manual item (its index counts only available tracks).
    pub fn playlist_playback(
        &self,
        id: PlaylistId,
        limit: usize,
        start_item: Option<PlaylistItemId>,
        expected_revision: Option<u64>,
    ) -> Result<(Vec<TrackId>, Option<usize>)> {
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let result = playlist::playback(&tx, id, limit, start_item, expected_revision)?;
            tx.commit()?;
            Ok(result)
        })
    }
}
