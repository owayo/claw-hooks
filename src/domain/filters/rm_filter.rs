//! rm コマンドフィルターの実装。

use super::Filter;
use crate::domain::parser::ShellParser;
use crate::domain::{Decision, HookEvent, HookInput, ToolInput};

/// rm ブロック時のデフォルトメッセージ（設定でカスタマイズ可能）。
const DEFAULT_RM_MESSAGE: &str = "🚫 rm/rmdir command blocked for safety. Configure rm_block_message in config.toml to customize this message.";

/// rm 関連コマンドをブロックするフィルター。
pub struct RmFilter {
    enabled: bool,
    message: String,
}

impl RmFilter {
    /// カスタムメッセージ付きの新しい RmFilter を作成する。
    pub fn new(enabled: bool, custom_message: Option<String>) -> Self {
        Self {
            enabled,
            message: custom_message.unwrap_or_else(|| DEFAULT_RM_MESSAGE.to_string()),
        }
    }

    /// Unix/Windows の rm コマンドパターン
    const RM_COMMANDS: &'static [&'static str] = &[
        "rm",    // Unix
        "rmdir", // Unix/Windows
        "del",   // Windows
        "erase", // Windows (del のエイリアス)
    ];

    /// コマンド文字列に rm 関連コマンドが含まれるか判定する。
    fn contains_rm_command(command: &str) -> bool {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(command);

        commands
            .iter()
            .any(|cmd| Self::RM_COMMANDS.contains(&cmd.as_str()))
    }
}

impl Filter for RmFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        if !self.enabled {
            return false;
        }

        // BeforeCommand イベントの Bash ツールにのみ適用
        if input.event != HookEvent::BeforeCommand || input.tool_name != "Bash" {
            return false;
        }

        if let ToolInput::Bash(bash) = &input.tool_input {
            return Self::contains_rm_command(&bash.command);
        }

        false
    }

    fn execute(&self, _input: &HookInput) -> Decision {
        Decision::Block {
            message: self.message.clone(),
        }
    }

    fn priority(&self) -> u32 {
        20 // 高優先度（kill より低い）
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_rm_command() {
        // Simple Unix commands
        assert!(RmFilter::contains_rm_command("rm file.txt"));
        assert!(RmFilter::contains_rm_command("rm -rf /tmp/test"));
        assert!(RmFilter::contains_rm_command("rmdir empty_dir"));
        assert!(!RmFilter::contains_rm_command("ls -la"));
        assert!(!RmFilter::contains_rm_command("echo rm"));

        // Windows commands
        assert!(RmFilter::contains_rm_command("del file.txt"));
        assert!(RmFilter::contains_rm_command("del /F /Q temp.log"));
        assert!(RmFilter::contains_rm_command("erase old_file.bak"));

        // Chained commands
        assert!(RmFilter::contains_rm_command("cd /tmp && rm -rf test"));
        assert!(RmFilter::contains_rm_command("echo done; rmdir old"));
        assert!(RmFilter::contains_rm_command("dir && del *.tmp"));
    }

    #[test]
    fn test_applies_to_before_command_bash_rm() {
        let filter = RmFilter::new(true, None);

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "rm -rf /tmp/test".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_when_disabled() {
        let filter = RmFilter::new(false, None);

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "rm -rf /tmp/test".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_after_file_edit() {
        let filter = RmFilter::new(true, None);

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "rm -rf /tmp/test".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_non_bash_tool() {
        let filter = RmFilter::new(true, None);

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/test".to_string(),
                content: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_execute_returns_block() {
        let filter = RmFilter::new(true, Some("Custom rm block message".to_string()));

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "rm -rf /tmp/test".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        let decision = filter.execute(&input);
        match decision {
            Decision::Block { message } => {
                assert_eq!(message, "Custom rm block message");
            }
            _ => panic!("Expected Block decision"),
        }
    }

    // === Edge Case Tests ===

    #[test]
    fn test_contains_rm_command_with_sudo_wrapper() {
        assert!(RmFilter::contains_rm_command("sudo rm -rf /tmp/test"));
        assert!(RmFilter::contains_rm_command(
            "sudo -u root rm -rf /tmp/test"
        ));
    }

    #[test]
    fn test_contains_rm_command_with_bash_c_subshell() {
        assert!(RmFilter::contains_rm_command("bash -c 'rm -rf /tmp/test'"));
        assert!(RmFilter::contains_rm_command("sh -c \"rm -rf /tmp/test\""));
    }

    #[test]
    fn test_contains_rm_command_in_command_substitution() {
        assert!(RmFilter::contains_rm_command("echo $(rm -rf /tmp/test)"));
    }

    #[test]
    fn test_contains_rm_command_in_subshell() {
        assert!(RmFilter::contains_rm_command("(cd /tmp && rm -rf test)"));
    }

    #[test]
    fn test_contains_rm_command_with_env_wrapper() {
        assert!(RmFilter::contains_rm_command(
            "env PATH=/usr/bin rm file.txt"
        ));
    }

    #[test]
    fn test_contains_rm_command_with_nohup_wrapper() {
        assert!(RmFilter::contains_rm_command("nohup rm -rf /tmp/test &"));
    }
}
