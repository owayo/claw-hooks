//! Configuration management module.
//!
//! Handles TOML configuration file loading, validation, and default generation.

mod service;
mod types;
mod validation;

pub use types::Config;

// Re-export for use in other modules
pub use service::ConfigService;
#[allow(unused_imports)]
pub(crate) use types::{CustomFilter, StopHook};
pub use validation::validate;
