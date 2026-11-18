//! Shared desktop/bridge data paths and all-or-nothing first-run migration.
use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

pub const APP_DIR: &str = "mpmp";

pub const LEGACY_IDENTIFIER: &str = "dev.cocytus.personal-music-player";

const NOTICE: &str = ".mpmp-migration-source";

const IN_PROGRESS: &str = ".mpmp-migration-in-progress";

const WEBVIEW_OWNER: &str = ".mpmp-webview-cache";

const WEBVIEW_SIGNATURE: &[u8] = b"mpmp owned regenerable webview profile v1\n";

/// Mark only newly created profiles as ours. Never claim an existing user directory.
pub fn prepare_webview_cache(root: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(root)?;
    let path = root.join("webview");
    match fs::create_dir(&path) {
        Ok(()) => {
            use io::Write;
            let mut marker = fs::File::create(path.join(WEBVIEW_OWNER))?;
            marker.write_all(WEBVIEW_SIGNATURE)?;
            marker.sync_all()?;
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => regular_directory(&path)?,
        Err(e) => return Err(e),
    }
    Ok(path)
}

#[derive(Clone, Copy)]
pub enum Platform {
    Windows,
    Macos,
    Linux,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::Linux
        }
    }
}

pub struct DataPaths {
    pub root: PathBuf,
    /// Explicit overrides are never migrated.
    pub legacy: Option<PathBuf>,
}

impl DataPaths {
    pub fn current() -> io::Result<Self> {
        Self::resolve(
            Platform::current(),
            |name| {
                std::env::var_os(name)
                    .filter(|value| name != "HOME" || Path::new(value).is_absolute())
                    .or_else(|| {
                        if name == "HOME" {
                            home_from_user_database().map(PathBuf::into_os_string)
                        } else {
                            None
                        }
                    })
            },
            &std::env::current_dir()?,
        )
    }

    /// Pure path policy, including Windows roaming AppData (not LocalAppData).
    pub fn resolve(
        platform: Platform,
        env: impl Fn(&str) -> Option<OsString>,
        cwd: &Path,
    ) -> io::Result<Self> {
        if let Some(value) = env("PLAYER_DATA_DIR").or_else(|| env("PLAYER_MANUAL_DATA_DIR")) {
            let path = PathBuf::from(value);
            return Ok(Self {
                root: if path.is_absolute() {
                    path
                } else {
                    cwd.join(path)
                },
                legacy: None,
            });
        }
        let absolute = |name| env(name).map(PathBuf::from).filter(|p| p.is_absolute());
        let base = match platform {
            Platform::Windows => absolute("APPDATA"),
            Platform::Macos => absolute("HOME").map(|p| p.join("Library/Application Support")),
            Platform::Linux => absolute("XDG_DATA_HOME")
                .or_else(|| absolute("HOME").map(|p| p.join(".local/share"))),
        }
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "mpmp: cannot determine the platform data directory",
            )
        })?;
        Ok(Self {
            root: base.join(APP_DIR),
            legacy: Some(base.join(LEGACY_IDENTIFIER)),
        })
    }
}

mod lease;
pub use lease::DataLease;
use lease::{publication_lease, runtime_lease};

/// Stable categories for native startup presentation; underlying I/O text is retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparationErrorKind {
    OldDatabaseInUse,
    NameConflict,
    Io,
}

#[derive(Debug)]
struct PreparationError {
    kind: PreparationErrorKind,
    message: String,
}

impl std::fmt::Display for PreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PreparationError {}

pub fn preparation_error_kind(error: &io::Error) -> PreparationErrorKind {
    error
        .get_ref()
        .and_then(|e| e.downcast_ref::<PreparationError>())
        .map_or(PreparationErrorKind::Io, |e| e.kind)
}

fn categorized(kind: PreparationErrorKind, message: String) -> io::Error {
    io::Error::other(PreparationError { kind, message })
}

/// Prepare storage before core, settings or WebViews. Publication remains held until
/// the runtime lease is acquired; moving trees contain no open runtime handles.
pub fn prepare(paths: &DataPaths) -> io::Result<DataLease> {
    let result = (|| {
        let _publication = publication_lease(&paths.root)?;
        fs::create_dir_all(
            paths
                .root
                .parent()
                .ok_or_else(|| io::Error::other("mpmp: data directory has no parent"))?,
        )?;
        match migrate(paths, |source, target| {
            io::copy(&mut fs::File::open(source)?, target)
        })? {
            Some(lease) => Ok(lease),
            None => {
                fs::create_dir_all(&paths.root)?;
                runtime_lease(&paths.root)
            }
        }
    })();
    result.map_err(|error| {
        let kind = preparation_error_kind(&error);
        let message = match kind {
            PreparationErrorKind::OldDatabaseInUse => concat!(
"旧数据库正在使用，启动已取消。请关闭旧播放器后重试；旧数据仍保留。\nOld database in use; startup ",
"cancelled. Close the old player and retry; old data retained.",
),
            PreparationErrorKind::NameConflict => concat!(
"数据目录存在保留名称冲突，启动已取消；旧数据仍保留。请按以下路径说明处理冲突。\nReserved-name conflict; startup ",
"cancelled, old data retained. Resolve the conflicting path as described ",
"below.",
),
            PreparationErrorKind::Io => concat!(
"数据目录准备失败，启动已取消；旧数据仍保留。请检查下方 I/O 错误及目录权限。\nData preparation failed; ",
"startup cancelled, old data retained. Check the I/O error and directory ",
"permissions below.",
),
        };
        categorized(kind, format!("{message}\n\n{error}"))
    })
}

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncKind {
    File,
    Directory,
    PublishedParent,
}

fn sync_entry(kind: SyncKind, path: &Path, file: Option<&fs::File>) -> io::Result<()> {
    if let Some(file) = file {
        return file.sync_all();
    }
    #[cfg(unix)]
    {
        let _ = kind;
        fs::File::open(path)?.sync_all()
    }
    #[cfg(windows)]
    {
        // File::sync_all calls FlushFileBuffers on writable FILE handles. Win32 does
        // not support that barrier on ordinary directory handles. Publication below
        // uses same-volume MoveFileExW(MOVEFILE_WRITE_THROUGH), without copy fallback.
        tracing::debug!(?kind,
         directory = %path.display(),

        "directory fsync unsupported on Windows; write-through rename used"
        );

        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (kind, path);
        Ok(())
    }
}

fn migrate(
    paths: &DataPaths,
    copy: impl FnMut(&Path, &mut fs::File) -> io::Result<u64>,
) -> io::Result<Option<DataLease>> {
    migrate_synced(paths, copy, sync_entry)
}

fn migrate_synced(
    paths: &DataPaths,
    copy: impl FnMut(&Path, &mut fs::File) -> io::Result<u64>,
    sync: impl FnMut(SyncKind, &Path, Option<&fs::File>) -> io::Result<()>,
) -> io::Result<Option<DataLease>> {
    migrate_with_ops(paths, copy, sync, publish)
}

fn migrate_with_ops(
    paths: &DataPaths,
    mut copy: impl FnMut(&Path, &mut fs::File) -> io::Result<u64>,
    mut sync: impl FnMut(SyncKind, &Path, Option<&fs::File>) -> io::Result<()>,
    mut rename: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> io::Result<Option<DataLease>> {
    if exists(&paths.root)? {
        if exists(&paths.root.join(IN_PROGRESS))? {
            let old = paths
                .legacy
                .as_ref()
                .map(|p| p.display().to_string())
                .or_else(|| {
                    let marker = paths.root.join(IN_PROGRESS);
                    let metadata = fs::symlink_metadata(&marker).ok()?;
                    (metadata.is_file() && !linked(&metadata))
                        .then(|| fs::read_to_string(marker).ok())
                        .flatten()
                })
                .unwrap_or_else(|| "(see migration marker / 见迁移标记)".into());
            return Err(io::Error::other(format!(
                concat!(
                    "mpmp: incomplete migration; startup refused. New directory: {}. Old ",
                    "directory: {}. Close all players, move the NEW directory aside, then ",
                    "restart with the original data-path configuration to retry. Keep the OLD ",
                    "directory unchanged. ",
                    "迁移尚未完成，拒绝启动。新目录：{}；旧目录：{}。请关闭所有播放器，将新目录移到其他位置，再使用原数据路径配置重新启动以重试；不要修改或删除旧目录。",
                ),
                paths.root.display(),
                old,
                paths.root.display(),
                old
            )));
        }
        tracing::info!("mpmp: existing data directory selected; migration skipped");
        return Ok(None);
    }
    let Some(old) = &paths.legacy else {
        return Ok(None);
    };
    if !exists(old)? {
        return Ok(None);
    }
    regular_directory(old)?;
    preflight_names(old)?;
    let mut database = legacy_database(old)?;
    let staging = tempfile::Builder::new()
        .prefix(".mpmp-migration-")
        .tempdir_in(paths.root.parent().unwrap())?;
    // The external publication lease protects copying, publication and rollback.
    // No handle inside the moving tree may remain open during either rename.
    // Acquire its runtime lease only after publication barriers, before handoff.
    let mut lease = None;
    let mut published = false;
    let result = (|| {
        copy_tree(
            old,
            staging.path(),
            old,
            &mut database,
            &mut copy,
            &mut sync,
        )?;
        // This marker moves WITH the tree, survives a failed rollback, and blocks
        // every later prepare until the operator moves the incomplete copy aside.
        let progress_path = staging.path().join(IN_PROGRESS);
        let mut progress = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&progress_path)?;
        use io::Write;
        progress.write_all(old.to_string_lossy().as_bytes())?;
        sync(SyncKind::File, &progress_path, Some(&progress))?;
        drop(progress);
        let notice_path = staging.path().join(NOTICE);
        let mut notice = fs::File::create(&notice_path)?;
        notice.write_all(old.to_string_lossy().as_bytes())?;
        sync(SyncKind::File, &notice_path, Some(&notice))?;
        drop(notice);
        fs::set_permissions(staging.path(), fs::metadata(old)?.permissions())?;
        sync(SyncKind::Directory, staging.path(), None)?;
        if exists(&paths.root)? {
            return Err(io::Error::other(
                "mpmp: destination appeared during migration",
            ));
        }
        rename(staging.path(), &paths.root)?;
        published = true;
        sync(
            SyncKind::PublishedParent,
            paths.root.parent().unwrap(),
            None,
        )?;
        lease = Some(runtime_lease(&paths.root)?);
        // All data and publication barriers succeeded. Remove the guard LAST.
        // No fallible sync follows deletion: a crash that resurrects this directory
        // entry only causes a conservative recovery refusal, never partial startup.
        fs::remove_file(paths.root.join(IN_PROGRESS))
    })();
    if let Err(error) = result {
        // Close every descendant handle before rollback or cleanup on Windows.
        // The external publication lease still excludes every other prepare().
        drop(lease.take());
        if published && let Err(rollback) = rename(&paths.root, staging.path()) {
            return Err(io::Error::other(format!(
                "{error}; cannot roll back publication: {rollback}; startup cancelled"
            )));
        }
        let cleanup = make_removable(staging.path()).and_then(|()| staging.close());
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(io::Error::other(format!(
                "{error}; migration temporary directory cleanup failed: {cleanup}"
            ))),
        };
    }
    tracing::info!(source = %old.display(),
     destination = %paths.root.display(),
     "mpmp: migration completed and synced; old data retained");

    Ok(lease)
}

fn publish(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
        let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<_> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: valid terminated paths; no REPLACE_EXISTING or COPY_ALLOWED flags.
        if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(source, target)
    }
}

/// Only explicit provenance establishes ownership of a regenerable profile.
/// Old unmarked WebKit namespaces may contain user storage: copy them normally.
fn skipped(
    path: &Path,
    relative: &Path,
    metadata: &fs::Metadata,
) -> io::Result<Option<&'static str>> {
    if linked(metadata) {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if relative == Path::new("run/mcp.sock") && metadata.file_type().is_socket() {
            return Ok(Some(
                "own MCP runtime socket (agent-mcp endpoint::host_path)",
            ));
        }
    }
    if relative == Path::new("webview") && owned_webview(path, metadata)? {
        return Ok(Some(
            "owned regenerable WebView profile (prepare_webview_cache marker)",
        ));
    }
    Ok(None)
}

fn owned_webview(path: &Path, metadata: &fs::Metadata) -> io::Result<bool> {
    if !metadata.is_dir() || linked(metadata) {
        return Ok(false);
    }
    let marker = path.join(WEBVIEW_OWNER);
    match fs::symlink_metadata(&marker) {
        Ok(m) if m.is_file() && !linked(&m) && m.len() == WEBVIEW_SIGNATURE.len() as u64 => {
            Ok(fs::read(marker)? == WEBVIEW_SIGNATURE)
        }
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(false),
    }
}

/// Inspect only the old root, before opening its DB or creating/copying a staging
/// tree. Existing targets and explicit data overrides do not enter migration.
fn preflight_names(old: &Path) -> io::Result<()> {
    for name in [IN_PROGRESS, NOTICE, ".mpmp.lock"] {
        let path = old.join(name);
        if exists(&path)? {
            return Err(name_conflict(old, &path));
        }
    }
    let path = old.join("webview");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            let owned = owned_webview(&path, &metadata).map_err(|error| {
                categorized(
                    PreparationErrorKind::NameConflict,
                    format!("{} ({error})", name_conflict(old, &path)),
                )
            })?;
            if !owned {
                return Err(name_conflict(old, &path));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

fn name_conflict(old: &Path, path: &Path) -> io::Error {
    categorized(
        PreparationErrorKind::NameConflict,
        format!(
            concat!(
                "路径 {} 与 mpmp 保留的名称冲突，迁移尚未开始。请先关闭所有播放器，将旧目录中的该项改名或移走，再重新启动；处理冲突后，也可以设置 ",
                "PLAYER_DATA_DIR={} 继续使用原来的目录，不进行迁移。\nmpmp: reserved-name conflict: {}. ",
                "Migration has not started. Close all players, rename or move this item in ",
                "the OLD directory, then restart. After resolving the conflict, you can ",
                "also set PLAYER_DATA_DIR={} to keep using the old directory instead of ",
                "migrating.",
            ),
            path.display(),
            old.display(),
            path.display(),
            old.display()
        ),
    )
}

fn regular_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || linked(&metadata) {
        return Err(io::Error::other(format!(
            "mpmp: linked or special directory cannot be migrated: {}",
            path.display()
        )));
    }
    Ok(())
}

fn linked(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT, including junctions.
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn copy_tree(
    source: &Path,
    target: &Path,
    old: &Path,
    database: &mut Option<fs::File>,
    copy: &mut impl FnMut(&Path, &mut fs::File) -> io::Result<u64>,
    sync: &mut impl FnMut(SyncKind, &Path, Option<&fs::File>) -> io::Result<()>,
) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if let Some(reason) = skipped(
            &path,
            path.strip_prefix(old).expect("inside legacy root"),
            &metadata,
        )? {
            tracing::info!(relative_path = %path.strip_prefix(old).unwrap().display(),
             reason,
             "mpmp: migration skipped regenerable content; source retained");

            continue;
        }
        let dest = target.join(entry.file_name());
        if linked(&metadata) {
            return Err(io::Error::other(format!(
                "mpmp: cannot migrate a link: {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            fs::create_dir(&dest)?;
            copy_tree(&path, &dest, old, database, copy, sync)?;
        } else if metadata.is_file() {
            // Keep a writable destination handle through chmod, including read-only
            // copies on Windows: FlushFileBuffers requires GENERIC_WRITE access.
            let mut out = fs::File::create(&dest)?;
            if path == old.join("library.db")
                && let Some(db) = database.as_mut()
            {
                // Do not open/close another DB fd: POSIX record locks belong to the process.
                io::copy(db, &mut out)?;
            } else {
                copy(&path, &mut out)?;
            }
            fs::set_permissions(&dest, metadata.permissions())?;
            sync(SyncKind::File, &dest, Some(&out))?;
        } else {
            return Err(io::Error::other(format!(
                "mpmp: cannot migrate a socket or special file: {}",
                path.display()
            )));
        }
    }
    // Set directory permissions only after all children are written.
    if source != old {
        fs::set_permissions(target, fs::metadata(source)?.permissions())?;
        sync(SyncKind::Directory, target, None)?;
    }
    Ok(())
}

fn make_removable(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let mut permissions = metadata.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.is_dir() {
            permissions.set_mode(permissions.mode() | 0o700);
        }
    }
    // Windows: clears FILE_ATTRIBUTE_READONLY on our own temporary copy so it can be removed.
    // The lint guards against Unix world-writable modes, which this branch never builds.
    #[cfg(windows)]
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            make_removable(&entry?.path())?;
        }
    }
    Ok(())
}

/// SQLite's byte-range locks also detect pre-mpmp players, which had no startup lease.
/// No SQLite connection is opened: no WAL recovery/checkpoint or writes to the old tree.
fn legacy_database(old: &Path) -> io::Result<Option<fs::File>> {
    let path = old.join("library.db");
    if !exists(&path)? {
        return Ok(None);
    }
    let meta = fs::symlink_metadata(&path)?;
    if !meta.is_file() || linked(&meta) {
        return Err(io::Error::other(
            "mpmp: legacy database is not a regular file",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.write(true); // Required for F_WRLCK; never write bytes.
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0); // Reject any old SQLite connection, including idle ones.
    }
    let file = options.open(&path);
    #[cfg(windows)]
    let file = file.map_err(|error| {
        if matches!(error.raw_os_error(), Some(32 | 33)) {
            return categorized(
                PreparationErrorKind::OldDatabaseInUse,
                format!(
                    "旧数据库被占用 / Old database in use: {}: {error}",
                    path.display()
                ),
            );
        }
        error
    });
    let file = file?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock is a plain C struct; the fd and pointer stay valid for this call.
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = libc::F_WRLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        // Whole DB includes SQLite's SHARED/RESERVED/PENDING locking bytes, even in WAL.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } == -1 {
            let error = io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::EACCES | libc::EAGAIN)) {
                return Err(categorized(
                    PreparationErrorKind::OldDatabaseInUse,
                    format!(
                        "旧数据库被占用 / Old database in use: {}: {error}",
                        path.display()
                    ),
                ));
            }
            return Err(error);
        }
    }
    Ok(Some(file))
}

/// Consume only from the authenticated host diagnostics page, never the theme gateway.
pub fn take_migration_notice(root: &Path) -> io::Result<Option<String>> {
    let path = root.join(NOTICE);
    match fs::read_to_string(&path) {
        Ok(source) => {
            fs::remove_file(path)?;
            Ok(Some(source))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
pub fn home_from_user_database() -> Option<PathBuf> {
    use std::{
        ffi::{CStr, OsStr},
        os::unix::ffi::OsStrExt,
    };
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let mut size = 1024;
    loop {
        let mut buf = vec![0 as libc::c_char; size];
        // SAFETY: passwd is plain data; zero is a valid initial value.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found = std::ptr::null_mut();
        // SAFETY: pointers refer to valid storage for the duration of the call.
        let rc =
            unsafe { libc::getpwuid_r(uid, &mut entry, buf.as_mut_ptr(), buf.len(), &mut found) };
        if rc == libc::ERANGE && size < 1 << 20 {
            size *= 2;
            continue;
        }
        if rc != 0 || found.is_null() || entry.pw_dir.is_null() {
            return None;
        }
        // SAFETY: a successful call returned a NUL-terminated string inside live buf.
        let home = PathBuf::from(OsStr::from_bytes(
            unsafe { CStr::from_ptr(entry.pw_dir) }.to_bytes(),
        ));
        return home.is_absolute().then_some(home);
    }
}

#[cfg(not(unix))]
pub fn home_from_user_database() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests;
