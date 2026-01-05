//! Stop event hook filter implementation.

use std::process::Command;
use tracing::{debug, warn};

use super::Filter;
use crate::config::StopHook;
use crate::domain::{Decision, HookInput};

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
    fn execute_hook(&self, hook: &StopHook) -> Result<(), String> {
        let parts: Vec<&str> = hook.command.split_whitespace().collect();
        if parts.is_empty() {
            return Err("Empty command".to_string());
        }

        let program = parts[0];
        let args = &parts[1..];

        debug!("Executing stop hook: {} {:?}", program, args);

        let mut cmd = Command::new(program);
        cmd.args(args);

        let output = cmd
            .output()
            .map_err(|e| format!("Failed to execute stop hook: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("Stop hook command failed: {}", stderr);
        }

        Ok(())
    }
}

impl Filter for StopHookFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        // Applies only to Stop events
        input.event == "Stop"
    }

    fn execute(&self, _input: &HookInput) -> Decision {
        // Execute all stop hooks
        for hook in &self.hooks {
            if let Err(e) = self.execute_hook(hook) {
                warn!("Stop hook failed: {}", e);
            }
        }

        // Always allow - stop hooks are side effects, not filters
        Decision::Allow
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
        }];
        let filter = StopHookFilter::new(hooks);

        let stop_input = HookInput {
            event: "Stop".to_string(),
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
        }];
        let filter = StopHookFilter::new(hooks);

        let bash_input = HookInput {
            event: "PreToolUse".to_string(),
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
        }];
        let filter = StopHookFilter::new(hooks);

        let stop_input = HookInput {
            event: "Stop".to_string(),
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput::default()),
            session_id: None,
        };

        let decision = filter.execute(&stop_input);
        assert!(matches!(decision, Decision::Allow));
    }
}
