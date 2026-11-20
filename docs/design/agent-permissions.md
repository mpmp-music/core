[中文](../zh-CN/design/agent-permissions.md)

# Permissions, agents, and MCP

Core owns player state, permissions, and confirmations. Adapters authenticate connections and forward requests. This document describes API 1.0. The [schema](../contract.schema.json) defines the message structure, and the [semantic guarantees](../../crates/core/src/guarantees.md) cover origin, ordering, and retries.

Implementation entry points include `crates/core/src/handle.rs`, `permissions.rs`, `permissions/checks.rs`, and `permissions/fs_deadline.rs`.

## Identity and origin

A command originates from UI, System, MediaControls, Remote, or an authenticated Agent. Adapters must derive the Origin without trusting a value supplied by the client. An Agent obtains an `AgentHandle` using a registered credential. The credential is shown once and stored as a hash. Rotation or removal immediately invalidates old connections. Registration, credential rotation, grant management, and confirmations belong exclusively to the trusted UI.

Grants depend on the Agent identity, operation, scope, and lifetime. Long-lived grants apply across connections, while session grants belong to one connection. Deny takes precedence over allow. Directory scope checks both the library-relative location and the actual canonical location. An overlapping deny can block the entire directory operation and return the rule ID and `all` or `part` without exposing the denied paths. Unknown locations cannot be assumed safe. Permanent deletion always requires confirmation. Undo is classified by its actual effects. Directory undo still follows directory rules, and undo that writes files still requires `file_write`.

## Bounded checks and confirmation

Blocking checks such as path resolution, directory identity, and file previews run outside the actor within command deadlines and concurrency budgets. Results carry the generations they depend on. Recheck when roots, grants, or the Agent state change. A slow filesystem cannot block the player control thread. Handle FIFOs, devices, symlinks, and hard links according to platform semantics and avoid authorizing paths by string prefix alone.

Confirmation displays canonical targets, counts, skip reasons, and actual effects. It reserves no files. After approval, revalidate identity, contents, and authorization. Changes produce structured errors such as `location_changed`. Ending a session, removing an Agent, or registering one again withdraws commands that have yet to execute. Jobs already started follow their own lifecycle. A confirmation token is bound to the request, origin, and session, expires, and cannot be replayed. Approval must come from the trusted window rather than the theme.

## MCP boundary

`mpmp-agent-mcp` serves the same local user over a Unix socket or a Windows named pipe keyed by SID. `player-mcp` connects to the MCP client over stdio. Each connection authenticates and opens its own session. A client can make several connections, so the conversation count does not indicate the session count. The bridge verifies the endpoint user before sending credentials and offers no network listener.

Tools cover status, queries, the queue, ratings, playlists, directories, action history, and contract queries and commands. Core origin policies, permissions, and confirmations still apply. A confirmation wait defaults to 25 seconds and is capped at 50. If it is pending, wait again. A timeout or disconnection cannot establish that a command did not execute. Use idempotency keys and authoritative queries to recover before retrying writes. Job IDs are valid only for one core run. `job_not_found` does not establish that a job never ran.

See the [MCP setup guide](../agents/mcp-setup.md) for connection details, diagnostic mode, environment variables, and client configuration.
