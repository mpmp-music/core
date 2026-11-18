//! A reversible byte program. Offsets address the complete current file.
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use super::ogg;

pub(crate) const MAGIC: &[u8; 8] = b"PMPTAG\0\x02";
pub(crate) const PAIR_MAGIC: &[u8; 8] = b"PMPTAG\0\x03";

/// The two verified programs travel together so undo can transfer its allocation.
pub(in crate::tags) fn pair(undo: &[u8], redo: &[u8]) -> Vec<u8> {
    let mut image = Vec::with_capacity(16 + undo.len() + redo.len());
    image.extend(PAIR_MAGIC);
    image.extend((undo.len() as u64).to_le_bytes());
    image.extend(undo);
    image.extend(redo);
    image
}

pub(in crate::tags) fn split_pair(image: &[u8]) -> io::Result<(&[u8], &[u8])> {
    let rest = image.strip_prefix(PAIR_MAGIC).ok_or_else(invalid)?;
    let (n, payload) = rest.split_first_chunk().ok_or_else(invalid)?;
    let n = usize::try_from(u64::from_le_bytes(*n)).map_err(|_| invalid())?;
    payload.split_at_checked(n).ok_or_else(invalid)
}

pub(in crate::tags) struct Region {
    pub range: Range<u64>,
    pub bytes: Vec<u8>,
}

enum Part {
    Copy(u64, u64),
    Bytes(Vec<u8>),
    Pages(u64, u64, u32),
}

#[derive(Default)]
pub(in crate::tags) struct Delta(Vec<Part>);

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid tag delta")
}

fn too_large() -> io::Error {
    io::Error::new(io::ErrorKind::FileTooLarge, "tag delta exceeds budget")
}

pub(super) fn read_at(input: &mut (impl Read + Seek), at: u64, len: usize) -> io::Result<Vec<u8>> {
    input.seek(SeekFrom::Start(at))?;
    let mut bytes = vec![0; len];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

impl Part {
    fn len(&self) -> u64 {
        match self {
            Self::Copy(_, len) | Self::Pages(_, len, _) => *len,
            Self::Bytes(bytes) => bytes.len() as u64,
        }
    }
}

impl Delta {
    pub(super) fn copy(&mut self, offset: u64, len: u64) {
        if len != 0 {
            if let Some(Part::Copy(previous, count)) = self.0.last_mut()
                && previous.checked_add(*count) == Some(offset)
            {
                *count += len;
            } else {
                self.0.push(Part::Copy(offset, len));
            }
        }
    }

    pub(super) fn literal(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            if let Some(Part::Bytes(previous)) = self.0.last_mut() {
                previous.extend_from_slice(bytes);
            } else {
                self.0.push(Part::Bytes(bytes.to_vec()));
            }
        }
    }

    /// Pair matching runs inside a rewritten region. Long unchanged payloads (APIC,
    /// covr, padding) remain references even if a serializer moves them.
    fn region(&mut self, old: &[u8], new: &[u8], offset: u64) {
        let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
        self.copy(offset, prefix as u64);
        let (old, new) = (&old[prefix..], &new[prefix..]);
        let suffix = old
            .iter()
            .rev()
            .zip(new.iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        let end = offset + prefix as u64 + (old.len() - suffix) as u64;
        let (old, new) = (&old[..old.len() - suffix], &new[..new.len() - suffix]);
        self.match_runs(old, new, offset + prefix as u64);
        self.copy(end, suffix as u64);
    }

    fn match_runs(&mut self, old: &[u8], new: &[u8], offset: u64) {
        const WIDTH: usize = 32;
        if old.len() < WIDTH || new.len() < WIDTH {
            self.literal(new);
            return;
        }
        let mut index = HashMap::new();
        for (i, key) in old.as_chunks::<WIDTH>().0.iter().enumerate() {
            index.entry(key.as_slice()).or_insert(i * WIDTH);
        }
        let (mut cursor, mut literal) = (0, 0);
        while cursor + WIDTH <= new.len() {
            if let Some(&from) = index.get(&new[cursor..cursor + WIDTH]) {
                let back = old[..from]
                    .iter()
                    .rev()
                    .zip(new[literal..cursor].iter().rev())
                    .take_while(|(a, b)| a == b)
                    .count();
                let forward = old[from + WIDTH..]
                    .iter()
                    .zip(&new[cursor + WIDTH..])
                    .take_while(|(a, b)| a == b)
                    .count();
                self.literal(&new[literal..cursor - back]);
                self.copy(
                    offset + (from - back) as u64,
                    (back + WIDTH + forward) as u64,
                );
                cursor += WIDTH + forward;
                literal = cursor;
            } else {
                cursor += 1;
            }
        }
        self.literal(&new[literal..]);
    }

    pub fn from_regions(
        source: &mut File,
        regions: Vec<Region>,
        pages: Option<(u64, u32)>,
    ) -> io::Result<Self> {
        let size = source.metadata()?.len();
        let mut delta = Self::default();
        let mut pos = 0;
        for region in regions {
            let range = region.range;
            if range.start < pos || range.end < range.start || range.end > size {
                return Err(invalid());
            }
            delta.copy(pos, range.start - pos);
            let count = usize::try_from(range.end - range.start).map_err(|_| too_large())?;
            if count > super::plan::MAX_REGION || region.bytes.len() > super::plan::MAX_REGION {
                return Err(too_large());
            }
            let old = read_at(source, range.start, count)?;
            delta.region(&old, &region.bytes, range.start);
            pos = range.end;
        }
        if let Some((start, shift)) = pages {
            if !(pos..=size).contains(&start) {
                return Err(invalid());
            }
            delta.copy(pos, start - pos);
            if size != start {
                delta.0.push(Part::Pages(start, size - start, shift));
            }
        } else {
            delta.copy(pos, size - pos);
        }
        Ok(delta)
    }

    fn ranges(&self, size: u64) -> io::Result<Vec<(u64, u64, u64, u32)>> {
        let mut pos = 0u64;
        let mut ranges = Vec::new();
        for part in &self.0 {
            let (start, len, shift) = match *part {
                Part::Copy(start, len) => (start, len, 0),
                Part::Pages(start, len, shift) => (start, len, shift),
                Part::Bytes(_) => {
                    pos = pos.checked_add(part.len()).ok_or_else(invalid)?;
                    continue;
                }
            };
            let end = start
                .checked_add(len)
                .filter(|e| *e <= size)
                .ok_or_else(invalid)?;
            ranges.push((start, end, pos, shift));
            pos = pos.checked_add(len).ok_or_else(invalid)?;
        }
        ranges.sort_unstable();
        Ok(ranges)
    }

    pub fn invert(&self, source: &mut File, limit: usize) -> io::Result<Self> {
        let size = source.metadata()?.len();
        let mut inverse = Self::default();
        let mut pos = 0;
        let mut literals = 0;
        let mut insert_literal = |inverse: &mut Self, start, end| -> io::Result<()> {
            let count = usize::try_from(end - start).map_err(|_| too_large())?;
            literals += count;
            if literals > limit {
                return Err(too_large());
            }
            if count != 0 {
                inverse.literal(&read_at(source, start, count)?);
            }
            Ok(())
        };
        for (start, end, output, shift) in self.ranges(size)? {
            if end <= pos {
                continue;
            }
            if start > pos {
                insert_literal(&mut inverse, pos, start)?;
            }
            if shift != 0 {
                if pos > start {
                    return Err(invalid());
                }
                inverse
                    .0
                    .push(Part::Pages(output, end - start, shift.wrapping_neg()));
            } else {
                let begin = pos.max(start);
                inverse.copy(output + begin - start, end - begin);
            }
            pos = end;
        }
        insert_literal(&mut inverse, pos, size)?;
        Ok(inverse)
    }

    pub fn literal_len(&self) -> usize {
        self.0
            .iter()
            .filter_map(|p| match p {
                Part::Bytes(bytes) => Some(bytes.len()),
                _ => None,
            })
            .sum()
    }

    pub fn encoded(&self, literal_limit: usize) -> io::Result<Vec<u8>> {
        if self.literal_len() > literal_limit {
            return Err(too_large());
        }
        let mut out = MAGIC.to_vec();
        for part in &self.0 {
            match part {
                Part::Copy(offset, len) | Part::Pages(offset, len, _) => {
                    out.push(if matches!(part, Part::Pages(..)) {
                        2
                    } else {
                        0
                    });
                    out.extend(offset.to_le_bytes());
                    out.extend(len.to_le_bytes());
                    if let Part::Pages(_, _, shift) = part {
                        out.extend(shift.to_le_bytes());
                    }
                }
                Part::Bytes(bytes) => {
                    out.push(1);
                    out.extend((bytes.len() as u64).to_le_bytes());
                    out.extend(bytes);
                }
            }
        }
        // Bound encoding overhead separately from the per-file literal budget.
        if out.len() > literal_limit.saturating_add(2 * 1024 * 1024) {
            return Err(too_large());
        }
        Ok(out)
    }

    pub fn decode(mut bytes: &[u8]) -> io::Result<Self> {
        fn take<const N: usize>(bytes: &mut &[u8]) -> io::Result<[u8; N]> {
            let (head, tail) = bytes.split_first_chunk().ok_or_else(invalid)?;
            *bytes = tail;
            Ok(*head)
        }
        bytes = bytes.strip_prefix(MAGIC).ok_or_else(invalid)?;
        let mut delta = Self::default();
        while let Some((&kind, rest)) = bytes.split_first() {
            bytes = rest;
            let n = u64::from_le_bytes(take(&mut bytes)?);
            match kind {
                0 | 2 => {
                    let len = u64::from_le_bytes(take(&mut bytes)?);
                    if kind == 0 {
                        delta.copy(n, len);
                    } else {
                        delta
                            .0
                            .push(Part::Pages(n, len, u32::from_le_bytes(take(&mut bytes)?)));
                    }
                }
                1 => {
                    let (literal, tail) = bytes
                        .split_at_checked(usize::try_from(n).map_err(|_| invalid())?)
                        .ok_or_else(invalid)?;
                    delta.literal(literal);
                    bytes = tail;
                }
                _ => return Err(invalid()),
            }
        }
        Ok(delta)
    }

    pub fn render(&self, input: &mut File, output: &mut impl Write) -> io::Result<()> {
        self.ranges(input.metadata()?.len())?;
        let mut output = BufWriter::new(output);
        for part in &self.0 {
            match part {
                Part::Copy(offset, len) => {
                    input.seek(SeekFrom::Start(*offset))?;
                    if io::copy(&mut (&mut *input).take(*len), &mut output)? != *len {
                        return Err(invalid());
                    }
                }
                Part::Bytes(bytes) => output.write_all(bytes)?,
                Part::Pages(offset, len, shift) => {
                    input.seek(SeekFrom::Start(*offset))?;
                    ogg::shift(
                        &mut BufReader::new((&mut *input).take(*len)),
                        &mut output,
                        *shift,
                    )?;
                }
            }
        }
        output.flush()
    }

    pub fn reader<'a>(&'a self, file: &'a mut File) -> io::Result<Projection<'a>> {
        let mut end = 0u64;
        let starts = self
            .0
            .iter()
            .map(|p| {
                let start = end;
                end = end.checked_add(p.len()).ok_or_else(invalid)?;
                Ok(start)
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Projection {
            file,
            delta: self,
            starts,
            end,
            pos: 0,
        })
    }
}

/// Read tags from the projected file without copying or hashing its audio. Page
/// resequencing affects only audio-page headers, which the tag-only parser skips.
pub(in crate::tags) struct Projection<'a> {
    file: &'a mut File,
    delta: &'a Delta,
    starts: Vec<u64>,
    end: u64,
    pos: u64,
}

impl Read for Projection<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.end || buf.is_empty() {
            return Ok(0);
        }
        let index = self.starts.partition_point(|start| *start <= self.pos) - 1;
        let part = &self.delta.0[index];
        let offset = self.pos - self.starts[index];
        let take = (part.len() - offset).min(buf.len() as u64) as usize;
        let n = match part {
            Part::Copy(start, _) | Part::Pages(start, _, _) => {
                self.file.seek(SeekFrom::Start(start + offset))?;
                self.file.read(&mut buf[..take])?
            }
            Part::Bytes(bytes) => {
                buf[..take].copy_from_slice(&bytes[offset as usize..offset as usize + take]);
                take
            }
        };
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Projection<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.pos = match position {
            SeekFrom::Start(pos) => Some(pos),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
            SeekFrom::End(delta) => self.end.checked_add_signed(delta),
        }
        .ok_or_else(invalid)?;
        Ok(self.pos)
    }
}
