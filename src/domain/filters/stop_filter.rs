//! Stop event hook filter implementation.

use std::process::{Command, Output};
use tracing::{debug, warn};

use super::Filter;
use crate::config::StopHook;
use crate::domain::{Decision, HookEvent, HookInput};

/// Filter for Stop event hooks.
pub struct StopHookFilter {
    hooks: Vec<StopHook>,
    nano_buddy: bool,
}

impl StopHookFilter {
    /// Create a new StopHookFilter.
    pub fn new(hooks: Vec<StopHook>, nano_buddy: bool) -> Self {
        Self { hooks, nano_buddy }
    }

    /// Execute a single command string safely.
    /// Uses shell-aware tokenizer to properly handle quoted arguments.
    fn execute_command(command: &str) -> Result<Output, String> {
        let parts = crate::domain::parse_shell_tokens(command);
        if parts.is_empty() {
            return Err("Empty command".to_string());
        }

        let program = &parts[0];
        let args = &parts[1..];

        debug!("🛑 Executing stop hook: {} {:?}", program, args);

        Command::new(program)
            .args(args)
            .output()
            .map_err(|e| format!("Failed to execute stop hook '{}': {}", command, e))
    }

    /// Build a block reason from command output (stdout + stderr).
    fn build_reason(command: &str, output: &Output) -> String {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let mut reason = format!("Stop hook failed: {}\n", command);
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

        // Phase 1: Evaluate conditions and collect commands to execute
        let mut unconditional_commands: Vec<String> = Vec::new();
        let mut conditional_commands: Vec<String> = Vec::new();

        for hook in &self.hooks {
            match &hook.condition {
                None => {
                    unconditional_commands.extend(hook.commands.iter().cloned());
                }
                Some(condition) => {
                    if condition.is_satisfied(&cwd) {
                        debug!("Stop hook condition met, queuing: {:?}", hook.commands);
                        conditional_commands.extend(hook.commands.iter().cloned());
                    } else {
                        debug!("Stop hook condition not met, skipping: {:?}", hook.commands);
                    }
                }
            }
        }

        // Phase 2: Run unconditional commands in parallel.
        // Unconditional hooks never block the decision, but we still wait for completion
        // so they are not dropped by process::exit in HookService::run().
        let unconditional_handles: Vec<_> = unconditional_commands
            .into_iter()
            .map(|command| {
                std::thread::spawn(move || match Self::execute_command(&command) {
                    Ok(output) => {
                        if !output.status.success() {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            warn!("🛑 Stop hook command failed: {}", stderr);
                        }
                    }
                    Err(e) => {
                        warn!("🛑 Stop hook failed: {}", e);
                    }
                })
            })
            .collect();

        // Phase 3: Execute conditional commands in parallel, wait for all results
        let mut failures: Vec<String> = Vec::new();
        let conditional_handles: Vec<_> = conditional_commands
            .into_iter()
            .map(|command| {
                std::thread::spawn(move || -> Option<String> {
                    match Self::execute_command(&command) {
                        Ok(output) => {
                            if output.status.success() {
                                None
                            } else {
                                Some(Self::build_reason(&command, &output))
                            }
                        }
                        Err(e) => Some(e),
                    }
                })
            })
            .collect();

        for handle in conditional_handles {
            match handle.join() {
                Ok(Some(reason)) => failures.push(reason),
                Ok(None) => {}
                Err(_) => failures.push("Stop hook thread panicked".to_string()),
            }
        }

        // Unconditional hook failures are intentionally ignored (warn only), but panic is logged.
        for handle in unconditional_handles {
            if handle.join().is_err() {
                warn!("🛑 Unconditional stop hook thread panicked");
            }
        }

        // NanoBuddy notification (after all stop hooks complete)
        if self.nano_buddy {
            debug!("🐱 NanoBuddy stop notification");
            crate::notify::nano_buddy::notify_stop_hook();
        }

        if failures.is_empty() {
            Decision::allow()
        } else {
            Decision::Block {
                message: failures.join("\n\n"),
            }
        }
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
            commands: vec!["echo done".to_string()],
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks, false);

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
            commands: vec!["echo done".to_string()],
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks, false);

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
            commands: vec!["echo done".to_string()],
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks, false);

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
    fn test_execute_command_empty_is_error() {
        assert!(StopHookFilter::execute_command("   ").is_err());
    }

    #[test]
    fn test_execute_command_with_quoted_args() {
        assert!(StopHookFilter::execute_command("echo 'hello world'").is_ok());
    }

    #[test]
    fn test_execute_ignores_hook_failure_and_allows() {
        // Non-existent command should fail but filter should still return Allow
        let hooks = vec![StopHook {
            commands: vec!["nonexistent-command-xyz-abc-123".to_string()],
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks, false);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput::default()),
            session_id: None,
        };

        let decision = filter.execute(&stop_input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_priority() {
        let filter = StopHookFilter::new(vec![], false);
        assert_eq!(filter.priority(), 100);
    }

    // === Loop prevention ===

    #[test]
    fn test_stop_hook_active_true_skips_all_hooks() {
        let hooks = vec![StopHook {
            commands: vec!["echo should-not-run".to_string()],
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks, false);

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
        let hooks = vec![StopHook {
            commands: vec!["echo running".to_string()],
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks, false);

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
        let hooks = vec![StopHook {
            commands: vec!["nonexistent-command-xyz".to_string()],
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks, false);

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

    // === Conditional hooks ===

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
        let hooks = vec![StopHook {
            commands: vec!["false".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("nonexistent-file-xyz-abc.toml".to_string()),
            }),
        }];
        let filter = StopHookFilter::new(hooks, false);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_conditional_hook_file_exists_command_succeeds() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec!["true".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
        }];
        let filter = StopHookFilter::new(hooks, false);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_conditional_hook_file_exists_command_fails_blocks() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo lint-error >&2; exit 1'".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
        }];
        let filter = StopHookFilter::new(hooks, false);
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
        let hooks = vec![StopHook {
            commands: vec!["nonexistent-lint-tool-xyz-123".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
        }];
        let filter = StopHookFilter::new(hooks, false);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_multiple_hooks_both_fail_collects_all() {
        use crate::config::HookCondition;
        // All conditional hooks run in parallel, both failures collected
        let hooks = vec![
            StopHook {
                commands: vec!["sh -c 'echo first-error >&2; exit 1'".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
            },
            StopHook {
                commands: vec!["sh -c 'echo second-error >&2; exit 1'".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
            },
        ];
        let filter = StopHookFilter::new(hooks, false);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("first-error"),
                    "Expected first-error in message, got: {}",
                    message
                );
                assert!(
                    message.contains("second-error"),
                    "Expected second-error in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }

    #[test]
    fn test_mixed_hooks_unconditional_then_conditional() {
        use crate::config::HookCondition;
        let hooks = vec![
            StopHook {
                commands: vec!["echo unconditional".to_string()],
                condition: None,
            },
            StopHook {
                commands: vec!["true".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
            },
        ];
        let filter = StopHookFilter::new(hooks, false);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_conditional_hook_stdout_in_block_reason() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec![
                "sh -c 'echo stdout-content; echo stderr-content >&2; exit 1'".to_string(),
            ],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
        }];
        let filter = StopHookFilter::new(hooks, false);
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

    #[test]
    fn test_conditional_hook_multiple_commands_all_succeed() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec!["true".to_string(), "echo ok".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
        }];
        let filter = StopHookFilter::new(hooks, false);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_conditional_hook_multiple_commands_second_fails_blocks() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec![
                "true".to_string(),
                "sh -c 'echo second-cmd-error >&2; exit 1'".to_string(),
            ],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
        }];
        let filter = StopHookFilter::new(hooks, false);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("second-cmd-error"),
                    "Expected second-cmd-error in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }

    #[test]
    fn test_conditional_hook_multiple_commands_both_fail_collects_all() {
        use crate::config::HookCondition;
        // All commands run in parallel, both failures collected
        let hooks = vec![StopHook {
            commands: vec![
                "sh -c 'echo first-error >&2; exit 1'".to_string(),
                "sh -c 'echo second-error >&2; exit 1'".to_string(),
            ],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
        }];
        let filter = StopHookFilter::new(hooks, false);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("first-error"),
                    "Expected first-error in message, got: {}",
                    message
                );
                assert!(
                    message.contains("second-error"),
                    "Expected second-error in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }

    #[test]
    fn test_unconditional_hook_multiple_commands_fire_and_forget() {
        let hooks = vec![StopHook {
            commands: vec![
                "echo first".to_string(),
                "nonexistent-command-xyz".to_string(),
                "echo third".to_string(),
            ],
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks, false);
        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_unconditional_hook_is_completed_before_execute_returns() {
        let marker =
            std::env::temp_dir().join(format!("claw-hooks-stop-marker-{}", std::process::id()));
        let marker_path = marker.to_string_lossy().replace('\'', "'\\''");

        let _ = std::fs::remove_file(&marker);

        let hooks = vec![StopHook {
            commands: vec![format!("sh -c 'sleep 0.1; echo done > {}'", marker_path)],
            condition: None,
        }];
        let filter = StopHookFilter::new(hooks, false);

        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
        assert!(
            marker.exists(),
            "unconditional hook should complete before execute() returns"
        );

        let _ = std::fs::remove_file(marker);
    }
}
