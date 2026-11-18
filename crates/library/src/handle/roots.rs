use super::*;
use crate::{db::reads, journal};

impl Library {
    // ---- roots ---

    /// Adds a folder to the library; returns the existing id if it is already a root (no
    /// action), and revives a removed root with the same path (same id and track ids).
    /// Folders inside or around an active root are rejected. Call [`Library::scan`] to
    /// index it. Recorded as `root_add`; its undo removes the root again. A path checked
    /// by the caller ([`ActionMeta::checked`]) must still be the folder's canonical form
    /// (else `location_changed`).
    pub fn add_root(&self, path: &Path, meta: ActionMeta<'_>) -> Result<Done<RootId>> {
        self.inner.check_open()?;
        if !path.is_dir() {
            return Err(LibraryError::NotADirectory(path.to_owned()));
        }
        // Canonical, so that `..`, symlinks and (on Windows) letter case cannot make two
        // roots overlap unnoticed.
        let canonical = storage_access::canonical(self.inner.storage.files.as_ref(), path)?;
        if meta.checked && canonical != path {
            return Err(storage_access::location_changed(path));
        }
        let path = canonical;
        let origin = meta.owned();
        let storage = self.inner.storage.files.clone();
        self.inner
            .writer
            .run_data(move |conn| writer::add_root(storage.as_ref(), conn, &path, origin.meta()))
    }

    /// Removes a root. Removal is soft: its tracks become unavailable but keep their rows,
    /// so re-adding the same folder (then scanning) brings back the same track ids and the
    /// play history stays linked. Recorded as `root_remove`; its undo makes the root active
    /// again (its tracks come back with the next scan). Removing a root that is removed
    /// already changes nothing (no action); an unknown id is `root_not_found`.
    pub fn remove_root(&self, id: RootId, meta: ActionMeta<'_>) -> Result<Done<()>> {
        let origin = meta.owned();
        let done = self
            .inner
            .writer
            .run_data(move |conn| writer::remove_root(conn, id, origin.meta()))?;
        self.inner.file_access.resume(id);
        Ok(done)
    }

    /// Stop opening files in this root and wait for all registered file access to end.
    /// Session-local: `resume_root`, removing the root, or reopening the library ends suspension.
    pub fn release_root(&self, root: RootId) -> Result<bool> {
        self.check_active_root(root)?;
        let changed = self.suspend_root(root)?;
        if !self.wait_root_idle(root, std::time::Duration::from_secs(5))? {
            return Err(LibraryError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "root file access did not drain within 5 seconds",
            )));
        }
        Ok(changed)
    }
    /// Mark suspension without waiting. Call before stopping playback and draining leases.
    pub fn suspend_root(&self, root: RootId) -> Result<bool> {
        self.check_active_root(root)?;
        Ok(self.inner.file_access.suspend(root))
    }
    /// Audit an authorized root-access request, independently of its later barrier
    /// result. Record before changing state so journal I/O errors fail closed.
    pub fn audit_root_access(
        &self,
        root: RootId,
        released: bool,
        meta: ActionMeta<'_>,
    ) -> Result<ActionId> {
        self.check_active_root(root)?;
        let origin = meta.owned();
        self.inner.writer.run(move |conn| {
            let kind = if released {
                "root_release"
            } else {
                "root_resume"
            };
            let tx = conn.unchecked_transaction()?;
            let path: String = tx.query_row(
                "SELECT path FROM roots WHERE id=?1 AND removed = 0",
                [root.0],
                |row| row.get(0),
            )?;
            let id = journal::record_audit(
                &tx,
                origin.meta(),
                kind,
                format!("request {kind} for library root {root}"),
                1,
                now_ms(),
            )?;
            let details =
                serde_json::json!({"v":1,"details":{"type":"root","root":root.0,"path":path}});
            tx.execute(
                "UPDATE actions SET details_json=?2 WHERE id=?1",
                rusqlite::params![id.0, details.to_string()],
            )?;
            tx.commit()?;
            Ok(id)
        })
    }
    pub fn wait_root_idle(&self, root: RootId, timeout: std::time::Duration) -> Result<bool> {
        self.inner.check_open()?;
        Ok(self.inner.file_access.wait_idle(root, timeout))
    }
    /// Leases for a playback URI; acquire before resolver/open and retain through decode.
    pub fn playback_lease(&self, uri: &MediaUri) -> Result<Vec<crate::FileLease>> {
        self.inner.check_open()?;
        match uri.to_file_path() {
            Some(path) => self.path_leases(&path),
            None => Ok(Vec::new()),
        }
    }
    /// Re-enable file access. Does not start playback or a scan itself.
    pub fn resume_root(&self, root: RootId) -> Result<bool> {
        self.check_active_root(root)?;
        Ok(self.inner.file_access.resume(root))
    }
    pub fn root_released(&self, root: RootId) -> bool {
        self.inner.file_access.released(root)
    }
    fn check_active_root(&self, root: RootId) -> Result<()> {
        if self.inner.pool.read(|c| reads::root_removed(c, root))? != Some(false) {
            return Err(LibraryError::not_found(
                NotFound::Root,
                format!("library root {root}"),
            ));
        }
        Ok(())
    }
    /// Hold across a complete physical file read, including opening and decoding.
    pub fn file_lease(&self, track: TrackId) -> Result<crate::FileLease> {
        let track = self
            .track(track)?
            .ok_or_else(|| LibraryError::not_found(NotFound::Track, "unknown track"))?;
        self.inner
            .file_access
            .acquire(track.root_id)
            .ok_or(LibraryError::RootReleased(track.root_id))
    }

    /// Explicit M3U paths inside a released root must obey the same barrier as media.
    /// Hold both the named location and its resolved location until the operation ends.
    pub(super) fn path_leases(&self, path: &Path) -> Result<Vec<crate::FileLease>> {
        let roots = self.inner.pool.read(reads::root_paths)?;
        let mut leases = Vec::new();
        let mut held = std::collections::HashSet::new();
        let mut hold = |path: &Path| -> Result<()> {
            for (id, root_path) in &roots {
                if path.starts_with(root_path) && held.insert(*id) {
                    leases.push(
                        self.inner
                            .file_access
                            .acquire(*id)
                            .ok_or(LibraryError::RootReleased(*id))?,
                    );
                }
            }
            Ok(())
        };
        hold(path)?;
        if let Ok(resolved) = std::fs::canonicalize(path) {
            hold(&resolved)?;
        } else if let Some(parent) = path.parent()
            && let Ok(parent) = std::fs::canonicalize(parent)
        {
            hold(&parent)?;
        }
        Ok(leases)
    }

    /// Sets or clears the encoding used for Latin-1 ID3 text in this root, instead of
    /// detection. Files of the root are re-read on the next scan.
    pub fn set_root_encoding(&self, id: RootId, label: Option<&str>) -> Result<()> {
        let label = match label {
            None => None,
            Some(label) => match encoding_for_label(label) {
                Some(encoding) => Some(encoding.name().to_ascii_lowercase()),
                None => {
                    return Err(LibraryError::invalid(
                        Invalid::UnknownEncoding,
                        format!("unknown text encoding {label:?}"),
                    ));
                }
            },
        };
        self.inner
            .writer
            .call(|reply| WriteCmd::SetRootEncoding { id, label, reply })
    }

    /// Internal scoped scanning; None retains the ordinary full-library scan.
    pub fn scan_roots(
        &self,
        roots: Option<Vec<RootId>>,
        progress: impl Fn(crate::ScanProgress) + Send + 'static,
    ) -> Result<crate::ScanStart> {
        crate::scan::start_scoped(
            Arc::clone(&self.inner),
            roots.map(|ids| ids.into_iter().collect()),
            progress,
        )
    }

    /// Retry only these directories beneath active registered roots. This repair pass
    /// preserves missing/out-of-scope rows; ordinary verification handles removals.
    pub fn scan_directories(
        &self,
        directories: Vec<PathBuf>,
        progress: impl Fn(crate::ScanProgress) + Send + 'static,
    ) -> Result<crate::ScanStart> {
        crate::scan::start_directories(Arc::clone(&self.inner), directories, progress)
    }

    pub fn roots(&self) -> Result<Vec<Root>> {
        self.inner.pool.read(reads::roots)
    }

    /// Whether root `id` is removed (`Some(true)`), active (`Some(false)`) or unknown
    /// (`None`).
    pub fn root_removed(&self, id: RootId) -> Result<Option<bool>> {
        self.inner.pool.read(|conn| reads::root_removed(conn, id))
    }

    /// Starts an incremental scan of all roots on a background thread. `progress` is called
    /// from that thread, at most about ten times a second and once at the end.
    ///
    /// Only one scan runs at a time: a request while one runs returns
    /// [`ScanStart::Coalesced`] with the running scan's id and makes it do another pass over
    /// the then current roots before it finishes; its [`crate::ScanReport`] covers that pass
    /// and counts the request. Fails only if the scan thread cannot be started.
    pub fn scan(&self, progress: impl Fn(ScanProgress) + Send + 'static) -> Result<ScanStart> {
        scan::start(self.inner.clone(), progress)
    }
}
