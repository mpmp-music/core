use super::*;

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
enum PlaybackAction {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Stop,
    Seek,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaybackArgs {
    action: PlaybackAction,
    position_ms: Option<u64>,
    tracks: Option<Vec<i64>>,
    playlist: Option<i64>,
}

pub(super) async fn playback(session: &Session, args: &JsonObject) -> Outcome {
    let a: PlaybackArgs = parse(args)?;
    let action = action_name(args);
    let command = match a.action {
        PlaybackAction::Play => {
            only(args, action, &["tracks", "playlist"])?;
            match (a.tracks, a.playlist) {
                (Some(_), Some(_)) => {
                    return Err(invalid("give tracks or playlist, not both", "playlist"));
                }
                (Some(tracks), None) => play(PlayContext::Tracks {
                    tracks: track_ids(tracks),
                }),
                (None, Some(playlist)) => play(PlayContext::Playlist {
                    playlist: PlaylistId(playlist),
                    start_item: None,
                    expected_revision: None,
                }),
                (None, None) => Command::Resume,
            }
        }
        PlaybackAction::Seek => {
            only(args, action, &["position_ms"])?;
            Command::Seek {
                position_ms: need(a.position_ms, "position_ms", "seek")?,
                entry: None,
            }
        }
        simple => {
            only(args, action, &[])?;
            match simple {
                PlaybackAction::Pause => Command::Pause,
                PlaybackAction::Toggle => Command::TogglePause,
                PlaybackAction::Next => Command::Next,
                PlaybackAction::Previous => Command::Previous,
                _ => Command::Stop,
            }
        }
    };
    Ok(session.command_outcome(session.run(command).await))
}

fn play(context: PlayContext) -> Command {
    Command::Play {
        context,
        start: 0,
        start_track: None,
    }
}
