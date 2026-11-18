use super::*;

/// The commands that act on something (see the table on `contract::COMMANDS`); the others
/// act on nothing in particular, so only the scope `all` contains them.
const TARGETED: &[&str] = &[
    "add_root",
    "remove_root",
    "release_root",
    "resume_root",
    "import_m3u",
    "export_m3u",
    "undo_action",
    "edit_tags",
    "edit_cover",
];

/// What the candidate grants say about an agent's command: a `forbid` that
/// contains any of its targets refuses it; else an `allow` that contains all of them runs
/// it (never permanently deleting library data, nor the user's own commands); else no
/// grant applies. A command with no targets is contained only by `all`. `reach` says
/// whether a scope contains a target. Each `forbid` says whether it covers all of the
/// command (every target inside its scope) or a part (a folder target holds its scope).
pub(crate) fn decide(
    grants: &[Grant],
    request: &Request<'_>,
    reach: impl Fn(&Scope, &Target, Doubt) -> Reach,
) -> Decision {
    let allows = |scope: &Scope| {
        if request.targets.is_empty() {
            return matches!(scope, Scope::All);
        }
        request
            .targets
            .iter()
            .all(|t| reach(scope, t, Doubt::Exclude) != Reach::Outside)
    };
    // Each target is reached once per `forbid` (the platform layer may be slow, within the
    // agent's time limit): whether it applies and how much it covers come from the same
    // answers.
    let forbid = |scope: &Scope| {
        if request.targets.is_empty() {
            return matches!(scope, Scope::All).then_some(Covers::All);
        }
        let reaches: Vec<Reach> = request
            .targets
            .iter()
            .map(|t| reach(scope, t, Doubt::Include))
            .collect();
        if reaches.iter().all(|r| *r == Reach::Inside) {
            Some(Covers::All)
        } else if reaches.iter().any(|r| *r != Reach::Outside) {
            Some(Covers::Part)
        } else {
            None
        }
    };
    let mut forbids: Vec<(GrantId, Covers)> = grants
        .iter()
        .filter(|g| g.effect == Effect::Forbid)
        .filter_map(|g| Some((g.id, forbid(&g.scope)?)))
        .collect();
    if !forbids.is_empty() {
        forbids.sort_by_key(|(id, _)| *id);
        return Decision::Forbid(forbids);
    }
    if !may_allow(request.class, request.command) {
        return Decision::Ask;
    }
    match grants
        .iter()
        .find(|g| g.effect == Effect::Allow && allows(&g.scope))
    {
        Some(allow) => Decision::Allow(allow.id),
        None => Decision::Ask,
    }
}

/// Whether a grant is in effect at `now` (an expiry this build does not know never is).
pub(super) fn live(grant: &Grant, now: i64) -> bool {
    match grant.until {
        Expiry::Session | Expiry::Forever => true,
        Expiry::At { at_ms } => now < at_ms,
        _ => false,
    }
}

/// Whether a grant may allow a command at all: never permanently deleting library data,
/// nor the user's own commands.
fn may_allow(class: OperationClass, command: &str) -> bool {
    let spec = COMMANDS.iter().find(|s| s.name == command);
    !matches!(class, OperationClass::Purge | OperationClass::Permissions)
        && spec.is_none_or(|s| !s.ui_only && s.class != OperationClass::Purge)
}

/// Whether a grant with `effect` for `operation` (a known command or class) could ever
/// apply. An `allow` applies only to what an agent would be asked about (a command whose
/// default for agents is `confirm`, or `undo_action`, which asks for folder changes), and
/// never to permanently deleting library data or the user's own commands. A `forbid`
/// applies to anything an agent may send, so not to the user's own commands.
pub fn grantable(effect: Effect, operation: &Operation) -> bool {
    let agent = Origin::agent("");
    let asks = |name: &str| {
        COMMANDS.iter().any(|s| {
            s.name == name && (s.policy(&agent) == Policy::Confirm || s.name == "undo_action")
        })
    };
    match (effect, operation) {
        (Effect::Forbid, Operation::Command { command }) => {
            COMMANDS.iter().any(|s| s.name == command && !s.ui_only)
        }
        (Effect::Forbid, Operation::Class { class }) => {
            !matches!(class, OperationClass::Permissions | OperationClass::Unknown)
        }
        (Effect::Allow, Operation::Command { command }) => {
            asks(command)
                && COMMANDS
                    .iter()
                    .any(|s| may_allow(s.class, s.name) && s.name == command)
        }
        (Effect::Allow, Operation::Class { class }) => {
            may_allow(*class, "")
                && COMMANDS
                    .iter()
                    .any(|s| s.class == *class && s.policy(&agent) == Policy::Confirm)
        }
        _ => false,
    }
}

/// Whether `operation` acts on something, so that a scope other than `all` can apply to
/// it: `local_path` and `file_write`, and the commands in `TARGETED`.
pub fn takes_scope(operation: &Operation) -> bool {
    match operation {
        Operation::Command { command } => TARGETED.contains(&command.as_str()),
        Operation::Class { class } => {
            matches!(class, OperationClass::LocalPath | OperationClass::FileWrite)
        }
        _ => false,
    }
}
