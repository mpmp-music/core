//! Trusted ui scenarios.
use super::*;

#[test]
fn trusted_button_payloads_parse_and_revoke_real_credentials_and_grants() {
    let payloads: serde_json::Value = serde_json::from_str(client_parity::TRUSTED).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (core, sub) = start(dir.path());
    let send = |button: &str| {
        let raw = payloads[button].clone();
        let _: Command = serde_json::from_value(raw.clone()).unwrap();
        let response: Response = serde_json::from_str(&core.execute_json(
            &serde_json::json!({"request_id":1,"command":raw}).to_string(),
            Origin::Ui,
        ))
        .unwrap();
        ok(response.result)
    };
    let first = send("register-agent").agent_token.unwrap();
    assert!(core.authenticate_agent_connection(&first).is_some());
    ui(
        &core,
        grant(
            "fixture",
            None,
            command("add_root"),
            Scope::All,
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    let next = send("renew-agent").agent_token.unwrap();
    assert!(core.authenticate_agent_connection(&first).is_none());
    assert!(core.authenticate_agent_connection(&next).is_some());
    send("remove-agent");
    assert!(core.authenticate_agent_connection(&next).is_none());
    let QueryResult::Permissions { grants, .. } = core.query(Query::Permissions {
        agent: Some("fixture".into()),
        offset: 0,
        limit: 100,
    }) else {
        panic!()
    };
    assert!(grants.is_empty());
    stop(core, sub);
}

#[test]
fn agent_cannot_indirectly_purge_expired_trash_and_trusted_cleanup_is_bound_and_audited() {
    let dir = tempfile::tempdir().unwrap();
    let (core, sub) = start(dir.path());
    let create = |name: &str| {
        ui(
            &core,
            Command::CreatePlaylist {
                name: name.into(),
                kind: PlaylistKind::Manual,
            },
        )
        .playlist
        .unwrap()
        .playlist
    };
    let old = create("expired mix");
    ui(&core, Command::DeletePlaylist { playlist: old });
    let conn = rusqlite::Connection::open(dir.path().join("library.db")).unwrap();
    conn.execute("UPDATE playlists SET deleted_at=1 WHERE id=?1", [old.0])
        .unwrap();
    let other = create("recent mix");
    ui(
        &core,
        grant(
            "claude",
            None,
            command("purge_playlists"),
            Scope::All,
            Expiry::Forever,
            Effect::Forbid,
        ),
    );
    ok(run(
        &core,
        Command::DeletePlaylist { playlist: other },
        session("claude", "s"),
    ));
    assert_eq!(core.describe_purge(None).unwrap().playlists.len(), 2);
    assert_eq!(
        err(run(
            &core,
            Command::PurgePlaylists { playlists: None },
            session("claude", "s")
        ))
        .code,
        ErrorCode::PermissionDenied
    );
    ok(run(
        &core,
        Command::RestorePlaylist { playlist: old },
        session("claude", "s"),
    ));
    ok(run(
        &core,
        Command::DeletePlaylist { playlist: old },
        session("claude", "s"),
    ));
    conn.execute("UPDATE playlists SET deleted_at=1 WHERE id=?1", [old.0])
        .unwrap();
    let expected = core.describe_purge(None).unwrap().expired_selection();
    assert_eq!(
        expected
            .playlists
            .iter()
            .map(|p| p.id.0)
            .collect::<Vec<_>>(),
        vec![old.0]
    );
    let request = || {
        Request::new(
            1,
            Command::PurgePlaylists {
                playlists: Some(vec![old]),
            },
        )
    };
    assert_eq!(
        err(core
            .execute_described_purge(request(), session("claude", "s"), expected.clone())
            .result)
        .code,
        ErrorCode::OriginNotAllowed
    );
    // Even restoring an unselected recent item invalidates the whole-trash review.
    ui(&core, Command::RestorePlaylist { playlist: other });
    assert_eq!(
        err(core
            .execute_described_purge(request(), Origin::Ui, expected)
            .result)
        .code,
        ErrorCode::PlaylistConflict
    );
    assert_eq!(core.describe_purge(None).unwrap().playlists.len(), 1);
    let expected = core.describe_purge(None).unwrap().expired_selection();
    ok(core
        .execute_described_purge(request(), Origin::Ui, expected)
        .result);
    assert!(core.describe_purge(None).unwrap().playlists.is_empty());
    let audit = actions(&core)
        .into_iter()
        .find(|a| a.kind == ActionKind::Purge)
        .unwrap();
    assert_eq!(audit.target_count, 1);
    assert!(!audit.undoable);
    assert!(audit.summary.contains("expired mix"));
    stop(core, sub);
}

/// The trusted host sends null for "all" throughout review; the transaction must
/// still compare the whole trash, even if an item arrives or leaves after review.
#[test]
fn trusted_null_purge_all_executes_over_100_and_refuses_added_or_removed_items() {
    let dir = tempfile::tempdir().unwrap();
    let (core, sub) = start(dir.path());
    let mut ids = Vec::new();
    for i in 0..102 {
        let id = ui(
            &core,
            Command::CreatePlaylist {
                name: format!("Trash {i}"),
                kind: PlaylistKind::Manual,
            },
        )
        .playlist
        .unwrap()
        .playlist;
        ui(&core, Command::DeletePlaylist { playlist: id });
        ids.push(id);
    }
    let execute = |guard| {
        core.execute_described_purge(
            Request::new(1, Command::PurgePlaylists { playlists: None }),
            Origin::Ui,
            guard,
        )
        .result
    };
    let selected = core.describe_purge(Some(&ids[..1])).unwrap();
    assert_eq!(err(execute(selected)).code, ErrorCode::OriginNotAllowed);
    // A genuine all guard cannot authorize an agent.
    assert_eq!(
        err(core
            .execute_described_purge(
                Request::new(1, Command::PurgePlaylists { playlists: None }),
                session("fixture", "s"),
                core.describe_purge(None).unwrap(),
            )
            .result)
        .code,
        ErrorCode::OriginNotAllowed
    );
    ui(&core, Command::RestorePlaylist { playlist: ids[101] });
    let before_add = core.describe_purge(None).unwrap();
    assert_eq!(before_add.playlists.len(), 101);
    ui(&core, Command::DeletePlaylist { playlist: ids[101] });
    assert_eq!(err(execute(before_add)).code, ErrorCode::PlaylistConflict);
    let before_remove = core.describe_purge(None).unwrap();
    ui(&core, Command::RestorePlaylist { playlist: ids[101] });
    assert_eq!(
        err(execute(before_remove)).code,
        ErrorCode::PlaylistConflict
    );
    let fresh = core.describe_purge(None).unwrap();
    assert!(fresh.all);
    assert_eq!(fresh.playlists.len(), 101);
    assert!(ok(execute(fresh)).changed);
    assert!(core.describe_purge(None).unwrap().playlists.is_empty());
    stop(core, sub);
}

#[test]
fn trusted_pending_recovery_is_ui_only_authoritative_and_expires() {
    let dir = tempfile::tempdir().unwrap();
    let (core, sub) = start_with_config(
        dir.path(),
        CoreConfig {
            confirmation_ttl: Some(Duration::from_millis(300)),
            ..CoreConfig::default()
        },
    );
    let music = folder(dir.path(), "pending-music");
    let origin = session("test:host", "pending-1");
    assert!(is_needs_confirmation(&run(
        &core,
        add_root(&music),
        origin.clone()
    )));
    let first_read = core.pending_confirmations_snapshot(Origin::Ui).unwrap();
    let second_read = core.pending_confirmations_snapshot(Origin::Ui).unwrap();
    assert!(second_read.revision > first_read.revision);
    let pending = second_read.pending;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].requested_by, origin);
    assert!(pending[0].scope.is_some());
    for other in [
        Origin::System,
        Origin::MediaControls,
        Origin::agent("a"),
        Origin::Remote { device: "x".into() },
    ] {
        assert_eq!(
            core.pending_confirmations(other).unwrap_err().code,
            ErrorCode::OriginNotAllowed
        );
    }
    ui(
        &core,
        Command::Reject {
            token: pending[0].token.clone(),
        },
    );
    assert!(core.pending_confirmations(Origin::Ui).unwrap().is_empty());
    assert!(is_needs_confirmation(&run(&core, add_root(&music), origin)));
    std::thread::sleep(Duration::from_millis(350));
    assert!(core.pending_confirmations(Origin::Ui).unwrap().is_empty());
    stop(core, sub);
}

#[test]
fn trusted_guarded_undo_rechecks_state_and_rejects_other_origins() {
    let dir = tempfile::tempdir().unwrap();
    let (core, sub) = start(dir.path());
    let music = folder(dir.path(), "undo-music");
    let receipt = ui(&core, add_root(&music));
    let action = receipt.action.unwrap();
    let description = core.describe_undo(action).unwrap();
    assert!(!description.writes_files);
    for other in [
        Origin::System,
        Origin::MediaControls,
        Origin::agent("a"),
        Origin::Remote { device: "x".into() },
    ] {
        let result = core.execute_described_undo(
            Request::new(1, Command::UndoAction { action }),
            other,
            description.clone(),
        );
        assert_eq!(err(result.result).code, ErrorCode::OriginNotAllowed);
    }
    let undone = ok(core
        .execute_described_undo(
            Request::new(2, Command::UndoAction { action }),
            Origin::Ui,
            description.clone(),
        )
        .result);
    assert!(matches!(
        core.execute_described_undo(
            Request::new(3, Command::UndoAction { action }),
            Origin::Ui,
            description
        )
        .result,
        CommandResult::Err { .. }
    ));
    let redo = core.describe_undo(undone.action.unwrap()).unwrap();
    assert_eq!(redo.chain.len(), 2);
    ok(core
        .execute_described_undo(
            Request::new(
                4,
                Command::UndoAction {
                    action: ActionId(redo.action.0),
                },
            ),
            Origin::Ui,
            redo,
        )
        .result);
    stop(core, sub);
}

#[test]
fn trusted_pending_table_recovers_a_confirmation_lost_to_subscription_overflow() {
    let dir = tempfile::tempdir().unwrap();
    let (core, sub) = start(dir.path());
    for i in 0..300 {
        ui(&core, Command::SetMuted { muted: i % 2 == 0 });
    }
    let music = folder(dir.path(), "overflow-pending");
    assert!(is_needs_confirmation(&run(
        &core,
        add_root(&music),
        session("test:host", "overflow-1")
    )));
    let end = Instant::now() + Duration::from_secs(5);
    let mut resync = false;
    while Instant::now() < end {
        if let Ok(event) = sub.events.recv_timeout(Duration::from_millis(100))
            && matches!(event.event, Event::Resync { .. })
        {
            resync = true;
            break;
        }
    }
    assert!(resync);
    let pending = core.pending_confirmations(Origin::Ui).unwrap();
    assert_eq!(pending.len(), 1);
    ui(
        &core,
        Command::Reject {
            token: pending[0].token.clone(),
        },
    );
    assert!(core.pending_confirmations(Origin::Ui).unwrap().is_empty());
    stop(core, sub);
}

/// Replaces hand-entered root/import/export path probes in the retired debug UI.
#[test]
fn ui_path_errors_are_structured_and_do_not_mutate_the_library() {
    let dir = tempfile::tempdir().unwrap();
    let (core, sub) = start(dir.path());
    let playlist = new_playlist(&core, "path probe");
    let missing = dir.path().join("absent");
    let regular = dir.path().join("regular.m3u");
    std::fs::write(&regular, "#EXTM3U\n").unwrap();
    let directory = folder(dir.path(), "folder.m3u");
    let mut paths = vec![text(&missing), "bad\0path.m3u".into()];
    paths.push("/pmp-dgroup-missing-parent/list.m3u".into());
    let windows_paths = [
        r"Z:\pmp-dgroup-missing-parent\list.m3u",
        r"C:relative\list.m3u",
    ];
    if cfg!(windows) {
        paths.extend(windows_paths.map(str::to_owned));
    }
    // On POSIX a backslash is a legal filename character. Raw Windows spellings are
    // refused at the agent's local-path boundary, rather than changing UI semantics.
    #[cfg(unix)]
    for path in windows_paths {
        for command in [
            Command::AddRoot { path: path.into() },
            Command::ImportM3u {
                path: path.into(),
                name: None,
            },
            Command::ExportM3u {
                playlist,
                path: path.into(),
            },
        ] {
            let e = err(run(&core, command, Origin::agent("path-probe")));
            assert_eq!(e.code, ErrorCode::InvalidLocation);
            assert_eq!(e.details.field.as_deref(), Some("command.path"));
        }
    }
    for path in paths {
        for command in [
            Command::AddRoot { path: path.clone() },
            Command::ImportM3u {
                path: path.clone(),
                name: None,
            },
            Command::ExportM3u {
                playlist,
                path: path.clone(),
            },
        ] {
            let result = run(&core, command.clone(), Origin::Ui);
            let CommandResult::Err { error: e } = result else {
                panic!("{command:?}: {result:?}")
            };
            assert_eq!(
                e.details.field.as_deref(),
                Some("command.path"),
                "{command:?}: {e:?}"
            );
            assert!(
                matches!(
                    e.code,
                    ErrorCode::NotADirectory
                        | ErrorCode::FileNotFound
                        | ErrorCode::NotAnM3u
                        | ErrorCode::Io
                ),
                "{command:?}: {e:?}"
            );
        }
    }
    let e = err(run(
        &core,
        Command::AddRoot {
            path: text(&regular),
        },
        Origin::Ui,
    ));
    assert_eq!(e.code, ErrorCode::NotADirectory);
    let e = err(run(
        &core,
        Command::ImportM3u {
            path: text(&directory),
            name: None,
        },
        Origin::Ui,
    ));
    assert_eq!(e.code, ErrorCode::NotARegularFile);
    assert_eq!(e.details.field.as_deref(), Some("command.path"));
    assert!(matches!(core.query(Query::Roots), QueryResult::Roots { roots } if roots.is_empty()));
    assert_eq!(std::fs::read_to_string(regular).unwrap(), "#EXTM3U\n");
    stop(core, sub);
}

#[test]
fn authenticated_agent_cannot_replay_ui_confirmation_in_another_session() {
    let dir = tempfile::tempdir().unwrap();
    let root = folder(dir.path(), "music");
    let (core, sub) = start(dir.path());
    let credential = ui(
        &core,
        Command::RegisterAgent {
            name: "replay".into(),
        },
    )
    .agent_token
    .unwrap();
    let agent = core.authenticate_agent_connection(&credential).unwrap();
    let result: Response = serde_json::from_str(
        &agent.execute_json(
            &serde_json::json!({"request_id":1,"command":{"type":"add_root","path":text(&root)}})
                .to_string(),
            "session-one",
        ),
    )
    .unwrap();
    assert_eq!(err(result.result).code, ErrorCode::NeedsConfirmation);
    let token = confirmation_of(&sub).token;
    for session in ["session-one", "session-two"] {
        let response: Response = serde_json::from_str(
            &agent.execute_json(
                &serde_json::json!({"request_id":2,"command":{"type":"confirm","token":token}})
                    .to_string(),
                session,
            ),
        )
        .unwrap();
        assert_eq!(err(response.result).code, ErrorCode::OriginNotAllowed);
    }
    core.end_agent_session("replay", "session-one");
    assert_eq!(
        err(run(&core, Command::Confirm { token }, Origin::Ui)).code,
        ErrorCode::ConfirmationNotFound
    );
    assert!(matches!(core.query(Query::Roots), QueryResult::Roots { roots } if roots.is_empty()));
    stop(core, sub);
}
