//! What core needs from the library: track data, file locations,
//! play contexts, read-only library queries, and history writes. `mpmp_library::Library`
//! implements it; tests use an in-memory catalog. Not part of the frozen contract.
//!
//! Errors are contract errors, with the library's stable codes.

use std::sync::Arc;

use mpmp_media_io::MediaUri;

use crate::contract::{
    ActionId, Command, ContractError, EndReason, ErrorCode, Grant, GrantId, JobOutcome, Lyrics,
    ObjectRef, Origin, PlayContext, PlaylistId, PlaylistReceipt, Query, QueryResult, RootId,
    TagChange, TrackId, TrackInfo, Waveform,
};
use crate::permissions::{StoredAgent, StoredPermissions};

mod convert;
#[cfg(feature = "desktop-fs")]
mod desktop;
mod errors;
mod library;
mod playlists;
mod query;

pub(crate) use convert::{cover_change, placeholder};
#[cfg(test)]
pub(crate) use errors::library_error;

/// Reads tracks, resolves playback contexts and runs library queries.
pub trait Browse: Send + Sync + 'static {
    /// Track data for `ids`, in the same order (repeats repeated); unknown ids are left
    /// out.
    fn tracks(&self, ids: &[TrackId]) -> Result<Vec<TrackInfo>, ContractError>;

    fn uri_of(&self, id: TrackId) -> Result<Option<MediaUri>, ContractError>;

    /// The part of its file a track plays, in frames at the file's sample rate: set for a
    /// track of a CUE sheet (`TrackInfo.cue`), `None` for a whole file (the default, for
    /// catalogs without sheets). Playback and waveforms use it.
    fn frame_range(&self, _id: TrackId) -> Result<Option<mpmp_engine::FrameRange>, ContractError> {
        Ok(None)
    }

    /// The tracks a play context stands for, in play order. Core checks the ids of a
    /// `Tracks` context itself; an unknown album or playlist is `not_found`.
    fn context_tracks(&self, context: &PlayContext) -> Result<Vec<TrackId>, ContractError>;

    /// Library queries (search, albums, artists, history, roots, tracks, playlists). Runs
    /// on the caller's thread against read connections ; never called for queue or
    /// state queries. Explicit page limits are already capped; an unpaged Album is complete.
    fn query(&self, query: &Query) -> QueryResult;
}

/// Resolves guarded playlist playback and applies playlist commands.
pub trait Playlists: Browse {
    /// Resolve a playlist and its optional starting item from one catalog snapshot.
    /// Catalogs implementing playlists must override this for guarded playback.
    fn playlist_playback(
        &self,
        context: &PlayContext,
    ) -> Result<(Vec<TrackId>, Option<usize>), ContractError> {
        if matches!(
            context,
            PlayContext::Playlist {
                start_item: Some(_),
                ..
            } | PlayContext::Playlist {
                expected_revision: Some(_),
                ..
            }
        ) {
            return Err(not_supported("guarded playlist playback"));
        }
        self.context_tracks(context).map(|tracks| (tracks, None))
    }

    /// Runs a playlist command (`CreatePlaylist` … `ExportM3u`) as `origin`; core has
    /// already checked who may run it and that the tracks exist. What cannot be undone
    /// (`PurgePlaylists`, an `ExportM3u` that replaces a file, or that an agent ran) is
    /// recorded as an audit entry, the outcome's `audit`.
    fn playlist_command(
        &self,
        _command: &Command,
        _origin: &Origin,
        _grant: Option<GrantId>,
    ) -> Result<PlaylistOutcome, ContractError> {
        Err(not_supported("playlists"))
    }

    /// Trusted permanent-deletion binding; unknown catalogs fail closed.
    fn describe_purge(
        &self,
        _ids: Option<&[crate::contract::PlaylistId]>,
    ) -> Result<mpmp_library::PurgeGuard, ContractError> {
        Err(not_supported("permanent deletion binding"))
    }

    fn purge_guarded(
        &self,
        _guard: mpmp_library::PurgeGuard,
        _origin: &Origin,
    ) -> Result<PlaylistOutcome, ContractError> {
        Err(not_supported("guarded permanent deletion"))
    }
}

/// A catalog reported incomplete shutdown. SQLite catalogs also supply individual stages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogCloseError {
    pub message: String,
    pub progress: Option<Arc<mpmp_library::CloseProgress>>,
}

impl std::fmt::Display for CatalogCloseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CatalogCloseError {}

/// Manages library roots, file access barriers and scan scheduling.
pub trait LibraryRoots: Send + Sync + 'static {
    /// Close catalog storage after core finishes its jobs and saves state.
    fn close(&self) -> Result<(), CatalogCloseError> {
        Ok(())
    }

    // Changes to user data are recorded in the catalog's action journal as made by
    // `origin` (contract guarantee 15); each returns the action (`None`: nothing changed).
    // `grant`: the user's permission grant that let an agent run the command without
    // asking, recorded with the action.
    /// Adds a library folder: its id (the existing one if it already is a root, then with
    /// no action).
    fn add_root(
        &self,
        _path: &str,
        _origin: &Origin,
        _grant: Option<GrantId>,
    ) -> Result<Recorded<RootId>, ContractError> {
        Err(not_supported("library roots"))
    }

    fn remove_root(
        &self,
        _root: RootId,
        _origin: &Origin,
        _grant: Option<GrantId>,
    ) -> Result<Recorded<()>, ContractError> {
        Err(not_supported("library roots"))
    }

    /// Session-local suspension of physical access to one root. Success is a barrier:
    /// no current scan, tag, cover, lyric or analysis operation still holds its files.
    fn check_root_access(&self, _root: RootId) -> Result<(), ContractError> {
        Err(not_supported("root release"))
    }

    /// Mark suspension on the actor before stopping transport. Must not wait for leases.
    fn suspend_root(&self, _root: RootId) -> Result<bool, ContractError> {
        Err(not_supported("root release"))
    }

    /// Persist a non-UI root access request before changing suspension. Retries
    /// are audited too, since an unsuccessful barrier can still leave it suspended.
    fn audit_root_access(
        &self,
        _root: RootId,
        _released: bool,
        _origin: &Origin,
        _grant: Option<GrantId>,
    ) -> Result<ActionId, ContractError> {
        Err(not_supported("root access audit"))
    }

    /// Background barrier; false means timeout. Suspension remains in effect.
    fn wait_root_idle(
        &self,
        _root: RootId,
        _timeout: std::time::Duration,
    ) -> Result<bool, ContractError> {
        Ok(false)
    }

    /// Acquire before resolving/opening playback media, retain through decoder destruction.
    fn playback_lease(&self, _uri: &MediaUri) -> Result<Box<dyn Send>, ContractError> {
        Ok(Box::new(()))
    }

    fn resume_root(&self, _root: RootId) -> Result<bool, ContractError> {
        Err(not_supported("root release"))
    }

    /// An owned lease held across an analysis decode. Released roots refuse acquisition.
    fn file_lease(&self, _track: TrackId) -> Result<Box<dyn Send>, ContractError> {
        Ok(Box::new(()))
    }

    /// A library folder's location (`RootInfo.path`), also of a removed folder; `None`
    /// when unknown.
    fn root_location(&self, _root: RootId) -> Result<Option<String>, ContractError> {
        Ok(None)
    }

    /// True when the files should be re-read (after a library upgrade); core then
    /// starts a scan on its own.
    fn rescan_recommended(&self) -> Result<bool, ContractError> {
        Ok(false)
    }

    /// Pending artist parser work, independently of the legacy rescan request flag.
    fn artist_backfill_pending(&self) -> Result<bool, ContractError> {
        Ok(false)
    }

    /// Internal host/watcher scan scope; no new public command or permissions.
    fn rescan_roots(
        &self,
        roots: Option<Vec<RootId>>,
        events: Box<dyn Fn(ScanEvent) + Send + Sync>,
    ) -> Result<ScanTicket, ContractError> {
        if roots.is_some() {
            return Err(not_supported("scoped scanning"));
        }
        self.rescan(events)
    }

    /// Trusted repair scan. Directories are absolute paths beneath active roots.
    fn rescan_directories(
        &self,
        _directories: Vec<std::path::PathBuf>,
        _events: Box<dyn Fn(ScanEvent) + Send + Sync>,
    ) -> Result<ScanTicket, ContractError> {
        Err(not_supported("directory scanning"))
    }

    /// Starts an incremental scan, or joins the running one. `events` is called from
    /// other threads (never from inside this call's caller's lock): progress, and exactly
    /// one `Finished` per scan id. A joined scan reports through the callback of the call
    /// that started it.
    fn rescan(
        &self,
        _events: Box<dyn Fn(ScanEvent) + Send + Sync>,
    ) -> Result<ScanTicket, ContractError> {
        Err(not_supported("scanning"))
    }
}

/// Records user data changes and describes or applies journal undo operations.
pub trait Actions: Send + Sync + 'static {
    /// User data, stored in the library only (not in the files). Ratings are 1–5. Returns
    /// the tracks that changed (unknown ids and tracks that already have the value are
    /// left out).
    fn set_rating(
        &self,
        _tracks: &[TrackId],
        _rating: Option<u8>,
        _origin: &Origin,
    ) -> Result<Recorded<Vec<TrackId>>, ContractError> {
        Err(not_supported("ratings"))
    }

    fn set_loved(
        &self,
        _tracks: &[TrackId],
        _loved: bool,
        _origin: &Origin,
    ) -> Result<Recorded<Vec<TrackId>>, ContractError> {
        Err(not_supported("loved tracks"))
    }

    /// Undoes a recorded action (`UndoAction`).
    fn undo_action(
        &self,
        _action: ActionId,
        _origin: &Origin,
        _grant: Option<GrantId>,
    ) -> Result<Undone, ContractError> {
        Err(not_supported("actions"))
    }

    /// A file undo's read-only before/after and resolved locations.
    fn preview_undo(&self, _action: ActionId) -> Result<QueryResult, ContractError> {
        Err(not_supported("undo preview"))
    }

    /// Full effective inverse for trusted host routing; unknown catalogs fail closed.
    fn describe_undo(
        &self,
        _action: ActionId,
    ) -> Result<mpmp_library::UndoDescription, ContractError> {
        Err(not_supported("undo description"))
    }

    fn undo_action_guarded(
        &self,
        _expected: mpmp_library::UndoDescription,
        _origin: &Origin,
        _grant: Option<GrantId>,
    ) -> Result<Undone, ContractError> {
        Err(not_supported("guarded undo"))
    }

    /// What undoing `action` amounts to when it adds or removes a music folder (the action
    /// changed folders: a folder change, or an undo or redo of one, which records the same
    /// folders): `"add_root"` when it brings a removed folder back, `"remove_root"` when
    /// it removes one in use (judged by the folders' state now, not by older actions,
    /// which may have been pruned). Such an undo follows the local-path rules, and the
    /// grants for that command apply to it too. An error when that cannot be told (the
    /// command then fails: it is never taken for a plain data edit).
    fn undo_root_command(&self, _action: ActionId) -> Result<Option<&'static str>, ContractError> {
        Ok(None)
    }

    /// The library folders an action changed (a folder change, or an undo of one).
    fn action_roots(&self, _action: ActionId) -> Result<Vec<RootId>, ContractError> {
        Ok(Vec::new())
    }
}

/// Reports storage capabilities and health and checks commands before execution.
pub trait CatalogChecks: Send + Sync + 'static {
    /// Storage writes are opt-in for catalogs. Desktop Library forwards its backend's capabilities.
    fn storage_write_capabilities(&self) -> mpmp_storage::WriteCapabilities {
        mpmp_storage::WriteCapabilities {
            durable_replace: false,
            legacy_tag_write: false,
        }
    }

    /// Cheap checks of a command core is about to hold for the user's confirmation, so
    /// the user is never asked to approve one that cannot run (unknown ids, a path that
    /// is not a folder or an M3U file …). `Ok` when nothing is known to be wrong.
    /// True when `command` is known to change nothing (e.g. `AddRoot` of a folder that
    /// already is one): core then runs it without asking for confirmation.
    fn is_noop(&self, _command: &Command) -> bool {
        false
    }

    fn precheck(&self, _command: &Command) -> Result<(), ContractError> {
        Ok(())
    }

    /// Why the catalog's database is damaged, if any of its calls so far found it so (on
    /// any thread: also writes whose failures are only logged, scans, queries). Core asks
    /// after every message and on every tick, and reports it once as `library_damaged` in
    /// `Snapshot.problems`. Must be cheap.
    fn damaged(&self) -> Option<String> {
        None
    }
}

/// Persists registered agents, authentication records and lasting permission grants.
pub trait AgentPermissions: Send + Sync + 'static {
    // Agent permissions. A catalog that does not store them (the defaults) leaves core to
    // keep them in memory only: they are lost when core stops.
    /// Registered agents and lasting grants, read once when core starts. An error leaves
    /// agents refused until core restarts (`permissions_unavailable`); a record that cannot
    /// be read is returned as well as it can be (an agent whose token hash is unreadable
    /// has none; a grant of a shape this build does not know never applies).
    fn load_permissions(&self) -> Result<StoredPermissions, ContractError> {
        Ok(StoredPermissions::default())
    }

    fn add_agent(&self, _agent: &StoredAgent) -> Result<(), ContractError> {
        Ok(())
    }

    fn set_agent_token(&self, _name: &str, _token_hash: &[u8; 32]) -> Result<(), ContractError> {
        Ok(())
    }

    fn set_agent_seen(&self, _name: &str, _at_ms: i64) -> Result<(), ContractError> {
        Ok(())
    }

    /// Removes an agent with all its stored grants.
    fn remove_agent(&self, _name: &str) -> Result<(), ContractError> {
        Ok(())
    }

    /// Stores a lasting grant (not one for a session).
    fn add_grant(&self, _grant: &Grant) -> Result<(), ContractError> {
        Ok(())
    }

    fn remove_grants(&self, _ids: &[GrantId]) -> Result<(), ContractError> {
        Ok(())
    }

    /// A grant id never handed out before, also across restarts; `None`: core counts from
    /// the clock itself.
    fn next_grant_id(&self) -> Result<Option<GrantId>, ContractError> {
        Ok(None)
    }
}

/// Reads covers and lyrics and manages the derived waveform cache.
pub trait MediaResources: Browse {
    /// Cover image for a track (embedded, or a cover file next to it), read on the
    /// caller's thread. Binary, so it is not part of the JSON contract; adapters serve it
    /// their own way (e.g. a custom URL scheme in the desktop app).
    fn cover_art(&self, _track: TrackId) -> Result<Option<CoverArt>, ContractError> {
        Ok(None)
    }

    /// [`Catalog::cover_art`](MediaResources::cover_art) shrunk to fit `max_edge` pixels on its longest edge (rounded
    /// up to 128, 256, 512 or 1024; JPEG, or PNG for an image with transparency), made on
    /// demand on the caller's thread and cached on disk. The original when it is no larger
    /// than that, `max_edge` is above 1024, or it cannot be decoded. Binary, so not part of
    /// the JSON contract.
    fn cover_thumbnail(
        &self,
        _track: TrackId,
        _max_edge: u32,
    ) -> Result<Option<CoverArt>, ContractError> {
        Ok(None)
    }

    /// Lyrics of a track (a `.lrc` file next to it, else its tags), read on the caller's
    /// thread like [`Catalog::cover_art`](MediaResources::cover_art). `Ok(None)`: the track has none; an unknown
    /// track is `not_found` / `track_not_found`. The default knows no lyrics, for tracks
    /// that exist.
    fn lyrics(&self, track: TrackId) -> Result<Option<Lyrics>, ContractError> {
        if self.tracks(&[track])?.is_empty() {
            return Err(track_not_found(track));
        }
        Ok(None)
    }

    /// The cached waveform of a track, read on the caller's thread; `Ok(None)` when none is
    /// stored for the file as it is now. An unknown track is `not_found` /
    /// `track_not_found`. The default stores none (for tracks that exist).
    fn waveform(&self, track: TrackId) -> Result<Option<Waveform>, ContractError> {
        if self.tracks(&[track])?.is_empty() {
            return Err(track_not_found(track));
        }
        Ok(None)
    }

    /// The version of a track's file as last scanned (what a waveform is kept for), read
    /// before its waveform is computed. `None`: unknown track, or the catalog keeps no
    /// waveforms (the default; core then computes none).
    fn file_version(&self, _track: TrackId) -> Result<Option<FileVersion>, ContractError> {
        Ok(None)
    }

    /// Keeps a waveform core computed from the track's file at version `from`, replacing
    /// an older one; called from core's waveform thread. `Ok(false)`: not kept, the file
    /// was scanned as another version meanwhile (core computes it again). The default
    /// keeps none (`not_implemented`): core then computes each track at most once a run.
    fn store_waveform(
        &self,
        _track: TrackId,
        _from: FileVersion,
        _waveform: &Waveform,
    ) -> Result<bool, ContractError> {
        Err(not_supported("waveforms"))
    }

    /// Whether missing waveform data may be queued without touching a suspended root.
    fn waveform_accessible(&self, _track: TrackId) -> bool {
        true
    }
}

/// Reads and updates derived track and album loudness caches.
pub trait Loudness: Browse {
    /// Derived loudness cache. Defaults keep catalogs without this facility usable.
    fn supports_loudness_scan(&self) -> bool {
        false
    }

    fn loudness_source(
        &self,
        _track: TrackId,
    ) -> Result<Option<mpmp_library::LoudnessSource>, ContractError> {
        Err(not_supported("loudness scans"))
    }

    fn loudness(
        &self,
        track: TrackId,
    ) -> Result<Option<crate::contract::LoudnessInfo>, ContractError> {
        if self.tracks(&[track])?.is_empty() {
            return Err(track_not_found(track));
        }
        Ok(None)
    }

    /// A page of the tracks an automatic pass looks at (after track id `after`), with what
    /// the cache holds for each.
    fn automatic_loudness_tracks(
        &self,
        _after: i64,
    ) -> Result<Vec<mpmp_library::AutomaticLoudnessTrack>, ContractError> {
        Ok(vec![])
    }

    fn loudness_statistics(&self, _track: TrackId) -> Result<Option<Vec<u8>>, ContractError> {
        Ok(None)
    }

    fn pending_album_loudness(
        &self,
        _album: &mpmp_library::AlbumKey,
    ) -> Result<mpmp_library::AlbumLoudnessStatistics, ContractError> {
        Ok(vec![])
    }

    fn store_automatic_album_loudness(
        &self,
        _album: mpmp_library::AlbumLoudnessUpdate,
    ) -> Result<bool, ContractError> {
        Ok(false)
    }

    fn store_automatic_loudness(
        &self,
        _tracks: Vec<mpmp_library::LoudnessUpdate>,
    ) -> Result<mpmp_library::LoudnessCommit, ContractError> {
        Err(not_supported("automatic loudness"))
    }
}

/// Starts tag and cover edits and binds their undo and recovery operations.
pub trait TagsAndCovers: Send + Sync + 'static {
    /// Edits the tags of `tracks`' files (`EditTags`, docs/design/media-resources.md
    ///) as `origin` on threads of its own, and returns at once; core has checked who may
    /// run it and that the tracks exist. The changes are checked here (`invalid_tag`,
    /// naming `command.changes[i]…`) before anything is read. `events` is called from
    /// another thread: progress, then exactly one `Finished` (whose `tracks` were written
    /// and read again, visible to queries by then; the report names the action recorded).
    fn describe_cover_recovery(
        &self,
        _track: TrackId,
        _resolution: mpmp_library::CoverRecoveryResolution,
        _selection: Option<mpmp_library::CoverRecoverySelection>,
    ) -> Result<mpmp_library::CoverRecoveryGuard, ContractError> {
        Err(not_supported("cover recovery resolution"))
    }

    fn accept_cover_recovery(
        &self,
        _guard: &mpmp_library::CoverRecoveryGuard,
        _origin: &Origin,
    ) -> Result<(ActionId, String), ContractError> {
        Err(not_supported("cover recovery resolution"))
    }

    fn edit_cover(
        &self,
        _tracks: &[TrackId],
        _change: &crate::contract::CoverChange,
        _files: TagFiles,
        _origin: &Origin,
        _grant: Option<GrantId>,
        _events: Box<dyn Fn(TagJobEvent) + Send + Sync>,
    ) -> Result<(), ContractError> {
        Err(not_supported("cover editing"))
    }

    fn undo_cover_edit(
        &self,
        _action: ActionId,
        _files: TagFiles,
        _origin: &Origin,
        _grant: Option<GrantId>,
        _events: Box<dyn Fn(TagJobEvent) + Send + Sync>,
    ) -> Result<bool, ContractError> {
        Ok(false)
    }

    fn cover_undo_tracks(&self, _action: ActionId) -> Result<Vec<TrackId>, ContractError> {
        Ok(vec![])
    }

    fn take_recovered_covers(&self) -> Vec<TrackId> {
        vec![]
    }

    fn cover_recovery_required(&self) -> bool {
        false
    }

    fn cover_sources(
        &self,
        _ids: &[TrackId],
    ) -> Result<Vec<mpmp_library::CoverSource>, ContractError> {
        Err(not_supported("cover confirmation binding"))
    }

    fn edit_tags(
        &self,
        _tracks: &[TrackId],
        _changes: &[TagChange],
        _files: TagFiles,
        _origin: &Origin,
        _grant: Option<GrantId>,
        _events: Box<dyn Fn(TagJobEvent) + Send + Sync>,
    ) -> Result<(), ContractError> {
        Err(not_supported("tag edits"))
    }

    /// Undoes a tag edit (or an undo of one) by writing the old values back, as
    /// [`Catalog::edit_tags`](TagsAndCovers::edit_tags) does; `Ok(false)`: `action` does not write files back (it is
    /// undone by [`Catalog::undo_action`](Actions::undo_action)). What the journal tells is checked first (an
    /// error here: unknown, undone, a later tag edit of the files); what the files tell by
    /// the job, before it writes any (its outcome `failed`: `action_conflict`,
    /// `file_in_use`).
    fn undo_tag_edit(
        &self,
        _action: ActionId,
        _files: TagFiles,
        _origin: &Origin,
        _grant: Option<GrantId>,
        _events: Box<dyn Fn(TagJobEvent) + Send + Sync>,
    ) -> Result<bool, ContractError> {
        Ok(false)
    }

    /// The tracks whose files undoing `action` writes: a tag edit's, or an undo's of one;
    /// empty for any other action. Such an undo follows the file-write rules, and grants
    /// for `edit_tags` apply to it too.
    fn tag_undo_tracks(&self, _action: ActionId) -> Result<Vec<TrackId>, ContractError> {
        Ok(Vec::new())
    }
}

/// Records finished plays in the playback history.
pub trait PlaybackHistory: Send + Sync + 'static {
    fn record_play(&self, record: PlayRecord);
}

/// The complete library interface used by core.
pub trait Catalog:
    Browse
    + Playlists
    + LibraryRoots
    + Actions
    + CatalogChecks
    + AgentPermissions
    + MediaResources
    + Loudness
    + TagsAndCovers
    + PlaybackHistory
{
}

impl<
    T: Browse
        + Playlists
        + LibraryRoots
        + Actions
        + CatalogChecks
        + AgentPermissions
        + MediaResources
        + Loudness
        + TagsAndCovers
        + PlaybackHistory
        + ?Sized,
> Catalog for T
{
}

/// All `tracks` exist (empty: yes); else `not_found` / `track_not_found` listing those that
/// do not, `details.field` naming the first one (`field[i]`).
pub(crate) fn check_tracks_exist<C: Catalog + ?Sized>(
    catalog: &C,
    tracks: &[TrackId],
    field: &str,
) -> Result<(), ContractError> {
    if tracks.is_empty() {
        return Ok(());
    }
    let known: std::collections::HashSet<_> =
        catalog.tracks(tracks)?.into_iter().map(|t| t.id).collect();
    let Some(first) = tracks.iter().position(|id| !known.contains(id)) else {
        return Ok(());
    };
    let mut missing: Vec<TrackId> = Vec::new();
    for id in tracks {
        if !known.contains(id) && !missing.contains(id) {
            missing.push(*id);
        }
    }
    Err(ContractError::not_found(
        ErrorCode::TrackNotFound,
        format!(
            "unknown tracks: {}",
            missing
                .iter()
                .map(|t| t.0.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
    .with_missing(
        missing
            .into_iter()
            .map(|id| ObjectRef::Track { id })
            .collect(),
    )
    // The first unknown one.
    .with_field(format!("{field}[{first}]")))
}

/// An unknown track, for a query about one.
pub(crate) fn track_not_found(track: TrackId) -> ContractError {
    ContractError::not_found(ErrorCode::TrackNotFound, format!("track {}", track.0))
        .with_missing(vec![ObjectRef::Track { id: track }])
}

fn not_supported(what: &str) -> ContractError {
    ContractError::unsupported(
        ErrorCode::NotImplemented,
        format!("{what} are not supported by this catalog"),
    )
}

/// A change to user data and the action that recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded<T> {
    pub value: T,
    /// `None`: nothing changed (nothing was recorded).
    pub action: Option<ActionId>,
}

impl<T> Recorded<T> {
    pub fn new(value: T, action: Option<ActionId>) -> Self {
        Self { value, action }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Recorded<U> {
        Recorded {
            value: f(self.value),
            action: self.action,
        }
    }
}

/// What a playlist command did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistOutcome {
    /// `None`: no single playlist to report (purge).
    pub receipt: Option<PlaylistReceipt>,
    /// The playlists to announce in `PlaylistsChanged` (empty: unknown which).
    pub playlists: Vec<PlaylistId>,
    /// `false`: nothing changed (no event is emitted).
    pub changed: bool,
    /// The action it recorded (`None` for commands that are not recorded, and when
    /// nothing changed): what the receipt names, and can be undone.
    pub action: Option<ActionId>,
    /// The audit entry it recorded, for what cannot be undone (listed by `Actions`, never
    /// named by the receipt).
    pub audit: Option<ActionId>,
}

/// What an undo changed, for the receipt and the change events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undone {
    /// The undo's own action.
    pub action: ActionId,
    /// Tracks whose rating or loved flag changed.
    pub tracks: Vec<TrackId>,
    /// Playlists that changed (contents, or moved into or out of the trash).
    pub playlists: Vec<PlaylistId>,
    /// Library folders that were removed or brought back.
    pub roots: Vec<RootId>,
    /// A folder came back: its tracks return with the next scan, which core starts.
    pub rescan: bool,
}

/// What a tag job needs to know of the files besides its changes.
#[derive(Clone)]
pub struct TagFiles {
    pub cover_sources: Option<Vec<mpmp_library::CoverSource>>,
    pub undo_guard: Option<mpmp_library::UndoDescription>,
    /// For an agent: each track's file in the platform layer's normal form, as core
    /// checked it (and showed it to the user); the catalog checks once more right before it
    /// writes one that it is still there (`location_changed`), and writes no other.
    /// `None`: nothing was checked (the UI's own command).
    pub checked: Option<Vec<(TrackId, String)>>,
    /// Whether the player has a track's file open now (the track being heard, or the one
    /// prepared to follow it): such a file is not written (`file_in_use`).
    pub in_use: Arc<dyn Fn(TrackId) -> bool + Send + Sync>,
}

/// What a tag job reports ([`Catalog::edit_tags`](TagsAndCovers::edit_tags)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagJobEvent {
    Progress {
        done: u32,
        total: u32,
    },
    Finished {
        /// `Completed` with `JobReport::TagEdit`.
        outcome: JobOutcome,
        /// Tracks whose files were written.
        tracks: Vec<TrackId>,
    },
}

/// A scan the catalog started or joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanTicket {
    /// The catalog's id of the scan (core maps it to a job).
    pub scan: u64,
    /// The scan was already running.
    pub joined: bool,
}

/// Rust-only completion facts; the public JobOutcome keeps its bounded sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanResult {
    pub outcome: JobOutcome,
    pub diagnostics: Option<Arc<mpmp_library::ScanDiagnostics>>,
}

impl From<JobOutcome> for ScanResult {
    fn from(outcome: JobOutcome) -> Self {
        Self {
            outcome,
            diagnostics: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanEvent {
    Diagnostics {
        scan: u64,
        diagnostics: Arc<mpmp_library::ScanDiagnostics>,
    },
    Progress {
        scan: u64,
        done: u32,
        total: Option<u32>,
    },
    Finished {
        scan: u64,
        outcome: JobOutcome,
    },
}

/// One finished play, recorded raw.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayRecord {
    pub track: TrackId,
    pub context: Option<PlayContext>,
    /// Unix time, milliseconds.
    pub started_at_ms: i64,
    pub played_ms: u64,
    pub ended: EndReason,
    pub origin: Option<Origin>,
}

/// One version of a track's file, as the catalog last scanned it (its size and
/// modification time): what a stored waveform belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileVersion {
    pub size: u64,
    pub mtime_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverArt {
    pub mime: String,
    pub data: Vec<u8>,
}
