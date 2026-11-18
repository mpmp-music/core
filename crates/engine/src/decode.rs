//! Symphonia behind a small interface; symphonia types do not leave this module.

use std::sync::Arc;

use mpmp_media_io::{ByteSource, ByteSourceReader};
use symphonia::core::audio::{ChannelLabel, Channels, Position};
use symphonia::core::codecs::audio::well_known::{
    CODEC_ID_AAC, CODEC_ID_ALAC, CODEC_ID_FLAC, CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3,
    CODEC_ID_OPUS, CODEC_ID_PCM_MULAW, CODEC_ID_PCM_S32LE, CODEC_ID_VORBIS, CODEC_ID_WAVPACK,
};
use symphonia::core::codecs::audio::{AudioCodecId, AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::{Error as SymphoniaError, SeekErrorKind};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{TimeBase, Timestamp};

use crate::EngineError;

/// Properties of the decoded stream, as far as the container tells us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFormat {
    /// Short codec name, e.g. "FLAC", "MP3", "PCM".
    pub codec: &'static str,
    pub lossless: bool,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: Option<u32>,
    /// Total frames after gapless trimming, when the container knows it.
    pub frames: Option<u64>,
}

/// Outcome of [`Decoder::seek`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seek {
    Landed(u64),
    PastEnd,
}

/// One decoded packet: interleaved f32 in the caller's buffer, plus where it starts.
pub(crate) struct Chunk {
    /// Track frame of the first sample, when the container provides timestamps.
    pub start_frame: Option<i64>,
}

/// Where a channel of the stream sits, as far as a stereo down-mix cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    /// The front-left channel itself.
    FrontLeft,
    /// Any other channel on the left (surround, wide, height …).
    Left,
    FrontRight,
    Right,
    /// Centre channels (front, rear, top), and channels of no known position.
    Center,
    /// Low-frequency effects: left out of a down-mix.
    Lfe,
}

pub(crate) struct Decoder {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    codec: AudioCodecId,
    format: SourceFormat,
    /// Each channel's side, in buffer order.
    sides: Vec<Side>,
    loudness_channels: Option<Vec<ebur128::Channel>>,
    pub analysis_incomplete: bool,
    // Declared last: all decoder/source fields close before the access lease is dropped.
    pub file_lease: Option<Box<dyn Send>>,
}

/// Playback tolerates malformed historical ID3 tags using the pre-ID3 format probe.
/// File-write validation calls the strict entry point and never uses this fallback.
pub(crate) fn probe_reader(
    source: Arc<dyn ByteSource>,
    extension: Option<&str>,
) -> Result<Box<dyn FormatReader>, EngineError> {
    match probe_reader_strict(source.clone(), extension) {
        Ok(reader) => Ok(reader),
        Err(_) => probe_with(source, extension, format_probe()),
    }
}

macro_rules! fallback_formats {
    ($consumer:ident) => {
        $consumer! {
        AdtsReader=>"aac",
        FlacReader=>"flac",
        IsoMp4Reader=>"isomp4",
        MpaReader=>"mpa",
        AiffReader=>"aiff",
        WavReader=>"wav",
        OggReader=>"ogg"}
    };
}

fn format_probe() -> &'static symphonia::core::formats::probe::Probe {
    use symphonia::default::formats::*;
    static PROBE: std::sync::OnceLock<symphonia::core::formats::probe::Probe> =
        std::sync::OnceLock::new();
    PROBE.get_or_init(|| {
        let mut probe = symphonia::core::formats::probe::Probe::default();
        macro_rules! register {
        ($($reader:ident=>$feature:literal),
        *)=>{
        $(probe.register_format::<$reader<'_>>();
        )*}
        ;
        }

        fallback_formats!(register);
        probe
    })
}

pub(crate) fn probe_reader_strict(
    source: Arc<dyn ByteSource>,
    extension: Option<&str>,
) -> Result<Box<dyn FormatReader>, EngineError> {
    let mut header = [0; 10];
    if source.read_at(0, &mut header).ok() == Some(10) && &header[..3] == b"ID3" {
        if header[6..10].iter().any(|b| b & 0x80 != 0) {
            return Err(EngineError::Unsupported("invalid ID3 size".into()));
        }
        let size = header[6..10]
            .iter()
            .fold(0u64, |n, b| (n << 7) | u64::from(*b));
        let end = 10
            + size
            + if header[3] == 4 && header[5] & 0x10 != 0 {
                10
            } else {
                0
            };
        let mut sync = [0; 2];
        if source.read_at(end, &mut sync).ok() != Some(2)
            || sync[0] != 0xff
            || sync[1] & 0xe0 != 0xe0
            || sync[1] & 6 == 0
        {
            return Err(EngineError::Unsupported(
                "ID3 does not end at MPEG audio".into(),
            ));
        }
    }
    probe_with(source, extension, symphonia::default::get_probe())
}

fn probe_with(
    source: Arc<dyn ByteSource>,
    extension: Option<&str>,
    probe: &symphonia::core::formats::probe::Probe,
) -> Result<Box<dyn FormatReader>, EngineError> {
    let stream = MediaSourceStream::new(Box::new(SymphoniaSource::new(source)), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = extension {
        hint.with_extension(ext);
    }
    // Readers that honor this option need no artwork for audio decoding. Symphonia
    // 0.6.1's ID3 reader currently ignores MetadataOptions (documented limitation).
    probe
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default()
                .limit_visual_bytes(symphonia::core::common::Limit::Maximum(0)),
        )
        .map_err(unsupported)
}

impl Decoder {
    pub fn open(source: Arc<dyn ByteSource>, extension: Option<&str>) -> Result<Self, EngineError> {
        Self::from_reader(probe_reader(source, extension)?)
    }
    fn from_reader(reader: Box<dyn FormatReader>) -> Result<Self, EngineError> {
        let track = reader
            .default_track(TrackType::Audio)
            .ok_or_else(|| EngineError::Unsupported("no audio track".into()))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| EngineError::Unsupported("missing audio codec parameters".into()))?;
        let track_id = track.id;
        let frames = track.num_frames;
        let time_base = track.time_base;

        // `gapless: true` is the default; spelled out because the frame trimmer relies on it.
        let mut options = AudioDecoderOptions::default();
        options.gapless = true;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &options)
            .map_err(unsupported)?;

        let sample_rate = params
            .sample_rate
            .ok_or_else(|| EngineError::Unsupported("unknown sample rate".into()))?;
        let layout = params
            .channels
            .as_ref()
            .ok_or_else(|| EngineError::Unsupported("unknown channel layout".into()))?;
        let channels = layout.count() as u16;
        let sides = sides_of(layout);
        let loudness_channels = loudness_channels_of(layout);
        let codec_id = params.codec;
        let (codec, lossless) = describe(codec_id);
        let format = SourceFormat {
            codec,
            lossless,
            sample_rate,
            channels,
            bits_per_sample: params.bits_per_sample,
            frames,
        };

        Ok(Self {
            reader,
            decoder,
            track_id,
            time_base,
            codec: codec_id,
            format,
            sides,
            loudness_channels,
            analysis_incomplete: false,
            file_lease: None,
        })
    }

    pub fn format(&self) -> &SourceFormat {
        &self.format
    }

    /// BS.1770 weights in source buffer order, when every position is understood.
    pub fn loudness_channels(&self) -> Option<&[ebur128::Channel]> {
        self.loudness_channels.as_deref()
    }

    /// Coarse positions for waveform downmix, in source buffer order.
    pub fn sides(&self) -> &[Side] {
        &self.sides
    }

    /// Decodes the next packet of the selected track, replacing `out` with interleaved f32
    /// samples. Returns `Ok(None)` at end of stream.
    pub fn next_chunk(&mut self, out: &mut Vec<f32>) -> Result<Option<Chunk>, EngineError> {
        loop {
            let packet = match self.reader.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => return Ok(None),
                // Chained Ogg streams: rebuilding the decoder mid-stream is later work.
                Err(SymphoniaError::ResetRequired) => {
                    self.analysis_incomplete = true;
                    return Ok(None);
                }
                Err(err) => return Err(EngineError::Decode(err.to_string())),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            // PTS covers the whole decoded packet, including any delay frames the decoder
            // trims; the first frame we receive is `trim_start` later (symphonia-core
            // packet.rs: delay frames carry negative PTS).
            let start_frame = self
                .ts_to_frames(packet.pts)
                .map(|pts| pts + self.ts_to_frames_dur(packet.trim_start.get()));
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    let channels = buf.spec().channels().count();
                    if channels != usize::from(self.format.channels) {
                        return Err(EngineError::Decode(format!(
                            "channel count changed mid-stream ({} -> {channels})",
                            self.format.channels
                        )));
                    }
                    out.resize(buf.samples_interleaved(), 0.0);
                    buf.copy_to_slice_interleaved(&mut out[..]);
                    if !out.is_empty() {
                        return Ok(Some(Chunk { start_frame }));
                    }
                }
                // A corrupt packet: skip it, as every mainstream player does.
                Err(SymphoniaError::DecodeError(err)) => {
                    self.analysis_incomplete = true;
                    tracing::warn!("skipping undecodable packet: {err}");
                }
                Err(err) => return Err(EngineError::Decode(err.to_string())),
            }
        }
    }

    /// Frames to decode and discard before a seek target so that the decoder's internal
    /// state has settled: MDCT overlap, the MP3 bit reservoir, Opus pre-roll.
    pub fn preroll_frames(&self) -> u64 {
        match self.codec {
            CODEC_ID_MP3 | CODEC_ID_MP2 | CODEC_ID_MP1 => 2 * 1152,
            CODEC_ID_AAC => 2 * 1024,
            CODEC_ID_VORBIS => 2 * 2048,
            CODEC_ID_OPUS => u64::from(self.format.sample_rate) * 80 / 1000,
            _ => 0,
        }
    }

    /// Seeks to at most `frame` (track frames). Returns the frame actually reached, or
    /// `Seek::PastEnd` when `frame` is at or beyond the end of the track, which the
    /// caller treats like reaching the end rather than as an error. The caller discards
    /// frames up to the target using the start frames of the following chunks (or
    /// counting from the returned frame without timestamps).
    pub fn seek(&mut self, frame: u64) -> Result<Seek, EngineError> {
        if self.format.frames.is_some_and(|total| frame >= total) {
            return Ok(Seek::PastEnd);
        }
        let target = frame.saturating_sub(self.preroll_frames());
        let ts = self.frames_to_ts(target);
        let seeked = match self.reader.seek(
            SeekMode::Accurate,
            SeekTo::Timestamp {
                ts: Timestamp::new(ts),
                track_id: self.track_id,
            },
        ) {
            Ok(seeked) => seeked,
            Err(SymphoniaError::SeekError(SeekErrorKind::OutOfRange)) => return Ok(Seek::PastEnd),
            Err(err) => return Err(EngineError::Decode(format!("seek failed: {err}"))),
        };
        self.decoder.reset();
        let landed = self.ts_to_frames(seeked.actual_ts).unwrap_or(target as i64);
        Ok(Seek::Landed(landed.max(0) as u64))
    }

    fn ts_to_frames(&self, ts: Timestamp) -> Option<i64> {
        let tb = self.time_base?;
        Some(self.scale_ts(ts.get(), tb))
    }

    fn ts_to_frames_dur(&self, dur: u64) -> i64 {
        match self.time_base {
            Some(tb) => self.scale_ts(dur as i64, tb),
            None => dur as i64,
        }
    }

    fn scale_ts(&self, ts: i64, tb: TimeBase) -> i64 {
        let rate = i128::from(self.format.sample_rate);
        let num = i128::from(tb.numer.get());
        let den = i128::from(tb.denom.get());
        (i128::from(ts) * num * rate / den) as i64
    }

    fn frames_to_ts(&self, frames: u64) -> i64 {
        match self.time_base {
            Some(tb) => {
                let rate = i128::from(self.format.sample_rate);
                let num = i128::from(tb.numer.get());
                let den = i128::from(tb.denom.get());
                (i128::from(frames) * den / (num * rate)) as i64
            }
            None => frames as i64,
        }
    }
}

/// The side of each channel of `layout`, in buffer order (the order of the position bits).
/// Channels without positions are taken in the standard WAVE order (front left, front
/// right, centre, LFE, rear left, rear right …), which is what most files with more than two
/// unlabelled channels use.
fn sides_of(layout: &Channels) -> Vec<Side> {
    let positions = |mask: Position| -> Vec<Side> {
        (0..u64::BITS)
            .map(|bit| 1u64 << bit)
            .filter(|bit| mask.bits() & bit != 0)
            .map(|bit| side_of(Position::from_bits_retain(bit)))
            .collect()
    };
    match layout {
        Channels::Positioned(mask) => positions(*mask),
        Channels::Custom(labels) => labels
            .iter()
            .map(|label| match label {
                ChannelLabel::Positioned(position) => side_of(*position),
                _ => Side::Center,
            })
            .collect(),
        other => {
            let count = other.count();
            let mut sides = Position::from_count(count.min(18) as u32)
                .map(positions)
                .unwrap_or_default();
            sides.resize(count, Side::Center);
            sides
        }
    }
}

/// Only map positions whose BS.1770 weighting is unambiguous. Do not reuse the
/// coarse waveform downmix roles (wide/height channels have different meanings).
fn loudness_channels_of(layout: &Channels) -> Option<Vec<ebur128::Channel>> {
    use ebur128::Channel;
    let map = |p: Position| -> Option<Channel> {
        if p == Position::FRONT_LEFT {
            Some(Channel::Left)
        } else if p == Position::FRONT_RIGHT {
            Some(Channel::Right)
        } else if p == Position::FRONT_CENTER {
            Some(Channel::Center)
        } else if p == Position::SIDE_LEFT || p == Position::REAR_LEFT {
            Some(Channel::LeftSurround)
        } else if p == Position::SIDE_RIGHT || p == Position::REAR_RIGHT {
            Some(Channel::RightSurround)
        } else if p == Position::LFE1 || p == Position::LFE2 {
            Some(Channel::Unused)
        } else {
            None
        }
    };
    match layout {
        Channels::Positioned(mask) => (0..64)
            .filter_map(|i| {
                let bit = 1u64 << i;
                (mask.bits() & bit != 0).then(|| Position::from_bits_retain(bit))
            })
            .map(map)
            .collect(),
        Channels::Custom(labels) => labels
            .iter()
            .map(|label| match label {
                ChannelLabel::Positioned(p) => map(*p),
                _ => None,
            })
            .collect(),
        other => Position::from_count(other.count() as u32)
            .and_then(|mask| loudness_channels_of(&Channels::Positioned(mask))),
    }
}

fn side_of(position: Position) -> Side {
    const LEFT: Position = Position::REAR_LEFT
        .union(Position::FRONT_LEFT_CENTER)
        .union(Position::SIDE_LEFT)
        .union(Position::TOP_FRONT_LEFT)
        .union(Position::TOP_REAR_LEFT)
        .union(Position::TOP_SIDE_LEFT)
        .union(Position::BOTTOM_FRONT_LEFT)
        .union(Position::FRONT_LEFT_WIDE);
    const RIGHT: Position = Position::REAR_RIGHT
        .union(Position::FRONT_RIGHT_CENTER)
        .union(Position::SIDE_RIGHT)
        .union(Position::TOP_FRONT_RIGHT)
        .union(Position::TOP_REAR_RIGHT)
        .union(Position::TOP_SIDE_RIGHT)
        .union(Position::BOTTOM_FRONT_RIGHT)
        .union(Position::FRONT_RIGHT_WIDE);
    if position == Position::FRONT_LEFT {
        Side::FrontLeft
    } else if position == Position::FRONT_RIGHT {
        Side::FrontRight
    } else if position.intersects(Position::LFE1.union(Position::LFE2)) {
        Side::Lfe
    } else if position.intersects(LEFT) {
        Side::Left
    } else if position.intersects(RIGHT) {
        Side::Right
    } else {
        Side::Center
    }
}

fn describe(codec: AudioCodecId) -> (&'static str, bool) {
    match codec {
        CODEC_ID_FLAC => ("FLAC", true),
        CODEC_ID_ALAC => ("ALAC", true),
        CODEC_ID_WAVPACK => ("WavPack", true),
        CODEC_ID_MP3 => ("MP3", false),
        CODEC_ID_MP2 => ("MP2", false),
        CODEC_ID_MP1 => ("MP1", false),
        CODEC_ID_AAC => ("AAC", false),
        CODEC_ID_VORBIS => ("Vorbis", false),
        CODEC_ID_OPUS => ("Opus", false),
        // symphonia numbers its PCM variants contiguously from S32LE to MULAW. Anything
        // unrecognised counts as lossy, so it can never be reported as bit-perfect.
        other if (CODEC_ID_PCM_S32LE..=CODEC_ID_PCM_MULAW).contains(&other) => ("PCM", true),
        _ => ("unknown", false),
    }
}

fn unsupported(err: SymphoniaError) -> EngineError {
    EngineError::Unsupported(err.to_string())
}

/// `mpmp_media_io::ByteSource` seen through symphonia's `MediaSource`.
struct SymphoniaSource {
    reader: ByteSourceReader,
    seekable: bool,
    len: Option<u64>,
}

impl SymphoniaSource {
    fn new(source: Arc<dyn ByteSource>) -> Self {
        let seekable = source.is_seekable();
        let len = source.len();
        Self {
            reader: ByteSourceReader::new(source),
            seekable,
            len,
        }
    }
}

impl std::io::Read for SymphoniaSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}

impl std::io::Seek for SymphoniaSource {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.reader.seek(pos)
    }
}

impl MediaSource for SymphoniaSource {
    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

#[cfg(test)]
mod id3_compatibility_tests {
    use super::*;
    fn decoded(mut decoder: Decoder) -> (u64, Vec<u32>) {
        let mut frames = 0;
        let mut samples = Vec::new();
        let mut chunk = Vec::new();
        while decoder.next_chunk(&mut chunk).unwrap().is_some() {
            frames += chunk.len() as u64 / u64::from(decoder.format.channels);
            samples.extend(chunk.iter().map(|s| s.to_bits()));
        }
        (frames, samples)
    }
    #[test]
    fn fallback_format_registry_matches_workspace_symphonia_features() {
        use std::collections::BTreeSet;
        macro_rules! features {
        ($($reader:ident=>$feature:literal),
        *)=>{
        BTreeSet::from([$($feature),
        *])}
        ;
        }

        let registered = fallback_formats!(features);
        let manifest = include_str!("../../../Cargo.toml");
        let dependency = manifest
            .split("symphonia = {")
            .nth(1)
            .unwrap()
            .split('}')
            .next()
            .unwrap();
        assert!(
            dependency.contains("default-features = false"),
            "Account for default formats before enabling them"
        );
        let features = dependency
            .split("features = [")
            .nth(1)
            .unwrap()
            .split(']')
            .next()
            .unwrap();
        let expected = features
            .split(',')
            .map(|s| s.trim().trim_matches('"'))
            .filter_map(|feature| match feature {
                "id3v2" | "id3v1" | "ape" | "vorbis" | "pcm" | "alac" => None,
                "mp1" | "mp2" | "mp3" => Some("mpa"),
                other => Some(other),
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            registered, expected,
            "Update fallback registration when workspace enables formats"
        );
    }
    #[test]
    fn malformed_id3_playback_matches_old_format_only_probe_but_strict_writes_refuse_it() {
        let fixture = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sine44_a.mp3"),
        )
        .unwrap();
        let skip = if &fixture[..3] == b"ID3" {
            10 + fixture[6..10]
                .iter()
                .fold(0usize, |n, b| (n << 7) | usize::from(*b))
        } else {
            0
        };
        let audio = &fixture[skip..];
        for case in 0..6 {
            let mut header = *b"ID3\x03\x00\x00\x00\x00\x00\x64";
            let mut padding = vec![0; 100];
            match case {
                0 => header[3] = 5,
                1 => header[3] = 255,
                2 => {
                    header[3] = 2;
                    header[5] = 0x40;
                }
                3 => {
                    header[5] = 0x40;
                    padding[..4].copy_from_slice(&1u32.to_be_bytes());
                }
                4 => header[6..10].copy_from_slice(&[0x7f; 4]),
                5 => {
                    let size = 3100u32;
                    for i in 0..4 {
                        header[6 + i] = ((size >> (7 * (3 - i))) & 127) as u8;
                    }
                }
                _ => unreachable!(),
            }
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("malformed.mp3");
            std::fs::write(&path, [header.as_slice(), &padding, audio].concat()).unwrap();
            let source: Arc<dyn ByteSource> =
                Arc::new(mpmp_media_io::FileSource::open(&path).unwrap());
            let old = Decoder::from_reader(
                probe_with(source.clone(), Some("mp3"), format_probe()).unwrap(),
            )
            .unwrap();
            let current = Decoder::open(source.clone(), Some("mp3")).unwrap();
            let expected = decoded(old);
            assert!(expected.0 > 0);
            assert_eq!(decoded(current), expected, "malformed case {case}");
            assert!(
                probe_reader_strict(source, Some("mp3")).is_err(),
                "strict case {case}"
            );
        }
    }
}
