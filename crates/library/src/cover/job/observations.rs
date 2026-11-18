use super::*;

fn observation_key(relative: &str) -> std::borrow::Cow<'_, str> {
    #[cfg(windows)]
    {
        std::borrow::Cow::Owned(relative.replace('\\', "/").to_lowercase())
    }
    #[cfg(not(windows))]
    {
        std::borrow::Cow::Borrowed(relative)
    }
}

impl ScanObservations {
    /// Seed current identities before visiting any root, independently of scan order.
    /// Read metadata only, outside the writer; failure never resolves an intent.
    pub(crate) fn prewarm_pending(&self, inner: &Inner) -> Result<()> {
        self.prewarm_pending_with(inner, |path| {
            let metadata = std::fs::metadata(path).ok()?;
            if !metadata.is_file() {
                return None;
            }
            inner
                .storage
                .files
                .as_ref()
                .entry_identity(path, &metadata)
                .ok()
                .map(|id| id.parts())
        })
    }
    pub(super) fn prewarm_pending_with(
        &self,
        inner: &Inner,
        mut inspect: impl FnMut(&Path) -> Option<[u64; 2]>,
    ) -> Result<()> {
        inner.check_open()?;
        let _writes = inner.tag_jobs.lock().unwrap_or_else(|e| e.into_inner());
        if !inner
            .cover_recovery
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(());
        }
        // Preserve original path spelling for metadata access (not the normalized
        // lookup key), including Windows volumes with case-sensitive directories.
        let grouped = inner.pool.read(|conn| {
            let mut grouped: HashMap<crate::RootId, HashSet<PathBuf>> = HashMap::new();
            let mut statement = conn.prepare(
                "SELECT t.root_id, i.target_json FROM file_write_intents i
                 JOIN tracks t ON t.id=json_extract(iif(json_valid(i.target_json), i.target_json, NULL),'$.track')
                 WHERE i.family='cover_edit' AND i.stage NOT IN ('finalized','retained')",
            )?;
            let rows = statement.query_map([], |r| {
                Ok((crate::RootId(r.get(0)?), r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let Ok((root, json)) = row else { continue };
                let Ok(target) = parse::<Target>(&json) else { continue };
                let paths = grouped.entry(root).or_default();
                paths.insert(target.path);
                for candidate in [target.backup, target.staged, target.rollback_after]
                    .into_iter()
                    .flatten()
                {
                    paths.insert(candidate.path);
                }
            }
            Ok(grouped)
        })?;
        for (root, paths) in grouped {
            let Some(_lease) = inner.file_access.acquire(root) else {
                continue;
            };
            for path in paths {
                if inner.file_access.released(root) {
                    break;
                }
                if let Some(identity) = inspect(&path)
                    && let Some(parent) = path.parent()
                    && let Some(name) = path.file_name().and_then(|name| name.to_str())
                {
                    self.record_root(parent, std::iter::once((name, Some(identity))));
                }
            }
        }
        Ok(())
    }
    /// `root` uses the stored root's canonical spelling, just like intent paths.
    /// Normalization here is lexical only: never resolve paths on the writer.
    pub(crate) fn record_root<'a>(
        &self,
        root: &Path,
        files: impl Iterator<Item = (&'a str, Option<[u64; 2]>)>,
    ) {
        let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        let found = roots.entry(lexical(root)).or_default();
        for (relative, identity) in files {
            if let Some(identity) = identity {
                found.insert(observation_key(relative).into_owned(), identity);
                #[cfg(all(test, feature = "desktop-fs"))]
                self.insertions
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

pub(crate) fn pending(inner: &Inner, track: TrackId) -> Result<bool> {
    inner
        .pool
        .read(|conn| pending_conn(inner.storage.files.as_ref(), conn, track))
}

pub(super) fn track_path(conn: &Connection, track: TrackId) -> Result<PathBuf> {
    let (root, relative): (String, String) = conn.query_row(
        "SELECT r.path,t.rel_path FROM tracks t JOIN roots r ON r.id=t.root_id WHERE t.id=?1",
        [track.0],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(Path::new(&root).join(relative))
}

pub(crate) fn pending_conn(
    storage: &dyn mpmp_storage::LibraryStorage,
    conn: &Connection,
    track: TrackId,
) -> Result<bool> {
    let path = track_path(conn, track)?;
    pending_path(storage, conn, &path)
}

/// A transaction-local snapshot: never opens an intent target (which may be offline).
/// A fresh snapshot for each scan batch observes all preceding writer mutations.
#[derive(Default)]
pub(crate) struct PendingTargets {
    paths: HashSet<PathBuf>,
    identities: HashSet<[u64; 2]>,
    tracks: HashSet<i64>,
    blocks_missing: bool,
}

pub(super) fn lexical(path: &Path) -> PathBuf {
    #[cfg(all(test, feature = "desktop-fs"))]
    PATH_NORMALIZATIONS.with(|n| n.set(n.get() + 1));
    #[cfg(windows)]
    let path = PathBuf::from(path.to_string_lossy().replace('/', "\\").to_lowercase());
    #[cfg(not(windows))]
    let path = path.to_owned();
    path.components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect()
}

impl PendingTargets {
    pub(crate) fn load(conn: &Connection) -> Result<Self> {
        let mut pending = Self::load_covers(conn)?;
        let (blocks_missing, tags) = tag_job::intents::pending_targets(conn)?;
        pending.blocks_missing = blocks_missing;
        for tag in tags {
            pending.tracks.insert(tag.track.0);
            pending.paths.insert(lexical(&tag.path));
            pending.identities.extend(tag.identity_before);
            pending.identities.insert(tag.identity_after);
        }
        Ok(pending)
    }

    fn load_covers(conn: &Connection) -> Result<Self> {
        const SELECT_FILE_WRITE_INTENTS: &str = r#"
SELECT target_json FROM file_write_intents WHERE family='cover_edit'
AND stage NOT IN ('finalized','retained')
"#;

        let mut result = Self::default();
        for row in conn
            .prepare(SELECT_FILE_WRITE_INTENTS)?
            .query_map([], |r| r.get::<_, String>(0))?
        {
            let Ok(json) = row else { continue };
            let Ok(target) = parse::<Target>(&json) else {
                continue;
            };
            result.tracks.insert(target.track);
            result.paths.insert(lexical(&target.path));
            if let Some(id) = target.before_identity {
                result.identities.insert(id);
            }
            for c in [&target.backup, &target.staged, &target.rollback_after]
                .into_iter()
                .flatten()
            {
                result.paths.insert(lexical(&c.path));
                if c.identity != [0, 0] {
                    result.identities.insert(c.identity);
                }
            }
        }
        Ok(result)
    }
    pub(crate) fn protects_missing(&self, track: TrackId, path: &Path) -> bool {
        if self.paths.is_empty() {
            return false;
        }
        if self.tracks.contains(&track.0) || self.protects(path, None) {
            return true;
        }
        // A broken local symlink still identifies its missing destination. Read only
        // the link itself; never follow it into a possibly offline target directory.
        std::fs::read_link(path).is_ok_and(|link| {
            let target = if link.is_absolute() {
                link
            } else {
                path.parent().unwrap_or(Path::new("")).join(link)
            };
            self.protects(&target, None)
        })
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
    pub(crate) fn blocks_missing(&self) -> bool {
        self.blocks_missing
    }
    /// Only query the latest pending paths, never iterate the entire scan per batch.
    pub(crate) fn merge_observed(&mut self, observed: &ScanObservations) {
        if self.is_empty() {
            return;
        }
        let roots = observed.roots.lock().unwrap_or_else(|e| e.into_inner());
        for path in &self.paths {
            #[cfg(all(test, feature = "desktop-fs"))]
            observed
                .lookups
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            for parent in path.ancestors().skip(1) {
                if let Some(found) = roots.get(parent)
                    && let Some(relative) = path.strip_prefix(parent).ok().and_then(Path::to_str)
                    && let Some(identity) = found.get(observation_key(relative).as_ref())
                {
                    self.identities.insert(*identity);
                }
            }
        }
    }
    /// Scan callers supply canonical root + relative path, without target I/O.
    pub(crate) fn protects(&self, path: &Path, identity: Option<[u64; 2]>) -> bool {
        !self.is_empty()
            && (self.paths.contains(&lexical(path))
                || identity.is_some_and(|i| self.identities.contains(&i)))
    }
}

/// Writes resolve only the supplied target. Scans reuse metadata and a batch snapshot.
pub(crate) fn pending_path(
    storage: &dyn mpmp_storage::LibraryStorage,
    conn: &Connection,
    path: &Path,
) -> Result<bool> {
    const SELECT_FILE_WRITE_INTENTS: &str = r#"
SELECT target_json FROM file_write_intents WHERE family='cover_edit'
AND stage NOT IN ('finalized','retained')
"#;

    let pending = PendingTargets::load_covers(conn)?;
    if pending.paths.is_empty() {
        return Ok(false);
    }
    if pending.protects(path, None) {
        return Ok(true);
    }
    let canonical = storage_access::comparable_with(storage, path);
    let id = identity(storage, path);
    if pending.protects(&canonical, id) {
        return Ok(true);
    }
    // Explicit writes must also detect hard links to an externally replaced target.
    // This slow path is deliberately excluded from scan batches and ordinary files.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(path).is_ok_and(|m| m.nlink() <= 1) {
            return Ok(false);
        }
    }
    if let Some(id) = id {
        for row in conn
            .prepare(SELECT_FILE_WRITE_INTENTS)?
            .query_map([], |r| r.get::<_, String>(0))?
        {
            if protects_identity(storage, &parse::<Target>(&row?)?, id) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(super) fn identity(
    storage: &dyn mpmp_storage::LibraryStorage,
    path: &Path,
) -> Option<[u64; 2]> {
    storage_access::open_regular(storage, path, true)
        .ok()
        .and_then(|f| storage.opened_identity(&f).ok())
        .map(|i| i.parts())
}

pub(super) fn protects_identity(
    storage: &dyn mpmp_storage::LibraryStorage,
    target: &Target,
    id: [u64; 2],
) -> bool {
    target.before_identity == Some(id)
        || identity(storage, &target.path) == Some(id)
        || target.backup.as_ref().is_some_and(|c| c.identity == id)
        || target.staged.as_ref().is_some_and(|c| c.identity == id)
        || target
            .rollback_after
            .as_ref()
            .is_some_and(|c| c.identity == id)
}
