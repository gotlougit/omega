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

/// Persist a session's active-project binding the way omega-loop does after a
/// rename: JSON metadata under `session_dir/<id>/metadata.json`.
fn persist_active_binding(session_dir: &Path, session_id: &str, active: &omega_projects::ActiveProject) {
    let dir = session_dir.join(session_id);
    std::fs::create_dir_all(&dir).unwrap();
    let meta = serde_json::json!({
        "session_id": session_id,
        "agent_type": "coder",
        "name": "Coder",
        "description": "Test agent",
        "conversation_name": "Fix the thing",
        "model": "gpt-test",
        "provider": "openai",
        "created_at": "2026-08-15T00:00:00Z",
        "updated_at": "2026-08-16T01:00:00Z",
        "custom": {
            "active_project": {
                "project": {
                    "name": active.project.name,
                    "url": active.project.url,
                    "default_branch": "main",
                    "created_at": "2026-08-15T00:00:00Z",
                },
                "worktree_path": active.worktree_path,
                "branch": active.branch,
            }
        }
    });
    std::fs::write(dir.join("metadata.json"), serde_json::to_string_pretty(&meta).unwrap()).unwrap();
    std::fs::write(dir.join("history.jsonl"), "{\"role\":\"user\",\"content\":\"hi\"}\n").unwrap();
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

    // A realistic transcript: markdown text plus thinking + tool call/result
    // blocks. The tool result arrives as a separate user-role message, exactly
    // like omega-loop writes it.
    let history = r#"{"role":"user","content":"please add a feature"}
{"role":"assistant","content":[{"type":"thinking","thinking":"hmm, let me look around","signature":"sig"},{"type":"text","text":"**let me check**"},{"type":"tool_use","id":"call_1","name":"Bash","input":{"command":"ls"}}]}
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
/// the port so handlers that build URLs from it see the real base. Retries
/// briefly to absorb transient connection resets (the test server may still
/// be draining the listener accept queue right after startup).
async fn http_get(port: u16, path: &str) -> String {
    for attempt in 0..5 {
        let mut stream = match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(s) => s,
            Err(e) if attempt < 4 => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
            Err(e) => panic!("connect failed: {e}"),
        };
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        );
        if stream.write_all(req.as_bytes()).await.is_err() && attempt < 4 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        let mut buf = Vec::new();
        match stream.read_to_end(&mut buf).await {
            Ok(_) => return String::from_utf8_lossy(&buf).to_string(),
            Err(_) if attempt < 4 => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
            Err(e) => panic!("read failed: {e}"),
        }
    }
    unreachable!()
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

    // Summary: repo chrome tabs + the commit feed + session worktrees
    // (each linking to its chat transcript).
    let body = http_get(port, &format!("/{project}/")).await;
    assert!(body.contains("200 OK"), "summary:\n{body}");
    assert!(body.contains(">summary<"), "summary:\n{body}");
    assert!(body.contains(">refs<"), "summary:\n{body}");
    assert!(body.contains(">sessions<"), "summary:\n{body}");
    assert!(body.contains("initial commit"), "summary:\n{body}");
    assert!(
        body.contains(&escaped_url(&format!("{base}/{project}.git"))),
        "summary must show the clone URL:\n{body}"
    );
    // Sessions are directly reachable from the project's summary page.
    assert!(body.contains("session worktrees"), "summary:\n{body}");
    assert!(body.contains("Fix the thing"), "summary:\n{body}");
    assert!(
        body.contains("/sessions/sess-123"),
        "summary must link session worktrees to their chats:\n{body}"
    );

    // The project's sessions tab lists the same sessions.
    let body = http_get(port, &format!("/{project}/sessions")).await;
    assert!(body.contains("200 OK"), "repo sessions:\n{body}");
    assert!(body.contains("Fix the thing"), "repo sessions:\n{body}");
    assert!(body.contains("/sessions/sess-123"), "repo sessions:\n{body}");
    assert!(
        body.contains(&escaped_url(&worktree_branch)),
        "repo sessions:\n{body}"
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

/// After a worktree rename, the web UI (summary + refs) must reflect the NEW
/// branch and keep the chat session linked — never the orphaned old one.
#[tokio::test]
async fn worktree_rename_is_reflected_in_summary_and_refs() {
    let store = TempDir::new().unwrap();
    let sessions = TempDir::new().unwrap();

    // Build the store with ProjectManager, exactly like omega-loop does.
    let source_root = TempDir::new().unwrap();
    let source = source_root.path().join("my-project");
    std::fs::create_dir_all(&source).unwrap();
    init_source_repo(&source).await;
    let manager = omega_projects::ProjectManager::with_root(store.path());
    let active = manager
        .activate(source.to_str().unwrap(), "sess-123", None)
        .await
        .unwrap();
    let old_branch = active.branch.clone();
    persist_active_binding(sessions.path(), "sess-123", &active);

    let port = free_port();
    let server = spawn_server(store.path(), sessions.path(), port).await;
    wait_for_server(port).await;

    // Pre-rename: old branch shown + linked to chat.
    let body = http_get(port, &format!("/{}/refs", active.project.name)).await;
    assert!(body.contains(&escaped_url(&old_branch)), "pre-rename refs:\n{body}");
    assert!(body.contains("/sessions/sess-123"), "pre-rename refs:\n{body}");

    // Simulate the omega-loop rename tool: rename_worktree + persist metadata.
    let renamed = manager
        .rename_worktree(&active, "fix-the-thing")
        .await
        .expect("rename");
    persist_active_binding(sessions.path(), "sess-123", &renamed);

    // Post-rename: refs must show the NEW branch, still linked to the chat,
    // and must NOT reference the old (now gone) branch.
    let body = http_get(port, &format!("/{}/refs", active.project.name)).await;
    assert!(body.contains(&escaped_url(&renamed.branch)), "post-rename refs:\n{body}");
    assert!(body.contains("/sessions/sess-123"), "post-rename refs:\n{body}");
    assert!(!body.contains(&escaped_url(&old_branch)), "old branch leaked:\n{body}");

    // Summary page likewise.
    let body = http_get(port, &format!("/{}/", active.project.name)).await;
    assert!(body.contains(&escaped_url(&renamed.branch)), "post-rename summary:\n{body}");
    assert!(body.contains("/sessions/sess-123"), "post-rename summary:\n{body}");
    assert!(!body.contains(&escaped_url(&old_branch)), "old branch in summary:\n{body}");

    // The session transcript still opens.
    let body = http_get(port, "/sessions/sess-123").await;
    assert!(body.contains("200 OK"), "session:\n{body}");

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

    // Transcript: user text, markdown-rendered assistant body, thinking
    // folded in (collapsed), tool call folded in with its result — not a
    // raw user/assistant exchange.
    let body = http_get(port, "/sessions/sess-123").await;
    assert!(body.contains("200 OK"), "session page:\n{body}");
    assert!(body.contains("please add a feature"), "session page:\n{body}");
    // Assistant body is markdown-rendered.
    assert!(
        body.contains("<strong>let me check</strong>"),
        "markdown body:\n{body}"
    );
    // Thinking is part of the assistant message, hidden by default.
    assert!(body.contains("class=\"thinking\""), "session page:\n{body}");
    assert!(body.contains("hmm, let me look around"), "session page:\n{body}");
    // Tool call + result are one collapsed unit inside the assistant message.
    assert!(body.contains("class=\"tools\""), "session page:\n{body}");
    assert!(body.contains("<code>Bash</code>"), "session page:\n{body}");
    assert!(
        body.contains("file1 file2"),
        "tool preview in summary line:\n{body}"
    );
    assert!(body.contains("system-prompt"), "session page:\n{body}");
    assert!(
        body.contains(&format!("/{project}/tree/{}", escaped_url(&worktree_branch))),
        "session page must link back to the worktree:\n{body}"
    );

    // The tool result must NOT appear as a separate user message.
    assert!(
        !body.contains("tool result"),
        "tool results must not render as standalone messages:\n{body}"
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

#[tokio::test]
async fn empty_sessions_are_not_listed_or_linked() {
    let store = TempDir::new().unwrap();
    let sessions = TempDir::new().unwrap();
    let (project, worktree_branch) = seed_store(store.path()).await;
    // One real session (with a transcript)...
    seed_session(
        sessions.path(),
        "sess-real",
        &worktree_branch,
        "Real session",
    )
    .await;
    // ...and one that only ever wrote metadata (aborted creation). It must
    // be invisible everywhere in the web UI.
    let empty_dir = sessions.path().join("sess-empty");
    std::fs::create_dir_all(&empty_dir).unwrap();
    let meta = serde_json::json!({
        "session_id": "sess-empty",
        "name": "Coder",
        "conversation_name": "Empty session",
        "custom": {
            "active_project": {
                "project": {
                    "name": project,
                    "url": "file:///tmp/origin",
                    "default_branch": "main",
                    "created_at": "2026-08-15T00:00:00Z",
                },
                "worktree_path": "/tmp/wt",
                "branch": "omega/sess-empty-000000",
            }
        }
    });
    std::fs::write(
        empty_dir.join("metadata.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();

    let port = free_port();
    let server = spawn_server(store.path(), sessions.path(), port).await;
    wait_for_server(port).await;

    // Not in the global sessions index.
    let body = http_get(port, "/sessions/").await;
    assert!(body.contains("Real session"), "sessions index:\n{body}");
    assert!(
        !body.contains("Empty session"),
        "empty session must not be listed:\n{body}"
    );

    // Not on the project's sessions tab or refs page.
    let body = http_get(port, &format!("/{project}/sessions")).await;
    assert!(
        !body.contains("Empty session"),
        "empty session must not appear on repo sessions:\n{body}"
    );
    let body = http_get(port, &format!("/{project}/refs")).await;
    assert!(
        !body.contains("sess-empty"),
        "empty session must not be linked from refs:\n{body}"
    );

    drop(server);
}

/// Minimal HTTP/1.1 POST with an urlencoded form body, returning the raw
/// response (a 303 redirect for the rebase controls).
async fn http_post_form(port: u16, path: &str, form: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{form}",
        form.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).to_string()
}

/// The rebase page renders, and the imperative controls persist to the
/// store's state file (which is what omega-loop acts on).
#[tokio::test]
async fn rebase_page_renders_and_controls_persist_state() {
    let store = TempDir::new().unwrap();
    let sessions = TempDir::new().unwrap();
    let (project, _worktree_branch) = seed_store(store.path()).await;

    let port = free_port();
    let server = spawn_server(store.path(), sessions.path(), port).await;
    wait_for_server(port).await;

    // Initial page: no cron-jobbable projects, project listed as addable.
    let body = http_get(port, "/rebase").await;
    assert!(
        body.contains("Upstream rebase cron"),
        "rebase page title:\n{body}"
    );
    assert!(body.contains("No cron-jobbable projects yet"));
    assert!(body.contains("Enable</button>"), "addable row:\n{body}");

    // Enable the project imperatively → state file gets `project: true`.
    let resp = http_post_form(
        port,
        "/rebase/toggle",
        &format!("project={project}&enabled=true"),
    )
    .await;
    assert!(resp.contains("303 See Other"), "toggle response:\n{resp}");
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(store.path().join("rebase-job.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        state["project_states"][project.as_str()].as_bool(),
        Some(true),
        "state file after enable: {state}"
    );

    // The page now lists it as enabled (Disable button).
    let body = http_get(port, "/rebase").await;
    assert!(body.contains("Disable</button>"), "enabled row:\n{body}");

    // Set the interval; the state file records the override.
    let resp = http_post_form(port, "/rebase/interval", "interval_seconds=90").await;
    assert!(resp.contains("303 See Other"));
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(store.path().join("rebase-job.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["interval_seconds"].as_u64(), Some(90));

    // "Run now" drops the marker file the daemon polls.
    let resp = http_post_form(port, "/rebase/run", "").await;
    assert!(resp.contains("303 See Other"), "run now response:\n{resp}");
    assert!(
        store.path().join("rebase-now").is_file(),
        "run-now marker should exist"
    );

    // Disabling again persists `project: false` (which wins over any NixOS
    // defaults list).
    let resp = http_post_form(
        port,
        "/rebase/toggle",
        &format!("project={project}&enabled=false"),
    )
    .await;
    assert!(resp.contains("303 See Other"));
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(store.path().join("rebase-job.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        state["project_states"][project.as_str()].as_bool(),
        Some(false),
        "state file after disable: {state}"
    );

    drop(server);
}

/// The web control panel can create a project from an upstream URL, merge a
/// session worktree's branch into main (making it fetchable), and delete the
/// project — end to end over HTTP against real git smart-HTTP.
#[tokio::test]
async fn create_project_merge_worktree_into_main_and_delete() {
    let store = TempDir::new().unwrap();
    let sessions = TempDir::new().unwrap();

    // An upstream repo the server clones when the project is created.
    let source_root = TempDir::new().unwrap();
    let source = source_root.path().join("upstream");
    std::fs::create_dir_all(&source).unwrap();
    init_source_repo(&source).await;
    let url = source.to_str().unwrap().to_string();
    let encoded_url = url.replace('/', "%2F");

    let port = free_port();
    let server = spawn_server(store.path(), sessions.path(), port).await;
    wait_for_server(port).await;

    // The empty index carries the "New project" form.
    let body = http_get(port, "/").await;
    assert!(body.contains("200 OK"), "index:\n{body}");
    assert!(body.contains("Create project"), "new project form:\n{body}");

    // Create a project from the upstream URL under a friendly name.
    let resp = http_post_form(
        port,
        "/projects/create",
        &format!("name=my-fork&url={encoded_url}"),
    )
    .await;
    assert!(resp.contains("303 See Other"), "create response:\n{resp}");

    // The new project's summary renders with the danger zone.
    let body = http_get(port, "/my-fork/").await;
    assert!(body.contains("200 OK"), "new project summary:\n{body}");
    assert!(body.contains("my-fork"), "summary shows name:\n{body}");
    assert!(body.contains("Delete project"), "danger zone:\n{body}");

    // A fresh clone of the fork works immediately (it was cloned on create).
    let dest = TempDir::new().unwrap();
    let out = Command::new("git")
        .arg("clone")
        .arg(format!("http://127.0.0.1:{port}/my-fork.git"))
        .arg(dest.path())
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dest.path().join("README.md").exists());

    // Seed a session worktree + commit in it, exactly as omega-loop would.
    let manager = omega_projects::ProjectManager::with_root(store.path());
    let active = manager.activate("my-fork", "sess-1", None).await.unwrap();
    let repo = manager.repo_dir("my-fork");
    git(&repo, &["config", "user.email", "omega-test@example.com"])
        .await
        .unwrap();
    git(&repo, &["config", "user.name", "Omega Test"])
        .await
        .unwrap();
    git(&repo, &["config", "commit.gpgsign", "false"])
        .await
        .unwrap();
    let wt = Path::new(&active.worktree_path);
    std::fs::write(wt.join("feature.txt"), "done\n").unwrap();
    git(wt, &["add", "."]).await.unwrap();
    git(wt, &["commit", "-m", "add feature"]).await.unwrap();

    // The feature is NOT yet on main.
    let dest_before = TempDir::new().unwrap();
    let out = Command::new("git")
        .arg("clone")
        .arg(format!("http://127.0.0.1:{port}/my-fork.git"))
        .arg(dest_before.path())
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    assert!(!dest_before.path().join("feature.txt").exists());

    // The refs page shows a "Merge into main" action for the worktree.
    let body = http_get(port, "/my-fork/refs").await;
    assert!(body.contains("Merge into main"), "refs merge action:\n{body}");

    // Merge the session branch into main via the web action.
    let encoded_branch = active.branch.replace('/', "%2F");
    let resp = http_post_form(
        port,
        "/my-fork/merge",
        &format!("branch={encoded_branch}&message=Merge+the+feature"),
    )
    .await;
    assert!(resp.contains("303 See Other"), "merge response:\n{resp}");

    // A fresh clone now sees the feature on main — shipped and fetchable.
    let dest_after = TempDir::new().unwrap();
    let out = Command::new("git")
        .arg("clone")
        .arg(format!("http://127.0.0.1:{port}/my-fork.git"))
        .arg(dest_after.path())
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "clone after merge failed");
    assert!(
        dest_after.path().join("feature.txt").exists(),
        "feature must land on main and be fetchable"
    );

    // Delete the project via web; the repo page and clone now 404.
    let resp = http_post_form(port, "/projects/delete", "name=my-fork").await;
    assert!(resp.contains("303 See Other"), "delete response:\n{resp}");
    let body = http_get(port, "/my-fork/").await;
    assert!(body.contains("404 Not Found"), "after delete:\n{body}");
    assert!(!manager.repo_dir("my-fork").exists(), "bare clone should be gone");

    drop(server);
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Live chat bridge (web server + a mock omega-loop daemon socket)
// ---------------------------------------------------------------------------

/// GET a path and return the first chunk (for SSE streams that stay open).
async fn http_get_stream_head(port: u16, path: &str) -> String {
    use tokio::io::AsyncReadExt;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    // Accumulate until we see the first SSE data frame (the body arrives in
    // later TCP segments, after the chunked headers).
    let mut all = Vec::new();
    let mut buf = [0u8; 4096];
    for _ in 0..50 {
        let n = tokio::time::timeout(Duration::from_millis(150), stream.read(&mut buf))
            .await
            .unwrap_or(Ok(0));
        match n {
            Ok(0) => break, // EOF
            Ok(n) => all.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
        let text = String::from_utf8_lossy(&all);
        // Keep reading until the mock's tool_end frame (or an error frame)
        // lands; the first "data:" (a cleared marker) arrives before the
        // chunks.
        if text.contains("tool_end") || text.contains("\"error\"") {
            break;
        }
    }
    String::from_utf8_lossy(&all).to_string()
}

/// A minimal mock omega-loop daemon on a Unix socket: it understands just
/// enough of the wire protocol to make the web chat bridge behave. For each
/// connection it reads newline-delimited JSON requests and replies with the
/// events a real daemon would emit.
async fn spawn_mock_daemon(socket: &Path) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (reader, mut writer) = stream.into_split();
            tokio::spawn(async move {
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let v: serde_json::Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    let sid = v
                        .get("session_id")
                        .and_then(|s| s.as_str())
                        .unwrap_or("mock");
                    let mut out = String::new();
                    match ty {
                        "activate_project" => {
                            out = format!(
                                "{{\"type\":\"ProjectActive\",\"project\":{{\"name\":\"mock-proj\",\"url\":\"x\",\"created_at\":\"2025-01-01T00:00:00Z\"}},\"worktree_path\":\"/tmp/mock/wt\",\"branch\":\"omega/web-abc123\"}}\n"
                            );
                        }
                        "resume_session" => {
                            out = format!(
                                "{{\"type\":\"SessionResumed\",\"session_id\":\"{sid}\",\"session_name\":\"{sid}\"}}\n"
                            );
                            out += &format!(
                                "{{\"type\":\"HistoryMessage\",\"session_id\":\"{sid}\",\"role\":\"user\",\"content\":\"hello\"}}\n"
                            );
                            // A realistic in-flight turn: thinking, text, and a
                            // tool call with progress + result, ending in Done.
                            out += &format!(
                                "{{\"type\":\"Chunk\",\"session_id\":\"{sid}\",\"chunk\":{{\"ThinkingDelta\":\"hmm, \"}}}}\n"
                            );
                            out += &format!(
                                "{{\"type\":\"Chunk\",\"session_id\":\"{sid}\",\"chunk\":{{\"ThinkingComplete\":\"hmm, let me check\"}}}}\n"
                            );
                            out += &format!(
                                "{{\"type\":\"Chunk\",\"session_id\":\"{sid}\",\"chunk\":{{\"TextDelta\":\"world\"}}}}\n"
                            );
                            out += &format!(
                                "{{\"type\":\"Chunk\",\"session_id\":\"{sid}\",\"chunk\":{{\"ToolStart\":{{\"id\":\"call_1\",\"name\":\"Bash\",\"input\":{{\"command\":\"ls\"}}}}}}}}\n"
                            );
                            out += &format!(
                                "{{\"type\":\"Chunk\",\"session_id\":\"{sid}\",\"chunk\":{{\"ToolProgress\":{{\"id\":\"call_1\",\"output\":\"reading…\"}}}}}}\n"
                            );
                            out += &format!(
                                "{{\"type\":\"Chunk\",\"session_id\":\"{sid}\",\"chunk\":{{\"ToolEnd\":{{\"id\":\"call_1\",\"name\":\"Bash\",\"input\":{{\"command\":\"ls\"}},\"result\":{{\"content\":{{\"Text\":\"file1\\nfile2\"}},\"is_error\":false}}}}}}}}\n"
                            );
                            out += &format!(
                                "{{\"type\":\"Chunk\",\"session_id\":\"{sid}\",\"chunk\":\"Done\"}}\n"
                            );
                        }
                        "run" | "message" => {
                            out = format!(
                                "{{\"type\":\"Created\",\"session_id\":\"{sid}\",\"session_name\":\"{sid}\"}}\n"
                            );
                        }
                        _ => {}
                    }
                    if !out.is_empty() {
                        if writer.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                }
            });
        }
    });
}

/// Web chat end-to-end: the web server drives the daemon to start a session,
/// renders the live chat page, streams its output over SSE, and forwards
/// messages + interrupts. The daemon is a mock so the test stays hermetic.
#[tokio::test]
async fn web_chat_start_session_stream_message_and_interrupt() {
    let store = TempDir::new().unwrap();
    let sessions = TempDir::new().unwrap();
    let socket = sessions.path().join("loop.sock");
    spawn_mock_daemon(&socket).await;

    // Seed a registered project + a session on disk so the live page renders.
    let (project, worktree_branch) = seed_store(store.path()).await;
    seed_session(sessions.path(), "web-chat-1", &worktree_branch, "Chat session").await;

    let port = free_port();
    let server = Command::new(env!("CARGO_BIN_EXE_omega-git-host"))
        .env("OMEGA_PROJECTS_DIR", store.path())
        .env("OMEGA_SESSION_DIR", sessions.path())
        .env("OMEGA_LOOP_SOCKET_PATH", &socket)
        .env("OMEGA_GIT_HOST_LISTEN", "127.0.0.1")
        .env("OMEGA_GIT_HOST_PORT", port.to_string())
        .env("RUST_LOG", "warn")
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn omega-git-host");
    wait_for_server(port).await;

    // The project's sessions page has the "Start session" form.
    let body = http_get(port, &format!("/{project}/sessions")).await;
    assert!(body.contains("200 OK"), "repo sessions:\n{body}");
    assert!(body.contains("Start session"), "repo sessions:\n{body}");

    // Start a session: the web POSTs activate_project to the daemon, which
    // (mock) replies ProjectActive → we land on the live chat page.
    let resp = http_post_form(port, &format!("/{project}/sessions/create"), "prompt=hi").await;
    assert!(resp.contains("303 See Other"), "create session:\n{resp}");
    let location = resp
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("location:"))
        .map(|l| l.splitn(2, ':').nth(1).unwrap().trim().to_string())
        .expect("redirect location");
    assert!(
        location.starts_with("/sessions/web-"),
        "redirect to live chat: {location}"
    );

    // The live chat page renders with the message box + interrupt + stream
    // url, and loads the shared client that renders live turns with the same
    // per-message layout as the persisted transcript.
    let body = http_get(port, "/sessions/web-chat-1/live").await;
    assert!(body.contains("200 OK"), "live page:\n{body}");
    assert!(body.contains("Send a message"), "live page:\n{body}");
    assert!(body.contains("Interrupt"), "live page:\n{body}");
    assert!(
        body.contains("/sessions/web-chat-1/stream"),
        "live page streams:\n{body}"
    );
    assert!(
        body.contains("/static/live.js"),
        "live page loads the client:\n{body}"
    );
    assert!(
        body.contains("id=\"live-conversation\""),
        "live page has the shared conversation container:\n{body}"
    );
    // The persisted transcript is server-rendered *inside* the live
    // container too (one code path for past and future messages)…
    assert!(
        body.contains("please add a feature"),
        "live page renders the persisted transcript:\n{body}"
    );

    // The SSE stream opens: 200, event-stream, and the mock daemon's turn
    // arrives as *structured* frames (text + thinking + tool call) — the raw
    // input the client folds into one assistant message, like the transcript
    // assembler does server-side.
    let head = http_get_stream_head(port, "/sessions/web-chat-1/stream").await;
    assert!(
        head.contains("200 OK") && head.contains("text/event-stream"),
        "stream head:\n{head}"
    );
    assert!(
        head.contains("\"type\":\"text\"") && head.contains("world"),
        "text delta frame:\n{head}"
    );
    assert!(
        head.contains("\"type\":\"thinking\""),
        "thinking frames:\n{head}"
    );
    assert!(
        head.contains("\"type\":\"thinking_complete\""),
        "thinking complete frame:\n{head}"
    );
    assert!(
        head.contains("\"type\":\"tool_start\"") && head.contains("\"name\":\"Bash\""),
        "tool start frame:\n{head}"
    );
    assert!(
        head.contains("\"type\":\"tool_end\"") && head.contains("\"preview\":\"file1 file2\""),
        "tool end frame with server-computed preview:\n{head}"
    );
    assert!(
        head.contains("\"type\":\"done\""),
        "done frame:\n{head}"
    );

    // Send a chat message → the web forwards run to the daemon and redirects.
    let resp = http_post_form(
        port,
        "/sessions/web-chat-1/message",
        "content=please+do+the+thing",
    )
    .await;
    assert!(resp.contains("303 See Other"), "message:\n{resp}");

    // Interrupt → forwarded to the daemon, redirect back.
    let resp = http_post_form(port, "/sessions/web-chat-1/interrupt", "").await;
    assert!(resp.contains("303 See Other"), "interrupt:\n{resp}");

    drop(server);
}
