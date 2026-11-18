//! Shared JSON Schema generation and command annotations.

use super::*;

/// JSON Schema of everything that crosses the contract, for clients in other languages
/// and agents: five roots (`Request`, `Query`, `Response`, `QueryResult`,
/// `EventEnvelope`) that share one top-level `$defs`. Each command variant carries
/// `x-risk` and `x-origins` from [`COMMANDS`]; the root's `description` has the semantic
/// guarantees. Only the outermost request objects (the `Request` envelope and each
/// `Query` variant) have `additionalProperties: false`, so each shared definition has one
/// shape. Core still rejects unknown fields at any depth of a request (`unsupported` /
/// `unknown_field`). A `$ref` never has sibling keywords (they are put in an `allOf`),
/// which code generators handle poorly.
pub fn json_schema() -> serde_json::Value {
    let mut defs = serde_json::Map::new();
    let mut roots = serde_json::Map::new();
    let mut dialect = serde_json::Value::Null;
    for (name, mut root) in root_schemas() {
        if let Some(map) = root.as_object_mut() {
            if let Some(d) = map.remove("$schema") {
                dialect = d;
            }
            // Identical in every root (a test checks): one copy is kept.
            if let Some(serde_json::Value::Object(root_defs)) = map.remove("$defs") {
                for (def, schema) in root_defs {
                    defs.entry(def).or_insert(schema);
                }
            }
        }
        roots.insert(name.to_owned(), root);
    }
    let mut schema = serde_json::json!({
            "$schema": dialect,
            "$comment": concat!(
    "Every integer is within ±(2^53-1). Unknown enum values decode as ",
    "\"unknown\" (core → client): a decode-only fallback, never sent. Commands ",
    "and queries reject unknown fields (client → core). The five roots share ",
    "the definitions in `$defs`: validate a message against its root resolved ",
    "in this document (e.g. `{\"$ref\": \"#/Request\"}`).",
    ),
            "description": format!(
                concat!(
    "The contract between the music player's core and every client (UI, media ",
    "controls, remote control, agents). Send a `Request` (a `Command`) or a ",
    "`Query`; core answers with a `Response` or a `QueryResult` and announces ",
    "changes as `EventEnvelope`s.\n\n{}",
    ),
                // A Windows checkout may have CRLF line ends; the schema is LF everywhere.
                include_str!("../guarantees.md").replace("\r\n", "\n")
            ),
            "contract_version": CONTRACT_VERSION,
            "contract_minor": CONTRACT_MINOR,
            "limits": {
                "max_track_page": MAX_TRACK_PAGE,
                "max_page": MAX_PAGE,
                "max_filter_parts": MAX_FILTER_PARTS,
                "max_filter_nodes": MAX_FILTER_NODES,
                "max_filter_depth": MAX_FILTER_DEPTH,
                "max_context_tracks": MAX_CONTEXT_TRACKS,
                "max_tag_edit_tracks": MAX_TAG_EDIT_TRACKS,
                "max_artist_name_bytes": MAX_ARTIST_NAME_BYTES,
                "max_cover_edit_tracks": MAX_COVER_EDIT_TRACKS,
                "max_cover_image_bytes": MAX_COVER_IMAGE_BYTES,
                "max_cover_image_pixels": MAX_COVER_IMAGE_PIXELS,
                "max_cover_image_edge": MAX_COVER_IMAGE_EDGE,
                "max_cover_undo_bytes": MAX_COVER_UNDO_BYTES,
                "max_sleep_timer_ms": MAX_SLEEP_TIMER_MS,
                "max_output_devices": MAX_OUTPUT_DEVICES,
                "max_output_format_ranges": MAX_OUTPUT_FORMAT_RANGES,
            },
            "$defs": defs,
        });
    if let Some(map) = schema.as_object_mut() {
        map.extend(roots);
    }
    annotate_commands(&mut schema);
    bound_integers(&mut schema);
    split_ref_siblings(&mut schema);
    schema
}

/// The schema of each root as schemars generates it, each with its own `$defs`; requests
/// closed at the outermost object.
pub(super) fn root_schemas() -> [(&'static str, serde_json::Value); 5] {
    let value = |schema: schemars::Schema| serde_json::to_value(schema).unwrap_or_default();
    let mut request = value(schemars::schema_for!(Request));
    let mut query = value(schemars::schema_for!(Query));
    close_root(&mut request);
    if let Some(variants) = query.get_mut("oneOf").and_then(|v| v.as_array_mut()) {
        variants.iter_mut().for_each(close_root);
    }
    let mut roots = [
        ("Request", request),
        ("Response", value(schemars::schema_for!(Response))),
        ("Query", query),
        ("QueryResult", value(schemars::schema_for!(QueryResult))),
        ("EventEnvelope", value(schemars::schema_for!(EventEnvelope))),
    ];
    for (_, root) in &mut roots {
        if let Some(branches) = root
            .get_mut("$defs")
            .and_then(|v| v.get_mut("ErrorCode"))
            .and_then(|v| v.get_mut("oneOf"))
            .and_then(serde_json::Value::as_array_mut)
        {
            let mut annotated = Vec::new();
            for mut branch in std::mem::take(branches) {
                if let Some(values) = branch.as_object_mut().unwrap().remove("enum") {
                    for code in values.as_array().unwrap() {
                        let mut single = branch.clone();
                        single["const"] = code.clone();
                        single["x-kind"] =
                            serde_json::to_value(ErrorCode::parse(code.as_str().unwrap()).kind())
                                .unwrap();
                        annotated.push(single);
                    }
                } else {
                    branch["x-kind"] = serde_json::to_value(
                        ErrorCode::parse(branch["const"].as_str().unwrap()).kind(),
                    )
                    .unwrap();
                    annotated.push(branch);
                }
            }
            *branches = annotated;
        }
    }
    roots
}

/// `additionalProperties: false` on one object schema (a request's outermost object).
fn close_root(value: &mut serde_json::Value) {
    if let Some(map) = value.as_object_mut()
        && map.contains_key("properties")
    {
        map.insert("additionalProperties".into(), false.into());
    }
}

/// A `$ref` never has siblings: `{"$ref": R, "description": d, k: v}` becomes
/// `{"description": d, "allOf": [{"$ref": R}, {k: v}]}`. Code generators such as
/// json-schema-to-typescript emit duplicate types for a `$ref` with siblings.
fn split_ref_siblings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.values_mut().for_each(split_ref_siblings);
            // Annotations stay on the outer node (next to `allOf`, not next to `$ref`);
            // other keywords go into a second `allOf` part.
            let annotation =
                |k: &str| matches!(k, "description" | "title" | "$comment" | "default");
            if map.contains_key("$ref") && map.len() > 1 {
                let reference = map.remove("$ref").unwrap_or_default();
                let rest: serde_json::Map<_, _> = map
                    .iter()
                    .filter(|(k, _)| !annotation(k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                map.retain(|k, _| annotation(k));
                let mut parts = vec![serde_json::json!({ "$ref": reference })];
                if !rest.is_empty() {
                    parts.push(serde_json::Value::Object(rest));
                }
                map.insert("allOf".into(), serde_json::Value::Array(parts));
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(split_ref_siblings),
        _ => {}
    }
}

/// `minimum` / `maximum` of ±(2^53 − 1) on 64-bit integers (the module docs' number rule).
fn bound_integers(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            match map.get("format").and_then(|f| f.as_str()) {
                Some("uint64") => {
                    map.insert("maximum".into(), MAX_SAFE_INTEGER.into());
                }
                Some("int64") => {
                    map.insert("minimum".into(), (-(MAX_SAFE_INTEGER as i64)).into());
                    map.insert("maximum".into(), MAX_SAFE_INTEGER.into());
                }
                _ => {}
            }
            map.values_mut().for_each(bound_integers);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(bound_integers),
        _ => {}
    }
}

/// The `x-risk` / `x-origins` of one command, as the schema has them.
pub fn command_extensions(spec: &CommandSpec) -> serde_json::Value {
    let origins: serde_json::Map<String, serde_json::Value> = origin_kinds()
        .iter()
        .map(|origin| {
            let policy = match spec.policy(origin) {
                Policy::Allow => "allow",
                Policy::Confirm => "confirm",
                Policy::Forbidden(_) => "forbidden",
            };
            (origin.kind().to_owned(), policy.into())
        })
        .collect();
    serde_json::json!({
        "x-risk": spec.risk,
        "x-local-path": spec.local_path,
        "x-class": spec.class,
        "x-origins": origins,
    })
}

/// The `type` a variant schema of a tagged enum stands for.
pub fn variant_tag(variant: &serde_json::Value) -> Option<&str> {
    if let Some(parts) = variant.get("allOf").and_then(|a| a.as_array()) {
        return parts.iter().find_map(variant_tag);
    }
    let tag = variant.get("properties")?.get("type")?;
    tag.get("const")
        .and_then(|c| c.as_str())
        .or_else(|| tag.get("enum")?.get(0)?.as_str())
}

fn annotate_commands(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::Object(defs)) = map.get_mut("$defs")
                && let Some(command) = defs.get_mut("Command")
                && let Some(serde_json::Value::Array(variants)) = command.get_mut("oneOf")
            {
                for variant in variants {
                    let spec = variant_tag(variant)
                        .and_then(|name| COMMANDS.iter().find(|s| s.name == name));
                    if let (Some(spec), serde_json::Value::Object(obj)) = (spec, variant)
                        && let serde_json::Value::Object(ext) = command_extensions(spec)
                    {
                        obj.extend(ext);
                    }
                }
            }
            for v in map.values_mut() {
                annotate_commands(v);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(annotate_commands),
        _ => {}
    }
}
