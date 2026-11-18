use super::*;
use crate::gain;

impl Actor {
    pub(super) fn audio_changed(&mut self) {
        self.engine.set_volume(self.master_volume());
        self.persistence.dirty = true;
        self.emit_state();
    }

    /// The renderer's master multiplier: the slider, or 0 when muted.
    pub(super) fn master_volume(&self) -> f32 {
        if self.audio.muted {
            0.0
        } else {
            gain::volume_linear(self.audio.volume)
        }
    }

    /// Re-derives the ReplayGain of what is playing and what comes next after a settings
    /// change; the engine applies it to audio not yet buffered.
    pub(super) fn regain(&mut self) {
        let live: Vec<u64> = self
            .playback
            .playing
            .as_ref()
            .map(|p| p.instance)
            .into_iter()
            .chain(self.playback.pending_next)
            .collect();
        for instance in live {
            let Some(i) = self.playback.instances.get(&instance) else {
                continue;
            };
            let gain = gain::track_gain(
                i.track.replay_gain.as_ref(),
                &self.audio.replay_gain,
                self.album_context(i.entry, &i.track),
            );
            self.engine.set_gain(
                mpmp_engine::EntryId(instance),
                gain.map_or(1.0, TrackGain::linear),
            );
            if let Some(i) = self.playback.instances.get_mut(&instance) {
                i.gain = gain;
            }
        }
    }

    pub(super) fn regain_if_smart(&mut self) {
        if self.audio.replay_gain.mode == ReplayGainMode::Smart {
            self.regain();
        }
    }

    /// The engine's signal path plus the gain stages core adds (ReplayGain, volume), with
    /// the bit-perfect verdict adjusted to match.
    pub(super) fn with_gain_stages(
        &self,
        mut path: SignalPathInfo,
        gain: Option<TrackGain>,
    ) -> SignalPathInfo {
        path.output_device_id = self.engine.output_device().map(|d| d.id.0);
        path.backend = Some(crate::contract::OutputBackend::Cpal);
        path.sample_integrity = Some(crate::contract::SampleIntegrity::Unverified);
        let mut reasons = Vec::new();
        // A known effective-precision loss takes precedence over missing transport
        // evidence. Legacy narrowing already carries word_length; this also covers
        // a wide container whose valid bits are narrower than the source.
        let valid_bits = path
            .stages
            .iter()
            .rev()
            .find_map(|stage| stage.output_format.as_ref())
            .and_then(|format| format.valid_bits_per_sample);
        if path
            .source_bits_per_sample
            .zip(valid_bits)
            .is_some_and(|(source, valid)| source > valid)
            && !path.reason_codes.contains(&SignalReasonCode::WordLength)
        {
            reasons.push("output loses effective PCM precision".to_owned());
            path.reason_codes.push(SignalReasonCode::PrecisionLoss);
        }
        // Gain is applied before the output's word length is set.
        let mut at = path
            .stages
            .iter()
            .position(|s| s.kind == SignalStageKind::WordLength)
            .unwrap_or(path.stages.len());
        if let Some(gain) = gain {
            path.stages.insert(at, gain.stage());
            at += 1;
            if gain.db != 0.0 {
                reasons.push("ReplayGain changes the level".to_owned());
                path.reason_codes.push(SignalReasonCode::ReplayGain);
            }
        }
        if self.master_volume() != 1.0 {
            let detail = match gain::volume_db(self.audio.volume) {
                _ if self.audio.muted => "muted".to_owned(),
                Some(db) => format!("{db:+.1} dB"),
                None => "silent".to_owned(),
            };
            path.stages.insert(
                at,
                StageInfo {
                    name: "volume".into(),
                    detail,
                    alters_samples: true,
                    kind: SignalStageKind::Volume,
                    gain_db: if self.master_volume() == 0.0 {
                        None
                    } else {
                        gain::volume_db(self.audio.volume)
                    },
                    silent: Some(self.master_volume() == 0.0),
                    ..Default::default()
                },
            );
            reasons.push("volume is below 100 %".to_owned());
            path.reason_codes.push(SignalReasonCode::Volume);
        }
        if !reasons.is_empty() {
            path.reason_codes
                .retain(|code| *code != SignalReasonCode::SharedMixer);
            path.bit_perfect = match path.bit_perfect {
                BitPerfectInfo::No {
                    reasons: mut before,
                } => {
                    before.extend(reasons);
                    BitPerfectInfo::No { reasons: before }
                }
                _ => BitPerfectInfo::No { reasons },
            };
        }
        if matches!(path.bit_perfect, BitPerfectInfo::Yes)
            && path.sample_integrity != Some(crate::contract::SampleIntegrity::Verified)
        {
            path.bit_perfect = BitPerfectInfo::Uncertain {
                reason: "endpoint driver transport is not verified".into(),
            };
            path.reason_codes.push(SignalReasonCode::PathUnverified);
        }
        path
    }
}
