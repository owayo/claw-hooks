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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_error_display() {
        let err = ClawError::Config("missing field".to_string());
        assert_eq!(err.to_string(), "Configuration error: missing field");
    }

    #[test]
    fn test_hook_error_display() {
        let err = ClawError::Hook("invalid input".to_string());
        assert_eq!(err.to_string(), "Hook error: invalid input");
    }

    #[test]
    fn test_io_error_from() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: ClawError = io_err.into();
        assert!(err.to_string().contains("file not found"));
    }

    #[test]
    fn test_json_error_from() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let err: ClawError = json_err.into();
        assert!(err.to_string().starts_with("JSON error:"));
    }

    #[test]
    fn test_regex_error_from() {
        #[allow(clippy::invalid_regex)]
        let regex_err = regex::Regex::new("[invalid").unwrap_err();
        let err: ClawError = regex_err.into();
        assert!(err.to_string().starts_with("Regex error:"));
    }

    #[test]
    fn test_error_debug_format() {
        let err = ClawError::Config("test".to_string());
        let debug = format!("{:?}", err);
        assert!(debug.contains("Config"));
        assert!(debug.contains("test"));
    }
}
