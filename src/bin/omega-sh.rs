//! omega-sh — A persistent Unix-socket daemon for filesystem/shell tools.
//!
//! Handles Read, Write, Edit, and Bash tool requests from one or more
//! picrust agent processes.  Protocol is newline-delimited JSON over a
//! Unix stream socket.
//!
//! ## Protocol
//!
//! Request (one line):
//! ```json
//! {"id":"<uuid>","tool":"Read","args":{"file_path":"..."}}
//! ```
//!
//! Response (one line):
//! ```json
//! {"id":"<uuid>","result":{"content":{"type":"Text","data":"..."},"is_error":false}}
//! ```
//!
//! ## Environment
//!
//! - `OMEGA_SOCKET_PATH` — path to the Unix socket (default: /tmp/omega-sh.sock)

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::time::timeout;
use tracing_subscriber::EnvFilter;

// ---------------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct OmegaRequest {
    id: String,
    tool: String,
    args: Value,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

#[derive(Debug, Serialize)]
struct OmegaResponse {
    id: String,
    result: OmegaToolResult,
}

#[derive(Debug, Serialize)]
struct OmegaToolResult {
    #[serde(flatten)]
    content: OmegaContent,
    is_error: bool,
}

/// Serializable form of ToolResultData — binary payloads are base64-encoded.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum OmegaContent {
    Text { data: String },
    Image { data: String, media_type: String },
    Document {
        data: String,
        media_type: String,
        description: String,
    },
}

/// Extract the most interesting parameter from tool args for logging.
fn log_param(tool: &str, args: &Value) -> String {
    match tool {
        "Bash" => args
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| {
                let truncated: String = c.chars().take(100).collect();
                if c.len() > 100 {
                    format!("{truncated}…")
                } else {
                    truncated
                }
            })
            .unwrap_or_default(),
        "Read" | "Write" | "Edit" => args
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        "Glob" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        "Grep" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Tool implementations (self-contained, no AgentInternals dependency)
// ---------------------------------------------------------------------------

mod tools {
    use super::*;

    const DEFAULT_TIMEOUT_MS: u64 = 120_000;
    const MAX_TIMEOUT_MS: u64 = 600_000;
    const MAX_OUTPUT_LENGTH: usize = 30_000;

    /// Execute a shell command, return (combined_stdout_stderr, exit_code).
    pub async fn bash(args: &Value) -> OmegaToolResult {
        let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let timeout_ms = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        let duration = Duration::from_millis(timeout_ms);

        let child = Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let output = match child {
            Ok(c) => match timeout(duration, c.wait_with_output()).await {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => {
                    return err(format!("Failed to read output: {e}"));
                }
                Err(_) => {
                    return err(format!("Command timed out after {timeout_ms}ms"));
                }
            },
            Err(e) => {
                return err(format!("Failed to spawn shell: {e}"));
            }
        };

        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let mut result = String::new();
        let stdout = stdout.trim();
        let stderr = stderr.trim();
        if !stdout.is_empty() {
            result.push_str(stdout);
        }
        if !stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str("STDERR:\n");
            result.push_str(stderr);
        }
        if result.len() > MAX_OUTPUT_LENGTH {
            result.truncate(MAX_OUTPUT_LENGTH);
            result.push_str("\n... (output truncated)");
        }

        if exit_code == 0 {
            if result.is_empty() {
                ok_text("Command completed successfully (no output)")
            } else {
                ok_text(&result)
            }
        } else {
            err(format!("Command failed with exit code {exit_code}\n{result}"))
        }
    }

    /// Read a file — dispatches by extension.
    pub async fn read(args: &Value) -> OmegaToolResult {
        let file_path = args
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if file_path.is_empty() {
            return err("Missing required field: file_path");
        }

        let offset = args.get("offset").and_then(|v| v.as_u64()).map(|v| v as usize);
        let limit = args.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);

        let path = Path::new(file_path);

        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_lowercase())
            .as_deref()
        {
            Some("png") | Some("jpg") | Some("jpeg") | Some("gif") | Some("webp") => {
                read_image(path)
            }
            Some("pdf") => read_pdf(path),
            _ => read_text(path, offset, limit),
        }
    }

    fn read_text(path: &Path, offset: Option<usize>, limit: Option<usize>) -> OmegaToolResult {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => return err(format!("Failed to read file: {e}")),
        };

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let start = offset.unwrap_or(1).saturating_sub(1);
        let count = limit.unwrap_or(2000);
        let end = (start + count).min(total);

        if start >= total {
            return ok_text(format!(
                "File has {total} lines. Requested offset {} is out of range.",
                start + 1
            ));
        }

        let mut out = format!("File: {}\n\n", path.display());
        for (i, line) in lines[start..end].iter().enumerate() {
            let line_num = start + i + 1;
            let display = if line.len() > 2000 {
                format!("{}...", &line[..2000])
            } else {
                line.to_string()
            };
            out.push_str(&format!("{line_num:>6}\t{display}\n"));
        }
        if end < total {
            out.push_str(&format!(
                "\n... ({} more lines, use offset and limit to read more)\n",
                total - end
            ));
        }
        ok_text(out)
    }

    fn read_image(path: &Path) -> OmegaToolResult {
        let max = 5 * 1024 * 1024;
        let data = match checked_read(path, max) {
            Ok(d) => d,
            Err(e) => return err(e),
        };
        let media_type = match path.extension().and_then(|e| e.to_str()) {
            Some("png") => "image/png",
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            _ => "application/octet-stream",
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
        OmegaToolResult {
            content: OmegaContent::Image {
                data: b64,
                media_type: media_type.to_string(),
            },
            is_error: false,
        }
    }

    fn read_pdf(path: &Path) -> OmegaToolResult {
        let max = 32 * 1024 * 1024;
        let data = match checked_read(path, max) {
            Ok(d) => d,
            Err(e) => return err(e),
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
        OmegaToolResult {
            content: OmegaContent::Document {
                data: b64,
                media_type: "application/pdf".to_string(),
                description: format!("PDF: {}", path.display()),
            },
            is_error: false,
        }
    }

    fn checked_read(path: &Path, max_size: u64) -> std::result::Result<Vec<u8>, String> {
        let meta = std::fs::metadata(path).map_err(|e| format!("Cannot access {path:?}: {e}"))?;
        if meta.len() > max_size {
            return Err(format!(
                "File too large: {} bytes (max: {max_size} bytes)",
                meta.len()
            ));
        }
        std::fs::read(path).map_err(|e| format!("Failed to read {path:?}: {e}"))
    }

    /// Write content to a file, creating parent directories as needed.
    pub async fn write(args: &Value) -> OmegaToolResult {
        let file_path = args
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if file_path.is_empty() {
            return err("Missing required field: file_path");
        }

        let path = Path::new(file_path);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return err(format!("Failed to create parent directories: {e}"));
                }
            }
        }

        let existed = path.exists();
        match std::fs::write(path, content) {
            Ok(()) => {
                if existed {
                    ok_text(format!("File updated successfully: {file_path}"))
                } else {
                    ok_text(format!("File created successfully: {file_path}"))
                }
            }
            Err(e) => err(format!("Failed to write file: {e}")),
        }
    }

    /// Exact-string replacement in a file.
    pub async fn edit(args: &Value) -> OmegaToolResult {
        let file_path = args
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let old_string = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let new_string = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if file_path.is_empty() || old_string.is_empty() {
            return err("Missing required fields: file_path, old_string");
        }
        if old_string == new_string {
            return err("old_string and new_string must be different");
        }

        let path = Path::new(file_path);
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => return err(format!("Failed to read file: {e}")),
        };

        let occurrences = content.matches(old_string).count();
        if occurrences == 0 {
            return err(
                "String not found in file. Make sure to include exact text including whitespace.",
            );
        }
        if !replace_all && occurrences > 1 {
            return err(format!(
                "Found {occurrences} occurrences of the string. Either provide a more specific string \
                 to ensure only one match, or use replace_all: true to change every instance."
            ));
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        match std::fs::write(path, &new_content) {
            Ok(()) => {
                if replace_all {
                    ok_text(format!(
                        "Successfully replaced {occurrences} occurrences in {file_path}"
                    ))
                } else {
                    ok_text(format!("Successfully replaced text in {file_path}"))
                }
            }
            Err(e) => err(format!("Failed to write file: {e}")),
        }
    }

    /// Search files by glob pattern (sorted by mtime, newest first).
    pub async fn glob(args: &Value) -> OmegaToolResult {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if pattern.is_empty() {
            return err("Missing required field: pattern");
        }

        let search_dir = args.get("path").and_then(|v| v.as_str());
        let base_dir = std::env::current_dir()
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_default();
        let base = search_dir.unwrap_or(&base_dir);

        let full_pattern = if std::path::Path::new(pattern).is_absolute() {
            pattern.to_string()
        } else {
            format!("{base}/{pattern}")
        };

        let mut entries: Vec<(String, std::time::SystemTime)> = match glob::glob(&full_pattern) {
            Ok(glob_iter) => glob_iter
                .filter_map(|entry| entry.ok())
                .filter_map(|path| {
                    let mtime = path.metadata().ok()?.modified().ok()?;
                    let display = path
                        .to_string_lossy()
                        .to_string();
                    Some((display, mtime))
                })
                .collect(),
            Err(e) => return err(format!("Invalid glob pattern: {e}")),
        };

        // Sort by mtime descending
        entries.sort_by(|a, b| b.1.cmp(&a.1));

        if entries.is_empty() {
            return ok_text(format!("No files found matching pattern: {pattern}"));
        }

        let mut result = format!(
            "Found {} files matching '{pattern}':\n",
            entries.len()
        );
        for (path, _) in entries.iter().take(100) {
            result.push_str(&format!("{path}\n"));
        }
        if entries.len() > 100 {
            result.push_str(&format!("... and {} more\n", entries.len() - 100));
        }
        ok_text(result)
    }

    /// Search file contents with ripgrep.
    pub async fn grep(args: &Value) -> OmegaToolResult {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if pattern.is_empty() {
            return err("Missing required field: pattern");
        }

        let search_path = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|d| d.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string())
            });

        let output_mode = args
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("files_with_matches");
        let glob_filter = args.get("glob").and_then(|v| v.as_str());
        let before_ctx = args.get("-B").and_then(|v| v.as_u64());
        let after_ctx = args.get("-A").and_then(|v| v.as_u64());
        let ctx = args.get("-C").and_then(|v| v.as_u64());
        let line_numbers = args.get("-n").and_then(|v| v.as_bool()).unwrap_or(true);
        let case_insensitive = args.get("-i").and_then(|v| v.as_bool()).unwrap_or(false);
        let file_type = args.get("type").and_then(|v| v.as_str());
        let multiline = args.get("multiline").and_then(|v| v.as_bool()).unwrap_or(false);
        let head_limit = args.get("head_limit").and_then(|v| v.as_u64()).map(|v| v as usize);
        let offset = args.get("offset").and_then(|v| v.as_u64()).map(|v| v as usize);

        let mut cmd = Command::new("rg");
        cmd.arg(pattern).arg(&search_path);

        match output_mode {
            "content" => {
                if line_numbers {
                    cmd.arg("-n");
                }
                if let Some(b) = before_ctx {
                    cmd.arg("-B").arg(b.to_string());
                }
                if let Some(a) = after_ctx {
                    cmd.arg("-A").arg(a.to_string());
                }
                if let Some(c) = ctx {
                    cmd.arg("-C").arg(c.to_string());
                }
            }
            "count" => {
                cmd.arg("-c");
            }
            _ => {
                // files_with_matches (default)
                cmd.arg("-l");
            }
        }

        if case_insensitive {
            cmd.arg("-i");
        }
        if let Some(ft) = file_type {
            cmd.arg("--type").arg(ft);
        }
        if let Some(g) = glob_filter {
            cmd.arg("--glob").arg(g);
        }
        if multiline {
            cmd.arg("-U").arg("--multiline-dotall");
        }
        cmd.arg("--color=never");

        let output = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output().await {
            Ok(o) => o,
            Err(e) => return err(format!("Failed to run ripgrep: {e}")),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() && stdout.is_empty() {
            if stderr.contains("No such file or directory") {
                return ok_text(format!("Path not found: {search_path}"));
            }
            if output.status.code() == Some(1) {
                // rg exit 1 = no matches
                return ok_text(format!("No matches found for pattern: {pattern}"));
            }
            return ok_text(format!("Search error: {stderr}"));
        }

        let mut lines: Vec<&str> = stdout.lines().collect();

        if let Some(off) = offset {
            if off < lines.len() {
                lines = lines[off..].to_vec();
            } else {
                lines.clear();
            }
        }
        if let Some(lim) = head_limit {
            lines.truncate(lim);
        }

        ok_text(lines.join("\n"))
    }

    // -- helpers -----------------------------------------------------------

    fn ok_text(t: impl Into<String>) -> OmegaToolResult {
        OmegaToolResult {
            content: OmegaContent::Text {
                data: t.into(),
            },
            is_error: false,
        }
    }

    fn err(m: impl Into<String>) -> OmegaToolResult {
        OmegaToolResult {
            content: OmegaContent::Text {
                data: m.into(),
            },
            is_error: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let socket_path = std::env::var("OMEGA_SOCKET_PATH")
        .unwrap_or_else(|_| "/tmp/omega-sh.sock".to_string());

    // Clean up leftover socket
    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("Cannot bind to {socket_path}"))?;

    tracing::info!(socket = %socket_path, "omega-sh started");
    eprintln!("omega-sh listening on {socket_path}");

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                tracing::debug!(peer = ?addr, "accepted connection");
                tokio::spawn(handle_connection(stream));
            }
            Err(e) => {
                tracing::error!("accept error: {e}");
                // Keep going on transient errors
            }
        }
    }
}

async fn handle_connection(stream: UnixStream) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let request: OmegaRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let err_resp = OmegaResponse {
                    id: "unknown".to_string(),
                    result: OmegaToolResult {
                        content: OmegaContent::Text {
                            data: format!("Invalid JSON: {e}"),
                        },
                        is_error: true,
                    },
                };
                let _ = write_response(&mut writer, &err_resp).await;
                continue;
            }
        };

        // --- logging ---
        let param = log_param(&request.tool, &request.args);
        let session = request.session.as_deref().unwrap_or("-");
        let dir = request.dir.as_deref().unwrap_or("-");
        tracing::info!(
            session = %session,
            dir = %dir,
            tool = %request.tool,
            param = %param,
            "request"
        );

        let result = match request.tool.as_str() {
            "Bash" => tools::bash(&request.args).await,
            "Read" => tools::read(&request.args).await,
            "Write" => tools::write(&request.args).await,
            "Edit" => tools::edit(&request.args).await,
            "Glob" => tools::glob(&request.args).await,
            "Grep" => tools::grep(&request.args).await,
            other => OmegaToolResult {
                content: OmegaContent::Text {
                    data: format!("Unknown tool: {other}"),
                },
                is_error: true,
            },
        };

        let response = OmegaResponse {
            id: request.id,
            result,
        };

        if let Err(e) = write_response(&mut writer, &response).await {
            tracing::warn!("write response failed: {e}");
            break;
        }
    }
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &OmegaResponse,
) -> Result<()> {
    let mut buf = serde_json::to_vec(response)?;
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}
