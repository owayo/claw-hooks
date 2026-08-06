//! ビルトインコマンドフィルターの共通実装。

use super::Filter;
use crate::domain::parser::{ShellParser, command_key};
use crate::domain::{Decision, HookEvent, HookInput};

/// ビルトインコマンド（rm, kill, dd）をブロックする共通フィルター。
pub struct BuiltinCommandFilter {
    enabled: bool,
    message: String,
    commands: &'static [&'static str],
    priority: u32,
}

impl BuiltinCommandFilter {
    /// 新しい BuiltinCommandFilter を作成する。
    pub fn new(
        enabled: bool,
        custom_message: Option<String>,
        default_message: &str,
        commands: &'static [&'static str],
        priority: u32,
    ) -> Self {
        Self {
            enabled,
            message: custom_message.unwrap_or_else(|| default_message.to_string()),
            commands,
            priority,
        }
    }

    /// コマンド文字列にブロック対象のコマンドが含まれるか判定する。
    ///
    /// 検出はパーサーの `extract_commands` に一本化する。パーサーは xargs
    /// （`xargs -I` / `xargs sh -c` 等）を含む実行委譲を展開して内側コマンドを
    /// 抽出済みのため、xargs 経由の kill もこの主経路で検出される。
    fn contains_blocked_command(&self, command: &str) -> bool {
        let mut parser = ShellParser::new();
        let extracted = parser.extract_commands(command);

        extracted.iter().any(|cmd| {
            let key = command_key(cmd);
            self.commands.iter().any(|blocked| *blocked == key)
        })
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
        ) || !input.is_shell_tool()
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

    fn make_test_filter(enabled: bool) -> BuiltinCommandFilter {
        BuiltinCommandFilter::new(
            enabled,
            None,
            "テストブロックメッセージ",
            &["testcmd", "testcmd2"],
            50,
        )
    }

    #[test]
    fn test_enabled_filter_blocks_matching_command() {
        let filter = make_test_filter(true);
        let input = make_bash_input("testcmd --flag");
        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_disabled_filter_does_not_apply() {
        let filter = make_test_filter(false);
        let input = make_bash_input("testcmd --flag");
        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_non_matching_command_is_allowed() {
        let filter = make_test_filter(true);
        let input = make_bash_input("safe_command --flag");
        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_non_bash_tool_is_not_applied() {
        let filter = make_test_filter(true);
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
        let filter = make_test_filter(true);
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
        let filter = make_test_filter(true);
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
        let filter = make_test_filter(true);
        assert_eq!(filter.priority(), 50);
    }

    #[test]
    fn test_second_command_pattern_matches() {
        let filter = make_test_filter(true);
        let input = make_bash_input("testcmd2 --arg");
        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_chained_command_matches() {
        let filter = make_test_filter(true);
        let input = make_bash_input("echo hello && testcmd --flag");
        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_permission_request_event_is_applied() {
        // BeforeCommand と同じく PermissionRequest でも該当する Bash コマンドはブロック対象
        let filter = make_test_filter(true);
        let input = HookInput {
            event: HookEvent::PermissionRequest,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "testcmd --flag".to_string(),
                timeout: None,
            }),
            session_id: None,
        };
        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_permission_request_safe_command_allowed() {
        // PermissionRequest でも対象外コマンドは applies_to=false
        let filter = make_test_filter(true);
        let input = HookInput {
            event: HookEvent::PermissionRequest,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "safe_command".to_string(),
                timeout: None,
            }),
            session_id: None,
        };
        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_absolute_path_command_is_blocked() {
        // /bin/rm 等の絶対パス指定でブロックリスト判定をバイパスできないこと
        let filter = make_test_filter(true);
        assert!(filter.applies_to(&make_bash_input("/bin/testcmd --flag")));
        assert!(filter.applies_to(&make_bash_input("/usr/bin/testcmd --flag")));
        assert!(filter.applies_to(&make_bash_input("./testcmd --flag")));
    }

    #[test]
    fn test_uppercase_command_is_blocked() {
        // Windows の `DEL` 等の大文字コマンドが小文字判定経由でブロックされること
        let filter = make_test_filter(true);
        assert!(filter.applies_to(&make_bash_input("TESTCMD --flag")));
        assert!(filter.applies_to(&make_bash_input("TestCmd --flag")));
    }

    #[test]
    fn test_executable_extension_is_blocked() {
        // `testcmd.exe` のように Windows 実行ファイル拡張子付きでもブロック対象
        let filter = make_test_filter(true);
        assert!(filter.applies_to(&make_bash_input("testcmd.exe --flag")));
        assert!(filter.applies_to(&make_bash_input("TESTCMD.EXE --flag")));
        assert!(filter.applies_to(&make_bash_input("testcmd.cmd --flag")));
        assert!(filter.applies_to(&make_bash_input("testcmd.bat --flag")));
        assert!(filter.applies_to(&make_bash_input("testcmd.com --flag")));
    }

    #[test]
    fn test_windows_path_command_is_blocked() {
        // Windows パス区切り `\` を含む絶対パスでもブロック対象
        let filter = make_test_filter(true);
        assert!(filter.applies_to(&make_bash_input("C:\\Windows\\testcmd.exe --flag")));
    }

    #[test]
    fn test_path_prefixed_wrapper_with_value_option_is_blocked() {
        // パス付き sudo の値付きオプションで、実行対象コマンドを見落とさないこと
        let filter = make_test_filter(true);
        assert!(filter.applies_to(&make_bash_input("/usr/bin/sudo -u root testcmd --flag")));
    }
}
