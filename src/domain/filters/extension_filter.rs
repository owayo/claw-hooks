//! 拡張子ベースのフックフィルターの実装。

use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::process::{Command, Stdio};
use tracing::{debug, info, warn};

use super::Filter;
use crate::domain::command::{configure_process_group, run_with_timeout};
use crate::domain::normalize::normalize_lint_output;
use crate::domain::{Decision, FileOperationInput, HookEvent, HookInput, ToolInput};

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

/// 拡張子ベースのフックフィルター。
pub struct ExtensionHookFilter {
    /// 拡張子 → コマンドのマップ（例: ".go" → ["gofmt -w {file}", "golangci-lint run {file}"]）
    hooks: BTreeMap<String, Vec<String>>,
    nano_buddy: bool,
    timeout_secs: u64,
}

impl ExtensionHookFilter {
    /// 新しい ExtensionHookFilter を作成する。
    pub fn new(hooks: BTreeMap<String, Vec<String>>, nano_buddy: bool, timeout_secs: u64) -> Self {
        Self {
            hooks,
            nano_buddy,
            timeout_secs,
        }
    }

    /// ファイルパスから拡張子を抽出する（先頭のドットを含まない）。
    fn extract_ext(file_path: &str) -> Option<String> {
        Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_string())
    }

    /// ファイルパスにマッチするコマンドを取得する。
    fn get_matching_commands(&self, file_path: &str) -> Option<&Vec<String>> {
        let path = Path::new(file_path);
        let extension = path.extension()?.to_str()?;
        let ext_with_dot = format!(".{}", extension);

        self.hooks.get(&ext_with_dot)
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
    fn execute_command(
        &self,
        command_template: &str,
        file_path: &str,
    ) -> Result<CommandResult, String> {
        // ファイルパスの検証
        Self::validate_file_path(file_path)?;

        // コマンドテンプレートのパース
        let parsed = Self::parse_command_template(command_template)?;

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
            parsed.program,
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

        // 永続ログ・タイムアウト理由・エージェント返却用ラベルには
        // 展開済みファイルパスを含めない。
        let sanitized_command = format!(
            "{} args_before={} args_after={} inline={} path_bytes={}",
            parsed.program,
            parsed.args_before.len(),
            parsed.args_after.len(),
            parsed.inline_template.is_some(),
            safe_path.len()
        );

        let start = std::time::Instant::now();
        let child = cmd
            .spawn()
            .map_err(|e| format!("Failed to execute hook: {}", e))?;
        let result = run_with_timeout(child, self.timeout_secs, &sanitized_command);
        let elapsed = start.elapsed();
        // 完了ログには展開済みコマンド全文（ファイルパスを含む）を残さず、
        // プログラム名と所要時間のサマリのみを記録する（機密非永続化方針）。
        info!(
            "⏰️ Extension hook [{}] completed in {:.2}s",
            parsed.program,
            elapsed.as_secs_f64()
        );
        let output = result?;

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
            display_label: parsed.program,
            success: output.status.success(),
            output: combined_output,
        })
    }

    /// 拡張子に対応するすべてのコマンドを実行し、出力を収集する。
    /// 警告/エラーを出力したすべてのコマンドの結合出力を返す。
    fn execute_commands(&self, commands: &[String], file_path: &str) -> (bool, Option<String>) {
        let mut all_success = true;
        let mut outputs: Vec<String> = Vec::new();

        for cmd_template in commands {
            match self.execute_command(cmd_template, file_path) {
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
                    let noop_success =
                        result.success && crate::domain::is_noop_success_output(&result.output);
                    // 空でない出力を収集（警告、エラー、lint メッセージ）
                    if !result.output.is_empty() && !noop_success {
                        outputs.push(format!("[{}] {}", result.display_label, result.output));
                    }
                }
                Err(e) => {
                    all_success = false;
                    warn!("❌ Extension hook failed: {}", e);
                    outputs.push(format!("[ERROR] {}", e));
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
}

impl Filter for ExtensionHookFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        // 拡張子フックは保存後のイベントにのみ適用する。
        // 保存前に formatter/linter を実行すると、未保存内容ではなく旧内容を検査してしまう。
        if input.event != HookEvent::AfterFileEdit {
            return false;
        }

        if !matches!(input.tool_name.as_str(), "Write" | "Edit" | "MultiEdit") {
            return false;
        }

        // マッチする拡張子フックがあるか確認
        match &input.tool_input {
            ToolInput::File(file_input) => {
                self.get_matching_commands(&file_input.file_path).is_some()
            }
            ToolInput::Files(file_inputs) => file_inputs
                .iter()
                .any(|file_input| self.get_matching_commands(&file_input.file_path).is_some()),
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

        let mut outputs = Vec::new();
        for file_input in file_inputs {
            if let Some(commands) = self.get_matching_commands(&file_input.file_path) {
                // NanoBuddy 通知（フックコマンドより先に到達するよう先に送信）
                if self.nano_buddy {
                    if let Some(ext) = Self::extract_ext(&file_input.file_path) {
                        debug!("🐱 NanoBuddy ext notification: .{}", ext);
                        crate::notify::nano_buddy::notify_extension_hook(&ext);
                    }
                }

                // コマンドを実行して出力を収集
                let (_all_success, output) = self.execute_commands(commands, &file_input.file_path);

                // 出力がある場合は追加コンテキスト付きの Allow を返す
                // lint 警告/エラーをエージェントに渡す（Claude Code のみ）
                // トークン効率のため出力を正規化（ANSI 除去、空行圧縮）
                if let Some(ctx) = output {
                    outputs.push(ctx);
                }
            }
        }

        if !outputs.is_empty() {
            return Decision::allow_with_context(normalize_lint_output(&outputs.join("\n")));
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
        ExtensionHookFilter::new(BTreeMap::new(), false, 60)
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
            Decision::Allow { additional_context } => {
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
            Decision::Allow { additional_context } => {
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
            Decision::Allow { additional_context } => {
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
            } => {}
            other => panic!("no-op 成功出力は抑制されるべき: {other:?}"),
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

    // === get_matching_commands のテスト ===

    #[test]
    fn test_get_matching_commands_match() {
        let filter = create_filter_with_go_hooks();
        let cmds = filter.get_matching_commands("/tmp/file.go");
        assert!(cmds.is_some());
    }

    #[test]
    fn test_get_matching_commands_no_match() {
        let filter = create_filter_with_go_hooks();
        let cmds = filter.get_matching_commands("/tmp/file.rs");
        assert!(cmds.is_none());
    }

    #[test]
    fn test_get_matching_commands_hidden_file() {
        let filter = create_filter_with_go_hooks();
        let cmds = filter.get_matching_commands(".gitignore");
        assert!(cmds.is_none());
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
}
