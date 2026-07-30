//! Tool system for the omega agent framework
//!
//! This crate provides:
//! - `Tool` trait — interface for implementing tools
//! - `ToolRegistry` — registry for managing available tools
//! - Built-in tool implementations (Bash, Read, Write, Edit, AskUserQuestion, Transfer)

mod registry;
mod tool;

/// Common/built-in tools
pub mod common;

pub use registry::ToolRegistry;
pub use tool::Tool;

// Re-export common tools for convenience
pub use common::{AskUserQuestionTool, BashTool, EditTool, ReadTool, TransferTool, WriteTool};

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // def_to_tool_definition
    // -----------------------------------------------------------------------

    /// Every canonical def round-trips through def_to_tool_definition
    /// with the correct name and description.
    #[test]
    fn test_def_to_tool_definition_preserves_name_and_description() {
        for &def in omega_tool_defs::ALL {
            let td = def_to_tool_definition(def);
            let ToolDefinition::Custom(custom) = &td;
            assert_eq!(custom.name, def.name);
            assert_eq!(
                custom.description.as_deref(),
                Some(def.description),
                "description mismatch for '{}'",
                def.name
            );
        }
    }

    /// The input_schema produced by def_to_tool_definition must be valid JSON
    /// and structurally match the original schema.
    #[test]
    fn test_def_to_tool_definition_schema_round_trip() {
        for &def in omega_tool_defs::ALL {
            let td = def_to_tool_definition(def);
            let ToolDefinition::Custom(custom) = &td;
            let original: serde_json::Value = serde_json::from_str(def.input_schema_json).unwrap();
            // The CustomTool.input_schema is a ToolInputSchema which
            // serializes as a JSON object. Re-parse to compare.
            let parsed: serde_json::Value = serde_json::to_value(&custom.input_schema).unwrap();
            assert_eq!(
                original, parsed,
                "schema for '{}' changed through conversion",
                def.name
            );
        }
    }

    /// def_to_tool_definition always produces the Custom variant.
    #[test]
    fn test_def_to_tool_definition_is_always_custom() {
        for &def in omega_tool_defs::ALL {
            let td = def_to_tool_definition(def);
            assert!(
                matches!(td, ToolDefinition::Custom(_)),
                "'{}' produced unexpected variant",
                def.name
            );
        }
    }

    /// To-string consistent: convert twice with same input → identical output.
    #[test]
    fn test_def_to_tool_definition_deterministic() {
        let def = &omega_tool_defs::bash::DEF;
        let a = def_to_tool_definition(def);
        let b = def_to_tool_definition(def);
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::to_value(&b).unwrap()
        );
    }

    // -----------------------------------------------------------------------
    // Native tool definitions match canonical defs
    // -----------------------------------------------------------------------

    /// AskUserQuestionTool.definition() must produce the same result as
    /// def_to_tool_definition(&omega_tool_defs::ask_user_question::DEF).
    #[test]
    fn test_ask_user_question_definition_matches_canonical() {
        let tool = AskUserQuestionTool::new();
        let tool_def = tool.definition();
        let canonical = def_to_tool_definition(&omega_tool_defs::ask_user_question::DEF);
        assert_eq!(
            serde_json::to_value(&tool_def).unwrap(),
            serde_json::to_value(&canonical).unwrap(),
            "AskUserQuestionTool.definition() differs from canonical def"
        );
    }

    /// TransferTool.definition() must produce the same result as
    /// def_to_tool_definition(&omega_tool_defs::transfer::DEF).
    #[test]
    fn test_transfer_definition_matches_canonical() {
        let tool = TransferTool::new();
        let tool_def = tool.definition();
        let canonical = def_to_tool_definition(&omega_tool_defs::transfer::DEF);
        assert_eq!(
            serde_json::to_value(&tool_def).unwrap(),
            serde_json::to_value(&canonical).unwrap(),
            "TransferTool.definition() differs from canonical def"
        );
    }

    // -----------------------------------------------------------------------
    // register_default_tools
    // -----------------------------------------------------------------------

    /// register_default_tools must register AskUserQuestion and Transfer.
    #[test]
    fn test_register_default_tools_registers_native_tools() {
        let mut registry = ToolRegistry::new();
        register_default_tools(&mut registry);

        assert!(
            registry.get("AskUserQuestion").is_some(),
            "AskUserQuestion should be registered"
        );
        assert!(
            registry.get("Transfer").is_some(),
            "Transfer should be registered"
        );
    }

    /// register_default_tools must NOT register proxy tools.
    #[test]
    fn test_register_default_tools_does_not_register_proxies() {
        let mut registry = ToolRegistry::new();
        register_default_tools(&mut registry);

        assert!(
            registry.get("Bash").is_none(),
            "Bash should not be registered by default"
        );
        assert!(
            registry.get("Read").is_none(),
            "Read should not be registered by default"
        );
    }

    /// register_default_tools should not silently drop existing registrations.
    /// Uses `BashTool` as a pre-existing registration (it's a proxy tool so
    /// register_default_tools shouldn't touch it).
    #[test]
    fn test_register_default_tools_preserves_existing() {
        let mut registry = ToolRegistry::new();
        // BashTool is a local Tool impl that we can instantiate
        registry.register(BashTool::new().unwrap());
        assert!(registry.get("Bash").is_some());

        register_default_tools(&mut registry);

        // Bash still present
        assert!(
            registry.get("Bash").is_some(),
            "existing Bash registration was lost"
        );
        // Native tools now present too
        assert!(
            registry.get("AskUserQuestion").is_some(),
            "AskUserQuestion should have been added"
        );
        assert!(
            registry.get("Transfer").is_some(),
            "Transfer should have been added"
        );
    }
}

use omega_llm::{types::CustomTool, ToolDefinition};

/// Convert a canonical `ToolDef` into a `ToolDefinition` for the LLM.
///
/// This is the single conversion point used by both native tool
/// implementations and proxy tools.
pub fn def_to_tool_definition(def: &omega_tool_defs::ToolDef) -> ToolDefinition {
    ToolDefinition::Custom(CustomTool {
        name: def.name.to_string(),
        description: Some(def.description.to_string()),
        input_schema: serde_json::from_str(def.input_schema_json)
            .expect("invalid ToolInputSchema in ToolDef"),
        tool_type: None,
        cache_control: None,
    })
}

/// Register all default in-process tools into the given registry.
///
/// These tools run directly in the agent process and don't need
/// an external executor like omega-sh.
pub fn register_default_tools(registry: &mut ToolRegistry) {
    // In-process tools that don't need omega-sh
    registry.register(AskUserQuestionTool::new());
    registry.register(TransferTool::new());

    // Auto-register any future native tools from the canonical defs.
    // Native tools that need special runtime access (like AskUserQuestion
    // and Transfer) are registered above; the loop catches any that
    // don't need special setup but are marked Native in omega-tool-defs.
    for &def in omega_tool_defs::ALL {
        if def.kind == omega_tool_defs::ToolKind::Native {
            // Check if already registered by name to avoid duplicates.
            if registry.get(def.name).is_none() {
                // TODO: create a generic NativeTool wrapper for tools
                // whose execute() lives in omega-tools.
                tracing::warn!(
                    "Native tool '{}' is defined in omega-tool-defs but has no in-process implementation yet",
                    def.name
                );
            }
        }
    }
}
