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

/// Rebase-cron configuration & state, shared with omega-loop and
/// omega-git-host (see `rebase.rs`).
pub mod rebase;

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

/// Outcome of [`ProjectManager::update_default_branch`]: the project's
/// default branch relative to upstream after a fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultBranchStatus {
    /// Local default branch == upstream; nothing to do.
    UpToDate,
    /// Local default branch has commits that upstream does not, but
    /// upstream has no new commits; nothing to do (local-only
    /// functionality — never touch it).
    AheadOnly,
    /// Local default branch is strictly behind upstream; it was
    /// fast-forwarded. Carries (old_sha, new_sha).
    FastForwarded(String, String),
    /// Both sides moved: upstream has `behind` new commits and the local
    /// default branch has `ahead` local-only commits on top of
    /// upstream's history. Needs a real rebase (the dedicated
    /// upstream-rebaser agent handles it). Carries local_sha, upstream_sha.
    Diverged {
        local: String,
        upstream: String,
        ahead: usize,
        behind: usize,
    },
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

        // Make sure the default branch is as current as possible before the
        // worktree forks off it. Best-effort: an offline fetch must never
        // block activation — the local clone is still usable.
        if let Err(e) = self.update_default_branch(info).await {
            tracing::debug!(
                project = %info.name,
                error = %e,
                "could not refresh default branch before creating worktree"
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

    /// Rename a session worktree to a descriptive name, so it can be easily
    /// identified once the work is done.
    ///
    /// Renames both the checkout directory and its branch (`omega/<name>`),
    /// keeping the worktree attached to the same HEAD and preserving any
    /// (possibly uncommitted) files. Returns the updated [`ActiveProject`]
    /// with the new path and branch.
    pub async fn rename_worktree(
        &self,
        active: &ActiveProject,
        new_name: &str,
    ) -> Result<ActiveProject> {
        let name = sanitize_ref_component(new_name);
        if new_name.trim().is_empty() {
            bail!("worktree name must not be empty");
        }

        let repo = self.repo_dir(&active.project.name);
        if !repo.join("HEAD").exists() {
            bail!(
                "project '{}' has no local clone (repo {} missing)",
                active.project.name,
                repo.display()
            );
        }
        if !Path::new(&active.worktree_path).is_dir() {
            bail!(
                "worktree '{}' no longer exists on disk",
                active.worktree_path
            );
        }

        let new_branch = format!("omega/{name}");
        let new_path = self.worktrees_dir(&active.project.name).join(&name);

        // Nothing to do when the name is unchanged — the branch (and by
        // extension the checkout directory) already carries it.
        if new_branch == active.branch {
            return Ok(active.clone());
        }

        if new_path.exists() {
            bail!(
                "a worktree named '{name}' already exists at {}",
                new_path.display()
            );
        }

        // Move the checkout directory (--force so uncommitted files survive).
        if active.worktree_path != new_path.display().to_string() {
            git(
                &repo,
                &[
                    "worktree",
                    "move",
                    "--force",
                    &active.worktree_path,
                    &new_path.display().to_string(),
                ],
            )
            .await
            .with_context(|| {
                format!(
                    "failed to move worktree '{}' to '{}'",
                    active.worktree_path,
                    new_path.display()
                )
            })?;
        }

        // Rename the branch so the worktree keeps its identity. This works
        // while the branch is checked out in its own (just moved) worktree.
        if active.branch != new_branch {
            git(&repo, &["branch", "-m", &active.branch, &new_branch])
                .await
                .with_context(|| {
                    format!(
                        "failed to rename branch '{}' to '{new_branch}'",
                        active.branch
                    )
                })?;
        }

        Ok(ActiveProject {
            project: active.project.clone(),
            worktree_path: new_path
                .canonicalize()
                .unwrap_or(new_path)
                .display()
                .to_string(),
            branch: new_branch,
        })
    }

    /// Reconcile a session's worktree binding with reality on disk.
    ///
    /// Returns `Some(active)` when the worktree still exists — possibly with
    /// a corrected path/branch if it was renamed or moved *outside* the
    /// daemon (raw `git` commands). Resolves the worktree by:
    ///
    /// 1. the persisted path, when it still exists:
    ///    - the persisted branch is checked out → binding is current;
    ///    - otherwise → rebind to the branch actually checked out there;
    /// 2. the persisted *branch*, when the path is gone but the branch still
    ///    exists somewhere in the project (e.g. `git worktree move` kept the
    ///    branch but changed the directory).
    ///
    /// Returns `None` when neither the path nor the branch can be found, so
    /// the caller knows a fresh worktree must be created.
    ///
    /// This is what keeps the session metadata (and therefore the web UI's
    /// session → worktree mapping) accurate after a rename.
    pub async fn reconcile_worktree(&self, active: &ActiveProject) -> Option<ActiveProject> {
        let repo = self.repo_dir(&active.project.name);

        // Fast path: persisted binding is completely current.
        if Path::new(&active.worktree_path).is_dir() && branch_exists(&repo, &active.branch).await {
            return Some(active.clone());
        }

        // The directory survived → it owns the truth (branch may have been
        // renamed in place; path may be stale/canonicalized differently).
        let dir = Path::new(&active.worktree_path);
        if dir.is_dir() {
            let actual = git(dir, &["symbolic-ref", "--short", "HEAD"]).await.ok()?;
            if actual.is_empty() || actual == "HEAD" {
                return Some(active.clone());
            }
            return Some(ActiveProject {
                project: active.project.clone(),
                worktree_path: dir
                    .canonicalize()
                    .unwrap_or_else(|_| dir.to_path_buf())
                    .display()
                    .to_string(),
                branch: actual,
            });
        }

        // The directory moved but its branch survived (e.g. `git worktree
        // move`): find where the branch is checked out now.
        if branch_exists(&repo, &active.branch).await {
            if let Some(path) = worktree_path_for_branch(&repo, &active.branch).await {
                return Some(ActiveProject {
                    project: active.project.clone(),
                    worktree_path: path,
                    branch: active.branch.clone(),
                });
            }
        }

        None
    }

    // -----------------------------------------------------------------------
    // Upstream sync — the "rebase cron" keeps the default branch current
    // -----------------------------------------------------------------------

    /// Fetch upstream and classify (and where safe, mechanically update) the
    /// project's default branch relative to `origin/<default>`.
    ///
    /// - strictly behind  → fast-forward `refs/heads/<default>` (a pure
    ///   mirror update — never drops local-only commits),
    /// - diverged         → left alone for the upstream-rebaser agent (a
    ///   rebase needs judgment),
    /// - ahead-only       → left alone (local-only functionality).
    ///
    /// Fails only when the fetch itself fails (e.g. no network): callers
    /// that must not block on the network should ignore the error.
    pub async fn update_default_branch(
        &self,
        info: &ProjectInfo,
    ) -> Result<Option<DefaultBranchStatus>> {
        let repo = self.repo_dir(&info.name);
        if !repo.join("HEAD").exists() {
            bail!(
                "project '{}' has no local clone (repo {} missing)",
                info.name,
                repo.display()
            );
        }
        self.fetch(info).await?;

        let Some(default) = self.default_branch(info).await else {
            return Ok(None);
        };
        if default.is_empty() || default == "HEAD" {
            return Ok(None);
        }

        let local_ref = format!("refs/heads/{default}");
        let upstream_ref = format!("refs/remotes/origin/{default}");
        let local = match git(&repo, &["rev-parse", &local_ref]).await {
            Ok(sha) if !sha.is_empty() => sha,
            _ => return Ok(None),
        };
        let upstream = match git(&repo, &["rev-parse", &upstream_ref]).await {
            Ok(sha) if !sha.is_empty() => sha,
            _ => return Ok(None), // no upstream ref yet (e.g. unborn remote)
        };

        if local == upstream {
            return Ok(Some(DefaultBranchStatus::UpToDate));
        }

        // `git merge-base --is-ancestor A B` exits 0 when A is an ancestor
        // of B.
        let local_behind = is_ancestor(&repo, &local, &upstream).await;
        if local_behind {
            git(&repo, &["update-ref", &local_ref, &upstream]).await?;
            return Ok(Some(DefaultBranchStatus::FastForwarded(local, upstream)));
        }

        let upstream_behind = is_ancestor(&repo, &upstream, &local).await;
        if upstream_behind {
            return Ok(Some(DefaultBranchStatus::AheadOnly));
        }

        let ahead = rev_count(&repo, &upstream, &local).await;
        let behind = rev_count(&repo, &local, &upstream).await;
        Ok(Some(DefaultBranchStatus::Diverged {
            local,
            upstream,
            ahead,
            behind,
        }))
    }

    /// Ensure a dedicated worktree exists for the project's default branch —
    /// the "main worktree" at `worktrees/<name>/main`. `git rebase` needs a
    /// working tree, and the bare clone has none, so the upstream-rebaser
    /// agent operates here, on the branch that may carry local-only commits.
    ///
    /// Returns the binding (existing worktree is reconciled, never
    /// duplicated).
    pub async fn ensure_main_worktree(&self, info: &ProjectInfo) -> Result<ActiveProject> {
        let repo = self.repo_dir(&info.name);
        if !repo.join("HEAD").exists() {
            bail!(
                "project '{}' has no local clone (repo {} missing)",
                info.name,
                repo.display()
            );
        }
        let default = self
            .default_branch(info)
            .await
            .unwrap_or_else(|| "HEAD".to_string());
        let dir = self.worktrees_dir(&info.name).join("main");
        let wt_str = dir.display().to_string();

        // Already a live checkout? Reconcile with what's actually checked
        // out there (identity, not the dir name, is the truth).
        if dir.is_dir() && git(&dir, &["rev-parse", "--is-inside-work-tree"]).await.is_ok() {
            let actual = git(&dir, &["symbolic-ref", "--short", "HEAD"]).await.unwrap_or(default.clone());
            return Ok(ActiveProject {
                project: info.clone(),
                worktree_path: dir.canonicalize().unwrap_or(dir).display().to_string(),
                branch: if actual.is_empty() || actual == "HEAD" { default } else { actual },
            });
        }
        if dir.is_dir() && git(&dir, &["rev-parse", "--is-inside-work-tree"]).await.is_err() {
            tokio::fs::remove_dir_all(&dir).await.ok();
        }

        // The default branch may already be checked out elsewhere (a stale
        // main worktree someone created manually): reuse that location
        // instead of failing `git worktree add`.
        if let Some(path) = worktree_path_for_branch(&repo, &default).await {
            return Ok(ActiveProject {
                project: info.clone(),
                worktree_path: path,
                branch: default,
            });
        }

        git(&repo, &["worktree", "add", &wt_str, &default]).await?;
        tracing::info!(
            project = %info.name,
            path = %wt_str,
            branch = %default,
            "ensured main worktree"
        );
        Ok(ActiveProject {
            project: info.clone(),
            worktree_path: dir.canonicalize().unwrap_or(dir).display().to_string(),
            branch: default,
        })
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
        // Explicit refspec: keeps refs/remotes/origin/* up to date even for
        // clones made by `git clone` local-path optimizations, which skip
        // the remote-tracking refspec (tests use local dirs as remotes).
        git(
            &repo,
            &[
                "fetch",
                "origin",
                "--prune",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        )
        .await?;
        Ok(())
    }

    /// Best-effort default branch detection: the bare clone's own HEAD
    /// (which mirrors the remote's default branch at clone time).
    pub async fn default_branch(&self, info: &ProjectInfo) -> Option<String> {
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

/// Whether `ancestor` is an ancestor of `descendant` (`git merge-base
/// --is-ancestor`, which exits 0 for yes).
async fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> bool {
    git(
        repo,
        &[
            "merge-base",
            "--is-ancestor",
            ancestor,
            descendant,
        ],
    )
    .await
    .is_ok()
}

/// Number of commits reachable from `from` but not `to`
/// (`git rev-list --count from..to`).
async fn rev_count(repo: &Path, from: &str, to: &str) -> usize {
    git(repo, &["rev-list", "--count", &format!("{from}..{to}")])
        .await
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Whether a local branch exists in the bare clone.
async fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await
    .is_ok()
}

/// Find the worktree checkout directory where `branch` is currently checked
/// out, if any. Parses `git worktree list --porcelain` — each entry starts
/// with `worktree <path>` followed by a `branch refs/heads/...` line.
async fn worktree_path_for_branch(repo: &Path, branch: &str) -> Option<String> {
    let out = git(repo, &["worktree", "list", "--porcelain"]).await.ok()?;
    let target = format!("branch refs/heads/{branch}");
    let mut current_path: Option<String> = None;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(path.to_string());
        } else if line.trim() == target {
            return current_path;
        }
    }
    None
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
    // Worktree renaming
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rename_worktree_moves_dir_and_renames_branch() {
        let fx = fixture().await;
        let active = fx
            .manager
            .activate(&fx.remote_url, "sess-1", None)
            .await
            .unwrap();
        let repo = fx.manager.repo_dir(&active.project.name);
        configure_worktree_identity(&fx.manager, &active.project).await;

        // Put some (uncommitted) work in the worktree — a rename must keep it.
        let wt = Path::new(&active.worktree_path);
        tokio::fs::write(wt.join("wip.txt"), "work in progress\n")
            .await
            .unwrap();

        let renamed = fx
            .manager
            .rename_worktree(&active, "fix tui crash")
            .await
            .unwrap();

        // Directory moved under the store's worktrees dir.
        assert_eq!(
            renamed.worktree_path,
            fx.manager
                .worktrees_dir(&active.project.name)
                .join("fix-tui-crash")
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        );
        assert!(Path::new(&renamed.worktree_path).is_dir());
        assert!(
            !Path::new(&active.worktree_path).exists(),
            "old worktree dir should be gone"
        );
        assert_eq!(
            renamed.branch, "omega/fix-tui-crash",
            "branch should be renamed to omega/<name>"
        );

        // The moved worktree is still a git checkout on the renamed branch,
        // with the uncommitted file intact.
        let new_wt = Path::new(&renamed.worktree_path);
        assert_eq!(
            git(new_wt, &["rev-parse", "--is-inside-work-tree"])
                .await
                .unwrap(),
            "true"
        );
        assert_eq!(
            git(new_wt, &["branch", "--show-current"]).await.unwrap(),
            renamed.branch
        );
        assert_eq!(
            tokio::fs::read_to_string(new_wt.join("wip.txt"))
                .await
                .unwrap(),
            "work in progress\n"
        );

        // Git's worktree bookkeeping points at the new path only.
        let list = git(&repo, &["worktree", "list"]).await.unwrap();
        assert!(list.contains(&renamed.worktree_path), "{list}");
        assert!(!list.contains(&active.worktree_path), "{list}");
        assert_eq!(count_lines(&list), 2, "bare repo + 1 moved worktree");

        // The old branch no longer exists; the new one does.
        let old_branch = git(&repo, &["branch", "--list", &active.branch])
            .await
            .unwrap();
        assert!(old_branch.is_empty(), "old branch should be deleted");
        let new_branch = git(&repo, &["branch", "--list", "omega/fix-tui-crash"])
            .await
            .unwrap();
        assert!(new_branch.contains("omega/fix-tui-crash"));
    }

    #[tokio::test]
    async fn rename_worktree_preserves_commit_history() {
        let fx = fixture().await;
        let active = fx
            .manager
            .activate(&fx.remote_url, "sess-1", None)
            .await
            .unwrap();
        configure_worktree_identity(&fx.manager, &active.project).await;

        let wt = Path::new(&active.worktree_path);
        tokio::fs::write(wt.join("feature.txt"), "feature\n")
            .await
            .unwrap();
        git(wt, &["add", "."]).await.unwrap();
        git(wt, &["commit", "-m", "add feature"]).await.unwrap();

        let renamed = fx
            .manager
            .rename_worktree(&active, "add-feature")
            .await
            .unwrap();

        // Commits live on the renamed branch, reachable from the new name.
        let log = git(Path::new(&renamed.worktree_path), &["log", "--oneline"])
            .await
            .unwrap();
        assert!(log.contains("add feature"), "{log}");
        assert_eq!(
            git(
                Path::new(&renamed.worktree_path),
                &["log", "--oneline", "-1"]
            )
            .await
            .unwrap(),
            git(
                &fx.manager.repo_dir(&active.project.name),
                &["log", "--oneline", "-1", &renamed.branch]
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn rename_worktree_rejects_empty_and_duplicate_names() {
        let fx = fixture().await;
        let active = fx
            .manager
            .activate(&fx.remote_url, "sess-1", None)
            .await
            .unwrap();

        // Empty name → rejected before touching git.
        let err = fx
            .manager
            .rename_worktree(&active, "   ")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("must not be empty"),
            "empty name error: {err:#}"
        );

        // Renaming to a name already taken by another worktree → rejected.
        let other = fx
            .manager
            .activate(&fx.remote_url, "sess-2", None)
            .await
            .unwrap();
        let taken = fx
            .manager
            .rename_worktree(&other, "taken-name")
            .await
            .unwrap();
        let err = fx
            .manager
            .rename_worktree(&active, "taken-name")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "duplicate name error: {err:#}"
        );
        // The first worktree is untouched by the failed rename.
        assert!(Path::new(&taken.worktree_path).is_dir());
        assert_eq!(
            git(
                &Path::new(&taken.worktree_path),
                &["branch", "--show-current"]
            )
            .await
            .unwrap(),
            taken.branch
        );
    }

    #[tokio::test]
    async fn rename_worktree_same_name_is_a_noop() {
        let fx = fixture().await;
        let active = fx
            .manager
            .activate(&fx.remote_url, "sess-1", None)
            .await
            .unwrap();
        let repo = fx.manager.repo_dir(&active.project.name);

        // Re-naming to the current directory/branch name must succeed and
        // change nothing.
        let current_dir = Path::new(&active.worktree_path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let renamed = fx
            .manager
            .rename_worktree(&active, &current_dir)
            .await
            .unwrap();
        assert_eq!(renamed.worktree_path, active.worktree_path);
        assert_eq!(renamed.branch, active.branch);
        let list = git(&repo, &["worktree", "list"]).await.unwrap();
        assert_eq!(count_lines(&list), 2);
    }

    // -----------------------------------------------------------------------
    // Worktree binding reconciliation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn reconcile_worktree_finds_moved_dir_by_branch() {
        let fx = fixture().await;
        let active = fx
            .manager
            .activate(&fx.remote_url, "sess-1", None)
            .await
            .unwrap();

        // Directory moved, branch kept (what `git worktree move` does).
        let old_dir = Path::new(&active.worktree_path);
        let new_dir = old_dir.parent().unwrap().join("moved-elsewhere");
        let repo = fx.manager.repo_dir(&active.project.name);
        git(
            &repo,
            &[
                "worktree",
                "move",
                "--force",
                &active.worktree_path,
                &new_dir.display().to_string(),
            ],
        )
        .await
        .unwrap();
        assert!(!old_dir.exists());

        let reconciled = fx
            .manager
            .reconcile_worktree(&active)
            .await
            .expect("worktree found via its branch");
        assert_eq!(reconciled.branch, active.branch);
        assert_eq!(reconciled.worktree_path, new_dir.display().to_string());
        assert!(Path::new(&reconciled.worktree_path).is_dir());
    }

    #[tokio::test]
    async fn reconcile_worktree_returns_none_when_dir_and_branch_gone() {
        let fx = fixture().await;
        let active = fx
            .manager
            .activate(&fx.remote_url, "sess-1", None)
            .await
            .unwrap();

        // Neither the persisted path nor the persisted branch exists — the
        // binding is unrecoverable, so the caller must re-create a worktree.
        let missing = ActiveProject {
            project: active.project.clone(),
            worktree_path: "/nonexistent/vanished".to_string(),
            branch: "omega/never-existed".to_string(),
        };
        assert!(fx.manager.reconcile_worktree(&missing).await.is_none());
    }

    #[tokio::test]
    async fn reconcile_worktree_is_noop_when_binding_is_current() {
        let fx = fixture().await;
        let active = fx
            .manager
            .activate(&fx.remote_url, "sess-1", None)
            .await
            .unwrap();

        let reconciled = fx
            .manager
            .reconcile_worktree(&active)
            .await
            .expect("worktree exists");
        assert_eq!(reconciled.worktree_path, active.worktree_path);
        assert_eq!(reconciled.branch, active.branch);
    }

    #[tokio::test]
    async fn reconcile_worktree_rebinds_after_external_branch_rename() {
        let fx = fixture().await;
        let active = fx
            .manager
            .activate(&fx.remote_url, "sess-1", None)
            .await
            .unwrap();

        // Branch renamed in place, directory untouched (raw `git branch -m`
        // run from inside the worktree). The persisted binding still names
        // the old branch.
        let repo = fx.manager.repo_dir(&active.project.name);
        git(
            &repo,
            &["branch", "-m", &active.branch, "omega/renamed-in-place"],
        )
        .await
        .unwrap();

        let reconciled = fx
            .manager
            .reconcile_worktree(&active)
            .await
            .expect("worktree still exists at persisted path");
        assert_eq!(reconciled.branch, "omega/renamed-in-place");
        assert_eq!(reconciled.worktree_path, active.worktree_path);
    }

    #[tokio::test]
    async fn reconcile_worktree_returns_none_when_full_rename_unrecoverable() {
        let fx = fixture().await;
        let active = fx
            .manager
            .activate(&fx.remote_url, "sess-1", None)
            .await
            .unwrap();

        // Renamed *outside* the store with no trace left: directory moved AND
        // branch renamed (raw `git branch -m` + `git worktree move`). Nothing
        // links the persisted binding to the new checkout — the daemon must
        // create a fresh worktree for the session.
        let old_dir = Path::new(&active.worktree_path);
        let new_dir = old_dir.parent().unwrap().join("renamed-outside");
        let repo = fx.manager.repo_dir(&active.project.name);
        git(
            &repo,
            &[
                "worktree",
                "move",
                "--force",
                &active.worktree_path,
                &new_dir.display().to_string(),
            ],
        )
        .await
        .unwrap();
        git(
            &repo,
            &["branch", "-m", &active.branch, "omega/renamed-outside"],
        )
        .await
        .unwrap();
        assert!(!old_dir.exists());

        assert!(
            fx.manager.reconcile_worktree(&active).await.is_none(),
            "no trace left to reconcile against"
        );
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

    // -----------------------------------------------------------------------
    // Upstream sync (update_default_branch / ensure_main_worktree)
    // -----------------------------------------------------------------------

    /// Push a new commit to the *remote* (upstream) main.
    async fn advance_remote(remote: &Path) {
        let stamp = uuid::Uuid::new_v4().simple().to_string();
        tokio::fs::write(remote.join(format!("remote-{stamp}.txt")), "upstream\n")
            .await
            .unwrap();
        git(remote, &["add", "."]).await.unwrap();
        git(remote, &["commit", "-m", &format!("upstream advance {stamp}")])
            .await
            .unwrap();
    }

    /// Create a *local-only* commit on the project's main branch (through its
    /// main worktree — the only place `refs/heads/main` can be written
    /// locally). Returns the new sha.
    async fn local_only_commit(manager: &ProjectManager, info: &ProjectInfo) -> String {
        let main = manager.ensure_main_worktree(info).await.unwrap();
        configure_worktree_identity(manager, info).await;
        let dir = Path::new(&main.worktree_path);
        let stamp = uuid::Uuid::new_v4().simple().to_string();
        tokio::fs::write(dir.join(format!("local-{stamp}.txt")), "local-only\n")
            .await
            .unwrap();
        git(dir, &["add", "."]).await.unwrap();
        git(
            dir,
            &["commit", "-m", &format!("local-only {stamp}")],
        )
        .await
        .unwrap();
        git(dir, &["rev-parse", "HEAD"]).await.unwrap()
    }

    #[tokio::test]
    async fn update_default_branch_fast_forwards_when_strictly_behind() {
        let fx = fixture().await;
        let info = fx
            .manager
            .resolve(&fx.remote_url)
            .await
            .unwrap();
        let repo = fx.manager.repo_dir(&info.name);

        // Upstream advances; local main is an ancestor → mechanical FF.
        advance_remote(fx._remote.path()).await;
        let status = fx
            .manager
            .update_default_branch(&info)
            .await
            .unwrap()
            .expect("a status");
        match status {
            DefaultBranchStatus::FastForwarded(old, new) => {
                assert_ne!(old, new, "fast-forward should move the ref");
                let local = git(&repo, &["rev-parse", "refs/heads/main"])
                    .await
                    .unwrap();
                assert_eq!(local, new);
                let upstream = git(&repo, &["rev-parse", "refs/remotes/origin/main"])
                    .await
                    .unwrap();
                assert_eq!(local, upstream, "main should now equal upstream");
            }
            other => panic!("expected FastForwarded, got {other:?}"),
        }

        // A fully-synced branch stays UpToDate.
        let status = fx
            .manager
            .update_default_branch(&info)
            .await
            .unwrap()
            .expect("a status");
        assert!(matches!(status, DefaultBranchStatus::UpToDate));
    }

    #[tokio::test]
    async fn update_default_branch_detects_diverged_and_never_drops_local_commits() {
        let fx = fixture().await;
        let info = fx
            .manager
            .resolve(&fx.remote_url)
            .await
            .unwrap();
        let repo = fx.manager.repo_dir(&info.name);

        // Local-only commit first (omega-added functionality upstream lacks).
        let local_sha = local_only_commit(&fx.manager, &info).await;
        // Then upstream advances.
        advance_remote(fx._remote.path()).await;

        let status = fx
            .manager
            .update_default_branch(&info)
            .await
            .unwrap()
            .expect("a status");
        match status {
            DefaultBranchStatus::Diverged {
                ahead, behind, ..
            } => {
                assert_eq!(ahead, 1, "one local-only commit");
                assert_eq!(behind, 1, "one upstream commit");
            }
            other => panic!("expected Diverged, got {other:?}"),
        }

        // The divergence must NOT have been resolved mechanically: the local
        // commit is still on main.
        assert!(is_ancestor(&repo, &local_sha, "refs/heads/main").await);
    }

    #[tokio::test]
    async fn update_default_branch_reports_ahead_only() {
        let fx = fixture().await;
        let info = fx
            .manager
            .resolve(&fx.remote_url)
            .await
            .unwrap();

        // Local-only commit, upstream unchanged.
        local_only_commit(&fx.manager, &info).await;
        let status = fx
            .manager
            .update_default_branch(&info)
            .await
            .unwrap()
            .expect("a status");
        assert!(matches!(status, DefaultBranchStatus::AheadOnly));
    }

    #[tokio::test]
    async fn ensure_main_worktree_creates_once_and_reconciles() {
        let fx = fixture().await;
        let info = fx
            .manager
            .resolve(&fx.remote_url)
            .await
            .unwrap();

        let first = fx.manager.ensure_main_worktree(&info).await.unwrap();
        assert_eq!(first.branch, "main");
        let dir = Path::new(&first.worktree_path);
        assert!(dir.is_dir(), "main worktree dir should exist");
        assert_eq!(
            git(dir, &["branch", "--show-current"]).await.unwrap(),
            "main",
            "main worktree checks out the default branch"
        );

        // Idempotent: calling again reuses the same checkout instead of
        // creating a second one.
        let second = fx.manager.ensure_main_worktree(&info).await.unwrap();
        assert_eq!(first.worktree_path, second.worktree_path);
        let count = git(&fx.manager.repo_dir(&info.name), &["worktree", "list"])
            .await
            .unwrap()
            .lines()
            .count();
        assert_eq!(count, 2, "bare repo + one main worktree");
    }

}
