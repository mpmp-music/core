//! Per-library injection regressions.
use crate::{Library, LibraryError, tests::UI};
use mpmp_fs_desktop::DesktopStorage;
use mpmp_storage::*;
use std::{
    fs::{File, Metadata},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub(crate) struct RecordingStorage {
    pub(crate) calls: Mutex<Vec<(&'static str, PathBuf)>>,
    deny: Option<PathBuf>,
    space_error: bool,
}

impl RecordingStorage {
    fn record(&self, operation: &'static str, path: &Path) {
        self.calls
            .lock()
            .unwrap()
            .push((operation, path.to_owned()));
    }
    pub(crate) fn backends(self: &Arc<Self>) -> StorageBackends {
        StorageBackends {
            files: self.clone(),
            database: self.clone(),
            replacements: Arc::new(DesktopStorage),
        }
    }
}

impl LibraryStorage for RecordingStorage {
    fn canonical(&self, path: &Path) -> io::Result<LocalPath> {
        self.record("canonical", path);
        if self.deny.as_deref() == Some(path) {
            return Err(io::Error::from_raw_os_error(123));
        }
        DesktopStorage.canonical(path)
    }
    fn strip_verbatim(&self, path: PathBuf) -> PathBuf {
        self.record("strip_verbatim", &path);
        DesktopStorage.strip_verbatim(path)
    }
    fn comparable(&self, path: &Path) -> LocalPath {
        self.record("comparable", path);
        DesktopStorage.comparable(path)
    }
    fn case_insensitive_fs(&self, path: &Path) -> Option<bool> {
        self.record("case_insensitive_fs", path);
        DesktopStorage.case_insensitive_fs(path)
    }
    fn case_insensitive_inside(&self, dir: &Path) -> Option<bool> {
        self.record("case_insensitive_inside", dir);
        DesktopStorage.case_insensitive_inside(dir)
    }
    fn same_dir(&self, a: &Path, b: &Path) -> bool {
        self.record("same_dir", a);
        DesktopStorage.same_dir(a, b)
    }
    fn overlap(&self, a: &Path, b: &Path) -> bool {
        self.record("overlap", a);
        DesktopStorage.overlap(a, b)
    }
    fn ensure_canonical(&self, path: &Path) -> StorageResult<()> {
        self.record("ensure_canonical", path);
        DesktopStorage.ensure_canonical(path)
    }
    fn open_regular(&self, path: &Path, checked: bool) -> StorageResult<OpenedRegular> {
        self.record("open_regular", path);
        DesktopStorage.open_regular(path, checked)
    }
    fn entry_id(&self, path: &Path) -> io::Result<EntryId> {
        self.record("entry_id", path);
        DesktopStorage.entry_id(path)
    }
    fn entry_id_from_metadata(&self, path: &Path, meta: &Metadata) -> io::Result<EntryId> {
        self.record("entry_id_from_metadata", path);
        DesktopStorage.entry_id_from_metadata(path, meta)
    }
    fn opened_id(&self, file: &File) -> io::Result<EntryId> {
        self.record("opened_id", Path::new(""));
        DesktopStorage.opened_id(file)
    }
    fn entry_identity(&self, path: &Path, meta: &Metadata) -> io::Result<FileIdentity> {
        self.record("entry_identity", path);
        DesktopStorage.entry_identity(path, meta)
    }
    fn opened_identity(&self, file: &File) -> io::Result<FileIdentity> {
        self.record("opened_identity", Path::new(""));
        DesktopStorage.opened_identity(file)
    }
    fn write_capabilities(&self) -> WriteCapabilities {
        self.record("write_capabilities", Path::new(""));
        DesktopStorage.write_capabilities()
    }
}

impl DatabaseStorage for RecordingStorage {
    fn available_space(&self, path: &Path) -> io::Result<u64> {
        self.record("available_space", path);
        if self.space_error {
            return Err(io::Error::from_raw_os_error(123));
        }
        DesktopStorage.available_space(path)
    }
}

#[test]
fn two_libraries_do_not_share_storage_backend() {
    fn check_operation<T>(
        label: &str,
        backend: &RecordingStorage,
        untouched: &RecordingStorage,
        expected: &[(&str, &Path)],
        run: impl FnOnce() -> T,
    ) -> T {
        let start = backend.calls.lock().unwrap().len();
        let other_before = untouched.calls.lock().unwrap().clone();
        let result = run();
        let calls = backend.calls.lock().unwrap()[start..].to_vec();
        assert_eq!(
            *untouched.calls.lock().unwrap(),
            other_before,
            "{label}: called the other library's backend"
        );
        for &(operation, path) in expected {
            assert!(
                calls.iter().any(|(op, at)| *op == operation && at == path),
                "{label}: missing {operation} for {}; operation trace: {calls:?}",
                path.display()
            );
        }
        result
    }

    let dir = mpmp_test_support::TempDir::new().unwrap();
    // TMPDIR may sit behind a symlink; the storage backend reports canonical paths.
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let music = base.as_path().join("music");
    let denied = base.as_path().join("denied");
    std::fs::create_dir(&music).unwrap();
    std::fs::create_dir(&denied).unwrap();
    let music = DesktopStorage.canonical(&music).unwrap().0;
    let a = Arc::new(RecordingStorage {
        deny: Some(denied.clone()),
        ..Default::default()
    });
    let b = Arc::new(RecordingStorage::default());
    let first = Library::open_with(
        &base.as_path().join("a.sqlite"),
        crate::OpenOptions {
            storage: Some(a.backends()),
            migrate: true,
        },
    )
    .unwrap();
    let second = Library::open_with(
        &base.as_path().join("b.sqlite"),
        crate::OpenOptions {
            storage: Some(b.backends()),
            migrate: false,
        },
    )
    .unwrap();
    let root = check_operation("first add_root", &a, &b, &[("canonical", &music)], || {
        first.add_root(&music, UI).unwrap().value
    });
    let other = check_operation("second add_root", &b, &a, &[("canonical", &music)], || {
        second.add_root(&music, UI).unwrap().value
    });
    check_operation(
        "first denied root",
        &a,
        &b,
        &[("canonical", &denied)],
        || {
            assert!(matches!(first.add_root(&denied,
                 UI),
                 Err(LibraryError::Io(e))
                if e.raw_os_error()==Some(123)));
        },
    );
    check_operation(
        "second accepts root",
        &b,
        &a,
        &[("canonical", &denied)],
        || {
            second.add_root(&denied, UI).unwrap();
        },
    );
    // Writer-side root revival checks must also retain the originating backend.
    let removed = check_operation("remove root", &a, &b, &[], || {
        first.remove_root(root, UI).unwrap()
    });
    check_operation(
        "undo root removal",
        &a,
        &b,
        &[("comparable", &music)],
        || {
            first.undo_action(removed.action.unwrap(), UI).unwrap();
        },
    );

    let song = music.join("song.mp3");
    crate::cover::edit::tests::tagged_copy("sine44_a.mp3", &song);
    let lyrics = music.join("song.lrc");
    std::fs::write(&lyrics, "[00:01.00]hello").unwrap();
    let playlist = music.join("list.m3u8");
    std::fs::write(&playlist, "#EXTM3U\nsong.mp3\n").unwrap();
    let exported = music.join("export.m3u8");
    // Both instances exercise checked replacement of an existing export target.
    std::fs::write(&exported, "#EXTM3U\n").unwrap();
    let (size, mtime_ms) = crate::tags::job::stamp(&std::fs::metadata(&song).unwrap());
    for (library, root, backend, untouched) in [(&first, root, &a, &b), (&second, other, &b, &a)] {
        let mut record = crate::tests::record(
            root,
            "song.mp3",
            crate::tests::meta("untouched title", "untouched artist", ""),
        );
        record.size = size;
        record.mtime_ms = mtime_ms;
        check_operation("catalogue track", backend, untouched, &[], || {
            crate::tests::upsert(library, vec![record]);
        });
        let track = crate::tests::id_of(library, "untouched title");

        // Each operation gets its own trace slice: an open from lyrics or M3U cannot
        // satisfy the tag reader's assertion, even when all use the same backend.
        check_operation(
            "tag preview",
            backend,
            untouched,
            &[("canonical", &song), ("open_regular", &song)],
            || {
                let preview = library
                    .preview_tag_edit(
                        &[track],
                        &[crate::TagChange::Set {
                            key: "TITLE".into(),
                            values: vec!["New".into()],
                        }],
                    )
                    .unwrap();
                assert!(preview.skipped.is_empty(), "{preview:?}");
                assert_eq!(preview.files.len(), 1, "{preview:?}");
                let file = &preview.files[0];
                assert_eq!(file.track, track);
                assert_eq!(file.before["TITLE"], ["untouched title"]);
                assert_eq!(file.after["TITLE"], ["New"]);
                assert!(file.changed);
                assert_eq!(Path::new(&file.display_location), song);
            },
        );
        check_operation(
            "lyrics",
            backend,
            untouched,
            &[("open_regular", &lyrics)],
            || {
                let lyrics = library.lyrics(track).unwrap().unwrap();
                assert_eq!(lyrics.source, crate::LyricsSource::File);
                assert_eq!(lyrics.lines[0].text, "hello");
            },
        );
        let imported = check_operation(
            "M3U import",
            backend,
            untouched,
            &[
                ("open_regular", &playlist),
                ("ensure_canonical", &playlist),
                ("entry_id", &playlist),
                ("opened_id", Path::new("")),
                ("canonical", &song),
                ("comparable", &music),
                ("case_insensitive_fs", &music),
                ("case_insensitive_inside", &music),
            ],
            || {
                let (id, report) = library
                    .import_m3u(&playlist, None, UI.with_checked(true))
                    .unwrap()
                    .value;
                assert_eq!(report.matched, 1);
                assert_eq!(report.missing_total, 0);
                assert!(report.missing.is_empty());
                id
            },
        );
        check_operation(
            "M3U export",
            backend,
            untouched,
            &[
                ("entry_id_from_metadata", &exported),
                ("ensure_canonical", &music),
                ("canonical", &music),
                ("entry_id", &music),
                ("entry_id", &exported),
                ("opened_id", Path::new("")),
                ("comparable", &music),
                ("case_insensitive_fs", &music),
                ("case_insensitive_inside", &music),
            ],
            || {
                assert!(
                    library
                        .export_m3u(imported, &exported, true, false, UI.with_checked(true))
                        .unwrap()
                        .value
                );
                assert!(
                    std::fs::read_to_string(&exported)
                        .unwrap()
                        .lines()
                        .any(|line| line == "song.mp3")
                );
            },
        );
        check_operation(
            "scan",
            backend,
            untouched,
            &[("entry_identity", &song), ("canonical", &music)],
            || {
                let crate::ScanStart::Started(scan) = library.scan(|_| {}).unwrap() else {
                    panic!("scan already running")
                };
                let report = scan.wait().unwrap();
                assert_eq!(report.scanned, 1, "{report:?}");
                assert_eq!(report.errors, 0, "{report:?}");
                assert!(!report.cancelled);
            },
        );
    }
}

#[test]
fn injected_database_backend_is_used_before_upgrade_and_recovery() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    // TMPDIR may sit behind a symlink; the storage backend reports canonical paths.
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let path = base.as_path().join("old.sqlite");
    let mut conn = rusqlite::Connection::open(&path).unwrap();
    crate::db::schema::migrate(&mut conn).unwrap();
    conn.pragma_update(None, "user_version", 13).unwrap();
    conn.pragma_update(None, "application_id", 0).unwrap();
    drop(conn);
    let backend = Arc::new(RecordingStorage {
        space_error: true,
        ..Default::default()
    });
    assert!(matches!(Library::open_with(
            &path,
            crate::OpenOptions::with_storage(backend.backends()),
        ),
         Err(LibraryError::Io(e))
        if e.raw_os_error()==Some(123)));
    assert_eq!(
        *backend.calls.lock().unwrap(),
        [
            ("write_capabilities", PathBuf::new()),
            ("entry_identity", path.clone()),
            ("entry_identity", path.clone()),
            ("entry_identity", path.clone()),
            ("available_space", base.as_path().to_owned())
        ]
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        13
    );
    drop(conn);
    assert!(!base.as_path().join("migration-backups").exists());
    let desktop = Arc::new(RecordingStorage::default());
    drop(
        Library::open_with(
            &path,
            crate::OpenOptions {
                storage: Some(desktop.backends()),
                migrate: true,
            },
        )
        .unwrap(),
    );
    assert!(
        desktop
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(op, _)| *op == "available_space")
    );
}
