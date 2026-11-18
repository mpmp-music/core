//! Reads tags and audio properties of one file with lofty.

#[cfg(all(test, feature = "desktop-fs"))]
use mpmp_test_support::client_parity;

use std::fs::File;
use std::io::{BufReader, Seek};
use std::path::Path;

use encoding_rs::Encoding;
use lofty::TextEncoding;
use std::collections::{HashMap, HashSet, VecDeque};

use lofty::aac::AacFile;
use lofty::config::ParseOptions;
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::flac::FlacFile;
use lofty::id3::v2::{Frame, Id3v2Tag, Id3v2Version};
use lofty::iff::aiff::AiffFile;
use lofty::iff::wav::WavFile;
use lofty::mp4::{AtomData, AtomIdent, Ilst};
use lofty::mp4::{Mp4Codec, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::ogg::tag::VorbisComments;
use lofty::ogg::{OpusFile, SpeexFile, VorbisFile};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, Tag, TagType};

use crate::tags::encoding::{is_latin1_mojibake_candidate, repair_latin1_text};
use crate::types::ReplayGain;

/// Everything the library stores about a file's contents.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TrackMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub artists: Option<crate::person::ArtistList>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub year: Option<i32>,
    pub track_no: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_no: Option<u32>,
    pub disc_total: Option<u32>,
    pub compilation: bool,
    pub duration_ms: Option<u64>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    pub bits_per_sample: Option<u8>,
    pub bitrate_kbps: Option<u32>,
    pub codec: Option<&'static str>,
    pub lossless: Option<bool>,
    pub replay_gain: ReplayGain,
    pub has_embedded_art: bool,
    pub tag_encoding: Option<&'static Encoding>,
    /// All text tags as `(KEY, value)`, one entry per value (`track_tags`).
    pub tags: Vec<(String, String)>,
    /// A FLAC file's `CUESHEET` Vorbis comment, whole (cue.rs).
    pub embedded_cue: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TagError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Parse(#[from] lofty::error::FileParseError),
    #[error("tag reader panicked")]
    Panicked,
}

/// Text fields of one tag, in the order used for encoding repair.
#[derive(Default)]
struct TextFields {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    genre: Option<String>,
}

impl TextFields {
    fn of(tag: &Tag) -> Self {
        Self {
            title: tag.title().and_then(|s| clean(&s)),
            artist: tag.artist().and_then(|s| clean(&s)),
            album: tag.album().and_then(|s| clean(&s)),
            album_artist: tag.get_string(ItemKey::AlbumArtist).and_then(clean),
            genre: tag.genre().and_then(|s| clean(&s)),
        }
    }

    fn slots(&mut self) -> [&mut Option<String>; 5] {
        [
            &mut self.title,
            &mut self.artist,
            &mut self.album,
            &mut self.album_artist,
            &mut self.genre,
        ]
    }
}

/// Reads `path`. `forced` is the root's encoding override for Latin-1 ID3 text.
pub(crate) fn read_track(
    path: &Path,
    forced: Option<&'static Encoding>,
) -> Result<TrackMeta, TagError> {
    let options = ParseOptions::new();
    let reader = BufReader::new(File::open(path)?);
    let probe = Probe::new(reader).options(options).guess_file_type()?;
    let mut mp4_codec = None;
    let mut embedded_cue = None;
    // The main formats are read through their own types: only those expose the text encoding
    // byte of each ID3v2 frame (which decides what may be repaired) and the tags that lofty's
    // unified mapping does not know (which "store all tags" needs).
    let (tagged, raw) = match probe.file_type() {
        Some(FileType::Mp4) => read_typed(probe, options, |file: &Mp4File| {
            // The generic properties do not say whether an MP4 holds AAC or ALAC.
            mp4_codec = file.properties().codec();
            file.ilst().map(raw_ilst).unwrap_or_default()
        })?,
        Some(FileType::Mpeg) => read_typed(probe, options, |f: &MpegFile| raw_id3v2(f.id3v2()))?,
        Some(FileType::Wav) => read_typed(probe, options, |f: &WavFile| raw_id3v2(f.id3v2()))?,
        Some(FileType::Aiff) => read_typed(probe, options, |f: &AiffFile| raw_id3v2(f.id3v2()))?,
        Some(FileType::Aac) => read_typed(probe, options, |f: &AacFile| raw_id3v2(f.id3v2()))?,
        Some(FileType::Flac) => read_typed(probe, options, |f: &FlacFile| {
            embedded_cue = f
                .vorbis_comments()
                .and_then(|c| c.get("CUESHEET"))
                .filter(|sheet| !sheet.trim().is_empty())
                .map(str::to_owned);
            f.vorbis_comments().map(raw_vorbis).unwrap_or_default()
        })?,
        Some(FileType::Vorbis) => read_typed(probe, options, |f: &VorbisFile| {
            raw_vorbis(f.vorbis_comments())
        })?,
        Some(FileType::Opus) => read_typed(probe, options, |f: &OpusFile| {
            raw_vorbis(f.vorbis_comments())
        })?,
        Some(FileType::Speex) => read_typed(probe, options, |f: &SpeexFile| {
            raw_vorbis(f.vorbis_comments())
        })?,
        _ => (probe.read()?, RawTags::default()),
    };
    let mut meta = meta_from(&tagged, mp4_codec, &raw, forced);
    meta.embedded_cue = embedded_cue;
    Ok(meta)
}

/// Which of the [`TextFields`] of a file's ID3v2 tag were stored as Latin-1 (frame text
/// encoding byte 0), in [`TextFields::slots`] order.
type Latin1Mask = [bool; 5];

const MASK_FRAMES: [&str; 5] = ["TIT2", "TPE1", "TALB", "TPE2", "TCON"];

/// What only the format-specific tag types tell.
#[derive(Debug, Default)]
pub(crate) struct RawTags {
    /// ID3v2 text fields stored as Latin-1.
    latin1: Latin1Mask,
    /// Format-aware values; None falls back to unified tags for other formats.
    person_values: Option<Vec<(String, String, bool)>>,
    /// Non-MP4 secondary tags may supply an otherwise absent ARTIST.
    person_artist_fallback: bool,
    /// ID3v2 frame ids (and TXXX descriptions) stored as Latin-1.
    latin1_frames: HashSet<String>,
    /// Encodings of comment/lyrics items, which can share a key across encodings.
    text_encodings: HashMap<(ItemKey, String), VecDeque<bool>>,
    /// Items lofty's unified mapping drops: `(KEY, value, stored as Latin-1)`.
    unmapped: Vec<(String, String, bool)>,
}

impl RawTags {
    fn push_person(&mut self, key: &str, value: &str, latin1: bool, split_nul: bool) {
        let values = self.person_values.get_or_insert_with(Vec::new);
        if split_nul {
            values.extend(
                value
                    .split('\0')
                    .map(|v| (key.to_owned(), v.to_owned(), latin1)),
            );
        } else {
            values.push((key.to_owned(), value.to_owned(), latin1));
        }
    }

    fn artists(
        &self,
        tags: &[(String, String)],
        encoding: Option<&'static Encoding>,
        display: Option<&str>,
    ) -> crate::person::ArtistList {
        let Some(values) = &self.person_values else {
            return crate::person::ArtistList::from_tags(tags, display);
        };
        let mut values = values
            .iter()
            .filter_map(|(key, value, latin1)| {
                let mut value = value.clone();
                if *latin1
                    && let Some(encoding) = encoding
                    && is_latin1_mojibake_candidate(&value)
                {
                    let bytes: Vec<u8> = value.chars().map(|c| c as u32 as u8).collect();
                    if let Some(decoded) =
                        encoding.decode_without_bom_handling_and_without_replacement(&bytes)
                    {
                        value = decoded.into_owned();
                    }
                }
                clean(&value).map(|value| (key.clone(), value))
            })
            .collect::<Vec<_>>();
        if self.person_artist_fallback
            && !values
                .iter()
                .any(|(k, v)| k == "ARTIST" && !crate::text::normalize(v).is_empty())
        {
            values.extend(tags.iter().filter(|(k, _)| k == "ARTIST").cloned());
            if !values
                .iter()
                .any(|(k, v)| k == "ARTIST" && !crate::text::normalize(v).is_empty())
                && let Some(display) = display
            {
                values.push(("ARTIST".into(), display.to_owned()));
            }
        }
        // A missing declaration in a typed format means no people, even if an unrelated
        // MP4 namespace has an ARTISTS item in the general tag listing.
        crate::person::ArtistList::from_tags(&values, None)
    }
}

fn read_typed<F: AudioFile>(
    probe: Probe<BufReader<File>>,
    options: ParseOptions,
    raw: impl FnOnce(&F) -> RawTags,
) -> Result<(TaggedFile, RawTags), TagError> {
    let mut reader = probe.into_inner();
    reader.rewind()?;
    let file = F::read_from(&mut reader, options)?;
    let raw = raw(&file);
    Ok((file.into(), raw))
}

pub(crate) fn raw_id3v2(tag: Option<&Id3v2Tag>) -> RawTags {
    let mut raw = RawTags::default();
    let Some(tag) = tag else {
        return raw;
    };
    raw.person_values = Some(Vec::new());
    raw.person_artist_fallback = true;
    // Preserve the legacy interpretation of nonstandard v2.3 NULs. Only explicit
    // v2.3 ARTISTS uses Picard's slash join; display credits remain untouched.
    let split_nul = true;
    for frame in tag {
        match frame {
            Frame::Text(text) => {
                let id = frame.id().as_str();
                let latin1 = text.encoding == TextEncoding::Latin1;
                if id == "TPE1" {
                    raw.push_person("ARTIST", &text.value, latin1, split_nul);
                }
                if latin1 {
                    raw.latin1_frames.insert(id.to_owned());
                    if let Some(i) = MASK_FRAMES.iter().position(|f| *f == id) {
                        raw.latin1[i] = true;
                    }
                }
                if ItemKey::from_key(TagType::Id3v2, id).is_none() {
                    raw.unmapped
                        .push((id.to_ascii_uppercase(), text.value.to_string(), latin1));
                }
            }
            // `TIPL` (`PRODUCER`, `ARRANGER` …): one frame of several keys.
            Frame::KeyValue(list) if list.encoding == TextEncoding::Latin1 => {
                raw.latin1_frames.insert(frame.id().as_str().to_owned());
            }
            Frame::Comment(_) | Frame::UnsynchronizedText(_) => {
                let latin1 = match frame {
                    Frame::Comment(t) => t.encoding == TextEncoding::Latin1,
                    Frame::UnsynchronizedText(t) => t.encoding == TextEncoding::Latin1,
                    _ => unreachable!(),
                };
                let mut one = Id3v2Tag::new();
                one.insert(frame.clone());
                for item in Tag::from(one).items() {
                    if let Some(text) = item.value().text() {
                        raw.text_encodings
                            .entry((item.key(), text.to_owned()))
                            .or_default()
                            .push_back(latin1);
                    }
                }
            }
            Frame::UserText(text) => {
                let latin1 = text.encoding == TextEncoding::Latin1;
                if text.description.eq_ignore_ascii_case("ARTISTS") {
                    if tag.original_version() == Id3v2Version::V3 {
                        for value in text.content.split('/') {
                            raw.push_person("ARTISTS", value, latin1, split_nul);
                        }
                    } else {
                        raw.push_person("ARTISTS", &text.content, latin1, split_nul);
                    }
                }
                if latin1 {
                    raw.latin1_frames.insert(text.description.to_string());
                }
                if ItemKey::from_key(TagType::Id3v2, &text.description).is_none() {
                    raw.unmapped.push((
                        text.description.to_uppercase(),
                        text.content.to_string(),
                        latin1,
                    ));
                }
            }
            _ => {}
        }
    }
    raw
}

pub(crate) fn raw_vorbis(tag: &VorbisComments) -> RawTags {
    let mut raw = RawTags {
        person_values: Some(Vec::new()),
        person_artist_fallback: true,
        ..RawTags::default()
    };
    for (key, value) in tag.items() {
        if key.eq_ignore_ascii_case("ARTISTS") || key.eq_ignore_ascii_case("ARTIST") {
            raw.push_person(&key.to_ascii_uppercase(), value, false, false);
        }
        if ItemKey::from_key(TagType::VorbisComments, key).is_none() && !key.contains(':') {
            raw.unmapped
                .push((key.to_ascii_uppercase(), value.to_owned(), false));
        }
    }
    raw
}

pub(crate) fn raw_ilst(tag: &Ilst) -> RawTags {
    let mut raw = RawTags {
        person_values: Some(Vec::new()),
        ..RawTags::default()
    };
    for atom in tag {
        let person_key = match atom.ident() {
            AtomIdent::Fourcc([0xa9, b'A', b'R', b'T']) => Some("ARTIST"),
            AtomIdent::Freeform { mean, name }
                if mean.as_ref() == "com.apple.iTunes" && name.as_ref() == "ARTISTS" =>
            {
                Some("ARTISTS")
            }
            _ => None,
        };
        if let Some(key) = person_key {
            for data in atom.data() {
                if let AtomData::UTF8(value) | AtomData::UTF16(value) = data {
                    raw.push_person(key, value, false, false);
                }
            }
        }
        let (lookup, key) = match atom.ident() {
            AtomIdent::Fourcc(code) => {
                let code: String = code.iter().map(|&b| b as char).collect();
                (code.clone(), code)
            }
            AtomIdent::Freeform { mean, name } => (format!("----:{mean}:{name}"), name.to_string()),
        };
        if ItemKey::from_key(TagType::Mp4Ilst, &lookup).is_some() {
            continue;
        }
        for data in atom.data() {
            let value = match data {
                AtomData::UTF8(text) | AtomData::UTF16(text) => text.clone(),
                AtomData::SignedInteger(n) => n.to_string(),
                AtomData::UnsignedInteger(n) => n.to_string(),
                AtomData::Bool(b) => u8::from(*b).to_string(),
                _ => continue,
            };
            raw.unmapped.push((key.to_uppercase(), value, false));
        }
    }
    raw
}

/// The unified, upper-case name of a mapped item: its Vorbis comment name (`TITLE`,
/// `ALBUMARTIST`, `COMPOSER`, `LABEL`,...), else the variant name.
pub(crate) fn key_name(key: ItemKey) -> String {
    key.map_key(TagType::VorbisComments)
        .map(str::to_ascii_uppercase)
        .unwrap_or_else(|| format!("{key:?}").to_ascii_uppercase())
}

/// Longest tag value stored (lyrics and the like are cut).
pub(crate) const MAX_TAG_VALUE: usize = 8 * 1024;

/// All text tags of the file as `(KEY, value)`, one entry per value: the primary tag's
/// items, then keys only other tags have, then items lofty does not map. Latin-1 ID3 text is
/// re-decoded with `encoding` when it was repaired. Values longer than `limit` bytes
/// are cut ([`MAX_TAG_VALUE`] for what the library stores).
fn all_tags(
    tags: &[&Tag],
    raw: &RawTags,
    encoding: Option<&'static Encoding>,
    limit: usize,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut text_encodings = raw.text_encodings.clone();
    let push = |out: &mut Vec<(String, String)>, key: &str, value: &str, latin1: bool| {
        for part in value.split('\0') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let mut text = part.to_owned();
            if latin1
                && let Some(encoding) = encoding
                && is_latin1_mojibake_candidate(&text)
            {
                let bytes: Vec<u8> = text.chars().map(|c| c as u32 as u8).collect();
                if let Some(decoded) =
                    encoding.decode_without_bom_handling_and_without_replacement(&bytes)
                {
                    text = decoded.into_owned();
                }
            }
            if text.len() > limit {
                let mut end = limit;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            out.push((key.to_owned(), text));
        }
    };
    for tag in tags {
        let mut keys_here = HashSet::new();
        for item in tag.items() {
            let Some(value) = item.value().text().or_else(|| item.value().locator()) else {
                continue;
            };
            let name = key_name(item.key());
            if seen_keys.contains(&name) {
                continue;
            }
            let latin1 = match tag.tag_type() {
                TagType::Id3v1 => true,
                TagType::Id3v2 => text_encodings
                    .get_mut(&(item.key(), value.to_owned()))
                    .and_then(VecDeque::pop_front)
                    .unwrap_or_else(|| {
                        item.key()
                            .map_key(TagType::Id3v2)
                            .is_some_and(|id| raw.latin1_frames.contains(id))
                    }),
                _ => false,
            };
            push(&mut out, &name, value, latin1);
            keys_here.insert(name);
        }
        seen_keys.extend(keys_here);
    }
    for (key, value, latin1) in &raw.unmapped {
        if !seen_keys.contains(key) {
            push(&mut out, key, value, *latin1);
        }
    }
    out
}

fn meta_from(
    tagged: &TaggedFile,
    mp4_codec: Option<Mp4Codec>,
    raw: &RawTags,
    forced: Option<&'static Encoding>,
) -> TrackMeta {
    let mut meta = TrackMeta::default();

    // Primary tag first, then the others (e.g. ID3v1 after ID3v2) to fill gaps.
    let primary = tagged.primary_tag_type();
    let mut tags: Vec<&Tag> = tagged.tags().iter().collect();
    tags.sort_by_key(|tag| tag.tag_type() != primary);

    let mut texts: Vec<TextFields> = tags.iter().map(|tag| TextFields::of(tag)).collect();
    meta.tag_encoding = repair_id3_text(&tags, &mut texts, raw.latin1, forced);
    meta.tags = all_tags(&tags, raw, meta.tag_encoding.or(forced), MAX_TAG_VALUE);

    for (tag, mut text) in tags.iter().zip(texts) {
        let [title, artist, album, album_artist, genre] = text.slots();
        fill(&mut meta.title, title.take());
        fill(&mut meta.artist, artist.take());
        fill(&mut meta.album, album.take());
        fill(&mut meta.album_artist, album_artist.take());
        fill(&mut meta.genre, genre.take());
        fill(&mut meta.year, year_of(tag));
        fill(&mut meta.track_no, tag.track().filter(|&n| n > 0));
        fill(&mut meta.track_total, tag.track_total().filter(|&n| n > 0));
        fill(&mut meta.disc_no, tag.disk().filter(|&n| n > 0));
        fill(&mut meta.disc_total, tag.disk_total().filter(|&n| n > 0));
        meta.compilation |= tag
            .get_string(ItemKey::FlagCompilation)
            .is_some_and(is_true);
        let rg = &mut meta.replay_gain;
        fill(
            &mut rg.track_gain_db,
            number(tag, ItemKey::ReplayGainTrackGain),
        );
        fill(
            &mut rg.track_peak,
            number(tag, ItemKey::ReplayGainTrackPeak),
        );
        fill(
            &mut rg.album_gain_db,
            number(tag, ItemKey::ReplayGainAlbumGain),
        );
        fill(
            &mut rg.album_peak,
            number(tag, ItemKey::ReplayGainAlbumPeak),
        );
        meta.has_embedded_art |= !tag.pictures().is_empty();
    }

    meta.artists = Some(raw.artists(
        &meta.tags,
        meta.tag_encoding.or(forced),
        meta.artist.as_deref(),
    ));

    let props = tagged.properties();
    let duration = props.duration();
    meta.duration_ms = (!duration.is_zero()).then_some(duration.as_millis() as u64);
    meta.sample_rate = props.sample_rate().filter(|&r| r > 0);
    meta.channels = props.channels().filter(|&c| c > 0);
    meta.bits_per_sample = props.bit_depth().filter(|&b| b > 0);
    meta.bitrate_kbps = props
        .audio_bitrate()
        .or(props.overall_bitrate())
        .filter(|&b| b > 0);
    (meta.codec, meta.lossless) = codec_of(tagged.file_type(), mp4_codec);
    meta
}

/// The encoding a file's tags' Latin-1 ID3 text reads in, as a scan reads it (`primary`:
/// the file's primary tag type; `raw`: what only its own type tells): `forced` (its root's
/// encoding), or the one detected across all of them together ; `None`: as Latin-1.
pub(crate) fn latin1_encoding(
    primary: TagType,
    tags: &[Tag],
    raw: &RawTags,
    forced: Option<&'static Encoding>,
) -> Option<&'static Encoding> {
    let mut tags: Vec<&Tag> = tags.iter().collect();
    tags.sort_by_key(|tag| tag.tag_type() != primary);
    let mut texts: Vec<TextFields> = tags.iter().map(|tag| TextFields::of(tag)).collect();
    repair_id3_text(&tags, &mut texts, raw.latin1, forced).or(forced)
}

/// The text tags of a file's tags as [`TrackMeta::tags`] has them (`primary`: the file's
/// primary tag type, whose tag comes first; `raw`: what only its own type tells), values
/// cut at `limit` bytes, Latin-1 ID3 text read in `encoding` ([`latin1_encoding`] of all the
/// file's tags; nothing is detected here, so that a part of them, one frame, reads as it
/// does in the whole): what tag editing shows and keeps of a file it holds in memory
/// (`crate::tags::edit`).
pub(crate) fn text_tags_in(
    primary: TagType,
    tags: &[Tag],
    raw: &RawTags,
    encoding: Option<&'static Encoding>,
    limit: usize,
) -> Vec<(String, String)> {
    let mut tags: Vec<&Tag> = tags.iter().collect();
    tags.sort_by_key(|tag| tag.tag_type() != primary);
    all_tags(&tags, raw, encoding, limit)
}

/// Repairs Latin-1 mojibake across the ID3 tags of the file together: all of ID3v1
/// (Latin-1 by definition) and the ID3v2 frames marked Latin-1. Unicode frames are never
/// touched.
fn repair_id3_text(
    tags: &[&Tag],
    texts: &mut [TextFields],
    id3v2_latin1: Latin1Mask,
    forced: Option<&'static Encoding>,
) -> Option<&'static Encoding> {
    let mut slots: Vec<&mut Option<String>> = Vec::new();
    for (tag, text) in tags.iter().zip(texts.iter_mut()) {
        let mask = match tag.tag_type() {
            TagType::Id3v1 => [true; 5],
            TagType::Id3v2 => id3v2_latin1,
            _ => continue,
        };
        for (slot, latin1) in text.slots().into_iter().zip(mask) {
            if latin1 && slot.is_some() {
                slots.push(slot);
            }
        }
    }
    let mut fields: Vec<String> = slots
        .iter_mut()
        .map(|slot| slot.take().unwrap_or_default())
        .collect();
    let applied = repair_latin1_text(&mut fields, forced);
    for (slot, field) in slots.into_iter().zip(fields) {
        *slot = clean(&field);
    }
    applied
}

fn codec_of(file_type: FileType, mp4: Option<Mp4Codec>) -> (Option<&'static str>, Option<bool>) {
    match file_type {
        FileType::Flac => (Some("flac"), Some(true)),
        FileType::Mpeg => (Some("mp3"), Some(false)),
        FileType::Vorbis => (Some("vorbis"), Some(false)),
        FileType::Opus => (Some("opus"), Some(false)),
        FileType::Speex => (Some("speex"), Some(false)),
        FileType::Wav | FileType::Aiff => (Some("pcm"), Some(true)),
        FileType::Aac => (Some("aac"), Some(false)),
        FileType::Ape => (Some("ape"), Some(true)),
        FileType::WavPack => (Some("wavpack"), None),
        FileType::Mpc => (Some("musepack"), Some(false)),
        FileType::Mp4 => match mp4 {
            Some(Mp4Codec::AAC) => (Some("aac"), Some(false)),
            Some(Mp4Codec::ALAC) => (Some("alac"), Some(true)),
            Some(Mp4Codec::MP3) => (Some("mp3"), Some(false)),
            Some(Mp4Codec::FLAC) => (Some("flac"), Some(true)),
            _ => (None, None),
        },
        _ => (None, None),
    }
}

fn fill<T>(slot: &mut Option<T>, value: Option<T>) {
    if slot.is_none() {
        *slot = value;
    }
}

/// Trims, drops NULs and turns empty strings into `None`.
fn clean(text: &str) -> Option<String> {
    let text = text.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    (!text.is_empty()).then(|| text.replace('\0', "; "))
}

pub(crate) fn is_true(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

fn year_of(tag: &Tag) -> Option<i32> {
    if let Some(date) = tag.date() {
        return Some(i32::from(date.year)).filter(|&y| y > 0);
    }
    [
        ItemKey::RecordingDate,
        ItemKey::Year,
        ItemKey::ReleaseDate,
        ItemKey::OriginalReleaseDate,
    ]
    .into_iter()
    .filter_map(|key| tag.get_string(key))
    .find_map(leading_year)
}

/// The first four-digit run of `text`, e.g. `2003` from `"2003-07-31"` or `"(c) 2003"`.
pub(crate) fn leading_year(text: &str) -> Option<i32> {
    let bytes = text.as_bytes();
    bytes.windows(4).enumerate().find_map(|(i, w)| {
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
        let after_ok = bytes.get(i + 4).is_none_or(|b| !b.is_ascii_digit());
        (before_ok && after_ok && w.iter().all(u8::is_ascii_digit))
            .then(|| std::str::from_utf8(w).ok()?.parse().ok())
            .flatten()
            .filter(|&y| y > 0)
    })
}

/// Parses ReplayGain values such as `"-6.52 dB"` or `"0.988"`.
fn number(tag: &Tag, key: ItemKey) -> Option<f32> {
    gain_value(tag.get_string(key)?)
}

/// A ReplayGain value such as `"-6.52 dB"` or `"0.988"`.
pub(crate) fn gain_value(text: &str) -> Option<f32> {
    let text = text.trim();
    let text = text
        .strip_suffix("dB")
        .or_else(|| text.strip_suffix("db"))
        .unwrap_or(text);
    text.trim().parse::<f32>().ok().filter(|v| v.is_finite())
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::*;
    use lofty::tag::{ItemValue, TagItem};

    #[test]
    fn year_parsing() {
        assert_eq!(leading_year("2003-07-31"), Some(2003));
        assert_eq!(leading_year("(c) 1999 Foo"), Some(1999));
        assert_eq!(leading_year("12345"), None);
        assert_eq!(leading_year("n/a"), None);
    }

    #[test]
    fn sdk_mock_grouping_uses_cleaned_tags() {
        let path =
            client_parity::core_crate_path(env!("CARGO_MANIFEST_DIR"), "mock-state-cases.json");
        let mut cases: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let update = std::env::var_os("UPDATE_MOCK_GROUP_CASES").is_some();
        for case in cases["grouping"].as_array_mut().unwrap() {
            let meta = TrackMeta {
                album_artist: case["album_artist"].as_str().and_then(clean),
                artist: case["artist"].as_str().and_then(clean),
                album: case["album"].as_str().and_then(clean),
                compilation: case["compilation"] == true,
                ..Default::default()
            };
            let (key, artist) = crate::db::writer::album_grouping(&meta);
            let actual = serde_json::json!({"group_artist":artist,"album_key":key});
            if update {
                case["expected"] = actual;
            } else {
                assert_eq!(actual, case["expected"], "{case}");
            }
        }
        if update {
            std::fs::write(path, serde_json::to_string_pretty(&cases).unwrap() + "\n").unwrap();
        }
    }

    #[test]
    fn clean_trims_and_drops_empty() {
        assert_eq!(clean("  x \0"), Some("x".to_owned()));
        assert_eq!(clean("a\0b"), Some("a; b".to_owned()));
        assert_eq!(clean(" \0 "), None);
    }

    fn all_latin1() -> RawTags {
        RawTags {
            latin1: [true; 5],
            person_values: None,
            person_artist_fallback: false,
            latin1_frames: MASK_FRAMES.iter().map(|f| f.to_string()).collect(),
            unmapped: Vec::new(),
            text_encodings: HashMap::new(),
        }
    }

    #[test]
    fn id3_tags_are_repaired_together_and_others_are_not() {
        let gbk = |s: &str| -> String {
            let (bytes, _, _) = encoding_rs::GBK.encode(s);
            bytes.iter().map(|&b| b as char).collect()
        };
        let mut id3 = Tag::new(TagType::Id3v2);
        id3.set_title(gbk("三棱雾"));
        id3.set_artist(gbk("周砚舟"));
        id3.set_album(gbk("三棱雾"));
        let mut vorbis = Tag::new(TagType::VorbisComments);
        vorbis.set_title("Café".to_owned());
        vorbis.insert(TagItem::new(
            ItemKey::ReplayGainTrackGain,
            ItemValue::Text("-6.5 dB".to_owned()),
        ));
        let tagged = TaggedFile::new(FileType::Mpeg, Default::default(), vec![id3, vorbis]);

        let meta = meta_from(&tagged, None, &all_latin1(), None);
        assert_eq!(meta.title.as_deref(), Some("三棱雾"));

        // The same text in Unicode frames is what the tag really says: left alone.
        let meta = meta_from(&tagged, None, &RawTags::default(), None);
        assert_eq!(meta.title.as_deref(), Some(gbk("三棱雾").as_str()));
        assert_eq!(meta.tag_encoding, None);
        let meta = meta_from(&tagged, None, &RawTags::default(), Some(encoding_rs::GBK));
        assert_eq!(meta.tag_encoding, None);

        let meta = meta_from(&tagged, None, &all_latin1(), None);
        assert_eq!(meta.title.as_deref(), Some("三棱雾"));
        assert_eq!(meta.artist.as_deref(), Some("周砚舟"));
        assert_eq!(meta.tag_encoding, Some(encoding_rs::GBK));
        assert_eq!(meta.replay_gain.track_gain_db, Some(-6.5));
        assert_eq!(meta.codec, Some("mp3"));
    }
}
