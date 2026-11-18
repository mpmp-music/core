//! The tools and how each maps onto the contract (docs/design/agent-permissions.md). Field
//! names and meanings are the contract's; nothing here can do what the contract cannot.
//!
//! | tool | contract |
//! |---|---|
//! | `player_status` | query `snapshot` |
//! | `search_tracks` | query `tracks`: a `search` filter, and / or `root` = and a `path` prefix |
//! | `playback` | `resume` / `play` (tracks, playlist), `pause`, `toggle_pause`, `next`, `previous`, `stop`, `seek` |
//! | `queue` | query `queue_page`; `enqueue` (`at`: `end` / `next`); `clear` |
//! | `playlists` | query `playlists`, `playlist_items`; `create_playlist` (manual), `rename_playlist`, `add_to_playlist`, `remove_from_playlist`, `delete_playlist` (to the trash), `export_m3u`, `import_m3u` |
//! | `rate_tracks` | `set_rating`, `set_loved` |
//! | `library_folders` | query `roots`; `add_root`, `remove_root`, `rescan` |
//! | `recent_actions` | query `actions` |
//! | `undo` | `undo_action` |
//! | `wait_for_confirmation` | `ConfirmationResolved` events (see `confirm`) |
//! | `contract_reference` | the commands and queries, and the JSON Schema of one (`reference`) |
//! | `contract_command`, `contract_query` | any command or query, as contract JSON (not the `agents` / `permissions` queries) |
//!
//! Results are JSON text (also as structured content). A failure is a result with
//! `isError: true` whose content is the contract error (`kind`, `code`, `message`,
//! `details`); bad arguments are reported the same way (`invalid_request`, `unknown_field`
//! …, with `details.field`). A command that needs the user's confirmation is not a failure:
//! the result says so and gives the token for `wait_for_confirmation`.

mod search;
use search::search_tracks;
#[cfg(test)]
use search::{is_absolute, search_filter, split_root};
mod playback;
use playback::playback;
mod queue;
use queue::queue;
mod playlist;
use playlist::playlists;
mod ratings;
use ratings::rate_tracks;
mod folders;
use folders::library_folders;
mod history;
use history::{recent_actions, undo};
mod confirmation;
use confirmation::wait_for_confirmation;
mod contract;
#[cfg(test)]
use contract::refused_query;
use contract::{contract_command, contract_query, contract_reference};
use std::sync::Arc;
use std::time::Duration;

use mpmp_core::contract::{
    ActionId, Command, CommandResult, CompareOp, ContractError, ErrorCode, ErrorKind, Field,
    Filter, InsertAt, PlayContext, PlayState, PlaylistId, PlaylistItemId, PlaylistKind,
    PlaylistSummary, Query, QueryResult, Receipt, RootId, Snapshot, TextOp, TrackId, TrackInfo,
    TrackQuery,
};
use rmcp::model::{CallToolResult, JsonObject, Tool, ToolAnnotations};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};

use crate::confirm::Status;
use crate::reference;
use crate::service::Session;

/// The longest a `wait_for_confirmation` call waits.
pub const MAX_WAIT: Duration = Duration::from_secs(50);

/// How long a `wait_for_confirmation` call waits when it does not say: under the shortest
/// default tool timeout among the frameworks (GitHub Copilot's portable format: 30 s; Codex:
/// 60 s), so the framework does not cut the call off.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(25);

const NEEDS_CONFIRMATION: &str =
    "Ask the user to approve this in mpmp, then call wait_for_confirmation with this token.";

// --- definitions ---

/// Every tool, as `tools/list` shows it.
pub(crate) fn definitions() -> Vec<Tool> {
    let offset = json!({"type": "integer", "minimum": 0, "description": "First row (default 0)"});
    let limit = |default: u32| {
        json!({"type": "integer", "minimum": 0, "maximum": 100,
               "description": format!("Most rows (default {default}, max 100)")})
    };
    let ids =
        |what: &str| json!({"type": "array", "items": {"type": "integer"}, "description": what});
    vec![
        tool(
            "player_status",
            concat!(
                "What is playing: state, current track, position, queue summary, volume, ",
                "shuffle/repeat, and any problems with the player's data.",
            ),
            json!({"type": "object", "properties": {}}),
            Kind::ReadOnly,
        ),
        tool(
            "search_tracks",
            concat!(
                "Find tracks by text (title, artist, album, album artist, genre), by ",
                "place, or both. Place: root (a music folder id from library_folders list) ",
                "and folder (a folder in it, as in the tracks' path). With only a place, ",
                "lists the tracks there. Returns tracks with ids, root and path.",
            ),
            json!({"type": "object", "properties": {
                "text": {"type": "string", "description": "Words to search for"},
                "root": {"type": "integer", "description": "Music folder id"},
                "folder": {
                "type": "string",
                 "description": "Folder inside it, /-separated, e.g. \"Jazz/Miles Davis\""}
                ,

                "offset": offset, "limit": limit(20)}}),
            Kind::ReadOnly,
        ),
        tool(
            "playback",
            concat!(
                "Control playback. play: resume, or play the given tracks or playlist ",
                "(replaces the queue). seek: to position_ms in the current track.",
            ),
            json!({"type": "object", "properties": {
                "action": {
                "type": "string",
                 "enum": ["play",
                 "pause",
                 "toggle",
                 "next",
                 "previous",
                 "stop",
                 "seek"]}
                ,

                "position_ms": {"type": "integer", "minimum": 0, "description": "seek: where"},
                "tracks": ids("play: track ids"),
                "playlist": {"type": "integer", "description": "play: playlist id"}},
                "required": ["action"]}),
            Kind::Harmless,
        ),
        tool(
            "queue",
            concat!(
                "The play queue. list: entries in play order; add: tracks at the end, or ",
                "next after the current one; clear: empty it.",
            ),
            json!({"type": "object", "properties": {
                "action": {"type": "string", "enum": ["list", "add", "clear"]},
                "tracks": ids("add: track ids"),
                "at": {
                "type": "string",
                 "enum": ["end",
                 "next"],
                 "description": "add: where (default end)"}
                ,

                "offset": offset, "limit": limit(50)},
                "required": ["action"]}),
            Kind::Harmless,
        ),
        tool(
            "playlists",
            concat!(
                "Playlists. list; items: a playlist's tracks with item ids; create: a ",
                "manual playlist; rename; add_tracks; remove_items (item ids); delete: ",
                "move to the trash (undo brings it back); export_m3u: write to an ",
                ".m3u/.m3u8 file (replaces one that is there); import_m3u: a new playlist ",
                "from one. Export and import need the user's approval.",
            ),
            json!({"type": "object", "properties": {
                "action": {
                "type": "string",
                 "enum": ["list",
                 "items",
                 "create",
                 "rename",
                 "add_tracks",
                 "remove_items",
                 "delete",
                 "export_m3u",
                 "import_m3u"]}
                ,

                "playlist": {"type": "integer", "description": "Playlist id"},
                "name": {
                "type": "string",
                 "description": "create, rename; import_m3u (default: file name)"}
                ,

                "path": {
                "type": "string",
                 "description": "export_m3u, import_m3u: absolute file path, no .. parts"}
                ,

                "tracks": ids("add_tracks: track ids"),
                "items": ids("remove_items: item ids from items"),
                "offset": offset, "limit": limit(50)},
                "required": ["action"]}),
            Kind::Destructive,
        ),
        tool(
            "rate_tracks",
            "Rate tracks 1-5 stars (null clears the rating) and/or mark them loved. Undoable.",
            json!({"type": "object", "properties": {
                "tracks": ids("Track ids"),
                "rating": {"type": ["integer", "null"], "minimum": 1, "maximum": 5},
                "loved": {"type": "boolean"}},
                "required": ["tracks"]}),
            Kind::Harmless,
        ),
        tool(
            "library_folders",
            concat!(
                "Music folders. list (each root id; search_tracks with root lists its ",
                "tracks); add: a folder by absolute path; remove: by root id; rescan all. ",
                "Adding and removing need the user's approval.",
            ),
            json!({"type": "object", "properties": {
                "action": {"type": "string", "enum": ["list", "add", "remove", "rescan"]},
                "path": {"type": "string", "description": "add: absolute folder path, no .. parts"},
                "root": {"type": "integer", "description": "remove: root id from list"}},
                "required": ["action"]}),
            Kind::Destructive,
        ),
        tool(
            "recent_actions",
            concat!(
                "Recent changes to library data (ratings, playlists, folders), newest ",
                "first, with ids for undo.",
            ),
            json!({"type": "object", "properties": {"offset": offset, "limit": limit(20)}}),
            Kind::ReadOnly,
        ),
        tool(
            "undo",
            concat!(
                "Undo a recorded action by id (from recent_actions). Undoing an undo ",
                "redoes it. Undoing a tag edit writes the old tags back into the files, ",
                "and undoing a folder change changes the folders: for those the user is ",
                "asked to approve (unless they allowed it).",
            ),
            json!({"type": "object", "properties": {
                "action": {"type": "integer", "description": "Action id"}},
                "required": ["action"]}),
            // It may rewrite the user's files (a tag edit's undo).
            Kind::Destructive,
        ),
        tool(
            "wait_for_confirmation",
            concat!(
                "Wait for the user's answer to a request that needs confirmation (its ",
                "token). Returns approved (with the result), rejected, expired, withdrawn, ",
                "or pending: then call again. Keep the default timeout_s (25): some ",
                "clients cut a call off at about 30 s. Pass more (up to 50) only if you ",
                "know yours lets calls run longer.",
            ),
            json!({"type": "object", "properties": {
                "token": {"type": "string"},
                "timeout_s": {
                "type": "integer",
                 "minimum": 0,
                 "maximum": 50,
                 "description": "Seconds to wait: default 25, max 50"}
                }
                ,

                "required": ["token"]}),
            Kind::ReadOnly,
        ),
        tool(
            "contract_reference",
            concat!(
                "The player's contract for contract_command and contract_query: without ",
                "name, every command and query with one line each and whether you may use ",
                "it; with name, its JSON Schema (parameters).",
            ),
            json!({"type": "object", "properties": {
            "name": {
            "type": "string",
             "description": "A command or query, e.g. set_shuffle"}
            }
            }
            ),
            Kind::ReadOnly,
        ),
        tool(
            "contract_command",
            concat!(
                "Run any command of the player's contract, as its JSON, e.g. ",
                "{\"type\":\"set_shuffle\",\"shuffle\":true}. Look up names and parameters ",
                "with contract_reference first. Loudness is automatic; there are no manual ",
                "scan/clear/cancel commands. release_root waits for file access to drain ",
                "for safe drive removal; resume_root explicitly restores access. Both use ",
                "local_path permissions (agents confirm by default). Prefer the tools ",
                "above.",
            ),
            json!({"type": "object", "properties": {
                "command": {"type": "object", "description": "Contract command, with its \"type\""},
                "idempotency_key": {
                "type": "string",
                 "description": "Makes a retry safe (on this connection only)"}
                }
                ,

                "required": ["command"]}),
            Kind::Destructive,
        ),
        tool(
            "contract_query",
            concat!(
                "Read contract JSON; look up names/parameters with contract_reference. ",
                "Album: omit offset/limit for all tracks; either enables paging ",
                "(default/cap 1000). Search: kind (tracks/albums/artists), offset/limit ",
                "pages complete results with stable relevance ties; omit kind for the ",
                "mixed first page. Host byte budgets apply. job (job ID) reads full ",
                "reports; jobs (offset/limit) lists summaries. Read-only, all running plus ",
                "up to 1000 finished within 32 MiB, this run only, including automatic ",
                "loudness. Never save IDs across runs; missing jobs may have run. Actions ",
                "include optional structured details; scan errors include optional codes. ",
                "Loudness is automatic, without manual scan/clear/cancel commands. ",
                "release_root drains file access for safe removal; resume_root restores ",
                "it. Both use local_path permissions (agents confirm by default). Not ",
                "available: agents, permissions.",
            ),
            json!({"type": "object", "properties": {
                "query": {"type": "object", "description": "Contract query, with its \"type\""}},
                "required": ["query"]}),
            Kind::ReadOnly,
        ),
    ]
}

/// What a tool may do, for its annotations (hints a framework may use to decide what to
/// ask the user before a call).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Changes nothing.
    ReadOnly,
    /// Changes things, but nothing that cannot be undone or replayed: playback, the queue,
    /// ratings (`destructiveHint: false`; MCP takes a tool that is not read-only as
    /// destructive unless it says otherwise).
    Harmless,
    /// May remove or replace things (playlists, music folders, the user's files by an
    /// undo, any contract command): the default hints.
    Destructive,
}

fn tool(name: &'static str, description: &'static str, schema: Value, kind: Kind) -> Tool {
    let mut schema = match schema {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    schema.insert("additionalProperties".into(), false.into());
    let mut tool = Tool::new(name, description, Arc::new(schema));
    let mut annotations = ToolAnnotations::new()
        .read_only(kind == Kind::ReadOnly)
        .open_world(false);
    if kind != Kind::Destructive {
        annotations = annotations.destructive(false);
    }
    tool.annotations = Some(annotations);
    tool
}

// --- calls ---

/// Runs tool `name`; `None` for a tool that does not exist.
pub(crate) async fn call(
    session: &Session,
    name: &str,
    args: JsonObject,
    stop: impl Future<Output = ()>,
) -> Option<CallToolResult> {
    let result = match name {
        "player_status" => player_status(session, &args).await,
        "search_tracks" => search_tracks(session, &args).await,
        "playback" => playback(session, &args).await,
        "queue" => queue(session, &args).await,
        "playlists" => playlists(session, &args).await,
        "rate_tracks" => rate_tracks(session, &args).await,
        "library_folders" => library_folders(session, &args).await,
        "recent_actions" => recent_actions(session, &args).await,
        "undo" => undo(session, &args).await,
        "wait_for_confirmation" => wait_for_confirmation(session, &args, stop).await,
        "contract_reference" => contract_reference(&args),
        "contract_command" => contract_command(session, &args).await,
        "contract_query" => contract_query(session, &args).await,
        _ => return None,
    };
    Some(result.unwrap_or_else(|error| failure(to_value(&error))))
}

type Outcome = Result<CallToolResult, ContractError>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {}

async fn player_status(session: &Session, args: &JsonObject) -> Outcome {
    let StatusArgs {} = parse(args)?;
    match session.query(Query::Snapshot).await? {
        // Now on core's clock, read after the snapshot: the snapshot is the one last
        // published (core does not publish on every position tick), so its own `now_ms` is
        // when that was.
        QueryResult::Snapshot { snapshot } => Ok(success(status(&snapshot, session.now_ms()))),
        other => Err(unexpected(&other)),
    }
}

// --- results ---

/// The result of a command, as a tool result: the receipt; a `needs_confirmation` with its
/// token (recorded for the session already, `Session::run`); or the contract error.
pub(crate) fn command_result(result: CommandResult) -> CallToolResult {
    match result {
        CommandResult::Ok(receipt) => success(receipt_value(&receipt)),
        CommandResult::Err { error } if error.kind == ErrorKind::NeedsConfirmation => {
            match &error.details.confirmation {
                Some(ticket) => success(json!({
                    "status": "needs_confirmation",
                    "token": ticket.token,
                    "summary": ticket.summary,
                    "expires_at_ms": ticket.expires_at_ms,
                    "message": NEEDS_CONFIRMATION,
                })),
                None => failure(to_value(&error)),
            }
        }
        CommandResult::Err { error } => failure(to_value(&error)),
        other => failure(to_value(&unexpected_value(&other))),
    }
}

fn receipt_value(receipt: &Receipt) -> Value {
    let mut value = to_value(receipt);
    // Agents never get a token (core refuses them the commands that make one); never pass
    // one on all the same.
    if let Some(map) = value.as_object_mut() {
        map.remove("agent_token");
    }
    value
}

fn success(value: Value) -> CallToolResult {
    CallToolResult::structured(value)
}

fn failure(value: Value) -> CallToolResult {
    CallToolResult::structured_error(value)
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// The player's state, short; the position moved on to `now_ms` (core's clock).
fn status(s: &Snapshot, now_ms: u64) -> Value {
    let position = s.position.map(|p| {
        let since = now_ms.saturating_sub(p.at_ms) as f64 * f64::from(p.rate);
        let at = p.position_ms.saturating_add(since.max(0.0) as u64);
        let duration = s.current.as_ref().and_then(|c| c.track.duration_ms);
        duration.map_or(at, |d| at.min(d))
    });
    let state = match s.state {
        PlayState::Playing => "playing",
        PlayState::Paused => "paused",
        PlayState::Stopped => "stopped",
        _ => "unknown",
    };
    json!({
        "state": state,
        "current": s.current.as_ref().map(|c| json!({"entry": c.entry, "track": track(&c.track)})),
        "position_ms": position,
        "queue": {"entry_count": s.queue.entry_count, "current_index": s.queue.current_index,
                  "revision": s.queue.revision},
        "shuffle": s.shuffle,
        "repeat": s.repeat,
        "volume": s.audio.volume,
        "muted": s.audio.muted,
        "problems": s.problems.iter().map(|p| json!({
        "code": p.code,
         "message": p.message}
        )).collect::<Vec<_>>(),

    })
}

/// A track, short: what a model needs to name and choose it.
fn track(t: &TrackInfo) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), t.id.0.into());
    map.insert("title".into(), t.title.clone().into());
    map.insert("root".into(), t.root.0.into());
    map.insert("path".into(), t.path.clone().into());
    let mut put = |key: &str, value: Option<Value>| {
        if let Some(value) = value {
            map.insert(key.into(), value);
        }
    };
    put("artist", t.artist.clone().map(Into::into));
    put("album", t.album.clone().map(Into::into));
    put("year", t.year.map(Into::into));
    put("duration_ms", t.duration_ms.map(Into::into));
    put("rating", t.rating.map(Into::into));
    put("loved", t.loved.then_some(true.into()));
    put("available", (!t.available).then_some(false.into()));
    Value::Object(map)
}

// --- arguments ---

/// The tool's arguments as `T`; serde's complaint as a contract error.
fn parse<T: DeserializeOwned>(args: &JsonObject) -> Result<T, ContractError> {
    serde_json::from_value(Value::Object(args.clone())).map_err(|e| argument_error(&e, args))
}

fn argument_error(e: &serde_json::Error, args: &JsonObject) -> ContractError {
    let message = e.to_string();
    let quoted = |prefix: &str| {
        message
            .strip_prefix(prefix)
            .and_then(|rest| rest.split('`').next())
            .map(str::to_owned)
    };
    if let Some(field) = quoted("unknown field `") {
        ContractError::unsupported(
            ErrorCode::UnknownField,
            format!("this tool does not take {field}"),
        )
        .with_field(field)
    } else if let Some(variant) = quoted("unknown variant `") {
        let error = ContractError::unsupported(
            ErrorCode::UnknownVariant,
            format!("unknown value {variant}: {message}"),
        );
        // serde does not say which field: the argument with this value (`action`, `at`).
        match args
            .iter()
            .find(|(_, value)| value.as_str() == Some(variant.as_str()))
        {
            Some((field, _)) => error.with_field(field.as_str()),
            None => error,
        }
    } else if let Some(field) = quoted("missing field `") {
        ContractError::invalid(ErrorCode::InvalidRequest, format!("{field} is required"))
            .with_field(field)
    } else {
        ContractError::invalid(ErrorCode::InvalidRequest, message)
    }
}

fn action_name(args: &JsonObject) -> Option<&str> {
    args.get("action").and_then(Value::as_str)
}

/// Refuses arguments that do not go with `action` (besides `action`).
fn only(args: &JsonObject, action: Option<&str>, allowed: &[&str]) -> Result<(), ContractError> {
    match args
        .keys()
        .find(|k| *k != "action" && !allowed.contains(&k.as_str()))
    {
        None => Ok(()),
        Some(key) => Err(invalid(
            &match action {
                Some(action) => format!("{key} does not go with action {action}"),
                None => format!("this tool does not take {key}"),
            },
            key,
        )),
    }
}

fn need<T>(value: Option<T>, field: &str, action: &str) -> Result<T, ContractError> {
    value.ok_or_else(|| invalid(&format!("action {action} needs {field}"), field))
}

fn invalid(message: &str, field: &str) -> ContractError {
    ContractError::invalid(ErrorCode::InvalidRequest, message).with_field(field)
}

/// How long a `wait_for_confirmation` waits: `timeout_s` or [`DEFAULT_WAIT`], at most
/// [`MAX_WAIT`] and the server's own cap (`ServerOptions::max_wait`).
fn wait_time(timeout_s: Option<u64>, cap: Duration) -> Duration {
    let asked = timeout_s.map_or(DEFAULT_WAIT, Duration::from_secs);
    asked.min(MAX_WAIT).min(cap)
}

/// A page size: `default` when not given, at most 100.
fn page_limit(limit: Option<u32>, default: u32) -> u32 {
    limit.unwrap_or(default).min(100)
}

fn track_ids(ids: Vec<i64>) -> Vec<TrackId> {
    ids.into_iter().map(TrackId).collect()
}

fn unexpected(result: &QueryResult) -> ContractError {
    match result {
        QueryResult::Err { error } => error.clone(),
        _ => ContractError::failed(ErrorCode::Io, "core answered with another result"),
    }
}

fn unexpected_value(_result: &CommandResult) -> ContractError {
    ContractError::failed(ErrorCode::Io, "core answered with an unknown result")
}

#[cfg(test)]
mod tests {
    use mpmp_core::contract::PositionInfo;

    use super::*;

    #[test]
    fn unknown_command_result_has_a_concrete_error_code() {
        let result = serde_json::to_value(command_result(CommandResult::Unknown)).unwrap();
        assert_eq!(result["isError"], true);
        let error: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(error["code"], "io");
        assert_eq!(error["kind"], "failed");
    }

    /// Tool definitions stay small: each is part of every model's context.
    #[test]
    fn definitions_are_short_and_closed() {
        let tools = definitions();
        assert_eq!(tools.len(), 13);
        for tool in &tools {
            let size = serde_json::to_string(tool).unwrap().len();
            // Roughly 300 tokens at about 4 bytes a token.
            assert!(size < 1300, "{}: {size} bytes", tool.name);
            assert_eq!(tool.input_schema["additionalProperties"], false);
            assert_eq!(tool.input_schema["type"], "object");
        }
    }

    /// The position moves on with core's clock now, not the snapshot's: core does not
    /// publish a snapshot on every position tick.
    #[test]
    fn the_position_moves_on_while_playing() {
        let golden = include_str!("../../core/tests/golden/current/query_result/snapshot.json");
        let QueryResult::Snapshot { mut snapshot } = serde_json::from_str(golden).unwrap() else {
            panic!("not a snapshot")
        };
        let p = snapshot.position.unwrap();
        assert_eq!(snapshot.state, PlayState::Playing);
        assert_eq!(p.rate, 1.0);
        // Published a while ago (its own clock says so too); 1.5 s later on core's clock.
        snapshot.now_ms = p.at_ms;
        let later = p.at_ms + 1_500;
        assert_eq!(
            status(&snapshot, later)["position_ms"],
            p.position_ms + 1_500
        );
        assert_eq!(status(&snapshot, p.at_ms)["position_ms"], p.position_ms);
        // Never past the track's end; not moving while paused.
        let duration = snapshot
            .current
            .as_ref()
            .unwrap()
            .track
            .duration_ms
            .unwrap();
        assert_eq!(
            status(&snapshot, p.at_ms + duration)["position_ms"],
            duration
        );
        snapshot.position = Some(PositionInfo { rate: 0.0, ..p });
        assert_eq!(status(&snapshot, later)["position_ms"], p.position_ms);
    }

    /// 25 s unless asked for more (frameworks with a 30 s tool timeout keep working), at
    /// most 50 s and the server's cap.
    #[test]
    fn waits_default_to_25_seconds() {
        let secs = Duration::from_secs;
        assert_eq!(wait_time(None, MAX_WAIT), secs(25));
        assert_eq!(wait_time(Some(0), MAX_WAIT), secs(0));
        assert_eq!(wait_time(Some(10), MAX_WAIT), secs(10));
        assert_eq!(wait_time(Some(40), MAX_WAIT), secs(40));
        assert_eq!(wait_time(Some(50), MAX_WAIT), secs(50));
        assert_eq!(wait_time(Some(3600), MAX_WAIT), secs(50));
        assert_eq!(wait_time(Some(u64::MAX), MAX_WAIT), secs(50));
        let cap = Duration::from_millis(300);
        assert_eq!(wait_time(None, cap), cap);
        assert_eq!(wait_time(Some(50), cap), cap);
    }

    /// A search by text, by place, or both, with what the contract has (no file system).
    #[test]
    fn searches_by_text_and_place() {
        let root = |id| Filter::Compare {
            field: Field::Root,
            op: CompareOp::Eq,
            value: mpmp_core::contract::Value::Int(id),
        };
        let under = |folder: &str| Filter::Text {
            field: Field::Path,
            op: TextOp::StartsWith,
            text: folder.into(),
        };
        let text = |t: &str| Filter::Search { text: t.into() };
        assert_eq!(
            search_filter(Some("blue".into()), None, None).unwrap(),
            text("blue")
        );
        assert_eq!(search_filter(None, Some(2), None).unwrap(), root(2));
        // A folder is a whole name: "Jazz" is not "Jazzy".
        assert_eq!(
            search_filter(None, Some(2), Some("Jazz//Miles\\")).unwrap(),
            Filter::And {
                all: vec![root(2), under("Jazz/Miles/")]
            }
        );
        assert_eq!(
            search_filter(Some("so".into()), None, Some("Jazz")).unwrap(),
            Filter::And {
                all: vec![text("so"), under("Jazz/")]
            }
        );
        // The whole folder: no condition on the path.
        assert_eq!(search_filter(None, Some(2), Some("/")).unwrap(), root(2));
        // Nothing to look for is refused, not the whole library.
        for (t, f) in [(None, None), (Some(""), Some("")), (Some(" "), Some("/"))] {
            let e = search_filter(t.map(Into::into), None, f).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidRequest);
        }
        // An empty text is none.
        assert_eq!(
            search_filter(Some("".into()), Some(2), None).unwrap(),
            root(2)
        );
    }

    #[test]
    fn absolute_folders_map_to_a_music_folder() {
        assert!(is_absolute("D:\\Music") && is_absolute("d:/x") && is_absolute("/home/me"));
        assert!(is_absolute("\\\\server\\share") && !is_absolute("Jazz/Miles"));
        let unix = [
            (1, "/home/me/Music"),
            (2, "/home/me/Music/Jazz"),
            (3, "/srv"),
        ];
        // The deepest music folder wins.
        assert_eq!(
            split_root(&unix, "/home/me/Music//Jazz/Miles/", false),
            Some((2, "Miles".into()))
        );
        assert_eq!(
            split_root(&unix, "/home/me/Music/Rock", false),
            Some((1, "Rock".into()))
        );
        assert_eq!(split_root(&unix, "/srv", false), Some((3, String::new())));
        // Whole names only; case matters off Windows.
        assert_eq!(split_root(&unix, "/home/me/Musical", false), None);
        assert_eq!(split_root(&unix, "/home/me/music", false), None);
        assert_eq!(split_root(&unix, "/etc", false), None);
        // Windows: no letter case, `\` is `/`.
        let win = [(7, "D:\\Music"), (8, "C:\\Users\\Me\\Music\\")];
        assert_eq!(
            split_root(&win, "d:/music\\Jazz\\Miles", true),
            Some((7, "Jazz/Miles".into()))
        );
        assert_eq!(
            split_root(&win, "c:\\users\\me\\MUSIC", true),
            Some((8, String::new()))
        );
        assert_eq!(split_root(&win, "E:\\Music", true), None);
        assert_eq!(split_root(&win, "D:\\Musicals", true), None);
    }

    #[test]
    fn agents_and_permissions_queries_are_refused() {
        assert!(refused_query(&json!({"type": "agents"})));
        assert!(refused_query(&json!({"type": "permissions", "limit": 5})));
        assert!(refused_query(
            &json!({"type": "permissions", "agent": "me", "limit": 5})
        ));
        assert!(!refused_query(&json!({"type": "roots"})));
        assert!(!refused_query(&json!({"type": "Agents"})));
    }
}
