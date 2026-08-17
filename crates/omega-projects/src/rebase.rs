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
//!    the interval and toggles individual projects in or out of the
//!    cron-jobbable set. Also records `last_run` so the web UI can show what
//!    the cron did.
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
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Env var pointing at the defaults file written by the NixOS module.
pub const ENV_DEFAULTS_CONFIG: &str = "OMEGA_REBASE_JOB_CONFIG";

/// Imperative state file name, inside the project store root.
pub const STATE_FILE: &str = "rebase-job.json";

/// Marker file the web UI creates to ask the cron for an immediate run
/// ("run now"). Also inside the project store root.
pub const RUN_NOW_FILE: &str = "rebase-now";

/// Default interval (seconds) when neither defaults file nor state overrides
/// it: 6 hours.
pub const DEFAULT_INTERVAL_SECONDS: u64 = 6 * 3600;

/// Built-in instruction appended to the system prompt of every session that
/// enters a project: keep the checkout in sync with upstream. The NixOS
/// module's `rebaseJob.updatePrompt` default is the same text — keep the two
/// in sync.
pub const DEFAULT_UPDATE_INSTRUCTION: &str =
    "Keep your checkout up to date with upstream. Before starting work in this project, \
     update your checkout: run `git fetch origin` (if your worktree predates the latest \
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
     1. Run `git fetch origin` so the upstream refs are current.\n\
     2. Rebase the main branch onto the latest upstream (`git rebase origin/<default-branch>` \
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
    /// What the last scheduled run did, for the web UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<serde_json::Value>,
}

/// Merged configuration the cron actually acts on.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub interval_seconds: u64,
    /// Sorted, deduplicated list of cron-jobbable projects.
    pub projects: Vec<String>,
}

impl EffectiveConfig {
    /// True when the cron has at least one project to keep up to date.
    pub fn is_active(&self) -> bool {
        !self.projects.is_empty()
    }
}

/// Merge NixOS defaults with the imperative state (defaults win on the
/// interval only when the state does not set one; a `false` project entry
/// always wins over the defaults list).
pub fn resolve_effective(defaults: Option<&RebaseDefaults>, state: &RebaseState) -> EffectiveConfig {
    let interval = state
        .interval_seconds
        .or_else(|| defaults.map(|d| d.interval_seconds))
        .unwrap_or(DEFAULT_INTERVAL_SECONDS);

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

    EffectiveConfig {
        interval_seconds: if interval == 0 { DEFAULT_INTERVAL_SECONDS } else { interval },
        projects: set.into_keys().collect(),
    }
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
    let tmp = path.with_file_name(format!("{STATE_FILE}.tmp"));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
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
        assert_eq!(eff.projects, vec!["alpha", "gamma"]);
    }

    #[test]
    fn effective_without_defaults_uses_state_and_builtin_interval() {
        let mut s = RebaseState::default();
        s.project_states.insert("solo".into(), true);
        let eff = resolve_effective(None, &s);
        assert_eq!(eff.projects, vec!["solo"]);
        assert_eq!(eff.interval_seconds, DEFAULT_INTERVAL_SECONDS);
    }

    #[test]
    fn effective_empty_state_uses_defaults_verbatim() {
        let d = defaults(7200, &["x", "y"]);
        let eff = resolve_effective(Some(&d), &RebaseState::default());
        assert_eq!(eff.projects, vec!["x", "y"]);
        assert_eq!(eff.interval_seconds, 7200);
    }

    #[test]
    fn effective_deduplicates_and_sorts() {
        let d = defaults(100, &["b", "a", "b"]);
        let eff = resolve_effective(Some(&d), &RebaseState::default());
        assert_eq!(eff.projects, vec!["a", "b"]);
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
}