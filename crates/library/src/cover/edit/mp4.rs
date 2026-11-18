use super::*;

pub(super) fn atom(kind: &[u8], body: &[u8]) -> Checked<Vec<u8>> {
    let n = u32::try_from(body.len().checked_add(8).ok_or("cover_edit_unsupported")?)
        .map_err(|_| "cover_edit_unsupported")?;
    let mut b = n.to_be_bytes().to_vec();
    b.extend(kind);
    b.extend(body);
    Ok(b)
}

pub(super) fn atoms(b: &[u8]) -> Checked<Vec<(&[u8], &[u8])>> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < b.len() {
        let n = be32(b.get(pos..).ok_or("cover_edit_unsupported")?)? as usize;
        if n < 8 {
            return Err("cover_edit_unsupported");
        }
        let end = pos
            .checked_add(n)
            .filter(|e| *e <= b.len())
            .ok_or("cover_edit_unsupported")?;
        out.push((&b[pos + 4..pos + 8], &b[pos + 8..end]));
        pos = end;
    }
    Ok(out)
}

pub(super) fn mp4(file: &mut std::fs::File, change: &CoverChange) -> Checked<Plan> {
    let len = file.metadata().map_err(|_| "open_failed")?.len();
    let mut pos = 0;
    let mut moov = None;
    let mut mdat = Vec::new();
    while pos < len {
        let h = read_at(file, pos, 8)?;
        let mut n = u64::from(be32(&h)?);
        let mut header = 8;
        if n == 1 {
            n = u64::from_be_bytes(read_at(file, pos + 8, 8)?.try_into().unwrap());
            header = 16;
        }
        if n == 0 {
            n = len - pos;
        }
        if n < header || n > len - pos {
            return Err("cover_edit_unsupported");
        }
        match &h[4..8] {
            b"moov" if moov.is_none() && header == 8 => moov = Some((pos, n)),
            b"mdat" => mdat.push((pos + header, pos + n)),
            b"moof" | b"mfra" | b"sidx" | b"moov" => return Err("cover_edit_unsupported"),
            b"ftyp" | b"free" | b"skip" | b"wide" => {}
            _ => return Err("cover_edit_unsupported"),
        }
        pos += n;
    }
    let (offset, size) = moov.ok_or("cover_edit_unsupported")?;
    let raw = read_at(
        file,
        offset + 8,
        usize::try_from(size - 8).map_err(|_| "undo_too_large")?,
    )?;
    let mut before = None;
    let mut fallback = false;
    let mut found = false;
    fn edit(
        b: &[u8],
        path: &[&[u8]],
        change: &CoverChange,
        before: &mut Option<CoverImageInfo>,
        _fallback: &mut bool,
        found: &mut bool,
    ) -> Checked<Vec<u8>> {
        let mut out = Vec::new();
        let mut child_found = false;
        for (kind, body) in atoms(b)? {
            if let Some(next) = path.first()
                && kind == *next
            {
                if child_found {
                    return Err("cover_edit_unsupported");
                }
                child_found = true;
                let (prefix, body) = if kind == b"meta" {
                    if body.get(..4) != Some(&[0, 0, 0, 0]) {
                        return Err("cover_edit_unsupported");
                    }
                    (&body[..4], &body[4..])
                } else {
                    (&[][..], body)
                };
                let changed = edit(body, &path[1..], change, before, _fallback, found)?;
                let mut payload = prefix.to_vec();
                payload.extend(changed);
                out.extend(atom(kind, &payload)?);
            } else if path.is_empty() && kind == b"covr" {
                if *found {
                    return Err("cover_edit_unsupported");
                }
                *found = true;
                for (k, p) in atoms(body)? {
                    if k != b"data" || p.len() < 8 {
                        return Err("cover_edit_unsupported");
                    }
                    let mime = match be32(p)? {
                        13 => "image/jpeg",
                        14 => "image/png",
                        _ => "application/octet-stream",
                    };
                    if before.is_none() {
                        *before = image_info(mime, &p[8..]).ok();
                    }
                }
                if let Some(new) = covr(change)? {
                    out.extend(new);
                }
            } else {
                out.extend(atom(kind, body)?);
            }
        }
        if !child_found && !path.is_empty() {
            let next = path[0];
            let nested = edit(&[], &path[1..], change, before, _fallback, found)?;
            let mut payload = if next == b"meta" { vec![0; 4] } else { vec![] };
            if next == b"meta" {
                let mut handler = vec![0; 8];
                handler.extend(b"mdirappl");
                handler.extend([0; 9]);
                payload.extend(atom(b"hdlr", &handler)?);
            }
            payload.extend(nested);
            out.extend(atom(next, &payload)?);
        } else if path.is_empty()
            && !*found
            && let Some(new) = covr(change)?
        {
            out.extend(new);
        }
        Ok(out)
    }
    fn covr(change: &CoverChange) -> Checked<Option<Vec<u8>>> {
        let CoverChange::Set { mime, data } = change else {
            return Ok(None);
        };
        let mut body = (if mime == "image/jpeg" { 13u32 } else { 14 })
            .to_be_bytes()
            .to_vec();
        body.extend([0; 4]);
        body.extend(data);
        Ok(Some(atom(b"covr", &atom(b"data", &body)?)?))
    }
    let edited = edit(
        &raw,
        &[b"udta", b"meta", b"ilst"],
        change,
        &mut before,
        &mut fallback,
        &mut found,
    )?;
    if !found && matches!(change, CoverChange::Remove) {
        return Ok(Plan {
            regions: vec![],
            before,
            fallback,
            scope: "all_covr",
        });
    }
    let delta = edited.len() as i64 - raw.len() as i64;
    fn offsets(
        b: &[u8],
        depth: usize,
        delta: i64,
        moov_end: u64,
        mdat: &[(u64, u64)],
    ) -> Checked<Vec<u8>> {
        if depth > 16 {
            return Err("cover_edit_unsupported");
        }
        let mut out = Vec::new();
        for (kind, body) in atoms(b)? {
            let changed = if kind == b"meta" {
                if body.get(..4) != Some(&[0, 0, 0, 0]) {
                    return Err("cover_edit_unsupported");
                }
                let mut payload = body[..4].to_vec();
                payload.extend(offsets(&body[4..], depth + 1, delta, moov_end, mdat)?);
                payload
            } else if matches!(
                kind,
                b"trak" | b"mdia" | b"minf" | b"stbl" | b"udta" | b"dinf"
            ) {
                offsets(body, depth + 1, delta, moov_end, mdat)?
            } else if kind == b"stco" || kind == b"co64" {
                if body.get(..4) != Some(&[0, 0, 0, 0]) {
                    return Err("cover_edit_unsupported");
                }
                let count = be32(body.get(4..).ok_or("cover_edit_unsupported")?)? as usize;
                let width = if kind == b"co64" { 8 } else { 4 };
                if count.checked_mul(width).and_then(|n| n.checked_add(8)) != Some(body.len()) {
                    return Err("cover_edit_unsupported");
                }
                let mut body = body.to_vec();
                for c in body[8..].chunks_exact_mut(width) {
                    let old = if width == 8 {
                        u64::from_be_bytes(c[..].try_into().unwrap())
                    } else {
                        u64::from(be32(c)?)
                    };
                    // Refuse external data references and offsets into mutable metadata.
                    if !mdat.iter().any(|(a, b)| old >= *a && old < *b) {
                        return Err("cover_edit_unsupported");
                    }
                    let n = if old >= moov_end {
                        old.checked_add_signed(delta)
                            .ok_or("cover_edit_unsupported")?
                    } else {
                        old
                    };
                    if width == 8 {
                        c.copy_from_slice(&n.to_be_bytes());
                    } else {
                        c.copy_from_slice(
                            &u32::try_from(n)
                                .map_err(|_| "cover_edit_unsupported")?
                                .to_be_bytes(),
                        );
                    }
                }
                body
            } else if matches!(kind, b"saio" | b"iloc" | b"mvex" | b"cmov") {
                return Err("cover_edit_unsupported");
            } else {
                body.to_vec()
            };
            out.extend(atom(kind, &changed)?);
        }
        Ok(out)
    }
    let bytes = atom(b"moov", &offsets(&edited, 0, delta, offset + size, &mdat)?)?;
    let original = atom(b"moov", &raw)?;
    Ok(Plan {
        regions: if bytes == original {
            vec![]
        } else {
            vec![Region {
                offset,
                replaced: size,
                bytes,
            }]
        },
        before,
        fallback,
        scope: "all_covr",
    })
}
