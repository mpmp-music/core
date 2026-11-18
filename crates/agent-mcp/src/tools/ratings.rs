use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RateArgs {
    tracks: Vec<i64>,
    /// Absent: leave the rating; `null`: clear it.
    #[serde(default, deserialize_with = "present")]
    rating: Option<Option<u8>>,
    loved: Option<bool>,
}

/// A field that may be `null`: `Some(None)` when sent as `null`, `None` when absent.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

/// One command, or two (`rating` and `loved`), each with its own outcome. With both, each
/// part is its receipt or `{"error": <contract error>}`; the call fails only when both did,
/// and a part that failed is named in `message` (the other one took effect).
pub(super) async fn rate_tracks(session: &Session, args: &JsonObject) -> Outcome {
    let a: RateArgs = parse(args)?;
    let tracks = track_ids(a.tracks);
    let mut commands = Vec::new();
    if let Some(rating) = a.rating {
        let command = Command::SetRating {
            tracks: tracks.clone(),
            rating,
        };
        commands.push(("rating", command));
    }
    if let Some(loved) = a.loved {
        commands.push(("loved", Command::SetLoved { tracks, loved }));
    }
    if commands.is_empty() {
        return Err(invalid("give rating, loved or both", "rating"));
    }
    let mut parts = Vec::new();
    for (name, command) in commands {
        parts.push((name, session.command_outcome(session.run(command).await)));
    }
    if let [(name, only)] = parts.as_slice() {
        // One command: its result as is, a receipt under its name.
        let value = structured(only);
        return Ok(
            match only.is_error == Some(true) || value.get("status").is_some() {
                true => only.clone(),
                false => success(json!({ (*name): value })),
            },
        );
    }
    let failed: Vec<&str> = parts
        .iter()
        .filter(|(_, r)| r.is_error == Some(true))
        .map(|(name, _)| *name)
        .collect();
    let mut value = Map::new();
    for (name, result) in &parts {
        let part = structured(result);
        let part = match result.is_error {
            Some(true) => json!({ "error": part }),
            _ => part,
        };
        value.insert((*name).into(), part);
    }
    if failed.len() == parts.len() {
        // Both failed: the first one's error, as for any failure, with both parts.
        let mut error = match structured(&parts[0].1) {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        error.extend(value);
        return Ok(failure(Value::Object(error)));
    }
    if !failed.is_empty() {
        let done: Vec<&str> = parts
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| !failed.contains(name))
            .collect();
        value.insert(
            "message".into(),
            format!(
                "{} failed (see its error); {} took effect",
                failed.join(", "),
                done.join(", ")
            )
            .into(),
        );
    }
    Ok(success(Value::Object(value)))
}

/// A tool result's JSON (its structured content).
fn structured(result: &CallToolResult) -> Value {
    result.structured_content.clone().unwrap_or(Value::Null)
}
