use super::*;

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FolderAction {
    List,
    Add,
    Remove,
    Rescan,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderArgs {
    action: FolderAction,
    path: Option<String>,
    root: Option<i64>,
}

pub(super) async fn library_folders(session: &Session, args: &JsonObject) -> Outcome {
    let a: FolderArgs = parse(args)?;
    let action = action_name(args);
    let command = match a.action {
        FolderAction::List => {
            only(args, action, &[])?;
            return match session.query(Query::Roots).await? {
                QueryResult::Roots { roots } => Ok(success(json!({
                                    "folders": roots.iter().map(|r| json!({
                                    "root": r.id,
                                     "path": r.path,
                                     "track_count": r.track_count}
                                    )).collect::<Vec<_>>(),

                                    "hint": concat!(
                "search_tracks with root (and folder, a folder inside it) lists the tracks ",
                "there.",
                ),
                                }))),
                other => Err(unexpected(&other)),
            };
        }
        FolderAction::Add => {
            only(args, action, &["path"])?;
            Command::AddRoot {
                path: need(a.path, "path", "add")?,
            }
        }
        FolderAction::Remove => {
            only(args, action, &["root"])?;
            Command::RemoveRoot {
                root: RootId(need(a.root, "root", "remove")?),
            }
        }
        FolderAction::Rescan => {
            only(args, action, &[])?;
            Command::Rescan
        }
    };
    Ok(session.command_outcome(session.run(command).await))
}
