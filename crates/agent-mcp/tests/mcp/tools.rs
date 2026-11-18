//! Tools scenarios.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_tool_works_and_reports_contract_errors() {
    let h = Harness::new(ServerOptions::default());
    within(async {
        let client = h.client().await;
        let tools = client.list_all_tools().await.unwrap();
        assert_eq!(tools.len(), 13);
        // Hints: only playlists, folders, undo (of a tag edit: the user's files) and
        // contract commands may destroy something.
        let destructive = |name: &str| {
            let tool = tools.iter().find(|t| t.name == name).unwrap();
            let hints = tool.annotations.as_ref().unwrap();
            hints.destructive_hint
        };
        for name in ["playback", "queue", "rate_tracks", "wait_for_confirmation"] {
            assert_eq!(destructive(name), Some(false), "{name}");
        }
        for name in ["playlists", "library_folders", "undo", "contract_command"] {
            assert_eq!(destructive(name), None, "{name}");
        }

        // player_status
        let status = ok(&client, "player_status", json!({})).await;
        assert_eq!(status["state"], "stopped");
        assert_eq!(status["queue"]["entry_count"], 0);
        assert_eq!(status["problems"], json!([]));
        let (_, refused) = call(&client, "player_status", json!({"verbose": true})).await;
        assert_eq!(
            (
                &refused["kind"],
                &refused["code"],
                &refused["details"]["field"]
            ),
            (
                &json!("unsupported"),
                &json!("unknown_field"),
                &json!("verbose")
            )
        );

        // search_tracks
        let found = ok(&client, "search_tracks", json!({"text": "moon"})).await;
        assert_eq!(found["total"], 2);
        let titles: Vec<_> = found["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["title"].as_str().unwrap())
            .collect();
        assert!(titles.contains(&"Blue Moon") && titles.contains(&"Moon Song"));
        let page = ok(
            &client,
            "search_tracks",
            json!({"text": "moon", "limit": 1}),
        )
        .await;
        assert_eq!(page["tracks"].as_array().unwrap().len(), 1);
        assert_eq!(page["next_offset"], 1);
        assert_eq!(
            error(&client, "search_tracks", json!({})).await,
            pair("invalid", "invalid_request")
        );
        for empty in [json!({"text": "", "folder": ""}), json!({"folder": "/"})] {
            assert_eq!(
                error(&client, "search_tracks", empty).await,
                pair("invalid", "invalid_request")
            );
        }
        // An absolute folder is a music folder, plus the place in it.
        let listed = ok(&client, "library_folders", json!({"action": "list"})).await;
        let music = listed["folders"][0]["path"].as_str().unwrap().to_owned();
        let music_root = listed["folders"][0]["root"].as_i64().unwrap();
        let whole = ok(&client, "search_tracks", json!({"folder": music})).await;
        assert_eq!(whole["total"], 3);
        let doubled = format!("{music}//");
        let same = ok(
            &client,
            "search_tracks",
            json!({"folder": doubled, "root": music_root, "text": "moon"}),
        )
        .await;
        assert_eq!(same["total"], 2);
        for bad in [
            json!({"folder": "/definitely/not/a/music/folder"}),
            json!({"folder": music, "root": music_root + 1}),
        ] {
            let (failed, value) = call(&client, "search_tracks", bad).await;
            assert!(failed, "{value}");
            assert_eq!(value["code"], "invalid_request");
            assert_eq!(value["details"]["field"], "folder", "{value}");
        }

        // playback
        let blue = h.track("Blue Moon");
        let red = h.track("Red River");
        let played = ok(
            &client,
            "playback",
            json!({"action": "play", "tracks": [blue, red]}),
        )
        .await;
        assert_eq!(played["changed"], true);
        ok(&client, "playback", json!({"action": "pause"})).await;
        assert_eq!(
            error(&client, "playback", json!({"action": "seek"})).await,
            pair("invalid", "invalid_request")
        );
        assert_eq!(
            error(
                &client,
                "playback",
                json!({"action": "pause", "position_ms": 3})
            )
            .await,
            pair("invalid", "invalid_request")
        );
        assert_eq!(
            error(
                &client,
                "playback",
                json!({"action": "play", "tracks": [999_999]})
            )
            .await,
            pair("not_found", "track_not_found")
        );
        let (failed, refused) = call(&client, "playback", json!({"action": "rewind"})).await;
        assert!(failed);
        assert_eq!(
            (&refused["code"], &refused["details"]["field"]),
            (&json!("unknown_variant"), &json!("action"))
        );
        let (_, refused) = call(
            &client,
            "queue",
            json!({"action": "add", "tracks": [1], "at": "middle"}),
        )
        .await;
        assert_eq!(refused["details"]["field"], "at");
        ok(&client, "playback", json!({"action": "stop"})).await;

        // queue
        let queue = ok(&client, "queue", json!({"action": "list"})).await;
        assert_eq!(queue["total"], 2);
        assert_eq!(queue["entries"][0]["track"]["title"], "Blue Moon");
        let added = ok(
            &client,
            "queue",
            json!({"action": "add", "tracks": [red], "at": "next"}),
        )
        .await;
        assert_eq!(added["created_entries"].as_array().unwrap().len(), 1);
        assert_eq!(
            error(&client, "queue", json!({"action": "add"})).await,
            pair("invalid", "invalid_request")
        );
        assert_eq!(
            error(
                &client,
                "queue",
                json!({"action": "add", "tracks": [999_999]})
            )
            .await,
            pair("not_found", "track_not_found")
        );
        ok(&client, "queue", json!({"action": "clear"})).await;
        assert_eq!(
            ok(&client, "player_status", json!({})).await["queue"]["entry_count"],
            0
        );

        // playlists
        let created = ok(
            &client,
            "playlists",
            json!({"action": "create", "name": "Night"}),
        )
        .await;
        let playlist = created["playlist"]["playlist"].as_i64().unwrap();
        ok(
            &client,
            "playlists",
            json!({"action": "add_tracks", "playlist": playlist, "tracks": [blue, red]}),
        )
        .await;
        let items = ok(
            &client,
            "playlists",
            json!({"action": "items", "playlist": playlist}),
        )
        .await;
        assert_eq!(items["total"], 2);
        assert_eq!(items["playlist"]["name"], "Night");
        let first_item = items["items"][0]["item"].as_i64().unwrap();
        ok(
            &client,
            "playlists",
            json!({"action": "remove_items", "playlist": playlist, "items": [first_item]}),
        )
        .await;
        ok(
            &client,
            "playlists",
            json!({"action": "rename", "playlist": playlist, "name": "Late night"}),
        )
        .await;
        let list = ok(&client, "playlists", json!({"action": "list"})).await;
        assert_eq!(list["playlists"][0]["name"], "Late night");
        assert_eq!(list["playlists"][0]["track_count"], 1);
        ok(
            &client,
            "playlists",
            json!({"action": "delete", "playlist": playlist}),
        )
        .await;
        assert_eq!(
            ok(&client, "playlists", json!({"action": "list"})).await["total"],
            0
        );
        assert_eq!(
            error(
                &client,
                "playlists",
                json!({"action": "rename", "playlist": 999_999, "name": "x"})
            )
            .await,
            pair("not_found", "playlist_not_found")
        );
        assert_eq!(
            error(&client, "playlists", json!({"action": "create"})).await,
            pair("invalid", "invalid_request")
        );

        // rate_tracks
        let rated = ok(
            &client,
            "rate_tracks",
            json!({"tracks": [blue], "rating": 5, "loved": true}),
        )
        .await;
        assert_eq!(rated["rating"]["changed"], true);
        assert_eq!(rated["loved"]["changed"], true);
        let found = ok(&client, "search_tracks", json!({"text": "blue"})).await;
        assert_eq!(found["tracks"][0]["rating"], 5);
        assert_eq!(found["tracks"][0]["loved"], true);
        let cleared = ok(
            &client,
            "rate_tracks",
            json!({"tracks": [blue], "rating": null}),
        )
        .await;
        assert_eq!(cleared["rating"]["changed"], true);
        assert_eq!(
            error(
                &client,
                "rate_tracks",
                json!({"tracks": [blue], "rating": 0})
            )
            .await,
            pair("invalid", "out_of_range")
        );
        assert_eq!(
            error(&client, "rate_tracks", json!({"tracks": [blue]})).await,
            pair("invalid", "invalid_request")
        );

        // library_folders (adding and removing need confirmation: see below)
        let folders = ok(&client, "library_folders", json!({"action": "list"})).await;
        assert_eq!(folders["folders"][0]["track_count"], 3);
        let root = folders["folders"][0]["root"].as_i64().unwrap();
        let rescan = ok(&client, "library_folders", json!({"action": "rescan"})).await;
        assert!(rescan["job"].is_u64(), "{rescan}");
        assert_eq!(
            error(
                &client,
                "library_folders",
                json!({"action": "remove", "root": 999_999})
            )
            .await,
            pair("not_found", "root_not_found")
        );
        assert_eq!(
            error(&client, "library_folders", json!({"action": "remove"})).await,
            pair("invalid", "invalid_request")
        );
        let asked = ok(
            &client,
            "library_folders",
            json!({"action": "remove", "root": root}),
        )
        .await;
        assert_eq!(asked["status"], "needs_confirmation");
        h.confirmation_request();

        // recent_actions and undo
        let actions = ok(&client, "recent_actions", json!({"limit": 5})).await;
        let newest = &actions["actions"][0];
        assert_eq!(newest["kind"], "set_rating");
        assert_eq!(newest["origin"]["name"], "claude");
        assert_eq!(
            error(&client, "recent_actions", json!({"limit": "five"})).await,
            pair("invalid", "invalid_request")
        );
        let undone = ok(&client, "undo", json!({"action": newest["id"]})).await;
        assert!(undone["action"].is_i64(), "{undone}");
        assert_eq!(
            error(&client, "undo", json!({"action": 999_999})).await,
            pair("not_found", "action_not_found")
        );

        // contract_command
        let shuffled = ok(
            &client,
            "contract_command",
            json!({"command": {"type": "set_shuffle", "shuffle": true}, "idempotency_key": "k1"}),
        )
        .await;
        assert_eq!(shuffled["changed"], true);
        assert_eq!(
            error(
                &client,
                "contract_command",
                json!({"command": {"type": "set_shuffle", "shuffle": true, "now": 1}})
            )
            .await,
            pair("unsupported", "unknown_field")
        );
        assert_eq!(
            error(
                &client,
                "contract_command",
                json!({"command": {"type": "register_agent", "name": "mallory"}})
            )
            .await,
            pair("forbidden", "origin_not_allowed")
        );

        // contract_query
        let roots = ok(
            &client,
            "contract_query",
            json!({"query": {"type": "roots"}}),
        )
        .await;
        assert_eq!(roots["type"], "roots");
        assert_eq!(
            error(
                &client,
                "contract_query",
                json!({"query": {"type": "teleports", "limit": 1}})
            )
            .await,
            pair("unsupported", "unknown_query")
        );

        // wait_for_confirmation: a token never given to this session
        assert_eq!(
            error(
                &client,
                "wait_for_confirmation",
                json!({"token": "confirm-1-1"})
            )
            .await,
            pair("not_found", "confirmation_not_found")
        );

        // Unknown tools are protocol errors.
        assert!(
            client
                .call_tool(CallToolRequestParams::new("teleport"))
                .await
                .is_err()
        );
        client.cancel().await.unwrap();
    })
    .await;
}

/// What the field tests missed (docs/design/agent-permissions.md): finding a folder's
/// tracks, M3U export and import, looking the contract up, and which grant forbade what.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn places_playlist_files_and_the_contract_reference() {
    let h = Harness::new(ServerOptions::default());
    within(async {
        let client = h.client().await;
        // A second music folder with folders in it.
        let more = folder(h.dir.path(), "more");
        for (dir, title) in [
            ("Jazz", "Blue Train"),
            ("Jazzy", "Not Jazz"),
            ("Rock", "Back"),
        ] {
            std::fs::create_dir_all(more.join(dir)).unwrap();
            wav(&more.join(dir).join(format!("{title}.wav")));
        }
        h.ui_ok(Command::AddRoot {
            path: more.display().to_string(),
        });
        h.wait_ui(|e| matches!(e, Event::JobFinished { .. }));

        // library_folders says how to list a folder's tracks.
        let folders = ok(&client, "library_folders", json!({"action": "list"})).await;
        assert!(folders["hint"].as_str().unwrap().contains("search_tracks"));
        let root_of = |path: &Path| {
            folders["folders"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["path"] == path.display().to_string())
                .unwrap()["root"]
                .as_i64()
                .unwrap()
        };
        let more_root = root_of(&more);
        let titles = |found: &Value| {
            let mut titles: Vec<String> = found["tracks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["title"].as_str().unwrap().to_owned())
                .collect();
            titles.sort();
            titles
        };
        let found = ok(&client, "search_tracks", json!({"root": more_root})).await;
        assert_eq!(titles(&found), ["Back", "Blue Train", "Not Jazz"]);
        // A folder is a whole name ("Jazz" is not "Jazzy"); letter case does not count.
        let jazz = ok(
            &client,
            "search_tracks",
            json!({"root": more_root, "folder": "Jazz"}),
        )
        .await;
        assert_eq!(titles(&jazz), ["Blue Train"]);
        assert_eq!(jazz["tracks"][0]["root"], more_root);
        assert_eq!(jazz["tracks"][0]["path"], "Jazz/Blue Train.wav");
        let anywhere = ok(&client, "search_tracks", json!({"folder": "jazz/"})).await;
        assert_eq!(titles(&anywhere), ["Blue Train"]);
        // With text: both.
        let blue = ok(
            &client,
            "search_tracks",
            json!({"text": "blue", "root": more_root}),
        )
        .await;
        assert_eq!(titles(&blue), ["Blue Train"]);

        // M3U export and import, approved by the user like any file.
        let created = ok(
            &client,
            "playlists",
            json!({"action": "create", "name": "Trains"}),
        )
        .await;
        let playlist = created["playlist"]["playlist"].as_i64().unwrap();
        let train = jazz["tracks"][0]["id"].clone();
        ok(
            &client,
            "playlists",
            json!({"action": "add_tracks", "playlist": playlist, "tracks": [train]}),
        )
        .await;
        let file = folder(h.dir.path(), "lists").join("trains.m3u8");
        let path = file.display().to_string();
        assert_eq!(
            error(
                &client,
                "playlists",
                json!({"action": "export_m3u", "playlist": playlist})
            )
            .await,
            pair("invalid", "invalid_request")
        );
        assert_eq!(
            error(
                &client,
                "playlists",
                json!({"action": "export_m3u", "playlist": playlist, "path": path, "name": "x"})
            )
            .await,
            pair("invalid", "invalid_request")
        );
        approved(
            &h,
            &client,
            "playlists",
            json!({"action": "export_m3u", "playlist": playlist, "path": path}),
        )
        .await;
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("Blue Train")
        );
        let imported = approved(
            &h,
            &client,
            "playlists",
            json!({"action": "import_m3u", "path": path, "name": "Trains again"}),
        )
        .await;
        let again = imported["playlist"]["playlist"].as_i64().unwrap();
        let items = ok(
            &client,
            "playlists",
            json!({"action": "items", "playlist": again}),
        )
        .await;
        assert_eq!(items["playlist"]["name"], "Trains again");
        assert_eq!(items["items"][0]["track"]["title"], "Blue Train");

        // contract_reference: the names, then one's schema.
        let list = ok(&client, "contract_reference", json!({})).await;
        let commands = list["commands"].as_array().unwrap();
        let export = commands.iter().find(|c| c["name"] == "export_m3u").unwrap();
        assert_eq!(export["agent"], "confirm");
        let register = commands
            .iter()
            .find(|c| c["name"] == "register_agent")
            .unwrap();
        assert_eq!(register["agent"], "not_available");
        let size = list.to_string().len();
        assert!(size < 16 * 1024, "{size} bytes");
        let shuffle = ok(
            &client,
            "contract_reference",
            json!({"name": "set_shuffle"}),
        )
        .await;
        assert_eq!(shuffle["kind"], "command");
        assert!(
            shuffle["schema"]["properties"]["shuffle"].is_object(),
            "{shuffle}"
        );
        assert_eq!(
            error(
                &client,
                "contract_reference",
                json!({"name": "export_playlist"})
            )
            .await,
            pair("unsupported", "unknown_variant")
        );

        // A forbid: the error names the grant and how much of the folder it covers, as core
        // gives them, not where it forbids.
        let private = folder(&more, "Private");
        let forbid = h
            .ui_ok(Command::GrantPermission {
                agent: "claude".into(),
                session: None,
                operation: Operation::Class {
                    class: OperationClass::LocalPath,
                },
                scope: Scope::Location {
                    location: private.display().to_string(),
                },
                until: Expiry::Forever,
                effect: Effect::Forbid,
            })
            .grant
            .unwrap();
        let (failed, refused) = call(
            &client,
            "library_folders",
            json!({"action": "remove", "root": more_root}),
        )
        .await;
        assert!(failed, "{refused}");
        assert_eq!(refused["code"], "permission_denied");
        assert_eq!(refused["details"]["forbidden_by"][0]["grant"], forbid.0);
        assert_eq!(refused["details"]["forbidden_by"][0]["covers"], "part");
        assert!(!refused.to_string().contains("Private"), "{refused}");
        assert!(
            refused["message"]
                .as_str()
                .unwrap()
                .contains(&format!("forbidden by grant {}", forbid.0)),
            "{refused}"
        );
        client.cancel().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agents_and_permissions_queries_are_refused() {
    let h = Harness::new(ServerOptions::default());
    within(async {
        let client = h.client().await;
        for query in [
            json!({"type": "agents"}),
            json!({"type": "permissions", "limit": 10}),
            json!({"type": "permissions", "agent": "claude", "limit": 10}),
        ] {
            let (failed, value) = call(&client, "contract_query", json!({"query": query})).await;
            assert!(failed);
            assert_eq!(value["kind"], "forbidden");
            assert_eq!(value["code"], "origin_not_allowed");
            // Nothing about agents or grants comes back.
            assert!(!value.to_string().contains("claude"), "{value}");
        }
        client.cancel().await.unwrap();
    })
    .await;
}

/// `rate_tracks` reports both parts: one may fail while the other took effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_tracks_reports_each_part() {
    let h = Harness::new(ServerOptions::default());
    within(async {
        let client = h.client().await;
        let blue = h.track("Blue Moon");
        h.ui_ok(Command::GrantPermission {
            agent: "claude".into(),
            session: None,
            operation: Operation::Command {
                command: "set_loved".into(),
            },
            scope: Scope::All,
            until: Expiry::Forever,
            effect: Effect::Forbid,
        });
        let both = |tracks: Value| json!({"tracks": tracks, "rating": 4, "loved": true});
        let (failed, value) = call(&client, "rate_tracks", both(json!([blue]))).await;
        assert!(!failed, "{value}");
        assert_eq!(value["rating"]["changed"], true);
        assert!(value["rating"]["action"].is_i64(), "{value}");
        assert_eq!(value["loved"]["error"]["code"], "permission_denied");
        let message = value["message"].as_str().unwrap();
        assert!(message.contains("loved failed"), "{message}");
        let found = ok(&client, "search_tracks", json!({"text": "blue"})).await;
        assert_eq!(found["tracks"][0]["rating"], 4);
        // Both failed: a failure like any other (the first error at the top), with both.
        let (failed, value) = call(&client, "rate_tracks", both(json!([999_999]))).await;
        assert!(failed, "{value}");
        assert_eq!(value["code"], "track_not_found");
        assert!(value["message"].is_string(), "{value}");
        assert_eq!(value["rating"]["error"]["code"], "track_not_found");
        assert!(value["loved"]["error"]["code"].is_string(), "{value}");
        // One part: as before.
        assert_eq!(
            error(
                &client,
                "rate_tracks",
                json!({"tracks": [blue], "loved": true})
            )
            .await,
            pair("forbidden", "permission_denied")
        );
        client.cancel().await.unwrap();
    })
    .await;
}
