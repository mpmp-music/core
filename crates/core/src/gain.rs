//! Volume and ReplayGain arithmetic (docs/design/loudness-volume.md): which gain a
//! track gets, and how the slider maps to a multiplier.

use crate::contract::{
    GainDataSource, GainScope, ReplayGainInfo, ReplayGainMode, ReplayGainSettings, SignalStageKind,
    StageInfo,
};

/// The selected ReplayGain, including unity so its source can still be explained.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TrackGain {
    pub db: f32,
    pub source: GainSource,
    pub provenance: GainDataSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GainSource {
    Track,
    Album,
    /// No usable ReplayGain value in file tags or the computed cache: fallback pre-amp.
    Untagged,
}

impl TrackGain {
    pub fn linear(self) -> f32 {
        db_to_linear(self.db)
    }

    pub fn stage(self) -> StageInfo {
        let source = match self.source {
            GainSource::Track => "track",
            GainSource::Album => "album",
            GainSource::Untagged => "no RG data; fallback",
        };
        let adjustment = if self.db == 0.0 {
            " (no adjustment)"
        } else {
            ""
        };
        StageInfo {
            name: "replay gain".into(),
            detail: format!("{source} {:+.2} dB{adjustment}", self.db),
            alters_samples: self.db != 0.0,
            kind: SignalStageKind::ReplayGain,
            gain_db: Some(self.db),
            gain_source: Some(self.provenance),
            gain_scope: match self.source {
                GainSource::Track => Some(GainScope::Track),
                GainSource::Album => Some(GainScope::Album),
                GainSource::Untagged => None,
            },
            ..Default::default()
        }
    }
}

/// The selected gain, including 0 dB; `None` when off or the result is non-finite.
/// Unity still maps to multiplier 1.0 and does not alter the engine's sample path.
/// `album_context`: whether the smart mode would use album gain for it.
pub(crate) fn track_gain(
    tags: Option<&ReplayGainInfo>,
    settings: &ReplayGainSettings,
    album_context: bool,
) -> Option<TrackGain> {
    let prefer_album = match settings.mode {
        // Unknown never gets this far (refused as a command); untouched to be safe.
        ReplayGainMode::Off | ReplayGainMode::Unknown => return None,
        ReplayGainMode::Track => false,
        ReplayGainMode::Album => true,
        ReplayGainMode::Smart => album_context,
    };
    let tags = tags.copied().unwrap_or_default();
    let track = tags
        .track_gain_db
        .map(|g| (g, tags.track_peak, GainSource::Track, tags.track_source));
    let album = tags
        .album_gain_db
        .map(|g| (g, tags.album_peak, GainSource::Album, tags.album_source));
    let picked = if prefer_album {
        album.or(track)
    } else {
        track.or(album)
    };
    let (mut db, peak, source, provenance) = match picked {
        Some((gain, peak, source, provenance)) => {
            (gain + settings.preamp_db, peak, source, provenance)
        }
        None => (
            settings.fallback_preamp_db,
            None,
            GainSource::Untagged,
            GainDataSource::Fallback,
        ),
    };
    if settings.prevent_clipping
        && let Some(peak) = peak.filter(|p| p.is_finite() && *p > 0.0)
    {
        db = db.min(-20.0 * peak.log10());
    }
    if !db.is_finite() {
        return None;
    }
    Some(TrackGain {
        db,
        source,
        provenance,
    })
}

/// Slider position (0.0–1.0) → multiplier: `volume³`, so equal slider steps sound about
/// equally loud. 0.5 is about -18 dB.
pub(crate) fn volume_linear(volume: f32) -> f32 {
    let v = volume.clamp(0.0, 1.0);
    v * v * v
}

/// The slider position in dB; `None` at 0 (silence).
pub(crate) fn volume_db(volume: f32) -> Option<f32> {
    let linear = volume_linear(volume);
    (linear > 0.0).then(|| 20.0 * linear.log10())
}

pub(crate) fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags() -> ReplayGainInfo {
        ReplayGainInfo {
            track_gain_db: Some(-7.0),
            track_peak: Some(0.9),
            album_gain_db: Some(-5.0),
            album_peak: Some(0.99),
            track_source: GainDataSource::Tags,
            album_source: GainDataSource::Tags,
        }
    }

    fn settings(mode: ReplayGainMode) -> ReplayGainSettings {
        ReplayGainSettings {
            mode,
            ..Default::default()
        }
    }

    #[test]
    fn modes_pick_the_right_gain() {
        let t = tags();
        assert_eq!(
            track_gain(Some(&t), &settings(ReplayGainMode::Off), true),
            None
        );
        let g = track_gain(Some(&t), &settings(ReplayGainMode::Track), true).unwrap();
        assert_eq!((g.db, g.source), (-7.0, GainSource::Track));
        let g = track_gain(Some(&t), &settings(ReplayGainMode::Album), false).unwrap();
        assert_eq!((g.db, g.source), (-5.0, GainSource::Album));
        let smart = settings(ReplayGainMode::Smart);
        assert_eq!(
            track_gain(Some(&t), &smart, true).unwrap().source,
            GainSource::Album
        );
        assert_eq!(
            track_gain(Some(&t), &smart, false).unwrap().source,
            GainSource::Track
        );
    }

    #[test]
    fn missing_gain_falls_back_to_the_other_then_to_the_preamp() {
        let only_track = ReplayGainInfo {
            album_gain_db: None,
            album_peak: None,
            ..tags()
        };
        let g = track_gain(Some(&only_track), &settings(ReplayGainMode::Album), true).unwrap();
        assert_eq!(g.source, GainSource::Track);
        let untagged = ReplayGainSettings {
            fallback_preamp_db: -6.0,
            ..settings(ReplayGainMode::Track)
        };
        let g = track_gain(None, &untagged, false).unwrap();
        assert_eq!((g.db, g.source), (-6.0, GainSource::Untagged));
        // No data, no fallback: still explained, with unity and bit-perfect possible.
        let unity = track_gain(None, &settings(ReplayGainMode::Track), false).unwrap();
        assert_eq!(unity.linear(), 1.0);
        assert_eq!(unity.source, GainSource::Untagged);
        assert!(!unity.stage().alters_samples);
        assert!(
            unity
                .stage()
                .detail
                .contains("no RG data; fallback +0.00 dB")
        );
    }

    #[test]
    fn peaks_cap_the_gain_and_preamp_adds() {
        let loud = ReplayGainInfo {
            track_gain_db: Some(3.0),
            track_peak: Some(0.8),
            album_gain_db: None,
            album_peak: None,
            track_source: GainDataSource::Tags,
            album_source: GainDataSource::Tags,
        };
        let s = ReplayGainSettings {
            preamp_db: 2.0,
            ..settings(ReplayGainMode::Track)
        };
        let capped = track_gain(Some(&loud), &s, false).unwrap();
        // 0.8 peak allows +1.94 dB.
        assert!((capped.db - 1.938).abs() < 0.01, "{capped:?}");
        assert!(capped.linear() * 0.8 <= 1.0 + 1e-6);
        let unprotected = ReplayGainSettings {
            prevent_clipping: false,
            ..s
        };
        assert_eq!(
            track_gain(Some(&loud), &unprotected, false).unwrap().db,
            5.0
        );
    }

    #[test]
    fn zero_gain_keeps_its_source_after_preamp_or_clipping() {
        // Zero can mean measured unity, preamp cancelling a gain, or peak protection
        // limiting a boost. None of these means that RG data is missing.
        for (mode, track, album, preamp, peak, expected) in [
            (
                ReplayGainMode::Track,
                Some(0.0),
                None,
                0.0,
                0.5,
                GainSource::Track,
            ),
            (
                ReplayGainMode::Album,
                None,
                Some(0.0),
                0.0,
                0.5,
                GainSource::Album,
            ),
            (
                ReplayGainMode::Album,
                Some(0.0),
                None,
                0.0,
                0.5,
                GainSource::Track,
            ),
            (
                ReplayGainMode::Track,
                Some(-3.0),
                None,
                3.0,
                0.5,
                GainSource::Track,
            ),
            (
                ReplayGainMode::Track,
                Some(3.0),
                None,
                0.0,
                1.0,
                GainSource::Track,
            ),
        ] {
            let tags = ReplayGainInfo {
                track_gain_db: track,
                album_gain_db: album,
                track_peak: Some(peak),
                album_peak: Some(peak),
                track_source: GainDataSource::Tags,
                album_source: GainDataSource::Tags,
            };
            let settings = ReplayGainSettings {
                preamp_db: preamp,
                ..settings(mode)
            };
            let gain = track_gain(Some(&tags), &settings, false).unwrap();
            assert_eq!((gain.db, gain.source), (0.0, expected));
            assert_eq!(gain.linear(), 1.0);
            let stage = gain.stage();
            assert!(!stage.alters_samples);
            assert!(stage.detail.contains("no adjustment"));
            assert!(!stage.detail.contains("no RG data"));
        }
    }

    #[test]
    fn volume_curve() {
        assert_eq!(volume_linear(1.0), 1.0);
        assert_eq!(volume_linear(0.0), 0.0);
        assert_eq!(volume_linear(2.0), 1.0);
        assert_eq!(volume_db(1.0), Some(0.0));
        assert_eq!(volume_db(0.0), None);
        assert!((volume_db(0.5).unwrap() + 18.06).abs() < 0.01);
    }
}
