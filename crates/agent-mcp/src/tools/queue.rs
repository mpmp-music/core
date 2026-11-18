use super::*;

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum QueueAction {
    List,
    Add,
    Clear,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum At {
    End,
    Next,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueArgs {
    action: QueueAction,
    tracks: Option<Vec<i64>>,
    at: Option<At>,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

pub(super) async fn queue(session: &Session, args: &JsonObject) -> Outcome {
    let a: QueueArgs = parse(args)?;
    let action = action_name(args);
    let command = match a.action {
        QueueAction::List => {
            only(args, action, &["offset", "limit"])?;
            let query = Query::QueuePage {
                offset: a.offset,
                limit: page_limit(a.limit, 50),
            };
            return match session.query(query).await? {
                QueryResult::QueuePage {
                    revision,
                    offset,
                    total,
                    next_offset,
                    entries,
                } => Ok(success(json!({
                    "revision": revision,
                    "entries": entries.iter().map(|e| json!({
                    "entry": e.entry,
                     "track": track(&e.track)}
                    )).collect::<Vec<_>>(),

                    "offset": offset, "total": total, "next_offset": next_offset,
                }))),
                other => Err(unexpected(&other)),
            };
        }
        QueueAction::Add => {
            only(args, action, &["tracks", "at"])?;
            Command::Enqueue {
                tracks: track_ids(need(a.tracks, "tracks", "add")?),
                at: match a.at.unwrap_or(At::End) {
                    At::End => InsertAt::End,
                    At::Next => InsertAt::Next,
                },
                expected_revision: None,
                expected_current: None,
            }
        }
        QueueAction::Clear => {
            only(args, action, &[])?;
            Command::Clear {
                expected_revision: None,
            }
        }
    };
    Ok(session.command_outcome(session.run(command).await))
}
