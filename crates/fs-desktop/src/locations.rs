//! Location strings on desktop (contract guarantee 16): absolute file system paths. Core
//! never interprets locations; the desktop assembly gives it these rules (core's
//! `Locations` hook, used for permission scopes: docs/design/agent-permissions.md).

use std::path::{Component, Path, PathBuf};

use crate::{paths, unc};

/// Desktop locations. Their normal form ([`DesktopLocations::normalize`]) is what the
/// operating system would act on: the longest part of the path that exists, in canonical
/// form (symlinks and `..` resolved as the system resolves them), then the rest as written.
/// Letter case is ignored where the file system ignores it, as for library roots.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DesktopLocations;

impl DesktopLocations {
    /// The normal form of a location: the canonical form of its longest existing part
    /// (resolved by the operating system: `link/..` is the parent of the link's target, not
    /// the folder holding the link), without Windows' `\\?\` prefix, and the rest as written.
    /// `None` for a path that is relative, empty or not Unicode, whose part that does not
    /// exist (yet) contains `..`, or that runs through something that exists but cannot be
    /// resolved (a symlink to nothing, a folder that cannot be read).
    ///
    /// On Windows also `None` for a name Windows would read as something else
    /// ([`windows_name`]: a stream `x.m3u:s`, a device `CON`, a name ending in `.` or a
    /// space); and a UNC path back to this computer (`\\localhost\C$\…`,
    /// `\\127.0.0.1\c$\…`, `\\<this computer>\<share>\…`, a drive mapped to one) is the
    /// local path it names, in that path's normal form, when the share's local folder can
    /// be told (`X$` is `X:\`; other shares are asked of Windows); else it stays a UNC path
    /// (see [`DesktopLocations::contains`]).
    pub fn normalize(&self, location: &str) -> Option<String> {
        resolve(location, 2, true)
    }

    /// Whether `location` can name a place at all, told from its text alone (no file
    /// system call): absolute, not empty, with no `..` part (a place named straight, not by
    /// a way back up; a `.` part inside the path is dropped, as `Path` does, except in a
    /// `\\?\` path, where it is refused too), no control or text-direction character (they
    /// would show the user something else in a confirmation), and on Windows only names
    /// Windows reads as written ([`windows_name`]).
    pub fn well_formed(&self, location: &str) -> bool {
        let path = Path::new(location);
        !location.is_empty()
            && !location.chars().any(hidden_char)
            && path.is_absolute()
            && path.components().all(|c| match c {
                Component::CurDir | Component::ParentDir => false,
                // A verbatim path (`\\?\…`) keeps them as names.
                Component::Normal(name) => {
                    name != "."
                        && name != ".."
                        && (!cfg!(windows) || name.to_str().is_some_and(windows_name))
                }
                Component::Prefix(_) | Component::RootDir => true,
            })
    }

    /// Where a well-formed location that [`DesktopLocations::normalize`] cannot read would
    /// be, by its name as far as the file system tells: the canonical form of its longest
    /// part that resolves, then the rest as written (a link to nothing, a folder that cannot
    /// be read, are passed as names); the location as written when no part resolves (a
    /// drive or share that is not there). `None` for a location that is not well formed.
    pub fn nearest(&self, location: &str) -> Option<String> {
        if !self.well_formed(location) {
            return None;
        }
        resolve(location, 2, false).or_else(|| {
            paths::strip_verbatim(PathBuf::from(location))
                .into_os_string()
                .into_string()
                .ok()
        })
    }

    /// Whether `location` is `scope` or inside it; both in normal form (as
    /// [`DesktopLocations::normalize`] returns them; they are compared as given). Relative
    /// or empty paths are never contained (nor contain anything). Where only letter case
    /// differs, it counts when the file system ignores case there, and `if_unsure` when that
    /// cannot be told.
    ///
    /// On Windows, when `if_unsure`: a UNC path whose host is or may be this computer, but
    /// that could not be told as a local path (a share Windows does not say the folder of,
    /// a host spelled in a way that cannot be told without the network), may be any local
    /// folder, so it contains and is contained by anything.
    pub fn contains(&self, scope: &str, location: &str, if_unsure: bool) -> bool {
        let (scope, location) = (Path::new(scope), Path::new(location));
        if !scope.is_absolute() || !location.is_absolute() {
            return false;
        }
        if location.starts_with(scope) {
            return true;
        }
        if cfg!(windows)
            && if_unsure
            && [scope, location]
                .iter()
                .any(|p| unc::may_be_local(&p.to_string_lossy()))
        {
            return true;
        }
        let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        lower(location).starts_with(lower(scope))
            && self
                .ignores_case(&scope.to_string_lossy())
                .unwrap_or(if_unsure)
    }

    /// `relative` (a `/`-separated location relative to `location`, as a library prefix) as
    /// a path inside `location`; `None` when a part of it is empty, `.`, `..` or holds a
    /// separator, on Windows also a name that cannot be a folder's ([`windows_name`]: a `:`
    /// as in a drive or a stream name, `Jazz:x` being a stream of `Jazz`; a device name;
    /// a trailing `.` or space), or `location` is not absolute.
    pub fn child(&self, location: &str, relative: &str) -> Option<String> {
        let mut path = PathBuf::from(location);
        if location.is_empty() || !path.is_absolute() {
            return None;
        }
        for part in relative.split('/') {
            if cfg!(windows) && !windows_name(part) {
                return None;
            }
            let mut one = Path::new(part).components();
            match (one.next(), one.next()) {
                (Some(Component::Normal(name)), None) if name == part => path.push(name),
                _ => return None,
            }
        }
        path.into_os_string().into_string().ok()
    }

    /// Whether letter case is ignored for names at `location` (an absolute path): probed at
    /// its longest existing part, by the file system that holds it (inside it, for a folder
    /// that is a mount point; a name without letters is probed through the folders above
    /// it, or the names inside it). `None` when that cannot be told (see
    /// `paths::case_insensitive_fs`).
    pub fn ignores_case(&self, location: &str) -> Option<bool> {
        let path = Path::new(location);
        if !path.is_absolute() {
            return None;
        }
        let existing = path
            .ancestors()
            .find(|p| std::fs::symlink_metadata(p).is_ok())?;
        paths::case_insensitive_fs(existing)
    }

    /// The location as the user should see it: its normal form, or the string as given
    /// when it has none.
    pub fn display(&self, location: &str) -> String {
        self.normalize(location)
            .unwrap_or_else(|| location.to_owned())
    }
}

/// [`DesktopLocations::normalize`] (`strict`), or [`DesktopLocations::nearest`] (not
/// `strict`: a part that exists but cannot be resolved is taken as a name, and a shorter
/// part is tried); `hops`: how many more times a UNC path may be turned into a local one
/// (once is enough; this only bounds it).
fn resolve(location: &str, hops: u8, strict: bool) -> Option<String> {
    // Back to this computer: judged (and acted on) as the local path, without asking the
    // network (whose names in that path are then checked as any).
    if cfg!(windows)
        && let Some(local) = unc::local_path(location)
    {
        return resolve(&local, hops.checked_sub(1)?, strict);
    }
    let path = Path::new(location);
    if location.is_empty() || !path.is_absolute() {
        return None;
    }
    let components: Vec<Component<'_>> = path.components().collect();
    if cfg!(windows)
        && components.iter().any(|c| match c {
            Component::Normal(name) => !name.to_str().is_some_and(windows_name),
            _ => false,
        })
    {
        return None;
    }
    for existing in (1..=components.len()).rev() {
        let prefix: PathBuf = components[..existing].iter().collect();
        match std::fs::canonicalize(&prefix) {
            Ok(canonical) => {
                let mut out = paths::strip_verbatim(canonical);
                for rest in &components[existing..] {
                    match rest {
                        Component::Normal(name) => out.push(name),
                        // `..` (or `.`) past what exists: nothing to resolve it against.
                        _ => return None,
                    }
                }
                let out = out.into_os_string().into_string().ok()?;
                // A drive mapped to this computer, or a link to it, resolves to UNC.
                if cfg!(windows)
                    && let Some(local) = unc::local_path(&out)
                {
                    return resolve(&local, hops.checked_sub(1)?, strict);
                }
                return Some(out);
            }
            // Something is there (a dangling symlink, an unreadable folder …) but it
            // cannot be resolved: not a location to compare.
            Err(_) if strict && std::fs::symlink_metadata(&prefix).is_ok() => return None,
            Err(_) => {}
        }
    }
    None
}

/// A control character (NUL …) or one that changes the direction text is shown in (as
/// `U+202E` does): never in a location an agent names.
fn hidden_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Whether Windows reads the name as written, as a file or folder name: not with a `:` (a
/// drive, or a stream: `x.m3u:s` is a stream of `x.m3u`, `C:\X:s` one of the folder `X`),
/// a separator, a character Windows does not allow in names (`<>"|?*`, control characters),
/// not a device (`CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9`, also with
/// superscript digits, `CONIN$`, `CONOUT$`; with any extension, as `CON.txt`; in any letter
/// case), and not ending in `.` or a space (Windows drops them). Plain text: told the same
/// on every platform, used on Windows.
pub(crate) fn windows_name(name: &str) -> bool {
    // A separator in a name (inside a `\\?\` path only `\` separates parts, so
    // `Music/../Diary` would be one name there and a way back up to Windows' own path
    // functions), and the characters Windows does not allow in names.
    if name.contains([':', '/', '\\', '<', '>', '"', '|', '?', '*'])
        || name.chars().any(|c| c < ' ')
        || name.ends_with(['.', ' '])
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_uppercase();
    let device = match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => ["COM", "LPT"].iter().any(|port| {
            stem.strip_prefix(port).is_some_and(|n| {
                let mut chars = n.chars();
                matches!(
                    (chars.next(), chars.next()),
                    (Some('0'..='9' | '¹' | '²' | '³'), None)
                )
            })
        }),
    };
    !device
}

#[cfg(test)]
mod tests;
