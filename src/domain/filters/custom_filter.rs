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
            pattern: regex,
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

    /// Check if any command in the string matches the pattern.
    /// Uses ShellParser to extract all command strings (command + arguments)
    /// including those after semicolons, pipes, and logical operators.
    /// Quoted content is stripped before matching to avoid false positives.
    fn matches(&self, command: &str) -> bool {
        let mut parser = ShellParser::new();
        let command_strings = parser.extract_command_strings(command);

        command_strings
            .iter()
            .any(|cmd| self.pattern.is_match(&Self::strip_quoted_content(cmd)))
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

#[cfg(test)]
mod debug_tests {
    use super::*;

    #[test]
    fn debug_extract_and_strip() {
        let mut parser = ShellParser::new();

        let test_cases = vec![
            "echo \"not yarn install\"; pnpm install",
            "echo \"python is great\"",
            "echo install; yarn install",
            "npm install hoge",
        ];

        for cmd in test_cases {
            let strings = parser.extract_command_strings(cmd);
            println!("\nInput: {}", cmd);
            println!("Command strings: {:?}", strings);
            for s in &strings {
                let stripped = CustomCommandFilter::strip_quoted_content(s);
                println!("  '{}' -> stripped: '{}'", s, stripped);
            }
        }
    }
}

#[test]
fn debug_pipe_command() {
    let mut parser = ShellParser::new();

    let cmd = "echo \"yarn\" | grep yarn";
    let strings = parser.extract_command_strings(cmd);
    println!("\nInput: {}", cmd);
    println!("Command strings: {:?}", strings);
    for s in &strings {
        let stripped = CustomCommandFilter::strip_quoted_content(s);
        println!("  '{}' -> stripped: '{}'", s, stripped);
        println!("  matches 'yarn': {}", stripped.contains("yarn"));
    }
}
