#[cfg(feature = "desktop-fs")]
pub(crate) use fixtures::*;

#[cfg(feature = "desktop-fs")]
pub(crate) mod fixtures {
    //! Database-level tests on synthetic rows (no audio files, no ffmpeg).

    use std::path::Path;
    use std::time::{Duration, Instant};

    use mpmp_test_support::TempDir;

    use crate::db::writer::{ScanBatch, TrackRecord, WriteCmd};
    use crate::tags::read::TrackMeta;
    use crate::{
        ActionMeta, AlbumKey, AlbumSort, Done, EndedReason, Library, Page, PlayEvent, RootId,
        SearchResults, TrackId, VARIOUS_ARTISTS,
    };

    /// The origin tests act as.
    pub(crate) const UI: ActionMeta<'static> = ActionMeta::new("ui");

    pub(crate) struct Fixture {
        // Declared first so the database closes before the directory is deleted (Windows).
        pub lib: Library,
        pub root: RootId,
        pub _dir: TempDir,
    }

    pub(crate) fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let music = dir.path().join("music");
        std::fs::create_dir(&music).unwrap();
        let lib = Library::open(&dir.path().join("db/library.sqlite")).unwrap();
        let root = lib.add_root(&music, UI).unwrap().value;
        Fixture {
            _dir: dir,
            lib,
            root,
        }
    }

    pub(crate) fn meta(title: &str, artist: &str, album: &str) -> TrackMeta {
        let some = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        TrackMeta {
            title: some(title),
            artist: some(artist),
            album: some(album),
            duration_ms: Some(1000),
            ..TrackMeta::default()
        }
    }

    pub(crate) fn record(root: RootId, rel_path: &str, meta: TrackMeta) -> TrackRecord {
        TrackRecord {
            root_id: root,
            rel_path: rel_path.to_owned(),
            size: 1,
            mtime_ms: 1,
            has_folder_art: false,
            meta,
            cue: None,
            based_on: None,
            based_on_revision: None,
            artists_only: false,
            file_identity: None,
        }
    }

    pub(crate) fn write(lib: &Library, root_id: RootId, mut batch: ScanBatch) {
        // Synthetic writes do not parse files; use the current root configuration.
        batch.forced_encoding = lib
            .roots()
            .unwrap()
            .into_iter()
            .find(|root| root.id == root_id)
            .and_then(|root| root.forced_encoding);
        lib.inner()
            .writer
            .call(|reply| WriteCmd::Scan {
                root_id,
                batch,
                reply,
            })
            .unwrap();
    }

    pub(crate) fn upsert(lib: &Library, records: Vec<TrackRecord>) {
        let Some(root) = records.first().map(|r| r.root_id) else {
            return;
        };
        write(
            lib,
            root,
            ScanBatch {
                upserts: records,
                ..ScanBatch::default()
            },
        );
    }

    pub(crate) fn titles(results: &SearchResults) -> Vec<&str> {
        results.tracks.iter().map(|t| t.display_title()).collect()
    }

    pub(crate) fn id_of(lib: &Library, title: &str) -> TrackId {
        let results = lib.search(title, 10).unwrap();
        results
            .tracks
            .iter()
            .find(|t| t.display_title() == title)
            .unwrap_or_else(|| panic!("no track titled {title}"))
            .id
    }

    fn sample(f: &Fixture) {
        let r = f.root;
        let mut compilation_a = meta("Song A", "Artist One", "Hits");
        compilation_a.compilation = true;
        let mut compilation_b = meta("Song B", "Artist Two", "Hits");
        compilation_b.compilation = true;
        let mut disc2 = meta("Second Disc Opener", "周砚舟", "叶窗集");
        disc2.disc_no = Some(2);
        disc2.track_no = Some(1);
        let mut disc1 = meta("晶港", "周砚舟", "叶窗集");
        disc1.disc_no = Some(1);
        disc1.track_no = Some(3);
        let mut disc1b = meta("云盒之声", "周砚舟", "叶窗集");
        disc1b.disc_no = Some(1);
        disc1b.track_no = Some(1);
        disc1b.has_embedded_art = true;
        upsert(
            &f.lib,
            vec![
                record(r, "jay/2.flac", disc2),
                record(r, "jay/1.flac", disc1),
                record(r, "jay/0.flac", disc1b),
                record(
                    r,
                    "full/abc.mp3",
                    meta("ＡＢＣ　Ｓｏｎｇ", "ＤＪ Ｗｉｄｅ", ""),
                ),
                record(r, "va/a.ogg", compilation_a),
                record(r, "va/b.ogg", compilation_b),
                record(r, "jp/x.m4a", meta("静かな雲箱のアーチ", "高雲帆子", "")),
                record(r, "untagged/Lonely Noise.wav", TrackMeta::default()),
                record(r, "odd/q.mp3", meta(r#"Say "Hi" 100% _done_"#, "Quote", "")),
            ],
        );
    }

    // ---- scale ---

    /// Deterministic pseudo-random numbers (64-bit LCG).
    pub(crate) struct Lcg(pub u64);

    impl Lcg {
        pub fn next(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % n
        }

        pub fn words(&mut self, pool: &[&str], count: usize, sep: &str) -> String {
            (0..count)
                .map(|_| pool[self.next(pool.len())])
                .collect::<Vec<_>>()
                .join(sep)
        }
    }

    pub(crate) const LATIN: &[&str] = &[
        "love", "night", "dream", "fire", "blue", "heart", "rain", "summer", "light", "road",
        "river", "star", "dance", "shadow", "golden", "wild", "moon", "silver", "ocean", "city",
        "echo", "storm", "glass", "paper", "velvet", "winter", "ghost", "neon", "sugar", "thunder",
    ];

    pub(crate) const CJK: &[&str] = &[
        "爱", "夜", "梦", "火", "蓝", "心", "雨", "夏", "光", "路", "河", "星", "舞", "影", "金",
        "月", "海", "城", "风", "花", "雪", "春", "秋", "天", "晶", "歌", "恋", "桜", "空", "時",
    ];

    pub(crate) fn synthetic(
        rng: &mut Lcg,
        root: RootId,
        i: usize,
        artists: &[String],
    ) -> TrackRecord {
        let artist = &artists[(i / 10) % artists.len()];
        let album_no = i / 10;
        let cjk = i.is_multiple_of(3);
        let title = if cjk {
            let n = 2 + rng.next(4);
            rng.words(CJK, n, "")
        } else {
            let n = 1 + rng.next(3);
            rng.words(LATIN, n, " ")
        };
        let mut m = meta(&title, artist, &format!("Album {album_no}"));
        m.track_no = Some((i % 10) as u32 + 1);
        m.genre = Some(["Pop", "Rock", "Jazz", "流行"][i % 4].to_owned());
        record(root, &format!("synthetic/{album_no}/{i}.flac"), m)
    }

    /// Inserts `rows` synthetic tracks through the writer, runs a fixed set of searches and
    /// returns the worst best-of-three search latency.
    fn search_latency(rows: usize) -> Duration {
        let f = fixture();
        let lib = &f.lib;
        let mut rng = Lcg(42);
        let artists: Vec<String> = (0..rows.div_ceil(50))
            .map(|i| {
                if i.is_multiple_of(2) {
                    let n = 2 + rng.next(2);
                    rng.words(CJK, n, "")
                } else {
                    format!("{} {}", rng.words(LATIN, 1, ""), rng.words(LATIN, 1, ""))
                }
            })
            .collect();

        let start = Instant::now();
        let mut batch = Vec::with_capacity(500);
        for i in 0..rows {
            batch.push(synthetic(&mut rng, f.root, i, &artists));
            if batch.len() == 500 {
                upsert(lib, std::mem::take(&mut batch));
            }
        }
        upsert(lib, batch);
        eprintln!("inserted {rows} rows in {:.2?}", start.elapsed());

        // Broad and narrow, long (trigram) and short (substring scan), mixed, and no-hit queries.
        let queries = [
            "love",
            "dream night",
            "梦",
            "雨夜",
            "星月海",
            "love 夜",
            "zzzz",
            "album 99",
            "pop",
            "sil",
            "s",
        ];
        let mut worst = Duration::ZERO;
        for query in queries {
            // Warm once, then take the best of three (the box is shared).
            lib.search(query, 50).unwrap();
            let mut best = Duration::MAX;
            let mut counts = (0, 0, 0);
            for _ in 0..3 {
                let t = Instant::now();
                let results = lib.search(query, 50).unwrap();
                best = best.min(t.elapsed());
                counts = (
                    results.tracks.len(),
                    results.albums.len(),
                    results.artists.len(),
                );
            }
            eprintln!("search {query:?}: {best:.2?} (tracks/albums/artists {counts:?})");
            let rare = matches!(query, "zzzz" | "星月海");
            assert!(rare || counts.0 > 0, "no tracks for {query:?}");
            worst = worst.max(best);
        }
        let t = Instant::now();
        let albums = lib.albums(AlbumSort::Title, Page::new(0, 100)).unwrap();
        assert_eq!(albums.items.len(), 100.min(rows / 10));
        eprintln!("albums page: {:.2?}", t.elapsed());
        eprintln!("worst search: {worst:.2?}");
        worst
    }

    fn coalesced_scope_scenario(full: bool) {
        use std::sync::{Arc, Mutex};
        let f = fixture();
        let first = f.lib.roots().unwrap()[0].path.clone();
        let audio =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_a.flac");
        std::fs::copy(&audio, first.join("a.flac")).unwrap();
        let mut ids = vec![f.root];
        for name in ["second", "unrelated"] {
            let path = f._dir.path().join(name);
            std::fs::create_dir(&path).unwrap();
            std::fs::copy(&audio, path.join("a.flac")).unwrap();
            ids.push(f.lib.add_root(&path, UI).unwrap().value);
        }
        let visits = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&visits);
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (resume_tx, resume) = std::sync::mpsc::channel();
        let resume = Mutex::new(resume);
        *f.lib.inner().hooks.scan_file.lock().unwrap() = Some(Arc::new(move |root, _| {
            let initial = {
                let mut seen = seen.lock().unwrap();
                let initial = seen.is_empty();
                seen.push(root);
                initial
            };
            if initial {
                entered_tx.send(()).unwrap();
                resume
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
        }));
        let crate::ScanStart::Started(scan) = f
            .lib
            .scan_roots(if full { None } else { Some(vec![ids[0]]) }, |_| {})
            .unwrap()
        else {
            panic!()
        };
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            f.lib.scan_roots(Some(vec![ids[1]]), |_| {}).unwrap(),
            crate::ScanStart::Coalesced(_)
        ));
        resume_tx.send(()).unwrap();
        let report = scan.wait().unwrap();
        assert_eq!(report.passes, 2);
        assert_eq!(report.scanned, if full { 3 } else { 2 });
        let seen = visits.lock().unwrap();
        assert!(seen.contains(&ids[0]) && seen.contains(&ids[1]));
        assert_eq!(seen.iter().filter(|&&id| id == ids[0]).count(), 1);
        assert_eq!(
            seen.iter().filter(|&&id| id == ids[1]).count(),
            if full { 2 } else { 1 }
        );
        assert_eq!(
            seen.iter().filter(|&&id| id == ids[2]).count(),
            if full { 1 } else { 0 }
        );
    }

    mod catalog;
}

#[cfg(feature = "desktop-fs")]
mod cue;

#[cfg(feature = "desktop-fs")]
mod journal;

#[cfg(feature = "desktop-fs")]
mod lyrics;

#[cfg(feature = "desktop-fs")]
mod playlist;

#[cfg(feature = "desktop-fs")]
mod query;

#[cfg(feature = "desktop-fs")]
mod regressions;

#[cfg(feature = "desktop-fs")]
pub(crate) mod storage;

#[cfg(feature = "desktop-fs")]
mod person;

#[cfg(feature = "desktop-fs")]
pub(crate) mod storage_adapter;

mod storage_boundary;
