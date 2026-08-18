//! Session listing types shared between the daemon and clients.

use serde::{Deserialize, Serialize};

/// A named alternative system prompt ("role") that a client can start a
/// session with via `/<role> <prompt>`.
///
/// This is the client-facing summary of a role — only the name (used as the
/// slash-command name) is exposed to clients; the full system prompt stays
/// server-side in the daemon / NixOS module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleInfo {
    /// Role name — used as the slash-command name in the TUI (`/name …`).
    pub name: String,
}

/// A lightweight, client-facing summary of a stored session.
///
/// This is what the daemon returns for `list_sessions`. The list is
/// sorted most-recently-updated first so clients can present a
/// "nearest first" resume picker without doing their own ordering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Unique session ID (used with `/resume <id>`).
    pub session_id: String,

    /// Human-readable agent name (from session metadata).
    pub name: String,

    /// Auto-generated conversation name, if the session has been named.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_name: Option<String>,

    /// RFC3339 creation timestamp.
    pub created_at: String,

    /// RFC3339 last-updated timestamp (sort key for recency).
    pub updated_at: String,

    /// Number of persisted messages in the session.
    pub message_count: usize,

    /// Short text preview of the last meaningful message (for scanning).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message: Option<String>,

    /// Short text preview of the first user prompt in the session. This is
    /// what a resume picker shows on its single-line entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_user_message: Option<String>,
}

impl SessionInfo {
    /// A searchable display title: conversation name if present, else the
    /// session ID (so anonymous sessions are still findable).
    pub fn title(&self) -> &str {
        self.conversation_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&self.session_id)
    }

    /// Case-insensitive search across every field a user might type.
    pub fn matches_query(&self, query: &str) -> bool {
        if query.trim().is_empty() {
            return true;
        }
        let q = query.trim().to_lowercase();
        self.session_id.to_lowercase().contains(&q)
            || self.name.to_lowercase().contains(&q)
            || self
                .conversation_name
                .as_deref()
                .map(|s| s.to_lowercase().contains(&q))
                .unwrap_or(false)
            || self
                .last_message
                .as_deref()
                .map(|s| s.to_lowercase().contains(&q))
                .unwrap_or(false)
            || self
                .first_user_message
                .as_deref()
                .map(|s| s.to_lowercase().contains(&q))
                .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, conv: Option<&str>, last: Option<&str>) -> SessionInfo {
        SessionInfo {
            session_id: id.to_string(),
            name: "omega-tui".to_string(),
            conversation_name: conv.map(|s| s.to_string()),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
            message_count: 3,
            last_message: last.map(|s| s.to_string()),
            first_user_message: None,
        }
    }

    #[test]
    fn title_prefers_conversation_name() {
        let s = info("sess-1", Some("Fix the build"), Some("hello"));
        assert_eq!(s.title(), "Fix the build");
    }

    #[test]
    fn title_falls_back_to_id() {
        let s = info("sess-1", None, Some("hello"));
        assert_eq!(s.title(), "sess-1");
        let s = info("sess-2", Some("   "), None);
        assert_eq!(s.title(), "sess-2");
    }

    #[test]
    fn empty_query_matches_everything() {
        let s = info("sess-1", None, None);
        assert!(s.matches_query(""));
        assert!(s.matches_query("   "));
    }

    #[test]
    fn query_matches_session_id() {
        let s = info("tui-abc123", None, None);
        assert!(s.matches_query("abc123"));
        assert!(!s.matches_query("zzz"));
    }

    #[test]
    fn query_matches_conversation_name_case_insensitive() {
        let s = info("sess-1", Some("Fix the Build"), None);
        assert!(s.matches_query("fix the"));
        assert!(s.matches_query("BUILD"));
        assert!(!s.matches_query("test"));
    }

    #[test]
    fn query_matches_last_message() {
        let s = info("sess-1", None, Some("Refactor the TUI tests"));
        assert!(s.matches_query("tui tests"));
        assert!(!s.matches_query("boring"));
    }

    #[test]
    fn serde_round_trip() {
        let s = info("sess-1", Some("A chat"), Some("hi there"));
        let json = serde_json::to_string(&s).unwrap();
        let back: SessionInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, "sess-1");
        assert_eq!(back.conversation_name.as_deref(), Some("A chat"));
        assert_eq!(back.last_message.as_deref(), Some("hi there"));
    }

    #[test]
    fn serde_omits_none_fields() {
        let s = info("sess-1", None, None);
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("conversation_name"));
        assert!(!json.contains("last_message"));
    }
}
