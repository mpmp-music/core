//! Waiting scenarios.
use super::*;

/// Removing an agent, or the end of one of its sessions, refuses what it still waits for
/// (as if the user had rejected it); a session's end also revokes its grants.
#[test]
fn removing_an_agent_or_ending_a_session_rejects_what_it_waits_for() {
    let dir = tempfile::tempdir().unwrap();
    let (one, two, three) = (
        folder(dir.path(), "one"),
        folder(dir.path(), "two"),
        folder(dir.path(), "three"),
    );
    let (core, sub) = start(dir.path());
    ui(
        &core,
        Command::RegisterAgent {
            name: "claude".into(),
        },
    );
    let session_grant = ui(
        &core,
        grant(
            "claude",
            Some("s1"),
            class(OperationClass::FileWrite),
            Scope::All,
            Expiry::Session,
            Effect::Allow,
        ),
    )
    .grant
    .unwrap();
    let ask = |path: &Path, origin: Origin, key: &str| {
        let result = core
            .execute(
                Request {
                    request_id: 1,
                    idempotency_key: Some(key.into()),
                    command: add_root(path),
                },
                origin,
            )
            .result;
        err(result).details.confirmation.unwrap().token
    };
    let s1 = ask(&one, session("claude", "s1"), "k1");
    let s2 = ask(&two, session("claude", "s2"), "k2");
    let codex = ask(&three, session("codex", "s1"), "k3");
    drain(&sub);

    core.end_agent_session("claude", "s1");
    let events = drain(&sub);
    assert_eq!(
        events
            .iter()
            .map(|e| (&e.event, &e.origin))
            .collect::<Vec<_>>(),
        [
            (&Event::PermissionsChanged, &None),
            (
                &Event::ConfirmationResolved {
                    outcome: ConfirmationOutcome {
                        token: s1.clone(),
                        approved: false,
                        result: None,
                        reason: Some(ConfirmationReason::Withdrawn),
                    }
                },
                &None
            ),
        ]
    );
    assert!(grants(&core, None).iter().all(|g| g.id != session_grant));
    // A grant for the session that ended could never apply ("allow for this session" in a
    // dialog answered after the agent left): refused. Another session is not.
    let for_session = |agent: &str, session: &str| {
        grant(
            agent,
            Some(session),
            class(OperationClass::LocalPath),
            Scope::All,
            Expiry::Session,
            Effect::Allow,
        )
    };
    let e = err(run(&core, for_session("claude", "s1"), Origin::Ui));
    assert_eq!(
        (e.kind, e.code, e.details.field.as_deref()),
        (
            ErrorKind::Invalid,
            ErrorCode::SessionEnded,
            Some("command.session")
        )
    );
    assert!(
        grants(&core, None)
            .iter()
            .all(|g| g.session.as_deref() != Some("s1"))
    );
    let s3_grant = ui(&core, for_session("claude", "s3")).grant.unwrap();
    ui(&core, Command::RevokePermission { grant: s3_grant });
    // Only that agent's session: another agent's session of the same id is not over.
    let codex_grant = ui(&core, for_session("codex", "s1")).grant.unwrap();
    ui(&core, Command::RevokePermission { grant: codex_grant });
    let e = err(run(&core, Command::Confirm { token: s1 }, Origin::Ui));
    assert_eq!(e.code, ErrorCode::ConfirmationNotFound);
    // A retry from the session that ended is withdrawn again (not from the idempotency
    // cache: its session is over; from another session it runs, below).
    let retry = core.execute(
        Request {
            request_id: 2,
            idempotency_key: Some("k1".into()),
            command: add_root(&one),
        },
        session("claude", "s1"),
    );
    assert_eq!(err(retry.result).code, ErrorCode::ConfirmationRejected);

    ui(
        &core,
        Command::RemoveAgent {
            agent: "claude".into(),
        },
    );
    let resolved: Vec<_> = drain(&sub)
        .into_iter()
        .filter_map(|e| match e.event {
            Event::ConfirmationResolved { outcome } => {
                Some((outcome.token, outcome.reason, e.origin))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        resolved,
        [(s2, Some(ConfirmationReason::Withdrawn), Some(Origin::Ui))]
    );
    // Removal keeps ended sessions: a late caller cannot regain that session's grants.
    let ended = err(run(&core, for_session("claude", "s1"), Origin::Ui));
    assert_eq!(ended.code, ErrorCode::SessionEnded);
    drain(&sub);
    // A withdrawal is not kept for a retry: the command never ran, so a retry
    // runs it, and it is asked about again.
    let retry = core.execute(
        Request {
            request_id: 3,
            idempotency_key: Some("k2".into()),
            command: add_root(&two),
        },
        session("claude", "s2"),
    );
    let asked = err(retry.result);
    assert_eq!(asked.kind, ErrorKind::NeedsConfirmation);
    ui(
        &core,
        Command::Reject {
            token: asked.details.confirmation.unwrap().token,
        },
    );
    drain(&sub);
    // Ending a session that has nothing announces nothing.
    core.end_agent_session("codex", "s9");
    assert!(drain(&sub).is_empty());
    // Another agent's request still waits for the user.
    let confirmed = ui(&core, Command::Confirm { token: codex })
        .confirmed
        .unwrap();
    assert!(confirmed.approved);
    assert_eq!(confirmed.reason, Some(ConfirmationReason::Approved));

    // An agent an adapter names itself (`<platform>:<name>`) ends its sessions alike.
    let phone = session("ios:shortcuts", "u-1");
    let its_grant = ui(
        &core,
        grant(
            "ios:shortcuts",
            Some("u-1"),
            class(OperationClass::FileWrite),
            Scope::All,
            Expiry::Session,
            Effect::Allow,
        ),
    )
    .grant
    .unwrap();
    let waiting = ask(&one, phone.clone(), "k4");
    drain(&sub);
    core.end_agent_session("ios:shortcuts", "u-1");
    let events = drain(&sub);
    assert!(events.iter().any(|e| e.event == Event::PermissionsChanged));
    assert!(events.iter().any(|e| matches!(
        &e.event,
        Event::ConfirmationResolved { outcome } if outcome.token == waiting && !outcome.approved
    )));
    assert!(grants(&core, None).iter().all(|g| g.id != its_grant));
    let e = err(run(&core, for_session("ios:shortcuts", "u-1"), Origin::Ui));
    assert_eq!(e.code, ErrorCode::SessionEnded);
    let retry = core.execute(
        Request {
            request_id: 4,
            idempotency_key: Some("k4".into()),
            command: add_root(&one),
        },
        phone,
    );
    assert_eq!(err(retry.result).code, ErrorCode::ConfirmationRejected);
}

/// A location checked and shown to the user can be
/// changed while the command waits for the user's answer (a folder replaced by a symlink to
/// elsewhere, a playlist file by a link to any other file). Approved, the command is
/// checked again first: it is not run (`location_changed`), nothing is written, added or
/// read, and a retry learns the same.
#[cfg(unix)]
#[test]
fn locations_changed_while_waiting_are_not_acted_on() {
    let dir = tempfile::tempdir().unwrap();
    let music = folder(dir.path(), "music");
    let outside = folder(dir.path(), "outside");
    folder(&outside, "secret");
    std::fs::write(
        outside.join("secret.txt"),
        "TOP-SECRET-LINE\nsecond-secret\n",
    )
    .unwrap();
    std::fs::write(music.join("x.m3u"), "#EXTM3U\n").unwrap();
    folder(&music, "sub");
    let (core, sub) = start(dir.path());
    let playlist = new_playlist(&core, "Mix");
    let agent = session("claude", "s1");
    let ask = |command: Command, key: &str| {
        let result = core
            .execute(
                Request {
                    request_id: 1,
                    idempotency_key: Some(key.into()),
                    command,
                },
                agent.clone(),
            )
            .result;
        assert!(is_needs_confirmation(&result), "{result:?}");
        err(result).details.confirmation.unwrap().token
    };
    let changed = |result: CommandResult| {
        let e = err(result);
        assert_eq!(
            (e.kind, e.code, e.details.field.as_deref()),
            (
                ErrorKind::Invalid,
                ErrorCode::LocationChanged,
                Some("command.path")
            )
        );
        e
    };

    // P1: an export to a folder that becomes a link.
    folder(&music, "new");
    let export = Command::ExportM3u {
        playlist,
        path: text(&music.join("new").join("x.m3u")),
    };
    let token = ask(export.clone(), "p1");
    std::fs::remove_dir(music.join("new")).unwrap();
    std::os::unix::fs::symlink(&outside, music.join("new")).unwrap();
    changed(approve(&core, &sub, token));
    assert!(!outside.join("x.m3u").exists());
    assert!(
        !actions(&core).iter().any(|a| a.summary.contains("x.m3u")),
        "nothing written, nothing recorded"
    );

    // P5: an import of a playlist file that becomes a link to any other file.
    let import = Command::ImportM3u {
        path: text(&music.join("x.m3u")),
        name: None,
    };
    let token = ask(import.clone(), "p5");
    std::fs::remove_file(music.join("x.m3u")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), music.join("x.m3u")).unwrap();
    let e = changed(approve(&core, &sub, token));
    assert!(!format!("{e:?}").contains("SECRET"));
    let retry = core.execute(
        Request {
            request_id: 2,
            idempotency_key: Some("p5".into()),
            command: import,
        },
        agent.clone(),
    );
    let e = changed(retry.result);
    assert!(!format!("{e:?}").contains("SECRET"));

    // A music folder replaced by a link to elsewhere.
    let token = ask(add_root(&music.join("sub")), "root");
    std::fs::remove_dir(music.join("sub")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret"), music.join("sub")).unwrap();
    changed(approve(&core, &sub, token));
    match core.query(Query::Roots) {
        QueryResult::Roots { roots } => assert!(roots.is_empty(), "{roots:?}"),
        other => panic!("{other:?}"),
    }
    stop(core, sub);
}

/// A `forbid` the user gives while a request waits applies to it
/// when the user then approves it (`permission_denied`); an `allow` given meanwhile changes
/// nothing (the user answers the request).
#[test]
fn a_forbid_given_while_waiting_refuses_the_request() {
    let dir = tempfile::tempdir().unwrap();
    let music = folder(dir.path(), "music");
    let (core, sub) = start(dir.path());
    let playlist = new_playlist(&core, "Mix");
    let agent = Origin::agent("claude");
    let export = Command::ExportM3u {
        playlist,
        path: text(&music.join("a.m3u")),
    };
    let token = err(run(&core, export.clone(), agent.clone()))
        .details
        .confirmation
        .unwrap()
        .token;
    let forbid = ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::FileWrite),
            Scope::All,
            Expiry::Forever,
            Effect::Forbid,
        ),
    )
    .grant
    .unwrap();
    let e = err(approve(&core, &sub, token));
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    // The check on approval names the grant too.
    assert_eq!(
        e.details.forbidden_by,
        [ForbiddenBy {
            grant: forbid,
            operation: class(OperationClass::FileWrite),
            covers: Covers::All,
        }]
    );
    assert!(
        e.message.contains(&format!(
            "grant {} (file_write commands: all of what it acts on)",
            forbid.0
        )),
        "{}",
        e.message
    );
    assert!(!music.join("a.m3u").exists());

    // An allow given while it waits: the user's answer still decides.
    let other = Origin::agent("codex");
    let token = err(run(&core, export.clone(), other.clone()))
        .details
        .confirmation
        .unwrap()
        .token;
    ui(
        &core,
        grant(
            "codex",
            None,
            class(OperationClass::FileWrite),
            Scope::All,
            Expiry::Forever,
            Effect::Allow,
        ),
    );
    ok(approve(&core, &sub, token));
    assert!(music.join("a.m3u").exists());
    stop(core, sub);
}

/// The check when the user approves says only "a part" too: a forbid of a place inside the
/// folder an agent asked to add, given while it waited, refuses it without naming the place.
#[test]
fn a_forbid_inside_a_waiting_folder_refuses_it_without_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let music = folder(dir.path(), "music");
    let diary = folder(&music, "diary");
    let (core, sub) = start(dir.path());
    let token = err(run(&core, add_root(&music), Origin::agent("claude")))
        .details
        .confirmation
        .unwrap()
        .token;
    let forbid = ui(
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
    let e = err(approve(&core, &sub, token));
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    assert_eq!(
        e.details.forbidden_by,
        [ForbiddenBy {
            grant: forbid,
            operation: class(OperationClass::LocalPath),
            covers: Covers::Part,
        }]
    );
    says_nowhere(&e, &[&text(&diary), "diary"]);
    match core.query(Query::Roots) {
        QueryResult::Roots { roots } => assert!(roots.is_empty(), "{roots:?}"),
        other => panic!("{other:?}"),
    }
    stop(core, sub);
}

/// The checks of an agent's command run off the actor (docs/design/agent-permissions.md): while they wait on a location, the user's commands are answered at once;
/// what changes meanwhile counts (a forbid given, a session ended); the same idempotency
/// key gets the same answer; and the user's approval of a held command waits for its
/// check again, without holding up anyone else, and can no longer be answered meanwhile.
#[test]
fn checks_run_off_the_actor_and_see_what_changed_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let slow_dir = folder(dir.path(), "pmp-slow");
    let slow = Slow::new();
    // Long enough that nothing here runs out of time.
    let (core, sub) = start_slow(dir.path(), &slow, Duration::from_secs(30));
    let playlist = new_playlist(&core, "Mix");
    let export = Command::ExportM3u {
        playlist,
        path: text(&slow_dir.join("x.m3u")),
    };
    let in_thread = |request: Request, origin: Origin| {
        let core = core.clone();
        std::thread::spawn(move || core.execute(request, origin).result)
    };
    let answered_at_once = |what: &str| {
        let core = core.clone();
        let asked = Instant::now();
        let result = promptly(what, move || run(&core, Command::Pause, Origin::Ui));
        assert!(matches!(result, CommandResult::Ok(_)), "{result:?}");
        assert!(asked.elapsed() < Duration::from_secs(1), "{what}");
    };
    let asked_nobody = |sub: &Subscription| {
        assert!(
            drain(sub)
                .iter()
                .all(|e| !matches!(e.event, Event::ConfirmationRequested { .. }))
        );
    };

    // A forbid given while the check waits: the check runs again, and refuses it.
    slow.shut(true);
    drain(&sub);
    let waiting = in_thread(Request::new(1, export.clone()), Origin::agent("claude"));
    eventually("the first check waiting", || slow.waited() == 1);
    answered_at_once("the user's command while the agent's is checked");
    assert!(!waiting.is_finished());
    // (Of the folder above: the UI's own commands are not time-limited, and would wait at
    // the gate.)
    let forbid = ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::FileWrite),
            location(slow_dir.parent().unwrap()),
            Expiry::Forever,
            Effect::Forbid,
        ),
    )
    .grant
    .unwrap();
    slow.shut(false);
    let e = err(waiting.join().unwrap());
    assert_eq!(e.code, ErrorCode::PermissionDenied, "{e:?}");
    assert_eq!(e.details.forbidden_by[0].grant, forbid);
    asked_nobody(&sub);
    ui(&core, Command::RevokePermission { grant: forbid });

    // A session that ends while its command is checked: withdrawn, nobody asked.
    slow.shut(true);
    let waiting = in_thread(Request::new(1, export.clone()), session("claude", "s1"));
    eventually("the second check waiting", || slow.waited() == 2);
    core.end_agent_session("claude", "s1");
    slow.shut(false);
    let e = err(waiting.join().unwrap());
    assert_eq!(e.code, ErrorCode::ConfirmationRejected, "{e:?}");
    assert!(e.message.contains("withdrawn: its session ended"), "{e:?}");
    asked_nobody(&sub);
    // A registered agent removed while its command is checked: the same (its removal
    // changes what checks depend on, so they run again: it still counts that the agent
    // was registered when the command came).
    ui(&core, Command::RegisterAgent { name: "reg".into() });
    slow.shut(true);
    let waiting = in_thread(Request::new(1, export.clone()), Origin::agent("reg"));
    eventually("the removed agent's check waiting", || slow.waited() == 3);
    ui(
        &core,
        Command::RemoveAgent {
            agent: "reg".into(),
        },
    );
    slow.shut(false);
    let e = err(waiting.join().unwrap());
    assert_eq!(e.code, ErrorCode::ConfirmationRejected, "{e:?}");
    asked_nobody(&sub);

    // The same idempotency key while the first request is checked: the same answer; with
    // another command, refused.
    let keyed = |command: Command| Request {
        request_id: 7,
        idempotency_key: Some("k".into()),
        command,
    };
    slow.shut(true);
    let first = in_thread(keyed(export.clone()), Origin::agent("claude"));
    eventually("the third check waiting", || slow.waited() == 4);
    let second = in_thread(keyed(export.clone()), Origin::agent("claude"));
    // (Time for the second request to reach core while the first is still checked.)
    std::thread::sleep(Duration::from_millis(200));
    let other = core
        .execute(keyed(Command::Pause), Origin::agent("claude"))
        .result;
    assert_eq!(err(other).code, ErrorCode::IdempotencyKeyReused);
    // Joined: not answered yet, and no check of its own.
    assert!(!second.is_finished());
    assert_eq!(slow.waited(), 4);
    slow.shut(false);
    let (first, second) = (first.join().unwrap(), second.join().unwrap());
    assert_eq!(first, second);
    let token = err(first).details.confirmation.unwrap().token;
    ui(&core, Command::Reject { token });

    // Approving a held command whose check again waits: the user's other commands are
    // answered meanwhile, the token can no longer be answered, and the approval is
    // answered once the check is done.
    let token = err(run(&core, export.clone(), Origin::agent("claude")))
        .details
        .confirmation
        .unwrap()
        .token;
    slow.shut(true);
    let confirm = in_thread(
        Request::new(
            1,
            Command::Confirm {
                token: token.clone(),
            },
        ),
        Origin::Ui,
    );
    eventually("the check again waiting", || slow.waited() == 5);
    answered_at_once("the user's command while an approval is checked");
    let e = err(run(
        &core,
        Command::Reject {
            token: token.clone(),
        },
        Origin::Ui,
    ));
    assert_eq!(e.code, ErrorCode::ConfirmationNotFound, "{e:?}");
    assert!(!confirm.is_finished());
    slow.shut(false);
    let outcome = ok(confirm.join().unwrap()).confirmed.unwrap();
    assert!(outcome.approved);
    assert!(
        matches!(outcome.result.as_deref(), Some(CommandResult::Ok(_))),
        "{outcome:?}"
    );
    assert!(slow_dir.join("x.m3u").exists());

    // A forbid given while the approval is checked again: checked again with it, refused.
    let token = err(run(&core, export.clone(), Origin::agent("claude")))
        .details
        .confirmation
        .unwrap()
        .token;
    slow.shut(true);
    let confirm = in_thread(Request::new(1, Command::Confirm { token }), Origin::Ui);
    eventually("the second check again waiting", || slow.waited() == 6);
    let forbid = ui(
        &core,
        grant(
            "claude",
            None,
            class(OperationClass::FileWrite),
            location(slow_dir.parent().unwrap()),
            Expiry::Forever,
            Effect::Forbid,
        ),
    )
    .grant
    .unwrap();
    slow.shut(false);
    let outcome = ok(confirm.join().unwrap()).confirmed.unwrap();
    assert!(
        matches!(outcome.result.as_deref(), Some(CommandResult::Err { error })
            if error.code == ErrorCode::PermissionDenied),
        "{outcome:?}"
    );
    ui(&core, Command::RevokePermission { grant: forbid });

    // Removed and registered again while its command is checked: not the agent that sent
    // it.
    ui(
        &core,
        Command::RegisterAgent {
            name: "again".into(),
        },
    );
    slow.shut(true);
    let waiting = in_thread(Request::new(1, export.clone()), Origin::agent("again"));
    eventually("the re-registered agent's check waiting", || {
        slow.waited() == 7
    });
    ui(
        &core,
        Command::RemoveAgent {
            agent: "again".into(),
        },
    );
    ui(
        &core,
        Command::RegisterAgent {
            name: "again".into(),
        },
    );
    slow.shut(false);
    assert_eq!(
        err(waiting.join().unwrap()).code,
        ErrorCode::ConfirmationRejected
    );

    // Withdrawn while checked: not kept for a retry with its key, which, from the new
    // session after reconnecting, runs it.
    let keyed = |command: Command| Request {
        request_id: 9,
        idempotency_key: Some("again".into()),
        command,
    };
    slow.shut(true);
    let waiting = in_thread(keyed(export.clone()), session("claude", "s5"));
    eventually("the keyed check waiting", || slow.waited() == 8);
    core.end_agent_session("claude", "s5");
    slow.shut(false);
    assert_eq!(
        err(waiting.join().unwrap()).code,
        ErrorCode::ConfirmationRejected
    );
    let retry = err(core
        .execute(keyed(export.clone()), session("claude", "s6"))
        .result);
    assert_eq!(retry.kind, ErrorKind::NeedsConfirmation, "{retry:?}");
    stop(core, sub);
}

/// An agent removed while the user's approval of its command is checked again: the user forbids it, then removes the agent, which removes the
/// forbid too. The command is withdrawn (as one waiting for the user is), not checked again
/// without the agent's grants and run; also when the agent is registered again meanwhile.
#[test]
fn removing_an_agent_withdraws_its_approved_command_being_checked_again() {
    let dir = tempfile::tempdir().unwrap();
    let slow_dir = folder(dir.path(), "pmp-slow");
    let slow = Slow::new();
    let (core, sub) = start_slow(dir.path(), &slow, Duration::from_secs(30));
    let playlist = new_playlist(&core, "Mix");
    let file = slow_dir.join("x.m3u");
    let export = Command::ExportM3u {
        playlist,
        path: text(&file),
    };
    for registered_again in [false, true] {
        ui(&core, Command::RegisterAgent { name: "reg".into() });
        let keyed = || Request {
            request_id: 1,
            idempotency_key: Some("remove-recheck".into()),
            command: export.clone(),
        };
        let token = err(core.execute(keyed(), session("reg", "old")).result)
            .details
            .confirmation
            .unwrap()
            .token;
        let old_token = token.clone();
        let waited = slow.waited();
        slow.shut(true);
        let confirm = {
            let core = core.clone();
            let request = Request::new(1, Command::Confirm { token });
            std::thread::spawn(move || core.execute(request, Origin::Ui).result)
        };
        eventually("the check again waiting", || slow.waited() == waited + 1);
        ui(
            &core,
            grant(
                "reg",
                None,
                class(OperationClass::FileWrite),
                location(slow_dir.parent().unwrap()),
                Expiry::Forever,
                Effect::Forbid,
            ),
        );
        ui(
            &core,
            Command::RemoveAgent {
                agent: "reg".into(),
            },
        );
        assert!(grants(&core, Some("reg")).is_empty());
        if registered_again {
            ui(&core, Command::RegisterAgent { name: "reg".into() });
        }
        let outcome = ok(confirm.join().unwrap()).confirmed.unwrap();
        assert!(!outcome.approved, "{outcome:?}");
        assert_eq!(outcome.reason, Some(ConfirmationReason::Withdrawn));
        assert!(outcome.result.is_none(), "{outcome:?}");
        let resolved = wait_event(&sub, |e| {
            matches!(e.event, Event::ConfirmationResolved { .. })
        });
        let Event::ConfirmationResolved { outcome: told } = resolved.event else {
            panic!()
        };
        assert_eq!(told, outcome);
        let retry = registered_again.then(|| {
            let core = core.clone();
            let request = keyed();
            let retry =
                std::thread::spawn(move || core.execute(request, session("reg", "new")).result);
            eventually("the new registration starts its own check", || {
                slow.waited() == waited + 2
            });
            retry
        });
        slow.shut(false);
        if let Some(retry) = retry {
            let fresh = err(retry.join().unwrap());
            assert_eq!(fresh.kind, ErrorKind::NeedsConfirmation, "{fresh:?}");
            assert_ne!(fresh.details.confirmation.unwrap().token, old_token);
        }
        assert!(!file.exists(), "registered again: {registered_again}");
        if registered_again {
            ui(
                &core,
                Command::RemoveAgent {
                    agent: "reg".into(),
                },
            );
        }
    }
    stop(core, sub);
}

/// Agent commands checked at once, on a location that answers: each is checked as if alone
/// (only calls stuck past their time limit count against the few that may be left
/// running), and so is the user's approval meanwhile.
#[test]
fn concurrent_checks_of_a_healthy_location_all_answer() {
    let dir = tempfile::tempdir().unwrap();
    let out = folder(dir.path(), "pmp-slow");
    let slow = Slow::new();
    let (core, sub) = start_slow(dir.path(), &slow, Duration::from_secs(5));
    let playlist = new_playlist(&core, "Mix");
    let export = |name: &str| Command::ExportM3u {
        playlist,
        path: text(&out.join(name)),
    };
    let held = err(run(&core, export("held.m3u"), Origin::agent("claude")))
        .details
        .confirmation
        .unwrap()
        .token;
    let agents: Vec<_> = (0..12)
        .map(|i| {
            let (core, command) = (core.clone(), export(&format!("{i}.m3u")));
            std::thread::spawn(move || run(&core, command, Origin::agent("claude")))
        })
        .collect();
    let approved = ok(run(&core, Command::Confirm { token: held }, Origin::Ui))
        .confirmed
        .unwrap();
    assert!(
        matches!(approved.result.as_deref(), Some(CommandResult::Ok(_))),
        "{approved:?}"
    );
    for agent in agents {
        let e = err(agent.join().unwrap());
        assert_eq!(e.kind, ErrorKind::NeedsConfirmation, "{e:?}");
    }
    stop(core, sub);
}

/// an agent naming an offline network location
/// (`\\10.255.255.1\share\x.m3u` takes Windows about 21 s to give up on) held core's
/// command thread, and the UI with it, for as long, again and again. Now an agent
/// command's file system calls run on helper threads with a time limit: past it, the
/// command fails closed (`invalid_location`, "did not answer in time"; a `forbid` that may
/// hold the location refuses it), the user's commands are answered meanwhile within the
/// limit and right away after it, and at most a few stuck calls are left running (beyond
/// that an agent's command fails at once, without starting another). Also when the user
/// approves a held command whose location stopped answering while it waited.
#[test]
fn a_slow_location_holds_up_neither_core_nor_the_user() {
    const LIMIT: Duration = Duration::from_millis(500);
    let dir = tempfile::tempdir().unwrap();
    let slow_dir = folder(dir.path(), "pmp-slow");
    let elsewhere = folder(dir.path(), "elsewhere");
    let slow = Slow::new();
    let library = mpmp_library::Library::open(&dir.path().join("library.db")).unwrap();
    let output = support::null_output();
    let core = CoreHandle::start(
        Box::new(output),
        Arc::new(FileResolver),
        Arc::new(library) as Arc<dyn Catalog>,
        CoreConfig {
            locations: Some(slow.clone()),
            agent_file_system_timeout: Some(LIMIT),
            ..Default::default()
        },
    )
    .unwrap();
    let sub = core
        .subscribe(Origin::Ui, SubscribeOptions::default())
        .unwrap();
    let playlist = new_playlist(&core, "Mix");
    let export = move |at: &Path| Command::ExportM3u {
        playlist,
        path: text(&at.join("x.m3u")),
    };
    let late = |e: &ContractError| {
        assert_eq!(
            (e.kind, e.code, e.details.field.as_deref()),
            (
                ErrorKind::Invalid,
                ErrorCode::InvalidLocation,
                Some("command.path")
            ),
            "{e:?}"
        );
        assert!(e.message.contains("did not answer in time"), "{e:?}");
    };
    let agent = |core: &CoreHandle, command: Command| {
        let core = core.clone();
        promptly("the agent's command", move || {
            let started = Instant::now();
            (
                run(&core, command, Origin::agent("claude")),
                started.elapsed(),
            )
        })
    };

    // The agent's command waits for the limit, the user's meanwhile no longer than that.
    slow.shut(true);
    let (c, x) = (core.clone(), export(&slow_dir));
    let started = Instant::now();
    let waiting = std::thread::spawn(move || run(&c, x, Origin::agent("claude")));
    eventually("the agent's location being asked", || slow.waited() == 1);
    let c = core.clone();
    let user = promptly("the user's command meanwhile", move || {
        let asked = Instant::now();
        (run(&c, Command::Pause, Origin::Ui), asked.elapsed())
    });
    assert!(matches!(user.0, CommandResult::Ok(_)), "{user:?}");
    // Answered while the agent's command is still being checked: the checks run off the
    // actor (docs/design/agent-permissions.md).
    assert!(user.1 < LIMIT / 2, "{user:?}");
    assert!(
        !waiting.is_finished(),
        "the agent's command was answered first"
    );
    late(&err(promptly("the agent's command", move || {
        waiting.join().unwrap()
    })));
    let took = started.elapsed();
    assert!(
        took >= LIMIT && took < LIMIT + Duration::from_secs(5),
        "{took:?}"
    );
    // The call is still stuck; the user's commands are answered right away.
    let c = core.clone();
    let user = promptly("the user's command after", move || {
        let asked = Instant::now();
        (run(&c, Command::Pause, Origin::Ui), asked.elapsed())
    });
    assert!(user.1 < LIMIT, "{user:?}");

    // A location that does not answer is reported so, before any grant: not even where it
    // is by its name is known, so a `forbid` cannot be told to hold it or not.
    ui(
        &core,
        grant(
            "other",
            None,
            class(OperationClass::FileWrite),
            location(&elsewhere),
            Expiry::Forever,
            Effect::Forbid,
        ),
    );
    let (c, x) = (core.clone(), export(&slow_dir));
    let result = promptly("the forbidden agent's command", move || {
        run(&c, x, Origin::agent("other"))
    });
    let e = err(result);
    assert_eq!(e.code, ErrorCode::InvalidLocation, "{e:?}");
    assert!(
        e.message.contains("did not answer in time"),
        "{}",
        e.message
    );
    assert!(e.details.forbidden_by.is_empty(), "{e:?}");
    assert_eq!(slow.waited(), 2);

    // Stuck calls are limited (4): two more, then an agent's command fails at once, even
    // for a location that would answer, and starts no call.
    for _ in 0..2 {
        late(&err(agent(&core, export(&slow_dir)).0));
    }
    assert_eq!(slow.waited(), 4);
    let (result, took) = agent(&core, export(&elsewhere));
    late(&err(result));
    assert!(took < LIMIT, "{took:?}");
    assert_eq!(slow.waited(), 4, "no call started");
    assert!(
        drain(&sub)
            .iter()
            .all(|e| !matches!(e.event, Event::ConfirmationRequested { .. })),
        "the user was asked nothing"
    );

    // Once the location answers again, the stuck calls end, and the agent is asked about
    // as before.
    slow.shut(false);
    eventually("the stuck calls ending", || {
        is_needs_confirmation(&agent(&core, export(&elsewhere)).0)
    });
    drain(&sub);
    let result = agent(&core, export(&slow_dir)).0;
    assert!(is_needs_confirmation(&result), "{result:?}");
    let token = confirmation_of(&sub).token;

    // The location stops answering while the command waits for the user: approved, it is
    // checked again within the limit, and not run.
    slow.shut(true);
    let asked = Instant::now();
    late(&err(approve(&core, &sub, token)));
    assert!(asked.elapsed() < LIMIT + Duration::from_secs(5));
    assert!(!slow_dir.join("x.m3u").exists());
    slow.shut(false);
    stop(core, sub);
}

/// Disconnecting during the approval's recheck withdraws the command and releases its key.
#[test]
fn ending_a_session_withdraws_an_approved_command_being_checked_again() {
    let dir = tempfile::tempdir().unwrap();
    let out = folder(dir.path(), "pmp-slow");
    let slow = Slow::new();
    let (core, sub) = start_slow(dir.path(), &slow, Duration::from_secs(30));
    let file = out.join("x.m3u");
    let export = Command::ExportM3u {
        playlist: new_playlist(&core, "Mix"),
        path: text(&file),
    };
    let keyed = || Request {
        request_id: 1,
        idempotency_key: Some("disconnect-recheck".into()),
        command: export.clone(),
    };
    let token = err(core.execute(keyed(), session("claude", "old")).result)
        .details
        .confirmation
        .unwrap()
        .token;
    let waited = slow.waited();
    slow.shut(true);
    let approved = {
        let core = core.clone();
        std::thread::spawn(move || {
            core.execute(Request::new(2, Command::Confirm { token }), Origin::Ui)
                .result
        })
    };
    eventually("the approval's recheck", || slow.waited() == waited + 1);
    core.end_agent_session("claude", "old");
    let outcome = ok(approved.join().unwrap()).confirmed.unwrap();
    assert_eq!(outcome.reason, Some(ConfirmationReason::Withdrawn));
    assert!(!outcome.approved && outcome.result.is_none(), "{outcome:?}");
    assert!(!file.exists());
    // The first recheck is still blocked. A new session must not get its dead token.
    let retry = {
        let core = core.clone();
        let request = keyed();
        std::thread::spawn(move || core.execute(request, session("claude", "new")).result)
    };
    eventually("the retry starts its own check", || {
        slow.waited() == waited + 2
    });
    slow.shut(false);
    let retry = err(retry.join().unwrap());
    assert_eq!(retry.kind, ErrorKind::NeedsConfirmation, "{retry:?}");
    let outcome = ui(
        &core,
        Command::Confirm {
            token: retry.details.confirmation.unwrap().token,
        },
    )
    .confirmed
    .unwrap();
    assert!(outcome.approved, "{outcome:?}");
    assert!(
        matches!(outcome.result.as_deref(), Some(CommandResult::Ok(_))),
        "{outcome:?}"
    );
    assert!(file.exists());
    stop(core, sub);
}
