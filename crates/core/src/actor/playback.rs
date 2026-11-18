use super::*;
use crate::gain;

/// One playback of a queue entry. The engine sees instances, not entries, so that the
/// same entry played twice (repeat one, previous) is never ambiguous.
#[derive(Debug, Clone)]
pub(super) struct Instance {
    pub(super) entry: EntryId,
    pub(super) track: TrackInfo,
    /// ReplayGain the engine applies to it; `None` = untouched.
    pub(super) gain: Option<TrackGain>,
    pub(super) output_track: TrackRef,
    pub(super) start_announced: bool,
    /// Selected silently; it is not a history play until content is heard.
    pub(super) deferred_start: bool,
    pub(super) ended: Option<EndReason>,
}

pub(super) struct Playing {
    pub(super) instance: u64,
    pub(super) signal_path: Option<SignalPathInfo>,
    pub(super) started_at_ms: i64,
}

impl Playing {
    pub(super) fn new(instance: u64) -> Self {
        Self {
            instance,
            signal_path: None,
            started_at_ms: unix_ms(),
        }
    }
}

/// Where a seek went, until the engine reports positions from there (it may keep
/// reporting the old position for a while, e.g. all along a pause).
#[derive(Debug, Clone, Copy)]
pub(super) struct SeekHint {
    pub(super) instance: u64,
    pub(super) position_ms: u64,
    pub(super) at_ms: u64,
    pub(super) running: bool,
}

impl SeekHint {
    pub(super) fn at(&self, now_ms: u64) -> u64 {
        if self.running {
            self.position_ms
                .saturating_add(now_ms.saturating_sub(self.at_ms))
        } else {
            self.position_ms
        }
    }
}

/// Dropped after the decoder source, including failed/cancelled asynchronous opens.
pub(super) struct PlaybackFile {
    pub(super) uri: mpmp_media_io::MediaUri,
    pub(super) open: Arc<Mutex<Vec<mpmp_media_io::MediaUri>>>,
    pub(super) _root: Box<dyn Send>,
}

impl Drop for PlaybackFile {
    fn drop(&mut self) {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(at) = open.iter().position(|uri| uri == &self.uri) {
            open.swap_remove(at);
        }
    }
}

pub(super) fn entry_not_found(entry: EntryId) -> ContractError {
    ContractError::not_found(ErrorCode::EntryNotFound, format!("queue entry {}", entry.0))
        .with_missing(vec![ObjectRef::Entry { id: entry }])
}

impl Actor {
    /// The tracks of a play context; a `Tracks` context's ids must all exist.
    pub(super) fn context_tracks(
        &self,
        context: &PlayContext,
    ) -> Result<Vec<TrackId>, ContractError> {
        match context {
            PlayContext::Tracks { tracks } => {
                self.check_tracks_exist(tracks, "command.context.tracks")?;
                Ok(tracks.clone())
            }
            PlayContext::Unknown => {
                Err(ContractError::unknown_variant("play context").with_field("command.context"))
            }
            other => self.catalog.context_tracks(other),
        }
    }

    /// Announces the tracks a recorded user-data change changed: whether it changed any.
    pub(super) fn tracks_changed(
        &mut self,
        changed: Recorded<Vec<TrackId>>,
        receipt: &mut Receipt,
    ) -> bool {
        receipt.action = changed.action;
        if changed.value.is_empty() {
            return false;
        }
        self.emit(Event::TracksChanged {
            tracks: changed.value,
        });
        true
    }

    pub(super) fn check_tracks_exist(
        &self,
        tracks: &[TrackId],
        field: &str,
    ) -> Result<(), ContractError> {
        crate::catalog::check_tracks_exist(self.catalog.as_ref(), tracks, field)
    }

    /// Stage every fallible navigation step while the accepted instance and limit
    /// remain intact. Only a prepared decoder/output can cross the commit boundary.
    pub(super) fn commit_guarded_navigation(
        &mut self,
        staged: Queue,
        entry: EntryId,
        transport: PlayState,
        origin: &Origin,
    ) -> Result<(), ContractError> {
        let open = transport == PlayState::Playing || self.output.output_at.is_none();
        let fresh = open && transport == PlayState::Paused;
        let deferred_start = transport == PlayState::Paused;
        let accepted = std::mem::replace(&mut self.playback.queue, staged);
        let mut candidate = Some(entry);
        let mut failures = Vec::new();
        let mut selected = None;
        let mut output_error = None;
        while let Some(entry) = candidate {
            let result = match self.new_instance(entry) {
                Ok((id, track)) => {
                    if open {
                        match self.engine.prepare_playback(track, 0, fresh) {
                            Ok(prepared) => Ok((id, Some(prepared))),
                            Err(error) => {
                                self.playback.instances.remove(&id);
                                if matches!(error, EngineError::Output(_)) {
                                    output_error = Some(engine_error(&error));
                                    break;
                                }
                                Err(engine_error(&error))
                            }
                        }
                    } else {
                        Ok((id, None))
                    }
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(prepared) => {
                    selected = Some((entry, prepared));
                    break;
                }
                Err(error) => failures.push((entry, error)),
            }
            if failures.len() >= MAX_CONSECUTIVE_FAILURES {
                break;
            }
            candidate = self
                .playback
                .queue
                .next_after(Some(entry))
                .filter(|&n| n != entry);
        }
        let mut staged = std::mem::replace(&mut self.playback.queue, accepted);
        if let Some(error) = output_error {
            // Media failures remain facts even if a later candidate cannot open its
            // output. Publish them with their staged context without accepting it.
            let accepted = std::mem::replace(&mut self.playback.queue, staged);
            for (entry, error) in failures {
                self.record_navigation_failure(entry, error, origin, deferred_start);
            }
            self.playback.queue = accepted;
            return Err(error);
        }
        let Some((entry, (id, prepared))) = selected else {
            // Exhausting media candidates is the existing successful navigation to
            // stopped transport, not a command error. Output preparation errors above
            // remain transactional and preserve the accepted instance and boundary.
            self.limits.stop_after_current = None;
            self.remember_pause_anchor();
            let frozen = self.engine.freeze_output();
            self.reduce_frozen_inner(frozen, self.playback.state, false);
            self.finish_playing(EndReason::Skipped, origin);
            self.playback.queue = staged;
            for (entry, error) in failures {
                self.record_navigation_failure(entry, error, origin, deferred_start);
            }
            self.stop_engine();
            return Ok(());
        };
        staged.set_current(Some(entry));
        let selected = self.playback.instances[&id].clone();
        // From here navigation is committed: no fallible opens or catalog calls remain.
        self.limits.stop_after_current = None;
        if fresh {
            self.remember_pause_anchor();
            let frozen = self.engine.freeze_output();
            self.reduce_frozen_inner(frozen, self.playback.state, false);
        } else {
            // Settle already-published facts against the old instance. Instance-scoped
            // EOF also rejects facts published after this drain and after the commit.
            while let Ok(event) = self.engine_events.try_recv() {
                self.reduce_engine(event, false);
            }
        }
        self.finish_playing(EndReason::Skipped, origin);
        self.playback.queue = staged;
        for (entry, error) in failures {
            self.record_navigation_failure(entry, error, origin, deferred_start);
        }
        self.playback.instances.insert(id, selected);
        self.playback.instances.get_mut(&id).unwrap().deferred_start =
            transport == PlayState::Paused;
        self.playback.playing = Some(Playing::new(id));
        self.playback.pending_next = None;
        self.playback.failed_next.clear();
        self.output.output_policy.cancel();
        self.playback.state = transport;
        self.playback.resume_at = None;
        self.output.output_at = None;
        self.playback
            .seek_hint
            .set((transport == PlayState::Paused).then_some(SeekHint {
                instance: id,
                position_ms: 0,
                at_ms: self.now_ms(),
                running: false,
            }));
        if let Some(prepared) = prepared {
            self.engine.commit_playback(prepared);
            self.apply_output_transport();
            self.output_recovered();
            self.output.output_policy.recovered();
        } else {
            self.output.output_at = Some((id, 0));
        }
        self.persistence.dirty = true;
        self.refresh_next();
        self.emit_state();
        if self.playback.state == PlayState::Paused {
            self.save_now();
            self.emit_position();
        }
        Ok(())
    }

    /// A transport skip preserves pause or stop, including a closed output.
    pub(super) fn skip_to(&mut self, entry: EntryId, origin: &Origin) {
        if self.playback.state == PlayState::Stopped {
            self.retire_session();
            // Selection only: do not resolve the URI, prepare an instance, or open output.
            // Resume owns file failures and starts only the last selected entry.
            self.playback.queue.set_current(Some(entry));
            self.playback.resume_at = Some((entry, 0));
            self.persistence.dirty = true;
            self.save_now();
            self.emit_state();
            self.emit_position();
            return;
        }
        if self.playback.state != PlayState::Paused {
            self.finish_playing(EndReason::Skipped, origin);
            self.play_or_skip(entry, 0, origin);
            return;
        }
        let open = self.output.output_at.is_none();
        // Commit any real old-track tail before retiring its instances. No prefetched
        // or late start from that session may take over the newly selected entry.
        self.remember_pause_anchor();
        let frozen = self.engine.freeze_output();
        self.reduce_frozen_inner(frozen, PlayState::Paused, false);
        self.finish_playing(EndReason::Skipped, origin);
        self.playback.instances.clear();
        self.playback.pending_next = None;
        self.playback.failed_next.clear();
        self.output.output_policy.cancel();
        self.playback.state = PlayState::Paused;
        self.play_or_skip_with_budget(entry, 0, origin, open, MAX_CONSECUTIVE_FAILURES, true);
        self.save_now();
    }

    /// Plays `entry`; if it cannot be opened, reports it and moves on along the queue, up
    /// to a few failures in a row, then stops. File problems are never a command
    /// error: the queue change the command made stands either way.
    pub(super) fn play_or_skip(&mut self, entry: EntryId, position_ms: u64, origin: &Origin) {
        self.authorize_playback();
        self.play_or_skip_with_output(entry, position_ms, origin, true);
    }

    fn play_or_skip_with_output(
        &mut self,
        entry: EntryId,
        position_ms: u64,
        origin: &Origin,
        open: bool,
    ) {
        self.play_or_skip_with_budget(
            entry,
            position_ms,
            origin,
            open,
            MAX_CONSECUTIVE_FAILURES,
            false,
        );
    }

    pub(super) fn play_or_skip_with_budget(
        &mut self,
        entry: EntryId,
        position_ms: u64,
        origin: &Origin,
        open: bool,
        budget: usize,
        deferred_start: bool,
    ) -> usize {
        let mut candidate = Some(entry);
        let mut position = position_ms;
        let mut failures = 0;
        while let Some(entry) = candidate {
            let result = self.prepare_entry(entry, position, open, deferred_start);
            let error = match result {
                Ok(()) => return failures,
                Err(err) => err,
            };
            failures += 1;
            self.record_navigation_failure(entry, error, origin, deferred_start);
            if failures >= budget {
                break;
            }
            candidate = self
                .playback
                .queue
                .next_after(Some(entry))
                .filter(|&n| n != entry);
            position = 0;
        }
        if self.engine.is_active() {
            // Failed opens may leave the previous stream (and an audible successor)
            // running. Commit its rendered boundary before retiring the instances.
            self.finish_transport(EndReason::Skipped, false, origin);
        } else {
            self.stop_engine();
        }
        failures
    }

    /// Both navigation paths publish identical media failure facts at commit.
    fn record_navigation_failure(
        &mut self,
        entry: EntryId,
        error: ContractError,
        origin: &Origin,
        deferred_start: bool,
    ) {
        if !deferred_start && let Some(track) = self.playback.queue.entry(entry).map(|e| e.track) {
            self.record(PlayRecord {
                track,
                context: self.playback.queue.context().cloned(),
                started_at_ms: unix_ms(),
                played_ms: 0,
                ended: EndReason::Failed,
                origin: Some(origin.clone()),
            });
        }
        tracing::warn!("cannot play queue entry {}: {error}", entry.0);
        self.emit(Event::TrackFailed {
            entry,
            code: error.code,
            message: error.message,
            upcoming: false,
        });
    }

    fn new_instance(&mut self, entry: EntryId) -> Result<(u64, TrackRef), ContractError> {
        let track_id = self
            .playback
            .queue
            .entry(entry)
            .ok_or_else(|| entry_not_found(entry))?
            .track;
        let track = self.catalog.tracks(&[track_id])?.pop().ok_or_else(|| {
            ContractError::not_found(
                ErrorCode::TrackNotFound,
                format!("track {} is not in the library", track_id.0),
            )
        })?;
        if !track.available {
            // Its folder was removed from the library, or the file was missing at the last
            // scan: the library no longer vouches for it.
            return Err(ContractError::failed(
                ErrorCode::TrackUnavailable,
                format!("track {} is unavailable", track_id.0),
            ));
        }
        let uri = self.catalog.uri_of(track_id)?.ok_or_else(|| {
            ContractError::failed(
                ErrorCode::FileMissing,
                format!("track {} has no file", track_id.0),
            )
        })?;
        if let Some(path) = uri.to_file_path()
            && !path.exists()
        {
            return Err(ContractError::failed(
                ErrorCode::FileMissing,
                format!("{} is missing", path.display()),
            ));
        }
        // A track of a CUE sheet: its part of the file. Consecutive tracks of one file
        // play gaplessly like separate files do (the next one is opened and seeked
        // sample-accurately while the current one plays).
        let range = self.catalog.frame_range(track_id)?;
        let gain = gain::track_gain(
            track.replay_gain.as_ref(),
            &self.audio.replay_gain,
            self.album_context(entry, &track),
        );
        self.playback.next_instance += 1;
        let instance = self.playback.next_instance;
        let mut track_ref = TrackRef::new(mpmp_engine::EntryId(instance), uri)
            .with_gain(gain.map_or(1.0, TrackGain::linear));
        track_ref.range = range;
        self.playback.instances.insert(
            instance,
            Instance {
                entry,
                track,
                gain,
                output_track: track_ref.clone(),
                start_announced: false,
                deferred_start: false,
                ended: None,
            },
        );
        Ok((instance, track_ref))
    }

    /// Whether smart ReplayGain uses album gain for `entry`: the queue plays in order and
    /// is an album, or a neighbour in the queue is from the same album.
    pub(super) fn album_context(&self, entry: EntryId, track: &TrackInfo) -> bool {
        if self.audio.replay_gain.mode != ReplayGainMode::Smart || self.playback.queue.shuffle() {
            return false;
        }
        if matches!(
            self.playback.queue.context(),
            Some(PlayContext::Album { .. })
        ) {
            return true;
        }
        let key = &track.album_key;
        let Some(i) = self.playback.queue.index_of(entry) else {
            return false;
        };
        if key.is_empty() {
            return false;
        }
        let entries = self.playback.queue.entries();
        let neighbours: Vec<TrackId> = [i.checked_sub(1), Some(i + 1)]
            .into_iter()
            .flatten()
            .filter_map(|j| entries.get(j).map(|e| e.track))
            .collect();
        self.catalog
            .tracks(&neighbours)
            .is_ok_and(|ts| ts.iter().any(|t| &t.album_key == key))
    }

    #[cfg(all(test, feature = "desktop-fs"))]
    pub(super) fn play_entry(
        &mut self,
        entry: EntryId,
        position_ms: u64,
    ) -> Result<(), ContractError> {
        self.authorize_playback();
        self.play_entry_with_output(entry, position_ms, true)
    }

    #[cfg(all(test, feature = "desktop-fs"))]
    pub(super) fn play_entry_with_output(
        &mut self,
        entry: EntryId,
        position_ms: u64,
        open: bool,
    ) -> Result<(), ContractError> {
        self.prepare_entry(entry, position_ms, open, false)
    }

    fn prepare_entry(
        &mut self,
        entry: EntryId,
        position_ms: u64,
        open: bool,
        deferred_start: bool,
    ) -> Result<(), ContractError> {
        self.output.output_at = None;
        let (instance, track_ref) = self.new_instance(entry)?;
        self.playback
            .instances
            .get_mut(&instance)
            .unwrap()
            .deferred_start = deferred_start;
        let rate = self.playback.instances[&instance]
            .track
            .sample_rate
            .unwrap_or(44_100);
        let frame = position_ms.saturating_mul(u64::from(rate)) / 1000;
        if !open {
            self.playback.queue.set_current(Some(entry));
            self.playback.playing = Some(Playing::new(instance));
            self.output.output_at = Some((instance, frame));
            self.playback.seek_hint.set(Some(SeekHint {
                instance,
                position_ms,
                at_ms: self.now_ms(),
                running: false,
            }));
            self.persistence.dirty = true;
            self.playback.pending_next = None;
            self.playback.failed_next.clear();
            self.playback.resume_at = None;
            self.emit_state();
            if self.playback.state == PlayState::Paused {
                self.emit_position();
            }
            return Ok(());
        }
        // A paused automatic successor needs a fresh silent renderer: the previous
        // session may still have an unfinished pause fade queued for the old track.
        self.engine.pause();
        if self.playback.state != PlayState::Playing {
            self.engine.stop();
        }
        if let Err(error) = self.engine.play_from(track_ref, frame) {
            if !matches!(error, EngineError::Output(_)) {
                return Err(engine_error(&error));
            }
            tracing::warn!(%error, "output unavailable; retaining current entry");
            self.playback.queue.set_current(Some(entry));
            self.playback.playing = Some(Playing::new(instance));
            self.engine.pause();
            self.playback.state = PlayState::Paused;
            self.output.output_at = Some((instance, frame));
            self.playback.seek_hint.set(Some(SeekHint {
                instance,
                position_ms,
                at_ms: self.now_ms(),
                running: false,
            }));
            if self.engine.take_output_disconnect() {
                self.playback.state = PlayState::Paused;
                self.output.output_policy.cancel();
            }
            self.output_failed(&error);
            self.output_unavailable();
            self.emit_state();
            self.emit_position();
            return Ok(());
        }
        self.output_recovered();
        self.apply_output_transport();
        self.output.output_policy.recovered();
        self.playback.queue.set_current(Some(entry));
        self.playback.playing = Some(Playing::new(instance));
        self.playback.seek_hint.set(
            (position_ms > 0 || self.playback.state == PlayState::Paused).then_some(SeekHint {
                instance,
                position_ms,
                at_ms: self.now_ms(),
                running: self.playback.state == PlayState::Playing,
            }),
        );
        self.playback.pending_next = None;
        self.playback.failed_next.clear();
        self.playback.resume_at = None;
        self.persistence.dirty = true;
        self.refresh_next();
        self.emit_state();
        if self.playback.state == PlayState::Paused {
            self.emit_position();
        }
        Ok(())
    }

    /// Tells the engine what follows the entry being heard, if that changed.
    pub(super) fn refresh_next(&mut self) {
        if self.limits.stop_after_current.is_some() {
            return;
        }
        if self.playback.playing.as_ref().is_none_or(|p| {
            let i = &self.playback.instances[&p.instance];
            self.playback.state == PlayState::Paused && i.deferred_start && !i.start_announced
        }) {
            return;
        }
        // Also when the entry being heard was removed: then what followed its place, or
        // nothing (a cleared queue must not keep playing its old next track).
        let mut next = self.playback.queue.upcoming();
        for _ in 0..MAX_CONSECUTIVE_FAILURES {
            match next {
                Some(entry) if self.playback.failed_next.contains(&entry) => {
                    next = self
                        .playback
                        .queue
                        .next_after(Some(entry))
                        .filter(|&e| e != entry);
                }
                _ => break,
            }
        }
        if next.is_some_and(|e| self.playback.failed_next.contains(&e)) {
            next = None;
        }
        let pending = self
            .playback
            .pending_next
            .and_then(|i| self.playback.instances.get(&i))
            .map(|i| i.entry);
        if next == pending {
            return;
        }
        match next {
            Some(entry) => match self.new_instance(entry) {
                Ok((instance, track_ref)) => {
                    self.engine.set_next(Some(track_ref));
                    self.playback.pending_next = Some(instance);
                }
                Err(err) => {
                    self.engine.set_next(None);
                    self.playback.pending_next = None;
                    self.next_failed(entry, err.code, err.message);
                    if self.playback.failed_next.len() < MAX_CONSECUTIVE_FAILURES {
                        self.refresh_next();
                    }
                }
            },
            None => {
                self.engine.set_next(None);
                self.playback.pending_next = None;
            }
        }
    }

    /// The entry meant to come next cannot be opened: report it now (once) as upcoming,
    /// record it, and let `refresh_next` skip it.
    pub(super) fn next_failed(&mut self, entry: EntryId, code: ErrorCode, message: String) {
        if self.playback.failed_next.contains(&entry) {
            return;
        }
        tracing::warn!("cannot play the next queue entry {}: {message}", entry.0);
        self.playback.failed_next.push(entry);
        if let Some(track) = self.playback.queue.entry(entry).map(|e| e.track) {
            self.record(PlayRecord {
                track,
                context: self.playback.queue.context().cloned(),
                started_at_ms: unix_ms(),
                played_ms: 0,
                ended: EndReason::Failed,
                origin: self.publication.cause.clone(),
            });
        }
        self.emit(Event::TrackFailed {
            entry,
            code,
            message,
            upcoming: true,
        });
    }

    /// The entry a seek applies to: what is heard, else the queue's current entry.
    pub(super) fn current_entry(&self) -> Option<EntryId> {
        match &self.playback.playing {
            Some(p) => self.playback.instances.get(&p.instance).map(|i| i.entry),
            None => self.playback.queue.current(),
        }
    }

    pub(super) fn current_track(&self) -> Option<&TrackInfo> {
        match &self.playback.playing {
            Some(p) => self.playback.instances.get(&p.instance).map(|i| &i.track),
            None => self
                .idle_track
                .as_ref()
                .filter(|(e, _)| Some(*e) == self.playback.queue.current())
                .map(|(_, t)| t),
        }
    }

    /// Seeks and announces the new position at once (contract guarantee 10).
    pub(super) fn seek_to(
        &mut self,
        entry: EntryId,
        position_ms: u64,
    ) -> Result<(), ContractError> {
        self.seek_entry(entry, position_ms)?;
        if self.limits.stop_after_current.is_some()
            && self.current_track().and_then(|t| t.duration_ms) == Some(position_ms)
        {
            self.reach_stop_limit(PlaybackLimitReason::EndOfCurrent, EndReason::Completed);
        }
        self.emit_position();
        Ok(())
    }

    pub(super) fn seek_entry(
        &mut self,
        entry: EntryId,
        position_ms: u64,
    ) -> Result<(), ContractError> {
        if self.playback.state == PlayState::Paused
            && self.playback.playing.as_ref().is_some_and(|p| {
                let i = &self.playback.instances[&p.instance];
                i.deferred_start && !i.start_announced
            })
            && self.output.output_at.is_none()
        {
            // An ordinary engine seek suppresses TrackStarted. This selection has
            // never started: prepare it at the seek target on Resume instead.
            self.close_output();
        }
        // Freezing may commit a decode failure and prepare another entry. The seek
        // stays bound to its original target, including when `Seek.entry` was omitted.
        let current = self.current_entry();
        if current != Some(entry) {
            let mut error = ContractError::new(
                ErrorKind::Conflict,
                ErrorCode::EntryNotCurrent,
                format!(
                    "entry {} is no longer current after preparing the seek",
                    entry.0
                ),
            )
            .with_field("command.entry");
            error.details.expected = Some(entry.0);
            error.details.actual = current.map(|entry| entry.0);
            return Err(error);
        }
        if let Some(duration) = self.current_track().and_then(|t| t.duration_ms)
            && position_ms > duration
        {
            return Err(ContractError::invalid(
                ErrorCode::OutOfRange,
                format!("position {position_ms} ms is past the track's end ({duration} ms)"),
            )
            .with_field("command.position_ms"));
        }
        let Some(playing) = &self.playback.playing else {
            // Stopped: remember where to start.
            self.playback.resume_at = Some((entry, position_ms));
            self.persistence.dirty = true;
            return Ok(());
        };
        let instance_id = playing.instance;
        let instance = self
            .playback
            .instances
            .get(&instance_id)
            .ok_or_else(|| entry_not_found(entry))?;
        let rate = self
            .engine
            .position()
            .map(|p| p.sample_rate)
            .or(instance.track.sample_rate)
            .unwrap_or(44_100);
        if self.output.output_at.is_some() {
            self.output.output_at = Some((
                instance_id,
                position_ms.saturating_mul(u64::from(rate)) / 1000,
            ));
        }
        self.engine.seek(
            mpmp_engine::EntryId(instance_id),
            position_ms.saturating_mul(u64::from(rate)) / 1000,
        );
        self.playback.seek_hint.set(Some(SeekHint {
            instance: instance_id,
            position_ms,
            at_ms: self.now_ms(),
            running: self.playback.state == PlayState::Playing,
        }));
        self.persistence.dirty = true;
        Ok(())
    }

    /// Keeps a pending seek's position in step with pause and resume.
    pub(super) fn rebase_hint(&self, running: bool) {
        if let Some(mut h) = self.playback.seek_hint.get() {
            let now = self.now_ms();
            h.position_ms = h.at(now);
            h.at_ms = now;
            h.running = running;
            self.playback.seek_hint.set(Some(h));
        }
    }

    pub(super) fn pause(&mut self) -> bool {
        if self.playback.state != PlayState::Playing {
            return false;
        }
        self.rebase_hint(false);
        self.engine.pause();
        self.playback.state = PlayState::Paused;
        self.save_now();
        self.emit_state();
        self.emit_position();
        true
    }

    pub(super) fn resume(&mut self, origin: &Origin) -> bool {
        match self.playback.state {
            PlayState::Paused | PlayState::Playing
                if self.playback.state == PlayState::Paused || self.output.output_at.is_some() =>
            {
                if self.output.output_at.is_some() {
                    self.playback.state = PlayState::Paused;
                    self.output.output_policy.cancel();
                    self.output.output_policy.prepare_output();
                }
                if self.output.output_at.is_some() && self.rebuild_output() != RebuildResult::Ready
                {
                    self.output_unavailable();
                    return false;
                }
                self.playback.state = PlayState::Playing;
                if self.apply_output_transport() {
                    self.output_action(policy::Action::PauseClose);
                    return false;
                }
                self.rebase_hint(true);
                self.refresh_next();
                self.emit_state();
                self.emit_position();
                true
            }
            PlayState::Stopped => {
                let (entry, position) = match self.playback.resume_at {
                    Some(at) => at,
                    None => match self
                        .playback
                        .queue
                        .current()
                        .or_else(|| self.playback.queue.upcoming())
                        .or_else(|| self.playback.queue.next_after(None))
                    {
                        Some(entry) => (entry, 0),
                        None => return false,
                    },
                };
                self.play_or_skip(entry, position, origin);
                true
            }
            _ => false,
        }
    }

    pub(super) fn stop(&mut self, origin: &Origin) -> bool {
        let had_fallback = self.output.output_fallback.take().is_some();
        let limits = self.limits.stop_after_current.take().is_some()
            | self.limits.sleep_deadline.take().is_some();
        self.limits.playback_deadline.set(None);
        if limits {
            self.emit_state();
        }
        if self.playback.state == PlayState::Stopped
            && self.playback.playing.is_none()
            && !self.engine.is_active()
        {
            if had_fallback {
                self.emit_state();
            }
            return limits || had_fallback;
        }
        self.finish_transport(EndReason::Stopped, true, origin);
        true
    }

    pub(super) fn finish_transport(&mut self, ended: EndReason, restart: bool, origin: &Origin) {
        // Commit all rendered transitions before choosing the resume pair. Unlike an
        // output rebuild, Stop must not prepare an automatic successor.
        self.remember_pause_anchor();
        let frozen = self.engine.freeze_output();
        self.reduce_frozen_inner(frozen, self.playback.state, false);
        // An entry that left the queue (removed while it was heard) is no place to resume:
        // then `Resume` continues with what followed it (`Queue::upcoming`).
        let position = if restart {
            self.position()
                .map(|p| (p.entry, 0))
                .filter(|(entry, _)| self.playback.queue.entry(*entry).is_some())
        } else {
            self.playback.resume_at
        };
        self.finish_playing(ended, origin);
        // Set before stop_engine saves and announces the state, so both carry it.
        self.playback.resume_at = position;
        self.stop_engine();
    }

    /// Retired events cannot select a track or authorize output, regardless of how
    /// the session stopped. History must be settled before calling this.
    pub(super) fn retire_session(&mut self) {
        self.playback.playing = None;
        self.playback.instances.clear();
        self.playback.pending_next = None;
        self.playback.failed_next.clear();
        self.output.output_at = None;
        self.output.unrendered_resume = None;
        self.playback.seek_hint.set(None);
        self.output.output_policy.cancel();
    }

    pub(super) fn stop_engine(&mut self) {
        self.output.output_fallback = None;
        self.output_recovered();
        self.engine.stop();
        self.retire_session();
        self.playback.state = PlayState::Stopped;
        self.persistence.dirty = true;
        self.save_now();
        self.emit_state();
        self.emit_position();
    }

    /// Records the play of the instance being heard, if any.
    pub(super) fn finish_playing(&mut self, ended: EndReason, origin: &Origin) {
        let Some(playing) = self.playback.playing.take() else {
            return;
        };
        self.finish_instance(playing, ended, Some(origin.clone()));
    }

    pub(super) fn finish_instance(
        &mut self,
        playing: Playing,
        ended: EndReason,
        origin: Option<Origin>,
    ) {
        let heard_ms = self.heard_ms(playing.instance);
        if let Some(instance) = self.playback.instances.get_mut(&playing.instance) {
            if instance.ended.is_some() {
                return;
            }
            instance.ended = Some(ended);
            if instance.deferred_start && !instance.start_announced && heard_ms == 0 {
                return;
            }
            let record = PlayRecord {
                track: instance.track.id,
                context: self.playback.queue.context().cloned(),
                started_at_ms: playing.started_at_ms,
                played_ms: heard_ms,
                ended,
                origin,
            };
            self.record(record);
        }
    }

    fn record(&mut self, record: PlayRecord) {
        self.catalog.record_play(record);
        self.emit(Event::HistoryChanged);
    }
}
