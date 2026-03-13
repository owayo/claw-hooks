//! 設定データ型。

use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::validation;
use crate::domain::normalize::DEFAULT_OUTPUT_MAX_LENGTH;

/// フックコマンドのデフォルトタイムアウト（秒）。
fn default_hook_timeout() -> u64 {
    60
}

/// 出力最大長のデフォルト値。
fn default_output_max_length() -> usize {
    DEFAULT_OUTPUT_MAX_LENGTH
}

/// メイン設定構造体。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// rm/rmdir コマンドのブロックを有効化
    pub rm_block: bool,

    /// rm ブロック時のカスタムメッセージ（任意）
    pub rm_block_message: Option<String>,

    /// kill/pkill/killall コマンドのブロックを有効化
    pub kill_block: bool,

    /// kill ブロック時のカスタムメッセージ（任意）
    pub kill_block_message: Option<String>,

    /// dd コマンドのブロックを有効化
    pub dd_block: bool,

    /// dd ブロック時のカスタムメッセージ（任意）
    pub dd_block_message: Option<String>,

    /// ファイルへのデバッグログを有効化
    pub debug: bool,

    /// ログディレクトリのパス
    pub log_path: PathBuf,

    /// カスタムコマンドフィルター
    #[serde(default)]
    pub custom_filters: Vec<CustomFilter>,

    /// 拡張子ベースのフック（マップ形式: ".ext" = ["cmd1", "cmd2"]）
    #[serde(default)]
    pub extension_hooks: BTreeMap<String, Vec<String>>,

    /// Stop イベントフック
    #[serde(default)]
    pub stop_hooks: Vec<StopHook>,

    /// NanoBuddy連携を有効化（隠しオプション）
    #[serde(default)]
    pub nano_buddy: bool,

    /// フックコマンド実行のタイムアウト（秒、デフォルト: 60）
    #[serde(default = "default_hook_timeout")]
    pub hook_timeout: u64,

    /// 出力メッセージの最大長（文字数、デフォルト: 1000）。
    /// AIエージェントのコンテキストウィンドウ溢れを防止する。
    /// 0 の場合は無制限。
    #[serde(default = "default_output_max_length")]
    pub output_max_length: usize,
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
            output_max_length: default_output_max_length(),
        }
    }
}

impl Config {
    /// 設定を検証し、無効な場合はエラーを返す。
    /// 包括的なバリデーションモジュールに委譲。
    pub fn validate(&self) -> Result<()> {
        validation::validate(self)
    }

    /// プロジェクトレベルの設定オーバーライドをこの設定にマージする。
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
        if let Some(v) = project.output_max_length {
            self.output_max_length = v;
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

/// プロジェクトレベルの設定オーバーライド。
///
/// すべてのフィールドは `Option<T>` — `None` は「未指定」（グローバルデフォルトを維持）を意味する。
/// プロジェクトルートの `.claw-hooks.toml` に配置。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProjectConfig {
    /// rm ブロックの上書き
    pub rm_block: Option<bool>,
    /// rm ブロックメッセージの上書き
    pub rm_block_message: Option<String>,
    /// kill ブロックの上書き
    pub kill_block: Option<bool>,
    /// kill ブロックメッセージの上書き
    pub kill_block_message: Option<String>,
    /// dd ブロックの上書き
    pub dd_block: Option<bool>,
    /// dd ブロックメッセージの上書き
    pub dd_block_message: Option<String>,
    /// フックタイムアウトの上書き
    pub hook_timeout: Option<u64>,
    /// 出力最大長の上書き
    pub output_max_length: Option<usize>,
    /// カスタムフィルターの上書き（グローバルを置換）
    pub custom_filters: Option<Vec<CustomFilter>>,
    /// 拡張子フックの上書き（グローバルを置換）
    pub extension_hooks: Option<BTreeMap<String, Vec<String>>>,
    /// 追加の Stop フック（グローバルとマージ）
    pub stop_hooks: Option<Vec<StopHook>>,
}

/// カスタムコマンドフィルター設定。
///
/// 2つのモードをサポート:
/// 1. 正規表現モード: `command` フィールドのみ設定（正規表現パターン）
/// 2. 引数モード: `command` と `args` 両方を設定（コマンド完全一致 + 引数マッチング）
///
/// # 例
///
/// 正規表現モード:
/// ```toml
/// [[custom_filters]]
/// command = "npm (install|i|add)"
/// message = "Use pnpm instead"
/// ```
///
/// 引数モード:
/// ```toml
/// [[custom_filters]]
/// command = "npm"
/// args = ["install", "i", "add"]
/// message = "Use pnpm instead"
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct CustomFilter {
    /// コマンド名（`args` 指定時は完全一致）または正規表現パターン
    pub command: String,

    /// マッチさせる引数のリスト（任意、いずれか一致でフィルター発動）
    /// 指定時は `command` は正規表現ではなく完全一致として扱われる
    #[serde(default)]
    pub args: Vec<String>,

    /// コマンドがブロックされた際に表示するメッセージ
    pub message: String,
}

/// Stop フックの実行条件。
/// 指定されたすべてのフィールドは AND で評価（すべて満たす必要がある）。
#[derive(Debug, Clone, Deserialize)]
pub struct HookCondition {
    /// このファイルが存在する場合のみフックを実行（cwd からの相対パス）
    #[serde(default)]
    pub file_exists: Option<String>,

    /// このコマンドが PATH に存在する場合のみフックを実行
    #[serde(default)]
    pub command_exists: Option<String>,
}

impl HookCondition {
    /// 作業ディレクトリに対して条件を評価する。
    /// 指定されたすべての条件が満たされる場合に true を返す（AND ロジック）。
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

    /// コマンドが PATH に存在するか確認する。
    fn command_in_path(cmd: &str) -> bool {
        if cmd.is_empty() {
            return false;
        }

        let command_path = Path::new(cmd);
        // 明示的なパス（"./tool", "/usr/bin/tool", "dir\\tool.exe"）は直接チェック。
        if command_path.components().count() > 1 || command_path.is_absolute() {
            return command_path.is_file();
        }

        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };

        #[cfg(windows)]
        {
            // Windows は拡張子省略時に PATHEXT を使用してコマンドを解決する。
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

/// Stop イベントフック設定。
///
/// ```toml
/// [[stop_hooks]]
/// commands = ["cargo clippy --all-targets --all-features -- -D warnings", "cargo fmt --check"]
/// condition = { file_exists = "Cargo.toml" }
/// stage = 3
/// report = true
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct StopHook {
    /// Stop イベント時に実行するコマンド（並列実行）
    pub commands: Vec<String>,

    /// 実行条件（任意）
    #[serde(default)]
    pub condition: Option<HookCondition>,

    /// 実行ステージ（1-5、小さい値が先に実行、デフォルト: 5）
    /// ステージ値が小さいフックが先に実行される。
    /// 同じステージのフックは並列実行される。
    #[serde(default)]
    pub stage: Option<u8>,

    /// 結果をAIエージェントに報告するかどうか。
    /// 未指定の場合: `condition` が設定されていれば true、そうでなければ false。
    #[serde(default)]
    pub report: Option<bool>,
}

impl StopHook {
    /// 有効なステージ値を取得（未指定時はデフォルト5）。
    pub fn stage_value(&self) -> u8 {
        self.stage.unwrap_or(5)
    }

    /// このフックの結果をAIエージェントに報告すべきかを判定する。
    /// 明示的な `report` 値が優先され、未指定時は `condition` の有無に基づくデフォルト。
    pub fn should_report(&self) -> bool {
        self.report.unwrap_or(self.condition.is_some())
    }
}

/// デフォルトのログパスを取得（設定ディレクトリからの相対）。
/// プレースホルダーを返す。実際のパスは ConfigService が設定ファイルの場所に基づいて設定する。
pub fn default_log_path() -> PathBuf {
    default_log_path_for_config_dir(None)
}

/// 設定ディレクトリに基づくログパスを取得。
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

    // === HookCondition テスト ===

    #[test]
    fn test_hook_condition_file_exists_satisfied() {
        // プロジェクトルートに Cargo.toml が存在する
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
        // 空文字列と存在しないパスの結合 → 条件不成立
        assert!(!condition.is_satisfied(cwd));
    }

    // === command_exists テスト ===

    #[test]
    fn test_hook_condition_command_exists_satisfied() {
        // "sh" はすべての Unix システムに存在するはず
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
        // 両方の条件が true である必要がある（AND ロジック）
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

    // === TOML デシリアライゼーションテスト ===

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

        // 1番目: 条件なし
        assert!(wrapper.stop_hooks[0].condition.is_none());
        assert_eq!(wrapper.stop_hooks[0].commands, vec!["notify-send 'Done'"]);

        // 2番目: Cargo.toml 条件、コマンド配列
        let cond1 = wrapper.stop_hooks[1].condition.as_ref().unwrap();
        assert_eq!(cond1.file_exists, Some("Cargo.toml".to_string()));
        assert_eq!(
            wrapper.stop_hooks[1].commands,
            vec![
                "cargo clippy --all-targets --all-features -- -D warnings",
                "cargo fmt --check"
            ]
        );

        // 3番目: tsconfig.json 条件
        let cond2 = wrapper.stop_hooks[2].condition.as_ref().unwrap();
        assert_eq!(cond2.file_exists, Some("tsconfig.json".to_string()));
    }

    // === hook_timeout テスト ===

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
        // hook_timeout = 0 は技術的に有効（即時タイムアウト）
        let config: Config = toml::from_str("hook_timeout = 0").unwrap();
        assert_eq!(config.hook_timeout, 0);
    }

    // === output_max_length テスト ===

    #[test]
    fn test_output_max_length_default_value() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.output_max_length, 1000);
    }

    #[test]
    fn test_output_max_length_custom_value() {
        let config: Config = toml::from_str("output_max_length = 2000").unwrap();
        assert_eq!(config.output_max_length, 2000);
    }

    #[test]
    fn test_output_max_length_zero_means_unlimited() {
        let config: Config = toml::from_str("output_max_length = 0").unwrap();
        assert_eq!(config.output_max_length, 0);
    }

    // === ProjectConfig デシリアライゼーションテスト ===

    #[test]
    fn test_project_config_deserialize_empty() {
        let pc: ProjectConfig = toml::from_str("").unwrap();
        assert!(pc.rm_block.is_none());
        assert!(pc.kill_block.is_none());
        assert!(pc.dd_block.is_none());
        assert!(pc.hook_timeout.is_none());
        assert!(pc.output_max_length.is_none());
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

    // === merge_project テスト ===

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
            stage: None,
            report: None,
        });

        let project = ProjectConfig {
            stop_hooks: Some(vec![StopHook {
                commands: vec!["project-cmd".to_string()],
                condition: None,
                stage: None,
                report: None,
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
    fn test_merge_project_overrides_output_max_length() {
        let mut config = Config::default();
        assert_eq!(config.output_max_length, 1000);

        let project = ProjectConfig {
            output_max_length: Some(500),
            ..Default::default()
        };
        config.merge_project(&project);

        assert_eq!(config.output_max_length, 500);
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

    // === StopHook stage/report テスト ===

    #[test]
    fn test_stop_hook_stage_default_value() {
        let hook = StopHook {
            commands: vec!["echo test".to_string()],
            condition: None,
            stage: None,
            report: None,
        };
        assert_eq!(hook.stage_value(), 5);
    }

    #[test]
    fn test_stop_hook_stage_explicit_value() {
        let hook = StopHook {
            commands: vec!["echo test".to_string()],
            condition: None,
            stage: Some(1),
            report: None,
        };
        assert_eq!(hook.stage_value(), 1);
    }

    #[test]
    fn test_stop_hook_should_report_defaults_true_with_condition() {
        let hook = StopHook {
            commands: vec!["cargo clippy".to_string()],
            condition: Some(HookCondition {
                file_exists: Some("Cargo.toml".to_string()),
                command_exists: None,
            }),
            stage: None,
            report: None,
        };
        assert!(hook.should_report());
    }

    #[test]
    fn test_stop_hook_should_report_defaults_false_without_condition() {
        let hook = StopHook {
            commands: vec!["echo done".to_string()],
            condition: None,
            stage: None,
            report: None,
        };
        assert!(!hook.should_report());
    }

    #[test]
    fn test_stop_hook_should_report_explicit_true_overrides() {
        let hook = StopHook {
            commands: vec!["echo done".to_string()],
            condition: None,
            stage: None,
            report: Some(true),
        };
        assert!(hook.should_report());
    }

    #[test]
    fn test_stop_hook_should_report_explicit_false_overrides() {
        let hook = StopHook {
            commands: vec!["cargo clippy".to_string()],
            condition: Some(HookCondition {
                file_exists: Some("Cargo.toml".to_string()),
                command_exists: None,
            }),
            stage: None,
            report: Some(false),
        };
        assert!(!hook.should_report());
    }

    #[test]
    fn test_stop_hook_with_stage_deserializes() {
        let toml_str = r#"
            [[stop_hooks]]
            commands = ["cargo clippy"]
            stage = 1
            report = true
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            stop_hooks: Vec<StopHook>,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        assert_eq!(wrapper.stop_hooks[0].stage, Some(1));
        assert_eq!(wrapper.stop_hooks[0].report, Some(true));
    }

    #[test]
    fn test_stop_hook_without_stage_defaults_none() {
        let toml_str = r#"
            [[stop_hooks]]
            commands = ["echo done"]
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            stop_hooks: Vec<StopHook>,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        assert_eq!(wrapper.stop_hooks[0].stage, None);
        assert_eq!(wrapper.stop_hooks[0].stage_value(), 5);
        assert_eq!(wrapper.stop_hooks[0].report, None);
        assert!(!wrapper.stop_hooks[0].should_report());
    }

    #[test]
    fn test_stop_hook_with_condition_and_no_report_defaults_report_true() {
        let toml_str = r#"
            [[stop_hooks]]
            commands = ["cargo clippy"]
            condition = { file_exists = "Cargo.toml" }
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            stop_hooks: Vec<StopHook>,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        assert!(wrapper.stop_hooks[0].should_report());
    }
}
