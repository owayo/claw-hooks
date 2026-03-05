//! ビルトインコマンドフィルターの共通実装。

use super::Filter;
use crate::domain::parser::ShellParser;
use crate::domain::{Decision, HookEvent, HookInput};

/// ビルトインコマンド（rm, kill, dd）をブロックする共通フィルター。
pub struct BuiltinCommandFilter {
    enabled: bool,
    message: String,
    commands: &'static [&'static str],
    priority: u32,
    /// 追加のコマンド検出ロジック（KillFilter の xargs チェック用）
    extra_check: Option<fn(&str) -> bool>,
}

impl BuiltinCommandFilter {
    /// 新しい BuiltinCommandFilter を作成する。
    pub fn new(
        enabled: bool,
        custom_message: Option<String>,
        default_message: &str,
        commands: &'static [&'static str],
        priority: u32,
        extra_check: Option<fn(&str) -> bool>,
    ) -> Self {
        Self {
            enabled,
            message: custom_message.unwrap_or_else(|| default_message.to_string()),
            commands,
            priority,
            extra_check,
        }
    }

    /// コマンド文字列にブロック対象のコマンドが含まれるか判定する。
    fn contains_blocked_command(&self, command: &str) -> bool {
        let mut parser = ShellParser::new();
        let extracted = parser.extract_commands(command);

        if extracted
            .iter()
            .any(|cmd| self.commands.contains(&cmd.as_str()))
        {
            return true;
        }

        // 追加チェック（例: xargs 経由の kill）
        if let Some(check) = self.extra_check {
            return check(command);
        }

        false
    }
}

impl Filter for BuiltinCommandFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        if !self.enabled {
            return false;
        }

        if input.event != HookEvent::BeforeCommand || input.tool_name != "Bash" {
            return false;
        }

        if let Some(command) = input.bash_command() {
            return self.contains_blocked_command(command);
        }

        false
    }

    fn execute(&self, _input: &HookInput) -> Decision {
        Decision::Block {
            message: self.message.clone(),
        }
    }

    fn priority(&self) -> u32 {
        self.priority
    }
}
