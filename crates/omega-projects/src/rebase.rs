//! # Rebase cron configuration & state
//!
//! Shared by `omega-loop` (the cron worker + the "upstream rebaser" agent
//! sessions) and `omega-git-host` (the web UI that edits it imperatively).
//!
//! There are two sources of configuration:
//!
//! 1. **Defaults** — written by the NixOS module (`services.omega.rebaseJob`)
//!    to a JSON file pointed at by `OMEGA_REBASE_JOB_CONFIG` (default
//!    `/etc/omega/rebase-job.defaults.json`). Carries the interval, the
//!    projects seeded as "cron jobbable", the system prompt for the
//!    dedicated upstream-rebaser chat, and the per-session "update your
//!    checkout" instruction.
//! 2. **Imperative state** — `rebase-job.json` inside the project store
//!    root, edited by the web UI (`GET /rebase`, POST endpoints). Overrides
//!    enablement, interval, and conflict prompt per project and durably
//!    records manual requests plus run status.
//!
//! Effective configuration = defaults ∪ state overrides:
//!
//! ```text
//! enabled projects = (defaults.projects ∪ state.project_states == true)
//!                    − state.project_states == false
//! interval         = state.interval_seconds or defaults.interval_seconds
//! ```
//!
//! A `<project>: false` entry in the state file wins over the NixOS default
//! list (so a project configured in NixOS can be disabled from the web UI
//! and vice versa).

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Env var pointing at the defaults file written by the NixOS module.
pub const ENV_DEFAULTS_CONFIG: &str = "OMEGA_REBASE_JOB_CONFIG";

/// Imperative state file name, inside the project store root.
pub const STATE_FILE: &str = "rebase-job.json";

/// Legacy marker path retained for compatibility with older installations.
/// Current Web UI manual requests live durably in [`RebaseState::project_runs`].
pub const RUN_NOW_FILE: &str = "rebase-now";

/// Lock used to serialize read-modify-write updates from omega-loop and the
/// web process. Without this, a run finishing at the same time as a form
/// submission can silently discard either the result or the new settings.
pub const STATE_LOCK_FILE: &str = "rebase-job.lock";

/// Default interval (seconds) when neither defaults file nor state overrides
/// it: 6 hours.
pub const DEFAULT_INTERVAL_SECONDS: u64 = 6 * 3600;

/// Bounds accepted by the control panel. A tiny interval can accidentally
/// create an expensive LLM/network hot loop; an enormous one is almost
/// certainly a unit mistake.
pub const MIN_INTERVAL_SECONDS: u64 = 30;
pub const MAX_INTERVAL_SECONDS: u64 = 90 * 24 * 3600;
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;

/// Built-in instruction appended to the system prompt of every session that
/// enters a project: keep the checkout in sync with upstream. The NixOS
/// module's `rebaseJob.updatePrompt` default is the same text — keep the two
/// in sync.
pub const DEFAULT_UPDATE_INSTRUCTION: &str =
    "Keep your checkout up to date with upstream. Before starting work in this project, \
     update your checkout: run `git fetch upstream` (if your worktree predates the latest \
     upstream), then rebase your worktree branch onto the project's up-to-date main branch \
     so the worktree is on top of the latest upstream changes. The project's main branch is \
     kept in sync with upstream by a periodic job, so a simple `git rebase <main>` is \
     normally all that is needed. If rebasing surfaces conflicts, resolve them yourself \
     before making changes, and make sure the worktree still builds and tests pass.";

/// Built-in system prompt for the dedicated "upstream rebaser" chat: told to
/// rebase the project's main branch onto upstream and fix conflicts itself.
/// The NixOS module's `rebaseJob.systemPrompt` default is the same text —
/// keep the two in sync.
pub const DEFAULT_REBASER_PROMPT: &str =
    "You are the dedicated 'upstream rebaser' chat for this project.\n\n\
     Your job: keep the project's main branch in sync with upstream. The project's main \
     branch may carry commits that exist only locally (functionality upstream won't or \
     can't add) on top of the upstream history.\n\n\
     When you are woken (by the periodic rebase job or by the user), do this:\n\
     1. Run `git fetch upstream` so the upstream refs are current.\n\
     2. Rebase the main branch onto the latest upstream (`git rebase upstream/<default-branch>` \
     in your worktree). The worktree is your sole writer of the main branch.\n\
     3. If the rebase stops on conflicts, resolve them yourself: keep the intent of \
     upstream's changes AND preserve the local-only functionality. When in doubt, keep \
     both sides' intent, prefer the upstream change where they genuinely conflict, and \
     ask the user in the session if a choice is really ambiguous.\n\
     4. Once the rebase is complete (continue it to the end), verify the worktree builds \
     / tests pass and report what you did.\n\n\
     The user can also ask you directly to rebase at any time; treat that the same way.";

/// Defaults file written by the NixOS module. May be absent when running
/// omega without the module — callers then fall back to the built-in
/// constants above.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebaseDefaults {
    /// Seconds between scheduled runs.
    pub interval_seconds: u64,
    /// Projects seeded as cron jobbable.
    #[serde(default)]
    pub projects: Vec<String>,
    /// System prompt for the dedicated upstream-rebaser chat.
    #[serde(default = "default_rebaser_prompt")]
    pub agent_prompt: String,
    /// Instruction appended to every session's prompt when it enters a
    /// project ("update your checkout").
    #[serde(default = "default_update_instruction")]
    pub update_instruction: String,
}

fn default_rebaser_prompt() -> String {
    DEFAULT_REBASER_PROMPT.to_string()
}

fn default_update_instruction() -> String {
    DEFAULT_UPDATE_INSTRUCTION.to_string()
}

/// Imperative state, edited by the web UI, living in the project store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RebaseState {
    /// Interval override (seconds). `None` → use the defaults' interval.
    #[serde(default)]
    pub interval_seconds: Option<u64>,
    /// Per-project overrides: `true` = cron-jobbable even if not in the
    /// NixOS defaults; `false` = excluded even if listed in the defaults.
    #[serde(default)]
    pub project_states: BTreeMap<String, bool>,
    /// Per-project web overrides. These are deliberately separate from the
    /// NixOS defaults so each soft fork can run on its own cadence and use
    /// conflict-resolution instructions appropriate to that codebase.
    #[serde(default)]
    pub project_configs: BTreeMap<String, RebaseProjectConfig>,
    /// Durable schedule/manual-trigger bookkeeping. Persisting this makes
    /// timers survive daemon restarts and means a manual request cannot be
    /// lost between the web and loop processes.
    #[serde(default)]
    pub project_runs: BTreeMap<String, RebaseProjectRun>,
    /// What the last scheduled run did, for the web UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RebaseProjectConfig {
    /// `None` inherits the legacy/NixOS enabled state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RebaseProjectRun {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_since: Option<DateTime<Utc>>,
    /// Why the pending request was queued (`manual` or `recovery`). Older
    /// state files omit this and are treated as manual requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_trigger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_trigger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reconciled_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

/// Merged configuration the cron actually acts on.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub interval_seconds: u64,
    /// Sorted, deduplicated list of cron-jobbable projects, each with its
    /// independently resolved cadence and rebaser prompt.
    pub projects: Vec<EffectiveProjectConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveProjectConfig {
    pub name: String,
    pub enabled: bool,
    pub interval_seconds: u64,
    pub prompt: String,
}

impl EffectiveConfig {
    /// True when the cron has at least one project to keep up to date.
    pub fn is_active(&self) -> bool {
        !self.projects.is_empty()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.projects.iter().any(|p| p.name == name)
    }
}

/// Merge NixOS defaults with the imperative state (defaults win on the
/// interval only when the state does not set one; a `false` project entry
/// always wins over the defaults list).
pub fn resolve_effective(
    defaults: Option<&RebaseDefaults>,
    state: &RebaseState,
) -> EffectiveConfig {
    let mut set: BTreeMap<String, bool> = BTreeMap::new();
    if let Some(defaults) = defaults {
        for name in &defaults.projects {
            set.insert(name.trim().to_string(), true);
        }
    }
    for (name, enabled) in &state.project_states {
        let name = name.trim().to_string();
        if *enabled {
            set.insert(name, true);
        } else {
            set.remove(&name);
        }
    }

    for (name, config) in &state.project_configs {
        if let Some(enabled) = config.enabled {
            if enabled {
                set.insert(name.trim().to_string(), true);
            } else {
                set.remove(name.trim());
            }
        }
    }

    EffectiveConfig {
        // Kept for older callers and the NixOS-wide default display.
        interval_seconds: effective_interval(defaults, state, None),
        projects: set
            .into_keys()
            .map(|name| resolve_project(defaults, state, &name, true))
            .collect(),
    }
}

/// Resolve a single project's configuration, including disabled projects
/// that can still be triggered manually from the UI.
pub fn resolve_project(
    defaults: Option<&RebaseDefaults>,
    state: &RebaseState,
    name: &str,
    enabled_fallback: bool,
) -> EffectiveProjectConfig {
    let config = state.project_configs.get(name);
    let enabled = config
        .and_then(|c| c.enabled)
        .or_else(|| state.project_states.get(name).copied())
        .unwrap_or(
            enabled_fallback || defaults.is_some_and(|d| d.projects.iter().any(|p| p == name)),
        );
    let prompt = config
        .and_then(|c| c.prompt.as_deref())
        .filter(|p| !p.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| defaults.map(|d| d.agent_prompt.clone()))
        .unwrap_or_else(default_rebaser_prompt);
    EffectiveProjectConfig {
        name: name.to_string(),
        enabled,
        interval_seconds: effective_interval(defaults, state, config),
        prompt,
    }
}

fn effective_interval(
    defaults: Option<&RebaseDefaults>,
    state: &RebaseState,
    project: Option<&RebaseProjectConfig>,
) -> u64 {
    let interval = project
        .and_then(|c| c.interval_seconds)
        .or(state.interval_seconds)
        .or_else(|| defaults.map(|d| d.interval_seconds))
        .unwrap_or(DEFAULT_INTERVAL_SECONDS);
    if interval == 0 {
        DEFAULT_INTERVAL_SECONDS
    } else {
        interval
    }
}

pub fn validate_interval(seconds: u64) -> Result<()> {
    anyhow::ensure!(
        (MIN_INTERVAL_SECONDS..=MAX_INTERVAL_SECONDS).contains(&seconds),
        "interval must be between {MIN_INTERVAL_SECONDS} and {MAX_INTERVAL_SECONDS} seconds"
    );
    Ok(())
}

pub fn validate_prompt(prompt: &str) -> Result<()> {
    anyhow::ensure!(
        prompt.len() <= MAX_PROMPT_BYTES,
        "prompt must not exceed {MAX_PROMPT_BYTES} bytes"
    );
    anyhow::ensure!(!prompt.contains('\0'), "prompt must not contain NUL bytes");
    Ok(())
}

/// A project with no previous attempt is immediately due. Afterwards its
/// persisted start time controls the cadence, so restarts do not reset or
/// duplicate the timer.
pub fn project_is_due(
    now: DateTime<Utc>,
    last_started_at: Option<DateTime<Utc>>,
    interval_seconds: u64,
) -> bool {
    let Some(last) = last_started_at else {
        return true;
    };
    now.signed_duration_since(last).num_seconds() >= interval_seconds as i64
}

/// Scheduled attempts pause while a manual request is queued, a fetch is
/// running, or a rebaser session is still verifying (or resolving conflicts
/// for) the in-progress run. A user can always explicitly queue another
/// manual wake.
pub fn run_blocks_schedule(run: Option<&RebaseProjectRun>) -> bool {
    let Some(run) = run else {
        return false;
    };
    if run.pending_since.is_some() {
        return true;
    }
    matches!(
        run.result
            .as_ref()
            .and_then(|r| r.get("status"))
            .and_then(serde_json::Value::as_str),
        Some("queued" | "running" | "conflicted" | "verifying")
    )
}

pub fn run_status(run: &RebaseProjectRun) -> Option<&str> {
    run.result
        .as_ref()
        .and_then(|result| result.get("status"))
        .and_then(serde_json::Value::as_str)
}

/// Recover work claimed by a previous omega-loop process which exited
/// before recording a terminal result. This is called once at daemon start;
/// it never touches conflicted or verifying entries, whose Git state is
/// reconciled separately without automatically waking another model turn.
pub fn recover_interrupted_runs(state: &mut RebaseState, now: DateTime<Utc>) -> Vec<String> {
    let mut recovered = Vec::new();
    for (project, run) in &mut state.project_runs {
        if run_status(run) != Some("running") {
            continue;
        }
        let previous_trigger = run.last_trigger.clone();
        run.pending_since = Some(now);
        run.pending_trigger = Some("recovery".to_string());
        run.last_trigger = Some("recovery".to_string());
        run.result = Some(serde_json::json!({
            "status": "queued",
            "outcome": "recovered-after-restart",
            "previous_trigger": previous_trigger,
        }));
        recovered.push(project.clone());
    }
    recovered
}

pub fn conflict_reconciliation_is_due(
    run: &RebaseProjectRun,
    now: DateTime<Utc>,
    interval_seconds: i64,
) -> bool {
    if run_status(run) != Some("conflicted") {
        return false;
    }
    run.last_reconciled_at
        .is_none_or(|last| now.signed_duration_since(last).num_seconds() >= interval_seconds)
}

// ---------------------------------------------------------------------------
// File I/O
// ---------------------------------------------------------------------------

/// Load the defaults file referenced by `OMEGA_REBASE_JOB_CONFIG`, if set
/// and readable. Absent/malformed config is a warning, never fatal.
pub fn load_defaults() -> Option<RebaseDefaults> {
    let path = std::env::var(ENV_DEFAULTS_CONFIG).ok()?;
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<RebaseDefaults>(&raw) {
            Ok(d) => Some(d),
            Err(e) => {
                tracing::warn!(path = %path, error = %e, "malformed rebase defaults");
                None
            }
        },
        Err(e) => {
            tracing::warn!(path = %path, error = %e, "could not read rebase defaults");
            None
        }
    }
}

/// Path of the imperative state file inside the project store root.
pub fn state_path(root: &Path) -> std::path::PathBuf {
    root.join(STATE_FILE)
}

/// Path of the "run now" marker file inside the project store root.
pub fn run_now_path(root: &Path) -> std::path::PathBuf {
    root.join(RUN_NOW_FILE)
}

pub fn state_lock_path(root: &Path) -> std::path::PathBuf {
    root.join(STATE_LOCK_FILE)
}

/// Load the imperative state file (missing file → default state).
pub fn load_state(root: &Path) -> RebaseState {
    let path = state_path(root);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "malformed rebase state; ignoring");
            RebaseState::default()
        }),
        Err(_) => RebaseState::default(),
    }
}

/// Atomically persist the imperative state file.
pub fn save_state(root: &Path, state: &RebaseState) -> Result<()> {
    let path = state_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state)?;
    let tmp = path.with_file_name(format!(
        "{STATE_FILE}.tmp-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

/// Serialize an atomic state read-modify-write across omega-loop and
/// omega-git-host. A stale lock left by a killed process is reclaimed after
/// 30 seconds; normal contention waits for at most five seconds.
pub fn update_state<T>(
    root: &Path,
    update: impl FnOnce(&mut RebaseState) -> Result<T>,
) -> Result<T> {
    std::fs::create_dir_all(root)?;
    let lock_path = state_lock_path(root);
    let started = SystemTime::now();
    let lock = loop {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => break file,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&lock_path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| SystemTime::now().duration_since(m).ok())
                    .is_some_and(|age| age > Duration::from_secs(30));
                if stale {
                    let _ = std::fs::remove_file(&lock_path);
                    continue;
                }
                if SystemTime::now()
                    .duration_since(started)
                    .unwrap_or_default()
                    > Duration::from_secs(5)
                {
                    anyhow::bail!("timed out waiting for rebase state lock");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e.into()),
        }
    };

    let mut state = load_state(root);
    let result = update(&mut state).and_then(|value| {
        save_state(root, &state)?;
        Ok(value)
    });
    drop(lock);
    let _ = std::fs::remove_file(&lock_path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn defaults(interval: u64, projects: &[&str]) -> RebaseDefaults {
        RebaseDefaults {
            interval_seconds: interval,
            projects: projects.iter().map(|s| s.to_string()).collect(),
            agent_prompt: DEFAULT_REBASER_PROMPT.to_string(),
            update_instruction: DEFAULT_UPDATE_INSTRUCTION.to_string(),
        }
    }

    fn names(config: &EffectiveConfig) -> Vec<&str> {
        config.projects.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn effective_merges_defaults_with_state_overrides() {
        // NixOS defaults seed A + B; web UI adds C, disables B, overrides
        // the interval.
        let d = defaults(3600, &["alpha", "beta"]);
        let mut s = RebaseState {
            interval_seconds: Some(60),
            ..Default::default()
        };
        s.project_states.insert("beta".into(), false);
        s.project_states.insert("gamma".into(), true);

        let eff = resolve_effective(Some(&d), &s);
        assert_eq!(eff.interval_seconds, 60);
        assert_eq!(names(&eff), vec!["alpha", "gamma"]);
    }

    #[test]
    fn effective_without_defaults_uses_state_and_builtin_interval() {
        let mut s = RebaseState::default();
        s.project_states.insert("solo".into(), true);
        let eff = resolve_effective(None, &s);
        assert_eq!(names(&eff), vec!["solo"]);
        assert_eq!(eff.interval_seconds, DEFAULT_INTERVAL_SECONDS);
    }

    #[test]
    fn effective_empty_state_uses_defaults_verbatim() {
        let d = defaults(7200, &["x", "y"]);
        let eff = resolve_effective(Some(&d), &RebaseState::default());
        assert_eq!(names(&eff), vec!["x", "y"]);
        assert_eq!(eff.interval_seconds, 7200);
    }

    #[test]
    fn effective_deduplicates_and_sorts() {
        let d = defaults(100, &["b", "a", "b"]);
        let eff = resolve_effective(Some(&d), &RebaseState::default());
        assert_eq!(names(&eff), vec!["a", "b"]);
    }

    #[test]
    fn effective_zero_interval_falls_back_to_default() {
        let d = defaults(0, &["x"]);
        let eff = resolve_effective(Some(&d), &RebaseState::default());
        assert_eq!(eff.interval_seconds, DEFAULT_INTERVAL_SECONDS);
    }

    #[test]
    fn state_round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let mut s = RebaseState {
            interval_seconds: Some(300),
            last_run: Some(serde_json::json!({ "at": "2026-07-21T00:00:00Z" })),
            ..Default::default()
        };
        s.project_states.insert("omega".into(), true);
        save_state(tmp.path(), &s).unwrap();

        let loaded = load_state(tmp.path());
        assert_eq!(loaded.interval_seconds, Some(300));
        assert_eq!(loaded.project_states.get("omega"), Some(&true));
        assert!(loaded.last_run.is_some());
    }

    #[test]
    fn state_missing_file_loads_empty() {
        let tmp = TempDir::new().unwrap();
        let loaded = load_state(tmp.path());
        assert!(loaded.project_states.is_empty());
        assert!(loaded.last_run.is_none());
    }

    #[test]
    fn defaults_serde_fields_default_sensibly() {
        // Old/partial defaults files without the prompt fields still parse,
        // falling back to the built-in prompts.
        let raw = r#"{"interval_seconds": 60, "projects": ["a"]}"#;
        let d: RebaseDefaults = serde_json::from_str(raw).unwrap();
        assert_eq!(d.agent_prompt, DEFAULT_REBASER_PROMPT);
        assert_eq!(d.update_instruction, DEFAULT_UPDATE_INSTRUCTION);
    }

    #[test]
    fn per_project_schedule_and_prompt_override_global_defaults() {
        let d = defaults(3600, &["alpha"]);
        let mut state = RebaseState::default();
        state.project_configs.insert(
            "alpha".into(),
            RebaseProjectConfig {
                enabled: Some(true),
                interval_seconds: Some(90),
                prompt: Some("preserve our parser behavior".into()),
            },
        );
        let effective = resolve_effective(Some(&d), &state);
        assert_eq!(effective.projects[0].interval_seconds, 90);
        assert_eq!(effective.projects[0].prompt, "preserve our parser behavior");
    }

    #[test]
    fn persisted_start_time_drives_restart_safe_due_check() {
        let now = Utc::now();
        assert!(project_is_due(now, None, 3600));
        assert!(!project_is_due(
            now,
            Some(now - chrono::Duration::seconds(3599)),
            3600
        ));
        assert!(project_is_due(
            now,
            Some(now - chrono::Duration::seconds(3600)),
            3600
        ));
    }

    #[test]
    fn unresolved_run_pauses_only_the_automatic_schedule() {
        let mut run = RebaseProjectRun {
            result: Some(serde_json::json!({ "status": "conflicted" })),
            ..Default::default()
        };
        assert!(run_blocks_schedule(Some(&run)));
        // "verifying" also blocks (rebaser is working)
        run.result = Some(serde_json::json!({ "status": "verifying" }));
        assert!(run_blocks_schedule(Some(&run)));
        run.result = Some(serde_json::json!({ "status": "completed" }));
        assert!(!run_blocks_schedule(Some(&run)));
        run.pending_since = Some(Utc::now());
        assert!(run_blocks_schedule(Some(&run)));
    }

    #[test]
    fn interrupted_running_job_is_requeued_with_recovery_trigger() {
        let now = Utc::now();
        let mut state = RebaseState::default();
        state.project_runs.insert(
            "omega".into(),
            RebaseProjectRun {
                last_trigger: Some("schedule".into()),
                result: Some(serde_json::json!({ "status": "running" })),
                ..Default::default()
            },
        );
        state.project_runs.insert(
            "conflicted".into(),
            RebaseProjectRun {
                result: Some(serde_json::json!({ "status": "conflicted" })),
                ..Default::default()
            },
        );
        state.project_runs.insert(
            "verifying".into(),
            RebaseProjectRun {
                result: Some(serde_json::json!({ "status": "verifying" })),
                ..Default::default()
            },
        );

        assert_eq!(recover_interrupted_runs(&mut state, now), vec!["omega"]);
        let recovered = &state.project_runs["omega"];
        assert_eq!(recovered.pending_since, Some(now));
        assert_eq!(recovered.pending_trigger.as_deref(), Some("recovery"));
        assert_eq!(run_status(recovered), Some("queued"));
        assert_eq!(
            recovered.result.as_ref().unwrap()["outcome"],
            "recovered-after-restart"
        );
        assert_eq!(
            run_status(&state.project_runs["conflicted"]),
            Some("conflicted")
        );
        // "verifying" runs are not recovered here — the reconciliation
        // loop handles them in the daemon (via reconcile_conflicted_runs).
        assert_eq!(
            run_status(&state.project_runs["verifying"]),
            Some("verifying")
        );
    }

    #[test]
    fn conflicted_run_reconciliation_is_rate_limited() {
        let now = Utc::now();
        let mut run = RebaseProjectRun {
            result: Some(serde_json::json!({ "status": "conflicted" })),
            ..Default::default()
        };
        assert!(conflict_reconciliation_is_due(&run, now, 60));
        run.last_reconciled_at = Some(now - chrono::Duration::seconds(59));
        assert!(!conflict_reconciliation_is_due(&run, now, 60));
        run.last_reconciled_at = Some(now - chrono::Duration::seconds(60));
        assert!(conflict_reconciliation_is_due(&run, now, 60));
    }

    #[test]
    fn locked_update_preserves_config_and_run_fields() {
        let tmp = TempDir::new().unwrap();
        update_state(tmp.path(), |state| {
            state
                .project_configs
                .entry("omega".into())
                .or_default()
                .enabled = Some(true);
            Ok(())
        })
        .unwrap();
        update_state(tmp.path(), |state| {
            state
                .project_runs
                .entry("omega".into())
                .or_default()
                .pending_since = Some(Utc::now());
            Ok(())
        })
        .unwrap();
        let state = load_state(tmp.path());
        assert_eq!(state.project_configs["omega"].enabled, Some(true));
        assert!(state.project_runs["omega"].pending_since.is_some());
        assert!(!state_lock_path(tmp.path()).exists());
    }
}
