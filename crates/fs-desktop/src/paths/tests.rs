use super::*;

#[cfg(unix)]
#[test]
fn publication_without_hardlinks_and_injected_rename_or_sync_failures_keeps_original_name() {
    for fail in ["none", "backup_sync", "rename", "published_sync"] {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("music.mp3");
        let staged = dir.path().join("staged");
        let backup = dir.path().join("backup");
        std::fs::write(&original, b"before").unwrap();
        std::fs::write(&staged, b"after").unwrap();
        let prepared = prepare_backup_with(
            &original,
            &backup,
            |_, _| Err(io::Error::from_raw_os_error(libc::EPERM)),
            |p| {
                assert_eq!(std::fs::read(&original)?, b"before");
                if fail == "backup_sync" {
                    Err(io::Error::other("fsync failed"))
                } else {
                    sync_replacement(p)
                }
            },
            |_, _| Ok(()),
        );
        if prepared.is_ok() {
            let result = publish_prepared_with(
                &staged,
                &original,
                &backup,
                true,
                |_, _| Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP)),
                |a, b| {
                    assert_eq!(std::fs::read(b)?, b"before");
                    if fail == "rename" {
                        Err(io::Error::other("rename failed"))
                    } else {
                        std::fs::rename(a, b)
                    }
                },
                |p| {
                    if fail == "published_sync" {
                        Err(io::Error::other("fsync failed"))
                    } else {
                        sync_replacement(p)
                    }
                },
            );
            assert_eq!(result.is_ok(), fail == "none");
            if result.is_err() && std::fs::read(&original).unwrap() == b"after" {
                let saved = dir.path().join("saved-after");
                std::fs::copy(&original, &saved).unwrap();
                rollback_replacement(&original, &backup, &staged, &saved).unwrap();
            }
        }
        assert_eq!(
            std::fs::read(&original).unwrap(),
            if fail == "none" {
                b"after".as_slice()
            } else {
                b"before".as_slice()
            }
        );
        assert_eq!(std::fs::read(&backup).unwrap(), b"before");
    }
}
#[cfg(unix)]
#[test]
fn exchange_error_classification_includes_both_distinct_darwin_codes() {
    // Darwin: ENOSYS=78, EINVAL=22, ENOTSUP=45, EOPNOTSUPP=102.
    for code in [78, 22, 45, 102] {
        assert!(unsupported_exchange_code(code, [78, 22, 45, 102]));
    }
    assert!(!unsupported_exchange_code(5, [78, 22, 45, 102]));
    for code in [libc::ENOTSUP, libc::EOPNOTSUPP] {
        assert!(exchange_unsupported(&io::Error::from_raw_os_error(code)));
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("music");
        let staged = dir.path().join("stage");
        let backup = dir.path().join("backup");
        let saved = dir.path().join("after");
        std::fs::write(&original, b"before").unwrap();
        std::fs::write(&backup, b"before").unwrap();
        std::fs::write(&staged, b"after").unwrap();
        publish_prepared_with(
            &staged,
            &original,
            &backup,
            true,
            |_, _| Err(io::Error::from_raw_os_error(code)),
            |a, b| std::fs::rename(a, b),
            sync_replacement,
        )
        .unwrap();
        assert_eq!(std::fs::read(&original).unwrap(), b"after");
        std::fs::copy(&original, &saved).unwrap();
        rollback_with(
            &original,
            &backup,
            &staged,
            &saved,
            |_, _| Err(io::Error::from_raw_os_error(code)),
            |a, b| std::fs::rename(a, b),
            sync_replacement,
        )
        .unwrap();
        assert_eq!(std::fs::read(&original).unwrap(), b"before");
        assert_eq!(std::fs::read(&saved).unwrap(), b"after");
    }
}
#[cfg(unix)]
#[test]
fn rollback_without_exchange_keeps_after_when_post_rename_sync_fails() {
    for safe_copy in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("music");
        let staged = dir.path().join("stage");
        let backup = dir.path().join("backup");
        let saved = dir.path().join("registered-after");
        std::fs::write(&original, b"after").unwrap();
        std::fs::write(&backup, b"before").unwrap();
        if safe_copy {
            std::fs::copy(&original, &saved).unwrap();
            sync_replacement(&saved).unwrap();
        }
        let result = rollback_with(
            &original,
            &backup,
            &staged,
            &saved,
            |_, _| Err(io::Error::from_raw_os_error(libc::ENOTSUP)),
            |a, b| {
                assert_eq!(std::fs::read(b)?, b"after");
                std::fs::rename(a, b)
            },
            |p| {
                if p == original {
                    Err(io::Error::other("fsync after rollback rename failed"))
                } else {
                    sync_replacement(p)
                }
            },
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(&original).unwrap(),
            if safe_copy {
                b"before".as_slice()
            } else {
                b"after".as_slice()
            }
        );
        assert_eq!(std::fs::read(&backup).unwrap(), b"before");
        if safe_copy {
            assert_eq!(std::fs::read(&saved).unwrap(), b"after");
        }
    }
}
#[cfg(unix)]
#[test]
fn linked_backup_without_atomic_exchange_refuses_before_touching_original() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("music");
    let staged = dir.path().join("staged");
    let backup = dir.path().join("backup");
    std::fs::write(&path, b"before").unwrap();
    std::fs::write(&staged, b"after").unwrap();
    assert!(!prepare_backup(&path, &backup).unwrap());
    let result = publish_prepared_with(
        &staged,
        &path,
        &backup,
        false,
        |_, _| Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP)),
        |_, _| panic!("must not rename over an unprotected inode"),
        sync_replacement,
    );
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Unsupported);
    assert_eq!(std::fs::read(&path).unwrap(), b"before");
    assert_eq!(std::fs::read(&staged).unwrap(), b"after");
}
#[cfg(unix)]
#[test]
fn atomic_publication_has_no_missing_name_and_retains_a_concurrent_path_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("music");
    let staged = dir.path().join("staged");
    let backup = dir.path().join("backup");
    std::fs::write(&original, b"before").unwrap();
    std::fs::write(&staged, b"after").unwrap();
    prepare_backup(&original, &backup).unwrap();
    // The external save occurs after the final backup check, before publication.
    let external = dir.path().join("external");
    std::fs::write(&external, b"external complete file").unwrap();
    std::fs::rename(external, &original).unwrap();
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let keep = running.clone();
    let watched = original.clone();
    let reader = std::thread::spawn(move || {
        while keep.load(std::sync::atomic::Ordering::Acquire) {
            let bytes = std::fs::read(&watched).unwrap();
            assert!(bytes == b"external complete file" || bytes == b"after");
        }
    });
    assert!(replace_with_backup(&staged, &original, &backup).is_err());
    running.store(false, std::sync::atomic::Ordering::Release);
    reader.join().unwrap();
    assert_eq!(std::fs::read(&original).unwrap(), b"after");
    assert_eq!(std::fs::read(&staged).unwrap(), b"external complete file");
    assert_eq!(std::fs::read(&backup).unwrap(), b"before");
}
#[test]
fn failed_second_windows_seal_move_rolls_back_immediately_including_post_move_flush_errors() {
    for post_move in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("music");
        let staged = dir.path().join("seal");
        std::fs::write(&original, b"after").unwrap();
        let mut calls = 0;
        let result = seal_with(&original, &staged, |a, b| {
            calls += 1;
            if calls == 2 && !post_move {
                return Err(io::Error::other("move denied"));
            }
            std::fs::rename(a, b)?;
            if calls == 2 {
                Err(io::Error::other("FlushFileBuffers failed"))
            } else {
                Ok(())
            }
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&original).unwrap(), b"after");
    }
}

#[cfg(windows)]
#[test]
fn windows_replacement_clears_temporary_attributes_and_denies_writers() {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TEMPORARY;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("music.mp3");
    let backup = dir.path().join("backup.mp3");
    std::fs::write(&path, b"original complete audio").unwrap();
    let staged = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
    std::fs::write(staged.path(), b"edited complete audio").unwrap();
    prepare_replacement(staged.path(), &path).unwrap();
    assert_eq!(
        std::fs::metadata(staged.path()).unwrap().file_attributes() & FILE_ATTRIBUTE_TEMPORARY,
        0
    );
    let guard = open_replace_source(&path).unwrap();
    assert!(std::fs::write(&path, b"external writer").is_err());
    // ReplaceFileW opens the replacement exclusively: the fixture must not keep
    // NamedTempFile's writable handle. Retain the production deny-writers guard.
    let staged = staged.into_temp_path();
    replace_with_backup(&staged, &path, &backup).unwrap();
    drop(guard);
    hide_candidate(&backup).unwrap();
    assert!(hidden(&backup).unwrap());
    assert!(!hidden(&path).unwrap());
    sync_replacement(&backup).unwrap();
    seal_replacement(&path, &staged).unwrap();
    verify_replacement_attributes(&path, &backup).unwrap();
    assert_eq!(std::fs::read(&backup).unwrap(), b"original complete audio");
    assert_eq!(std::fs::read(&path).unwrap(), b"edited complete audio");
}
#[test]
fn verbatim_prefix() {
    assert_eq!(
        strip_verbatim(PathBuf::from(r"\\?\C:\Music")),
        PathBuf::from(r"C:\Music")
    );
    assert_eq!(
        strip_verbatim(PathBuf::from(r"\\?\UNC\nas\music")),
        PathBuf::from(r"\\nas\music")
    );
    assert_eq!(
        strip_verbatim(PathBuf::from("/home/x")),
        PathBuf::from("/home/x")
    );
}

#[test]
fn dot_dot_is_resolved() {
    let dir = tempfile::TempDir::new().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir_all(music.join("sub")).unwrap();
    std::fs::create_dir(dir.path().join("other")).unwrap();
    let sneaky = dir.path().join("other/../music/sub");
    let music = canonical_dir(&music).unwrap();
    let sneaky = canonical_dir(&sneaky).unwrap();
    assert!(overlap(&music, &sneaky));
    assert!(same_dir(
        &music,
        &canonical_dir(&music.join("sub/..")).unwrap()
    ));
}

/// A folder named without ASCII letters (numeric or non-Latin names) is
/// told by the folders above it on the same file system, else by a name inside it;
/// only a folder with neither cannot be told.
#[test]
fn letter_case_is_probed_where_there_are_letters() {
    let dir = tempfile::TempDir::new().unwrap();
    let music = canonical_dir(dir.path()).unwrap().join("Music");
    std::fs::create_dir(&music).unwrap();
    let here = case_insensitive_fs(&music);
    assert!(here.is_some(), "\"Music\" has letters to flip");
    for name in ["2024", "古典", "01"] {
        let folder = music.join(name);
        std::fs::create_dir(&folder).unwrap();
        assert_eq!(case_insensitive_fs(&folder), here, "{name}");
        let file = folder.join("1.flac");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(case_insensitive_fs(&file), here, "{name}/1.flac");
    }
    // Inside a folder: the names in it.
    let digits = music.join("2024");
    assert_eq!(probe_inside(&digits), here, "1.flac");
    let empty = music.join("古典").join("3");
    std::fs::create_dir(&empty).unwrap();
    assert_eq!(probe_inside(&empty), None, "nothing inside");
    std::fs::create_dir(empty.join("Disc")).unwrap();
    assert_eq!(probe_inside(&empty), here);
    // Roots and M3U matching with such names are compared as before (by the folders
    // above them).
    let upper = PathBuf::from(digits.to_string_lossy().to_uppercase());
    assert_eq!(same_dir(&digits, &upper), here == Some(true));
    assert_eq!(overlap(&upper, &digits.join("1.flac")), here == Some(true));
}

/// a root's own name is looked up in the folder above it,
/// the names inside it in the root itself, so M3U matching probes inside first (on
/// Windows a case-sensitive root on a volume that is not would otherwise be folded).
#[test]
fn letter_case_inside_a_root_is_probed_inside_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let music = canonical_dir(dir.path()).unwrap().join("2024");
    std::fs::create_dir(&music).unwrap();
    let above = case_insensitive_fs(&music);
    assert!(above.is_some(), "told by the folders above");
    // Nothing inside: as the folders above.
    assert_eq!(case_insensitive_inside(&music), above);
    std::fs::create_dir(music.join("Disc")).unwrap();
    assert_eq!(case_insensitive_inside(&music), probe_inside(&music));
    assert_eq!(
        case_insensitive_inside(&music),
        above,
        "one file system here"
    );
    assert_eq!(case_insensitive_inside(&music.join("none")), None);
}

/// The probe opens nothing and follows no link. A
/// FIFO under the flipped name would block an open (and the actor with it); a symlink
/// there leading back to the original would make a file system where case counts look
/// as if it did not, and an `allow` wider. Both are other entries: case counts.
#[cfg(unix)]
#[test]
fn letter_case_probes_open_and_follow_nothing() {
    use std::time::Duration;
    let probe = |path: &Path| {
        let path = path.to_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(case_insensitive_fs(&path));
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the probe returns without waiting")
    };
    let dir = tempfile::TempDir::new().unwrap();
    let base = canonical_dir(dir.path()).unwrap();
    let music = base.join("Music");
    std::fs::create_dir(&music).unwrap();
    let flipped = base.join("mUSIC");
    let fifo = std::process::Command::new("mkfifo")
        .arg(&flipped)
        .status()
        .unwrap();
    if !fifo.success() {
        // Case is ignored here: `mUSIC` is `Music`, nothing else can be put there.
        assert_eq!(probe(&music), Some(true));
        return;
    }
    let other = base.join("music");
    std::fs::create_dir(&other).unwrap();
    let l = crate::DesktopLocations;
    let (m, inside) = (
        music.to_str().unwrap(),
        other.join("x.m3u").to_str().unwrap().to_owned(),
    );
    assert_eq!(probe(&music), Some(false), "a FIFO is another entry");
    assert!(!l.contains(m, &inside, false));
    // Probed inside a folder whose name has no letters: the same.
    let digits = music.join("2024");
    std::fs::create_dir(&digits).unwrap();
    std::fs::create_dir(digits.join("Disc")).unwrap();
    let fifo = std::process::Command::new("mkfifo")
        .arg(digits.join("dISC"))
        .status()
        .unwrap();
    assert!(fifo.success());
    let (tx, rx) = std::sync::mpsc::channel();
    let at = digits.clone();
    std::thread::spawn(move || {
        let _ = tx.send(probe_inside(&at));
    });
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the probe returns without waiting"),
        Some(false)
    );

    // A link back to the folder, absolute or relative.
    std::fs::remove_file(&flipped).unwrap();
    std::os::unix::fs::symlink(&music, &flipped).unwrap();
    assert_eq!(probe(&music), Some(false), "a link is another entry");
    assert!(!l.contains(m, &inside, false));
    assert!(!same_dir(&music, &other));
    assert!(!overlap(&music, &other.join("x")));
    std::fs::remove_file(&flipped).unwrap();
    std::os::unix::fs::symlink("Music", &flipped).unwrap();
    assert_eq!(probe(&music), Some(false), "a link is another entry");
}

/// A regular file with a hard link under the flipped name
/// is the same file found by another name, not because case is ignored: such a name is
/// skipped and the next one probed.
#[cfg(unix)]
#[test]
fn letter_case_probes_skip_files_with_hard_links() {
    let dir = tempfile::TempDir::new().unwrap();
    let music = canonical_dir(dir.path()).unwrap().join("Music");
    let digits = music.join("2024");
    std::fs::create_dir_all(&digits).unwrap();
    let here = case_insensitive_fs(&music);
    let file = digits.join("a.flac");
    std::fs::write(&file, b"").unwrap();
    if std::fs::hard_link(&file, digits.join("A.FLAC")).is_err() {
        // Case is ignored here: `A.FLAC` is `a.flac` already.
        assert_eq!(here, Some(true));
        return;
    }
    assert_eq!(
        here,
        Some(false),
        "two names for one file: case counts here"
    );
    assert_eq!(case_insensitive_fs(&file), here, "by the folders above");
    assert_eq!(probe_name(&file), None, "not probed");
    assert_eq!(probe_inside(&digits), None, "no name inside to probe");
    let upper = PathBuf::from(digits.to_string_lossy().to_uppercase());
    assert!(!same_dir(&digits, &upper));
}

/// Strict NTFS case. Run with TMP and TEMP on NTFS.
#[cfg(windows)]
#[test]
fn tag_replace_retains_the_displaced_ntfs_file_with_the_source_handle_open() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original.flac");
    let candidate = dir.path().join("candidate.tmp");
    let backup = dir.path().join("displaced.tmp");
    std::fs::write(&original, b"original bytes").unwrap();
    std::fs::write(&candidate, b"edited bytes").unwrap();
    let source = open_replace_source(&original).unwrap();
    assert!(
        std::fs::OpenOptions::new()
            .write(true)
            .open(&original)
            .is_err()
    );
    let old_id = FileIdentity::of_file(&source).unwrap();
    let new_id = FileIdentity::of_file(&File::open(&candidate).unwrap()).unwrap();
    replace_with_backup(&candidate, &original, &backup).unwrap();
    assert_eq!(
        FileIdentity::of_file(&File::open(&original).unwrap()).unwrap(),
        new_id
    );
    assert_eq!(
        FileIdentity::of_file(&File::open(&backup).unwrap()).unwrap(),
        old_id
    );
    assert_eq!(std::fs::read(&original).unwrap(), b"edited bytes");
    assert_eq!(std::fs::read(&backup).unwrap(), b"original bytes");
    drop(source);
    replace_with_backup(&backup, &original, &candidate).unwrap();
    assert_eq!(std::fs::read(&original).unwrap(), b"original bytes");
    assert_eq!(std::fs::read(&candidate).unwrap(), b"edited bytes");
}

/// Select a directory on another volume to exercise exactly the protocol preview probes.
#[cfg(any(unix, windows))]
#[test]
fn tag_publication_on_selected_volume() {
    use mpmp_storage::TagPublish;

    let dir = match std::env::var_os("MPMP_TAG_TEST_DIRECTORY") {
        Some(path) => tempfile::tempdir_in(path).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    let original = dir.path().join("original.flac");
    let candidate = dir.path().join("candidate.tmp");
    let backup = dir.path().join("displaced.tmp");
    std::fs::write(&original, b"original bytes").unwrap();
    std::fs::write(&candidate, b"edited bytes").unwrap();
    let mode = match probe_tag_publish(dir.path()) {
        Ok(mode) => mode,
        Err(error) => {
            assert!(
                std::env::var_os("MPMP_TAG_TEST_DIRECTORY").is_some(),
                "{error}"
            );
            eprintln!("Preview explicitly refuses this volume: {error}");
            assert_eq!(std::fs::read(&original).unwrap(), b"original bytes");
            assert_eq!(std::fs::read(&candidate).unwrap(), b"edited bytes");
            assert_eq!(dir.path().read_dir().unwrap().count(), 2);
            return;
        }
    };
    eprintln!("Preview selected {mode:?}");
    let before = FileIdentity::of_file(&File::open(&original).unwrap()).unwrap();
    let after = FileIdentity::of_file(&File::open(&candidate).unwrap()).unwrap();
    let (saved, stable_ids) = match mode {
        #[cfg(unix)]
        TagPublish::Exchange => {
            exchange(&candidate, &original).unwrap();
            (&candidate, true)
        }
        TagPublish::Exclusive { stable_ids } => {
            move_missing(&original, &backup).unwrap();
            // No-clobber publication must preserve a file arriving in the gap.
            std::fs::write(&original, b"late save").unwrap();
            assert_eq!(
                move_missing(&candidate, &original).unwrap_err().kind(),
                io::ErrorKind::AlreadyExists
            );
            assert_eq!(std::fs::read(&original).unwrap(), b"late save");
            std::fs::remove_file(&original).unwrap();
            publish_missing(&candidate, &original).unwrap();
            (&backup, stable_ids)
        }
        #[cfg(unix)]
        TagPublish::Link => {
            move_to_private(&original, &backup).unwrap();
            std::fs::write(&original, b"late save").unwrap();
            assert_eq!(
                link_missing(&candidate, &original).unwrap_err().kind(),
                io::ErrorKind::AlreadyExists
            );
            assert_eq!(std::fs::read(&original).unwrap(), b"late save");
            std::fs::remove_file(&original).unwrap();
            link_missing(&candidate, &original).unwrap();
            (&backup, true)
        }
        #[cfg(windows)]
        TagPublish::Replace { stable_ids } => {
            let source = open_replace_source(&original).unwrap();
            replace_with_backup(&candidate, &original, &backup).unwrap();
            drop(source);
            (&backup, stable_ids)
        }
        _ => panic!("unexpected publication protocol {mode:?}"),
    };
    assert_eq!(std::fs::read(&original).unwrap(), b"edited bytes");
    assert_eq!(std::fs::read(saved).unwrap(), b"original bytes");
    let moved_after = FileIdentity::of_file(&File::open(&original).unwrap()).unwrap();
    let moved_before = FileIdentity::of_file(&File::open(saved).unwrap()).unwrap();
    if stable_ids {
        assert_eq!(moved_after, after);
        assert_eq!(moved_before, before);
    }
    eprintln!("Candidate ID {after:?} -> {moved_after:?}, original {before:?} -> {moved_before:?}");
}
