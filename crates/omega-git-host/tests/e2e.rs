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
