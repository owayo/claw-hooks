//! Core domain types for hook input/output.

use serde::{Deserialize, Serialize};

/// Hook input received from AI agent.
#[derive(Debug, Clone, Deserialize)]
pub struct HookInput {
    /// Event type: "PreToolUse", "PostToolUse", "Stop"
    pub event: String,

    /// Tool name: "Bash", "Write", "Edit", "MultiEdit", "Read", etc.
    pub tool_name: String,

    /// Tool-specific input
    pub tool_input: ToolInput,

    /// Optional session identifier
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Tool-specific input variants.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ToolInput {
    /// Bash command input
    Bash(BashInput),
    /// File operation input (Write, Edit, MultiEdit, Read)
    File(FileOperationInput),
    /// Stop event input (agent loop ended)
    #[allow(dead_code)]
    Stop(StopInput),
    /// Other/unknown tool input
    #[allow(dead_code)]
    Other(serde_json::Value),
}

/// Bash command input.
#[derive(Debug, Clone, Deserialize)]
pub struct BashInput {
    /// Command to execute
    pub command: String,

    /// Optional timeout in milliseconds
    #[serde(default)]
    #[allow(dead_code)]
    pub timeout: Option<u64>,
}

/// File operation input.
#[derive(Debug, Clone, Deserialize)]
pub struct FileOperationInput {
    /// File path
    pub file_path: String,

    /// Optional content (for Write/Edit)
    #[serde(default)]
    #[allow(dead_code)]
    pub content: Option<String>,
}

/// Stop event input.
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub struct StopInput {
    /// Stop status (Cursor: "completed", "aborted", "error")
    #[serde(default)]
    pub status: Option<String>,

    /// Loop count (Cursor: number of auto-followups triggered)
    #[serde(default)]
    pub loop_count: Option<u32>,

    /// Response content (Windsurf: full cascade response)
    #[serde(default)]
    pub response: Option<String>,
}

/// Hook output sent back to AI agent.
#[derive(Debug, Clone, Serialize)]
pub struct HookOutput {
    /// Decision: "approve" or "block"
    pub decision: String,

    /// Optional message (usually present when blocking)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// Hook-specific output for Claude Code (PostToolUse additionalContext)
    #[serde(rename = "hookSpecificOutput", skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

/// Hook-specific output for Claude Code PostToolUse.
#[derive(Debug, Clone, Serialize)]
pub struct HookSpecificOutput {
    /// Hook event name
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,

    /// Additional context for the agent (e.g., lint warnings)
    #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

/// Processing decision with optional block message.
#[derive(Debug, Clone)]
pub enum Decision {
    /// Allow the operation with optional context for the agent
    Allow {
        /// Additional context to pass to the agent (e.g., lint warnings)
        additional_context: Option<String>,
    },
    /// Block the operation with a message
    Block { message: String },
}

impl Default for Decision {
    fn default() -> Self {
        Decision::Allow {
            additional_context: None,
        }
    }
}

impl Decision {
    /// Create an Allow decision with no additional context.
    pub fn allow() -> Self {
        Decision::Allow {
            additional_context: None,
        }
    }

    /// Create an Allow decision with additional context for the agent.
    pub fn allow_with_context(context: String) -> Self {
        Decision::Allow {
            additional_context: Some(context),
        }
    }

    /// Convert decision to HookOutput for PostToolUse event.
    pub fn into_output(self, event: &str) -> HookOutput {
        match self {
            Decision::Allow { additional_context } => {
                let hook_specific_output = if event == "PostToolUse" {
                    additional_context.map(|ctx| HookSpecificOutput {
                        hook_event_name: "PostToolUse".to_string(),
                        additional_context: Some(ctx),
                    })
                } else {
                    None
                };

                HookOutput {
                    decision: "approve".to_string(),
                    message: None,
                    hook_specific_output,
                }
            }
            Decision::Block { message } => HookOutput {
                decision: "block".to_string(),
                message: Some(message),
                hook_specific_output: None,
            },
        }
    }

    /// Get exit code for this decision.
    ///
    /// - Allow: 0
    /// - Block: 2
    pub fn exit_code(&self) -> i32 {
        match self {
            Decision::Allow { .. } => 0,
            Decision::Block { .. } => 2,
        }
    }

    /// Merge additional context from another decision.
    /// If both have context, they are joined with newlines.
    #[allow(dead_code)]
    pub fn merge_context(self, other_context: Option<String>) -> Self {
        match self {
            Decision::Allow { additional_context } => {
                let merged = match (additional_context, other_context) {
                    (Some(a), Some(b)) => Some(format!("{}\n{}", a, b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
                Decision::Allow {
                    additional_context: merged,
                }
            }
            Decision::Block { message } => Decision::Block { message },
        }
    }
}
