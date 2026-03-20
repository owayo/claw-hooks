//! AIコーディングエージェント向けフォーマットアダプター。
//!
//! 以下のエージェントの入力パースと出力フォーマットを提供する:
//! - Claude Code（デフォルト）
//! - Cursor
//! - Windsurf (Cascade)
//! - Codex CLI

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::cli::Format;
use crate::domain::{Decision, HookEvent, HookInput, normalize_lint_output, truncate_output};

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
            Format::Gemini => self.parse_gemini_input(input),
            Format::Codex => self.parse_codex_input(input),
        }
    }

    /// エージェントフォーマットに基づいて出力をフォーマットする。
    /// eventパラメータはClaude CodeのAfterFileEditでhookSpecificOutputを含めるために使用される。
    pub fn format_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
        match self.format {
            Format::Claude => self.format_claude_output(decision, event),
            Format::Cursor => self.format_cursor_output(decision, event),
            Format::Windsurf => self.format_windsurf_output(decision, event),
            Format::Gemini => self.format_gemini_output(decision),
            Format::Codex => self.format_codex_output(decision),
        }
    }

    /// 判定結果に対する終了コードを取得する。
    /// 注意: エージェントごとに終了コードのセマンティクスが異なる。
    /// - Claude/Windsurf: 0 = 許可, 2 = ブロック
    /// - Cursor: 0 = 許可/停止, 2 = ブロック（停止以外）
    /// - Gemini CLI: 0 = 成功（判定はJSON内）, 2 = システムエラーのみ
    pub fn exit_code(&self, decision: &Decision, event: HookEvent) -> i32 {
        match self.format {
            Format::Gemini => {
                // Gemini CLI: JSON出力が成功した場合は常に0を返す。
                // 判定（allow/deny）はJSONレスポンスで伝達される。
                // 終了コード2はシステムエラー専用（stderrがreasonとして使用される）。
                0
            }
            Format::Cursor if event == HookEvent::Stop => {
                // Cursor Stop: 判定はJSON内のfollowup_messageで伝達される。
                0
            }
            _ => decision.exit_code(),
        }
    }

    /// 出力をstdoutではなくstderrに書き込むべきかどうか。
    /// Windsurf Stop Blockはエラー出力にstderrを使用する。
    pub fn use_stderr(&self, decision: &Decision, event: HookEvent) -> bool {
        matches!(
            (&self.format, event, decision),
            (&Format::Windsurf, HookEvent::Stop, Decision::Block { .. })
        )
    }

    /// エラーメッセージを出力用にフォーマットする。
    /// 入力パース失敗時に使用される。
    /// セキュリティ: フェイルクローズド設計 - パースエラー時はブロックする。
    pub fn format_error(&self, message: &str) -> String {
        let error_message = format!("🚫 Hook error (fail-closed): {}", message);
        match self.format {
            Format::Claude | Format::Windsurf => {
                // ClaudeとWindsurfはdecisionとmessageで同じフォーマットを使用
                // セキュリティ: パースエラー時はブロック（フェイルクローズド設計）
                serde_json::json!({
                    "decision": "block",
                    "message": error_message
                })
                .to_string()
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
            Format::Gemini => {
                // Geminiはdecisionとreasonを使用
                // セキュリティ: パースエラー時は拒否（フェイルクローズド設計）
                serde_json::json!({
                    "decision": "deny",
                    "reason": error_message
                })
                .to_string()
            }
            Format::Codex => {
                // Codex: Claude Code と同様のフォーマットをデフォルトとする
                serde_json::json!({
                    "decision": "block",
                    "message": error_message
                })
                .to_string()
            }
        }
    }

    /// エラー時の終了コードを取得する（フェイルクローズド = ブロック = 終了コード2）。
    pub fn error_exit_code(&self) -> i32 {
        2 // Decision::Blockの終了コードと同じ
    }

    // === Claude Code フォーマット ===

    fn parse_claude_input(&self, input: &str) -> Result<HookInput> {
        debug!(raw_input = %input, "{} raw input", self.log_prefix());

        let claude_input: ClaudeInput = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Claude input: {}", e))?;

        let raw_event = claude_input.hook_event_name.clone();

        // Claude Codeのイベント名をHookEventにマッピング
        let event = match raw_event.as_str() {
            "PreToolUse" => HookEvent::BeforeCommand,
            "PostToolUse" => HookEvent::AfterFileEdit,
            "Stop" => HookEvent::Stop,
            "SubagentStart" => HookEvent::SubagentStart,
            "SubagentStop" => HookEvent::SubagentStop,
            other => return Err(anyhow!("Unknown Claude event: {}", other)),
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
            let raw: serde_json::Value = serde_json::from_str(input)
                .map_err(|e| anyhow!("Failed to re-parse raw input: {}", e))?;
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
            let tool_input = claude_input
                .tool_input
                .ok_or_else(|| anyhow!("Missing tool_input field"))?;
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
        // StopイベントではBlock判定に"message"ではなく"reason"を使用
        if event == HookEvent::Stop {
            let output = match decision {
                Decision::Allow { .. } => ClaudeStopOutput {
                    decision: "approve".to_string(),
                    reason: None,
                },
                Decision::Block { message } => {
                    let normalized = normalize_lint_output(message);
                    let truncated = truncate_output(&normalized, self.output_max_length);
                    ClaudeStopOutput {
                        decision: "block".to_string(),
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
        debug!(raw_input = %input, "{} raw input", self.log_prefix());

        let cursor_input: CursorInput = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Cursor input: {}", e))?;

        // フックタイプに基づいてCursorフォーマットを内部HookInputに変換
        match cursor_input {
            CursorInput::SubagentStart {
                subagent_type,
                prompt,
                ..
            } => {
                debug!(
                    agent = self.format.label(),
                    hook_type = "subagentStart",
                    subagent_type = %subagent_type,
                    mapped_event = ?HookEvent::SubagentStart,
                    "{} parsed input", self.log_prefix()
                );

                Ok(HookInput {
                    event: HookEvent::SubagentStart,
                    tool_name: "SubagentStart".to_string(),
                    tool_input: crate::domain::ToolInput::Subagent(crate::domain::SubagentInput {
                        subagent_type: Some(subagent_type),
                        prompt,
                        status: None,
                        duration: None,
                    }),
                    session_id: None,
                })
            }
            CursorInput::SubagentStop {
                subagent_type,
                subagent_status,
                duration,
                ..
            } => {
                debug!(
                    agent = self.format.label(),
                    hook_type = "subagentStop",
                    subagent_type = %subagent_type,
                    status = %subagent_status,
                    mapped_event = ?HookEvent::SubagentStop,
                    "{} parsed input", self.log_prefix()
                );

                Ok(HookInput {
                    event: HookEvent::SubagentStop,
                    tool_name: "SubagentStop".to_string(),
                    tool_input: crate::domain::ToolInput::Subagent(crate::domain::SubagentInput {
                        subagent_type: Some(subagent_type),
                        prompt: None,
                        status: Some(subagent_status),
                        duration,
                    }),
                    session_id: None,
                })
            }
            CursorInput::Stop { status, loop_count } => {
                debug!(
                    agent = self.format.label(),
                    hook_type = "stop",
                    status = %status,
                    loop_count = ?loop_count,
                    mapped_event = ?HookEvent::Stop,
                    "{} parsed input", self.log_prefix()
                );

                // CursorのstopフックはStopイベントに相当
                Ok(HookInput {
                    event: HookEvent::Stop,
                    tool_name: "Stop".to_string(),
                    tool_input: crate::domain::ToolInput::Stop(crate::domain::StopInput {
                        status: Some(status),
                        loop_count,
                        response: None,
                        agent_message: None,
                        stop_hook_active: false,
                    }),
                    session_id: None,
                })
            }
            CursorInput::ShellExecution { command, cwd } => {
                debug!(
                    agent = self.format.label(),
                    hook_type = "beforeShellExecution",
                    command = %command,
                    cwd = ?cwd,
                    mapped_event = ?HookEvent::BeforeCommand,
                    mapped_tool = "Bash",
                    "{} parsed input", self.log_prefix()
                );

                // CursorのbeforeShellExecutionはBashのBeforeCommandに相当
                Ok(HookInput {
                    event: HookEvent::BeforeCommand,
                    tool_name: "Bash".to_string(),
                    tool_input: crate::domain::ToolInput::Bash(crate::domain::BashInput {
                        command,
                        timeout: None,
                    }),
                    session_id: None,
                })
            }
            CursorInput::FileEdit { file_path } => {
                debug!(
                    agent = self.format.label(),
                    hook_type = "afterFileEdit",
                    file_path = %file_path,
                    mapped_event = ?HookEvent::AfterFileEdit,
                    mapped_tool = "Write",
                    "{} parsed input", self.log_prefix()
                );

                // CursorのafterFileEditはWriteのAfterFileEditに対応する
                Ok(HookInput {
                    event: HookEvent::AfterFileEdit,
                    tool_name: "Write".to_string(),
                    tool_input: crate::domain::ToolInput::File(crate::domain::FileOperationInput {
                        file_path,
                        content: None,
                    }),
                    session_id: None,
                })
            }
        }
    }

    fn format_cursor_output(&self, decision: &Decision, event: HookEvent) -> Result<String> {
        // Stop Blockイベントは"followup_message"を使用してエージェントに修正を指示する
        if event == HookEvent::Stop {
            if let Decision::Block { message } = decision {
                let normalized = normalize_lint_output(message);
                let truncated = truncate_output(&normalized, self.output_max_length);
                let output = CursorStopOutput {
                    followup_message: truncated,
                };
                return serde_json::to_string(&output)
                    .map_err(|e| anyhow!("Failed to serialize Cursor output: {}", e));
            }
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
        debug!(raw_input = %input, "{} raw input", self.log_prefix());

        let windsurf_input: WindsurfInput = serde_json::from_str(input)
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
                return Err(anyhow!("Unknown Windsurf action: {}", other));
            }
        };

        debug!(
            agent = self.format.label(),
            agent_action_name = %windsurf_input.agent_action_name,
            mapped_event = ?event,
            mapped_tool = %tool_name,
            cwd = ?windsurf_input.tool_info.as_ref().and_then(|ti| ti.cwd.as_ref()),
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
        // Stop の Block は標準エラー向けにプレーンテキストを返す。
        // Windsurf は終了コード 2 のとき stderr を読む。
        if event == HookEvent::Stop {
            if let Decision::Block { message } = decision {
                let normalized = normalize_lint_output(message);
                return Ok(truncate_output(&normalized, self.output_max_length));
            }
        }

        // Windsurf は Claude Code と同系統の出力形式を使うが、
        // additionalContext / hookSpecificOutput はサポートしないため簡略化する。
        let output = match &self.truncate_decision(decision) {
            Decision::Allow { .. } => crate::domain::HookOutput {
                decision: "approve".to_string(),
                message: None,
                hook_specific_output: None,
            },
            Decision::Block { message } => crate::domain::HookOutput {
                decision: "block".to_string(),
                message: Some(message.clone()),
                hook_specific_output: None,
            },
        };
        serde_json::to_string(&output).map_err(|e| anyhow!("Failed to serialize output: {}", e))
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
    tool_input: Option<crate::domain::ToolInput>,

    /// セッション識別子
    #[serde(default)]
    session_id: Option<String>,

    /// このセッションで stop hooks が既に有効かどうか
    #[serde(default)]
    #[allow(dead_code)]
    stop_hook_active: Option<bool>,

    /// エージェントの最後のメッセージ（Stop イベント）
    #[serde(default)]
    last_assistant_message: Option<String>,
}

/// Claude Code の Stop イベント出力フォーマット。
/// Stop イベントの Block 判定では "message" ではなく "reason" を使う。
/// これにより Claude は停止せず、"reason" に従って続行する。
#[derive(Debug, Serialize)]
struct ClaudeStopOutput {
    /// 判定: "approve" または "block"
    decision: String,
    /// ブロック理由（エージェントへの修正指示）
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

// === Cursor フォーマット型 ===

/// Cursor の入力フォーマット。
/// beforeShellExecution、afterFileEdit、stop、subagent hooks に対応する。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CursorInput {
    // 注意: serde(untagged) は上から順に評価するため、
    // SubagentStop は SubagentStart より前に置く必要がある。
    // SubagentStop は "status" を要求するが、SubagentStart は
    // "subagent_type" だけでも一致してしまうため。
    /// subagentStop hook - サブエージェントの終了
    SubagentStop {
        /// サブエージェント種別
        subagent_type: String,
        /// サブエージェント完了状態: "completed" または "error"
        #[serde(rename = "status")]
        subagent_status: String,
        /// サブエージェント出力
        #[serde(default)]
        #[allow(dead_code)]
        result: Option<String>,
        /// 実行時間（ミリ秒）
        #[serde(default)]
        duration: Option<u64>,
    },
    /// subagentStart hook - サブエージェント起動
    SubagentStart {
        /// サブエージェント種別: "generalPurpose", "explore", "shell" など
        subagent_type: String,
        /// サブエージェントに渡されたプロンプト
        #[serde(default)]
        prompt: Option<String>,
        /// サブエージェントで使用したモデル
        #[serde(default)]
        #[allow(dead_code)]
        model: Option<String>,
    },
    /// stop hook - エージェントループ終了
    Stop {
        /// 停止状態: "completed", "aborted", "error"
        status: String,
        /// この会話で発生した自動フォローアップ回数
        #[serde(default)]
        loop_count: Option<u32>,
    },
    /// beforeShellExecution hook - 実行対象コマンドを含む
    ShellExecution {
        /// 実行するコマンド
        command: String,
        /// 現在の作業ディレクトリ
        #[serde(default)]
        #[allow(dead_code)]
        cwd: Option<String>,
    },
    /// afterFileEdit hook - 編集されたファイルパスを含む
    FileEdit {
        /// 編集されたファイルのパス
        #[serde(alias = "filePath")]
        file_path: String,
    },
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
        let input = r#"{"command":"rm -rf /tmp/test","cwd":"/path/to/project"}"#;
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
        let input = r#"{"file_path":"/path/to/file.rs"}"#;
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
        let input = r#"{"filePath":"/path/to/file.tsx"}"#;
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
        assert!(output.contains(r#""decision":"approve""#));
        // BeforeCommand には hookSpecificOutput がない
        assert!(!output.contains("hookSpecificOutput"));
    }

    #[test]
    fn test_claude_output_allow_with_context() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let decision = Decision::allow_with_context("Lint warning: unused variable".to_string());
        let output = adapter
            .format_output(&decision, HookEvent::AfterFileEdit)
            .unwrap();
        assert!(output.contains(r#""decision":"approve""#));
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
        assert!(output.contains(r#""decision":"block""#));
        assert!(output.contains("Command blocked for safety"));
    }

    #[test]
    fn test_cursor_input_parsing_stop() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let input = r#"{"status":"completed","loop_count":3}"#;
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
        let input = r#"{"status":"aborted"}"#;
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

    // === Gemini CLI フォーマットのテスト ===

    #[test]
    fn test_gemini_input_parsing_before_tool() {
        // Gemini CLI 公式のツール名 run_shell_command を使う
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let input = r#"{"hook_event_name":"BeforeTool","tool_name":"run_shell_command","tool_input":{"command":"rm -rf /tmp/test"}}"#;
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
    fn test_gemini_input_parsing_run_shell_command() {
        // run_shell_command は Gemini CLI の公式組み込みツール名
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let input = r#"{"hook_event_name":"BeforeTool","tool_name":"run_shell_command","tool_input":{"command":"git status"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
    }

    #[test]
    fn test_gemini_input_parsing_after_tool() {
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let input = r#"{"hook_event_name":"AfterTool","tool_name":"write_file","tool_input":{"file_path":"/path/to/file.rs"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::AfterFileEdit);
        assert_eq!(result.tool_name, "Write");
    }

    #[test]
    fn test_gemini_input_parsing_replace_tool() {
        // Gemini CLIは既存ファイルの編集に"replace"ツールを使用する
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let input = r#"{"hook_event_name":"AfterTool","tool_name":"replace","tool_input":{"file_path":"/path/to/file.rs"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::AfterFileEdit);
        assert_eq!(result.tool_name, "Write"); // replace は Write として扱う
    }

    #[test]
    fn test_gemini_input_parsing_after_agent() {
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let input = r#"{"hook_event_name":"AfterAgent"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::Stop);
        assert_eq!(result.tool_name, "Stop");
    }

    #[test]
    fn test_gemini_input_parsing_before_agent() {
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let input = r#"{"hook_event_name":"BeforeAgent"}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforePrompt);
        assert_eq!(result.tool_name, "UserPrompt");
    }

    #[test]
    fn test_gemini_input_parsing_with_event_alias() {
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        // Gemini が "hook_event_name" の代わりに "event" を使う場合がある
        let input = r#"{"event":"BeforeTool","tool_name":"run_shell_command","tool_input":{"command":"ls"}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "Bash");
    }

    #[test]
    fn test_gemini_input_parsing_unknown_tool() {
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let input = r#"{"hook_event_name":"BeforeTool","tool_name":"custom_tool","tool_input":{}}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(result.event, HookEvent::BeforeCommand);
        assert_eq!(result.tool_name, "custom_tool"); // 不明なツールはそのまま保持
    }

    #[test]
    fn test_gemini_output_allow() {
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::BeforeCommand)
            .unwrap();
        assert!(output.contains(r#""decision":"allow""#));
        assert!(!output.contains("reason"));
    }

    #[test]
    fn test_gemini_output_deny() {
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "🚫 rm command blocked. Use safe-rm instead.".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        assert!(output.contains(r#""decision":"deny""#));
        assert!(output.contains("reason"));
        assert!(output.contains("rm command blocked"));
    }

    #[test]
    fn test_gemini_error_format() {
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let error_output = adapter.format_error("Invalid JSON input");
        assert!(error_output.contains(r#""decision":"deny""#));
        assert!(error_output.contains("reason"));
        assert!(error_output.contains("fail-closed"));
    }

    // === Windsurf 出力のテスト ===

    #[test]
    fn test_windsurf_output_allow() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::BeforeCommand)
            .unwrap();
        assert!(output.contains(r#""decision":"approve""#));
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
        assert!(output.contains(r#""decision":"block""#));
        assert!(output.contains("Command blocked for safety"));
    }

    #[test]
    fn test_windsurf_output_allow_after_file_edit() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        // WindsurfはadditionalContextをサポートしないため、コンテキストは無視される
        let decision = Decision::allow_with_context("Some lint warning".to_string());
        let output = adapter
            .format_output(&decision, HookEvent::AfterFileEdit)
            .unwrap();
        assert!(output.contains(r#""decision":"approve""#));
        // Windsurf は additionalContext をサポートしない
        assert!(!output.contains("hookSpecificOutput"));
        assert!(!output.contains("additionalContext"));
    }

    #[test]
    fn test_windsurf_error_format() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let error_output = adapter.format_error("Invalid JSON input");
        assert!(error_output.contains(r#""decision":"block""#));
        assert!(error_output.contains("fail-closed"));
    }

    // === Claude Code / Gemini CLI の Stop Block 出力 ===

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
        assert!(output.contains(r#""decision":"approve""#));
        // Allowにはreasonフィールドがない
        assert!(!output.contains(r#""reason""#));
    }

    #[test]
    fn test_claude_output_before_command_block_still_uses_message() {
        // Stop以外のイベントは引き続き"message"フィールドを使用する
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "Command blocked".to_string(),
                },
                HookEvent::BeforeCommand,
            )
            .unwrap();
        assert!(output.contains(r#""decision":"block""#));
        assert!(output.contains(r#""message""#));
        assert!(!output.contains(r#""reason""#));
    }

    #[test]
    fn test_gemini_output_stop_block() {
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "lint errors found".to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        assert!(output.contains(r#""decision":"deny""#));
        assert!(output.contains(r#""reason""#));
        assert!(output.contains("lint errors found"));
    }

    #[test]
    fn test_gemini_output_stop_allow() {
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::Stop)
            .unwrap();
        assert!(output.contains(r#""decision":"allow""#));
        assert!(!output.contains(r#""reason""#));
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
        assert!(reason.contains("error: unused"));
        assert!(reason.contains("--> src/main.rs:1:1"));
        assert!(!reason.contains("    error"));
        assert!(!reason.contains("        -->"));
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

    #[test]
    fn test_normalize_gemini_strips_ansi_and_whitespace() {
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let message = "  \x1b[1;31merror\x1b[0m: type mismatch\n    expected `u32`".to_string();
        let output = adapter
            .format_output(&Decision::Block { message }, HookEvent::Stop)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let reason = parsed["reason"].as_str().unwrap();
        assert!(reason.contains("error: type mismatch"));
        assert!(reason.contains("expected `u32`"));
        assert!(!reason.contains("\x1b"));
        assert!(!reason.starts_with(' '));
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
    fn test_cursor_output_stop_allow_unchanged() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::Stop)
            .unwrap();
        // Stop の Allow は標準の allow 形式を使う
        assert!(output.contains(r#""permission":"allow""#));
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
    fn test_windsurf_output_stop_block_plain_text() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let output = adapter
            .format_output(
                &Decision::Block {
                    message: "cargo clippy failed\nerror: unused variable".to_string(),
                },
                HookEvent::Stop,
            )
            .unwrap();
        // Stop の Block は stderr 向けにプレーンテキストを返す
        assert!(!output.starts_with('{'));
        assert!(output.contains("unused variable"));
    }

    #[test]
    fn test_windsurf_output_stop_allow_unchanged() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::Stop)
            .unwrap();
        // Stop の Allow は標準の approve 形式を使う
        assert!(output.contains(r#""decision":"approve""#));
    }

    #[test]
    fn test_windsurf_output_stop_block_normalizes_output() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let message = "  \x1b[31merror\x1b[0m: unused\n\n\n    --> src/main.rs:1:1".to_string();
        let output = adapter
            .format_output(&Decision::Block { message }, HookEvent::Stop)
            .unwrap();
        // ANSI 除去、先頭空白除去、連続空行圧縮が効くこと
        assert!(output.contains("error: unused"));
        assert!(!output.contains("\x1b"));
        assert!(output.contains("--> src/main.rs:1:1"));
        assert!(!output.contains("    -->"));
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
    fn test_windsurf_use_stderr_for_stop_block() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let block = Decision::Block {
            message: "error".to_string(),
        };
        assert!(adapter.use_stderr(&block, HookEvent::Stop));
    }

    #[test]
    fn test_windsurf_use_stdout_for_non_stop() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let block = Decision::Block {
            message: "error".to_string(),
        };
        assert!(!adapter.use_stderr(&block, HookEvent::BeforeCommand));
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
        let input = r#"{"subagent_type":"explore","prompt":"Explore the authentication flow","model":"claude-sonnet-4-20250514"}"#;
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
    fn test_cursor_input_parsing_subagent_stop() {
        let adapter = FormatAdapter::new(Format::Cursor, 0);
        let input = r#"{"subagent_type":"generalPurpose","status":"completed","result":"Task done","duration":45000}"#;
        let result = adapter.parse_input(input).unwrap();
        assert_eq!(
            result.event,
            HookEvent::SubagentStop,
            "SubagentStart ではなく SubagentStop として解釈されるべき"
        );
        assert_eq!(result.tool_name, "SubagentStop");
        if let crate::domain::ToolInput::Subagent(ref sub) = result.tool_input {
            assert_eq!(sub.subagent_type, Some("generalPurpose".to_string()));
            assert_eq!(sub.status, Some("completed".to_string()));
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
        assert!(output.contains(r#""decision":"approve""#));
    }

    #[test]
    fn test_claude_output_subagent_stop_allow() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let output = adapter
            .format_output(&Decision::allow(), HookEvent::SubagentStop)
            .unwrap();
        assert!(output.contains(r#""decision":"approve""#));
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
    fn test_claude_parse_unknown_event_is_error() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        let input =
            r#"{"hook_event_name":"Unknown","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
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
        // 空オブジェクトはどのCursorInputバリアントにもマッチしない
        let input = r#"{}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_windsurf_unknown_action_is_error() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        let input = r#"{"agent_action_name":"unknown_action","tool_info":{}}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_windsurf_parse_invalid_json_is_error() {
        let adapter = FormatAdapter::new(Format::Windsurf, 0);
        assert!(adapter.parse_input("{invalid}").is_err());
    }

    #[test]
    fn test_gemini_parse_missing_tool_name_for_tool_event_is_error() {
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let input = r#"{"hook_event_name":"BeforeTool"}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_gemini_parse_missing_tool_input_for_tool_event_is_error() {
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let input = r#"{"hook_event_name":"BeforeTool","tool_name":"run_shell_command"}"#;
        assert!(adapter.parse_input(input).is_err());
    }

    #[test]
    fn test_gemini_unknown_event_is_error() {
        let adapter = FormatAdapter::new(Format::Gemini, 0);
        let input = r#"{"hook_event_name":"UnknownEvent"}"#;
        assert!(adapter.parse_input(input).is_err());
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
        assert!(output.contains(r#""decision":"block""#));
        assert!(output.contains("fail-closed"));
    }

    #[test]
    fn test_error_exit_code() {
        let adapter = FormatAdapter::new(Format::Claude, 0);
        assert_eq!(adapter.error_exit_code(), 2);
    }
}

// === Gemini CLI フォーマット型 ===

/// Gemini CLI の入力フォーマット。
/// 参照: https://github.com/google-gemini/gemini-cli#hooks
/// 基本スキーマには session_id、transcript_path、cwd、hook_event_name、timestamp が含まれる。
/// ツールイベントでは tool_name、tool_input、mcp_context が追加される。
#[derive(Debug, Deserialize)]
struct GeminiInput {
    /// フックイベント名: BeforeTool, AfterTool, AfterAgent など
    #[serde(alias = "event")]
    hook_event_name: String,

    /// ツール名（ツール以外のイベントでは省略可）
    #[serde(default)]
    tool_name: Option<String>,

    /// ツール入力（ツール以外のイベントでは省略可）
    #[serde(default)]
    tool_input: Option<crate::domain::ToolInput>,

    /// セッション識別子
    #[serde(default)]
    session_id: Option<String>,

    /// 現在の作業ディレクトリ（基本入力フィールド）
    #[serde(default)]
    #[allow(dead_code)]
    cwd: Option<String>,

    /// セッショントランスクリプト JSON の絶対パス（基本入力フィールド）
    #[serde(default)]
    #[allow(dead_code)]
    transcript_path: Option<String>,

    /// ISO 8601 形式の実行時刻（基本入力フィールド）
    #[serde(default)]
    #[allow(dead_code)]
    timestamp: Option<String>,

    /// MCP ベースツール向けのコンテキスト（任意）
    #[serde(default)]
    #[allow(dead_code)]
    mcp_context: Option<serde_json::Value>,

    /// AfterTool イベントのツール応答
    #[serde(default)]
    #[allow(dead_code)]
    tool_response: Option<serde_json::Value>,

    /// BeforeAgent / AfterAgent イベントのユーザープロンプト
    #[serde(default)]
    #[allow(dead_code)]
    prompt: Option<String>,

    /// AfterAgent イベントのエージェント応答
    #[serde(default)]
    #[allow(dead_code)]
    prompt_response: Option<String>,

    /// AfterAgent イベントでの stop hook 有効フラグ
    #[serde(default)]
    #[allow(dead_code)]
    stop_hook_active: Option<bool>,
}

/// Gemini CLI の出力フォーマット。
#[derive(Debug, Serialize)]
struct GeminiOutput {
    /// 判定: "allow" または "deny"
    decision: String,

    /// 拒否理由（decision が "deny" のとき）
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl FormatAdapter {
    // === Gemini CLI フォーマット ===

    fn parse_gemini_input(&self, input: &str) -> Result<HookInput> {
        debug!(raw_input = %input, "{} raw input", self.log_prefix());

        let gemini_input: GeminiInput = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Gemini input: {}", e))?;

        let raw_event = gemini_input.hook_event_name.clone();

        // Geminiイベントを内部のHookEventにマッピング
        let event = match raw_event.as_str() {
            "BeforeTool" => HookEvent::BeforeCommand,
            "AfterTool" => HookEvent::AfterFileEdit,
            "AfterAgent" => HookEvent::Stop,
            "BeforeAgent" => HookEvent::BeforePrompt,
            other => return Err(anyhow!("Unknown Gemini event: {}", other)),
        };

        // ツール以外のイベントを処理
        if event == HookEvent::Stop || event == HookEvent::BeforePrompt {
            let tool_name = if event == HookEvent::Stop {
                "Stop".to_string()
            } else {
                "UserPrompt".to_string()
            };

            debug!(
                agent = self.format.label(),
                raw_event = %raw_event,
                mapped_event = ?event,
                "{} parsed input (non-tool event)", self.log_prefix()
            );

            return Ok(HookInput {
                event,
                tool_name,
                tool_input: crate::domain::ToolInput::Stop(crate::domain::StopInput {
                    status: None,
                    loop_count: None,
                    response: None,
                    agent_message: gemini_input.prompt_response.clone(),
                    stop_hook_active: gemini_input.stop_hook_active.unwrap_or(false),
                }),
                session_id: gemini_input.session_id,
            });
        }

        // ツールイベントにはtool_nameとtool_inputが必要
        let raw_tool_name = gemini_input
            .tool_name
            .ok_or_else(|| anyhow!("Missing tool_name field"))?;
        let tool_input = gemini_input
            .tool_input
            .ok_or_else(|| anyhow!("Missing tool_input field"))?;

        // Gemini のツール名を内部のツール名にマッピングする。
        // 参照: https://ai.google.dev/gemini-api/docs/tools
        let tool_name = match raw_tool_name.as_str() {
            // シェル実行
            "shell" | "run_shell_command" | "execute_command" => "Bash".to_string(),
            // ファイル書き込み・編集。replace は既存ファイル編集で使われる。
            "write_file" | "create_file" | "update_file" | "replace" | "edit_file" => {
                "Write".to_string()
            }
            // ファイル読み取り
            "read_file" | "view_file" | "read_many_files" => "Read".to_string(),
            // 不明なツールはそのまま保持
            other => other.to_string(),
        };

        debug!(
            agent = self.format.label(),
            raw_event = %raw_event,
            mapped_event = ?event,
            raw_tool_name = %raw_tool_name,
            mapped_tool = %tool_name,
            "{} parsed input", self.log_prefix()
        );

        Ok(HookInput {
            event,
            tool_name,
            tool_input,
            session_id: gemini_input.session_id,
        })
    }

    fn format_gemini_output(&self, decision: &Decision) -> Result<String> {
        let output = match decision {
            Decision::Allow { .. } => GeminiOutput {
                decision: "allow".to_string(),
                reason: None,
            },
            Decision::Block { message } => {
                let normalized = normalize_lint_output(message);
                let truncated = truncate_output(&normalized, self.output_max_length);
                GeminiOutput {
                    decision: "deny".to_string(),
                    reason: Some(truncated),
                }
            }
        };
        serde_json::to_string(&output)
            .map_err(|e| anyhow!("Failed to serialize Gemini output: {}", e))
    }

    // === Codex CLI フォーマット ===

    fn parse_codex_input(&self, input: &str) -> Result<HookInput> {
        debug!(raw_input = %input, "{} raw input", self.log_prefix());

        // Codex CLI のJSON構造はまだ未公開のため、まずは生のJSONをログに出力し、
        // Claude Code互換のフィールドがあればパースを試みる。
        // フォールバックとして serde_json::Value でパースして柔軟に対応する。
        let raw: serde_json::Value = serde_json::from_str(input)
            .map_err(|e| anyhow!("Failed to parse Codex input: {}", e))?;

        debug!(parsed = %raw, "{} parsed JSON", self.log_prefix());

        // イベント名の検出: "hook_event_name" または "event" フィールドを探す
        let raw_event = raw
            .get("hook_event_name")
            .or_else(|| raw.get("event"))
            .and_then(|v| v.as_str())
            .unwrap_or("Stop")
            .to_string();

        let event = match raw_event.as_str() {
            "Stop" | "stop" => HookEvent::Stop,
            "PreToolUse" | "pre_tool_use" | "BeforeTool" => HookEvent::BeforeCommand,
            "PostToolUse" | "post_tool_use" | "AfterTool" => HookEvent::AfterFileEdit,
            other => {
                debug!(event = %other, "{} unknown event, treating as Stop", self.log_prefix());
                HookEvent::Stop
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
                session_id: raw
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            });
        }

        // ツールイベント
        let tool_name = raw
            .get("tool_name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let tool_input: crate::domain::ToolInput = if let Some(ti) = raw.get("tool_input") {
            serde_json::from_value(ti.clone())
                .map_err(|e| anyhow!("Failed to parse Codex tool_input: {}", e))?
        } else {
            // tool_input がなければコマンドフィールドを探す
            let command = raw.get("command").and_then(|v| v.as_str()).unwrap_or("");
            crate::domain::ToolInput::Bash(crate::domain::BashInput {
                command: command.to_string(),
                timeout: None,
            })
        };

        // Codex のツール名を内部のツール名にマッピング
        let mapped_tool_name = match tool_name.as_str() {
            "shell" | "run_command" | "execute" => "Bash".to_string(),
            "write_file" | "create_file" | "edit_file" | "apply_patch" => "Write".to_string(),
            "read_file" => "Read".to_string(),
            other => other.to_string(),
        };

        debug!(
            agent = self.format.label(),
            raw_event = %raw_event,
            mapped_event = ?event,
            raw_tool_name = %tool_name,
            mapped_tool = %mapped_tool_name,
            "{} parsed input", self.log_prefix()
        );

        Ok(HookInput {
            event,
            tool_name: mapped_tool_name,
            tool_input,
            session_id: raw
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        })
    }

    fn format_codex_output(&self, decision: &Decision) -> Result<String> {
        // Codex CLI の出力フォーマットも未公開のため、
        // Claude Code と同様のプレーンテキスト出力をデフォルトとする。
        // JSON出力が必要と判明した場合は後で変更する。
        match decision {
            Decision::Allow { .. } => Ok(String::new()),
            Decision::Block { message } => {
                let normalized = normalize_lint_output(message);
                let truncated = truncate_output(&normalized, self.output_max_length);
                Ok(truncated)
            }
        }
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
}
