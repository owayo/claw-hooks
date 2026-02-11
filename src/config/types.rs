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

    /// NanoBuddy連携を有効化 (隠しオプション)
    #[serde(default)]
    pub nano_buddy: bool,
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
            nano_buddy: false,
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

/// Stop hook execution condition.
/// All specified fields are evaluated as AND (all must be satisfied).
#[derive(Debug, Clone, Deserialize)]
pub struct HookCondition {
    /// Execute hook only when this file exists (relative to cwd)
    #[serde(default)]
    pub file_exists: Option<String>,

    /// Execute hook only when this command exists in PATH
    #[serde(default)]
    pub command_exists: Option<String>,
}

impl HookCondition {
    /// Evaluate the condition against the working directory.
    /// Returns true if all specified conditions are satisfied (AND logic).
    pub fn is_satisfied(&self, cwd: &Path) -> bool {
        if let Some(ref file) = self.file_exists {
            if !cwd.join(file).exists() {
                return false;
            }
        }
        if let Some(ref cmd) = self.command_exists {
            if !Self::command_in_path(cmd) {
                return false;
            }
        }
        true
    }

    /// Check if a command exists in PATH.
    fn command_in_path(cmd: &str) -> bool {
        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).any(|dir| dir.join(cmd).is_file()))
            .unwrap_or(false)
    }
}

/// Stop event hook configuration.
///
/// ```toml
/// [[stop_hooks]]
/// commands = ["cargo clippy --all-targets --all-features -- -D warnings", "cargo fmt --check"]
/// condition = { file_exists = "Cargo.toml" }
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct StopHook {
    /// Commands to execute on Stop event (executed in parallel)
    pub commands: Vec<String>,

    /// Optional condition for execution
    #[serde(default)]
    pub condition: Option<HookCondition>,
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

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use super::*;
    use std::path::Path;

    // === HookCondition tests ===

    #[test]
    fn test_hook_condition_file_exists_satisfied() {
        // Cargo.toml exists in the project root
        let condition = HookCondition {
            file_exists: Some("Cargo.toml".to_string()),
            command_exists: None,
        };
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_file_exists_not_satisfied() {
        let condition = HookCondition {
            file_exists: Some("nonexistent-file-xyz.toml".to_string()),
            command_exists: None,
        };
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(!condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_no_conditions_always_satisfied() {
        let condition = HookCondition {
            file_exists: None,
            command_exists: None,
        };
        let cwd = Path::new("/tmp");
        assert!(condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_invalid_path() {
        let condition = HookCondition {
            file_exists: Some("".to_string()),
            command_exists: None,
        };
        let cwd = Path::new("/nonexistent-path-xyz");
        // Empty string joined with nonexistent path → not satisfied
        assert!(!condition.is_satisfied(cwd));
    }

    // === command_exists tests ===

    #[test]
    fn test_hook_condition_command_exists_satisfied() {
        // "sh" should exist on any Unix system
        let condition = HookCondition {
            file_exists: None,
            command_exists: Some("sh".to_string()),
        };
        let cwd = Path::new("/tmp");
        assert!(condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_command_exists_not_satisfied() {
        let condition = HookCondition {
            file_exists: None,
            command_exists: Some("nonexistent-command-xyz-abc-999".to_string()),
        };
        let cwd = Path::new("/tmp");
        assert!(!condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_both_file_and_command_satisfied() {
        // Both conditions must be true (AND logic)
        let condition = HookCondition {
            file_exists: Some("Cargo.toml".to_string()),
            command_exists: Some("sh".to_string()),
        };
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_file_satisfied_command_not() {
        // file_exists OK, command_exists NG → false
        let condition = HookCondition {
            file_exists: Some("Cargo.toml".to_string()),
            command_exists: Some("nonexistent-command-xyz-abc-999".to_string()),
        };
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(!condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_command_satisfied_file_not() {
        // command_exists OK, file_exists NG → false
        let condition = HookCondition {
            file_exists: Some("nonexistent-file-xyz.toml".to_string()),
            command_exists: Some("sh".to_string()),
        };
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(!condition.is_satisfied(cwd));
    }

    // === TOML deserialization tests ===

    #[test]
    fn test_stop_hook_with_condition_deserializes() {
        let toml_str = r#"
            [[stop_hooks]]
            commands = ["cargo clippy --all-targets --all-features -- -D warnings"]
            condition = { file_exists = "Cargo.toml" }
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            stop_hooks: Vec<StopHook>,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        assert_eq!(wrapper.stop_hooks.len(), 1);
        assert_eq!(
            wrapper.stop_hooks[0].commands,
            vec!["cargo clippy --all-targets --all-features -- -D warnings"]
        );
        let condition = wrapper.stop_hooks[0].condition.as_ref().unwrap();
        assert_eq!(condition.file_exists, Some("Cargo.toml".to_string()));
        assert_eq!(condition.command_exists, None);
    }

    #[test]
    fn test_stop_hook_with_commands_array_deserializes() {
        let toml_str = r#"
            [[stop_hooks]]
            commands = ["cargo clippy --all-targets --all-features -- -D warnings", "cargo fmt --check"]
            condition = { file_exists = "Cargo.toml" }
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            stop_hooks: Vec<StopHook>,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        assert_eq!(wrapper.stop_hooks.len(), 1);
        assert_eq!(
            wrapper.stop_hooks[0].commands,
            vec![
                "cargo clippy --all-targets --all-features -- -D warnings",
                "cargo fmt --check"
            ]
        );
        let condition = wrapper.stop_hooks[0].condition.as_ref().unwrap();
        assert_eq!(condition.file_exists, Some("Cargo.toml".to_string()));
    }

    #[test]
    fn test_stop_hook_rejects_missing_commands() {
        let toml_str = r#"
            [[stop_hooks]]
            condition = { file_exists = "Cargo.toml" }
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            stop_hooks: Vec<StopHook>,
        }

        let result: Result<Wrapper, toml::de::Error> = toml::from_str(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn test_stop_hook_with_command_exists_condition_deserializes() {
        let toml_str = r#"
            [[stop_hooks]]
            commands = ["cargo clippy --all-targets --all-features -- -D warnings"]
            condition = { file_exists = "Cargo.toml", command_exists = "cargo" }
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            stop_hooks: Vec<StopHook>,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        let condition = wrapper.stop_hooks[0].condition.as_ref().unwrap();
        assert_eq!(condition.file_exists, Some("Cargo.toml".to_string()));
        assert_eq!(condition.command_exists, Some("cargo".to_string()));
    }

    #[test]
    fn test_stop_hook_with_only_command_exists_deserializes() {
        let toml_str = r#"
            [[stop_hooks]]
            commands = ["cargo clippy --all-targets --all-features -- -D warnings"]
            condition = { command_exists = "cargo" }
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            stop_hooks: Vec<StopHook>,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        let condition = wrapper.stop_hooks[0].condition.as_ref().unwrap();
        assert_eq!(condition.file_exists, None);
        assert_eq!(condition.command_exists, Some("cargo".to_string()));
    }

    #[test]
    fn test_stop_hook_without_condition_deserializes() {
        let toml_str = r#"
            [[stop_hooks]]
            commands = ["notify-send 'Agent stopped'"]
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            stop_hooks: Vec<StopHook>,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        assert_eq!(wrapper.stop_hooks.len(), 1);
        assert_eq!(
            wrapper.stop_hooks[0].commands,
            vec!["notify-send 'Agent stopped'"]
        );
        assert!(wrapper.stop_hooks[0].condition.is_none());
    }

    #[test]
    fn test_multiple_stop_hooks_mixed_conditions() {
        let toml_str = r#"
            [[stop_hooks]]
            commands = ["notify-send 'Done'"]

            [[stop_hooks]]
            commands = ["cargo clippy --all-targets --all-features -- -D warnings", "cargo fmt --check"]
            condition = { file_exists = "Cargo.toml" }

            [[stop_hooks]]
            commands = ["pnpm exec tsc --noEmit"]
            condition = { file_exists = "tsconfig.json" }
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            stop_hooks: Vec<StopHook>,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        assert_eq!(wrapper.stop_hooks.len(), 3);

        // First: no condition
        assert!(wrapper.stop_hooks[0].condition.is_none());
        assert_eq!(wrapper.stop_hooks[0].commands, vec!["notify-send 'Done'"]);

        // Second: Cargo.toml condition, commands array
        let cond1 = wrapper.stop_hooks[1].condition.as_ref().unwrap();
        assert_eq!(cond1.file_exists, Some("Cargo.toml".to_string()));
        assert_eq!(
            wrapper.stop_hooks[1].commands,
            vec![
                "cargo clippy --all-targets --all-features -- -D warnings",
                "cargo fmt --check"
            ]
        );

        // Third: tsconfig.json condition
        let cond2 = wrapper.stop_hooks[2].condition.as_ref().unwrap();
        assert_eq!(cond2.file_exists, Some("tsconfig.json".to_string()));
    }
}
