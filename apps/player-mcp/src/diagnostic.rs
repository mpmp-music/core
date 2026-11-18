use super::*;

/// Why the bridge cannot serve; each is told to the framework as a JSON-RPC error.
#[derive(Debug)]
pub(super) enum Failure {
    NotRunning,
    TokenMissing,
    /// A variable reference the framework left as it was (`${PLAYER_MCP_TOKEN}`, `$…`,
    /// `%…%`): a token is `pmpa_` and hex digits, never `$`, `%`, `{`.
    TokenNotExpanded,
    TokenInvalid,
    NotSameUser,
    AccessDenied,
    NoAnswer,
    Busy,
    Endpoint(String),
    Connect(String),
    Refused(String),
}

impl Failure {
    /// A JSON-RPC server error code (-32000 … -32099).
    fn code(&self) -> i64 {
        match self {
            Failure::NotRunning => -32000,
            Failure::TokenMissing | Failure::TokenNotExpanded | Failure::TokenInvalid => -32001,
            Failure::NotSameUser => -32002,
            Failure::NoAnswer | Failure::Refused(_) => -32003,
            Failure::Busy => -32004,
            Failure::Endpoint(_) | Failure::Connect(_) => -32005,
            Failure::AccessDenied => -32006,
        }
    }

    /// What is wrong.
    fn reason(&self) -> String {
        match self {
            Failure::NotRunning => "mpmp is not running".into(),
            Failure::TokenMissing => format!("{TOKEN_ENV} is missing"),
            Failure::TokenNotExpanded => {
                format!("{TOKEN_ENV} is a variable reference the framework did not expand")
            }
            Failure::TokenInvalid => format!(
                "{TOKEN_ENV} is not a valid token (mistyped, renewed, or the agent was removed)"
            ),
            Failure::NotSameUser => {
                "The mpmp endpoint belongs to another user (the token was not sent)".into()
            }
            Failure::AccessDenied => concat!(
                "Access to the mpmp endpoint was denied: it belongs to another user (the ",
                "token was not sent)",
            )
            .into(),
            Failure::NoAnswer => "mpmp did not answer".into(),
            Failure::Busy => "mpmp has too many agent connections".into(),
            Failure::Endpoint(err) => {
                format!("Cannot tell where mpmp listens: {err}")
            }
            Failure::Connect(err) => format!("Cannot connect to mpmp: {err}"),
            Failure::Refused(why) => format!("mpmp refused the connection: {why}"),
        }
    }

    /// What the user should do about it: then start a new session, or, when it may mend
    /// itself ([`Failure::may_mend`]), call again.
    pub(super) fn remedy(&self) -> String {
        match self {
            Failure::NotRunning => {
                format!("start mpmp (with its agent service on), then {CALL_AGAIN}")
            }
            // Some frameworks pass MCP servers little or none of the shell's environment
            // (GitHub Copilot CLI only PATH; Gemini CLI hides *TOKEN* variables).
            Failure::TokenMissing => format!(
                concat!(
                    "register this agent in mpmp and set {TOKEN_ENV} in the \"env\" of this ",
                    "MCP server's entry in the framework's config: some frameworks do not pass ",
                    "the shell's environment variables to MCP servers; then restart the ",
                    "framework",
                ),
                TOKEN_ENV = TOKEN_ENV,
            ),
            Failure::TokenNotExpanded => concat!(
                "set the variable in the environment the framework starts in (or check how ",
                "the framework writes variables in \"env\"); a program that was already ",
                "running (the framework's app, and terminals opened from it) does not see ",
                "a variable set later (setx): restart it",
            )
            .into(),
            Failure::TokenInvalid => format!(
                concat!(
                    "create a new token for this agent in mpmp (renew it), put it in ",
                    "{TOKEN_ENV}, then restart the framework",
                ),
                TOKEN_ENV = TOKEN_ENV,
            ),
            Failure::NotSameUser | Failure::AccessDenied => {
                "run mpmp as the same user as the framework, then start a new session".into()
            }
            Failure::NoAnswer | Failure::Refused(_) => {
                "restart mpmp, then start a new session".into()
            }
            Failure::Busy => "close some agent sessions, then start a new one".into(),
            Failure::Endpoint(_) => concat!(
                "check PLAYER_MCP_ENDPOINT (usually it should not be set), then start a ",
                "new session",
            )
            .into(),
            Failure::Connect(_) => {
                format!("make sure mpmp is running, then {CALL_AGAIN}")
            }
        }
    }

    /// Whether it may mend itself while the bridge runs: the player was not running, or
    /// connecting failed. The diagnostic server then checks again at each tool call.
    pub(super) fn may_mend(&self) -> bool {
        matches!(self, Failure::NotRunning | Failure::Connect(_))
    }

    /// One line: what is wrong — what to do.
    pub(super) fn message(&self) -> String {
        format!("{} — {}", self.reason(), self.remedy())
    }
}

/// What to do once a failure that may mend itself was dealt with.
pub(super) const CALL_AGAIN: &str = concat!(
    "call a tool of this server again (if it still fails, start a new session ",
    "or reconnect this MCP server in the framework)",
);

/// The one tool of the diagnostic server.
pub(super) const STATUS_TOOL: &str = "player_mcp_status";

/// Tells the framework why there is no player to talk to: on stderr, and as a small MCP
/// server of its own (`Diagnostic`) until stdin closes, or, for a failure that may mend
/// itself, until a tool call finds the player listening and has been answered.
pub(super) async fn fail(failure: Failure) -> ExitCode {
    eprintln!("player-mcp: {}", failure.message());
    let mut diagnostic = Diagnostic::new(&failure);
    if failure.may_mend() {
        diagnostic.recheck = Endpoint::candidates_from_env().ok();
    }
    let reachable = Arc::clone(&diagnostic.reachable);
    let transport = AsyncRwTransport::new_server(tokio::io::stdin(), tokio::io::stdout());
    match diagnostic.serve(transport).await {
        Ok(running) => {
            let stop = running.cancellation_token();
            let done = running.waiting();
            tokio::pin!(done);
            tokio::select! {
                            _ = &mut done => {}
                            () = reachable.notified() => {
                                // rmcp still sends the answers already made (this call's among them)
                                // before it closes stdout.
                                stop.cancel();
                                let _ = done.await;
                                eprintln!(concat!(
            "player-mcp: mpmp is running now: exiting so that the framework starts the ",
            "bridge again",
            ));
                            }
                        }
        }
        Err(err) => eprintln!("player-mcp: the framework's MCP session did not start: {err}"),
    }
    ExitCode::FAILURE
}

/// The MCP server the bridge is when it cannot reach the player: its one tool's description
/// says why, and any tool call fails (`isError`) saying the same. The code is the bridge's
/// error code (-32000 … -32006, `Failure::code`), as the docs' table lists them.
pub(super) struct Diagnostic {
    pub(super) tools: Vec<Tool>,
    pub(super) error: Value,
    instructions: String,
    /// Where to look for the player at each tool call, when the failure may mend itself
    /// ([`Failure::may_mend`]).
    recheck: Option<Vec<Endpoint>>,
    /// Told when a tool call found the player listening: the bridge exits once that call
    /// was answered.
    reachable: Arc<Notify>,
}

/// The answer to a tool call once the player listens: not an error of the player's, but
/// the call did not happen.
fn call_again() -> Value {
    json!({
            "retry": true,
            "reason": "mpmp is running now; this server (player-mcp) started before it did",
            "message": concat!(
    "mpmp is running now. This server (player-mcp) restarts to connect to it: ",
    "call the tool again (nothing was done). If the next call fails because ",
    "the server is closed or disconnected, the framework does not restart MCP ",
    "servers by itself: tell the user to reconnect this MCP server in the ",
    "framework, or start a new session.",
    ),
        })
}

/// Whether the player listens at one of `endpoints`, as the current user: connects (with
/// the same-user check) and hangs up at once, sending nothing; at most [`PROBE`].
async fn listens(endpoints: &[Endpoint]) -> bool {
    matches!(
        tokio::time::timeout(PROBE, endpoint::connect_any(endpoints)).await,
        Ok(Ok(_))
    )
}

impl Diagnostic {
    pub(super) fn new(failure: &Failure) -> Self {
        let (reason, remedy) = (failure.reason(), failure.remedy());
        let description = format!(
            concat!(
                "mpmp's MCP service is not available right now: {reason}. Tell the user: ",
                "{remedy}. (No other tool works until then; this one only reports the ",
                "problem.)",
            ),
            reason = reason,
            remedy = remedy,
        );
        let mut schema = Map::new();
        schema.insert("type".into(), "object".into());
        schema.insert("properties".into(), Value::Object(Map::new()));
        let mut tool = Tool::new(STATUS_TOOL, description.clone(), Arc::new(schema));
        tool.annotations = Some(
            ToolAnnotations::new()
                .read_only(true)
                .destructive(false)
                .open_world(false),
        );
        Self {
            tools: vec![tool],
            error: json!({
                "code": failure.code(),
                "reason": reason,
                "tell_the_user": remedy,
                "message": description,
            }),
            instructions: description,
            recheck: None,
            reachable: Arc::new(Notify::new()),
        }
    }
}

impl ServerHandler for Diagnostic {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("personal-music-player", env!("CARGO_PKG_VERSION"))
                    .with_title("mpmp"),
            )
            .with_instructions(self.instructions.clone())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools.clone()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|t| t.name == name).cloned()
    }

    /// Any tool, also one the framework still knows from a session that worked (Codex keeps
    /// the old names): the same failure; or, when the player listens now, "call again", and
    /// the bridge exits.
    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if let Some(endpoints) = &self.recheck
            && listens(endpoints).await
        {
            self.reachable.notify_one();
            return Ok(CallToolResult::structured_error(call_again()).into());
        }
        Ok(CallToolResult::structured_error(self.error.clone()).into())
    }
}
