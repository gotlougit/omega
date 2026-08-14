//! # omega-projects — git-worktree-based project store for agent sessions
//!
//! Manages a store of registered git projects plus a dedicated git worktree
//! per agent session:
//!
//! ```text
//! <root>/
//!   registry.json               — registered projects (name → url)
//!   repos/<name>/               — bare clone of the project (cloned once)
//!   worktrees/<name>/           — one directory per session worktree
//!     <session>-<rand>/         — `git worktree add` checkout
//! ```
//!
//! Every activation of a project for a session creates a **brand-new**
//! worktree on its own branch. Git worktrees are fully isolated checkouts
//! that share only the object database, so other sessions working on the
//! same repository in their own worktrees are never disturbed — they keep
//! their own working tree, index, and branch.
//!
//! The store is self-contained under a single root directory (default:
//! `$OMEGA_PROJECTS_DIR` or `./projects`), so tests can point it at a
//! [`tempfile::TempDir`] and tear everything down simply by dropping it.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// A registered project: a git repository cloned into the project store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectInfo {
    /// Stable local name (derived from the URL at registration).
    pub name: String,
    /// Remote URL the project was registered from.
    pub url: String,
    /// Default branch at clone time (informational; may drift).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// When the project was first registered.
    pub created_at: DateTime<Utc>,
}

/// A project bound to one agent session: the registered repo plus the
/// dedicated git worktree the session operates in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveProject {
    pub project: ProjectInfo,
    /// Absolute path to the worktree on disk.
    pub worktree_path: String,
    /// Branch this worktree is checked out on (unique per worktree).
    pub branch: String,
}

/// Manager for the project store.
#[derive(Debug, Clone)]
pub struct ProjectManager {
    root: PathBuf,
}

impl ProjectManager {
    /// The default projects directory: `$OMEGA_PROJECTS_DIR` if set,
    /// otherwise `./projects` relative to the daemon's working directory.
    pub fn default_root() -> PathBuf {
        std::env::var("OMEGA_PROJECTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("projects"))
    }

    pub fn new() -> Self {
        Self::with_root(Self::default_root())
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The store root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory of the bare clone for `name`.
    pub fn repo_dir(&self, name: &str) -> PathBuf {
        self.root.join("repos").join(name)
    }

    /// Directory holding all session worktrees for `name`.
    pub fn worktrees_dir(&self, name: &str) -> PathBuf {
        self.root.join("worktrees").join(name)
    }

    fn registry_path(&self) -> PathBuf {
        self.root.join("registry.json")
    }

    // -----------------------------------------------------------------------
    // Listing & lookup
    // -----------------------------------------------------------------------

    /// List all registered projects, sorted by name.
    pub async fn list(&self) -> Result<Vec<ProjectInfo>> {
        let mut projects = self.load_registry().await?.unwrap_or_default();
        projects.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(projects)
    }

    /// Look up a registered project by exact name.
    pub async fn find(&self, name: &str) -> Result<Option<ProjectInfo>> {
        Ok(self
            .load_registry()
            .await?
            .unwrap_or_default()
            .into_iter()
            .find(|p| p.name == name))
    }

    // -----------------------------------------------------------------------
    // Activation
    // -----------------------------------------------------------------------

    /// Activate `spec` for `session_id`.
    ///
    /// `spec` is either the name of an already-registered project or a git
    /// URL (in which case the project is registered and cloned on first
    /// use). When `existing` is a worktree for the *same* project that is
    /// still present on disk, it is reused; otherwise a fresh worktree is
    /// created.
    pub async fn activate(
        &self,
        spec: &str,
        session_id: &str,
        existing: Option<&ActiveProject>,
    ) -> Result<ActiveProject> {
        let info = self.resolve(spec).await?;
        if let Some(active) = existing {
            if active.project.name == info.name && Path::new(&active.worktree_path).is_dir() {
                return Ok(active.clone());
            }
        }
        self.create_worktree(&info, session_id).await
    }

    /// Resolve `spec` to a registered [`ProjectInfo`], cloning the
    /// repository on first use. Registered names are matched exactly; any
    /// other input is treated as a git URL.
    pub async fn resolve(&self, spec: &str) -> Result<ProjectInfo> {
        let spec = spec.trim();
        if spec.is_empty() {
            bail!("empty project spec");
        }
        if let Some(existing) = self.find(spec).await? {
            // Refresh the clone best-effort. A transient network problem
            // must never block activation — the local clone is still usable.
            if let Err(e) = self.fetch(&existing).await {
                tracing::debug!(project = %existing.name, error = %e, "background fetch failed");
            }
            return Ok(existing);
        }

        let name = name_from_url(spec);
        if name.is_empty() {
            bail!("cannot derive a project name from '{spec}'");
        }
        let mut info = ProjectInfo {
            name,
            url: spec.to_string(),
            default_branch: None,
            created_at: Utc::now(),
        };
        self.clone_repo(&info).await?;
        info.default_branch = self.default_branch(&info).await;
        self.register(&info).await?;
        tracing::info!(project = %info.name, url = %info.url, "registered project");
        Ok(info)
    }

    /// Create a fresh worktree for `info` on its own branch, named after
    /// the owning session. Every call creates a new, isolated checkout.
    pub async fn create_worktree(
        &self,
        info: &ProjectInfo,
        session_id: &str,
    ) -> Result<ActiveProject> {
        let repo = self.repo_dir(&info.name);
        if !repo.join("HEAD").exists() {
            bail!(
                "project '{}' has no local clone yet (repo {} missing)",
                info.name,
                repo.display()
            );
        }

        let base = self
            .default_branch(info)
            .await
            .unwrap_or_else(|| "HEAD".to_string());
        let session_tag = sanitize_ref_component(session_id);
        let wt_root = self.worktrees_dir(&info.name);
        tokio::fs::create_dir_all(&wt_root).await?;

        // Try a handful of random suffixes; a branch-name collision is
        // virtually impossible but retrying is free.
        let mut last_err: Option<anyhow::Error> = None;
        for _ in 0..5 {
            let rand = random_suffix();
            let branch = format!("omega/{session_tag}-{rand}");
            let worktree_dir = wt_root.join(format!("{session_tag}-{rand}"));
            let wt_str = worktree_dir.to_string_lossy().to_string();
            match git(&repo, &["worktree", "add", "-b", &branch, &wt_str, &base]).await {
                Ok(_) => {
                    let worktree_path = worktree_dir.canonicalize().unwrap_or(worktree_dir);
                    return Ok(ActiveProject {
                        project: info.clone(),
                        worktree_path: worktree_path.display().to_string(),
                        branch,
                    });
                }
                Err(e) => last_err = Some(e),
            }
        }
        bail!(
            "could not create a git worktree for '{}' after 5 attempts{}",
            info.name,
            last_err.map(|e| format!(": {e:#}")).unwrap_or_default()
        )
    }

    /// Remove a session worktree: deletes the checkout and its branch, and
    /// prunes the worktree metadata. Other worktrees are untouched.
    pub async fn remove_worktree(&self, active: &ActiveProject) -> Result<()> {
        let repo = self.repo_dir(&active.project.name);
        if !repo.join("HEAD").exists() {
            return Ok(()); // repo already gone; nothing to prune
        }
        let _ = git(
            &repo,
            &["worktree", "remove", "--force", &active.worktree_path],
        )
        .await;
        let _ = git(&repo, &["branch", "-D", &active.branch]).await;
        let _ = git(&repo, &["worktree", "prune"]).await;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Low-level git operations
    // -----------------------------------------------------------------------

    async fn clone_repo(&self, info: &ProjectInfo) -> Result<()> {
        let repo = self.repo_dir(&info.name);
        if repo.join("HEAD").exists() {
            return Ok(()); // already cloned
        }
        if let Some(parent) = repo.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Clone into a temp dir and rename into place, so two concurrent
        // activations of the same URL cannot clobber each other.
        let file_name = repo
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        let tmp = repo.with_file_name(format!("{file_name}.tmp-{}", random_suffix()));

        let out = Command::new("git")
            .arg("clone")
            .arg("--bare")
            .arg(&info.url)
            .arg(&tmp)
            .output()
            .await
            .with_context(|| format!("failed to clone {}", info.url))?;
        if !out.status.success() {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            bail!(
                "git clone {} failed: {}",
                info.url,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        match tokio::fs::rename(&tmp, &repo).await {
            Ok(()) => Ok(()),
            Err(_) => {
                // Someone else cloned it meanwhile — keep theirs.
                let _ = tokio::fs::remove_dir_all(&tmp).await;
                if repo.join("HEAD").exists() {
                    Ok(())
                } else {
                    bail!(
                        "clone raced and produced no repository at {}",
                        repo.display()
                    )
                }
            }
        }
    }

    async fn fetch(&self, info: &ProjectInfo) -> Result<()> {
        let repo = self.repo_dir(&info.name);
        git(&repo, &["fetch", "origin", "--prune"]).await?;
        Ok(())
    }

    /// Best-effort default branch detection: the bare clone's own HEAD
    /// (which mirrors the remote's default branch at clone time).
    async fn default_branch(&self, info: &ProjectInfo) -> Option<String> {
        let repo = self.repo_dir(&info.name);
        for args in [
            vec!["symbolic-ref", "--short", "HEAD"],
            vec!["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            vec!["rev-parse", "--abbrev-ref", "HEAD"],
        ] {
            if let Ok(out) = git(&repo, &args).await {
                let branch = out.trim().to_string();
                if !branch.is_empty() && branch != "HEAD" {
                    return Some(branch);
                }
            }
        }
        None
    }

    async fn register(&self, info: &ProjectInfo) -> Result<()> {
        let mut registry = self.load_registry().await?.unwrap_or_default();
        if let Some(existing) = registry.iter_mut().find(|p| p.name == info.name) {
            *existing = info.clone();
        } else {
            registry.push(info.clone());
        }
        tokio::fs::create_dir_all(&self.root).await?;
        let json = serde_json::to_string_pretty(&registry)?;
        tokio::fs::write(self.registry_path(), json).await?;
        Ok(())
    }

    async fn load_registry(&self) -> Result<Option<Vec<ProjectInfo>>> {
        let path = self.registry_path();
        if !path.exists() {
            return Ok(None);
        }
        let data = tokio::fs::read(&path).await?;
        let registry: Vec<ProjectInfo> = serde_json::from_slice(&data)?;
        Ok(Some(registry))
    }
}

impl Default for ProjectManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run `git -C <repo> <args>` and return trimmed stdout on success.
async fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to run `git -C {} {}`",
                repo.display(),
                args.join(" ")
            )
        })?;
    if !out.status.success() {
        bail!(
            "`git -C {} {}` failed: {}",
            repo.display(),
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Derive a stable local project name from a git URL.
///
/// Handles https/ssh/file URLs, scp-style `git@host:path` syntax, ports,
/// and a trailing `.git` or `/`:
/// - `https://github.com/foo/bar.git` → `bar`
/// - `git@github.com:foo/baz.git` → `baz`
/// - `https://host:8443/x/y` → `y`
/// - `/tmp/some/repo` → `repo`
fn name_from_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let no_scheme = ["file://", "ssh://", "git://", "http://", "https://"]
        .iter()
        .find_map(|scheme| trimmed.strip_prefix(scheme))
        .unwrap_or(trimmed);
    // scp-style `git@host:path/to/repo.git` — take everything after the
    // last colon (also handles `https://host:8443/...` after scheme strip).
    let path_part = no_scheme
        .rsplit_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or(no_scheme);
    let name = path_part
        .trim_end_matches(".git")
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(trimmed);
    name.to_string()
}

/// Make `s` safe to embed in a git branch name / directory name.
fn sanitize_ref_component(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "session".to_string()
    } else {
        out
    }
}

/// Short random suffix for unique branch/worktree names.
fn random_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..6].to_string()
}

// ---------------------------------------------------------------------------
// Tests — use real `git` against throwaway temp-dir repositories
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A throwaway "origin" repository plus a throwaway project store.
    /// Dropping the fixture removes both (nothing is committed or shared).
    struct Fixture {
        _remote: TempDir,
        _store: TempDir,
        remote_url: String,
        manager: ProjectManager,
    }

    async fn fixture() -> Fixture {
        let remote = TempDir::new().unwrap();
        let store = TempDir::new().unwrap();
        init_remote(remote.path()).await;
        let remote_url = remote.path().display().to_string();
        let manager = ProjectManager::with_root(store.path());
        Fixture {
            _remote: remote,
            _store: store,
            remote_url,
            manager,
        }
    }

    /// Create a git repo with one commit and a README file.
    async fn init_remote(dir: &Path) {
        git(dir, &["init", "-b", "main"]).await.unwrap();
        git(dir, &["config", "user.email", "omega-test@example.com"])
            .await
            .unwrap();
        git(dir, &["config", "user.name", "Omega Test"])
            .await
            .unwrap();
        // Don't inherit the host's commit.gpgsign — signing would prompt/
        // hang on a throwaway repo that has no signing key.
        git(dir, &["config", "commit.gpgsign", "false"])
            .await
            .unwrap();
        tokio::fs::write(dir.join("README.md"), "# Dummy project\n")
            .await
            .unwrap();
        git(dir, &["add", "."]).await.unwrap();
        git(dir, &["commit", "-m", "initial commit"]).await.unwrap();
    }

    /// Make commits from inside a worktree possible (identity lives in the
    /// bare clone's common config, shared by all of its worktrees).
    async fn configure_worktree_identity(manager: &ProjectManager, info: &ProjectInfo) {
        let repo = manager.repo_dir(&info.name);
        git(&repo, &["config", "user.email", "omega-test@example.com"])
            .await
            .unwrap();
        git(&repo, &["config", "user.name", "Omega Test"])
            .await
            .unwrap();
        // Worktree commits run against the shared config too — disable
        // signing so the test never prompts/hangs on a missing key.
        git(&repo, &["config", "commit.gpgsign", "false"])
            .await
            .unwrap();
    }

    fn count_lines(s: &str) -> usize {
        s.lines().count()
    }

    // -----------------------------------------------------------------------
    // Listing & registration
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_starts_empty() {
        let fx = fixture().await;
        assert!(fx.manager.list().await.unwrap().is_empty());
        assert!(fx.manager.find("anything").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn activate_by_url_registers_clones_and_creates_worktree() {
        let fx = fixture().await;
        let active = fx
            .manager
            .activate(&fx.remote_url, "sess-1", None)
            .await
            .unwrap();

        // Registered in the store with a derived name.
        let projects = fx.manager.list().await.unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].url, fx.remote_url);
        assert_eq!(projects[0].name, active.project.name);
        assert!(!active.project.name.is_empty());

        // Bare clone exists and the default branch was detected.
        let repo = fx.manager.repo_dir(&active.project.name);
        assert!(repo.join("HEAD").exists(), "bare clone should exist");
        assert_eq!(active.project.default_branch.as_deref(), Some("main"));

        // Worktree exists, is a real git checkout, and has the content.
        let wt = Path::new(&active.worktree_path);
        assert!(wt.is_dir(), "worktree dir should exist");
        assert_eq!(
            git(wt, &["rev-parse", "--is-inside-work-tree"])
                .await
                .unwrap(),
            "true"
        );
        assert!(wt.join("README.md").exists());
        assert!(
            active.worktree_path.starts_with(
                fx.manager
                    .worktrees_dir(&active.project.name)
                    .display()
                    .to_string()
                    .as_str()
            ) || active.worktree_path.contains("worktrees"),
            "worktree should live under the store's worktrees dir"
        );
        assert!(
            active.branch.starts_with("omega/"),
            "branch: {}",
            active.branch
        );
    }

    #[tokio::test]
    async fn activating_by_registered_name_reuses_clone() {
        let fx = fixture().await;
        let first = fx
            .manager
            .activate(&fx.remote_url, "sess-a", None)
            .await
            .unwrap();

        // Second activation by NAME (not URL) — repo must not be re-cloned,
        // but a brand-new worktree is created for the other session.
        let second = fx
            .manager
            .activate(&first.project.name, "sess-b", None)
            .await
            .unwrap();

        assert_eq!(second.project.url, fx.remote_url);
        assert_ne!(first.worktree_path, second.worktree_path);
        assert_ne!(first.branch, second.branch);

        let list = git(
            &fx.manager.repo_dir(&first.project.name),
            &["worktree", "list"],
        )
        .await
        .unwrap();
        assert!(
            list.contains(&first.worktree_path),
            "worktree list should contain first worktree:\n{list}"
        );
        assert!(
            list.contains(&second.worktree_path),
            "worktree list should contain second worktree:\n{list}"
        );
        assert_eq!(count_lines(&list), 3, "bare repo + 2 worktrees");
    }

    #[tokio::test]
    async fn same_session_same_project_reuses_worktree() {
        let fx = fixture().await;
        let first = fx
            .manager
            .activate(&fx.remote_url, "sess-1", None)
            .await
            .unwrap();

        // Re-activating the same project for the same session reuses the
        // worktree instead of piling up new ones.
        let second = fx
            .manager
            .activate(&fx.remote_url, "sess-1", Some(&first))
            .await
            .unwrap();
        assert_eq!(second.worktree_path, first.worktree_path);
        assert_eq!(second.branch, first.branch);

        let list = git(
            &fx.manager.repo_dir(&first.project.name),
            &["worktree", "list"],
        )
        .await
        .unwrap();
        assert_eq!(count_lines(&list), 2, "bare repo + 1 worktree");
    }

    // -----------------------------------------------------------------------
    // Worktree isolation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn worktrees_are_fully_isolated() {
        let fx = fixture().await;
        let w1 = fx
            .manager
            .activate(&fx.remote_url, "sess-a", None)
            .await
            .unwrap();
        let w2 = fx
            .manager
            .activate(&fx.remote_url, "sess-b", None)
            .await
            .unwrap();
        configure_worktree_identity(&fx.manager, &w1.project).await;

        let wt1 = Path::new(&w1.worktree_path);
        let wt2 = Path::new(&w2.worktree_path);

        // Each worktree is on its own branch.
        let b1 = git(wt1, &["branch", "--show-current"]).await.unwrap();
        let b2 = git(wt2, &["branch", "--show-current"]).await.unwrap();
        assert_eq!(b1, w1.branch);
        assert_eq!(b2, w2.branch);
        assert_ne!(b1, b2);

        // A change in worktree 1 (new file + edit to a tracked file) must
        // not leak into worktree 2.
        tokio::fs::write(wt1.join("feature.txt"), "sess-a only\n")
            .await
            .unwrap();
        tokio::fs::write(wt1.join("README.md"), "# Changed by sess-a\n")
            .await
            .unwrap();
        git(wt1, &["add", "."]).await.unwrap();
        git(wt1, &["commit", "-m", "sess-a change"]).await.unwrap();

        assert!(
            !wt2.join("feature.txt").exists(),
            "w2 must not see w1's new file"
        );
        assert_eq!(
            tokio::fs::read_to_string(wt2.join("README.md"))
                .await
                .unwrap(),
            "# Dummy project\n",
            "w2 must keep its own unmodified working tree"
        );

        // The committed change lives on w1's own branch in the shared
        // object database — visible from anywhere, but w2's branch is
        // untouched.
        let w1_branch_log = git(wt2, &["log", "--oneline", "-1", &w1.branch])
            .await
            .unwrap();
        assert!(w1_branch_log.contains("sess-a change"));
        let w2_log = git(wt2, &["log", "--oneline", "-1"]).await.unwrap();
        assert!(w2_log.contains("initial commit"));
    }

    // -----------------------------------------------------------------------
    // Cleanup
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn remove_worktree_prunes_dir_and_branch() {
        let fx = fixture().await;
        let active = fx
            .manager
            .activate(&fx.remote_url, "sess-1", None)
            .await
            .unwrap();
        let repo = fx.manager.repo_dir(&active.project.name);

        fx.manager.remove_worktree(&active).await.unwrap();

        assert!(
            !Path::new(&active.worktree_path).exists(),
            "worktree dir removed"
        );
        let branches = git(&repo, &["branch", "--list", &active.branch])
            .await
            .unwrap();
        assert!(branches.is_empty(), "branch should be deleted");
        let list = git(&repo, &["worktree", "list"]).await.unwrap();
        assert!(
            !list.contains(&active.worktree_path),
            "worktree list should no longer contain the removed worktree:\n{list}"
        );

        // The project itself stays registered; a later activation works.
        let again = fx
            .manager
            .activate(&active.project.name, "sess-1", None)
            .await
            .unwrap();
        assert!(Path::new(&again.worktree_path).is_dir());
    }

    // -----------------------------------------------------------------------
    // Errors
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn activation_of_uncloneable_url_fails_cleanly() {
        let fx = fixture().await;
        let err = fx
            .manager
            .activate("/nonexistent/definitely-not-a-repo-xyz", "sess-1", None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("clone"),
            "error should mention the clone failure: {err:#}"
        );
        assert!(
            fx.manager.list().await.unwrap().is_empty(),
            "failed activation must not register the project"
        );
    }

    #[tokio::test]
    async fn empty_spec_is_rejected() {
        let fx = fixture().await;
        assert!(fx.manager.activate("   ", "sess-1", None).await.is_err());
    }

    // -----------------------------------------------------------------------
    // name_from_url
    // -----------------------------------------------------------------------

    #[test]
    fn name_from_url_variants() {
        assert_eq!(name_from_url("https://github.com/foo/bar.git"), "bar");
        assert_eq!(name_from_url("git@github.com:foo/baz.git"), "baz");
        assert_eq!(name_from_url("https://host:8443/x/y"), "y");
        assert_eq!(name_from_url("/tmp/some/repo"), "repo");
        assert_eq!(name_from_url("file:///tmp/one/two"), "two");
        assert_eq!(name_from_url("https://github.com/foo/bar.git/"), "bar");
    }

    #[test]
    fn sanitize_ref_component_replaces_illegal_chars() {
        assert_eq!(sanitize_ref_component("tui-abc123"), "tui-abc123");
        assert_eq!(sanitize_ref_component("sess 1/foo"), "sess-1-foo");
        assert_eq!(sanitize_ref_component("!!!"), "session");
    }
}
