use super::*;

#[test]
fn audio_extensions() {
    assert!(is_audio(Path::new("a/b.FLAC")));
    assert!(is_audio(Path::new("x.m4a")));
    assert!(!is_audio(Path::new("cover.jpg")));
    assert!(!is_audio(Path::new("flac")));
}

#[test]
fn relative_paths_use_slashes() {
    let root = Path::new("/music");
    let path = root.join("A").join("B").join("01.flac");
    assert_eq!(relative_path(root, &path).as_deref(), Some("A/B/01.flac"));
    assert_eq!(relative_path(root, root), None);
    assert_eq!(relative_path(root, Path::new("/elsewhere/x.mp3")), None);
    assert_eq!(parent_dir("A/B/01.flac"), "A/B");
    assert_eq!(parent_dir("01.flac"), "");
}

mod regressions;
