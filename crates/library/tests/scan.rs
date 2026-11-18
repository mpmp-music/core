//! Scans a generated music library (scripts/gen-library-fixtures.py) and checks tags,
//! encodings, grouping, search and incremental rescans.
//!
//! Needs ffmpeg (`$FFMPEG`, `target/tools/ffmpeg/bin/ffmpeg` or `ffmpeg` on PATH) and
//! Python 3. Without them the tests print a notice and pass, so CI without ffmpeg stays green.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crossbeam_channel::bounded;
use mpmp_library::{
    ActionMeta, AlbumKey, AlbumSort, Library, Page, ScanProgress, ScanReport, ScanStart, Track,
    VARIOUS_ARTISTS,
};
use tempfile::TempDir;

const UI: ActionMeta<'static> = ActionMeta::new("ui");

/// Audio files the script writes, including the undecodable `Broken/garbage.flac`.
const AUDIO_FILES: u64 = 28;

const READABLE_FILES: u64 = AUDIO_FILES - 1;

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn runs(program: &Path, arg: &str) -> bool {
    Command::new(program)
        .arg(arg)
        .output()
        .is_ok_and(|out| out.status.success())
}

/// The ffmpeg to use and whether it was configured explicitly (`$FFMPEG` or the workspace
/// copy) rather than merely found on PATH.
fn find_ffmpeg() -> Option<(PathBuf, bool)> {
    let candidates = [
        std::env::var_os("FFMPEG").map(|p| (PathBuf::from(p), true)),
        Some((workspace().join("target/tools/ffmpeg/bin/ffmpeg"), true)),
        Some((PathBuf::from("ffmpeg"), false)),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|(path, _)| runs(path, "-version"))
}

fn find_python() -> Option<PathBuf> {
    ["python3", "python"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| runs(path, "--version"))
}

/// FNV-1a, to key the fixture cache on the generator script's contents.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

/// Generates the fixture library once per script version into the cargo target directory.
fn generated() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let script = workspace().join("scripts/gen-library-fixtures.py");
        let hash = fnv(&fs::read(&script).expect("read generator script"));
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("library-fixtures-{hash:x}"));
        if dir.join(".complete").exists() {
            return Some(dir);
        }
        let (Some((ffmpeg, explicit)), Some(python)) = (find_ffmpeg(), find_python()) else {
            eprintln!("SKIPPED: ffmpeg or python3 not found; set FFMPEG to run library scan tests");
            return None;
        };
        let staging = dir.with_extension(format!("tmp{}", std::process::id()));
        let _ = fs::remove_dir_all(&staging);
        let status = Command::new(python)
            .arg(&script)
            .arg(&staging)
            .env("FFMPEG", &ffmpeg)
            .status()
            .expect("run generator script");
        if !status.success() {
            // A system ffmpeg may lack an encoder (libmp3lame, libvorbis); only a configured
            // one must work.
            assert!(
                !explicit,
                "gen-library-fixtures.py failed with {}",
                ffmpeg.display()
            );
            eprintln!("SKIPPED: gen-library-fixtures.py failed with the ffmpeg on PATH");
            let _ = fs::remove_dir_all(&staging);
            return None;
        }
        fs::write(staging.join(".complete"), b"").unwrap();
        if fs::rename(&staging, &dir).is_err() {
            // Another test process won the race; use its copy.
            let _ = fs::remove_dir_all(&staging);
        }
        Some(dir)
    })
    .as_deref()
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else if entry.file_name() != ".complete" {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// A private copy of the fixture library plus a fresh database.
struct Setup {
    // Declared first so the database closes before the directory is deleted (Windows).
    lib: Library,
    dir: TempDir,
}

impl Setup {
    fn new() -> Option<Self> {
        let source = generated()?;
        let dir = TempDir::new().unwrap();
        copy_tree(source, &dir.path().join("music"));
        let lib = Library::open(&dir.path().join("library.sqlite")).unwrap();
        lib.add_root(&dir.path().join("music"), UI).unwrap();
        Some(Self { lib, dir })
    }

    fn music(&self) -> PathBuf {
        self.dir.path().join("music")
    }

    fn scan(&self) -> ScanReport {
        started(self.lib.scan(|_| {})).wait().unwrap()
    }

    fn by_path(&self, rel_path: &str) -> Track {
        let stem = Path::new(rel_path).file_stem().unwrap().to_str().unwrap();
        let results = self.lib.search(stem, 100).unwrap();
        // Untagged files are found by file name; others by a title search below.
        if let Some(track) = results.tracks.into_iter().find(|t| t.rel_path == rel_path) {
            return track;
        }
        let albums = self
            .lib
            .albums(AlbumSort::Title, Page::new(0, 1000))
            .unwrap()
            .items;
        for album in albums {
            let album = self.lib.album(&album.key).unwrap().unwrap();
            if let Some(track) = album.tracks.into_iter().find(|t| t.rel_path == rel_path) {
                return track;
            }
        }
        panic!("no track at {rel_path}");
    }
}

fn album_count(lib: &Library) -> u64 {
    lib.albums(AlbumSort::Title, Page::new(0, 0)).unwrap().total
}

fn started(start: mpmp_library::Result<ScanStart>) -> mpmp_library::ScanHandle {
    match start.unwrap() {
        ScanStart::Started(handle) => handle,
        ScanStart::Coalesced(_) => panic!("expected a new scan, got a coalesced request"),
    }
}

fn titles(lib: &Library, query: &str) -> Vec<String> {
    let mut titles: Vec<String> = lib
        .search(query, 100)
        .unwrap()
        .tracks
        .iter()
        .map(|t| t.display_title().to_owned())
        .collect();
    titles.sort();
    titles
}

#[test]
fn first_scan_reads_formats_tags_and_encodings() {
    let Some(s) = Setup::new() else { return };
    let reports = Arc::new(Mutex::new(Vec::new()));
    let seen = reports.clone();
    let handle = started(s.lib.scan(move |p| seen.lock().unwrap().push(p)));
    let id = handle.id();
    let report = handle.wait().unwrap();
    let broken = report.first_errors[0].clone();
    assert_eq!(broken.code, mpmp_library::ScanErrorCode::AudioReadFailed);
    assert!(broken.path.ends_with(".flac"), "{broken:?}");
    assert!(
        broken.message.contains("cannot read audio file"),
        "{broken:?}"
    );
    assert_eq!(
        report,
        ScanReport {
            diagnostics: report.diagnostics.clone(),
            scan: id,
            scanned: AUDIO_FILES,
            added: READABLE_FILES,
            updated: 0,
            restored: 0,
            removed: 0,
            errors: 1,
            first_errors: vec![broken],
            passes: 1,
            coalesced: 0,
            cancelled: false,
        }
    );
    let progress = ScanProgress {
        scanned: AUDIO_FILES,
        total_estimate: AUDIO_FILES,
        added: READABLE_FILES,
        updated: 0,
        restored: 0,
        removed: 0,
        errors: 1,
    };
    let reports = reports.lock().unwrap();
    assert_eq!(reports.last(), Some(&progress));
    assert!(
        reports
            .iter()
            .any(|p| p.total_estimate == AUDIO_FILES && p.scanned == 0)
    );

    // FLAC: properties, numbering, ReplayGain, cover.
    let first = s.by_path("Artist A/Album One (2001)/01 First Light.flac");
    assert_eq!(first.title.as_deref(), Some("First Light"));
    assert_eq!(first.codec.as_deref(), Some("flac"));
    assert_eq!(first.lossless, Some(true));
    assert_eq!(
        (first.sample_rate, first.channels, first.bits_per_sample),
        (Some(44100), Some(2), Some(16))
    );
    assert!(first.duration_ms.is_some_and(|d| d.abs_diff(300) <= 30));
    assert_eq!((first.track_no, first.track_total), (Some(1), Some(3)));
    assert_eq!(first.year, Some(2001));
    assert_eq!(first.genre.as_deref(), Some("Rock"));
    assert_eq!(first.replay_gain.track_gain_db, Some(-6.5));
    assert_eq!(first.replay_gain.track_peak, Some(0.988));
    assert!(first.has_embedded_art);
    assert!(first.available);
    let uri = s.lib.uri_of(first.id).unwrap().unwrap();
    assert!(uri.to_file_path().unwrap().is_file());

    let album = s.lib.album(&first.album_key).unwrap().unwrap();
    assert_eq!(album.summary.track_count, 3);
    assert_eq!(album.summary.art_track, Some(first.id));
    assert_eq!(album.summary.year, Some(2001));

    // MP3, two discs, in disc/track order.
    let key = AlbumKey::new("Artist B", "Double Album");
    let album = s.lib.album(&key).unwrap().unwrap();
    assert_eq!(album.summary.disc_count, 2);
    let order: Vec<&str> = album.tracks.iter().map(|t| t.rel_path.as_str()).collect();
    assert_eq!(
        order,
        [
            "Artist B/Double Album/CD1/01.mp3",
            "Artist B/Double Album/CD1/02.mp3",
            "Artist B/Double Album/CD2/01.mp3",
            "Artist B/Double Album/CD2/02.mp3",
        ]
    );
    assert_eq!(album.tracks[2].codec.as_deref(), Some("mp3"));
    assert_eq!(album.tracks[2].lossless, Some(false));

    // Compilations: explicit album artist (Ogg) and flag only (M4A) both make one album.
    for (album, codec) in [("Now Hits", "vorbis"), ("Summer Mix", "aac")] {
        let album = s
            .lib
            .album(&AlbumKey::new(VARIOUS_ARTISTS, album))
            .unwrap()
            .unwrap_or_else(|| panic!("{album} not grouped"));
        assert_eq!(album.summary.track_count, 3);
        assert_eq!(album.summary.artist.as_deref(), Some(VARIOUS_ARTISTS));
        assert!(album.tracks.iter().all(|t| t.compilation));
        assert_eq!(album.tracks[0].codec.as_deref(), Some(codec));
    }

    // Untagged files: no tags, found by file name, multi-level relative path.
    let noise = s.by_path("Untagged/deep/nested/folder/noise.wav");
    assert_eq!(noise.title, None);
    assert_eq!(noise.display_title(), "noise");
    assert_eq!(noise.codec.as_deref(), Some("pcm"));
    assert_eq!(titles(&s.lib, "no tags"), ["no tags"]);

    // RIFF INFO tags.
    assert_eq!(titles(&s.lib, "wave title"), ["Wave Title"]);

    // Unicode tags, trigram and short-query search, full-width folding.
    assert_eq!(
        titles(&s.lib, "周砚舟"),
        ["三棱雾", "云盒之声", "晶港", "晶港的故事"]
    );
    assert_eq!(titles(&s.lib, "晶港"), ["晶港", "晶港的故事"]);
    assert_eq!(titles(&s.lib, "abc song"), ["ＡＢＣ　Ｓｏｎｇ"]);
    assert_eq!(titles(&s.lib, "雲箱"), ["静かな雲箱のアーチ"]);
    let results = s.lib.search("叶窗集", 10).unwrap();
    assert_eq!(results.albums.len(), 1);
    assert_eq!(results.albums[0].track_count, 3);
    let results = s.lib.search("周砚舟", 10).unwrap();
    assert_eq!(results.artists.len(), 1);
    assert_eq!(results.artists[0].track_count, 4);

    // Legacy encodings repaired; real Latin-1 left alone.
    let cases = [
        (
            "Legacy/GBK v2.3/sanlengwu.mp3",
            "三棱雾",
            "周砚舟",
            Some("GBK"),
        ),
        (
            "Legacy/Big5 v2.3/xingqiao.mp3",
            "星橋漫步",
            "星舟團",
            Some("Big5"),
        ),
        (
            "Legacy/Shift_JIS v1/hoshibako.mp3",
            "星箱桜",
            "星野ノノ",
            Some("Shift_JIS"),
        ),
        (
            "Legacy/GBK v1/jingwan.mp3",
            "晶港的故事",
            "周砚舟",
            Some("GBK"),
        ),
        (
            "Legacy/Latin-1 v2.3/cafe.mp3",
            "Café des Nues",
            "Vélinéa",
            None,
        ),
        (
            "Legacy/Latin-1 v2.3/agaevis.mp3",
            "Ágævis nyrven",
            "Sivar Nól",
            None,
        ),
        // Unicode frames are never re-decoded, whatever they look like.
        ("Legacy/UTF-16 v2.3/unicode.mp3", "ÈýÀâÎí", "ÖÜÑâÖÛ", None),
    ];
    for (path, title, artist, encoding) in cases {
        let results = s.lib.search(title, 10).unwrap();
        let track = results
            .tracks
            .iter()
            .find(|t| t.rel_path == path)
            .unwrap_or_else(|| panic!("{path}: no track titled {title}"));
        assert_eq!(track.artist.as_deref(), Some(artist), "{path}");
        assert_eq!(track.tag_encoding.as_deref(), encoding, "{path}");
    }
    let sjis = s.by_path("Legacy/Shift_JIS v1/hoshibako.mp3");
    assert_eq!(sjis.album.as_deref(), Some("ホシバコ曲集"));
    assert_eq!((sjis.year, sjis.track_no), (Some(2011), Some(4)));

    let artists = s.lib.artists(Page::new(0, 100)).unwrap();
    assert!(artists.items.iter().any(|a| a.name == "星舟團"));
    assert_eq!(artists.total, artists.items.len() as u64);
    let albums = s.lib.albums(AlbumSort::Title, Page::new(0, 1000)).unwrap();
    assert_eq!(albums.total, albums.items.len() as u64);
    let jay = s
        .lib
        .albums_by_artist("周砚舟", AlbumSort::Title, Page::new(0, 1))
        .unwrap();
    assert_eq!((jay.items.len(), jay.total), (1, 2));

    // Untagged files form the (unknown artist, unknown album) group.
    let unknown = s.lib.album(&AlbumKey::new("", "")).unwrap().unwrap();
    assert_eq!(unknown.summary.title, "");
    assert_eq!(unknown.tracks.len(), 2);
    let albums = s.lib.albums(AlbumSort::Year, Page::new(0, 100)).unwrap();
    assert_eq!(albums.items[0].title, "Double Album");
}

#[test]
fn rescan_is_incremental() {
    let Some(s) = Setup::new() else { return };
    s.scan();
    let second = s.scan();
    assert_eq!(
        (second.added, second.updated, second.removed),
        (0, 0, 0),
        "{second:?}"
    );
    assert_eq!(second.scanned, AUDIO_FILES);
    // The broken file is retried (it has no row) and fails again.
    assert_eq!(second.errors, 1);

    let changed_path = "CJK/周砚舟/叶窗集/02 晶港.flac";
    let gone_path = "Artist A/Album One (2001)/02 Second Wind.flac";
    let changed = s.by_path(changed_path);
    let gone = s.by_path(gone_path);

    // Grow one file (new size) and remove another; keep the removed bytes and mtime.
    let file = s.music().join(changed_path);
    let mut bytes = fs::read(&file).unwrap();
    bytes.extend_from_slice(&[0; 64]);
    fs::write(&file, &bytes).unwrap();
    let gone_file = s.music().join(gone_path);
    let gone_bytes = fs::read(&gone_file).unwrap();
    let gone_mtime = fs::metadata(&gone_file).unwrap().modified().unwrap();
    fs::remove_file(&gone_file).unwrap();

    let third = s.scan();
    assert_eq!(
        (third.added, third.updated, third.removed),
        (0, 1, 1),
        "{third:?}"
    );
    assert_eq!(
        s.lib.track(changed.id).unwrap().unwrap().size,
        changed.size + 64
    );
    let gone_now = s.lib.track(gone.id).unwrap().unwrap();
    assert!(!gone_now.available);
    assert!(titles(&s.lib, "second wind").is_empty());
    let album = s.lib.album(&gone.album_key).unwrap().unwrap();
    assert_eq!(album.summary.track_count, 2);

    // Restored unchanged: becomes available again under the same id without a re-read.
    fs::write(&gone_file, &gone_bytes).unwrap();
    fs::File::options()
        .write(true)
        .open(&gone_file)
        .unwrap()
        .set_modified(gone_mtime)
        .unwrap();
    let fourth = s.scan();
    assert_eq!(
        (
            fourth.added,
            fourth.updated,
            fourth.restored,
            fourth.removed
        ),
        (0, 0, 1, 0),
        "{fourth:?}"
    );
    let back = s.lib.track(gone.id).unwrap().unwrap();
    assert!(back.available);
    assert_eq!(back.updated_at, gone.updated_at);
    assert_eq!(titles(&s.lib, "second wind"), ["Second Wind"]);

    // A new file is added with a fresh id.
    fs::copy(&gone_file, s.music().join("Artist A/copy.flac")).unwrap();
    let fifth = s.scan();
    assert_eq!((fifth.added, fifth.updated, fifth.removed), (1, 0, 0));
    assert_eq!(titles(&s.lib, "second wind").len(), 2);
}

#[test]
fn unreachable_root_keeps_its_tracks() {
    let Some(s) = Setup::new() else { return };
    s.scan();
    let track = s.by_path("Untagged/no tags.flac");
    let albums = album_count(&s.lib);

    // The folder is gone (unplugged drive): skipped with an error, nothing changes.
    let moved = s.dir.path().join("elsewhere");
    fs::rename(s.music(), &moved).unwrap();
    let progress = s.scan();
    assert_eq!((progress.removed, progress.errors), (0, 1), "{progress:?}");
    assert!(s.lib.track(track.id).unwrap().unwrap().available);
    assert_eq!(album_count(&s.lib), albums);

    // An empty mount point is treated the same way.
    fs::create_dir(s.music()).unwrap();
    let progress = s.scan();
    assert_eq!((progress.removed, progress.errors), (0, 1), "{progress:?}");
    assert!(s.lib.track(track.id).unwrap().unwrap().available);

    // Back: nothing to do.
    fs::remove_dir(s.music()).unwrap();
    fs::rename(&moved, s.music()).unwrap();
    let progress = s.scan();
    assert_eq!(
        (progress.added, progress.updated, progress.removed),
        (0, 0, 0)
    );
    assert!(s.lib.roots().unwrap()[0].last_scan_at.is_some());
}

#[test]
fn forced_encoding_rereads_the_root() {
    let Some(s) = Setup::new() else { return };
    s.scan();
    let root = s.lib.roots().unwrap()[0].id;
    s.lib.set_root_encoding(root, Some("big5")).unwrap();
    let progress = s.scan();
    assert_eq!(progress.updated, READABLE_FILES);
    let results = s.lib.search("星橋漫步", 10).unwrap();
    assert_eq!(results.tracks[0].tag_encoding.as_deref(), Some("Big5"));
    // Real Latin-1 text is now (wrongly, as the user asked) decoded as Big5 where it can be;
    // ASCII and Unicode tags are unaffected.
    assert_eq!(titles(&s.lib, "first light"), ["First Light"]);
    assert_eq!(titles(&s.lib, "云盒之声"), ["云盒之声"]);

    s.lib.set_root_encoding(root, None).unwrap();
    assert_eq!(s.scan().updated, READABLE_FILES);
    assert_eq!(titles(&s.lib, "café"), ["Café des Nues"]);
}

#[test]
fn one_scan_at_a_time_and_cancel() {
    let Some(s) = Setup::new() else { return };

    // Hold the first scan inside its first progress report (sent right after the walk).
    let (release_tx, release_rx) = bounded::<()>(0);
    let (entered_tx, entered_rx) = bounded::<()>(1);
    let first = started(s.lib.scan(move |_| {
        if entered_tx.try_send(()).is_ok() {
            let _ = release_rx.recv();
        }
    }));
    entered_rx.recv_timeout(Duration::from_secs(20)).unwrap();
    let id = first.id();
    assert!(matches!(s.lib.scan(|_| {}), Ok(ScanStart::Coalesced(joined)) if joined == id));
    // Cancelling also drops the coalesced pass.
    first.cancel();
    release_tx.send(()).unwrap();
    let report = first.wait().unwrap();
    assert!(report.cancelled, "{report:?}");
    assert_eq!((report.scan, report.coalesced, report.passes), (id, 1, 1));

    // Nothing was marked unavailable by the cancelled scan, and a new scan can start.
    let progress = s.scan();
    assert_eq!(progress.removed, 0);
    assert_eq!(progress.added + progress.updated, READABLE_FILES);
    assert!(s.lib.roots().unwrap()[0].last_scan_at.is_some());
}

#[test]
fn cover_art_embedded_then_folder() {
    let Some(s) = Setup::new() else { return };
    s.scan();
    let png = |c: &mpmp_library::CoverArt| c.mime == "image/png" && c.data.starts_with(b"\x89PNG");

    // Embedded front cover wins over the folder's cover.jpg.
    let first = s.by_path("Artist A/Album One (2001)/01 First Light.flac");
    assert!(png(&s.lib.cover_art(first.id).unwrap().unwrap()));
    // Without embedded art, the folder's cover.jpg.
    let second = s.by_path("Artist A/Album One (2001)/02 Second Wind.flac");
    assert!(!second.has_embedded_art && second.has_folder_art);
    let cover = s.lib.cover_art(second.id).unwrap().unwrap();
    assert_eq!(cover.mime, "image/jpeg");
    assert_eq!(cover.data, b"\xff\xd8\xff\xd9");

    // An album with only a folder cover (Cover.PNG) still gets an art track.
    let hits = s
        .lib
        .album(&AlbumKey::new(VARIOUS_ARTISTS, "Now Hits"))
        .unwrap()
        .unwrap();
    let art = hits.summary.art_track.expect("art track from folder cover");
    assert!(png(&s.lib.cover_art(art).unwrap().unwrap()));

    // No art anywhere.
    let double = s
        .lib
        .album(&AlbumKey::new("Artist B", "Double Album"))
        .unwrap()
        .unwrap();
    assert_eq!(double.summary.art_track, None);
    assert_eq!(s.lib.cover_art(double.tracks[0].id).unwrap(), None);

    // A cover added later is picked up by a rescan without re-reading the audio files.
    let summer_key = AlbumKey::new(VARIOUS_ARTISTS, "Summer Mix");
    assert_eq!(
        s.lib.album(&summer_key).unwrap().unwrap().summary.art_track,
        None
    );
    fs::write(
        s.music().join("Compilations/Summer Mix/front.jpeg"),
        b"\xff\xd8\xff\xd9",
    )
    .unwrap();
    let progress = s.scan();
    assert_eq!((progress.added, progress.updated), (0, 0));
    let art = s
        .lib
        .album(&summer_key)
        .unwrap()
        .unwrap()
        .summary
        .art_track
        .unwrap();
    assert_eq!(s.lib.cover_art(art).unwrap().unwrap().mime, "image/jpeg");
    assert_eq!(
        s.lib.cover_art(mpmp_library::TrackId(i64::MAX)).unwrap(),
        None
    );
}

#[test]
fn remove_and_re_add_root_keeps_track_ids() {
    let Some(s) = Setup::new() else { return };
    s.scan();
    let root = s.lib.roots().unwrap()[0].id;
    assert_eq!(s.lib.roots().unwrap()[0].track_count, READABLE_FILES);
    let track = s.by_path("CJK/周砚舟/叶窗集/02 晶港.flac");
    s.lib
        .record_play(mpmp_library::PlayEvent {
            track_id: track.id,
            context: None,
            started_at: 1,
            played_ms: 300,
            ended_reason: mpmp_library::EndedReason::Completed,
            origin: "ui".into(),
        })
        .unwrap();

    s.lib.remove_root(root, UI).unwrap();
    assert!(s.lib.roots().unwrap().is_empty());
    assert_eq!(album_count(&s.lib), 0);
    assert!(!s.lib.track(track.id).unwrap().unwrap().available);
    // A scan with no active roots changes nothing.
    assert_eq!(s.scan().scanned, 0);

    assert_eq!(s.lib.add_root(&s.music(), UI).unwrap().value, root);
    let progress = s.scan();
    // The re-added folder's files are back unchanged: restored, not added or updated.
    assert_eq!(
        (progress.added, progress.updated, progress.restored),
        (0, 0, READABLE_FILES)
    );
    let back = s.by_path("CJK/周砚舟/叶窗集/02 晶港.flac");
    assert_eq!(back.id, track.id);
    assert!(back.available);
    assert_eq!(s.lib.roots().unwrap()[0].track_count, READABLE_FILES);
    let history = s.lib.history(Page::new(0, 10)).unwrap();
    assert_eq!(history.total, 1);
    assert_eq!(history.items[0].event.track_id, back.id);
}

#[test]
fn root_added_during_a_scan_is_scanned_before_it_finishes() {
    let Some(s) = Setup::new() else { return };
    let more = s.dir.path().join("more");
    fs::create_dir(&more).unwrap();
    fs::copy(
        s.music().join("Tagged WAV/wav song.wav"),
        more.join("late arrival.wav"),
    )
    .unwrap();

    // Hold the scan right after its first walk, add a root, request a scan.
    let (release_tx, release_rx) = bounded::<()>(0);
    let (entered_tx, entered_rx) = bounded::<()>(1);
    let first = started(s.lib.scan(move |_| {
        if entered_tx.try_send(()).is_ok() {
            let _ = release_rx.recv();
        }
    }));
    entered_rx.recv_timeout(Duration::from_secs(20)).unwrap();
    let more_root = s.lib.add_root(&more, UI).unwrap().value;
    assert!(matches!(s.lib.scan(|_| {}), Ok(ScanStart::Coalesced(id)) if id == first.id()));
    assert!(matches!(s.lib.scan(|_| {}), Ok(ScanStart::Coalesced(id)) if id == first.id()));
    release_tx.send(()).unwrap();
    // Later reports must not block.
    drop(release_tx);

    let progress = first.wait().unwrap();
    assert_eq!(
        progress.scanned,
        AUDIO_FILES + 1,
        "distinct paths across passes"
    );
    // The first pass adds the original files, the second only the
    // new root's file. The broken file fails in both passes but counts once.
    assert_eq!(progress.added, READABLE_FILES + 1, "{progress:?}");
    assert_eq!(progress.errors, 1, "{progress:?}");
    assert_eq!(progress.first_errors.len(), 1, "{progress:?}");
    // Two requests joined during the first pass: one more pass.
    assert_eq!(
        (progress.passes, progress.coalesced),
        (2, 2),
        "{progress:?}"
    );
    assert!(!progress.cancelled);
    let roots = s.lib.roots().unwrap();
    let added = roots.iter().find(|r| r.id == more_root).unwrap();
    assert_eq!(added.track_count, 1);
    assert!(added.last_scan_at.is_some());
    // The next request starts a new scan.
    assert_eq!(s.scan().added, 0);
}

#[test]
fn all_tags_are_stored_and_queryable() {
    use mpmp_library::{Field, Filter, TextOp, TrackQuery};
    let Some(s) = Setup::new() else { return };
    s.scan();
    let by_tag = |key: &str, text: &str| {
        let query = TrackQuery {
            filter: Filter::Text {
                field: Field::Tag { key: key.into() },
                op: TextOp::Contains,
                text: text.into(),
            },
            sort: vec![],
        };
        let keys = vec![key.to_owned(), "TITLE".to_owned()];
        s.lib
            .tracks_query(&query, Page::new(0, 10), &keys)
            .unwrap()
            .0
    };
    // Vorbis comments: a mapped key and an unknown one.
    let rows = by_tag("composer", "savelle");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tags["COMPOSER"], ["Eran Savelle"]);
    assert_eq!(rows[0].tags["TITLE"], ["First Light"]);
    assert_eq!(by_tag("my_custom", "custom value").len(), 1);
    // ID3v2: TCOM and TXXX.
    let rows = by_tag("COMPOSER", "marnovel");
    assert_eq!(rows[0].track.rel_path, "Artist B/Double Album/CD1/01.mp3");
    assert_eq!(by_tag("catalog_custom", "xyz-1").len(), 1);
    // Repaired legacy text in the tag table too.
    let rows = by_tag("ARTIST", "星舟團");
    assert_eq!(rows[0].track.rel_path, "Legacy/Big5 v2.3/xingqiao.mp3");
    // MP4 and Ogg tags.
    assert_eq!(by_tag("ALBUM", "summer mix").len(), 3);
    assert_eq!(by_tag("ALBUMARTIST", "various").len(), 3);

    let (values, total) = s
        .lib
        .facet(
            &Field::Tag {
                key: "COMPOSER".into(),
            },
            &Filter::All,
            Page::new(0, 10),
        )
        .unwrap();
    assert_eq!(total, 3);
    assert_eq!(values[2].value, None);
    assert_eq!(values[2].tracks, READABLE_FILES - 2);
}

#[test]
fn ratings_survive_real_rescans_and_root_re_adding() {
    use mpmp_library::{CompareOp, Field, Filter, TrackQuery, Value};
    let Some(s) = Setup::new() else { return };
    s.scan();
    let path = "CJK/周砚舟/叶窗集/02 晶港.flac";
    let track = s.by_path(path);
    s.lib.set_rating(&[track.id], Some(4), UI).unwrap();
    s.lib.set_loved(&[track.id], true, UI).unwrap();
    let rated = || {
        let query = TrackQuery {
            filter: Filter::And {
                all: vec![
                    Filter::Compare {
                        field: Field::Rating,
                        op: CompareOp::Eq,
                        value: Value::Int(4),
                    },
                    Filter::Compare {
                        field: Field::Loved,
                        op: CompareOp::Eq,
                        value: Value::Bool(true),
                    },
                ],
            },
            sort: vec![],
        };
        s.lib.track_ids(&query, 10).unwrap()
    };
    assert_eq!(rated(), [track.id]);

    // The file changes and is re-read.
    let file = s.music().join(path);
    let mut bytes = fs::read(&file).unwrap();
    bytes.extend_from_slice(&[0; 16]);
    fs::write(&file, &bytes).unwrap();
    assert_eq!(s.scan().updated, 1);
    assert_eq!(rated(), [track.id]);

    // Removed (soft) and added again.
    let root = s.lib.roots().unwrap()[0].id;
    s.lib.remove_root(root, UI).unwrap();
    assert!(rated().is_empty());
    s.lib.add_root(&s.music(), UI).unwrap();
    s.scan();
    assert_eq!(rated(), [track.id]);
}
