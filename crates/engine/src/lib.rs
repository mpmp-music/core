//! Playback engine: decoding, processing and output.
//!
//! Knows nothing about libraries, playlists or platforms; platform capabilities come in
//! through [`output::OutputBackend`] and [`mpmp_media_io::SourceResolver`].

mod decode;
mod delay_guard;
mod dsp;
mod loudness;
pub mod null_output;
pub mod output;
pub mod output_task;
mod pipeline;
mod player;
mod render;
mod segment;
mod signal;
mod suspend_clock;
pub use suspend_clock::{
    MonotonicPlaybackClock, PlaybackDeadline, SuspendClock, SystemSuspendClock,
};
mod timeline;
mod waveform;

pub use decode::SourceFormat;
pub use loudness::{Loudness, REPLAYGAIN_REFERENCE_LUFS, album_loudness};
pub use null_output::{NullOutput, NullOutputHandle};
pub use player::{
    BufferStatus, Engine, EngineConfig, EngineEvent, FileAccess, FrameRange, FrozenOutput,
    PreparedPlayback, TrackRef, wait_until,
};
pub use segment::EntryId;
pub use signal::{BitPerfect, SignalPath, StageKind, StageReport};
pub use timeline::{Heard, Position};
pub use waveform::{AudioAnalysis, analyze_audio};
pub use waveform::{WAVEFORM_BUCKETS, Waveform, WaveformChannel, compute_waveform};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Source(#[from] mpmp_media_io::Error),
    #[error("unsupported media: {0}")]
    Unsupported(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error(transparent)]
    Output(#[from] output::OutputError),
}

/// Probe a staged file with playback's format registry and options, without opening
/// an output device or requiring a decoder for an already unsupported codec (e.g. Opus).
pub fn probe_audio(
    source: std::sync::Arc<dyn mpmp_media_io::ByteSource>,
    extension: Option<&str>,
) -> Result<(), EngineError> {
    let reader = decode::probe_reader_strict(source, extension)?;
    if reader
        .default_track(symphonia::core::formats::TrackType::Audio)
        .is_none()
    {
        return Err(EngineError::Unsupported("no audio track".into()));
    }
    Ok(())
}
