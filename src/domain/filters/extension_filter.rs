//! Extension-based hook filter implementation.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use tracing::{debug, warn};

use super::Filter;
use crate::domain::{Decision, HookInput, ToolInput};

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

    /// Parse command template and return (program, args_before_file, args_after_file).
    /// Handles {file} placeholder safely.
    fn parse_command_template(
        template: &str,
    ) -> Result<(String, Vec<String>, Vec<String>), String> {
        let parts: Vec<&str> = template.split_whitespace().collect();
        if parts.is_empty() {
            return Err("Empty command template".to_string());
        }

        let program = parts[0].to_string();
        let mut args_before = Vec::new();
        let mut args_after = Vec::new();
        let mut found_placeholder = false;

        for part in parts.iter().skip(1) {
            if *part == "{file}" {
                found_placeholder = true;
            } else if found_placeholder {
                args_after.push(part.to_string());
            } else {
                args_before.push(part.to_string());
            }
        }

        if !found_placeholder {
            return Err("Command template must contain {file} placeholder".to_string());
        }

        Ok((program, args_before, args_after))
    }

    /// Execute a single command safely.
    /// SECURITY: File path is passed as a separate argument to prevent injection.
    fn execute_command(&self, command_template: &str, file_path: &str) -> Result<(), String> {
        // Validate file path first
        Self::validate_file_path(file_path)?;

        // Parse command template
        let (program, args_before, args_after) = Self::parse_command_template(command_template)?;

        debug!(
            "Executing extension hook: {} {:?} [file] {:?}",
            program, args_before, args_after
        );

        // Build command with file path as a separate, properly escaped argument
        let mut cmd = Command::new(&program);
        cmd.args(&args_before);

        // For tools that might interpret - as flag, use -- to signal end of options
        // or prefix with ./ for relative paths starting with special chars
        let safe_path = if file_path.starts_with('-') {
            // This shouldn't happen due to validation, but double-check
            format!("./{}", file_path)
        } else {
            file_path.to_string()
        };
        cmd.arg(&safe_path);
        cmd.args(&args_after);

        let output = cmd
            .output()
            .map_err(|e| format!("Failed to execute hook: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("Hook command failed: {}", stderr);
        }

        Ok(())
    }

    /// Execute all commands for an extension.
    fn execute_commands(&self, commands: &[String], file_path: &str) -> Result<(), String> {
        for cmd in commands {
            self.execute_command(cmd, file_path)?;
        }
        Ok(())
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
                // Execute commands but don't block on failure
                if let Err(e) = self.execute_commands(commands, &file_input.file_path) {
                    warn!("Extension hook failed: {}", e);
                }
            }
        }

        // Always allow - extension hooks are side effects, not filters
        Decision::Allow
    }

    fn priority(&self) -> u32 {
        100 // Low priority - runs after other filters
    }
}
