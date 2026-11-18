//! Grants scenarios.
use super::*;

#[test]
fn permanent_delete_binds_ids_and_refuses_changed_trash_without_reinterpreting_null() {
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
    let a = create("Reviewed mix");
    ui(&core, Command::DeletePlaylist { playlist: a });
    let token = ui(
        &core,
        Command::RegisterAgent {
            name: "fixture".into(),
        },
    )
    .agent_token
    .unwrap();
    let agent = core.authenticate_agent_connection(&token).unwrap();
    let hold = || {
        assert!(matches!(
            agent
                .execute(
                    Request::new(1, Command::PurgePlaylists { playlists: None }),
                    "binding-session",
                )
                .result,
            CommandResult::Err { error } if error.code == ErrorCode::NeedsConfirmation,
        ));
        core.pending_confirmations(Origin::Ui)
            .unwrap()
            .pop()
            .unwrap()
    };
    let pending = hold();
    assert_eq!(
        pending.command,
        Command::PurgePlaylists {
            playlists: Some(vec![a])
        }
    );
    assert!(pending.summary.contains("Reviewed mix") && pending.summary.contains('0'));
    let b = create("Arrived later");
    ui(&core, Command::DeletePlaylist { playlist: b });
    let result = ui(
        &core,
        Command::Confirm {
            token: pending.token,
        },
    )
    .confirmed
    .unwrap();
    assert!(matches!(
        *result.result.unwrap(),
        CommandResult::Err { error } if error.code == ErrorCode::PlaylistConflict,
    ));
    let selected = core.describe_purge(Some(&[a])).unwrap();
    // This Rust API grants no authority to an Agent, even with a genuine description.
    assert!(matches!(
        core.execute_described_purge(
            Request::new(
                1,
                Command::PurgePlaylists {
                    playlists: Some(vec![a])
                }
            ),
            session("fixture", "s"),
            selected.clone()
        )
        .result,
        CommandResult::Err { .. }
    ));
    let pending = hold();
    ui(&core, Command::RestorePlaylist { playlist: a });
    let result = ui(
        &core,
        Command::Confirm {
            token: pending.token,
        },
    )
    .confirmed
    .unwrap();
    assert!(matches!(*result.result.unwrap(), CommandResult::Err { .. }));
    assert!(matches!(
        core.execute_described_purge(
            Request::new(
                1,
                Command::PurgePlaylists {
                    playlists: Some(vec![a])
                }
            ),
            Origin::Ui,
            selected
        )
        .result,
        CommandResult::Err { .. }
    ));
    ui(&core, Command::DeletePlaylist { playlist: a });
    let pending = hold();
    let result = ui(
        &core,
        Command::Confirm {
            token: pending.token,
        },
    )
    .confirmed
    .unwrap();
    assert!(matches!(*result.result.unwrap(), CommandResult::Ok(_)));
    let QueryResult::Playlists { playlists, .. } = core.query(Query::Playlists {
        deleted: true,
        offset: 0,
        limit: 100,
    }) else {
        panic!()
    };
    assert!(playlists.is_empty());
    stop(core, sub);
}

#[test]
fn pending_recovery_keeps_creation_order_across_ticket_counter_digits() {
    let dir = tempfile::tempdir().unwrap();
    let (core, sub) = start(dir.path());
    let id = ui(
        &core,
        Command::CreatePlaylist {
            name: "fixture".into(),
            kind: PlaylistKind::Manual,
        },
    )
    .playlist
    .unwrap()
    .playlist;
    ui(&core, Command::DeletePlaylist { playlist: id });
    let mut tokens = vec![];
    for _ in 0..15 {
        let error = err(run(
            &core,
            Command::PurgePlaylists { playlists: None },
            session("fixture", "arrival"),
        ));
        tokens.push(error.details.confirmation.unwrap().token);
    }
    assert_eq!(
        core.pending_confirmations(Origin::Ui)
            .unwrap()
            .into_iter()
            .map(|p| p.token)
            .collect::<Vec<_>>(),
        tokens
    );
    stop(core, sub);
}

#[test]
fn only_the_ui_manages_agents_and_grants() {
    let dir = tempfile::tempdir().unwrap();
    let (core, sub) = start(dir.path());
    let token = ui(
        &core,
        Command::RegisterAgent {
            name: "claude".into(),
        },
    )
    .agent_token
    .unwrap();
    assert!(core.authenticate_agent(&token).is_some());
    let allow_all = grant(
        "claude",
        None,
        class(OperationClass::LocalPath),
        Scope::All,
        Expiry::Forever,
        Effect::Allow,
    );
    let commands = [
        Command::RegisterAgent { name: "me".into() },
        Command::RenewAgentToken {
            agent: "claude".into(),
        },
        Command::RemoveAgent {
            agent: "claude".into(),
        },
        allow_all.clone(),
        Command::RevokePermission { grant: GrantId(1) },
    ];
    for command in &commands {
        assert_eq!(command.policy(&Origin::Ui), Policy::Allow);
        for origin in [
            Origin::agent("claude"),
            session("claude", "s1"),
            Origin::Remote {
                device: "phone".into(),
            },
            Origin::MediaControls,
            Origin::System,
        ] {
            let e = err(run(&core, command.clone(), origin.clone()));
            assert_eq!(
                (e.kind, e.code),
                (ErrorKind::Forbidden, ErrorCode::OriginNotAllowed),
                "{command:?} from {origin:?}"
            );
        }
    }
    // Nothing changed: still one agent, no grants, and nothing announced.
    assert_eq!(agents(&core).len(), 1);
    assert!(grants(&core, None).is_empty());
    assert!(
        drain(&sub)
            .iter()
            .filter(|e| matches!(e.event, Event::PermissionsChanged | Event::AgentsChanged))
            .count()
            == 1,
        "only the registration"
    );
    // The schema says so too.
    let schema = json_schema();
    for name in [
        "register_agent",
        "renew_agent_token",
        "remove_agent",
        "grant_permission",
        "revoke_permission",
    ] {
        let variant = schema["$defs"]["Command"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| variant_tag(v) == Some(name))
            .unwrap();
        assert_eq!(variant["x-class"], "permissions", "{name}");
        for (origin, policy) in variant["x-origins"].as_object().unwrap() {
            let expected = if origin == "ui" { "allow" } else { "forbidden" };
            assert_eq!(policy, expected, "{name} {origin}");
        }
    }
}

#[test]
fn grants_decide_for_agents_what_would_need_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let music = folder(dir.path(), "music");
    let (jazz, rock) = (folder(&music, "jazz"), folder(&music, "rock"));
    let elsewhere = folder(dir.path(), "elsewhere");
    let (core, sub) = start(dir.path());
    let s1 = session("claude", "s1");
    let s2 = session("claude", "s2");

    // No grant: the user is asked, and told what to allow where.
    let held = run(&core, add_root(&jazz), s1.clone());
    assert!(is_needs_confirmation(&held), "{held:?}");
    let asked = wait_event(&sub, |e| {
        matches!(e.event, Event::ConfirmationRequested { .. })
    });
    let Event::ConfirmationRequested { confirmation } = asked.event else {
        unreachable!()
    };
    assert_eq!(confirmation.requested_by, s1, "with its session");
    assert_eq!(asked.origin, Some(s1.clone()));
    assert_eq!(
        confirmation.operation,
        Some(OperationInfo {
            class: OperationClass::LocalPath,
            command: "add_root".into()
        })
    );
    assert_eq!(confirmation.scope, Some(location(&jazz)));
    assert_eq!(confirmation.display_location, Some(text(&jazz)));

    // "Allow for this session" (in /music): a session grant, then the confirmation.
    let session_grant = ui(
        &core,
        grant(
            "claude",
            Some("s1"),
            class(OperationClass::LocalPath),
            location(&music),
            Expiry::Session,
            Effect::Allow,
        ),
    )
    .grant
    .unwrap();
    // The grant does not answer the pending request: still pending, then confirmed.
    let events = drain(&sub);
    assert_eq!(
        events
            .iter()
            .map(|e| (&e.event, &e.origin))
            .collect::<Vec<_>>(),
        [(&Event::PermissionsChanged, &Some(Origin::Ui))]
    );
    let confirmed = ui(
        &core,
        Command::Confirm {
            token: confirmation.token,
        },
    );
    let outcome = confirmed.confirmed.unwrap();
    assert!(outcome.approved);
    let CommandResult::Ok(by_user) = *outcome.result.unwrap() else {
        panic!()
    };
    assert_eq!(
        action(&core, by_user.action.unwrap()).grant,
        None,
        "the user allowed this one"
    );

    // From now on this session adds folders in /music without asking …
    let added = ok(run(&core, add_root(&rock), s1.clone()));
    let jazz_root = match core.query(Query::Roots) {
        QueryResult::Roots { roots } => roots.iter().find(|r| r.path == text(&jazz)).unwrap().id,
        other => panic!("{other:?}"),
    };
    let rock_root = added.root.unwrap();
    // … and the journal says which grant allowed it.
    let recorded = action(&core, added.action.unwrap());
    assert_eq!(
        (recorded.kind, recorded.grant, recorded.origin.label()),
        (
            ActionKind::RootAdd,
            Some(session_grant),
            "agent:claude".into()
        )
    );
    // Not another session, agent or place; not the agent without a session.
    assert!(is_needs_confirmation(&run(
        &core,
        add_root(&elsewhere),
        s1.clone()
    )));
    let e = err(run(
        &core,
        Command::RemoveRoot { root: jazz_root },
        s2.clone(),
    ));
    assert_eq!(e.kind, ErrorKind::NeedsConfirmation);
    let e = err(run(
        &core,
        Command::RemoveRoot { root: jazz_root },
        Origin::agent("claude"),
    ));
    assert_eq!(e.kind, ErrorKind::NeedsConfirmation);
    let e = err(run(
        &core,
        Command::RemoveRoot { root: jazz_root },
        session("codex", "s1"),
    ));
    assert_eq!(e.kind, ErrorKind::NeedsConfirmation);
    // Not another class: importing M3U files is `local_path` too, exporting is not.
    let mix = music.join("mix.m3u");
    std::fs::write(&mix, "#EXTM3U\n").unwrap();
    let imported = ok(run(
        &core,
        Command::ImportM3u {
            path: text(&mix),
            name: None,
        },
        s1.clone(),
    ));
    let playlist = imported.playlist.unwrap().playlist;
    let export = Command::ExportM3u {
        playlist,
        path: text(&music.join("out.m3u")),
    };
    assert!(is_needs_confirmation(&run(
        &core,
        export.clone(),
        s1.clone()
    )));

    // A forbid wins over any allow, for every session.
    let forbid = ui(
        &core,
        grant(
            "claude",
            None,
            command("remove_root"),
            Scope::Library {
                root: rock_root,
                prefix: None,
            },
            Expiry::Forever,
            Effect::Forbid,
        ),
    )
    .grant
    .unwrap();
    let e = err(run(
        &core,
        Command::RemoveRoot { root: rock_root },
        s1.clone(),
    ));
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::Forbidden, ErrorCode::PermissionDenied),
        "{e:?}"
    );
    // The grant is named (message and details), with what it forbids and how much of the
    // command, not where: the agent can tell the user which.
    assert_eq!(
        e.details.forbidden_by,
        [ForbiddenBy {
            grant: forbid,
            operation: command("remove_root"),
            covers: Covers::All,
        }]
    );
    assert!(
        e.message.contains(&format!(
            "forbidden by grant {} (remove_root: all of what it acts on)",
            forbid.0
        )),
        "{}",
        e.message
    );
    says_nowhere(&e, &[&text(&rock), "library folder"]);
    // The removal of the other folder is still allowed (its location is in /music).
    let removed = ok(run(
        &core,
        Command::RemoveRoot { root: jazz_root },
        s1.clone(),
    ));
    assert_eq!(
        action(&core, removed.action.unwrap()).grant,
        Some(session_grant)
    );
    // Undoing that is a folder change of the jazz root: allowed by the same grant.
    let undone = ok(run(
        &core,
        Command::UndoAction {
            action: removed.action.unwrap(),
        },
        s1.clone(),
    ));
    assert_eq!(
        action(&core, undone.action.unwrap()).grant,
        Some(session_grant)
    );
    // Revoked: asked again.
    ui(&core, Command::RevokePermission { grant: forbid });
    assert!(is_needs_confirmation(&run(
        &core,
        Command::RemoveRoot { root: rock_root },
        s2.clone()
    )));
    let e = err(run(
        &core,
        Command::RevokePermission { grant: forbid },
        Origin::Ui,
    ));
    assert_eq!(
        (e.code, e.details.missing.clone()),
        (
            ErrorCode::GrantNotFound,
            vec![ObjectRef::Grant { id: forbid }]
        )
    );

    // A part of a root: only library targets under its prefix (a root itself is not).
    ui(
        &core,
        grant(
            "claude",
            Some("s2"),
            class(OperationClass::LocalPath),
            Scope::Library {
                root: rock_root,
                prefix: Some("live".into()),
            },
            Expiry::Session,
            Effect::Allow,
        ),
    );
    assert!(is_needs_confirmation(&run(
        &core,
        Command::RemoveRoot { root: rock_root },
        s2.clone()
    )));
    // The whole root: also the root itself, and files inside its location.
    let whole = ui(
        &core,
        grant(
            "claude",
            Some("s2"),
            Operation::Class {
                class: OperationClass::FileWrite,
            },
            Scope::Library {
                root: rock_root,
                prefix: None,
            },
            Expiry::Session,
            Effect::Allow,
        ),
    )
    .grant
    .unwrap();
    let into_rock = Command::ExportM3u {
        playlist,
        path: text(&rock.join("rock.m3u")),
    };
    drain(&sub);
    let first = ok(run(&core, into_rock.clone(), s2.clone()));
    // An agent's new file cannot be undone either: an audit entry, listed (not in the
    // receipt, whose `action` can be undone), with the grant.
    assert_eq!(first.action, None);
    let created = actions(&core).remove(0);
    assert!(
        created.summary.starts_with("write the new file "),
        "{}",
        created.summary
    );
    assert!(
        drain(&sub)
            .iter()
            .any(|e| e.event == Event::ActionsChanged && e.origin == Some(s2.clone()))
    );
    // Replacing it: another one.
    let again = ok(run(&core, into_rock, s2.clone()));
    assert_eq!(again.action, None);
    let audit = actions(&core).remove(0);
    assert_ne!(audit.id, created.id);
    for entry in [&created, &audit] {
        assert_eq!(
            (
                entry.kind,
                entry.undoable,
                entry.grant,
                entry.origin.label()
            ),
            (
                ActionKind::FileWrite,
                false,
                Some(whole),
                "agent:claude".into()
            )
        );
    }
    assert!(
        audit.summary.starts_with("replace the file "),
        "{}",
        audit.summary
    );
    assert!(
        is_needs_confirmation(&run(&core, export, s2.clone())),
        "outside the root"
    );

    // A forbid also refuses what an agent may do without asking by default; another agent is not affected.
    let no_edits = ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::LibraryEdit),
            Scope::All,
            Expiry::Forever,
            Effect::Forbid,
        ),
    )
    .grant
    .unwrap();
    let create = Command::CreatePlaylist {
        name: "not any more".into(),
        kind: PlaylistKind::Manual,
    };
    let e = err(run(&core, create.clone(), s1.clone()));
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::Forbidden, ErrorCode::PermissionDenied)
    );
    ok(run(&core, create, session("codex", "s1")));
    ui(&core, Command::RevokePermission { grant: no_edits });
    // And remotes have no grants.
    let e = err(run(
        &core,
        add_root(&jazz),
        Origin::Remote {
            device: "claude".into(),
        },
    ));
    assert_eq!(e.code, ErrorCode::PathNotAllowed);
}

#[test]
fn permanently_deleting_always_asks_an_agent() {
    let dir = tempfile::tempdir().unwrap();
    let (core, sub) = start(dir.path());
    let doomed = ui(
        &core,
        Command::CreatePlaylist {
            name: "Old mix".into(),
            kind: PlaylistKind::Manual,
        },
    )
    .playlist
    .unwrap()
    .playlist;
    ui(&core, Command::DeletePlaylist { playlist: doomed });
    let purge = Command::PurgePlaylists { playlists: None };

    // No grant can allow it …
    for operation in [class(OperationClass::Purge), command("purge_playlists")] {
        for scope in [
            Scope::All,
            Scope::Location {
                location: "/".into(),
            },
        ] {
            let e = err(run(
                &core,
                grant(
                    "claude",
                    None,
                    operation.clone(),
                    scope,
                    Expiry::Forever,
                    Effect::Allow,
                ),
                Origin::Ui,
            ));
            assert_eq!(
                (e.kind, e.code, e.details.field.as_deref()),
                (
                    ErrorKind::Invalid,
                    ErrorCode::NotGrantable,
                    Some("command.operation")
                )
            );
        }
    }
    // … (nor the user's own commands) …
    let e = err(run(
        &core,
        grant(
            "claude",
            None,
            command("confirm"),
            Scope::All,
            Expiry::Forever,
            Effect::Allow,
        ),
        Origin::Ui,
    ));
    assert_eq!(e.code, ErrorCode::NotGrantable);
    assert!(grants(&core, None).is_empty());
    let held = err(run(&core, purge.clone(), session("claude", "s")));
    assert_eq!(held.kind, ErrorKind::NeedsConfirmation);
    let token = held.details.confirmation.unwrap().token;
    ui(&core, Command::Reject { token });

    // … but one can forbid it.
    ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::Purge),
            Scope::All,
            Expiry::Forever,
            Effect::Forbid,
        ),
    );
    let e = err(run(&core, purge.clone(), session("claude", "s")));
    assert_eq!(e.code, ErrorCode::PermissionDenied);

    // The user purges: an audit entry, announced, that cannot be undone. The receipt does
    // not name it (its `action` is what can be undone).
    drain(&sub);
    let purged = ui(&core, purge.clone());
    assert_eq!((purged.changed, purged.action), (true, None));
    let audit = actions(&core).remove(0);
    assert_eq!(
        (
            audit.kind,
            audit.summary.as_str(),
            audit.undoable,
            audit.grant,
            audit.origin
        ),
        (
            ActionKind::Purge,
            "permanently delete playlist \"Old mix\"",
            false,
            None,
            Origin::Ui
        )
    );
    assert!(
        drain(&sub)
            .iter()
            .any(|e| e.event == Event::ActionsChanged && e.origin == Some(Origin::Ui))
    );
    let e = err(run(
        &core,
        Command::UndoAction { action: audit.id },
        Origin::Ui,
    ));
    assert_eq!(e.code, ErrorCode::ActionNotFound);
    // An empty trash: nothing to record.
    let before = actions(&core).len();
    let again = ui(&core, purge.clone());
    assert_eq!((again.changed, again.action), (false, None));
    assert_eq!(actions(&core).len(), before);
    // A remote's purge asks the user too: grants are for agents only (a forbid for an agent
    // of the same name does not touch it).
    let held = err(run(
        &core,
        purge,
        Origin::Remote {
            device: "claude".into(),
        },
    ));
    assert_eq!(held.kind, ErrorKind::NeedsConfirmation);
}

#[test]
fn lasting_grants_survive_a_restart_and_session_grants_do_not() {
    let dir = tempfile::tempdir().unwrap();
    let music = folder(dir.path(), "music");
    let (core, sub) = start(dir.path());
    let lasting = [
        (Expiry::Forever, Effect::Allow),
        (
            Expiry::At {
                at_ms: unix_ms() + 3_600_000,
            },
            Effect::Forbid,
        ),
    ];
    let mut kept = Vec::new();
    for (until, effect) in lasting {
        kept.push(
            ui(
                &core,
                grant(
                    "claude",
                    None,
                    class(OperationClass::LocalPath),
                    location(&music),
                    until,
                    effect,
                ),
            )
            .grant
            .unwrap(),
        );
    }
    // A grant for one session lasts for that session only.
    let e = err(run(
        &core,
        grant(
            "claude",
            Some("conn-1"),
            class(OperationClass::LocalPath),
            location(&music),
            Expiry::Forever,
            Effect::Allow,
        ),
        Origin::Ui,
    ));
    assert_eq!(
        (e.code, e.details.field.as_deref()),
        (ErrorCode::InvalidRequest, Some("command.until"))
    );
    let gone = ui(
        &core,
        grant(
            "claude",
            Some("conn-1"),
            command("import_m3u"),
            Scope::All,
            Expiry::Session,
            Effect::Allow,
        ),
    )
    .grant
    .unwrap();
    let before = grants(&core, None);
    assert_eq!(
        before.iter().map(|g| g.id).collect::<Vec<_>>(),
        [kept[0], kept[1], gone]
    );
    stop(core, sub);

    let (core, _sub) = start(dir.path());
    let after = grants(&core, None);
    assert_eq!(after, before[..2], "as they were, without the session's");
    let e = err(run(
        &core,
        Command::RevokePermission { grant: gone },
        Origin::Ui,
    ));
    assert_eq!(e.code, ErrorCode::GrantNotFound);
    // Ids are never handed out again, not even the session grant's.
    let next = ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::FileWrite),
            Scope::All,
            Expiry::Session,
            Effect::Allow,
        ),
    )
    .grant
    .unwrap();
    assert!(next > gone, "{next:?} after {gone:?}");
    // The forbid still wins, for every session.
    let e = err(run(&core, add_root(&music), session("claude", "conn-2")));
    assert_eq!(e.code, ErrorCode::PermissionDenied);
}

#[test]
fn grants_expire() {
    let dir = tempfile::tempdir().unwrap();
    let music = folder(dir.path(), "music");
    let (core, sub) = start(dir.path());
    let id = ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::LocalPath),
            Scope::All,
            Expiry::At {
                at_ms: unix_ms() + 1_000,
            },
            Effect::Forbid,
        ),
    )
    .grant
    .unwrap();
    assert_eq!(grants(&core, None).len(), 1);
    let e = err(run(&core, add_root(&music), Origin::agent("claude")));
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    // Pruned, and announced by core itself.
    let expired = wait_event(&sub, |e| {
        e.event == Event::PermissionsChanged && e.origin.is_none()
    });
    assert_eq!(expired.origin, None);
    assert!(grants(&core, None).is_empty());
    let e = err(run(
        &core,
        Command::RevokePermission { grant: id },
        Origin::Ui,
    ));
    assert_eq!(e.code, ErrorCode::GrantNotFound);
    assert!(is_needs_confirmation(&run(
        &core,
        add_root(&music),
        Origin::agent("claude")
    )));
    stop(core, sub);
    let (core, _sub) = start(dir.path());
    assert!(grants(&core, None).is_empty(), "gone from the database too");
}

#[test]
fn grant_requests_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let music = folder(dir.path(), "music");
    let (core, _sub) = start(dir.path());
    let root = ui(&core, add_root(&music)).root.unwrap();
    let base = |operation: Operation, scope: Scope, until: Expiry| {
        grant("claude", None, operation, scope, until, Effect::Allow)
    };
    let local = || class(OperationClass::LocalPath);
    for (command, code, field) in [
        (
            base(command("teleport"), Scope::All, Expiry::Forever),
            ErrorCode::UnknownCommand,
            "command.operation.command",
        ),
        (
            base(
                local(),
                Scope::All,
                Expiry::At {
                    at_ms: unix_ms() - 1,
                },
            ),
            ErrorCode::OutOfRange,
            "command.until.at_ms",
        ),
        (
            base(
                local(),
                Scope::Location {
                    location: " ".into(),
                },
                Expiry::Forever,
            ),
            ErrorCode::InvalidLocation,
            "command.scope.location",
        ),
        (
            base(
                local(),
                Scope::Library {
                    root: RootId(999),
                    prefix: None,
                },
                Expiry::Forever,
            ),
            ErrorCode::RootNotFound,
            "command.scope.root",
        ),
        (
            Command::GrantPermission {
                agent: "".into(),
                session: None,
                operation: local(),
                scope: Scope::All,
                until: Expiry::Forever,
                effect: Effect::Allow,
            },
            ErrorCode::InvalidAgentName,
            "command.agent",
        ),
        (
            Command::GrantPermission {
                agent: "claude".into(),
                session: Some("".into()),
                operation: local(),
                scope: Scope::All,
                until: Expiry::Forever,
                effect: Effect::Allow,
            },
            ErrorCode::InvalidRequest,
            "command.session",
        ),
    ] {
        let e = err(run(&core, command.clone(), Origin::Ui));
        assert_eq!(
            (e.code, e.details.field.as_deref()),
            (code, Some(field)),
            "{command:?}"
        );
    }
    // Over JSON, values this build does not know are refused as such.
    for (json, field) in [
        (
            r#"{"type":"grant_permission","agent":"a","operation":{"type":"class","class":"teleport"},"scope":{"type":"all"},"until":{"type":"forever"},"effect":"allow"}"#,
            "command.operation.class",
        ),
        (
            r#"{"type":"grant_permission","agent":"a","operation":{"type":"class","class":"local_path"},"scope":{"type":"galaxy"},"until":{"type":"forever"},"effect":"allow"}"#,
            "command.scope",
        ),
        (
            r#"{"type":"grant_permission","agent":"a","operation":{"type":"class","class":"local_path"},"scope":{"type":"all"},"until":{"type":"never"},"effect":"allow"}"#,
            "command.until",
        ),
        (
            r#"{"type":"grant_permission","agent":"a","operation":{"type":"class","class":"local_path"},"scope":{"type":"all"},"until":{"type":"forever"},"effect":"maybe"}"#,
            "command.effect",
        ),
    ] {
        let answer: serde_json::Value = serde_json::from_str(&core.execute_json(
            &format!(r#"{{"request_id":1,"command":{json}}}"#),
            Origin::Ui,
        ))
        .unwrap();
        assert_eq!(
            answer["result"]["error"]["code"], "unknown_variant",
            "{json}"
        );
        assert_eq!(
            answer["result"]["error"]["details"]["field"], field,
            "{json}"
        );
    }
    // A prefix is kept without its slashes; an empty one is the whole root.
    ui(
        &core,
        base(
            local(),
            Scope::Library {
                root,
                prefix: Some("/Jazz/".into()),
            },
            Expiry::Forever,
        ),
    );
    ui(
        &core,
        base(
            local(),
            Scope::Library {
                root,
                prefix: Some("/".into()),
            },
            Expiry::Forever,
        ),
    );
    let scopes: Vec<Scope> = grants(&core, None).into_iter().map(|g| g.scope).collect();
    assert_eq!(
        scopes,
        [
            Scope::Library {
                root,
                prefix: Some("Jazz".into())
            },
            Scope::Library { root, prefix: None }
        ]
    );
    // Paged like every list.
    match core.query(Query::Permissions {
        agent: None,
        offset: 1,
        limit: 5,
    }) {
        QueryResult::Permissions {
            grants,
            offset,
            total,
            next_offset,
        } => assert_eq!((grants.len(), offset, total, next_offset), (1, 1, 2, None)),
        other => panic!("{other:?}"),
    }
    match core.query(Query::Permissions {
        agent: None,
        offset: 0,
        limit: 1,
    }) {
        QueryResult::Permissions { next_offset, .. } => assert_eq!(next_offset, Some(1)),
        other => panic!("{other:?}"),
    }
}

/// Grants that could never apply are refused; a forbid also tightens what an agent may do
/// without asking; names, sessions and locations are
/// checked, and locations stored in normal form.
#[test]
fn grants_say_only_what_can_apply() {
    let dir = tempfile::tempdir().unwrap();
    let music = folder(dir.path(), "music");
    folder(&music, "jazz");
    let (core, _sub) = start(dir.path());
    let refused = |command: Command| err(run(&core, command, Origin::Ui));
    let all = |operation: Operation, effect: Effect| {
        grant(
            "claude",
            None,
            operation,
            Scope::All,
            Expiry::Forever,
            effect,
        )
    };
    // An allow only for what an agent would be asked about.
    for operation in [
        command("set_rating"),
        command("play"),
        command("create_playlist"),
        class(OperationClass::LibraryEdit),
    ] {
        let e = refused(all(operation.clone(), Effect::Allow));
        assert_eq!(
            (e.code, e.details.field.as_deref()),
            (ErrorCode::NotGrantable, Some("command.operation")),
            "{operation:?}"
        );
    }
    ui(&core, all(class(OperationClass::Playback), Effect::Allow));
    ui(&core, all(command("undo_action"), Effect::Allow));
    // A forbid of the user's own commands means nothing: agents cannot send them.
    for operation in [
        command("confirm"),
        command("grant_permission"),
        class(OperationClass::Permissions),
    ] {
        assert_eq!(
            refused(all(operation.clone(), Effect::Forbid)).code,
            ErrorCode::NotGrantable,
            "{operation:?}"
        );
    }
    // A forbid refuses what is allowed by default: library edits, playback.
    for operation in [command("set_rating"), class(OperationClass::Playback)] {
        ui(&core, all(operation, Effect::Forbid));
    }
    let play = Command::Play {
        context: PlayContext::Tracks {
            tracks: vec![TrackId(1)],
        },
        start: 0,
        start_track: None,
    };
    for (command, others_may) in [
        (
            Command::SetRating {
                tracks: Vec::new(),
                rating: Some(5),
            },
            true,
        ),
        (Command::Pause, true),
        (play, false),
    ] {
        let e = err(run(&core, command.clone(), session("claude", "s")));
        assert_eq!(
            (e.kind, e.code),
            (ErrorKind::Forbidden, ErrorCode::PermissionDenied),
            "{command:?}"
        );
        if others_may {
            ok(run(&core, command, Origin::agent("codex")));
        }
    }
    // Only what acts on something takes a scope.
    let e = refused(grant(
        "claude",
        None,
        class(OperationClass::LibraryEdit),
        location(&music),
        Expiry::Forever,
        Effect::Forbid,
    ));
    assert_eq!(
        (e.code, e.details.field.as_deref()),
        (ErrorCode::InvalidRequest, Some("command.scope"))
    );
    // A grant must already be in reviewed normal form; core must not silently
    // change the scope while creating it.
    let changed = refused(grant(
        "claude",
        None,
        class(OperationClass::LocalPath),
        Scope::Location {
            location: text(&music.join("jazz").join("..")),
        },
        Expiry::Forever,
        Effect::Allow,
    ));
    assert_eq!(changed.code, ErrorCode::LocationNotNormal);
    let id = ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::LocalPath),
            location(&music),
            Expiry::Forever,
            Effect::Allow,
        ),
    )
    .grant
    .unwrap();
    let stored = grants(&core, None)
        .into_iter()
        .find(|g| g.id == id)
        .unwrap();
    assert_eq!(stored.scope, location(&music));
    let mut bad_locations = vec!["music".to_owned(), format!("/{}", "x".repeat(4096))];
    // `..` past what exists (Win32 resolves `..` by name, so there it is a fine location).
    if cfg!(unix) {
        bad_locations.push(text(&music.join("new").join("..")));
    }
    for bad in bad_locations {
        let e = refused(grant(
            "claude",
            None,
            class(OperationClass::LocalPath),
            Scope::Location {
                location: bad.clone(),
            },
            Expiry::Forever,
            Effect::Allow,
        ));
        assert_eq!(
            (e.kind, e.code, e.details.field.as_deref()),
            (
                ErrorKind::Invalid,
                ErrorCode::InvalidLocation,
                Some("command.scope.location")
            ),
            "{bad:?}"
        );
    }
    // Agents: registered names, or `<platform>:<name>` for those an adapter names.
    ui(
        &core,
        grant(
            "ios:shortcuts",
            None,
            class(OperationClass::LocalPath),
            Scope::All,
            Expiry::Forever,
            Effect::Forbid,
        ),
    );
    for bad in ["iOS:shortcuts", "two words", "a:b:c", ":x"] {
        let e = refused(grant(
            bad,
            None,
            class(OperationClass::LocalPath),
            Scope::All,
            Expiry::Forever,
            Effect::Forbid,
        ));
        assert_eq!(
            (e.code, e.details.field.as_deref()),
            (ErrorCode::InvalidAgentName, Some("command.agent")),
            "{bad:?}"
        );
    }
    let e = refused(Command::RegisterAgent {
        name: "ios:shortcuts".into(),
    });
    assert_eq!(
        e.code,
        ErrorCode::InvalidAgentName,
        "registered names have no ':'"
    );
    // Sessions: at most 128 characters, and only until the session ends.
    let e = refused(grant(
        "claude",
        Some(&"s".repeat(129)),
        class(OperationClass::LocalPath),
        Scope::All,
        Expiry::Session,
        Effect::Allow,
    ));
    assert_eq!(
        (e.code, e.details.field.as_deref()),
        (ErrorCode::InvalidRequest, Some("command.session"))
    );
    for until in [
        Expiry::Forever,
        Expiry::At {
            at_ms: unix_ms() + 60_000,
        },
    ] {
        let e = refused(grant(
            "claude",
            Some("s1"),
            class(OperationClass::LocalPath),
            Scope::All,
            until,
            Effect::Forbid,
        ));
        assert_eq!(
            (e.code, e.details.field.as_deref()),
            (ErrorCode::InvalidRequest, Some("command.until"))
        );
    }
}

#[test]
fn output_preferences_confirm_by_default_and_agent_forbid_overrides_allow() {
    for update in [
        Command::SetOutputDevice {
            selection: OutputDeviceSelection::Device {
                id: "unseen-output".into(),
            },
            expected_revision: None,
        },
        Command::SetOutputMode {
            mode: None,
            fallback: Some(OutputFallbackPolicy::Shared),
            rate_policy: None,
            expected_revision: None,
        },
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (core, sub) = start(dir.path());
        ui(
            &core,
            Command::RegisterAgent {
                name: "output-agent".into(),
            },
        );
        let origin = session("output-agent", "output-session");
        for caller in [
            origin.clone(),
            Origin::Remote {
                device: "remote".into(),
            },
        ] {
            assert_eq!(update.policy(&caller), Policy::Confirm);
            let error = err(run(&core, update.clone(), caller));
            assert_eq!(error.kind, ErrorKind::NeedsConfirmation);
            let token = error.details.confirmation.unwrap().token;
            ui(&core, Command::Reject { token });
        }
        assert_eq!(update.policy(&Origin::Ui), Policy::Allow);
        assert_eq!(update.policy(&Origin::System), Policy::Allow);
        assert!(matches!(
            update.policy(&Origin::MediaControls),
            Policy::Forbidden(_)
        ));
        ui(
            &core,
            grant(
                "output-agent",
                None,
                command(update.name()),
                Scope::All,
                Expiry::Forever,
                Effect::Allow,
            ),
        );
        let forbid = ui(
            &core,
            grant(
                "output-agent",
                None,
                command(update.name()),
                Scope::All,
                Expiry::Forever,
                Effect::Forbid,
            ),
        )
        .grant
        .unwrap();
        assert_eq!(
            err(run(&core, update.clone(), origin.clone())).code,
            ErrorCode::PermissionDenied
        );
        ui(&core, Command::RevokePermission { grant: forbid });
        assert!(ok(run(&core, update, origin)).changed);
        stop(core, sub);
    }
}
