//! Lyrics text parsing (docs/design/media-resources.md): plain text, LRC and
//! enhanced (word-timed) LRC into one structure. Pure: text in, struct out; finding the
//! text is [`crate::lyrics::find`]. Lenient, and never panics on any input.

pub(crate) mod find;

use serde::{Deserialize, Serialize};

/// Where the lyrics were found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LyricsSource {
    /// A `.lrc` file next to the track.
    File,
    /// The track's own tags.
    Embedded,
}

/// What the text looked like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LyricsFormat {
    /// No timestamps: lines have no times.
    Plain,
    /// Line timestamps (`[mm:ss.xx]`).
    Lrc,
    /// LRC with word timestamps (`<mm:ss.xx>`) on at least one line.
    EnhancedLrc,
}

/// A track's lyrics: parsed, with the text they were parsed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lyrics {
    pub source: LyricsSource,
    pub format: LyricsFormat,
    /// By time (stable); `Plain` keeps the text's order.
    pub lines: Vec<LyricsLine>,
    /// The decoded text, as found.
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricsLine {
    /// Milliseconds from the start of the track, `[offset:]` applied; `None` for `Plain`.
    pub start_ms: Option<u64>,
    /// Without timestamps; for word-timed lines the words put together.
    pub text: String,
    /// Word timing; empty when the line has none.
    pub words: Vec<LyricsWord>,
    /// A second line with the same timestamp (the Chinese players' convention); several
    /// are joined with `\n`.
    pub translation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricsWord {
    pub start_ms: u64,
    pub text: String,
}

/// Most line timestamps taken from one line (a chorus repeats a dozen times at most).
const MAX_STAMPS_PER_LINE: usize = 256;

/// Most text a timed result may hold in all its lines; expanding timestamps of a huge line
/// stops there.
const MAX_LINE_BYTES: usize = 8 << 20;

/// The longest tag (`[...]`, `<...>`) looked at; anything longer is text. Keeps scanning
/// linear on input like `[[[[[[`.
const MAX_TAG_LEN: usize = 40;

/// Parses `raw`. `None` when there is nothing to show: empty text, or only metadata tags.
pub fn parse(raw: &str, source: LyricsSource) -> Option<Lyrics> {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut offset = None;
    let mut entries = Vec::new();
    let mut texts = Vec::new();
    for line in raw.lines() {
        let (tags, rest) = leading_tags(line);
        for tag in &tags {
            if let Tag::Offset(value) = tag
                && offset.is_none()
            {
                offset = *value;
            }
        }
        // A timestamp written twice on a line is one.
        let mut times: Vec<u64> = Vec::new();
        for tag in &tags {
            if let Tag::Time(ms) = tag
                && times.len() < MAX_STAMPS_PER_LINE
                && !times.contains(ms)
            {
                times.push(*ms);
            }
        }
        if times.is_empty() {
            // Only metadata tags on the line: not text.
            if tags.is_empty() || !rest.trim().is_empty() {
                texts.push(if tags.is_empty() { line } else { rest });
            }
        } else {
            entries.push((times, rest));
        }
    }
    let (format, lines) = if entries.is_empty() {
        (LyricsFormat::Plain, plain_lines(&texts))
    } else {
        timed_lines(&entries, offset.unwrap_or(0))
    };
    if lines.is_empty() {
        return None;
    }
    Some(Lyrics {
        source,
        format,
        lines,
        raw: raw.to_owned(),
    })
}

fn plain_lines(texts: &[&str]) -> Vec<LyricsLine> {
    let mut lines: Vec<&str> = texts.iter().map(|t| t.trim()).collect();
    // Blank lines at the very start and end go; those between stanzas stay.
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let first = lines.iter().position(|l| !l.is_empty()).unwrap_or(0);
    lines[first..]
        .iter()
        .map(|text| LyricsLine {
            start_ms: None,
            text: (*text).to_owned(),
            words: Vec::new(),
            translation: None,
        })
        .collect()
}

fn timed_lines(entries: &[(Vec<u64>, &str)], offset: i64) -> (LyricsFormat, Vec<LyricsLine>) {
    // One entry per timestamp, in document order, then by time: stable, so lines that
    // share a time stay in the order they were written.
    let mut expanded: Vec<(u64, Line)> = Vec::new();
    let mut bytes = 0usize;
    'entries: for (times, rest) in entries {
        let line = Line::parse(rest);
        for &time in times {
            bytes = bytes.saturating_add(line.text.len());
            if bytes > MAX_LINE_BYTES {
                break 'entries;
            }
            expanded.push((time, line.clone()));
        }
    }
    expanded.sort_by_key(|(time, _)| *time);

    let shifted = |ms: u64| {
        u64::try_from(i64::try_from(ms).unwrap_or(i64::MAX).saturating_sub(offset)).unwrap_or(0)
    };
    let mut lines: Vec<LyricsLine> = Vec::new();
    let mut last = None;
    let mut enhanced = false;
    for (time, line) in expanded {
        // Grouped by the time as written: an offset that clamps several lines to 0 does
        // not make them translations of each other.
        if last == Some(time)
            && let Some(main) = lines.last_mut()
        {
            if !line.text.is_empty() {
                match &mut main.translation {
                    Some(translation) => {
                        translation.push('\n');
                        translation.push_str(&line.text);
                    }
                    none => *none = Some(line.text),
                }
            }
            continue;
        }
        last = Some(time);
        enhanced |= !line.words.is_empty();
        lines.push(LyricsLine {
            start_ms: Some(shifted(time)),
            text: line.text,
            words: line
                .words
                .into_iter()
                .map(|(start, text)| LyricsWord {
                    start_ms: shifted(start),
                    text,
                })
                .collect(),
            translation: None,
        });
    }
    let format = if enhanced {
        LyricsFormat::EnhancedLrc
    } else {
        LyricsFormat::Lrc
    };
    (format, lines)
}

/// A timed line's text with its word timestamps (as written, before the offset).
#[derive(Clone)]
struct Line {
    text: String,
    words: Vec<(u64, String)>,
}

impl Line {
    /// `rest`: what follows the line's timestamps; may hold `<mm:ss.xx>` word markers.
    fn parse(rest: &str) -> Line {
        let mut words: Vec<(u64, String)> = Vec::new();
        // Text before the first marker has no time of its own: it starts with the first
        // word, or is the whole line.
        let mut head = String::new();
        let mut rest = rest;
        let mut current: Option<u64> = None;
        let mut text = String::new();
        while !rest.is_empty() {
            let Some(at) = rest.find('<') else {
                text.push_str(rest);
                break;
            };
            match marker(&rest[at..]) {
                Some((time, len)) => {
                    text.push_str(&rest[..at]);
                    flush(&mut head, &mut words, current, &mut text);
                    current = Some(time);
                    rest = &rest[at + len..];
                }
                None => {
                    // A `<` that is not a marker is text.
                    text.push_str(&rest[..=at]);
                    rest = &rest[at + 1..];
                }
            }
        }
        flush(&mut head, &mut words, current, &mut text);
        if words.is_empty() {
            return Line {
                text: head.trim().to_owned(),
                words,
            };
        }
        // Text before the first marker takes the first word's time.
        if !head.trim().is_empty() {
            let time = words[0].0;
            words.insert(0, (time, head));
        }
        // The line's edges are trimmed, so its text is what the words add up to.
        if let Some(first) = words.first_mut() {
            first.1 = first.1.trim_start().to_owned();
        }
        if let Some(last) = words.last_mut() {
            last.1 = last.1.trim_end().to_owned();
        }
        words.retain(|(_, text)| !text.is_empty());
        let text = words.iter().map(|(_, w)| w.as_str()).collect();
        Line { text, words }
    }
}

/// Ends the word being read: `text` belongs to the marker before it, or is the head of
/// the line when there was none. Empty words (a closing marker) are dropped.
fn flush(head: &mut String, words: &mut Vec<(u64, String)>, time: Option<u64>, text: &mut String) {
    let taken = std::mem::take(text);
    match time {
        Some(time) if !taken.is_empty() => words.push((time, taken)),
        Some(_) => {}
        None => head.push_str(&taken),
    }
}

/// `<mm:ss.xx>` at the start of `s`: the time in ms and the marker's length.
fn marker(s: &str) -> Option<(u64, usize)> {
    let end = s
        .as_bytes()
        .iter()
        .take(MAX_TAG_LEN)
        .position(|&b| b == b'>')?;
    Some((timestamp(&s[1..end])?, end + 1))
}

enum Tag {
    /// A timestamp, in ms.
    Time(u64),
    /// `[offset:...]`: the value in ms, if it is a number.
    Offset(Option<i64>),
    /// Any other `[key:value]`.
    Meta,
}

/// The tags at the start of `line` (`[00:12.00][ti:Song]`, spaces between them allowed)
/// and what follows them.
fn leading_tags(line: &str) -> (Vec<Tag>, &str) {
    let mut tags = Vec::new();
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start();
        let Some(inner_start) = trimmed.strip_prefix('[') else {
            break;
        };
        let Some(end) = inner_start
            .as_bytes()
            .iter()
            .take(MAX_TAG_LEN)
            .position(|&b| b == b']')
        else {
            break;
        };
        let inner = &inner_start[..end];
        let tag = if let Some(ms) = timestamp(inner) {
            Tag::Time(ms)
        } else if let Some((key, value)) = inner.split_once(':')
            && !key.is_empty()
            && key.bytes().all(|b| b.is_ascii_alphabetic())
        {
            if key.eq_ignore_ascii_case("offset") {
                Tag::Offset(value.trim().parse::<i64>().ok())
            } else {
                Tag::Meta
            }
        } else {
            break;
        };
        tags.push(tag);
        rest = &inner_start[end + 1..];
    }
    (tags, rest)
}

/// `mm:ss`, `mm:ss.xx`, `mm:ss.xxx` or `mm:ss:xx` in ms. The minutes may exceed 59; a
/// fraction is a decimal fraction of a second (`.5` is 500 ms, `.05` is 50 ms).
fn timestamp(s: &str) -> Option<u64> {
    let (minutes, rest) = s.split_once(':')?;
    let (seconds, fraction) = match rest.split_once(['.', ':']) {
        Some((seconds, fraction)) => (seconds, Some(fraction)),
        None => (rest, None),
    };
    let digits = |s: &str, max: usize| {
        (!s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    let minutes = digits(minutes, 5)?;
    let seconds = digits(seconds, 2)?;
    let millis = match fraction {
        None => 0,
        Some(f) => {
            let value = digits(f, 3)?;
            // Right-padded to three digits: `5` is 500.
            value * 10u64.pow(3 - u32::try_from(f.len()).ok()?)
        }
    };
    Some((minutes * 60 + seconds) * 1000 + millis)
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Lyrics {
        parse(text, LyricsSource::File).expect("lyrics")
    }

    fn texts(lyrics: &Lyrics) -> Vec<&str> {
        lyrics.lines.iter().map(|l| l.text.as_str()).collect()
    }

    fn times(lyrics: &Lyrics) -> Vec<Option<u64>> {
        lyrics.lines.iter().map(|l| l.start_ms).collect()
    }

    #[test]
    fn timestamp_forms() {
        let l =
            parsed("[00:01]a\n[00:02.5]b\n[00:03.25]c\n[00:04.125]d\n[00:05:50]e\n[125:00.00]f");
        assert_eq!(l.format, LyricsFormat::Lrc);
        assert_eq!(
            times(&l),
            [1000, 2500, 3250, 4125, 5500, 7_500_000].map(Some)
        );
        assert_eq!(texts(&l), ["a", "b", "c", "d", "e", "f"]);
        assert_eq!(l.source, LyricsSource::File);
        assert!(l.lines.iter().all(|l| l.words.is_empty()));
    }

    #[test]
    fn malformed_timestamps_are_not_lines() {
        let l = parsed("[00:01.00]ok\n[0a:01.00]no\n[00:01.0000]no\n[00:01.00 no\n[1:2:3:4]no");
        assert_eq!(texts(&l), ["ok"]);
    }

    #[test]
    fn multiple_timestamps_expand_and_sort() {
        let l = parsed("[00:30.00]second\n[00:10.00][00:50.00] chorus \n[00:20.00]middle");
        assert_eq!(texts(&l), ["chorus", "middle", "second", "chorus"]);
        assert_eq!(times(&l), [10_000, 20_000, 30_000, 50_000].map(Some));
    }

    #[test]
    fn equal_times_stay_in_order() {
        let l = parsed("[00:10.00]a\n[00:05.00]b");
        assert_eq!(texts(&l), ["b", "a"]);
    }

    #[test]
    fn offset_shifts_and_clamps() {
        // Positive: the lyrics come earlier.
        let l = parsed("[offset:+500]\n[00:00.20]a\n[00:01.00]b\n[00:02.00]c");
        assert_eq!(times(&l), [0, 500, 1500].map(Some));
        // Negative: later. Words move with their line.
        let l = parsed("[offset:-1000]\n[00:01.00]<00:01.00>a<00:01.50>b");
        assert_eq!(l.lines[0].start_ms, Some(2000));
        assert_eq!(l.lines[0].words[1].start_ms, 2500);
        // The offset may come anywhere; a bad one is ignored; the first one counts.
        let l = parsed("[00:01.00]a\n[offset:200]\n[offset:900]");
        assert_eq!(l.lines[0].start_ms, Some(800));
        let l = parsed("[offset:soon]\n[00:01.00]a");
        assert_eq!(l.lines[0].start_ms, Some(1000));
        // Lines the offset clamps to 0 are not each other's translations.
        let l = parsed("[offset:5000]\n[00:01.00]a\n[00:02.00]b");
        assert_eq!(texts(&l), ["a", "b"]);
        assert_eq!(times(&l), [0, 0].map(Some));
        assert!(l.lines.iter().all(|l| l.translation.is_none()));
        // Absurd values saturate instead of overflowing.
        let l = parsed("[offset:-99999999999999999999]\n[00:01.00]a");
        assert_eq!(l.lines[0].start_ms, Some(1000));
        let l = parsed("[offset:9223372036854775807]\n[00:01.00]a");
        assert_eq!(l.lines[0].start_ms, Some(0));
        let l = parsed("[offset:-9223372036854775808]\n[00:01.00]a");
        assert!(l.lines[0].start_ms.is_some());
    }

    #[test]
    fn metadata_tags_are_not_lines() {
        let l = parsed(
            "[ar:Artist]\n[ti:Title]\n[al:Album]\n[by:me]\n[length:03:45]\n[re:tool]\n[ve:1.0]\n[xx:y]\n\
             [00:01.00]first",
        );
        assert_eq!(texts(&l), ["first"]);
        // Together with a timestamp on one line, and with other spellings.
        let l = parsed("[TI:Song][00:01.00]first\n[Offset: 0]");
        assert_eq!(texts(&l), ["first"]);
    }

    #[test]
    fn a_file_of_metadata_only_has_no_lyrics() {
        assert_eq!(
            parse("[ar:A]\n[ti:T]\n[length:03:00]\n", LyricsSource::File),
            None
        );
        assert_eq!(parse("", LyricsSource::File), None);
        assert_eq!(parse(" \n\t\n\r\n", LyricsSource::Embedded), None);
        assert_eq!(parse("\u{feff}", LyricsSource::Embedded), None);
        // Timestamps with nothing after them are empty lines, still lyrics (a pause).
        assert!(parse("[00:01.00]", LyricsSource::File).is_some());
    }

    #[test]
    fn lines_without_a_timestamp_are_dropped_from_timed_lyrics() {
        let l = parsed("intro text\n[Chorus]\n[00:01.00]a\nstray\n[00:02.00]b\n");
        assert_eq!(texts(&l), ["a", "b"]);
    }

    #[test]
    fn same_timestamp_is_a_translation() {
        let l = parsed(
            "[00:01.00]Hello\n[00:01.00]你好\n[00:02.00]World\n[00:02.00]世界\n[00:02.00](shi jie)\n[00:03.00]Alone",
        );
        assert_eq!(texts(&l), ["Hello", "World", "Alone"]);
        assert_eq!(l.lines[0].translation.as_deref(), Some("你好"));
        assert_eq!(l.lines[1].translation.as_deref(), Some("世界\n(shi jie)"));
        assert_eq!(l.lines[2].translation, None);
        // Repeated lines carry their translations to each time.
        let l = parsed("[00:01.00][00:09.00]La\n[00:01.00][00:09.00]啦");
        assert_eq!(texts(&l), ["La", "La"]);
        assert!(
            l.lines
                .iter()
                .all(|l| l.translation.as_deref() == Some("啦"))
        );
        // An empty duplicate adds nothing.
        let l = parsed("[00:01.00]A\n[00:01.00]");
        assert_eq!(
            (texts(&l), l.lines[0].translation.clone()),
            (vec!["A"], None)
        );
    }

    #[test]
    fn enhanced_words() {
        let l = parsed(
            "[00:01.00]<00:01.00>Hel<00:01.40>lo <00:02.00>world<00:02.50>\n[00:04.00]plain line",
        );
        assert_eq!(l.format, LyricsFormat::EnhancedLrc);
        let line = &l.lines[0];
        assert_eq!(line.text, "Hello world");
        assert_eq!(
            line.words,
            vec![
                LyricsWord {
                    start_ms: 1000,
                    text: "Hel".into()
                },
                LyricsWord {
                    start_ms: 1400,
                    text: "lo ".into()
                },
                LyricsWord {
                    start_ms: 2000,
                    text: "world".into()
                },
            ]
        );
        assert!(l.lines[1].words.is_empty());
        assert_eq!(l.lines[1].text, "plain line");
        // The words of an expanded line are shared; the translation has none.
        let l = parsed("[00:01.00][00:09.00]<00:01.00>a<00:01.50>b\n[00:01.00]翻译");
        assert_eq!(l.lines.len(), 2);
        assert_eq!(l.lines[0].translation.as_deref(), Some("翻译"));
        assert_eq!(l.lines[1].words.len(), 2);
    }

    #[test]
    fn enhanced_edges_and_odd_markers() {
        // Text before the first marker takes the first word's time; edges are trimmed.
        let l = parsed("[00:01.00]  lead <00:02.00> x <00:03.00>y  ");
        assert_eq!(l.lines[0].text, "lead  x y");
        assert_eq!(l.lines[0].words[0].start_ms, 2000);
        assert_eq!(l.lines[0].words[0].text, "lead ");
        // A `<` that is not a marker stays in the text.
        let l = parsed("[00:01.00]a < b <3 <00:01.50>c <zz> <00:xx>");
        assert_eq!(l.lines[0].text, "a < b <3 c <zz> <00:xx>");
        assert_eq!(l.lines[0].words.len(), 2);
        // Markers with nothing between them leave no empty word.
        let l = parsed("[00:01.00]<00:01.00><00:01.10>a");
        assert_eq!(l.lines[0].words.len(), 1);
        assert_eq!(l.lines[0].text, "a");
    }

    #[test]
    fn plain_lyrics() {
        let l = parsed("\n\nfirst line\r\n  second  \n\nthird\n\n\n");
        assert_eq!(l.format, LyricsFormat::Plain);
        assert_eq!(texts(&l), ["first line", "second", "", "third"]);
        assert!(
            l.lines
                .iter()
                .all(|l| l.start_ms.is_none() && l.words.is_empty())
        );
        // Section markers and brackets with no timestamp are text; metadata lines are not.
        let l = parsed("[ti:Song]\n[Verse 1]\nla <00:01.00> la\n[ar:X]");
        assert_eq!(texts(&l), ["[Verse 1]", "la <00:01.00> la"]);
    }

    #[test]
    fn raw_is_kept_without_the_bom() {
        let text = "\u{feff}[00:01.00]a\r\n";
        let l = parsed(text);
        assert_eq!(l.raw, "[00:01.00]a\r\n");
        assert_eq!(texts(&l), ["a"]);
    }

    #[test]
    fn huge_expansions_are_bounded() {
        let size = |l: &Lyrics| -> usize {
            let own = |l: &LyricsLine| l.text.len() + l.translation.as_ref().map_or(0, String::len);
            l.lines.iter().map(own).sum()
        };
        let stamps = |n: usize| (0..n).map(|i| format!("[{i}:00.00]")).collect::<String>();
        let l = parsed(&format!("{}{}", stamps(1000), "x".repeat(900_000)));
        assert!(l.lines.len() <= MAX_STAMPS_PER_LINE);
        assert!(size(&l) <= MAX_LINE_BYTES + 900_000, "{}", size(&l));
        // Many long lines, each repeated: the total stays bounded.
        let long = format!("{}{}\n", stamps(100), "y".repeat(40_000));
        let l = parsed(&long.repeat(20));
        assert!(size(&l) <= MAX_LINE_BYTES + 40_000, "{}", size(&l));
        // The same timestamp twice is one line.
        assert_eq!(parsed("[00:01.00][00:01.00]La").lines.len(), 1);
    }

    /// A small deterministic generator, so a failure can be replayed.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    #[test]
    fn odd_input_never_panics() {
        const PIECES: [&str; 28] = [
            "[",
            "]",
            "<",
            ">",
            ":",
            ".",
            "-",
            "+",
            "0",
            "1",
            "59",
            "99999",
            "00:01.00",
            "offset",
            "ti",
            "\n",
            "\r\n",
            " ",
            "\t",
            "字",
            "é",
            "\u{feff}",
            "\u{0}",
            "[00:",
            "<00:01.50>",
            "[offset:",
            "9223372036854775807",
            "-9223372036854775808",
        ];
        let mut rng = Lcg(7);
        for _ in 0..5000 {
            let count = rng.next() % 40;
            let text: String = (0..count)
                .map(|_| PIECES[(rng.next() % PIECES.len() as u64) as usize])
                .collect();
            let _ = parse(&text, LyricsSource::File);
        }
        // Arbitrary characters, long runs of one bracket.
        for _ in 0..2000 {
            let count = rng.next() % 60;
            let text: String = (0..count)
                .map(|_| char::from_u32((rng.next() % 0x3000) as u32).unwrap_or('?'))
                .collect();
            let _ = parse(&text, LyricsSource::Embedded);
        }
        for bracket in ["[", "]", "<", ">", "[00:01.00]", "<00:01.00>", "[a:"] {
            let _ = parse(&bracket.repeat(100_000), LyricsSource::File);
            let _ = parse(
                &format!("[00:01.00]{}", bracket.repeat(100_000)),
                LyricsSource::File,
            );
        }
    }
}
