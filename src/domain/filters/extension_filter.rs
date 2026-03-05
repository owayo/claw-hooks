//! 拡張子ベースのフックフィルターの実装。

use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::process::{Command, Stdio};
use tracing::{debug, info, warn};

use super::Filter;
use crate::domain::command::run_with_timeout;
use crate::domain::normalize::normalize_lint_output;
use crate::domain::{Decision, HookEvent, HookInput, ToolInput};

/// パース済みコマンドテンプレートの結果。
struct ParsedCommand {
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
    /// 実行されたコマンド
    command: String,
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
        const DANGEROUS_CHARS: &[char] = &['`', '$', '|', '&', ';', '<', '>', '\n', '\r', '\0'];
        for c in DANGEROUS_CHARS {
            if file_path.contains(*c) {
                return Err(format!("Path contains dangerous character: {:?}", c));
            }
        }

        Ok(())
    }

    /// コマンドテンプレートをパースして構造化された結果を返す。
    /// --file={file} のようなインラインパターンを含む {file} プレースホルダーを安全に処理する。
    fn parse_command_template(template: &str) -> Result<ParsedCommand, String> {
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

        debug!(
            "🪛 Executing extension hook: {} {:?} {} {:?} inline={:?}",
            parsed.program,
            parsed.args_before,
            safe_path,
            parsed.args_after,
            parsed.inline_template
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

        // ログ用に展開済みのコマンド文字列を構築
        let actual_command = {
            let mut parts = vec![parsed.program.clone()];
            parts.extend(parsed.args_before.iter().cloned());
            if let Some(ref template) = parsed.inline_template {
                parts.push(template.replace("{file}", &safe_path));
            } else {
                parts.push(safe_path.clone());
            }
            parts.extend(parsed.args_after.iter().cloned());
            parts.join(" ")
        };

        let start = std::time::Instant::now();
        let child = cmd
            .spawn()
            .map_err(|e| format!("Failed to execute hook: {}", e))?;
        let result = run_with_timeout(child, self.timeout_secs, &actual_command);
        let elapsed = start.elapsed();
        info!(
            "⏰️ Extension hook [{}] completed in {:.2}s",
            actual_command,
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
            let detail = [stderr.trim(), stdout.trim()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            if detail.is_empty() {
                warn!(
                    "⚠️ Extension hook command failed (exit code {}): {}",
                    exit_code, actual_command
                );
            } else {
                warn!(
                    "⚠️ Extension hook command failed (exit code {}): {}\n{}",
                    exit_code, actual_command, detail
                );
            }
        }

        Ok(CommandResult {
            command: actual_command,
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
                    }
                    // 空でない出力を収集（警告、エラー、lint メッセージ）
                    if !result.output.is_empty() {
                        outputs.push(format!("[{}] {}", result.command, result.output));
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
        // BeforeCommand/AfterFileEdit イベントで Write, Edit, MultiEdit に適用
        // Read 操作には適用しない
        if !matches!(
            input.event,
            HookEvent::BeforeCommand | HookEvent::AfterFileEdit
        ) {
            return false;
        }

        if !matches!(input.tool_name.as_str(), "Write" | "Edit" | "MultiEdit") {
            return false;
        }

        // マッチする拡張子フックがあるか確認
        if let ToolInput::File(file_input) = &input.tool_input {
            return self.get_matching_commands(&file_input.file_path).is_some();
        }

        false
    }

    fn execute(&self, input: &HookInput) -> Decision {
        // ファイルパスを抽出してコマンドを実行
        if let ToolInput::File(file_input) = &input.tool_input {
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
                    return Decision::allow_with_context(normalize_lint_output(&ctx));
                }
            }
        }

        // 常に許可 — 拡張子フックは副作用であり、フィルターではない
        Decision::allow()
    }

    fn priority(&self) -> u32 {
        100 // 低優先度 — 他のフィルターの後に実行
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

    // applies_to tests

    #[test]
    fn test_applies_to_before_command_with_write() {
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

        assert!(filter.applies_to(&input));
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
    fn test_does_not_apply_to_before_prompt_event() {
        let filter = create_filter_with_go_hooks();

        let input = HookInput {
            event: HookEvent::BeforePrompt,
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
                file_path: "/path/to/file.rs".to_string(), // .rs not in hooks
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

    // execute tests

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
        // Extension hooks always allow (they're side effects, not filters)
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_priority() {
        let filter = create_filter_with_go_hooks();
        assert_eq!(filter.priority(), 100);
    }

    // === validate_file_path tests ===

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
    fn test_validate_file_path_accepts_safe_paths() {
        assert!(ExtensionHookFilter::validate_file_path("/path/to/file.go").is_ok());
        assert!(ExtensionHookFilter::validate_file_path("relative/path.rs").is_ok());
        assert!(ExtensionHookFilter::validate_file_path("file with spaces.txt").is_ok());
    }

    // === parse_command_template tests ===

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

    // === Timeout tests ===

    #[test]
    fn test_extension_hook_timeout_returns_allow_with_error_context() {
        // Extension hooks always allow, but timeout error should appear in context
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

        // Should still allow (extension hooks are side effects, not blockers)
        assert!(
            matches!(decision, Decision::Allow { .. }),
            "Extension hooks always allow even on timeout"
        );
        assert!(
            elapsed.as_secs() < 5,
            "Should timeout in ~2s, took {:?}",
            elapsed
        );
        // Error context should mention the failure
        match decision {
            Decision::Allow { additional_context } => {
                let ctx = additional_context.expect("Should have error context on timeout");
                assert!(
                    ctx.contains("timed out") || ctx.contains("ERROR"),
                    "Context should indicate timeout: {}",
                    ctx
                );
            }
            _ => unreachable!(),
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
            "Fast hook should complete quickly: {:?}",
            elapsed
        );
    }

    // === Output normalization tests ===

    #[test]
    fn test_execute_normalizes_output() {
        // Use "sh -c" with printf to produce ANSI-colored output with indentation
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
                let ctx = additional_context.expect("Should have context");
                // ANSI codes should be stripped
                assert!(
                    !ctx.contains("\x1b"),
                    "ANSI codes should be stripped: {}",
                    ctx
                );
                // Leading whitespace should be stripped
                assert!(
                    !ctx.contains("\n  "),
                    "Leading whitespace should be stripped: {}",
                    ctx
                );
                // Consecutive blank lines should be collapsed
                assert!(
                    !ctx.contains("\n\n\n"),
                    "Blank lines should be collapsed: {}",
                    ctx
                );
            }
            _ => panic!("Expected Allow decision"),
        }
    }
}
