use super::*;

pub(super) fn picture(change: &CoverChange) -> Option<Vec<u8>> {
    let CoverChange::Set { mime, data } = change else {
        return None;
    };
    let info = change.image()?;
    let mut b = 3u32.to_be_bytes().to_vec();
    b.extend((mime.len() as u32).to_be_bytes());
    b.extend(mime.as_bytes());
    b.extend(0u32.to_be_bytes());
    for n in [info.width, info.height, 0, 0, data.len() as u32] {
        b.extend(n.to_be_bytes());
    }
    b.extend(data);
    Some(b)
}

pub(super) fn parse_picture(b: &[u8]) -> Checked<(bool, Option<CoverImageInfo>)> {
    let role = be32(b)?;
    let m = be32(b.get(4..).ok_or("cover_edit_unsupported")?)? as usize;
    let mime = std::str::from_utf8(b.get(8..8 + m).ok_or("cover_edit_unsupported")?)
        .map_err(|_| "cover_edit_unsupported")?;
    let p = 8 + m;
    let desc = be32(b.get(p..).ok_or("cover_edit_unsupported")?)? as usize;
    let p = p
        .checked_add(4 + desc + 16)
        .ok_or("cover_edit_unsupported")?;
    let n = be32(b.get(p..).ok_or("cover_edit_unsupported")?)? as usize;
    let bytes = b
        .get(p + 4..)
        .filter(|b| b.len() == n)
        .ok_or("cover_edit_unsupported")?;
    Ok((role == 3, image_info(mime, bytes).ok()))
}

pub(super) fn flac(file: &mut std::fs::File, change: &CoverChange) -> Checked<Plan> {
    let mut pos = 4u64;
    let mut blocks = Vec::new();
    let mut before = None;
    let mut fallback = false;
    let mut fronts = 0;
    loop {
        let h = read_at(file, pos, 4)?;
        let n = (u32::from_be_bytes([0, h[1], h[2], h[3]])) as usize;
        if pos + n as u64 > MAX_REGION as u64 {
            return Err("undo_too_large");
        }
        let data = read_at(file, pos + 4, n)?;
        pos += 4 + n as u64;
        let role = h[0] & 127;
        if role == 127 {
            return Err("cover_edit_unsupported");
        }
        if role == 6 {
            let (front, info) = parse_picture(&data)?;
            if front {
                fronts += 1;
                if before.is_none() {
                    before = info;
                }
            } else {
                fallback = true;
                blocks.push((role, data));
            }
        } else {
            blocks.push((role, data));
        }
        if h[0] & 128 != 0 {
            break;
        }
    }
    if fronts == 0 && matches!(change, CoverChange::Remove) {
        return Ok(Plan {
            regions: vec![],
            before,
            fallback,
            scope: "front_cover",
        });
    }
    if let Some(p) = picture(change) {
        blocks.push((6, p));
    }
    let mut bytes = Vec::new();
    let last = blocks
        .len()
        .checked_sub(1)
        .ok_or("cover_edit_unsupported")?;
    for (i, (kind, data)) in blocks.iter().enumerate() {
        if data.len() > 0xffffff {
            return Err("cover_edit_unsupported");
        }
        bytes.push(*kind | if i == last { 128 } else { 0 });
        bytes.extend(&(data.len() as u32).to_be_bytes()[1..]);
        bytes.extend(data);
    }
    let old = read_at(file, 4, (pos - 4) as usize)?;
    Ok(Plan {
        regions: if bytes == old {
            vec![]
        } else {
            vec![Region {
                offset: 4,
                replaced: pos - 4,
                bytes,
            }]
        },
        before,
        fallback,
        scope: "front_cover",
    })
}
