//! An agent's command, checked before it runs or is held for the user, and checked again
//! when the user approves it (docs/design/agent-permissions.md): what it acts on, in
//! the platform layer's normal form; what the user's grants say; whether it can run, or
//! would change nothing; what the confirmation shows. Every file system call goes through
//! the command's time limit ([`Bounded`]), and nothing here needs the actor: for a command
//! that acts on a location or a music folder this runs on a thread of its own, and the
//! actor goes on with other messages until the answer comes back.

mod context;
mod initial;
pub(crate) use initial::Check;
mod recheck;
pub(crate) use recheck::Recheck;
mod messages;
pub(crate) use messages::unreadable_location;
use messages::{confirmation_summary, malformed_location, permission_denied, unreadable_files};

use std::collections::HashMap;
use std::sync::Arc;

use crate::catalog::{Catalog, check_tracks_exist};
use crate::contract::{
    Command, ContractError, Covers, ErrorCode, ForbiddenBy, Grant, GrantId, MAX_FORBIDDEN_BY,
    Operation, OperationClass, Origin, Policy, RootId, Scope, TagChange, TrackId,
};
use crate::permissions::fs_deadline::{Bounded, Limits};
use crate::permissions::{self, Decision, Doubt, Locations, MAX_LOCATION, Target};

/// What the checks need besides the command: the catalog, and the platform layer (with
/// the command's time limit, for an agent).
pub(crate) struct CheckContext<'a> {
    pub catalog: &'a dyn Catalog,
    pub locations: &'a dyn Locations,
}

/// What a confirmation shows of a held command (`PendingConfirmation`).
pub(crate) struct View {
    pub sources: Option<Vec<mpmp_library::CoverSource>>,
    pub summary: String,
    pub scope: Option<Scope>,
    pub display_location: Option<String>,
}

/// Whether a command's checks call the file system (it acts on a location or a music
/// folder): those of an agent run off the actor.
pub(crate) fn slow(command: &Command) -> bool {
    permissions::takes_scope(&Operation::Command {
        command: command.name().to_owned(),
    })
}

/// What the checks of an agent's command found.
pub(crate) enum Checked {
    /// Refused, or it cannot run: the answer.
    Refused(ContractError),
    /// It runs now (in normal form): allowed by `grant`, or asking no one; with what it
    /// acts on as checked (a tag job writes those files and no other).
    Run {
        command: Command,
        grant: Option<GrantId>,
        targets: Vec<Target>,
    },
    /// It waits for the user (in normal form, as checked and shown).
    Hold {
        command: Command,
        class: OperationClass,
        targets: Vec<Target>,
        view: Box<View>,
    },
}

/// What can be checked about a command without running it (ids, enum values, paths):
/// before it is held for confirmation. `bounded`: an agent's command, whose checks of the
/// file system (the path it names) run with its time limit; one that does not answer in
/// time is `invalid_location`.
pub(crate) fn precheck(
    catalog: &Arc<dyn Catalog>,
    command: &Command,
    bounded: Option<&Bounded>,
) -> Result<(), ContractError> {
    if let Command::ResolveCoverRecovery {
        resolution: crate::contract::CoverRecoveryResolution::Unknown,
        ..
    } = command
    {
        return Err(ContractError::unknown_variant("cover recovery resolution")
            .with_field("command.resolution"));
    }
    if let Command::ResolveCoverRecovery {
        selection: Some(selection),
        resolution,
        ..
    } = command
    {
        if selection.role == crate::contract::CoverCandidateRole::Unknown {
            return Err(ContractError::unknown_variant("cover candidate role")
                .with_field("command.selection.role"));
        }
        if selection.intent <= 0
            || selection.sha256.len() != 64
            || !selection
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            || !matches!(
                resolution,
                crate::contract::CoverRecoveryResolution::RestoreBefore
                    | crate::contract::CoverRecoveryResolution::RestoreAfter
            )
        {
            return Err(ContractError::invalid(
                ErrorCode::OutOfRange,
                "Select an intent, candidate role and complete lowercase SHA-256 for a restore",
            )
            .with_field("command.selection"));
        }
    }
    if let Command::ExportM3u { path, .. } | Command::ImportM3u { path, .. } = command
        && !is_m3u(path)
    {
        return Err(ContractError::invalid(
            ErrorCode::NotAnM3u,
            format!("{path} is not an .m3u or .m3u8 file"),
        )
        .with_field("command.path"));
    }
    match bounded {
        Some(bounded) if command_path(command).is_some() => {
            let (catalog, held) = (Arc::clone(catalog), command.clone());
            bounded
                .run(move || catalog.precheck(&held))
                .unwrap_or_else(|| Err(unreadable_location(command, true)))
        }
        _ => catalog.precheck(command),
    }
}

/// Whether the command would change nothing (then it runs without asking). For an
/// agent's command naming a path, with its time limit: one that does not answer in time
/// is taken to change something (the user is asked).
pub(crate) fn is_noop(
    catalog: &Arc<dyn Catalog>,
    command: &Command,
    bounded: Option<&Bounded>,
) -> bool {
    match bounded {
        Some(bounded) if command_path(command).is_some() => {
            let (catalog, held) = (Arc::clone(catalog), command.clone());
            bounded.run(move || catalog.is_noop(&held)).unwrap_or(false)
        }
        _ => catalog.is_noop(command),
    }
}

/// The location a command names, if any.
pub(crate) fn command_path(command: &Command) -> Option<&str> {
    match command {
        Command::AddRoot { path }
        | Command::ImportM3u { path, .. }
        | Command::ExportM3u { path, .. } => Some(path),
        _ => None,
    }
}

/// An M3U file name (what `ImportM3u` / `ExportM3u` accept).
fn is_m3u(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("m3u") || e.eq_ignore_ascii_case("m3u8"))
}

/// Each track once, in the order first named (the files a tag edit writes).
pub(crate) fn unique(tracks: &[TrackId]) -> Vec<TrackId> {
    let mut seen = std::collections::HashSet::new();
    tracks.iter().copied().filter(|t| seen.insert(*t)).collect()
}

#[cfg(all(test, feature = "desktop-fs"))]
mod tests {
    use super::*;
    use crate::catalog::LibraryRoots;
    use crate::contract::{Effect, Expiry};

    #[test]
    fn confirmation_locations_use_platform_case_semantics() {
        struct Insensitive;
        impl Locations for Insensitive {
            fn normalize(&self, location: &str) -> Option<String> {
                Some(location.to_uppercase())
            }
            fn contains(&self, scope: &str, location: &str, _: Doubt) -> bool {
                let scope = scope.to_lowercase();
                let location = location.to_lowercase();
                location == scope || location.starts_with(&format!("{scope}/"))
            }
            fn child(&self, location: &str, relative: &str) -> Option<String> {
                Some(format!("{location}/{relative}"))
            }
            fn ignores_case(&self, _: &str) -> Option<bool> {
                Some(true)
            }
            fn display(&self, location: &str) -> String {
                location.to_owned()
            }
        }
        let dir = mpmp_test_support::TempDir::new().unwrap();
        let catalog = mpmp_library::Library::open(&dir.path().join("library.db")).unwrap();
        let root = RootId(
            catalog
                .add_root(dir.path(), mpmp_library::ActionMeta::new("ui"))
                .unwrap()
                .value
                .0,
        );
        let place = catalog.root_location(root).unwrap().unwrap();
        let target = Target::Library {
            root,
            rel: "Album/Song.flac".into(),
            location: Some(format!("{}/album/song.flac", place.to_lowercase())),
            named: None,
            folder: false,
        };
        let command = Command::EditTags {
            tracks: vec![TrackId(1)],
            changes: vec![TagChange::Remove {
                key: "TITLE".into(),
            }],
        };
        let ctx = CheckContext {
            catalog: &catalog,
            locations: &Insensitive,
        };
        let view = ctx.view(&command, std::slice::from_ref(&target));
        assert!(!view.summary.contains("link"), "{}", view.summary);
        let scope = view.scope.unwrap();
        assert!(matches!(scope, Scope::Library { root: r, .. } if r == root));
        let allow = Grant {
            id: GrantId(1),
            agent: "test".into(),
            session: None,
            operation: Operation::Class {
                class: OperationClass::FileWrite,
            },
            scope,
            until: Expiry::Forever,
            effect: Effect::Allow,
            created_at_ms: 0,
        };
        assert_eq!(
            ctx.decide(&[allow], &command, OperationClass::FileWrite, &[target]),
            Decision::Allow(GrantId(1))
        );
    }

    #[test]
    fn tag_confirmation_quotes_keys_that_could_impersonate_instructions() {
        let command = Command::EditTags {
            tracks: vec![TrackId(1)],
            changes: vec![TagChange::Set {
                key: "TITLE TO \"SAFE\"; REMOVE X".into(),
                values: vec!["evil".into()],
            }],
        };
        let summary = confirmation_summary(&command, &[]);
        assert!(
            summary.contains(r#""TITLE TO \"SAFE\"; REMOVE X""#),
            "{summary}"
        );
        assert!(summary.contains("bytes of keys and values per file"));
    }

    /// `permission_denied` lists the forbids that refused a command, by id, at most
    /// [`MAX_FORBIDDEN_BY`], with how much of it each covers; the message names each and
    /// says how many more there were. Neither says where they forbid.
    #[test]
    fn a_refusal_names_its_grants_not_their_places() {
        let forbid = |id: i64, scope: Scope| Grant {
            id: GrantId(id),
            agent: "claude".into(),
            session: None,
            operation: Operation::Class {
                class: OperationClass::LocalPath,
            },
            scope,
            until: Expiry::Forever,
            effect: Effect::Forbid,
            created_at_ms: 0,
        };
        let mut candidates: Vec<Grant> = (1..=10)
            .map(|id| forbid(id, Scope::All))
            .chain([forbid(
                11,
                Scope::Location {
                    location: "/music/Private".into(),
                },
            )])
            .collect();
        candidates.push(forbid(
            12,
            Scope::Library {
                root: crate::contract::RootId(3),
                prefix: Some("Hidden".into()),
            },
        ));
        let command = Command::AddRoot {
            path: "/music".into(),
        };
        let origin = Origin::agent("claude");
        let part = [(GrantId(11), Covers::Part), (GrantId(12), Covers::Part)];
        let e = permission_denied(&origin, &command, &[], &candidates, &part);
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert_eq!(
            e.message,
            "the user forbade agent:claude this command (add_root); forbidden by grant 11 \
             (local_path commands: a part of what it acts on), grant 12 (local_path commands: \
             a part of what it acts on); what lies outside the forbidden part may still be \
             asked for, a part at a time (a subfolder, say)"
        );
        assert_eq!(
            e.details
                .forbidden_by
                .iter()
                .map(|f| (f.grant.0, f.covers))
                .collect::<Vec<_>>(),
            [(11, Covers::Part), (12, Covers::Part)]
        );
        let json = serde_json::to_string(&e).unwrap();
        for place in ["Private", "Hidden", "library folder", "\"root\"", "scope"] {
            assert!(!json.contains(place), "{place} in {json}");
        }
        let mut all: Vec<(GrantId, Covers)> =
            (1..=12).map(|id| (GrantId(id), Covers::Part)).collect();
        all[11].1 = Covers::All;
        let e = permission_denied(&origin, &command, &[], &candidates, &all);
        assert_eq!(e.details.forbidden_by.len(), MAX_FORBIDDEN_BY);
        // The one that covers all of it comes first, then by id: never left out.
        assert_eq!(
            e.details
                .forbidden_by
                .iter()
                .map(|f| (f.grant.0, f.covers))
                .take(3)
                .collect::<Vec<_>>(),
            [(12, Covers::All), (1, Covers::Part), (2, Covers::Part)]
        );
        assert!(e.message.ends_with(", and 4 more"), "{}", e.message);
        // Removing a music folder cannot be done a part at a time: no hint.
        let remove = Command::RemoveRoot {
            root: crate::contract::RootId(3),
        };
        let e = permission_denied(&origin, &remove, &[], &candidates, &part);
        assert_eq!(e.details.forbidden_by[0].covers, Covers::Part);
        assert!(!e.message.contains("a part at a time"), "{}", e.message);
        // Nor a folder that cannot be read: what is inside it cannot be either.
        let unreadable = [Target::Unreadable {
            named: Some("/music".into()),
            folder: true,
        }];
        let e = permission_denied(&origin, &command, &unreadable, &candidates, &part);
        assert!(!e.message.contains("a part at a time"), "{}", e.message);
    }

    #[test]
    fn m3u_names() {
        assert!(is_m3u("/a/b.m3u"));
        assert!(is_m3u("C:\\x\\Mix.M3U8"));
        assert!(!is_m3u("/a/b.txt"));
        assert!(!is_m3u("/a/m3u"));
    }
}
