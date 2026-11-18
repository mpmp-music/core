//! Strict JSON adapter regression tests.
use super::*;
use crate::contract::Query;

#[test]
fn scalar_type_errors_keep_their_paths_through_tagged_enums() {
    assert!(
        parse_strict::<Query>("7")
            .unwrap_err()
            .details
            .field
            .is_none()
    );
    for (raw, path) in [
        (r#"{"type":"tracks","limit":"ten"}"#, "limit"),
        (r#"{"type":"tracks","limit":true}"#, "limit"),
        (r#"{"type":"tracks","limit":-1}"#, "limit"),
        (
            r#"{"type":"tracks","limit":1,"tags":["ten"],"query":{"sort":[{"field":{"type":"tag","key":"ten"},"descending":"ten"}]}}"#,
            "query.sort[0].descending",
        ),
        (r#"{"type":"tracks","limit":1,"tags":["ok",2]}"#, "tags[1]"),
    ] {
        let e = parse_strict::<Query>(raw).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidRequest, "{raw}");
        assert_eq!(e.details.field.as_deref(), Some(path), "{e:?}");
        assert!(!e.message.contains("__core_error_path_probe__"));
    }
    let e = err(r#"{"request_id":7,"command":{"type":"seek","position_ms":"ten"}}"#);
    assert_eq!(e.details.field.as_deref(), Some("command.position_ms"));
    // No fabricated path for a syntax failure, and retain serde's line/column.
    let e = parse_strict::<Query>("{\n\"type\":\"tracks\",\"offset\":0,,}").unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidJson);
    assert!(e.details.field.is_none());
    assert_eq!((e.details.line, e.details.column), (Some(2), Some(28)));
    let empty = parse_strict::<Query>("").unwrap_err();
    assert_eq!(
        (empty.details.line, empty.details.column),
        (Some(1), Some(1))
    );
    // Valid tag keys use up the probe budget before the malformed sort entry.
    let mut sort =
        vec![serde_json::json!({"field":{"type":"tag","key":"ten"}}); MAX_PATH_PROBES + 1];
    sort.push(serde_json::json!({"field":{"type":"title"},"descending":"ten"}));
    let raw = serde_json::json!({"type":"tracks","limit":1,"query":{"sort":sort}});
    let e = parse_strict::<Query>(&raw.to_string()).unwrap_err();
    assert!(e.details.field.is_none());
}
use crate::contract::ErrorKind;

fn err(json: &str) -> ContractError {
    match parse_request(json) {
        Err((_, e)) => e,
        Ok(r) => panic!("{r:?}"),
    }
}

/// Finding where a missing field belongs is bounded, so a
/// huge malformed request cannot stall core (it took minutes before the bound).
#[test]
fn locating_a_missing_field_in_a_huge_request_is_bounded() {
    // 20 000 objects, each lacking `op`: the path search tries at most a few of them.
    let item = r#"{"type":"text","field":{"type":"title"},"text":"x"}"#;
    let items = vec![item; 20_000].join(",");
    let json = format!(
        r#"{{"request_id":1,"command":{{"type":"set_smart_query","playlist":1,"query":{{"filter":{{"type":"or","any":[{items}]}}}},"max_tracks":null,"expected_revision":null}}}}"#
    );
    let started = std::time::Instant::now();
    let e = err(&json);
    assert_eq!(e.kind, ErrorKind::Invalid, "{e:?}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn json_requests_are_parsed_strictly() {
    let e = err(r#"{"request_id":3,"command":{"type":"pause","now":true}}"#);
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::Unsupported, ErrorCode::UnknownField)
    );
    assert_eq!(e.details.field.as_deref(), Some("command.now"));
    let e = err(r#"{"request_id":3,"command":{"type":"seek","position_ms":1,"exact":true}}"#);
    assert_eq!(e.code, ErrorCode::UnknownField);
    assert_eq!(e.details.field.as_deref(), Some("command.exact"));
    let e = err(
        r#"{"request_id":3,"command":{"type":"play","context":{"type":"query","query":{"filter":{"type":"all","x":1}}}}}"#,
    );
    assert_eq!(e.code, ErrorCode::UnknownField);
    assert_eq!(
        e.details.field.as_deref(),
        Some("command.context.query.filter.x")
    );
    let e = err(r#"{"request_id":3,"command":{"type":"pause"},"priority":1}"#);
    assert_eq!(e.code, ErrorCode::UnknownField);
    let e = err("{nope");
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::Invalid, ErrorCode::InvalidJson)
    );
    let e = err(r#"{"request_id":3,"command":{"type":"seek"}}"#);
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::Invalid, ErrorCode::InvalidRequest)
    );
    // A missing field is named by its path, also deep inside the request.
    assert_eq!(e.details.field.as_deref(), Some("command.position_ms"));
    for (json, field) in [
        (
            r#"{"request_id":3,"command":{"type":"set_rating","tracks":[1]}}"#,
            "command.rating",
        ),
        (r#"{"command":{"type":"pause"}}"#, "request_id"),
        (
            r#"{"request_id":3,"command":{"type":"play","context":{"type":"album"}}}"#,
            "command.context.key",
        ),
        (
            r#"{"request_id":3,"command":{"type":"set_smart_query","playlist":1,"query":{"sort":[{"field":{"type":"year"}},{"descending":true}]}}}"#,
            "command.query.sort[1].field",
        ),
    ] {
        let e = err(json);
        assert_eq!(e.code, ErrorCode::InvalidRequest, "{json}");
        assert_eq!(e.details.field.as_deref(), Some(field), "{json}");
    }
    let e = parse_strict::<Query>(r#"{"type":"tracks"}"#).unwrap_err();
    assert_eq!(e.details.field.as_deref(), Some("limit"));
    assert!(matches!(
        parse_request(r#"{"request_id":9,"command":{"type":"seek"}}"#),
        Err((9, _))
    ));
    // Unknown commands parse; core answers them.
    let r = parse_request(r#"{"request_id":3,"command":{"type":"teleport","to":1}}"#).unwrap();
    assert_eq!(r.command, Command::Unknown);
    // A nested unknown variant parses too; the typed layer refuses it.
    parse_request(
        r#"{"request_id":3,"command":{"type":"play","context":{"type":"radio","station":1}}}"#,
    )
    .unwrap();
    // Unit variants and defaults are fine.
    parse_request(r#"{"request_id":3,"command":{"type":"pause"}}"#).unwrap();
    parse_request(r#"{"request_id":3,"idempotency_key":"k","command":{"type":"clear"}}"#).unwrap();
    let e = parse_strict::<Query>(r#"{"type":"roots","all":true}"#).unwrap_err();
    assert_eq!(e.details.field.as_deref(), Some("all"));
}

/// The contract's types decode leniently (they also travel core → client); the JSON
/// adapters reject what they do not understand, at every depth.
#[test]
fn unknown_fields_are_rejected_at_every_depth() {
    for (json, field) in [
        (
            r#"{"type":"seek","position_ms":1,"entry":2,"exact":true}"#,
            "command.exact",
        ),
        (
            r#"{"type":"play","context":{"type":"album","key":"k","disc":2}}"#,
            "command.context.disc",
        ),
        (
            r#"{"type":"set_smart_query","playlist":1,"query":{"filter":{"type":"all"},"limit":3}}"#,
            "command.query.limit",
        ),
        (
            r#"{"type":"set_smart_query","playlist":1,"query":{"sort":[{"field":{"type":"year"},"asc":true}]}}"#,
            "command.query.sort[0].asc",
        ),
        (
            r#"{"type":"set_smart_query","playlist":1,"query":{"filter":{"type":"compare","field":{"type":"year"},"op":"eq","value":{"type":"int","value":1,"unit":"y"}}}}"#,
            "command.query.filter.value.unit",
        ),
        (
            r#"{"type":"create_playlist","name":"x","kind":{"type":"smart","query":{},"boost":1}}"#,
            "command.kind.boost",
        ),
        (
            r#"{"type":"set_replay_gain","mode":"track","limiter":true}"#,
            "command.limiter",
        ),
        (
            r#"{"type":"undo_action","action":1,"force":true}"#,
            "command.force",
        ),
    ] {
        let e = err(&format!(r#"{{"request_id":1,"command":{json}}}"#));
        assert_eq!(
            (e.kind, e.code),
            (ErrorKind::Unsupported, ErrorCode::UnknownField),
            "{json}"
        );
        assert_eq!(e.details.field.as_deref(), Some(field), "{json}");
    }
    for (json, field) in [
        (r#"{"type":"tracks","limit":1,"page":2}"#, "page"),
        (
            r#"{"type":"actions","limit":1,"origin":{"type":"agent","name":"a","since":0}}"#,
            "origin.since",
        ),
        (
            r#"{"type":"tracks","limit":1,"query":{"filter":{"type":"missing","field":{"type":"year","x":1}}}}"#,
            "query.filter.field.x",
        ),
    ] {
        let e = parse_strict::<Query>(json).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnknownField, "{json}");
        assert_eq!(e.details.field.as_deref(), Some(field), "{json}");
    }
}
