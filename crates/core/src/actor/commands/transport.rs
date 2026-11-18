use super::super::*;
use crate::contract::*;

impl Actor {
    pub(in crate::actor) fn command_pause(&mut self) -> Result<bool, ContractError> {
        let changed = self.pause();
        Ok(changed)
    }

    pub(in crate::actor) fn command_resume(
        &mut self,
        origin: &Origin,
    ) -> Result<bool, ContractError> {
        let changed = self.resume(origin);
        Ok(changed)
    }

    pub(in crate::actor) fn command_stop(
        &mut self,
        origin: &Origin,
    ) -> Result<bool, ContractError> {
        let changed = self.stop(origin);
        Ok(changed)
    }

    pub(in crate::actor) fn command_play_entry(
        &mut self,
        entry: EntryId,
        origin: &Origin,
    ) -> Result<bool, ContractError> {
        let changed = {
            self.playback
                .queue
                .entry(entry)
                .ok_or_else(|| entry_not_found(entry))?;
            if self.limits.stop_after_current.is_some() {
                let mut staged = self.playback.queue.clone();
                staged.start_at(entry);
                self.commit_guarded_navigation(staged, entry, PlayState::Playing, origin)?;
            } else {
                self.finish_playing(EndReason::Skipped, origin);
                self.playback.queue.start_at(entry);
                self.play_or_skip(entry, 0, origin);
            }
            true
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_toggle_pause(
        &mut self,
        origin: &Origin,
    ) -> Result<bool, ContractError> {
        let changed = {
            if self.playback.state == PlayState::Playing {
                self.pause()
            } else {
                self.resume(origin)
            }
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_next(
        &mut self,
        origin: &Origin,
    ) -> Result<bool, ContractError> {
        let changed = {
            let next = self.playback.queue.skip_target();
            let active =
                self.playback.playing.is_some() || self.playback.state != PlayState::Stopped;
            match next {
                Some(entry) => {
                    if self.limits.stop_after_current.is_some() {
                        let staged = self.playback.queue.clone();
                        self.commit_guarded_navigation(staged, entry, self.playback.state, origin)?;
                    } else {
                        self.skip_to(entry, origin);
                    }
                }
                None if active => {
                    self.cancel_stop_after_current();
                    self.finish_transport(EndReason::Skipped, false, origin);
                }
                None => {}
            }
            active || next.is_some()
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_previous(
        &mut self,
        origin: &Origin,
    ) -> Result<bool, ContractError> {
        let changed = {
            let position = self.snapshot_position().map_or(0, |p| p.position_ms);
            let current = self.playback.queue.current();
            let previous = current.and_then(|c| self.playback.queue.previous_before(c));
            match (current, previous) {
                (Some(_), Some(previous)) if position < RESTART_THRESHOLD_MS => {
                    if self.limits.stop_after_current.is_some() {
                        let staged = self.playback.queue.clone();
                        self.commit_guarded_navigation(
                            staged,
                            previous,
                            self.playback.state,
                            origin,
                        )?;
                    } else {
                        self.skip_to(previous, origin);
                    }
                    true
                }
                (Some(current), _) => {
                    self.seek_to(current, 0)?;
                    self.cancel_stop_after_current();
                    true
                }
                (None, _) => false,
            }
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_seek(
        &mut self,
        position_ms: u64,
        entry: Option<EntryId>,
    ) -> Result<bool, ContractError> {
        let changed = {
            let current = self.current_entry().ok_or_else(|| {
                ContractError::invalid(ErrorCode::NothingPlaying, "nothing is playing")
            })?;
            if let Some(expected) = entry
                && expected != current
            {
                return Err(ContractError::conflict(
                    ErrorCode::EntryNotCurrent,
                    format!(
                        "entry {} is not the current entry ({})",
                        expected.0, current.0
                    ),
                    expected.0,
                    current.0,
                )
                .with_field("command.entry"));
            }
            if position_ms > MAX_SAFE_INTEGER {
                return Err(ContractError::invalid(
                    ErrorCode::OutOfRange,
                    format!("position {position_ms} ms is past 2^53 - 1"),
                )
                .with_field("command.position_ms"));
            }
            self.seek_to(current, position_ms)?;
            true
        };
        Ok(changed)
    }
}
