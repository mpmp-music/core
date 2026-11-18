//! Undo scenarios.
use super::*;

#[test]
fn exact_undo_refuses_an_external_write_even_if_it_restored_the_old_bytes() {
    let f = Fixture::new();
    let id = f.id("a.flac");
    let path = f.music.join("a.flac");
    let before = fs::read(&path).unwrap();
    let edited = f
        .lib
        .edit_tags(
            &[id],
            &[set("ARTIST", &["New"])],
            UI,
            TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    let action = edited.action.unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(before.len() as u64, fs::metadata(&path).unwrap().len());
    fs::write(&path, &before).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let undo = f
        .lib
        .undo_tag_edit(action, UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert!(undo.written.is_empty());
    assert!(undo.completed.is_empty());
    assert_eq!(undo.skipped, [(id, "file_changed")], "{undo:?}");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    assert!(undo.action.is_none());
    assert!(f.lib.action(action).unwrap().unwrap().undone_by.is_none());
}

/// Every format: the preview shows before and after as the library will; the edit writes
/// them (several values, a key of the file's own), keeps the other tags and the pictures,
/// and records one action; its undo writes the old values back, and undoing the undo
/// redoes it.
#[test]
fn edits_every_format_and_undoes_them() {
    let f = Fixture::new();
    let tracks = f.all();
    let hashes = || {
        FILES
            .iter()
            .map(|(name, _)| file_hash(&f.music.join(name)))
            .collect::<Vec<_>>()
    };
    let before_hashes = hashes();
    let before = values(&[
        ("TITLE", &["Song"]),
        ("ARTIST", &["Old"]),
        ("ALBUM", &["Album"]),
        ("COMMENT", &["a comment"]),
        ("COMPOSER", &["Composer"]),
        ("TRACKNUMBER", &["3"]),
        ("TRACKTOTAL", &["12"]),
    ]);
    for (name, _) in FILES {
        assert_eq!(f.tags(f.id(name), KEYS), before, "{name}");
        assert_eq!(pictures(&f.music.join(name)), 1, "{name}");
    }
    let changes = [
        set("artist", &["New A", " New B "]),
        remove("COMMENT"),
        set("TRACKTOTAL", &["10"]),
        set("MOOD_OF_THE_DAY", &["Bright"]),
    ];
    let after = values(&[
        ("TITLE", &["Song"]),
        ("ARTIST", &["New A", "New B"]),
        ("ALBUM", &["Album"]),
        ("COMPOSER", &["Composer"]),
        ("TRACKNUMBER", &["3"]),
        ("TRACKTOTAL", &["10"]),
        ("MOOD_OF_THE_DAY", &["Bright"]),
    ]);
    let preview = f.lib.preview_tag_edit(&tracks, &changes).unwrap();
    assert!(preview.skipped.is_empty(), "{:?}", preview.skipped);
    assert_eq!(preview.files.len(), FILES.len());
    let shown = |m: &BTreeMap<String, Vec<String>>| -> BTreeMap<String, Vec<String>> {
        m.iter()
            .filter(|(k, _)| KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };
    for (file, (name, _)) in preview.files.iter().zip(FILES) {
        assert_eq!(file.track, f.id(name));
        assert_eq!(shown(&file.before), before, "{name}");
        assert_eq!(shown(&file.after), after, "{name}");
    }
    // A preview writes nothing.
    assert_eq!(f.tags(f.id("a.flac"), KEYS), before);

    let progress = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&progress);
    let options = TagJobOptions {
        progress: Box::new(move |done, total| seen.lock().unwrap().push((done, total))),
        ..TagJobOptions::default()
    };
    let report = f
        .lib
        .edit_tags(&tracks, &changes, UI, options)
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, tracks, "{report:?}");
    let after_hashes = hashes();
    assert_ne!(after_hashes, before_hashes);
    assert!(
        report.skipped.is_empty() && report.failed.is_empty(),
        "{report:?}"
    );
    let action = report.action.unwrap();
    let progress = progress.lock().unwrap().clone();
    assert_eq!(progress.first(), Some(&(0, FILES.len() as u64)));
    assert_eq!(
        progress.last(),
        Some(&(FILES.len() as u64, FILES.len() as u64))
    );
    for (name, _) in FILES {
        assert_eq!(f.tags(f.id(name), KEYS), after, "{name}");
        assert_eq!(pictures(&f.music.join(name)), 1, "{name}: pictures kept");
        // What the file says, as a scan of it would read it.
        let track = f.lib.track(f.id(name)).unwrap().unwrap();
        assert_eq!(track.artist.as_deref(), Some("New A"), "{name}");
        assert_eq!(track.track_total, Some(10), "{name}");
        assert!(track.has_embedded_art, "{name}");
    }
    let summary = f.lib.action(action).unwrap().unwrap();
    assert_eq!(summary.kind, "tag_edit");
    assert_eq!(
        summary.summary,
        "edit the tags of 7 files: set ARTIST, remove COMMENT, set TRACKTOTAL, set MOOD_OF_THE_DAY"
    );
    assert_eq!(summary.target_count, FILES.len() as u64);
    assert!(summary.undoable);
    assert_eq!(f.lib.tag_edit_tracks(action).unwrap().len(), FILES.len());

    // A rescan finds the files as the edit left them.
    let undo = f.lib.undo_action(action, UI).unwrap();
    assert_eq!(undo.value.tracks.len(), FILES.len());
    for (name, _) in FILES {
        assert_eq!(f.tags(f.id(name), KEYS), before, "{name}: undone");
        assert_eq!(pictures(&f.music.join(name)), 1, "{name}");
    }
    assert_eq!(hashes(), before_hashes);
    let undo_action = undo.action.unwrap();
    let summary = f.lib.action(undo_action).unwrap().unwrap();
    assert_eq!(summary.kind, "undo");
    assert!(
        summary
            .summary
            .starts_with("undo: edit the tags of 7 files")
    );
    // Undoing the undo redoes the edit.
    let redo = f.lib.undo_action(undo_action, UI).unwrap().action.unwrap();
    assert_eq!(hashes(), after_hashes);
    for (name, _) in FILES {
        assert_eq!(f.tags(f.id(name), KEYS), after, "{name}: redone");
    }
    f.lib.undo_action(redo, UI).unwrap();
    assert_eq!(hashes(), before_hashes);
    assert!(matches!(
        f.lib.undo_action(action, UI),
        Err(LibraryError::Invalid { .. })
    ));
}

/// An undo writes back only files as the edit left them: one changed since, or a later
/// tag edit of it, is a conflict and nothing is written; once the later edit is undone,
/// the earlier one can be.
#[test]
fn undo_conflicts() {
    let f = Fixture::new();
    let (flac, mp3) = (f.id("a.flac"), f.id("a.mp3"));
    let edit = |tracks: &[TrackId], changes: &[TagChange]| {
        f.lib
            .edit_tags(tracks, changes, UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .unwrap()
            .action
            .unwrap()
    };
    let first = edit(&[flac, mp3], &[set("ALBUM", &["First"])]);
    let second = edit(&[mp3], &[set("ARTIST", &["Second"])]);
    let conflict =
        |result: Result<mpmp_library::Done<mpmp_library::UndoEffect>, LibraryError>| match result {
            Err(LibraryError::ActionConflict { changed, .. }) => changed,
            other => panic!("{other:?}"),
        };
    assert_eq!(
        conflict(f.lib.undo_action(first, UI)),
        [mpmp_library::ActionTarget::Track { id: mp3 }]
    );
    assert_eq!(f.tags(flac, &["ALBUM"]), values(&[("ALBUM", &["First"])]));
    f.lib.undo_action(second, UI).unwrap();
    assert_eq!(f.tags(mp3, &["ARTIST"]), values(&[("ARTIST", &["Old"])]));
    // A rating is not a tag: no conflict.
    f.lib.set_rating(&[flac], Some(4), UI).unwrap();
    // The player's file: not now.
    let job = f
        .lib
        .undo_tag_edit(
            first,
            UI,
            TagJobOptions {
                in_use: Arc::new(move |t| t == mp3),
                ..TagJobOptions::default()
            },
        )
        .unwrap()
        .unwrap();
    assert!(matches!(job.wait(), Err(LibraryError::InUse(t)) if t == [mp3]));
    // The later edit undone (which wrote the file again): the earlier one can be undone.
    f.lib.undo_action(first, UI).unwrap();
    for id in [flac, mp3] {
        assert_eq!(
            f.tags(id, &["ALBUM", "ARTIST"]),
            values(&[("ALBUM", &["Album"]), ("ARTIST", &["Old"])])
        );
    }
    let first = edit(&[flac, mp3], &[set("ALBUM", &["First"])]);
    // A file changed since: not written back.
    let path = f.music.join("a.flac");
    let bytes = fs::read(&path).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(5))
        .unwrap();
    let job = f
        .lib
        .undo_tag_edit(first, UI, TagJobOptions::default())
        .unwrap()
        .unwrap();
    match job.wait() {
        Err(LibraryError::ActionConflict { changed, .. }) => {
            assert_eq!(changed, [mpmp_library::ActionTarget::Track { id: flac }]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(f.tags(mp3, &["ALBUM"]), values(&[("ALBUM", &["First"])]));
    assert!(f.lib.action(first).unwrap().unwrap().undoable);
    // Not a tag edit: no job.
    let rating = f
        .lib
        .set_rating(&[mp3], Some(2), UI)
        .unwrap()
        .action
        .unwrap();
    assert!(
        f.lib
            .undo_tag_edit(rating, UI, TagJobOptions::default())
            .unwrap()
            .is_none()
    );
    assert!(f.lib.tag_edit_tracks(rating).unwrap().is_empty());
}

/// Retained iTunes data must never be copied into an ordinary comment, including when
/// saving displayed values, undoing, or undoing that undo.
#[test]
fn protected_comments_survive_displayed_sets_and_undo() {
    use lofty::config::ParseOptions;
    use lofty::id3::v2::{CommentFrame, Frame};
    use lofty::mpeg::MpegFile;
    let mut f = Fixture::new();
    let path = f.music.join("a.mp3");
    let mut tag = MpegFile::read_from(&mut fs::File::open(&path).unwrap(), ParseOptions::new())
        .unwrap()
        .id3v2()
        .unwrap()
        .clone();
    tag.insert(Frame::Comment(CommentFrame::new(
        TextEncoding::UTF8,
        *b"eng",
        "iTunNORM",
        " 0000021A 0000021A",
    )));
    tag.save_to_path(&path, WriteOptions::new()).unwrap();
    f.rescan();
    let id = f.id("a.mp3");
    let original = f.tags(id, &["COMMENT"]);
    let changes = [TagChange::Set {
        key: "COMMENT".into(),
        values: original["COMMENT"].clone(),
    }];
    let untouched = fs::read(&path).unwrap();
    let report = f
        .lib
        .edit_tags(&[id], &changes, UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert!(report.written.is_empty(), "{report:?}");
    assert_eq!(fs::read(&path).unwrap(), untouched);
    let report = f
        .lib
        .edit_tags(
            &[id],
            &[set("COMMENT", &["new"])],
            UI,
            TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    let action = report.action.unwrap();
    let undo = f
        .lib
        .undo_tag_edit(action, UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert!(undo.failed.is_empty(), "{undo:?}");
    let mut expected = original["COMMENT"].clone();
    expected.sort();
    let mut actual = f.tags(id, &["COMMENT"])["COMMENT"].clone();
    actual.sort();
    assert_eq!(actual, expected);
    let redo = f
        .lib
        .undo_tag_edit(undo.action.unwrap(), UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert!(redo.failed.is_empty(), "{redo:?}");
    let back = f
        .lib
        .undo_tag_edit(redo.action.unwrap(), UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert!(back.failed.is_empty(), "{back:?}");
    let mut expected = original["COMMENT"].clone();
    expected.sort();
    let mut actual = f.tags(id, &["COMMENT"])["COMMENT"].clone();
    actual.sort();
    assert_eq!(actual, expected);
    let native =
        MpegFile::read_from(&mut fs::File::open(&path).unwrap(), ParseOptions::new()).unwrap();
    let comments: Vec<_> = native
        .id3v2()
        .unwrap()
        .into_iter()
        .filter_map(|frame| match frame {
            Frame::Comment(c) => Some((c.description.to_string(), c.content.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(
        comments
            .iter()
            .any(|(d, c)| d == "iTunNORM" && c == " 0000021A 0000021A")
    );
    assert!(
        comments
            .iter()
            .any(|(d, c)| d.is_empty() && c == "a comment")
    );
}

#[test]
fn oversized_undo_data_is_rejected_before_writing() {
    let mut f = Fixture::new();
    let path = f.music.join("a.flac");
    let mut file = Probe::open(&path).unwrap().read().unwrap();
    file.primary_tag_mut()
        .unwrap()
        .set_comment("x".repeat(8 * 1024 * 1024));
    file.save_to_path(&path, WriteOptions::new()).unwrap();
    f.rescan();
    let id = f.id("a.flac");
    let original = fs::read(&path).unwrap();
    let changes = [set("COMMENT", &["new"])];
    let preview = f.lib.preview_tag_edit(&[id], &changes).unwrap();
    assert_eq!(preview.skipped, [(id, "undo_too_large")]);
    let report = f
        .lib
        .edit_tags(&[id], &changes, UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.failed, [(id, "undo_too_large")]);
    assert!(report.action.is_none());
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn undo_budget_is_shared_across_files_and_keeps_partial_success_undoable() {
    let mut f = Fixture::new();
    let base = f.music.join("a.flac");
    let size = 6 * 1024 * 1024;
    let mut file = Probe::open(&base).unwrap().read().unwrap();
    file.primary_tag_mut()
        .unwrap()
        .set_comment("x".repeat(size));
    file.save_to_path(&base, WriteOptions::new()).unwrap();
    let paths: Vec<_> = (0..6).map(|n| format!("budget-{n}.flac")).collect();
    for name in &paths {
        fs::copy(&base, f.music.join(name)).unwrap();
    }
    f.rescan();
    let ids: Vec<_> = paths.iter().map(|name| f.id(name)).collect();
    let hash = file_hash(&base);
    let changes = [set("COMMENT", &["new"])];
    let preview = f.lib.preview_tag_edit(&ids, &changes).unwrap();
    assert_eq!(preview.files.len(), 5);
    assert_eq!(preview.skipped, [(ids[5], "undo_too_large")]);
    let report = f
        .lib
        .edit_tags(&ids, &changes, UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, ids[..5], "{report:?}");
    assert_eq!(report.failed, [(ids[5], "undo_too_large")]);
    assert_eq!(file_hash(&f.music.join(&paths[5])), hash);
    let undo = f
        .lib
        .undo_tag_edit(report.action.unwrap(), UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(undo.written, ids[..5], "{undo:?}");
    for name in &paths {
        assert_eq!(file_hash(&f.music.join(name)), hash, "{name}");
    }
}

/// The original TRCK is readable but cannot be recreated with the value editor.
#[test]
fn undo_restores_unserializable_original_tag_values() {
    let mut f = Fixture::new();
    mp3_frames(
        &mut f,
        vec![text_frame("TRCK", TextEncoding::UTF8, "3/abc".into())],
    );
    let id = f.id("a.mp3");
    let path = f.music.join("a.mp3");
    let original = file_hash(&path);
    let changes = [remove("TRACKTOTAL")];
    let preview = f.lib.preview_tag_edit(&[id], &changes).unwrap();
    assert!(preview.skipped.is_empty(), "{preview:?}");
    let report = f
        .lib
        .edit_tags(&[id], &changes, UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, [id], "{report:?}");
    f.lib.undo_action(report.action.unwrap(), UI).unwrap();
    assert_eq!(file_hash(&path), original);
}

/// The undo data of both ways fits the budget: an edit that adds much (escaped JSON six
/// times the bytes) to files that had little is not written where its undo would keep too
/// much to redo it; what it wrote is undone, and redone, whole.
#[test]
fn undo_budget_counts_what_a_redo_writes_again() {
    let mut f = Fixture::new();
    let names: Vec<String> = (0..7).map(|i| format!("big{i}.flac")).collect();
    for name in &names {
        fs::copy(f.music.join("a.flac"), f.music.join(name)).unwrap();
    }
    f.rescan();
    let ids: Vec<TrackId> = names.iter().map(|n| f.id(n)).collect();
    // 31 values of 8 KiB: under the 256 KiB of one edit; about 250 KiB per file before deduplication.
    let value = "\u{1}".repeat(8 * 1024);
    let changes = [TagChange::Set {
        key: "COMMENT".into(),
        values: vec![value; 31],
    }];
    let preview = f.lib.preview_tag_edit(&ids, &changes).unwrap();
    assert_eq!(preview.files.len(), ids.len(), "{:?}", preview.skipped);
    assert!(preview.skipped.is_empty());
    let report = f
        .lib
        .edit_tags(&ids, &changes, UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, ids, "{report:?}");
    assert!(report.failed.is_empty());
    let undo = f
        .lib
        .undo_tag_edit(report.action.unwrap(), UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(undo.written, ids, "{undo:?}");
    assert!(
        undo.failed.is_empty() && undo.skipped.is_empty(),
        "{undo:?}"
    );
    let edit = f.lib.action(report.action.unwrap()).unwrap().unwrap();
    assert_eq!(edit.undone_by, undo.action);
    for id in &ids {
        assert_eq!(f.tags(*id, &["COMMENT"])["COMMENT"], ["a comment"]);
    }
    let redo = f
        .lib
        .undo_tag_edit(undo.action.unwrap(), UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(redo.written, ids, "{redo:?}");
    assert!(redo.failed.is_empty(), "{redo:?}");
}

/// An undo that cannot write a file back (made read-only since) reports it, and the edit
/// stays undoable for it: not undone, counting that file; the undo counts those it wrote
/// back. Undoing the edit again, once the file can be written, writes it back and undoes
/// the edit.
#[test]
fn an_undo_that_cannot_write_a_file_back_leaves_it_undoable() {
    let f = Fixture::new();
    let ids = [f.id("a.flac"), f.id("a.mp3")];
    let report = f
        .lib
        .edit_tags(
            &ids,
            &[set("TITLE", &["Changed"])],
            UI,
            TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, ids);
    let edit = report.action.unwrap();
    let frozen = f.lib.describe_undo(edit).unwrap();
    let mp3 = f.music.join("a.mp3");
    let writable = fs::metadata(&mp3).unwrap().permissions();
    let mut readonly = writable.clone();
    readonly.set_readonly(true);
    fs::set_permissions(&mp3, readonly).unwrap();
    let undo = f
        .lib
        .undo_tag_edit(edit, UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(undo.written, [ids[0]], "{undo:?}");
    assert_eq!(undo.failed, [(ids[1], "file_read_only")], "{undo:?}");
    let first = undo.action.unwrap();
    let remaining = f.lib.describe_undo(edit).unwrap();
    assert!(remaining.writes_files);
    assert_ne!(remaining.fingerprint, frozen.fingerprint);
    assert_eq!(
        remaining.targets,
        vec![mpmp_library::ActionTarget::Track { id: ids[1] }]
    );
    let stale = f
        .lib
        .undo_tag_edit(
            edit,
            UI,
            TagJobOptions {
                undo_guard: Some(frozen),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap()
        .wait();
    assert!(stale.is_err());

    let summary = f.lib.action(edit).unwrap().unwrap();
    assert_eq!(summary.undone_by, None);
    assert!(summary.undoable);
    assert_eq!(summary.target_count, 1);
    assert_eq!(f.lib.tag_edit_tracks(edit).unwrap(), [ids[1]]);
    let undo_summary = f.lib.action(first).unwrap().unwrap();
    assert_eq!(undo_summary.undo_of, Some(edit));
    assert_eq!(undo_summary.target_count, 1);
    assert_eq!(f.tags(ids[0], &["TITLE"])["TITLE"], ["Song"]);
    assert_eq!(f.tags(ids[1], &["TITLE"])["TITLE"], ["Changed"]);

    fs::set_permissions(&mp3, writable).unwrap();
    let again = f
        .lib
        .undo_tag_edit(edit, UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(again.written, [ids[1]], "{again:?}");
    assert!(again.failed.is_empty());
    let summary = f.lib.action(edit).unwrap().unwrap();
    assert_eq!(summary.undone_by, again.action);
    assert!(!summary.undoable);
    assert_eq!(f.tags(ids[1], &["TITLE"])["TITLE"], ["Song"]);
    // Each undo redoes what it wrote back.
    let redo = f
        .lib
        .undo_tag_edit(first, UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(redo.written, [ids[0]], "{redo:?}");
    assert_eq!(f.tags(ids[0], &["TITLE"])["TITLE"], ["Changed"]);
}

#[test]
fn whole_block_undo_and_redo_reject_equal_length_external_binary_changes() {
    use lofty::id3::v2::{AttachedPictureFrame, BinaryFrame, Frame, FrameId};
    for private in [false, true] {
        for redo in [false, true] {
            let mut f = Fixture::new();
            let marker = b"private-content-before";
            mp3_frames(
                &mut f,
                vec![
                    text_frame("TIT2", TextEncoding::Latin1, gbk_latin1("三棱雾")),
                    Frame::Binary(BinaryFrame::new(
                        FrameId::Valid("PRIV".into()),
                        marker.to_vec(),
                    )),
                    Frame::Picture(AttachedPictureFrame::new(
                        TextEncoding::UTF8,
                        Picture::unchecked(PNG.to_vec())
                            .pic_type(PictureType::CoverFront)
                            .mime_type(MimeType::Png)
                            .build(),
                    )),
                ],
            );
            let id = f.id("a.mp3");
            let path = f.music.join("a.mp3");
            let report = f
                .lib
                .edit_tags(
                    &[id],
                    &[set("GENRE", &["Pop"])],
                    UI,
                    TagJobOptions::default(),
                )
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(report.written, [id], "{report:?}");
            let mut action = report.action.unwrap();
            if redo {
                let undo = f
                    .lib
                    .undo_tag_edit(action, UI, TagJobOptions::default())
                    .unwrap()
                    .unwrap()
                    .wait()
                    .unwrap();
                assert_eq!(undo.written, [id]);
                action = undo.action.unwrap();
            }
            let mtime = fs::metadata(&path).unwrap().modified().unwrap();
            let mut bytes = fs::read(&path).unwrap();
            let target = if private { marker.as_slice() } else { PNG };
            let at = bytes
                .windows(target.len())
                .position(|b| b == target)
                .unwrap();
            bytes[at + target.len() - 1] ^= 1;
            fs::write(&path, &bytes).unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
            let result = f
                .lib
                .undo_tag_edit(action, UI, TagJobOptions::default())
                .unwrap()
                .unwrap()
                .wait();
            let report = result.unwrap();
            assert_eq!(report.skipped, [(id, "file_changed")]);
            assert!(report.written.is_empty());
            assert!(report.action.is_none());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }
}

fn edit_genre_and_restore(f: &Fixture, name: &str) {
    let path = f.music.join(name);
    let before = file_hash(&path);
    let report = f
        .lib
        .edit_tags(
            &[f.id(name)],
            &[set("GENRE", &["longer replacement genre"])],
            UI,
            TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, [f.id(name)], "{name} {report:?}");
    let after = file_hash(&path);
    assert_ne!(after, before, "{name}");
    let undo = f.lib.undo_action(report.action.unwrap(), UI).unwrap();
    assert_eq!(file_hash(&path), before, "{name}");
    f.lib.undo_action(undo.action.unwrap(), UI).unwrap();
    assert_eq!(file_hash(&path), after, "{name}");
}

#[test]
fn flac_restores_metadata_order_vendor_comments_and_padding() {
    let mut f = Fixture::new();
    let fixture = fs::read(fixture("sine44_a.flac")).unwrap();
    let mut pos = 4;
    loop {
        let [kind, a, b, c]: [u8; 4] = fixture[pos..pos + 4].try_into().unwrap();
        pos += 4 + u32::from_be_bytes([0, a, b, c]) as usize;
        if kind & 128 != 0 {
            break;
        }
    }
    let mut comments = Vec::new();
    let vendor = b"Test vendor with exact spelling";
    comments.extend((vendor.len() as u32).to_le_bytes());
    comments.extend(vendor);
    let items = [
        b"TITLE=Song".as_slice(),
        b"artist=One",
        b"GENRE=Original",
        b"DATE=2009",
        b"ARTIST=Two",
        b"COMMENT=leading SPACE",
    ];
    comments.extend((items.len() as u32).to_le_bytes());
    for item in items {
        comments.extend((item.len() as u32).to_le_bytes());
        comments.extend(item);
    }
    let seek = [255; 18]; // Seek-table placeholder.
    let padding = [0u8; 137];
    let streaminfo = &fixture[8..42];
    let layouts = [
        [
            (0, streaminfo),
            (3, seek.as_slice()),
            (4, &comments),
            (129, padding.as_slice()),
        ],
        [
            (0, streaminfo),
            (1, padding.as_slice()),
            (3, seek.as_slice()),
            (132, &comments),
        ],
    ];
    for layout in layouts {
        let mut original = b"fLaC".to_vec();
        for (kind, block) in layout {
            original.push(kind);
            original.extend(&(block.len() as u32).to_be_bytes()[1..]);
            original.extend(block);
        }
        original.extend(&fixture[pos..]);
        fs::write(f.music.join("a.flac"), &original).unwrap();
        f.rescan();
        edit_genre_and_restore(&f, "a.flac");
    }
}

#[test]
fn mp3_restores_v23_and_v24_with_separate_id3v1_and_ape_tags() {
    use lofty::ape::{ApeItem, ApeTag};
    use lofty::id3::{v1::Id3v1Tag, v2::Id3v2Tag};
    let mut f = Fixture::new();
    let path = f.music.join("a.mp3");
    for v23 in [true, false] {
        fs::copy(fixture("sine44_a.mp3"), &path).unwrap();
        let mut v2 = Id3v2Tag::new();
        v2.set_title("Original title".into());
        v2.set_genre("Original genre".into());
        v2.save_to_path(
            &path,
            WriteOptions::new().use_id3v23(v23).preferred_padding(735),
        )
        .unwrap();
        let mut ape = ApeTag::new();
        ape.insert(ApeItem::new("COMMENT".into(), ItemValue::Text("APE tail".into())).unwrap());
        ape.save_to_path(&path, WriteOptions::new()).unwrap();
        let mut v1 = Id3v1Tag::new();
        v1.set_artist("ID3v1 artist".into());
        v1.save_to_path(&path, WriteOptions::new()).unwrap();
        let before = fs::read(&path).unwrap();
        assert_eq!(before[3], if v23 { 3 } else { 4 });
        assert_eq!(&before[before.len() - 128..][..3], b"TAG");
        assert_eq!(&before[before.len() - 160..][..8], b"APETAGEX");
        f.rescan();
        edit_genre_and_restore(&f, "a.mp3");
    }
}

#[test]
fn same_size_and_mtime_audio_changes_abort_the_whole_undo() {
    let f = Fixture::new();
    let ids = [f.id("a.flac"), f.id("a.mp3")];
    let report = f
        .lib
        .edit_tags(
            &ids,
            &[set("GENRE", &["Edited"])],
            UI,
            TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, ids, "{report:?}");
    let flac = f.music.join("a.flac");
    let mp3 = f.music.join("a.mp3");
    let other_before = file_hash(&mp3);
    let modified = fs::metadata(&flac).unwrap().modified().unwrap();
    let mut changed = fs::read(&flac).unwrap();
    *changed.last_mut().unwrap() ^= 1;
    fs::write(&flac, &changed).unwrap();
    fs::File::options()
        .write(true)
        .open(&flac)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let id = report.action.unwrap();
    let preview = f.lib.preview_tag_undo(id).unwrap();
    assert_eq!(preview.skipped, [(ids[0], "file_changed")]);
    let report = f
        .lib
        .undo_tag_edit(id, UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.skipped, [(ids[0], "file_changed")]);
    assert!(report.written.is_empty());
    assert_eq!(file_hash(&mp3), other_before);
    assert_eq!(fs::read(flac).unwrap(), changed);
}

#[test]
fn unchanged_embedded_covers_do_not_spend_the_album_budget() {
    let mut f = Fixture::new();
    let path = f.music.join("a.flac");
    let mut file = Probe::open(&path).unwrap().read().unwrap();
    let mut cover = PNG.to_vec();
    cover.resize(1024 * 1024, 0);
    file.primary_tag_mut()
        .unwrap()
        .remove_picture_type(PictureType::CoverFront);
    file.primary_tag_mut().unwrap().push_picture(
        Picture::unchecked(cover)
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Png)
            .build(),
    );
    file.save_to_path(&path, WriteOptions::new()).unwrap();
    let names: Vec<_> = (0..12).map(|n| format!("cover-{n}.flac")).collect();
    for name in &names {
        fs::copy(&path, f.music.join(name)).unwrap();
    }
    let mut cover = PNG.to_vec();
    cover.resize(9 * 1024 * 1024, 0);
    file.primary_tag_mut()
        .unwrap()
        .remove_picture_type(PictureType::CoverFront);
    file.primary_tag_mut().unwrap().push_picture(
        Picture::unchecked(cover)
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Png)
            .build(),
    );
    file.save_to_path(&path, WriteOptions::new()).unwrap();
    f.rescan();
    let names: Vec<_> = names.iter().map(String::as_str).chain(["a.flac"]).collect();
    let ids: Vec<_> = names.iter().map(|name| f.id(name)).collect();
    let hashes = || {
        names
            .iter()
            .map(|name| file_hash(&f.music.join(name)))
            .collect::<Vec<_>>()
    };
    let before = hashes();
    let changes = [set("GENRE", &["Edited"])];
    let preview = f.lib.preview_tag_edit(&ids, &changes).unwrap();
    assert!(preview.skipped.is_empty(), "{preview:?}");
    let report = f
        .lib
        .edit_tags(&ids, &changes, UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, ids, "{report:?}");
    let after = hashes();
    let undo = f.lib.undo_action(report.action.unwrap(), UI).unwrap();
    assert_eq!(hashes(), before);
    f.lib.undo_action(undo.action.unwrap(), UI).unwrap();
    assert_eq!(hashes(), after);
}

#[test]
fn adts_aac_speex_and_fast_start_m4a_are_restored_exactly() {
    let mut f = Fixture::new();
    let fixtures = [
        ("tone.aac", include_bytes!("data/tone.aac").as_slice()),
        (
            "tone-speex.ogg",
            include_bytes!("data/tone-speex.ogg").as_slice(),
        ),
        (
            "tone-faststart.m4a",
            include_bytes!("data/tone-faststart.m4a").as_slice(),
        ),
    ];
    for (name, bytes) in fixtures {
        fs::write(f.music.join(name), bytes).unwrap();
    }
    f.rescan();
    for (name, _) in fixtures {
        edit_genre_and_restore(&f, name);
    }
}

#[test]
fn fast_start_chunk_offsets_still_address_the_same_samples() {
    fn layout(file: &[u8], kind: &[u8; 4], width: usize) -> (usize, Vec<usize>) {
        let mdat = file.windows(4).position(|b| b == b"mdat").unwrap() + 4;
        let table = file.windows(4).position(|b| b == kind).unwrap() + 4;
        let count = u32::from_be_bytes(file[table + 4..table + 8].try_into().unwrap()) as usize;
        assert!(count > 0);
        let offsets = file[table + 8..table + 8 + count * width]
            .chunks_exact(width)
            .map(|b| b.iter().fold(0usize, |n, x| n * 256 + usize::from(*x)))
            .collect();
        (mdat, offsets)
    }
    let mut f = Fixture::new();
    let path = f.music.join("a.m4a");
    let fixtures = [
        (
            include_bytes!("data/tone-faststart.m4a").as_slice(),
            b"stco",
            4,
        ),
        (
            include_bytes!("data/tone-faststart-co64.m4a").as_slice(),
            b"co64",
            8,
        ),
    ];
    for (original, kind, width) in fixtures {
        fs::write(&path, original).unwrap();
        f.rescan();
        let id = f.id("a.m4a");
        let report = f
            .lib
            .edit_tags(
                &[id],
                &[set("TITLE", &[&"q".repeat(6000)])],
                UI,
                TagJobOptions::default(),
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(report.written, [id], "{report:?}");
        let edited = fs::read(&path).unwrap();
        let (old_start, old_offsets) = layout(original, kind, width);
        let (new_start, new_offsets) = layout(&edited, kind, width);
        assert!(new_start > old_start);
        assert_eq!(old_offsets.len(), new_offsets.len());
        for (old, new) in old_offsets.into_iter().zip(new_offsets) {
            assert_eq!(old - old_start, new - new_start);
            assert_eq!(&original[old..old + 16], &edited[new..new + 16]);
        }
        let redo = f
            .lib
            .undo_action(report.action.unwrap(), UI)
            .unwrap()
            .action
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        f.lib.undo_action(redo, UI).unwrap();
        assert_eq!(fs::read(&path).unwrap(), edited);
    }
}

#[test]
fn id3_extended_header_footer_unsync_and_odd_iff_padding_are_restored() {
    let mut f = Fixture::new();
    let mut body = vec![0, 0, 0, 6, 1, 0]; // v2.4 extended header with no optional fields.
    body.extend(b"TIT2\0\0\0\x05\0\0\0A\xff\0\xe1");
    let mut tag = b"ID3\x04\x00\xd0\0\0\0".to_vec();
    tag.push(body.len() as u8);
    tag.extend(body);
    tag.extend(b"3DI");
    tag.extend_from_within(3..10);
    assert_eq!(tag.len() % 2, 1);

    let audio = fs::read(fixture("sine44_a.mp3")).unwrap();
    let skip = if audio.starts_with(b"ID3") {
        10 + audio[6..10]
            .iter()
            .fold(0usize, |n, b| n * 128 + usize::from(*b))
    } else {
        0
    };
    let mut mpeg = tag.clone();
    mpeg.extend(&audio[skip..]);
    fs::write(f.music.join("a.mp3"), mpeg).unwrap();

    let mut form = aiff();
    let mut chunk = b"id3 ".to_vec();
    chunk.extend((tag.len() as u32).to_be_bytes());
    chunk.extend(tag);
    chunk.push(0x9d);
    form.splice(12..12, chunk);
    let size = (form.len() - 8) as u32;
    form[4..8].copy_from_slice(&size.to_be_bytes());
    fs::write(f.music.join("a.aiff"), form).unwrap();
    f.rescan();
    for name in ["a.mp3", "a.aiff"] {
        edit_genre_and_restore(&f, name);
    }
}

#[test]
fn a_thousand_track_job_can_stage_a_small_delta_for_each_track() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    fs::create_dir(&music).unwrap();
    let source = fixture("sine44_a.flac");
    let original = file_hash(&source);
    for index in 0..1000 {
        fs::copy(&source, music.join(format!("{index:04}.flac"))).unwrap();
    }
    let library = Library::open(&dir.path().join("library.sqlite")).unwrap();
    library.add_root(&music, UI).unwrap();
    let ScanStart::Started(scan) = library.scan(|_| {}).unwrap() else {
        panic!("expected scan")
    };
    scan.wait().unwrap();
    let ids = library.track_ids(&TrackQuery::default(), 1000).unwrap();
    assert_eq!(ids.len(), 1000);
    let changes = [set("GENRE", &["Edited"])];
    let preview = library.preview_tag_edit(&ids, &changes).unwrap();
    assert_eq!(preview.files.len(), ids.len(), "{:?}", preview.skipped);
    assert!(
        fs::read_dir(&music)
            .unwrap()
            .all(|e| e.unwrap().path().extension().unwrap() == "flac")
    );
    let report = library
        .edit_tags(&ids, &changes, UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written.len(), ids.len(), "{:?}", report.failed);
    let undo = library.undo_action(report.action.unwrap(), UI).unwrap();
    assert_eq!(undo.value.tracks.len(), ids.len());
    for path in ["0000.flac", "0999.flac"] {
        assert_eq!(file_hash(&music.join(path)), original);
    }
}

#[test]
fn fast_start_m4a_with_a_top_level_item_location_is_refused_in_preview() {
    let mut f = Fixture::new();
    let path = f.music.join("a.m4a");
    let mut bytes = include_bytes!("data/tone-faststart.m4a").to_vec();
    bytes.extend(b"\0\0\0\x1cmeta\0\0\0\0\0\0\0\x10iloc\0\0\0\0\0\0\0\0");
    fs::write(&path, &bytes).unwrap();
    f.rescan();
    let id = f.id("a.m4a");
    let changes = [set("TITLE", &[&"q".repeat(6000)])];
    let preview = f.lib.preview_tag_edit(&[id], &changes).unwrap();
    assert_eq!(preview.skipped, [(id, "not_undoable")]);
    let report = f
        .lib
        .edit_tags(&[id], &changes, UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.failed, [(id, "not_undoable")]);
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn m4a_with_top_level_meta_can_be_edited_when_mdat_stays_put() {
    let mut f = Fixture::new();
    let path = f.music.join("a.m4a");
    let mut bytes = fs::read(fixture("sine44_a.m4a")).unwrap();
    let position = |code: &[u8]| bytes.windows(4).position(|b| b == code).unwrap();
    assert!(position(b"mdat") < position(b"moov"));
    bytes.extend(b"\0\0\0\x1cmeta\0\0\0\0\0\0\0\x10iloc\0\0\0\0\0\0\0\0");
    fs::write(&path, &bytes).unwrap();
    f.rescan();
    edit_genre_and_restore(&f, "a.m4a");
}
