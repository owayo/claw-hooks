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

/// フェイルクローズ応答の本文と終了コードを組み立てる。
///
/// 本文・終了コード・出力ストリームは同じイベント判定から導かなければならない。
/// 経路ごとに個別に組み立てると、片方だけ `--event` の明示指定を見落とす等のズレが生じる
/// （実際に `emit_config_error` は `--event` を渡しておらず、設定が壊れているときだけ
/// Antigravity の Stop / PostToolUse に PreToolUse 用の deny を返していた）。
///
/// `raw_input` には元入力を渡す。読めなかった場合も `None` ではなく空文字列を渡すこと。
/// 空文字列ならイベント名を持たないフォーマットは従来どおり汎用形式へフォールバックし、
/// かつ `--event` の明示指定（Antigravity で PreToolUse / PostToolUse / Stop を
/// 区別する唯一の手段）は生かされる。
fn fail_closed_response(adapter: &FormatAdapter, message: &str, raw_input: &str) -> (String, i32) {
    (
        adapter.format_error_for_input(message, raw_input),
        adapter.error_exit_code(Some(raw_input)),
    )
}

/// フェイルクローズ応答を stderr に書くべきかを判定する。
///
/// 不変条件はひとつだけ: **エラー本文を stderr に出してよいのは、実際にブロックする
/// （exit != 0）ときだけ**。Claude / Windsurf は exit 2 のとき stdout の JSON を読まず
/// stderr 本文を理由として扱うため、ブロック時は stderr に書く必要がある。
/// 一方 Stop 系のフェイルクローズは無限ループ回避のため「停止許可 + exit 0」に倒れるが、
/// これは判定 JSON であってエラー本文ではない。stderr に出すと本来 stdout で返すべき
/// JSON がデバッグログ側へ流れ、stdout が空のままになる。
///
/// 設定エラー / ランタイムエラー / 入力のフェイルクローズの 3 経路すべてが同じ判断を
/// 必要とするため、判定をここに集約する。分散していると片方だけ直して残りが取り残され、
/// 実際に `emit_config_error` / `emit_runtime_error` は終了コードを見ない分岐のまま
/// Stop の停止許可 JSON を stderr へ流していた。
fn fail_closed_uses_stderr(adapter: &FormatAdapter, exit_code: i32) -> bool {
    exit_code != 0 && adapter.format_uses_stderr_for_errors()
}

/// フェイルクローズ応答を、終了コードに応じた正しいストリームへ書き出す。
fn write_fail_closed_response(
    adapter: &FormatAdapter,
    output: &str,
    exit_code: i32,
) -> io::Result<()> {
    if fail_closed_uses_stderr(adapter, exit_code) {
        let stderr = io::stderr();
        let mut stderr = stderr.lock();
        writeln!(stderr, "{}", output)?;
        stderr.flush()
    } else {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        writeln!(stdout, "{}", output)?;
        // パイプ越しに読まれるため exit 前にフラッシュする。
        stdout.flush()
    }
}

/// フェイルクローズ経路の診断を stderr へ書く（書けなくても失敗しない）。
///
/// `eprintln!` は stderr への書き込みに失敗すると panic する。panic は exit 101 になり、
/// Codex / Antigravity では「フック失敗＝判定を無視」と解釈されてフェイルオープンする
/// （危険コマンドのブロックが素通りする）。フェイルクローズ経路は出力先が壊れている
/// ことが前提の経路なので、診断を出せないことより終了コードを守る方を優先する。
fn warn_fail_closed_diagnostic(message: &str) {
    let stderr = io::stderr();
    let mut stderr = stderr.lock();
    let _ = writeln!(stderr, "{}", message);
    let _ = stderr.flush();
}

/// フェイルクローズ応答を書き出す。書き込み自体に失敗しても終了コードで意思を表明する。
///
/// 出力先が壊れている（パイプ切断・ディスクフル）場合、本文を届ける手段はもう無い。
/// ここでエラーを呼び出し元へ返すと「フェイルクローズのつもりが汎用ブロックに化ける」
/// 経路を再び作ってしまうため、診断だけ stderr に出して終了コードに委ねる。
fn emit_fail_closed_response(adapter: &FormatAdapter, output: &str, exit_code: i32) {
    if let Err(e) = write_fail_closed_response(adapter, output, exit_code) {
        warn_fail_closed_diagnostic(&format!(
            "claw-hooks failed to write fail-closed response: {}",
            e
        ));
    }
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
    /// `event` には `--event` の明示指定をそのまま渡す。正常経路
    /// （`HookService::with_event_override`）だけに渡して設定エラー経路で落とすと、
    /// 設定が壊れているときだけイベント判別が別物になる。Antigravity は入力に
    /// イベント名フィールドが無く、PreToolUse と PostToolUse は形状も同一
    /// （どちらも `toolCall` + `stepIdx`）なので `--event` が唯一の判別手段であり、
    /// 渡さないと PostToolUse（仕様上 `{}` 固定）や Stop（`decision` 語彙に `deny` は
    /// 存在しない）に対して PreToolUse 用の `{"decision":"deny"}` を返してしまう。
    ///
    /// 終了コードを返す（`main` 側でログガードを drop してから終了するため）。
    pub fn emit_config_error(
        format: Format,
        trace: bool,
        event: Option<String>,
        error: &anyhow::Error,
    ) -> i32 {
        let adapter = FormatAdapter::new(format, 0).with_event_override(event);
        let message = "claw-hooks configuration is invalid. Run `claw-hooks check`.";

        // イベント別の拒否形式を選ぶため、生入力からイベント名だけを読み取る。
        // 上限超過の入力も捨てずに渡す。捨てると Stop を判別できず、設定エラー時に
        // Stop へブロック（= 継続プロンプト）を返して無限ループを招く。
        // 先頭は読めているため、イベント名の走査フォールバックで判別できる。
        // 読めない / 空のときも `None` ではなく空文字列として判定経路に載せる。
        // `None` だと `format_error` の汎用形式に直行し、`--event` の明示指定まで
        // 無視されてしまう（空文字列ならイベント名を持たないフォーマットは従来どおり
        // 汎用形式へフォールバックする）。
        let raw_input = read_stdin_bounded(io::stdin())
            .ok()
            .map(|raw| String::from_utf8_lossy(&raw).into_owned())
            .unwrap_or_default();

        if trace {
            eprintln!("🔍 [TRACE] Config error fail-closed: {:#}", error);
        }

        let (output, exit_code) = fail_closed_response(&adapter, message, &raw_input);
        // 設定内容そのもの（設定ファイルの絶対パス、TOML エラーが引用する該当行）は
        // エージェントへ返す本文に含めない。ところが Claude / Windsurf は exit != 0 のとき
        // **stderr 本文をそのままブロック理由として読む**ため、stderr が判定チャネルに
        // なる場合は詳細を出すと露出してしまう（`fail_closed_uses_stderr` が表す契約）。
        // その場合は定型文だけを返し、詳細は本文が案内する `claw-hooks check` で
        // ユーザー自身が確認する。
        if !fail_closed_uses_stderr(&adapter, exit_code) {
            warn_fail_closed_diagnostic(&format!("claw-hooks configuration error: {:#}", error));
        }
        emit_fail_closed_response(&adapter, &output, exit_code);

        exit_code
    }

    /// `run` の残余の内部エラーに対するフェイルクローズ応答を出力する。
    ///
    /// `emit_config_error` と同じ理由でエラーを `?` で伝播させられないが、
    /// この時点では stdin を読み切っており、ペイロードからイベント名を取り直せない。
    /// 判別材料は `--event` の明示指定だけなので、それをアダプターへ渡した上で
    /// 空入力として整形する（Antigravity の Stop / PostToolUse はこれで正しい応答に倒れる。
    /// イベント名をペイロードに持つ他フォーマットは、空入力なら従来どおり汎用形式になる）。
    ///
    /// イベント名が既知の状態で起きるエラー（出力の整形・書き込み失敗）は、ここまで
    /// 上げずに `run` 側でイベント別のフェイルクローズへ倒す。汎用ブロックに化けると、
    /// Stop では「ブロック = 停止させず reason を継続プロンプトにする」意味になり、
    /// 「失敗 → 継続 → Stop 再発火 → 同じ失敗」の自己維持ループになるため
    /// （パイプ切断やディスクフルのように原因が持続するほど確実にループする）。
    pub fn emit_runtime_error(
        format: Format,
        trace: bool,
        event: Option<String>,
        error: &anyhow::Error,
    ) -> i32 {
        if trace {
            eprintln!("🔍 [TRACE] Runtime error fail-closed: {:#}", error);
        }

        let adapter = FormatAdapter::new(format, 0).with_event_override(event);
        let (output, exit_code) =
            fail_closed_response(&adapter, "claw-hooks encountered an internal error", "");
        // `emit_config_error` と同じ理由で、stderr が判定チャネルになる場合は
        // 内部エラーの詳細をエージェントへ流さない。
        if !fail_closed_uses_stderr(&adapter, exit_code) {
            warn_fail_closed_diagnostic(&format!("claw-hooks internal error: {:#}", error));
        }
        emit_fail_closed_response(&adapter, &output, exit_code);

        exit_code
    }

    /// 指定フォーマットで新しい HookService を作成する。
    ///
    /// アダプターを先に作り、呼び出し元エージェントの性質（識別子と、実行前フックの
    /// 補足がエージェントへ届くか）をフィルターチェーンへ渡す。command hooks の判定器は
    /// これを入力として受け取り、補足を書くかどうかを決める。
    pub fn new(config: Config, format: Format, trace: bool) -> Self {
        let adapter = FormatAdapter::new(format, config.output_max_length);
        let filter_chain = FilterChain::with_agent(&config, adapter.agent_profile());
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
                // 本文が 1 バイトも無くても空文字列を渡す。`None` を渡すと
                // イベント名の判定経路自体が飛ばされ、`--event` による明示指定
                // （Antigravity でイベントを一意に決める唯一の手段）まで無視されて、
                // Stop に対して PreToolUse 用の deny を返してしまう。
                return Ok(self.fail_closed(&log_message, "Failed to read hook input", ""));
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
            return Ok(self.fail_closed(&log_message, "Input too large", &input));
        }

        // トレースモード: 生の入力を即座に stderr に出力
        if self.trace {
            eprintln!("🔍 [TRACE] Raw input received:");
            eprintln!("{}", input);
            eprintln!("🔍 [TRACE] End of input");
        }

        if input.is_empty() {
            // セキュリティ: フェイルクローズ - 入力がない場合はブロック。
            // ただし空文字列を渡してイベント判定経路には載せる（`--event` の明示指定を
            // 活かすため）。判別できないフォーマットでは従来どおりブロックに倒れる。
            return Ok(self.fail_closed(
                "No input received from stdin",
                "No input received from stdin",
                &input,
            ));
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
                return Ok(self.fail_closed(&error_msg, &error_msg, &input));
            }
        };

        // フックを処理
        let decision = self.process(&hook_input);
        let exit_code = self.adapter.exit_code(&decision, hook_input.event);

        if self.trace {
            eprintln!("🔍 [TRACE] Decision: {:?}", decision);
            eprintln!("🔍 [TRACE] Exit code: {}", exit_code);
        }

        // フォーマットアダプターで出力を整形する。
        //
        // 整形失敗を `?` で投げ捨ててはいけない。`main` 側の `emit_runtime_error` は
        // stdin を読み切った後で呼ばれるためペイロードからイベント名を取り直せず、
        // 汎用ブロックに化ける。Stop の「ブロック」は拒否ではなく
        // 「停止させず reason を継続プロンプトにする」意味なので、
        // 「失敗 → 継続 → Stop 再発火 → 同じ失敗」の自己維持ループになる
        // （ループ防止層は 3 層ともこの経路をすり抜ける）。
        // イベントが分かっているこの場でイベント別のフェイルクローズへ倒す。
        let output = match self.adapter.format_output(&decision, hook_input.event) {
            Ok(output) => output,
            Err(e) => {
                let log_message = format!("Failed to format output: {}", e);
                return Ok(self.fail_closed(&log_message, "Failed to format hook output", &input));
            }
        };

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
        let write_result = if self.adapter.use_stderr(&decision, hook_input.event) {
            let stderr = io::stderr();
            let mut stderr = stderr.lock();
            writeln!(stderr, "{}", output).and_then(|()| stderr.flush())
        } else {
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            // パイプのためexit前にフラッシュ
            writeln!(stdout, "{}", output).and_then(|()| stdout.flush())
        };

        if let Err(e) = write_result {
            // 判定が確定した後の書き込み失敗（パイプ切断・ディスクフル）も `?` で返さない。
            // 返すと Allow に決まった判定まで `emit_runtime_error` の汎用ブロックに化け、
            // Stop では「停止させず継続」と解釈されて自己維持ループになる。
            // 書き込み失敗の原因は持続的なことが多く、再発火のたびに同じ失敗を繰り返す。
            // イベントが既知のここで、イベント別のフェイルクローズ
            // （Stop なら停止許可 + exit 0、実行前ゲートならブロック）へ倒す。
            let log_message = format!("Failed to write hook output: {}", e);
            return Ok(self.fail_closed(&log_message, "Failed to write hook output", &input));
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

        // Write/Edit/MultiEdit/NotebookEdit の場合、拡張子フック用にフィルターチェーンを実行
        // 対応エージェント:
        // - Claude Code: PostToolUse (Write / NotebookEdit)
        // - Cursor: afterFileEdit (AfterFileEdit + Write にマッピング)
        // - Windsurf: post_write_code (AfterFileEdit + Write にマッピング)
        if matches!(
            input.tool_name.as_str(),
            "Write" | "Edit" | "MultiEdit" | "NotebookEdit"
        ) {
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
    /// 5 つのフェイルクローズ経路（stdin 読み取り失敗 / 入力過大 / 空入力 / パース失敗 /
    /// 判定確定後の出力整形・書き込み失敗）で重複していた
    /// 「トレース出力 → error! ログ → エラー整形 → 出力書き込み → 終了コード返却」を集約する。
    ///
    /// - `log_message`: トレース（stderr）と `error!` ログに残す診断メッセージ。
    /// - `emit_message`: エージェントへ返す整形済みエラーの本文。通常は `log_message`
    ///   と同一だが、入力過大時のみ短い定型文（"Input too large"）を用いる。
    /// - `raw_input`: 元入力。読めなかった経路（stdin の I/O 失敗）でも空文字列を渡す。
    ///   イベント名を判定してエージェント別の適切な deny フォーマットで返すため
    ///   （詳細は `fail_closed_response` のドキュメント参照）。
    ///
    /// 出力の書き込み失敗はここで握り潰して終了コードだけを返す。`?` で返すと
    /// `main` 側の `emit_runtime_error` が汎用ブロックに差し替えてしまい、
    /// せっかくイベント別に選んだ応答（Stop の停止許可など）が失われるため。
    ///
    /// プロセスを直接終了せず終了コードを返すのは、非同期ログ（tracing-appender）の
    /// フラッシュ用ガードを呼び出し側（main）で確実に drop してから終了するため
    /// （`process::exit` はスタックローカルの drop を実行しないため、ここで直接終了すると
    /// 診断ログが欠落し得る）。
    fn fail_closed(&self, log_message: &str, emit_message: &str, raw_input: &str) -> i32 {
        if self.trace {
            eprintln!("🔍 [TRACE] ERROR: {}", log_message);
        }
        error!("{}", log_message);
        let (output_json, exit_code) = fail_closed_response(&self.adapter, emit_message, raw_input);
        emit_fail_closed_response(&self.adapter, &output_json, exit_code);
        exit_code
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

    // === フェイルクローズ応答のストリーム選択 ===

    #[test]
    fn test_fail_closed_block_uses_stderr_for_claude_and_windsurf() {
        // Claude / Windsurf は exit 2 のとき stdout の JSON を読まず stderr 本文を
        // 理由として扱うため、実際にブロックするときだけ stderr を使う。
        for format in [Format::Claude, Format::Windsurf] {
            let adapter = FormatAdapter::new(format, 0);
            assert!(
                fail_closed_uses_stderr(&adapter, 2),
                "{:?}: ブロック時のエラー本文は stderr へ",
                format
            );
        }
    }

    #[test]
    fn test_fail_closed_stop_allow_stays_on_stdout() {
        // Stop 系のフェイルクローズは無限ループ回避で「停止許可 + exit 0」に倒れる。
        // これは判定 JSON であってエラー本文ではないので stdout に出さないと、
        // エージェントから見て stdout が空（= 判定なし）のままになる。
        for format in [
            Format::Claude,
            Format::Windsurf,
            Format::Cursor,
            Format::Codex,
            Format::Agy,
            Format::Grok,
        ] {
            let adapter = FormatAdapter::new(format, 0);
            assert!(
                !fail_closed_uses_stderr(&adapter, 0),
                "{:?}: exit 0 の判定 JSON は stdout へ",
                format
            );
        }
    }

    #[test]
    fn test_fail_closed_block_stays_on_stdout_for_json_formats() {
        // Cursor / Codex / Antigravity / Grok は判定を stdout の JSON で伝える。
        for format in [Format::Cursor, Format::Codex, Format::Agy, Format::Grok] {
            let adapter = FormatAdapter::new(format, 0);
            assert!(
                !fail_closed_uses_stderr(&adapter, 2),
                "{:?}: 判定 JSON は stdout へ",
                format
            );
        }
    }

    // === フェイルクローズ応答と `--event` の明示指定 ===

    #[test]
    fn test_fail_closed_response_honors_event_override_for_agy_stop() {
        // Antigravity は入力にイベント名フィールドが無く、Stop の `decision` 語彙に
        // `deny` は存在しない。`--event` を落とすと停止許可の代わりに未定義の応答を返す。
        let adapter =
            FormatAdapter::new(Format::Agy, 0).with_event_override(Some("Stop".to_string()));
        // 空入力はペイロードを読めない状況（stdin 読み取り失敗・設定エラー）を模す。
        let (output, exit_code) = fail_closed_response(&adapter, "boom", "");
        assert_eq!(output, r#"{"decision":"stop"}"#);
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn test_fail_closed_response_honors_event_override_for_agy_post_tool_use() {
        // PostToolUse の出力は公式仕様で `{}` 固定。deny を返すと契約違反になる。
        let adapter =
            FormatAdapter::new(Format::Agy, 0).with_event_override(Some("PostToolUse".to_string()));
        let (output, exit_code) = fail_closed_response(&adapter, "boom", "");
        assert_eq!(output, "{}");
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn test_fail_closed_response_keeps_blocking_pre_tool_use() {
        // 実行前ゲートはイベントが判別できてもできなくてもブロックを維持する
        // （`--event` 指定あり / ペイロード形状からの推定 / 判別不能の 3 通り）。
        let cases = [
            (Some("PreToolUse".to_string()), ""),
            (None, r#"{"stepIdx":1,"toolCall":{"name":"run_command"}}"#),
            (None, ""),
        ];
        for (event, raw_input) in cases {
            let adapter = FormatAdapter::new(Format::Agy, 0).with_event_override(event.clone());
            let (output, _) = fail_closed_response(&adapter, "boom", raw_input);
            assert!(
                output.contains(r#""decision":"deny""#),
                "event={:?} input={:?} ではブロックを維持すること: {}",
                event,
                raw_input,
                output
            );
        }
    }

    #[test]
    fn test_fail_closed_response_blocks_pre_tool_use_on_claude() {
        // ペイロードにイベント名を持つフォーマットは、空入力なら従来どおり
        // 汎用のブロック（exit 2 + stderr 本文）へフォールバックする。
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let (output, exit_code) = fail_closed_response(&adapter, "boom", "");
        assert!(output.contains("fail-closed"), "{}", output);
        assert_eq!(exit_code, 2);

        // Stop と判別できれば停止許可（exit 0）に倒れる。
        let (output, exit_code) = fail_closed_response(
            &adapter,
            "boom",
            r#"{"hook_event_name":"Stop","stop_hook_active":false}"#,
        );
        assert_eq!(output, "{}");
        assert_eq!(exit_code, 0);
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
                cwd: None,
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
