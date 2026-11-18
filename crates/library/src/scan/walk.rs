use super::*;

impl Scanner<'_> {
    /// Lists the audio files under `root`. `None` when the root folder itself is missing
    /// (e.g. an unplugged drive).
    pub(super) fn walk(&mut self, root: &Root) -> Result<Option<Walked>> {
        self.check_root(root.id)?;
        if !root.path.is_dir() {
            tracing::warn!(root = %root.path.display(), "library root is unreachable; skipped");
            self.error(
                &root.path,
                ScanErrorCode::RootUnreachable,
                "library root is unreachable; skipped",
            );
            self.root_skipped = true;
            return Ok(None);
        }
        let mut files = Vec::new();
        let mut sheets = Vec::new();
        let mut enumerated = HashSet::new();
        let mut completeness = WalkCompleteness {
            root_readable: true,
            unreadable: Vec::new(),
        };
        // Folders (relative, `/`-separated, "" for the root) that hold a cover image.
        let mut art_dirs: HashSet<String> = HashSet::new();
        // Start at the selected directories, never walk unrelated sibling trees.
        let mut starts: Vec<PathBuf> = if self.scope.directory_only(root) {
            let dirs = self.scope.directories.as_ref().expect("directory scope");
            dirs.iter()
                .filter(|dir| {
                    dir.starts_with(&root.path)
                        && !dirs.iter().any(|parent| {
                            *dir != parent
                                && parent.starts_with(&root.path)
                                && dir.starts_with(parent)
                        })
                })
                .cloned()
                .collect()
        } else {
            vec![root.path.clone()]
        };
        starts.sort();
        self.diagnostics.coverage.extend(starts.iter().cloned());
        let mut entries = starts
            .into_iter()
            .flat_map(|path| WalkDir::new(path).follow_links(true).sort_by_file_name());
        loop {
            self.check_root(root.id)?;
            let Some(entry) = entries.next() else {
                break;
            };
            self.check_root(root.id)?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    tracing::warn!(%err, "cannot read part of library root");
                    let path = err.path().unwrap_or(root.path.as_path()).to_owned();
                    self.error(
                        &path,
                        ScanErrorCode::DirectoryReadFailed,
                        format_args!("cannot read: {err}"),
                    );
                    completeness.failed(&root.path, &path);
                    continue;
                }
            };
            #[cfg(all(test, feature = "desktop-fs"))]
            if self
                .inner
                .hooks
                .fail_walk
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|path| entry.path().starts_with(path))
            {
                self.error(
                    entry.path(),
                    ScanErrorCode::DirectoryReadFailed,
                    "injected enumeration failure",
                );
                completeness.failed(&root.path, entry.path());
                continue;
            }
            if entry.file_type().is_dir() {
                enumerated.insert(entry.path().to_owned());
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let sheet = is_sheet(entry.path());
            if !sheet && !is_audio(entry.path()) {
                let is_cover = entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| cover_rank(name).is_some());
                if is_cover && let Some(rel_path) = relative_path(&root.path, entry.path()) {
                    art_dirs.insert(parent_dir(&rel_path).to_owned());
                }
                continue;
            }
            let Some(rel_path) = relative_path(&root.path, entry.path()) else {
                tracing::warn!(path = %entry.path().display(), "skipping non-Unicode path");
                self.error(
                    entry.path(),
                    ScanErrorCode::InvalidPath,
                    "path is not valid Unicode; skipped",
                );
                continue;
            };
            let metadata = entry.metadata().map_err(std::io::Error::from);
            #[cfg(all(test, feature = "desktop-fs"))]
            let metadata = if self.inner.hooks.fail_metadata.lock().unwrap().as_deref()
                == Some(entry.path())
            {
                Err(std::io::Error::other("injected stat failure"))
            } else {
                metadata
            };
            let metadata = match metadata {
                Ok(metadata) => metadata,
                Err(err) => {
                    tracing::warn!(%err, path = %entry.path().display(), "cannot stat file");
                    self.error(
                        entry.path(),
                        ScanErrorCode::StatFailed,
                        format_args!("cannot read file metadata: {err}"),
                    );
                    completeness.failed(&root.path, entry.path());
                    continue;
                }
            };
            let mtime_ms = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            let found = Found {
                file_identity: self
                    .inner
                    .storage
                    .files
                    .entry_identity(entry.path(), &metadata)
                    .ok()
                    .map(|i| i.parts()),
                rel_path,
                size: metadata.len(),
                mtime_ms,
                folder_art: false,
            };
            if sheet {
                sheets.push(found);
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(entry) =
                self.audio_paths.entry((root.id, found.rel_path.clone()))
            {
                entry.insert(false);
                self.progress.total_estimate += 1;
            }
            files.push(found);
            self.tick();
        }
        // Model a read_dir iterator that yields entries and then fails.
        #[cfg(all(test, feature = "desktop-fs"))]
        if let Some(path) = self.inner.hooks.fail_walk_after.lock().unwrap().clone() {
            self.error(
                &path,
                ScanErrorCode::DirectoryReadFailed,
                "injected late enumeration failure",
            );
            completeness.failed(&root.path, &path);
        }
        if self.scope.directory_only(root) {
            self.retry_reverse_cues(root, &files, &mut sheets)?;
            self.retry_cue_dependencies(root, &sheets, &mut files, &mut art_dirs)?;
        } else {
            // Prune removed sheets only after a complete, readable walk. Failed reads
            // keep the old dependency evidence until a later successful observation.
            let present: HashSet<_> = sheets
                .iter()
                .map(|sheet| root.path.join(&sheet.rel_path))
                .collect();
            if completeness.complete() {
                let mut dependencies = self
                    .inner
                    .cue_dependencies
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                dependencies.retain(|_, sheets| {
                    sheets
                        .retain(|sheet| !sheet.starts_with(&root.path) || present.contains(sheet));
                    !sheets.is_empty()
                });
            }
        }
        for file in &mut files {
            self.check_root(root.id)?;
            file.folder_art = art_dirs.contains(parent_dir(&file.rel_path));
        }
        sheets.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        let enumerated = enumerated
            .into_iter()
            .filter(|dir| {
                let relative = dir
                    .strip_prefix(&root.path)
                    .ok()
                    .map(|p| p.to_string_lossy().replace('\\', "/"));
                relative.is_some_and(|p| completeness.can_mark_missing(&p))
            })
            .filter_map(|dir| {
                crate::storage_access::canonical(self.inner.storage.files.as_ref(), &dir).ok()
            })
            .collect();
        Ok(Some(Walked {
            files,
            sheets,
            completeness,
            enumerated,
        }))
    }
}

/// What the walk of one root found.
pub(super) struct Walked {
    pub(super) files: Vec<Found>,
    /// CUE sheets (`.cue` files), by path.
    pub(super) sheets: Vec<Found>,
    pub(super) completeness: WalkCompleteness,
    pub(super) enumerated: HashSet<PathBuf>,
}

/// Failure to observe a path is not evidence that it disappeared.
pub(super) struct WalkCompleteness {
    pub(super) root_readable: bool,
    /// Relative paths (files or folders) whose old rows must be retained.
    pub(super) unreadable: Vec<String>,
}

impl WalkCompleteness {
    pub(super) fn failed(&mut self, root: &Path, path: &Path) {
        if let Some(relative) = relative_path(root, path) {
            self.unreadable.push(relative);
        } else {
            // Root enumeration errors and errors with no usable path cannot be
            // bounded to a subtree: disable all missing-file decisions for this root.
            self.root_readable = false;
        }
    }
    pub(super) fn can_mark_missing(&self, path: &str) -> bool {
        self.root_readable
            && !self.unreadable.iter().any(|unknown| {
                path == unknown
                    || path
                        .strip_prefix(unknown)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
    }
    pub(super) fn complete(&self) -> bool {
        self.root_readable && self.unreadable.is_empty()
    }
}

/// A file found by the walk.
#[derive(Debug, Clone)]
pub(super) struct Found {
    pub(super) file_identity: Option<[u64; 2]>,
    pub(super) rel_path: String,
    pub(super) size: u64,
    pub(super) mtime_ms: i64,
    /// A cover image file is in the same folder.
    pub(super) folder_art: bool,
}
