use super::*;

fn grant(id: i64, agent: &str, operation: Operation, scope: Scope, effect: Effect) -> Grant {
    Grant {
        id: GrantId(id),
        agent: agent.into(),
        session: None,
        operation,
        scope,
        until: Expiry::Forever,
        effect,
        created_at_ms: 0,
    }
}

fn class(class: OperationClass) -> Operation {
    Operation::Class { class }
}

fn command(name: &str) -> Operation {
    Operation::Command {
        command: name.into(),
    }
}

fn folder(path: &str) -> Scope {
    Scope::Location {
        location: path.into(),
    }
}

fn loc(path: &str) -> Target {
    Target::Location {
        location: path.into(),
        folder: false,
    }
}

/// A folder added as a music folder.
fn dir(path: &str) -> Target {
    Target::Location {
        location: path.into(),
        folder: true,
    }
}

/// Symlinks: in root 0 (`/music`) to elsewhere; in root 1 (`/music/1`) to a folder in
/// it, and out of it.
const LINKS: &[(&str, &str)] = &[
    ("/music/link", "/elsewhere"),
    ("/music/1/Public/link", "/music/1/Private"),
    ("/music/1/Current", "/music/1/2026"),
    ("/music/1/out", "/elsewhere/out"),
];

/// Paths as plain `/`-separated prefixes; letter case ignored under `/ci`, unknown
/// under `/unsure`; the [`LINKS`] resolve.
struct Plain;

impl Locations for Plain {
    fn normalize(&self, location: &str) -> Option<String> {
        if !location.starts_with('/') {
            return None;
        }
        for (link, to) in LINKS {
            if let Some(rest) = location.strip_prefix(link)
                && (rest.is_empty() || rest.starts_with('/'))
            {
                return Some(format!("{to}{rest}"));
            }
        }
        Some(location.to_owned())
    }
    fn contains(&self, scope: &str, location: &str, doubt: Doubt) -> bool {
        let inside = |s: &str, l: &str| l == s || l.starts_with(&format!("{s}/"));
        inside(scope, location)
            || (inside(&scope.to_lowercase(), &location.to_lowercase())
                && self.ignores_case(scope).unwrap_or(doubt == Doubt::Include))
    }
    fn child(&self, location: &str, relative: &str) -> Option<String> {
        Some(format!("{location}/{relative}"))
    }
    fn ignores_case(&self, location: &str) -> Option<bool> {
        if location.starts_with("/unsure") {
            None
        } else {
            Some(location.starts_with("/ci"))
        }
    }
    fn display(&self, location: &str) -> String {
        location.to_owned()
    }
}

/// Roots at `/music/<id>`; root 0 at `/music`, 7 at `/ci/7`, 8 at `/unsure/8`.
fn root_location(root: RootId) -> Option<String> {
    Some(match root.0 {
        0 => "/music".into(),
        7 => "/ci/7".into(),
        8 => "/unsure/8".into(),
        id => format!("/music/{id}"),
    })
}

fn contains(scope: &Scope, target: &Target, doubt: Doubt) -> bool {
    reach(scope, target, doubt) != Reach::Outside
}

fn reach(scope: &Scope, target: &Target, doubt: Doubt) -> Reach {
    scope_reach(scope, target, doubt, &Plain, root_location)
}

fn decide_all(
    p: &Permissions,
    agent: &str,
    session: Option<&str>,
    request: &Request<'_>,
    now: i64,
) -> Decision {
    let candidates = p.candidates(agent, session, &[request.command], request.class, now);
    decide(&candidates, request, reach)
}

fn with(grants: Vec<Grant>) -> Permissions {
    Permissions::new(
        StoredPermissions {
            agents: Vec::new(),
            grants,
        },
        0,
    )
    .0
}

fn add_root(targets: &[Target]) -> Request<'_> {
    Request {
        command: "add_root",
        class: OperationClass::LocalPath,
        targets,
    }
}

#[test]
fn registrations_are_distinct_even_with_the_same_wall_clock_timestamp() {
    let agent = StoredAgent {
        name: "same".into(),
        token_hash: None,
        created_at_ms: 7,
        last_seen_at_ms: None,
    };
    let (mut permissions, _) = Permissions::new(
        StoredPermissions {
            agents: vec![agent.clone()],
            grants: Vec::new(),
        },
        7,
    );
    let first = permissions.registration("same").unwrap();
    permissions.remove_agent("same");
    permissions.add_agent(agent.clone());
    let second = permissions.registration("same").unwrap();
    assert!(second > first);
    assert_eq!(permissions.agent("same").unwrap().created_at_ms, 7);
    permissions.remove_agent("same");
    permissions.add_agent(StoredAgent {
        created_at_ms: 1,
        ..agent
    });
    assert!(permissions.registration("same").unwrap() > second);
}

#[test]
fn grants_match_on_every_dimension() {
    let jazz = [loc("/home/me/jazz")];
    let p = with(vec![grant(
        1,
        "claude",
        class(OperationClass::LocalPath),
        folder("/home/me"),
        Effect::Allow,
    )]);
    let d = |p: &Permissions, agent, session, request: &Request<'_>, now| {
        decide_all(p, agent, session, request, now)
    };
    assert_eq!(
        d(&p, "claude", None, &add_root(&jazz), 5),
        Decision::Allow(GrantId(1))
    );
    // Another agent, class, place; nothing in particular.
    assert_eq!(d(&p, "codex", None, &add_root(&jazz), 5), Decision::Ask);
    let other = Request {
        class: OperationClass::FileWrite,
        command: "export_m3u",
        ..add_root(&jazz)
    };
    assert_eq!(d(&p, "claude", None, &other, 5), Decision::Ask);
    let elsewhere = [loc("/home/mere/jazz")];
    assert_eq!(
        d(&p, "claude", None, &add_root(&elsewhere), 5),
        Decision::Ask
    );
    assert_eq!(d(&p, "claude", None, &add_root(&[]), 5), Decision::Ask);

    // By command name; one session only.
    let mut p = with(vec![Grant {
        session: Some("s1".into()),
        ..grant(2, "claude", command("add_root"), Scope::All, Effect::Allow)
    }]);
    assert_eq!(
        d(&p, "claude", Some("s1"), &add_root(&jazz), 5),
        Decision::Allow(GrantId(2))
    );
    assert_eq!(
        d(&p, "claude", Some("s2"), &add_root(&jazz), 5),
        Decision::Ask
    );
    assert_eq!(d(&p, "claude", None, &add_root(&jazz), 5), Decision::Ask);
    let import = Request {
        command: "import_m3u",
        ..add_root(&jazz)
    };
    assert_eq!(d(&p, "claude", Some("s1"), &import, 5), Decision::Ask);

    // Expiry: in effect before `at_ms`, then pruned.
    p.add_grant(Grant {
        until: Expiry::At { at_ms: 10 },
        ..grant(
            3,
            "codex",
            class(OperationClass::LocalPath),
            Scope::All,
            Effect::Allow,
        )
    });
    assert_eq!(
        d(&p, "codex", None, &add_root(&jazz), 9),
        Decision::Allow(GrantId(3))
    );
    assert_eq!(d(&p, "codex", None, &add_root(&jazz), 10), Decision::Ask);
    assert!(p.grant(GrantId(3), 10).is_none());
    assert_eq!(p.take_expired(10).len(), 1);
    assert!(p.take_expired(10).is_empty());
    assert_eq!(p.grants.len(), 1);
}

#[test]
fn forbid_wins_and_purge_is_never_allowed() {
    let target = [Target::Library {
        root: RootId(4),
        rel: String::new(),
        location: Some("/music/4".into()),
        named: None,
        folder: true,
    }];
    let p = with(vec![
        grant(
            1,
            "a",
            class(OperationClass::LocalPath),
            Scope::All,
            Effect::Allow,
        ),
        grant(
            2,
            "a",
            command("remove_root"),
            Scope::Library {
                root: RootId(4),
                prefix: None,
            },
            Effect::Forbid,
        ),
        grant(
            3,
            "a",
            class(OperationClass::Purge),
            Scope::All,
            Effect::Allow,
        ),
        grant(
            4,
            "a",
            command("purge_playlists"),
            Scope::All,
            Effect::Allow,
        ),
        grant(5, "a", command("set_rating"), Scope::All, Effect::Forbid),
    ]);
    let remove = Request {
        command: "remove_root",
        class: OperationClass::LocalPath,
        targets: &target,
    };
    assert_eq!(
        decide_all(&p, "a", None, &remove, 0),
        Decision::Forbid(vec![(GrantId(2), Covers::All)])
    );
    let add = Request {
        command: "add_root",
        ..remove
    };
    assert_eq!(
        decide_all(&p, "a", None, &add, 0),
        Decision::Allow(GrantId(1))
    );
    let purge = Request {
        command: "purge_playlists",
        class: OperationClass::Purge,
        targets: &[],
    };
    assert_eq!(
        decide_all(&p, "a", None, &purge, 0),
        Decision::Ask,
        "purge always asks"
    );
    // A forbid also refuses what an agent may do without asking.
    let rate = Request {
        command: "set_rating",
        class: OperationClass::LibraryEdit,
        targets: &[],
    };
    assert_eq!(
        decide_all(&p, "a", None, &rate, 0),
        Decision::Forbid(vec![(GrantId(5), Covers::All)])
    );
    let p = with(vec![grant(
        6,
        "a",
        class(OperationClass::Purge),
        Scope::All,
        Effect::Forbid,
    )]);
    assert_eq!(
        decide_all(&p, "a", None, &purge, 0),
        Decision::Forbid(vec![(GrantId(6), Covers::All)]),
        "but may be forbidden"
    );
    // Every forbid that matches, by id (the error lists them all).
    let p = with(vec![
        grant(
            9,
            "a",
            class(OperationClass::LocalPath),
            Scope::All,
            Effect::Forbid,
        ),
        grant(
            1,
            "a",
            class(OperationClass::LocalPath),
            Scope::All,
            Effect::Allow,
        ),
        grant(
            7,
            "a",
            command("remove_root"),
            Scope::Library {
                root: RootId(4),
                prefix: None,
            },
            Effect::Forbid,
        ),
        grant(
            8,
            "a",
            command("remove_root"),
            Scope::Library {
                root: RootId(5),
                prefix: None,
            },
            Effect::Forbid,
        ),
    ]);
    assert_eq!(
        decide_all(&p, "a", None, &remove, 0),
        Decision::Forbid(vec![(GrantId(7), Covers::All), (GrantId(9), Covers::All)])
    );
}

#[test]
fn what_can_be_granted() {
    let allow = |op: Operation| grantable(Effect::Allow, &op);
    let forbid = |op: Operation| grantable(Effect::Forbid, &op);
    for ok in [
        "add_root",
        "remove_root",
        "release_root",
        "resume_root",
        "import_m3u",
        "export_m3u",
        "undo_action",
    ] {
        assert!(allow(command(ok)), "{ok}");
        assert!(forbid(command(ok)), "{ok}");
    }
    for never in [
        "purge_playlists",
        "set_rating",
        "play",
        "confirm",
        "grant_permission",
    ] {
        assert!(!allow(command(never)), "{never}");
    }
    assert!(forbid(command("purge_playlists")));
    assert!(forbid(command("set_rating")));
    assert!(forbid(command("play")));
    assert!(!forbid(command("confirm")));
    assert!(!forbid(command("register_agent")));
    assert!(allow(class(OperationClass::LocalPath)));
    assert!(allow(class(OperationClass::FileWrite)));
    assert!(allow(class(OperationClass::Playback)));
    assert!(allow(command("set_output_device")));
    assert!(allow(command("set_output_mode")));
    for never in [
        OperationClass::LibraryEdit,
        OperationClass::Purge,
        OperationClass::Permissions,
        OperationClass::Unknown,
    ] {
        assert!(!allow(class(never)), "{never:?}");
    }
    assert!(forbid(class(OperationClass::Playback)));
    assert!(forbid(class(OperationClass::Purge)));
    assert!(!forbid(class(OperationClass::Permissions)));
    assert!(takes_scope(&class(OperationClass::LocalPath)));
    assert!(takes_scope(&command("undo_action")));
    assert!(!takes_scope(&class(OperationClass::LibraryEdit)));
    assert!(!takes_scope(&command("purge_playlists")));
}

/// A file in a library root, at `rel`, with its location (resolved).
fn in_root(root: i64, rel: &str) -> Target {
    let base = root_location(RootId(root)).unwrap();
    let location = if rel.is_empty() {
        base
    } else {
        format!("{base}/{rel}")
    };
    Target::Library {
        root: RootId(root),
        rel: rel.into(),
        location: Plain.normalize(&location),
        named: None,
        folder: false,
    }
}

fn lib(root: i64, prefix: Option<&str>) -> Scope {
    Scope::Library {
        root: RootId(root),
        prefix: prefix.map(Into::into),
    }
}

#[test]
fn library_scopes_contain_by_root_and_prefix() {
    let x = Doubt::Exclude;
    let i = Doubt::Include;
    let in_jazz = in_root(1, "Jazz/Miles");
    assert!(contains(&lib(1, None), &in_jazz, x));
    assert!(contains(&lib(1, Some("Jazz")), &in_jazz, x));
    assert!(contains(&lib(1, Some("Jazz/Miles")), &in_jazz, x));
    assert!(!contains(&lib(1, Some("Ja")), &in_jazz, x));
    assert!(!contains(&lib(1, Some("Jazz/Miles/x")), &in_jazz, x));
    assert!(!contains(&lib(2, None), &in_jazz, x));
    // Letter case as the root's file system has it; where unknown, per grant.
    assert!(!contains(&lib(1, Some("jazz")), &in_jazz, i));
    assert!(contains(
        &lib(7, Some("jazz")),
        &in_root(7, "Jazz/Miles"),
        x
    ));
    assert!(!contains(
        &lib(8, Some("jazz")),
        &in_root(8, "Jazz/Miles"),
        x
    ));
    assert!(contains(
        &lib(8, Some("jazz")),
        &in_root(8, "Jazz/Miles"),
        i
    ));
    // Locations inside the root's location, or inside the prefix's place in it.
    let file = loc("/music/1/mix.m3u");
    assert!(contains(&lib(1, None), &file, x));
    assert!(!contains(&lib(1, Some("Jazz")), &file, x));
    assert!(contains(
        &lib(1, Some("Jazz")),
        &loc("/music/1/Jazz/mix.m3u"),
        x
    ));
    assert!(!contains(&lib(2, None), &file, x));
    assert!(!contains(&lib(1, None), &loc("/music/12/x.m3u"), x));
    let root = Target::Library {
        root: RootId(1),
        rel: String::new(),
        location: Some("/music/1".into()),
        named: None,
        folder: true,
    };
    assert!(contains(&lib(1, None), &root, x));
    assert!(!contains(&lib(1, Some("Jazz")), &root, x));
    assert!(contains(&folder("/music/1"), &root, x));
    assert!(!contains(&folder("/music/1/Jazz"), &root, x));
    let unknown = Target::Unreadable {
        named: None,
        folder: false,
    };
    assert!(contains(&Scope::All, &unknown, x));
    assert!(!contains(&Scope::Unknown, &root, i));
    // A location core cannot read: only a forbid may contain it.
    assert!(!contains(&folder("/music"), &unknown, x));
    assert!(contains(&folder("/music"), &unknown, i));
    // By its name, where that is known: a forbid elsewhere does not (a drive that is not plugged in is not "forbidden by every rule").
    let named = |path: &str, folder| Target::Unreadable {
        named: Some(path.into()),
        folder,
    };
    assert_eq!(
        reach(&folder("/home/diary"), &named("/media/e/music", true), i),
        Reach::Outside
    );
    assert_eq!(
        reach(
            &folder("/home/diary"),
            &named("/home/diary/2024/x.m3u", false),
            i
        ),
        Reach::Inside
    );
    assert_eq!(
        reach(&folder("/home/diary"), &named("/home", true), i),
        Reach::Holds
    );
    assert_eq!(
        reach(&lib(1, None), &named("/music/1/link", false), i),
        Reach::Inside
    );
    assert_eq!(
        reach(&lib(2, None), &named("/music/1/link", false), i),
        Reach::Outside
    );
    // An allow never contains it, by its name or not.
    assert!(!contains(
        &folder("/media"),
        &named("/media/e/music", true),
        x
    ));
    // A music folder that is not there now: by its name too.
    let unplugged = Target::Library {
        root: RootId(3),
        rel: String::new(),
        location: None,
        named: Some("/media/e/music".into()),
        folder: true,
    };
    assert_eq!(reach(&folder("/home/diary"), &unplugged, i), Reach::Outside);
    assert_eq!(reach(&folder("/media/e"), &unplugged, i), Reach::Inside);
    assert_eq!(reach(&lib(3, Some("Hidden")), &unplugged, i), Reach::Holds);
    assert!(!contains(&folder("/media/e"), &unplugged, x));
    // Where a target is not known, a forbid takes it to be there.
    let nowhere = Target::Library {
        root: RootId(3),
        rel: String::new(),
        location: None,
        named: None,
        folder: false,
    };
    assert!(contains(&folder("/music"), &nowhere, i));
    assert!(contains(&lib(1, None), &nowhere, i));
    assert!(!contains(&folder("/music"), &nowhere, x));
    assert!(!contains(&lib(1, None), &nowhere, x));
    // A prefix that resolves out of the root (a symlink in it): an allow never covers
    // where it leads, a forbid covers both places.
    let link = lib(0, Some("link"));
    assert!(!contains(&link, &loc("/elsewhere/x.m3u"), x));
    assert!(contains(&link, &loc("/elsewhere/x.m3u"), i));
    assert!(!contains(&link, &loc("/music/link/x.m3u"), x));
    // A prefix that is a symlink inside the root: its real place.
    let current = lib(1, Some("Current"));
    assert!(contains(&current, &loc("/music/1/2026/mix.m3u"), x));
    assert!(!contains(&current, &loc("/music/1/2025/mix.m3u"), x));
}

/// A target in a root is judged by `rel` and by where it really is.
#[test]
fn symlinks_inside_a_root_are_judged_where_they_lead() {
    let x = Doubt::Exclude;
    let i = Doubt::Include;
    // `Public/link` leads to `Private`: a forbid of `Private` applies.
    let linked = in_root(1, "Public/link/song.flac");
    assert_eq!(
        linked,
        Target::Library {
            root: RootId(1),
            rel: "Public/link/song.flac".into(),
            location: Some("/music/1/Private/song.flac".into()),
            named: None,
            folder: false,
        }
    );
    assert!(contains(&lib(1, Some("Private")), &linked, i));
    assert!(contains(&folder("/music/1/Private"), &linked, i));
    assert!(contains(&lib(1, Some("Public")), &linked, i));
    // Allowed by `rel` and by where it is: both hold in the root.
    assert!(contains(&lib(1, None), &linked, x));
    assert!(!contains(&lib(1, Some("Public")), &linked, x));
    // `out` leads out of the root: the root's allow does not cover it.
    let out = in_root(1, "out/song.flac");
    assert!(!contains(&lib(1, None), &out, x));
    assert!(contains(&lib(1, None), &out, i));
    assert!(contains(&folder("/elsewhere"), &out, i));
}

/// A forbid of a place inside a folder applies to adding or
/// removing the whole folder (they overlap); an allow of the place does not.
#[test]
fn a_forbid_inside_a_folder_applies_to_the_whole_folder() {
    let x = Doubt::Exclude;
    let i = Doubt::Include;
    let whole = Target::Library {
        root: RootId(1),
        rel: String::new(),
        location: Some("/music/1".into()),
        named: None,
        folder: true,
    };
    for scope in [lib(1, Some("Private")), folder("/music/1/Private")] {
        assert!(contains(&scope, &whole, i), "{scope:?}");
        assert!(!contains(&scope, &whole, x), "{scope:?}");
    }
    // Adding a folder that holds the place.
    for scope in [folder("/music/1/Private"), lib(1, Some("Private"))] {
        assert!(contains(&scope, &dir("/music"), i), "{scope:?}");
        assert!(contains(&scope, &dir("/music/1"), i), "{scope:?}");
        assert!(!contains(&scope, &dir("/music"), x), "{scope:?}");
        assert!(!contains(&scope, &dir("/music/2"), i), "{scope:?}");
    }
    // Only folders: a file next to the place is not in it.
    assert!(!contains(&folder("/music/1/Private"), &loc("/music/1"), i));
    // Through a prefix that is a symlink: the place it leads to counts too.
    assert!(contains(&lib(0, Some("link")), &dir("/elsewhere"), i));
    // As the user sets it up: all local paths allowed, a subfolder forbidden.
    let p = with(vec![
        grant(
            1,
            "a",
            class(OperationClass::LocalPath),
            Scope::All,
            Effect::Allow,
        ),
        grant(
            2,
            "a",
            class(OperationClass::LocalPath),
            folder("/music/1/Private"),
            Effect::Forbid,
        ),
    ]);
    assert_eq!(
        decide_all(&p, "a", None, &add_root(&[dir("/music")]), 0),
        Decision::Forbid(vec![(GrantId(2), Covers::Part)])
    );
    assert_eq!(
        decide_all(&p, "a", None, &add_root(&[dir("/music/2")]), 0),
        Decision::Allow(GrantId(1))
    );
}

#[test]
fn several_targets() {
    let in_root = |root: i64, rel: &str| Target::Library {
        root: RootId(root),
        rel: rel.into(),
        location: Some(format!("/music/{root}")),
        named: None,
        folder: true,
    };
    let both = [in_root(1, ""), in_root(2, "")];
    let p = with(vec![grant(
        1,
        "a",
        class(OperationClass::LocalPath),
        Scope::Library {
            root: RootId(1),
            prefix: None,
        },
        Effect::Allow,
    )]);
    let undo = Request {
        command: "undo_action",
        class: OperationClass::LocalPath,
        targets: &both,
    };
    assert_eq!(
        decide_all(&p, "a", None, &undo, 0),
        Decision::Ask,
        "not all"
    );
    let p = with(vec![
        grant(
            1,
            "a",
            class(OperationClass::LocalPath),
            folder("/music"),
            Effect::Allow,
        ),
        grant(
            2,
            "a",
            command("undo_action"),
            Scope::Library {
                root: RootId(2),
                prefix: None,
            },
            Effect::Forbid,
        ),
    ]);
    assert_eq!(
        decide_all(&p, "a", None, &undo, 0),
        Decision::Forbid(vec![(GrantId(2), Covers::Part)]),
        "any"
    );
    // What a confirmation names: the smallest scope with all of them.
    assert_eq!(common_scope(&both), None);
    assert_eq!(common_scope(&[]), None);
    assert_eq!(
        common_scope(&[in_root(1, "Jazz/Miles/a"), in_root(1, "Jazz/Monk")]),
        Some(Scope::Library {
            root: RootId(1),
            prefix: Some("Jazz".into())
        })
    );
    assert_eq!(
        common_scope(&[in_root(1, "Jazz"), in_root(1, "Rock")]),
        Some(Scope::Library {
            root: RootId(1),
            prefix: None
        })
    );
    assert_eq!(
        common_scope(&[loc("/a")]),
        Some(folder("/a")),
        "a location is its own scope"
    );
    assert_eq!(common_scope(&[loc("/a"), loc("/b")]), None);
    assert_eq!(
        common_scope(&[Target::Unreadable {
            named: None,
            folder: false
        }]),
        None
    );
}

#[test]
fn prefixes() {
    assert_eq!(
        normalize_prefix("/Jazz/Miles/"),
        Ok(Some("Jazz/Miles".into()))
    );
    assert_eq!(normalize_prefix("/"), Ok(None));
    assert_eq!(normalize_prefix(""), Ok(None));
    for bad in ["Jazz//Miles", "./Jazz", "Jazz/..", "Jazz\\Miles", "a/./b"] {
        assert_eq!(normalize_prefix(bad), Err(()), "{bad:?}");
    }
}

#[test]
fn tokens_are_random_and_only_their_hash_is_kept() {
    let token = new_token().unwrap();
    assert_eq!(token.len(), 69);
    assert!(token.starts_with("pmpa_"));
    assert_ne!(token, new_token().unwrap());
    let mut p = with(Vec::new());
    p.add_agent(StoredAgent {
        name: "claude".into(),
        token_hash: Some(token_hash(&token)),
        created_at_ms: 1,
        last_seen_at_ms: None,
    });
    p.add_agent(StoredAgent {
        name: "a".into(),
        token_hash: Some(token_hash("other")),
        created_at_ms: 1,
        last_seen_at_ms: None,
    });
    // A hash that could not be read matches no token, not even the empty one.
    p.add_agent(StoredAgent {
        name: "broken".into(),
        token_hash: None,
        created_at_ms: 1,
        last_seen_at_ms: None,
    });
    assert_eq!(p.authenticate(&token, 7), Some(("claude".into(), true)));
    assert_eq!(p.agent("claude").unwrap().last_seen_at_ms, Some(7));
    // Sightings are written at most once a minute.
    assert_eq!(
        p.authenticate(&token, 59_000),
        Some(("claude".into(), false))
    );
    assert_eq!(p.agent("claude").unwrap().last_seen_at_ms, Some(59_000));
    assert_eq!(
        p.authenticate(&token, 60_007),
        Some(("claude".into(), true))
    );
    assert_eq!(p.authenticate("pmpa_00", 8), None);
    assert_eq!(p.authenticate("", 8), None);
    assert_eq!(
        p.agents()
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        ["a", "broken", "claude"]
    );
    // Unavailable permissions know no one.
    assert_eq!(Permissions::unavailable().authenticate(&token, 9), None);
    // SHA-256 of "abc" (FIPS 180-2).
    assert_eq!(
        token_hash("abc")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert!(same_hash(&[3; 32], &[3; 32]));
    assert!(!same_hash(
        &[3; 32],
        &[[3; 31].as_slice(), &[4]].concat().try_into().unwrap()
    ));
}

#[test]
fn agent_names() {
    for ok in [
        "claude",
        "claude-code",
        "codex_2.1",
        "助手",
        &"a".repeat(64),
    ] {
        check_agent_name(ok, "command.name").unwrap();
        check_grant_agent(ok, "command.agent").unwrap();
    }
    for bad in ["", " ", "a b", "agent:x", "a/b", &"a".repeat(65)] {
        let e = check_agent_name(bad, "command.name").unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidAgentName, "{bad:?}");
        assert_eq!(e.details.field.as_deref(), Some("command.name"));
    }
    // Grants also name agents an adapter names itself: `<platform>:<name>`.
    for ok in ["ios:shortcuts", "android2:assistant", "agent:x"] {
        check_grant_agent(ok, "command.agent").unwrap();
    }
    for bad in [
        "",
        ":x",
        "ios:",
        "iOS:x",
        "ios:a b",
        "ios:x:y",
        "i-os:x",
        &format!("{}:x", "a".repeat(33)),
    ] {
        let e = check_grant_agent(bad, "command.agent").unwrap_err();
        assert_eq!(
            (e.code, e.details.field.as_deref()),
            (ErrorCode::InvalidAgentName, Some("command.agent")),
            "{bad:?}"
        );
    }
}

/// Ended sessions survive agent removal, up to a limit, oldest forgotten first.
#[test]
fn ended_sessions_are_remembered_up_to_a_limit() {
    let mut p = with(Vec::new());
    p.end_session("a", "s0");
    p.end_session("a", "s0");
    p.end_session("b", "s0");
    assert!(p.session_ended("a", "s0") && p.session_ended("b", "s0"));
    assert!(!p.session_ended("a", "s1"));
    assert_eq!(p.ended.order.len(), 2, "each session once");
    p.remove_agent("b");
    assert!(p.session_ended("b", "s0"));
    for i in 1..=MAX_ENDED_SESSIONS {
        p.end_session("a", &format!("s{i}"));
    }
    assert_eq!(p.ended.order.len(), MAX_ENDED_SESSIONS);
    assert_eq!(p.ended.known.len(), MAX_ENDED_SESSIONS);
    assert!(!p.session_ended("a", "s0"), "the oldest is forgotten");
    assert!(p.session_ended("a", "s1"));
    assert!(p.session_ended("a", &format!("s{MAX_ENDED_SESSIONS}")));
}
