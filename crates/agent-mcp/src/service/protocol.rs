use super::*;

const INSTRUCTIONS: &str = concat!(
    "Tools of the mpmp on this computer. Some requests (adding or removing ",
    "music folders, writing or reading playlist files, permanent deletion) ",
    "wait for the user's approval in the player: the result is ",
    "needs_confirmation with a token; tell the user, then call ",
    "wait_for_confirmation. Library edits are recorded and can be undone ",
    "(recent_actions, undo). A permission_denied error names the user's forbid ",
    "rules by number (details.forbidden_by), never the places they forbid: ",
    "tell the user which rule refused it. Where every rule says covers: part, ",
    "only a part of what was asked is forbidden: a folder's other subfolders ",
    "may still be added one by one instead of the folder; covers: all means ",
    "nothing inside it may be. For anything the task tools do not cover, look ",
    "it up with contract_reference, then use contract_command or ",
    "contract_query.",
);

impl ServerHandler for Session {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("personal-music-player", env!("CARGO_PKG_VERSION"))
                    .with_title("mpmp"),
            )
            .with_instructions(INSTRUCTIONS)
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

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if *self.closed.borrow() {
            return Ok(tools::command_result(closed()).into());
        }
        let args = request.arguments.unwrap_or_default();
        let cancelled = context.ct.clone();
        let stop = async move { cancelled.cancelled().await };
        match tools::call(self, &request.name, args, stop).await {
            Some(result) => Ok(result.into()),
            None => Err(ErrorData::invalid_params(
                format!("unknown tool: {}", request.name),
                None,
            )),
        }
    }
}
