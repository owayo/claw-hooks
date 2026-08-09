//! フックイベント処理サービス。

use std::io::{self, Read as _, Write};

use anyhow::Result;
use tracing::{debug, error, info};

use crate::cli::Format;
use crate::config::Config;
use crate::domain::{Decision, FilterChain, HookEvent, HookInput};
use crate::service::adapter::FormatAdapter;
use crate::service::log_sanitizer::{summarize_hook_input, summarize_parsed_hook_input};

/// stdin から受け取るフック入力の最大バイト数。
///
/// クライアント（AI エージェント）からの単発 JSON 入力は通常 1〜数百 KB に収まる。
/// 上限を設けないと、暴走したエージェントや悪意ある呼び出しによって巨大入力で
/// claw-hooks プロセスを OOM kill させられる（パーサ側の `MAX_COMMAND_LEN` は
/// 読み込み後の防御で、ここまで来てから止めてもメモリは既に確保済み）。
/// 4 MiB は通常の hook ペイロードを十分カバーしつつ、メモリ圧迫を防ぐ目安。
const MAX_INPUT_BYTES: u64 = 4 * 1024 * 1024;

/// stdin を上限付きで読み取る。
///
/// 上限超過の判定を呼び出し側でできるよう、`MAX_INPUT_BYTES + 1` バイトまで読む
/// （読み取れたバイト数が上限を超えていれば過大入力）。
fn read_stdin_bounded(stdin: io::Stdin) -> Result<Vec<u8>> {
    let stdin_locked = stdin.lock();
    let mut raw = Vec::new();
    let mut limited = stdin_locked.take(MAX_INPUT_BYTES + 1);
    limited.read_to_end(&mut raw)?;
    Ok(raw)
}

/// フックイベント処理サービス。
pub struct HookService {
    config: Config,
    filter_chain: FilterChain,
    adapter: FormatAdapter,
    /// トレースモード: デバッグ用に生の入力を stderr に出力
    trace: bool,
}

impl HookService {
    /// 設定の読み込み・検証に失敗したときのフェイルクローズ応答を出力する。
    ///
    /// `main` が設定エラーを `?` で伝播すると exit 1 + stdout 空で終了するが、
    /// Codex / Antigravity は「フック失敗＝判定を無視して処理継続」と解釈するため、
    /// 設定ファイルの TOML タイポ 1 つで危険コマンドのブロックが全て無効化される
    /// （フェイルオープン）。`--format` は設定を読まずに分かるので、
    /// エージェント別の適切な拒否形式は設定なしでも組み立てられる。
    ///
    /// イベント名の判別のため stdin を読む。読めない場合は汎用形式へフォールバックする。
    /// Stop 系は `format_error_for_input` 側でイベント固有の停止許可に倒れるため、
    /// 設定エラーで継続ループに陥ることはない。
    ///
    /// 終了コードを返す（`main` 側でログガードを drop してから終了するため）。
    pub fn emit_config_error(format: Format, trace: bool, error: &anyhow::Error) -> i32 {
        // 設定内容そのもの（パスやフィルター定義）はエージェントへ返す本文に含めない。
        // 診断の詳細は stderr に出し、ユーザーが `claw-hooks check` で確認できるようにする。
        eprintln!("claw-hooks configuration error: {:#}", error);

        let adapter = FormatAdapter::new(format, 0);
        let message = "claw-hooks configuration is invalid. Run `claw-hooks check`.";

        // イベント別の拒否形式を選ぶため、生入力からイベント名だけを読み取る。
        // 上限超過の入力も捨てずに渡す。捨てると Stop を判別できず、設定エラー時に
        // Stop へブロック（= 継続プロンプト）を返して無限ループを招く。
        // 先頭は読めているため、イベント名の走査フォールバックで判別できる。
        let raw_input = read_stdin_bounded(io::stdin())
            .ok()
            .filter(|raw| !raw.is_empty())
            .map(|raw| String::from_utf8_lossy(&raw).into_owned());

        if trace {
            eprintln!("🔍 [TRACE] Config error fail-closed: {:#}", error);
        }

        let output = match raw_input.as_deref() {
            Some(input) => adapter.format_error_for_input(message, input),
            None => adapter.format_error(message),
        };

        let write_result = if adapter.format_uses_stderr_for_errors() {
            let stderr = io::stderr();
            let mut stderr = stderr.lock();
            writeln!(stderr, "{}", output).and_then(|()| stderr.flush())
        } else {
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            writeln!(stdout, "{}", output).and_then(|()| stdout.flush())
        };
        if let Err(e) = write_result {
            // 書き込み自体が失敗した場合は終了コードだけでブロックを表明する。
            eprintln!("claw-hooks failed to write fail-closed response: {}", e);
        }

        adapter.error_exit_code(raw_input.as_deref())
    }

    /// `run` の内部エラー（出力の書き込み失敗など）に対するフェイルクローズ応答を出力する。
    ///
    /// `emit_config_error` と同じ理由でエラーを `?` で伝播させられないが、
    /// この時点では stdin を読み切っているためイベント名を判別できない。
    /// そのため各フォーマットの汎用拒否形式で返す。
    pub fn emit_runtime_error(format: Format, trace: bool, error: &anyhow::Error) -> i32 {
        eprintln!("claw-hooks internal error: {:#}", error);
        if trace {
            eprintln!("🔍 [TRACE] Runtime error fail-closed: {:#}", error);
        }

        let adapter = FormatAdapter::new(format, 0);
        let output = adapter.format_error("claw-hooks encountered an internal error");

        let write_result = if adapter.format_uses_stderr_for_errors() {
            let stderr = io::stderr();
            let mut stderr = stderr.lock();
            writeln!(stderr, "{}", output).and_then(|()| stderr.flush())
        } else {
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            writeln!(stdout, "{}", output).and_then(|()| stdout.flush())
        };
        if let Err(e) = write_result {
            eprintln!("claw-hooks failed to write fail-closed response: {}", e);
        }

        adapter.error_exit_code(None)
    }

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

    /// `--event` によるイベント名の明示指定を設定する。
    pub fn with_event_override(mut self, event: Option<String>) -> Self {
        self.adapter = self.adapter.with_event_override(event);
        self
    }

    /// フック処理ループを実行する。
    ///
    /// stdin から JSON 入力を読み取り、処理して stdout に JSON 出力を書き込む。
    /// 入出力フォーマットは設定されたエージェントフォーマットに依存する。
    ///
    /// 終了コードを `Ok(i32)` で返す。プロセスを直接終了せず呼び出し側に委ねることで、
    /// 非同期ログ（tracing-appender）のフラッシュ用ガードを確実に drop してから
    /// 終了できるようにする（ガード未 drop だと終了直前のログが欠落する）。
    pub fn run(&self) -> Result<i32> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut stdout = stdout.lock();

        // stdin から全入力を読み取り（改行を保持して正確なJSONを維持）。
        // サイズ制限を設け、悪意ある/暴走エージェントによる OOM 攻撃を防ぐ。
        // 制限超過時はフェイルクローズ（ブロック）として扱う。
        // バイト列として読み取り、サイズ制限は生のバイト長で判定する。
        // `read_to_string` は不正な UTF-8 で `?` により即時エラー終了
        // （exit 1・stdout 空）に倒れるが、これは Codex/Antigravity では
        // 「フック失敗＝判定無視」と解釈されフェイルオープンになる
        // （危険コマンドのブロックが効かなくなる）。そのため一旦バイトで読み、
        // 損失あり変換でフェイルクローズ経路（パース失敗→ブロック、または
        // 不正バイトを置換文字に変換した上での危険コマンド検出）に確実に載せる。
        // stdin の I/O 失敗も `?` で伝播させない。伝播させると exit 1 + stdout 空になり、
        // Codex / Antigravity では「フック失敗＝判定を無視」でフェイルオープンする。
        let raw = match read_stdin_bounded(stdin) {
            Ok(raw) => raw,
            Err(e) => {
                let log_message = format!("Failed to read stdin: {}", e);
                return self.fail_closed(
                    &mut stdout,
                    &log_message,
                    "Failed to read hook input",
                    None,
                );
            }
        };
        // 不正な UTF-8 を含んでいても処理を継続できるよう損失あり変換する
        // （置換文字 U+FFFD に変換され、後続のパース/検出はフェイルクローズで動作する）。
        let input = String::from_utf8_lossy(&raw).into_owned();

        if raw.len() as u64 > MAX_INPUT_BYTES {
            // 入力過大でフェイルクローズ。ログには実バイト数と上限を残しつつ、
            // エージェントへ返す本文は短い定型文（"Input too large"）にする。
            //
            // 元入力（先頭が読めている分）を渡すのが重要。渡さないと
            // `blocks_would_loop_or_be_ignored` がイベント名を判別できず、Stop に対しても
            // ブロックを返してしまう。Stop のブロックは「停止させず reason を継続プロンプト
            // にする」意味なので、巨大な Stop ペイロードが来るたびに
            // ブロック → 継続 → Stop 再発火 → 同じ失敗、の無限ループになる。
            // `read_stdin_bounded` は上限 +1 バイトで打ち切るため JSON としては壊れているが、
            // イベント名は先頭付近にあるため走査フォールバックで判別できる。
            let log_message = format!(
                "Input exceeds limit: {} bytes (max {})",
                raw.len(),
                MAX_INPUT_BYTES
            );
            return self.fail_closed(&mut stdout, &log_message, "Input too large", Some(&input));
        }

        // トレースモード: 生の入力を即座に stderr に出力
        if self.trace {
            eprintln!("🔍 [TRACE] Raw input received:");
            eprintln!("{}", input);
            eprintln!("🔍 [TRACE] End of input");
        }

        if input.is_empty() {
            // セキュリティ: フェイルクローズ - 入力がない場合はブロック
            return self.fail_closed(
                &mut stdout,
                "No input received from stdin",
                "No input received from stdin",
                None,
            );
        }

        debug!("Received input: {}", summarize_hook_input(&input));

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
                // セキュリティ: フェイルクローズ終了コード（2 = block）。
                // パース失敗経路は元入力からイベント名を判定し、エージェント別の
                // 適切な deny フォーマット（format_error_for_input）で返す。
                let error_msg = format!("Failed to parse input: {}", e);
                return self.fail_closed(&mut stdout, &error_msg, &error_msg, Some(&input));
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
        // 永続ログには結果の種別とサイズのみ記録する。出力本文には lint/format の
        // 診断（ソース行を含み得る）や reason が入るため、機密非永続化（Debug Log
        // Safety）の方針に従って本文は残さない。全文が必要なときは `--trace`
        // （stderr へ出力、ディスク非永続）を使う。
        info!("Output {} ({} bytes)", emoji, output.len());

        // Windsurf は pre_run_command / post_write_code のブロック時に stderr を使う
        // （exit 2 のエラーメッセージは stderr から読まれるのが公式仕様）。
        // post_cascade_response は事後フックのため、Stop の失敗も stdout 側の許可応答に丸める。
        if self.adapter.use_stderr(&decision, hook_input.event) {
            let stderr = io::stderr();
            let mut stderr = stderr.lock();
            writeln!(stderr, "{}", output)?;
            stderr.flush()?;
        } else {
            writeln!(stdout, "{}", output)?;
            stdout.flush()?; // パイプのためexit前にフラッシュ
        }

        Ok(exit_code)
    }

    /// フック入力を処理して判定を返す。
    pub fn process(&self, input: &HookInput) -> Decision {
        debug!(
            "Processing hook: event={:?}, tool_name={}",
            input.event, input.tool_name
        );

        match input.event {
            HookEvent::BeforeCommand | HookEvent::PermissionRequest => {
                self.handle_before_command(input)
            }
            HookEvent::AfterFileEdit => self.handle_after_file_edit(input),
            HookEvent::Stop => self.handle_stop(input),
            HookEvent::Passthrough => self.handle_passthrough(input),
            HookEvent::SubagentStart | HookEvent::SubagentStop => self.handle_subagent(input),
        }
    }

    /// BeforeCommand/PermissionRequest イベントの処理（ツール使用前/承認前）。
    fn handle_before_command(&self, input: &HookInput) -> Decision {
        debug!("Handling BeforeCommand for tool: {}", input.tool_name);

        // フィルターチェーンを実行
        self.filter_chain.execute(input)
    }

    /// AfterFileEdit イベントの処理（ファイル操作後）。
    fn handle_after_file_edit(&self, input: &HookInput) -> Decision {
        if self.config.debug {
            debug!("AfterFileEdit: {}", summarize_parsed_hook_input(input));
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

    /// Passthrough イベントの処理。
    ///
    /// Passthrough は claw-hooks が対応しない/スコープ外のイベント
    /// （SessionStart / UserPromptSubmit や各エージェント固有の未対応イベント等）を
    /// 集約するパススルー用のマーカー。claw-hooks は意図的に
    /// コマンドブロック・保存後フック・Stop フック・サブエージェント通知に機能を
    /// 限定しており、ライフサイクル/プロンプトのオーケストレーションには踏み込まない。
    /// そのため常に Allow を返す。
    fn handle_passthrough(&self, _input: &HookInput) -> Decision {
        debug!("Handling Passthrough event");

        // スコープ外イベントは常に許可（パススルー）
        Decision::allow()
    }

    /// フェイルクローズ（ブロック）でエラー応答を返す共通処理。
    ///
    /// 3 つのフェイルクローズ経路（入力過大 / 空入力 / パース失敗）で重複していた
    /// 「トレース出力 → error! ログ → エラー整形 → 出力書き込み → 終了コード返却」を集約する。
    ///
    /// - `log_message`: トレース（stderr）と `error!` ログに残す診断メッセージ。
    /// - `emit_message`: エージェントへ返す整形済みエラーの本文。通常は `log_message`
    ///   と同一だが、入力過大時のみ短い定型文（"Input too large"）を用いる。
    /// - `raw_input`: パース失敗経路のみ元入力を渡す。イベント名を判定して
    ///   エージェント別の適切な deny フォーマット（`format_error_for_input`）で返すため。
    ///   `None` の場合は汎用フォーマット（`format_error`）を用いる。
    ///
    /// プロセスを直接終了せず終了コードを返すのは、非同期ログ（tracing-appender）の
    /// フラッシュ用ガードを呼び出し側（main）で確実に drop してから終了するため
    /// （`process::exit` はスタックローカルの drop を実行しないため、ここで直接終了すると
    /// 診断ログが欠落し得る）。
    fn fail_closed(
        &self,
        stdout: &mut io::StdoutLock,
        log_message: &str,
        emit_message: &str,
        raw_input: Option<&str>,
    ) -> Result<i32> {
        if self.trace {
            eprintln!("🔍 [TRACE] ERROR: {}", log_message);
        }
        error!("{}", log_message);
        // パース失敗経路は元入力からイベント名を判定して適切なフォーマットで返す。
        let output_json = match raw_input {
            Some(input) => self.adapter.format_error_for_input(emit_message, input),
            None => self.adapter.format_error(emit_message),
        };
        let exit_code = self.adapter.error_exit_code(raw_input);
        self.write_error_output(stdout, &output_json, exit_code)?;
        Ok(exit_code)
    }

    /// フェイルクローズ時のエラー出力を適切なストリームに書き込む。
    ///
    /// Windsurf / Claude はブロック時に exit code 2 + stderr からメッセージを読むため、
    /// フェイルクローズパスでも stderr に書く必要がある。
    ///
    /// ただし終了コードが 0 の場合は「ブロックではない」応答なので stdout に書く。
    /// Stop 系のパースエラーは無限ループ回避のためイベント固有の停止許可 + exit 0 を返すが、
    /// これは判定 JSON であってエラー本文ではない。stderr に出すと本来 stdout で
    /// 返すべき JSON がデバッグログ側へ流れ、stdout が空のままになる。
    fn write_error_output(
        &self,
        stdout: &mut io::StdoutLock,
        output_json: &str,
        exit_code: i32,
    ) -> Result<()> {
        if exit_code != 0 && self.adapter.format_uses_stderr_for_errors() {
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
    fn test_process_blocks_rm_on_permission_request() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::PermissionRequest,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "rm -rf /tmp/foo".to_string(),
                timeout: None,
            }),
            session_id: None,
        };
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
    fn test_process_passthrough_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::Passthrough,
            tool_name: "Passthrough".to_string(),
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

    #[test]
    fn test_process_blocks_rm_in_eval() {
        let service = make_service();
        let input = make_bash_input("eval 'rm -rf /tmp/generated'");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_blocks_rm_in_find_exec() {
        let service = make_service();
        let input = make_bash_input(r"find . -name '*.tmp' -exec rm -rf {} \;");
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
