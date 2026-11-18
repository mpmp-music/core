use super::{Layout, invalid, too_large};
use lofty::file::FileType;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

fn at<const N: usize>(file: &mut File, offset: u64) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub(super) fn flac(file: &mut File, layout: &mut Layout) -> io::Result<()> {
    let mut pos = super::super::raw::leading_id3(file, layout.size)?;
    if &at::<4>(file, pos)? != b"fLaC" {
        return Err(invalid());
    }
    pos += 4;
    loop {
        let [kind, a, b, c] = at(file, pos)?;
        pos += 4 + u64::from(u32::from_be_bytes([0, a, b, c]));
        if pos > layout.size {
            return Err(invalid());
        }
        if kind & 0x80 != 0 {
            break;
        }
    }
    layout.media(pos..layout.size)
}

pub(super) fn mpeg(file: &mut File, layout: &mut Layout) -> io::Result<()> {
    let head = super::super::raw::leading_id3(file, layout.size)?;
    let mut end = layout.size;
    if end >= 128 && &at::<3>(file, end - 128)? == b"TAG" {
        end -= 128;
        if end >= 227 && &at::<4>(file, end - 227)? == b"TAG+" {
            end -= 227;
        }
    }
    if end >= 15 {
        let lyrics = at::<15>(file, end - 15)?;
        if &lyrics[6..] == b"LYRICS200" {
            let size = std::str::from_utf8(&lyrics[..6])
                .ok()
                .and_then(|size| size.parse::<u64>().ok())
                .ok_or_else(invalid)?;
            end = end.checked_sub(size + 15).ok_or_else(invalid)?;
        }
    }
    if end >= 32 {
        let footer = at::<32>(file, end - 32)?;
        if &footer[..8] == b"APETAGEX" {
            let len = u64::from(u32::from_le_bytes(footer[12..16].try_into().unwrap()));
            let flags = u32::from_le_bytes(footer[20..24].try_into().unwrap());
            if len < 32 {
                return Err(invalid());
            }
            end = end.checked_sub(len).ok_or_else(invalid)?;
            if flags & (1 << 31) != 0 {
                end = end.checked_sub(32).ok_or_else(invalid)?;
                if &at::<8>(file, end)? != b"APETAGEX" {
                    return Err(invalid());
                }
            }
        }
    }
    layout.media(head..end)
}

pub(super) fn iff(file: &mut File, layout: &mut Layout) -> io::Result<()> {
    let header = at::<12>(file, 0)?;
    let little = match (&header[..4], &header[8..]) {
        (b"RIFF", b"WAVE") => true,
        (b"FORM", b"AIFF" | b"AIFC") => false,
        _ => return Err(invalid()),
    };
    let size = |bytes: &[u8]| {
        let bytes = bytes.try_into().unwrap();
        u64::from(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };
    let end = size(&header[4..8]) + 8;
    if !(12..=layout.size).contains(&end) {
        return Err(invalid());
    }
    let mut pos = 12;
    while pos < end {
        let chunk = at::<8>(file, pos)?;
        let len = size(&chunk[4..]);
        let next = pos + 8 + len + len % 2;
        if next > end {
            return Err(invalid());
        }
        if matches!(&chunk[..4], b"data" | b"SSND")
            || (&chunk[..4] == b"LIST" && len >= 4 && &at::<4>(file, pos + 8)? == b"wavl")
        {
            layout.media(pos..next)?;
        }
        pos = next;
    }
    layout.media(end..layout.size)
}

pub(super) fn mp4(file: &mut File, layout: &mut Layout) -> io::Result<()> {
    let mut pos = 0;
    while pos < layout.size {
        let header = at::<8>(file, pos)?;
        let size = u32::from_be_bytes(header[..4].try_into().unwrap());
        let (len, minimum) = match size {
            0 => (layout.size - pos, 8),
            1 => (u64::from_be_bytes(at(file, pos + 8)?), 16),
            n => (u64::from(n), 8),
        };
        if len < minimum || len > layout.size - pos {
            return Err(invalid());
        }
        if matches!(&header[4..], b"moof" | b"mfra") {
            // These formats can have offset tables outside moov.
            return Err(invalid());
        }
        if matches!(&header[4..], b"mdat" | b"idat") {
            layout.media(pos..pos + len)?;
        }
        pos += len;
    }
    Ok(())
}

pub(super) fn ogg(file: &mut File, layout: &mut Layout, format: FileType) -> io::Result<()> {
    let mut headers = match format {
        FileType::Vorbis => 3,
        FileType::Opus | FileType::Speex => 2,
        _ => return Err(invalid()),
    };
    let mut pos = 0;
    let mut serial = None;
    let mut metadata = 0;
    while pos < layout.size {
        file.seek(SeekFrom::Start(pos))?;
        let page = super::super::ogg::Header::read(file)?.ok_or_else(invalid)?;
        let stream: [u8; 4] = page.bytes[14..18].try_into().unwrap();
        if serial.get_or_insert(stream) != &stream {
            return Err(invalid());
        }
        let body = pos + 27 + page.laces().len() as u64;
        let mut start = body;
        let mut end = body;
        for &lace in page.laces() {
            end += u64::from(lace);
            if headers != 0 {
                start = end;
                if lace < 255 {
                    headers -= 1;
                }
            }
        }
        if end > layout.size {
            return Err(invalid());
        }
        metadata += start - pos;
        if metadata > super::super::MAX_TAG_UNDO_BYTES as u64 {
            return Err(too_large());
        }
        layout.media(start..end)?;
        pos = end;
    }
    if headers != 0 {
        return Err(invalid());
    }
    Ok(())
}
