//! Replace one comment packet while preserving all other packet bytes and audio-page
//! granules, even if the last comment page also carries setup or audio packets.
use std::io::{self, Read, Seek, SeekFrom, Write};

pub(super) struct Header {
    pub bytes: [u8; 27],
    laces: [u8; 255],
}

impl Header {
    pub fn read(reader: &mut impl Read) -> io::Result<Option<Self>> {
        let mut bytes = [0; 27];
        if reader.read(&mut bytes[..1])? == 0 {
            return Ok(None);
        }
        reader.read_exact(&mut bytes[1..])?;
        if &bytes[..5] != b"OggS\0" || bytes[5] & !7 != 0 {
            return Err(invalid());
        }
        let mut laces = [0; 255];
        reader.read_exact(&mut laces[..usize::from(bytes[26])])?;
        Ok(Some(Self { bytes, laces }))
    }

    pub fn laces(&self) -> &[u8] {
        &self.laces[..usize::from(self.bytes[26])]
    }

    fn sequence(&self) -> u32 {
        u32::from_le_bytes(self.bytes[18..22].try_into().unwrap())
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid Ogg page layout")
}

const CRC_TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut i = 0;
    while i < table.len() {
        let mut crc = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            let polynomial = if crc & 0x8000_0000 == 0 {
                0
            } else {
                0x04c1_1db7
            };
            crc = (crc << 1) ^ polynomial;
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

fn emit(
    mut bytes: [u8; 27],
    laces: &[u8],
    body: &[u8],
    sequence: u32,
    writer: &mut impl Write,
) -> io::Result<()> {
    bytes[18..22].copy_from_slice(&sequence.to_le_bytes());
    bytes[22..26].fill(0);
    bytes[26] = laces.len() as u8;
    let mut crc = 0u32;
    for b in bytes.iter().chain(laces).chain(body) {
        crc = (crc << 8) ^ CRC_TABLE[((crc >> 24) as u8 ^ b) as usize];
    }
    bytes[22..26].copy_from_slice(&crc.to_le_bytes());
    writer.write_all(&bytes)?;
    writer.write_all(laces)?;
    writer.write_all(body)
}

impl Header {
    fn copy_page(&self, body: &[u8], sequence: u32, output: &mut impl Write) -> io::Result<()> {
        if sequence == self.sequence() {
            output.write_all(&self.bytes)?;
            output.write_all(self.laces())?;
            output.write_all(body)
        } else {
            emit(self.bytes, self.laces(), body, sequence, output)
        }
    }
}

fn body_size(laces: &[u8]) -> usize {
    laces.iter().map(|&lace| usize::from(lace)).sum()
}

struct Replacement<'a> {
    packet: &'a [u8],
    started: bool,
    /// Number of emitted pages minus consumed pages of the selected stream.
    shift: u32,
}

impl Replacement<'_> {
    fn packet_pages(
        &mut self,
        header: &Header,
        writer: &mut impl Write,
        mut sequence: u32,
        eof: bool,
    ) -> io::Result<u32> {
        const FULL: usize = 255 * 255;
        let mut packet = self.packet;
        let mut flags = header.bytes[5] & 2;
        if self.started {
            flags = 0;
        }
        loop {
            let mut bytes = header.bytes;
            let mut laces = [255; 255];
            if packet.len() < FULL {
                let count = packet.len() / 255;
                laces[count] = (packet.len() % 255) as u8;
                bytes[5] = flags | if eof { 4 } else { 0 };
                bytes[6..14].copy_from_slice(&0u64.to_le_bytes());
                emit(bytes, &laces[..=count], packet, sequence, writer)?;
                return Ok(sequence.wrapping_add(1));
            }
            bytes[5] = flags;
            bytes[6..14].fill(255); // No packet finishes on this page.
            emit(bytes, &laces, &packet[..FULL], sequence, writer)?;
            packet = &packet[FULL..];
            sequence = sequence.wrapping_add(1);
            flags = 1;
        }
    }
}

pub(super) fn rewrite_header(
    reader: &mut impl Read,
    writer: &mut impl Write,
    packet: &[u8],
) -> io::Result<(u32, usize)> {
    let mut state = Replacement {
        packet,
        started: false,
        shift: 0,
    };
    let mut identification = true;
    let mut body = Vec::new();
    let mut serial = None;
    while let Some(header) = Header::read(reader)? {
        let current: [u8; 4] = header.bytes[14..18].try_into().unwrap();
        if serial.get_or_insert(current) != &current {
            return Err(invalid());
        }
        let laces = header.laces();
        body.resize(body_size(laces), 0);
        reader.read_exact(&mut body)?;
        let mut sequence = header.sequence().wrapping_add(state.shift);
        let mut head = 0;
        if identification {
            if let Some(i) = laces.iter().position(|&lace| lace < 255) {
                head = i + 1;
                identification = false;
            } else {
                head = laces.len();
            }
            if head == laces.len() {
                header.copy_page(&body, sequence, writer)?;
                continue;
            }
        }
        if head != 0 {
            // Identification and comment on the same BOS page is nonconforming.
            if header.bytes[5] & 2 != 0 {
                return Err(invalid());
            }
            let mut bytes = header.bytes;
            bytes[5] &= !4;
            bytes[6..14].copy_from_slice(&0u64.to_le_bytes());
            emit(
                bytes,
                &laces[..head],
                &body[..body_size(&laces[..head])],
                sequence,
                writer,
            )?;
            sequence = sequence.wrapping_add(1);
        }
        if let Some(last) = laces[head..].iter().position(|&lace| lace < 255) {
            let tail = head + last + 1;
            let page_ends_here = tail == laces.len();
            let eof = page_ends_here && header.bytes[5] & 4 != 0;
            sequence = state.packet_pages(&header, writer, sequence, eof)?;
            let shared_payload = &body[body_size(&laces[..tail])..];
            if !page_ends_here {
                let mut bytes = header.bytes;
                bytes[5] &= 4;
                if laces[tail..].iter().all(|&lace| lace == 255) {
                    bytes[6..14].fill(255);
                }
                emit(bytes, &laces[tail..], shared_payload, sequence, writer)?;
                sequence = sequence.wrapping_add(1);
            }
            let shift = sequence.wrapping_sub(header.sequence().wrapping_add(1));
            return Ok((shift, shared_payload.len()));
        }
        state.started = true;
        state.shift = sequence.wrapping_sub(header.sequence().wrapping_add(1));
    }
    Err(invalid())
}

#[derive(Default)]
struct StreamPages {
    next: Option<([u8; 4], u32)>,
    ended: bool,
}

impl StreamPages {
    fn check(&mut self, header: &Header, bos_allowed: bool) -> io::Result<()> {
        let serial = header.bytes[14..18].try_into().unwrap();
        let sequence = header.sequence();
        let bos = header.bytes[5] & 2 != 0;
        if self.ended
            || bos != bos_allowed
            || self
                .next
                .is_some_and(|expected| expected != (serial, sequence))
        {
            return Err(invalid());
        }
        self.next = Some((serial, sequence.wrapping_add(1)));
        self.ended = header.bytes[5] & 4 != 0;
        Ok(())
    }
}

/// Validates every page header and skips payloads with seeks.
pub(super) fn classify(file: &mut std::fs::File) -> io::Result<()> {
    file.rewind()?;
    let end = file.metadata()?.len();
    let mut stream = StreamPages::default();
    let mut at = 0;
    while at < end {
        let header = Header::read(file)?.ok_or_else(invalid)?;
        stream.check(&header, at == 0)?;
        if at == 0
            && header
                .laces()
                .iter()
                .position(|&lace| lace < 255)
                .is_some_and(|index| index + 1 != header.laces().len())
        {
            return Err(invalid());
        }
        at += (27 + header.laces().len() + body_size(header.laces())) as u64;
        if at > end {
            return Err(invalid());
        }
        file.seek(SeekFrom::Start(at))?;
    }
    file.rewind()
}

pub(super) fn shift(reader: &mut impl Read, writer: &mut impl Write, shift: u32) -> io::Result<()> {
    let mut body = Vec::new();
    let mut stream = StreamPages::default();
    while let Some(header) = Header::read(reader)? {
        stream.check(&header, false)?;
        body.resize(body_size(header.laces()), 0);
        reader.read_exact(&mut body)?;
        header.copy_page(&body, header.sequence().wrapping_add(shift), writer)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_comment_across_pages_without_storing_or_rewriting_audio_packets() {
        let mut header = [0; 27];
        header[..4].copy_from_slice(b"OggS");
        header[5] = 2;
        let mut data = Vec::new();
        emit(header, &[1], b"I", 0, &mut data).unwrap();
        header[5] = 4;
        header[6..14].copy_from_slice(&320u64.to_le_bytes());
        emit(header, &[1, 5], b"CAUDIO", 1, &mut data).unwrap();

        // An exact multiple of the maximum page body needs a zero terminator on the
        // following continued page.
        let comment = vec![b'x'; 255 * 255];
        let mut edited = Vec::new();
        rewrite_header(&mut data.as_slice(), &mut edited, &comment).unwrap();
        let mut edited = edited.as_slice();
        let ident = Header::read(&mut edited).unwrap().unwrap();
        assert_eq!(ident.laces(), [1]);
        assert_eq!(&edited[..1], b"I");
        edited = &edited[1..];

        let first = Header::read(&mut edited).unwrap().unwrap();
        assert_eq!(first.sequence(), 1);
        assert_eq!(first.bytes[5], 0);
        assert_eq!(first.bytes[6..14], [255; 8]);
        assert_eq!(first.laces(), [255; 255]);
        assert_eq!(&edited[..comment.len()], comment);
        edited = &edited[comment.len()..];

        let last = Header::read(&mut edited).unwrap().unwrap();
        assert_eq!(last.bytes[5], 1);
        assert_eq!(last.sequence(), 2);
        assert_eq!(last.laces(), [0]);

        let audio = Header::read(&mut edited).unwrap().unwrap();
        assert_eq!(audio.bytes[5], 4);
        assert_eq!(audio.bytes[6..14], 320u64.to_le_bytes());
        assert_eq!(audio.sequence(), 3);
        assert_eq!(audio.laces(), [5]);
        assert_eq!(edited, b"AUDIO");
    }

    fn pages(mut bytes: &[u8]) -> Vec<(Header, Vec<u8>)> {
        let mut pages = Vec::new();
        while !bytes.is_empty() {
            let begin = bytes;
            let header = Header::read(&mut bytes).unwrap().unwrap();
            let size = body_size(header.laces());
            let body = bytes[..size].to_vec();
            bytes = &bytes[size..];
            let page = &begin[..begin.len() - bytes.len()];
            let mut crc = 0u32;
            for (i, &byte) in page.iter().enumerate() {
                crc ^= u32::from(if (22..26).contains(&i) { 0 } else { byte }) << 24;
                for _ in 0..8 {
                    crc = (crc << 1) ^ if crc >> 31 == 1 { 0x04c1_1db7 } else { 0 };
                }
            }
            assert_eq!(crc.to_le_bytes(), header.bytes[22..26]);
            pages.push((header, body));
        }
        pages
    }

    #[test]
    fn real_vorbis_split_setup_page_has_no_completed_packet_granule() {
        let fixture = include_bytes!("../../../../engine/tests/fixtures/sine44_a.ogg");
        let original = pages(fixture);
        let (ident, ident_body) = &original[0];
        let (setup, body) = &original[1];
        let laces = setup.laces();
        assert!(laces[0] < 255 && laces[1] == 255);
        let split = body_size(&laces[..2]);
        let mut input = Vec::new();
        emit(ident.bytes, ident.laces(), ident_body, 0, &mut input).unwrap();
        emit(setup.bytes, &laces[..2], &body[..split], 1, &mut input).unwrap();
        let mut continued = setup.bytes;
        continued[5] = 1;
        emit(continued, &laces[2..], &body[split..], 2, &mut input).unwrap();
        let (audio, audio_body) = &original[2];
        emit(audio.bytes, audio.laces(), audio_body, 3, &mut input).unwrap();
        let mut input = input.as_slice();
        let mut output = Vec::new();
        let new_comment = &body[..usize::from(laces[0])];
        let (sequence_shift, shared_payload) =
            rewrite_header(&mut input, &mut output, new_comment).unwrap();
        assert_eq!(shared_payload, 255);
        shift(&mut input, &mut output, sequence_shift).unwrap();
        let edited = pages(&output);
        assert_eq!(edited.len(), 5);
        for (index, (header, _)) in edited.iter().enumerate() {
            assert_eq!(header.sequence(), index as u32);
        }
        assert_eq!(edited[2].0.bytes[5], 0);
        assert_eq!(edited[2].0.bytes[6..14], [255; 8]);
        assert_eq!(edited[2].0.laces(), [255]);
        assert_eq!(edited[3].0.bytes[5], 1);
        assert_eq!(edited[4].0.bytes[5], audio.bytes[5]);
        assert_eq!(edited[4].0.bytes[6..14], audio.bytes[6..14]);
        assert_eq!(&edited[4].1, audio_body);
    }

    #[test]
    fn real_opus_cannot_emit_two_bos_pages() {
        let fixture = include_bytes!("../../../../engine/tests/fixtures/sine44_a.opus");
        let original = pages(fixture);
        let (ident, ident_body) = &original[0];
        let (comments, comment_body) = &original[1];
        let mut laces = ident.laces().to_vec();
        laces.extend(comments.laces());
        let mut body = ident_body.clone();
        body.extend(comment_body);
        let mut input = Vec::new();
        emit(ident.bytes, &laces, &body, 0, &mut input).unwrap();
        let mut output = Vec::new();
        assert!(rewrite_header(&mut input.as_slice(), &mut output, comment_body).is_err());
        assert!(output.is_empty());
        let mut input = fixture.as_slice();
        let longer = vec![b'X'; 65_026];
        let (offset, shared_payload) = rewrite_header(&mut input, &mut output, &longer).unwrap();
        assert_eq!(shared_payload, 0);
        shift(&mut input, &mut output, offset).unwrap();
        let edited = pages(&output);
        assert_eq!(
            edited.iter().filter(|(h, _)| h.bytes[5] & 2 != 0).count(),
            1
        );
        assert_eq!(edited[2].0.bytes[5], 1);
        let actual = edited.last().unwrap();
        let expected = original.last().unwrap();
        assert_eq!(actual.0.sequence(), expected.0.sequence() + 1);
        assert_eq!(actual.0.bytes[6..14], expected.0.bytes[6..14]);
        assert_eq!(actual.1, expected.1);
    }
}

#[cfg(test)]
#[test]
fn preview_rejects_corrupt_or_chained_interior_pages() {
    let samples = include_bytes!("../../../../engine/tests/fixtures/sine44_a.opus");
    let first = Header::read(&mut &samples[..]).unwrap().unwrap();
    let second = 27 + first.laces().len() + body_size(first.laces());
    let file = tempfile::NamedTempFile::new().unwrap();
    for (index, mask) in [
        (second, 1),
        (second + 5, 2),
        (second + 14, 1),
        (second + 18, 1),
    ] {
        let mut corrupted = samples.to_vec();
        corrupted[index] ^= mask;
        std::fs::write(file.path(), corrupted).unwrap();
        assert!(classify(&mut std::fs::File::open(file.path()).unwrap()).is_err());
    }
    let chain = [samples.as_slice(), samples.as_slice()].concat();
    std::fs::write(file.path(), chain).unwrap();
    assert!(classify(&mut std::fs::File::open(file.path()).unwrap()).is_err());
}
