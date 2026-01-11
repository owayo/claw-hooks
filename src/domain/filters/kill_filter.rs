//! Kill command filter implementation.

use super::Filter;
use crate::domain::parser::ShellParser;
use crate::domain::{Decision, HookInput, ToolInput};

/// Default message for kill blocking (generic, can be customized via config).
const DEFAULT_KILL_MESSAGE: &str = "🚫 kill/pkill/killall command blocked for safety. Use safe-kill: safe-kill <PID>, safe-kill -N <name>, or safe-kill -p <port>.";

/// Filter for blocking kill-related commands.
pub struct KillFilter {
    enabled: bool,
    message: String,
}

impl KillFilter {
    /// Create a new KillFilter with optional custom message.
    pub fn new(enabled: bool, custom_message: Option<String>) -> Self {
        Self {
            enabled,
            message: custom_message.unwrap_or_else(|| DEFAULT_KILL_MESSAGE.to_string()),
        }
    }

    /// Kill command patterns for Unix and Windows
    const KILL_COMMANDS: &'static [&'static str] = &[
        "kill",     // Unix
        "pkill",    // Unix
        "killall",  // Unix
        "taskkill", // Windows
    ];

    /// Check if any command in the string is a kill-related command.
    fn contains_kill_command(command: &str) -> bool {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(command);

        // Check for direct kill commands (Unix and Windows)
        if commands
            .iter()
            .any(|cmd| Self::KILL_COMMANDS.contains(&cmd.as_str()))
        {
            return true;
        }

        // Also check for xargs with kill commands
        // Pattern: "xargs kill", "xargs -0 kill", etc.
        Self::contains_xargs_kill(command)
    }

    /// Check if the command contains xargs with a kill command.
    fn contains_xargs_kill(command: &str) -> bool {
        // Split by pipes and check each segment
        for segment in command.split('|') {
            let trimmed = segment.trim();
            if trimmed.starts_with("xargs") {
                // Check if any kill command is mentioned after xargs
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                for part in parts.iter().skip(1) {
                    // Skip xargs flags
                    if !part.starts_with('-') && Self::KILL_COMMANDS.contains(part) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

impl Filter for KillFilter {
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
            return Self::contains_kill_command(&bash.command);
        }

        false
    }

    fn execute(&self, _input: &HookInput) -> Decision {
        Decision::Block {
            message: self.message.clone(),
        }
    }

    fn priority(&self) -> u32 {
        10 // High priority
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_kill_command() {
        // Simple Unix commands
        assert!(KillFilter::contains_kill_command("kill 1234"));
        assert!(KillFilter::contains_kill_command("pkill node"));
        assert!(KillFilter::contains_kill_command("killall python"));
        assert!(!KillFilter::contains_kill_command("ls -la"));
        assert!(!KillFilter::contains_kill_command("echo kill"));

        // Windows commands
        assert!(KillFilter::contains_kill_command("taskkill /PID 1234"));
        assert!(KillFilter::contains_kill_command(
            "taskkill /IM node.exe /F"
        ));

        // Piped commands
        assert!(KillFilter::contains_kill_command(
            "ps aux | grep node | xargs kill"
        ));
        assert!(KillFilter::contains_kill_command(
            "pgrep node | xargs kill -9"
        ));

        // Chained commands
        assert!(KillFilter::contains_kill_command("cd /tmp && kill 1234"));
        assert!(KillFilter::contains_kill_command("echo test; pkill node"));
    }
}
