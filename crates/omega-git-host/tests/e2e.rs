//! End-to-end tests for omega-git-host: real project store, real server
//! binary, real `git clone` over smart HTTP.
//!
//! Everything lives in throwaway temp dirs — no fixtures are committed.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use omega_projects::ProjectManager;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

/// Run `git <args>` in `dir`, returning trimmed stdout on success.
async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Build a throwaway source repo (one commit on `main`).
async fn init_source_repo(dir: &Path) {
    git(dir, &["init", "-b", "main"]).await.unwrap();
    git(dir, &["config", "user.email", "omega-test@example.com"])
        .await
        .unwrap();
    git(dir, &["config", "user.name", "Omega Test"]).await.unwrap();
    // Don't inherit the host's commit.gpgsign — signing would prompt/
    // hang on a throwaway repo that has no signing key.
    git(dir, &["config", "commit.gpgsign", "false"]).await.unwrap();
    std::fs::write(dir.join("README.md"), "# Dummy project\n").unwrap();
    git(dir, &["add", "."]).await.unwrap();
    git(dir, &["commit", "-m", "initial commit"]).await.unwrap();
}

/// Seed a project store: register a project (bare clone + default branch) and
/// create one session worktree. Returns (project name, worktree branch).
async fn seed_store(store: &Path) -> (String, String) {
    let source_root = TempDir::new().unwrap();
    let source = source_root.path().join("my-project");
    std::fs::create_dir_all(&source).unwrap();
    init_source_repo(&source).await;

    let manager = ProjectManager::with_root(store);
    let active = manager
        .activate(source.to_str().unwrap(), "sess-123", None)
        .await
        .unwrap();
    (active.project.name, active.branch)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Spawn the real `omega-git-host` binary against `projects_dir` (and
/// `session_dir` for the session-backed refs page).
/// `kill_on_drop` ensures the server dies even if a test panics.
async fn spawn_server(projects_dir: &Path, session_dir: &Path, port: u16) -> Child {
    Command::new(env!("CARGO_BIN_EXE_omega-git-host"))
        .env("OMEGA_PROJECTS_DIR", projects_dir)
        .env("OMEGA_SESSION_DIR", session_dir)
        .env("OMEGA_GIT_HOST_LISTEN", "127.0.0.1")
        .env("OMEGA_GIT_HOST_PORT", port.to_string())
        .env("RUST_LOG", "warn")
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn omega-git-host")
}

/// Write a fake session's metadata.json so the refs page can link a worktree
/// branch to a chat conversation (mirrors omega-loop's custom metadata).
async fn seed_session(
    session_dir: &Path,
    session_id: &str,
    branch: &str,
    conversation_name: &str,
) {
    let dir = session_dir.join(session_id);
    std::fs::create_dir_all(&dir).unwrap();
    let meta = serde_json::json!({
        "session_id": session_id,
        "agent_type": "coder",
        "name": "Coder",
        "description": "Test agent",
        "conversation_name": conversation_name,
        "model": "gpt-test",
        "provider": "openai",
        "created_at": "2026-08-15T00:00:00Z",
        "updated_at": "2026-08-15T01:00:00Z",
        "custom": {
            "active_project": {
                "project": {
                    "name": "my-project",
                    "url": "file:///tmp/origin",
                    "default_branch": "main",
                    "created_at": "2026-08-15T00:00:00Z",
                },
                "worktree_path": "/tmp/wt",
                "branch": branch,
            }
        }
    });
    std::fs::write(
        dir.join("metadata.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();

    // A realistic transcript: plain text plus tool call/result blocks.
    let history = r#"{"role":"user","content":"please add a feature"}
{"role":"assistant","content":[{"type":"text","text":"let me check"},{"type":"tool_use","id":"call_1","name":"Bash","input":{"command":"ls"}}]}
{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"file1\nfile2"}]}
"#;
    std::fs::write(dir.join("history.jsonl"), history).unwrap();
    std::fs::write(dir.join("system_prompt.md"), "You are Omega, a test agent.").unwrap();
}

/// Poll until the server accepts TCP connections on `port`.
async fn wait_for_server(port: u16) {
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("omega-git-host never became reachable on port {port}");
}

/// HTML-escape the way minijinja does, for asserting rendered URLs.
fn escaped_url(url: &str) -> String {
    url.replace('/', "&#x2f;")
}

/// Minimal HTTP/1.1 GET returning the raw response. The Host header carries
/// the port so handlers that build URLs from it see the real base.
async fn http_get(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).to_string()
}

#[tokio::test]
async fn clone_serves_default_branch_and_worktree_branches() {
    let store = TempDir::new().unwrap();
    let (project, worktree_branch) = seed_store(store.path()).await;
    let port = free_port();
    let server = spawn_server(store.path(), store.path(), port).await;
    wait_for_server(port).await;

    let dest = TempDir::new().unwrap();
    let url = format!("http://127.0.0.1:{port}/{project}.git");
    let out = Command::new("git")
        .arg("clone")
        .arg(&url)
        .arg(dest.path())
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Default branch content is present.
    assert!(dest.path().join("README.md").exists());

    // The session worktree is visible as a remote-tracking branch...
    let branches = git(dest.path(), &["branch", "-r"]).await.unwrap();
    assert!(
        branches.contains("origin/main"),
        "expected origin/main in:\n{branches}"
    );
    let remote_worktree = format!("origin/{worktree_branch}");
    assert!(
        branches.contains(&remote_worktree),
        "expected {remote_worktree} in:\n{branches}"
    );

    // ...and referenceable like any other branch.
    let sha = git(
        dest.path(),
        &["rev-parse", &remote_worktree],
    )
    .await
    .unwrap();
    assert!(sha.len() >= 40, "resolved sha: {sha}");

    drop(server);
}

#[tokio::test]
async fn ls_remote_lists_worktree_refs() {
    let store = TempDir::new().unwrap();
    let (project, worktree_branch) = seed_store(store.path()).await;
    let port = free_port();
    let server = spawn_server(store.path(), store.path(), port).await;
    wait_for_server(port).await;

    let url = format!("http://127.0.0.1:{port}/{project}.git");
    let out = Command::new("git")
        .arg("ls-remote")
        .arg(&url)
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(stdout.contains("refs/heads/main"));
    assert!(
        stdout.contains(&format!("refs/heads/{worktree_branch}")),
        "expected worktree ref in:\n{stdout}"
    );

    drop(server);
}

#[tokio::test]
async fn push_is_refused() {
    let store = TempDir::new().unwrap();
    let (project, _) = seed_store(store.path()).await;
    let port = free_port();
    let server = spawn_server(store.path(), store.path(), port).await;
    wait_for_server(port).await;

    let dest = TempDir::new().unwrap();
    let url = format!("http://127.0.0.1:{port}/{project}.git");
    let out = Command::new("git")
        .arg("clone")
        .arg(&url)
        .arg(dest.path())
        .output()
        .await
        .unwrap();
    assert!(out.status.success());

    let out = Command::new("git")
        .arg("-C")
        .arg(dest.path())
        .arg("push")
        .arg("origin")
        .arg("main")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .unwrap();
    assert!(
        !out.status.success(),
        "push must be refused (read-only forge); stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    drop(server);
}

#[tokio::test]
async fn unknown_repo_is_404_and_index_lists_projects() {
    let store = TempDir::new().unwrap();
    let (project, _) = seed_store(store.path()).await;
    let port = free_port();
    let server = spawn_server(store.path(), store.path(), port).await;
    wait_for_server(port).await;

    // The index is a sourcehut-style HTML page listing the project and its
    // clone URL, linking the compiled stylesheet. (minijinja HTML-escapes `/`
    // as `&#x2f;` in displayed URLs — same as Jinja on sr.ht.)
    let body = http_get(port, "/").await;
    assert!(body.contains("200 OK"), "index response:\n{body}");
    assert!(body.contains("<!doctype html>"), "index response:\n{body}");
    assert!(body.contains("omega-git-host"), "index response:\n{body}");
    assert!(
        body.contains(&format!(
            "http:&#x2f;&#x2f;127.0.0.1:{port}&#x2f;{project}.git"
        )),
        "index must show the clone URL:\n{body}"
    );
    assert!(
        body.contains("/static/main.css"),
        "index must link the compiled stylesheet:\n{body}"
    );

    // The compiled sr.ht stylesheet is served (Bootstrap + dark mode rules).
    let css = http_get(port, "/static/main.css").await;
    assert!(css.contains("200 OK"), "css response:\n{css}");
    assert!(css.contains("navbar"), "css response:\n{css}");
    assert!(
        css.contains("prefers-color-scheme"),
        "css must include dark mode:\n{css}"
    );

    // Cloning an unregistered repo fails with 404.
    let dest = TempDir::new().unwrap();
    let out = Command::new("git")
        .arg("clone")
        .arg(format!("http://127.0.0.1:{port}/nope.git"))
        .arg(dest.path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .unwrap();
    assert!(
        !out.status.success(),
        "clone of unknown repo should fail: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    drop(server);
}

#[tokio::test]
async fn repo_pages_render_and_refs_link_worktrees_to_sessions() {
    let store = TempDir::new().unwrap();
    let sessions = TempDir::new().unwrap();
    let (project, worktree_branch) = seed_store(store.path()).await;
    seed_session(
        sessions.path(),
        "sess-123",
        &worktree_branch,
        "Fix the thing",
    )
    .await;
    let port = free_port();
    let server = spawn_server(store.path(), sessions.path(), port).await;
    wait_for_server(port).await;

    let base = format!("http://127.0.0.1:{port}");

    // Summary: repo chrome tabs + the commit feed.
    let body = http_get(port, &format!("/{project}/")).await;
    assert!(body.contains("200 OK"), "summary:\n{body}");
    assert!(body.contains(">summary<"), "summary:\n{body}");
    assert!(body.contains(">refs<"), "summary:\n{body}");
    assert!(body.contains("initial commit"), "summary:\n{body}");
    assert!(
        body.contains(&escaped_url(&format!("{base}/{project}.git"))),
        "summary must show the clone URL:\n{body}"
    );

    // Refs: worktree branch grouped with its session + view-chat link.
    let body = http_get(port, &format!("/{project}/refs")).await;
    assert!(body.contains("200 OK"), "refs:\n{body}");
    assert!(body.contains("session worktrees"), "refs:\n{body}");
    assert!(body.contains("Fix the thing"), "refs:\n{body}");
    assert!(
        body.contains(&escaped_url(&worktree_branch)),
        "refs:\n{body}"
    );
    assert!(
        body.contains("/sessions/sess-123"),
        "refs must link to the session transcript:\n{body}"
    );

    // Tree at the worktree branch (slashed ref) and at main.
    let body = http_get(port, &format!("/{project}/tree/{worktree_branch}")).await;
    assert!(body.contains("200 OK"), "tree (worktree):\n{body}");
    assert!(body.contains("README.md"), "tree (worktree):\n{body}");
    let body = http_get(port, &format!("/{project}/tree/main")).await;
    assert!(body.contains("README.md"), "tree (main):\n{body}");

    // Blob content on the worktree branch.
    let body = http_get(
        port,
        &format!("/{project}/blob/{worktree_branch}/README.md"),
    )
    .await;
    assert!(body.contains("200 OK"), "blob:\n{body}");
    assert!(body.contains("Dummy project"), "blob:\n{body}");

    // Commit page (rev can be a bare sha or branch name).
    let body = http_get(port, &format!("/{project}/commit/main")).await;
    assert!(body.contains("200 OK"), "commit:\n{body}");
    assert!(body.contains("initial commit"), "commit:\n{body}");

    // Log page.
    let body = http_get(port, &format!("/{project}/log/main")).await;
    assert!(body.contains("200 OK"), "log:\n{body}");
    assert!(body.contains("initial commit"), "log:\n{body}");

    // Clone page.
    let body = http_get(port, &format!("/{project}/clone")).await;
    assert!(body.contains("200 OK"), "clone:\n{body}");
    assert!(
        body.contains(&escaped_url(&format!("{base}/{project}.git"))),
        "clone:\n{body}"
    );

    // Unknown project → 404.
    let body = http_get(port, "/nope/").await;
    assert!(body.contains("404 Not Found"), "unknown project:\n{body}");

    drop(server);
}

#[tokio::test]
async fn session_pages_render_transcripts_and_system_prompts() {
    let store = TempDir::new().unwrap();
    let sessions = TempDir::new().unwrap();
    let (project, worktree_branch) = seed_store(store.path()).await;
    seed_session(
        sessions.path(),
        "sess-123",
        &worktree_branch,
        "Fix the thing",
    )
    .await;
    let port = free_port();
    let server = spawn_server(store.path(), sessions.path(), port).await;
    wait_for_server(port).await;

    // Sessions index lists the top-level session with its worktree.
    let body = http_get(port, "/sessions/").await;
    assert!(body.contains("200 OK"), "sessions index:\n{body}");
    assert!(body.contains("Fix the thing"), "sessions index:\n{body}");
    assert!(
        body.contains(&format!("{project} / {}", escaped_url(&worktree_branch))),
        "sessions index must link the worktree:\n{body}"
    );

    // Transcript: header, user/assistant text, tool call + result, prompt link.
    let body = http_get(port, "/sessions/sess-123").await;
    assert!(body.contains("200 OK"), "session page:\n{body}");
    assert!(body.contains("please add a feature"), "session page:\n{body}");
    assert!(body.contains("let me check"), "session page:\n{body}");
    assert!(body.contains("tool: <code>Bash</code>"), "session page:\n{body}");
    assert!(body.contains("tool result"), "session page:\n{body}");
    assert!(body.contains("system-prompt"), "session page:\n{body}");
    assert!(
        body.contains(&format!("/{project}/tree/{}", escaped_url(&worktree_branch))),
        "session page must link back to the worktree:\n{body}"
    );

    // System prompt page.
    let body = http_get(port, "/sessions/sess-123/system-prompt").await;
    assert!(body.contains("200 OK"), "system prompt:\n{body}");
    assert!(
        body.contains("You are Omega, a test agent."),
        "system prompt:\n{body}"
    );

    // Unknown session → 404.
    let body = http_get(port, "/sessions/nope").await;
    assert!(body.contains("404 Not Found"), "unknown session:\n{body}");

    drop(server);
}
