//! `contract_reference`: the contract's commands and queries as the model needs them to use
//! `contract_command` / `contract_query` without the repository at hand. Built from the
//! same generator as docs/contract.schema.json (`mpmp_core::contract::json_schema`), once.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use mpmp_core::contract::{ContractError, ErrorCode, json_schema};
use serde_json::{Map, Value, json};

/// A first sentence shorter than this gets the next one too.
const MIN_SUMMARY: usize = 30;

/// The longest one-line summary in the list.
const MAX_SUMMARY: usize = 160;

/// The queries the adapter never runs for an agent (`contract_query` refuses them).
pub(crate) const REFUSED_QUERIES: [&str; 2] = ["agents", "permissions"];

fn schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(json_schema)
}

/// Every command and query: its name, its first sentence, and whether an agent may use it.
pub(crate) fn list() -> Value {
    let commands: Vec<Value> = commands()
        .map(|(name, variant)| {
            json!({"name": name, "summary": summary(variant), "agent": command_policy(variant)})
        })
        .collect();
    let queries: Vec<Value> = queries()
        .map(|(name, variant)| {
            json!({"name": name, "summary": summary(variant), "agent": query_policy(name)})
        })
        .collect();
    json!({
            "commands": commands,
            "queries": queries,
            "agent": concat!(
    "allow: runs; confirm: needs the user's approval (needs_confirmation, then ",
    "wait_for_confirmation); not_available: the user's own (only the player's ",
    "UI may send it).",
    ),
            "next": concat!(
    "Call contract_reference with a name for its parameters (JSON Schema). Run ",
    "commands with contract_command {\"command\": {\"type\": <name>, ...}}, ",
    "queries with contract_query {\"query\": {\"type\": <name>, ...}}.",
    ),
        })
}

/// One command or query by name: its JSON Schema, with the definitions it refers to
/// (`$defs`, resolvable from this object's root).
pub(crate) fn lookup(name: &str) -> Result<Value, ContractError> {
    let wanted = folded(name);
    let mut found = Vec::new();
    for (name, variant) in commands() {
        if folded(name) == wanted {
            found.push(entry(name, "command", command_policy(variant), variant));
        }
    }
    for (name, variant) in queries() {
        if folded(name) == wanted {
            found.push(entry(name, "query", query_policy(name), variant));
        }
    }
    match found.len() {
        0 => Err(ContractError::unsupported(
            ErrorCode::UnknownVariant,
            format!(
                concat!(
                    "the contract has no command or query named {name:?}: call ",
                    "contract_reference without a name for the list",
                ),
                name = name,
            ),
        )
        .with_field("name")),
        1 => Ok(found.remove(0)),
        _ => Ok(json!({"matches": found})),
    }
}

fn entry(name: &str, kind: &str, agent: &str, variant: &Value) -> Value {
    let mut schema = match variant {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    let defs = referenced_defs(variant);
    if !defs.is_empty() {
        schema.insert(
            "$defs".into(),
            Value::Object(defs.into_iter().collect::<Map<_, _>>()),
        );
    }
    let how = match kind {
        "command" => format!("contract_command {{\"command\": {{\"type\": \"{name}\", ...}}}}"),
        _ => format!("contract_query {{\"query\": {{\"type\": \"{name}\", ...}}}}"),
    };
    json!({"name": name, "kind": kind, "agent": agent, "how": how, "schema": Value::Object(schema)})
}

/// The commands, by `type`, as the schema has them (`$defs.Command.oneOf`).
fn commands() -> impl Iterator<Item = (&'static str, &'static Value)> {
    variants(schema().pointer("/$defs/Command/oneOf"))
}

/// The queries, by `type` (`Query.oneOf`).
fn queries() -> impl Iterator<Item = (&'static str, &'static Value)> {
    variants(schema().pointer("/Query/oneOf"))
}

fn variants(list: Option<&'static Value>) -> impl Iterator<Item = (&'static str, &'static Value)> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|variant| {
            let name = variant.pointer("/properties/type/const")?.as_str()?;
            // The decode-only fallback is never sent.
            (name != "unknown").then_some((name, variant))
        })
}

/// What an agent's command gets by default (`x-origins.agent`).
fn command_policy(variant: &Value) -> &'static str {
    match variant.pointer("/x-origins/agent").and_then(Value::as_str) {
        Some("allow") => "allow",
        Some("confirm") => "confirm",
        _ => "not_available",
    }
}

fn query_policy(name: &str) -> &'static str {
    match REFUSED_QUERIES.contains(&name) {
        true => "not_available",
        false => "allow",
    }
}

/// The first sentence of a variant's description (the second too when the first is
/// shorter than [`MIN_SUMMARY`] characters), on one line, at most [`MAX_SUMMARY`]
/// characters.
fn summary(variant: &Value) -> String {
    let text = variant
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let end = |from: usize| text[from..].find(". ").map(|i| from + i + 1);
    let first = match end(0) {
        Some(e) if text[..e].chars().count() < MIN_SUMMARY => {
            let more = end(e + 1).unwrap_or(text.len());
            &text[..more]
        }
        Some(e) => &text[..e],
        None => text.as_str(),
    };
    match first.char_indices().nth(MAX_SUMMARY) {
        Some((cut, _)) => format!("{}…", &first[..cut]),
        None => first.to_owned(),
    }
}

/// The `$defs` a schema refers to (`#/$defs/<name>`), and those they refer to, by name.
fn referenced_defs(value: &Value) -> BTreeMap<String, Value> {
    let defs = schema().get("$defs").and_then(Value::as_object);
    let mut found = BTreeMap::new();
    let mut todo = Vec::new();
    refs(value, &mut todo);
    while let Some(name) = todo.pop() {
        if found.contains_key(&name) {
            continue;
        }
        let Some(def) = defs.and_then(|defs| defs.get(&name)) else {
            continue;
        };
        refs(def, &mut todo);
        found.insert(name, def.clone());
    }
    found
}

fn refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(name) = map
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix("#/$defs/"))
            {
                out.push(name.to_owned());
            }
            map.values().for_each(|v| refs(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| refs(v, out)),
        _ => {}
    }
}

/// A name as it is compared: lower case, without `_` and `-` (`ExportM3u`, `export-m3u`).
fn folded(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_names_every_command_and_query() {
        let list = list();
        let commands = list["commands"].as_array().unwrap();
        let queries = list["queries"].as_array().unwrap();
        let find = |items: &[Value], name: &str| {
            items
                .iter()
                .find(|c| c["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("{name}"))
        };
        assert_eq!(find(commands, "export_m3u")["agent"], "confirm");
        assert_eq!(find(commands, "set_shuffle")["agent"], "allow");
        assert_eq!(find(commands, "grant_permission")["agent"], "not_available");
        assert_eq!(find(queries, "albums")["agent"], "allow");
        assert_eq!(find(queries, "permissions")["agent"], "not_available");
        assert!(
            commands
                .iter()
                .chain(queries)
                .all(|c| c["name"] != "unknown")
        );
        // One line each, not the whole description.
        let pause = find(commands, "pause");
        assert_eq!(
            pause["summary"],
            "Pause playback; no-op (`changed: false`) unless playing."
        );
        for item in commands.iter().chain(queries) {
            let summary = item["summary"].as_str().unwrap();
            assert!(!summary.is_empty() && !summary.contains('\n'), "{item}");
            assert!(summary.chars().count() <= MAX_SUMMARY + 1, "{item}");
        }
    }

    #[test]
    fn api_browsing_release_and_loudness_reference_is_current() {
        for name in ["scan_loudness", "clear_loudness", "cancel_loudness_scan"] {
            assert!(lookup(name).is_err());
        }
        for name in ["release_root", "resume_root"] {
            let entry = lookup(name).unwrap();
            assert_eq!(entry["agent"], "confirm");
            assert_eq!(
                entry["schema"]["properties"]["root"]["$ref"],
                "#/$defs/RootId"
            );
        }
        let album = lookup("album").unwrap();
        assert!(
            album["schema"]["description"]
                .as_str()
                .unwrap()
                .contains("Without offset and limit, returns all tracks")
        );
        assert!(album["schema"]["properties"]["offset"].is_object());
        assert!(album["schema"]["properties"]["limit"]["default"].is_null());
        assert!(
            !album["schema"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "offset" || v == "limit")
        );
        assert!(lookup("search").unwrap()["schema"]["properties"]["kind"].is_object());
    }

    /// A schema comes with every definition it refers to, also indirectly, so it can be
    /// read (and resolved) on its own.
    #[test]
    fn a_lookup_brings_its_definitions() {
        let found = lookup("ExportM3u").unwrap();
        assert_eq!(found["name"], "export_m3u");
        assert_eq!(found["kind"], "command");
        let schema = &found["schema"];
        assert_eq!(schema["properties"]["type"]["const"], "export_m3u");
        assert!(schema["$defs"]["PlaylistId"].is_object(), "{schema}");
        // Indirect: tracks → TrackQuery → Filter → Field …
        let tracks = lookup("tracks").unwrap();
        let defs = tracks["schema"]["$defs"].as_object().unwrap();
        for name in ["TrackQuery", "Filter", "Field", "SortKey"] {
            assert!(defs.contains_key(name), "{name}: {:?}", defs.keys());
        }
        let mut todo = Vec::new();
        refs(&tracks["schema"], &mut todo);
        for name in todo {
            assert!(
                defs.contains_key(&name),
                "{name} is referred to but not given"
            );
        }
        let e = lookup("export_playlist").unwrap_err();
        assert_eq!(e.details.field.as_deref(), Some("name"));
    }
}

#[cfg(test)]
mod job_action_scan_reference_tests {
    use super::*;
    #[test]
    fn authoritative_job_queries_are_available_read_only() {
        for name in ["job", "jobs"] {
            assert_eq!(query_policy(name), "allow");
            let entry = lookup(name).unwrap();
            assert_eq!(entry["kind"], "query");
        }
    }
}
