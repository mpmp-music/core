use super::*;
fn scan(lib: &crate::Library) -> ScanReport {
    let Ok(ScanStart::Started(job)) = lib.scan(|_| {}) else {
        panic!("unexpected joined scan")
    };
    job.wait().unwrap()
}
fn indexed_pair() -> (crate::tests::Fixture, PathBuf, PathBuf) {
    let f = crate::tests::fixture();
    let music = f.lib.roots().unwrap()[0].path.clone();
    std::fs::create_dir(music.join("later")).unwrap();
    let good = music.join("first.flac");
    let unknown = music.join("later/unknown.flac");
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_full.flac");
    for path in [&good, &unknown] {
        std::fs::copy(&source, path).unwrap();
    }
    assert_eq!(scan(&f.lib).added, 2);
    (f, good, unknown)
}
fn assert_pair_available(lib: &crate::Library) {
    let root = lib.roots().unwrap()[0].id;
    let rows = lib
        .inner()
        .pool
        .read(|conn| reads::root_files(conn, root))
        .unwrap();
    assert_eq!(rows.files.len(), 2);
    assert!(rows.files.values().all(|row| row.available));
}
#[test]
fn partial_enumeration_after_disconnect_preserves_unknown_subtree() {
    let (f, _, unknown) = indexed_pair();
    std::fs::remove_file(&unknown).unwrap();
    *f.lib.inner().hooks.fail_walk.lock().unwrap() = Some(unknown.parent().unwrap().to_owned());
    let report = scan(&f.lib);
    assert_eq!((report.scanned, report.errors, report.removed), (1, 1, 0));
    assert_pair_available(&f.lib);
    // Once readable, a genuinely absent file can be marked missing.
    *f.lib.inner().hooks.fail_walk.lock().unwrap() = None;
    assert_eq!(scan(&f.lib).removed, 1);
}
#[test]
fn single_file_stat_failure_preserves_old_row() {
    let (f, _, unknown) = indexed_pair();
    *f.lib.inner().hooks.fail_metadata.lock().unwrap() = Some(unknown);
    let report = scan(&f.lib);
    assert_eq!((report.scanned, report.errors, report.removed), (1, 1, 0));
    assert_eq!(report.first_errors[0].code, ScanErrorCode::StatFailed);
    assert_pair_available(&f.lib);
}
#[test]
fn root_enumeration_failure_after_partial_success_disables_missing_detection() {
    let (f, _, unknown) = indexed_pair();
    std::fs::remove_file(unknown).unwrap();
    let root = f.lib.roots().unwrap()[0].path.clone();
    *f.lib.inner().hooks.fail_walk_after.lock().unwrap() = Some(root);
    let report = scan(&f.lib);
    assert_eq!((report.scanned, report.errors, report.removed), (1, 1, 0));
    assert_pair_available(&f.lib);
}
#[test]
fn root_enumeration_failure_before_any_entries_preserves_rows() {
    let (f, _, _) = indexed_pair();
    *f.lib.inner().hooks.fail_walk.lock().unwrap() = Some(f.lib.roots().unwrap()[0].path.clone());
    let report = scan(&f.lib);
    assert_eq!(report.removed, 0);
    assert_pair_available(&f.lib);
}
#[test]
fn periodic_full_scan_stat_jitter_never_removes_or_restores_tracks() {
    let (f, _, unknown) = indexed_pair();
    for failing in [true, false, true, false] {
        *f.lib.inner().hooks.fail_metadata.lock().unwrap() = failing.then(|| unknown.clone());
        // Core's periodic verify uses Catalog::rescan_roots(None), which calls
        // this same entry point; it does not have a separate availability path.
        let ScanStart::Started(job) = f.lib.scan_roots(None, |_| {}).unwrap() else {
            panic!()
        };
        let report = job.wait().unwrap();
        assert_eq!((report.removed, report.restored), (0, 0));
        assert_eq!(report.errors, u64::from(failing));
        assert_pair_available(&f.lib);
    }
}
#[test]
fn unreadable_cue_sheet_retains_virtual_tracks_and_audio_ownership() {
    let (f, _, audio) = indexed_pair();
    let sheet = audio.with_extension("cue");
    std::fs::write(
        &sheet,
        concat!(
            "FILE \"unknown.flac\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"Cut\"\n    ",
            "INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 01 00:00:01\n",
        ),
    )
    .unwrap();
    scan(&f.lib);
    *f.lib.inner().hooks.fail_metadata.lock().unwrap() = Some(sheet);
    let report = scan(&f.lib);
    assert_eq!((report.removed, report.restored), (0, 0));
    let known = f
        .lib
        .inner()
        .pool
        .read(|conn| reads::root_files(conn, f.root))
        .unwrap();
    assert_eq!(known.cues.len(), 2);
    assert!(known.cues.values().all(|cue| cue.available));
    assert!(known.files["later/unknown.flac"].hidden);
}
#[test]
fn obsolete_cue_does_not_reclaim_visible_audio_after_partial_root_failure() {
    let (f, _, audio) = indexed_pair();
    let sheet = audio.with_extension("cue");
    std::fs::write(
        &sheet,
        concat!(
            "FILE \"unknown.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 ",
            "00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:00:01\n",
        ),
    )
    .unwrap();
    scan(&f.lib);
    let rows = || {
        f.lib
            .inner()
            .pool
            .read(|conn| reads::root_files(conn, f.root))
            .unwrap()
    };
    assert!(rows().files["later/unknown.flac"].hidden);
    assert!(rows().cues.values().all(|cue| cue.available));
    std::fs::remove_file(sheet).unwrap();
    scan(&f.lib);
    let before = rows();
    assert!(!before.cues.is_empty());
    assert!(before.cues.values().all(|cue| !cue.available));
    let whole = &before.files["later/unknown.flac"];
    assert!(whole.available);
    assert!(!whole.hidden);
    *f.lib.inner().hooks.fail_walk_after.lock().unwrap() =
        Some(f.lib.roots().unwrap()[0].path.clone());
    let report = scan(&f.lib);
    assert_eq!(report.errors, 1);
    let after = rows();
    let whole = &after.files["later/unknown.flac"];
    assert!(
        whole.available,
        "unknown enumeration must retain the visible whole track"
    );
    assert!(
        !whole.hidden,
        "unavailable historical cuts cannot claim its audio"
    );
    assert!(after.cues.values().all(|cue| !cue.available));
    // Bounded retries must not revive historical ownership either.
    *f.lib.inner().hooks.fail_walk_after.lock().unwrap() = None;
    let ScanStart::Started(job) = f
        .lib
        .scan_directories(vec![audio.parent().unwrap().to_owned()], |_| {})
        .unwrap()
    else {
        panic!()
    };
    job.wait().unwrap();
    let after = rows();
    assert!(after.files["later/unknown.flac"].available);
    assert!(!after.files["later/unknown.flac"].hidden);
}
#[test]
fn unreadable_path_protects_itself_and_descendants_only() {
    let root = Path::new("/music");
    let mut completeness = WalkCompleteness {
        root_readable: true,
        unreadable: Vec::new(),
    };
    completeness.failed(root, &root.join("album"));
    assert!(!completeness.can_mark_missing("album"));
    assert!(!completeness.can_mark_missing("album/disc/track.flac"));
    assert!(completeness.can_mark_missing("album-other/track.flac"));
    completeness.failed(root, root);
    assert!(!completeness.can_mark_missing("elsewhere.flac"));
}
#[test]
fn directory_retry_recovers_an_unchanged_file_after_transient_reader_failure() {
    let f = crate::tests::fixture();
    let music = f.lib.roots().unwrap()[0].path.clone();
    let album = music.join("album");
    let other = music.join("unrelated");
    std::fs::create_dir_all(&album).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let audio = album.join("retry.flac");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_full.flac");
    std::fs::copy(&fixture, &audio).unwrap();
    std::fs::copy(&fixture, other.join("keep.flac")).unwrap();
    *f.lib.inner().hooks.fail_audio_read.lock().unwrap() = Some(audio.clone());
    let first = scan(&f.lib);
    assert_eq!((first.scanned, first.errors), (2, 1));
    let stamp = std::fs::metadata(&audio).unwrap().modified().unwrap();
    *f.lib.inner().hooks.fail_audio_read.lock().unwrap() = None;
    // The retry must neither visit this subtree nor mark its old row missing.
    std::fs::remove_file(other.join("keep.flac")).unwrap();
    let visited = Arc::new(std::sync::Mutex::new(Vec::new()));
    let paths = visited.clone();
    *f.lib.inner().hooks.scan_file.lock().unwrap() = Some(Arc::new(move |_, path| {
        paths.lock().unwrap().push(path.to_owned())
    }));
    let ScanStart::Started(job) = f
        .lib
        .scan_directories(vec![album.clone(), album], |_| {})
        .unwrap()
    else {
        panic!()
    };
    let report = job.wait().unwrap();
    assert_eq!(
        (report.scanned, report.added, report.errors, report.removed),
        (1, 1, 0, 0)
    );
    assert_eq!(*visited.lock().unwrap(), vec!["album/retry.flac"]);
    assert_eq!(
        std::fs::metadata(&audio).unwrap().modified().unwrap(),
        stamp
    );
    let known = f
        .lib
        .inner()
        .pool
        .read(|conn| reads::root_files(conn, f.root))
        .unwrap();
    assert_eq!(
        known.files.values().filter(|file| file.available).count(),
        2
    );
}

#[test]
fn directory_retry_resolves_cross_directory_cue_and_preserves_external_sheet_ownership() {
    let f = crate::tests::fixture();
    let root = f.lib.roots().unwrap()[0].path.clone();
    for name in ["sheets", "audio", "unrelated"] {
        std::fs::create_dir(root.join(name)).unwrap();
    }
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_full.flac");
    std::fs::copy(&fixture, root.join("unrelated/keep.flac")).unwrap();
    std::fs::write(
        root.join("sheets/rip.cue"),
        concat!(
            "FILE \"../audio/rip.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK ",
            "02 AUDIO\nINDEX 01 00:00:01\n",
        ),
    )
    .unwrap();
    assert_eq!(scan(&f.lib).errors, 1);
    std::fs::copy(fixture, root.join("audio/rip.flac")).unwrap();
    let visited = Arc::new(std::sync::Mutex::new(Vec::new()));
    let paths = visited.clone();
    *f.lib.inner().hooks.scan_file.lock().unwrap() = Some(Arc::new(move |_, path| {
        paths.lock().unwrap().push(path.to_owned())
    }));
    for directory in ["sheets", "audio"] {
        let ScanStart::Started(job) = f
            .lib
            .scan_directories(vec![root.join(directory)], |_| {})
            .unwrap()
        else {
            panic!()
        };
        let report = job.wait().unwrap();
        assert_eq!((report.scanned, report.errors, report.removed), (1, 0, 0));
        assert_eq!(report.added, 2 * u64::from(directory == "sheets"));
    }
    assert_eq!(
        *visited.lock().unwrap(),
        vec!["audio/rip.flac", "audio/rip.flac"]
    );
    let known = f
        .lib
        .inner()
        .pool
        .read(|conn| reads::root_files(conn, f.root))
        .unwrap();
    assert!(
        known
            .files
            .get("audio/rip.flac")
            .is_none_or(|file| file.hidden)
    );
    assert_eq!(known.cues.values().filter(|cue| cue.available).count(), 2);
    assert!(known.files["unrelated/keep.flac"].available);
}

#[test]
fn reverse_cue_retry_recovers_first_import_without_standalone_audio() {
    reverse_cue_read_failure(false);
}

#[test]
fn reverse_cue_retry_rechecks_existing_tracks_and_recovers_without_file_changes() {
    reverse_cue_read_failure(true);
}

fn reverse_cue_read_failure(existing: bool) {
    let f = crate::tests::fixture();
    let root = f.lib.roots().unwrap()[0].path.clone();
    for dir in ["sheets", "audio", "unrelated"] {
        std::fs::create_dir(root.join(dir)).unwrap();
    }
    let audio = root.join("audio/album.flac");
    let sheet = root.join("sheets/album.cue");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_full.flac");
    std::fs::copy(&fixture, &audio).unwrap();
    std::fs::write(
        &sheet,
        concat!(
            "FILE \"../audio/album.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 ",
            "00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:00:01\n",
        ),
    )
    .unwrap();
    let old_ids = if existing {
        assert_eq!(scan(&f.lib).added, 2);
        let known = f
            .lib
            .inner()
            .pool
            .read(|conn| reads::root_files(conn, f.root))
            .unwrap();
        // Make the next ordinary scan actually reread the previously cached audio.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&audio)
            .unwrap();
        std::io::Write::write_all(&mut file, &[0]).unwrap();
        known
            .cues
            .values()
            .map(|cue| cue.id)
            .collect::<HashSet<_>>()
    } else {
        HashSet::new()
    };
    *f.lib.inner().hooks.fail_audio_read.lock().unwrap() = Some(audio.clone());
    let failed = scan(&f.lib);
    assert_eq!(failed.errors, 1);
    assert_eq!(failed.first_errors[0].path, audio.to_string_lossy());
    assert!(f.lib.inner().cue_dependencies.lock().unwrap()[&audio].contains(&sheet));
    let metadata = std::fs::metadata(&audio).unwrap();
    let stamp = (metadata.len(), metadata.modified().unwrap());
    // A permanent read failure must be rechecked and remain an error, never a fake
    // success from skipping an audio path claimed by an out-of-scope sheet.
    let ScanStart::Started(job) = f
        .lib
        .scan_directories(vec![root.join("audio")], |_| {})
        .unwrap()
    else {
        panic!()
    };
    let retry = job.wait().unwrap();
    assert_eq!((retry.errors, retry.added, retry.updated), (1, 0, 0));
    assert!(failed.diagnostics.same_errors_in(&retry.diagnostics));
    assert!(retry.diagnostics.coverage.contains(&sheet));
    // Remove only the injected I/O failure, leaving bytes, size and mtime intact.
    *f.lib.inner().hooks.fail_audio_read.lock().unwrap() = None;
    std::fs::write(root.join("unrelated/broken.flac"), b"unrelated failure").unwrap();
    let ScanStart::Started(job) = f
        .lib
        .scan_directories(vec![root.join("audio")], |_| {})
        .unwrap()
    else {
        panic!()
    };
    let recovered = job.wait().unwrap();
    assert_eq!((recovered.errors, recovered.scanned), (0, 1));
    assert_eq!(
        (recovered.added, recovered.updated),
        if existing { (0, 2) } else { (2, 0) }
    );
    let metadata = std::fs::metadata(&audio).unwrap();
    assert_eq!((metadata.len(), metadata.modified().unwrap()), stamp);
    let known = f
        .lib
        .inner()
        .pool
        .read(|conn| reads::root_files(conn, f.root))
        .unwrap();
    assert_eq!(known.cues.len(), 2);
    assert!(known.cues.values().all(|cue| cue.available));
    assert!(
        known
            .files
            .get("audio/album.flac")
            .is_none_or(|file| file.hidden)
    );
    if existing {
        assert_eq!(
            known
                .cues
                .values()
                .map(|cue| cue.id)
                .collect::<HashSet<_>>(),
            old_ids
        );
    }
}

#[test]
fn directory_and_root_requests_merge_without_widening_to_unrelated_roots() {
    let f = crate::tests::fixture();
    let root = f.lib.roots().unwrap()[0].path.clone();
    std::fs::create_dir(root.join("album")).unwrap();
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_full.flac");
    for path in ["album/retry.flac", "keep.flac"] {
        std::fs::copy(&fixture, root.join(path)).unwrap();
    }
    let second = f._dir.path().join("second");
    let unrelated = f._dir.path().join("unrelated");
    for path in [&second, &unrelated] {
        std::fs::create_dir(path).unwrap();
        std::fs::copy(&fixture, path.join("song.flac")).unwrap();
    }
    let second_id = f.lib.add_root(&second, crate::tests::UI).unwrap().value;
    let unrelated_id = f.lib.add_root(&unrelated, crate::tests::UI).unwrap().value;
    let visited = Arc::new(std::sync::Mutex::new(Vec::new()));
    let paths = visited.clone();
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (resume_tx, resume) = std::sync::mpsc::channel();
    let gate = std::sync::Mutex::new(Some(resume));
    *f.lib.inner().hooks.scan_file.lock().unwrap() = Some(Arc::new(move |id, path| {
        paths.lock().unwrap().push((id, path.to_owned()));
        if let Some(resume) = gate.lock().unwrap().take() {
            entered_tx.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(5)).unwrap();
        }
    }));
    let ScanStart::Started(job) = f.lib.scan_roots(Some(vec![f.root]), |_| {}).unwrap() else {
        panic!()
    };
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        f.lib
            .scan_directories(vec![root.join("album")], |_| {})
            .unwrap(),
        ScanStart::Coalesced(_)
    ));
    assert!(matches!(
        f.lib.scan_roots(Some(vec![second_id]), |_| {}).unwrap(),
        ScanStart::Coalesced(_)
    ));
    resume_tx.send(()).unwrap();
    let report = job.wait().unwrap();
    assert_eq!((report.passes, report.scanned), (2, 3));
    let visited = visited.lock().unwrap();
    assert_eq!(
        visited
            .iter()
            .filter(|(_, path)| path == "keep.flac")
            .count(),
        1
    );
    assert_eq!(visited.iter().filter(|(id, _)| *id == second_id).count(), 1);
    assert!(!visited.iter().any(|(id, _)| *id == unrelated_id));
}

fn injected_reader_panic(cue: bool) {
    let f = crate::tests::fixture();
    let music = f.lib.roots().unwrap()[0].path.clone();
    let audio = music.join("rip.flac");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_full.flac"),
        &audio,
    )
    .unwrap();
    if cue {
        std::fs::write(
            music.join("rip.cue"),
            "FILE \"rip.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        )
        .unwrap();
    }
    *f.lib.inner().hooks.panic_audio_reader.lock().unwrap() = Some(audio.clone());
    let report = scan(&f.lib);
    let error = report
        .first_errors
        .iter()
        .find(|e| e.path == audio.to_string_lossy())
        .unwrap();
    assert_eq!(error.code, ScanErrorCode::ReaderPanicked);
    assert!(error.message.contains("cannot read audio file"));
}
#[test]
fn request_after_completion_claim_starts_new_scan_and_old_drop_keeps_it_running() {
    use std::sync::{Mutex, mpsc};
    let f = crate::tests::fixture();
    let music = f.lib.roots().unwrap()[0].path.clone();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_a.flac"),
        music.join("song.flac"),
    )
    .unwrap();
    let library = f.lib.clone();
    let (at_finished_claim, reached) = mpsc::channel();
    let (finish_old, old_gate) = mpsc::channel();
    let ScanStart::Started(old) = f
        .lib
        .scan(move |_| {
            // Final progress is emitted after the atomic finish/rerun decision, but
            // while the old worker and ScanClaim are still alive.
            if library.inner().scan_state.lock().unwrap().running.is_none() {
                at_finished_claim.send(()).unwrap();
                old_gate.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        })
        .unwrap()
    else {
        panic!()
    };
    reached.recv_timeout(Duration::from_secs(5)).unwrap();
    let (new_entered, new_reached) = mpsc::channel();
    let (finish_new, new_gate) = mpsc::channel();
    let new_gate = Mutex::new(Some(new_gate));
    let ScanStart::Started(new) = f
        .lib
        .scan(move |_| {
            if let Some(gate) = new_gate.lock().unwrap().take() {
                new_entered.send(()).unwrap();
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        })
        .unwrap()
    else {
        panic!("a request after the finish decision must start a new scan")
    };
    assert_ne!(old.id(), new.id());
    new_reached.recv_timeout(Duration::from_secs(5)).unwrap();
    finish_old.send(()).unwrap();
    assert_eq!(old.wait().unwrap().passes, 1);
    // The released old claim must not clear the running state of the new scan.
    assert!(matches!(f.lib.scan(|_| {}).unwrap(), ScanStart::Coalesced(id) if id == new.id()));
    finish_new.send(()).unwrap();
    let report = new.wait().unwrap();
    assert_eq!((report.passes, report.coalesced, report.scanned), (2, 1, 1));
}

#[test]
fn ordinary_audio_reader_panic_has_its_own_code() {
    injected_reader_panic(false);
}
#[test]
fn cue_referenced_audio_reader_panic_has_its_own_code() {
    injected_reader_panic(true);
}
#[test]
fn twenty_large_cue_diagnostics_fit_twenty_kib_and_keep_omission_counts() {
    let f = crate::tests::fixture();
    let music = f.lib.roots().unwrap()[0].path.clone();
    for sheet in 0..20 {
        let mut text = String::new();
        for i in 0..20 {
            text.push_str(&format!(
                "FILE \"{i:02}-{}.wav\" WAVE\nTRACK {:02} AUDIO\nINDEX 01 00:00:00\n",
                "字🎵".repeat(5700),
                i + 1
            ));
        }
        assert!(text.len() < MAX_SHEET_BYTES as usize);
        std::fs::write(music.join(format!("{sheet:02}.cue")), text).unwrap();
    }
    let report = scan(&f.lib);
    assert_eq!(report.first_errors.len(), 20);
    for error in &report.first_errors {
        assert_eq!(error.code, ScanErrorCode::CueMissingFiles);
        assert!(error.message.len() <= 1024);
        assert!(error.message.ends_with("and 10 more"));
        for i in 0..10 {
            assert!(error.message.contains(&format!("{i:02}-")));
        }
        assert!(!error.message.contains("10-"));
    }
    assert!(
        report
            .first_errors
            .iter()
            .map(|e| e.message.len())
            .sum::<usize>()
            <= 20 * 1024
    );
    assert!(diagnostic("🎵".repeat(1024)).len() <= 1024);
}
#[test]
fn existing_root_without_audio_is_not_unreachable() {
    let f = crate::tests::fixture();
    let music = f.lib.roots().unwrap()[0].path.clone();
    let audio = music.join("rip.flac");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_full.flac"),
        &audio,
    )
    .unwrap();
    assert_eq!(scan(&f.lib).added, 1);
    std::fs::remove_file(audio).unwrap();
    assert_eq!(
        scan(&f.lib).first_errors[0].code,
        ScanErrorCode::NoAudioFiles
    );
    std::fs::remove_dir(&music).unwrap();
    assert_eq!(
        scan(&f.lib).first_errors[0].code,
        ScanErrorCode::RootUnreachable
    );
}
