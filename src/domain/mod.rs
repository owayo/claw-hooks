//! Domain layer containing core business logic.
//!
//! This module contains:
//! - Input/output data types for hook processing
//! - Filter trait and implementations
//! - Shell command parser
//! - Logger with rotation

mod error;
pub mod filters;
pub mod logger;
pub mod parser;
mod types;

pub use filters::FilterChain;
pub use types::{Decision, HookInput, ToolInput};

// Allow unused for potential future use / library API
#[allow(unused)]
pub use error::ClawError;

#[allow(unused)]
pub use types::{BashInput, FileOperationInput, HookOutput, StopInput};

pub use parser::parse_shell_tokens;
