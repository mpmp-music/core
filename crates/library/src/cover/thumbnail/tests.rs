use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use tempfile::TempDir;

use super::*;
use crate::TrackId;
use crate::tests::{fixture, id_of, meta, record, upsert};

fn encode(image: impl Into<DynamicImage>, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    image.into().write_to(&mut out, format).unwrap();
    out.into_inner()
}

/// Pseudo-random pixels, so that every seed is a different picture of a similar size.
fn noise(width: u32, height: u32, seed: u64) -> RgbImage {
    let mut state = seed;
    RgbImage::from_fn(width, height, |_, _| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        Rgb([
            (state >> 33) as u8,
            (state >> 41) as u8,
            (state >> 49) as u8,
        ])
    })
}

/// Red, with a fully transparent top-left quarter.
fn translucent() -> RgbaImage {
    let mut image = RgbaImage::from_pixel(500, 400, Rgba([200, 30, 30, 255]));
    for x in 0..250 {
        for y in 0..200 {
            image.put_pixel(x, y, Rgba([10, 200, 10, 0]));
        }
    }
    image
}

fn art(data: Vec<u8>) -> CoverArt {
    CoverArt {
        mime: sniff_mime(&data).to_owned(),
        data,
    }
}

fn size_of(art: &CoverArt) -> (u32, u32) {
    let image = image::load_from_memory(&art.data).unwrap();
    (image.width(), image.height())
}

fn cache() -> (TempDir, ThumbnailCache) {
    let dir = TempDir::new().unwrap();
    let cache = ThumbnailCache::new(dir.path().join("covers"));
    (dir, cache)
}

/// The names of the files in the cache folder, sorted.
fn files(cache: &ThumbnailCache) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(&cache.dir)
        .map(|read| {
            read.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Makes a cache file look last used `secs` seconds ago.
fn age(cache: &ThumbnailCache, name: &str, secs: u64) {
    let file = OpenOptions::new()
        .write(true)
        .open(cache.dir.join(name))
        .unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(secs))
        .unwrap();
}

#[test]
fn mislabeled_embedded_and_folder_png_decode_at_original_and_thumbnail_sizes() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("mislabeled.flac");
    let data = encode(translucent(), ImageFormat::Png);
    // A real FLAC front-picture block with a deliberately incorrect MIME declaration.
    let mut picture = Vec::new();
    for value in [3u32, 10] {
        picture.extend(value.to_be_bytes());
    }
    picture.extend(b"image/jpeg");
    for value in [0u32, 500, 400, 32, 0, data.len() as u32] {
        picture.extend(value.to_be_bytes());
    }
    picture.extend(&data);
    let mut flac = b"fLaC\0\0\0\x22".to_vec();
    flac.extend([0u8; 34]);
    flac.push(0x86);
    flac.extend(&u32::try_from(picture.len()).unwrap().to_be_bytes()[1..]);
    flac.extend(picture);
    fs::write(&path, flac).unwrap();
    let original = crate::cover::read_cover(&path).unwrap().unwrap();
    assert_eq!(original.mime, "image/png");
    assert_eq!(original.data, data);
    let (_cache_dir, cache) = cache();
    for (edge, expected) in [(1024, (500, 400)), (128, (128, 102))] {
        let result = cache.thumbnail(original.clone(), edge);
        assert_eq!(result.mime, "image/png");
        let decoded = image::ImageReader::with_format(
            Cursor::new(&result.data),
            ImageFormat::from_mime_type(&result.mime).unwrap(),
        )
        .decode()
        .unwrap();
        assert_eq!((decoded.width(), decoded.height()), expected);
    }
    fs::write(dir.path().join("cover.jpg"), &data).unwrap();
    assert_eq!(
        crate::cover::folder_cover(dir.path())
            .unwrap()
            .unwrap()
            .mime,
        "image/png"
    );
}

#[test]
fn requests_round_up_to_a_bucket() {
    let requests = [0, 1, 128, 129, 256, 300, 512, 513, 1024, 1025, u32::MAX];
    let buckets: Vec<_> = requests.into_iter().map(bucket_for).collect();
    let expected = [128, 128, 128, 256, 256, 512, 512, 1024, 1024];
    let expected: Vec<_> = expected.into_iter().map(Some).chain([None, None]).collect();
    assert_eq!(buckets, expected);

    let (_dir, cache) = cache();
    let original = art(encode(noise(2000, 1000, 1), ImageFormat::Jpeg));
    let thumb = cache.thumbnail(original.clone(), 300);
    assert_eq!(thumb.mime, "image/jpeg");
    assert!(thumb.data.starts_with(&[0xFF, 0xD8, 0xFF]));
    assert_eq!(size_of(&thumb), (512, 256));
    // The name is the first 16 bytes of the SHA-256 as hex, then the bucket.
    let key = cache_key(&original.data);
    assert_eq!(key.len(), 32);
    assert!(key.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(files(&cache), [format!("{key}-512.jpg")]);
    // Tall images are bounded by their height; above 1024 the original is served.
    let tall = art(encode(noise(300, 1500, 2), ImageFormat::Png));
    assert_eq!(size_of(&cache.thumbnail(tall.clone(), 1000)), (205, 1024));
    assert_eq!(cache.thumbnail(tall.clone(), 1025), tall);
    assert_eq!(cache.thumbnail(tall.clone(), u32::MAX), tall);
    assert_eq!(files(&cache).len(), 2);
}

#[test]
fn small_originals_are_not_enlarged() {
    let (_dir, cache) = cache();
    let original = art(encode(noise(200, 100, 3), ImageFormat::Png));
    // Fits the 256 bucket and the 1024 one: unchanged, and nothing is cached.
    assert_eq!(cache.thumbnail(original.clone(), 256), original);
    assert_eq!(cache.thumbnail(original.clone(), 1024), original);
    assert!(files(&cache).is_empty());
    // The 128 bucket is smaller than the image: shrunk.
    assert_eq!(size_of(&cache.thumbnail(original.clone(), 100)), (128, 64));
    // Exactly the bucket's size counts as not larger.
    let square = art(encode(noise(128, 128, 4), ImageFormat::Png));
    assert_eq!(cache.thumbnail(square.clone(), 128), square);
}

#[test]
fn second_request_reads_the_cache_and_marks_it_used() {
    let (_dir, cache) = cache();
    let original = art(encode(noise(600, 600, 5), ImageFormat::Jpeg));
    let first = cache.thumbnail(original.clone(), 128);
    let name = files(&cache).remove(0);
    // Replace the file: a hit returns what is on disk, so nothing was encoded again.
    fs::write(cache.dir.join(&name), b"cached").unwrap();
    age(&cache, &name, 1000);
    let second = cache.thumbnail(original.clone(), 128);
    assert_eq!(second.data, b"cached");
    assert_eq!(second.mime, "image/jpeg");
    assert_ne!(first.data, second.data);
    //... and the file counts as used now.
    let modified = fs::metadata(cache.dir.join(&name))
        .unwrap()
        .modified()
        .unwrap();
    assert!(modified > SystemTime::now() - Duration::from_secs(60));
    assert_eq!(files(&cache), [name]);
}

#[test]
fn tracks_sharing_one_image_share_the_thumbnail() {
    let f = fixture();
    let album = f._dir.path().join("music/album");
    fs::create_dir_all(&album).unwrap();
    let cover = encode(noise(700, 700, 6), ImageFormat::Png);
    fs::write(album.join("cover.png"), &cover).unwrap();
    upsert(
        &f.lib,
        vec![
            record(f.root, "album/a.flac", meta("A", "Artist", "Album")),
            record(f.root, "album/b.flac", meta("B", "Artist", "Album")),
        ],
    );
    let (a, b) = (id_of(&f.lib, "A"), id_of(&f.lib, "B"));
    let first = f.lib.cover_thumbnail(a, 200).unwrap().unwrap();
    let second = f.lib.cover_thumbnail(b, 200).unwrap().unwrap();
    assert_eq!(first, second);
    assert_eq!(size_of(&first), (256, 256));
    // In `covers` next to the database, one file for the two tracks.
    let covers = f._dir.path().join("db/covers");
    assert_eq!(fs::read_dir(&covers).unwrap().count(), 1);
    // Another size is another file; the original is what `cover_art` gives.
    f.lib.cover_thumbnail(a, 100).unwrap().unwrap();
    assert_eq!(fs::read_dir(&covers).unwrap().count(), 2);
    assert_eq!(f.lib.cover_thumbnail(a, 5000).unwrap().unwrap().data, cover);
    // An unknown track has no cover.
    assert_eq!(f.lib.cover_thumbnail(TrackId(i64::MAX), 100).unwrap(), None);
}

#[test]
fn least_recently_used_files_are_evicted_over_the_cap() {
    let (_dir, cache) = cache();
    let originals: Vec<_> = (0..4)
        .map(|seed| art(encode(noise(400, 300, 10 + seed), ImageFormat::Png)))
        .collect();
    let name_of = |i: usize| format!("{}-128.jpg", cache_key(&originals[i].data));
    for original in &originals[..3] {
        cache.thumbnail(original.clone(), 128);
    }
    age(&cache, &name_of(0), 300);
    age(&cache, &name_of(1), 200);
    age(&cache, &name_of(2), 100);
    // Using the oldest makes the second one the least recently used.
    cache.thumbnail(originals[0].clone(), 128);

    let three: u64 = (0..3)
        .map(|i| fs::metadata(cache.dir.join(name_of(i))).unwrap().len())
        .sum();
    // Room for about four files of this size, minus a byte: adding the fourth goes over
    // the cap, and removing one file gets under 90% of it.
    cache.set_cap(three + three / 3 - 1);
    cache.thumbnail(originals[3].clone(), 128);
    let mut left = vec![name_of(0), name_of(2), name_of(3)];
    left.sort();
    assert_eq!(files(&cache), left);
    let total: u64 = left
        .iter()
        .map(|n| fs::metadata(cache.dir.join(n)).unwrap().len())
        .sum();
    assert_eq!(*cache.total.lock().unwrap(), Some(total));
    assert!(total <= cache.cap.load(Ordering::Relaxed) / 10 * 9);
}

#[test]
fn a_file_over_the_cap_alone_stays_and_the_rest_goes() {
    let (_dir, cache) = cache();
    cache.set_cap(1);
    let one = art(encode(noise(400, 300, 20), ImageFormat::Png));
    let two = art(encode(noise(400, 300, 21), ImageFormat::Png));
    cache.thumbnail(one, 128);
    cache.thumbnail(two.clone(), 128);
    assert_eq!(files(&cache), [format!("{}-128.jpg", cache_key(&two.data))]);
}

#[test]
fn the_first_scan_counts_what_is_there_and_drops_stale_temp_files() {
    let (_dir, cache) = cache();
    fs::create_dir_all(&cache.dir).unwrap();
    fs::write(cache.dir.join("old-128.jpg"), vec![0; 5000]).unwrap();
    fs::write(cache.dir.join(".tmp-1-1"), b"stale").unwrap();
    fs::write(cache.dir.join(".tmp-2-2"), b"fresh").unwrap();
    age(&cache, ".tmp-1-1", 2 * 3600);
    let original = art(encode(noise(300, 300, 30), ImageFormat::Png));
    let thumb = cache.thumbnail(original, 128);
    assert_eq!(
        *cache.total.lock().unwrap(),
        Some(5000 + thumb.data.len() as u64)
    );
    let names = files(&cache);
    assert!(names.contains(&".tmp-2-2".to_owned()));
    assert!(!names.contains(&".tmp-1-1".to_owned()));
}

#[test]
fn transparent_images_stay_png() {
    let (_dir, cache) = cache();
    let original = art(encode(translucent(), ImageFormat::Png));
    let thumb = cache.thumbnail(original.clone(), 256);
    assert_eq!(thumb.mime, "image/png");
    assert!(thumb.data.starts_with(b"\x89PNG"));
    let decoded = image::load_from_memory(&thumb.data).unwrap().to_rgba8();
    assert_eq!(decoded.dimensions(), (256, 205));
    assert_eq!(decoded.get_pixel(5, 5).0[3], 0);
    assert_eq!(decoded.get_pixel(250, 200).0[3], 255);
    assert!(files(&cache)[0].ends_with("-256.png"));
    // A hit finds the PNG file as well.
    assert_eq!(cache.thumbnail(original, 256), thumb);

    // An alpha channel without a transparent pixel is not worth a PNG.
    let opaque = RgbaImage::from_pixel(500, 400, Rgba([1, 2, 3, 255]));
    let thumb = cache.thumbnail(art(encode(opaque, ImageFormat::Png)), 256);
    assert_eq!(thumb.mime, "image/jpeg");
}

#[test]
fn other_formats_are_decoded_too() {
    let (_dir, cache) = cache();
    let image = noise(300, 200, 40);
    for format in [ImageFormat::Bmp, ImageFormat::Gif, ImageFormat::WebP] {
        let thumb = cache.thumbnail(art(encode(image.clone(), format)), 128);
        assert_eq!(size_of(&thumb), (128, 85), "{format:?}");
    }
}

#[test]
fn what_cannot_be_decoded_is_served_as_it_is() {
    let (dir, cache) = cache();
    let mut truncated = encode(noise(500, 500, 50), ImageFormat::Png);
    truncated.truncate(truncated.len() / 2);
    let mut junk_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
    junk_jpeg.extend_from_slice(&[0x5A; 300]);
    // 16385 pixels wide is over the limit; the file itself is tiny.
    let too_wide = encode(RgbImage::new(16385, 1), ImageFormat::Png);
    let samples = [
        b"not an image at all".to_vec(),
        Vec::new(),
        b"GIF89a....".to_vec(),
        b"RIFF\0\0\0\0WEBPjunk".to_vec(),
        b"BM garbage".to_vec(),
        b"%PDF-1.7 something".to_vec(),
        junk_jpeg,
        truncated,
        too_wide,
    ];
    for data in samples {
        let original = CoverArt {
            mime: "image/whatever".to_owned(),
            data,
        };
        assert_eq!(cache.thumbnail(original.clone(), 128), original);
    }
    assert!(!dir.path().join("covers").exists());
    // The limit is on the size, not on being wide: 16384 pixels are fine.
    let widest = art(encode(RgbImage::new(16384, 1), ImageFormat::Png));
    assert_eq!(size_of(&cache.thumbnail(widest, 128)), (128, 1));
}

#[test]
fn a_cache_folder_that_cannot_be_written_still_gives_the_thumbnail() {
    let dir = TempDir::new().unwrap();
    // A file where the folder should be.
    let blocked = dir.path().join("covers");
    fs::write(&blocked, b"in the way").unwrap();
    let cache = ThumbnailCache::new(blocked);
    let thumb = cache.thumbnail(art(encode(noise(300, 300, 60), ImageFormat::Png)), 128);
    assert_eq!(size_of(&thumb), (128, 128));
}

#[test]
fn the_limit_is_set_on_the_library() {
    let f = fixture();
    assert_eq!(
        f.lib.inner().thumbnails.cap.load(Ordering::Relaxed),
        DEFAULT_COVER_CACHE_BYTES
    );
    f.lib.set_cover_cache_limit(7);
    assert_eq!(f.lib.inner().thumbnails.cap.load(Ordering::Relaxed), 7);
}
