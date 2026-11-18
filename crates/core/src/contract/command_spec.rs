//! Command metadata, risk classes, and origin policies.

use super::*;

/// How much harm a command can do (the schema's `x-risk`), by its worst case. It
/// describes the command; what an origin may do is its policy (`x-origins`, guarantee 12).
/// Future API versions may add values; preserve the unknown fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Risk {
    /// Playback control only (transport, shuffle/repeat, volume): no data changes.
    Control,
    /// Changes data that can be changed back, or starts a job: the queue (`Undo`),
    /// settings, user data recorded in the action journal (ratings, loved flags, playlists,
    /// library folders: `UndoAction`), scans.
    Write,
    /// Can lose data for good (purging the trash, overwriting a file). Whether the user
    /// may grant an agent such a command is per command, not per tier: purging library
    /// data never is, writing a file can be (guarantee 12).
    Destructive,
    /// Rewrites the user's files, in a way that is recorded and can be undone (tag
    /// editing). Agents and remotes need the user's confirmation by default.
    FileWrite,
    /// Decode-only fallback for a value this build does not know; never send it.
    #[serde(other)]
    Unknown,
}

/// What core does with a command from an origin by default (the schema's `x-origins`).
/// Not on the wire: the schema has only the names `allow`, `confirm`, `forbidden`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Policy {
    Allow,
    /// Held until the user confirms (`needs_confirmation`).
    Confirm,
    /// Refused: `forbidden` with this code.
    Forbidden(ErrorCode),
}

/// One row of the command table that drives both enforcement and the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// The command's `type`.
    pub name: &'static str,
    pub risk: Risk,
    /// Names a path on core's file system.
    pub local_path: bool,
    /// Only the UI may send it (answers to confirmations, the user's permission settings).
    pub ui_only: bool,
    /// The class a grant can name it by (the schema's `x-class`).
    pub class: OperationClass,
}

const fn spec(name: &'static str, risk: Risk, class: OperationClass) -> CommandSpec {
    CommandSpec {
        name,
        risk,
        local_path: false,
        ui_only: false,
        class,
    }
}

const fn path(name: &'static str, risk: Risk, class: OperationClass) -> CommandSpec {
    CommandSpec {
        local_path: true,
        ..spec(name, risk, class)
    }
}

const fn ui(name: &'static str) -> CommandSpec {
    CommandSpec {
        ui_only: true,
        ..spec(name, Risk::Write, OperationClass::Permissions)
    }
}

use OperationClass::{FileWrite, LibraryEdit, LocalPath, Playback, Purge};

/// Every command, with its risk, restrictions and class. A test checks it against the
/// schema.
///
/// What a command acts on (its targets), for a grant's `scope` (and
/// `PendingConfirmation.scope`):
///
/// | command | targets |
/// |---|---|
/// | `add_root`, `import_m3u`, `export_m3u` | `location` (its `path`, in the platform layer's normal form: an agent's path is normalized, and core acts on that; it is checked again before it is used) |
/// | `remove_root` | `library` (the root), which is also at the root's location |
/// | `undo_action` of a folder change | `library` (every root the action changed), likewise; grants for `add_root` / `remove_root` (what the undo amounts to) apply too |
/// | `edit_tags` | `library` for each track's file (its root and `TrackInfo.path`), which is also at the file's location in the platform layer's normal form (where it really is: a symlink is followed) |
/// | `undo_action` of a tag edit | the same for every file it writes back; grants for `edit_tags` apply too |
/// | everything else (`purge_playlists`, playback, library edits) | nothing in particular: only a grant for `all` covers it |
///
/// The targets of `add_root`, `remove_root` and a folder undo are whole folders; those of
/// `edit_tags` and a tag-edit undo single files.
///
/// A `library` scope contains its root's library targets under its `prefix` (all of them
/// without one; by their place in the root and by where they really are), and the
/// locations inside the root's location (with a `prefix`: inside that place in the root's
/// location; the platform layer decides). An `allow` grant must contain every target of a
/// command; a `forbid` grant applies when it contains any, or when a whole folder target
/// holds the grant's scope (they overlap).
pub const COMMANDS: &[CommandSpec] = &[
    spec("play", Risk::Write, Playback),
    spec("play_entry", Risk::Write, Playback),
    spec("pause", Risk::Control, Playback),
    spec("resume", Risk::Control, Playback),
    spec("toggle_pause", Risk::Control, Playback),
    spec("stop", Risk::Control, Playback),
    spec("next", Risk::Control, Playback),
    spec("previous", Risk::Control, Playback),
    spec("seek", Risk::Control, Playback),
    spec("enqueue", Risk::Write, Playback),
    spec("remove", Risk::Write, Playback),
    spec("move", Risk::Write, Playback),
    spec("clear", Risk::Write, Playback),
    spec("set_shuffle", Risk::Control, Playback),
    spec("set_repeat", Risk::Control, Playback),
    spec("set_stop_after_current", Risk::Control, Playback),
    spec("set_sleep_timer", Risk::Write, Playback),
    spec("set_output_device", Risk::Write, Playback),
    spec("set_output_mode", Risk::Write, Playback),
    spec("undo", Risk::Write, Playback),
    path("add_root", Risk::Write, LocalPath),
    path("remove_root", Risk::Write, LocalPath),
    path("release_root", Risk::Write, LocalPath),
    path("resume_root", Risk::Write, LocalPath),
    spec("rescan", Risk::Write, LibraryEdit),
    spec("set_rating", Risk::Write, LibraryEdit),
    spec("set_loved", Risk::Write, LibraryEdit),
    ui("confirm"),
    ui("reject"),
    spec("set_volume", Risk::Control, Playback),
    spec("set_muted", Risk::Control, Playback),
    spec("set_volume_mode", Risk::Write, Playback),
    spec("set_replay_gain", Risk::Write, Playback),
    spec("create_playlist", Risk::Write, LibraryEdit),
    spec("rename_playlist", Risk::Write, LibraryEdit),
    spec("delete_playlist", Risk::Write, LibraryEdit),
    spec("restore_playlist", Risk::Write, LibraryEdit),
    spec("purge_playlists", Risk::Destructive, Purge),
    spec("add_to_playlist", Risk::Write, LibraryEdit),
    spec("remove_from_playlist", Risk::Write, LibraryEdit),
    spec("move_playlist_item", Risk::Write, LibraryEdit),
    spec("set_smart_query", Risk::Write, LibraryEdit),
    spec("undo_playlist", Risk::Write, LibraryEdit),
    path("import_m3u", Risk::Write, LocalPath),
    path("export_m3u", Risk::Destructive, FileWrite),
    spec("edit_tags", Risk::FileWrite, FileWrite),
    spec("edit_cover", Risk::FileWrite, FileWrite),
    CommandSpec {
        ui_only: true,
        ..spec("resolve_cover_recovery", Risk::Destructive, FileWrite)
    },
    // Undoing a folder change counts as `local_path` (guarantee 12).
    spec("undo_action", Risk::Write, LibraryEdit),
    ui("register_agent"),
    ui("renew_agent_token"),
    ui("remove_agent"),
    ui("grant_permission"),
    ui("revoke_permission"),
];

impl CommandSpec {
    /// What core does by default with this command from `origin` (the user's grants and
    /// core's per-argument rules, such as for `UndoAction`, come on top):
    /// - UI and System may do everything (answering confirmations is UI only);
    /// - media controls: playback control (`risk: control`) only;
    /// - remote: no local paths (`path_not_allowed`); destructive commands need the
    ///   user's confirmation;
    /// - agents: local paths, destructive commands and those that rewrite files
    ///   (`file_write` risk) need the user's confirmation; remotes the latter two.
    ///
    /// So agents and remotes change ratings, loved flags and playlists without asking:
    /// those changes are recorded and can be undone (`UndoAction`, also allowed to them).
    pub fn policy(&self, origin: &Origin) -> Policy {
        if self.ui_only {
            return match origin {
                Origin::Ui if self.name == "resolve_cover_recovery" => Policy::Confirm,
                Origin::Ui => Policy::Allow,
                _ => Policy::Forbidden(ErrorCode::OriginNotAllowed),
            };
        }
        match origin {
            Origin::Ui | Origin::System => Policy::Allow,
            Origin::MediaControls if self.risk == Risk::Control => Policy::Allow,
            Origin::MediaControls => Policy::Forbidden(ErrorCode::OriginNotAllowed),
            Origin::Remote { .. } if self.local_path => {
                Policy::Forbidden(ErrorCode::PathNotAllowed)
            }
            Origin::Remote { .. } | Origin::Agent { .. }
                if self.local_path
                    || matches!(self.risk, Risk::Destructive | Risk::FileWrite)
                    || matches!(self.name, "set_output_device" | "set_output_mode") =>
            {
                Policy::Confirm
            }
            Origin::Remote { .. } | Origin::Agent { .. } => Policy::Allow,
            Origin::Unknown => Policy::Forbidden(ErrorCode::OriginNotAllowed),
        }
    }
}

impl Command {
    /// The command's `type` in JSON.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Play { .. } => "play",
            Command::PlayEntry { .. } => "play_entry",
            Command::Pause => "pause",
            Command::Resume => "resume",
            Command::TogglePause => "toggle_pause",
            Command::Stop => "stop",
            Command::Next => "next",
            Command::Previous => "previous",
            Command::Seek { .. } => "seek",
            Command::Enqueue { .. } => "enqueue",
            Command::Remove { .. } => "remove",
            Command::Move { .. } => "move",
            Command::Clear { .. } => "clear",
            Command::SetShuffle { .. } => "set_shuffle",
            Command::SetRepeat { .. } => "set_repeat",
            Command::SetOutputDevice { .. } => "set_output_device",
            Command::SetOutputMode { .. } => "set_output_mode",
            Command::SetStopAfterCurrent { .. } => "set_stop_after_current",
            Command::SetSleepTimer { .. } => "set_sleep_timer",
            Command::Undo { .. } => "undo",
            Command::AddRoot { .. } => "add_root",
            Command::RemoveRoot { .. } => "remove_root",
            Command::ReleaseRoot { .. } => "release_root",
            Command::ResumeRoot { .. } => "resume_root",
            Command::Rescan => "rescan",
            Command::SetRating { .. } => "set_rating",
            Command::SetLoved { .. } => "set_loved",
            Command::Confirm { .. } => "confirm",
            Command::Reject { .. } => "reject",
            Command::SetVolume { .. } => "set_volume",
            Command::SetMuted { .. } => "set_muted",
            Command::SetVolumeMode { .. } => "set_volume_mode",
            Command::SetReplayGain { .. } => "set_replay_gain",
            Command::CreatePlaylist { .. } => "create_playlist",
            Command::RenamePlaylist { .. } => "rename_playlist",
            Command::DeletePlaylist { .. } => "delete_playlist",
            Command::RestorePlaylist { .. } => "restore_playlist",
            Command::PurgePlaylists { .. } => "purge_playlists",
            Command::AddToPlaylist { .. } => "add_to_playlist",
            Command::RemoveFromPlaylist { .. } => "remove_from_playlist",
            Command::MovePlaylistItem { .. } => "move_playlist_item",
            Command::SetSmartQuery { .. } => "set_smart_query",
            Command::UndoPlaylist { .. } => "undo_playlist",
            Command::ImportM3u { .. } => "import_m3u",
            Command::ExportM3u { .. } => "export_m3u",
            Command::UndoAction { .. } => "undo_action",
            Command::RegisterAgent { .. } => "register_agent",
            Command::RenewAgentToken { .. } => "renew_agent_token",
            Command::RemoveAgent { .. } => "remove_agent",
            Command::GrantPermission { .. } => "grant_permission",
            Command::RevokePermission { .. } => "revoke_permission",
            Command::EditTags { .. } => "edit_tags",
            Command::EditCover { .. } => "edit_cover",
            Command::ResolveCoverRecovery { .. } => "resolve_cover_recovery",
            Command::Unknown => "unknown",
        }
    }

    /// The command's row of [`COMMANDS`]; `None` for `Unknown`.
    pub fn spec(&self) -> Option<&'static CommandSpec> {
        let name = self.name();
        COMMANDS.iter().find(|s| s.name == name)
    }

    pub fn risk(&self) -> Risk {
        self.spec().map_or(Risk::Unknown, |s| s.risk)
    }

    /// The command's class in [`COMMANDS`] (`undo_action` of a folder change counts as
    /// `local_path` when core runs it, of a tag edit as `file_write`).
    pub fn class(&self) -> OperationClass {
        self.spec().map_or(OperationClass::Unknown, |s| s.class)
    }

    /// What core does with this command from `origin` (see [`CommandSpec::policy`]).
    pub fn policy(&self, origin: &Origin) -> Policy {
        match self.spec() {
            Some(spec) => spec.policy(origin),
            None => Policy::Forbidden(ErrorCode::UnknownCommand),
        }
    }
}
