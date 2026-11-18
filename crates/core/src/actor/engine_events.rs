use super::*;

impl Actor {
    pub(super) fn on_engine(&mut self, event: EngineEvent) {
        self.reduce_engine(event, true);
        self.announce_queue();
    }

    /// Frozen batches commit history and transitions without issuing output commands.
    pub(super) fn reduce_engine(
        &mut self,
        event: EngineEvent,
        allow_output: bool,
    ) -> Option<EntryId> {
        match event {
            EngineEvent::TrackStarted { entry, signal_path } => {
                // Resume authorizes playback before a legitimate start arrives.
                // A report alone can never grant a stopped session playback rights.
                if self.playback.state == PlayState::Stopped {
                    return None;
                }
                let instance = entry.0;
                let heard = self.playback.instances.get(&instance).cloned()?;
                let announced = self.playback.instances.get_mut(&instance)?;
                announced.start_announced = true;
                announced.deferred_start = false;
                if heard.ended.is_some() {
                    // A late start commits rendered audio, never recovery authority.
                    if !heard.start_announced {
                        let path =
                            self.with_gain_stages(convert_signal_path(&signal_path), heard.gain);
                        self.emit(Event::TrackStarted {
                            entry: heard.entry,
                            track: Box::new(heard.track),
                            signal_path: Some(Box::new(path)),
                        });
                    }
                    return None;
                }
                let same = self
                    .playback
                    .playing
                    .as_ref()
                    .is_some_and(|p| p.instance == instance);
                if !same {
                    self.playback.failed_next.clear();
                    self.playback.seek_hint.set(None);
                    // A gapless transition: the previous instance played to its end.
                    if let Some(previous) = self.playback.playing.take() {
                        self.finish_instance(previous, EndReason::Completed, None);
                    }
                    self.playback.playing = Some(Playing::new(instance));
                    if self.playback.pending_next == Some(instance) {
                        self.playback.pending_next = None;
                    }
                }
                let announce_start = !heard.start_announced;
                if announce_start
                    && heard.deferred_start
                    && let Some(playing) = &mut self.playback.playing
                {
                    playing.started_at_ms = unix_ms();
                }
                let signal_path = convert_signal_path(&signal_path);
                if let Some(playing) = &mut self.playback.playing {
                    playing.signal_path = Some(signal_path.clone());
                }
                self.playback.queue.set_current(Some(heard.entry));
                self.playback.state = if !allow_output && self.playback.state == PlayState::Paused
                    || self.engine.is_paused()
                {
                    PlayState::Paused
                } else {
                    PlayState::Playing
                };
                self.persistence.dirty = true;
                if allow_output {
                    self.forget_old_instances(instance);
                    self.refresh_next();
                }
                let signal_path = self.with_gain_stages(signal_path, heard.gain);
                if announce_start {
                    self.emit(Event::TrackStarted {
                        entry: heard.entry,
                        track: Box::new(heard.track),
                        signal_path: Some(Box::new(signal_path)),
                    });
                }
                self.emit_state();
                self.emit_position();
            }
            EngineEvent::NextAlreadyStarted { .. } => {
                // The engine kept the old next (it is already heard) and dropped the new
                // one; its TrackStarted follows and sets the next again.
                self.playback.pending_next = None;
            }
            EngineEvent::TrackFailed { entry, reason } => {
                let instance = entry.0;
                let failed = self.playback.instances.get(&instance).cloned()?;
                if failed.ended.is_some() {
                    return None;
                }
                if self.playback.pending_next == Some(instance) {
                    // Skip it: queue what comes after it instead.
                    self.playback.pending_next = None;
                    self.playback.instances.get_mut(&instance)?.ended = Some(EndReason::Failed);
                    self.next_failed(failed.entry, engine_failure_code(&reason), reason);
                    if allow_output {
                        self.refresh_next();
                    }
                } else if self
                    .playback
                    .playing
                    .as_ref()
                    .is_some_and(|p| p.instance == instance)
                {
                    return self.fail_current(engine_failure_code(&reason), reason, allow_output);
                }
            }
            EngineEvent::EndOfQueue { entry } => {
                if self.playback.playing.as_ref()?.instance != entry.0 {
                    return None;
                }
                if self.limits.stop_after_current
                    == self.playback.playing.as_ref().map(|p| p.instance)
                {
                    self.reach_stop_limit(PlaybackLimitReason::EndOfCurrent, EndReason::Completed);
                    return None;
                }
                self.finish_playing(EndReason::Completed, &Origin::System);
                self.retire_session();
                self.playback.state = PlayState::Stopped;
                self.playback.resume_at = None;
                self.persistence.dirty = true;
                self.save_now();
                self.emit_state();
            }
        }
        None
    }

    pub(super) fn fail_current(
        &mut self,
        code: ErrorCode,
        message: String,
        allow_output: bool,
    ) -> Option<EntryId> {
        let playing = self.playback.playing.as_ref()?;
        let entry = self.playback.instances.get(&playing.instance)?.entry;
        let limited = self.limits.stop_after_current == Some(playing.instance);
        let deferred_start = self.playback.instances[&playing.instance].deferred_start;
        tracing::warn!("queue entry {} failed in the engine: {message}", entry.0);
        self.emit(Event::TrackFailed {
            entry,
            code,
            message,
            upcoming: false,
        });
        if limited {
            self.reach_stop_limit(PlaybackLimitReason::CurrentFailed, EndReason::Failed);
            return None;
        }
        self.finish_playing(EndReason::Failed, &Origin::System);
        self.output.output_at = None;
        if !allow_output {
            return Some(entry);
        }
        match self
            .playback
            .queue
            .next_after(Some(entry))
            .filter(|&e| e != entry)
        {
            Some(next) => {
                self.play_or_skip_with_budget(
                    next,
                    0,
                    &Origin::System,
                    true,
                    MAX_CONSECUTIVE_FAILURES,
                    deferred_start,
                );
            }
            None => {
                if self.engine.is_active() {
                    self.finish_transport(EndReason::Skipped, false, &Origin::System);
                } else {
                    self.stop_engine();
                }
            }
        }
        None
    }

    pub(super) fn forget_old_instances(&mut self, keep_from: u64) {
        let pending = self.playback.pending_next;
        self.playback
            .instances
            .retain(|&id, _| id >= keep_from || Some(id) == pending);
    }

    /// True when an automatic pass is ready to start (a retry fell due, or the scan it
    /// waited for has run long enough).
    pub(super) fn on_tick(&mut self) -> bool {
        self.reap_retired_decodes();
        let before = self.publication.seq;
        self.poll_output();
        if self
            .workers
            .automatic_retry_at
            .is_some_and(|at| Instant::now() >= at)
        {
            self.workers.automatic_retry_at = None;
            self.workers.automatic_dirty = true;
            if self.workers.automatic_rescan {
                self.workers.automatic_rescan = false;
                // Started by time, not by a command: origin `null` (guarantee 5).
                if let Err(error) = self.start_scan(None) {
                    self.note(&error);
                }
            }
        }
        if !self.confirmations.held.is_empty() {
            self.expire_confirmations();
        }
        self.expire_grants();
        if self.playback.state == PlayState::Playing
            && let Some(position) = self.position()
        {
            self.emit(Event::Position { position });
        }
        self.resync_lagging();
        if (self.persistence.dirty && self.persistence.last_save.elapsed() >= SAVE_EVERY)
            || (self.playback.state == PlayState::Playing
                && self.engine.is_active()
                && self.persistence.last_save.elapsed() >= SAVE_POSITION_EVERY)
        {
            self.save_now();
        }
        self.automatic_ready() || self.publication.seq != before
    }

    /// Audio of `instance` actually played out so far (play counts use time
    /// heard, not where playback ended). Read when the instance ends; audio still
    /// fading out after a skip adds a few ms that are not counted.
    pub(super) fn heard_ms(&self, instance: u64) -> u64 {
        self.engine
            .heard(mpmp_engine::EntryId(instance))
            .map_or(0, |h| h.as_duration().as_millis() as u64)
    }
}
