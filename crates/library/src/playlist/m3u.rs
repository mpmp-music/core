//! M3U / M3U8 import and export (docs/design/queries-playlists-undo.md).
//!
//! Portable like `storage_access.rs`: no `#[cfg(target_os)]`; platform differences are decided by
//! [`MAIN_SEPARATOR`] and by probing the filesystem.

use std::collections::HashMap;
use std::collections::hash_map::Entry as MapEntry;
use std::path::{Component, MAIN_SEPARATOR, MAIN_SEPARATOR_STR, Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::playlist::ExportTrack;
use crate::tags::encoding::decode_text_file;
use crate::types::{RootId, TrackId};
use crate::{Result, db::reads, storage_access};

/// One playlist entry of an M3U file: the line as written and the paths it may name (none
/// if it is not a local file, e.g. `http://...`), most likely first.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    pub line: String,
    /// The path with `.` and `..` resolved lexically (symlinks kept: the scanner stores
    /// files under a symlinked folder by their path inside the root), then its canonical
    /// form when that differs.
    pub paths: Vec<PathBuf>,
}

/// Whether `path` has an M3U file name: `Some(true)` for `.m3u8` (UTF-8), `Some(false)` for
/// `.m3u`, ignoring letter case; `None` otherwise.
pub(crate) fn kind(path: &Path) -> Option<bool> {
    let ext = path.extension()?;
    if ext.eq_ignore_ascii_case("m3u8") {
        Some(true)
    } else if ext.eq_ignore_ascii_case("m3u") {
        Some(false)
    } else {
        None
    }
}

/// Reads and parses an M3U file: `.m3u8` is UTF-8; `.m3u` is UTF-8 when it decodes as such,
/// else detected like legacy tag text (GBK, Big5, Shift_JIS,...; Windows-1252 otherwise).
/// Only a regular file is read (else `not_a_regular_file`; opening never waits, see
/// [`storage_access::open_regular`]). `checked` ([`crate::ActionMeta::checked`]): the file opened
/// must be the one at `path`, which must still be in canonical form (else
/// `location_changed`, and nothing is read).
pub(crate) fn read(
    storage: &dyn mpmp_storage::LibraryStorage,
    path: &Path,
    checked: bool,
) -> Result<Vec<Entry>> {
    use std::io::Read;
    let utf8 = kind(path).unwrap_or(false);
    let mut opened = storage_access::open_regular(storage, path, checked)?;
    if checked {
        // Opened first, then compared: a symlink swapped in before the open is caught by
        // either check, one swapped in after it changes nothing (the file is open). The
        // path is not opened again: on Unix its entry is compared with the open file.
        storage_access::ensure_canonical(storage, path)?;
        let there = storage.entry_id(path).ok();
        if there != Some(storage.opened_id(&opened)?) {
            return Err(storage_access::location_changed(path));
        }
    }
    let mut bytes = Vec::new();
    opened.read_to_end(&mut bytes)?;
    let text = decode_text_file(&bytes, utf8);
    let base = std::path::absolute(path)?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    Ok(parse(storage, &text, &base))
}

/// Whether an entry line looks like a reference to media: a path or URL whose name ends in
/// an extension the scanner reads, or `.cue` (a URL's query and fragment aside). What an
/// agent may see of an imported file's lines ([`crate::ActionMeta::checked`]).
pub(crate) fn looks_like_media(line: &str) -> bool {
    let name = if line.contains("://") {
        line.split(['?', '#']).next().unwrap_or_default()
    } else {
        line
    };
    // Either separator: a playlist may come from another system.
    let name = name.rsplit(['/', '\\']).next().unwrap_or_default();
    name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty()
            && (ext.eq_ignore_ascii_case("cue")
                || crate::scan::AUDIO_EXTENSIONS
                    .iter()
                    .any(|known| ext.eq_ignore_ascii_case(known)))
    })
}

/// Entries of M3U text; comments (`#EXTM3U`, `#EXTINF`,...) and blank lines are skipped,
/// relative paths are resolved against `base`.
pub(crate) fn parse(
    storage: &dyn mpmp_storage::LibraryStorage,
    text: &str,
    base: &Path,
) -> Vec<Entry> {
    // The playlist's folder may itself be reached through a symlink.
    let canonical_base = storage_access::canonical(storage, base)
        .ok()
        .filter(|b| b != base);
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut paths = Vec::new();
            for base in std::iter::once(base).chain(canonical_base.as_deref()) {
                for path in entry_paths(storage, line, base) {
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
            }
            Entry {
                line: line.to_owned(),
                paths,
            }
        })
        .collect()
}

/// The lexical and the canonical path of an entry line (see [`Entry::paths`]).
fn entry_paths(
    storage: &dyn mpmp_storage::LibraryStorage,
    line: &str,
    base: &Path,
) -> Vec<PathBuf> {
    let Some(path) = entry_path(line, base) else {
        return Vec::new();
    };
    let lexical = lexical(&path);
    // Desktop canonical is exactly the original canonicalize + strip_verbatim pair.
    match storage_access::canonical(storage, &path) {
        Ok(canonical) if canonical != lexical => vec![lexical, canonical],
        _ => vec![lexical],
    }
}

fn entry_path(line: &str, base: &Path) -> Option<PathBuf> {
    let text = match strip_prefix_ignore_case(line, "file:") {
        Some(rest) => file_uri_path(rest)?,
        // Other URLs (http://...) are not files.
        None if line.contains("://") => return None,
        None => line.to_owned(),
    };
    // A playlist written on Windows uses `\`; on systems where `/` is the separator, read
    // it as a separator too (a `\` in a file name is rare there).
    let text = if MAIN_SEPARATOR == '/' {
        text.replace('\\', "/")
    } else {
        text
    };
    let path = Path::new(&text);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// The path of a `file:` URI (after the scheme): percent-decoded; `localhost` or an empty
/// host is the local machine, another host is a UNC share; `/C:/x` is the drive path `C:/x`.
fn file_uri_path(rest: &str) -> Option<String> {
    let path = match rest.strip_prefix("//") {
        Some(authority_and_path) => {
            let (host, path) = match authority_and_path.find('/') {
                Some(i) => authority_and_path.split_at(i),
                None => (authority_and_path, ""),
            };
            if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
                path.to_owned()
            } else {
                format!("//{host}{path}")
            }
        }
        None => rest.to_owned(),
    };
    let decoded = percent_decode(&path)?;
    let b = decoded.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        return Some(decoded[1..].to_owned());
    }
    Some(decoded)
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `path` with `.` and `..` resolved lexically.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `path`'s components after `root`, if it is inside `root` (letter case ignored on
/// case-insensitive filesystems).
fn relative_components(path: &Path, root: &Path, case_insensitive: bool) -> Option<Vec<String>> {
    let path: Vec<Component<'_>> = path.components().collect();
    let root: Vec<Component<'_>> = root.components().collect();
    if path.len() < root.len() {
        return None;
    }
    for (a, b) in path.iter().zip(&root) {
        let same = if case_insensitive {
            a.as_os_str().to_string_lossy().to_lowercase()
                == b.as_os_str().to_string_lossy().to_lowercase()
        } else {
            a == b
        };
        if !same {
            return None;
        }
    }
    path[root.len()..]
        .iter()
        .map(|c| match c {
            Component::Normal(name) => name.to_str().map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// Finds library tracks by file path, under the active roots.
pub(crate) struct Matcher {
    /// Id, comparable path, whether letter case is ignored for the root's own path (by the
    /// file system above it) and for the names inside it (Windows can make one folder
    /// case-sensitive on a volume that is not).
    roots: Vec<(RootId, PathBuf, bool, bool)>,
    /// Per root that ignores case inside: lower-cased relative path → track, built on
    /// first miss.
    folded: HashMap<RootId, HashMap<String, TrackId>>,
}

impl Matcher {
    pub fn new(storage: &dyn mpmp_storage::LibraryStorage, conn: &Connection) -> Result<Self> {
        let roots = reads::roots(conn)?
            .into_iter()
            .map(|root| {
                let path = storage_access::comparable_with(storage, &root.path);
                let ci = storage.case_insensitive_fs(&path) == Some(true);
                let inside = storage.case_insensitive_inside(&path) == Some(true);
                (root.id, path, ci, inside)
            })
            .collect();
        Ok(Self {
            roots,
            folded: HashMap::new(),
        })
    }

    /// The track at the first of `paths` that names one.
    pub fn find_any(&mut self, conn: &Connection, paths: &[PathBuf]) -> Result<Option<TrackId>> {
        for path in paths {
            if let Some(id) = self.find(conn, path)? {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    pub fn find(&mut self, conn: &Connection, path: &Path) -> Result<Option<TrackId>> {
        for &(root, ref root_path, ci, inside) in &self.roots {
            let Some(rel) = relative_components(path, root_path, ci) else {
                continue;
            };
            if rel.is_empty() {
                continue;
            }
            let rel_path = rel.join("/");
            let exact = conn
                // A file cut by a CUE sheet: its first track (the file's own row is hidden).
                .prepare_cached(
                    "SELECT id FROM tracks WHERE root_id = ?1 AND rel_path = ?2
                     ORDER BY hidden, cue_number LIMIT 1",
                )?
                .query_row(params![root.0, rel_path], |row| row.get(0))
                .optional()?;
            if let Some(id) = exact {
                return Ok(Some(TrackId(id)));
            }
            if inside {
                let folded = match self.folded.entry(root) {
                    MapEntry::Occupied(entry) => entry.into_mut(),
                    MapEntry::Vacant(entry) => entry.insert(
                        conn.prepare(
                            "SELECT id, rel_path FROM tracks WHERE root_id = ?1
                             ORDER BY hidden DESC, cue_number DESC",
                        )?
                        .query_map([root.0], |row| {
                            Ok((
                                row.get::<_, String>(1)?.to_lowercase(),
                                TrackId(row.get(0)?),
                            ))
                        })?
                        .collect::<rusqlite::Result<_>>()?,
                    ),
                };
                if let Some(&id) = folded.get(&rel_path.to_lowercase()) {
                    return Ok(Some(id));
                }
            }
            return Ok(None);
        }
        Ok(None)
    }
}

/// M3U text for `tracks`, to be written at `m3u_path`: `#EXTM3U`, then per track an
/// `#EXTINF:<seconds>,<artist> - <title>` line and its path, relative to the M3U file's
/// folder when that folder is inside the track's root, else absolute.
pub(crate) fn format(
    storage: &dyn mpmp_storage::LibraryStorage,
    tracks: &[ExportTrack],
    m3u_dir: &Path,
) -> String {
    let m3u_dir = storage_access::comparable_with(storage, m3u_dir);
    let mut roots: HashMap<&str, Option<Vec<String>>> = HashMap::new();
    let mut out = String::from("#EXTM3U\n");
    for track in tracks {
        let seconds = track.duration_ms.map_or(-1, |ms| (ms.max(0) + 500) / 1000);
        let title = track
            .title
            .clone()
            .unwrap_or_else(|| crate::types::file_stem(&track.rel_path).to_owned());
        let label = match &track.artist {
            Some(artist) => format!("{artist} - {title}"),
            None => title,
        };
        let label: String = label
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        out.push_str(&format!("#EXTINF:{seconds},{label}\n"));

        // The M3U folder's place inside this track's root, if it is inside.
        let dir_in_root = roots.entry(&track.root).or_insert_with(|| {
            let root = storage_access::comparable_with(storage, Path::new(&track.root));
            let ci = storage.case_insensitive_fs(&root) == Some(true);
            let inside = storage.case_insensitive_inside(&root) == Some(true);
            relative_components(&m3u_dir, &root, ci).map(|dir| {
                if inside {
                    dir.into_iter().map(|c| c.to_lowercase()).collect()
                } else {
                    dir
                }
            })
        });
        let line = match dir_in_root {
            Some(dir) => {
                let line = relative_line(dir, &track.rel_path);
                // Import skips comment lines and trims: keep such a name a path.
                if line.starts_with('#') || line.starts_with(char::is_whitespace) {
                    format!(".{MAIN_SEPARATOR}{line}")
                } else {
                    line
                }
            }
            None => {
                let mut path = PathBuf::from(&track.root);
                path.extend(track.rel_path.split('/'));
                path.to_string_lossy().into_owned()
            }
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// The path from folder `dir` (components inside the root; lower case on case-insensitive
/// filesystems) to `rel_path` (inside the same root), with the platform's separator.
fn relative_line(dir: &[String], rel_path: &str) -> String {
    let target: Vec<&str> = rel_path.split('/').collect();
    let ci_same = |a: &str, b: &String| a == b || a.to_lowercase() == *b;
    let common = dir
        .iter()
        .zip(&target[..target.len() - 1])
        .take_while(|(d, t)| ci_same(t, d))
        .count();
    let mut parts: Vec<&str> = vec![".."; dir.len() - common];
    parts.extend(&target[common..]);
    parts.join(MAIN_SEPARATOR_STR)
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::*;

    #[test]
    fn file_uris() {
        assert_eq!(
            file_uri_path("///home/me/a%20b/%E6%99%B6%E6%B8%AF.flac").as_deref(),
            Some("/home/me/a b/晶港.flac")
        );
        assert_eq!(
            file_uri_path("//localhost/x/y.mp3").as_deref(),
            Some("/x/y.mp3")
        );
        assert_eq!(
            file_uri_path("///C:/Music/x.mp3").as_deref(),
            Some("C:/Music/x.mp3")
        );
        assert_eq!(
            file_uri_path("//nas/share/x.mp3").as_deref(),
            Some("//nas/share/x.mp3")
        );
        assert_eq!(file_uri_path("/plain").as_deref(), Some("/plain"));
        assert_eq!(file_uri_path("///bad%zz"), None);
        assert_eq!(file_uri_path("///bad%ff"), None);
    }

    #[test]
    fn parse_skips_comments_and_resolves_relative() {
        let base = std::env::temp_dir().join("no-such-dir-m3u-test");
        let text =
            "\u{feff}#EXTM3U\n#EXTINF:10,A - B\nsub/../a.flac\n\n  ./b.mp3  \nhttp://x/y.mp3\n";
        let entries = parse(
            &mpmp_fs_desktop::DesktopStorage,
            text.trim_start_matches('\u{feff}'),
            &base,
        );
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].line, "sub/../a.flac");
        assert_eq!(entries[0].paths, [base.join("a.flac")]);
        assert_eq!(entries[1].line, "./b.mp3");
        assert_eq!(entries[1].paths, [base.join("b.mp3")]);
        // Not a file: kept, so the import reports it as missing.
        assert_eq!(entries[2].line, "http://x/y.mp3");
        assert!(entries[2].paths.is_empty());
    }

    #[test]
    fn m3u_kinds() {
        assert_eq!(kind(Path::new("a/x.M3U8")), Some(true));
        assert_eq!(kind(Path::new("x.m3u")), Some(false));
        assert_eq!(kind(Path::new("x.Mp3")), None);
        assert_eq!(kind(Path::new("x")), None);
        assert_eq!(kind(Path::new("m3u")), None);
    }

    #[test]
    fn relative_lines() {
        let sep = MAIN_SEPARATOR.to_string();
        let dir = |d: &[&str]| d.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            relative_line(&dir(&[]), "a/b.flac"),
            ["a", "b.flac"].join(&sep)
        );
        assert_eq!(
            relative_line(&dir(&["lists"]), "a/b.flac"),
            ["..", "a", "b.flac"].join(&sep)
        );
        assert_eq!(relative_line(&dir(&["a"]), "a/b.flac"), "b.flac");
        assert_eq!(
            relative_line(&dir(&["a", "x"]), "a/b.flac"),
            ["..", "b.flac"].join(&sep)
        );
        // The file name is never treated as a folder.
        assert_eq!(
            relative_line(&dir(&["b.flac"]), "b.flac"),
            ["..", "b.flac"].join(&sep)
        );
    }
}
