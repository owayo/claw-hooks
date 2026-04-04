//! kill コマンドフィルターの実装。

use super::builtin_filter::BuiltinCommandFilter;

/// kill ブロック時のデフォルトメッセージ（設定でカスタマイズ可能）。
const DEFAULT_KILL_MESSAGE: &str = "🚫 kill/pkill/killall command blocked for safety. Use safe-kill: safe-kill <PID>, safe-kill -N <name>, or safe-kill -p <port>.";

/// Unix/Windows の kill コマンドパターン
const KILL_COMMANDS: &[&str] = &[
    "kill",     // Unix
    "pkill",    // Unix
    "killall",  // Unix
    "taskkill", // Windows
];

/// xargs 経由の kill コマンドが含まれるか判定する。
/// xargs の最初の非フラグ引数が実際に実行されるコマンドであるため、
/// それのみを検査する（後続引数はコマンドの引数であり実行対象ではない）。
fn contains_xargs_kill(command: &str) -> bool {
    for segment in command.split('|') {
        let trimmed = segment.trim();
        if trimmed.starts_with("xargs") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            // xargs の最初の非フラグ引数 = 実行されるコマンド
            if let Some(cmd) = parts.iter().skip(1).find(|a| !a.starts_with('-')) {
                if KILL_COMMANDS.contains(cmd) {
                    return true;
                }
            }
        }
    }
    false
}

/// kill 関連コマンドをブロックするフィルターを作成する。
pub fn new_kill_filter(enabled: bool, custom_message: Option<String>) -> BuiltinCommandFilter {
    BuiltinCommandFilter::new(
        enabled,
        custom_message,
        DEFAULT_KILL_MESSAGE,
        KILL_COMMANDS,
        10,
        Some(contains_xargs_kill),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::filters::Filter;
    use crate::domain::{Decision, HookEvent, HookInput, ToolInput};

    fn make_filter(enabled: bool, msg: Option<String>) -> BuiltinCommandFilter {
        new_kill_filter(enabled, msg)
    }

    fn contains_kill_command(command: &str) -> bool {
        let filter = make_filter(true, None);
        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: command.to_string(),
                timeout: None,
            }),
            session_id: None,
        };
        filter.applies_to(&input)
    }

    #[test]
    fn test_contains_kill_command() {
        // Unix コマンド
        assert!(contains_kill_command("kill 1234"));
        assert!(contains_kill_command("pkill node"));
        assert!(contains_kill_command("killall python"));
        assert!(!contains_kill_command("ls -la"));
        assert!(!contains_kill_command("echo kill"));

        // Windows コマンド
        assert!(contains_kill_command("taskkill /PID 1234"));
        assert!(contains_kill_command("taskkill /IM node.exe /F"));

        // パイプコマンド
        assert!(contains_kill_command("ps aux | grep node | xargs kill"));
        assert!(contains_kill_command("pgrep node | xargs kill -9"));

        // チェーンコマンド
        assert!(contains_kill_command("cd /tmp && kill 1234"));
        assert!(contains_kill_command("echo test; pkill node"));
    }

    #[test]
    fn test_applies_to_before_command_bash_kill() {
        let filter = make_filter(true, None);

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
        let filter = make_filter(false, None);

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
        let filter = make_filter(true, None);

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
        let filter = make_filter(true, Some("Custom kill block message".to_string()));

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
        assert!(contains_kill_command("sudo kill -9 1234"));
        assert!(contains_kill_command("sudo -u root pkill node"));
    }

    #[test]
    fn test_contains_kill_command_with_bash_c_subshell() {
        assert!(contains_kill_command("bash -c 'kill 1234'"));
        assert!(contains_kill_command("sh -c \"pkill node\""));
    }

    #[test]
    fn test_contains_kill_command_with_xargs_double_dash() {
        assert!(contains_kill_command("pgrep node | xargs -- kill -9"));
    }

    #[test]
    fn test_contains_kill_command_in_command_substitution() {
        assert!(contains_kill_command("echo $(kill -9 1234)"));
    }

    #[test]
    fn test_contains_kill_command_in_subshell() {
        assert!(contains_kill_command("(cd /tmp && kill 1234)"));
    }

    #[test]
    fn test_contains_kill_command_with_nohup_wrapper() {
        assert!(contains_kill_command("nohup kill -9 1234 &"));
    }

    #[test]
    fn test_xargs_non_kill_command_not_blocked() {
        // xargs が実行するコマンドが kill でない場合はブロックしない
        assert!(!contains_kill_command("ps | xargs echo kill"));
        assert!(!contains_kill_command("find . | xargs grep kill"));
    }

    // === ファクトリ関数テスト ===

    #[test]
    fn test_new_kill_filter_default_message() {
        let filter = make_filter(true, None);
        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "kill 1234".to_string(),
                timeout: None,
            }),
            session_id: None,
        };
        match filter.execute(&input) {
            Decision::Block { message } => {
                assert!(
                    message.contains("kill"),
                    "デフォルトメッセージに kill が含まれるべき: {}",
                    message
                );
                assert!(
                    message.contains("safe-kill"),
                    "デフォルトメッセージに safe-kill が含まれるべき: {}",
                    message
                );
            }
            _ => panic!("Block判定が期待される"),
        }
    }

    #[test]
    fn test_new_kill_filter_priority() {
        let filter = make_filter(true, None);
        assert_eq!(filter.priority(), 10);
    }

    // === 偽陽性テスト ===

    #[test]
    fn test_no_false_positive_for_similar_commands() {
        assert!(!contains_kill_command("skill check"));
        assert!(!contains_kill_command("overkill mode"));
        assert!(!contains_kill_command("grep killall docs.txt"));
    }

    // === xargs 追加テスト ===

    #[test]
    fn test_contains_xargs_kill_direct() {
        assert!(contains_xargs_kill("ps | xargs kill"));
        assert!(contains_xargs_kill("pgrep node | xargs kill -9"));
        assert!(contains_xargs_kill("ps | xargs pkill"));
        assert!(contains_xargs_kill("ps | xargs killall"));
        assert!(contains_xargs_kill("ps | xargs taskkill"));
    }

    #[test]
    fn test_contains_xargs_kill_with_flags() {
        assert!(contains_xargs_kill("ps | xargs -n1 kill"));
        assert!(contains_xargs_kill("ps | xargs -P4 kill"));
    }

    #[test]
    fn test_contains_xargs_kill_non_kill() {
        assert!(!contains_xargs_kill("find . | xargs echo"));
        assert!(!contains_xargs_kill("find . | xargs rm"));
    }

    #[test]
    fn test_kill_with_timeout_wrapper() {
        assert!(contains_kill_command("timeout 10 kill -9 1234"));
    }
}
