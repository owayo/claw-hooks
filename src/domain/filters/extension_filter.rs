//! 拡張子ベースのフックフィルターの実装。

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tracing::{debug, info, warn};

use super::Filter;
use crate::config::{ExtensionHookCommand, HookCondition};
use crate::domain::command::{
    configure_process_group, display_label, run_with_timeout, spawn_serialized,
};
use crate::domain::normalize::normalize_lint_output;
use crate::domain::state_store::{MarkOutcome, StateStore};
use crate::domain::{AgentProfile, Decision, FileOperationInput, HookEvent, HookInput, ToolInput};

/// 見つからないコマンドの通知の記録を置く種類（`StateStore` のサブディレクトリ）。
const NOTICE_KIND: &str = "notices";

/// 見つからないコマンドの通知の記録のキーの版。キーの組み立て方を変えたら上げる
/// （古い記録と取り違えないため）。
const MISSING_COMMAND_KEY_VERSION: &str = "missing-command-v1";

/// 見つからないコマンドの通知の記録を残す期間。
///
/// これより長く続くセッションでは、記録が掃除されてもう一度知らせることがある。
/// 記録が消えても通知が 1 回増えるだけなので、保証は「記録が残っている間は 1 回」に留める。
const MISSING_COMMAND_NOTICE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// 見つからないコマンドの通知を初めて返すときに、文面の後ろへ付ける一文。
/// 記録を残せたときだけ付ける（残せなければ次の編集でもまた知らせるため）。
const NOT_REPEATED_SUFFIX: &str = ". This notice is not repeated in this session.";

/// パース済みコマンドテンプレートの結果。
pub(crate) struct ParsedCommand {
    /// 実行するコマンド/プログラム
    program: String,
    /// ファイルプレースホルダーの前の引数
    args_before: Vec<String>,
    /// ファイルプレースホルダーの後の引数
    args_after: Vec<String>,
    /// {file} がインラインで使用される場合（例: --file={file}）のテンプレートトークン
    inline_template: Option<String>,
}

/// 単一コマンドの実行結果。
struct CommandResult {
    /// エージェント向け表示ラベル（プログラム名のみ）。
    /// ログ用のサニタイズ要約（args_before= 等）はエージェントには意味がなく
    /// トークンの無駄になるため、表示にはプログラム名だけを使う。
    /// ファイルパスは含まれない（プログラム名は設定テンプレート由来のため）。
    display_label: String,
    /// コマンドが成功したかどうか
    success: bool,
    /// 結合された stdout と stderr の出力
    output: String,
}

/// 1 コマンドを動かせなかった理由。
///
/// エージェントへ返す文面は `agent_message` で組み立てる。起動と待機の失敗には、どのコマンドの
/// 失敗かが分かるようにラベルを付ける（同じファイルに複数のフックがあるとき、成功したフックの
/// 出力と区別できるように）。`Debug` は実装しない（`program` をログへ出さないため）。
enum CommandFailure {
    /// 起動する前に弾いた（ファイルパスの検証・コマンドテンプレートのパース）。
    /// 文面は `[ERROR] <理由>` のまま返す
    Rejected(String),
    /// プロセスを起動できなかった
    NotStarted {
        /// 表示ラベル（`display_label`）
        label: String,
        /// 設定のテンプレートから取った、引用符除去後のプログラムトークン。
        /// 通知の記録のキーにだけ使い、ログや文面には出さない（ディレクトリを含み得るため）
        program: String,
        /// 文面を決める原因の分類
        cause: StartFailure,
        /// spawn の失敗の種類（ログ用）
        kind: io::ErrorKind,
        /// OS のエラー番号（ログ用）
        os_error: Option<i32>,
    },
    /// 起動したプロセスの終了を待てず、結果が分からない
    WaitFailed {
        /// 表示ラベル（`display_label`）
        label: String,
    },
}

impl CommandFailure {
    /// エージェントへ返す文面。
    fn agent_message(&self) -> String {
        match self {
            Self::Rejected(reason) => format!("[ERROR] {}", reason),
            Self::NotStarted { label, cause, .. } => format!("[{}] not started: {}", label, cause),
            Self::WaitFailed { label } => format!(
                "[{}] execution result unavailable: failed to wait for the process",
                label
            ),
        }
    }

    /// 永続ログへ失敗を残す。ラベル・失敗の種類・OS のエラー番号だけを残し、
    /// パスや引数は残さない（`io::Error` の本文もパスを含み得るので残さない）。
    fn log(&self) {
        match self {
            Self::Rejected(reason) => warn!("❌ Extension hook failed: {}", reason),
            Self::NotStarted {
                label,
                kind,
                os_error,
                ..
            } => warn!(
                "❌ Extension hook [{}] not started: kind={:?} os_error={:?}",
                label, kind, os_error
            ),
            Self::WaitFailed { label } => warn!(
                "❌ Extension hook [{}] execution result unavailable: failed to wait for the process",
                label
            ),
        }
    }
}

/// プロセスを起動できなかった原因。エージェントへ返す文面を分けるために使う。
///
/// spawn の `NotFound` は「設定したプログラムが無い」とは限らない。スクリプトの shebang の
/// インタープリターや実行ファイルのローダーが無い場合も exec は ENOENT を返す。そこで
/// プログラム自体が無いことを確かめられたときだけ「見つからない」と言い切り、それ以外は
/// 「実行ファイルかインタープリターが無い」と原因を絞らずに返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartFailure {
    /// PATH から探すプログラムが PATH に無い
    NotFoundInPath,
    /// パスで指定したプログラムのファイルが無い
    NotFoundAtPath,
    /// プログラムのファイルはあるのに NotFound になった（インタープリター等が無い）、
    /// またはファイルの有無を確かめられなかった
    ExecutableOrInterpreterNotFound,
    /// 実行する権限が無い
    PermissionDenied,
    /// その他の起動の失敗
    Other(io::ErrorKind),
}

impl StartFailure {
    /// spawn の失敗の種類から原因を決める。
    fn classify(program: &str, kind: io::ErrorKind) -> Self {
        match kind {
            // Windows は `cmd /c` を起動するので、NotFound は設定のプログラムではなく cmd の
            // 失敗になる。設定のプログラムの有無で言い分けない（プログラムが無いときは cmd が
            // 9009 で終わり、通常の失敗として出力が返る）
            io::ErrorKind::NotFound if !cfg!(target_os = "windows") => {
                Self::classify_not_found(program)
            }
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            kind => Self::Other(kind),
        }
    }

    /// spawn が NotFound だったときに、プログラム自体が無いのかを確かめる。
    ///
    /// PATH から探したプログラムは、条件の `command_exists` と同じ解決規則
    /// （`HookCondition::command_in_path`）で有無を確かめる。パスで指定したかどうかの判定も
    /// 同じ規則（`HookCondition::is_explicit_path`）を使い、`claw-hooks check` の警告の
    /// 「PATH に無い」と「設定したパスに無い」の書き分けと食い違わないようにする。
    fn classify_not_found(program: &str) -> Self {
        if HookCondition::is_explicit_path(program) {
            match Path::new(program).try_exists() {
                Ok(false) => Self::NotFoundAtPath,
                // ファイルはあるのに起動できない（インタープリターが無い等）か、有無を確かめられない
                Ok(true) | Err(_) => Self::ExecutableOrInterpreterNotFound,
            }
        } else if HookCondition::command_in_path(program) {
            // PATH に実行ファイルはある。起動に要るインタープリター等が無い
            Self::ExecutableOrInterpreterNotFound
        } else {
            Self::NotFoundInPath
        }
    }

    /// プログラムが無いことを確かめられた失敗か（通知をセッションにつき 1 回にする対象）。
    ///
    /// それ以外の起動の失敗（インタープリターが無い・権限不足など）は、設定か環境を直すまで
    /// 毎回起きる別の問題なので、毎回返す。
    fn is_missing_command(self) -> bool {
        matches!(self, Self::NotFoundInPath | Self::NotFoundAtPath)
    }
}

impl std::fmt::Display for StartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFoundInPath => f.write_str("command not found in PATH"),
            Self::NotFoundAtPath => f.write_str("command not found at the configured path"),
            Self::ExecutableOrInterpreterNotFound => {
                f.write_str("executable or required interpreter not found")
            }
            Self::PermissionDenied => f.write_str("permission denied"),
            Self::Other(kind) => write!(f, "{}", kind),
        }
    }
}

/// コマンドテンプレートのプログラムの表示ラベル（テンプレートが空なら `<unknown>`）。
fn template_label(template: &str) -> String {
    let tokens = crate::domain::parse_shell_tokens(template);
    display_label(tokens.first().map_or("", String::as_str))
}

/// 1 回のフック呼び出し（1 回の編集）の中で、ファイルとコマンドをまたいで共有する情報。
struct RunContext<'a> {
    /// 見つからないコマンドの通知の記録に使うセッション ID（空白だけなら無し）
    session_id: Option<&'a str>,
    /// フックプロセスの作業ディレクトリ。条件の評価（Stop フックの条件と同じ基準）と、
    /// 相対パスで指定したプログラムの通知の記録のキーに使う
    cwd: PathBuf,
    /// この呼び出しで既に扱った「見つからないコマンド」の記録のキー。
    ///
    /// 同じ編集で同じプログラムが何度失敗しても（拡張子のキーと `"*"` の両方に書いた、
    /// 複数ファイルの apply_patch）、通知は 1 回にする。記録を残せない場合（セッション ID が
    /// 無い・状態の置き場所が使えない）も、同じ編集の中で同じ文面を並べないためにこれで除く。
    missing_notices: HashSet<String>,
    /// コマンドを動かせなかった知らせ（起動・待機の失敗）。起きた順に並べる。
    ///
    /// エージェントへ返すときはコマンドの出力より先に置き、lint 出力の正規化にもかけない。
    /// アダプターは追加コンテキストを先頭から `output_max_length` で切り詰めるので、長い lint
    /// 出力の後ろに置くと切り落とされる。見つからないコマンドの通知は、返した時点で「知らせた」と
    /// 記録するため、切り落とされるとそのセッションでは二度と届かなくなる。
    notices: Vec<String>,
}

impl<'a> RunContext<'a> {
    fn new(input: &'a HookInput) -> Self {
        Self {
            session_id: input
                .session_id
                .as_deref()
                .filter(|session_id| !session_id.trim().is_empty()),
            cwd: std::env::current_dir().unwrap_or_default(),
            missing_notices: HashSet::new(),
            notices: Vec::new(),
        }
    }
}

/// 拡張子ベースのフックフィルター。
pub struct ExtensionHookFilter {
    /// 拡張子 → コマンドのマップ（例: ".go" → ["gofmt -w {file}", "golangci-lint run {file}"]）
    by_extension: BTreeMap<String, Vec<ExtensionHookCommand>>,
    /// すべてのファイルに当てるコマンド（キー `"*"`）。拡張子のキーのコマンドの後に動かす
    catch_all: Vec<ExtensionHookCommand>,
    nano_buddy: bool,
    timeout_secs: u64,
    /// 呼び出し元エージェント（通知の記録のキーの名前空間に使う。セッション ID は
    /// エージェントごとの採番なので、別のエージェントの同じ ID と取り違えないため）
    agent: AgentProfile,
    /// 見つからないコマンドの通知を「セッションにつき 1 回」にするための記録の置き場所
    store: StateStore,
}

impl ExtensionHookFilter {
    /// 拡張子を問わず、編集したすべてのファイルに当てるコマンドを書くキー。
    /// 拡張子のないファイル（`Makefile`）やドットファイル（`.gitignore`）にも当たる。
    pub const CATCH_ALL_KEY: &'static str = "*";

    /// 新しい ExtensionHookFilter を作成する。
    ///
    /// `hooks` は設定の `[extension_hooks]` そのもので、`"*"` のキーがあれば catch-all として
    /// 取り出す。状態の置き場所は無効（通知の記録を残さない）で、`with_state` で渡す。
    pub fn new<C: Into<ExtensionHookCommand>>(
        hooks: BTreeMap<String, Vec<C>>,
        nano_buddy: bool,
        timeout_secs: u64,
    ) -> Self {
        let mut hooks: BTreeMap<String, Vec<ExtensionHookCommand>> = hooks
            .into_iter()
            .map(|(key, commands)| (key, commands.into_iter().map(Into::into).collect()))
            .collect();
        let catch_all = hooks.remove(Self::CATCH_ALL_KEY).unwrap_or_default();
        Self {
            by_extension: hooks,
            catch_all,
            nano_buddy,
            timeout_secs,
            agent: AgentProfile::default(),
            store: StateStore::disabled(),
        }
    }

    /// 呼び出し元エージェントと状態の置き場所を設定する。
    pub fn with_state(mut self, agent: AgentProfile, store: StateStore) -> Self {
        self.agent = agent;
        self.store = store;
        self
    }

    /// ファイルパスから拡張子を抽出する（先頭のドットを含まない）。
    fn extract_ext(file_path: &str) -> Option<String> {
        Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_string())
    }

    /// ファイルパスに当てるコマンドを、実行する順に返す。
    ///
    /// 拡張子のキーのコマンドを先に、`"*"` のコマンドを後に並べる。formatter が書き換えた後の
    /// 内容を catch-all の linter に渡すためである。TOML のキーの書き順は `BTreeMap` に読んだ
    /// 時点で失われるので、順序は書き順ではなくここで固定する。
    ///
    /// 条件付きのコマンドも条件を評価せずに返す（条件は動かす直前に `execute_commands` で
    /// 評価する。前のコマンドが作ったファイルを条件に使えるように）。
    fn matching_commands(&self, file_path: &str) -> Vec<&ExtensionHookCommand> {
        let by_extension = Self::extract_ext(file_path)
            .and_then(|extension| self.by_extension.get(&format!(".{}", extension)));
        by_extension
            .into_iter()
            .flatten()
            .chain(&self.catch_all)
            .collect()
    }

    /// ファイルパスのセキュリティ検証。
    /// パスが安全なら Ok(())、危険なら Err を返す。
    fn validate_file_path(file_path: &str) -> Result<(), String> {
        // 親ディレクトリトラバーサル（../ や /a/../b）を防止
        if Path::new(file_path)
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err("Path traversal detected".to_string());
        }

        // コマンドフラグとして解釈されるパスを防止
        // '-' をフラグと解釈するツール向けに ./ プレフィックスで安全化
        if file_path.starts_with('-') {
            return Err("Path starting with '-' could be interpreted as flag".to_string());
        }

        // インジェクションを引き起こすシェルメタ文字を防止
        // 注: シェルは使用しないが、一部ツールがこれらを解釈する可能性がある
        // Windows では `cmd /c` 経由のため `%VAR%` 環境変数展開、`!VAR!` 遅延展開、
        // `^` エスケープ、`"` クォート切替が攻撃ベクタになり得るため一律拒否する。
        // タブ文字 (`\t`) は POSIX シェルの IFS として単語分割を引き起こすため
        // `\n` `\r` と同様に拒否する。
        const DANGEROUS_CHARS: &[char] = &[
            '`', '$', '|', '&', ';', '<', '>', '\n', '\r', '\t', '\0', '%', '!', '^', '"',
        ];
        for c in DANGEROUS_CHARS {
            if file_path.contains(*c) {
                return Err(format!("Path contains dangerous character: {:?}", c));
            }
        }

        Ok(())
    }

    /// コマンドテンプレートをパースして構造化された結果を返す。
    /// --file={file} のようなインラインパターンを含む {file} プレースホルダーを安全に処理する。
    pub(crate) fn parse_command_template(template: &str) -> Result<ParsedCommand, String> {
        let parts = crate::domain::parse_shell_tokens(template);
        if parts.is_empty() {
            return Err("Empty command template".to_string());
        }

        let program = parts[0].clone();
        if program.contains("{file}") {
            return Err("Command template cannot use {file} as executable".to_string());
        }

        let mut args_before = Vec::new();
        let mut args_after = Vec::new();
        let mut found_placeholder = false;
        let mut placeholder_count = 0usize;
        let mut inline_template: Option<String> = None;

        for part in parts.iter().skip(1) {
            if *part == "{file}" {
                // 単独の {file} プレースホルダー
                found_placeholder = true;
                placeholder_count += 1;
            } else if part.contains("{file}") {
                // --file={file} のようなインラインプレースホルダー
                found_placeholder = true;
                let count = part.matches("{file}").count();
                placeholder_count += count;
                inline_template = Some(part.clone());
            } else if found_placeholder {
                args_after.push(part.clone());
            } else {
                args_before.push(part.clone());
            }
        }

        if !found_placeholder || placeholder_count == 0 {
            return Err("Command template must contain {file} placeholder".to_string());
        }
        if placeholder_count != 1 {
            return Err("Command template must contain exactly one {file} placeholder".to_string());
        }

        Ok(ParsedCommand {
            program,
            args_before,
            args_after,
            inline_template,
        })
    }

    /// 単一コマンドを安全に実行して結果を返す。
    /// セキュリティ: ファイルパスはインジェクション防止のため個別の引数として渡される。
    ///
    /// 起動できなかった・終了を待てなかった場合は、どのコマンドの失敗かと原因を
    /// `CommandFailure` で返す（文面は呼び出し元が `agent_message` で組み立てる）。
    fn execute_command(
        &self,
        command_template: &str,
        file_path: &str,
    ) -> Result<CommandResult, CommandFailure> {
        // ファイルパスの検証。呼び出し元（execute_commands）がファイル単位で検証済みだが、
        // パスをコマンドラインへ渡す直前のここでも確かめる
        Self::validate_file_path(file_path).map_err(CommandFailure::Rejected)?;

        // コマンドテンプレートのパース
        let parsed =
            Self::parse_command_template(command_template).map_err(CommandFailure::Rejected)?;
        // エージェントへ返す文面にもログにも使うラベル。ディレクトリを除き、制御文字を潰す
        let label = display_label(&parsed.program);

        // '-' をフラグと解釈するツール向けに ./ プレフィックスを付与
        let safe_path = if file_path.starts_with('-') {
            // 検証で弾かれるはずだが念のため
            format!("./{}", file_path)
        } else {
            file_path.to_string()
        };

        // 永続ログには展開済みファイルパスを残さず、プログラムと引数構造の要約のみ記録する。
        // ファイルパスはユーザーの作業ディレクトリ階層を含み、機密的になり得るため、
        // 詳細確認は `--trace` （stderr 出力、ディスク非永続）に委ねる。
        debug!(
            "🪛 Executing extension hook: program={} args_before={} args_after={} path_bytes={} inline={}",
            label,
            parsed.args_before.len(),
            parsed.args_after.len(),
            safe_path.len(),
            parsed.inline_template.is_some()
        );

        // ファイルパスを個別の引数としてコマンドを構築
        // Windows では `cmd /c` を使用して .cmd/.bat ラッパー（例: npx.cmd）を解決
        let mut cmd = if cfg!(target_os = "windows") {
            let mut c = Command::new("cmd");
            c.arg("/c").arg(&parsed.program);
            c
        } else {
            Command::new(&parsed.program)
        };
        cmd.args(&parsed.args_before);

        if let Some(ref template) = parsed.inline_template {
            // --file={file} のようなインラインテンプレートを処理
            let arg = template.replace("{file}", &safe_path);
            cmd.arg(&arg);
        } else {
            // 単独の {file} プレースホルダー
            cmd.arg(&safe_path);
        }

        cmd.args(&parsed.args_after);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        // Unix では子プロセスを新しいプロセスグループに配置し、タイムアウト時に
        // 孫プロセス (例: `sh -c 'sleep'` の `sleep`) も含めて確実に停止できるようにする。
        configure_process_group(&mut cmd);

        // 永続ログには展開済みファイルパスを含めない（機密非永続化方針）。
        // ただしこの文字列は**ログ専用**で、エージェントへ返すラベルには使わない。
        // `run_with_timeout` の第 3 引数はタイムアウト本文へそのまま埋め込まれ、
        // それが `additionalContext` としてエージェントに届くため、ここに
        // `args_before=... path_bytes=...` のような内部カウンタを渡すと
        // 「エージェント向けラベルは設定されたプログラム名だけ」という
        // 設計上の不変条件（AGENTS.md）を破り、無意味なトークンを消費する。
        let sanitized_command = format!(
            "{} args_before={} args_after={} inline={} path_bytes={}",
            label,
            parsed.args_before.len(),
            parsed.args_after.len(),
            parsed.inline_template.is_some(),
            safe_path.len()
        );

        let start = std::time::Instant::now();
        // 並列のフックとパイプを取り違えないよう、起動は spawn_serialized を通す
        let child = match spawn_serialized(&mut cmd) {
            Ok(child) => child,
            Err(e) => {
                return Err(CommandFailure::NotStarted {
                    cause: StartFailure::classify(&parsed.program, e.kind()),
                    kind: e.kind(),
                    os_error: e.raw_os_error(),
                    label,
                    program: parsed.program,
                });
            }
        };
        // タイムアウト本文はエージェントへ返るので、プログラム名だけを渡す
        // （stop_filter 側と同じ扱い）。
        let result = run_with_timeout(child, self.timeout_secs, &label);
        let elapsed = start.elapsed();
        // 待機の失敗の理由（`run_with_timeout` の Err の本文）は文面にもログにも使わない。
        // 結果が分からないことだけを、どのコマンドかを付けて返す。
        let Ok(output) = result else {
            return Err(CommandFailure::WaitFailed { label });
        };
        // 完了ログには展開済みコマンド全文（ファイルパスを含む）を残さず、
        // プログラム名と所要時間のサマリのみを記録する（機密非永続化方針）。
        info!(
            "⏰️ Extension hook [{}] completed in {:.2}s",
            label,
            elapsed.as_secs_f64()
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // stdout と stderr を結合（空行を除外）
        let combined_output = [stdout.trim(), stderr.trim()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("\n");

        if !output.status.success() {
            let exit_code = output
                .status
                .code()
                .map_or("signal".to_string(), |c| c.to_string());
            let has_output = !stderr.trim().is_empty() || !stdout.trim().is_empty();
            if !has_output {
                warn!(
                    "⚠️ Extension hook command failed (exit code {}): {} stdout=0 bytes stderr=0 bytes",
                    exit_code, sanitized_command
                );
            } else {
                warn!(
                    "⚠️ Extension hook command failed (exit code {}): {} stdout={} bytes stderr={} bytes",
                    exit_code,
                    sanitized_command,
                    output.stdout.len(),
                    output.stderr.len()
                );
            }
        }

        Ok(CommandResult {
            display_label: label,
            success: output.status.success(),
            output: combined_output,
        })
    }

    /// 1 つのファイルに当てるすべてのコマンドを実行し、出力を収集する。
    /// 警告/エラーを出力したすべてのコマンドの結合出力を返す。
    /// 起動・待機に失敗したコマンドの知らせは、出力ではなく `run.notices` に積む。
    ///
    /// 条件付きのコマンドは、動かす直前に条件を評価し、満たさなければそのコマンドだけを
    /// 飛ばす。`before_first_start` は、このファイルで最初のコマンドを起動する直前に 1 回だけ
    /// 呼ぶ（NanoBuddy の通知を、実際に動かすコマンドがあるときだけ、フックより先に送るため）。
    fn execute_commands(
        &self,
        commands: &[&ExtensionHookCommand],
        file_path: &str,
        run: &mut RunContext<'_>,
        before_first_start: impl FnOnce(),
    ) -> (bool, Option<String>) {
        // パスの検証はファイル単位で先に 1 回行い、落ちたらどのコマンドも起動しない。
        // コマンドごとの検証に任せると、同じ理由の `[ERROR]` がコマンドの数だけ並ぶ
        // （`"*"` はすべてのファイルに当たるので、1 ファイルあたりのコマンドが増える）。
        if let Err(e) = Self::validate_file_path(file_path) {
            warn!("❌ Extension hooks skipped for this file: {}", e);
            return (false, Some(format!("[ERROR] {}", e)));
        }

        let mut before_first_start = Some(before_first_start);
        let mut all_success = true;
        let mut outputs: Vec<String> = Vec::new();

        for entry in commands {
            // 条件は動かす直前に評価する（前のコマンドが作ったファイルを条件に使えるように）
            if !Self::condition_allows(entry, &run.cwd) {
                continue;
            }
            if let Some(notify) = before_first_start.take() {
                notify();
            }
            match self.execute_command(&entry.command, file_path) {
                Ok(result) => {
                    if !result.success {
                        all_success = false;
                        // 終了コードだけで失敗したコマンドも、エージェントが認識できるようにする。
                        if result.output.is_empty() {
                            outputs.push(format!(
                                "[{}] command failed without output",
                                result.display_label
                            ));
                            continue;
                        }
                    }
                    // 成功時の no-op 完了メッセージ（`1 file already formatted` /
                    // `All checks passed!` 等）は編集のたびに毎回出る定型通知で、
                    // エージェントに返しても行動につながらないため破棄する。
                    //
                    // 判定は「正規化後」の文字列で行う。生の出力には正規化で捨てられる
                    // ノイズが混ざり得るためである。例えば ruff の
                    // `check --select D...` は stdout に `All checks passed!` を出しつつ
                    // stderr へ毎回 `warning: ... are incompatible. Ignoring ...` を出す。
                    // 生の結合文字列で判定すると「no-op ではない」と見なされて出力が残り、
                    // その後の正規化で警告だけが消えるため、結果として
                    // `[ruff] All checks passed!` が編集のたびにエージェントへ返っていた。
                    // 収集する文字列自体は変更しない（正規化は最後に一度だけ適用する）。
                    //
                    // 正規化で中身が空になった場合も no-op とみなす。正規化はノイズ行
                    // （ルールセット非互換警告・冗長な集計行・枠線など）を落とすため、
                    // 「ノイズしか無かった」出力はここで空文字列になる。これを no-op に
                    // 含めないと、空の追加コンテキストをエージェントへ返してしまう。
                    let normalized_output = normalize_lint_output(&result.output);
                    let noop_success = result.success
                        && (normalized_output.is_empty()
                            || crate::domain::is_noop_success_output(&normalized_output));
                    // 空でない出力を収集（警告、エラー、lint メッセージ）
                    if !result.output.is_empty() && !noop_success {
                        outputs.push(format!("[{}] {}", result.display_label, result.output));
                    }
                }
                Err(failure) => {
                    all_success = false;
                    failure.log();
                    if let Some(notice) = self.failure_message(&failure, run) {
                        run.notices.push(notice);
                    }
                }
            }
        }

        let combined = if outputs.is_empty() {
            None
        } else {
            Some(outputs.join("\n"))
        };

        (all_success, combined)
    }

    /// コマンドの実行条件を評価し、動かしてよいかを返す。条件が無ければ常に動かす。
    ///
    /// 評価できなかった条件（ファイルの有無を権限不足などで確かめられない）は、満たさない
    /// ものとして動かさず、警告をログに残す。条件を付けるのは「無ければ動かさなくてよい」
    /// 任意のツールなので、動かさない側に倒す。どちらの場合もエージェントへは返さない
    /// （拡張子フックは編集のたびに走るので、同じ知らせを毎回エージェントの文脈へ積まない）。
    fn condition_allows(entry: &ExtensionHookCommand, cwd: &Path) -> bool {
        let Some(condition) = &entry.condition else {
            return true;
        };
        match condition.evaluate(cwd) {
            Ok(true) => true,
            Ok(false) => {
                debug!(
                    "⏭️ Extension hook [{}] skipped: condition not met",
                    template_label(&entry.command)
                );
                false
            }
            Err(e) => {
                warn!(
                    "⚠️ Extension hook [{}] skipped: {}",
                    template_label(&entry.command),
                    e
                );
                false
            }
        }
    }

    /// コマンドの失敗を、エージェントへ返す文面にする。
    ///
    /// プログラムが無いことを確かめられた失敗の通知は、同じセッション・同じプログラムにつき
    /// 1 回にする。既に知らせていれば `None` を返し、ログにだけ残す。抑止するのは文面だけで、
    /// コマンドの起動は毎回試みる（セッションの途中でインストールされれば、そのまま動き出す）。
    ///
    /// 記録を残せないとき（セッション ID が無い・状態の置き場所が使えない）は、毎回知らせる。
    /// 知らせないまま黙るより、同じ通知が重なるほうを選ぶ。
    fn failure_message(
        &self,
        failure: &CommandFailure,
        run: &mut RunContext<'_>,
    ) -> Option<String> {
        let message = failure.agent_message();
        let CommandFailure::NotStarted {
            label,
            program,
            cause,
            ..
        } = failure
        else {
            return Some(message);
        };
        if !cause.is_missing_command() {
            return Some(message);
        }

        // 相対パスで指定したプログラムは、作業ディレクトリによって指す先が変わるので
        // キーに作業ディレクトリを含める。PATH から探す名前と絶対パスには含めない
        let cwd_for_relative =
            if HookCondition::is_explicit_path(program) && !Path::new(program).is_absolute() {
                run.cwd.to_string_lossy()
            } else {
                Cow::Borrowed("")
            };
        let key = StateStore::key(&[
            MISSING_COMMAND_KEY_VERSION,
            self.agent.id,
            run.session_id.unwrap_or(""),
            program,
            &cwd_for_relative,
        ]);
        // 同じ呼び出しの中で既に扱ったプログラムは、記録を見るまでもなく返さない
        if !run.missing_notices.insert(key.clone()) {
            return None;
        }
        if run.session_id.is_none() {
            return Some(message);
        }
        match self
            .store
            .mark_once(NOTICE_KIND, &key, MISSING_COMMAND_NOTICE_MAX_AGE)
        {
            MarkOutcome::First => Some(format!("{}{}", message, NOT_REPEATED_SUFFIX)),
            MarkOutcome::AlreadyMarked => {
                info!(
                    "🔕 Extension hook [{}] not started: missing-command notice already given in this session",
                    label
                );
                None
            }
            MarkOutcome::Unavailable => Some(message),
        }
    }

    /// NanoBuddy へ拡張子を通知する。通知するのは拡張子なので、拡張子のないファイル
    /// （`"*"` だけが当たる）では送らない。
    fn notify_nano_buddy(&self, file_path: &str) {
        if self.nano_buddy
            && let Some(ext) = Self::extract_ext(file_path)
        {
            debug!("🐱 NanoBuddy ext notification: .{}", ext);
            crate::notify::nano_buddy::notify_extension_hook(&ext);
        }
    }
}

impl Filter for ExtensionHookFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        // 拡張子フックは保存後のイベントにのみ適用する。
        // 保存前に formatter/linter を実行すると、未保存内容ではなく旧内容を検査してしまう。
        if input.event != HookEvent::AfterFileEdit {
            return false;
        }

        // NotebookEdit を含めるのは `.ipynb` の保存後フックを発火させるため。
        if !matches!(
            input.tool_name.as_str(),
            "Write" | "Edit" | "MultiEdit" | "NotebookEdit"
        ) {
            return false;
        }

        // 当てるコマンドがあるファイルを含むか確認
        match &input.tool_input {
            ToolInput::File(file_input) => {
                !self.matching_commands(&file_input.file_path).is_empty()
            }
            ToolInput::Files(file_inputs) => file_inputs
                .iter()
                .any(|file_input| !self.matching_commands(&file_input.file_path).is_empty()),
            _ => false,
        }
    }

    fn execute(&self, input: &HookInput) -> Decision {
        // ファイルパスを抽出してコマンドを実行
        let file_inputs: Vec<&FileOperationInput> = match &input.tool_input {
            ToolInput::File(file_input) => vec![file_input],
            ToolInput::Files(file_inputs) => file_inputs.iter().collect(),
            _ => Vec::new(),
        };

        let mut run = RunContext::new(input);
        let mut outputs = Vec::new();
        for file_input in file_inputs {
            let commands = self.matching_commands(&file_input.file_path);
            if commands.is_empty() {
                continue;
            }

            // コマンドを実行して出力を収集。
            // NanoBuddy 通知は、このファイルで実際に動かすコマンドがあるときだけ、
            // フックコマンドより先に到達するよう最初の起動の直前に送る
            let (_all_success, output) =
                self.execute_commands(&commands, &file_input.file_path, &mut run, || {
                    self.notify_nano_buddy(&file_input.file_path)
                });

            if let Some(ctx) = output {
                outputs.push(ctx);
            }
        }

        // 出力がある場合は追加コンテキスト付きの Allow を返す
        // lint 警告/エラーをエージェントに渡す（Claude Code のみ）
        // コマンドを動かせなかった知らせは先頭に、正規化せずに置く（切り詰めで消えないように。
        // `RunContext::notices` 参照）。コマンドの出力はトークン効率のため正規化する
        // （ANSI 除去、空行圧縮）
        let mut sections = run.notices;
        let output = normalize_lint_output(&outputs.join("\n"));
        if !output.is_empty() {
            sections.push(output);
        }
        if !sections.is_empty() {
            return Decision::allow_with_context(sections.join("\n"));
        }

        // 常に許可 — 拡張子フックは副作用であり、フィルターではない
        Decision::allow()
    }

    fn priority(&self) -> u32 {
        super::priority::EXTENSION // 低優先度 — 他のフィルターの後に実行
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn create_filter_with_go_hooks() -> ExtensionHookFilter {
        let mut hooks = BTreeMap::new();
        hooks.insert(".go".to_string(), vec!["gofmt -w {file}".to_string()]);
        ExtensionHookFilter::new(hooks, false, 60)
    }

    fn create_empty_filter() -> ExtensionHookFilter {
        ExtensionHookFilter::new(BTreeMap::<String, Vec<String>>::new(), false, 60)
    }

    // applies_to のテスト

    #[test]
    fn test_does_not_apply_to_before_command_with_write() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.go".to_string(),
                content: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_applies_to_after_file_edit_with_write() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.go".to_string(),
                content: None,
            }),
            session_id: None,
        };

        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_applies_to_edit_tool() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Edit".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.go".to_string(),
                content: None,
            }),
            session_id: None,
        };

        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_applies_to_multi_edit_tool() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "MultiEdit".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.go".to_string(),
                content: None,
            }),
            session_id: None,
        };

        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_applies_to_multi_file_edit_when_any_file_matches() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "MultiEdit".to_string(),
            tool_input: ToolInput::Files(vec![
                crate::domain::FileOperationInput {
                    file_path: "/path/to/file.rs".to_string(),
                    content: None,
                },
                crate::domain::FileOperationInput {
                    file_path: "/path/to/file.go".to_string(),
                    content: None,
                },
            ]),
            session_id: None,
        };

        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_stop_event() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput::default()),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_passthrough_event() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::Passthrough,
            tool_name: "UserPrompt".to_string(),
            tool_input: ToolInput::Stop(crate::domain::StopInput::default()),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_read_tool() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Read".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.go".to_string(),
                content: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_bash_tool() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "ls".to_string(),
                timeout: None,
                cwd: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_non_matching_extension() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.rs".to_string(), // .rs は設定対象外
                content: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_when_no_hooks_configured() {
        let filter = create_empty_filter();

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.go".to_string(),
                content: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    // execute のテスト

    #[test]
    fn test_execute_returns_allow() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/path/to/file.go".to_string(),
                content: None,
            }),
            session_id: None,
        };

        let decision = filter.execute(&input);
        // 拡張子フックは副作用であり、常に Allow を返す
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_execute_multi_file_edit_combines_matching_outputs() {
        let mut hooks = BTreeMap::new();
        hooks.insert(".txt".to_string(), vec!["echo {file}".to_string()]);
        let filter = ExtensionHookFilter::new(hooks, false, 60);

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "MultiEdit".to_string(),
            tool_input: ToolInput::Files(vec![
                crate::domain::FileOperationInput {
                    file_path: "/tmp/a.txt".to_string(),
                    content: None,
                },
                crate::domain::FileOperationInput {
                    file_path: "/tmp/b.rs".to_string(),
                    content: None,
                },
                crate::domain::FileOperationInput {
                    file_path: "/tmp/c.txt".to_string(),
                    content: None,
                },
            ]),
            session_id: None,
        };

        let decision = filter.execute(&input);
        if let Decision::Allow {
            additional_context: Some(context),
            ..
        } = decision
        {
            assert!(context.contains("/tmp/a.txt"));
            assert!(context.contains("/tmp/c.txt"));
            assert!(!context.contains("/tmp/b.rs"));
        } else {
            panic!("マッチした複数ファイルの出力を additional_context にまとめるべき");
        }
    }

    #[test]
    fn test_priority() {
        let filter = create_filter_with_go_hooks();
        assert_eq!(filter.priority(), 100);
    }

    // === validate_file_path のテスト ===

    #[test]
    fn test_validate_file_path_rejects_path_traversal() {
        assert!(ExtensionHookFilter::validate_file_path("../secret.txt").is_err());
        assert!(ExtensionHookFilter::validate_file_path("/tmp/../etc/passwd").is_err());
    }

    #[test]
    fn test_validate_file_path_accepts_double_dot_in_filename() {
        assert!(ExtensionHookFilter::validate_file_path("src/foo..bar.rs").is_ok());
        assert!(ExtensionHookFilter::validate_file_path("/tmp/a..b.txt").is_ok());
    }

    #[test]
    fn test_validate_file_path_rejects_dash_prefix() {
        assert!(ExtensionHookFilter::validate_file_path("-rf").is_err());
        assert!(ExtensionHookFilter::validate_file_path("--help").is_err());
    }

    #[test]
    fn test_validate_file_path_rejects_dangerous_chars() {
        assert!(ExtensionHookFilter::validate_file_path("bad;rm -rf /").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file`id`").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file$HOME").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file|pipe").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file&bg").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file>out").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file<input").is_err());
    }

    #[test]
    fn test_validate_file_path_rejects_windows_cmd_meta_chars() {
        // Windows の `cmd /c` 経由でファイルパスが渡されると、`%VAR%` が環境変数展開、
        // `!VAR!` が遅延展開、`^` がエスケープ、`"` がクォート切替として扱われ、
        // パス中にこれらが含まれると追加コマンドが注入される可能性がある。
        assert!(ExtensionHookFilter::validate_file_path("file%X%.rs").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file!X!.rs").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file^calc.rs").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file\"injected.rs").is_err());
    }

    #[test]
    fn test_validate_file_path_rejects_newline_and_null() {
        assert!(ExtensionHookFilter::validate_file_path("file\nname.rs").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file\rname.rs").is_err());
        assert!(ExtensionHookFilter::validate_file_path("file\0name.rs").is_err());
    }

    #[test]
    fn test_validate_file_path_rejects_tab() {
        // タブ文字は IFS による単語分割で引数 bleed を起こすため拒否する
        assert!(ExtensionHookFilter::validate_file_path("file\tname.rs").is_err());
        assert!(ExtensionHookFilter::validate_file_path("/tmp/foo\tbar.rs").is_err());
    }

    #[test]
    fn test_validate_file_path_accepts_safe_paths() {
        assert!(ExtensionHookFilter::validate_file_path("/path/to/file.go").is_ok());
        assert!(ExtensionHookFilter::validate_file_path("relative/path.rs").is_ok());
        assert!(ExtensionHookFilter::validate_file_path("file with spaces.txt").is_ok());
    }

    // === parse_command_template のテスト ===

    #[test]
    fn test_parse_command_template_basic() {
        let parsed = ExtensionHookFilter::parse_command_template("gofmt -w {file}").unwrap();
        assert_eq!(parsed.program, "gofmt");
        assert_eq!(parsed.args_before, vec!["-w"]);
        assert!(parsed.args_after.is_empty());
        assert!(parsed.inline_template.is_none());
    }

    #[test]
    fn test_parse_command_template_inline_placeholder() {
        let parsed =
            ExtensionHookFilter::parse_command_template("tool --flag --file={file} --opt").unwrap();
        assert_eq!(parsed.program, "tool");
        assert_eq!(parsed.args_before, vec!["--flag"]);
        assert_eq!(parsed.args_after, vec!["--opt"]);
        assert_eq!(parsed.inline_template.as_deref(), Some("--file={file}"));
    }

    #[test]
    fn test_parse_command_template_missing_placeholder_is_error() {
        assert!(ExtensionHookFilter::parse_command_template("gofmt -w").is_err());
        assert!(ExtensionHookFilter::parse_command_template("rustfmt").is_err());
    }

    #[test]
    fn test_parse_command_template_multiple_placeholders_is_error() {
        assert!(ExtensionHookFilter::parse_command_template("tool {file} {file}").is_err());
        assert!(ExtensionHookFilter::parse_command_template("tool --in={file}:{file}").is_err());
    }

    #[test]
    fn test_parse_command_template_placeholder_as_program_is_error() {
        assert!(ExtensionHookFilter::parse_command_template("{file} --flag").is_err());
    }

    #[test]
    fn test_parse_command_template_empty_is_error() {
        assert!(ExtensionHookFilter::parse_command_template("").is_err());
        assert!(ExtensionHookFilter::parse_command_template("   ").is_err());
    }

    // === タイムアウトのテスト ===

    #[test]
    fn test_extension_hook_timeout_returns_allow_with_error_context() {
        // 拡張フックは常に許可するが、タイムアウトエラーはコンテキストに表示されるべき
        let mut hooks = BTreeMap::new();
        hooks.insert(
            ".txt".to_string(),
            vec!["sh -c 'sleep 30 #ignore {file}'".to_string()],
        );
        let filter = ExtensionHookFilter::new(hooks, false, 2);

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };

        let start = std::time::Instant::now();
        let decision = filter.execute(&input);
        let elapsed = start.elapsed();

        // タイムアウトしても Allow のままにする
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "拡張子フックはタイムアウト時も Allow のままにする"
        );
        assert!(
            elapsed.as_secs() < 5,
            "おおむね 2 秒でタイムアウトすべきだが {:?} かかった",
            elapsed
        );
        // エラーコンテキストに失敗が記載されるべき
        match decision {
            Decision::Allow {
                additional_context, ..
            } => {
                let ctx = additional_context.expect("タイムアウト時はエラーコンテキストが付くべき");
                assert!(
                    ctx.contains("timed out") || ctx.contains("ERROR"),
                    "コンテキストにタイムアウトが示されるべき: {}",
                    ctx
                );
                assert!(
                    !ctx.contains("/tmp/test.txt"),
                    "タイムアウト理由のコマンドラベルにファイルパスを含めるべきではない: {}",
                    ctx
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_extension_hook_failure_context_sanitizes_command_label() {
        let mut hooks = BTreeMap::new();
        hooks.insert(
            ".txt".to_string(),
            vec!["sh -c 'printf failed >&2; exit 1 #ignore {file}'".to_string()],
        );
        let filter = ExtensionHookFilter::new(hooks, false, 60);

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/secret-path.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };

        let decision = filter.execute(&input);
        match decision {
            Decision::Allow {
                additional_context, ..
            } => {
                let ctx = additional_context.expect("失敗時はエラーコンテキストが付くべき");
                assert!(ctx.contains("failed"), "stderr は保持されるべき: {}", ctx);
                assert!(
                    ctx.starts_with("[sh]"),
                    "コマンドラベルはプログラム名だけにすべき: {}",
                    ctx
                );
                assert!(
                    !ctx.contains("/tmp/secret-path.txt"),
                    "コマンドラベルにファイルパスを含めるべきではない: {}",
                    ctx
                );
                assert!(
                    !ctx.contains("path_bytes=") && !ctx.contains("args_before="),
                    "エージェント向け出力にログ用の引数要約を含めるべきではない: {}",
                    ctx
                );
            }
            _ => panic!("Expected Allow decision"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_extension_hook_failure_context_hides_program_directory() {
        let mut hooks = BTreeMap::new();
        hooks.insert(
            ".txt".to_string(),
            vec!["/bin/sh -c 'printf failed >&2; exit 1 #ignore {file}'".to_string()],
        );
        let filter = ExtensionHookFilter::new(hooks, false, 60);

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };

        match filter.execute(&input) {
            Decision::Allow {
                additional_context: Some(context),
                ..
            } => {
                assert!(context.starts_with("[sh]"));
                assert!(
                    !context.contains("/bin/sh"),
                    "実行ファイルのディレクトリを表示すべきではない: {context}"
                );
            }
            other => panic!("失敗時は安全なラベルのコンテキストが付くべき: {other:?}"),
        }
    }

    #[test]
    fn test_extension_hook_failure_without_output_returns_context() {
        let mut hooks = BTreeMap::new();
        hooks.insert(
            ".txt".to_string(),
            vec!["sh -c 'exit 1 #ignore {file}'".to_string()],
        );
        let filter = ExtensionHookFilter::new(hooks, false, 60);

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };

        match filter.execute(&input) {
            Decision::Allow {
                additional_context: Some(context),
                ..
            } => {
                assert_eq!(context, "[sh] command failed without output");
            }
            other => panic!("無出力の失敗にもコンテキストが付くべき: {other:?}"),
        }
    }

    #[test]
    fn test_extension_hook_completes_within_timeout() {
        let mut hooks = BTreeMap::new();
        hooks.insert(
            ".txt".to_string(),
            vec!["sh -c 'echo ok #ignore {file}'".to_string()],
        );
        let filter = ExtensionHookFilter::new(hooks, false, 60);

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };

        let start = std::time::Instant::now();
        let decision = filter.execute(&input);
        let elapsed = start.elapsed();

        assert!(matches!(decision, Decision::Allow { .. }));
        assert!(
            elapsed.as_secs() < 5,
            "軽いフックは短時間で終わるべき: {:?}",
            elapsed
        );
    }

    // === 出力正規化のテスト ===

    #[test]
    fn test_execute_normalizes_output() {
        // "sh -c"とprintfを使用してANSIカラー付きのインデント出力を生成する
        let mut hooks = BTreeMap::new();
        hooks.insert(
            ".txt".to_string(),
            vec![
                "sh -c 'printf \"\\033[31m  error:\\033[0m bad\\n\\n\\n  detail\" #ignore {file}'"
                    .to_string(),
            ],
        );
        let filter = ExtensionHookFilter::new(hooks, false, 60);

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };

        let decision = filter.execute(&input);
        match decision {
            Decision::Allow {
                additional_context, ..
            } => {
                let ctx = additional_context.expect("出力コンテキストが付くべき");
                // ANSI コードが除去されるべき
                assert!(
                    !ctx.contains("\x1b"),
                    "ANSI コードが除去されるべき: {}",
                    ctx
                );
                // 先頭の空白が除去されるべき
                assert!(!ctx.contains("\n  "), "先頭空白が除去されるべき: {}", ctx);
                // 連続する空行が圧縮されるべき
                assert!(!ctx.contains("\n\n\n"), "連続空行が圧縮されるべき: {}", ctx);
            }
            _ => panic!("Expected Allow decision"),
        }
    }

    // === extract_ext のテスト ===

    #[test]
    fn test_execute_suppresses_noop_success_output() {
        let mut hooks = BTreeMap::new();
        hooks.insert(
            ".txt".to_string(),
            vec!["sh -c 'printf \"All checks passed!\" #ignore {file}'".to_string()],
        );
        let filter = ExtensionHookFilter::new(hooks, false, 60);
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };

        match filter.execute(&input) {
            Decision::Allow {
                additional_context: None,
                ..
            } => {}
            other => panic!("no-op 成功出力は抑制されるべき: {other:?}"),
        }
    }

    #[test]
    fn test_execute_suppresses_noop_success_output_with_ruleset_warnings() {
        // ruff の `check --select D...` は stdout に `All checks passed!` を出しつつ、
        // stderr へ毎回ルールセット非互換の警告を出す（exit 0）。生の結合文字列で
        // no-op 判定すると「no-op ではない」と誤判定し、その後の正規化で警告だけが
        // 消えるため `[ruff] All checks passed!` が編集のたびに返ってしまっていた。
        let mut hooks = BTreeMap::new();
        hooks.insert(
            ".txt".to_string(),
            vec![
                "sh -c 'printf \"All checks passed!\\n\"; printf \"warning: D203 and D211 are incompatible. Ignoring D203.\\n\" >&2 #ignore {file}'"
                    .to_string(),
            ],
        );
        let filter = ExtensionHookFilter::new(hooks, false, 60);
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };

        match filter.execute(&input) {
            Decision::Allow {
                additional_context: None,
                ..
            } => {}
            other => panic!("非互換警告付きの no-op 成功出力も抑制されるべき: {other:?}"),
        }
    }

    // コマンドは sh の引用 (printf の引数の中の `|`) に頼る。Windows は cmd /c を経由するので、
    // cmd が `|` をパイプとして読み、sh まで届かない
    #[cfg(unix)]
    #[test]
    fn test_execute_suppresses_output_that_normalizes_to_empty() {
        // 正規化で全て落ちる出力（枠線のみ等）は空の追加コンテキストを返さない。
        let mut hooks = BTreeMap::new();
        hooks.insert(
            ".txt".to_string(),
            vec!["sh -c 'printf \"   |\\n  ^^^\\n\" #ignore {file}'".to_string()],
        );
        let filter = ExtensionHookFilter::new(hooks, false, 60);
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };

        match filter.execute(&input) {
            Decision::Allow {
                additional_context: None,
                ..
            } => {}
            other => panic!("正規化で空になる出力は返さないべき: {other:?}"),
        }
    }

    #[test]
    fn test_execute_preserves_noop_text_when_command_fails() {
        let mut hooks = BTreeMap::new();
        hooks.insert(
            ".txt".to_string(),
            vec!["sh -c 'printf \"All checks passed!\"; exit 1 #ignore {file}'".to_string()],
        );
        let filter = ExtensionHookFilter::new(hooks, false, 60);
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };

        match filter.execute(&input) {
            Decision::Allow {
                additional_context: Some(context),
                ..
            } => {
                assert!(context.contains("All checks passed!"));
                assert!(context.starts_with("[sh]"));
            }
            other => panic!("失敗出力が保持されるべき: {other:?}"),
        }
    }

    #[test]
    fn test_extract_ext_simple() {
        assert_eq!(
            ExtensionHookFilter::extract_ext("main.rs"),
            Some("rs".to_string())
        );
    }

    #[test]
    fn test_extract_ext_multiple_dots() {
        assert_eq!(
            ExtensionHookFilter::extract_ext("file.test.spec.ts"),
            Some("ts".to_string())
        );
    }

    #[test]
    fn test_extract_ext_hidden_file() {
        // .gitignore → 拡張子なし（stem が空）
        assert_eq!(ExtensionHookFilter::extract_ext(".gitignore"), None);
    }

    #[test]
    fn test_extract_ext_no_extension() {
        assert_eq!(ExtensionHookFilter::extract_ext("Makefile"), None);
    }

    #[test]
    fn test_extract_ext_trailing_dot() {
        assert_eq!(
            ExtensionHookFilter::extract_ext("file."),
            Some("".to_string())
        );
    }

    #[test]
    fn test_extract_ext_path_with_dirs() {
        assert_eq!(
            ExtensionHookFilter::extract_ext("/usr/src/lib.rs"),
            Some("rs".to_string())
        );
    }

    // === matching_commands のテスト ===

    /// ファイルパスに当てるコマンドのテンプレートを、実行する順に返す。
    fn command_templates<'a>(filter: &'a ExtensionHookFilter, file_path: &str) -> Vec<&'a str> {
        filter
            .matching_commands(file_path)
            .into_iter()
            .map(|entry| entry.command.as_str())
            .collect()
    }

    #[test]
    fn test_matching_commands_match() {
        let filter = create_filter_with_go_hooks();
        assert_eq!(
            command_templates(&filter, "/tmp/file.go"),
            vec!["gofmt -w {file}"]
        );
    }

    #[test]
    fn test_matching_commands_no_match() {
        let filter = create_filter_with_go_hooks();
        assert!(filter.matching_commands("/tmp/file.rs").is_empty());
    }

    #[test]
    fn test_matching_commands_hidden_file() {
        let filter = create_filter_with_go_hooks();
        assert!(filter.matching_commands(".gitignore").is_empty());
    }

    // === catch-all（キー "*"）のテスト ===

    /// `"*"` と拡張子のキーを持つフィルターを作る。
    fn create_filter_with_catch_all(
        extension_hooks: &[(&str, &[&str])],
        catch_all: &[&str],
    ) -> ExtensionHookFilter {
        let mut hooks: BTreeMap<String, Vec<String>> = extension_hooks
            .iter()
            .map(|(key, commands)| {
                (
                    key.to_string(),
                    commands.iter().map(|c| c.to_string()).collect(),
                )
            })
            .collect();
        hooks.insert(
            ExtensionHookFilter::CATCH_ALL_KEY.to_string(),
            catch_all.iter().map(|c| c.to_string()).collect(),
        );
        ExtensionHookFilter::new(hooks, false, 60)
    }

    fn after_file_edit(file_path: &str) -> HookInput {
        HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: file_path.to_string(),
                content: None,
            }),
            session_id: None,
        }
    }

    fn context_of(decision: Decision) -> String {
        match decision {
            Decision::Allow {
                additional_context: Some(context),
                ..
            } => context,
            other => panic!("出力を additional_context に載せた Allow を返すべき: {other:?}"),
        }
    }

    #[test]
    fn test_catch_all_applies_to_extensionless_files_and_dotfiles() {
        // 拡張子で引けないファイルにも "*" は当たる
        let filter = create_filter_with_catch_all(&[], &["lint {file}"]);
        for path in [
            "Makefile",
            "/repo/Dockerfile",
            ".gitignore",
            "/repo/.env",
            "/repo/src/main.rs",
        ] {
            assert!(
                filter.applies_to(&after_file_edit(path)),
                "{path} にも catch-all が当たるべき"
            );
            assert_eq!(command_templates(&filter, path), vec!["lint {file}"]);
        }
    }

    #[test]
    fn test_extensionless_file_does_not_apply_without_catch_all() {
        let filter = create_filter_with_go_hooks();
        assert!(!filter.applies_to(&after_file_edit("/repo/Makefile")));
    }

    #[test]
    fn test_extension_commands_come_before_catch_all() {
        let filter = create_filter_with_catch_all(
            &[(".rs", &["rustfmt {file}", "clippy-driver {file}"])],
            &["noslop hook file {file}", "typos {file}"],
        );
        assert_eq!(
            command_templates(&filter, "/repo/src/lib.rs"),
            vec![
                "rustfmt {file}",
                "clippy-driver {file}",
                "noslop hook file {file}",
                "typos {file}",
            ]
        );
        // 拡張子のキーに当たらないファイルは catch-all だけ
        assert_eq!(
            command_templates(&filter, "/repo/README.md"),
            vec!["noslop hook file {file}", "typos {file}"]
        );
    }

    #[test]
    fn test_trailing_dot_matches_dot_key_then_catch_all() {
        // "file." の拡張子は空文字列なので、キー "." に当たり、その後に "*" が続く
        let filter = create_filter_with_catch_all(&[(".", &["dot {file}"])], &["all {file}"]);
        assert_eq!(
            command_templates(&filter, "/tmp/file."),
            vec!["dot {file}", "all {file}"]
        );
    }

    #[test]
    fn test_extension_matching_remains_case_sensitive() {
        // 拡張子の照合は大文字小文字を区別する。"*" は大文字の拡張子にも当たる
        let filter = create_filter_with_catch_all(&[(".rs", &["rustfmt {file}"])], &["all {file}"]);
        assert_eq!(
            command_templates(&filter, "/repo/MAIN.RS"),
            vec!["all {file}"]
        );
    }

    // formatter の書き換え (`>` によるファイルへの書き込み) を sh に任せる。Windows は cmd /c を
    // 経由するので、cmd が `>` をリダイレクトとして読み、sh まで届かない
    #[cfg(unix)]
    #[test]
    fn test_catch_all_sees_the_file_after_the_extension_commands_rewrote_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("sample.txt");
        std::fs::write(&target, "original").unwrap();
        let filter = create_filter_with_catch_all(
            &[(".txt", &["sh -c 'printf formatted > \"$0\"' {file}"])],
            &["sh -c 'printf \"seen: \"; cat \"$0\"' {file}"],
        );

        let context = context_of(filter.execute(&after_file_edit(target.to_str().unwrap())));

        assert!(
            context.contains("seen: formatted"),
            "catch-all は拡張子のキーのコマンドが書き換えた後の内容を受け取るべき: {context}"
        );
    }

    #[test]
    fn test_execute_runs_catch_all_after_extension_commands() {
        let filter =
            create_filter_with_catch_all(&[(".txt", &["echo EXT {file}"])], &["echo ALL {file}"]);

        let context = context_of(filter.execute(&after_file_edit("/tmp/a.txt")));

        let ext = context.find("EXT /tmp/a.txt").expect(&context);
        let all = context.find("ALL /tmp/a.txt").expect(&context);
        assert!(
            ext < all,
            "拡張子のキーのコマンドを先に動かすべき: {context}"
        );
    }

    #[test]
    fn test_duplicate_command_runs_once_for_each_key() {
        // 同じコマンドを拡張子のキーと "*" の両方に書いた場合は、書いたとおり 2 回動かす
        // （重複は除かない。`claw-hooks check` が警告で知らせる）
        let filter =
            create_filter_with_catch_all(&[(".txt", &["echo DUP {file}"])], &["echo DUP {file}"]);

        let context = context_of(filter.execute(&after_file_edit("/tmp/a.txt")));

        assert_eq!(
            context.matches("DUP /tmp/a.txt").count(),
            2,
            "拡張子のキーと \"*\" の両方で 1 回ずつ動くべき: {context}"
        );
    }

    #[test]
    fn test_execute_multi_file_edit_runs_matching_commands_per_file() {
        let filter =
            create_filter_with_catch_all(&[(".txt", &["echo EXT {file}"])], &["echo ALL {file}"]);
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "MultiEdit".to_string(),
            tool_input: ToolInput::Files(
                ["/tmp/a.txt", "/tmp/Makefile", "/tmp/b.rs"]
                    .into_iter()
                    .map(|path| crate::domain::FileOperationInput {
                        file_path: path.to_string(),
                        content: None,
                    })
                    .collect(),
            ),
            session_id: None,
        };

        let context = context_of(filter.execute(&input));

        let positions: Vec<usize> = [
            "EXT /tmp/a.txt",
            "ALL /tmp/a.txt",
            "ALL /tmp/Makefile",
            "ALL /tmp/b.rs",
        ]
        .iter()
        .map(|needle| context.find(needle).expect(&context))
        .collect();
        assert!(
            positions.is_sorted(),
            "ファイルごとに「拡張子のキー → \"*\"」の順で動かすべき: {context}"
        );
        assert!(
            !context.contains("EXT /tmp/Makefile") && !context.contains("EXT /tmp/b.rs"),
            "拡張子のキーは一致したファイルにだけ当てるべき: {context}"
        );
    }

    #[test]
    fn test_invalid_path_reports_one_error_and_runs_no_commands() {
        // パスの検証に落ちたファイルでは、どのコマンドも起動せず、エラーを 1 件だけ返す。
        // コマンドごとに返すと、"*" でコマンドが増えるぶん同じ行が並ぶ
        let filter = create_filter_with_catch_all(
            &[(".txt", &["echo RAN {file}", "echo RAN {file}"])],
            &["echo RAN {file}"],
        );

        let context = context_of(filter.execute(&after_file_edit("/tmp/100%.txt")));

        assert_eq!(
            context, "[ERROR] Path contains dangerous character: '%'",
            "エラーはファイルにつき 1 件だけ返すべき"
        );
    }

    #[test]
    fn test_invalid_path_does_not_stop_other_files() {
        // 検証に落ちたファイルがあっても、同じ編集のほかのファイルのコマンドは動かす
        let filter = create_filter_with_catch_all(&[], &["echo ALL {file}"]);
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "MultiEdit".to_string(),
            tool_input: ToolInput::Files(
                ["/tmp/bad;name.txt", "/tmp/good.txt"]
                    .into_iter()
                    .map(|path| crate::domain::FileOperationInput {
                        file_path: path.to_string(),
                        content: None,
                    })
                    .collect(),
            ),
            session_id: None,
        };

        let context = context_of(filter.execute(&input));

        assert_eq!(context.matches("[ERROR]").count(), 1, "{context}");
        assert!(context.contains("ALL /tmp/good.txt"), "{context}");
    }

    // === パストラバーサル検証の追加テスト ===

    #[test]
    fn test_validate_file_path_complex_traversal() {
        // 複雑なパストラバーサルパターンを正しく検出する
        assert!(ExtensionHookFilter::validate_file_path("/a/b/../../etc/passwd").is_err());
        assert!(ExtensionHookFilter::validate_file_path("./../../secret").is_err());
        assert!(ExtensionHookFilter::validate_file_path("src/../../../etc/hosts").is_err());
    }

    #[test]
    fn test_validate_file_path_dot_dot_in_filename_is_ok() {
        // ".." がディレクトリコンポーネントではなくファイル名の一部の場合は許可
        // Path::components() は "..test" を Normal("..test") として扱う
        assert!(ExtensionHookFilter::validate_file_path("..test.go").is_ok());
    }

    #[test]
    fn test_validate_file_path_null_byte() {
        assert!(ExtensionHookFilter::validate_file_path("file\0.go").is_err());
    }

    #[test]
    fn test_validate_file_path_pipe() {
        assert!(ExtensionHookFilter::validate_file_path("file|cat.go").is_err());
    }

    #[test]
    fn test_validate_file_path_ampersand() {
        assert!(ExtensionHookFilter::validate_file_path("file&rm.go").is_err());
    }

    #[test]
    fn test_validate_file_path_semicolon() {
        assert!(ExtensionHookFilter::validate_file_path("file;rm.go").is_err());
    }

    #[test]
    fn test_validate_file_path_backtick() {
        assert!(ExtensionHookFilter::validate_file_path("file`id`.go").is_err());
    }

    #[test]
    fn test_validate_file_path_dollar_sign() {
        assert!(ExtensionHookFilter::validate_file_path("file$(id).go").is_err());
    }

    #[test]
    fn test_validate_file_path_newline() {
        assert!(ExtensionHookFilter::validate_file_path("file\n.go").is_err());
    }

    #[test]
    fn test_validate_file_path_valid_paths() {
        // 正常なパスが許可されること
        assert!(ExtensionHookFilter::validate_file_path("/src/main.go").is_ok());
        assert!(ExtensionHookFilter::validate_file_path("src/main.go").is_ok());
        assert!(ExtensionHookFilter::validate_file_path("file with spaces.go").is_ok());
        assert!(ExtensionHookFilter::validate_file_path("/Users/dev/project/日本語.go").is_ok());
    }

    #[test]
    fn test_validate_file_path_dash_prefix() {
        // '-' で始まるパスはフラグと解釈される可能性があるため拒否
        assert!(ExtensionHookFilter::validate_file_path("-file.go").is_err());
        assert!(ExtensionHookFilter::validate_file_path("--help.go").is_err());
    }

    // === parse_command_template 追加テスト ===

    #[test]
    fn test_parse_command_template_file_as_executable() {
        // {file} がプログラム名として使用された場合はエラー
        let result = ExtensionHookFilter::parse_command_template("{file}");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_command_template_empty() {
        let result = ExtensionHookFilter::parse_command_template("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_command_template_inline_config_placeholder() {
        // --config={file} のようなインラインプレースホルダー
        let result = ExtensionHookFilter::parse_command_template("tool --config={file}");
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(parsed.program, "tool");
        assert!(parsed.inline_template.is_some());
        assert_eq!(parsed.inline_template.as_deref(), Some("--config={file}"));
    }

    #[test]
    fn test_parse_command_template_no_placeholder_is_error() {
        // {file} が含まれないテンプレートはエラー
        let result = ExtensionHookFilter::parse_command_template("echo hello");
        assert!(result.is_err());
    }

    // === 起動できなかったコマンドの文面のテスト ===

    /// PATH に無いプログラム名（見つからないコマンドのテストで使う）。
    const MISSING_TOOL: &str = "claw-hooks-test-missing-tool";

    fn test_agent() -> AgentProfile {
        AgentProfile {
            id: "claude-code",
            pre_command_context: true,
            stop_retry: true,
        }
    }

    /// `.txt` に `commands` を当て、見つからないコマンドの通知の記録を `store` に置くフィルターを作る。
    fn create_filter_with_state<C: Into<ExtensionHookCommand>>(
        commands: Vec<C>,
        store: StateStore,
    ) -> ExtensionHookFilter {
        let hooks = BTreeMap::from([(".txt".to_string(), commands)]);
        ExtensionHookFilter::new(hooks, false, 60).with_state(test_agent(), store)
    }

    /// セッション ID 付きの保存後イベントを作る。
    fn after_file_edit_in_session(file_path: &str, session_id: Option<&str>) -> HookInput {
        HookInput {
            session_id: session_id.map(str::to_string),
            ..after_file_edit(file_path)
        }
    }

    /// 追加コンテキストを返す（無ければ None）。
    fn optional_context_of(decision: Decision) -> Option<String> {
        match decision {
            Decision::Allow {
                additional_context, ..
            } => additional_context,
            other => panic!("拡張子フックは常に Allow を返すべき: {other:?}"),
        }
    }

    /// PATH に無いプログラムの通知の文面（後ろの一文なし）。
    fn missing_tool_notice() -> String {
        format!("[{MISSING_TOOL}] not started: command not found in PATH")
    }

    /// 条件付きのコマンドを作る。
    fn conditional(command: &str, condition: HookCondition) -> ExtensionHookCommand {
        ExtensionHookCommand {
            command: command.to_string(),
            condition: Some(condition),
        }
    }

    /// `path` に `contents` を書き、`mode`（`chmod` の 8 進表記）の権限にする。
    ///
    /// ファイルは子プロセス（`sh`）に書かせる。このテストプロセスが書き込み用に開くと、並列の
    /// テストがその間に起動した子プロセスが fd の写しを exec まで持ち、Linux ではこのファイルの
    /// exec が ETXTBSY（Text file busy）で失敗することがある。
    #[cfg(unix)]
    fn write_file_via_child(path: &Path, contents: &str, mode: &str) {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("printf '%s' \"$1\" > \"$0\" && chmod \"$2\" \"$0\"")
            .arg(path)
            .arg(contents)
            .arg(mode);
        let status = spawn_serialized(&mut cmd).unwrap().wait().unwrap();
        assert!(status.success(), "テスト用のファイルを書けなかった");
    }

    #[test]
    fn test_failure_messages_are_fixed_sentences_with_the_label() {
        let not_started = |cause| {
            CommandFailure::NotStarted {
                label: "tool".to_string(),
                program: "/opt/private/tool".to_string(),
                cause,
                kind: io::ErrorKind::NotFound,
                os_error: Some(2),
            }
            .agent_message()
        };
        assert_eq!(
            not_started(StartFailure::NotFoundInPath),
            "[tool] not started: command not found in PATH"
        );
        assert_eq!(
            not_started(StartFailure::NotFoundAtPath),
            "[tool] not started: command not found at the configured path"
        );
        assert_eq!(
            not_started(StartFailure::ExecutableOrInterpreterNotFound),
            "[tool] not started: executable or required interpreter not found"
        );
        assert_eq!(
            not_started(StartFailure::PermissionDenied),
            "[tool] not started: permission denied"
        );
        // その他の起動の失敗は ErrorKind の表示をそのまま使う
        assert_eq!(
            not_started(StartFailure::Other(io::ErrorKind::NotADirectory)),
            format!("[tool] not started: {}", io::ErrorKind::NotADirectory)
        );
        assert_eq!(
            CommandFailure::WaitFailed {
                label: "tool".to_string()
            }
            .agent_message(),
            "[tool] execution result unavailable: failed to wait for the process"
        );
        // パスの検証・テンプレートのパースのエラーは従来どおりの文面
        assert_eq!(
            CommandFailure::Rejected("Empty command template".to_string()).agent_message(),
            "[ERROR] Empty command template"
        );
    }

    #[test]
    fn test_only_confirmed_missing_programs_are_missing_commands() {
        assert!(StartFailure::NotFoundInPath.is_missing_command());
        assert!(StartFailure::NotFoundAtPath.is_missing_command());
        assert!(!StartFailure::ExecutableOrInterpreterNotFound.is_missing_command());
        assert!(!StartFailure::PermissionDenied.is_missing_command());
        assert!(!StartFailure::Other(io::ErrorKind::NotFound).is_missing_command());
    }

    #[test]
    fn test_classify_not_found_confirms_that_the_program_is_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(
            StartFailure::classify_not_found(MISSING_TOOL),
            StartFailure::NotFoundInPath
        );
        let missing = dir.path().join("missing-tool");
        assert_eq!(
            StartFailure::classify_not_found(missing.to_str().unwrap()),
            StartFailure::NotFoundAtPath
        );
        // ファイルはあるのに NotFound になったなら、無いのはインタープリター等
        let present = dir.path().join("present-tool");
        std::fs::write(&present, "").unwrap();
        assert_eq!(
            StartFailure::classify_not_found(present.to_str().unwrap()),
            StartFailure::ExecutableOrInterpreterNotFound
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_classify_not_found_for_program_in_path_blames_the_interpreter() {
        // PATH に実行ファイルがあるのに NotFound になったなら、プログラムが無いとは言わない
        assert_eq!(
            StartFailure::classify_not_found("sh"),
            StartFailure::ExecutableOrInterpreterNotFound
        );
        // Unix の exec は `\` を区切り文字と見なさず PATH から探すので、PATH の文面にする
        assert_eq!(
            StartFailure::classify_not_found("claw-hooks-test\\missing"),
            StartFailure::NotFoundInPath
        );
    }

    #[test]
    fn test_classify_maps_error_kinds() {
        assert_eq!(
            StartFailure::classify("tool", io::ErrorKind::PermissionDenied),
            StartFailure::PermissionDenied
        );
        assert_eq!(
            StartFailure::classify("tool", io::ErrorKind::NotADirectory),
            StartFailure::Other(io::ErrorKind::NotADirectory)
        );
        // Windows は cmd を起動するので、NotFound を設定のプログラムの有無で言い分けない
        let expected_not_found = if cfg!(target_os = "windows") {
            StartFailure::Other(io::ErrorKind::NotFound)
        } else {
            StartFailure::NotFoundInPath
        };
        assert_eq!(
            StartFailure::classify(MISSING_TOOL, io::ErrorKind::NotFound),
            expected_not_found
        );
    }

    // 以下、プログラムが無い・起動できないケースは Unix に限る。Windows は `cmd /c` を
    // 起動するので、プログラムが無くても起動自体は成功する（cmd が 9009 で終わり、通常の
    // 失敗として cmd の出力が返る）。

    #[cfg(unix)]
    #[test]
    fn test_missing_program_is_reported_with_its_label() {
        let filter = create_filter_with_state(
            vec![format!("{MISSING_TOOL} run {{file}}")],
            StateStore::disabled(),
        );

        let context = context_of(filter.execute(&after_file_edit("/tmp/a.txt")));

        assert_eq!(context, missing_tool_notice());
    }

    #[cfg(unix)]
    #[test]
    fn test_missing_program_is_distinguished_from_hooks_that_ran() {
        // 依頼の再現: formatter は動き、linter だけが無い。動いたフックの出力と、
        // 見つからないフックの名前がそれぞれ区別できること
        let filter = create_filter_with_state(
            vec![
                "sh -c 'echo FORMATTER-RAN' {file}".to_string(),
                format!("{MISSING_TOOL} run {{file}}"),
            ],
            StateStore::disabled(),
        );

        let context = context_of(filter.execute(&after_file_edit("/tmp/a.txt")));

        // 動かせなかったコマンドの知らせは、コマンドの出力より先に置く
        let notice = missing_tool_notice();
        assert_eq!(
            context.lines().collect::<Vec<_>>(),
            [notice.as_str(), "[sh] FORMATTER-RAN"],
            "{context}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_start_failure_notice_survives_truncation_after_long_output() {
        // アダプターは追加コンテキストを先頭から切り詰める。見つからない通知は返した時点で
        // 「知らせた」と記録するので、長い lint 出力の後ろに置くと、切り落とされたまま
        // そのセッションでは二度と届かなくなる。通知を先頭に置き、切り詰め後も残ること
        let state = tempfile::TempDir::new().unwrap();
        let filter = create_filter_with_state(
            vec![
                "sh -c 'seq 1 500 | sed s/^/finding-/' {file}".to_string(),
                format!("{MISSING_TOOL} run {{file}}"),
            ],
            StateStore::at(state.path()),
        );

        let context = context_of(
            filter.execute(&after_file_edit_in_session("/tmp/a.txt", Some("session-1"))),
        );

        let notice = format!("{}{}", missing_tool_notice(), NOT_REPEATED_SUFFIX);
        assert!(context.len() > 1000, "前提: 出力が切り詰めの長さを超える");
        assert!(
            crate::domain::truncate_output(&context, 1000).starts_with(&notice),
            "{context}"
        );
        assert!(context.contains("[sh] finding-1\n"), "出力も続けて返す");
    }

    #[cfg(unix)]
    #[test]
    fn test_missing_command_notice_is_given_once_per_session() {
        let state = tempfile::TempDir::new().unwrap();
        let filter = create_filter_with_state(
            vec![format!("{MISSING_TOOL} run {{file}}")],
            StateStore::at(state.path()),
        );

        let first = filter.execute(&after_file_edit_in_session("/tmp/a.txt", Some("session-1")));
        assert_eq!(
            context_of(first),
            format!(
                "[{MISSING_TOOL}] not started: command not found in PATH. This notice is not repeated in this session."
            )
        );

        let second = filter.execute(&after_file_edit_in_session("/tmp/b.txt", Some("session-1")));
        assert_eq!(
            optional_context_of(second),
            None,
            "同じセッションの 2 回目の編集では知らせない"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_missing_command_notice_returns_in_another_session() {
        let state = tempfile::TempDir::new().unwrap();
        let filter = create_filter_with_state(
            vec![format!("{MISSING_TOOL} run {{file}}")],
            StateStore::at(state.path()),
        );
        let expected = format!("{}{}", missing_tool_notice(), NOT_REPEATED_SUFFIX);

        for session_id in ["session-1", "session-2"] {
            let context = context_of(
                filter.execute(&after_file_edit_in_session("/tmp/a.txt", Some(session_id))),
            );
            assert_eq!(context, expected, "{session_id} では 1 回目なので知らせる");
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_missing_command_notice_is_scoped_by_agent() {
        // セッション ID はエージェントごとの採番なので、別のエージェントの同じ ID とは分ける
        let state = tempfile::TempDir::new().unwrap();
        let hooks = BTreeMap::from([(
            ".txt".to_string(),
            vec![format!("{MISSING_TOOL} run {{file}}")],
        )]);
        let codex_agent = AgentProfile {
            id: "codex",
            ..test_agent()
        };
        let input = after_file_edit_in_session("/tmp/a.txt", Some("session-1"));

        for agent in [test_agent(), codex_agent] {
            let filter = ExtensionHookFilter::new(hooks.clone(), false, 60)
                .with_state(agent, StateStore::at(state.path()));
            let context = context_of(filter.execute(&input));
            assert!(
                context.ends_with(NOT_REPEATED_SUFFIX),
                "{}: {context}",
                agent.id
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_missing_command_notice_repeats_without_session_id() {
        let state = tempfile::TempDir::new().unwrap();
        let filter = create_filter_with_state(
            vec![format!("{MISSING_TOOL} run {{file}}")],
            StateStore::at(state.path()),
        );

        // セッション ID が無い（空白だけも無しと扱う）と記録を残せないので、毎回知らせる。
        // 次も知らせるので、後ろの一文は付けない
        for session_id in [None, None, Some("  "), Some("  ")] {
            let context =
                context_of(filter.execute(&after_file_edit_in_session("/tmp/a.txt", session_id)));
            assert_eq!(context, missing_tool_notice(), "session_id={session_id:?}");
        }
        assert!(
            !state.path().join(NOTICE_KIND).exists(),
            "セッションを識別できない通知は記録しない"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_missing_command_notice_repeats_when_state_is_unavailable() {
        // 状態の置き場所が使えない（無効・ディレクトリを作れない）場合は記録を残せないので、
        // セッション ID があっても毎回知らせる
        let dir = tempfile::TempDir::new().unwrap();
        let blocked = dir.path().join("state");
        std::fs::write(&blocked, "").unwrap();

        for store in [StateStore::disabled(), StateStore::at(&blocked)] {
            let filter =
                create_filter_with_state(vec![format!("{MISSING_TOOL} run {{file}}")], store);
            for _ in 0..2 {
                let context = context_of(
                    filter.execute(&after_file_edit_in_session("/tmp/a.txt", Some("session-1"))),
                );
                assert_eq!(context, missing_tool_notice());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_missing_command_notice_is_given_once_within_one_edit() {
        // 拡張子のキーと "*" の両方に書いたプログラムが、複数ファイルの編集でファイルごとに
        // 失敗しても、1 回の編集で返す通知は 1 回にする（記録を残せない場合も同じ）
        let hooks = BTreeMap::from([
            (".txt".to_string(), vec![format!("{MISSING_TOOL} {{file}}")]),
            (
                ExtensionHookFilter::CATCH_ALL_KEY.to_string(),
                vec![format!("{MISSING_TOOL} {{file}}")],
            ),
        ]);
        let multi_file_edit = |session_id: Option<&str>| HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "MultiEdit".to_string(),
            tool_input: ToolInput::Files(
                ["/tmp/a.txt", "/tmp/b.txt"]
                    .into_iter()
                    .map(|path| crate::domain::FileOperationInput {
                        file_path: path.to_string(),
                        content: None,
                    })
                    .collect(),
            ),
            session_id: session_id.map(str::to_string),
        };
        let state = tempfile::TempDir::new().unwrap();

        for (store, session_id) in [
            (StateStore::at(state.path()), Some("session-1")),
            (StateStore::disabled(), Some("session-1")),
            (StateStore::disabled(), None),
        ] {
            let filter =
                ExtensionHookFilter::new(hooks.clone(), false, 60).with_state(test_agent(), store);
            let context = context_of(filter.execute(&multi_file_edit(session_id)));
            assert_eq!(
                context.matches("not started").count(),
                1,
                "session_id={session_id:?}: {context}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_missing_program_at_configured_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let program = dir.path().join("not-installed").join("tool");
        let filter = create_filter_with_state(
            vec![
                format!("'{}' {{file}}", program.display()),
                "./claw-hooks-test-not-installed/relative-tool {file}".to_string(),
            ],
            StateStore::disabled(),
        );

        let context = context_of(filter.execute(&after_file_edit("/tmp/a.txt")));

        assert_eq!(
            context.lines().collect::<Vec<_>>(),
            [
                "[tool] not started: command not found at the configured path",
                "[relative-tool] not started: command not found at the configured path",
            ],
            "{context}"
        );
        assert!(
            !context.contains("not-installed"),
            "プログラムのディレクトリは文面に出さない: {context}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_missing_interpreter_is_not_reported_as_missing_command() {
        // shebang のインタープリターが無いスクリプトも spawn は NotFound になるが、
        // プログラム自体はあるので「見つからない」とは言わない。直すまで毎回起きるので、
        // 同じセッションでも毎回返し、後ろの一文も付けない
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("needs-interpreter");
        write_file_via_child(&script, "#!/nonexistent/interpreter\n", "755");
        let state = tempfile::TempDir::new().unwrap();
        let filter = create_filter_with_state(
            vec![format!("'{}' {{file}}", script.display())],
            StateStore::at(state.path()),
        );

        for _ in 0..2 {
            let context = context_of(
                filter.execute(&after_file_edit_in_session("/tmp/a.txt", Some("session-1"))),
            );
            assert_eq!(
                context,
                "[needs-interpreter] not started: executable or required interpreter not found"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_permission_denied_is_reported_every_time() {
        let dir = tempfile::TempDir::new().unwrap();
        let program = dir.path().join("not-executable");
        write_file_via_child(&program, "#!/bin/sh\n", "644");
        let state = tempfile::TempDir::new().unwrap();
        let filter = create_filter_with_state(
            vec![format!("'{}' {{file}}", program.display())],
            StateStore::at(state.path()),
        );

        for _ in 0..2 {
            let context = context_of(
                filter.execute(&after_file_edit_in_session("/tmp/a.txt", Some("session-1"))),
            );
            assert_eq!(context, "[not-executable] not started: permission denied");
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_start_failure_label_does_not_carry_control_characters() {
        // 設定値に紛れた ESC が、ラベルの位置から色付きの文面を偽装できないようにする
        let filter = create_filter_with_state(
            vec!["claw-hooks-test\u{1b}[31m-missing {file}".to_string()],
            StateStore::disabled(),
        );

        let context = context_of(filter.execute(&after_file_edit("/tmp/a.txt")));

        assert!(!context.contains('\u{1b}'), "{context:?}");
        assert!(
            context.starts_with("[claw-hooks-test?[31m-missing] not started"),
            "{context:?}"
        );
    }

    #[test]
    fn test_relative_program_notice_is_keyed_by_working_directory() {
        // 相対パスのプログラムは作業ディレクトリで指す先が変わるので、別のディレクトリでは
        // もう一度知らせる。絶対パスのプログラムは作業ディレクトリに依らない
        let state = tempfile::TempDir::new().unwrap();
        let filter = create_filter_with_state(
            Vec::<ExtensionHookCommand>::new(),
            StateStore::at(state.path()),
        );
        let input = after_file_edit_in_session("/tmp/a.txt", Some("session-1"));
        let absolute = std::env::temp_dir()
            .join("claw-hooks-test-absolute")
            .join("tool");
        let notified = |program: &str, cwd: &str| {
            let failure = CommandFailure::NotStarted {
                label: "tool".to_string(),
                program: program.to_string(),
                cause: StartFailure::NotFoundAtPath,
                kind: io::ErrorKind::NotFound,
                os_error: Some(2),
            };
            let mut run = RunContext::new(&input);
            run.cwd = PathBuf::from(cwd);
            filter.failure_message(&failure, &mut run).is_some()
        };

        assert!(notified("./bin/tool", "/repo/a"));
        assert!(!notified("./bin/tool", "/repo/a"));
        assert!(
            notified("./bin/tool", "/repo/b"),
            "別のディレクトリの相対パス"
        );

        let absolute = absolute.to_str().unwrap();
        assert!(notified(absolute, "/repo/a"));
        assert!(
            !notified(absolute, "/repo/b"),
            "絶対パスは作業ディレクトリに依らない"
        );
        // 同じ名前でも別のディレクトリのプログラムは別に知らせる
        let other_dir = std::env::temp_dir()
            .join("claw-hooks-test-other")
            .join("tool");
        assert!(notified(other_dir.to_str().unwrap(), "/repo/a"));
    }

    // === 条件付きエントリのテスト ===

    #[test]
    fn test_conditional_entry_is_skipped_when_command_exists_does_not_hold() {
        let filter = create_filter_with_state(
            vec![
                conditional(
                    "echo SKIPPED {file}",
                    HookCondition {
                        command_exists: Some(MISSING_TOOL.to_string()),
                        ..Default::default()
                    },
                ),
                ExtensionHookCommand::from("echo RAN {file}"),
            ],
            StateStore::disabled(),
        );

        let context = context_of(filter.execute(&after_file_edit("/tmp/a.txt")));

        assert!(
            context.contains("RAN"),
            "条件の無いコマンドは動く: {context}"
        );
        assert!(
            !context.contains("SKIPPED"),
            "条件を満たさないコマンドは動かさない: {context}"
        );
    }

    #[test]
    fn test_conditional_entry_is_silent_when_every_command_is_skipped() {
        // 見つからないプログラムを条件で守ったエントリは、起動の失敗も通知も出さない
        let filter = create_filter_with_state(
            vec![conditional(
                &format!("{MISSING_TOOL} run {{file}}"),
                HookCondition {
                    command_exists: Some(MISSING_TOOL.to_string()),
                    ..Default::default()
                },
            )],
            StateStore::disabled(),
        );

        assert!(filter.applies_to(&after_file_edit("/tmp/a.txt")));
        assert_eq!(
            optional_context_of(filter.execute(&after_file_edit("/tmp/a.txt"))),
            None
        );
    }

    #[test]
    fn test_conditional_entry_runs_when_command_exists_holds() {
        let filter = create_filter_with_state(
            vec![conditional(
                "echo RAN {file}",
                HookCondition {
                    command_exists: Some("sh".to_string()),
                    ..Default::default()
                },
            )],
            StateStore::disabled(),
        );

        let context = context_of(filter.execute(&after_file_edit("/tmp/a.txt")));

        assert!(context.contains("RAN"), "条件を満たせば動く: {context}");
    }

    #[test]
    fn test_conditional_entry_is_skipped_when_condition_cannot_be_evaluated() {
        // NUL を含むパスは有無を確かめられない（`try_exists` が InvalidInput を返す）。
        // 評価できない条件のコマンドは動かさず、エージェントへも返さない（ログにだけ残す）
        let filter = create_filter_with_state(
            vec![
                conditional(
                    "echo SKIPPED {file}",
                    HookCondition {
                        file_exists: Some("bad\0name".to_string()),
                        ..Default::default()
                    },
                ),
                ExtensionHookCommand::from("echo RAN {file}"),
            ],
            StateStore::disabled(),
        );

        let context = context_of(filter.execute(&after_file_edit("/tmp/a.txt")));

        assert!(context.contains("RAN"), "{context}");
        assert!(!context.contains("SKIPPED"), "{context}");
        assert!(
            !context.contains("condition"),
            "条件の評価エラーはエージェントへ返さない: {context}"
        );
    }

    // 1 つ目のコマンドがファイルを作るのに sh の引用を使う。Windows は cmd /c を経由するので、
    // cmd が引用を解釈し、sh まで届かない
    #[cfg(unix)]
    #[test]
    fn test_condition_is_evaluated_right_before_its_command() {
        // 前のコマンドが作ったファイルを、後のコマンドの条件に使える
        let dir = tempfile::TempDir::new().unwrap();
        let marker = dir.path().join("created-by-formatter");
        let filter = create_filter_with_state(
            vec![
                ExtensionHookCommand::from(format!(
                    "sh -c ': > \"$1\"' {{file}} '{}'",
                    marker.display()
                )),
                conditional(
                    "sh -c 'echo SECOND-RAN' {file}",
                    HookCondition {
                        file_exists: Some(marker.to_string_lossy().into_owned()),
                        ..Default::default()
                    },
                ),
            ],
            StateStore::disabled(),
        );

        let context = context_of(filter.execute(&after_file_edit("/tmp/a.txt")));

        assert_eq!(context, "[sh] SECOND-RAN");
    }

    // === NanoBuddy の通知の時機のテスト ===

    /// `execute_commands` を 1 回動かし、`before_first_start` が呼ばれた回数を返す。
    fn count_before_first_start(filter: &ExtensionHookFilter, file_path: &str) -> usize {
        let input = after_file_edit(file_path);
        let commands = filter.matching_commands(file_path);
        let mut run = RunContext::new(&input);
        let mut calls = 0;
        filter.execute_commands(&commands, file_path, &mut run, || calls += 1);
        calls
    }

    #[test]
    fn test_nano_buddy_notice_needs_a_command_that_runs() {
        let skipped = || {
            conditional(
                "echo SKIPPED {file}",
                HookCondition {
                    command_exists: Some(MISSING_TOOL.to_string()),
                    ..Default::default()
                },
            )
        };

        // すべてのコマンドが条件で飛ばされたら送らない
        let all_skipped =
            create_filter_with_state(vec![skipped(), skipped()], StateStore::disabled());
        assert_eq!(count_before_first_start(&all_skipped, "/tmp/a.txt"), 0);

        // 動かすコマンドがあれば、コマンドの数によらず 1 回だけ送る
        let some_run = create_filter_with_state(
            vec![
                skipped(),
                ExtensionHookCommand::from("echo A {file}"),
                ExtensionHookCommand::from("echo B {file}"),
            ],
            StateStore::disabled(),
        );
        assert_eq!(count_before_first_start(&some_run, "/tmp/a.txt"), 1);

        // パスの検証に落ちたファイルでは、どのコマンドも動かさないので送らない
        assert_eq!(count_before_first_start(&some_run, "/tmp/100%.txt"), 0);
    }

    // コマンドがファイルを作るのに sh を使う（Windows は cmd /c を経由するので引用が届かない）
    #[cfg(unix)]
    #[test]
    fn test_nano_buddy_notice_is_sent_before_the_first_command_starts() {
        let dir = tempfile::TempDir::new().unwrap();
        let marker = dir.path().join("started");
        let filter = create_filter_with_state(
            vec![format!(
                "sh -c ': > \"$1\"' {{file}} '{}'",
                marker.display()
            )],
            StateStore::disabled(),
        );
        let input = after_file_edit("/tmp/a.txt");
        let commands = filter.matching_commands("/tmp/a.txt");
        let mut run = RunContext::new(&input);
        let mut started_before_notice = None;

        filter.execute_commands(&commands, "/tmp/a.txt", &mut run, || {
            started_before_notice = Some(marker.exists());
        });

        assert_eq!(
            started_before_notice,
            Some(false),
            "通知はコマンドを起動する前に送る"
        );
        assert!(marker.exists(), "コマンドは通知の後に動く");
    }
}
