//! Encoding scenarios.
use super::*;

#[test]
fn unrepresentable_values_leave_the_original_file_untouched() {
    let f = Fixture::new();
    for (name, changes) in [
        (
            "a.mp3",
            vec![set("TRACKNUMBER", &["abc"]), set("TITLE", &["changed"])],
        ),
        ("a.mp3", vec![set("DATE", &["not a date"])]),
        ("a.m4a", vec![set("DISCNUMBER", &["65536"])]),
        ("a.m4a", vec![set("TRACKNUMBER", &["1", "2"])]),
    ] {
        let id = f.id(name);
        let path = f.music.join(name);
        let original = fs::read(&path).unwrap();
        let preview = f.lib.preview_tag_edit(&[id], &changes).unwrap();
        assert_eq!(
            preview.skipped,
            [(id, "invalid_tag")],
            "{name}: {preview:?}"
        );
        assert!(preview.files.is_empty());
        let report = f
            .lib
            .edit_tags(&[id], &changes, UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(report.failed, [(id, "invalid_tag")], "{name}: {report:?}");
        assert!(report.action.is_none());
        assert_eq!(fs::read(path).unwrap(), original);
    }
}

#[test]
fn long_utf8_names_can_be_edited() {
    let mut f = Fixture::new();
    let name = format!("{}.mp3", "歌".repeat(80));
    fs::copy(f.music.join("a.mp3"), f.music.join(&name)).unwrap();
    f.rescan();
    let id = f.id(&name);
    let report = f
        .lib
        .edit_tags(
            &[id],
            &[set("TITLE", &["new"])],
            UI,
            TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, [id], "{report:?}");
    assert!(report.failed.is_empty());
}

/// A folder whose encoding is set to GBK, an MP3 whose Latin-1 frames hold GBK text that
/// only that tells: saving values as shown writes
/// nothing; editing one key of a frame that holds two keeps the other as shown; the
/// edit's undo writes it all back.
#[test]
fn gbk_in_latin1_frames_of_a_folder_set_to_gbk() {
    use lofty::id3::v2::{Frame, FrameId, KeyValueFrame};
    let mut f = Fixture::new();
    let latin1 = TextEncoding::Latin1;
    let people = Frame::KeyValue(KeyValueFrame::new(
        FrameId::Valid(std::borrow::Cow::Borrowed("TIPL")),
        latin1,
        vec![
            ("producer".into(), gbk_latin1("周砚舟").into()),
            ("arranger".into(), gbk_latin1("林砚河").into()),
        ],
    ));
    mp3_frames(
        &mut f,
        vec![
            text_frame("TIT2", latin1, "Qi Li Xiang".into()),
            text_frame("TCOM", latin1, gbk_latin1("方文山")),
            people,
        ],
    );
    let root = f.lib.roots().unwrap()[0].id;
    f.lib.set_root_encoding(root, Some("gbk")).unwrap();
    f.rescan();
    let id = f.id("a.mp3");
    let path = f.music.join("a.mp3");
    let keys = ["COMPOSER", "PRODUCER", "ARRANGER", "TITLE"];
    let shown = values(&[
        ("COMPOSER", &["方文山"]),
        ("PRODUCER", &["周砚舟"]),
        ("ARRANGER", &["林砚河"]),
        ("TITLE", &["Qi Li Xiang"]),
    ]);
    assert_eq!(f.tags(id, &keys), shown);
    // As shown: nothing to write.
    let untouched = fs::read(&path).unwrap();
    let as_shown = [
        set("COMPOSER", &["方文山"]),
        set("PRODUCER", &["周砚舟"]),
        set("ARRANGER", &["林砚河"]),
    ];
    let preview = f.lib.preview_tag_edit(&[id], &as_shown).unwrap();
    assert!(preview.skipped.is_empty(), "{preview:?}");
    let report = f
        .lib
        .edit_tags(&[id], &as_shown, UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert!(
        report.written.is_empty() && report.failed.is_empty(),
        "{report:?}"
    );
    assert_eq!(fs::read(&path).unwrap(), untouched);
    // One key of the `TIPL`: the other is written again as shown, not as mojibake.
    let report = f
        .lib
        .edit_tags(
            &[id],
            &[set("PRODUCER", &["张三"])],
            UI,
            TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, [id], "{report:?}");
    let mut after = shown.clone();
    after.insert("PRODUCER".into(), vec!["张三".into()]);
    assert_eq!(f.tags(id, &keys), after);
    let undo = f
        .lib
        .undo_tag_edit(report.action.unwrap(), UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(undo.written, [id], "{undo:?}");
    assert_eq!(f.tags(id, &keys), shown);
}

/// A people frame with only native roles survives adding a mapped role, undo and redo.
#[test]
fn native_only_people_frame_survives_edit_undo_and_redo() {
    use lofty::id3::v2::{Frame, FrameId, KeyValueFrame};
    let mut f = Fixture::new();
    mp3_frames(
        &mut f,
        vec![Frame::KeyValue(KeyValueFrame::new(
            FrameId::Valid("TIPL".into()),
            TextEncoding::UTF8,
            vec![("mastering".into(), "某人".into())],
        ))],
    );
    let id = f.id("a.mp3");
    let native = || {
        let file = lofty::mpeg::MpegFile::read_from(
            &mut fs::File::open(f.music.join("a.mp3")).unwrap(),
            lofty::config::ParseOptions::new().read_properties(false),
        )
        .unwrap();
        file.id3v2()
            .unwrap()
            .into_iter()
            .find_map(|frame| match frame {
                Frame::KeyValue(list) => Some(
                    list.key_value_pairs
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(native(), [("mastering".into(), "某人".into())]);
    let mut report = f
        .lib
        .edit_tags(
            &[id],
            &[set("PRODUCER", &["制作人"])],
            UI,
            TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    for (step, producer) in [true, false, true].into_iter().enumerate() {
        assert_eq!(report.written, [id], "{report:?}");
        let pairs = native();
        assert_eq!(
            pairs
                .iter()
                .filter(|(k, _)| k == "mastering")
                .collect::<Vec<_>>(),
            [&("mastering".into(), "某人".into())]
        );
        assert_eq!(
            pairs.iter().any(|(k, v)| k == "producer" && v == "制作人"),
            producer
        );
        assert_eq!(pairs.len(), if producer { 2 } else { 1 });
        if step == 2 {
            break;
        }
        report = f
            .lib
            .undo_tag_edit(report.action.unwrap(), UI, TagJobOptions::default())
            .unwrap()
            .unwrap()
            .wait()
            .unwrap();
    }
}

/// Editing the detection evidence preserves short legacy fields and explicit equal Sets.
#[test]
fn changing_automatic_encoding_evidence_keeps_display_values_through_undo() {
    for composer in [false, true] {
        let mut f = Fixture::new();
        let mut frames = vec![
            text_frame("TIT2", TextEncoding::Latin1, gbk_latin1("三棱雾")),
            text_frame("TPE1", TextEncoding::Latin1, gbk_latin1("刀")),
        ];
        if composer {
            frames.push(text_frame(
                "TCOM",
                TextEncoding::Latin1,
                gbk_latin1("方文山"),
            ));
        }
        mp3_frames(&mut f, frames);
        let id = f.id("a.mp3");
        let keys = ["TITLE", "ARTIST", "COMPOSER"];
        let before = f.tags(id, &keys);
        assert_eq!(before["TITLE"], ["三棱雾"]);
        assert_eq!(before["ARTIST"], ["刀"]);
        let mut changes = vec![set("TITLE", &["新标题"])];
        if composer {
            changes.push(set("COMPOSER", &["方文山"]));
        }
        let preview = f.lib.preview_tag_edit(&[id], &changes).unwrap();
        assert!(preview.skipped.is_empty(), "{preview:?}");
        let report = f
            .lib
            .edit_tags(&[id], &changes, UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(report.written, [id], "{report:?}");
        let mut after = before.clone();
        after.insert("TITLE".into(), vec!["新标题".into()]);
        assert_eq!(f.tags(id, &keys), after);
        f.rescan();
        assert_eq!(f.tags(id, &keys), after);
        let undone = f
            .lib
            .undo_tag_edit(report.action.unwrap(), UI, TagJobOptions::default())
            .unwrap()
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(undone.written, [id], "{undone:?}");
        assert_eq!(f.tags(id, &keys), before);
        f.rescan();
        assert_eq!(f.tags(id, &keys), before);
    }
}

/// Both detection and a root override convert unknown people roles, including duplicate
/// roles and non-ASCII role names. Undo restores the entire original ID3 payload.
#[test]
fn legacy_people_conversion_is_lossless_through_undo_and_redo() {
    use lofty::id3::v2::{Frame, FrameId, KeyValueFrame};
    for forced in [false, true] {
        for mixed in [false, true] {
            for frame_id in ["TIPL", "TMCL"] {
                let mut f = Fixture::new();
                let mut pairs = vec![
                    ("mastering".into(), gbk_latin1("某人").into()),
                    ("mastering".into(), gbk_latin1("另一人").into()),
                    (gbk_latin1("未知角色").into(), gbk_latin1("姓名").into()),
                ];
                if mixed {
                    pairs.push(("arranger".into(), gbk_latin1("林砚河").into()));
                }
                mp3_frames(
                    &mut f,
                    vec![
                        text_frame("TIT2", TextEncoding::Latin1, gbk_latin1("三棱雾")),
                        text_frame("TPE1", TextEncoding::Latin1, gbk_latin1("周砚舟")),
                        Frame::KeyValue(KeyValueFrame::new(
                            FrameId::Valid(frame_id.into()),
                            TextEncoding::Latin1,
                            pairs,
                        )),
                    ],
                );
                if forced {
                    let root = f.lib.roots().unwrap()[0].id;
                    f.lib.set_root_encoding(root, Some("gbk")).unwrap();
                    f.rescan();
                }
                let id = f.id("a.mp3");
                let path = f.music.join("a.mp3");
                let original = fs::read(&path).unwrap();
                let changes = [set("PRODUCER", &["制作人"])];
                let preview = f.lib.preview_tag_edit(&[id], &changes).unwrap();
                assert!(preview.skipped.is_empty(), "{preview:?}");
                assert!(preview.files[0].encoding_conversion);
                let report = f
                    .lib
                    .edit_tags(&[id], &changes, UI, TagJobOptions::default())
                    .unwrap()
                    .wait()
                    .unwrap();
                assert_eq!(report.written, [id], "{report:?}");
                assert_eq!(report.encoding_converted, [id]);
                let converted = fs::read(&path).unwrap();
                let assert_people = || {
                    let file = lofty::mpeg::MpegFile::read_from(
                        &mut fs::File::open(&path).unwrap(),
                        lofty::config::ParseOptions::new().read_properties(false),
                    )
                    .unwrap();
                    let list = file
                        .id3v2()
                        .unwrap()
                        .into_iter()
                        .find_map(|frame| match frame {
                            Frame::KeyValue(t) if t.id().as_str() == frame_id => Some(t),
                            _ => None,
                        })
                        .unwrap();
                    assert_ne!(list.encoding, TextEncoding::Latin1);
                    let pairs: Vec<_> = list
                        .key_value_pairs
                        .iter()
                        .map(|(k, v)| (k.as_ref(), v.as_ref()))
                        .collect();
                    assert_eq!(
                        pairs
                            .iter()
                            .filter(|(k, _)| *k == "mastering")
                            .copied()
                            .collect::<Vec<_>>(),
                        [("mastering", "某人"), ("mastering", "另一人")]
                    );
                    assert!(pairs.contains(&("未知角色", "姓名")));
                    assert_eq!(f.tags(id, &["PRODUCER"])["PRODUCER"], ["制作人"]);
                };
                assert_people();
                let undone = f
                    .lib
                    .undo_tag_edit(report.action.unwrap(), UI, TagJobOptions::default())
                    .unwrap()
                    .unwrap()
                    .wait()
                    .unwrap();
                assert_eq!(undone.written, [id], "{undone:?}");
                assert_eq!(fs::read(&path).unwrap(), original);
                let redone = f
                    .lib
                    .undo_tag_edit(undone.action.unwrap(), UI, TagJobOptions::default())
                    .unwrap()
                    .unwrap()
                    .wait()
                    .unwrap();
                assert_eq!(redone.written, [id], "{redone:?}");
                assert_eq!(fs::read(&path).unwrap(), converted);
                assert_people();
            }
        }
    }
}

#[test]
fn unrelated_edit_converts_legacy_frames_and_restores_original_bytes() {
    use lofty::id3::v2::{Frame, Id3v2Tag};
    for name in ["a.mp3", "a.wav", "a.aiff"] {
        let mut f = Fixture::new();
        let path = f.music.join(name);
        let mut tag = Id3v2Tag::new();
        tag.insert(text_frame(
            "TIT2",
            TextEncoding::Latin1,
            gbk_latin1("三棱雾"),
        ));
        tag.insert(text_frame(
            "TPE1",
            TextEncoding::Latin1,
            gbk_latin1("周砚舟"),
        ));
        tag.insert(text_frame(
            "TCOM",
            TextEncoding::Latin1,
            gbk_latin1("方文山"),
        ));
        tag.save_to_path(&path, WriteOptions::new().use_id3v23(true))
            .unwrap();
        f.rescan();
        let id = f.id(name);
        let original = fs::read(&path).unwrap();
        let before = f.tags(id, &["TITLE", "ARTIST", "COMPOSER"]);
        let changes = [set("GENRE", &["Pop"])];
        let preview = f.lib.preview_tag_edit(&[id], &changes).unwrap();
        assert!(preview.skipped.is_empty(), "{name}: {preview:?}");
        assert!(preview.files[0].encoding_conversion);
        let report = f
            .lib
            .edit_tags(&[id], &changes, UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(report.written, [id], "{name}: {report:?}");
        assert_eq!(report.encoding_converted, [id]);
        assert_eq!(f.tags(id, &["TITLE", "ARTIST", "COMPOSER"]), before);
        let after = fs::read(&path).unwrap();
        let file = lofty::read_from_path(&path).unwrap();
        let id3 = Id3v2Tag::from(file.tag(lofty::tag::TagType::Id3v2).unwrap().clone());
        assert!(
            id3.into_iter().all(
                |frame| !matches!(frame, Frame::Text(t) if t.encoding == TextEncoding::Latin1)
            )
        );
        let undo = f
            .lib
            .undo_tag_edit(report.action.unwrap(), UI, TagJobOptions::default())
            .unwrap()
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(undo.written, [id], "{name}: {undo:?}");
        assert_eq!(fs::read(&path).unwrap(), original);
        let redo = f
            .lib
            .undo_tag_edit(undo.action.unwrap(), UI, TagJobOptions::default())
            .unwrap()
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(redo.written, [id], "{name}: {redo:?}");
        assert_eq!(redo.encoding_converted, [id]);
        assert_eq!(fs::read(&path).unwrap(), after);
    }
}

#[test]
fn undecodable_native_role_refuses_the_file_before_writing() {
    use lofty::id3::v2::{Frame, FrameId, KeyValueFrame};
    let mut f = Fixture::new();
    mp3_frames(
        &mut f,
        vec![
            text_frame("TIT2", TextEncoding::Latin1, gbk_latin1("三棱雾")),
            Frame::KeyValue(KeyValueFrame::new(
                FrameId::Valid("TIPL".into()),
                TextEncoding::Latin1,
                vec![("mastering".into(), "\u{81}".into())],
            )),
        ],
    );
    let root = f.lib.roots().unwrap()[0].id;
    f.lib.set_root_encoding(root, Some("gbk")).unwrap();
    f.rescan();
    let id = f.id("a.mp3");
    let original = fs::read(f.music.join("a.mp3")).unwrap();
    let changes = [set("GENRE", &["Pop"])];
    assert_eq!(
        f.lib.preview_tag_edit(&[id], &changes).unwrap().skipped,
        [(id, "invalid_tag")]
    );
    let report = f
        .lib
        .edit_tags(&[id], &changes, UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.failed, [(id, "invalid_tag")]);
    assert_eq!(fs::read(f.music.join("a.mp3")).unwrap(), original);
}

#[test]
fn conversion_payload_counts_against_the_undo_budget() {
    let mut f = Fixture::new();
    mp3_frames(
        &mut f,
        vec![
            text_frame("TIT2", TextEncoding::Latin1, gbk_latin1("三棱雾")),
            text_frame(
                "TCOM",
                TextEncoding::Latin1,
                gbk_latin1(&"方文山".repeat(1_500_000)),
            ),
        ],
    );
    let id = f.id("a.mp3");
    let path = f.music.join("a.mp3");
    let original = fs::read(&path).unwrap();
    let changes = [set("GENRE", &["Pop"])];
    assert_eq!(
        f.lib.preview_tag_edit(&[id], &changes).unwrap().skipped,
        [(id, "undo_too_large")]
    );
    let report = f
        .lib
        .edit_tags(&[id], &changes, UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.failed, [(id, "undo_too_large")]);
    assert!(report.action.is_none());
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn legacy_comments_convert_without_reinterpreting_unicode_siblings() {
    use lofty::id3::v2::{CommentFrame, Frame};
    let mut f = Fixture::new();
    mp3_frames(
        &mut f,
        vec![
            text_frame("TIT2", TextEncoding::Latin1, gbk_latin1("三棱雾")),
            Frame::Comment(CommentFrame::new(
                TextEncoding::Latin1,
                *b"eng",
                "native",
                gbk_latin1("某人"),
            )),
            Frame::Comment(CommentFrame::new(
                TextEncoding::UTF8,
                *b"eng",
                "unicode",
                "Café",
            )),
        ],
    );
    let id = f.id("a.mp3");
    let path = f.music.join("a.mp3");
    let original = fs::read(&path).unwrap();
    let before = f.tags(id, &["COMMENT"]);
    assert!(before["COMMENT"].contains(&"某人".into()));
    assert!(before["COMMENT"].contains(&"Café".into()));
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
    assert_eq!(f.tags(id, &["COMMENT"]), before);
    let undone = f
        .lib
        .undo_tag_edit(report.action.unwrap(), UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(undone.written, [id], "{undone:?}");
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn unknown_native_role_encoding_refuses_merging_unicode_values() {
    use lofty::id3::v2::{Frame, FrameId, KeyValueFrame};
    let mut f = Fixture::new();
    mp3_frames(
        &mut f,
        vec![Frame::KeyValue(KeyValueFrame::new(
            FrameId::Valid("TIPL".into()),
            TextEncoding::Latin1,
            vec![("mastering".into(), gbk_latin1("某人").into())],
        ))],
    );
    let id = f.id("a.mp3");
    let original = fs::read(f.music.join("a.mp3")).unwrap();
    let changes = [set("PRODUCER", &["制作人"])];
    assert_eq!(
        f.lib.preview_tag_edit(&[id], &changes).unwrap().skipped,
        [(id, "invalid_tag")]
    );
    let report = f
        .lib
        .edit_tags(&[id], &changes, UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.failed, [(id, "invalid_tag")]);
    assert_eq!(fs::read(f.music.join("a.mp3")).unwrap(), original);
}

#[test]
fn replacing_the_only_detection_evidence_converts_native_people_too() {
    use lofty::id3::v2::{Frame, FrameId, KeyValueFrame};
    let mut f = Fixture::new();
    mp3_frames(
        &mut f,
        vec![
            text_frame("TIT2", TextEncoding::Latin1, gbk_latin1("三棱雾")),
            Frame::KeyValue(KeyValueFrame::new(
                FrameId::Valid("TIPL".into()),
                TextEncoding::Latin1,
                vec![
                    ("producer".into(), gbk_latin1("周砚舟").into()),
                    ("mastering".into(), gbk_latin1("某人").into()),
                ],
            )),
        ],
    );
    let id = f.id("a.mp3");
    let path = f.music.join("a.mp3");
    let original = fs::read(&path).unwrap();
    let report = f
        .lib
        .edit_tags(
            &[id],
            &[set("TITLE", &["Qi Li Xiang"])],
            UI,
            TagJobOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, [id], "{report:?}");
    assert_eq!(f.tags(id, &["PRODUCER"])["PRODUCER"], ["周砚舟"]);
    let undo = f
        .lib
        .undo_tag_edit(report.action.unwrap(), UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(undo.written, [id], "{undo:?}");
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn removing_the_last_legacy_frame_preserves_undo_and_redo() {
    let mut f = Fixture::new();
    mp3_frames(
        &mut f,
        vec![text_frame(
            "TIT2",
            TextEncoding::Latin1,
            gbk_latin1("三棱雾"),
        )],
    );
    let id = f.id("a.mp3");
    let path = f.music.join("a.mp3");
    let original = fs::read(&path).unwrap();
    let report = f
        .lib
        .edit_tags(&[id], &[remove("TITLE")], UI, TagJobOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(report.written, [id], "{report:?}");
    let after = fs::read(&path).unwrap();
    let undo = f
        .lib
        .undo_tag_edit(report.action.unwrap(), UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(undo.written, [id], "{undo:?}");
    assert_eq!(fs::read(&path).unwrap(), original);
    let redo = f
        .lib
        .undo_tag_edit(undo.action.unwrap(), UI, TagJobOptions::default())
        .unwrap()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(redo.written, [id], "{redo:?}");
    assert_eq!(fs::read(&path).unwrap(), after);
}

#[test]
fn nested_legacy_text_is_rejected_in_preview_and_execution() {
    use lofty::id3::v2::{ChapterFrame, ChapterTableOfContentsFrame, CtocFlags, Frame, FrameList};
    for depth in 0..3 {
        let mut f = Fixture::new();
        let mut children = FrameList::default();
        children.insert(text_frame(
            "TIT2",
            TextEncoding::Latin1,
            gbk_latin1("三棱雾"),
        ));
        let chapter = Frame::Chapter(ChapterFrame::new(
            "chapter",
            0..1000,
            u32::MAX..u32::MAX,
            children.clone(),
        ));
        let nested = if depth > 0 {
            if depth == 2 {
                children = FrameList::default();
                children.insert(chapter);
            }
            Frame::TableOfContents(ChapterTableOfContentsFrame::new(
                "toc",
                CtocFlags {
                    top_level: true,
                    ordered: false,
                },
                vec![],
                children,
            ))
        } else {
            chapter
        };
        mp3_frames(
            &mut f,
            vec![
                text_frame("TIT2", TextEncoding::UTF8, "Unicode".into()),
                nested,
            ],
        );
        let root = f.lib.roots().unwrap()[0].id;
        f.lib.set_root_encoding(root, Some("gbk")).unwrap();
        f.rescan();
        let id = f.id("a.mp3");
        let path = f.music.join("a.mp3");
        let original = fs::read(&path).unwrap();
        let changes = [set("GENRE", &["Pop"])];
        assert_eq!(
            f.lib.preview_tag_edit(&[id], &changes).unwrap().skipped,
            [(id, "invalid_tag")]
        );
        let report = f
            .lib
            .edit_tags(&[id], &changes, UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(report.failed, [(id, "invalid_tag")]);
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}

/// Real legacy taggers wrote compact v2.3 tags, without lofty padding or UTF-16.
#[test]
fn compact_gbk_v23_with_and_without_id3v1_is_editable_and_byte_exact_undoable() {
    for (id3v1, padding, secondary) in [false, true].into_iter().flat_map(|v1| {
        [0, 1501, 20000]
            .into_iter()
            .flat_map(move |p| [false, true].map(|second| (v1, p, second)))
    }) {
        let mut f = Fixture::new();
        let path = f.music.join("a.mp3");
        let audio = fs::read(fixture("sine44_a.mp3")).unwrap();
        let skip = if audio.starts_with(b"ID3") {
            10 + audio[6..10]
                .iter()
                .fold(0usize, |n, b| (n << 7) | usize::from(*b))
        } else {
            0
        };
        let mut body = Vec::new();
        for (id, value) in [
            (b"TIT2", "三棱雾"),
            (b"TPE1", "周砚舟"),
            (b"TALB", "三棱雾专辑"),
        ] {
            let (gbk, _, _) = encoding_rs::GBK.encode(value);
            body.extend(id);
            body.extend(((gbk.len() + 1) as u32).to_be_bytes());
            body.extend([0, 0, 0]); // frame flags and Latin-1 encoding byte
            body.extend(gbk.as_ref());
        }
        body.extend(vec![0; padding]);
        let mut original = b"ID3\x03\x00\x00".to_vec();
        for shift in [21, 14, 7, 0] {
            original.push(((body.len() >> shift) & 127) as u8);
        }
        original.extend(body);
        if secondary {
            // Consecutive ID3 blocks that lofty merges, as left by some tagging tools.
            let mut extra =
                b"ID3\x03\x00\x00\x00\x00\x00\x11TCOM\x00\x00\x00\x07\x00\x00\x00".to_vec();
            extra.extend(encoding_rs::GBK.encode("方文山").0.as_ref());
            original.extend(extra);
        }
        original.extend(&audio[skip..]);
        if id3v1 {
            original.extend(b"TAG");
            original.extend([0; 125]);
        }
        fs::write(&path, &original).unwrap();
        f.rescan();
        let id = f.id("a.mp3");
        assert_eq!(f.tags(id, &["TITLE"])["TITLE"], ["三棱雾"]);
        let changes = [set("GENRE", &["Pop"])];
        let preview = f.lib.preview_tag_edit(&[id], &changes).unwrap();
        assert!(
            preview.skipped.is_empty(),
            "id3v1={id3v1}, padding={padding}, secondary={secondary}: {preview:?}"
        );
        assert!(preview.files[0].encoding_conversion);
        let report = f
            .lib
            .edit_tags(&[id], &changes, UI, TagJobOptions::default())
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(report.written, [id], "{report:?}");
        assert_eq!(report.encoding_converted, [id]);
        let after = fs::read(&path).unwrap();
        // All padding belongs to the syncsafe tag length; no stray bytes precede audio.
        fn audio_start(bytes: &[u8]) -> usize {
            let mut at = 0;
            while bytes[at..].starts_with(b"ID3") {
                assert_eq!(&bytes[at + 3..at + 6], &[3, 0, 0]);
                assert!(bytes[at + 6..at + 10].iter().all(|b| *b < 128));
                at += 10
                    + bytes[at + 6..at + 10]
                        .iter()
                        .fold(0usize, |n, b| (n << 7) | usize::from(*b));
            }
            at
        }
        assert_eq!(
            &after[audio_start(&after)..],
            &original[audio_start(&original)..]
        );
        assert_eq!(f.tags(id, &["TITLE"])["TITLE"], ["三棱雾"]);
        let undo = f
            .lib
            .undo_tag_edit(report.action.unwrap(), UI, TagJobOptions::default())
            .unwrap()
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(undo.written, [id], "{undo:?}");
        assert_eq!(fs::read(&path).unwrap(), original);
        let redo = f
            .lib
            .undo_tag_edit(undo.action.unwrap(), UI, TagJobOptions::default())
            .unwrap()
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(redo.written, [id]);
        assert_eq!(fs::read(&path).unwrap(), after);
    }
}
