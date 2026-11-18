use super::*;

pub(super) fn synchsafe(b: &[u8]) -> Checked<u32> {
    if b.len() != 4 || b.iter().any(|v| v & 128 != 0) {
        return Err("cover_edit_unsupported");
    }
    Ok(b.iter().fold(0, |n, b| (n << 7) | u32::from(*b)))
}

pub(super) fn sync_bytes(n: usize) -> Checked<[u8; 4]> {
    if n > 0xfffffff {
        return Err("cover_edit_unsupported");
    }
    Ok([
        ((n >> 21) & 127) as u8,
        ((n >> 14) & 127) as u8,
        ((n >> 7) & 127) as u8,
        (n & 127) as u8,
    ])
}

fn apic(b: &[u8]) -> Checked<(bool, Option<CoverImageInfo>)> {
    let encoding = *b.first().ok_or("cover_edit_unsupported")?;
    let end = b
        .get(1..)
        .and_then(|b| b.iter().position(|v| *v == 0))
        .ok_or("cover_edit_unsupported")?
        + 1;
    let mime = std::str::from_utf8(&b[1..end]).map_err(|_| "cover_edit_unsupported")?;
    let front = *b.get(end + 1).ok_or("cover_edit_unsupported")? == 3;
    let desc = b.get(end + 2..).ok_or("cover_edit_unsupported")?;
    let skip = match encoding {
        0 | 3 => desc.iter().position(|v| *v == 0).map(|p| p + 1),
        1 | 2 => desc
            .as_chunks::<2>()
            .0
            .iter()
            .position(|v| *v == [0, 0])
            .map(|p| p * 2 + 2),
        _ => None,
    }
    .ok_or("cover_edit_unsupported")?;
    Ok((front, image_info(mime, &desc[skip..]).ok()))
}

pub(super) fn id3(file: &mut std::fs::File, change: &CoverChange) -> Checked<Plan> {
    let h = read_at(file, 0, 10)?;
    let tagged = h.starts_with(b"ID3");
    // Unsynchronisation, extended headers, footer and v2.2 require a different writer.
    let version = if tagged {
        if !matches!(h[3], 3 | 4) || h[4] != 0 || h[5] != 0 {
            return Err("cover_edit_unsupported");
        }
        h[3]
    } else {
        4
    };
    let n = if tagged {
        synchsafe(&h[6..10])? as usize
    } else {
        0
    };
    if n > MAX_REGION {
        return Err("undo_too_large");
    }
    let audio = read_at(file, if tagged { (n + 10) as u64 } else { 0 }, 2)?;
    if audio[0] != 255 || audio[1] & 0xe0 != 0xe0 || audio[1] & 6 == 0 {
        return Err("cover_edit_unsupported");
    }
    let raw = read_at(file, if tagged { 10 } else { 0 }, n)?;
    let mut pos = 0;
    let mut frames = Vec::new();
    let mut before = None;
    let mut fallback = false;
    let mut fronts = 0;
    let mut absolute_offsets = false;
    while pos < raw.len() && raw[pos] != 0 {
        let h = raw.get(pos..pos + 10).ok_or("cover_edit_unsupported")?;
        if !h[..4]
            .iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            return Err("cover_edit_unsupported");
        }
        absolute_offsets |= matches!(&h[..4], b"CHAP" | b"ASPI");
        let size = if version == 4 {
            synchsafe(&h[4..8])?
        } else {
            be32(&h[4..8])?
        } as usize;
        let end = pos
            .checked_add(10 + size)
            .filter(|p| *p <= raw.len())
            .ok_or("cover_edit_unsupported")?;
        let front = if &h[..4] == b"APIC" {
            if h[8..10] != [0, 0] {
                return Err("cover_edit_unsupported");
            }
            let (front, info) = apic(&raw[pos + 10..end])?;
            if front {
                fronts += 1;
                if before.is_none() {
                    before = info;
                }
            } else {
                fallback = true;
            }
            front
        } else {
            false
        };
        if !front {
            frames.extend(&raw[pos..end]);
        }
        pos = end;
    }
    if raw[pos..].iter().any(|b| *b != 0) {
        return Err("cover_edit_unsupported");
    }
    if fronts == 0 && matches!(change, CoverChange::Remove) {
        return Ok(Plan {
            regions: vec![],
            before,
            fallback,
            scope: "front_cover",
        });
    }
    if let CoverChange::Set { mime, data } = change {
        let mut p = vec![0];
        p.extend(mime.as_bytes());
        p.extend([0, 3, 0]);
        p.extend(data);
        frames.extend(b"APIC");
        frames.extend(if version == 4 {
            sync_bytes(p.len())?
        } else {
            (p.len() as u32).to_be_bytes()
        });
        frames.extend([0, 0]);
        frames.extend(p);
    }
    if absolute_offsets && frames.len() > raw.len() {
        return Err("cover_edit_unsupported");
    }
    // Retain padding when possible, so offsets outside the ID3 block are stable.
    frames.resize(frames.len().max(raw.len()), 0);
    let mut bytes = b"ID3".to_vec();
    bytes.extend([version, 0, 0]);
    bytes.extend(sync_bytes(frames.len())?);
    bytes.extend(frames);
    let replaced = if tagged { n + 10 } else { 0 };
    let old = read_at(file, 0, replaced)?;
    Ok(Plan {
        regions: if bytes == old {
            vec![]
        } else {
            vec![Region {
                offset: 0,
                replaced: replaced as u64,
                bytes,
            }]
        },
        before,
        fallback,
        scope: "front_cover",
    })
}
