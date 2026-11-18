//! Keeps sample tables and other boxes in place. When metadata grows, change the
//! stored offsets to address the same relative positions in their original mdat.
use super::*;

#[derive(Clone, Copy)]
struct Atom {
    start: u64,
    body: u64,
    end: u64,
    kind: [u8; 4],
    open_ended: bool,
}

fn atoms(file: &mut File, mut at: u64, end: u64) -> io::Result<Vec<Atom>> {
    let mut atoms = Vec::new();
    while at < end {
        if end - at < 8 {
            return Err(unsupported());
        }
        let header = read_at(file, at, 8)?;
        let short = u32::from_be_bytes(header[..4].try_into().unwrap());
        let (size, head) = match short {
            0 => (end - at, 8),
            1 => (
                u64::from_be_bytes(read_at(file, at + 8, 8)?.try_into().unwrap()),
                16,
            ),
            n => (u64::from(n), 8),
        };
        if size < head || size > end - at {
            return Err(unsupported());
        }
        atoms.push(Atom {
            start: at,
            body: at + head,
            end: at + size,
            kind: header[4..].try_into().unwrap(),
            open_ended: short == 0,
        });
        at += size;
    }
    Ok(atoms)
}

pub(super) fn classify(file: &mut File) -> io::Result<()> {
    let top = atoms(file, 0, file.metadata()?.len())?;
    if top
        .iter()
        .any(|a| matches!(&a.kind, b"moof" | b"mfra" | b"sidx"))
    {
        return Err(unsupported());
    }
    Ok(())
}

fn find(atoms: &[Atom], kind: &[u8; 4]) -> io::Result<Option<Atom>> {
    let mut matches = atoms.iter().filter(|a| &a.kind == kind);
    let first = matches.next().copied();
    if matches.next().is_some() {
        return Err(unsupported());
    }
    Ok(first)
}

fn boxed(kind: &[u8; 4], bytes: &[u8]) -> io::Result<Vec<u8>> {
    let size = u32::try_from(bytes.len() + 8).map_err(|_| unsupported())?;
    let mut atom = size.to_be_bytes().to_vec();
    atom.extend(kind);
    atom.extend(bytes);
    Ok(atom)
}

fn meta(ilst: &[u8]) -> io::Result<Vec<u8>> {
    let mut body = vec![0; 4];
    let mut handler = vec![0; 8];
    handler.extend(b"mdirappl");
    handler.extend([0; 9]);
    body.extend(boxed(b"hdlr", &handler)?);
    body.extend(ilst);
    boxed(b"meta", &body)
}

pub(super) fn regions(file: &mut File, ilst: Vec<u8>) -> io::Result<Vec<Region>> {
    let size = file.metadata()?.len();
    let top = atoms(file, 0, size)?;
    let movie = find(&top, b"moov")?.ok_or_else(unsupported)?;
    let payloads: Vec<_> = top.iter().filter(|a| &a.kind == b"mdat").copied().collect();
    if payloads.is_empty() {
        return Err(unsupported());
    }
    let mut parents = vec![movie];
    let children = atoms(file, movie.body, movie.end)?;
    let edit = if let Some(user) = find(&children, b"udta")? {
        parents.push(user);
        let children = atoms(file, user.body, user.end)?;
        if let Some(metadata) = find(&children, b"meta")? {
            parents.push(metadata);
            let body = metadata.body;
            // iTunes FullBox or QuickTime's unversioned meta.
            let marker = read_at(file, body, 4)?;
            let start = if marker == [0; 4] { body + 4 } else { body };
            let nested = atoms(file, start, metadata.end)?;
            let range =
                find(&nested, b"ilst")?.map_or(metadata.end..metadata.end, |a| a.start..a.end);
            Region { range, bytes: ilst }
        } else {
            Region {
                range: user.end..user.end,
                bytes: meta(&ilst)?,
            }
        }
    } else {
        Region {
            range: movie.end..movie.end,
            bytes: boxed(b"udta", &meta(&ilst)?)?,
        }
    };
    let movement = edit.bytes.len() as i64 - (edit.range.end - edit.range.start) as i64;
    if movement != 0 && payloads.iter().any(|data| data.start >= movie.end) {
        if top
            .iter()
            .any(|atom| matches!(&atom.kind, b"meta" | b"iloc" | b"saio" | b"ssix" | b"uuid"))
        {
            return Err(unsupported());
        }
        check_offset_extensions(file, &children, 0)?;
    }
    let mut patches = Vec::new();
    if movement != 0 {
        for a in parents {
            if a.open_ended {
                continue;
            }
            let size = (a.end - a.start)
                .checked_add_signed(movement)
                .ok_or_else(unsupported)?;
            let (start, bytes) = if a.body - a.start == 8 {
                (
                    a.start,
                    u32::try_from(size)
                        .map_err(|_| unsupported())?
                        .to_be_bytes()
                        .to_vec(),
                )
            } else {
                (a.start + 8, size.to_be_bytes().to_vec())
            };
            patches.push(Region {
                range: start..start + bytes.len() as u64,
                bytes,
            });
        }
    }
    offsets(
        file,
        &children,
        &payloads,
        movie.end,
        movement,
        &mut patches,
        0,
    )?;
    patches.push(edit);
    patches.sort_by_key(|a| a.range.start);
    Ok(patches)
}

fn check_offset_extensions(file: &mut File, children: &[Atom], depth: usize) -> io::Result<()> {
    if depth > 12 {
        return Err(unsupported());
    }
    for atom in children {
        let from = match &atom.kind {
            b"udta" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"dinf" => atom.body,
            b"meta" => {
                if read_at(file, atom.body, 4)? == [0; 4] {
                    atom.body + 4
                } else {
                    atom.body
                }
            }
            b"dref" => {
                let header = read_at(file, atom.body, 8)?;
                let entries = atoms(file, atom.body + 8, atom.end)?;
                if header[..4] != [0; 4]
                    || u32::from_be_bytes(header[4..].try_into().unwrap()) as usize != entries.len()
                {
                    return Err(unsupported());
                }
                for entry in entries {
                    // A self-contained data reference has no external URL or URN.
                    if &entry.kind != b"url "
                        || entry.end - entry.body != 4
                        || read_at(file, entry.body, 4)? != [0, 0, 0, 1]
                    {
                        return Err(unsupported());
                    }
                }
                continue;
            }
            b"iloc" | b"saio" | b"sidx" | b"ssix" | b"uuid" => return Err(unsupported()),
            _ => continue,
        };
        let nested = atoms(file, from, atom.end)?;
        check_offset_extensions(file, &nested, depth + 1)?;
    }
    Ok(())
}

struct Sample {
    mdat: usize,
    relative: u64,
    absolute: u64,
    length: usize,
}

fn samples(
    file: &mut File,
    tree: &[Atom],
    data: &[Atom],
    output: &mut Vec<Sample>,
    depth: usize,
) -> io::Result<()> {
    if depth > 12 {
        return Err(unsupported());
    }
    for atom in tree {
        match &atom.kind {
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" => {
                let children = atoms(file, atom.body, atom.end)?;
                samples(file, &children, data, output, depth + 1)?;
            }
            b"stco" | b"co64" => {
                let width = if &atom.kind == b"co64" { 8 } else { 4 };
                let count =
                    u32::from_be_bytes(read_at(file, atom.body + 4, 4)?.try_into().unwrap()) as u64;
                if count * width != atom.end.saturating_sub(atom.body + 8) {
                    return Err(unsupported());
                }
                let probes = count.min(32);
                for i in 0..probes {
                    let index = i * count.saturating_sub(1) / probes.saturating_sub(1).max(1);
                    let bytes = read_at(file, atom.body + 8 + index * width, width as usize)?;
                    let absolute = if width == 8 {
                        u64::from_be_bytes(bytes.try_into().unwrap())
                    } else {
                        u64::from(u32::from_be_bytes(bytes.try_into().unwrap()))
                    };
                    let (mdat, payload) = data
                        .iter()
                        .enumerate()
                        .find(|(_, payload)| (payload.body..payload.end).contains(&absolute))
                        .ok_or_else(unsupported)?;
                    output.push(Sample {
                        mdat,
                        relative: absolute - payload.body,
                        absolute,
                        length: (payload.end - absolute).min(64) as usize,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Parse the rendered file independently of the rewrite map. For each moved
/// sample table, compare its first, last and evenly spaced media references.
pub(super) fn validate_samples(before: &mut File, after: &mut File) -> io::Result<()> {
    let old = atoms(before, 0, before.metadata()?.len())?;
    let new = atoms(after, 0, after.metadata()?.len())?;
    let media = |top: &[Atom]| {
        top.iter()
            .filter(|atom| &atom.kind == b"mdat")
            .copied()
            .collect::<Vec<_>>()
    };
    let (old_data, new_data) = (media(&old), media(&new));
    if old_data.len() != new_data.len() {
        return Err(unsupported());
    }
    if old_data
        .iter()
        .zip(&new_data)
        .all(|(a, b)| a.body == b.body)
    {
        return Ok(());
    }
    let (mut left, mut right) = (Vec::new(), Vec::new());
    samples(before, &old, &old_data, &mut left, 0)?;
    samples(after, &new, &new_data, &mut right, 0)?;
    if left.is_empty() || left.len() != right.len() {
        return Err(unsupported());
    }
    for (a, b) in left.iter().zip(&right) {
        if (a.mdat, a.relative, a.length) != (b.mdat, b.relative, b.length)
            || read_at(before, a.absolute, a.length)? != read_at(after, b.absolute, b.length)?
        {
            return Err(unsupported());
        }
    }
    Ok(())
}

fn offsets(
    file: &mut File,
    atoms_here: &[Atom],
    payloads: &[Atom],
    split: u64,
    movement: i64,
    patches: &mut Vec<Region>,
    depth: usize,
) -> io::Result<()> {
    if depth > 12 {
        return Err(unsupported());
    }
    for a in atoms_here {
        match &a.kind {
            b"trak" | b"mdia" | b"minf" | b"stbl" => {
                let children = atoms(file, a.body, a.end)?;
                offsets(
                    file,
                    &children,
                    payloads,
                    split,
                    movement,
                    patches,
                    depth + 1,
                )?;
            }
            b"saio" | b"iloc" | b"cmov" | b"mvex" => return Err(unsupported()),
            b"stco" | b"co64" => {
                let width = if &a.kind == b"co64" { 8 } else { 4 };
                let header = read_at(file, a.body, 8)?;
                if header[..4] != [0; 4] {
                    return Err(unsupported());
                }
                let count = u32::from_be_bytes(header[4..].try_into().unwrap()) as u64;
                if count * width != a.end.saturating_sub(a.body + 8)
                    || count * width > MAX_REGION as u64
                {
                    return Err(unsupported());
                }
                let mut table = read_at(file, a.body + 8, (count * width) as usize)?;
                for entry in table.chunks_exact_mut(width as usize) {
                    let old = if width == 8 {
                        u64::from_be_bytes((*entry).try_into().unwrap())
                    } else {
                        u64::from(u32::from_be_bytes((*entry).try_into().unwrap()))
                    };
                    let atom = payloads
                        .iter()
                        .find(|p| (p.body..p.end).contains(&old))
                        .ok_or_else(unsupported)?;
                    let shift = if atom.start >= split { movement } else { 0 };
                    let start = atom
                        .body
                        .checked_add_signed(shift)
                        .ok_or_else(unsupported)?;
                    let new = start.checked_add(old - atom.body).ok_or_else(unsupported)?;
                    if width == 8 {
                        entry.copy_from_slice(&new.to_be_bytes());
                    } else {
                        entry.copy_from_slice(
                            &u32::try_from(new).map_err(|_| unsupported())?.to_be_bytes(),
                        );
                    }
                }
                if movement != 0 {
                    patches.push(Region {
                        range: a.body + 8..a.end,
                        bytes: table,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(())
}
