use super::*;
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
pub(crate) fn png() -> Vec<u8> {
    let mut b = Vec::new();
    image::codecs::png::PngEncoder::new(&mut b)
        .write_image(&[17, 23, 41], 1, 1, image::ExtendedColorType::Rgb8)
        .unwrap();
    b
}
use image::ImageEncoder;
fn jpeg() -> Vec<u8> {
    let mut b = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut b)
        .encode(&[11, 22, 33], 1, 1, image::ExtendedColorType::Rgb8)
        .unwrap();
    b
}
#[cfg(feature = "desktop-fs")]
pub(crate) fn max_png() -> Vec<u8> {
    let mut b = png();
    let n = MAX_COVER_IMAGE_BYTES - b.len() - 12;
    let mut chunk = (n as u32).to_be_bytes().to_vec();
    chunk.extend(b"tEXt");
    chunk.extend(b"padding\0");
    chunk.resize(n + 8, b'x');
    let mut crc = 0xffff_ffffu32;
    for byte in &chunk[4..] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb8_8320 } else { 0 };
        }
    }
    chunk.extend((!crc).to_be_bytes());
    b.splice(b.len() - 12..b.len() - 12, chunk);
    assert_eq!(b.len(), MAX_COVER_IMAGE_BYTES);
    validate_image("image/png", &b).unwrap();
    b
}
#[test]
fn inverse_hash_rejects_a_source_changed_and_restored_during_staging() {
    let before = b"HEAD original audio";
    let mut source = tempfile::tempfile().unwrap();
    source.write_all(b"HEAD external audio").unwrap();
    let mut staged = tempfile::tempfile().unwrap();
    let inverse = stage(
        &mut source,
        &mut staged,
        &[Region {
            offset: 0,
            replaced: 4,
            bytes: b"COVR".to_vec(),
        }],
    )
    .unwrap();
    source.rewind().unwrap();
    source.write_all(before).unwrap();
    assert_eq!(file_digest(&mut source).unwrap(), digest(before));
    assert_ne!(
        patched_digest(&mut staged, &inverse).unwrap(),
        digest(before)
    );
}
#[test]
fn id3_chapter_and_index_offsets_refuse_growth_but_allow_existing_padding() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    for version in [3, 4] {
        for kind in [b"CHAP", b"ASPI"] {
            for padding in [0, 4096] {
                let path = dir.path().join("offset.mp3");
                let audio = std::fs::read(fixture("sine44_a.mp3")).unwrap();
                let start = if audio.starts_with(b"ID3") {
                    10 + synchsafe(&audio[6..10]).unwrap() as usize
                } else {
                    0
                };
                let body = if kind == b"CHAP" {
                    let mut body = b"chapter\0".to_vec();
                    body.extend([0; 16]);
                    body
                } else {
                    vec![0; 12]
                };
                let mut frame = kind.to_vec();
                frame.extend(if version == 4 {
                    sync_bytes(body.len()).unwrap()
                } else {
                    (body.len() as u32).to_be_bytes()
                });
                frame.extend([0, 0]);
                frame.extend(body);
                frame.resize(frame.len() + padding, 0);
                let mut bytes = b"ID3".to_vec();
                bytes.extend([version, 0, 0]);
                bytes.extend(sync_bytes(frame.len()).unwrap());
                bytes.extend(frame);
                bytes.extend(&audio[start..]);
                std::fs::write(&path, &bytes).unwrap();
                let change = CoverChange::Set {
                    mime: "image/png".into(),
                    data: png(),
                };
                let result = prepare(&mut File::open(&path).unwrap(), &change);
                if padding == 0 {
                    assert_eq!(result.unwrap_err(), "cover_edit_unsupported");
                } else {
                    let plan = result.unwrap();
                    assert_eq!(plan.regions[0].bytes.len() as u64, plan.regions[0].replaced);
                    let (after, _) = apply(&path, &change);
                    assert_eq!(
                        &after[10 + synchsafe(&after[6..10]).unwrap() as usize..],
                        &audio[start..]
                    );
                }
            }
        }
    }
}
#[test]
fn adts_and_reserved_flac_metadata_are_refused() {
    for bytes in [
        vec![255, 241, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        b"fLaC\xff\0\0\0audio".to_vec(),
    ] {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&bytes).unwrap();
        assert_eq!(
            prepare(
                &mut file,
                &CoverChange::Set {
                    mime: "image/png".into(),
                    data: png()
                }
            )
            .unwrap_err(),
            "cover_edit_unsupported"
        );
    }
}
#[test]
fn newly_created_mp4_meta_contains_a_metadata_handler() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let path = dir.path().join("no-meta.m4a");
    tagged_copy("sine44_a.m4a", &path);
    let old = std::fs::read(&path).unwrap();
    let mut clean = Vec::new();
    for (kind, body) in atoms(&old).unwrap() {
        if kind == b"moov" {
            let mut m = Vec::new();
            for (k, b) in atoms(body).unwrap() {
                if k != b"udta" {
                    m.extend(atom(k, b).unwrap());
                }
            }
            clean.extend(atom(kind, &m).unwrap());
        } else {
            clean.extend(atom(kind, body).unwrap());
        }
    }
    std::fs::write(&path, clean).unwrap();
    apply(
        &path,
        &CoverChange::Set {
            mime: "image/png".into(),
            data: png(),
        },
    );
    let after = std::fs::read(&path).unwrap();
    let moov = atoms(&after)
        .unwrap()
        .into_iter()
        .find(|(k, _)| *k == b"moov")
        .unwrap()
        .1;
    let udta = atoms(moov)
        .unwrap()
        .into_iter()
        .find(|(k, _)| *k == b"udta")
        .unwrap()
        .1;
    let meta = atoms(udta)
        .unwrap()
        .into_iter()
        .find(|(k, _)| *k == b"meta")
        .unwrap()
        .1;
    let items = atoms(&meta[4..]).unwrap();
    assert_eq!(items[0].0, b"hdlr");
    assert_eq!(items[0].1.len(), 25);
    assert_eq!(&items[0].1[8..16], b"mdirappl");
}
#[test]
fn image_budgets_and_canonical_base64() {
    let png = png();
    assert_eq!(decode_base64(&encode_base64(&png)).unwrap(), png);
    for bad in [
        "Zg", "Zg=", "Zh==", "Zm9=", "Zg==\n", "Zg__", "====", "AA=A",
    ] {
        assert!(decode_base64(bad).is_err(), "{bad}");
    }
    assert_eq!(
        CoverChange::from_base64(
            "image/png",
            &"A".repeat(MAX_COVER_IMAGE_BYTES.div_ceil(3) * 4 + 4)
        ),
        Err("image_too_large")
    );
    assert_eq!(validate_image("image/jpeg", &png), Err("invalid_image"));
    let mut damaged = png.clone();
    *damaged.last_mut().unwrap() ^= 1;
    assert_eq!(validate_image("image/png", &damaged), Err("invalid_image"));
    let mut animated = png.clone();
    animated.splice(33..33, b"\0\0\0\0acTL\0\0\0\0".iter().copied());
    assert_eq!(validate_image("image/png", &animated), Err("invalid_image"));
    assert_eq!(
        validate_image("image/png", &vec![0; MAX_COVER_IMAGE_BYTES + 1]),
        Err("image_too_large")
    );
    let mut b = Vec::new();
    image::codecs::png::PngEncoder::new(&mut b)
        .write_image(&vec![0; 8193 * 3], 8193, 1, image::ExtendedColorType::Rgb8)
        .unwrap();
    assert_eq!(validate_image("image/png", &b), Err("image_too_large"));
    let mut b = Vec::new();
    image::codecs::png::PngEncoder::new(&mut b)
        .write_image(
            &vec![0; 4097 * 4097],
            4097,
            4097,
            image::ExtendedColorType::L8,
        )
        .unwrap();
    assert_eq!(validate_image("image/png", &b), Err("image_too_large"));
    assert!(validate_image("image/jpeg", &jpeg()).is_ok());
}
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../engine/tests/fixtures")
        .join(name)
}
pub(crate) fn tagged_copy(name: &str, dest: &std::path::Path) {
    std::fs::copy(fixture(name), dest).unwrap();
    let mut tagged = lofty::probe::Probe::open(dest)
        .unwrap()
        .guess_file_type()
        .unwrap()
        .read()
        .unwrap();
    let primary = tagged.primary_tag_type();
    if tagged.tag(primary).is_none() {
        tagged.insert_tag(lofty::tag::Tag::new(primary));
    }
    let tag = tagged.tag_mut(primary).unwrap();
    tag.set_title("untouched title".into());
    tag.set_artist("untouched artist".into());
    tag.push_picture(
        Picture::unchecked(png())
            .pic_type(PictureType::CoverBack)
            .mime_type(MimeType::Png)
            .build(),
    );
    tagged.save_to_path(dest, WriteOptions::default()).unwrap();
}
fn apply(path: &std::path::Path, change: &CoverChange) -> (Vec<u8>, Vec<Region>) {
    let mut original = File::open(path).unwrap();
    let plan = prepare(&mut original, change).unwrap();
    let mut staged = tempfile::tempfile().unwrap();
    let inverse = stage(&mut original, &mut staged, &plan.regions).unwrap();
    staged.rewind().unwrap();
    let mut b = Vec::new();
    staged.read_to_end(&mut b).unwrap();
    std::fs::write(path, &b).unwrap();
    (b, inverse)
}
use std::fs::File;
#[test]
fn formats_set_replace_remove_preserve_bytes_and_inverse() {
    let d = mpmp_test_support::TempDir::new().unwrap();
    for name in [
        "sine44_a.flac",
        "sine44_a.mp3",
        "sine44_a.m4a",
        "sine44_a.ogg",
        "sine44_a.opus",
    ] {
        let path = d.path().join(name);
        tagged_copy(name, &path);
        let original = std::fs::read(&path).unwrap();
        let png = CoverChange::Set {
            mime: "image/png".into(),
            data: png(),
        };
        let jpeg = CoverChange::Set {
            mime: "image/jpeg".into(),
            data: jpeg(),
        };
        let (_, inverse) = apply(&path, &png);
        assert_eq!(
            patched_digest(&mut File::open(&path).unwrap(), &inverse).unwrap(),
            digest(&original),
            "{name}"
        );
        mpmp_engine::probe_audio(
            std::sync::Arc::new(mpmp_media_io::FileSource::open(&path).unwrap()),
            path.extension().and_then(|s| s.to_str()),
        )
        .unwrap();
        let cover = crate::cover::read_cover(&path).unwrap().unwrap();
        assert_eq!(
            Some(cover.data),
            match &png {
                CoverChange::Set { data, .. } => Some(data.clone()),
                _ => None,
            },
            "{name}"
        );
        let mut file = File::open(&path).unwrap();
        let mut copy = tempfile::tempfile().unwrap();
        stage(&mut file, &mut copy, &inverse).unwrap();
        copy.rewind().unwrap();
        let mut restored = Vec::new();
        copy.read_to_end(&mut restored).unwrap();
        assert_eq!(restored, original, "{name}");
        apply(&path, &jpeg);
        let cover = crate::cover::read_cover(&path).unwrap().unwrap();
        assert_eq!(cover.mime, "image/jpeg", "{name}");
        apply(&path, &CoverChange::Remove);
        let tagged = lofty::probe::Probe::open(&path)
            .unwrap()
            .guess_file_type()
            .unwrap()
            .read()
            .unwrap();
        assert_eq!(
            tagged.primary_tag().unwrap().title().as_deref(),
            Some("untouched title"),
            "{name}"
        );
        let pics: Vec<_> = tagged.tags().iter().flat_map(|t| t.pictures()).collect();
        if name.ends_with("m4a") {
            assert!(pics.is_empty());
        } else {
            assert_eq!(pics.len(), 1, "{name}");
            assert_eq!(pics[0].pic_type(), PictureType::CoverBack, "{name}");
            assert_eq!(pics[0].data(), self::png());
        }
    }
}
#[test]
fn huge_old_regions_are_refused_before_copying_audio() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"ID3\x04\x00\x00").unwrap();
    file.write_all(&sync_bytes(MAX_COVER_UNDO_BYTES + 1).unwrap())
        .unwrap();
    file.set_len((MAX_COVER_UNDO_BYTES + 11) as u64).unwrap();
    assert_eq!(
        prepare(&mut file, &CoverChange::Remove).unwrap_err(),
        "undo_too_large"
    );
}
