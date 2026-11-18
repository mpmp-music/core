use super::*;

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PlaylistAction {
    List,
    Items,
    Create,
    Rename,
    AddTracks,
    RemoveItems,
    Delete,
    ExportM3u,
    ImportM3u,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaylistArgs {
    action: PlaylistAction,
    playlist: Option<i64>,
    name: Option<String>,
    path: Option<String>,
    tracks: Option<Vec<i64>>,
    items: Option<Vec<i64>>,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

pub(super) async fn playlists(session: &Session, args: &JsonObject) -> Outcome {
    let a: PlaylistArgs = parse(args)?;
    let action = action_name(args);
    let name = action.unwrap_or_default();
    let playlist = |a: &PlaylistArgs| need(a.playlist, "playlist", name).map(PlaylistId);
    let command = match a.action {
        PlaylistAction::List => {
            only(args, action, &["offset", "limit"])?;
            let query = Query::Playlists {
                deleted: false,
                offset: a.offset,
                limit: page_limit(a.limit, 50),
            };
            return match session.query(query).await? {
                QueryResult::Playlists {
                    playlists,
                    offset,
                    total,
                    next_offset,
                } => Ok(success(json!({
                    "playlists": playlists.iter().map(playlist_summary).collect::<Vec<_>>(),
                    "offset": offset, "total": total, "next_offset": next_offset,
                }))),
                other => Err(unexpected(&other)),
            };
        }
        PlaylistAction::Items => {
            only(args, action, &["playlist", "offset", "limit"])?;
            let query = Query::PlaylistItems {
                playlist: playlist(&a)?,
                offset: a.offset,
                limit: page_limit(a.limit, 50),
                tags: Vec::new(),
            };
            return match session.query(query).await? {
                QueryResult::PlaylistItems {
                    playlist,
                    items,
                    offset,
                    total,
                    next_offset,
                } => Ok(success(json!({
                    "playlist": playlist_summary(&playlist),
                    "items": items.iter().map(|i| json!({
                    "item": i.item,
                     "track": track(&i.track)}
                    )).collect::<Vec<_>>(),

                    "offset": offset, "total": total, "next_offset": next_offset,
                }))),
                other => Err(unexpected(&other)),
            };
        }
        PlaylistAction::Create => {
            only(args, action, &["name"])?;
            Command::CreatePlaylist {
                name: need(a.name, "name", name)?,
                kind: PlaylistKind::Manual,
            }
        }
        PlaylistAction::Rename => {
            only(args, action, &["playlist", "name"])?;
            Command::RenamePlaylist {
                playlist: playlist(&a)?,
                name: need(a.name, "name", name)?,
                expected_revision: None,
            }
        }
        PlaylistAction::AddTracks => {
            only(args, action, &["playlist", "tracks"])?;
            Command::AddToPlaylist {
                playlist: playlist(&a)?,
                tracks: track_ids(need(a.tracks, "tracks", name)?),
                index: None,
                expected_revision: None,
            }
        }
        PlaylistAction::RemoveItems => {
            only(args, action, &["playlist", "items"])?;
            Command::RemoveFromPlaylist {
                playlist: playlist(&a)?,
                items: need(a.items, "items", name)?
                    .into_iter()
                    .map(PlaylistItemId)
                    .collect(),
                expected_revision: None,
            }
        }
        PlaylistAction::Delete => {
            only(args, action, &["playlist"])?;
            Command::DeletePlaylist {
                playlist: playlist(&a)?,
            }
        }
        // The contract's export always replaces a file that is there: there is no
        // "only if new" to offer (the user approves each write, seeing the path).
        PlaylistAction::ExportM3u => {
            only(args, action, &["playlist", "path"])?;
            Command::ExportM3u {
                playlist: playlist(&a)?,
                path: need(a.path, "path", name)?,
            }
        }
        PlaylistAction::ImportM3u => {
            only(args, action, &["path", "name"])?;
            Command::ImportM3u {
                path: need(a.path, "path", name)?,
                name: a.name,
            }
        }
    };
    Ok(session.command_outcome(session.run(command).await))
}

fn playlist_summary(p: &PlaylistSummary) -> Value {
    let kind = match p.kind {
        PlaylistKind::Manual => "manual",
        PlaylistKind::Smart { .. } => "smart",
        _ => "unknown",
    };
    let mut value = json!({
        "id": p.id, "name": p.name, "kind": kind, "track_count": p.track_count,
        "duration_ms": p.duration_ms, "revision": p.revision,
    });
    if p.deleted_at_ms.is_some()
        && let Some(map) = value.as_object_mut()
    {
        map.insert("in_trash".into(), true.into());
    }
    value
}
