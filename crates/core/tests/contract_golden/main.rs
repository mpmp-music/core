//! Golden JSON examples of contract v1 and the compatibility tests built on them
//! (docs/api-v1-freeze.md; how to change the contract: tests/golden/README.md).
//!
//! - `golden_files_are_up_to_date`: the files are what the Rust values serialize to
//!   (`UPDATE_GOLDEN=1` rewrites them).
//! - `every_variant_and_optional_field_has_a_golden`: nothing in the schema lacks one.
//! - `golden_files_round_trip_byte_for_byte`, `golden_files_match_the_schema`.
//! - `old_clients_read_new_results_and_events`: core → client is lenient.
//! - `unknown_fields_and_variants_in_requests_are_refused`: client → core is strict.
//! - `frozen_messages_are_still_understood`, `frozen_schema_only_grew`: the
//!   freeze guard.

mod examples;
mod freeze;
mod frozen_manifest;
mod integrity;
mod schema_walk;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mpmp_core::contract::Origin;
use mpmp_core::{Catalog, CoreConfig, CoreHandle};
use mpmp_engine::NullOutput;
use mpmp_engine::output::{SampleType, StreamFormat};
use mpmp_media_io::FileResolver;
use serde_json::{Value as J, json};

use examples::Kind;
use schema_walk::{SchemaRoot, Seg, field_path, get_mut};

/// Every frozen set later cores must still understand. Never replace an older set.
const FROZEN_SETS: &[&str] = &["v1.0"];

fn frozen_schema(set: &str) -> J {
    serde_json::from_str(&read(&golden_dir().join(set).join("contract.schema.json")))
        .unwrap_or_else(|e| panic!("{set}/contract.schema.json: {e}"))
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        // Line endings may differ on a Windows checkout; the content may not.
        .replace("\r\n", "\n")
}

/// docs/contract.schema.json, as clients get it.
fn schema() -> J {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/contract.schema.json");
    serde_json::from_str(&read(&path)).unwrap()
}

struct File {
    kind: Kind,
    /// `request/play_album.json`
    path: String,
    text: String,
}

impl File {
    fn json(&self) -> J {
        serde_json::from_str(&self.text).unwrap_or_else(|e| panic!("{}: {e}", self.path))
    }
}

/// The golden files of one set (`current`, `v1.0` …), in a stable order.
fn read_set(set: &str) -> Vec<File> {
    let mut files = Vec::new();
    for kind in Kind::ALL {
        let dir = golden_dir().join(set).join(kind.dir());
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<_> = entries
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".json"))
            .collect();
        names.sort();
        for name in names {
            files.push(File {
                kind,
                path: format!("{}/{name}", kind.dir()),
                text: read(&dir.join(&name)),
            });
        }
    }
    assert!(!files.is_empty(), "no golden files in {set}");
    files
}

/// A validator per root, as the schema's `$comment` tells clients to build one: the
/// document itself, pointed at the root (`{"$ref": "#/Request"}`), so that the root's
/// `$ref`s resolve in the shared `$defs`.
fn validators(schema: &J) -> Vec<(Kind, jsonschema::Validator)> {
    Kind::ALL
        .iter()
        .map(|&kind| {
            let mut document = schema.clone();
            document["$ref"] = format!("#/{}", kind.schema_root()).into();
            let validator = jsonschema::validator_for(&document)
                .unwrap_or_else(|e| panic!("{}: {e}", kind.schema_root()));
            (kind, validator)
        })
        .collect()
}

fn schema_errors(
    validators: &[(Kind, jsonschema::Validator)],
    kind: Kind,
    json: &J,
) -> Vec<String> {
    let (_, validator) = validators.iter().find(|(k, _)| *k == kind).unwrap();
    validator
        .iter_errors(json)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect()
}

/// A core over an empty library in a temporary directory.
struct TestCore {
    core: CoreHandle,
    _dir: tempfile::TempDir,
}

impl TestCore {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let library = mpmp_library::Library::open(&dir.path().join("library.db")).unwrap();
        let output = NullOutput::new(StreamFormat {
            sample_rate: 44_100,
            channels: 2,
            sample_type: SampleType::F32,
        });
        let core = CoreHandle::start(
            Box::new(output),
            Arc::new(FileResolver),
            Arc::new(library) as Arc<dyn Catalog>,
            CoreConfig::default(),
        )
        .unwrap();
        Self { core, _dir: dir }
    }

    /// Sends a request or query; returns `(result, error)` of the answer.
    fn send(&self, kind: Kind, json: &J, origin: Origin) -> (J, J) {
        let text = json.to_string();
        match kind {
            Kind::Request => {
                let response: J =
                    serde_json::from_str(&self.core.execute_json(&text, origin)).unwrap();
                assert_eq!(response["request_id"], json["request_id"], "{response}");
                let result = response["result"].clone();
                let error = result["error"].clone();
                (result, error)
            }
            Kind::Query => {
                let result: J = serde_json::from_str(&self.core.query_json(&text)).unwrap();
                let error = result["error"].clone();
                (result, error)
            }
            _ => unreachable!("only requests and queries go to core"),
        }
    }
}

/// Every value of `old` is in `new` at the same place (new may have more fields).
fn covers(new: &J, old: &J, path: &str, out: &mut Vec<String>) {
    match (old, new) {
        (J::Object(o), J::Object(n)) => {
            for (key, value) in o {
                match n.get(key) {
                    Some(v) => covers(v, value, &format!("{path}.{key}"), out),
                    None => out.push(format!("{path}.{key} was dropped")),
                }
            }
        }
        (J::Array(o), J::Array(n)) if o.len() == n.len() => {
            for (i, (o, n)) in o.iter().zip(n).enumerate() {
                covers(n, o, &format!("{path}[{i}]"), out);
            }
        }
        _ if old == new => {}
        _ => out.push(format!("{path}: {old} became {new}")),
    }
}

// --- the files ---

/// The golden files are exactly what the Rust values serialize to. Regenerate with
/// `UPDATE_GOLDEN=1 cargo test -p mpmp-core --test contract_golden`.
#[test]
fn golden_files_are_up_to_date() {
    let current = golden_dir().join("current");
    let goldens = examples::all();
    let mut names = BTreeSet::new();
    for golden in &goldens {
        assert!(names.insert(golden.path()), "{} twice", golden.path());
    }
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        for kind in Kind::ALL {
            let dir = current.join(kind.dir());
            std::fs::create_dir_all(&dir).unwrap();
            for entry in std::fs::read_dir(&dir).unwrap() {
                let name = entry.unwrap().file_name().into_string().unwrap();
                if name.ends_with(".json") && !names.contains(&format!("{}/{name}", kind.dir())) {
                    std::fs::remove_file(dir.join(name)).unwrap();
                }
            }
        }
        for golden in &goldens {
            std::fs::write(current.join(golden.path()), golden.text()).unwrap();
        }
    }
    let mut stale = Vec::new();
    for golden in &goldens {
        let saved = std::fs::read_to_string(current.join(golden.path()))
            .unwrap_or_default()
            .replace("\r\n", "\n");
        if saved != golden.text() {
            stale.push(golden.path());
        }
    }
    for file in read_set("current") {
        if !names.contains(&file.path) {
            stale.push(format!("{} (no longer generated)", file.path));
        }
    }
    assert!(
        stale.is_empty(),
        concat!(
            "stale golden files (run UPDATE_GOLDEN=1 cargo test -p mpmp-core --test ",
            "contract_golden): {stale:#?}",
        ),
        stale = stale,
    );
}

/// Every golden file decodes into its Rust type and encodes back to the same bytes: the
/// files are canonical (fields in declaration order, pretty-printed) and nothing is lost.
#[test]
fn golden_files_round_trip_byte_for_byte() {
    for file in read_set("current") {
        let again = file
            .kind
            .reencode_text(&file.text)
            .unwrap_or_else(|e| panic!("{}: {e}", file.path));
        assert!(
            again == file.text,
            "{} does not round-trip:\n{again}",
            file.path
        );
    }
}

/// Every golden file, current and frozen, is valid under docs/contract.schema.json (a
/// third-party validator, as clients in other languages would use).
#[test]
fn golden_files_match_the_schema() {
    let schema = schema();
    let validators = validators(&schema);
    for set in std::iter::once("current").chain(FROZEN_SETS.iter().copied()) {
        for file in read_set(set) {
            let errors = schema_errors(&validators, file.kind, &file.json());
            assert!(errors.is_empty(), "{set}/{}: {errors:#?}", file.path);
        }
    }
    // The validator does check: a request with a field the schema does not know fails
    // (the schema closes the outermost request objects; core checks every depth).
    let errors = schema_errors(
        &validators,
        Kind::Request,
        &json!({"request_id": 1, "command": {"type": "pause"}, "fade_ms": 5}),
    );
    assert!(!errors.is_empty());
}

/// Every variant of every enum (but the `unknown` fallbacks) and every optional field of
/// the schema appears in some golden file; a new command, event, variant or optional field
/// needs an example. Error codes are an open set and exempt (they are checked against the
/// library's table in `contract.rs`).
#[test]
fn every_variant_and_optional_field_has_a_golden() {
    let schema = schema();
    let mut coverable = BTreeSet::new();
    let mut covered = BTreeSet::new();
    for kind in Kind::ALL {
        coverable.extend(SchemaRoot::new(&schema, kind.schema_root()).coverable());
    }
    for file in read_set("current") {
        let walk = SchemaRoot::new(&schema, file.kind.schema_root()).walk(&file.json());
        covered.extend(walk.covered);
    }
    let missing: Vec<_> = coverable
        .difference(&covered)
        .filter(|name| !name.starts_with("ErrorCode."))
        .collect();
    assert!(missing.is_empty(), "no golden shows: {missing:#?}");
}

// --- compatibility ---

const FUTURE_FIELD: &str = "x_future_field";

/// Old client reads new core: a result or event with a field this build does not know (at
/// any object) decodes, the field ignored; one with an enum value this build does not know
/// (at any enum position) decodes too, that value — and only it — as `unknown`.
#[test]
fn old_clients_read_new_results_and_events() {
    let schema = schema();
    let validators = validators(&schema);
    let mut checked = (0, 0);
    for file in read_set("current") {
        if file.kind.is_request_side() {
            continue;
        }
        let original = file.json();
        let walk = SchemaRoot::new(&schema, file.kind.schema_root()).walk(&original);
        assert!(
            !walk.objects.is_empty() && !walk.enums.is_empty(),
            "{}",
            file.path
        );
        for object in &walk.objects {
            let mut message = original.clone();
            get_mut(&mut message, object)
                .as_object_mut()
                .unwrap()
                .insert(
                    FUTURE_FIELD.into(),
                    json!({"nested": [1, "two", null, {"type": "x"}], "flag": true}),
                );
            let at = format!("{} at {:?}", file.path, field_path(object));
            let decoded = file
                .kind
                .reencode(&message)
                .unwrap_or_else(|e| panic!("{at}: a new field broke decoding: {e}"));
            assert_eq!(decoded, original, "{at}");
            // The core → client schema is open: a validating client accepts it too.
            let errors = schema_errors(&validators, file.kind, &message);
            assert!(errors.is_empty(), "{at}: {errors:#?}");
            checked.0 += 1;
        }
        for position in &walk.enums {
            let mut message = original.clone();
            let mut expected = original.clone();
            let target = get_mut(&mut message, &position.path);
            if position.tagged {
                let object = target.as_object_mut().unwrap();
                object.insert("type".into(), "x_future_variant".into());
                object.insert(FUTURE_FIELD.into(), json!({"a": 1}));
                *get_mut(&mut expected, &position.path) = json!({"type": "unknown"});
            } else {
                *target = "x_future_value".into();
                *get_mut(&mut expected, &position.path) = "unknown".into();
            }
            let at = format!(
                "{} at {:?} ({})",
                file.path,
                field_path(&position.path),
                position.name
            );
            let decoded = file
                .kind
                .reencode(&message)
                .unwrap_or_else(|e| panic!("{at}: a new variant broke decoding: {e}"));
            assert_eq!(decoded, expected, "{at}");
            checked.1 += 1;
        }
    }
    // Sanity: the walk found the positions.
    assert!(checked.0 > 300 && checked.1 > 150, "{checked:?}");
}

/// New client to old core: a request or query with a field this core does not know, at
/// any object, is refused as `unsupported` / `unknown_field` with `details.field` the path
/// to it; an unknown command, query or enum value is refused too. Nothing is run
/// half-understood. As sent, every golden request passes the strict parser.
#[test]
fn unknown_fields_and_variants_in_requests_are_refused() {
    let schema = schema();
    let core = TestCore::new();
    let mut failures = Vec::new();
    let mut checked = 0;
    for file in read_set("current") {
        if !file.kind.is_request_side() {
            continue;
        }
        let original = file.json();
        accepted_as_sent(&core, &file, &original, &mut failures);
        let walk = SchemaRoot::new(&schema, file.kind.schema_root()).walk(&original);
        for object in &walk.objects {
            let mut message = original.clone();
            get_mut(&mut message, object)
                .as_object_mut()
                .unwrap()
                .insert("x_new_field".into(), json!(1));
            let mut path = object.clone();
            path.push(Seg::Key("x_new_field".into()));
            let expected = field_path(&path);
            let (result, error) = core.send(file.kind, &message, Origin::Ui);
            if (
                error["kind"].as_str(),
                error["code"].as_str(),
                error["details"]["field"].as_str(),
            ) != (Some("unsupported"), Some("unknown_field"), Some(&expected))
            {
                failures.push(format!("{}: {expected}: {result}", file.path));
            }
            checked += 1;
        }
        for (n, position) in walk.enums.iter().enumerate() {
            let mut message = original.clone();
            // A retry with the same key and another command would be
            // `idempotency_key_reused`: each variant gets a key of its own.
            if let Some(key) = message["idempotency_key"].as_str() {
                message["idempotency_key"] = format!("{key}-variant-{n}").into();
            }
            let target = get_mut(&mut message, &position.path);
            if position.tagged {
                target["type"] = "x_new_variant".into();
            } else {
                *target = "x_new_variant".into();
            }
            let at = field_path(&position.path);
            let code = match (file.kind, at.as_str()) {
                (Kind::Request, "command") => "unknown_command",
                (Kind::Query, "") => "unknown_query",
                _ => "unknown_variant",
            };
            let (result, error) = core.send(file.kind, &message, Origin::Ui);
            let field_ok = match error["details"]["field"].as_str() {
                None => code != "unknown_variant",
                Some(field) => field == at,
            };
            if (error["kind"].as_str(), error["code"].as_str()) != (Some("unsupported"), Some(code))
                || !field_ok
            {
                failures.push(format!(
                    "{}: {} at {at:?}: expected {code}: {result}",
                    file.path, position.name
                ));
            }
            checked += 1;
        }
    }
    assert!(
        failures.is_empty(),
        "{checked} checks, failures: {failures:#?}"
    );
    assert!(checked > 400, "{checked}");
}

/// A golden request or query as sent is understood. Requests are sent with the `unknown`
/// origin, which every command refuses (`forbidden` / `origin_not_allowed`) after parsing,
/// so nothing runs; queries run on the empty library, where only `not_found` may fail.
fn accepted_as_sent(core: &TestCore, file: &File, json: &J, failures: &mut Vec<String>) {
    let (result, error) = core.send(file.kind, json, Origin::Unknown);
    let ok = match file.kind {
        Kind::Request => error["code"] == "origin_not_allowed",
        _ => result["type"] != "err" || error["kind"] == "not_found",
    };
    if !ok {
        failures.push(format!("{}: not accepted as sent: {result}", file.path));
    }
}

// --- the freeze guard ---

/// Every message of every frozen set is still understood: it decodes into the current
/// Rust type and encodes back with every value it had (nothing dropped, renamed or turned
/// into `unknown`), it is valid under the current schema, and core accepts each request
/// and query of it.
#[test]
fn frozen_messages_are_still_understood() {
    let schema = schema();
    let validators = validators(&schema);
    for set in FROZEN_SETS {
        let core = TestCore::new();
        let mut failures = Vec::new();
        for mut file in read_set(set) {
            file.path = format!("{set}/{}", file.path);
            let frozen = file.json();
            match file.kind.reencode(&frozen) {
                Ok(decoded) => covers(&decoded, &frozen, &file.path, &mut failures),
                Err(e) => failures.push(format!("{}: no longer decodes: {e}", file.path)),
            }
            for error in schema_errors(&validators, file.kind, &frozen) {
                failures.push(format!("{}: {error}", file.path));
            }
            if file.kind.is_request_side() {
                accepted_as_sent(&core, &file, &frozen, &mut failures);
            }
        }
        assert!(
            failures.is_empty(),
            concat!(
                "the contract changed in a way {set} clients notice (see ",
                "tests/golden/README.md): {failures:#?}",
            ),
            set = set,
            failures = failures,
        );
    }
}

/// The current schema only grew since every frozen set: no variant, field or definition removed or
/// renamed, no field's type or default changed, no request field newly required, no
/// result field no longer always sent, no command's policy tightened, no limit lowered.
#[test]
fn frozen_schema_only_grew() {
    for set in FROZEN_SETS {
        let frozen = frozen_schema(set);
        let changes = freeze::breaking_changes(&frozen, &schema());
        assert!(
            changes.is_empty(),
            "non-additive contract changes since {set} (see tests/golden/README.md): {changes:#?}"
        );
        // The guard trips on what it should.
        let mut broken = schema();
        let command = broken["$defs"]["Command"]["oneOf"].as_array_mut().unwrap();
        command.retain(|v| mpmp_core::contract::variant_tag(v) != Some("seek"));
        let pause = command
            .iter_mut()
            .find(|v| mpmp_core::contract::variant_tag(v) == Some("pause"))
            .unwrap();
        pause["x-origins"]["media_controls"] = "confirm".into();
        broken["$defs"]["TrackInfo"]["required"]
            .as_array_mut()
            .unwrap()
            .retain(|f| f != "title");
        broken["$defs"]["Command"]["oneOf"][0]["required"]
            .as_array_mut()
            .unwrap()
            .push("start".into());
        broken["limits"]["max_page"] = 10.into();
        broken["$defs"]["EntryId"]["format"] = "uint32".into();
        let changes = freeze::breaking_changes(&frozen, &broken);
        for needle in [
            "Command.seek: variant removed",
            "Command.pause: policy for media_controls tightened",
            "TrackInfo.title is no longer always sent",
            ".start became required in a request",
            "limits.max_page went down",
            "$defs.EntryId: format changed",
        ] {
            assert!(
                changes.iter().any(|c| c.contains(needle)),
                "{set}: {needle}: {changes:#?}"
            );
        }
    }
}

/// The guard also sees changes of scalar definitions and of
/// string enum values, which have no properties to compare.
#[test]
fn freeze_guard_sees_scalar_and_enum_value_changes() {
    for set in FROZEN_SETS {
        let frozen = frozen_schema(set);
        let mut broken = schema();
        // An id changes its integer format (Kotlin `Long` → `ULong`) or its JSON type.
        let track = broken["$defs"]["TrackId"].as_object_mut().unwrap();
        track.insert("format".into(), "uint64".into());
        track.remove("minimum");
        let root = broken["$defs"]["RootId"].as_object_mut().unwrap();
        root.insert("type".into(), "string".into());
        root.remove("format");
        // A string enum value turns into a tagged object.
        let insert = broken["$defs"]["InsertAt"]["oneOf"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|v| v["const"] == "next")
            .unwrap();
        *insert = json!({
        "type": "object",
         "properties": {
        "type": {
        "const": "next",
         "type": "string"}
        }
        ,

                         "required": ["type"], "description": "x"});
        let changes = freeze::breaking_changes(&frozen, &broken);
        for needle in [
            "$defs.TrackId: format changed",
            "$defs.TrackId: minimum changed",
            "$defs.RootId: type changed",
            "$defs.InsertAt.next: type changed",
        ] {
            assert!(
                changes.iter().any(|c| c.contains(needle)),
                "{set}: {needle}: {changes:#?}"
            );
        }

        // A command's risk tier is informative (guarantee 12): reclassifying it alone, with
        // its policies unchanged, is not a breaking change. Its policies are guarded.
        let mut reclassified = frozen.clone();
        let rate = reclassified["$defs"]["Command"]["oneOf"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|v| mpmp_core::contract::variant_tag(v) == Some("set_rating"))
            .unwrap();
        rate["x-risk"] = "destructive".into();
        assert_eq!(
            freeze::breaking_changes(&frozen, &reclassified),
            Vec::<String>::new(),
            "{set}: risk reclassification alone must remain additive"
        );
    }
}

/// Frozen capability names survive, and generated snapshots advertise the real core.
#[test]
fn snapshot_advertises_frozen_minor_features() {
    let core = TestCore::new();
    let (live, _) = core.send(Kind::Query, &json!({"type": "snapshot"}), Origin::Ui);
    let live = &live["snapshot"];
    for set in std::iter::once("current").chain(FROZEN_SETS.iter().copied()) {
        let frozen: J = serde_json::from_str(&read(
            &golden_dir().join(set).join("query_result/snapshot.json"),
        ))
        .unwrap();
        let frozen = &frozen["snapshot"];
        assert_eq!(
            live["contract_version"], frozen["contract_version"],
            "{set}"
        );
        assert!(
            live["contract_minor"].as_u64().unwrap() >= frozen["contract_minor"].as_u64().unwrap(),
            "{set}"
        );
        assert!(
            freeze::feature_changes(frozen, live).is_empty(),
            "{set}: {:?}",
            freeze::feature_changes(frozen, live)
        );
        for feature in frozen["features"].as_array().unwrap() {
            assert!(
                live["features"].as_array().unwrap().contains(feature),
                "{set}: missing {feature}"
            );
        }
        if set == "current" {
            assert_eq!(
                live["features"], frozen["features"],
                "current snapshot features diverged from the core"
            );
        }
    }
}

#[test]
fn query_error_goldens_match_live_diagnostics() {
    let core = TestCore::new();
    for (name, raw) in [
        (
            "err_query_scalar_path",
            r#"{"type":"tracks","query":{"sort":[{"field":{"type":"title"},"descending":"ten"}]}}"#,
        ),
        (
            "err_query_json_position",
            "{\n\"type\":\"tracks\",\"offset\":0,,}",
        ),
    ] {
        let live: J = serde_json::from_str(&core.core.query_json(raw)).unwrap();
        let golden: J = serde_json::from_str(&read(
            &golden_dir()
                .join("current/query_result")
                .join(format!("{name}.json")),
        ))
        .unwrap();
        assert_eq!(live, golden, "{name}");
    }
}

#[test]
fn freeze_guard_checks_frozen_command_metadata() {
    for set in FROZEN_SETS {
        let frozen = frozen_schema(set);
        let commands = frozen["$defs"]["Command"]["oneOf"].as_array().unwrap();
        let mut checked = 0;
        for (index, command) in commands.iter().enumerate() {
            let name = mpmp_core::contract::variant_tag(command).unwrap();
            for key in ["x-class", "x-local-path"] {
                let Some(old) = command.get(key) else {
                    continue;
                };
                for remove in [false, true] {
                    let mut broken = frozen.clone();
                    broken["contract_minor"] =
                        json!(frozen["contract_minor"].as_u64().unwrap() + 1);
                    let fields = broken["$defs"]["Command"]["oneOf"][index]
                        .as_object_mut()
                        .unwrap();
                    if remove {
                        fields.remove(key);
                    } else {
                        let changed = if key == "x-local-path" {
                            json!(!old.as_bool().unwrap())
                        } else if old == "file_write" {
                            json!("playback")
                        } else {
                            json!("file_write")
                        };
                        assert_ne!(&changed, old);
                        fields.insert(key.into(), changed);
                    }
                    let expected = format!("$defs.Command.{name}: {key} changed");
                    assert!(
                        freeze::breaking_changes(&frozen, &broken).contains(&expected),
                        "{set}: {name}: {key}: remove={remove}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "{set}: no command metadata exercised");
    }
}

#[test]
fn freeze_guard_requires_a_new_minor_for_new_fields_and_queries() {
    for set in FROZEN_SETS {
        let frozen = frozen_schema(set);
        for kind in ["field", "query"] {
            let mut added = frozen.clone();
            if kind == "field" {
                added["$defs"]["ErrorDetails"]["properties"]["future"] = json!({"type":"string"});
            } else {
                added["Query"]["oneOf"].as_array_mut().unwrap().push(json!({
                "type":"object",
                "properties":{
                "type":{
                "const":"future_query",
                "type":"string"}
                }
                ,
                "required":["type"],
                "additionalProperties":false}
                ));
            }
            assert!(
                freeze::breaking_changes(&frozen, &added)
                    .iter()
                    .any(|e| e.contains("without raising contract_minor")),
                "{kind}"
            );
            added["contract_minor"] = json!(frozen["contract_minor"].as_u64().unwrap() + 1);
            assert!(
                freeze::breaking_changes(&frozen, &added).is_empty(),
                "{kind}"
            );
        }
        let mut docs = frozen.clone();
        docs["description"] = json!("clearer documentation");
        docs["$defs"]["Command"]["oneOf"][0]["x-risk"] = json!("new explanatory label");
        assert!(freeze::breaking_changes(&frozen, &docs).is_empty());
    }
}

#[test]
fn freeze_guard_checks_error_code_kinds() {
    for set in FROZEN_SETS {
        let frozen = frozen_schema(set);
        for (index, code) in frozen["$defs"]["ErrorCode"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let Some(old) = code.get("x-kind") else {
                continue;
            };
            let name = code["const"].as_str().unwrap();
            for remove in [false, true] {
                let mut broken = frozen.clone();
                // Raise the minor so the per-value check must catch this independently.
                broken["contract_minor"] = json!(frozen["contract_minor"].as_u64().unwrap() + 1);
                let fields = broken["$defs"]["ErrorCode"]["oneOf"][index]
                    .as_object_mut()
                    .unwrap();
                if remove {
                    fields.remove("x-kind");
                } else {
                    fields.insert(
                        "x-kind".into(),
                        if old == "failed" {
                            json!("invalid")
                        } else {
                            json!("failed")
                        },
                    );
                }
                let expected = format!("$defs.ErrorCode.{name}: x-kind changed");
                assert!(
                    freeze::breaking_changes(&frozen, &broken).contains(&expected),
                    "{set}: {name}: remove={remove}"
                );
            }
        }
    }
}

#[test]
fn documented_error_code_kinds_are_complete_and_current() {
    let schema = schema();
    let code_kinds: std::collections::BTreeMap<String, String> =
        schema["$defs"]["ErrorCode"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|branch| {
                let code = branch["const"]
                    .as_str()
                    .expect("one annotated branch per code");
                let kind = branch["x-kind"].as_str().expect("every code has x-kind");
                assert_eq!(
                    serde_json::to_value(mpmp_core::contract::ErrorCode::parse(code).kind())
                        .unwrap(),
                    kind
                );
                (code.to_owned(), kind.to_owned())
            })
            .collect();
    let docs = include_str!("../../../../docs/api-v1-changes.md");
    let table = docs
        .split_once("<!-- error-kinds:start -->")
        .unwrap()
        .1
        .split_once("<!-- error-kinds:end -->")
        .unwrap()
        .0;
    let mut documented = std::collections::BTreeMap::new();
    for line in table.lines().filter(|line| line.starts_with("| ")) {
        let cells: Vec<_> = line.split('|').map(str::trim).collect();
        if cells[1] == "Kind" {
            continue;
        }
        for code in cells[2]
            .split(',')
            .map(|code| code.trim().trim_matches('`'))
        {
            assert!(
                documented
                    .insert(code.to_owned(), cells[1].to_owned())
                    .is_none()
            );
        }
    }
    assert_eq!(documented, code_kinds);
}

#[test]
fn descriptions_can_split_string_enum_groups_without_a_new_minor() {
    let frozen = frozen_schema("v1.0");
    let mut clearer = frozen.clone();
    let branches = clearer["$defs"]["ActionKind"]["oneOf"]
        .as_array_mut()
        .unwrap();
    let group = branches
        .iter_mut()
        .find(|b| b["enum"].as_array().is_some_and(|v| v.len() > 1))
        .unwrap();
    let tag = group["enum"].as_array_mut().unwrap().pop().unwrap();
    branches.push(json!({
    "type":"string",
    "const":tag,
    "description":"Clearer explanation of this existing value"}
    ));

    assert!(freeze::breaking_changes(&frozen, &clearer).is_empty());
}

#[test]
fn freeze_guard_requires_a_new_minor_for_new_features() {
    for set in FROZEN_SETS {
        let frozen: J = serde_json::from_str(&read(
            &golden_dir().join(set).join("query_result/snapshot.json"),
        ))
        .unwrap();
        let frozen = &frozen["snapshot"];
        let mut added = frozen.clone();
        added["features"]
            .as_array_mut()
            .unwrap()
            .push(json!("future_feature"));
        assert!(!freeze::feature_changes(frozen, &added).is_empty());
        added["contract_minor"] = json!(frozen["contract_minor"].as_u64().unwrap() + 1);
        assert!(freeze::feature_changes(frozen, &added).is_empty());
        // Every baseline capability must survive future compatible additions.
        for index in 0..frozen["features"].as_array().unwrap().len() {
            let mut removed = frozen.clone();
            removed["contract_minor"] = json!(frozen["contract_minor"].as_u64().unwrap() + 1);
            removed["features"].as_array_mut().unwrap().remove(index);
            assert!(
                !freeze::feature_changes(frozen, &removed).is_empty(),
                "{set}: removed feature {index}"
            );
        }
    }
}

#[test]
fn freeze_guard_rejects_new_error_kinds_even_with_a_new_minor() {
    for set in FROZEN_SETS {
        let frozen = frozen_schema(set);
        let mut added = frozen.clone();
        added["contract_minor"] = json!(frozen["contract_minor"].as_u64().unwrap() + 1);
        let kind = &mut added["$defs"]["ErrorKind"];
        if let Some(values) = kind["enum"].as_array_mut() {
            values.push(json!("future_kind"));
        } else {
            kind["oneOf"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"string","const":"future_kind"}));
        }
        assert!(
            freeze::breaking_changes(&frozen, &added)
                .iter()
                .any(|e| e.contains("ErrorKind is closed")),
            "{set}"
        );
    }
}

#[test]
fn freeze_guard_ignores_annotations_consistently_but_preserves_field_names() {
    let frozen = frozen_schema("v1.0");
    for minor in [0, 1] {
        let mut docs = frozen.clone();
        docs["contract_minor"] = json!(minor);
        for annotation in ["description", "title", "$comment", "x-risk"] {
            docs[annotation] = json!("clarification");
            docs["$defs"]["ErrorDetails"]["properties"]["field"][annotation] =
                json!("clarification");
        }
        assert!(freeze::breaking_changes(&frozen, &docs).is_empty());
    }
    for name in ["description", "title", "$comment", "x-risk"] {
        let before = json!({"properties":{name:{"type":"string"}}});
        let after = json!({"properties":{name:{"type":"integer"}}});
        assert_ne!(
            freeze::normalized_shape(&before),
            freeze::normalized_shape(&after)
        );
    }
}

#[test]
fn api_1_0_features_match_documentation_and_current_snapshot() {
    let core = TestCore::new();
    let (live, _) = core.send(Kind::Query, &json!({"type":"snapshot"}), Origin::Ui);
    let current: J = serde_json::from_str(&read(
        &golden_dir().join("current/query_result/snapshot.json"),
    ))
    .unwrap();
    let features = |snapshot: &J| -> BTreeSet<String> {
        snapshot["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    };
    let inventory = include_str!("../../src/guarantees.md")
        .split_once("<!-- api-1.0-features:start -->")
        .unwrap()
        .1
        .split_once("<!-- api-1.0-features:end -->")
        .unwrap()
        .0;
    let documented: BTreeSet<_> = inventory
        .lines()
        .filter_map(|line| line.strip_prefix("- `")?.strip_suffix('`'))
        .map(str::to_owned)
        .collect();
    assert_eq!(documented.len(), 23);
    assert_eq!(features(&live["snapshot"]), documented);
    assert_eq!(features(&current["snapshot"]), documented);
    assert_eq!(live["snapshot"]["contract_version"], 1);
    assert_eq!(live["snapshot"]["contract_minor"], 0);
}

#[test]
fn freeze_guard_rejects_removing_api_1_0_fields() {
    let baseline = frozen_schema("v1.0");
    for (definition, field) in [
        ("TagFilePreview", "changed"),
        ("ActionSummary", "details"),
        ("TrackInfo", "artists"),
        ("TrackInfo", "cover_revision"),
        ("Snapshot", "playback_limits"),
        ("Snapshot", "output"),
    ] {
        let mut broken = baseline.clone();
        broken["contract_minor"] = json!(1);
        assert!(
            broken["$defs"][definition]["properties"]
                .as_object_mut()
                .unwrap()
                .remove(field)
                .is_some()
        );
        let expected = format!("$defs.{definition}.{field} was removed or renamed");
        assert!(
            freeze::breaking_changes(&baseline, &broken).contains(&expected),
            "{expected}"
        );
    }
}
