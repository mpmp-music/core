use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RebuildResult {
    Ready,
    Disconnected,
    Failed,
    Stopped,
}

impl Actor {
    pub(super) fn output_failed(&mut self, error: &EngineError) {
        if self.output.output_error.is_none() {
            self.output.output_policy.prepare_output();
        }
        let error = engine_error(error).retryable();
        self.output.output_failure_serial += 1;
        self.output.output_error = Some(error.clone());
        if let Some(playing) = &mut self.playback.playing {
            playing.signal_path = None;
        }
        if let Some(problem) = self
            .problems
            .iter_mut()
            .find(|p| p.code == ErrorCode::OutputFailed)
        {
            problem.message = error.message;
        } else {
            self.add_problem(ErrorCode::OutputFailed, error.message);
        }
    }

    pub(super) fn output_recovered(&mut self) {
        self.output.output_error = None;
        self.output.output_selection_failure = None;
        self.problems.retain(|p| p.code != ErrorCode::OutputFailed);
    }

    pub(super) fn authorize_playback(&mut self) {
        self.output.output_policy.cancel();
        self.output.output_policy.prepare_output();
        // Only a new user operation starts playback; failed opens return to Paused.
        self.playback.state = PlayState::Playing;
    }

    /// New streams stay silent unless transport is Playing and no disconnect is latched.
    pub(super) fn apply_output_transport(&mut self) -> bool {
        let disconnected = self.engine.take_output_disconnect();
        if disconnected {
            self.playback.state = PlayState::Paused;
            self.output.output_policy.cancel();
        }
        if self.playback.state == PlayState::Playing {
            self.engine.resume();
        } else {
            self.playback.state = PlayState::Paused;
            self.engine.pause();
        }
        disconnected
    }

    pub(super) fn on_output(&mut self) {
        self.process_playback_limits();
        if let Some(facts) = self.engine.take_output_facts() {
            if facts.generation != self.engine.output_generation() {
                return;
            }
            let fixed = matches!(
                self.output.output_settings.selection,
                crate::contract::OutputDeviceSelection::Device { .. }
            );
            let hard_loss = facts.route.headphones_disconnected
                || facts.contains(mpmp_engine::output::OutputEvent::DeviceUnplugged);
            let unavailable = facts.contains(mpmp_engine::output::OutputEvent::DeviceUnavailable);
            if hard_loss
                || unavailable
                || (fixed && facts.contains(mpmp_engine::output::OutputEvent::StreamInvalidated))
            {
                if self.playback.playing.is_some()
                    && (self.engine.is_active() || self.output.output_error.is_none())
                {
                    if hard_loss {
                        self.output.output_policy.cancel();
                    }
                    self.output_action(policy::Action::PauseClose);
                    if fixed {
                        self.output_failed(&EngineError::Output(
                            mpmp_engine::output::OutputError::NoDevice,
                        ));
                        self.output_unavailable();
                        self.emit_state();
                    }
                }
                if facts.route.format_changed {
                    self.output_format_changed();
                }
                let _ = self.refresh_output_devices();
                if !fixed && unavailable && !hard_loss && self.playback.playing.is_some() {
                    // Preserve default-output silent recovery, after closing the
                    // lost stream and without waiting for discovery.
                    self.output_action(policy::Action::PauseRebuild);
                }
                return;
            }
            if facts.route.devices_changed
                || facts.route.route.is_some()
                || facts.route.default_changed
                || facts.route.format_changed
            {
                let _ = self.refresh_output_devices();
            }
            if facts.route.identity_changed {
                self.emit_state();
            }
            if facts.route.format_changed {
                self.output_format_changed();
            }
            if self.playback.playing.is_none() {
                if self.output.output_selection_failure.is_some()
                    && self.engine.default_output_format().is_ok()
                {
                    self.output.output_selection_failure = None;
                    self.emit_state();
                }
                return;
            }
            if facts.contains(mpmp_engine::output::OutputEvent::Xrun) {
                tracing::debug!(generation = facts.generation, "output xrun");
            }
            if facts.contains(mpmp_engine::output::OutputEvent::ClockUnreliable) {
                tracing::warn!(
                    generation = facts.generation,
                    "output clock unreliable; checking format"
                );
            }
            let mut facts = facts;
            if matches!(
                self.output.output_settings.selection,
                crate::contract::OutputDeviceSelection::Device { .. }
            ) {
                if facts.route.default_changed && !facts.route.format_changed {
                    // A fixed endpoint does not follow unrelated default changes.
                    return;
                }
                facts.route.default_changed = false;
                facts.route.rebuild_required = false;
            }
            let notified = facts.route.route.is_some() || facts.route.default_changed;
            if notified {
                self.output.output_policy.external_change();
            } else if facts.route.format_changed {
                self.output.output_policy.format_change();
            }
            let hard_loss = facts.route.headphones_disconnected
                || facts.contains(mpmp_engine::output::OutputEvent::DeviceUnplugged);
            let played = self.engine.buffer_status().map_or(0, |s| s.played_frames);
            if let Some(action) = self.output.output_policy.facts(
                facts,
                self.now_ms(),
                self.engine.callback_count(),
                played,
            ) {
                self.output_action(action);
                if action == policy::Action::PauseClose
                    && !hard_loss
                    && notified
                    && self.output.output_at.is_some()
                {
                    self.output_action(policy::Action::Rebuild);
                }
            } else if notified && self.output.output_at.is_some() {
                self.output_action(policy::Action::Rebuild);
            }
        }
    }

    fn output_format_changed(&mut self) {
        self.output.output_devices_generation += 1;
        self.emit(Event::OutputDevicesChanged {
            generation: self.output.output_devices_generation,
        });
        self.emit_output_as(self.publication.cause.clone());
    }

    pub(super) fn poll_output(&mut self) {
        self.on_output();
        self.poll_output_devices();
        let played = self.engine.buffer_status().map_or(0, |s| s.played_frames);
        if let Some(action) = self.output.output_policy.poll(
            self.now_ms(),
            self.engine.output_generation(),
            self.engine.callback_count(),
            played,
            self.playback.state != PlayState::Playing,
        ) {
            self.output_action(action);
        }
    }

    pub(super) fn remember_pause_anchor(&self) {
        if self.playback.state == PlayState::Paused
            && let Some((instance, position)) = self
                .playback
                .playing
                .as_ref()
                .map(|p| p.instance)
                .zip(self.position())
        {
            self.playback.seek_hint.set(Some(SeekHint {
                instance,
                position_ms: position.position_ms,
                at_ms: self.now_ms(),
                running: false,
            }));
        }
    }

    pub(super) fn close_output(&mut self) {
        self.close_output_with_pause_anchor(true);
    }

    fn close_output_with_pause_anchor(&mut self, preserve: bool) {
        let transport = self.playback.state;
        if preserve {
            self.remember_pause_anchor();
        }
        let frozen = self.engine.freeze_output();
        self.reduce_frozen(frozen, transport);
    }

    pub(super) fn freeze_for_shutdown(&mut self) -> Option<std::thread::JoinHandle<()>> {
        self.remember_pause_anchor();
        if self.playback.state == PlayState::Playing {
            self.playback.state = PlayState::Paused;
        }
        let (frozen, thread) = self.engine.freeze_output_detached();
        self.engine_events = self.engine.events();
        self.reduce_frozen_inner(frozen, self.playback.state, false);
        self.rebase_hint(false);
        thread
    }

    pub(super) fn reduce_frozen(
        &mut self,
        frozen: mpmp_engine::FrozenOutput,
        transport: PlayState,
    ) {
        self.reduce_frozen_inner(frozen, transport, true);
    }

    pub(super) fn reduce_frozen_inner(
        &mut self,
        frozen: mpmp_engine::FrozenOutput,
        transport: PlayState,
        recover_successor: bool,
    ) {
        let paused_anchor = (transport == PlayState::Paused
            && self
                .playback
                .seek_hint
                .get()
                .is_some_and(|hint| !hint.running))
        .then(|| {
            self.playback
                .playing
                .as_ref()
                .map(|p| p.instance)
                .zip(self.position())
        })
        .flatten();
        let was_limited = self.limits.stop_after_current.is_some();
        let mut recovery_from = None;
        let mut defer_recovery = false;
        let mut ended_entries = Vec::new();
        for event in frozen.events {
            let deferred_start = self
                .playback
                .playing
                .as_ref()
                .is_some_and(|p| self.playback.instances[&p.instance].deferred_start);
            if let EngineEvent::TrackFailed { entry, .. } = &event
                && let Some(instance) = self.playback.instances.get(&entry.0)
            {
                ended_entries.push(instance.entry);
            }
            let completed = if matches!(&event, EngineEvent::EndOfQueue { entry }
                if self.playback.playing.as_ref().is_some_and(|p| p.instance == entry.0))
            {
                self.playback
                    .playing
                    .as_ref()
                    .map(|p| self.playback.instances[&p.instance].entry)
            } else {
                None
            };
            if let Some(entry) = completed {
                ended_entries.push(entry);
            }
            if let Some(entry) = self.reduce_engine(event, false).or(completed) {
                recovery_from = Some(entry);
                defer_recovery = deferred_start;
            }
            if self.playback.playing.is_some() {
                // An actually started successor supersedes an earlier failure.
                recovery_from = None;
            }
        }
        if recover_successor
            && !was_limited
            && self.playback.playing.is_none()
            && let Some(ended) = recovery_from
        {
            let mut from = ended;
            let mut next = None;
            for _ in 0..MAX_CONSECUTIVE_FAILURES {
                let Some(candidate) = self
                    .playback
                    .queue
                    .next_after(Some(from))
                    .filter(|&e| e != from)
                else {
                    break;
                };
                if !ended_entries.contains(&candidate) {
                    next = Some(candidate);
                    break;
                }
                from = candidate;
            }
            if let Some(next) = next {
                self.play_or_skip_with_budget(
                    next,
                    0,
                    &Origin::System,
                    false,
                    MAX_CONSECUTIVE_FAILURES,
                    defer_recovery,
                );
            } else {
                self.stop_engine();
            }
            if self.playback.playing.is_some() {
                self.playback.state = transport;
            }
        }
        if self.output.output_at.is_none()
            && let Some(p) = &self.playback.playing
        {
            let instance = p.instance;
            let position = frozen.position.filter(|p| p.entry.0 == instance);
            let rate = position
                .map(|p| p.sample_rate)
                .or(self.playback.instances[&instance].track.sample_rate)
                .unwrap_or(44_100);
            let frame = frozen
                .pending_seek
                .filter(|(entry, _)| entry.0 == instance)
                .map(|(_, frame)| frame)
                .or(position.map(|p| p.frame))
                .or(self
                    .output
                    .unrendered_resume
                    .filter(|(id, _)| *id == instance)
                    .map(|(_, frame)| frame))
                .unwrap_or_else(|| {
                    self.playback
                        .seek_hint
                        .get()
                        .filter(|h| h.instance == instance)
                        .map_or(0, |h| h.position_ms.saturating_mul(u64::from(rate)) / 1000)
                });
            self.output.output_at = Some((instance, frame));
        }
        if let Some((instance, frame)) = self.output.output_at {
            let rate = self.playback.instances[&instance]
                .track
                .sample_rate
                .unwrap_or(44_100);
            self.playback.seek_hint.set(Some(SeekHint {
                instance,
                position_ms: frame * 1000 / u64::from(rate),
                at_ms: self.now_ms(),
                running: false,
            }));
        }
        if let Some((instance, position)) = paused_anchor
            && self
                .playback
                .playing
                .as_ref()
                .is_some_and(|p| p.instance == instance)
        {
            self.playback.seek_hint.set(Some(SeekHint {
                instance,
                position_ms: position.position_ms,
                at_ms: self.now_ms(),
                running: false,
            }));
        }
        self.engine.pause();
        self.playback.pending_next = None;
        if let Some(p) = &self.playback.playing {
            self.forget_old_instances(p.instance);
        }
        self.announce_queue();
    }

    pub(super) fn rebuild_output(&mut self) -> RebuildResult {
        self.output.output_policy.rebuild_required();
        let paused_position = (self.playback.state == PlayState::Paused)
            .then(|| {
                self.position()
                    .zip(self.playback.playing.as_ref().map(|p| p.instance))
            })
            .flatten();
        if self.playback.playing.is_none() {
            return RebuildResult::Failed;
        }
        if self.output.output_at.is_none() {
            self.close_output();
        }
        let mut failures = 0;
        while failures < MAX_CONSECUTIVE_FAILURES {
            let Some((instance, frame)) = self.output.output_at else {
                return RebuildResult::Stopped;
            };
            let Some(i) = self.playback.instances.get(&instance) else {
                return RebuildResult::Stopped;
            };
            let deferred_start = i.deferred_start;
            let mut track = i.output_track.clone();
            track.gain = i.gain.map_or(1.0, TrackGain::linear);
            // New streams remain silent until all pre-retirement disconnect evidence is
            // applied. The persistent device latch also covers notifications during open.
            self.engine.pause();
            match self.engine.rebuild_output(track, frame) {
                Ok(path) => {
                    let disconnected = self.apply_output_transport();
                    self.output_recovered();
                    if let Some(playing) = &mut self.playback.playing {
                        playing.signal_path = Some(convert_signal_path(&path));
                    }
                    self.output.output_at = None;
                    self.output.unrendered_resume = Some((instance, frame));
                    self.output.output_policy.recovered();
                    self.engine.set_volume(self.master_volume());
                    if let Some((position, instance)) = paused_position
                        && self
                            .playback
                            .playing
                            .as_ref()
                            .is_some_and(|p| p.instance == instance)
                    {
                        // Keep the paused display anchor across a format-only rebuild.
                        // Resume still uses the final rendered boundary, avoiding replay
                        // and duplicate history for the device-queued tail.
                        self.playback.seek_hint.set(Some(SeekHint {
                            instance,
                            position_ms: position.position_ms,
                            at_ms: self.now_ms(),
                            running: false,
                        }));
                    }
                    self.rebase_hint(self.playback.state == PlayState::Playing);
                    self.playback.pending_next = None;
                    self.refresh_next();
                    self.emit_state();
                    self.emit_position();
                    return if disconnected {
                        RebuildResult::Disconnected
                    } else {
                        RebuildResult::Ready
                    };
                }
                Err(error) if !matches!(error, EngineError::Output(_)) => {
                    failures += 1;
                    let error = engine_error(&error);
                    let failed = self.fail_current(error.code, error.message, false);
                    let next = failed
                        .filter(|_| self.playback.state != PlayState::Stopped)
                        .and_then(|entry| {
                            self.playback
                                .queue
                                .next_after(Some(entry))
                                .filter(|&e| e != entry)
                        });
                    let Some(next) = next.filter(|_| failures < MAX_CONSECUTIVE_FAILURES) else {
                        self.stop_engine();
                        return RebuildResult::Stopped;
                    };
                    let transport = self.playback.state;
                    failures += self.play_or_skip_with_budget(
                        next,
                        0,
                        &Origin::System,
                        false,
                        MAX_CONSECUTIVE_FAILURES - failures,
                        deferred_start,
                    );
                    if self.playback.playing.is_none() {
                        return RebuildResult::Stopped;
                    }
                    self.playback.state = transport;
                    self.announce_queue();
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot rebuild output");
                    self.engine.pause();
                    // Retain transport intent across transient output failures. Safety
                    // actions and user Pause already changed it before this attempt.
                    if self.engine.take_output_disconnect() {
                        self.playback.state = PlayState::Paused;
                        self.output.output_policy.cancel();
                    }
                    self.output_failed(&error);
                    self.rebase_hint(false);
                    self.emit_state();
                    return RebuildResult::Failed;
                }
            }
        }
        self.stop_engine();
        RebuildResult::Stopped
    }

    pub(super) fn output_unavailable(&mut self) {
        // This runs only after a failed attempt: the last fast success can still
        // continue an explicit switch, but waiting for slow preparation cannot.
        if self.output.output_policy.unavailable(self.now_ms()) {
            self.pause();
        }
    }

    pub(super) fn output_action(&mut self, action: policy::Action) {
        use policy::Action;
        match action {
            Action::PauseClose => {
                let already_paused = self.playback.state == PlayState::Paused;
                self.playback.state = PlayState::Paused;
                self.close_output_with_pause_anchor(already_paused);
                if self.playback.playing.is_some() {
                    self.playback.state = PlayState::Paused;
                }
                self.rebase_hint(false);
                self.persistence.dirty = true;
                self.emit_state();
                self.emit_position();
            }
            Action::PauseRebuild | Action::PauseCheckFormat => {
                self.pause();
                if action == Action::PauseRebuild {
                    self.output_action(Action::Follow);
                } else {
                    self.output_action(Action::CheckFormat);
                }
            }
            Action::CheckFormat => {
                if !self
                    .output
                    .output_policy
                    .begin_attempt(self.now_ms(), Action::CheckFormat)
                {
                    return;
                }
                match self.engine.default_output_format() {
                    Ok(format) if Some(format) == self.engine.output_format() => {
                        self.output.output_policy.recovered();
                    }
                    Ok(_) => {
                        if self.rebuild_output() == RebuildResult::Failed {
                            self.output_unavailable();
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            generation = self.engine.output_generation(),
                            retry_at = ?self.output.output_policy.next_due(),
                            "cannot query default output format; retaining current stream"
                        );
                    }
                }
            }
            Action::Follow | Action::Rebuild => {
                if !self
                    .output
                    .output_policy
                    .begin_attempt(self.now_ms(), Action::Rebuild)
                {
                    return;
                }
                if self.rebuild_output() == RebuildResult::Failed {
                    self.output_unavailable();
                }
            }
        }
    }
}
