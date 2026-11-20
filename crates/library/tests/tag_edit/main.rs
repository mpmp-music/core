//! Tag editing on real files of every format the library writes (docs/design/media-resources.md): the engine's encoded fixtures (FLAC, MP3, MP4, Ogg Vorbis,
//! Opus, WAV) and a hand-made AIFF, tagged here with lofty first (other tags, a picture).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use lofty::TextEncoding;
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::{ItemValue, Tag, TagItem};
use mpmp_library::{
    ActionMeta, Library, LibraryError, ScanStart, TagChange, TagJobOptions, TrackId, TrackQuery,
};
use mpmp_test_support::TempDir;

const UI: ActionMeta<'static> = ActionMeta::new("ui");

/// A 1×1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0x60, 0xf8, 0x5f,
    0x0f, 0x00, 0x02, 0x87, 0x01, 0x80, 0xeb, 0x47, 0xba, 0x92, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// The formats, as `(file in the library, fixture)`; `None`: the AIFF made here.
const FILES: &[(&str, Option<&str>)] = &[
    ("a.flac", Some("sine44_a.flac")),
    ("a.mp3", Some("sine44_a.mp3")),
    ("a.m4a", Some("sine44_a.m4a")),
    ("vorbis.ogg", Some("sine44_a.ogg")),
    // The library indexes Opus in `.ogg` files.
    ("opus.ogg", Some("sine44_a.opus")),
    ("a.wav", Some("sine44_a.wav")),
    ("a.aiff", None),
];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../engine/tests/fixtures")
        .join(name)
}

/// A minimal AIFF: 100 frames of 16-bit mono silence at 44.1 kHz.
fn aiff() -> Vec<u8> {
    let frames = 100u32;
    let mut comm = Vec::new();
    comm.extend(1u16.to_be_bytes());
    comm.extend(frames.to_be_bytes());
    comm.extend(16u16.to_be_bytes());
    comm.extend([0x40, 0x0e, 0xac, 0x44, 0, 0, 0, 0, 0, 0]);
    let mut ssnd = vec![0u8; 8];
    ssnd.extend(vec![0u8; frames as usize * 2]);
    let mut body = b"AIFF".to_vec();
    for (id, data) in [(b"COMM", comm), (b"SSND", ssnd)] {
        body.extend(id);
        body.extend((data.len() as u32).to_be_bytes());
        body.extend(data);
    }
    let mut out = b"FORM".to_vec();
    out.extend((body.len() as u32).to_be_bytes());
    out.extend(body);
    out
}

/// Tags a file in its primary tag with lofty: what the edits must keep.
fn pre_tag(path: &Path) {
    let mut tagged = Probe::open(path)
        .unwrap()
        .guess_file_type()
        .unwrap()
        .read()
        .unwrap();
    let primary = tagged.primary_tag_type();
    if tagged.tag(primary).is_none() {
        tagged.insert_tag(Tag::new(primary));
    }
    let tag = tagged.tag_mut(primary).unwrap();
    tag.set_title("Song".into());
    tag.set_artist("Old".into());
    tag.set_album("Album".into());
    tag.set_comment("a comment".into());
    tag.set_track(3);
    tag.set_track_total(12);
    tag.push(TagItem::new(
        ItemKey::Composer,
        ItemValue::Text("Composer".into()),
    ));
    tag.push_picture(
        Picture::unchecked(PNG.to_vec())
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Png)
            .build(),
    );
    tagged.save_to_path(path, WriteOptions::default()).unwrap();
}

fn file_hash(path: &Path) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(fs::read(path).unwrap()).into()
}

/// Pictures in the file, by lofty.
fn pictures(path: &Path) -> usize {
    let tagged = Probe::open(path)
        .unwrap()
        .guess_file_type()
        .unwrap()
        .read()
        .unwrap();
    tagged.tags().iter().map(|t| t.pictures().len()).sum()
}

struct Fixture {
    lib: Library,
    music: PathBuf,
    ids: HashMap<String, TrackId>,
    _dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let music = dir.path().join("music");
        fs::create_dir(&music).unwrap();
        for (name, source) in FILES {
            let path = music.join(name);
            match source {
                Some(source) => {
                    fs::copy(fixture(source), &path).unwrap();
                }
                None => fs::write(&path, aiff()).unwrap(),
            }
            pre_tag(&path);
        }
        let lib = Library::open(&dir.path().join("db/library.sqlite")).unwrap();
        lib.add_root(&music, UI).unwrap();
        let mut fixture = Fixture {
            lib,
            music,
            ids: HashMap::new(),
            _dir: dir,
        };
        fixture.rescan();
        assert_eq!(fixture.ids.len(), FILES.len(), "{:?}", fixture.ids);
        fixture
    }

    fn rescan(&mut self) {
        match self.lib.scan(|_| {}).unwrap() {
            ScanStart::Started(handle) => {
                handle.wait().unwrap();
            }
            ScanStart::Coalesced(_) => panic!("a scan was running"),
        }
        let ids = self.lib.track_ids(&TrackQuery::default(), 1000).unwrap();
        self.ids = self
            .lib
            .tracks(&ids)
            .unwrap()
            .into_iter()
            .map(|t| (t.rel_path, t.id))
            .collect();
    }

    fn id(&self, name: &str) -> TrackId {
        self.ids[name]
    }

    fn all(&self) -> Vec<TrackId> {
        FILES.iter().map(|(name, _)| self.id(name)).collect()
    }

    /// The tags the library shows of a track.
    fn tags(&self, id: TrackId, keys: &[&str]) -> BTreeMap<String, Vec<String>> {
        let keys: Vec<String> = keys.iter().map(|k| (*k).to_owned()).collect();
        self.lib.track_rows(&[id], &keys).unwrap()[0].tags.clone()
    }
}

fn set(key: &str, values: &[&str]) -> TagChange {
    TagChange::Set {
        key: key.into(),
        values: values.iter().map(|v| (*v).to_owned()).collect(),
    }
}

fn remove(key: &str) -> TagChange {
    TagChange::Remove { key: key.into() }
}

fn values(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
    pairs
        .iter()
        .map(|(k, vs)| {
            (
                (*k).to_owned(),
                vs.iter().map(|v| (*v).to_owned()).collect(),
            )
        })
        .collect()
}

const KEYS: &[&str] = &[
    "TITLE",
    "ARTIST",
    "ALBUM",
    "COMMENT",
    "COMPOSER",
    "TRACKNUMBER",
    "TRACKTOTAL",
    "MOOD_OF_THE_DAY",
];

/// Rewrites the ID3v2 tag of the fixture's `a.mp3` as version 2.3 with `frames` only.
fn mp3_frames(f: &mut Fixture, frames: Vec<lofty::id3::v2::Frame<'static>>) {
    use lofty::id3::v2::Id3v2Tag;
    let path = f.music.join("a.mp3");
    let mut tag = Id3v2Tag::new();
    for frame in frames {
        tag.insert(frame);
    }
    let v23 = !(&tag).into_iter().any(|frame| frame.id_str() == "TMCL");
    tag.save_to_path(&path, WriteOptions::new().use_id3v23(v23))
        .unwrap();
    f.rescan();
}

fn text_frame(
    id: &'static str,
    encoding: TextEncoding,
    value: String,
) -> lofty::id3::v2::Frame<'static> {
    use lofty::id3::v2::{Frame, FrameId, TextInformationFrame};
    Frame::Text(TextInformationFrame::new(
        FrameId::Valid(std::borrow::Cow::Borrowed(id)),
        encoding,
        value,
    ))
}

/// GBK bytes stored as Latin-1, as old Chinese taggers wrote them.
fn gbk_latin1(text: &str) -> String {
    let (gbk, _, _) = encoding_rs::GBK.encode(text);
    gbk.iter().map(|&b| b as char).collect()
}

mod undo;

mod edits;

mod encoding;
