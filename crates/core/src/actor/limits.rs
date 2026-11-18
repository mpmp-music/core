use super::*;

impl Actor {
    pub(super) fn playback_limits_info(&self) -> PlaybackLimitsInfo {
        let now = self.now_ms();
        PlaybackLimitsInfo {
            stop_after_current: self
                .limits
                .stop_after_current
                .and_then(|id| self.playback.instances.get(&id))
                .map(|i| StopAfterCurrentInfo { entry: i.entry }),
            sleep_timer: self.limits.sleep_deadline.map(|deadline| SleepTimerInfo {
                remaining_ms: deadline.saturating_sub(self.limits.playback_clock.now_ms()),
                sampled_at_ms: now,
                includes_suspend: Some(self.limits.playback_clock.includes_suspend()),
            }),
        }
    }

    pub(super) fn cancel_stop_after_current(&mut self) {
        if self.limits.stop_after_current.take().is_some() {
            self.refresh_next();
            self.emit_state();
        }
    }

    pub(super) fn set_stop_after_current(
        &mut self,
        enabled: bool,
        expected: Option<EntryId>,
    ) -> Result<bool, ContractError> {
        if expected.is_some_and(|entry| entry.0 > MAX_SAFE_INTEGER) {
            return Err(ContractError::invalid(
                ErrorCode::OutOfRange,
                "expected entry is past 2^53 - 1",
            )
            .with_field("command.expected_entry"));
        }
        // Reject stale expectations using a read-only submitted cursor, before even
        // reducing queued engine facts. A flushed predecessor's declick is not the
        // newly accepted instance. Neither rejection nor a no-op touches output.
        let submitted = self
            .engine
            .rendered_position()
            .and_then(|p| self.playback.instances.get(&p.entry.0))
            .filter(|i| i.ended.is_none())
            .map(|i| i.entry);
        let current = submitted.or_else(|| {
            self.playback
                .playing
                .as_ref()
                .and_then(|p| self.playback.instances.get(&p.instance))
                .map(|i| i.entry)
        });
        if let Some(expected) = expected.filter(|e| Some(*e) != current) {
            let mut error = ContractError::new(
                ErrorKind::Conflict,
                ErrorCode::EntryNotCurrent,
                "the expected entry is no longer playing",
            );
            error.details.expected = Some(expected.0);
            error.details.actual = current.map(|e| e.0);
            return Err(error.with_field("command.expected_entry"));
        }
        if !enabled && self.limits.stop_after_current.is_none()
            || enabled
                && self
                    .playback
                    .playing
                    .as_ref()
                    .is_some_and(|p| self.limits.stop_after_current == Some(p.instance))
        {
            return Ok(false);
        }
        self.settle_rendered_starts();
        loop {
            let current = self
                .playback
                .playing
                .as_ref()
                .and_then(|p| self.playback.instances.get(&p.instance))
                .map(|i| i.entry);
            if let Some(expected) = expected.filter(|e| Some(*e) != current) {
                let mut error = ContractError::new(
                    ErrorKind::Conflict,
                    ErrorCode::EntryNotCurrent,
                    "the expected entry is no longer playing",
                );
                error.details.expected = Some(expected.0);
                error.details.actual = current.map(|e| e.0);
                return Err(error.with_field("command.expected_entry"));
            }
            if enabled && current.is_none() {
                return Err(ContractError::invalid(
                    ErrorCode::NothingPlaying,
                    "nothing is playing",
                ));
            }
            let target = enabled.then(|| self.playback.playing.as_ref().unwrap().instance);
            if self.limits.stop_after_current == target {
                return Ok(false);
            }
            if let Some(instance) = target {
                if !self
                    .engine
                    .truncate_after(self.playback.instances[&instance].output_track.clone())
                    .map_err(|e| engine_error(&e))?
                {
                    // A successor won the atomic commitment race. Settle that fact,
                    // then either reject expected_entry or capture the new instance.
                    self.settle_rendered_starts();
                    std::thread::yield_now();
                    continue;
                }
                self.playback.pending_next = None;
            }
            self.limits.stop_after_current = target;
            if !enabled {
                self.refresh_next();
            }
            self.emit_state();
            return Ok(true);
        }
    }

    fn settle_rendered_starts(&mut self) {
        self.engine.settle_rendered_starts();
        while let Ok(event) = self.engine_events.try_recv() {
            if matches!(event, EngineEvent::TrackStarted { .. }) {
                self.reduce_engine(event, false);
            } else {
                // Do not swallow autonomous failures/EOF while reading starts: their
                // normal policy must still settle history and prepare any successor.
                self.on_engine(event);
            }
        }
        self.announce_queue();
    }

    pub(super) fn reach_stop_limit(&mut self, reason: PlaybackLimitReason, ended: EndReason) {
        let cause = self.publication.cause.take();
        let entry = self.current_entry();
        self.limits.stop_after_current = None;
        self.limits.sleep_deadline = None;
        // Keep an expired gate closed until all old callbacks have retired.
        self.finish_transport(ended, true, &Origin::System);
        self.limits.playback_deadline.set(None);
        self.emit(Event::PlaybackLimitReached {
            reason,
            action: PlaybackLimitAction::Stop,
            entry,
        });
        self.publication.cause = cause;
    }

    pub(super) fn process_playback_limits(&mut self) {
        if !self.limits.playback_deadline.expired() {
            return;
        }
        let cause = self.publication.cause.take();
        // Observe already reached audio boundaries first. A coincident EOF wins.
        if self.playback.state == PlayState::Playing {
            while let Ok(event) = self.engine_events.try_recv() {
                self.on_engine(event);
            }
        }
        if self.limits.sleep_deadline.is_some() && self.limits.playback_deadline.expired() {
            // Freeze with the deadline gate still closed, before clearing it. No
            // callback after suspend may output fresh content before this reduction.
            let was_playing = self.playback.state == PlayState::Playing;
            if was_playing {
                self.close_output();
            }
            if self.limits.sleep_deadline.take().is_some() {
                let entry = self.current_entry();
                self.limits.stop_after_current = None;
                if self.playback.state == PlayState::Playing {
                    self.playback.state = PlayState::Paused;
                }
                if was_playing {
                    self.rebase_hint(false);
                }
                self.limits.playback_deadline.set(None);
                if was_playing {
                    self.persistence.dirty = true;
                    self.save_now();
                }
                self.emit_state();
                self.emit_position();
                self.emit(Event::PlaybackLimitReached {
                    reason: PlaybackLimitReason::SleepTimer,
                    action: PlaybackLimitAction::Pause,
                    entry,
                });
            }
        }
        self.publication.cause = cause;
    }
}
