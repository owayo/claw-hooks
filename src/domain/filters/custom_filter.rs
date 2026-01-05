//! Custom command filter implementation.

use regex::Regex;

use super::Filter;
use crate::domain::parser::ShellParser;
use crate::domain::{Decision, HookInput, ToolInput};

/// Filter for custom command patterns.
pub struct CustomCommandFilter {
    pattern: Regex,
    message: String,
}

impl CustomCommandFilter {
    /// Create a new CustomCommandFilter.
    ///
    /// # Errors
    ///
    /// Returns error if the pattern is not a valid regex.
    pub fn new(pattern: &str, message: String) -> Result<Self, regex::Error> {
        let regex = Regex::new(pattern)?;
        Ok(Self {
            pattern: regex,
            message,
        })
    }

    /// Check if any command in the string matches the pattern.
    /// Uses ShellParser to extract all commands including those after
    /// semicolons, pipes, and logical operators.
    fn matches(&self, command: &str) -> bool {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(command);

        commands.iter().any(|cmd| self.pattern.is_match(cmd))
    }
}

impl Filter for CustomCommandFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        // Only applies to Bash tool in PreToolUse event
        if input.event != "PreToolUse" || input.tool_name != "Bash" {
            return false;
        }

        // Extract command from tool input
        if let ToolInput::Bash(bash) = &input.tool_input {
            return self.matches(&bash.command);
        }

        false
    }

    fn execute(&self, _input: &HookInput) -> Decision {
        Decision::Block {
            message: self.message.clone(),
        }
    }

    fn priority(&self) -> u32 {
        50 // Medium priority
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_custom_filter() {
        let filter = CustomCommandFilter::new("python", "Use uv instead".to_string()).unwrap();
        assert!(filter.matches("python script.py"));
        assert!(filter.matches("python"));
        assert!(!filter.matches("ls"));
    }

    #[test]
    fn test_custom_filter_with_semicolon() {
        let filter = CustomCommandFilter::new("yarn", "Use pnpm instead".to_string()).unwrap();

        // yarn after semicolon should be detected
        assert!(filter.matches("echo \"install\"; yarn install"));

        // yarn in quotes should NOT trigger (it's not a command)
        assert!(!filter.matches("echo \"not yarn install\"; pnpm install"));

        // Direct yarn command
        assert!(filter.matches("yarn install"));
        assert!(filter.matches("yarn add react"));

        // pnpm should pass
        assert!(!filter.matches("pnpm install"));
    }

    #[test]
    fn test_custom_filter_with_chained_commands() {
        let filter = CustomCommandFilter::new("python", "Use uv instead".to_string()).unwrap();

        // python in chained commands
        assert!(filter.matches("cd /app && python script.py"));
        assert!(filter.matches("echo done; python main.py"));
        assert!(filter.matches("ls | python filter.py"));

        // python in quotes should NOT trigger
        assert!(!filter.matches("echo \"python is great\""));
    }
}
