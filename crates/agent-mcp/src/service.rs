//! One connection's MCP service: an agent's session, whose tool calls run as
//! `Origin::Agent { name, session }`.

mod protocol;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mpmp_core::CoreHandle;
use mpmp_core::contract::{
    CommandResult, ContractError, ErrorCode, ErrorKind, Query, QueryResult, Request, Response,
};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::Value;
use tokio::sync::{OwnedRwLockReadGuard, watch};

use crate::confirm::{Confirmations, Owner, Status};
use crate::tools;

/// Whether the session may still send commands to core (see `SessionGuard`). A command
/// holds it (read) from before it is sent until core has answered and a token it got is
/// recorded; ending the session first withdraws commands in core, then closes it (write)
/// before forgetting tokens, waiting for any late claims.
pub(crate) type Gate = tokio::sync::RwLock<bool>;

pub(crate) struct Session {
    core: CoreHandle,
    agent: mpmp_core::AgentHandle,
    owner: Owner,
    confirmations: Arc<Confirmations>,
    max_wait: Duration,
    /// `true` once the connection is being closed.
    closed: watch::Receiver<bool>,
    gate: Arc<Gate>,
    next_request: AtomicU64,
    tools: Vec<Tool>,
}

impl Session {
    pub(crate) fn new(
        core: CoreHandle,
        agent: mpmp_core::AgentHandle,
        owner: Owner,
        confirmations: Arc<Confirmations>,
        max_wait: Duration,
        closed: watch::Receiver<bool>,
        gate: Arc<Gate>,
    ) -> Self {
        Self {
            core,
            agent,
            owner,
            confirmations,
            max_wait,
            closed,
            gate,
            next_request: AtomicU64::new(1),
            tools: tools::definitions(),
        }
    }

    pub(crate) fn next_request_id(&self) -> u64 {
        self.next_request.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn max_wait(&self) -> Duration {
        self.max_wait
    }

    /// This connection's session id (a UUID).
    pub(crate) fn session_id(&self) -> &str {
        &self.owner.session
    }

    /// Now on core's clock, the clock of `PositionInfo::at_ms`.
    pub(crate) fn now_ms(&self) -> u64 {
        self.core.now_ms()
    }

    /// The gate, held open (read) for one command; `None` once the session is ending.
    async fn open(&self) -> Option<OwnedRwLockReadGuard<bool>> {
        let open = Arc::clone(&self.gate).read_owned().await;
        (*open && !*self.closed.borrow()).then_some(open)
    }

    /// Runs a command as this session's agent (off the async threads: core blocks).
    pub(crate) async fn run(&self, command: mpmp_core::contract::Command) -> CommandResult {
        let Some(open) = self.open().await else {
            return closed();
        };
        let request = Request::new(self.next_request_id(), command);
        let (agent, session) = (self.agent.clone(), self.owner.session.clone());
        // The gate goes with the call into the blocking task and comes back with its result:
        // it stays open until core has answered even when this call is dropped meanwhile
        // (the blocking task runs on), and until a token core gave is recorded.
        match blocking(move || (agent.execute(request, &session).result, open)).await {
            Ok((result, open)) => self.claim(result, open),
            Err(error) => CommandResult::Err { error },
        }
    }

    /// Runs a request given as contract JSON, parsed by the contract's strict rules (with
    /// the gate as for [`Session::run`]).
    pub(crate) async fn run_json(&self, request: String) -> CommandResult {
        let Some(open) = self.open().await else {
            return closed();
        };
        let (agent, session) = (self.agent.clone(), self.owner.session.clone());
        match blocking(move || (agent.execute_json(&request, &session), open)).await {
            Ok((json, open)) => {
                let result = match serde_json::from_str::<Response>(&json) {
                    Ok(response) => response.result,
                    Err(e) => CommandResult::Err {
                        error: ContractError::failed(ErrorCode::Io, e.to_string()),
                    },
                };
                self.claim(result, open)
            }
            Err(error) => CommandResult::Err { error },
        }
    }

    /// Records a token core gave this session (`needs_confirmation`) as this session's,
    /// while the gate is still open: a session that ended keeps no tokens.
    fn claim(&self, result: CommandResult, open: OwnedRwLockReadGuard<bool>) -> CommandResult {
        if let CommandResult::Err { error } = &result
            && error.kind == ErrorKind::NeedsConfirmation
            && let Some(ticket) = &error.details.confirmation
        {
            self.confirmations
                .claim(&ticket.token, &self.owner, ticket.expires_at_ms);
        }
        drop(open);
        result
    }

    /// Answers a query; a failed query is its contract error.
    pub(crate) async fn query(&self, query: Query) -> Result<QueryResult, ContractError> {
        let core = self.core.clone();
        match blocking(move || core.query(query)).await? {
            QueryResult::Err { error } => Err(error),
            result => Ok(result),
        }
    }

    /// Answers a query given as contract JSON.
    pub(crate) async fn query_json(&self, query: String) -> Value {
        let core = self.core.clone();
        let json = match blocking(move || core.query_json(&query)).await {
            Ok(json) => json,
            Err(error) => return serde_json::json!({"type": "err", "error": error}),
        };
        serde_json::from_str(&json).unwrap_or(Value::Null)
    }

    /// A command's result as a tool result (its token, if any, is this session's already:
    /// [`Session::run`]).
    pub(crate) fn command_outcome(&self, result: CommandResult) -> rmcp::model::CallToolResult {
        tools::command_result(result)
    }

    pub(crate) async fn wait(
        &self,
        token: &str,
        timeout: Duration,
        stop: impl Future<Output = ()>,
    ) -> Status {
        let mut closed = self.closed.clone();
        let stop = async move {
            tokio::select! {
                () = stop => {}
                _ = closed.wait_for(|c| *c) => {}
            }
        };
        self.confirmations
            .wait(token, &self.owner, timeout, stop)
            .await
    }
}

/// A command sent after the connection closed.
fn closed() -> CommandResult {
    CommandResult::Err {
        error: ContractError::failed(ErrorCode::Cancelled, "the connection has closed"),
    }
}

/// Runs a blocking call to core on tokio's blocking threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ContractError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ContractError::failed(ErrorCode::Io, e.to_string()))
}
