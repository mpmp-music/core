//! Lossless primary ID3 blocks for encoding conversion undo. Audio is moved in bounded
//! chunks inside the job's private copy; it is never part of the undo data.
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use super::MAX_TAG_UNDO_BYTES;

struct Block {
    start: u64,
    len: u64,
    container: Option<bool>, // RIFF: little endian; FORM: big endian.
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "cannot preserve the primary ID3 block",
    )
}

fn block(file: &mut File) -> io::Result<Block> {
    file.rewind()?;
    let mut header = [0; 12];
    file.read_exact(&mut header)?;
    let size = file.metadata()?.len();
    let container = match &header[..4] {
        b"RIFF" if &header[8..] == b"WAVE" => Some(true),
        b"FORM" if matches!(&header[8..], b"AIFF" | b"AIFC") => Some(false),
        _ => None,
    };
    if let Some(little) = container {
        let len = header[4..8].try_into().unwrap();
        let end = 8 + u64::from(if little {
            u32::from_le_bytes(len)
        } else {
            u32::from_be_bytes(len)
        });
        if !(12..=size).contains(&end) {
            return Err(invalid());
        }
        let mut pos = 12;
        let mut found = None;
        while pos < end {
            if pos + 8 > end {
                return Err(invalid());
            }
            file.seek(SeekFrom::Start(pos))?;
            let mut chunk = [0; 8];
            file.read_exact(&mut chunk)?;
            let bytes = chunk[4..].try_into().unwrap();
            let len = u64::from(if little {
                u32::from_le_bytes(bytes)
            } else {
                u32::from_be_bytes(bytes)
            });
            let next = pos + 8 + len + len % 2;
            if next > end {
                return Err(invalid());
            }
            if chunk[..4].eq_ignore_ascii_case(b"ID3 ") {
                if found.is_some() {
                    return Err(invalid());
                }
                found = Some(Block {
                    start: pos,
                    len: next - pos,
                    container,
                });
            }
            pos = next;
        }
        return Ok(found.unwrap_or(Block {
            start: end,
            len: 0,
            container,
        }));
    }
    Ok(Block {
        start: 0,
        len: leading_id3(file, size)?,
        container: None,
    })
}

/// MPEG/AAC readers merge consecutive ID3 tags. Include any ID3v2.4 footer.
pub(super) fn leading_id3(file: &mut File, size: u64) -> io::Result<u64> {
    file.rewind()?;
    let mut len = 0;
    while size - len >= 10 {
        let mut header = [0; 10];
        file.read_exact(&mut header)?;
        if &header[..3] != b"ID3" {
            break;
        }
        if !matches!(header[3], 2..=4) || header[6..10].iter().any(|b| *b >= 128) {
            return Err(invalid());
        }
        let body = header[6..10]
            .iter()
            .fold(0u64, |n, b| (n << 7) | u64::from(*b));
        len += 10
            + body
            + if header[3] == 4 && header[5] & 0x10 != 0 {
                10
            } else {
                0
            };
        if len > size {
            return Err(invalid());
        }
        file.seek(SeekFrom::Start(len))?;
    }
    Ok(len)
}

// Container snapshots are versioned separately from legacy payload-only journal data.
const MAGIC: &[u8; 8] = b"PMPID3\0\x01";

const PREFIX: usize = 28; // magic, chunk offset, original 12-byte container header.

pub(in crate::tags) fn read(file: &mut File) -> io::Result<Vec<u8>> {
    let block = block(file)?;
    if block.len > MAX_TAG_UNDO_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "raw ID3 undo exceeds budget",
        ));
    }
    let mut bytes = Vec::new();
    if block.container.is_some() {
        bytes.extend(MAGIC);
        bytes.extend(if block.len == 0 { 0 } else { block.start }.to_le_bytes());
        file.rewind()?;
        let mut header = [0; 12];
        file.read_exact(&mut header)?;
        bytes.extend(header);
    }
    file.seek(SeekFrom::Start(block.start))?;
    let prefix = bytes.len();
    bytes.resize(prefix + block.len as usize, 0);
    file.read_exact(&mut bytes[prefix..])?;
    Ok(bytes)
}

pub(in crate::tags) fn payload(bytes: &[u8]) -> io::Result<&[u8]> {
    if !bytes.starts_with(MAGIC) {
        return Ok(bytes);
    }
    let chunk = bytes.get(PREFIX..).ok_or_else(invalid)?;
    if chunk.is_empty() {
        return Ok(chunk);
    }
    let little = &bytes[16..20] == b"RIFF";
    if chunk.len() < 8 || !chunk[..4].eq_ignore_ascii_case(b"ID3 ") {
        return Err(invalid());
    }
    let size = chunk[4..8].try_into().unwrap();
    let len = if little {
        u32::from_le_bytes(size)
    } else {
        u32::from_be_bytes(size)
    } as usize;
    if chunk.len() != 8 + len + len % 2 {
        return Err(invalid());
    }
    Ok(&chunk[8..8 + len])
}

/// Plan replacement of either the leading ID3 span or just the IFF chunk and size.
pub(in crate::tags) fn regions(
    file: &mut File,
    payload_bytes: &[u8],
    legacy: Option<&[u8]>,
) -> io::Result<Vec<super::delta::Region>> {
    use super::delta::{Region, read_at};
    let mut block = block(file)?;
    let Some(little) = block.container else {
        return Ok(vec![Region {
            range: 0..block.len,
            bytes: payload_bytes.to_vec(),
        }]);
    };
    let before = read_at(file, 0, 12)?;
    let size_bytes = before[4..8].try_into().unwrap();
    let old = if little {
        u32::from_le_bytes(size_bytes)
    } else {
        u32::from_be_bytes(size_bytes)
    };
    let mut bytes = Vec::new();
    if let Some(legacy) = legacy.filter(|b| b.starts_with(MAGIC)) {
        payload(legacy)?;
        bytes.extend(&legacy[PREFIX..]);
        let old_position = u64::from_le_bytes(legacy[8..16].try_into().unwrap());
        if !bytes.is_empty() && old_position != block.start {
            if block.len != 0 || !(12..=block.start).contains(&old_position) {
                return Err(invalid());
            }
            let mut position = 12;
            while position < old_position {
                let len = read_at(file, position + 4, 4)?.try_into().unwrap();
                let len = u64::from(if little {
                    u32::from_le_bytes(len)
                } else {
                    u32::from_be_bytes(len)
                });
                position += 8 + len + len % 2;
            }
            if position != old_position {
                return Err(invalid());
            }
            block.start = old_position;
        }
    } else if !payload_bytes.is_empty() {
        let kind = if block.len != 0 {
            read_at(file, block.start, 4)?
        } else {
            b"ID3 ".to_vec()
        };
        bytes.extend(kind);
        let len = u32::try_from(payload_bytes.len()).map_err(|_| invalid())?;
        bytes.extend(if little {
            len.to_le_bytes()
        } else {
            len.to_be_bytes()
        });
        bytes.extend(payload_bytes);
        if len % 2 != 0 {
            bytes.push(0);
        }
    }
    let size = u32::try_from(i64::from(old) + bytes.len() as i64 - block.len as i64)
        .map_err(|_| invalid())?;
    Ok(vec![
        Region {
            range: 4..8,
            bytes: if little {
                size.to_le_bytes()
            } else {
                size.to_be_bytes()
            }
            .to_vec(),
        },
        Region {
            range: block.start..block.start + block.len,
            bytes,
        },
    ])
}
