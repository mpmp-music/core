use super::*;

impl Scanner<'_> {
    pub(super) fn run(&mut self, id: ScanId, claim: &mut ScanClaim<'_>) -> Result<ScanReport> {
        let (coalesced, cancelled) = match self.scan_all(id, claim) {
            Ok(coalesced) => (coalesced, false),
            Err(LibraryError::Cancelled) => {
                // Keep the work done so far.
                if let Err(err) = self.flush() {
                    tracing::warn!(%err, "cannot write rows of cancelled scan");
                }
                tracing::info!(?self.progress, "library scan cancelled");
                (claim.coalesced(), true)
            }
            Err(err) => return Err(err),
        };
        let p = self.progress;
        Ok(ScanReport {
            diagnostics: std::mem::take(&mut self.diagnostics),
            scan: id,
            scanned: p.scanned,
            added: p.added,
            updated: p.updated,
            restored: p.restored,
            removed: p.removed,
            errors: p.errors,
            first_errors: std::mem::take(&mut self.first_errors),
            passes: self.passes,
            coalesced,
            cancelled,
        })
    }

    /// Counts an error and keeps it if it is among the first [`SCAN_ERROR_LIMIT`]. A path
    /// counts once per scan, also when a later pass meets it again.
    pub(super) fn error(
        &mut self,
        path: &Path,
        code: ScanErrorCode,
        message: impl std::fmt::Display,
    ) {
        let message = diagnostic(message);
        if self.diagnostics.errors.contains_key(path) {
            return;
        }
        let mut digest = Sha256::new();
        digest.update(format!("{code:?}\0"));
        digest.update(message.as_bytes());
        self.diagnostics
            .errors
            .insert(path.to_owned(), digest.finalize().into());
        self.progress.errors += 1;
        if self.first_errors.len() < SCAN_ERROR_LIMIT {
            self.first_errors.push(ScanError {
                code,
                path: path.to_string_lossy().into_owned(),
                message,
            });
        }
    }

    /// The passes; returns how many requests were coalesced into the scan.
    fn scan_all(&mut self, id: ScanId, claim: &mut ScanClaim<'_>) -> Result<u32> {
        let coalesced = loop {
            self.passes += 1;
            tracing::debug!(scan = id.0,
             pass = self.passes,
             scope = ?self.scope,
             "library scan pass started");

            self.pass()?;
            match claim.finish_or_rerun() {
                Ok(coalesced) => break coalesced,
                Err(scope) => {
                    self.scope = scope;
                    tracing::info!(?self.scope,
                     "library scan requested during scan; scanning again");
                }
            }
        };
        if let Some(seen) = self.rescan_request.filter(|_| !self.root_skipped) {
            self.inner
                .writer
                .run(move |conn| Ok(schema::clear_rescan(conn, seen)?))?;
        }
        // Planner statistics for what the scan added (the first scan fills the library).
        if let Err(err) = self.inner.writer.run(|conn| Ok(schema::optimize(conn)?)) {
            tracing::warn!(%err, "cannot update the library statistics");
        }
        (self.report)(self.progress);
        tracing::info!(scan = id.0,
         passes = self.passes,
         coalesced,
         ?self.progress,
         "library scan finished");

        Ok(coalesced)
    }

    /// One pass over the roots as they are now.
    fn pass(&mut self) -> Result<()> {
        let (roots, rescan_request) = self
            .inner
            .pool
            .read(|conn| Ok((reads::roots(conn)?, schema::rescan_request(conn)?)))?;
        // Removed roots no longer own session-local sidecar dependency evidence.
        self.inner
            .cue_dependencies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|audio, _| roots.iter().any(|root| audio.starts_with(&root.path)));
        let scope = self.scope.clone();
        // A scoped scan cannot acknowledge a full-library upgrade rescan request.
        self.rescan_request = rescan_request.filter(|_| scope.full());
        self.root_skipped = false;
        self.check_cancel()?;
        // Seed pending identities before the first root can flush an alias batch.
        self.observed.prewarm_pending(self.inner)?;
        for root in roots {
            if !scope.covers(&root) {
                continue;
            }
            // Acquire only for the root being processed, never while scanning another root.
            let Some(_access) = self.inner.file_access.acquire(root.id) else {
                self.root_skipped = true;
                continue;
            };
            let mut epoch = self.inner.file_namespace_epoch.load(Ordering::Acquire);
            #[cfg(all(test, feature = "desktop-fs"))]
            if let Some(hook) = self.inner.hooks.scan_before_walk.lock().unwrap().clone() {
                hook();
            }
            let result = self.walk(&root).and_then(|mut files| {
                #[cfg(all(test, feature = "desktop-fs"))]
                if let Some(hook) = self.inner.hooks.scan_after_walk.lock().unwrap().clone() {
                    hook();
                }
                {
                    // Startup already reconciled all intents. Runtime retries are
                    // limited to enumerated directories, never unrelated offline paths.
                    let _writes = self
                        .inner
                        .tag_jobs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let directories = files
                        .as_ref()
                        .map(|w| w.enumerated.clone())
                        .unwrap_or_default();
                    let restored = if self.inner.cover_recovery.load(Ordering::Acquire) {
                        let observed: Vec<_> = files
                            .iter()
                            .flat_map(|walked| walked.files.iter())
                            .map(|file| (root.path.join(&file.rel_path), file.file_identity))
                            .collect();
                        crate::cover::job::recover_observed(self.inner, &observed, &directories)?
                    } else {
                        false
                    };
                    let current = self.inner.file_namespace_epoch.load(Ordering::Acquire);
                    if restored || current != epoch {
                        // A walk concurrent with either move is invalid, even after its
                        // intent has been finalized. Rewalk the selected scope under the gate.
                        files = self.walk(&root)?;
                        epoch = self.inner.file_namespace_epoch.load(Ordering::Acquire);
                    }
                }
                (self.report)(self.progress);
                self.scan_root(&root, files, epoch)
            });
            match result {
                Err(LibraryError::RootReleased(_)) => {
                    self.root_skipped = true;
                    self.batch = ScanBatch::default();
                    self.batch_root = None;
                }
                other => other?,
            }
        }
        self.inner.writer.call(|reply| WriteCmd::Flush { reply })
    }

    pub(super) fn check_root(&self, root: RootId) -> Result<()> {
        self.check_cancel()?;
        if self.inner.file_access.released(root) {
            Err(LibraryError::RootReleased(root))
        } else {
            Ok(())
        }
    }

    fn check_cancel(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(LibraryError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub(super) fn tick(&mut self) {
        if self.last_report.elapsed() >= PROGRESS_INTERVAL {
            self.last_report = Instant::now();
            (self.report)(self.progress);
        }
    }

    fn scan_root(&mut self, root: &Root, walked: Option<Walked>, epoch: u64) -> Result<()> {
        self.flush()?;
        self.check_root(root.id)?;
        self.batch_root = Some(root.id);
        self.batch.forced_encoding = root.forced_encoding.clone();
        // An unreachable root (walk already counted the error) keeps its tracks as they are.
        let Some(Walked {
            files,
            sheets,
            completeness,
            enumerated: _,
        }) = walked
        else {
            return Ok(());
        };
        self.observed.record_root(
            &root.path,
            files
                .iter()
                .map(|file| (file.rel_path.as_str(), file.file_identity)),
        );
        self.batch.observed = self.observed.clone();
        let known = self
            .inner
            .pool
            .read(|conn| reads::root_files(conn, root.id))?;
        if !self.scope.directory_only(root) && files.is_empty() && known.any_available() {
            // An empty mount point looks like this when its drive is unplugged.
            tracing::warn!(
                root = %root.path.display(),
                "library root has no audio files any more; skipped"
            );
            self.error(
                &root.path,
                ScanErrorCode::NoAudioFiles,
                "library root has no audio files any more; skipped",
            );
            self.root_skipped = true;
            return Ok(());
        }
        let forced = match root.forced_encoding.as_deref() {
            None => None,
            Some(label) => match encoding_for_label(label) {
                Some(encoding) => Some(encoding),
                None => {
                    tracing::warn!(label, "ignoring unknown forced encoding of library root");
                    None
                }
            },
        };
        let mut seen: HashSet<TrackId> =
            HashSet::with_capacity(known.files.len() + known.cues.len());
        let mut plan = self.plan_sheets(root, &sheets, &files, forced, &known)?;
        // A virtual track is keyed by its sheet as well as its audio path. An
        // unreadable sheet retains current tracks and ownership of the audio.
        // Unavailable historical cuts are not evidence of current ownership.
        for ((sheet, _), cue) in &known.cues {
            if cue.available
                && (!completeness.can_mark_missing(sheet)
                    || !completeness.can_mark_missing(&cue.rel_path))
            {
                plan.kept.push(cue.id);
                plan.claimed.insert(cue.rel_path.clone());
            }
        }
        if self.scope.directory_only(root) {
            // A sidecar outside the retry scope still owns its audio. Do not unhide it
            // or replace its virtual tracks just because that sheet was not walked.
            for ((sheet, _), cue) in &known.cues {
                if cue.available
                    && !sheets.iter().any(|found| found.rel_path == *sheet)
                    && *sheet != cue.rel_path
                {
                    plan.claimed.insert(cue.rel_path.clone());
                }
            }
        }
        let singletons: Vec<_> = files
            .iter()
            .filter(|f| {
                plan.supplements.contains_key(&f.rel_path) && !plan.claimed.contains(&f.rel_path)
            })
            .map(|f| crate::db::writer::Singleton {
                path: f.rel_path.clone(),
                identity: f.file_identity,
                sheet_track: plan.singleton_evidence.get(&f.rel_path).cloned(),
            })
            .collect();
        if !singletons.is_empty() {
            let root_id = root.id;
            let retained = self.inner.writer.run_data(move |conn| {
                crate::db::writer::reclassify_singletons(conn, root_id, &singletons)
            })?;
            for (path, ids) in retained {
                plan.claimed.insert(path.clone());
                plan.supplements.remove(&path);
                plan.kept.extend(ids);
            }
        }
        let known = self
            .inner
            .pool
            .read(|conn| reads::root_files(conn, root.id))?;

        self.scan_sheet_tracks(root, &plan, &files, forced, &known, &mut seen)?;
        // Tracks of sheets embedded in a file, by that file.
        let mut embedded: HashMap<&str, Vec<(u32, &KnownCue)>> = HashMap::new();
        for ((sheet, number), cue) in &known.cues {
            if *sheet == cue.rel_path {
                embedded.entry(sheet).or_default().push((*number, cue));
            }
        }
        for file in &files {
            #[cfg(all(test, feature = "desktop-fs"))]
            if let Some(hook) = self.inner.hooks.scan_file.lock().unwrap().clone() {
                hook(root.id, &file.rel_path);
            }
            self.check_root(root.id)?;
            let counted = self
                .audio_paths
                .entry((root.id, file.rel_path.clone()))
                .or_default();
            if !std::mem::replace(counted, true) {
                self.progress.scanned += 1;
            }
            let existing = known.files.get(&file.rel_path);
            if plan.claimed.contains(&file.rel_path) {
                // Cut into tracks by a sheet next to it: not a track of its own.
                if let Some(known) = existing {
                    seen.insert(known.id);
                    if !known.hidden {
                        self.batch.hidden.push(known.id);
                        self.flush_if_full()?;
                    }
                }
                self.tick();
                continue;
            }
            let same = |size: u64, mtime_ms: i64| size == file.size && mtime_ms == file.mtime_ms;
            let unchanged_file = existing.filter(|k| !k.hidden && same(k.size, k.mtime_ms));
            let changed_supplement = plan.changed_supplements.contains(&file.rel_path);
            // An unchanged file with a sheet in it: its tracks as last read (rows from older
            // versions of the file, or of its sheet, have other stamps).
            let current: Vec<&KnownCue> = embedded
                .get(file.rel_path.as_str())
                .into_iter()
                .flatten()
                .map(|&(_, cue)| cue)
                .filter(|cue| same(cue.size, cue.mtime_ms) && same(cue.cue_size, cue.cue_mtime_ms))
                .collect();
            if !current.is_empty() && current.iter().all(|c| c.artists_complete) {
                for cue in current {
                    seen.insert(cue.id);
                    self.keep_unchanged(cue.id, cue.available, cue.has_folder_art, file);
                }
                if let Some(known) = existing {
                    seen.insert(known.id);
                    if !known.hidden {
                        self.batch.hidden.push(known.id);
                    }
                }
                self.flush_if_full()?;
                self.tick();
                continue;
            }
            if let Some(known) = existing {
                seen.insert(known.id);
                // A hidden file is read again: whether it is still cut into tracks depends
                // on what is in it.
                if known.artists_complete && unchanged_file.is_some() && !changed_supplement {
                    self.keep_unchanged(known.id, known.available, known.has_folder_art, file);
                    self.flush_if_full()?;
                    self.tick();
                    continue;
                }
            }
            let path = root.path.join(&file.rel_path);
            match read_audio(self.inner, &path, forced) {
                Ok(mut meta) => {
                    #[cfg(all(test, feature = "desktop-fs"))]
                    if let Some(hook) = self
                        .inner
                        .hooks
                        .after_scan_audio_read
                        .lock()
                        .unwrap()
                        .clone()
                    {
                        hook(root.id, &file.rel_path);
                    }
                    if let Some(tags) = plan.supplements.get(&file.rel_path) {
                        for (key, slot) in
                            [("TITLE", &mut meta.title), ("ARTIST", &mut meta.artist)]
                        {
                            if slot.is_none()
                                && let Some((_, value)) = tags.iter().find(|(k, _)| k == key)
                            {
                                *slot = Some(value.clone());
                                meta.tags.push((key.into(), value.clone()));
                                if key == "ARTIST" {
                                    meta.artists = None;
                                }
                            }
                        }
                    }
                    let cuts = panic::catch_unwind(AssertUnwindSafe(|| {
                        meta.embedded_cue
                            .as_deref()
                            .map(|text| usable_cuts(&Sheet::parse(text), true, &meta))
                            .unwrap_or_default()
                    }));
                    let cuts = match cuts {
                        Ok(cuts) => cuts,
                        Err(_) => {
                            self.error(&path, ScanErrorCode::ReaderPanicked, "CUE reader panicked");
                            for &(_, cue) in
                                embedded.get(file.rel_path.as_str()).into_iter().flatten()
                            {
                                seen.insert(cue.id);
                            }
                            continue;
                        }
                    };
                    if cuts.is_empty() {
                        let people_only = !changed_supplement && unchanged_file.is_some();
                        let compare = unchanged_file
                            .filter(|old| changed_supplement && old.artists_complete)
                            .map(|old| old.id);
                        if !people_only
                            && let Some(fingerprint) =
                                plan.supplement_fingerprints.get(&file.rel_path)
                        {
                            self.inner
                                .cue_supplements
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(
                                    (root.id, file.rel_path.clone()),
                                    SupplementStamp {
                                        fingerprint: *fingerprint,
                                        confirmed: None,
                                    },
                                );
                            self.batch
                                .supplement_reads
                                .insert(file.rel_path.clone(), compare);
                        }
                        self.batch.upserts.push(TrackRecord {
                            root_id: root.id,
                            rel_path: file.rel_path.clone(),
                            size: file.size,
                            mtime_ms: file.mtime_ms,
                            has_folder_art: file.folder_art,
                            meta,
                            cue: None,
                            based_on: existing.map(|k| (k.size, k.mtime_ms)),
                            based_on_revision: existing.map(|k| k.revision),
                            artists_only: people_only,
                            file_identity: file.file_identity,
                        });
                        if people_only {
                            if let Some(old) = existing {
                                self.keep_unchanged(
                                    old.id,
                                    old.available,
                                    old.has_folder_art,
                                    file,
                                );
                            }
                        } else if compare.is_none() {
                            self.count_upsert(existing.is_some());
                        }
                    } else {
                        for cut in &cuts {
                            let key = (file.rel_path.clone(), cut.number);
                            let old = known.cues.get(&key);
                            seen.extend(old.map(|k| k.id));
                            // The sheet is the file itself.
                            self.push_cut(root, file, file, cut, &meta, None);
                            self.guard_cut(old, file, file);
                            if self.batch.upserts.last().is_some_and(|r| r.artists_only) {
                                if let Some(old) = old {
                                    self.keep_unchanged(
                                        old.id,
                                        old.available,
                                        old.has_folder_art,
                                        file,
                                    );
                                }
                            } else {
                                self.count_upsert(old.is_some());
                            }
                        }
                        // Its earlier tracks the sheet no longer has go below; the file's
                        // own row is hidden.
                        if let Some(known) = existing
                            && !known.hidden
                        {
                            self.batch.hidden.push(known.id);
                        }
                    }
                    self.flush_if_full()?;
                }
                Err(err) => {
                    // Existing rows keep their old data.
                    tracing::warn!(%err, path = %path.display(), "cannot read audio file");
                    if current.is_empty() && (changed_supplement || unchanged_file.is_none()) {
                        self.error(
                            &path,
                            audio_error_code(&err),
                            format_args!("cannot read audio file: {err}"),
                        );
                    }
                    for &(_, cue) in embedded.get(file.rel_path.as_str()).into_iter().flatten() {
                        seen.insert(cue.id);
                    }
                }
            }
            self.tick();
        }
        self.check_root(root.id)?;
        let mut gone: Vec<_> = known
            .files
            .iter()
            .map(|(rel_path, k)| (rel_path, k.id, k.available))
            .chain(
                known
                    .cues
                    .values()
                    .map(|k| (&k.rel_path, k.id, k.available)),
            )
            .filter(|&(rel_path, id, available)| {
                !self.scope.directory_only(root)
                    && available
                    && !seen.contains(&id)
                    && completeness.can_mark_missing(rel_path)
            })
            .map(|(relative, id, _)| (root.path.join(relative), id))
            .collect();
        self.flush()?;
        if !gone.is_empty() {
            // The generation and the absence update share the file-write gate.
            // A later publisher cannot open a gap between re-enumeration and commit.
            let _writes = self
                .inner
                .tag_jobs
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if self
                .inner
                .pool
                .read(crate::tags::job::intents::blocks_metadata_writes)?
            {
                gone.clear();
            }
            let stale = self.inner.file_namespace_epoch.load(Ordering::Acquire) != epoch;
            let mut refreshed = HashMap::new();
            for (path, id) in gone {
                self.check_root(root.id)?;
                if stale {
                    let (parent, name) = (path.parent().unwrap(), path.file_name().unwrap());
                    let names = refreshed.entry(parent.to_owned()).or_insert_with(|| {
                        std::fs::read_dir(parent).and_then(|entries| {
                            entries
                                .map(|entry| entry.map(|entry| entry.file_name()))
                                .collect::<std::io::Result<HashSet<_>>>()
                        })
                    });
                    if names.as_ref().map_or(true, |names| names.contains(name)) {
                        continue;
                    }
                }
                self.batch.unavailable.push(id);
                self.progress.removed += 1;
                self.flush_if_full()?;
            }
            self.flush()?;
        }
        self.check_root(root.id)?;
        let root_id = root.id;
        if !completeness.root_readable {
            self.root_skipped = true;
            Ok(())
        } else if !self.scope.directory_only(root) {
            self.inner
                .writer
                .call(|reply| WriteCmd::FinishScan { root_id, reply })
        } else {
            Ok(())
        }
    }

    /// An unchanged row: made available again if it was not, its folder's cover noted.
    pub(super) fn keep_unchanged(
        &mut self,
        id: TrackId,
        available: bool,
        folder_art: bool,
        file: &Found,
    ) {
        if !available {
            // Back unchanged: its folder was re-added, or the file returned.
            self.batch.available.push(id);
            self.progress.restored += 1;
        }
        if folder_art != file.folder_art {
            // Not counted as updated: the file itself did not change.
            self.batch.folder_art.push((id, file.folder_art));
        }
    }

    pub(super) fn count_upsert(&mut self, existed: bool) {
        if existed {
            self.progress.updated += 1;
        } else {
            self.progress.added += 1;
        }
    }

    pub(super) fn flush_if_full(&mut self) -> Result<()> {
        if self.batch.len() >= BATCH_ROWS {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let Some(root_id) = self.batch_root else {
            return Ok(());
        };
        if self.batch.len() == 0 {
            return Ok(());
        }
        self.check_root(root_id)?;
        // Keep the captured root configuration across every page of this scan.
        let next = ScanBatch {
            forced_encoding: self.batch.forced_encoding.clone(),
            observed: self.batch.observed.clone(),
            ..ScanBatch::default()
        };
        let batch = std::mem::replace(&mut self.batch, next);
        let result = self.inner.writer.call(|reply| WriteCmd::Scan {
            root_id,
            batch,
            reply,
        })?;
        self.progress.updated += result.updated;
        self.progress.restored += result.restored;
        if !result.confirmed_supplements.is_empty() {
            let mut cache = self
                .inner
                .cue_supplements
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for (path, id, revision) in result.confirmed_supplements {
                let key = (root_id, path);
                if let Some(stamp) = cache.get_mut(&key) {
                    if stamp.fingerprint.is_some() {
                        stamp.confirmed = Some((id, revision));
                    } else {
                        cache.remove(&key);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Reads one audio file's tags and properties. Tag parsers see arbitrary bytes; a panic on
/// one file must not end the scan.
pub(super) fn audio_error_code(error: &TagError) -> ScanErrorCode {
    match error {
        TagError::Panicked => ScanErrorCode::ReaderPanicked,
        _ => ScanErrorCode::AudioReadFailed,
    }
}

pub(super) fn read_audio(
    _inner: &Inner,
    path: &Path,
    forced: Option<&'static Encoding>,
) -> Result<TrackMeta, TagError> {
    panic::catch_unwind(AssertUnwindSafe(|| {
        #[cfg(all(test, feature = "desktop-fs"))]
        if _inner.hooks.panic_audio_reader.lock().unwrap().as_deref() == Some(path) {
            panic!("injected audio reader panic");
        }
        #[cfg(all(test, feature = "desktop-fs"))]
        if _inner.hooks.fail_audio_read.lock().unwrap().as_deref() == Some(path) {
            return Err(TagError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "temporary reader failure",
            )));
        }
        read_track(path, forced)
    }))
    .unwrap_or_else(|_| Err(TagError::Panicked))
}

const DIAGNOSTIC_BYTES: usize = 1024;

fn utf8_prefix(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Format directly into a bounded buffer, even when an error's Display is very large.
pub(super) fn diagnostic(message: impl std::fmt::Display) -> String {
    struct Bounded(String);
    impl std::fmt::Write for Bounded {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.0
                .push_str(utf8_prefix(text, DIAGNOSTIC_BYTES - self.0.len()));
            Ok(())
        }
    }
    let mut output = Bounded(String::new());
    let _ = std::fmt::write(&mut output, format_args!("{message}"));
    output.0
}

pub(super) fn missing_diagnostic(missing: &[&str]) -> String {
    const PREFIX: &str = "CUE sheet names missing files: ";
    let shown = missing.len().min(10);
    let tail = if missing.len() > shown {
        format!(", and {} more", missing.len() - shown)
    } else {
        String::new()
    };
    // Share the 1 KiB envelope among the first ten names, keeping the suffix intact.
    let name_bytes = ((DIAGNOSTIC_BYTES - PREFIX.len() - tail.len() - shown.saturating_sub(1) * 2)
        / shown.max(1))
    .min(256);
    let names: Vec<_> = missing
        .iter()
        .take(shown)
        .map(|name| utf8_prefix(name, name_bytes))
        .collect();
    format!("{PREFIX}{}{tail}", names.join(", "))
}

pub(super) struct Scanner<'a> {
    pub(super) inner: &'a Inner,
    pub(super) cancel: &'a AtomicBool,
    pub(super) report: Box<dyn Fn(ScanProgress) + 'a>,
    pub(super) progress: ScanProgress,
    /// The first [`SCAN_ERROR_LIMIT`] errors.
    pub(super) first_errors: Vec<ScanError>,
    /// Paths with an error so far (each is counted once).
    pub(super) diagnostics: crate::ScanDiagnostics,
    /// Distinct paths in this job; bool says processing (rather than just walking) reached it.
    pub(super) audio_paths: HashMap<(RootId, String), bool>,
    /// Shared for the entire scan, including later roots and coalesced passes.
    pub(super) observed: Arc<crate::cover::job::ScanObservations>,
    pub(super) passes: u32,
    pub(super) last_report: Instant,
    pub(super) batch: ScanBatch,
    /// The root `batch` belongs to; batches never span roots.
    pub(super) batch_root: Option<RootId>,
    /// The rescan request (see [`crate::Library::rescan_recommended`]) when the last pass
    /// began; cleared when the scan completes with every root reached.
    pub(super) rescan_request: Option<i64>,
    /// The last pass skipped an unreachable root, whose files were not re-read.
    pub(super) root_skipped: bool,
    /// This pass's immutable scope; pending requests belong to the next pass.
    pub(super) scope: ScanScope,
}
