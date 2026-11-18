use super::*;

/// What `run_command` leaves to do.
pub(super) enum Flow {
    Withdrawn,
    Done(Result<Receipt, ContractError>),
    /// An agent's command, to check off the actor.
    Check(Check),
    /// The user approved this held command: to check again off the actor.
    Recheck {
        token: String,
        held: Box<Held>,
    },
}

impl Actor {
    /// Checks who may run the command, then runs it (or holds it for the user), or leaves
    /// the checks that call the file system to run off the actor.
    pub(super) fn run_command(
        &mut self,
        command: Command,
        origin: &Origin,
        registered: Option<u64>,
        key: Option<IdempotencyKey>,
    ) -> Flow {
        match self.try_run_command(command, origin, registered, key) {
            Ok(flow) => flow,
            Err(_) if self.withdrawn(origin, registered) => Flow::Withdrawn,
            Err(error) => Flow::Done(Err(error)),
        }
    }

    fn try_run_command(
        &mut self,
        command: Command,
        origin: &Origin,
        registered: Option<u64>,
        key: Option<IdempotencyKey>,
    ) -> Result<Flow, ContractError> {
        if command == Command::Unknown {
            return Err(ContractError::unsupported(
                ErrorCode::UnknownCommand,
                "this core does not know that command",
            ));
        }
        let agent = matches!(origin, Origin::Agent { .. });
        if (agent || manages_permissions(&command))
            && !lock_permissions(&self.access.permissions).available()
        {
            return Err(ContractError::failed(
                ErrorCode::PermissionsUnavailable,
                "the agents and grants could not be read when core started",
            ));
        }
        let policy = self.policy(&command, origin)?;
        if let Policy::Forbidden(code) = policy {
            let why = match code {
                ErrorCode::PathNotAllowed => "may not name local paths",
                _ => "may not send this command",
            };
            return Err(ContractError::forbidden(
                code,
                format!("{} {why} ({})", origin.kind(), command.name()),
            ));
        }
        // Approving a held agent command that acts on a location or a music folder: it is
        // checked again off the actor (the `Confirm` is answered once that is done). The
        // user's answer is taken now: it can no longer expire or be answered again.
        if let Command::Confirm { token } = &command {
            self.expire_confirmations();
            if self
                .confirmations
                .held
                .get(token)
                .is_some_and(|h| h.targets.is_some() && checks::slow(&h.command))
                && let Some(held) = self.confirmations.held.remove(token)
            {
                return Ok(Flow::Recheck {
                    token: token.clone(),
                    held: Box::new(held),
                });
            }
        }
        // The user's grants decide for an agent (`Check`): those checks that call the file
        // system (with the command's time limit: an offline network location must not hold
        // core up) run off the actor; the UI's commands call the platform layer directly.
        if agent {
            let class = self.class_of(&command)?;
            let candidates = self.candidates(&command, origin, class)?;
            let check = Check {
                command,
                origin: origin.clone(),
                policy,
                class,
                candidates,
                catalog: Arc::clone(&self.catalog),
                locations: Arc::clone(&self.access.locations),
                limits: self.access.agent_fs.clone(),
            };
            if checks::slow(&check.command)
                && (policy == Policy::Confirm || !check.candidates.is_empty())
            {
                return Ok(Flow::Check(check));
            }
            // Nothing to ask the file system: checked here at once.
            return Ok(self.conclude(check.run(false), origin, registered, key));
        }
        if policy == Policy::Confirm {
            // Never ask the user to approve a command that cannot run, or one that would
            // change nothing (it runs right away and reports `changed: false`).
            checks::precheck(&self.catalog, &command, None)?;
            if !checks::is_noop(&self.catalog, &command, None) {
                let mut command = command;
                let ctx = CheckContext {
                    catalog: self.catalog.as_ref(),
                    locations: self.access.locations.as_ref(),
                };
                // What a UI needs to offer a grant for this; best effort (it only informs).
                let class = self.class_of(&command).unwrap_or_else(|_| command.class());
                let targets = ctx.targets(&mut command).unwrap_or_default();
                let view = ctx.view(&command, &targets);
                return Err(self.hold(command, (origin, registered), key, class, None, view));
            }
        }
        Ok(self.command(command, origin, registered, None, None, None))
    }

    /// The grants that may apply to an agent's command: those for the command, for what it
    /// amounts to (undoing a folder change is adding or removing that folder), or for its
    /// class.
    pub(super) fn candidates(
        &self,
        command: &Command,
        origin: &Origin,
        class: OperationClass,
    ) -> Result<Vec<Grant>, ContractError> {
        let Origin::Agent { name, session } = origin else {
            return Ok(Vec::new());
        };
        let mut names = vec![command.name()];
        if let Command::UndoAction { action } = command {
            if let Some(same) = self.catalog.undo_root_command(*action)? {
                names.push(same);
            } else if !self.catalog.tag_undo_tracks(*action)?.is_empty() {
                names.push(if !self.catalog.cover_undo_tracks(*action)?.is_empty() {
                    "edit_cover"
                } else {
                    "edit_tags"
                });
            }
        }
        Ok(lock_permissions(&self.access.permissions).candidates(
            name,
            session.as_deref(),
            &names,
            class,
            unix_ms(),
        ))
    }

    /// The command's class as core runs it: an undo of a folder change is `local_path`, of
    /// a tag edit `file_write` (guarantee 12).
    pub(super) fn class_of(&self, command: &Command) -> Result<OperationClass, ContractError> {
        if let Command::UndoAction { action } = command {
            if self.catalog.undo_root_command(*action)?.is_some() {
                return Ok(OperationClass::LocalPath);
            }
            if !self.catalog.tag_undo_tracks(*action)?.is_empty() {
                return Ok(OperationClass::FileWrite);
            }
        }
        Ok(command.class())
    }

    /// The command's policy for `origin` (the default table in the contract), refined
    /// where the table cannot know: undoing an action follows the policy of the command
    /// that made it, so undoing a music-folder change follows the local-path rules (agents
    /// ask the user, remotes may not), and undoing a tag edit the file-write rules (agents
    /// and remotes ask the user).
    pub(super) fn policy(
        &self,
        command: &Command,
        origin: &Origin,
    ) -> Result<Policy, ContractError> {
        let policy = command.policy(origin);
        if let Command::UndoAction { action } = command
            && policy == Policy::Allow
            && matches!(origin, Origin::Agent { .. } | Origin::Remote { .. })
        {
            if self.catalog.undo_root_command(*action)?.is_some() {
                return Ok(match origin {
                    Origin::Remote { .. } => Policy::Forbidden(ErrorCode::PathNotAllowed),
                    _ => Policy::Confirm,
                });
            }
            if !self.catalog.tag_undo_tracks(*action)?.is_empty() {
                return Ok(Policy::Confirm);
            }
        }
        Ok(policy)
    }

    /// Runs a command whose origin has been checked; `grant`: the user's grant that let it
    /// run without asking; `checked`: for an agent, what it acts on as checked (and shown
    /// to the user), which a tag job writes and nothing else.
    pub(super) fn command(
        &mut self,
        command: Command,
        origin: &Origin,
        registered: Option<u64>,
        grant: Option<GrantId>,
        checked: Option<&[Target]>,
        purge: Option<&mpmp_library::PurgeGuard>,
    ) -> Flow {
        if self.withdrawn(origin, registered) {
            return Flow::Withdrawn;
        }
        Flow::Done(if purge.is_some() {
            self.apply_command_guarded(command, origin, grant, checked, purge)
        } else {
            self.apply_command(command, origin, grant, checked)
        })
    }

    pub(super) fn apply_command(
        &mut self,
        command: Command,
        origin: &Origin,
        grant: Option<GrantId>,
        checked: Option<&[Target]>,
    ) -> Result<Receipt, ContractError> {
        self.apply_command_guarded(command, origin, grant, checked, None)
    }

    fn apply_command_guarded(
        &mut self,
        command: Command,
        origin: &Origin,
        grant: Option<GrantId>,
        checked: Option<&[Target]>,
        purge: Option<&mpmp_library::PurgeGuard>,
    ) -> Result<Receipt, ContractError> {
        let failure_before = self.output.output_failure_serial;
        let before = self.playback.queue.revision();
        let mut receipt = Receipt::default();
        let changed = match command {
            Command::SetOutputDevice {
                selection,
                expected_revision,
            } => {
                // Applying a committed preference may fail; the receipt remains success.
                let changed = self.set_output_device(selection, expected_revision)?;
                receipt.changed = changed;
                receipt.queue_revision = self.playback.queue.revision();
                return Ok(receipt);
            }
            Command::SetOutputMode {
                mode,
                fallback,
                rate_policy,
                expected_revision,
            } => self.command_set_output_mode(mode, fallback, rate_policy, expected_revision)?,
            Command::SetStopAfterCurrent {
                enabled,
                expected_entry,
            } => self.command_set_stop_after_current(enabled, expected_entry)?,
            Command::SetSleepTimer { duration_ms } => self.command_set_sleep_timer(duration_ms)?,
            Command::Play {
                context,
                start,
                start_track,
            } => {
                let (tracks, item_index) = if matches!(context, PlayContext::Playlist { .. }) {
                    self.catalog.playlist_playback(&context)?
                } else {
                    (self.context_tracks(&context)?, None)
                };
                if tracks.is_empty() {
                    return Err(ContractError::not_found(
                        ErrorCode::EmptyContext,
                        "the play context has no playable tracks",
                    )
                    .with_field("command.context"));
                }
                let index = match (item_index, start_track) {
                    (Some(index), _) => index,
                    (None, Some(track)) => {
                        tracks.iter().position(|&t| t == track).ok_or_else(|| {
                            ContractError::not_found(
                                ErrorCode::TrackNotFound,
                                format!("track {} is not in the play context", track.0),
                            )
                            .with_missing(vec![ObjectRef::Track { id: track }])
                            .with_field("command.start_track")
                        })?
                    }
                    (None, None) if start as usize >= tracks.len() => {
                        return Err(ContractError::invalid(
                            ErrorCode::OutOfRange,
                            format!(
                                "start {start} is past the end of a {}-track context",
                                tracks.len()
                            ),
                        )
                        .with_field("command.start"));
                    }
                    (None, None) => start as usize,
                };
                if self.limits.stop_after_current.is_some() {
                    let mut staged = self.playback.queue.clone();
                    let created = staged.replace(&tracks, Some(context));
                    staged.start_at(created[index]);
                    self.commit_guarded_navigation(
                        staged,
                        created[index],
                        PlayState::Playing,
                        origin,
                    )?;
                    receipt.created_entries = created;
                    receipt.changed = true;
                    receipt.queue_revision = self.playback.queue.revision();
                    return Ok(receipt);
                }
                self.finish_playing(EndReason::Skipped, origin);
                let created = self.playback.queue.replace(&tracks, Some(context));
                self.playback.queue.start_at(created[index]);
                self.play_or_skip(created[index], 0, origin);
                receipt.created_entries = created;
                true
            }
            Command::PlayEntry { entry } => self.command_play_entry(entry, origin)?,
            Command::Pause => self.command_pause()?,
            Command::Resume => self.command_resume(origin)?,
            Command::TogglePause => self.command_toggle_pause(origin)?,
            Command::Stop => self.command_stop(origin)?,
            Command::Next => self.command_next(origin)?,
            Command::Previous => self.command_previous(origin)?,
            Command::Seek { position_ms, entry } => self.command_seek(position_ms, entry)?,
            Command::Enqueue {
                tracks,
                at,
                expected_revision,
                expected_current,
            } => self.command_enqueue(
                tracks,
                at,
                expected_revision,
                expected_current,
                &mut receipt,
            )?,
            Command::Remove {
                entries,
                expected_revision,
            } => self.command_remove(entries, expected_revision, &mut receipt)?,
            Command::Move {
                entry,
                to,
                expected_revision,
            } => self.command_move(entry, to, expected_revision)?,
            Command::Clear { expected_revision } => self.command_clear(expected_revision)?,
            Command::SetShuffle { shuffle } => self.command_set_shuffle(shuffle)?,
            Command::SetRepeat { mode } => self.command_set_repeat(mode)?,
            Command::Undo { revision } => self.command_undo(revision)?,
            Command::AddRoot { path } => {
                self.command_add_root(path, origin, grant, &mut receipt)?
            }
            Command::RemoveRoot { root } => {
                self.command_remove_root(root, origin, grant, &mut receipt)?
            }
            Command::Rescan => self.command_rescan(origin, &mut receipt)?,
            Command::SetRating { tracks, rating } => {
                self.command_set_rating(tracks, rating, origin, &mut receipt)?
            }
            Command::SetLoved { tracks, loved } => {
                self.command_set_loved(tracks, loved, origin, &mut receipt)?
            }
            Command::UndoAction { action } => {
                self.command_undo_action(action, origin, grant, checked, &mut receipt)?
            }
            Command::ReleaseRoot { root } => {
                self.command_release_root(root, origin, grant, &mut receipt)?
            }
            Command::ResumeRoot { root } => {
                self.command_resume_root(root, origin, grant, &mut receipt)?
            }
            Command::ResolveCoverRecovery {
                track, resolution, ..
            } => self.command_resolve_cover_recovery(track, resolution, origin, &mut receipt)?,
            Command::EditCover { tracks, change } => {
                self.command_edit_cover(tracks, change, origin, grant, checked, &mut receipt)?
            }
            Command::EditTags { tracks, changes } => {
                self.command_edit_tags(tracks, changes, origin, grant, checked, &mut receipt)?
            }
            Command::Confirm { token } => self.command_confirm(token, &mut receipt)?,
            Command::Reject { token } => self.command_reject(token, &mut receipt)?,
            Command::SetVolume { volume } => self.command_set_volume(volume)?,
            Command::SetMuted { muted } => self.command_set_muted(muted)?,
            Command::SetVolumeMode { mode } => self.command_set_volume_mode(mode)?,
            Command::SetReplayGain {
                mode,
                preamp_db,
                fallback_preamp_db,
                prevent_clipping,
            } => {
                self.command_set_replay_gain(mode, preamp_db, fallback_preamp_db, prevent_clipping)?
            }
            command @ (Command::CreatePlaylist { .. }
            | Command::RenamePlaylist { .. }
            | Command::DeletePlaylist { .. }
            | Command::RestorePlaylist { .. }
            | Command::PurgePlaylists { .. }
            | Command::AddToPlaylist { .. }
            | Command::RemoveFromPlaylist { .. }
            | Command::MovePlaylistItem { .. }
            | Command::SetSmartQuery { .. }
            | Command::UndoPlaylist { .. }
            | Command::ImportM3u { .. }
            | Command::ExportM3u { .. }) => {
                self.command_playlist(command, origin, grant, purge, &mut receipt)?
            }
            Command::RegisterAgent { name } => self.command_register_agent(name, &mut receipt)?,
            Command::RenewAgentToken { agent } => {
                self.command_renew_agent_token(agent, &mut receipt)?
            }
            Command::RemoveAgent { agent } => self.command_remove_agent(agent)?,
            Command::GrantPermission {
                agent,
                session,
                operation,
                scope,
                until,
                effect,
            } => self.command_grant_permission(
                agent,
                session,
                operation,
                scope,
                until,
                effect,
                &mut receipt,
            )?,
            Command::RevokePermission { grant } => self.command_revoke_permission(grant)?,
            Command::Unknown => self.command_unknown()?,
        };
        if receipt.action.is_some() {
            self.emit(Event::ActionsChanged);
        }
        let queue_changed = self.playback.queue.revision() != before;
        if queue_changed {
            if self
                .limits
                .stop_after_current
                .and_then(|id| self.playback.instances.get(&id))
                .is_some_and(|i| self.playback.queue.entry(i.entry).is_none())
            {
                self.cancel_stop_after_current();
            }
            self.persistence.dirty = true;
            if self
                .playback
                .resume_at
                .is_some_and(|(entry, _)| self.playback.queue.entry(entry).is_none())
            {
                self.playback.resume_at = None;
            }
            self.refresh_next();
            // Smart ReplayGain depends on the neighbours in the queue.
            self.regain_if_smart();
        }
        receipt.changed = changed || queue_changed;
        receipt.queue_revision = self.playback.queue.revision();
        if self.output.output_failure_serial != failure_before
            && let Some(error) = &self.output.output_error
        {
            return Err(error.clone());
        }
        Ok(receipt)
    }
}
