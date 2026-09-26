//! 設定バリデーション。

use anyhow::{Result, bail};
use regex::Regex;
use std::collections::BTreeMap;

use super::types::ProjectConfig;
use super::{CommandHook, Config, CustomFilter, StopHook};
use crate::domain::filters::ExtensionHookFilter;

/// フックコマンドの最大タイムアウト秒数。
/// フックは短時間で終わる前提のため、1日を超える値は設定ミスとして扱う。
pub(crate) const MAX_HOOK_TIMEOUT_SECS: u64 = 86_400;

/// `claw-hooks check` 用の検証エントリポイント。
///
/// 値の検証に加えて、読み込み時に記録された警告（無視したプロジェクト設定・未知キー）を
/// stderr に出力する。検証に失敗しても警告は見えるよう、先に警告を出す。
///
/// フック実行経路（`Config::validate`）は `validate_values` を直接呼んで沈黙させる。
/// Claude / Windsurf ではブロック時の stderr 本文がそのままエージェントへ渡す理由に
/// なるため、そこへ設定警告が混ざると判定メッセージが濁るからである。
pub fn validate(config: &Config) -> Result<()> {
    // `check` はロガー初期化後に走るため、ログにも同じ警告を残せる
    // （設定読み込み時点ではロガーがまだ無い — ログの出力先が設定そのものだから）。
    config.log_warnings();
    for warning in &config.warnings {
        eprintln!("warning: {}", warning);
    }
    validate_values(config)
}

/// 設定の値そのものを検証する（出力を伴わない本体）。
pub(crate) fn validate_values(config: &Config) -> Result<()> {
    // ログパスの検証（NUL文字を含まないこと）
    if !config.log_path.as_os_str().is_empty() && config.log_path.to_string_lossy().contains('\0') {
        bail!("Invalid log_path: contains null character");
    }

    validate_hook_timeout(config.hook_timeout, "hook_timeout")?;
    validate_custom_filters(&config.custom_filters)?;
    validate_extension_hooks(&config.extension_hooks)?;
    validate_stop_hooks(&config.stop_hooks)?;
    validate_command_hooks(&config.command_hooks)?;

    Ok(())
}

/// フックコマンドのタイムアウト値を検証する。
fn validate_hook_timeout(timeout_secs: u64, field: &str) -> Result<()> {
    // 0 を「無制限」のつもりで書く利用者が必ず出る（同じ Config の `output_max_length` は
    // 0 = 無制限のため）。実際には全フックが起動直後に kill され出力ごと捨てられ、
    // lint も通知も commit も静かに全滅する。上限だけ見ていると `check` が
    // "Configuration is valid." と答えてしまうので、明示的に弾いて誤解を解く。
    if timeout_secs == 0 {
        bail!(
            "{} must be >= 1 second, got 0 \
             (0 does NOT mean unlimited: every hook command would be killed immediately \
             and its output discarded; use a large value such as {} for a practically unlimited timeout)",
            field,
            MAX_HOOK_TIMEOUT_SECS
        );
    }
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
///
/// キーは `.` で始まる拡張子か、すべてのファイルに当てる `"*"` のどちらか。glob やファイル名の
/// キーは無いので弾く。受け付けてしまうと、`"*.rs"` を glob のつもりで書いた設定が
/// どのファイルにも当たらないまま `check` が "Configuration is valid." と答えてしまう。
pub fn validate_extension_hooks(hooks: &BTreeMap<String, Vec<String>>) -> Result<()> {
    for (ext, commands) in hooks {
        if ext != ExtensionHookFilter::CATCH_ALL_KEY && !ext.starts_with('.') {
            bail!(
                "extension_hooks: key '{}' must be an extension starting with '.' (e.g. \".rs\") \
                 or \"*\" for every file (globs and file names are not supported)",
                ext
            );
        }

        if commands.is_empty() {
            bail!("extension_hooks['{}']: commands cannot be empty", ext);
        }

        // セキュリティ: すべてのコマンドが {file} プレースホルダーを1つだけ含むことを保証
        for (j, cmd) in commands.iter().enumerate() {
            if cmd.is_empty() {
                bail!("extension_hooks['{}']: command[{}] cannot be empty", ext, j);
            }
            if let Err(e) = ExtensionHookFilter::parse_command_template(cmd) {
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

/// 拡張子フックの設定のうち、エラーにはしないが知らせておくべき点を警告文にする。
///
/// 同じコマンドを拡張子のキーと `"*"` の両方に書くと、そのファイルでは 2 回動く。
/// 重複を黙って除くと書いた順序と回数が読めなくなるので、書いたとおりに動かしたうえで
/// 知らせる。`"*"` を足したときに拡張子のキーへ残した同じ行を消し忘れると起きる。
///
/// 警告はフック実行時にデバッグログ（ディスク）にも残るため、コマンド本文は入れず、
/// キーと番号で場所を示す（実行ファイルのディレクトリをログに残さない方針）。
pub(crate) fn extension_hook_warnings(hooks: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let Some(catch_all) = hooks.get(ExtensionHookFilter::CATCH_ALL_KEY) else {
        return Vec::new();
    };
    catch_all
        .iter()
        .enumerate()
        .filter_map(|(i, command)| {
            let keys: Vec<String> = hooks
                .iter()
                .filter(|(key, commands)| {
                    key.as_str() != ExtensionHookFilter::CATCH_ALL_KEY && commands.contains(command)
                })
                .map(|(key, _)| format!("\"{}\"", key))
                .collect();
            if keys.is_empty() {
                return None;
            }
            Some(format!(
                "extension_hooks: \"*\" command[{}] is also listed under {}, \
                 so it runs twice on those files (once for each key)",
                i,
                keys.join(", ")
            ))
        })
        .collect()
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
        if let Some(stage) = hook.stage
            && !(1..=5).contains(&stage)
        {
            bail!(
                "stop_hooks[{}]: stage must be between 1 and 5, got {}",
                i,
                stage
            );
        }

        // 条件が指定されている場合の検証
        if let Some(ref condition) = hook.condition {
            if let Some(ref file_exists) = condition.file_exists
                && file_exists.is_empty()
            {
                bail!("stop_hooks[{}]: condition.file_exists cannot be empty", i);
            }
            if let Some(ref file_not_exists) = condition.file_not_exists
                && file_not_exists.is_empty()
            {
                bail!(
                    "stop_hooks[{}]: condition.file_not_exists cannot be empty",
                    i
                );
            }
            if let Some(ref command_exists) = condition.command_exists
                && command_exists.is_empty()
            {
                bail!(
                    "stop_hooks[{}]: condition.command_exists cannot be empty",
                    i
                );
            }
            if let Some(ref command_not_exists) = condition.command_not_exists
                && command_not_exists.is_empty()
            {
                bail!(
                    "stop_hooks[{}]: condition.command_not_exists cannot be empty",
                    i
                );
            }
        }
    }
    Ok(())
}

/// command hooks の定義を検証する。
///
/// `command` は呼び出しのプログラム名と完全一致で照合する名前なので、1 語でなければならない。
/// `gws docs` のように引数まで書くと、どの呼び出しとも一致しないまま `check` が
/// "Configuration is valid." と答え、判定器が一度も走らない状態を黙って作ってしまう。
/// 空白は Unicode の空白全般（NBSP・全角空白を含む）を対象にする。
pub fn validate_command_hooks(hooks: &[CommandHook]) -> Result<()> {
    for (i, hook) in hooks.iter().enumerate() {
        let command = hook.command.trim();
        if command.is_empty() {
            bail!("command_hooks[{}]: command cannot be empty", i);
        }
        if command.contains('\0') {
            bail!(
                "command_hooks[{}]: command cannot contain a null character",
                i
            );
        }
        if command.chars().any(char::is_whitespace) {
            bail!(
                "command_hooks[{}]: command must be a single program name without whitespace, got {:?} \
                 (arguments cannot be matched here; the checker receives the full argv)",
                i,
                command
            );
        }
        // `.exe` のように正規化すると何も残らない名前は、どの呼び出しとも一致しない。
        if hook.command_key().is_empty() {
            bail!(
                "command_hooks[{}]: command {:?} does not name a program",
                i,
                command
            );
        }

        if hook.run.trim().is_empty() {
            bail!("command_hooks[{}]: run cannot be empty", i);
        }
        // 判定器は他のフックと同じく argv に分割して起動する（Windows では `cmd /c` 経由）。
        // 先頭語が起動するプログラムになるため、クォートだけの `''` のように
        // quote removal 後に空になる先頭語も「プログラムが無い」として弾く。
        let run_tokens = crate::domain::parse_shell_tokens(&hook.run);
        if run_tokens.first().is_none_or(|program| program.is_empty()) {
            bail!(
                "command_hooks[{}]: run must start with the program to execute",
                i
            );
        }

        if let Some(timeout_secs) = hook.timeout {
            validate_hook_timeout(timeout_secs, &format!("command_hooks[{}].timeout", i))?;
        }
    }
    Ok(())
}

/// プロジェクトレベルの設定を検証する。
/// `Some` のフィールドのみ検証（プロジェクト設定で指定されたもの）。
///
/// 検証するのは **実際に適用されるフィールドだけ**。`extension_hooks` / `stop_hooks` /
/// `command_hooks` は信頼境界により一切適用されず（`Config::merge_project` を参照。
/// いずれも任意コマンドを実行するため、未信頼のプロジェクト設定からは受け付けない）、
/// 無視した旨は警告で伝える。
/// 適用しない値を検証してハードエラーにすると、clone したリポジトリに壊れた 2 行を
/// 置くだけで設定読み込み全体が失敗し、そのディレクトリでは `ls` のような無関係な
/// コマンドまでフェイルクローズドで deny になる。正しい記述は黙って無視されるのに
/// 壊れた記述だけが致命的、という非対称も生む。
pub fn validate_project(config: &ProjectConfig) -> Result<()> {
    if let Some(timeout_secs) = config.hook_timeout {
        validate_hook_timeout(timeout_secs, "hook_timeout")?;
    }
    if let Some(ref filters) = config.custom_filters {
        validate_custom_filters(filters)?;
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
            session_scope: Default::default(),
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
    fn test_validate_rejects_hook_timeout_zero() {
        // 0 は「無制限」ではなく「即タイムアウト」。valid 扱いのままだと
        // `check` が "Configuration is valid." と答えた上で全フックが黙って死ぬ。
        let mut config = default_config();
        config.hook_timeout = 0;
        let err = validate_values(&config).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("hook_timeout"),
            "どのフィールドかを示すべき: {}",
            message
        );
        assert!(
            message.contains("unlimited"),
            "0 が無制限ではないことを伝えるべき: {}",
            message
        );
    }

    #[test]
    fn test_validate_accepts_hook_timeout_one_second() {
        // 境界値: 1 秒は有効（下限は 0 の拒否のみ）
        let mut config = default_config();
        config.hook_timeout = 1;
        assert!(validate_values(&config).is_ok());
    }

    #[test]
    fn test_validate_project_rejects_hook_timeout_zero() {
        // プロジェクト設定側も同じ理由で 0 を弾く
        let pc = ProjectConfig {
            hook_timeout: Some(0),
            ..Default::default()
        };
        assert!(validate_project(&pc).is_err());
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
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
            }),
            stage: None,
            report: None,
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
            }),
            stage: None,
            report: None,
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
            }),

            stage: None,

            report: None,
            session_scope: Default::default(),
        });
        assert!(validate(&config).is_err());
    }

    #[test]
    fn test_validate_rejects_empty_file_not_exists_condition() {
        let mut config = default_config();
        config.stop_hooks.push(StopHook {
            commands: vec!["cargo clippy --all-targets --all-features -- -D warnings".to_string()],
            condition: Some(HookCondition {
                file_not_exists: Some("".to_string()),
                ..Default::default()
            }),
            stage: None,
            report: None,
            session_scope: Default::default(),
        });
        assert!(validate(&config).is_err());
    }

    #[test]
    fn test_validate_rejects_empty_command_not_exists_condition() {
        let mut config = default_config();
        config.stop_hooks.push(StopHook {
            commands: vec!["cargo clippy --all-targets --all-features -- -D warnings".to_string()],
            condition: Some(HookCondition {
                command_not_exists: Some("".to_string()),
                ..Default::default()
            }),
            stage: None,
            report: None,
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
            }),
            stage: None,
            report: None,
            session_scope: Default::default(),
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
            session_scope: Default::default(),
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
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
            }),
            stage: None,
            report: None,
            session_scope: Default::default(),
        });
        assert!(validate(&config).is_ok());
    }

    // === ヘルパー関数のテスト ===

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

    /// `[extension_hooks]` を (キー, コマンド) の組から作る。
    fn extension_hooks(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(key, commands)| {
                (
                    key.to_string(),
                    commands.iter().map(|c| c.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn test_validate_extension_hooks_accepts_catch_all_key() {
        let hooks = extension_hooks(&[
            (".rs", &["rustfmt {file}"]),
            ("*", &["noslop hook file {file}"]),
        ]);
        assert!(validate_extension_hooks(&hooks).is_ok());

        // "*" だけの設定も有効
        let hooks = extension_hooks(&[("*", &["noslop hook file {file}"])]);
        assert!(validate_extension_hooks(&hooks).is_ok());
    }

    #[test]
    fn test_validate_extension_hooks_rejects_glob_and_non_extension_keys() {
        // glob・ファイル名のキーは無い。受け付けると、どのファイルにも当たらないまま
        // check が通ってしまう
        for key in ["*.rs", "**", "*.{yml,yaml}", " * ", "rs", "Makefile", ""] {
            let hooks = extension_hooks(&[(key, &["lint {file}"])]);
            let Err(err) = validate_extension_hooks(&hooks) else {
                panic!("キー {key:?} は拒否すべき");
            };
            let err = err.to_string();
            assert!(
                err.contains("\"*\" for every file") && err.contains("\".rs\""),
                "正しい書き方を案内すべき: {err}"
            );
        }
    }

    #[test]
    fn test_validate_extension_hooks_checks_catch_all_commands() {
        // "*" のコマンドにも拡張子のキーと同じ検証を掛ける
        for commands in [
            &[][..],
            &[""][..],
            &["noslop hook file"][..],
            &["tool {file} {file}"][..],
            &["{file} --flag"][..],
        ] {
            let hooks = extension_hooks(&[("*", commands)]);
            assert!(
                validate_extension_hooks(&hooks).is_err(),
                "\"*\" = {commands:?} は拒否すべき"
            );
        }
    }

    #[test]
    fn test_extension_hook_warnings_reports_commands_shared_with_catch_all() {
        let hooks = extension_hooks(&[
            (".go", &["gofmt -w {file}"]),
            (".md", &["noslop hook file {file}"]),
            (".rs", &["rustfmt {file}", "noslop hook file {file}"]),
            ("*", &["noslop hook file {file}", "typos {file}"]),
        ]);

        let warnings = extension_hook_warnings(&hooks);

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let warning = &warnings[0];
        assert!(
            warning.contains("\"*\" command[0]") && warning.contains("\".md\", \".rs\""),
            "重複しているキーと番号を示すべき: {warning}"
        );
        assert!(!warning.contains("\".go\""), "{warning}");
        assert!(
            !warning.contains("noslop"),
            "警告はログにも残るので、コマンド本文を入れない: {warning}"
        );
    }

    #[test]
    fn test_extension_hook_warnings_ignores_non_identical_commands() {
        // 完全に一致するものだけを知らせる（空白や引数が違えば別のコマンドとして扱う）
        let hooks = extension_hooks(&[
            (".md", &["noslop hook file  {file}"]),
            (".rs", &["noslop hook file --max-chars 900 {file}"]),
            ("*", &["noslop hook file {file}"]),
        ]);
        assert!(extension_hook_warnings(&hooks).is_empty());

        // "*" が無ければ重複は起きない
        let hooks = extension_hooks(&[
            (".md", &["noslop hook file {file}"]),
            (".rs", &["noslop hook file {file}"]),
        ]);
        assert!(extension_hook_warnings(&hooks).is_empty());
    }

    #[test]
    fn test_validate_accepts_commands_shared_with_catch_all() {
        // 重複は警告に留め、設定エラーにはしない
        let mut config = default_config();
        config.extension_hooks = extension_hooks(&[
            (".md", &["noslop hook file {file}"]),
            ("*", &["noslop hook file {file}"]),
        ]);
        assert!(validate_values(&config).is_ok());
    }

    #[test]
    fn test_validate_stop_hooks_valid() {
        let hooks = vec![StopHook {
            commands: vec!["echo done".to_string()],
            condition: None,
            stage: None,
            report: None,
            session_scope: Default::default(),
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
            session_scope: Default::default(),
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
    fn test_validate_project_does_not_reject_ignored_extension_hooks() {
        // プロジェクト設定の `extension_hooks` は適用されない（`merge_project` 参照）。
        // 適用しない値の書式エラーで設定読み込み全体を落とすと、clone したリポジトリの
        // 2 行で無関係なコマンドまで deny になるため、ここでは検証しない。
        let pc = ProjectConfig {
            extension_hooks: Some({
                let mut m = BTreeMap::new();
                // `.` 始まりでない不正なキー。
                m.insert("ts".to_string(), vec!["biome check {file}".to_string()]);
                m
            }),
            ..Default::default()
        };
        assert!(validate_project(&pc).is_ok());
    }

    #[test]
    fn test_validate_project_valid_stop_hooks() {
        let pc = ProjectConfig {
            stop_hooks: Some(vec![StopHook {
                commands: vec!["pnpm exec tsc --noEmit".to_string()],
                condition: Some(HookCondition {
                    file_exists: Some("tsconfig.json".to_string()),
                    command_exists: None,
                    file_not_exists: None,
                    command_not_exists: None,
                }),
                stage: None,
                report: None,
                session_scope: Default::default(),
            }]),
            ..Default::default()
        };
        assert!(validate_project(&pc).is_ok());
    }

    #[test]
    fn test_validate_project_does_not_reject_ignored_stop_hooks() {
        // `extension_hooks` と同じ理由で、適用されない `stop_hooks` も検証しない。
        let pc = ProjectConfig {
            stop_hooks: Some(vec![StopHook {
                // 空コマンド = 適用されるなら不正な書式。
                commands: vec!["".to_string()],
                condition: None,
                stage: None,
                report: None,
                session_scope: Default::default(),
            }]),
            ..Default::default()
        };
        assert!(validate_project(&pc).is_ok());
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
                session_scope: Default::default(),
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
                session_scope: Default::default(),
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
            session_scope: Default::default(),
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
            session_scope: Default::default(),
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
            session_scope: Default::default(),
        }];
        assert!(validate_stop_hooks(&hooks).is_ok());
    }

    // === command hooks のバリデーションテスト ===

    fn command_hook(command: &str, run: &str, timeout: Option<u64>) -> CommandHook {
        CommandHook {
            command: command.to_string(),
            run: run.to_string(),
            timeout,
            on_error: Default::default(),
        }
    }

    /// 1 件だけの command hook を持つ設定を検証し、エラー文を返す。
    fn command_hook_error(hook: CommandHook) -> String {
        let mut config = default_config();
        config.command_hooks.push(hook);
        validate_values(&config)
            .expect_err("不正な command hook は検証エラーになるべき")
            .to_string()
    }

    #[test]
    fn test_validate_accepts_valid_command_hooks() {
        let mut config = default_config();
        config.command_hooks = vec![
            command_hook("gws", "checker hook command", None),
            // パス付きの名前も command_key で正規化して照合するので受理する
            command_hook("/usr/local/bin/gws", "checker --flag 'quoted arg'", Some(1)),
            // 前後の空白は trim してから判定する
            command_hook("  git  ", "checker", Some(MAX_HOOK_TIMEOUT_SECS)),
        ];
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn test_validate_rejects_empty_command_hook_command() {
        for command in ["", "   ", "\t\n"] {
            let message = command_hook_error(command_hook(command, "checker", None));
            assert!(
                message.contains("command_hooks[0]: command cannot be empty"),
                "{command:?}: {message}"
            );
        }
    }

    #[test]
    fn test_validate_rejects_command_hook_command_with_whitespace() {
        // 引数まで書いた `gws docs` はどの呼び出しとも一致しないため設定ミスとして弾く。
        // タブ・NBSP・全角空白のような ASCII 以外の空白も同様に扱う。
        for command in [
            "gws docs",
            "gws\tdocs",
            "gws\u{00A0}docs",
            "gws\u{3000}docs",
        ] {
            let message = command_hook_error(command_hook(command, "checker", None));
            assert!(
                message.contains("command_hooks[0]: command must be a single program name"),
                "{command:?}: {message}"
            );
        }
    }

    #[test]
    fn test_validate_rejects_command_hook_command_with_nul() {
        let message = command_hook_error(command_hook("gws\0", "checker", None));
        assert!(
            message.contains("command_hooks[0]: command cannot contain a null character"),
            "{message}"
        );
    }

    #[test]
    fn test_validate_rejects_command_hook_command_without_program_name() {
        // 実行拡張子だけの名前は command_key で空になり、どの呼び出しとも一致しない。
        for command in [".exe", "/usr/bin/.CMD"] {
            let message = command_hook_error(command_hook(command, "checker", None));
            assert!(
                message.contains("command_hooks[0]: command")
                    && message.contains("does not name a program"),
                "{command:?}: {message}"
            );
        }
    }

    #[test]
    fn test_validate_rejects_empty_command_hook_run() {
        for run in ["", "   "] {
            let message = command_hook_error(command_hook("gws", run, None));
            assert!(
                message.contains("command_hooks[0]: run cannot be empty"),
                "{run:?}: {message}"
            );
        }
    }

    #[test]
    fn test_validate_rejects_command_hook_run_without_program() {
        // クォートだけの先頭語は quote removal 後に空になり、起動するプログラムが無い。
        for run in ["''", "\"\" --flag"] {
            let message = command_hook_error(command_hook("gws", run, None));
            assert!(
                message.contains("command_hooks[0]: run must start with the program to execute"),
                "{run:?}: {message}"
            );
        }
    }

    #[test]
    fn test_validate_rejects_command_hook_timeout_out_of_range() {
        // 0 は「無制限」ではなく即タイムアウト。値域（1〜MAX_HOOK_TIMEOUT_SECS）は他のフックと同じ
        // （値そのものは hook_timeout と独立で、判定器の時間はこの timeout だけで決まる）。
        for timeout in [0, MAX_HOOK_TIMEOUT_SECS + 1] {
            let message = command_hook_error(command_hook("gws", "checker", Some(timeout)));
            assert!(
                message.contains("command_hooks[0].timeout"),
                "{timeout}: {message}"
            );
        }
    }

    #[test]
    fn test_validate_command_hook_error_names_the_entry_index() {
        let mut config = default_config();
        config.command_hooks = vec![
            command_hook("gws", "checker", None),
            command_hook("git", "", None),
        ];
        let message = validate_values(&config).unwrap_err().to_string();
        assert!(message.contains("command_hooks[1]"), "{message}");
    }

    #[test]
    fn test_validate_project_does_not_validate_ignored_command_hooks() {
        // `extension_hooks` / `stop_hooks` と同じ理由で、適用されない command_hooks は
        // 検証しない。適用されるなら不正な値（空の command / run、timeout = 0）でも
        // 設定読み込みを落とさない。
        let pc: ProjectConfig =
            toml::from_str("[[command_hooks]]\ncommand = \"\"\nrun = \"\"\ntimeout = 0\n").unwrap();
        assert!(validate_project(&pc).is_ok());
    }
}
