use super::*;
use crate::journal;

impl Library {
    /// Creates a manual playlist from an M3U/M3U8 file, named `name` or the
    /// file name without extension. Only `.m3u` / `.m3u8` files (letter case ignored) are
    /// read; another file name is [`LibraryError::Invalid`] (`not_an_m3u`). Entries that are not tracks of
    /// an active root are not added: [`M3uReport::missing`] lists the first
    /// [`crate::M3U_MISSING_LIMIT`] of them, [`M3uReport::missing_total`] counts them all.
    /// A file that does not exist is `file_not_found`; only a regular file is read (a
    /// folder, FIFO or device is `not_a_regular_file`, and opening one never waits for a
    /// writer). Recorded as `playlist_import` (its undo moves the playlist to the trash).
    ///
    /// A path checked by the caller ([`ActionMeta::checked`]): the file opened must be the
    /// one at `path`, still in canonical form (else `location_changed`, and nothing is
    /// read; a symlink at `path` is never opened, on Unix by `O_NOFOLLOW`), and `missing` lists only lines that look like references to media (a path or
    /// URL with an audio or cue sheet extension); `missing_total` counts them all.
    pub fn import_m3u(
        &self,
        path: &Path,
        name: Option<&str>,
        meta: ActionMeta<'_>,
    ) -> Result<Done<(PlaylistId, M3uReport)>> {
        let _access = self.path_leases(path)?;
        if m3u::kind(path).is_none() {
            return Err(not_m3u(path));
        }
        let entries =
            m3u::read(self.inner.storage.files.as_ref(), path, meta.checked).map_err(|err| {
                match err {
                    LibraryError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
                        LibraryError::not_found(NotFound::File, format!("{}: {io}", path.display()))
                    }
                    err => err,
                }
            })?;
        let name = match name {
            Some(name) => name.to_owned(),
            None => path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        let mut tracks = Vec::with_capacity(entries.len());
        let mut missing = Vec::new();
        let mut missing_total = 0u64;
        self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let mut matcher = m3u::Matcher::new(self.inner.storage.files.as_ref(), &tx)?;
            for entry in entries {
                match matcher.find_any(&tx, &entry.paths)? {
                    Some(track) => tracks.push(track),
                    None => {
                        missing_total += 1;
                        // Only what a playlist is made of goes back to someone confined
                        // to what was checked: any other line of any file could be read so.
                        if missing.len() < playlist::M3U_MISSING_LIMIT
                            && (!meta.checked || m3u::looks_like_media(&entry.line))
                        {
                            missing.push(entry.line);
                        }
                    }
                }
            }
            Ok(tx.commit()?)
        })?;
        let matched = tracks.len() as u64;
        let origin = meta.owned();
        let done = self.inner.writer.run(move |conn| {
            let meta = origin.meta();
            playlist::create(conn, &name, &PlaylistKind::Manual, &tracks, true, meta)
        })?;
        Ok(done.map(|id| {
            (
                id,
                M3uReport {
                    matched,
                    missing,
                    missing_total,
                },
            )
        }))
    }

    /// Writes a playlist as UTF-8 M3U: every manual item (unavailable tracks
    /// too, at their last known path) or the smart query's tracks. Paths are relative to
    /// the file's folder when that folder is inside the track's root, else absolute. The
    /// folder must exist.
    ///
    /// The file name must end in `.m3u` or `.m3u8` (letter case ignored), else
    /// [`LibraryError::Invalid`] (`not_an_m3u`). The file is written to a temporary file in
    /// the same folder and renamed into place, so readers never see a partial file. An
    /// existing file is replaced only with `overwrite` (else
    /// [`LibraryError::AlreadyExists`]), and only if it is a regular file (a symlink, folder
    /// or device is `not_a_regular_file`).
    ///
    /// Returns whether a file was replaced. Replacing one cannot be undone: it is recorded
    /// as an audit entry (kind `file_write`, never undoable), named by the returned action.
    /// With `record_new`, writing a new file is recorded too (core asks for it for agents,
    /// so that what they wrote can be traced).
    ///
    /// A path checked by the caller ([`ActionMeta::checked`]): a symlink at the path is
    /// `location_changed` (the path was checked in canonical form). Its folder must still
    /// be in canonical form and the same folder before the temporary file is created in it,
    /// right after (else the temporary file is removed again), and right before it is
    /// renamed into place, with the temporary file still the one written and the file at
    /// the path still the one found when the export began (or still none); else
    /// `location_changed`, and the file is not put in place. A folder that does not exist
    /// is the same `NotFound` error as without the check. What remains (closing it needs
    /// `openat`-style calls): the moment between the last check and the rename, and the
    /// folder swapped for a link right before the temporary file is created and back
    /// before the check right after it; the export then fails, but that hidden temporary
    /// file (holding the playlist) stays where the link led.
    pub fn export_m3u(
        &self,
        id: PlaylistId,
        path: &Path,
        overwrite: bool,
        record_new: bool,
        meta: ActionMeta<'_>,
    ) -> Result<Done<bool>> {
        self.inner
            .register_file_job()?
            .run(|| self.export_m3u_inner(id, path, overwrite, record_new, meta))
    }

    fn export_m3u_inner(
        &self,
        id: PlaylistId,
        path: &Path,
        overwrite: bool,
        record_new: bool,
        meta: ActionMeta<'_>,
    ) -> Result<Done<bool>> {
        let _access = self.path_leases(path)?;
        if m3u::kind(path).is_none() {
            return Err(not_m3u(path));
        }
        let tracks = self.inner.pool.read(|conn| {
            let tx = conn.unchecked_transaction()?;
            let tracks = playlist::export_tracks(&tx, id)?;
            tx.commit()?;
            Ok(tracks)
        })?;
        let path = std::path::absolute(path)?;
        // What is there now; for a checked path also which file (compared again before
        // the new file is put in place).
        let (replaces, found) = match std::fs::symlink_metadata(&path) {
            // Checked in canonical form: a symlink there now was put in since.
            Ok(file) if meta.checked && file.file_type().is_symlink() => {
                return Err(storage_access::location_changed(&path));
            }
            Ok(file) if !file.file_type().is_file() => {
                return Err(storage_access::not_a_regular_file(&path));
            }
            Ok(_) if !overwrite => return Err(LibraryError::AlreadyExists(path)),
            Ok(file) if meta.checked => (
                true,
                Some(
                    self.inner
                        .storage
                        .files
                        .entry_id_from_metadata(&path, &file)?,
                ),
            ),
            Ok(_) => (true, None),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (false, None),
            Err(err) => return Err(err.into()),
        };
        let dir = path.parent().unwrap_or(Path::new(""));
        write_atomically(
            self.inner.storage.files.as_ref(),
            &path,
            m3u::format(self.inner.storage.files.as_ref(), &tracks, dir).as_bytes(),
            overwrite,
            meta.checked.then_some(Checked { found }),
        )?;
        #[cfg(all(test, feature = "desktop-fs"))]
        {
            let hook = self.inner.hooks.after_m3u_publish.lock().unwrap().take();
            if let Some(hook) = hook {
                hook()?;
            }
        }
        if !replaces && !record_new {
            return Ok(Done::unchanged(false));
        }
        let origin = meta.owned();
        let name = self.playlist(id)?.map(|p| p.name).unwrap_or_default();
        let summary = if replaces {
            format!(
                "replace the file {} with playlist \"{name}\"",
                path.display()
            )
        } else {
            format!(
                "write the new file {} from playlist \"{name}\"",
                path.display()
            )
        };
        let action = self.inner.writer.run(move |conn| {
            journal::record_audit(conn, origin.meta(), "file_write", summary, 1, now_ms())
        })?;
        Ok(Done {
            value: replaces,
            action: Some(action),
        })
    }
}

fn not_m3u(path: &Path) -> LibraryError {
    LibraryError::invalid(
        Invalid::NotAnM3u,
        format!("{} is not an M3U playlist (.m3u or .m3u8)", path.display()),
    )
}

/// For a write to a path checked by the caller ([`ActionMeta::checked`]): the file found
/// at the path then (`None`: none).
pub(super) struct Checked {
    pub(super) found: Option<mpmp_storage::EntryId>,
}

/// The folder of a checked write, as it was when the write began, and what was at the path.
pub(super) struct Guard<'a> {
    storage: &'a dyn mpmp_storage::LibraryStorage,
    dir: &'a Path,
    folder: mpmp_storage::EntryId,
    pub(super) found: Option<mpmp_storage::EntryId>,
}

impl<'a> Guard<'a> {
    /// The folder must be in canonical form now (a folder that does not exist is the plain
    /// `NotFound` error: nothing changed).
    fn new(
        storage: &'a dyn mpmp_storage::LibraryStorage,
        dir: &'a Path,
        checked: Checked,
    ) -> Result<Self> {
        storage_access::ensure_canonical(storage, dir)?;
        Ok(Self {
            storage,
            dir,
            folder: storage.entry_id(dir)?,
            found: checked.found,
        })
    }

    /// Whether the folder is still in canonical form and still the folder it was.
    fn folder_holds(&self) -> bool {
        storage_access::canonical(self.storage, self.dir).is_ok_and(|c| c == self.dir)
            && self
                .storage
                .entry_id(self.dir)
                .is_ok_and(|id| id == self.folder)
    }

    /// Right after the temporary file `temp` was created: in the folder checked? If the
    /// folder was swapped for a link just before, the file was created where the link
    /// leads; it is removed again while the link is still there to tell.
    fn created(&self, temp: &Path, written: &mpmp_storage::EntryId) -> Result<()> {
        if self.folder_holds() {
            return Ok(());
        }
        remove_if_written(self.storage, temp, written);
        Err(storage_access::location_changed(self.dir))
    }

    /// Right before `temp` is put in place at `path`: the folder holds, `temp` is still the
    /// file written, and (when `overwrite`) `path` still holds what was found there (or
    /// still nothing). Consumes the guard and `written`, so that nothing they hold is open
    /// while the file is put in place.
    fn last(
        self,
        path: &Path,
        temp: &Path,
        written: mpmp_storage::EntryId,
        overwrite: bool,
    ) -> Result<()> {
        let now = |at: &Path| match self.storage.entry_id(at) {
            Ok(id) => Ok(Some(id)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        };
        let changed =
            if !self.folder_holds() || !now(temp).is_ok_and(|id| id.as_ref() == Some(&written)) {
                self.dir
            } else if overwrite && !now(path).is_ok_and(|id| id == self.found) {
                path
            } else {
                return Ok(());
            };
        remove_if_written(self.storage, temp, &written);
        Err(storage_access::location_changed(changed))
    }
}

/// Removes `temp` if it is still the file written (else leaves whatever is there now).
fn remove_if_written(
    storage: &dyn mpmp_storage::LibraryStorage,
    temp: &Path,
    written: &mpmp_storage::EntryId,
) {
    if storage.entry_id(temp).is_ok_and(|id| id == *written) {
        let _ = std::fs::remove_file(temp);
    }
}

/// Writes `bytes` to `path` through a new temporary file in the same folder, renamed into
/// place: a crash leaves either the old file or the new one. Without `overwrite`, an
/// existing file at `path` (even one created meanwhile) is [`LibraryError::AlreadyExists`].
/// `checked` ([`Guard`]): the folder must still be in canonical form and the same folder
/// before the temporary file is created, right after (else it is removed again) and
/// before it is put in place, with the temporary file still the one written and the file
/// at `path` still the one found (else `location_changed`).
pub(super) fn write_atomically(
    storage: &dyn mpmp_storage::LibraryStorage,
    path: &Path,
    bytes: &[u8],
    overwrite: bool,
    checked: Option<Checked>,
) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let dir = path.parent().unwrap_or(Path::new(""));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let guard = checked
        .map(|checked| Guard::new(storage, dir, checked))
        .transpose()?;
    let (temp, mut file) = loop {
        let temp = dir.join(format!(
            ".{name}.{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // `create_new`: never opens (or follows a symlink to) an existing file.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(file) => break (temp, file),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    };
    let at: &Path = &temp;
    let result = (move || -> Result<()> {
        let written = match &guard {
            Some(guard) => {
                let written = storage.opened_id(&file)?;
                guard.created(at, &written)?;
                Some(written)
            }
            None => None,
        };
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if let (Some(guard), Some(written)) = (guard, written) {
            guard.last(path, at, written, overwrite)?;
        }
        if overwrite {
            std::fs::rename(at, path)?;
            return Ok(());
        }
        // A hard link fails if `path` exists, atomically; filesystems without hard links
        // (FAT, some Android storage) fall back to a check, then the rename.
        match std::fs::hard_link(at, path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(LibraryError::AlreadyExists(path.to_owned()))
            }
            Err(_) if std::fs::symlink_metadata(path).is_ok() => {
                Err(LibraryError::AlreadyExists(path.to_owned()))
            }
            Err(_) => Ok(std::fs::rename(at, path)?),
        }
    })();
    // Gone after a rename; after a hard link or a failure, still there. A checked write
    // that found its place changed has removed it already, if it still could be told.
    let changed = matches!(
        &result,
        Err(LibraryError::Invalid {
            why: Invalid::LocationChanged,
            ..
        })
    );
    if !changed {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Joins a `/`-separated relative path onto a root with the platform's separator.
pub(super) fn join_relative(root: &Path, rel_path: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    path.extend(rel_path.split('/'));
    path
}
