//! Stores and validates cached whole-track waveforms supplied by core.
//!
//! Core schedules computation with engine and supplies plain [`Waveform`] values. Library
//! also uses engine for audio probing after cover edits; caching itself only handles data.
//! See docs/design/media-resources.md for the resource lifecycle.
//!
//! One file per track in a `waveforms` folder next to the database, named
//! `<track id>-<size>-<mtime_ms>.wf` after the file's size and modification time as the
//! library last scanned them: when the file changes, its waveform no longer matches and is
//! not found. Storing a track's waveform removes its other versions. Each file starts with
//! a header (magic, format version, channels, buckets); anything that does not read back
//! exactly is no waveform.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use crate::types::TrackId;

/// A track's waveform: for each channel (one or two), per bucket the smallest and largest
/// sample (−128…127 for −1.0…1.0) and the RMS (0…255 for 0…1.0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waveform {
    /// Buckets of each channel; every vector of every channel has this length.
    pub buckets: u32,
    pub channels: Vec<WaveformChannel>,
}

/// One channel of a [`Waveform`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaveformChannel {
    pub min: Vec<i8>,
    pub max: Vec<i8>,
    pub rms: Vec<u8>,
}

/// Start of every cache file.
const MAGIC: &[u8; 4] = b"PMWF";

/// The layout after the header; a file of another version is no waveform (it is replaced
/// when the track's waveform is stored again).
const FORMAT_VERSION: u8 = 1;

/// Magic, version, channels, two reserved bytes, buckets (u32 LE).
const HEADER: usize = 12;

/// Most channels and buckets a stored waveform may have.
const MAX_CHANNELS: usize = 8;

const MAX_BUCKETS: u32 = 1 << 16;

const EXTENSION: &str = "wf";

/// Prefix of in-flight files; they are not cache entries.
const TEMP_PREFIX: &str = ".tmp-";

/// Age after which a temp file is a leftover of a crash.
const STALE_TEMP: Duration = Duration::from_secs(3600);

/// The waveform folder.
pub(crate) struct WaveformCache {
    dir: PathBuf,
    /// Makes temp file names unique within this process.
    next_temp: AtomicU64,
}

/// What identifies one version of a track's file, as last scanned: its size and
/// modification time ([`crate::Library::file_version`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileVersion {
    pub size: u64,
    pub mtime_ms: i64,
}

impl WaveformCache {
    pub(crate) fn new(dir: PathBuf) -> Self {
        WaveformCache {
            dir,
            next_temp: AtomicU64::new(0),
        }
    }

    fn path(&self, id: TrackId, stamp: FileVersion) -> PathBuf {
        self.dir.join(file_name(id, stamp))
    }

    /// The cached waveform of this version of the track, if there is one that reads back.
    pub(crate) fn get(&self, id: TrackId, stamp: FileVersion) -> Option<Waveform> {
        let path = self.path(id, stamp);
        let data = match read_limited(&path) {
            Ok(data) => data,
            Err(err) => {
                if err.kind() != io::ErrorKind::NotFound {
                    tracing::debug!(%err, path = %path.display(), "waveform not read");
                }
                return None;
            }
        };
        let waveform = decode(&data);
        if waveform.is_none() {
            tracing::debug!(path = %path.display(), "waveform cache file is not readable");
        }
        waveform
    }

    /// Stores the waveform of this version of the track (through a temp file and a rename,
    /// so a reader never sees a partial file) and removes the track's other versions.
    pub(crate) fn put(
        &self,
        id: TrackId,
        stamp: FileVersion,
        waveform: &Waveform,
    ) -> io::Result<()> {
        let data = encode(waveform)?;
        fs::create_dir_all(&self.dir)?;
        let temp = self.dir.join(format!(
            "{TEMP_PREFIX}{}-{}",
            std::process::id(),
            self.next_temp.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(err) = fs::write(&temp, &data) {
            let _ = fs::remove_file(&temp);
            return Err(err);
        }
        let name = file_name(id, stamp);
        if let Err(err) = fs::rename(&temp, self.dir.join(&name)) {
            let _ = fs::remove_file(&temp);
            return Err(err);
        }
        self.remove_others(id, &name);
        Ok(())
    }

    /// Removes the track's cache files other than `keep`, and temp files a crash left.
    /// Failures are only logged: the new waveform is stored either way.
    fn remove_others(&self, id: TrackId, keep: &str) {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::debug!(%err, dir = %self.dir.display(), "waveform folder not listed");
                return;
            }
        };
        let prefix = format!("{}-", id.0);
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let stale = if name.starts_with(TEMP_PREFIX) {
                entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|at| now.duration_since(at).unwrap_or_default() > STALE_TEMP)
            } else {
                name != keep
                    && name.starts_with(&prefix)
                    && Path::new(name).extension().is_some_and(|e| e == EXTENSION)
            };
            if stale && let Err(err) = fs::remove_file(entry.path()) {
                tracing::debug!(%err, name, "old waveform not removed");
            }
        }
    }
}

fn file_name(id: TrackId, stamp: FileVersion) -> String {
    format!("{}-{}-{}.{EXTENSION}", id.0, stamp.size, stamp.mtime_ms)
}

/// Reads a cache file, refusing one larger than any waveform can be.
fn read_limited(path: &Path) -> io::Result<Vec<u8>> {
    let max = HEADER as u64 + 3 * MAX_CHANNELS as u64 * u64::from(MAX_BUCKETS);
    if fs::metadata(path)?.len() > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "too large"));
    }
    fs::read(path)
}

fn encode(waveform: &Waveform) -> io::Result<Vec<u8>> {
    let buckets = waveform.buckets;
    let n = buckets as usize;
    let valid = (1..=MAX_CHANNELS).contains(&waveform.channels.len())
        && (1..=MAX_BUCKETS).contains(&buckets)
        && waveform
            .channels
            .iter()
            .all(|c| c.min.len() == n && c.max.len() == n && c.rms.len() == n);
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a waveform needs 1 to 8 channels of 1 to 65536 buckets, all of the same length",
        ));
    }
    let mut data = Vec::with_capacity(HEADER + 3 * n * waveform.channels.len());
    data.extend_from_slice(MAGIC);
    data.push(FORMAT_VERSION);
    data.push(waveform.channels.len() as u8);
    data.extend_from_slice(&[0, 0]);
    data.extend_from_slice(&buckets.to_le_bytes());
    for channel in &waveform.channels {
        data.extend(channel.min.iter().map(|&v| v as u8));
        data.extend(channel.max.iter().map(|&v| v as u8));
        data.extend_from_slice(&channel.rms);
    }
    Ok(data)
}

fn decode(data: &[u8]) -> Option<Waveform> {
    let header = data.get(..HEADER)?;
    if &header[..4] != MAGIC || header[4] != FORMAT_VERSION {
        return None;
    }
    let channels = usize::from(header[5]);
    let buckets = u32::from_le_bytes(header[8..12].try_into().ok()?);
    if !(1..=MAX_CHANNELS).contains(&channels) || !(1..=MAX_BUCKETS).contains(&buckets) {
        return None;
    }
    let n = buckets as usize;
    let body = &data[HEADER..];
    if body.len() != 3 * n * channels {
        return None;
    }
    let channels = body
        .chunks_exact(3 * n)
        .map(|c| WaveformChannel {
            min: c[..n].iter().map(|&v| v as i8).collect(),
            max: c[n..2 * n].iter().map(|&v| v as i8).collect(),
            rms: c[2 * n..].to_vec(),
        })
        .collect();
    Some(Waveform { buckets, channels })
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::*;

    fn waveform(channels: usize, buckets: u32, seed: u8) -> Waveform {
        let n = buckets as usize;
        Waveform {
            buckets,
            channels: (0..channels)
                .map(|c| WaveformChannel {
                    min: (0..n).map(|i| -((i as u8 ^ seed) as i8 & 0x7f)).collect(),
                    max: (0..n)
                        .map(|i| (i as u8 ^ seed ^ c as u8) as i8 & 0x7f)
                        .collect(),
                    rms: (0..n).map(|i| (i as u8).wrapping_mul(3) ^ seed).collect(),
                })
                .collect(),
        }
    }

    const STAMP: FileVersion = FileVersion {
        size: 1234,
        mtime_ms: 1_700_000_000_000,
    };

    #[test]
    fn a_stored_waveform_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WaveformCache::new(dir.path().join("waveforms"));
        assert_eq!(cache.get(TrackId(7), STAMP), None, "nothing stored yet");
        for (channels, buckets) in [(2, 2048), (1, 2048), (2, 3)] {
            let w = waveform(channels, buckets, 5);
            cache.put(TrackId(7), STAMP, &w).unwrap();
            assert_eq!(cache.get(TrackId(7), STAMP), Some(w));
        }
        let path = cache.path(TrackId(7), STAMP);
        assert_eq!(
            path.file_name().unwrap(),
            "7-1234-1700000000000.wf",
            "named by track, size and modification time"
        );
        // Another version of the file is another entry.
        let changed = FileVersion {
            mtime_ms: STAMP.mtime_ms + 1,
            ..STAMP
        };
        assert_eq!(cache.get(TrackId(7), changed), None);
        assert_eq!(cache.get(TrackId(8), STAMP), None);
    }

    #[test]
    fn storing_a_new_version_removes_the_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WaveformCache::new(dir.path().to_path_buf());
        let old = FileVersion {
            size: 1,
            mtime_ms: -1,
        };
        cache.put(TrackId(4), old, &waveform(2, 16, 1)).unwrap();
        cache.put(TrackId(42), old, &waveform(2, 16, 2)).unwrap();
        // A temp file a crash left long ago, and one being written now.
        let stale = dir.path().join(".tmp-1-1");
        fs::write(&stale, b"x").unwrap();
        let file = fs::File::options().write(true).open(&stale).unwrap();
        file.set_modified(SystemTime::now() - 2 * STALE_TEMP)
            .unwrap();
        let fresh = dir.path().join(".tmp-1-2");
        fs::write(&fresh, b"x").unwrap();

        cache.put(TrackId(4), STAMP, &waveform(2, 16, 3)).unwrap();
        assert_eq!(cache.get(TrackId(4), old), None);
        assert_eq!(cache.get(TrackId(4), STAMP), Some(waveform(2, 16, 3)));
        assert_eq!(
            cache.get(TrackId(42), old),
            Some(waveform(2, 16, 2)),
            "another track whose id starts the same is kept"
        );
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn a_damaged_or_foreign_file_is_no_waveform() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WaveformCache::new(dir.path().to_path_buf());
        cache.put(TrackId(1), STAMP, &waveform(2, 64, 9)).unwrap();
        let path = cache.path(TrackId(1), STAMP);
        let good = fs::read(&path).unwrap();
        let mut bad_version = good.clone();
        bad_version[4] = FORMAT_VERSION + 1;
        let mut bad_channels = good.clone();
        bad_channels[5] = 0;
        let mut bad_buckets = good.clone();
        bad_buckets[8] = 65;
        for data in [
            Vec::new(),
            good[..HEADER - 1].to_vec(),
            good[..good.len() - 1].to_vec(),
            [good.as_slice(), &[0]].concat(),
            b"RIFF".iter().chain(&good[4..]).copied().collect(),
            bad_version,
            bad_channels,
            bad_buckets,
        ] {
            fs::write(&path, &data).unwrap();
            assert_eq!(cache.get(TrackId(1), STAMP), None, "{} bytes", data.len());
        }
        // Storing again replaces it.
        cache.put(TrackId(1), STAMP, &waveform(1, 8, 0)).unwrap();
        assert_eq!(cache.get(TrackId(1), STAMP), Some(waveform(1, 8, 0)));
    }

    #[test]
    fn the_library_keys_waveforms_by_the_scanned_size_and_time() {
        use crate::tests::{fixture, id_of, meta, record, upsert};
        let f = fixture();
        let scanned = |size, mtime_ms| {
            let mut r = record(f.root, "a.flac", meta("Song", "A", "B"));
            (r.size, r.mtime_ms) = (size, mtime_ms);
            upsert(&f.lib, vec![r]);
        };
        scanned(100, 5);
        let id = id_of(&f.lib, "Song");
        assert_eq!(f.lib.waveform(id).unwrap(), None);
        let w = waveform(2, 2048, 1);
        let first = f.lib.file_version(id).unwrap().unwrap();
        assert_eq!(
            first,
            FileVersion {
                size: 100,
                mtime_ms: 5
            }
        );
        assert!(f.lib.store_waveform(id, first, &w).unwrap());
        assert_eq!(f.lib.waveform(id).unwrap(), Some(w.clone()));
        let dir = f.lib.inner().waveforms.dir.clone();
        assert_eq!(
            dir.file_name().unwrap(),
            "waveforms",
            "next to the database"
        );
        assert!(dir.join(format!("{}-100-5.wf", id.0)).is_file());

        // The file changed (a rescan saw a new time, then a new size): no waveform until
        // one is stored for it, which replaces the old one.
        scanned(100, 6);
        assert_eq!(f.lib.waveform(id).unwrap(), None);
        scanned(101, 5);
        assert_eq!(f.lib.waveform(id).unwrap(), None);
        // One computed from the file before (a rescan came while it was computed) is not
        // stored for the new one.
        assert!(!f.lib.store_waveform(id, first, &w).unwrap());
        assert_eq!(f.lib.waveform(id).unwrap(), None);
        let new = waveform(1, 2048, 2);
        let now = f.lib.file_version(id).unwrap().unwrap();
        assert!(f.lib.store_waveform(id, now, &new).unwrap());
        assert_eq!(f.lib.waveform(id).unwrap(), Some(new));
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "the old version is gone"
        );
        scanned(100, 5);
        assert_eq!(f.lib.waveform(id).unwrap(), None);

        let unknown = TrackId(i64::MAX);
        assert_eq!(f.lib.waveform(unknown).unwrap(), None);
        assert_eq!(f.lib.file_version(unknown).unwrap(), None);
        let err = f.lib.store_waveform(unknown, first, &w).unwrap_err();
        assert_eq!(err.code(), "track_not_found");
    }

    #[test]
    fn a_malformed_waveform_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WaveformCache::new(dir.path().to_path_buf());
        let mut uneven = waveform(2, 16, 0);
        uneven.channels[1].rms.pop();
        for w in [
            uneven,
            waveform(0, 16, 0),
            waveform(9, 16, 0),
            waveform(1, 0, 0),
        ] {
            let err = cache.put(TrackId(1), STAMP, &w).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
