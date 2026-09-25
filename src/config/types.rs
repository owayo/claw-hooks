//! 設定データ型。

use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
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
///
/// # 設定フィールド追加時のチェックリスト
///
/// フィールド定義は外部設定スキーマの明示性を優先して単一ソース化（マクロ生成）して
/// いないため、フィールドを追加・変更する際は以下を漏れなく更新すること:
///
/// 1. この `Config` 構造体（フィールド + doc コメント）
/// 2. `impl Default for Config`（デフォルト値）
/// 3. `ProjectConfig`（プロジェクト上書き用の `Option<T>` 版）
/// 4. `Config::merge_project`（上書き/マージの規則）
/// 5. `config/validation.rs`（値域チェックが必要な場合）
/// 6. `config/service.rs` のデフォルト設定テンプレートと docs/configuration.md / docs/configuration.ja.md（ユーザ向け文書）
/// 7. `merge_project` / デシリアライズのテスト
/// 8. `config/service.rs` の `KNOWN_GLOBAL_KEYS` / `KNOWN_PROJECT_KEYS`
///    （未知キー警告の対象外にする。漏れると新しいキーが「タイポ」として警告される）
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

    /// フックコマンド実行のタイムアウト（秒、デフォルト: 60、最大: 86400）
    #[serde(default = "default_hook_timeout")]
    pub hook_timeout: u64,

    /// 出力メッセージの最大長（文字数、デフォルト: 1000）。
    /// AIエージェントのコンテキストウィンドウ溢れを防止する。
    /// 0 の場合は無制限。
    #[serde(default = "default_output_max_length")]
    pub output_max_length: usize,

    /// 読み込み時に記録した警告（無視したプロジェクト設定・未知のトップレベルキー）。
    ///
    /// 設定ファイルからは読まない（`ConfigService` と `merge_project` が埋める）。
    /// 無言で無視すると「書いたのに効かない」理由が利用者から見えなくなるため、
    /// `claw-hooks check` が stderr に、フック実行時はログに出す。
    #[serde(skip)]
    pub warnings: Vec<String>,
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
            warnings: Vec::new(),
        }
    }
}

impl Config {
    /// 設定を検証し、無効な場合はエラーを返す。
    /// 包括的なバリデーションモジュールに委譲。
    ///
    /// 警告の出力は伴わない（`validation::validate_values`）。フック実行経路から呼ばれ、
    /// そこでの stderr はブロック理由の伝達チャネルだからである。警告を見せる
    /// `claw-hooks check` は `validation::validate` を使う。
    pub fn validate(&self) -> Result<()> {
        validation::validate_values(self)
    }

    /// プロジェクトレベルの設定オーバーライドをこの設定にマージする。
    ///
    /// # 信頼境界（この関数の設計理由）
    ///
    /// `.claw-hooks.toml` は「AI エージェントが clone してきたリポジトリの中のファイル」
    /// でもあり得るため、**未信頼の入力**として扱う。リポジトリに 1 ファイル置くだけで
    /// claw-hooks 自身の防御を外したり任意コマンドを実行させたりできてはならない
    /// （それが可能だと、危険コマンドを止めるという本ツールの存在意義と正面から衝突する）。
    /// したがってプロジェクト設定には**防御の緩和も、新しいコマンド実行権限の付与も
    /// 認めない**。強化方向（ブロックを増やす）だけを受け入れる:
    ///
    /// | 設定 | プロジェクト設定に許すこと |
    /// |---|---|
    /// | `rm_block` / `kill_block` / `dd_block` | 有効化のみ（`global \|\| project`。`false` への上書きは無視） |
    /// | `custom_filters` | 追加のみ（グローバル定義の削除・置換は無視） |
    /// | `stop_hooks` | 禁止（エージェント停止時に任意コマンドが走る = コード実行） |
    /// | `extension_hooks` | 禁止（ファイル編集時に任意コマンドが走る = コード実行） |
    /// | メッセージ文言 / `hook_timeout` / `output_max_length` | 従来どおり上書き可 |
    ///
    /// 最後の行を許すのは、ブロック判定そのものを弱めず、新しいコマンド実行も生まないため
    /// （メッセージはブロック時にエージェントへ返す文言で、ブロック自体は成立したままになる）。
    ///
    /// 無視した項目は `self.warnings` に理由付きで記録する。無言で無視すると
    /// 「設定を書いたのに効かない」理由が利用者から見えなくなるため。
    pub fn merge_project(&mut self, project: &ProjectConfig) {
        // ブロック設定は「有効化のみ」。緩和方向の上書きだけを落とす。
        Self::merge_block_flag(
            &mut self.rm_block,
            project.rm_block,
            "rm_block",
            &mut self.warnings,
        );
        Self::merge_block_flag(
            &mut self.kill_block,
            project.kill_block,
            "kill_block",
            &mut self.warnings,
        );
        Self::merge_block_flag(
            &mut self.dd_block,
            project.dd_block,
            "dd_block",
            &mut self.warnings,
        );

        if let Some(ref v) = project.rm_block_message {
            self.rm_block_message = Some(v.clone());
        }
        if let Some(ref v) = project.kill_block_message {
            self.kill_block_message = Some(v.clone());
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

        // カスタムフィルターは「追加のみ」。以前は置換だったため、プロジェクト設定に
        // 1 行書くだけでグローバルのルールを全部消せてしまっていた。
        if let Some(ref v) = project.custom_filters {
            // グローバル定義がある場合だけ知らせる。空リスト（旧挙動では「全消し」の
            // 指定だった）でも鳴らすのは意図的で、消したつもりの利用者に効いていない
            // ことを伝えるため。
            if !self.custom_filters.is_empty() {
                self.warnings.push(format!(
                    "project config: custom_filters cannot remove or replace the {} global filter(s) \
                     — project entries are appended to them instead",
                    self.custom_filters.len()
                ));
            }
            self.custom_filters.extend(v.iter().cloned());
        }

        // 拡張子フックと Stop フックはどちらも「任意コマンドの実行」そのものなので、
        // プロジェクト設定からは一切受け付けない（信頼確認なしのコード実行になるため）。
        if let Some(ref v) = project.extension_hooks
            && !v.is_empty()
        {
            self.warnings.push(format!(
                    "project config: {} extension_hooks entr{} ignored \
                     (extension hooks run arbitrary commands on file edits and are only accepted from the global config)",
                    v.len(),
                    if v.len() == 1 { "y was" } else { "ies were" }
                ));
        }
        if let Some(ref v) = project.stop_hooks
            && !v.is_empty()
        {
            self.warnings.push(format!(
                    "project config: {} stop_hooks entr{} ignored \
                     (stop hooks run arbitrary commands when the agent stops and are only accepted from the global config)",
                    v.len(),
                    if v.len() == 1 { "y was" } else { "ies were" }
                ));
        }
    }

    /// ブロック設定を「有効化のみ」でマージする。
    ///
    /// `global || project.unwrap_or(false)` と等価。`true` への変更（強化）は通し、
    /// `false` への変更（緩和）は無視する。無視したときだけ警告を残すことで、
    /// 「グローバルが元から false」の無害なケースで警告を出さない。
    fn merge_block_flag(
        current: &mut bool,
        requested: Option<bool>,
        field: &str,
        warnings: &mut Vec<String>,
    ) {
        let Some(requested) = requested else {
            return;
        };
        if requested {
            *current = true;
            return;
        }
        if *current {
            warnings.push(format!(
                "project config: `{field} = false` was ignored \
                 (project configs may only enable command blocking, never disable it)"
            ));
        }
    }

    /// 記録済みの警告をログに出力する。
    ///
    /// stderr ではなくログへ出す理由: Claude / Windsurf ではブロック時の stderr 本文が
    /// そのままエージェントへのブロック理由になるため、そこへ設定警告を混ぜると
    /// 判定メッセージが濁る。`claw-hooks check` だけは stderr に出す
    /// （`validation::validate` 参照）。
    pub fn log_warnings(&self) {
        for warning in &self.warnings {
            tracing::warn!("{}", warning);
        }
    }
}

/// プロジェクトレベルの設定オーバーライド。
///
/// すべてのフィールドは `Option<T>` — `None` は「未指定」（グローバルデフォルトを維持）を意味する。
/// プロジェクトルートの `.claw-hooks.toml` に配置。
///
/// **未信頼の入力**として扱うため、ここでデシリアライズできることと実際に適用される
/// ことは別である。適用範囲の規則は `Config::merge_project` を参照
/// （`stop_hooks` / `extension_hooks` は受理するが適用しない。無視した理由を警告に
/// 残すために、パースエラーにせず一度受け取っている）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProjectConfig {
    /// rm ブロックの上書き（有効化のみ。`false` は無視）
    pub rm_block: Option<bool>,
    /// rm ブロックメッセージの上書き
    pub rm_block_message: Option<String>,
    /// kill ブロックの上書き（有効化のみ。`false` は無視）
    pub kill_block: Option<bool>,
    /// kill ブロックメッセージの上書き
    pub kill_block_message: Option<String>,
    /// dd ブロックの上書き（有効化のみ。`false` は無視）
    pub dd_block: Option<bool>,
    /// dd ブロックメッセージの上書き
    pub dd_block_message: Option<String>,
    /// フックタイムアウトの上書き
    pub hook_timeout: Option<u64>,
    /// 出力最大長の上書き
    pub output_max_length: Option<usize>,
    /// 追加のカスタムフィルター（グローバルへ追記。削除・置換は不可）
    pub custom_filters: Option<Vec<CustomFilter>>,
    /// 拡張子フック（**適用されない**。任意コマンド実行のためグローバル設定限定）
    pub extension_hooks: Option<BTreeMap<String, Vec<String>>>,
    /// Stop フック（**適用されない**。任意コマンド実行のためグローバル設定限定）
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
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookCondition {
    /// このファイルが存在する場合のみフックを実行（cwd からの相対パス）
    #[serde(default)]
    pub file_exists: Option<String>,

    /// このファイルが存在しない場合のみフックを実行（cwd からの相対パス）
    #[serde(default)]
    pub file_not_exists: Option<String>,

    /// このコマンドが PATH に存在する場合のみフックを実行
    #[serde(default)]
    pub command_exists: Option<String>,

    /// このコマンドが PATH に存在しない場合のみフックを実行
    #[serde(default)]
    pub command_not_exists: Option<String>,
}

impl HookCondition {
    /// 作業ディレクトリに対して条件を評価する。
    /// 指定されたすべての条件が満たされる場合に true を返す（AND ロジック）。
    pub fn is_satisfied(&self, cwd: &Path) -> bool {
        if let Some(ref file) = self.file_exists
            && !cwd.join(file).exists()
        {
            return false;
        }
        if let Some(ref file) = self.file_not_exists
            && cwd.join(file).exists()
        {
            return false;
        }
        if let Some(ref cmd) = self.command_exists
            && !Self::command_in_path(cmd)
        {
            return false;
        }
        if let Some(ref cmd) = self.command_not_exists
            && Self::command_in_path(cmd)
        {
            return false;
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
            return Self::is_executable_command_file(command_path);
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
                if Self::is_executable_command_file(&base) {
                    return true;
                }
                if !has_extension {
                    for ext in &pathext {
                        if Self::is_executable_command_file(&dir.join(format!("{}{}", cmd, ext))) {
                            return true;
                        }
                    }
                }
            }
            false
        }

        #[cfg(not(windows))]
        {
            std::env::split_paths(&path).any(|dir| Self::is_executable_command_file(&dir.join(cmd)))
        }
    }

    /// パスがコマンドとして実行可能な通常ファイルか確認する。
    fn is_executable_command_file(path: &Path) -> bool {
        #[cfg(unix)]
        {
            path.metadata()
                .is_ok_and(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
        }

        #[cfg(not(unix))]
        {
            path.is_file()
        }
    }
}

/// Stop フックを実行するセッション種別の範囲。
///
/// Claude Code のチーム開発機能では teammate（別プロセスのエージェント）ごとに
/// Stop イベントが発火するため、デフォルトではメインセッションのみで実行し、
/// 通知スパム・重複 lint・並列 git コミットのレースを防ぐ。
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopSessionScope {
    /// メインセッションのみ実行（デフォルト）。
    #[default]
    Primary,
    /// teammate 等の委譲エージェントセッションのみ実行。
    Delegated,
    /// 両方で実行（セッション種別導入前の従来動作）。
    All,
}

impl StopSessionScope {
    /// このスコープが指定されたセッション種別で実行対象になるかを判定する。
    pub fn includes(self, kind: crate::domain::StopSessionKind) -> bool {
        use crate::domain::StopSessionKind;
        matches!(
            (self, kind),
            (Self::All, _)
                | (Self::Primary, StopSessionKind::Primary)
                | (Self::Delegated, StopSessionKind::Delegated)
        )
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
/// session_scope = "primary"
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

    /// 実行対象のセッション種別（デフォルト: primary = メインセッションのみ）。
    /// teammate 等のエージェントセッションでも実行したい場合は
    /// "delegated"（エージェントのみ）または "all"（両方）を指定する。
    #[serde(default)]
    pub session_scope: StopSessionScope,
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
            file_not_exists: None,
            command_not_exists: None,
        };
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_file_exists_not_satisfied() {
        let condition = HookCondition {
            file_exists: Some("nonexistent-file-xyz.toml".to_string()),
            command_exists: None,
            file_not_exists: None,
            command_not_exists: None,
        };
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(!condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_no_conditions_always_satisfied() {
        let condition = HookCondition {
            file_exists: None,
            command_exists: None,
            file_not_exists: None,
            command_not_exists: None,
        };
        let cwd = Path::new("/tmp");
        assert!(condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_invalid_path() {
        let condition = HookCondition {
            file_exists: Some("".to_string()),
            command_exists: None,
            file_not_exists: None,
            command_not_exists: None,
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
            file_not_exists: None,
            command_not_exists: None,
        };
        let cwd = Path::new("/tmp");
        assert!(condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_command_exists_not_satisfied() {
        let condition = HookCondition {
            file_exists: None,
            command_exists: Some("nonexistent-command-xyz-abc-999".to_string()),
            file_not_exists: None,
            command_not_exists: None,
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
            file_not_exists: None,
            command_not_exists: None,
        };
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_file_satisfied_command_not() {
        // file_exists は成立、command_exists は不成立 → false
        let condition = HookCondition {
            file_exists: Some("Cargo.toml".to_string()),
            command_exists: Some("nonexistent-command-xyz-abc-999".to_string()),
            file_not_exists: None,
            command_not_exists: None,
        };
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(!condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_command_satisfied_file_not() {
        // command_exists は成立、file_exists は不成立 → false
        let condition = HookCondition {
            file_exists: Some("nonexistent-file-xyz.toml".to_string()),
            command_exists: Some("sh".to_string()),
            file_not_exists: None,
            command_not_exists: None,
        };
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(!condition.is_satisfied(cwd));
    }

    // === file_not_exists / command_not_exists テスト ===

    #[test]
    fn test_hook_condition_file_not_exists_satisfied() {
        // 対象ファイルが「存在しない」ときにフックを実行する条件
        let condition = HookCondition {
            file_not_exists: Some("nonexistent-file-xyz.toml".to_string()),
            ..Default::default()
        };
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_file_not_exists_blocks_when_present() {
        // 対象ファイルが存在すると条件不成立
        let condition = HookCondition {
            file_not_exists: Some("Cargo.toml".to_string()),
            ..Default::default()
        };
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(!condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_command_not_exists_satisfied() {
        // 対象コマンドが PATH に存在しないとき条件成立
        let condition = HookCondition {
            command_not_exists: Some("nonexistent-command-xyz-abc-999".to_string()),
            ..Default::default()
        };
        let cwd = Path::new("/tmp");
        assert!(condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_command_not_exists_blocks_when_present() {
        // 対象コマンドが PATH にあると条件不成立
        let condition = HookCondition {
            command_not_exists: Some("sh".to_string()),
            ..Default::default()
        };
        let cwd = Path::new("/tmp");
        assert!(!condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_file_exists_and_file_not_exists_combined() {
        // file_exists と file_not_exists の AND — 一方が満たされない → false
        let condition = HookCondition {
            file_exists: Some("Cargo.toml".to_string()),
            file_not_exists: Some("Cargo.toml".to_string()),
            ..Default::default()
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
    fn test_stop_hook_session_scope_defaults_to_primary() {
        let toml_str = r#"
            [[stop_hooks]]
            commands = ["cargo fmt --check"]
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            stop_hooks: Vec<StopHook>,
        }

        let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
        assert_eq!(
            wrapper.stop_hooks[0].session_scope,
            StopSessionScope::Primary
        );
    }

    #[test]
    fn test_stop_hook_session_scope_deserializes_all_variants() {
        #[derive(Deserialize)]
        struct Wrapper {
            stop_hooks: Vec<StopHook>,
        }

        for (value, expected) in [
            ("primary", StopSessionScope::Primary),
            ("delegated", StopSessionScope::Delegated),
            ("all", StopSessionScope::All),
        ] {
            let toml_str = format!(
                r#"
                [[stop_hooks]]
                commands = ["echo done"]
                session_scope = "{}"
            "#,
                value
            );
            let wrapper: Wrapper = toml::from_str(&toml_str).unwrap();
            assert_eq!(
                wrapper.stop_hooks[0].session_scope, expected,
                "session_scope = {:?}",
                value
            );
        }
    }

    #[test]
    fn test_stop_hook_session_scope_rejects_unknown_value() {
        let toml_str = r#"
            [[stop_hooks]]
            commands = ["echo done"]
            session_scope = "sometimes"
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            stop_hooks: Vec<StopHook>,
        }

        let result: Result<Wrapper, toml::de::Error> = toml::from_str(toml_str);
        assert!(result.is_err(), "unknown session_scope value should fail");
    }

    #[test]
    fn test_stop_session_scope_includes() {
        use crate::domain::StopSessionKind;
        // (scope, kind) の全組み合わせを網羅
        assert!(StopSessionScope::Primary.includes(StopSessionKind::Primary));
        assert!(!StopSessionScope::Primary.includes(StopSessionKind::Delegated));
        assert!(!StopSessionScope::Delegated.includes(StopSessionKind::Primary));
        assert!(StopSessionScope::Delegated.includes(StopSessionKind::Delegated));
        assert!(StopSessionScope::All.includes(StopSessionKind::Primary));
        assert!(StopSessionScope::All.includes(StopSessionKind::Delegated));
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

    // 以下の一連のテストは元々「プロジェクト設定はグローバルを上書き/置換できる」という
    // 旧挙動を固定していた。プロジェクト設定は未信頼入力（clone してきたリポジトリに
    // 同梱され得る）なので、防御の緩和と新規のコマンド実行を認めない方針へ変更し、
    // 期待値をそれに合わせて更新している（詳細は `Config::merge_project` の doc 参照）。

    #[test]
    fn test_merge_project_cannot_disable_block_but_can_set_timeout() {
        let mut config = Config::default();
        assert!(config.rm_block); // default true

        let project = ProjectConfig {
            rm_block: Some(false),
            hook_timeout: Some(30),
            ..Default::default()
        };
        config.merge_project(&project);

        // 旧挙動は rm_block=false になっていた。プロジェクト設定からブロックは外せない。
        assert!(config.rm_block);
        // ブロック判定を弱めない値は従来どおり上書きできる。
        assert_eq!(config.hook_timeout, 30);
        assert!(
            config.warnings.iter().any(|w| w.contains("rm_block")),
            "無視した理由が警告に残るべき: {:?}",
            config.warnings
        );
    }

    #[test]
    fn test_merge_project_can_enable_block() {
        // 強化方向（false → true）は受け入れる。プロジェクト側でより厳しくするのは安全。
        let mut config = Config {
            rm_block: false,
            kill_block: false,
            dd_block: false,
            ..Config::default()
        };

        let project = ProjectConfig {
            rm_block: Some(true),
            kill_block: Some(true),
            dd_block: Some(true),
            ..Default::default()
        };
        config.merge_project(&project);

        assert!(config.rm_block);
        assert!(config.kill_block);
        assert!(config.dd_block);
        assert!(
            config.warnings.is_empty(),
            "強化方向の指定は無視していないので警告を出さない: {:?}",
            config.warnings
        );
    }

    #[test]
    fn test_merge_project_appends_custom_filters() {
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

        // 旧挙動は置換（グローバルの npm ルールが消えた）。現在は追加のみ。
        assert_eq!(config.custom_filters.len(), 2);
        assert_eq!(config.custom_filters[0].command, "npm");
        assert_eq!(config.custom_filters[1].command, "yarn");
        assert!(
            config.warnings.iter().any(|w| w.contains("custom_filters")),
            "置換ではなく追加になったことを警告で伝えるべき: {:?}",
            config.warnings
        );
    }

    #[test]
    fn test_merge_project_empty_vec_cannot_clear_custom_filters() {
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

        // 旧挙動では Some(vec![]) がグローバルのルールを全消しする抜け道だった。
        assert_eq!(config.custom_filters.len(), 1);
        assert_eq!(config.custom_filters[0].command, "npm");
    }

    #[test]
    fn test_merge_project_ignores_stop_hooks() {
        let mut config = Config::default();
        config.stop_hooks.push(StopHook {
            commands: vec!["global-cmd".to_string()],
            condition: None,
            stage: None,
            report: None,
            session_scope: Default::default(),
        });

        let project = ProjectConfig {
            stop_hooks: Some(vec![StopHook {
                commands: vec!["project-cmd".to_string()],
                condition: None,
                stage: None,
                report: None,
                session_scope: Default::default(),
            }]),
            ..Default::default()
        };
        config.merge_project(&project);

        // 旧挙動は extend で、リポジトリ同梱のファイルから任意コマンドを実行できた。
        assert_eq!(config.stop_hooks.len(), 1);
        assert_eq!(config.stop_hooks[0].commands, vec!["global-cmd"]);
        assert!(
            config.warnings.iter().any(|w| w.contains("stop_hooks")),
            "無視した理由が警告に残るべき: {:?}",
            config.warnings
        );
    }

    #[test]
    fn test_merge_project_ignores_extension_hooks() {
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

        // 旧挙動は置換で、グローバルの formatter を潰しつつ任意コマンドを仕込めた。
        assert!(config.extension_hooks.contains_key(".rs"));
        assert!(!config.extension_hooks.contains_key(".ts"));
        assert!(
            config
                .warnings
                .iter()
                .any(|w| w.contains("extension_hooks")),
            "無視した理由が警告に残るべき: {:?}",
            config.warnings
        );
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
            session_scope: Default::default(),
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
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
            }),
            stage: None,
            report: None,
            session_scope: Default::default(),
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
            session_scope: Default::default(),
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
            session_scope: Default::default(),
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
                file_not_exists: None,
                command_not_exists: None,
            }),
            stage: None,
            report: Some(false),
            session_scope: Default::default(),
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

    // === command_in_path 追加テスト ===

    #[test]
    fn test_command_in_path_empty_string() {
        assert!(!HookCondition::command_in_path(""));
    }

    #[test]
    fn test_command_in_path_absolute_path_existing() {
        // 絶対パスで書いた実在のコマンド。Unix は /bin/sh、Windows は ComSpec (cmd.exe の絶対パス)
        #[cfg(unix)]
        let command = String::from("/bin/sh");
        #[cfg(windows)]
        let command = std::env::var("ComSpec").expect("ComSpec should be set on Windows");
        assert!(HookCondition::command_in_path(&command));
    }

    #[test]
    fn test_command_in_path_absolute_path_nonexistent() {
        assert!(!HookCondition::command_in_path(
            "/nonexistent/path/to/command"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_command_in_path_absolute_path_non_executable_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();

        // 通常ファイルが存在しても、実行ビットが無いものはコマンドとして扱わない。
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&path, perms).unwrap();
        assert!(!HookCondition::command_in_path(path.to_str().unwrap()));

        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        assert!(HookCondition::command_in_path(path.to_str().unwrap()));
    }

    #[test]
    fn test_command_in_path_relative_path_with_slash() {
        // "./nonexistent" はコンポーネント数 > 1 で直接ファイル存在チェック
        assert!(!HookCondition::command_in_path("./nonexistent-cmd"));
    }

    #[test]
    fn test_command_in_path_known_command() {
        // "echo" は PATH に存在するはず（ビルトインだが通常 /bin/echo もある）
        // 代わりに "sh" を使う（確実に存在）
        assert!(HookCondition::command_in_path("sh"));
    }

    #[test]
    fn test_command_in_path_spaces_only() {
        // スペースのみの文字列はコマンドとして無効
        assert!(!HookCondition::command_in_path("   "));
    }

    // === HookCondition AND ロジックの境界テスト ===

    #[test]
    fn test_hook_condition_both_none_is_satisfied() {
        let condition = HookCondition {
            file_exists: None,
            command_exists: None,
            file_not_exists: None,
            command_not_exists: None,
        };
        // 任意のパスで条件を満たす
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(condition.is_satisfied(cwd));
    }

    #[test]
    fn test_hook_condition_file_exists_with_subdirectory() {
        // サブディレクトリ内のファイルを確認
        let condition = HookCondition {
            file_exists: Some("src/main.rs".to_string()),
            command_exists: None,
            file_not_exists: None,
            command_not_exists: None,
        };
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(condition.is_satisfied(cwd));
    }

    // === merge_project のテスト ===

    #[test]
    fn test_merge_project_none_fields_preserve_global() {
        // None フィールドはグローバル設定を維持する
        let mut config = Config {
            rm_block: true,
            kill_block: true,
            dd_block: true,
            hook_timeout: 120,
            ..Default::default()
        };

        let project = ProjectConfig::default(); // 全フィールド None
        config.merge_project(&project);

        assert!(config.rm_block);
        assert!(config.kill_block);
        assert!(config.dd_block);
        assert_eq!(config.hook_timeout, 120);
    }

    #[test]
    fn test_merge_project_bool_fields_cannot_be_weakened() {
        // 旧挙動: Some(false) でグローバルの true を上書きできた。
        // 現在: 緩和方向の上書きは無視し、警告のみ残す（未信頼のプロジェクト設定から
        // 安全ガードを外せないようにするため）。
        let mut config = Config {
            rm_block: true,
            kill_block: true,
            ..Default::default()
        };

        let project = ProjectConfig {
            rm_block: Some(false),
            kill_block: Some(false),
            ..Default::default()
        };
        config.merge_project(&project);

        assert!(config.rm_block);
        assert!(config.kill_block);
        assert!(config.dd_block); // 未指定のため変更なし
        assert_eq!(
            config.warnings.len(),
            2,
            "無視した 2 件分の警告が残るべき: {:?}",
            config.warnings
        );
    }

    #[test]
    fn test_merge_project_no_warning_when_global_already_disabled() {
        // グローバルが元から false なら、プロジェクトの false は何も無効化していない。
        // 警告の意味を「実際に無視した」ケースに限定して、無害なケースで鳴らさない。
        let mut config = Config {
            rm_block: false,
            ..Default::default()
        };

        let project = ProjectConfig {
            rm_block: Some(false),
            ..Default::default()
        };
        config.merge_project(&project);

        assert!(!config.rm_block);
        assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    }

    #[test]
    fn test_merge_project_overrides_message_fields() {
        // カスタムメッセージの上書き
        let mut config = Config {
            rm_block_message: Some("global msg".to_string()),
            ..Default::default()
        };

        let project = ProjectConfig {
            rm_block_message: Some("project msg".to_string()),
            ..Default::default()
        };
        config.merge_project(&project);

        assert_eq!(config.rm_block_message, Some("project msg".to_string()));
    }

    #[test]
    fn test_merge_project_custom_filters_keep_global_rules() {
        // 旧挙動: custom_filters は置換だったため、プロジェクト設定に 1 件書くだけで
        // グローバルのルール（ここでは npm ブロック）を丸ごと外せた。
        let mut config = Config {
            custom_filters: vec![CustomFilter {
                command: "npm".to_string(),
                args: vec!["install".to_string()],
                message: "use pnpm".to_string(),
            }],
            ..Default::default()
        };

        let project = ProjectConfig {
            custom_filters: Some(vec![CustomFilter {
                command: "yarn".to_string(),
                args: vec!["add".to_string()],
                message: "use pnpm".to_string(),
            }]),
            ..Default::default()
        };
        config.merge_project(&project);

        assert_eq!(config.custom_filters.len(), 2);
        assert_eq!(config.custom_filters[0].command, "npm");
        assert_eq!(config.custom_filters[1].command, "yarn");
    }

    #[test]
    fn test_merge_project_extension_hooks_keep_global_only() {
        // 旧挙動: extension_hooks は置換。グローバルの formatter を潰した上で、
        // ファイル編集のたびに任意コマンドを実行させられた。
        let mut config = Config {
            extension_hooks: BTreeMap::from([
                (".rs".to_string(), vec!["rustfmt {file}".to_string()]),
                (".ts".to_string(), vec!["prettier {file}".to_string()]),
            ]),
            ..Default::default()
        };

        let project = ProjectConfig {
            extension_hooks: Some(BTreeMap::from([(
                ".py".to_string(),
                vec!["black {file}".to_string()],
            )])),
            ..Default::default()
        };
        config.merge_project(&project);

        // グローバルの定義だけが残り、プロジェクトの定義は採用されない
        assert_eq!(config.extension_hooks.len(), 2);
        assert!(config.extension_hooks.contains_key(".rs"));
        assert!(config.extension_hooks.contains_key(".ts"));
        assert!(!config.extension_hooks.contains_key(".py"));
    }

    #[test]
    fn test_merge_project_stop_hooks_stay_global_only() {
        // 旧挙動: stop_hooks は extend。リポジトリに .claw-hooks.toml を置くだけで
        // エージェント停止時に任意コマンドが走った（コード実行）。
        let mut config = Config {
            stop_hooks: vec![StopHook {
                commands: vec!["echo global".to_string()],
                condition: None,
                stage: None,
                report: None,
                session_scope: Default::default(),
            }],
            ..Default::default()
        };

        let project = ProjectConfig {
            stop_hooks: Some(vec![StopHook {
                commands: vec!["echo project".to_string()],
                condition: None,
                stage: None,
                report: None,
                session_scope: Default::default(),
            }]),
            ..Default::default()
        };
        config.merge_project(&project);

        assert_eq!(config.stop_hooks.len(), 1);
        assert_eq!(config.stop_hooks[0].commands[0], "echo global");
    }

    #[test]
    fn test_merge_project_empty_list_does_not_clear_global_filters() {
        // 旧挙動: Some(vec![]) が「明示的な全消し」として働く抜け道だった。
        let mut config = Config {
            custom_filters: vec![CustomFilter {
                command: "npm".to_string(),
                args: vec!["install".to_string()],
                message: "use pnpm".to_string(),
            }],
            ..Default::default()
        };

        let project = ProjectConfig {
            custom_filters: Some(vec![]),
            stop_hooks: Some(vec![]),
            extension_hooks: Some(BTreeMap::new()),
            ..Default::default()
        };
        config.merge_project(&project);

        assert_eq!(config.custom_filters.len(), 1);
        // 「全消し」が効いていないことは伝える（消したつもりの利用者向け）。
        // 一方 stop_hooks / extension_hooks の空リストは無視する中身が無いので黙る。
        assert_eq!(config.warnings.len(), 1, "{:?}", config.warnings);
        assert!(config.warnings[0].contains("custom_filters"));
    }

    #[test]
    fn test_merge_project_hook_timeout_override() {
        let config_default = Config::default();
        assert_eq!(config_default.hook_timeout, 60); // デフォルト確認

        let mut config = Config::default();
        let project = ProjectConfig {
            hook_timeout: Some(300),
            ..Default::default()
        };
        config.merge_project(&project);

        assert_eq!(config.hook_timeout, 300);
    }

    #[test]
    fn test_merge_project_output_max_length_override() {
        let default_max = Config::default().output_max_length;

        let mut config = Config::default();
        let project = ProjectConfig {
            output_max_length: Some(5000),
            ..Default::default()
        };
        config.merge_project(&project);

        assert_eq!(config.output_max_length, 5000);
        assert_ne!(config.output_max_length, default_max);
    }
}
