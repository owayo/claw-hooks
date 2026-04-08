//! フック入出力用のコアドメイン型。

use serde::{Deserialize, Serialize};

/// フックイベント型。
///
/// エージェント非依存の方法でフックイベントの種類を表現する。
/// 各 AI コーディングエージェントは外部で異なるイベント名を使用するが、
/// 内部的には型安全性のためにこの統一列挙型を使用する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// コマンド実行前（rm, kill ブロッキングの対象）。
    ///
    /// 外部イベント名:
    /// - Claude Code: `PreToolUse`
    /// - Cursor: `ShellExecution`
    /// - Windsurf: `pre_run_command`
    /// - Gemini CLI: `BeforeTool`
    /// - Codex CLI: `PreToolUse`
    BeforeCommand,

    /// ファイル編集後（拡張フックの対象）。
    ///
    /// 外部イベント名:
    /// - Claude Code: `PostToolUse`
    /// - Cursor: `FileEdit`
    /// - Windsurf: `post_write_code`
    /// - Gemini CLI: `AfterTool`
    /// - Codex CLI: `PostToolUse`
    AfterFileEdit,

    /// エージェントループ停止。
    ///
    /// 外部イベント名:
    /// - Claude Code: `Stop`
    /// - Cursor: `Stop`
    /// - Windsurf: `post_cascade_response`
    /// - Gemini CLI: `AfterAgent`
    /// - Codex CLI: `Stop`
    Stop,

    /// ユーザープロンプト送信前（Gemini CLI のみ）。
    ///
    /// 外部イベント名:
    /// - Gemini CLI: `BeforeAgent`
    BeforePrompt,

    /// サブエージェント起動前。
    ///
    /// 外部イベント名:
    /// - Claude Code: `SubagentStart`
    /// - Cursor: `subagentStart`
    SubagentStart,

    /// サブエージェント終了後。
    ///
    /// 外部イベント名:
    /// - Claude Code: `SubagentStop`
    /// - Cursor: `subagentStop`
    SubagentStop,
}

/// AI エージェントから受信したフック入力。
///
/// 注意: この構造体は JSON から直接デシリアライズできない。
/// エージェント固有の JSON 形式をこの内部表現に変換するには
/// `FormatAdapter::parse_input()` を使用すること。
#[derive(Debug, Clone)]
pub struct HookInput {
    /// イベント型（エージェント非依存）。
    pub event: HookEvent,

    /// ツール名: "Bash", "Write", "Edit", "MultiEdit", "Read" 等。
    pub tool_name: String,

    /// ツール固有の入力
    pub tool_input: ToolInput,

    /// オプションのセッション識別子
    pub session_id: Option<String>,
}

impl HookInput {
    /// Bash コマンド入力からコマンド文字列を取得する。
    pub fn bash_command(&self) -> Option<&str> {
        if let ToolInput::Bash(bash) = &self.tool_input {
            Some(&bash.command)
        } else {
            None
        }
    }

    /// ファイル操作入力からファイルパスを取得する。
    #[allow(dead_code)]
    pub fn file_path(&self) -> Option<&str> {
        if let ToolInput::File(file) = &self.tool_input {
            Some(&file.file_path)
        } else {
            None
        }
    }
}

/// ツール固有の入力バリアント。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ToolInput {
    /// Bash コマンド入力
    Bash(BashInput),
    /// ファイル操作入力（Write, Edit, MultiEdit, Read）
    File(FileOperationInput),
    /// Stop イベント入力（エージェントループ終了）
    #[allow(dead_code)]
    Stop(StopInput),
    /// サブエージェントイベント入力（SubagentStart/SubagentStop）
    Subagent(SubagentInput),
    /// その他/未知のツール入力
    #[allow(dead_code)]
    Other(serde_json::Value),
}

/// Bash コマンド入力。
#[derive(Debug, Clone, Deserialize)]
pub struct BashInput {
    /// 実行するコマンド
    pub command: String,

    /// オプションの timeout（ミリ秒）
    #[serde(default)]
    #[allow(dead_code)]
    pub timeout: Option<u64>,
}

/// ファイル操作入力。
#[derive(Debug, Clone, Deserialize)]
pub struct FileOperationInput {
    /// ファイルパス
    pub file_path: String,

    /// オプションのコンテンツ（Write/Edit 用）
    #[serde(default)]
    #[allow(dead_code)]
    pub content: Option<String>,
}

/// サブエージェントイベント入力（SubagentStart/SubagentStop）。
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub struct SubagentInput {
    /// サブエージェントの種類（例: "generalPurpose", "explore", "shell"）
    #[serde(default)]
    pub subagent_type: Option<String>,

    /// サブエージェントに与えられたプロンプト（SubagentStart のみ）
    #[serde(default)]
    pub prompt: Option<String>,

    /// サブエージェントのステータス（SubagentStop のみ: "completed", "error"）
    #[serde(default)]
    pub status: Option<String>,

    /// 実行時間（ミリ秒、SubagentStop のみ）
    #[serde(default)]
    pub duration: Option<u64>,
}

/// Stop イベント入力。
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub struct StopInput {
    /// 停止ステータス（Cursor: "completed", "aborted", "error"）
    #[serde(default)]
    pub status: Option<String>,

    /// ループ回数（Cursor: 自動フォローアップの実行回数）
    #[serde(default)]
    pub loop_count: Option<u32>,

    /// レスポンスコンテンツ（Windsurf: 完全なカスケードレスポンス）
    #[serde(default)]
    pub response: Option<String>,

    /// エージェントの最後のメッセージ（Claude Code: last_assistant_message, Windsurf: response）
    #[serde(default)]
    pub agent_message: Option<String>,

    /// Stop フックが既にアクティブかどうか（無限ループ防止）
    #[serde(default)]
    pub stop_hook_active: bool,
}

/// AI エージェントに返されるフック出力。
#[derive(Debug, Clone, Serialize)]
pub struct HookOutput {
    /// 判定: "approve" または "block"
    pub decision: String,

    /// オプションのメッセージ（通常ブロック時に存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// Claude Code 用のフック固有出力（PostToolUse additionalContext）
    #[serde(rename = "hookSpecificOutput", skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

/// Claude Code 用のフック固有出力。
/// PreToolUse: permissionDecision / permissionDecisionReason
/// PostToolUse: additionalContext
#[derive(Debug, Clone, Serialize)]
pub struct HookSpecificOutput {
    /// フックイベント名
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,

    /// エージェントへの追加コンテキスト（例: lint 警告）
    #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,

    /// PreToolUse 用の権限判定: "allow" / "deny"
    #[serde(rename = "permissionDecision", skip_serializing_if = "Option::is_none")]
    pub permission_decision: Option<String>,

    /// PreToolUse 用のブロック理由
    #[serde(
        rename = "permissionDecisionReason",
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_decision_reason: Option<String>,
}

/// オプションのブロックメッセージ付き処理判定。
#[derive(Debug, Clone)]
pub enum Decision {
    /// オプションのコンテキスト付きで操作を許可
    Allow {
        /// エージェントに渡す追加コンテキスト（例: lint 警告）
        additional_context: Option<String>,
    },
    /// メッセージ付きで操作をブロック
    Block { message: String },
}

impl Default for Decision {
    fn default() -> Self {
        Decision::Allow {
            additional_context: None,
        }
    }
}

impl Decision {
    /// 追加コンテキストなしの Allow 判定を作成する。
    pub fn allow() -> Self {
        Decision::Allow {
            additional_context: None,
        }
    }

    /// エージェント向けの追加コンテキスト付き Allow 判定を作成する。
    pub fn allow_with_context(context: String) -> Self {
        Decision::Allow {
            additional_context: Some(context),
        }
    }

    /// 指定イベントに対して判定を HookOutput に変換する（Claude Code 形式）。
    ///
    /// - BeforeCommand (PreToolUse): hookSpecificOutput.permissionDecision を使用
    /// - AfterFileEdit (PostToolUse): hookSpecificOutput.additionalContext を使用
    /// - Stop: トップレベル decision/reason を使用（format_claude_output 側で処理）
    pub fn into_output(self, event: HookEvent) -> HookOutput {
        match self {
            Decision::Allow { additional_context } => {
                let hook_specific_output = match event {
                    // PreToolUse: hookSpecificOutput.permissionDecision = "allow"
                    HookEvent::BeforeCommand => Some(HookSpecificOutput {
                        hook_event_name: "PreToolUse".to_string(),
                        additional_context: None,
                        permission_decision: Some("allow".to_string()),
                        permission_decision_reason: None,
                    }),
                    // PostToolUse: hookSpecificOutput.additionalContext
                    HookEvent::AfterFileEdit => additional_context.map(|ctx| HookSpecificOutput {
                        hook_event_name: "PostToolUse".to_string(),
                        additional_context: Some(ctx),
                        permission_decision: None,
                        permission_decision_reason: None,
                    }),
                    _ => None,
                };

                HookOutput {
                    decision: "approve".to_string(),
                    message: None,
                    hook_specific_output,
                }
            }
            Decision::Block { message } => {
                let hook_specific_output = if event == HookEvent::BeforeCommand {
                    // PreToolUse: hookSpecificOutput.permissionDecision = "deny"
                    Some(HookSpecificOutput {
                        hook_event_name: "PreToolUse".to_string(),
                        additional_context: None,
                        permission_decision: Some("deny".to_string()),
                        permission_decision_reason: Some(message.clone()),
                    })
                } else {
                    None
                };

                HookOutput {
                    decision: "block".to_string(),
                    message: Some(message),
                    hook_specific_output,
                }
            }
        }
    }

    /// この判定の終了コードを取得する。
    ///
    /// - Allow: 0
    /// - Block: 2
    pub fn exit_code(&self) -> i32 {
        match self {
            Decision::Allow { .. } => 0,
            Decision::Block { .. } => 2,
        }
    }

    /// 別の判定から追加コンテキストをマージする。
    /// 両方にコンテキストがある場合は改行で結合される。
    #[allow(dead_code)]
    pub fn merge_context(self, other_context: Option<String>) -> Self {
        match self {
            Decision::Allow { additional_context } => {
                let merged = match (additional_context, other_context) {
                    (Some(a), Some(b)) => Some(format!("{}\n{}", a, b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
                Decision::Allow {
                    additional_context: merged,
                }
            }
            Decision::Block { message } => Decision::Block { message },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_event_equality() {
        assert_eq!(HookEvent::BeforeCommand, HookEvent::BeforeCommand);
        assert_eq!(HookEvent::AfterFileEdit, HookEvent::AfterFileEdit);
        assert_eq!(HookEvent::Stop, HookEvent::Stop);
        assert_eq!(HookEvent::BeforePrompt, HookEvent::BeforePrompt);
        assert_eq!(HookEvent::SubagentStart, HookEvent::SubagentStart);
        assert_eq!(HookEvent::SubagentStop, HookEvent::SubagentStop);

        assert_ne!(HookEvent::BeforeCommand, HookEvent::AfterFileEdit);
        assert_ne!(HookEvent::BeforeCommand, HookEvent::Stop);
        assert_ne!(HookEvent::BeforeCommand, HookEvent::BeforePrompt);
        assert_ne!(HookEvent::SubagentStart, HookEvent::SubagentStop);
    }

    #[test]
    fn test_hook_event_copy() {
        let event = HookEvent::BeforeCommand;
        let copied = event; // Copy（ムーブではない）
        assert_eq!(event, copied);
        // コピー後も両方使用可能
        assert_eq!(event, HookEvent::BeforeCommand);
        assert_eq!(copied, HookEvent::BeforeCommand);
    }

    #[test]
    fn test_hook_event_clone() {
        let event = HookEvent::AfterFileEdit;
        // Clone トレイトを明示的にテスト（Copy だけでなく）
        let cloned = Clone::clone(&event);
        assert_eq!(event, cloned);
    }

    #[test]
    fn test_hook_event_debug() {
        assert_eq!(format!("{:?}", HookEvent::BeforeCommand), "BeforeCommand");
        assert_eq!(format!("{:?}", HookEvent::AfterFileEdit), "AfterFileEdit");
        assert_eq!(format!("{:?}", HookEvent::Stop), "Stop");
        assert_eq!(format!("{:?}", HookEvent::BeforePrompt), "BeforePrompt");
        assert_eq!(format!("{:?}", HookEvent::SubagentStart), "SubagentStart");
        assert_eq!(format!("{:?}", HookEvent::SubagentStop), "SubagentStop");
    }

    // Decision::into_output() テスト

    #[test]
    fn test_decision_into_output_allow_before_command() {
        let decision = Decision::allow();
        let output = decision.into_output(HookEvent::BeforeCommand);

        assert_eq!(output.decision, "approve");
        assert!(output.message.is_none());
        // BeforeCommand では hookSpecificOutput に permissionDecision = "allow" が含まれる
        let hook_output = output
            .hook_specific_output
            .expect("BeforeCommand Allow には hookSpecificOutput が必要");
        assert_eq!(hook_output.hook_event_name, "PreToolUse");
        assert_eq!(hook_output.permission_decision, Some("allow".to_string()));
        assert!(hook_output.permission_decision_reason.is_none());
        assert!(hook_output.additional_context.is_none());
    }

    #[test]
    fn test_decision_into_output_allow_after_file_edit_no_context() {
        let decision = Decision::allow();
        let output = decision.into_output(HookEvent::AfterFileEdit);

        assert_eq!(output.decision, "approve");
        assert!(output.message.is_none());
        // 追加コンテキストがない場合 hookSpecificOutput なし
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_decision_into_output_allow_after_file_edit_with_context() {
        let decision = Decision::allow_with_context("Lint warning: unused variable".to_string());
        let output = decision.into_output(HookEvent::AfterFileEdit);

        assert_eq!(output.decision, "approve");
        assert!(output.message.is_none());
        // コンテキスト付き AfterFileEdit では hookSpecificOutput が存在するべき
        let hook_output = output
            .hook_specific_output
            .expect("Should have hookSpecificOutput");
        assert_eq!(hook_output.hook_event_name, "PostToolUse");
        assert_eq!(
            hook_output.additional_context,
            Some("Lint warning: unused variable".to_string())
        );
    }

    #[test]
    fn test_decision_into_output_allow_with_context_before_command() {
        // BeforeCommand では permissionDecision = "allow" が hookSpecificOutput に含まれる
        let decision = Decision::allow_with_context("Some context".to_string());
        let output = decision.into_output(HookEvent::BeforeCommand);

        assert_eq!(output.decision, "approve");
        let hook_output = output
            .hook_specific_output
            .expect("BeforeCommand Allow には hookSpecificOutput が必要");
        assert_eq!(hook_output.hook_event_name, "PreToolUse");
        assert_eq!(hook_output.permission_decision, Some("allow".to_string()));
    }

    #[test]
    fn test_decision_into_output_block() {
        let decision = Decision::Block {
            message: "Command blocked for safety".to_string(),
        };
        let output = decision.into_output(HookEvent::BeforeCommand);

        assert_eq!(output.decision, "block");
        assert_eq!(
            output.message,
            Some("Command blocked for safety".to_string())
        );
        // BeforeCommand Block では hookSpecificOutput に permissionDecision = "deny" が含まれる
        let hook_output = output
            .hook_specific_output
            .expect("BeforeCommand Block には hookSpecificOutput が必要");
        assert_eq!(hook_output.hook_event_name, "PreToolUse");
        assert_eq!(hook_output.permission_decision, Some("deny".to_string()));
        assert_eq!(
            hook_output.permission_decision_reason,
            Some("Command blocked for safety".to_string())
        );
    }

    #[test]
    fn test_decision_into_output_stop_event() {
        let decision = Decision::allow();
        let output = decision.into_output(HookEvent::Stop);

        assert_eq!(output.decision, "approve");
        // Stop イベントでは hookSpecificOutput なし
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_decision_into_output_before_prompt_event() {
        let decision = Decision::allow();
        let output = decision.into_output(HookEvent::BeforePrompt);

        assert_eq!(output.decision, "approve");
        // BeforePrompt イベントでは hookSpecificOutput なし
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_decision_into_output_block_after_file_edit_no_hook_specific() {
        // AfterFileEdit の Block では hookSpecificOutput を生成しない
        let decision = Decision::Block {
            message: "Error".to_string(),
        };
        let output = decision.into_output(HookEvent::AfterFileEdit);
        assert_eq!(output.decision, "block");
        assert!(
            output.hook_specific_output.is_none(),
            "AfterFileEdit Block には hookSpecificOutput を含めない"
        );
    }

    #[test]
    fn test_decision_into_output_allow_with_context_stop_discards() {
        // Stop ではコンテキストを無視し hookSpecificOutput を生成しない
        let decision = Decision::allow_with_context("ctx".to_string());
        let output = decision.into_output(HookEvent::Stop);
        assert_eq!(output.decision, "approve");
        assert!(
            output.hook_specific_output.is_none(),
            "Stop ではコンテキストを無視すべき"
        );
    }

    #[test]
    fn test_decision_exit_code() {
        let allow = Decision::allow();
        assert_eq!(allow.exit_code(), 0);

        let block = Decision::Block {
            message: "blocked".to_string(),
        };
        assert_eq!(block.exit_code(), 2);
    }

    #[test]
    fn test_decision_default_is_allow() {
        let decision: Decision = Decision::default();
        assert!(matches!(
            decision,
            Decision::Allow {
                additional_context: None
            }
        ));
    }

    #[test]
    fn test_decision_merge_context_both_some() {
        let decision = Decision::allow_with_context("first".to_string());
        let merged = decision.merge_context(Some("second".to_string()));
        if let Decision::Allow { additional_context } = merged {
            assert_eq!(additional_context, Some("first\nsecond".to_string()));
        } else {
            panic!("Expected Allow");
        }
    }

    #[test]
    fn test_decision_merge_context_first_some() {
        let decision = Decision::allow_with_context("only".to_string());
        let merged = decision.merge_context(None);
        if let Decision::Allow { additional_context } = merged {
            assert_eq!(additional_context, Some("only".to_string()));
        } else {
            panic!("Expected Allow");
        }
    }

    #[test]
    fn test_decision_merge_context_second_some() {
        let decision = Decision::allow();
        let merged = decision.merge_context(Some("new".to_string()));
        if let Decision::Allow { additional_context } = merged {
            assert_eq!(additional_context, Some("new".to_string()));
        } else {
            panic!("Expected Allow");
        }
    }

    #[test]
    fn test_decision_merge_context_both_none() {
        let decision = Decision::allow();
        let merged = decision.merge_context(None);
        if let Decision::Allow { additional_context } = merged {
            assert!(additional_context.is_none());
        } else {
            panic!("Expected Allow");
        }
    }

    #[test]
    fn test_decision_merge_context_block_ignores() {
        let decision = Decision::Block {
            message: "blocked".to_string(),
        };
        let merged = decision.merge_context(Some("context".to_string()));
        if let Decision::Block { message } = merged {
            assert_eq!(message, "blocked");
        } else {
            panic!("Expected Block");
        }
    }

    #[test]
    fn test_decision_into_output_subagent_start() {
        let decision = Decision::allow();
        let output = decision.into_output(HookEvent::SubagentStart);
        assert_eq!(output.decision, "approve");
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_decision_into_output_subagent_stop() {
        let decision = Decision::allow();
        let output = decision.into_output(HookEvent::SubagentStop);
        assert_eq!(output.decision, "approve");
        assert!(output.hook_specific_output.is_none());
    }
}
