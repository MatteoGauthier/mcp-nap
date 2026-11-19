//! A small MCP server on the official rmcp SDK, used by the integration tests.
//!
//! It appends `start <pid>` and each handled method to `FAKE_SERVER_LOG`,
//! lists the tools named in `FAKE_SERVER_TOOLS` and the prompts named in `FAKE_SERVER_PROMPTS`,
//! gives its discover and list results `FAKE_SERVER_TTL_MS` as a public `ttlMs` on 2026-07-28,
//! and exits when the `crash` tool is called.

use std::env;
use std::fs::OpenOptions;
use std::io::Write;

use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    DiscoverResult, JsonObject, ListPromptsResult, ListToolsResult, PaginatedRequestParams, Prompt,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};

fn record(entry: &str) {
    if let Ok(path) = env::var("FAKE_SERVER_LOG") {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{entry}").unwrap();
    }
}

fn names(variable: &str, default: &str) -> Vec<String> {
    env::var(variable)
        .unwrap_or_else(|_| default.into())
        .split(',')
        .map(str::to_owned)
        .collect()
}

fn ttl_ms() -> Option<u64> {
    env::var("FAKE_SERVER_TTL_MS")
        .ok()
        .and_then(|ttl| ttl.parse().ok())
}

struct FakeServer;

impl ServerHandler for FakeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_prompts()
                .enable_prompts_list_changed()
                .build(),
        )
    }

    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        record("server/discover");
        let mut result = DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        );
        if let Some(ttl_ms) = ttl_ms() {
            result.ttl_ms = ttl_ms;
            result.cache_scope = CacheScope::Public;
        }
        Ok(result)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        record("tools/list");
        let tools = names("FAKE_SERVER_TOOLS", "echo")
            .into_iter()
            .map(|name| Tool::new(name, "a test tool", JsonObject::new()))
            .collect();
        let mut result = ListToolsResult::with_all_items(tools);
        if let Some(ttl_ms) = ttl_ms() {
            result.ttl_ms = Some(ttl_ms);
            result.cache_scope = Some(CacheScope::Public);
        }
        Ok(result)
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        record("prompts/list");
        let prompts = names("FAKE_SERVER_PROMPTS", "greet")
            .into_iter()
            .map(|name| Prompt::new(name, None::<String>, None))
            .collect();
        let mut result = ListPromptsResult::with_all_items(prompts);
        if let Some(ttl_ms) = ttl_ms() {
            result.ttl_ms = Some(ttl_ms);
            result.cache_scope = Some(CacheScope::Public);
        }
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        record("tools/call");
        if request.name == "crash" {
            std::process::exit(1);
        }
        let text = format!("called {}", request.name);
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    record(&format!("start {}", std::process::id()));
    let service = FakeServer
        .serve(rmcp::transport::stdio())
        .await
        .expect("the fake server starts");
    let _ = service.waiting().await;
}
