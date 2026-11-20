//! Cover art lookup: the embedded front cover, else an image file next to the track.
//! Reads from disk on the caller's thread and returns the image as stored; thumbnails of it
//! (decoded, shrunk, cached on disk) are made by `thumbnail`.

pub(crate) mod edit;
pub(crate) mod job;
pub(crate) mod thumbnail;

use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use lofty::config::ParseOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::{Picture, PictureType};
use lofty::probe::Probe;
use serde::{Deserialize, Serialize};

/// An image as stored: no decoding or resizing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverArt {
    /// `image/jpeg`, `image/png`,...; `application/octet-stream` if unknown.
    pub mime: String,
    pub data: Vec<u8>,
}

/// Base names of cover image files, in order of preference.
const COVER_NAMES: [&str; 3] = ["cover", "folder", "front"];

/// Input budgets apply before allocating image bytes, including legacy UI callers.
pub(crate) const MAX_COVER_BYTES: u64 = 16 * 1024 * 1024;

const MAX_TAG_READ_BYTES: u64 = 32 * 1024 * 1024;

pub(crate) fn bounded_image(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_COVER_BYTES {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(MAX_COVER_BYTES + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= MAX_COVER_BYTES).then_some(bytes))
}

// Seeking through a large audio payload is allowed; re-reading consumes the same
// cumulative budget, so parsers cannot evade it by seeking back or trusting headers.
struct TagInput<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for TagInput<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(io::Error::other("cover tag input budget exceeded"));
        }
        let limit = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..limit])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

impl<R: Seek> Seek for TagInput<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.inner.seek(from)
    }
}

const COVER_EXTENSIONS: [(&str, &str); 3] = [
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("png", "image/png"),
];

/// Preference of `file_name` as a folder cover (lower is better), `None` if it is not one.
/// Case-insensitive: `Cover.JPG` and `folder.png` both count.
pub(crate) fn cover_rank(file_name: &str) -> Option<usize> {
    let (stem, ext) = file_name.rsplit_once('.')?;
    let name = COVER_NAMES
        .iter()
        .position(|n| stem.eq_ignore_ascii_case(n))?;
    let ext = COVER_EXTENSIONS
        .iter()
        .position(|(e, _)| ext.eq_ignore_ascii_case(e))?;
    Some(name * COVER_EXTENSIONS.len() + ext)
}

fn mime_for_extension(file_name: &str) -> &'static str {
    let ext = file_name.rsplit_once('.').map_or("", |(_, ext)| ext);
    COVER_EXTENSIONS
        .iter()
        .find(|(e, _)| ext.eq_ignore_ascii_case(e))
        .map_or("application/octet-stream", |(_, mime)| mime)
}

pub(crate) fn sniff_mime(data: &[u8]) -> &'static str {
    match data {
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "image/webp",
        [b'B', b'M', ..] => "image/bmp",
        _ => "application/octet-stream",
    }
}

// Tags and file extensions are hints; byte signatures take precedence on every read.
fn actual_mime<'a>(data: &[u8], declared: &'a str) -> &'a str {
    match sniff_mime(data) {
        "application/octet-stream" => declared,
        mime => mime,
    }
}

/// Cover for the audio file at `path`: embedded front cover (else the first embedded
/// picture), else the best-ranked cover image file in the same folder.
pub(crate) fn read_cover(path: &Path) -> io::Result<Option<CoverArt>> {
    read_cover_with(path, embedded_cover)
}

type Embedded = Result<Option<CoverArt>, Box<dyn std::error::Error>>;

fn read_cover_with(
    path: &Path,
    embedded: impl Fn(&Path) -> Embedded,
) -> io::Result<Option<CoverArt>> {
    // Tag parsers see arbitrary bytes; a panic must not reach the caller (as in scanning).
    let result = panic::catch_unwind(AssertUnwindSafe(|| embedded(path)))
        .unwrap_or_else(|_| Err("tag reader panicked".into()));
    match result {
        Ok(Some(cover)) => return Ok(Some(cover)),
        Ok(None) => {}
        Err(err) => tracing::debug!(%err, path = %path.display(), "no embedded cover"),
    }
    match path.parent() {
        Some(dir) => folder_cover(dir),
        None => Ok(None),
    }
}

pub(crate) fn embedded_cover(path: &Path) -> Embedded {
    embedded_selected(path, false, false)
}

pub(crate) fn fallback_cover(path: &Path, all_covr: bool) -> Embedded {
    match embedded_selected(path, true, all_covr)? {
        Some(art) => Ok(Some(art)),
        None => Ok(path.parent().map(folder_cover).transpose()?.flatten()),
    }
}

fn embedded_selected(path: &Path, remove_front: bool, all_covr: bool) -> Embedded {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    let reader = BufReader::new(TagInput {
        inner: file,
        remaining: MAX_TAG_READ_BYTES,
    });
    let tagged = Probe::new(reader)
        .options(ParseOptions::new().read_properties(false))
        .guess_file_type()?
        .read()?;
    let pictures: Vec<&Picture> = tagged
        .tags()
        .iter()
        .flat_map(|t| t.pictures())
        .filter(|p| !remove_front || !all_covr && p.pic_type() != PictureType::CoverFront)
        .collect();
    let picture = pictures
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.first());
    Ok(picture
        .filter(|p| p.data().len() as u64 <= MAX_COVER_BYTES)
        .map(|picture| {
            let data = picture.data().to_vec();
            let declared = picture
                .mime_type()
                .map_or("application/octet-stream", |mime| mime.as_str());
            let mime = actual_mime(&data, declared).to_owned();
            CoverArt { mime, data }
        }))
}

/// The best-ranked cover image file in `dir`; `None` if there is none or `dir` is gone.
pub(crate) fn folder_cover(dir: &Path) -> io::Result<Option<CoverArt>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut best: Option<(usize, String)> = None;
    for entry in entries {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if let Some(rank) = cover_rank(&name)
            && best.as_ref().is_none_or(|(b, _)| rank < *b)
            && entry.file_type()?.is_file()
        {
            best = Some((rank, name));
        }
    }
    let Some((_, name)) = best else {
        return Ok(None);
    };
    let Some(data) = bounded_image(&dir.join(&name))? else {
        return Ok(None);
    };
    Ok(Some(CoverArt {
        mime: actual_mime(&data, mime_for_extension(&name)).to_owned(),
        data,
    }))
}

/// All embedded pictures, including non-front and unknown roles, protected by tag undo.
pub(crate) fn picture_guard(path: &Path) -> std::io::Result<[u8; 32]> {
    picture_guard_open(File::open(path)?)
}

pub(crate) fn picture_guard_open(mut file: File) -> std::io::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    file.rewind()?;
    let input = BufReader::new(TagInput {
        inner: file,
        remaining: MAX_TAG_READ_BYTES,
    });
    let tagged = Probe::new(input)
        .options(ParseOptions::new().read_properties(false))
        .guess_file_type()?
        .read()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let mut h = Sha256::new();
    for p in tagged.tags().iter().flat_map(|t| t.pictures()) {
        h.update(
            format!(
                "{:?}:{:?}:{:?}:",
                p.pic_type(),
                p.mime_type(),
                p.description()
            )
            .as_bytes(),
        );
        h.update((p.data().len() as u64).to_le_bytes());
        h.update(p.data());
    }
    Ok(h.finalize().into())
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::*;

    #[test]
    fn oversized_images_and_repeated_tag_reads_are_bounded() {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("cover.jpg");
        File::create(&path)
            .unwrap()
            .set_len(MAX_COVER_BYTES + 1)
            .unwrap();
        assert_eq!(folder_cover(dir.path()).unwrap(), None);
        let mut input = TagInput {
            inner: io::Cursor::new(vec![1; 100]),
            remaining: 8,
        };
        let mut bytes = [0; 6];
        assert_eq!(input.read(&mut bytes).unwrap(), 6);
        input.rewind().unwrap();
        assert_eq!(input.read(&mut bytes).unwrap(), 2);
        assert!(input.read(&mut bytes).is_err());
        // Metadata alone cannot protect against a file growing during a read.
        let mut growing = TagInput {
            inner: io::repeat(1),
            remaining: 9,
        };
        let mut result = Vec::new();
        assert!(growing.read_to_end(&mut result).is_err());
        assert_eq!(result.len(), 9);
    }

    #[test]
    fn oversized_embedded_picture_is_absent_before_copying() {
        use std::io::Write;
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let path = dir.path().join("large.mp3");
        let picture_size = MAX_COVER_BYTES as u32 + 1;
        let tag_size = picture_size + 10;
        let mut file = File::create(&path).unwrap();
        file.write_all(b"ID3\x03\0\0").unwrap();
        file.write_all(&[
            ((tag_size >> 21) & 127) as u8,
            ((tag_size >> 14) & 127) as u8,
            ((tag_size >> 7) & 127) as u8,
            (tag_size & 127) as u8,
        ])
        .unwrap();
        file.write_all(b"APIC").unwrap();
        file.write_all(&picture_size.to_be_bytes()).unwrap();
        file.write_all(b"\0\0").unwrap();
        file.set_len(10 + tag_size as u64).unwrap();
        drop(file);
        assert_eq!(read_cover(&path).unwrap(), None);
    }

    #[test]
    fn cover_names() {
        assert_eq!(cover_rank("cover.jpg"), Some(0));
        assert!(cover_rank("Cover.PNG").is_some());
        assert!(cover_rank("FOLDER.jpeg").is_some());
        assert!(cover_rank("front.png").is_some());
        assert!(cover_rank("cover.jpg") < cover_rank("folder.jpg"));
        assert!(cover_rank("folder.jpg") < cover_rank("front.jpg"));
        assert_eq!(cover_rank("back.jpg"), None);
        assert_eq!(cover_rank("cover.gif"), None);
        assert_eq!(cover_rank("cover"), None);
        assert_eq!(mime_for_extension("x.JPEG"), "image/jpeg");
        assert_eq!(sniff_mime(b"\x89PNG\r\n"), "image/png");
    }

    #[test]
    fn folder_cover_prefers_best_name() {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        assert_eq!(folder_cover(dir.path()).unwrap(), None);
        fs::write(dir.path().join("Front.PNG"), b"front").unwrap();
        fs::write(dir.path().join("folder.jpg"), b"folder").unwrap();
        fs::write(dir.path().join("back.jpg"), b"back").unwrap();
        let cover = folder_cover(dir.path()).unwrap().unwrap();
        assert_eq!(cover.mime, "image/jpeg");
        assert_eq!(cover.data, b"folder");
        // A track file that has no tags falls back to the folder.
        let track = dir.path().join("01.flac");
        fs::write(&track, b"not audio").unwrap();
        assert_eq!(read_cover(&track).unwrap().unwrap().data, b"folder");
        assert_eq!(folder_cover(&dir.path().join("gone")).unwrap(), None);

        // A panicking tag reader falls back to the folder instead of unwinding.
        let cover = read_cover_with(&track, |_| panic!("lofty bug")).unwrap();
        assert_eq!(cover.unwrap().data, b"folder");
    }
}
