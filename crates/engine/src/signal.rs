//! Signal path reporting: what happens to the samples between the
//! file and the device, and whether the result can be called bit-perfect.

use crate::decode::SourceFormat;
use crate::dsp::channel_map_alters;
use crate::output::{ShareMode, StreamFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageKind {
    Resample,
    Channels,
    WordLength,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageReport {
    pub kind: StageKind,
    pub name: &'static str,
    pub alters_samples: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BitPerfect {
    Yes,
    No(Vec<String>),
    /// Nothing in our path alters the samples, but the OS mixer might.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalPath {
    pub source: SourceFormat,
    pub stages: Vec<StageReport>,
    pub output: StreamFormat,
    pub share_mode: ShareMode,
    pub bit_perfect: BitPerfect,
}

impl SignalPath {
    pub fn new(source: &SourceFormat, output: StreamFormat, share_mode: ShareMode) -> Self {
        let mut stages = Vec::new();
        if source.sample_rate != output.sample_rate {
            stages.push(StageReport {
                kind: StageKind::Resample,
                name: "resample",
                alters_samples: true,
                detail: format!("{} Hz → {} Hz", source.sample_rate, output.sample_rate),
            });
        }
        let (from, to) = (usize::from(source.channels), usize::from(output.channels));
        if from != to {
            stages.push(StageReport {
                kind: StageKind::Channels,
                name: "channels",
                alters_samples: channel_map_alters(from, to),
                detail: format!("{from} ch → {to} ch"),
            });
        }
        if let Some(bits) = source.bits_per_sample
            && bits > output.sample_type.bits()
        {
            stages.push(StageReport {
                kind: StageKind::WordLength,
                name: "word length",
                alters_samples: true,
                detail: format!("{bits} bit → {} bit", output.sample_type.bits()),
            });
        }

        let mut reasons: Vec<String> = Vec::new();
        if !source.lossless {
            reasons.push(format!("lossy source ({})", source.codec));
        }
        reasons.extend(
            stages
                .iter()
                .filter(|s| s.alters_samples)
                .map(|s| format!("{}: {}", s.name, s.detail)),
        );
        let bit_perfect = if !reasons.is_empty() {
            BitPerfect::No(reasons)
        } else if share_mode == ShareMode::Shared {
            BitPerfect::Unknown("shared mode: the OS mixer may process the signal".into())
        } else {
            BitPerfect::Yes
        };

        Self {
            source: source.clone(),
            stages,
            output,
            share_mode,
            bit_perfect,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::SampleType;

    fn source(
        codec: &'static str,
        lossless: bool,
        rate: u32,
        channels: u16,
        bits: u32,
    ) -> SourceFormat {
        SourceFormat {
            codec,
            lossless,
            sample_rate: rate,
            channels,
            bits_per_sample: Some(bits),
            frames: None,
        }
    }

    fn output(rate: u32, sample_type: SampleType) -> StreamFormat {
        StreamFormat {
            sample_rate: rate,
            channels: 2,
            sample_type,
        }
    }

    #[test]
    fn clean_lossless_path_in_shared_mode_is_unknown() {
        let path = SignalPath::new(
            &source("FLAC", true, 48_000, 2, 16),
            output(48_000, SampleType::F32),
            ShareMode::Shared,
        );
        assert!(path.stages.is_empty());
        assert!(matches!(path.bit_perfect, BitPerfect::Unknown(_)));
    }

    #[test]
    fn exclusive_clean_path_is_bit_perfect() {
        let path = SignalPath::new(
            &source("FLAC", true, 44_100, 2, 24),
            output(44_100, SampleType::I32),
            ShareMode::Exclusive,
        );
        assert_eq!(path.bit_perfect, BitPerfect::Yes);
    }

    #[test]
    fn resampling_and_lossy_sources_are_named() {
        let path = SignalPath::new(
            &source("MP3", false, 44_100, 1, 16),
            output(48_000, SampleType::F32),
            ShareMode::Exclusive,
        );
        let BitPerfect::No(reasons) = &path.bit_perfect else {
            panic!("{:?}", path.bit_perfect);
        };
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        assert!(reasons[0].contains("MP3"));
        assert!(reasons[1].contains("44100 Hz → 48000 Hz"));
        // Mono to stereo duplicates samples without changing them.
        let channels = path.stages.iter().find(|s| s.name == "channels").unwrap();
        assert!(!channels.alters_samples);
    }

    #[test]
    fn narrowing_word_length_is_reported() {
        let path = SignalPath::new(
            &source("FLAC", true, 48_000, 2, 24),
            output(48_000, SampleType::I16),
            ShareMode::Exclusive,
        );
        assert!(matches!(&path.bit_perfect, BitPerfect::No(r) if r[0].contains("24 bit → 16 bit")));
    }
}
