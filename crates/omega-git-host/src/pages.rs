//! UI page handlers (Phase 3: repo pages).
//!
//! Page data comes from two read-only sources: the project store's bare
//! clones (via `repo.rs`) and the session store (via `sessions.rs`). All
//! rendering is server-side; context is serialized into minijinja templates.

use std::path::PathBuf;

use axum::extract::{Form, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use omega_projects::rebase::{self};
use omega_projects::{ProjectInfo, ProjectManager};

use crate::repo::{self, RefKind};
use crate::sessions::{SessionIndex, SessionMeta};
use crate::transcript;
use crate::{html_response, text_response, AppState};

const LOG_PER_PAGE: usize = 25;
const SUMMARY_COMMITS: usize = 20;

/// Query string for the log page.
#[derive(Deserialize, Default)]
pub(crate) struct PageQuery {
    page: Option<usize>,
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/// Look up a registered project and confirm its bare clone exists on disk.
async fn repo_for(
    projects: &ProjectManager,
    name: &str,
) -> Result<(ProjectInfo, PathBuf), Response> {
    let project = match projects.find(name).await {
        Ok(Some(project)) => project,
        Ok(None) => return Err(text_response(StatusCode::NOT_FOUND, "project not found\n")),
        Err(e) => {
            tracing::error!(project = %name, error = %e, "failed to look up project");
            return Err(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "project lookup failed\n",
            ));
        }
    };
    let repo = projects.repo_dir(name);
    if !repo.join("HEAD").is_file() {
        return Err(text_response(
            StatusCode::NOT_FOUND,
            "repository missing on disk\n",
        ));
    }
    Ok((project, repo))
}

fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    format!("http://{host}")
}

/// Context shared by every repo page (sr.ht repo chrome + nav).
fn base_ctx(
    name: &str,
    project: &ProjectInfo,
    default_branch: &str,
    base: &str,
    view: &str,
) -> Value {
    json!({
        "name": name,
        "origin": project.url,
        "default_branch": default_branch,
        "clone_url": format!("{base}/{name}.git"),
        "view": view,
        "nav_projects_active": false,
        "nav_sessions_active": false,
    })
}

fn commits_json(commits: &[repo::CommitInfo]) -> Value {
    json!(commits)
}

/// Human-readable size (sr.ht-style: bytes, KB, MB).
fn size_display(bytes: Option<u64>) -> String {
    match bytes {
        None => String::new(),
        Some(b) if b < 1024 => format!("{b} B"),
        Some(b) if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        Some(b) => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

/// `/{name}/` — repo summary: recent commit feed + refs/clone sidebar.
pub async fn summary(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (project, repo) = match repo_for(&state.projects, &name).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let default = repo::default_branch(&repo, project.default_branch.as_deref()).await;

    let commits = match repo::log_commits(&repo, &default, SUMMARY_COMMITS, 0).await {
        Ok(commits) => commits,
        Err(e) => {
            tracing::warn!(project = %name, error = %e, "log failed");
            Vec::new()
        }
    };
    let refs = repo::list_refs(&repo).await.unwrap_or_default();
    let branches: Vec<&repo::RefInfo> = refs
        .iter()
        .filter(|r| r.kind == RefKind::Branch && !r.name.starts_with("omega/"))
        .collect();
    let tags: Vec<&repo::RefInfo> = refs.iter().filter(|r| r.kind == RefKind::Tag).collect();

    // Session worktrees for this project, each linking to its chat transcript
    // — so a repo's sessions are reachable straight from the summary page.
    let sessions = match SessionIndex::load(&state.session_dir) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "session index unavailable");
            SessionIndex::default()
        }
    };
    let worktrees_json: Vec<Value> = sessions
        .by_project(&name)
        .iter()
        .map(|s| {
            json!({
                "session_id": s.session_id,
                "display_name": s.display_name,
                "model": s.model,
                "branch": s.branch,
                "updated_at": s.updated_at.map(|d| d.to_rfc3339()),
            })
        })
        .collect();

    let base = base_url(&headers);
    let mut ctx = base_ctx(&name, &project, &default, &base, "summary");
    ctx["commits"] = commits_json(&commits);
    ctx["branches"] = json!(branches);
    ctx["tags"] = json!(tags);
    ctx["worktrees"] = json!(worktrees_json);
    // Rebase-cron status for this project (from the imperative state file
    // plus the NixOS defaults).
    let root = state.projects.root().to_path_buf();
    let state_file = rebase::load_state(&root);
    let eff = rebase::resolve_effective(state.rebase_defaults.as_ref(), &state_file);
    ctx["rebase_enabled"] = json!(eff.projects.contains(&name));
    render(state, "summary.html", ctx)
}

/// `/{name}/refs/` — branches & tags; session worktrees grouped with the
/// chat session behind each one.
pub async fn refs_page(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (project, repo) = match repo_for(&state.projects, &name).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let default = repo::default_branch(&repo, project.default_branch.as_deref()).await;

    let refs = match repo::list_refs(&repo).await {
        Ok(refs) => refs,
        Err(e) => {
            tracing::warn!(project = %name, error = %e, "list_refs failed");
            return text_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to list refs\n");
        }
    };
    let sessions = match SessionIndex::load(&state.session_dir) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "session index unavailable");
            SessionIndex::default()
        }
    };

    let mut worktrees = Vec::new();
    let mut branches = Vec::new();
    for r in &refs {
        match r.kind {
            RefKind::Tag => continue,
            RefKind::Branch if r.name.starts_with("omega/") => {
                let session = sessions.by_branch(&r.name).map(|s| {
                    json!({
                        "session_id": s.session_id,
                        "display_name": s.display_name,
                        "model": s.model,
                        "updated": s.updated_at.map(|d| d.to_rfc3339()),
                    })
                });
                worktrees.push(json!({
                    "branch": r.name,
                    "short": r.short,
                    "subject": r.subject,
                    "date": r.date.map(|d| d.to_rfc3339()),
                    "session": session,
                }));
            }
            RefKind::Branch => branches.push(r),
        }
    }

    let mut ctx = base_ctx(&name, &project, &default, &base_url(&headers), "refs");
    ctx["worktrees"] = json!(worktrees);
    ctx["branches"] = json!(branches);
    let tags: Vec<&repo::RefInfo> = refs.iter().filter(|r| r.kind == RefKind::Tag).collect();
    ctx["tags"] = json!(tags);
    render(state, "refs.html", ctx)
}

/// `/{name}/log/{ref}` — paginated commit log.
pub async fn log_page(
    State(state): State<AppState>,
    Path((name, path)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Response {
    let (project, repo) = match repo_for(&state.projects, &name).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let default = repo::default_branch(&repo, project.default_branch.as_deref()).await;
    let segments: Vec<String> = if path.is_empty() {
        vec![default.clone()]
    } else {
        path.split('/').map(|s| s.to_string()).collect()
    };
    let (ref_name, _) = match repo::lookup_ref(&repo, &segments).await {
        Some(x) => x,
        None => return text_response(StatusCode::NOT_FOUND, "ref not found\n"),
    };

    let page = query.page.unwrap_or(1).max(1);
    let offset = (page - 1) * LOG_PER_PAGE;
    let mut commits = match repo::log_commits(&repo, &ref_name, LOG_PER_PAGE + 1, offset).await {
        Ok(commits) => commits,
        Err(e) => {
            tracing::warn!(project = %name, error = %e, "log failed");
            return text_response(StatusCode::INTERNAL_SERVER_ERROR, "log failed\n");
        }
    };
    let has_more = commits.len() > LOG_PER_PAGE;
    commits.truncate(LOG_PER_PAGE);

    let total = repo::commit_count(&repo, &ref_name).await.unwrap_or(0);
    let mut ctx = base_ctx(&name, &project, &default, &base_url(&headers), "log");
    ctx["ref"] = json!(ref_name);
    ctx["page"] = json!(page);
    ctx["has_more"] = json!(has_more);
    ctx["total"] = json!(total);
    ctx["commits"] = commits_json(&commits);
    render(state, "log.html", ctx)
}

/// `/{name}/tree/{ref}[/{path}]` — file tree at a ref.
pub async fn tree_page(
    State(state): State<AppState>,
    Path((name, path)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (project, repo) = match repo_for(&state.projects, &name).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let default = repo::default_branch(&repo, project.default_branch.as_deref()).await;
    let segments: Vec<String> = if path.is_empty() {
        vec![default.clone()]
    } else {
        path.split('/').map(|s| s.to_string()).collect()
    };
    let (ref_name, rest) = match repo::lookup_ref(&repo, &segments).await {
        Some(x) => x,
        None => return text_response(StatusCode::NOT_FOUND, "ref not found\n"),
    };
    let dir_path = rest.join("/");

    let mut entries = match repo::ls_tree(&repo, &ref_name, &dir_path).await {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(project = %name, error = %e, "ls-tree failed");
            return text_response(StatusCode::NOT_FOUND, "path not found\n");
        }
    };

    // One `git log --name-only` walk powers the "last commit" column.
    let last = repo::last_commits(&repo, &ref_name, 30).await.unwrap_or_default();
    for entry in &mut entries {
        entry.last = if entry.kind == "blob" {
            last.get(&entry.name).cloned()
        } else {
            let prefix = format!("{}/", entry.name);
            last.iter()
                .find(|(k, _)| k.starts_with(&prefix))
                .map(|(_, v)| v.clone())
        };
    }

    let prefix = if dir_path.is_empty() {
        String::new()
    } else {
        format!("{dir_path}/")
    };
    let entries_json: Vec<Value> = entries
        .iter()
        .map(|e| {
            let link_kind = if e.kind == "tree" { "tree" } else { "blob" };
            json!({
                "mode": e.mode,
                "kind": e.kind,
                "name": e.name,
                "href": format!("/{name}/{link_kind}/{ref_name}/{prefix}{}", e.name),
                "size": size_display(e.size),
                "commit": e.last.as_ref().map(|c| json!({
                    "short": c.short,
                    "subject": c.subject,
                    "date": c.date.to_rfc3339(),
                })),
            })
        })
        .collect();

    // Breadcrumbs for the tree header.
    let mut crumbs = Vec::new();
    {
        let mut acc = String::new();
        for seg in rest.iter() {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(seg);
            crumbs.push(json!({ "name": seg, "path": acc }));
        }
    }
    let parent = rest[..rest.len().saturating_sub(1)].join("/");

    let mut ctx = base_ctx(&name, &project, &default, &base_url(&headers), "tree");
    ctx["ref"] = json!(ref_name);
    ctx["path"] = json!(dir_path);
    ctx["crumbs"] = json!(crumbs);
    ctx["parent"] = json!(parent);
    ctx["entries"] = json!(entries_json);
    render(state, "tree.html", ctx)
}

/// `/{name}/blob/{ref}[/{path}]` — file content at a ref.
pub async fn blob_page(
    State(state): State<AppState>,
    Path((name, path)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (project, repo) = match repo_for(&state.projects, &name).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let default = repo::default_branch(&repo, project.default_branch.as_deref()).await;
    let segments: Vec<String> = if path.is_empty() {
        vec![default.clone()]
    } else {
        path.split('/').map(|s| s.to_string()).collect()
    };
    let (ref_name, rest) = match repo::lookup_ref(&repo, &segments).await {
        Some(x) => x,
        None => return text_response(StatusCode::NOT_FOUND, "ref not found\n"),
    };
    if rest.is_empty() {
        return text_response(StatusCode::NOT_FOUND, "missing file path\n");
    }
    let file_path = rest.join("/");

    // Resolve `ref:path` to a blob, then read it.
    let sha = match repo::git(&repo, &["rev-parse", "--verify", "--quiet", &format!("{ref_name}:{file_path}")]).await {
        Ok(sha) => sha,
        Err(_) => return text_response(StatusCode::NOT_FOUND, "path not found\n"),
    };
    let kind = repo::git(&repo, &["cat-file", "-t", &sha]).await.unwrap_or_default();
    if kind.trim() != "blob" {
        return text_response(StatusCode::NOT_FOUND, "not a blob\n");
    }
    let bytes = match repo::git_bytes(&repo, &["cat-file", "blob", &sha]).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(project = %name, error = %e, "cat-file failed");
            return text_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to read blob\n");
        }
    };
    let binary = bytes.contains(&0);
    let content = if binary {
        String::new()
    } else {
        String::from_utf8_lossy(&bytes).to_string()
    };
    let line_count = content.lines().count();

    let mut ctx = base_ctx(&name, &project, &default, &base_url(&headers), "tree");
    ctx["ref"] = json!(ref_name);
    ctx["file"] = json!(file_path);
    ctx["binary"] = json!(binary);
    ctx["size"] = json!(bytes.len());
    ctx["content"] = json!(content);
    ctx["line_numbers"] = json!((1..=line_count).collect::<Vec<_>>());
    render(state, "blob.html", ctx)
}

/// `/{name}/commit/{rev}` — commit header + diff.
pub async fn commit_page(
    State(state): State<AppState>,
    Path((name, rev)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (project, repo) = match repo_for(&state.projects, &name).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let default = repo::default_branch(&repo, project.default_branch.as_deref()).await;

    let sha = match repo::resolve_commit(&repo, &rev).await {
        Ok(sha) => sha,
        Err(_) => return text_response(StatusCode::NOT_FOUND, "commit not found\n"),
    };
    let detail = match repo::commit_detail(&repo, &sha).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(project = %name, error = %e, "commit_detail failed");
            return text_response(StatusCode::INTERNAL_SERVER_ERROR, "commit lookup failed\n");
        }
    };
    let diff = repo::commit_diff(&repo, &sha).await.unwrap_or_default();

    let mut ctx = base_ctx(&name, &project, &default, &base_url(&headers), "log");
    ctx["c"] = json!(detail.info);
    ctx["body"] = json!(detail.body);
    ctx["diff"] = json!(diff);
    render(state, "commit.html", ctx)
}

// ---------------------------------------------------------------------------
// Sessions (Phase 4)
// ---------------------------------------------------------------------------

/// `/{name}/clone/` — clone instructions.
pub async fn clone_page(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (project, repo) = match repo_for(&state.projects, &name).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let default = repo::default_branch(&repo, project.default_branch.as_deref()).await;
    let ctx = base_ctx(&name, &project, &default, &base_url(&headers), "clone");
    render(state, "clone.html", ctx)
}

/// `/{name}/sessions/` — every omega session bound to this project (its
/// worktree branch + the chat transcript behind it).
pub async fn repo_sessions(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (project, repo) = match repo_for(&state.projects, &name).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let default = repo::default_branch(&repo, project.default_branch.as_deref()).await;

    let sessions = match SessionIndex::load(&state.session_dir) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "session index unavailable");
            SessionIndex::default()
        }
    };
    let sessions_json: Vec<Value> = sessions
        .by_project(&name)
        .iter()
        .map(|s| {
            json!({
                "session_id": s.session_id,
                "display_name": s.display_name,
                "model": s.model,
                "branch": s.branch,
                "updated_at": s.updated_at.map(|d| d.to_rfc3339()),
            })
        })
        .collect();

    let mut ctx = base_ctx(&name, &project, &default, &base_url(&headers), "sessions");
    ctx["sessions"] = json!(sessions_json);
    render(state, "repo-sessions.html", ctx)
}

/// `/sessions/` — top-level sessions, newest first.
pub async fn sessions_index(State(state): State<AppState>) -> Response {
    let index = match SessionIndex::load(&state.session_dir) {
        Ok(index) => index,
        Err(e) => {
            tracing::error!(error = %e, "session index load failed");
            return text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session index unavailable\n",
            );
        }
    };
    let mut sessions: Vec<&crate::sessions::SessionInfo> = index
        .all()
        .iter()
        .filter(|s| s.parent.is_none())
        .collect();
    // Newest first; sessions without a timestamp sink to the bottom.
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));

    let sessions_json: Vec<Value> = sessions
        .iter()
        .map(|s| {
            json!({
                "session_id": s.session_id,
                "display_name": s.display_name,
                "model": s.model,
                "updated_at": s.updated_at.map(|d| d.to_rfc3339()),
                "project": s.project,
                "branch": s.branch,
                "children": s.children.len(),
            })
        })
        .collect();
    let ctx = json!({
        "sessions": sessions_json,
        "nav_projects_active": false,
        "nav_sessions_active": true,
    });
    render(state, "sessions.html", ctx)
}

/// `/sessions/{id}` — transcript view.
pub async fn session_page(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let dir = state.session_dir.join(&id);
    let meta_path = dir.join("metadata.json");
    if !meta_path.is_file() {
        return text_response(StatusCode::NOT_FOUND, "session not found\n");
    }
    let meta: SessionMeta = match serde_json::from_slice(&std::fs::read(&meta_path).unwrap_or_default())
    {
        Ok(meta) => meta,
        Err(e) => {
            tracing::warn!(session = %id, error = %e, "unreadable session metadata");
            return text_response(StatusCode::INTERNAL_SERVER_ERROR, "unreadable session\n");
        }
    };
    let info = crate::sessions::SessionInfo::from_meta(&meta);

    let history_path = dir.join("history.jsonl");
    let messages = if history_path.is_file() {
        match std::fs::read_to_string(&history_path) {
            Ok(raw) => transcript::parse_history(&raw),
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "unreadable history");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let total = messages.len();
    let truncated = total > transcript::MAX_MESSAGES;
    let shown = messages
        .iter()
        .rev()
        .take(transcript::MAX_MESSAGES)
        .collect::<Vec<_>>();
    // Fold the raw stream into the display conversation (tool results and
    // thinking attach to the assistant message that produced them).
    let display_msgs: Vec<transcript::Message> = shown
        .iter()
        .rev()
        .map(|m| (*m).clone())
        .collect();
    let display = transcript::assemble(&display_msgs);

    let messages_json: Vec<Value> = display
        .iter()
        .map(|m| {
            json!({
                "role": m.role,
                "body": m.body,
                "thinking": m.thinking,
                "tools": m.tools.iter().map(|t| json!({
                    "name": t.name,
                    "id": t.id,
                    "input": t.input,
                    "result": t.result,
                    "is_error": t.is_error,
                    "preview": t.preview,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    let session_json = json!({
        "session_id": info.session_id,
        "display_name": info.display_name,
        "agent": info.agent,
        "description": info.description,
        "model": info.model,
        "provider": info.provider,
        "created_at": info.created_at.map(|d| d.to_rfc3339()),
        "updated_at": info.updated_at.map(|d| d.to_rfc3339()),
        "parent": info.parent,
        "children": info.children,
        "project": info.project,
        "branch": info.branch,
    });
    let ctx = json!({
        "session": session_json,
        "messages": messages_json,
        "truncated": truncated,
        "total": total,
        "nav_projects_active": false,
        "nav_sessions_active": true,
    });
    render(state, "session.html", ctx)
}

/// `/sessions/{id}/system-prompt` — the prompt this session was started with.
pub async fn session_system_prompt(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let path = state.session_dir.join(&id).join("system_prompt.md");
    let prompt = match std::fs::read_to_string(&path) {
        Ok(p) => p,
        Err(_) => return text_response(StatusCode::NOT_FOUND, "system prompt not found\n"),
    };
    let ctx = json!({
        "session_id": id,
        "prompt": prompt,
        "nav_projects_active": false,
        "nav_sessions_active": true,
    });
    render(state, "session-prompt.html", ctx)
}

// ---------------------------------------------------------------------------
// Rebase cron — status page + imperative controls
// ---------------------------------------------------------------------------

/// Form for `/rebase/toggle`: enable or disable one project's cron rebase.
#[derive(Deserialize)]
pub(crate) struct RebaseToggleForm {
    project: String,
    enabled: String,
}

/// Form for `/rebase/interval`: set the interval in seconds.
#[derive(Deserialize)]
pub(crate) struct RebaseIntervalForm {
    interval_seconds: u64,
}

/// The rebaser session id omega-loop uses for a project (same derivation as
/// `rebase_job::session_id_for` — keep in sync).
fn rebaser_session_id(project: &str) -> String {
    let mut out = String::from("rebaser-");
    for c in project.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    out
}

/// `GET /rebase` — current cron config, per-project toggles, interval
/// editor, "run now", and a summary of the last run.
pub async fn rebase_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let root = state.projects.root().to_path_buf();
    let state_file = rebase::load_state(&root);
    let eff = rebase::resolve_effective(state.rebase_defaults.as_ref(), &state_file);

    let mut projects = Vec::new();
    let registered: Vec<String> = match state.projects.list().await {
        Ok(list) => list.into_iter().map(|p| p.name).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "list projects for rebase page");
            Vec::new()
        }
    };
    for name in &eff.projects {
        projects.push(json!({
            "name": name,
            "registered": registered.contains(name),
            "from_config": !state_file.project_states.contains_key(name),
        }));
    }
    // Projects the user can add: registered but not currently enabled.
    let addable: Vec<String> = registered
        .into_iter()
        .filter(|n| !eff.projects.contains(n))
        .collect();

    // Link to each project's upstream-rebaser chat transcript, if it has
    // one (created lazily by omega-loop on first divergence).
    let sessions = SessionIndex::load(&state.session_dir).unwrap_or_default();
    let rebasers: Vec<Value> = projects
        .iter()
        .map(|p| {
            let id = rebaser_session_id(p["name"].as_str().unwrap_or_default());
            json!({
                "project": p["name"],
                "session_id": id,
                "exists": sessions.all().iter().any(|s| s.session_id == id),
            })
        })
        .collect();

    // Can this service write the imperative state file? (The NixOS service runs
// the store read-only apart from the state file; standalone runs are on the
// same filesystem as the daemon and can write.)
    use std::os::unix::fs::PermissionsExt;
    let writable = std::fs::metadata(&root)
        .map(|m| m.permissions().mode() & 0o200 != 0)
        .unwrap_or(false);

    let base = base_url(&headers);
    let ctx = json!({
        "base": base,
        "interval_seconds": eff.interval_seconds,
        "interval_display": format_interval(eff.interval_seconds),
        "projects": projects,
        "addable": addable,
        "rebasers": rebasers,
        "last_run": state_file.last_run,
        "defaults_present": state.rebase_defaults.is_some(),
        "agent_prompt": state.rebase_defaults.as_ref().map(|d| d.agent_prompt.clone()),
        "writable": writable,
        "nav_projects_active": false,
        "nav_rebase_active": true,
        "nav_sessions_active": false,
    });
    render(state, "rebase.html", ctx)
}

/// Human "6h", "30m" display of a seconds interval.
fn format_interval(seconds: u64) -> String {
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// `POST /rebase/toggle` — enable/disable a project's cron rebase in the
/// imperative state file.
pub async fn rebase_toggle(
    State(state): State<AppState>,
    Form(form): Form<RebaseToggleForm>,
) -> Response {
    let project = form.project.trim();
    if project.is_empty() {
        return Redirect::to("/rebase").into_response();
    }
    let enabled = form.enabled == "true" || form.enabled == "1" || form.enabled == "on";
    let root = state.projects.root().to_path_buf();
    let mut state_file = rebase::load_state(&root);
    state_file
        .project_states
        .insert(project.to_string(), enabled);
    match rebase::save_state(&root, &state_file) {
        Ok(()) => {
            tracing::info!(project, enabled, "rebase cron toggle");
            Redirect::to("/rebase").into_response()
        }
        Err(e) => {
            tracing::error!(project, enabled, error = %e, "could not save rebase state");
            text_response(StatusCode::INTERNAL_SERVER_ERROR, "could not save rebase state\n")
        }
    }
}

/// `POST /rebase/interval` — override the cron interval (seconds) in the
/// imperative state file.
pub async fn rebase_interval(
    State(state): State<AppState>,
    Form(form): Form<RebaseIntervalForm>,
) -> Response {
    if form.interval_seconds == 0 {
        return Redirect::to("/rebase").into_response();
    }
    let root = state.projects.root().to_path_buf();
    let mut state_file = rebase::load_state(&root);
    state_file.interval_seconds = Some(form.interval_seconds);
    match rebase::save_state(&root, &state_file) {
        Ok(()) => {
            tracing::info!(interval = form.interval_seconds, "rebase cron interval set");
            Redirect::to("/rebase").into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "could not save rebase state");
            text_response(StatusCode::INTERNAL_SERVER_ERROR, "could not save rebase state\n")
        }
    }
}

/// `POST /rebase/run` — drop a `rebase-now` marker; omega-loop's cron picks
/// it up within seconds and runs immediately.
pub async fn rebase_run_now(State(state): State<AppState>) -> Response {
    let root = state.projects.root().to_path_buf();
    match std::fs::write(rebase::run_now_path(&root), b"") {
        Ok(()) => {
            tracing::info!("rebase-now marker written");
            Redirect::to("/rebase").into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "could not write run-now marker");
            text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not write run-now marker\n",
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Render helper
// ---------------------------------------------------------------------------

fn render(state: AppState, template: &str, ctx: Value) -> Response {
    match state.templates.render(template, ctx) {
        Ok(html) => html_response(html),
        Err(e) => {
            tracing::error!(template, error = %format!("{e:#}"), "template render failed");
            text_response(StatusCode::INTERNAL_SERVER_ERROR, "template error\n")
        }
    }
}
