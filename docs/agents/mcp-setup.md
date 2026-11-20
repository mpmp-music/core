[中文](../zh-CN/agents/mcp-setup.md)

# Connect to mpmp over MCP

Agent clients such as Claude Code, Codex, and Gemini CLI launch `player-mcp` over stdio. The bridge connects to a running mpmp instance. Connections are limited to the same OS user, and there is no public network listener. See [permissions and sessions](../design/agent-permissions.md). The snippets below provide baseline configurations. The program name, environment variables, transports, and wait behavior were checked against the repository's bridge implementation. Recent client options were not verified online. After upgrading a client, test it against the documentation for that version.

## Build and register

The first public release provides source code. Run the following in core.

```sh
cargo build --release -p mpmp-mcp --locked --offline
```

Use the absolute path to `target/release/player-mcp` (`player-mcp.exe` on Windows) in your configuration. Use `--offline` only when dependencies are cached. Register an identity in the desktop host's trusted Agent settings. The credential is shown once and the player stores only its hash. Assign identities by purpose and rotate any lost credential. Rotation immediately invalidates the old credential and connections. Keep credentials out of repositories, conversations, and debug logs.

The bridge reads its credential only from `PLAYER_MCP_TOKEN`. The client must explicitly forward the variable, and it must exist before the client process starts. Graphical apps may not inherit shell settings. Fully restart the client after changing them. The examples contain variable references and placeholder paths. Escape Windows backslashes in JSON.

## Baseline client configurations

Keep `personal-music-player` as the service name for compatibility. The client itself has no authority over player permissions. Its tool approvals cannot substitute for mpmp confirmations, and a client can open several connections.

Claude Code project `.mcp.json` example. Commit only the variable reference.

```json
{
  "mcpServers": {
    "personal-music-player": {
      "type": "stdio",
      "command": "/absolute/path/to/player-mcp",
      "args": [],
      "env": { "PLAYER_MCP_TOKEN": "${PLAYER_MCP_TOKEN}" }
    }
  }
}
```

Codex `config.toml` example. Use `env_vars` to forward the startup environment and keep plaintext out of `env`.

```toml
[mcp_servers.personal-music-player]
command = "/absolute/path/to/player-mcp"
env_vars = ["PLAYER_MCP_TOKEN"]
tool_timeout_sec = 120
```

VS Code `mcp.json` example. The client manages the input value.

```json
{
  "inputs": [{ "type": "promptString", "id": "mpmp-token", "description": "mpmp Agent token", "password": true }],
  "servers": {
    "personal-music-player": {
      "type": "stdio",
      "command": "/absolute/path/to/player-mcp",
      "env": { "PLAYER_MCP_TOKEN": "${input:mpmp-token}" }
    }
  }
}
```

For other clients, use their stdio service configuration and their own variable syntax.

| Client | Configuration and credential forwarding |
|---|---|
| Gemini CLI | `mcpServers` in settings.json, with `"PLAYER_MCP_TOKEN":"$PLAYER_MCP_TOKEN"` explicitly set in `env` |
| Cursor | `mcpServers` in mcp.json, with `${env:PLAYER_MCP_TOKEN}` referenced in `env` |
| GitHub Copilot CLI | A local stdio entry in `mcpServers`, with `${PLAYER_MCP_TOKEN}` referenced in `env`. Check required fields in the installed version. |
| Claude Desktop | `mcpServers` in developer settings. Verify how the installed version stores credentials securely. Shell variable expansion is version-dependent. |
| Windsurf / Devin | `mcpServers`, using the version's supported `${env:PLAYER_MCP_TOKEN}` syntax or a protected file input |
| Zed | `context_servers` in settings.json. Verify the installed version's environment or secure storage interface and keep credentials out of shared project settings. |

The named clients can connect over standard MCP stdio. Retest service activation, reconnection, and secure storage on the installed version. There are no client-specific approval bypass configurations or connection count guarantees.

## Operations and confirmations

After connecting, call `player_status` to verify the connection. A healthy bridge logs `connected to mpmp as agent` and the identity name to stderr without logging the credential. Tools use core for commands, queries, and subscriptions, with permissions controlled by the player. Directory operations, M3U reads and writes, file changes, and output settings require confirmations or grants according to policy. Permanent deletion always requires confirmation, and deny takes precedence.

Each bridge connection is a session. Session grants do not carry over to other connections. Disconnecting withdraws commands that have not started. Jobs already started follow their normal lifecycle. `wait_for_confirmation` defaults to 25 seconds and allows up to 50. Leave headroom in the client's tool timeout. Use the default as a conservative starting point and wait again if pending. A timeout cannot establish whether the operation ran. Use an idempotency key and query authoritative state before retrying.

`contract_reference` provides the query schema. Use read-only `job` and `jobs` queries through `contract_query` without rerunning jobs. JobId values are scoped to a core run, and terminal jobs may be evicted under the cache budget. `job_not_found` cannot establish that a job never ran. `recent_actions` and `undo` still follow actual action permissions and undo limits.

## Diagnostics and transport

When the bridge cannot connect, it enters diagnostic MCP mode and exposes only `player_mcp_status`. Calls to old tools also return `isError` and a structured reason. Missing, invalid, and unexpanded credentials, endpoints owned by another user, connection refusals, timeouts, and excessive connection counts are reported separately. Credentials are never sent to another user's endpoint.

If the player is stopped or a connection fails temporarily, subsequent calls probe the endpoint. Once the player is found, the bridge returns a retry hint and exits so the client can restart it. Reconnect manually if the client does not restart it. After rotating a credential, update the startup environment and then fully restart the client. The bridge cannot obtain the new credential itself.

- If `PLAYER_MCP_ENDPOINT` is set, connect only to that location. Use an absolute socket path on Unix or a named pipe on Windows.
- `PLAYER_DATA_DIR` takes precedence over `PLAYER_MANUAL_DATA_DIR`. A Unix directory override uses `run/mcp.sock`. Windows still names the pipe by the current user's SID.
- The Windows default is `\\.\pipe\personal-music-player-mcp-<SID>`.
- The Linux host prefers `$XDG_RUNTIME_DIR/personal-music-player/mcp.sock` and otherwise uses `mpmp/run/mcp.sock` under the data root. The bridge checks the runtime directory, the XDG data root, the compatibility locations `~/.local/share` and `/run/user/<uid>`, and then the old personal-music-player data root. It tries each location once and proceeds only if nothing is listening there.
- The new macOS default is `~/Library/Application Support/mpmp/run/mcp.sock`. The bridge then tries the old personal-music-player data root. It does not resolve macOS paths through XDG.
- Only the bridge still checks old addresses. The new host does not listen in the old data root. Update an explicit endpoint that points to the old socket. Default directory migration preserves a copy of the old data.

When maintaining this page, check environment variables, diagnostic codes, wait limits, and endpoint tests in `apps/player-mcp` and `crates/agent-mcp`. For changes to client snippets, record the client version and test variable expansion, credential-free logs, reconnection, and confirmation in that client. Keep public documentation independent of internal research and personal device records.
