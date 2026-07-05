//! 設定ファイルの読み込みと生成を行う設定サービス。

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

use super::Config;
use super::types::{ProjectConfig, default_log_path_for_config_dir};
use super::validation;

/// プロジェクトレベルの設定ファイル名。
const PROJECT_CONFIG_NAME: &str = ".claw-hooks.toml";

/// グローバル設定でのみ許可され、プロジェクト設定では使用できないキー。
const GLOBAL_ONLY_KEYS: &[&str] = &["debug", "log_path", "nano_buddy"];

/// 設定サービス。
pub struct ConfigService;

impl ConfigService {
    /// デフォルトの設定ファイルパスを取得。
    /// クロスプラットフォームの一貫性のため常に ~/.config/claw-hooks/config.toml を使用。
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("claw-hooks")
            .join("config.toml")
    }

    /// ファイルから設定を読み込む。
    ///
    /// `path` が `None` の場合はデフォルトパスを使用。
    /// ファイルが存在しない場合はデフォルト設定ファイルを作成。
    /// 読み込み後に設定を検証。
    /// ログパスはデフォルトで設定ファイルと同じディレクトリ。
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let project_search_dir = std::env::current_dir().ok();
        Self::load_inner(path, project_search_dir.as_deref())
    }

    /// 明示的なプロジェクト検索ディレクトリを受け取る内部読み込み実装。
    fn load_inner(path: Option<&Path>, project_search_dir: Option<&Path>) -> Result<Config> {
        let path = path.map(PathBuf::from).unwrap_or_else(Self::default_path);
        let config_dir = path.parent();

        if !path.exists() {
            // デフォルト設定ファイルを作成
            Self::generate_at(&path)?;
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let mut config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        // log_path が設定ファイルで明示的に設定されていない場合、設定ファイルのディレクトリを使用
        // log_path が汎用デフォルトと一致するかチェック（ファイルで設定されていないことを意味する）
        let general_default = default_log_path_for_config_dir(None);
        if config.log_path == general_default {
            config.log_path = default_log_path_for_config_dir(config_dir);
        }

        // グローバル設定の検証
        config
            .validate()
            .with_context(|| format!("Invalid configuration in {}", path.display()))?;

        // プロジェクトレベルの設定を検索してマージ
        let project_path = project_search_dir.and_then(Self::find_project_config_from);
        if let Some(project_path) = project_path {
            let project = Self::load_project_config(&project_path)?;
            config.merge_project(&project);

            // マージ後に再検証
            config.validate().with_context(|| {
                format!(
                    "Invalid configuration after merging project config from {}",
                    project_path.display()
                )
            })?;
        }

        Ok(config)
    }

    /// カレントディレクトリで `.claw-hooks.toml` を検索。
    pub fn find_project_config() -> Option<PathBuf> {
        let cwd = std::env::current_dir().ok()?;
        Self::find_project_config_from(&cwd)
    }

    /// 指定ディレクトリに `.claw-hooks.toml` が存在するか確認。
    fn find_project_config_from(dir: &Path) -> Option<PathBuf> {
        let candidate = dir.join(PROJECT_CONFIG_NAME);
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    }

    /// プロジェクトレベルの設定ファイルを読み込み検証する。
    pub fn load_project_config(path: &Path) -> Result<ProjectConfig> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read project config: {}", path.display()))?;

        // プロジェクト設定でグローバル専用キーを拒否
        Self::reject_global_only_keys(&content, path)?;

        let project: ProjectConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse project config: {}", path.display()))?;

        validation::validate_project(&project)
            .with_context(|| format!("Invalid project config in {}", path.display()))?;

        Ok(project)
    }

    /// プロジェクト設定でのグローバル専用キー（debug, log_path, nano_buddy）の使用を拒否する。
    fn reject_global_only_keys(content: &str, path: &Path) -> Result<()> {
        // テキスト行走査ではなく TOML としてパースしてトップレベルキーを照合する。
        // 行走査だと複数行文字列値の継続行（行頭が `debug =` 等で始まる本文）を
        // 誤ってキーと判定して正当な設定を誤拒否し、逆に引用符付きキー
        // （`"debug" = ...`）は素通りしてしまうため、構文解析で正確に判定する。
        // 注意: toml 1.x の `Value::FromStr` は単一スカラー値用のため、ドキュメント
        // （key = value の集合）をパースするには `toml::Table` を使う必要がある。
        let table: toml::Table = match content.parse() {
            Ok(t) => t,
            // パースできない場合は、後続の本パースで詳細なエラーになるため何もしない。
            Err(_) => return Ok(()),
        };
        for key in GLOBAL_ONLY_KEYS {
            if table.contains_key(*key) {
                bail!(
                    "Project config {} contains '{}' which is only allowed in global config",
                    path.display(),
                    key
                );
            }
        }
        Ok(())
    }

    /// デフォルトパスにデフォルト設定ファイルを生成する。
    pub fn generate_default() -> Result<()> {
        Self::generate_at(&Self::default_path())
    }

    /// 指定パスにデフォルト設定ファイルを生成する。
    pub fn generate_at(path: &Path) -> Result<()> {
        // 必要に応じて親ディレクトリを作成
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

    /// コメント付きのデフォルト設定内容を返す。
    ///
    /// テンプレート本文は同ディレクトリの `default_config.toml` に外出しし、
    /// コンパイル時に `include_str!` で埋め込む(内容はバイト単位で同一)。
    fn default_config_content() -> String {
        include_str!("default_config.toml").to_string()
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

        // ファイルが作成され、デフォルト値がロードされていること
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

    #[test]
    fn test_default_config_content_parses_as_config() {
        // 外出しした default_config.toml が crate::config::Config として
        // 正しくパースできることを保証する（テンプレート破損の早期検出）。
        let content = ConfigService::default_config_content();
        let config: Config = toml::from_str(&content)
            .expect("デフォルト設定テンプレートは Config としてパースできるべき");

        // Config::default() との等価比較は行わない。理由:
        // (1) テンプレートは rm/kill/dd_block_message を明示設定するため、
        //     これらが None の Config::default() とは意図的に異なる。
        // (2) Config は PartialEq を derive していないため等価比較自体が不可。
        // 代わりにテンプレートの主要な既定値が反映されていることを確認する。
        assert!(config.rm_block);
        assert!(config.kill_block);
        assert!(config.dd_block);
        assert!(!config.debug);
        assert!(config.rm_block_message.is_some());
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
        // 親ディレクトリに.claw-hooks.tomlを配置
        fs::write(dir.path().join(".claw-hooks.toml"), "rm_block = false\n").unwrap();

        // サブディレクトリには.claw-hooks.tomlがない — 親のものを検出してはならない
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
    fn test_reject_global_only_keys_allows_multiline_string_with_keylike_body() {
        // 複数行文字列値の本文に行頭 `debug =` 等が現れても、それは値であって
        // キーではないため誤って拒否してはならない（行走査ではなく構文解析で判定）。
        let content = "rm_block_message = \"\"\"\nblocked\ndebug = true\n\"\"\"\n";
        let path = PathBuf::from("test.toml");
        assert!(
            ConfigService::reject_global_only_keys(content, &path).is_ok(),
            "複数行文字列の本文を誤ってグローバル専用キーと判定してはならない"
        );
    }

    #[test]
    fn test_reject_global_only_keys_detects_quoted_key() {
        // 引用符付きのトップレベルキー（"debug"）も正しく検出する
        // （旧来の行頭テキスト走査ではすり抜けていたバイパス）。
        let content = "\"debug\" = true\n";
        let path = PathBuf::from("test.toml");
        assert!(
            ConfigService::reject_global_only_keys(content, &path).is_err(),
            "引用符付きキー \"debug\" もグローバル専用キーとして拒否すべき"
        );
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

        // サブディレクトリにプロジェクト設定を作成
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

        // 一時ディレクトリに.claw-hooks.tomlがない — 明示的な検索ディレクトリでload_innerを使用
        let config = ConfigService::load_inner(Some(&config_path), Some(dir.path())).unwrap();

        assert!(config.rm_block);
        assert!(config.kill_block);
    }

    // === reject_global_only_keys 追加エッジケース ===

    #[test]
    fn test_reject_global_only_keys_allows_key_in_value() {
        // 値に "debug" を含むが、キーではないため許可
        let content = r#"rm_block_message = "debug mode is disabled""#;
        let path = PathBuf::from("test.toml");
        assert!(ConfigService::reject_global_only_keys(content, &path).is_ok());
    }

    #[test]
    fn test_reject_global_only_keys_detects_log_path() {
        let content = "log_path = \"/tmp/logs\"\n";
        let path = PathBuf::from("test.toml");
        let err = ConfigService::reject_global_only_keys(content, &path).unwrap_err();
        assert!(err.to_string().contains("log_path"));
    }

    #[test]
    fn test_reject_global_only_keys_detects_nano_buddy() {
        let content = "nano_buddy = true\n";
        let path = PathBuf::from("test.toml");
        let err = ConfigService::reject_global_only_keys(content, &path).unwrap_err();
        assert!(err.to_string().contains("nano_buddy"));
    }

    #[test]
    fn test_reject_global_only_keys_allows_empty_content() {
        let content = "";
        let path = PathBuf::from("test.toml");
        assert!(ConfigService::reject_global_only_keys(content, &path).is_ok());
    }

    #[test]
    fn test_reject_global_only_keys_allows_section_headers() {
        // セクションヘッダーは無視される
        let content = "[custom_filters]\ncommand = \"debug\"\n";
        let path = PathBuf::from("test.toml");
        assert!(ConfigService::reject_global_only_keys(content, &path).is_ok());
    }

    #[test]
    fn test_reject_global_only_keys_detects_with_tab() {
        // タブ + キーの組み合わせも検出する
        let content = "\tdebug = true\n";
        let path = PathBuf::from("test.toml");
        assert!(ConfigService::reject_global_only_keys(content, &path).is_err());
    }

    // === load_inner 追加テスト ===

    #[test]
    fn test_load_inner_with_none_project_dir() {
        // プロジェクト検索ディレクトリがNoneの場合、プロジェクト設定はマージされない
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            "rm_block = true\nkill_block = true\ndd_block = true\n",
        )
        .unwrap();

        let config = ConfigService::load_inner(Some(&config_path), None).unwrap();
        assert!(config.rm_block);
    }

    #[test]
    fn test_load_project_config_validates_extension_hooks_missing_placeholder() {
        // {file} プレースホルダーなしの拡張子フック → バリデーションエラー
        let dir = tempfile::TempDir::new().unwrap();
        let project_path = dir.path().join(".claw-hooks.toml");
        fs::write(
            &project_path,
            "[extension_hooks]\n\".rs\" = [\"rustfmt\"]\n",
        )
        .unwrap();

        let err = ConfigService::load_project_config(&project_path).unwrap_err();
        let err_msg = format!("{:#}", err);
        let placeholder = "{file}";
        assert!(
            err_msg.contains(placeholder),
            "エラーメッセージにplaceholder関連の記述がない: {}",
            err_msg
        );
    }

    #[test]
    fn test_load_project_config_validates_extension_key_prefix() {
        // 拡張子キーは '.' で始まる必要がある
        let dir = tempfile::TempDir::new().unwrap();
        let project_path = dir.path().join(".claw-hooks.toml");
        fs::write(
            &project_path,
            "[extension_hooks]\nrs = [\"rustfmt {file}\"]\n",
        )
        .unwrap();

        let err = ConfigService::load_project_config(&project_path).unwrap_err();
        let err_msg = format!("{:#}", err);
        assert!(err_msg.contains("must start with"));
    }
}
