//! 設定バリデーション。

use anyhow::{Result, bail};
use regex::Regex;
use std::collections::BTreeMap;

use super::types::ProjectConfig;
use super::{Config, CustomFilter, StopHook};

/// フックコマンドの最大タイムアウト秒数。
/// フックは短時間で終わる前提のため、1日を超える値は設定ミスとして扱う。
pub(crate) const MAX_HOOK_TIMEOUT_SECS: u64 = 86_400;

/// 設定を検証する。
pub fn validate(config: &Config) -> Result<()> {
    // ログパスの検証（NUL文字を含まないこと）
    if !config.log_path.as_os_str().is_empty() && config.log_path.to_string_lossy().contains('\0') {
        bail!("Invalid log_path: contains null character");
    }

    validate_hook_timeout(config.hook_timeout, "hook_timeout")?;
    validate_custom_filters(&config.custom_filters)?;
    validate_extension_hooks(&config.extension_hooks)?;
    validate_stop_hooks(&config.stop_hooks)?;

    Ok(())
}

/// フックコマンドのタイムアウト値を検証する。
fn validate_hook_timeout(timeout_secs: u64, field: &str) -> Result<()> {
    if timeout_secs > MAX_HOOK_TIMEOUT_SECS {
        bail!(
            "{} must be <= {} seconds, got {}",
            field,
            MAX_HOOK_TIMEOUT_SECS,
            timeout_secs
        );
    }
    Ok(())
}

/// カスタムフィルター定義を検証する。
pub fn validate_custom_filters(filters: &[CustomFilter]) -> Result<()> {
    for (i, filter) in filters.iter().enumerate() {
        if filter.command.is_empty() {
            bail!("custom_filters[{}]: command cannot be empty", i);
        }

        // 正規表現パターンの検証
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
    Ok(())
}

/// 拡張子フック定義を検証する。
pub fn validate_extension_hooks(hooks: &BTreeMap<String, Vec<String>>) -> Result<()> {
    for (ext, commands) in hooks {
        if !ext.starts_with('.') {
            bail!("extension_hooks: key '{}' must start with '.'", ext);
        }

        if commands.is_empty() {
            bail!("extension_hooks['{}']: commands cannot be empty", ext);
        }

        // セキュリティ: すべてのコマンドが {file} プレースホルダーを1つだけ含むことを保証
        for (j, cmd) in commands.iter().enumerate() {
            if cmd.is_empty() {
                bail!("extension_hooks['{}']: command[{}] cannot be empty", ext, j);
            }
            if let Err(e) = crate::domain::filters::ExtensionHookFilter::parse_command_template(cmd)
            {
                bail!(
                    "extension_hooks['{}']: command[{}] {}",
                    ext,
                    j,
                    e.to_lowercase()
                );
            }
        }
    }
    Ok(())
}

/// Stop フック定義を検証する。
pub fn validate_stop_hooks(hooks: &[StopHook]) -> Result<()> {
    for (i, hook) in hooks.iter().enumerate() {
        if hook.commands.is_empty() {
            bail!("stop_hooks[{}]: commands cannot be empty", i);
        }
        for (j, cmd) in hook.commands.iter().enumerate() {
            if cmd.is_empty() {
                bail!("stop_hooks[{}]: commands[{}] cannot be empty", i, j);
            }
        }

        // ステージ範囲の検証（1-5）
        if let Some(stage) = hook.stage {
            if !(1..=5).contains(&stage) {
                bail!(
                    "stop_hooks[{}]: stage must be between 1 and 5, got {}",
                    i,
                    stage
                );
            }
        }

        // 条件が指定されている場合の検証
        if let Some(ref condition) = hook.condition {
            if let Some(ref file_exists) = condition.file_exists {
                if file_exists.is_empty() {
                    bail!("stop_hooks[{}]: condition.file_exists cannot be empty", i);
                }
            }
            if let Some(ref command_exists) = condition.command_exists {
                if command_exists.is_empty() {
                    bail!(
                        "stop_hooks[{}]: condition.command_exists cannot be empty",
                        i
                    );
                }
            }
        }
    }
    Ok(())
}

/// プロジェクトレベルの設定を検証する。
/// `Some` のフィールドのみ検証（プロジェクト設定で指定されたもの）。
pub fn validate_project(config: &ProjectConfig) -> Result<()> {
    if let Some(timeout_secs) = config.hook_timeout {
        validate_hook_timeout(timeout_secs, "hook_timeout")?;
    }
    if let Some(ref filters) = config.custom_filters {
        validate_custom_filters(filters)?;
    }
    if let Some(ref hooks) = config.extension_hooks {
        validate_extension_hooks(hooks)?;
    }
    if let Some(ref hooks) = config.stop_hooks {
        validate_stop_hooks(hooks)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HookCondition, ProjectConfig};
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
            command: "[".to_string(), // 無効な正規表現
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
    fn test_validate_rejects_extension_hook_multiple_placeholders() {
        let mut config = default_config();
        let mut hooks = BTreeMap::new();
        hooks.insert(".rs".to_string(), vec!["tool {file} {file}".to_string()]);
        config.extension_hooks = hooks;
        assert!(validate(&config).is_err());
    }

    #[test]
    fn test_validate_rejects_extension_hook_placeholder_as_program() {
        let mut config = default_config();
        let mut hooks = BTreeMap::new();
        hooks.insert(".rs".to_string(), vec!["{file} --write".to_string()]);
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
            commands: vec!["".to_string()],
            condition: None,
            stage: None,
            report: None,
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
    fn test_validate_rejects_hook_timeout_too_large() {
        let mut config = default_config();
        config.hook_timeout = MAX_HOOK_TIMEOUT_SECS + 1;
        let err = validate(&config).unwrap_err();
        assert!(
            err.to_string().contains("hook_timeout"),
            "エラーメッセージに hook_timeout が含まれるべき: {}",
            err
        );
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
            commands: vec!["notify-send 'Done'".to_string()],
            condition: None,
            stage: None,
            report: None,
        });
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_rejects_empty_file_exists_condition() {
        let mut config = default_config();
        config.stop_hooks.push(StopHook {
            commands: vec!["cargo clippy --all-targets --all-features -- -D warnings".to_string()],
            condition: Some(HookCondition {
                file_exists: Some("".to_string()),
                command_exists: None,
            }),
            stage: None,
            report: None,
        });
        assert!(validate(&config).is_err());
    }

    #[test]
    fn test_validate_accepts_valid_file_exists_condition() {
        let mut config = default_config();
        config.stop_hooks.push(StopHook {
            commands: vec!["cargo clippy --all-targets --all-features -- -D warnings".to_string()],
            condition: Some(HookCondition {
                file_exists: Some("Cargo.toml".to_string()),
                command_exists: None,
            }),
            stage: None,
            report: None,
        });
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_rejects_empty_command_exists_condition() {
        let mut config = default_config();
        config.stop_hooks.push(StopHook {
            commands: vec!["cargo clippy --all-targets --all-features -- -D warnings".to_string()],
            condition: Some(HookCondition {
                file_exists: None,
                command_exists: Some("".to_string()),
            }),

            stage: None,

            report: None,
        });
        assert!(validate(&config).is_err());
    }

    #[test]
    fn test_validate_accepts_valid_command_exists_condition() {
        let mut config = default_config();
        config.stop_hooks.push(StopHook {
            commands: vec!["cargo clippy --all-targets --all-features -- -D warnings".to_string()],
            condition: Some(HookCondition {
                file_exists: Some("Cargo.toml".to_string()),
                command_exists: Some("cargo".to_string()),
            }),
            stage: None,
            report: None,
        });
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_accepts_stop_hook_without_condition() {
        let mut config = default_config();
        config.stop_hooks.push(StopHook {
            commands: vec!["echo done".to_string()],
            condition: None,
            stage: None,
            report: None,
        });
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_rejects_empty_commands_array() {
        let mut config = default_config();
        config.stop_hooks.push(StopHook {
            commands: vec![],
            condition: None,
            stage: None,
            report: None,
        });
        assert!(validate(&config).is_err());
    }

    #[test]
    fn test_validate_accepts_multiple_commands() {
        let mut config = default_config();
        config.stop_hooks.push(StopHook {
            commands: vec![
                "cargo clippy --all-targets --all-features -- -D warnings".to_string(),
                "cargo fmt --check".to_string(),
            ],
            condition: Some(HookCondition {
                file_exists: Some("Cargo.toml".to_string()),
                command_exists: None,
            }),
            stage: None,
            report: None,
        });
        assert!(validate(&config).is_ok());
    }

    // === Helper function tests ===

    #[test]
    fn test_validate_custom_filters_valid() {
        let filters = vec![CustomFilter {
            command: "npm".to_string(),
            args: vec!["install".to_string()],
            message: "Use pnpm".to_string(),
        }];
        assert!(validate_custom_filters(&filters).is_ok());
    }

    #[test]
    fn test_validate_custom_filters_empty_command() {
        let filters = vec![CustomFilter {
            command: "".to_string(),
            args: vec![],
            message: "msg".to_string(),
        }];
        assert!(validate_custom_filters(&filters).is_err());
    }

    #[test]
    fn test_validate_extension_hooks_valid() {
        let mut hooks = BTreeMap::new();
        hooks.insert(".rs".to_string(), vec!["rustfmt {file}".to_string()]);
        assert!(validate_extension_hooks(&hooks).is_ok());
    }

    #[test]
    fn test_validate_extension_hooks_missing_dot() {
        let mut hooks = BTreeMap::new();
        hooks.insert("rs".to_string(), vec!["rustfmt {file}".to_string()]);
        assert!(validate_extension_hooks(&hooks).is_err());
    }

    #[test]
    fn test_validate_extension_hooks_rejects_multiple_placeholders() {
        let mut hooks = BTreeMap::new();
        hooks.insert(
            ".rs".to_string(),
            vec!["tool --in={file}:{file}".to_string()],
        );
        assert!(validate_extension_hooks(&hooks).is_err());
    }

    #[test]
    fn test_validate_extension_hooks_rejects_placeholder_as_program() {
        let mut hooks = BTreeMap::new();
        hooks.insert(".rs".to_string(), vec!["{file} --flag".to_string()]);
        assert!(validate_extension_hooks(&hooks).is_err());
    }

    #[test]
    fn test_validate_stop_hooks_valid() {
        let hooks = vec![StopHook {
            commands: vec!["echo done".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        assert!(validate_stop_hooks(&hooks).is_ok());
    }

    #[test]
    fn test_validate_stop_hooks_empty_commands() {
        let hooks = vec![StopHook {
            commands: vec![],
            condition: None,
            stage: None,
            report: None,
        }];
        assert!(validate_stop_hooks(&hooks).is_err());
    }

    // === プロジェクト設定バリデーションテスト ===

    #[test]
    fn test_validate_project_empty() {
        let pc = ProjectConfig::default();
        assert!(validate_project(&pc).is_ok());
    }

    #[test]
    fn test_validate_project_valid_custom_filters() {
        let pc = ProjectConfig {
            custom_filters: Some(vec![CustomFilter {
                command: "yarn".to_string(),
                args: vec![],
                message: "Use pnpm".to_string(),
            }]),
            ..Default::default()
        };
        assert!(validate_project(&pc).is_ok());
    }

    #[test]
    fn test_validate_project_invalid_custom_filters() {
        let pc = ProjectConfig {
            custom_filters: Some(vec![CustomFilter {
                command: "[".to_string(), // 無効な正規表現
                args: vec![],
                message: "msg".to_string(),
            }]),
            ..Default::default()
        };
        assert!(validate_project(&pc).is_err());
    }

    #[test]
    fn test_validate_project_rejects_hook_timeout_too_large() {
        let pc = ProjectConfig {
            hook_timeout: Some(MAX_HOOK_TIMEOUT_SECS + 1),
            ..Default::default()
        };
        assert!(validate_project(&pc).is_err());
    }

    #[test]
    fn test_validate_project_valid_extension_hooks() {
        let pc = ProjectConfig {
            extension_hooks: Some({
                let mut m = BTreeMap::new();
                m.insert(".ts".to_string(), vec!["biome check {file}".to_string()]);
                m
            }),
            ..Default::default()
        };
        assert!(validate_project(&pc).is_ok());
    }

    #[test]
    fn test_validate_project_invalid_extension_hooks() {
        let pc = ProjectConfig {
            extension_hooks: Some({
                let mut m = BTreeMap::new();
                m.insert("ts".to_string(), vec!["biome check {file}".to_string()]);
                m
            }),
            ..Default::default()
        };
        assert!(validate_project(&pc).is_err());
    }

    #[test]
    fn test_validate_project_valid_stop_hooks() {
        let pc = ProjectConfig {
            stop_hooks: Some(vec![StopHook {
                commands: vec!["pnpm exec tsc --noEmit".to_string()],
                condition: Some(HookCondition {
                    file_exists: Some("tsconfig.json".to_string()),
                    command_exists: None,
                }),
                stage: None,
                report: None,
            }]),
            ..Default::default()
        };
        assert!(validate_project(&pc).is_ok());
    }

    #[test]
    fn test_validate_project_invalid_stop_hooks() {
        let pc = ProjectConfig {
            stop_hooks: Some(vec![StopHook {
                commands: vec!["".to_string()],
                condition: None,
                stage: None,
                report: None,
            }]),
            ..Default::default()
        };
        assert!(validate_project(&pc).is_err());
    }

    #[test]
    fn test_validate_project_skips_none_fields() {
        // stop_hooksのみ設定済み、custom_filtersとextension_hooksはNone
        let pc = ProjectConfig {
            rm_block: Some(false),
            stop_hooks: Some(vec![StopHook {
                commands: vec!["echo done".to_string()],
                condition: None,
                stage: None,
                report: None,
            }]),
            ..Default::default()
        };
        assert!(validate_project(&pc).is_ok());
    }

    // === ステージバリデーションテスト ===

    #[test]
    fn test_validate_stop_hooks_stage_valid_range() {
        for stage in 1..=5 {
            let hooks = vec![StopHook {
                commands: vec!["echo test".to_string()],
                condition: None,
                stage: Some(stage),
                report: None,
            }];
            assert!(
                validate_stop_hooks(&hooks).is_ok(),
                "Stage {} should be valid",
                stage
            );
        }
    }

    #[test]
    fn test_validate_stop_hooks_stage_zero_rejected() {
        let hooks = vec![StopHook {
            commands: vec!["echo test".to_string()],
            condition: None,
            stage: Some(0),
            report: None,
        }];
        let err = validate_stop_hooks(&hooks).unwrap_err();
        assert!(
            err.to_string().contains("stage must be between 1 and 5"),
            "Error message should mention stage range: {}",
            err
        );
    }

    #[test]
    fn test_validate_stop_hooks_stage_six_rejected() {
        let hooks = vec![StopHook {
            commands: vec!["echo test".to_string()],
            condition: None,
            stage: Some(6),
            report: None,
        }];
        assert!(validate_stop_hooks(&hooks).is_err());
    }

    #[test]
    fn test_validate_stop_hooks_stage_none_accepted() {
        let hooks = vec![StopHook {
            commands: vec!["echo test".to_string()],
            condition: None,
            stage: None,
            report: None,
        }];
        assert!(validate_stop_hooks(&hooks).is_ok());
    }
}
