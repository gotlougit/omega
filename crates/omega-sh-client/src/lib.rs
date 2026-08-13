//! Client module for connecting to the omega-sh daemon.
//!
/// Re-exports `omega_core` so consumers can access `omega_core::core::ToolResult` etc.
pub use omega_core;
pub use omega_tools;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use omega_core::core::{ToolResult, ToolResultData};

// ---------------------------------------------------------------------------
// Wire protocol (mirrors the types in src/bin/omega-sh.rs)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct OmegaRequest {
    id: String,
    tool: String,
    args: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dir: Option<String>,
}

#[derive(Deserialize)]
struct OmegaResponse {
    id: String,
    result: OmegaToolResult,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum OmegaContent {
    Text {
        data: String,
    },
    Image {
        data: String,
        media_type: String,
    },
    Document {
        data: String,
        media_type: String,
        description: String,
    },
}

#[derive(Deserialize)]
struct OmegaToolResult {
    #[serde(flatten)]
    content: OmegaContent,
    is_error: bool,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// A client that connects to a running omega-sh daemon over a Unix socket.
#[derive(Clone)]
pub struct OmegaClient {
    socket_path: String,
    session: Option<String>,
    dir: Option<String>,
}

impl OmegaClient {
    /// Create a client using `OMEGA_SOCKET_PATH` env-var or the default path.
    pub fn new() -> Self {
        let socket_path =
            std::env::var("OMEGA_SOCKET_PATH").unwrap_or_else(|_| "/tmp/omega-sh.sock".to_string());
        Self {
            socket_path,
            session: None,
            dir: None,
        }
    }

    /// Create a client with an explicit socket path.
    pub fn with_socket(path: impl Into<String>) -> Self {
        Self {
            socket_path: path.into(),
            session: None,
            dir: None,
        }
    }

    /// Set an optional session identifier (included in omega-sh request logs).
    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }

    /// Set an optional working-directory hint (included in omega-sh request logs).
    pub fn with_dir(mut self, dir: impl Into<String>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    /// Send a tool request to omega-sh and wait for the response.
    pub async fn execute(&self, tool: &str, args: Value) -> Result<ToolResult, String> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| format!("Cannot connect to omega-sh at {}: {e}", self.socket_path))?;

        let (reader, mut writer) = stream.into_split();
        let id = uuid::Uuid::new_v4().to_string();

        let request = OmegaRequest {
            id: id.clone(),
            tool: tool.to_string(),
            args,
            session: self.session.clone(),
            dir: self.dir.clone(),
        };

        let mut buf = serde_json::to_vec(&request).map_err(|e| format!("Serialize error: {e}"))?;
        buf.push(b'\n');
        writer
            .write_all(&buf)
            .await
            .map_err(|e| format!("Write error: {e}"))?;
        writer
            .flush()
            .await
            .map_err(|e| format!("Flush error: {e}"))?;

        // Read the response line
        let mut lines = BufReader::new(reader).lines();
        let line = lines
            .next_line()
            .await
            .map_err(|e| format!("Read error: {e}"))?
            .ok_or_else(|| "Connection closed before response".to_string())?;

        let resp: OmegaResponse =
            serde_json::from_str(&line).map_err(|e| format!("Parse error: {e}"))?;

        if resp.id != id {
            return Err(format!("ID mismatch: sent {id}, got {}", resp.id));
        }

        Ok(convert_result(resp.result))
    }
}

impl Default for OmegaClient {
    fn default() -> Self {
        Self::new()
    }
}

fn convert_result(tr: OmegaToolResult) -> ToolResult {
    let content = match tr.content {
        OmegaContent::Text { data } => ToolResultData::Text(data),
        OmegaContent::Image { data, media_type } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&data)
                .unwrap_or_else(|_| Vec::new());
            ToolResultData::Image {
                data: bytes,
                media_type,
            }
        }
        OmegaContent::Document {
            data,
            media_type,
            description,
        } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&data)
                .unwrap_or_else(|_| Vec::new());
            ToolResultData::Document {
                data: bytes,
                media_type,
                description,
            }
        }
    };

    ToolResult {
        content,
        is_error: tr.is_error,
    }
}

// ---------------------------------------------------------------------------
// Proxy tool implementations
// ---------------------------------------------------------------------------

/// Proxy tool implementations that delegate to the omega-sh daemon.
// ---------------------------------------------------------------------------
// Generic proxy tool  –  forwards execution to omega-sh
// ---------------------------------------------------------------------------

pub mod proxy {
    use async_trait::async_trait;
    use serde_json::Value;
    use std::ops::Deref;
    use std::sync::LazyLock;

    use super::OmegaClient;
    use omega_core::core::{ToolInfo, ToolResult, ToolRuntime};
    use omega_tool_defs::ToolDef;
    use omega_tools::Tool;

    /// A tool whose execution is forwarded to a running `omega-sh` daemon.
    ///
    /// One instance handles any proxy-able tool — the name, description,
    /// and schema come from the canonical `ToolDef` in `omega-tool-defs`.
    pub struct ProxyTool {
        client: OmegaClient,
        def: &'static LazyLock<ToolDef>,
    }

    impl ProxyTool {
        pub fn new(client: OmegaClient, def: &'static LazyLock<ToolDef>) -> Self {
            Self { client, def }
        }
    }

    #[async_trait]
    impl Tool for ProxyTool {
        fn name(&self) -> &str {
            self.def.name
        }

        fn description(&self) -> &str {
            self.def.description
        }

        fn definition(&self) -> omega_llm::ToolDefinition {
            let def: &ToolDef = self.def.deref();
            omega_tools::def_to_tool_definition(def)
        }

        fn get_info(&self, _input: &Value) -> ToolInfo {
            ToolInfo {
                name: self.def.name.to_string(),
                action_description: String::new(),
                details: None,
            }
        }

        async fn execute(
            &self,
            input: &Value,
            _rt: &mut dyn ToolRuntime,
        ) -> anyhow::Result<ToolResult> {
            match self.client.execute(self.def.name, input.clone()).await {
                Ok(r) => Ok(r),
                Err(e) => Ok(ToolResult::error(e)),
            }
        }
    }
}

use omega_tools::ToolRegistry;

/// Register all proxy tools that delegate to an omega-sh daemon.
///
/// Iterates `omega_tool_defs::ALL` and registers every tool marked
/// `ToolKind::Proxy` as a generic `ProxyTool`.
pub fn register_proxy_tools(registry: &mut ToolRegistry, client: OmegaClient) {
    for &def in omega_tool_defs::ALL {
        if def.kind == omega_tool_defs::ToolKind::Proxy {
            registry.register(proxy::ProxyTool::new(client.clone(), def));
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use omega_tools::Tool;

    // -----------------------------------------------------------------------
    // register_proxy_tools
    // -----------------------------------------------------------------------

    /// register_proxy_tools registers all 6 proxy tools.
    #[test]
    fn test_register_proxy_tools_registers_all_proxies() {
        let mut registry = ToolRegistry::new();
        let client = OmegaClient::with_socket("/tmp/nonexistent-test-socket.sock");
        register_proxy_tools(&mut registry, client);

        for proxy_name in &["Bash", "Read", "Write", "Edit"] {
            assert!(
                registry.get(proxy_name).is_some(),
                "{} should be registered as a proxy tool",
                proxy_name
            );
        }
        assert_eq!(registry.len(), 4);
    }

    /// register_proxy_tools does NOT register native tools.
    #[test]
    fn test_register_proxy_tools_skips_native() {
        let mut registry = ToolRegistry::new();
        let client = OmegaClient::with_socket("/tmp/nonexistent-test-socket.sock");
        register_proxy_tools(&mut registry, client);

        assert!(
            registry.get("Transfer").is_none(),
            "Transfer is native and should not be in proxy registry"
        );
    }

    /// register_proxy_tools works with an empty registry.
    #[test]
    fn test_register_proxy_tools_onto_empty() {
        let mut registry = ToolRegistry::new();
        let client = OmegaClient::with_socket("/tmp/irrelevant.sock");
        assert!(registry.is_empty());
        register_proxy_tools(&mut registry, client);
        assert!(!registry.is_empty());
    }

    // -----------------------------------------------------------------------
    // ProxyTool unit tests
    // -----------------------------------------------------------------------

    /// ProxyTool::name() returns the canonical name.
    #[test]
    fn test_proxy_tool_name() {
        let client = OmegaClient::with_socket("/tmp/irrelevant.sock");
        let tool = proxy::ProxyTool::new(client, &omega_tool_defs::bash::DEF);
        assert_eq!(tool.name(), "Bash");
    }

    /// ProxyTool::description() returns the canonical description.
    #[test]
    fn test_proxy_tool_description() {
        let client = OmegaClient::with_socket("/tmp/irrelevant.sock");
        let tool = proxy::ProxyTool::new(client, &omega_tool_defs::read::DEF);
        assert_eq!(tool.description(), omega_tool_defs::read::DEF.description);
    }

    /// ProxyTool::get_info returns a stub with the tool name.
    #[test]
    fn test_proxy_tool_get_info() {
        let client = OmegaClient::with_socket("/tmp/irrelevant.sock");
        let tool = proxy::ProxyTool::new(client, &omega_tool_defs::bash::DEF);
        let info = tool.get_info(&serde_json::json!({}));
        assert_eq!(info.name, "Bash");
    }

    /// Two ProxyTools with different defs are independent.
    #[test]
    fn test_proxy_tool_independence() {
        let client = OmegaClient::with_socket("/tmp/irrelevant.sock");
        let bash = proxy::ProxyTool::new(client.clone(), &omega_tool_defs::bash::DEF);
        let read = proxy::ProxyTool::new(client, &omega_tool_defs::read::DEF);
        assert_eq!(bash.name(), "Bash");
        assert_eq!(read.name(), "Read");
        assert_ne!(bash.name(), read.name());
    }

    // -----------------------------------------------------------------------
    // Execute error handling (no real omega-sh running)
    // -----------------------------------------------------------------------

    /// Helper: a do-nothing ToolRuntime for execute tests.
    struct DummyRuntime;

    #[async_trait::async_trait]
    impl omega_core::core::ToolRuntime for DummyRuntime {
        fn send_output(&self, _chunk: omega_core::core::OutputChunk) {}

        fn is_interrupted(&self) -> bool {
            false
        }
    }

    /// ProxyTool::execute on a non-existent socket returns an error ToolResult.
    #[tokio::test]
    async fn test_proxy_tool_execute_no_daemon() {
        let client = OmegaClient::with_socket("/tmp/omega-sh-test-does-not-exist-382917.sock");
        let tool = proxy::ProxyTool::new(client, &omega_tool_defs::bash::DEF);
        let input = serde_json::json!({ "command": "echo hi" });
        let result = tool.execute(&input, &mut DummyRuntime).await.unwrap();
        assert!(
            result.is_error,
            "execute with no daemon should return error"
        );
    }

    /// Executing with no arguments still fails gracefully (not a panic).
    #[tokio::test]
    async fn test_proxy_tool_execute_empty_args() {
        let client = OmegaClient::with_socket("/tmp/omega-sh-test-empty-291837.sock");
        let tool = proxy::ProxyTool::new(client, &omega_tool_defs::bash::DEF);
        let result = tool
            .execute(&serde_json::json!({}), &mut DummyRuntime)
            .await
            .unwrap();
        assert!(result.is_error, "should error, not panic");
    }
}
