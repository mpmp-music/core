//! Cover thumbnails (docs/design/media-resources.md): a cover is decoded, shrunk to
//! one of a few sizes and kept in a disk cache next to the database, keyed by the SHA-256
//! of the original image, so the tracks of an album that embed one picture share one file.
//!
//! The image bytes are untrusted: decoding is bounded in dimensions and memory and runs
//! under `catch_unwind`. Whenever no thumbnail can or need be made, the caller gets the
//! original back.

use std::fs::{self, OpenOptions};
use std::io::{self, Cursor};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageReader, Limits};
use sha2::{Digest, Sha256};

use crate::cover::{CoverArt, sniff_mime};

/// Default upper bound of the cache's total size.
pub const DEFAULT_COVER_CACHE_BYTES: u64 = 512 * 1024 * 1024;

/// Longest edges (pixels) a thumbnail can have; a request is rounded up to one of them.
const BUCKETS: [u32; 4] = [128, 256, 512, 1024];

/// JPEG quality of generated thumbnails.
const JPEG_QUALITY: u8 = 85;

/// Largest width and height of an original that is decoded.
const MAX_DIMENSION: u32 = 16384;

/// Largest allocation a decoder may make for an original.
const MAX_ALLOC: u64 = 256 * 1024 * 1024;

/// Prefix of in-flight files in the cache folder; they are not cache entries.
const TEMP_PREFIX: &str = ".tmp-";

/// Age after which a temp file found by the first scan is a leftover of a crash.
const STALE_TEMP: Duration = Duration::from_secs(3600);

/// The smallest bucket that holds `max_edge` pixels; `None` above the largest (the original
/// is served then). 0 counts as the smallest bucket.
fn bucket_for(max_edge: u32) -> Option<u32> {
    BUCKETS.into_iter().find(|&bucket| max_edge <= bucket)
}

/// The formats that are decoded, by header; anything else is served as it is.
fn is_decodable(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "image/bmp"
    )
}

/// Lower-case hex of the first 16 bytes of the SHA-256 of `data`.
fn cache_key(data: &[u8]) -> String {
    use std::fmt::Write;
    Sha256::digest(data)[..16]
        .iter()
        .fold(String::with_capacity(32), |mut key, byte| {
            let _ = write!(key, "{byte:02x}");
            key
        })
}

fn mime_of(png: bool) -> &'static str {
    if png { "image/png" } else { "image/jpeg" }
}

/// What making a thumbnail came to.
enum Made {
    /// The original is no larger than the bucket: serve it unchanged.
    Original,
    Thumbnail {
        data: Vec<u8>,
        png: bool,
    },
}

/// The thumbnail folder and its size accounting.
pub(crate) struct ThumbnailCache {
    dir: PathBuf,
    /// Upper bound of the total size of the cache files, in bytes.
    cap: AtomicU64,
    /// Total size of the cache files: `None` until the first scan of the folder (made by the
    /// first thing that adds a file), then kept up to date by adding and evicting.
    total: Mutex<Option<u64>>,
    /// Makes temp file names unique within this process.
    next_temp: AtomicU64,
}

impl ThumbnailCache {
    pub(crate) fn new(dir: PathBuf) -> Self {
        ThumbnailCache {
            dir,
            cap: AtomicU64::new(DEFAULT_COVER_CACHE_BYTES),
            total: Mutex::new(None),
            next_temp: AtomicU64::new(0),
        }
    }

    /// Sets the size the cache is kept under; files are only evicted when one is added.
    pub(crate) fn set_cap(&self, bytes: u64) {
        self.cap.store(bytes, Ordering::Relaxed);
    }

    /// The thumbnail of `original` for a longest edge of `max_edge` pixels (rounded up to a
    /// bucket), from the cache or made now. The original itself when it is above the
    /// largest bucket, not larger than the bucket, not a decodable format, or fails to
    /// decode. Blocks on the calling thread while decoding.
    pub(crate) fn thumbnail(&self, original: CoverArt, max_edge: u32) -> CoverArt {
        let Some(bucket) = bucket_for(max_edge) else {
            return original;
        };
        let mime = sniff_mime(&original.data);
        if !is_decodable(mime) {
            tracing::debug!(mime, "cover is not a format to make thumbnails of");
            return original;
        }
        let key = cache_key(&original.data);
        if let Some(cached) = self.lookup(&key, bucket) {
            return cached;
        }
        let made = panic::catch_unwind(AssertUnwindSafe(|| make(&original.data, bucket)))
            .unwrap_or_else(|_| Err("image decoder panicked".to_owned()));
        match made {
            Ok(Made::Original) => original,
            Ok(Made::Thumbnail { data, png }) => {
                if let Err(err) = self.store(&key, bucket, png, &data) {
                    // The thumbnail is good; only keeping it failed.
                    tracing::debug!(%err, dir = %self.dir.display(), "cover thumbnail not cached");
                }
                CoverArt {
                    mime: mime_of(png).to_owned(),
                    data,
                }
            }
            Err(err) => {
                tracing::debug!(%err, "no cover thumbnail, serving the original");
                original
            }
        }
    }

    fn entry_path(&self, key: &str, bucket: u32, png: bool) -> PathBuf {
        let ext = if png { "png" } else { "jpg" };
        self.dir.join(format!("{key}-{bucket}.{ext}"))
    }

    /// The cached thumbnail, if there is one; marks it as used now.
    fn lookup(&self, key: &str, bucket: u32) -> Option<CoverArt> {
        [false, true].into_iter().find_map(|png| {
            let path = self.entry_path(key, bucket, png);
            let data = crate::cover::bounded_image(&path).ok()??;
            // The modified time is the last use (a failure only makes the file look older).
            let touched = OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(SystemTime::now()));
            if let Err(err) = touched {
                tracing::debug!(%err, path = %path.display(), "cover thumbnail not touched");
            }
            Some(CoverArt {
                mime: mime_of(png).to_owned(),
                data,
            })
        })
    }

    /// Writes a new cache file (through a temp file and a rename, so a reader never sees a
    /// partial one and two writers of the same entry do no harm) and keeps the cache under
    /// its cap.
    fn store(&self, key: &str, bucket: u32, png: bool, data: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let temp = self.dir.join(format!(
            "{TEMP_PREFIX}{}-{}",
            std::process::id(),
            self.next_temp.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(err) = fs::write(&temp, data) {
            let _ = fs::remove_file(&temp);
            return Err(err);
        }
        let path = self.entry_path(key, bucket, png);
        let mut total = self.total.lock().unwrap_or_else(PoisonError::into_inner);
        // Scanned before the rename, so that the new file is not counted twice.
        let mut size = total.unwrap_or_else(|| scan(&self.dir).1);
        let replaced = fs::metadata(&path).map_or(0, |m| m.len());
        let renamed = fs::rename(&temp, &path);
        if renamed.is_err() {
            let _ = fs::remove_file(&temp);
        } else {
            size = size.saturating_sub(replaced) + data.len() as u64;
        }
        let cap = self.cap.load(Ordering::Relaxed);
        *total = Some(if size > cap {
            evict(&self.dir, &path, cap - cap / 10)
        } else {
            size
        });
        renamed
    }
}

/// Decodes `data` under limits and shrinks it to fit `bucket` pixels on its longest edge.
fn make(data: &[u8], bucket: u32) -> Result<Made, String> {
    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_ALLOC);
    decoder.set_limits(limits).map_err(|e| e.to_string())?;
    let (width, height) = decoder.dimensions();
    if width.max(height) <= bucket {
        return Ok(Made::Original);
    }
    // The thumbnail has no EXIF block, so the rotation the original asks for is applied.
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    image.apply_orientation(orientation);
    let image = image.resize(bucket, bucket, FilterType::Lanczos3);
    let (width, height) = (image.width(), image.height());
    // GIFs and PNGs with an alpha channel but no transparent pixel are common, and JPEG
    // is a fraction of the size.
    let png = image.color().has_alpha() && image.to_rgba8().pixels().any(|p| p.0[3] != u8::MAX);
    let mut out = Vec::new();
    if png {
        PngEncoder::new(&mut out).write_image(
            image.to_rgba8().as_raw(),
            width,
            height,
            ExtendedColorType::Rgba8,
        )
    } else {
        JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY).write_image(
            image.to_rgb8().as_raw(),
            width,
            height,
            ExtendedColorType::Rgb8,
        )
    }
    .map_err(|e| e.to_string())?;
    Ok(Made::Thumbnail { data: out, png })
}

/// A cache entry on disk.
struct Entry {
    path: PathBuf,
    len: u64,
    modified: SystemTime,
}

/// The cache files in `dir` and their total size; removes temp files left by a crash.
fn scan(dir: &Path) -> (Vec<Entry>, u64) {
    let mut entries = Vec::new();
    let Ok(read) = fs::read_dir(dir) else {
        return (entries, 0);
    };
    for item in read.flatten() {
        let Ok(meta) = item.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let name = item.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(TEMP_PREFIX) {
            let age = SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default();
            if age > STALE_TEMP {
                let _ = fs::remove_file(item.path());
            }
        } else if name.ends_with(".jpg") || name.ends_with(".png") {
            entries.push(Entry {
                path: item.path(),
                len: meta.len(),
                modified,
            });
        }
    }
    let total = entries.iter().map(|e| e.len).sum();
    (entries, total)
}

/// Deletes the least recently used cache files in `dir` (never `keep`) until their total
/// size is at most `target`; returns the total size left.
fn evict(dir: &Path, keep: &Path, target: u64) -> u64 {
    let (mut entries, mut total) = scan(dir);
    entries.sort_by(|a, b| {
        a.modified
            .cmp(&b.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    for entry in entries.iter().filter(|e| e.path != keep) {
        if total <= target {
            break;
        }
        match fs::remove_file(&entry.path) {
            Ok(()) => total -= entry.len,
            Err(err) if err.kind() == io::ErrorKind::NotFound => total -= entry.len,
            Err(err) => {
                tracing::debug!(%err, path = %entry.path.display(), "cover thumbnail not evicted");
            }
        }
    }
    total
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests;
