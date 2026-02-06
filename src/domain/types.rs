//! Core domain types for hook input/output.

use serde::{Deserialize, Serialize};

/// Hook event type.
///
/// Represents the type of hook event in an agent-agnostic way.
/// Each AI coding agent uses different event names externally,
/// but internally we use this unified enum for type safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// Before command execution (target for rm, kill blocking).
    ///
    /// External event names:
    /// - Claude Code: `PreToolUse`
    /// - Cursor: `ShellExecution`
    /// - Windsurf: `pre_run_command`
    /// - Gemini CLI: `BeforeTool`
    BeforeCommand,

    /// After file edit (target for extension hooks).
    ///
    /// External event names:
    /// - Claude Code: `PostToolUse`
    /// - Cursor: `FileEdit`
    /// - Windsurf: `post_write_code`
    /// - Gemini CLI: `AfterTool`
    AfterFileEdit,

    /// Agent loop stopped.
    ///
    /// External event names:
    /// - Claude Code: `Stop`
    /// - Cursor: `Stop`
    /// - Windsurf: `post_cascade_response`
    /// - Gemini CLI: `AfterAgent`
    Stop,

    /// Before user prompt submission (Gemini CLI only).
    ///
    /// External event names:
    /// - Gemini CLI: `BeforeAgent`
    BeforePrompt,
}

/// Hook input received from AI agent.
///
/// Note: This struct is not directly deserializable from JSON.
/// Use `FormatAdapter::parse_input()` to convert agent-specific
/// JSON formats into this internal representation.
#[derive(Debug, Clone)]
pub struct HookInput {
    /// Event type (agent-agnostic).
    pub event: HookEvent,

    /// Tool name: "Bash", "Write", "Edit", "MultiEdit", "Read", etc.
    pub tool_name: String,

    /// Tool-specific input
    pub tool_input: ToolInput,

    /// Optional session identifier
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

    /// Whether stop hooks are already active (prevents infinite loops)
    #[serde(default)]
    pub stop_hook_active: bool,
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

    /// Convert decision to HookOutput for the given event.
    pub fn into_output(self, event: HookEvent) -> HookOutput {
        match self {
            Decision::Allow { additional_context } => {
                // Only include hookSpecificOutput for AfterFileEdit (PostToolUse in Claude Code)
                let hook_specific_output = if event == HookEvent::AfterFileEdit {
                    additional_context.map(|ctx| HookSpecificOutput {
                        // External format uses "PostToolUse" for Claude Code compatibility
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_event_equality() {
        assert_eq!(HookEvent::BeforeCommand, HookEvent::BeforeCommand);
        assert_eq!(HookEvent::AfterFileEdit, HookEvent::AfterFileEdit);
        assert_eq!(HookEvent::Stop, HookEvent::Stop);
        assert_eq!(HookEvent::BeforePrompt, HookEvent::BeforePrompt);

        assert_ne!(HookEvent::BeforeCommand, HookEvent::AfterFileEdit);
        assert_ne!(HookEvent::BeforeCommand, HookEvent::Stop);
        assert_ne!(HookEvent::BeforeCommand, HookEvent::BeforePrompt);
    }

    #[test]
    fn test_hook_event_copy() {
        let event = HookEvent::BeforeCommand;
        let copied = event; // Copy, not move
        assert_eq!(event, copied);
        // Both can be used after copy
        assert_eq!(event, HookEvent::BeforeCommand);
        assert_eq!(copied, HookEvent::BeforeCommand);
    }

    #[test]
    fn test_hook_event_clone() {
        let event = HookEvent::AfterFileEdit;
        // Explicitly test Clone trait (not just Copy)
        let cloned = Clone::clone(&event);
        assert_eq!(event, cloned);
    }

    #[test]
    fn test_hook_event_debug() {
        assert_eq!(format!("{:?}", HookEvent::BeforeCommand), "BeforeCommand");
        assert_eq!(format!("{:?}", HookEvent::AfterFileEdit), "AfterFileEdit");
        assert_eq!(format!("{:?}", HookEvent::Stop), "Stop");
        assert_eq!(format!("{:?}", HookEvent::BeforePrompt), "BeforePrompt");
    }

    // Decision::into_output() tests

    #[test]
    fn test_decision_into_output_allow_before_command() {
        let decision = Decision::allow();
        let output = decision.into_output(HookEvent::BeforeCommand);

        assert_eq!(output.decision, "approve");
        assert!(output.message.is_none());
        // No hookSpecificOutput for BeforeCommand
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_decision_into_output_allow_after_file_edit_no_context() {
        let decision = Decision::allow();
        let output = decision.into_output(HookEvent::AfterFileEdit);

        assert_eq!(output.decision, "approve");
        assert!(output.message.is_none());
        // No hookSpecificOutput when no additional context
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_decision_into_output_allow_after_file_edit_with_context() {
        let decision = Decision::allow_with_context("Lint warning: unused variable".to_string());
        let output = decision.into_output(HookEvent::AfterFileEdit);

        assert_eq!(output.decision, "approve");
        assert!(output.message.is_none());
        // hookSpecificOutput should be present for AfterFileEdit with context
        let hook_output = output
            .hook_specific_output
            .expect("Should have hookSpecificOutput");
        assert_eq!(hook_output.hook_event_name, "PostToolUse");
        assert_eq!(
            hook_output.additional_context,
            Some("Lint warning: unused variable".to_string())
        );
    }

    #[test]
    fn test_decision_into_output_allow_with_context_before_command() {
        // Context is ignored for BeforeCommand (only AfterFileEdit supports it)
        let decision = Decision::allow_with_context("Some context".to_string());
        let output = decision.into_output(HookEvent::BeforeCommand);

        assert_eq!(output.decision, "approve");
        // hookSpecificOutput should NOT be present for BeforeCommand
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_decision_into_output_block() {
        let decision = Decision::Block {
            message: "Command blocked for safety".to_string(),
        };
        let output = decision.into_output(HookEvent::BeforeCommand);

        assert_eq!(output.decision, "block");
        assert_eq!(
            output.message,
            Some("Command blocked for safety".to_string())
        );
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_decision_into_output_stop_event() {
        let decision = Decision::allow();
        let output = decision.into_output(HookEvent::Stop);

        assert_eq!(output.decision, "approve");
        // No hookSpecificOutput for Stop event
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_decision_into_output_before_prompt_event() {
        let decision = Decision::allow();
        let output = decision.into_output(HookEvent::BeforePrompt);

        assert_eq!(output.decision, "approve");
        // No hookSpecificOutput for BeforePrompt event
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_decision_exit_code() {
        let allow = Decision::allow();
        assert_eq!(allow.exit_code(), 0);

        let block = Decision::Block {
            message: "blocked".to_string(),
        };
        assert_eq!(block.exit_code(), 2);
    }
}
