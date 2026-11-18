//! Plans metadata writes as splices against the original file.
use super::*;
use delta::{Delta, Region, read_at};
use std::io;

mod mp4;

pub(super) const MAX_REGION: usize = 96 * 1024 * 1024;

pub(super) fn unsupported() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "layout cannot be edited safely")
}

pub(in crate::tags) fn classify(file: &mut File) -> io::Result<()> {
    if file.metadata()?.len() < 8 {
        return Ok(());
    }
    let magic = read_at(file, 0, 8)?;
    if &magic[4..] == b"ftyp" {
        mp4::classify(file)?;
    } else if &magic[..4] == b"OggS" {
        ogg::classify(file)?;
    }
    Ok(())
}

pub(in crate::tags) fn validate_samples(source: &mut File, candidate: &mut File) -> io::Result<()> {
    mp4::validate_samples(source, candidate)
}

impl Editable {
    pub(in crate::tags) fn plan(&self, source: &mut File) -> io::Result<Delta> {
        let encode_error = |message: &dyn std::fmt::Display| io::Error::other(message.to_string());
        let (regions, pages) = match (&self.primary, self.format) {
            (Native::Id3v2(_), _) => {
                let bytes = self.serialized_id3().map_err(|e| encode_error(&e))?;
                (raw::regions(source, &bytes, None)?, None)
            }
            (Native::Vorbis(tag), Format::Flac) => {
                let mut bytes = Vec::new();
                tag.dump_to(&mut bytes, WriteOptions::new())
                    .map_err(|e| encode_error(&e))?;
                (flac(source, bytes)?, None)
            }
            (Native::Vorbis(tag), Format::Ogg) => {
                let signature: &[u8] = match self.file_type {
                    FileType::Vorbis => b"\x03vorbis",
                    FileType::Opus => b"OpusTags",
                    _ => &[],
                };
                let mut packet = signature.to_vec();
                tag.dump_to(&mut packet, WriteOptions::new())
                    .map_err(|e| encode_error(&e))?;
                if self.file_type == FileType::Vorbis {
                    packet.push(1);
                }
                source.rewind()?;
                let mut reader = BufReader::new(&mut *source);
                let mut bytes = Vec::new();
                let (shift, shared_payload) =
                    ogg::rewrite_header(&mut reader, &mut bytes, &packet)?;
                let end = reader.stream_position()?;
                // Read the shared setup or audio payload from its source range.
                bytes.truncate(bytes.len() - shared_payload);
                (
                    vec![Region {
                        range: 0..end - shared_payload as u64,
                        bytes,
                    }],
                    Some((end, shift)),
                )
            }
            (Native::Ilst(tag), _) => {
                let mut bytes = Vec::new();
                tag.dump_to(&mut bytes, WriteOptions::new())
                    .map_err(|e| encode_error(&e))?;
                (mp4::regions(source, bytes)?, None)
            }
            _ => return Err(unsupported()),
        };
        Delta::from_regions(source, regions, pages)
    }
}

fn flac(file: &mut File, bytes: Vec<u8>) -> io::Result<Vec<Region>> {
    let size = file.metadata()?.len();
    let start = raw::leading_id3(file, size)?;
    if read_at(file, start, 4)? != b"fLaC" {
        return Err(unsupported());
    }
    let length = u32::try_from(bytes.len())
        .ok()
        .filter(|n| *n < 1 << 24)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::FileTooLarge,
                "FLAC comment exceeds block size",
            )
        })?;
    let block = |last| {
        let mut block = vec![4 | last];
        block.extend(&length.to_be_bytes()[1..]);
        block.extend(&bytes);
        block
    };
    let mut pos = start + 4;
    let mut comment = None;
    loop {
        let header = read_at(file, pos, 4)?;
        let len = u64::from(u32::from_be_bytes([0, header[1], header[2], header[3]]));
        let end = (pos + 4 + len <= size)
            .then_some(pos + 4 + len)
            .ok_or_else(unsupported)?;
        if header[0] & 127 == 4 && comment.replace((pos..end, header[0] & 128)).is_some() {
            return Err(unsupported());
        }
        if header[0] & 128 != 0 {
            return Ok(if let Some((range, last)) = comment {
                vec![Region {
                    range,
                    bytes: block(last),
                }]
            } else {
                vec![
                    Region {
                        range: pos..pos + 1,
                        bytes: vec![header[0] & 127],
                    },
                    Region {
                        range: end..end,
                        bytes: block(128),
                    },
                ]
            });
        }
        pos = end;
    }
}
