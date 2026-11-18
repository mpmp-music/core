//! Lossless cover surgery. Only container framing and front-picture records may change.
//! Audio is streamed; image dimensions are read from headers without decoding pixels.
use serde::{Deserialize, Serialize};

use sha2::{Digest, Sha256};

use std::io::{self, Read, Seek, SeekFrom, Write};

pub const MAX_COVER_IMAGE_BYTES: usize = 1_048_576;

pub const MAX_COVER_IMAGE_PIXELS: u64 = 16_777_216;

pub const MAX_COVER_IMAGE_EDGE: u32 = 8192;

pub const MAX_COVER_EDIT_TRACKS: usize = 1000;

pub const MAX_COVER_UNDO_BYTES: usize = 67_108_864;

// Container metadata is subject to the undo budget, independently of audio length.
const MAX_REGION: usize = MAX_COVER_UNDO_BYTES;

type Checked<T> = Result<T, &'static str>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverImageInfo {
    pub mime: String,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverChange {
    Set { mime: String, data: Vec<u8> },
    Remove,
}

impl CoverChange {
    pub fn from_base64(mime: &str, data: &str) -> Checked<Self> {
        if data.len() > MAX_COVER_IMAGE_BYTES.div_ceil(3) * 4 {
            return Err("image_too_large");
        }
        let bytes = decode_base64(data)?;
        validate_image(mime, &bytes)?;
        Ok(Self::Set {
            mime: mime.into(),
            data: bytes,
        })
    }
    pub fn validate(&self) -> Checked<()> {
        if let Self::Set { mime, data } = self {
            validate_image(mime, data)?;
        }
        Ok(())
    }
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Set { .. } => "set",
            Self::Remove => "remove",
        }
    }
    pub fn image(&self) -> Option<CoverImageInfo> {
        match self {
            Self::Set { mime, data } => image_info(mime, data).ok(),
            Self::Remove => None,
        }
    }
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn file_digest(file: &mut std::fs::File) -> io::Result<String> {
    file.rewind()?;
    let mut hash = Sha256::new();
    let mut buf = [0; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn validate_image(mime: &str, bytes: &[u8]) -> Checked<CoverImageInfo> {
    if bytes.len() > MAX_COVER_IMAGE_BYTES {
        return Err("image_too_large");
    }
    if !matches!(mime, "image/jpeg" | "image/png") {
        return Err("invalid_image");
    }
    if mime == "image/png" {
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err("invalid_image");
        }
        let mut pos = 8usize;
        let mut data_seen = false;
        let mut ended = false;
        while pos < bytes.len() {
            let n = be32(bytes.get(pos..).ok_or("invalid_image")?).map_err(|_| "invalid_image")?
                as usize;
            let end = pos
                .checked_add(12)
                .and_then(|p| p.checked_add(n))
                .filter(|e| *e <= bytes.len())
                .ok_or("invalid_image")?;
            let kind = &bytes[pos + 4..pos + 8];
            if ended || kind == b"acTL" || (pos == 8 && (kind != b"IHDR" || n != 13)) {
                return Err("invalid_image");
            }
            let mut crc = 0xffff_ffffu32;
            for byte in &bytes[pos + 4..end - 4] {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb8_8320 } else { 0 };
                }
            }
            if !crc != be32(&bytes[end - 4..]).map_err(|_| "invalid_image")? {
                return Err("invalid_image");
            }
            match kind {
                b"IDAT" => data_seen = true,
                b"IEND" => {
                    if n != 0 || !data_seen {
                        return Err("invalid_image");
                    }
                    ended = true;
                }
                b"IHDR" if pos != 8 => return Err("invalid_image"),
                _ => {}
            }
            pos = end;
        }
        if !ended {
            return Err("invalid_image");
        }
    }
    let info = image_info(mime, bytes)?;
    if info.width > MAX_COVER_IMAGE_EDGE
        || info.height > MAX_COVER_IMAGE_EDGE
        || u64::from(info.width) * u64::from(info.height) > MAX_COVER_IMAGE_PIXELS
    {
        return Err("image_too_large");
    }
    Ok(info)
}

pub(crate) fn image_info(mime: &str, bytes: &[u8]) -> Checked<CoverImageInfo> {
    let format = match mime {
        "image/jpeg" if bytes.starts_with(&[255, 216, 255]) => image::ImageFormat::Jpeg,
        "image/png" if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => image::ImageFormat::Png,
        "image/gif" if bytes.starts_with(b"GIF8") => image::ImageFormat::Gif,
        "image/webp" if bytes.starts_with(b"RIFF") => image::ImageFormat::WebP,
        "image/bmp" if bytes.starts_with(b"BM") => image::ImageFormat::Bmp,
        _ => return Err("invalid_image"),
    };
    // PNG dimensions come directly from IHDR: even compressed colour profiles are
    // never expanded while validating an upload. Other formats read headers only.
    let (width, height) = if format == image::ImageFormat::Png {
        if bytes.get(12..16) != Some(b"IHDR")
            || bytes.get(8..12) != Some(&[0, 0, 0, 13])
            || bytes.len() < 33
        {
            return Err("invalid_image");
        }
        let depth = bytes[24];
        let colour = bytes[25];
        if !matches!(
            (colour, depth),
            (0, 1 | 2 | 4 | 8 | 16) | (2 | 4 | 6, 8 | 16) | (3, 1 | 2 | 4 | 8)
        ) || bytes[26] != 0
            || bytes[27] != 0
            || bytes[28] > 1
        {
            return Err("invalid_image");
        }
        (
            be32(&bytes[16..]).map_err(|_| "invalid_image")?,
            be32(&bytes[20..]).map_err(|_| "invalid_image")?,
        )
    } else {
        image::ImageReader::with_format(io::Cursor::new(bytes), format)
            .into_dimensions()
            .map_err(|_| "invalid_image")?
    };
    if width == 0 || height == 0 {
        return Err("invalid_image");
    }
    Ok(CoverImageInfo {
        mime: mime.into(),
        bytes: bytes.len() as u64,
        width,
        height,
        sha256: digest(bytes),
    })
}

/// One exact replacement; inverse offsets refer to the resulting file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Region {
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub replaced: u64,
}

#[derive(Debug)]
pub(crate) struct Plan {
    pub regions: Vec<Region>,
    pub before: Option<CoverImageInfo>,
    pub fallback: bool,
    pub scope: &'static str,
}

impl Plan {
    pub fn undo_bytes(&self, file: &mut std::fs::File) -> Checked<usize> {
        if self.regions.is_empty() {
            return Ok(0);
        }
        let mut size = 256usize;
        for r in &self.regions {
            size = size
                .saturating_add((r.replaced as usize).max(r.bytes.len()))
                .saturating_add(32);
        }
        let _ = file;
        if size > MAX_COVER_UNDO_BYTES {
            return Err("undo_too_large");
        }
        Ok(size)
    }
}

fn read_at(file: &mut std::fs::File, offset: u64, len: usize) -> Checked<Vec<u8>> {
    if len > MAX_REGION {
        return Err("undo_too_large");
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| "open_failed")?;
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes)
        .map_err(|_| "cover_edit_unsupported")?;
    Ok(bytes)
}

pub(crate) fn prepare(file: &mut std::fs::File, change: &CoverChange) -> Checked<Plan> {
    change.validate()?;
    let head = read_at(file, 0, 12)?;
    if head.starts_with(b"fLaC") {
        flac(file, change)
    } else if head.starts_with(b"ID3")
        || head[0] == 255 && head[1] & 0xe0 == 0xe0 && head[1] & 6 != 0
    {
        id3(file, change)
    } else if head.starts_with(b"OggS") {
        ogg(file, change)
    } else if &head[4..8] == b"ftyp" {
        mp4(file, change)
    } else {
        Err("cover_edit_unsupported")
    }
}

/// Apply and verify exact replacement bytes and every untouched byte, in bounded chunks.
pub(crate) fn stage(
    original: &mut std::fs::File,
    copy: &mut std::fs::File,
    regions: &[Region],
) -> Checked<Vec<Region>> {
    original.rewind().map_err(|_| "open_failed")?;
    let mut pos = 0;
    let mut delta = 0i64;
    let mut inverse = Vec::new();
    for r in regions {
        if r.offset < pos {
            return Err("cover_edit_unsupported");
        }
        io::copy(&mut original.take(r.offset - pos), copy).map_err(|_| "write_failed")?;
        let old = read_at(
            original,
            r.offset,
            usize::try_from(r.replaced).map_err(|_| "undo_too_large")?,
        )?;
        copy.write_all(&r.bytes).map_err(|_| "write_failed")?;
        inverse.push(Region {
            offset: r
                .offset
                .checked_add_signed(delta)
                .ok_or("cover_edit_unsupported")?,
            replaced: r.bytes.len() as u64,
            bytes: old,
        });
        delta += r.bytes.len() as i64 - r.replaced as i64;
        pos = r.offset + r.replaced;
    }
    io::copy(original, copy).map_err(|_| "write_failed")?;
    copy.flush().map_err(|_| "write_failed")?;
    original.rewind().map_err(|_| "open_failed")?;
    copy.rewind().map_err(|_| "write_failed")?;
    let compare = |a: &mut std::fs::File, b: &mut std::fs::File, mut n: u64| -> Checked<()> {
        let mut left = [0; 65536];
        let mut right = [0; 65536];
        while n > 0 {
            let len = n.min(65536) as usize;
            a.read_exact(&mut left[..len]).map_err(|_| "file_changed")?;
            b.read_exact(&mut right[..len])
                .map_err(|_| "write_failed")?;
            if left[..len] != right[..len] {
                return Err("write_failed");
            }
            n -= len as u64;
        }
        Ok(())
    };
    pos = 0;
    for r in regions {
        compare(original, copy, r.offset - pos)?;
        original
            .seek(SeekFrom::Current(r.replaced as i64))
            .map_err(|_| "open_failed")?;
        let mut check = vec![0; r.bytes.len()];
        copy.read_exact(&mut check).map_err(|_| "write_failed")?;
        if check != r.bytes {
            return Err("write_failed");
        }
        pos = r.offset + r.replaced;
    }
    let tail = original
        .metadata()
        .map_err(|_| "open_failed")?
        .len()
        .checked_sub(pos)
        .ok_or("file_changed")?;
    compare(original, copy, tail)?;
    if copy.stream_position().map_err(|_| "write_failed")?
        != copy.metadata().map_err(|_| "write_failed")?.len()
    {
        return Err("write_failed");
    }
    Ok(inverse)
}

/// Hash a conceptual patch without allocating or writing a complete restored file.
/// Applying a staged file's inverse must reproduce the initial whole-file digest,
/// even if an external writer changed and restored the source during the copy.
pub(crate) fn patched_digest(file: &mut std::fs::File, regions: &[Region]) -> Checked<String> {
    let len = file.metadata().map_err(|_| "open_failed")?.len();
    file.rewind().map_err(|_| "open_failed")?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut position = 0;
    for region in regions {
        let end = region
            .offset
            .checked_add(region.replaced)
            .filter(|end| *end <= len)
            .ok_or("file_changed")?;
        let mut count = region.offset.checked_sub(position).ok_or("file_changed")?;
        while count > 0 {
            let n = count.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..n])
                .map_err(|_| "file_changed")?;
            hash.update(&buffer[..n]);
            count -= n as u64;
        }
        file.seek(SeekFrom::Start(end)).map_err(|_| "open_failed")?;
        hash.update(&region.bytes);
        position = end;
    }
    let mut count = len - position;
    while count > 0 {
        let n = count.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..n])
            .map_err(|_| "file_changed")?;
        hash.update(&buffer[..n]);
        count -= n as u64;
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn be32(b: &[u8]) -> Checked<u32> {
    Ok(u32::from_be_bytes(
        b.get(..4)
            .ok_or("cover_edit_unsupported")?
            .try_into()
            .unwrap(),
    ))
}

fn le32(b: &[u8]) -> Checked<u32> {
    Ok(u32::from_le_bytes(
        b.get(..4)
            .ok_or("cover_edit_unsupported")?
            .try_into()
            .unwrap(),
    ))
}

#[cfg(test)]
pub(crate) mod tests;

mod base64;
pub(crate) use self::base64::decode_base64;
pub(crate) use self::base64::encode_base64;

mod flac;
use self::flac::flac;
use self::flac::parse_picture;
use self::flac::picture;

mod id3;
use self::id3::id3;
#[cfg(test)]
use self::id3::sync_bytes;
#[cfg(test)]
use self::id3::synchsafe;

mod mp4;
#[cfg(test)]
use self::mp4::atom;
#[cfg(test)]
use self::mp4::atoms;
use self::mp4::mp4;

mod ogg;
use self::ogg::ogg;
