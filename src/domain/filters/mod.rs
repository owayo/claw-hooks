//! Filter system for command filtering.

mod chain;
mod custom_filter;
mod dd_filter;
mod extension_filter;
mod filter_trait;
mod kill_filter;
mod rm_filter;
mod stop_filter;

pub use chain::FilterChain;
pub use custom_filter::CustomCommandFilter;
pub use dd_filter::DdFilter;
pub use extension_filter::ExtensionHookFilter;
pub use filter_trait::Filter;
pub use kill_filter::KillFilter;
pub use rm_filter::RmFilter;
pub use stop_filter::StopHookFilter;
