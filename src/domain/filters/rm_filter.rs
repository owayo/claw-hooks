//! RM command filter implementation.

use super::Filter;
use crate::domain::parser::ShellParser;
use crate::domain::{Decision, HookInput, ToolInput};

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

        // Only applies to Bash tool in PreToolUse event
        if input.event != "PreToolUse" || input.tool_name != "Bash" {
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
}
