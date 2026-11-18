use super::*;

/// Settings read back from the state file, kept within what the commands accept.
fn sanitize_audio(mut audio: SavedAudio) -> SavedAudio {
    if !(0.0..=1.0).contains(&audio.volume) {
        audio.volume = 1.0;
    }
    if let Some(rg) = &mut audio.replay_gain {
        for db in [&mut rg.preamp_db, &mut rg.fallback_preamp_db] {
            if !(-24.0..=24.0).contains(db) {
                *db = 0.0;
            }
        }
    }
    audio
}

impl Actor {
    pub(super) fn restore(&mut self, now: u64) {
        let saved = match &self.persistence.state_file {
            Some(file) => file.load(),
            None => SavedState::default(),
        };
        // Ids and revisions never go back (contract guarantee 6).
        self.playback.queue = Queue::with_seeds(
            saved.next_entry_id.max(now),
            saved.queue_revision.saturating_add(1).max(now),
        );
        let audio = sanitize_audio(saved.audio);
        self.restore_output(audio.output);
        self.audio.volume = audio.volume;
        self.audio.muted = audio.muted;
        self.audio.replay_gain = audio
            .replay_gain
            .map(SavedReplayGain::to_contract)
            .unwrap_or_default();
        let queue = saved.queue;
        self.playback.queue.set_shuffle(queue.shuffle);
        self.playback.queue.set_repeat(
            queue
                .repeat
                .map_or(RepeatMode::Off, SavedRepeat::to_contract),
        );
        if queue.entries.is_empty() {
            return;
        }
        let context = queue.context.as_ref().and_then(SavedContext::to_contract);
        self.playback.queue.restore(&queue.entries(), context);
        let current = queue
            .current_index
            .and_then(|i| self.playback.queue.entries().get(i as usize).map(|e| e.id));
        if let Some(entry) = current {
            self.playback.queue.set_current(Some(entry));
            self.playback.resume_at = Some((entry, queue.position_ms));
        }
    }

    pub(super) fn save_now(&mut self) {
        self.persistence.last_save = Instant::now();
        self.persistence.dirty = false;
        if self.persistence.state_file.is_none() {
            return;
        }
        // Keep identity and position from one sample, even when the engine is ahead
        // of the actor's queue cursor at a gapless boundary. Removed entries have no
        // saved current position; never attach their offset to another entry.
        let sample = match self.playback.state {
            PlayState::Stopped => self.playback.resume_at,
            _ => self.position().map(|p| (p.entry, p.position_ms)),
        }
        .or_else(|| self.playback.queue.current().map(|entry| (entry, 0)));
        let saved_position = sample
            .and_then(|(entry, ms)| self.playback.queue.index_of(entry).map(|index| (index, ms)));
        let (current_index, position_ms) = saved_position
            .map(|(index, ms)| (Some(u32::try_from(index).unwrap_or(u32::MAX)), ms))
            .unwrap_or((None, 0));
        let state = SavedState {
            queue: SavedQueue {
                entries: self
                    .playback
                    .queue
                    .entries()
                    .iter()
                    .map(|e| SavedEntry {
                        id: e.id.0,
                        track: e.track.0,
                    })
                    .collect(),
                current_index,
                position_ms,
                context: self
                    .playback
                    .queue
                    .context()
                    .and_then(SavedContext::from_contract),
                shuffle: self.playback.queue.shuffle(),
                repeat: Some(SavedRepeat::from_contract(self.playback.queue.repeat())),
            },
            audio: SavedAudio {
                volume: self.audio.volume,
                muted: self.audio.muted,
                replay_gain: Some(SavedReplayGain::from_contract(&self.audio.replay_gain)),
                output: Some(crate::state_file::SavedOutput {
                    settings: self.output.output_settings.clone(),
                    device: self.output.output_selected.clone(),
                }),
            },
            next_entry_id: self.playback.queue.next_entry_id(),
            queue_revision: self.playback.queue.revision().0,
        };
        if let Err(err) = self.persistence.state_file.as_mut().unwrap().submit(state) {
            tracing::warn!("saving playback state: {err}");
        }
    }
}
