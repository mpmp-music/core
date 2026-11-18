//! BS.1770 / EBU R128 measurements of native decoded samples, before any DSP.
use crate::EngineError;
use ebur128::{Channel, EbuR128, Mode};

/// ReplayGain 2.0 reference loudness, in LUFS (not the broadcast -23 LUFS target).
pub const REPLAYGAIN_REFERENCE_LUFS: f64 = -18.0;

/// A measurement; the bounded histogram is retained for proper album gating.
#[derive(Debug)]
pub struct Loudness {
    pub integrated_lufs: Option<f64>,
    pub sample_peak: f64,
    statistics: Box<[u64; 1000]>,
}

impl Loudness {
    pub fn gain_db(&self) -> Option<f64> {
        self.integrated_lufs
            .map(|lufs| REPLAYGAIN_REFERENCE_LUFS - lufs)
    }
}

/// Album loudness by combining gated-block statistics, never by averaging LUFS.
pub fn album_loudness(tracks: &[&Loudness]) -> Result<Option<(f64, f64)>, EngineError> {
    if tracks.is_empty() {
        return Ok(None);
    }
    let mut counts = [0u64; 1000];
    let mut peak = 0.0f64;
    for track in tracks {
        peak = peak.max(track.sample_peak);
        for (out, count) in counts.iter_mut().zip(track.statistics.iter()) {
            *out = out
                .checked_add(*count)
                .ok_or_else(|| EngineError::Decode("loudness block count overflow".into()))?;
        }
    }
    Ok(integrated(&counts).map(|lufs| (lufs, peak)))
}

// Versioned little-endian fixed histogram, independent of the meter's private state.
// Each bin represents 0.1 LU; persist completed 400 ms blocks at 100 ms steps so
// complete albums can be regrouped without decoding unchanged tracks again.
impl Loudness {
    /// Combine persisted gated-block histograms with bounded working memory.
    pub fn merge_statistics(&mut self, other: &Self) -> Result<(), EngineError> {
        for (out, &count) in self.statistics.iter_mut().zip(other.statistics.iter()) {
            *out = out
                .checked_add(count)
                .ok_or_else(|| EngineError::Decode("loudness block count overflow".into()))?;
        }
        self.sample_peak = self.sample_peak.max(other.sample_peak);
        self.integrated_lufs = integrated(&self.statistics);
        Ok(())
    }
    pub fn statistics(&self) -> Vec<u8> {
        let mut bytes = b"R128\x01".to_vec();
        for count in self.statistics.iter() {
            bytes.extend(count.to_le_bytes());
        }
        bytes
    }
    pub fn from_statistics(bytes: &[u8], sample_peak: f64) -> Option<Self> {
        if bytes.len() != 8005
            || !bytes.starts_with(b"R128\x01")
            || !sample_peak.is_finite()
            || sample_peak < 0.0
        {
            return None;
        }
        let mut statistics = Box::new([0; 1000]);
        let mut total = 0u64;
        for (out, chunk) in statistics.iter_mut().zip(bytes[5..].as_chunks::<8>().0) {
            *out = u64::from_le_bytes(*chunk);
            total = total.checked_add(*out)?;
        }
        // More than 3000 years of 100 ms blocks indicates corrupt cache data.
        if total > 1_000_000_000_000 {
            return None;
        }
        Some(Self {
            integrated_lufs: integrated(&statistics),
            sample_peak,
            statistics,
        })
    }
}

fn energies() -> &'static [f64; 1000] {
    static VALUES: std::sync::OnceLock<[f64; 1000]> = std::sync::OnceLock::new();
    VALUES.get_or_init(|| {
        std::array::from_fn(|i| 10f64.powf((i as f64 / 10.0 - 69.95 + 0.691) / 10.0))
    })
}

fn integrated(counts: &[u64; 1000]) -> Option<f64> {
    let count: u64 = counts.iter().try_fold(0u64, |a, &b| a.checked_add(b))?;
    if count == 0 {
        return None;
    }
    let sum: f64 = counts
        .iter()
        .zip(energies())
        .map(|(&n, &e)| n as f64 * e)
        .sum();
    let gate = sum / count as f64 * 0.1;
    let (count, sum) = counts
        .iter()
        .zip(energies())
        .filter(|(_, e)| **e >= gate)
        .fold((0u64, 0.0), |(n, sum), (&c, &e)| {
            (n + c, sum + c as f64 * e)
        });
    (count > 0).then(|| ebur128::energy_to_loudness(sum / count as f64))
}

pub(crate) struct Meter {
    meter: EbuR128,
    peak: f64,
    statistics: Box<[u64; 1000]>,
    step_samples: usize,
    until_step: usize,
    steps: u64,
}

impl Meter {
    pub fn new(rate: u32, channels: &[Channel]) -> Result<Self, EngineError> {
        let mut meter =
            EbuR128::new(channels.len() as u32, rate, Mode::I | Mode::HISTOGRAM).map_err(error)?;
        meter.set_channel_map(channels).map_err(error)?;
        let step_samples = ((rate as usize + 5) / 10) * channels.len();
        Ok(Self {
            meter,
            peak: 0.0,
            statistics: Box::new([0; 1000]),
            step_samples,
            until_step: step_samples,
            steps: 0,
        })
    }
    pub fn push(&mut self, samples: &[f32]) -> Result<(), EngineError> {
        for &sample in samples {
            if !sample.is_finite() {
                return Err(EngineError::Decode(
                    "non-finite PCM in loudness scan".into(),
                ));
            }
            self.peak = self.peak.max(f64::from(sample).abs());
        }
        let mut samples = samples;
        while !samples.is_empty() {
            let take = self.until_step.min(samples.len());
            self.meter.add_frames_f32(&samples[..take]).map_err(error)?;
            self.until_step -= take;
            samples = &samples[take..];
            if self.until_step == 0 {
                self.steps += 1;
                self.until_step = self.step_samples;
                if self.steps >= 4 {
                    let lufs = self.meter.loudness_momentary().map_err(error)?;
                    if lufs.is_finite() && lufs >= -70.0 {
                        let bin = (((lufs + 70.0) * 10.0).floor() as usize).min(999);
                        self.statistics[bin] += 1;
                    }
                }
            }
        }
        Ok(())
    }
    pub fn finish(self) -> Result<Loudness, EngineError> {
        let lufs = self.meter.loudness_global().map_err(error)?;
        Ok(Loudness {
            integrated_lufs: lufs.is_finite().then_some(lufs),
            sample_peak: self.peak,
            statistics: self.statistics,
        })
    }
}

fn error(error: ebur128::Error) -> EngineError {
    EngineError::Decode(format!("loudness measurement: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn measure(channels: &[Channel], samples: &[f32]) -> Loudness {
        let mut meter = Meter::new(48_000, channels).unwrap();
        meter.push(samples).unwrap();
        meter.finish().unwrap()
    }
    #[test]
    fn exported_statistics_match_native_album_gating_for_varying_signals() {
        for rate in [11025, 44100, 48000, 96000] {
            let mut native = Vec::new();
            let mut exported = Vec::new();
            for offset in [0usize, 17, 31] {
                let samples: Vec<_> = (0..rate * 4)
                    .map(|i| {
                        let block = i as usize / (rate as usize / 10);
                        let amplitude = if (block + offset).is_multiple_of(5) {
                            0.0001
                        } else {
                            0.005 + ((block * 37 + offset * 13) % 90) as f32 / 100.0
                        };
                        amplitude * (std::f32::consts::TAU * 1000.0 * i as f32 / rate as f32).sin()
                    })
                    .collect();
                let mut reference = EbuR128::new(1, rate, Mode::I | Mode::HISTOGRAM).unwrap();
                reference.set_channel_map(&[Channel::Center]).unwrap();
                reference.add_frames_f32(&samples).unwrap();
                native.push(reference);
                let mut meter = Meter::new(rate, &[Channel::Center]).unwrap();
                // Deliberately unaligned packets exercise the independent 100 ms clock.
                for chunk in samples.chunks(137) {
                    meter.push(chunk).unwrap();
                }
                exported.push(meter.finish().unwrap());
            }
            let expected = EbuR128::loudness_global_multiple(native.iter()).unwrap();
            let refs: Vec<_> = exported.iter().collect();
            let actual = album_loudness(&refs).unwrap().unwrap().0;
            assert!(
                (actual - expected).abs() < 0.11,
                "rate {rate}: {actual} vs {expected}"
            );
        }
    }
    #[test]
    fn persisted_statistics_preserve_album_gating() {
        let mut original = Vec::new();
        for amplitude in [0.5, 0.25, 0.005, 0.0] {
            let samples: Vec<_> = (0..96_000)
                .map(|i| amplitude * (std::f32::consts::TAU * 1000.0 * i as f32 / 48_000.0).sin())
                .collect();
            original.push(measure(&[Channel::Center], &samples));
        }
        let restored: Vec<_> = original
            .iter()
            .map(|m| Loudness::from_statistics(&m.statistics(), m.sample_peak).unwrap())
            .collect();
        let a: Vec<_> = original.iter().collect();
        let b: Vec<_> = restored.iter().collect();
        assert_eq!(album_loudness(&a).unwrap(), album_loudness(&b).unwrap());
        for (a, b) in original.iter().zip(&restored) {
            if let (Some(a), Some(b)) = (a.integrated_lufs, b.integrated_lufs) {
                assert!((a - b).abs() < 0.11);
            }
        }
        assert!(Loudness::from_statistics(b"bad", 0.5).is_none());
        let mut bad = original[0].statistics();
        bad[5..13].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(Loudness::from_statistics(&bad, 0.5).is_none());
    }
    #[test]
    fn positioned_channels_lfe_peak_and_nonfinite_pcm() {
        let mono: Vec<_> = (0..48_000)
            .map(|i| 0.5 * (std::f32::consts::TAU * 1000.0 * i as f32 / 48_000.0).sin())
            .collect();
        let center = measure(&[Channel::Center], &mono);
        let surround = measure(&[Channel::LeftSurround], &mono);
        assert!(
            (surround.integrated_lufs.unwrap() - center.integrated_lufs.unwrap() - 1.5).abs()
                < 0.11
        );
        let samples: Vec<_> = mono.iter().flat_map(|&v| [v, 0.9]).collect();
        let with_lfe = measure(&[Channel::Center, Channel::Unused], &samples);
        assert_eq!(center.integrated_lufs, with_lfe.integrated_lufs);
        assert!((with_lfe.sample_peak - 0.9).abs() < 1e-6);
        for sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(
                Meter::new(48_000, &[Channel::Left])
                    .unwrap()
                    .push(&[sample])
                    .is_err()
            );
        }
    }
}
