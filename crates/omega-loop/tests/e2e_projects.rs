//! End-to-end tests for project support: spawn the real `omega-loop` daemon
//! against throwaway temp directories and drive it over the Unix socket with
//! the real client library. Everything is set up and torn down inside the
//! test — no fixtures are committed.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use omega_loop_client::{connect_to, DaemonReader, ServerEvent};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

/// Spawn the actual `omega-loop` binary with isolated temp state.
async fn spawn_daemon() -> (Child, TempWorkspace) {
    let bin = env!("CARGO_BIN_EXE_omega-loop");
    eprintln!("DEBUG: omega-loop bin = {bin}");
    assert!(
        std::path::Path::new(bin).exists(),
        "omega-loop binary does not exist at {bin}"
    );
    let ws = TempWorkspace::new().await;
    let child = Command::new(bin)
        .env("OMEGA_LOOP_SOCKET_PATH", &ws.socket_path)
        .env("OMEGA_PROJECTS_DIR", &ws.projects_dir)
        .env("RUST_LOG", "warn")
        .current_dir(&ws.work_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn omega-loop");
    wait_for_socket(&ws.socket_path).await;
    (child, ws)
}

/// Poll until the daemon's Unix socket is connectable.
async fn wait_for_socket(path: &str) {
    for _ in 0..100 {
        if UnixStream::connect(path).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon socket {path} never became connectable");
}

/// Read events until one matches `predicate`.
async fn wait_for_event<F>(reader: &mut DaemonReader, predicate: F) -> ServerEvent
where
    F: Fn(&ServerEvent) -> bool,
{
    for _ in 0..200 {
        if let Some(event) = reader.recv_event().await.expect("recv event") {
            if predicate(&event) {
                return event;
            }
        } else {
            panic!("daemon closed the connection");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for daemon event");
}

fn project_active(event: &ServerEvent) -> Option<(String, String, String)> {
    match event {
        ServerEvent::ProjectActive {
            project,
            worktree_path,
            branch,
        } => Some((project.name.clone(), worktree_path.clone(), branch.clone())),
        _ => None,
    }
}

/// Everything the daemon touches, in temp dirs that vanish on drop.
struct TempWorkspace {
    socket_path: String,
    /// cwd of the daemon — `./sessions` lands here.
    work_dir: PathBuf,
    /// OMEGA_PROJECTS_DIR for the daemon.
    projects_dir: PathBuf,
    /// Keeps the whole tree alive for the duration of the test.
    _base: tempfile::TempDir,
    _remote: tempfile::TempDir,
}

impl TempWorkspace {
    async fn new() -> Self {
        let base = tempfile::TempDir::new().unwrap();
        let work_dir = base.path().join("work");
        let projects_dir = base.path().join("projects");
        tokio::fs::create_dir_all(&work_dir).await.unwrap();
        tokio::fs::create_dir_all(&projects_dir).await.unwrap();
        let socket_path = base
            .path()
            .join("omega-loop.sock")
            .display()
            .to_string();

        // A throwaway remote repo with one committed file. It lives in a
        // `dummy-repo` subdir so the derived project name is stable.
        let remote = tempfile::TempDir::new().unwrap();
        let remote_repo = remote.path().join("dummy-repo");
        tokio::fs::create_dir_all(&remote_repo).await.unwrap();
        git(&remote_repo, &["init", "-b", "main"]).await;
        git(&remote_repo, &["config", "user.email", "e2e@test"]).await;
        git(&remote_repo, &["config", "user.name", "E2E"]).await;
        tokio::fs::write(remote_repo.join("README.md"), "# Dummy repo\n")
            .await
            .unwrap();
        git(&remote_repo, &["add", "."]).await;
        git(&remote_repo, &["commit", "-m", "initial"]).await;

        Self {
            socket_path,
            work_dir,
            projects_dir,
            _base: base,
            _remote: remote,
        }
    }

    fn remote_url(&self) -> String {
        self._remote.path().join("dummy-repo").display().to_string()
    }
}

async fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full flow: list (empty) → activate by URL (clone + worktree created) →
/// list (now registered) → re-activate same session (worktree reused).
#[tokio::test]
async fn activate_project_end_to_end() {
    let (mut daemon, ws) = spawn_daemon().await;
    let (mut reader, mut writer) = connect_to(&ws.socket_path).await.unwrap();

    // 1. No projects registered yet.
    writer.send_list_projects().await.unwrap();
    match wait_for_event(&mut reader, |e| matches!(e, ServerEvent::ProjectList { .. })).await {
        ServerEvent::ProjectList { projects } => assert!(projects.is_empty()),
        _ => unreachable!(),
    }

    // 2. Activate by URL: daemon clones the repo and creates a worktree.
    writer
        .send_activate_project("e2e-sess", &ws.remote_url())
        .await
        .unwrap();
    let (name, worktree_path, branch) = wait_for_event(&mut reader, |e| project_active(e).is_some())
        .await
        .let_else_unwrap_project_active();

    assert_eq!(name, "dummy-repo");
    let wt = Path::new(&worktree_path);
    assert!(wt.is_dir(), "worktree must exist: {worktree_path}");
    assert!(
        wt.join("README.md").exists(),
        "worktree must contain the repo's files"
    );
    assert_eq!(
        tokio::fs::read_to_string(wt.join("README.md")).await.unwrap(),
        "# Dummy repo\n"
    );
    assert!(branch.starts_with("omega/e2e-sess-"), "branch: {branch}");

    // Worktree lives under the projects dir; the bare clone exists too.
    assert!(
        worktree_path.starts_with(&ws.projects_dir.display().to_string()),
        "worktree should be inside OMEGA_PROJECTS_DIR: {worktree_path}"
    );
    let bare = ws.projects_dir.join("repos").join("dummy-repo");
    assert!(bare.join("HEAD").exists(), "bare clone should exist");

    // 3. Now the project is registered.
    writer.send_list_projects().await.unwrap();
    match wait_for_event(&mut reader, |e| matches!(e, ServerEvent::ProjectList { .. })).await {
        ServerEvent::ProjectList { projects } => {
            assert_eq!(projects.len(), 1);
            assert_eq!(projects[0].name, "dummy-repo");
            assert_eq!(projects[0].url, ws.remote_url());
        }
        _ => unreachable!(),
    }

    // 4. Re-activating for the SAME session reuses the same worktree (no
    // pile-up of worktrees per session).
    writer
        .send_activate_project("e2e-sess", "dummy-repo")
        .await
        .unwrap();
    let (_, reused_path, reused_branch) = wait_for_event(&mut reader, |e| project_active(e).is_some())
        .await
        .let_else_unwrap_project_active();
    assert_eq!(reused_path, worktree_path, "same session reuses its worktree");
    assert_eq!(reused_branch, branch);

    // 5. A DIFFERENT session gets its OWN isolated worktree (no disturbance).
    writer
        .send_activate_project("e2e-sess-2", "dummy-repo")
        .await
        .unwrap();
    let (_, other_path, other_branch) = wait_for_event(&mut reader, |e| project_active(e).is_some())
        .await
        .let_else_unwrap_project_active();
    assert_ne!(other_path, worktree_path, "different session → different worktree");
    assert_ne!(other_branch, branch);

    // Both worktrees exist side by side in the shared repo.
    let list = git_list(&bare, &["worktree", "list"]).await;
    assert!(list.contains(&worktree_path));
    assert!(list.contains(&other_path));

    // 6. A normal chat message (non-slash) is just forwarded — no LLM call
    // happens unless the agent runs tools, so this must succeed silently.
    writer.send_run("e2e-sess", "hello", &Default::default(), None, None).await.unwrap();
    writer.send_run("e2e-sess-2", "hello", &Default::default(), None, None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Both sessions' worktrees are untouched by the other's existence.
    assert!(Path::new(&worktree_path).is_dir());
    assert!(Path::new(&other_path).is_dir());

    // Cleanup: disconnect (daemon shuts down the sessions) + kill daemon.
    drop(reader);
    drop(writer);
    let _ = daemon.kill().await;
    let _ = daemon.wait().await;
}

/// Activating a bogus URL fails with a SystemMsg and registers nothing.
#[tokio::test]
async fn activate_bogus_url_reports_error() {
    let (mut daemon, ws) = spawn_daemon().await;
    let (mut reader, mut writer) = connect_to(&ws.socket_path).await.unwrap();

    writer
        .send_activate_project("e2e-sess", "/nonexistent/not-a-repo-xyz")
        .await
        .unwrap();
    match wait_for_event(&mut reader, |e| matches!(e, ServerEvent::SystemMsg(_))).await {
        ServerEvent::SystemMsg(msg) => {
            assert!(
                msg.contains("Cannot activate project"),
                "unexpected message: {msg}"
            );
        }
        _ => unreachable!(),
    }

    writer.send_list_projects().await.unwrap();
    match wait_for_event(&mut reader, |e| matches!(e, ServerEvent::ProjectList { .. })).await {
        ServerEvent::ProjectList { projects } => assert!(projects.is_empty()),
        _ => unreachable!(),
    }

    drop(reader);
    drop(writer);
    let _ = daemon.kill().await;
    let _ = daemon.wait().await;
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

async fn git_list(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Tiny extension so tests can destructure ProjectActive ergonomically.
trait LetElseUnwrapProjectActive {
    fn let_else_unwrap_project_active(self) -> (String, String, String);
}

impl LetElseUnwrapProjectActive for ServerEvent {
    fn let_else_unwrap_project_active(self) -> (String, String, String) {
        match self {
            ServerEvent::ProjectActive {
                project,
                worktree_path,
                branch,
            } => (project.name, worktree_path, branch),
            other => panic!("expected ProjectActive, got {other:?}"),
        }
    }
}
