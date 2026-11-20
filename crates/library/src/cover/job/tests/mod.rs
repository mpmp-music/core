use super::*;
use crate::cover::edit::tests::{png, tagged_copy};
mod transactions;
const UI: ActionMeta<'static> = ActionMeta::new("ui");

struct Fixture {
    lib: Library,
    id: TrackId,
    path: PathBuf,
    // Keep the directory alive until the library has closed SQLite.
    dir: mpmp_test_support::TempDir,
}
impl Fixture {
    fn new() -> Self {
        Self::new_format("sine44_a.mp3")
    }
    fn new_format(name: &str) -> Self {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let music = dir.path().join("music");
        std::fs::create_dir(&music).unwrap();
        // Synthetic observation batches use the same spelling as stored roots.
        let music = storage_access::canonical_dir(&music).unwrap();
        let path = music.join(format!(
            "a.{}",
            Path::new(name).extension().unwrap().to_str().unwrap()
        ));
        tagged_copy(name, &path);
        let lib = Library::open(&dir.path().join("library.db")).unwrap();
        lib.add_root(&music, UI).unwrap();
        if let crate::ScanStart::Started(scan) = lib.scan(|_| {}).unwrap() {
            scan.wait().unwrap();
        }
        let id = lib
            .tracks_query(
                &crate::TrackQuery::default(),
                crate::Page {
                    offset: 0,
                    limit: 10,
                },
                &[],
            )
            .unwrap()
            .0[0]
            .track
            .id;
        Self { lib, id, path, dir }
    }
    fn change(&self) -> CoverChange {
        CoverChange::Set {
            mime: "image/png".into(),
            data: png(),
        }
    }
    fn edit(&self) -> Result<CoverEditReport> {
        self.lib
            .edit_cover(&[self.id], &self.change(), UI, TagJobOptions::default())
            .unwrap()
            .wait()
    }
    fn cover_actions(&self) -> usize {
        self.lib
            .actions(
                None,
                crate::Page {
                    offset: 0,
                    limit: 100,
                },
            )
            .unwrap()
            .items
            .iter()
            .filter(|a| a.kind == "cover_edit")
            .count()
    }
}

fn scan(f: &Fixture) {
    if let crate::ScanStart::Started(job) = f.lib.scan(|_| {}).unwrap() {
        job.wait().unwrap();
    }
}
fn targets(f: &Fixture) -> Vec<Target> {
    f.lib
        .inner
        .pool
        .read(|conn| {
            conn.prepare("SELECT target_json FROM file_write_intents ORDER BY id")?
                .query_map([], |r| r.get::<_, String>(0))?
                .map(|r| parse(&r?))
                .collect::<Result<_>>()
        })
        .unwrap()
}

#[cfg(unix)]
fn alias_scan_batches(target_upsert: bool) {
    use crate::db::writer::{ScanBatch, WriteCmd};
    let f = Fixture::new();
    let alias = f.path.with_file_name("alias.mp3");
    std::fs::hard_link(&f.path, &alias).unwrap();
    scan(&f);
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(f.edit().is_err());
    let replacement = f.path.with_file_name("new-inode");
    std::fs::copy(&f.path, &replacement).unwrap();
    std::fs::rename(replacement, &f.path).unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::fs::hard_link(&f.path, &alias).unwrap();
    let file = tag_job::file_refs(&f.lib.inner, &[f.id]).unwrap().remove(0);
    let root = file.track.root_id;
    let new_id = identity(&mpmp_fs_desktop::DesktopStorage, &f.path);
    let observed = Arc::new(ScanObservations::default());
    observed.record_root(
        f.path.parent().unwrap(),
        [("a.mp3", new_id), ("alias.mp3", new_id)].into_iter(),
    );
    let mut record = *tag_job::reread_record(
        &f.lib.inner,
        &file,
        &f.path,
        tag_job::stamp(&std::fs::metadata(&f.path).unwrap()),
    )
    .unwrap();
    record.file_identity = new_id;
    record.meta.title = Some("SHOULD BE BLOCKED".into());
    let mut alias_record = record.clone();
    alias_record.rel_path = "alias.mp3".into();
    // Every batch gets the walk observation even if the target has no upsert.
    if target_upsert {
        let batch = ScanBatch {
            observed: observed.clone(),
            upserts: vec![record],
            ..ScanBatch::default()
        };
        f.lib
            .inner
            .writer
            .call(|reply| WriteCmd::Scan {
                root_id: root,
                batch,
                reply,
            })
            .unwrap();
    }
    // The latest intent snapshot must still be merged, not cached from an earlier batch.
    let batch = ScanBatch {
        observed,
        upserts: vec![alias_record],
        ..ScanBatch::default()
    };
    f.lib
        .inner
        .writer
        .call(|reply| WriteCmd::Scan {
            root_id: root,
            batch,
            reply,
        })
        .unwrap();
    let alias_title = f
        .lib
        .inner
        .pool
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT title FROM tracks WHERE rel_path='alias.mp3'",
                [],
                |r| r.get::<_, Option<String>>(0),
            )?)
        })
        .unwrap();
    assert_ne!(alias_title.as_deref(), Some("SHOULD BE BLOCKED"));
}

#[cfg(unix)]
fn cross_root_alias_protection(alias_first: bool) {
    use std::io::Write;
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = dir.path().join("music-a");
    let second = dir.path().join("music-b");
    std::fs::create_dir(&music).unwrap();
    std::fs::create_dir(&second).unwrap();
    let path = music.join("a.mp3");
    tagged_copy("sine44_a.mp3", &path);
    let alias = second.join("alias.mp3");
    std::fs::hard_link(&path, &alias).unwrap();
    let lib = Library::open(&dir.path().join("library.db")).unwrap();
    let (first_root, second_root) = if alias_first {
        let b = lib.add_root(&second, UI).unwrap().value;
        (lib.add_root(&music, UI).unwrap().value, b)
    } else {
        let a = lib.add_root(&music, UI).unwrap().value;
        (a, lib.add_root(&second, UI).unwrap().value)
    };
    if let crate::ScanStart::Started(job) = lib.scan(|_| {}).unwrap() {
        job.wait().unwrap();
    }
    let tracks = lib
        .tracks_query(
            &crate::TrackQuery::default(),
            crate::Page {
                offset: 0,
                limit: 10,
            },
            &[],
        )
        .unwrap()
        .0;
    let original = tracks
        .iter()
        .find(|r| r.track.root_id == first_root)
        .unwrap()
        .track
        .clone();
    let alias_track = tracks
        .iter()
        .find(|r| r.track.root_id == second_root)
        .unwrap()
        .track
        .clone();
    let f = Fixture {
        dir,
        lib,
        id: original.id,
        path,
    };
    assert_eq!(second_root.0 < first_root.0, alias_first);
    let old_id = identity(&mpmp_fs_desktop::DesktopStorage, &f.path);
    *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
    assert!(f.edit().is_err());
    let replacement = f.dir.path().join("external-save");
    std::fs::copy(&f.path, &replacement).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&replacement)
        .unwrap()
        .write_all(b"external complete save, outside both intent digests")
        .unwrap();
    std::fs::rename(&replacement, &f.path).unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::fs::hard_link(&f.path, &alias).unwrap();
    assert_ne!(old_id, identity(&mpmp_fs_desktop::DesktopStorage, &f.path));
    assert_eq!(
        identity(&mpmp_fs_desktop::DesktopStorage, &f.path),
        identity(&mpmp_fs_desktop::DesktopStorage, &alias)
    );
    assert!(crate::tags::read::read_track(&alias, None).is_ok());
    scan(&f);
    assert_eq!(f.lib.track(f.id).unwrap().unwrap().size, original.size);
    assert_eq!(
        f.lib.track(alias_track.id).unwrap().unwrap().size,
        alias_track.size
    );
    assert_eq!(targets(&f).len(), 1);
    // Explicit acceptance unfreezes indexing, proving the alias was otherwise readable.
    let guard = f.lib.describe_cover_recovery(f.id).unwrap();
    f.lib.accept_cover_recovery(&guard, UI).unwrap();
    scan(&f);
    assert_eq!(
        f.lib.track(alias_track.id).unwrap().unwrap().size,
        std::fs::metadata(&alias).unwrap().len()
    );
    assert!(f.lib.track(alias_track.id).unwrap().unwrap().size > alias_track.size);
}

fn counted_scan_import(count: usize, with_pending: bool) -> (usize, usize, usize) {
    use crate::db::writer::{ScanBatch, WriteCmd};
    use std::sync::atomic::Ordering;
    let f = Fixture::new();
    if with_pending {
        *f.lib.inner.hooks.cover_fault.lock().unwrap() = Some("after_intent");
        assert!(f.edit().is_err());
    }
    let source = tag_job::file_refs(&f.lib.inner, &[f.id]).unwrap().remove(0);
    let template = *tag_job::reread_record(
        &f.lib.inner,
        &source,
        &f.path,
        tag_job::stamp(&std::fs::metadata(&f.path).unwrap()),
    )
    .unwrap();
    let names: Vec<_> = (0..count).map(|i| format!("import/{i}.mp3")).collect();
    let observed = Arc::new(ScanObservations::default());
    observed.record_root(
        f.path.parent().unwrap(),
        names
            .iter()
            .enumerate()
            .map(|(i, name)| (name.as_str(), Some([42, i as u64 + 100]))),
    );
    f.lib
        .inner
        .writer
        .run(|_| {
            PATH_NORMALIZATIONS.with(|n| n.set(0));
            Ok(())
        })
        .unwrap();
    for start in (0..count).step_by(500) {
        let upserts = (start..(start + 500).min(count))
            .map(|i| {
                let mut record = template.clone();
                record.rel_path = names[i].clone();
                record.file_identity = Some([42, i as u64 + 100]);
                record
            })
            .collect();
        let batch = ScanBatch {
            observed: observed.clone(),
            upserts,
            ..ScanBatch::default()
        };
        f.lib
            .inner
            .writer
            .call(|reply| WriteCmd::Scan {
                root_id: source.track.root_id,
                batch,
                reply,
            })
            .unwrap();
    }
    let paths = f
        .lib
        .inner
        .writer
        .run_data(|_| Ok(PATH_NORMALIZATIONS.with(|n| n.get())))
        .unwrap();
    assert_eq!(
        f.lib
            .track_ids(&crate::TrackQuery::default(), count + 10)
            .unwrap()
            .len(),
        count + 1
    );
    (
        observed.insertions.load(Ordering::Relaxed),
        observed.lookups.load(Ordering::Relaxed),
        paths,
    )
}

mod recovery;

mod edits;

mod undo;
