//! AIコーディングエージェント向けフォーマットアダプター。
//!
//! 以下のエージェントの入力パースと出力フォーマットを提供する:
//! - Claude Code（デフォルト）
//! - Cursor
//! - Windsurf (Cascade)
//! - Antigravity CLI
//! - Codex CLI

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::cli::Format;
use crate::domain::{Decision, HookEvent, HookInput, normalize_lint_output, truncate_output};
use crate::service::log_sanitizer::summarize_hook_input;

/// フォーマット固有のI/Oと内部型を変換するアダプター。
pub struct FormatAdapter {
    format: Format,
    /// 出力メッセージの最大長（0 = 無制限）
    output_max_length: usize,
}

impl FormatAdapter {
    /// 指定されたフォーマット用の新しいアダプターを作成する。
    pub fn new(format: Format, output_max_length: usize) -> Self {
        Self {
            format,
            output_max_length,
        }
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
        }
    }

    /// 判定結果に対する終了コードを取得する。
    /// 注意: エージェントごとに終了コードのセマンティクスが異なる。
    /// - Claude: 0 = 成功（判定は stdout の JSON で伝達される）
    /// - Windsurf: 0 = 許可/Stop, 2 = ブロック（pre_run_command のみ）
    /// - Cursor: 0 = 許可/停止, 2 = ブロック（停止以外）
    /// - Codex CLI: 0 = 成功（判定は stdout の JSON で伝達される）
    /// - Antigravity CLI: 0 = 成功（判定はJSON内、エラーも 0 + deny JSON）
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
            Format::Cursor if event == HookEvent::Stop => {
                // Cursor Stop: 判定はJSON内のfollowup_messageで伝達される。
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
    /// Windsurf は exit code 2 でブロック時、stderr からエラーメッセージを読み取る。
    /// ただし Stop は post_cascade_response に対応する事後フックのため対象外。
    pub fn use_stderr(&self, decision: &Decision, event: HookEvent) -> bool {
        matches!(
            (&self.format, event, decision),
            (
                &Format::Windsurf,
                HookEvent::BeforeCommand,
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
                // Antigravity CLI: PreToolUse 仕様の {"decision":"deny","reason":...} を返す。
                // PostToolUse / Stop でも余分なフィールドは仕様上無害（PostToolUse の output は
                // {} 固定だが、追加フィールドは無視される）。Stop も Block と同じ JSON で
                // フェイルクローズドを成立させる。
                // セキュリティ: パースエラー時は拒否（フェイルクローズド設計）。
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
        }
    }

    /// 入力のイベント名を考慮してエラー出力をフォーマットする。
    ///
    /// Codex の PermissionRequest は通常の block 形式ではなく専用の deny schema を要求する。
    /// PreToolUse も推奨形式（hookSpecificOutput.permissionDecision="deny"）で返す。
    /// イベント名が判別できない場合は legacy block 形式（全イベントで受理される）に
    /// フォールバックし、フェイルクローズドを維持する。
    pub fn format_error_for_input(&self, message: &str, input: &str) -> String {
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

    /// 生 JSON からイベント名だけを取り出す。失敗時は通常のエラー形式へフォールバックする。
    fn raw_hook_event_name(input: &str) -> Option<String> {
        let raw: serde_json::Value = serde_json::from_str(input).ok()?;
        raw.get("hook_event_name")
            .or_else(|| raw.get("event"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    }

    /// エラー時の終了コードを取得する（フェイルクローズド = ブロック）。
    /// Codex/Agy: 非0終了コードはフック失敗として扱われ判定が無視されるため0を返す。
    /// Claude/Cursor/Windsurf: 終了コード2でブロックを表現する。
    pub fn error_exit_code(&self) -> i32 {
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
            "Bash" => {
                let command = raw_tool_input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
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
                if file.file_path.is_empty() {
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
                    mapped_event = ?HookEvent::BeforePrompt,
                    "{} unsupported event, passing through", self.log_prefix()
                );
                return Ok(HookInput {
                    event: HookEvent::BeforePrompt, // パススルー用
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

        // Stopイベントを特別に処理（tool_nameやtool_inputが無い）
        let (tool_name, tool_input) = if event == HookEvent::Stop {
            (
                "Stop".to_string(),
                crate::domain::ToolInput::Stop(crate::domain::StopInput {
                    status: None,
                    loop_count: None,
                    response: None,
                    agent_message: claude_input.last_assistant_message.clone(),
                    stop_hook_active: claude_input.stop_hook_active.unwrap_or(false),
                }),
            )
        } else if event == HookEvent::SubagentStart || event == HookEvent::SubagentStop {
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
            };
            (
                tool_name.to_string(),
                crate::domain::ToolInput::Subagent(subagent_input),
            )
        } else {
            let tool_name = claude_input
                .tool_name
                .ok_or_else(|| anyhow!("Missing tool_name field"))?;
            let raw_tool_input = claude_input
                .tool_input
                .ok_or_else(|| anyhow!("Missing tool_input field"))?;
            let tool_input =
                Self::parse_tool_input_for_tool("Claude", &tool_name, &raw_tool_input)?;
            (tool_name, tool_input)
        };

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
            .unwrap_or("")
            .to_string();

        match event_name.as_str() {
            "beforeShellExecution" => {
                let parsed: CursorShellInput = serde_json::from_value(raw)
                    .map_err(|e| anyhow!("Failed to parse Cursor beforeShellExecution: {}", e))?;

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
            "preToolUse" => {
                let tool_name = raw
                    .get("tool_name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("Missing tool_name for Cursor preToolUse"))?
                    .to_string();

                if tool_name != "Shell" && tool_name != "Bash" {
                    debug!(
                        agent = self.format.label(),
                        hook_type = "preToolUse",
                        tool_name = %tool_name,
                        mapped_event = ?HookEvent::BeforePrompt,
                        "{} unsupported preToolUse tool, passing through", self.log_prefix()
                    );

                    return Ok(HookInput {
                        event: HookEvent::BeforePrompt,
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
            "afterFileEdit" | "afterTabFileEdit" => {
                let parsed: CursorFileEditInput = serde_json::from_value(raw)
                    .map_err(|e| anyhow!("Failed to parse Cursor afterFileEdit: {}", e))?;

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
            "stop" => {
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
                    }),
                    session_id: None,
                })
            }
            "subagentStart" => {
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
            "subagentStop" => {
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
            // 未対応イベント（afterShellExecution, postToolUse 等）は
            // パススルーとして処理し、ブロックしない
            other => {
                debug!(
                    agent = self.format.label(),
                    hook_event_name = other,
                    mapped_event = ?HookEvent::BeforePrompt,
                    "{} unsupported event, passing through", self.log_prefix()
                );

                Ok(HookInput {
                    event: HookEvent::BeforePrompt,
                    tool_name: event_name.to_string(),
                    tool_input: crate::domain::ToolInput::Other(raw),
                    session_id: None,
                })
            }
        }
    }

    fn format_cursor_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
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

        let output = match &self.truncate_decision(decision) {
            Decision::Allow { .. } => CursorOutput {
                permission: "allow".to_string(),
                user_message: None,
                agent_message: None,
            },
            Decision::Block { message } => CursorOutput {
                permission: "deny".to_string(),
                user_message: Some(message.clone()),
                agent_message: Some("Command blocked by claw-hooks".to_string()),
            },
        };
        serde_json::to_string(&output)
            .map_err(|e| anyhow!("Failed to serialize Cursor output: {}", e))
    }

    // === Windsurf フォーマット ===

    fn parse_windsurf_input(&self, input: &str) -> Result<HookInput> {
        debug!(input = %summarize_hook_input(input), "{} raw input", self.log_prefix());

        let raw: serde_json::Value = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Windsurf input: {}", e))?;
        let windsurf_input: WindsurfInput = serde_json::from_value(raw.clone())
            .map_err(|e| anyhow!("Failed to parse Windsurf input: {}", e))?;

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
                    }),
                )
            }
            other => {
                debug!(
                    agent = self.format.label(),
                    agent_action_name = other,
                    mapped_event = ?HookEvent::BeforePrompt,
                    "{} unsupported action, passing through", self.log_prefix()
                );

                return Ok(HookInput {
                    event: HookEvent::BeforePrompt,
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

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

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
        assert!(output.contains(r#""permission":"allow""#));
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
        // BeforeCommand Allow では hookSpecificOutput に permissionDecision = "allow" が含まれる
        assert!(output.contains("hookSpecificOutput"));
        assert!(output.contains(r#""permissionDecision":"allow""#));
        assert!(output.contains(r#""hookEventName":"PreToolUse""#));
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
    fn test_claude_input_parsing_stop_hook_active_unset() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name":"Stop"}"#;
        let result = adapter.parse_input(input).unwrap();
        if let crate::domain::ToolInput::Stop(stop) = &result.tool_input {
            assert!(!stop.stop_hook_active);
        } else {
            panic!("Expected Stop tool input");
        }
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
    fn test_cursor_exit_code_before_command_block_is_two() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let decision = Decision::Block {
            message: "blocked".to_string(),
        };
        // Stop 以外の Block は従来どおり終了コード 2
        assert_eq!(adapter.exit_code(&decision, HookEvent::BeforeCommand), 2);
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
        let result = adapter.parse_input(input).unwrap();

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

        let result = adapter.parse_input(input).unwrap();
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

        let result = adapter.parse_input(input).unwrap();
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

        let result = adapter.parse_input(input).unwrap();
        // SessionStart は処理対象外 → BeforePrompt（パススルー）にマッピング
        assert_eq!(result.event, HookEvent::BeforePrompt);
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

        let result = adapter.parse_input(input).unwrap();
        // UserPromptSubmit は処理対象外 → BeforePrompt（パススルー）にマッピング
        assert_eq!(result.event, HookEvent::BeforePrompt);
        assert_eq!(result.tool_name, "UserPromptSubmit");
    }

    #[test]
    fn test_codex_unknown_event_passthrough() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "SomeNewEvent",
            "session_id": "abc-123"
        }"#;

        // 未知イベントは Stop にフォールバックせず、パススルーになること
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
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

        let result = adapter.parse_input(input).unwrap();
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

        let result = adapter.parse_input(input).unwrap();
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

        let result = adapter.parse_input(input).unwrap();
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

        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_codex_parse_missing_tool_input_is_error() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash"
        }"#;

        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_codex_parse_missing_tool_input_command_is_error() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {}
        }"#;

        assert!(adapter.parse_input(input).is_err());
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
        assert_eq!(adapter.error_exit_code(), 0);
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
        assert_eq!(result.event, HookEvent::BeforePrompt);
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
        assert_eq!(result.event, HookEvent::BeforePrompt);
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
            crate::domain::HookEvent::BeforePrompt
        ));
    }

    #[test]
    fn test_claude_parse_invalid_json_is_error() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        assert!(adapter.parse_input("{").is_err());
        assert!(adapter.parse_input("not json").is_err());
    }

    #[test]
    fn test_cursor_parse_empty_object_is_passthrough() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        // hook_event_name がない空オブジェクトは未対応イベントとしてパススルー
        let input = r#"{}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
    }

    #[test]
    fn test_cursor_unsupported_event_passthrough() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        // afterShellExecution は claw-hooks の対象外でパススルー
        let input =
            r#"{"hook_event_name":"afterShellExecution","command":"echo test","output":"test"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
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
        assert_eq!(result.event, HookEvent::BeforePrompt);
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
            HookEvent::BeforePrompt,
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
        assert_eq!(result.event, HookEvent::BeforePrompt);
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
        assert_eq!(adapter.error_exit_code(), 2);
    }

    #[test]
    fn test_error_exit_code_codex_returns_zero() {
        // Codex CLI: 非0終了コードはフック失敗として扱われ判定が無視されるため、
        // エラー時も0を返しJSON内のblock判定を有効にする
        let adapter = FormatAdapter::new(Format::Codex, 0);
        assert_eq!(adapter.error_exit_code(), 0);
    }

    #[test]
    fn test_error_exit_code_cursor_returns_two() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        assert_eq!(adapter.error_exit_code(), 2);
    }

    #[test]
    fn test_error_exit_code_windsurf_returns_two() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        assert_eq!(adapter.error_exit_code(), 2);
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
        // Claude: Allow + BeforeCommand → 0
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
    fn test_cursor_exit_code_block_before_command_two() {
        // Cursor: BeforeCommand の Block → 2
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let decision = Decision::Block {
            message: "blocked".to_string(),
        };
        assert_eq!(adapter.exit_code(&decision, HookEvent::BeforeCommand), 2);
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
        assert_eq!(adapter.error_exit_code(), 0);
    }

    #[test]
    fn test_error_exit_code_claude_two() {
        // Claude: エラー時は2
        let adapter = FormatAdapter::new(Format::Claude, 0);
        assert_eq!(adapter.error_exit_code(), 2);
    }

    #[test]
    fn test_error_exit_code_cursor_two() {
        // Cursor: エラー時は2
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        assert_eq!(adapter.error_exit_code(), 2);
    }

    #[test]
    fn test_error_exit_code_windsurf_two() {
        // Windsurf: エラー時は2
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        assert_eq!(adapter.error_exit_code(), 2);
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
    fn test_windsurf_use_stderr_after_file_edit_block_false() {
        // Windsurf + AfterFileEdit + Block → stderr を使用しない（pre_run_command のみ）
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let decision = Decision::Block {
            message: "error".to_string(),
        };
        assert!(!adapter.use_stderr(&decision, HookEvent::AfterFileEdit));
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
        // Codex: PreToolUse + Bash → BeforeCommand
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hello"},
            "session_id": "test-session"
        }"#;
        let result = adapter.parse_input(input).unwrap();
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
        let result = adapter.parse_input(input).unwrap();
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
        let result = adapter.parse_input(input).unwrap();
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
        let result = adapter.parse_input(input).unwrap();
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
        let result = adapter.parse_input(input).unwrap();
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
        let err = adapter.parse_input(input).unwrap_err();
        assert!(
            err.to_string().contains("command"),
            "空コマンドは command 関連のエラーになるべき: {}",
            err
        );
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
        let result = adapter.parse_input(input).unwrap();
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
    fn test_claude_session_start_maps_to_before_prompt() {
        // SessionStart → BeforePrompt にマップ
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name": "SessionStart", "session_id": "s1"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
        assert_eq!(result.tool_name, "SessionStart");
    }

    #[test]
    fn test_claude_user_prompt_submit_maps_to_before_prompt() {
        // UserPromptSubmit → BeforePrompt にマップ
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name": "UserPromptSubmit", "session_id": "s1"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
        assert_eq!(result.tool_name, "UserPromptSubmit");
    }

    #[test]
    fn test_claude_session_end_maps_to_before_prompt() {
        // SessionEnd → BeforePrompt にマップ
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name": "SessionEnd", "session_id": "s1"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
        assert_eq!(result.tool_name, "SessionEnd");
    }

    #[test]
    fn test_claude_notification_maps_to_before_prompt() {
        // Notification → BeforePrompt にマップ
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input = r#"{"hook_event_name": "Notification", "session_id": "s1"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
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
            crate::domain::HookEvent::BeforePrompt
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
                HookEvent::BeforePrompt,
                "パススル���イベント {} は BeforePrompt にマッピングされるべき",
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
        assert_eq!(result.event, HookEvent::BeforePrompt);
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
    fn test_codex_input_missing_tool_name_for_tool_event_is_error() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{"hook_event_name":"PreToolUse","tool_input":{"command":"ls"}}"#;
        let err = adapter.parse_input(input).unwrap_err();
        assert!(err.to_string().contains("tool_name"));
    }

    #[test]
    fn test_codex_input_missing_command_in_bash_tool_is_error() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{}}"#;
        let err = adapter.parse_input(input).unwrap_err();
        assert!(err.to_string().contains("command"));
    }

    #[test]
    fn test_codex_input_unknown_event_passthrough() {
        // 未知のイベントはパススルーとして処理される
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input = r#"{"hook_event_name":"FutureEvent","data":"test"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
        assert_eq!(result.tool_name, "FutureEvent");
    }

    #[test]
    fn test_codex_input_stop_with_stop_hook_active() {
        let adapter = FormatAdapter::new(Format::Codex, 0);
        let input =
            r#"{"hook_event_name":"Stop","stop_hook_active":true,"last_assistant_message":"done"}"#;
        let result = adapter.parse_input(input).unwrap();
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
        let result = adapter.parse_input(input).unwrap();
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
            adapter.parse_input(input).is_err(),
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
            "hook_event_name":"PreToolUse",
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
        let input =
            r#"{"hook_event_name":"PreToolUse","toolCall":{"name":"run_command","args":{}}}"#;
        let err = adapter.parse_input(input).unwrap_err().to_string();
        assert!(err.contains("CommandLine"));
    }

    #[test]
    fn test_agy_input_parsing_pre_tool_use_empty_command_line_is_error() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{"hook_event_name":"PreToolUse","toolCall":{"name":"run_command","args":{"CommandLine":""}}}"#;
        let err = adapter.parse_input(input).unwrap_err().to_string();
        assert!(err.contains("CommandLine"));
    }

    #[test]
    fn test_agy_input_parsing_pre_tool_use_write_to_file_is_passthrough() {
        // Antigravity の write_to_file は claw-hooks のコマンドブロックの対象外。
        // BeforePrompt（パススルー）にマップされる。
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "hook_event_name":"PreToolUse",
            "toolCall":{"name":"write_to_file","args":{"TargetFile":"/workspace/foo.rs","Overwrite":false,"CodeContent":"fn main(){}"}}
        }"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
        assert_eq!(result.tool_name, "write_to_file");
    }

    #[test]
    fn test_agy_input_parsing_pre_tool_use_replace_file_content_passthrough() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "hook_event_name":"PreToolUse",
            "toolCall":{"name":"replace_file_content","args":{"TargetFile":"/workspace/foo.rs"}}
        }"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
        assert_eq!(result.tool_name, "replace_file_content");
    }

    #[test]
    fn test_agy_input_parsing_pre_tool_use_missing_tool_call_is_error() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{"hook_event_name":"PreToolUse"}"#;
        let err = adapter.parse_input(input).unwrap_err().to_string();
        assert!(err.contains("toolCall"));
    }

    #[test]
    fn test_agy_input_parsing_pre_tool_use_missing_name_is_error() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{"hook_event_name":"PreToolUse","toolCall":{"args":{}}}"#;
        let err = adapter.parse_input(input).unwrap_err().to_string();
        assert!(err.contains("toolCall.name"));
    }

    #[test]
    fn test_agy_input_parsing_post_tool_use_passthrough() {
        // PostToolUse には toolCall が無いため、claw-hooks の拡張子フック対象外。
        // パススルーで Allow を返す。
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "hook_event_name":"PostToolUse",
            "stepIdx":5,
            "error":"exit status 1",
            "conversationId":"abc-123"
        }"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
        assert_eq!(result.tool_name, "PostToolUse");
        assert_eq!(result.session_id, Some("abc-123".to_string()));
    }

    #[test]
    fn test_agy_input_parsing_pre_invocation_passthrough() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{"hook_event_name":"PreInvocation","invocationNum":3,"initialNumSteps":10}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
        assert_eq!(result.tool_name, "PreInvocation");
    }

    #[test]
    fn test_agy_input_parsing_post_invocation_passthrough() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input =
            r#"{"hook_event_name":"PostInvocation","invocationNum":3,"initialNumSteps":10}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
        assert_eq!(result.tool_name, "PostInvocation");
    }

    #[test]
    fn test_agy_input_parsing_stop() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "hook_event_name":"Stop",
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
        assert_eq!(result.event, HookEvent::BeforePrompt);
        assert_eq!(result.tool_name, "SomeNewEvent");
    }

    #[test]
    fn test_agy_input_parsing_missing_hook_event_name_is_error() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{"conversationId":"abc-123"}"#;
        let err = adapter.parse_input(input).unwrap_err().to_string();
        assert!(err.contains("hook_event_name"));
    }

    #[test]
    fn test_agy_input_parsing_event_alias_accepted() {
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let input = r#"{
            "event":"PreToolUse",
            "toolCall":{"name":"run_command","args":{"CommandLine":"ls"}}
        }"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
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
        // BeforePrompt（PreInvocation / PostInvocation / 未対応イベントが内部マップされる先）は
        // ブロック仕様が無いため常に {} を返す。
        let adapter = FormatAdapter::new(Format::Agy, 0);
        let allow = adapter
            .format_output(&Decision::allow(), HookEvent::BeforePrompt)
            .unwrap();
        assert_eq!(allow, "{}");
        let block = adapter
            .format_output(
                &Decision::Block {
                    message: "blocked".to_string(),
                },
                HookEvent::BeforePrompt,
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
        assert_eq!(adapter.error_exit_code(), 0);
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
            HookEvent::BeforePrompt,
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
            .or_else(|| raw.get("event"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Missing hook_event_name field"))?
            .to_string();

        let session_id = raw
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let event = match raw_event.as_str() {
            "Stop" | "stop" => HookEvent::Stop,
            "PreToolUse" | "pre_tool_use" | "BeforeTool" => HookEvent::BeforeCommand,
            "PermissionRequest" | "permission_request" => HookEvent::PermissionRequest,
            "PostToolUse" | "post_tool_use" | "AfterTool" => HookEvent::AfterFileEdit,
            "SubagentStart" | "subagent_start" => HookEvent::SubagentStart,
            "SubagentStop" | "subagent_stop" => HookEvent::SubagentStop,
            // claw-hooks の処理対象外イベント: パススルーで Allow を返す
            "SessionStart" | "session_start" | "UserPromptSubmit" | "user_prompt_submit" => {
                return Ok(HookInput {
                    event: HookEvent::BeforePrompt, // パススルー用
                    tool_name: raw_event,
                    tool_input: crate::domain::ToolInput::Other(raw),
                    session_id,
                });
            }
            other => {
                debug!(event = %other, "{} unknown event, treating as passthrough", self.log_prefix());
                return Ok(HookInput {
                    event: HookEvent::BeforePrompt, // パススルー用
                    tool_name: other.to_string(),
                    tool_input: crate::domain::ToolInput::Other(raw),
                    session_id,
                });
            }
        };

        if event == HookEvent::Stop {
            let agent_message = raw
                .get("last_assistant_message")
                .or_else(|| raw.get("stop_reason"))
                .or_else(|| raw.get("message"))
                .or_else(|| raw.get("response"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let stop_hook_active = raw
                .get("stop_hook_active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            return Ok(HookInput {
                event,
                tool_name: "Stop".to_string(),
                tool_input: crate::domain::ToolInput::Stop(crate::domain::StopInput {
                    status: None,
                    loop_count: None,
                    response: None,
                    agent_message,
                    stop_hook_active,
                }),
                session_id,
            });
        }

        if event == HookEvent::SubagentStart || event == HookEvent::SubagentStop {
            let subagent_type = raw
                .get("agent_type")
                .or_else(|| raw.get("subagent_type"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("Missing agent_type field"))?
                .to_string();

            return Ok(HookInput {
                event,
                tool_name: raw_event,
                tool_input: crate::domain::ToolInput::Subagent(crate::domain::SubagentInput {
                    subagent_type: Some(subagent_type),
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
            });
        }

        // ツールイベント
        let raw_tool_name = raw
            .get("tool_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Missing tool_name field"))?
            .to_string();

        // Codex のツール名を内部のツール名にマッピング
        let mapped_tool_name = match raw_tool_name.as_str() {
            "shell" | "run_command" | "execute" => "Bash".to_string(),
            "write_file" | "create_file" | "edit_file" => "Write".to_string(),
            "apply_patch" => "MultiEdit".to_string(),
            "read_file" => "Read".to_string(),
            other => other.to_string(),
        };

        let raw_tool_input = raw
            .get("tool_input")
            .ok_or_else(|| anyhow!("Missing tool_input field"))?;
        let tool_input = if raw_tool_name == "apply_patch" {
            // Bash 経路と同様に空文字列の command も fail-closed にする（必須フィールド扱い）。
            let command = raw_tool_input
                .get("command")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
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
//   - PreToolUse: toolCall { name, args }, stepIdx
//   - PostToolUse: stepIdx, error (toolCall は含まれない)
//   - PreInvocation / PostInvocation: invocationNum, initialNumSteps
//   - Stop: executionNum, terminationReason, error, fullyIdle
//
// 出力（stdout JSON）:
//   - PreToolUse: { decision: "allow|deny|ask|force_ask", reason?, permissionOverrides? }
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

        // hook_event_name が正規フィールド。event エイリアスも受ける。
        let raw_event = raw
            .get("hook_event_name")
            .or_else(|| raw.get("event"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Missing hook_event_name field"))?
            .to_string();

        // Antigravity は conversationId を会話 ID として渡す。
        // session_id エイリアスでも受ける（ローカルテストや他エージェントからの転送向け）。
        let session_id = raw
            .get("conversationId")
            .or_else(|| raw.get("session_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        match raw_event.as_str() {
            "PreToolUse" => self.parse_agy_pre_tool_use(&raw, session_id),
            "Stop" => Ok(Self::parse_agy_stop(&raw, session_id)),
            // PostToolUse は発火するが、ペイロードに toolCall が含まれない（stepIdx と error のみ）
            // ため、ファイル単位の拡張子フックは成立しない。PreInvocation / PostInvocation は
            // モデル呼び出し前後のオーケストレーション系で claw-hooks のスコープ外。
            // いずれもパススルーで Allow を返す。
            "PostToolUse" | "PreInvocation" | "PostInvocation" => {
                debug!(
                    agent = self.format.label(),
                    hook_event_name = %raw_event,
                    mapped_event = ?HookEvent::BeforePrompt,
                    "{} event is out of scope for claw-hooks, passing through", self.log_prefix()
                );
                Ok(HookInput {
                    event: HookEvent::BeforePrompt,
                    tool_name: raw_event,
                    tool_input: crate::domain::ToolInput::Other(raw),
                    session_id,
                })
            }
            other => {
                debug!(
                    agent = self.format.label(),
                    hook_event_name = other,
                    mapped_event = ?HookEvent::BeforePrompt,
                    "{} unsupported event, passing through", self.log_prefix()
                );
                Ok(HookInput {
                    event: HookEvent::BeforePrompt,
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
        let tool_call = raw
            .get("toolCall")
            .ok_or_else(|| anyhow!("Missing toolCall field"))?;
        let raw_tool_name = tool_call
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Missing toolCall.name field"))?
            .to_string();
        let raw_args = tool_call
            .get("args")
            .ok_or_else(|| anyhow!("Missing toolCall.args field"))?;

        match raw_tool_name.as_str() {
            // run_command: Antigravity のシェル実行ツール。args.CommandLine をコマンド本文として扱う。
            "run_command" => {
                let command = raw_args
                    .get("CommandLine")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
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
                    mapped_event = ?HookEvent::BeforePrompt,
                    raw_tool_name = other,
                    "{} tool out of scope for claw-hooks, passing through", self.log_prefix()
                );

                Ok(HookInput {
                    event: HookEvent::BeforePrompt,
                    tool_name: other.to_string(),
                    tool_input: crate::domain::ToolInput::Other(raw_args.clone()),
                    session_id,
                })
            }
        }
    }

    /// Antigravity の Stop イベントを内部 Stop HookInput に変換する。
    fn parse_agy_stop(raw: &serde_json::Value, session_id: Option<String>) -> HookInput {
        let termination_reason = raw
            .get("terminationReason")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // terminationReason が "error" のとき、Antigravity は別途 error フィールドに詳細メッセージを入れる。
        // 互換性のため、agent_message は error → terminationReason の順で取り出す。
        let agent_message = raw
            .get("error")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| termination_reason.clone());

        HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: crate::domain::ToolInput::Stop(crate::domain::StopInput {
                status: termination_reason,
                loop_count: None,
                response: None,
                agent_message,
                stop_hook_active: false,
            }),
            session_id,
        }
    }

    fn format_agy_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
        // Antigravity の出力スキーマはイベントによって異なる:
        //   - PreToolUse: {decision: "allow|deny|ask|force_ask", reason?, permissionOverrides?}
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
            // PostToolUse / 内部 BeforePrompt（PreInvocation / PostInvocation / 未対応ツール）は
            // ブロック仕様が無いため、claw-hooks 側で Block を検出しても {} に倒す（公式仕様準拠）。
            (HookEvent::AfterFileEdit, _) | (HookEvent::BeforePrompt, _) => serde_json::json!({}),
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
