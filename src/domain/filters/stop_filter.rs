//! Stop event hook filter implementation.

use std::process::{Command, Output};
use tracing::{debug, warn};

use super::Filter;
use crate::config::StopHook;
use crate::domain::{Decision, HookEvent, HookInput};

/// Filter for Stop event hooks.
pub struct StopHookFilter {
    hooks: Vec<StopHook>,
}

impl StopHookFilter {
    /// Create a new StopHookFilter.
    pub fn new(hooks: Vec<StopHook>) -> Self {
        Self { hooks }
    }

    /// Execute a stop hook command safely.
    /// Uses shell-aware tokenizer to properly handle quoted arguments.
    fn execute_hook(&self, hook: &StopHook) -> Result<Output, String> {
        let parts = crate::domain::parse_shell_tokens(&hook.command);
        if parts.is_empty() {
            return Err("Empty command".to_string());
        }

        let program = &parts[0];
        let args = &parts[1..];

        debug!("🛑 Executing stop hook: {} {:?}", program, args);

        let mut cmd = Command::new(program);
        cmd.args(args);

        cmd.output()
            .map_err(|e| format!("Failed to execute stop hook '{}': {}", hook.command, e))
    }

    /// Build a block reason from command output (stdout + stderr).
    fn build_reason(&self, hook: &StopHook, output: &Output) -> String {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let mut reason = format!("Stop hook failed: {}\n", hook.command);
        if !stdout.trim().is_empty() {
            reason.push_str(&stdout);
        }
        if !stderr.trim().is_empty() {
            if !stdout.trim().is_empty() {
                reason.push('\n');
            }
            reason.push_str(&stderr);
        }
        reason.trim().to_string()
    }
}

impl Filter for StopHookFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        // Applies only to Stop events
        input.event == HookEvent::Stop
    }

    fn execute(&self, input: &HookInput) -> Decision {
        // Check stop_hook_active to prevent infinite loops
        if let crate::domain::ToolInput::Stop(ref stop_input) = input.tool_input {
            if stop_input.stop_hook_active {
                debug!("🛑 stop_hook_active=true, skipping all stop hooks");
                return Decision::allow();
            }
        }

        let cwd = std::env::current_dir().unwrap_or_default();

        // Execute all stop hooks
        for hook in &self.hooks {
            match &hook.condition {
                None => {
                    // Legacy hook (no condition): fire and forget, ignore result
                    match self.execute_hook(hook) {
                        Ok(output) => {
                            if !output.status.success() {
                                let stderr = String::from_utf8_lossy(&output.stderr);
                                warn!("🛑 Stop hook command failed: {}", stderr);
                            }
                        }
                        Err(e) => {
                            warn!("🛑 Stop hook failed: {}", e);
                        }
                    }
                }
                Some(condition) => {
                    // Conditional hook: evaluate condition → execute → evaluate result
                    if !condition.is_satisfied(&cwd) {
                        debug!("Stop hook condition not met, skipping: {}", hook.command);
                        continue;
                    }

                    debug!("Stop hook condition met, executing: {}", hook.command);
                    match self.execute_hook(hook) {
                        Ok(output) => {
                            if !output.status.success() {
                                let reason = self.build_reason(hook, &output);
                                return Decision::Block { message: reason };
                            }
                        }
                        Err(e) => {
                            // Command not found or execution error → Block
                            return Decision::Block { message: e };
                        }
                    }
                }
            }
        }

        Decision::allow()
    }

    fn priority(&self) -> u32 {
        100 // Low priority - runs after other filters
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ToolInput;

    #[test]
    fn test_stop_hook_filter_applies_to_stop_event() {
        let hooks = vec![StopHook {
            command: "echo done".to_string(),
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput::default()),
            session_id: None,
        };

        assert!(filter.applies_to(&stop_input));
    }

    #[test]
    fn test_stop_hook_filter_does_not_apply_to_other_events() {
        let hooks = vec![StopHook {
            command: "echo done".to_string(),
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks);

        let bash_input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "ls".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&bash_input));
    }

    #[test]
    fn test_stop_hook_filter_execute_returns_allow() {
        let hooks = vec![StopHook {
            command: "echo done".to_string(),
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput::default()),
            session_id: None,
        };

        let decision = filter.execute(&stop_input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    // === Edge Case Tests ===

    #[test]
    fn test_execute_hook_empty_command_is_error() {
        let hooks = vec![];
        let filter = StopHookFilter::new(hooks);
        let hook = StopHook {
            command: "   ".to_string(),
            condition: None,
        };
        assert!(filter.execute_hook(&hook).is_err());
    }

    #[test]
    fn test_execute_hook_with_quoted_args() {
        // Test that quoted arguments are parsed correctly
        let hooks = vec![];
        let filter = StopHookFilter::new(hooks);
        let hook = StopHook {
            command: "echo 'hello world'".to_string(),
            condition: None,
        };
        // Should not error, echo is a valid command
        assert!(filter.execute_hook(&hook).is_ok());
    }

    #[test]
    fn test_execute_ignores_hook_failure_and_allows() {
        // Non-existent command should fail but filter should still return Allow
        let hooks = vec![StopHook {
            command: "nonexistent-command-xyz-abc-123".to_string(),
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput::default()),
            session_id: None,
        };

        // execute() should return Allow even if hooks fail
        let decision = filter.execute(&stop_input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_priority() {
        let filter = StopHookFilter::new(vec![]);
        assert_eq!(filter.priority(), 100);
    }

    // === Task 3.1: Loop prevention and backward compatibility ===

    #[test]
    fn test_stop_hook_active_true_skips_all_hooks() {
        // When stop_hook_active is true, all hooks should be skipped
        let hooks = vec![StopHook {
            command: "echo should-not-run".to_string(),
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                status: None,
                loop_count: None,
                response: None,
                stop_hook_active: true,
            }),
            session_id: None,
        };

        let decision = filter.execute(&stop_input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_stop_hook_active_false_runs_hooks() {
        // When stop_hook_active is false, hooks should execute normally
        let hooks = vec![StopHook {
            command: "echo running".to_string(),
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                status: None,
                loop_count: None,
                response: None,
                stop_hook_active: false,
            }),
            session_id: None,
        };

        let decision = filter.execute(&stop_input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_condition_none_hook_always_allows_even_on_failure() {
        // Hooks without condition should fire-and-forget (backward compat)
        let hooks = vec![StopHook {
            command: "nonexistent-command-xyz".to_string(),
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                status: None,
                loop_count: None,
                response: None,
                stop_hook_active: false,
            }),
            session_id: None,
        };

        // Should still Allow even when command fails (fire-and-forget)
        let decision = filter.execute(&stop_input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    // === Task 3.2: Conditional hooks - condition evaluation & result evaluation ===

    fn make_stop_input() -> HookInput {
        HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                status: None,
                loop_count: None,
                response: None,
                stop_hook_active: false,
            }),
            session_id: None,
        }
    }

    #[test]
    fn test_conditional_hook_file_not_found_skips() {
        use crate::config::HookCondition;
        // When condition file doesn't exist, hook should be skipped
        let hooks = vec![StopHook {
            command: "false".to_string(), // Would fail if executed
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("nonexistent-file-xyz-abc.toml".to_string()),
            }),
        }];
        let filter = StopHookFilter::new(hooks);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_conditional_hook_file_exists_command_succeeds() {
        use crate::config::HookCondition;
        // Cargo.toml exists, command succeeds → Allow
        let hooks = vec![StopHook {
            command: "true".to_string(),
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
        }];
        let filter = StopHookFilter::new(hooks);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_conditional_hook_file_exists_command_fails_blocks() {
        use crate::config::HookCondition;
        // Cargo.toml exists, command fails → Block with reason
        let hooks = vec![StopHook {
            command: "sh -c 'echo lint-error >&2; exit 1'".to_string(),
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
        }];
        let filter = StopHookFilter::new(hooks);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("lint-error"),
                    "Expected lint-error in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }

    #[test]
    fn test_conditional_hook_command_not_found_blocks() {
        use crate::config::HookCondition;
        // Cargo.toml exists, command not found → Block with error
        let hooks = vec![StopHook {
            command: "nonexistent-lint-tool-xyz-123".to_string(),
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
        }];
        let filter = StopHookFilter::new(hooks);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_multiple_hooks_first_failure_stops() {
        use crate::config::HookCondition;
        // First conditional hook fails → Block, second never runs
        let hooks = vec![
            StopHook {
                command: "sh -c 'echo first-error >&2; exit 1'".to_string(),
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
            },
            StopHook {
                command: "sh -c 'echo second-error >&2; exit 1'".to_string(),
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
            },
        ];
        let filter = StopHookFilter::new(hooks);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("first-error"),
                    "Expected first-error in message, got: {}",
                    message
                );
                assert!(
                    !message.contains("second-error"),
                    "Second hook should not have run"
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }

    #[test]
    fn test_mixed_hooks_legacy_then_conditional() {
        use crate::config::HookCondition;
        // Legacy hook (fire-and-forget) + conditional hook (succeeds) → Allow
        let hooks = vec![
            StopHook {
                command: "echo legacy".to_string(),
                condition: None,
            },
            StopHook {
                command: "true".to_string(),
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
            },
        ];
        let filter = StopHookFilter::new(hooks);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_conditional_hook_stdout_in_block_reason() {
        use crate::config::HookCondition;
        // stdout should also be included in block reason
        let hooks = vec![StopHook {
            command: "sh -c 'echo stdout-content; echo stderr-content >&2; exit 1'".to_string(),
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
        }];
        let filter = StopHookFilter::new(hooks);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("stdout-content") || message.contains("stderr-content"),
                    "Expected output in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }
}
