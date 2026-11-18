use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageArgs {
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

pub(super) async fn recent_actions(session: &Session, args: &JsonObject) -> Outcome {
    let a: PageArgs = parse(args)?;
    let query = Query::Actions {
        origin: None,
        offset: a.offset,
        limit: page_limit(a.limit, 20),
    };
    match session.query(query).await? {
        QueryResult::Actions {
            actions,
            offset,
            total,
            next_offset,
        } => Ok(success(json!({
            "actions": actions, "offset": offset, "total": total, "next_offset": next_offset,
        }))),
        other => Err(unexpected(&other)),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UndoArgs {
    action: i64,
}

pub(super) async fn undo(session: &Session, args: &JsonObject) -> Outcome {
    let a: UndoArgs = parse(args)?;
    let command = Command::UndoAction {
        action: ActionId(a.action),
    };
    Ok(session.command_outcome(session.run(command).await))
}
