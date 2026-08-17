//! # Upstream rebaser cron
//!
//! A scheduled job that keeps each cron-jobbable project's **main branch**
//! in sync with upstream, plus the dedicated "upstream rebaser" chats that
//! handle the judgment parts.
//!
//! Per scheduled run, for every cron-jobbable project:
//!
//! 1. `git fetch origin` and classify the local default branch against
//!    `origin/<default>` ([`omega_projects::DefaultBranchStatus`]):
//!    - strictly behind  → mechanical fast-forward (no LLM involved),
//!    - up to date / ahead-only (local-only commits) → nothing,
//!    - **diverged** (both sides moved) → wake the project's dedicated
//!      upstream-rebaser chat, which rebases `main` onto upstream and
//!      auto-fixes the merge conflicts (the local-only commits are
//!      preserved — the rebaser keeps the intent of both sides).
//!
//! Configuration comes from the NixOS defaults file
//! (`OMEGA_REBASE_JOB_CONFIG`, see [`omega_projects::rebase`]) overlaid with
//! the imperative state file the web UI edits (`rebase-job.json` in the
//! project store). A `rebase-now` marker file in the store root triggers an
//! immediate run ("run now" from the web UI).
//!
//! The rebaser chats are real persistent sessions (just like any other),
//! bound to the project's dedicated `main` worktree, so the user can also
//! wake them by hand from the TUI and read their transcripts in the web UI.
//! The cron creates one per cron-jobbable project (idle until woken) so
//! they are ready to be called into at any time.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use omega_llm::LlmProvider;
use omega_projects::rebase::{
    self, EffectiveConfig, RebaseDefaults, RebaseState,
};
use omega_projects::{ActiveProject, DefaultBranchStatus, ProjectInfo, ProjectManager};
use serde_json::json;
use tokio::sync::{broadcast, Mutex};

use crate::agent::{AgentConfig, StandardAgent};
use crate::runtime::{AgentHandle, AgentRuntime};
use crate::session::{AgentSession, SessionStorage};

/// How often the loop wakes to check the interval / "run now" marker.
const POLL: Duration = Duration::from_secs(5);

/// Wake messages start with this so the rebaser (and a human skimming the
/// transcript) can tell a scheduled wake from a user message.
const WAKE_HINT: &str = "⏰ upstream-rebaser job:";

/// The rebaser session id is deterministic per project so resumes and the
/// web UI can find it again.
fn session_id_for(project: &str) -> String {
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

/// The cron job. `defaults` comes from the NixOS module (may be `None`
/// when running omega without it); the imperative state file is re-read
/// every tick so web-UI changes apply without a restart.
pub struct RebaseJob {
    projects: Arc<ProjectManager>,
    runtime: AgentRuntime,
    provider: Arc<std::sync::RwLock<Arc<dyn LlmProvider>>>,
    session_storage: Arc<SessionStorage>,
    defaults: Option<RebaseDefaults>,
    /// Live rebaser handles (session_id → handle), kept for the lifetime of
    /// the daemon so a wake is just a message.
    rebasers: Arc<Mutex<HashMap<String, AgentHandle>>>,
}

impl RebaseJob {
    #[allow(clippy::too_many_arguments)] // private daemon wiring, same as create_session
    pub fn new(
        projects: Arc<ProjectManager>,
        runtime: AgentRuntime,
        provider: Arc<std::sync::RwLock<Arc<dyn LlmProvider>>>,
        session_storage: Arc<SessionStorage>,
        defaults: Option<RebaseDefaults>,
    ) -> Self {
        Self {
            projects,
            runtime,
            provider,
            session_storage,
            defaults,
            rebasers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Run forever: every `POLL` seconds, run the job when the interval is
    /// due or a `rebase-now` marker exists. Each cron-jobbable project also
    /// gets its dedicated upstream-rebaser chat (idle) so the user can wake
    /// it by hand at any time.
    pub async fn run(self) {
        let mut next_run: Option<Instant> = None;
        let mut last_ensured: String = String::new();
        loop {
            // Reload state each tick so a web-UI toggle / interval change /
            // run-now request takes effect promptly.
            let state = rebase::load_state(self.projects.root());
            let eff = rebase::resolve_effective(self.defaults.as_ref(), &state);
            let run_now = rebase::run_now_path(self.projects.root());

            // (Re)create idle rebaser chats when the project set changes,
            // so a project freshly enabled from the web UI gets its chat.
            let sig = eff.projects.join(",");
            if sig != last_ensured {
                for name in &eff.projects {
                    if let Err(e) = self.ensure_rebaser_for(name).await {
                        tracing::warn!(
                            project = %name,
                            error = %e,
                            "could not prepare upstream rebaser chat"
                        );
                    }
                }
                last_ensured = sig;
            }

            let due = next_run.is_some_and(|n| Instant::now() >= n);
            let marked = eff.is_active() && run_now.is_file();
            if due || marked {
                if marked {
                    let _ = tokio::fs::remove_file(&run_now).await;
                }
                let interval = eff.interval_seconds;
                tracing::info!(
                    projects = ?eff.projects,
                    interval_seconds = interval,
                    triggered = if due { "schedule" } else { "run-now" },
                    "rebase cron run"
                );
                self.run_once(&eff, &state).await;
                next_run = Some(Instant::now() + Duration::from_secs(interval));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// One full run: for each cron-jobbable project, sync main with upstream
    /// and wake the rebaser on divergence. Records a summary in the
    /// imperative state file (`last_run`) for the web UI.
    async fn run_once(&self, eff: &EffectiveConfig, _state: &RebaseState) {
        let mut per_project = serde_json::Map::new();
        for name in &eff.projects {
            let info = match self.projects.find(name).await {
                Ok(Some(info)) => info,
                Ok(None) => {
                    tracing::warn!(project = %name, "cron-jobbable project is not registered");
                    per_project.insert(
                        name.clone(),
                        json!({ "status": "missing", "detail": "not registered" }),
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(project = %name, error = %e, "cron project lookup failed");
                    per_project.insert(
                        name.clone(),
                        json!({ "status": "error", "detail": format!("{e:#}") }),
                    );
                    continue;
                }
            };

            per_project.insert(
                name.clone(),
                match self.sync_project(&info).await {
                    ProjectSync::Ok(entry) => entry,
                    ProjectSync::Skipped(entry) => entry,
                    ProjectSync::Err(entry) => entry,
                },
            );
        }

        // Persist `last_run` so the web UI shows what happened. Reload the
        // state first (a web-UI toggle may have landed while we ran) so we
        // don't clobber it with the tick's stale copy.
        let mut merged = rebase::load_state(self.projects.root());
        merged.last_run = Some(json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "per_project": serde_json::Value::Object(per_project),
        }));
        if let Err(e) = rebase::save_state(self.projects.root(), &merged) {
            tracing::warn!(error = %e, "could not persist rebase last_run");
        }
    }

    /// Sync one project's main branch with upstream; may wake the rebaser.
    async fn sync_project(&self, info: &ProjectInfo) -> ProjectSync {
        match self.projects.update_default_branch(info).await {
            Ok(Some(status)) => match status {
                DefaultBranchStatus::UpToDate => {
                    tracing::info!(project = %info.name, "main already up to date with upstream");
                    ProjectSync::Ok(json!({ "status": "up-to-date" }))
                }
                DefaultBranchStatus::AheadOnly => {
                    tracing::info!(project = %info.name, "main has local-only commits; upstream unchanged");
                    ProjectSync::Ok(json!({ "status": "ahead-only" }))
                }
                DefaultBranchStatus::FastForwarded(old, new) => {
                    tracing::info!(
                        project = %info.name,
                        from = %old,
                        to = %new,
                        "fast-forwarded main to upstream"
                    );
                    ProjectSync::Ok(json!({
                        "status": "fast-forwarded",
                        "from": old,
                        "to": new,
                    }))
                }
                DefaultBranchStatus::Diverged {
                    local,
                    upstream,
                    ahead,
                    behind,
                } => self.handle_diverged(info, local, upstream, ahead, behind).await,
            },
            Ok(None) => {
                tracing::warn!(project = %info.name, "main sync: no default branch to track");
                ProjectSync::Skipped(json!({ "status": "skipped", "detail": "no default branch" }))
            }
            Err(e) => {
                tracing::warn!(project = %info.name, error = %e, "main sync failed");
                ProjectSync::Err(json!({ "status": "error", "detail": format!("{e:#}") }))
            }
        }
    }

    /// Both sides moved: hand the rebase to the project's dedicated
    /// upstream-rebaser chat, which fixes the conflicts itself.
    async fn handle_diverged(
        &self,
        info: &ProjectInfo,
        local: String,
        upstream: String,
        ahead: usize,
        behind: usize,
    ) -> ProjectSync {
        let default = self
            .projects
            .default_branch(info)
            .await
            .unwrap_or_else(|| "main".to_string());
        let main_active = match self.projects.ensure_main_worktree(info).await {
            Ok(active) => active,
            Err(e) => {
                tracing::warn!(
                    project = %info.name,
                    error = %e,
                    "could not ensure main worktree for rebase"
                );
                return ProjectSync::Err(json!({
                    "status": "error",
                    "detail": format!("main worktree unavailable: {e:#}"),
                }));
            }
        };

        let session_id = session_id_for(&info.name);
        match self.ensure_rebaser(info, &main_active).await {
            Ok(handle) => {
                let wake = format!(
                    "{WAKE_HINT} upstream moved for '{project}': origin/{default} has {behind} new \
                     commit(s) and local main has {ahead} local-only commit(s). Rebase main onto \
                     origin/{default} in your worktree (branch {branch}), resolve any conflicts \
                     yourself — preserve the local-only functionality — finish the rebase, and \
                     verify the tree. Report what you did.\n\
                     Main was at {local}; upstream is at {upstream}.",
                    project = info.name,
                    default = default,
                    branch = main_active.branch,
                    local = &local[..local.len().min(12)],
                    upstream = &upstream[..upstream.len().min(12)],
                );
                if let Err(e) = handle.send_input(wake.clone()).await {
                    tracing::warn!(
                        %session_id,
                        project = %info.name,
                        error = %e,
                        "could not wake rebaser"
                    );
                    return ProjectSync::Err(json!({
                        "status": "error",
                        "detail": format!("wake failed: {e:#}"),
                    }));
                }
                tracing::info!(
                    %session_id,
                    project = %info.name,
                    ahead,
                    behind,
                    "woke upstream rebaser for diverged main"
                );
                ProjectSync::Ok(json!({
                    "status": "diverged",
                    "ahead": ahead,
                    "behind": behind,
                    "session": session_id,
                    "rebaser_woken": true,
                }))
            }
            Err(e) => {
                tracing::warn!(
                    project = %info.name,
                    error = %e,
                    "could not create upstream rebaser session"
                );
                ProjectSync::Err(json!({
                    "status": "error",
                    "detail": format!("rebaser unavailable: {e:#}"),
                }))
            }
        }
    }

    /// Get (creating if needed) the dedicated upstream-rebaser chat for a
    /// cron-jobbable project by name: resolves the project, ensures its main
    /// worktree exists, then ensures the session. Used by the idle-prep pass
    /// so every enabled project has a chat the user can wake by hand.
    async fn ensure_rebaser_for(&self, name: &str) -> anyhow::Result<AgentHandle> {
        let info = self
            .projects
            .find(name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("project '{name}' is not registered"))?;
        let main_active = self.projects.ensure_main_worktree(&info).await?;
        self.ensure_rebaser(&info, &main_active).await
    }

    /// Get (creating if needed) the dedicated upstream-rebaser chat for
    /// `info`, bound to the project's `main` worktree, and keep its handle
    /// for the daemon's lifetime so a wake is just a message.
    async fn ensure_rebaser(
        &self,
        info: &ProjectInfo,
        main_active: &ActiveProject,
    ) -> anyhow::Result<AgentHandle> {
        let session_id = session_id_for(&info.name);
        {
            let map = self.rebasers.lock().await;
            if let Some(handle) = map.get(&session_id) {
                return Ok(handle.clone());
            }
        }

        let default = self
            .projects
            .default_branch(info)
            .await
            .unwrap_or_else(|| "main".to_string());

        // System prompt: configured rebaser prompt (NixOS) or the built-in
        // default, plus a project header so the agent knows the repo.
        let agent_prompt = self
            .defaults
            .as_ref()
            .map(|d| d.agent_prompt.clone())
            .unwrap_or_else(|| omega_projects::rebase::DEFAULT_REBASER_PROMPT.to_string());
        let system_prompt = format!(
            "{agent_prompt}\n\n\
             You are bound to the project '{name}'.\n\
             Repo URL: {url}\n\
             Worktree branch: {branch} (the project's main branch)\n\
             Working directory: {worktree}\n\
             All file reads/writes/edits and shell commands operate inside this directory.",
            name = info.name,
            url = info.url,
            branch = main_active.branch,
            worktree = main_active.worktree_path,
        )
        .replace("{default-branch}", &default);

        // Reuse an existing session (keeps transcript history); otherwise
        // create one, seeded with a bootstrap exchange so it survives the
        // daemon's empty-session pruning.
        let mut session = match AgentSession::load_with_storage(&session_id, self.session_storage.as_ref().clone()) {
            Ok(s) => s,
            Err(_) => {
                let mut s = AgentSession::new_with_storage(
                    &session_id,
                    "rebaser",
                    format!("rebaser-{}", info.name),
                    "Upstream rebaser chat",
                    &system_prompt,
                    self.session_storage.as_ref().clone(),
                )?;
                s.add_message(omega_llm::Message::user(format!(
                    "[bootstrap] You are the dedicated upstream rebaser chat for '{name}'. \
                     This session was created by the rebase cron job so you can be woken at any \
                     time. Your worktree: {worktree} (branch {branch}).",
                    name = info.name,
                    worktree = main_active.worktree_path,
                    branch = main_active.branch,
                )))?;
                s.add_message(omega_llm::Message::assistant(
                    "[bootstrap] Ready. I keep the main branch rebased on the latest upstream \
                     and resolve conflicts myself.",
                ))?;
                s
            }
        };
        session.update_system_prompt(&system_prompt)?;

        // Tools bound to the main worktree, with the usual shell/fs tools.
        // The rename tool is wired to nothing (it errors harmlessly if the
        // agent ever tries it — rebaser worktrees are not meant to be renamed).
        let event_tx = broadcast::channel(16).0;
        let rename_tool = crate::rename_worktree_tool(
            &self.projects,
            &session_id,
            &Arc::new(Mutex::new(HashMap::new())),
            &self.session_storage,
            &event_tx,
        );
        let tools = crate::create_tools_for_dir(Path::new(&main_active.worktree_path), &session_id, rename_tool);

        let provider = self.provider.read().unwrap().clone();
        let agent_cfg = AgentConfig::new().with_tools(tools).with_prompt_caching(true);
        let agent = StandardAgent::new(agent_cfg, provider);
        let handle = self.runtime.spawn(session, |internals| agent.run(internals)).await?;

        self.rebasers
            .lock()
            .await
            .insert(session_id.clone(), handle.clone());
        tracing::info!(%session_id, project = %info.name, "upstream rebaser chat ready");
        Ok(handle)
    }
}

/// Outcome of syncing one project, used to build the `last_run` report.
enum ProjectSync {
    Ok(serde_json::Value),
    Skipped(serde_json::Value),
    Err(serde_json::Value),
}