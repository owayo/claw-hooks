//! フックイベント処理サービス。

use std::io::{self, Read as _, Write};
use std::process;

use anyhow::Result;
use tracing::{debug, error, info};

use crate::cli::Format;
use crate::config::Config;
use crate::domain::{Decision, FilterChain, HookEvent, HookInput};
use crate::service::adapter::FormatAdapter;

/// フックイベント処理サービス。
pub struct HookService {
    config: Config,
    filter_chain: FilterChain,
    adapter: FormatAdapter,
    /// トレースモード: デバッグ用に生の入力を stderr に出力
    trace: bool,
}

impl HookService {
    /// 指定フォーマットで新しい HookService を作成する。
    pub fn new(config: Config, format: Format, trace: bool) -> Self {
        let filter_chain = FilterChain::new(&config);
        let adapter = FormatAdapter::new(format, config.output_max_length);
        Self {
            config,
            filter_chain,
            adapter,
            trace,
        }
    }

    /// フック処理ループを実行する。
    ///
    /// stdin から JSON 入力を読み取り、処理して stdout に JSON 出力を書き込む。
    /// 入出力フォーマットは設定されたエージェントフォーマットに依存する。
    pub fn run(&self) -> Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut stdout = stdout.lock();

        // stdin から全入力を読み取り（改行を保持して正確なJSONを維持）
        let mut input = String::new();
        stdin.lock().read_to_string(&mut input)?;

        // トレースモード: 生の入力を即座に stderr に出力
        if self.trace {
            eprintln!("🔍 [TRACE] Raw input received:");
            eprintln!("{}", input);
            eprintln!("🔍 [TRACE] End of input");
        }

        if input.is_empty() {
            if self.trace {
                eprintln!("🔍 [TRACE] ERROR: No input received from stdin");
            }
            error!("No input received from stdin");
            // セキュリティ: フェイルクローズ - 入力がない場合はブロック
            let output_json = self.adapter.format_error("No input received from stdin");
            self.write_error_output(&mut stdout, &output_json)?;
            process::exit(self.adapter.error_exit_code());
        }

        debug!("Received input: {}", input);

        // フォーマットアダプターで入力をパース
        let hook_input: HookInput = match self.adapter.parse_input(&input) {
            Ok(parsed) => {
                if self.trace {
                    eprintln!("🔍 [TRACE] Parsed input:");
                    eprintln!("  event: {:?}", parsed.event);
                    eprintln!("  tool_name: {}", parsed.tool_name);
                    eprintln!("  tool_input: {:?}", parsed.tool_input);
                    eprintln!("  session_id: {:?}", parsed.session_id);
                }
                parsed
            }
            Err(e) => {
                let error_msg = format!("Failed to parse input: {}", e);
                if self.trace {
                    eprintln!("🔍 [TRACE] Parse error: {}", error_msg);
                }
                error!("{}", error_msg);
                // 適切なフォーマットでエラーを出力
                // セキュリティ: フェイルクローズ終了コード（2 = block）
                let output_json = self.adapter.format_error(&error_msg);
                self.write_error_output(&mut stdout, &output_json)?;
                process::exit(self.adapter.error_exit_code());
            }
        };

        // フックを処理
        let decision = self.process(&hook_input);
        let exit_code = self.adapter.exit_code(&decision, hook_input.event);

        if self.trace {
            eprintln!("🔍 [TRACE] Decision: {:?}", decision);
            eprintln!("🔍 [TRACE] Exit code: {}", exit_code);
        }

        // フォーマットアダプターで出力を書き込み
        let output = self.adapter.format_output(&decision, hook_input.event)?;

        if self.trace {
            eprintln!("🔍 [TRACE] Output:");
            eprintln!("{}", output);
        }

        let emoji = if matches!(decision, crate::domain::Decision::Block { .. }) {
            "🚫"
        } else {
            "✅"
        };
        info!("Output {}: {}", emoji, output);

        // Windsurf Stop Block は stderr に出力（エージェントは exit 2 時に stderr を読む）
        if self.adapter.use_stderr(&decision, hook_input.event) {
            let stderr = io::stderr();
            let mut stderr = stderr.lock();
            writeln!(stderr, "{}", output)?;
            stderr.flush()?;
        } else {
            writeln!(stdout, "{}", output)?;
            stdout.flush()?; // パイプのためexit前にフラッシュ
        }

        // fire-and-forget スレッド（report=false の stop hooks）のログ出力完了を待つ。
        // Decision は既に出力済みなので、エージェント側の応答時間には影響しない。
        crate::domain::filters::drain_pending_handles();

        process::exit(exit_code);
    }

    /// フック入力を処理して判定を返す。
    pub fn process(&self, input: &HookInput) -> Decision {
        debug!(
            "Processing hook: event={:?}, tool_name={}",
            input.event, input.tool_name
        );

        match input.event {
            HookEvent::BeforeCommand => self.handle_before_command(input),
            HookEvent::AfterFileEdit => self.handle_after_file_edit(input),
            HookEvent::Stop => self.handle_stop(input),
            HookEvent::BeforePrompt => self.handle_before_prompt(input),
            HookEvent::SubagentStart | HookEvent::SubagentStop => self.handle_subagent(input),
        }
    }

    /// BeforeCommand イベントの処理（ツール使用前）。
    fn handle_before_command(&self, input: &HookInput) -> Decision {
        debug!("Handling BeforeCommand for tool: {}", input.tool_name);

        // フィルターチェーンを実行
        self.filter_chain.execute(input)
    }

    /// AfterFileEdit イベントの処理（ファイル操作後）。
    fn handle_after_file_edit(&self, input: &HookInput) -> Decision {
        if self.config.debug {
            debug!(
                "AfterFileEdit: tool_name={}, tool_input={:?}",
                input.tool_name, input.tool_input
            );
        }

        // Write/Edit/MultiEdit の場合、拡張子フック用にフィルターチェーンを実行
        // 対応エージェント:
        // - Claude Code: PostToolUse (Write)
        // - Cursor: afterFileEdit (AfterFileEdit + Write にマッピング)
        // - Windsurf: post_write_code (AfterFileEdit + Write にマッピング)
        if matches!(input.tool_name.as_str(), "Write" | "Edit" | "MultiEdit") {
            return self.filter_chain.execute(input);
        }

        // その他の AfterFileEdit イベントは常に許可
        Decision::allow()
    }

    /// Stop イベントの処理。
    fn handle_stop(&self, input: &HookInput) -> Decision {
        info!("Stop event received: session_id={:?}", input.session_id);

        // フィルターチェーン経由で Stop フックを実行
        self.filter_chain.execute(input)
    }

    /// BeforePrompt イベントの処理（Gemini CLI のみ）。
    fn handle_before_prompt(&self, _input: &HookInput) -> Decision {
        debug!("Handling BeforePrompt event");

        // BeforePrompt は現在パススルーイベント
        Decision::allow()
    }

    /// フェイルクローズ時のエラー出力を適切なストリームに書き込む。
    ///
    /// Windsurf はブロック時に exit code 2 + stderr からメッセージを読むため、
    /// フェイルクローズパスでも stderr に書く必要がある。
    fn write_error_output(&self, stdout: &mut io::StdoutLock, output_json: &str) -> Result<()> {
        if self.adapter.format_uses_stderr_for_errors() {
            let stderr = io::stderr();
            let mut stderr = stderr.lock();
            writeln!(stderr, "{}", output_json)?;
            stderr.flush()?;
        } else {
            writeln!(stdout, "{}", output_json)?;
            stdout.flush()?;
        }
        Ok(())
    }

    /// SubagentStart/SubagentStop イベントの処理。
    fn handle_subagent(&self, input: &HookInput) -> Decision {
        info!(
            "Subagent event received: {:?}, session_id={:?}",
            input.event, input.session_id
        );

        // フィルターチェーン経由で実行（SubagentFilter が NanoBuddy 通知を処理）
        self.filter_chain.execute(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::test_helpers::make_bash_input;
    use crate::domain::{FileOperationInput, StopInput, SubagentInput, ToolInput};

    fn make_service() -> HookService {
        let config = Config::default();
        HookService::new(config, Format::Claude, false)
    }

    #[test]
    fn test_process_allows_safe_command() {
        let service = make_service();
        let input = make_bash_input("ls -la");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_blocks_rm() {
        let service = make_service();
        let input = make_bash_input("rm -rf /tmp/foo");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_blocks_kill() {
        let service = make_service();
        let input = make_bash_input("kill -9 1234");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_blocks_dd() {
        let service = make_service();
        let input = make_bash_input("dd if=/dev/zero of=/dev/sda");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_after_file_edit_write_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: Some("content".to_string()),
            }),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_after_file_edit_read_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Read".to_string(),
            tool_input: ToolInput::File(FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_stop_event_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(StopInput::default()),
            session_id: Some("session-123".to_string()),
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_before_prompt_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::BeforePrompt,
            tool_name: "BeforePrompt".to_string(),
            tool_input: ToolInput::Other(serde_json::json!({})),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_subagent_start_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::SubagentStart,
            tool_name: "SubagentStart".to_string(),
            tool_input: ToolInput::Subagent(SubagentInput {
                subagent_type: Some("explore".to_string()),
                prompt: Some("Search the codebase".to_string()),
                status: None,
                duration: None,
            }),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_subagent_stop_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::SubagentStop,
            tool_name: "SubagentStop".to_string(),
            tool_input: ToolInput::Subagent(SubagentInput {
                subagent_type: Some("explore".to_string()),
                prompt: None,
                status: Some("completed".to_string()),
                duration: Some(5000),
            }),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_blocks_sudo_rm() {
        let service = make_service();
        let input = make_bash_input("sudo rm -rf /");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_blocks_piped_kill() {
        let service = make_service();
        let input = make_bash_input("ps aux | grep node | xargs kill");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_with_custom_filter() {
        let mut config = Config::default();
        config.custom_filters.push(crate::config::CustomFilter {
            command: "yarn".to_string(),
            args: vec![],
            message: "Use pnpm instead".to_string(),
        });
        let service = HookService::new(config, Format::Claude, false);

        let input = make_bash_input("yarn install");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_custom_filter_allows_non_matching() {
        let mut config = Config::default();
        config.custom_filters.push(crate::config::CustomFilter {
            command: "yarn".to_string(),
            args: vec![],
            message: "Use pnpm instead".to_string(),
        });
        let service = HookService::new(config, Format::Claude, false);

        let input = make_bash_input("pnpm install");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_with_disabled_rm_block() {
        let config = Config {
            rm_block: false,
            ..Config::default()
        };
        let service = HookService::new(config, Format::Claude, false);
        let input = make_bash_input("rm file.txt");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_after_file_edit_non_write_tool_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Grep".to_string(),
            tool_input: ToolInput::Other(serde_json::json!({"pattern": "test"})),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_after_file_edit_edit_tool_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Edit".to_string(),
            tool_input: ToolInput::File(FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: Some("new content".to_string()),
            }),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    // === フィルター無効化テスト ===

    #[test]
    fn test_process_with_disabled_kill_block() {
        let config = Config {
            kill_block: false,
            ..Config::default()
        };
        let service = HookService::new(config, Format::Claude, false);
        let input = make_bash_input("kill -9 1234");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_with_disabled_dd_block() {
        let config = Config {
            dd_block: false,
            ..Config::default()
        };
        let service = HookService::new(config, Format::Claude, false);
        let input = make_bash_input("dd if=/dev/zero of=/dev/sda");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_all_blocks_disabled() {
        let config = Config {
            rm_block: false,
            kill_block: false,
            dd_block: false,
            ..Config::default()
        };
        let service = HookService::new(config, Format::Claude, false);

        assert!(matches!(
            service.process(&make_bash_input("rm -rf /")),
            Decision::Allow { .. }
        ));
        assert!(matches!(
            service.process(&make_bash_input("kill 1234")),
            Decision::Allow { .. }
        ));
        assert!(matches!(
            service.process(&make_bash_input("dd if=/dev/zero of=/dev/sda")),
            Decision::Allow { .. }
        ));
    }

    // === ブロックメッセージ内容テスト ===

    #[test]
    fn test_process_rm_block_returns_non_empty_message() {
        let service = make_service();
        let input = make_bash_input("rm -rf /tmp/foo");
        let decision = service.process(&input);
        match decision {
            Decision::Block { message } => {
                assert!(
                    !message.is_empty(),
                    "rm ブロックメッセージは空であってはならない"
                );
                assert!(
                    message.contains("rm"),
                    "rm ブロックメッセージは rm に言及すべき: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }

    #[test]
    fn test_process_kill_block_returns_non_empty_message() {
        let service = make_service();
        let input = make_bash_input("kill -9 1234");
        let decision = service.process(&input);
        match decision {
            Decision::Block { message } => {
                assert!(
                    !message.is_empty(),
                    "kill ブロックメッセージは空であってはならない"
                );
                assert!(
                    message.contains("kill"),
                    "kill ブロックメッセージは kill に言及すべき: {}",
                    message
                );
            }
            _ => panic!("Expected Block decision"),
        }
    }

    // === カスタムフィルター引数モードテスト ===

    #[test]
    fn test_process_custom_filter_with_args_blocks_matching_arg() {
        let mut config = Config::default();
        config.custom_filters.push(crate::config::CustomFilter {
            command: "npm".to_string(),
            args: vec!["install".to_string(), "i".to_string()],
            message: "Use pnpm instead".to_string(),
        });
        let service = HookService::new(config, Format::Claude, false);

        assert!(matches!(
            service.process(&make_bash_input("npm install lodash")),
            Decision::Block { .. }
        ));
        assert!(matches!(
            service.process(&make_bash_input("npm i lodash")),
            Decision::Block { .. }
        ));
    }

    #[test]
    fn test_process_custom_filter_with_args_allows_non_matching_arg() {
        let mut config = Config::default();
        config.custom_filters.push(crate::config::CustomFilter {
            command: "npm".to_string(),
            args: vec!["install".to_string()],
            message: "Use pnpm instead".to_string(),
        });
        let service = HookService::new(config, Format::Claude, false);

        assert!(matches!(
            service.process(&make_bash_input("npm run build")),
            Decision::Allow { .. }
        ));
        assert!(matches!(
            service.process(&make_bash_input("npm test")),
            Decision::Allow { .. }
        ));
    }

    // === チェーンコマンド内の検出テスト ===

    #[test]
    fn test_process_blocks_rm_in_chained_command() {
        let service = make_service();
        let input = make_bash_input("cd /tmp && rm -rf foo");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_blocks_kill_in_semicolon_chain() {
        let service = make_service();
        let input = make_bash_input("echo done; killall node");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_blocks_dd_in_subshell() {
        let service = make_service();
        let input = make_bash_input("bash -c 'dd if=/dev/zero of=/dev/sda'");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    // === 非Bashツールのテスト ===

    #[test]
    fn test_process_before_command_non_bash_tool_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(FileOperationInput {
                file_path: "/tmp/rm.txt".to_string(),
                content: Some("rm content".to_string()),
            }),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_before_command_write_does_not_run_extension_hooks() {
        let config = Config {
            extension_hooks: std::collections::BTreeMap::from([(
                ".rs".to_string(),
                vec!["echo lint {file}".to_string()],
            )]),
            ..Config::default()
        };
        let service = HookService::new(config, Format::Claude, false);
        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(FileOperationInput {
                file_path: "/tmp/test.rs".to_string(),
                content: Some("fn main() {}".to_string()),
            }),
            session_id: None,
        };

        match service.process(&input) {
            Decision::Allow { additional_context } => {
                assert!(
                    additional_context.is_none(),
                    "保存前イベントでは拡張子フックを実行してはならない"
                );
            }
            _ => panic!("Expected Allow decision"),
        }
    }

    // === カスタムブロックメッセージテスト ===

    #[test]
    fn test_process_custom_rm_block_message() {
        let config = Config {
            rm_block_message: Some("カスタムrmブロック".to_string()),
            ..Config::default()
        };
        let service = HookService::new(config, Format::Claude, false);
        let input = make_bash_input("rm file.txt");
        match service.process(&input) {
            Decision::Block { message } => {
                assert_eq!(message, "カスタムrmブロック");
            }
            _ => panic!("Expected Block decision"),
        }
    }

    #[test]
    fn test_process_after_file_edit_multi_edit_tool_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "MultiEdit".to_string(),
            tool_input: ToolInput::File(FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }
}
