//! Filter chain implementation.

use crate::config::Config;
use crate::domain::Decision;
use crate::domain::HookInput;

use super::{
    CustomCommandFilter, DdFilter, ExtensionHookFilter, Filter, KillFilter, RmFilter,
    StopHookFilter,
};

/// Chain of filters that processes hook inputs.
pub struct FilterChain {
    filters: Vec<Box<dyn Filter>>,
}

impl FilterChain {
    /// Create a new FilterChain from configuration.
    pub fn new(config: &Config) -> Self {
        let mut filters: Vec<Box<dyn Filter>> = Vec::new();

        // Add built-in filters
        filters.push(Box::new(KillFilter::new(
            config.kill_block,
            config.kill_block_message.clone(),
        )));
        filters.push(Box::new(DdFilter::new(
            config.dd_block,
            config.dd_block_message.clone(),
        )));
        filters.push(Box::new(RmFilter::new(
            config.rm_block,
            config.rm_block_message.clone(),
        )));

        // Add custom filters
        for custom in &config.custom_filters {
            let filter: Box<dyn Filter> = if custom.args.is_empty() {
                // Regex mode: command is treated as regex pattern
                if let Ok(f) = CustomCommandFilter::new(&custom.command, custom.message.clone()) {
                    Box::new(f)
                } else {
                    continue;
                }
            } else {
                // Args mode: regex command name + args matching
                if let Ok(f) = CustomCommandFilter::with_args(
                    &custom.command,
                    custom.args.clone(),
                    custom.message.clone(),
                ) {
                    Box::new(f)
                } else {
                    continue;
                }
            };
            filters.push(filter);
        }

        // Add extension hook filter
        if !config.extension_hooks.is_empty() {
            filters.push(Box::new(ExtensionHookFilter::new(
                config.extension_hooks.clone(),
            )));
        }

        // Add stop hook filter
        if !config.stop_hooks.is_empty() {
            filters.push(Box::new(StopHookFilter::new(config.stop_hooks.clone())));
        }

        // Sort by priority (lower = higher priority)
        filters.sort_by_key(|f| f.priority());

        Self { filters }
    }

    /// Execute all applicable filters and return the first blocking decision.
    pub fn execute(&self, input: &HookInput) -> Decision {
        for filter in &self.filters {
            if filter.applies_to(input) {
                let decision = filter.execute(input);
                if matches!(decision, Decision::Block { .. }) {
                    return decision;
                }
            }
        }

        Decision::Allow
    }
}
