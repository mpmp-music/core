use super::super::*;
use crate::contract::*;

impl Actor {
    pub(in crate::actor) fn command_enqueue(
        &mut self,
        tracks: Vec<TrackId>,
        at: InsertAt,
        expected_revision: Option<QueueRevision>,
        expected_current: Option<EntryId>,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            if at == InsertAt::Unknown {
                // Also for no tracks: an enum is checked even when nothing would change.
                return Err(
                    ContractError::unknown_variant("insert position").with_field("command.at")
                );
            }
            self.check_tracks_exist(&tracks, "command.tracks")?;
            if let Some(expected) = expected_current {
                if at != InsertAt::Next {
                    return Err(ContractError::invalid(
                        ErrorCode::InvalidRequest,
                        "expected_current requires at: next",
                    )
                    .with_field("command.expected_current"));
                }
                if self.playback.queue.current() != Some(expected) {
                    return Err(ContractError::new(
                        ErrorKind::Conflict,
                        ErrorCode::EntryNotCurrent,
                        "the enqueue anchor is no longer current",
                    )
                    .with_field("command.expected_current"));
                }
            }
            receipt.created_entries =
                self.playback
                    .queue
                    .enqueue(&tracks, at, expected_revision)?;
            !receipt.created_entries.is_empty()
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_remove(
        &mut self,
        entries: Vec<EntryId>,
        expected_revision: Option<QueueRevision>,
        receipt: &mut Receipt,
    ) -> Result<bool, ContractError> {
        let changed = {
            receipt.removed_entries = self.playback.queue.remove(&entries, expected_revision)?;
            !receipt.removed_entries.is_empty()
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_move(
        &mut self,
        entry: EntryId,
        to: u32,
        expected_revision: Option<QueueRevision>,
    ) -> Result<bool, ContractError> {
        let changed = self
            .playback
            .queue
            .move_entry(entry, to as usize, expected_revision)?;
        Ok(changed)
    }

    pub(in crate::actor) fn command_clear(
        &mut self,
        expected_revision: Option<QueueRevision>,
    ) -> Result<bool, ContractError> {
        let changed = self.playback.queue.clear(expected_revision)?;
        Ok(changed)
    }

    pub(in crate::actor) fn command_set_shuffle(
        &mut self,
        shuffle: bool,
    ) -> Result<bool, ContractError> {
        let changed = {
            let changed = self.playback.queue.set_shuffle(shuffle);
            if changed {
                // The engine may hold the old next track already.
                self.refresh_next();
                self.regain_if_smart();
                self.persistence.dirty = true;
                self.emit_state();
            }
            changed
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_set_repeat(
        &mut self,
        mode: RepeatMode,
    ) -> Result<bool, ContractError> {
        let changed = {
            if mode == RepeatMode::Unknown {
                return Err(
                    ContractError::unknown_variant("repeat mode").with_field("command.mode")
                );
            }
            let changed = self.playback.queue.set_repeat(mode);
            if changed {
                // What follows the current track depends on the mode (repeat one holds
                // it); the engine may hold the old next track already.
                self.refresh_next();
                self.persistence.dirty = true;
                self.emit_state();
            }
            changed
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_undo(
        &mut self,
        revision: QueueRevision,
    ) -> Result<bool, ContractError> {
        let changed = {
            self.playback.queue.undo(revision)?;
            true
        };
        Ok(changed)
    }
}
