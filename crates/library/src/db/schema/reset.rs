//! The only development transition: verified v13 -> published 1, two header fields.
use super::{
    APPLICATION_ID, LATEST_VERSION, SCHEMA_SHA256,
    fingerprint::{self, Data},
    v1,
};
use crate::{LibraryError, Result, db::with_busy_timeout};
use mpmp_storage::{FileIdentity, StorageBackends};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

pub(crate) const RESET_NOTICE: &str = "library-schema-reset-notice";
const PENDING: &str = "library-schema-reset-pending";

fn refused(detail: impl std::fmt::Display) -> LibraryError {
    LibraryError::Io(std::io::Error::other(detail.to_string()))
}
fn early(detail: impl std::fmt::Display) -> LibraryError {
    refused(format!(
        concat!(
            "检测到早期开发版v13，结构或人物身份键不符合重标基线；未修改数据库。请用最后开发版在副本上完成修复并验证，再重试；不要手动改user_version。原因：{detail}。 ",
            "Early development v13 does not match the schema reset baseline; database unchanged. ",
            "Repair and verify a copy with the last development build, then retry; do not edit user_version. Detail: {detail}"
        ),
        detail = detail
    ))
}

#[derive(Debug, PartialEq, Eq)]
enum State {
    Empty,
    Published,
    Development,
}

fn header(conn: &Connection) -> Result<(i64, i64)> {
    Ok((
        conn.pragma_query_value(None, "user_version", |r| r.get(0))?,
        conn.pragma_query_value(None, "application_id", |r| r.get(0))?,
    ))
}

fn recognize(conn: &Connection) -> Result<State> {
    let (version, app) = header(conn)?;
    if app == APPLICATION_ID && version > LATEST_VERSION {
        return Err(LibraryError::SchemaTooNew {
            found: version,
            supported: LATEST_VERSION,
        });
    }
    if app == 0 && (1..=12).contains(&version) {
        return Err(refused(format!(
            concat!(
                "检测到旧开发版数据库 schema v{version}。此首次公开版本只支持新 schema 1，或结构符合要求的开发版 v13；不支持直接打开 v12 及更早开发库。数据库未修改。请使用保留的开发版程序在数据副本上升级到当前 v13并验证，或选择新的数据目录。不要手动修改 user_version。 ",
                "Development schema v{version} is unsupported. This first public build supports published schema 1 or baseline development v13 only. Database unchanged. Upgrade and verify a copy with the retained development build, or select a new data directory. Do not edit user_version."
            ),
            version = version
        )));
    }
    if version == 0
        && app == 0
        && conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| {
            r.get::<_, i64>(0)
        })? == 0
    {
        return Ok(State::Empty);
    }
    if !((version == 13 && app == 0) || (version == 1 && app == APPLICATION_ID)) {
        return Err(if version == 13 {
            early(format!("unexpected application_id={app}"))
        } else {
            refused(format!(
                "数据库归属或版本不受支持；未修改数据库。 Unsupported database identity/version; unchanged: user_version={version}, application_id={app}"
            ))
        });
    }
    validate(conn, version == 13).map_err(|error| {
        if version == 13 && !error.is_damaged() && !recovery_required(&error) {
            early(error)
        } else {
            error
        }
    })?;
    Ok(if version == 13 {
        State::Development
    } else {
        State::Published
    })
}

fn validate(conn: &Connection, development: bool) -> Result<()> {
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(LibraryError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some(check),
        )));
    }
    let actual = fingerprint::schema(conn)?;
    if actual != SCHEMA_SHA256 {
        // Include exact object differences, not just an opaque hash mismatch.
        let baseline = with_busy_timeout(Connection::open_in_memory())?;
        baseline.execute_batch(&v1())?;
        let objects = |c: &Connection| -> Result<std::collections::BTreeSet<(String, String)>> {
            const OBJECTS: &str = concat!(
                "SELECT name,coalesce(sql,'NULL') FROM sqlite_schema ",
                "WHERE name NOT LIKE 'sqlite_%'"
            );
            Ok(c.prepare(OBJECTS)?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        };
        let expected = objects(&baseline)?;
        let found = objects(conn)?;
        return Err(refused(format!(
            "schema fingerprint {actual}; missing/changed: {:?}; extra/changed: {:?}",
            expected.difference(&found).collect::<Vec<_>>(),
            found.difference(&expected).collect::<Vec<_>>()
        )));
    }
    // The required public fingerprint excludes sqlite_% objects. Pin automatic
    // indexes separately, including their NULL SQL and owning table.
    const AUTOMATIC_SQL: &str = concat!(
        "SELECT type,name,tbl_name FROM sqlite_schema ",
        "WHERE name LIKE 'sqlite_%' AND sql IS NULL ORDER BY type,name"
    );
    let automatic = conn
        .prepare(AUTOMATIC_SQL)?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let expected = [
        (
            "index",
            "sqlite_autoindex_album_loudness_1",
            "album_loudness",
        ),
        (
            "index",
            "sqlite_autoindex_person_artists_2",
            "person_artists",
        ),
        ("index", "sqlite_autoindex_roots_1", "roots"),
        ("index", "sqlite_autoindex_track_artists_2", "track_artists"),
    ]
    .map(|(kind, name, table)| (kind.to_owned(), name.to_owned(), table.to_owned()));
    if automatic != expected {
        return Err(refused(format!(
            "unknown or changed automatic schema objects: {automatic:?}"
        )));
    }
    let internal = conn
        .prepare(
            "SELECT name,sql FROM sqlite_schema WHERE name LIKE 'sqlite_%' AND sql IS NOT NULL",
        )?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (name, sql) in internal {
        let allowed = match name.as_str() {
            "sqlite_sequence" => sql == "CREATE TABLE sqlite_sequence(name,seq)",
            "sqlite_stat1" => sql == "CREATE TABLE sqlite_stat1(tbl,idx,stat)",
            "sqlite_stat4" => {
                sql == "CREATE TABLE sqlite_stat4(tbl,idx,neq,nlt,ndlt,sample)"
                    && conn.query_row(
                        "SELECT sqlite_compileoption_used('ENABLE_STAT4')",
                        [],
                        |r| r.get::<_, bool>(0),
                    )?
            }
            _ => false,
        };
        if !allowed {
            return Err(refused(format!(
                "unknown internal schema object: {name}: {sql}"
            )));
        }
    }
    if !development {
        return Ok(());
    }
    let broken = conn
        .prepare("PRAGMA foreign_key_check")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !broken.is_empty() {
        return Err(refused(format!("broken references: {broken:?}")));
    }
    let mut stmt = conn.prepare("SELECT key,name_norm FROM person_artists")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let norm: String = row.get(1)?;
        if norm.is_empty()
            || crate::text::normalize(&norm) != norm
            || key != crate::person::person_key(&norm)
        {
            return Err(refused(format!("person identity key mismatch: {key}")));
        }
    }
    let mut stmt = conn.prepare("SELECT artist_key,name FROM track_artists")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let name: String = row.get(1)?;
        if key != crate::person::person_key(&crate::text::normalize(&name)) {
            return Err(refused("person membership identity mismatch"));
        }
    }
    let bad_order: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM track_artists GROUP BY track_id HAVING min(ordinal)<>0 OR max(ordinal)+1<>count(*))",
        [], |r| r.get(0))?;
    if bad_order {
        return Err(refused("person membership ordinal mismatch"));
    }
    let mut stmt = conn.prepare("SELECT id,inverse FROM actions WHERE inverse IS NOT NULL")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if crate::journal::Inverse::decode(&row.get::<_, Vec<u8>>(1)?).is_none() {
            return Err(refused(format!(
                "unreadable action encoding: {}",
                row.get::<_, i64>(0)?
            )));
        }
    }
    // Relabeling a development database still requires its exact known encodings.
    // A published library can open with unreadable recovery rows left untouched.
    if development {
        let unknown: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM file_write_intents WHERE
              CASE family
              WHEN 'cover_edit' THEN format_version<>1
                OR stage NOT IN ('building','prepared','replaced','retained','finalized')
              WHEN 'tag_edit' THEN format_version NOT IN (1,2,3,4)
                OR stage NOT IN ('building','prepared','publishing','replaced',
                                 'retained','rolling_back','verified_external',
                                 'displacing','displaced','installing','rollback_installing','unreadable')
              ELSE 1 END)",
            [],
            |r| r.get(0),
        )?;
        if unknown {
            return Err(refused(
                "unknown file_write_intents family/format_version/stage; retained unchanged",
            ));
        }
        crate::cover::job::validate_reset_intents(conn)?;
        crate::tags::job::intents::validate(conn)?;
    }
    Ok(())
}

/// Shared for every live library writer; exclusive during development reset. This is
/// independent of desktop's data-directory lease and works with injected storage too.
pub(crate) struct Guard {
    lock: File,
    path: PathBuf,
    identity: Option<FileIdentity>,
}
impl Guard {
    pub(crate) fn check(&self, storage: &StorageBackends) -> Result<()> {
        if let Some(expected) = &self.identity {
            let found = path_identity(storage, &self.path)?;
            if !expected.eq(&found) {
                return Err(refused(
                    "数据库路径已更换；停止重标。 Database path identity changed; reset stopped",
                ));
            }
        }
        Ok(())
    }
    fn identity(&self, storage: &StorageBackends) -> Result<[u64; 2]> {
        Ok(path_identity(storage, &self.path)?.parts())
    }
}

pub(crate) fn prepare(
    path: &Path,
    storage: &StorageBackends,
    allow: bool,
) -> Result<(Guard, bool)> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let canonical = fs::canonicalize(parent)?.join(
        path.file_name()
            .ok_or_else(|| refused("missing database filename"))?,
    );
    let path = canonical.as_path();
    let lock_path = path.parent().unwrap().join(".library.maintenance.lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lock.try_lock_shared()
        .map_err(|e| refused(format!("曲库维护中。 Library maintenance is active: {e}")))?;
    let mut guard = Guard {
        lock,
        path: path.to_owned(),
        identity: None,
    };
    // symlink_metadata also sees dangling links. Never follow a database symlink.
    let exists = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(refused(
                "数据库路径不能是符号链接。 Database path must not be a symlink",
            ));
        }
        Ok(meta) if !meta.is_file() => return Err(refused("database is not a regular file")),
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let state = if exists {
        match probe(path) {
            Ok(state) => state,
            Err(error) if recovery_required(&error) => {
                exclusive(&guard)?;
                // Only SQLite performs recovery. No application PRAGMA or DDL writes.
                // A read forces hot-journal recovery; drop before full re-recognition.
                let recovery = with_busy_timeout(Connection::open_with_flags(
                    path,
                    OpenFlags::SQLITE_OPEN_READ_WRITE,
                ))?;
                recovery.query_row("SELECT count(*) FROM sqlite_schema", [], |r| {
                    r.get::<_, i64>(0)
                })?;
                drop(recovery);
                let state = probe(path)?;
                guard.lock.unlock()?;
                guard
                    .lock
                    .try_lock_shared()
                    .map_err(|e| refused(format!("Library maintenance is active: {e}")))?;
                state
            }
            Err(error) => return Err(error),
        }
    } else {
        State::Empty
    };
    if state == State::Development && !allow {
        return Err(refused(
            "开发版 v13 需要允许重标；数据库未修改，未创建备份。 Development v13 requires schema reset; reopen with --allow-migrate after stopping the player. No backup or database changes were made.",
        ));
    }
    if state != State::Published {
        if state == State::Development {
            exclusive(&guard)?;
            // Snapshot only on the reset path. Never open a raw database File: on
            // Unix, closing it would release this process's SQLite fcntl locks.
            guard.identity = Some(path_identity(storage, path).map_err(|error| refused(format!(
                "存储后端不支持重标所需的数据库身份核验；未修改数据库。 \
                 Storage backend cannot verify database identity for schema reset; unchanged: {error}"
            )))?);
        }
        guard.check(storage)?;
        let mut conn = with_busy_timeout(Connection::open(path))?;
        if recognize(&conn)? != state {
            return Err(refused("database changed during recognition"));
        }
        if state == State::Empty {
            initialize(&mut conn)?;
        } else {
            reset(&mut conn, storage, &guard)?;
        }
        guard.check(storage)?;
        if state == State::Development {
            // Likewise, downgrade by unlocking first rather than overlapping locks.
            guard.lock.unlock()?;
            guard.lock.try_lock_shared().map_err(|error| refused(format!(
                "重标已完成；备份已保留，其他进程正在维护数据目录。 Reset completed; backup retained; another maintenance operation holds the directory: {error}"
            )))?;
        }
    }
    guard.check(storage)?;
    if allow && state == State::Published {
        recover_notice(&guard, storage)?;
    }
    Ok((guard, state == State::Development))
}

fn path_identity(storage: &StorageBackends, path: &Path) -> std::io::Result<FileIdentity> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(std::io::Error::other("database path is not a regular file"));
    }
    storage.files.entry_identity(path, &meta)
}
fn exclusive(guard: &Guard) -> Result<()> {
    // Windows cannot upgrade an overlapping range lock. Re-recognition after
    // acquiring this lock closes the gap left by releasing the shared probe lock.
    guard.lock.unlock()?;
    guard.lock.try_lock().map_err(|e| {
        refused(format!(
            "请停止其他曲库写入者。 Stop other library writers before reset/recovery: {e}"
        ))
    })?;
    Ok(())
}
fn probe(path: &Path) -> Result<State> {
    let conn = readonly(path)?;
    let tx = conn.unchecked_transaction()?;
    let state = recognize(&tx)?;
    tx.rollback()?;
    Ok(state)
}
fn recovery_required(error: &LibraryError) -> bool {
    matches!(error, LibraryError::Db(rusqlite::Error::SqliteFailure(code, _))
        if code.extended_code == rusqlite::ffi::SQLITE_READONLY_ROLLBACK
            || code.extended_code == rusqlite::ffi::SQLITE_READONLY_RECOVERY)
}

/// Exact production recognition, without locks, recovery, backup or file writes.
/// The caller must stop writers and supply an existing regular database file.
pub(crate) fn inspect(path: &Path) -> Result<&'static str> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(refused("database path must be an existing regular file"));
    }
    // A cleanly closed WAL database has neither WAL nor SHM. Even mode=ro
    // would create WAL while trying to initialize SHM, so immutable is required
    // for that quiescent snapshot. Never ignore a WAL that is actually present.
    let suffix = inspection_suffix(path)?;
    let absolute = std::path::absolute(path)?;
    let mut uri = String::from("file:");
    let mut bytes = absolute.as_os_str().as_encoded_bytes();
    if cfg!(windows) {
        if bytes.starts_with(br"\\?\UNC\") {
            uri.push_str("////");
            bytes = &bytes[8..];
        } else if bytes.starts_with(br"\\?\") {
            bytes = &bytes[4..];
        } else if bytes.starts_with(br"\\") {
            uri.push_str("//");
        }
    }
    for &byte in bytes {
        if cfg!(windows) && byte == b'\\' {
            uri.push('/');
        } else if byte.is_ascii_alphanumeric() || b"/:-_.".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(uri, "%{byte:02X}").unwrap();
        }
    }
    uri.push_str(suffix);
    let conn = with_busy_timeout(Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    ))?;
    let tx = conn.unchecked_transaction()?;
    let state = recognize(&tx)?;
    tx.rollback()?;
    Ok(match state {
        State::Empty => "empty",
        State::Published => "published schema 1",
        State::Development => "eligible development v13; schema reset required",
    })
}

fn inspection_suffix(path: &Path) -> Result<&'static str> {
    let sibling = |suffix: &str| -> std::io::Result<bool> {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        match fs::symlink_metadata(Path::new(&name)) {
            Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => Ok(true),
            Ok(_) => Err(std::io::Error::other(
                "SQLite sibling is not a regular file",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    };
    let wal = sibling("-wal")?;
    let shm = sibling("-shm")?;
    let journal = sibling("-journal")?;
    match (wal, shm, journal) {
        (false, false, false) => Ok("?mode=ro&immutable=1"),
        (true, true, false) => Ok("?mode=ro&readonly_shm=1"),
        _ => Err(refused(
            "副本包含 journal 或不完整的 WAL/SHM；请在正常退出后重新复制。 \
             Incomplete SQLite snapshot; exit normally and copy again before inspection.",
        )),
    }
}

/// Diagnostic for a stopped, disposable copy: reuse reset without writer startup.
pub(crate) fn relabel_copy(path: &Path, storage: &StorageBackends) -> Result<()> {
    if inspect(path)? != "eligible development v13; schema reset required" {
        return Err(refused(
            "relabel-only requires an eligible development v13 copy",
        ));
    }
    let (_guard, reset) = prepare(path, storage, true)?;
    if !reset {
        return Err(refused(
            "copy changed before relabel-only acquired maintenance ownership",
        ));
    }
    Ok(())
}

fn readonly(path: &Path) -> rusqlite::Result<Connection> {
    with_busy_timeout(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    ))
}

fn initialize(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if recognize(&tx)? != State::Empty {
        return Err(refused("database is no longer empty"));
    }
    tx.execute_batch(&v1())?;
    phase("initialized_schema", &tx)?;
    tx.pragma_update(None, "application_id", APPLICATION_ID)?;
    phase("initialized_marker", &tx)?;
    tx.pragma_update(None, "user_version", LATEST_VERSION)?;
    phase("initialized_version", &tx)?;
    validate(&tx, true)?;
    tx.commit()?;
    Ok(())
}

#[cfg(all(test, feature = "desktop-fs"))]
pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    initialize(conn)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Receipt {
    from_schema: i64,
    to_schema: i64,
    completed: bool,
    database: PathBuf,
    identity: [u64; 2],
    backup: PathBuf,
    schema: String,
    data: Data,
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn publish(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap();
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    sync_directory(parent)
}

/// Explicit close reports cleanup errors; tempfile's ordinary Drop cannot report them.
struct PartialBackup(Option<tempfile::NamedTempFile>);
impl std::ops::Deref for PartialBackup {
    type Target = tempfile::NamedTempFile;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for PartialBackup {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().unwrap()
    }
}
impl PartialBackup {
    fn persist(&mut self, path: &Path) -> std::io::Result<()> {
        match self.0.take().unwrap().persist_noclobber(path) {
            Ok(_) => Ok(()),
            Err(error) => {
                self.0 = Some(error.file);
                Err(error.error)
            }
        }
    }
}
impl Drop for PartialBackup {
    fn drop(&mut self) {
        if let Some(file) = self.0.take() {
            let path = file.path().to_owned();
            if let Err(error) = file.close() {
                tracing::warn!(
                    %error, path=%path.display(),
                    "cannot clean partial schema reset backup"
                );
            }
        }
    }
}

fn reset(conn: &mut Connection, storage: &StorageBackends, guard: &Guard) -> Result<()> {
    guard.check(storage)?;
    let path = &guard.path;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut wal_path = path.as_os_str().to_owned();
    wal_path.push("-wal");
    let required = fs::metadata(path)?
        .len()
        .saturating_add(fs::metadata(Path::new(&wal_path)).map_or(0, |m| m.len()))
        .saturating_mul(2);
    let available = storage.database.available_space(parent)?;
    if available < required {
        return Err(refused(format!(
            "空间不足；未重标。 insufficient free space: need {required} bytes (2 x database + WAL), available {available}"
        )));
    }
    phase("space_checked", conn)?;
    let before = {
        let tx = conn.transaction()?;
        if recognize(&tx)? != State::Development {
            return Err(refused("database changed before backup"));
        }
        let data = fingerprint::data(&tx)?;
        tx.rollback()?;
        data
    };
    let directory = parent.join("migration-backups");
    fs::create_dir_all(&directory)?;
    sync_directory(parent)?;
    let mut partial = PartialBackup(Some(
        tempfile::Builder::new()
            .prefix("library.v13-")
            .suffix(".partial")
            .tempfile_in(&directory)?,
    ));
    phase("before_vacuum", conn)?;
    let partial_name = partial.path().to_str().ok_or_else(|| {
        refused("备份路径不是有效 Unicode；未重标。 Backup path is not Unicode; database not reset")
    })?;
    conn.execute("VACUUM INTO ?1", [partial_name])?;
    phase("backup_written", conn)?;
    let verified = (|| -> Result<()> {
        let backup = readonly(partial.path())?;
        if recognize(&backup)? != State::Development
            || fingerprint::data(&backup)?.content != before.content
        {
            return Err(refused("backup header/schema/logical data mismatch"));
        }
        Ok(())
    })();
    // Damage to a backup must never classify the intact original as damaged.
    verified.map_err(|error| refused(format!(
        "备份校验失败；未重标，原库未修改。 Backup verification failed; original database not reset: {error}"
    )))?;
    phase("backup_verified", conn)?;
    phase("before_backup_sync", conn)?;
    partial.flush()?;
    partial.as_file().sync_all()?;
    let backup = partial.path().with_extension("sqlite");
    phase("before_backup_publish", conn)?;
    partial.persist(&backup)?;
    // Once published, a backup is retained even if this or a later barrier fails.
    sync_directory(&directory).map_err(|error| {
        refused(format!(
            "未重标；备份已保留。 Not reset; retained backup {}: {error}",
            backup.display()
        ))
    })?;
    tracing::warn!(path=%backup.display(), "verified schema reset backup retained");
    let receipt = Receipt {
        from_schema: 13,
        to_schema: 1,
        completed: false,
        database: fs::canonicalize(path)?,
        identity: guard.identity(storage)?,
        backup: fs::canonicalize(&backup)?,
        schema: SCHEMA_SHA256.into(),
        data: before,
    };
    let result = (|| -> Result<()> {
        guard.check(storage)?;
        phase("backup_persisted", conn)?;
        let encoded = serde_json::to_vec(&receipt).map_err(|e| refused(e.to_string()))?;
        publish(&parent.join(PENDING), &encoded)?;
        phase("pending_persisted", conn)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        guard.check(storage)?;
        if recognize(&tx)? != State::Development || fingerprint::data(&tx)? != receipt.data {
            return Err(refused(
                "备份后数据库被其他写入者修改；未重标。 Database changed after backup; not reset",
            ));
        }
        phase("transaction_checked", &tx)?;
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        phase("application_id_written", &tx)?;
        tx.pragma_update(None, "user_version", LATEST_VERSION)?;
        phase("version_written", &tx)?;
        validate(&tx, true)?;
        if header(&tx)? != (LATEST_VERSION, APPLICATION_ID)
            || fingerprint::data(&tx)? != receipt.data
        {
            return Err(refused(
                "pre-commit verification failed; transaction rolled back",
            ));
        }
        guard.check(storage)?;
        phase("before_commit", &tx)?;
        guard.check(storage)?;
        let commit = tx.commit();
        phase("sqlite_commit_returned", conn)?;
        // Resolve uncertain COMMIT by opening a fresh connection; never retry the writes.
        let verify = readonly(path)?;
        let state = recognize(&verify)?;
        if state == State::Published && fingerprint::data(&verify)? == receipt.data {
            guard.check(storage)?;
            if let Err(error) = commit {
                tracing::warn!(%error, "commit returned error but reset verified complete");
            }
        } else {
            commit?;
            return Err(refused("post-commit verification failed"));
        }
        Ok(())
    })();
    if let Err(error) = result {
        let state = readonly(path)
            .map_err(LibraryError::from)
            .and_then(|c| header(&c));
        let outcome = if matches!(state, Ok((1, APPLICATION_ID))) {
            "当前header已为发布1；提交后核验失败，不能宣称未重标。 \
             Header is published 1; post-commit verification failed; do not infer rollback. "
        } else {
            ""
        };
        let detail = format!(
            "{outcome}{error}; header(user_version,application_id)={state:?}; 已保留备份 / retained backup: {}",
            backup.display()
        );
        // Preserve corruption classification when verification of the ORIGINAL fails.
        if error.is_damaged()
            && let LibraryError::Db(rusqlite::Error::SqliteFailure(code, _)) = error
        {
            return Err(LibraryError::Db(rusqlite::Error::SqliteFailure(
                code,
                Some(detail),
            )));
        }
        return Err(refused(detail));
    }
    if let Err(error) = phase("after_commit", conn).and_then(|_| recover_notice(guard, storage)) {
        tracing::warn!(
            %error, path=%backup.display(),
            "schema reset completed; pending receipt retained for notice recovery"
        );
    }
    Ok(())
}

fn recover_notice(guard: &Guard, storage: &StorageBackends) -> Result<()> {
    let parent = guard
        .path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // Ownership precedes reading pending, including retries by concurrent opens.
    #[cfg(test)]
    phase("before_notice_lock", &readonly(&guard.path)?)?;
    let _notice_lock = notice_lock(parent)?;
    let pending = parent.join(PENDING);
    let bytes = match fs::read(&pending) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            tracing::warn!(%error, "cannot read pending schema reset receipt; retained for retry");
            return Ok(());
        }
    };
    let recover = || -> Result<()> {
        let mut receipt: Receipt =
            serde_json::from_slice(&bytes).map_err(|e| refused(e.to_string()))?;
        guard.check(storage)?;
        if receipt.from_schema != 13
            || receipt.to_schema != 1
            || receipt.database != fs::canonicalize(&guard.path)?
            || receipt.identity != guard.identity(storage)?
            || receipt.schema != SCHEMA_SHA256
        {
            return Err(refused("pending receipt database identity/schema mismatch"));
        }
        let current = readonly(&guard.path)?;
        if recognize(&current)? != State::Published {
            return Ok(());
        }
        let backup = readonly(&receipt.backup)?;
        if recognize(&backup)? != State::Development
            || fingerprint::data(&backup)?.content != receipt.data.content
        {
            return Err(refused("pending receipt backup verification failed"));
        }
        phase("before_notice", &current)?;
        receipt.completed = true;
        let encoded = serde_json::to_vec(&receipt).map_err(|e| refused(e.to_string()))?;
        publish(&pending, &encoded)?;
        phase("completed_receipt_persisted", &current)?;
        // Atomic pending -> notice transition: a crash cannot leave both receipts.
        fs::rename(&pending, parent.join(RESET_NOTICE))?;
        sync_directory(parent)?;
        Ok(())
    };
    if let Err(error) = recover() {
        tracing::warn!(%error, "cannot recover schema reset notice; pending evidence retained");
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    pub(super) static HOOK: std::cell::RefCell<Option<Box<ResetHook>>> =
        const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
type ResetHook = dyn Fn(&str, &Connection) -> Result<()>;
fn phase(name: &str, conn: &Connection) -> Result<()> {
    #[cfg(test)]
    HOOK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook(name, conn)
        } else {
            Ok(())
        }
    })?;
    let _ = (name, conn);
    Ok(())
}

/// Serializes receipt recovery and consumption on a separate file, never the DB.
pub(crate) fn notice_lock(parent: &Path) -> std::io::Result<File> {
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join(".library.schema-reset-notice.lock"))?;
    lock.lock()?;
    Ok(lock)
}

pub(crate) fn notice_backup(encoded: &str) -> std::io::Result<String> {
    let receipt: Receipt = serde_json::from_str(encoded)?;
    if !receipt.completed || receipt.from_schema != 13 || receipt.to_schema != 1 {
        return Err(std::io::Error::other(
            "not a completed schema reset receipt",
        ));
    }
    Ok(receipt.backup.to_string_lossy().into_owned())
}
