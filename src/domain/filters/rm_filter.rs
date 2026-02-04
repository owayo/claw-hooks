//! RM command filter implementation.

use super::Filter;
use crate::domain::parser::ShellParser;
use crate::domain::{Decision, HookEvent, HookInput, ToolInput};

/// Default message for rm blocking (generic, can be customized via config).
const DEFAULT_RM_MESSAGE: &str = "🚫 rm/rmdir command blocked for safety. Configure rm_block_message in config.toml to customize this message.";

/// Filter for blocking rm-related commands.
pub struct RmFilter {
    enabled: bool,
    message: String,
}

impl RmFilter {
    /// Create a new RmFilter with optional custom message.
    pub fn new(enabled: bool, custom_message: Option<String>) -> Self {
        Self {
            enabled,
            message: custom_message.unwrap_or_else(|| DEFAULT_RM_MESSAGE.to_string()),
        }
    }

    /// RM command patterns for Unix and Windows
    const RM_COMMANDS: &'static [&'static str] = &[
        "rm",    // Unix
        "rmdir", // Unix/Windows
        "del",   // Windows
        "erase", // Windows (alias for del)
    ];

    /// Check if any command in the string is an rm-related command.
    fn contains_rm_command(command: &str) -> bool {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(command);

        commands
            .iter()
            .any(|cmd| Self::RM_COMMANDS.contains(&cmd.as_str()))
    }
}

impl Filter for RmFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        if !self.enabled {
            return false;
        }

        // Only applies to Bash tool in BeforeCommand event
        if input.event != HookEvent::BeforeCommand || input.tool_name != "Bash" {
            return false;
        }

        // Extract command from tool input
        if let ToolInput::Bash(bash) = &input.tool_input {
            return Self::contains_rm_command(&bash.command);
        }

        false
    }

    fn execute(&self, _input: &HookInput) -> Decision {
        Decision::Block {
            message: self.message.clone(),
        }
    }

    fn priority(&self) -> u32 {
        20 // High priority, but lower than kill
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_rm_command() {
        // Simple Unix commands
        assert!(RmFilter::contains_rm_command("rm file.txt"));
        assert!(RmFilter::contains_rm_command("rm -rf /tmp/test"));
        assert!(RmFilter::contains_rm_command("rmdir empty_dir"));
        assert!(!RmFilter::contains_rm_command("ls -la"));
        assert!(!RmFilter::contains_rm_command("echo rm"));

        // Windows commands
        assert!(RmFilter::contains_rm_command("del file.txt"));
        assert!(RmFilter::contains_rm_command("del /F /Q temp.log"));
        assert!(RmFilter::contains_rm_command("erase old_file.bak"));

        // Chained commands
        assert!(RmFilter::contains_rm_command("cd /tmp && rm -rf test"));
        assert!(RmFilter::contains_rm_command("echo done; rmdir old"));
        assert!(RmFilter::contains_rm_command("dir && del *.tmp"));
    }

    #[test]
    fn test_applies_to_before_command_bash_rm() {
        let filter = RmFilter::new(true, None);

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "rm -rf /tmp/test".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_when_disabled() {
        let filter = RmFilter::new(false, None);

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "rm -rf /tmp/test".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_after_file_edit() {
        let filter = RmFilter::new(true, None);

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "rm -rf /tmp/test".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_non_bash_tool() {
        let filter = RmFilter::new(true, None);

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test".to_string(),
                content: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_execute_returns_block() {
        let filter = RmFilter::new(true, Some("Custom rm block message".to_string()));

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "rm -rf /tmp/test".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        let decision = filter.execute(&input);
        match decision {
            Decision::Block { message } => {
                assert_eq!(message, "Custom rm block message");
            }
            _ => panic!("Expected Block decision"),
        }
    }
}
