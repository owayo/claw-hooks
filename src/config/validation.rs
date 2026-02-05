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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CustomFilter, StopHook};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn default_config() -> Config {
        Config::default()
    }

    #[test]
    fn test_validate_default_config() {
        let config = default_config();
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_rejects_invalid_regex() {
        let mut config = default_config();
        config.custom_filters.push(CustomFilter {
            command: "[".to_string(), // Invalid regex
            args: vec![],
            message: "msg".to_string(),
        });
        assert!(validate(&config).is_err());
    }

    #[test]
    fn test_validate_rejects_empty_custom_filter_command() {
        let mut config = default_config();
        config.custom_filters.push(CustomFilter {
            command: "".to_string(),
            args: vec![],
            message: "msg".to_string(),
        });
        assert!(validate(&config).is_err());
    }

    #[test]
    fn test_validate_rejects_empty_custom_filter_message() {
        let mut config = default_config();
        config.custom_filters.push(CustomFilter {
            command: "npm".to_string(),
            args: vec![],
            message: "".to_string(),
        });
        assert!(validate(&config).is_err());
    }

    #[test]
    fn test_validate_rejects_extension_hooks_without_dot() {
        let mut config = default_config();
        let mut hooks = BTreeMap::new();
        hooks.insert("rs".to_string(), vec!["rustfmt {file}".to_string()]);
        config.extension_hooks = hooks;
        assert!(validate(&config).is_err());
    }

    #[test]
    fn test_validate_rejects_extension_hook_missing_placeholder() {
        let mut config = default_config();
        let mut hooks = BTreeMap::new();
        hooks.insert(".rs".to_string(), vec!["rustfmt".to_string()]);
        config.extension_hooks = hooks;
        assert!(validate(&config).is_err());
    }

    #[test]
    fn test_validate_rejects_empty_extension_hook_command() {
        let mut config = default_config();
        let mut hooks = BTreeMap::new();
        hooks.insert(".rs".to_string(), vec!["".to_string()]);
        config.extension_hooks = hooks;
        assert!(validate(&config).is_err());
    }

    #[test]
    fn test_validate_rejects_empty_stop_hook_command() {
        let mut config = default_config();
        config.stop_hooks.push(StopHook {
            command: "".to_string(),
        });
        assert!(validate(&config).is_err());
    }

    #[test]
    fn test_validate_rejects_log_path_with_nul() {
        let mut config = default_config();
        config.log_path = PathBuf::from("bad\0path");
        assert!(validate(&config).is_err());
    }

    #[test]
    fn test_validate_accepts_valid_extension_hooks() {
        let mut config = default_config();
        let mut hooks = BTreeMap::new();
        hooks.insert(".rs".to_string(), vec!["rustfmt {file}".to_string()]);
        hooks.insert(
            ".go".to_string(),
            vec!["gofmt -w {file}".to_string(), "golint {file}".to_string()],
        );
        config.extension_hooks = hooks;
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_accepts_valid_custom_filters() {
        let mut config = default_config();
        config.custom_filters.push(CustomFilter {
            command: "npm".to_string(),
            args: vec!["install".to_string()],
            message: "Use pnpm instead".to_string(),
        });
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_accepts_valid_stop_hooks() {
        let mut config = default_config();
        config.stop_hooks.push(StopHook {
            command: "notify-send 'Done'".to_string(),
        });
        assert!(validate(&config).is_ok());
    }
}
