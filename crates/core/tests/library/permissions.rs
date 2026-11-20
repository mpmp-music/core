//! Permissions scenarios.
use super::*;

/// Core checks what it cheaply can before asking the user to approve an agent's command,
/// and a missing M3U file is `not_found`, not a retryable I/O error.
#[test]
fn commands_that_cannot_run_are_refused_before_asking() {
    let (dir, core, sub, _audio, _ids) = scanned();
    let agent = Origin::agent("a");
    let live = receipt(execute(
        &core,
        Command::CreatePlaylist {
            name: "Live".into(),
            kind: PlaylistKind::Manual,
        },
        Origin::Ui,
    ))
    .playlist;
    let missing_file = dir.path().join("nope.m3u8").display().to_string();
    for (command, kind, code, field) in [
        (
            Command::RemoveRoot {
                root: RootId(424_242),
            },
            ErrorKind::NotFound,
            ErrorCode::RootNotFound,
            "command.root",
        ),
        (
            Command::ExportM3u {
                playlist: PlaylistId(777),
                path: dir.path().join("x.m3u").display().to_string(),
            },
            ErrorKind::NotFound,
            ErrorCode::PlaylistNotFound,
            "command.playlist",
        ),
        (
            Command::PurgePlaylists {
                playlists: Some(vec![live]),
            },
            ErrorKind::Invalid,
            ErrorCode::NotInTrash,
            "command.playlists[0]",
        ),
        (
            Command::AddRoot {
                path: dir.path().join("no such folder").display().to_string(),
            },
            ErrorKind::Invalid,
            ErrorCode::NotADirectory,
            "command.path",
        ),
        (
            Command::ImportM3u {
                path: missing_file.clone(),
                name: None,
            },
            ErrorKind::NotFound,
            ErrorCode::FileNotFound,
            "command.path",
        ),
    ] {
        let e = error(execute(&core, command.clone(), agent.clone()));
        assert_eq!((e.kind, e.code), (kind, code), "{command:?}: {e:?}");
        assert_eq!(e.details.field.as_deref(), Some(field), "{command:?}");
    }
    assert!(
        !sub.events
            .try_iter()
            .any(|e| matches!(e.event, Event::ConfirmationRequested { .. }))
    );
    // From the UI (no confirmation) the library says the same.
    let e = error(execute(
        &core,
        Command::ImportM3u {
            path: missing_file,
            name: None,
        },
        Origin::Ui,
    ));
    assert_eq!(
        (e.kind, e.code),
        (ErrorKind::NotFound, ErrorCode::FileNotFound)
    );
    assert!(!e.details.retryable);
}

#[test]
fn an_agent_adding_an_existing_folder_is_not_asked_to_confirm() {
    let (dir, core, _sub, _audio, _ids) = scanned();
    // The folder `scanned` added: nothing would change, so nothing to confirm.
    let again = ok(execute(
        &core,
        Command::AddRoot {
            path: dir.path().join("music").display().to_string(),
        },
        Origin::agent("helper"),
    ));
    assert!(!again.changed, "{again:?}");
    assert_eq!((again.action, again.job), (None, None));
}
