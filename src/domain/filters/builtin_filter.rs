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

        if !matches!(
            input.event,
            HookEvent::BeforeCommand | HookEvent::PermissionRequest
        ) || input.tool_name != "Bash"
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::test_helpers::make_bash_input;
    use crate::domain::{FileOperationInput, ToolInput};

    fn make_test_filter(enabled: bool, extra: Option<fn(&str) -> bool>) -> BuiltinCommandFilter {
        BuiltinCommandFilter::new(
            enabled,
            None,
            "テストブロックメッセージ",
            &["testcmd", "testcmd2"],
            50,
            extra,
        )
    }

    #[test]
    fn test_enabled_filter_blocks_matching_command() {
        let filter = make_test_filter(true, None);
        let input = make_bash_input("testcmd --flag");
        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_disabled_filter_does_not_apply() {
        let filter = make_test_filter(false, None);
        let input = make_bash_input("testcmd --flag");
        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_non_matching_command_is_allowed() {
        let filter = make_test_filter(true, None);
        let input = make_bash_input("safe_command --flag");
        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_non_bash_tool_is_not_applied() {
        let filter = make_test_filter(true, None);
        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(FileOperationInput {
                file_path: "/tmp/test".to_string(),
                content: None,
            }),
            session_id: None,
        };
        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_after_file_edit_event_is_not_applied() {
        let filter = make_test_filter(true, None);
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "testcmd".to_string(),
                timeout: None,
            }),
            session_id: None,
        };
        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_execute_returns_block_with_message() {
        let filter = make_test_filter(true, None);
        let input = make_bash_input("testcmd");
        let decision = filter.execute(&input);
        match decision {
            Decision::Block { message } => {
                assert_eq!(message, "テストブロックメッセージ");
            }
            _ => panic!("Block判定が期待される"),
        }
    }

    #[test]
    fn test_custom_message_overrides_default() {
        let filter = BuiltinCommandFilter::new(
            true,
            Some("カスタムメッセージ".to_string()),
            "デフォルトメッセージ",
            &["testcmd"],
            50,
            None,
        );
        let input = make_bash_input("testcmd");
        let decision = filter.execute(&input);
        match decision {
            Decision::Block { message } => {
                assert_eq!(message, "カスタムメッセージ");
            }
            _ => panic!("Block判定が期待される"),
        }
    }

    #[test]
    fn test_priority_returns_configured_value() {
        let filter = make_test_filter(true, None);
        assert_eq!(filter.priority(), 50);
    }

    #[test]
    fn test_extra_check_triggers_block() {
        // extra_check が true を返す場合、コマンド名が一致しなくてもブロック
        fn always_block(_cmd: &str) -> bool {
            true
        }
        let filter = make_test_filter(true, Some(always_block));
        let input = make_bash_input("safe_command");
        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_extra_check_not_called_when_command_matches() {
        // コマンド名が一致した場合、extra_check は呼ばれずに即座にブロック
        fn should_not_reach(_cmd: &str) -> bool {
            // この関数が呼ばれなくてもテストは通る
            false
        }
        let filter = make_test_filter(true, Some(should_not_reach));
        let input = make_bash_input("testcmd");
        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_second_command_pattern_matches() {
        let filter = make_test_filter(true, None);
        let input = make_bash_input("testcmd2 --arg");
        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_chained_command_matches() {
        let filter = make_test_filter(true, None);
        let input = make_bash_input("echo hello && testcmd --flag");
        assert!(filter.applies_to(&input));
    }
}
