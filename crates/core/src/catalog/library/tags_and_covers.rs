use super::*;

impl TagsAndCovers for Library {
    fn cover_sources(
        &self,
        ids: &[TrackId],
    ) -> Result<Vec<mpmp_library::CoverSource>, ContractError> {
        Library::cover_sources(self, &library_ids(ids)).map_err(library_error)
    }

    fn take_recovered_covers(&self) -> Vec<TrackId> {
        contract_ids(Library::take_recovered_covers(self))
    }

    fn cover_recovery_required(&self) -> bool {
        Library::has_cover_recovery_required(self)
    }

    fn cover_undo_tracks(&self, action: ActionId) -> Result<Vec<TrackId>, ContractError> {
        Library::cover_edit_tracks(self, mpmp_library::ActionId(action.0))
            .map(contract_ids)
            .map_err(library_error)
    }

    fn describe_cover_recovery(
        &self,
        track: TrackId,
        resolution: mpmp_library::CoverRecoveryResolution,
        selection: Option<mpmp_library::CoverRecoverySelection>,
    ) -> Result<mpmp_library::CoverRecoveryGuard, ContractError> {
        Library::describe_cover_recovery_selected(
            self,
            mpmp_library::TrackId(track.0),
            resolution,
            selection.as_ref(),
        )
        .map_err(library_error)
    }

    fn accept_cover_recovery(
        &self,
        guard: &mpmp_library::CoverRecoveryGuard,
        origin: &Origin,
    ) -> Result<(ActionId, String), ContractError> {
        if !matches!(origin, Origin::Ui) {
            return Err(ContractError::forbidden(
                ErrorCode::OriginNotAllowed,
                "cover recovery resolution is UI-only",
            ));
        }
        Library::accept_cover_recovery(self, guard, mpmp_library::ActionMeta::new("ui"))
            .map(|id| {
                (
                    ActionId(id.0),
                    Library::describe_cover_recovery(self, guard.track)
                        .map(|g| {
                            format!("Recovery completed. Retained candidates: {}", g.inventory)
                        })
                        .unwrap_or_else(|_| {
                            "Recovery completed; no registered candidates remain.".into()
                        }),
                )
            })
            .map_err(library_error)
    }

    fn edit_cover(
        &self,
        tracks: &[TrackId],
        change: &contract::CoverChange,
        files: TagFiles,
        origin: &Origin,
        grant: Option<GrantId>,
        events: Box<dyn Fn(TagJobEvent) + Send + Sync>,
    ) -> Result<(), ContractError> {
        check_cover_tracks(tracks)?;
        let change = cover_change(change)?;
        let label = origin.label();
        let events: Arc<dyn Fn(TagJobEvent) + Send + Sync> = Arc::from(events);
        let job = Library::edit_cover(
            self,
            &library_ids(tracks),
            &change,
            action_meta(&label, grant, origin),
            tag_job_options(files, &events),
        )
        .map_err(library_error)?;
        wait_for_covers(job, events)
    }

    fn undo_cover_edit(
        &self,
        action: ActionId,
        files: TagFiles,
        origin: &Origin,
        grant: Option<GrantId>,
        events: Box<dyn Fn(TagJobEvent) + Send + Sync>,
    ) -> Result<bool, ContractError> {
        let label = origin.label();
        let events: Arc<dyn Fn(TagJobEvent) + Send + Sync> = Arc::from(events);
        match Library::undo_cover_edit(
            self,
            mpmp_library::ActionId(action.0),
            action_meta(&label, grant, origin),
            tag_job_options(files, &events),
        )
        .map_err(library_error)?
        {
            Some(job) => wait_for_covers(job, events).map(|()| true),
            None => Ok(false),
        }
    }

    fn edit_tags(
        &self,
        tracks: &[TrackId],
        changes: &[TagChange],
        files: TagFiles,
        origin: &Origin,
        grant: Option<GrantId>,
        events: Box<dyn Fn(TagJobEvent) + Send + Sync>,
    ) -> Result<(), ContractError> {
        check_tag_tracks(tracks, "command.tracks")?;
        let changes = tag_changes(changes, "command.")?;
        let label = origin.label();
        let events: Arc<dyn Fn(TagJobEvent) + Send + Sync> = Arc::from(events);
        let job = Library::edit_tags(
            self,
            &library_ids(tracks),
            &changes,
            action_meta(&label, grant, origin),
            tag_job_options(files, &events),
        )
        .map_err(library_error)?;
        wait_for_tags(job, events)
    }

    fn undo_tag_edit(
        &self,
        action: ActionId,
        files: TagFiles,
        origin: &Origin,
        grant: Option<GrantId>,
        events: Box<dyn Fn(TagJobEvent) + Send + Sync>,
    ) -> Result<bool, ContractError> {
        let label = origin.label();
        let events: Arc<dyn Fn(TagJobEvent) + Send + Sync> = Arc::from(events);
        let job = Library::undo_tag_edit(
            self,
            mpmp_library::ActionId(action.0),
            action_meta(&label, grant, origin),
            tag_job_options(files, &events),
        )
        .map_err(|e| undo_error(e, action))?;
        match job {
            Some(job) => wait_for_tags(job, events).map(|()| true),
            None => Ok(false),
        }
    }

    fn tag_undo_tracks(&self, action: ActionId) -> Result<Vec<TrackId>, ContractError> {
        let mut ids = Library::tag_edit_tracks(self, mpmp_library::ActionId(action.0))
            .map_err(library_error)?;
        ids.extend(
            Library::cover_edit_tracks(self, mpmp_library::ActionId(action.0))
                .map_err(library_error)?,
        );
        Ok(contract_ids(ids))
    }
}
