//! Stop event hook filter implementation.

use std::process::Output;
use tracing::{debug, info, warn};

use super::Filter;
use crate::config::StopHook;
use crate::domain::command::{TimedOutput, run_with_timeout_tracked, spawn_piped_with_env};
use crate::domain::{Decision, HookEvent, HookInput};

/// Environment variable to prevent recursive stop hook execution across processes.
/// When claw-hooks executes stop hooks, this env var is set on child processes.
/// If a child process (e.g. git-sc → Gemini CLI) triggers another claw-hooks stop event,
/// this env var will be inherited and claw-hooks will skip stop hooks to break the loop.
const STOP_ACTIVE_ENV: &str = "CLAW_HOOKS_STOP_ACTIVE";

/// Filter for Stop event hooks.
pub struct StopHookFilter {
    hooks: Vec<StopHook>,
    nano_buddy: bool,
    timeout_secs: u64,
}

impl StopHookFilter {
    /// Create a new StopHookFilter.
    pub fn new(hooks: Vec<StopHook>, nano_buddy: bool, timeout_secs: u64) -> Self {
        Self {
            hooks,
            nano_buddy,
            timeout_secs,
        }
    }

    /// Environment variable to pass agent's last message to child processes.
    /// This allows tools like git-sc to use the agent's context for better commit messages.
    const AGENT_MESSAGE_ENV: &'static str = "CLAW_HOOKS_AGENT_MESSAGE";

    /// Execute a single command string safely with timeout.
    /// Uses shell-aware tokenizer to properly handle quoted arguments.
    /// Sets `CLAW_HOOKS_STOP_ACTIVE=1` on the child process to prevent recursive loops.
    /// Optionally sets `CLAW_HOOKS_AGENT_MESSAGE` with the agent's last message.
    fn execute_command_tracked(
        command: &str,
        timeout_secs: u64,
        agent_message: Option<&str>,
    ) -> Result<TimedOutput, String> {
        let parts = crate::domain::parse_shell_tokens(command);
        if parts.is_empty() {
            return Err("Empty command".to_string());
        }

        let program = &parts[0];
        let args = &parts[1..];

        debug!("🛑 Executing stop hook: {} {:?}", program, args);

        let mut envs: Vec<(&str, &str)> = vec![(STOP_ACTIVE_ENV, "1")];
        if let Some(msg) = agent_message {
            envs.push((Self::AGENT_MESSAGE_ENV, msg));
        }

        let start = std::time::Instant::now();
        let child = spawn_piped_with_env(program, args, &envs)
            .map_err(|e| format!("Failed to execute stop hook '{}': {}", command, e))?;
        let result = run_with_timeout_tracked(child, timeout_secs, command);
        let elapsed = start.elapsed();
        info!(
            "⏰️ Stop hook [{}] completed in {:.2}s",
            command,
            elapsed.as_secs_f64()
        );
        result
    }

    /// Execute a single stop hook command and return captured process output.
    #[cfg(test)]
    fn execute_command(
        command: &str,
        timeout_secs: u64,
        agent_message: Option<&str>,
    ) -> Result<Output, String> {
        Self::execute_command_tracked(command, timeout_secs, agent_message).map(|r| r.output)
    }

    /// Log stdout/stderr output from a stop hook command.
    fn log_output(command: &str, output: &Output) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stdout.trim().is_empty() {
            info!("Stop hook [{}] stdout:\n{}", command, stdout.trim());
        }
        if !stderr.trim().is_empty() {
            info!("Stop hook [{}] stderr:\n{}", command, stderr.trim());
        }
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
        // Check environment variable to prevent cross-process recursive loops.
        // When claw-hooks runs stop hooks, child processes inherit CLAW_HOOKS_STOP_ACTIVE=1.
        // If those children (e.g. git-sc → Gemini CLI) trigger another claw-hooks stop event,
        // this check breaks the loop.
        if std::env::var(STOP_ACTIVE_ENV).is_ok() {
            debug!(
                "⛔ {}=1 detected, skipping all stop hooks (cross-process loop prevention)",
                STOP_ACTIVE_ENV
            );
            return Decision::allow();
        }

        // Check stop_hook_active to prevent infinite loops (agent-internal flag)
        // Also extract agent_message for passing to child processes
        let agent_message = if let crate::domain::ToolInput::Stop(ref stop_input) = input.tool_input
        {
            if stop_input.stop_hook_active {
                debug!("🛑 stop_hook_active=true, skipping all stop hooks");
                return Decision::allow();
            }
            stop_input.agent_message.clone()
        } else {
            None
        };

        // NanoBuddy notification (before all stop hooks so it arrives first)
        if self.nano_buddy {
            debug!("🐱 NanoBuddy stop notification");
            crate::notify::nano_buddy::notify_stop_hook();
        }

        let cwd = std::env::current_dir().unwrap_or_default();
        let timeout_secs = self.timeout_secs;

        // Collect hooks that pass their condition check, tagged with report flag
        struct QualifiedCommand {
            command: String,
            report: bool,
        }

        // Group commands by stage (ascending order)
        let mut stage_map: std::collections::BTreeMap<u8, Vec<QualifiedCommand>> =
            std::collections::BTreeMap::new();

        for hook in &self.hooks {
            if let Some(ref condition) = hook.condition {
                if !condition.is_satisfied(&cwd) {
                    debug!("Stop hook condition not met, skipping: {:?}", hook.commands);
                    continue;
                }
                debug!("Stop hook condition met, queuing: {:?}", hook.commands);
            }

            let stage = hook.stage_value();
            let report = hook.should_report();
            let entry = stage_map.entry(stage).or_default();
            for cmd in &hook.commands {
                entry.push(QualifiedCommand {
                    command: cmd.clone(),
                    report,
                });
            }
        }

        // Execute stages sequentially (1 → 5), commands within each stage in parallel
        let mut failures: Vec<String> = Vec::new();

        for (stage, commands) in &stage_map {
            debug!("▶ Executing stop hook stage {}", stage);

            let handles: Vec<_> = commands
                .iter()
                .map(|qc| {
                    let command = qc.command.clone();
                    let report = qc.report;
                    let agent_msg = agent_message.clone();
                    std::thread::spawn(move || -> (bool, Option<String>) {
                        match Self::execute_command_tracked(
                            &command,
                            timeout_secs,
                            agent_msg.as_deref(),
                        ) {
                            Ok(result) => {
                                Self::log_output(&command, &result.output);
                                if result.output.status.success() || result.timed_out {
                                    (report, None)
                                } else if report {
                                    (report, Some(Self::build_reason(&command, &result.output)))
                                } else {
                                    warn!(
                                        "⚠️ Stop hook command failed (exit {}): {}",
                                        result.output.status, command
                                    );
                                    (report, None)
                                }
                            }
                            Err(e) => {
                                if report {
                                    (report, Some(e))
                                } else {
                                    warn!("❌ Stop hook failed: {}", e);
                                    (report, None)
                                }
                            }
                        }
                    })
                })
                .collect();

            for handle in handles {
                match handle.join() {
                    Ok((_, Some(reason))) => failures.push(reason),
                    Ok((_, None)) => {}
                    Err(_) => {
                        warn!("🛑 Stop hook thread panicked");
                        failures.push("Stop hook thread panicked".to_string());
                    }
                }
            }
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
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

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
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

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
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

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
        assert!(StopHookFilter::execute_command("   ", 60, None).is_err());
    }

    #[test]
    fn test_execute_command_with_quoted_args() {
        assert!(StopHookFilter::execute_command("echo 'hello world'", 60, None).is_ok());
    }

    #[test]
    fn test_execute_ignores_hook_failure_and_allows() {
        // Non-existent command should fail but filter should still return Allow
        let hooks = vec![StopHook {
            commands: vec!["nonexistent-command-xyz-abc-123".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

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
        let filter = StopHookFilter::new(vec![], false, 60);
        assert_eq!(filter.priority(), 100);
    }

    // === Loop prevention ===

    #[test]
    fn test_stop_hook_active_true_skips_all_hooks() {
        let hooks = vec![StopHook {
            commands: vec!["echo should-not-run".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                status: None,
                loop_count: None,
                response: None,
                stop_hook_active: true,
                ..Default::default()
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
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                status: None,
                loop_count: None,
                response: None,
                stop_hook_active: false,
                ..Default::default()
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
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                status: None,
                loop_count: None,
                response: None,
                stop_hook_active: false,
                ..Default::default()
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
                ..Default::default()
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

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
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

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
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

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
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
    fn test_conditional_hook_fake_timeout_message_with_exit_124_blocks() {
        use crate::config::HookCondition;
        let hooks = vec![StopHook {
            commands: vec![
                "sh -c 'echo \"[Command timed out after 2s: fake]\" >&2; exit 124'".to_string(),
            ],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 2);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("timed out after 2s: fake"),
                    "Expected fake timeout stderr in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision for fake timeout output"),
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

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
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

                stage: None,

                report: None,
            },
            StopHook {
                commands: vec!["sh -c 'echo second-error >&2; exit 1'".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),

                stage: None,

                report: None,
            },
        ];
        let filter = StopHookFilter::new(hooks, false, 60);
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
                stage: None,
                report: None,
            },
            StopHook {
                commands: vec!["true".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),

                stage: None,

                report: None,
            },
        ];
        let filter = StopHookFilter::new(hooks, false, 60);
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

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
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

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
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

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
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

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
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
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
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
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let decision = filter.execute(&make_stop_input());
        assert!(matches!(decision, Decision::Allow { .. }));
        assert!(
            marker.exists(),
            "unconditional hook should complete before execute() returns"
        );

        let _ = std::fs::remove_file(marker);
    }

    // === Timeout tests ===

    #[test]
    fn test_execute_command_timeout_kills_process() {
        // sleep 10 should be killed after 2 second timeout
        let start = std::time::Instant::now();
        let result = StopHookFilter::execute_command_tracked("sleep 10", 2, None);
        let elapsed = start.elapsed();

        // Timeout should return explicit timeout output
        assert!(result.is_ok(), "Timeout should return Ok");
        let result = result.unwrap();
        assert!(result.timed_out, "Output should indicate timeout result");
        let output = result.output;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("timed out"),
            "Stderr should contain timeout notice: {}",
            stderr
        );
        assert!(
            elapsed.as_secs() < 5,
            "Should have timed out in ~2s, took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_execute_command_completes_before_timeout() {
        let result = StopHookFilter::execute_command("echo hello", 60, None);
        assert!(result.is_ok(), "Should complete before timeout");
    }

    #[test]
    fn test_stop_hook_timeout_unconditional() {
        // Unconditional hook with timeout should allow (fire-and-forget, timeout just warns)
        let hooks = vec![StopHook {
            commands: vec!["sleep 10".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 2);

        let start = std::time::Instant::now();
        let decision = filter.execute(&make_stop_input());
        let elapsed = start.elapsed();

        assert!(matches!(decision, Decision::Allow { .. }));
        assert!(
            elapsed.as_secs() < 5,
            "Unconditional hook should timeout in ~2s, took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_stop_hook_timeout_conditional_allows() {
        use crate::config::HookCondition;
        // Conditional hook with timeout should allow (timeout treated as success)
        let hooks = vec![StopHook {
            commands: vec!["sleep 10".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 2);

        let start = std::time::Instant::now();
        let decision = filter.execute(&make_stop_input());
        let elapsed = start.elapsed();

        assert!(
            matches!(decision, Decision::Allow { .. }),
            "Timeout should be treated as Allow, got: {:?}",
            decision
        );
        assert!(
            elapsed.as_secs() < 5,
            "Conditional hook should timeout in ~2s, took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_execute_command_timeout_returns_timeout_notice() {
        // Command that outputs then hangs: timeout should return Ok with notice
        let result = StopHookFilter::execute_command_tracked(
            "sh -c 'echo before-timeout; sleep 30'",
            2,
            None,
        );
        // Timeout is treated as Ok
        assert!(result.is_ok(), "Timeout should return Ok");
        let result = result.unwrap();
        assert!(result.timed_out, "Expected tracked timeout output");
        let output = result.output;
        // stdout is empty on timeout (reader threads are not joined to avoid blocking)
        assert!(
            output.stdout.is_empty(),
            "Stdout should be empty on timeout"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("timed out"),
            "Stderr should contain timeout notice: {}",
            stderr
        );
    }

    #[test]
    fn test_stop_hook_timeout_mixed_fast_and_slow_conditional() {
        use crate::config::HookCondition;
        // Two conditional commands in parallel: one fast success, one timeout
        // Timeout is treated as success, so both should allow
        let hooks = vec![StopHook {
            commands: vec!["true".to_string(), "sleep 10".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 2);

        let start = std::time::Instant::now();
        let decision = filter.execute(&make_stop_input());
        let elapsed = start.elapsed();

        assert!(
            matches!(decision, Decision::Allow { .. }),
            "Timeout should be treated as Allow, got: {:?}",
            decision
        );
        assert!(
            elapsed.as_secs() < 5,
            "Should timeout in ~2s, took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_stop_hook_timeout_mixed_fast_and_slow_unconditional() {
        // Unconditional: one fast, one slow. Both should complete/timeout without blocking
        let hooks = vec![StopHook {
            commands: vec!["echo fast".to_string(), "sleep 10".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 2);

        let start = std::time::Instant::now();
        let decision = filter.execute(&make_stop_input());
        let elapsed = start.elapsed();

        assert!(
            matches!(decision, Decision::Allow { .. }),
            "Unconditional hooks always allow"
        );
        assert!(
            elapsed.as_secs() < 5,
            "Should timeout in ~2s, took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_stop_hook_timeout_process_is_killed() {
        // Use a marker file: the sleep process should be killed before it creates it
        let marker =
            std::env::temp_dir().join(format!("claw-hooks-timeout-kill-{}", std::process::id()));
        let marker_path = marker.to_string_lossy().replace('\'', "'\\''");
        let _ = std::fs::remove_file(&marker);

        // Command: sleep 10 then create marker. If killed properly, marker won't exist
        let cmd = format!("sh -c 'sleep 10; echo done > {}'", marker_path);
        let result = StopHookFilter::execute_command_tracked(&cmd, 2, None);
        assert!(result.is_ok(), "Timeout should return Ok");
        assert!(result.unwrap().timed_out, "Expected timeout kill");

        // Give a moment for any zombie/orphan cleanup
        std::thread::sleep(std::time::Duration::from_millis(500));

        assert!(
            !marker.exists(),
            "Marker file should not exist because process was killed before sleep finished"
        );

        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn test_stop_hook_custom_timeout_value() {
        // Verify custom timeout value is respected (3s timeout, 1s command succeeds)
        let hooks = vec![StopHook {
            commands: vec!["sleep 1".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 3);

        let start = std::time::Instant::now();
        let decision = filter.execute(&make_stop_input());
        let elapsed = start.elapsed();

        assert!(matches!(decision, Decision::Allow { .. }));
        assert!(
            elapsed.as_secs() < 3,
            "1s command should complete before 3s timeout: {:?}",
            elapsed
        );
    }

    // === Agent message propagation tests ===

    #[test]
    fn test_execute_command_passes_agent_message_env() {
        // Verify CLAW_HOOKS_AGENT_MESSAGE is set when agent_message is provided
        let result = StopHookFilter::execute_command(
            "sh -c 'echo $CLAW_HOOKS_AGENT_MESSAGE'",
            60,
            Some("test agent message"),
        );
        assert!(result.is_ok());
        let output = result.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("test agent message"),
            "Expected agent message in stdout, got: {}",
            stdout
        );
    }

    #[test]
    fn test_execute_command_no_agent_message_env_when_none() {
        // Verify CLAW_HOOKS_AGENT_MESSAGE is not set when agent_message is None
        let result = StopHookFilter::execute_command(
            "sh -c 'echo \"${CLAW_HOOKS_AGENT_MESSAGE:-unset}\"'",
            60,
            None,
        );
        assert!(result.is_ok());
        let output = result.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("unset"),
            "Expected 'unset' in stdout when no agent_message, got: {}",
            stdout
        );
    }

    #[test]
    fn test_stop_hook_propagates_agent_message_to_child() {
        use crate::config::HookCondition;
        // Conditional hook that echoes the agent message env var
        let hooks = vec![StopHook {
            commands: vec![
                "sh -c 'test \"$CLAW_HOOKS_AGENT_MESSAGE\" = \"hello from agent\"'".to_string(),
            ],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),

            stage: None,

            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);

        let stop_input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput {
                agent_message: Some("hello from agent".to_string()),
                ..Default::default()
            }),
            session_id: None,
        };

        let decision = filter.execute(&stop_input);
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "Expected Allow (env var should match), got: {:?}",
            decision
        );
    }

    // === Stage ordering tests ===

    #[test]
    fn test_stage_ordering_lower_stage_runs_first() {
        use crate::config::HookCondition;
        // Stage 1 creates a marker file, Stage 3 checks for it.
        // If stage ordering works, marker will exist when stage 3 runs.
        let marker =
            std::env::temp_dir().join(format!("claw-hooks-stage-order-{}", std::process::id()));
        let marker_path = marker.to_string_lossy().replace('\'', "'\\''");
        let _ = std::fs::remove_file(&marker);

        let hooks = vec![
            StopHook {
                commands: vec![format!("sh -c 'echo done > {}'", marker_path)],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(1),
                report: None,
            },
            StopHook {
                commands: vec![format!(
                    "sh -c 'test -f {} || (echo stage-order-failed >&2; exit 1)'",
                    marker_path
                )],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(3),
                report: Some(true),
            },
        ];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "Stage 3 should see marker from stage 1, got: {:?}",
            decision
        );
        let _ = std::fs::remove_file(marker);
    }

    // === Report behavior tests ===

    #[test]
    fn test_report_false_ignores_failure() {
        use crate::config::HookCondition;
        // Hook with report=false should not block even on failure
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo report-off-error >&2; exit 1'".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
            stage: None,
            report: Some(false),
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "report=false should not block, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_report_true_without_condition_blocks_on_failure() {
        // Hook without condition but report=true should block on failure
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo explicit-report-error >&2; exit 1'".to_string()],
            condition: None,
            stage: None,
            report: Some(true),
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("explicit-report-error"),
                    "Expected error in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision for report=true hook failure"),
        }
    }

    #[test]
    fn test_default_report_no_condition_allows_on_failure() {
        // Hook without condition and without explicit report (defaults to false)
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo no-report-error >&2; exit 1'".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "No condition + no report should default to fire-and-forget, got: {:?}",
            decision
        );
    }

    #[test]
    fn test_default_report_with_condition_blocks_on_failure() {
        use crate::config::HookCondition;
        // Hook with condition and without explicit report (defaults to true)
        let hooks = vec![StopHook {
            commands: vec!["sh -c 'echo default-report-error >&2; exit 1'".to_string()],
            condition: Some(HookCondition {
                command_exists: None,
                file_exists: Some("Cargo.toml".to_string()),
            }),
            stage: None,
            report: None,
        }];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("default-report-error"),
                    "Expected error in message, got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision for conditional hook"),
        }
    }

    #[test]
    fn test_mixed_stages_and_reports() {
        use crate::config::HookCondition;
        // Stage 1: report=false (fire-and-forget)
        // Stage 3: report=true (should block)
        let hooks = vec![
            StopHook {
                commands: vec!["sh -c 'echo stage1-error >&2; exit 1'".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(1),
                report: Some(false),
            },
            StopHook {
                commands: vec!["sh -c 'echo stage3-error >&2; exit 1'".to_string()],
                condition: Some(HookCondition {
                    command_exists: None,
                    file_exists: Some("Cargo.toml".to_string()),
                }),
                stage: Some(3),
                report: Some(true),
            },
        ];
        let filter = StopHookFilter::new(hooks, false, 60);
        let decision = filter.execute(&make_stop_input());
        match decision {
            Decision::Block { message } => {
                assert!(
                    message.contains("stage3-error"),
                    "Expected stage3-error in message, got: {}",
                    message
                );
                assert!(
                    !message.contains("stage1-error"),
                    "Should not contain stage1-error (report=false), got: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }
}
