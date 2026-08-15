//! # omega-git-host
//!
//! Read-only self-hosted git forge over the omega project store (SELFGIT.md).
//!
//! Phase 1: serve every registered project's bare clone as a fetchable smart
//! HTTP remote via `git http-backend`. Phase 2: sourcehut-style web UI shell
//! (layout + nav + project index) rendered server-side with minijinja, the
//! sr.ht stylesheet compiled at build time from vendored SCSS.

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use omega_projects::ProjectManager;
use tracing::info;

mod git;
mod pages;
mod repo;
mod sessions;
mod style;
mod templates;
mod transcript;

/// Directory of the project store (bare clones + worktrees). Same default
/// rule as `omega-projects`: `$OMEGA_PROJECTS_DIR` or `./projects`.
const ENV_PROJECTS_DIR: &str = "OMEGA_PROJECTS_DIR";
/// Directory where omega-loop stores sessions (metadata.json, history.jsonl).
/// Used by the refs page to link worktree branches to their chat sessions.
const ENV_SESSION_DIR: &str = "OMEGA_SESSION_DIR";
/// Bind address. Defaults to loopback on purpose: the forge is read-only
/// today, but it will serve full session transcripts (possibly sensitive)
/// from Phase 4 on — do not expose casually.
const ENV_LISTEN: &str = "OMEGA_GIT_HOST_LISTEN";
/// TCP port (default 8080).
const ENV_PORT: &str = "OMEGA_GIT_HOST_PORT";
const DEFAULT_LISTEN: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8080;

#[derive(Clone)]
pub struct AppState {
    pub projects: ProjectManager,
    pub templates: Arc<templates::Templates>,
    pub session_dir: std::path::PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "omega_git_host=info".into()),
        )
        .init();

    let projects_dir = env::var(ENV_PROJECTS_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(|_| ProjectManager::default_root());
    let listen = env::var(ENV_LISTEN).unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
    let port: u16 = match env::var(ENV_PORT) {
        Ok(p) => p.parse().with_context(|| format!("{ENV_PORT} must be a TCP port"))?,
        Err(_) => DEFAULT_PORT,
    };

    let projects = ProjectManager::with_root(&projects_dir);
    if !projects_dir.is_dir() {
        tracing::warn!(
            projects_dir = %projects_dir.display(),
            "project store root does not exist yet; only the index will respond"
        );
    }
    let session_dir = env::var(ENV_SESSION_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("sessions"));

    let state = AppState {
        projects,
        templates: Arc::new(templates::Templates::new()?),
        session_dir,
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/static/main.css", get(css))
        // Repo pages (param routes; more specific than the git wildcard).
        .route("/{name}", get(pages::summary))
        .route("/{name}/", get(pages::summary))
        .route("/{name}/refs", get(pages::refs_page))
        .route("/{name}/refs/", get(pages::refs_page))
        .route("/{name}/clone", get(pages::clone_page))
        .route("/{name}/clone/", get(pages::clone_page))
        .route("/{name}/log/{*path}", get(pages::log_page))
        .route("/{name}/tree/{*path}", get(pages::tree_page))
        .route("/{name}/blob/{*path}", get(pages::blob_page))
        .route("/{name}/commit/{*sha}", get(pages::commit_page))
        // Session transcript pages (Phase 4).
        .route("/sessions", get(pages::sessions_index))
        .route("/sessions/", get(pages::sessions_index))
        .route("/sessions/{id}", get(pages::session_page))
        .route("/sessions/{id}/system-prompt", get(pages::session_system_prompt))
        // Smart-HTTP git endpoints live at /{name}.git/info/refs,
        // /{name}.git/git-upload-pack, etc. — handled by the fallback (the
        // root-level wildcard would conflict with the repo page routes).
        .fallback(git::git_route)
        .with_state(state);

    let addr: SocketAddr = format!("{listen}:{port}")
        .parse()
        .with_context(|| format!("invalid listen address '{listen}:{port}'"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    let local = listener.local_addr()?;
    info!(%local, projects_dir = %projects_dir.display(), "omega-git-host ready");
    // Machine-readable line for tests / scripts that need the bound address.
    println!("listening on http://{local}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Project index (sourcehut-style dashboard): every registered project with
/// its clone URL.
async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost")
        .to_string();
    let base = format!("http://{host}");

    let projects = match state.projects.list().await {
        Ok(projects) => projects,
        Err(e) => {
            tracing::error!(error = %e, "failed to list projects");
            return text_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to list projects\n");
        }
    };

    let projects: Vec<serde_json::Value> = projects
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name,
                "url": p.url,
                "default_branch": p.default_branch,
                "clone_url": format!("{base}/{}.git", p.name),
            })
        })
        .collect();
    let ctx = serde_json::json!({
        "projects": projects,
        "nav_projects_active": true,
        "nav_sessions_active": false,
    });

    match state.templates.render("index.html", ctx) {
        Ok(html) => html_response(html),
        Err(e) => {
            tracing::error!(error = %e, "failed to render index");
            text_response(StatusCode::INTERNAL_SERVER_ERROR, "template error\n")
        }
    }
}

/// The compiled sr.ht stylesheet.
async fn css() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/css; charset=utf-8")
        .header("cache-control", "public, max-age=3600")
        .body(Body::from(style::MAIN_CSS))
        .unwrap()
}

pub(crate) fn html_response(body: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

pub(crate) fn text_response(status: StatusCode, body: impl Into<String>) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body.into()))
        .unwrap()
}