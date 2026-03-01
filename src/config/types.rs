//! Configuration data types.

use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::validation;

/// Default hook command timeout in seconds.
fn default_hook_timeout() -> u64 {
    60
}

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

    /// Timeout in seconds for hook command execution (default: 60)
    #[serde(default = "default_hook_timeout")]
    pub hook_timeout: u64,
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
            hook_timeout: default_hook_timeout(),
        }
    }
}

impl Config {
    /// Validate configuration and return errors if invalid.
    /// Delegates to the comprehensive validation module.
    pub fn validate(&self) -> Result<()> {
        validation::validate(self)
    }

    /// Merge project-level configuration overrides into this config.
    ///
    /// - `Option<T>` が `Some` の場合のみ上書き/マージ
    /// - `None` = 未指定 → グローバルを維持
    /// - `Some(vec![])` = 明示的に空 → グローバルを空で上書き
    /// - `stop_hooks` のみ `extend` でマージ（グローバル + プロジェクト両方を実行）
    pub fn merge_project(&mut self, project: &ProjectConfig) {
        if let Some(v) = project.rm_block {
            self.rm_block = v;
        }
        if let Some(ref v) = project.rm_block_message {
            self.rm_block_message = Some(v.clone());
        }
        if let Some(v) = project.kill_block {
            self.kill_block = v;
        }
        if let Some(ref v) = project.kill_block_message {
            self.kill_block_message = Some(v.clone());
        }
        if let Some(v) = project.dd_block {
            self.dd_block = v;
        }
        if let Some(ref v) = project.dd_block_message {
            self.dd_block_message = Some(v.clone());
        }
        if let Some(v) = project.hook_timeout {
            self.hook_timeout = v;
        }
        if let Some(ref v) = project.custom_filters {
            self.custom_filters = v.clone();
        }
        if let Some(ref v) = project.extension_hooks {
            self.extension_hooks = v.clone();
        }
        if let Some(ref v) = project.stop_hooks {
            self.stop_hooks.extend(v.clone());
        }
    }
}

/// Project-level configuration overrides.
///
/// All fields are `Option<T>` — `None` means "not specified" (keep global default).
/// Placed at `.claw-hooks.toml` in the project root.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProjectConfig {
    /// Override rm blocking
    pub rm_block: Option<bool>,
    /// Override rm block message
    pub rm_block_message: Option<String>,
    /// Override kill blocking
    pub kill_block: Option<bool>,
    /// Override kill block message
    pub kill_block_message: Option<String>,
    /// Override dd blocking
    pub dd_block: Option<bool>,
    /// Override dd block message
    pub dd_block_message: Option<String>,
    /// Override hook timeout
    pub hook_timeout: Option<u64>,
    /// Override custom filters (replaces global)
    pub custom_filters: Option<Vec<CustomFilter>>,
    /// Override extension hooks (replaces global)
    pub extension_hooks: Option<BTreeMap<String, Vec<String>>>,
    /// Additional stop hooks (merged with global)
    pub stop_hooks: Option<Vec<StopHook>>,
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
        if cmd.is_empty() {
            return false;
        }

        let command_path = Path::new(cmd);
        // Explicit paths ("./tool", "/usr/bin/tool", "dir\\tool.exe") are checked directly.
        if command_path.components().count() > 1 || command_path.is_absolute() {
            return command_path.is_file();
        }

        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };

        #[cfg(windows)]
        {
            // Windows resolves commands using PATHEXT when extension is omitted.
            let has_extension = command_path.extension().is_some();
            let pathext = std::env::var_os("PATHEXT")
                .map(|v| {
                    v.to_string_lossy()
                        .split(';')
                        .map(|ext| ext.trim().to_ascii_lowercase())
                        .filter(|ext| !ext.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| {
                    vec![
                        ".com".to_string(),
                        ".exe".to_string(),
                        ".bat".to_string(),
                        ".cmd".to_string(),
                    ]
                });

            for dir in std::env::split_paths(&path) {
                let base = dir.join(cmd);
                if base.is_file() {
                    return true;
                }
                if !has_extension {
                    for ext in &pathext {
                        if dir.join(format!("{}{}", cmd, ext)).is_file() {
                            return true;
                        }
                    }
                }
            }
            false
        }

        #[cfg(not(windows))]
        {
            std::env::split_paths(&path).any(|dir| dir.join(cmd).is_file())
        }
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

    // === hook_timeout tests ===

    #[test]
    fn test_hook_timeout_default_value() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.hook_timeout, 60);
    }

    #[test]
    fn test_hook_timeout_custom_value() {
        let config: Config = toml::from_str("hook_timeout = 120").unwrap();
        assert_eq!(config.hook_timeout, 120);
    }

    #[test]
    fn test_hook_timeout_zero() {
        // hook_timeout = 0 is technically valid (immediate timeout)
        let config: Config = toml::from_str("hook_timeout = 0").unwrap();
        assert_eq!(config.hook_timeout, 0);
    }

    // === ProjectConfig deserialization tests ===

    #[test]
    fn test_project_config_deserialize_empty() {
        let pc: ProjectConfig = toml::from_str("").unwrap();
        assert!(pc.rm_block.is_none());
        assert!(pc.kill_block.is_none());
        assert!(pc.dd_block.is_none());
        assert!(pc.hook_timeout.is_none());
        assert!(pc.custom_filters.is_none());
        assert!(pc.extension_hooks.is_none());
        assert!(pc.stop_hooks.is_none());
    }

    #[test]
    fn test_project_config_deserialize_partial() {
        let toml_str = r#"
            rm_block = false
            hook_timeout = 30
        "#;
        let pc: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(pc.rm_block, Some(false));
        assert_eq!(pc.hook_timeout, Some(30));
        assert!(pc.kill_block.is_none());
        assert!(pc.custom_filters.is_none());
    }

    #[test]
    fn test_project_config_deserialize_with_stop_hooks() {
        let toml_str = r#"
            [[stop_hooks]]
            commands = ["pnpm exec tsc --noEmit"]
            condition = { file_exists = "tsconfig.json" }
        "#;
        let pc: ProjectConfig = toml::from_str(toml_str).unwrap();
        let hooks = pc.stop_hooks.unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].commands, vec!["pnpm exec tsc --noEmit"]);
    }

    // === merge_project tests ===

    #[test]
    fn test_merge_project_none_keeps_global() {
        let mut config = Config {
            rm_block: true,
            hook_timeout: 120,
            ..Config::default()
        };

        let project = ProjectConfig::default(); // all None
        config.merge_project(&project);

        assert!(config.rm_block);
        assert_eq!(config.hook_timeout, 120);
    }

    #[test]
    fn test_merge_project_overrides_scalar() {
        let mut config = Config::default();
        assert!(config.rm_block); // default true

        let project = ProjectConfig {
            rm_block: Some(false),
            hook_timeout: Some(30),
            ..Default::default()
        };
        config.merge_project(&project);

        assert!(!config.rm_block);
        assert_eq!(config.hook_timeout, 30);
    }

    #[test]
    fn test_merge_project_overrides_custom_filters() {
        let mut config = Config::default();
        config.custom_filters.push(CustomFilter {
            command: "npm".to_string(),
            args: vec![],
            message: "global".to_string(),
        });

        let project = ProjectConfig {
            custom_filters: Some(vec![CustomFilter {
                command: "yarn".to_string(),
                args: vec![],
                message: "project".to_string(),
            }]),
            ..Default::default()
        };
        config.merge_project(&project);

        // custom_filters は上書き（グローバルが消える）
        assert_eq!(config.custom_filters.len(), 1);
        assert_eq!(config.custom_filters[0].command, "yarn");
    }

    #[test]
    fn test_merge_project_empty_vec_clears_custom_filters() {
        let mut config = Config::default();
        config.custom_filters.push(CustomFilter {
            command: "npm".to_string(),
            args: vec![],
            message: "msg".to_string(),
        });

        let project = ProjectConfig {
            custom_filters: Some(vec![]),
            ..Default::default()
        };
        config.merge_project(&project);

        // Some(vec![]) = 明示的に空で上書き
        assert!(config.custom_filters.is_empty());
    }

    #[test]
    fn test_merge_project_stop_hooks_extend() {
        let mut config = Config::default();
        config.stop_hooks.push(StopHook {
            commands: vec!["global-cmd".to_string()],
            condition: None,
        });

        let project = ProjectConfig {
            stop_hooks: Some(vec![StopHook {
                commands: vec!["project-cmd".to_string()],
                condition: None,
            }]),
            ..Default::default()
        };
        config.merge_project(&project);

        // stop_hooks はマージ（両方残る）
        assert_eq!(config.stop_hooks.len(), 2);
        assert_eq!(config.stop_hooks[0].commands, vec!["global-cmd"]);
        assert_eq!(config.stop_hooks[1].commands, vec!["project-cmd"]);
    }

    #[test]
    fn test_merge_project_overrides_extension_hooks() {
        let mut config = Config::default();
        config
            .extension_hooks
            .insert(".rs".to_string(), vec!["rustfmt {file}".to_string()]);

        let project = ProjectConfig {
            extension_hooks: Some({
                let mut m = BTreeMap::new();
                m.insert(".ts".to_string(), vec!["biome check {file}".to_string()]);
                m
            }),
            ..Default::default()
        };
        config.merge_project(&project);

        // extension_hooks は上書き
        assert!(!config.extension_hooks.contains_key(".rs"));
        assert!(config.extension_hooks.contains_key(".ts"));
    }

    #[test]
    fn test_merge_project_overrides_block_messages() {
        let mut config = Config::default();
        assert!(config.rm_block_message.is_none());

        let project = ProjectConfig {
            rm_block_message: Some("Project rm message".to_string()),
            kill_block_message: Some("Project kill message".to_string()),
            ..Default::default()
        };
        config.merge_project(&project);

        assert_eq!(
            config.rm_block_message,
            Some("Project rm message".to_string())
        );
        assert_eq!(
            config.kill_block_message,
            Some("Project kill message".to_string())
        );
    }
}
