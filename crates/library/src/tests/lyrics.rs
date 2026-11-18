//! `Library::lyrics` on real files: a WAV carrying an ID3v2 tag, and `.lrc` files beside it.

use std::fs;
use std::path::PathBuf;

use lofty::config::WriteOptions;
use lofty::tag::{ItemKey, Tag, TagExt, TagType};

use crate::lyrics::find::MAX_LYRICS_BYTES;
use crate::lyrics::{LyricsFormat, LyricsSource};
use crate::tests::{Fixture, fixture, id_of, meta, record, upsert};
use crate::{Library, TrackId};

/// A silent 8 kHz mono WAV of 100 samples.
fn wav() -> Vec<u8> {
    let data = 200u32;
    let mut out = b"RIFF".to_vec();
    out.extend((36 + data).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes()); // PCM
    out.extend(1u16.to_le_bytes()); // mono
    out.extend(8000u32.to_le_bytes());
    out.extend(16000u32.to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data.to_le_bytes());
    out.extend(vec![0; data as usize]);
    out
}

fn music_dir(lib: &Library) -> PathBuf {
    lib.roots().unwrap()[0].path.clone()
}

/// Writes `01 Song.wav` into the fixture's folder, with `lyrics` in its tag if given, and
/// returns its track.
fn track(f: &Fixture, lyrics: Option<&str>) -> TrackId {
    let path = music_dir(&f.lib).join("01 Song.wav");
    fs::write(&path, wav()).unwrap();
    if let Some(text) = lyrics {
        let mut tag = Tag::new(TagType::Id3v2);
        assert!(tag.insert_text(ItemKey::UnsyncLyrics, text.to_owned()));
        tag.save_to_path(&path, WriteOptions::default()).unwrap();
    }
    upsert(
        &f.lib,
        vec![record(f.root, "01 Song.wav", meta("Song", "A", "B"))],
    );
    id_of(&f.lib, "Song")
}

#[test]
fn embedded_lyrics() {
    let f = fixture();
    let id = track(&f, Some("[00:01.00]Hello\n[00:02.00]World"));
    let lyrics = f.lib.lyrics(id).unwrap().unwrap();
    assert_eq!(lyrics.source, LyricsSource::Embedded);
    assert_eq!(lyrics.format, LyricsFormat::Lrc);
    assert_eq!(lyrics.lines.len(), 2);
    assert_eq!(lyrics.lines[1].start_ms, Some(2000));
    assert!(lyrics.raw.starts_with("[00:01.00]Hello"));
}

#[test]
fn plain_embedded_lyrics() {
    let f = fixture();
    let id = track(&f, Some("la la\nla"));
    let lyrics = f.lib.lyrics(id).unwrap().unwrap();
    assert_eq!(lyrics.format, LyricsFormat::Plain);
    assert_eq!(lyrics.lines[0].start_ms, None);
}

#[test]
fn no_lyrics_and_unknown_tracks() {
    let f = fixture();
    let id = track(&f, None);
    assert_eq!(f.lib.lyrics(id).unwrap(), None);
    assert_eq!(f.lib.lyrics(TrackId(i64::MAX)).unwrap(), None);
    // A file that is not audio at all, and one that is gone: no lyrics, no error.
    fs::write(music_dir(&f.lib).join("01 Song.wav"), b"garbage").unwrap();
    assert_eq!(f.lib.lyrics(id).unwrap(), None);
    fs::remove_file(music_dir(&f.lib).join("01 Song.wav")).unwrap();
    assert_eq!(f.lib.lyrics(id).unwrap(), None);
}

#[test]
fn a_lrc_file_wins_over_the_tag() {
    let f = fixture();
    let id = track(&f, Some("[00:01.00]from the tag"));
    let sidecar = music_dir(&f.lib).join("01 Song.LRC");
    fs::write(&sidecar, "[00:05.00]from the file\n[00:05.00]译文").unwrap();
    let lyrics = f.lib.lyrics(id).unwrap().unwrap();
    assert_eq!(lyrics.source, LyricsSource::File);
    assert_eq!(lyrics.lines.len(), 1);
    assert_eq!(lyrics.lines[0].text, "from the file");
    assert_eq!(lyrics.lines[0].translation.as_deref(), Some("译文"));
    // Without the file, the tag again.
    fs::remove_file(&sidecar).unwrap();
    let lyrics = f.lib.lyrics(id).unwrap().unwrap();
    assert_eq!(lyrics.source, LyricsSource::Embedded);
}

#[test]
fn a_lrc_file_over_the_limit_is_ignored() {
    let f = fixture();
    let id = track(&f, Some("[00:01.00]from the tag"));
    let sidecar = music_dir(&f.lib).join("01 Song.lrc");
    let mut text = "[00:05.00]x\n".repeat(MAX_LYRICS_BYTES / 12 + 1);
    assert!(text.len() > MAX_LYRICS_BYTES);
    fs::write(&sidecar, &text).unwrap();
    let lyrics = f.lib.lyrics(id).unwrap().unwrap();
    assert_eq!(lyrics.source, LyricsSource::Embedded);
    // Cut to the limit, it is read.
    text.truncate(MAX_LYRICS_BYTES);
    fs::write(&sidecar, &text).unwrap();
    let lyrics = f.lib.lyrics(id).unwrap().unwrap();
    assert_eq!(lyrics.source, LyricsSource::File);
}
