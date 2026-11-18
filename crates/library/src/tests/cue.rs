//! CUE sheets: the parser and cutter (cue.rs), and scans of generated files (WAV written
//! here, a FLAC fixture of the engine).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::cue::{Cut, HTOA_MIN, Sheet, cue_meta, decode_sheet, parse_time};
use crate::tags::read::TrackMeta;
use crate::tests::{UI, fixture};
use crate::track_query::{CompareOp, Field, Filter, TrackQuery, Value};
use crate::{EndedReason, Library, Page, PlayEvent, ScanReport, ScanStart, Track, TrackId};

// --- parser ---

const EAC: &str = "\u{feff}REM GENRE \"Jazz\"\r\n\
REM DATE 1959\r\n\
REM DISCID 860B640B\r\n\
REM COMMENT \"ExactAudioCopy v1.6\"\r\n\
REM REPLAYGAIN_ALBUM_GAIN -7.89 dB\r\n\
REM REPLAYGAIN_ALBUM_PEAK 0.988\r\n\
CATALOG 5099749522626\r\n\
PERFORMER \"Miren Dalen\"\r\n\
TITLE \"Kites of Brass\"\r\n\
FILE \"Miren Dalen - Kites of Brass.wav\" WAVE\r\n\
  TRACK 01 AUDIO\r\n\
    TITLE \"Soft Wheels\"\r\n\
    PERFORMER \"Miren Dalen\"\r\n\
    SONGWRITER \"Miren Dalen\"\r\n\
    REM REPLAYGAIN_TRACK_GAIN -6.50 dB\r\n\
    ISRC USSM15900113\r\n\
    INDEX 01 00:00:00\r\n\
  TRACK 02 AUDIO\r\n\
    TITLE \"Fennel Ferryman\"\r\n\
    FLAGS DCP\r\n\
    INDEX 00 09:22:10\r\n\
    INDEX 01 09:24:00\r\n\
  track 03 audio\r\n\
    title Brass in Glass\r\n\
    performer \"Miren Dalen & Bren Elven\"\r\n\
    index 01 19:10:74\r\n";

#[test]
fn timestamps_are_cd_frames() {
    assert_eq!(parse_time("00:00:00"), Some(0));
    assert_eq!(parse_time("00:01:00"), Some(75));
    assert_eq!(parse_time("01:02:03"), Some((62 * 75) + 3));
    // Minutes past 99 (long images).
    assert_eq!(parse_time("120:00:00"), Some(120 * 60 * 75));
    for bad in [
        "00:60:00",
        "00:00:75",
        "1:2",
        "a:00:00",
        "00:00:00:00",
        ":00:00",
        "",
    ] {
        assert_eq!(parse_time(bad), None, "{bad}");
    }
}

fn tag<'a>(tags: &'a [(String, String)], key: &str) -> Option<&'a str> {
    tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

#[test]
fn sheet_fields_quoting_and_rem() {
    let sheet = Sheet::parse(EAC);
    assert_eq!(sheet.files, ["Miren Dalen - Kites of Brass.wav"]);
    assert_eq!(tag(&sheet.tags, "ALBUM"), Some("Kites of Brass"));
    assert_eq!(tag(&sheet.tags, "ALBUMARTIST"), Some("Miren Dalen"));
    assert_eq!(tag(&sheet.tags, "GENRE"), Some("Jazz"));
    assert_eq!(tag(&sheet.tags, "DATE"), Some("1959"));
    assert_eq!(tag(&sheet.tags, "COMMENT"), Some("ExactAudioCopy v1.6"));
    assert_eq!(tag(&sheet.tags, "CATALOG"), Some("5099749522626"));
    assert_eq!(tag(&sheet.tags, "REPLAYGAIN_ALBUM_GAIN"), Some("-7.89 dB"));
    assert_eq!(sheet.tracks.len(), 3);
    let cuts = sheet.cut(false);
    let numbers: Vec<u32> = cuts.iter().map(|c| c.number).collect();
    assert_eq!(numbers, [1, 2, 3]);
    let t1 = &cuts[0].tags;
    assert_eq!(tag(t1, "TITLE"), Some("Soft Wheels"));
    assert_eq!(tag(t1, "ARTIST"), Some("Miren Dalen"));
    assert_eq!(tag(t1, "COMPOSER"), Some("Miren Dalen"));
    assert_eq!(tag(t1, "ISRC"), Some("USSM15900113"));
    assert_eq!(tag(t1, "REPLAYGAIN_TRACK_GAIN"), Some("-6.50 dB"));
    assert_eq!(tag(t1, "TRACKNUMBER"), Some("1"));
    assert_eq!(tag(t1, "TRACKTOTAL"), Some("3"));
    // Unquoted values and lower-case commands.
    assert_eq!(tag(&cuts[2].tags, "TITLE"), Some("Brass in Glass"));
    assert_eq!(
        tag(&cuts[2].tags, "ARTIST"),
        Some("Miren Dalen & Bren Elven")
    );
    // A track without a performer has the sheet's; every track has the album's tags.
    assert_eq!(tag(&cuts[1].tags, "ARTIST"), Some("Miren Dalen"));
    assert_eq!(tag(&cuts[1].tags, "ALBUM"), Some("Kites of Brass"));
    assert_eq!(tag(&cuts[1].tags, "COMPOSER"), None);
}

#[test]
fn pregap_belongs_to_the_previous_track() {
    let cuts = Sheet::parse(EAC).cut(false);
    let ranges: Vec<(u64, Option<u64>)> = cuts.iter().map(|c| (c.start, c.end)).collect();
    let t = |m: u64, s: u64, f: u64| (m * 60 + s) * 75 + f;
    // Track 2 starts at its INDEX 01; its INDEX 00 (09:22:10) ends track 1 no earlier.
    assert_eq!(
        ranges,
        [
            (0, Some(t(9, 24, 0))),
            (t(9, 24, 0), Some(t(19, 10, 74))),
            (t(19, 10, 74), None),
        ]
    );
}

#[test]
fn hidden_track_before_track_one() {
    let sheet = |index1: &str| {
        Sheet::parse(&format!(
            "FILE \"a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 00 00:00:00\nINDEX 01 {index1}\n\
             TRACK 02 AUDIO\nINDEX 01 05:00:00\n"
        ))
    };
    // Longer than 10 s: track 0, up to track 1's INDEX 01.
    let cuts = sheet("00:10:01").cut(false);
    assert_eq!(cuts[0].number, 0);
    assert_eq!((cuts[0].start, cuts[0].end), (0, Some(HTOA_MIN + 1)));
    assert_eq!(tag(&cuts[0].tags, "TRACKNUMBER"), Some("0"));
    assert_eq!(tag(&cuts[0].tags, "TRACKTOTAL"), Some("2"));
    assert_eq!(tag(&cuts[0].tags, "TITLE"), None);
    assert_eq!((cuts[1].number, cuts[1].start), (1, HTOA_MIN + 1));
    // Exactly 10 s or shorter: silence, ignored (track 1 still starts at INDEX 01).
    let cuts = sheet("00:10:00").cut(false);
    assert_eq!(cuts.len(), 2);
    assert_eq!((cuts[0].number, cuts[0].start), (1, HTOA_MIN));
}

#[test]
fn multi_file_sheets() {
    // A gap appended to the previous file (EAC "noncompliant"): track 2's INDEX 00 is in
    // a.wav, its INDEX 01 at the start of b.wav.
    let text = "TITLE \"Album\"\n\
        FILE \"CD1\\a.wav\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n\
        TRACK 02 AUDIO\n    INDEX 00 03:00:00\n\
        FILE \"CD1\\b.wav\" WAVE\n    INDEX 01 00:00:00\n\
        TRACK 03 AUDIO\n    INDEX 01 02:00:00\n\
        TRACK 04 MODE1/2352\n    INDEX 01 04:00:00\n";
    let sheet = Sheet::parse(text);
    assert_eq!(sheet.files, ["CD1/a.wav", "CD1/b.wav"]);
    let cuts: Vec<(usize, u32, u64, Option<u64>)> = sheet
        .cut(false)
        .iter()
        .map(|c| (c.file, c.number, c.start, c.end))
        .collect();
    // Track 1 ends with its file; the data track is left out (and not counted).
    assert_eq!(
        cuts,
        [(0, 1, 0, None), (1, 2, 0, Some(9000)), (1, 3, 9000, None)]
    );
    assert_eq!(tag(&sheet.cut(false)[2].tags, "TRACKTOTAL"), Some("3"));
    // Embedded in one file, FILE lines do not matter.
    let single: Vec<(usize, u32)> = sheet.cut(true).iter().map(|c| (c.file, c.number)).collect();
    assert_eq!(
        single,
        [(0, 1), (0, 3)],
        "track 2 does not start after track 3"
    );
}

#[test]
fn broken_sheets_are_read_leniently() {
    let text = "garbage line\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n\
        FILE noquotes name.wav WAVE\nTRACK 01 AUDIO\nTITLE \"First\"\nINDEX 01 00:00:00\n\
        TRACK 01 AUDIO\nTITLE \"Repeated\"\nINDEX 01 00:30:00\n\
        TRACK 02 AUDIO\nTITLE \"No index\"\n\
        TRACK 03 AUDIO\nTITLE \"Quote \"inside\" title\"\nINDEX 01 bad\nINDEX 01 01:00:00\n";
    let sheet = Sheet::parse(text);
    assert_eq!(sheet.files, ["noquotes name.wav"]);
    let cuts = sheet.cut(false);
    let titles: Vec<(u32, Option<&str>)> = cuts
        .iter()
        .map(|c| (c.number, tag(&c.tags, "TITLE")))
        .collect();
    assert_eq!(
        titles,
        [(1, Some("First")), (3, Some("Quote \"inside\" title"))]
    );
    assert_eq!(cuts[0].end, Some(75 * 60));
    assert!(Sheet::parse("").cut(false).is_empty());
    assert!(Sheet::parse("not a sheet at all").cut(false).is_empty());
}

#[test]
fn legacy_encodings_and_byte_order_marks() {
    let text = "PERFORMER \"周砚舟\"\r\nTITLE \"三棱雾\"\r\nFILE \"三棱雾.flac\" WAVE\r\n\
        TRACK 01 AUDIO\r\nTITLE \"我的地盘\"\r\nINDEX 01 00:00:00\r\n";
    let (gbk, _, _) = encoding_rs::GBK.encode(text);
    let (decoded, encoding) = decode_sheet(&gbk, None);
    assert_eq!(decoded, text);
    assert_eq!(encoding, Some(encoding_rs::GBK));
    let sheet = Sheet::parse(&decoded);
    assert_eq!(sheet.files, ["三棱雾.flac"]);
    assert_eq!(tag(&sheet.cut(false)[0].tags, "TITLE"), Some("我的地盘"));
    // A root's forced encoding replaces the detection.
    let (big5, _, _) = encoding_rs::BIG5.encode("TITLE \"愛情\"\n");
    let (decoded, encoding) = decode_sheet(&big5, Some(encoding_rs::BIG5));
    assert_eq!(
        (decoded.as_str(), encoding),
        ("TITLE \"愛情\"\n", Some(encoding_rs::BIG5))
    );
    // UTF-8 with and without a byte-order mark, UTF-16 LE with one.
    let (decoded, encoding) = decode_sheet(format!("\u{feff}{text}").as_bytes(), None);
    assert_eq!((decoded.as_str(), encoding), (text, None));
    let utf16: Vec<u8> = [0xff, 0xfe]
        .into_iter()
        .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    assert_eq!(decode_sheet(&utf16, None), (text.to_owned(), None));
}

#[test]
fn sheet_tags_win_and_the_file_fills_in() {
    let base = TrackMeta {
        title: Some("Whole image".into()),
        artist: Some("File Artist".into()),
        album: Some("File Album".into()),
        genre: Some("File Genre".into()),
        year: Some(2001),
        track_no: Some(1),
        disc_no: Some(2),
        duration_ms: Some(1_500_000),
        sample_rate: Some(44_100),
        codec: Some("flac"),
        tags: vec![
            ("TITLE".into(), "Whole image".into()),
            ("ARTIST".into(), "File Artist".into()),
            ("GENRE".into(), "File Genre".into()),
            ("LABEL".into(), "Cloudbox".into()),
            ("TRACKNUMBER".into(), "1".into()),
            ("CUESHEET".into(), "FILE …".into()),
        ],
        embedded_cue: Some("FILE …".into()),
        ..TrackMeta::default()
    };
    let sheet = Sheet::parse(EAC);
    let cuts: Vec<Cut> = sheet.cut(false);
    let meta = cue_meta(&base, &cuts[0], Some(encoding_rs::GBK));
    assert_eq!(meta.title.as_deref(), Some("Soft Wheels"));
    assert_eq!(meta.artist.as_deref(), Some("Miren Dalen"));
    assert_eq!(meta.album.as_deref(), Some("Kites of Brass"));
    assert_eq!(meta.album_artist.as_deref(), Some("Miren Dalen"));
    assert_eq!(meta.genre.as_deref(), Some("Jazz"));
    assert_eq!(meta.year, Some(1959));
    assert_eq!((meta.track_no, meta.track_total), (Some(1), Some(3)));
    // Not in the sheet: the file's.
    assert_eq!(meta.disc_no, Some(2));
    assert_eq!(meta.codec, Some("flac"));
    assert_eq!(meta.replay_gain.track_gain_db, Some(-6.5));
    assert_eq!(meta.replay_gain.album_gain_db, Some(-7.89));
    assert_eq!(meta.tag_encoding, Some(encoding_rs::GBK));
    assert_eq!(meta.embedded_cue, None);
    assert_eq!(meta.duration_ms, Some(9 * 60_000 + 24_000));
    let keys = |m: &TrackMeta, key: &str| -> Vec<String> {
        m.tags
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    };
    assert_eq!(keys(&meta, "TITLE"), ["Soft Wheels"]);
    assert_eq!(keys(&meta, "LABEL"), ["Cloudbox"]);
    assert_eq!(keys(&meta, "TRACKNUMBER"), ["1"]);
    assert!(keys(&meta, "CUESHEET").is_empty());
    // The last track runs to the end of the file.
    let last = cue_meta(&base, &cuts[2], None);
    assert_eq!(
        last.duration_ms,
        Some(1_500_000 - (19 * 60 + 10) * 1000 - 74 * 1000 / 75)
    );
    assert_eq!(last.tag_encoding, None);
}

// --- scans ---

const RATE: u32 = 44_100;

/// A 16-bit stereo WAV of `seconds` at 44.1 kHz.
fn write_wav(path: &Path, seconds: u32) {
    let frames = RATE * seconds;
    let data = frames * 4;
    let mut bytes = Vec::with_capacity(44 + data as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&RATE.to_le_bytes());
    bytes.extend_from_slice(&(RATE * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for i in 0..frames {
        let s = ((i % 200) as i16 - 100) * 50;
        bytes.extend_from_slice(&s.to_le_bytes());
        bytes.extend_from_slice(&(-s).to_le_bytes());
    }
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// Writes `text` and gives it a modification time `age` seconds in the past (a rescan
/// then sees each write as a change even within one millisecond).
fn write_at(path: &Path, bytes: &[u8], age: u64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(age))
        .unwrap();
}

fn scan(lib: &Library) -> ScanReport {
    match lib.scan(|_| {}).unwrap() {
        ScanStart::Started(handle) => handle.wait().unwrap(),
        ScanStart::Coalesced(_) => panic!("no scan should be running"),
    }
}

/// The tracks queries see (available, not hidden), by path then CUE number.
fn visible(lib: &Library) -> Vec<Track> {
    let (rows, _) = lib
        .tracks_query(&TrackQuery::default(), Page::new(0, 1000), &[])
        .unwrap();
    let mut tracks: Vec<Track> = rows.into_iter().map(|r| r.track).collect();
    tracks.sort_by_key(|t| {
        (
            t.rel_path.clone(),
            t.cue.as_ref().map(|c| (c.sheet.clone(), c.number)),
        )
    });
    tracks
}

/// Every row a query mentioning `available` sees: available or not, but never a hidden one.
fn all_rows(lib: &Library) -> Vec<Track> {
    let either = Filter::Or {
        any: [true, false]
            .map(|on| Filter::Compare {
                field: Field::Available,
                op: CompareOp::Eq,
                value: Value::Bool(on),
            })
            .into(),
    };
    let query = TrackQuery {
        filter: either,
        sort: Vec::new(),
    };
    let (rows, _) = lib.tracks_query(&query, Page::new(0, 1000), &[]).unwrap();
    rows.into_iter().map(|r| r.track).collect()
}

fn titles(tracks: &[Track]) -> Vec<(Option<u32>, String)> {
    tracks
        .iter()
        .map(|t| {
            (
                t.cue.as_ref().map(|c| c.number),
                t.display_title().to_owned(),
            )
        })
        .collect()
}

fn by_number(tracks: &[Track], number: u32) -> &Track {
    tracks
        .iter()
        .find(|t| t.cue.as_ref().is_some_and(|c| c.number == number))
        .unwrap_or_else(|| panic!("no track {number} in {:?}", titles(tracks)))
}

const SHEET: &str = "PERFORMER \"Band\"\nTITLE \"Album\"\nREM GENRE Rock\n\
    FILE \"image.wav\" WAVE\n\
    TRACK 01 AUDIO\n  TITLE \"One\"\n  INDEX 01 00:00:00\n\
    TRACK 02 AUDIO\n  TITLE \"Two\"\n  INDEX 00 00:03:00\n  INDEX 01 00:04:00\n\
    TRACK 03 AUDIO\n  TITLE \"Three\"\n  INDEX 01 00:08:00\n";

#[test]
fn sheet_tracks_have_stable_ids_and_the_image_is_hidden() {
    let f = fixture();
    let music = f.lib.roots().unwrap()[0].path.clone();
    let album = music.join("Band - Album");
    write_wav(&album.join("image.wav"), 12);
    // Indexed as a file first.
    scan(&f.lib);
    let image = visible(&f.lib);
    assert_eq!(titles(&image), [(None, "image".to_owned())]);
    let image_id = image[0].id;

    write_at(&album.join("image.cue"), SHEET.as_bytes(), 50);
    let report = scan(&f.lib);
    assert_eq!((report.added, report.errors), (3, 0), "{report:?}");
    let tracks = visible(&f.lib);
    assert_eq!(
        titles(&tracks),
        [
            (Some(1), "One".to_owned()),
            (Some(2), "Two".to_owned()),
            (Some(3), "Three".to_owned())
        ]
    );
    for track in &tracks {
        // The image's path and size, the track's own duration and tags.
        assert_eq!(track.rel_path, "Band - Album/image.wav");
        assert_eq!(track.size, image[0].size);
        assert_eq!(track.duration_ms, Some(4000));
        assert_eq!(track.album.as_deref(), Some("Album"));
        assert_eq!(track.artist.as_deref(), Some("Band"));
        assert_eq!(track.genre.as_deref(), Some("Rock"));
        assert_eq!(track.sample_rate, Some(RATE));
        assert_eq!(track.cue.as_ref().unwrap().sheet, "Band - Album/image.cue");
    }
    let two = by_number(&tracks, 2);
    assert_eq!(
        (
            two.cue.as_ref().unwrap().start,
            two.cue.as_ref().unwrap().end
        ),
        (300, Some(600))
    );
    assert_eq!(two.track_no, Some(2));
    assert_eq!(
        f.lib.frame_range(two.id).unwrap(),
        Some((4 * 44_100, Some(8 * 44_100)))
    );
    assert_eq!(f.lib.frame_range(image_id).unwrap(), None);
    // The image is not a track of its own any more, not even an unavailable one.
    let hidden = f.lib.track(image_id).unwrap().unwrap();
    assert!(!hidden.available);
    assert!(all_rows(&f.lib).iter().all(|t| t.id != image_id));
    assert_eq!(f.lib.roots().unwrap()[0].track_count, 3);
    let album_view = f.lib.album(&tracks[0].album_key).unwrap().unwrap();
    assert_eq!(album_view.summary.track_count, 3);
    assert_eq!(album_view.summary.duration_ms, 12_000);
    // Search finds tracks by their sheet titles.
    let found = f.lib.search("three", 10).unwrap();
    assert_eq!(titles(&found.tracks), [(Some(3), "Three".to_owned())]);

    // Statistics stay with the track across edits of the sheet.
    f.lib
        .record_play(PlayEvent {
            track_id: two.id,
            context: None,
            started_at: 1_700_000_000_000,
            played_ms: 4000,
            ended_reason: EndedReason::Completed,
            origin: "ui".into(),
        })
        .unwrap();
    f.lib.set_rating(&[two.id], Some(4), UI).unwrap();
    f.lib.flush().unwrap();
    let ids: Vec<TrackId> = tracks.iter().map(|t| t.id).collect();

    // Edited: track 2 renamed, a fourth track added; a rescan of an unchanged sheet
    // changes nothing.
    assert_eq!(scan(&f.lib).updated, 0);
    let edited = SHEET.replace("\"Two\"", "\"Deux\"")
        + "TRACK 04 AUDIO\n  TITLE \"Four\"\n  INDEX 01 00:10:00\n";
    write_at(&album.join("image.cue"), edited.as_bytes(), 40);
    let report = scan(&f.lib);
    assert_eq!((report.added, report.updated), (1, 3), "{report:?}");
    let tracks = visible(&f.lib);
    assert_eq!(tracks.len(), 4);
    assert_eq!(tracks[..3].iter().map(|t| t.id).collect::<Vec<_>>(), ids);
    assert_eq!(by_number(&tracks, 2).display_title(), "Deux");
    assert_eq!(by_number(&tracks, 3).duration_ms, Some(2000));
    assert_eq!(by_number(&tracks, 4).duration_ms, Some(2000));
    let row = &f.lib.track_rows(&[ids[1]], &[]).unwrap()[0];
    assert_eq!((row.play_count, row.rating), (1, Some(4)));

    // Track 4 removed again: it becomes unavailable (kept for history).
    write_at(&album.join("image.cue"), SHEET.as_bytes(), 30);
    let report = scan(&f.lib);
    assert_eq!(report.removed, 1, "{report:?}");
    let four = by_number(&tracks, 4).id;
    assert!(!f.lib.track(four).unwrap().unwrap().available);

    // The sheet removed: its tracks go, the image is a track again, with its old id.
    std::fs::remove_file(album.join("image.cue")).unwrap();
    let report = scan(&f.lib);
    assert_eq!(report.removed, 3, "{report:?}");
    let back = visible(&f.lib);
    assert_eq!(titles(&back), [(None, "image".to_owned())]);
    assert_eq!(back[0].id, image_id);
    assert_eq!(back[0].duration_ms, Some(12_000));

    // And back: the same tracks return.
    write_at(&album.join("image.cue"), SHEET.as_bytes(), 20);
    scan(&f.lib);
    let tracks = visible(&f.lib);
    assert_eq!(tracks.iter().map(|t| t.id).collect::<Vec<_>>(), ids);
    let row = &f.lib.track_rows(&[ids[1]], &[]).unwrap()[0];
    assert_eq!((row.play_count, row.rating), (1, Some(4)));
}

#[test]
fn sheet_queries() {
    let f = fixture();
    let music = f.lib.roots().unwrap()[0].path.clone();
    write_wav(&music.join("A/image.wav"), 12);
    write_at(&music.join("A/image.cue"), SHEET.as_bytes(), 50);
    write_wav(&music.join("B/single.wav"), 1);
    scan(&f.lib);
    let cue = |on: bool| TrackQuery {
        filter: Filter::Compare {
            field: Field::CueSheet,
            op: CompareOp::Eq,
            value: Value::Bool(on),
        },
        sort: Vec::new(),
    };
    let (rows, total) = f
        .lib
        .tracks_query(&cue(true), Page::new(0, 10), &[])
        .unwrap();
    assert_eq!(total, 3);
    assert!(rows.iter().all(|r| r.track.cue.is_some()));
    let (rows, _) = f
        .lib
        .tracks_query(&cue(false), Page::new(0, 10), &[])
        .unwrap();
    assert_eq!(
        titles(&rows.into_iter().map(|r| r.track).collect::<Vec<_>>()),
        [(None, "single".to_owned())]
    );
    // Album order is the sheet's.
    let tracks = visible(&f.lib);
    let album = f
        .lib
        .album(&by_number(&tracks, 1).album_key)
        .unwrap()
        .unwrap();
    let order: Vec<u32> = album.tracks.iter().filter_map(|t| t.track_no).collect();
    assert_eq!(order, [1, 2, 3]);
}

/// The engine's 3 s, 44.1 kHz stereo FLAC fixture.
fn flac_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_full.flac")
}

#[test]
fn file_names_resolve_by_stem_and_letter_case() {
    let f = fixture();
    let music = f.lib.roots().unwrap()[0].path.clone();
    std::fs::create_dir_all(music.join("A")).unwrap();
    // A sheet written for the WAV, next to the FLAC it was encoded to.
    std::fs::copy(flac_fixture(), music.join("A/rip.flac")).unwrap();
    let sheet = "FILE \"rip.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n\
        TRACK 02 AUDIO\nINDEX 01 00:01:00\n";
    write_at(&music.join("A/rip.cue"), sheet.as_bytes(), 50);
    // Another letter case; a format the library does not read (left alone, no tracks);
    // a missing file (an error).
    write_wav(&music.join("B/Image.WAV"), 2);
    let sheet = "FILE \"image.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n\
        FILE \"other.ape\" WAVE\nTRACK 02 AUDIO\nINDEX 01 00:00:00\n\
        FILE \"gone.wav\" WAVE\nTRACK 03 AUDIO\nINDEX 01 00:00:00\n";
    write_at(&music.join("B/b.cue"), sheet.as_bytes(), 50);
    std::fs::write(music.join("B/other.ape"), b"MAC not really").unwrap();
    let report = scan(&f.lib);
    assert_eq!(report.errors, 1, "{report:?}");
    assert!(
        report.first_errors[0].message.contains("gone.wav"),
        "{report:?}"
    );
    let tracks = visible(&f.lib);
    let paths: Vec<(String, Option<u32>, Option<u64>)> = tracks
        .iter()
        .map(|t| {
            (
                t.rel_path.clone(),
                t.cue.as_ref().map(|c| c.number),
                t.duration_ms,
            )
        })
        .collect();
    assert_eq!(
        paths,
        [
            ("A/rip.flac".to_owned(), Some(1), Some(1000)),
            ("A/rip.flac".to_owned(), Some(2), Some(2000)),
            ("B/Image.WAV".to_owned(), None, Some(2000)),
        ]
    );
    assert_eq!(by_number(&tracks, 2).codec.as_deref(), Some("flac"));
}

#[test]
fn hidden_track_and_two_sheets_for_one_image() {
    let f = fixture();
    let music = f.lib.roots().unwrap()[0].path.clone();
    write_wav(&music.join("A/disc.wav"), 14);
    let htoa = "FILE \"disc.wav\" WAVE\nTRACK 01 AUDIO\nTITLE \"First\"\n\
        INDEX 00 00:00:00\nINDEX 01 00:11:00\n";
    write_at(&music.join("A/disc.cue"), htoa.as_bytes(), 50);
    // A second sheet for the same file (e.g. another encoding): the one named like the
    // file wins, so the tracks are not doubled.
    let other = "FILE \"disc.wav\" WAVE\nTRACK 01 AUDIO\nTITLE \"Other\"\nINDEX 01 00:00:00\n";
    write_at(&music.join("A/another.cue"), other.as_bytes(), 50);
    scan(&f.lib);
    let tracks = visible(&f.lib);
    assert_eq!(
        titles(&tracks),
        [(Some(0), "disc".to_owned()), (Some(1), "First".to_owned())]
    );
    assert_eq!(tracks[0].duration_ms, Some(11_000));
    assert_eq!(tracks[0].track_no, Some(0));
    assert_eq!(tracks[1].duration_ms, Some(3000));
}

#[test]
fn embedded_sheets_and_a_sidecar_that_wins() {
    use lofty::config::WriteOptions;
    use lofty::ogg::tag::VorbisComments;
    use lofty::tag::TagExt;

    let f = fixture();
    let music = f.lib.roots().unwrap()[0].path.clone();
    std::fs::create_dir_all(music.join("A")).unwrap();
    let flac = music.join("A/image.flac");
    std::fs::copy(flac_fixture(), &flac).unwrap();
    let mut comments = VorbisComments::default();
    comments.push("ALBUM".into(), "Embedded Album".into());
    comments.push(
        "CUESHEET".into(),
        "FILE \"CDImage.wav\" WAVE\nTRACK 01 AUDIO\nTITLE \"E1\"\nINDEX 01 00:00:00\n\
         TRACK 02 AUDIO\nTITLE \"E2\"\nINDEX 01 00:02:00\n"
            .into(),
    );
    comments
        .save_to_path(&flac, WriteOptions::default())
        .unwrap();
    scan(&f.lib);
    let tracks = visible(&f.lib);
    assert_eq!(
        titles(&tracks),
        [(Some(1), "E1".to_owned()), (Some(2), "E2".to_owned())]
    );
    assert_eq!(tracks[0].cue.as_ref().unwrap().sheet, "A/image.flac");
    assert_eq!(tracks[0].album.as_deref(), Some("Embedded Album"));
    assert_eq!(tracks[1].duration_ms, Some(1000));
    let embedded: Vec<TrackId> = tracks.iter().map(|t| t.id).collect();
    // Unchanged: nothing re-read.
    let report = scan(&f.lib);
    assert_eq!((report.updated, report.added, report.removed), (0, 0, 0));

    // A sidecar sheet wins.
    let sidecar = r#"TITLE "Sidecar"
FILE "image.flac" WAVE
TRACK 01 AUDIO
TITLE "S1"
INDEX 01 00:00:00
TRACK 02 AUDIO
TITLE "S2"
INDEX 01 00:01:00
"#;
    write_at(&music.join("A/image.cue"), sidecar.as_bytes(), 50);
    let report = scan(&f.lib);
    assert_eq!((report.added, report.removed), (2, 2), "{report:?}");
    let tracks = visible(&f.lib);
    assert_eq!(
        titles(&tracks),
        [(Some(1), "S1".to_owned()), (Some(2), "S2".to_owned())]
    );
    assert_eq!(tracks[0].album.as_deref(), Some("Sidecar"));
    assert_eq!(tracks[0].duration_ms, Some(1000));

    // Gone again: the embedded tracks are back, with their ids.
    std::fs::remove_file(music.join("A/image.cue")).unwrap();
    let report = scan(&f.lib);
    assert_eq!((report.restored, report.removed), (2, 2), "{report:?}");
    let tracks = visible(&f.lib);
    assert_eq!(tracks.iter().map(|t| t.id).collect::<Vec<_>>(), embedded);
}

#[test]
fn extreme_timestamps_do_not_panic_or_extend_a_file() {
    for bad in [
        "18446744073709551615:00:00",
        "99999999999:00:00",
        "100001:00:00",
    ] {
        assert_eq!(parse_time(bad), None);
    }
    assert_eq!(crate::cue::cd_frames_to_ms(u64::MAX), u64::MAX);
    let f = fixture();
    let music = f.lib.roots().unwrap()[0].path.clone();
    write_wav(&music.join("image.wav"), 3);
    write_at(&music.join("image.cue"),

    b"FILE image.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:02:00\nTRACK 03 AUDIO\nINDEX 01 00:10:00\nTRACK 04 AUDIO\nINDEX 01 18446744073709551615:00:00\n"
    ,
     50);

    let report = scan(&f.lib);
    assert_eq!(report.errors, 0, "{report:?}");
    let tracks = visible(&f.lib);
    assert_eq!(tracks.len(), 2);
    assert_eq!(by_number(&tracks, 2).duration_ms, Some(1000));
    assert_eq!(by_number(&tracks, 2).cue.as_ref().unwrap().end_ms(), None);
    assert_eq!(scan(&f.lib).errors, 0);
}

#[test]
fn eac_independent_files_shared_image_and_mixed_sheets() {
    for (sheet, expected, shared) in [
        (
            concat!(
                "FILE \"01 feat.KAORI.wav\" WAVE\nTRACK 01 AUDIO\nTITLE \"First\"\nINDEX ",
                "01 00:00:00\nFILE \"02 other.wav\" WAVE\nTRACK 02 AUDIO\nINDEX 01 ",
                "00:00:00\n",
            ),
            2,
            0,
        ),
        (
            concat!(
                "FILE \"01 feat.KAORI.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK ",
                "02 AUDIO\nINDEX 01 00:01:00\n",
            ),
            3,
            2,
        ),
        (
            concat!(
                "FILE \"01 feat.KAORI.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK ",
                "02 AUDIO\nINDEX 01 00:01:00\nFILE \"02 other.wav\" WAVE\nTRACK 03 ",
                "AUDIO\nINDEX 01 00:00:00\n",
            ),
            3,
            2,
        ),
    ] {
        let f = fixture();
        let root = f.lib.roots().unwrap()[0].path.clone();
        for name in ["01 FEAT. KAORI.flac", "02 Other.flac"] {
            std::fs::copy(flac_fixture(), root.join(name)).unwrap();
        }
        write_at(&root.join("EAC.cue"), sheet.as_bytes(), 50);
        let report = scan(&f.lib);
        assert_eq!(report.errors, 0, "{report:?}");
        let tracks = visible(&f.lib);
        assert_eq!(tracks.len(), expected);
        assert_eq!(tracks.iter().filter(|t| t.cue.is_some()).count(), shared);
        let ids: Vec<_> = tracks
            .iter()
            .filter(|t| t.cue.is_none())
            .map(|t| t.id)
            .collect();
        let preview = f
            .lib
            .preview_cover_edit(&ids, &crate::CoverChange::Remove)
            .unwrap();
        assert!(preview.skipped.iter().all(|(_, code)| *code != "cue_track"));
        let unchanged = scan(&f.lib);
        assert_eq!(
            unchanged.updated, 0,
            "unchanged independent CUE files must stay quiet"
        );
        assert_eq!(
            visible(&f.lib).iter().map(|t| t.id).collect::<Vec<_>>(),
            tracks.iter().map(|t| t.id).collect::<Vec<_>>()
        );
    }
}

#[test]
fn rescanning_preserves_ambiguous_shared_cue_identity_and_missing_advisories_stay_stable() {
    let f = fixture();
    let root = f.lib.roots().unwrap()[0].path.clone();
    for name in ["a.flac", "b.flac"] {
        std::fs::copy(flac_fixture(), root.join(name)).unwrap();
    }
    write_at(
        &root.join("EAC.cue"),
        b"FILE a.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:01:00\n",
        50,
    );
    scan(&f.lib);
    assert_eq!(
        visible(&f.lib).iter().filter(|t| t.cue.is_some()).count(),
        2
    );
    write_at(&root.join("EAC.cue"),
    b"FILE a.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nFILE b.wav WAVE\nTRACK 02 AUDIO\nINDEX 01 00:00:00\n"
    ,
    40);

    scan(&f.lib);
    let tracks = visible(&f.lib);
    // Two historical cuts of a.flac are not evidence of a mistaken singleton.
    // Preserve both rather than merging their identities into a new whole-file id.
    assert_eq!(tracks.len(), 3);
    assert_eq!(
        tracks
            .iter()
            .filter(|t| t.rel_path == "a.flac" && t.cue.is_some())
            .count(),
        2
    );
    assert!(
        tracks
            .iter()
            .any(|t| t.rel_path == "b.flac" && t.cue.is_none())
    );
    write_at(&root.join("EAC.cue"),
    b"FILE absent.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nFILE also-absent.wav WAVE\nTRACK 02 AUDIO\nINDEX 01 00:00:00\n"
    ,
    30);

    let first = scan(&f.lib);
    assert_eq!(first.errors, 1);
    assert_eq!(
        first.first_errors[0].code,
        crate::ScanErrorCode::CueMissingFiles
    );
    let repeated = scan(&f.lib);
    assert_eq!(
        repeated.errors, 1,
        "one sheet-wide diagnostic, not one error per FILE"
    );
    assert_eq!(
        repeated.first_errors, first.first_errors,
        "stable diagnostics let automatic retries suppress duplicate cards"
    );
    assert_eq!(visible(&f.lib).len(), 2);
}

#[test]
fn independent_cue_tags_only_fill_missing_tags_and_no_index_is_full_file() {
    let f = fixture();
    let root = f.lib.roots().unwrap()[0].path.clone();
    write_wav(&root.join("untagged.wav"), 2);
    write_at(
        &root.join("EAC.cue"),
        b"FILE untagged.wav WAVE\nTRACK 01 AUDIO\nTITLE \"Supplement\"\nPERFORMER \"Cue Person\"\n",
        50,
    );
    scan(&f.lib);
    let tracks = visible(&f.lib);
    assert_eq!(tracks.len(), 1);
    assert!(tracks[0].cue.is_none());
    assert_eq!(tracks[0].title.as_deref(), Some("Supplement"));
    assert_eq!(tracks[0].artist.as_deref(), Some("Cue Person"));
    let id = tracks[0].id;
    f.lib
        .edit_tags(
            &[id],
            &[crate::TagChange::Set {
                key: "TITLE".into(),
                values: vec!["File title".into()],
            }],
            UI,
            crate::TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    scan(&f.lib);
    assert_eq!(visible(&f.lib)[0].title.as_deref(), Some("File title"));
    assert!(visible(&f.lib)[0].cue.is_none());
    std::fs::remove_file(root.join("EAC.cue")).unwrap();
    scan(&f.lib);
    assert_eq!(
        visible(&f.lib)[0].artist,
        None,
        "removing the sheet removes its display supplement"
    );
}

#[test]
fn historical_embedded_cut_with_incomplete_people_does_not_mask_a_supplement_change() {
    const REL_PATH: &str = "song.flac";
    const SHEET: &str = concat!(
        "FILE song.wav WAVE\nTRACK 01 AUDIO\nTITLE Original title\n",
        "PERFORMER Original artist\nINDEX 01 00:00:00\n",
    );

    let f = fixture();
    let root = f.lib.roots().unwrap()[0].path.clone();
    std::fs::copy(flac_fixture(), root.join(REL_PATH)).unwrap();
    let cue = root.join("album.cue");
    write_at(&cue, SHEET.as_bytes(), 50);
    assert_eq!(scan(&f.lib).added, 1);
    let whole = visible(&f.lib).pop().unwrap();
    assert_eq!(whole.title.as_deref(), Some("Original title"));
    assert_eq!(whole.artist.as_deref(), Some("Original artist"));
    assert!(whole.artists_complete);

    let (size, mtime) = (whole.size, whole.mtime_ms);
    let historical_cut = crate::db::writer::TrackRecord {
        size,
        mtime_ms: mtime,
        cue: Some(crate::db::writer::CueRecord {
            sheet: REL_PATH.into(),
            number: 1,
            start: 75,
            end: None,
            size,
            mtime_ms: mtime,
        }),
        ..crate::tests::record(f.root, REL_PATH, TrackMeta::default())
    };
    crate::tests::upsert(&f.lib, vec![historical_cut]);
    f.lib
        .inner
        .writer
        .run_data(|conn| {
            conn.execute(
                "UPDATE tracks SET available=0, artists_parser_version=0 WHERE cue_sheet=?1",
                [REL_PATH],
            )?;
            Ok(())
        })
        .unwrap();

    let changed = SHEET.replace("Original", "Changed");
    write_at(&cue, changed.as_bytes(), 40);
    let report = scan(&f.lib);
    assert_eq!((report.updated, report.errors), (1, 0), "{report:?}");
    let keys = ["TITLE".into(), "ARTIST".into()];
    let row = &f.lib.track_rows(&[whole.id], &keys).unwrap()[0];
    assert_eq!(row.track.title.as_deref(), Some("Changed title"));
    assert_eq!(row.track.artist.as_deref(), Some("Changed artist"));
    assert_eq!(row.tags["TITLE"], ["Changed title"]);
    assert_eq!(row.tags["ARTIST"], ["Changed artist"]);
    assert_eq!(row.track.artists[0].name, "Changed artist");
    let tracks = visible(&f.lib);
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].id, whole.id);
    assert!(tracks[0].cue.is_none());
    assert_eq!(scan(&f.lib).updated, 0);
}

#[test]
fn delayed_supplement_scan_keeps_committed_tag_edit() {
    const REL_PATH: &str = "song.wav";
    const SHEET: &str = concat!(
        "FILE song.wav WAVE\nTRACK 01 AUDIO\nTITLE Initial title\n",
        "PERFORMER Initial artist\nINDEX 01 00:00:00\n",
    );

    let f = fixture();
    let root = f.lib.roots().unwrap()[0].path.clone();
    write_wav(&root.join(REL_PATH), 2);
    let cue = root.join("album.cue");
    write_at(&cue, SHEET.as_bytes(), 50);
    assert_eq!(scan(&f.lib).added, 1);
    let original = scan_snapshot(&f.lib);
    let id = original[0].id;

    let changed = SHEET.replace("Initial", "Changed CUE");
    write_at(&cue, changed.as_bytes(), 40);
    let (read, reached) = crossbeam_channel::bounded(1);
    let (resume, resume_scan) = crossbeam_channel::bounded(1);
    let wait = Duration::from_secs(15);
    *f.lib.inner.hooks.after_scan_audio_read.lock().unwrap() =
        Some(std::sync::Arc::new(move |_, path| {
            assert_eq!(path, REL_PATH);
            read.send(()).unwrap();
            resume_scan.recv_timeout(wait).unwrap();
        }));
    let ScanStart::Started(delayed) = f.lib.scan(|_| {}).unwrap() else {
        panic!("expected a new scan");
    };
    reached.recv_timeout(wait).unwrap();

    let edit = f
        .lib
        .edit_tags(
            &[id],
            &[crate::TagChange::Set {
                key: "TITLE".into(),
                values: vec!["Edited file title".into()],
            }],
            UI,
            crate::TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(edit.written, [id], "{edit:?}");
    let committed = scan_snapshot(&f.lib);
    assert_eq!(committed[0].revision, original[0].revision + 1);
    let keys = ["TITLE".into(), "ARTIST".into()];
    let edited = f.lib.track_rows(&[id], &keys).unwrap();
    assert_eq!(edited[0].track.title.as_deref(), Some("Edited file title"));
    assert_eq!(edited[0].tags["TITLE"], ["Edited file title"]);
    assert_eq!(edited[0].track.artist, None);
    assert!(!edited[0].tags.contains_key("ARTIST"));

    resume.send(()).unwrap();
    let report = delayed.wait().unwrap();
    *f.lib.inner.hooks.after_scan_audio_read.lock().unwrap() = None;
    assert_eq!((report.updated, report.errors), (0, 0), "{report:?}");
    assert_eq!(f.lib.track_rows(&[id], &keys).unwrap(), edited);
    assert_eq!(scan_snapshot(&f.lib), committed);

    let report = scan(&f.lib);
    assert_eq!((report.updated, report.errors), (1, 0), "{report:?}");
    let row = &f.lib.track_rows(&[id], &keys).unwrap()[0];
    assert_eq!(row.track.title.as_deref(), Some("Edited file title"));
    assert_eq!(row.track.artist.as_deref(), Some("Changed CUE artist"));
    assert_eq!(row.tags["TITLE"], ["Edited file title"]);
    assert_eq!(row.tags["ARTIST"], ["Changed CUE artist"]);
    assert_eq!(scan_snapshot(&f.lib)[0].revision, committed[0].revision + 1);
    assert_eq!(scan(&f.lib).updated, 0);
}

#[test]
fn supplement_rescans_only_update_tracks_with_persisted_changes() {
    use lofty::config::WriteOptions;
    use lofty::ogg::tag::VorbisComments;
    use lofty::tag::TagExt;

    const SINGLES: &str = concat!(
        "FILE tagged.flac WAVE\nTRACK 01 AUDIO\nTITLE Ignored title\nPERFORMER Ignored artist\n",
        "INDEX 01 00:00:00\nFILE untagged.wav WAVE\nTRACK 02 AUDIO\n",
        "TITLE CUE title\nPERFORMER CUE artist\nINDEX 01 00:00:00\n",
    );
    const IMAGE: &str = concat!(
        "FILE image.wav WAVE\nTRACK 01 AUDIO\nTITLE First cut\nINDEX 01 00:00:00\n",
        "TRACK 02 AUDIO\nTITLE Second cut\nINDEX 01 00:01:00\n",
    );

    let f = fixture();
    let root = f.lib.roots().unwrap()[0].path.clone();
    let tagged = root.join("tagged.flac");
    std::fs::copy(flac_fixture(), &tagged).unwrap();
    let mut comments = VorbisComments::default();
    comments.push("TITLE".into(), "File title".into());
    comments.push("ARTIST".into(), "First Artist".into());
    comments.push("ARTIST".into(), "Second Artist".into());
    comments
        .save_to_path(&tagged, WriteOptions::default())
        .unwrap();
    let image = root.join("image.wav");
    write_wav(&image, 2);
    write_wav(&root.join("untagged.wav"), 2);
    let singles = root.join("singles.cue");
    let image_sheet = root.join("image.cue");
    write_at(&singles, SINGLES.as_bytes(), 50);
    write_at(&image_sheet, IMAGE.as_bytes(), 50);
    let initial = scan(&f.lib);
    assert_eq!(initial.errors, 0, "{initial:?}");
    let tracks = visible(&f.lib);
    let [first, second, tagged, untagged] = tracks.as_slice() else {
        panic!("expected two cuts and two whole files");
    };
    assert_eq!(first.rel_path, "image.wav");
    assert_eq!(second.rel_path, "image.wav");
    assert_eq!(first.cue.as_ref().unwrap().number, 1);
    assert_eq!(second.cue.as_ref().unwrap().number, 2);
    assert!(tagged.cue.is_none() && untagged.cue.is_none());
    assert_eq!(tagged.title.as_deref(), Some("File title"));
    assert_eq!(tagged.artist.as_deref(), Some("First Artist"));
    assert_eq!(untagged.title.as_deref(), Some("CUE title"));
    assert_eq!(untagged.artist.as_deref(), Some("CUE artist"));

    let db_dir = f._dir;
    let db = db_dir.path().join("db/library.sqlite");
    let before = scan_snapshot(&f.lib);
    drop(f.lib);
    let lib = Library::open(&db).unwrap();
    assert_eq!(scan(&lib).updated, 0);
    assert_eq!(scan_snapshot(&lib), before);
    *lib.inner.hooks.fail_audio_read.lock().unwrap() = Some(root.join("untagged.wav"));
    let cached = scan(&lib);
    assert_eq!((cached.updated, cached.errors), (0, 0), "{cached:?}");
    *lib.inner.hooks.fail_audio_read.lock().unwrap() = None;

    let rescan = |updated: &[TrackId]| {
        let before = scan_snapshot(&lib);
        let report = scan(&lib);
        assert_eq!(report.errors, 0, "{report:?}");
        assert_eq!(report.updated, updated.len() as u64, "{report:?}");
        let after = scan_snapshot(&lib);
        assert_eq!(after.len(), before.len());
        for (old, new) in before.iter().zip(&after) {
            assert_eq!(new.id, old.id);
            if updated.contains(&old.id) {
                assert_eq!(new.revision, old.revision + 1, "{new:?}");
            } else {
                assert_eq!(new, old);
            }
        }
    };
    let shadowed = SINGLES
        .replace("Ignored title", "Different title")
        .replace("Ignored artist", "Different artist");
    write_at(&singles, shadowed.as_bytes(), 40);
    rescan(&[]);
    let row = &lib.track_rows(&[tagged.id], &["ARTIST".into()]).unwrap()[0];
    assert_eq!(row.tags["ARTIST"], ["First Artist", "Second Artist"]);
    let names: Vec<_> = row.track.artists.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["First Artist", "Second Artist"]);

    let changed = shadowed.replace("CUE artist", "Another CUE artist");
    write_at(&singles, changed.as_bytes(), 30);
    let inner = std::sync::Arc::downgrade(&lib.inner);
    let id = untagged.id;
    *lib.inner.hooks.scan_file.lock().unwrap() = Some(std::sync::Arc::new(move |_, path| {
        if path == "untagged.wav" {
            inner
                .upgrade()
                .unwrap()
                .writer
                .run_data(move |conn| {
                    conn.execute(
                        "UPDATE tracks SET metadata_revision=metadata_revision+1 WHERE id=?1",
                        [id.0],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
    }));
    let rejected = scan(&lib);
    assert_eq!((rejected.updated, rejected.errors), (0, 0), "{rejected:?}");
    assert_eq!(
        lib.track(id).unwrap().unwrap().artist.as_deref(),
        Some("CUE artist")
    );
    *lib.inner.hooks.scan_file.lock().unwrap() = None;
    rescan(&[untagged.id]);
    let track = lib.track(untagged.id).unwrap().unwrap();
    assert_eq!(track.artist.as_deref(), Some("Another CUE artist"));
    assert_eq!(track.artists[0].name, "Another CUE artist");

    write_at(
        &image_sheet,
        IMAGE.replace("Second cut", "Updated cut").as_bytes(),
        40,
    );
    rescan(&[first.id, second.id]);
    assert_eq!(
        lib.track(second.id).unwrap().unwrap().title.as_deref(),
        Some("Updated cut")
    );
    write_wav(&image, 3);
    rescan(&[first.id, second.id]);
    assert_eq!(
        lib.track(second.id).unwrap().unwrap().duration_ms,
        Some(2000)
    );

    let removed = changed.replace("TITLE CUE title\nPERFORMER Another CUE artist\n", "");
    write_at(&singles, removed.as_bytes(), 20);
    rescan(&[untagged.id]);
    let keys = ["TITLE".into(), "ARTIST".into()];
    let row = &lib.track_rows(&[untagged.id], &keys).unwrap()[0];
    assert_eq!(
        (row.track.title.as_deref(), row.track.artist.as_deref()),
        (None, None)
    );
    assert!(row.track.artists.is_empty());
    assert!(row.tags.is_empty());
    rescan(&[]);
}

#[test]
fn gap_appended_and_prepended_files_are_independent_but_single_image_is_shared() {
    let appended = concat!(
        "FILE a.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX ",
        "00 00:01:00\nFILE b.wav WAVE\nINDEX 01 00:00:00\nTRACK 03 AUDIO\nINDEX 00 ",
        "00:01:00\nFILE c.wav WAVE\nINDEX 01 00:00:00\n",
    );
    let prepended = concat!(
        "FILE a.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nFILE b.wav ",
        "WAVE\nTRACK 02 AUDIO\nINDEX 00 00:00:00\nINDEX 01 00:00:10\nFILE c.wav ",
        "WAVE\nTRACK 03 AUDIO\nINDEX 00 00:00:00\nINDEX 01 00:00:10\n",
    );
    for text in [appended, prepended] {
        let parsed = Sheet::parse(text);
        assert_eq!(
            parsed.tracks.iter().map(|t| t.file).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!((0..3).all(|file| parsed.whole_file_tags(file).is_some()));
        let f = fixture();
        let root = f.lib.roots().unwrap()[0].path.clone();
        for name in ["a.wav", "b.wav", "c.wav"] {
            write_wav(&root.join(name), 2);
        }
        write_at(&root.join("EAC.cue"), text.as_bytes(), 50);
        scan(&f.lib);
        let tracks = visible(&f.lib);
        assert_eq!(tracks.len(), 3);
        assert!(tracks.iter().all(|t| t.cue.is_none()));
        let ids: Vec<_> = tracks.iter().map(|t| t.id).collect();
        let preview = f
            .lib
            .preview_tag_edit(
                &ids,
                &[crate::TagChange::Set {
                    key: "TITLE".into(),
                    values: vec!["editable".into()],
                }],
            )
            .unwrap();
        assert!(preview.skipped.is_empty());
        write_at(&root.join("EAC.cue"),
        b"FILE a.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:01:00\n"
        ,
        40);

        scan(&f.lib);
        assert_eq!(
            visible(&f.lib).iter().filter(|t| t.cue.is_some()).count(),
            2
        );
    }
}

#[test]
fn singleton_correction_requires_complete_matching_evidence_and_keeps_references() {
    const UPDATE_TRACKS: &str = r#"
UPDATE tracks SET cue_sheet='EAC.cue',cue_number=1,cue_start=0,cue_end=NULL,cue_size=1,cue_mtime_ms=1
WHERE id=?1
"#;

    const INSERT_TRACKS: &str = r#"
INSERT INTO tracks(root_id,rel_path,size,mtime_ms,album_key,added_at,updated_at,available,cue_sheet,cue_number,cue_start,cue_size,cue_mtime_ms)
SELECT root_id,rel_path,size,mtime_ms,album_key,added_at,updated_at,1,'EAC.cue',2,0,1,1
FROM tracks WHERE id=?1
"#;

    const INSERT_PLAY_EVENTS: &str = r#"
INSERT INTO play_events(track_id,started_at,played_ms,ended_reason,origin)
VALUES(?1,1,2000,'completed','ui')
"#;

    const SELECT_TRACKS: &str = r#"
SELECT count(*) FROM tracks WHERE root_id=?1 AND rel_path='a.wav' AND
cue_sheet IS NULL
"#;

    const SELECT_PLAYLIST_ITEMS: &str = r#"
SELECT track_id FROM playlist_items
WHERE playlist_id=?1 ORDER BY position
"#;

    for case in [
        "valid",
        "null_start",
        "nonzero_start",
        "bounded",
        "different_sheet",
        "different_number",
        "missing_index",
        "nonzero_index",
        "multiple_old",
    ] {
        let f = fixture();
        let root = f.lib.roots().unwrap()[0].path.clone();
        write_wav(&root.join("a.wav"), 2);
        write_at(
            &root.join("EAC.cue"),
            b"FILE a.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
            50,
        );
        scan(&f.lib);
        let old = visible(&f.lib)[0].id;
        let playlist = f
            .lib
            .create_playlist("Keep references", crate::PlaylistKind::Manual, UI)
            .unwrap()
            .value;
        f.lib
            .add_to_playlist(playlist, &[old, old], None, None, UI)
            .unwrap();
        let rating = f.lib.set_rating(&[old], Some(5), UI).unwrap();
        f.lib
            .inner
            .writer
            .run_data(move |conn| {
                conn.execute(UPDATE_TRACKS, [old.0])?;
                match case {
                    "null_start" => {
                        conn.execute("UPDATE tracks SET cue_start=NULL WHERE id=?1", [old.0])?;
                    }
                    "nonzero_start" => {
                        conn.execute("UPDATE tracks SET cue_start=10 WHERE id=?1", [old.0])?;
                    }
                    "bounded" => {
                        conn.execute("UPDATE tracks SET cue_end=75 WHERE id=?1", [old.0])?;
                    }
                    "different_sheet" => {
                        conn.execute(
                            "UPDATE tracks SET cue_sheet='Other.cue' WHERE id=?1",
                            [old.0],
                        )?;
                    }
                    "different_number" => {
                        conn.execute("UPDATE tracks SET cue_number=2 WHERE id=?1", [old.0])?;
                    }
                    "multiple_old" => {
                        conn.execute(INSERT_TRACKS, [old.0])?;
                    }
                    _ => {}
                }
                conn.execute(INSERT_PLAY_EVENTS, [old.0])?;
                Ok(())
            })
            .unwrap();
        if case == "missing_index" {
            write_at(
                &root.join("EAC.cue"),
                b"FILE a.wav WAVE\nTRACK 01 AUDIO\n",
                51,
            );
        }
        if case == "nonzero_index" {
            write_at(
                &root.join("EAC.cue"),
                b"FILE a.wav WAVE\nTRACK 01 AUDIO\nINDEX 00 00:00:00\nINDEX 01 00:00:10\n",
                51,
            );
        }
        let before = f.lib.track(old).unwrap().unwrap();
        scan(&f.lib);
        let current = f.lib.track(old).unwrap().unwrap();
        let reusable = matches!(case, "valid" | "null_start");
        assert_eq!(current.cue.is_none(), reusable, "{case}");
        assert!(current.available, "{case}");
        if !reusable {
            assert_eq!(current.cue, before.cue, "{case}");
        }
        f.lib
            .inner
            .pool
            .read(|conn| {
                assert_eq!(
                    conn.query_row(SELECT_TRACKS, [f.lib.roots().unwrap()[0].id.0], |r| r
                        .get::<_, i64>(0))?,
                    i64::from(reusable),
                    "{case}"
                );
                assert_eq!(
                    conn.query_row("SELECT track_id FROM play_events LIMIT 1", [], |r| r
                        .get::<_, i64>(0))?,
                    old.0
                );
                let refs: Vec<i64> = conn
                    .prepare(SELECT_PLAYLIST_ITEMS)?
                    .query_map([playlist.0], |r| r.get(0))?
                    .collect::<std::result::Result<_, _>>()?;
                assert_eq!(refs, [old.0, old.0]);
                Ok(())
            })
            .unwrap();
        assert_eq!(f.lib.track_rows(&[old], &[]).unwrap()[0].rating, Some(5));
        f.lib.undo_action(rating.action.unwrap(), UI).unwrap();
        assert_eq!(f.lib.track_rows(&[old], &[]).unwrap()[0].rating, None);
    }
}

#[test]
fn already_split_test_data_and_unused_alias_metadata_are_not_migrated() {
    const INSERT_TRACKS: &str = r#"
INSERT INTO tracks(root_id,rel_path,size,mtime_ms,album_key,added_at,updated_at,available,cue_sheet,cue_number,cue_start,cue_size,cue_mtime_ms)
SELECT root_id,rel_path,size,mtime_ms,album_key,added_at,updated_at,0,'EAC.cue',1,0,1,1
FROM tracks WHERE id=?1
"#;

    let f = fixture();
    let root = f.lib.roots().unwrap()[0].path.clone();
    write_wav(&root.join("a.wav"), 2);
    write_at(
        &root.join("EAC.cue"),
        b"FILE a.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        50,
    );
    scan(&f.lib);
    let independent = visible(&f.lib)[0].id;
    let old = f
        .lib
        .inner
        .writer
        .run_data(move |conn| {
            conn.execute(INSERT_TRACKS, [independent.0])?;
            let old = TrackId(conn.last_insert_rowid());
            conn.execute(
                "INSERT INTO meta(key,value) VALUES(?1,?2)",
                rusqlite::params![format!("cue_alias:{}", old.0), independent.0],
            )?;
            Ok(old)
        })
        .unwrap();
    scan(&f.lib);
    assert_eq!(visible(&f.lib)[0].id, independent);
    let reopened = Library::open(&f._dir.path().join("db/library.sqlite")).unwrap();
    let retired = reopened.track(old).unwrap().unwrap();
    assert!(!retired.available);
    assert!(retired.cue.is_some());
    reopened.set_rating(&[old], Some(4), UI).unwrap();
    assert_eq!(
        reopened.track_rows(&[independent], &[]).unwrap()[0].rating,
        None
    );
    assert_eq!(reopened.track_rows(&[old], &[]).unwrap()[0].rating, Some(4));
}

#[test]
fn an_unreadable_owning_sheet_prevents_singleton_identity_reclassification() {
    let f = fixture();
    let root = f.lib.roots().unwrap()[0].path.clone();
    write_wav(&root.join("disc.wav"), 14);
    write_at(
        &root.join("disc.cue"),
        b"FILE disc.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:02:00\n",
        50,
    );
    write_at(
        &root.join("alternate.cue"),
        b"FILE disc.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        50,
    );
    scan(&f.lib);
    let track = visible(&f.lib).pop().unwrap();
    assert!(track.cue.is_some());
    *f.lib.inner.hooks.fail_metadata.lock().unwrap() = Some(root.join("disc.cue"));
    let report = scan(&f.lib);
    assert!(report.errors > 0);
    let tracks = visible(&f.lib);
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].id, track.id);
    assert_eq!(tracks[0].cue, track.cue);
}

#[test]
fn gap_appended_rows_keep_hidden_whole_file_history() {
    const UPDATE_TRACKS: &str = r#"
UPDATE tracks SET cue_sheet='Mirror.cue',cue_number=?2,cue_start=0,cue_end=?3,cue_size=1,cue_mtime_ms=1
WHERE id=?1
"#;

    const INSERT_TRACKS: &str = r#"
INSERT INTO tracks(id,root_id,rel_path,size,mtime_ms,album_key,added_at,updated_at,available,hidden)
SELECT 20,root_id,rel_path,size,mtime_ms,album_key,added_at,updated_at,0,1
FROM tracks WHERE id=?1
"#;

    let f = fixture();
    let root = f.lib.roots().unwrap()[0].path.clone();
    let mut cue = String::from("FILE 01.wav WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n");
    for number in 1..=11 {
        std::fs::copy(flac_fixture(), root.join(format!("{number:02}.flac"))).unwrap();
    }
    std::fs::copy(flac_fixture(), root.join("ordinary.flac")).unwrap();
    for number in 2..=11 {
        cue.push_str(&format!(
            concat!(
                "TRACK {number:02} AUDIO\nINDEX 00 04:25:35\nFILE {number:02}.wav ",
                "WAVE\nINDEX 01 00:00:00\n",
            ),
            number = number,
        ));
    }
    write_at(&root.join("Mirror.cue"), cue.as_bytes(), 50);
    scan(&f.lib);
    let initial = visible(&f.lib);
    assert_eq!(initial.len(), 12);
    assert!(initial.iter().all(|track| track.cue.is_none()));
    let ids: Vec<_> = (1..=11)
        .map(|number| {
            initial
                .iter()
                .find(|track| track.rel_path == format!("{number:02}.flac"))
                .unwrap()
                .id
        })
        .collect();
    let original = ids.clone();
    f.lib
        .inner
        .writer
        .run_data(move |conn| {
            // Match v12's whole-file rows bounded by the following track's appended gap.
            for (index, id) in ids.iter().enumerate() {
                conn.execute(
                    UPDATE_TRACKS,
                    rusqlite::params![
                        id.0,
                        (index + 1) as i64,
                        (index < 10).then_some(265 * 75 + 35)
                    ],
                )?;
            }
            conn.execute(INSERT_TRACKS, [ids[0].0])?;

            Ok(())
        })
        .unwrap();
    let db_dir = f._dir;
    drop(f.lib);
    let db = db_dir.path().join("db/library.sqlite");
    let reopened = Library::open(&db).unwrap();
    scan(&reopened);
    let corrected = visible(&reopened);
    assert_eq!(corrected.len(), 12);
    assert!(corrected.iter().all(|track| track.cue.is_none()));
    for id in original {
        let track = reopened.track(id).unwrap().unwrap();
        assert!(track.available && track.cue.is_none());
    }
    reopened
        .inner
        .pool
        .read(|conn| {
            assert!(conn.query_row(
                "SELECT (available,hidden) IS (0,1) FROM tracks WHERE id=20",
                [],
                |row| row.get::<_, bool>(0)
            )?);
            assert!(conn.query_row(
                "SELECT cue_sheet IS NULL FROM tracks WHERE id=20",
                [],
                |row| row.get::<_, bool>(0)
            )?);
            Ok(())
        })
        .unwrap();
    let again = scan(&reopened);
    assert_eq!(again.updated, 0);
    assert_eq!(visible(&reopened).len(), 12);

    let before = scan_snapshot(&reopened);
    assert_eq!(before.len(), 12);
    assert!(before.iter().all(|row| !row.tag_rowids.is_empty()));
    drop(reopened);
    let reopened = Library::open(&db).unwrap();
    let report = scan(&reopened);
    assert_eq!(report.errors, 0, "{report:?}");
    assert_eq!(
        report.updated, 0,
        "reopening must not rewrite whole-file tracks"
    );
    assert_eq!(scan_snapshot(&reopened), before);
}

#[test]
fn reflection_gap_appended_whole_files_are_unchanged_after_reopen() {
    use lofty::config::WriteOptions;
    use lofty::ogg::tag::VorbisComments;
    use lofty::tag::TagExt;
    use sha2::{Digest, Sha256};

    const DIR: &str = "DJ Example - Reflection (2010) [CD FLAC]";
    const CUE: &[u8] = include_bytes!("../../tests/data/reflection.cue");
    const HASH: &str = "0be7f3aaee607433e4c944fe8dc796c647e703858ed498c6c907d0d48e970c21";
    const TRACKS: [(&str, &str); 11] = [
        ("01. Quiet Harbor feat. Nora.flac", "Quiet Harbor feat.NORA"),
        ("02. Paper Kite.flac", "Paper Kite"),
        (
            "03. Long Way Home feat. Talin.flac",
            "Long Way Home feat.TALIN",
        ),
        ("04. Carry Less feat. Mira.flac", "Carry Less feat.MIRA"),
        ("05. Rooftop.flac", "Rooftop"),
        ("06. Chasing Lights.flac", "Chasing Lights"),
        ("07. Open Sky feat. Soren.flac", "Open Sky feat.SOREN"),
        ("08. Warm Signal.flac", "Warm Signal"),
        (
            "09. All We Keep feat. Lena Rose.flac",
            "All We Keep feat.LENA ROSE",
        ),
        ("10. Still Water.flac", "Still Water"),
        ("11. Dusk Returns 2.flac", "Dusk Returns 2"),
    ];

    assert_eq!(CUE.len(), 2123);
    assert_eq!(format!("{:x}", Sha256::digest(CUE)), HASH);
    let f = fixture();
    let album = f.lib.roots().unwrap()[0].path.join(DIR);
    std::fs::create_dir(&album).unwrap();
    for (file, _) in TRACKS {
        std::fs::copy(flac_fixture(), album.join(file)).unwrap();
    }
    let tagged = TRACKS[1].0;
    let mut comments = VorbisComments::default();
    comments.push("TITLE".into(), "File title".into());
    comments.push("ARTIST".into(), "File artist".into());
    comments.push("ALBUM".into(), "File album".into());
    comments
        .save_to_path(album.join(tagged), WriteOptions::default())
        .unwrap();
    std::fs::write(album.join("DJ Example - Reflection.cue"), CUE).unwrap();
    std::fs::write(
        album.join("DJ Example - Reflection.log"),
        b"Synthetic rip log\r\n",
    )
    .unwrap();
    let playlist: String = TRACKS
        .iter()
        .map(|(file, _)| format!("{file}\r\n"))
        .collect();
    std::fs::write(album.join("DJ Example - Reflection.m3u"), playlist).unwrap();
    image::RgbImage::from_pixel(2, 2, image::Rgb([8, 16, 32]))
        .save(album.join("folder.jpg"))
        .unwrap();

    let report = scan(&f.lib);
    assert_eq!(
        (report.scanned, report.added, report.errors),
        (11, 11, 0),
        "{report:?}"
    );
    let tracks = visible(&f.lib);
    assert_eq!(tracks.len(), TRACKS.len());
    for (track, (file, cue_title)) in tracks.iter().zip(TRACKS) {
        assert_eq!(track.rel_path, format!("{DIR}/{file}"));
        assert!(track.cue.is_none() && track.has_folder_art, "{track:?}");
        let (title, artist, album) = if file == tagged {
            ("File title", "File artist", Some("File album"))
        } else {
            (cue_title, "DJ EXAMPLE", None)
        };
        assert_eq!(track.title.as_deref(), Some(title), "{file}");
        assert_eq!(track.artist.as_deref(), Some(artist), "{file}");
        assert_eq!(track.album.as_deref(), album, "{file}");
    }

    let before = scan_snapshot(&f.lib);
    assert!(before.iter().all(|row| !row.tag_rowids.is_empty()));
    let db_dir = f._dir;
    drop(f.lib);
    let reopened = Library::open(&db_dir.path().join("db/library.sqlite")).unwrap();
    let report = scan(&reopened);
    assert_eq!(
        (report.added, report.updated, report.errors),
        (0, 0, 0),
        "{report:?}"
    );
    assert_eq!(scan_snapshot(&reopened), before);
    assert_eq!(visible(&reopened), tracks);
}

#[derive(Debug, PartialEq, Eq)]
struct ScanRow {
    id: TrackId,
    updated_at: i64,
    revision: i64,
    tag_rowids: Vec<i64>,
}

fn scan_snapshot(lib: &Library) -> Vec<ScanRow> {
    lib.inner
        .pool
        .read(|conn| {
            let mut tracks = conn.prepare(
                "SELECT id,updated_at,metadata_revision FROM tracks WHERE hidden=0 ORDER BY id",
            )?;
            let mut tags =
                conn.prepare("SELECT rowid FROM track_tags WHERE track_id=?1 ORDER BY rowid")?;
            let rows = tracks.query_map([], |row| {
                let id = TrackId(row.get(0)?);
                Ok(ScanRow {
                    id,
                    updated_at: row.get(1)?,
                    revision: row.get(2)?,
                    tag_rowids: tags
                        .query_map([id.0], |row| row.get(0))?
                        .collect::<rusqlite::Result<_>>()?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .unwrap()
}
