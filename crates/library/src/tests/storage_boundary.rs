//! Backend contract tests also run without desktop-fs.
use crate::{db::writer::now_ms, journal, *};
use local::LocalStorage;
use mpmp_test_support::local_storage as local;
use rusqlite::{Connection, types::Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

const UI: ActionMeta<'_> = ActionMeta::new("ui");

fn temp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    (dir, base)
}

#[cfg(not(feature = "desktop-fs"))]
#[test]
fn missing_storage_fails_before_database_creation() {
    let (_dir, base) = temp();
    let db = base.join("absent/library.db");
    for result in [
        Library::open(&db),
        Library::open_with(
            &db,
            crate::OpenOptions {
                storage: None,
                migrate: false,
            },
        ),
    ] {
        assert!(matches!(result,
             Err(LibraryError::Storage(mpmp_storage::StorageError::Configuration(message)))
            if message.contains("with_storage")));
        assert!(!db.parent().unwrap().exists());
    }
}

#[test]
fn inconsistent_capabilities_fail_before_database_creation() {
    let (_dir, base) = temp();
    for caps in [
        mpmp_storage::WriteCapabilities {
            durable_replace: true,
            legacy_tag_write: true,
        },
        mpmp_storage::WriteCapabilities {
            durable_replace: false,
            legacy_tag_write: true,
        },
    ] {
        let backend = Arc::new(LocalStorage {
            capabilities: Some(caps),
            ..Default::default()
        });
        let db = base.join("absent/library.db");
        for result in [
            Library::open_with(
                &db,
                crate::OpenOptions {
                    storage: Some(backend.backends()),
                    migrate: true,
                },
            ),
            Library::open_with(
                &db,
                crate::OpenOptions {
                    storage: Some(backend.backends()),
                    migrate: false,
                },
            ),
        ] {
            assert!(matches!(
                result,
                Err(LibraryError::Storage(
                    mpmp_storage::StorageError::Configuration(_)
                ))
            ));
            assert!(!db.parent().unwrap().exists());
        }
        assert!(
            backend
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|op| *op == "capabilities")
        );
    }
}

fn rows(conn: &Connection, table: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY 1,2"))
        .unwrap();
    let count = stmt.column_count();
    stmt.query_map([], |row| (0..count).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn state(lib: &Library) -> Vec<Vec<Vec<Value>>> {
    lib.inner
        .pool
        .read(|conn| {
            Ok(
                ["actions", "action_targets", "file_write_intents", "tracks"]
                    .iter()
                    .map(|table| rows(conn, table))
                    .collect(),
            )
        })
        .unwrap()
}

fn seed(lib: &Library, base: &Path, cover: bool, redo: bool) -> ActionId {
    const INSERT_TRACKS: &str = r#"
INSERT INTO tracks(id,root_id,rel_path,size,mtime_ms,album_key,added_at,updated_at)
VALUES (1,?1,'track.flac',8,0,'test',0,0)
"#;

    let root = lib.add_root(base, UI).unwrap().value;
    lib.inner
        .writer
        .run_data(move |conn| {
            conn.execute(INSERT_TRACKS, [root.0])?;
            let now = now_ms();
            let tag_files = || {
                vec![journal::FileTags {
                    track: 1,
                    size: 8,
                    mtime_ms: 0,
                    values: vec![("TITLE".into(), vec!["before".into()])],
                    raw_id3: Some(vec![1, 2, 3]),
                    protected_hash: None,
                    exact: None,
                }]
            };
            let cover_files = || {
                vec![crate::cover::job::FileCover {
                    track: 1,
                    regions: vec![],
                    expected: "after".into(),
                    restored_digest: "before".into(),
                }]
            };
            let id = if cover {
                journal::record_cover(conn, UI, cover_files(), "remove", None, now, "original")?
            } else {
                journal::record_tag_edit(
                    conn,
                    UI,
                    "tag edit".into(),
                    tag_files(),
                    serde_json::json!([]),
                    now,
                )?
            };
            if !redo {
                return Ok(id);
            }
            if cover {
                journal::record_cover(conn, UI, cover_files(), "undo", Some(id), now, "undo")
            } else {
                journal::record_tag_undo(conn, id, UI, tag_files(), now, false)
            }
        })
        .unwrap()
}

#[test]
fn unsupported_replacement_rejects_cover_tags_and_file_undo_without_mutation() {
    for cover in [false, true] {
        for redo in [false, true] {
            let (_dir, base) = temp();
            let path = base.join("track.flac");
            std::fs::write(&path, b"original").unwrap();
            let backend = Arc::new(LocalStorage::default());
            let lib = Library::open_with(
                &base.join("library.db"),
                crate::OpenOptions {
                    storage: Some(backend.backends()),
                    migrate: true,
                },
            )
            .unwrap();
            let id = seed(&lib, &base, cover, redo);
            let before = state(&lib);
            backend.calls.lock().unwrap().clear();
            let cover_error =
                |error: LibraryError| assert_eq!(error.code(), "cover_edit_unsupported");
            let tag_error = |error: LibraryError| {
                assert!(matches!(
                    error,
                    LibraryError::Storage(mpmp_storage::StorageError::Unsupported {
                        operation: mpmp_storage::Operation::TagWrite
                    })
                ))
            };
            cover_error(
                lib.preview_cover_edit(&[TrackId(1)], &CoverChange::Remove)
                    .err()
                    .unwrap(),
            );
            cover_error(
                lib.edit_cover(
                    &[TrackId(1)],
                    &CoverChange::Remove,
                    UI,
                    TagJobOptions::default(),
                )
                .err()
                .unwrap(),
            );
            tag_error(lib.preview_tag_edit(&[TrackId(1)], &[]).err().unwrap());
            tag_error(
                lib.edit_tags(&[TrackId(1)], &[], UI, TagJobOptions::default())
                    .err()
                    .unwrap(),
            );
            if cover {
                cover_error(lib.preview_cover_undo(id).err().unwrap());
                cover_error(
                    lib.undo_cover_edit(id, UI, TagJobOptions::default())
                        .err()
                        .unwrap(),
                );
                cover_error(lib.undo_action(id, UI).err().unwrap());
            } else {
                tag_error(lib.preview_tag_undo(id).err().unwrap());
                tag_error(
                    lib.undo_tag_edit(id, UI, TagJobOptions::default())
                        .err()
                        .unwrap(),
                );
                tag_error(lib.undo_action(id, UI).err().unwrap());
            }
            let description = lib.describe_undo(id).unwrap();
            assert!(description.writes_files);
            let error = lib.undo_action_guarded(description, UI).err().unwrap();
            if cover {
                cover_error(error);
            } else {
                tag_error(error);
            }
            assert_eq!(state(&lib), before);
            assert_eq!(std::fs::read(&path).unwrap(), b"original");
            assert!(
                backend
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|op| *op == "capabilities")
            );
            assert!(std::fs::read_dir(&base).unwrap().all(|e| {
                !e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".cover-")
            }));
            // Database-only changes and undo remain usable.
            let playlist = lib
                .create_playlist("local", PlaylistKind::Manual, UI)
                .unwrap();
            lib.undo_action(playlist.action.unwrap(), UI).unwrap();
        }
    }
}

#[test]
fn unsupported_backend_preserves_pending_intents_on_open() {
    const INSERT_FILE_WRITE_INTENTS: &str = r#"
INSERT INTO file_write_intents(family,format_version,authorization_json,target_json,before_digest,before_image,after_digest,stage,created_at,updated_at)
VALUES('cover_edit',1,?1,?2,'before',X'010203','after',?3,1,1)
"#;

    for stage in ["building", "prepared", "finalized", "retained"] {
        let (_dir, base) = temp();
        let db = base.join("library.db");
        let backend = Arc::new(LocalStorage::default());
        let lib = Library::open_with(
            &db,
            crate::OpenOptions {
                storage: Some(backend.backends()),
                migrate: true,
            },
        )
        .unwrap();
        seed(&lib, &base, false, false);
        let path = base.join("track.flac");
        let backup = base.join(".cover-backup-known");
        let staged = base.join(".cover-stage-known");
        for p in [&path, &backup, &staged] {
            std::fs::write(p, b"registered bytes").unwrap();
        }
        let target = serde_json::json!({
        "track":1,
        "path":path,
        "group":"pending",
        "operation":"remove",
        "undo_of":null,
        "at":0,
        "baseline":{
        "hidden":null,
        "stamp":[16,
        0],
        "picture":null}
        ,
        "staged":{
        "path":staged,
        "identity":[1,
        2]}
        ,
        "backup":{
        "path":backup,
        "identity":[3,
        4]}
        }
        )
        .to_string();

        lib.inner
            .writer
            .run(move |conn| {
                conn.execute(
                    INSERT_FILE_WRITE_INTENTS,
                    rusqlite::params![
                        r#"{"origin":"ui","grant":null,"checked":false}"#,
                        target,
                        stage
                    ],
                )?;
                Ok(())
            })
            .unwrap();
        let before = state(&lib);
        drop(lib);
        backend.calls.lock().unwrap().clear();
        backend.opened_paths.lock().unwrap().clear();
        let reopened = Library::open_with(
            &db,
            crate::OpenOptions {
                storage: Some(backend.backends()),
                migrate: false,
            },
        )
        .unwrap();
        assert!(
            backend
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|op| *op == "capabilities")
        );
        assert!(backend.opened_paths.lock().unwrap().is_empty());
        assert!(reopened.has_cover_recovery_required());
        assert_eq!(reopened.cover_recovery_required().unwrap(), [TrackId(1)]);
        let guard = reopened.describe_cover_recovery(TrackId(1)).unwrap();
        assert_eq!(
            reopened
                .accept_cover_recovery(&guard, UI)
                .err()
                .unwrap()
                .code(),
            "cover_edit_unsupported"
        );
        assert_eq!(state(&reopened), before);
        for p in [&path, &backup, &staged] {
            assert_eq!(std::fs::read(p).unwrap(), b"registered bytes");
        }
        assert!(
            !backend
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|op| matches!(*op, "resume" | "begin"))
        );
    }
}

#[test]
fn storage_io_errors_preserve_kind_raw_os_error_and_source() {
    use std::error::Error;
    let backend = LocalStorage {
        open_error: Some(123),
        ..Default::default()
    };
    let translated =
        crate::storage_access::open_regular(&backend, Path::new("injected-error"), true)
            .err()
            .unwrap();
    let LibraryError::Io(io) = &translated else {
        panic!("lost I/O error: {translated}")
    };
    assert_eq!(io.raw_os_error(), Some(123));
    assert_eq!(
        translated.source().map(ToString::to_string),
        io.source().map(ToString::to_string)
    );
    let expected = std::io::Error::from_raw_os_error(123);
    let error = mpmp_storage::StorageError::Io(std::io::Error::from_raw_os_error(123));
    let actual = error
        .source()
        .unwrap()
        .downcast_ref::<std::io::Error>()
        .unwrap();
    assert_eq!(actual.kind(), expected.kind());
    assert_eq!(actual.raw_os_error(), expected.raw_os_error());
    assert_eq!(actual.to_string(), expected.to_string());
    let wrapped = LibraryError::from(error);
    let actual = wrapped
        .source()
        .unwrap()
        .downcast_ref::<std::io::Error>()
        .unwrap();
    assert_eq!(actual.kind(), expected.kind());
    assert_eq!(actual.raw_os_error(), Some(123));
}

#[test]
fn a_legacy_adapter_must_advertise_guarded_tag_publication() {
    use mpmp_storage::{
        DurableReplace, Operation, ReplacementPaths, ReplacementTransaction, StorageError,
        StorageResult, WriteCapabilities,
    };
    struct OldReplacement;
    impl DurableReplace for OldReplacement {
        fn supported(&self) -> bool {
            true
        }
        fn begin(&self, _: ReplacementPaths) -> StorageResult<Box<dyn ReplacementTransaction>> {
            panic!("tag entry points must reject this adapter before starting a transaction")
        }
        fn resume(&self, _: ReplacementPaths) -> StorageResult<Box<dyn ReplacementTransaction>> {
            self.begin(ReplacementPaths {
                destination: mpmp_storage::LocalPath("unused".into()),
                staged: None,
                backup: None,
                saved_after: None,
            })
        }
    }

    let (_dir, base) = temp();
    let backend = Arc::new(LocalStorage {
        capabilities: Some(WriteCapabilities {
            durable_replace: true,
            legacy_tag_write: true,
        }),
        ..Default::default()
    });
    let mut backends = backend.backends();
    backends.replacements = Arc::new(OldReplacement);
    let library = Library::open_with(
        &base.join("library.db"),
        OpenOptions::with_storage(backends),
    )
    .unwrap();
    let capabilities = library.storage_write_capabilities();
    assert!(capabilities.durable_replace);
    assert!(!capabilities.legacy_tag_write);
    assert!(matches!(
        library.preview_tag_edit(&[TrackId(99)], &[]),
        Err(LibraryError::Storage(StorageError::Unsupported {
            operation: Operation::TagWrite
        }))
    ));
}
