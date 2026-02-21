//! Configuration service for loading and generating config files.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use super::types::{default_log_path_for_config_dir, ProjectConfig};
use super::validation;
use super::Config;

/// Project-level configuration file name.
const PROJECT_CONFIG_NAME: &str = ".claw-hooks.toml";

/// Keys that are only allowed in global configuration, not in project config.
const GLOBAL_ONLY_KEYS: &[&str] = &["debug", "log_path", "nano_buddy"];

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
        let project_search_dir = std::env::current_dir().ok();
        Self::load_inner(path, project_search_dir.as_deref())
    }

    /// Internal load implementation that accepts an explicit project search directory.
    fn load_inner(path: Option<&Path>, project_search_dir: Option<&Path>) -> Result<Config> {
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

        // Validate global configuration
        config
            .validate()
            .with_context(|| format!("Invalid configuration in {}", path.display()))?;

        // Search for and merge project-level configuration
        let project_path = project_search_dir.and_then(Self::find_project_config_from);
        if let Some(project_path) = project_path {
            let project = Self::load_project_config(&project_path)?;
            config.merge_project(&project);

            // Re-validate after merge
            config.validate().with_context(|| {
                format!(
                    "Invalid configuration after merging project config from {}",
                    project_path.display()
                )
            })?;
        }

        Ok(config)
    }

    /// Search for `.claw-hooks.toml` in the current working directory.
    pub fn find_project_config() -> Option<PathBuf> {
        let cwd = std::env::current_dir().ok()?;
        Self::find_project_config_from(&cwd)
    }

    /// Check if `.claw-hooks.toml` exists in the given directory.
    fn find_project_config_from(dir: &Path) -> Option<PathBuf> {
        let candidate = dir.join(PROJECT_CONFIG_NAME);
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    }

    /// Load and validate a project-level configuration file.
    pub fn load_project_config(path: &Path) -> Result<ProjectConfig> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read project config: {}", path.display()))?;

        // Reject global-only keys in project config
        Self::reject_global_only_keys(&content, path)?;

        let project: ProjectConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse project config: {}", path.display()))?;

        validation::validate_project(&project)
            .with_context(|| format!("Invalid project config in {}", path.display()))?;

        Ok(project)
    }

    /// Reject usage of global-only keys (debug, log_path, nano_buddy) in project config.
    fn reject_global_only_keys(content: &str, path: &Path) -> Result<()> {
        for line in content.lines() {
            let trimmed = line.trim();
            // Skip comments and empty lines
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            for key in GLOBAL_ONLY_KEYS {
                if trimmed.starts_with(key) && trimmed[key.len()..].trim_start().starts_with('=') {
                    bail!(
                        "Project config {} contains '{}' which is only allowed in global config",
                        path.display(),
                        key
                    );
                }
            }
        }
        Ok(())
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
# command = "npm"
# args = ["install", "i", "add"]         # Blocks: npm install, npm i, npm add
# message = "⚠️ Use `pnpm` instead of `npm`"

# [[custom_filters]]
# command = "yarn"
# message = "⚠️ Use `pnpm` instead of `yarn`"

# [[custom_filters]]
# command = "pip3?"                       # Regex: matches pip or pip3
# args = ["install", "uninstall"]
# message = "Use `uv pip` instead"

# Timeout in seconds for hook command execution (default: 60)
# Commands exceeding this timeout will be killed (SIGKILL)
# hook_timeout = 60

# Extension-based hooks (map format)
# Execute external tools when specific file types are modified
# [extension_hooks]
# ".css" = ["biome format --write {file}", "biome lint --write {file}"]
# ".py" = ["ruff format --check {file}", "ruff check --preview --select=I,F,DOC {file}"]
# ".rs" = ["rustfmt {file}"]
# ".ts" = ["biome check {file}"]
# ".tsx" = ["biome check {file}"]

# Stop hooks
# Execute commands when the agent loop ends (notifications, sounds, cleanup)
# All commands in the array are executed in parallel.
# [[stop_hooks]]
# commands = ["afplay /System/Library/Sounds/Glass.aiff"]  # macOS notification sound

# [[stop_hooks]]
# commands = ["notify-send 'Agent completed'"]  # Linux notification

# Conditional stop hooks (project-wide lint on stop)
# Detects project type and runs lint/typecheck.
# On failure, the result is returned to the AI agent so it can fix the issues.
# All commands are executed in parallel; failures are collected and returned.
# condition fields (AND logic): file_exists, command_exists
# [[stop_hooks]]
# commands = ["cargo clippy --all-targets --all-features -- -D warnings", "cargo fmt --check"]
# condition = { file_exists = "Cargo.toml" }

# [[stop_hooks]]
# commands = ["pnpm exec tsc --noEmit"]
# condition = { file_exists = "tsconfig.json" }

# [[stop_hooks]]
# commands = ["ruff format .", "ruff check --preview --fix --select=I,F,DOC --unsafe-fixes"]
# condition = { file_exists = "pyproject.toml", command_exists = "ruff" }

# [[stop_hooks]]
# commands = ["biome check --write ."]
# condition = { file_exists = "package.json" }

# Auto commit & push (requires git-sc: https://github.com/owayo/git-smart-commit)
# [[stop_hooks]]
# commands = ["git-sc --all --yes --quiet"]
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_path_ends_with_config_toml() {
        let path = ConfigService::default_path();
        assert!(path.ends_with("claw-hooks/config.toml"));
    }

    #[test]
    fn test_default_path_contains_dot_config() {
        let path = ConfigService::default_path();
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains(".config"),
            "Path should contain .config: {}",
            path_str
        );
    }

    #[test]
    fn test_generate_at_creates_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("test_config.toml");

        ConfigService::generate_at(&config_path).unwrap();

        assert!(config_path.exists());
        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("rm_block = true"));
        assert!(content.contains("kill_block = true"));
        assert!(content.contains("dd_block = true"));
    }

    #[test]
    fn test_generate_at_creates_parent_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("nested").join("dir").join("config.toml");

        ConfigService::generate_at(&config_path).unwrap();

        assert!(config_path.exists());
    }

    #[test]
    fn test_load_creates_default_when_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("new_config.toml");

        let config = ConfigService::load(Some(&config_path)).unwrap();

        // Should have created the file and loaded defaults
        assert!(config_path.exists());
        assert!(config.rm_block);
        assert!(config.kill_block);
        assert!(config.dd_block);
    }

    #[test]
    fn test_load_parses_existing_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            "rm_block = false\nkill_block = true\ndd_block = false\n",
        )
        .unwrap();

        let config = ConfigService::load(Some(&config_path)).unwrap();

        assert!(!config.rm_block);
        assert!(config.kill_block);
        assert!(!config.dd_block);
    }

    #[test]
    fn test_load_invalid_toml_returns_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("bad_config.toml");

        fs::write(&config_path, "this is not valid toml [[[").unwrap();

        let result = ConfigService::load(Some(&config_path));
        assert!(result.is_err());
    }

    #[test]
    fn test_default_config_content_has_all_sections() {
        let content = ConfigService::default_config_content();

        assert!(content.contains("rm_block"));
        assert!(content.contains("kill_block"));
        assert!(content.contains("dd_block"));
        assert!(content.contains("debug = false"));
        assert!(content.contains("custom_filters"));
        assert!(content.contains("extension_hooks"));
        assert!(content.contains("stop_hooks"));
        assert!(content.contains("hook_timeout"));
    }

    // === Project config tests ===

    #[test]
    fn test_find_project_config_from_with_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join(".claw-hooks.toml");
        fs::write(&config_path, "rm_block = false\n").unwrap();

        let result = ConfigService::find_project_config_from(dir.path());
        assert_eq!(result, Some(config_path));
    }

    #[test]
    fn test_find_project_config_from_without_file() {
        let dir = tempfile::TempDir::new().unwrap();

        let result = ConfigService::find_project_config_from(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_find_project_config_from_does_not_traverse_parent() {
        let dir = tempfile::TempDir::new().unwrap();
        // Place .claw-hooks.toml in parent dir
        fs::write(dir.path().join(".claw-hooks.toml"), "rm_block = false\n").unwrap();

        // Subdir does NOT have .claw-hooks.toml — should NOT find parent's
        let sub = dir.path().join("subdir");
        fs::create_dir_all(&sub).unwrap();

        let result = ConfigService::find_project_config_from(&sub);
        assert!(result.is_none());
    }

    #[test]
    fn test_load_project_config_valid() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join(".claw-hooks.toml");
        fs::write(
            &config_path,
            r#"
rm_block = false
hook_timeout = 30

[[stop_hooks]]
commands = ["echo done"]
"#,
        )
        .unwrap();

        let project = ConfigService::load_project_config(&config_path).unwrap();
        assert_eq!(project.rm_block, Some(false));
        assert_eq!(project.hook_timeout, Some(30));
        assert!(project.stop_hooks.is_some());
    }

    #[test]
    fn test_load_project_config_rejects_debug() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join(".claw-hooks.toml");
        fs::write(&config_path, "debug = true\n").unwrap();

        let result = ConfigService::load_project_config(&config_path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("debug"));
    }

    #[test]
    fn test_load_project_config_rejects_log_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join(".claw-hooks.toml");
        fs::write(&config_path, "log_path = \"/tmp/logs\"\n").unwrap();

        let result = ConfigService::load_project_config(&config_path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("log_path"));
    }

    #[test]
    fn test_load_project_config_rejects_nano_buddy() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join(".claw-hooks.toml");
        fs::write(&config_path, "nano_buddy = true\n").unwrap();

        let result = ConfigService::load_project_config(&config_path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("nano_buddy"));
    }

    #[test]
    fn test_reject_global_only_keys_allows_comments() {
        let content = "# debug = true\nrm_block = false\n";
        let path = PathBuf::from("test.toml");
        assert!(ConfigService::reject_global_only_keys(content, &path).is_ok());
    }

    #[test]
    fn test_reject_global_only_keys_detects_debug() {
        let content = "debug = true\n";
        let path = PathBuf::from("test.toml");
        assert!(ConfigService::reject_global_only_keys(content, &path).is_err());
    }

    #[test]
    fn test_reject_global_only_keys_detects_with_spaces() {
        let content = "  debug  =  true\n";
        let path = PathBuf::from("test.toml");
        assert!(ConfigService::reject_global_only_keys(content, &path).is_err());
    }

    #[test]
    fn test_load_with_project_config_merge() {
        let dir = tempfile::TempDir::new().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
rm_block = true
kill_block = true
dd_block = true

[[stop_hooks]]
commands = ["echo global"]
"#,
        )
        .unwrap();

        // Create project config in a subdirectory
        let project_dir = dir.path().join("project");
        fs::create_dir_all(&project_dir).unwrap();
        let project_path = project_dir.join(".claw-hooks.toml");
        fs::write(
            &project_path,
            r#"
rm_block = false

[[stop_hooks]]
commands = ["echo project"]
"#,
        )
        .unwrap();

        // Use load_inner with explicit project search dir (avoids set_current_dir)
        let config = ConfigService::load_inner(Some(&global_path), Some(&project_dir)).unwrap();

        assert!(!config.rm_block); // overridden by project
        assert!(config.kill_block); // kept from global
        assert_eq!(config.stop_hooks.len(), 2); // merged
    }

    #[test]
    fn test_load_without_project_config_unchanged() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            "rm_block = true\nkill_block = true\ndd_block = true\n",
        )
        .unwrap();

        // No .claw-hooks.toml in temp dir — use load_inner with explicit search dir
        let config = ConfigService::load_inner(Some(&config_path), Some(dir.path())).unwrap();

        assert!(config.rm_block);
        assert!(config.kill_block);
    }
}
