//! Configuration validation.

use anyhow::{bail, Result};
use regex::Regex;

use super::Config;

/// Validate configuration.
pub fn validate(config: &Config) -> Result<()> {
    // Validate log path
    if !config.log_path.as_os_str().is_empty() {
        // Path will be created if it doesn't exist, so just check it's valid
        if config.log_path.to_string_lossy().contains('\0') {
            bail!("Invalid log_path: contains null character");
        }
    }

    // Validate custom filters
    for (i, filter) in config.custom_filters.iter().enumerate() {
        if filter.command.is_empty() {
            bail!("custom_filters[{}]: command cannot be empty", i);
        }

        // Validate regex pattern
        if let Err(e) = Regex::new(&filter.command) {
            bail!(
                "custom_filters[{}]: invalid regex pattern '{}': {}",
                i,
                filter.command,
                e
            );
        }

        if filter.message.is_empty() {
            bail!("custom_filters[{}]: message cannot be empty", i);
        }
    }

    // Validate extension hooks (map format)
    for (ext, commands) in &config.extension_hooks {
        if !ext.starts_with('.') {
            bail!("extension_hooks: key '{}' must start with '.'", ext);
        }

        if commands.is_empty() {
            bail!("extension_hooks['{}']: commands cannot be empty", ext);
        }

        // SECURITY: Ensure all commands contain {file} placeholder
        // This is required for safe argument handling
        for (j, cmd) in commands.iter().enumerate() {
            if cmd.is_empty() {
                bail!("extension_hooks['{}']: command[{}] cannot be empty", ext, j);
            }
            if !cmd.contains("{file}") {
                bail!(
                    "extension_hooks['{}']: command[{}] must contain {{file}} placeholder",
                    ext,
                    j
                );
            }
        }
    }

    // Validate stop hooks
    for (i, hook) in config.stop_hooks.iter().enumerate() {
        if hook.command.is_empty() {
            bail!("stop_hooks[{}]: command cannot be empty", i);
        }
    }

    Ok(())
}
