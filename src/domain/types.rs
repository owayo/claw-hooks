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
    /// Decision: "allow" or "block"
    pub decision: String,

    /// Optional message (usually present when blocking)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Processing decision with optional block message.
#[derive(Debug, Clone)]
pub enum Decision {
    /// Allow the operation
    Allow,
    /// Block the operation with a message
    Block { message: String },
}

impl Decision {
    /// Convert decision to HookOutput.
    pub fn into_output(self) -> HookOutput {
        match self {
            Decision::Allow => HookOutput {
                decision: "approve".to_string(),
                message: None,
            },
            Decision::Block { message } => HookOutput {
                decision: "block".to_string(),
                message: Some(message),
            },
        }
    }

    /// Get exit code for this decision.
    ///
    /// - Allow: 0
    /// - Block: 2
    pub fn exit_code(&self) -> i32 {
        match self {
            Decision::Allow => 0,
            Decision::Block { .. } => 2,
        }
    }
}
