use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandArgs {
    command: Value,
    idempotency_key: Option<String>,
}

pub(super) async fn contract_command(session: &Session, args: &JsonObject) -> Outcome {
    let a: CommandArgs = parse(args)?;
    let mut request = Map::new();
    request.insert("request_id".into(), session.next_request_id().into());
    if let Some(key) = a.idempotency_key {
        // Core keeps keys per agent (guarantee 13), and a model's keys ("next-1") easily
        // repeat across its conversations: this connection's only, so another session's
        // command with the same key is not skipped for this one's result.
        let key = format!("{}:{key}", session.session_id());
        request.insert("idempotency_key".into(), key.into());
    }
    request.insert("command".into(), a.command);
    let result = session.run_json(Value::Object(request).to_string()).await;
    Ok(session.command_outcome(result))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceArgs {
    name: Option<String>,
}

pub(super) fn contract_reference(args: &JsonObject) -> Outcome {
    let a: ReferenceArgs = parse(args)?;
    Ok(success(match a.name {
        Some(name) => reference::lookup(&name)?,
        None => reference::list(),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryArgs {
    query: Value,
}

pub(super) async fn contract_query(session: &Session, args: &JsonObject) -> Outcome {
    let a: QueryArgs = parse(args)?;
    if refused_query(&a.query) {
        return Err(ContractError::forbidden(
            ErrorCode::OriginNotAllowed,
            "the agents and permissions queries are not available to agents",
        )
        .with_field("query"));
    }
    let answer = session.query_json(a.query.to_string()).await;
    match answer.get("type").and_then(Value::as_str) {
        Some("err") => Ok(failure(answer.get("error").cloned().unwrap_or(answer))),
        _ => Ok(success(answer)),
    }
}

/// The queries that list every agent, grant and the user's folders in them: never handed
/// to the model (docs/design/agent-permissions.md).
pub(super) fn refused_query(query: &Value) -> bool {
    let named = query
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|name| reference::REFUSED_QUERIES.contains(&name));
    let decoded = matches!(
        serde_json::from_value::<Query>(query.clone()),
        Ok(Query::Agents | Query::Permissions { .. })
    );
    named || decoded
}
