//! Strict JSON parsing, rejected-field checks and bounded error-path discovery.

use crate::contract::{Command, ContractError, ErrorCode, Request};
use serde::Deserialize;

pub(super) fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|err| {
        // Contract types always serialize; this is only a fallback.
        format!(r#"{{"type":"err","error":{{"kind":"failed","code":"io","message":"{err}"}}}}"#)
    })
}

pub(super) fn parse_request(json: &str) -> Result<Request, (u64, ContractError)> {
    let raw: serde_json::Value =
        serde_json::from_str(json).map_err(|e| (0, json_error(&e, None)))?;
    let request_id = raw.get("request_id").and_then(|v| v.as_u64()).unwrap_or(0);
    let request =
        Request::deserialize(&raw).map_err(|e| (request_id, data_error::<Request>(&e, &raw)))?;
    if request.command != Command::Unknown {
        check_no_dropped_fields(&raw, &request).map_err(|e| (request_id, e))?;
    }
    Ok(request)
}

/// Parses a client → core message by the contract's strict rules.
pub(super) fn parse_strict<T>(json: &str) -> Result<T, ContractError>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let raw: serde_json::Value = serde_json::from_str(json).map_err(|e| json_error(&e, None))?;
    let value = T::deserialize(&raw).map_err(|e| data_error::<T>(&e, &raw))?;
    check_no_dropped_fields(&raw, &value)?;
    Ok(value)
}

/// serde rejects unknown fields of structs and struct variants, but not of unit variants
/// of internally tagged enums (`{"type":"pause","x":1}`): compare what was sent with what
/// was understood. Subtrees that decoded as an unknown variant are left to the typed
/// layer (`unknown_variant`).
fn check_no_dropped_fields<T: serde::Serialize>(
    raw: &serde_json::Value,
    parsed: &T,
) -> Result<(), ContractError> {
    let understood = serde_json::to_value(parsed).map_err(|e| json_error(&e, None))?;
    match dropped_field(raw, &understood, String::new()) {
        Some(path) => Err(unknown_field(path)),
        None => Ok(()),
    }
}

fn dropped_field(
    sent: &serde_json::Value,
    understood: &serde_json::Value,
    path: String,
) -> Option<String> {
    use serde_json::Value as J;
    let join = |key: &str| {
        if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        }
    };
    match (sent, understood) {
        (J::Object(sent), J::Object(understood)) => {
            if understood.get("type").and_then(J::as_str) == Some("unknown")
                && sent.get("type").and_then(J::as_str) != Some("unknown")
            {
                return None;
            }
            for (key, value) in sent {
                match understood.get(key) {
                    None => return Some(join(key)),
                    Some(u) => {
                        if let Some(found) = dropped_field(value, u, join(key)) {
                            return Some(found);
                        }
                    }
                }
            }
            None
        }
        (J::Array(sent), J::Array(understood)) => sent
            .iter()
            .zip(understood)
            .enumerate()
            .find_map(|(i, (s, u))| dropped_field(s, u, format!("{path}[{i}]"))),
        _ => None,
    }
}

fn unknown_field(path: String) -> ContractError {
    ContractError::unsupported(
        ErrorCode::UnknownField,
        format!("this core does not know the field {path}"),
    )
    .with_field(path)
}

/// A serde error as a contract error. `raw`: the parsed JSON, to find where an unknown
/// field is.
fn json_error(e: &serde_json::Error, raw: Option<&serde_json::Value>) -> ContractError {
    use serde_json::error::Category;
    match e.classify() {
        Category::Syntax | Category::Eof => {
            let mut error = ContractError::invalid(ErrorCode::InvalidJson, e.to_string());
            error.details.line = Some(e.line().clamp(1, u32::MAX as usize) as u32);
            error.details.column = Some(e.column().clamp(1, u32::MAX as usize) as u32);
            error
        }
        Category::Io => ContractError::failed(ErrorCode::Io, e.to_string()),
        Category::Data => {
            let message = e.to_string();
            match message
                .strip_prefix("unknown field `")
                .and_then(|rest| rest.split('`').next())
            {
                Some(name) => {
                    let path = raw
                        .and_then(|raw| find_key(raw, name, String::new()))
                        .unwrap_or_else(|| name.to_owned());
                    unknown_field(path)
                }
                None => ContractError::invalid(ErrorCode::InvalidRequest, message),
            }
        }
    }
}

/// A serde error decoding `raw` as `T`, as a contract error; a missing field gets its
/// path in `details.field` (`command.rating`).
fn data_error<T: serde::de::DeserializeOwned>(
    e: &serde_json::Error,
    raw: &serde_json::Value,
) -> ContractError {
    let error = json_error(e, Some(raw));
    let message = e.to_string();
    let missing = message
        .strip_prefix("missing field `")
        .and_then(|rest| rest.split('`').next());
    match missing.and_then(|name| missing_field_path::<T>(raw, name)) {
        Some(path) => error.with_field(path),
        None => match scalar_error_path::<T>(raw, &message) {
            Some(path) => error.with_field(path),
            None => error,
        },
    }
}

/// Internally tagged enums buffer their fields, losing serde's traversal location.
/// On scalar type/value errors, probe matching values with a distinct scalar and require
/// the *same expected type* and that exact replacement in the new error. Merely changing an
/// error is insufficient: it could introduce a different, earlier failure. Like missing
/// fields, diagnostics are best effort and bounded; never guess from repeated values.
fn scalar_error_path<T: serde::de::DeserializeOwned>(
    raw: &serde_json::Value,
    message: &str,
) -> Option<String> {
    use serde::de::Unexpected;
    use serde_json::Value as J;
    let (kind, rest) = if let Some(rest) = message.strip_prefix("invalid type: ") {
        ("invalid type", rest)
    } else {
        ("invalid value", message.strip_prefix("invalid value: ")?)
    };
    let (actual, expected) = rest.rsplit_once(", expected ")?;
    fn unexpected(value: &J) -> Option<Unexpected<'_>> {
        match value {
            J::String(s) => Some(Unexpected::Str(s)),
            J::Bool(b) => Some(Unexpected::Bool(*b)),
            J::Number(n) => n
                .as_u64()
                .map(Unexpected::Unsigned)
                .or_else(|| n.as_i64().map(Unexpected::Signed))
                .or_else(|| n.as_f64().map(Unexpected::Float)),
            _ => None,
        }
    }
    let mut candidates = Vec::new();
    fn walk(
        value: &J,
        pointer: String,
        path: String,
        actual: &str,
        out: &mut Vec<(String, String)>,
    ) {
        if out.len() >= MAX_PATH_PROBES {
            return;
        }
        if !path.is_empty() && unexpected(value).is_some_and(|u| u.to_string() == actual) {
            out.push((pointer.clone(), path.clone()));
        }
        match value {
            J::Object(map) => {
                for (key, value) in map {
                    let escaped = key.replace('~', "~0").replace('/', "~1");
                    let path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    walk(value, format!("{pointer}/{escaped}"), path, actual, out);
                }
            }
            J::Array(items) => {
                for (i, value) in items.iter().enumerate() {
                    walk(
                        value,
                        format!("{pointer}/{i}"),
                        format!("{path}[{i}]"),
                        actual,
                        out,
                    );
                }
            }
            _ => {}
        }
    }
    walk(raw, String::new(), String::new(), actual, &mut candidates);
    candidates.into_iter().find_map(|(pointer, path)| {
        let mut probe = raw.clone();
        let at = probe.pointer_mut(&pointer)?;
        // Keep the JSON kind: changing an integer to a string could fix the error.
        *at = match &*at {
            J::String(s) => J::String(if s == "__core_error_path_probe__" {
                "__core_error_path_probe_2__".into()
            } else {
                "__core_error_path_probe__".into()
            }),
            J::Bool(b) => J::Bool(!b),
            J::Number(n) => {
                if let Some(n) = n.as_u64() {
                    J::from(if n == u64::MAX {
                        u64::MAX - 1
                    } else {
                        u64::MAX
                    })
                } else if let Some(n) = n.as_i64() {
                    J::from(if n == i64::MIN {
                        i64::MIN + 1
                    } else {
                        i64::MIN
                    })
                } else {
                    J::from(if n.as_f64()? == f64::MAX {
                        -f64::MAX
                    } else {
                        f64::MAX
                    })
                }
            }
            _ => return None,
        };
        let target = format!("{kind}: {}, expected {expected}", unexpected(at)?);
        T::deserialize(&probe)
            .err()
            .filter(|e| e.to_string() == target)
            .map(|_| path)
    })
}

/// Most objects [`missing_field_path`] tries: each try decodes the whole request again,
/// so without a bound one huge malformed request would cost quadratic time. Beyond it
/// the error simply has no `details.field`.
const MAX_PATH_PROBES: usize = 32;

/// Where the field `name` that serde reported missing belongs: serde does not say in
/// which object. Tries each object of `raw` that lacks it (outermost first, at most
/// [`MAX_PATH_PROBES`]): the one where adding it changes the outcome is the one that
/// needs it. Only runs for a failed request; `None` when it cannot tell.
fn missing_field_path<T: serde::de::DeserializeOwned>(
    raw: &serde_json::Value,
    name: &str,
) -> Option<String> {
    fn objects_without(
        value: &serde_json::Value,
        name: &str,
        path: &mut Vec<serde_json::Value>,
        out: &mut Vec<Vec<serde_json::Value>>,
    ) {
        match value {
            serde_json::Value::Object(map) => {
                if !map.contains_key(name) {
                    out.push(path.clone());
                }
                for (key, v) in map {
                    path.push(key.as_str().into());
                    objects_without(v, name, path, out);
                    path.pop();
                }
            }
            serde_json::Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    path.push(i.into());
                    objects_without(v, name, path, out);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut candidates = Vec::new();
    objects_without(raw, name, &mut Vec::new(), &mut candidates);
    candidates.truncate(MAX_PATH_PROBES);
    let still_missing = |message: &str| {
        message.starts_with(&format!("missing field `{name}`"))
            || message.starts_with(&format!("unknown field `{name}`"))
    };
    candidates.into_iter().find_map(|path| {
        let mut probe = raw.clone();
        let mut at = &mut probe;
        for seg in &path {
            at = match seg {
                serde_json::Value::String(key) => &mut at[key.as_str()],
                other => &mut at[other.as_u64().unwrap_or_default() as usize],
            };
        }
        at.as_object_mut()?
            .insert(name.to_owned(), serde_json::Value::Null);
        let changed = match T::deserialize(&probe) {
            Ok(_) => true,
            Err(e) => !still_missing(&e.to_string()),
        };
        changed.then(|| {
            let mut out = String::new();
            for seg in path.iter().map(Some).chain([None]) {
                match seg {
                    Some(serde_json::Value::String(key)) => {
                        if !out.is_empty() {
                            out.push('.');
                        }
                        out.push_str(key);
                    }
                    Some(index) => out.push_str(&format!("[{index}]")),
                    None => {
                        if !out.is_empty() {
                            out.push('.');
                        }
                        out.push_str(name);
                    }
                }
            }
            out
        })
    })
}

/// The path of the first object key `name` in `value`.
fn find_key(value: &serde_json::Value, name: &str, path: String) -> Option<String> {
    let join = |key: &str| {
        if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        }
    };
    match value {
        serde_json::Value::Object(map) => {
            if map.contains_key(name) {
                return Some(join(name));
            }
            map.iter().find_map(|(k, v)| find_key(v, name, join(k)))
        }
        serde_json::Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, v)| find_key(v, name, format!("{path}[{i}]"))),
        _ => None,
    }
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests;
