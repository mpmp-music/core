//! `player-mcp`: the stdio bridge between a desktop agent framework (Claude Code, Codex …)
//! and mpmp (docs/design/agent-permissions.md).
//!
//! The framework starts it as an MCP server over stdio, with the agent's token in
//! `PLAYER_MCP_TOKEN`. It connects to the player's endpoint (`PLAYER_MCP_ENDPOINT`, else
//! where the user's player may listen, `Endpoint::candidates_for_current_user`), makes sure
//! the player runs as the same user, presents the token (`PLAYER-MCP/1 <token>`), and once
//! the player accepts, copies bytes both ways unchanged. It reads only the ids of requests
//! and responses going through, to know at the end whether every request was answered.
//!
//! When it cannot get there (the player is not running, the token is missing or no longer
//! valid, the endpoint is another user's …) it does not exit: frameworks show the model
//! nothing of a server that fails to start (Claude Code: "Connection closed"; Codex drops
//! its tools but keeps their old names). It writes why to stderr and serves MCP itself
//! (rmcp, every protocol version the adapter serves), with one tool, `player_mcp_status`,
//! whose description and every call's error say what is wrong and what to tell the user,
//! until stdin closes; then it exits non-zero. When the player was only not running (or the
//! connection failed), each tool call first checks whether it listens now (only connecting,
//! with the same-user check: no token, no handshake); once it does, the call's answer says
//! to call again and the bridge exits (non-zero), so that a framework that restarts MCP
//! servers by itself (Claude Code) starts a new bridge, which connects.
//! Other failures do not mend themselves (the token, another user's endpoint …): once
//! fixed, a new session starts a new bridge. The token is never written anywhere.

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("player-mcp supports only desktop endpoint targets");

mod connection;
#[cfg(test)]
use connection::not_expanded;
use connection::{Reader, Writer, connect};
mod forward;
#[cfg(test)]
use forward::Pending;
use forward::forward;
mod diagnostic;
#[cfg(test)]
use diagnostic::{CALL_AGAIN, Diagnostic, STATUS_TOOL};
use diagnostic::{Failure, fail};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::io;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use mpmp_agent_mcp::endpoint::{self, ClientStream, ConnectError, Endpoint, TOKEN_ENV};
use mpmp_agent_mcp::handshake::{self, Answer, Refusal};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Notify;
use tokio::time::Instant;

/// After the framework closes stdin: once every request it sent was answered, the bridge
/// exits when the player has sent nothing for this long.
const QUIET: Duration = Duration::from_millis(300);

/// … and waits for answers at most this long (a `wait_for_confirmation` takes up to 50 s).
const ANSWERS: Duration = Duration::from_secs(60);

/// The longest message whose id the bridge reads; longer ones go through unread.
const MAX_TRACKED_LINE: usize = 16 * 1024 * 1024;

/// In the diagnostic mode, how long a tool call waits to learn whether the player listens
/// now.
const PROBE: Duration = Duration::from_secs(1);

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("player-mcp: cannot start: {err}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(run());
    // Stdin is read on a blocking thread that may still wait for input: do not wait for it.
    runtime.shutdown_background();
    code
}

async fn run() -> ExitCode {
    match connect().await {
        Ok((reader, writer)) => forward(reader, writer).await,
        Err(failure) => fail(failure).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requests are known by their ids, also across chunks and in batches; notifications
    /// and the player's own requests are not waited for.
    #[test]
    fn pending_requests_are_tracked() {
        let mut p = Pending::default();
        p.sent(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"meth");
        assert!(p.is_empty(), "not a whole line yet");
        p.sent(
        b"od\":\"tools/call\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n"
        );

        p.sent(b"[{\"id\":\"a\",\"method\":\"ping\"},{\"id\":null,\"method\":\"x\"}]\ngarbage\n");
        assert_eq!(p.ids.len(), 2);
        // The player's notification, and a request of its own: no answer to anything.
        p.answered(b"{\"method\":\"notifications/progress\"}\n{\"id\":1,\"method\":\"ping\"}\n");
        assert_eq!(p.ids.len(), 2);
        p.answered(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n{\"id\":\"a\",\"err");
        assert_eq!(p.ids.len(), 1);
        p.answered(b"or\":{}}\n");
        assert!(p.is_empty());
        // An answer too long to read answers everything.
        p.sent(b"{\"id\":7,\"method\":\"tools/call\"}\n");
        let long = vec![b'x'; MAX_TRACKED_LINE + 1];
        p.answered(&long);
        assert!(p.is_empty());
        p.answered(b"\n{\"id\":8}\n");
        assert!(p.is_empty());
        // A cancelled request is not answered (rmcp drops the answer): not waited for. The
        // player's own cancellations change nothing.
        p.sent(b"{\"id\":9,\"method\":\"tools/call\"}\n{\"id\":\"b\",\"method\":\"ping\"}\n");
        p.answered(b"{\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":9}}\n");
        assert_eq!(p.ids.len(), 2);
        p.sent(
        b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":9,\"reason\":\"x\"}}\n"
        );

        assert_eq!(p.ids.len(), 1);
        p.sent(b"[{\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":\"b\"}}]\n");
        assert!(p.is_empty());
    }

    /// Each failure says what is wrong and what to tell the user, in the tool's
    /// description and in every call's error, with the bridge's error code.
    #[test]
    fn the_diagnostic_server_says_why() {
        let diagnostic = Diagnostic::new(&Failure::NotRunning);
        let [tool] = diagnostic.tools.as_slice() else {
            panic!("one tool")
        };
        assert_eq!(tool.name, STATUS_TOOL);
        let description = tool.description.as_deref().unwrap();
        assert!(
            description.starts_with(concat!(
                "mpmp's MCP service is not available right now: mpmp is not running. Tell ",
                "the user: start mpmp",
            )),
            "{description}"
        );
        assert_eq!(diagnostic.error["code"], -32000);
        assert_eq!(diagnostic.error["message"], description);
        for (failure, code) in [
            (Failure::TokenInvalid, -32001),
            (Failure::NotSameUser, -32002),
            (Failure::NoAnswer, -32003),
            (Failure::Busy, -32004),
            (Failure::Connect("x".into()), -32005),
            (Failure::AccessDenied, -32006),
        ] {
            assert_eq!(Diagnostic::new(&failure).error["code"], code);
        }
    }

    /// Only a player that was not there (or a connection that failed) is looked for again;
    /// the token, another user's endpoint … do not mend themselves.
    #[test]
    fn what_may_mend_itself() {
        assert!(Failure::NotRunning.may_mend());
        assert!(Failure::Connect("x".into()).may_mend());
        for failure in [
            Failure::TokenMissing,
            Failure::TokenNotExpanded,
            Failure::TokenInvalid,
            Failure::NotSameUser,
            Failure::AccessDenied,
            Failure::NoAnswer,
            Failure::Busy,
            Failure::Endpoint("x".into()),
            Failure::Refused("x".into()),
        ] {
            assert!(!failure.may_mend(), "{failure:?}");
        }
        assert!(Failure::NotRunning.remedy().ends_with(CALL_AGAIN));
        assert!(!Failure::TokenInvalid.remedy().contains(CALL_AGAIN));
    }

    #[test]
    fn messages_never_carry_the_token() {
        for failure in [
            Failure::TokenMissing,
            Failure::TokenNotExpanded,
            Failure::TokenInvalid,
        ] {
            assert!(failure.message().contains(TOKEN_ENV));
        }
        // A missing token says to name it in the config's env: the shell's may not reach us.
        let missing = Failure::TokenMissing.message();
        assert!(missing.contains("\"env\""), "{missing}");
        assert!(
            missing.contains("do not pass the shell's environment"),
            "{missing}"
        );
    }

    #[test]
    fn unexpanded_references_are_told_apart() {
        for token in [
            "${PLAYER_MCP_TOKEN}",
            "$PLAYER_MCP_TOKEN",
            "%PLAYER_MCP_TOKEN%",
            "${env:PLAYER_MCP_TOKEN}",
        ] {
            assert!(not_expanded(token), "{token}");
        }
        assert!(!not_expanded(&format!("pmpa_{}", "0a".repeat(32))));
    }
}
