//! Search text normalization and query building.
//!
//! Everything that is indexed or compared goes through [`normalize`] first, so full-width and
//! half-width forms, compatibility characters, case and accents all compare equal.

use unicode_normalization::UnicodeNormalization;

/// Normalizes text into a search key: NFKD, lower case, accents dropped, recomposed (NFC),
/// whitespace runs collapsed to one space and trimmed. Display text is never changed.
///
/// - Compatibility decomposition maps full-width Latin (`ＡＢＣ`), half-width katakana
///   (half-width katakana) and ideographic spaces to their ordinary forms.
/// - Accent folding drops the combining diacritical marks (U+0300–036F and the extended
///   diacritic blocks), so `Étude` matches `etude` and `Dvořál` matches `dvoral`; a few
///   letters without a decomposition are spelled out (`ø`→`o`, `ł`→`l`, `ß`→`ss`, `æ`→`ae`,
///   ...). Marks of other scripts are kept, including kana voicing marks that distinguish
///   voiced and unvoiced kana.
/// - Recomposition keeps Hangul syllables and voiced kana as single characters, so the
///   trigram index and the one/two-character fallback see the same units as the user.
/// - CJK characters themselves are left alone (no simplified ↔ traditional folding).
///
/// Control characters count as whitespace, so normalized text never contains the separator
/// used inside [`crate::AlbumKey`].
pub fn normalize(text: &str) -> String {
    // Lower case before the decomposition (context-sensitive rules such as final sigma, and
    // `İ` → `i̇`) and again after it: compatibility characters decompose to capitals
    // (`™` → `TM`, `№` → `No`, `℃` → `°C`).
    let lowered = text.to_lowercase();
    let mut folded = String::with_capacity(lowered.len());
    for c in lowered.nfkd().flat_map(char::to_lowercase) {
        if is_diacritic(c) {
            continue;
        }
        match c {
            'ø' => folded.push('o'),
            'đ' | 'ð' => folded.push('d'),
            'ł' => folded.push('l'),
            'ħ' => folded.push('h'),
            'ı' => folded.push('i'),
            'ß' => folded.push_str("ss"),
            'æ' => folded.push_str("ae"),
            'œ' => folded.push_str("oe"),
            'þ' => folded.push_str("th"),
            c if c.is_control() => folded.push(' '),
            c => folded.push(c),
        }
    }
    let lowered: String = folded.nfc().collect();
    let mut out = String::with_capacity(lowered.len());
    for word in lowered.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Combining diacritical marks folded away by [`normalize`].
fn is_diacritic(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'
        | '\u{1AB0}'..='\u{1AFF}'
        | '\u{1DC0}'..='\u{1DFF}'
        | '\u{20D0}'..='\u{20FF}'
        | '\u{FE20}'..='\u{FE2F}',
    )
}

/// Shortest term the FTS5 trigram tokenizer can match; shorter terms fall back to `LIKE`.
pub(crate) const TRIGRAM_MIN_CHARS: usize = 3;

/// A search query split into terms that every result must contain (AND semantics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchQuery {
    /// Terms of three or more characters, matched through the trigram index.
    pub fts_terms: Vec<String>,
    /// Terms of one or two characters, matched by substring scan (the `LIKE` fallback).
    pub like_terms: Vec<String>,
}

impl SearchQuery {
    /// Normalizes `text` and splits it on whitespace. Returns `None` for an empty query.
    pub fn parse(text: &str) -> Option<Self> {
        let normalized = normalize(text);
        let mut query = SearchQuery {
            fts_terms: Vec::new(),
            like_terms: Vec::new(),
        };
        for term in normalized.split(' ').filter(|t| !t.is_empty()) {
            let target = if term.chars().count() >= TRIGRAM_MIN_CHARS {
                &mut query.fts_terms
            } else {
                &mut query.like_terms
            };
            if !target.iter().any(|t| t == term) {
                target.push(term.to_owned());
            }
        }
        if query.fts_terms.is_empty() && query.like_terms.is_empty() {
            None
        } else {
            Some(query)
        }
    }

    /// All terms, long and short.
    pub fn all_terms(&self) -> Vec<&str> {
        self.fts_terms
            .iter()
            .chain(&self.like_terms)
            .map(String::as_str)
            .collect()
    }

    /// The FTS5 MATCH expression for the long terms, optionally restricted to `columns`.
    /// `None` when every term is short.
    pub fn fts_expression(&self, columns: &[&str]) -> Option<String> {
        if self.fts_terms.is_empty() {
            return None;
        }
        let phrases: Vec<String> = self.fts_terms.iter().map(|t| fts_phrase(t)).collect();
        let body = phrases.join(" AND ");
        Some(if columns.is_empty() {
            body
        } else {
            format!("{{{}}} : ({body})", columns.join(" "))
        })
    }
}

/// Quotes a term as an FTS5 string literal: the whole term is one phrase, so operators and
/// punctuation inside it have no special meaning.
pub(crate) fn fts_phrase(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_width_and_case() {
        assert_eq!(normalize("ＡＢＣ　Ｓｏｎｇ"), "abc song");
        assert_eq!(normalize("  Hello\tWORLD  "), "hello world");
        assert_eq!(normalize("ｶﾀｶﾅ"), "カタカナ");
        assert_eq!(normalize("Ⅻ"), "xii");
        assert_eq!(normalize("周砚舟"), "周砚舟");
        assert_eq!(normalize("Ǆ"), "dz");
        assert_eq!(normalize("a\u{1f}b\u{0}"), "a b");
        // Compatibility characters that decompose to capitals.
        assert_eq!(normalize("Song™"), "songtm");
        assert_eq!(normalize("№5"), "no5");
        assert_eq!(normalize("20℃"), "20°c");
        assert_eq!(normalize("㎒"), "mhz");
    }

    #[test]
    fn normalize_folds_accents_only_where_meant() {
        assert_eq!(normalize("Étude"), "etude");
        assert_eq!(normalize("Dvořál"), "dvoral");
        assert_eq!(normalize("Ágævis Nyrven"), "agaevis nyrven");
        assert_eq!(normalize("Nörven Brüa"), "norven brua");
        assert_eq!(normalize("Sivar Nól"), "sivar nol");
        assert_eq!(normalize("Nøyksarn"), "noyksarn");
        assert_eq!(normalize("Straße"), "strasse");
        assert_eq!(normalize("İstanbul"), "istanbul");
        assert_eq!(normalize("Tiếng Việt"), "tieng viet");
        // Kana voicing marks are kept and recomposed; Hangul stays syllables.
        assert_eq!(normalize("ガンダム"), "ガンダム");
        assert_eq!(normalize("ｶﾞﾝﾀﾞﾑ"), "ガンダム");
        assert_ne!(normalize("ガ"), normalize("カ"));
        assert_eq!(normalize("사랑해요"), "사랑해요");
        assert_eq!(normalize("사랑해요").chars().count(), 4);
    }

    #[test]
    fn query_splits_by_length() {
        let q = SearchQuery::parse("晶港 周砚舟 ＡＢ").unwrap();
        assert_eq!(q.fts_terms, vec!["周砚舟"]);
        assert_eq!(q.like_terms, vec!["晶港", "ab"]);
        assert!(SearchQuery::parse("  　 ").is_none());
        let short = SearchQuery::parse("a").unwrap();
        assert!(short.fts_expression(&[]).is_none());
    }

    #[test]
    fn fts_expression_escapes_quotes_and_operators() {
        let q = SearchQuery::parse(r#"say "hi" OR NOT*"#).unwrap();
        assert_eq!(
            q.fts_expression(&[]).unwrap(),
            r#""say" AND """hi""" AND "not*""#
        );
        assert_eq!(
            q.fts_expression(&["artist_norm"]).unwrap(),
            r#"{artist_norm} : ("say" AND """hi""" AND "not*")"#
        );
    }
}
