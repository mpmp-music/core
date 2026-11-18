use super::*;
use lofty::id3::v2::{CommentFrame, FrameId, KeyValueFrame, TextInformationFrame};
use std::borrow::Cow;

fn set(key: &str, values: &[&str]) -> TagChange {
    TagChange::Set {
        key: key.into(),
        values: values.iter().map(|v| (*v).to_owned()).collect(),
    }
}

/// Multi-value edits upgrade ID3v2.3; legacy Latin-1 text is converted to Unicode;
/// comments with a description (other programs' data) stay; a `TRCK` holding the
/// number and the total keeps the number when the
/// total changes.
#[test]
fn id3v2_keeps_what_the_change_does_not_name() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("a.mp3");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_a.mp3"),
        &path,
    )
    .unwrap();
    // GBK bytes stored as Latin-1, as old Chinese taggers wrote them.
    let (gbk, _, _) = encoding_rs::GBK.encode("三棱雾");
    let latin1: String = gbk.iter().map(|&b| b as char).collect();
    let mut tag = Id3v2Tag::new();
    tag.insert(Frame::Text(TextInformationFrame::new(
        FrameId::Valid(Cow::Borrowed("TIT2")),
        TextEncoding::Latin1,
        latin1,
    )));
    tag.insert(Frame::Text(TextInformationFrame::new(
        FrameId::Valid(Cow::Borrowed("TRCK")),
        TextEncoding::UTF8,
        "3/12",
    )));
    for (description, content) in [("", "plain"), ("iTunNORM", " 0000021A 0000021A")] {
        tag.insert(Frame::Comment(CommentFrame::new(
            TextEncoding::UTF8,
            *b"eng",
            description,
            content,
        )));
    }
    let mut options = WriteOptions::new();
    tag.save_to_path(&path, options.use_id3v23(true)).unwrap();
    assert_eq!(std::fs::read(&path).unwrap()[..4], *b"ID3\x03");

    let mut edited = open(&path).unwrap();
    let forced = Some(encoding_rs::GBK);
    let shown = edited.shown(forced);
    assert_eq!(shown["TITLE"], ["三棱雾"]);
    assert_eq!(shown["COMMENT"], ["plain", "0000021A 0000021A"]);
    let changes = check_changes(&[
        set("ARTIST", &["周砚舟", "Zhou"]),
        set("TRACKTOTAL", &["10"]),
        set("LYRICS", &["one", "two"]),
        TagChange::Remove {
            key: "comment".into(),
        },
    ])
    .unwrap();
    assert!(
        edited
            .apply_verified(&changes, edited.decoding(forced))
            .unwrap()
    );
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    edited.save(&mut file).unwrap();
    drop(file);
    assert_eq!(std::fs::read(&path).unwrap()[..4], *b"ID3\x04");
    let again = open(&path).unwrap();
    let shown = again.shown(forced);
    assert_eq!(shown["TITLE"], ["三棱雾"]);
    assert_eq!(shown["ARTIST"], ["周砚舟", "Zhou"]);
    assert_eq!(shown["LYRICS"], ["one", "two"]);
    assert_eq!(shown["TRACKNUMBER"], ["3"]);
    assert_eq!(shown["TRACKTOTAL"], ["10"]);
    assert_eq!(shown["COMMENT"], ["0000021A 0000021A"]);
    let Native::Id3v2(tag) = &again.primary else {
        panic!("not ID3v2");
    };
    let title = tag.into_iter().find(|f| f.id_str() == "TIT2").unwrap();
    assert!(matches!(title, Frame::Text(t) if t.encoding != TextEncoding::Latin1));
    // What an undo writes back: the primary tag's own values, whole.
    assert_eq!(
        again.primary_values(&["TRACKTOTAL", "COMPOSER"], again.decoding(forced)),
        [
            ("TRACKTOTAL".to_owned(), vec!["10".to_owned()]),
            ("COMPOSER".to_owned(), vec![])
        ]
    );
}

/// GBK bytes stored as Latin-1, as old Chinese taggers wrote them.
fn gbk_latin1(text: &str) -> String {
    let (gbk, _, _) = encoding_rs::GBK.encode(text);
    gbk.iter().map(|&b| b as char).collect()
}

/// An MP3 with an ID3v2.3 tag of `frames`, at `dir/name`.
fn mp3(dir: &Path, name: &str, frames: Vec<Frame<'static>>) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine/tests/fixtures/sine44_a.mp3"),
        &path,
    )
    .unwrap();
    let mut tag = Id3v2Tag::new();
    for frame in frames {
        tag.insert(frame);
    }
    tag.save_to_path(&path, WriteOptions::new().use_id3v23(true))
        .unwrap();
    path
}

fn text(id: &'static str, encoding: TextEncoding, value: String) -> Frame<'static> {
    Frame::Text(TextInformationFrame::new(
        FrameId::Valid(Cow::Borrowed(id)),
        encoding,
        value,
    ))
}

fn people(roles: &[(&'static str, &str)]) -> Frame<'static> {
    let pairs: Vec<(Cow<'static, str>, Cow<'static, str>)> = roles
        .iter()
        .map(|(role, name)| (Cow::Borrowed(*role), Cow::Owned(gbk_latin1(name))))
        .collect();
    Frame::KeyValue(KeyValueFrame::new(
        FrameId::Valid(Cow::Borrowed("TIPL")),
        TextEncoding::Latin1,
        pairs,
    ))
}

fn save(edited: &Editable, path: &Path) {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    edited.save(&mut file).unwrap();
}

/// Chinese GBK text in Latin-1 frames: an item
/// on its own reads as the whole file reads it (detected from the title and artist, or
/// the root's encoding), so editing one key of a frame that holds several (a `TIPL`)
/// writes the others again as the library shows them, not as Latin-1 mojibake; values
/// saved as shown change nothing; protected items read the same, so they are not
/// copied; and the edit can be undone.
#[test]
fn latin1_gbk_frames_read_as_the_whole_file_reads_them() {
    let dir = tempfile::TempDir::new().unwrap();
    let latin1 = TextEncoding::Latin1;
    // Detected: the title and artist are GBK.
    let detected = mp3(
        dir.path(),
        "detected.mp3",
        vec![
            text("TIT2", latin1, gbk_latin1("三棱雾")),
            text("TPE1", latin1, gbk_latin1("周砚舟")),
            text("TCOM", latin1, gbk_latin1("方文山")),
            people(&[("producer", "周砚舟"), ("arranger", "林砚河")]),
        ],
    );
    // Forced: nothing to detect it from (ASCII title), the root says GBK.
    let forced_file = mp3(
        dir.path(),
        "forced.mp3",
        vec![
            text("TIT2", latin1, "Qi Li Xiang".into()),
            text("TCOM", latin1, gbk_latin1("方文山")),
            people(&[("producer", "周砚舟"), ("arranger", "林砚河")]),
        ],
    );
    for (path, forced) in [(&detected, None), (&forced_file, Some(encoding_rs::GBK))] {
        let mut edited = open(path).unwrap();
        let shown = edited.shown(forced);
        assert_eq!(shown["COMPOSER"], ["方文山"], "{path:?}");
        assert_eq!(shown["PRODUCER"], ["周砚舟"], "{path:?}");
        assert_eq!(shown["ARRANGER"], ["林砚河"], "{path:?}");
        let decoding = edited.decoding(forced);
        // Saved as shown: nothing to write (the frame keeps its encoding).
        for key in ["COMPOSER", "ARRANGER"] {
            let same = check_changes(&[TagChange::Set {
                key: key.into(),
                values: shown[key].clone(),
            }])
            .unwrap();
            assert!(!edited.apply_verified(&same, decoding).unwrap(), "{key}");
        }
        let before = edited.primary_values(&["PRODUCER"], decoding);
        let changes = check_changes(&[set("PRODUCER", &["张三"])]).unwrap();
        assert!(
            edited
                .apply_verified(&changes, decoding)
                .unwrap_or_else(|e| panic!("{changes:?}: {e}; {:?}", edited.primary))
        );
        edited.undo_check(&before, forced).unwrap();
        save(&edited, path);
        let again = open(path).unwrap();
        let shown = again.shown(forced);
        assert_eq!(shown["PRODUCER"], ["张三"], "{path:?}");
        assert_eq!(shown["ARRANGER"], ["林砚河"], "{path:?}");
        assert_eq!(shown["COMPOSER"], ["方文山"], "{path:?}");
        // Undone: the producer is back, the arranger as it was.
        let mut back = open(path).unwrap();
        let decoding = back.decoding(forced);
        assert!(
            back.apply_verified(&reverse_changes(&before), decoding)
                .unwrap()
        );
        save(&back, path);
        let shown = open(path).unwrap().shown(forced);
        assert_eq!(shown["PRODUCER"], ["周砚舟"], "{path:?}");
        assert_eq!(shown["ARRANGER"], ["林砚河"], "{path:?}");
    }

    // A protected item (a `TIPL` with a role the library does not show) reads as the
    // file reads it: values saved as shown do not copy it.
    let kept = mp3(
        dir.path(),
        "kept.mp3",
        vec![
            text("TIT2", latin1, "Qi Li Xiang".into()),
            people(&[("producer", "周砚舟"), ("mastering", "某人")]),
        ],
    );
    let forced = Some(encoding_rs::GBK);
    let mut edited = open(&kept).unwrap();
    let shown = edited.shown(forced);
    assert_eq!(shown["PRODUCER"], ["周砚舟"]);
    let decoding = edited.decoding(forced);
    let same = check_changes(&[set("PRODUCER", &["周砚舟"])]).unwrap();
    assert!(!edited.apply_verified(&same, decoding).unwrap());
}

#[test]
fn unknown_people_roles_are_preserved_when_rebuilding() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = mp3(
        dir.path(),
        "native.mp3",
        vec![people(&[("mastering", "某人")])],
    );
    let mut edited = open(&path).unwrap();
    for changes in [
        vec![set("PRODUCER", &["制作人"])],
        vec![TagChange::Remove {
            key: "PRODUCER".into(),
        }],
        vec![set("PRODUCER", &["制作人"])],
    ] {
        let decoding = edited.decoding(Some(encoding_rs::GBK));
        assert!(
            edited
                .apply_verified(&changes, decoding)
                .unwrap_or_else(|e| panic!("{changes:?}: {e}; {:?}", edited.primary))
        );
    }
}

/// An edit an undo could not reverse: the file's value cannot be written back (a
/// `TRCK` another program wrote, `3/abc`, which the library shows as a total).
#[test]
fn undo_check_finds_values_that_cannot_be_written_back() {
    let dir = tempfile::TempDir::new().unwrap();
    for (name, trck, undoable) in [("a.mp3", "3/12", true), ("b.mp3", "3/abc", false)] {
        let path = mp3(
            dir.path(),
            name,
            vec![text("TRCK", TextEncoding::UTF8, trck.into())],
        );
        let mut edited = open(&path).unwrap();
        let decoding = edited.decoding(None);
        let changes = check_changes(&[TagChange::Remove {
            key: "TRACKTOTAL".into(),
        }])
        .unwrap();
        let before = edited.primary_values(&["TRACKTOTAL"], decoding);
        assert!(edited.apply_verified(&changes, decoding).unwrap(), "{trck}");
        assert_eq!(edited.undo_check(&before, None).is_ok(), undoable, "{trck}");
    }
}

/// Formats whose tags are not written are told apart from files that cannot be read.
#[test]
fn unsupported_formats() {
    let dir = tempfile::TempDir::new().unwrap();
    let mpc = dir.path().join("a.mpc");
    let mut bytes = b"MPCK".to_vec();
    bytes.extend([0u8; 64]);
    std::fs::write(&mpc, bytes).unwrap();
    assert!(matches!(open(&mpc), Err(OpenError::Unsupported)));
    let garbage = dir.path().join("a.flac");
    std::fs::write(&garbage, b"fLaC-not-really").unwrap();
    assert!(matches!(open(&garbage), Err(OpenError::Unreadable(_))));
}

#[test]
fn change_payloads_and_protected_keys_are_bounded() {
    assert!(check_changes(&[set("CUESHEET", &["FILE x WAVE"])]).is_err());
    let value = "x".repeat(MAX_TAG_VALUE_BYTES);
    let change = TagChange::Set {
        key: "LYRICS".into(),
        values: vec![value; 33],
    };
    assert!(check_changes(&[change]).is_err());
    assert_eq!(
        display_key("TITLE TO \"SAFE\"; REMOVE X"),
        r#""TITLE TO \"SAFE\"; REMOVE X""#
    );
}

#[test]
fn keys_are_the_librarys() {
    let ok = |k: &str| check_key(k).unwrap();
    assert_eq!(ok("artist"), "ARTIST");
    assert_eq!(ok("AlbumArtist"), "ALBUMARTIST");
    assert_eq!(ok("MY KEY"), "MY KEY");
    assert_eq!(ok("REPLAYGAIN_TRACK_GAIN"), "REPLAYGAIN_TRACK_GAIN");
    for bad in [
        "TPE1",
        "TALB",
        "©nam",
        "aART",
        "TOTALTRACKS",
        "rating",
        "COVR",
        " A",
        "A:B",
    ] {
        assert!(check_key(bad).is_err(), "{bad}");
    }
    let long = "K".repeat(MAX_TAG_KEY + 1);
    assert!(check_key(&long).is_err());
}
