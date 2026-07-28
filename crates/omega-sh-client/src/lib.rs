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
pub mod proxy {
    use async_trait::async_trait;
    use serde_json::Value;

    use super::OmegaClient;
    use omega_core::core::{ToolInfo, ToolResult, ToolRuntime};
    use omega_tools::Tool;
    use omega_llm::{types::CustomTool, ToolDefinition};

    macro_rules! proxy_tool {
        ($name:ident, $tool_name:expr, $description:expr, $schema_json:expr) => {
            pub struct $name {
                client: OmegaClient,
            }

            impl $name {
                pub fn new(client: OmegaClient) -> Self {
                    Self { client }
                }
            }

            #[async_trait]
            impl Tool for $name {
                fn name(&self) -> &str {
                    $tool_name
                }

                fn description(&self) -> &str {
                    $description
                }

                fn definition(&self) -> ToolDefinition {
                    ToolDefinition::Custom(CustomTool {
                        name: $tool_name.to_string(),
                        description: Some($description.to_string()),
                        input_schema: serde_json::from_value($schema_json)
                            .expect(concat!("invalid ToolInputSchema JSON for ", $tool_name)),
                        tool_type: None,
                        cache_control: None,
                    })
                }

                fn get_info(&self, _input: &Value) -> ToolInfo {
                    ToolInfo {
                        name: $tool_name.to_string(),
                        action_description: String::new(),
                        details: None,
                    }
                }

                async fn execute(
                    &self,
                    input: &Value,
                    _rt: &mut dyn ToolRuntime,
                ) -> anyhow::Result<ToolResult> {
                    match self.client.execute($tool_name, input.clone()).await {
                        Ok(r) => Ok(r),
                        Err(e) => Ok(ToolResult::error(e)),
                    }
                }
            }
        };
    }

    proxy_tool!(
        ReadProxy,
        "Read",
        "Read a file from the local filesystem. Supports text files, images (PNG, JPEG, GIF, WebP), and PDFs.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to read"
                },
                "offset": {
                    "type": "number",
                    "description": "The line number to start reading from (1-indexed)"
                },
                "limit": {
                    "type": "number",
                    "description": "The number of lines to read"
                }
            },
            "required": ["file_path"]
        })
    );

    proxy_tool!(
        WriteProxy,
        "Write",
        "Write content to a file on the local filesystem.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to write (must be absolute, not relative)"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            },
            "required": ["file_path", "content"]
        })
    );

    proxy_tool!(
        EditProxy,
        "Edit",
        "Perform exact string replacements in files.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to modify"
                },
                "old_string": {
                    "type": "string",
                    "description": "The text to replace"
                },
                "new_string": {
                    "type": "string",
                    "description": "The text to replace it with (must be different from old_string)"
                },
                "replace_all": {
                    "type": "boolean",
                    "default": false,
                    "description": "Replace all occurrences of old_string (default false)"
                }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    );

    proxy_tool!(
        BashProxy,
        "Bash",
        "Execute a bash command in the shell. Use for terminal operations like git, npm, docker, etc.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The command to execute"
                },
                "timeout": {
                    "type": "number",
                    "description": "Optional timeout in milliseconds (max 600000). Default is 120000ms (2 minutes)."
                },
                "description": {
                    "type": "string",
                    "description": "Clear, concise description of what this command does in 5-10 words, in active voice."
                }
            },
            "required": ["command"]
        })
    );

    proxy_tool!(
        GlobProxy,
        "Glob",
        "Fast file pattern matching tool. Supports glob patterns like **/*.js or src/**/*.ts.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The glob pattern to match files against"
                },
                "path": {
                    "type": "string",
                    "description": "The directory to search in. If not specified, uses the daemon's current working directory."
                }
            },
            "required": ["pattern"]
        })
    );

    proxy_tool!(
        GrepProxy,
        "Grep",
        "Search file contents using regex patterns. Uses ripgrep for fast searching.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The regular expression pattern to search for in file contents"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in. Defaults to daemon's working directory."
                },
                "glob": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\")"
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output mode: 'content', 'files_with_matches' (default), or 'count'"
                },
                "-B": {
                    "type": "number",
                    "description": "Number of lines to show before each match"
                },
                "-A": {
                    "type": "number",
                    "description": "Number of lines to show after each match"
                },
                "-C": {
                    "type": "number",
                    "description": "Number of lines to show before and after each match"
                },
                "-n": {
                    "type": "boolean",
                    "description": "Show line numbers in output. Defaults to true."
                },
                "-i": {
                    "type": "boolean",
                    "description": "Case insensitive search"
                },
                "type": {
                    "type": "string",
                    "description": "File type to search (e.g. 'js', 'py', 'rust')"
                },
                "head_limit": {
                    "type": "number",
                    "description": "Limit output to first N lines/entries"
                },
                "offset": {
                    "type": "number",
                    "description": "Skip first N lines/entries"
                },
                "multiline": {
                    "type": "boolean",
                    "description": "Enable multiline mode where . matches newlines"
                }
            },
            "required": ["pattern"]
        })
    );
}
