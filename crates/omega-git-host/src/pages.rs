//! UI page handlers (Phase 3: repo pages).
//!
//! Page data comes from two read-only sources: the project store's bare
//! clones (via `repo.rs`) and the session store (via `sessions.rs`). All
//! rendering is server-side; context is serialized into minijinja templates.

use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Form, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use omega_loop_client::{OutputChunk, ServerEvent};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use omega_projects::mirror::TokenUpdate;
use omega_projects::rebase::{self};
use omega_projects::{ActiveProject, ProjectInfo, ProjectManager};

use crate::changes;
use crate::daemon;
use crate::repo::{self, RefKind};
use crate::sessions::SessionIndex;
use crate::transcript;
use crate::{html_response, text_response, AppState};

/// Minimal `/` query string for the project index: one-shot flash notices
/// set by the project create/delete redirects.
#[derive(Deserialize, Default)]
pub(crate) struct NoticeQuery {
    pub(crate) error: Option<String>,
    pub(crate) ok: Option<String>,
}

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

/// Browser form posts must be same-origin when an Origin header is present.
/// The service is loopback-only by default, but this prevents a malicious
/// website open in the user's browser from silently scheduling local work.
fn reject_cross_origin(headers: &HeaderMap) -> Option<Response> {
    let origin_value = headers.get(header::ORIGIN)?;
    let Ok(origin) = origin_value.to_str() else {
        return Some(text_response(
            StatusCode::FORBIDDEN,
            "invalid Origin header\n",
        ));
    };
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return Some(text_response(
            StatusCode::FORBIDDEN,
            "missing Host header\n",
        ));
    };
    let Some(origin_host) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return Some(text_response(
            StatusCode::FORBIDDEN,
            "invalid Origin header\n",
        ));
    };
    let origin_host = origin_host.strip_suffix('/').unwrap_or(origin_host);
    if origin_host.contains('/') || origin_host.contains('?') || origin_host.contains('#') {
        return Some(text_response(
            StatusCode::FORBIDDEN,
            "invalid Origin header\n",
        ));
    }
    if origin_host.eq_ignore_ascii_case(host) {
        None
    } else {
        Some(text_response(
            StatusCode::FORBIDDEN,
            "cross-origin form post rejected\n",
        ))
    }
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
    Query(query): Query<NoticeQuery>,
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
    let config = rebase::resolve_project(
        state.rebase_defaults.as_ref(),
        &state_file,
        &name,
        eff.contains(&name),
    );
    let run = state_file.project_runs.get(&name);
    let status = run
        .and_then(|r| {
            r.pending_since.map(|_| "queued".to_string()).or_else(|| {
                r.result
                    .as_ref()?
                    .get("status")?
                    .as_str()
                    .map(str::to_string)
            })
        })
        .unwrap_or_else(|| "never run".to_string());
    ctx["rebase_enabled"] = json!(config.enabled);
    ctx["rebase_interval"] = json!(format_interval(config.interval_seconds));
    ctx["rebase_status"] = json!(status);
    ctx["rebase_outcome"] = json!(run
        .and_then(|r| r.result.as_ref())
        .and_then(|r| r.get("outcome"))
        .and_then(Value::as_str));
    ctx["recurring_session"] = json!(recurring_session_id(&name));
    let mirror = state.mirrors.project(&name);
    ctx["mirror_configured"] = json!(mirror.is_some());
    ctx["mirror_url"] = json!(mirror.as_ref().map(|m| m.url.as_str()));
    ctx["mirror_auto_push"] = json!(mirror.as_ref().is_some_and(|m| m.auto_push));
    ctx["mirror_has_token"] = json!(state.mirrors.has_any_token(&name));
    ctx["mirror_has_project_token"] = json!(state.mirrors.has_project_token(&name));
    ctx["mirror_status"] = json!(mirror
        .as_ref()
        .and_then(|m| m.result.as_ref())
        .map(|r| r.status.as_str())
        .unwrap_or("never pushed"));
    ctx["mirror_detail"] = json!(mirror
        .as_ref()
        .and_then(|m| m.result.as_ref())
        .and_then(|r| r.detail.as_deref()));
    ctx["mirror_last_success"] = json!(mirror
        .as_ref()
        .and_then(|m| m.last_success_at)
        .map(|at| at.to_rfc3339()));
    ctx["error"] = json!(query.error);
    ctx["ok"] = json!(query.ok);
    render(state, "summary.html", ctx)
}

/// `/{name}/refs/` — branches & tags; session worktrees grouped with the
/// chat session behind each one.
pub async fn refs_page(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Query(query): Query<NoticeQuery>,
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
                    "merged": repo::git_ok(
                        &repo,
                        &["merge-base", "--is-ancestor", &r.name, &default],
                    ).await,
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
    ctx["error"] = json!(query.error);
    ctx["ok"] = json!(query.ok);
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
    let last = repo::last_commits(&repo, &ref_name, 30)
        .await
        .unwrap_or_default();
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
    let sha = match repo::git(
        &repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{ref_name}:{file_path}"),
        ],
    )
    .await
    {
        Ok(sha) => sha,
        Err(_) => return text_response(StatusCode::NOT_FOUND, "path not found\n"),
    };
    let kind = repo::git(&repo, &["cat-file", "-t", &sha])
        .await
        .unwrap_or_default();
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
/// worktree branch + the chat transcript behind it), plus a form to start a
/// brand-new session on the project.
pub async fn repo_sessions(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Query(query): Query<NoticeQuery>,
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
    ctx["error"] = json!(query.error);
    let (models, models_error) = available_models("").await;
    ctx["models"] = json!(models);
    ctx["models_error"] = json!(models_error);
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
    let mut sessions: Vec<&crate::sessions::SessionInfo> =
        index.all().iter().filter(|s| s.parent.is_none()).collect();
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
/// `/sessions/{id}` — transcript view.
pub async fn session_page(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<NoticeQuery>,
) -> Response {
    match transcript_context(&state, &id) {
        TranscriptResult::NotFound => text_response(StatusCode::NOT_FOUND, "session not found\n"),
        TranscriptResult::Unreadable => {
            text_response(StatusCode::INTERNAL_SERVER_ERROR, "unreadable session\n")
        }
        TranscriptResult::Found {
            session_json,
            messages_json,
            truncated,
            total,
        } => {
            let current_model = session_json["model"].as_str().unwrap_or("");
            let (models, models_error) = available_models(current_model).await;
            let ctx = json!({
                "session": session_json,
                "messages": messages_json,
                "truncated": truncated,
                "total": total,
                "models": models,
                "models_error": models_error,
                "error": query.error,
                "ok": query.ok,
                "nav_projects_active": false,
                "nav_sessions_active": true,
            });
            render(state, "session.html", ctx)
        }
    }
}

/// `/sessions/{id}/changes` — a fresh, bounded snapshot of everything the
/// session branch and worktree have changed so far. This is intentionally a
/// normal GET: the page is read-only and its Refresh link simply repeats the
/// collection against current Git state.
pub async fn session_changes(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match changes::load_page(&state.projects, &state.session_dir, &id).await {
        Ok(page) => {
            let ctx = json!({
                "page": page,
                "nav_projects_active": false,
                "nav_sessions_active": true,
            });
            let mut response = render(state, "session-changes.html", ctx);
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(changes::PageError::NotFound) => {
            text_response(StatusCode::NOT_FOUND, "session not found\n")
        }
        Err(changes::PageError::Invalid(message)) => {
            tracing::warn!(session = %id, reason = %message, "changes page rejected session metadata");
            text_response(StatusCode::BAD_REQUEST, format!("{message}\n"))
        }
    }
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
// Live chat — TUI capabilities in the web UI
// ---------------------------------------------------------------------------

/// Short random suffix for a fresh web session id.
fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{nanos:06x}")
}

fn sanitize_id(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        "session".to_string()
    } else {
        out
    }
}

const MAX_SESSION_ID_BYTES: usize = 128;
const MAX_MODEL_BYTES: usize = 256;
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID_BYTES
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && id != "."
        && id != ".."
}

/// Confirm that a mutation path names the same persisted session as its
/// metadata. This rejects traversal-shaped/aliased ids and never falls back
/// to another chat.
fn exact_session_exists(state: &AppState, id: &str) -> bool {
    if !valid_session_id(id) {
        return false;
    }
    let path = state.session_dir.join(id).join("metadata.json");
    std::fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<crate::sessions::SessionMeta>(&raw).ok())
        .is_some_and(|meta| meta.session_id == id)
}

async fn available_models(current: &str) -> (Vec<String>, Option<String>) {
    match daemon::list_models().await {
        Ok(mut models) => {
            models.retain(|m| {
                !m.is_empty() && m.len() <= MAX_MODEL_BYTES && !m.chars().any(char::is_control)
            });
            models.sort();
            models.dedup();
            if !current.is_empty() && !models.iter().any(|m| m == current) {
                models.insert(0, current.to_string());
            }
            (models, None)
        }
        Err(e) => {
            let models = if current.is_empty() {
                Vec::new()
            } else {
                vec![current.to_string()]
            };
            (models, Some(e))
        }
    }
}

/// Form for `POST /{name}/sessions/create`: an optional initial prompt for
/// the brand-new session.
#[derive(Deserialize)]
pub(crate) struct SessionCreateForm {
    prompt: Option<String>,
    model: Option<String>,
}

/// Form for `POST /sessions/{id}/message`.
#[derive(Deserialize)]
pub(crate) struct MessageForm {
    content: String,
}

#[derive(Deserialize)]
pub(crate) struct ModelForm {
    model: String,
}

/// `POST /{name}/sessions/create` — start a brand-new session bound to this
/// project (the daemon creates a dedicated git worktree). The session keeps
/// running in the daemon even after the page is closed; the user arrives on
/// the live chat page.
pub async fn session_create(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<SessionCreateForm>,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    if state.projects.find(&name).await.ok().flatten().is_none() {
        return text_response(StatusCode::NOT_FOUND, "project not found\n");
    }
    let requested_model = form
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    if let Some(model) = requested_model {
        if model.len() > MAX_MODEL_BYTES || model.chars().any(char::is_control) {
            return text_response(StatusCode::BAD_REQUEST, "invalid model\n");
        }
        match daemon::list_models().await {
            Ok(models) if models.iter().any(|m| m == model) => {}
            Ok(_) => return text_response(StatusCode::BAD_REQUEST, "unsupported model\n"),
            Err(e) => {
                return Redirect::to(&format!("/{name}/sessions?error={}", url_encode(&e)))
                    .into_response()
            }
        }
    }
    if form
        .prompt
        .as_deref()
        .is_some_and(|p| p.len() > MAX_MESSAGE_BYTES)
    {
        return text_response(StatusCode::PAYLOAD_TOO_LARGE, "prompt too large\n");
    }
    let id = format!("web-{}-{}", sanitize_id(&name), &rand_suffix()[..6]);
    match daemon::create_session_on_project(&name, &id).await {
        Ok(Ok(())) => {
            if let Some(model) = requested_model {
                if let Err(e) = daemon::set_model(&id, model).await {
                    return Redirect::to(&format!("/sessions/{id}/live?error={}", url_encode(&e)))
                        .into_response();
                }
            }
            // Optional first prompt into the freshly-bound session.
            if let Some(p) = form
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
            {
                let _ = daemon::send_message(&id, p, None).await;
            }
            Redirect::to(&format!("/sessions/{id}/live")).into_response()
        }
        Ok(Err(msg)) => {
            Redirect::to(&format!("/{name}/sessions?error={}", url_encode(&msg))).into_response()
        }
        Err(e) => {
            Redirect::to(&format!("/{name}/sessions?error={}", url_encode(&e))).into_response()
        }
    }
}

/// `GET /sessions/{id}/live` — interactive chat page: the persisted
/// transcript plus a message box, an interrupt button, and an SSE stream that
/// appends the agent's live output.
pub async fn session_live(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<NoticeQuery>,
) -> Response {
    let ctx = match transcript_context(&state, &id) {
        TranscriptResult::NotFound => {
            return text_response(StatusCode::NOT_FOUND, "session not found\n");
        }
        TranscriptResult::Unreadable => {
            return text_response(StatusCode::INTERNAL_SERVER_ERROR, "unreadable session\n");
        }
        TranscriptResult::Found {
            session_json,
            messages_json,
            truncated,
            total,
        } => {
            let current_model = session_json["model"].as_str().unwrap_or("");
            let (models, models_error) = available_models(current_model).await;
            json!({
                "session": session_json,
                "messages": messages_json,
                "truncated": truncated,
                "total": total,
                "live": true,
                "stream_url": format!("/sessions/{id}/stream"),
                "models": models,
                "models_error": models_error,
                "error": query.error,
                "ok": query.ok,
                "nav_projects_active": false,
                "nav_sessions_active": true,
            })
        }
    };
    render(state, "session.html", ctx)
}

/// `GET /sessions/{id}/stream` — SSE feed for the live chat page. Resumes the
/// session in the daemon (attaching to its live output) and pumps events to
/// the browser. The daemon connection stays open; it closes when the client
/// disconnects or the daemon does.
pub async fn session_stream(State(_state): State<AppState>, Path(id): Path<String>) -> Response {
    let sid = id.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(64);
    tokio::spawn(async move {
        // Heartbeat: keep an idle (no events) stream alive through proxies.
        let heartbeat_tx = tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            loop {
                interval.tick().await;
                if heartbeat_tx
                    .send(Ok(": ping\n\n".to_string()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let (mut reader, _writer) = match daemon::open_stream(&sid).await {
            Ok(x) => x,
            Err(e) => {
                let _ = tx
                    .send(Ok(format!(
                        "data: {}\n\n",
                        json!({ "type": "error", "message": e })
                    )))
                    .await;
                return;
            }
        };
        // Tells the JS to reset the live pane on (re)connect.
        let _ = tx
            .send(Ok(format!("data: {}\n\n", json!({ "type": "cleared" }))))
            .await;
        while let Ok(Some(event)) = reader.recv_event().await {
            for payload in sse_payloads(&event) {
                if tx.send(Ok(format!("data: {payload}\n\n"))).await.is_err() {
                    return;
                }
            }
        }
    });
    let stream = ReceiverStream::new(rx);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Convert a daemon event into one or more SSE JSON payloads for the live
/// chat page.
///
/// The payloads mirror the [`transcript::DisplayMessage`] model the
/// read-only session page is built from (body text, thinking trace, tool
/// calls), so the client-side renderer can build the *same* per-message
/// layout the server-rendered transcript uses — one assistant block with its
/// own thinking `<details>` and tool `<details>`, instead of a single
/// undifferentiated text dump. Tool events (previously dropped!) are
/// forwarded so live tool calls show up exactly like persisted ones.
fn sse_payloads(event: &ServerEvent) -> Vec<Value> {
    match event {
        ServerEvent::Chunk { chunk, .. } => match chunk.as_known() {
            Some(chunk) => match chunk {
            OutputChunk::TextDelta(t) => vec![json!({ "type": "text", "text": t })],
            // Carries the authoritative full text block (empty when the
            // deltas already covered it, e.g. joined at block start).
            OutputChunk::TextComplete(t) => {
                vec![json!({ "type": "text_complete", "text": t })]
            }
            OutputChunk::ThinkingDelta(t) => vec![json!({ "type": "thinking", "text": t })],
            OutputChunk::ThinkingComplete(t) => {
                vec![json!({ "type": "thinking_complete", "text": t })]
            }
            OutputChunk::ToolStart { id, name, input } => {
                vec![json!({ "type": "tool_start", "id": id, "name": name, "input": input })]
            }
            OutputChunk::ToolProgress { id, output } => {
                vec![json!({ "type": "tool_progress", "id": id, "output": output })]
            }
            OutputChunk::ToolEnd {
                id,
                name,
                input,
                result,
            } => {
                // Send the same truncated result + one-line preview the
                // persisted transcript renders, so live and saved tool calls
                // look identical (and the browser never renders megabytes).
                let result_text = omega_loop_client::tool_result_text(result);
                let text = transcript::truncate(&result_text);
                let preview = transcript::preview_of(&text, result.is_error);
                vec![json!({
                    "type": "tool_end",
                    "id": id,
                    "name": name,
                    "input": input,
                    "result": text,
                    "is_error": result.is_error,
                    "preview": preview,
                })]
            }
            OutputChunk::Status(s) => vec![json!({ "type": "status", "message": s })],
            OutputChunk::Error(e) => vec![json!({ "type": "error", "message": e })],
            OutputChunk::Done => vec![json!({ "type": "done" })],
            // Cache telemetry and non-chat core chunks carry no chat content.
            _ => vec![],
            },
            None => vec![],
        },
        ServerEvent::SystemMsg { message } => {
            vec![json!({ "type": "status", "message": message })]
        }
        ServerEvent::ModelChanged { session_id, model } => {
            vec![json!({ "type": "model_changed", "session_id": session_id, "model": model })]
        }
        ServerEvent::SessionCompacted { session_id } => {
            vec![json!({ "type": "compacted", "session_id": session_id })]
        }
        _ => vec![],
    }
}

/// `POST /sessions/{id}/message` — send a chat message to the (live, detached)
/// session. The reply streams back over the SSE feed.
pub async fn session_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<MessageForm>,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    if !exact_session_exists(&state, &id) {
        return text_response(StatusCode::NOT_FOUND, "session not found\n");
    }
    if form.content.len() > MAX_MESSAGE_BYTES {
        return text_response(StatusCode::PAYLOAD_TOO_LARGE, "message too large\n");
    }
    let content = form.content.trim().to_string();
    if content.is_empty() {
        return Redirect::to(&format!("/sessions/{id}/live")).into_response();
    }
    match daemon::send_message(&id, &content, None).await {
        Ok(()) => Redirect::to(&format!("/sessions/{id}/live")).into_response(),
        Err(e) => {
            Redirect::to(&format!("/sessions/{id}/live?error={}", url_encode(&e))).into_response()
        }
    }
}

/// `POST /sessions/{id}/interrupt` — stop the running turn, like the TUI's
/// Ctrl-C.
pub async fn session_interrupt(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    if !exact_session_exists(&state, &id) {
        return text_response(StatusCode::NOT_FOUND, "session not found\n");
    }
    match daemon::interrupt(&id).await {
        Ok(()) => Redirect::to(&format!("/sessions/{id}/live")).into_response(),
        Err(e) => {
            Redirect::to(&format!("/sessions/{id}/live?error={}", url_encode(&e))).into_response()
        }
    }
}

/// Change the provider model for exactly this session.
pub async fn session_set_model(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<ModelForm>,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    if !exact_session_exists(&state, &id) {
        return text_response(StatusCode::NOT_FOUND, "session not found\n");
    }
    let model = form.model.trim();
    if model.is_empty() || model.len() > MAX_MODEL_BYTES || model.chars().any(char::is_control) {
        return text_response(StatusCode::BAD_REQUEST, "invalid model\n");
    }
    match daemon::list_models().await {
        Ok(models) if !models.iter().any(|m| m == model) => {
            return text_response(StatusCode::BAD_REQUEST, "unsupported model\n")
        }
        Err(e) => {
            return Redirect::to(&format!("/sessions/{id}/live?error={}", url_encode(&e)))
                .into_response()
        }
        _ => {}
    }
    match daemon::set_model(&id, model).await {
        Ok(selected) => Redirect::to(&format!(
            "/sessions/{id}/live?ok={}",
            url_encode(&format!("Model changed to {selected}"))
        ))
        .into_response(),
        Err(e) => {
            Redirect::to(&format!("/sessions/{id}/live?error={}", url_encode(&e))).into_response()
        }
    }
}

/// Summarize and replace this idle session's history.
pub async fn session_compact(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    if !exact_session_exists(&state, &id) {
        return text_response(StatusCode::NOT_FOUND, "session not found\n");
    }
    match daemon::compact(&id).await {
        Ok(()) => Redirect::to(&format!(
            "/sessions/{id}/live?ok={}",
            url_encode("Session compacted")
        ))
        .into_response(),
        Err(e) => {
            Redirect::to(&format!("/sessions/{id}/live?error={}", url_encode(&e))).into_response()
        }
    }
}

/// Outcome of [`transcript_context`]: either the transcript is ready to
/// render, or the session store doesn't carry it (or has it but unreadable).
enum TranscriptResult {
    /// No `metadata.json` for this session.
    NotFound,
    /// `metadata.json` exists but couldn't be parsed.
    Unreadable,
    /// Everything needed to render the transcript/chat page.
    Found {
        session_json: Value,
        messages_json: Value,
        truncated: bool,
        total: usize,
    },
}

/// Shared transcript building for the read-only session page and the live
/// chat page: reads `metadata.json` + `history.jsonl` and folds the raw
/// stream into the display conversation (tool results and thinking attach to
/// the assistant message that produced them).
fn transcript_context(state: &AppState, id: &str) -> TranscriptResult {
    let dir = state.session_dir.join(id);
    let meta_path = dir.join("metadata.json");
    if !meta_path.is_file() {
        return TranscriptResult::NotFound;
    }
    let meta: crate::sessions::SessionMeta =
        match serde_json::from_slice(&std::fs::read(&meta_path).unwrap_or_default()) {
            Ok(meta) => meta,
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "unreadable session metadata");
                return TranscriptResult::Unreadable;
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
    let display_msgs: Vec<transcript::Message> = shown.iter().rev().map(|m| (*m).clone()).collect();
    let display = transcript::assemble(&display_msgs);
    let messages_json: Value = display
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
    TranscriptResult::Found {
        session_json,
        messages_json,
        truncated,
        total,
    }
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

/// Complete per-project schedule + conflict prompt editor.
#[derive(Deserialize)]
pub(crate) struct RebaseConfigureForm {
    project: String,
    enabled: String,
    interval_seconds: u64,
    #[serde(default)]
    prompt: String,
}

#[derive(Deserialize, Default)]
pub(crate) struct RebaseRunForm {
    project: Option<String>,
}

/// The recurring chat session id omega-loop uses for a project (same derivation as
/// `rebase_job::session_id_for` — keep in sync).
fn recurring_session_id(project: &str) -> String {
    let mut out = String::from("recurring-");
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

    let registered = match state.projects.list().await {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!(error = %e, "list projects for rebase page");
            Vec::new()
        }
    };
    let sessions = SessionIndex::load(&state.session_dir).unwrap_or_default();
    let mut projects = Vec::new();
    for info in registered {
        let name = &info.name;
        let config = rebase::resolve_project(
            state.rebase_defaults.as_ref(),
            &state_file,
            name,
            eff.contains(name),
        );
        let run = state_file.project_runs.get(name);
        let run_status = run
            .and_then(|r| {
                r.pending_since.map(|_| "queued".to_string()).or_else(|| {
                    r.result
                        .as_ref()?
                        .get("status")?
                        .as_str()
                        .map(str::to_string)
                })
            })
            .unwrap_or_else(|| "never run".to_string());
        let run_outcome = run
            .and_then(|r| r.result.as_ref())
            .and_then(|r| r.get("outcome"))
            .and_then(Value::as_str);
        let next_run_at = if config.enabled {
            run.and_then(|r| r.last_started_at)
                .map(|last| last + chrono::Duration::seconds(config.interval_seconds as i64))
                .map(|next| next.to_rfc3339())
                .or_else(|| Some("as soon as omega-loop polls".to_string()))
        } else {
            None
        };
        let session_id = recurring_session_id(name);
        projects.push(json!({
            "name": name,
            "upstream_url": info.url,
            "default_branch": info.default_branch,
            "enabled": config.enabled,
            "interval_seconds": config.interval_seconds,
            "interval_display": format_interval(config.interval_seconds),
            "prompt": config.prompt,
            "status": run_status,
            "outcome": run_outcome,
            "detail": run.and_then(|r| r.result.as_ref()).and_then(|r| r.get("detail")),
            "last_started_at": run.and_then(|r| r.last_started_at).map(|d| d.to_rfc3339()),
            "last_finished_at": run.and_then(|r| r.last_finished_at).map(|d| d.to_rfc3339()),
            "next_run_at": next_run_at,
            "trigger": run.and_then(|r| r.last_trigger.as_deref()),
            "session_id": session_id,
            "session_exists": sessions.all().iter().any(|s| s.session_id == session_id),
        }));
    }

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
    headers: HeaderMap,
    Form(form): Form<RebaseToggleForm>,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    let project = form.project.trim();
    if state.projects.find(project).await.ok().flatten().is_none() {
        return text_response(StatusCode::BAD_REQUEST, "unknown project\n");
    }
    let enabled = form.enabled == "true" || form.enabled == "1" || form.enabled == "on";
    let root = state.projects.root().to_path_buf();
    match rebase::update_state(&root, |state_file| {
        state_file
            .project_states
            .insert(project.to_string(), enabled);
        state_file
            .project_configs
            .entry(project.to_string())
            .or_default()
            .enabled = Some(enabled);
        Ok(())
    }) {
        Ok(()) => {
            tracing::info!(project, enabled, "rebase cron toggle");
            Redirect::to("/rebase").into_response()
        }
        Err(e) => {
            tracing::error!(project, enabled, error = %e, "could not save rebase state");
            text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not save rebase state\n",
            )
        }
    }
}

/// `POST /rebase/interval` — override the cron interval (seconds) in the
/// imperative state file.
pub async fn rebase_interval(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RebaseIntervalForm>,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    if let Err(e) = rebase::validate_interval(form.interval_seconds) {
        return text_response(StatusCode::BAD_REQUEST, format!("{e}\n"));
    }
    let root = state.projects.root().to_path_buf();
    match rebase::update_state(&root, |state_file| {
        state_file.interval_seconds = Some(form.interval_seconds);
        Ok(())
    }) {
        Ok(()) => {
            tracing::info!(interval = form.interval_seconds, "rebase cron interval set");
            Redirect::to("/rebase").into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "could not save rebase state");
            text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not save rebase state\n",
            )
        }
    }
}

/// `POST /rebase/configure` — atomically update one project's automatic
/// cadence and conflict-resolution prompt.
pub async fn rebase_configure(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RebaseConfigureForm>,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    let project = form.project.trim();
    if state.projects.find(project).await.ok().flatten().is_none() {
        return text_response(StatusCode::BAD_REQUEST, "unknown project\n");
    }
    if let Err(e) = rebase::validate_interval(form.interval_seconds) {
        return text_response(StatusCode::BAD_REQUEST, format!("{e}\n"));
    }
    if let Err(e) = rebase::validate_prompt(&form.prompt) {
        return text_response(StatusCode::BAD_REQUEST, format!("{e}\n"));
    }
    let enabled = matches!(form.enabled.as_str(), "true" | "1" | "on");
    let prompt = form.prompt.trim().to_string();
    let root = state.projects.root().to_path_buf();
    match rebase::update_state(&root, |stored| {
        let config = stored
            .project_configs
            .entry(project.to_string())
            .or_default();
        config.enabled = Some(enabled);
        config.interval_seconds = Some(form.interval_seconds);
        config.prompt = (!prompt.is_empty()).then_some(prompt.clone());
        Ok(())
    }) {
        Ok(()) => Redirect::to("/rebase").into_response(),
        Err(e) => {
            tracing::error!(project, error = %e, "could not save rebase project config");
            text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not save rebase state\n",
            )
        }
    }
}

/// `POST /rebase/run` — durably queue one registered project. An omitted
/// project is retained as a backwards-compatible "all enabled" action.
pub async fn rebase_run_now(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RebaseRunForm>,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    let root = state.projects.root().to_path_buf();
    let state_file = rebase::load_state(&root);
    let effective = rebase::resolve_effective(state.rebase_defaults.as_ref(), &state_file);
    let projects: Vec<String> = if let Some(project) = form
        .project
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        if state.projects.find(project).await.ok().flatten().is_none() {
            return text_response(StatusCode::BAD_REQUEST, "unknown project\n");
        }
        vec![project.to_string()]
    } else {
        effective.projects.iter().map(|p| p.name.clone()).collect()
    };
    if projects.is_empty() {
        return text_response(StatusCode::BAD_REQUEST, "no projects selected\n");
    }
    let now = chrono::Utc::now();
    match rebase::update_state(&root, |stored| {
        for project in &projects {
            let run = stored.project_runs.entry(project.clone()).or_default();
            run.pending_since = Some(now);
            run.pending_trigger = Some("manual".to_string());
            run.result = Some(json!({ "status": "queued" }));
        }
        Ok(())
    }) {
        Ok(()) => {
            tracing::info!(?projects, "manual upstream rebase queued");
            Redirect::to("/rebase").into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "could not queue rebase run");
            text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not queue rebase run\n",
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Project lifecycle (web control panel)
// ---------------------------------------------------------------------------

/// Form for `/projects/create`: a friendly name (optional) and the upstream
/// git URL the project should track (and periodically rebase against).
#[derive(Deserialize)]
pub(crate) struct ProjectCreateForm {
    name: Option<String>,
    url: String,
}

/// Form for `/projects/delete`.
#[derive(Deserialize)]
pub(crate) struct ProjectDeleteForm {
    name: String,
}

/// Form for `/{name}/merge`: merge one session worktree branch into the
/// project's default branch so it becomes fetchable.
#[derive(Deserialize)]
pub(crate) struct MergeForm {
    branch: String,
    message: Option<String>,
}

/// Form for `/{name}/worktrees/delete`.
#[derive(Deserialize)]
pub(crate) struct WorktreeDeleteForm {
    branch: String,
}

#[derive(Deserialize)]
pub(crate) struct MirrorConfigureForm {
    mirror: String,
    auto_push: Option<String>,
    token: Option<String>,
    remove_token: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct MirrorRemoveForm {
    remove_token: Option<String>,
}

/// `POST /projects/create` — register a project from an upstream repo URL,
/// optionally under a friendly name. On success land on the new project's
/// summary page; on failure return to the index with a flash error.
pub async fn project_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ProjectCreateForm>,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    match state.projects.create(form.name.as_deref(), &form.url).await {
        Ok(info) => Redirect::to(&format!("/{}/", info.name)).into_response(),
        Err(e) => {
            tracing::warn!(url = %form.url, error = %e, "project create failed");
            Redirect::to(&format!("/?error={}", url_encode(&format!("{e:#}")))).into_response()
        }
    }
}

/// `POST /projects/delete` — remove a project and everything under it.
pub async fn project_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ProjectDeleteForm>,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    if let Ok(Some(info)) = state.projects.find(form.name.trim()).await {
        // Deleting a project also removes its mirror state and per-project
        // secret. Failure here must not turn deletion into an accidental
        // partial-project retention; ProjectManager remains authoritative.
        let _ = state.mirrors.remove(&info, true).await;
    }
    match state.projects.delete(&form.name).await {
        Ok(()) => Redirect::to(&format!(
            "/?ok={}",
            url_encode(&format!("deleted {}", form.name))
        ))
        .into_response(),
        Err(e) => {
            tracing::warn!(project = %form.name, error = %e, "project delete failed");
            Redirect::to(&format!("/?error={}", url_encode(&format!("{e:#}")))).into_response()
        }
    }
}

/// `POST /{name}/merge` — merge a session worktree branch into the project's
/// default branch (so anyone cloning the repo can fetch it).
pub async fn project_merge(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<MergeForm>,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    let branch = form.branch.trim().to_string();
    if branch.is_empty() {
        return Redirect::to(&format!(
            "/{name}/refs?error={}",
            url_encode("no branch given")
        ))
        .into_response();
    }
    let project = match state.projects.find(&name).await {
        Ok(Some(p)) => p,
        _ => {
            return Redirect::to(&format!(
                "/?error={}",
                url_encode(&format!("project '{name}' not found"))
            ))
            .into_response()
        }
    };
    let default = state
        .projects
        .default_branch(&project)
        .await
        .unwrap_or_else(|| "main".to_string());
    // merge_to_default only needs the branch + project; the checkout dir is
    // irrelevant to the merge itself.
    let active = ActiveProject {
        project: project.clone(),
        worktree_path: String::new(),
        branch: branch.clone(),
    };
    match state
        .projects
        .merge_to_default(&active, form.message.as_deref().unwrap_or(""))
        .await
    {
        Ok(_) => {
            let notice = match state.mirrors.auto_push(&project).await {
                Some(Ok(outcome)) => format!(
                    "merged {branch} into {default}; mirror {}",
                    outcome.status
                ),
                Some(Err(_)) => format!(
                    "merged {branch} into {default}; automatic mirror push failed (see project status)"
                ),
                None => format!("merged {branch} into {default}"),
            };
            Redirect::to(&format!("/{name}/refs?ok={}", url_encode(&notice))).into_response()
        }
        Err(e) => {
            tracing::warn!(project = %name, branch = %branch, error = %e, "merge failed");
            Redirect::to(&format!(
                "/{name}/refs?error={}",
                url_encode(&format!("{e:#}"))
            ))
            .into_response()
        }
    }
}

/// `POST /{name}/worktrees/delete` — remove one omega session checkout and
/// its branch. The manager resolves the checkout from Git and verifies it is
/// contained by this project's worktree directory before deleting it.
pub async fn worktree_delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<WorktreeDeleteForm>,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    let branch = form.branch.trim().to_string();
    let project = match state.projects.find(&name).await {
        Ok(Some(project)) => project,
        _ => {
            return Redirect::to(&format!(
                "/?error={}",
                url_encode(&format!("project '{name}' not found"))
            ))
            .into_response()
        }
    };
    match state
        .projects
        .remove_worktree_by_branch(&project, &branch)
        .await
    {
        Ok(()) => Redirect::to(&format!(
            "/{name}/refs?ok={}",
            url_encode(&format!("deleted worktree {branch}"))
        ))
        .into_response(),
        Err(e) => {
            tracing::warn!(project = %name, branch = %branch, error = %e, "worktree delete failed");
            Redirect::to(&format!(
                "/{name}/refs?error={}",
                url_encode(&format!("{e:#}"))
            ))
            .into_response()
        }
    }
}

/// Configure a distinct writable GitHub mirror. The token field is
/// deliberately write-only: blank preserves it and no response reflects it.
pub async fn mirror_configure(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<MirrorConfigureForm>,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    let info = match state.projects.find(&name).await {
        Ok(Some(info)) if info.name == name => info,
        _ => return text_response(StatusCode::NOT_FOUND, "project not found\n"),
    };
    let token = if form.remove_token.is_some() {
        TokenUpdate::Remove
    } else {
        match form
            .token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(token) => TokenUpdate::Set(token),
            None => TokenUpdate::Unchanged,
        }
    };
    let auto_push = form.auto_push.is_some();
    match state
        .mirrors
        .configure(&info, &form.mirror, auto_push, token)
        .await
    {
        Ok(_) => Redirect::to(&format!(
            "/{name}/?ok={}",
            url_encode("GitHub mirror settings saved")
        ))
        .into_response(),
        Err(e) => Redirect::to(&format!(
            "/{name}/?error={}",
            url_encode(&format!("mirror configuration failed: {e}"))
        ))
        .into_response(),
    }
}

/// Manually push the local configured default branch with an exact lease.
pub async fn mirror_push(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    let info = match state.projects.find(&name).await {
        Ok(Some(info)) if info.name == name => info,
        _ => return text_response(StatusCode::NOT_FOUND, "project not found\n"),
    };
    match state.mirrors.push(&info).await {
        Ok(outcome) => Redirect::to(&format!(
            "/{name}/?ok={}",
            url_encode(&format!("mirror {}", outcome.status))
        ))
        .into_response(),
        Err(e) => Redirect::to(&format!(
            "/{name}/?error={}",
            url_encode(&format!("mirror push failed: {e}"))
        ))
        .into_response(),
    }
}

/// Remove the managed `mirror` remote/configuration. A checked box also
/// removes the per-project token; the systemd credential is never mutated.
pub async fn mirror_remove(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<MirrorRemoveForm>,
) -> Response {
    if let Some(response) = reject_cross_origin(&headers) {
        return response;
    }
    let info = match state.projects.find(&name).await {
        Ok(Some(info)) if info.name == name => info,
        _ => return text_response(StatusCode::NOT_FOUND, "project not found\n"),
    };
    match state
        .mirrors
        .remove(&info, form.remove_token.is_some())
        .await
    {
        Ok(()) => Redirect::to(&format!(
            "/{name}/?ok={}",
            url_encode("GitHub mirror configuration removed")
        ))
        .into_response(),
        Err(e) => Redirect::to(&format!(
            "/{name}/?error={}",
            url_encode(&format!("could not remove mirror configuration: {e}"))
        ))
        .into_response(),
    }
}

/// Percent-encode a string for use in a redirect query string (the sr.ht
/// server-rendered UI has no session store, so flash notices ride the URL).
fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use omega_loop_client::{OutputChunk, WireChunk};
    use omega_loop_client::omega_core::core::{CacheTelemetry, ToolResult};
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Build a `ServerEvent::Chunk` for the given chunk (session id is
    /// irrelevant to `sse_payloads`).
    fn chunk(c: OutputChunk) -> ServerEvent {
        ServerEvent::Chunk {
            session_id: "sess".to_string(),
            chunk: WireChunk::Known(c),
        }
    }

    #[test]
    fn mutation_origin_guard_rejects_cross_site_browser_posts() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8080".parse().unwrap());
        assert!(reject_cross_origin(&headers).is_none());

        headers.insert(header::ORIGIN, "http://127.0.0.1:8080".parse().unwrap());
        assert!(reject_cross_origin(&headers).is_none());

        headers.insert(header::ORIGIN, "https://attacker.example".parse().unwrap());
        assert_eq!(
            reject_cross_origin(&headers).unwrap().status(),
            StatusCode::FORBIDDEN
        );
        headers.insert(header::ORIGIN, "null".parse().unwrap());
        assert_eq!(
            reject_cross_origin(&headers).unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }

    fn test_state(session_dir: &std::path::Path) -> AppState {
        let projects = ProjectManager::with_root(session_dir.join("projects"));
        AppState {
            mirrors: omega_projects::mirror::MirrorManager::new(
                projects.clone(),
                session_dir.join("mirror-credentials"),
                None,
            ),
            projects,
            templates: Arc::new(crate::templates::Templates::new().unwrap()),
            session_dir: session_dir.to_path_buf(),
            rebase_defaults: None,
        }
    }

    fn seed_meta(dir: &std::path::Path, path_id: &str, metadata_id: &str) {
        let session = dir.join(path_id);
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(
            session.join("metadata.json"),
            serde_json::to_vec(&json!({
                "session_id": metadata_id,
                "name": "omega",
                "description": "test",
                "model": "model-a",
                "provider": "mock"
            }))
            .unwrap(),
        )
        .unwrap();
    }

    async fn git_ok(dir: &std::path::Path, args: &[&str]) {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn mirror_form_is_same_origin_and_token_is_write_only() {
        let temp = TempDir::new().unwrap();
        let upstream = temp.path().join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        git_ok(&upstream, &["init", "-b", "main"]).await;
        git_ok(&upstream, &["config", "user.name", "Web Mirror Test"]).await;
        git_ok(
            &upstream,
            &["config", "user.email", "web-mirror@example.test"],
        )
        .await;
        git_ok(&upstream, &["config", "commit.gpgsign", "false"]).await;
        std::fs::write(upstream.join("README"), "test\n").unwrap();
        git_ok(&upstream, &["add", "README"]).await;
        git_ok(&upstream, &["commit", "-m", "initial"]).await;

        let state = test_state(temp.path());
        state
            .projects
            .create(Some("demo"), &upstream.display().to_string())
            .await
            .unwrap();
        let token = "github_pat_must-never-render";
        let form = || MirrorConfigureForm {
            mirror: "octocat/demo".into(),
            auto_push: Some("on".into()),
            token: Some(token.into()),
            remove_token: None,
        };

        let mut hostile = HeaderMap::new();
        hostile.insert(header::HOST, "127.0.0.1:8080".parse().unwrap());
        hostile.insert(header::ORIGIN, "https://attacker.example".parse().unwrap());
        let response = mirror_configure(
            State(state.clone()),
            Path("demo".into()),
            hostile,
            Form(form()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!state.mirrors.has_project_token("demo"));

        let mut same_origin = HeaderMap::new();
        same_origin.insert(header::HOST, "127.0.0.1:8080".parse().unwrap());
        same_origin.insert(header::ORIGIN, "http://127.0.0.1:8080".parse().unwrap());
        let missing = mirror_configure(
            State(state.clone()),
            Path("missing".into()),
            same_origin.clone(),
            Form(form()),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert!(!state.mirrors.has_project_token("missing"));

        let response = mirror_configure(
            State(state.clone()),
            Path("demo".into()),
            same_origin.clone(),
            Form(form()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(!response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .contains(token));
        assert!(state.mirrors.has_project_token("demo"));
        let mirror_state = std::fs::read_to_string(
            state
                .projects
                .root()
                .join(omega_projects::mirror::STATE_FILE),
        )
        .unwrap();
        assert!(!mirror_state.contains(token));

        // A blank token preserves the existing write-only value.
        let blank = MirrorConfigureForm {
            mirror: "https://github.com/octocat/demo.git".into(),
            auto_push: None,
            token: Some(String::new()),
            remove_token: None,
        };
        let response = mirror_configure(
            State(state.clone()),
            Path("demo".into()),
            same_origin.clone(),
            Form(blank),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(state.mirrors.has_project_token("demo"));

        let page = summary(
            State(state.clone()),
            Path("demo".into()),
            same_origin.clone(),
            Query(NoticeQuery::default()),
        )
        .await;
        assert_eq!(page.status(), StatusCode::OK);
        let body = page.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains("github.com") && html.contains("octocat"),
            "mirror URL missing from rendered page: {html}"
        );
        assert!(!html.contains(token));

        // State is normally canonical-only, but template escaping remains a
        // final defence if an administrator edits the JSON by hand.
        let state_path = state
            .projects
            .root()
            .join(omega_projects::mirror::STATE_FILE);
        let mut stored: omega_projects::mirror::MirrorState =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        stored.projects.get_mut("demo").unwrap().url =
            "https://github.com/octocat/\"><script>alert(1)</script>".into();
        std::fs::write(&state_path, serde_json::to_vec_pretty(&stored).unwrap()).unwrap();
        let escaped_page = summary(
            State(state.clone()),
            Path("demo".into()),
            same_origin.clone(),
            Query(NoticeQuery {
                error: Some("<script>notice</script>".into()),
                ok: None,
            }),
        )
        .await;
        let escaped_body = escaped_page.into_body().collect().await.unwrap().to_bytes();
        let escaped_html = String::from_utf8(escaped_body.to_vec()).unwrap();
        assert!(!escaped_html.contains("<script>"));
        assert!(escaped_html.contains("&lt;script&gt;"));

        let remove = MirrorConfigureForm {
            mirror: "octocat/demo".into(),
            auto_push: None,
            token: None,
            remove_token: Some("on".into()),
        };
        let response = mirror_configure(
            State(state.clone()),
            Path("demo".into()),
            same_origin,
            Form(remove),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(!state.mirrors.has_project_token("demo"));
    }

    #[tokio::test]
    async fn session_mutation_posts_enforce_origin_and_exact_identity() {
        let temp = TempDir::new().unwrap();
        seed_meta(temp.path(), "safe", "safe");
        let state = test_state(temp.path());
        let mut cross_site = HeaderMap::new();
        cross_site.insert(header::HOST, "127.0.0.1:8080".parse().unwrap());
        cross_site.insert(header::ORIGIN, "https://attacker.example".parse().unwrap());

        let compact = session_compact(
            State(state.clone()),
            Path("safe".to_string()),
            cross_site.clone(),
        )
        .await;
        assert_eq!(compact.status(), StatusCode::FORBIDDEN);
        let message = session_message(
            State(state.clone()),
            Path("safe".to_string()),
            cross_site,
            Form(MessageForm {
                content: "hello".to_string(),
            }),
        )
        .await;
        assert_eq!(message.status(), StatusCode::FORBIDDEN);

        seed_meta(temp.path(), "alias", "some-other-session");
        let wrong_target =
            session_interrupt(State(state), Path("alias".to_string()), HeaderMap::new()).await;
        assert_eq!(wrong_target.status(), StatusCode::NOT_FOUND);
        assert!(!valid_session_id("../safe"));
    }

    #[test]
    fn session_template_has_escaped_model_controls_and_live_identity() {
        let templates = crate::templates::Templates::new().unwrap();
        let html = templates
            .render(
                "session.html",
                json!({
                    "session": {
                        "session_id": "session-1",
                        "display_name": "chat",
                        "model": "model-a",
                        "provider": "mock"
                    },
                    "messages": [],
                    "models": ["model-a", "bad\"><script>alert(1)</script>"],
                    "live": true,
                    "stream_url": "/sessions/session-1/stream"
                }),
            )
            .unwrap();
        assert!(html.contains("data-session-id=\"session-1\""));
        assert!(html.contains("/sessions/session-1/model"));
        assert!(html.contains("/sessions/session-1/compact"));
        assert!(html.contains("window.confirm"));
        assert!(!html.contains("<script>alert(1)</script>"));
    }

    #[test]
    fn model_and_compaction_events_keep_session_identity() {
        let model = sse_payloads(&ServerEvent::ModelChanged {
            session_id: "session-1".to_string(),
            model: "model-a".to_string(),
        });
        assert_eq!(model[0]["type"], "model_changed");
        assert_eq!(model[0]["session_id"], "session-1");
        assert_eq!(model[0]["model"], "model-a");
        let compacted = sse_payloads(&ServerEvent::SessionCompacted {
            session_id: "session-1".to_string(),
        });
        assert_eq!(compacted[0]["type"], "compacted");
        assert_eq!(compacted[0]["session_id"], "session-1");
    }

    #[test]
    fn text_deltas_and_complete_are_distinct_events() {
        let payloads = sse_payloads(&chunk(OutputChunk::TextDelta("hel".into())));
        assert_eq!(payloads[0]["type"], "text");
        assert_eq!(payloads[0]["text"], "hel");
        let payloads = sse_payloads(&chunk(OutputChunk::TextComplete("hello".into())));
        assert_eq!(payloads[0]["type"], "text_complete");
        assert_eq!(payloads[0]["text"], "hello");
    }

    #[test]
    fn thinking_maps_to_thinking_events() {
        let payloads = sse_payloads(&chunk(OutputChunk::ThinkingDelta("hmm".into())));
        assert_eq!(payloads[0]["type"], "thinking");
        let payloads = sse_payloads(&chunk(OutputChunk::ThinkingComplete("hmm".into())));
        assert_eq!(payloads[0]["type"], "thinking_complete");
    }

    #[test]
    fn tools_are_forwarded_with_preview_and_truncated_result() {
        let big = "x".repeat(transcript::MAX_BLOCK_RENDER + 50);
        let input = serde_json::json!({ "command": "ls" });
        let start = sse_payloads(&chunk(OutputChunk::ToolStart {
            id: "t1".into(),
            name: "Bash".into(),
            input: input.clone(),
        }));
        assert_eq!(start[0]["type"], "tool_start");
        assert_eq!(start[0]["id"], "t1");
        assert_eq!(start[0]["name"], "Bash");
        assert_eq!(start[0]["input"]["command"], "ls");

        let progress = sse_payloads(&chunk(OutputChunk::ToolProgress {
            id: "t1".into(),
            output: "building\ncompiling".into(),
        }));
        assert_eq!(progress[0]["type"], "tool_progress");
        assert_eq!(progress[0]["output"], "building\ncompiling");

        let end = sse_payloads(&chunk(OutputChunk::ToolEnd {
            id: "t1".into(),
            name: "Bash".into(),
            input,
            result: ToolResult::success(big.clone()),
        }));
        assert_eq!(end[0]["type"], "tool_end");
        assert_eq!(end[0]["is_error"], false);
        assert!(
            end[0]["result"].as_str().unwrap().contains("(truncated)"),
            "result must be truncated: {}",
            end[0]["result"]
        );
        let preview = end[0]["preview"].as_str().unwrap();
        assert!(
            preview.starts_with("xxx") && preview.ends_with('…'),
            "preview is one truncated line: {preview:?}"
        );
        assert!(
            !preview.contains('\n') && !preview.contains(' '),
            "preview collapses whitespace: {preview:?}"
        );
    }

    #[test]
    fn tool_result_error_is_marked_and_previewed() {
        let end = sse_payloads(&chunk(OutputChunk::ToolEnd {
            id: "t1".into(),
            name: "Bash".into(),
            input: serde_json::json!({}),
            result: ToolResult::error(""),
        }));
        assert_eq!(end[0]["is_error"], true);
        assert_eq!(end[0]["preview"], "error");
    }

    #[test]
    fn done_and_status_pass_through() {
        let done = sse_payloads(&chunk(OutputChunk::Done));
        assert_eq!(done[0]["type"], "done");
        let st = sse_payloads(&chunk(OutputChunk::Status("retrying".into())));
        assert_eq!(st[0]["type"], "status");
        assert_eq!(st[0]["message"], "retrying");
        let sys = sse_payloads(&ServerEvent::SystemMsg {
            message: "hello".into(),
        });
        assert_eq!(sys[0]["type"], "status");
    }

    #[test]
    fn non_chat_chunks_are_dropped() {
        assert!(sse_payloads(&chunk(OutputChunk::CacheTelemetry(CacheTelemetry {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: 1,
            cache_creation_tokens: 1,
        })))
        .is_empty());
        assert!(sse_payloads(&ServerEvent::Chunk {
            session_id: "sess".into(),
            chunk: WireChunk::Unknown(serde_json::json!({"FutureChunk": {}})),
        })
        .is_empty());
        // Session events without chat content produce nothing either.
        assert!(sse_payloads(&ServerEvent::ModelList { models: vec![] }).is_empty());
    }
}
