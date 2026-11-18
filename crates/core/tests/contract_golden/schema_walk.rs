//! Walks a JSON message alongside its schema (docs/contract.schema.json): which enum
//! variants and optional fields it shows, where its enum values and objects are. Only the
//! keywords the generated schema uses are understood (`$ref`, `oneOf`, `anyOf`,
//! `properties`, `required`, `items`, `additionalProperties`, `const`, `enum`); validation
//! itself is the `jsonschema` crate's job.

use std::collections::BTreeSet;

use serde_json::Value as J;

/// One step of a path into a JSON document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

/// A path as the contract's error `details.field` writes it: `command.tracks[2]`.
pub fn field_path(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Seg::Index(i) => out.push_str(&format!("[{i}]")),
        }
    }
    out
}

pub fn get_mut<'a>(value: &'a mut J, path: &[Seg]) -> &'a mut J {
    path.iter().fold(value, |v, seg| match seg {
        Seg::Key(k) => &mut v[k.as_str()],
        Seg::Index(i) => &mut v[*i],
    })
}

/// A position holding a value of an enum with an `unknown` fallback.
#[derive(Debug, Clone)]
pub struct EnumPos {
    pub path: Vec<Seg>,
    /// `{"type": …}` (tagged) rather than a plain string.
    pub tagged: bool,
    /// The `$defs` name (or schema root) of the enum.
    pub name: String,
}

#[derive(Debug, Default)]
pub struct Walk {
    /// `Def.variant` for every variant shown, `Def[.variant].field` for every optional
    /// field present (not `null`).
    pub covered: BTreeSet<String>,
    pub enums: Vec<EnumPos>,
    /// Objects with fields (not maps such as `tags`, whose keys are data).
    pub objects: Vec<Vec<Seg>>,
}

/// One of the schema's five roots, with the shared `$defs` its `$ref`s point into.
pub struct SchemaRoot<'a> {
    pub name: &'a str,
    root: &'a J,
    defs: &'a J,
}

impl<'a> SchemaRoot<'a> {
    pub fn new(schema: &'a J, name: &'a str) -> Self {
        Self {
            name,
            root: &schema[name],
            defs: &schema["$defs"],
        }
    }

    fn def(&self, reference: &str) -> (&'a J, String) {
        let name = reference
            .strip_prefix("#/$defs/")
            .unwrap_or_else(|| panic!("unexpected $ref {reference}"));
        let def = self
            .defs
            .get(name)
            .unwrap_or_else(|| panic!("{}: no $defs/{name}", self.name));
        (def, name.to_owned())
    }

    /// Walks a whole message of this root.
    pub fn walk(&self, message: &J) -> Walk {
        let mut out = Walk::default();
        self.walk_at(self.root, message, self.name, &mut Vec::new(), &mut out);
        out
    }

    fn walk_at(&self, schema: &J, value: &J, name: &str, path: &mut Vec<Seg>, out: &mut Walk) {
        if let Some(reference) = schema.get("$ref").and_then(J::as_str) {
            let (def, def_name) = self.def(reference);
            self.walk_at(def, value, &def_name, path, out);
        }
        // `allOf: [{$ref}, {inline}]` (a `$ref` with extra keywords): every part applies.
        if let Some(J::Array(parts)) = schema.get("allOf") {
            for part in parts {
                self.walk_at(part, value, name, path, out);
            }
            return;
        }
        if let Some(J::Array(branches)) = schema.get("oneOf") {
            let (branch, tag, tagged) = match value {
                J::Object(map) => {
                    let tag = map.get("type").and_then(J::as_str).unwrap_or_else(|| {
                        panic!("{}: untagged value of {name}", field_path(path))
                    });
                    let branch = branches
                        .iter()
                        .find(|b| mpmp_core::contract::variant_tag(b) == Some(tag));
                    (branch, tag.to_owned(), true)
                }
                J::String(s) => {
                    let branch = branches.iter().find(|b| {
                        b.get("const").and_then(J::as_str) == Some(s.as_str())
                            || b.get("enum")
                                .and_then(J::as_array)
                                .is_some_and(|e| e.contains(value))
                    });
                    (branch, s.clone(), false)
                }
                other => panic!("{}: {name} cannot be {other}", field_path(path)),
            };
            let branch = branch
                .unwrap_or_else(|| panic!("{}: {name} has no variant {tag}", field_path(path)));
            out.covered.insert(format!("{name}.{tag}"));
            if branches.iter().any(is_unknown_branch) {
                out.enums.push(EnumPos {
                    path: path.clone(),
                    tagged,
                    name: name.to_owned(),
                });
            }
            self.walk_at(branch, value, &format!("{name}.{tag}"), path, out);
            return;
        }
        if let Some(J::Array(branches)) = schema.get("anyOf") {
            if !value.is_null() {
                for branch in branches {
                    if branch.get("type") != Some(&J::from("null")) {
                        self.walk_at(branch, value, name, path, out);
                    }
                }
            }
            return;
        }
        match value {
            J::Object(map) => {
                let properties = schema.get("properties").and_then(J::as_object);
                let required: BTreeSet<&str> = schema
                    .get("required")
                    .and_then(J::as_array)
                    .map(|r| r.iter().filter_map(J::as_str).collect())
                    .unwrap_or_default();
                if properties.is_some() && !out.objects.iter().any(|p| p == path) {
                    out.objects.push(path.clone());
                }
                if let Some(properties) = properties {
                    for (key, sub) in properties {
                        let Some(v) = map.get(key) else { continue };
                        if !required.contains(key.as_str()) && !v.is_null() {
                            out.covered.insert(format!("{name}.{key}"));
                        }
                        path.push(Seg::Key(key.clone()));
                        self.walk_at(sub, v, &format!("{name}.{key}"), path, out);
                        path.pop();
                    }
                }
                if let Some(extra @ J::Object(_)) = schema.get("additionalProperties") {
                    for (key, v) in map {
                        if properties.is_some_and(|p| p.contains_key(key)) {
                            continue;
                        }
                        path.push(Seg::Key(key.clone()));
                        self.walk_at(extra, v, name, path, out);
                        path.pop();
                    }
                }
            }
            J::Array(items) => {
                if let Some(item) = schema.get("items") {
                    for (i, v) in items.iter().enumerate() {
                        path.push(Seg::Index(i));
                        self.walk_at(item, v, name, path, out);
                        path.pop();
                    }
                }
            }
            _ => {}
        }
    }

    /// Every variant (but `unknown`) and every optional field this root's schema and the
    /// shared definitions describe, named as [`Walk::covered`] names them.
    pub fn coverable(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        coverable_of(self.root, self.name, &mut out);
        if let J::Object(defs) = self.defs {
            for (name, def) in defs {
                coverable_of(def, name, &mut out);
            }
        }
        out
    }
}

fn is_unknown_branch(branch: &J) -> bool {
    branch.get("const") == Some(&J::from("unknown"))
        || mpmp_core::contract::variant_tag(branch) == Some("unknown")
}

/// The variants and optional fields of one schema node.
fn coverable_of(schema: &J, name: &str, out: &mut BTreeSet<String>) {
    if let Some(J::Array(branches)) = schema.get("oneOf") {
        for branch in branches {
            if is_unknown_branch(branch) {
                continue;
            }
            let tags: Vec<String> = if let Some(tag) = mpmp_core::contract::variant_tag(branch) {
                vec![tag.to_owned()]
            } else if let Some(c) = branch.get("const").and_then(J::as_str) {
                vec![c.to_owned()]
            } else if let Some(values) = branch.get("enum").and_then(J::as_array) {
                values
                    .iter()
                    .filter_map(J::as_str)
                    .map(str::to_owned)
                    .collect()
            } else {
                panic!("{name}: a variant without a tag: {branch}")
            };
            for tag in tags {
                optional_fields(branch, &format!("{name}.{tag}"), out);
                // Inline parts of an `allOf` (a `$ref`'s own fields are its definition's).
                for part in branch
                    .get("allOf")
                    .and_then(J::as_array)
                    .into_iter()
                    .flatten()
                {
                    optional_fields(part, &format!("{name}.{tag}"), out);
                }
                out.insert(format!("{name}.{tag}"));
            }
        }
    } else {
        optional_fields(schema, name, out);
    }
}

fn optional_fields(schema: &J, name: &str, out: &mut BTreeSet<String>) {
    let Some(properties) = schema.get("properties").and_then(J::as_object) else {
        return;
    };
    let required: BTreeSet<&str> = schema
        .get("required")
        .and_then(J::as_array)
        .map(|r| r.iter().filter_map(J::as_str).collect())
        .unwrap_or_default();
    for key in properties.keys() {
        if !required.contains(key.as_str()) {
            out.insert(format!("{name}.{key}"));
        }
    }
}
