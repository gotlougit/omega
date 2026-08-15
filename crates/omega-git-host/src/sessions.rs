//! Session index: maps git branches to the omega chat sessions that own them,
//! and provides the session listing + metadata used by the transcript pages.
//!
//! omega-loop persists each session's active project (including the worktree
//! branch) in `metadata.json` under `custom["active_project"]`. Reading that
//! gives an exact branch → session mapping — no branch-name parsing needed.
//!
//! Sessions live under `$OMEGA_SESSION_DIR` (default `sessions`), one
//! directory per session id, with `metadata.json` alongside `history.jsonl`
//! and `system_prompt.md`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Minimal view of a session's `metadata.json` (unknown fields ignored).
#[derive(Debug, Clone, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub conversation_name: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub child_session_ids: Vec<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub custom: HashMap<String, serde_json::Value>,
}

/// What the web UI needs to know about a session.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub display_name: String,
    pub agent: String,
    pub description: String,
    pub model: String,
    pub provider: String,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub parent: Option<String>,
    pub children: Vec<String>,
    /// The project + worktree branch this session is bound to, if any.
    pub project: Option<String>,
    pub branch: Option<String>,
}

impl SessionInfo {
    pub fn from_meta(meta: &SessionMeta) -> Self {
        let active = meta
            .custom
            .get("active_project")
            .and_then(|v| serde_json::from_value::<omega_projects::ActiveProject>(v.clone()).ok());
        Self {
            display_name: meta
                .conversation_name
                .clone()
                .unwrap_or_else(|| meta.session_id.clone()),
            session_id: meta.session_id.clone(),
            agent: meta.name.clone(),
            description: meta.description.clone(),
            model: meta.model.clone(),
            provider: meta.provider.clone(),
            created_at: meta.created_at,
            updated_at: meta.updated_at,
            parent: meta.parent_session_id.clone(),
            children: meta.child_session_ids.clone(),
            project: active.as_ref().map(|a| a.project.name.clone()),
            branch: active.as_ref().map(|a| a.branch.clone()),
        }
    }
}

/// Branch → session lookup plus the full session list, rebuilt from disk per
/// request (the store is tiny).
#[derive(Debug, Default)]
pub struct SessionIndex {
    by_branch: HashMap<String, SessionInfo>,
    all: Vec<SessionInfo>,
}

impl SessionIndex {
    /// Scan `dir` for session metadata. Unreadable or malformed sessions are
    /// skipped with a warning — the index never fails the page.
    pub fn load(dir: &Path) -> Result<Self> {
        let mut index = SessionIndex::default();
        if !dir.is_dir() {
            return Ok(index);
        }
        let entries = std::fs::read_dir(dir)
            .with_context(|| format!("failed to read session dir {}", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let meta_path = path.join("metadata.json");
            if !meta_path.is_file() {
                continue;
            }
            let meta: SessionMeta = match serde_json::from_slice(&std::fs::read(&meta_path)?) {
                Ok(meta) => meta,
                Err(e) => {
                    tracing::warn!(
                        session = %path.display(),
                        error = %e,
                        "skipping unreadable session metadata"
                    );
                    continue;
                }
            };
            let info = SessionInfo::from_meta(&meta);
            if let Some(branch) = &info.branch {
                index
                    .by_branch
                    .entry(branch.clone())
                    .or_insert(info.clone());
            }
            index.all.push(info);
        }
        Ok(index)
    }

    pub fn by_branch(&self, branch: &str) -> Option<&SessionInfo> {
        self.by_branch.get(branch)
    }

    pub fn all(&self) -> &[SessionInfo] {
        &self.all
    }
}
