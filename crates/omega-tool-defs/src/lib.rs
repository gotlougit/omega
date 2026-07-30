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

tool!(glob, Proxy, "Glob",
    "Fast file pattern matching tool. Supports glob patterns like **/*.js or src/**/*.ts.",
    {
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "The glob pattern to match files against"
            },
            "path": {
                "type": "string",
                "description": "The directory to search in. If not specified, uses the daemon's current working directory."
            }
        },
        "required": ["pattern"]
    }
);

tool!(grep, Proxy, "Grep",
    "Search file contents using regex patterns. Uses ripgrep for fast searching.",
    {
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "The regular expression pattern to search for in file contents"
            },
            "path": {
                "type": "string",
                "description": "File or directory to search in. Defaults to daemon's working directory."
            },
            "glob": {
                "type": "string",
                "description": "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\")"
            },
            "output_mode": {
                "type": "string",
                "enum": ["content", "files_with_matches", "count"],
                "description": "Output mode: 'content', 'files_with_matches' (default), or 'count'"
            },
            "-B": {
                "type": "number",
                "description": "Number of lines to show before each match"
            },
            "-A": {
                "type": "number",
                "description": "Number of lines to show after each match"
            },
            "-C": {
                "type": "number",
                "description": "Number of lines to show before and after each match"
            },
            "-n": {
                "type": "boolean",
                "description": "Show line numbers in output. Defaults to true."
            },
            "-i": {
                "type": "boolean",
                "description": "Case insensitive search"
            },
            "type": {
                "type": "string",
                "description": "File type to search (e.g. 'js', 'py', 'rust')"
            },
            "head_limit": {
                "type": "number",
                "description": "Limit output to first N lines/entries"
            },
            "offset": {
                "type": "number",
                "description": "Skip first N lines/entries"
            },
            "multiline": {
                "type": "boolean",
                "description": "Enable multiline mode where . matches newlines"
            }
        },
        "required": ["pattern"]
    }
);

// ============================================================================
// In-process tools  –  executed by omega-loop directly
// ============================================================================

tool!(ask_user_question, Native, "AskUserQuestion",
    "Use this tool to ask the user questions during execution. This allows you to:\n\
     1. Gather user preferences or requirements\n\
     2. Clarify ambiguous instructions\n\
     3. Get decisions on implementation choices as you work\n\
     4. Offer choices to the user about what direction to take.\n\n\
     Usage notes:\n\
     - Users will always be able to select \"Other\" to provide custom text input\n\
     - Use multiSelect: true to allow multiple answers to be selected for a question\n\
     - If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label",
    {
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "description": "Questions to ask the user (1-4 questions)",
                "minItems": 1,
                "maxItems": 4,
                "items": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The complete question to ask the user. Should be clear, specific, and end with a question mark."
                        },
                        "header": {
                            "type": "string",
                            "description": "Very short label displayed as a chip/tag (max 12 chars). Examples: \"Auth method\", \"Library\", \"Approach\"."
                        },
                        "options": {
                            "type": "array",
                            "description": "The available choices for this question. Must have 2-4 options.",
                            "minItems": 2,
                            "maxItems": 4,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {
                                        "type": "string",
                                        "description": "The display text for this option (1-5 words)."
                                    },
                                    "description": {
                                        "type": "string",
                                        "description": "Explanation of what this option means or what will happen if chosen."
                                    }
                                },
                                "required": ["label", "description"]
                            }
                        },
                        "multiSelect": {
                            "type": "boolean",
                            "default": false,
                            "description": "Set to true to allow the user to select multiple options."
                        }
                    },
                    "required": ["question", "header", "options", "multiSelect"]
                }
            },
            "answers": {
                "type": "object",
                "description": "Optional pre-filled answers (header -> selected label)",
                "additionalProperties": {
                    "type": "string"
                }
            }
        },
        "required": ["questions"]
    }
);

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
    &glob::DEF,
    &grep::DEF,
    &ask_user_question::DEF,
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
        let proxies: std::collections::HashSet<&str> =
            ["Bash", "Read", "Write", "Edit", "Glob", "Grep"].into();
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
        let natives: std::collections::HashSet<&str> = ["AskUserQuestion", "Transfer"].into();
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
        let classified: std::collections::HashSet<&str> = [
            "Bash",
            "Read",
            "Write",
            "Edit",
            "Glob",
            "Grep",
            "AskUserQuestion",
            "Transfer",
        ]
        .into();
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
        assert_eq!(ALL.len(), 8);
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

    /// Bash schema: the longest schema still parses correctly.
    #[test]
    fn test_grep_schema_parses() {
        let v: serde_json::Value = serde_json::from_str(grep::DEF.input_schema_json).unwrap();
        let props = v["properties"].as_object().unwrap();
        // Grep has many properties — at least 10
        assert!(
            props.len() >= 10,
            "Grep schema has {} properties",
            props.len()
        );
    }

    /// AskUserQuestion schema: includes special characters (newlines, quotes).
    #[test]
    fn test_ask_user_description_has_special_chars() {
        let desc = ask_user_question::DEF.description;
        assert!(
            desc.contains("\"Other\""),
            "description should contain escaped quotes"
        );
        assert!(desc.contains("\n"), "description should contain newlines");
    }

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

    /// Proxy tools must have a non-empty description.
    #[test]
    fn test_proxy_descriptions_describe_delegation() {
        for &def in ALL {
            if def.kind == ToolKind::Proxy {
                assert!(
                    def.description.len() > 20,
                    "Proxy tool '{}' has a very short description ({})",
                    def.name,
                    def.description.len()
                );
            }
        }
    }
}
