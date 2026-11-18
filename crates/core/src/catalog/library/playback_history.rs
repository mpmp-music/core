use super::*;

impl PlaybackHistory for Library {
    fn record_play(&self, record: PlayRecord) {
        if let Err(error) = Library::record_play(
            self,
            mpmp_library::PlayEvent {
                track_id: mpmp_library::TrackId(record.track.0),
                context: record.context.as_ref().map(context_summary),
                started_at: record.started_at_ms,
                played_ms: record.played_ms,
                ended_reason: match record.ended {
                    EndReason::Completed => mpmp_library::EndedReason::Completed,
                    EndReason::Skipped => mpmp_library::EndedReason::Skipped,
                    EndReason::Stopped => mpmp_library::EndedReason::Stopped,
                    EndReason::Failed => mpmp_library::EndedReason::Failed,
                    _ => mpmp_library::EndedReason::Unknown,
                },
                // Plays with no command behind them are the system's.
                origin: record.origin.as_ref().unwrap_or(&Origin::System).label(),
            },
        ) {
            tracing::warn!(%error, "play event could not be queued");
        }
    }
}
