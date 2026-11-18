use super::super::*;
use crate::contract::*;

impl Actor {
    pub(in crate::actor) fn command_add_root(
        &mut self,
        path: String,
        origin: &Origin,
        grant: Option<GrantId>,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            let added = self.catalog.add_root(&path, origin, grant)?;
            receipt.root = Some(added.value);
            receipt.action = added.action;
            // A folder that already is a root changes nothing: no scan either.
            if added.action.is_some() {
                // The folder list changed now; its tracks come with the scan (another
                // `LibraryChanged` follows its `JobFinished`).
                tracing::debug!(
                    reason = "add_root",
                    root = added.value.0,
                    "library scan trigger"
                );
                match self.start_scan_scoped(Some(origin.clone()), Some(vec![added.value])) {
                    Ok(job) => receipt.job = Some(job),
                    Err(err) => tracing::warn!("cannot scan the new root: {err}"),
                }
                self.folders_changed();
            }
            added.action.is_some()
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_remove_root(
        &mut self,
        root: RootId,
        origin: &Origin,
        grant: Option<GrantId>,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            if self.root_transition_pending(root)? {
                return Err(ContractError::failed(
                    ErrorCode::JobBusy,
                    "root release is still pending",
                )
                .retryable());
            }
            let path = self.catalog.root_location(root)?.unwrap_or_default();
            let removed = self.catalog.remove_root(root, origin, grant)?;
            receipt.root = Some(root);
            receipt.action = removed.action;
            // Removing a removed folder again changes nothing.
            let changed = removed.action.is_some();
            if changed {
                self.folders_changed();
                self.members_changed();
                self.roots
                    .removed_root_notifications
                    .insert(root, RemovalRecovery::new(path));
            }
            let notified = self.retry_removed_root(root);
            if changed && notified.is_err() {
                self.emit(Event::ActionsChanged);
            }
            notified?;
            changed
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_rescan(
        &mut self,
        origin: &Origin,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            receipt.job = Some(self.start_scan(Some(origin.clone()))?);
            true
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_set_rating(
        &mut self,
        tracks: Vec<TrackId>,
        rating: Option<u8>,
        origin: &Origin,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            if let Some(r) = rating.filter(|r| !(1..=5).contains(r)) {
                return Err(ContractError::invalid(
                    ErrorCode::OutOfRange,
                    format!("rating {r} is not 1 to 5 stars (None clears it)"),
                )
                .with_field("command.rating"));
            }
            self.check_tracks_exist(&tracks, "command.tracks")?;
            let rated = self.catalog.set_rating(&tracks, rating, origin)?;
            self.tracks_changed(rated, receipt)
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_set_loved(
        &mut self,
        tracks: Vec<TrackId>,
        loved: bool,
        origin: &Origin,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            self.check_tracks_exist(&tracks, "command.tracks")?;
            let loved = self.catalog.set_loved(&tracks, loved, origin)?;
            self.tracks_changed(loved, receipt)
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_undo_action(
        &mut self,
        action: ActionId,
        origin: &Origin,
        grant: Option<GrantId>,
        checked: Option<&[Target]>,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            let guard = self.confirmations.host_undo_guard.take();
            if let Some(expected) = &guard
                && (expected.action.0 != action.0
                    || self.catalog.describe_undo(action)? != *expected)
            {
                return Err(ContractError::new(
                    ErrorKind::Conflict,
                    ErrorCode::ActionConflict,
                    "undo description changed",
                ));
            }
            let root_targets = self.catalog.action_roots(action)?;
            for root in &root_targets {
                if self.root_transition_pending(*root)? {
                    return Err(ContractError::failed(
                        ErrorCode::JobBusy,
                        "root release is still pending",
                    )
                    .retryable());
                }
            }
            let removes_root = self.catalog.undo_root_command(action)? == Some("remove_root");
            let root_paths = if removes_root {
                root_targets
                    .iter()
                    .map(|root| {
                        Ok((
                            *root,
                            self.catalog.root_location(*root)?.unwrap_or_default(),
                        ))
                    })
                    .collect::<Result<Vec<_>, ContractError>>()?
            } else {
                Vec::new()
            };
            // A tag edit's undo writes files: a job.
            let tracks = self.catalog.tag_undo_tracks(action)?;
            if !tracks.is_empty() {
                let job = self.new_job();
                let mut files = self.tag_files(&tracks, checked);
                files.undo_guard = guard.clone();
                let events = self.tag_events(job);
                let is_cover = !self.catalog.cover_undo_tracks(action)?.is_empty();
                let started = if is_cover {
                    self.catalog
                        .undo_cover_edit(action, files, origin, grant, events)?
                } else {
                    self.catalog
                        .undo_tag_edit(action, files, origin, grant, events)?
                };
                if started {
                    if is_cover {
                        self.workers.cover_jobs.insert(job);
                    }
                    self.workers.tag_jobs.insert(job, origin.clone());
                    crate::jobs::lock(&self.job_history).start(
                        job,
                        if is_cover {
                            JobKind::CoverEdit
                        } else {
                            JobKind::TagEdit
                        },
                    );
                    receipt.job = Some(job);
                    return Ok(true);
                }
            }
            let undone = match guard {
                Some(expected) => self.catalog.undo_action_guarded(expected, origin, grant)?,
                None => self.catalog.undo_action(action, origin, grant)?,
            };
            receipt.action = Some(undone.action);
            if !undone.tracks.is_empty() {
                self.emit(Event::TracksChanged {
                    tracks: undone.tracks,
                });
            }
            if !undone.playlists.is_empty() {
                self.emit(Event::PlaylistsChanged {
                    playlists: undone.playlists,
                });
            }
            if !undone.roots.is_empty() {
                self.folders_changed();
                self.members_changed();
            }
            for (root, path) in &root_paths {
                self.roots
                    .removed_root_notifications
                    .insert(*root, RemovalRecovery::new(path.clone()));
            }
            for (root, _) in root_paths {
                if let Err(mut error) = self.retry_removed_root(root) {
                    error.details.retryable = false;
                    error.details.recovery_command = Some("remove_root".into());
                    self.emit(Event::ActionsChanged);
                    return Err(error);
                }
            }
            if undone.rescan {
                // A folder came back: its tracks return with a scan.
                match self.start_scan(Some(origin.clone())) {
                    Ok(job) => receipt.job = Some(job),
                    Err(err) => tracing::warn!("cannot rescan the restored root: {err}"),
                }
            }
            true
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_release_root(
        &mut self,
        root: RootId,
        origin: &Origin,
        grant: Option<GrantId>,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            // Mark before stopping, so no canceled/replaced prefetch can reopen it.
            if self.root_transition_pending(root)? || self.roots.releasing.len() >= MAX_CHECKS {
                return Err(ContractError::failed(
                    ErrorCode::JobBusy,
                    "root release is already pending or the release worker limit was reached",
                )
                .retryable());
            }
            self.catalog.check_root_access(root)?;
            if !matches!(origin, Origin::Ui) {
                receipt.audit = Some(self.catalog.audit_root_access(root, true, origin, grant)?);
                self.emit(Event::ActionsChanged);
            }
            let deadline = Instant::now() + ROOT_DRAIN_TIMEOUT;
            let changed = self.catalog.suspend_root(root)?;
            let uses_root = self
                .playback
                .playing
                .as_ref()
                .map(|p| p.instance)
                .into_iter()
                .chain(self.playback.pending_next)
                .filter_map(|id| self.playback.instances.get(&id))
                .any(|i| i.track.root == root);
            if uses_root {
                self.remember_pause_anchor();
                let (frozen, thread) = self.engine.freeze_output_detached();
                self.engine_events = self.engine.events();
                self.reduce_frozen_inner(frozen, self.playback.state, false);
                let position = self
                    .position()
                    .map(|p| (p.entry, 0))
                    .filter(|(e, _)| self.playback.queue.entry(*e).is_some());
                self.finish_playing(EndReason::Stopped, origin);
                self.playback.resume_at = position;
                self.output_recovered();
                self.retire_session();
                self.playback.state = PlayState::Stopped;
                self.persistence.dirty = true;
                self.save_now();
                self.emit_state();
                self.emit_position();
                if let Some(thread) = thread {
                    self.roots
                        .retired_decodes
                        .entry(root)
                        .or_default()
                        .push(Arc::new(Mutex::new(Some(thread))));
                }
            }
            receipt.root = Some(root);
            if changed {
                self.emit(Event::LibraryChanged);
            }
            self.roots.release_start = Some(ReleaseStart {
                audit: receipt.audit,
                deadline,
                root,
                changed,
                origin: origin.clone(),
                confirmation: None,
            });
            changed
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_resume_root(
        &mut self,
        root: RootId,
        origin: &Origin,
        grant: Option<GrantId>,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            // Do not reopen a root while a successful barrier can still be answered.
            if self.root_transition_pending(root)? {
                return Err(ContractError::failed(
                    ErrorCode::JobBusy,
                    "root release is still pending",
                )
                .retryable());
            }
            let path = self.catalog.root_location(root)?.ok_or_else(|| {
                ContractError::not_found(ErrorCode::RootNotFound, "unknown library root")
            })?;
            self.catalog.check_root_access(root)?;
            if !matches!(origin, Origin::Ui) {
                receipt.audit = Some(self.catalog.audit_root_access(root, false, origin, grant)?);
                self.emit(Event::ActionsChanged);
            }
            self.root_access.notify(&path, false)?;
            let changed = self.catalog.resume_root(root)?;
            self.reap_retired_decodes();
            receipt.root = Some(root);
            if changed {
                self.members_changed();
                self.emit(Event::LibraryChanged);
                match self.start_scan_scoped(Some(origin.clone()), Some(vec![root])) {
                    Ok(job) => receipt.job = Some(job),
                    Err(error) => tracing::warn!(%error, "cannot scan resumed root"),
                }
            }
            changed
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_resolve_cover_recovery(
        &mut self,
        track: TrackId,
        resolution: CoverRecoveryResolution,
        origin: &Origin,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            if !matches!(origin, Origin::Ui) {
                return Err(ContractError::forbidden(
                    ErrorCode::OriginNotAllowed,
                    "cover recovery resolution is UI-only",
                ));
            }
            if resolution == crate::contract::CoverRecoveryResolution::Unknown {
                return Err(ContractError::unknown_variant("cover recovery resolution")
                    .with_field("command.resolution"));
            }
            let guard = self
                .confirmations
                .host_recovery_guard
                .take()
                .ok_or_else(|| {
                    ContractError::new(
                        ErrorKind::NeedsConfirmation,
                        ErrorCode::NeedsConfirmation,
                        "cover recovery resolution requires UI confirmation",
                    )
                })?;
            if guard.track.0 != track.0 {
                return Err(ContractError::new(
                    ErrorKind::Conflict,
                    ErrorCode::CoverRecoveryChanged,
                    "cover recovery target changed",
                ));
            }
            let (audit, summary) = self.catalog.accept_cover_recovery(&guard, origin)?;
            receipt.audit = Some(audit);
            self.emit(Event::CoverRecoveryResolved { track, summary });
            self.emit(Event::ActionsChanged);
            self.emit(Event::CoversChanged {
                tracks: vec![track],
            });
            self.emit(Event::TracksChanged {
                tracks: vec![track],
            });
            self.emit(Event::LibraryChanged);
            if let Ok(job) = self.start_scan(Some(origin.clone())) {
                receipt.job = Some(job);
            }
            true
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_edit_cover(
        &mut self,
        tracks: Vec<TrackId>,
        change: CoverChange,
        origin: &Origin,
        grant: Option<GrantId>,
        checked: Option<&[Target]>,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            if tracks.len() > crate::contract::MAX_COVER_EDIT_TRACKS as usize {
                return Err(ContractError::invalid(
                    ErrorCode::OutOfRange,
                    "too many cover targets",
                ));
            }
            crate::catalog::cover_change(&change).map_err(|e| e.with_field("command.change"))?;
            self.check_tracks_exist(&tracks, "command.tracks")?;
            if tracks.is_empty() {
                false
            } else {
                let job = self.new_job();
                let mut files = self.tag_files(&tracks, checked);
                files.cover_sources = self.confirmations.host_cover_sources.take();
                let events = self.tag_events(job);
                self.catalog
                    .edit_cover(&tracks, &change, files, origin, grant, events)?;
                self.workers.tag_jobs.insert(job, origin.clone());
                self.workers.cover_jobs.insert(job);
                crate::jobs::lock(&self.job_history).start(job, JobKind::CoverEdit);
                receipt.job = Some(job);
                true
            }
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_edit_tags(
        &mut self,
        tracks: Vec<TrackId>,
        changes: Vec<TagChange>,
        origin: &Origin,
        grant: Option<GrantId>,
        checked: Option<&[Target]>,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            if let Some(i) = changes.iter().position(|c| matches!(c, TagChange::Unknown)) {
                return Err(ContractError::unknown_variant("tag change")
                    .with_field(format!("command.changes[{i}]")));
            }
            if tracks.len() > MAX_TAG_EDIT_TRACKS as usize {
                return Err(ContractError::invalid(
                    ErrorCode::OutOfRange,
                    format!("at most {MAX_TAG_EDIT_TRACKS} tracks in one tag edit"),
                )
                .with_field("command.tracks"));
            }
            self.check_tracks_exist(&tracks, "command.tracks")?;
            if tracks.is_empty() || changes.is_empty() {
                false
            } else {
                let job = self.new_job();
                let files = self.tag_files(&tracks, checked);
                let events = self.tag_events(job);
                self.catalog
                    .edit_tags(&tracks, &changes, files, origin, grant, events)?;
                self.workers.tag_jobs.insert(job, origin.clone());
                crate::jobs::lock(&self.job_history).start(job, JobKind::TagEdit);
                receipt.job = Some(job);
                true
            }
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_playlist(
        &mut self,
        command: Command,
        origin: &Origin,
        grant: Option<GrantId>,
        purge: Option<&mpmp_library::PurgeGuard>,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            if let Command::AddToPlaylist { tracks, .. } = &command {
                self.check_tracks_exist(tracks, "command.tracks")?;
            }
            let guard = purge
                .cloned()
                .or_else(|| self.confirmations.host_purge_guard.take());
            let outcome = if let Some(guard) = guard {
                self.catalog.purge_guarded(guard, origin)?
            } else {
                self.catalog.playlist_command(&command, origin, grant)?
            };
            if outcome.changed && !matches!(command, Command::ExportM3u { .. }) {
                self.emit(Event::PlaylistsChanged {
                    playlists: outcome.playlists,
                });
            }
            receipt.playlist = outcome.receipt;
            receipt.action = outcome.action;
            // Keep the audit distinct from the undoable action (guarantee 15).
            receipt.audit = outcome.audit;
            if outcome.audit.is_some() && outcome.action.is_none() {
                self.emit(Event::ActionsChanged);
            }
            outcome.changed
        };
        Ok(changed)
    }
}
