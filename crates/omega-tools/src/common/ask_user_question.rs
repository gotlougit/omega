//! AskUserQuestion tool for interactive user queries
//!
//! This tool allows the agent to ask the user questions and wait for responses.
//! Questions are sent via `OutputChunk::AskUserQuestion`, and the tool waits
//! for the user's answers via `InputMessage::UserQuestionResponse`.

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use omega_core::core::{QuestionOption, ToolInfo, ToolResult, UserQuestion};
use omega_core::core::ToolRuntime;
use super::super::tool::Tool;
use omega_llm::ToolDefinition;

/// Input for a single question option
#[derive(Debug, Deserialize)]
struct QuestionOptionInput {
    label: String,
    description: String,
}

/// Input for a single question
#[derive(Debug, Deserialize)]
struct QuestionInput {
    question: String,
    header: String,
    options: Vec<QuestionOptionInput>,
    #[serde(rename = "multiSelect", default)]
    multi_select: bool,
}

/// Input for the AskUserQuestion tool
#[derive(Debug, Deserialize)]
struct AskUserQuestionInput {
    questions: Vec<QuestionInput>,
    /// Pre-filled answers (optional, not typically used)
    #[serde(default)]
    _answers: Option<std::collections::HashMap<String, String>>,
}

/// AskUserQuestion tool for interacting with users
///
/// This tool allows the agent to ask the user questions with multiple-choice
/// options and receive their responses.
pub struct AskUserQuestionTool;

impl AskUserQuestionTool {
    /// Create a new AskUserQuestion tool
    pub fn new() -> Self {
        Self
    }
}

impl Default for AskUserQuestionTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        "AskUserQuestion"
    }

    fn description(&self) -> &str {
        "Ask the user questions to gather information, clarify requirements, or get decisions."
    }

    fn definition(&self) -> ToolDefinition {
        crate::def_to_tool_definition(&omega_tool_defs::ask_user_question::DEF)
    }

    fn get_info(&self, input: &Value) -> ToolInfo {
        let question_count = input
            .get("questions")
            .and_then(|v| v.as_array())
            .map(|arr| arr.len())
            .unwrap_or(0);

        ToolInfo {
            name: "AskUserQuestion".to_string(),
            action_description: format!("Ask user {} question(s)", question_count),
            details: None,
        }
    }

    async fn execute(&self, input: &Value, rt: &mut dyn ToolRuntime) -> Result<ToolResult> {
        // Parse the input
        let ask_input: AskUserQuestionInput = serde_json::from_value(input.clone())
            .map_err(|e| anyhow::anyhow!("Invalid AskUserQuestion input: {}", e))?;

        // Validate: 1-4 questions
        if ask_input.questions.is_empty() {
            return Ok(ToolResult::error("At least one question is required"));
        }
        if ask_input.questions.len() > 4 {
            return Ok(ToolResult::error("Maximum of 4 questions allowed"));
        }

        // Validate each question has 2-4 options
        for (i, q) in ask_input.questions.iter().enumerate() {
            if q.options.len() < 2 {
                return Ok(ToolResult::error(format!(
                    "Question {} ('{}') must have at least 2 options",
                    i + 1,
                    q.header
                )));
            }
            if q.options.len() > 4 {
                return Ok(ToolResult::error(format!(
                    "Question {} ('{}') can have at most 4 options",
                    i + 1,
                    q.header
                )));
            }
        }

        // Convert to UserQuestion format
        let questions: Vec<UserQuestion> = ask_input
            .questions
            .into_iter()
            .map(|q| UserQuestion {
                question: q.question,
                header: q.header,
                options: q
                    .options
                    .into_iter()
                    .map(|o| QuestionOption {
                        label: o.label,
                        description: o.description,
                    })
                    .collect(),
                multi_select: q.multi_select,
            })
            .collect();

        // Generate a unique request ID
        let request_id = format!("ask_{}", uuid::Uuid::new_v4());

        // Call the helper method to ask questions and wait for response
        match rt.ask_user_question(&request_id, questions).await {
            Ok(answers) => {
                // Format answers as JSON for the tool result
                let answers_json = serde_json::to_string_pretty(&answers)
                    .unwrap_or_else(|_| format!("{:?}", answers));
                Ok(ToolResult::success(format!(
                    "User responded with the following answers:\n{}",
                    answers_json
                )))
            }
            Err(omega_core::core::FrameworkError::Interrupted) => {
                Ok(ToolResult::error("User interrupted the question"))
            }
            Err(omega_core::core::FrameworkError::Shutdown) => {
                Ok(ToolResult::error("Shutdown requested"))
            }
            Err(omega_core::core::FrameworkError::ChannelClosed) => Ok(ToolResult::error(
                "Connection closed before receiving response",
            )),
            Err(e) => Ok(ToolResult::error(format!(
                "Failed to get user response: {}",
                e
            ))),
        }
    }

}
