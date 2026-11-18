use super::*;
use serde_json::json;

#[test]
fn commands_and_events_have_readable_json() {
    let request = Request::new(
        7,
        Command::Enqueue {
            tracks: vec![TrackId(3)],
            at: InsertAt::Next,
            expected_revision: Some(QueueRevision(12)),
            expected_current: None,
        },
    );
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        json!({
            "request_id": 7,
            "idempotency_key": null,
            "command": {
            "type": "enqueue",
             "tracks": [3],
             "at": "next",
             "expected_revision": 12,
             "expected_current": null}

        })
    );
    let event: Event = serde_json::from_str(
        r#"{"type":"queue_changed","queue":{"revision":4,"entry_count":2,"current_index":null}}"#,
    )
    .unwrap();
    assert_eq!(
        event,
        Event::QueueChanged {
            queue: QueueSummary {
                revision: QueueRevision(4),
                entry_count: 2,
                current_index: None,
                ..Default::default()
            }
        }
    );
    // Optional request fields may be omitted.
    let seek: Command = serde_json::from_str(r#"{"type":"seek","position_ms":5}"#).unwrap();
    assert_eq!(
        seek,
        Command::Seek {
            position_ms: 5,
            entry: None
        }
    );
    assert_eq!(
        serde_json::to_value(Command::Seek {
            position_ms: 5,
            entry: Some(EntryId(9))
        })
        .unwrap(),
        json!({"type": "seek", "position_ms": 5, "entry": 9})
    );
}

#[test]
fn unknown_variants_decode_at_every_level() {
    let event: Event = serde_json::from_str(r#"{"type":"lyrics_changed","line":3}"#).unwrap();
    assert_eq!(event, Event::Unknown);

    // A snapshot with a newer play state, origin and sample format.
    let mut snapshot = serde_json::to_value(sample_snapshot()).unwrap();
    snapshot["state"] = json!("buffering");
    snapshot["repeat"] = json!("shuffle_albums");
    snapshot["audio"]["volume_mode"] = json!("exclusive");
    snapshot["current"]["signal_path"]["output_format"] = json!("dsd");
    snapshot["current"]["signal_path"]["bit_perfect"] = json!({"type": "maybe", "p": 0.5});
    let envelope = json!({
        "seq": 3,
        "origin": {"type": "scheduler", "rule": 1},
        "event": {"type": "state_changed", "snapshot": snapshot},
    });
    let envelope: EventEnvelope = serde_json::from_value(envelope).unwrap();
    assert_eq!(envelope.origin, Some(Origin::Unknown));
    let Event::StateChanged { snapshot } = envelope.event else {
        panic!()
    };
    assert_eq!(snapshot.state, PlayState::Unknown);
    assert_eq!(snapshot.repeat, RepeatMode::Unknown);
    assert_eq!(snapshot.audio.volume_mode, VolumeMode::Unknown);
    let path = snapshot.current.unwrap().signal_path.unwrap();
    assert_eq!(path.output_format, SampleFormat::Unknown);
    assert_eq!(path.bit_perfect, BitPerfectInfo::Unknown);

    // A confirmation of a command, query and value kind this build does not know.
    let event: Event = serde_json::from_value(json!({
        "type": "confirmation_requested",
        "confirmation": {
            "token": "t", "summary": "s", "expires_at_ms": 1,
            "requested_by": {"type": "agent", "name": "a"},
            "command": {"type": "create_playlist", "name": "x", "kind": {"type": "smart",
                "query": {"filter": {"type": "and", "all": [
                    {"type": "compare", "field": {"type": "mood"}, "op": "near",
                     "value": {"type": "float", "value": 0.5}},
                    {"type": "fuzzy", "text": "x"}
                ]}}}}
        }
    }))
    .unwrap();
    let Event::ConfirmationRequested { confirmation } = event else {
        panic!()
    };
    let Command::CreatePlaylist {
        kind: PlaylistKind::Smart { query, .. },
        ..
    } = confirmation.command
    else {
        panic!()
    };
    assert_eq!(
        query.filter,
        Filter::And {
            all: vec![
                Filter::Compare {
                    field: Field::Unknown,
                    op: CompareOp::Unknown,
                    value: Value::Unknown
                },
                Filter::Unknown
            ]
        }
    );
    let unknown: Command = serde_json::from_str(r#"{"type":"teleport","to":3}"#).unwrap();
    assert_eq!(unknown, Command::Unknown);
    let unknown: Query =
        serde_json::from_str(r#"{"type":"no_such_query_in_any_version"}"#).unwrap();
    assert_eq!(unknown, Query::Unknown);
    let result: CommandResult = serde_json::from_str(r#"{"type":"partial","n":1}"#).unwrap();
    assert_eq!(result, CommandResult::Unknown);
    // An error of a newer kind and code still gives its message.
    let error: ContractError = serde_json::from_str(
        r#"{"kind":"throttled","code":"too_many_requests","message":"slow down","details":{"wait_ms":5}}"#,
    )
    .unwrap();
    assert_eq!(
        (error.kind, error.code, error.message.as_str()),
        (ErrorKind::Unknown, ErrorCode::Unknown, "slow down")
    );
    let job: Event = serde_json::from_value(json!({"type": "job_finished", "job": 1,
    "kind": "future_analysis_job",
     "outcome": {
    "type": "completed",
     "report": {
    "type": "future_analysis_job",
     "files": 3}
    }
    }
    ))
    .unwrap();
    assert_eq!(
        job,
        Event::JobFinished {
            job: JobId(1),
            kind: JobKind::Unknown,
            outcome: JobOutcome::Completed {
                report: JobReport::Unknown
            }
        }
    );
}

/// Request types also travel core → client (a `Command` in `ConfirmationRequested`, a
/// `TrackQuery` in a `PlaylistSummary`): a field a newer core adds to one must not
/// break an old client reading results and events. The JSON adapters
/// stay strict (`lib.rs`).
#[test]
fn results_and_events_with_new_fields_in_request_types_still_decode() {
    let playlists = json!({
        "type": "playlists", "offset": 0, "total": 1, "next_offset": null,
        "playlists": [{
            "id": 1, "name": "recent", "track_count": 0, "duration_ms": 0, "revision": 1,
            "created_at_ms": 0, "updated_at_ms": 0, "deleted_at_ms": null,
            "kind": {"type": "smart", "max_tracks": null, "boost": 2, "query": {
                "filter": {"type": "compare", "field": {"type": "year", "x": 1}, "op": "ge",
                           "value": {"type": "int", "value": 2000, "unit": "y"}, "y": 2},
                "sort": [{"field": {"type": "added_at"}, "descending": true,
                          "nulls_first": false}],
                "limit_per_album": 3}}
        }]
    });
    let QueryResult::Playlists { playlists, .. } =
        serde_json::from_value::<QueryResult>(playlists).unwrap()
    else {
        panic!()
    };
    let PlaylistKind::Smart { query, .. } = &playlists[0].kind else {
        panic!()
    };
    assert!(query.sort[0].descending);
    assert_eq!(
        query.filter,
        Filter::Compare {
            field: Field::Year,
            op: CompareOp::Ge,
            value: Value::Int(2000)
        }
    );

    let event = json!({"seq": 5, "origin": null, "event": {
        "type": "confirmation_requested", "confirmation": {
            "token": "t", "summary": "s", "expires_at_ms": 1,
            "requested_by": {"type": "agent", "name": "a"},
            "command": {"type": "play", "start": 0, "start_track": null, "shuffle": true,
                        "context": {"type": "album", "key": "k", "disc": 2}}}}});
    let envelope: EventEnvelope = serde_json::from_value(event).unwrap();
    let Event::ConfirmationRequested { confirmation } = envelope.event else {
        panic!()
    };
    assert_eq!(
        confirmation.command,
        Command::Play {
            context: PlayContext::Album { key: "k".into() },
            start: 0,
            start_track: None
        }
    );
    let purge: Command = serde_json::from_value(
        json!({"type": "purge_playlists", "playlists": null, "older_than_days": 30}),
    )
    .unwrap();
    assert_eq!(purge, Command::PurgePlaylists { playlists: None });

    // Clients that validate with the schema: only the request side is closed, at its
    // outermost objects; the shared definitions are open.
    let schema = json_schema();
    for root in ["Response", "QueryResult", "EventEnvelope", "$defs"] {
        assert!(
            !schema[root]
                .to_string()
                .contains(r#""additionalProperties":false"#),
            "{root} has closed objects"
        );
    }
    // One `$defs` for all roots: what schemars generates for each root is the same
    // wherever two roots share a definition, so nothing is lost by keeping one copy.
    let mut seen: BTreeMap<String, (&str, serde_json::Value)> = BTreeMap::new();
    for (root, generated) in root_schemas() {
        for (def, shape) in generated["$defs"].as_object().unwrap() {
            match seen.get(def) {
                Some((first, earlier)) => {
                    assert_eq!(earlier, shape, "{def} differs between {first} and {root}")
                }
                None => {
                    seen.insert(def.clone(), (root, shape.clone()));
                }
            }
        }
        assert!(
            schema[root].get("$defs").is_none(),
            "{root} has its own $defs"
        );
    }
    let shared: Vec<_> = schema["$defs"].as_object().unwrap().keys().collect();
    assert_eq!(shared, seen.keys().collect::<Vec<_>>());
    // No `$ref` with siblings anywhere.
    fn no_ref_siblings(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Object(m) => {
                (!m.contains_key("$ref") || m.len() == 1) && m.values().all(no_ref_siblings)
            }
            serde_json::Value::Array(a) => a.iter().all(no_ref_siblings),
            _ => true,
        }
    }
    assert!(no_ref_siblings(&schema));
    assert_eq!(schema["Request"]["additionalProperties"], false);
    assert_eq!(schema["Query"]["oneOf"][0]["additionalProperties"], false);
}

#[test]
fn value_receipt_and_error_json() {
    assert_eq!(
        serde_json::to_value(Value::Int(5)).unwrap(),
        json!({"type": "int", "value": 5})
    );
    assert_eq!(
        serde_json::to_value(Value::DaysAgo(30)).unwrap(),
        json!({"type": "days_ago", "value": 30})
    );
    for v in [
        Value::Int(-1),
        Value::Bool(true),
        Value::Text("x".into()),
        Value::Timestamp(1_700_000_000_000),
        Value::DaysAgo(7),
    ] {
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&json).unwrap(), v);
    }
    assert!(serde_json::from_str::<Value>(r#"{"type":"int","value":"x"}"#).is_err());
    // Lenient here; the JSON adapters reject the extra field.
    assert_eq!(
        serde_json::from_str::<Value>(r#"{"type":"int","value":1,"x":1}"#).unwrap(),
        Value::Int(1)
    );
    assert_eq!(
        serde_json::from_str::<Value>(r#"{"type":"float","value":0.5,"x":1}"#).unwrap(),
        Value::Unknown
    );

    let receipt = CommandResult::Ok(Receipt {
        changed: true,
        queue_revision: QueueRevision(41),
        created_entries: vec![EntryId(7), EntryId(8)],
        job: Some(JobId(3)),
        ..Default::default()
    });
    assert_eq!(
        serde_json::to_value(&receipt).unwrap(),
        json!({
        "type": "ok",
         "changed": true,
         "queue_revision": 41,
         "created_entries": [7,
         8],
         "job": 3}
        )
    );
    let back: CommandResult =
        serde_json::from_str(r#"{"type":"ok","changed":false,"queue_revision":2}"#).unwrap();
    assert_eq!(
        back,
        CommandResult::Ok(Receipt {
            changed: false,
            queue_revision: QueueRevision(2),
            ..Default::default()
        })
    );

    let error = ContractError::conflict(ErrorCode::QueueConflict, "queue changed", 4, 6);
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        json!({"kind": "conflict", "code": "queue_conflict", "message": "queue changed",
               "details": {"expected": 4, "actual": 6}})
    );
    let error = ContractError::not_found(ErrorCode::TrackNotFound, "no such tracks")
        .with_missing(vec![ObjectRef::Track { id: TrackId(9) }]);
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        json!({"kind": "not_found", "code": "track_not_found", "message": "no such tracks",
               "details": {"missing": [{"type": "track", "id": 9}]}})
    );
    assert_eq!(
        serde_json::to_value(ContractError::invalid(ErrorCode::OutOfRange, "x")).unwrap(),
        json!({"kind": "invalid", "code": "out_of_range", "message": "x"})
    );
    assert_eq!(ErrorCode::parse("root_overlaps"), ErrorCode::RootOverlaps);
    assert_eq!(ErrorCode::NotADirectory.as_str(), "not_a_directory");
}

#[test]
fn every_library_error_code_is_a_contract_code() {
    // The table on `mpmp_library::LibraryError`.
    for code in [
        "db",
        "library_damaged",
        "io",
        "schema_too_new",
        "not_a_directory",
        "root_overlaps",
        "path_not_unicode",
        "invalid_query",
        "playlist_conflict",
        "action_conflict",
        "already_exists",
        "playlist_not_found",
        "track_not_found",
        "item_not_found",
        "root_not_found",
        "nothing_to_undo",
        "action_not_found",
        "file_not_found",
        "playlist_kind_mismatch",
        "out_of_range",
        "blank_name",
        "not_in_trash",
        "playlist_in_trash",
        "already_undone",
        "not_an_m3u",
        "not_a_regular_file",
        "unknown_encoding",
        "corrupt_data",
        "cancelled",
        "writer_stopped",
        "scan_panicked",
        "file_in_use",
        "file_read_only",
        "location_changed",
        "invalid_tag",
    ]
    .into_iter()
    // What a tag job lists files with.
    .chain(mpmp_library::TAG_FILE_CODES.iter().copied())
    {
        assert_ne!(ErrorCode::parse(code), ErrorCode::Unknown, "{code}");
        assert_eq!(ErrorCode::parse(code).as_str(), code);
    }
}

#[test]
fn resolving_cover_recovery_is_always_confirmed_and_ui_only() {
    let command = Command::ResolveCoverRecovery {
        track: TrackId(1),
        resolution: CoverRecoveryResolution::AcceptCurrent,
        selection: None,
    };
    assert_eq!(command.policy(&Origin::Ui), Policy::Confirm);
    for origin in [
        Origin::System,
        Origin::MediaControls,
        Origin::agent("a"),
        Origin::Remote { device: "d".into() },
    ] {
        assert_eq!(
            command.policy(&origin),
            Policy::Forbidden(ErrorCode::OriginNotAllowed)
        );
    }
    assert_eq!(command.class(), OperationClass::FileWrite);
}
#[test]
fn policies() {
    let agent = Origin::agent("a");
    let remote = Origin::Remote { device: "d".into() };
    let add_root = Command::AddRoot { path: "/".into() };
    assert_eq!(add_root.policy(&Origin::Ui), Policy::Allow);
    assert_eq!(add_root.policy(&agent), Policy::Confirm);
    assert_eq!(
        add_root.policy(&remote),
        Policy::Forbidden(ErrorCode::PathNotAllowed)
    );
    assert_eq!(
        add_root.policy(&Origin::MediaControls),
        Policy::Forbidden(ErrorCode::OriginNotAllowed)
    );
    let purge = Command::PurgePlaylists { playlists: None };
    assert_eq!(purge.policy(&remote), Policy::Confirm);
    assert_eq!(purge.policy(&agent), Policy::Confirm);
    assert_eq!(purge.policy(&Origin::System), Policy::Allow);
    assert_eq!(Command::Pause.policy(&Origin::MediaControls), Policy::Allow);
    let confirm = Command::Confirm { token: "t".into() };
    assert_eq!(confirm.policy(&Origin::Ui), Policy::Allow);
    for origin in [Origin::System, agent, remote, Origin::MediaControls] {
        assert_eq!(
            confirm.policy(&origin),
            Policy::Forbidden(ErrorCode::OriginNotAllowed)
        );
    }
    assert_eq!(
        Command::Pause.policy(&Origin::Unknown),
        Policy::Forbidden(ErrorCode::OriginNotAllowed)
    );
    // Recorded changes need no confirmation: they can be undone, and undoing is
    // allowed to everyone who may write.
    let agent = Origin::agent("a");
    let remote = Origin::Remote { device: "d".into() };
    for command in [
        Command::SetRating {
            tracks: vec![TrackId(1)],
            rating: None,
        },
        Command::SetLoved {
            tracks: vec![TrackId(1)],
            loved: false,
        },
        Command::DeletePlaylist {
            playlist: PlaylistId(1),
        },
        Command::UndoAction {
            action: ActionId(1),
        },
    ] {
        assert_eq!(command.risk(), Risk::Write, "{command:?}");
        for origin in [Origin::Ui, Origin::System, agent.clone(), remote.clone()] {
            assert_eq!(
                command.policy(&origin),
                Policy::Allow,
                "{command:?} {origin:?}"
            );
        }
        assert_eq!(
            command.policy(&Origin::MediaControls),
            Policy::Forbidden(ErrorCode::OriginNotAllowed)
        );
    }
    // What an agent may be granted is what it would otherwise ask for: local paths,
    // writing files, permanently deleting (never granted, but asked); nothing else.
    let agent = Origin::agent("a");
    for spec in COMMANDS {
        let asks = spec.policy(&agent) == Policy::Confirm;
        let class = spec.class;
        assert_eq!(
            asks,
            !spec.ui_only
                && (matches!(
                    class,
                    OperationClass::LocalPath | OperationClass::FileWrite | OperationClass::Purge
                ) || matches!(spec.name, "set_output_device" | "set_output_mode")),
            "{}",
            spec.name
        );
    }
    // Local paths stay as decided: agents ask, remotes may not.
    let import = Command::ImportM3u {
        path: "/x.m3u".into(),
        name: None,
    };
    assert_eq!(import.policy(&agent), Policy::Confirm);
    assert_eq!(
        import.policy(&remote),
        Policy::Forbidden(ErrorCode::PathNotAllowed)
    );
}

#[test]
fn origin_labels_round_trip_and_filter() {
    for origin in [
        Origin::Ui,
        Origin::MediaControls,
        Origin::System,
        Origin::agent("claude"),
        Origin::Remote {
            device: "phone:2".into(),
        },
    ] {
        assert_eq!(Origin::from_label(&origin.label()), origin);
    }
    // An agent's session is not part of its label.
    let session = Origin::Agent {
        name: "claude".into(),
        session: Some("conn-7".into()),
    };
    assert_eq!(session.label(), "agent:claude");
    assert_eq!(
        serde_json::to_value(Origin::agent("claude")).unwrap(),
        json!({"type": "agent", "name": "claude"}),
        "no session: omitted optional session"
    );
    assert_eq!(
        serde_json::to_value(&session).unwrap(),
        json!({"type": "agent", "name": "claude", "session": "conn-7"})
    );
    assert_eq!(Origin::agent("x").label(), "agent:x");
    assert_eq!(Origin::from_label("unknown"), Origin::Unknown);
    assert_eq!(Origin::from_label("scheduler:1"), Origin::Unknown);
    assert_eq!(
        OriginFilter::Agent { name: None }.label_prefix().as_deref(),
        Some("agent:")
    );
    assert_eq!(
        OriginFilter::Remote {
            device: Some("p".into())
        }
        .label_prefix()
        .as_deref(),
        Some("remote:p")
    );
    assert_eq!(OriginFilter::Unknown.label_prefix(), None);
    assert_eq!(
        ActionKind::parse("playlist_set_query"),
        ActionKind::PlaylistSetQuery
    );
    assert_eq!(ActionKind::parse("edit_tags"), ActionKind::Unknown);
}

/// Following `next_offset` always ends.
#[test]
fn next_offset_ends_on_an_empty_or_last_page() {
    assert_eq!(next_offset(0, 2, 5), Some(2));
    assert_eq!(next_offset(3, 2, 5), None);
    assert_eq!(next_offset(0, 0, 5), None, "an empty page (limit 0)");
    assert_eq!(next_offset(3, 0, 10), None);
    assert_eq!(next_offset(8, 5, 10), None, "past the total");
}

/// The schema's `x-risk` / `x-origins` and the code come from one table, and the
/// table covers every command.
#[test]
fn schema_documents_every_command_and_query_for_agents() {
    let schema = json_schema();
    let description = schema["description"].as_str().unwrap();
    for needle in ["Ordering per caller", "Idempotency", "Actions and undo"] {
        assert!(description.contains(needle), "{needle}");
    }
    for (root, def) in [("$defs", "Command"), ("Query", "")] {
        let node = if def.is_empty() {
            &schema[root]
        } else {
            &schema[root][def]
        };
        for variant in node["oneOf"].as_array().unwrap() {
            let tag = variant_tag(variant).unwrap();
            let text = variant["description"].as_str().unwrap_or_default();
            assert!(!text.is_empty(), "{root} {tag} has no description");
            if tag == "unknown" {
                assert!(text.contains("never send"), "{root} {tag}");
            }
        }
    }
    // Every `unknown` fallback says it is decode-only, wherever it is.
    fn check_unknowns(value: &serde_json::Value, path: &str) {
        match value {
            serde_json::Value::Object(map) => {
                let listed = map
                    .get("enum")
                    .and_then(|e| e.as_array())
                    .is_some_and(|e| e.contains(&json!("unknown")));
                assert!(!listed, "{path}: `unknown` listed without a description");
                let is_unknown = map.get("const") == Some(&json!("unknown"))
                    || variant_tag(value) == Some("unknown");
                if is_unknown {
                    let text = map.get("description").and_then(|d| d.as_str());
                    assert!(
                        text.is_some_and(|t| t.contains("never send")),
                        "{path}: {value}"
                    );
                }
                for (k, v) in map {
                    if k != "properties" || !is_unknown {
                        check_unknowns(v, &format!("{path}.{k}"));
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    check_unknowns(v, &format!("{path}[{i}]"));
                }
            }
            _ => {}
        }
    }
    check_unknowns(&schema, "");
    // 64-bit integers carry the 2^53 bound.
    let request_id = &schema["Request"]["properties"]["request_id"];
    assert_eq!(request_id["maximum"], json!(MAX_SAFE_INTEGER));
    let at_ms = &schema["$defs"]["ActionSummary"]["properties"]["recorded_at_ms"];
    assert_eq!(at_ms["minimum"], json!(-(MAX_SAFE_INTEGER as i64)));
    // Descriptions speak JSON, not Rust: no intra-doc links, `null` rather than `None`,
    // no section numbers of documents an agent does not have.
    fn check_descriptions(value: &serde_json::Value, path: &str) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(text) = map.get("description").and_then(|d| d.as_str()) {
                    for needle in ["[`", "`None`", "`Some", "§", "module docs", "::"] {
                        assert!(!text.contains(needle), "{path}: {needle} in {text:?}");
                    }
                }
                for (k, v) in map {
                    check_descriptions(v, &format!("{path}.{k}"));
                }
            }
            serde_json::Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    check_descriptions(v, &format!("{path}[{i}]"));
                }
            }
            _ => {}
        }
    }
    for root in [
        "Request",
        "Response",
        "Query",
        "QueryResult",
        "EventEnvelope",
        "$defs",
    ] {
        check_descriptions(&schema[root], root);
    }
    // The fields every paged query and result has are described.
    let tracks = schema["Query"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| variant_tag(v) == Some("tracks"))
        .unwrap();
    for field in ["offset", "limit", "tags"] {
        assert!(
            tracks["properties"][field]["description"].is_string(),
            "{field}"
        );
    }
}

#[test]
fn schema_command_metadata_matches_the_table() {
    let schema = json_schema();
    let variants = schema["$defs"]["Command"]["oneOf"].as_array().unwrap();
    let mut named = Vec::new();
    for variant in variants {
        let name = variant_tag(variant).unwrap();
        if name == "unknown" {
            continue;
        }
        let spec = COMMANDS
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} is not in COMMANDS"));
        let ext = command_extensions(spec);
        assert_eq!(variant["x-risk"], ext["x-risk"], "{name}");
        assert_eq!(variant["x-origins"], ext["x-origins"], "{name}");
        assert_eq!(variant["x-class"], ext["x-class"], "{name}");
        // Permission-management commands and explicit recovery resolution are UI-only;
        // recovery remains file_write even though it cannot be granted to agents.
        assert_ne!(spec.class, OperationClass::Unknown, "{name}");
        assert_eq!(
            spec.ui_only,
            spec.class == OperationClass::Permissions || spec.name == "resolve_cover_recovery",
            "{name}"
        );
        for origin in origin_kinds() {
            let expected = match spec.policy(&origin) {
                Policy::Allow => "allow",
                Policy::Confirm => "confirm",
                Policy::Forbidden(_) => "forbidden",
            };
            assert_eq!(variant["x-origins"][origin.kind()], expected, "{name}");
        }
        named.push(name.to_owned());
    }
    let mut table: Vec<_> = COMMANDS.iter().map(|s| s.name.to_owned()).collect();
    named.sort();
    table.sort();
    assert_eq!(named, table);
    // `Command::name` agrees with serde for every table entry's variant.
    for variant in variants {
        let name = variant_tag(variant).unwrap();
        let cmd: Result<Command, _> = serde_json::from_value(json!({"type": name}));
        if let Ok(cmd) = cmd {
            assert_eq!(cmd.name(), name);
        }
    }
}

fn sample_snapshot() -> Snapshot {
    Snapshot {
        seq: 1,
        contract_version: CONTRACT_VERSION,
        contract_minor: CONTRACT_MINOR,
        playback_limits: None,
        output: None,
        features: Vec::new(),
        now_ms: 5,
        state: PlayState::Playing,
        current: Some(NowPlaying {
            entry: EntryId(1),
            track: TrackInfo::default(),
            signal_path: Some(SignalPathInfo {
                share_mode: Some(crate::contract::OutputShareMode::Shared),
                output_device_id: None,
                backend: Some(crate::contract::OutputBackend::Cpal),
                sample_integrity: Some(crate::contract::SampleIntegrity::Unverified),
                codec: "flac".into(),
                lossless: true,
                source_rate: 44_100,
                source_channels: 2,
                source_bits_per_sample: Some(16),
                stages: Vec::new(),
                output_rate: 44_100,
                output_channels: 2,
                output_format: SampleFormat::F32,
                bit_perfect: BitPerfectInfo::Yes,
                reason_codes: Vec::new(),
            }),
        }),
        position: None,
        queue: QueueSummary::default(),
        shuffle: false,
        repeat: RepeatMode::Off,
        audio: AudioSettings::default(),
        problems: Vec::new(),
    }
}

/// Check the field that previously drifted even without the Node generator.
/// Rust -> exported schema is also checked by exported_schema_is_up_to_date.
#[test]
fn tag_completion_documentation_matches_schema() {
    fn description(schema: &serde_json::Value) -> &str {
        schema["$defs"]["JobReport"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variant| variant["properties"]["type"]["const"] == "tag_edit")
            .unwrap()["properties"]["written_count"]["description"]
            .as_str()
            .unwrap()
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let live = json_schema();
    let exported: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("docs/contract.schema.json")).unwrap(),
    )
    .unwrap();
    let expected = description(&live);
    assert_eq!(
        description(&exported),
        expected,
        "regenerate the schema from Rust"
    );
}

/// docs/contract.schema.json is what the UI and other clients build against; it must
/// match the code. Regenerate with `UPDATE_SCHEMA=1 cargo test -p mpmp-core`.
#[test]
fn exported_schema_is_up_to_date() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/contract.schema.json");
    let current = serde_json::to_string_pretty(&json_schema()).unwrap() + "\n";
    if std::env::var_os("UPDATE_SCHEMA").is_some() {
        std::fs::write(&path, &current).unwrap();
    }
    // Line endings may differ on a Windows checkout; the content may not.
    let saved = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert!(
        saved == current,
        "docs/contract.schema.json is stale: run UPDATE_SCHEMA=1 cargo test -p mpmp-core"
    );
}
