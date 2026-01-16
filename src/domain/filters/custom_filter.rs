//! Custom command filter implementation.

use regex::Regex;

use super::Filter;
use crate::domain::parser::ShellParser;
use crate::domain::{Decision, HookInput, ToolInput};

/// Filter mode for custom command matching.
enum FilterMode {
    /// Regex-based pattern matching (command field is regex)
    Regex(Regex),
    /// Regex command name + args matching
    Args { command: Regex, args: Vec<String> },
}

/// Filter for custom command patterns.
///
/// Supports two modes:
/// 1. Regex mode: When only `command` is specified, it's treated as a regex pattern
/// 2. Args mode: When both `command` and `args` are specified, matches regex command + any arg
pub struct CustomCommandFilter {
    mode: FilterMode,
    message: String,
}

impl CustomCommandFilter {
    /// Create a new CustomCommandFilter with regex pattern.
    ///
    /// The pattern is automatically anchored at the start of the command string
    /// to ensure it matches the command name, not arguments.
    /// For example, pattern "yarn" will match "yarn install" but not "grep yarn".
    ///
    /// # Errors
    ///
    /// Returns error if the pattern is not a valid regex.
    pub fn new(pattern: &str, message: String) -> Result<Self, regex::Error> {
        // Anchor pattern at start to match command name, not arbitrary arguments
        let anchored_pattern = if pattern.starts_with('^') {
            pattern.to_string()
        } else {
            format!("^{}", pattern)
        };
        let regex = Regex::new(&anchored_pattern)?;
        Ok(Self {
            mode: FilterMode::Regex(regex),
            message,
        })
    }

    /// Create a new CustomCommandFilter with regex command + args matching.
    ///
    /// Matches if the command name matches `command` regex AND any of the `args` is present
    /// as the first argument.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let filter = CustomCommandFilter::with_args("npm", vec!["install", "i", "add"], "msg")?;
    /// // Matches: npm install, npm i, npm add package
    /// // Does not match: npm run, npm test
    ///
    /// let filter = CustomCommandFilter::with_args("pip3?", vec!["install"], "msg")?;
    /// // Matches: pip install, pip3 install
    /// ```
    ///
    /// # Errors
    ///
    /// Returns error if the command pattern is not a valid regex.
    pub fn with_args(
        command: &str,
        args: Vec<String>,
        message: String,
    ) -> Result<Self, regex::Error> {
        // Compile command as regex (anchored to match full command name)
        let anchored = format!("^{}$", command);
        let regex = Regex::new(&anchored)?;
        Ok(Self {
            mode: FilterMode::Args {
                command: regex,
                args,
            },
            message,
        })
    }

    /// Strip quoted content from a command string for pattern matching.
    /// This prevents false positives like matching "yarn" in `echo "yarn"`.
    fn strip_quoted_content(s: &str) -> String {
        let mut result = String::new();
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let mut chars = s.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '\\' && !in_single_quote {
                // Skip escaped character
                chars.next();
                continue;
            }

            if c == '\'' && !in_double_quote {
                in_single_quote = !in_single_quote;
                continue;
            }

            if c == '"' && !in_single_quote {
                in_double_quote = !in_double_quote;
                continue;
            }

            if !in_single_quote && !in_double_quote {
                result.push(c);
            }
        }

        result
    }

    /// Check if any command in the string matches using regex mode.
    fn matches_regex(&self, command: &str, pattern: &Regex) -> bool {
        let mut parser = ShellParser::new();
        let command_strings = parser.extract_command_strings(command);

        command_strings
            .iter()
            .any(|cmd| pattern.is_match(&Self::strip_quoted_content(cmd)))
    }

    /// Check if any command in the string matches using args mode.
    fn matches_args(
        &self,
        input_command: &str,
        target_cmd: &Regex,
        target_args: &[String],
    ) -> bool {
        let mut parser = ShellParser::new();
        let command_strings = parser.extract_command_strings(input_command);

        for cmd_str in command_strings {
            let stripped = Self::strip_quoted_content(&cmd_str);
            let parts: Vec<&str> = stripped.split_whitespace().collect();

            if parts.is_empty() {
                continue;
            }

            // Check if command name matches regex
            if !target_cmd.is_match(parts[0]) {
                continue;
            }

            // If no args specified, any usage of the command matches
            if target_args.is_empty() {
                return true;
            }

            // Check if any of the target args is present
            if parts.len() > 1 && target_args.iter().any(|arg| parts[1] == arg) {
                return true;
            }
        }

        false
    }

    /// Check if any command in the string matches the filter.
    fn matches(&self, command: &str) -> bool {
        match &self.mode {
            FilterMode::Regex(pattern) => self.matches_regex(command, pattern),
            FilterMode::Args { command: cmd, args } => self.matches_args(command, cmd, args),
        }
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

    // Regex mode tests
    #[test]
    fn test_custom_filter_regex() {
        let filter = CustomCommandFilter::new("python", "Use uv instead".to_string()).unwrap();
        assert!(filter.matches("python script.py"));
        assert!(filter.matches("python"));
        assert!(!filter.matches("ls"));
    }

    #[test]
    fn test_custom_filter_regex_with_semicolon() {
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
    fn test_custom_filter_regex_with_chained_commands() {
        let filter = CustomCommandFilter::new("python", "Use uv instead".to_string()).unwrap();

        // python in chained commands
        assert!(filter.matches("cd /app && python script.py"));
        assert!(filter.matches("echo done; python main.py"));
        assert!(filter.matches("ls | python filter.py"));

        // python in quotes should NOT trigger
        assert!(!filter.matches("echo \"python is great\""));
    }

    // Args mode tests
    #[test]
    fn test_custom_filter_args_basic() {
        let filter = CustomCommandFilter::with_args(
            "npm",
            vec!["install".to_string(), "i".to_string(), "add".to_string()],
            "Use pnpm instead".to_string(),
        )
        .unwrap();

        // Should match
        assert!(filter.matches("npm install"));
        assert!(filter.matches("npm i"));
        assert!(filter.matches("npm add react"));
        assert!(filter.matches("npm install lodash"));

        // Should not match (different subcommand)
        assert!(!filter.matches("npm run build"));
        assert!(!filter.matches("npm test"));
        assert!(!filter.matches("npm --version"));

        // Should not match (different command)
        assert!(!filter.matches("pnpm install"));
        assert!(!filter.matches("yarn add"));
    }

    #[test]
    fn test_custom_filter_args_in_chained_commands() {
        let filter = CustomCommandFilter::with_args(
            "npm",
            vec!["install".to_string(), "i".to_string()],
            "Use pnpm instead".to_string(),
        )
        .unwrap();

        // Should match in chained commands
        assert!(filter.matches("echo done; npm install"));
        assert!(filter.matches("cd /app && npm i lodash"));

        // Should not match when in quotes
        assert!(!filter.matches("echo \"npm install\""));

        // Should not match different subcommand in chain
        assert!(!filter.matches("npm run build && echo done"));
    }

    #[test]
    fn test_custom_filter_args_empty_args() {
        // Empty args means match any usage of the command
        let filter =
            CustomCommandFilter::with_args("yarn", vec![], "Use pnpm instead".to_string()).unwrap();

        // Should match all yarn commands
        assert!(filter.matches("yarn"));
        assert!(filter.matches("yarn install"));
        assert!(filter.matches("yarn add react"));
        assert!(filter.matches("yarn run build"));

        // Should not match other commands
        assert!(!filter.matches("npm install"));
    }

    #[test]
    fn test_custom_filter_args_with_flags() {
        let filter = CustomCommandFilter::with_args(
            "hoge",
            vec!["--fuga".to_string(), "-f".to_string()],
            "Block!!!".to_string(),
        )
        .unwrap();

        // Should match
        assert!(filter.matches("hoge --fuga"));
        assert!(filter.matches("hoge -f value"));

        // Should not match
        assert!(!filter.matches("hoge --other"));
        assert!(!filter.matches("hoge run"));
    }

    #[test]
    fn test_custom_filter_args_with_regex_command() {
        // Test regex pattern in command field with args mode
        let filter = CustomCommandFilter::with_args(
            "pip3?",
            vec!["install".to_string(), "uninstall".to_string()],
            "Use uv pip instead".to_string(),
        )
        .unwrap();

        // Should match both pip and pip3
        assert!(filter.matches("pip install requests"));
        assert!(filter.matches("pip3 install requests"));
        assert!(filter.matches("pip uninstall requests"));
        assert!(filter.matches("pip3 uninstall requests"));

        // Should not match other subcommands
        assert!(!filter.matches("pip list"));
        assert!(!filter.matches("pip3 --version"));

        // Should not match other commands
        assert!(!filter.matches("python install"));
    }
}
