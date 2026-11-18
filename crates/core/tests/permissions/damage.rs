//! Damage scenarios.
use super::*;

/// Records from another build, or damaged ones: an unreadable token hash leaves the agent
/// listed (and removable) but unmatched; grants of shapes this build does not know never
/// apply; grants that expired while core was stopped are deleted when it starts.
#[test]
fn stored_records_core_cannot_read_never_let_an_agent_in() {
    let dir = tempfile::tempdir().unwrap();
    let music = folder(dir.path(), "music");
    let (core, sub) = start(dir.path());
    let token = ui(
        &core,
        Command::RegisterAgent {
            name: "claude".into(),
        },
    )
    .agent_token
    .unwrap();
    let short = ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::LocalPath),
            Scope::All,
            Expiry::At {
                at_ms: unix_ms() + 300,
            },
            Effect::Allow,
        ),
    )
    .grant
    .unwrap();
    stop(core, sub);
    std::thread::sleep(Duration::from_millis(400));
    let db = rusqlite::Connection::open(dir.path().join("library.db")).unwrap();
    db.execute(
        "UPDATE agents SET token_hash = x'0102' WHERE name = 'claude'",
        [],
    )
    .unwrap();
    for (id, operation, scope, effect) in [
        (
            9001,
            r#"{"type":"teleport"}"#,
            r#"{"type":"all"}"#,
            "forbid",
        ),
        (
            9002,
            r#"{"type":"class","class":"local_path"}"#,
            r#"{"type":"galaxy"}"#,
            "allow",
        ),
        (
            9003,
            r#"{"type":"class","class":"local_path"}"#,
            r#"{"type":"all"}"#,
            "maybe",
        ),
        (
            9004,
            r#"{"type":"class","class":"warp"}"#,
            r#"{"type":"all"}"#,
            "allow",
        ),
    ] {
        db.execute(
            "INSERT INTO grants (id, agent, session, operation, scope, expires_at, effect,
                                 created_at)
             VALUES (?1, 'claude', NULL, ?2, ?3, NULL, ?4, 0)",
            rusqlite::params![id, operation, scope, effect],
        )
        .unwrap();
    }
    drop(db);

    let (core, sub) = start(dir.path());
    // Expired meanwhile: gone, from the database too.
    assert!(grants(&core, None).iter().all(|g| g.id != short));
    let db = rusqlite::Connection::open(dir.path().join("library.db")).unwrap();
    let left: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM grants WHERE id = ?1",
            [short.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0);
    drop(db);
    // Listed as what they are: values this build does not know.
    let odd: Vec<Grant> = grants(&core, None);
    assert_eq!(odd.len(), 4);
    assert_eq!(odd[0].operation, Operation::Unknown);
    assert_eq!(odd[1].scope, Scope::Unknown);
    assert_eq!(odd[2].effect, Effect::Unknown);
    assert_eq!(
        odd[3].operation,
        Operation::Class {
            class: OperationClass::Unknown
        }
    );
    // None of them applies: neither forbids nor allows.
    assert!(is_needs_confirmation(&run(
        &core,
        add_root(&music),
        Origin::agent("claude")
    )));
    // The agent is listed, its old token names no one; it can be fixed or removed.
    assert_eq!(agents(&core).len(), 1);
    assert_eq!(core.authenticate_agent(&token), None);
    let renewed = ui(
        &core,
        Command::RenewAgentToken {
            agent: "claude".into(),
        },
    )
    .agent_token
    .unwrap();
    assert_eq!(core.authenticate_agent(&renewed).as_deref(), Some("claude"));
    ui(
        &core,
        Command::RemoveAgent {
            agent: "claude".into(),
        },
    );
    assert!(agents(&core).is_empty());
    assert!(grants(&core, None).is_empty());
    stop(core, sub);
}

/// Agents and grants that cannot be read at startup: core starts, says so in
/// `Snapshot.problems`, and refuses agents everything (failing closed: a lost `forbid` must
/// not let an agent through); the UI and playback carry on.
#[test]
fn permissions_that_cannot_be_read_refuse_agents() {
    let (core, sub) = start_failing(Failing {
        permissions: true,
        damaged: false,
    });
    assert_eq!(sub.snapshot.problems.len(), 1);
    assert_eq!(
        sub.snapshot.problems[0].code,
        ErrorCode::PermissionsUnavailable
    );
    assert!(sub.snapshot.problems[0].since_ms > 0);
    for command in [
        Command::Pause,
        Command::CreatePlaylist {
            name: "x".into(),
            kind: PlaylistKind::Manual,
        },
        Command::RegisterAgent { name: "me".into() },
    ] {
        let e = err(run(&core, command.clone(), session("claude", "s")));
        assert_eq!(
            (e.kind, e.code),
            (ErrorKind::Failed, ErrorCode::PermissionsUnavailable),
            "{command:?}"
        );
    }
    assert_eq!(core.authenticate_agent("pmpa_anything"), None);
    // The UI plays on, but cannot manage agents it cannot see.
    ok(run(&core, Command::SetVolume { volume: 0.5 }, Origin::Ui));
    ok(run(
        &core,
        Command::Pause,
        Origin::Remote {
            device: "phone".into(),
        },
    ));
    let e = err(run(
        &core,
        Command::RegisterAgent {
            name: "claude".into(),
        },
        Origin::Ui,
    ));
    assert_eq!(e.code, ErrorCode::PermissionsUnavailable);
    for query in [
        Query::Agents,
        Query::Permissions {
            agent: None,
            offset: 0,
            limit: 10,
        },
    ] {
        match core.query(query) {
            QueryResult::Err { error } => {
                assert_eq!(error.code, ErrorCode::PermissionsUnavailable)
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(problems(&core), [ErrorCode::PermissionsUnavailable]);
}

/// A damaged library database, seen by any command or query, becomes a problem in
/// `Snapshot.problems` (announced once, origin `null`) until core restarts; core leaves
/// the database alone.
#[test]
fn a_damaged_library_is_reported() {
    let (core, sub) = start_failing(Failing {
        permissions: false,
        damaged: false,
    });
    assert!(sub.snapshot.problems.is_empty());
    drain(&sub);
    let loved = Command::SetLoved {
        tracks: Vec::new(),
        loved: true,
    };
    let e = err(run(&core, loved.clone(), Origin::Ui));
    assert_eq!(
        (e.kind, e.code, e.details.retryable),
        (ErrorKind::Failed, ErrorCode::LibraryDamaged, false)
    );
    let events = drain(&sub);
    let announced: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::StateChanged { snapshot } => Some((snapshot.problems.clone(), &e.origin)),
            _ => None,
        })
        .collect();
    assert_eq!(announced.len(), 1, "{events:?}");
    assert_eq!(announced[0].1, &None);
    assert_eq!(announced[0].0[0].code, ErrorCode::LibraryDamaged);
    assert_eq!(problems(&core), [ErrorCode::LibraryDamaged]);
    // Once is enough.
    err(run(&core, loved, Origin::agent("claude")));
    assert!(drain(&sub).is_empty());

    // A query that finds it (on the caller's thread) reports it too.
    let (core, sub) = start_failing(Failing {
        permissions: false,
        damaged: true,
    });
    match core.query(Query::Roots) {
        QueryResult::Err { error } => assert_eq!(error.code, ErrorCode::LibraryDamaged),
        other => panic!("{other:?}"),
    }
    let state = wait_event(&sub, |e| matches!(e.event, Event::StateChanged { .. }));
    let Event::StateChanged { snapshot } = state.event else {
        unreachable!()
    };
    assert_eq!(snapshot.problems[0].code, ErrorCode::LibraryDamaged);
}

/// Damage found by a catalog call whose failure is only logged (here a play event written
/// on the library's writer thread, outside any command or query) still reaches
/// `Snapshot.problems`: core asks the catalog after every message and on every tick.
#[test]
fn damage_found_outside_commands_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("library.db");
    let library = mpmp_library::Library::open(&db).unwrap();
    let core = CoreHandle::start(
        Box::new(support::null_output()),
        Arc::new(FileResolver),
        Arc::new(library.clone()) as Arc<dyn Catalog>,
        CoreConfig {
            automatic_loudness_scan: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    let sub = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    assert!(sub.snapshot.problems.is_empty());
    // The schema is damaged behind the library's back …
    let raw = rusqlite::Connection::open(&db).unwrap();
    let version: i64 = raw
        .query_row("PRAGMA schema_version", [], |r| r.get(0))
        .unwrap();
    raw.execute_batch(&format!(
        "PRAGMA writable_schema = ON;
         INSERT INTO sqlite_master VALUES ('table', 'bogus', 'bogus', 0, 'CREATE TABLE bogus (');
         PRAGMA schema_version = {};",
        version + 1
    ))
    .unwrap();
    drop(raw);
    // … and found by a write nobody waits for.
    library
        .record_play(mpmp_library::PlayEvent {
            track_id: mpmp_library::TrackId(1),
            context: None,
            started_at: 1,
            played_ms: 1,
            ended_reason: mpmp_library::EndedReason::Completed,
            origin: "system".into(),
        })
        .unwrap();
    library.flush().unwrap();
    let state = wait_event(
        &sub,
        |e| matches!(&e.event, Event::StateChanged { snapshot } if !snapshot.problems.is_empty()),
    );
    assert_eq!(state.origin, None);
    // What the event carries (a query is not ordered with an event that no command caused, so the test reads the event); core publishes before it announces.
    let Event::StateChanged { snapshot } = state.event else {
        unreachable!()
    };
    assert_eq!(snapshot.problems[0].code, ErrorCode::LibraryDamaged);
    assert_eq!(problems(&core), [ErrorCode::LibraryDamaged]);
    drop(sub);
    core.shutdown();
}

/// A location that cannot be read now (a link to nothing, a music folder whose drive is not
/// plugged in) is judged by its name: a `forbid` it is
/// not in by its name does not refuse it, so the agent learns that it cannot be read, not
/// that every rule forbids it; one it is in by its name refuses it as before.
#[cfg(unix)]
#[test]
fn a_location_that_cannot_be_read_is_judged_by_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let music = folder(dir.path(), "music");
    let diary = folder(dir.path(), "diary");
    let (core, sub) = start(dir.path());
    let agent = Origin::agent("claude");
    let no_diary = ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::LocalPath),
            location(&diary),
            Expiry::Forever,
            Effect::Forbid,
        ),
    )
    .grant
    .unwrap();
    // Links to nothing: one in the music folder, one in the forbidden one.
    std::os::unix::fs::symlink(dir.path().join("gone"), music.join("away")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("gone"), diary.join("away")).unwrap();
    let e = err(run(&core, add_root(&music.join("away")), agent.clone()));
    assert_eq!(e.code, ErrorCode::InvalidLocation, "{e:?}");
    assert!(e.details.forbidden_by.is_empty(), "{e:?}");
    let e = err(run(&core, add_root(&diary.join("away")), agent.clone()));
    assert_eq!(e.code, ErrorCode::PermissionDenied, "{e:?}");
    assert_eq!(
        e.details.forbidden_by,
        [ForbiddenBy {
            grant: no_diary,
            operation: class(OperationClass::LocalPath),
            covers: Covers::All,
        }]
    );
    // A music folder that is not there now (here: its folder replaced by a link to
    // nothing, as a drive that is not plugged in): removing it is asked about as usual,
    // not refused by the forbid of `diary`.
    let classical = folder(&music, "classical");
    let root = ui(&core, add_root(&classical)).root.unwrap();
    std::fs::remove_dir(&classical).unwrap();
    std::os::unix::fs::symlink(dir.path().join("gone"), &classical).unwrap();
    drain(&sub);
    let held = err(run(&core, Command::RemoveRoot { root }, agent.clone()));
    assert_eq!(held.kind, ErrorKind::NeedsConfirmation, "{held:?}");
    // Approved, it is removed (from the library only: nothing on the drive is touched).
    let removed = ok(approve(
        &core,
        &sub,
        held.details.confirmation.unwrap().token,
    ));
    // Undoing that (bringing it back) is judged by its name too: asked about.
    let undo = Command::UndoAction {
        action: removed.action.unwrap(),
    };
    let held = err(run(&core, undo.clone(), agent.clone()));
    assert_eq!(held.kind, ErrorKind::NeedsConfirmation, "{held:?}");
    ui(
        &core,
        Command::Reject {
            token: held.details.confirmation.unwrap().token,
        },
    );
    // Plugged in again while the removal waits: it no longer is what the user was shown.
    let root = ui(&core, undo).root.unwrap_or(root);
    let held = err(run(&core, Command::RemoveRoot { root }, agent.clone()));
    assert_eq!(held.kind, ErrorKind::NeedsConfirmation, "{held:?}");
    std::fs::remove_file(&classical).unwrap();
    std::fs::create_dir(&classical).unwrap();
    let e = err(approve(
        &core,
        &sub,
        held.details.confirmation.unwrap().token,
    ));
    assert_eq!(e.code, ErrorCode::LocationChanged, "{e:?}");
    // One by its name in a forbidden place is refused.
    let kept = folder(&diary, "kept");
    let kept_root = ui(&core, add_root(&kept)).root.unwrap();
    std::fs::remove_dir(&kept).unwrap();
    std::os::unix::fs::symlink(dir.path().join("gone"), &kept).unwrap();
    let e = err(run(&core, Command::RemoveRoot { root: kept_root }, agent));
    assert_eq!(e.code, ErrorCode::PermissionDenied, "{e:?}");
    assert_eq!(e.details.forbidden_by[0].covers, Covers::All);
    stop(core, sub);
}
