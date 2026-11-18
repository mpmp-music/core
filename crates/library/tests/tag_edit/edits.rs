//! Edits scenarios.
use super::*;

#[cfg(windows)]
#[test]
fn windows_sharing_conflicts_are_file_in_use_in_preview_and_execution() {
    use std::os::windows::fs::OpenOptionsExt;

    const SHARE_READ_WRITE: u32 = 0x1 | 0x2;
    let f = Fixture::new();
    let id = f.id("a.mp3");
    let path = f.music.join("a.mp3");
    let edit = |changes| {
        f.lib
            .edit_tags(&[id], changes, UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .unwrap()
    };
    let initial_hash = file_hash(&path);
    let changes = [set("ARTIST", &["New"])];
    // This handle allows reads but forbids replacement.
    let deny_delete = || {
        fs::File::options()
            .read(true)
            .share_mode(SHARE_READ_WRITE)
            .open(&path)
            .unwrap()
    };
    let reader = deny_delete();
    let preview = f.lib.preview_tag_edit(&[id], &changes).unwrap();
    assert!(preview.files.is_empty());
    assert_eq!(preview.skipped, [(id, "file_in_use")]);
    let report = edit(&changes);
    assert_eq!(report.skipped, [(id, "file_in_use")]);
    assert!(report.failed.is_empty());
    assert!(report.action.is_none());
    drop(reader);

    // This handle conflicts with the writer guard even though it allows replacement.
    let writer = fs::File::options().write(true).open(&path).unwrap();
    let preview = f.lib.preview_tag_edit(&[id], &changes).unwrap();
    assert_eq!(preview.skipped, [(id, "file_in_use")]);
    assert_eq!(edit(&changes).skipped, [(id, "file_in_use")]);
    drop(writer);
    assert_eq!(file_hash(&path), initial_hash);

    let other = f.id("a.flac");
    let other_path = f.music.join("a.flac");
    let other_initial = file_hash(&other_path);
    let ids = [other, id];
    let edited = f
        .lib
        .edit_tags(&ids, &changes, UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(edited.written, ids);
    let changed = [file_hash(&other_path), file_hash(&path)];
    let action = edited.action.unwrap();
    let reader = deny_delete();
    let preview = f.lib.preview_tag_undo(action).unwrap();
    assert_eq!(preview.skipped, [(id, "file_in_use")]);
    assert_eq!(preview.files.len(), 1);
    assert_eq!(preview.files[0].track, other);
    let undo = || {
        f.lib
            .undo_tag_edit(action, UI, TagJobOptions::default())
            .unwrap()
            .unwrap()
            .wait()
    };
    assert!(matches!(undo(), Err(LibraryError::ExternalInUse(tracks)) if tracks == [id]));
    assert_eq!([file_hash(&other_path), file_hash(&path)], changed);
    drop(reader);
    let mut restored = undo().unwrap().written;
    restored.sort_unstable();
    assert_eq!(restored, [other.min(id), other.max(id)]);
    assert_eq!(file_hash(&other_path), other_initial);
    assert_eq!(file_hash(&path), initial_hash);
}

/// Nothing is written over someone else's change: a file changed since the last scan is
/// skipped, as are the player's files and missing ones; a checked caller's file that is
/// elsewhere now fails; one that has the values already is left alone.
#[test]
fn files_changed_in_use_or_missing_are_not_written() {
    let mut f = Fixture::new();
    let (flac, mp3, m4a, wav) = (f.id("a.flac"), f.id("a.mp3"), f.id("a.m4a"), f.id("a.wav"));
    let touched = f.music.join("a.flac");
    let bytes = fs::read(&touched).unwrap();
    let later = SystemTime::now() + Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&touched)
        .unwrap()
        .set_modified(later)
        .unwrap();
    fs::remove_file(f.music.join("a.wav")).unwrap();
    let in_use = TagJobOptions {
        in_use: Arc::new(move |t| t == mp3),
        ..TagJobOptions::default()
    };
    let changes = [set("ARTIST", &["New"])];
    let preview = f.lib.preview_tag_edit(&[flac, mp3, wav], &changes).unwrap();
    assert_eq!(
        preview.skipped,
        [(flac, "file_changed"), (wav, "file_missing")]
    );
    let report = f
        .lib
        .edit_tags(&[flac, mp3, wav, m4a], &changes, UI, in_use)
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, [m4a]);
    assert_eq!(
        report.skipped,
        [
            (flac, "file_changed"),
            (mp3, "file_in_use"),
            (wav, "file_missing")
        ]
    );
    assert_eq!(fs::read(&touched).unwrap(), bytes, "not written");
    assert_eq!(f.tags(mp3, &["ARTIST"]), values(&[("ARTIST", &["Old"])]));
    // The same values again: nothing written, nothing recorded.
    let report = f
        .lib
        .edit_tags(&[m4a], &changes, UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report, mpmp_library::TagEditReport::default());

    // Checked: only the locations checked.
    f.rescan();
    let flac = f.id("a.flac");
    let canonical = mpmp_fs_desktop::paths::canonical_dir(&f.music.join("a.flac")).unwrap();
    let elsewhere = TagJobOptions {
        checked: Some(HashMap::from([(flac, canonical.with_file_name("b.flac"))])),
        ..TagJobOptions::default()
    };
    let agent = ActionMeta::new("agent:claude").with_checked(true);
    let report = f
        .lib
        .edit_tags(&[flac, mp3], &changes, agent, elsewhere)
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(
        report.failed,
        [(flac, "location_changed"), (mp3, "location_changed")]
    );
    let here = TagJobOptions {
        checked: Some(HashMap::from([(flac, canonical)])),
        ..TagJobOptions::default()
    };
    let report = f
        .lib
        .edit_tags(&[flac], &changes, agent, here)
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, [flac]);
    let summary = f.lib.action(report.action.unwrap()).unwrap().unwrap();
    assert_eq!(summary.origin, "agent:claude");
}

/// Bad changes are refused before anything is read: keys the library shows under another
/// name, pictures and ratings, blank or repeated ones, empty values.
#[test]
fn bad_changes_are_refused() {
    let f = Fixture::new();
    let flac = f.id("a.flac");
    for changes in [
        vec![set("TPE1", &["x"])],
        vec![set("aART", &["x"])],
        vec![set("ALBUM ARTIST", &["x"])],
        vec![set("RATING", &["5"])],
        vec![remove("METADATA_BLOCK_PICTURE")],
        vec![set("", &["x"])],
        vec![set("A=B", &["x"])],
        vec![set("ARTIST", &[])],
        vec![set("ARTIST", &["  "])],
        vec![set("ARTIST", &["a\0b"])],
        vec![set("ARTIST", &["x"]), remove("artist")],
    ] {
        let invalid = |e: &LibraryError| e.code() == "invalid_tag";
        let error = f.lib.preview_tag_edit(&[flac], &changes).unwrap_err();
        assert!(invalid(&error), "{changes:?}: {error}");
        let error = f
            .lib
            .edit_tags(&[flac], &changes, UI, TagJobOptions::default())
            .unwrap_err();
        assert!(invalid(&error), "{changes:?}: {error}");
    }
    let error = f.lib.preview_tag_edit(&[TrackId(999)], &[]).unwrap_err();
    assert_eq!(error.code(), "track_not_found");
    let many = vec![flac; mpmp_library::MAX_TAG_EDIT_TRACKS + 1];
    let error = f.lib.preview_tag_edit(&many, &[]).unwrap_err();
    assert_eq!(error.code(), "out_of_range");
}

#[test]
fn container_lowercase_chunk_layout_round_trips_even_when_last_frame_is_removed() {
    use lofty::id3::v2::Id3v2Tag;
    for name in ["a.wav", "a.aiff"] {
        for deleting in [false, true] {
            let mut f = Fixture::new();
            let path = f.music.join(name);
            let little = name.ends_with("wav");
            let mut tag = Id3v2Tag::new();
            tag.insert(text_frame(
                "TIT2",
                TextEncoding::Latin1,
                gbk_latin1("三棱雾"),
            ));
            let mut payload = Vec::new();
            tag.dump_to(&mut payload, WriteOptions::new().use_id3v23(true))
                .unwrap();
            // Exercise an odd-sized payload with a nonzero external alignment byte.
            if payload.len().is_multiple_of(2) {
                payload.push(0);
                let len = payload.len() - 10;
                for i in 0..4 {
                    payload[6 + i] = ((len >> (7 * (3 - i))) & 0x7f) as u8;
                }
            }
            let source = fs::read(&path).unwrap();
            let mut chunks = Vec::new();
            let mut pos = 12;
            while pos < source.len() {
                let size = source[pos + 4..pos + 8].try_into().unwrap();
                let len = if little {
                    u32::from_le_bytes(size)
                } else {
                    u32::from_be_bytes(size)
                } as usize;
                let end = pos + 8 + len + len % 2;
                if !source[pos..pos + 4].eq_ignore_ascii_case(b"ID3 ") {
                    chunks.push(source[pos..end].to_vec());
                }
                pos = end;
            }
            let mut chunk = b"id3 ".to_vec();
            let len = payload.len() as u32;
            chunk.extend(if little {
                len.to_le_bytes()
            } else {
                len.to_be_bytes()
            });
            chunk.extend(payload);
            chunk.push(0xa5);
            chunks.insert(1, chunk);
            let mut original = source[..12].to_vec();
            for chunk in chunks {
                original.extend(chunk);
            }
            let size = (original.len() - 8) as u32;
            original[4..8].copy_from_slice(&if little {
                size.to_le_bytes()
            } else {
                size.to_be_bytes()
            });
            fs::write(&path, &original).unwrap();
            let root = f.lib.roots().unwrap()[0].id;
            f.lib.set_root_encoding(root, Some("gbk")).unwrap();
            f.rescan();
            let id = f.id(name);
            let changes = [if deleting {
                remove("TITLE")
            } else {
                set("GENRE", &["Pop"])
            }];
            let report = f
                .lib
                .edit_tags(&[id], &changes, UI, TagJobOptions::default())
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(
                report.written,
                [id],
                "{name}, deleting={deleting}: {report:?}"
            );
            let after = fs::read(&path).unwrap();
            let undo = f
                .lib
                .undo_tag_edit(report.action.unwrap(), UI, TagJobOptions::default())
                .unwrap()
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(undo.written, [id], "{name}: {undo:?}");
            assert_eq!(fs::read(&path).unwrap(), original, "{name}: undo");
            let redo = f
                .lib
                .undo_tag_edit(undo.action.unwrap(), UI, TagJobOptions::default())
                .unwrap()
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(redo.written, [id], "{name}: {redo:?}");
            assert_eq!(fs::read(&path).unwrap(), after, "{name}: redo");
        }
    }
}
