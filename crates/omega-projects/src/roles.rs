//! # Named roles — alternative system prompts
//!
//! A "role" is a named, NixOS-configured alternative system prompt.  When a
//! user starts a session with `/<role> <prompt>` in the TUI, the session is
//! created with that role's system prompt (instead of the default) and the
//! given text as its first input.  This lets the user define specialised
//! modes of the agent (e.g. `reverseengineer` with tailored instructions)
//! in their NixOS configuration.
//!
//! Roles are written by the NixOS module (`services.omega.roles`) to a JSON
//! file pointed at by `OMEGA_ROLES_PATH` (default `/etc/omega/roles.json`)
//! as an array of `{ name, system_prompt }` objects.  The daemon reads
//! these at startup and exposes the role *names* to clients (via the
//! `list_roles` request) so the TUI can wire up `/<role>` slash commands.
//! Only the name is sent to the client; the full system prompt stays on the
//! server, so it is not observable to the TUI process.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Env var pointing at the roles file written by the NixOS module.
pub const ENV_ROLES_CONFIG: &str = "OMEGA_ROLES_PATH";

/// One configured role: a name (the slash-command name) and the full
/// alternative system prompt to use when a session is started with it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    /// The name — used in the TUI as `/<name> <prompt>`.
    pub name: String,
    /// The alternative system prompt to use for the session.
    #[serde(default)]
    pub system_prompt: String,
}

/// Load the roles file referenced by `OMEGA_ROLES_PATH`, if set and
/// readable.  Absent/malformed config is a warning, never fatal — the
/// daemon then simply has no custom roles (only the default "no role"
/// prompt).
pub fn load_roles() -> Vec<Role> {
    let path = match std::env::var(ENV_ROLES_CONFIG).ok().filter(|p| !p.is_empty()) {
        Some(p) => p,
        None => return Vec::new(),
    };
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<Vec<Role>>(&raw) {
            Ok(roles) => roles
                .into_iter()
                .filter(|r| !r.name.trim().is_empty())
                .collect(),
            Err(e) => {
                tracing::warn!(path = %path, error = %e, "malformed roles config");
                Vec::new()
            }
        },
        Err(e) => {
            tracing::warn!(path = %path, error = %e, "could not read roles config");
            Vec::new()
        }
    }
}

/// Look up a role's system prompt by name.  Returns `None` when no role by
/// that name is configured, or when the name is the empty string.
pub fn role_prompt(roles: &[Role], name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    roles
        .iter()
        .find(|r| r.name.trim() == name)
        .map(|r| r.system_prompt.clone())
}

/// The sorted names of all configured roles (for the `list_roles` reply),
/// so clients can wire up `/<role>` slash commands.
pub fn role_names(roles: &[Role]) -> Vec<String> {
    let mut names: BTreeMap<String, ()> = roles
        .iter()
        .map(|r| (r.name.trim().to_string(), ()))
        .collect();
    names.remove("");
    names.into_keys().collect()
}
