//! Byte-level media input shared by `engine` and `library`.
//!
//! Platform shells resolve a [`MediaUri`] into a [`ResolvedSource`] (a path, a file
//! descriptor or a URL); everything after that is portable Rust working on [`ByteSource`].

#![warn(missing_docs)]

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A resolution or byte-source failure; local open errors retain their path and cause.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The resolver does not recognize the URI scheme.
    #[error("unsupported URI scheme: {0}")]
    UnsupportedScheme(String),
    /// The source kind is reserved but cannot be opened yet.
    #[error("not supported yet: {0}")]
    Unsupported(&'static str),
    /// A local file could not be opened or its metadata could not be read.
    #[error("cannot open {}: {source}", path.display())]
    Open {
        /// The path whose open or metadata query failed.
        path: PathBuf,
        /// The original I/O failure.
        source: io::Error,
    },
    /// The resolved local path names a directory.
    #[error("{} is a directory, not a file", .0.display())]
    IsDirectory(PathBuf),
    /// An underlying I/O operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Identifies a piece of media independently of how a platform opens it.
///
/// This is an internal identifier, not a web URL: `file://` URIs hold the absolute path
/// verbatim, without percent-encoding.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct MediaUri(String);

impl MediaUri {
    /// Stores a URI verbatim without validating or resolving it.
    pub fn new(uri: impl Into<String>) -> Self {
        Self(uri.into())
    }

    /// Builds a `file://` identifier from an absolute path without percent-encoding.
    /// Relative paths use the current directory; failure to obtain it is returned.
    /// Non-Unicode path components use the platform display representation.
    pub fn from_path(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = std::path::absolute(path)?;
        Ok(Self(format!("file://{}", path.display())))
    }

    /// Returns the original URI text without normalization.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the prefix before `://`, or `None` when that delimiter is absent.
    pub fn scheme(&self) -> Option<&str> {
        self.0.split_once("://").map(|(scheme, _)| scheme)
    }

    /// Returns the verbatim suffix of `file://` as a path, or `None` for other schemes.
    /// This performs no decoding, canonicalization, or existence check.
    pub fn to_file_path(&self) -> Option<PathBuf> {
        self.0.strip_prefix("file://").map(PathBuf::from)
    }

    /// Lower-cased file extension of the last path segment, used as a format hint.
    pub fn extension(&self) -> Option<String> {
        let segment = self.0.rsplit('/').next()?;
        let (_, ext) = segment.rsplit_once('.')?;
        Some(ext.to_ascii_lowercase())
    }
}

impl fmt::Debug for MediaUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MediaUri({})", self.0)
    }
}

impl fmt::Display for MediaUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a platform hands back for a [`MediaUri`]. Kept FFI-friendly on purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedSource {
    /// A local path; opening and directory checks happen in `open`.
    Path(PathBuf),
    /// A detached file descriptor (Android SAF/MediaStore). Currently unsupported by `open`.
    Fd(i32),
    /// Remote media. Reserved; not implemented in v1.
    Url(String),
}

/// Maps media identifiers to platform sources and opens their bytes.
/// Resolution errors must propagate rather than silently changing URI identity.
pub trait SourceResolver: Send + Sync {
    /// Resolves an identifier without opening it, or returns a scheme/platform error.
    fn resolve(&self, uri: &MediaUri) -> Result<ResolvedSource, Error>;
    /// Open bytes through the platform resolver; embedders may supply a controllable
    /// byte source without changing URI identity or decoder access-lease ownership.
    fn open(&self, uri: &MediaUri) -> Result<Arc<dyn ByteSource>, Error> {
        open(self.resolve(uri)?)
    }
}

/// Resolves `file://` URIs; the default on desktop.
#[derive(Debug, Default, Clone, Copy)]
pub struct FileResolver;

impl SourceResolver for FileResolver {
    /// Resolves an identifier without opening it, or returns a scheme/platform error.
    fn resolve(&self, uri: &MediaUri) -> Result<ResolvedSource, Error> {
        match uri.to_file_path() {
            Some(path) => Ok(ResolvedSource::Path(path)),
            None => Err(Error::UnsupportedScheme(
                uri.scheme().unwrap_or("<none>").to_owned(),
            )),
        }
    }
}

/// Random-access bytes. Implementations must be safe to share between threads.
pub trait ByteSource: Send + Sync {
    /// Reads at an absolute byte offset without changing a shared cursor.
    /// Returns the bytes read (possibly short), zero at EOF, or the underlying I/O error.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    /// Returns the known byte length, or `None` if it cannot be determined.
    fn len(&self) -> Option<u64>;
    /// Reports whether arbitrary positional access is supported.
    fn is_seekable(&self) -> bool;

    /// Returns true only when a known length is zero; unknown lengths return false.
    fn is_empty(&self) -> bool {
        self.len() == Some(0)
    }
}

/// Opens a resolved source as a [`ByteSource`].
///
/// Local directories return `IsDirectory`; open and metadata failures retain the path
/// in `Open`. Descriptor and URL variants return `Unsupported` without opening bytes.
pub fn open(source: ResolvedSource) -> Result<Arc<dyn ByteSource>, Error> {
    match source {
        ResolvedSource::Path(path) => {
            // Checked up front: opening a directory fails with a misleading
            // "Access is denied" on Windows.
            if path.is_dir() {
                return Err(Error::IsDirectory(path));
            }
            match FileSource::open(&path) {
                Ok(source) => Ok(Arc::new(source)),
                Err(source) => Err(Error::Open { path, source }),
            }
        }
        ResolvedSource::Fd(_) => Err(Error::Unsupported("file descriptor sources")),
        ResolvedSource::Url(_) => Err(Error::Unsupported("URL sources")),
    }
}

/// Resolves and opens `uri` through the resolver's `open` implementation.
///
/// Resolution and open failures propagate unchanged; custom resolvers may supply bytes
/// directly without going through a local path.
pub fn open_uri(
    resolver: &dyn SourceResolver,
    uri: &MediaUri,
) -> Result<Arc<dyn ByteSource>, Error> {
    resolver.open(uri)
}

/// A local file. Positional reads go through a mutex-guarded seek + read so that the
/// implementation stays portable; sources are only read from non-realtime threads.
pub struct FileSource {
    file: Mutex<File>,
    len: u64,
}

impl FileSource {
    /// Opens a local file and records its initial length; open and metadata errors propagate.
    /// The recorded length is not refreshed after external writes.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            file: Mutex::new(file),
            len,
        })
    }
}

impl ByteSource for FileSource {
    /// Reads at an absolute byte offset without changing a shared cursor.
    /// Returns the bytes read (possibly short), zero at EOF, or the underlying I/O error.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        file.seek(SeekFrom::Start(offset))?;
        file.read(buf)
    }

    /// Returns the known byte length, or `None` if it cannot be determined.
    fn len(&self) -> Option<u64> {
        Some(self.len)
    }

    /// Reports whether arbitrary positional access is supported.
    fn is_seekable(&self) -> bool {
        true
    }
}

/// Bytes held in memory; mainly for tests and embedded resources.
pub struct MemorySource(Vec<u8>);

impl MemorySource {
    /// Owns these bytes without copying them; subsequent positional reads cannot fail.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl ByteSource for MemorySource {
    /// Reads at an absolute byte offset without changing a shared cursor.
    /// Returns the bytes read (possibly short), zero at EOF, or the underlying I/O error.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let Ok(start) = usize::try_from(offset) else {
            return Ok(0);
        };
        let Some(rest) = self.0.get(start..) else {
            return Ok(0);
        };
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        Ok(n)
    }

    /// Returns the known byte length, or `None` if it cannot be determined.
    fn len(&self) -> Option<u64> {
        Some(self.0.len() as u64)
    }

    /// Reports whether arbitrary positional access is supported.
    fn is_seekable(&self) -> bool {
        true
    }
}

/// Adapts a [`ByteSource`] to `Read + Seek` with its own cursor.
pub struct ByteSourceReader {
    source: Arc<dyn ByteSource>,
    pos: u64,
}

impl ByteSourceReader {
    /// Starts an independent cursor at byte zero over a shared byte source.
    pub fn new(source: Arc<dyn ByteSource>) -> Self {
        Self { source, pos: 0 }
    }

    /// Returns the shared source without changing this reader's cursor.
    pub fn source(&self) -> &Arc<dyn ByteSource> {
        &self.source
    }
}

impl Read for ByteSourceReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.source.read_at(self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for ByteSourceReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
            SeekFrom::End(delta) => {
                let len = self.source.len().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::Unsupported, "source length unknown")
                })?;
                len.checked_add_signed(delta)
            }
        };
        let target = target.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek before start of source")
        })?;
        self.pos = target;
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_parts() {
        let uri = MediaUri::new("file:///music/Album/01 Track.FLAC");
        assert_eq!(uri.scheme(), Some("file"));
        assert_eq!(uri.extension().as_deref(), Some("flac"));
        assert_eq!(
            uri.to_file_path(),
            Some(PathBuf::from("/music/Album/01 Track.FLAC"))
        );
        assert_eq!(MediaUri::new("content://media/42").extension(), None);
    }

    #[test]
    fn file_resolver_rejects_other_schemes() {
        let err = FileResolver
            .resolve(&MediaUri::new("content://media/42"))
            .unwrap_err();
        assert!(matches!(err, Error::UnsupportedScheme(s) if s == "content"));
    }

    #[test]
    fn reader_reads_and_seeks() {
        let source: Arc<dyn ByteSource> = Arc::new(MemorySource::new((0u8..10).collect()));
        let mut reader = ByteSourceReader::new(source);
        let mut buf = [0u8; 4];
        assert_eq!(reader.read(&mut buf).unwrap(), 4);
        assert_eq!(buf, [0, 1, 2, 3]);
        assert_eq!(reader.seek(SeekFrom::End(-2)).unwrap(), 8);
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], &[8, 9]);
        assert!(reader.seek(SeekFrom::Current(-20)).is_err());
    }

    #[test]
    fn open_errors_name_the_path() {
        let missing = std::env::temp_dir().join("media-io-test-missing.flac");
        let err = open(ResolvedSource::Path(missing.clone())).err().unwrap();
        assert!(matches!(&err, Error::Open { path, .. } if *path == missing));
        assert!(err.to_string().contains("media-io-test-missing.flac"));

        let dir = std::env::temp_dir();
        let err = open(ResolvedSource::Path(dir.clone())).err().unwrap();
        assert!(matches!(err, Error::IsDirectory(path) if path == dir));
    }

    #[test]
    fn file_source_round_trip() {
        let path = std::env::temp_dir().join(format!("media-io-test-{}", std::process::id()));
        std::fs::write(&path, b"hello world").unwrap();
        let uri = MediaUri::from_path(&path).unwrap();
        let source = open_uri(&FileResolver, &uri).unwrap();
        let mut buf = [0u8; 5];
        assert_eq!(source.read_at(6, &mut buf).unwrap(), 5);
        assert_eq!(&buf, b"world");
        assert_eq!(source.len(), Some(11));
        std::fs::remove_file(path).unwrap();
    }
}
