use super::*;

fn ogg_crc(b: &[u8]) -> u32 {
    let mut crc = 0u32;
    for (i, v) in b.iter().enumerate() {
        crc ^= u32::from(if (22..26).contains(&i) { 0 } else { *v }) << 24;
        for _ in 0..8 {
            crc = if crc & 0x80000000 != 0 {
                (crc << 1) ^ 0x04c11db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn ogg_page(
    serial: u32,
    seq: u32,
    flags: u8,
    granule: u64,
    laces: &[u8],
    payload: &[u8],
) -> Vec<u8> {
    let mut b = b"OggS\0".to_vec();
    b.push(flags);
    b.extend(granule.to_le_bytes());
    b.extend(serial.to_le_bytes());
    b.extend(seq.to_le_bytes());
    b.extend([0; 4]);
    b.push(laces.len() as u8);
    b.extend(laces);
    b.extend(payload);
    let crc = ogg_crc(&b);
    b[22..26].copy_from_slice(&crc.to_le_bytes());
    b
}

fn comments(
    packet: &[u8],
    change: &CoverChange,
    before: &mut Option<CoverImageInfo>,
    fallback: &mut bool,
) -> Checked<Vec<u8>> {
    let prefix = if packet.starts_with(b"\x03vorbis") {
        7
    } else if packet.starts_with(b"OpusTags") {
        8
    } else {
        return Err("cover_edit_unsupported");
    };
    let mut p = prefix;
    let vendor = le32(packet.get(p..).ok_or("cover_edit_unsupported")?)? as usize;
    p += 4;
    let vendor = packet
        .get(p..p.checked_add(vendor).ok_or("cover_edit_unsupported")?)
        .ok_or("cover_edit_unsupported")?;
    p += vendor.len();
    let count = le32(packet.get(p..).ok_or("cover_edit_unsupported")?)? as usize;
    p += 4;
    if count > packet.len() / 4 {
        return Err("cover_edit_unsupported");
    }
    let mut values = Vec::new();
    for _ in 0..count {
        let n = le32(packet.get(p..).ok_or("cover_edit_unsupported")?)? as usize;
        p += 4;
        let value = packet
            .get(p..p.checked_add(n).ok_or("cover_edit_unsupported")?)
            .ok_or("cover_edit_unsupported")?;
        p += n;
        let mut front = false;
        if let Some(eq) = value.iter().position(|b| *b == b'=')
            && value[..eq].eq_ignore_ascii_case(b"METADATA_BLOCK_PICTURE")
        {
            let text =
                std::str::from_utf8(&value[eq + 1..]).map_err(|_| "cover_edit_unsupported")?;
            let picture = decode_base64(text).map_err(|_| "cover_edit_unsupported")?;
            let (f, info) = parse_picture(&picture)?;
            front = f;
            if front && before.is_none() {
                *before = info;
            } else if !front {
                *fallback = true;
            }
        }
        if !front {
            values.push(value.to_vec());
        }
    }
    if let Some(b) = picture(change) {
        let mut v = b"METADATA_BLOCK_PICTURE=".to_vec();
        v.extend(encode_base64(&b).as_bytes());
        values.push(v);
    }
    let mut out = packet[..prefix].to_vec();
    out.extend((vendor.len() as u32).to_le_bytes());
    out.extend(vendor);
    out.extend((values.len() as u32).to_le_bytes());
    for v in values {
        out.extend((v.len() as u32).to_le_bytes());
        out.extend(v);
    }
    // Preserve Vorbis framing and Opus padding verbatim.
    out.extend(&packet[p..]);
    Ok(out)
}

pub(super) fn ogg(file: &mut std::fs::File, change: &CoverChange) -> Checked<Plan> {
    let len = file.metadata().map_err(|_| "open_failed")?.len();
    let mut pos = 0;
    let mut serial = None;
    let mut seq = 0;
    let mut packets = Vec::new();
    let mut packet = Vec::new();
    let mut start = 0;
    let mut end = 0;
    let mut first_seq = 0;
    let mut last_seq = 0;
    let mut tail = Vec::new();
    let mut before = None;
    let mut fallback = false;
    let mut edited_pages = None;
    let mut comment_index = None;
    let mut new_page_count = 0;
    while pos < len {
        let h = read_at(file, pos, 27)?;
        if &h[..5] != b"OggS\0" {
            return Err("cover_edit_unsupported");
        }
        let s = le32(&h[14..])?;
        let q = le32(&h[18..])?;
        if h[5] & !7 != 0 {
            return Err("cover_edit_unsupported");
        }
        if serial.is_none() {
            if q != 0 {
                return Err("cover_edit_unsupported");
            }
            serial = Some(s);
            seq = q;
            if h[5] & 2 == 0 {
                return Err("cover_edit_unsupported");
            }
        }
        if serial != Some(s) || q != seq {
            return Err("cover_edit_unsupported");
        }
        seq = seq.checked_add(1).ok_or("cover_edit_unsupported")?;
        let laces = read_at(file, pos + 27, h[26] as usize)?;
        let n: usize = laces.iter().map(|v| usize::from(*v)).sum();
        let payload = read_at(file, pos + 27 + laces.len() as u64, n)?;
        let mut whole = h.clone();
        whole.extend(&laces);
        whole.extend(&payload);
        if ogg_crc(&whole) != le32(&h[22..])? {
            return Err("cover_edit_unsupported");
        }
        if edited_pages.is_none() {
            if h[5] & 4 != 0 || (h[5] & 1 != 0) != !packet.is_empty() {
                return Err("cover_edit_unsupported");
            }
            // Rewrite complete header pages only. Audio sharing the final header page is
            // refused because its granule cannot be assigned safely after repagination.
            if packet.is_empty() && packets.is_empty() {
                start = pos;
                first_seq = q;
            }
            let mut cursor = 0;
            for &lace in &laces {
                packet.extend(&payload[cursor..cursor + lace as usize]);
                cursor += lace as usize;
                if packet.len() > MAX_REGION {
                    return Err("undo_too_large");
                }
                if lace < 255 {
                    if packets.is_empty()
                        && start == 0
                        && !packet.starts_with(b"\x01vorbis")
                        && !packet.starts_with(b"OpusHead")
                    {
                        return Err("cover_edit_unsupported");
                    }
                    if packet.starts_with(b"\x03vorbis") || packet.starts_with(b"OpusTags") {
                        comment_index = Some(packets.len());
                    }
                    if packets.len() >= 3
                        || packets
                            .iter()
                            .map(Vec::len)
                            .sum::<usize>()
                            .saturating_add(packet.len())
                            > MAX_REGION
                    {
                        return Err("cover_edit_unsupported");
                    }
                    packets.push(std::mem::take(&mut packet));
                }
            }
            if let Some(index) = comment_index
                && packet.is_empty()
            {
                if packets[index + 1..]
                    .iter()
                    .any(|p| !p.starts_with(b"\x05vorbis"))
                {
                    return Err("cover_edit_unsupported");
                }
                let changed = comments(&packets[index], change, &mut before, &mut fallback)?;
                if changed == packets[index] {
                    return Ok(Plan {
                        regions: vec![],
                        before,
                        fallback,
                        scope: "front_cover",
                    });
                }
                packets[index] = changed;
                let mut pages = Vec::new();
                let mut page_seq = first_seq;
                for (i, p) in packets.iter().enumerate() {
                    let mut ls = vec![255; p.len() / 255];
                    ls.push((p.len() % 255) as u8);
                    let mut cursor = 0;
                    let chunks = ls.len().div_ceil(255);
                    for (j, l) in ls.chunks(255).enumerate() {
                        let n: usize = l.iter().map(|b| *b as usize).sum();
                        let flags = if page_seq == 0 { 2 } else { 0 } | if j > 0 { 1 } else { 0 };
                        let granule = if j + 1 != chunks {
                            u64::MAX
                        } else if i + 1 == packets.len() {
                            u64::from_le_bytes(h[6..14].try_into().unwrap())
                        } else {
                            0
                        };
                        pages.extend(ogg_page(
                            s,
                            page_seq,
                            flags,
                            granule,
                            l,
                            &p[cursor..cursor + n],
                        ));
                        cursor += n;
                        page_seq += 1;
                    }
                }
                end = pos + whole.len() as u64;
                last_seq = q;
                new_page_count = page_seq - first_seq;
                edited_pages = Some(pages);
            }
        } else {
            let old_count = last_seq - first_seq + 1;
            let delta = i64::from(new_page_count) - i64::from(old_count);
            if delta != 0 {
                let next =
                    u32::try_from(i64::from(q) + delta).map_err(|_| "cover_edit_unsupported")?;
                let mut header = h;
                header[18..22].copy_from_slice(&next.to_le_bytes());
                whole[..27].copy_from_slice(&header);
                let crc = ogg_crc(&whole);
                header[22..26].copy_from_slice(&crc.to_le_bytes());
                tail.push(Region {
                    offset: pos,
                    replaced: 27,
                    bytes: header,
                });
                if tail.len() * 54 > MAX_REGION {
                    return Err("undo_too_large");
                }
            }
        }
        pos += whole.len() as u64;
    }
    let bytes = edited_pages.ok_or("cover_edit_unsupported")?;
    let mut regions = vec![Region {
        offset: start,
        replaced: end - start,
        bytes,
    }];
    regions.extend(tail);
    Ok(Plan {
        regions,
        before,
        fallback,
        scope: "front_cover",
    })
}
