use super::*;

impl Actions for Library {
    fn set_rating(
        &self,
        tracks: &[TrackId],
        rating: Option<u8>,
        origin: &Origin,
    ) -> Result<Recorded<Vec<TrackId>>, ContractError> {
        let label = origin.label();
        let done = Library::set_rating(self, &library_ids(tracks), rating, ActionMeta::new(&label))
            .map_err(|e| library_error(e).with_field("command.rating"))?;
        Ok(recorded(done).map(contract_ids))
    }

    fn set_loved(
        &self,
        tracks: &[TrackId],
        loved: bool,
        origin: &Origin,
    ) -> Result<Recorded<Vec<TrackId>>, ContractError> {
        let label = origin.label();
        let done = Library::set_loved(self, &library_ids(tracks), loved, ActionMeta::new(&label))
            .map_err(library_error)?;
        Ok(recorded(done).map(contract_ids))
    }

    fn preview_undo(&self, action: ActionId) -> Result<QueryResult, ContractError> {
        if !Library::cover_edit_tracks(self, mpmp_library::ActionId(action.0))
            .map_err(library_error)?
            .is_empty()
        {
            return Library::preview_cover_undo(self, mpmp_library::ActionId(action.0))
                .map(cover_preview)
                .map_err(|e| undo_error(e, action));
        }

        let preview = Library::preview_tag_undo(self, mpmp_library::ActionId(action.0))
            .map_err(|e| undo_error(e, action))?;
        Ok(QueryResult::TagEditPreview {
            files: preview
                .files
                .into_iter()
                .map(|f| TagFilePreview {
                    changed: Some(f.changed),
                    track: TrackId(f.track.0),
                    before: f.before,
                    after: f.after,
                    encoding_conversion: Some(f.encoding_conversion),
                    display_location: Some(f.display_location),
                })
                .collect(),
            skipped: tag_results(&preview.skipped),
        })
    }

    fn describe_undo(
        &self,
        action: ActionId,
    ) -> Result<mpmp_library::UndoDescription, ContractError> {
        Library::describe_undo(self, mpmp_library::ActionId(action.0))
            .map_err(|e| undo_error(e, action))
    }

    fn undo_action_guarded(
        &self,
        expected: mpmp_library::UndoDescription,
        origin: &Origin,
        grant: Option<GrantId>,
    ) -> Result<Undone, ContractError> {
        let action = ActionId(expected.action.0);
        let label = origin.label();
        let done = Library::undo_action_guarded(self, expected, action_meta(&label, grant, origin))
            .map_err(|e| undo_error(e, action))?;
        let effect = done.value;
        let active = self.roots().map_err(library_error)?;
        let rescan = effect
            .roots
            .iter()
            .any(|r| active.iter().any(|a| a.id == *r));
        Ok(Undone {
            action: done.action.map_or(action, action_id),
            tracks: contract_ids(effect.tracks),
            playlists: effect.playlists.iter().map(|p| PlaylistId(p.0)).collect(),
            roots: effect.roots.iter().map(|r| RootId(r.0)).collect(),
            rescan,
        })
    }

    fn undo_root_command(&self, action: ActionId) -> Result<Option<&'static str>, ContractError> {
        // The action's own folder targets (an undo or redo copies those of what it
        // reverses), so nothing depends on older actions, which may have been pruned.
        // Undoing it flips the folders back: a removed one is added back, one in use is
        // removed (any other state is a conflict and the undo changes nothing).
        let roots =
            Library::action_roots(self, mpmp_library::ActionId(action.0)).map_err(library_error)?;
        let mut same = None;
        for root in roots {
            let removed = self.root_removed(root).map_err(library_error)?;
            let command = match removed {
                Some(true) => "add_root",
                Some(false) => "remove_root",
                None => {
                    return Err(ContractError::failed(
                        ErrorCode::CorruptData,
                        format!(
                            "action {} changed library folder {}, which is unknown: what \
                             undoing it amounts to cannot be told",
                            action.0, root.0
                        ),
                    ));
                }
            };
            if same.is_some_and(|other| other != command) {
                // Not one command (a later version's action, say): refused rather than
                // judged by one of them.
                return Err(ContractError::failed(
                    ErrorCode::CorruptData,
                    format!(
                        "action {} changed library folders that are now some removed, some \
                         in use: what undoing it amounts to cannot be told",
                        action.0
                    ),
                ));
            }
            same = Some(command);
        }
        Ok(same)
    }

    fn action_roots(&self, action: ActionId) -> Result<Vec<RootId>, ContractError> {
        Ok(
            Library::action_roots(self, mpmp_library::ActionId(action.0))
                .map_err(library_error)?
                .into_iter()
                .map(|r| RootId(r.0))
                .collect(),
        )
    }

    fn undo_action(
        &self,
        action: ActionId,
        origin: &Origin,
        grant: Option<GrantId>,
    ) -> Result<Undone, ContractError> {
        let label = origin.label();
        let done = Library::undo_action(
            self,
            mpmp_library::ActionId(action.0),
            action_meta(&label, grant, origin),
        )
        .map_err(|e| undo_error(e, action))?;
        let effect = done.value;
        // A folder that is active after the undo came back: it needs a scan.
        let rescan = !effect.roots.is_empty() && {
            let active = self.roots().map_err(library_error)?;
            effect
                .roots
                .iter()
                .any(|r| active.iter().any(|a| a.id == *r))
        };
        Ok(Undone {
            // An undo always records itself.
            action: done.action.map_or(action, action_id),
            tracks: contract_ids(effect.tracks),
            playlists: effect.playlists.iter().map(|p| PlaylistId(p.0)).collect(),
            roots: effect.roots.iter().map(|r| RootId(r.0)).collect(),
            rescan,
        })
    }
}
