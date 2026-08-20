//! Safe pushes of a project's local soft-fork branch to a distinct GitHub
//! mirror. The authoritative `upstream` remote is never a push target.
//!
//! Configuration and status are non-secret JSON in the project store. GitHub
//! tokens live in separate, mode-0600 files (or a systemd credential supplied
//! by `OMEGA_GITHUB_TOKEN_FILE`) and are read by a short-lived askpass helper.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use uuid::Uuid;

use crate::{ProjectInfo, ProjectManager, MIRROR_REMOTE};

pub const STATE_FILE: &str = "github-mirrors.json";
const LOCK_FILE: &str = "github-mirrors.lock";
const DEFAULT_CREDENTIALS_DIR: &str = "mirror-credentials";
const MAX_TOKEN_BYTES: u64 = 4096;
/// Each authenticated Git subprocess is killed if GitHub (or its network)
/// stops responding. A push holds the state lock across these bounded calls
/// so another process cannot race the observed SHA/lease decision.
const NETWORK_GIT_TIMEOUT: Duration = Duration::from_secs(60);
const STALE_LOCK_AGE: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MirrorState {
    #[serde(default)]
    pub projects: BTreeMap<String, MirrorProject>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MirrorProject {
    /// Canonical `https://github.com/<owner>/<repo>.git` URL. Never contains
    /// credentials.
    pub url: String,
    #[serde(default)]
    pub auto_push: bool,
    /// The remote tip observed after our last successful push. Every rewrite
    /// is leased against this exact object id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_remote_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_local_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_remote_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<MirrorPushResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MirrorPushResult {
    /// `pushed`, `up-to-date`, `rejected`, or `failed`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl MirrorProject {
    fn new(url: String, auto_push: bool) -> Self {
        Self {
            url,
            auto_push,
            expected_remote_sha: None,
            last_attempt_at: None,
            last_success_at: None,
            last_local_sha: None,
            last_remote_sha: None,
            result: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenUpdate<'a> {
    /// A blank token form means “leave the current per-project token alone”.
    Unchanged,
    Set(&'a str),
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOutcome {
    pub status: String,
    pub local_sha: String,
    pub remote_sha: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MirrorManager {
    projects: ProjectManager,
    credentials_dir: PathBuf,
    default_token_file: Option<PathBuf>,
}

impl MirrorManager {
    pub fn from_env(projects: ProjectManager) -> Self {
        let credentials_dir = std::env::var("OMEGA_MIRROR_CREDENTIALS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| projects.root().join(DEFAULT_CREDENTIALS_DIR));
        let default_token_file = std::env::var("OMEGA_GITHUB_TOKEN_FILE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from);
        Self::new(projects, credentials_dir, default_token_file)
    }

    pub fn new(
        projects: ProjectManager,
        credentials_dir: impl Into<PathBuf>,
        default_token_file: Option<PathBuf>,
    ) -> Self {
        Self {
            projects,
            credentials_dir: credentials_dir.into(),
            default_token_file,
        }
    }

    pub fn load_state(&self) -> MirrorState {
        load_state(self.projects.root())
    }

    pub fn project(&self, name: &str) -> Option<MirrorProject> {
        self.load_state().projects.get(name).cloned()
    }

    pub fn has_project_token(&self, name: &str) -> bool {
        self.project_token_path(name).is_file()
    }

    pub fn has_any_token(&self, name: &str) -> bool {
        self.has_project_token(name)
            || self
                .default_token_file
                .as_deref()
                .is_some_and(Path::is_file)
    }

    /// Configure the canonical GitHub URL and the dedicated `mirror` remote.
    /// `input` may be `owner/repo` or the canonical HTTPS URL.
    pub async fn configure(
        &self,
        info: &ProjectInfo,
        input: &str,
        auto_push: bool,
        token: TokenUpdate<'_>,
    ) -> Result<MirrorProject> {
        self.require_exact_project(info).await?;
        let url = canonical_github_url(input)?;
        let branch = self
            .projects
            .default_branch(info)
            .await
            .ok_or_else(|| anyhow::anyhow!("project has no configured default branch"))?;
        validate_branch(&branch)?;
        // Serialize remote URL, credential, lease state, and push operations
        // as one unit. In particular, configuration cannot retarget
        // `mirror` while another process is pushing it.
        let _lock = StoreLock::acquire(self.projects.root())?;
        self.ensure_remote(info, &url).await?;
        self.apply_token_update(&info.name, token)?;

        let mut state = load_state(self.projects.root());
        let entry = state
            .projects
            .entry(info.name.clone())
            .or_insert_with(|| MirrorProject::new(url.clone(), auto_push));
        if entry.url != url {
            // A different GitHub repository has no relationship to the old
            // lease; bootstrap it conservatively on the first push.
            entry.url = url;
            entry.expected_remote_sha = None;
            entry.last_remote_sha = None;
            entry.result = None;
        }
        entry.auto_push = auto_push;
        let configured = entry.clone();
        save_state(self.projects.root(), &state)?;
        Ok(configured)
    }

    pub async fn remove(&self, info: &ProjectInfo, remove_token: bool) -> Result<()> {
        self.require_exact_project(info).await?;
        let _lock = StoreLock::acquire(self.projects.root())?;
        let mut state = load_state(self.projects.root());
        state.projects.remove(&info.name);
        save_state(self.projects.root(), &state)?;
        if remove_token {
            self.remove_project_token(&info.name)?;
        }
        let repo = self.projects.repo_dir(&info.name);
        let _ = git_output(&repo, &["remote", "remove", MIRROR_REMOTE]).await;
        Ok(())
    }

    pub fn remove_project_token(&self, name: &str) -> Result<()> {
        let path = self.project_token_path(name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| "could not remove the project mirror token"),
        }
    }

    pub async fn auto_push(&self, info: &ProjectInfo) -> Option<Result<PushOutcome>> {
        if self
            .project(&info.name)
            .is_some_and(|config| config.auto_push)
        {
            Some(self.push(info).await)
        } else {
            None
        }
    }

    /// Push the configured local default branch to the same branch on the
    /// dedicated mirror. Rewrites are protected by an exact expected SHA;
    /// no blind force is ever used.
    pub async fn push(&self, info: &ProjectInfo) -> Result<PushOutcome> {
        self.push_inner(info, false).await
    }

    async fn push_inner(
        &self,
        info: &ProjectInfo,
        allow_local_test_url: bool,
    ) -> Result<PushOutcome> {
        self.require_exact_project(info).await?;
        let _lock = StoreLock::acquire(self.projects.root())?;
        let mut state = load_state(self.projects.root());
        let Some(mut config) = state.projects.get(&info.name).cloned() else {
            bail!("GitHub mirror is not configured for this project");
        };
        if !allow_local_test_url {
            let canonical = canonical_github_url(&config.url)?;
            if canonical != config.url {
                bail!("stored mirror URL is not canonical");
            }
        }
        let branch = self
            .projects
            .default_branch(info)
            .await
            .ok_or_else(|| anyhow::anyhow!("project has no configured default branch"))?;
        validate_branch(&branch)?;
        self.ensure_remote_unchecked(info, &config.url).await?;
        let token_file = match self.token_file(&info.name) {
            Ok(path) => path,
            Err(_) => {
                config.last_attempt_at = Some(Utc::now());
                config.result = Some(result(
                    "failed",
                    "no usable GitHub token is configured for this project",
                ));
                state.projects.insert(info.name.clone(), config);
                save_state(self.projects.root(), &state)?;
                bail!("no usable GitHub token is configured for this project");
            }
        };
        let askpass = AskPass::create(&self.credentials_dir, &token_file)?;
        let repo = self.projects.repo_dir(&info.name);
        let local_ref = format!("refs/heads/{branch}");
        let local = git_stdout(&repo, &["rev-parse", &local_ref]).await?;
        if !valid_object_id(&local) {
            bail!("local default branch did not resolve to an object id");
        }

        config.last_attempt_at = Some(Utc::now());
        config.last_local_sha = Some(local.clone());
        let observed = match ls_remote(&repo, &branch, &askpass).await {
            Ok(sha) => sha,
            Err(_) => {
                config.result = Some(result("failed", "could not read the GitHub mirror branch"));
                state.projects.insert(info.name.clone(), config);
                save_state(self.projects.root(), &state)?;
                bail!("could not read the GitHub mirror branch");
            }
        };
        config.last_remote_sha = observed.clone();

        if let Some(expected) = config.expected_remote_sha.as_deref() {
            if observed.as_deref() != Some(expected) {
                config.result = Some(result(
                    "rejected",
                    "the mirror branch moved since omega last pushed; refusing to overwrite it",
                ));
                state.projects.insert(info.name.clone(), config);
                save_state(self.projects.root(), &state)?;
                bail!("mirror branch moved unexpectedly; push refused");
            }
        } else if let Some(remote) = observed.as_deref() {
            // First push to a non-empty mirror is safe only when that tip is
            // already contained in local history. An unrelated repository is
            // never overwritten merely because it was configured in a form.
            let fetched = fetch_remote_tip(&repo, &branch, &askpass).await;
            if fetched.as_deref() != Some(remote)
                || !git_success(&repo, &["merge-base", "--is-ancestor", remote, &local]).await
            {
                config.result = Some(result(
                    "rejected",
                    "the existing mirror branch is not an ancestor of local; refusing the initial overwrite",
                ));
                state.projects.insert(info.name.clone(), config);
                save_state(self.projects.root(), &state)?;
                bail!("existing mirror history is unrelated or newer; initial push refused");
            }
        }

        if observed.as_deref() == Some(&local) {
            config.expected_remote_sha = Some(local.clone());
            config.last_success_at = Some(Utc::now());
            config.result = Some(MirrorPushResult {
                status: "up-to-date".into(),
                detail: None,
            });
            state.projects.insert(info.name.clone(), config);
            save_state(self.projects.root(), &state)?;
            return Ok(PushOutcome {
                status: "up-to-date".into(),
                local_sha: local,
                remote_sha: observed,
            });
        }

        let lease = format!(
            "--force-with-lease=refs/heads/{branch}:{}",
            observed.as_deref().unwrap_or("")
        );
        let refspec = format!("{local_ref}:refs/heads/{branch}");
        if !git_authenticated_success(
            &repo,
            &["push", "--porcelain", &lease, MIRROR_REMOTE, &refspec],
            &askpass,
        )
        .await
        {
            config.result = Some(result(
                "rejected",
                "GitHub rejected the leased push; the branch may have moved",
            ));
            state.projects.insert(info.name.clone(), config);
            save_state(self.projects.root(), &state)?;
            bail!("GitHub rejected the leased mirror push");
        }
        let verified = ls_remote(&repo, &branch, &askpass).await.ok().flatten();
        if verified.as_deref() != Some(&local) {
            config.last_remote_sha = verified;
            config.result = Some(result(
                "failed",
                "push returned successfully but the mirror tip could not be verified",
            ));
            state.projects.insert(info.name.clone(), config);
            save_state(self.projects.root(), &state)?;
            bail!("mirror tip verification failed after push");
        }
        config.expected_remote_sha = Some(local.clone());
        config.last_remote_sha = Some(local.clone());
        config.last_success_at = Some(Utc::now());
        config.result = Some(MirrorPushResult {
            status: "pushed".into(),
            detail: None,
        });
        state.projects.insert(info.name.clone(), config);
        save_state(self.projects.root(), &state)?;
        Ok(PushOutcome {
            status: "pushed".into(),
            local_sha: local.clone(),
            remote_sha: Some(local),
        })
    }

    async fn require_exact_project(&self, info: &ProjectInfo) -> Result<()> {
        let Some(stored) = self.projects.find(&info.name).await? else {
            bail!("project is not registered");
        };
        if stored != *info {
            bail!("project registry changed; reload before configuring its mirror");
        }
        if !self.projects.repo_dir(&info.name).join("HEAD").is_file() {
            bail!("project repository is missing");
        }
        Ok(())
    }

    async fn ensure_remote(&self, info: &ProjectInfo, url: &str) -> Result<()> {
        let canonical = canonical_github_url(url)?;
        self.ensure_remote_unchecked(info, &canonical).await
    }

    async fn ensure_remote_unchecked(&self, info: &ProjectInfo, url: &str) -> Result<()> {
        let repo = self.projects.repo_dir(&info.name);
        if git_success(&repo, &["remote", "get-url", MIRROR_REMOTE]).await {
            git_checked(&repo, &["remote", "set-url", MIRROR_REMOTE, url]).await?;
        } else {
            git_checked(&repo, &["remote", "add", MIRROR_REMOTE, url]).await?;
        }
        // A stale pushurl could silently target something other than the URL
        // displayed in the UI. Remove all of them; with none, Git uses url.
        let _ = git_output(
            &repo,
            &[
                "config",
                "--unset-all",
                &format!("remote.{MIRROR_REMOTE}.pushurl"),
            ],
        )
        .await;
        let push_urls = git_stdout(
            &repo,
            &["remote", "get-url", "--push", "--all", MIRROR_REMOTE],
        )
        .await?;
        if push_urls.lines().collect::<Vec<_>>() != [url] {
            bail!("mirror remote did not resolve to the configured URL");
        }
        Ok(())
    }

    fn apply_token_update(&self, name: &str, update: TokenUpdate<'_>) -> Result<()> {
        match update {
            TokenUpdate::Unchanged => Ok(()),
            TokenUpdate::Remove => self.remove_project_token(name),
            TokenUpdate::Set(token) => self.write_project_token(name, token),
        }
    }

    fn write_project_token(&self, name: &str, token: &str) -> Result<()> {
        validate_token(token)?;
        secure_dir(&self.credentials_dir)?;
        let path = self.project_token_path(name);
        let temp = self
            .credentials_dir
            .join(format!(".{name}.{}.tmp", Uuid::new_v4().simple()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .context("could not create temporary mirror credential")?;
        let write = (|| -> Result<()> {
            file.write_all(token.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temp, &path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            Ok(())
        })();
        if write.is_err() {
            let _ = fs::remove_file(&temp);
        }
        write.context("could not save mirror credential")
    }

    fn token_file(&self, name: &str) -> Result<PathBuf> {
        let project = self.project_token_path(name);
        let path = if project.is_file() {
            project
        } else if let Some(default) = self.default_token_file.as_deref().filter(|p| p.is_file()) {
            default.to_path_buf()
        } else {
            bail!("no GitHub token is configured for this project");
        };
        validate_token_file(&path)?;
        Ok(path)
    }

    fn project_token_path(&self, name: &str) -> PathBuf {
        // Registered project names are sanitized to this alphabet. Keeping
        // the filename derivation defensive prevents traversal if this
        // helper is accidentally called before registry validation.
        let safe: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect();
        self.credentials_dir.join(format!("{safe}.token"))
    }

    #[cfg(test)]
    async fn configure_local_for_test(
        &self,
        info: &ProjectInfo,
        url: &str,
        auto_push: bool,
        token: &str,
    ) -> Result<()> {
        self.require_exact_project(info).await?;
        self.ensure_remote_unchecked(info, url).await?;
        self.write_project_token(&info.name, token)?;
        let _lock = StoreLock::acquire(self.projects.root())?;
        let mut state = load_state(self.projects.root());
        state.projects.insert(
            info.name.clone(),
            MirrorProject::new(url.to_string(), auto_push),
        );
        save_state(self.projects.root(), &state)
    }
}

pub fn canonical_github_url(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
        bail!("mirror must be owner/repo or a GitHub HTTPS URL");
    }
    let path = if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        rest
    } else if !trimmed.contains("://") {
        trimmed
    } else {
        bail!("mirror URL must use https://github.com");
    };
    if path.contains(['?', '#', '@', '\\']) || path.starts_with('/') || path.ends_with('/') {
        bail!("invalid GitHub repository path");
    }
    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or("");
    let raw_repo = parts.next().unwrap_or("");
    if parts.next().is_some() {
        bail!("GitHub mirror must name exactly one owner and repository");
    }
    let repo = raw_repo.strip_suffix(".git").unwrap_or(raw_repo);
    if owner.is_empty()
        || owner.len() > 39
        || owner.starts_with('-')
        || owner.ends_with('-')
        || !owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        bail!("invalid GitHub owner");
    }
    if repo.is_empty()
        || repo.len() > 100
        || matches!(repo, "." | "..")
        || !repo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("invalid GitHub repository name");
    }
    Ok(format!("https://github.com/{owner}/{repo}.git"))
}

fn validate_branch(branch: &str) -> Result<()> {
    if branch.is_empty()
        || branch.len() > 255
        || branch.starts_with(['-', '/', '.'])
        || branch.ends_with(['/', '.'])
        || branch.contains("..")
        || branch.contains("@{")
        || branch.contains("//")
        || branch.split('/').any(|part| part.ends_with(".lock"))
        || !branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    {
        bail!("configured default branch is not safe to push");
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<()> {
    if token.is_empty()
        || token.len() as u64 > MAX_TOKEN_BYTES
        || token.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        bail!("GitHub token must be a non-empty single token of at most 4096 bytes");
    }
    Ok(())
}

fn validate_token_file(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path).context("could not inspect GitHub credential")?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_TOKEN_BYTES + 1 {
        bail!("GitHub credential file is empty, oversized, or not a regular file");
    }
    let token = fs::read_to_string(path).context("could not read GitHub credential")?;
    validate_token(token.trim_end_matches(['\r', '\n']))
}

fn secure_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).context("could not create mirror credentials directory")?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .context("could not restrict mirror credentials directory")
}

fn load_state(root: &Path) -> MirrorState {
    fs::read(root.join(STATE_FILE))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

fn save_state(root: &Path, state: &MirrorState) -> Result<()> {
    fs::create_dir_all(root)?;
    let path = root.join(STATE_FILE);
    let temp = root.join(format!(".{STATE_FILE}.{}.tmp", Uuid::new_v4().simple()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| -> Result<()> {
        serde_json::to_writer_pretty(&mut file, state)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result.context("could not save GitHub mirror state")
}

struct StoreLock(PathBuf);

impl StoreLock {
    fn acquire(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let path = root.join(LOCK_FILE);
        let started = Instant::now();
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self(path));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_owner_is_dead(&path) || lock_is_stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if started.elapsed() >= Duration::from_secs(10) {
                        bail!("timed out waiting for GitHub mirror state lock");
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(e).context("could not acquire GitHub mirror state lock"),
            }
        }
    }
}

fn lock_owner_is_dead(path: &Path) -> bool {
    let Some(pid) = fs::read_to_string(path)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
    else {
        return false;
    };
    !Path::new("/proc").join(pid.to_string()).exists()
}

fn lock_is_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= STALE_LOCK_AGE)
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct AskPass {
    script: PathBuf,
    token_file: PathBuf,
}

impl AskPass {
    fn create(credentials_dir: &Path, token_file: &Path) -> Result<Self> {
        secure_dir(credentials_dir)?;
        let script = credentials_dir.join(format!(".askpass-{}", Uuid::new_v4().simple()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&script)
            .context("could not create temporary Git askpass helper")?;
        file.write_all(
            b"#!/bin/sh\ncase \"$1\" in\n  *sername*) printf '%s\\n' x-access-token ;;\n  *assword*) IFS= read -r token < \"$OMEGA_MIRROR_TOKEN_FILE\"; printf '%s\\n' \"$token\"; unset token ;;\n  *) exit 1 ;;\nesac\n",
        )?;
        file.sync_all()?;
        Ok(Self {
            script,
            token_file: token_file.to_path_buf(),
        })
    }

    fn command(&self, repo: &Path, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_ASKPASS", &self.script)
            .env("SSH_ASKPASS", &self.script)
            .env("OMEGA_MIRROR_TOKEN_FILE", &self.token_file)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "Never")
            // An empty helper entry suppresses helpers inherited from global
            // config, so this operation uses only our short-lived askpass.
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "credential.helper")
            .env("GIT_CONFIG_VALUE_0", "")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }
}

impl Drop for AskPass {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.script);
    }
}

fn result(status: &str, detail: &str) -> MirrorPushResult {
    MirrorPushResult {
        status: status.into(),
        detail: Some(detail.into()),
    }
}

fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

async fn ls_remote(repo: &Path, branch: &str, askpass: &AskPass) -> Result<Option<String>> {
    let remote_ref = format!("refs/heads/{branch}");
    let output = authenticated_output(
        askpass,
        repo,
        &["ls-remote", "--refs", MIRROR_REMOTE, &remote_ref],
    )
    .await?;
    if !output.status.success() {
        bail!("git ls-remote failed");
    }
    let stdout = String::from_utf8(output.stdout).context("git ls-remote returned non-UTF-8")?;
    let mut lines = stdout.lines().filter(|line| !line.trim().is_empty());
    let Some(line) = lines.next() else {
        return Ok(None);
    };
    if lines.next().is_some() {
        bail!("GitHub returned more than one exact mirror branch");
    }
    let Some((sha, found_ref)) = line.split_once('\t') else {
        bail!("GitHub returned a malformed branch record");
    };
    if found_ref != remote_ref || !valid_object_id(sha) {
        bail!("GitHub returned an invalid mirror branch record");
    }
    Ok(Some(sha.to_string()))
}

async fn fetch_remote_tip(repo: &Path, branch: &str, askpass: &AskPass) -> Option<String> {
    let remote_ref = format!("refs/heads/{branch}");
    let output = authenticated_output(
        askpass,
        repo,
        &["fetch", "--no-tags", MIRROR_REMOTE, &remote_ref],
    )
    .await
    .ok()?;
    if !output.status.success() {
        return None;
    }
    git_stdout(repo, &["rev-parse", "FETCH_HEAD"]).await.ok()
}

async fn git_authenticated_success(repo: &Path, args: &[&str], askpass: &AskPass) -> bool {
    authenticated_output(askpass, repo, args)
        .await
        .is_ok_and(|output| output.status.success())
}

async fn authenticated_output(
    askpass: &AskPass,
    repo: &Path,
    args: &[&str],
) -> Result<std::process::Output> {
    let mut command = askpass.command(repo, args);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    tokio::time::timeout(NETWORK_GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| anyhow::anyhow!("authenticated Git operation timed out"))?
        .context("could not start authenticated Git operation")
}

async fn git_output(repo: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
}

async fn git_checked(repo: &Path, args: &[&str]) -> Result<()> {
    let output = git_output(repo, args).await?;
    if !output.status.success() {
        bail!("git operation failed while configuring the mirror remote");
    }
    Ok(())
}

async fn git_stdout(repo: &Path, args: &[&str]) -> Result<String> {
    let output = git_output(repo, args).await?;
    if !output.status.success() {
        bail!("git operation failed while inspecting the mirror repository");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn git_success(repo: &Path, args: &[&str]) -> bool {
    git_output(repo, args)
        .await
        .is_ok_and(|output| output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
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
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    async fn commit(repo: &Path, name: &str, body: &str) -> String {
        fs::write(repo.join(name), body).unwrap();
        git(repo, &["add", name]).await;
        git(repo, &["commit", "-m", name]).await;
        git(repo, &["rev-parse", "HEAD"]).await
    }

    async fn fixture() -> (TempDir, TempDir, TempDir, ProjectManager, ProjectInfo) {
        let upstream = TempDir::new().unwrap();
        git(upstream.path(), &["init", "-b", "main"]).await;
        git(upstream.path(), &["config", "user.name", "Mirror Test"]).await;
        git(
            upstream.path(),
            &["config", "user.email", "mirror@example.test"],
        )
        .await;
        git(upstream.path(), &["config", "commit.gpgsign", "false"]).await;
        commit(upstream.path(), "initial.txt", "initial\n").await;

        let store = TempDir::new().unwrap();
        let credentials = TempDir::new().unwrap();
        let manager = ProjectManager::with_root(store.path());
        let info = manager
            .create(Some("demo"), &upstream.path().display().to_string())
            .await
            .unwrap();
        let repo = manager.repo_dir(&info.name);
        git(&repo, &["config", "user.name", "Mirror Test"]).await;
        git(&repo, &["config", "user.email", "mirror@example.test"]).await;
        git(&repo, &["config", "commit.gpgsign", "false"]).await;
        (upstream, store, credentials, manager, info)
    }

    #[test]
    fn github_url_validation_is_strict_and_canonical() {
        assert_eq!(
            canonical_github_url("octo/example").unwrap(),
            "https://github.com/octo/example.git"
        );
        assert_eq!(
            canonical_github_url("https://github.com/octo/example.git").unwrap(),
            "https://github.com/octo/example.git"
        );
        for bad in [
            "http://github.com/o/r",
            "https://evil.example/o/r",
            "https://github.com/o/r/extra",
            "https://token@github.com/o/r",
            "o/../r",
            "o/r?x=1",
        ] {
            assert!(canonical_github_url(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn dead_process_lock_is_reclaimed() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join(LOCK_FILE), "4294967295\n").unwrap();
        let lock = StoreLock::acquire(root.path()).unwrap();
        assert!(root.path().join(LOCK_FILE).is_file());
        drop(lock);
        assert!(!root.path().join(LOCK_FILE).exists());
    }

    #[tokio::test]
    async fn pushes_only_mirror_handles_rewrite_and_rejects_movement() {
        let (upstream, _store, credentials, manager, info) = fixture().await;
        let upstream_before = git(upstream.path(), &["rev-parse", "main"]).await;
        let mirror_dir = TempDir::new().unwrap();
        git(mirror_dir.path(), &["init", "--bare", "-b", "main"]).await;
        let mirrors = MirrorManager::new(manager.clone(), credentials.path(), None);
        mirrors
            .configure_local_for_test(
                &info,
                &mirror_dir.path().display().to_string(),
                true,
                "github_pat_test-only",
            )
            .await
            .unwrap();

        let first = mirrors.push_inner(&info, true).await.unwrap();
        assert_eq!(first.status, "pushed");
        let repo = manager.repo_dir(&info.name);
        let initial = git(&repo, &["rev-parse", "refs/heads/main"]).await;
        assert_eq!(
            git(mirror_dir.path(), &["rev-parse", "main"]).await,
            initial
        );
        assert_eq!(
            git(upstream.path(), &["rev-parse", "main"]).await,
            upstream_before
        );

        let main = manager.ensure_main_worktree(&info).await.unwrap();
        let worktree = Path::new(&main.worktree_path);
        let normal = commit(worktree, "normal.txt", "normal\n").await;
        mirrors.push_inner(&info, true).await.unwrap();
        assert_eq!(git(mirror_dir.path(), &["rev-parse", "main"]).await, normal);

        git(worktree, &["reset", "--hard", &initial]).await;
        let rewritten = commit(worktree, "rewritten.txt", "rewrite\n").await;
        mirrors.push_inner(&info, true).await.unwrap();
        assert_eq!(
            git(mirror_dir.path(), &["rev-parse", "main"]).await,
            rewritten
        );

        let outsider = TempDir::new().unwrap();
        git(
            outsider.path(),
            &["clone", &mirror_dir.path().display().to_string(), "."],
        )
        .await;
        git(outsider.path(), &["config", "user.name", "Outsider"]).await;
        git(
            outsider.path(),
            &["config", "user.email", "outside@example.test"],
        )
        .await;
        git(outsider.path(), &["config", "commit.gpgsign", "false"]).await;
        let outside_sha = commit(outsider.path(), "outside.txt", "outside\n").await;
        git(outsider.path(), &["push", "origin", "main"]).await;

        let error = mirrors.push_inner(&info, true).await.unwrap_err();
        assert!(error.to_string().contains("moved unexpectedly"));
        assert_eq!(
            git(mirror_dir.path(), &["rev-parse", "main"]).await,
            outside_sha
        );
        assert_eq!(
            git(upstream.path(), &["rev-parse", "main"]).await,
            upstream_before
        );

        let state_text = fs::read_to_string(manager.root().join(STATE_FILE)).unwrap();
        assert!(!state_text.contains("github_pat_test-only"));
        let saved = mirrors.project(&info.name).unwrap();
        assert_eq!(saved.result.unwrap().status, "rejected");
        let token = credentials.path().join("demo.token");
        assert_eq!(
            fs::metadata(token).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            fs::read_dir(credentials.path()).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".askpass-")),
            "short-lived askpass helper must be removed"
        );
    }

    #[tokio::test]
    async fn initial_push_refuses_unrelated_nonempty_mirror() {
        let (_upstream, _store, credentials, manager, info) = fixture().await;
        let mirror_work = TempDir::new().unwrap();
        git(mirror_work.path(), &["init", "-b", "main"]).await;
        git(mirror_work.path(), &["config", "user.name", "Other"]).await;
        git(
            mirror_work.path(),
            &["config", "user.email", "other@example.test"],
        )
        .await;
        git(mirror_work.path(), &["config", "commit.gpgsign", "false"]).await;
        commit(mirror_work.path(), "other.txt", "unrelated\n").await;
        let mirror_bare = TempDir::new().unwrap();
        git(mirror_bare.path(), &["init", "--bare", "-b", "main"]).await;
        git(
            mirror_work.path(),
            &[
                "remote",
                "add",
                "target",
                &mirror_bare.path().display().to_string(),
            ],
        )
        .await;
        git(mirror_work.path(), &["push", "target", "main"]).await;
        let before = git(mirror_bare.path(), &["rev-parse", "main"]).await;

        let mirrors = MirrorManager::new(manager, credentials.path(), None);
        mirrors
            .configure_local_for_test(
                &info,
                &mirror_bare.path().display().to_string(),
                false,
                "github_pat_test-only",
            )
            .await
            .unwrap();
        assert!(mirrors.push_inner(&info, true).await.is_err());
        assert_eq!(
            git(mirror_bare.path(), &["rev-parse", "main"]).await,
            before
        );
    }
}
