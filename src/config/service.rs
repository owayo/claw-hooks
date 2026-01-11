//! Configuration service for loading and generating config files.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use super::types::default_log_path_for_config_dir;
use super::Config;

/// Configuration service.
pub struct ConfigService;

impl ConfigService {
    /// Get the default configuration file path.
    /// Always uses ~/.config/claw-hooks/config.toml for cross-platform consistency.
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("claw-hooks")
            .join("config.toml")
    }

    /// Load configuration from file.
    ///
    /// If `path` is `None`, uses the default path.
    /// If the file doesn't exist, creates default configuration file.
    /// Validates configuration after loading.
    /// Log path defaults to the same directory as config file.
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let path = path.map(PathBuf::from).unwrap_or_else(Self::default_path);
        let config_dir = path.parent();

        if !path.exists() {
            // Create default config file
            Self::generate_at(&path)?;
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let mut config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        // If log_path was not explicitly set in config, use config file directory
        // Check if log_path matches the general default (meaning it wasn't set in file)
        let general_default = default_log_path_for_config_dir(None);
        if config.log_path == general_default {
            config.log_path = default_log_path_for_config_dir(config_dir);
        }

        // Validate configuration
        config
            .validate()
            .with_context(|| format!("Invalid configuration in {}", path.display()))?;

        Ok(config)
    }

    /// Generate default configuration file at the default path.
    pub fn generate_default() -> Result<()> {
        Self::generate_at(&Self::default_path())
    }

    /// Generate default configuration file at the specified path.
    pub fn generate_at(path: &Path) -> Result<()> {
        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create config directory: {}", parent.display())
            })?;
        }

        let content = Self::default_config_content();
        fs::write(path, content)
            .with_context(|| format!("Failed to write config file: {}", path.display()))?;

        Ok(())
    }

    /// Generate default configuration content with comments.
    fn default_config_content() -> String {
        r#"# claw-hooks configuration file
# https://github.com/owayo/claw-hooks

# Enable blocking of rm/rmdir/del/erase commands (default: true)
rm_block = true
# Custom message for rm blocking (recommended: use with safe-rm)
# safe-rm: https://github.com/owayo/safe-rm
rm_block_message = "🚫 Use safe-rm instead: safe-rm <file> (validates Git status and path containment). Only clean/ignored files in project allowed."

# Enable blocking of kill/pkill/killall/taskkill commands (default: true)
kill_block = true
# Custom message for kill blocking (recommended: use with safe-kill)
# safe-kill: https://github.com/owayo/safe-kill
kill_block_message = "🚫 Use safe-kill instead: safe-kill <PID>, safe-kill -N <name> (pkill-style), or safe-kill -p <port>. Use -s <signal> for signal."

# Enable blocking of dd command (default: true)
dd_block = true
# Custom message for dd blocking
dd_block_message = "🚫 dd command blocked for safety."

# Enable debug logging to file (default: false)
debug = false

# Path to log directory (default: same directory as config.toml/logs)
# If --config is specified, logs go to that directory/logs
# log_path = "~/.config/claw-hooks/logs"

# Custom command filters
# Block specific commands and suggest alternatives
# [[custom_filters]]
# command = "python"
# message = "⚠️ Use `uv` instead of `python`"

# [[custom_filters]]
# command = "yarn"
# message = "⚠️ Use `pnpm` instead of `yarn`"

# Extension-based hooks (map format)
# Execute external tools when specific file types are modified
# [extension_hooks]
# ".rs" = ["rustfmt {file}"]
# ".go" = ["gofmt -w {file}", "golangci-lint run {file}"]
# ".py" = ["ruff format {file}", "ruff check --fix {file}"]
# ".ts" = ["biome format --write {file}", "biome lint --write {file}"]
# ".tsx" = ["biome format --write {file}", "biome lint --write {file}"]
# ".css" = ["biome format --write {file}", "biome lint --write {file}"]

# Stop hooks
# Execute commands when the agent loop ends (notifications, sounds, cleanup)
# [[stop_hooks]]
# command = "afplay /System/Library/Sounds/Glass.aiff"  # macOS notification sound

# [[stop_hooks]]
# command = "notify-send 'Agent completed'"  # Linux notification
"#
        .to_string()
    }
}
