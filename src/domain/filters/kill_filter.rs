//! kill コマンドフィルターの実装。

use super::Filter;
use crate::domain::parser::ShellParser;
use crate::domain::{Decision, HookEvent, HookInput, ToolInput};

/// kill ブロック時のデフォルトメッセージ（設定でカスタマイズ可能）。
const DEFAULT_KILL_MESSAGE: &str = "🚫 kill/pkill/killall command blocked for safety. Use safe-kill: safe-kill <PID>, safe-kill -N <name>, or safe-kill -p <port>.";

/// kill 関連コマンドをブロックするフィルター。
pub struct KillFilter {
    enabled: bool,
    message: String,
}

impl KillFilter {
    /// カスタムメッセージ付きの新しい KillFilter を作成する。
    pub fn new(enabled: bool, custom_message: Option<String>) -> Self {
        Self {
            enabled,
            message: custom_message.unwrap_or_else(|| DEFAULT_KILL_MESSAGE.to_string()),
        }
    }

    /// Unix/Windows の kill コマンドパターン
    const KILL_COMMANDS: &'static [&'static str] = &[
        "kill",     // Unix
        "pkill",    // Unix
        "killall",  // Unix
        "taskkill", // Windows
    ];

    /// コマンド文字列に kill 関連コマンドが含まれるか判定する。
    fn contains_kill_command(command: &str) -> bool {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(command);

        // 直接の kill コマンドをチェック（Unix/Windows）
        if commands
            .iter()
            .any(|cmd| Self::KILL_COMMANDS.contains(&cmd.as_str()))
        {
            return true;
        }

        // xargs 経由の kill コマンドもチェック
        // パターン: "xargs kill", "xargs -0 kill" 等
        Self::contains_xargs_kill(command)
    }

    /// xargs 経由の kill コマンドが含まれるか判定する。
    fn contains_xargs_kill(command: &str) -> bool {
        // パイプで分割して各セグメントをチェック
        for segment in command.split('|') {
            let trimmed = segment.trim();
            if trimmed.starts_with("xargs") {
                // xargs の後に kill コマンドが含まれるかチェック
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                for part in parts.iter().skip(1) {
                    // xargs のフラグをスキップ
                    if !part.starts_with('-') && Self::KILL_COMMANDS.contains(part) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

impl Filter for KillFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        if !self.enabled {
            return false;
        }

        // BeforeCommand イベントの Bash ツールにのみ適用
        if input.event != HookEvent::BeforeCommand || input.tool_name != "Bash" {
            return false;
        }

        if let ToolInput::Bash(bash) = &input.tool_input {
            return Self::contains_kill_command(&bash.command);
        }

        false
    }

    fn execute(&self, _input: &HookInput) -> Decision {
        Decision::Block {
            message: self.message.clone(),
        }
    }

    fn priority(&self) -> u32 {
        10 // 最高優先度
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_kill_command() {
        // Simple Unix commands
        assert!(KillFilter::contains_kill_command("kill 1234"));
        assert!(KillFilter::contains_kill_command("pkill node"));
        assert!(KillFilter::contains_kill_command("killall python"));
        assert!(!KillFilter::contains_kill_command("ls -la"));
        assert!(!KillFilter::contains_kill_command("echo kill"));

        // Windows commands
        assert!(KillFilter::contains_kill_command("taskkill /PID 1234"));
        assert!(KillFilter::contains_kill_command(
            "taskkill /IM node.exe /F"
        ));

        // Piped commands
        assert!(KillFilter::contains_kill_command(
            "ps aux | grep node | xargs kill"
        ));
        assert!(KillFilter::contains_kill_command(
            "pgrep node | xargs kill -9"
        ));

        // Chained commands
        assert!(KillFilter::contains_kill_command("cd /tmp && kill 1234"));
        assert!(KillFilter::contains_kill_command("echo test; pkill node"));
    }

    #[test]
    fn test_applies_to_before_command_bash_kill() {
        let filter = KillFilter::new(true, None);

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "kill -9 1234".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_when_disabled() {
        let filter = KillFilter::new(false, None);

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "kill -9 1234".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_after_file_edit() {
        let filter = KillFilter::new(true, None);

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "kill -9 1234".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_execute_returns_block() {
        let filter = KillFilter::new(true, Some("Custom kill block message".to_string()));

        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "kill -9 1234".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        let decision = filter.execute(&input);
        match decision {
            Decision::Block { message } => {
                assert_eq!(message, "Custom kill block message");
            }
            _ => panic!("Expected Block decision"),
        }
    }

    // === Edge Case Tests ===

    #[test]
    fn test_contains_kill_command_with_sudo_wrapper() {
        assert!(KillFilter::contains_kill_command("sudo kill -9 1234"));
        assert!(KillFilter::contains_kill_command("sudo -u root pkill node"));
    }

    #[test]
    fn test_contains_kill_command_with_bash_c_subshell() {
        assert!(KillFilter::contains_kill_command("bash -c 'kill 1234'"));
        assert!(KillFilter::contains_kill_command("sh -c \"pkill node\""));
    }

    #[test]
    fn test_contains_kill_command_with_xargs_double_dash() {
        // xargs -- kill should still detect kill
        assert!(KillFilter::contains_kill_command(
            "pgrep node | xargs -- kill -9"
        ));
    }

    #[test]
    fn test_contains_kill_command_in_command_substitution() {
        assert!(KillFilter::contains_kill_command("echo $(kill -9 1234)"));
    }

    #[test]
    fn test_contains_kill_command_in_subshell() {
        assert!(KillFilter::contains_kill_command("(cd /tmp && kill 1234)"));
    }

    #[test]
    fn test_contains_kill_command_with_nohup_wrapper() {
        assert!(KillFilter::contains_kill_command("nohup kill -9 1234 &"));
    }
}
