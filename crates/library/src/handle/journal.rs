use super::*;
use crate::journal;

impl Library {
    /// Sets (1–5 stars) or clears (`None`) the rating of tracks; stored in the database
    /// only. Unknown ids and tracks that already have that rating are left out: the tracks
    /// that changed, in request order (none: no action). One transaction, recorded as one
    /// `set_rating` action. A rating outside 1–5 is [`LibraryError::Invalid`]
    /// (`out_of_range`).
    pub fn set_rating(
        &self,
        ids: &[TrackId],
        rating: Option<u8>,
        meta: ActionMeta<'_>,
    ) -> Result<Done<Vec<TrackId>>> {
        if let Some(r) = rating.filter(|r| !(1..=5).contains(r)) {
            return Err(LibraryError::invalid(
                Invalid::OutOfRange,
                format!("rating {r} is out of range 1-5"),
            ));
        }
        self.set_user(ids, UserField::Rating(rating), meta)
    }

    /// Sets the loved flag of tracks; stored in the database only. Like
    /// [`Library::set_rating`]: the tracks that changed, one `set_loved` action.
    pub fn set_loved(
        &self,
        ids: &[TrackId],
        loved: bool,
        meta: ActionMeta<'_>,
    ) -> Result<Done<Vec<TrackId>>> {
        self.set_user(ids, UserField::Loved(loved), meta)
    }

    pub(super) fn set_user(
        &self,
        ids: &[TrackId],
        field: UserField,
        meta: ActionMeta<'_>,
    ) -> Result<Done<Vec<TrackId>>> {
        let ids = ids.to_vec();
        let origin = meta.owned();
        self.inner
            .writer
            .run_data(move |conn| ratings::set(conn, &ids, field, origin.meta()))
    }

    // ---- the action journal (docs/design/queries-playlists-undo.md) ---

    /// Undoes a recorded action: the undo's own action (kind `undo`, or `redo` when `id`
    /// was an undo: undoing an undo redoes the action) and what changed, for change events.
    ///
    /// Fails without changing anything when the action is unknown or can no longer be
    /// undone (`action_not_found`: older than [`crate::ACTION_RETENTION_MS`], beyond the
    /// newest [`crate::ACTION_LIMIT`] actions or its playlist's last
    /// [`crate::PLAYLIST_UNDO_STEPS`] edits, or purged with its playlist), was undone
    /// already (`already_undone`), or when any of its objects was changed since by another
    /// action ([`LibraryError::ActionConflict`], listing them): an undo never overwrites a
    /// later change. Undoing an action whose stored undo data cannot be read fails once
    /// with `corrupt_data`; the action is then no longer undoable and older actions on the
    /// same objects can be undone past it. Undoing a playlist edit while the playlist is in
    /// the trash is `playlist_in_trash`.
    ///
    /// A tag edit (or an undo of one) is undone by a tag job ([`Library::undo_tag_edit`],
    /// with no file in use), waited for here; when not one file could be written back it
    /// fails (`io`) and stays undoable.
    pub fn undo_action(&self, id: ActionId, meta: ActionMeta<'_>) -> Result<Done<UndoEffect>> {
        if let Some(job) = self.undo_cover_edit(id, meta, TagJobOptions::default())? {
            let report = job.wait()?;
            let action = report.action.ok_or_else(|| {
                LibraryError::Io(std::io::Error::other(format!(
                    "none of the files of action {id} could be written back"
                )))
            })?;
            return Ok(Done {
                value: UndoEffect {
                    undone: id,
                    tracks: report.written,
                    playlists: Vec::new(),
                    roots: Vec::new(),
                },
                action: Some(action),
            });
        }
        if let Some(job) = self.undo_tag_edit(id, meta, TagJobOptions::default())? {
            let report = job.wait()?;
            let Some(action) = report.action else {
                return Err(LibraryError::Io(std::io::Error::other(format!(
                    "none of the files of action {id} could be written back"
                ))));
            };
            return Ok(Done {
                value: UndoEffect {
                    undone: id,
                    tracks: report.written,
                    playlists: Vec::new(),
                    roots: Vec::new(),
                },
                action: Some(action),
            });
        }
        let origin = meta.owned();
        let storage = self.inner.storage.files.clone();
        let (action, effect) = self.inner.writer.run_data(move |conn| {
            journal::undo_action(storage.as_ref(), conn, id, origin.meta(), now_ms())
        })?;
        self.finish_root_undo(&effect);
        Ok(Done {
            value: effect,
            action: Some(action),
        })
    }

    // ---- tag editing (docs/design/media-resources.md; `crate::tags::job`) ---

    /// What editing the tags of `tracks` with `changes` would do: each file's tags as the
    /// library shows them, before and after, and the files it would skip (by code; the
    /// player's open files only when it runs). Reads the files on the caller's thread;
    /// writes nothing. Unknown tracks are `track_not_found`; bad changes `invalid_tag`
    /// ([`crate::check_changes`]).
    pub fn preview_tag_edit(
        &self,
        tracks: &[TrackId],
        changes: &[TagChange],
    ) -> Result<TagPreview> {
        tag_job::check_count(tracks)?;
        tag_job::preview(&self.inner, tracks, changes)
    }

    /// Edits the tags of `tracks`' files with `changes` ([`crate::check_changes`]; at most
    /// [`crate::MAX_TAG_EDIT_TRACKS`] tracks, each must exist) on a thread of its own, and
    /// returns at once; the job's report ([`TagJob::wait`]) lists what it wrote, skipped and
    /// could not write. The files written are read again and their rows updated, and one
    /// action records them (`tag_edit`, undoable: [`Library::undo_tag_edit`]), when the job
    /// ends; nothing is recorded when no file was written. Ratings are never written to
    /// files (a rating key is `invalid_tag`).
    pub fn edit_tags(
        &self,
        tracks: &[TrackId],
        changes: &[TagChange],
        meta: ActionMeta<'_>,
        options: TagJobOptions,
    ) -> Result<TagJob> {
        tag_job::start_edit(&self.inner, tracks, changes, meta, options)
    }

    /// Undoes a tag edit (or an undo or redo of one) by writing the old values back into
    /// its files, as a job ([`Library::edit_tags`]); the undo is itself an action. `None`:
    /// the action does not write files back (any other action: [`Library::undo_action`]).
    /// Checked first as any undo is (unknown, undone or expired; a later tag edit of the
    /// same files: `action_conflict`), then by the job before it writes anything: a file
    /// that changed since the edit wrote it (its size or modification time), or is gone,
    /// fails the job with [`LibraryError::ActionConflict`] listing those tracks. Files
    /// held during preflight fail the entire job with [`LibraryError::InUse`] for
    /// playback or [`LibraryError::ExternalInUse`] for a sharing conflict. A file that becomes busy during publication is reported
    /// as a skip and stays in the source action's inverse.
    pub fn undo_tag_edit(
        &self,
        id: ActionId,
        meta: ActionMeta<'_>,
        options: TagJobOptions,
    ) -> Result<Option<TagJob>> {
        tag_job::start_undo(&self.inner, id, meta, options)
    }

    /// Read-only before/after and canonical targets for a file undo. No write lock.
    pub fn preview_tag_undo(&self, id: ActionId) -> Result<TagPreview> {
        tag_job::preview_undo(&self.inner, id)
    }
    /// Whether a tag edit (or an undo of one) can be undone as far as the journal tells
    /// (unknown, undone, expired; a later tag edit of its files: `action_conflict`), without
    /// looking at the files. Any other action: `Ok`.
    pub fn check_tag_undo(&self, id: ActionId) -> Result<()> {
        let now = now_ms();
        self.inner
            .pool
            .read(|conn| journal::tag_undo(conn, id, now))
            .map(drop)
    }

    /// The tracks whose files undoing action `id` writes: those of a tag edit (or an undo
    /// or redo of one); empty for any other action (or an unknown one).
    pub fn tag_edit_tracks(&self, id: ActionId) -> Result<Vec<TrackId>> {
        tag_job::tracks_of(&self.inner, id)
    }

    /// A page of the journal, newest first, and how many actions match, from one snapshot.
    /// `origin` filters by origin label: exactly, or as a prefix when it ends in `:`
    /// (`"agent:"`: every agent).
    pub fn actions(&self, origin: Option<&str>, page: Page) -> Result<Paged<ActionSummary>> {
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let result = journal::list(&tx, origin, page, now_ms())?;
            tx.commit()?;
            Ok(result)
        })
    }

    /// The effective undo, including its complete chain, validated from one snapshot.
    pub fn describe_undo(&self, id: ActionId) -> Result<crate::UndoDescription> {
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let description = journal::describe_undo(&tx, id, now_ms())?;
            tx.commit()?;
            Ok(description)
        })
    }
    /// A data-only undo with the description checked inside the writer transaction.
    pub fn undo_action_guarded(
        &self,
        expected: crate::UndoDescription,
        meta: ActionMeta<'_>,
    ) -> Result<Done<UndoEffect>> {
        if expected.writes_files {
            if expected.effective_kind == "edit_cover" {
                self.require_cover_write()?;
            } else {
                tag_job::require_tag_write(&self.inner)?;
            }
        }
        let meta = meta.owned();
        let storage = self.inner.storage.files.clone();
        let (action, value) = self.inner.writer.run_data(move |conn| {
            journal::undo_guarded(storage.as_ref(), conn, expected, meta.meta(), now_ms())
        })?;
        self.finish_root_undo(&value);
        Ok(Done {
            value,
            action: Some(action),
        })
    }
    pub(super) fn finish_root_undo(&self, effect: &UndoEffect) {
        // The journal transaction has committed. On a failed read, conservatively
        // clear every affected suspension rather than report a fictitious rollback.
        let active = self.roots().ok();
        for root in &effect.roots {
            if active
                .as_ref()
                .is_none_or(|roots| !roots.iter().any(|r| r.id == *root))
            {
                self.inner.file_access.resume(*root);
            }
        }
    }

    /// One action of the journal; `None` if unknown or dropped.
    pub fn action(&self, id: ActionId) -> Result<Option<ActionSummary>> {
        self.inner
            .pool
            .read(|conn| journal::summary(conn, id, now_ms()))
    }
}
