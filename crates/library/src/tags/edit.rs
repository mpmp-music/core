//! Tag editing (docs/design/media-resources.md): a file held in memory in its own
//! format, its primary tag changed, and written back through a copy renamed into place.
//!
//! Keys are the library's (`read::all_tags`, `track_tags`): upper case; lofty's Vorbis
//! comment name for a key it knows (`ARTIST`, `ALBUMARTIST`, `TRACKNUMBER`), else the
//! format's own name (a Vorbis comment, an ID3v2 `TXXX` description or frame id, an MP4
//! freeform name or code). Only the file's primary tag is written, in its own type (FLAC,
//! Ogg Vorbis, Opus, Speex: Vorbis comments; MP3, WAV, AIFF, AAC: ID3v2, keeping version
//! 2.4 or writing 2.3 for an older one (multi-value edits upgrade to 2.4); MP4: `ilst`);
//! its other tags (ID3v1, APE, RIFF INFO,
//! AIFF text chunks, an ID3v2 tag in a FLAC file) stay as they are, so a key only they hold
//! still shows after it is removed (the preview shows that).
//!
//! A change of key K removes every item (Vorbis comment, ID3v2 frame, MP4 atom) of the
//! primary tag that the library shows under K and adds K's new values; an item that also
//! holds other keys (an ID3v2 `TRCK` with the track number and the total, an MP4 `trkn`) is
//! written again with those. Never removed: ID3v2 comments and lyrics with a description
//! (`iTunNORM`, `iTunSMPB`: other programs' data the library shows as `COMMENT`), and
//! native roles in shared people frames (a `TIPL` with roles it does not know).
//! Pictures and every item the change does not name are kept as they are (an ID3v2 frame
//! keeps its text encoding unless legacy text is converted to Unicode as part of an edit).

use std::collections::{BTreeMap, HashSet};

use std::fs::File;

use std::io::{BufReader, Cursor, Read, Seek};

use std::panic::{self, AssertUnwindSafe};

#[cfg(all(test, feature = "desktop-fs"))]
use std::path::Path;

use encoding_rs::Encoding;

use lofty::TextEncoding;

use lofty::aac::AacFile;

use lofty::config::{ParseOptions, WriteOptions};

use lofty::file::{AudioFile, FileType};

use lofty::flac::FlacFile;

use lofty::id3::v2::{ExtendedTextFrame, Frame, Id3v2Tag, Id3v2Version};

use lofty::iff::aiff::AiffFile;

use lofty::iff::wav::WavFile;

use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};

use lofty::mpeg::MpegFile;

use lofty::ogg::tag::VorbisComments;

use lofty::ogg::{OpusFile, SpeexFile, VorbisFile};

use lofty::probe::Probe;

use lofty::tag::{ItemKey, ItemValue, SplitTag, Tag, TagExt, TagItem, TagType};

use crate::tags::read::{self, RawTags, key_name, raw_id3v2, raw_ilst, raw_vorbis};

use crate::text::normalize;

pub(super) mod delta;
mod ogg;
pub(super) mod plan;
pub(super) mod raw;
pub(crate) mod snapshot;

/// A tag's values by key, each key's values in the file's order.
pub type TagValues = BTreeMap<String, Vec<String>>;

/// One change to the tags of a file: set a key's values (replacing what it has), or remove
/// the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagChange {
    Set { key: String, values: Vec<String> },
    Remove { key: String },
}

impl TagChange {
    pub fn key(&self) -> &str {
        match self {
            TagChange::Set { key, .. } | TagChange::Remove { key } => key,
        }
    }

    /// The values the key has after the change (none: removed).
    fn values(&self) -> &[String] {
        match self {
            TagChange::Set { values, .. } => values,
            TagChange::Remove { .. } => &[],
        }
    }
}

/// Most changes in one edit.
pub const MAX_TAG_CHANGES: usize = 64;

/// Most values one change sets.
pub const MAX_TAG_VALUES: usize = 64;

/// Longest key, in characters.
pub const MAX_TAG_KEY: usize = 64;

/// Longest value, in bytes of UTF-8: the longest the library keeps
/// ([`crate::Track`] tags are cut there).
pub const MAX_TAG_VALUE_BYTES: usize = read::MAX_TAG_VALUE;

/// Most tracks one edit (or preview) names.
pub const MAX_TAG_EDIT_TRACKS: usize = 1000;

/// Most UTF-8 bytes of keys and values in one edit.
pub const MAX_TAG_EDIT_BYTES: usize = 256 * 1024;

/// Per-file limit on literal delta data and its accounting overhead.
pub(crate) const MAX_TAG_UNDO_BYTES: usize = 8 * 1024 * 1024;
/// Total per-file charges for one job.
pub(crate) const MAX_TAG_JOB_BYTES: usize = 32 * 1024 * 1024;
/// Encoded delta BLOBs in unfinished tag intents.
pub(crate) const MAX_TAG_INTENT_BYTES: usize = 48 * 1024 * 1024;
/// Encoded tag inverse BLOBs across unfinished intents and journal history.
pub(crate) const MAX_TAG_HISTORY_BYTES: usize = 256 * 1024 * 1024;

/// Keys never edited: pictures (not text), and ratings (the library keeps ratings itself and
/// never writes them to files).
const NOT_EDITABLE: &[&str] = &[
    "METADATA_BLOCK_PICTURE",
    "COVERART",
    "COVERARTMIME",
    "COVR",
    "APIC",
    "PIC",
    "RATING",
    "POPULARIMETER",
    "FMPS_RATING",
    "CUESHEET",
];

/// What is wrong with one of an edit's changes ([`check_changes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagChangeError {
    /// What is wrong, as a path from the list of changes: `changes` (too many),
    /// `changes[2].key`, `changes[2].values` (none, or too many), `changes[2].values[0]`.
    pub field: String,
    pub message: String,
}

impl std::fmt::Display for TagChangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

/// Checks an edit's changes and puts them in the form they are applied in: keys in upper
/// case, values trimmed (the library trims what it reads). A key is 1 to [`MAX_TAG_KEY`]
/// printable ASCII characters without `=` or `:`, not starting or ending with a space, not
/// a picture or rating key, and the name the library shows it by (`ALBUMARTIST`, not
/// `TPE2`, `aART` or `ALBUM ARTIST`); each key once. A `set` has 1 to [`MAX_TAG_VALUES`]
/// values, each not blank, without NUL and at most [`MAX_TAG_VALUE_BYTES`] bytes; at most
/// [`MAX_TAG_CHANGES`] changes.
pub fn check_changes(changes: &[TagChange]) -> Result<Vec<TagChange>, TagChangeError> {
    let error = |field: String, message: String| TagChangeError { field, message };
    if changes.len() > MAX_TAG_CHANGES {
        return Err(error(
            "changes".into(),
            format!("at most {MAX_TAG_CHANGES} changes at once"),
        ));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(changes.len());
    for (i, change) in changes.iter().enumerate() {
        let key = check_key(change.key()).map_err(|m| error(format!("changes[{i}].key"), m))?;
        if !seen.insert(key.clone()) {
            return Err(error(
                format!("changes[{i}].key"),
                format!("{key} is changed twice"),
            ));
        }
        out.push(match change {
            TagChange::Remove { .. } => TagChange::Remove { key },
            TagChange::Set { values, .. } => {
                if values.is_empty() {
                    return Err(error(
                        format!("changes[{i}].values"),
                        format!("no values for {key}: remove a key with a `remove` change"),
                    ));
                }
                if values.len() > MAX_TAG_VALUES {
                    return Err(error(
                        format!("changes[{i}].values"),
                        format!("at most {MAX_TAG_VALUES} values for one key"),
                    ));
                }
                let mut trimmed = Vec::with_capacity(values.len());
                for (j, value) in values.iter().enumerate() {
                    let value = value.trim();
                    let problem = if value.is_empty() {
                        Some("a value is not blank".to_owned())
                    } else if value.contains('\0') {
                        Some("a value has no NUL character".to_owned())
                    } else if value.len() > MAX_TAG_VALUE_BYTES {
                        Some(format!(
                            "a value is at most {MAX_TAG_VALUE_BYTES} bytes of UTF-8"
                        ))
                    } else {
                        None
                    };
                    if let Some(problem) = problem {
                        return Err(error(format!("changes[{i}].values[{j}]"), problem));
                    }
                    trimmed.push(value.to_owned());
                }
                TagChange::Set {
                    key,
                    values: trimmed,
                }
            }
        });
    }
    if change_bytes(&out) > MAX_TAG_EDIT_BYTES {
        return Err(error(
            "changes".into(),
            format!("at most {MAX_TAG_EDIT_BYTES} bytes of keys and values in one edit"),
        ));
    }
    Ok(out)
}

/// Total size as displayed and bounded before applying changes.
pub fn change_bytes(changes: &[TagChange]) -> usize {
    changes
        .iter()
        .map(|c| c.key().len() + c.values().iter().map(String::len).sum::<usize>())
        .sum()
}

/// Quotes keys in summaries so user-supplied text cannot impersonate surrounding text.
pub fn display_key(key: &str) -> String {
    let key = key.to_ascii_uppercase();
    if key
        .bytes()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
    {
        key
    } else {
        format!("{key:?}")
    }
}

/// A key in the form the library shows it, or why it is not one.
fn check_key(key: &str) -> Result<String, String> {
    let ok_char = |c: char| (' '..='}').contains(&c) && c != '=' && c != ':';
    if key.is_empty()
        || key.chars().count() > MAX_TAG_KEY
        || !key.chars().all(ok_char)
        || key.starts_with(' ')
        || key.ends_with(' ')
    {
        return Err(format!(
            "{key:?} is not a tag key: 1 to {MAX_TAG_KEY} printable ASCII characters, no `=` \
             or `:`, not starting or ending with a space"
        ));
    }
    let key = key.to_ascii_uppercase();
    let mapped = |tag_type| ItemKey::from_key(tag_type, &key);
    if NOT_EDITABLE.contains(&key.as_str())
        || mapped(TagType::VorbisComments) == Some(ItemKey::Popularimeter)
    {
        return Err(format!(
            concat!(
                "{key} cannot be edited: pictures and CUE sheets are not edited, and ",
                "ratings stay in the library",
            ),
            key = key,
        ));
    }
    // Another name of a key the library shows under one name: editing it would change
    // that key.
    let known = mapped(TagType::VorbisComments)
        .or_else(|| mapped(TagType::Id3v2))
        .or_else(|| mapped(TagType::Mp4Ilst));
    if let Some(item) = known
        && key_name(item) != key
    {
        return Err(format!(
            "the library shows {key} as {}: change that key",
            key_name(item)
        ));
    }
    Ok(key)
}

/// The key a known item has in the library, if `key` is one ([`check_key`] made sure it is
/// its own name).
fn mapped_key(key: &str) -> Option<ItemKey> {
    ItemKey::from_key(TagType::VorbisComments, key).filter(|item| key_name(*item) == key)
}

/// Why a file cannot be edited (before anything is written).
#[derive(Debug)]
pub(crate) enum OpenError {
    /// Not a format whose tags are written (`unsupported_format`).
    Unsupported,
    /// It could not be read or parsed (`open_failed`).
    Unreadable(String),
}

/// The format a file is edited in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Flac,
    Ogg,
    Id3v2,
    Mp4,
}

/// A file's primary tag in its own type.
#[derive(Debug, Clone, PartialEq)]
enum Native {
    Vorbis(VorbisComments),
    Id3v2(Id3v2Tag),
    Ilst(Ilst),
}

/// The name an item has in its tag, kept for a key written again under it (letter case of a
/// Vorbis comment or a `TXXX` description, an MP4 atom's own code or freeform name).
#[derive(Debug, Clone)]
enum ItemName {
    Vorbis(String),
    UserText(String),
    Atom(AtomIdent<'static>),
}

/// How a file's Latin-1 ID3 text reads ([`Editable::decoding`]): in the encoding the whole
/// file reads it in (its folder's, or the one detected across all its tags), never
/// detected again from a part of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decoding(Option<&'static Encoding>, Option<&'static Encoding>);

/// A file held in memory to edit its tags.
#[derive(Clone)]
pub(crate) struct Editable {
    format: Format,
    pub(super) file_type: FileType,
    primary: Native,
    /// The file's other tags, as the library reads them after the primary one.
    others: Vec<Tag>,
    /// An ID3v2 tag older than 2.4 is written as 2.3.
    id3v23: bool,
}

#[cfg(all(test, feature = "desktop-fs"))]
pub(crate) fn open(path: &Path) -> Result<Editable, OpenError> {
    let file = crate::storage_access::open_regular(&mpmp_fs_desktop::DesktopStorage, path, false)
        .map_err(|e| OpenError::Unreadable(e.to_string()))?;
    open_file(file)
}

pub(super) fn open_file(file: impl Read + Seek) -> Result<Editable, OpenError> {
    panic::catch_unwind(AssertUnwindSafe(|| open_inner(file)))
        .unwrap_or_else(|_| Err(OpenError::Unreadable("tag reader panicked".into())))
}

fn open_inner(mut file: impl Read + Seek) -> Result<Editable, OpenError> {
    let unreadable = |e: &dyn std::fmt::Display| OpenError::Unreadable(e.to_string());
    let options = ParseOptions::new().read_properties(false);
    file.rewind().map_err(|e| unreadable(&e))?;
    let probe = Probe::new(BufReader::new(file))
        .options(options)
        .guess_file_type()
        .map_err(|e| unreadable(&e))?;
    let file_type = probe.file_type().ok_or(OpenError::Unsupported)?;
    let mut reader = probe.into_inner();
    reader.rewind().map_err(|e| unreadable(&e))?;
    let reader = &mut reader;
    let generic = |tag: Option<Tag>| tag.into_iter().collect::<Vec<_>>();
    let id3 = |tag: Option<&Id3v2Tag>| {
        let tag = tag.cloned().unwrap_or_default();
        let id3v23 = tag.original_version() != Id3v2Version::V4;
        (Native::Id3v2(tag), id3v23)
    };
    let ((primary, id3v23), others, format) = match file_type {
        FileType::Flac => {
            let f = FlacFile::read_from(&mut *reader, options).map_err(|e| unreadable(&e))?;
            let vc = f.vorbis_comments().cloned().unwrap_or_default();
            (
                (Native::Vorbis(vc), false),
                generic(f.id3v2().cloned().map(Tag::from)),
                Format::Flac,
            )
        }
        FileType::Vorbis => {
            let f = VorbisFile::read_from(&mut *reader, options).map_err(|e| unreadable(&e))?;
            let vc = Native::Vorbis(f.vorbis_comments().clone());
            ((vc, false), Vec::new(), Format::Ogg)
        }
        FileType::Opus => {
            let f = OpusFile::read_from(&mut *reader, options).map_err(|e| unreadable(&e))?;
            let vc = Native::Vorbis(f.vorbis_comments().clone());
            ((vc, false), Vec::new(), Format::Ogg)
        }
        FileType::Speex => {
            let f = SpeexFile::read_from(&mut *reader, options).map_err(|e| unreadable(&e))?;
            let vc = Native::Vorbis(f.vorbis_comments().clone());
            ((vc, false), Vec::new(), Format::Ogg)
        }
        FileType::Mpeg => {
            let f = MpegFile::read_from(&mut *reader, options).map_err(|e| unreadable(&e))?;
            let others = generic(f.id3v1().cloned().map(Tag::from))
                .into_iter()
                .chain(generic(f.ape().cloned().map(Tag::from)))
                .collect();
            (id3(f.id3v2()), others, Format::Id3v2)
        }
        FileType::Wav => {
            let f = WavFile::read_from(&mut *reader, options).map_err(|e| unreadable(&e))?;
            let others = generic(f.riff_info().cloned().map(Tag::from));
            (id3(f.id3v2()), others, Format::Id3v2)
        }
        FileType::Aiff => {
            let f = AiffFile::read_from(&mut *reader, options).map_err(|e| unreadable(&e))?;
            let others = generic(f.text_chunks().cloned().map(Tag::from));
            (id3(f.id3v2()), others, Format::Id3v2)
        }
        FileType::Aac => {
            let f = AacFile::read_from(&mut *reader, options).map_err(|e| unreadable(&e))?;
            let others = generic(f.id3v1().cloned().map(Tag::from));
            (id3(f.id3v2()), others, Format::Id3v2)
        }
        FileType::Mp4 => {
            let f = Mp4File::read_from(&mut *reader, options).map_err(|e| unreadable(&e))?;
            let ilst = Native::Ilst(f.ilst().cloned().unwrap_or_default());
            ((ilst, false), Vec::new(), Format::Mp4)
        }
        _ => return Err(OpenError::Unsupported),
    };
    Ok(Editable {
        format,
        file_type,
        primary,
        others,
        id3v23,
    })
}

/// The changes that write `values` (each key's, from [`Editable::primary_values`]) back:
/// a key with none is removed.
pub(crate) fn reverse_changes(values: &[(String, Vec<String>)]) -> Vec<TagChange> {
    values
        .iter()
        .map(|(key, values)| match values.is_empty() {
            true => TagChange::Remove { key: key.clone() },
            false => TagChange::Set {
                key: key.clone(),
                values: values.clone(),
            },
        })
        .collect()
}

/// `(key, value)` pairs grouped by key, in order.
fn grouped(pairs: Vec<(String, String)>) -> TagValues {
    let mut out = TagValues::new();
    for (key, value) in pairs {
        out.entry(key).or_default().push(value);
    }
    out
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests;

mod native;

mod editable;
