//! Service layer containing business logic orchestration.

mod adapter;
mod hook_service;

// Allow unused for potential library API usage
#[allow(unused)]
pub use adapter::FormatAdapter;

pub use hook_service::HookService;
