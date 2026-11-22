use super::*;

fn fixture() -> (tempfile::TempDir, DataPaths) {
    fixture_with(tempfile::tempdir().unwrap())
}

fn fixture_with(dir: tempfile::TempDir) -> (tempfile::TempDir, DataPaths) {
    let paths = DataPaths {
        root: dir.path().join("mpmp"),
        legacy: Some(dir.path().join(LEGACY_IDENTIFIER)),
    };
    (dir, paths)
}

fn seed(paths: &DataPaths) {
    let old = paths.legacy.as_ref().unwrap();
    fs::create_dir(old).unwrap();
    fs::write(old.join("state.json"), "old-state").unwrap();
}

#[test]
fn only_old_directory_migrates_and_notice_is_consumed_once() {
    let (_dir, paths) = fixture();
    seed(&paths);
    let _lease = prepare(&paths).unwrap();
    assert_eq!(
        fs::read(paths.root.join("state.json")).unwrap(),
        b"old-state"
    );
    assert_eq!(
        fs::read(paths.legacy.as_ref().unwrap().join("state.json")).unwrap(),
        b"old-state"
    );
    assert_eq!(
        take_migration_notice(&paths.root).unwrap(),
        Some(paths.legacy.unwrap().display().to_string())
    );
    assert_eq!(take_migration_notice(&paths.root).unwrap(), None);
}

#[test]
fn only_new_and_both_directories_use_new_without_copying() {
    for old in [false, true] {
        let (_dir, paths) = fixture();
        if old {
            seed(&paths);
            fs::write(
                paths.legacy.as_ref().unwrap().join(IN_PROGRESS),
                b"old user file",
            )
            .unwrap();
        }
        fs::create_dir(&paths.root).unwrap();
        fs::write(paths.root.join("state.json"), "new-state").unwrap();
        let _lease = prepare(&paths).unwrap();
        assert_eq!(
            fs::read(paths.root.join("state.json")).unwrap(),
            b"new-state"
        );
        assert_eq!(take_migration_notice(&paths.root).unwrap(), None);
    }
}

#[test]
fn empty_install_and_second_instance() {
    let (_dir, paths) = fixture();
    let lease = prepare(&paths).unwrap();
    assert!(paths.root.is_dir());
    assert!(prepare(&paths).is_err());
    drop(lease);
    assert!(prepare(&paths).is_ok());
}

#[test]
fn partial_copy_failure_removes_staging_and_never_publishes() {
    let (dir, paths) = fixture();
    seed(&paths);
    let result = migrate(&paths, |source, dest| {
        io::copy(&mut fs::File::open(source)?, dest)?;
        Err(io::Error::other("injected disk full after writing a file"))
    });
    assert!(result.is_err());
    assert!(!paths.root.exists());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    assert_eq!(
        fs::read(paths.legacy.unwrap().join("state.json")).unwrap(),
        b"old-state"
    );
    prepare(&paths_for_retry(dir.path())).unwrap();
}

fn paths_for_retry(parent: &Path) -> DataPaths {
    DataPaths {
        root: parent.join("mpmp"),
        legacy: Some(parent.join(LEGACY_IDENTIFIER)),
    }
}

#[test]
fn subdirectories_large_files_and_database_are_copied_intact() {
    let (_dir, paths) = fixture();
    seed(&paths);
    let old = paths.legacy.as_ref().unwrap();
    fs::create_dir_all(old.join("ui/themes/nested")).unwrap();
    let bytes: Vec<u8> = (0..9 * 1024 * 1024).map(|n| (n % 251) as u8).collect();
    fs::write(old.join("ui/themes/nested/large.bin"), &bytes).unwrap();
    fs::write(old.join("library.db"), &bytes).unwrap();
    fs::write(old.join("library.db-wal"), b"wal").unwrap();
    let _lease = prepare(&paths).unwrap();
    for root in [old, &paths.root] {
        assert_eq!(
            fs::read(root.join("ui/themes/nested/large.bin")).unwrap(),
            bytes
        );
        assert_eq!(fs::read(root.join("library.db")).unwrap(), bytes);
        assert_eq!(fs::read(root.join("library.db-wal")).unwrap(), b"wal");
    }
}

#[test]
fn platform_paths_and_override_priority() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let env = |key: &str| match key {
        "APPDATA" => Some(cwd.join("roaming").into_os_string()),
        "LOCALAPPDATA" => Some(cwd.join("local").into_os_string()),
        "HOME" => Some(cwd.join("home").into_os_string()),
        "XDG_DATA_HOME" => Some(cwd.join("xdg").into_os_string()),
        _ => None,
    };
    for (platform, base) in [
        (Platform::Windows, "roaming"),
        (Platform::Macos, "home/Library/Application Support"),
        (Platform::Linux, "xdg"),
    ] {
        let paths = DataPaths::resolve(platform, env, cwd).unwrap();
        assert_eq!(paths.root, cwd.join(base).join("mpmp"));
        assert_eq!(
            paths.legacy.unwrap(),
            cwd.join(base).join(LEGACY_IDENTIFIER)
        );
    }
    let fallback = DataPaths::resolve(
        Platform::Linux,
        |k| (k == "HOME").then(|| cwd.join("home").into_os_string()),
        cwd,
    )
    .unwrap();
    assert_eq!(fallback.root, cwd.join("home/.local/share/mpmp"));
    let overridden = DataPaths::resolve(
        Platform::Windows,
        |k| match k {
            "PLAYER_DATA_DIR" => Some("isolation".into()),
            "PLAYER_MANUAL_DATA_DIR" => Some("unused".into()),
            _ => None,
        },
        cwd,
    )
    .unwrap();
    assert_eq!(overridden.root, cwd.join("isolation"));
    assert!(overridden.legacy.is_none());
    let manual = DataPaths::resolve(
        Platform::Linux,
        |k| (k == "PLAYER_MANUAL_DATA_DIR").then(|| "manual".into()),
        cwd,
    )
    .unwrap();
    assert_eq!(manual.root, cwd.join("manual"));
    assert!(manual.legacy.is_none());
    assert!(DataPaths::resolve(Platform::Windows, |_| None, cwd).is_err());
}

#[cfg(windows)]
#[test]
fn windows_roaming_paths_are_absolute() {
    let paths = DataPaths::resolve(
        Platform::Windows,
        |k| (k == "APPDATA").then(|| r"C:\Users\me\AppData\Roaming".into()),
        Path::new(r"C:\work"),
    )
    .unwrap();
    assert_eq!(paths.root, Path::new(r"C:\Users\me\AppData\Roaming\mpmp"));
}

#[cfg(unix)]
#[test]
fn links_and_special_files_abort_without_publishing() {
    use std::os::unix::fs::symlink;
    let (_dir, paths) = fixture();
    seed(&paths);
    symlink("state.json", paths.legacy.as_ref().unwrap().join("linked")).unwrap();
    assert!(prepare(&paths).is_err());
    assert!(!paths.root.exists());
}

// A separate process is essential: POSIX record locks are per process.
const LOCK_CHILD_READY: &str = "MPMP_LOCK_TEST_READY";

#[test]
#[ignore = "child-process helper"]
fn legacy_lock_child() {
    use std::io::{Read, Write};

    let Some(root) = std::env::var_os("MPMP_LOCK_TEST_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let sqlite = std::env::var_os("MPMP_SQLITE_LOCK_TEST").map(|_| {
        let connection = rusqlite::Connection::open(root.join("library.db")).unwrap();
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        connection
            .execute_batch("CREATE TABLE IF NOT EXISTS example (value TEXT)")
            .unwrap();
        connection
    });
    let guard = if sqlite.is_none() {
        legacy_database(&root).unwrap()
    } else {
        None
    };
    // The parent waits for this flushed line, not a scheduling deadline. Keep
    // both locks alive until it closes stdin, including if the parent panics.
    println!("\n{LOCK_CHILD_READY}");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_to_end(&mut Vec::new()).unwrap();
    drop((sqlite, guard));
}

#[test]
fn old_database_in_use_blocks_migration_without_modifying_source() {
    check_legacy_in_use(false);
}

#[test]
fn idle_sqlite_wal_connection_blocks_migration_without_modifying_source() {
    check_legacy_in_use(true);
}

fn check_legacy_in_use(sqlite: bool) {
    use std::io::{BufRead, Read};
    use std::process::Stdio;

    let (_dir, paths) = fixture();
    seed(&paths);
    let old = paths.legacy.as_ref().unwrap();
    fs::write(
        old.join("library.db"),
        if sqlite {
            b"".as_slice()
        } else {
            b"database".as_slice()
        },
    )
    .unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "data::tests::legacy_lock_child",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("MPMP_LOCK_TEST_ROOT", old)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if sqlite {
        command.env("MPMP_SQLITE_LOCK_TEST", "1");
    }
    let mut child = command.spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut stderr = child.stderr.take().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    // Drain both streams concurrently, so neither a startup panic nor a full
    // output pipe can strand the parent waiting for readiness.
    let stdout = std::thread::spawn(move || -> io::Result<Vec<u8>> {
        let mut output = Vec::new();
        let mut line = Vec::new();
        while stdout.read_until(b'\n', &mut line)? != 0 {
            output.extend_from_slice(&line);
            if String::from_utf8_lossy(&line).trim() == LOCK_CHILD_READY {
                let _ = ready_tx.send(());
            }
            line.clear();
        }
        Ok(output)
    });
    let stderr = std::thread::spawn(move || -> io::Result<Vec<u8>> {
        let mut output = Vec::new();
        stderr.read_to_end(&mut output)?;
        Ok(output)
    });
    // EOF without the ready marker disconnects this channel and reports the
    // child's actual failure. Never probe/migrate a database still being opened.
    let ready = ready_rx.recv().is_ok();
    let read_database = || -> Vec<_> {
        ["library.db", "library.db-wal", "library.db-shm"]
            .iter()
            .map(|name| fs::read(old.join(name)).ok())
            .collect()
    };
    let before = ready.then(read_database);
    let result = ready.then(|| prepare(&paths));
    let after = ready.then(read_database);
    // Closing the pipe releases the child only after both snapshots were read.
    drop(stdin);
    let status = child.wait().unwrap();
    let stdout = stdout.join().unwrap().unwrap();
    let stderr = stderr.join().unwrap().unwrap();
    let diagnostics = format!(
        "ready={ready}, status={status}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    assert!(status.success() && ready, "{diagnostics}");
    let result = result.unwrap();
    assert!(
        result.is_err(),
        "migration must reject the live database; {diagnostics}"
    );
    assert_eq!(
        preparation_error_kind(result.as_ref().err().unwrap()),
        PreparationErrorKind::OldDatabaseInUse,
        "{diagnostics}"
    );
    assert_eq!(
        after, before,
        "migration must not write/checkpoint the old database; {diagnostics}"
    );
    assert!(!paths.root.exists(), "{diagnostics}");
    if !sqlite {
        assert_eq!(fs::read(old.join("library.db")).unwrap(), b"database");
    }
}

#[test]
fn every_retained_file_notice_and_directory_is_synced_before_publication() {
    let (_dir, paths) = fixture();
    seed_durable_files(&paths);
    let mut events = Vec::new();
    let mut lease = migrate_synced(
        &paths,
        |a, b| io::copy(&mut fs::File::open(a)?, b),
        |kind, path, file| {
            if kind == SyncKind::PublishedParent {
                assert!(paths.root.join("ui/host/preferences.json").is_file());
                assert!(
                    !paths.root.join(".mpmp.lock").exists(),
                    "runtime lease must follow the publication barrier"
                );
            } else {
                assert!(
                    !paths.root.exists(),
                    "no data is published before all barriers"
                );
            }
            events.push((kind, path.to_path_buf()));
            sync_entry(kind, path, file)
        },
    )
    .unwrap();
    assert!(lease.is_some());
    for name in [
        "library.db",
        "library.db-wal",
        "library.db-shm",
        "state.json",
        "ui/host/preferences.json",
        NOTICE,
        IN_PROGRESS,
    ] {
        assert!(
            events
                .iter()
                .any(|(kind, path)| *kind == SyncKind::File && path.ends_with(name)),
            "missing file barrier: {name}"
        );
    }
    assert!(
        events
            .iter()
            .any(|(k, p)| *k == SyncKind::Directory && p.ends_with("ui/host"))
    );
    assert_eq!(events.last().unwrap().0, SyncKind::PublishedParent);
    assert!(prepare(&paths).is_err());
    drop(lease.take());
    assert!(prepare(&paths).is_ok());
}

fn seed_durable_files(paths: &DataPaths) {
    seed(paths);
    let old = paths.legacy.as_ref().unwrap();
    fs::create_dir_all(old.join("ui/host")).unwrap();
    for name in [
        "library.db",
        "library.db-wal",
        "library.db-shm",
        "ui/host/preferences.json",
    ] {
        fs::write(old.join(name), name).unwrap();
    }
}

#[test]
fn sync_failures_in_files_notice_directories_or_published_parent_revoke_and_clean() {
    for (fail_kind, suffix) in [
        (SyncKind::File, "library.db"),
        (SyncKind::File, "library.db-wal"),
        (SyncKind::File, "library.db-shm"),
        (SyncKind::File, "ui/host/preferences.json"),
        (SyncKind::File, NOTICE),
        (SyncKind::File, IN_PROGRESS),
        (SyncKind::Directory, "ui/host"),
        (SyncKind::Directory, ""),
        (SyncKind::PublishedParent, ""),
    ] {
        let (dir, paths) = fixture();
        seed_durable_files(&paths);
        let result = migrate_synced(
            &paths,
            |a, b| io::copy(&mut fs::File::open(a)?, b),
            |kind, path, file| {
                if kind == fail_kind && (suffix.is_empty() || path.ends_with(suffix)) {
                    return Err(io::Error::other("injected sync failure"));
                }
                sync_entry(kind, path, file)
            },
        );
        assert!(result.is_err(), "{fail_kind:?}: {suffix}");
        assert!(!paths.root.exists(), "failed publication must be revoked");
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temporary copy remains"
        );
        assert_eq!(
            fs::read(paths.legacy.as_ref().unwrap().join("library.db-wal")).unwrap(),
            b"library.db-wal"
        );
        assert!(prepare(&paths).is_ok(), "cleanup must release both leases");
    }
}

#[cfg(unix)]
#[test]
fn readable_readonly_files_are_synced_and_permissions_preserved() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, paths) = fixture();
    seed(&paths);
    fs::set_permissions(
        paths.legacy.as_ref().unwrap().join("state.json"),
        fs::Permissions::from_mode(0o400),
    )
    .unwrap();
    prepare(&paths).unwrap();
    assert_eq!(
        fs::metadata(paths.root.join("state.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o400
    );
}

#[cfg(unix)]
#[test]
fn webview_links_and_recognized_stale_socket_are_skipped_but_sources_remain() {
    use std::os::unix::fs::{FileTypeExt, symlink};
    #[cfg(target_os = "linux")]
    let (dir, paths) = fixture();
    // Outside Linux the socket is really bound, and macOS's per-user `$TMPDIR`
    // is too long for a socket path (104 bytes), so the fixture lives in /tmp.
    #[cfg(not(target_os = "linux"))]
    let (dir, paths) =
        fixture_with(tempfile::tempdir_in(fs::canonicalize("/tmp").unwrap()).unwrap());
    seed(&paths);
    let old = paths.legacy.as_ref().unwrap();
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), b"untouched").unwrap();
    let cache = prepare_webview_cache(old).unwrap();
    symlink(&outside, cache.join("runtime-link")).unwrap();
    fs::create_dir(old.join("run")).unwrap();
    let socket = old.join("run/mcp.sock");
    #[cfg(target_os = "linux")]
    {
        let path = std::ffi::CString::new(socket.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: valid path. Create an abandoned socket inode, no bind/connect syscall.
        assert_eq!(
            unsafe { libc::mknod(path.as_ptr(), libc::S_IFSOCK | 0o600, 0) },
            0,
            "{}",
            io::Error::last_os_error()
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        drop(listener);
    }
    assert!(
        fs::symlink_metadata(&socket)
            .unwrap()
            .file_type()
            .is_socket()
    );
    prepare(&paths).unwrap();
    assert!(!paths.root.join("webview").exists());
    assert!(!paths.root.join("run/mcp.sock").exists());
    assert!(
        fs::symlink_metadata(old.join("webview/runtime-link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::symlink_metadata(socket)
            .unwrap()
            .file_type()
            .is_socket()
    );
    assert_eq!(fs::read(outside.join("keep")).unwrap(), b"untouched");
    assert_eq!(
        fs::read(old.join("webview").join(WEBVIEW_OWNER)).unwrap(),
        WEBVIEW_SIGNATURE
    );
}

#[test]
fn same_named_user_files_and_unmarked_webkit_storage_are_preserved() {
    let (_dir, paths) = fixture();
    seed(&paths);
    let old = paths.legacy.as_ref().unwrap();
    for name in [
        "storage/notes.json",
        "localstorage/notes.json",
        "databases/notes.json",
        "WebKitCache/notes.json",
        "run/mcp.sock",
        "mcp.sock",
        "cookies",
    ] {
        fs::create_dir_all(old.join(name).parent().unwrap()).unwrap();
        fs::write(old.join(name), name).unwrap();
    }
    prepare(&paths).unwrap();
    for name in [
        "storage/notes.json",
        "localstorage/notes.json",
        "databases/notes.json",
        "WebKitCache/notes.json",
        "run/mcp.sock",
        "mcp.sock",
        "cookies",
    ] {
        assert_eq!(fs::read(paths.root.join(name)).unwrap(), name.as_bytes());
        assert_eq!(fs::read(old.join(name)).unwrap(), name.as_bytes());
    }
}

// Record every source path, type and file byte, without traversing links.
fn source_snapshot(root: &Path) -> Vec<(PathBuf, fs::FileType, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<(PathBuf, fs::FileType, Vec<u8>)>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        let bytes = if linked(&metadata) {
            fs::read_link(path)
                .unwrap()
                .as_os_str()
                .as_encoded_bytes()
                .to_vec()
        } else if metadata.is_file() {
            fs::read(path).unwrap()
        } else {
            Vec::new()
        };
        entries.push((
            path.strip_prefix(root).unwrap().into(),
            metadata.file_type(),
            bytes,
        ));
        if metadata.is_dir() && !linked(&metadata) {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), entries);
            }
        }
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

fn assert_name_conflict_before_copy(paths: &DataPaths, conflict: &Path) {
    let old = paths.legacy.as_ref().unwrap();
    let before = source_snapshot(old);
    let error = migrate_with_ops(
        paths,
        |_, _| panic!("copy must not begin on a reserved-name conflict"),
        |_, _, _| panic!("sync must not begin on a reserved-name conflict"),
        |_, _| panic!("publish must not begin on a reserved-name conflict"),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains(conflict.to_str().unwrap()), "{error}");
    // Exercise the same prepare/error path displayed by the native startup dialog.
    let error = prepare(paths).err().unwrap().to_string();
    for message in [
        conflict.to_str().unwrap(),
        old.to_str().unwrap(),
        "reserved-name conflict",
        "保留的名称冲突",
        "Close all players",
        "改名或移走",
        "PLAYER_DATA_DIR",
    ] {
        assert!(error.contains(message), "missing {message}: {error}");
    }
    assert!(!paths.root.exists());
    assert_eq!(
        source_snapshot(old),
        before,
        "all source bytes and names must be unchanged"
    );
    assert!(
        fs::read_dir(paths.root.parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".mpmp-migration-")),
        "no staging copy may remain"
    );
}

#[test]
fn unowned_webview_items_conflict_before_copy_and_preserve_the_source() {
    for kind in ["file", "directory", "wrong-signature"] {
        let (_dir, paths) = fixture();
        seed_durable_files(&paths);
        let old = paths.legacy.as_ref().unwrap();
        let path = old.join("webview");
        if kind == "file" {
            fs::write(&path, b"user document\0\xff").unwrap();
        } else {
            fs::create_dir(&path).unwrap();
            fs::write(path.join("notes.json"), b"user notes\0\xff").unwrap();
            if kind == "wrong-signature" {
                fs::write(path.join(WEBVIEW_OWNER), b"user document").unwrap();
            }
            prepare_webview_cache(old).unwrap();
            // Existing user directories are never retroactively claimed.
            if kind == "directory" {
                assert!(!path.join(WEBVIEW_OWNER).exists());
            }
        }
        assert_name_conflict_before_copy(&paths, &path);
        // User-driven recovery: renamed data must be retained in the successful copy.
        let saved = old.join("saved-webview");
        fs::rename(&path, &saved).unwrap();
        drop(prepare(&paths).unwrap());
        assert_eq!(
            source_snapshot(&saved),
            source_snapshot(&paths.root.join("saved-webview"))
        );
    }
}

#[test]
fn reserved_protocol_items_conflict_before_copy_and_preserve_the_source() {
    for name in [IN_PROGRESS, NOTICE, ".mpmp.lock"] {
        for directory in [false, true] {
            let (_dir, paths) = fixture();
            seed_durable_files(&paths);
            let old = paths.legacy.as_ref().unwrap();
            let path = old.join(name);
            if directory {
                fs::create_dir(&path).unwrap();
                fs::write(path.join("user.bin"), b"user data\0\xff").unwrap();
            } else {
                fs::write(&path, b"user data\0\xff").unwrap();
            }
            assert_name_conflict_before_copy(&paths, &path);
            let saved = old.join(format!("saved-{name}"));
            fs::rename(&path, &saved).unwrap();
            drop(prepare(&paths).unwrap());
            assert_eq!(
                source_snapshot(&saved),
                source_snapshot(&paths.root.join(format!("saved-{name}")))
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn dangling_reserved_name_links_also_conflict_without_copying() {
    use std::os::unix::fs::symlink;
    for name in ["webview", IN_PROGRESS, NOTICE, ".mpmp.lock"] {
        let (_dir, paths) = fixture();
        seed_durable_files(&paths);
        let path = paths.legacy.as_ref().unwrap().join(name);
        symlink("missing-user-target", &path).unwrap();
        assert_name_conflict_before_copy(&paths, &path);
    }
}

#[cfg(unix)]
#[test]
fn unmarked_webview_links_and_forged_ownership_markers_are_fatal() {
    use std::os::unix::fs::symlink;
    for name in [
        "webview",
        "webview/link",
        "webview/.mpmp-webview-cache",
        "storage/link",
        "run/mcp.sock",
    ] {
        let (dir, paths) = fixture();
        seed(&paths);
        let old = paths.legacy.as_ref().unwrap();
        let target = dir.path().join("outside");
        fs::write(&target, WEBVIEW_SIGNATURE).unwrap();
        fs::create_dir_all(old.join(name).parent().unwrap()).unwrap();
        symlink(&target, old.join(name)).unwrap();
        assert!(prepare(&paths).is_err(), "unowned or linked marker: {name}");
        assert!(!paths.root.exists());
        assert_eq!(fs::read(target).unwrap(), WEBVIEW_SIGNATURE);
    }
}

#[test]
fn failed_parent_sync_and_failed_rollback_block_the_next_startup() {
    let (_dir, paths) = fixture();
    seed_durable_files(&paths);
    let old = paths.legacy.as_ref().unwrap();
    let result = migrate_with_ops(
        &paths,
        |a, b| io::copy(&mut fs::File::open(a)?, b),
        |kind, path, file| {
            if kind == SyncKind::PublishedParent {
                assert!(paths.root.join(IN_PROGRESS).is_file());
                return Err(io::Error::other("injected parent sync failure"));
            }
            sync_entry(kind, path, file)
        },
        |a, b| {
            if a == paths.root {
                return Err(io::Error::other("injected rollback failure"));
            }
            publish(a, b)
        },
    );
    let error = result.err().unwrap().to_string();
    assert!(error.contains("parent sync failure") && error.contains("rollback failure"));
    assert_eq!(
        fs::read_to_string(paths.root.join(IN_PROGRESS)).unwrap(),
        old.display().to_string()
    );
    for _ in 0..2 {
        let error = prepare(&paths).err().unwrap().to_string();
        for expected in [
            "incomplete migration",
            "迁移尚未完成",
            "move the NEW directory aside",
            paths.root.to_str().unwrap(),
            old.to_str().unwrap(),
        ] {
            assert!(error.contains(expected), "{error}");
        }
        assert_eq!(
            fs::read(old.join("library.db-wal")).unwrap(),
            b"library.db-wal"
        );
        assert!(paths.root.join(IN_PROGRESS).exists());
    }
    // Explicit overrides cannot bypass the incomplete-publication guard either.
    let explicit = DataPaths {
        root: paths.root.clone(),
        legacy: None,
    };
    let error = prepare(&explicit).err().unwrap().to_string();
    assert!(error.contains(old.to_str().unwrap()) && error.contains("incomplete migration"));
    // Follow the recovery instructions; the old source is never edited.
    fs::rename(&paths.root, paths.root.with_extension("incomplete")).unwrap();
    drop(prepare(&paths).unwrap());
    assert!(!paths.root.join(IN_PROGRESS).exists());
    assert_eq!(
        fs::read(old.join("library.db-wal")).unwrap(),
        b"library.db-wal"
    );
}

#[cfg(unix)]
#[test]
fn linked_database_wal_settings_and_unknown_user_files_are_fatal() {
    use std::os::unix::fs::symlink;
    for name in [
        "library.db",
        "library.db-wal",
        "ui/host/preferences.json",
        "unknown-user-file",
    ] {
        let (dir, paths) = fixture();
        seed(&paths);
        fs::create_dir_all(paths.legacy.as_ref().unwrap().join("ui/host")).unwrap();
        let original = dir.path().join("source");
        fs::write(&original, b"keep").unwrap();
        symlink(&original, paths.legacy.as_ref().unwrap().join(name)).unwrap();
        assert!(prepare(&paths).is_err(), "{name}");
        assert!(!paths.root.exists());
        assert_eq!(fs::read(original).unwrap(), b"keep");
    }
}

#[cfg(windows)]
#[test]
fn owned_webview_child_junction_is_skipped_but_a_database_junction_is_fatal() {
    for name in ["webview/runtime-link", "library.db"] {
        let (dir, paths) = fixture();
        seed(&paths);
        if name.starts_with("webview/") {
            prepare_webview_cache(paths.legacy.as_ref().unwrap()).unwrap();
        }
        let original = dir.path().join("junction-target");
        fs::create_dir(&original).unwrap();
        fs::write(original.join("keep"), b"keep").unwrap();
        create_junction(&paths.legacy.as_ref().unwrap().join(name), &original)
            .unwrap_or_else(|e| panic!("create junction {name} -> {}: {e}", original.display()));
        let result = prepare(&paths);
        assert_eq!(result.is_ok(), name.starts_with("webview/"));
        assert_eq!(fs::read(original.join("keep")).unwrap(), b"keep");
        // Remove the junction itself; do not recurse into its target during fixture cleanup.
        fs::remove_dir(paths.legacy.as_ref().unwrap().join(name)).unwrap();
    }
}

#[test]
fn publication_and_runtime_leases_overlap_through_sync_and_handoff() {
    let (_dir, paths) = fixture();
    seed_durable_files(&paths);
    let publication = lease::publication_lease(&paths.root).unwrap();
    let runtime = migrate_with_ops(
        &paths,
        |a, b| io::copy(&mut fs::File::open(a)?, b),
        |kind, path, file| {
            if kind == SyncKind::PublishedParent {
                assert!(paths.root.exists());
                assert!(lease::publication_lease(&paths.root).is_err());
                assert!(!paths.root.join(".mpmp.lock").exists());
                assert!(paths.root.join(IN_PROGRESS).is_file());
            }
            sync_entry(kind, path, file)
        },
        |a, b| {
            assert!(lease::publication_lease(&paths.root).is_err());
            assert!(
                !a.join(".mpmp.lock").exists(),
                "no runtime handle during rename"
            );
            assert_tree_handles_closed(a);
            publish(a, b)
        },
    )
    .unwrap()
    .unwrap();
    // Publication is still held when migration returns its already acquired runtime lease.
    assert!(lease::publication_lease(&paths.root).is_err());
    assert!(lease::runtime_lease(&paths.root).is_err());
    drop(publication);
    drop(lease::publication_lease(&paths.root).unwrap());
    assert!(prepare(&paths).is_err());
    drop(runtime);
    prepare(&paths).unwrap();
}

fn assert_tree_handles_closed(root: &Path) {
    if root.join(".mpmp.lock").exists() {
        drop(
            lease::runtime_lease(root)
                .expect("runtime lease must be closed before moving the tree"),
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                assert_tree_handles_closed(&path);
            } else {
                // An exclusive open detects even non-lock handles still open in this process.
                drop(
                    fs::OpenOptions::new()
                        .read(true)
                        .share_mode(0)
                        .open(&path)
                        .unwrap_or_else(|e| panic!("handle still open at {}: {e}", path.display())),
                );
            }
        }
    }
}

#[test]
fn runtime_handle_is_closed_before_rollback_when_marker_removal_fails() {
    let (_dir, paths) = fixture();
    seed_durable_files(&paths);
    let before = source_snapshot(paths.legacy.as_ref().unwrap());
    let mut moves = 0;
    let result = migrate_with_ops(
        &paths,
        |a, b| io::copy(&mut fs::File::open(a)?, b),
        |kind, path, file| {
            if kind == SyncKind::PublishedParent {
                // Fail the final remove_file after runtime acquisition, on every platform.
                fs::remove_file(paths.root.join(IN_PROGRESS))?;
                fs::create_dir(paths.root.join(IN_PROGRESS))?;
            }
            sync_entry(kind, path, file)
        },
        |a, b| {
            moves += 1;
            assert_eq!(a.join(".mpmp.lock").exists(), moves == 2);
            assert_tree_handles_closed(a);
            publish(a, b)
        },
    );
    assert!(result.is_err());
    assert_eq!(moves, 2, "rollback must run after runtime acquisition");
    assert!(!paths.root.exists());
    assert_eq!(source_snapshot(paths.legacy.as_ref().unwrap()), before);
    drop(prepare(&paths).unwrap());
}

#[test]
fn preparation_errors_keep_their_category_without_blaming_the_old_player_for_io() {
    let (_dir, paths) = fixture();
    seed(&paths);
    fs::write(paths.legacy.as_ref().unwrap().join("webview"), b"user file").unwrap();
    let conflict = prepare(&paths).err().unwrap();
    assert_eq!(
        preparation_error_kind(&conflict),
        PreparationErrorKind::NameConflict
    );
    assert!(conflict.to_string().contains("PLAYER_DATA_DIR"));
    assert!(!conflict.to_string().contains("Close the old player"));
    let bad = DataPaths {
        root: paths.root.join("nested"),
        legacy: None,
    };
    fs::write(&paths.root, b"not a directory").unwrap();
    let error = prepare(&bad).err().unwrap();
    assert_eq!(preparation_error_kind(&error), PreparationErrorKind::Io);
    assert!(!error.to_string().contains("Close the old player"));
    assert!(error.to_string().contains("旧数据仍保留"));
}

#[cfg(windows)]
fn create_junction(link: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::{
        Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT},
        System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_REPARSE_POINT},
    };
    let target = fs::canonicalize(target)?;
    // Normalize the link's existing parent too; verbatim Win32 paths reject '/'.
    let link = fs::canonicalize(
        link.parent()
            .ok_or_else(|| io::Error::other("junction has no parent"))?,
    )?
    .join(
        link.file_name()
            .ok_or_else(|| io::Error::other("junction has no filename"))?,
    );
    let printable = target.to_string_lossy();
    let native = format!(
        r"\??\{}",
        printable.strip_prefix(r"\\?\").unwrap_or(&printable)
    );
    let substitute: Vec<u16> = std::ffi::OsStr::new(&native).encode_wide().collect();
    let print: Vec<u16> = target.as_os_str().encode_wide().collect();
    let mut data = Vec::new();
    data.extend_from_slice(&0xa0000003u32.to_le_bytes()); // IO_REPARSE_TAG_MOUNT_POINT
    let bytes = 8 + (substitute.len() + print.len() + 2) * 2;
    data.extend_from_slice(&(u16::try_from(bytes).map_err(io::Error::other)?).to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    for value in [
        0,
        substitute.len() * 2,
        (substitute.len() + 1) * 2,
        print.len() * 2,
    ] {
        data.extend_from_slice(&(u16::try_from(value).map_err(io::Error::other)?).to_le_bytes());
    }
    for value in substitute
        .into_iter()
        .chain(Some(0))
        .chain(print)
        .chain(Some(0))
    {
        data.extend_from_slice(&value.to_le_bytes());
    }
    fs::create_dir(&link)?;
    let directory = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&link)?;
    let mut returned = 0;
    // SAFETY: live directory handle, correctly sized mount-point buffer, no async I/O.
    if unsafe {
        DeviceIoControl(
            directory.as_raw_handle(),
            FSCTL_SET_REPARSE_POINT,
            data.as_ptr().cast(),
            data.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
