use super::super::*;
use crate::contract::*;

impl Actor {
    pub(in crate::actor) fn command_set_output_mode(
        &mut self,
        mode: Option<OutputShareMode>,
        fallback: Option<OutputFallbackPolicy>,
        rate_policy: Option<OutputRatePolicy>,
        expected_revision: Option<u64>,
    ) -> Result<bool, ContractError> {
        let changed = self.set_output_mode(mode, fallback, rate_policy, expected_revision)?;
        Ok(changed)
    }

    pub(in crate::actor) fn command_set_stop_after_current(
        &mut self,
        enabled: bool,
        expected_entry: Option<EntryId>,
    ) -> Result<bool, ContractError> {
        let changed = self.set_stop_after_current(enabled, expected_entry)?;
        Ok(changed)
    }

    pub(in crate::actor) fn command_set_sleep_timer(
        &mut self,
        duration_ms: Option<u64>,
    ) -> Result<bool, ContractError> {
        let changed = {
            if duration_ms.is_some_and(|d| d == 0 || d > MAX_SLEEP_TIMER_MS) {
                return Err(ContractError::invalid(
                    ErrorCode::OutOfRange,
                    "sleep duration must be 1..=86400000 ms",
                )
                .with_field("command.duration_ms"));
            }
            let deadline =
                duration_ms.map(|d| self.limits.playback_clock.now_ms().saturating_add(d));
            let changed = self.limits.sleep_deadline != deadline;
            self.limits.sleep_deadline = deadline;
            self.limits.playback_deadline.set(deadline);
            if changed {
                self.emit_state();
            }
            changed
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_set_volume(
        &mut self,
        volume: f32,
    ) -> Result<bool, ContractError> {
        let changed = {
            if !(0.0..=1.0).contains(&volume) {
                return Err(ContractError::invalid(
                    ErrorCode::OutOfRange,
                    "volume must be 0.0 to 1.0",
                )
                .with_field("command.volume"));
            }
            let changed = volume != self.audio.volume;
            if changed {
                self.audio.volume = volume;
                self.audio_changed();
            }
            changed
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_set_muted(
        &mut self,
        muted: bool,
    ) -> Result<bool, ContractError> {
        let changed = {
            let changed = muted != self.audio.muted;
            if changed {
                self.audio.muted = muted;
                self.audio_changed();
            }
            changed
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_set_volume_mode(
        &mut self,
        mode: VolumeMode,
    ) -> Result<bool, ContractError> {
        let changed = match mode {
            VolumeMode::Software => false,
            VolumeMode::System => {
                return Err(ContractError::unsupported(
                    ErrorCode::NotImplemented,
                    "system volume is not available yet",
                )
                .with_field("command.mode"));
            }
            _ => {
                return Err(
                    ContractError::unknown_variant("volume mode").with_field("command.mode")
                );
            }
        };
        Ok(changed)
    }

    pub(in crate::actor) fn command_set_replay_gain(
        &mut self,
        mode: Option<ReplayGainMode>,
        preamp_db: Option<f32>,
        fallback_preamp_db: Option<f32>,
        prevent_clipping: Option<bool>,
    ) -> Result<bool, ContractError> {
        let changed = {
            let mut settings = self.audio.replay_gain;
            if let Some(mode) = mode {
                if mode == ReplayGainMode::Unknown {
                    return Err(ContractError::unknown_variant("ReplayGain mode")
                        .with_field("command.mode"));
                }
                settings.mode = mode;
            }
            for (name, value, target) in [
                ("preamp_db", preamp_db, &mut settings.preamp_db),
                (
                    "fallback_preamp_db",
                    fallback_preamp_db,
                    &mut settings.fallback_preamp_db,
                ),
            ] {
                if let Some(db) = value {
                    if !(-24.0..=24.0).contains(&db) {
                        return Err(ContractError::invalid(
                            ErrorCode::OutOfRange,
                            format!("{name} must be -24 to 24 dB"),
                        )
                        .with_field(format!("command.{name}")));
                    }
                    *target = db;
                }
            }
            if let Some(on) = prevent_clipping {
                settings.prevent_clipping = on;
            }
            let changed = settings != self.audio.replay_gain;
            if changed {
                self.audio.replay_gain = settings;
                self.regain();
                self.audio_changed();
            }
            changed
        };
        Ok(changed)
    }
}
