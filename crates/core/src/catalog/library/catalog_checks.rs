use super::*;

impl CatalogChecks for Library {
    fn storage_write_capabilities(&self) -> mpmp_storage::WriteCapabilities {
        Library::storage_write_capabilities(self)
    }

    fn damaged(&self) -> Option<String> {
        Library::damaged(self)
    }

    fn is_noop(&self, command: &Command) -> bool {
        if let Command::EditCover { tracks, .. } = command {
            return tracks.is_empty();
        }
        if let Command::EditTags { tracks, changes } = command {
            return tracks.is_empty() || changes.is_empty();
        }
        if let Command::ReleaseRoot { root } | Command::ResumeRoot { root } = command {
            return self.root_removed(mpmp_library::RootId(root.0)).ok() == Some(Some(false))
                && self.root_released(mpmp_library::RootId(root.0))
                    == matches!(command, Command::ReleaseRoot { .. });
        }
        if let Command::RemoveRoot { root } = command {
            // Removing a removed folder again changes nothing.
            return matches!(
                self.root_removed(mpmp_library::RootId(root.0)),
                Ok(Some(true))
            );
        }
        let Command::AddRoot { path } = command else {
            return false;
        };
        // Roots are stored canonicalised; an active one with the same path means no change.
        let Ok(canonical) = std::fs::canonicalize(path) else {
            return false;
        };
        self.roots().is_ok_and(|roots| {
            roots
                .iter()
                .any(|r| std::fs::canonicalize(&r.path).is_ok_and(|p| p == canonical))
        })
    }

    fn precheck(&self, command: &Command) -> Result<(), ContractError> {
        match command {
            Command::ReleaseRoot { root } | Command::ResumeRoot { root } => {
                self.check_root_access(*root)?;
            }
            Command::RemoveRoot { root } => {
                // A removed root still exists (removing it again is a no-op).
                if self
                    .root_removed(mpmp_library::RootId(root.0))
                    .map_err(library_error)?
                    .is_none()
                {
                    return Err(root_not_found(*root).with_field("command.root"));
                }
            }
            Command::AddRoot { path } => {
                if !Path::new(path).is_dir() {
                    return Err(library_error(LibraryError::NotADirectory(path.into()))
                        .with_field("command.path"));
                }
            }
            Command::ImportM3u { path, .. } => match std::fs::metadata(path) {
                Err(_) => {
                    return Err(ContractError::not_found(
                        ErrorCode::FileNotFound,
                        format!("{path} does not exist"),
                    )
                    .with_field("command.path"));
                }
                // Only a regular file is read (a FIFO, folder or device never is).
                Ok(file) if !file.is_file() => {
                    return Err(ContractError::invalid(
                        ErrorCode::NotARegularFile,
                        format!("{path} is not a regular file"),
                    )
                    .with_field("command.path"));
                }
                Ok(_) => {}
            },
            Command::ExportM3u { playlist, path } => {
                if self
                    .playlist(mpmp_library::PlaylistId(playlist.0))
                    .map_err(library_error)?
                    .is_none()
                {
                    return Err(playlist_not_found(*playlist).with_field("command.playlist"));
                }
                // The file is written into an existing folder (none is created).
                let path = Path::new(path);
                let folder = path.parent().unwrap_or(path);
                if !folder.is_dir() {
                    return Err(library_error(LibraryError::NotADirectory(folder.into()))
                        .with_field("command.path"));
                }
            }
            Command::UndoAction { action } => {
                // Undone already: asking the user would only lead to `already_undone`.
                if let Some(summary) = self
                    .action(mpmp_library::ActionId(action.0))
                    .map_err(library_error)?
                    && summary.undone_by.is_some()
                {
                    return Err(ContractError::invalid(
                        ErrorCode::AlreadyUndone,
                        format!("action {} was undone already", action.0),
                    )
                    .with_field("command.action"));
                }
                // A tag edit whose files a later one rewrote: `action_conflict` (what the
                // files tell is only checked when it runs).
                let id = mpmp_library::ActionId(action.0);
                if !self.tag_edit_tracks(id).map_err(library_error)?.is_empty() {
                    self.check_tag_undo(id)
                        .map_err(|e| undo_error(e, *action))?;
                }
            }
            Command::EditCover { tracks, change } => {
                check_cover_tracks(tracks)?;
                cover_change(change).map_err(|e| e.with_field("command.change"))?;
                check_tracks_exist(self, tracks, "command.tracks")?;
            }
            Command::EditTags { tracks, changes } => {
                check_tag_tracks(tracks, "command.tracks")?;
                tag_changes(changes, "command.")?;
                check_tracks_exist(self, tracks, "command.tracks")?;
            }
            Command::PurgePlaylists {
                playlists: Some(ids),
            } => {
                for (i, p) in ids.iter().enumerate() {
                    let field = format!("command.playlists[{i}]");
                    match self
                        .playlist(mpmp_library::PlaylistId(p.0))
                        .map_err(library_error)?
                    {
                        None => return Err(playlist_not_found(*p).with_field(field)),
                        Some(summary) if summary.deleted_at.is_none() => {
                            return Err(ContractError::invalid(
                                ErrorCode::NotInTrash,
                                format!("playlist {} is not in the trash", p.0),
                            )
                            .with_field(field));
                        }
                        Some(_) => {}
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}
