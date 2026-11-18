//! The session a server serves when it can't serve its client as its settings say: when
//! the client requires the shared engine and none could be used, or is set to drive a
//! background desktop that can't be. It lists the client's tools, and every call fails
//! with `engine_unavailable` or `background_unavailable` and why, so the agent can tell the
//! user. It never reaches an engine, niri or the desktop.

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt as _};

use super::log::Log;
use crate::error::{ErrorName, ToolError};
use crate::session::Incoming;

/// Answers every call with its one error.
pub(crate) struct Unavailable {
    tools: Vec<Tool>,
    error: ToolError,
    log: Log,
}

impl Unavailable {
    /// A session listing `tools`, whose calls fail with `engine_unavailable` and
    /// `reason`, why the shared engine couldn't be used, each logged to `log`.
    pub(crate) fn shared(tools: Vec<Tool>, reason: &str, log: Log) -> Self {
        Self {
            tools,
            log,
            error: ToolError::new(
                ErrorName::EngineUnavailable,
                format!(
                    "this client requires the shared engine (shared = \"required\" or NIRI_COMPUTER_USE_SHARED=required), which couldn't be used: {reason}. Fix the cause, then restart this MCP server"
                ),
            ),
        }
    }

    /// A session listing `tools`, whose calls fail with `background_unavailable` and
    /// `reason`, why the background desktop it is set to drive can't be used, each logged
    /// to `log`.
    pub(crate) fn background(tools: Vec<Tool>, reason: &str, log: Log) -> Self {
        Self {
            tools,
            log,
            error: ToolError::new(
                ErrorName::BackgroundUnavailable,
                format!(
                    "this server is set to drive a background desktop ([background] in the policy file or NIRI_COMPUTER_USE_BACKGROUND), which can't be used: {reason}. It never drives the live desktop instead. Fix the cause, then restart this MCP server"
                ),
            ),
        }
    }

    /// Serves the client on stdin and stdout until its end, or a line over `MAX_LINE`, as
    /// every other session.
    pub(crate) async fn run(self) -> Result<(), String> {
        let (input, _gone) = Incoming::new(tokio::io::stdin());
        let service = self
            .serve((input, tokio::io::stdout()))
            .await
            .map_err(|error| format!("start MCP session: {error}"))?;
        service
            .waiting()
            .await
            .map(drop)
            .map_err(|error| format!("MCP session: {error}"))
    }
}

impl ServerHandler for Unavailable {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "niri-computer-use",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "Every tool fails with {}: {}",
                serde_json::to_value(self.error.name)
                    .ok()
                    .and_then(|name| name.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                self.error.detail
            ))
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "rmcp's handler methods return futures; this answer needs nothing awaited"
    )]
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools.clone()))
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "rmcp's handler methods return futures; this answer needs nothing awaited"
    )]
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let error = self.error.clone();
        let client = context.client_info().map(|info| info.name);
        if let Some(asked) = self.log.asked(&request.name, client.as_deref()) {
            self.log.answered(&asked, &error);
        }
        Ok(error.into_result().into())
    }
}
