use super::*;

impl Library {
    pub fn describe_cover_recovery(&self, track: TrackId) -> Result<CoverRecoveryGuard> {
        self.inner
            .pool
            .read(|conn| recovery_guard(self.inner.storage.files.as_ref(), conn, track))
    }
    pub fn describe_cover_recovery_resolution(
        &self,
        track: TrackId,
        resolution: CoverRecoveryResolution,
    ) -> Result<CoverRecoveryGuard> {
        self.inner.pool.read(|conn| {
            recovery_guard_for(
                self.inner.storage.files.as_ref(),
                conn,
                track,
                resolution,
                None,
            )
        })
    }
    pub fn describe_cover_recovery_selected(
        &self,
        track: TrackId,
        resolution: CoverRecoveryResolution,
        selection: Option<&CoverRecoverySelection>,
    ) -> Result<CoverRecoveryGuard> {
        self.inner.pool.read(|conn| {
            recovery_guard_for(
                self.inner.storage.files.as_ref(),
                conn,
                track,
                resolution,
                selection,
            )
        })
    }
    /// Execute a confirmed UI recovery decision. Restores publish only into a missing
    /// name; acceptance retains candidate registrations for separately confirmed cleanup.
    /// The audit and registration changes share one durable transaction.
    pub fn accept_cover_recovery(
        &self,
        guard: &CoverRecoveryGuard,
        meta: ActionMeta<'_>,
    ) -> Result<ActionId> {
        self.inner
            .register_file_job()?
            .run(|| self.accept_cover_recovery_inner(guard, meta))
    }

    fn accept_cover_recovery_inner(
        &self,
        guard: &CoverRecoveryGuard,
        meta: ActionMeta<'_>,
    ) -> Result<ActionId> {
        const UPDATE_FILE_WRITE_INTENTS: &str = r#"
UPDATE file_write_intents SET stage='retained',before_image=zeroblob(0),target_json=?2
WHERE id=?1
"#;

        const UPDATE_TRACKS: &str = r#"
UPDATE tracks SET mtime_ms=-1,cover_revision=NULL,metadata_revision=metadata_revision+1
WHERE id=?1
"#;

        self.require_cover_write()?;
        if meta.origin != "ui" {
            return Err(LibraryError::invalid(
                crate::Invalid::OutOfRange,
                "cover recovery resolution is UI-only",
            ));
        }
        let _serial = self
            .inner
            .tag_jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _flag = RecoveryFlag(&self.inner);
        let refs = tag_job::file_refs(&self.inner, &[guard.track])?;
        let file = &refs[0];
        let _lease = self
            .inner
            .file_access
            .acquire(file.track.root_id)
            .ok_or(LibraryError::RootReleased(file.track.root_id))?;
        let guard = guard.clone();
        let meta = meta.owned();
        let file = file.clone();
        let inner = self.inner.clone();
        self.inner.writer.run_data(move |conn| {
            let tx = conn.transaction()?;
            tag_job::intents::require_readable(&tx)?;
            let current = recovery_guard_for(
                inner.storage.files.as_ref(),
                &tx,
                guard.track,
                guard.resolution,
                guard.selected.as_ref(),
            )?;

            if current != guard {
                return Err(LibraryError::CoverRecoveryChanged);
            }
            let mut targets: Vec<Target> = guard
                .intents
                .iter()
                .map(|s| parse(s))
                .collect::<Result<_>>()?;

            match guard.resolution {
                CoverRecoveryResolution::AcceptCurrent
                | CoverRecoveryResolution::CleanupCandidates => {
                    if digest_at(inner.storage.files.as_ref(), &guard.path).is_none() {
                        return Err(LibraryError::CoverRecoveryChanged);
                    }
                }
                CoverRecoveryResolution::RestoreBefore | CoverRecoveryResolution::RestoreAfter => {
                    if !std::fs::symlink_metadata(&guard.path)
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                    {
                        return Err(LibraryError::CoverRecoveryChanged);
                    }
                    let choice = guard
                        .selected
                        .as_ref()
                        .ok_or(LibraryError::CoverRecoveryChanged)?;
                    let index = guard
                        .ids
                        .iter()
                        .position(|id| *id == choice.intent)
                        .ok_or(LibraryError::CoverRecoveryChanged)?;

                    let target = &targets[index];
                    let c = match choice.role {
                        CoverCandidateRole::Backup => &target.backup,
                        CoverCandidateRole::Staged => &target.staged,
                        CoverCandidateRole::RollbackAfter => &target.rollback_after,
                    };

                    let c = c
                        .as_ref()
                        .filter(|c| {
                            candidate_matches(inner.storage.files.as_ref(), c, &choice.sha256)
                        })
                        .ok_or(LibraryError::CoverRecoveryChanged)?;

                    let selected = c.path.clone();
                    let transaction = inner
                        .storage
                        .replacements
                        .resume(replacement_paths(target))?;
                    transaction
                        .publish_missing(&selected, &guard.path)
                        .map_err(|_| LibraryError::CoverRecoveryRequired)?;

                    if let Some(hidden) = target.baseline.as_ref().and_then(|b| b.hidden) {
                        transaction.set_hidden(&guard.path, hidden)?;
                    }
                }
            }
            for (id, t) in guard.ids.iter().zip(&mut targets) {
                if guard
                    .selected
                    .as_ref()
                    .is_some_and(|selection| selection.intent != *id)
                {
                    continue;
                }
                let transaction = inner.storage.replacements.resume(replacement_paths(t))?;
                for c in [&mut t.backup, &mut t.staged, &mut t.rollback_after]
                    .into_iter()
                    .flatten()
                {
                    if guard.resolution == CoverRecoveryResolution::CleanupCandidates
                        || guard.files.iter().any(|file| {
                            file.will_delete && file.location == c.path.display().to_string()
                        })
                    {
                        // A building candidate has no historical digest. The explicit
                        // UI confirmation binds its CURRENT full contents instead.
                        let exists = match std::fs::symlink_metadata(&c.path) {
                            Ok(_) => true,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                            Err(_) => return Err(LibraryError::CoverRecoveryRequired),
                        };

                        if exists {
                            // The guard was revalidated above, including the actual identity
                            // of a registered building-stage file with a placeholder identity.

                            if c.identity == [0, 0] {
                                c.identity = guard
                                    .candidate_identities
                                    .iter()
                                    .find(|(path, _)| *path == c.path)
                                    .map(|(_, id)| *id)
                                    .ok_or(LibraryError::CoverRecoveryChanged)?;
                            }

                            let expected = guard
                                .files
                                .iter()
                                .find(|file| {
                                    file.intent == *id
                                        && file.location == c.path.display().to_string()
                                })
                                .and_then(|file| file.sha256.as_deref())
                                .ok_or(LibraryError::CoverRecoveryChanged)?;

                            if !candidate_matches(inner.storage.files.as_ref(), c, expected) {
                                return Err(LibraryError::CoverRecoveryChanged);
                            }

                            std::fs::remove_file(&c.path)?;
                            transaction.sync(&guard.path)?;
                        }
                    } else {
                        // Snapshot retained copies so a later cleanup cannot erase an
                        // external edit, even if its inode and length are unchanged.
                        c.retained_digest = digest_at(inner.storage.files.as_ref(), &c.path);

                        if let Some(id) = identity(inner.storage.files.as_ref(), &c.path) {
                            c.identity = id;
                        }
                    }
                }
                let absent = candidates_absent(t)?;

                if guard.resolution == CoverRecoveryResolution::CleanupCandidates && !absent {
                    return Err(LibraryError::CoverRecoveryRequired);
                }

                if absent {
                    tx.execute("DELETE FROM file_write_intents WHERE id=?1", [id])?;
                } else {
                    tx.execute(UPDATE_FILE_WRITE_INTENTS, params![id, json(t)?])?;
                }
            }
            let record = std::fs::metadata(&guard.path).ok().and_then(|m| {
                tag_job::reread_record(&inner, &file, &guard.path, tag_job::stamp(&m))
            });

            if let Some(record) = record {
                crate::db::writer::upsert_track(
                    inner.storage.files.as_ref(),
                    &tx,
                    &record,
                    now_ms(),
                )?;
            } else {
                tx.execute(UPDATE_TRACKS, [guard.track.0])?;
            }
            let resolution = json(&guard.resolution)?.trim_matches('"').to_owned();
            let action = journal::record_audit(
                &tx,
                meta.meta(),
                "cover_recovery_resolve",
                format!("{resolution} cover recovery for track {}", guard.track.0),
                1,
                now_ms(),
            )?;

            let details = serde_json::json!({
            "v":1,
            "retained_recovery":{
            "path":guard.path,
            "fingerprint":guard.fingerprint,
            "selected":guard.selected,
            "intents":guard.intents}
            ,
            "details":{
            "type":"cover_recovery_resolve",
            "track":guard.track.0,
            "resolution":resolution,
            "intent_count":
            if guard.selected.is_some(){
            1}
            else{
            guard.ids.len()}
            }
            }
            );

            tx.execute(
                "UPDATE actions SET details_json=?2 WHERE id=?1",
                params![action.0, details.to_string()],
            )?;

            crate::db::schema::refresh_aggregates(&tx)?;
            tx.commit()?;
            Ok(action)
        })
    }
    /// Abandoned or conflicting intents at the last completed writer boundary.
    pub fn has_cover_recovery_required(&self) -> bool {
        self.inner
            .cover_recovery
            .load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn take_recovered_covers(&self) -> Vec<TrackId> {
        let mut ids = std::mem::take(
            &mut *self
                .inner
                .recovered_covers
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        ids.sort_unstable();
        ids.dedup();
        ids
    }
    /// Private host confirmation locations; never projected to an untrusted theme.
    pub fn cover_edit_locations(&self, ids: &[TrackId]) -> Result<Vec<(TrackId, String)>> {
        let mut locations = Vec::new();
        for file in tag_job::file_refs(&self.inner, ids)? {
            if let Located::File { path, .. } =
                tag_job::locate(self.inner.storage.files.as_ref(), &file, None)
            {
                locations.push((file.track.id, path.display().to_string()));
            }
        }
        Ok(locations)
    }
    /// The picture that would be displayed after removing editable embedded covers.
    pub fn cover_art_after_removal(&self, id: TrackId) -> Result<Option<crate::CoverArt>> {
        let files = tag_job::file_refs(&self.inner, &[id])?;
        let Some(file) = files.first() else {
            return Ok(None);
        };
        let Some(_lease) = self.inner.file_access.acquire(file.track.root_id) else {
            return Err(LibraryError::RootReleased(file.track.root_id));
        };
        let Located::File { path, .. } =
            tag_job::locate(self.inner.storage.files.as_ref(), file, None)
        else {
            return Ok(None);
        };
        let mut source =
            storage_access::open_regular(self.inner.storage.files.as_ref(), &path, true)?;
        let plan = cover_edit::prepare(&mut source, &CoverChange::Remove)
            .map_err(LibraryError::CoverInput)?;
        Ok(
            crate::cover::fallback_cover(&path, plan.scope == "all_covr")
                .ok()
                .flatten(),
        )
    }
    pub fn cover_sources(&self, ids: &[TrackId]) -> Result<Vec<CoverSource>> {
        let mut sources = Vec::new();
        for file in tag_job::file_refs(&self.inner, ids)? {
            if let Located::File { path, .. } =
                tag_job::locate(self.inner.storage.files.as_ref(), &file, None)
            {
                let _lease = self
                    .inner
                    .file_access
                    .acquire(file.track.root_id)
                    .ok_or(LibraryError::RootReleased(file.track.root_id))?;
                let mut opened =
                    storage_access::open_regular(self.inner.storage.files.as_ref(), &path, true)?;
                sources.push(CoverSource {
                    track: file.track.id,
                    path,
                    digest: cover_edit::file_digest(&mut opened)?,
                    identity: self.inner.storage.files.as_ref().opened_identity(&opened)?,
                });
            }
        }
        Ok(sources)
    }

    pub fn cover_recovery_required(&self) -> Result<Vec<TrackId>> {
        self.inner.pool.read(|conn| {
            let rows: Vec<String> = conn
                .prepare("SELECT target_json FROM file_write_intents WHERE family='cover_edit'")?
                .query_map([], |r| r.get(0))?
                .filter_map(std::result::Result::ok)
                .collect();
            let mut seen_paths = HashSet::new();
            let mut seen_ids = HashSet::new();
            let mut tracks = Vec::new();
            for row in rows {
                let Ok(target) = parse::<Target>(&row) else {
                    continue;
                };
                let path = storage_access::comparable_with(
                    self.inner.storage.files.as_ref(),
                    &target.path,
                );
                let id = identity(self.inner.storage.files.as_ref(), &target.path);
                let new_path = seen_paths.insert(path);
                let new_id = id.is_none_or(|id| seen_ids.insert(id));
                if new_path && new_id {
                    tracks.push(TrackId(target.track));
                }
            }
            Ok(tracks)
        })
    }
    pub fn cover_edit_tracks(&self, action: ActionId) -> Result<Vec<TrackId>> {
        const SELECT_ACTION_TARGETS: &str = r#"
SELECT object_id FROM action_targets WHERE action_id=?1 AND kind='track_cover'
ORDER BY object_id
"#;

        self.inner.pool.read(|conn| {
            use rusqlite::OptionalExtension;
            let mut ids = conn
                .prepare(SELECT_ACTION_TARGETS)?
                .query_map([action.0], |r| Ok(TrackId(r.get(0)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let inverse: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT inverse FROM actions WHERE id=?1 AND undone_by IS NULL",
                    [action.0],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            if let Some(journal::Inverse::Covers(files)) =
                inverse.as_deref().and_then(journal::Inverse::decode)
            {
                let pending: HashSet<_> = files.iter().map(|file| file.track).collect();
                ids.retain(|id| pending.contains(&id.0));
            }
            Ok(ids)
        })
    }
    pub fn preview_cover_edit(
        &self,
        ids: &[TrackId],
        change: &CoverChange,
    ) -> Result<CoverPreview> {
        self.require_cover_write()?;
        self.inner.pool.read(tag_job::intents::require_readable)?;
        check(ids, change)?;
        let refs = tag_job::file_refs(&self.inner, ids)?;
        let mut preview = CoverPreview::default();
        let used = self.inner.pool.read(|conn| {
            Ok(conn.query_row(
                "SELECT coalesce(sum(length(before_image)),0) FROM file_write_intents WHERE family='cover_edit'",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })?;
        let mut left = MAX_COVER_UNDO_BYTES.saturating_sub(used.max(0) as usize);
        for file in refs {
            let id = file.track.id;
            let result = (|| {
                if pending(&self.inner, id).map_err(|_| "cover_recovery_required")? {
                    return Err("cover_recovery_required");
                }
                let _lease = self
                    .inner
                    .file_access
                    .acquire(file.track.root_id)
                    .ok_or("root_released")?;
                let path = match tag_job::locate(self.inner.storage.files.as_ref(), &file, None) {
                    Located::File { path, stamp }
                        if stamp == (file.track.size, file.track.mtime_ms) =>
                    {
                        path
                    }
                    Located::File { .. } => return Err("file_changed"),
                    Located::Skip(c) | Located::Fail(c) => return Err(c),
                };
                let mut source =
                    storage_access::open_regular(self.inner.storage.files.as_ref(), &path, true)
                        .map_err(|_| "open_failed")?;
                if source
                    .metadata()
                    .map_err(|_| "open_failed")?
                    .permissions()
                    .readonly()
                {
                    return Err("file_read_only");
                }
                let plan = cover_edit::prepare(&mut source, change)?;
                let bytes = plan.undo_bytes(&mut source)?;
                if bytes > left {
                    return Err("undo_too_large");
                }
                left -= bytes;
                let fallback_art = crate::cover::fallback_cover(&path, plan.scope == "all_covr")
                    .ok()
                    .flatten();
                let fallback = plan.fallback || fallback_art.is_some();
                let after = change.image().or_else(|| {
                    fallback_art.and_then(|a| cover_edit::image_info(&a.mime, &a.data).ok())
                });
                let before = crate::cover::read_cover(&path)
                    .ok()
                    .flatten()
                    .and_then(|a| cover_edit::image_info(&a.mime, &a.data).ok())
                    .or(plan.before);
                Ok(CoverFilePreview {
                    track: id,
                    before,
                    after,
                    changed: !plan.regions.is_empty(),
                    undo_bytes: bytes as u64,
                    fallback,
                    scope: plan.scope.into(),
                })
            })();
            match result {
                Ok(f) => preview.files.push(f),
                Err(code) => preview.skipped.push((id, code)),
            }
        }
        Ok(preview)
    }
    /// Private host thumbnail input for undo. The temporary result is never published.
    pub fn cover_art_after_undo(
        &self,
        action: ActionId,
        track: TrackId,
    ) -> Result<Option<crate::CoverArt>> {
        let undo = self
            .inner
            .pool
            .read(|conn| journal::cover_undo(conn, action, now_ms()))?
            .ok_or_else(|| LibraryError::not_found(crate::NotFound::Action, "not a cover undo"))?;
        let undo = undo
            .iter()
            .find(|f| f.track == track.0)
            .ok_or(LibraryError::CoverRecoveryChanged)?;
        let files = tag_job::file_refs(&self.inner, &[track])?;
        let file = files.first().ok_or(LibraryError::CoverRecoveryChanged)?;
        let _lease = self
            .inner
            .file_access
            .acquire(file.track.root_id)
            .ok_or(LibraryError::RootReleased(file.track.root_id))?;
        let Located::File { path, .. } =
            tag_job::locate(self.inner.storage.files.as_ref(), file, None)
        else {
            return Err(LibraryError::CoverRecoveryChanged);
        };
        let mut source =
            storage_access::open_regular(self.inner.storage.files.as_ref(), &path, true)?;
        // This optional thumbnail must not copy arbitrarily large audio files.
        if source.metadata()?.len() > 128 * 1024 * 1024 {
            return Ok(None);
        }
        if cover_edit::file_digest(&mut source)? != undo.expected {
            return Err(LibraryError::CoverRecoveryChanged);
        }
        let mut temp = tempfile::NamedTempFile::new_in(
            path.parent().ok_or(LibraryError::CoverRecoveryChanged)?,
        )?;
        cover_edit::stage(&mut source, temp.as_file_mut(), &undo.regions)
            .map_err(LibraryError::CoverInput)?;
        Ok(crate::cover::embedded_cover(temp.path())
            .ok()
            .flatten()
            .or_else(|| {
                path.parent()
                    .and_then(|dir| crate::cover::folder_cover(dir).ok().flatten())
            }))
    }
    pub fn preview_cover_undo(&self, action: ActionId) -> Result<CoverPreview> {
        self.require_cover_write()?;
        self.inner.pool.read(tag_job::intents::require_readable)?;
        let undo = self
            .inner
            .pool
            .read(|conn| journal::cover_undo(conn, action, now_ms()))?
            .ok_or_else(|| LibraryError::not_found(crate::NotFound::Action, "not a cover undo"))?;
        let ids: Vec<_> = undo.iter().map(|f| TrackId(f.track)).collect();
        let refs = tag_job::file_refs(&self.inner, &ids)?;
        let mut preview = CoverPreview::default();
        for file in refs {
            let id = file.track.id;
            let result = (|| -> std::result::Result<CoverFilePreview, &'static str> {
                if pending(&self.inner, id).map_err(|_| "cover_recovery_required")? {
                    return Err("cover_recovery_required");
                }
                let _lease = self
                    .inner
                    .file_access
                    .acquire(file.track.root_id)
                    .ok_or("root_released")?;
                let path = match tag_job::locate(self.inner.storage.files.as_ref(), &file, None) {
                    Located::File { path, .. } => path,
                    Located::Skip(c) | Located::Fail(c) => return Err(c),
                };
                let mut source =
                    storage_access::open_regular(self.inner.storage.files.as_ref(), &path, true)
                        .map_err(|_| "open_failed")?;
                if source
                    .metadata()
                    .map_err(|_| "open_failed")?
                    .permissions()
                    .readonly()
                {
                    return Err("file_read_only");
                }
                let undo = undo.iter().find(|f| f.track == id.0).unwrap();
                if cover_edit::file_digest(&mut source).map_err(|_| "open_failed")? != undo.expected
                {
                    return Err("file_changed");
                }
                let mut temp = tempfile::NamedTempFile::new().map_err(|_| "write_failed")?;
                cover_edit::stage(&mut source, temp.as_file_mut(), &undo.regions)?;
                let before = crate::cover::read_cover(&path)
                    .ok()
                    .flatten()
                    .and_then(|a| cover_edit::image_info(&a.mime, &a.data).ok());
                let embedded = crate::cover::embedded_cover(temp.path()).ok().flatten();
                let fallback = embedded.is_none();
                let after = embedded
                    .or_else(|| {
                        path.parent()
                            .and_then(|p| crate::cover::folder_cover(p).ok().flatten())
                    })
                    .and_then(|a| cover_edit::image_info(&a.mime, &a.data).ok());
                let scope = if file
                    .track
                    .codec
                    .as_deref()
                    .is_some_and(|c| matches!(c.to_lowercase().as_str(), "aac" | "alac"))
                {
                    "all_covr"
                } else {
                    "front_cover"
                };
                Ok(CoverFilePreview {
                    track: id,
                    before,
                    after,
                    changed: true,
                    undo_bytes: undo
                        .regions
                        .iter()
                        .map(|r| r.bytes.len().max(r.replaced as usize) as u64 + 32)
                        .sum::<u64>()
                        + 256,
                    fallback,
                    scope: scope.into(),
                })
            })();
            match result {
                Ok(file) => preview.files.push(file),
                Err(code) => preview.skipped.push((id, code)),
            }
        }
        Ok(preview)
    }
    pub fn edit_cover(
        &self,
        ids: &[TrackId],
        change: &CoverChange,
        meta: ActionMeta<'_>,
        options: TagJobOptions,
    ) -> Result<CoverJob> {
        let job = self.inner.register_file_job()?;
        self.require_cover_write()?;
        check(ids, change)?;
        let ids = ids.to_vec();
        let change = change.clone();
        let meta = meta.owned();
        let inner = Arc::clone(&self.inner);
        Ok(CoverJob {
            thread: std::thread::Builder::new()
                .name("cover-edit".into())
                .spawn(move || {
                    job.run(|| run(&inner, &ids, Some(&change), None, &meta, options))
                })?,
        })
    }
    pub fn undo_cover_edit(
        &self,
        action: ActionId,
        meta: ActionMeta<'_>,
        options: TagJobOptions,
    ) -> Result<Option<CoverJob>> {
        let job = self.inner.register_file_job()?;
        if self.cover_edit_tracks(action)?.is_empty() {
            return Ok(None);
        }
        self.require_cover_write()?;
        let meta = meta.owned();
        let inner = Arc::clone(&self.inner);
        Ok(Some(CoverJob {
            thread: std::thread::Builder::new()
                .name("cover-undo".into())
                .spawn(move || job.run(|| run(&inner, &[], None, Some(action), &meta, options)))?,
        }))
    }
}
