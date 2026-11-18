//! CUE sheets (docs/design/media-resources.md): parsing a sheet and cutting its
//! files into tracks. Every track starts at its `INDEX 01` and ends where the next track of
//! the same file starts (so a pregap, `INDEX 00` to `INDEX 01`, ends the previous track), or
//! at the end of the file; audio before the first track's `INDEX 01` that is longer than
//! [`HTOA_MIN`] becomes track 0 (the hidden track), shorter is ignored. Times are CD frames,
//! 1/75 s. The scanner (scan.rs) finds the files and builds the library rows.
//!
//! Tags use the library's (Vorbis comment) names: the sheet's `TITLE` is `ALBUM`, its
//! `PERFORMER` is `ALBUMARTIST` and the `ARTIST` of tracks without one, `SONGWRITER` is
//! `COMPOSER`, `CATALOG` stays, `REM <KEY> <value>` is `<KEY>` (`TOTALDISCS` →
//! `DISCTOTAL`); a track's own values win over the sheet's, and [`cue_meta`] lets them win
//! over the audio file's tags, which fill in what the sheet does not say.

use std::collections::HashSet;

use encoding_rs::Encoding;

use crate::tags::encoding::decode_text;
use crate::tags::read::{TrackMeta, gain_value, is_true, leading_year};

/// CD frames (sectors) per second: the unit of every CUE time.
pub(crate) const CD_FRAMES_PER_SECOND: u64 = 75;

/// Audio before track 1 longer than this (10 s) is track 0, the hidden track (HTOA).
pub(crate) const HTOA_MIN: u64 = 10 * CD_FRAMES_PER_SECOND;

/// Largest sheet read; real ones are a few KB.
pub(crate) const MAX_SHEET_BYTES: u64 = 1024 * 1024;

/// A parsed sheet: its `FILE` names in order and its tracks in sheet order.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Sheet {
    /// Sheet-level tags (before the first `TRACK`).
    pub tags: Vec<(String, String)>,
    /// `FILE` names as written (`\` turned into `/`).
    pub files: Vec<String>,
    pub tracks: Vec<SheetTrack>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SheetTrack {
    pub file: usize,
    pub number: u32,
    /// `TRACK nn AUDIO` (not a data track such as `MODE1/2352`).
    pub audio: bool,
    pub tags: Vec<(String, String)>,
    /// `INDEX 00` and `INDEX 01`: (index into [`Sheet::files`], CD frames). A track's
    /// `INDEX 00` may be in the previous file (a gap appended to it).
    pub index0: Option<(usize, u64)>,
    pub index1: Option<(usize, u64)>,
}

/// One track cut from a sheet: which file, which part of it, and its tags.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cut {
    /// Index into [`Sheet::files`].
    pub file: usize,
    /// The `TRACK` number; 0 for the hidden track before track 1.
    pub number: u32,
    /// CD frames from the start of the file.
    pub start: u64,
    /// `None`: to the end of the file.
    pub end: Option<u64>,
    /// The sheet's tags overridden by the track's, plus `TRACKNUMBER` and `TRACKTOTAL`.
    pub tags: Vec<(String, String)>,
}

impl Sheet {
    /// Parses sheet text (any line ending; commands in any letter case; values quoted or
    /// not). Lenient: lines it does not understand are skipped.
    pub fn parse(text: &str) -> Sheet {
        let mut sheet = Sheet::default();
        let mut numbers = HashSet::new();
        for line in text.split(['\r', '\n']) {
            let line = line.trim_start_matches('\u{feff}').trim();
            let (command, rest) = split_word(line);
            let in_track = !sheet.tracks.is_empty();
            match command.to_ascii_uppercase().as_str() {
                "FILE" => sheet.files.push(file_name(rest).replace('\\', "/")),
                "TRACK" => {
                    let (number, kind) = split_word(rest);
                    let Ok(number) = number.parse::<u32>() else {
                        continue;
                    };
                    // A track before any FILE has nothing to play; a repeated number is
                    // a broken sheet: the first one counts.
                    if number == 0 || sheet.files.is_empty() || !numbers.insert(number) {
                        sheet.tracks.push(SheetTrack::default());
                        continue;
                    }
                    sheet.tracks.push(SheetTrack {
                        file: sheet.files.len() - 1,
                        number,
                        audio: kind.trim().eq_ignore_ascii_case("AUDIO"),
                        ..SheetTrack::default()
                    });
                }
                "INDEX" => {
                    let (index, time) = split_word(rest);
                    let (Some(track), Ok(index), Some(time)) = (
                        sheet.tracks.last_mut(),
                        index.parse::<u32>(),
                        parse_time(time.trim()),
                    ) else {
                        continue;
                    };
                    let at = Some((sheet.files.len().saturating_sub(1), time));
                    match index {
                        0 if track.index0.is_none() => track.index0 = at,
                        1 if track.index1.is_none() => {
                            track.file = sheet.files.len().saturating_sub(1);
                            track.index1 = at;
                        }
                        _ => {}
                    }
                }
                "TITLE" | "PERFORMER" | "SONGWRITER" | "ISRC" | "CATALOG" => {
                    let value = unquote(rest);
                    let key = match (command.to_ascii_uppercase().as_str(), in_track) {
                        ("TITLE", true) => "TITLE",
                        ("TITLE", false) => "ALBUM",
                        ("PERFORMER", true) => "ARTIST",
                        ("SONGWRITER", _) => "COMPOSER",
                        ("ISRC", _) => "ISRC",
                        ("CATALOG", _) => "CATALOG",
                        _ => "ALBUMARTIST",
                    };
                    if key == "ALBUMARTIST" {
                        sheet.set(false, "ARTIST", value);
                    }
                    sheet.set(in_track, key, value);
                }
                "REM" => {
                    let (key, value) = split_word(rest);
                    let key = match key.to_ascii_uppercase().as_str() {
                        "TOTALDISCS" => "DISCTOTAL".to_owned(),
                        "YEAR" => "DATE".to_owned(),
                        other => other.to_owned(),
                    };
                    if !key.is_empty() && key.chars().all(|c| c.is_ascii_graphic()) {
                        sheet.set(in_track, &key, unquote(value));
                    }
                }
                _ => {}
            }
        }
        // Placeholders for skipped TRACK lines kept INDEX lines off the track before.
        sheet.tracks.retain(|t| t.number != 0);
        sheet
    }

    /// Shared ownership rule for fresh scans and legacy identity correction.
    pub fn whole_file_track(&self, file: usize) -> Option<&SheetTrack> {
        let mut tracks = self.tracks.iter().filter(|track| track.file == file);
        let track = tracks.next()?;
        (track.audio && tracks.next().is_none()).then_some(track)
    }

    /// A following track may append its INDEX 00 gap to this file without owning it.
    pub fn appended_gap_end(&self, file: usize) -> Option<u64> {
        let position = self.tracks.iter().position(|track| track.file == file)?;
        let next = self.tracks.get(position + 1)?;
        let (owner, end) = next.index0?;
        (owner == file && next.index1.is_some_and(|(owner, _)| owner != file)).then_some(end)
    }

    /// A single full-file song supplies missing display tags without becoming a cut.
    pub fn whole_file_tags(&self, file: usize) -> Option<Vec<(String, String)>> {
        let track = self.whole_file_track(file)?;
        if track.index1.is_some_and(|(_, start)| start > HTOA_MIN)
            || track
                .index0
                .filter(|(owner, _)| *owner == file)
                .or(track.index1)
                .is_some_and(|(_, start)| start != 0)
        {
            return None;
        }
        Some(self.merged(Some(track), track.number, &self.tracks.len().to_string()))
    }

    /// Sets `key` (replacing earlier values) on the sheet, or on the last track.
    fn set(&mut self, in_track: bool, key: &str, value: &str) {
        if value.is_empty() {
            return;
        }
        let tags = match self.tracks.last_mut() {
            Some(track) if in_track => &mut track.tags,
            _ => &mut self.tags,
        };
        tags.retain(|(k, _)| k != key);
        tags.push((key.to_owned(), value.to_owned()));
    }

    /// The audio tracks, cut. With `single_file` (a sheet embedded in the file it
    /// describes) every track is taken to be in file 0, whatever its `FILE` lines say.
    /// Tracks without an index, or that do not start after the track before them in the
    /// same file, are left out.
    pub fn cut(&self, single_file: bool) -> Vec<Cut> {
        let audio: Vec<&SheetTrack> = self.tracks.iter().filter(|t| t.audio).collect();
        let total = audio.len().to_string();
        let mut starts: Vec<(&SheetTrack, usize, u64)> = Vec::new();
        for track in audio {
            let Some((file, start)) = track.index1.or(track.index0) else {
                continue;
            };
            let file = if single_file { 0 } else { file };
            if starts
                .last()
                .is_some_and(|&(_, f, s)| f == file && s >= start)
            {
                continue;
            }
            starts.push((track, file, start));
        }
        let mut cuts = Vec::with_capacity(starts.len() + 1);
        if let Some(&(_, file, start)) = starts.first()
            && start > HTOA_MIN
        {
            cuts.push(Cut {
                file,
                number: 0,
                start: 0,
                end: Some(start),
                tags: self.merged(None, 0, &total),
            });
        }
        for (i, &(track, file, start)) in starts.iter().enumerate() {
            let end = starts
                .get(i + 1)
                .filter(|&&(_, f, _)| f == file)
                .map(|&(_, _, s)| s);
            cuts.push(Cut {
                file,
                number: track.number,
                start,
                end,
                tags: self.merged(Some(track), track.number, &total),
            });
        }
        cuts
    }

    fn merged(
        &self,
        track: Option<&SheetTrack>,
        number: u32,
        total: &str,
    ) -> Vec<(String, String)> {
        let own = track.map_or(&[][..], |t| &t.tags[..]);
        let mut tags: Vec<(String, String)> = self
            .tags
            .iter()
            .filter(|(key, _)| !own.iter().any(|(k, _)| k == key))
            .chain(own)
            .cloned()
            .collect();
        tags.push(("TRACKNUMBER".into(), number.to_string()));
        tags.push(("TRACKTOTAL".into(), total.to_owned()));
        tags
    }
}

/// Decodes a sheet file's bytes like an M3U file (byte-order mark, UTF-8, else detected:
/// GBK, Big5, Shift_JIS …, Windows-1252 otherwise), except that the root's forced encoding,
/// if any, replaces the detection. Also returns the legacy encoding used, if any.
pub(crate) fn decode_sheet(
    bytes: &[u8],
    forced: Option<&'static Encoding>,
) -> (String, Option<&'static Encoding>) {
    decode_text(bytes, false, forced)
}

/// The first whitespace-separated word and the rest (trimmed at the start).
fn split_word(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    match text.find(char::is_whitespace) {
        Some(i) => (&text[..i], text[i..].trim_start()),
        None => (text, ""),
    }
}

/// A value, quoted (`"a b"`) or not (the rest of the line).
fn unquote(text: &str) -> &str {
    let text = text.trim();
    match text.strip_prefix('"') {
        Some(inner) => inner.strip_suffix('"').unwrap_or(inner).trim(),
        None => text,
    }
}

/// The name of a `FILE` line: quoted (up to the last quote, so quotes inside a name
/// survive) or not (all but a trailing file type such as `WAVE`).
fn file_name(rest: &str) -> String {
    let rest = rest.trim();
    if let Some(inner) = rest.strip_prefix('"') {
        return match inner.rfind('"') {
            Some(end) => inner[..end].to_owned(),
            None => inner.to_owned(),
        };
    }
    match rest.rsplit_once(char::is_whitespace) {
        Some((name, kind)) if kind.chars().all(|c| c.is_ascii_alphanumeric()) => {
            name.trim().to_owned()
        }
        _ => rest.to_owned(),
    }
}

/// `mm:ss:ff` (minutes may exceed 99) in CD frames.
pub(crate) fn parse_time(text: &str) -> Option<u64> {
    let mut parts = text.split(':');
    let (m, s, f) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let number = |p: &str| -> Option<u64> {
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse().ok())
            .flatten()
    };
    let (m, s, f) = (number(m)?, number(s)?, number(f)?);
    // Allow long images (about 69 days), but not arbitrary integer-sized durations.
    if m > 100_000 || s >= 60 || f >= CD_FRAMES_PER_SECOND {
        return None;
    }
    m.checked_mul(60)?
        .checked_add(s)?
        .checked_mul(CD_FRAMES_PER_SECOND)?
        .checked_add(f)
}

/// CD frames as milliseconds (rounded down).
pub(crate) fn cd_frames_to_ms(frames: u64) -> u64 {
    (u128::from(frames) * 1000 / u128::from(CD_FRAMES_PER_SECOND)).min(u128::from(u64::MAX)) as u64
}

/// CD frames as frames at `sample_rate` (exact for rates that are multiples of 75, as all
/// common ones are; else rounded down).
pub(crate) fn cd_frames_to_samples(frames: u64, sample_rate: u32) -> u64 {
    (u128::from(frames) * u128::from(sample_rate) / u128::from(CD_FRAMES_PER_SECOND))
        .min(u128::from(u64::MAX)) as u64
}

/// Tags of a cut track's audio file that never carry over: they describe the whole file
/// (its own sheet, its place in an album of files).
const FILE_ONLY_TAGS: &[&str] = &["CUESHEET", "TRACKNUMBER", "TRACKTOTAL", "TOTALTRACKS"];

/// The stored data of a cut track: the sheet's tags first, the audio file's (`base`) for
/// what the sheet does not say; the duration of the cut (`None` when the file's length is
/// unknown and the cut runs to its end, or the cut starts beyond the end); the other audio
/// properties are the file's. `encoding`: the legacy encoding the sheet was decoded from.
pub(crate) fn cue_meta(
    base: &TrackMeta,
    cut: &Cut,
    encoding: Option<&'static Encoding>,
) -> TrackMeta {
    let get = |key: &str| {
        cut.tags
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    let mut meta = base.clone();
    meta.embedded_cue = None;
    let mut tags: Vec<(String, String)> = base
        .tags
        .iter()
        .filter(|(key, _)| {
            !FILE_ONLY_TAGS.contains(&key.as_str()) && !cut.tags.iter().any(|(k, _)| k == key)
        })
        .cloned()
        .collect();
    tags.extend(cut.tags.iter().cloned());
    meta.tags = tags;
    let or = |slot: &mut Option<String>, key: &str| {
        if let Some(value) = get(key) {
            *slot = Some(value);
        }
    };
    or(&mut meta.title, "TITLE");
    or(&mut meta.artist, "ARTIST");
    if let Some(performer) = get("ARTIST") {
        meta.artists = Some(crate::person::ArtistList::new(
            [performer.as_str()],
            crate::ArtistListSource::CuePerformer,
        ));
    }
    or(&mut meta.album, "ALBUM");
    or(&mut meta.album_artist, "ALBUMARTIST");
    or(&mut meta.genre, "GENRE");
    if let Some(year) = get("DATE").as_deref().and_then(leading_year) {
        meta.year = Some(year);
    }
    meta.track_no = Some(cut.number);
    meta.track_total = get("TRACKTOTAL")
        .and_then(|t| t.parse().ok())
        .filter(|&n| n > 0);
    if let Some(disc) = get("DISCNUMBER") {
        // `1` or `1/2`.
        let (no, total) = disc.split_once('/').unwrap_or((&disc, ""));
        if let Ok(no) = no.trim().parse::<u32>() {
            meta.disc_no = Some(no).filter(|&n| n > 0);
        }
        if let Ok(total) = total.trim().parse::<u32>() {
            meta.disc_total = Some(total).filter(|&n| n > 0);
        }
    }
    if let Some(total) = get("DISCTOTAL").and_then(|t| t.trim().parse::<u32>().ok()) {
        meta.disc_total = Some(total).filter(|&n| n > 0);
    }
    if let Some(compilation) = get("COMPILATION") {
        meta.compilation = is_true(&compilation);
    }
    let rg = &mut meta.replay_gain;
    for (slot, key) in [
        (&mut rg.track_gain_db, "REPLAYGAIN_TRACK_GAIN"),
        (&mut rg.track_peak, "REPLAYGAIN_TRACK_PEAK"),
        (&mut rg.album_gain_db, "REPLAYGAIN_ALBUM_GAIN"),
        (&mut rg.album_peak, "REPLAYGAIN_ALBUM_PEAK"),
    ] {
        if let Some(value) = get(key).as_deref().and_then(gain_value) {
            *slot = Some(value);
        }
    }
    let start_ms = cd_frames_to_ms(cut.start);
    meta.duration_ms = match cut.end {
        Some(end) => Some(cd_frames_to_ms(end) - start_ms),
        None => base
            .duration_ms
            .and_then(|total| total.checked_sub(start_ms))
            .filter(|&d| d > 0),
    };
    if encoding.is_some() {
        meta.tag_encoding = encoding;
    }
    meta
}
