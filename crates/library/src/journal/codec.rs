use super::*;

pub(super) const TAG_RATINGS: u8 = 1;

pub(super) const TAG_LOVED: u8 = 2;

const TAG_PLAYLIST: u8 = 3;

const TAG_ROOT: u8 = 4;

const TAG_TAGS: u8 = 5;

const TAG_TAGS_RAW: u8 = 6;

const TAG_TAGS_HASH: u8 = 7;

const TAG_TAGS_EXACT: u8 = 10;

const OP_NAME: u8 = 1;

const OP_QUERY: u8 = 2;

const OP_REMOVE: u8 = 3;

const OP_RESTORE: u8 = 4;

const OP_MOVE: u8 = 5;

const OP_TRASH: u8 = 6;

const OP_UNTRASH: u8 = 7;

impl Inverse {
    /// A tag byte, then the fields: ids as zigzag varints, integer lists as
    /// [`encode_ints`] columns and text as the remaining bytes (both last).
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Inverse::Covers(files) => {
                out.push(9);
                put_varint(&mut out, files.len() as u64);
                for f in files {
                    put_varint(&mut out, zigzag(f.track));
                    put_text(&mut out, &f.expected);
                    put_text(&mut out, &f.restored_digest);
                    put_varint(&mut out, f.regions.len() as u64);
                    for r in &f.regions {
                        put_varint(&mut out, r.offset);
                        put_varint(&mut out, r.replaced);
                        put_varint(&mut out, r.bytes.len() as u64);
                        out.extend(&r.bytes);
                    }
                }
            }
            Inverse::Ratings(values) => {
                out.push(TAG_RATINGS);
                let (ids, ratings): (Vec<i64>, Vec<i64>) = values
                    .iter()
                    .map(|&(id, r)| (id, r.map_or(0, i64::from)))
                    .unzip();
                out.extend(encode_ints(&[&ids, &ratings]));
            }
            Inverse::Loved(values) => {
                out.push(TAG_LOVED);
                let (ids, loved): (Vec<i64>, Vec<i64>) =
                    values.iter().map(|&(id, l)| (id, i64::from(l))).unzip();
                out.extend(encode_ints(&[&ids, &loved]));
            }
            Inverse::Playlist(id, op) => {
                out.push(TAG_PLAYLIST);
                put_varint(&mut out, zigzag(id.0));
                match op {
                    PlaylistOp::Name(name) => {
                        out.push(OP_NAME);
                        out.extend(name.as_bytes());
                    }
                    PlaylistOp::Query { json, limit } => {
                        out.push(OP_QUERY);
                        put_varint(&mut out, zigzag(limit.map_or(-1, i64::from)));
                        out.extend(json.as_bytes());
                    }
                    PlaylistOp::Remove(items) => {
                        out.push(OP_REMOVE);
                        out.extend(encode_ints(&[items]));
                    }
                    PlaylistOp::Restore(rows) => {
                        out.push(OP_RESTORE);
                        let (mut positions, mut items, mut tracks) = (vec![], vec![], vec![]);
                        for &(position, item, track) in rows {
                            positions.push(position);
                            items.push(item);
                            tracks.push(track);
                        }
                        out.extend(encode_ints(&[&positions, &items, &tracks]));
                    }
                    PlaylistOp::Move { item, from } => {
                        out.push(OP_MOVE);
                        out.extend(encode_ints(&[&[*item], &[*from]]));
                    }
                    PlaylistOp::Trash => out.push(OP_TRASH),
                    PlaylistOp::Untrash => out.push(OP_UNTRASH),
                }
            }
            Inverse::Root(id, op) => {
                out.push(TAG_ROOT);
                put_varint(&mut out, zigzag(id.0));
                out.push(match op {
                    RootOp::Remove => 1,
                    RootOp::Restore => 2,
                });
            }
            // Per file: track, size, time, then each key with its values; text as a
            // length and UTF-8.
            Inverse::Tags(files) if files.iter().any(|f| f.exact.is_some()) => {
                out.push(TAG_TAGS_EXACT);
                put_varint(&mut out, files.len() as u64);
                for file in files {
                    let exact = file
                        .exact
                        .as_ref()
                        .expect("one tag inverse format per action");
                    put_varint(&mut out, zigzag(file.track));
                    put_varint(&mut out, file.size);
                    put_varint(&mut out, zigzag(file.mtime_ms));
                    out.extend(exact.before);
                    out.extend(exact.after);
                    put_varint(&mut out, exact.image.len() as u64);
                    out.extend(&exact.image);
                }
            }
            Inverse::Tags(files) => {
                let raw = files.iter().any(|f| f.raw_id3.is_some());
                let hashed = files.iter().any(|f| f.protected_hash.is_some());
                out.push(if hashed {
                    TAG_TAGS_HASH
                } else if raw {
                    TAG_TAGS_RAW
                } else {
                    TAG_TAGS
                });
                put_varint(&mut out, files.len() as u64);
                for file in files {
                    put_varint(&mut out, zigzag(file.track));
                    put_varint(&mut out, file.size);
                    put_varint(&mut out, zigzag(file.mtime_ms));
                    put_varint(&mut out, file.values.len() as u64);
                    for (key, values) in &file.values {
                        put_text(&mut out, key);
                        put_varint(&mut out, values.len() as u64);
                        for value in values {
                            put_text(&mut out, value);
                        }
                    }
                    if raw || hashed {
                        match &file.raw_id3 {
                            None => put_varint(&mut out, 0),
                            Some(bytes) => {
                                put_varint(&mut out, bytes.len() as u64 + 1);
                                out.extend(bytes);
                            }
                        }
                    }
                    if hashed {
                        out.push(u8::from(file.protected_hash.is_some()));
                        if let Some(hash) = file.protected_hash {
                            out.extend(hash);
                        }
                    }
                }
            }
        }
        out
    }

    /// Inverse of [`Inverse::encode`]; `None` if `data` is not one (corrupt, or written by
    /// a newer build).
    pub(crate) fn decode(data: &[u8]) -> Option<Self> {
        let (&tag, rest) = data.split_first()?;
        Some(match tag {
            8 | 9 => {
                let mut pos = 0;
                let count = usize::try_from(get_varint(rest, &mut pos)?).ok()?;
                if count > rest.len() {
                    return None;
                }
                let mut files = Vec::with_capacity(count);
                for _ in 0..count {
                    let track = unzigzag(get_varint(rest, &mut pos)?);
                    let expected = get_text(rest, &mut pos)?;
                    let restored_digest = if tag == 9 {
                        get_text(rest, &mut pos)?
                    } else {
                        String::new()
                    };
                    let count = usize::try_from(get_varint(rest, &mut pos)?).ok()?;
                    if count > rest.len() {
                        return None;
                    }
                    let mut regions = Vec::with_capacity(count);
                    for _ in 0..count {
                        let offset = get_varint(rest, &mut pos)?;
                        let replaced = get_varint(rest, &mut pos)?;
                        let len = usize::try_from(get_varint(rest, &mut pos)?).ok()?;
                        let end = pos.checked_add(len)?;
                        let bytes = rest.get(pos..end)?.to_vec();
                        pos = end;
                        regions.push(crate::cover::edit::Region {
                            offset,
                            replaced,
                            bytes,
                        });
                    }
                    files.push(crate::cover::job::FileCover {
                        track,
                        expected,
                        restored_digest,
                        regions,
                    });
                }
                if pos != rest.len() {
                    return None;
                }
                Inverse::Covers(files)
            }
            TAG_RATINGS => {
                let c = decode_ints(rest, 2)?;
                let values = c[0]
                    .iter()
                    .zip(&c[1])
                    .map(|(&id, &r)| match r {
                        0 => Some((id, None)),
                        1..=5 => Some((id, Some(r as u8))),
                        _ => None,
                    })
                    .collect::<Option<_>>()?;
                Inverse::Ratings(values)
            }
            TAG_LOVED => {
                let c = decode_ints(rest, 2)?;
                let values = c[0]
                    .iter()
                    .zip(&c[1])
                    .map(|(&id, &l)| (l == 0 || l == 1).then_some((id, l == 1)))
                    .collect::<Option<_>>()?;
                Inverse::Loved(values)
            }
            TAG_PLAYLIST => {
                let mut pos = 0;
                let id = PlaylistId(unzigzag(get_varint(rest, &mut pos)?));
                let (&op, rest) = rest[pos..].split_first()?;
                let op = match op {
                    OP_NAME => PlaylistOp::Name(String::from_utf8(rest.to_vec()).ok()?),
                    OP_QUERY => {
                        let mut pos = 0;
                        let limit = match unzigzag(get_varint(rest, &mut pos)?) {
                            -1 => None,
                            n => Some(u32::try_from(n).ok()?),
                        };
                        let json = String::from_utf8(rest[pos..].to_vec()).ok()?;
                        PlaylistOp::Query { json, limit }
                    }
                    OP_REMOVE => PlaylistOp::Remove(decode_ints(rest, 1)?.remove(0)),
                    OP_RESTORE => {
                        let c = decode_ints(rest, 3)?;
                        PlaylistOp::Restore(
                            (0..c[0].len())
                                .map(|i| (c[0][i], c[1][i], c[2][i]))
                                .collect(),
                        )
                    }
                    OP_MOVE => {
                        let c = decode_ints(rest, 2)?;
                        PlaylistOp::Move {
                            item: *c[0].first()?,
                            from: *c[1].first()?,
                        }
                    }
                    OP_TRASH if rest.is_empty() => PlaylistOp::Trash,
                    OP_UNTRASH if rest.is_empty() => PlaylistOp::Untrash,
                    _ => return None,
                };
                Inverse::Playlist(id, op)
            }
            TAG_ROOT => {
                let mut pos = 0;
                let id = RootId(unzigzag(get_varint(rest, &mut pos)?));
                let op = match &rest[pos..] {
                    [1] => RootOp::Remove,
                    [2] => RootOp::Restore,
                    _ => return None,
                };
                Inverse::Root(id, op)
            }
            TAG_TAGS_EXACT => {
                use crate::tags::edit::{MAX_TAG_INTENT_BYTES, snapshot::ByteUndo};
                if rest.len() > MAX_TAG_INTENT_BYTES {
                    return None;
                }
                let mut pos = 0;
                let count = usize::try_from(get_varint(rest, &mut pos)?).ok()?;
                if count > rest.len() / 64 {
                    return None;
                }
                let mut files = Vec::with_capacity(count);
                for _ in 0..count {
                    let track = unzigzag(get_varint(rest, &mut pos)?);
                    let size = get_varint(rest, &mut pos)?;
                    let mtime_ms = unzigzag(get_varint(rest, &mut pos)?);
                    let before = rest.get(pos..pos + 32)?.try_into().ok()?;
                    let after = rest.get(pos + 32..pos + 64)?.try_into().ok()?;
                    pos += 64;
                    let len = usize::try_from(get_varint(rest, &mut pos)?).ok()?;
                    let end = pos.checked_add(len)?;
                    let image = rest.get(pos..end)?.to_vec();
                    pos = end;
                    files.push(FileTags {
                        track,
                        size,
                        mtime_ms,
                        values: Vec::new(),
                        raw_id3: None,
                        protected_hash: None,
                        exact: Some(Box::new(ByteUndo {
                            before,
                            after,
                            image,
                        })),
                    });
                }
                if pos != rest.len() {
                    return None;
                }
                Inverse::Tags(files)
            }
            TAG_TAGS | TAG_TAGS_RAW | TAG_TAGS_HASH => {
                let mut pos = 0;
                let count = |pos: &mut usize| -> Option<usize> {
                    let n = usize::try_from(get_varint(rest, pos)?).ok()?;
                    // Each entry takes a byte at least: a longer count is not data.
                    (n <= rest.len()).then_some(n)
                };
                let files = count(&mut pos)?;
                let mut out = Vec::with_capacity(files);
                for _ in 0..files {
                    let track = unzigzag(get_varint(rest, &mut pos)?);
                    let size = get_varint(rest, &mut pos)?;
                    let mtime_ms = unzigzag(get_varint(rest, &mut pos)?);
                    let keys = count(&mut pos)?;
                    let mut values = Vec::with_capacity(keys);
                    for _ in 0..keys {
                        let key = get_text(rest, &mut pos)?;
                        let n = count(&mut pos)?;
                        let list = (0..n)
                            .map(|_| get_text(rest, &mut pos))
                            .collect::<Option<Vec<_>>>()?;
                        values.push((key, list));
                    }
                    let raw_id3 = if matches!(tag, TAG_TAGS_RAW | TAG_TAGS_HASH) {
                        let n = count(&mut pos)?;
                        if n == 0 {
                            None
                        } else {
                            let end = pos.checked_add(n - 1)?;
                            let bytes = rest.get(pos..end)?.to_vec();
                            pos = end;
                            Some(bytes)
                        }
                    } else {
                        None
                    };
                    let protected_hash = if tag == TAG_TAGS_HASH {
                        let present = *rest.get(pos)?;
                        pos += 1;
                        match present {
                            0 => None,
                            1 => {
                                let hash = rest.get(pos..pos.checked_add(32)?)?.try_into().ok()?;
                                pos += 32;
                                Some(hash)
                            }
                            _ => return None,
                        }
                    } else {
                        None
                    };
                    out.push(FileTags {
                        protected_hash,
                        raw_id3,
                        exact: None,
                        track,
                        size,
                        mtime_ms,
                        values,
                    });
                }
                if pos != rest.len() {
                    return None;
                }
                Inverse::Tags(out)
            }
            _ => return None,
        })
    }
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    put_varint(out, text.len() as u64);
    out.extend(text.as_bytes());
}

fn get_text(data: &[u8], pos: &mut usize) -> Option<String> {
    let len = usize::try_from(get_varint(data, pos)?).ok()?;
    let end = pos.checked_add(len).filter(|&end| end <= data.len())?;
    let text = std::str::from_utf8(&data[*pos..end]).ok()?.to_owned();
    *pos = end;
    Some(text)
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(z: u64) -> i64 {
    ((z >> 1) as i64) ^ -((z & 1) as i64)
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *data.get(*pos)?;
        *pos += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(v);
        }
    }
    None
}

/// Equal-length integer columns as LEB128 varints: the length, then each column
/// delta-coded with zigzag signs. Sorted ids and positions take one or two bytes each.
pub(crate) fn encode_ints(columns: &[&[i64]]) -> Vec<u8> {
    let len = columns.first().map_or(0, |c| c.len());
    let mut out = Vec::with_capacity(len * columns.len() * 2 + 4);
    put_varint(&mut out, len as u64);
    for column in columns {
        debug_assert_eq!(column.len(), len);
        let mut prev = 0i64;
        for &v in *column {
            put_varint(&mut out, zigzag(v.wrapping_sub(prev)));
            prev = v;
        }
    }
    out
}

/// Inverse of [`encode_ints`]; `None` if `data` is malformed.
pub(crate) fn decode_ints(data: &[u8], columns: usize) -> Option<Vec<Vec<i64>>> {
    let mut pos = 0;
    let len = usize::try_from(get_varint(data, &mut pos)?).ok()?;
    if len > data.len() {
        return None;
    }
    let mut out = Vec::with_capacity(columns);
    for _ in 0..columns {
        let mut column = Vec::with_capacity(len);
        let mut prev = 0i64;
        for _ in 0..len {
            prev = prev.wrapping_add(unzigzag(get_varint(data, &mut pos)?));
            column.push(prev);
        }
        out.push(column);
    }
    (pos == data.len()).then_some(out)
}
