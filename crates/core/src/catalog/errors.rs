//! Stable library-to-contract error mappings.

use crate::contract::{
    ActionId, ContractError, ErrorCode, ErrorKind, ObjectRef, PlaylistId, RootId, TrackId,
};
use mpmp_library::LibraryError;

/// A library error with its stable code; `kind` from what the code means.
pub(crate) fn library_error(e: LibraryError) -> ContractError {
    use mpmp_library::LibraryError as E;
    if let E::Storage(error) = &e {
        return match error {
            mpmp_storage::StorageError::Unsupported { .. } => {
                ContractError::unsupported(ErrorCode::NotImplemented, error.to_string())
            }
            _ => ContractError::failed(ErrorCode::Io, error.to_string()),
        };
    }
    let code = ErrorCode::parse(e.code());
    debug_assert_ne!(code, ErrorCode::Unknown, "{}", e.code());
    let message = e.to_string();
    match e {
        E::CoverRecoveryChanged => ContractError::new(ErrorKind::Conflict, code, message),
        E::CoverRecoveryNotFound => ContractError::not_found(code, message),
        E::PurgeChanged => ContractError::new(ErrorKind::Conflict, code, message),
        E::PlaylistConflict { expected, actual } => {
            ContractError::conflict(code, message, expected, actual)
        }
        E::ActionConflict { changed, .. } => ContractError::new(ErrorKind::Conflict, code, message)
            .with_changed(
                changed
                    .iter()
                    .map(|target| match *target {
                        mpmp_library::ActionTarget::Track { id } => {
                            ObjectRef::Track { id: TrackId(id.0) }
                        }
                        mpmp_library::ActionTarget::Playlist { id } => ObjectRef::Playlist {
                            id: PlaylistId(id.0),
                        },
                        mpmp_library::ActionTarget::Root { id } => {
                            ObjectRef::Root { id: RootId(id.0) }
                        }
                    })
                    .collect(),
            ),
        E::NotFound { .. } => ContractError::not_found(code, message),
        E::InvalidQuery(_)
        | E::Invalid { .. }
        | E::NotADirectory(_)
        | E::OverlappingRoot { .. }
        | E::NonUnicodePath(_)
        | E::AlreadyExists(_) => ContractError::invalid(code, message),
        // A damaged database stays damaged: retrying will not help.
        E::Db(_) if code == ErrorCode::LibraryDamaged => ContractError::failed(code, message),
        E::Db(_) | E::Io(_) => ContractError::failed(code, message).retryable(),
        // Give the desktop a stable diagnostic for each known file-undo blocker,
        // using the existing file_in_use error envelope.
        E::InUse(_) => {
            ContractError::failed(code, "an undo target is in use by playback").retryable()
        }
        E::ExternalInUse(_) => {
            ContractError::failed(code, "an undo target has an external sharing conflict")
                .retryable()
        }
        _ => ContractError::new(ErrorKind::Failed, code, message),
    }
}

/// An `UndoAction` error: an unknown action and one undone already name it.
pub(super) fn undo_error(e: LibraryError, action: ActionId) -> ContractError {
    let error = library_error(e);
    match error.code {
        ErrorCode::ActionNotFound => error
            .with_missing(vec![ObjectRef::Action { id: action }])
            .with_field("command.action"),
        ErrorCode::AlreadyUndone => error.with_field("command.action"),
        _ => error,
    }
}

/// Whether an error of an M3U export is about its file (its path, the folder it goes to):
/// those name `command.path`.
pub(super) fn is_path_error(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::LocationChanged
            | ErrorCode::NotARegularFile
            | ErrorCode::NotAnM3u
            | ErrorCode::NotADirectory
            | ErrorCode::PathNotUnicode
            | ErrorCode::FileNotFound
            | ErrorCode::AlreadyExists
            | ErrorCode::Io
    )
}

pub(super) fn playlist_not_found(playlist: PlaylistId) -> ContractError {
    ContractError::not_found(
        ErrorCode::PlaylistNotFound,
        format!("playlist {}", playlist.0),
    )
    .with_missing(vec![ObjectRef::Playlist { id: playlist }])
}

pub(super) fn root_not_found(root: RootId) -> ContractError {
    ContractError::not_found(ErrorCode::RootNotFound, format!("library root {}", root.0))
        .with_missing(vec![ObjectRef::Root { id: root }])
}

pub(super) fn album_not_found(key: &str) -> ContractError {
    ContractError::not_found(ErrorCode::AlbumNotFound, format!("album {key:?}")).with_missing(vec![
        ObjectRef::Album {
            key: key.to_owned(),
        },
    ])
}
