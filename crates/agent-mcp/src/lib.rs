//! Agents over MCP (docs/design/agent-permissions.md): the app's MCP service, which
//! desktop agent frameworks reach through the `player-mcp` bridge.
//!
//! ```text
//! framework ──stdio── player-mcp ──Unix socket / named pipe── mpmp_agent_mcp::Server ── CoreHandle
//! ```
//!
//! - [`endpoint`]: where the service listens; both ends check that the other is the same
//!   user.
//! - [`handshake`]: the bridge's first line carries the agent's token (`PLAYER_MCP_TOKEN`),
//!   which core turns into the agent's name (`CoreHandle::authenticate_agent`). The token
//!   never enters MCP, so the model never sees it.
//! - With the `server` feature (default): [`Server::start`] runs the service on threads of
//!   its own. Each connection is one session of its agent (a random UUID): its commands run
//!   as `Origin::Agent { name, session }`, its session ends in core when it closes, and it
//!   is closed when its token stops naming its agent (`AgentsChanged`). The tools are a few
//!   task-sized ones plus `contract_command` / `contract_query` (not the `agents` /
//!   `permissions` queries) with `contract_reference` to look them up, and
//!   `wait_for_confirmation` for what the user must approve.
//!
//! The bridge enables only `desktop-endpoint` (endpoint and handshake). Desktop hosts
//! enable `server`, which also enables `desktop-endpoint` and mpmp-core/desktop-fs.
//! With no features this crate is an empty shell; no mobile transport is provided.

#[cfg(all(
    feature = "desktop-endpoint",
    not(any(target_os = "linux", target_os = "macos", target_os = "windows"))
))]
compile_error!("desktop-endpoint supports only Linux, macOS and Windows");

#[cfg(feature = "desktop-endpoint")]
pub mod endpoint;

#[cfg(feature = "desktop-endpoint")]
pub mod handshake;

#[cfg(all(windows, feature = "desktop-endpoint"))]
mod windows;

#[cfg(feature = "server")]
mod confirm;
#[cfg(feature = "server")]
mod listener;
#[cfg(feature = "server")]
mod reference;
#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
mod service;
#[cfg(feature = "server")]
mod tools;

#[cfg(feature = "desktop-endpoint")]
pub use endpoint::Endpoint;
#[cfg(feature = "server")]
pub use server::{Server, ServerHandle, ServerOptions};
#[cfg(feature = "server")]
pub use tools::{DEFAULT_WAIT, MAX_WAIT};
