//! Library adapter regression tests.

use super::*;
use crate::catalog::convert::lib_filter;
use crate::catalog::errors::library_error;
use crate::contract::{CompareOp, ErrorKind, Field, Filter, TrackQuery, Value};
use mpmp_library::LibraryError;

#[test]
fn semantic_filter_errors_name_the_expression_including_nested_arrays() {
    let filter: Filter = serde_json::from_str(
        r#"{"type":"compare","field":{"type":"title"},"op":"gt","value":{"type":"int","value":3}}"#,
    )
    .unwrap();

    let error = lib_filter(&filter, "query.filter").unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidQuery);
    assert_eq!(error.details.field.as_deref(), Some("query.filter"));
    assert!(error.message.contains("Title"));
    let nested = Filter::And {
        all: vec![
            Filter::All,
            Filter::Not {
                filter: Box::new(filter),
            },
        ],
    };
    let error = lib_filter(&nested, "filter").unwrap_err();
    assert_eq!(error.details.field.as_deref(), Some("filter.all[1].filter"));
}

#[test]
fn history_contexts_are_versioned() {
    let json: serde_json::Value = serde_json::from_str(&context_summary(&PlayContext::Tracks {
        tracks: vec![TrackId(1), TrackId(2)],
    }))
    .unwrap();
    assert_eq!(
        json,
        serde_json::json!({"v": 1, "type": "tracks", "count": 2})
    );
    let json: serde_json::Value = serde_json::from_str(&context_summary(&PlayContext::Query {
        query: TrackQuery {
            filter: Filter::Compare {
                field: Field::Year,
                op: CompareOp::Ge,
                value: Value::Int(2000),
            },
            sort: Vec::new(),
        },
    }))
    .unwrap();
    assert_eq!(json["v"], 1);
    assert_eq!(
        json["query"]["filter"]["value"],
        serde_json::json!({"type": "int", "value": 2000})
    );
}

#[test]
fn library_errors_keep_their_codes() {
    let e = library_error(LibraryError::PlaylistConflict {
        expected: 3,
        actual: 5,
    });
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::Conflict, ErrorCode::PlaylistConflict)
    );
    assert_eq!((e.details.expected, e.details.actual), (Some(3), Some(5)));
    let e = library_error(LibraryError::NotADirectory("/x".into()));
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::Invalid, ErrorCode::NotADirectory)
    );
    let e = library_error(LibraryError::WriterStopped);
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::Failed, ErrorCode::WriterStopped)
    );
    assert!(!e.details.retryable);
    // A damaged database is told apart from a database that failed this time.
    let sqlite = |code| {
        LibraryError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            None,
        ))
    };
    let e = library_error(sqlite(rusqlite::ffi::SQLITE_CORRUPT));
    assert_eq!(
        (e.kind, e.code, e.details.retryable),
        (ErrorKind::Failed, ErrorCode::LibraryDamaged, false)
    );
    let e = library_error(sqlite(rusqlite::ffi::SQLITE_BUSY));
    assert_eq!(
        (e.kind, e.code, e.details.retryable),
        (ErrorKind::Failed, ErrorCode::Db, true)
    );
}
