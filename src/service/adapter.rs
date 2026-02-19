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
use crate::domain::{normalize_lint_output, Decision, HookEvent, HookInput};

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
            Format::Gemini => self.parse_gemini_input(input),
        }
    }

    /// Format output based on the agent format.
    /// The event parameter is used to include hookSpecificOutput for Claude Code AfterFileEdit.
    pub fn format_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
        match self.format {
            Format::Claude => self.format_claude_output(decision, event),
            Format::Cursor => self.format_cursor_output(decision, event),
            Format::Windsurf => self.format_windsurf_output(decision, event),
            Format::Gemini => self.format_gemini_output(decision),
        }
    }

    /// Get the exit code for the decision.
    /// Note: Different agents use different exit code semantics.
    /// - Claude/Windsurf: 0 = allow, 2 = block
    /// - Cursor: 0 = allow/stop, 2 = block (non-stop)
    /// - Gemini CLI: 0 = success (decision in JSON), 2 = system error only
    pub fn exit_code(&self, decision: &Decision, event: HookEvent) -> i32 {
        match self.format {
            Format::Gemini => {
                // Gemini CLI: Always return 0 for successful JSON output.
                // The decision (allow/deny) is communicated via the JSON response.
                // Exit code 2 is reserved for system errors (stderr used as reason).
                0
            }
            Format::Cursor if event == HookEvent::Stop => {
                // Cursor Stop: decision communicated via followup_message in JSON.
                0
            }
            _ => decision.exit_code(),
        }
    }

    /// Whether the output should be written to stderr instead of stdout.
    /// Windsurf Stop Block uses stderr for error output.
    pub fn use_stderr(&self, decision: &Decision, event: HookEvent) -> bool {
        matches!(
            (&self.format, event, decision),
            (&Format::Windsurf, HookEvent::Stop, Decision::Block { .. })
        )
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
            Format::Gemini => {
                // Gemini uses decision and reason
                // SECURITY: Deny on parse errors (fail-closed design)
                serde_json::json!({
                    "decision": "deny",
                    "reason": error_message
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
        debug!(raw_input = %input, "🤖 Claude Code raw input");

        let claude_input: ClaudeInput = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Claude input: {}", e))?;

        let raw_event = claude_input.hook_event_name.clone();

        // Map Claude Code event names to HookEvent
        let event = match raw_event.as_str() {
            "PreToolUse" => HookEvent::BeforeCommand,
            "PostToolUse" => HookEvent::AfterFileEdit,
            "Stop" => HookEvent::Stop,
            "SubagentStart" => HookEvent::SubagentStart,
            "SubagentStop" => HookEvent::SubagentStop,
            other => return Err(anyhow!("Unknown Claude event: {}", other)),
        };

        // Handle Stop event specially (no tool_name or tool_input)
        let (tool_name, tool_input) = if event == HookEvent::Stop {
            (
                "Stop".to_string(),
                crate::domain::ToolInput::Stop(crate::domain::StopInput {
                    status: None,
                    loop_count: None,
                    response: None,
                    agent_message: claude_input.last_assistant_message.clone(),
                    stop_hook_active: claude_input.stop_hook_active.unwrap_or(false),
                }),
            )
        } else if event == HookEvent::SubagentStart || event == HookEvent::SubagentStop {
            // SubagentStart/SubagentStop: extract subagent info from raw JSON
            // NOTE: We re-parse from raw input because serde(untagged) ToolInput
            // may deserialize the tool_input object as Stop(StopInput) instead of
            // Other(Value), since StopInput has all-optional fields.
            let raw: serde_json::Value = serde_json::from_str(input)
                .map_err(|e| anyhow!("Failed to re-parse raw input: {}", e))?;
            // agent_type はルートレベルまたは tool_input 内にある場合がある
            let root_agent_type = raw
                .get("agent_type")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            let subagent_input = if let Some(val) = raw.get("tool_input") {
                let tool_input_type = val
                    .get("subagent_type")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                crate::domain::SubagentInput {
                    // root-level agent_type (human-readable name like "Explore") takes priority
                    // over tool_input.subagent_type (may contain session ID/UUID)
                    subagent_type: root_agent_type.or(tool_input_type),
                    prompt: val.get("prompt").and_then(|v| v.as_str()).map(String::from),
                    status: val.get("status").and_then(|v| v.as_str()).map(String::from),
                    duration: val.get("duration").and_then(|v| v.as_u64()),
                }
            } else {
                crate::domain::SubagentInput {
                    subagent_type: root_agent_type,
                    ..Default::default()
                }
            };
            let tool_name = if event == HookEvent::SubagentStart {
                "SubagentStart"
            } else {
                "SubagentStop"
            };
            (
                tool_name.to_string(),
                crate::domain::ToolInput::Subagent(subagent_input),
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
            event = ?event,
            tool_name = %tool_name,
            "🤖 Claude Code parsed input"
        );

        Ok(HookInput {
            event,
            tool_name,
            tool_input,
            session_id: claude_input.session_id,
        })
    }

    fn format_claude_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
        // Stop events use "reason" instead of "message" for Block decisions
        if event == HookEvent::Stop {
            let output = match decision {
                Decision::Allow { .. } => ClaudeStopOutput {
                    decision: "approve".to_string(),
                    reason: None,
                },
                Decision::Block { message } => ClaudeStopOutput {
                    decision: "block".to_string(),
                    reason: Some(normalize_lint_output(message)),
                },
            };
            return serde_json::to_string(&output)
                .map_err(|e| anyhow!("Failed to serialize output: {}", e));
        }

        let output = decision.clone().into_output(event);
        serde_json::to_string(&output).map_err(|e| anyhow!("Failed to serialize output: {}", e))
    }

    // === Cursor Format ===

    fn parse_cursor_input(&self, input: &str) -> Result<HookInput> {
        debug!(raw_input = %input, "🖱️ Cursor raw input");

        let cursor_input: CursorInput = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Cursor input: {}", e))?;

        // Convert Cursor format to internal HookInput based on hook type
        match cursor_input {
            CursorInput::SubagentStart {
                subagent_type,
                prompt,
                ..
            } => {
                debug!(
                    format = "cursor",
                    hook_type = "subagentStart",
                    subagent_type = %subagent_type,
                    mapped_event = ?HookEvent::SubagentStart,
                    "🖱️ Cursor parsed input"
                );

                Ok(HookInput {
                    event: HookEvent::SubagentStart,
                    tool_name: "SubagentStart".to_string(),
                    tool_input: crate::domain::ToolInput::Subagent(crate::domain::SubagentInput {
                        subagent_type: Some(subagent_type),
                        prompt,
                        status: None,
                        duration: None,
                    }),
                    session_id: None,
                })
            }
            CursorInput::SubagentStop {
                subagent_type,
                subagent_status,
                duration,
                ..
            } => {
                debug!(
                    format = "cursor",
                    hook_type = "subagentStop",
                    subagent_type = %subagent_type,
                    status = %subagent_status,
                    mapped_event = ?HookEvent::SubagentStop,
                    "🖱️ Cursor parsed input"
                );

                Ok(HookInput {
                    event: HookEvent::SubagentStop,
                    tool_name: "SubagentStop".to_string(),
                    tool_input: crate::domain::ToolInput::Subagent(crate::domain::SubagentInput {
                        subagent_type: Some(subagent_type),
                        prompt: None,
                        status: Some(subagent_status),
                        duration,
                    }),
                    session_id: None,
                })
            }
            CursorInput::Stop { status, loop_count } => {
                debug!(
                    format = "cursor",
                    hook_type = "stop",
                    status = %status,
                    loop_count = ?loop_count,
                    mapped_event = ?HookEvent::Stop,
                    "🖱️ Cursor parsed input"
                );

                // Cursor's stop hook is equivalent to Stop event
                Ok(HookInput {
                    event: HookEvent::Stop,
                    tool_name: "Stop".to_string(),
                    tool_input: crate::domain::ToolInput::Stop(crate::domain::StopInput {
                        status: Some(status),
                        loop_count,
                        response: None,
                        agent_message: None,
                        stop_hook_active: false,
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
                    mapped_event = ?HookEvent::BeforeCommand,
                    mapped_tool = "Bash",
                    "🖱️ Cursor parsed input"
                );

                // Cursor's beforeShellExecution is equivalent to BeforeCommand for Bash
                Ok(HookInput {
                    event: HookEvent::BeforeCommand,
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
                    mapped_event = ?HookEvent::AfterFileEdit,
                    mapped_tool = "Write",
                    "🖱️ Cursor parsed input"
                );

                // Cursor's afterFileEdit is equivalent to AfterFileEdit for Write
                Ok(HookInput {
                    event: HookEvent::AfterFileEdit,
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

    fn format_cursor_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
        // Stop Block events use "followup_message" to instruct the agent to fix issues
        if event == HookEvent::Stop {
            if let Decision::Block { message } = decision {
                let output = CursorStopOutput {
                    followup_message: normalize_lint_output(message),
                };
                return serde_json::to_string(&output)
                    .map_err(|e| anyhow!("Failed to serialize Cursor output: {}", e));
            }
        }

        let output = match decision {
            Decision::Allow { .. } => CursorOutput {
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
        debug!(raw_input = %input, "🏄 Windsurf raw input");

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
                    HookEvent::BeforeCommand,
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
                    HookEvent::AfterFileEdit,
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
                    HookEvent::Stop,
                    "Stop".to_string(),
                    crate::domain::ToolInput::Stop(crate::domain::StopInput {
                        status: None,
                        loop_count: None,
                        agent_message: response.clone(),
                        response,
                        stop_hook_active: false,
                    }),
                )
            }
            other => {
                return Err(anyhow!("Unknown Windsurf action: {}", other));
            }
        };

        debug!(
            format = "windsurf",
            agent_action_name = %windsurf_input.agent_action_name,
            mapped_event = ?event,
            mapped_tool = %tool_name,
            cwd = ?windsurf_input.tool_info.as_ref().and_then(|ti| ti.cwd.as_ref()),
            "🏄 Windsurf parsed input"
        );

        Ok(HookInput {
            event,
            tool_name,
            tool_input,
            session_id: None,
        })
    }

    fn format_windsurf_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
        // Stop Block: return plain text for stderr output (Windsurf reads stderr on exit 2)
        if event == HookEvent::Stop {
            if let Decision::Block { message } = decision {
                return Ok(normalize_lint_output(message));
            }
        }

        // Windsurf uses the same output format as Claude Code (but without hookSpecificOutput)
        // Since Windsurf doesn't support additionalContext, we use a simplified output
        let output = match decision {
            Decision::Allow { .. } => crate::domain::HookOutput {
                decision: "approve".to_string(),
                message: None,
                hook_specific_output: None,
            },
            Decision::Block { message } => crate::domain::HookOutput {
                decision: "block".to_string(),
                message: Some(message.clone()),
                hook_specific_output: None,
            },
        };
        serde_json::to_string(&output).map_err(|e| anyhow!("Failed to serialize output: {}", e))
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

    /// Agent's last assistant message (Stop event)
    #[serde(default)]
    last_assistant_message: Option<String>,
}

/// Claude Code Stop event output format.
/// Uses "reason" instead of "message" for Block decisions on Stop events.
/// This tells Claude to not stop and follow the instructions in "reason".
#[derive(Debug, Serialize)]
struct ClaudeStopOutput {
    /// Decision: "approve" or "block"
    decision: String,
    /// Reason for blocking (tells the agent to fix the issues)
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

// === Cursor Format Types ===

/// Cursor input format - supports beforeShellExecution, afterFileEdit, stop, and subagent hooks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CursorInput {
    // NOTE: SubagentStop MUST come before SubagentStart because serde(untagged)
    // tries variants in order. SubagentStop requires "status" (stricter),
    // while SubagentStart would match any payload with just "subagent_type".
    /// subagentStop hook - subagent has finished
    SubagentStop {
        /// Subagent type
        subagent_type: String,
        /// Subagent completion status: "completed" or "error"
        #[serde(rename = "status")]
        subagent_status: String,
        /// Subagent output/result
        #[serde(default)]
        #[allow(dead_code)]
        result: Option<String>,
        /// Duration in milliseconds
        #[serde(default)]
        duration: Option<u64>,
    },
    /// subagentStart hook - subagent is being spawned
    SubagentStart {
        /// Subagent type: "generalPurpose", "explore", "shell", etc.
        subagent_type: String,
        /// Prompt given to the subagent
        #[serde(default)]
        prompt: Option<String>,
        /// Model used by the subagent
        #[serde(default)]
        #[allow(dead_code)]
        model: Option<String>,
    },
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

/// Cursor Stop Block output format.
/// Uses "followup_message" to instruct the agent to fix lint/typecheck issues.
#[derive(Debug, Serialize)]
struct CursorStopOutput {
    /// Message instructing the agent to continue and fix the issues
    followup_message: String,
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
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
    }

    #[test]
    fn test_cursor_input_parsing_shell_execution() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let input = r#"{"command":"rm -rf /tmp/test","cwd":"/path/to/project"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
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
        assert_eq!(result.event, HookEvent::AfterFileEdit);
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
        assert_eq!(result.event, HookEvent::AfterFileEdit);
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
        assert_eq!(result.event, HookEvent::BeforeCommand);
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
        assert_eq!(result.event, HookEvent::AfterFileEdit);
        assert_eq!(result.tool_name, "Write");
    }

    #[test]
    fn test_cursor_output_allow() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::BeforeCommand)
            .unwrap();
        assert!(output.contains(r#""permission":"allow""#));
    }

    #[test]
    fn test_cursor_output_deny() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "Command blocked for safety".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        assert!(output.contains(r#""permission":"deny""#));
        assert!(output.contains("Command blocked for safety"));
    }

    #[test]
    fn test_claude_output_allow() {
        let adapter = FormatAdapter::new(Format::Claude);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::BeforeCommand)
            .unwrap();
        assert!(output.contains(r#""decision":"approve""#));
        // No hookSpecificOutput for BeforeCommand
        assert!(!output.contains("hookSpecificOutput"));
    }

    #[test]
    fn test_claude_output_allow_with_context() {
        let adapter = FormatAdapter::new(Format::Claude);
        let decision = Decision::allow_with_context("Lint warning: unused variable".to_string());
        let output = adapter
            .format_output(&decision, HookEvent::AfterFileEdit)
            .unwrap();
        assert!(output.contains(r#""decision":"approve""#));
        // hookSpecificOutput should be present for AfterFileEdit with context
        assert!(output.contains("hookSpecificOutput"));
        assert!(output.contains("additionalContext"));
        assert!(output.contains("Lint warning: unused variable"));
    }

    #[test]
    fn test_claude_output_block() {
        let adapter = FormatAdapter::new(Format::Claude);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "Command blocked for safety".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        assert!(output.contains(r#""decision":"block""#));
        assert!(output.contains("Command blocked for safety"));
    }

    #[test]
    fn test_cursor_input_parsing_stop() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let input = r#"{"status":"completed","loop_count":3}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
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
        assert_eq!(result.event, HookEvent::Stop);
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
        assert_eq!(result.event, HookEvent::Stop);
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
        assert_eq!(result.event, HookEvent::Stop);
        assert_eq!(result.tool_name, "Stop");
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert!(stop.stop_hook_active);
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_stop_hook_active_false() {
        let adapter = FormatAdapter::new(Format::Claude);
        let input = r#"{"hook_event_name":"Stop","stop_hook_active":false}"#;
        let result = adapter.parse_input(input).unwrap();
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert!(!stop.stop_hook_active);
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_stop_hook_active_unset() {
        let adapter = FormatAdapter::new(Format::Claude);
        let input = r#"{"hook_event_name":"Stop"}"#;
        let result = adapter.parse_input(input).unwrap();
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert!(!stop.stop_hook_active);
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_stop_with_last_assistant_message() {
        let adapter = FormatAdapter::new(Format::Claude);
        let input = r#"{"hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"I've completed the refactoring task."}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(
                stop.agent_message,
                Some("I've completed the refactoring task.".to_string())
            );
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_stop_without_last_assistant_message() {
        let adapter = FormatAdapter::new(Format::Claude);
        let input = r#"{"hook_event_name":"Stop","stop_hook_active":true}"#;
        let result = adapter.parse_input(input).unwrap();
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert!(stop.agent_message.is_none());
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_windsurf_input_parsing_post_cascade_response_sets_agent_message() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        let input = r#"{"agent_action_name":"post_cascade_response","tool_info":{"response":"All tasks completed successfully."}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(
                stop.agent_message,
                Some("All tasks completed successfully.".to_string())
            );
            assert_eq!(stop.response, stop.agent_message);
        } else {
            panic!("Expected Stop tool input");
        }
    }

    // === Gemini CLI Format Tests ===

    #[test]
    fn test_gemini_input_parsing_before_tool() {
        // Use official Gemini CLI tool name: run_shell_command
        let adapter = FormatAdapter::new(Format::Gemini);
        let input = r#"{"hook_event_name":"BeforeTool","tool_name":"run_shell_command","tool_input":{"command":"rm -rf /tmp/test"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
        if let crate::domain::ToolInput::Bash(bash) = &result.tool_input {
            assert_eq!(bash.command, "rm -rf /tmp/test");
        } else {
            panic!("Expected Bash tool input");
        }
    }

    #[test]
    fn test_gemini_input_parsing_run_shell_command() {
        // run_shell_command is the official Gemini CLI built-in tool name
        let adapter = FormatAdapter::new(Format::Gemini);
        let input = r#"{"hook_event_name":"BeforeTool","tool_name":"run_shell_command","tool_input":{"command":"git status"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
    }

    #[test]
    fn test_gemini_input_parsing_after_tool() {
        let adapter = FormatAdapter::new(Format::Gemini);
        let input = r#"{"hook_event_name":"AfterTool","tool_name":"write_file","tool_input":{"file_path":"/path/to/file.rs"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::AfterFileEdit);
        assert_eq!(result.tool_name, "Write");
    }

    #[test]
    fn test_gemini_input_parsing_replace_tool() {
        // Gemini CLI uses "replace" tool for editing existing files
        let adapter = FormatAdapter::new(Format::Gemini);
        let input = r#"{"hook_event_name":"AfterTool","tool_name":"replace","tool_input":{"file_path":"/path/to/file.rs"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::AfterFileEdit);
        assert_eq!(result.tool_name, "Write"); // replace maps to Write
    }

    #[test]
    fn test_gemini_input_parsing_after_agent() {
        let adapter = FormatAdapter::new(Format::Gemini);
        let input = r#"{"hook_event_name":"AfterAgent"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
        assert_eq!(result.tool_name, "Stop");
    }

    #[test]
    fn test_gemini_input_parsing_before_agent() {
        let adapter = FormatAdapter::new(Format::Gemini);
        let input = r#"{"hook_event_name":"BeforeAgent"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
        assert_eq!(result.tool_name, "UserPrompt");
    }

    #[test]
    fn test_gemini_input_parsing_with_event_alias() {
        let adapter = FormatAdapter::new(Format::Gemini);
        // Gemini might use "event" instead of "hook_event_name"
        let input = r#"{"event":"BeforeTool","tool_name":"run_shell_command","tool_input":{"command":"ls"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
    }

    #[test]
    fn test_gemini_input_parsing_unknown_tool() {
        let adapter = FormatAdapter::new(Format::Gemini);
        let input = r#"{"hook_event_name":"BeforeTool","tool_name":"custom_tool","tool_input":{}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "custom_tool"); // Unknown tools kept as-is
    }

    #[test]
    fn test_gemini_output_allow() {
        let adapter = FormatAdapter::new(Format::Gemini);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::BeforeCommand)
            .unwrap();
        assert!(output.contains(r#""decision":"allow""#));
        assert!(!output.contains("reason"));
    }

    #[test]
    fn test_gemini_output_deny() {
        let adapter = FormatAdapter::new(Format::Gemini);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "🚫 rm command blocked. Use safe-rm instead.".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        assert!(output.contains(r#""decision":"deny""#));
        assert!(output.contains("reason"));
        assert!(output.contains("rm command blocked"));
    }

    #[test]
    fn test_gemini_error_format() {
        let adapter = FormatAdapter::new(Format::Gemini);
        let error_output = adapter.format_error("Invalid JSON input");
        assert!(error_output.contains(r#""decision":"deny""#));
        assert!(error_output.contains("reason"));
        assert!(error_output.contains("fail-closed"));
    }

    // === Windsurf Output Tests ===

    #[test]
    fn test_windsurf_output_allow() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::BeforeCommand)
            .unwrap();
        assert!(output.contains(r#""decision":"approve""#));
        // Windsurf doesn't support hookSpecificOutput
        assert!(!output.contains("hookSpecificOutput"));
    }

    #[test]
    fn test_windsurf_output_block() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "Command blocked for safety".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        assert!(output.contains(r#""decision":"block""#));
        assert!(output.contains("Command blocked for safety"));
    }

    #[test]
    fn test_windsurf_output_allow_after_file_edit() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        // Windsurf doesn't support additionalContext, so context is ignored
        let decision = Decision::allow_with_context("Some lint warning".to_string());
        let output = adapter
            .format_output(&decision, HookEvent::AfterFileEdit)
            .unwrap();
        assert!(output.contains(r#""decision":"approve""#));
        // Context should NOT appear in output (Windsurf doesn't support it)
        assert!(!output.contains("hookSpecificOutput"));
        assert!(!output.contains("additionalContext"));
    }

    #[test]
    fn test_windsurf_error_format() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        let error_output = adapter.format_error("Invalid JSON input");
        assert!(error_output.contains(r#""decision":"block""#));
        assert!(error_output.contains("fail-closed"));
    }

    // === Task 4.1: Claude Code & Gemini CLI Stop Block Output ===

    #[test]
    fn test_claude_output_stop_block_uses_reason_field() {
        let adapter = FormatAdapter::new(Format::Claude);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "Stop hook failed: cargo clippy\nerror: unused variable".to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        // Stop Block should use "reason" instead of "message"
        assert!(output.contains(r#""decision":"block""#));
        assert!(output.contains(r#""reason":"#));
        assert!(!output.contains(r#""message""#));
        assert!(output.contains("unused variable"));
    }

    #[test]
    fn test_claude_output_stop_allow_unchanged() {
        let adapter = FormatAdapter::new(Format::Claude);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::Stop)
            .unwrap();
        assert!(output.contains(r#""decision":"approve""#));
        // No reason field for Allow
        assert!(!output.contains(r#""reason""#));
    }

    #[test]
    fn test_claude_output_before_command_block_still_uses_message() {
        // Non-Stop events should continue using "message" field
        let adapter = FormatAdapter::new(Format::Claude);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "Command blocked".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        assert!(output.contains(r#""decision":"block""#));
        assert!(output.contains(r#""message""#));
        assert!(!output.contains(r#""reason""#));
    }

    #[test]
    fn test_gemini_output_stop_block() {
        let adapter = FormatAdapter::new(Format::Gemini);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "lint errors found".to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        assert!(output.contains(r#""decision":"deny""#));
        assert!(output.contains(r#""reason""#));
        assert!(output.contains("lint errors found"));
    }

    #[test]
    fn test_gemini_output_stop_allow() {
        let adapter = FormatAdapter::new(Format::Gemini);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::Stop)
            .unwrap();
        assert!(output.contains(r#""decision":"allow""#));
        assert!(!output.contains(r#""reason""#));
    }

    #[test]
    fn test_normalize_strips_ansi_codes() {
        let adapter = FormatAdapter::new(Format::Claude);
        let message =
            "Stop hook failed: cargo clippy\n\x1b[31merror\x1b[0m: unused variable `x`".to_string();
        let output = adapter
            .format_output(&Decision::Block { message }, HookEvent::Stop)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["reason"].as_str().unwrap();
        assert!(reason.contains("error: unused variable `x`"));
        assert!(!reason.contains("\x1b"));
    }

    #[test]
    fn test_normalize_strips_leading_whitespace() {
        let adapter = FormatAdapter::new(Format::Claude);
        let message =
            "Stop hook failed: cargo clippy\n    error: unused\n        --> src/main.rs:1:1"
                .to_string();
        let output = adapter
            .format_output(&Decision::Block { message }, HookEvent::Stop)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["reason"].as_str().unwrap();
        // Leading whitespace should be stripped from each line
        assert!(reason.contains("error: unused"));
        assert!(reason.contains("--> src/main.rs:1:1"));
        assert!(!reason.contains("    error"));
        assert!(!reason.contains("        -->"));
    }

    #[test]
    fn test_normalize_collapses_blank_lines() {
        let adapter = FormatAdapter::new(Format::Claude);
        let message = "error 1\n\n\n\nerror 2\n\n\nerror 3".to_string();
        let output = adapter
            .format_output(&Decision::Block { message }, HookEvent::Stop)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["reason"].as_str().unwrap();
        // Consecutive blank lines should be collapsed into one
        assert_eq!(reason, "error 1\n\nerror 2\n\nerror 3");
    }

    #[test]
    fn test_normalize_preserves_simple_messages() {
        let adapter = FormatAdapter::new(Format::Claude);
        let message = "short lint error".to_string();
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: message.clone(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["reason"].as_str().unwrap();
        assert_eq!(reason, message);
    }

    #[test]
    fn test_normalize_gemini_strips_ansi_and_whitespace() {
        let adapter = FormatAdapter::new(Format::Gemini);
        let message = "  \x1b[1;31merror\x1b[0m: type mismatch\n    expected `u32`".to_string();
        let output = adapter
            .format_output(&Decision::Block { message }, HookEvent::Stop)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["reason"].as_str().unwrap();
        assert!(reason.contains("error: type mismatch"));
        assert!(reason.contains("expected `u32`"));
        assert!(!reason.contains("\x1b"));
        assert!(!reason.starts_with(' '));
    }

    // === Task 4.2: Cursor & Windsurf Stop Block Output ===

    #[test]
    fn test_cursor_output_stop_block_uses_followup_message() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "tsc --noEmit failed\nerror TS2322: Type 'string' is not assignable"
                        .to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        // Stop Block should use "followup_message" instead of "permission"/"user_message"
        assert!(output.contains(r#""followup_message":"#));
        assert!(!output.contains(r#""permission""#));
        assert!(!output.contains(r#""user_message""#));
        assert!(output.contains("Type 'string' is not assignable"));
    }

    #[test]
    fn test_cursor_output_stop_allow_unchanged() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::Stop)
            .unwrap();
        // Stop Allow should use standard allow format
        assert!(output.contains(r#""permission":"allow""#));
        assert!(!output.contains(r#""followup_message""#));
    }

    #[test]
    fn test_cursor_output_stop_block_normalizes_output() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let message =
            "  \x1b[1;31merror\x1b[0m: type mismatch\n    expected `u32`\n\n\n    got `String`"
                .to_string();
        let output = adapter
            .format_output(&Decision::Block { message }, HookEvent::Stop)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let followup = parsed["followup_message"].as_str().unwrap();
        // ANSI codes stripped, leading whitespace stripped, blank lines collapsed
        assert!(followup.contains("error: type mismatch"));
        assert!(!followup.contains("\x1b"));
        assert!(!followup.starts_with(' '));
        assert!(followup.contains("expected `u32`\n\ngot `String`"));
    }

    #[test]
    fn test_cursor_output_before_command_block_still_uses_deny() {
        // Non-Stop events should continue using "permission":"deny" format
        let adapter = FormatAdapter::new(Format::Cursor);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "Command blocked".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        assert!(output.contains(r#""permission":"deny""#));
        assert!(!output.contains(r#""followup_message""#));
    }

    #[test]
    fn test_windsurf_output_stop_block_plain_text() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "cargo clippy failed\nerror: unused variable".to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        // Stop Block should return plain text (not JSON) for stderr output
        assert!(!output.starts_with('{'));
        assert!(output.contains("unused variable"));
    }

    #[test]
    fn test_windsurf_output_stop_allow_unchanged() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::Stop)
            .unwrap();
        // Stop Allow should use standard approve format
        assert!(output.contains(r#""decision":"approve""#));
    }

    #[test]
    fn test_windsurf_output_stop_block_normalizes_output() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        let message = "  \x1b[31merror\x1b[0m: unused\n\n\n    --> src/main.rs:1:1".to_string();
        let output = adapter
            .format_output(&Decision::Block { message }, HookEvent::Stop)
            .unwrap();
        // ANSI codes stripped, leading whitespace stripped, blank lines collapsed
        assert!(output.contains("error: unused"));
        assert!(!output.contains("\x1b"));
        assert!(output.contains("--> src/main.rs:1:1"));
        assert!(!output.contains("    -->"));
    }

    #[test]
    fn test_cursor_exit_code_stop_block_is_zero() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let decision = Decision::Block {
            message: "lint error".to_string(),
        };
        // Cursor Stop Block should use exit code 0 (decision in JSON)
        assert_eq!(adapter.exit_code(&decision, HookEvent::Stop), 0);
    }

    #[test]
    fn test_cursor_exit_code_before_command_block_is_two() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let decision = Decision::Block {
            message: "blocked".to_string(),
        };
        // Non-Stop Block should still use exit code 2
        assert_eq!(adapter.exit_code(&decision, HookEvent::BeforeCommand), 2);
    }

    #[test]
    fn test_windsurf_use_stderr_for_stop_block() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        let block = Decision::Block {
            message: "error".to_string(),
        };
        assert!(adapter.use_stderr(&block, HookEvent::Stop));
    }

    #[test]
    fn test_windsurf_use_stdout_for_non_stop() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        let block = Decision::Block {
            message: "error".to_string(),
        };
        assert!(!adapter.use_stderr(&block, HookEvent::BeforeCommand));
    }

    #[test]
    fn test_claude_use_stdout_for_stop_block() {
        let adapter = FormatAdapter::new(Format::Claude);
        let block = Decision::Block {
            message: "error".to_string(),
        };
        assert!(!adapter.use_stderr(&block, HookEvent::Stop));
    }

    // === SubagentStart/SubagentStop Tests ===

    #[test]
    fn test_claude_input_parsing_subagent_start() {
        let adapter = FormatAdapter::new(Format::Claude);
        let input = r#"{"hook_event_name":"SubagentStart","tool_input":{"subagent_type":"explore","prompt":"Search the codebase"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStart);
        assert_eq!(result.tool_name, "SubagentStart");
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, Some("explore".to_string()));
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_subagent_stop() {
        let adapter = FormatAdapter::new(Format::Claude);
        let input = r#"{"hook_event_name":"SubagentStop","tool_input":{"subagent_type":"generalPurpose","status":"completed","duration":5000}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStop);
        assert_eq!(result.tool_name, "SubagentStop");
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, Some("generalPurpose".to_string()));
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_subagent_start_no_tool_input() {
        let adapter = FormatAdapter::new(Format::Claude);
        let input = r#"{"hook_event_name":"SubagentStart"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStart);
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, None);
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_subagent_start_root_agent_type() {
        // Claude Code sends agent_type at root level (not inside tool_input)
        let adapter = FormatAdapter::new(Format::Claude);
        let input =
            r#"{"hook_event_name":"SubagentStart","agent_id":"aa1c090","agent_type":"readme-en"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStart);
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, Some("readme-en".to_string()));
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_subagent_stop_root_agent_type() {
        let adapter = FormatAdapter::new(Format::Claude);
        let input = r#"{"hook_event_name":"SubagentStop","agent_id":"aa1c090","agent_type":"readme-en","permission_mode":"bypassPermissions"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStop);
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, Some("readme-en".to_string()));
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_subagent_start_no_agent_type() {
        // agent_type が無い場合は subagent_type = None
        let adapter = FormatAdapter::new(Format::Claude);
        let input = r#"{"hook_event_name":"SubagentStart","agent_id":"aa1c090"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStart);
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, None);
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_cursor_input_parsing_subagent_start() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let input = r#"{"subagent_type":"explore","prompt":"Explore the authentication flow","model":"claude-sonnet-4-20250514"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStart);
        assert_eq!(result.tool_name, "SubagentStart");
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, Some("explore".to_string()));
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_cursor_input_parsing_subagent_stop() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let input = r#"{"subagent_type":"generalPurpose","status":"completed","result":"Task done","duration":45000}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(
            result.event,
            HookEvent::SubagentStop,
            "Should parse as SubagentStop, not SubagentStart"
        );
        assert_eq!(result.tool_name, "SubagentStop");
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, Some("generalPurpose".to_string()));
            assert_eq!(sub.status, Some("completed".to_string()));
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_claude_output_subagent_start_allow() {
        let adapter = FormatAdapter::new(Format::Claude);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::SubagentStart)
            .unwrap();
        assert!(output.contains(r#""decision":"approve""#));
    }

    #[test]
    fn test_claude_output_subagent_stop_allow() {
        let adapter = FormatAdapter::new(Format::Claude);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::SubagentStop)
            .unwrap();
        assert!(output.contains(r#""decision":"approve""#));
    }

    // === Error Handling Tests ===

    #[test]
    fn test_claude_parse_missing_tool_input_is_error() {
        let adapter = FormatAdapter::new(Format::Claude);
        let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash"}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_claude_parse_missing_tool_name_is_error() {
        let adapter = FormatAdapter::new(Format::Claude);
        let input = r#"{"hook_event_name":"PreToolUse","tool_input":{"command":"ls"}}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_claude_parse_unknown_event_is_error() {
        let adapter = FormatAdapter::new(Format::Claude);
        let input =
            r#"{"hook_event_name":"Unknown","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_claude_parse_invalid_json_is_error() {
        let adapter = FormatAdapter::new(Format::Claude);
        assert!(adapter.parse_input("{").is_err());
        assert!(adapter.parse_input("not json").is_err());
    }

    #[test]
    fn test_cursor_parse_empty_object_is_error() {
        let adapter = FormatAdapter::new(Format::Cursor);
        // Empty object doesn't match any CursorInput variant
        let input = r#"{}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_windsurf_unknown_action_is_error() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        let input = r#"{"agent_action_name":"unknown_action","tool_info":{}}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_windsurf_parse_invalid_json_is_error() {
        let adapter = FormatAdapter::new(Format::Windsurf);
        assert!(adapter.parse_input("{invalid}").is_err());
    }

    #[test]
    fn test_gemini_parse_missing_tool_name_for_tool_event_is_error() {
        let adapter = FormatAdapter::new(Format::Gemini);
        let input = r#"{"hook_event_name":"BeforeTool"}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_gemini_parse_missing_tool_input_for_tool_event_is_error() {
        let adapter = FormatAdapter::new(Format::Gemini);
        let input = r#"{"hook_event_name":"BeforeTool","tool_name":"run_shell_command"}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_gemini_unknown_event_is_error() {
        let adapter = FormatAdapter::new(Format::Gemini);
        let input = r#"{"hook_event_name":"UnknownEvent"}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_cursor_error_format_fail_closed() {
        let adapter = FormatAdapter::new(Format::Cursor);
        let output = adapter.format_error("Invalid JSON input");
        assert!(output.contains(r#""permission":"deny""#));
        assert!(output.contains("fail-closed"));
    }

    #[test]
    fn test_claude_error_format_fail_closed() {
        let adapter = FormatAdapter::new(Format::Claude);
        let output = adapter.format_error("Parse error");
        assert!(output.contains(r#""decision":"block""#));
        assert!(output.contains("fail-closed"));
    }

    #[test]
    fn test_error_exit_code() {
        let adapter = FormatAdapter::new(Format::Claude);
        assert_eq!(adapter.error_exit_code(), 2);
    }
}

// === Gemini CLI Format Types ===

/// Gemini CLI input format.
/// See: https://github.com/google-gemini/gemini-cli#hooks
/// Base input schema includes: session_id, transcript_path, cwd, hook_event_name, timestamp
/// Tool events add: tool_name, tool_input, mcp_context
#[derive(Debug, Deserialize)]
struct GeminiInput {
    /// Hook event name: BeforeTool, AfterTool, AfterAgent, etc.
    #[serde(alias = "event")]
    hook_event_name: String,

    /// Tool name (optional for non-tool events)
    #[serde(default)]
    tool_name: Option<String>,

    /// Tool input (optional for non-tool events)
    #[serde(default)]
    tool_input: Option<crate::domain::ToolInput>,

    /// Session identifier
    #[serde(default)]
    session_id: Option<String>,

    /// Current working directory (base input field)
    #[serde(default)]
    #[allow(dead_code)]
    cwd: Option<String>,

    /// Absolute path to session transcript JSON (base input field)
    #[serde(default)]
    #[allow(dead_code)]
    transcript_path: Option<String>,

    /// ISO 8601 execution time (base input field)
    #[serde(default)]
    #[allow(dead_code)]
    timestamp: Option<String>,

    /// MCP context for MCP-based tools (optional)
    #[serde(default)]
    #[allow(dead_code)]
    mcp_context: Option<serde_json::Value>,

    /// Tool response for AfterTool events
    #[serde(default)]
    #[allow(dead_code)]
    tool_response: Option<serde_json::Value>,

    /// User prompt for BeforeAgent/AfterAgent events
    #[serde(default)]
    #[allow(dead_code)]
    prompt: Option<String>,

    /// Agent response for AfterAgent events
    #[serde(default)]
    #[allow(dead_code)]
    prompt_response: Option<String>,

    /// Stop hook active flag for AfterAgent events
    #[serde(default)]
    #[allow(dead_code)]
    stop_hook_active: Option<bool>,
}

/// Gemini CLI output format.
#[derive(Debug, Serialize)]
struct GeminiOutput {
    /// Decision: "allow" or "deny"
    decision: String,

    /// Reason for denial (when decision is "deny")
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl FormatAdapter {
    // === Gemini CLI Format ===

    fn parse_gemini_input(&self, input: &str) -> Result<HookInput> {
        debug!(raw_input = %input, "♊ Gemini CLI raw input");

        let gemini_input: GeminiInput = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Gemini input: {}", e))?;

        let raw_event = gemini_input.hook_event_name.clone();

        // Map Gemini events to internal HookEvent
        let event = match raw_event.as_str() {
            "BeforeTool" => HookEvent::BeforeCommand,
            "AfterTool" => HookEvent::AfterFileEdit,
            "AfterAgent" => HookEvent::Stop,
            "BeforeAgent" => HookEvent::BeforePrompt,
            other => return Err(anyhow!("Unknown Gemini event: {}", other)),
        };

        // Handle non-tool events
        if event == HookEvent::Stop || event == HookEvent::BeforePrompt {
            let tool_name = if event == HookEvent::Stop {
                "Stop".to_string()
            } else {
                "UserPrompt".to_string()
            };

            debug!(
                format = "gemini",
                raw_event = %raw_event,
                mapped_event = ?event,
                "♊ Gemini CLI parsed input (non-tool event)"
            );

            return Ok(HookInput {
                event,
                tool_name,
                tool_input: crate::domain::ToolInput::Stop(crate::domain::StopInput {
                    status: None,
                    loop_count: None,
                    response: None,
                    agent_message: gemini_input.prompt_response.clone(),
                    stop_hook_active: gemini_input.stop_hook_active.unwrap_or(false),
                }),
                session_id: gemini_input.session_id,
            });
        }

        // Tool events require tool_name and tool_input
        let raw_tool_name = gemini_input
            .tool_name
            .ok_or_else(|| anyhow!("Missing tool_name field"))?;
        let tool_input = gemini_input
            .tool_input
            .ok_or_else(|| anyhow!("Missing tool_input field"))?;

        // Map Gemini tool names to internal tool names
        // See: https://ai.google.dev/gemini-api/docs/tools (built-in tools reference)
        let tool_name = match raw_tool_name.as_str() {
            // Shell execution
            "shell" | "run_shell_command" | "execute_command" => "Bash".to_string(),
            // File writing/editing (replace is used for editing existing files)
            "write_file" | "create_file" | "update_file" | "replace" | "edit_file" => {
                "Write".to_string()
            }
            // File reading
            "read_file" | "view_file" | "read_many_files" => "Read".to_string(),
            // Keep unknown tools as-is
            other => other.to_string(),
        };

        debug!(
            format = "gemini",
            raw_event = %raw_event,
            mapped_event = ?event,
            raw_tool_name = %raw_tool_name,
            mapped_tool = %tool_name,
            "♊ Gemini CLI parsed input"
        );

        Ok(HookInput {
            event,
            tool_name,
            tool_input,
            session_id: gemini_input.session_id,
        })
    }

    fn format_gemini_output(&self, decision: &Decision) -> Result<String> {
        let output = match decision {
            Decision::Allow { .. } => GeminiOutput {
                decision: "allow".to_string(),
                reason: None,
            },
            Decision::Block { message } => GeminiOutput {
                decision: "deny".to_string(),
                reason: Some(normalize_lint_output(message)),
            },
        };
        serde_json::to_string(&output)
            .map_err(|e| anyhow!("Failed to serialize Gemini output: {}", e))
    }
}
