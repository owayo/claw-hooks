//! テスト用の共通ヘルパー関数。

#![cfg(test)]

use crate::domain::{BashInput, HookEvent, HookInput, ToolInput};

/// BeforeCommand イベントの Bash ツール入力を作成する。
pub fn make_bash_input(command: &str) -> HookInput {
    HookInput {
        event: HookEvent::BeforeCommand,
        tool_name: "Bash".to_string(),
        tool_input: ToolInput::Bash(BashInput {
            command: command.to_string(),
            timeout: None,
        }),
        session_id: None,
    }
}
