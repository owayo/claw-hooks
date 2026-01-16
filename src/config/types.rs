//! Configuration data types.

use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::validation;

/// Main configuration structure.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Enable blocking of rm/rmdir commands
    pub rm_block: bool,

    /// Custom message for rm blocking (optional)
    pub rm_block_message: Option<String>,

    /// Enable blocking of kill/pkill/killall commands
    pub kill_block: bool,

    /// Custom message for kill blocking (optional)
    pub kill_block_message: Option<String>,

    /// Enable blocking of dd command
    pub dd_block: bool,

    /// Custom message for dd blocking (optional)
    pub dd_block_message: Option<String>,

    /// Enable debug logging to file
    pub debug: bool,

    /// Path to log directory
    pub log_path: PathBuf,

    /// Custom command filters
    #[serde(default)]
    pub custom_filters: Vec<CustomFilter>,

    /// Extension-based hooks (map format: ".ext" = ["cmd1", "cmd2"])
    #[serde(default)]
    pub extension_hooks: BTreeMap<String, Vec<String>>,

    /// Stop event hooks
    #[serde(default)]
    pub stop_hooks: Vec<StopHook>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rm_block: true,
            rm_block_message: None,
            kill_block: true,
            kill_block_message: None,
            dd_block: true,
            dd_block_message: None,
            debug: false,
            log_path: default_log_path(),
            custom_filters: Vec::new(),
            extension_hooks: BTreeMap::new(),
            stop_hooks: Vec::new(),
        }
    }
}

impl Config {
    /// Validate configuration and return errors if invalid.
    /// Delegates to the comprehensive validation module.
    pub fn validate(&self) -> Result<()> {
        validation::validate(self)
    }
}

/// Custom command filter configuration.
///
/// Two modes are supported:
/// 1. Regex mode: Only `command` field is set (regex pattern)
/// 2. Args mode: Both `command` and `args` fields are set (exact command + args matching)
///
/// # Examples
///
/// Regex mode:
/// ```toml
/// [[custom_filters]]
/// command = "npm (install|i|add)"
/// message = "Use pnpm instead"
/// ```
///
/// Args mode:
/// ```toml
/// [[custom_filters]]
/// command = "npm"
/// args = ["install", "i", "add"]
/// message = "Use pnpm instead"
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct CustomFilter {
    /// Command name (exact match when `args` is specified) or regex pattern
    pub command: String,

    /// Optional list of arguments to match (any match triggers the filter)
    /// When specified, `command` is treated as exact match, not regex
    #[serde(default)]
    pub args: Vec<String>,

    /// Message to display when command is blocked
    pub message: String,
}

/// Stop event hook configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct StopHook {
    /// Command to execute on Stop event
    pub command: String,
}

/// Get default log path (relative to config directory).
/// This returns a placeholder; the actual path is set by ConfigService based on config file location.
pub fn default_log_path() -> PathBuf {
    default_log_path_for_config_dir(None)
}

/// Get log path based on config directory.
pub fn default_log_path_for_config_dir(config_dir: Option<&Path>) -> PathBuf {
    config_dir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
                .join("claw-hooks")
        })
        .join("logs")
}
