//! DD command filter implementation.

use super::Filter;
use crate::domain::parser::ShellParser;
use crate::domain::{Decision, HookInput, ToolInput};

/// Default message for dd blocking.
const DEFAULT_DD_MESSAGE: &str = "🚫 dd command is blocked for safety. Use cp or rsync for file operations. If you need dd specifically, use safe-dd or request explicit permission.";

/// Filter for blocking dd command.
pub struct DdFilter {
    enabled: bool,
    message: String,
}

impl DdFilter {
    /// Create a new DdFilter with optional custom message.
    pub fn new(enabled: bool, custom_message: Option<String>) -> Self {
        Self {
            enabled,
            message: custom_message.unwrap_or_else(|| DEFAULT_DD_MESSAGE.to_string()),
        }
    }

    /// DD command patterns
    const DD_COMMANDS: &'static [&'static str] = &[
        "dd", // Unix disk dump command
    ];

    /// Check if any command in the string is a dd command.
    fn contains_dd_command(command: &str) -> bool {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(command);

        commands
            .iter()
            .any(|cmd| Self::DD_COMMANDS.contains(&cmd.as_str()))
    }
}

impl Filter for DdFilter {
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
            return Self::contains_dd_command(&bash.command);
        }

        false
    }

    fn execute(&self, _input: &HookInput) -> Decision {
        Decision::Block {
            message: self.message.clone(),
        }
    }

    fn priority(&self) -> u32 {
        15 // High priority, between kill (10) and rm (20)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_dd_command() {
        // Simple dd commands
        assert!(DdFilter::contains_dd_command("dd if=/dev/zero of=/dev/sda"));
        assert!(DdFilter::contains_dd_command(
            "dd if=input.img of=output.img bs=4M"
        ));
        assert!(!DdFilter::contains_dd_command("ls -la"));
        assert!(!DdFilter::contains_dd_command("echo dd"));

        // Piped commands
        assert!(DdFilter::contains_dd_command("cat file | dd of=output.img"));

        // Chained commands
        assert!(DdFilter::contains_dd_command(
            "sync && dd if=/dev/sda of=backup.img"
        ));
    }
}
