//! Canonical tool definitions shared across the omega stack.
//!
//! Every tool has a single source of truth here: its name, description,
//! and input JSON Schema.  Consumers in `omega-sh` (dispatch + execution),
//! `omega-sh-client` (proxy tools), and `omega-tools` (native tools) all
//! reference these definitions so that adding or changing a tool touches
//! only one place.

use std::sync::LazyLock;

/// Whether a tool is handled by `omega-sh` or runs in-process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// Forwarded to the `omega-sh` daemon for execution.
    Proxy,
    /// Executed directly by `omega-loop` (no daemon needed).
    Native,
}

/// Canonical definition of a tool.
pub struct ToolDef {
    /// Human-readable tool name (e.g. `"Read"`, `"Bash"`).
    pub name: &'static str,
    /// Description of what the tool does.
    pub description: &'static str,
    /// Input JSON Schema as a serialized string.
    ///
    /// Consumers parse this once (at startup) into the type they need.
    /// Keeping it as a `&str` avoids pulling `serde_json::Value` into the
    /// type and keeps `ToolDef` copyable.
    pub input_schema_json: &'static str,
    /// How this tool is executed (proxy vs native).
    pub kind: ToolKind,
}

// ---------------------------------------------------------------------------
// Macro to define one tool module
// ---------------------------------------------------------------------------

macro_rules! tool {
    ($mod:ident, $kind:ident, $name:expr, $desc:expr, $($schema_body:tt)+) => {
        pub mod $mod {
            use std::sync::LazyLock;

            /// Canonical tool definition, lazily constructed.
            pub static DEF: LazyLock<super::ToolDef> = LazyLock::new(|| {
                let val = serde_json::json!($($schema_body)+);
                super::ToolDef {
                    name: $name,
                    description: $desc,
                    input_schema_json: serde_json::to_string(&val).unwrap().leak(),
                    kind: super::ToolKind::$kind,
                }
            });
        }
    };
}

// ============================================================================
// Proxy tools  –  executed by omega-sh
// ============================================================================

tool!(bash, Proxy, "Bash",
    "Execute a bash command in the shell. Use for terminal operations like git, npm, docker, etc.",
    {
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The command to execute"
            },
            "timeout": {
                "type": "number",
                "description": "Optional timeout in milliseconds (max 600000). Default is 120000ms (2 minutes)."
            },
            "description": {
                "type": "string",
                "description": "Clear, concise description of what this command does in 5-10 words, in active voice."
            }
        },
        "required": ["command"]
    }
);

tool!(read, Proxy, "Read",
    "Read a file from the local filesystem. Supports text files, images (PNG, JPEG, GIF, WebP), and PDFs.",
    {
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": "The absolute path to the file to read"
            },
            "offset": {
                "type": "number",
                "description": "The line number to start reading from (1-indexed)"
            },
            "limit": {
                "type": "number",
                "description": "The number of lines to read"
            }
        },
        "required": ["file_path"]
    }
);

tool!(write, Proxy, "Write",
    "Write content to a file on the local filesystem.",
    {
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": "The absolute path to the file to write (must be absolute, not relative)"
            },
            "content": {
                "type": "string",
                "description": "The content to write to the file"
            }
        },
        "required": ["file_path", "content"]
    }
);

tool!(edit, Proxy, "Edit",
    "Perform exact string replacements in files.",
    {
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": "The absolute path to the file to modify"
            },
            "old_string": {
                "type": "string",
                "description": "The text to replace"
            },
            "new_string": {
                "type": "string",
                "description": "The text to replace it with (must be different from old_string)"
            },
            "replace_all": {
                "type": "boolean",
                "default": false,
                "description": "Replace all occurrences of old_string (default false)"
            }
        },
        "required": ["file_path", "old_string", "new_string"]
    }
);

// ============================================================================
// In-process tools  –  executed by omega-loop directly
// ============================================================================

tool!(transfer, Native, "Transfer",
    "Use this tool to transfer a file from the agent's environment to the user.\n\
     The file content will be saved as a file on the user's local machine.\n\n\
     Typical usage:\n\
     1. Generate or create a file (e.g. a patch, result, or artifact)\n\
     2. Pass the absolute path to the file as `file_path`\n\n\
     The file will be saved with a name like `<original-name>-<session>-<timestamp>`.",
    {
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": "Absolute path to the file on the agent's filesystem to transfer"
            }
        },
        "required": ["file_path"]
    }
);

// ---------------------------------------------------------------------------
// Convenience
// ---------------------------------------------------------------------------

/// All canonical tool definitions, in a consistent order.
pub const ALL: &[&LazyLock<ToolDef>] = &[
    &bash::DEF,
    &read::DEF,
    &write::DEF,
    &edit::DEF,
    &transfer::DEF,
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Every canonical definition must have a valid JSON schema.
    #[test]
    fn test_all_schemas_are_valid_json() {
        for &def in ALL {
            let result: Result<serde_json::Value, _> = serde_json::from_str(def.input_schema_json);
            assert!(
                result.is_ok(),
                "schema for '{}' is not valid JSON: {}",
                def.name,
                result.unwrap_err()
            );
        }
    }

    /// Every schema must contain a "type" field set to "object".
    #[test]
    fn test_all_schemas_are_objects() {
        for &def in ALL {
            let v: serde_json::Value = serde_json::from_str(def.input_schema_json).unwrap();
            assert_eq!(
                v.get("type").and_then(|t| t.as_str()),
                Some("object"),
                "schema for '{}' must have type \"object\"",
                def.name
            );
        }
    }

    /// Every schema must define at least one property.
    #[test]
    fn test_all_schemas_have_properties() {
        for &def in ALL {
            let v: serde_json::Value = serde_json::from_str(def.input_schema_json).unwrap();
            assert!(
                v.get("properties").and_then(|p| p.as_object()).is_some(),
                "schema for '{}' must have a 'properties' object",
                def.name
            );
            let props = v["properties"].as_object().unwrap();
            assert!(
                !props.is_empty(),
                "schema for '{}' must have at least one property",
                def.name
            );
        }
    }

    /// Every def must have a non-empty name.
    #[test]
    fn test_all_names_non_empty() {
        for &def in ALL {
            assert!(!def.name.is_empty(), "all defs must have a non-empty name");
        }
    }

    /// Every def must have a non-empty description.
    #[test]
    fn test_all_descriptions_non_empty() {
        for &def in ALL {
            assert!(
                !def.description.is_empty(),
                "def '{}' must have a non-empty description",
                def.name
            );
        }
    }

    /// All tool names must be unique (case-sensitive).
    #[test]
    fn test_no_duplicate_names() {
        let mut seen = std::collections::HashSet::new();
        for &def in ALL {
            assert!(seen.insert(def.name), "duplicate tool name '{}'", def.name);
        }
    }

    /// Proxy tools are those executed by omega-sh.
    #[test]
    fn test_proxy_tool_kinds() {
        let proxies: std::collections::HashSet<&str> = ["Bash", "Read", "Write", "Edit"].into();
        for &def in ALL {
            if proxies.contains(def.name) {
                assert_eq!(
                    def.kind,
                    ToolKind::Proxy,
                    "'{}' should be a Proxy tool",
                    def.name
                );
            }
        }
    }

    /// Native tools are those executed in-process.
    #[test]
    fn test_native_tool_kinds() {
        let natives: std::collections::HashSet<&str> = ["Transfer"].into();
        for &def in ALL {
            if natives.contains(def.name) {
                assert_eq!(
                    def.kind,
                    ToolKind::Native,
                    "'{}' should be a Native tool",
                    def.name
                );
            }
        }
    }

    /// No tool should be unclassified (every name maps to exactly one kind).
    #[test]
    fn test_all_kinds_covered() {
        let classified: std::collections::HashSet<&str> =
            ["Bash", "Read", "Write", "Edit", "Transfer"].into();
        for &def in ALL {
            assert!(
                classified.contains(def.name),
                "tool '{}' is not in any expected-kind list",
                def.name
            );
        }
    }

    /// The ALL list has the expected count.
    #[test]
    fn test_all_count() {
        assert_eq!(ALL.len(), 5);
    }

    /// Schema with required fields: every required property must also
    /// appear in the properties object.
    #[test]
    fn test_required_properties_exist() {
        for &def in ALL {
            let v: serde_json::Value = serde_json::from_str(def.input_schema_json).unwrap();
            if let Some(required) = v.get("required").and_then(|r| r.as_array()) {
                let props = v["properties"].as_object().unwrap();
                for field in required {
                    let name = field.as_str().unwrap_or("?");
                    assert!(
                        props.contains_key(name),
                        "schema for '{}': required field '{}' is not in properties",
                        def.name,
                        name
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    /// Schemas round-trip through JSON: parse → serialize produces valid JSON
    /// with the same structure.
    #[test]
    fn test_schema_round_trip() {
        for &def in ALL {
            let original: serde_json::Value = serde_json::from_str(def.input_schema_json).unwrap();
            let serialized = serde_json::to_string(&original).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
            assert_eq!(
                original, parsed,
                "round-trip for '{}' did not preserve schema",
                def.name
            );
        }
    }

    /// A ToolDef with only one property (Transfer) has a valid schema.
    #[test]
    fn test_transfer_single_property() {
        let v: serde_json::Value = serde_json::from_str(transfer::DEF.input_schema_json).unwrap();
        let props = v["properties"].as_object().unwrap();
        assert_eq!(props.len(), 1, "Transfer has {} properties", props.len());
        assert!(props.contains_key("file_path"));
        assert_eq!(v["required"][0].as_str(), Some("file_path"));
    }
}
