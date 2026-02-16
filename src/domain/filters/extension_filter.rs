//! Extension-based hook filter implementation.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use tracing::{debug, info, warn};

use super::Filter;
use crate::domain::command::run_with_timeout;
use crate::domain::normalize::normalize_lint_output;
use crate::domain::{Decision, HookEvent, HookInput, ToolInput};

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
    nano_buddy: bool,
    timeout_secs: u64,
}

impl ExtensionHookFilter {
    /// Create a new ExtensionHookFilter.
    pub fn new(hooks: BTreeMap<String, Vec<String>>, nano_buddy: bool, timeout_secs: u64) -> Self {
        Self {
            hooks,
            nano_buddy,
            timeout_secs,
        }
    }

    /// Extract extension from file path (without the leading dot).
    fn extract_ext(file_path: &str) -> Option<String> {
        Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_string())
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
            "🪛 Executing extension hook: {} {:?} {} {:?} inline={:?}",
            parsed.program,
            parsed.args_before,
            safe_path,
            parsed.args_after,
            parsed.inline_template
        );

        // Build command with file path as a separate, properly escaped argument
        // On Windows, use `cmd /c` to resolve .cmd/.bat wrappers (e.g. npx.cmd)
        let mut cmd = if cfg!(target_os = "windows") {
            let mut c = Command::new("cmd");
            c.arg("/c").arg(&parsed.program);
            c
        } else {
            Command::new(&parsed.program)
        };
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
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        // Build the actual expanded command string for logging
        let actual_command = {
            let mut parts = vec![parsed.program.clone()];
            parts.extend(parsed.args_before.iter().cloned());
            if let Some(ref template) = parsed.inline_template {
                parts.push(template.replace("{file}", &safe_path));
            } else {
                parts.push(safe_path.clone());
            }
            parts.extend(parsed.args_after.iter().cloned());
            parts.join(" ")
        };

        let start = std::time::Instant::now();
        let child = cmd
            .spawn()
            .map_err(|e| format!("Failed to execute hook: {}", e))?;
        let result = run_with_timeout(child, self.timeout_secs, &actual_command);
        let elapsed = start.elapsed();
        info!(
            "⏰️ Extension hook [{}] completed in {:.2}s",
            actual_command,
            elapsed.as_secs_f64()
        );
        let output = result?;

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
            let exit_code = output
                .status
                .code()
                .map_or("signal".to_string(), |c| c.to_string());
            let detail = [stderr.trim(), stdout.trim()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            if detail.is_empty() {
                warn!(
                    "⚠️ Extension hook command failed (exit code {}): {}",
                    exit_code, actual_command
                );
            } else {
                warn!(
                    "⚠️ Extension hook command failed (exit code {}): {}\n{}",
                    exit_code, actual_command, detail
                );
            }
        }

        Ok(CommandResult {
            command: actual_command,
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
                    warn!("🪛 Extension hook failed: {}", e);
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
        // Applies to Write, Edit, MultiEdit in both BeforeCommand and AfterFileEdit events
        // NOT for Read operations
        //
        // BeforeCommand: Run hook before file write (e.g., validation)
        // AfterFileEdit: Run hook after file write (e.g., formatting, linting)
        //   - Claude Code: PostToolUse event
        //   - Cursor: afterFileEdit hook
        //   - Windsurf: post_write_code action
        if !matches!(
            input.event,
            HookEvent::BeforeCommand | HookEvent::AfterFileEdit
        ) {
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
                // NanoBuddy notification (before hook commands so it arrives first)
                if self.nano_buddy {
                    if let Some(ext) = Self::extract_ext(&file_input.file_path) {
                        debug!("🐱 NanoBuddy ext notification: .{}", ext);
                        crate::notify::nano_buddy::notify_extension_hook(&ext);
                    }
                }

                // Execute commands and collect output
                let (_all_success, output) = self.execute_commands(commands, &file_input.file_path);

                // Return Allow with additional context if there's any output
                // This passes lint warnings/errors to the agent (Claude Code only)
                // Normalize output for token efficiency (strip ANSI, collapse blanks)
                if let Some(ctx) = output {
                    return Decision::allow_with_context(normalize_lint_output(&ctx));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn create_filter_with_go_hooks() -> ExtensionHookFilter {
        let mut hooks = BTreeMap::new();
        hooks.insert(".go".to_string(), vec!["gofmt -w {file}".to_string()]);
        ExtensionHookFilter::new(hooks, false, 60)
    }

    fn create_empty_filter() -> ExtensionHookFilter {
        ExtensionHookFilter::new(BTreeMap::new(), false, 60)
    }

    // applies_to tests

    #[test]
    fn test_applies_to_before_command_with_write() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.go".to_string(),
                content: None,
            }),
            session_id: None,
        };

        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_applies_to_after_file_edit_with_write() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.go".to_string(),
                content: None,
            }),
            session_id: None,
        };

        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_applies_to_edit_tool() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Edit".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.go".to_string(),
                content: None,
            }),
            session_id: None,
        };

        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_applies_to_multi_edit_tool() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "MultiEdit".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.go".to_string(),
                content: None,
            }),
            session_id: None,
        };

        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_stop_event() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput::default()),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_before_prompt_event() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::BeforePrompt,
            tool_name: "UserPrompt".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput::default()),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_read_tool() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Read".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.go".to_string(),
                content: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_bash_tool() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "ls".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_non_matching_extension() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.rs".to_string(), // .rs not in hooks
                content: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_when_no_hooks_configured() {
        let filter = create_empty_filter();

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.go".to_string(),
                content: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    // execute tests

    #[test]
    fn test_execute_returns_allow() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.go".to_string(),
                content: None,
            }),
            session_id: None,
        };

        let decision = filter.execute(&input);
        // Extension hooks always allow (they're side effects, not filters)
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_priority() {
        let filter = create_filter_with_go_hooks();
        assert_eq!(filter.priority(), 100);
    }

    // === validate_file_path tests ===

    #[test]
    fn test_validate_file_path_rejects_path_traversal() {
        assert!(ExtensionHookFilter::validate_file_path("../secret.txt").is_err());
        assert!(ExtensionHookFilter::validate_file_path("/tmp/../etc/passwd").is_err());
    }

    #[test]
    fn test_validate_file_path_rejects_dash_prefix() {
        assert!(ExtensionHookFilter::validate_file_path("-rf").is_err());
        assert!(ExtensionHookFilter::validate_file_path("--help").is_err());
    }

    #[test]
    fn test_validate_file_path_rejects_dangerous_chars() {
        assert!(ExtensionHookFilter::validate_file_path("bad;rm -rf /").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file`id`").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file$HOME").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file|pipe").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file&bg").is_err());
    }

    #[test]
    fn test_validate_file_path_accepts_safe_paths() {
        assert!(ExtensionHookFilter::validate_file_path("/path/to/file.go").is_ok());
        assert!(ExtensionHookFilter::validate_file_path("relative/path.rs").is_ok());
        assert!(ExtensionHookFilter::validate_file_path("file with spaces.txt").is_ok());
    }

    // === parse_command_template tests ===

    #[test]
    fn test_parse_command_template_basic() {
        let parsed = ExtensionHookFilter::parse_command_template("gofmt -w {file}").unwrap();
        assert_eq!(parsed.program, "gofmt");
        assert_eq!(parsed.args_before, vec!["-w"]);
        assert!(parsed.args_after.is_empty());
        assert!(parsed.inline_template.is_none());
    }

    #[test]
    fn test_parse_command_template_inline_placeholder() {
        let parsed =
            ExtensionHookFilter::parse_command_template("tool --flag --file={file} --opt").unwrap();
        assert_eq!(parsed.program, "tool");
        assert_eq!(parsed.args_before, vec!["--flag"]);
        assert_eq!(parsed.args_after, vec!["--opt"]);
        assert_eq!(parsed.inline_template.as_deref(), Some("--file={file}"));
    }

    #[test]
    fn test_parse_command_template_missing_placeholder_is_error() {
        assert!(ExtensionHookFilter::parse_command_template("gofmt -w").is_err());
        assert!(ExtensionHookFilter::parse_command_template("rustfmt").is_err());
    }

    #[test]
    fn test_parse_command_template_empty_is_error() {
        assert!(ExtensionHookFilter::parse_command_template("").is_err());
        assert!(ExtensionHookFilter::parse_command_template("   ").is_err());
    }

    // === Timeout tests ===

    #[test]
    fn test_extension_hook_timeout_returns_allow_with_error_context() {
        // Extension hooks always allow, but timeout error should appear in context
        let mut hooks = BTreeMap::new();
        hooks.insert(
            ".txt".to_string(),
            vec!["sh -c 'sleep 30 #ignore {file}'".to_string()],
        );
        let filter = ExtensionHookFilter::new(hooks, false, 2);

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };

        let start = std::time::Instant::now();
        let decision = filter.execute(&input);
        let elapsed = start.elapsed();

        // Should still allow (extension hooks are side effects, not blockers)
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "Extension hooks always allow even on timeout"
        );
        assert!(
            elapsed.as_secs() < 5,
            "Should timeout in ~2s, took {:?}",
            elapsed
        );
        // Error context should mention the failure
        match decision {
            Decision::Allow { additional_context } => {
                let ctx = additional_context.expect("Should have error context on timeout");
                assert!(
                    ctx.contains("timed out") || ctx.contains("ERROR"),
                    "Context should indicate timeout: {}",
                    ctx
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_extension_hook_completes_within_timeout() {
        let mut hooks = BTreeMap::new();
        hooks.insert(
            ".txt".to_string(),
            vec!["sh -c 'echo ok #ignore {file}'".to_string()],
        );
        let filter = ExtensionHookFilter::new(hooks, false, 60);

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };

        let start = std::time::Instant::now();
        let decision = filter.execute(&input);
        let elapsed = start.elapsed();

        assert!(matches!(decision, Decision::Allow { .. }));
        assert!(
            elapsed.as_secs() < 5,
            "Fast hook should complete quickly: {:?}",
            elapsed
        );
    }

    // === Output normalization tests ===

    #[test]
    fn test_execute_normalizes_output() {
        // Use "sh -c" with printf to produce ANSI-colored output with indentation
        let mut hooks = BTreeMap::new();
        hooks.insert(
            ".txt".to_string(),
            vec![
                "sh -c 'printf \"\\033[31m  error:\\033[0m bad\\n\\n\\n  detail\" #ignore {file}'"
                    .to_string(),
            ],
        );
        let filter = ExtensionHookFilter::new(hooks, false, 60);

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };

        let decision = filter.execute(&input);
        match decision {
            Decision::Allow { additional_context } => {
                let ctx = additional_context.expect("Should have context");
                // ANSI codes should be stripped
                assert!(
                    !ctx.contains("\x1b"),
                    "ANSI codes should be stripped: {}",
                    ctx
                );
                // Leading whitespace should be stripped
                assert!(
                    !ctx.contains("\n  "),
                    "Leading whitespace should be stripped: {}",
                    ctx
                );
                // Consecutive blank lines should be collapsed
                assert!(
                    !ctx.contains("\n\n\n"),
                    "Blank lines should be collapsed: {}",
                    ctx
                );
            }
            _ => panic!("Expected Allow decision"),
        }
    }
}
