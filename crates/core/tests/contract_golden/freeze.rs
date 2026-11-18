//! The freeze guard's schema half: compares the schema frozen with a contract version
//! (`golden/v1.*/contract.schema.json`) with docs/contract.schema.json and lists every
//! change that is not additive (README.md in the golden directory): of the five roots and
//! of every shared definition in `$defs`, including scalar ones (an id's integer format).

use std::collections::BTreeSet;

use serde_json::Value as J;

const ROOTS: [&str; 5] = [
    "Request",
    "Query",
    "Response",
    "QueryResult",
    "EventEnvelope",
];

/// Client → core roots: old clients must still be able to send what they sent.
fn request_side(root: &str) -> bool {
    matches!(root, "Request" | "Query")
}

/// The `$defs` a root uses, directly or through other definitions.
fn reachable_defs(schema: &J, root: &str) -> BTreeSet<String> {
    fn refs(value: &J, out: &mut Vec<String>) {
        match value {
            J::Object(map) => {
                if let Some(name) = map
                    .get("$ref")
                    .and_then(J::as_str)
                    .and_then(|r| r.strip_prefix("#/$defs/"))
                {
                    out.push(name.to_owned());
                }
                map.values().for_each(|v| refs(v, out));
            }
            J::Array(items) => items.iter().for_each(|v| refs(v, out)),
            _ => {}
        }
    }
    let mut seen = BTreeSet::new();
    let mut todo = Vec::new();
    refs(&schema[root], &mut todo);
    while let Some(name) = todo.pop() {
        if seen.insert(name.clone()) {
            refs(&schema["$defs"][name.as_str()], &mut todo);
        }
    }
    seen
}

/// Every non-additive difference between the frozen and the current schema.
pub fn breaking_changes(frozen: &J, current: &J) -> Vec<String> {
    let mut out = Vec::new();
    if frozen["contract_version"] != current["contract_version"] {
        out.push(format!(
            "contract_version changed from {} to {}: a new major version needs a new frozen set",
            frozen["contract_version"], current["contract_version"]
        ));
    }
    if current["contract_minor"].as_u64() < frozen["contract_minor"].as_u64() {
        out.push("contract_minor went down".into());
    }
    if current["contract_minor"] == frozen["contract_minor"]
        && normalized_shape(current) != normalized_shape(frozen)
    {
        out.push("contract shape changed without raising contract_minor".into());
    }
    if let Some(limits) = frozen["limits"].as_object() {
        for (name, old) in limits {
            let new = &current["limits"][name];
            if new.as_u64() < old.as_u64() {
                out.push(format!("limits.{name} went down from {old} to {new}"));
            }
        }
    }
    if enum_values(&frozen["$defs"]["ErrorKind"]) != enum_values(&current["$defs"]["ErrorKind"]) {
        out.push("ErrorKind is closed: its complete value set must not change".into());
    }
    for root in ROOTS {
        compare_node(
            &frozen[root],
            &current[root],
            root,
            request_side(root),
            &mut out,
        );
    }
    // A shared definition is checked by the rules of each side that uses it (a
    // `TrackQuery` travels in requests and in results).
    let mut sides: Vec<(String, bool)> = Vec::new();
    for root in ROOTS {
        for name in reachable_defs(frozen, root) {
            let side = request_side(root);
            if !sides.contains(&(name.clone(), side)) {
                sides.push((name, side));
            }
        }
    }
    sides.sort();
    for (name, side) in sides {
        let at = format!("$defs.{name}");
        match current["$defs"].get(&name) {
            Some(c_def) => compare_node(&frozen["$defs"][&name], c_def, &at, side, &mut out),
            None => out.push(format!("{at} was removed or renamed")),
        }
    }
    // A definition used on both sides may report the same change twice.
    let mut reported = BTreeSet::new();
    out.retain(|change| reported.insert(change.clone()));
    out
}

/// The tags or values a variant of a `oneOf` stands for.
fn branch_tags(branch: &J) -> Vec<String> {
    if let Some(tag) = mpmp_core::contract::variant_tag(branch) {
        vec![tag.to_owned()]
    } else if let Some(c) = branch["const"].as_str() {
        vec![c.to_owned()]
    } else {
        branch["enum"]
            .as_array()
            .map(|v| v.iter().filter_map(J::as_str).map(str::to_owned).collect())
            .unwrap_or_default()
    }
}

fn compare_node(f: &J, c: &J, at: &str, side: bool, out: &mut Vec<String>) {
    let Some(f_branches) = f["oneOf"].as_array() else {
        compare_object(f, c, at, side, out);
        return;
    };
    let Some(c_branches) = c["oneOf"].as_array() else {
        out.push(format!("{at} is no longer an enum"));
        return;
    };
    for f_branch in f_branches {
        for tag in branch_tags(f_branch) {
            let at = format!("{at}.{tag}");
            let Some(c_branch) = c_branches.iter().find(|b| branch_tags(b).contains(&tag)) else {
                out.push(format!("{at}: variant removed or renamed"));
                continue;
            };
            // A tagged variant is an object of its own; a value of a string enum is a
            // scalar (which must stay one).
            if f_branch.get("properties").is_some()
                || f_branch.get("$ref").is_some()
                || f_branch.get("allOf").is_some()
            {
                compare_object(f_branch, c_branch, &at, side, out);
            } else {
                compare_scalar(
                    &scalar_value(f_branch, &tag),
                    &scalar_value(c_branch, &tag),
                    &at,
                    out,
                );
            }
            compare_policy(f_branch, c_branch, &at, out);
        }
    }
}

// schemars may group undocumented string values in enum and split documented
// values into const branches. Compare one value at a time, without rewriting frozen files.
fn scalar_value(branch: &J, tag: &str) -> J {
    let mut value = branch.clone();
    if value.as_object_mut().unwrap().remove("enum").is_some() {
        value["const"] = tag.into();
    }
    value
}

/// `x-origins`: allow < confirm < forbidden; a command may get more permissive, never
/// stricter.
fn compare_policy(f: &J, c: &J, at: &str, out: &mut Vec<String>) {
    for key in ["x-class", "x-local-path"] {
        if let Some(old) = f.get(key)
            && c.get(key) != Some(old)
        {
            out.push(format!("{at}: {key} changed"));
        }
    }
    let rank = |p: &J| match p.as_str() {
        Some("allow") => 0,
        Some("confirm") => 1,
        _ => 2,
    };
    if let Some(origins) = f["x-origins"].as_object() {
        for (origin, old) in origins {
            let new = &c["x-origins"][origin];
            if rank(new) > rank(old) {
                out.push(format!(
                    "{at}: policy for {origin} tightened from {old} to {new}"
                ));
            }
        }
    }
}

/// A property's schema without its documentation, for comparing types. A lone `allOf`
/// part is the part itself: describing a `$ref` property later wraps it in an `allOf`
/// (a `$ref` has no siblings), which changes nothing on the wire.
fn shape(schema: &J) -> J {
    normalized_shape(schema)
}

/// The keywords that fix a value's type on the wire (not its documentation).
const SCALAR_KEYWORDS: [&str; 7] = [
    "type",
    "format",
    "const",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
];

/// A node's own type keywords did not change: an id's integer format (`int64` →
/// `uint64` changes the generated type), a string turned object, a bound moved. An `enum`
/// may only grow.
fn compare_scalar(f: &J, c: &J, at: &str, out: &mut Vec<String>) {
    if let Some(old) = f.get("x-kind")
        && c.get("x-kind") != Some(old)
    {
        out.push(format!("{at}: x-kind changed"));
    }
    for key in SCALAR_KEYWORDS {
        if f.get(key) != c.get(key) {
            out.push(format!(
                "{at}: {key} changed from {} to {}",
                f.get(key).unwrap_or(&J::Null),
                c.get(key).unwrap_or(&J::Null)
            ));
        }
    }
    if let Some(values) = f["enum"].as_array() {
        for value in values {
            if !c["enum"].as_array().is_some_and(|c| c.contains(value)) {
                out.push(format!("{at}: enum value {value} removed"));
            }
        }
    }
}

/// Compare the complete contract, preserving wire constraints and frozen metadata.
/// Documentation and x-risk may change without a new minor. Names in properties/$defs
/// are data, even when a field happens to be called description or title.
pub fn normalized_shape(schema: &J) -> J {
    fn normalize(value: &J, names: bool) -> J {
        match value {
            J::Object(map) => {
                let mut out: serde_json::Map<String, J> = map
                    .iter()
                    .filter(|(k, _)| {
                        names
                            || !matches!(
                                k.as_str(),
                                "description" | "title" | "$comment" | "x-risk"
                            )
                    })
                    .map(|(k, v)| {
                        let mut v = normalize(v, matches!(k.as_str(), "properties" | "$defs"));
                        if !names
                            && matches!(k.as_str(), "oneOf" | "anyOf")
                            && let Some(items) = v.as_array_mut()
                        {
                            let mut expanded = Vec::new();
                            for item in std::mem::take(items) {
                                if item["type"] == "string"
                                    && let Some(values) = item["enum"].as_array()
                                {
                                    for tag in values.iter().filter_map(J::as_str) {
                                        expanded.push(scalar_value(&item, tag));
                                    }
                                } else {
                                    expanded.push(item);
                                }
                            }
                            *items = expanded;
                        }
                        if !names
                            && matches!(
                                k.as_str(),
                                "oneOf" | "anyOf" | "allOf" | "required" | "enum"
                            )
                            && let Some(items) = v.as_array_mut()
                        {
                            items.sort_by_key(J::to_string);
                        }
                        (k.clone(), v)
                    })
                    .collect();
                if out.len() == 1
                    && let Some(J::Array(parts)) = out.get_mut("allOf")
                    && parts.len() == 1
                {
                    return parts.remove(0);
                }
                J::Object(out)
            }
            J::Array(items) => J::Array(items.iter().map(|v| normalize(v, false)).collect()),
            other => other.clone(),
        }
    }
    normalize(schema, false)
}

fn required(schema: &J) -> BTreeSet<String> {
    schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(J::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

fn compare_object(f: &J, c: &J, at: &str, side: bool, out: &mut Vec<String>) {
    if f.get("$ref") != c.get("$ref") {
        out.push(format!(
            "{at}: $ref changed from {} to {}",
            f["$ref"], c["$ref"]
        ));
    }
    compare_scalar(f, c, at, out);
    // `allOf: [{$ref}, {inline fields}]`: each part by the same rules.
    if let Some(f_parts) = f["allOf"].as_array() {
        match c["allOf"].as_array() {
            Some(c_parts) if c_parts.len() == f_parts.len() => {
                for (i, (f_part, c_part)) in f_parts.iter().zip(c_parts).enumerate() {
                    compare_object(f_part, c_part, &format!("{at}.allOf[{i}]"), side, out);
                }
            }
            _ => out.push(format!("{at}: allOf changed")),
        }
    }
    if let Some(properties) = f["properties"].as_object() {
        for (key, f_prop) in properties {
            match c["properties"].get(key) {
                None => out.push(format!("{at}.{key} was removed or renamed")),
                Some(c_prop) => {
                    if shape(f_prop) != shape(c_prop) {
                        out.push(format!(
                            "{at}.{key}: type or default changed from {} to {}",
                            shape(f_prop),
                            shape(c_prop)
                        ));
                    }
                }
            }
        }
    }
    let (f_req, c_req) = (required(f), required(c));
    if side {
        // Old clients do not send fields added later.
        for key in c_req.difference(&f_req) {
            out.push(format!("{at}.{key} became required in a request"));
        }
    } else {
        // Old clients rely on what was always sent.
        for key in f_req.difference(&c_req) {
            out.push(format!("{at}.{key} is no longer always sent"));
        }
    }
}

/// ErrorKind is closed even when the contract minor increases.
fn enum_values(schema: &J) -> BTreeSet<String> {
    let mut values: BTreeSet<_> = branch_tags(schema).into_iter().collect();
    if let Some(branches) = schema["oneOf"].as_array() {
        values.extend(branches.iter().flat_map(branch_tags));
    }
    values
}

/// Frozen capabilities are additive only after increasing the minor.
pub fn feature_changes(frozen: &J, current: &J) -> Vec<String> {
    let features = |snapshot: &J| -> BTreeSet<String> {
        snapshot["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    };
    let old = features(frozen);
    let new = features(current);
    let mut out = Vec::new();
    if !old.is_subset(&new) {
        out.push("frozen Snapshot features were removed".into());
    }
    if current["contract_minor"] == frozen["contract_minor"] && old != new {
        out.push("Snapshot features changed without raising contract_minor".into());
    }
    out
}
