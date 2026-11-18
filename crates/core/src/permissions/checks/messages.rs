use super::*;

/// The user forbade the agent this command: the `forbid` grants `forbids` (of
/// `candidates`, by id, with how much of the command each covers), each named in the
/// message and listed in `details.forbidden_by` (at most [`MAX_FORBIDDEN_BY`]; those that
/// cover all of it first, so that one always shows), so the agent can tell the user which,
/// and what it may still do. Neither says where a grant forbids: the places a user forbids
/// an agent are not the agent's to learn.
pub(crate) fn permission_denied(
    origin: &Origin,
    command: &Command,
    targets: &[Target],
    candidates: &[Grant],
    forbids: &[(GrantId, Covers)],
) -> ContractError {
    let mut forbids = forbids.to_vec();
    forbids.sort_by_key(|(id, covers)| (*covers != Covers::All, *id));
    let forbidden_by: Vec<ForbiddenBy> = forbids
        .iter()
        .filter_map(|(id, covers)| {
            let grant = candidates.iter().find(|g| g.id == *id)?;
            Some(ForbiddenBy {
                grant: grant.id,
                operation: grant.operation.clone(),
                covers: *covers,
            })
        })
        .take(MAX_FORBIDDEN_BY)
        .collect();
    let mut by: Vec<String> = forbidden_by
        .iter()
        .map(|f| {
            let covers = match f.covers {
                Covers::All => "all of what it acts on",
                Covers::Part => "a part of what it acts on",
                _ => "an unknown part of what it acts on",
            };
            format!(
                "grant {} ({}: {covers})",
                f.grant.0,
                operation_text(&f.operation)
            )
        })
        .collect();
    if forbids.len() > forbidden_by.len() {
        by.push(format!("and {} more", forbids.len() - forbidden_by.len()));
    }
    // Only a folder to add can be asked for a part at a time (removing a music folder, or
    // undoing a change, cannot), and only one that can be read (what is inside one that
    // cannot cannot be read either).
    let all = forbids.iter().any(|(_, covers)| *covers == Covers::All);
    let splits =
        matches!(command, Command::AddRoot { .. }) && !targets.iter().any(Target::unreadable);
    let rest = if all || !splits {
        ""
    } else {
        "; what lies outside the forbidden part may still be asked for, a part at a time (a \
         subfolder, say)"
    };
    ContractError::forbidden(
        ErrorCode::PermissionDenied,
        format!(
            "the user forbade {} this command ({}); forbidden by {}{rest}",
            origin.label(),
            command.name(),
            by.join(", ")
        ),
    )
    .with_forbidden_by(forbidden_by)
}

/// A grant's operation in a message: the command's name, or `<class> commands`.
fn operation_text(operation: &Operation) -> String {
    match operation {
        Operation::Command { command } => command.clone(),
        Operation::Class { class } => format!("{} commands", enum_name(class)),
        _ => "unknown operation".into(),
    }
}

/// The JSON name of a unit enum value (`local_path`).
fn enum_name(value: &impl serde::Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        _ => "unknown".into(),
    }
}

/// An agent's location that core cannot act on: one the platform layer cannot read, or
/// (`late`) whose file system did not answer within the command's time limit.
pub(crate) fn unreadable_location(command: &Command, late: bool) -> ContractError {
    let Some(path) = command_path(command) else {
        // A music folder's (`RemoveRoot`, a folder undo).
        return ContractError::invalid(
            ErrorCode::InvalidLocation,
            "the location of a music folder it acts on did not answer in time (an offline or \
             slow network location?): core cannot act on a location it cannot check",
        );
    };
    let message = if late {
        format!(
            "{path:?} did not answer in time (an offline or slow network location?): core \
             cannot act on a location it cannot check"
        )
    } else {
        format!(
            "{path:?} cannot be read as a location now (a link to nothing, a drive or share \
             that is not there?)"
        )
    };
    ContractError::invalid(ErrorCode::InvalidLocation, message).with_field("command.path")
}

/// The files of a tag edit, or of its undo, that cannot be read now (gone, or on a drive
/// that is not plugged in), when the command cannot run for them (guarantee 12: the user
/// is not asked to approve it): a tag edit none of whose
/// files can be read, an undo with any such file (it writes every file back, or none).
/// A tag edit with some it can read runs: its job skips the others (`file_missing`), as
/// any file gone by then, and a confirmation says so.
pub(super) fn unreadable_files(command: &Command, targets: &[Target]) -> Option<ContractError> {
    let files = targets.iter().filter(|t| !t.folder()).count();
    let unread: Vec<&str> = targets.iter().filter_map(Target::unread_file).collect();
    let (field, what) = match command {
        Command::EditTags { .. } | Command::EditCover { .. }
            if files > 0 && unread.len() == files =>
        {
            ("command.tracks", "none of the files it rewrites")
        }
        Command::UndoAction { .. } if !unread.is_empty() => (
            "command.action",
            "it writes every file back or none, and some of them",
        ),
        _ => return None,
    };
    const SHOWN: usize = 5;
    let mut named: Vec<String> = unread
        .iter()
        .take(SHOWN)
        .map(|p| format!("{p:?}"))
        .collect();
    if unread.len() > SHOWN {
        named.push(format!("and {} more", unread.len() - SHOWN));
    }
    Some(
        ContractError::invalid(
            ErrorCode::InvalidLocation,
            format!(
                "{what} cannot be read now (gone, or on a drive or share that is not there?): \
                 {} in their music folders",
                named.join(", ")
            ),
        )
        .with_field(field),
    )
}

/// An agent's location that cannot name a place, by its text alone.
pub(super) fn malformed_location(path: &str) -> ContractError {
    ContractError::invalid(
        ErrorCode::InvalidLocation,
        format!(
            "{path:?} is not a location core takes from an agent (it must be absolute, with \
             no `..` part and no control character, at most {MAX_LOCATION} characters)"
        ),
    )
    .with_field("command.path")
}

/// What a confirmation says the command does (`targets`: what it acts on).
pub(crate) fn confirmation_summary(command: &Command, targets: &[Target]) -> String {
    let files = |n: usize| match n {
        1 => "1 file".to_owned(),
        n => format!("{n} files"),
    };
    match command {
        Command::EditCover { tracks, change } => {
            let image = crate::catalog::cover_change(change)
                .ok()
                .and_then(|c| c.image());
            let operation = if image.is_some() { "set" } else { "remove" };
            let mut summary = format!(
                "{operation} embedded front covers of {} (MP4: all covr items)",
                files(unique(tracks).len())
            );
            if let Some(i) = image {
                summary.push_str(&format!(
                    "; {} bytes {}, SHA-256 {}",
                    i.bytes,
                    i.mime,
                    &i.sha256[..12]
                ));
            }
            summary
        }
        Command::EditTags { tracks, changes } => {
            const SHOWN: usize = 5;
            let mut parts: Vec<String> = changes
                .iter()
                .take(SHOWN)
                .map(|c| match c {
                    // Quoted and escaped (control and text-direction characters), and cut: an
                    // agent's text must not make the dialog say something else.
                    TagChange::Set { key, values } => {
                        let mut shown: Vec<String> = values
                            .iter()
                            .take(SHOWN)
                            .map(|v| {
                                let cut: String = v.trim().chars().take(60).collect();
                                let more = if cut.len() < v.trim().len() {
                                    "…"
                                } else {
                                    ""
                                };
                                format!("{cut:?}{more}")
                            })
                            .collect();
                        if values.len() > SHOWN {
                            shown.push(format!("and {} more", values.len() - SHOWN));
                        }
                        format!(
                            "set {} to {}",
                            mpmp_library::display_tag_key(key),
                            shown.join(", ")
                        )
                    }
                    TagChange::Remove { key } => {
                        format!("remove {}", mpmp_library::display_tag_key(key))
                    }
                    _ => "an unknown change".into(),
                })
                .collect();
            if changes.len() > SHOWN {
                parts.push(format!("and {} more changes", changes.len() - SHOWN));
            }
            format!(
                "rewrite the tags of {}: {} ({} bytes of keys and values per file)",
                files(unique(tracks).len()),
                parts.join("; "),
                changes
                    .iter()
                    .map(|c| match c {
                        TagChange::Set { key, values } =>
                            key.len() + values.iter().map(String::len).sum::<usize>(),
                        TagChange::Remove { key } => key.len(),
                        _ => 0,
                    })
                    .sum::<usize>()
            )
        }
        Command::UndoAction { action } if targets.iter().any(|t| !t.folder()) => format!(
            "undo action {}, writing the old tags back into {}",
            action.0,
            files(targets.len())
        ),
        Command::PurgePlaylists { playlists: None } => {
            "permanently delete every playlist in the trash".into()
        }
        Command::PurgePlaylists {
            playlists: Some(ids),
        } => format!(
            "permanently delete {} playlist(s) from the trash",
            ids.len()
        ),
        Command::ExportM3u { path, .. } => format!("write a playlist to the file {path}"),
        Command::ImportM3u { path, .. } => format!("read a playlist from the file {path}"),
        Command::AddRoot { path } => format!("add the folder {path} to the library"),
        Command::UndoAction { action } => {
            format!("undo action {}, a change to the library folders", action.0)
        }
        Command::RemoveRoot { root } => {
            format!("remove library folder {} from the library", root.0)
        }
        Command::ReleaseRoot { root } => format!(
            concat!(
                "release library folder {} for removal, stopping playback if it is playing ",
                "or preloaded",
            ),
            root.0
        ),
        Command::ResumeRoot { root } => format!("resume access to library folder {}", root.0),
        other => format!("run the command {}", other.name()),
    }
}
