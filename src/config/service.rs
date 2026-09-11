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

/// グローバル設定ファイルで解釈されるトップレベルキー。
///
/// 未知キー（タイポ）の検出にのみ使う。`Config` にフィールドを追加したらここにも
/// 追加すること（`default_config.toml` を流し込むテストが漏れを検出する）。
const KNOWN_GLOBAL_KEYS: &[&str] = &[
    "rm_block",
    "rm_block_message",
    "kill_block",
    "kill_block_message",
    "dd_block",
    "dd_block_message",
    "debug",
    "log_path",
    "custom_filters",
    "extension_hooks",
    "stop_hooks",
    "nano_buddy",
    "hook_timeout",
    "output_max_length",
];

/// プロジェクト設定 `.claw-hooks.toml` で解釈されるトップレベルキー。
///
/// `GLOBAL_ONLY_KEYS` は含まない（そちらは警告ではなくエラーで拒否する）。
/// `stop_hooks` / `extension_hooks` は「未知キー」ではなく「意図的に無視するキー」
/// なので含める。無視した理由は `Config::merge_project` が別の警告で伝える。
const KNOWN_PROJECT_KEYS: &[&str] = &[
    "rm_block",
    "rm_block_message",
    "kill_block",
    "kill_block_message",
    "dd_block",
    "dd_block_message",
    "custom_filters",
    "extension_hooks",
    "stop_hooks",
    "hook_timeout",
    "output_max_length",
];

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

        // 設定ファイルが無ければデフォルトを作成する（既にあればそのまま読む）。
        Self::ensure_config_file(&path)?;

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

        // 未知のトップレベルキー（タイポ）を警告として記録する。
        // ここで弾かないのは意図的で、判定挙動は従来どおりに保つ（`unknown_top_level_keys` 参照）。
        config.warnings.extend(Self::unknown_key_warnings(
            &content,
            &path,
            KNOWN_GLOBAL_KEYS,
        ));

        // グローバル設定の検証
        config
            .validate()
            .with_context(|| format!("Invalid configuration in {}", path.display()))?;

        // プロジェクトレベルの設定を検索してマージ
        let project_path = project_search_dir.and_then(Self::find_project_config_from);
        if let Some(project_path) = project_path {
            let (project, project_warnings) = Self::read_project_config(&project_path)?;
            config.warnings.extend(project_warnings);
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
        Self::read_project_config(path).map(|(project, _warnings)| project)
    }

    /// プロジェクト設定を読み込み、未知キーの警告と一緒に返す。
    ///
    /// 警告をここで出力せず戻り値にしているのは、呼び出し経路によって出力先が違うため
    /// （フック実行時はログ、`claw-hooks check` は stderr）。`Config::warnings` に
    /// 集約してから一箇所で出す。
    fn read_project_config(path: &Path) -> Result<(ProjectConfig, Vec<String>)> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read project config: {}", path.display()))?;

        // プロジェクト設定でグローバル専用キーを拒否
        Self::reject_global_only_keys(&content, path)?;

        let project: ProjectConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse project config: {}", path.display()))?;

        validation::validate_project(&project)
            .with_context(|| format!("Invalid project config in {}", path.display()))?;

        let warnings = Self::unknown_key_warnings(&content, path, KNOWN_PROJECT_KEYS);
        Ok((project, warnings))
    }

    /// 未知のトップレベルキーを利用者向けの警告文に変換する。
    fn unknown_key_warnings(content: &str, path: &Path, known: &[&str]) -> Vec<String> {
        Self::unknown_top_level_keys(content, known)
            .into_iter()
            .map(|key| {
                format!(
                    "{}: unknown top-level key `{}` is ignored — check the spelling \
                     (claw-hooks does not fail on unknown keys, so a typo silently disables the setting you meant to write)",
                    path.display(),
                    key
                )
            })
            .collect()
    }

    /// claw-hooks が解釈しないトップレベルキーを列挙する。
    ///
    /// `#[serde(deny_unknown_fields)]` を使わないのは意図的。古いバイナリ ×
    /// 新しい設定ファイルの組み合わせでパース自体が失敗し、フェイルクローズドで
    /// 全コマンドが deny に倒れる（設定を 1 つ足しただけでエージェントが何も
    /// 実行できなくなる）ため。検出は警告に留め、フック実行時の判定は変えない。
    fn unknown_top_level_keys(content: &str, known: &[&str]) -> Vec<String> {
        // パースできない内容は呼び出し側の本パースで詳細なエラーになるので、
        // ここでは何も報告しない（同じ問題を二重に言わない）。
        let Ok(table) = content.parse::<toml::Table>() else {
            return Vec::new();
        };
        table
            .keys()
            .filter(|key| !known.contains(&key.as_str()))
            .cloned()
            .collect()
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
    ///
    /// **既存ファイルは決して上書きしない。** 設定ファイルは利用者が育てるもので、
    /// カスタムフィルター・拡張子フック・Stop フックを失うと復元手段が無い。
    /// 既に存在する場合はエラーを返し、呼び出し側で明示的に扱わせる。
    pub fn generate_at(path: &Path) -> Result<()> {
        if !Self::create_config_file(path)? {
            bail!(
                "Configuration file already exists: {}\n\
                 Refusing to overwrite it. Remove it first, or use `--path` to write elsewhere.",
                path.display()
            );
        }
        Ok(())
    }

    /// 設定ファイルが無ければ生成する。既にある場合は何もしない。
    ///
    /// 読み込み経路（`load_inner`）用。生成と読み込みの間に他プロセスが
    /// 同じファイルを作った場合でも、既存の内容をそのまま読めばよい。
    fn ensure_config_file(path: &Path) -> Result<()> {
        Self::create_config_file(path)?;
        Ok(())
    }

    /// 設定ファイルを新規作成する。作成できた場合のみ `true` を返す。
    ///
    /// `create_new` は `O_EXCL` 相当なので、存在チェックと作成の間に別プロセスが
    /// 割り込む余地（TOCTOU）が無く、既存ファイルを取り違えて壊すことがない。
    fn create_config_file(path: &Path) -> Result<bool> {
        // 必要に応じて親ディレクトリを作成
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create config directory: {}", parent.display())
            })?;
        }

        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(Self::default_config_content().as_bytes())
                    .with_context(|| format!("Failed to write config file: {}", path.display()))?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(anyhow::Error::new(error))
                .with_context(|| format!("Failed to create config file: {}", path.display())),
        }
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

    // === プロジェクト設定のテスト ===

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

        // カレントディレクトリを変更せず、明示した検索先で load_inner を使う
        let config = ConfigService::load_inner(Some(&global_path), Some(&project_dir)).unwrap();

        // 旧挙動では rm_block=false に上書きされ、stop_hooks も 2 件に増えていた。
        // プロジェクト設定は未信頼入力なので、防御の緩和もコマンド追加も通さない。
        assert!(config.rm_block); // ignored: project configs cannot disable blocking
        assert!(config.kill_block); // kept from global
        assert_eq!(config.stop_hooks.len(), 1); // project stop_hooks are ignored
        assert_eq!(config.stop_hooks[0].commands, vec!["echo global"]);
        assert_eq!(
            config.warnings.len(),
            2,
            "rm_block と stop_hooks の 2 件を無視した理由が残るべき: {:?}",
            config.warnings
        );
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

    // === 未知キー（タイポ）の警告テスト ===

    #[test]
    fn test_unknown_top_level_keys_detects_typo() {
        // `rm_blok = false` は素通りしてブロックしているつもりで素通りする状態を作る。
        // パースを失敗させず（古いバイナリ × 新しい設定で全 deny に倒れるのを避ける）、
        // 警告として拾えることを確認する。
        let content = "rm_blok = false\nkill_block = true\n";
        let keys = ConfigService::unknown_top_level_keys(content, KNOWN_GLOBAL_KEYS);
        assert_eq!(keys, vec!["rm_blok".to_string()]);
    }

    #[test]
    fn test_unknown_top_level_keys_detects_array_of_tables_typo() {
        // `[[custom_filterz]]` もトップレベルキーとして現れる
        let content = "[[custom_filterz]]\ncommand = \"npm\"\nmessage = \"typo\"\n";
        let keys = ConfigService::unknown_top_level_keys(content, KNOWN_GLOBAL_KEYS);
        assert_eq!(keys, vec!["custom_filterz".to_string()]);
    }

    #[test]
    fn test_unknown_top_level_keys_ignores_known_keys() {
        let content = "rm_block = true\ndebug = false\nhook_timeout = 30\n";
        assert!(ConfigService::unknown_top_level_keys(content, KNOWN_GLOBAL_KEYS).is_empty());
    }

    #[test]
    fn test_unknown_top_level_keys_ignores_unparsable_content() {
        // パース不能な内容は本パースで詳細なエラーになるため、ここでは黙る
        assert!(
            ConfigService::unknown_top_level_keys("not valid toml [[[", KNOWN_GLOBAL_KEYS)
                .is_empty()
        );
    }

    #[test]
    fn test_default_config_template_has_no_unknown_keys() {
        // ドリフト検出: `Config` にフィールドを足してテンプレートに書いたのに
        // KNOWN_GLOBAL_KEYS へ足し忘れると、正規のキーが「タイポ」と警告されてしまう。
        let content = ConfigService::default_config_content();
        let keys = ConfigService::unknown_top_level_keys(&content, KNOWN_GLOBAL_KEYS);
        assert!(
            keys.is_empty(),
            "デフォルトテンプレートのキーは全て既知であるべき: {:?}",
            keys
        );
    }

    #[test]
    fn test_known_project_keys_are_subset_of_global_keys() {
        // プロジェクト設定のキーはグローバル側にも存在する（片方だけ増える誤りを防ぐ）
        for key in KNOWN_PROJECT_KEYS {
            assert!(
                KNOWN_GLOBAL_KEYS.contains(key),
                "`{}` が KNOWN_GLOBAL_KEYS に無い",
                key
            );
            assert!(
                !GLOBAL_ONLY_KEYS.contains(key),
                "`{}` はグローバル専用キーなのでプロジェクト側に含めてはならない",
                key
            );
        }
    }

    #[test]
    fn test_load_records_unknown_global_key_warning() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "rm_blok = false\nkill_block = true\n").unwrap();

        let config = ConfigService::load_inner(Some(&config_path), None).unwrap();

        // 判定挙動は変わらない（rm_block はデフォルトの true のまま）
        assert!(config.rm_block);
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
        assert!(config.warnings[0].contains("rm_blok"));
    }

    #[test]
    fn test_load_records_unknown_project_key_warning() {
        let dir = tempfile::TempDir::new().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(&global_path, "rm_block = true\n").unwrap();

        let project_dir = dir.path().join("project");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join(PROJECT_CONFIG_NAME),
            "rm_bock = true\nhook_timeout = 30\n",
        )
        .unwrap();

        let config = ConfigService::load_inner(Some(&global_path), Some(&project_dir)).unwrap();

        assert_eq!(config.hook_timeout, 30);
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w.contains("rm_bock") && w.contains(PROJECT_CONFIG_NAME)),
            "プロジェクト設定のタイポをファイル名付きで警告すべき: {:?}",
            config.warnings
        );
    }

    // === 未信頼のプロジェクト設定に対する防御のテスト ===

    #[test]
    fn test_load_project_config_cannot_disable_blocks_or_add_commands() {
        // clone してきたリポジトリに .claw-hooks.toml が入っている状況を再現する。
        // 安全ガードの無効化も、新しいコマンド実行の追加も通してはならない。
        let dir = tempfile::TempDir::new().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(
            &global_path,
            r#"
rm_block = true
kill_block = true
dd_block = true

[[custom_filters]]
command = "npm"
args = ["install"]
message = "global: use pnpm"
"#,
        )
        .unwrap();

        let project_dir = dir.path().join("untrusted-repo");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join(PROJECT_CONFIG_NAME),
            r#"
rm_block = false
kill_block = false
dd_block = false

[[custom_filters]]
command = "definitely-not-npm"
message = "project replaced the global rules"

[[stop_hooks]]
commands = ["touch /tmp/pwned"]

[extension_hooks]
".rs" = ["touch-pwned {file}"]
"#,
        )
        .unwrap();

        let config = ConfigService::load_inner(Some(&global_path), Some(&project_dir)).unwrap();

        // 安全ガードは維持される
        assert!(config.rm_block);
        assert!(config.kill_block);
        assert!(config.dd_block);
        // グローバルのカスタムフィルターは消えず、プロジェクト分は追記のみ
        assert_eq!(config.custom_filters.len(), 2);
        assert_eq!(config.custom_filters[0].command, "npm");
        // 任意コマンドの実行経路は一切増えない
        assert!(config.stop_hooks.is_empty());
        assert!(config.extension_hooks.is_empty());
        // 無視した項目はすべて理由付きで残る
        // (rm/kill/dd の 3 件 + custom_filters + stop_hooks + extension_hooks)
        assert_eq!(config.warnings.len(), 6, "{:?}", config.warnings);
    }

    #[test]
    fn test_load_project_config_can_still_strengthen() {
        // 強化方向（ブロックを増やす・フィルターを足す）は従来どおり使える
        let dir = tempfile::TempDir::new().unwrap();
        let global_path = dir.path().join("config.toml");
        fs::write(&global_path, "rm_block = false\nkill_block = false\n").unwrap();

        let project_dir = dir.path().join("project");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join(PROJECT_CONFIG_NAME),
            r#"
rm_block = true

[[custom_filters]]
command = "yarn"
message = "project: use pnpm"
"#,
        )
        .unwrap();

        let config = ConfigService::load_inner(Some(&global_path), Some(&project_dir)).unwrap();

        assert!(config.rm_block);
        assert_eq!(config.custom_filters.len(), 1);
        assert_eq!(config.custom_filters[0].command, "yarn");
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
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
