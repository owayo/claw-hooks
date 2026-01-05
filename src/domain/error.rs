//! Error types for claw-hooks.

use thiserror::Error;

/// Main error type for claw-hooks.
#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum ClawError {
    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(String),

    /// Hook processing error
    #[error("Hook error: {0}")]
    Hook(String),

    /// I/O error
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON parsing error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Regex error
    #[error("Regex error: {0}")]
    Regex(#[from] regex::Error),
}
