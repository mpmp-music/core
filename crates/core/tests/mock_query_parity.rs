//! One corpus for the real core Catalog/SQLite path and SDK mock. Node checks the
//! same expected results in check:sdk; no Node dependency is needed by cargo test.
use mpmp_core::catalog::{Browse, Playlists};
use mpmp_test_support::client_parity;

use mpmp_core::contract::{Query, QueryResult};
use mpmp_library::{ActionMeta, Library, normalize};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::path::Path;

fn library(dir: &Path, corpus: &Value, populated: bool) -> Library {
    const REBUILD_SEARCH_SUMMARIES: &str = r#"
INSERT INTO albums SELECT album_key, COALESCE(MIN(album), ''), MIN(album_group_artist), MIN(year), COUNT(*), COUNT(DISTINCT COALESCE(disc_no, 1)), SUM(COALESCE(duration_ms, 0)), NULL, MIN(added_at), substr(album_key, instr(album_key, char(31)) + 1), substr(album_key, 1, instr(album_key, char(31)) - 1) FROM tracks WHERE available=1 GROUP BY album_key;
        INSERT INTO artists SELECT group_artist_norm, MIN(album_group_artist), COUNT(DISTINCT album_key), COUNT(*) FROM tracks WHERE available=1 AND group_artist_norm<>'' GROUP BY group_artist_norm;
"#;

    const INSERT_TRACK_TAGS: &str = r#"
INSERT INTO track_tags (track_id,key,value,value_norm)
VALUES (?1,?2,?3,?4)
"#;

    let music = dir.join("music");
    std::fs::create_dir_all(&music).unwrap();
    let path = dir.join("library.db");
    let library = Library::open(&path).unwrap();
    let root = library
        .add_root(&music, ActionMeta::new("ui"))
        .unwrap()
        .value;
    if !populated {
        return library;
    }
    // Synthetic indexed rows, like the permissions fixtures: no audio decoding or
    // platform output. All search keys are derived by the actual library function.
    let mut conn = Connection::open(path).unwrap();
    let tx = conn.transaction().unwrap();
    for row in corpus["tracks"].as_array().unwrap() {
        let mut t = corpus["defaults"].clone();
        t.as_object_mut()
            .unwrap()
            .extend(row.as_object().unwrap().clone());
        let name = |key: &str| t[key].as_str();
        let norm = |key: &str| normalize(name(key).unwrap_or(""));
        let group = name("album_artist").or_else(|| {
            if t["compilation"] == true {
                Some("Various Artists")
            } else {
                name("artist")
            }
        });
        let key = mpmp_library::AlbumKey::new(group.unwrap_or(""), name("album").unwrap_or(""));
        tx.execute(
            "INSERT INTO tracks (id, root_id, rel_path, size, mtime_ms, available,
             title, artist, album, album_artist, genre, year, disc_no, track_no,
             compilation, lossless, title_norm, artist_norm, album_norm,
             album_artist_norm, genre_norm, rel_path_norm, album_key,
             album_group_artist, group_artist_norm, added_at, updated_at)
             VALUES (?1,?2,?3,0,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,
                     ?16,?17,?18,?19,?20,?21,?22,?23,?24,0,0)",
            params![
                t["id"].as_i64(),
                root.0,
                name("path"),
                t["modified_at_ms"].as_i64(),
                t["available"].as_bool(),
                name("title"),
                name("artist"),
                name("album"),
                name("album_artist"),
                name("genre"),
                t["year"].as_i64(),
                t["disc_no"].as_i64(),
                t["track_no"].as_i64(),
                t["compilation"].as_bool(),
                t["lossless"].as_bool(),
                norm("title"),
                norm("artist"),
                norm("album"),
                norm("album_artist"),
                norm("genre"),
                norm("path"),
                key.as_str(),
                group,
                normalize(group.unwrap_or(""))
            ],
        )
        .unwrap();
        for (key, values) in t["tags"].as_object().unwrap() {
            for value in values.as_array().unwrap() {
                let value = value.as_str().unwrap();
                let normalized = normalize(value);
                // The writer omits tag values with empty normalized keys.
                if !normalized.is_empty() {
                    tx.execute(
                        INSERT_TRACK_TAGS,
                        params![t["id"].as_i64(), key, value, normalized],
                    )
                    .unwrap();
                }
            }
        }
    }
    tx.execute_batch(REBUILD_SEARCH_SUMMARIES).unwrap();

    tx.commit().unwrap();
    library
}

#[test]
fn sdk_mock_query_corpus_matches_core_catalog() {
    let path = client_parity::core_crate_path(env!("CARGO_MANIFEST_DIR"), "mock-query-cases.json");
    let mut corpus: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let populated = library(&dir.path().join("populated"), &corpus, true);
    let empty = library(&dir.path().join("empty"), &corpus, false);
    let update = std::env::var_os("UPDATE_MOCK_QUERY_CASES").is_some();
    for case in corpus["normalization"].as_array_mut().unwrap() {
        let actual = json!(normalize(case["input"].as_str().unwrap()));
        if update {
            case["expected"] = actual;
        } else {
            assert_eq!(actual, case["expected"], "normalize {:?}", case["input"]);
        }
    }
    for case in corpus["cases"].as_array_mut().unwrap() {
        let mut query: Query = serde_json::from_value(case["query"].clone()).unwrap();
        query.cap_limit();
        let source = if case["empty"] == true {
            &empty
        } else {
            &populated
        };
        let actual = match source.query(&query) {
            QueryResult::Tracks {
                tracks,
                total,
                next_offset,
                ..
            } => {
                json!({
                "ids":tracks.iter().map(|t| t.id.0).collect::<Vec<_>>(),
                "total":total,
                "next_offset":next_offset}
                )
            }
            QueryResult::Album { album } => {
                json!({
                "ids":album.tracks.iter().map(|t|t.id.0).collect::<Vec<_>>(),
                "total":album.total,
                "offset":album.offset,
                "next_offset":album.next_offset}
                )
            }
            QueryResult::Search { results: r } => {
                json!({
                "ids":r.tracks.iter().map(|t|t.id.0).collect::<Vec<_>>(),
                "albums":r.albums.iter().map(|a|a.key.clone()).collect::<Vec<_>>(),
                "artists":r.artists.iter().map(|a|a.name.clone()).collect::<Vec<_>>(),
                "total":r.total,
                "offset":r.offset,
                "next_offset":r.next_offset,
                "totals":[r.tracks_total,
                r.albums_total,
                r.artists_total]}
                )
            }
            QueryResult::Err { error } => json!({"error":error.code}),
            other => panic!("unexpected query result {other:?}"),
        };
        if update {
            case["expected"] = actual;
        } else {
            assert_eq!(actual, case["expected"], "{}", case["name"]);
        }
    }
    if update {
        std::fs::write(path, serde_json::to_string_pretty(&corpus).unwrap() + "\n").unwrap();
    }
}

#[test]
fn mock_smart_query_commands_use_core_validation_without_rows() {
    use mpmp_core::contract::{Command, Origin};
    let corpus: Value = serde_json::from_str(client_parity::QUERY).unwrap();
    let cases: Value = serde_json::from_str(client_parity::STATE).unwrap();
    for (i, case) in cases["smart"].as_array().unwrap().iter().enumerate() {
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let library = library(
            &dir.path().join(i.to_string()),
            &corpus,
            case["empty"] != true,
        );
        let initial: Command = serde_json::from_value(
            json!({"type":"create_playlist","name":"original","kind":{"type":"smart","query":{}}}),
        )
        .unwrap();
        let id = library
            .playlist_command(&initial, &Origin::Ui, None)
            .unwrap()
            .receipt
            .unwrap()
            .playlist;
        let before =
            serde_json::to_value(library.playlist(mpmp_library::PlaylistId(id.0)).unwrap())
                .unwrap();
        for command in [
            json!({
            "type":"create_playlist",
            "name":"candidate",
            "kind":{
            "type":"smart",
            "query":case["query"]}
            }
            ),
            json!({"type":"set_smart_query","playlist":id,"query":case["query"]}),
        ] {
            let command: Command = serde_json::from_value(command).unwrap();
            let result = library.playlist_command(&command, &Origin::Ui, None);
            assert_eq!(result.is_ok(), case["valid"] == true, "{}", case["name"]);
            if let Err(error) = result {
                assert_eq!(serde_json::to_value(error.code).unwrap(), "invalid_query");
                assert_eq!(
                    serde_json::to_value(library.playlist(mpmp_library::PlaylistId(id.0)).unwrap())
                        .unwrap(),
                    before
                );
            }
        }
    }
}

/// Beyond the page cap: the exact v1.0 request must not silently lose any rows.
/// Node consumes the same inputs and expectations, independently of Rust's paging code.
#[test]
fn legacy_album_over_1000_tracks_and_explicit_paging_match_mock() {
    let mut corpus: Value = serde_json::from_str(client_parity::QUERY).unwrap();
    let spec = corpus["large_album"].clone();
    let count = spec["count"].as_u64().unwrap();
    corpus["tracks"] = (1..=count)
        .map(|id| {
            json!({
                "id": id, "title": format!("Song {id}"), "artist": "A", "album": "Album",
                "path": format!("{id:04}.flac"), "track_no": id,
            })
        })
        .collect();
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let catalog = library(dir.path(), &corpus, true);
    for case in spec["cases"].as_array().unwrap() {
        let mut query: Query = serde_json::from_value(case["query"].clone()).unwrap();
        query.cap_limit();
        let QueryResult::Album { album } = catalog.query(&query) else {
            panic!("{case}")
        };
        assert_eq!(
            json!({
            "length":album.tracks.len(),
            "offset":album.offset,
            "next_offset":album.next_offset}
            ),
            json!({
            "length":case["length"],
            "offset":case["offset"],
            "next_offset":case["next_offset"]}
            ),
            "{case}"
        );
        assert_eq!(u64::from(album.total), count);
        assert_eq!(u64::from(album.summary.track_count), count);
        let start = case["offset"].as_u64().unwrap() + 1;
        assert_eq!(
            album
                .tracks
                .iter()
                .map(|t| t.id.0 as u64)
                .collect::<Vec<_>>(),
            (start..start + case["length"].as_u64().unwrap()).collect::<Vec<_>>()
        );
    }
}

#[test]
fn mixed_search_nonzero_offset_reports_query_offset() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let catalog = Library::open(&dir.path().join("library.db")).unwrap();
    let query: Query =
        serde_json::from_value(json!({"type":"search","text":"x","offset":1,"limit":10})).unwrap();
    let QueryResult::Err { error } = catalog.query(&query) else {
        panic!()
    };
    assert_eq!(error.code, mpmp_core::contract::ErrorCode::OutOfRange);
    assert_eq!(error.kind, mpmp_core::contract::ErrorKind::Invalid);
    assert_eq!(error.details.field.as_deref(), Some("query.offset"));
}

#[test]
fn future_action_details_are_omitted_instead_of_sending_decode_only_unknown() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let corpus: Value = serde_json::from_str(
        &std::fs::read_to_string(client_parity::core_crate_path(
            env!("CARGO_MANIFEST_DIR"),
            "mock-query-cases.json",
        ))
        .unwrap(),
    )
    .unwrap();
    let lib = library(dir.path(), &corpus, false);
    let conn = Connection::open(dir.path().join("library.db")).unwrap();
    for details in [
        json!({"type":"future","private":"value"}),
        json!({"type":"undo","original_kind":"future"}),
        json!({"type":"tag_edit","changes":[{"key":"TITLE","operation":"future"}]}),
    ] {
        conn.execute(
            "UPDATE actions SET details_json=?1",
            [json!({"v":1,"details":details}).to_string()],
        )
        .unwrap();
        let QueryResult::Actions { actions, .. } = lib.query(&Query::Actions {
            origin: None,
            offset: 0,
            limit: 50,
        }) else {
            panic!()
        };
        assert!(!actions.is_empty());
        assert!(actions.iter().all(|action| action.details.is_none()));
        assert!(!serde_json::to_string(&actions).unwrap().contains("unknown"));
    }
}
