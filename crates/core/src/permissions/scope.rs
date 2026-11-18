use super::*;

/// One thing a command acts on, for grant scopes (the table on `contract::COMMANDS`).
/// `folder`: it is a whole folder (adding or removing a music folder, or undoing that), so
/// a `forbid` of any place inside it applies too; else a single location (a file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// A library root, or a `/`-separated location in it (`rel`; empty: the whole root),
    /// with the target's own location in normal form when known: for a whole root the
    /// root's location; for a location in it (a track's file whose tags are edited) the
    /// location `rel` leads to, normalized (`None` when it cannot be: a file gone, a drive
    /// that is not there; [`Target::unread_file`]). `named`: where it is by its name
    /// ([`Locations::nearest`]) when `location` is not known, for `forbid` grants.
    Library {
        root: RootId,
        rel: String,
        location: Option<String>,
        named: Option<String>,
        folder: bool,
    },
    /// A location in the platform layer's normal form.
    Location { location: String, folder: bool },
    /// A location the platform layer cannot read (core then refuses the command:
    /// `invalid_location`): an `allow` never contains it; a `forbid` contains it by where
    /// it is by its name (`named`, [`Locations::nearest`]), or, when not even that is
    /// known, may contain it.
    Unreadable { named: Option<String>, folder: bool },
}

impl Target {
    /// The scope that contains exactly this target.
    fn scope(&self) -> Option<Scope> {
        match self {
            Target::Library { root, rel, .. } => Some(Scope::Library {
                root: *root,
                prefix: (!rel.is_empty()).then(|| rel.clone()),
            }),
            Target::Location { location, .. } => Some(Scope::Location {
                location: location.clone(),
            }),
            Target::Unreadable { .. } => None,
        }
    }

    /// Its location as `doubt` takes it: for a `forbid` (`Include`), where it is by its
    /// name when its real location is not known; for an `allow`, only its real location.
    fn location(&self, doubt: Doubt) -> Option<&str> {
        let (location, named) = match self {
            Target::Library {
                location, named, ..
            } => (location.as_deref(), named.as_deref()),
            Target::Location { location, .. } => (Some(location.as_str()), None),
            Target::Unreadable { named, .. } => (None, named.as_deref()),
        };
        match doubt {
            Doubt::Include => location.or(named),
            Doubt::Exclude => location,
        }
    }

    /// Whether its location cannot be read (whatever it is by its name).
    pub(crate) fn unreadable(&self) -> bool {
        matches!(self, Target::Unreadable { .. })
    }

    /// A file in a library root whose location cannot be read now (gone, a drive that is
    /// not there): its path in the root.
    pub(crate) fn unread_file(&self) -> Option<&str> {
        match self {
            Target::Library {
                rel,
                location: None,
                folder: false,
                ..
            } => Some(rel),
            _ => None,
        }
    }

    /// Whether not even where it is by its name is known (a music folder whose location did
    /// not answer in time, say).
    pub(crate) fn nowhere(&self) -> bool {
        matches!(
            self,
            Target::Unreadable { named: None, .. }
                | Target::Library {
                    location: None,
                    named: None,
                    ..
                }
        )
    }

    pub(crate) fn folder(&self) -> bool {
        match self {
            Target::Library { folder, .. }
            | Target::Location { folder, .. }
            | Target::Unreadable { folder, .. } => *folder,
        }
    }
}

/// The smallest single scope that contains every target, as far as core can tell without
/// the platform layer (`PendingConfirmation.scope`): the targets' own scope when they all
/// have the same one, or the common prefix of targets in one library root; `None`
/// otherwise (no targets, targets in different roots, different locations: core does not
/// work out the folder that holds them).
pub(crate) fn common_scope(targets: &[Target]) -> Option<Scope> {
    let (first, rest) = targets.split_first()?;
    let mut scope = first.scope()?;
    for target in rest {
        scope = match (scope, target.scope()?) {
            (
                Scope::Library { root, prefix: a },
                Scope::Library {
                    root: other,
                    prefix: b,
                },
            ) if root == other => {
                let (a, b) = (a.unwrap_or_default(), b.unwrap_or_default());
                let common: Vec<&str> = a
                    .split('/')
                    .zip(b.split('/'))
                    .take_while(|(x, y)| x == y && !x.is_empty())
                    .map(|(x, _)| x)
                    .collect();
                Scope::Library {
                    root,
                    prefix: (!common.is_empty()).then(|| common.join("/")),
                }
            }
            (a, b) if a == b => a,
            _ => return None,
        };
    }
    Some(scope)
}

/// Whether `scope` contains `target`: a library scope by root and `/`-separated prefix
/// (core's own rule; letter case as the root's file system has it), and the locations in
/// that place in the root's location; a location by the platform layer. `doubt` decides
/// what cannot be told. `root_location`: a root's location, `None` when unknown.
///
/// For a `forbid` (`Doubt::Include`) a scope also contains a folder target that holds it:
/// adding or removing a whole folder acts on every place inside it (they overlap;
/// [`Reach::Holds`]). An `allow` must hold the whole target (allowing a subfolder never
/// allows its parent).
pub(crate) fn scope_reach(
    scope: &Scope,
    target: &Target,
    doubt: Doubt,
    locations: &dyn Locations,
    root_location: impl Fn(RootId) -> Option<String>,
) -> Reach {
    let unsure = doubt == Doubt::Include;
    let inside = match (scope, target) {
        (Scope::All, _) => return Reach::Inside,
        // A target whose location is not known (not even by its name, for a `forbid`) may
        // be anywhere: unsure.
        (Scope::Location { location }, target) => target
            .location(doubt)
            .map_or(unsure, |t| locations.contains(location, t, doubt)),
        (Scope::Library { root, prefix }, target) => {
            let place = Place::new(*root, prefix.as_deref(), locations, &root_location);
            match target {
                // In the same root: by `rel`, and by where the target really is (a symlink
                // in the root may lead elsewhere): an `allow` needs both, a `forbid`
                // either.
                Target::Library { root: r, rel, .. } if r == root => {
                    let by_rel = within(rel, &place.prefix, place.ignores_case(unsure));
                    let by_location = target
                        .location(doubt)
                        .map_or(unsure, |t| place.holds(t, doubt));
                    if unsure {
                        by_rel || by_location
                    } else {
                        by_rel && by_location
                    }
                }
                other => other
                    .location(doubt)
                    .map_or(unsure, |t| place.holds(t, doubt)),
            }
        }
        // A scope this build does not know never applies.
        _ => return Reach::Outside,
    };
    if inside {
        Reach::Inside
    } else if unsure && target.folder() && holds_scope(scope, target, locations, &root_location) {
        Reach::Holds
    } else {
        Reach::Outside
    }
}

/// How a grant's scope and a command's target meet ([`scope_reach`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reach {
    /// The target is not in the scope.
    Outside,
    /// The target is in the scope (or, for a `forbid`, may be).
    Inside,
    /// For a `forbid`: the target, a folder, holds the scope (or may).
    Holds,
}

/// Whether the folder `target` holds the place `scope` names (for a `forbid`: unsure is
/// yes).
fn holds_scope(
    scope: &Scope,
    target: &Target,
    locations: &dyn Locations,
    root_location: &impl Fn(RootId) -> Option<String>,
) -> bool {
    let doubt = Doubt::Include;
    match scope {
        Scope::Location { location } => target
            .location(doubt)
            .is_none_or(|t| locations.contains(t, location, doubt)),
        Scope::Library { root, prefix } => {
            let place = Place::new(*root, prefix.as_deref(), locations, root_location);
            match target {
                Target::Library { root: r, rel, .. } if r == root => {
                    within(&place.prefix, rel, place.ignores_case(true))
                }
                other => match (other.location(doubt), &place.named) {
                    (Some(t), Some(Some(named))) => {
                        locations.contains(t, named, doubt)
                            || locations
                                .normalize(named)
                                .is_some_and(|real| locations.contains(t, &real, doubt))
                    }
                    _ => true,
                },
            }
        }
        _ => false,
    }
}

/// Where a library scope is: its root's location, or its prefix's place in it.
struct Place<'a> {
    prefix: String,
    /// The place as named (`None`: the root's location is unknown; `Some(None)`: the
    /// prefix cannot be a place in it).
    named: Option<Option<String>>,
    /// The root's location as it resolves now.
    root_real: Option<String>,
    locations: &'a dyn Locations,
}

impl<'a> Place<'a> {
    fn new(
        root: RootId,
        prefix: Option<&str>,
        locations: &'a dyn Locations,
        root_location: &impl Fn(RootId) -> Option<String>,
    ) -> Self {
        let prefix = prefix.unwrap_or_default().to_owned();
        let root = root_location(root);
        let root_real = root.as_deref().and_then(|r| locations.normalize(r));
        let named = root.map(|r| {
            if prefix.is_empty() {
                Some(r)
            } else {
                locations.child(&r, &prefix)
            }
        });
        Self {
            prefix,
            named,
            root_real,
            locations,
        }
    }

    /// Whether letter case is ignored in this place (`if_unsure` when that cannot be told).
    fn ignores_case(&self, if_unsure: bool) -> bool {
        self.named
            .clone()
            .flatten()
            .and_then(|p| self.locations.ignores_case(&p))
            .unwrap_or(if_unsure)
    }

    /// Whether the location `target` (normal form) is in this place. As named, or as it
    /// resolves now (the prefix may be a symlink in the root): a `forbid` applies either
    /// way; an `allow` only where the place really is, and only while that is still inside
    /// the root's real location (a symlink out of the root is not the root).
    fn holds(&self, target: &str, doubt: Doubt) -> bool {
        let l = self.locations;
        match &self.named {
            Some(Some(named)) => {
                let real = l.normalize(named);
                match doubt {
                    Doubt::Include => {
                        l.contains(named, target, doubt)
                            || real.is_some_and(|r| l.contains(&r, target, doubt))
                    }
                    Doubt::Exclude => real.is_some_and(|r| {
                        self.root_real
                            .as_deref()
                            .is_some_and(|root| l.contains(root, &r, doubt))
                            && l.contains(&r, target, doubt)
                    }),
                }
            }
            // The root's location is unknown, or the prefix cannot be a place in it.
            _ => doubt == Doubt::Include,
        }
    }
}

/// Whether the library location `rel` is `prefix` or under it (by `/`-separated parts).
fn within(rel: &str, prefix: &str, ignore_case: bool) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let (rel, prefix) = if ignore_case {
        (rel.to_lowercase(), prefix.to_lowercase())
    } else {
        (rel.to_owned(), prefix.to_owned())
    };
    rel == prefix || rel.starts_with(&format!("{prefix}/"))
}

/// A library prefix as stored: without leading and trailing `/` (`None`: the whole root);
/// `Err` for a part that is empty, `.` or `..`, or holds a backslash.
pub(crate) fn normalize_prefix(prefix: &str) -> Result<Option<String>, ()> {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(None);
    }
    let ok = trimmed
        .split('/')
        .all(|part| !matches!(part, "" | "." | "..") && !part.contains('\\'));
    if ok {
        Ok(Some(trimmed.to_owned()))
    } else {
        Err(())
    }
}
