use super::*;

fn text(p: &Path) -> String {
    p.to_str().unwrap().to_owned()
}

fn canonical(p: &Path) -> String {
    text(&paths::strip_verbatim(std::fs::canonicalize(p).unwrap()))
}

#[test]
fn folders_contain_what_is_inside_them() {
    let dir = tempfile::TempDir::new().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir_all(music.join("jazz")).unwrap();
    std::fs::create_dir(dir.path().join("musical")).unwrap();
    let l = DesktopLocations;
    let n = |p: &Path| l.normalize(&text(p)).unwrap();
    let m = n(&music);
    assert_eq!(m, canonical(&music));
    assert!(l.contains(&m, &m, false));
    assert!(l.contains(&m, &n(&music.join("jazz")), false));
    // Not created yet: a file to export, in a folder to come.
    assert!(l.contains(&m, &n(&music.join("jazz/new/mix.m3u")), false));
    assert!(l.contains(&m, &n(&dir.path().join("musical/../music/x.m3u")), false));
    // A sibling whose name starts the same is not inside.
    assert!(!l.contains(&m, &n(&dir.path().join("musical")), false));
    assert!(!l.contains(&m, &n(dir.path()), false));
    assert!(!l.contains(&n(&music.join("jazz")), &m, false));
    // Relative and empty strings are never locations.
    assert!(!l.contains(&m, "music/jazz", true));
    assert!(!l.contains("", &m, true));
    assert!(!l.contains(&m, "", true));
    assert_eq!(
        l.display(&text(&music.join("jazz/../jazz"))),
        canonical(&music.join("jazz"))
    );
    assert_eq!(l.display("not/absolute"), "not/absolute");
}

#[test]
fn normal_forms() {
    let dir = tempfile::TempDir::new().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir_all(music.join("jazz")).unwrap();
    let l = DesktopLocations;
    let m = canonical(&music);
    let n = |p: &Path| l.normalize(&text(p));
    // `.` and `..` where the path exists; a trailing separator is dropped.
    assert_eq!(n(&music.join("./jazz/..")), Some(m.clone()));
    assert_eq!(l.normalize(&format!("{}/", text(&music))), Some(m.clone()));
    // What does not exist yet is kept as written, but `..` past it is refused.
    assert_eq!(
        n(&music.join("new/mix.m3u")),
        Some(text(&Path::new(&m).join("new").join("mix.m3u")))
    );
    #[cfg(unix)]
    assert_eq!(n(&music.join("new/../mix.m3u")), None);
    // Win32 resolves `..` by name before the file system sees the path, so the file
    // is written where this says (also past a link).
    #[cfg(windows)]
    assert_eq!(
        n(&music.join("new/../mix.m3u")),
        Some(text(&Path::new(&m).join("mix.m3u")))
    );
    for bad in ["", "music", "./music", "../music"] {
        assert_eq!(l.normalize(bad), None, "{bad:?}");
    }
    // The normal form is its own normal form.
    let once = n(&music.join("jazz/new.m3u")).unwrap();
    assert_eq!(l.normalize(&once), Some(once.clone()));

    // Well formed, by the text alone: absolute, and no way back up.
    assert!(l.well_formed(&text(&music.join("jazz/new.m3u"))));
    assert!(
        l.well_formed(&text(&music.join("./jazz"))),
        "`.` inside is dropped"
    );
    for bad in ["", "music", "./music", "../music"] {
        assert!(!l.well_formed(bad), "{bad:?}");
    }
    assert!(!l.well_formed(&text(&music.join("jazz/.."))));
    // Nothing that would show the user something else.
    for hidden in ["\u{0}", "\n", "\u{202E}3u.m"] {
        assert!(
            !l.well_formed(&format!("{}{hidden}", text(&music))),
            "{hidden:?}"
        );
    }
    assert!(!l.well_formed(&text(&music.join("new/../mix.m3u"))));
    // Where a location is by its name: as `normalize` where that reads it; a place
    // that is not there at all, as written.
    assert_eq!(
        l.nearest(&text(&music.join("jazz"))),
        n(&music.join("jazz"))
    );
    assert_eq!(l.nearest(&text(&music.join("jazz/.."))), None);
    #[cfg(unix)]
    assert_eq!(l.nearest("/"), Some("/".into()));
}

#[test]
fn children() {
    let l = DesktopLocations;
    let root = text(&std::env::temp_dir());
    assert_eq!(
        l.child(&root, "Jazz/Miren"),
        Some(text(&std::env::temp_dir().join("Jazz").join("Miren")))
    );
    for bad in ["", "Jazz//Miren", "./Jazz", "Jazz/..", "/Jazz", "Jazz/"] {
        assert_eq!(l.child(&root, bad), None, "{bad:?}");
    }
    assert_eq!(l.child("relative", "Jazz"), None);
    #[cfg(windows)]
    for bad in [r"Jazz\Miren", "C:", "Jazz:x", "Jazz/Miren:Dalen"] {
        assert_eq!(l.child(&root, bad), None, "{bad:?}");
    }
    // Windows reads these as devices, or drops the trailing `.` or space (local file access
    // check): no folder of that name can be there. Elsewhere they are
    // names like any other.
    for odd in [
        "CON",
        "con",
        "Jazz/NUL",
        "aux.txt",
        "COM1",
        "lpt9.m3u",
        "COM¹",
        "CONOUT$",
        "Jazz./x",
        "Jazz /x",
        "Jazz/Miren.",
    ] {
        let expected = (!cfg!(windows)).then(|| {
            odd.split('/')
                .fold(std::env::temp_dir(), |p, part| p.join(part))
        });
        assert_eq!(
            l.child(&root, odd),
            expected.as_deref().map(text),
            "{odd:?}"
        );
    }
}

/// the names Windows reads otherwise, told on every
/// platform.
#[test]
fn names_windows_reads_otherwise() {
    for odd in [
        "CON",
        "con",
        "Con.txt",
        "PRN",
        "AUX",
        "NUL",
        "nul.tar.gz",
        "NUL .txt",
        "COM0",
        "COM1",
        "com9",
        "LPT1",
        "lpt5.m3u",
        "COM¹",
        "LPT³",
        "CONIN$",
        "conout$",
        "x.m3u:s",
        "X:s",
        "C:",
        "Jazz.",
        "Jazz ",
        "..",
        ".",
        // In a `\\?\` path only `\` separates parts.
        "Music/../Diary",
        "a\\b",
        "a<b",
        "a>b",
        "a\"b",
        "a|b",
        "a?b",
        "a*b",
        "a\u{0}b",
        "a\tb",
    ] {
        assert!(!windows_name(odd), "{odd:?}");
    }
    for fine in [
        "Jazz",
        "CONSOLE",
        "CON1",
        "COM10",
        "LPT",
        "COMX",
        "NULL",
        "aux_",
        "x.CON",
        ".hidden",
        "Miren Dalen",
        "古典",
        "a.m3u",
    ] {
        assert!(windows_name(fine), "{fine:?}");
    }
}

/// a stream name is no file or folder of its own, so a
/// `forbid` of a folder would not hold the stream `C:\X:s` of that folder.
#[test]
fn streams_are_not_locations() {
    let dir = tempfile::TempDir::new().unwrap();
    let l = DesktopLocations;
    let real = canonical(dir.path());
    for name in [
        "x.m3u:stream",
        "x.m3u:stream:$DATA",
        "CON",
        "nul.m3u",
        "new.",
        "new ",
    ] {
        let at = text(&dir.path().join(name));
        let expected = (!cfg!(windows)).then(|| text(&Path::new(&real).join(name)));
        assert_eq!(l.normalize(&at), expected, "{name:?}");
    }
    // The folder's own stream (`<dir>:s`), and a stream inside a name on the way.
    for at in [
        format!("{}:s", text(dir.path())),
        format!("{}:s\\x.m3u", text(dir.path())),
    ] {
        if cfg!(windows) {
            assert_eq!(l.normalize(&at), None, "{at:?}");
        }
    }
}

/// a UNC path back to this computer (`\\localhost\C$\…`,
/// `\\127.0.0.1\c$\…`, `\\?\UNC\…`) is the local path, so a `forbid` of the local
/// folder holds it.
#[cfg(windows)]
#[test]
fn unc_paths_back_to_this_computer_are_local() {
    let dir = tempfile::TempDir::new().unwrap();
    let real = canonical(dir.path());
    std::fs::create_dir(Path::new(&real).join("out")).unwrap();
    let out = text(&Path::new(&real).join("out"));
    let file = text(&Path::new(&out).join("f.m3u"));
    let (drive, tail) = real
        .split_once(":\\")
        .expect("a temporary folder on a drive");
    let l = DesktopLocations;
    let lower = drive.to_lowercase();
    for unc in [
        format!(r"\\localhost\{drive}$\{tail}\out\f.m3u"),
        format!(r"\\LOCALHOST\{lower}$\{tail}\out\f.m3u"),
        format!(r"\\127.0.0.1\{lower}$\{tail}\out\f.m3u"),
        format!(r"\\127.1\{drive}$\{tail}\out\f.m3u"),
        format!(r"\\[::1]\{drive}$\{tail}\out\f.m3u"),
        format!(r"\\?\UNC\localhost\{drive}$\{tail}\out\f.m3u"),
        format!("//localhost/{drive}$/{tail}/out/f.m3u"),
        format!(r"\\localhost\{drive}$\{}\out\new\..\f.m3u", tail),
    ] {
        assert_eq!(l.normalize(&unc).as_deref(), Some(file.as_str()), "{unc}");
        let normal = l.normalize(&unc).unwrap();
        assert!(l.contains(&out, &normal, true), "{unc}");
        assert!(l.contains(&out, &normal, false), "{unc}");
        assert_eq!(l.display(&unc), file, "{unc}");
    }
    // Not told as local (a share whose folder is not known): a `forbid` holds it, an
    // `allow` does not.
    let unknown = format!(
        r"\\localhost\pmp-no-such-share-{}\f.m3u",
        std::process::id()
    );
    assert!(l.contains(&out, &unknown, true));
    assert!(!l.contains(&out, &unknown, false));
    // Another computer, by address: neither.
    let other = r"\\192.0.2.1\c$\x\f.m3u";
    assert!(!l.contains(&out, other, true));
    assert!(!l.contains(&out, other, false));
}

#[cfg(unix)]
#[test]
fn symlinks_are_resolved_as_the_system_resolves_them() {
    let dir = tempfile::TempDir::new().unwrap();
    let music = dir.path().join("music");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(outside.join("sub")).unwrap();
    std::fs::create_dir(&music).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&music, &link).unwrap();
    let l = DesktopLocations;
    let n = |p: &Path| l.normalize(&text(p)).unwrap();
    assert!(l.contains(&n(&music), &n(&link.join("a.m3u")), false));
    assert!(l.contains(&n(&link), &n(&music.join("a.m3u")), false));

    // A link inside the folder to somewhere else, then `..`: the system goes to the
    // link's target first, so this is outside the folder (and says so).
    std::os::unix::fs::symlink(outside.join("sub"), music.join("l")).unwrap();
    let escaped = n(&music.join("l/../escaped.m3u"));
    assert_eq!(
        escaped,
        text(&Path::new(&canonical(&outside)).join("escaped.m3u"))
    );
    assert!(!l.contains(&n(&music), &escaped, true));
    assert!(l.contains(&n(&outside), &escaped, false));
    assert_eq!(
        l.display(&text(&music.join("l/../x"))),
        text(&Path::new(&canonical(&outside)).join("x"))
    );

    // A link to nothing is not a location, nor is anything through it.
    std::os::unix::fs::symlink(dir.path().join("gone"), music.join("dangling")).unwrap();
    assert_eq!(l.normalize(&text(&music.join("dangling"))), None);
    assert_eq!(l.normalize(&text(&music.join("dangling/x.m3u"))), None);
    // By its name it is in the folder (through the link above it too).
    let m = canonical(&music);
    assert_eq!(
        l.nearest(&text(&music.join("dangling/x.m3u"))),
        Some(text(&Path::new(&m).join("dangling").join("x.m3u")))
    );
    assert_eq!(
        l.nearest(&text(&link.join("dangling"))),
        Some(text(&Path::new(&m).join("dangling")))
    );
}

#[test]
fn letter_case() {
    let dir = tempfile::TempDir::new().unwrap();
    let music = dir.path().join("Music");
    std::fs::create_dir(&music).unwrap();
    let l = DesktopLocations;
    let m = l.normalize(&text(&music)).unwrap();
    let other = l
        .normalize(&text(&dir.path().join("Music").join("New")))
        .unwrap();
    let flipped = text(&Path::new(&m).with_file_name("mUSIC").join("new"));
    let probed = paths::case_insensitive_fs(Path::new(&m));
    assert_eq!(l.ignores_case(&m), probed);
    assert_eq!(l.ignores_case(&other), probed, "probed where it exists");
    match probed {
        Some(ignored) => {
            assert_eq!(l.contains(&m, &flipped, false), ignored);
            assert_eq!(l.contains(&m, &flipped, true), ignored);
        }
        None => unreachable!("\"Music\" has letters to flip"),
    }
    // A name without letters (numeric or non-Latin names) is probed through the folder above it,
    // on the same file system: told as for that folder.
    let digits = dir.path().join("2024");
    std::fs::create_dir(&digits).unwrap();
    let d = l.normalize(&text(&digits)).unwrap();
    assert_eq!(l.ignores_case(&d), probed);
    let han = l.normalize(&text(&music.join("古典"))).unwrap();
    std::fs::create_dir(&han).unwrap();
    assert_eq!(l.ignores_case(&han), probed);
    // Only the case of a folder above it differs: that counts as the file system has
    // it.
    let parent = Path::new(&d).parent().unwrap();
    let name = parent.file_name().unwrap().to_str().unwrap();
    let swapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    if swapped != name {
        let inside = text(&parent.with_file_name(swapped).join("2024").join("x"));
        assert_eq!(l.contains(&d, &inside, false), probed == Some(true));
        assert_eq!(l.contains(&d, &inside, true), probed == Some(true));
    }
}
