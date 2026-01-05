//! Format adapters for different AI coding agents.
//!
//! This module provides input parsing and output formatting for:
//! - Claude Code (default)
//! - Cursor
//! - Windsurf (Cascade)

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::cli::Format;
use crate::domain::{Decision, HookInput};

/// Adapter for converting between format-specific I/O and internal types.
pub struct FormatAdapter {
    format: Format,
}

impl FormatAdapter {
    /// Create a new adapter for the specified format.
    pub fn new(format: Format) -> Self {
        Self { format }
    }

    /// Parse input string to HookInput based on format.
    pub fn parse_input(&self, input: &str) -> Result<HookInput> {
        match self.format {
            Format::Claude => self.parse_claude_input(input),
            Format::Cursor => self.parse_cursor_input(input),
            Format::Windsurf => self.parse_windsurf_input(input),
        }
    }

    /// Format output based on the agent format.
    pub fn format_output(&self, decision: &Decision) -> Result<String> {
        match self.format {
            Format::Claude => self.format_claude_output(decision),
            Format::Cursor => self.format_cursor_output(decision),
            Format::Windsurf => self.format_windsurf_output(decision),
        }
    }

    /// Get the exit code for the decision.
    /// Note: Cursor uses different semantics but still uses exit codes.
    pub fn exit_code(&self, decision: &Decision) -> i32 {
        decision.exit_code()
    }

    /// Format an error message for output.
    /// This is used when input parsing fails.
    /// SECURITY: Uses fail-closed design - parse errors result in blocking.
    pub fn format_error(&self, message: &str) -> String {
        let error_message = format!("🚫 Hook error (fail-closed): {}", message);
        match self.format {
            Format::Claude | Format::Windsurf => {
                // Claude and Windsurf use the same format with decision and message
                // SECURITY: Block on parse errors (fail-closed design)
                serde_json::json!({
                    "decision": "block",
                    "message": error_message
                })
                .to_string()
            }
            Format::Cursor => {
                // Cursor uses permission and user_message
                // SECURITY: Deny on parse errors (fail-closed design)
                serde_json::json!({
                    "permission": "deny",
                    "user_message": error_message,
                    "agent_message": "Hook system encountered an error and blocked for safety"
                })
                .to_string()
            }
        }
    }

    /// Get the exit code for error scenarios (fail-closed = block = exit 2).
    pub fn error_exit_code(&self) -> i32 {
        2 // Same as Decision::Block exit code
    }

    // === Claude Code Format ===

    fn parse_claude_input(&self, input: &str) -> Result<HookInput> {
        debug!(raw_input = %input, "Claude raw input");

        let claude_input: ClaudeInput = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Claude input: {}", e))?;

        let event = claude_input.hook_event_name.clone();

        // Handle Stop event specially (no tool_name or tool_input)
        let (tool_name, tool_input) = if event == "Stop" {
            (
                "Stop".to_string(),
                crate::domain::ToolInput::Stop(crate::domain::StopInput {
                    status: None,
                    loop_count: None,
                    response: None,
                }),
            )
        } else {
            let tool_name = claude_input
                .tool_name
                .ok_or_else(|| anyhow!("Missing tool_name field"))?;
            let tool_input = claude_input
                .tool_input
                .ok_or_else(|| anyhow!("Missing tool_input field"))?;
            (tool_name, tool_input)
        };

        debug!(
            format = "claude",
            event = %event,
            tool_name = %tool_name,
            "Parsed Claude Code input"
        );

        Ok(HookInput {
            event,
            tool_name,
            tool_input,
            session_id: claude_input.session_id,
        })
    }

    fn format_claude_output(&self, decision: &Decision) -> Result<String> {
        let output = decision.clone().into_output();
        serde_json::to_string(&output).map_err(|e| anyhow!("Failed to serialize output: {}", e))
    }

    // === Cursor Format ===

    fn parse_cursor_input(&self, input: &str) -> Result<HookInput> {
        debug!(raw_input = %input, "Cursor raw input");

        let cursor_input: CursorInput = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Cursor input: {}", e))?;

        // Convert Cursor format to internal HookInput based on hook type
        match cursor_input {
            CursorInput::Stop { status, loop_count } => {
                debug!(
                    format = "cursor",
                    hook_type = "stop",
                    status = %status,
                    loop_count = ?loop_count,
                    mapped_event = "Stop",
                    "Parsed Cursor input"
                );

                // Cursor's stop hook is equivalent to Stop event
                Ok(HookInput {
                    event: "Stop".to_string(),
                    tool_name: "Stop".to_string(),
                    tool_input: crate::domain::ToolInput::Stop(crate::domain::StopInput {
                        status: Some(status),
                        loop_count,
                        response: None,
                    }),
                    session_id: None,
                })
            }
            CursorInput::ShellExecution { command, cwd } => {
                debug!(
                    format = "cursor",
                    hook_type = "beforeShellExecution",
                    command = %command,
                    cwd = ?cwd,
                    mapped_event = "PreToolUse",
                    mapped_tool = "Bash",
                    "Parsed Cursor input"
                );

                // Cursor's beforeShellExecution is equivalent to PreToolUse for Bash
                Ok(HookInput {
                    event: "PreToolUse".to_string(),
                    tool_name: "Bash".to_string(),
                    tool_input: crate::domain::ToolInput::Bash(crate::domain::BashInput {
                        command,
                        timeout: None,
                    }),
                    session_id: None,
                })
            }
            CursorInput::FileEdit { file_path } => {
                debug!(
                    format = "cursor",
                    hook_type = "afterFileEdit",
                    file_path = %file_path,
                    mapped_event = "PostToolUse",
                    mapped_tool = "Write",
                    "Parsed Cursor input"
                );

                // Cursor's afterFileEdit is equivalent to PostToolUse for Write
                Ok(HookInput {
                    event: "PostToolUse".to_string(),
                    tool_name: "Write".to_string(),
                    tool_input: crate::domain::ToolInput::File(crate::domain::FileOperationInput {
                        file_path,
                        content: None,
                    }),
                    session_id: None,
                })
            }
        }
    }

    fn format_cursor_output(&self, decision: &Decision) -> Result<String> {
        let output = match decision {
            Decision::Allow => CursorOutput {
                permission: "allow".to_string(),
                user_message: None,
                agent_message: None,
            },
            Decision::Block { message } => CursorOutput {
                permission: "deny".to_string(),
                user_message: Some(message.clone()),
                agent_message: Some("Command blocked by claw-hooks".to_string()),
            },
        };
        serde_json::to_string(&output)
            .map_err(|e| anyhow!("Failed to serialize Cursor output: {}", e))
    }

    // === Windsurf Format ===

    fn parse_windsurf_input(&self, input: &str) -> Result<HookInput> {
        debug!(raw_input = %input, "Windsurf raw input");

        let windsurf_input: WindsurfInput = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Windsurf input: {}", e))?;

        // Map Windsurf agent_action_name to internal event type
        let (event, tool_name, tool_input) = match windsurf_input.agent_action_name.as_str() {
            "pre_run_command" => {
                let command = windsurf_input
                    .tool_info
                    .as_ref()
                    .and_then(|ti| ti.command_line.clone())
                    .unwrap_or_default();
                (
                    "PreToolUse".to_string(),
                    "Bash".to_string(),
                    crate::domain::ToolInput::Bash(crate::domain::BashInput {
                        command,
                        timeout: None,
                    }),
                )
            }
            "post_write_code" => {
                let file_path = windsurf_input
                    .tool_info
                    .as_ref()
                    .and_then(|ti| ti.file_path.clone())
                    .unwrap_or_default();
                (
                    "PostToolUse".to_string(),
                    "Write".to_string(),
                    crate::domain::ToolInput::File(crate::domain::FileOperationInput {
                        file_path,
                        content: None,
                    }),
                )
            }
            "post_cascade_response" => {
                let response = windsurf_input
                    .tool_info
                    .as_ref()
                    .and_then(|ti| ti.response.clone());
                (
                    "Stop".to_string(),
                    "Stop".to_string(),
                    crate::domain::ToolInput::Stop(crate::domain::StopInput {
                        status: None,
                        loop_count: None,
                        response,
                    }),
                )
            }
            other => {
                // Unknown action, pass through as-is
                (
                    other.to_string(),
                    "Unknown".to_string(),
                    crate::domain::ToolInput::Other(serde_json::json!({})),
                )
            }
        };

        debug!(
            format = "windsurf",
            agent_action_name = %windsurf_input.agent_action_name,
            mapped_event = %event,
            mapped_tool = %tool_name,
            cwd = ?windsurf_input.tool_info.as_ref().and_then(|ti| ti.cwd.as_ref()),
            "Parsed Windsurf input"
        );

        Ok(HookInput {
            event,
            tool_name,
            tool_input,
            session_id: None,
        })
    }

    fn format_windsurf_output(&self, decision: &Decision) -> Result<String> {
        // Windsurf uses the same output format as Claude Code
        self.format_claude_output(decision)
    }
}

// === Claude Code Format Types ===

/// Claude Code input format (latest specification).
/// See: https://docs.anthropic.com/en/docs/claude-code/hooks
#[derive(Debug, Deserialize)]
struct ClaudeInput {
    /// Hook event name: PreToolUse, PostToolUse, Stop, etc.
    hook_event_name: String,

    /// Tool name (optional for Stop/Notification events)
    #[serde(default)]
    tool_name: Option<String>,

    /// Tool input (optional for Stop/Notification events)
    #[serde(default)]
    tool_input: Option<crate::domain::ToolInput>,

    /// Session identifier
    #[serde(default)]
    session_id: Option<String>,

    /// Whether stop hooks are active in this session
    #[serde(default)]
    #[allow(dead_code)]
    stop_hook_active: Option<bool>,
}

// === Cursor Format Types ===

/// Cursor input format - supports beforeShellExecution, afterFileEdit, and stop hooks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CursorInput {
    /// stop hook - agent loop has ended
    Stop {
        /// Stop status: "completed", "aborted", or "error"
        status: String,
        /// Number of auto-followups triggered in this conversation
        #[serde(default)]
        loop_count: Option<u32>,
    },
    /// beforeShellExecution hook - provides command to execute
    ShellExecution {
        /// Command to execute
        command: String,
        /// Current working directory
        #[serde(default)]
        #[allow(dead_code)]
        cwd: Option<String>,
    },
    /// afterFileEdit hook - provides edited file path
    FileEdit {
        /// Path of the edited file
        #[serde(alias = "filePath")]
        file_path: String,
    },
}

/// Cursor output format.
#[derive(Debug, Serialize)]
struct CursorOutput {
    /// Permission: "allow", "deny", or "ask"
    permission: String,
    /// Message shown to user (when denied)
    #[serde(skip_serializing_if = "Option::is_none")]
    user_message: Option<String>,
    /// Message for the agent (when denied)
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_message: Option<String>,
}

// === Windsurf Format Types ===

/// Windsurf input format.
#[derive(Debug, Deserialize)]
struct WindsurfInput {
    /// Action name: "pre_run_command", "post_write_code", etc.
    agent_action_name: String,
    /// Tool-specific information
    #[serde(default)]
    tool_info: Option<WindsurfToolInfo>,
}

/// Windsurf tool info.
#[derive(Debug, Default, Deserialize)]
struct WindsurfToolInfo {
    /// Command line for pre_run_command
    #[serde(default)]
    command_line: Option<String>,
    /// Current working directory
    #[serde(default)]
    #[allow(dead_code)]
    cwd: Option<String>,
    /// File path for post_write_code
    #[serde(default)]
    file_path: Option<String>,
    /// Response content for post_cascade_response
    #[serde(default)]
    response: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_input_parsing() {
        let adapter = FormatAdapter::new(Format::Claude);
        let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls -la"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, "PreToolUse");
        assert_eq!(result.tool_name, "Bash");
    }

    #[test]
    fn test_cursor_input_parsing_shell_execution() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let input = r#"{"command":"rm -rf /tmp/test","cwd":"/path/to/project"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, "PreToolUse");
        assert_eq!(result.tool_name, "Bash");
        if let crate::domain::ToolInput::Bash(bash) = &result.tool_input {
            assert_eq!(bash.command, "rm -rf /tmp/test");
        } else {
            panic!("Expected Bash tool input");
        }
    }

    #[test]
    fn test_cursor_input_parsing_file_edit() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let input = r#"{"file_path":"/path/to/file.rs"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, "PostToolUse");
        assert_eq!(result.tool_name, "Write");
        if let crate::domain::ToolInput::File(file) = &result.tool_input {
            assert_eq!(file.file_path, "/path/to/file.rs");
        } else {
            panic!("Expected File tool input");
        }
    }

    #[test]
    fn test_cursor_input_parsing_file_edit_camel_case() {
        let adapter = FormatAdapter::new(Format::Cursor);
        // Test with camelCase filePath (Cursor might use either)
        let input = r#"{"filePath":"/path/to/file.tsx"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, "PostToolUse");
        assert_eq!(result.tool_name, "Write");
        if let crate::domain::ToolInput::File(file) = &result.tool_input {
            assert_eq!(file.file_path, "/path/to/file.tsx");
        } else {
            panic!("Expected File tool input");
        }
    }

    #[test]
    fn test_windsurf_input_parsing_pre_run_command() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        let input = r#"{"agent_action_name":"pre_run_command","tool_info":{"command_line":"rm -rf /tmp/test","cwd":"/path/to/project"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, "PreToolUse");
        assert_eq!(result.tool_name, "Bash");
        if let crate::domain::ToolInput::Bash(bash) = &result.tool_input {
            assert_eq!(bash.command, "rm -rf /tmp/test");
        } else {
            panic!("Expected Bash tool input");
        }
    }

    #[test]
    fn test_windsurf_input_parsing_post_write_code() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        let input = r#"{"agent_action_name":"post_write_code","tool_info":{"file_path":"/path/to/file.rs"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, "PostToolUse");
        assert_eq!(result.tool_name, "Write");
    }

    #[test]
    fn test_cursor_output_allow() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let output = adapter.format_output(&Decision::Allow).unwrap();
        assert!(output.contains(r#""permission":"allow""#));
    }

    #[test]
    fn test_cursor_output_deny() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let output = adapter
            .format_output(&Decision::Block {
                message: "Command blocked for safety".to_string(),
            })
            .unwrap();
        assert!(output.contains(r#""permission":"deny""#));
        assert!(output.contains("Command blocked for safety"));
    }

    #[test]
    fn test_claude_output_allow() {
        let adapter = FormatAdapter::new(Format::Claude);
        let output = adapter.format_output(&Decision::Allow).unwrap();
        assert!(output.contains(r#""decision":"approve""#));
    }

    #[test]
    fn test_claude_output_block() {
        let adapter = FormatAdapter::new(Format::Claude);
        let output = adapter
            .format_output(&Decision::Block {
                message: "Command blocked for safety".to_string(),
            })
            .unwrap();
        assert!(output.contains(r#""decision":"block""#));
        assert!(output.contains("Command blocked for safety"));
    }

    #[test]
    fn test_cursor_input_parsing_stop() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let input = r#"{"status":"completed","loop_count":3}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, "Stop");
        assert_eq!(result.tool_name, "Stop");
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(stop.status, Some("completed".to_string()));
            assert_eq!(stop.loop_count, Some(3));
            assert!(stop.response.is_none());
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_cursor_input_parsing_stop_aborted() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let input = r#"{"status":"aborted"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, "Stop");
        assert_eq!(result.tool_name, "Stop");
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(stop.status, Some("aborted".to_string()));
            assert!(stop.loop_count.is_none());
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_windsurf_input_parsing_post_cascade_response() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        let input = r#"{"agent_action_name":"post_cascade_response","tool_info":{"response":"Task completed successfully."}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, "Stop");
        assert_eq!(result.tool_name, "Stop");
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert!(stop.status.is_none());
            assert!(stop.loop_count.is_none());
            assert_eq!(
                stop.response,
                Some("Task completed successfully.".to_string())
            );
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_stop() {
        let adapter = FormatAdapter::new(Format::Claude);
        // Stop events have no tool_name or tool_input
        let input = r#"{"hook_event_name":"Stop","stop_hook_active":true}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, "Stop");
        assert_eq!(result.tool_name, "Stop");
    }
}
