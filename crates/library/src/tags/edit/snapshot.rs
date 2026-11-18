//! Byte-level undo stores the complement of the media ranges and the lengths of the
//! holes. At restore time those holes are filled from the current file in stream order.
use std::fs::File;
use std::io::{self, Seek, Write};
use std::ops::Range;

use lofty::file::FileType;
use sha2::{Digest, Sha256};

mod layout;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ByteUndo {
    pub before: [u8; 32],
    pub after: [u8; 32],
    pub image: Vec<u8>,
}

pub(in crate::tags) struct Layout {
    size: u64,
    media: Vec<Range<u64>>,
}

impl Layout {
    pub fn read(file: &mut File, format: FileType) -> io::Result<Self> {
        let mut layout = Self {
            size: file.metadata()?.len(),
            media: Vec::new(),
        };
        match format {
            FileType::Flac => layout::flac(file, &mut layout)?,
            FileType::Mpeg | FileType::Aac => layout::mpeg(file, &mut layout)?,
            FileType::Wav | FileType::Aiff => layout::iff(file, &mut layout)?,
            FileType::Vorbis | FileType::Opus | FileType::Speex => {
                layout::ogg(file, &mut layout, format)?;
            }
            FileType::Mp4 => layout::mp4(file, &mut layout)?,
            _ => return Err(invalid()),
        }
        Ok(layout)
    }

    fn media(&mut self, range: Range<u64>) -> io::Result<()> {
        let previous = self.media.last().map_or(0, |r| r.end);
        if range.start < previous || range.end < range.start || range.end > self.size {
            return Err(invalid());
        }
        if range.is_empty() {
            return Ok(());
        }
        if previous == range.start
            && let Some(last) = self.media.last_mut()
        {
            last.end = range.end;
        } else {
            // Even zero metadata between holes would need two length fields.
            if self.media.len() >= super::MAX_TAG_UNDO_BYTES / (2 * 8) {
                return Err(too_large());
            }
            self.media.push(range);
        }
        Ok(())
    }

    /// Translate a version-1 full-metadata inverse into a byte program.
    pub fn translate(&self, mut image: &[u8]) -> io::Result<super::delta::Delta> {
        let mut delta = super::delta::Delta::default();
        let mut spans = self.media.iter();
        let mut current = 0..0;
        loop {
            let len = usize::try_from(length(&mut image)?).map_err(|_| invalid())?;
            let (literal, rest) = image.split_at_checked(len).ok_or_else(invalid)?;
            delta.literal(literal);
            image = rest;
            if image.is_empty() {
                return if current.is_empty() && spans.next().is_none() {
                    Ok(delta)
                } else {
                    Err(invalid())
                };
            }
            let mut len = length(&mut image)?;
            while len != 0 {
                if current.is_empty() {
                    current = spans.next().ok_or_else(invalid)?.clone();
                }
                let count = len.min(current.end - current.start);
                delta.copy(current.start, count);
                current.start += count;
                len -= count;
            }
        }
    }
}

fn length(image: &mut &[u8]) -> io::Result<u64> {
    let (bytes, rest) = image.split_first_chunk().ok_or_else(invalid)?;
    *image = rest;
    Ok(u64::from_le_bytes(*bytes))
}

pub(crate) fn digest(file: &mut File) -> io::Result<[u8; 32]> {
    file.rewind()?;
    let mut digest = HashWriter(Sha256::new());
    io::copy(file, &mut digest)?;
    Ok(digest.0.finalize().into())
}

pub(in crate::tags) fn projected_digest(
    delta: &super::delta::Delta,
    file: &mut File,
) -> io::Result<[u8; 32]> {
    let mut hash = HashWriter(Sha256::new());
    delta.render(file, &mut hash)?;
    Ok(hash.0.finalize().into())
}

pub(in crate::tags) fn render_digest(
    delta: &super::delta::Delta,
    file: &mut File,
    output: &mut File,
) -> io::Result<[u8; 32]> {
    struct Output<'a> {
        file: &'a mut File,
        hash: Sha256,
    }
    impl Write for Output<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.file.write_all(bytes)?;
            self.hash.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }
    let mut output = Output {
        file: output,
        hash: Sha256::new(),
    };
    delta.render(file, &mut output)?;
    Ok(output.hash.finalize().into())
}

struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "cannot reconstruct exact tag bytes",
    )
}

fn too_large() -> io::Error {
    io::Error::new(io::ErrorKind::FileTooLarge, "tag undo exceeds budget")
}
pub(crate) fn hex(bytes: [u8; 32]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(64);
    for b in bytes {
        write!(out, "{b:02x}").unwrap();
    }
    out
}
