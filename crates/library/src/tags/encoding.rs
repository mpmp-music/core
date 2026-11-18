//! Repair of legacy-encoded tag text.
//!
//! ID3v1 is Latin-1 by definition and ID3v2.3 frames often claim Latin-1, but taggers in
//! China, Taiwan, Japan and Korea wrote the local code page (GBK, Big5, Shift_JIS, EUC-KR)
//! into those fields. A tag reader then hands us mojibake whose characters are all in
//! `U+0000..=U+00FF`; mapping them back to bytes recovers the original byte string, which we
//! detect and decode properly.

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use encoding_rs::{BIG5, EUC_JP, EUC_KR, Encoding, GB18030, GBK, SHIFT_JIS, WINDOWS_1252};

/// Whether `text` looks like bytes decoded as Latin-1: every char is `<= U+00FF` and at least
/// one is non-ASCII. Pure ASCII needs no repair; anything above `U+00FF` came from a Unicode
/// encoding and is trusted.
pub fn is_latin1_mojibake_candidate(text: &str) -> bool {
    let mut non_ascii = false;
    for c in text.chars() {
        let c = c as u32;
        if c > 0xFF {
            return false;
        }
        non_ascii |= c >= 0x80;
    }
    non_ascii
}

/// Legacy encodings that detection may pick. Single-byte guesses (windows-1250,
/// ISO-8859-4,...) are never accepted: for short Western text they are wrong far more often
/// than right, and the result would silently corrupt correct tags.
const DETECTABLE: [&Encoding; 6] = [GBK, GB18030, BIG5, SHIFT_JIS, EUC_KR, EUC_JP];

/// Re-decodes Latin-1 mojibake in the text fields of one file, in place.
///
/// Callers pass only text that was stored as Latin-1 (ID3v1, ID3v2 frames with encoding
/// byte 0); Unicode frames must not be passed. All candidate fields (see
/// [`is_latin1_mojibake_candidate`]) are detected together, which gives the detector enough
/// text to tell GBK from Big5 or Shift_JIS. Without `forced`, the fields are replaced only
/// when the guess is a CJK multibyte encoding (GBK, Big5, Shift_JIS, EUC-KR, EUC-JP), every
/// candidate decodes without errors, and the bytes look like double-byte text rather than
/// Western accents (at least two multibyte characters, one with a non-ASCII trail byte);
/// otherwise nothing changes. With `forced` (a per-root override), these checks are skipped
/// and each candidate that decodes cleanly is replaced.
///
/// Returns the encoding that was applied, or `None` if nothing changed.
pub fn repair_latin1_text(
    fields: &mut [String],
    forced: Option<&'static Encoding>,
) -> Option<&'static Encoding> {
    let candidates: Vec<usize> = (0..fields.len())
        .filter(|&i| is_latin1_mojibake_candidate(&fields[i]))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let bytes: Vec<Vec<u8>> = candidates
        .iter()
        .map(|&i| fields[i].chars().map(|c| c as u32 as u8).collect())
        .collect();

    let encoding = match forced {
        Some(encoding) => encoding,
        None => {
            let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
            for field in &bytes {
                detector.feed(field, false);
                // A separator keeps one field's trailing lead byte from pairing with the
                // next field's first byte.
                detector.feed(b"\n", false);
            }
            detector.feed(&[], true);
            let guess = detector.guess(None, Utf8Detection::Deny);
            if !DETECTABLE.contains(&guess) || !looks_double_byte(&bytes, guess) {
                return None;
            }
            guess
        }
    };

    let decoded: Vec<Option<String>> = bytes
        .iter()
        .map(|field| {
            encoding
                .decode_without_bom_handling_and_without_replacement(field)
                .map(|text| text.into_owned())
        })
        .collect();
    if forced.is_none() && decoded.iter().any(Option::is_none) {
        return None;
    }

    let mut changed = false;
    for (&index, text) in candidates.iter().zip(decoded) {
        if let Some(text) = text
            && text != fields[index]
        {
            fields[index] = text;
            changed = true;
        }
    }
    changed.then_some(encoding)
}

/// Whether `fields` are plausibly double-byte text in `encoding` rather than Western
/// Latin-1 text: at least two multibyte characters, and at least one whose trail byte is
/// non-ASCII. Latin-1 accents are almost always followed by an ASCII letter ("Ñu", "Naïve",
/// "Ágævis"), which a double-byte reading turns into lead + ASCII trail; common GBK, Big5,
/// Shift_JIS and EUC text has many characters with both bytes `>= 0x80`.
fn looks_double_byte(fields: &[Vec<u8>], encoding: &'static Encoding) -> bool {
    let (mut chars, mut high_trail) = (0, 0);
    for field in fields {
        let mut i = 0;
        while i < field.len() {
            let lead = field[i];
            if lead < 0x80 {
                i += 1;
                continue;
            }
            chars += 1;
            if encoding == SHIFT_JIS && (0xA1..=0xDF).contains(&lead) {
                // Half-width katakana: a single byte.
                i += 1;
            } else if encoding == EUC_JP && lead == 0x8F {
                // JIS X 0212: three bytes, all high.
                high_trail += 1;
                i += 3;
            } else {
                if field.get(i + 1).is_some_and(|&trail| trail >= 0x80) {
                    high_trail += 1;
                }
                i += 2;
            }
        }
    }
    chars >= 2 && high_trail >= 1
}

/// Decodes a text file (M3U playlists). A byte-order mark (UTF-8 or UTF-16) decides; else
/// with `utf8` (`.m3u8`) or when the bytes are valid UTF-8, UTF-8 (invalid sequences become
/// U+FFFD); else the bytes are detected like legacy tag text: a CJK multibyte encoding
/// (GBK, Big5, Shift_JIS, EUC-KR, EUC-JP) when the whole file looks like one, Windows-1252
/// otherwise (it never fails, and Western playlists from Windows use it).
pub(crate) fn decode_text_file(bytes: &[u8], utf8: bool) -> String {
    decode_text(bytes, utf8, None).0
}

/// [`decode_text_file`], with `forced` (a root's encoding) replacing the detection of a
/// legacy encoding; also returns the legacy encoding used (`None` for UTF-8 and UTF-16).
pub(crate) fn decode_text(
    bytes: &[u8],
    utf8: bool,
    forced: Option<&'static Encoding>,
) -> (String, Option<&'static Encoding>) {
    if let Some((encoding, bom)) = Encoding::for_bom(bytes) {
        let text = encoding.decode_without_bom_handling(&bytes[bom..]).0;
        return (text.into_owned(), None);
    }
    if utf8 || std::str::from_utf8(bytes).is_ok() {
        return (String::from_utf8_lossy(bytes).into_owned(), None);
    }
    let encoding = forced.unwrap_or_else(|| {
        let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
        detector.feed(bytes, true);
        let guess = detector.guess(None, Utf8Detection::Deny);
        if DETECTABLE.contains(&guess)
            && looks_double_byte(std::slice::from_ref(&bytes.to_vec()), guess)
            && guess
                .decode_without_bom_handling_and_without_replacement(bytes)
                .is_some()
        {
            guess
        } else {
            WINDOWS_1252
        }
    });
    let text = encoding.decode_without_bom_handling(bytes).0.into_owned();
    (text, Some(encoding))
}

/// Resolves a user-supplied encoding label (`"gbk"`, `"shift_jis"`, `"big5"`,...) using the
/// WHATWG Encoding Standard names that `encoding_rs` understands.
pub fn encoding_for_label(label: &str) -> Option<&'static Encoding> {
    Encoding::for_label(label.trim().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a tag reader produces when `text` was stored in `encoding` but read as Latin-1.
    fn mojibake(text: &str, encoding: &'static Encoding) -> String {
        let (bytes, _, unmappable) = encoding.encode(text);
        assert!(
            !unmappable,
            "{text} not representable in {}",
            encoding.name()
        );
        bytes.iter().map(|&b| b as char).collect()
    }

    fn repair(
        originals: &[&str],
        encoding: &'static Encoding,
    ) -> (Vec<String>, Option<&'static Encoding>) {
        let mut fields: Vec<String> = originals.iter().map(|t| mojibake(t, encoding)).collect();
        let applied = repair_latin1_text(&mut fields, None);
        (fields, applied)
    }

    #[test]
    fn candidate_rules() {
        assert!(!is_latin1_mojibake_candidate("plain ascii"));
        assert!(!is_latin1_mojibake_candidate("周砚舟"));
        assert!(!is_latin1_mojibake_candidate("Ã© mixed 周"));
        assert!(is_latin1_mojibake_candidate("Café"));
        assert!(is_latin1_mojibake_candidate("\u{d6}\u{dc}"));
    }

    #[test]
    fn repairs_gbk() {
        let originals = ["三棱雾", "周砚舟", "三棱雾"];
        let (fields, applied) = repair(&originals, GBK);
        assert_eq!(fields, originals);
        assert_eq!(applied, Some(GBK));
    }

    #[test]
    fn repairs_big5() {
        let originals = ["星橋漫步", "星舟團", "雲盒航圖"];
        let (fields, applied) = repair(&originals, BIG5);
        assert_eq!(fields, originals);
        assert_eq!(applied, Some(BIG5));
    }

    #[test]
    fn repairs_shift_jis() {
        let originals = ["星箱桜", "星野ノノ", "ホシバコ曲集"];
        let (fields, applied) = repair(&originals, SHIFT_JIS);
        assert_eq!(fields, originals);
        assert_eq!(applied, Some(SHIFT_JIS));
    }

    #[test]
    fn repairs_euc_kr() {
        let originals = ["사랑해요", "이노라", "별상자"];
        let (fields, applied) = repair(&originals, EUC_KR);
        assert_eq!(fields, originals);
        assert_eq!(applied, Some(EUC_KR));
    }

    #[test]
    fn keeps_real_latin1() {
        // Single-byte guesses (windows-1250, ISO-8859-4) and CJK guesses for accented Latin
        // text (Big5 for "Ñu") must all leave the tags alone.
        let files: [&[&str]; 8] = [
            &["Café des Nues", "Vélinéa", "Déjà Nu"],
            &["Ágævis nyrven", "Sivar Nól"],
            &["Ágævis nyrven"],
            &["Naïve"],
            &["Ñu"],
            &["Grüße aus Nölm", "Die Ärlte"],
            &["Motörvale", "Arc of Spires"],
            &["Señor Nebulín", "El Baile Nuván"],
        ];
        for originals in files {
            let mut fields: Vec<String> = originals.iter().map(|s| s.to_string()).collect();
            assert_eq!(repair_latin1_text(&mut fields, None), None, "{originals:?}");
            assert_eq!(fields, originals);
        }
    }

    #[test]
    fn double_byte_shape() {
        let gbk = GBK.encode("三棱雾").0.into_owned();
        assert!(looks_double_byte(&[gbk], GBK));
        assert!(!looks_double_byte(&[b"\xd1u".to_vec()], BIG5));
        assert!(!looks_double_byte(&[b"\xc1g\xe6vis".to_vec()], GBK));
    }

    #[test]
    fn leaves_ascii_and_unicode_fields_alone() {
        let mut fields = vec![
            "Track 01".to_owned(),
            mojibake("周砚舟", GBK),
            "晶港".to_owned(),
            mojibake("叶窗集", GBK),
        ];
        assert_eq!(repair_latin1_text(&mut fields, None), Some(GBK));
        assert_eq!(fields, ["Track 01", "周砚舟", "晶港", "叶窗集"]);
    }

    #[test]
    fn forced_encoding_skips_detection() {
        // Two characters are too little for reliable detection; the per-root override wins.
        let mut fields = vec![mojibake("晶港", BIG5)];
        assert_eq!(repair_latin1_text(&mut fields, Some(BIG5)), Some(BIG5));
        assert_eq!(fields, ["晶港"]);

        // A field that does not decode under the forced encoding is kept as is.
        let mut fields = vec!["\u{ff}\u{ff}".to_owned(), mojibake("星箱桜", SHIFT_JIS)];
        assert_eq!(
            repair_latin1_text(&mut fields, Some(SHIFT_JIS)),
            Some(SHIFT_JIS)
        );
        assert_eq!(fields, ["\u{ff}\u{ff}", "星箱桜"]);
    }

    #[test]
    fn malformed_bytes_are_not_replaced() {
        // A GBK lead byte followed by an invalid trail byte.
        let mut fields = vec!["\u{81}\u{7f}\u{81}\u{7f}\u{81}\u{7f}\u{81}\u{7f}".to_owned()];
        let before = fields.clone();
        match repair_latin1_text(&mut fields, None) {
            None => assert_eq!(fields, before),
            Some(_) => assert!(!fields[0].contains('\u{fffd}')),
        }
    }

    #[test]
    fn labels() {
        assert_eq!(encoding_for_label("GBK"), Some(GBK));
        assert_eq!(encoding_for_label(" shift_jis "), Some(SHIFT_JIS));
        assert_eq!(encoding_for_label("big5"), Some(BIG5));
        assert_eq!(encoding_for_label("nope"), None);
    }
}
