//! Subagent event filter implementation.
//!
//! Sends NanoBuddy SubagentStart/SubagentStop notifications.
//! This is an observational filter that always allows the event.

use tracing::debug;

use super::Filter;
use crate::domain::{Decision, HookEvent, HookInput};

/// Filter for SubagentStart/SubagentStop events.
///
/// Sends NanoBuddy notifications and always allows.
/// Only registered when NanoBuddy is enabled (see FilterChain::new).
pub struct SubagentFilter;

impl SubagentFilter {
    /// Create a new SubagentFilter.
    pub fn new() -> Self {
        Self
    }

    /// Extract subagent_type from hook input.
    /// Returns `None` if the field is absent or empty.
    fn subagent_type(input: &HookInput) -> Option<&str> {
        if let crate::domain::ToolInput::Subagent(ref subagent) = input.tool_input {
            subagent.subagent_type.as_deref().filter(|s| !s.is_empty())
        } else {
            None
        }
    }
}

impl Filter for SubagentFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        input.event == HookEvent::SubagentStart || input.event == HookEvent::SubagentStop
    }

    fn execute(&self, input: &HookInput) -> Decision {
        if let Some(subagent_type) = Self::subagent_type(input) {
            let session_id = input.session_id.as_deref();
            match input.event {
                HookEvent::SubagentStart => {
                    debug!(
                        "🐱 NanoBuddy subagent.start notification: {}",
                        subagent_type
                    );
                    crate::notify::nano_buddy::notify_subagent_start(subagent_type, session_id);
                }
                HookEvent::SubagentStop => {
                    debug!("🐱 NanoBuddy subagent.stop notification: {}", subagent_type);
                    crate::notify::nano_buddy::notify_subagent_stop(subagent_type, session_id);
                }
                _ => {}
            }
        }

        Decision::allow()
    }

    fn priority(&self) -> u32 {
        90 // Between custom filters and extension/stop hooks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{SubagentInput, ToolInput};

    fn make_subagent_start_input(subagent_type: &str) -> HookInput {
        HookInput {
            event: HookEvent::SubagentStart,
            tool_name: "SubagentStart".to_string(),
            tool_input: ToolInput::Subagent(SubagentInput {
                subagent_type: Some(subagent_type.to_string()),
                prompt: Some("Explore the codebase".to_string()),
                status: None,
                duration: None,
            }),
            session_id: None,
        }
    }

    fn make_subagent_stop_input(subagent_type: &str) -> HookInput {
        HookInput {
            event: HookEvent::SubagentStop,
            tool_name: "SubagentStop".to_string(),
            tool_input: ToolInput::Subagent(SubagentInput {
                subagent_type: Some(subagent_type.to_string()),
                status: Some("completed".to_string()),
                duration: Some(5000),
                ..Default::default()
            }),
            session_id: None,
        }
    }

    #[test]
    fn test_applies_to_subagent_start() {
        let filter = SubagentFilter::new();
        assert!(filter.applies_to(&make_subagent_start_input("explore")));
    }

    #[test]
    fn test_applies_to_subagent_stop() {
        let filter = SubagentFilter::new();
        assert!(filter.applies_to(&make_subagent_stop_input("explore")));
    }

    #[test]
    fn test_does_not_apply_to_other_events() {
        let filter = SubagentFilter::new();
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
    fn test_always_allows_subagent_start() {
        let filter = SubagentFilter::new();
        let decision = filter.execute(&make_subagent_start_input("generalPurpose"));
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_always_allows_subagent_stop() {
        let filter = SubagentFilter::new();
        let decision = filter.execute(&make_subagent_stop_input("generalPurpose"));
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_subagent_type_extraction() {
        let input = make_subagent_start_input("explore");
        assert_eq!(SubagentFilter::subagent_type(&input), Some("explore"));
    }

    #[test]
    fn test_subagent_type_none_when_absent() {
        let input = HookInput {
            event: HookEvent::SubagentStart,
            tool_name: "SubagentStart".to_string(),
            tool_input: ToolInput::Subagent(SubagentInput::default()),
            session_id: None,
        };
        assert_eq!(SubagentFilter::subagent_type(&input), None);
    }

    #[test]
    fn test_subagent_type_none_when_empty() {
        let input = HookInput {
            event: HookEvent::SubagentStart,
            tool_name: "SubagentStart".to_string(),
            tool_input: ToolInput::Subagent(SubagentInput {
                subagent_type: Some("".to_string()),
                ..Default::default()
            }),
            session_id: None,
        };
        assert_eq!(SubagentFilter::subagent_type(&input), None);
    }

    #[test]
    fn test_execute_with_session_id() {
        let filter = SubagentFilter::new();
        let mut input = make_subagent_start_input("Explore");
        input.session_id = Some("abc-123".to_string());
        let decision = filter.execute(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_priority() {
        let filter = SubagentFilter::new();
        assert_eq!(filter.priority(), 90);
    }
}
