//! # omega-loop — Core agent daemon
//!
//! Listens on a Unix socket, accepts connections from UI processes, and
//! manages agent sessions.  Each session runs the full agent loop (LLM +
//! tool orchestration) and farms out shell/filesystem operations to the
//! omega-sh daemon.
//!
//! ## Protocol (newline-delimited JSON)
//!
//! **Client → Server**
//! ```json
//! {"type":"run","session_id":"...","content":"...","config":{"stream":true,"think":false}}
//! ```
//!
//! **Server → Client**
//! ```json
//! {"type":"Created","session_id":"...","session_name":"..."}
//! {"type":"Chunk","session_id":"...","chunk":<OutputChunk>}
//! ```

use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::StreamExt;
use omega_projects::{ActiveProject, ProjectManager};
#[cfg(test)]
use omega_projects::ProjectInfo;
use omega_protocol::daemon::{ClientRequest, ServerEvent, WireChunk};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, Mutex};
use tracing_subscriber::EnvFilter;

mod agent;
mod helpers;
mod rebase_job;
mod runtime;
mod session;

use omega_core::core::{RoleInfo, SessionInfo, ToolInfo, ToolResult, ToolRuntime};
#[cfg(test)]
use omega_core::core::OutputChunk;
use omega_llm::types::CustomTool;
use omega_llm::{
    ContentBlock, ContentDelta, LlmProvider, Message, MessageContent, OpenAIProvider, StreamEvent,
    SystemPrompt, ToolChoice, ToolDefinition, ToolInputSchema,
};
use omega_tools::{Tool, ToolRegistry};

use crate::agent::{AgentConfig, StandardAgent};
use crate::runtime::{AgentHandle, AgentRuntime};
use crate::session::{AgentSession, SessionStorage};
use omega_sh_client::OmegaClient;

// ---------------------------------------------------------------------------
// History replay + session listing helpers
// ---------------------------------------------------------------------------

/// Marker text the agent inserts into history when a turn is interrupted.
const INTERRUPT_MARKER: &str =
    "<vibe-working-agent-system>User interrupted this message</vibe-working-agent-system>";

/// A normalized, replayable message extracted from a session's history.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HistoryReplay {
    role: String,
    content: String,
}

/// Extract the user-visible text of a message.
///
/// Block messages (tool results, thinking, images) contribute only their
/// `Text` blocks — everything else is skipped so the client never renders
/// raw tool plumbing as a chat turn.
fn message_text(msg: &Message) -> String {
    match &msg.content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Normalize a session's persisted history into replayable turns.
///
/// Filters out the rows that would render as confusing or empty chat:
/// - empty user turns (e.g. tool-result messages, which are `user` role
///   but contain no visible text),
/// - assistant interrupt markers inserted when a user stopped a turn.
fn history_to_replay(messages: &[Message]) -> Vec<HistoryReplay> {
    let mut out = Vec::new();
    for msg in messages {
        if msg.role != "user" && msg.role != "assistant" {
            continue;
        }
        let content = message_text(msg);
        if content.trim().is_empty() {
            continue;
        }
        if content.trim() == INTERRUPT_MARKER {
            continue;
        }
        out.push(HistoryReplay {
            role: msg.role.clone(),
            content: content.trim().to_string(),
        });
    }
    out
}

/// Truncate `text` to at most `max` characters, appending an ellipsis when
/// cut. Used for the preview strings embedded in [`SessionInfo`].
fn preview(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    } else {
        s.to_string()
    }
}

/// Build a `SessionInfo` for a stored session, including short previews of
/// its first user prompt and last meaningful message, plus the full replayed
/// text used for search.
///
/// Returns `(info, search_blob)` where `search_blob` is every replayable
/// message joined together, so a query can match any point in the chat — not
/// just the last line.
fn build_session_info(
    storage: &crate::session::SessionStorage,
    session_id: &str,
    metadata: &crate::session::metadata::SessionMetadata,
) -> (SessionInfo, String) {
    let messages = storage.load_messages(session_id).unwrap_or_default();
    let replay = history_to_replay(&messages);
    let message_count = replay.len();
    let last_message = replay.last().map(|r| preview(&r.content, 80));
    let first_user_message = replay
        .iter()
        .find(|r| r.role == "user")
        .map(|r| preview(&r.content, 80));
    let search_blob = replay
        .iter()
        .map(|r| r.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    (
        SessionInfo {
            session_id: session_id.to_string(),
            name: metadata.name.clone(),
            conversation_name: metadata.conversation_name.clone(),
            created_at: metadata.created_at.to_rfc3339(),
            updated_at: metadata.updated_at.to_rfc3339(),
            message_count,
            last_message,
            first_user_message,
        },
        search_blob,
    )
}

/// List top-level sessions with metadata, most-recently-updated first, with
/// an optional case-insensitive search filter.
fn list_sessions_filtered(
    storage: &crate::session::SessionStorage,
    query: Option<&str>,
) -> Vec<SessionInfo> {
    let mut sessions = match storage.list_sessions_with_metadata(true) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("list_sessions error: {e}");
            return Vec::new();
        }
    };

    // Nearest first — most recently updated at the top.
    sessions.sort_by_key(|(_, meta)| std::cmp::Reverse(meta.updated_at));
    let q = query.map(str::trim).filter(|q| !q.is_empty());
    let q = q.map(|q| q.to_lowercase());
    sessions
        .into_iter()
        .map(|(sid, meta)| build_session_info(storage, &sid, &meta))
        // Drop empty sessions entirely: only show chats where the user
        // actually prompted something. This also keeps the search from
        // matching session ids/names of chats that have no conversation.
        .filter(|(info, _)| info.first_user_message.is_some())
        .filter(|(info, blob)| match &q {
            Some(q) => blob.to_lowercase().contains(q) || info.matches_query(q),
            None => true,
        })
        .map(|(info, _)| info)
        .collect()
}

// ---------------------------------------------------------------------------
// Shared infrastructure
// ---------------------------------------------------------------------------

fn create_llm_provider() -> Result<Arc<dyn LlmProvider>> {
    Ok(Arc::new(OpenAIProvider::from_env_with_dynamic_auth()?))
}

fn create_tools() -> Result<Arc<ToolRegistry>> {
    let mut registry = ToolRegistry::new();
    let omega = OmegaClient::new()
        .with_session("omega-loop".to_string())
        .with_dir(
            env::current_dir()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_else(|_| "?".to_string()),
        );

    omega_sh_client::register_proxy_tools(&mut registry, omega);
    omega_tools::register_default_tools(&mut registry);

    Ok(Arc::new(registry))
}

// ---------------------------------------------------------------------------
// Project support — per-session git worktrees
// ---------------------------------------------------------------------------

/// Custom-metadata key where a session's active project is persisted, so a
/// resumed session is re-bound to its worktree across daemon restarts.
const META_ACTIVE_PROJECT: &str = "active_project";

/// Build a tool registry whose proxy tools run inside `dir` — used for
/// project-bound sessions so every Bash/Read/Write/Edit executes in the
/// session's git worktree. Project-bound sessions also get the native
/// `RenameWorktree` tool so the agent can give the worktree a descriptive
/// name once its work is done.
pub(crate) fn create_tools_for_dir(
    dir: &Path,
    session_id: &str,
    mut rename_tool: RenameWorktreeTool,
) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    let omega = OmegaClient::new()
        .with_session(session_id.to_string())
        .with_dir(dir.display().to_string());
    rename_tool.omega_client = Some(omega.clone());
    omega_sh_client::register_proxy_tools(&mut registry, omega);
    omega_tools::register_default_tools(&mut registry);
    registry.register(rename_tool);
    Arc::new(registry)
}

/// Pick the tool registry for a session: the project worktree's registry
/// when a project is active, otherwise the daemon-wide default.
fn tools_for_session(
    default: &Arc<ToolRegistry>,
    active: Option<&ActiveProject>,
    session_id: &str,
    rename_tool: RenameWorktreeTool,
) -> Arc<ToolRegistry> {
    match active {
        Some(active) => {
            create_tools_for_dir(Path::new(&active.worktree_path), session_id, rename_tool)
        }
        None => default.clone(),
    }
}

/// Native tool that renames the session's git worktree once the work is
/// done, so the worktree (and its branch) can be easily identified later.
///
/// Renaming moves the checkout directory and renames the branch, then keeps
/// the daemon's view consistent: the live session binding, the persisted
/// session metadata (source of truth for resume + the git-host session
/// index) and connected clients (via `ProjectActive`) all see the new
/// path/branch. The shared omega-sh client is retargeted as part of the same
/// operation, so already-registered Bash/Read/Write/Edit tools follow the
/// moved checkout immediately.
#[derive(Clone)]
pub(crate) struct RenameWorktreeTool {
    projects: Arc<ProjectManager>,
    session_id: String,
    session_projects: Arc<Mutex<HashMap<String, ActiveProject>>>,
    session_storage: Arc<crate::session::SessionStorage>,
    event_tx: broadcast::Sender<ServerEvent>,
    /// Client shared by every omega-sh proxy in this session's registry.
    /// Metadata-only unit tests may leave it unbound.
    omega_client: Option<OmegaClient>,
}

/// Build a [`RenameWorktreeTool`] for a session, wiring it to the daemon
/// state the rename must keep consistent (live binding, persisted metadata,
/// connected clients).
pub(crate) fn rename_worktree_tool(
    projects: &Arc<ProjectManager>,
    session_id: &str,
    session_projects: &Arc<Mutex<HashMap<String, ActiveProject>>>,
    session_storage: &Arc<crate::session::SessionStorage>,
    event_tx: &broadcast::Sender<ServerEvent>,
) -> RenameWorktreeTool {
    RenameWorktreeTool {
        projects: Arc::clone(projects),
        session_id: session_id.to_string(),
        session_projects: Arc::clone(session_projects),
        session_storage: Arc::clone(session_storage),
        event_tx: event_tx.clone(),
        omega_client: None,
    }
}

/// Shared description used for the tool definition and the system prompt.
const RENAME_WORKTREE_DESCRIPTION: &str =
    "Rename this session's git worktree to a short, descriptive name so it can be easily \
     identified later (e.g. 'fix-tui-crash' or 'add-billing-api'). Use this once, as the final \
     step, once the task is complete and the work has been committed.";

#[async_trait::async_trait]
impl Tool for RenameWorktreeTool {
    fn name(&self) -> &str {
        "RenameWorktree"
    }

    fn description(&self) -> &str {
        RENAME_WORKTREE_DESCRIPTION
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::Custom(CustomTool {
            name: self.name().to_string(),
            description: Some(self.description().to_string()),
            input_schema: ToolInputSchema::new()
                .with_properties(serde_json::json!({
                    "name": {
                        "type": "string",
                        "description": "Short descriptive name for the worktree \
                                       (e.g. 'fix-tui-crash'); spaces and special \
                                       characters are converted to dashes"
                    }
                }))
                .with_required(vec!["name".to_string()]),
            tool_type: None,
            cache_control: None,
        })
    }

    fn get_info(&self, input: &Value) -> ToolInfo {
        ToolInfo {
            name: self.name().to_string(),
            action_description: "Rename the session's git worktree".to_string(),
            details: input
                .get("name")
                .and_then(|v| v.as_str())
                .map(|n| format!("rename worktree to '{n}'")),
        }
    }

    async fn execute(&self, input: &Value, rt: &mut dyn ToolRuntime) -> Result<ToolResult> {
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(name) = name else {
            return Ok(ToolResult::error(
                "RenameWorktree requires a non-empty 'name' argument",
            ));
        };

        // The live binding for this session — the daemon keeps it in sync
        // with the persisted metadata.
        let current = {
            let map = self.session_projects.lock().await;
            map.get(&self.session_id).cloned()
        };
        let Some(current) = current else {
            return Ok(ToolResult::error(
                "no active project worktree for this session",
            ));
        };

        let renamed = match self.projects.rename_worktree(&current, name).await {
            Ok(renamed) => renamed,
            Err(e) => {
                return Ok(ToolResult::error(format!("worktree rename failed: {e:#}")));
            }
        };

        // Proxy tools are long-lived and were created with the old checkout
        // path. Their cloned OmegaClients share one directory binding, so
        // this makes the next Bash/Read/Write/Edit call use the moved tree.
        if let Some(client) = &self.omega_client {
            client.set_dir(renamed.worktree_path.clone());
        }

        // Keep every view of the binding consistent: the per-connection map,
        // the persisted session metadata (source of truth for resume and the
        // git-host session index), and connected clients.
        self.session_projects
            .lock()
            .await
            .insert(self.session_id.clone(), renamed.clone());
        persist_active_project(&self.session_storage, &self.session_id, &renamed);
        // Keep the running agent's in-memory metadata in sync with what we
        // just persisted. Otherwise the next message the agent emits re-saves
        // its (stale) pre-rename binding over the new one, and the web UI
        // keeps showing the old worktree/branch.
        if let Ok(value) = serde_json::to_value(&renamed) {
            rt.set_session_meta(META_ACTIVE_PROJECT, value);
        }

        let _ = self.event_tx.send(ServerEvent::ProjectActive {
            project: renamed.project.clone(),
            worktree_path: renamed.worktree_path.clone(),
            branch: renamed.branch.clone(),
        });

        rt.send_text(&format!(
            "Worktree renamed to '{}' — new path: {} (branch {})",
            name, renamed.worktree_path, renamed.branch
        ));

        Ok(ToolResult::success(format!(
            "Renamed worktree to '{}'.\nNew path: {}\nNew branch: {}",
            name, renamed.worktree_path, renamed.branch
        )))
    }
}

/// System-prompt context injected into project-bound sessions, giving the
/// agent immediate context about the repository it is working in — that its
/// checkout must be kept up to date with upstream, and the instruction to
/// rename the worktree once the work is done so it can be easily identified
/// later.
///
/// `update_instruction` is the (configurable) "keep your checkout up to
/// date with upstream" paragraph; `{branch}` is replaced with the project's
/// default branch when present.
fn project_system_context(active: &ActiveProject, update_instruction: &str) -> String {
    let default_branch = active
        .project
        .default_branch
        .as_deref()
        .unwrap_or("main");
    let update = update_instruction.replace("{branch}", default_branch);
    format!(
        "\n\nYou are working on the project '{}'.\n\
         Repo URL: {}\n\
         Worktree branch: {}\n\
         Working directory: {}\n\
         All file reads/writes/edits and shell commands operate inside this\n\
         directory (a dedicated git worktree). Commit your work on the current\n\
         branch; do not push or merge unless the user asks.\n\n\
         {update}\n\n\
         When the task is complete and the work has been committed, rename the\n\
         worktree to a short, descriptive name with the RenameWorktree tool so\n\
         it can be easily identified later (e.g. 'fix-tui-crash' or\n\
         'add-billing-api'). The file and shell tools follow the renamed\n\
         directory, so they remain available afterwards.",
        active.project.name, active.project.url, active.branch, active.worktree_path
    )
}

/// Resolve the "update your checkout" instruction: the configured one from
/// the rebase-cron defaults (NixOS), or the built-in default. `{branch}`
/// is interpolated by [`project_system_context`].
fn update_instruction() -> String {
    omega_projects::rebase::load_defaults()
        .map(|d| d.update_instruction)
        .unwrap_or_else(|| omega_projects::rebase::DEFAULT_UPDATE_INSTRUCTION.to_string())
}

fn save_active_to_metadata(
    meta: &mut crate::session::metadata::SessionMetadata,
    active: &ActiveProject,
) {
    if let Ok(value) = serde_json::to_value(active) {
        meta.set_custom(META_ACTIVE_PROJECT, value);
    }
}

/// Persist a session's project binding to its metadata file, so resume and
/// the git-host session index see the current path/branch. Best-effort:
/// failures are logged, never fatal for the request that triggered them.
fn persist_active_project(
    session_storage: &Arc<SessionStorage>,
    session_id: &str,
    active: &ActiveProject,
) {
    match session_storage.load_metadata(session_id) {
        Ok(mut meta) => {
            save_active_to_metadata(&mut meta, active);
            if let Err(e) = session_storage.save_metadata(&meta) {
                tracing::warn!(%session_id, error = %e, "could not persist project binding");
            }
        }
        Err(e) => {
            tracing::warn!(
                %session_id,
                error = %e,
                "could not persist project binding: metadata unavailable"
            );
        }
    }
}

fn active_from_metadata(meta: &crate::session::metadata::SessionMetadata) -> Option<ActiveProject> {
    meta.get_custom(META_ACTIVE_PROJECT)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// Resolve the project context a session should use: the explicitly
/// requested project (from a `run`) or the one persisted in metadata
/// (resume/compact). The persisted binding is reconciled against the live
/// git state first — so a worktree renamed outside the daemon is re-bound to
/// its actual path/branch — and if the worktree is gone entirely it is
/// re-created, so a session always has a live checkout to operate in.
async fn resolve_project_context(
    projects: &ProjectManager,
    session_id: &str,
    requested: Option<&ActiveProject>,
    persisted: Option<&ActiveProject>,
) -> Option<ActiveProject> {
    let candidate = requested.or(persisted)?;
    if let Some(reconciled) = projects.reconcile_worktree(candidate).await {
        return Some(reconciled);
    }
    match projects
        .activate(&candidate.project.name, session_id, None)
        .await
    {
        Ok(active) => {
            tracing::info!(
                %session_id,
                project = %active.project.name,
                worktree = %active.worktree_path,
                "re-created missing worktree"
            );
            Some(active)
        }
        Err(e) => {
            tracing::warn!(%session_id, project = %candidate.project.name, error = %e, "could not re-create worktree");
            None
        }
    }
}

/// Spawn a task that forwards a live agent's output chunks to `event_tx` as
/// `ServerEvent::Chunk` events, tagging them with `session_id`.
///
/// A handle's output is a multi-subscriber broadcast, so any number of
/// connections (TUI, multiple web pages) can attach their own forwarder to
/// the same live session. Each forwarder ends when its `event_tx` is dropped
/// (its connection closed) or the session's broadcast closes — it never
/// outlives the session, and a detached session with no subscribers costs
/// nothing.
fn attach_forwarder(
    handle: &AgentHandle,
    event_tx: &broadcast::Sender<ServerEvent>,
    session_id: &str,
) {
    let mut output_rx = handle.subscribe();
    let ev_tx = event_tx.clone();
    let sid = session_id.to_string();
    tokio::spawn(async move {
        loop {
            match output_rx.recv().await {
                Ok(chunk) => {
                    let event = ServerEvent::Chunk {
                        session_id: sid.clone(),
                        chunk: WireChunk::Known(chunk),
                    };
                    if ev_tx.send(event).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(%sid, "output forwarder lagged by {n}");
                }
            }
        }
    });
}

/// Create a brand-new agent session (persisted under `./sessions`), bound
/// to `project` when given: the system prompt carries project context and
/// the tool registry runs inside the worktree. Output chunks are forwarded
/// to `event_tx` so the client renders them like a live conversation.
#[allow(clippy::too_many_arguments)] // private daemon helper wiring one session
async fn create_session(
    session_id: &str,
    base_system_prompt: &str,
    project: Option<&ActiveProject>,
    tools: Arc<ToolRegistry>,
    provider: Arc<dyn LlmProvider>,
    think: bool,
    no_cache: bool,
    runtime: &AgentRuntime,
    event_tx: &broadcast::Sender<ServerEvent>,
    session_storage: &Arc<SessionStorage>,
) -> anyhow::Result<AgentHandle> {
    let mut system_prompt = base_system_prompt.to_string();
    if let Some(active) = project {
        system_prompt.push_str(&project_system_context(active, &update_instruction()));
    }

    let mut agent_session = AgentSession::new_with_storage(
        session_id,
        "omega",
        "omega-tui",
        "A coding agent",
        &system_prompt,
        session_storage.as_ref().clone(),
    )?;

    if let Some(active) = project {
        save_active_to_metadata(&mut agent_session.metadata, active);
    }
    agent_session.set_model(provider.model());
    agent_session.set_provider(provider.provider_name());
    session_storage.save_metadata(&agent_session.metadata)?;

    let mut agent_cfg = AgentConfig::new()
        .with_tools(tools.clone())
        .with_prompt_caching(!no_cache);
    if think {
        agent_cfg = agent_cfg.with_thinking(16000);
    }
    let agent = StandardAgent::new(agent_cfg, provider);

    let handle = runtime
        .spawn(agent_session, |internals| agent.run(internals))
        .await?;

    attach_forwarder(&handle, event_tx, session_id);

    Ok(handle)
}

/// Select a provider for one session without changing the daemon default.
fn session_provider(
    default_provider: &Arc<dyn LlmProvider>,
    stored_model: &str,
    requested: Option<(&str, Option<u32>)>,
) -> Arc<dyn LlmProvider> {
    let (model, max_tokens) = requested
        .filter(|(model, _)| !model.trim().is_empty())
        .map(|(model, max)| (model.trim().to_string(), max))
        .unwrap_or_else(|| {
            let model = stored_model.trim();
            if model.is_empty() {
                (default_provider.model(), None)
            } else {
                (model.to_string(), None)
            }
        });
    default_provider.create_variant(&model, max_tokens)
}

/// Load and re-spawn one stored session with a session-specific provider.
/// History, metadata, system prompt, project binding, and project-scoped tools
/// are all retained. The caller is responsible for shutting down any old
/// handle while holding the live-session registry lock.
#[allow(clippy::too_many_arguments)]
async fn recreate_stored_session(
    session_id: &str,
    requested_model: Option<(&str, Option<u32>)>,
    default_provider: &Arc<dyn LlmProvider>,
    session_storage: &Arc<SessionStorage>,
    tools: &Arc<ToolRegistry>,
    runtime: &AgentRuntime,
    projects: &Arc<ProjectManager>,
    session_projects: &Arc<Mutex<HashMap<String, ActiveProject>>>,
    event_tx: &broadcast::Sender<ServerEvent>,
) -> anyhow::Result<(AgentHandle, Option<ActiveProject>, String)> {
    let mut agent_session =
        AgentSession::load_with_storage(session_id, session_storage.as_ref().clone())?;
    let provider = session_provider(
        default_provider,
        &agent_session.metadata.model,
        requested_model,
    );
    let model = provider.model();
    agent_session.set_model(&model);
    agent_session.set_provider(provider.provider_name());

    let persisted = active_from_metadata(&agent_session.metadata);
    let active = resolve_project_context(projects, session_id, None, persisted.as_ref()).await;
    if let Some(active) = &active {
        save_active_to_metadata(&mut agent_session.metadata, active);
    }
    session_storage.save_metadata(&agent_session.metadata)?;

    let session_tools = tools_for_session(
        tools,
        active.as_ref(),
        session_id,
        rename_worktree_tool(
            projects,
            session_id,
            session_projects,
            session_storage,
            event_tx,
        ),
    );
    let agent = StandardAgent::new(AgentConfig::new().with_tools(session_tools), provider);
    let handle = runtime
        .spawn(agent_session, |internals| agent.run(internals))
        .await?;
    attach_forwarder(&handle, event_tx, session_id);

    Ok((handle, active, model))
}

const COMPACTION_SYSTEM_PROMPT: &str = "You compact coding-agent conversations. Return only a dense, precise continuation summary. Preserve the user's goals, decisions, constraints, important code paths, commands and test results, unresolved errors, and the exact next work. Do not invent facts. Do not include preamble.";
const MAX_COMPACTION_SUMMARY_BYTES: usize = 64 * 1024;

/// Ask the session's current provider for a bounded replacement summary.
async fn compact_history(
    provider: Arc<dyn LlmProvider>,
    session_id: &str,
    messages: &[Message],
) -> anyhow::Result<Vec<Message>> {
    if messages.is_empty() {
        anyhow::bail!("session has no history to compact");
    }
    let original_size = serde_json::to_vec(messages)?.len();
    let mut prompt_messages = messages.to_vec();
    prompt_messages.push(Message::user(
        "Summarize the conversation above for another coding agent that will continue the work immediately.",
    ));
    let compact_provider = provider.create_variant(&provider.model(), Some(2048));
    let mut stream = compact_provider
        .stream_with_tools_and_system(
            prompt_messages,
            Some(SystemPrompt::Text(COMPACTION_SYSTEM_PROMPT.to_string())),
            Vec::new(),
            Some(ToolChoice::none()),
            None,
            Some(session_id),
        )
        .await?;
    let mut summary = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::ContentBlockDelta(delta) => {
                if let ContentDelta::TextDelta { text } = delta.delta {
                    summary.push_str(&text);
                    if summary.len() > MAX_COMPACTION_SUMMARY_BYTES {
                        anyhow::bail!(
                            "provider compaction summary exceeded {} bytes",
                            MAX_COMPACTION_SUMMARY_BYTES
                        );
                    }
                }
            }
            StreamEvent::Error(error) => anyhow::bail!("{}", error.error.message),
            _ => {}
        }
    }
    let summary = summary.trim();
    if summary.is_empty() {
        anyhow::bail!("provider returned an empty compaction summary");
    }
    let replacement = vec![
        Message::user("[Earlier conversation compacted for context]"),
        Message::assistant(summary),
    ];
    let replacement_size = serde_json::to_vec(&replacement)?.len();
    if replacement_size >= original_size {
        anyhow::bail!(
            "session is already too short to compact (summary was not smaller than history)"
        );
    }
    Ok(replacement)
}

async fn compact_stored_history(
    provider: Arc<dyn LlmProvider>,
    session_id: &str,
    session_storage: &SessionStorage,
) -> anyhow::Result<()> {
    let history = session_storage.load_messages(session_id)?;
    let replacement = compact_history(provider, session_id, &history).await?;
    session_storage.replace_messages(session_id, &replacement)?;
    Ok(())
}

async fn require_idle(handle: Option<&AgentHandle>, operation: &str) -> Result<(), String> {
    if let Some(handle) = handle {
        let state = handle.state().await;
        if state != omega_core::core::AgentState::Idle {
            return Err(format!(
                "Cannot {operation} while it is {state}; wait for the turn to finish"
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)] // private daemon wiring for one connection
async fn handle_connection(
    stream: tokio::net::UnixStream,
    current_provider: Arc<std::sync::RwLock<Arc<dyn LlmProvider>>>,
    session_storage: Arc<crate::session::SessionStorage>,
    tools: Arc<ToolRegistry>,
    runtime: AgentRuntime,
    projects: Arc<ProjectManager>,
    roles: Vec<omega_projects::roles::Role>,
    sessions: Arc<Mutex<HashMap<String, AgentHandle>>>,
    session_controls: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    session_projects: Arc<Mutex<HashMap<String, omega_projects::ActiveProject>>>,
) {
    let (reader, writer) = tokio::io::split(stream);
    let writer = Arc::new(Mutex::new(writer));

    // Broadcast channel for events that need to go out to the socket.
    // The writer task pumps this channel; session forwarders push into it.
    let (event_tx, _) = broadcast::channel(256);

    // Sessions this connection has already attached an output forwarder for.
    // A connection must attach at most once per session; otherwise a session
    // it created and later resumed on the same connection would double-render.
    let mut attached: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    // --- writer task (sole writer to the socket) -------------------------
    let writer_handle = {
        let writer = writer.clone();
        let mut rx = event_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let json = match serde_json::to_string(&event) {
                            Ok(j) => j,
                            Err(e) => {
                                tracing::warn!("serialize event: {e}");
                                continue;
                            }
                        };
                        let mut w = writer.lock().await;
                        if w.write_all(json.as_bytes()).await.is_err()
                            || w.write_all(b"\n").await.is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("event writer lagged by {n}");
                    }
                }
            }
        })
    };

    // --- reader task (reads JSON requests from the socket) ---------------
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let req: ClientRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("invalid request JSON: {e}");
                continue;
            }
        };

        // list_sessions doesn't need a session_id — handle it first.
        if req.msg_type == "list_sessions" {
            let list = list_sessions_filtered(&session_storage, req.query.as_deref());
            let _ = event_tx.send(ServerEvent::SessionList { sessions: list });
            continue;
        }

        // list_models doesn't need a session_id either — ask the current provider.
        if req.msg_type == "list_models" {
            let prov = {
                let lock = current_provider.read().unwrap();
                Arc::clone(&lock)
            };
            match prov.list_models().await {
                Ok(models) => {
                    let _ = event_tx.send(ServerEvent::ModelList { models });
                }
                Err(e) => {
                    tracing::warn!("list_models error: {e}");
                    let _ = event_tx.send(ServerEvent::ModelList { models: Vec::new() });
                }
            }
            continue;
        }

        // list_projects doesn't need a session_id either.
        if req.msg_type == "list_projects" {
            match projects.list().await {
                Ok(projects) => {
                    let _ = event_tx.send(ServerEvent::ProjectList { projects });
                }
                Err(e) => {
                    tracing::warn!("list_projects error: {e}");
                    let _ = event_tx.send(ServerEvent::ProjectList {
                        projects: Vec::new(),
                    });
                }
            }
            continue;
        }

        // list_roles doesn't need a session_id either.
        if req.msg_type == "list_roles" {
            let roles: Vec<RoleInfo> = omega_projects::roles::role_names(&roles)
                .into_iter()
                .map(|name| RoleInfo { name })
                .collect();
            let _ = event_tx.send(ServerEvent::RoleList { roles });
            continue;
        }

        let session_id = match req.session_id {
            Some(ref s) if !s.is_empty() => s.clone(),
            _ => {
                tracing::warn!("request missing session_id");
                continue;
            }
        };

        // Serialize control-plane changes and messages per session without
        // holding the daemon-global session registry. A slow compaction can
        // block its own chat, but never unrelated sessions.
        let session_control = {
            let mut controls = session_controls.lock().await;
            controls
                .entry(session_id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _session_guard = session_control.lock().await;

        match req.msg_type.as_str() {
            "run" | "message" => {
                let mut sessions_lock = sessions.lock().await;
                let mut session_projects_lock = session_projects.lock().await;

                let is_new = !sessions_lock.contains_key(&session_id);
                let current_project = session_projects_lock.get(&session_id).cloned();

                // Resolve the project this session should be bound to,
                // re-creating a missing worktree on the fly:
                // - the request carries one → use it (switch if different),
                // - no request project but the session is already bound →
                //   keep the existing binding (a plain message must never
                //   silently drop a session's project),
                // - otherwise no project.
                let desired_project: Option<ActiveProject> = match (&req.project, &current_project)
                {
                    (Some(requested), _) => {
                        match resolve_project_context(&projects, &session_id, Some(requested), None)
                            .await
                        {
                            Some(active) => Some(active),
                            None => {
                                let _ = event_tx.send(ServerEvent::SystemMsg {
                                    message: format!(
                                        "Cannot use project '{}': worktree unavailable",
                                        requested.project.name
                                    ),
                                });
                                None
                            }
                        }
                    }
                    (None, Some(existing)) => {
                        resolve_project_context(&projects, &session_id, None, Some(existing)).await
                    }
                    (None, None) => None,
                };

                // If reconciliation changed a *same-project* binding (e.g. the
                // worktree was renamed outside the daemon so the persisted
                // branch/path went stale), persist the corrected binding and
                // refresh the live map + clients — otherwise the web UI keeps
                // mapping the session to a branch that no longer exists.
                if let (Some(desired), Some(current)) = (&desired_project, &current_project) {
                    if desired.project.name == current.project.name
                        && (desired.branch != current.branch
                            || desired.worktree_path != current.worktree_path)
                    {
                        tracing::info!(
                            %session_id,
                            project = %desired.project.name,
                            branch = %desired.branch,
                            worktree = %desired.worktree_path,
                            "reconciled project binding with live git state"
                        );
                        persist_active_project(&session_storage, &session_id, desired);
                        session_projects_lock.insert(session_id.clone(), desired.clone());
                    }
                }

                // Switching projects on an existing session (or a run that
                // carries a project for a session created without one)
                // requires re-creating the session with new tools.
                let project_changed = !is_new
                    && desired_project.as_ref().map(|p| p.project.name.clone())
                        != current_project.as_ref().map(|p| p.project.name.clone());

                if is_new || project_changed {
                    if !is_new {
                        // Tear down the old handle before re-creating.
                        if let Some(handle) = sessions_lock.remove(&session_id) {
                            let _ = handle.shutdown().await;
                        }
                        session_projects_lock.remove(&session_id);
                        if let Some(old) = current_project {
                            if Some(old.project.name.as_str())
                                != desired_project.as_ref().map(|p| p.project.name.as_str())
                            {
                                // This session's old worktree only — other
                                // sessions' worktrees are never touched.
                                if let Err(e) = projects.remove_worktree(&old).await {
                                    tracing::warn!(
                                        %session_id,
                                        project = %old.project.name,
                                        error = %e,
                                        "remove old worktree"
                                    );
                                }
                            }
                        }
                    }

                    let config = req.config.unwrap_or_default();

                    // A requested model belongs to this session only. The
                    // daemon-wide provider remains the default/factory for
                    // other sessions.
                    let default_provider = current_provider.read().unwrap().clone();
                    let provider = session_provider(
                        &default_provider,
                        "",
                        req.model.as_deref().map(|m| (m, req.max_tokens)),
                    );

                    // Always broadcast the current model so the client knows
                    // what the session is using.
                    {
                        let _ = event_tx.send(ServerEvent::ModelChanged {
                            session_id: session_id.clone(),
                            model: provider.model(),
                        });
                    }

                    // Read system prompt from OMEGA_SYSTEM_PROMPT_PATH file, or empty.
                    // When a role is requested for a brand-new session, use the
                    // role's alternative system prompt instead of the default.
                    let default_prompt = || {
                        env::var("OMEGA_SYSTEM_PROMPT_PATH")
                            .ok()
                            .and_then(|p| std::fs::read_to_string(p).ok())
                            .unwrap_or_default()
                    };
                    let system_prompt = if is_new {
                        req.role
                            .as_deref()
                            .and_then(|name| omega_projects::roles::role_prompt(&roles, name))
                            .unwrap_or_else(&default_prompt)
                    } else {
                        default_prompt()
                    };
                    if is_new && req.role.is_some() {
                        tracing::info!(
                            %session_id,
                            role = req.role.as_deref().unwrap_or(""),
                            "session started with custom role system prompt"
                        );
                    }

                    let session_tools = tools_for_session(
                        &tools,
                        desired_project.as_ref(),
                        &session_id,
                        rename_worktree_tool(
                            &projects,
                            &session_id,
                            &session_projects,
                            &session_storage,
                            &event_tx,
                        ),
                    );
                    let handle = match create_session(
                        &session_id,
                        &system_prompt,
                        desired_project.as_ref(),
                        session_tools,
                        provider,
                        config.think,
                        config.no_cache,
                        &runtime,
                        &event_tx,
                        &session_storage,
                    )
                    .await
                    {
                        Ok(h) => h,
                        Err(e) => {
                            tracing::error!(%session_id, "create session: {e}");
                            continue;
                        }
                    };

                    if let Some(active) = &desired_project {
                        session_projects_lock.insert(session_id.clone(), active.clone());
                        tracing::info!(
                            %session_id,
                            project = %active.project.name,
                            worktree = %active.worktree_path,
                            "session bound to project worktree"
                        );
                    }

                    // --- announce the new session -------------------------
                    let _ = event_tx.send(ServerEvent::Created {
                        session_id: session_id.clone(),
                        session_name: session_id.clone(),
                    });

                    tracing::info!(%session_id, "session created");

                    sessions_lock.insert(session_id.clone(), handle);
                    attached.insert(session_id.clone());
                }

                // --- forward user input to the agent ---------------------
                if let Some(content) = &req.content {
                    if !content.is_empty() {
                        if let Some(handle) = sessions_lock.get(&session_id) {
                            if let Err(e) = handle.send_input(content.clone()).await {
                                tracing::warn!(%session_id, "send input: {e}");
                            }
                        }
                    }
                }
            }

            "activate_project" => {
                let Some(spec) = req.spec.as_deref().map(str::trim).filter(|s| !s.is_empty())
                else {
                    let _ = event_tx.send(ServerEvent::SystemMsg {
                        message: "Usage: /project <project-name|git-url>".to_string(),
                    });
                    continue;
                };

                let mut session_projects_lock = session_projects.lock().await;
                let existing = session_projects_lock.get(&session_id).cloned();

                let active = match projects
                    .activate(spec, &session_id, existing.as_ref())
                    .await
                {
                    Ok(active) => active,
                    Err(e) => {
                        tracing::warn!(%session_id, spec = %spec, error = %e, "activate_project failed");
                        let _ = event_tx.send(ServerEvent::SystemMsg {
                            message: format!("Cannot activate project '{spec}': {e}"),
                        });
                        continue;
                    }
                };

                // The session now works inside the project's worktree. If a
                // live session exists for a *different* project, recreate it
                // so its tools point at the new worktree; if the session
                // doesn't exist yet, create it now so the next run just
                // forwards input.
                let mut sessions_lock = sessions.lock().await;
                let current = session_projects_lock.get(&session_id).cloned();
                let needs_recreate = match sessions_lock.get(&session_id) {
                    None => true,
                    Some(_) => {
                        current.as_ref().map(|c| c.project.name.as_str())
                            != Some(active.project.name.as_str())
                    }
                };
                if needs_recreate {
                    if let Some(handle) = sessions_lock.remove(&session_id) {
                        let _ = handle.shutdown().await;
                    }
                    if let Some(old) = current {
                        if old.project.name != active.project.name {
                            if let Err(e) = projects.remove_worktree(&old).await {
                                tracing::warn!(
                                    %session_id,
                                    project = %old.project.name,
                                    error = %e,
                                    "remove old worktree on switch"
                                );
                            }
                        }
                    }

                    let system_prompt = env::var("OMEGA_SYSTEM_PROMPT_PATH")
                        .ok()
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .unwrap_or_default();
                    let session_tools = tools_for_session(
                        &tools,
                        Some(&active),
                        &session_id,
                        rename_worktree_tool(
                            &projects,
                            &session_id,
                            &session_projects,
                            &session_storage,
                            &event_tx,
                        ),
                    );
                    let provider = current_provider.read().unwrap().clone();

                    match create_session(
                        &session_id,
                        &system_prompt,
                        Some(&active),
                        session_tools,
                        provider,
                        false,
                        false,
                        &runtime,
                        &event_tx,
                        &session_storage,
                    )
                    .await
                    {
                        Ok(handle) => {
                            sessions_lock.insert(session_id.clone(), handle);
                            attached.insert(session_id.clone());
                        }
                        Err(e) => {
                            tracing::error!(%session_id, "activate_project create session: {e}");
                        }
                    }
                }
                drop(sessions_lock);

                session_projects_lock.insert(session_id.clone(), active.clone());

                let _ = event_tx.send(ServerEvent::ProjectActive {
                    project: active.project.clone(),
                    worktree_path: active.worktree_path.clone(),
                    branch: active.branch.clone(),
                });
                tracing::info!(
                    %session_id,
                    project = %active.project.name,
                    worktree = %active.worktree_path,
                    branch = %active.branch,
                    "project activated"
                );
            }

            "new_session" | "new" => {
                // Explicitly created on next run; just acknowledge.
                let _ = event_tx.send(ServerEvent::Created {
                    session_id: session_id.clone(),
                    session_name: session_id.clone(),
                });
            }

            "interrupt" => {
                let sessions_lock = sessions.lock().await;
                if let Some(handle) = sessions_lock.get(&session_id) {
                    if let Err(e) = handle.interrupt().await {
                        tracing::warn!(%session_id, "send interrupt: {e}");
                    }
                }
            }

            "resume_session" => {
                if session_storage.session_exists(&session_id) {
                    let mut resumed_active = None;
                    let live_handle = sessions.lock().await.get(&session_id).cloned();
                    if live_handle.is_none() {
                        let default_provider = current_provider.read().unwrap().clone();
                        match recreate_stored_session(
                            &session_id,
                            None,
                            &default_provider,
                            &session_storage,
                            &tools,
                            &runtime,
                            &projects,
                            &session_projects,
                            &event_tx,
                        )
                        .await
                        {
                            Ok((handle, active, _)) => {
                                sessions.lock().await.insert(session_id.clone(), handle);
                                attached.insert(session_id.clone());
                                resumed_active = active;
                            }
                            Err(e) => {
                                tracing::error!(%session_id, "resume session: {e}");
                                let _ = event_tx.send(ServerEvent::SystemMsg {
                                    message: format!("Cannot resume session: {e}"),
                                });
                                continue;
                            }
                        }
                    } else if let Some(handle) = live_handle.as_ref() {
                        // The session is already running — but it may have
                        // been created on THIS connection (which already
                        // attached a forwarder). Attach a fresh one only if
                        // we haven't, so a session that is created and later
                        // resumed on the same connection doesn't double-render.
                        if !attached.contains(&session_id) {
                            attach_forwarder(handle, &event_tx, &session_id);
                            attached.insert(session_id.clone());
                        }
                    }
                    if let Some(active) = resumed_active {
                        session_projects
                            .lock()
                            .await
                            .insert(session_id.clone(), active);
                    }

                    let _ = event_tx.send(ServerEvent::SessionResumed {
                        session_id: session_id.clone(),
                        session_name: session_id.clone(),
                    });
                    if let Ok(meta) = session_storage.load_metadata(&session_id) {
                        let _ = event_tx.send(ServerEvent::ModelChanged {
                            session_id: session_id.clone(),
                            model: meta.model,
                        });
                    }

                    // Re-announce the project binding so the client's status
                    // line reflects the resumed session's worktree.
                    if let Some(active) = session_projects.lock().await.get(&session_id).cloned() {
                        let _ = event_tx.send(ServerEvent::ProjectActive {
                            project: active.project.clone(),
                            worktree_path: active.worktree_path.clone(),
                            branch: active.branch.clone(),
                        });
                    }

                    // Load message history and replay it to the client,
                    // rendered exactly like a live conversation (no empty
                    // turns, no tool plumbing).
                    if let Ok(messages) = session_storage.load_messages(&session_id) {
                        for replay in history_to_replay(&messages) {
                            let _ = event_tx.send(ServerEvent::HistoryMessage {
                                session_id: session_id.clone(),
                                role: replay.role,
                                content: replay.content,
                            });
                        }
                    }
                } else {
                    let _ = event_tx.send(ServerEvent::SystemMsg {
                        message: format!("Session '{session_id}' not found"),
                    });
                }
            }

            "set_model" => {
                let Some(model) = req.model.as_deref().map(str::trim) else {
                    let _ = event_tx.send(ServerEvent::SystemMsg {
                        message: "Cannot change model: model is required".to_string(),
                    });
                    continue;
                };
                if model.is_empty()
                    || model.len() > 256
                    || model.chars().any(char::is_control)
                    || !session_storage.session_exists(&session_id)
                {
                    let _ = event_tx.send(ServerEvent::SystemMsg {
                        message: if !session_storage.session_exists(&session_id) {
                            format!("Cannot change model: session '{session_id}' not found")
                        } else {
                            "Cannot change model: invalid model name".to_string()
                        },
                    });
                    continue;
                }

                let old_handle = sessions.lock().await.get(&session_id).cloned();
                if let Err(message) = require_idle(old_handle.as_ref(), "change model").await {
                    let _ = event_tx.send(ServerEvent::SystemMsg { message });
                    continue;
                }

                // Build the replacement before stopping the old idle agent;
                // a provider/session load error therefore leaves it untouched.
                let default_provider = current_provider.read().unwrap().clone();
                match recreate_stored_session(
                    &session_id,
                    Some((model, req.max_tokens)),
                    &default_provider,
                    &session_storage,
                    &tools,
                    &runtime,
                    &projects,
                    &session_projects,
                    &event_tx,
                )
                .await
                {
                    Ok((new_handle, active, selected_model)) => {
                        sessions.lock().await.insert(session_id.clone(), new_handle);
                        if let Some(old_handle) = old_handle {
                            let _ = old_handle.shutdown().await;
                        }
                        attached.insert(session_id.clone());
                        if let Some(active) = active {
                            session_projects
                                .lock()
                                .await
                                .insert(session_id.clone(), active);
                        }
                        let _ = event_tx.send(ServerEvent::ModelChanged {
                            session_id: session_id.clone(),
                            model: selected_model.clone(),
                        });
                        tracing::info!(%session_id, model = %selected_model, "session model changed");
                    }
                    Err(e) => {
                        tracing::error!(%session_id, error = %e, "change session model");
                        let _ = event_tx.send(ServerEvent::SystemMsg {
                            message: format!("Cannot change model: {e}"),
                        });
                    }
                }
            }

            "compact" => {
                let old_handle = sessions.lock().await.get(&session_id).cloned();
                if let Err(message) = require_idle(old_handle.as_ref(), "compact session").await {
                    let _ = event_tx.send(ServerEvent::SystemMsg { message });
                    continue;
                }
                let meta = match session_storage.load_metadata(&session_id) {
                    Ok(meta) => meta,
                    Err(e) => {
                        let _ = event_tx.send(ServerEvent::SystemMsg {
                            message: format!("Cannot compact session: {e}"),
                        });
                        continue;
                    }
                };
                let default_provider = current_provider.read().unwrap().clone();
                let provider = session_provider(&default_provider, &meta.model, None);
                match tokio::time::timeout(
                    std::time::Duration::from_secs(120),
                    compact_stored_history(provider, &session_id, &session_storage),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        let _ = event_tx.send(ServerEvent::SystemMsg {
                            message: format!("Cannot compact session: {e}"),
                        });
                        continue;
                    }
                    Err(_) => {
                        let _ = event_tx.send(ServerEvent::SystemMsg {
                            message: "Cannot compact session: summarization timed out".to_string(),
                        });
                        continue;
                    }
                }
                if let Some(old_handle) = old_handle {
                    let _ = old_handle.shutdown().await;
                }
                match recreate_stored_session(
                    &session_id,
                    None,
                    &default_provider,
                    &session_storage,
                    &tools,
                    &runtime,
                    &projects,
                    &session_projects,
                    &event_tx,
                )
                .await
                {
                    Ok((handle, active, _)) => {
                        sessions.lock().await.insert(session_id.clone(), handle);
                        attached.insert(session_id.clone());
                        if let Some(active) = active {
                            session_projects
                                .lock()
                                .await
                                .insert(session_id.clone(), active);
                        }
                        let _ = event_tx.send(ServerEvent::SessionCompacted {
                            session_id: session_id.clone(),
                        });
                        tracing::info!(%session_id, "session history compacted");
                    }
                    Err(e) => {
                        tracing::error!(%session_id, error = %e, "restart compacted session");
                        let _ = event_tx.send(ServerEvent::SystemMsg {
                            message: format!(
                                "Session history was compacted, but the live session could not restart: {e}"
                            ),
                        });
                    }
                }
            }

            other => {
                tracing::warn!("unknown request type: {other}");
            }
        }
    }

    // --- connection closed -----------------------------------------------
    // Sessions are daemon-global and keep running after this connection
    // closes (they detach): a web page may unload freely, and the session
    // either finishes on its own or is resumed from any later connection.
    drop(event_tx); // signals the writer task to stop
    let _ = writer_handle.await; // wait for writer to finish
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn configured_session_dir(value: Option<String>) -> std::path::PathBuf {
    value
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("./sessions"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let llm = create_llm_provider()?;
    let tools = create_tools()?;
    let runtime = AgentRuntime::new();

    // Track the currently-active LLM provider so we can change models at runtime.
    let current_provider: Arc<std::sync::RwLock<Arc<dyn LlmProvider>>> =
        Arc::new(std::sync::RwLock::new(llm));

    // One configured store for every create/resume/model/compact path. The
    // NixOS module exports OMEGA_SESSION_DIR; local development keeps the
    // historical ./sessions default.
    let session_dir = configured_session_dir(env::var("OMEGA_SESSION_DIR").ok());
    let session_storage = Arc::new(SessionStorage::with_dir(session_dir));

    // Empty sessions (metadata written, but the agent never produced a
    // message — aborted creations) are useless to everyone: prune them so
    // they never surface in the git-host web UI or session lists.
    match session_storage.prune_empty_sessions() {
        Ok(removed) if !removed.is_empty() => {
            tracing::info!(count = removed.len(), "pruned empty sessions at startup");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "failed to prune empty sessions"),
    }

    // Project store — registered repos + per-session git worktrees.
    let projects = Arc::new(ProjectManager::new());
    let projects_root = projects.root().display().to_string();

    // Upstream rebaser cron: keeps cron-jobbable projects' main branches in
    // sync with upstream (mechanical fast-forward when possible, waking the
    // dedicated rebaser chat when a real rebase with conflict resolution is
    // needed). Runs in the background for the daemon's whole lifetime;
    // with no config it is idle and costs nothing.
    {
        let defaults = omega_projects::rebase::load_defaults();
        let job = rebase_job::RebaseJob::new(
            Arc::clone(&projects),
            runtime.clone(),
            Arc::clone(&current_provider),
            Arc::clone(&session_storage),
            defaults,
        );
        tokio::spawn(job.run());
        match omega_projects::rebase::load_defaults() {
            Some(d) if !d.projects.is_empty() => tracing::info!(
                projects = ?d.projects,
                interval_seconds = d.interval_seconds,
                "upstream rebaser cron enabled"
            ),
            _ => tracing::debug!("upstream rebaser cron idle (no configured projects)"),
        }
    }

    let socket_path =
        env::var("OMEGA_LOOP_SOCKET_PATH").unwrap_or_else(|_| "/tmp/omega-loop.sock".to_string());

    // Named roles (alternative system prompts) from the NixOS module
    // (`services.omega.roles`), via OMEGA_ROLES_PATH. With none configured
    // the daemon simply has the default "no role" prompt.
    let roles = omega_projects::roles::load_roles();
    tracing::info!(
        count = omega_projects::roles::role_names(&roles).len(),
        "loaded named roles"
    );

    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("Cannot bind to {socket_path}"))?;

    // Daemon-global live-session registry: unlike the previous per-connection
    // model (where closing a connection tore every session down), sessions now
    // live for the daemon's lifetime. A web page can come and go freely — its
    // session keeps running, and any later connection (or the agent itself,
    // in autonomous mode) can resume it. Handles are cheap channel wrappers;
    // output is forwarded to each interested connection individually, so a
    // detached session costs nothing until someone subscribes.
    let sessions: Arc<Mutex<HashMap<String, AgentHandle>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let session_controls: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let session_projects: Arc<Mutex<HashMap<String, omega_projects::ActiveProject>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let cwd = env::current_dir()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_else(|_| "?".to_string());
    {
        let prov = current_provider.read().unwrap();
        tracing::info!(socket = %socket_path, cwd = %cwd, projects = %projects_root, model = %prov.model(), "omega-loop started");
        eprintln!(
            "omega-loop ({}) listening on {socket_path} (projects: {projects_root})",
            prov.model()
        );
    }

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                tracing::debug!(peer = ?addr, "accepted connection");
                let current_provider = current_provider.clone();
                let session_storage = session_storage.clone();
                let tools = tools.clone();
                let runtime = runtime.clone();
                let projects = projects.clone();
                let roles = roles.clone();
                // Sessions and their project bindings are daemon-global: a
                // session keeps running after the connection that created it
                // closes (and survives across web page loads), so every
                // connection shares the same live-session registry.
                let sessions = sessions.clone();
                let session_controls = session_controls.clone();
                let session_projects = session_projects.clone();
                tokio::spawn(handle_connection(
                    stream,
                    current_provider,
                    session_storage,
                    tools,
                    runtime,
                    projects,
                    roles,
                    sessions,
                    session_controls,
                    session_projects,
                ));
            }
            Err(e) => {
                tracing::error!("accept error: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use omega_core::core::ToolResultData;
    use omega_llm::{
        ContentBlock, ContentBlockDeltaEvent, ContentBlockStart, ContentBlockStartEvent, Message,
        ToolChoice,
    };
    use std::pin::Pin;
    use tempfile::TempDir;

    #[derive(Clone)]
    struct MockProvider {
        model: String,
        summary: String,
        fail: bool,
        include_start_text: bool,
    }

    impl MockProvider {
        fn summary(model: &str, summary: impl Into<String>) -> Self {
            Self {
                model: model.to_string(),
                summary: summary.into(),
                fail: false,
                include_start_text: true,
            }
        }

        fn failing(model: &str) -> Self {
            Self {
                model: model.to_string(),
                summary: String::new(),
                fail: true,
                include_start_text: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        async fn stream_with_tools_and_system(
            &self,
            _messages: Vec<Message>,
            _system: Option<SystemPrompt>,
            _tools: Vec<ToolDefinition>,
            _tool_choice: Option<ToolChoice>,
            _thinking: Option<omega_llm::ThinkingConfig>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>>
        {
            if self.fail {
                anyhow::bail!("mock compaction failure");
            }
            let mut events = Vec::new();
            if self.include_start_text {
                // StandardAgent treats deltas as authoritative. Including the
                // same text here catches accidental start+delta duplication.
                events.push(Ok(StreamEvent::ContentBlockStart(ContentBlockStartEvent {
                    index: 0,
                    content_block: ContentBlockStart::Text {
                        text: self.summary.clone(),
                    },
                })));
            }
            events.push(Ok(StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                index: 0,
                delta: ContentDelta::TextDelta {
                    text: self.summary.clone(),
                },
            })));
            events.push(Ok(StreamEvent::MessageStop));
            Ok(Box::pin(stream::iter(events)))
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(vec![self.model.clone(), "model-a".into(), "model-b".into()])
        }

        fn model(&self) -> String {
            self.model.clone()
        }

        fn provider_name(&self) -> &str {
            "mock"
        }

        fn create_variant(
            &self,
            model: &str,
            _max_tokens: Option<u32>,
        ) -> Arc<dyn LlmProvider> {
            Arc::new(Self {
                model: model.to_string(),
                ..self.clone()
            })
        }
    }

    fn long_history() -> Vec<Message> {
        (0..12)
            .flat_map(|n| {
                [
                    Message::user(format!("request {n}: {}", "important context ".repeat(40))),
                    Message::assistant(format!("answer {n}: {}", "implementation detail ".repeat(40))),
                ]
            })
            .collect()
    }

    #[test]
    fn configured_session_dir_honors_override() {
        assert_eq!(
            configured_session_dir(Some("/custom/omega-sessions".to_string())),
            std::path::PathBuf::from("/custom/omega-sessions")
        );
        assert_eq!(
            configured_session_dir(None),
            std::path::PathBuf::from("./sessions")
        );
    }

    #[test]
    fn session_provider_variants_are_isolated() {
        let default: Arc<dyn LlmProvider> = Arc::new(MockProvider::summary("default", "summary"));
        let first = session_provider(&default, "", Some(("model-a", None)));
        let second = session_provider(&default, "model-b", None);
        assert_eq!(first.model(), "model-a");
        assert_eq!(second.model(), "model-b");
        assert_eq!(default.model(), "default", "session variants must not mutate default");
    }

    #[tokio::test]
    async fn model_recreation_changes_only_target_session() {
        let temp = TempDir::new().unwrap();
        let storage = Arc::new(SessionStorage::with_dir(temp.path().join("sessions")));
        for (id, model) in [("one", "old-one"), ("two", "old-two")] {
            let mut session = AgentSession::new_with_storage(
                id,
                "omega",
                "omega-tui",
                "test",
                "system",
                storage.as_ref().clone(),
            )
            .unwrap();
            session.set_model(model);
            session.set_provider("mock");
            session.save().unwrap();
        }
        let default: Arc<dyn LlmProvider> = Arc::new(MockProvider::summary("default", "summary"));
        let tools = Arc::new(ToolRegistry::new());
        let projects = Arc::new(ProjectManager::with_root(temp.path().join("projects")));
        let bindings = Arc::new(Mutex::new(HashMap::new()));
        let (events, _receiver) = broadcast::channel(16);
        let (handle, _, model) = recreate_stored_session(
            "one",
            Some(("model-a", None)),
            &default,
            &storage,
            &tools,
            &AgentRuntime::new(),
            &projects,
            &bindings,
            &events,
        )
        .await
        .unwrap();
        assert_eq!(model, "model-a");
        assert_eq!(storage.load_metadata("one").unwrap().model, "model-a");
        assert_eq!(storage.load_metadata("two").unwrap().model, "old-two");
        assert_eq!(default.model(), "default");
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn compaction_reduces_custom_store_and_does_not_duplicate_start_text() {
        let temp = TempDir::new().unwrap();
        let storage = SessionStorage::with_dir(temp.path().join("custom-session-dir"));
        let original = long_history();
        storage.replace_messages("session", &original).unwrap();
        let original_bytes = std::fs::read(storage.history_path("session")).unwrap().len();
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::summary("model-a", "dense continuation summary"));

        compact_stored_history(provider, "session", &storage)
            .await
            .unwrap();

        let compacted = storage.load_messages("session").unwrap();
        assert_eq!(compacted.len(), 2);
        assert_eq!(message_text(&compacted[1]), "dense continuation summary");
        assert!(
            std::fs::read(storage.history_path("session")).unwrap().len() < original_bytes,
            "compaction must actually reduce persisted history"
        );
    }

    #[tokio::test]
    async fn compaction_failure_and_summary_cap_leave_history_untouched() {
        let temp = TempDir::new().unwrap();
        let storage = SessionStorage::with_dir(temp.path().join("sessions"));
        storage.replace_messages("session", &long_history()).unwrap();
        let before = std::fs::read(storage.history_path("session")).unwrap();

        let failing: Arc<dyn LlmProvider> = Arc::new(MockProvider::failing("model-a"));
        assert!(compact_stored_history(failing, "session", &storage)
            .await
            .is_err());
        assert_eq!(std::fs::read(storage.history_path("session")).unwrap(), before);

        let oversized: Arc<dyn LlmProvider> = Arc::new(MockProvider::summary(
            "model-a",
            "x".repeat(MAX_COMPACTION_SUMMARY_BYTES + 1),
        ));
        assert!(compact_stored_history(oversized, "session", &storage)
            .await
            .is_err());
        assert_eq!(std::fs::read(storage.history_path("session")).unwrap(), before);
    }

    #[tokio::test]
    async fn busy_session_rejection_precedes_any_history_mutation() {
        let temp = TempDir::new().unwrap();
        let storage = SessionStorage::with_dir(temp.path());
        storage.replace_messages("busy", &long_history()).unwrap();
        let before = std::fs::read(storage.history_path("busy")).unwrap();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel(1);
        let (output_tx, _output_rx) = broadcast::channel(1);
        let handle = AgentHandle::new(
            "busy",
            input_tx,
            output_tx,
            Arc::new(tokio::sync::RwLock::new(
                omega_core::core::AgentState::Processing,
            )),
        );
        let error = require_idle(Some(&handle), "compact session")
            .await
            .unwrap_err();
        assert!(error.contains("Processing"));
        assert_eq!(std::fs::read(storage.history_path("busy")).unwrap(), before);
    }

    // -----------------------------------------------------------------------
    // Project helpers — system context + metadata round-trip + worktree
    // re-creation
    // -----------------------------------------------------------------------

    fn sample_active(name: &str, worktree: &str) -> ActiveProject {
        ActiveProject {
            project: ProjectInfo {
                name: name.to_string(),
                url: format!("https://example.com/{name}.git"),
                default_branch: Some("main".to_string()),
                created_at: Default::default(),
            },
            worktree_path: worktree.to_string(),
            branch: "omega/sess-abc123".to_string(),
        }
    }

    #[test]
    fn project_system_context_includes_repo_and_worktree() {
        let active = sample_active("omega", "/tmp/wt/omega/sess-1");
        let ctx = project_system_context(&active, &update_instruction());
        assert!(ctx.contains("omega"), "context mentions project name");
        assert!(ctx.contains("https://example.com/omega.git"));
        assert!(ctx.contains("/tmp/wt/omega/sess-1"));
        assert!(ctx.contains("omega/sess-abc123"));
        assert!(ctx.contains("git worktree"));
    }

    #[test]
    fn project_system_context_instructs_worktree_rename_when_done() {
        let active = sample_active("omega", "/tmp/wt/omega/sess-1");
        let ctx = project_system_context(&active, &update_instruction());
        assert!(
            ctx.contains("RenameWorktree"),
            "context must mention the rename tool: {ctx}"
        );
        assert!(
            ctx.contains("task is complete") || ctx.contains("final step"),
            "context must say to rename when the work is done: {ctx}"
        );
        assert!(
            ctx.contains("identified") || ctx.contains("identify"),
            "context must explain why renaming helps: {ctx}"
        );
    }

    #[test]
    fn project_system_context_instructs_checkout_update() {
        let active = sample_active("omega", "/tmp/wt/omega/sess-1");
        // The built-in default instruction is used when no config file is
        // set; it must tell the agent to keep the checkout in sync.
        let ctx = project_system_context(&active, &update_instruction());
        assert!(
            ctx.to_lowercase().contains("up to date with upstream")
                || ctx.to_lowercase().contains("update your checkout"),
            "context must tell the agent to update its checkout: {ctx}"
        );
        assert!(
            ctx.contains("rebase"),
            "context must tell the agent to rebase on the updated main: {ctx}"
        );
    }

    #[test]
    fn project_system_context_interpolates_branch_placeholder() {
        let active = sample_active("omega", "/tmp/wt/omega/sess-1");
        let instruction = "rebase your worktree on {branch} when entering";
        let ctx = project_system_context(&active, instruction);
        assert!(
            ctx.contains("rebase your worktree on main when entering"),
            "the {{branch}} placeholder must be replaced with the default branch: {ctx}"
        );
    }

    #[test]
    fn active_project_metadata_round_trips() {
        let active = sample_active("omega", "/tmp/wt/omega/sess-1");
        let mut meta =
            crate::session::metadata::SessionMetadata::new("sess-1", "omega", "omega-tui", "d");
        save_active_to_metadata(&mut meta, &active);
        let loaded = active_from_metadata(&meta).expect("should deserialize");
        assert_eq!(loaded.project.name, "omega");
        assert_eq!(loaded.worktree_path, "/tmp/wt/omega/sess-1");
        assert_eq!(loaded.branch, "omega/sess-abc123");
    }

    #[test]
    fn active_project_metadata_none_when_unset() {
        let meta =
            crate::session::metadata::SessionMetadata::new("sess-1", "omega", "omega-tui", "d");
        assert!(active_from_metadata(&meta).is_none());
    }

    #[tokio::test]
    async fn resolve_project_context_prefers_requested_over_persisted() {
        let (projects, remote_a, remote_b) = projects_fixture().await;
        let requested = projects.activate(&remote_a, "sess-1", None).await.unwrap();
        let persisted = projects.activate(&remote_b, "sess-1", None).await.unwrap();

        let resolved =
            resolve_project_context(&projects, "sess-1", Some(&requested), Some(&persisted))
                .await
                .expect("requested worktree exists");
        assert_eq!(resolved.project.name, requested.project.name);
    }

    #[tokio::test]
    async fn resolve_project_context_falls_back_to_persisted() {
        let (projects, remote_a, _) = projects_fixture().await;
        let persisted = projects.activate(&remote_a, "sess-1", None).await.unwrap();

        let resolved = resolve_project_context(&projects, "sess-1", None, Some(&persisted))
            .await
            .expect("persisted worktree exists");
        assert_eq!(resolved.project.name, persisted.project.name);
    }

    /// Two throwaway git remotes (with one commit each) + a throwaway store.
    async fn projects_fixture() -> (ProjectManager, String, String) {
        let store = TempDir::new().unwrap();
        let projects = ProjectManager::with_root(store.path());
        let a = init_remote_repo().await;
        let b = init_remote_repo().await;
        (projects, a, b)
    }

    /// A temp-dir git repo with one committed file, as a local "remote".
    /// The temp dir is intentionally leaked (not cleaned up) so the repo
    /// outlives this helper — tests run fast and the OS reclaims it.
    async fn init_remote_repo() -> String {
        let dir = TempDir::new().unwrap().keep();
        tokio::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "omega-test@example.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Omega Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        // Don't inherit the host's commit.gpgsign — signing would prompt/
        // hang on a throwaway repo that has no signing key.
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::fs::write(dir.join("README.md"), "# Dummy\n")
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        dir.display().to_string()
    }

    #[tokio::test]
    async fn resolve_project_context_finds_worktree_when_path_is_stale() {
        let (projects, remote, _) = projects_fixture().await;
        let first = projects.activate(&remote, "sess-1", None).await.unwrap();

        // Persisted path is wrong/gone, but the branch lives on (e.g. the
        // worktree directory moved). Reconcile must find the real checkout
        // instead of needlessly re-creating one.
        let wrong_path = ActiveProject {
            project: first.project.clone(),
            worktree_path: "/nonexistent/vanished-worktree".to_string(),
            branch: first.branch.clone(),
        };

        let resolved = resolve_project_context(&projects, "sess-1", None, Some(&wrong_path))
            .await
            .expect("should resolve via the live branch");
        assert_eq!(resolved.project.name, first.project.name);
        assert_eq!(resolved.branch, first.branch);
        assert!(
            std::path::Path::new(&resolved.worktree_path).is_dir(),
            "worktree must exist on disk: {}",
            resolved.worktree_path
        );
        assert_ne!(resolved.worktree_path, wrong_path.worktree_path);
    }

    #[tokio::test]
    async fn resolve_project_context_recreates_worktree_when_unrecoverable() {
        let (projects, remote, _) = projects_fixture().await;
        let first = projects.activate(&remote, "sess-1", None).await.unwrap();

        // Neither path nor branch can be found → a fresh worktree is created.
        let gone = ActiveProject {
            project: first.project.clone(),
            worktree_path: "/nonexistent/vanished-worktree".to_string(),
            branch: "omega/never-existed".to_string(),
        };

        let resolved = resolve_project_context(&projects, "sess-1", None, Some(&gone))
            .await
            .expect("should re-create a missing worktree");
        assert_eq!(resolved.project.name, first.project.name);
        assert!(
            std::path::Path::new(&resolved.worktree_path).is_dir(),
            "worktree must exist on disk: {}",
            resolved.worktree_path
        );
        assert_ne!(resolved.worktree_path, gone.worktree_path);
        assert_ne!(resolved.branch, gone.branch);
    }

    #[tokio::test]
    async fn resolve_project_context_self_heals_externally_renamed_binding() {
        let (projects, remote, _) = projects_fixture().await;
        let first = projects.activate(&remote, "sess-1", None).await.unwrap();

        // Outside the daemon, someone renames the worktree's branch in place
        // (raw `git branch -m` from inside the worktree). The persisted
        // binding is now stale.
        let repo = projects.repo_dir(&first.project.name);
        let branch_cmd = tokio::process::Command::new("git")
            .args([
                "-C",
                repo.to_str().unwrap(),
                "branch",
                "-m",
                &first.branch,
                "omega/renamed-in-place",
            ])
            .output()
            .await
            .unwrap();
        assert!(branch_cmd.status.success());

        let stale = ActiveProject {
            project: first.project.clone(),
            worktree_path: first.worktree_path.clone(),
            branch: first.branch.clone(),
        };
        let resolved = resolve_project_context(&projects, "sess-1", None, Some(&stale))
            .await
            .expect("worktree still exists in place");
        assert_eq!(
            resolved.branch, "omega/renamed-in-place",
            "resolved binding must reflect the actual branch"
        );
        assert_eq!(resolved.worktree_path, first.worktree_path);
    }

    #[tokio::test]
    async fn resolve_project_context_none_without_project() {
        let store = TempDir::new().unwrap();
        let projects = ProjectManager::with_root(store.path());
        assert!(resolve_project_context(&projects, "sess-1", None, None)
            .await
            .is_none());
    }

    // -----------------------------------------------------------------------
    // RenameWorktree tool — the agent-facing rename of a session worktree
    // -----------------------------------------------------------------------

    /// Minimal [`ToolRuntime`] for tool tests: records nothing, never
    /// interrupted.
    struct NoopRuntime;

    impl ToolRuntime for NoopRuntime {
        fn send_output(&self, _chunk: OutputChunk) {}
        fn is_interrupted(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn rename_worktree_tool_renames_binding_metadata_and_broadcasts() {
        // Live store + remote kept alive for the duration of the test.
        let base = TempDir::new().unwrap();
        let projects = ProjectManager::with_root(base.path().join("store"));
        let active = projects
            .activate(&init_remote_repo().await, "sess-1", None)
            .await
            .unwrap();

        // The daemon-side state the tool must keep consistent.
        let session_projects: Arc<Mutex<HashMap<String, ActiveProject>>> = Arc::new(Mutex::new(
            HashMap::from([("sess-1".to_string(), active.clone())]),
        ));
        let storage = Arc::new(crate::session::SessionStorage::with_dir(
            base.path().join("sessions"),
        ));
        let mut meta =
            crate::session::metadata::SessionMetadata::new("sess-1", "omega", "omega-tui", "d");
        save_active_to_metadata(&mut meta, &active);
        storage.save_metadata(&meta).unwrap();

        let (tx, mut rx) = broadcast::channel(16);
        let omega_client = OmegaClient::with_socket("/tmp/unused.sock")
            .with_dir(active.worktree_path.clone());
        let tool = RenameWorktreeTool {
            projects: Arc::new(projects),
            session_id: "sess-1".to_string(),
            session_projects: session_projects.clone(),
            session_storage: storage.clone(),
            event_tx: tx,
            omega_client: Some(omega_client.clone()),
        };

        let result = tool
            .execute(
                &serde_json::json!({ "name": "fix-tui-crash" }),
                &mut NoopRuntime,
            )
            .await
            .unwrap();
        assert!(!result.is_error, "rename failed: {:?}", result.content);

        // Worktree + branch moved on disk.
        let old_dir = Path::new(&active.worktree_path);
        assert!(!old_dir.exists(), "old worktree dir must be gone");
        let new_dir = old_dir.parent().unwrap().join("fix-tui-crash");
        assert!(new_dir.is_dir(), "renamed worktree must exist");
        assert_eq!(
            git_for_test(&new_dir, &["branch", "--show-current"]).await,
            "omega/fix-tui-crash"
        );
        let ToolResultData::Text(text) = &result.content else {
            panic!("unexpected result content");
        };
        assert!(text.contains("fix-tui-crash"), "result: {text}");

        // Live binding map updated.
        let binding = session_projects
            .lock()
            .await
            .get("sess-1")
            .cloned()
            .expect("binding present");
        assert_eq!(binding.branch, "omega/fix-tui-crash");
        assert_eq!(binding.worktree_path, new_dir.display().to_string());
        assert_eq!(
            omega_client.current_dir().as_deref(),
            Some(binding.worktree_path.as_str()),
            "existing omega-sh proxies must follow the renamed checkout"
        );

        // Persisted metadata updated (source of truth for resume + git-host).
        let persisted = active_from_metadata(&storage.load_metadata("sess-1").unwrap())
            .expect("active project persisted");
        assert_eq!(persisted.branch, "omega/fix-tui-crash");
        assert_eq!(persisted.worktree_path, binding.worktree_path);

        // Clients got a ProjectActive announcement with the new name.
        match rx.recv().await {
            Ok(ServerEvent::ProjectActive {
                worktree_path,
                branch,
                ..
            }) => {
                assert_eq!(branch, "omega/fix-tui-crash");
                assert_eq!(worktree_path, binding.worktree_path);
            }
            other => panic!("expected ProjectActive, got {other:?}"),
        }
    }

    /// Regression: the running agent's in-memory metadata holds the pre-rename
    /// binding. The rename tool persists the new binding to disk AND syncs the
    /// in-memory metadata (via the runtime); a subsequent message save must
    /// NOT revert the persisted binding to the stale pre-rename one — otherwise
    /// the web UI keeps showing the old worktree/branch and can't open the chat.
    #[tokio::test]
    async fn rename_worktree_tool_survives_later_message_saves() {
        let base = TempDir::new().unwrap();
        let projects = ProjectManager::with_root(base.path().join("store"));
        let active = projects
            .activate(&init_remote_repo().await, "sess-1", None)
            .await
            .unwrap();

        let session_projects: Arc<Mutex<HashMap<String, ActiveProject>>> = Arc::new(Mutex::new(
            HashMap::from([("sess-1".to_string(), active.clone())]),
        ));
        let storage = Arc::new(crate::session::SessionStorage::with_dir(
            base.path().join("sessions"),
        ));

        // Real agent session whose in-memory metadata holds the PRE-rename
        // binding — exactly what the daemon stores at create_session.
        let mut session = AgentSession::new_with_storage(
            "sess-1",
            "omega",
            "omega-tui",
            "A coding agent",
            "system prompt",
            (*storage).clone(),
        )
        .unwrap();
        save_active_to_metadata(&mut session.metadata, &active);
        storage.save_metadata(&session.metadata).unwrap();
        let session = Arc::new(tokio::sync::RwLock::new(session));

        // A runtime mirroring AgentInternals: set_session_meta keeps the
        // in-memory agent session metadata in sync with the rename.
        struct SessionMetaRuntime {
            session: Arc<tokio::sync::RwLock<AgentSession>>,
        }
        impl ToolRuntime for SessionMetaRuntime {
            fn send_output(&self, _chunk: OutputChunk) {}
            fn is_interrupted(&self) -> bool {
                false
            }
            fn set_session_meta(&self, key: &str, value: serde_json::Value) {
                if let Ok(mut s) = self.session.try_write() {
                    s.set_custom(key, value);
                }
            }
        }

        let (tx, _) = broadcast::channel(16);
        let tool = RenameWorktreeTool {
            projects: Arc::new(projects),
            session_id: "sess-1".to_string(),
            session_projects: session_projects.clone(),
            session_storage: storage.clone(),
            event_tx: tx,
            omega_client: None,
        };
        let mut rt = SessionMetaRuntime {
            session: session.clone(),
        };
        let result = tool
            .execute(&serde_json::json!({ "name": "fix-tui-crash" }), &mut rt)
            .await
            .unwrap();
        assert!(!result.is_error, "rename failed: {:?}", result.content);

        // The agent emits a message right after the tool (as the loop does), so
        // its in-memory metadata would be re-saved to disk. This must preserve
        // the renamed branch, not clobber it with the stale pre-rename value.
        session
            .write()
            .await
            .add_message(Message::assistant("done"))
            .unwrap();

        let persisted = active_from_metadata(&storage.load_metadata("sess-1").unwrap())
            .expect("active project persisted");
        assert_eq!(
            persisted.branch, "omega/fix-tui-crash",
            "a later message save must not clobber the renamed branch"
        );
        assert!(
            !persisted.worktree_path.contains("sess-1"),
            "persisted worktree must be the renamed one, got: {}",
            persisted.worktree_path
        );
    }

    #[tokio::test]
    async fn rename_worktree_tool_rejects_bad_input_and_missing_binding() {
        let base = TempDir::new().unwrap();
        let projects = ProjectManager::with_root(base.path().join("store"));
        let session_projects: Arc<Mutex<HashMap<String, ActiveProject>>> = Arc::default();
        let storage = Arc::new(crate::session::SessionStorage::with_dir(
            base.path().join("sessions"),
        ));
        let (tx, _rx) = broadcast::channel(16);
        let tool = RenameWorktreeTool {
            projects: Arc::new(projects),
            session_id: "sess-1".to_string(),
            session_projects: session_projects.clone(),
            session_storage: storage.clone(),
            event_tx: tx,
            omega_client: None,
        };

        // Missing/empty name → clean error result, no panic.
        for input in [
            serde_json::json!({}),
            serde_json::json!({ "name": "" }),
            serde_json::json!({ "name": "   " }),
        ] {
            let result = tool.execute(&input, &mut NoopRuntime).await.unwrap();
            assert!(result.is_error, "input {input} should fail");
        }

        // No project bound to the session → clean error result.
        let result = tool
            .execute(
                &serde_json::json!({ "name": "fix-tui-crash" }),
                &mut NoopRuntime,
            )
            .await
            .unwrap();
        assert!(result.is_error, "unbound session should fail");
    }

    /// Run a git command in `dir` and return trimmed stdout (test helper).
    async fn git_for_test(dir: &Path, args: &[&str]) -> String {
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    // -----------------------------------------------------------------------
    // history_to_replay — resume replay normalization
    // -----------------------------------------------------------------------

    #[test]
    fn replay_keeps_simple_user_and_assistant_turns() {
        let messages = vec![
            Message::user("hello"),
            Message::assistant("hi there"),
            Message::user("how are you?"),
        ];
        let replay = history_to_replay(&messages);
        assert_eq!(replay.len(), 3);
        assert_eq!(replay[0].role, "user");
        assert_eq!(replay[0].content, "hello");
        assert_eq!(replay[1].role, "assistant");
        assert_eq!(replay[1].content, "hi there");
    }

    /// Tool results are stored as `user` messages with non-text blocks.
    /// They must NOT be replayed as empty user turns (the reported bug).
    #[test]
    fn replay_skips_empty_user_turns_from_tool_results() {
        let messages = vec![
            Message::user("actual prompt"),
            Message::assistant_with_blocks(vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
                signature: None,
            }]),
            // The user turn that carries the tool result — role "user",
            // but only ToolResult blocks (no visible text).
            Message::user_with_blocks(vec![ContentBlock::tool_result("t1", "file1\nfile2", false)]),
        ];
        let replay = history_to_replay(&messages);
        assert_eq!(replay.len(), 1, "only the real user prompt should survive");
        assert_eq!(replay[0].content, "actual prompt");
    }

    #[test]
    fn replay_skips_empty_text_messages() {
        let messages = vec![
            Message::user(""),
            Message::assistant("   "),
            Message::user("real content"),
        ];
        let replay = history_to_replay(&messages);
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].content, "real content");
    }

    /// Interrupt markers inserted by the agent are noise for a resumed chat.
    #[test]
    fn replay_skips_interrupt_marker() {
        let messages = vec![
            Message::user("do the thing"),
            Message::assistant(INTERRUPT_MARKER),
            Message::user("no wait, do the other thing"),
        ];
        let replay = history_to_replay(&messages);
        assert_eq!(replay.len(), 2);
        assert!(
            replay.iter().all(|r| r.content != INTERRUPT_MARKER),
            "interrupt marker must not be replayed"
        );
    }

    /// Assistant replies with mixed blocks (thinking + text + tool use)
    /// contribute only their text.
    #[test]
    fn replay_assistant_blocks_only_use_text() {
        let messages = vec![Message::assistant_with_blocks(vec![
            ContentBlock::Thinking {
                thinking: "let me think".into(),
                signature: "sig".into(),
            },
            ContentBlock::Text {
                text: "final answer".into(),
                cache_control: None,
            },
            ContentBlock::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
                signature: None,
            },
        ])];
        let replay = history_to_replay(&messages);
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].content, "final answer");
    }

    #[test]
    fn replay_ignores_non_user_assistant_roles() {
        let messages = vec![
            Message::user("hello"),
            Message {
                role: "system".into(),
                content: omega_llm::MessageContent::Text("sys".into()),
            },
            Message::assistant("world"),
        ];
        let replay = history_to_replay(&messages);
        assert_eq!(replay.len(), 2);
    }

    #[test]
    fn replay_trims_whitespace() {
        let messages = vec![Message::user("  padded  ")];
        let replay = history_to_replay(&messages);
        assert_eq!(replay[0].content, "padded");
    }

    // -----------------------------------------------------------------------
    // list_sessions_filtered — recency sort + search
    // -----------------------------------------------------------------------

    fn storage_with_sessions(names: &[(&str, &str)]) -> (SessionStorage, TempDir) {
        let temp = TempDir::new().unwrap();
        let storage = SessionStorage::with_dir(temp.path());
        for (id, conv) in names {
            let mut meta =
                crate::session::metadata::SessionMetadata::new(*id, "omega", "omega-tui", "d");
            if !conv.is_empty() {
                meta.set_conversation_name(*conv);
            }
            storage.save_metadata(&meta).unwrap();
            // A session only appears in listings once the user has actually
            // prompted something — give named fixtures a real user message.
            if !conv.is_empty() {
                storage.append_message(id, &Message::user(*conv)).unwrap();
            }
        }
        (storage, temp)
    }

    /// A session where the user never prompted anything must not appear in
    /// the listing — even if it has a name or assistant-only messages.
    #[test]
    fn list_sessions_excludes_empty_sessions() {
        let (storage, _t) = storage_with_sessions(&[("real", "Real chat")]);
        // A session with metadata only (no messages at all).
        let mut empty_meta =
            crate::session::metadata::SessionMetadata::new("empty", "omega", "omega-tui", "d");
        empty_meta.set_conversation_name("Empty chat");
        storage.save_metadata(&empty_meta).unwrap();
        // A session with only an assistant message (no user prompt).
        storage
            .append_message("assistant-only", &Message::assistant("hello there"))
            .unwrap();
        let assistant_meta = crate::session::metadata::SessionMetadata::new(
            "assistant-only",
            "omega",
            "omega-tui",
            "d",
        );
        storage.save_metadata(&assistant_meta).unwrap();

        let list = list_sessions_filtered(&storage, None);
        assert_eq!(list.len(), 1, "only the session with a user prompt shows");
        assert_eq!(list[0].session_id, "real");
        assert!(list[0].first_user_message.is_some());
    }

    /// Sessions must come back most-recently-updated first so the TUI can
    /// present a "nearest first" resume list.
    #[test]
    fn list_sessions_sorted_by_recency() {
        let (storage, _t) = storage_with_sessions(&[("old", "Old chat"), ("new", "New chat")]);
        // Force a distinct updated_at for "new" (touching bumps the timestamp).
        {
            let mut meta = storage.load_metadata("new").unwrap();
            meta.touch();
            storage.save_metadata(&meta).unwrap();
        }
        let list = list_sessions_filtered(&storage, None);
        assert_eq!(list.len(), 2);
        // "new" was touched after "old" was created → it must be first.
        assert_eq!(list[0].session_id, "new");
        assert_eq!(list[1].session_id, "old");
    }

    #[test]
    fn list_sessions_query_filters() {
        let (storage, _t) =
            storage_with_sessions(&[("sess-fix", "Fix the build"), ("sess-tui", "TUI tests")]);
        let list = list_sessions_filtered(&storage, Some("build"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, "sess-fix");
    }

    #[test]
    fn list_sessions_query_matches_last_message() {
        let (storage, _t) = storage_with_sessions(&[("sess-a", ""), ("sess-b", "")]);
        // Add a message to sess-a so its preview contains the searchable text.
        storage
            .append_message("sess-a", &Message::user("refactor the parser"))
            .unwrap();
        let list = list_sessions_filtered(&storage, Some("parser"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, "sess-a");
        assert!(list[0].last_message.as_deref().unwrap().contains("parser"));
    }

    #[test]
    fn list_sessions_query_blank_returns_all() {
        let (storage, _t) = storage_with_sessions(&[("s1", "A"), ("s2", "B")]);
        let list = list_sessions_filtered(&storage, Some("   "));
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn list_sessions_excludes_subagents() {
        let (storage, _t) = storage_with_sessions(&[("parent", "Parent")]);
        let sub = crate::session::metadata::SessionMetadata::new_subagent(
            "child", "helper", "Child", "d", "parent", "tool_1",
        );
        storage.save_metadata(&sub).unwrap();
        let list = list_sessions_filtered(&storage, None);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, "parent");
    }

    #[test]
    fn session_info_last_message_truncated() {
        let (storage, _t) = storage_with_sessions(&[("sess", "")]);
        let long = "x".repeat(200);
        storage
            .append_message("sess", &Message::user(&long))
            .unwrap();
        let meta = storage.load_metadata("sess").unwrap();
        let (info, blob) = build_session_info(&storage, "sess", &meta);
        let preview = info.last_message.unwrap();
        assert!(preview.chars().count() <= 81, "preview must be truncated");
        assert!(preview.ends_with('…'));
        assert!(!blob.is_empty(), "search blob must contain the message");
    }

    /// build_session_info captures the FIRST user prompt for the picker line
    /// (and the last message for search), not some later turn.
    #[test]
    fn session_info_first_user_message() {
        let (storage, _t) = storage_with_sessions(&[("sess", "")]);
        storage
            .append_message("sess", &Message::user("first prompt"))
            .unwrap();
        storage
            .append_message("sess", &Message::assistant("reply"))
            .unwrap();
        storage
            .append_message("sess", &Message::user("second prompt"))
            .unwrap();
        let meta = storage.load_metadata("sess").unwrap();
        let (info, _) = build_session_info(&storage, "sess", &meta);
        assert_eq!(info.first_user_message.as_deref(), Some("first prompt"));
        assert_eq!(info.last_message.as_deref(), Some("second prompt"));
    }

    /// A query matching an EARLIER message (not the last one) still finds the
    /// session — search covers the whole conversation.
    #[test]
    fn list_sessions_query_matches_earlier_message() {
        let (storage, _t) = storage_with_sessions(&[("sess-a", ""), ("sess-b", "")]);
        storage
            .append_message("sess-a", &Message::user("lets discuss interrupt steering"))
            .unwrap();
        storage
            .append_message("sess-a", &Message::assistant("sure"))
            .unwrap();
        storage
            .append_message("sess-b", &Message::user("unrelated"))
            .unwrap();
        storage
            .append_message("sess-b", &Message::assistant("ok"))
            .unwrap();

        let list = list_sessions_filtered(&storage, Some("steering"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, "sess-a");
        // The last message of sess-a is "sure", which doesn't match, so this
        // proves the earlier user message was searched.
        assert_eq!(list[0].last_message.as_deref(), Some("sure"));
    }
}
