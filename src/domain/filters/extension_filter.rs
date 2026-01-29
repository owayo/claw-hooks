//! Extension-based hook filter implementation.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use tracing::{debug, warn};

use super::Filter;
use crate::domain::{Decision, HookInput, ToolInput};

/// Parsed command template result.
struct ParsedCommand {
    /// The command/program to execute
    program: String,
    /// Arguments before the file placeholder
    args_before: Vec<String>,
    /// Arguments after the file placeholder
    args_after: Vec<String>,
    /// If {file} appears inline (e.g., --file={file}), the template token
    inline_template: Option<String>,
}

/// Result of executing a single command.
struct CommandResult {
    /// Command that was executed
    command: String,
    /// Whether the command succeeded
    success: bool,
    /// Combined stdout and stderr output
    output: String,
}

/// Filter for extension-based hooks.
pub struct ExtensionHookFilter {
    /// Map of extension -> commands (e.g., ".go" -> ["gofmt -w {file}", "golangci-lint run {file}"])
    hooks: BTreeMap<String, Vec<String>>,
}

impl ExtensionHookFilter {
    /// Create a new ExtensionHookFilter.
    pub fn new(hooks: BTreeMap<String, Vec<String>>) -> Self {
        Self { hooks }
    }

    /// Get matching commands for file path.
    fn get_matching_commands(&self, file_path: &str) -> Option<&Vec<String>> {
        let path = Path::new(file_path);
        let extension = path.extension()?.to_str()?;
        let ext_with_dot = format!(".{}", extension);

        self.hooks.get(&ext_with_dot)
    }

    /// Validate file path for security issues.
    /// Returns Ok(()) if path is safe, Err with message if dangerous.
    fn validate_file_path(file_path: &str) -> Result<(), String> {
        // Prevent path traversal
        if file_path.contains("..") {
            return Err("Path traversal detected".to_string());
        }

        // Prevent paths that could be interpreted as command flags
        // Use ./ prefix to make it safe for tools that interpret - as flag
        if file_path.starts_with('-') {
            return Err("Path starting with '-' could be interpreted as flag".to_string());
        }

        // Prevent shell metacharacters that could cause injection
        // Note: We don't use shell, but some tools might interpret these
        const DANGEROUS_CHARS: &[char] = &['`', '$', '|', '&', ';', '\n', '\r', '\0'];
        for c in DANGEROUS_CHARS {
            if file_path.contains(*c) {
                return Err(format!("Path contains dangerous character: {:?}", c));
            }
        }

        Ok(())
    }

    /// Parse command template and return structured result.
    /// Handles {file} placeholder safely, including inline patterns like --file={file}.
    fn parse_command_template(template: &str) -> Result<ParsedCommand, String> {
        let parts = crate::domain::parse_shell_tokens(template);
        if parts.is_empty() {
            return Err("Empty command template".to_string());
        }

        let program = parts[0].clone();
        let mut args_before = Vec::new();
        let mut args_after = Vec::new();
        let mut found_placeholder = false;
        let mut inline_template: Option<String> = None;

        for part in parts.iter().skip(1) {
            if *part == "{file}" {
                // Standalone {file} placeholder
                found_placeholder = true;
            } else if part.contains("{file}") {
                // Inline placeholder like --file={file}
                found_placeholder = true;
                inline_template = Some(part.clone());
            } else if found_placeholder {
                args_after.push(part.clone());
            } else {
                args_before.push(part.clone());
            }
        }

        if !found_placeholder {
            return Err("Command template must contain {file} placeholder".to_string());
        }

        Ok(ParsedCommand {
            program,
            args_before,
            args_after,
            inline_template,
        })
    }

    /// Execute a single command safely and return the result.
    /// SECURITY: File path is passed as a separate argument to prevent injection.
    fn execute_command(
        &self,
        command_template: &str,
        file_path: &str,
    ) -> Result<CommandResult, String> {
        // Validate file path first
        Self::validate_file_path(file_path)?;

        // Parse command template
        let parsed = Self::parse_command_template(command_template)?;

        // For tools that might interpret - as flag, use -- to signal end of options
        // or prefix with ./ for relative paths starting with special chars
        let safe_path = if file_path.starts_with('-') {
            // This shouldn't happen due to validation, but double-check
            format!("./{}", file_path)
        } else {
            file_path.to_string()
        };

        debug!(
            "Executing extension hook: {} {:?} {} {:?} inline={:?}",
            parsed.program,
            parsed.args_before,
            safe_path,
            parsed.args_after,
            parsed.inline_template
        );

        // Build command with file path as a separate, properly escaped argument
        let mut cmd = Command::new(&parsed.program);
        cmd.args(&parsed.args_before);

        if let Some(ref template) = parsed.inline_template {
            // Handle inline template like --file={file}
            let arg = template.replace("{file}", &safe_path);
            cmd.arg(&arg);
        } else {
            // Standalone {file} placeholder
            cmd.arg(&safe_path);
        }

        cmd.args(&parsed.args_after);

        let output = cmd
            .output()
            .map_err(|e| format!("Failed to execute hook: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Combine stdout and stderr, filtering empty lines
        let combined_output = [stdout.trim(), stderr.trim()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("\n");

        if !output.status.success() {
            warn!("Hook command failed: {}", stderr);
        }

        Ok(CommandResult {
            command: command_template.to_string(),
            success: output.status.success(),
            output: combined_output,
        })
    }

    /// Execute all commands for an extension and collect output.
    /// Returns combined output from all commands that produced warnings/errors.
    fn execute_commands(&self, commands: &[String], file_path: &str) -> (bool, Option<String>) {
        let mut all_success = true;
        let mut outputs: Vec<String> = Vec::new();

        for cmd_template in commands {
            match self.execute_command(cmd_template, file_path) {
                Ok(result) => {
                    if !result.success {
                        all_success = false;
                    }
                    // Collect non-empty output (warnings, errors, lint messages)
                    if !result.output.is_empty() {
                        outputs.push(format!("[{}] {}", result.command, result.output));
                    }
                }
                Err(e) => {
                    all_success = false;
                    warn!("Extension hook failed: {}", e);
                    outputs.push(format!("[ERROR] {}", e));
                }
            }
        }

        let combined = if outputs.is_empty() {
            None
        } else {
            Some(outputs.join("\n"))
        };

        (all_success, combined)
    }
}

impl Filter for ExtensionHookFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        // Applies to Write, Edit, MultiEdit in both PreToolUse and PostToolUse events
        // NOT for Read operations
        //
        // PreToolUse: Run hook before file write (e.g., validation)
        // PostToolUse: Run hook after file write (e.g., formatting, linting)
        //   - Claude Code: PostToolUse event
        //   - Cursor: afterFileEdit hook
        //   - Windsurf: post_write_code action
        if !matches!(input.event.as_str(), "PreToolUse" | "PostToolUse") {
            return false;
        }

        if !matches!(input.tool_name.as_str(), "Write" | "Edit" | "MultiEdit") {
            return false;
        }

        // Check if we have a matching extension hook
        if let ToolInput::File(file_input) = &input.tool_input {
            return self.get_matching_commands(&file_input.file_path).is_some();
        }

        false
    }

    fn execute(&self, input: &HookInput) -> Decision {
        // Extract file path and execute commands
        if let ToolInput::File(file_input) = &input.tool_input {
            if let Some(commands) = self.get_matching_commands(&file_input.file_path) {
                // Execute commands and collect output
                let (_all_success, output) = self.execute_commands(commands, &file_input.file_path);

                // Return Allow with additional context if there's any output
                // This passes lint warnings/errors to the agent (Claude Code only)
                if let Some(ctx) = output {
                    return Decision::allow_with_context(ctx);
                }
            }
        }

        // Always allow - extension hooks are side effects, not filters
        Decision::allow()
    }

    fn priority(&self) -> u32 {
        100 // Low priority - runs after other filters
    }
}
