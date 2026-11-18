use super::*;

/// A scan job: the catalog's scan id maps to it.
pub(super) struct Job {
    pub(super) id: JobId,
    pub(super) origin: Option<Origin>,
    /// When its first scan started (a scan asked for meanwhile joins it).
    pub(super) started: Instant,
    /// None is an ordinary visible job. Maintenance is published at completion only
    /// if its diagnostics or library contents changed.
    pub(super) background: Option<ScanResult>,
    pub(super) completed: Vec<Sender<ScanResult>>,
    pub(super) diagnostics: Option<Arc<mpmp_library::ScanDiagnostics>>,
}

/// Background maintenance becomes visible when errors change or library rows change.
fn retry_result_changed(previous: &ScanResult, current: &ScanResult) -> bool {
    if !matches!(
        &current.outcome,
        JobOutcome::Completed {
            report: JobReport::Scan {
                added_count: 0,
                updated_count: 0,
                restored_count: 0,
                removed_count: 0,
                ..
            }
        }
    ) {
        return true;
    }
    // Truncated reports cannot establish scope equality. Missing internal facts must
    // conservatively publish, rather than hiding new failures or estimating counts.
    match (&previous.diagnostics, &current.diagnostics) {
        (Some(previous), Some(current)) => !previous.same_errors_in(current),
        _ => true,
    }
}

impl Actor {
    /// Tells the waveform thread which tracks come first, when that changed: the one being
    /// heard (or shown as current while stopped), then the next.
    pub(super) fn tell_waveforms(&mut self) {
        let current = match &self.playback.playing {
            Some(playing) => self
                .playback
                .instances
                .get(&playing.instance)
                .map(|i| i.track.id),
            None => self
                .idle_track
                .as_ref()
                .filter(|(entry, _)| Some(*entry) == self.playback.queue.current())
                .map(|(_, track)| track.id),
        };
        let next = self
            .playback
            .pending_next
            .and_then(|i| self.playback.instances.get(&i))
            .map(|i| i.track.id);
        let tracks: Vec<TrackId> = current.into_iter().chain(next).collect();
        if tracks != self.workers.waveform_tracks {
            self.workers.waveforms.queue().playing(tracks.clone());
            self.workers.waveform_tracks = tracks;
        }
    }

    /// Reports a damaged library that the catalog found on any of its calls, on any
    /// thread (`Snapshot.problems`): whether it is new.
    pub(super) fn check_catalog(&mut self) -> bool {
        let recovered = self.catalog.take_recovered_covers();
        if !recovered.is_empty() {
            for chunk in recovered.chunks(crate::contract::MAX_COVER_EDIT_TRACKS as usize) {
                self.emit_as(
                    None,
                    Event::CoversChanged {
                        tracks: chunk.to_vec(),
                    },
                );
                self.emit_as(
                    None,
                    Event::TracksChanged {
                        tracks: chunk.to_vec(),
                    },
                );
            }
            self.emit_as(None, Event::ActionsChanged);
            self.emit_as(None, Event::LibraryChanged);
            self.idle_track = None;
            self.refresh_idle_track();
        }
        let recovering = self.catalog.cover_recovery_required();
        if !recovering
            && self
                .problems
                .iter()
                .any(|p| p.code == ErrorCode::CoverRecoveryRequired)
        {
            self.problems
                .retain(|p| p.code != ErrorCode::CoverRecoveryRequired);
            self.emit_state_as(None);
        }
        if recovering
            && !self
                .problems
                .iter()
                .any(|p| p.code == ErrorCode::CoverRecoveryRequired)
        {
            self.add_problem(
                ErrorCode::CoverRecoveryRequired,
                concat!(
                    "An embedded cover write has pending recovery data or retained copies; ",
                    "review its recovery options in the UI",
                )
                .into(),
            );
        }

        if self
            .problems
            .iter()
            .any(|p| p.code == ErrorCode::LibraryDamaged)
        {
            return false;
        }
        match self.catalog.damaged() {
            Some(message) => {
                self.add_problem(ErrorCode::LibraryDamaged, message);
                true
            }
            None => false,
        }
    }

    /// Starts a library scan, or joins the running one: its job.
    pub(super) fn start_scan(&mut self, origin: Option<Origin>) -> Result<JobId, ContractError> {
        self.start_scan_scoped(origin, None)
    }

    pub(super) fn start_scan_scoped(
        &mut self,
        origin: Option<Origin>,
        roots: Option<Vec<RootId>>,
    ) -> Result<JobId, ContractError> {
        self.start_scan_scoped_inner(origin, roots, None)
    }

    pub(super) fn start_scan_scoped_inner(
        &mut self,
        origin: Option<Origin>,
        roots: Option<Vec<RootId>>,
        maintenance: Option<(Option<ScanResult>, Sender<ScanResult>)>,
    ) -> Result<JobId, ContractError> {
        let prepared = if let crate::contract::QueryResult::Roots { roots: configured } =
            self.catalog.query(&crate::contract::Query::Roots)
        {
            let paths = configured
                .into_iter()
                .filter(|r| !r.released && roots.as_ref().is_none_or(|ids| ids.contains(&r.id)))
                .map(|r| (r.id, r.path))
                .collect::<Vec<_>>();
            self.root_access.prepare_scan(&paths)
        } else {
            Vec::new()
        };
        let tx = self.tx.clone();
        let ticket = self.catalog.rescan_roots(
            roots.clone(),
            Box::new(move |event| {
                let _ = tx.send(Msg::Scan(event));
            }),
        )?;
        for coverage in prepared {
            coverage.acknowledge();
        }
        self.workers.awaiting_startup_scan = false;
        tracing::debug!(
            ?roots,
            scan = ticket.scan,
            joined = ticket.joined,
            ?origin,
            "core scan requested"
        );
        if let Some(job) = self.scans.jobs.get_mut(&ticket.scan) {
            // An explicit request joining a background retry has ordinary job semantics.
            if let Some((_, completed)) = maintenance {
                job.completed.push(completed);
            } else if job.background.take().is_some() {
                job.origin = origin;
                crate::jobs::lock(&self.job_history).start(job.id, JobKind::Scan);
            }
            return Ok(job.id);
        }
        let id = self.new_job();
        let (background, completed) = maintenance
            .map(|(previous, completed)| (previous, vec![completed]))
            .unwrap_or_default();
        if background.is_none() {
            crate::jobs::lock(&self.job_history).start(id, JobKind::Scan);
        }
        self.scans.jobs.insert(
            ticket.scan,
            Job {
                id,
                origin,
                started: Instant::now(),
                background,
                completed,
                diagnostics: None,
            },
        );
        Ok(id)
    }

    /// A job id never handed out before.
    pub(super) fn new_job(&mut self) -> JobId {
        self.scans.next_job += 1;
        JobId(self.scans.next_job)
    }

    /// What a tag job is told of the files of `tracks` (`checked`: an agent's targets, one
    /// per track, as `checks::CheckContext::targets` made them).
    pub(super) fn tag_files(&self, tracks: &[TrackId], checked: Option<&[Target]>) -> TagFiles {
        let open = Arc::clone(&self.open_files);
        let catalog = Arc::clone(&self.catalog);
        TagFiles {
            cover_sources: None,
            undo_guard: None,
            checked: checked.map(|targets| {
                checks::unique(tracks)
                    .into_iter()
                    .zip(targets)
                    .filter_map(|(track, target)| match target {
                        Target::Library {
                            location: Some(location),
                            folder: false,
                            ..
                        } => Some((track, location.clone())),
                        _ => None,
                    })
                    .collect()
            }),
            in_use: Arc::new(move |track| {
                catalog.uri_of(track).ok().flatten().is_some_and(|uri| {
                    open.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .contains(&uri)
                })
            }),
        }
    }

    /// A tag job's events, as messages to the actor.
    pub(super) fn tag_events(&self, job: JobId) -> Box<dyn Fn(TagJobEvent) + Send + Sync> {
        let tx = self.tx.clone();
        Box::new(move |event| {
            let _ = tx.send(Msg::TagJob { job, event });
        })
    }

    /// Whether an automatic pass should start now: something changed, no loudness job
    /// runs, and no library scan has just started (its results come first; a scan that
    /// goes on and on, as when one is asked for while it runs, does not hold it back).
    pub(super) fn automatic_ready(&self) -> bool {
        self.workers.automatic_loudness
            && self.workers.automatic_dirty
            && !self.workers.awaiting_startup_scan
            && self.workers.loudness_job.is_none()
            && !self
                .scans
                .jobs
                .values()
                .any(|job| job.started.elapsed() < AUTOMATIC_SCAN_WAIT)
    }

    /// A pass leaves a retry it scheduled in place: tracks that failed are skipped until
    /// they are due, and the pass's end schedules the next retry.
    pub(super) fn start_automatic_loudness(&mut self) {
        if !self.automatic_ready() {
            return;
        }
        self.workers.automatic_dirty = false;
        if !self.catalog.supports_loudness_scan() {
            return;
        }

        if matches!(
            self.catalog.query(&crate::contract::Query::Roots),
            crate::contract::QueryResult::Roots { roots } if roots.is_empty(),
        ) {
            return;
        }
        let job = self.new_job();
        match self.workers.loudness.start_automatic(job) {
            Ok(()) => {
                self.workers.loudness_job = Some((job, Origin::System));
                self.workers.automatic_job = Some(job);
                crate::jobs::lock(&self.job_history).start(job, JobKind::LoudnessScan);
            }
            Err(error) => {
                self.note(&error);
                self.workers.automatic_retry_at = Some(Instant::now() + Duration::from_secs(30));
            }
        }
    }

    /// Which tracks make up some albums changed (a folder removed or back): their
    /// aggregates were dropped, and an automatic pass rebuilds them from the members'
    /// statistics (decoding nothing). Loaded tracks may have lost their album gain.
    pub(super) fn members_changed(&mut self) {
        self.workers.automatic_dirty = true;
        if self.refresh_loaded_tracks() {
            self.emit_state();
        }
    }

    pub(super) fn on_loudness(&mut self, job: JobId, event: crate::loudness_scan::LoudnessEvent) {
        let Some((id, origin)) = self.workers.loudness_job.clone() else {
            return;
        };
        if id != job {
            return;
        }
        // No command started an automatic pass: its events have no origin (guarantee 5).
        let origin = (self.workers.automatic_job != Some(job)).then_some(origin);
        match event {
            crate::loudness_scan::LoudnessEvent::Progress { done, total } => self.emit_as(
                origin,
                Event::JobProgress {
                    job,
                    kind: JobKind::LoudnessScan,
                    done,
                    total,
                },
            ),
            crate::loudness_scan::LoudnessEvent::Finished {
                outcome,
                tracks,
                followup,
            } => {
                self.workers.loudness_job = None;
                let automatic = self.workers.automatic_job.take() == Some(job);
                if automatic {
                    self.workers.automatic_rescan |= followup.rescan;
                    self.workers.automatic_retry_at = match &outcome {
                        // The pass itself failed (the library could not be read).
                        JobOutcome::Failed { .. } => {
                            let delay = 30u64
                                .saturating_mul(1u64 << self.workers.automatic_failures.min(6))
                                .min(1800);
                            self.workers.automatic_failures =
                                self.workers.automatic_failures.saturating_add(1);
                            Some(Instant::now() + Duration::from_secs(delay))
                        }
                        // Cancelled (not by shutdown: the actor is still here): whoever
                        // cancelled it gets a moment for what it cancelled it for (a
                        // compatibility command refused as `job_busy` meanwhile), then a
                        // pass goes on with what is left (also what changed meanwhile).
                        JobOutcome::Cancelled => {
                            self.workers.automatic_dirty = false;
                            Some(Instant::now() + AUTOMATIC_RESUME)
                        }
                        _ => {
                            self.workers.automatic_failures = 0;
                            followup.retry_at
                        }
                    };
                }
                if !automatic {
                    self.workers.automatic_dirty = true;
                }
                if let JobOutcome::Failed { error } = &outcome {
                    self.note(error);
                }
                let changed = !tracks.is_empty();
                if changed {
                    self.refresh_loaded_tracks();
                    self.emit_state_as(origin.clone());
                    self.emit_as(origin.clone(), Event::TracksChanged { tracks });
                }
                self.emit_as(
                    origin.clone(),
                    Event::JobFinished {
                        job,
                        kind: JobKind::LoudnessScan,
                        outcome,
                    },
                );
                if changed {
                    self.emit_as(origin, Event::LibraryChanged);
                }
            }
        }
    }

    /// Refresh loaded metadata copies without reopening or changing playback instances.
    pub(super) fn refresh_loaded_tracks(&mut self) -> bool {
        let mut ids: Vec<_> = self
            .playback
            .instances
            .values()
            .map(|i| i.track.id)
            .collect();
        ids.extend(self.idle_track.as_ref().map(|(_, t)| t.id));
        ids.sort();
        ids.dedup();
        let Ok(updated) = self.catalog.tracks(&ids) else {
            return false;
        };
        let mut changed = false;
        let mut gain_changed = false;
        for instance in self.playback.instances.values_mut() {
            if let Some(track) = updated.iter().find(|t| t.id == instance.track.id) {
                gain_changed |= instance.track.replay_gain != track.replay_gain;
                changed |= instance.track != *track;
                instance.track = track.clone();
            }
        }
        if let Some((_, idle)) = &mut self.idle_track
            && let Some(track) = updated.iter().find(|t| t.id == idle.id)
        {
            gain_changed |= idle.replay_gain != track.replay_gain;
            changed |= *idle != *track;
            *idle = track.clone();
        }
        if gain_changed {
            self.regain();
        }
        changed
    }

    /// A tag job went on, or ended: what it wrote is announced (`TracksChanged`, then the
    /// action, `JobFinished` and `LibraryChanged`), with the origin of its command.
    pub(super) fn on_tag_job(&mut self, job: JobId, event: TagJobEvent) {
        let origin = self.workers.tag_jobs.get(&job).cloned();
        let kind = if self.workers.cover_jobs.contains(&job) {
            JobKind::CoverEdit
        } else {
            JobKind::TagEdit
        };
        match event {
            TagJobEvent::Progress { done, total } => self.emit_as(
                origin,
                Event::JobProgress {
                    job,
                    kind,
                    done,
                    total: Some(total),
                },
            ),
            TagJobEvent::Finished { outcome, tracks } => {
                // Publish terminal authority before any event about this job's changes.
                crate::jobs::finish(&self.job_history, job, kind, &outcome);
                self.workers.tag_jobs.remove(&job);
                self.workers.cover_jobs.remove(&job);
                if let JobOutcome::Failed { error } = &outcome {
                    self.note(error);
                }
                let recorded = matches!(
                    &outcome,
                    JobOutcome::Completed {
                        report: JobReport::TagEdit {
                            action: Some(_),
                            ..
                        } | JobReport::CoverEdit {
                            action: Some(_),
                            ..
                        }
                    }
                );
                let written = !tracks.is_empty();
                if written {
                    self.workers.automatic_dirty = true;
                    if self.refresh_loaded_tracks() {
                        self.emit_state_as(origin.clone());
                    }
                    // The current track while stopped is shown from a copy.
                    if self
                        .idle_track
                        .as_ref()
                        .is_some_and(|(_, t)| tracks.contains(&t.id))
                    {
                        self.idle_track = None;
                        self.refresh_idle_track();
                    }
                    for chunk in tracks.chunks(crate::contract::MAX_COVER_EDIT_TRACKS as usize) {
                        if kind == JobKind::CoverEdit {
                            self.emit_as(
                                origin.clone(),
                                Event::CoversChanged {
                                    tracks: chunk.to_vec(),
                                },
                            );
                        }
                        self.emit_as(
                            origin.clone(),
                            Event::TracksChanged {
                                tracks: chunk.to_vec(),
                            },
                        );
                    }
                }
                if recorded {
                    self.emit_as(origin.clone(), Event::ActionsChanged);
                }
                self.emit_as(origin.clone(), Event::JobFinished { job, kind, outcome });
                if written {
                    self.emit_as(origin, Event::LibraryChanged);
                }
            }
        }
    }

    pub(super) fn on_scan(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::Diagnostics { scan, diagnostics } => {
                if let Some(job) = self.scans.jobs.get_mut(&scan) {
                    job.diagnostics = Some(diagnostics);
                }
            }
            ScanEvent::Progress { scan, done, total } => {
                if let Some(job) = self
                    .scans
                    .jobs
                    .get(&scan)
                    .filter(|job| job.background.is_none())
                {
                    let (id, origin) = (job.id, job.origin.clone());
                    self.emit_as(
                        origin,
                        Event::JobProgress {
                            job: id,
                            kind: JobKind::Scan,
                            done,
                            total,
                        },
                    );
                }
            }
            ScanEvent::Finished { scan, outcome } => {
                // Only a scan that changed something gives an automatic pass work: one
                // that found every file as it was (the desktop's periodic check) does not.
                // A failed or cancelled scan may have applied part of its changes.
                self.workers.automatic_dirty |= !matches!(
                    &outcome,
                    JobOutcome::Completed {
                        report: JobReport::Scan {
                            added_count: 0,
                            updated_count: 0,
                            restored_count: 0,
                            removed_count: 0,
                            ..
                        }
                    }
                );
                if let JobOutcome::Failed { error } = &outcome {
                    self.note(error);
                }
                let (id, origin, publish, diagnostics) = match self.scans.jobs.remove(&scan) {
                    Some(job) => {
                        let result = ScanResult {
                            outcome: outcome.clone(),
                            diagnostics: job.diagnostics.clone(),
                        };
                        for completed in job.completed {
                            let _ = completed.send(result.clone());
                        }
                        let publish = job
                            .background
                            .as_ref()
                            .is_none_or(|previous| retry_result_changed(previous, &result));
                        (job.id, job.origin, publish, job.diagnostics)
                    }
                    None => (self.new_job(), None, true, None),
                };
                // A rescan may invalidate derived track/album gains, including another
                // member's album gain. Refresh the few loaded copies before publishing.
                if self.refresh_loaded_tracks() {
                    self.emit_state_as(origin.clone());
                }
                if publish {
                    // Retain exact Rust-only facts before publishing the public result.
                    crate::jobs::finish(&self.job_history, id, JobKind::Scan, &outcome);
                    if let Some(diagnostics) = diagnostics {
                        crate::jobs::lock(&self.job_history)
                            .attach_scan_diagnostics(id, diagnostics);
                    }
                    self.emit_recorded_as(
                        origin.clone(),
                        Event::JobFinished {
                            job: id,
                            kind: JobKind::Scan,
                            outcome,
                        },
                    );
                }
                self.emit_as(origin, Event::LibraryChanged);
            }
        }
    }
}
