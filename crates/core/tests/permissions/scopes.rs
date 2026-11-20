//! Scopes scenarios.
use super::*;

#[cfg(unix)]
#[test]
fn grant_scope_rejects_symlinks_appearing_or_retargeted_after_preview() {
    use std::os::unix::fs::symlink;
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let (core, sub) = start(dir.path());
    ui(
        &core,
        Command::RegisterAgent {
            name: "fixture".into(),
        },
    );
    let wider = folder(dir.path(), "wider");
    let other = folder(dir.path(), "other");
    for existed in [false, true] {
        let path = dir
            .path()
            .join(if existed { "existing" } else { "missing" });
        if existed {
            std::fs::create_dir(&path).unwrap();
        }
        // Exactly the same normalization as the host's preview.
        let reviewed = mpmp_fs_desktop::DesktopLocations
            .normalize(&text(&path))
            .unwrap();
        if existed {
            std::fs::remove_dir(&path).unwrap();
        }
        symlink(&wider, &path).unwrap();
        for target in [&wider, &other] {
            std::fs::remove_file(&path).unwrap();
            symlink(target, &path).unwrap();
            for (session_id, until) in [
                (None, Expiry::Forever),
                (Some("session-review"), Expiry::Session),
            ] {
                let error = err(run(
                    &core,
                    grant(
                        "fixture",
                        session_id,
                        command("add_root"),
                        Scope::Location {
                            location: reviewed.clone(),
                        },
                        until,
                        Effect::Allow,
                    ),
                    Origin::Ui,
                ));
                assert_eq!(error.kind, ErrorKind::Invalid);
                assert_eq!(error.code, ErrorCode::LocationNotNormal);
                assert_eq!(
                    error.details.field.as_deref(),
                    Some("command.scope.location")
                );
                assert!(grants(&core, Some("fixture")).is_empty());
            }
        }
    }
    // A symlink already present at preview is resolved by the host. Retargeting
    // that alias must not broaden the canonical value actually submitted.
    let reviewed_dir = folder(dir.path(), "reviewed");
    let alias = dir.path().join("alias");
    symlink(&reviewed_dir, &alias).unwrap();
    let reviewed = mpmp_fs_desktop::DesktopLocations
        .normalize(&text(&alias))
        .unwrap();
    std::fs::remove_file(&alias).unwrap();
    symlink(&wider, &alias).unwrap();
    ui(
        &core,
        grant(
            "fixture",
            None,
            command("add_root"),
            Scope::Location {
                location: reviewed.clone(),
            },
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    assert_eq!(
        grants(&core, Some("fixture"))[0].scope,
        Scope::Location { location: reviewed }
    );
    // Re-previewing produces an accepted canonical scope.
    let location = mpmp_fs_desktop::DesktopLocations
        .normalize(&text(&wider))
        .unwrap();
    ui(
        &core,
        grant(
            "fixture",
            None,
            command("add_root"),
            Scope::Location { location },
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    stop(core, sub);
}

/// A symlink inside an allowed folder leads outside it. Core
/// normalizes an agent's path once, as the system resolves it; the grants, the
/// confirmation dialog and the command itself all see where it really goes. A way back up
/// (`..`, which after a symlink the system takes from the link's target) is not taken from
/// an agent at all: refused by its text, before any grant is looked at.
#[cfg(unix)]
#[test]
fn symlinks_cannot_lead_an_agent_out_of_its_grants() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = folder(dir.path(), "music");
    let outside = folder(dir.path(), "outside");
    let sub_ = folder(&outside, "sub");
    std::os::unix::fs::symlink(&sub_, music.join("l")).unwrap();
    folder(&sub_, "secret");
    std::fs::write(sub_.join("list.m3u"), "#EXTM3U\n").unwrap();
    let (core, sub) = start(dir.path());
    let playlist = new_playlist(&core, "Mix");
    for class_ in [OperationClass::LocalPath, OperationClass::FileWrite] {
        ui(
            &core,
            grant(
                "claude",
                None,
                class(class_),
                location(&music),
                Expiry::Forever,
                Effect::Allow,
            ),
        );
    }
    let agent = session("claude", "s1");
    let commands = |path: &dyn Fn(&str) -> String| {
        [
            Command::AddRoot {
                path: path("secret"),
            },
            Command::ImportM3u {
                path: path("list.m3u"),
                name: None,
            },
            Command::ExportM3u {
                playlist,
                path: path("escaped.m3u"),
            },
        ]
    };
    let through_link = |name: &str| text(&music.join("l").join(name));
    for (command, name) in
        commands(&through_link)
            .into_iter()
            .zip(["secret", "list.m3u", "escaped.m3u"])
    {
        let real = sub_.join(name);
        drain(&sub);
        let held = run(&core, command.clone(), agent.clone());
        assert!(is_needs_confirmation(&held), "{command:?}: {held:?}");
        let confirmation = confirmation_of(&sub);
        // The user sees where it really goes, and that is where it would go.
        assert_eq!(confirmation.display_location, Some(text(&real)));
        assert_eq!(confirmation.scope, Some(location(&real)));
        assert_eq!(path_of(&confirmation.command), text(&real));
        assert!(
            confirmation.summary.contains(&text(&real)),
            "{}",
            confirmation.summary
        );
        ui(
            &core,
            Command::Reject {
                token: confirmation.token,
            },
        );
    }
    assert!(!sub_.join("escaped.m3u").exists());
    // Without the detour: allowed.
    ok(run(
        &core,
        Command::ExportM3u {
            playlist,
            path: text(&music.join("in.m3u")),
        },
        agent.clone(),
    ));
    // A forbid of the outside catches the detour, though it is written inside.
    ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::FileWrite),
            location(&outside),
            Expiry::Forever,
            Effect::Forbid,
        ),
    );
    let e = err(run(
        &core,
        Command::ExportM3u {
            playlist,
            path: through_link("escaped.m3u"),
        },
        agent.clone(),
    ));
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    assert!(!sub_.join("escaped.m3u").exists());

    // `..`: refused by its text, the same whatever it leads to and whatever is forbidden
    // (nobody is asked, nothing is named).
    let up = |name: &str| text(&music.join("l").join("..").join(name));
    let nowhere = |name: &str| text(&music.join("none").join("..").join(name));
    drain(&sub);
    for command in commands(&up).into_iter().chain(commands(&nowhere)) {
        let e = err(run(&core, command.clone(), agent.clone()));
        assert_eq!(
            (e.code, e.details.field.as_deref()),
            (ErrorCode::InvalidLocation, Some("command.path")),
            "{command:?}: {e:?}"
        );
        assert!(e.details.forbidden_by.is_empty(), "{e:?}");
        assert!(e.message.contains("no `..` part"), "{}", e.message);
    }
    assert!(drain(&sub).is_empty());
    stop(core, sub);
}

/// A location the platform layer cannot read (a link to nothing, a relative path, `..`
/// past what exists) is refused for an agent, never asked about; a forbid says so first.
/// The UI keeps the library's own errors.
#[cfg(unix)]
#[test]
fn agents_name_locations_core_can_read() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = folder(dir.path(), "music");
    std::os::unix::fs::symlink(dir.path().join("gone"), music.join("dangling")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("gone.m3u"), music.join("x.m3u")).unwrap();
    let (core, sub) = start(dir.path());
    let playlist = new_playlist(&core, "Mix");
    ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::LocalPath),
            location(&music),
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    drain(&sub);
    let agent = Origin::agent("claude");
    for command in [
        add_root(&music.join("dangling")),
        add_root(&music.join("dangling").join("sub")),
        Command::ExportM3u {
            playlist,
            path: text(&music.join("x.m3u")),
        },
        Command::ExportM3u {
            playlist,
            path: text(&music.join("new").join("..").join("y.m3u")),
        },
        Command::ImportM3u {
            path: "music/list.m3u".into(),
            name: None,
        },
        Command::AddRoot { path: "".into() },
    ] {
        let e = err(run(&core, command.clone(), agent.clone()));
        assert_eq!(
            (e.kind, e.code, e.details.field.as_deref()),
            (
                ErrorKind::Invalid,
                ErrorCode::InvalidLocation,
                Some("command.path")
            ),
            "{command:?}"
        );
    }
    assert!(
        !drain(&sub)
            .iter()
            .any(|e| matches!(e.event, Event::ConfirmationRequested { .. })),
        "nobody was asked"
    );
    // A forbid that may contain it refuses it, before anything else is said.
    ui(
        &core,
        grant(
            "claude",
            None,
            command("add_root"),
            location(&music),
            Expiry::Forever,
            Effect::Forbid,
        ),
    );
    let e = err(run(&core, add_root(&music.join("dangling")), agent));
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    // The UI is trusted with its paths: the library answers as before.
    let e = err(run(&core, add_root(&music.join("dangling")), Origin::Ui));
    assert_eq!(e.code, ErrorCode::NotADirectory);
}

/// A part of a library folder (`library` with a `prefix`) also covers the locations in that
/// place: the grant the docs show (`export_m3u` in folder 1's `Playlists`) works.
#[test]
fn library_prefixes_cover_locations_in_that_place() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = folder(dir.path(), "music");
    let lists = folder(&music, "Playlists");
    let private = folder(&music, "Private");
    let (core, _sub) = start(dir.path());
    let root = ui(&core, add_root(&music)).root.unwrap();
    let playlist = new_playlist(&core, "Mix");
    let export = |path: PathBuf| Command::ExportM3u {
        playlist,
        path: text(&path),
    };
    let in_lists = ui(
        &core,
        grant(
            "claude",
            None,
            command("export_m3u"),
            Scope::Library {
                root,
                prefix: Some("/Playlists/".into()),
            },
            Expiry::At {
                at_ms: unix_ms() + 3_600_000,
            },
            Effect::Allow,
        ),
    )
    .grant
    .unwrap();
    let agent = Origin::agent("claude");
    ok(run(&core, export(lists.join("a.m3u")), agent.clone()));
    assert_eq!(actions(&core)[0].grant, Some(in_lists));
    assert!(is_needs_confirmation(&run(
        &core,
        export(music.join("b.m3u")),
        agent.clone()
    )));
    // A part forbidden inside a folder allowed whole: the forbid wins there.
    ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::FileWrite),
            Scope::Library { root, prefix: None },
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    ui(
        &core,
        grant(
            "claude",
            None,
            command("export_m3u"),
            Scope::Library {
                root,
                prefix: Some("Private".into()),
            },
            Expiry::Forever,
            Effect::Forbid,
        ),
    );
    ok(run(&core, export(music.join("b.m3u")), agent.clone()));
    let e = err(run(&core, export(private.join("c.m3u")), agent));
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    assert!(!private.join("c.m3u").exists());
    // Prefixes are `/`-separated names of folders the platform can have (on Windows `C:` is no folder name, and a grant for it would never apply).
    let mut bad_prefixes = vec![
        "Jazz//Miles".to_owned(),
        "./Jazz".into(),
        "Jazz/..".into(),
        r"Jazz\Miles".into(),
        "x".repeat(1025),
    ];
    if cfg!(windows) {
        bad_prefixes.push("C:".into());
    }
    for bad in bad_prefixes {
        let e = err(run(
            &core,
            grant(
                "claude",
                None,
                class(OperationClass::LocalPath),
                Scope::Library {
                    root,
                    prefix: Some(bad.clone()),
                },
                Expiry::Forever,
                Effect::Allow,
            ),
            Origin::Ui,
        ));
        assert_eq!(
            (e.code, e.details.field.as_deref()),
            (ErrorCode::InvalidRequest, Some("command.scope.prefix")),
            "{bad:?}"
        );
    }
}

/// Undoing a folder change is checked against every folder it changed, and an undo that
/// was undone already is refused before anyone is asked.
#[test]
fn undoing_folder_changes_is_checked_for_every_folder() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let (a, b) = (folder(dir.path(), "a"), folder(dir.path(), "b"));
    let (core, sub) = start(dir.path());
    let root_a = ui(&core, add_root(&a)).root.unwrap();
    let root_b = ui(&core, add_root(&b)).root.unwrap();
    let removed = ui(&core, Command::RemoveRoot { root: root_a })
        .action
        .unwrap();
    ui(&core, Command::RemoveRoot { root: root_b });
    // An action that changed both folders (one of a later version, say); both are
    // removed, so undoing it brings both back.
    let db = rusqlite::Connection::open(dir.path().join("library.db")).unwrap();
    db.execute(
        "INSERT INTO action_targets
             (kind, object_id, version_after, action_id, state_before, state_after)
         SELECT 'root', ?1, version_after + 1000, action_id, state_before, state_after
         FROM action_targets WHERE action_id = ?2",
        [root_b.0, removed.0],
    )
    .unwrap();
    drop(db);
    let undo = Command::UndoAction { action: removed };
    let agent = Origin::agent("claude");
    // Allowed for one of them only: asked, and the dialog names no single scope.
    let one = ui(
        &core,
        grant(
            "claude",
            None,
            command("undo_action"),
            Scope::Library {
                root: root_a,
                prefix: None,
            },
            Expiry::Forever,
            Effect::Allow,
        ),
    )
    .grant
    .unwrap();
    drain(&sub);
    assert!(is_needs_confirmation(&run(
        &core,
        undo.clone(),
        agent.clone()
    )));
    let confirmation = confirmation_of(&sub);
    assert_eq!(
        (confirmation.scope, confirmation.display_location),
        (None, None)
    );
    ui(
        &core,
        Command::Reject {
            token: confirmation.token,
        },
    );
    // Forbidden for either: refused.
    ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::LocalPath),
            Scope::Library {
                root: root_b,
                prefix: None,
            },
            Expiry::Forever,
            Effect::Forbid,
        ),
    );
    let e = err(run(&core, undo, agent.clone()));
    assert_eq!(e.code, ErrorCode::PermissionDenied);

    // An undo by command name covers a folder undo; one that was undone already is
    // `already_undone` before anyone is asked.
    let added = ui(&core, add_root(&a)).action.unwrap();
    let undo = ui(&core, Command::UndoAction { action: added })
        .action
        .unwrap();
    drain(&sub);
    let e = err(run(
        &core,
        Command::UndoAction { action: added },
        agent.clone(),
    ));
    assert_eq!(
        (e.code, e.details.field.as_deref()),
        (ErrorCode::AlreadyUndone, Some("command.action"))
    );
    assert!(
        !drain(&sub)
            .iter()
            .any(|e| matches!(e.event, Event::ConfirmationRequested { .. }))
    );
    let redone = ok(run(
        &core,
        Command::UndoAction { action: undo },
        agent.clone(),
    ));
    assert_eq!(action(&core, redone.action.unwrap()).grant, Some(one));

    // Now one of the two folders is in use and the other removed: undoing the action
    // that changed both is no one command, so it is refused rather than judged as either.
    let e = err(run(&core, Command::UndoAction { action: removed }, agent));
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::Failed, ErrorCode::CorruptData)
    );
}

/// A `forbid` of a place inside a folder applies to adding or
/// removing the whole folder, and to undoing that, though an `allow` covers the rest. The
/// refusal says that only a part of the folder is forbidden, never which (asking for `/` must not list the agent's forbidden places).
#[test]
fn a_forbid_inside_a_folder_applies_to_the_whole_folder() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = folder(dir.path(), "music");
    let private = folder(&music, "private");
    let other = folder(dir.path(), "other");
    let (core, sub) = start(dir.path());
    let agent = Origin::agent("claude");
    ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::LocalPath),
            Scope::All,
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    let no_private = ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::LocalPath),
            location(&private),
            Expiry::Forever,
            Effect::Forbid,
        ),
    )
    .grant
    .unwrap();
    let hidden = [text(&private)];
    let hidden: Vec<&str> = hidden
        .iter()
        .map(String::as_str)
        .chain(["private"])
        .collect();
    let e = err(run(&core, add_root(&music), agent.clone()));
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    assert_eq!(
        e.details.forbidden_by,
        [ForbiddenBy {
            grant: no_private,
            operation: class(OperationClass::LocalPath),
            covers: Covers::Part,
        }]
    );
    assert!(
        e.message
            .ends_with("may still be asked for, a part at a time (a subfolder, say)"),
        "{}",
        e.message
    );
    says_nowhere(&e, &hidden);
    // Above it too, up to the top: still only "a part".
    let top = music.ancestors().last().unwrap();
    let e = err(run(&core, add_root(top), agent.clone()));
    assert_eq!(e.details.forbidden_by[0].covers, Covers::Part, "{e:?}");
    says_nowhere(&e, &hidden);
    // The forbidden place itself, or a place inside it: all of it.
    for path in [private.clone(), folder(&private, "2024")] {
        let e = err(run(&core, add_root(&path), agent.clone()));
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert_eq!(e.details.forbidden_by[0].covers, Covers::All, "{e:?}");
        assert!(!e.message.contains("a part at a time"), "{}", e.message);
    }
    assert!(ok(run(&core, add_root(&other), agent.clone())).changed);

    // A part of a library folder: removing the folder, or undoing its addition.
    let added = ui(&core, add_root(&music));
    let root = added.root.unwrap();
    let no_hidden = ui(
        &core,
        grant(
            "claude",
            None,
            command("remove_root"),
            Scope::Library {
                root,
                prefix: Some("Hidden".into()),
            },
            Expiry::Forever,
            Effect::Forbid,
        ),
    )
    .grant
    .unwrap();
    let e = err(run(&core, Command::RemoveRoot { root }, agent.clone()));
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    // The forbid of `private` (in this folder too) and that of `Hidden`.
    assert_eq!(
        e.details.forbidden_by,
        [
            ForbiddenBy {
                grant: no_private,
                operation: class(OperationClass::LocalPath),
                covers: Covers::Part,
            },
            ForbiddenBy {
                grant: no_hidden,
                operation: command("remove_root"),
                covers: Covers::Part,
            },
        ]
    );
    says_nowhere(&e, &["Hidden", "library folder"]);
    says_nowhere(&e, &hidden);
    let e = err(run(
        &core,
        Command::UndoAction {
            action: added.action.unwrap(),
        },
        agent,
    ));
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    says_nowhere(&e, &["Hidden", "library folder"]);
    says_nowhere(&e, &hidden);
    stop(core, sub);
}

/// Undoing a folder change is adding or removing that folder,
/// so the grants for `add_root` / `remove_root` apply to it as to the command itself.
#[test]
fn grants_for_a_folder_command_apply_to_undoing_its_opposite() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = folder(dir.path(), "music");
    let jazz = folder(dir.path(), "jazz");
    let (core, sub) = start(dir.path());
    let agent = Origin::agent("claude");
    let root = ui(&core, add_root(&music)).root.unwrap();
    let removed = ui(&core, Command::RemoveRoot { root }).action.unwrap();
    // Forbidden to add it: also to bring it back by undoing its removal.
    ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::LocalPath),
            Scope::All,
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    let no_add = ui(
        &core,
        grant(
            "claude",
            None,
            command("add_root"),
            location(&music),
            Expiry::Forever,
            Effect::Forbid,
        ),
    )
    .grant
    .unwrap();
    let e = err(run(
        &core,
        Command::UndoAction { action: removed },
        agent.clone(),
    ));
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    assert!(e.message.contains(&format!("grant {}", no_add.0)), "{e:?}");
    ui(&core, Command::RevokePermission { grant: no_add });

    // Allowed to add a folder: also to bring one back (and only that).
    let other = Origin::agent("codex");
    let jazz_root = ui(&core, add_root(&jazz)).root.unwrap();
    let jazz_removed = ui(&core, Command::RemoveRoot { root: jazz_root })
        .action
        .unwrap();
    let may_add = ui(
        &core,
        grant(
            "codex",
            None,
            command("add_root"),
            location(&jazz),
            Expiry::Forever,
            Effect::Allow,
        ),
    )
    .grant
    .unwrap();
    let back = ok(run(
        &core,
        Command::UndoAction {
            action: jazz_removed,
        },
        other.clone(),
    ));
    assert_eq!(action(&core, back.action.unwrap()).grant, Some(may_add));
    // Undoing that brings removal: `add_root` does not cover it.
    drain(&sub);
    assert!(is_needs_confirmation(&run(
        &core,
        Command::UndoAction {
            action: back.action.unwrap(),
        },
        other,
    )));
    stop(core, sub);
}

/// An agent's import never reads a file's text back to it, only
/// the unmatched lines that look like media; the count has them all. The UI sees all.
#[test]
fn an_agents_import_reads_back_only_what_looks_like_media() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = folder(dir.path(), "music");
    std::fs::write(
        music.join("notes.m3u"),
        "TOP-SECRET-LINE\nmissing/song.flac\npassword=hunter2\nhttp://radio.example/x.mp3\n",
    )
    .unwrap();
    let (core, sub) = start(dir.path());
    ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::LocalPath),
            location(&music),
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    let import = Command::ImportM3u {
        path: text(&music.join("notes.m3u")),
        name: None,
    };
    let report = |receipt: Receipt| receipt.playlist.unwrap().import.unwrap();
    let agents = report(ok(run(&core, import.clone(), Origin::agent("claude"))));
    assert_eq!(
        agents.missing,
        ["missing/song.flac", "http://radio.example/x.mp3"]
    );
    assert_eq!((agents.matched_count, agents.missing_count), (0, 4));
    let users = report(ui(&core, import));
    assert_eq!(users.missing.len(), 4);
    stop(core, sub);
}

/// No FIFO or device an agent puts in the file system holds
/// core up (a slow or hung file system, such as an offline network share or the agent's
/// own FUSE mount, holds up an agent's command for its time limit at most:
/// `a_slow_location_holds_up_neither_core_nor_the_user`). A FIFO named like a playlist: importing it is `not_a_regular_file` right away, for an
/// agent (allowed, or asking) and for the user. A FIFO or a symlink under the case-flipped
/// name of a granted folder: the letter-case probe neither opens nor follows it, so core
/// goes on (the user's commands too), and the `allow` does not reach a folder whose name
/// differs only in case.
#[cfg(unix)]
#[test]
fn what_an_agent_puts_in_the_way_neither_holds_core_up_nor_widens_a_grant() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = folder(dir.path(), "music");
    let playlists = folder(dir.path(), "Playlists");
    assert!(mkfifo(&music.join("x.m3u")));
    let (core, sub) = start(dir.path());
    let playlist = new_playlist(&core, "Mix");
    let not_regular = |result: CommandResult| {
        let e = err(result);
        assert_eq!(
            (e.kind, e.code, e.details.field.as_deref()),
            (
                ErrorKind::Invalid,
                ErrorCode::NotARegularFile,
                Some("command.path")
            )
        );
    };
    let import = Command::ImportM3u {
        path: text(&music.join("x.m3u")),
        name: None,
    };
    let (c, i) = (core.clone(), import.clone());
    not_regular(promptly("an agent's import", move || {
        run(&c, i, Origin::agent("claude"))
    }));
    ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::LocalPath),
            location(&music),
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    let (c, i) = (core.clone(), import.clone());
    not_regular(promptly("an allowed agent's import", move || {
        run(&c, i, Origin::agent("claude"))
    }));
    let c = core.clone();
    let e = promptly("the user's import", move || {
        err(run(&c, import, Origin::Ui))
    });
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::Invalid, ErrorCode::NotARegularFile)
    );

    // The case-flipped name of a granted folder, beside a folder that differs only in case.
    let flipped = playlists.with_file_name("pLAYLISTS");
    if !mkfifo(&flipped) {
        // Case is ignored here: no other entry can have that name.
        stop(core, sub);
        return;
    }
    let lower = folder(dir.path(), "playlists");
    ui(
        &core,
        grant(
            "claude",
            None,
            command("export_m3u"),
            location(&playlists),
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    let export = Command::ExportM3u {
        playlist,
        path: text(&lower.join("x.m3u")),
    };
    for what in ["a FIFO", "a link"] {
        let (c, x) = (core.clone(), export.clone());
        let result = promptly("an export beside a case-flipped name", move || {
            run(&c, x, Origin::agent("claude"))
        });
        assert!(is_needs_confirmation(&result), "{what}: {result:?}");
        let c = core.clone();
        promptly("the user's pause", move || {
            run(&c, Command::Pause, Origin::Ui)
        });
        // The same with a link back to the granted folder.
        std::fs::remove_file(&flipped).unwrap();
        std::os::unix::fs::symlink(&playlists, &flipped).unwrap();
    }
    assert!(!lower.join("x.m3u").exists());
    stop(core, sub);
}

/// An agent's export to a folder that does not exist is
/// refused before the user is asked (`not_a_directory`), allowed or not; the user's own
/// gets the plain error it always got.
#[test]
fn an_export_to_a_missing_folder_is_refused_before_anyone_is_asked() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = folder(dir.path(), "music");
    let (core, sub) = start(dir.path());
    let playlist = new_playlist(&core, "Mix");
    let export = Command::ExportM3u {
        playlist,
        path: text(&music.join("nope").join("x.m3u")),
    };
    let refused = |result: CommandResult| {
        let e = err(result);
        assert_eq!(
            (e.kind, e.code, e.details.field.as_deref()),
            (
                ErrorKind::Invalid,
                ErrorCode::NotADirectory,
                Some("command.path")
            )
        );
    };
    refused(run(&core, export.clone(), Origin::agent("claude")));
    ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::LocalPath),
            location(&music),
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    refused(run(&core, export.clone(), Origin::agent("claude")));
    let e = err(run(&core, export, Origin::Ui));
    assert_eq!((e.kind, e.code), (ErrorKind::Failed, ErrorCode::Io));
    assert!(
        drain(&sub)
            .iter()
            .all(|e| !matches!(e.event, Event::ConfirmationRequested { .. }))
    );
    stop(core, sub);
}

/// What undoing an action amounts to is judged from the
/// folders it records itself (an undo or redo copies them) and their state now, so an
/// undo whose original action was pruned is still a folder change: the agent is asked,
/// the `forbid`s of the command it amounts to (and of `local_path`) refuse it, a remote
/// may not. Both ways: bringing a folder back (`add_root`) and removing it (`remove_root`).
#[test]
fn undoing_a_folder_change_is_judged_without_the_actions_before_it() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = folder(dir.path(), "music");
    let jazz = folder(dir.path(), "jazz");
    let (core, sub) = start(dir.path());
    let agent = Origin::agent("claude");
    let remote = Origin::Remote {
        device: "phone".into(),
    };
    // Added, then the addition undone (the folder is removed); the addition is pruned.
    let added = ui(&core, add_root(&music)).action.unwrap();
    let removes = ui(&core, Command::UndoAction { action: added })
        .action
        .unwrap();
    // Removed, then the removal undone (the folder is back); the removal is pruned.
    let jazz_root = ui(&core, add_root(&jazz)).root.unwrap();
    let removed = ui(&core, Command::RemoveRoot { root: jazz_root })
        .action
        .unwrap();
    let restores = ui(&core, Command::UndoAction { action: removed })
        .action
        .unwrap();
    forget_action(dir.path(), added);
    forget_action(dir.path(), removed);
    let ids: Vec<_> = actions(&core).iter().map(|a| a.id).collect();
    assert!(!ids.contains(&added) && !ids.contains(&removed), "{ids:?}");

    for (undo, place, same) in [
        (removes, &music, "add_root"),
        (restores, &jazz, "remove_root"),
    ] {
        let undo = Command::UndoAction { action: undo };
        drain(&sub);
        assert!(is_needs_confirmation(&run(
            &core,
            undo.clone(),
            agent.clone()
        )));
        let confirmation = confirmation_of(&sub);
        ui(
            &core,
            Command::Reject {
                token: confirmation.token,
            },
        );
        let e = err(run(&core, undo.clone(), remote.clone()));
        assert_eq!(e.code, ErrorCode::PathNotAllowed);
        for operation in [command(same), class(OperationClass::LocalPath)] {
            let no = ui(
                &core,
                grant(
                    "claude",
                    None,
                    operation,
                    location(place),
                    Expiry::Forever,
                    Effect::Forbid,
                ),
            )
            .grant
            .unwrap();
            let e = err(run(&core, undo.clone(), agent.clone()));
            assert_eq!(e.code, ErrorCode::PermissionDenied, "{same}");
            assert!(e.message.contains(&format!("grant {}", no.0)), "{e:?}");
            ui(&core, Command::RevokePermission { grant: no });
        }
        // The command it amounts to, allowed: run, under that grant.
        let may = ui(
            &core,
            grant(
                "claude",
                None,
                command(same),
                location(place),
                Expiry::Forever,
                Effect::Allow,
            ),
        )
        .grant
        .unwrap();
        let done = ok(run(&core, undo, agent.clone()));
        assert_eq!(action(&core, done.action.unwrap()).grant, Some(may));
    }

    // A folder the library does not know: what the undo amounts to cannot be told, so it
    // is refused, never run as a plain data edit.
    let db = rusqlite::Connection::open(dir.path().join("library.db")).unwrap();
    db.execute(
        "UPDATE action_targets SET object_id = 424242 WHERE action_id = ?1 AND kind = 'root'",
        [restores.0],
    )
    .unwrap();
    drop(db);
    let e = err(run(&core, Command::UndoAction { action: restores }, agent));
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::Failed, ErrorCode::CorruptData)
    );
    stop(core, sub);
}

/// A whole music folder is judged where its stored path leads
/// now, in normal form: replaced by a link to elsewhere, a `forbid` of that place applies
/// to removing it.
#[cfg(unix)]
#[test]
fn a_music_folder_is_judged_where_it_leads_now() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = folder(dir.path(), "music");
    let elsewhere = folder(dir.path(), "elsewhere");
    let (core, sub) = start(dir.path());
    let root = ui(&core, add_root(&music)).root.unwrap();
    std::fs::remove_dir(&music).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &music).unwrap();
    ui(
        &core,
        grant(
            "claude",
            None,
            command("remove_root"),
            location(&elsewhere),
            Expiry::Forever,
            Effect::Forbid,
        ),
    );
    let e = err(run(
        &core,
        Command::RemoveRoot { root },
        Origin::agent("claude"),
    ));
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    stop(core, sub);
}

/// an agent's export to a folder named like a playlist file
/// (`folder.m3u`) failed with `not_a_regular_file` but no field; every error about the
/// path names `command.path`, for an agent and for the user.
#[test]
fn an_export_onto_a_folder_names_the_path() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = folder(dir.path(), "music");
    let target = folder(&music, "folder.m3u");
    let (core, sub) = start(dir.path());
    let playlist = new_playlist(&core, "Mix");
    let export = Command::ExportM3u {
        playlist,
        path: text(&target),
    };
    let not_regular = |result: CommandResult| {
        let e = err(result);
        assert_eq!(
            (e.kind, e.code, e.details.field.as_deref()),
            (
                ErrorKind::Invalid,
                ErrorCode::NotARegularFile,
                Some("command.path")
            ),
            "{e:?}"
        );
    };
    not_regular(run(&core, export.clone(), Origin::Ui));
    drain(&sub);
    let asked = run(&core, export.clone(), Origin::agent("claude"));
    assert!(is_needs_confirmation(&asked), "{asked:?}");
    let token = confirmation_of(&sub).token;
    not_regular(approve(&core, &sub, token));
    ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::FileWrite),
            location(&music),
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    not_regular(run(&core, export, Origin::agent("claude")));
    assert!(target.is_dir());
    stop(core, sub);
}

/// a UNC path back to this computer (`\\localhost\C$\…`,
/// `\\127.0.0.1\c$\…`, `\\?\UNC\…`) is the local folder, so a `forbid` of that folder
/// refuses it (it was only asked about, showing the UNC path).
#[cfg(windows)]
#[test]
fn a_forbidden_folder_is_forbidden_by_any_path_back_to_it() {
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let out = folder(dir.path(), "out");
    let (core, sub) = start(dir.path());
    let playlist = new_playlist(&core, "Mix");
    ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::FileWrite),
            location(&out),
            Expiry::Forever,
            Effect::Forbid,
        ),
    );
    let local = text(&out.join("f.m3u"));
    let (drive, tail) = local
        .split_once(":\\")
        .expect("a temporary folder on a drive");
    let lower = drive.to_lowercase();
    for path in [
        format!(r"\\localhost\{drive}$\{tail}"),
        format!(r"\\127.0.0.1\{lower}$\{tail}"),
        format!(r"\\?\UNC\localhost\{drive}$\{tail}"),
    ] {
        let e = err(run(
            &core,
            Command::ExportM3u {
                playlist,
                path: path.clone(),
            },
            Origin::agent("claude"),
        ));
        assert_eq!(e.code, ErrorCode::PermissionDenied, "{path}: {e:?}");
    }
    assert!(
        drain(&sub)
            .iter()
            .all(|e| !matches!(e.event, Event::ConfirmationRequested { .. }))
    );
    stop(core, sub);
}

/// An unreachable folder has no narrower allow scope; the command can still be confirmed.
#[test]
fn offline_folder_confirmations_do_not_offer_an_ineffective_scope() {
    struct Offline;
    impl mpmp_core::Locations for Offline {
        fn normalize(&self, location: &str) -> Option<String> {
            if location.contains("offline") && !Path::new(location).exists() {
                None
            } else {
                mpmp_fs_desktop::DesktopLocations.normalize(location)
            }
        }
        fn contains(&self, scope: &str, location: &str, doubt: mpmp_core::Doubt) -> bool {
            mpmp_fs_desktop::DesktopLocations.contains(
                scope,
                location,
                doubt == mpmp_core::Doubt::Include,
            )
        }
        fn child(&self, location: &str, relative: &str) -> Option<String> {
            mpmp_fs_desktop::DesktopLocations.child(location, relative)
        }
        fn ignores_case(&self, location: &str) -> Option<bool> {
            mpmp_fs_desktop::DesktopLocations.ignores_case(location)
        }
        fn display(&self, location: &str) -> String {
            mpmp_fs_desktop::DesktopLocations.display(location)
        }
        fn nearest(&self, location: &str) -> Option<String> {
            mpmp_fs_desktop::DesktopLocations.nearest(location)
        }
    }
    let dir = mpmp_test_support::TempDir::new().unwrap();
    let music = folder(dir.path(), "offline");
    let (core, sub) = start_with_config(
        dir.path(),
        CoreConfig {
            locations: Some(Arc::new(Offline)),
            ..Default::default()
        },
    );
    let added = ui(&core, add_root(&music));
    let root = added.root.unwrap();
    std::fs::remove_dir(&music).unwrap();
    let agent = Origin::agent("claude");
    for command in [
        Command::RemoveRoot { root },
        Command::UndoAction {
            action: added.action.unwrap(),
        },
    ] {
        drain(&sub);
        let error = err(run(&core, command, agent.clone()));
        assert_eq!(error.kind, ErrorKind::NeedsConfirmation, "{error:?}");
        let asked = confirmation_of(&sub);
        assert_eq!(asked.scope, None);
        assert!(asked.summary.contains("not reachable now"), "{asked:?}");
        assert!(!asked.summary.contains("skipped"), "{asked:?}");
        ui(&core, Command::Reject { token: asked.token });
    }
    let removed = ui(&core, Command::RemoveRoot { root });
    drain(&sub);
    let error = err(run(
        &core,
        Command::UndoAction {
            action: removed.action.unwrap(),
        },
        agent,
    ));
    assert_eq!(error.kind, ErrorKind::NeedsConfirmation, "{error:?}");
    let asked = confirmation_of(&sub);
    assert_eq!(asked.scope, None);
    ui(&core, Command::Reject { token: asked.token });
    stop(core, sub);
}
