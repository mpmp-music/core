use super::reset::HOOK;
use super::*;
use crate::{Library, OpenOptions};
use mpmp_storage::StorageBackends;
use rusqlite::{OpenFlags, types::Value};
use std::{collections::BTreeMap, path::Path};

pub(super) fn storage() -> StorageBackends {
    std::sync::Arc::new(mpmp_test_support::local_storage::LocalStorage {
        identities: true,
        ..Default::default()
    })
    .backends()
}
pub(super) fn open(path: &Path, migrate: bool) -> crate::Result<Library> {
    Library::open_with(
        path,
        OpenOptions {
            storage: Some(storage()),
            migrate,
        },
    )
}
pub(super) fn development(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    conn.execute_batch(include_str!("../../../tests/data/development-v13.sql"))
        .unwrap();
    conn.execute_batch("COMMIT; PRAGMA foreign_keys=OFF")
        .unwrap();
    conn
}
fn seed(conn: &Connection) {
    conn.execute_batch("BEGIN").unwrap();
    conn.execute_batch(concat!(
        "INSERT INTO roots(id,path,added_at) VALUES(7,'/synthetic',9);",
        "INSERT INTO tracks(id,root_id,rel_path,size,mtime_ms,album_key,added_at,updated_at,artists_parser_version) VALUES(42,7,'one.flac',123,456,'album',1,2,0);",
        "INSERT INTO track_tags(rowid,track_id,key,value,value_norm) VALUES(3,42,'ARTIST','One','one'),(8,42,'ARTIST','Two','two'),(19,42,'ARTIST','Two','two');",
        "INSERT INTO track_user VALUES(42,5,1,4);",
        "INSERT INTO meta VALUES('rescan_recommended',2);",
        "INSERT INTO agents VALUES('synthetic',x'00ff01',1,NULL);",
        "INSERT INTO grants VALUES(3,'synthetic',NULL,'read','all',NULL,'allow',1);",
        "INSERT INTO playlists(id,name,name_norm,kind,created_at,updated_at) VALUES(5,'List','list','manual',1,2);",
        "INSERT INTO playlist_items(playlist_id,position,track_id) VALUES(5,0,42);",
        "INSERT INTO track_stats VALUES(42,4,2,NULL,99);",
        "INSERT INTO album_loudness VALUES('album',-14.25,0.75,9);",
        "INSERT INTO actions(id,at_ms,origin,kind,summary,target_count,grant_id,details_json) VALUES(9,1,'ui','set_loved','loved',1,3,'{}');",
        "INSERT INTO action_targets VALUES('track',42,4,9,3,4);",
        r#"INSERT INTO file_write_intents(family,format_version,authorization_json,target_json,before_digest,before_image,after_digest,stage,action_id,created_at,updated_at) VALUES('cover_edit',1,'{"origin":"ui","grant":null,"checked":false}','{"track":42,"path":"/synthetic/one.flac","group":"g","operation":"set","undo_of":null,"at":1}','b',x'000102ff','a','retained',9,1,1);"#,
        "INSERT INTO track_loudness VALUES(42,'v',-15.5,0.8,9,x'000102ff');",
        "INSERT INTO play_events(id,track_id,context,started_at,played_ms,ended_reason,origin) VALUES(81,42,NULL,2,3,'end','ui');",
        "INSERT INTO dirty_artists(rowid,artist_norm) VALUES(11,'unused'),(30,'unused'); ANALYZE;"
    )).unwrap();
    let key = crate::person::person_key("one");
    conn.execute("INSERT INTO person_artists VALUES(?1,'one')", [&key])
        .unwrap();
    conn.execute("INSERT INTO track_artists VALUES(42,?1,0,'One')", [&key])
        .unwrap();
    let inverse = crate::journal::Inverse::Loved(vec![(42, false)]).encode();
    conn.execute("UPDATE actions SET inverse=?1 WHERE id=9", [inverse])
        .unwrap();
    conn.execute_batch("COMMIT").unwrap();
}
pub(super) fn header(conn: &Connection) -> (i64, i64) {
    (
        conn.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap(),
        conn.pragma_query_value(None, "application_id", |r| r.get(0))
            .unwrap(),
    )
}
/// Independent, value-by-value assertions supplement the production streaming hash.
pub(super) fn snapshot(conn: &Connection) -> BTreeMap<String, Vec<Vec<Value>>> {
    let names = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    names
        .into_iter()
        .map(|name| {
            let q = fingerprint::quote(&name);
            let without: bool = conn
                .query_row(
                    "SELECT instr(sql,'WITHOUT ROWID')>0 FROM sqlite_schema WHERE name=?1",
                    [&name],
                    |r| r.get(0),
                )
                .unwrap();
            let sql = if without {
                format!("SELECT * FROM {q}")
            } else {
                format!("SELECT rowid,* FROM {q} ORDER BY rowid")
            };
            let mut stmt = conn.prepare(&sql).unwrap();
            let columns = stmt.column_count();
            let rows = stmt
                .query_map([], |r| {
                    (0..columns)
                        .map(|i| r.get(i))
                        .collect::<rusqlite::Result<Vec<Value>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            (name, rows)
        })
        .collect()
}
pub(super) struct Hook;
impl Drop for Hook {
    fn drop(&mut self) {
        HOOK.with(|h| *h.borrow_mut() = None);
    }
}
pub(super) fn hook(f: impl Fn(&str, &Connection) -> crate::Result<()> + 'static) -> Hook {
    HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
    Hook
}
pub(super) fn backups(path: &Path) -> Vec<std::path::PathBuf> {
    let directory = path.join("migration-backups");
    if !directory.exists() {
        return vec![];
    }
    std::fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect()
}

pub(super) fn assert_backups(directory: &Path, before: &BTreeMap<String, Vec<Vec<Value>>>) {
    for path in backups(directory) {
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(header(&conn), (13, 0));
        assert_eq!(fingerprint::schema(&conn).unwrap(), SCHEMA_SHA256);
        assert_eq!(
            conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert!(
            !conn
                .prepare("PRAGMA foreign_key_check")
                .unwrap()
                .exists([])
                .unwrap()
        );
        let mut actual = snapshot(&conn);
        let mut expected = before.clone();
        // VACUUM may change implicit rowids, but every stored value must agree.
        for (table, rows) in &mut actual {
            let without: bool = conn
                .query_row(
                    "SELECT instr(sql,'WITHOUT ROWID')>0 FROM sqlite_schema WHERE name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            if !without {
                for row in rows {
                    row.remove(0);
                }
                for row in expected.get_mut(table).unwrap() {
                    row.remove(0);
                }
            }
        }
        assert_eq!(actual, expected, "backup {}", path.display());
        assert!(
            conn.execute("INSERT INTO meta VALUES('readonly',1)", [])
                .is_err()
        );
    }
}

#[test]
fn historical_v13_fixture_has_independent_provenance_and_matches_baseline() {
    use sha2::{Digest, Sha256};
    let fixture = include_bytes!("../../../tests/data/development-v13.sql");
    assert_eq!(
        format!("{:x}", Sha256::digest(fixture)),
        "0829e24744a220e7b0ef26ca58f2477187495c2aebb627f324f6fcd48ff0625f"
    );
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = development(&path);
    assert_eq!(header(&conn), (13, 0));
    assert_eq!(fingerprint::schema(&conn).unwrap(), SCHEMA_SHA256);
    assert!(
        Library::inspect_schema(&path)
            .unwrap()
            .contains("eligible development v13")
    );
}

#[test]
fn a_crlf_checkout_of_the_ddl_still_creates_the_published_schema() {
    let crlf = super::V1_SOURCE.replace("\r\n", "\n").replace('\n', "\r\n");
    let raw = Connection::open_in_memory().unwrap();
    raw.execute_batch(&crlf).unwrap();
    assert_ne!(fingerprint::schema(&raw).unwrap(), SCHEMA_SHA256);
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&super::lf(&crlf)).unwrap();
    assert_eq!(fingerprint::schema(&conn).unwrap(), SCHEMA_SHA256);
}

#[test]
fn v1_ddl_matches_author_v13_exact_schema_fingerprint() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&v1()).unwrap();
    assert_eq!(
        fingerprint::schema(&conn).unwrap(),
        "a0c2e40b19c08cb291dc067e723e1a5bbc82ae09e068a9aaccc95bcfa3438a26"
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        61
    );
    assert_eq!(APPLICATION_ID, 0x4d504d50);
}

#[test]
fn missing_and_empty_databases_initialize_published_1_atomically() {
    for existing in [false, true] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        if existing {
            drop(Connection::open(&path).unwrap());
        }
        drop(open(&path, false).unwrap());
        let conn = Connection::open(&path).unwrap();
        assert_eq!(header(&conn), (1, APPLICATION_ID));
        assert_eq!(fingerprint::schema(&conn).unwrap(), SCHEMA_SHA256);
        assert!(backups(dir.path()).is_empty());
        drop(open(&path, false).unwrap());
        assert_eq!(Library::take_upgrade_notice(dir.path()).unwrap(), None);
    }
}

#[test]
fn development_1_through_12_are_rejected_with_and_without_migrate() {
    for version in 1..=12 {
        for migrate in [false, true] {
            let dir = mpmp_test_support::TempDir::new().unwrap();
            let path = dir.path().join("library.db");
            let conn = development(&path);
            conn.pragma_update(None, "user_version", version).unwrap();
            drop(conn);
            let before = std::fs::read(&path).unwrap();
            let error = open(&path, migrate).err().unwrap().to_string();
            assert!(
                error.contains(&format!("检测到旧开发版数据库 schema v{version}")),
                "{error}"
            );
            assert!(error.contains("不要手动修改 user_version"));
            assert!(!error.contains("--allow-migrate"));
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(backups(dir.path()).is_empty());
            assert!(!dir.path().join(reset::RESET_NOTICE).exists());
        }
    }
}

#[test]
fn migrate_false_recognizes_v13_without_backup_notice_or_writes() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = development(&path);
    seed(&conn);
    drop(conn);
    let before = std::fs::read(&path).unwrap();
    assert!(
        open(&path, false)
            .err()
            .unwrap()
            .to_string()
            .contains("--allow-migrate")
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(backups(dir.path()).is_empty());
    assert!(!dir.path().join(reset::RESET_NOTICE).exists());
    assert!(!dir.path().join("library-schema-reset-pending").exists());
}

#[test]
fn reset_preserves_all_values_rowids_schema_and_wal_and_backup_is_readonly_v13() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = development(&path);
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    seed(&conn);
    let before = snapshot(&conn);
    let data = fingerprint::data(&conn).unwrap();
    // This SQLite build preserves rowids in VACUUM INTO. Simulate the legal
    // renumbering of an older SQLite build, keeping relative order and every value.
    let directory = dir.path().to_owned();
    let vacuum_rowids = hook(move |phase, _| {
        if phase == "backup_written" {
            let partial = backups(&directory).pop().unwrap();
            Connection::open(partial)?.execute("UPDATE dirty_artists SET rowid=rowid+1000", [])?;
        }
        Ok(())
    });
    let (lib, _) = prepare(&path, &storage(), true).unwrap();
    drop(vacuum_rowids);
    assert_eq!(header(&conn), (1, APPLICATION_ID));
    assert_eq!(snapshot(&conn), before);
    assert_eq!(fingerprint::data(&conn).unwrap(), data);
    assert_eq!(fingerprint::schema(&conn).unwrap(), SCHEMA_SHA256);
    let backup = Library::take_upgrade_notice(dir.path()).unwrap().unwrap();
    assert_eq!(Library::take_upgrade_notice(dir.path()).unwrap(), None);
    let mut saved = Connection::open_with_flags(&backup, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(header(&saved), (13, 0));
    assert_eq!(fingerprint::schema(&saved).unwrap(), SCHEMA_SHA256);
    let backup_data = fingerprint::data(&saved).unwrap();
    assert_eq!(backup_data.content, data.content);
    assert_ne!(
        backup_data.rowids, data.rowids,
        "fixture must exercise VACUUM implicit-rowid renumbering"
    );
    assert_eq!(
        saved
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert!(saved.transaction().is_ok());
    assert!(
        saved
            .execute("INSERT INTO meta VALUES('cannot-write',1)", [])
            .is_err()
    );
    drop(lib);
    // Recognition itself does not run ANALYZE, pruning or recovery again.
    let (_lease, reset) = reset::prepare(&path, &storage(), true).unwrap();
    assert!(!reset);
    assert_eq!(backups(dir.path()).len(), 1);
    assert_eq!(Library::take_upgrade_notice(dir.path()).unwrap(), None);
}

#[test]
fn early_v13_variants_keys_references_formats_and_unknown_objects_are_rejected() {
    let variants = [
        "DROP INDEX tracks_file; CREATE UNIQUE INDEX tracks_file ON tracks(root_id,rel_path) WHERE cue_sheet IS NULL;",
        "INSERT INTO person_artists VALUES('name:one','one');",
        "INSERT INTO person_artists VALUES('person:v1:wrong','one');",
        "CREATE TABLE unknown_object(id);",
        "CREATE TRIGGER unknown_trigger AFTER INSERT ON meta BEGIN SELECT 1; END;",
        "INSERT INTO track_artists VALUES(999,'missing',0,'Missing');",
        "INSERT INTO actions(at_ms,origin,kind,summary,target_count,inverse) VALUES(1,'ui','future','future',1,x'ff');",
        "INSERT INTO file_write_intents(family,format_version,authorization_json,target_json,before_digest,before_image,after_digest,stage,created_at,updated_at) VALUES('future',77,'{}','{}','b',x'00','a','future',1,1);",
        "PRAGMA application_id=77;",
        "DROP TRIGGER loudness_track_version;",
        "INSERT INTO file_write_intents(family,format_version,authorization_json,target_json,before_digest,before_image,after_digest,stage,created_at,updated_at) VALUES('cover_edit',1,'{}','{}','b',x'00','a','retained',1,1);",
    ];
    for sql in variants {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        let conn = development(&path);
        conn.execute_batch(sql).unwrap();
        drop(conn);
        let before = std::fs::read(&path).unwrap();
        let probe_error = Library::inspect_schema(&path).unwrap_err().to_string();
        assert!(
            probe_error.contains("检测到早期开发版v13"),
            "{sql}: {probe_error}"
        );
        let error = open(&path, true).err().unwrap().to_string();
        assert!(error.contains("检测到早期开发版v13"), "{sql}: {error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(backups(dir.path()).is_empty());
    }
}

#[test]
fn future_published_schemas_foreign_markers_nonempty_zero_and_damage_are_distinct() {
    for (version, app, expected) in [
        (2, APPLICATION_ID, "schema_too_new"),
        (13, APPLICATION_ID, "schema_too_new"),
        (0, 0, "io"),
        (-1, 0, "io"),
        (99, 0, "io"),
        (1, 123, "io"),
    ] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        let conn = development(&path);
        conn.pragma_update(None, "user_version", version).unwrap();
        conn.pragma_update(None, "application_id", app).unwrap();
        drop(conn);
        let before = std::fs::read(&path).unwrap();
        assert_eq!(open(&path, true).err().unwrap().code(), expected);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    std::fs::write(&path, vec![0x5a; 8192]).unwrap();
    assert!(open(&path, true).err().unwrap().is_damaged());
}

#[test]
fn every_precommit_failure_keeps_v13_all_values_and_retains_published_backups() {
    for at in [
        "space_checked",
        "before_vacuum",
        "backup_written",
        "backup_verified",
        "before_backup_sync",
        "before_backup_publish",
        "backup_persisted",
        "pending_persisted",
        "transaction_checked",
        "application_id_written",
        "version_written",
        "before_commit",
    ] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        let conn = development(&path);
        seed(&conn);
        let before = snapshot(&conn);
        drop(conn);
        let injection = hook(move |phase, _| {
            if phase == at {
                Err(std::io::Error::other(at).into())
            } else {
                Ok(())
            }
        });
        assert!(open(&path, true).is_err(), "{at}");
        drop(injection);
        let conn = Connection::open(&path).unwrap();
        assert_eq!(header(&conn), (13, 0), "{at}");
        assert_eq!(snapshot(&conn), before, "{at}");
        assert_eq!(fingerprint::schema(&conn).unwrap(), SCHEMA_SHA256);
        let files = backups(dir.path());
        assert!(
            files.iter().all(|p| p.extension().unwrap() == "sqlite"),
            "partial left after {at}"
        );
        assert_eq!(
            files.len(),
            usize::from(matches!(
                at,
                "backup_persisted"
                    | "pending_persisted"
                    | "transaction_checked"
                    | "application_id_written"
                    | "version_written"
                    | "before_commit"
            )),
            "{at}"
        );
        assert_backups(dir.path(), &before);
        assert_eq!(Library::take_upgrade_notice(dir.path()).unwrap(), None);
    }
}

#[test]
fn failed_commit_rolls_back_both_headers_and_keeps_backup_and_pending() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = development(&path);
    seed(&conn);
    let before = snapshot(&conn);
    drop(conn);
    let injection = hook(|phase, conn| {
        if phase == "before_commit" {
            conn.commit_hook(Some(|| true))?;
        }
        Ok(())
    });
    assert!(open(&path, true).is_err());
    drop(injection);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(header(&conn), (13, 0));
    assert_eq!(snapshot(&conn), before);
    assert_eq!(backups(dir.path()).len(), 1);
    assert_backups(dir.path(), &before);
    assert!(dir.path().join("library-schema-reset-pending").exists());
    assert_eq!(Library::take_upgrade_notice(dir.path()).unwrap(), None);
    let old_backup = backups(dir.path()).pop().unwrap();
    let old_bytes = std::fs::read(&old_backup).unwrap();
    drop(conn);
    drop(open(&path, true).unwrap());
    assert_eq!(backups(dir.path()).len(), 2);
    assert_eq!(std::fs::read(&old_backup).unwrap(), old_bytes);
    assert!(Library::take_upgrade_notice(dir.path()).unwrap().is_some());
}

#[test]
fn external_write_after_backup_is_detected_before_header_changes() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = development(&path);
    seed(&conn);
    let before = snapshot(&conn);
    drop(conn);
    let other = path.clone();
    let injection = hook(move |phase, _| {
        if phase == "backup_persisted" {
            Connection::open(&other)?.execute("INSERT INTO meta VALUES('external',99)", [])?;
        }
        Ok(())
    });
    let error = open(&path, true).err().unwrap();
    drop(injection);
    assert!(error.to_string().contains("Database changed after backup"));
    let conn = Connection::open(&path).unwrap();
    assert_eq!(header(&conn), (13, 0));
    assert_eq!(
        conn.query_row("SELECT value FROM meta WHERE key='external'", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        99
    );
    assert_backups(dir.path(), &before);
    let mut after = snapshot(&conn);
    let external = vec![Value::Text("external".into()), Value::Integer(99)];
    let meta = after.get_mut("meta").unwrap();
    let index = meta.iter().position(|row| row == &external).unwrap();
    meta.remove(index);
    assert_eq!(after, before, "all existing rows must remain unchanged");
    assert_eq!(backups(dir.path()).len(), 1);
}

#[test]
fn corrupt_partial_backup_is_rejected_and_cleaned() {
    for physical in [false, true] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        let conn = development(&path);
        seed(&conn);
        let before = snapshot(&conn);
        drop(conn);
        let directory = dir.path().to_owned();
        let injection = hook(move |phase, _| {
            if phase == "backup_written" {
                let partial = backups(&directory).pop().unwrap();
                if physical {
                    std::fs::write(partial, vec![0x5a; 8192])?;
                } else {
                    Connection::open(partial)?
                        .execute("INSERT INTO meta VALUES('corruption',123)", [])?;
                }
            }
            Ok(())
        });
        let error = open(&path, true).err().unwrap();
        drop(injection);
        assert_eq!(error.code(), "io");
        assert!(
            !error.is_damaged(),
            "backup damage must not condemn the original"
        );
        assert!(error.to_string().contains("Backup verification failed"));
        let conn = Connection::open(&path).unwrap();
        assert_eq!(header(&conn), (13, 0));
        assert_eq!(snapshot(&conn), before);
        assert!(backups(dir.path()).is_empty());
    }
}

#[test]
fn low_space_capacity_errors_and_blocked_backup_directory_never_reset() {
    struct NoSpace(bool);
    impl mpmp_storage::DatabaseStorage for NoSpace {
        fn available_space(&self, _: &Path) -> std::io::Result<u64> {
            if self.0 {
                Ok(0)
            } else {
                Err(std::io::Error::other("capacity query failed"))
            }
        }
    }
    for failure in 0..3 {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        let conn = development(&path);
        seed(&conn);
        let before = snapshot(&conn);
        drop(conn);
        let mut backend = storage();
        if failure < 2 {
            backend.database = std::sync::Arc::new(NoSpace(failure == 0));
        } else {
            std::fs::write(dir.path().join("migration-backups"), b"blocked").unwrap();
        }
        let error = Library::open_with(&path, OpenOptions::with_storage(backend))
            .err()
            .unwrap();
        if failure == 0 {
            assert!(error.to_string().contains("insufficient free space"));
        }
        let conn = Connection::open(&path).unwrap();
        assert_eq!(header(&conn), (13, 0));
        assert_eq!(snapshot(&conn), before);
        assert!(!dir.path().join(reset::RESET_NOTICE).exists());
    }
}

#[test]
fn pending_receipt_recovers_after_commit_notice_failure_and_history_is_readonly() {
    for at in [
        "after_commit",
        "before_notice",
        "completed_receipt_persisted",
    ] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        let conn = development(&path);
        seed(&conn);
        drop(conn);
        let history = dir.path().join("library-v13-upgrade-notice");
        std::fs::write(&history, "historical-v12-backup.sqlite").unwrap();
        let injection = hook(move |phase, _| {
            if phase == at {
                Err(std::io::Error::other("notice unavailable").into())
            } else {
                Ok(())
            }
        });
        drop(open(&path, true).unwrap());
        drop(injection);
        assert!(dir.path().join("library-schema-reset-pending").exists());
        assert_eq!(Library::take_upgrade_notice(dir.path()).unwrap(), None);
        assert_eq!(
            header(&Connection::open(&path).unwrap()),
            (1, APPLICATION_ID)
        );
        drop(open(&path, false).unwrap()); // migrate=false does not consume pending evidence.
        assert!(dir.path().join("library-schema-reset-pending").exists());
        drop(open(&path, true).unwrap());
        assert!(Library::take_upgrade_notice(dir.path()).unwrap().is_some());
        assert_eq!(Library::take_upgrade_notice(dir.path()).unwrap(), None);
        assert_eq!(backups(dir.path()).len(), 1);
        assert_eq!(
            Library::historical_upgrade_notice(dir.path())
                .unwrap()
                .as_deref(),
            Some("historical-v12-backup.sqlite")
        );
        assert!(history.exists());
    }
}

#[test]
fn lifetime_maintenance_lock_excludes_reset_while_another_library_writer_is_live() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let live = open(&path, true).unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "user_version", 13).unwrap();
    conn.pragma_update(None, "application_id", 0).unwrap();
    assert!(
        open(&path, true)
            .err()
            .unwrap()
            .to_string()
            .contains("Stop other library writers")
    );
    assert_eq!(header(&conn), (13, 0));
    assert!(backups(dir.path()).is_empty());
    drop(live);
    drop(open(&path, true).unwrap());
    assert_eq!(header(&conn), (1, APPLICATION_ID));
}

#[test]
fn initialization_errors_roll_back_schema_and_both_header_fields() {
    for at in [
        "initialized_schema",
        "initialized_marker",
        "initialized_version",
    ] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        let injection = hook(move |phase, _| {
            if phase == at {
                Err(std::io::Error::other(at).into())
            } else {
                Ok(())
            }
        });
        assert!(open(&path, true).is_err());
        drop(injection);
        let conn = Connection::open(&path).unwrap();
        assert_eq!(header(&conn), (0, 0));
        assert_eq!(
            conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(backups(dir.path()).is_empty());
    }
}

#[test]
fn sqlite_readonly_vacuum_denial_and_disk_full_leave_original_v13() {
    for failure in ["readonly", "vacuum", "full"] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        let conn = development(&path);
        seed(&conn);
        let before = snapshot(&conn);
        drop(conn);
        let injection = hook(move |phase, conn| {
            if phase == "before_vacuum" {
                if failure == "readonly" {
                    conn.pragma_update(None, "query_only", true)?;
                }
                if failure == "vacuum" {
                    conn.authorizer(Some(|ctx: rusqlite::hooks::AuthContext<'_>| {
                        if matches!(ctx.action, rusqlite::hooks::AuthAction::Attach { .. }) {
                            rusqlite::hooks::Authorization::Deny
                        } else {
                            rusqlite::hooks::Authorization::Allow
                        }
                    }))?;
                }
            }
            if failure == "full" && phase == "application_id_written" {
                return Err(crate::LibraryError::Db(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
                    None,
                )));
            }
            Ok(())
        });
        assert!(open(&path, true).is_err(), "{failure}");
        drop(injection);
        let conn = Connection::open(&path).unwrap();
        assert_eq!(header(&conn), (13, 0));
        assert_eq!(snapshot(&conn), before);
        assert_eq!(backups(dir.path()).len(), usize::from(failure == "full"));
        assert_backups(dir.path(), &before);
        assert_eq!(Library::take_upgrade_notice(dir.path()).unwrap(), None);
    }
}

#[test]
fn external_write_lock_timeout_keeps_v13_and_retains_backup() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = development(&path);
    seed(&conn);
    let before = snapshot(&conn);
    drop(conn);
    let external_path = path.clone();
    let writer = std::cell::RefCell::new(None);
    let injection = hook(move |phase, _| {
        if phase == "backup_persisted" {
            let conn = Connection::open(&external_path)?;
            conn.execute_batch("BEGIN IMMEDIATE")?;
            *writer.borrow_mut() = Some(conn);
        }
        Ok(())
    });
    assert!(
        open(&path, true)
            .err()
            .unwrap()
            .to_string()
            .contains("locked")
    );
    drop(injection);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(header(&conn), (13, 0));
    assert_eq!(snapshot(&conn), before);
    assert_eq!(backups(dir.path()).len(), 1);
    assert_backups(dir.path(), &before);
}

#[cfg(unix)]
#[test]
fn path_replacement_is_detected_before_header_writes() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = development(&path);
    seed(&conn);
    let before = snapshot(&conn);
    drop(conn);
    let source = path.clone();
    let moved = dir.path().join("moved.db");
    let moved_to = moved.clone();
    let injection = hook(move |phase, _| {
        if phase == "backup_persisted" {
            std::fs::rename(&source, &moved_to)?;
            std::fs::copy(&moved_to, &source)?;
        }
        Ok(())
    });
    assert!(
        open(&path, true)
            .err()
            .unwrap()
            .to_string()
            .contains("identity changed")
    );
    drop(injection);
    for path in [&path, &moved] {
        let conn = Connection::open(path).unwrap();
        assert_eq!(header(&conn), (13, 0));
        assert_eq!(snapshot(&conn), before);
    }
    assert_eq!(backups(dir.path()).len(), 1);
    assert_backups(dir.path(), &before);
}

#[test]
fn pending_receipt_write_failure_retains_backup_without_reset() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = development(&path);
    seed(&conn);
    let before = snapshot(&conn);
    drop(conn);
    std::fs::create_dir(dir.path().join("library-schema-reset-pending")).unwrap();
    assert!(open(&path, true).is_err());
    let conn = Connection::open(&path).unwrap();
    assert_eq!(header(&conn), (13, 0));
    assert_eq!(snapshot(&conn), before);
    assert_eq!(backups(dir.path()).len(), 1);
    assert_backups(dir.path(), &before);
    assert_eq!(Library::take_upgrade_notice(dir.path()).unwrap(), None);
}

#[test]
fn reset_from_delete_mode_preserves_journal_mode_and_foreign_keys() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = development(&path);
    seed(&conn);
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    assert_eq!(
        conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "delete"
    );
    let before = snapshot(&conn);
    let lib = prepare(&path, &storage(), true).unwrap();
    assert_eq!(header(&conn), (1, APPLICATION_ID));
    assert_eq!(snapshot(&conn), before);
    assert_eq!(
        conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "delete"
    );
    assert!(
        conn.pragma_query_value::<bool, _>(None, "foreign_keys", |r| r.get(0))
            .unwrap()
    );
    drop(lib);
}

#[test]
fn pending_receipt_requires_matching_backup_and_database_identity() {
    for tamper in ["backup", "identity"] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        let conn = development(&path);
        seed(&conn);
        let before = snapshot(&conn);
        drop(conn);
        let injection = hook(|phase, _| {
            if phase == "after_commit" {
                Err(std::io::Error::other("crash before notice").into())
            } else {
                Ok(())
            }
        });
        drop(prepare(&path, &storage(), true).unwrap());
        drop(injection);
        let pending = dir.path().join("library-schema-reset-pending");
        if tamper == "backup" {
            let backup = backups(dir.path()).pop().unwrap();
            Connection::open(backup)
                .unwrap()
                .execute("INSERT INTO meta VALUES('tampered',9)", [])
                .unwrap();
        } else {
            let mut receipt: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&pending).unwrap()).unwrap();
            receipt["identity"] = serde_json::json!([0, 0]);
            std::fs::write(&pending, serde_json::to_vec(&receipt).unwrap()).unwrap();
        }
        let (_guard, reset) = reset::prepare(&path, &storage(), true).unwrap();
        assert!(!reset);
        assert_eq!(
            header(&Connection::open(&path).unwrap()),
            (1, APPLICATION_ID)
        );
        assert_eq!(snapshot(&Connection::open(&path).unwrap()), before);
        assert!(pending.exists());
        assert_eq!(Library::take_upgrade_notice(dir.path()).unwrap(), None);
        assert_eq!(backups(dir.path()).len(), 1);
    }
}

#[test]
fn concurrent_published_opens_and_notice_consumption_publish_once() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    use std::time::Duration;
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    drop(development(&path));
    let fault = hook(|phase, _| {
        if phase == "after_commit" {
            return Err(std::io::Error::other("pending only").into());
        }
        Ok(())
    });
    drop(prepare(&path, &storage(), true).unwrap());
    drop(fault);
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let publications = Arc::new(AtomicUsize::new(0));
    let count = publications.clone();
    let first_path = path.clone();
    let first = std::thread::spawn(move || {
        let _hook = hook(move |phase, _| {
            if phase == "before_notice" {
                count.fetch_add(1, Ordering::SeqCst);
                held_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            Ok(())
        });
        open(&first_path, true).unwrap()
    });
    held_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let second_path = path.clone();
    let count = publications.clone();
    let second = std::thread::spawn(move || {
        let _hook = hook(move |phase, _| {
            if phase == "before_notice_lock" {
                entered_tx.send(()).unwrap();
            }
            if phase == "before_notice" {
                count.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        });
        let lib = open(&second_path, true).unwrap();
        done_tx.send(()).unwrap();
        lib
    });
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(
        done_rx.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    let notice_dir = dir.path().to_owned();
    let consumer = std::thread::spawn(move || Library::take_upgrade_notice(&notice_dir).unwrap());
    release_tx.send(()).unwrap();
    let first = first.join().unwrap();
    let second = second.join().unwrap();
    assert!(consumer.join().unwrap().is_some());
    assert_eq!(publications.load(Ordering::SeqCst), 1);
    assert_eq!(Library::take_upgrade_notice(dir.path()).unwrap(), None);
    assert!(!dir.path().join("library-schema-reset-pending").exists());
    assert_eq!(backups(dir.path()).len(), 1);
    drop((first, second));
}

#[test]
fn published_open_waits_for_rollback_writer_before_enabling_wal() {
    use crate::db::writer::ON_WAL_BUSY;
    use std::{sync::mpsc, time::Duration};

    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    drop(prepare(&path, &storage(), true).unwrap());
    let blocker = Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

    let (busy_tx, busy_rx) = mpsc::channel();
    let (released_tx, released_rx) = mpsc::channel();
    let open_path = path.clone();
    let opener = std::thread::spawn(move || {
        ON_WAL_BUSY.set(Some(Box::new(move || {
            busy_tx.send(()).unwrap();
            released_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        })));
        open(&open_path, false)
    });
    busy_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    blocker.execute_batch("ROLLBACK").unwrap();
    released_tx.send(()).unwrap();
    let library = opener.join().unwrap().unwrap();
    let mode: String = blocker
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    drop(library);
}

#[test]
fn published_open_reports_sqlite_busy_when_wal_upgrade_times_out() {
    use crate::{LibraryError, db::BUSY_TIMEOUT};
    use std::error::Error;
    use std::time::{Duration, Instant};

    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    drop(prepare(&path, &storage(), true).unwrap());
    let blocker = Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

    let started = Instant::now();
    let Err(error) = open(&path, false) else {
        panic!("opening switched the database to WAL while another writer held the lock");
    };
    let elapsed = started.elapsed();
    assert!(
        (BUSY_TIMEOUT..=BUSY_TIMEOUT + Duration::from_secs(2)).contains(&elapsed),
        "opening returned after {elapsed:?}"
    );
    assert!(matches!(error, LibraryError::DatabaseBusy(_)), "{error}");
    assert_eq!(error.code(), "db");
    assert_eq!(
        error.to_string(),
        "曲库当前正被占用，请稍后重试。\nThe library database is busy. Try again shortly."
    );
    let source = error
        .source()
        .and_then(|source| source.downcast_ref::<rusqlite::Error>())
        .unwrap();
    assert_eq!(
        source.sqlite_error().unwrap().extended_code,
        rusqlite::ffi::SQLITE_BUSY
    );
    blocker.execute_batch("ROLLBACK").unwrap();
    open(&path, false).unwrap().flush().unwrap();
}

#[test]
fn identity_unsupported_backend_opens_published_but_refuses_development_reset() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let backend = std::sync::Arc::new(mpmp_test_support::local_storage::LocalStorage {
        identities: false,
        ..Default::default()
    });
    for migrate in [false, true] {
        drop(
            Library::open_with(
                &path,
                OpenOptions {
                    storage: Some(backend.backends()),
                    migrate,
                },
            )
            .unwrap(),
        );
    }
    assert!(backend.opened_paths.lock().unwrap().is_empty());
    assert!(
        backend
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|op| *op == "capabilities")
    );
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "application_id", 0).unwrap();
    conn.pragma_update(None, "user_version", 13).unwrap();
    let before = snapshot(&conn);
    drop(conn);
    let bytes = std::fs::read(&path).unwrap();
    let error = Library::open_with(&path, OpenOptions::with_storage(backend.backends()))
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("Storage backend cannot verify database identity")
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(header(&conn), (13, 0));
    assert_eq!(snapshot(&conn), before);
    assert!(backups(dir.path()).is_empty());
    assert!(backend.opened_paths.lock().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn dangling_database_symlink_is_rejected_without_creating_target() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let target = dir.path().join("missing.db");
    std::os::unix::fs::symlink(&target, &path).unwrap();
    for migrate in [false, true] {
        assert!(
            open(&path, migrate)
                .err()
                .unwrap()
                .to_string()
                .contains("must not be a symlink")
        );
        assert!(!target.exists());
        assert_eq!(std::fs::read_link(&path).unwrap(), target);
        assert!(backups(dir.path()).is_empty());
    }
}

#[cfg(feature = "desktop-fs")]
#[test]
fn reset_session_runs_normal_wal_optimize_cover_recovery_and_pruning() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = development(&path);
    seed(&conn);
    // A finalized intent with a matching live file must be cleaned on this open.
    let media = dir.path().join("synthetic.flac");
    std::fs::write(&media, b"synthetic media bytes").unwrap();
    let digest =
        crate::cover::edit::file_digest(&mut std::fs::File::open(&media).unwrap()).unwrap();
    conn.execute(
        "UPDATE file_write_intents SET stage='finalized',after_digest=?1,target_json=json_set(target_json,'$.path',?2)",
        rusqlite::params![digest, media.to_str().unwrap()],
    ).unwrap();
    conn.execute_batch("DROP TABLE sqlite_stat1").unwrap();
    let before = snapshot(&conn);
    drop(conn);
    drop(Library::open(&path).unwrap());
    let conn = Connection::open(&path).unwrap();
    assert_eq!(header(&conn), (1, APPLICATION_ID));
    assert_eq!(
        conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
    assert!(
        conn.prepare("SELECT 1 FROM sqlite_stat1 WHERE tbl='tracks'")
            .unwrap()
            .exists([])
            .unwrap()
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM file_write_intents", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM actions", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_backups(dir.path(), &before);
}

#[test]
fn readonly_probe_runs_exact_checks_without_creating_files() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join(if cfg!(windows) {
        "library # 测试.db"
    } else {
        "library ? # 测试.db"
    });
    let conn = development(&path);
    seed(&conn);
    drop(conn);
    let before = std::fs::read(&path).unwrap();
    assert!(
        Library::inspect_schema(&path)
            .unwrap()
            .starts_with("eligible")
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    let conn = Connection::open(&path).unwrap();
    conn.execute("UPDATE track_artists SET ordinal=1", [])
        .unwrap();
    assert!(
        Library::inspect_schema(&path)
            .unwrap_err()
            .to_string()
            .contains("ordinal")
    );
    drop(conn);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn published_900_track_startup_retains_strict_recognition_at_measured_cost() {
    use std::time::Instant;
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = development(&path);
    conn.execute_batch(concat!(
        "BEGIN; INSERT INTO roots(id,path,added_at) VALUES(1,'/synthetic',1);",
        "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<900)",
        " INSERT INTO tracks(root_id,rel_path,size,mtime_ms,album_key,added_at,updated_at)",
        " SELECT 1,printf('%d.flac',i),10,1,'album',1,1 FROM n; ANALYZE; COMMIT;"
    ))
    .unwrap();
    drop(conn);
    drop(prepare(&path, &storage(), true).unwrap());
    let start = Instant::now();
    for _ in 0..20 {
        assert_eq!(
            Library::inspect_schema(&path).unwrap(),
            "published schema 1"
        );
    }
    let recognize = start.elapsed() / 20;
    let start = Instant::now();
    for _ in 0..20 {
        drop(open(&path, true).unwrap());
    }
    let startup = start.elapsed() / 20;
    eprintln!(
        "900 tracks, 20 warm iterations: strict recognition={recognize:?}, Library startup={startup:?}"
    );
    assert_eq!(
        header(&Connection::open(&path).unwrap()),
        (1, APPLICATION_ID)
    );
}

#[test]
fn postcommit_external_write_reports_published_header_and_retains_original_backup() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = development(&path);
    seed(&conn);
    let before = snapshot(&conn);
    drop(conn);
    let external = path.clone();
    let _hook = hook(move |phase, _| {
        if phase == "sqlite_commit_returned" {
            Connection::open(&external)?.execute("INSERT INTO meta VALUES('external',99)", [])?;
        }
        Ok(())
    });
    let error = prepare(&path, &storage(), true).err().unwrap();
    assert!(
        error
            .to_string()
            .contains("Header is published 1; post-commit verification failed")
    );
    let conn = Connection::open(&path).unwrap();
    assert_eq!(header(&conn), (1, APPLICATION_ID));
    let mut after = snapshot(&conn);
    after
        .get_mut("meta")
        .unwrap()
        .retain(|r| r[0] != Value::Text("external".into()));
    assert_eq!(after, before);
    assert_backups(dir.path(), &before);
    assert_eq!(backups(dir.path()).len(), 1);
    assert!(dir.path().join("library-schema-reset-pending").exists());
    assert_eq!(Library::take_upgrade_notice(dir.path()).unwrap(), None);
}

fn directory_metadata(path: &Path) -> BTreeMap<std::ffi::OsString, (u64, std::time::SystemTime)> {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            (entry.file_name(), (meta.len(), meta.modified().unwrap()))
        })
        .collect()
}

#[test]
fn readonly_probe_cleanly_closed_wal_succeeds_without_file_or_metadata_changes() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join(if cfg!(windows) {
        "library # 测试.db"
    } else {
        "library ? # 测试.db"
    });
    let conn = development(&path);
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    seed(&conn);
    drop(conn);
    let before = directory_metadata(dir.path());
    assert_eq!(before.len(), 1, "normal close must remove WAL and SHM");
    for _ in 0..2 {
        assert!(
            Library::inspect_schema(&path)
                .unwrap()
                .starts_with("eligible")
        );
        assert_eq!(directory_metadata(dir.path()), before);
    }
}

#[test]
fn readonly_probe_existing_wal_and_shm_includes_frames_without_metadata_changes() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = development(&path);
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    seed(&conn);
    let before = directory_metadata(dir.path());
    assert_eq!(before.len(), 3);
    assert!(
        Library::inspect_schema(&path)
            .unwrap()
            .starts_with("eligible")
    );
    assert_eq!(directory_metadata(dir.path()), before);
    // This invalid inverse lives only in WAL. immutable would silently miss it.
    conn.execute(
        concat!(
            "INSERT INTO actions(at_ms,origin,kind,summary,target_count,inverse) ",
            "VALUES(1,'ui','x','x',0,x'ff')"
        ),
        [],
    )
    .unwrap();
    let before = directory_metadata(dir.path());
    assert!(
        Library::inspect_schema(&path)
            .unwrap_err()
            .to_string()
            .contains("unreadable action encoding")
    );
    assert_eq!(directory_metadata(dir.path()), before);
}

#[test]
fn readonly_probe_refuses_journal_or_incomplete_siblings_without_writes() {
    for suffixes in [
        vec!["-wal"],
        vec!["-shm"],
        vec!["-journal"],
        vec!["-wal", "-shm", "-journal"],
    ] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        drop(development(&path));
        for suffix in suffixes {
            std::fs::write(dir.path().join(format!("library.db{suffix}")), []).unwrap();
        }
        let before = directory_metadata(dir.path());
        assert!(
            Library::inspect_schema(&path)
                .unwrap_err()
                .to_string()
                .contains("请在正常退出后重新复制")
        );
        assert_eq!(directory_metadata(dir.path()), before);
    }
}

#[test]
fn relabel_copy_reuses_reset_and_preserves_all_rows_before_normal_startup() {
    for mode in ["DELETE", "WAL"] {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        let conn = development(&path);
        seed(&conn);
        // Normal startup would create statistics and prune the old action.
        conn.execute_batch("DROP TABLE sqlite_stat1").unwrap();
        conn.pragma_update(None, "journal_mode", mode).unwrap();
        let before = snapshot(&conn);
        drop(conn);
        Library::relabel_schema_copy(&path, &storage()).unwrap();
        assert_eq!(
            Library::inspect_schema(&path).unwrap(),
            "published schema 1"
        );
        let conn = Connection::open(&path).unwrap();
        assert_eq!(header(&conn), (1, APPLICATION_ID));
        assert_eq!(snapshot(&conn), before);
        assert_eq!(fingerprint::schema(&conn).unwrap(), SCHEMA_SHA256);
        assert_eq!(
            conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                .unwrap(),
            mode.to_ascii_lowercase()
        );
        assert_backups(dir.path(), &before);
        assert_eq!(backups(dir.path()).len(), 1);
        let files = directory_metadata(dir.path());
        assert!(Library::relabel_schema_copy(&path, &storage()).is_err());
        assert_eq!(directory_metadata(dir.path()), files);
    }
}
