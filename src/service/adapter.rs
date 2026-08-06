//! AIコーディングエージェント向けフォーマットアダプター。
//!
//! 以下のエージェントの入力パースと出力フォーマットを提供する:
//! - Claude Code（デフォルト）
//! - Cursor
//! - Windsurf (Cascade)
//! - Antigravity CLI
//! - Codex CLI
//! - Grok CLI

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::cli::Format;
use crate::domain::{Decision, HookEvent, HookInput, normalize_lint_output, truncate_output};
use crate::service::log_sanitizer::summarize_hook_input;

/// 壊れた JSON からイベント名を走査する際に読む先頭バイト数。
///
/// 各エージェントはイベント名を識別子としてペイロード先頭付近に置くため、
/// 先頭だけを見れば十分。全体を走査すると、巨大な `tool_input.command` の本文に
/// 偶然含まれる同名キーを拾ってしまう余地が広がるため、意図的に狭くする。
const EVENT_NAME_SCAN_PREFIX_BYTES: usize = 4096;

/// JSON としてパースできない入力から、イベント名フィールドの値だけを走査で取り出す。
///
/// 用途は「Stop 系など、ブロックを返すとループする / 無視されるイベントか」の判定に限る。
/// 入力が上限超過で途中までしか読めていない場合（`read_stdin_bounded` は
/// `MAX_INPUT_BYTES + 1` で打ち切るため必ず壊れた JSON になる）や、エージェント側の
/// 書き込みが途中で切れた場合でも、Stop のループ回避を効かせるために必要。
/// これが無いと「パース失敗 → ブロック → 継続 → Stop 再発火 → 同じ失敗」の
/// 自己維持ループになる（ループ防止層はどれもパース成功後にしか働かない）。
///
/// 誤判定の方向づけ:
/// - キーは引用符込みの完全一致で探し、値は最初の 1 件だけを採用する。
/// - 走査対象を先頭 `EVENT_NAME_SCAN_PREFIX_BYTES` に限定する。実エージェントは
///   イベント名を先頭付近に置くため、巨大なコマンド本文中の同名文字列より先に
///   本物のキーへ到達する。
/// - 見つからなければ `None` を返し、呼び出し側は従来どおりフェイルクローズド
///   （ブロック）に倒れる。
fn scan_event_name_in_malformed_input(input: &str, keys: &[&str]) -> Option<String> {
    let mut end = input.len().min(EVENT_NAME_SCAN_PREFIX_BYTES);
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = &input[..end];

    keys.iter()
        .filter_map(|key| {
            let key_at = prefix.find(key)?;
            let value = scan_json_string_value(&prefix[key_at + key.len()..])?;
            Some((key_at, value))
        })
        // 複数キーが見つかった場合は、より手前に現れた方を採用する。
        .min_by_key(|(key_at, _)| *key_at)
        .map(|(_, value)| value)
}

/// `: "value"` の形を読み、文字列値を返す。読めなければ `None`。
fn scan_json_string_value(after_key: &str) -> Option<String> {
    let mut chars = after_key
        .char_indices()
        .skip_while(|(_, c)| c.is_whitespace());
    let (_, colon) = chars.next()?;
    if colon != ':' {
        return None;
    }
    let mut rest = chars.skip_while(|(_, c)| c.is_whitespace());
    let (quote_at, quote) = rest.next()?;
    if quote != '"' {
        return None;
    }

    let body = &after_key[quote_at + 1..];
    let mut value = String::new();
    let mut escaped = false;
    for c in body.chars() {
        if escaped {
            // エスケープの厳密な復元は不要（イベント名に必要な文字は現れない）。
            value.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => {
                let trimmed = value.trim();
                return (!trimmed.is_empty()).then(|| trimmed.to_string());
            }
            _ => value.push(c),
        }
    }
    // 閉じ引用符が無い（切り詰められた）場合は判定に使わない。
    None
}

/// フォーマット固有のI/Oと内部型を変換するアダプター。
pub struct FormatAdapter {
    format: Format,
    /// 出力メッセージの最大長（0 = 無制限）
    output_max_length: usize,
    /// `--event` で明示指定されたイベント名。
    ///
    /// Antigravity の PreToolUse と PostToolUse はペイロード形状が同一
    /// （どちらも `toolCall` + `stepIdx`）で、区別できるのは Optional な `error` だけ。
    /// 一方 `hooks.json` はイベントごとに別エントリでコマンドを登録するため、
    /// 呼び出し側がどちらのイベントかを知っている。その情報を受け取るための項目。
    event_override: Option<String>,
}

impl FormatAdapter {
    /// 指定されたフォーマット用の新しいアダプターを作成する。
    pub fn new(format: Format, output_max_length: usize) -> Self {
        Self {
            format,
            output_max_length,
            event_override: None,
        }
    }

    /// `--event` によるイベント名の明示指定を設定する。
    ///
    /// 空文字・空白のみは未指定として扱う。現状これを参照するのは Antigravity のみ
    /// （他エージェントはペイロードにイベント名フィールドがあり判別が一意なため）。
    pub fn with_event_override(mut self, event: Option<String>) -> Self {
        self.event_override = event
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty());
        self
    }

    /// ログ出力用のプレフィックス（例: "✴️ Claude Code"）を返す。
    fn log_prefix(&self) -> String {
        format!("{} {}", self.format.emoji(), self.format.label())
    }

    /// フォーマットに基づいて入力文字列をHookInputにパースする。
    pub fn parse_input(&self, input: &str) -> Result<HookInput> {
        match self.format {
            Format::Claude => self.parse_claude_input(input),
            Format::Cursor => self.parse_cursor_input(input),
            Format::Windsurf => self.parse_windsurf_input(input),
            Format::Agy => self.parse_agy_input(input),
            Format::Codex => self.parse_codex_input(input),
            Format::Grok => self.parse_grok_input(input),
        }
    }

    /// エージェントフォーマットに基づいて出力をフォーマットする。
    /// event パラメータはイベント固有の JSON 形式を選ぶために使用する。
    pub fn format_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
        match self.format {
            Format::Claude => self.format_claude_output(decision, event),
            Format::Cursor => self.format_cursor_output(decision, event),
            Format::Windsurf => self.format_windsurf_output(decision, event),
            Format::Agy => self.format_agy_output(decision, event),
            Format::Codex => self.format_codex_output(decision, event),
            Format::Grok => self.format_grok_output(decision, event),
        }
    }

    /// 判定結果に対する終了コードを取得する。
    /// 注意: エージェントごとに終了コードのセマンティクスが異なる。
    /// - Claude: 0 = 成功（判定は stdout の JSON で伝達される）
    /// - Windsurf: 0 = 許可/Stop, 2 = ブロック（pre_run_command のみ）
    /// - Cursor: 0 = 成功（判定は stdout の JSON で伝達される）
    /// - Codex CLI: 0 = 成功（判定は stdout の JSON で伝達される）
    /// - Antigravity CLI: 0 = 成功（判定はJSON内、エラーも 0 + deny JSON）
    /// - Grok CLI: 0 = 許可, 2 = 拒否（PreToolUse のみ。それ以外の終了コードはフェイルオープン）
    pub fn exit_code(&self, decision: &Decision, event: HookEvent) -> i32 {
        match self.format {
            Format::Claude => {
                // Claude Code: stdout の JSON は exit 0 のときだけ解析される。
                // フェイルクローズのパースエラーだけは error_exit_code() で exit 2 を使う。
                0
            }
            Format::Agy => {
                // Antigravity CLI: 判定は stdout の JSON で伝達される。
                // PreToolUse は decision: "allow|deny|ask|force_ask"、
                // Stop は decision: "continue" で再投入する。
                // 終了コードは Codex と同じく常に 0。
                0
            }
            Format::Codex => {
                // Codex CLI: ブロック判定も stdout の JSON で解釈される。
                // 非0終了コードだとフック失敗として扱われ、判定が無視される。
                0
            }
            Format::Grok if event != HookEvent::BeforeCommand => {
                // Grok CLI で唯一ブロックできるのは PreToolUse。PostToolUse / Stop /
                // Subagent 系は「stdout は無視され、成功なら exit 0」の事後フックなので、
                // 内部判定が Block でも 0 を返して余計なフック失敗記録を残さない。
                0
            }
            Format::Grok => {
                // Grok CLI の PreToolUse: stdout の deny JSON と exit code 2 を両方返す。
                // 公式仕様は「exit 0 は許可、exit 2 は拒否」かつ「それ以外（タイムアウト・
                // クラッシュ・不正出力）はフェイルオープン」なので、Block で exit 0 を返すと
                // 実装側が終了コードを優先した場合に許可へ倒れる恐れがある。
                // 一方 exit 2 は明示的に拒否と規定されているため、両方を出すことで
                // どちらの解釈でもブロックが成立する（フェイルクローズド）。
                decision.exit_code()
            }
            Format::Cursor => {
                // Cursor: 判定はすべて stdout の JSON で伝達する。
                // 公式仕様では「exit 0 = 成功、JSON 出力を使う」「exit 2 = アクションを
                // ブロック（permission: "deny" と等価。Claude Code の挙動に合わせている）」。
                // Claude Code は exit 2 のとき stdout の JSON を無視するため、
                // deny JSON を出しつつ exit 2 で終了すると「safe-rm を使え」という
                // 代替案の本文がエージェントに届かなくなる（claw-hooks の主機能が失われる）。
                // 意図的なポリシー拒否は JSON 契約（exit 0 + deny JSON）に統一し、
                // 判定系自体が壊れたパースエラーだけを exit code 契約（error_exit_code）に回す。
                // Stop / SubagentStop も同様に followup_message は exit 0 でしか読まれない。
                0
            }
            Format::Windsurf if event == HookEvent::Stop => {
                // Windsurf の post_cascade_response は事後フックなのでブロックできない。
                0
            }
            _ => decision.exit_code(),
        }
    }

    /// 出力を stdout ではなく stderr に書き込むべきかどうか。
    /// Windsurf は exit code 2 でブロック時、stderr からエラーメッセージを読み取る
    /// （pre_run_command / post_write_code とも「exit 2 のエラーメッセージは stderr から」
    /// が公式仕様）。ただし Stop は post_cascade_response に対応する事後フックで
    /// ブロック不可のため対象外。
    pub fn use_stderr(&self, decision: &Decision, event: HookEvent) -> bool {
        matches!(
            (&self.format, event, decision),
            (
                &Format::Windsurf,
                HookEvent::BeforeCommand | HookEvent::AfterFileEdit,
                Decision::Block { .. }
            )
        )
    }

    /// フェイルクローズ時にエラー出力を stderr に書くべきかを返す。
    ///
    /// Claude/Windsurf はブロック時に exit code 2 + stderr からメッセージを読むため、
    /// パースエラー等のフェイルクローズパスでも stderr に書く必要がある。
    pub fn format_uses_stderr_for_errors(&self) -> bool {
        matches!(self.format, Format::Claude | Format::Windsurf)
    }

    /// エラーメッセージを出力用にフォーマットする。
    /// 入力パース失敗時に使用される。
    /// セキュリティ: フェイルクローズド設計 - パースエラー時はブロックする。
    pub fn format_error(&self, message: &str) -> String {
        let error_message = format!("🚫 Hook error (fail-closed): {}", message);
        match self.format {
            Format::Claude => {
                // Claude: exit 2 では stdout の JSON は無視され、stderr 本文がエラーメッセージとして
                // Claude に渡される（公式仕様: "Claude Code ignores JSON when you exit 2"）。
                // そのため fail-closed では JSON ではなくプレーンテキスト本文のみを返す。
                // format_uses_stderr_for_errors()=true + error_exit_code()=2 により stderr + exit 2 で
                // ブロックが成立し、フェイルクローズド設計は維持される。
                error_message
            }
            Format::Windsurf => {
                // Windsurf: stderr はプレーンテキストのエラーメッセージとして扱われる（JSON 解析なし）。
                // 公式仕様では exit code 2 + stderr 本文をエージェント/UI に提示するため、本文のみを返す。
                // セキュリティ: exit code 2 自体がブロックを意味するため、フェイルクローズドは維持される。
                error_message
            }
            Format::Cursor => {
                // Cursorはpermissionとuser_messageを使用
                // セキュリティ: パースエラー時は拒否（フェイルクローズド設計）
                serde_json::json!({
                    "permission": "deny",
                    "user_message": error_message,
                    "agent_message": "Hook system encountered an error and blocked for safety"
                })
                .to_string()
            }
            Format::Agy => {
                // Antigravity CLI: イベントを特定できない汎用エラーでは PreToolUse 仕様の
                // {"decision":"deny","reason":...} を返す。Stop は deny で停止を阻止できないため、
                // 入力を参照できる経路では format_error_for_input が continue を返す。
                serde_json::json!({
                    "decision": "deny",
                    "reason": error_message
                })
                .to_string()
            }
            Format::Codex => {
                // Codex CLI は "reason" フィールドでブロック理由を受け取る
                serde_json::json!({
                    "decision": "block",
                    "reason": error_message
                })
                .to_string()
            }
            Format::Grok => {
                // Grok CLI: PreToolUse の拒否は {"decision":"deny","reason":...}。
                // error_exit_code() が 2（= 拒否）を返すため、stdout JSON と終了コードの
                // 両方で拒否を表明してフェイルクローズドを維持する。
                // PreToolUse 以外は事後フックでブロック不可なので、この deny は無害に無視される。
                serde_json::json!({
                    "decision": "deny",
                    "reason": error_message
                })
                .to_string()
            }
        }
    }

    /// フェイルクローズのブロックが「継続ループ」または「無視」に終わるイベントか判定する。
    ///
    /// Stop 系イベントでは、多くのエージェントで「ブロック」が拒否ではなく
    /// **継続指示** を意味する:
    /// - Claude / Codex: `decision:"block"` は停止を阻止し、`reason` を継続プロンプトにする
    /// - Antigravity: `decision:"continue"` で再投入する
    /// - Cursor: `followup_message` は次のユーザーメッセージとして自動送信される
    ///
    /// 壊れたペイロードや設定エラーに対してこれを返すと、
    ///   フェイルクローズ → 継続 → Stop 再発火 → 同じ失敗 → …
    /// という自己維持ループになる。ループ防止層（環境変数フラグ / `stop_hook_active` /
    /// `loop_count`）はいずれもパース成功後にしか働かないため、この循環を断てない。
    ///
    /// Stop は危険操作の実行前ゲートではないので、停止を許可しても新しい副作用は
    /// 発生しない。むしろ自動継続する方が新しいツール実行を誘発する。
    /// そのためフェイルクローズドの原則よりループ回避を優先する。
    ///
    /// Windsurf の `post_cascade_response` と Grok の PreToolUse 以外は、そもそも
    /// ブロック不可の事後フックであり、拒否を返しても無視されるだけ（Windsurf では
    /// エージェントに無用なエラーが注入される）ため同様に扱う。
    fn blocks_would_loop_or_be_ignored(&self, input: &str) -> bool {
        match self.format {
            Format::Claude => matches!(
                Self::raw_hook_event_name(input).as_deref(),
                Some("Stop" | "SubagentStop")
            ),
            Format::Codex => matches!(
                Self::raw_hook_event_name(input).as_deref(),
                Some("Stop" | "stop" | "SubagentStop" | "subagent_stop")
            ),
            Format::Cursor => matches!(
                Self::raw_hook_event_name(input).as_deref(),
                Some("stop" | "subagentStop")
            ),
            Format::Agy => matches!(
                Self::agy_hook_event_name_from_input(input).as_deref(),
                Some("Stop")
            ),
            Format::Windsurf => {
                // Windsurf のイベント名フィールドは agent_action_name。
                Self::windsurf_action_name_from_input(input).as_deref()
                    == Some("post_cascade_response")
            }
            Format::Grok => {
                // Grok でブロックできるのは PreToolUse だけ。
                // イベント名が読めない場合はフェイルクローズドを維持する（false）。
                match Self::grok_hook_event_name_from_input(input) {
                    Some(event) => event != "PreToolUse",
                    None => false,
                }
            }
        }
    }

    /// 入力のイベント名を考慮してエラー出力をフォーマットする。
    ///
    /// Codex の PermissionRequest/PreToolUse、Cursor の stop 系イベント、Antigravity の
    /// Stop はそれぞれ異なる拒否形式を要求する。イベント名を判別できる場合は専用形式を
    /// 使い、判別できない場合は各フォーマットの汎用エラー形式へフォールバックする。
    pub fn format_error_for_input(&self, message: &str, input: &str) -> String {
        // ブロック不可のイベントではフェイルクローズの拒否を返してはいけない。
        // 詳細は `blocks_would_loop_or_be_ignored` のドキュメントを参照。
        if self.blocks_would_loop_or_be_ignored(input) {
            return "{}".to_string();
        }

        if self.format == Format::Codex {
            if let Some(raw_event) = Self::raw_hook_event_name(input) {
                if matches!(
                    raw_event.as_str(),
                    "PermissionRequest" | "permission_request"
                ) {
                    let error_message = format!("🚫 Hook error (fail-closed): {}", message);
                    return serde_json::json!({
                        "hookSpecificOutput": {
                            "hookEventName": "PermissionRequest",
                            "decision": {
                                "behavior": "deny",
                                "message": error_message
                            }
                        }
                    })
                    .to_string();
                }
                if matches!(raw_event.as_str(), "PreToolUse" | "pre_tool_use") {
                    let error_message = format!("🚫 Hook error (fail-closed): {}", message);
                    return serde_json::json!({
                        "hookSpecificOutput": {
                            "hookEventName": "PreToolUse",
                            "permissionDecision": "deny",
                            "permissionDecisionReason": error_message
                        }
                    })
                    .to_string();
                }
            }
        }

        self.format_error(message)
    }

    /// 壊れた JSON から `agent_action_name`（Windsurf のイベント名）を取り出す。
    fn windsurf_action_name_from_input(input: &str) -> Option<String> {
        if let Ok(raw) = serde_json::from_str::<serde_json::Value>(input) {
            return raw
                .get("agent_action_name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(String::from);
        }
        scan_event_name_in_malformed_input(input, &["\"agent_action_name\""])
    }

    /// 生 JSON からイベント名だけを取り出す。失敗時は通常のエラー形式へフォールバックする。
    fn raw_hook_event_name(input: &str) -> Option<String> {
        let parsed = serde_json::from_str::<serde_json::Value>(input).ok();
        if let Some(raw) = parsed {
            return raw
                .get("hook_event_name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    raw.get("event")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                })
                .map(String::from);
        }
        scan_event_name_in_malformed_input(input, &["\"hook_event_name\"", "\"event\""])
    }

    /// Antigravity のイベント名を明示フィールドまたは公式のイベント固有フィールドから得る。
    ///
    /// 公式ペイロードには `hook_event_name` が含まれないため、PreToolUse と Stop は
    /// 必須フィールドの形から判別する。従来の明示フィールドも後方互換で優先する。
    fn agy_hook_event_name(raw: &serde_json::Value) -> Option<String> {
        if let Some(explicit) = raw
            .get("hook_event_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                raw.get("event")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
            })
        {
            return Some(explicit.to_string());
        }

        if raw.get("toolCall").is_some() {
            return Some("PreToolUse".to_string());
        }
        if raw.get("executionNum").is_some()
            || raw.get("terminationReason").is_some()
            || raw.get("fullyIdle").is_some()
        {
            return Some("Stop".to_string());
        }
        // PostToolUse: `toolCall` を持たず `stepIdx` を持つのは PostToolUse だけなので
        // `stepIdx` 単独で一意に判別できる。公式仕様の `error` は
        // 「Optional。ツール呼び出しが失敗した場合の詳細メッセージ。成功時は空」なので、
        // これを必須条件にすると成功時のペイロードでイベント判別に失敗し、
        // ブロック不可の事後フックに対してフェイルクローズドの deny を返してしまう。
        if raw.get("stepIdx").is_some() {
            return Some("PostToolUse".to_string());
        }
        if raw.get("invocationNum").is_some() || raw.get("initialNumSteps").is_some() {
            return Some("Invocation".to_string());
        }

        None
    }

    /// 生 JSON から Grok のイベント名を取得する（camelCase の hookEventName）。
    fn grok_hook_event_name_from_input(input: &str) -> Option<String> {
        if let Ok(raw) = serde_json::from_str::<serde_json::Value>(input) {
            return raw
                .get("hookEventName")
                .or_else(|| raw.get("hook_event_name"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(String::from);
        }
        scan_event_name_in_malformed_input(input, &["\"hookEventName\"", "\"hook_event_name\""])
    }

    /// 生 JSON から Antigravity のイベント名を取得する。
    fn agy_hook_event_name_from_input(input: &str) -> Option<String> {
        let raw: serde_json::Value = serde_json::from_str(input).ok()?;
        Self::agy_hook_event_name(&raw)
    }

    /// エラー時の終了コードを取得する（フェイルクローズド = ブロック）。
    /// Codex/Agy: 非0終了コードはフック失敗として扱われ判定が無視されるため0を返す。
    /// Claude/Cursor/Windsurf/Grok: 終了コード2でブロックを表現する。
    /// Grok は「exit 2 = 拒否、それ以外の異常終了はフェイルオープン」なので、
    /// パースエラーでも 1 ではなく 2 を返す必要がある。
    ///
    /// `input` にはパース失敗した元入力を渡す。イベント名によって終了コードの
    /// セマンティクスが変わるフォーマット（Cursor の stop 系）を判別するために使う。
    /// イベント名を判別できない場合は各フォーマットの既定値へフォールバックする。
    pub fn error_exit_code(&self, input: Option<&str>) -> i32 {
        // ブロック不可のイベント（Stop 系など）では、format_error_for_input が
        // 空オブジェクトを返して停止を許可する。終了コードもそれに揃えて 0 にする。
        // exit 2 を返すと、Claude/Windsurf では「ブロック = 停止阻止 + 継続」と
        // 解釈されて継続ループを招き、それ以外でも無用なフック失敗記録が残る。
        if input.is_some_and(|raw| self.blocks_would_loop_or_be_ignored(raw)) {
            return 0;
        }

        match self.format {
            Format::Codex | Format::Agy => 0,
            _ => 2,
        }
    }

    /// ツール名が確定した後で tool_input を明示的に解析する。
    ///
    /// `ToolInput` は untagged enum なので直接デシリアライズすると、空オブジェクトが
    /// 全フィールド optional の StopInput に誤マッチする。ツール種別ごとの必須項目を
    /// ここで検証して fail-closed にする。
    fn parse_tool_input_for_tool(
        agent: &str,
        tool_name: &str,
        raw_tool_input: &serde_json::Value,
    ) -> Result<crate::domain::ToolInput> {
        match tool_name {
            // PowerShell ツールの tool_input は Bash と同一形状（`command` にコマンド文字列）。
            // Windows で Git Bash が無い環境では Claude Code が Bash ツールを登録せず
            // PowerShell が主シェルになるため、ここで拾わないと危険コマンドの
            // ブロックが全く効かなくなる（公式仕様も `Bash|PowerShell` の両方に
            // マッチさせるよう明記している）。
            "Bash" | "PowerShell" => {
                let command = raw_tool_input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("Missing tool_input.command field"))?;
                Ok(crate::domain::ToolInput::Bash(crate::domain::BashInput {
                    command: command.to_string(),
                    timeout: raw_tool_input.get("timeout").and_then(|v| v.as_u64()),
                }))
            }
            "Write" | "Edit" | "MultiEdit" => {
                let file = serde_json::from_value::<crate::domain::FileOperationInput>(
                    raw_tool_input.clone(),
                )
                .map_err(|e| anyhow!("Failed to parse {} tool_input: {}", agent, e))?;
                if file.file_path.trim().is_empty() {
                    return Err(anyhow!("Missing tool_input.file_path field"));
                }
                Ok(crate::domain::ToolInput::File(file))
            }
            _ => Ok(crate::domain::ToolInput::Other(raw_tool_input.clone())),
        }
    }

    // === Claude Code フォーマット ===

    fn parse_claude_input(&self, input: &str) -> Result<HookInput> {
        debug!(input = %summarize_hook_input(input), "{} raw input", self.log_prefix());

        let raw: serde_json::Value = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Claude input: {}", e))?;
        let claude_input: ClaudeInput = serde_json::from_value(raw.clone())
            .map_err(|e| anyhow!("Failed to parse Claude input: {}", e))?;

        let raw_event = claude_input.hook_event_name.clone();
        if raw_event.trim().is_empty() {
            return Err(anyhow!("Missing hook_event_name field"));
        }

        // Claude Codeのイベント名をHookEventにマッピング。
        // 未対応イベント（StopFailure, PreCompact, PermissionRequest 等）は claw-hooks の
        // スコープ外なのでパススルーで Allow を返す（Cursor/Codex/Antigravity と同じ方針）。
        let event = match raw_event.as_str() {
            "PreToolUse" => HookEvent::BeforeCommand,
            "PostToolUse" => HookEvent::AfterFileEdit,
            "Stop" => HookEvent::Stop,
            "SubagentStart" => HookEvent::SubagentStart,
            "SubagentStop" => HookEvent::SubagentStop,
            other => {
                debug!(
                    agent = self.format.label(),
                    hook_event_name = other,
                    mapped_event = ?HookEvent::Passthrough,
                    "{} unsupported event, passing through", self.log_prefix()
                );
                return Ok(HookInput {
                    event: HookEvent::Passthrough, // パススルー用
                    tool_name: raw_event,
                    tool_input: raw
                        .get("tool_input")
                        .cloned()
                        .map(crate::domain::ToolInput::Other)
                        .unwrap_or_else(|| crate::domain::ToolInput::Other(serde_json::json!({}))),
                    session_id: claude_input.session_id,
                });
            }
        };

        // Stop / Subagent / ツール系をイベント別ハンドラに委譲する。
        if event == HookEvent::Stop {
            return self.parse_claude_stop(claude_input);
        }
        if event == HookEvent::SubagentStart || event == HookEvent::SubagentStop {
            return Ok(self.parse_claude_subagent(&raw, event, claude_input));
        }
        self.parse_claude_tool(event, claude_input)
    }

    /// Claude の Stop イベントを内部 Stop HookInput に変換する（tool_name/tool_input は持たない）。
    fn parse_claude_stop(&self, claude_input: ClaudeInput) -> Result<HookInput> {
        let tool_name = "Stop".to_string();
        // stop_hook_active は Stop による継続ループを防ぐための必須フィールド。
        // 欠落を false とみなすと、壊れた Stop 入力でも stop hook を実行し、
        // 失敗時の decision:"block" が同じ Stop を再発火させ続ける。
        // パースエラーにすれば format_error_for_input が {} を返して安全に停止できる。
        let stop_hook_active = claude_input
            .stop_hook_active
            .ok_or_else(|| anyhow!("Missing stop_hook_active field"))?;
        // `--agent` で起動したメインセッションにも agent_type は入るため、
        // サブエージェント固有の agent_id と agent_type が両方ある場合だけ委譲扱いにする。
        // 欠落・空・型不一致は最終 lint / コミットを落とさないよう Primary に倒す。
        let has_agent_id = claude_input
            .agent_id
            .as_ref()
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty());
        let has_agent_type = claude_input
            .agent_type
            .as_ref()
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty());
        let session_kind = if has_agent_id && has_agent_type {
            crate::domain::StopSessionKind::Delegated
        } else {
            crate::domain::StopSessionKind::Primary
        };
        let tool_input = crate::domain::ToolInput::Stop(crate::domain::StopInput {
            status: None,
            loop_count: None,
            response: None,
            agent_message: claude_input.last_assistant_message.clone(),
            stop_hook_active,
            session_kind,
        });

        debug!(
            agent = self.format.label(),
            event = ?HookEvent::Stop,
            tool_name = %tool_name,
            "{} parsed input", self.log_prefix()
        );

        Ok(HookInput {
            event: HookEvent::Stop,
            tool_name,
            tool_input,
            session_id: claude_input.session_id,
        })
    }

    /// Claude の SubagentStart / SubagentStop を内部 HookInput に変換する。
    fn parse_claude_subagent(
        &self,
        raw: &serde_json::Value,
        event: HookEvent,
        claude_input: ClaudeInput,
    ) -> HookInput {
        // SubagentStart/SubagentStop: 生のJSONからサブエージェント情報を抽出
        // 注意: serde(untagged)のToolInputはStopInputの全フィールドがオプションのため、
        // tool_inputオブジェクトをOther(Value)ではなくStop(StopInput)として
        // デシリアライズする可能性があるため、生の入力を再パースする。
        // agent_type はルートレベルまたは tool_input 内にある場合がある
        let root_agent_type = raw
            .get("agent_type")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let subagent_input = if let Some(val) = raw.get("tool_input") {
            let tool_input_type = val
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            crate::domain::SubagentInput {
                // ルートレベルのagent_type（"Explore"のような人間が読める名前）を優先
                // tool_input.subagent_type（セッションID/UUIDを含む場合がある）より優先
                subagent_type: root_agent_type.or(tool_input_type),
                prompt: val.get("prompt").and_then(|v| v.as_str()).map(String::from),
                status: val.get("status").and_then(|v| v.as_str()).map(String::from),
                duration: val.get("duration").and_then(|v| v.as_u64()),
            }
        } else {
            crate::domain::SubagentInput {
                subagent_type: root_agent_type,
                ..Default::default()
            }
        };
        let tool_name = if event == HookEvent::SubagentStart {
            "SubagentStart"
        } else {
            "SubagentStop"
        }
        .to_string();

        debug!(
            agent = self.format.label(),
            event = ?event,
            tool_name = %tool_name,
            "{} parsed input", self.log_prefix()
        );

        HookInput {
            event,
            tool_name,
            tool_input: crate::domain::ToolInput::Subagent(subagent_input),
            session_id: claude_input.session_id,
        }
    }

    /// Claude のツールイベント（Bash / Write / Edit / MultiEdit）を内部 HookInput に変換する。
    fn parse_claude_tool(&self, event: HookEvent, claude_input: ClaudeInput) -> Result<HookInput> {
        let tool_name = claude_input
            .tool_name
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("Missing tool_name field"))?;
        let raw_tool_input = claude_input
            .tool_input
            .ok_or_else(|| anyhow!("Missing tool_input field"))?;
        let tool_input = Self::parse_tool_input_for_tool("Claude", &tool_name, &raw_tool_input)?;

        debug!(
            agent = self.format.label(),
            event = ?event,
            tool_name = %tool_name,
            "{} parsed input", self.log_prefix()
        );

        Ok(HookInput {
            event,
            tool_name,
            tool_input,
            session_id: claude_input.session_id,
        })
    }

    fn format_claude_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
        // Stop イベントでは Block 時に "decision":"block" + "reason" を使用。
        // Allow 時は decision を省略（"Omit to allow Claude to stop"）。
        if event == HookEvent::Stop {
            let output = match decision {
                Decision::Allow { .. } => ClaudeStopOutput {
                    decision: None,
                    reason: None,
                },
                Decision::Block { message } => {
                    let truncated = self.normalize_and_truncate(message);
                    ClaudeStopOutput {
                        decision: Some("block".to_string()),
                        reason: Some(truncated),
                    }
                }
            };
            return serde_json::to_string(&output)
                .map_err(|e| anyhow!("Failed to serialize output: {}", e));
        }

        let output = self.truncate_decision(decision).into_output(event);
        serde_json::to_string(&output).map_err(|e| anyhow!("Failed to serialize output: {}", e))
    }

    // === Cursor フォーマット ===

    fn parse_cursor_input(&self, input: &str) -> Result<HookInput> {
        debug!(input = %summarize_hook_input(input), "{} raw input", self.log_prefix());

        // Cursor は全フックに hook_event_name フィールドを送信するため、
        // これを使ってイベントを安全に識別する。
        // afterShellExecution 等の未対応イベントが command フィールドを持つ場合に
        // beforeShellExecution と誤マッチするのを防ぐ。
        let raw: serde_json::Value = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Cursor JSON: {}", e))?;

        let event_name = raw
            .get("hook_event_name")
            .and_then(|v| v.as_str())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("Missing hook_event_name field"))?
            .to_string();

        match event_name.as_str() {
            "beforeShellExecution" => self.parse_cursor_before_shell(raw),
            "preToolUse" => self.parse_cursor_pre_tool_use(raw),
            "afterFileEdit" | "afterTabFileEdit" => {
                self.parse_cursor_after_file_edit(raw, &event_name)
            }
            "stop" => self.parse_cursor_stop(raw),
            "subagentStart" => self.parse_cursor_subagent_start(raw),
            "subagentStop" => self.parse_cursor_subagent_stop(raw),
            // 未対応イベント（afterShellExecution, postToolUse 等）は
            // パススルーとして処理し、ブロックしない
            other => {
                debug!(
                    agent = self.format.label(),
                    hook_event_name = other,
                    mapped_event = ?HookEvent::Passthrough,
                    "{} unsupported event, passing through", self.log_prefix()
                );

                Ok(HookInput {
                    event: HookEvent::Passthrough,
                    tool_name: event_name.to_string(),
                    tool_input: crate::domain::ToolInput::Other(raw),
                    session_id: None,
                })
            }
        }
    }

    /// Cursor の beforeShellExecution をパースして内部 HookInput に変換する。
    fn parse_cursor_before_shell(&self, raw: serde_json::Value) -> Result<HookInput> {
        let parsed: CursorShellInput = serde_json::from_value(raw)
            .map_err(|e| anyhow!("Failed to parse Cursor beforeShellExecution: {}", e))?;
        if parsed.command.trim().is_empty() {
            return Err(anyhow!("Missing command for Cursor beforeShellExecution"));
        }

        debug!(
            agent = self.format.label(),
            hook_type = "beforeShellExecution",
            command_bytes = parsed.command.len(),
            has_cwd = parsed.cwd.is_some(),
            mapped_event = ?HookEvent::BeforeCommand,
            mapped_tool = "Bash",
            "{} parsed input", self.log_prefix()
        );

        Ok(HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: crate::domain::ToolInput::Bash(crate::domain::BashInput {
                command: parsed.command,
                timeout: None,
            }),
            session_id: None,
        })
    }

    /// Cursor の preToolUse（Shell/Bash のみ対象）をパースして内部 HookInput に変換する。
    fn parse_cursor_pre_tool_use(&self, raw: serde_json::Value) -> Result<HookInput> {
        let tool_name = raw
            .get("tool_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("Missing tool_name for Cursor preToolUse"))?
            .to_string();

        if tool_name != "Shell" && tool_name != "Bash" {
            debug!(
                agent = self.format.label(),
                hook_type = "preToolUse",
                tool_name = %tool_name,
                mapped_event = ?HookEvent::Passthrough,
                "{} unsupported preToolUse tool, passing through", self.log_prefix()
            );

            return Ok(HookInput {
                event: HookEvent::Passthrough,
                tool_name,
                tool_input: crate::domain::ToolInput::Other(raw),
                session_id: None,
            });
        }

        let command = raw
            .get("tool_input")
            .and_then(|v| v.get("command"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("Missing tool_input.command for Cursor preToolUse"))?;

        debug!(
            agent = self.format.label(),
            hook_type = "preToolUse",
            raw_tool_name = %tool_name,
            command_bytes = command.len(),
            mapped_event = ?HookEvent::BeforeCommand,
            mapped_tool = "Bash",
            "{} parsed input", self.log_prefix()
        );

        Ok(HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: crate::domain::ToolInput::Bash(crate::domain::BashInput {
                command: command.to_string(),
                timeout: None,
            }),
            session_id: None,
        })
    }

    /// Cursor の afterFileEdit / afterTabFileEdit をパースして内部 HookInput に変換する。
    fn parse_cursor_after_file_edit(
        &self,
        raw: serde_json::Value,
        event_name: &str,
    ) -> Result<HookInput> {
        let parsed: CursorFileEditInput = serde_json::from_value(raw)
            .map_err(|e| anyhow!("Failed to parse Cursor afterFileEdit: {}", e))?;
        if parsed.file_path.trim().is_empty() {
            return Err(anyhow!("Missing file_path for Cursor afterFileEdit"));
        }

        debug!(
            agent = self.format.label(),
            hook_type = event_name,
            file_path_bytes = parsed.file_path.len(),
            mapped_event = ?HookEvent::AfterFileEdit,
            mapped_tool = "Write",
            "{} parsed input", self.log_prefix()
        );

        Ok(HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: crate::domain::ToolInput::File(crate::domain::FileOperationInput {
                file_path: parsed.file_path,
                content: None,
            }),
            session_id: None,
        })
    }

    /// Cursor の stop をパースして内部 Stop HookInput に変換する。
    fn parse_cursor_stop(&self, raw: serde_json::Value) -> Result<HookInput> {
        let parsed: CursorStopInput = serde_json::from_value(raw)
            .map_err(|e| anyhow!("Failed to parse Cursor stop: {}", e))?;

        debug!(
            agent = self.format.label(),
            hook_type = "stop",
            status = %parsed.status,
            loop_count = ?parsed.loop_count,
            mapped_event = ?HookEvent::Stop,
            "{} parsed input", self.log_prefix()
        );

        Ok(HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: crate::domain::ToolInput::Stop(crate::domain::StopInput {
                status: Some(parsed.status),
                loop_count: parsed.loop_count,
                response: None,
                agent_message: None,
                stop_hook_active: false,
                // Cursor にはセッション種別を判別する入力フィールドが無いため常にメイン扱い
                session_kind: crate::domain::StopSessionKind::Primary,
            }),
            session_id: None,
        })
    }

    /// Cursor の subagentStart をパースして内部 HookInput に変換する。
    fn parse_cursor_subagent_start(&self, raw: serde_json::Value) -> Result<HookInput> {
        let parsed: CursorSubagentStartInput = serde_json::from_value(raw)
            .map_err(|e| anyhow!("Failed to parse Cursor subagentStart: {}", e))?;

        debug!(
            agent = self.format.label(),
            hook_type = "subagentStart",
            subagent_type = %parsed.subagent_type,
            mapped_event = ?HookEvent::SubagentStart,
            "{} parsed input", self.log_prefix()
        );

        Ok(HookInput {
            event: HookEvent::SubagentStart,
            tool_name: "SubagentStart".to_string(),
            tool_input: crate::domain::ToolInput::Subagent(crate::domain::SubagentInput {
                subagent_type: Some(parsed.subagent_type),
                prompt: parsed.prompt,
                status: None,
                duration: None,
            }),
            session_id: None,
        })
    }

    /// Cursor の subagentStop をパースして内部 HookInput に変換する。
    fn parse_cursor_subagent_stop(&self, raw: serde_json::Value) -> Result<HookInput> {
        let parsed: CursorSubagentStopInput = serde_json::from_value(raw)
            .map_err(|e| anyhow!("Failed to parse Cursor subagentStop: {}", e))?;

        debug!(
            agent = self.format.label(),
            hook_type = "subagentStop",
            subagent_type = %parsed.subagent_type,
            status = %parsed.subagent_status,
            mapped_event = ?HookEvent::SubagentStop,
            "{} parsed input", self.log_prefix()
        );

        Ok(HookInput {
            event: HookEvent::SubagentStop,
            tool_name: "SubagentStop".to_string(),
            tool_input: crate::domain::ToolInput::Subagent(crate::domain::SubagentInput {
                subagent_type: Some(parsed.subagent_type),
                prompt: None,
                status: Some(parsed.subagent_status),
                duration: parsed.duration,
            }),
            session_id: None,
        })
    }

    fn format_cursor_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
        // スコープ外イベント（beforeReadFile / beforeMCPExecution / beforeTabFileRead /
        // sessionStart 等）は中身を一切検査していないため、許可を表明する根拠が無い。
        // Cursor は「複数ソースのフックの応答が衝突したら優先度の高いソースが勝つ」仕様なので、
        // 未検査イベントに permission: "allow" を返すと他フックの deny を上書きし得る。
        // また beforeSubmitPrompt のように permission フィールドを持たないイベントもある。
        if event == HookEvent::Passthrough {
            return Ok("{}".to_string());
        }

        // Stop / SubagentStop の出力スキーマは followup_message のみ
        // （公式に permission フィールドは存在しない）。
        // Block は修正を指示する followup_message を返し、Allow は空オブジェクトを返す。
        if event == HookEvent::Stop || event == HookEvent::SubagentStop {
            return match decision {
                Decision::Block { message } => {
                    let truncated = self.normalize_and_truncate(message);
                    let output = CursorStopOutput {
                        followup_message: truncated,
                    };
                    serde_json::to_string(&output)
                        .map_err(|e| anyhow!("Failed to serialize Cursor output: {}", e))
                }
                Decision::Allow { .. } => Ok("{}".to_string()),
            };
        }

        match &self.truncate_decision(decision) {
            // claw-hooks は deny-only ポリシー。安全と判定した場合も明示 allow ではなく
            // 空オブジェクトを返し、Cursor 本来の権限設定と他フックの判定を尊重する。
            Decision::Allow { .. } => Ok("{}".to_string()),
            Decision::Block { message } => {
                let output = CursorOutput {
                    permission: "deny".to_string(),
                    user_message: Some(message.clone()),
                    agent_message: Some("Command blocked by claw-hooks".to_string()),
                };
                serde_json::to_string(&output)
                    .map_err(|e| anyhow!("Failed to serialize Cursor output: {}", e))
            }
        }
    }

    // === Windsurf フォーマット ===

    fn parse_windsurf_input(&self, input: &str) -> Result<HookInput> {
        debug!(input = %summarize_hook_input(input), "{} raw input", self.log_prefix());

        let raw: serde_json::Value = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Windsurf input: {}", e))?;
        let windsurf_input: WindsurfInput = serde_json::from_value(raw.clone())
            .map_err(|e| anyhow!("Failed to parse Windsurf input: {}", e))?;
        if windsurf_input.agent_action_name.trim().is_empty() {
            return Err(anyhow!("Missing agent_action_name field"));
        }

        // Windsurf の agent_action_name を内部イベント型にマッピング
        let (event, tool_name, tool_input) = match windsurf_input.agent_action_name.as_str() {
            "pre_run_command" => {
                let tool_info = windsurf_input.tool_info.as_ref().ok_or_else(|| {
                    anyhow!("Missing tool_info for Windsurf action: pre_run_command")
                })?;
                let command = tool_info
                    .command_line
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        anyhow!(
                            "Missing tool_info.command_line for Windsurf action: pre_run_command"
                        )
                    })?;
                (
                    HookEvent::BeforeCommand,
                    "Bash".to_string(),
                    crate::domain::ToolInput::Bash(crate::domain::BashInput {
                        command,
                        timeout: None,
                    }),
                )
            }
            "post_write_code" => {
                let tool_info = windsurf_input.tool_info.as_ref().ok_or_else(|| {
                    anyhow!("Missing tool_info for Windsurf action: post_write_code")
                })?;
                let file_path = tool_info
                    .file_path
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        anyhow!("Missing tool_info.file_path for Windsurf action: post_write_code")
                    })?;
                (
                    HookEvent::AfterFileEdit,
                    "Write".to_string(),
                    crate::domain::ToolInput::File(crate::domain::FileOperationInput {
                        file_path,
                        content: None,
                    }),
                )
            }
            "post_cascade_response" => {
                let response = windsurf_input
                    .tool_info
                    .as_ref()
                    .and_then(|ti| ti.response.clone());
                (
                    HookEvent::Stop,
                    "Stop".to_string(),
                    crate::domain::ToolInput::Stop(crate::domain::StopInput {
                        status: None,
                        loop_count: None,
                        agent_message: response.clone(),
                        response,
                        stop_hook_active: false,
                        // Windsurf にはセッション種別を判別する入力フィールドが無いため常にメイン扱い
                        session_kind: crate::domain::StopSessionKind::Primary,
                    }),
                )
            }
            other => {
                debug!(
                    agent = self.format.label(),
                    agent_action_name = other,
                    mapped_event = ?HookEvent::Passthrough,
                    "{} unsupported action, passing through", self.log_prefix()
                );

                return Ok(HookInput {
                    event: HookEvent::Passthrough,
                    tool_name: other.to_string(),
                    tool_input: crate::domain::ToolInput::Other(raw),
                    session_id: None,
                });
            }
        };

        debug!(
            agent = self.format.label(),
            agent_action_name = %windsurf_input.agent_action_name,
            mapped_event = ?event,
            mapped_tool = %tool_name,
            has_cwd = windsurf_input
                .tool_info
                .as_ref()
                .and_then(|ti| ti.cwd.as_ref())
                .is_some(),
            "{} parsed input", self.log_prefix()
        );

        Ok(HookInput {
            event,
            tool_name,
            tool_input,
            session_id: None,
        })
    }

    fn format_windsurf_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
        // post_cascade_response は事後フックであり、仕様上ブロックできない。
        // Stop フック内で失敗を検出しても、Windsurf には常に許可を返す。
        if event == HookEvent::Stop {
            let _ = decision;
            return Ok("{}".to_string());
        }

        // Windsurf は stdout/stderr を JSON として解析せず、プレーンテキストとして扱う。
        // - Allow: 判定は exit code 0 で伝達する。stdout には無害な空 JSON を出力する。
        // - Block: exit code 2 + stderr のメッセージ本文でブロック理由を提示する（公式仕様）。
        //   JSON ではなくメッセージ本文のみを返すことで、UI/エージェントに生 JSON が
        //   そのまま表示されるのを防ぐ。
        //   他エージェントと同様に ANSI/空白の正規化を行ってからトリムする。
        //   stderr に ANSI エスケープが混入すると Windsurf UI の表示が壊れるため。
        match decision {
            Decision::Allow { .. } => Ok("{}".to_string()),
            Decision::Block { message } => {
                let truncated = self.normalize_and_truncate(message);
                Ok(truncated)
            }
        }
    }
}

// === Claude Code フォーマット型 ===

/// Claude Code の入力フォーマット（現行仕様）。
/// 参照: https://docs.anthropic.com/en/docs/claude-code/hooks
#[derive(Debug, Deserialize)]
struct ClaudeInput {
    /// フックイベント名: PreToolUse, PostToolUse, Stop など
    hook_event_name: String,

    /// ツール名（Stop/Notification イベントでは省略可）
    #[serde(default)]
    tool_name: Option<String>,

    /// ツール入力（Stop/Notification イベントでは省略可）
    #[serde(default)]
    tool_input: Option<serde_json::Value>,

    /// セッション識別子
    #[serde(default)]
    session_id: Option<String>,

    /// このセッションで stop hooks が既に有効かどうか
    #[serde(default)]
    stop_hook_active: Option<bool>,

    /// エージェントの最後のメッセージ（Stop イベント）
    #[serde(default)]
    last_assistant_message: Option<String>,

    /// エージェント種別（例: "general-purpose"）。
    /// `--agent` のメインセッションにも含まれるため、agent_id と組み合わせて
    /// Stop のセッション種別判定に使う。
    /// 補助フィールドのため型不一致でパース全体を落とさないよう Value で受け、
    /// 文字列以外は「判別不能 = メインセッション扱い」に倒す。
    #[serde(default)]
    agent_type: Option<serde_json::Value>,

    /// サブエージェント固有の識別子。メインセッションの `--agent` には付かない。
    #[serde(default)]
    agent_id: Option<serde_json::Value>,
}

/// Claude Code の Stop イベント出力フォーマット。
/// Stop の Block 判定では "reason" に修正指示を含め、Claude は停止せず続行する。
/// Allow 時は decision を省略する（公式ドキュメント: "Omit to allow Claude to stop"）。
#[derive(Debug, Serialize)]
struct ClaudeStopOutput {
    /// 判定: Block 時のみ "block" を設定。Allow 時は省略。
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<String>,
    /// ブロック理由（エージェントへの修正指示）
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

// === Cursor フォーマット型 ===

/// Cursor の beforeShellExecution 入力フォーマット。
#[derive(Debug, Deserialize)]
struct CursorShellInput {
    /// 実行するコマンド
    command: String,
    /// 現在の作業ディレクトリ
    #[serde(default)]
    #[allow(dead_code)]
    cwd: Option<String>,
}

/// Cursor の afterFileEdit 入力フォーマット。
#[derive(Debug, Deserialize)]
struct CursorFileEditInput {
    /// 編集されたファイルのパス
    #[serde(alias = "filePath")]
    file_path: String,
}

/// Cursor の stop 入力フォーマット。
#[derive(Debug, Deserialize)]
struct CursorStopInput {
    /// 停止状態: "completed", "aborted", "error"
    status: String,
    /// この会話で発生した自動フォローアップ回数
    #[serde(default)]
    loop_count: Option<u32>,
}

/// Cursor の subagentStart 入力フォーマット。
#[derive(Debug, Deserialize)]
struct CursorSubagentStartInput {
    /// サブエージェント種別: "generalPurpose", "explore", "shell" など
    subagent_type: String,
    /// サブエージェントに渡されたタスク説明（公式フィールド名は "task"）
    #[serde(rename = "task", default)]
    prompt: Option<String>,
    /// サブエージェントで使用するモデル（公式フィールド名は "subagent_model"）
    #[serde(rename = "subagent_model", default)]
    #[allow(dead_code)]
    model: Option<String>,
}

/// Cursor の subagentStop 入力フォーマット。
#[derive(Debug, Deserialize)]
struct CursorSubagentStopInput {
    /// サブエージェント種別
    subagent_type: String,
    /// サブエージェント完了状態: "completed" / "error" / "aborted"
    #[serde(rename = "status")]
    subagent_status: String,
    /// サブエージェント出力サマリー（公式フィールド名は "summary"）
    #[serde(rename = "summary", default)]
    #[allow(dead_code)]
    result: Option<String>,
    /// 実行時間（ミリ秒、公式フィールド名は "duration_ms"）
    #[serde(rename = "duration_ms", default)]
    duration: Option<u64>,
}

/// Cursor の Stop Block 出力フォーマット。
/// "followup_message" でエージェントに lint/typecheck の修正を促す。
#[derive(Debug, Serialize)]
struct CursorStopOutput {
    /// 続行して修正するよう促すメッセージ
    followup_message: String,
}

/// Cursor の出力フォーマット。
#[derive(Debug, Serialize)]
struct CursorOutput {
    /// 権限: "allow", "deny", "ask"
    permission: String,
    /// ユーザー向けメッセージ（拒否時）
    #[serde(skip_serializing_if = "Option::is_none")]
    user_message: Option<String>,
    /// エージェント向けメッセージ（拒否時）
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_message: Option<String>,
}

// === Windsurf フォーマット型 ===

/// Windsurf の入力フォーマット。
#[derive(Debug, Deserialize)]
struct WindsurfInput {
    /// アクション名: "pre_run_command", "post_write_code" など
    agent_action_name: String,
    /// ツール固有情報
    #[serde(default)]
    tool_info: Option<WindsurfToolInfo>,
}

/// Windsurf のツール固有情報。
#[derive(Debug, Default, Deserialize)]
struct WindsurfToolInfo {
    /// pre_run_command 用のコマンドライン
    #[serde(default)]
    command_line: Option<String>,
    /// 現在の作業ディレクトリ
    #[serde(default)]
    #[allow(dead_code)]
    cwd: Option<String>,
    /// post_write_code 用のファイルパス
    #[serde(default)]
    file_path: Option<String>,
    /// post_cascade_response 用のレスポンス本文
    #[serde(default)]
    response: Option<String>,
}

// === Codex CLI フォーマット ===
// 公式ドキュメント: https://developers.openai.com/codex/hooks

impl FormatAdapter {
    fn parse_codex_input(&self, input: &str) -> Result<HookInput> {
        debug!(input = %summarize_hook_input(input), "{} raw input", self.log_prefix());

        let raw: serde_json::Value = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Codex input: {}", e))?;

        debug!(input = %summarize_hook_input(input), "{} parsed JSON", self.log_prefix());

        // イベント名は必須。互換性のため "event" エイリアスも受け付ける。
        let raw_event = raw
            .get("hook_event_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                raw.get("event")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
            })
            .ok_or_else(|| anyhow!("Missing hook_event_name field"))?
            .to_string();

        // session_id はログ・通知の相関用でしかないため、存在すれば取り出すだけにする。
        // 公式の共通フィールド表は「通常使うフィールド」の一覧であって厳密なスキーマでは
        // なく（実際に仕様中の SessionEnd 実例ペイロードには model が無い）、
        // 判定に使わないメタデータを必須にすると、フィールド 1 つの省略で
        // 全イベントがフェイルクローズドに倒れて claw-hooks 全体が機能しなくなる。
        // 検証は「claw-hooks が実際に判定に使うフィールド」に絞る（各 parse_* 側で実施）。
        let session_id = raw
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());

        let event = match raw_event.as_str() {
            "Stop" | "stop" => HookEvent::Stop,
            "PreToolUse" | "pre_tool_use" | "BeforeTool" => HookEvent::BeforeCommand,
            "PermissionRequest" | "permission_request" => HookEvent::PermissionRequest,
            "PostToolUse" | "post_tool_use" | "AfterTool" => HookEvent::AfterFileEdit,
            "SubagentStart" | "subagent_start" => HookEvent::SubagentStart,
            "SubagentStop" | "subagent_stop" => HookEvent::SubagentStop,
            // claw-hooks の処理対象外イベント: ライフサイクル/プロンプト/コンパクション系は
            // 危険コマンドブロック・保存後フック・Stop フックの責務外なので Allow で素通しする。
            // SessionEnd は advisory（出力がエージェントに影響しない）なメイン
            // スレッド終了イベントのため、同様にスコープ外として素通しする。
            "SessionStart" | "session_start" | "SessionEnd" | "session_end"
            | "UserPromptSubmit" | "user_prompt_submit" | "PreCompact" | "pre_compact"
            | "PostCompact" | "post_compact" => {
                return Ok(HookInput {
                    event: HookEvent::Passthrough, // パススルー用
                    tool_name: raw_event,
                    tool_input: crate::domain::ToolInput::Other(raw),
                    session_id,
                });
            }
            other => {
                debug!(event = %other, "{} unknown event, treating as passthrough", self.log_prefix());
                return Ok(HookInput {
                    event: HookEvent::Passthrough, // パススルー用
                    tool_name: other.to_string(),
                    tool_input: crate::domain::ToolInput::Other(raw),
                    session_id,
                });
            }
        };

        if event == HookEvent::Stop {
            return Self::parse_codex_stop(&raw, session_id);
        }

        if event == HookEvent::SubagentStart || event == HookEvent::SubagentStop {
            return Ok(Self::parse_codex_subagent(
                &raw, event, raw_event, session_id,
            ));
        }

        self.parse_codex_tool(&raw, event, raw_event, session_id)
    }

    /// 必須の真偽値フィールドを検証する。
    fn require_codex_bool(raw: &serde_json::Value, field: &str) -> Result<bool> {
        raw.get(field)
            .ok_or_else(|| anyhow!("Missing {} field", field))?
            .as_bool()
            .ok_or_else(|| anyhow!("Missing or invalid {} field", field))
    }

    /// Codex の Stop イベントを内部 Stop HookInput に変換する。
    fn parse_codex_stop(raw: &serde_json::Value, session_id: Option<String>) -> Result<HookInput> {
        let agent_message = raw
            .get("last_assistant_message")
            .or_else(|| raw.get("stop_reason"))
            .or_else(|| raw.get("message"))
            .or_else(|| raw.get("response"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // stop_hook_active は「このターンが Stop により既に継続されたか」を示す
        // 無限ループ防止の要であり、継続（Codex では decision:"block"）を返す判断に
        // 直接使う唯一のフィールドなので厳密に検証する。
        // 検証に失敗しても format_error_for_input が停止許可（{}）を返すため、
        // ここでフェイルクローズドにしても継続ループにはならない。
        let stop_hook_active = Self::require_codex_bool(raw, "stop_hook_active")?;

        Ok(HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: crate::domain::ToolInput::Stop(crate::domain::StopInput {
                status: None,
                loop_count: None,
                response: None,
                agent_message,
                stop_hook_active,
                // Codex にはセッション種別を判別する入力フィールドが無いため常にメイン扱い
                session_kind: crate::domain::StopSessionKind::Primary,
            }),
            session_id,
        })
    }

    /// Codex の SubagentStart / SubagentStop を内部 HookInput に変換する。
    ///
    /// これらは NanoBuddy 通知専用の内部イベントで、ブロック判定には使わない。
    /// `agent_type` は通知ラベルにしか使わないため、欠落してもフェイルクローズドに
    /// せず通知ラベルなしで処理する（通知のためにエージェントを止める理由がない）。
    fn parse_codex_subagent(
        raw: &serde_json::Value,
        event: HookEvent,
        raw_event: String,
        session_id: Option<String>,
    ) -> HookInput {
        let subagent_type = raw
            .get("agent_type")
            .or_else(|| raw.get("subagent_type"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());

        HookInput {
            event,
            tool_name: raw_event,
            tool_input: crate::domain::ToolInput::Subagent(crate::domain::SubagentInput {
                subagent_type,
                prompt: raw
                    .get("prompt")
                    .or_else(|| raw.get("task"))
                    .and_then(|v| v.as_str())
                    .map(String::from),
                status: raw.get("status").and_then(|v| v.as_str()).map(String::from),
                duration: raw
                    .get("duration_ms")
                    .or_else(|| raw.get("duration"))
                    .and_then(|v| v.as_u64()),
            }),
            session_id,
        }
    }

    /// Codex のツールイベント（Bash / Write / apply_patch 等）を内部 HookInput に変換する。
    fn parse_codex_tool(
        &self,
        raw: &serde_json::Value,
        event: HookEvent,
        raw_event: String,
        session_id: Option<String>,
    ) -> Result<HookInput> {
        let raw_tool_name = raw
            .get("tool_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("Missing tool_name field"))?
            .to_string();

        // Codex のツール名を内部のツール名にマッピングする。
        //
        // 公式仕様が列挙する正規のフックツール名は `Bash`（shell / exec_command 由来）、
        // `apply_patch`、`mcp__*`、および `update_plan` のような関数ツール名。
        // `shell` / `execute` / `write_file` などは旧版互換のエイリアスとして残す。
        //
        // ただし未知のツール名を `Write` へ寄せてはいけない。`Write` に写すと
        // `parse_tool_input_for_tool` が `file_path` を必須にするため、引数キーが
        // `path` / `filename` 等の別名になっている（現在または将来の）関数ツールに対して
        // PreToolUse の誤 deny が発生する。未知のツール名はそのまま渡し、
        // `ToolInput::Other` として扱わせる（組み込みフィルターは tool_name == "Bash"
        // しか見ないため無害に素通しされる）。
        let mapped_tool_name = match raw_tool_name.as_str() {
            "shell" | "run_command" | "execute" => "Bash".to_string(),
            "apply_patch" => "MultiEdit".to_string(),
            other => other.to_string(),
        };

        // `tool_input` の取得は「あれば使う」に留め、必須判定は後段のツール種別で行う。
        // 公式仕様では MCP ツールや `update_plan` のような関数ツールも同じフック経路を
        // 通り、引数を持たないツールは `tool_input` を送らない。
        // ここで全ツール共通に必須化すると次の実害が出る:
        //   - PostToolUse: `decision:"block"` が「実際のツール出力をフックのメッセージで
        //     置き換える」動作のため、モデルが本来のツール出力を失う
        //   - PreToolUse: 引数を持たないツールに対する誤 deny（即時ハードブロック）
        let raw_tool_input = raw.get("tool_input");

        // PostToolUse（実行後）では、claw-hooks は保存後フックのためにファイルパスしか
        // 使わない。ファイル編集以外のツールに対して tool_input を厳密検証しても
        // 得るものが無い一方、失敗時のコストが極端に大きい（上記の出力置き換え）。
        // そのためファイル編集系ツール以外は検証せず素通しする。
        let is_file_editing_tool = raw_tool_name == "apply_patch"
            || matches!(mapped_tool_name.as_str(), "Write" | "Edit" | "MultiEdit");
        if event == HookEvent::AfterFileEdit && !is_file_editing_tool {
            debug!(
                agent = self.format.label(),
                raw_event = %raw_event,
                raw_tool_name = %raw_tool_name,
                mapped_event = ?event,
                "{} PostToolUse for non-file tool, no post-edit hook applies", self.log_prefix()
            );
            return Ok(HookInput {
                event,
                tool_name: mapped_tool_name,
                tool_input: crate::domain::ToolInput::Other(
                    raw_tool_input
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                ),
                session_id,
            });
        }

        // claw-hooks が実際に判定へ使うツールだけ `tool_input` を必須にする
        // （危険コマンド判定の `Bash` / `PowerShell`、保存後フックのファイル編集系）。
        // それ以外は欠落を許容し、`ToolInput::Other` として素通しする。
        let inspects_tool_input = raw_tool_name == "apply_patch"
            || matches!(
                mapped_tool_name.as_str(),
                "Bash" | "PowerShell" | "Write" | "Edit" | "MultiEdit"
            );
        let Some(raw_tool_input) = raw_tool_input else {
            if inspects_tool_input {
                return Err(anyhow!("Missing tool_input field"));
            }
            debug!(
                agent = self.format.label(),
                raw_event = %raw_event,
                raw_tool_name = %raw_tool_name,
                mapped_event = ?event,
                "{} tool_input absent for tool outside claw-hooks' scope, passing through",
                self.log_prefix()
            );
            return Ok(HookInput {
                event,
                tool_name: mapped_tool_name,
                tool_input: crate::domain::ToolInput::Other(serde_json::json!({})),
                session_id,
            });
        };

        let tool_input = if raw_tool_name == "apply_patch" {
            // Bash 経路と同様に空文字列の command も fail-closed にする（必須フィールド扱い）。
            let command = raw_tool_input
                .get("command")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow!("Missing tool_input.command field"))?;
            crate::domain::ToolInput::Files(Self::parse_apply_patch_file_inputs(command))
        } else {
            Self::parse_tool_input_for_tool("Codex", &mapped_tool_name, raw_tool_input)?
        };

        debug!(
            agent = self.format.label(),
            raw_event = %raw_event,
            mapped_event = ?event,
            raw_tool_name = %raw_tool_name,
            mapped_tool = %mapped_tool_name,
            "{} parsed input", self.log_prefix()
        );

        Ok(HookInput {
            event,
            tool_name: mapped_tool_name,
            tool_input,
            session_id,
        })
    }

    fn format_codex_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
        // Codex CLI: Allow は exit 0 + 空 JSON（公式ドキュメント推奨）。
        // PermissionRequest の Block は専用の hookSpecificOutput で deny を返す。
        // PreToolUse の Block は推奨形式 hookSpecificOutput.permissionDecision="deny" を使用する
        // （legacy の {"decision":"block"} も受理されるが、公式ドキュメントの主形式に合わせる）。
        // PostToolUse / Stop の Block は {"decision":"block","reason":"..."} がそれぞれの正式形式。
        let output = match decision {
            Decision::Allow {
                additional_context: Some(ctx),
            } if event == HookEvent::AfterFileEdit => {
                let truncated = truncate_output(ctx, self.output_max_length);
                serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PostToolUse",
                        "additionalContext": truncated
                    }
                })
            }
            Decision::Allow { .. } => serde_json::json!({}),
            Decision::Block { message } => {
                let truncated = self.normalize_and_truncate(message);
                if event == HookEvent::PermissionRequest {
                    serde_json::json!({
                        "hookSpecificOutput": {
                            "hookEventName": "PermissionRequest",
                            "decision": {
                                "behavior": "deny",
                                "message": truncated
                            }
                        }
                    })
                } else if event == HookEvent::BeforeCommand {
                    serde_json::json!({
                        "hookSpecificOutput": {
                            "hookEventName": "PreToolUse",
                            "permissionDecision": "deny",
                            "permissionDecisionReason": truncated
                        }
                    })
                } else {
                    serde_json::json!({
                        "decision": "block",
                        "reason": truncated
                    })
                }
            }
        };
        serde_json::to_string(&output)
            .map_err(|e| anyhow!("Failed to serialize Codex output: {}", e))
    }

    /// Decision のメッセージを output_max_length で切り詰める。
    fn truncate_decision(&self, decision: &Decision) -> Decision {
        match decision {
            Decision::Allow {
                additional_context: ctx,
            } => Decision::Allow {
                additional_context: ctx
                    .as_ref()
                    .map(|c| truncate_output(c, self.output_max_length)),
            },
            Decision::Block { message } => Decision::Block {
                message: truncate_output(message, self.output_max_length),
            },
        }
    }

    /// メッセージを正規化してから output_max_length で切り詰める。
    fn normalize_and_truncate(&self, message: &str) -> String {
        truncate_output(&normalize_lint_output(message), self.output_max_length)
    }

    fn parse_apply_patch_file_inputs(command: &str) -> Vec<crate::domain::FileOperationInput> {
        Self::extract_apply_patch_paths(command)
            .into_iter()
            .map(|file_path| crate::domain::FileOperationInput {
                file_path,
                content: None,
            })
            .collect()
    }

    fn extract_apply_patch_paths(command: &str) -> Vec<String> {
        let mut paths = Vec::new();
        let mut last_replaceable_index = None;

        for line in command.lines() {
            let line = line.trim_end_matches('\r');
            if let Some(path) = line
                .strip_prefix("*** Add File: ")
                .or_else(|| line.strip_prefix("*** Update File: "))
            {
                last_replaceable_index = Self::push_unique_apply_patch_path(&mut paths, path);
            } else if let Some(path) = line.strip_prefix("*** Move to: ") {
                let path = path.trim();
                if path.is_empty() {
                    last_replaceable_index = None;
                    continue;
                }

                if let Some(index) = last_replaceable_index.take() {
                    if let Some(existing_index) = paths.iter().position(|existing| existing == path)
                    {
                        if existing_index != index {
                            paths.remove(index);
                        }
                    } else if let Some(existing) = paths.get_mut(index) {
                        *existing = path.to_string();
                    }
                } else {
                    Self::push_unique_apply_patch_path(&mut paths, path);
                }
            } else if line.strip_prefix("*** Delete File: ").is_some() || line.starts_with("*** ") {
                // 削除対象には保存後フックを実行できない。その他の patch 制御行で rename 候補を閉じる。
                last_replaceable_index = None;
            }
        }

        paths
    }

    fn push_unique_apply_patch_path(paths: &mut Vec<String>, path: &str) -> Option<usize> {
        let path = path.trim();
        if path.is_empty() {
            return None;
        }

        if let Some(index) = paths.iter().position(|existing| existing == path) {
            Some(index)
        } else {
            paths.push(path.to_string());
            Some(paths.len() - 1)
        }
    }
}

// === Antigravity CLI フォーマット ===
// 公式仕様: docs/hooks/antigravity/antigravity_cli.md
//
// 入力（stdin JSON, camelCase）:
//   - 共通: conversationId / workspacePaths / transcriptPath / artifactDirectoryPath
//   - ツール実行前: toolCall { name, args }, stepIdx
//   - PostToolUse: stepIdx, error (toolCall は含まれない)
//   - 呼び出し前／後: invocationNum, initialNumSteps
//   - Stop イベント: executionNum, terminationReason, error, fullyIdle
//
// 出力（stdout JSON）:
//   - ツール実行前: { decision: "allow|deny|ask|force_ask", reason?, permissionOverrides? }
//   - PostToolUse: {} （事後フックでありブロック不可。エラー伝達も仕様には無い）
//   - PreInvocation / PostInvocation: { injectSteps?, terminationBehavior? } （claw-hooks スコープ外）
//   - Stop: { decision: "continue", reason? } で再投入、それ以外（または {}）で停止許可
//
// claw-hooks のスコープ的制約:
//   - PostToolUse は仕様上発火するが、ペイロードは stepIdx と error のみで toolCall を持たない。
//     出力も {} 固定。よって「どのファイルが編集されたか」を復元できず、ファイル単位の
//     拡張子フック（保存後の auto-format）は成立しない。代替: Stop hooks で lint/typecheck を
//     回し、failure を Stop の "continue" で再投入する。
//   - PreInvocation / PostInvocation はモデル呼び出し前後のオーケストレーション系で、
//     コマンドブロック・拡張子フックの責務外なのでパススルー（{}）で素通しする。
impl FormatAdapter {
    fn parse_agy_input(&self, input: &str) -> Result<HookInput> {
        debug!(input = %summarize_hook_input(input), "{} raw input", self.log_prefix());

        let raw: serde_json::Value = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Antigravity input: {}", e))?;

        // イベント名の決定順:
        //   1. `--event` による明示指定（hooks.json はイベントごとに別エントリで
        //      コマンドを登録するため、呼び出し側が正確なイベント名を知っている）
        //   2. ペイロード中の hook_event_name / event（旧版互換）
        //   3. イベント固有フィールドの形状からの推定
        //
        // 3 の推定では PreToolUse と PostToolUse を区別できない。公式仕様では
        // どちらも `toolCall` + `stepIdx` を持ち、差は Optional な `error` だけだからである。
        // 推定時は PreToolUse に倒す（危険コマンドのブロックを維持するフェイルクローズド方向。
        // 逆に倒すと実行前のコマンドを素通ししてしまう）。正確な PostToolUse 処理
        // （保存後フックの実行と `{}` 固定の出力）が必要な場合は `--event PostToolUse` を使う。
        let raw_event = self
            .event_override
            .clone()
            .or_else(|| Self::agy_hook_event_name(&raw))
            .ok_or_else(|| anyhow!("Unable to infer Antigravity hook event"))?;

        // Antigravity は conversationId を会話 ID として渡す。
        // session_id エイリアスでも受ける（ローカルテストや他エージェントからの転送向け）。
        let session_id = raw
            .get("conversationId")
            .or_else(|| raw.get("session_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        match raw_event.as_str() {
            "PreToolUse" => self.parse_agy_pre_tool_use(&raw, session_id),
            "Stop" => Self::parse_agy_stop(&raw, session_id),
            // PostToolUse は `toolCall`（name と args）を含むため、編集対象ファイルを
            // 復元できる。`write_to_file` / `replace_file_content` /
            // `multi_replace_file_content` の `args.TargetFile` を拾って保存後フックを回す。
            // 出力は仕様どおり `{}` 固定（format_agy_output の AfterFileEdit 分岐）。
            "PostToolUse" => self.parse_agy_post_tool_use(&raw, session_id),
            // PreInvocation / PostInvocation はモデル呼び出し前後のオーケストレーション系で
            // claw-hooks のスコープ外。パススルーで Allow を返す。
            "PreInvocation" | "PostInvocation" | "Invocation" => {
                debug!(
                    agent = self.format.label(),
                    hook_event_name = %raw_event,
                    mapped_event = ?HookEvent::Passthrough,
                    "{} event is out of scope for claw-hooks, passing through", self.log_prefix()
                );
                Ok(HookInput {
                    event: HookEvent::Passthrough,
                    tool_name: raw_event,
                    tool_input: crate::domain::ToolInput::Other(raw),
                    session_id,
                })
            }
            other => {
                debug!(
                    agent = self.format.label(),
                    hook_event_name = other,
                    mapped_event = ?HookEvent::Passthrough,
                    "{} unsupported event, passing through", self.log_prefix()
                );
                Ok(HookInput {
                    event: HookEvent::Passthrough,
                    tool_name: other.to_string(),
                    tool_input: crate::domain::ToolInput::Other(raw),
                    session_id,
                })
            }
        }
    }

    /// Antigravity の PreToolUse をパースして内部 HookInput に変換する。
    fn parse_agy_pre_tool_use(
        &self,
        raw: &serde_json::Value,
        session_id: Option<String>,
    ) -> Result<HookInput> {
        // `stepIdx` は公式仕様で Required マークが無く、claw-hooks も値を判定に使わない。
        // 必須にすると、このフィールドを送らない（あるいは名前を変えた）ビルドで
        // 全ての run_command が「即時ハードブロック」を意味する deny に倒れるため、
        // 判定に使うフィールドだけ検証する方針（Codex と同じ）に揃えて任意扱いにする。
        let tool_call = raw
            .get("toolCall")
            .ok_or_else(|| anyhow!("Missing toolCall field"))?;
        let raw_tool_name = tool_call
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("Missing toolCall.name field"))?
            .to_string();
        // `args` は run_command 分岐でしか読まないため、ここでは必須にしない。
        // 公式仕様は matcher `""` / `"*"`（全ツール一致）を認めており、
        // 引数を持たないツール（`list_permissions` 等）も列挙されている。
        // 全ツール共通で `args` を必須にすると、スコープ外のツール呼び出しに対して
        // 「即時ハードブロック」を意味する deny を返してしまう。
        let raw_args = tool_call.get("args");

        match raw_tool_name.as_str() {
            // run_command: Antigravity のシェル実行ツール。args.CommandLine をコマンド本文として扱う。
            "run_command" => {
                let command = raw_args
                    .ok_or_else(|| anyhow!("Missing toolCall.args field"))?
                    .get("CommandLine")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("Missing toolCall.args.CommandLine field"))?;

                debug!(
                    agent = self.format.label(),
                    raw_event = "PreToolUse",
                    mapped_event = ?HookEvent::BeforeCommand,
                    raw_tool_name = %raw_tool_name,
                    mapped_tool = "Bash",
                    command_bytes = command.len(),
                    "{} parsed input", self.log_prefix()
                );

                Ok(HookInput {
                    event: HookEvent::BeforeCommand,
                    tool_name: "Bash".to_string(),
                    tool_input: crate::domain::ToolInput::Bash(crate::domain::BashInput {
                        command: command.to_string(),
                        timeout: None,
                    }),
                    session_id,
                })
            }
            // run_command 以外のツール（write_to_file / replace_file_content / multi_replace_file_content /
            // view_file / list_dir / find_by_name / grep_search / invoke_subagent / ...）は
            // claw-hooks のコマンドブロックの対象外。Allow パスとして素通しする。
            other => {
                debug!(
                    agent = self.format.label(),
                    raw_event = "PreToolUse",
                    mapped_event = ?HookEvent::Passthrough,
                    raw_tool_name = other,
                    "{} tool out of scope for claw-hooks, passing through", self.log_prefix()
                );

                Ok(HookInput {
                    event: HookEvent::Passthrough,
                    tool_name: other.to_string(),
                    tool_input: crate::domain::ToolInput::Other(
                        raw_args.cloned().unwrap_or_else(|| serde_json::json!({})),
                    ),
                    session_id,
                })
            }
        }
    }

    /// Antigravity の PostToolUse を内部 HookInput に変換する。
    ///
    /// 公式仕様の PostToolUse は `toolCall`（`name` と `args`）を含むため、
    /// ファイル編集ツールの `args.TargetFile` から編集対象を復元できる。
    /// これにより保存後の formatter / linter 実行（拡張子フック）が成立する。
    ///
    /// 出力は仕様上 `{}` 固定でブロック不可のため、ここで返す HookInput が
    /// どの判定になっても Antigravity へは `{}` しか返らない
    /// （`format_agy_output` の `AfterFileEdit` / `Passthrough` 分岐）。
    /// したがって `toolCall` 欠落などのパース失敗でフェイルクローズドしても
    /// エージェントに伝わらないため、ここは寛容側に倒してパススルーする。
    fn parse_agy_post_tool_use(
        &self,
        raw: &serde_json::Value,
        session_id: Option<String>,
    ) -> Result<HookInput> {
        let tool_name = raw
            .get("toolCall")
            .and_then(|c| c.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        // Antigravity のファイル編集ツールは編集先を `args.TargetFile` で渡す。
        let target_file = raw
            .get("toolCall")
            .and_then(|c| c.get("args"))
            .and_then(|a| a.get("TargetFile"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty());

        let is_file_edit_tool = matches!(
            tool_name.as_str(),
            "write_to_file" | "replace_file_content" | "multi_replace_file_content"
        );

        if let (true, Some(file_path)) = (is_file_edit_tool, target_file) {
            debug!(
                agent = self.format.label(),
                raw_event = "PostToolUse",
                mapped_event = ?HookEvent::AfterFileEdit,
                raw_tool_name = %tool_name,
                mapped_tool = "Write",
                file_path_bytes = file_path.len(),
                "{} parsed input", self.log_prefix()
            );
            return Ok(HookInput {
                event: HookEvent::AfterFileEdit,
                tool_name: "Write".to_string(),
                tool_input: crate::domain::ToolInput::File(crate::domain::FileOperationInput {
                    file_path: file_path.to_string(),
                    content: None,
                }),
                session_id,
            });
        }

        // run_command の PostToolUse（コマンドは実行済みでブロック不可）や
        // 編集対象を持たないツールはスコープ外としてパススルーする。
        debug!(
            agent = self.format.label(),
            raw_event = "PostToolUse",
            mapped_event = ?HookEvent::Passthrough,
            raw_tool_name = %tool_name,
            "{} PostToolUse has no post-edit hook target, passing through", self.log_prefix()
        );
        Ok(HookInput {
            event: HookEvent::Passthrough,
            tool_name: if tool_name.is_empty() {
                "PostToolUse".to_string()
            } else {
                tool_name
            },
            tool_input: crate::domain::ToolInput::Other(raw.clone()),
            session_id,
        })
    }

    /// Antigravity の Stop イベントを内部 Stop HookInput に変換する。
    fn parse_agy_stop(raw: &serde_json::Value, session_id: Option<String>) -> Result<HookInput> {
        // `executionNum` / `terminationReason` は公式仕様で **Required** マークが無く、
        // かつ claw-hooks は判定に使わない（前者は破棄、後者は status / agent_message の
        // 情報表示のみ）。これらを必須にすると、フィールドが 1 つ欠けただけで Stop の
        // パースが失敗する。Antigravity の Stop のパース失敗は無限ループ回避のため
        // `{}` + exit 0（停止許可）に倒れる仕様なので、失敗は「エラー表示なしで
        // 全 stop hook（lint / commit / 通知）が黙って実行されない」形で現れる。
        // 判定に使わないフィールドで機能を丸ごと落とさないよう、任意扱いにする。
        let termination_reason = raw
            .get("terminationReason")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        // fullyIdle は Stop 固有の必須フィールド。値自体はフィルター判定に使わないが、
        // 欠落や型不正を受理すると壊れたペイロードで stop hook を実行してしまう。
        let _fully_idle = raw
            .get("fullyIdle")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| anyhow!("Missing fullyIdle field"))?;

        // terminationReason が "error" のとき、Antigravity は別途 error フィールドに詳細メッセージを入れる。
        // 互換性のため、agent_message は error → terminationReason の順で取り出す。
        let agent_message = raw
            .get("error")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| Some(termination_reason.clone()))
            .filter(|s| !s.is_empty());

        Ok(HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: crate::domain::ToolInput::Stop(crate::domain::StopInput {
                status: Some(termination_reason),
                loop_count: None,
                response: None,
                agent_message,
                stop_hook_active: false,
                // Antigravity にはセッション種別を判別する入力フィールドが無いため常にメイン扱い
                session_kind: crate::domain::StopSessionKind::Primary,
            }),
            session_id,
        })
    }

    fn format_agy_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
        // Antigravity の出力スキーマはイベントによって異なる:
        //   - ツール実行前: {decision: "allow|deny|ask|force_ask", reason?, permissionOverrides?}
        //   - PostToolUse / PreInvocation / PostInvocation: {} 固定（ブロック不可）
        //   - Stop: 再投入は {decision: "continue", reason?}、停止許可は {}
        let output = match (event, decision) {
            // Stop の Block は "continue" + reason でエージェントを再起動させ、reason を
            // system message としてエージェントに注入する（lint/typecheck の修正指示等）。
            (HookEvent::Stop, Decision::Block { message }) => {
                let truncated = self.normalize_and_truncate(message);
                serde_json::json!({
                    "decision": "continue",
                    "reason": truncated
                })
            }
            // Stop の Allow は {} を返す（"decision":"continue" 以外なら停止許可、最も無害な空オブジェクト）。
            (HookEvent::Stop, Decision::Allow { .. }) => serde_json::json!({}),
            // PostToolUse / 内部 Passthrough（PreInvocation / PostInvocation / 未対応ツール）は
            // ブロック仕様が無いため、claw-hooks 側で Block を検出しても {} に倒す（公式仕様準拠）。
            (HookEvent::AfterFileEdit, _) | (HookEvent::Passthrough, _) => serde_json::json!({}),
            // PreToolUse の Block は deny として返す（Antigravity 公式の主形式）。
            (_, Decision::Block { message }) => {
                let truncated = self.normalize_and_truncate(message);
                serde_json::json!({
                    "decision": "deny",
                    "reason": truncated
                })
            }
            // PreToolUse の Allow は "allow" を返す（明示的に allow を宣言する）。
            (_, Decision::Allow { .. }) => serde_json::json!({"decision": "allow"}),
        };
        serde_json::to_string(&output)
            .map_err(|e| anyhow!("Failed to serialize Antigravity output: {}", e))
    }
}

// === Grok CLI フォーマット ===
// 公式仕様: docs/hooks/grok/hooks.md
//
// 入力（stdin JSON, camelCase）:
//   - 共通: hookEventName / sessionId / cwd / workspaceRoot
//   - ツール系イベント: toolName / toolInput
//
// 出力:
//   - PreToolUse だけがブロック可能な唯一のイベント。
//     拒否は {"decision":"deny","reason":...}、許可は exit 0。
//   - それ以外（PostToolUse / Stop / Subagent 系）は事後フックで stdout は無視され、
//     成功時は exit 0 を返す。
//
// 終了コード: 0 = 許可、2 = 拒否。それ以外（タイムアウト・クラッシュ・不正出力）は
// フェイルオープンとして扱われ、ツール呼び出しはそのまま実行される。
// そのため claw-hooks はフェイルクローズしたい経路で必ず exit 2 を返す。
impl FormatAdapter {
    fn parse_grok_input(&self, input: &str) -> Result<HookInput> {
        debug!(input = %summarize_hook_input(input), "{} raw input", self.log_prefix());

        let raw: serde_json::Value = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Grok input: {}", e))?;

        // 公式仕様のフィールド名は camelCase の hookEventName。
        // Grok は Claude Code / Cursor のフック設定ファイルも読み込むため、
        // 後方互換として snake_case の hook_event_name も受理する。
        let raw_event = raw
            .get("hookEventName")
            .or_else(|| raw.get("hook_event_name"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("Missing hookEventName field"))?
            .to_string();

        let session_id = raw
            .get("sessionId")
            .or_else(|| raw.get("session_id"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());

        match raw_event.as_str() {
            "PreToolUse" => self.parse_grok_tool(&raw, HookEvent::BeforeCommand, session_id),
            "PostToolUse" => self.parse_grok_tool(&raw, HookEvent::AfterFileEdit, session_id),
            "Stop" => Ok(Self::parse_grok_stop(&raw, session_id)),
            "SubagentStart" => Ok(Self::parse_grok_subagent(
                &raw,
                HookEvent::SubagentStart,
                session_id,
            )),
            "SubagentStop" => Ok(Self::parse_grok_subagent(
                &raw,
                HookEvent::SubagentStop,
                session_id,
            )),
            // 未対応イベント（SessionStart / SessionEnd / UserPromptSubmit /
            // PostToolUseFailure / PermissionDenied / StopFailure / Notification /
            // PreCompact / PostCompact）は claw-hooks のスコープ外なのでパススルーで許可する。
            other => {
                debug!(
                    agent = self.format.label(),
                    hook_event_name = other,
                    mapped_event = ?HookEvent::Passthrough,
                    "{} unsupported event, passing through", self.log_prefix()
                );
                Ok(HookInput {
                    event: HookEvent::Passthrough,
                    tool_name: other.to_string(),
                    tool_input: crate::domain::ToolInput::Other(raw),
                    session_id,
                })
            }
        }
    }

    /// Grok の PreToolUse / PostToolUse をパースして内部 HookInput に変換する。
    ///
    /// Grok は Claude のツール名（`Bash` / `Edit` 等）を自前のツール名へ自動マッピングする
    /// と明記しているが、マッピング後の実際のツール名は公開仕様に列挙されていない。
    /// ツール名の文字列一致に頼ると、想定外の名前のシェル実行ツールが素通りして
    /// 危険コマンドのブロックを回避できてしまう。そのため `toolInput` の形（`command` を
    /// 持つか、ファイルパスを持つか）で判定し、名前に依存しないフェイルクローズド設計にする。
    fn parse_grok_tool(
        &self,
        raw: &serde_json::Value,
        event: HookEvent,
        session_id: Option<String>,
    ) -> Result<HookInput> {
        let raw_tool_name = raw
            .get("toolName")
            .or_else(|| raw.get("tool_name"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("Missing toolName field"))?
            .to_string();
        let tool_input = raw
            .get("toolInput")
            .or_else(|| raw.get("tool_input"))
            .ok_or_else(|| anyhow!("Missing toolInput field"))?;

        // シェル実行系: toolInput.command を持つツールはコマンドブロックの対象。
        if let Some(command) = tool_input
            .get("command")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            // PostToolUse（実行後）はコマンドをブロックできないため、拡張子フックの
            // 対象にもならない。素通しする。
            if event != HookEvent::BeforeCommand {
                return Ok(Self::grok_passthrough(
                    raw_tool_name,
                    tool_input.clone(),
                    session_id,
                ));
            }

            debug!(
                agent = self.format.label(),
                raw_event = "PreToolUse",
                raw_tool_name = %raw_tool_name,
                mapped_event = ?HookEvent::BeforeCommand,
                mapped_tool = "Bash",
                command_bytes = command.len(),
                "{} parsed input", self.log_prefix()
            );

            return Ok(HookInput {
                event: HookEvent::BeforeCommand,
                tool_name: "Bash".to_string(),
                tool_input: crate::domain::ToolInput::Bash(crate::domain::BashInput {
                    command: command.to_string(),
                    timeout: tool_input.get("timeout").and_then(|v| v.as_u64()),
                }),
                session_id,
            });
        }

        // ファイル編集系: PostToolUse のみ保存後フック（フォーマッタ/リンタ）の対象。
        // Grok は Claude 互換の tool_input を渡すため file_path を主に見るが、
        // camelCase の filePath でも受理する。
        if let Some(file_path) = tool_input
            .get("file_path")
            .or_else(|| tool_input.get("filePath"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            // PreToolUse（編集前）は claw-hooks の責務外。保存後フックは PostToolUse で実行する。
            if event != HookEvent::AfterFileEdit {
                return Ok(Self::grok_passthrough(
                    raw_tool_name,
                    tool_input.clone(),
                    session_id,
                ));
            }

            debug!(
                agent = self.format.label(),
                raw_event = "PostToolUse",
                raw_tool_name = %raw_tool_name,
                mapped_event = ?HookEvent::AfterFileEdit,
                mapped_tool = "Write",
                file_path_bytes = file_path.len(),
                "{} parsed input", self.log_prefix()
            );

            return Ok(HookInput {
                event: HookEvent::AfterFileEdit,
                tool_name: "Write".to_string(),
                tool_input: crate::domain::ToolInput::File(crate::domain::FileOperationInput {
                    file_path: file_path.to_string(),
                    content: None,
                }),
                session_id,
            });
        }

        // command / file_path のどちらも持たないツール（read / search / list 等）は
        // claw-hooks の対象外なのでパススルーで許可する。
        debug!(
            agent = self.format.label(),
            raw_tool_name = %raw_tool_name,
            mapped_event = ?HookEvent::Passthrough,
            "{} tool out of scope for claw-hooks, passing through", self.log_prefix()
        );
        Ok(Self::grok_passthrough(
            raw_tool_name,
            tool_input.clone(),
            session_id,
        ))
    }

    /// スコープ外ツール用のパススルー HookInput を組み立てる。
    fn grok_passthrough(
        tool_name: String,
        tool_input: serde_json::Value,
        session_id: Option<String>,
    ) -> HookInput {
        HookInput {
            event: HookEvent::Passthrough,
            tool_name,
            tool_input: crate::domain::ToolInput::Other(tool_input),
            session_id,
        }
    }

    /// Grok の Stop イベントを内部 Stop HookInput に変換する。
    ///
    /// Grok には `stop_hook_active` / `loop_count` に相当する入力フィールドが無い。
    /// 無限ループ防止はプロセス間の環境変数フラグ層が担う。
    /// また API エラー終了は別イベント `StopFailure` に分かれており、こちらには来ない。
    fn parse_grok_stop(raw: &serde_json::Value, session_id: Option<String>) -> HookInput {
        // セッション種別を判別できる入力フィールドが無いため常にメイン扱い。
        let agent_message = raw
            .get("lastAssistantMessage")
            .or_else(|| raw.get("last_assistant_message"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());

        HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: crate::domain::ToolInput::Stop(crate::domain::StopInput {
                status: None,
                loop_count: None,
                response: None,
                agent_message,
                stop_hook_active: false,
                session_kind: crate::domain::StopSessionKind::Primary,
            }),
            session_id,
        }
    }

    /// Grok の SubagentStart / SubagentStop を内部 HookInput に変換する（NanoBuddy 通知用）。
    fn parse_grok_subagent(
        raw: &serde_json::Value,
        event: HookEvent,
        session_id: Option<String>,
    ) -> HookInput {
        let subagent_type = raw
            .get("subagentType")
            .or_else(|| raw.get("subagent_type"))
            .or_else(|| raw.get("agentType"))
            .or_else(|| raw.get("agent_type"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());
        let tool_name = if event == HookEvent::SubagentStart {
            "SubagentStart"
        } else {
            "SubagentStop"
        }
        .to_string();

        HookInput {
            event,
            tool_name,
            tool_input: crate::domain::ToolInput::Subagent(crate::domain::SubagentInput {
                subagent_type,
                prompt: raw
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string()),
                status: raw
                    .get("status")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string()),
                duration: raw.get("duration").and_then(|v| v.as_u64()),
            }),
            session_id,
        }
    }

    fn format_grok_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
        // PreToolUse 以外は事後フックで stdout が無視されるため、常に空オブジェクトを返す。
        // Stop フックの lint 失敗も Grok へは伝えられない（ブロック不可）ためベストエフォート。
        if event != HookEvent::BeforeCommand {
            return Ok("{}".to_string());
        }

        match decision {
            // 許可は exit 0 で伝わる。stdout には無害な空オブジェクトを返す
            // （公式に文書化されている decision 値は "deny" のみのため、
            // 未文書の "allow" を返して不正出力扱い＝フェイルオープンになるのを避ける）。
            Decision::Allow { .. } => Ok("{}".to_string()),
            Decision::Block { message } => {
                let truncated = self.normalize_and_truncate(message);
                serde_json::to_string(&serde_json::json!({
                    "decision": "deny",
                    "reason": truncated
                }))
                .map_err(|e| anyhow!("Failed to serialize Grok output: {}", e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の Codex 入力に、各イベントで必須のメタデータを補完する。
    fn complete_codex_input(input: &str) -> String {
        fn insert_default(
            object: &mut serde_json::Map<String, serde_json::Value>,
            field: &str,
            value: serde_json::Value,
        ) {
            object.entry(field.to_string()).or_insert(value);
        }

        let mut value: serde_json::Value = serde_json::from_str(input).unwrap();
        let object = value.as_object_mut().unwrap();
        let event = object
            .get("hook_event_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();

        insert_default(object, "session_id", serde_json::json!("test-session"));
        insert_default(object, "transcript_path", serde_json::Value::Null);
        insert_default(object, "cwd", serde_json::json!("/tmp/project"));
        insert_default(object, "model", serde_json::json!("gpt-5.4"));

        match event.as_str() {
            "SessionStart" => {
                insert_default(object, "permission_mode", serde_json::json!("default"));
            }
            "PreToolUse" => {
                insert_default(object, "turn_id", serde_json::json!("turn-1"));
                insert_default(object, "permission_mode", serde_json::json!("default"));
                insert_default(object, "tool_use_id", serde_json::json!("tool-1"));
            }
            "PermissionRequest" => {
                insert_default(object, "turn_id", serde_json::json!("turn-1"));
                insert_default(object, "permission_mode", serde_json::json!("default"));
            }
            "PostToolUse" => {
                insert_default(object, "turn_id", serde_json::json!("turn-1"));
                insert_default(object, "permission_mode", serde_json::json!("default"));
                insert_default(object, "tool_use_id", serde_json::json!("tool-1"));
                insert_default(object, "tool_response", serde_json::Value::Null);
            }
            "PreCompact" | "PostCompact" => {
                insert_default(object, "turn_id", serde_json::json!("turn-1"));
            }
            "UserPromptSubmit" | "SubagentStart" | "SubagentStop" | "Stop" => {
                insert_default(object, "turn_id", serde_json::json!("turn-1"));
                insert_default(object, "permission_mode", serde_json::json!("default"));
            }
            _ => {}
        }

        serde_json::to_string(&value).unwrap()
    }

    #[test]
    fn test_claude_input_parsing() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls -la"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
    }

    #[test]
    fn test_cursor_input_parsing_shell_execution() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let input = r#"{"hook_event_name":"beforeShellExecution","command":"rm -rf /tmp/test","cwd":"/path/to/project"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
        if let crate::domain::ToolInput::Bash(bash) = &result.tool_input {
            assert_eq!(bash.command, "rm -rf /tmp/test");
        } else {
            panic!("Expected Bash tool input");
        }
    }

    #[test]
    fn test_cursor_input_parsing_file_edit() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let input = r#"{"hook_event_name":"afterFileEdit","file_path":"/path/to/file.rs"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::AfterFileEdit);
        assert_eq!(result.tool_name, "Write");
        if let crate::domain::ToolInput::File(file) = &result.tool_input {
            assert_eq!(file.file_path, "/path/to/file.rs");
        } else {
            panic!("Expected File tool input");
        }
    }

    #[test]
    fn test_cursor_input_parsing_file_edit_camel_case() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        // Cursor は camelCase の filePath を送る場合もある
        let input = r#"{"hook_event_name":"afterFileEdit","filePath":"/path/to/file.tsx"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::AfterFileEdit);
        assert_eq!(result.tool_name, "Write");
        if let crate::domain::ToolInput::File(file) = &result.tool_input {
            assert_eq!(file.file_path, "/path/to/file.tsx");
        } else {
            panic!("Expected File tool input");
        }
    }

    #[test]
    fn test_windsurf_input_parsing_pre_run_command() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let input = r#"{"agent_action_name":"pre_run_command","tool_info":{"command_line":"rm -rf /tmp/test","cwd":"/path/to/project"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
        if let crate::domain::ToolInput::Bash(bash) = &result.tool_input {
            assert_eq!(bash.command, "rm -rf /tmp/test");
        } else {
            panic!("Expected Bash tool input");
        }
    }

    #[test]
    fn test_windsurf_input_parsing_post_write_code() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let input = r#"{"agent_action_name":"post_write_code","tool_info":{"file_path":"/path/to/file.rs"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::AfterFileEdit);
        assert_eq!(result.tool_name, "Write");
    }

    #[test]
    fn test_windsurf_input_parsing_pre_run_command_without_tool_info_is_error() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let input = r#"{"agent_action_name":"pre_run_command"}"#;
        let error = adapter.parse_input(input).unwrap_err().to_string();
        assert!(error.contains("Missing tool_info"));
    }

    #[test]
    fn test_windsurf_input_parsing_pre_run_command_without_command_line_is_error() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let input = r#"{"agent_action_name":"pre_run_command","tool_info":{}}"#;
        let error = adapter.parse_input(input).unwrap_err().to_string();
        assert!(error.contains("tool_info.command_line"));
    }

    #[test]
    fn test_windsurf_input_parsing_post_write_code_without_tool_info_is_error() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let input = r#"{"agent_action_name":"post_write_code"}"#;
        let error = adapter.parse_input(input).unwrap_err().to_string();
        assert!(error.contains("Missing tool_info"));
    }

    #[test]
    fn test_windsurf_input_parsing_post_write_code_with_empty_file_path_is_error() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let input = r#"{"agent_action_name":"post_write_code","tool_info":{"file_path":"   "}}"#;
        let error = adapter.parse_input(input).unwrap_err().to_string();
        assert!(error.contains("tool_info.file_path"));
    }

    #[test]
    fn test_cursor_output_allow() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::BeforeCommand)
            .unwrap();
        // claw-hooks は deny-only ポリシー。安全と判定しても明示 allow は返さず、
        // Cursor 本来の権限設定と他フックの判定を尊重する。
        assert_eq!(output, "{}");
    }

    #[test]
    fn test_cursor_output_deny() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "Command blocked for safety".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        assert!(output.contains(r#""permission":"deny""#));
        assert!(output.contains("Command blocked for safety"));
    }

    #[test]
    fn test_claude_output_allow() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::BeforeCommand)
            .unwrap();
        // BeforeCommand Allow は空オブジェクト（判定なし = 通常の権限フローに委ねる）。
        // permissionDecision "allow" を返すと Claude の権限プロンプトがスキップされる。
        assert_eq!(output, "{}");
    }

    #[test]
    fn test_claude_output_allow_with_context() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let decision = Decision::allow_with_context("Lint warning: unused variable".to_string());
        let output = adapter
            .format_output(&decision, HookEvent::AfterFileEdit)
            .unwrap();
        // PostToolUse Allow: トップレベル decision を含めない
        assert!(!output.contains(r#""decision""#));
        // AfterFileEdit で追加コンテキストがある場合のみ hookSpecificOutput が付く
        assert!(output.contains("hookSpecificOutput"));
        assert!(output.contains("additionalContext"));
        assert!(output.contains("Lint warning: unused variable"));
    }

    #[test]
    fn test_claude_output_block() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "Command blocked for safety".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        // PreToolUse Block: トップレベル decision ではなく hookSpecificOutput を使用
        assert!(!output.contains(r#""decision""#));
        assert!(output.contains(r#""permissionDecision":"deny""#));
        assert!(output.contains("Command blocked for safety"));
    }

    #[test]
    fn test_claude_output_block_before_command_json_structure() {
        // BeforeCommand Block の JSON に hookSpecificOutput.permissionDecision = "deny" が含まれる
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "rm is blocked".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let hso = &parsed["hookSpecificOutput"];
        assert_eq!(hso["hookEventName"], "PreToolUse");
        assert_eq!(hso["permissionDecision"], "deny");
        assert_eq!(hso["permissionDecisionReason"], "rm is blocked");
        // additionalContext は含まれない
        assert!(hso.get("additionalContext").is_none());
    }

    #[test]
    fn test_claude_output_after_file_edit_with_context_no_permission_decision() {
        // PostToolUse(AfterFileEdit) のコンテキスト出力に permissionDecision を含めない
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let decision = Decision::allow_with_context("warning: unused".to_string());
        let output = adapter
            .format_output(&decision, HookEvent::AfterFileEdit)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let hso = &parsed["hookSpecificOutput"];
        assert_eq!(hso["hookEventName"], "PostToolUse");
        assert_eq!(hso["additionalContext"], "warning: unused");
        // permissionDecision は含まれない
        assert!(hso.get("permissionDecision").is_none());
    }

    #[test]
    fn test_claude_output_stop_block_no_hook_specific_output() {
        // Stop Block の JSON に hookSpecificOutput が含まれない
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "lint errors".to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["decision"], "block");
        assert_eq!(parsed["reason"], "lint errors");
        assert!(parsed.get("hookSpecificOutput").is_none());
    }

    #[test]
    fn test_claude_output_before_command_block_truncates_permission_reason() {
        // output_max_length で permissionDecisionReason が切り詰められる
        let adapter = FormatAdapter::new(Format::Claude, 50);
        let long_msg = "a".repeat(100);
        let output = adapter
            .format_output(
                &Decision::Block { message: long_msg },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        assert!(
            reason.len() <= 50,
            "permissionDecisionReason は切り詰められるべき: len={}",
            reason.len()
        );
    }

    #[test]
    fn test_cursor_input_parsing_stop() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let input = r#"{"hook_event_name":"stop","status":"completed","loop_count":3}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
        assert_eq!(result.tool_name, "Stop");
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(stop.status, Some("completed".to_string()));
            assert_eq!(stop.loop_count, Some(3));
            assert!(stop.response.is_none());
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_cursor_input_parsing_stop_aborted() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let input = r#"{"hook_event_name":"stop","status":"aborted"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
        assert_eq!(result.tool_name, "Stop");
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(stop.status, Some("aborted".to_string()));
            assert!(stop.loop_count.is_none());
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_windsurf_input_parsing_post_cascade_response() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let input = r#"{"agent_action_name":"post_cascade_response","tool_info":{"response":"Task completed successfully."}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
        assert_eq!(result.tool_name, "Stop");
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert!(stop.status.is_none());
            assert!(stop.loop_count.is_none());
            assert_eq!(
                stop.response,
                Some("Task completed successfully.".to_string())
            );
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_stop() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        // Stopイベントにはtool_nameやtool_inputがない
        let input = r#"{"hook_event_name":"Stop","stop_hook_active":true}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
        assert_eq!(result.tool_name, "Stop");
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert!(stop.stop_hook_active);
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_stop_hook_active_false() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"Stop","stop_hook_active":false}"#;
        let result = adapter.parse_input(input).unwrap();
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert!(!stop.stop_hook_active);
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_stop_hook_active_unset_allows_stop_without_loop() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"Stop"}"#;
        let error = adapter.parse_input(input).unwrap_err();
        assert!(error.to_string().contains("stop_hook_active"));
        assert_eq!(
            adapter.format_error_for_input(&error.to_string(), input),
            "{}"
        );
        assert_eq!(adapter.error_exit_code(Some(input)), 0);
    }

    #[test]
    fn test_claude_input_parsing_stop_with_last_assistant_message() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"I've completed the refactoring task."}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(
                stop.agent_message,
                Some("I've completed the refactoring task.".to_string())
            );
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_stop_without_last_assistant_message() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"Stop","stop_hook_active":true}"#;
        let result = adapter.parse_input(input).unwrap();
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert!(stop.agent_message.is_none());
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_stop_with_agent_id_and_type_is_delegated() {
        // サブエージェントの Stop には agent_id と agent_type の両方が入る。
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"session_id":"62beaa88-5cdf-445c-b114-172ef29c3af0","hook_event_name":"Stop","stop_hook_active":false,"agent_id":"agent-123","agent_type":"general-purpose","last_assistant_message":"done"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(stop.session_kind, crate::domain::StopSessionKind::Delegated);
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_main_agent_type_without_agent_id_is_primary() {
        // `--agent` で起動したメインセッションは agent_type のみを持つ。
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"session_id":"1391e2cc-95c3-4a73-ad1a-51fd588e20be","hook_event_name":"Stop","stop_hook_active":false,"agent_type":"security-reviewer","last_assistant_message":"done"}"#;
        let result = adapter.parse_input(input).unwrap();
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(stop.session_kind, crate::domain::StopSessionKind::Primary);
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_stop_without_agent_type_is_primary() {
        // 通常のメインセッションの Stop には agent_type が入らない。
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"session_id":"1391e2cc-95c3-4a73-ad1a-51fd588e20be","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"done"}"#;
        let result = adapter.parse_input(input).unwrap();
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(stop.session_kind, crate::domain::StopSessionKind::Primary);
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_stop_with_blank_agent_type_is_primary() {
        // 空文字・空白のみの agent_type はメイン扱い（誤って Delegated と判定すると
        // 最終 lint / コミットが走らなくなるため、非空文字列のみ Delegated とする）
        let adapter = FormatAdapter::new(Format::Claude, 0);
        for agent_type in ["", "  "] {
            let input = format!(
                r#"{{"hook_event_name":"Stop","stop_hook_active":false,"agent_type":"{}"}}"#,
                agent_type
            );
            let result = adapter.parse_input(&input).unwrap();
            if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
                assert_eq!(
                    stop.session_kind,
                    crate::domain::StopSessionKind::Primary,
                    "agent_type={:?} should be Primary",
                    agent_type
                );
            } else {
                panic!("Expected Stop tool input");
            }
        }
    }

    #[test]
    fn test_claude_input_parsing_stop_with_non_string_agent_type_is_primary() {
        // 非文字列の agent_type は無視してメイン扱い（パースは失敗させない）
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"Stop","stop_hook_active":false,"agent_type":123}"#;
        let result = adapter.parse_input(input).unwrap();
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(stop.session_kind, crate::domain::StopSessionKind::Primary);
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_windsurf_input_parsing_post_cascade_response_sets_agent_message() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let input = r#"{"agent_action_name":"post_cascade_response","tool_info":{"response":"All tasks completed successfully."}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(
                stop.agent_message,
                Some("All tasks completed successfully.".to_string())
            );
            assert_eq!(stop.response, stop.agent_message);
        } else {
            panic!("Expected Stop tool input");
        }
    }

    // === Windsurf 出力のテスト ===

    #[test]
    fn test_windsurf_output_allow() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::BeforeCommand)
            .unwrap();
        // Allow: 空 JSON（decision 省略）
        assert_eq!(output, "{}");
        // WindsurfはhookSpecificOutputをサポートしない
        assert!(!output.contains("hookSpecificOutput"));
    }

    #[test]
    fn test_windsurf_output_block() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "Command blocked for safety".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        // Windsurf はブロックメッセージをプレーンテキスト本文として返す（JSON ではない）
        assert_eq!(output, "Command blocked for safety");
        assert!(!output.contains(r#""decision""#));
    }

    #[test]
    fn test_windsurf_output_block_strips_ansi_codes() {
        // Block メッセージに ANSI エスケープが含まれる場合、normalize_lint_output が
        // 適用されて ANSI を除去してから stderr に書かれることを保証する。
        // ANSI が混入したまま stderr に流れると Windsurf UI の表示が壊れる。
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "\x1b[31merror\x1b[0m: rm is blocked".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        assert!(!output.contains('\x1b'), "ANSI escapes should be stripped");
        assert!(output.contains("error: rm is blocked"));
    }

    #[test]
    fn test_windsurf_output_allow_after_file_edit() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        // WindsurfはadditionalContextをサポートしないため、コンテキストは無視される
        let decision = Decision::allow_with_context("Some lint warning".to_string());
        let output = adapter
            .format_output(&decision, HookEvent::AfterFileEdit)
            .unwrap();
        // Allow: 空 JSON（decision 省略）
        assert_eq!(output, "{}");
        // Windsurf は additionalContext をサポートしない
        assert!(!output.contains("hookSpecificOutput"));
        assert!(!output.contains("additionalContext"));
    }

    #[test]
    fn test_windsurf_error_format() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let error_output = adapter.format_error("Invalid JSON input");
        // Windsurf はエラーメッセージをプレーンテキストで返す（JSON ではない）
        assert!(error_output.contains("fail-closed"));
        assert!(error_output.contains("Invalid JSON input"));
        assert!(!error_output.contains(r#""decision""#));
    }

    // === Claude Code の Stop Block 出力 ===

    #[test]
    fn test_claude_output_stop_block_uses_reason_field() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "Stop hook failed: cargo clippy\nerror: unused variable".to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        // Stop の Block は "message" ではなく "reason" を使う
        assert!(output.contains(r#""decision":"block""#));
        assert!(output.contains(r#""reason":"#));
        assert!(!output.contains(r#""message""#));
        assert!(output.contains("unused variable"));
    }

    #[test]
    fn test_claude_output_stop_allow_unchanged() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::Stop)
            .unwrap();
        // Stop Allow では decision を省略（空 JSON: "{}"）
        assert!(!output.contains(r#""decision""#));
        assert!(!output.contains(r#""reason""#));
        assert_eq!(output, "{}");
    }

    #[test]
    fn test_claude_output_before_command_block_uses_hook_specific_output() {
        // PreToolUse Block: hookSpecificOutput.permissionDecision = "deny" のみ
        // トップレベル decision/reason は deprecated のため含めない
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "Command blocked".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        // トップレベルに decision/reason がないこと
        assert!(parsed.get("decision").is_none());
        assert!(parsed.get("reason").is_none());
        // hookSpecificOutput に deny が含まれること
        let hso = &parsed["hookSpecificOutput"];
        assert_eq!(hso["permissionDecision"], "deny");
        assert_eq!(hso["permissionDecisionReason"], "Command blocked");
    }

    #[test]
    fn test_normalize_strips_ansi_codes() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let message =
            "Stop hook failed: cargo clippy\n\x1b[31merror\x1b[0m: unused variable `x`".to_string();
        let output = adapter
            .format_output(&Decision::Block { message }, HookEvent::Stop)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["reason"].as_str().unwrap();
        assert!(reason.contains("error: unused variable `x`"));
        assert!(!reason.contains("\x1b"));
    }

    #[test]
    fn test_normalize_strips_leading_whitespace() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let message =
            "Stop hook failed: cargo clippy\n    error: unused\n        --> src/main.rs:1:1"
                .to_string();
        let output = adapter
            .format_output(&Decision::Block { message }, HookEvent::Stop)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["reason"].as_str().unwrap();
        // 各行の先頭の空白が除去されていること
        // `-->` は rustc 位置マーカー圧縮で `->` になる
        assert!(reason.contains("error: unused"));
        assert!(reason.contains("-> src/main.rs:1:1"));
        assert!(!reason.contains("    error"));
        assert!(!reason.contains("        ->"));
    }

    #[test]
    fn test_normalize_collapses_blank_lines() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let message = "error 1\n\n\n\nerror 2\n\n\nerror 3".to_string();
        let output = adapter
            .format_output(&Decision::Block { message }, HookEvent::Stop)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["reason"].as_str().unwrap();
        // 連続する空行は1行に圧縮されること
        assert_eq!(reason, "error 1\n\nerror 2\n\nerror 3");
    }

    #[test]
    fn test_normalize_preserves_simple_messages() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let message = "short lint error".to_string();
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: message.clone(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["reason"].as_str().unwrap();
        assert_eq!(reason, message);
    }

    // === Cursor / Windsurf の Stop Block 出力 ===

    #[test]
    fn test_cursor_output_stop_block_uses_followup_message() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "tsc --noEmit failed\nerror TS2322: Type 'string' is not assignable"
                        .to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        // Stop の Block は "permission" / "user_message" ではなく
        // "followup_message" を使う
        assert!(output.contains(r#""followup_message":"#));
        assert!(!output.contains(r#""permission""#));
        assert!(!output.contains(r#""user_message""#));
        assert!(output.contains("Type 'string' is not assignable"));
    }

    #[test]
    fn test_cursor_output_stop_allow_returns_empty_json() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::Stop)
            .unwrap();
        // Stop の出力スキーマは followup_message のみ。Allow は空オブジェクトを返す（permission は出さない）
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed, serde_json::json!({}));
        assert!(!output.contains(r#""permission""#));
        assert!(!output.contains(r#""followup_message""#));
    }

    #[test]
    fn test_cursor_output_stop_block_normalizes_output() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let message =
            "  \x1b[1;31merror\x1b[0m: type mismatch\n    expected `u32`\n\n\n    got `String`"
                .to_string();
        let output = adapter
            .format_output(&Decision::Block { message }, HookEvent::Stop)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let followup = parsed["followup_message"].as_str().unwrap();
        // ANSI 除去、先頭空白除去、連続空行圧縮が効くこと
        assert!(followup.contains("error: type mismatch"));
        assert!(!followup.contains("\x1b"));
        assert!(!followup.starts_with(' '));
        assert!(followup.contains("expected `u32`\n\ngot `String`"));
    }

    #[test]
    fn test_cursor_output_before_command_block_still_uses_deny() {
        // Stop以外のイベントは引き続き"permission":"deny"形式を使用する
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "Command blocked".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        assert!(output.contains(r#""permission":"deny""#));
        assert!(!output.contains(r#""followup_message""#));
    }

    #[test]
    fn test_windsurf_output_stop_allow_unchanged() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::Stop)
            .unwrap();
        // Stop の Allow は空 JSON（decision 省略）
        assert_eq!(output, "{}");
    }

    #[test]
    fn test_windsurf_output_stop_block_returns_empty_json_even_with_ansi() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let message = "  \x1b[31merror\x1b[0m: unused\n\n\n    --> src/main.rs:1:1".to_string();
        let output = adapter
            .format_output(&Decision::Block { message }, HookEvent::Stop)
            .unwrap();
        // 失敗内容は利用者向け出力へ出さず、空 JSON に固定する
        assert_eq!(output, "{}");
    }

    #[test]
    fn test_cursor_exit_code_stop_block_is_zero() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let decision = Decision::Block {
            message: "lint error".to_string(),
        };
        // Cursor の Stop Block は JSON 側で判定を返すため終了コード 0
        assert_eq!(adapter.exit_code(&decision, HookEvent::Stop), 0);
    }

    #[test]
    fn test_cursor_exit_code_before_command_block_is_zero() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let decision = Decision::Block {
            message: "blocked".to_string(),
        };
        // Cursor は exit 0 のときだけ stdout の JSON を使う。exit 2 だと deny 自体は
        // 成立しても user_message（safe-rm 等の代替案）がエージェントに届かない。
        assert_eq!(adapter.exit_code(&decision, HookEvent::BeforeCommand), 0);
    }

    #[test]
    fn test_windsurf_use_stderr_for_stop_block_is_false() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let block = Decision::Block {
            message: "error".to_string(),
        };
        assert!(!adapter.use_stderr(&block, HookEvent::Stop));
    }

    #[test]
    fn test_windsurf_use_stderr_for_before_command_block() {
        // Windsurf pre_run_command Block は exit code 2 + stderr を使う
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let block = Decision::Block {
            message: "error".to_string(),
        };
        assert!(adapter.use_stderr(&block, HookEvent::BeforeCommand));
    }

    #[test]
    fn test_claude_use_stdout_for_stop_block() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let block = Decision::Block {
            message: "error".to_string(),
        };
        assert!(!adapter.use_stderr(&block, HookEvent::Stop));
    }

    // === SubagentStart / SubagentStop のテスト ===

    #[test]
    fn test_claude_input_parsing_subagent_start() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"SubagentStart","tool_input":{"subagent_type":"explore","prompt":"Search the codebase"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStart);
        assert_eq!(result.tool_name, "SubagentStart");
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, Some("explore".to_string()));
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_subagent_stop() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"SubagentStop","tool_input":{"subagent_type":"generalPurpose","status":"completed","duration":5000}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStop);
        assert_eq!(result.tool_name, "SubagentStop");
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, Some("generalPurpose".to_string()));
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_subagent_start_no_tool_input() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"SubagentStart"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStart);
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, None);
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_subagent_start_root_agent_type() {
        // Claude Code は agent_type を tool_input ではなくルートに置く場合がある
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input =
            r#"{"hook_event_name":"SubagentStart","agent_id":"aa1c090","agent_type":"readme-en"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStart);
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, Some("readme-en".to_string()));
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_subagent_stop_root_agent_type() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"SubagentStop","agent_id":"aa1c090","agent_type":"readme-en","permission_mode":"bypassPermissions"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStop);
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, Some("readme-en".to_string()));
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_claude_input_parsing_subagent_start_no_agent_type() {
        // agent_type が無い場合は subagent_type = None
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"SubagentStart","agent_id":"aa1c090"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStart);
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, None);
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_cursor_input_parsing_subagent_start() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let input = r#"{"hook_event_name":"subagentStart","subagent_type":"explore","task":"Explore the authentication flow","subagent_model":"claude-sonnet-4-20250514"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStart);
        assert_eq!(result.tool_name, "SubagentStart");
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, Some("explore".to_string()));
            // 公式フィールド名 "task" から prompt が取得できること
            assert_eq!(
                sub.prompt,
                Some("Explore the authentication flow".to_string())
            );
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_cursor_input_parsing_subagent_stop() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let input = r#"{"hook_event_name":"subagentStop","subagent_type":"generalPurpose","status":"completed","summary":"Task done","duration_ms":45000}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(
            result.event,
            HookEvent::SubagentStop,
            "subagentStart ではなく subagentStop として解釈されるべき"
        );
        assert_eq!(result.tool_name, "SubagentStop");
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, Some("generalPurpose".to_string()));
            assert_eq!(sub.status, Some("completed".to_string()));
            // 公式フィールド名 "duration_ms" から duration が取得できること
            assert_eq!(sub.duration, Some(45000));
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_claude_output_subagent_start_allow() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::SubagentStart)
            .unwrap();
        // SubagentStart Allow: 空 JSON（decision 省略）
        assert_eq!(output, "{}");
    }

    #[test]
    fn test_claude_output_subagent_stop_allow() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::SubagentStop)
            .unwrap();
        // SubagentStop Allow: 空 JSON（decision 省略）
        assert_eq!(output, "{}");
    }

    #[test]
    fn test_cursor_output_subagent_stop_allow_is_empty_json() {
        // Cursor の subagentStop 出力スキーマは公式に `{ followup_message?: string }`
        // のみで、`permission` フィールドは存在しない。Stop と同じ扱いで Allow は `{}` を返す。
        // 回帰テスト: かつては `{"permission":"allow"}` を返しており公式スキーマと不一致だった。
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::SubagentStop)
            .unwrap();
        assert_eq!(output, "{}");
    }

    // === Codex CLI フォーマットのテスト ===

    #[test]
    fn test_codex_input_parsing_stop() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{"session_id":"019d193e-b16a-70e2-bd83-dc692a870e9a","transcript_path":"/tmp/rollout.jsonl","cwd":"/home/user/claw-hooks","hook_event_name":"Stop","model":"gpt-5.4","permission_mode":"bypassPermissions","stop_hook_active":false,"last_assistant_message":"OK"}"#;
        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();

        assert_eq!(result.event, HookEvent::Stop);
        assert_eq!(result.tool_name, "Stop");
        assert_eq!(
            result.session_id,
            Some("019d193e-b16a-70e2-bd83-dc692a870e9a".to_string())
        );
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(stop.agent_message, Some("OK".to_string()));
            assert!(!stop.stop_hook_active);
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_codex_input_parsing_subagent_start() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "SubagentStart",
            "session_id": "abc-123",
            "turn_id": "turn-1",
            "agent_id": "agent-1",
            "agent_type": "Explore",
            "permission_mode": "default"
        }"#;

        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStart);
        assert_eq!(result.tool_name, "SubagentStart");
        assert_eq!(result.session_id, Some("abc-123".to_string()));
        if let crate::domain::ToolInput::Subagent(subagent) = &result.tool_input {
            assert_eq!(subagent.subagent_type, Some("Explore".to_string()));
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_codex_input_parsing_subagent_stop() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "SubagentStop",
            "session_id": "abc-123",
            "turn_id": "turn-1",
            "agent_id": "agent-1",
            "agent_type": "Plan",
            "agent_transcript_path": "/tmp/subagent.jsonl",
            "stop_hook_active": false,
            "last_assistant_message": "Done"
        }"#;

        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::SubagentStop);
        assert_eq!(result.tool_name, "SubagentStop");
        assert_eq!(result.session_id, Some("abc-123".to_string()));
        if let crate::domain::ToolInput::Subagent(subagent) = &result.tool_input {
            assert_eq!(subagent.subagent_type, Some("Plan".to_string()));
        } else {
            panic!("Expected Subagent tool input");
        }
    }

    #[test]
    fn test_codex_output_allow_uses_empty_json() {
        // Codex: Allow は空 JSON を返す（公式ドキュメント推奨）
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::Stop)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert!(parsed.as_object().unwrap().is_empty());
    }

    #[test]
    fn test_codex_output_block_uses_reason_field() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "  \x1b[31merror\x1b[0m: unused variable  ".to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert_eq!(parsed["decision"], "block");
        assert_eq!(parsed["reason"], "error: unused variable");
        assert!(parsed.get("message").is_none());
    }

    #[test]
    fn test_codex_output_after_file_edit_allow_context() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter
            .format_output(
                &Decision::allow_with_context("formatted".to_string()),
                HookEvent::AfterFileEdit,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert_eq!(
            parsed["hookSpecificOutput"]["additionalContext"],
            "formatted"
        );
    }

    #[test]
    fn test_codex_output_permission_request_block_uses_deny_decision() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "rm is blocked".to_string(),
                },
                HookEvent::PermissionRequest,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert_eq!(
            parsed["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(parsed["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert_eq!(
            parsed["hookSpecificOutput"]["decision"]["message"],
            "rm is blocked"
        );
        assert!(parsed.get("decision").is_none());
        assert!(parsed.get("reason").is_none());
    }

    #[test]
    fn test_codex_permission_request_error_uses_deny_decision() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{}}"#;
        let output = adapter.format_error_for_input("Failed to parse input", input);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert_eq!(
            parsed["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(parsed["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert!(parsed.get("decision").is_none());
    }

    #[test]
    fn test_codex_pre_tool_use_error_uses_permission_decision() {
        // PreToolUse のパースエラーは推奨形式 hookSpecificOutput.permissionDecision="deny" で返す
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash"}"#;
        let output = adapter.format_error_for_input("Missing tool_input field", input);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            parsed["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("fail-closed")
        );
        assert!(parsed.get("decision").is_none());
    }

    #[test]
    fn test_codex_unknown_event_error_uses_legacy_block() {
        // イベント名が判別できない入力では legacy block 形式にフォールバックする
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter.format_error_for_input("Failed to parse input", "{not json");
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert_eq!(parsed["decision"], "block");
        assert!(parsed["reason"].as_str().unwrap().contains("fail-closed"));
    }

    #[test]
    fn test_codex_stop_block_keeps_legacy_format() {
        // Stop の Block は {"decision":"block","reason":...} が正式形式（継続プロンプト化される）
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "lint failed".to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert_eq!(parsed["decision"], "block");
        assert!(parsed["reason"].as_str().unwrap().contains("lint failed"));
        assert!(parsed.get("hookSpecificOutput").is_none());
    }

    #[test]
    fn test_codex_post_tool_use_block_keeps_legacy_format() {
        // PostToolUse の Block も {"decision":"block","reason":...} が正式形式
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "format failed".to_string(),
                },
                HookEvent::AfterFileEdit,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();

        assert_eq!(parsed["decision"], "block");
        assert!(parsed["reason"].as_str().unwrap().contains("format failed"));
        assert!(parsed.get("hookSpecificOutput").is_none());
    }

    #[test]
    fn test_codex_exit_code_block_is_zero() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let decision = Decision::Block {
            message: "blocked".to_string(),
        };

        assert_eq!(adapter.exit_code(&decision, HookEvent::BeforeCommand), 0);
        assert_eq!(
            adapter.exit_code(&decision, HookEvent::PermissionRequest),
            0
        );
        assert_eq!(adapter.exit_code(&decision, HookEvent::Stop), 0);
    }

    #[test]
    fn test_codex_input_parsing_session_start_passthrough() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "SessionStart",
            "session_id": "abc-123",
            "source": "startup",
            "cwd": "/tmp",
            "model": "gpt-5.4"
        }"#;

        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        // SessionStart は処理対象外 → Passthrough（パススルー）にマッピング
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "SessionStart");
    }

    #[test]
    fn test_codex_input_parsing_user_prompt_submit_passthrough() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "UserPromptSubmit",
            "session_id": "abc-123",
            "turn_id": "turn-1",
            "prompt": "fix the bug"
        }"#;

        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        // UserPromptSubmit は処理対象外 → Passthrough（パススルー）にマッピング
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "UserPromptSubmit");
    }

    #[test]
    fn test_codex_input_parsing_compact_events_passthrough() {
        let adapter = FormatAdapter::new(Format::Codex, 0);

        for event in ["PreCompact", "PostCompact"] {
            let input = format!(
                r#"{{
                    "hook_event_name": "{event}",
                    "session_id": "abc-123",
                    "turn_id": "turn-1",
                    "trigger": "manual"
                }}"#
            );

            let result = adapter.parse_input(&complete_codex_input(&input)).unwrap();
            // Codex のコンパクションイベントは claw-hooks の責務外なのでパススルーする。
            assert_eq!(result.event, HookEvent::Passthrough);
            assert_eq!(result.tool_name, event);
        }
    }

    #[test]
    fn test_codex_unknown_event_passthrough() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "SomeNewEvent",
            "session_id": "abc-123"
        }"#;

        // 未知イベントは Stop にフォールバックせず、パススルーになること
        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
    }

    #[test]
    fn test_codex_input_parsing_session_end_passthrough() {
        // SessionEnd は advisory なメインスレッド終了イベント（出力はエージェントに
        // 影響しない）。claw-hooks のスコープ外として明示的にパススルーする。
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "session_id": "thr_123",
            "transcript_path": "/workspace/.codex/rollout.jsonl",
            "cwd": "/workspace",
            "hook_event_name": "SessionEnd",
            "reason": "other"
        }"#;

        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "SessionEnd");
        assert_eq!(result.session_id, Some("thr_123".to_string()));
    }

    #[test]
    fn test_codex_input_parsing_pre_tool_use() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "rm -rf /tmp"},
            "session_id": "abc-123"
        }"#;

        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
    }

    #[test]
    fn test_codex_input_parsing_permission_request() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PermissionRequest",
            "session_id": "abc-123",
            "turn_id": "turn-1",
            "transcript_path": null,
            "cwd": "/tmp/project",
            "model": "gpt-5.4",
            "tool_name": "Bash",
            "tool_input": {"command": "rm -rf /tmp/build", "description": "cleanup"}
        }"#;

        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::PermissionRequest);
        assert_eq!(result.tool_name, "Bash");
        if let crate::domain::ToolInput::Bash(bash) = &result.tool_input {
            assert_eq!(bash.command, "rm -rf /tmp/build");
        } else {
            panic!("Expected Bash tool input");
        }
    }

    #[test]
    fn test_codex_input_parsing_post_tool_use() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "ls -la"},
            "tool_response": "file1.txt\nfile2.txt"
        }"#;

        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::AfterFileEdit);
        assert_eq!(result.tool_name, "Bash");
    }

    #[test]
    fn test_codex_parse_missing_hook_event_name_is_error() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "tool_name": "Bash",
            "tool_input": {"command": "ls"}
        }"#;

        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_codex_parse_missing_tool_name_is_error() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_input": {"command": "ls"}
        }"#;

        assert!(adapter.parse_input(&complete_codex_input(input)).is_err());
    }

    #[test]
    fn test_codex_parse_missing_tool_input_is_error() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash"
        }"#;

        assert!(adapter.parse_input(&complete_codex_input(input)).is_err());
    }

    #[test]
    fn test_codex_parse_missing_tool_input_command_is_error() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {}
        }"#;

        assert!(adapter.parse_input(&complete_codex_input(input)).is_err());
    }

    #[test]
    fn test_codex_error_output_uses_reason() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let error_output = adapter.format_error("test error");
        let parsed: serde_json::Value = serde_json::from_str(&error_output).unwrap();
        assert_eq!(parsed["decision"], "block");
        assert!(parsed["reason"].as_str().unwrap().contains("test error"));
        // "message" フィールドは存在しないこと
        assert!(parsed.get("message").is_none());
    }

    #[test]
    fn test_codex_error_exit_code_is_zero() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        assert_eq!(adapter.error_exit_code(None), 0);
    }

    // === Claude Code パススルーイベントテスト ===

    #[test]
    fn test_claude_session_start_passthrough() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{
            "hook_event_name": "SessionStart",
            "session_id": "sess-123"
        }"#;

        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "SessionStart");
    }

    #[test]
    fn test_claude_user_prompt_submit_passthrough() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{
            "hook_event_name": "UserPromptSubmit",
            "session_id": "sess-123"
        }"#;

        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "UserPromptSubmit");
    }

    // === エラーハンドリングのテスト ===

    #[test]
    fn test_claude_parse_missing_tool_input_is_error() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash"}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_claude_parse_missing_tool_name_is_error() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"PreToolUse","tool_input":{"command":"ls"}}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_claude_parse_unknown_event_is_passthrough() {
        // 未対応イベント（StopFailure, PreCompact, PermissionRequest など）は
        // claw-hooks のスコープ外なのでパススルーして Allow を返す。
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input =
            r#"{"hook_event_name":"Unknown","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
        let parsed = adapter
            .parse_input(input)
            .expect("未知イベントはパススルーされるべき");
        assert!(matches!(
            parsed.event,
            crate::domain::HookEvent::Passthrough
        ));
    }

    #[test]
    fn test_claude_parse_blank_event_is_error() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input =
            r#"{"hook_event_name":"   ","tool_name":"Bash","tool_input":{"command":"rm -rf /"}}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_claude_parse_invalid_json_is_error() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        assert!(adapter.parse_input("{").is_err());
        assert!(adapter.parse_input("not json").is_err());
    }

    #[test]
    fn test_cursor_parse_empty_object_is_error() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        assert!(adapter.parse_input(r#"{}"#).is_err());
    }

    #[test]
    fn test_cursor_parse_blank_event_is_error() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let input = r#"{"hook_event_name":"   ","command":"rm -rf /"}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_cursor_unsupported_event_passthrough() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        // afterShellExecution は claw-hooks の対象外でパススルー
        let input =
            r#"{"hook_event_name":"afterShellExecution","command":"echo test","output":"test"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "afterShellExecution");
    }

    #[test]
    fn test_cursor_pre_tool_use_shell_maps_to_before_command() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        // Cursor の現行 preToolUse は Shell ツールの command を渡す
        let input = r#"{"hook_event_name":"preToolUse","tool_name":"Shell","tool_input":{"command":"rm -rf /"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
        assert!(matches!(
            result.tool_input,
            crate::domain::ToolInput::Bash(crate::domain::BashInput { .. })
        ));
    }

    #[test]
    fn test_cursor_pre_tool_use_non_shell_passthrough() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        // command を持たないツールは claw-hooks の責務外としてパススルーする
        let input = r#"{"hook_event_name":"preToolUse","tool_name":"Read","tool_input":{"path":"README.md"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "Read");
    }

    #[test]
    fn test_cursor_after_shell_execution_not_confused_with_before() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        // afterShellExecution は command フィールドを持つが、
        // hook_event_name で正しく判別され、BeforeCommand にならないことを検証
        let input = r#"{"hook_event_name":"afterShellExecution","command":"rm -rf /tmp","output":"removed"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(
            result.event,
            HookEvent::Passthrough,
            "afterShellExecution が BeforeCommand に誤マッチしてはならない"
        );
        assert_ne!(result.event, HookEvent::BeforeCommand);
    }

    #[test]
    fn test_cursor_after_tab_file_edit() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        // afterTabFileEdit も afterFileEdit と同じ AfterFileEdit にマッピングされる
        let input = r#"{"hook_event_name":"afterTabFileEdit","file_path":"/path/to/file.rs"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::AfterFileEdit);
        assert_eq!(result.tool_name, "Write");
    }

    #[test]
    fn test_windsurf_unknown_action_is_passthrough_legacy_case() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let input = r#"{"agent_action_name":"unknown_action","tool_info":{}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "unknown_action");
    }

    #[test]
    fn test_windsurf_parse_invalid_json_is_error() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        assert!(adapter.parse_input("{invalid}").is_err());
    }

    #[test]
    fn test_cursor_error_format_fail_closed() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let output = adapter.format_error("Invalid JSON input");
        assert!(output.contains(r#""permission":"deny""#));
        assert!(output.contains("fail-closed"));
    }

    #[test]
    fn test_claude_error_format_fail_closed() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter.format_error("Parse error");
        // Claude は exit 2 のとき stderr 本文をプレーンテキストのエラーとして扱うため、JSON ではなく本文を返す
        assert_eq!(output, "🚫 Hook error (fail-closed): Parse error");
        assert!(output.contains("fail-closed"));
        assert!(!output.contains(r#""decision":"block""#));
    }

    #[test]
    fn test_error_exit_code() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        assert_eq!(adapter.error_exit_code(None), 2);
    }

    #[test]
    fn test_error_exit_code_codex_returns_zero() {
        // Codex CLI: 非0終了コードはフック失敗として扱われ判定が無視されるため、
        // エラー時も0を返しJSON内のblock判定を有効にする
        let adapter = FormatAdapter::new(Format::Codex, 0);
        assert_eq!(adapter.error_exit_code(None), 0);
    }

    #[test]
    fn test_error_exit_code_cursor_returns_two() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        assert_eq!(adapter.error_exit_code(None), 2);
    }

    #[test]
    fn test_error_exit_code_windsurf_returns_two() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        assert_eq!(adapter.error_exit_code(None), 2);
    }

    // === exit_code のテスト ===

    #[test]
    fn test_codex_exit_code_allow_always_zero() {
        // Codex CLI: Allow時は常に0
        let adapter = FormatAdapter::new(Format::Codex, 0);
        assert_eq!(
            adapter.exit_code(&Decision::allow(), HookEvent::BeforeCommand),
            0
        );
    }

    #[test]
    fn test_codex_exit_code_block_always_zero() {
        // Codex CLI: Block時もstdout JSONで判定を返すため常に0
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let decision = Decision::Block {
            message: "blocked".to_string(),
        };
        assert_eq!(adapter.exit_code(&decision, HookEvent::Stop), 0);
    }

    #[test]
    fn test_claude_exit_code_allow_before_command_zero() {
        // Claude: 許可 + BeforeCommand → 0
        let adapter = FormatAdapter::new(Format::Claude, 0);
        assert_eq!(
            adapter.exit_code(&Decision::allow(), HookEvent::BeforeCommand),
            0
        );
    }

    #[test]
    fn test_claude_exit_code_block_before_command_zero() {
        // Claude: JSON block 判定を stdout で返すため exit 0
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let decision = Decision::Block {
            message: "blocked".to_string(),
        };
        assert_eq!(adapter.exit_code(&decision, HookEvent::BeforeCommand), 0);
    }

    #[test]
    fn test_cursor_exit_code_allow_stop_zero() {
        // Cursor: Stop イベントの Allow は常に0
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        assert_eq!(adapter.exit_code(&Decision::allow(), HookEvent::Stop), 0);
    }

    #[test]
    fn test_cursor_exit_code_block_after_file_edit_zero() {
        // Cursor: 判定は全イベントで stdout の JSON 契約に統一する（終了コードは常に 0）
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let decision = Decision::Block {
            message: "blocked".to_string(),
        };
        assert_eq!(adapter.exit_code(&decision, HookEvent::AfterFileEdit), 0);
    }

    #[test]
    fn test_windsurf_exit_code_block_before_command_two() {
        // Windsurf: BeforeCommand の Block → 2
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let decision = Decision::Block {
            message: "blocked".to_string(),
        };
        assert_eq!(adapter.exit_code(&decision, HookEvent::BeforeCommand), 2);
    }

    // === error_exit_code のテスト ===

    #[test]
    fn test_error_exit_code_codex_zero() {
        // Codex: エラー時も0（JSON内でblock判定を有効にするため）
        let adapter = FormatAdapter::new(Format::Codex, 0);
        assert_eq!(adapter.error_exit_code(None), 0);
    }

    #[test]
    fn test_error_exit_code_claude_two() {
        // Claude: エラー時は2
        let adapter = FormatAdapter::new(Format::Claude, 0);
        assert_eq!(adapter.error_exit_code(None), 2);
    }

    #[test]
    fn test_error_exit_code_cursor_two() {
        // Cursor: エラー時は2
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        assert_eq!(adapter.error_exit_code(None), 2);
    }

    #[test]
    fn test_error_exit_code_windsurf_two() {
        // Windsurf: エラー時は2
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        assert_eq!(adapter.error_exit_code(None), 2);
    }

    // === use_stderr のテスト ===

    #[test]
    fn test_windsurf_use_stderr_stop_block_false() {
        // Windsurf + Stop + Block → stderr を使用しない
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let decision = Decision::Block {
            message: "lint error".to_string(),
        };
        assert!(!adapter.use_stderr(&decision, HookEvent::Stop));
    }

    #[test]
    fn test_windsurf_use_stderr_stop_allow_false() {
        // Windsurf + Stop + Allow → stderr を使用しない
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        assert!(!adapter.use_stderr(&Decision::allow(), HookEvent::Stop));
    }

    #[test]
    fn test_windsurf_use_stderr_before_command_block_true() {
        // Windsurf + BeforeCommand + Block → stderr を使用する（exit code 2 でブロック）
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let decision = Decision::Block {
            message: "blocked".to_string(),
        };
        assert!(adapter.use_stderr(&decision, HookEvent::BeforeCommand));
    }

    #[test]
    fn test_windsurf_use_stderr_after_file_edit_block_true() {
        // Windsurf + AfterFileEdit + Block → stderr を使用する。
        // 公式仕様では exit 2 のエラーメッセージは post_write_code でも stderr から
        // 読まれる（post-hook はブロック不可だがメッセージはエージェントに見える）。
        // stdout に書くと exit 2 時に読まれず lint 結果が失われる。
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let decision = Decision::Block {
            message: "error".to_string(),
        };
        assert!(adapter.use_stderr(&decision, HookEvent::AfterFileEdit));
    }

    #[test]
    fn test_windsurf_use_stderr_after_file_edit_allow_false() {
        // Windsurf + AfterFileEdit + Allow → stderr を使用しない
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        assert!(!adapter.use_stderr(&Decision::allow(), HookEvent::AfterFileEdit));
    }

    #[test]
    fn test_windsurf_use_stderr_before_command_allow_false() {
        // Windsurf + BeforeCommand + Allow → stderr を使用しない
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        assert!(!adapter.use_stderr(&Decision::allow(), HookEvent::BeforeCommand));
    }

    #[test]
    fn test_claude_use_stderr_stop_block_false() {
        // Claude + Stop + Block → stderr を使用しない
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let decision = Decision::Block {
            message: "error".to_string(),
        };
        assert!(!adapter.use_stderr(&decision, HookEvent::Stop));
    }

    #[test]
    fn test_non_windsurf_use_stderr_always_false() {
        // Windsurf 以外のフォーマットでは use_stderr は常に false
        let block = Decision::Block {
            message: "err".to_string(),
        };
        for format in [Format::Claude, Format::Cursor, Format::Codex] {
            let adapter = FormatAdapter::new(format, 0);
            for event in [
                HookEvent::BeforeCommand,
                HookEvent::Stop,
                HookEvent::AfterFileEdit,
            ] {
                assert!(
                    !adapter.use_stderr(&block, event),
                    "{:?} + {:?} + Block で use_stderr は false であるべき",
                    format,
                    event
                );
                assert!(
                    !adapter.use_stderr(&Decision::allow(), event),
                    "{:?} + {:?} + Allow + use_stderr は false であるべき",
                    format,
                    event
                );
            }
        }
    }

    // === format_error のテスト ===

    #[test]
    fn test_format_error_claude_is_plain_text() {
        // Claude: exit 2 では JSON が無視されるため、プレーンテキスト本文のみを返す
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter.format_error("test error");
        assert_eq!(output, "🚫 Hook error (fail-closed): test error");
        assert!(output.contains("fail-closed"));
        // JSON の decision/reason キーは含まない
        assert!(!output.contains(r#""decision""#));
        assert!(!output.contains(r#""reason""#));
    }

    #[test]
    fn test_format_error_cursor_contains_deny_and_user_message() {
        // Cursor: "deny" と "user_message" を含む
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let output = adapter.format_error("test error");
        assert!(output.contains("deny"));
        assert!(output.contains("user_message"));
        assert!(output.contains("fail-closed"));
    }

    #[test]
    fn test_format_error_codex_contains_block_and_reason() {
        // Codex: "block" と "reason" を含む
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter.format_error("test error");
        assert!(output.contains("block"));
        assert!(output.contains("reason"));
        assert!(output.contains("fail-closed"));
    }

    #[test]
    fn test_format_error_all_formats_contain_fail_closed() {
        // すべてのフォーマットで "fail-closed" を含む
        for format in [
            Format::Claude,
            Format::Cursor,
            Format::Windsurf,
            Format::Codex,
        ] {
            let adapter = FormatAdapter::new(format, 0);
            let output = adapter.format_error("some error");
            assert!(
                output.contains("fail-closed"),
                "{:?} の format_error に fail-closed が含まれていない",
                format
            );
        }
    }

    // === format_uses_stderr_for_errors のテスト ===

    #[test]
    fn test_windsurf_uses_stderr_for_errors() {
        // Windsurf はブロック時に stderr からメッセージを読むため true
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        assert!(adapter.format_uses_stderr_for_errors());
    }

    #[test]
    fn test_claude_uses_stderr_for_errors() {
        // Claude の exit 2 フェイルクローズでは stdout JSON が無視されるため stderr に出す
        let adapter = FormatAdapter::new(Format::Claude, 0);
        assert!(adapter.format_uses_stderr_for_errors());
    }

    #[test]
    fn test_json_error_formats_use_stdout_for_errors() {
        // Cursor/Codex/Antigravity はエラー判定を stdout JSON で伝達する
        for format in [Format::Cursor, Format::Codex, Format::Agy] {
            let adapter = FormatAdapter::new(format, 0);
            assert!(
                !adapter.format_uses_stderr_for_errors(),
                "{:?} は stderr を使わないべき",
                format
            );
        }
    }

    // === Codex CLI のテスト ===

    #[test]
    fn test_codex_input_pre_tool_use_bash_maps_to_before_command() {
        // Codex: PreToolUse + Bash は BeforeCommand に変換
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hello"},
            "session_id": "test-session"
        }"#;
        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
        if let crate::domain::ToolInput::Bash(bash) = &result.tool_input {
            assert_eq!(bash.command, "echo hello");
        } else {
            panic!("Expected Bash tool input");
        }
    }

    #[test]
    fn test_codex_input_post_tool_use_bash_maps_to_after_file_edit() {
        // Codex: PostToolUse + Bash はコマンド出力確認用の AfterFileEdit として扱う
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "cat file.txt"},
            "tool_response": "file contents"
        }"#;
        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::AfterFileEdit);
        assert_eq!(result.tool_name, "Bash");
    }

    #[test]
    fn test_codex_input_post_tool_use_apply_patch_maps_to_multi_edit_files() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PostToolUse",
            "tool_name": "apply_patch",
            "tool_input": {
                "command": "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n old\n new\n*** Add File: tests/new_test.rs\n+test\n*** End Patch\n"
            }
        }"#;
        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::AfterFileEdit);
        assert_eq!(result.tool_name, "MultiEdit");
        if let crate::domain::ToolInput::Files(files) = &result.tool_input {
            let paths: Vec<&str> = files.iter().map(|file| file.file_path.as_str()).collect();
            assert_eq!(paths, vec!["src/lib.rs", "tests/new_test.rs"]);
        } else {
            panic!("apply_patch は ToolInput::Files に変換されるべき");
        }
    }

    #[test]
    fn test_codex_input_post_tool_use_apply_patch_delete_only_has_no_files() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PostToolUse",
            "tool_name": "apply_patch",
            "tool_input": {
                "command": "*** Begin Patch\n*** Delete File: src/removed.rs\n*** End Patch\n"
            }
        }"#;
        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::AfterFileEdit);
        assert_eq!(result.tool_name, "MultiEdit");
        if let crate::domain::ToolInput::Files(files) = &result.tool_input {
            assert!(
                files.is_empty(),
                "削除だけの patch では保存後フック対象ファイルを持たない"
            );
        } else {
            panic!("apply_patch は ToolInput::Files に変換されるべき");
        }
    }

    #[test]
    fn test_codex_input_pre_tool_use_apply_patch_parses_without_file_path() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "apply_patch",
            "tool_input": {
                "command": "*** Begin Patch\n*** Update File: src/main.rs\n@@\n old\n new\n*** End Patch\n"
            }
        }"#;
        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "MultiEdit");
        assert!(matches!(
            result.tool_input,
            crate::domain::ToolInput::Files(_)
        ));
    }

    #[test]
    fn test_codex_input_empty_command_in_apply_patch_tool_is_error() {
        // apply_patch の command が空文字列の場合は Bash 経路と同様に fail-closed にする
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "apply_patch",
            "tool_input": { "command": "" }
        }"#;
        let err = adapter
            .parse_input(&complete_codex_input(input))
            .unwrap_err();
        assert!(
            err.to_string().contains("command"),
            "空コマンドは command 関連のエラーになるべき: {}",
            err
        );
    }

    #[test]
    fn test_codex_optional_metadata_absence_is_tolerated() {
        // 公式仕様の共通フィールド表は「通常使うフィールド」の一覧であって厳密なスキーマ
        // ではない（仕様中の SessionEnd 実例ペイロードには model が無い）。
        // 判定に使わないメタデータを必須にすると、Codex 側がフィールドを省略した瞬間に
        // 全イベントがフェイルクローズドに倒れて claw-hooks 全体が停止する。
        // そのため欠落を許容し、パースを成功させる。
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let complete = complete_codex_input(
            r#"{
                "hook_event_name": "Stop",
                "stop_hook_active": false,
                "last_assistant_message": null
            }"#,
        );

        for field in [
            "session_id",
            "transcript_path",
            "cwd",
            "model",
            "turn_id",
            "permission_mode",
            "last_assistant_message",
        ] {
            let mut input: serde_json::Value = serde_json::from_str(&complete).unwrap();
            input.as_object_mut().unwrap().remove(field);
            let parsed = adapter
                .parse_input(&input.to_string())
                .unwrap_or_else(|e| panic!("{field} 欠落でフェイルクローズドすべきではない: {e}"));
            assert_eq!(parsed.event, HookEvent::Stop);
        }
    }

    #[test]
    fn test_codex_used_fields_are_still_validated() {
        // 逆に、claw-hooks が実際に判定に使うフィールドは厳密に検証する。
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let cases = [
            // PreToolUse のツール名・コマンド本文は危険コマンド検出の入力そのもの
            (
                complete_codex_input(
                    r#"{"hook_event_name":"PreToolUse","tool_input":{"command":"ls"}}"#,
                ),
                "tool_name",
            ),
            (
                complete_codex_input(r#"{"hook_event_name":"PreToolUse","tool_name":"Bash"}"#),
                "tool_input",
            ),
            // Stop の継続判断に使う唯一のフィールド
            (
                complete_codex_input(r#"{"hook_event_name":"Stop"}"#),
                "stop_hook_active",
            ),
        ];

        for (input, field) in cases {
            let err = adapter.parse_input(&input).unwrap_err();
            assert!(
                err.to_string().contains(field),
                "{field} 欠落時のエラーが不適切: {err}"
            );
        }

        // 型不一致も検出する
        let mut input: serde_json::Value = serde_json::from_str(&complete_codex_input(
            r#"{"hook_event_name":"Stop","stop_hook_active":false}"#,
        ))
        .unwrap();
        input["stop_hook_active"] = serde_json::json!("false");
        let err = adapter.parse_input(&input.to_string()).unwrap_err();
        assert!(err.to_string().contains("stop_hook_active"), "{err}");

        // PreToolUse の Bash で command が空文字なら fail-closed
        let input = complete_codex_input(
            r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"  "}}"#,
        );
        let err = adapter.parse_input(&input).unwrap_err();
        assert!(err.to_string().contains("command"), "{err}");
    }

    #[test]
    fn test_codex_passthrough_events_skip_field_validation() {
        // スコープ外イベントの固有フィールドは一切検証しない。
        // 検証してフェイルクローズドすると、読んでもいないメタデータのために
        // UserPromptSubmit ではユーザーのプロンプト自体を拒否してしまう
        // （UserPromptSubmit では decision:"block" がプロンプト拒否の正式形式）。
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let inputs = [
            r#"{"hook_event_name":"SessionStart"}"#,
            r#"{"hook_event_name":"SessionEnd"}"#,
            r#"{"hook_event_name":"UserPromptSubmit"}"#,
            r#"{"hook_event_name":"PreCompact"}"#,
            r#"{"hook_event_name":"PostCompact"}"#,
            r#"{"hook_event_name":"SomeFutureEvent"}"#,
        ];

        for input in inputs {
            let parsed = adapter
                .parse_input(input)
                .unwrap_or_else(|e| panic!("{input} はパススルーすべき: {e}"));
            assert_eq!(parsed.event, HookEvent::Passthrough, "input: {input}");
        }
    }

    #[test]
    fn test_codex_post_tool_use_non_file_tool_is_tolerated() {
        // Codex の PostToolUse では decision:"block" が「実際のツール出力を
        // フックのメッセージで置き換える」動作になる。保存後フックはファイルパスしか
        // 使わないため、Bash など非ファイル系ツールの tool_input を厳密検証して
        // フェイルクローズドすると、モデルが本来のコマンド出力を失う。
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = complete_codex_input(
            r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{}}"#,
        );

        let parsed = adapter.parse_input(&input).unwrap();
        assert_eq!(parsed.event, HookEvent::AfterFileEdit);
        assert_eq!(parsed.tool_name, "Bash");
    }

    #[test]
    fn test_codex_unknown_tool_name_is_not_mapped_to_write() {
        // 未知のツール名を Write に寄せると file_path が必須化され、引数キーが
        // path / filename 等の関数ツールに対して PreToolUse の誤 deny が起きる。
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = complete_codex_input(
            r#"{"hook_event_name":"PreToolUse","tool_name":"write_file","tool_input":{"path":"/tmp/a.txt"}}"#,
        );

        let parsed = adapter
            .parse_input(&input)
            .expect("未知のツール名は素通しすべき");
        assert_eq!(parsed.tool_name, "write_file");
        assert!(matches!(
            parsed.tool_input,
            crate::domain::ToolInput::Other(_)
        ));
    }

    #[test]
    fn test_codex_apply_patch_move_uses_destination_path() {
        let paths = FormatAdapter::extract_apply_patch_paths(
            "*** Begin Patch\n*** Update File: src/old.rs\n*** Move to: src/new.rs\n@@\n old\n new\n*** Delete File: src/deleted.rs\n*** End Patch\n",
        );
        assert_eq!(paths, vec!["src/new.rs"]);
    }

    #[test]
    fn test_codex_input_stop_event() {
        // Codex: Stop イベント
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "Stop",
            "session_id": "test-session",
            "stop_hook_active": true,
            "last_assistant_message": "Done"
        }"#;
        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
        assert_eq!(result.tool_name, "Stop");
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert!(stop.stop_hook_active);
            assert_eq!(stop.agent_message, Some("Done".to_string()));
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_codex_output_allow() {
        // Codex: Allow は空 JSON を返す
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::BeforeCommand)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.as_object().unwrap().is_empty());
    }

    #[test]
    fn test_codex_output_block() {
        // Codex: PreToolUse の Block → 推奨形式 hookSpecificOutput.permissionDecision="deny"
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "dangerous command".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let hso = &parsed["hookSpecificOutput"];
        assert_eq!(hso["hookEventName"], "PreToolUse");
        assert_eq!(hso["permissionDecision"], "deny");
        assert!(
            hso["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("dangerous command")
        );
        // legacy のトップレベル decision/reason は含めない
        assert!(parsed.get("decision").is_none());
        assert!(parsed.get("reason").is_none());
    }

    #[test]
    fn test_codex_error_format() {
        // Codex: エラーフォーマット → "block" と "reason" と "fail-closed" を含む
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter.format_error("parse failed");
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["decision"], "block");
        let reason = parsed["reason"].as_str().unwrap();
        assert!(reason.contains("parse failed"));
        assert!(reason.contains("fail-closed"));
    }

    // === Claude パススルーイベントのテスト ===

    #[test]
    fn test_claude_session_start_maps_to_passthrough() {
        // SessionStart → Passthrough にマップ
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name": "SessionStart", "session_id": "s1"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "SessionStart");
    }

    #[test]
    fn test_claude_user_prompt_submit_maps_to_passthrough() {
        // UserPromptSubmit → Passthrough にマップ
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name": "UserPromptSubmit", "session_id": "s1"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "UserPromptSubmit");
    }

    #[test]
    fn test_claude_session_end_maps_to_passthrough() {
        // SessionEnd → Passthrough にマップ
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name": "SessionEnd", "session_id": "s1"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "SessionEnd");
    }

    #[test]
    fn test_claude_notification_maps_to_passthrough() {
        // Notification → Passthrough にマップ
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name": "Notification", "session_id": "s1"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "Notification");
    }

    // === output_max_length のテスト ===

    #[test]
    fn test_output_max_length_truncates_long_output() {
        // max_length > 0 のとき長い出力が切り詰められる（PreToolUse: hookSpecificOutput 内）
        let adapter = FormatAdapter::new(Format::Claude, 20);
        let long_message = "a".repeat(100);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: long_message.clone(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        // 元のメッセージより短く切り詰められていること
        assert!(
            reason.len() < long_message.len(),
            "メッセージが切り詰められていない: {} >= {}",
            reason.len(),
            long_message.len()
        );
    }

    #[test]
    fn test_output_max_length_zero_no_truncation() {
        // max_length = 0 のとき出力が切り詰められない（PreToolUse: hookSpecificOutput 内）
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let long_message = "a".repeat(10000);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: long_message.clone(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        // 元のメッセージがそのまま含まれていること
        assert_eq!(reason, long_message);
    }

    // === Windsurf Stop 出力テスト ===

    #[test]
    fn test_windsurf_output_stop_block_returns_empty_json() {
        // Windsurf の Stop は事後フックのため、失敗しても空 JSON を返す
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "cargo clippy failed\nerror: unused variable".to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.as_object().unwrap().is_empty());
    }

    #[test]
    fn test_windsurf_output_stop_allow_returns_json() {
        // Windsurf の Stop Allow は空 JSON
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::Stop)
            .unwrap();
        assert_eq!(output, "{}");
    }

    #[test]
    fn test_windsurf_output_stop_block_discards_message() {
        // Stop の失敗はエージェントへ返さず、出力は常に空 JSON のままにする
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "\x1b[31merror\x1b[0m: failed".to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        assert_eq!(output, "{}");
    }

    // === Codex Stop 出力テスト ===

    #[test]
    fn test_codex_output_stop_block_uses_reason() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "lint errors found".to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["decision"], "block");
        assert!(
            parsed["reason"]
                .as_str()
                .unwrap()
                .contains("lint errors found")
        );
    }

    #[test]
    fn test_codex_output_stop_allow() {
        // Codex: Stop Allow も空 JSON
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::Stop)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed.as_object().unwrap().is_empty());
    }

    // === exit_code エッジケーステスト ===

    #[test]
    fn test_windsurf_exit_code_stop_block_is_zero() {
        // post_cascade_response は事後フックのため、Stop の失敗でも exit code は 0
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let code = adapter.exit_code(
            &Decision::Block {
                message: "errors".to_string(),
            },
            HookEvent::Stop,
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn test_claude_exit_code_stop_block_is_zero() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let code = adapter.exit_code(
            &Decision::Block {
                message: "errors".to_string(),
            },
            HookEvent::Stop,
        );
        assert_eq!(code, 0);
    }

    // === use_stderr エッジケーステスト ===

    #[test]
    fn test_codex_use_stderr_always_false() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        assert!(!adapter.use_stderr(
            &Decision::Block {
                message: "err".to_string()
            },
            HookEvent::Stop,
        ));
        assert!(!adapter.use_stderr(&Decision::allow(), HookEvent::BeforeCommand));
    }

    #[test]
    fn test_cursor_use_stderr_always_false() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        assert!(!adapter.use_stderr(
            &Decision::Block {
                message: "err".to_string()
            },
            HookEvent::Stop,
        ));
    }

    // === Codex 入力パースエッジケース ===

    #[test]
    fn test_codex_input_empty_json_is_error() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let err = adapter.parse_input("{}").unwrap_err();
        assert!(err.to_string().contains("hook_event_name"));
    }

    #[test]
    fn test_codex_input_invalid_json_is_error() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let err = adapter.parse_input("not json").unwrap_err();
        assert!(err.to_string().contains("parse"));
    }

    // === output_max_length が各フォーマットの Stop 出力に効くことを確認 ===

    #[test]
    fn test_output_max_length_truncates_windsurf_stop_block() {
        let adapter = FormatAdapter::new(Format::Windsurf, 100);
        let long_message = "e".repeat(500);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: long_message.clone(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        assert!(output.len() < long_message.len());
    }

    #[test]
    fn test_output_max_length_truncates_codex_stop_block() {
        let adapter = FormatAdapter::new(Format::Codex, 100);
        let long_message = "e".repeat(500);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: long_message.clone(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["reason"].as_str().unwrap();
        assert!(reason.len() < long_message.len());
    }

    #[test]
    fn test_output_max_length_truncates_cursor_stop_block() {
        let adapter = FormatAdapter::new(Format::Cursor, 100);
        let long_message = "e".repeat(500);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: long_message.clone(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let followup = parsed["followup_message"].as_str().unwrap();
        assert!(followup.len() < long_message.len());
    }

    // === Claude Code 入力パースのエッジケース ===

    #[test]
    fn test_claude_input_missing_tool_name_is_error() {
        // PreToolUse で tool_name が無い場合はエラー
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"PreToolUse","tool_input":{"command":"ls"}}"#;
        let err = adapter.parse_input(input).unwrap_err();
        assert!(
            err.to_string().contains("tool_name"),
            "tool_name 欠落エラーのメッセージが不適切: {}",
            err
        );
    }

    #[test]
    fn test_claude_input_blank_tool_name_is_error() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"PreToolUse","tool_name":"   ","tool_input":{"command":"rm -rf /"}}"#;
        let err = adapter.parse_input(input).unwrap_err();
        assert!(err.to_string().contains("tool_name"));
    }

    #[test]
    fn test_claude_input_missing_tool_input_is_error() {
        // PreToolUse で tool_input が無い場合はエラー
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash"}"#;
        let err = adapter.parse_input(input).unwrap_err();
        assert!(
            err.to_string().contains("tool_input"),
            "tool_input 欠落エラーのメッセージが不適切: {}",
            err
        );
    }

    #[test]
    fn test_claude_bash_empty_object_is_error() {
        // 空 tool_input が StopInput に誤マッチして fail-open しないこと
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{}}"#;
        let err = adapter.parse_input(input).unwrap_err();
        assert!(err.to_string().contains("command"));
    }

    #[test]
    fn test_claude_write_empty_object_is_error() {
        // ファイル編集後フックでは file_path が必須
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"PostToolUse","tool_name":"Write","tool_input":{}}"#;
        let err = adapter.parse_input(input).unwrap_err();
        assert!(err.to_string().contains("Claude tool_input"));
    }

    #[test]
    fn test_claude_input_unknown_event_is_passthrough() {
        // 未対応イベントはパススルーで Allow を返す（fail-open）。
        // Cursor/Codex/Antigravity と同じ挙動に揃えることでフォーマット間の一貫性を保つ。
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"UnknownEvent","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
        let parsed = adapter
            .parse_input(input)
            .expect("未知イベントはパススルーされるべき");
        assert!(matches!(
            parsed.event,
            crate::domain::HookEvent::Passthrough
        ));
    }

    #[test]
    fn test_claude_input_invalid_json_is_error() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let err = adapter.parse_input("{invalid json}").unwrap_err();
        assert!(
            err.to_string().contains("parse"),
            "JSON パースエラーのメッセージが不適切: {}",
            err
        );
    }

    #[test]
    fn test_claude_input_passthrough_events() {
        // SessionStart, UserPromptSubmit, SessionEnd, Notification はパススルー
        let adapter = FormatAdapter::new(Format::Claude, 0);
        for event_name in [
            "SessionStart",
            "UserPromptSubmit",
            "SessionEnd",
            "Notification",
        ] {
            let input = format!(r#"{{"hook_event_name":"{}"}}"#, event_name);
            let result = adapter.parse_input(&input).unwrap();
            assert_eq!(
                result.event,
                HookEvent::Passthrough,
                "パススルーイベント {} は Passthrough にマッピングされるべき",
                event_name
            );
            assert_eq!(result.tool_name, event_name);
        }
    }

    // === Windsurf 入力パースのエッジケース ===

    #[test]
    fn test_windsurf_input_unknown_action_is_passthrough() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let input = r#"{"agent_action_name":"unknown_action","tool_info":{}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "unknown_action");
    }

    #[test]
    fn test_windsurf_input_pre_run_command_empty_command_is_error() {
        // コマンドが空文字列の場合はエラー
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let input = r#"{"agent_action_name":"pre_run_command","tool_info":{"command_line":""}}"#;
        let err = adapter.parse_input(input).unwrap_err();
        assert!(err.to_string().contains("command_line"));
    }

    #[test]
    fn test_cursor_before_shell_blank_command_is_error() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let input = r#"{"hook_event_name":"beforeShellExecution","command":"   "}"#;
        let err = adapter.parse_input(input).unwrap_err();
        assert!(err.to_string().contains("command"));
    }

    #[test]
    fn test_windsurf_input_blank_action_name_is_error() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let input =
            r#"{"agent_action_name":"   ","tool_info":{"command_line":"rm -rf /tmp/test"}}"#;
        let err = adapter.parse_input(input).unwrap_err();
        assert!(err.to_string().contains("agent_action_name"));
    }

    #[test]
    fn test_windsurf_input_post_cascade_response_without_response() {
        // response フィールドがなくても Stop として処理できること
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let input = r#"{"agent_action_name":"post_cascade_response","tool_info":{}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
    }

    // === Codex 入力パースのエッジケース ===

    #[test]
    fn test_codex_input_empty_event_name_is_error() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{"hook_event_name":""}"#;
        let err = adapter.parse_input(input).unwrap_err();
        assert!(err.to_string().contains("hook_event_name"));
    }

    #[test]
    fn test_codex_input_blank_event_name_is_error() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = complete_codex_input(r#"{"hook_event_name":"   "}"#);
        let err = adapter.parse_input(&input).unwrap_err();
        assert!(err.to_string().contains("hook_event_name"));
    }

    #[test]
    fn test_codex_event_alias_used_when_primary_field_is_invalid() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = complete_codex_input(
            r#"{
                "hook_event_name":42,
                "event":"PreToolUse",
                "turn_id":"turn-1",
                "permission_mode":"default",
                "tool_use_id":"tool-1",
                "tool_name":"Bash",
                "tool_input":{"command":"ls"}
            }"#,
        );
        let result = adapter.parse_input(&input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
    }

    #[test]
    fn test_codex_input_missing_tool_name_for_tool_event_is_error() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{"hook_event_name":"PreToolUse","tool_input":{"command":"ls"}}"#;
        let err = adapter
            .parse_input(&complete_codex_input(input))
            .unwrap_err();
        assert!(err.to_string().contains("tool_name"));
    }

    #[test]
    fn test_codex_input_missing_command_in_bash_tool_is_error() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{}}"#;
        let err = adapter
            .parse_input(&complete_codex_input(input))
            .unwrap_err();
        assert!(err.to_string().contains("command"));
    }

    #[test]
    fn test_codex_input_unknown_event_passthrough() {
        // 未知のイベントはパススルーとして処理される
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{"hook_event_name":"FutureEvent","data":"test"}"#;
        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "FutureEvent");
    }

    #[test]
    fn test_codex_input_stop_with_stop_hook_active() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input =
            r#"{"hook_event_name":"Stop","stop_hook_active":true,"last_assistant_message":"done"}"#;
        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert!(stop.stop_hook_active);
            assert_eq!(stop.agent_message, Some("done".to_string()));
        } else {
            panic!("Expected Stop tool input");
        }
    }

    // === format_error の各フォーマット出力テスト ===

    #[test]
    fn test_format_error_claude_is_not_json() {
        // Claude の fail-closed は exit 2 で JSON が無視されるためプレーンテキスト本文（JSON ではない）
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter.format_error("test error");
        assert!(serde_json::from_str::<serde_json::Value>(&output).is_err());
        assert!(output.contains("test error"));
    }

    #[test]
    fn test_format_error_cursor_is_valid_json() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let output = adapter.format_error("test error");
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["permission"], "deny");
        assert!(
            parsed["user_message"]
                .as_str()
                .unwrap()
                .contains("test error")
        );
    }

    #[test]
    fn test_format_error_codex_is_valid_json() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter.format_error("test error");
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["decision"], "block");
        assert!(parsed["reason"].as_str().unwrap().contains("test error"));
    }

    #[test]
    fn test_format_error_windsurf_is_plain_text() {
        // Windsurf は stderr をプレーンテキストとして扱うため、エラーは JSON ではなく本文を返す
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let output = adapter.format_error("test error");
        assert!(output.contains("test error"));
        assert!(output.contains("fail-closed"));
        // JSON ではないこと（decision キーを含まず、JSON としてパースできない）を確認
        assert!(!output.contains(r#""decision""#));
        assert!(serde_json::from_str::<serde_json::Value>(&output).is_err());
    }

    // === Windsurf BeforeCommand Block の出力形式テスト ===

    #[test]
    fn test_windsurf_before_command_block_stderr_is_plain_text() {
        // Windsurf の BeforeCommand Block はプレーンテキスト本文を stderr に書く。
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let decision = Decision::Block {
            message: "rm is blocked".to_string(),
        };

        // use_stderr が true であることを確認
        assert!(
            adapter.use_stderr(&decision, HookEvent::BeforeCommand),
            "Windsurf BeforeCommand Block は stderr を使うべき"
        );

        // 出力がメッセージ本文そのもの（プレーンテキスト）であることを確認
        let output = adapter
            .format_output(&decision, HookEvent::BeforeCommand)
            .unwrap();
        assert_eq!(output, "rm is blocked");
        // JSON ではないことを確認
        assert!(!output.contains(r#""decision""#));
    }

    #[test]
    fn test_windsurf_stop_block_does_not_use_stderr() {
        // post_cascade_response は事後フックなので、Stop の失敗を stderr に流さない。
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let decision = Decision::Block {
            message: "lint errors found".to_string(),
        };

        assert!(
            !adapter.use_stderr(&decision, HookEvent::Stop),
            "Windsurf Stop は stderr を使うべきではない"
        );

        // 出力は常に空 JSON を返す
        let output = adapter.format_output(&decision, HookEvent::Stop).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(
            parsed.as_object().unwrap().is_empty(),
            "Windsurf Stop の出力は空 JSON であるべき: {}",
            output
        );
    }

    // === Codex PostToolUse Block 出力テスト ===

    #[test]
    fn test_codex_output_after_file_edit_block_uses_reason() {
        // Codex の PostToolUse(AfterFileEdit) Block も decision/reason を使用する。
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "Extension hook failed".to_string(),
                },
                HookEvent::AfterFileEdit,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["decision"], "block");
        assert_eq!(parsed["reason"], "Extension hook failed");
        assert!(
            parsed.get("hookSpecificOutput").is_none(),
            "Codex は hookSpecificOutput を使わない"
        );
    }

    #[test]
    fn test_codex_write_tool_explicit_deserialization() {
        // Codex の Write ツールが FileOperationInput に正しくデシリアライズされる
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{"hook_event_name":"PostToolUse","tool_name":"Write","tool_input":{"file_path":"/tmp/test.rs","content":"fn main() {}"}}"#;
        let result = adapter.parse_input(&complete_codex_input(input)).unwrap();
        assert_eq!(result.event, HookEvent::AfterFileEdit);
        assert_eq!(result.tool_name, "Write");
        if let crate::domain::ToolInput::File(file) = &result.tool_input {
            assert_eq!(file.file_path, "/tmp/test.rs");
        } else {
            panic!("Write ツールは ToolInput::File にデシリアライズされるべき");
        }
    }

    #[test]
    fn test_codex_write_empty_object_is_error() {
        // Codex の Write ツールで空の tool_input は FileOperationInput デシリアライズ失敗
        // 旧実装では ToolInput::Stop に誤マッチしていた
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{"hook_event_name":"PostToolUse","tool_name":"Write","tool_input":{}}"#;
        assert!(
            adapter.parse_input(&complete_codex_input(input)).is_err(),
            "空の tool_input は FileOperationInput へのデシリアライズに失敗すべき"
        );
    }

    // === Claude PreToolUse Block ANSI 正規化テスト ===

    #[test]
    fn test_claude_before_command_block_ansi_not_normalized() {
        // PreToolUse Block のメッセージは truncate_decision を通るが
        // normalize_lint_output は呼ばれないため ANSI コードが残る。
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "\x1b[31merror\x1b[0m: rm is blocked".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        // PreToolUse では ANSI コードが残る（Stop パスとは異なる挙動）
        assert!(
            reason.contains("\x1b"),
            "PreToolUse Block では ANSI コードが正規化されないことを確認"
        );
    }

    // === Antigravity CLI フォーマットのテスト ===

    #[test]
    fn test_agy_input_parsing_pre_tool_use_run_command() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "toolCall":{"name":"run_command","args":{"CommandLine":"rm -rf /tmp/test","Cwd":"/workspace","WaitMsBeforeAsync":5000}},
            "stepIdx":3,
            "conversationId":"ec33ebf9-0cba-4100-8142-c61503f6c587"
        }"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
        assert_eq!(
            result.session_id,
            Some("ec33ebf9-0cba-4100-8142-c61503f6c587".to_string())
        );
        if let crate::domain::ToolInput::Bash(bash) = &result.tool_input {
            assert_eq!(bash.command, "rm -rf /tmp/test");
        } else {
            panic!("Expected Bash tool input");
        }
    }

    #[test]
    fn test_agy_input_parsing_pre_tool_use_run_command_missing_command_line_is_error() {
        // CommandLine 空または欠落 → フェイルクローズド
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{"hook_event_name":"PreToolUse","stepIdx":0,"toolCall":{"name":"run_command","args":{}}}"#;
        let err = adapter.parse_input(input).unwrap_err().to_string();
        assert!(err.contains("CommandLine"));
    }

    #[test]
    fn test_agy_input_parsing_pre_tool_use_empty_command_line_is_error() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        for command in ["", "   "] {
            let input = format!(
                r#"{{"hook_event_name":"PreToolUse","stepIdx":0,"toolCall":{{"name":"run_command","args":{{"CommandLine":"{command}"}}}}}}"#
            );
            let err = adapter.parse_input(&input).unwrap_err().to_string();
            assert!(err.contains("CommandLine"));
        }
    }

    #[test]
    fn test_agy_input_parsing_pre_tool_use_write_to_file_is_passthrough() {
        // Antigravity の write_to_file は claw-hooks のコマンドブロックの対象外。
        // Passthrough（パススルー）にマップされる。
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "hook_event_name":"PreToolUse",
            "stepIdx":0,
            "toolCall":{"name":"write_to_file","args":{"TargetFile":"/workspace/foo.rs","Overwrite":false,"CodeContent":"fn main(){}"}}
        }"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "write_to_file");
    }

    #[test]
    fn test_agy_input_parsing_pre_tool_use_replace_file_content_passthrough() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "hook_event_name":"PreToolUse",
            "stepIdx":0,
            "toolCall":{"name":"replace_file_content","args":{"TargetFile":"/workspace/foo.rs"}}
        }"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "replace_file_content");
    }

    #[test]
    fn test_agy_input_parsing_pre_tool_use_missing_tool_call_is_error() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{"hook_event_name":"PreToolUse","stepIdx":0}"#;
        let err = adapter.parse_input(input).unwrap_err().to_string();
        assert!(err.contains("toolCall"));
    }

    #[test]
    fn test_agy_input_parsing_pre_tool_use_missing_name_is_error() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{"hook_event_name":"PreToolUse","stepIdx":0,"toolCall":{"args":{}}}"#;
        let err = adapter.parse_input(input).unwrap_err().to_string();
        assert!(err.contains("toolCall.name"));
    }

    #[test]
    fn test_agy_input_parsing_post_tool_use_passthrough() {
        // PostToolUse には toolCall が無いため、claw-hooks の拡張子フック対象外。
        // 公式ペイロードにイベント名がなくてもパススルーで Allow を返す。
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "stepIdx":5,
            "error":"exit status 1",
            "conversationId":"abc-123"
        }"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "PostToolUse");
        assert_eq!(result.session_id, Some("abc-123".to_string()));
    }

    #[test]
    fn test_agy_input_parsing_pre_invocation_passthrough() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{"hook_event_name":"PreInvocation","invocationNum":3,"initialNumSteps":10}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "PreInvocation");
    }

    #[test]
    fn test_agy_input_parsing_post_invocation_passthrough() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input =
            r#"{"hook_event_name":"PostInvocation","invocationNum":3,"initialNumSteps":10}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "PostInvocation");
    }

    #[test]
    fn test_agy_input_parsing_stop() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "executionNum":1,
            "terminationReason":"model_stop",
            "error":"",
            "fullyIdle":true,
            "conversationId":"ec33ebf9-0cba-4100-8142-c61503f6c587"
        }"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
        assert_eq!(result.tool_name, "Stop");
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(stop.status, Some("model_stop".to_string()));
            assert_eq!(stop.agent_message, Some("model_stop".to_string()));
            assert!(!stop.stop_hook_active);
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_agy_input_parsing_stop_with_error_message() {
        // terminationReason が "error" 系のときは error フィールドのメッセージを優先する。
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "hook_event_name":"Stop",
            "executionNum":1,
            "terminationReason":"error",
            "error":"oom killed",
            "fullyIdle":false
        }"#;
        let result = adapter.parse_input(input).unwrap();
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert_eq!(stop.agent_message, Some("oom killed".to_string()));
            assert_eq!(stop.status, Some("error".to_string()));
        } else {
            panic!("Expected Stop tool input");
        }
    }

    #[test]
    fn test_agy_input_parsing_unknown_event_passthrough() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{"hook_event_name":"SomeNewEvent","conversationId":"abc-123"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Passthrough);
        assert_eq!(result.tool_name, "SomeNewEvent");
    }

    #[test]
    fn test_agy_input_parsing_missing_hook_event_name_is_error() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{"conversationId":"abc-123"}"#;
        let err = adapter.parse_input(input).unwrap_err().to_string();
        assert!(err.contains("hook event"));
    }

    #[test]
    fn test_agy_input_parsing_event_alias_accepted() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "event":"PreToolUse",
            "stepIdx":0,
            "toolCall":{"name":"run_command","args":{"CommandLine":"ls"}}
        }"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
    }

    #[test]
    fn test_agy_input_parsing_event_alias_used_when_primary_field_is_invalid() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "hook_event_name":42,
            "event":"PreToolUse",
            "stepIdx":0,
            "toolCall":{"name":"run_command","args":{"CommandLine":"ls"}}
        }"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
    }

    #[test]
    fn test_agy_input_parsing_blank_explicit_event_still_validates_command() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "hook_event_name":"   ",
            "stepIdx":0,
            "toolCall":{"name":"run_command","args":{}}
        }"#;
        let error = adapter.parse_input(input).unwrap_err().to_string();
        assert!(error.contains("CommandLine"));
    }

    #[test]
    fn test_agy_input_parsing_invalid_json_is_error() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        assert!(adapter.parse_input("{").is_err());
        assert!(adapter.parse_input("not json").is_err());
    }

    #[test]
    fn test_agy_output_pre_tool_use_allow() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::BeforeCommand)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["decision"], "allow");
        assert!(parsed.get("reason").is_none());
    }

    #[test]
    fn test_agy_output_pre_tool_use_deny() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "🚫 rm is blocked. Use safe-rm instead.".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["decision"], "deny");
        assert!(parsed["reason"].as_str().unwrap().contains("rm is blocked"));
    }

    #[test]
    fn test_agy_output_post_tool_use_always_empty() {
        // Antigravity の PostToolUse は output {} 固定。
        // claw-hooks 側でブロックを検出しても {} を返す（公式仕様に従う）。
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let allow = adapter
            .format_output(&Decision::allow(), HookEvent::AfterFileEdit)
            .unwrap();
        assert_eq!(allow, "{}");
        let block = adapter
            .format_output(
                &Decision::Block {
                    message: "lint failed".to_string(),
                },
                HookEvent::AfterFileEdit,
            )
            .unwrap();
        assert_eq!(block, "{}");
    }

    #[test]
    fn test_agy_output_passthrough_event_always_empty() {
        // Passthrough（PreInvocation / PostInvocation / 未対応イベントが内部マップされる先）は
        // ブロック仕様が無いため常に {} を返す。
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let allow = adapter
            .format_output(&Decision::allow(), HookEvent::Passthrough)
            .unwrap();
        assert_eq!(allow, "{}");
        let block = adapter
            .format_output(
                &Decision::Block {
                    message: "blocked".to_string(),
                },
                HookEvent::Passthrough,
            )
            .unwrap();
        assert_eq!(block, "{}");
    }

    #[test]
    fn test_agy_output_stop_allow_is_empty() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::Stop)
            .unwrap();
        assert_eq!(output, "{}");
    }

    #[test]
    fn test_agy_output_stop_block_uses_continue() {
        // Stop の Block は "continue" でエージェントを再投入し、reason を system message として注入する。
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "lint errors found".to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["decision"], "continue");
        assert!(parsed["reason"].as_str().unwrap().contains("lint errors"));
    }

    #[test]
    fn test_agy_output_normalizes_ansi_and_whitespace() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "  \x1b[1;31merror\x1b[0m: unused variable\n    expected `u32`"
                        .to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["reason"].as_str().unwrap();
        assert!(!reason.contains('\x1b'));
        assert!(reason.contains("error: unused variable"));
        assert!(reason.contains("expected `u32`"));
        assert!(!reason.starts_with(' '));
    }

    #[test]
    fn test_cursor_stop_parse_errors_allow_stop_without_followup() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let inputs = [
            r#"{"hook_event_name":"stop","loop_count":0}"#,
            r#"{"hook_event_name":"subagentStop","status":"completed"}"#,
        ];

        for input in inputs {
            let error = adapter.parse_input(input).unwrap_err().to_string();
            let output = adapter.format_error_for_input(&error, input);

            // followup_message は「次のユーザーメッセージとして自動送信される」=
            // 継続指示に相当する。壊れたペイロードで継続させても同じ失敗を繰り返すだけ
            // なので、停止を許可する空オブジェクトを返す。
            assert_eq!(output, "{}");
            // Cursor は exit 0 のときだけ stdout の JSON を解釈するため、
            // stop 系のフェイルクローズ経路も終了コード 0 に揃える。
            assert_eq!(adapter.error_exit_code(Some(input)), 0);
        }
    }

    #[test]
    fn test_agy_pre_tool_use_without_step_idx_is_accepted() {
        // `stepIdx` は公式仕様で Required マークが無く claw-hooks も判定に使わない。
        // 必須にすると、送られてこないビルドで全 run_command が
        // 「即時ハードブロック」の deny に倒れるため、任意扱いで受理する。
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "toolCall":{"name":"run_command","args":{"CommandLine":"echo ok"}}
        }"#;

        let parsed = adapter
            .parse_input(input)
            .expect("stepIdx は任意であるべき");
        assert_eq!(parsed.event, HookEvent::BeforeCommand);
        assert_eq!(parsed.bash_command(), Some("echo ok"));
    }

    #[test]
    fn test_agy_stop_without_execution_num_is_accepted() {
        // `executionNum` は Required マークが無く、claw-hooks は値を破棄している。
        // 必須にすると Stop のパースが失敗し、Antigravity の Stop パース失敗は
        // `{}` + exit 0 に倒れる仕様のため、全 stop hook（lint / commit / 通知）が
        // エラー表示なしで黙って実行されなくなる。
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "terminationReason":"model_stop",
            "fullyIdle":true
        }"#;

        let parsed = adapter
            .parse_input(input)
            .expect("executionNum は任意であるべき");
        assert_eq!(parsed.event, HookEvent::Stop);
    }

    #[test]
    fn test_agy_stop_missing_fully_idle_fails_closed() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "executionNum":1,
            "terminationReason":"model_stop"
        }"#;

        let error = adapter.parse_input(input).unwrap_err().to_string();
        assert!(error.contains("fullyIdle"));

        // continue（再投入）ではなく停止許可を返す（無限ループ回避）。
        let output = adapter.format_error_for_input(&error, input);
        assert_eq!(output, "{}");
    }

    #[test]
    fn test_agy_stop_without_termination_reason_is_accepted() {
        // `terminationReason` も Required マークが無く、status / agent_message の
        // 情報表示にしか使わない。欠落で stop hook 全体を落とさない。
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{"executionNum":1,"fullyIdle":true}"#;

        let parsed = adapter
            .parse_input(input)
            .expect("terminationReason は任意であるべき");
        assert_eq!(parsed.event, HookEvent::Stop);
    }

    #[test]
    fn test_agy_post_tool_use_maps_file_edit_to_after_file_edit() {
        // 公式仕様の PostToolUse は toolCall を含むため args.TargetFile から
        // 編集対象を復元でき、保存後フック（formatter / linter）が成立する。
        let adapter =
            FormatAdapter::new(Format::Agy, 0).with_event_override(Some("PostToolUse".to_string()));
        let input = r#"{
            "toolCall":{"name":"write_to_file","args":{"TargetFile":"/tmp/a.rs","CodeContent":"fn main(){}"}},
            "stepIdx":5,
            "conversationId":"c1"
        }"#;

        let parsed = adapter.parse_input(input).unwrap();
        assert_eq!(parsed.event, HookEvent::AfterFileEdit);
        assert_eq!(parsed.tool_name, "Write");
        match &parsed.tool_input {
            crate::domain::ToolInput::File(file) => assert_eq!(file.file_path, "/tmp/a.rs"),
            other => panic!("File 入力を期待したが {:?}", other),
        }
    }

    #[test]
    fn test_agy_post_tool_use_run_command_does_not_deny() {
        // 実行済みコマンドの PostToolUse に deny を返してはいけない
        // （公式仕様では PostToolUse の出力は `{}` 固定）。
        let adapter =
            FormatAdapter::new(Format::Agy, 0).with_event_override(Some("PostToolUse".to_string()));
        let input = r#"{
            "toolCall":{"name":"run_command","args":{"CommandLine":"rm -rf /tmp/x"}},
            "stepIdx":5
        }"#;

        let parsed = adapter.parse_input(input).unwrap();
        assert_eq!(parsed.event, HookEvent::Passthrough);

        let output = adapter
            .format_output(&Decision::allow(), parsed.event)
            .unwrap();
        assert_eq!(output, "{}");
    }

    #[test]
    fn test_agy_post_tool_use_block_still_outputs_empty_object() {
        // PostToolUse はブロック不可。内部判定が Block でも `{}` に倒す。
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "lint failed".to_string(),
                },
                HookEvent::AfterFileEdit,
            )
            .unwrap();
        assert_eq!(output, "{}");
    }

    #[test]
    fn test_agy_event_override_takes_precedence_over_shape_inference() {
        // 形状推定では PreToolUse に倒れるペイロードでも、--event で PostToolUse を選べる。
        let shape_only = FormatAdapter::new(Format::Agy, 0);
        let overridden =
            FormatAdapter::new(Format::Agy, 0).with_event_override(Some("PostToolUse".to_string()));
        let input = r#"{
            "toolCall":{"name":"run_command","args":{"CommandLine":"echo ok"}},
            "stepIdx":1
        }"#;

        assert_eq!(
            shape_only.parse_input(input).unwrap().event,
            HookEvent::BeforeCommand
        );
        assert_eq!(
            overridden.parse_input(input).unwrap().event,
            HookEvent::Passthrough
        );
    }

    #[test]
    fn test_agy_event_override_blank_is_ignored() {
        // 空文字・空白のみの --event は未指定として扱う。
        let adapter =
            FormatAdapter::new(Format::Agy, 0).with_event_override(Some("   ".to_string()));
        let input = r#"{
            "toolCall":{"name":"run_command","args":{"CommandLine":"echo ok"}},
            "stepIdx":1
        }"#;

        assert_eq!(
            adapter.parse_input(input).unwrap().event,
            HookEvent::BeforeCommand
        );
    }

    // === 壊れた JSON からのイベント名走査（Stop のループ回避） ===

    #[test]
    fn test_scan_event_name_reads_truncated_payload() {
        let truncated = r#"{"hook_event_name":"Stop","stop_hook_active":"#;
        assert_eq!(
            scan_event_name_in_malformed_input(truncated, &["\"hook_event_name\"", "\"event\""]),
            Some("Stop".to_string())
        );
    }

    #[test]
    fn test_scan_event_name_returns_none_without_closing_quote() {
        // 値そのものが切れている場合は判定に使わない（フェイルクローズドを維持）。
        let truncated = r#"{"hook_event_name":"Sto"#;
        assert_eq!(
            scan_event_name_in_malformed_input(truncated, &["\"hook_event_name\""]),
            None
        );
    }

    #[test]
    fn test_scan_event_name_ignores_content_beyond_prefix_window() {
        // 先頭ウィンドウ外に現れるキーは拾わない（巨大なコマンド本文での誤検出防止）。
        let padded = format!(
            "{{\"pad\":\"{}\",\"hook_event_name\":\"Stop\"",
            "x".repeat(EVENT_NAME_SCAN_PREFIX_BYTES)
        );
        assert_eq!(
            scan_event_name_in_malformed_input(&padded, &["\"hook_event_name\""]),
            None
        );
    }

    #[test]
    fn test_scan_event_name_prefers_earliest_key() {
        let input = r#"{"hook_event_name":"Stop","event":"PreToolUse""#;
        assert_eq!(
            scan_event_name_in_malformed_input(input, &["\"hook_event_name\"", "\"event\""]),
            Some("Stop".to_string())
        );
    }

    #[test]
    fn test_truncated_stop_does_not_block_for_claude_and_codex() {
        // Stop のブロックは「停止させず reason を継続プロンプトにする」意味なので、
        // 壊れたペイロードで返すと Stop 再発火の無限ループになる。
        let truncated = r#"{"hook_event_name":"Stop","stop_hook_active":"#;
        for format in [Format::Claude, Format::Codex] {
            let adapter = FormatAdapter::new(format, 0);
            assert_eq!(
                adapter.format_error_for_input("boom", truncated),
                "{}",
                "{format:?} は停止を許可すべき"
            );
            assert_eq!(adapter.error_exit_code(Some(truncated)), 0);
        }
    }

    #[test]
    fn test_truncated_pre_tool_use_still_fails_closed() {
        // 実行前ゲートは壊れた入力でもブロックを維持する（フェイルクローズド）。
        let truncated =
            r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"#;
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let output = adapter.format_error_for_input("boom", truncated);
        assert!(
            output.contains("\"permissionDecision\":\"deny\""),
            "PreToolUse は拒否を維持すべき: {output}"
        );
    }

    #[test]
    fn test_truncated_cursor_stop_does_not_block() {
        let truncated = r#"{"hook_event_name":"stop","conversation_id":"#;
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        assert_eq!(adapter.format_error_for_input("boom", truncated), "{}");
        assert_eq!(adapter.error_exit_code(Some(truncated)), 0);
    }

    #[test]
    fn test_truncated_windsurf_post_cascade_does_not_block() {
        let truncated = r#"{"agent_action_name":"post_cascade_response","cascade_id":"#;
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        assert_eq!(adapter.format_error_for_input("boom", truncated), "{}");
        assert_eq!(adapter.error_exit_code(Some(truncated)), 0);
    }

    #[test]
    fn test_agy_format_error_uses_deny() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let output = adapter.format_error("Invalid JSON input");
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["decision"], "deny");
        assert!(parsed["reason"].as_str().unwrap().contains("fail-closed"));
        assert!(
            parsed["reason"]
                .as_str()
                .unwrap()
                .contains("Invalid JSON input")
        );
    }

    #[test]
    fn test_agy_error_exit_code_is_zero() {
        // Agy: エラー時も 0（JSON 内で deny を有効にするため、Codex と同じ）
        let adapter = FormatAdapter::new(Format::Agy, 0);
        assert_eq!(adapter.error_exit_code(None), 0);
    }

    #[test]
    fn test_agy_exit_code_always_zero() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let block = Decision::Block {
            message: "blocked".to_string(),
        };
        assert_eq!(
            adapter.exit_code(&Decision::allow(), HookEvent::BeforeCommand),
            0
        );
        assert_eq!(adapter.exit_code(&block, HookEvent::BeforeCommand), 0);
        assert_eq!(adapter.exit_code(&block, HookEvent::Stop), 0);
        assert_eq!(adapter.exit_code(&block, HookEvent::AfterFileEdit), 0);
    }

    #[test]
    fn test_agy_use_stderr_is_false() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let block = Decision::Block {
            message: "blocked".to_string(),
        };
        for event in [
            HookEvent::BeforeCommand,
            HookEvent::AfterFileEdit,
            HookEvent::Stop,
            HookEvent::Passthrough,
        ] {
            assert!(!adapter.use_stderr(&block, event));
            assert!(!adapter.use_stderr(&Decision::allow(), event));
        }
    }

    #[test]
    fn test_agy_format_uses_stderr_for_errors_is_false() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        // Antigravity は JSON を stdout に返すため stderr 書き込みは不要
        assert!(!adapter.format_uses_stderr_for_errors());
    }

    // === Grok CLI フォーマット ===

    #[test]
    fn test_grok_pre_tool_use_bash_maps_to_before_command() {
        let adapter = FormatAdapter::new(Format::Grok, 0);
        let input = r#"{
            "hookEventName":"PreToolUse",
            "sessionId":"grok-session",
            "cwd":"/tmp",
            "workspaceRoot":"/tmp",
            "toolName":"Bash",
            "toolInput":{"command":"rm -rf /tmp/x","timeout":30}
        }"#;

        let parsed = adapter.parse_input(input).unwrap();
        assert_eq!(parsed.event, HookEvent::BeforeCommand);
        assert_eq!(parsed.tool_name, "Bash");
        assert_eq!(parsed.session_id.as_deref(), Some("grok-session"));
        match parsed.tool_input {
            crate::domain::ToolInput::Bash(bash) => {
                assert_eq!(bash.command, "rm -rf /tmp/x");
                assert_eq!(bash.timeout, Some(30));
            }
            other => panic!("Bash 入力になるべき: {other:?}"),
        }
    }

    #[test]
    fn test_grok_pre_tool_use_detects_command_regardless_of_tool_name() {
        // Grok は Claude のツール名を自前のツール名へ自動マッピングするが、
        // マッピング後の名前は公開仕様に列挙されていない。ツール名の文字列一致に
        // 頼ると未知の名前のシェル実行ツールが素通りしてブロックを回避できるため、
        // toolInput の形（command を持つか）で判定する。
        let adapter = FormatAdapter::new(Format::Grok, 0);
        let input = r#"{
            "hookEventName":"PreToolUse",
            "sessionId":"s",
            "toolName":"grok_internal_shell_v2",
            "toolInput":{"command":"rm -rf /"}
        }"#;

        let parsed = adapter.parse_input(input).unwrap();
        assert_eq!(parsed.event, HookEvent::BeforeCommand);
        assert_eq!(parsed.tool_name, "Bash");
    }

    #[test]
    fn test_grok_post_tool_use_file_edit_maps_to_after_file_edit() {
        let adapter = FormatAdapter::new(Format::Grok, 0);
        for key in ["file_path", "filePath"] {
            let input = format!(
                r#"{{
                    "hookEventName":"PostToolUse",
                    "sessionId":"s",
                    "toolName":"Edit",
                    "toolInput":{{"{key}":"/tmp/a.py"}}
                }}"#
            );

            let parsed = adapter.parse_input(&input).unwrap();
            assert_eq!(parsed.event, HookEvent::AfterFileEdit, "key: {key}");
            assert_eq!(parsed.tool_name, "Write");
            match parsed.tool_input {
                crate::domain::ToolInput::File(file) => assert_eq!(file.file_path, "/tmp/a.py"),
                other => panic!("File 入力になるべき: {other:?}"),
            }
        }
    }

    #[test]
    fn test_grok_pre_tool_use_file_edit_is_passthrough() {
        // 編集前は claw-hooks の責務外（保存後フックは PostToolUse で実行する）
        let adapter = FormatAdapter::new(Format::Grok, 0);
        let input = r#"{
            "hookEventName":"PreToolUse",
            "sessionId":"s",
            "toolName":"Edit",
            "toolInput":{"file_path":"/tmp/a.py"}
        }"#;

        let parsed = adapter.parse_input(input).unwrap();
        assert_eq!(parsed.event, HookEvent::Passthrough);
    }

    #[test]
    fn test_grok_post_tool_use_command_is_passthrough() {
        // 実行後のコマンドはブロックできないため保存後フックの対象にもならない
        let adapter = FormatAdapter::new(Format::Grok, 0);
        let input = r#"{
            "hookEventName":"PostToolUse",
            "sessionId":"s",
            "toolName":"Bash",
            "toolInput":{"command":"ls"}
        }"#;

        let parsed = adapter.parse_input(input).unwrap();
        assert_eq!(parsed.event, HookEvent::Passthrough);
    }

    #[test]
    fn test_grok_out_of_scope_tool_is_passthrough() {
        let adapter = FormatAdapter::new(Format::Grok, 0);
        let input = r#"{
            "hookEventName":"PreToolUse",
            "sessionId":"s",
            "toolName":"Read",
            "toolInput":{"path":"/tmp/a"}
        }"#;

        let parsed = adapter.parse_input(input).unwrap();
        assert_eq!(parsed.event, HookEvent::Passthrough);
        assert_eq!(parsed.tool_name, "Read");
    }

    #[test]
    fn test_grok_stop_and_subagent_events() {
        let adapter = FormatAdapter::new(Format::Grok, 0);

        let stop = adapter
            .parse_input(
                r#"{"hookEventName":"Stop","sessionId":"s","lastAssistantMessage":"done"}"#,
            )
            .unwrap();
        assert_eq!(stop.event, HookEvent::Stop);
        match stop.tool_input {
            crate::domain::ToolInput::Stop(input) => {
                assert_eq!(input.agent_message.as_deref(), Some("done"));
                // Grok には stop_hook_active / loop_count 相当の入力が無い
                assert!(!input.stop_hook_active);
                assert_eq!(input.loop_count, None);
                assert_eq!(input.session_kind, crate::domain::StopSessionKind::Primary);
            }
            other => panic!("Stop 入力になるべき: {other:?}"),
        }

        let start = adapter
            .parse_input(r#"{"hookEventName":"SubagentStart","sessionId":"s","subagentType":"Explore","prompt":"look"}"#)
            .unwrap();
        assert_eq!(start.event, HookEvent::SubagentStart);
        match start.tool_input {
            crate::domain::ToolInput::Subagent(input) => {
                assert_eq!(input.subagent_type.as_deref(), Some("Explore"));
                assert_eq!(input.prompt.as_deref(), Some("look"));
            }
            other => panic!("Subagent 入力になるべき: {other:?}"),
        }

        let stopped = adapter
            .parse_input(r#"{"hookEventName":"SubagentStop","sessionId":"s","agentType":"Plan","status":"completed","duration":1200}"#)
            .unwrap();
        assert_eq!(stopped.event, HookEvent::SubagentStop);
        match stopped.tool_input {
            crate::domain::ToolInput::Subagent(input) => {
                assert_eq!(input.subagent_type.as_deref(), Some("Plan"));
                assert_eq!(input.status.as_deref(), Some("completed"));
                assert_eq!(input.duration, Some(1200));
            }
            other => panic!("Subagent 入力になるべき: {other:?}"),
        }
    }

    #[test]
    fn test_grok_unsupported_events_are_passthrough() {
        let adapter = FormatAdapter::new(Format::Grok, 0);
        for event in [
            "SessionStart",
            "SessionEnd",
            "UserPromptSubmit",
            "PostToolUseFailure",
            "PermissionDenied",
            "StopFailure",
            "Notification",
            "PreCompact",
            "PostCompact",
            "SomeFutureEvent",
        ] {
            let input = format!(r#"{{"hookEventName":"{event}","sessionId":"s"}}"#);
            let parsed = adapter.parse_input(&input).unwrap();
            assert_eq!(parsed.event, HookEvent::Passthrough, "event: {event}");
            assert_eq!(parsed.tool_name, event);
        }
    }

    #[test]
    fn test_grok_missing_required_fields_fail_closed() {
        let adapter = FormatAdapter::new(Format::Grok, 0);
        let cases = [
            (r#"{"sessionId":"s"}"#, "hookEventName"),
            (
                r#"{"hookEventName":"PreToolUse","toolInput":{"command":"ls"}}"#,
                "toolName",
            ),
            (
                r#"{"hookEventName":"PreToolUse","toolName":"Bash"}"#,
                "toolInput",
            ),
        ];

        for (input, field) in cases {
            let err = adapter.parse_input(input).unwrap_err().to_string();
            assert!(err.contains(field), "{field} 欠落時のエラーが不適切: {err}");
        }
    }

    #[test]
    fn test_grok_output_and_exit_codes() {
        let adapter = FormatAdapter::new(Format::Grok, 0);
        let allow = Decision::allow();
        let block = Decision::Block {
            message: "🚫 Use safe-rm instead".to_string(),
        };

        // PreToolUse: 許可は空オブジェクト + exit 0
        assert_eq!(
            adapter
                .format_output(&allow, HookEvent::BeforeCommand)
                .unwrap(),
            "{}"
        );
        assert_eq!(adapter.exit_code(&allow, HookEvent::BeforeCommand), 0);

        // PreToolUse: 拒否は deny JSON + exit 2。
        // 公式仕様は「exit 0 は許可、exit 2 は拒否、それ以外はフェイルオープン」なので、
        // 両方で拒否を表明してどちらの解釈でもブロックが成立するようにする。
        let output = adapter
            .format_output(&block, HookEvent::BeforeCommand)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["decision"], "deny");
        assert!(value["reason"].as_str().unwrap().contains("safe-rm"));
        assert_eq!(adapter.exit_code(&block, HookEvent::BeforeCommand), 2);

        // PreToolUse 以外は stdout が無視される事後フックなので常に {} + exit 0
        for event in [
            HookEvent::AfterFileEdit,
            HookEvent::Stop,
            HookEvent::SubagentStop,
            HookEvent::Passthrough,
        ] {
            assert_eq!(adapter.format_output(&block, event).unwrap(), "{}");
            assert_eq!(adapter.exit_code(&block, event), 0, "event: {event:?}");
        }
    }

    #[test]
    fn test_grok_parse_error_is_fail_closed_with_deny_and_exit_two() {
        let adapter = FormatAdapter::new(Format::Grok, 0);
        // Grok は exit 2 だけが明示的な拒否で、それ以外の異常終了はフェイルオープン。
        assert_eq!(adapter.error_exit_code(None), 2);
        // stdout を読む方式なので stderr へは書かない
        assert!(!adapter.format_uses_stderr_for_errors());

        let output = adapter.format_error("broken input");
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["decision"], "deny");
        assert!(value["reason"].as_str().unwrap().contains("fail-closed"));
    }

    #[test]
    fn test_grok_stop_parse_error_allows_stop() {
        // Grok の Stop はブロック不可の事後フック。拒否を返しても無視されるだけなので、
        // 余計なフック失敗記録を残さず停止を許可する。
        let adapter = FormatAdapter::new(Format::Grok, 0);
        let input = r#"{"hookEventName":"Stop","sessionId":"s"}"#;

        assert_eq!(adapter.format_error_for_input("broken", input), "{}");
        assert_eq!(adapter.error_exit_code(Some(input)), 0);

        // PreToolUse はフェイルクローズドを維持する
        let pre = r#"{"hookEventName":"PreToolUse","toolName":"Bash"}"#;
        assert!(
            adapter
                .format_error_for_input("broken", pre)
                .contains("deny")
        );
        assert_eq!(adapter.error_exit_code(Some(pre)), 2);
    }

    #[test]
    fn test_grok_format_label_and_emoji() {
        assert_eq!(Format::Grok.label(), "Grok CLI");
        assert_eq!(Format::Grok.emoji(), "🤖");
    }
}
