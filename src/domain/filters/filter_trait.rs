//! Filter trait definition.

use crate::domain::{Decision, HookInput};

/// Trait for command filters.
pub trait Filter: Send + Sync {
    /// Check if this filter applies to the given input.
    fn applies_to(&self, input: &HookInput) -> bool;

    /// Execute the filter and return a decision.
    fn execute(&self, input: &HookInput) -> Decision;

    /// Get the priority of this filter (lower = higher priority).
    fn priority(&self) -> u32;
}
