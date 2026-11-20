//! Real process termination at SQLite's journal commit barriers, without sockets.
use super::tests::{backups, development, header, open, snapshot, storage};
use super::{APPLICATION_ID, fingerprint, reset};
use rusqlite::{Connection, ffi};
use std::{
    collections::HashMap,
    ffi::{CStr, c_char, c_int},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

static ARMED: AtomicBool = AtomicBool::new(false);
// Only used in the isolated child. The actual OS sqlite3_file stays at its original
// address; all unmodified I/O methods therefore receive their native file layout.
#[derive(Clone, Copy)]
struct Methods {
    original: usize,
    wrapper: usize,
    wal: bool,
}
static FILES: OnceLock<Mutex<HashMap<usize, Methods>>> = OnceLock::new();
static ORIGINAL_VFS: OnceLock<usize> = OnceLock::new();
fn files() -> &'static Mutex<HashMap<usize, Methods>> {
    FILES.get_or_init(Default::default)
}
fn pause_commit() {
    std::fs::write(
        std::env::var_os("MPMP_CRASH_READY").unwrap(),
        b"commit barrier",
    )
    .unwrap();
    loop {
        std::thread::sleep(Duration::from_millis(50));
    }
}
unsafe extern "C" fn sync(file: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    let methods = files().lock().unwrap()[&(file as usize)];
    if methods.wal && ARMED.load(Ordering::SeqCst) {
        pause_commit();
    }
    // SAFETY: xOpen retained the native methods for this live SQLite file.
    unsafe {
        (*(methods.original as *const ffi::sqlite3_io_methods))
            .xSync
            .unwrap()(file, flags)
    }
}
unsafe extern "C" fn close(file: *mut ffi::sqlite3_file) -> c_int {
    let methods = files().lock().unwrap()[&(file as usize)];
    // SAFETY: the wrapper belongs to this file and stays alive until native close returns.
    let result = unsafe {
        (*(methods.original as *const ffi::sqlite3_io_methods))
            .xClose
            .unwrap()(file)
    };
    files().lock().unwrap().remove(&(file as usize));
    // SAFETY: allocated once in xOpen and removed once by this xClose.
    unsafe {
        drop(Box::from_raw(
            methods.wrapper as *mut ffi::sqlite3_io_methods,
        ));
    }
    result
}
unsafe extern "C" fn open_file(
    _: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    let vfs = *ORIGINAL_VFS.get().unwrap() as *mut ffi::sqlite3_vfs;
    // SAFETY: SQLite supplies valid xOpen arguments; delegate with the native VFS.
    let result = unsafe { (*vfs).xOpen.unwrap()(vfs, name, file, flags, out) };
    if result == ffi::SQLITE_OK {
        // SAFETY: successful native xOpen initialized the live file's methods.
        unsafe {
            let original = (*file).pMethods;
            let mut methods = Box::new(*original);
            methods.xSync = Some(sync);
            methods.xClose = Some(close);
            let wrapper = Box::into_raw(methods);
            files().lock().unwrap().insert(
                file as usize,
                Methods {
                    original: original as usize,
                    wrapper: wrapper as usize,
                    wal: flags & ffi::SQLITE_OPEN_WAL != 0,
                },
            );
            (*file).pMethods = wrapper;
        }
    }
    result
}
unsafe extern "C" fn delete_file(
    _: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    sync_dir: c_int,
) -> c_int {
    // SAFETY: SQLite's xDelete name is a live NUL-terminated filename.
    if ARMED.load(Ordering::SeqCst)
        && unsafe { CStr::from_ptr(name) }
            .to_bytes()
            .ends_with(b"-journal")
    {
        pause_commit();
    }
    let vfs = *ORIGINAL_VFS.get().unwrap() as *mut ffi::sqlite3_vfs;
    // SAFETY: forward unchanged arguments to the native VFS.
    unsafe { (*vfs).xDelete.unwrap()(vfs, name, sync_dir) }
}
fn install_vfs() {
    // SAFETY: only called before any connection in this single-test child. Native
    // VFS and our leaked descriptor live until process termination.
    unsafe {
        let original = ffi::sqlite3_vfs_find(std::ptr::null());
        assert!(!original.is_null());
        ORIGINAL_VFS.set(original as usize).unwrap();
        let mut wrapper = Box::new(*original);
        wrapper.zName = c"mpmp-crash-test".as_ptr();
        wrapper.pNext = std::ptr::null_mut();
        wrapper.xOpen = Some(open_file);
        wrapper.xDelete = Some(delete_file);
        assert_eq!(
            ffi::sqlite3_vfs_register(Box::into_raw(wrapper), 1),
            ffi::SQLITE_OK
        );
    }
}
fn child_command(name: &str, path: &Path) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", name, "--ignored", "--nocapture"])
        .env("MPMP_CHILD_DATABASE", path)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    cmd
}
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
#[ignore = "subprocess helper, invoked by the two real crash regressions"]
fn commit_crash_child() {
    let path = std::env::var_os("MPMP_CHILD_DATABASE").unwrap();
    install_vfs();
    let _hook = super::tests::hook(|phase, _| {
        if phase == "before_commit" {
            ARMED.store(true, Ordering::SeqCst);
        }
        Ok(())
    });
    reset::prepare(Path::new(&path), &storage(), true).unwrap();
    panic!("commit did not reach the armed VFS barrier");
}
fn crash_during_commit(mode: &str) {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = development(&path);
    conn.pragma_update(None, "journal_mode", mode).unwrap();
    // Small synthetic content remains unchanged by normal startup as well.
    conn.execute("INSERT INTO meta VALUES('crash_evidence',99)", [])
        .unwrap();
    let before = snapshot(&conn);
    drop(conn);
    let ready = dir.path().join("ready");
    let mut child = KillOnDrop(
        child_command("db::schema::crash_tests::commit_crash_child", &path)
            .env("MPMP_CRASH_READY", &ready)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "child exited before commit barrier"
        );
        assert!(
            Instant::now() < deadline,
            "child did not reach commit barrier"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    let retained = backups(dir.path());
    assert_eq!(retained.len(), 1);
    super::tests::assert_backups(dir.path(), &before);
    if mode == "DELETE" {
        assert!(path.with_extension("db-journal").exists());
        let probe =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let error = probe
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap_err();
        assert!(
            matches!(error, rusqlite::Error::SqliteFailure(code, _)
            if code.extended_code == ffi::SQLITE_READONLY_ROLLBACK
                || code.extended_code == ffi::SQLITE_READONLY_RECOVERY),
            "{error}"
        );
        drop(probe);
    } else {
        let conn =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert!(matches!(header(&conn), (13, 0) | (1, APPLICATION_ID)));
    }
    // migrate=false may perform only SQLite recovery, then must refuse the
    // recovered v13 without another backup or either application header write.
    if mode == "DELETE" {
        let error = reset::prepare(&path, &storage(), false).err().unwrap();
        assert!(error.to_string().contains("--allow-migrate"));
        let conn = Connection::open(&path).unwrap();
        assert_eq!(header(&conn), (13, 0));
        assert_eq!(snapshot(&conn), before);
        assert_eq!(backups(dir.path()).len(), 1);
    }
    // Preparation must recover SQLite's journal, then recognize/reset in full.
    drop(reset::prepare(&path, &storage(), true).unwrap());
    let conn = Connection::open(&path).unwrap();
    assert_eq!(header(&conn), (1, APPLICATION_ID));
    assert_eq!(snapshot(&conn), before);
    assert_eq!(
        conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(fingerprint::schema(&conn).unwrap(), super::SCHEMA_SHA256);
    assert!(retained[0].exists());
    super::tests::assert_backups(dir.path(), &before);
    drop(conn);
    drop(open(&path, true).unwrap());
    assert!(
        crate::Library::take_upgrade_notice(dir.path())
            .unwrap()
            .is_some()
    );
}
#[test]
fn real_delete_commit_crash_recovers_hot_journal_and_reopens() {
    crash_during_commit("DELETE");
}
#[test]
fn real_wal_commit_crash_recovers_and_reopens() {
    crash_during_commit("WAL");
}

#[cfg(unix)]
#[test]
#[ignore = "subprocess helper for POSIX database lock regression"]
fn posix_observer_child() {
    let path = std::env::var_os("MPMP_CHILD_DATABASE").unwrap();
    let conn = Connection::open(path).unwrap();
    assert_eq!(
        conn.query_row("SELECT value FROM meta WHERE key='parent'", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        std::env::var("MPMP_EXPECT_PARENT")
            .unwrap()
            .parse::<i64>()
            .unwrap()
    );
    conn.execute("INSERT OR REPLACE INTO meta VALUES('child',8)", [])
        .unwrap();
}

#[cfg(unix)]
#[test]
fn failed_second_open_preserves_live_sqlite_posix_locks_and_wal_visibility() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let live = open(&path, true).unwrap();
    live.inner
        .writer
        .run(|conn| {
            conn.pragma_update(None, "user_version", 13)?;
            conn.pragma_update(None, "application_id", 0)?;
            Ok(())
        })
        .unwrap();
    let error = open(&path, true).err().unwrap();
    assert!(error.to_string().contains("Stop other library writers"));
    drop(error);
    live.inner
        .writer
        .run(|conn| {
            conn.pragma_update(None, "application_id", APPLICATION_ID)?;
            conn.pragma_update(None, "user_version", 1)?;
            conn.execute("INSERT INTO meta VALUES('parent',7)", [])?;
            Ok(())
        })
        .unwrap();
    let observe = |expected: &str| {
        assert!(
            child_command("db::schema::crash_tests::posix_observer_child", &path)
                .env("MPMP_EXPECT_PARENT", expected)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            path.with_extension("db-wal").exists(),
            "another process deleted a live writer's WAL"
        );
        assert_eq!(
            live.inner
                .pool
                .read(|conn| Ok(conn.query_row(
                    "SELECT value FROM meta WHERE key='child'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?))
                .unwrap(),
            8
        );
    };
    observe("7");
    live.inner
        .writer
        .run(|conn| {
            conn.execute("UPDATE meta SET value=9 WHERE key='parent'", [])?;
            Ok(())
        })
        .unwrap();
    observe("9");
}
