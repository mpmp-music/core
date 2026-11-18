use super::*;
use crate::gain;

pub(super) struct Subscriber {
    pub(super) tx: Sender<EventEnvelope>,
    pub(super) lagging: bool,
    /// Receives `ConfirmationRequested`.
    pub(super) ui: bool,
    pub(super) positions: bool,
}

/// Tries to deliver a lagging subscriber's `Resync`; false if it is gone.
fn send_resync(sub: &mut Subscriber, seq: u64, snapshot: Option<&Snapshot>) -> bool {
    let Some(snapshot) = snapshot else {
        return true;
    };
    match sub.tx.try_send(EventEnvelope {
        seq,
        origin: None,
        event: Event::Resync {
            snapshot: Box::new(snapshot.clone()),
        },
    }) {
        Ok(()) => {
            sub.lagging = false;
            true
        }
        Err(TrySendError::Full(_)) => true,
        Err(TrySendError::Disconnected(_)) => false,
    }
}

impl Actor {
    /// Notes what a failure tells of core's data (`Snapshot.problems`).
    pub(super) fn note(&mut self, error: &ContractError) {
        if error.code == ErrorCode::LibraryDamaged {
            self.add_problem(error.code, error.message.clone());
        }
    }

    /// Adds a problem (once per code) and announces it.
    pub(super) fn add_problem(&mut self, code: ErrorCode, message: String) {
        if self.problems.iter().any(|p| p.code == code) {
            return;
        }
        tracing::error!("{}: {message}", code.as_str());
        self.problems.push(Problem {
            code,
            message,
            since_ms: unix_ms(),
        });
        // Published before it is announced, so that a query on the event sees it (as for
        // the tick, which has no command to publish after). Core noticed it; no command
        // caused it.
        self.publish();
        let cause = self.publication.cause.take();
        self.emit_state();
        self.publication.cause = cause;
    }

    /// Where the instance being heard is, or a seek just sent to it.
    pub(super) fn position(&self) -> Option<PositionInfo> {
        let now = self.now_ms();
        let rate = if self.playback.state == PlayState::Playing {
            1.0
        } else {
            0.0
        };
        let engine = self.engine.position().and_then(|p| {
            let instance = self.playback.instances.get(&p.entry.0)?;
            let ms = p.frame * 1000 / u64::from(p.sample_rate.max(1));
            Some((p.entry.0, instance.entry, ms))
        });
        if let Some(hint) = self.playback.seek_hint.get() {
            let expected = hint.at(now);
            // The engine reports from the seek target on, allowing for output buffering
            // and faster-than-real-time rendering (tests).
            let caught_up = self.playback.state != PlayState::Paused
                && engine.is_some_and(|(instance, _, ms)| {
                    instance == hint.instance
                        && ms.saturating_add(250) >= hint.position_ms
                        && ms
                            <= expected
                                .saturating_add(1_000)
                                .saturating_add(now.saturating_sub(hint.at_ms).saturating_mul(8))
                });
            match self.playback.instances.get(&hint.instance) {
                Some(i) if !caught_up => {
                    return Some(PositionInfo {
                        entry: i.entry,
                        position_ms: expected,
                        at_ms: now,
                        rate,
                    });
                }
                _ => self.playback.seek_hint.set(None),
            }
        }
        engine.map(|(_, entry, ms)| PositionInfo {
            entry,
            position_ms: ms,
            at_ms: now,
            rate,
        })
    }

    /// The position as the snapshot reports it: also while stopped (where a resume
    /// starts).
    pub(super) fn snapshot_position(&self) -> Option<PositionInfo> {
        match self.playback.state {
            PlayState::Stopped => self
                .playback
                .resume_at
                .filter(|(entry, _)| self.playback.queue.entry(*entry).is_some())
                .map(|(entry, position_ms)| PositionInfo {
                    entry,
                    position_ms,
                    at_ms: self.now_ms(),
                    rate: 0.0,
                }),
            _ => self.position(),
        }
    }

    pub(super) fn emit_position(&mut self) {
        if let Some(position) = self.snapshot_position() {
            self.emit(Event::Position { position });
        }
    }

    /// Keeps the track of a current-but-stopped entry at hand, so snapshots never read
    /// the library (they are taken on every change).
    pub(super) fn refresh_idle_track(&mut self) {
        let wanted = self.playback.queue.current();
        if self.idle_track.as_ref().map(|(e, _)| *e) == wanted {
            return;
        }
        self.idle_track = wanted.and_then(|entry| {
            let id = self.playback.queue.entry(entry)?.track;
            Some((entry, self.catalog.tracks(&[id]).ok()?.pop()?))
        });
    }

    pub(super) fn queue_summary(&self) -> QueueSummary {
        QueueSummary {
            next_entry: self.playback.queue.skip_target(),
            natural_next_entry: self.playback.queue.upcoming(),
            revision: self.playback.queue.revision(),
            entry_count: u32::try_from(self.playback.queue.len()).unwrap_or(u32::MAX),
            current_index: self
                .playback
                .queue
                .current_index()
                .map(|i| u32::try_from(i).unwrap_or(u32::MAX)),
        }
    }

    /// Emits `QueueChanged` if the queue summary differs from the last one announced.
    pub(super) fn announce_queue(&mut self) {
        let queue = self.queue_summary();
        if queue != self.publication.announced_queue {
            self.publication.announced_queue = queue;
            self.emit(Event::QueueChanged { queue });
        }
    }

    pub(super) fn snapshot(&self) -> Snapshot {
        // What is heard wins, even if its entry has left the queue.
        let current = match &self.playback.playing {
            Some(playing) => self
                .playback
                .instances
                .get(&playing.instance)
                .map(|i| NowPlaying {
                    entry: i.entry,
                    track: i.track.clone(),
                    signal_path: playing
                        .signal_path
                        .clone()
                        .map(|path| self.with_gain_stages(path, i.gain)),
                }),
            None => self
                .idle_track
                .as_ref()
                .filter(|(entry, _)| Some(*entry) == self.playback.queue.current())
                .map(|(entry, track)| NowPlaying {
                    entry: *entry,
                    track: track.clone(),
                    signal_path: None,
                }),
        };
        Snapshot {
            seq: self.publication.state_seq,
            now_ms: self.now_ms(),
            state: self.playback.state,
            current,
            position: self.snapshot_position(),
            queue: self.queue_summary(),
            shuffle: self.playback.queue.shuffle(),
            repeat: self.playback.queue.repeat(),
            audio: AudioSettings {
                volume: self.audio.volume,
                volume_db: gain::volume_db(self.audio.volume),
                muted: self.audio.muted,
                volume_mode: VolumeMode::Software,
                replay_gain: self.audio.replay_gain,
                output: Some(self.output.output_settings.clone()),
            },
            playback_limits: Some(self.playback_limits_info()),
            output: Some(self.output_state()),
            problems: self.problems.clone(),
            ..empty_snapshot(self.catalog.storage_write_capabilities())
        }
    }

    /// Publishes the snapshot for queries. The queue list is rebuilt only when it changed.
    pub(super) fn publish(&mut self) {
        self.refresh_idle_track();
        let revision = self.playback.queue.revision();
        if self.published_queue.0 != revision {
            let entries = self
                .playback
                .queue
                .entries()
                .iter()
                .map(|e| (e.id, e.track))
                .collect();
            self.published_queue = (revision, Arc::new(entries));
        }
        self.published.store(Arc::new(Published {
            snapshot: self.snapshot(),
            queue: Arc::clone(&self.published_queue.1),
        }));
    }

    pub(super) fn emit_state(&mut self) {
        self.emit_state_as(self.publication.cause.clone());
    }

    pub(super) fn emit_output_as(&mut self, origin: Option<Origin>) {
        let output = self.output_state();
        if self.output.announced_output.as_ref() != Some(&output) {
            self.output.announced_output = Some(output.clone());
            self.emit_as(origin, Event::OutputChanged { output });
        }
    }

    pub(super) fn emit_state_as(&mut self, origin: Option<Origin>) {
        self.emit_output_as(origin.clone());
        self.refresh_idle_track();
        let mut snapshot = Box::new(self.snapshot());
        // The snapshot reflects this very event.
        snapshot.seq = self.publication.seq + 1;
        self.emit_as(origin, Event::StateChanged { snapshot });
    }

    /// Emits an event caused by the command being handled (if any).
    pub(super) fn emit(&mut self, event: Event) {
        self.emit_as(self.publication.cause.clone(), event);
    }

    pub(super) fn emit_as(&mut self, origin: Option<Origin>, event: Event) {
        if let Event::TracksChanged { tracks } = &event
            && self
                .current_track()
                .is_some_and(|track| tracks.contains(&track.id))
            && self.refresh_loaded_tracks()
        {
            self.emit_state_as(origin.clone());
        }
        if matches!(event, Event::JobProgress { .. } | Event::JobFinished { .. }) {
            crate::jobs::observe(&self.job_history, &event);
        }
        self.emit_recorded_as(origin, event);
    }

    /// Publish an event whose job facts were already recorded. Do not reinsert a job
    /// evicted while attaching its exact diagnostics under the history budget.
    pub(super) fn emit_recorded_as(&mut self, origin: Option<Origin>, event: Event) {
        // Music folders count too: where they change, `folders_changed` (a scan's
        // `LibraryChanged` changes none of what checks depend on).
        if matches!(event, Event::PermissionsChanged | Event::AgentsChanged) {
            self.generation += 1;
        }
        self.publication.seq += 1;
        let is_position = matches!(event, Event::Position { .. });
        if !is_position {
            self.publication.state_seq = self.publication.seq;
        }
        let ui_only = matches!(
            event,
            Event::ConfirmationRequested { .. } | Event::CoverRecoveryResolved { .. }
        );
        let envelope = EventEnvelope {
            seq: self.publication.seq,
            origin,
            event,
        };
        let resync = self
            .publication
            .subscribers
            .iter()
            .any(|s| s.lagging)
            .then(|| self.snapshot());
        let seq = self.publication.seq;
        self.publication.subscribers.retain_mut(|sub| {
            if (ui_only && !sub.ui) || (is_position && !sub.positions) {
                return true;
            }
            if sub.lagging {
                // Dropped events: once there is room, tell it to start over.
                return send_resync(sub, seq, resync.as_ref());
            }
            match sub.tx.try_send(envelope.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    sub.lagging = true;
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
    }

    /// Sends the pending `Resync` to subscribers that fell behind, as soon as they have
    /// room (not only with the next event).
    pub(super) fn resync_lagging(&mut self) {
        if !self.publication.subscribers.iter().any(|s| s.lagging) {
            return;
        }
        let snapshot = self.snapshot();
        let seq = self.publication.seq;
        self.publication.subscribers.retain_mut(|sub| {
            if sub.lagging {
                send_resync(sub, seq, Some(&snapshot))
            } else {
                true
            }
        });
    }
}
