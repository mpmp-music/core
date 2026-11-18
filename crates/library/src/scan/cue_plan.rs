use super::*;

impl Scanner<'_> {
    /// Include reverse sidecar dependencies even if the failed first import wrote no
    /// CUE rows. Persisted rows supply the same evidence after reopening the library.
    pub(super) fn retry_reverse_cues(
        &mut self,
        root: &Root,
        files: &[Found],
        sheets: &mut Vec<Found>,
    ) -> Result<()> {
        let selected = |path: &Path| {
            self.scope
                .directories
                .as_ref()
                .is_some_and(|dirs| dirs.iter().any(|dir| path.starts_with(dir)))
                || files
                    .iter()
                    .any(|file| root.path.join(&file.rel_path) == path)
        };
        let mut required = std::collections::BTreeSet::new();
        {
            let dependencies = self
                .inner
                .cue_dependencies
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for (_, owners) in dependencies
                .iter()
                .filter(|(audio, _)| audio.starts_with(&root.path) && selected(audio))
            {
                required.extend(owners.iter().cloned());
            }
        }
        let known = self
            .inner
            .pool
            .read(|conn| reads::root_files(conn, root.id))?;
        for ((sheet, _), cue) in &known.cues {
            if *sheet != cue.rel_path && selected(&root.path.join(&cue.rel_path)) {
                required.insert(root.path.join(sheet));
            }
        }
        let mut present: HashSet<_> = sheets
            .iter()
            .map(|sheet| root.path.join(&sheet.rel_path))
            .collect();
        for path in required {
            self.check_root(root.id)?;
            if !present.insert(path.clone()) {
                continue;
            }
            self.diagnostics.coverage.insert(path.clone());
            let Some(rel_path) = relative_path(&root.path, &path) else {
                continue;
            };
            match std::fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => sheets.push(Found {
                    file_identity: self
                        .inner
                        .storage
                        .files
                        .entry_identity(&path, &metadata)
                        .ok()
                        .map(|i| i.parts()),
                    rel_path,
                    size: metadata.len(),
                    mtime_ms: metadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_millis() as i64),
                    folder_art: false,
                }),
                _ => self.error(
                    &path,
                    ScanErrorCode::CueReadFailed,
                    "cannot read CUE dependency",
                ),
            }
        }
        Ok(())
    }

    /// A retried CUE may name audio in a sibling directory. Resolve only its direct
    /// dependencies, using the ordinary case/stem fallback, without walking that tree.
    pub(super) fn retry_cue_dependencies(
        &mut self,
        root: &Root,
        sheets: &[Found],
        files: &mut Vec<Found>,
        art_dirs: &mut HashSet<String>,
    ) -> Result<()> {
        let forced = root.forced_encoding.as_deref().and_then(encoding_for_label);
        let mut present: HashSet<String> = files.iter().map(|f| f.rel_path.clone()).collect();
        for sheet in sheets {
            self.check_root(root.id)?;
            let Ok(bytes) = read_limited(&root.path.join(&sheet.rel_path)) else {
                continue;
            };
            let (text, _) = decode_sheet(&bytes, forced);
            let Ok(parsed) = panic::catch_unwind(AssertUnwindSafe(|| Sheet::parse(&text))) else {
                continue;
            };
            let dir = parent_dir(&sheet.rel_path);
            for name in &parsed.files {
                self.check_root(root.id)?;
                let Some(rel) = cue_relative_path(dir, name) else {
                    continue;
                };
                if present.contains(&rel) {
                    continue;
                }
                let parent = parent_dir(&rel);
                let Ok(entries) = std::fs::read_dir(root.path.join(parent)) else {
                    continue;
                };
                let mut candidates = Vec::new();
                for entry in entries.flatten() {
                    self.check_root(root.id)?;
                    let path = entry.path();
                    if path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| cover_rank(n).is_some())
                    {
                        art_dirs.insert(parent.to_owned());
                    }
                    if is_audio(&path)
                        && let Some(rel_path) = relative_path(&root.path, &path)
                    {
                        candidates.push(Found {
                            file_identity: None,
                            rel_path,
                            size: 0,
                            mtime_ms: 0,
                            folder_art: false,
                        });
                    }
                }
                candidates.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
                let Resolved::Audio(target) =
                    AudioIndex::new(&candidates).resolve(&root.path, dir, name)
                else {
                    continue;
                };
                if !present.insert(target.clone()) {
                    continue;
                }
                let path = root.path.join(&target);
                self.diagnostics.coverage.insert(path.clone());
                let metadata = match std::fs::metadata(&path) {
                    Ok(metadata) if metadata.is_file() => metadata,
                    Ok(_) => continue,
                    Err(error) => {
                        self.error(
                            &path,
                            ScanErrorCode::StatFailed,
                            format_args!("cannot read file metadata: {error}"),
                        );
                        continue;
                    }
                };
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    self.audio_paths.entry((root.id, target.clone()))
                {
                    entry.insert(false);
                    self.progress.total_estimate += 1;
                }
                files.push(Found {
                    file_identity: self
                        .inner
                        .storage
                        .files
                        .entry_identity(&path, &metadata)
                        .ok()
                        .map(|i| i.parts()),
                    rel_path: target,
                    size: metadata.len(),
                    mtime_ms: metadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_millis() as i64),
                    folder_art: false,
                });
            }
        }
        Ok(())
    }

    /// Queues the row of one track cut from `file` by `sheet`.
    pub(super) fn push_cut(
        &mut self,
        root: &Root,
        file: &Found,
        sheet: &Found,
        cut: &Cut,
        base: &TrackMeta,
        encoding: Option<&'static Encoding>,
    ) {
        self.batch.upserts.push(TrackRecord {
            root_id: root.id,
            rel_path: file.rel_path.clone(),
            size: file.size,
            mtime_ms: file.mtime_ms,
            has_folder_art: file.folder_art,
            meta: cue_meta(base, cut, encoding),
            cue: Some(CueRecord {
                sheet: sheet.rel_path.clone(),
                number: cut.number,
                start: cut.start,
                end: cut.end,
                size: sheet.size,
                mtime_ms: sheet.mtime_ms,
            }),
            // Tags of sheet tracks are not edited (`cue_track`): no row to guard.
            based_on: None,
            based_on_revision: None,
            artists_only: false,
            file_identity: file.file_identity,
        });
    }

    pub(super) fn guard_cut(&mut self, old: Option<&KnownCue>, file: &Found, sheet: &Found) {
        if let Some(record) = self.batch.upserts.last_mut() {
            record.based_on_revision = old.map(|k| k.revision);
            record.based_on = old.map(|k| (k.size, k.mtime_ms));
            record.artists_only = old.is_some_and(|k| {
                k.rel_path == file.rel_path
                    && (k.size, k.mtime_ms) == (file.size, file.mtime_ms)
                    && (k.cue_size, k.cue_mtime_ms) == (sheet.size, sheet.mtime_ms)
            });
        }
    }

    /// Reads and parses every `.cue` file of the root and decides which audio file each one
    /// cuts. A file is cut by at most one sheet: the one with the file's stem if any, else
    /// the first by path; and only by a sheet that has tracks in it.
    pub(super) fn plan_sheets(
        &mut self,
        root: &Root,
        sheets: &[Found],
        files: &[Found],
        forced: Option<&'static Encoding>,
        known: &KnownRows,
    ) -> Result<SheetPlan> {
        let index = AudioIndex::new(files);
        let mut plan = SheetPlan::default();
        // (preference, sheet, file index) per audio file.
        let mut claims: HashMap<String, (bool, usize)> = HashMap::new();
        for found in sheets {
            self.check_root(root.id)?;
            let path = root.path.join(&found.rel_path);
            let bytes = match read_limited(&path) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::warn!(%err, path = %path.display(), "cannot read CUE sheet");
                    self.error(
                        &path,
                        ScanErrorCode::CueReadFailed,
                        format_args!("cannot read CUE sheet: {err}"),
                    );
                    // Its tracks and files are left as they are.
                    for ((sheet, _), cue) in &known.cues {
                        if cue.available && *sheet == found.rel_path {
                            plan.kept.push(cue.id);
                            plan.claimed.insert(cue.rel_path.clone());
                        }
                    }
                    continue;
                }
            };
            let (text, encoding) = decode_sheet(&bytes, forced);
            let parsed = panic::catch_unwind(AssertUnwindSafe(|| {
                let sheet = Sheet::parse(&text);
                let cuts = sheet.cut(false);
                (sheet, cuts)
            }));
            let (sheet, cuts) = match parsed {
                Ok(parsed) => parsed,
                Err(_) => {
                    self.error(&path, ScanErrorCode::ReaderPanicked, "CUE reader panicked");
                    for ((sheet, _), cue) in &known.cues {
                        if cue.available && *sheet == found.rel_path {
                            plan.kept.push(cue.id);
                            plan.claimed.insert(cue.rel_path.clone());
                        }
                    }
                    continue;
                }
            };
            let dir = parent_dir(&found.rel_path);
            let mut targets = Vec::with_capacity(sheet.files.len());
            let mut missing = Vec::new();
            for name in &sheet.files {
                self.check_root(root.id)?;
                targets.push(match index.resolve(&root.path, dir, name) {
                    Resolved::Audio(rel_path) => Some(rel_path),
                    Resolved::Other => None,
                    Resolved::Missing => {
                        missing.push(name.as_str());
                        None
                    }
                });
            }
            if !missing.is_empty() {
                let message = missing_diagnostic(&missing);
                tracing::warn!(path = %path.display(), %message, "CUE sheet names missing files");
                self.error(&path, ScanErrorCode::CueMissingFiles, message);
            }
            // A full-file singleton is an independent song, not a shared-file cut.
            let mut singletons = HashSet::new();
            for file in 0..sheet.files.len() {
                if let Some(tags) = sheet.whole_file_tags(file)
                    && let Some(target) = targets.get(file).and_then(Option::as_ref)
                    && sheet
                        .tracks
                        .iter()
                        .filter(|track| {
                            track.audio
                                && targets.get(track.file).and_then(Option::as_ref) == Some(target)
                        })
                        .count()
                        == 1
                {
                    singletons.insert(file);
                    plan.supplements.entry(target.clone()).or_insert(tags);
                    if let Some(track) = sheet.whole_file_track(file)
                        && track.index1 == Some((file, 0))
                    {
                        plan.singleton_evidence.entry(target.clone()).or_insert((
                            found.rel_path.clone(),
                            track.number,
                            sheet.appended_gap_end(file),
                        ));
                    }
                }
            }
            let cuts: Vec<Cut> = cuts
                .into_iter()
                .filter(|cut| !singletons.contains(&cut.file))
                .filter(|cut| targets.get(cut.file).is_some_and(Option::is_some))
                .collect();
            let n = plan.sheets.len();
            let stem = file_stem(&found.rel_path).to_lowercase();
            for target in targets.iter().flatten() {
                if !cuts
                    .iter()
                    .any(|cut| targets[cut.file].as_ref() == Some(target))
                {
                    continue;
                }
                let preferred = file_stem(target).to_lowercase() == stem;
                match claims.get(target) {
                    Some(&(was_preferred, _)) if was_preferred || !preferred => {}
                    _ => {
                        claims.insert(target.clone(), (preferred, n));
                    }
                }
            }
            plan.sheets.push(PlannedSheet {
                found: found.clone(),
                encoding,
                cuts,
                targets,
            });
        }
        {
            let mut dependencies = self
                .inner
                .cue_dependencies
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for sheet in &plan.sheets {
                let path = root.path.join(&sheet.found.rel_path);
                dependencies.retain(|_, owners| {
                    owners.remove(&path);
                    !owners.is_empty()
                });
            }
            for (target, (_, owner)) in &claims {
                dependencies
                    .entry(root.path.join(target))
                    .or_default()
                    .insert(root.path.join(&plan.sheets[*owner].found.rel_path));
            }
        }
        for (target, (_, sheet)) in claims {
            plan.supplements.remove(&target);
            plan.claimed.insert(target.clone());
            plan.owner.insert(target, sheet);
        }
        // A missing or changed supplement requires a reread. Only the writer can confirm
        // that it has been applied, even when the persisted metadata already matches.
        {
            use sha2::{Digest, Sha256};
            let mut cache = self
                .inner
                .cue_supplements
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !self.scope.directory_only(root) {
                let present: HashSet<_> = files.iter().map(|file| file.rel_path.as_str()).collect();
                cache.retain(|(id, path), _| *id != root.id || present.contains(path.as_str()));
            }
            for file in files {
                let old = cache.get(&(root.id, file.rel_path.clone()));
                let fingerprint = plan.supplements.get(&file.rel_path).map(|tags| {
                    let display = ["TITLE", "ARTIST"]
                        .map(|key| tags.iter().find(|(k, _)| k == key).map(|(_, value)| value));
                    Sha256::digest(serde_json::to_vec(&display).expect("CUE text serializes"))
                        .into()
                });
                if fingerprint.is_some() || old.is_some() {
                    if !old.is_some_and(|previous| {
                        previous.fingerprint == fingerprint
                            && previous.confirmed.is_some_and(|confirmed| {
                                known
                                    .files
                                    .get(&file.rel_path)
                                    .map(|row| (row.id, row.revision))
                                    == Some(confirmed)
                            })
                    }) {
                        plan.changed_supplements.insert(file.rel_path.clone());
                    }
                    plan.supplement_fingerprints
                        .insert(file.rel_path.clone(), fingerprint);
                }
            }
        }
        Ok(plan)
    }

    /// The tracks of the sheets next to their audio files: unchanged ones are kept, the
    /// others (re)built from their sheet and file.
    pub(super) fn scan_sheet_tracks(
        &mut self,
        root: &Root,
        plan: &SheetPlan,
        files: &[Found],
        forced: Option<&'static Encoding>,
        known: &KnownRows,
        seen: &mut HashSet<TrackId>,
    ) -> Result<()> {
        seen.extend(plan.kept.iter().copied());
        let by_path: HashMap<&str, &Found> =
            files.iter().map(|f| (f.rel_path.as_str(), f)).collect();
        // Each audio file is read at most once, and only when a track of it changed.
        let mut metas: HashMap<String, Option<TrackMeta>> = HashMap::new();
        for (n, sheet) in plan.sheets.iter().enumerate() {
            for cut in &sheet.cuts {
                self.check_root(root.id)?;
                let Some(target) = &sheet.targets[cut.file] else {
                    continue;
                };
                if plan.owner.get(target) != Some(&n) {
                    continue;
                }
                let Some(&file) = by_path.get(target.as_str()) else {
                    continue;
                };
                let key = (sheet.found.rel_path.clone(), cut.number);
                let old = known.cues.get(&key);
                if let Some(old) = old {
                    seen.insert(old.id);
                    if old.artists_complete
                        && old.rel_path == file.rel_path
                        && (old.size, old.mtime_ms) == (file.size, file.mtime_ms)
                        && (old.cue_size, old.cue_mtime_ms)
                            == (sheet.found.size, sheet.found.mtime_ms)
                    {
                        self.keep_unchanged(old.id, old.available, old.has_folder_art, file);
                        self.flush_if_full()?;
                        continue;
                    }
                }
                let people_only = old.is_some_and(|old| {
                    old.rel_path == file.rel_path
                        && (old.size, old.mtime_ms) == (file.size, file.mtime_ms)
                        && (old.cue_size, old.cue_mtime_ms)
                            == (sheet.found.size, sheet.found.mtime_ms)
                });
                let meta = metas.entry(target.clone()).or_insert_with(|| {
                    let path = root.path.join(target);
                    match read_audio(self.inner, &path, forced) {
                        Ok(meta) => Some(meta),
                        Err(err) => {
                            tracing::warn!(%err, path = %path.display(), "cannot read audio file");
                            if !people_only {
                                self.error(
                                    &path,
                                    audio_error_code(&err),
                                    format_args!("cannot read audio file: {err}"),
                                );
                            }
                            None
                        }
                    }
                });
                // An unreadable file: existing rows keep their old data.
                let Some(meta) = meta.as_ref() else {
                    continue;
                };
                if !usable(cut, meta) {
                    continue;
                }
                let cut = bounded_cut(cut, meta);
                self.push_cut(root, file, &sheet.found, &cut, meta, sheet.encoding);
                self.guard_cut(old, file, &sheet.found);
                if people_only {
                    if let Some(old) = old {
                        self.keep_unchanged(old.id, old.available, old.has_folder_art, file);
                    }
                } else {
                    self.count_upsert(old.is_some());
                }
                self.flush_if_full()?;
                self.tick();
            }
        }
        Ok(())
    }
}

/// Reads a sheet file, at most [`MAX_SHEET_BYTES`] of it.
fn read_limited(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_SHEET_BYTES)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Whether a cut track is in its file as read: one that starts at or after the file's end
/// (a sheet for another rip) is left out.
fn usable(cut: &Cut, meta: &TrackMeta) -> bool {
    meta.duration_ms
        .is_none_or(|total| cd_frames_to_ms(cut.start) < total)
}

/// The final usable track reaches EOF, even when the sheet names more audio.
fn bounded_cut(cut: &Cut, meta: &TrackMeta) -> Cut {
    let mut cut = cut.clone();
    if cut
        .end
        .zip(meta.duration_ms)
        .is_some_and(|(end, total)| cd_frames_to_ms(end) >= total)
    {
        cut.end = None;
    }
    cut
}

/// The tracks a sheet embedded in a file (`meta`) cuts it into.
pub(super) fn usable_cuts(sheet: &Sheet, single_file: bool, meta: &TrackMeta) -> Vec<Cut> {
    sheet
        .cut(single_file)
        .into_iter()
        .filter(|cut| usable(cut, meta))
        .map(|cut| bounded_cut(&cut, meta))
        .collect()
}

/// The root's sheets, read, and which audio files they cut.
#[derive(Default)]
pub(super) struct SheetPlan {
    sheets: Vec<PlannedSheet>,
    /// Audio files a sheet cuts into tracks (also those of unreadable sheets): hidden.
    pub(super) claimed: HashSet<String>,
    pub(super) supplements: HashMap<String, Vec<(String, String)>>,
    pub(super) singleton_evidence: HashMap<String, (String, u32, Option<u64>)>,
    pub(super) changed_supplements: HashSet<String>,
    pub(super) supplement_fingerprints: HashMap<String, Option<[u8; 32]>>,
    /// The sheet (index into `sheets`) that cuts each audio file.
    owner: HashMap<String, usize>,
    /// Tracks of unreadable sheets: left as they are.
    pub(super) kept: Vec<TrackId>,
}

pub(super) struct PlannedSheet {
    found: Found,
    /// The legacy encoding its text was decoded from.
    encoding: Option<&'static Encoding>,
    /// Its tracks in files it names that resolved to audio files.
    cuts: Vec<Cut>,
    /// What each `FILE` resolved to (`rel_path` of an audio file of the root).
    targets: Vec<Option<String>>,
}

/// What a sheet's `FILE` name refers to.
#[derive(Debug, PartialEq)]
pub(super) enum Resolved {
    /// An audio file the library reads.
    Audio(String),
    /// A file it does not read (e.g. `.ape`): its tracks are left out, nothing is hidden.
    Other,
    Missing,
}

/// The walked audio files, for resolving `FILE` names.
pub(super) struct AudioIndex<'a> {
    exact: HashSet<&'a str>,
    /// Lower-case `rel_path` → `rel_path`.
    folded: HashMap<String, &'a str>,
    /// (lower-case folder, lower-case stem) → `rel_path`s, in [`AUDIO_EXTENSIONS`] order.
    stems: HashMap<(String, String), Vec<&'a str>>,
}

impl<'a> AudioIndex<'a> {
    pub(super) fn new(files: &'a [Found]) -> Self {
        let mut index = AudioIndex {
            exact: HashSet::with_capacity(files.len()),
            folded: HashMap::with_capacity(files.len()),
            stems: HashMap::new(),
        };
        for file in files {
            let rel = file.rel_path.as_str();
            index.exact.insert(rel);
            index.folded.entry(rel.to_lowercase()).or_insert(rel);
            let key = (
                parent_dir(rel).to_lowercase(),
                file_stem(rel).to_lowercase(),
            );
            index.stems.entry(key).or_default().push(rel);
        }
        let rank = |rel: &str| {
            let ext = rel
                .rsplit_once('.')
                .map_or("", |(_, e)| e)
                .to_ascii_lowercase();
            AUDIO_EXTENSIONS.iter().position(|e| *e == ext)
        };
        for list in index.stems.values_mut() {
            list.sort_by_key(|rel| (rank(rel), *rel));
        }
        index
    }

    /// Resolves `name` from a sheet in folder `dir` (both root-relative): the file as
    /// written, or with another letter case; else, when no file of that name exists, an
    /// audio file in the same folder with the same stem and another extension (a sheet
    /// written for `x.wav` next to `x.flac`). An absolute name is looked up by its file
    /// name in the sheet's folder.
    fn resolve(&self, root: &Path, dir: &str, name: &str) -> Resolved {
        let Some(rel) = cue_relative_path(dir, name) else {
            return Resolved::Missing;
        };
        if self.exact.contains(rel.as_str()) {
            return Resolved::Audio(rel);
        }
        if let Some(found) = self.folded.get(&rel.to_lowercase()) {
            return Resolved::Audio((*found).to_owned());
        }
        let path = root.join(&rel);
        if path.is_file() {
            return Resolved::Other;
        }
        let key = (
            parent_dir(&rel).to_lowercase(),
            file_stem(&rel).to_lowercase(),
        );
        match self.stems.get(&key).and_then(|list| list.first()) {
            Some(found) => Resolved::Audio((*found).to_owned()),
            None => {
                let compact = |s: &str| {
                    s.chars()
                        .filter(|c| !c.is_whitespace())
                        .flat_map(char::to_lowercase)
                        .collect::<String>()
                };
                let wanted = compact(file_stem(&rel));
                let mut matches = self.exact.iter().filter(|p| {
                    parent_dir(p).eq_ignore_ascii_case(parent_dir(&rel))
                        && compact(file_stem(p)) == wanted
                });
                match (matches.next(), matches.next()) {
                    (Some(found), None) => Resolved::Audio((*found).to_owned()),
                    _ => Resolved::Missing,
                }
            }
        }
    }
}

/// Root-relative CUE target; reject traversal above the registered root.
fn cue_relative_path<'a>(dir: &'a str, name: &'a str) -> Option<String> {
    let absolute = name.starts_with('/') || name.as_bytes().get(1) == Some(&b':');
    let name = if absolute {
        name.rsplit('/').next().unwrap_or(name)
    } else {
        name
    };
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

pub(crate) struct SupplementStamp {
    pub(super) fingerprint: Option<[u8; 32]>,
    /// The row and revision accepted by the writer. A rejected write stays unconfirmed.
    pub(super) confirmed: Option<(TrackId, i64)>,
}
