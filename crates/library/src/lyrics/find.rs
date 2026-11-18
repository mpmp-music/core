//! Finding a track's lyrics (docs/design/media-resources.md): a `.lrc` file with
//! the track's name next to it, else the lyrics in its tags. Reads from disk on the
//! caller's thread; parsing is [`crate::lyrics`].

use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use lofty::config::ParseOptions;
use lofty::file::TaggedFileExt;
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag};

use crate::lyrics::{Lyrics, LyricsSource, parse};
use crate::storage_access;
use crate::tags::encoding::decode_text_file;

/// The most text of lyrics read; more is taken for not being lyrics.
pub(crate) const MAX_LYRICS_BYTES: usize = 1 << 20;

/// The lyrics of the audio file at `path`: the first of its sidecar files that holds some,
/// else those in its tags. `None` if there are none (a file that is too big, unreadable,
/// or has no lines to show is skipped).
pub(crate) fn read_lyrics(
    storage: &dyn mpmp_storage::LibraryStorage,
    path: &Path,
) -> io::Result<Option<Lyrics>> {
    read_lyrics_with_storage(storage, path, embedded_texts)
}

#[cfg(all(test, feature = "desktop-fs"))]
fn read_lyrics_with(
    path: &Path,
    embedded: impl Fn(&Path) -> Embedded,
) -> io::Result<Option<Lyrics>> {
    read_lyrics_with_storage(&mpmp_fs_desktop::DesktopStorage, path, embedded)
}

type Embedded = Result<Vec<String>, Box<dyn std::error::Error>>;

fn read_lyrics_with_storage(
    storage: &dyn mpmp_storage::LibraryStorage,
    path: &Path,
    embedded: impl Fn(&Path) -> Embedded,
) -> io::Result<Option<Lyrics>> {
    for sidecar in sidecars(path)? {
        match read_text(storage, &sidecar) {
            Ok(Some(text)) => {
                if let Some(lyrics) = parse(&text, LyricsSource::File) {
                    return Ok(Some(lyrics));
                }
            }
            Ok(None) => tracing::debug!(path = %sidecar.display(), "lyrics file too big"),
            Err(err) => tracing::debug!(%err, path = %sidecar.display(), "cannot read lyrics file"),
        }
    }
    // Tag parsers see arbitrary bytes; a panic must not reach the caller (as in scanning).
    let texts = panic::catch_unwind(AssertUnwindSafe(|| embedded(path)))
        .unwrap_or_else(|_| Err("tag reader panicked".into()));
    match texts {
        Ok(texts) => Ok(texts
            .iter()
            .find_map(|text| parse(text, LyricsSource::Embedded))),
        Err(err) => {
            tracing::debug!(%err, path = %path.display(), "no embedded lyrics");
            Ok(None)
        }
    }
}

/// The `.lrc` files next to `track` with its name (`01 Song.flac`: `01 Song.lrc`), best
/// first: the extension in exactly that case, then the others (`.LRC`,...) by name.
fn sidecars(track: &Path) -> io::Result<Vec<PathBuf>> {
    let (Some(dir), Some(stem)) = (track.parent(), track.file_stem()) else {
        return Ok(Vec::new());
    };
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut found = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        let candidate = Path::new(&name);
        let ext = candidate.extension().and_then(|e| e.to_str());
        if candidate.file_stem() == Some(stem) && ext.is_some_and(|e| e.eq_ignore_ascii_case("lrc"))
        {
            found.push((ext != Some("lrc"), name));
        }
    }
    found.sort();
    Ok(found.into_iter().map(|(_, name)| dir.join(name)).collect())
}

/// The decoded text of a lyrics file; `None` if it is over [`MAX_LYRICS_BYTES`]. Only a
/// regular file is read.
fn read_text(
    storage: &dyn mpmp_storage::LibraryStorage,
    path: &Path,
) -> crate::Result<Option<String>> {
    let mut bytes = Vec::new();
    storage_access::open_regular(storage, path, false)?
        .take(MAX_LYRICS_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_LYRICS_BYTES {
        return Ok(None);
    }
    Ok(Some(decode_text_file(&bytes, false)))
}

fn embedded_texts(path: &Path) -> Embedded {
    let reader = BufReader::new(File::open(path)?);
    let tagged = Probe::new(reader)
        .options(ParseOptions::new().read_properties(false))
        .guess_file_type()?
        .read()?;
    Ok(tag_texts(tagged.tags()))
}

/// The lyrics in `tags` that are not too big: all the `Lyrics` items (`©lyr`, `LYRICS`),
/// then all the `UnsyncLyrics` ones (`USLT`).
fn tag_texts(tags: &[Tag]) -> Vec<String> {
    [ItemKey::Lyrics, ItemKey::UnsyncLyrics]
        .into_iter()
        .flat_map(|key| tags.iter().filter_map(move |tag| tag.get_string(key)))
        .filter(|text| !text.trim().is_empty() && text.len() <= MAX_LYRICS_BYTES)
        .map(str::to_owned)
        .collect()
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::*;
    use crate::lyrics::LyricsFormat;
    use lofty::tag::{ItemValue, TagItem, TagType};

    fn tag_with(kind: TagType, items: &[(ItemKey, &str)]) -> Tag {
        let mut tag = Tag::new(kind);
        for (key, text) in items {
            tag.push(TagItem::new(*key, ItemValue::Text((*text).to_owned())));
        }
        tag
    }

    fn none(_: &Path) -> Embedded {
        Ok(Vec::new())
    }

    fn lyrics_in(dir: &Path, embedded: &'static str) -> Option<Lyrics> {
        read_lyrics_with(&dir.join("01 Song.flac"), move |_| {
            Ok(vec![embedded.to_owned()])
        })
        .unwrap()
    }

    #[test]
    fn tag_texts_prefer_lyrics_over_unsynced() {
        let tags = [
            tag_with(
                TagType::Id3v2,
                &[(ItemKey::UnsyncLyrics, "[00:01.00]unsynced")],
            ),
            tag_with(
                TagType::VorbisComments,
                &[
                    (ItemKey::UnsyncLyrics, "second"),
                    (ItemKey::Lyrics, "first"),
                ],
            ),
        ];
        assert_eq!(tag_texts(&tags), ["first", "[00:01.00]unsynced", "second"]);
        // Blank and oversized texts are not lyrics.
        let big = "x".repeat(MAX_LYRICS_BYTES + 1);
        let tags = [tag_with(
            TagType::VorbisComments,
            &[(ItemKey::Lyrics, "  \n"), (ItemKey::UnsyncLyrics, &big)],
        )];
        assert!(tag_texts(&tags).is_empty());
    }

    #[test]
    fn embedded_lyrics_are_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let lyrics = lyrics_in(dir.path(), "[00:01.00]hi").unwrap();
        assert_eq!(lyrics.source, LyricsSource::Embedded);
        assert_eq!(lyrics.format, LyricsFormat::Lrc);
        assert_eq!(lyrics_in(dir.path(), "  ").map(|l| l.raw), None);
    }

    #[test]
    fn sidecar_wins_over_embedded() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("01 Song.lrc"), "[00:01.00]from file").unwrap();
        let lyrics = lyrics_in(dir.path(), "from tags").unwrap();
        assert_eq!(lyrics.source, LyricsSource::File);
        assert_eq!(lyrics.lines[0].text, "from file");
    }

    #[test]
    fn sidecar_name_and_extension_case() {
        let dir = tempfile::tempdir().unwrap();
        let find = || {
            read_lyrics_with(&dir.path().join("01 Song.flac"), none)
                .unwrap()
                .map(|l| l.lines[0].text.clone())
        };
        assert_eq!(find(), None);
        // Another name, or another extension, is not it.
        fs::write(dir.path().join("02 Song.lrc"), "no").unwrap();
        fs::write(dir.path().join("01 Song.flac.lrc"), "no").unwrap();
        fs::write(dir.path().join("01 Song.txt"), "no").unwrap();
        assert_eq!(find(), None);
        // Probe the directory, not the OS: NTFS may be case sensitive and macOS may
        // be either. Every spelling must work even when they cannot coexist.
        let upper = dir.path().join("01 Song.LRC");
        let mixed = dir.path().join("01 Song.Lrc");
        fs::write(&upper, "upper").unwrap();
        let distinct_case = !mixed.exists();
        assert_eq!(find().as_deref(), Some("upper"));
        fs::remove_file(&upper).unwrap();
        for (extension, content) in [("Lrc", "mixed"), ("lrc", "lower")] {
            let path = dir.path().join(format!("01 Song.{extension}"));
            fs::write(&path, content).unwrap();
            assert_eq!(find().as_deref(), Some(content));
            fs::remove_file(path).unwrap();
        }
        if distinct_case {
            // Exact `.lrc` wins, then lexicographic name (`.LRC` < `.Lrc`).
            fs::write(&upper, "upper").unwrap();
            fs::write(&mixed, "mixed").unwrap();
            assert_eq!(find().as_deref(), Some("upper"));
            fs::write(dir.path().join("01 Song.lrc"), "lower").unwrap();
            assert_eq!(find().as_deref(), Some("lower"));
        }
    }

    #[test]
    fn sidecar_encodings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("01 Song.lrc");
        let text = "[00:01.00]你好，世界\n[00:02.00]晶港\n";
        let find = || {
            read_lyrics_with(&dir.path().join("01 Song.flac"), none)
                .unwrap()
                .unwrap()
        };
        let (gbk, _, bad) = encoding_rs::GBK.encode(text);
        assert!(!bad);
        fs::write(&path, &*gbk).unwrap();
        let lyrics = find();
        assert_eq!(lyrics.raw, text);
        assert_eq!(lyrics.lines[0].text, "你好，世界");
        // UTF-8 with a byte-order mark.
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend_from_slice(text.as_bytes());
        fs::write(&path, &bytes).unwrap();
        let lyrics = find();
        assert_eq!(lyrics.raw, text);
        assert_eq!(lyrics.lines[0].start_ms, Some(1000));
        // UTF-16 with a byte-order mark.
        let mut bytes = b"\xFF\xFE".to_vec();
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        fs::write(&path, &bytes).unwrap();
        assert_eq!(find().raw, text);
    }

    #[test]
    fn size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("01 Song.lrc");
        let find = || {
            read_lyrics_with(&dir.path().join("01 Song.flac"), |_| {
                Ok(vec!["from tags".to_owned()])
            })
            .unwrap()
            .unwrap()
        };
        // Exactly the limit is read.
        let line = "[00:01.00]lyrics\n";
        let mut text = line.repeat(MAX_LYRICS_BYTES / line.len());
        text.push_str(&"\n".repeat(MAX_LYRICS_BYTES - text.len()));
        assert_eq!(text.len(), MAX_LYRICS_BYTES);
        fs::write(&path, &text).unwrap();
        assert_eq!(find().source, LyricsSource::File);
        // One byte more is no lyrics file: the tags are looked at.
        text.push('\n');
        fs::write(&path, &text).unwrap();
        assert_eq!(find().source, LyricsSource::Embedded);
        // No tags either: none.
        let none = read_lyrics_with(&dir.path().join("01 Song.flac"), none).unwrap();
        assert_eq!(none, None);
    }

    #[test]
    fn unusable_sidecars_fall_back_to_tags() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("01 Song.lrc");
        // Empty, or only metadata: nothing to show.
        fs::write(&path, "[ar:A]\n[ti:T]\n").unwrap();
        let lyrics = lyrics_in(dir.path(), "from tags").unwrap();
        assert_eq!(lyrics.source, LyricsSource::Embedded);
        // Not a regular file.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let lyrics = lyrics_in(dir.path(), "from tags").unwrap();
        assert_eq!(lyrics.source, LyricsSource::Embedded);
    }

    #[test]
    fn a_panicking_tag_reader_is_no_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        let lyrics = read_lyrics_with(&dir.path().join("a.flac"), |_| panic!("tags")).unwrap();
        assert_eq!(lyrics, None);
        // Nor does a missing folder fail.
        assert_eq!(
            read_lyrics(
                &mpmp_fs_desktop::DesktopStorage,
                &dir.path().join("gone/a.flac")
            )
            .unwrap(),
            None
        );
    }
}
