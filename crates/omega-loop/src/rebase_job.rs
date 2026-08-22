//! # Upstream rebaser cron
//!
//! A scheduled job that keeps each cron-jobbable project's **main branch**
//! in sync with upstream, plus the dedicated "upstream rebaser" chats that
//! handle the judgment parts.
//!
//! Per scheduled run, for every cron-jobbable project:
//!
//! 1. `git fetch upstream` and classify the local default branch against
//!    `upstream/<default>` ([`omega_projects::DefaultBranchStatus`]):
//!    - strictly behind  → mechanical fast-forward, then wake the rebaser
//!      chat to **verify the worktree builds and tests pass**,
//!    - up to date / ahead-only (local-only commits) → nothing,
//!    - **diverged** (both sides moved) → rebase `main` onto upstream, then
//!      wake the project's dedicated upstream-rebaser chat to fix conflicts
//!      (if any) **and** verify the build. The local-only commits are
//!      preserved — the rebaser keeps the intent of both sides.
//!
//! The rebaser chat is always invoked after any git operation that changes
//! the branch — even a zero-conflict fast-forward or clean rebase — because
//! upstream changes can introduce build failures even without merge
//! conflicts.
//!
//! Configuration comes from the NixOS defaults file
//! (`OMEGA_REBASE_JOB_CONFIG`, see [`omega_projects::rebase`]) overlaid with
//! per-project settings and durable manual requests in `rebase-job.json`.
//!
//! The rebaser chats are real persistent sessions (just like any other),
//! bound to the project's dedicated `main` worktree, so the user can also
//! wake them by hand from the TUI and read their transcripts in the web UI.
//! A rebaser chat is created lazily when a project first needs one.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use omega_core::core::OutputChunk;
use omega_llm::LlmProvider;
use omega_projects::mirror::MirrorManager;
use omega_projects::rebase::{self, EffectiveProjectConfig, RebaseDefaults};
use omega_projects::{
    ActiveProject, DefaultBranchRebaseStatus, DefaultBranchStatus, ProjectInfo, ProjectManager,
};
use serde_json::json;
use tokio::sync::{broadcast, Mutex};

use crate::agent::{AgentConfig, StandardAgent};
use crate::runtime::{AgentHandle, AgentRuntime};
use crate::session::{AgentSession, SessionStorage};

/// How often the loop wakes to check the interval / "run now" marker.
const POLL: Duration = Duration::from_secs(5);
const CONFLICT_RECONCILE_SECONDS: i64 = 60;

/// Wake messages start with this so the rebaser (and a human skimming the
/// transcript) can tell a scheduled wake from a user message.
const WAKE_HINT: &str = "⏰ upstream-rebaser job:";

struct DivergedRebase {
    local: String,
    upstream: String,
    ahead: usize,
    behind: usize,
    upstream_ref: String,
    conflict_detail: String,
}

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
    mirrors: MirrorManager,
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
        let mirrors = MirrorManager::from_env(projects.as_ref().clone());
        Self {
            projects,
            runtime,
            provider,
            session_storage,
            defaults,
            mirrors,
            rebasers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Run forever: every `POLL` seconds, atomically claim durable manual
    /// requests and independently due per-project schedules.
    pub async fn run(self) {
        match rebase::update_state(self.projects.root(), |state| {
            Ok(rebase::recover_interrupted_runs(state, Utc::now()))
        }) {
            Ok(recovered) if !recovered.is_empty() => {
                tracing::warn!(?recovered, "requeued interrupted upstream rebase runs")
            }
            Err(e) => tracing::warn!(error = %e, "could not recover interrupted rebase runs"),
            _ => {}
        }

        loop {
            // Cadence and pending manual runs are persisted, so neither a
            // web/loop race nor a daemon restart loses or resets a trigger.
            let state = rebase::load_state(self.projects.root());
            self.reconcile_conflicted_runs(&state).await;
            let state = rebase::load_state(self.projects.root());
            let eff = rebase::resolve_effective(self.defaults.as_ref(), &state);
            let now = Utc::now();
            let mut candidates: BTreeMap<String, EffectiveProjectConfig> = BTreeMap::new();

            for project in &eff.projects {
                let run = state.project_runs.get(&project.name);
                let last = run.and_then(|r| r.last_started_at);
                if !rebase::run_blocks_schedule(run)
                    && rebase::project_is_due(now, last, project.interval_seconds)
                {
                    candidates.insert(project.name.clone(), project.clone());
                }
            }
            // Manual runs are allowed even while the automatic schedule is
            // disabled. Resolve their cadence/prompt from stored overrides.
            for (name, run) in &state.project_runs {
                if run.pending_since.is_some() {
                    candidates.entry(name.clone()).or_insert_with(|| {
                        rebase::resolve_project(self.defaults.as_ref(), &state, name, false)
                    });
                }
            }

            for (_, project) in candidates {
                if let Some((trigger, claimed)) = self.claim_run(&project).await {
                    tracing::info!(
                        project = %project.name,
                        interval_seconds = project.interval_seconds,
                        %trigger,
                        "upstream rebase run"
                    );
                    let result = self.run_project(&claimed).await;
                    self.finish_run(&claimed.name, &trigger, result).await;
                }
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn claim_run(
        &self,
        requested: &EffectiveProjectConfig,
    ) -> Option<(String, EffectiveProjectConfig)> {
        let now = Utc::now();
        let result = rebase::update_state(self.projects.root(), |state| {
            let current =
                rebase::resolve_project(self.defaults.as_ref(), state, &requested.name, false);
            let run = state
                .project_runs
                .entry(requested.name.clone())
                .or_default();
            let trigger = if run.pending_since.take().is_some() {
                Some(
                    run.pending_trigger
                        .take()
                        .unwrap_or_else(|| "manual".to_string()),
                )
            } else {
                (current.enabled
                    && !rebase::run_blocks_schedule(Some(run))
                    && rebase::project_is_due(now, run.last_started_at, current.interval_seconds))
                .then_some("schedule".to_string())
            };
            let Some(trigger) = trigger else {
                return Ok(None);
            };
            run.last_started_at = Some(now);
            run.last_finished_at = None;
            run.last_trigger = Some(trigger.clone());
            run.result = Some(json!({ "status": "running" }));
            Ok(Some((trigger, current)))
        });
        match result {
            Ok(trigger) => trigger,
            Err(e) => {
                tracing::warn!(project = %requested.name, error = %e, "could not claim rebase run");
                None
            }
        }
    }

    /// Reconcile in-progress rebase runs after a daemon restart. This
    /// survives losing the in-memory output subscriber.
    ///
    /// For runs stuck in "conflicted" status: checks whether Git has finished
    /// the rebase (no more rebase state) and the branch is up to date with
    /// upstream — if so, promotes to completed.
    ///
    /// For runs stuck in "verifying" status (fast-forward/clean-rebase where
    /// the rebaser was woken for build verification): the git operation
    /// already completed, so we just verify the branch state and promote.
    /// Never wakes an unresolved conflict.
    async fn reconcile_conflicted_runs(&self, snapshot: &rebase::RebaseState) {
        let now = Utc::now();
        let candidates: Vec<String> = snapshot
            .project_runs
            .iter()
            .filter(|(_, run)| {
                rebase::conflict_reconciliation_is_due(run, now, CONFLICT_RECONCILE_SECONDS)
            })
            .map(|(name, _)| name.clone())
            .collect();

        for project in candidates {
            let claimed = rebase::update_state(self.projects.root(), |state| {
                let Some(run) = state.project_runs.get_mut(&project) else {
                    return Ok(false);
                };
                if !rebase::conflict_reconciliation_is_due(run, now, CONFLICT_RECONCILE_SECONDS) {
                    return Ok(false);
                }
                run.last_reconciled_at = Some(now);
                Ok(true)
            });
            if !matches!(claimed, Ok(true)) {
                continue;
            }

            let info = match self.projects.find(&project).await {
                Ok(Some(info)) => info,
                _ => continue,
            };
            match self.projects.default_branch_rebase_in_progress(&info).await {
                Ok(true) | Err(_) => continue,
                Ok(false) => {}
            }
            let verified = self.projects.update_default_branch(&info).await;
            if !matches!(
                verified,
                Ok(Some(
                    DefaultBranchStatus::UpToDate
                        | DefaultBranchStatus::AheadOnly
                        | DefaultBranchStatus::FastForwarded(_, _)
                ))
            ) {
                continue;
            }

            // Git operation completed outside the original scheduler call.
            // Push mirrors and promote the run to completed.
            let _ = self.mirrors.auto_push(&info).await;

            let _ = rebase::update_state(self.projects.root(), |state| {
                let Some(run) = state.project_runs.get_mut(&project) else {
                    return Ok(());
                };
                let current_status = rebase::run_status(run);
                let (outcome, reconciled) = match current_status {
                    Some("verifying") => ("verification-resolved", true),
                    Some("conflicted") => ("conflicts-resolved", true),
                    _ => return Ok(()),
                };
                let session = run
                    .result
                    .as_ref()
                    .and_then(|result| result.get("session"))
                    .cloned();
                run.last_finished_at = Some(Utc::now());
                run.result = Some(json!({
                    "status": "completed",
                    "outcome": outcome,
                    "session": session,
                    "reconciled": reconciled,
                }));
                Ok(())
            });
        }
    }

    async fn run_project(&self, project: &EffectiveProjectConfig) -> serde_json::Value {
        let info = match self.projects.find(&project.name).await {
            Ok(Some(info)) => info,
            Ok(None) => {
                return json!({
                    "status": "failed", "outcome": "missing", "detail": "not registered"
                })
            }
            Err(e) => {
                return json!({
                    "status": "failed", "outcome": "lookup-error", "detail": format!("{e:#}")
                })
            }
        };
        let mut value = match self.sync_project(project, &info).await {
            ProjectSync::Ok(entry) | ProjectSync::Skipped(entry) | ProjectSync::Err(entry) => entry,
        };
        if let Some(object) = value.as_object_mut() {
            let outcome = object
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let status = match outcome.as_str() {
                // All rebaser-wake cases — the background monitor in
                // `do_wake_rebaser` will eventually write the final result.
                "conflicts" | "fast-forwarded" | "rebased" => "verifying",
                "error" | "missing" | "skipped" => "failed",
                _ => "completed",
            };
            object.insert("outcome".into(), json!(outcome));
            object.insert("status".into(), json!(status));
        }
        value
    }

    async fn finish_run(&self, project: &str, trigger: &str, result: serde_json::Value) {
        let finished = Utc::now();
        let save = rebase::update_state(self.projects.root(), |state| {
            let run = state.project_runs.entry(project.to_string()).or_default();
            run.last_finished_at = Some(finished);
            run.last_trigger = Some(trigger.to_string());
            run.result = Some(result.clone());
            // Maintain the old summary field for backwards-compatible API/
            // UI readers while the structured project_runs map is canonical.
            let mut per_project = serde_json::Map::new();
            per_project.insert(project.to_string(), result.clone());
            state.last_run = Some(json!({
                "at": finished.to_rfc3339(),
                "per_project": per_project,
            }));
            Ok(())
        });
        if let Err(e) = save {
            tracing::warn!(project, error = %e, "could not persist rebase result");
        }
    }

    /// Sync one project's main branch with upstream. Always invokes the
    /// rebaser chat for any change (fast-forward, clean rebase, or conflicts)
    /// so the LLM can verify the worktree still builds — even zero-conflict
    /// operations can introduce build failures.
    async fn sync_project(
        &self,
        config: &EffectiveProjectConfig,
        info: &ProjectInfo,
    ) -> ProjectSync {
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
                    // Even a clean fast-forward can break a build — invoke rebaser to verify.
                    self.wake_rebaser_for_verification(
                        config,
                        info,
                        format!(
                            "Main was fast-forwarded from {old} to {new} (upstream has new commits). \
                             Please verify that the worktree builds and tests pass."
                        ),
                        "fast-forwarded",
                    )
                    .await
                }
                DefaultBranchStatus::Diverged {
                    local,
                    upstream,
                    ahead,
                    behind,
                } => match self.projects.rebase_default_branch(info).await {
                    Ok(DefaultBranchRebaseStatus::Rebased { old, new }) => {
                        tracing::info!(
                            project = %info.name,
                            from = %old,
                            to = %new,
                            ahead,
                            behind,
                            "rebased main onto upstream (no conflicts)"
                        );
                        // Clean rebase can still break a build — invoke rebaser to verify.
                        self.wake_rebaser_for_verification(
                            config,
                            info,
                            format!(
                                "Main was rebased from {old} to {new} with {ahead} local commits ahead \
                                 and {behind} upstream commits behind. No merge conflicts occurred, but \
                                 please verify that the worktree still builds and tests pass."
                            ),
                            "rebased",
                        )
                        .await
                    },
                    Ok(DefaultBranchRebaseStatus::Conflicts {
                        upstream_ref,
                        detail,
                    }) => {
                        self.handle_diverged(config, info, DivergedRebase {
                            local,
                            upstream,
                            ahead,
                            behind,
                            upstream_ref,
                            conflict_detail: detail,
                        })
                        .await
                    }
                    Err(e) => ProjectSync::Err(json!({
                        "status": "error",
                        "detail": format!("could not start rebase: {e:#}"),
                    })),
                },
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
    /// upstream-rebaser chat, which fixes the conflicts itself and then
    /// verifies the build.
    async fn handle_diverged(
        &self,
        config: &EffectiveProjectConfig,
        info: &ProjectInfo,
        divergence: DivergedRebase,
    ) -> ProjectSync {
        let DivergedRebase {
            local,
            upstream,
            ahead,
            behind,
            upstream_ref,
            conflict_detail,
        } = divergence;
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
        let wake = format!(
            "{WAKE_HINT} upstream moved for '{project}': {upstream_ref} has {behind} new \
             commit(s) and local main has {ahead} local-only commit(s). Git has already \
             started the rebase in your worktree (branch {branch}) and stopped for \
             conflicts. Resolve them, continue the rebase to completion, verify the tree, \
             and report what you did.\n\
             Main was at {local}; upstream is at {upstream}.\n\
             Git reported: {conflict_detail}\n\n\
             <configured-conflict-resolution-prompt>\n{prompt}\n\
             </configured-conflict-resolution-prompt>",
            project = info.name,
            branch = main_active.branch,
            local = &local[..local.len().min(12)],
            upstream = &upstream[..upstream.len().min(12)],
            prompt = config.prompt,
        );

        self.do_wake_rebaser(config, info, &main_active, &session_id, wake, "conflicts")
            .await
    }

    /// Wake the rebaser after a mechanical sync (fast-forward or clean rebase)
    /// where no merge conflicts occurred. The rebaser verifies that the
    /// worktree still builds and tests pass.
    async fn wake_rebaser_for_verification(
        &self,
        config: &EffectiveProjectConfig,
        info: &ProjectInfo,
        description: String,
        outcome: &'static str,
    ) -> ProjectSync {
        let main_active = match self.projects.ensure_main_worktree(info).await {
            Ok(active) => active,
            Err(e) => {
                tracing::warn!(
                    project = %info.name,
                    error = %e,
                    "could not ensure main worktree for build verification"
                );
                return ProjectSync::Err(json!({
                    "status": "error",
                    "detail": format!("main worktree unavailable: {e:#}"),
                }));
            }
        };

        let session_id = session_id_for(&info.name);
        let wake = format!(
            "{WAKE_HINT} {description}\n\n\
             <configured-conflict-resolution-prompt>\n{prompt}\n\
             </configured-conflict-resolution-prompt>",
            description = description,
            prompt = config.prompt,
        );

        self.do_wake_rebaser(config, info, &main_active, &session_id, wake, outcome)
            .await
    }

    /// Common wake-the-rebaser-and-monitor-completion logic.
    /// Sends `wake` to the rebaser session, records the run as `status`,
    /// then spawns a background task that waits for the rebaser to finish
    /// and promotes the run to completed (or failed).
    async fn do_wake_rebaser(
        &self,
        config: &EffectiveProjectConfig,
        info: &ProjectInfo,
        main_active: &ActiveProject,
        session_id: &str,
        wake: String,
        status: &'static str,
    ) -> ProjectSync {
        match self
            .ensure_rebaser(info, main_active, &config.prompt)
            .await
        {
            Ok(handle) => {
                let mut output = handle.subscribe();
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
                    status,
                    "woke upstream rebaser"
                );

                // Follow the dedicated session until a turn actually leaves
                // main based on upstream, then promote it to completed. If
                // another user turn was already ahead of this wake, its
                // `Done` will still leave the repository diverged and this
                // monitor keeps waiting for the queued rebaser turn.
                let projects = Arc::clone(&self.projects);
                let info_for_monitor = info.clone();
                let root = self.projects.root().to_path_buf();
                let monitored_session_id = session_id.to_string();
                let monitored_outcome = format!("{status}-verified");
                let mirrors = self.mirrors.clone();
                tokio::spawn(async move {
                    loop {
                        match output.recv().await {
                            Ok(OutputChunk::Done) => {
                                let verified =
                                    projects.update_default_branch(&info_for_monitor).await;
                                let completed = matches!(
                                    verified,
                                    Ok(Some(
                                        DefaultBranchStatus::UpToDate
                                            | DefaultBranchStatus::AheadOnly
                                            | DefaultBranchStatus::FastForwarded(_, _)
                                    ))
                                );
                                if !completed {
                                    continue;
                                }
                                let _ = mirrors.auto_push(&info_for_monitor).await;
                                let _ = rebase::update_state(&root, |state| {
                                    let run = state
                                        .project_runs
                                        .entry(info_for_monitor.name.clone())
                                        .or_default();
                                    run.last_finished_at = Some(Utc::now());
                                    run.result = Some(json!({
                                        "status": "completed",
                                        "outcome": &monitored_outcome,
                                        "session": &monitored_session_id,
                                    }));
                                    Ok(())
                                });
                                break;
                            }
                            Ok(OutputChunk::Error(error)) => {
                                let _ = rebase::update_state(&root, |state| {
                                    let run = state
                                        .project_runs
                                        .entry(info_for_monitor.name.clone())
                                        .or_default();
                                    run.last_finished_at = Some(Utc::now());
                                    run.result = Some(json!({
                                        "status": "failed",
                                        "outcome": "rebaser-error",
                                        "detail": error,
                                        "session": &monitored_session_id,
                                    }));
                                    Ok(())
                                });
                                break;
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                });
                ProjectSync::Ok(json!({
                    "status": status,
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

    /// Get (creating if needed) the dedicated upstream-rebaser chat for
    /// `info`, bound to the project's `main` worktree, and keep its handle
    /// for the daemon's lifetime so a wake is just a message.
    async fn ensure_rebaser(
        &self,
        info: &ProjectInfo,
        main_active: &ActiveProject,
        configured_prompt: &str,
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
            agent_prompt = configured_prompt,
        )
        .replace("{default-branch}", &default);

        // Reuse an existing session (keeps transcript history); otherwise
        // create one, seeded with a bootstrap exchange so it survives the
        // daemon's empty-session pruning.
        let mut session = match AgentSession::load_with_storage(
            &session_id,
            self.session_storage.as_ref().clone(),
        ) {
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
        let tools = crate::create_tools_for_dir(
            Path::new(&main_active.worktree_path),
            &session_id,
            rename_tool,
        );

        let provider = self.provider.read().unwrap().clone();
        let agent_cfg = AgentConfig::new()
            .with_tools(tools)
            .with_prompt_caching(true);
        let agent = StandardAgent::new(agent_cfg, provider);
        let handle = self
            .runtime
            .spawn(session, |internals| agent.run(internals))
            .await?;

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
