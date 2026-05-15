//! rm コマンドフィルターの実装。

use super::builtin_filter::BuiltinCommandFilter;

/// rm ブロック時のデフォルトメッセージ（設定でカスタマイズ可能）。
const DEFAULT_RM_MESSAGE: &str = "🚫 rm/rmdir command blocked for safety. Configure rm_block_message in config.toml to customize this message.";

/// Unix/Windows の rm コマンドパターン
const RM_COMMANDS: &[&str] = &[
    "rm",    // Unix
    "rmdir", // Unix/Windows
    "del",   // Windows
    "erase", // Windows (del のエイリアス)
];

/// rm 関連コマンドをブロックするフィルターを作成する。
pub fn new_rm_filter(enabled: bool, custom_message: Option<String>) -> BuiltinCommandFilter {
    BuiltinCommandFilter::new(
        enabled,
        custom_message,
        DEFAULT_RM_MESSAGE,
        RM_COMMANDS,
        20,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::filters::Filter;
    use crate::domain::{Decision, HookEvent, HookInput, ToolInput};

    fn make_filter(enabled: bool, msg: Option<String>) -> BuiltinCommandFilter {
        new_rm_filter(enabled, msg)
    }

    fn contains_rm_command(command: &str) -> bool {
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
    fn test_contains_rm_command() {
        // Unix コマンド
        assert!(contains_rm_command("rm file.txt"));
        assert!(contains_rm_command("rm -rf /tmp/test"));
        assert!(contains_rm_command("rmdir empty_dir"));
        assert!(!contains_rm_command("ls -la"));
        assert!(!contains_rm_command("echo rm"));

        // Windows コマンド
        assert!(contains_rm_command("del file.txt"));
        assert!(contains_rm_command("del /F /Q temp.log"));
        assert!(contains_rm_command("erase old_file.bak"));

        // チェーンコマンド
        assert!(contains_rm_command("cd /tmp && rm -rf test"));
        assert!(contains_rm_command("echo done; rmdir old"));
        assert!(contains_rm_command("dir && del *.tmp"));
    }

    #[test]
    fn test_applies_to_before_command_bash_rm() {
        let filter = make_filter(true, None);

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
        let filter = make_filter(false, None);

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
        let filter = make_filter(true, None);

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
        let filter = make_filter(true, None);

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
        let filter = make_filter(true, Some("Custom rm block message".to_string()));

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

    // === エッジケーステスト ===

    #[test]
    fn test_contains_rm_command_with_sudo_wrapper() {
        assert!(contains_rm_command("sudo rm -rf /tmp/test"));
        assert!(contains_rm_command("sudo -u root rm -rf /tmp/test"));
        assert!(contains_rm_command("sudo FOO=bar rm -rf /tmp/test"));
        assert!(contains_rm_command("sudo -u root FOO=bar rm -rf /tmp/test"));
    }

    #[test]
    fn test_contains_rm_command_with_bash_c_subshell() {
        assert!(contains_rm_command("bash -c 'rm -rf /tmp/test'"));
        assert!(contains_rm_command("sh -c \"rm -rf /tmp/test\""));
        assert!(contains_rm_command("bash -lc 'rm -rf /tmp/test'"));
        assert!(contains_rm_command("cmd /c del C:\\tmp\\file.txt"));
    }

    #[test]
    fn test_contains_rm_command_in_command_substitution() {
        assert!(contains_rm_command("echo $(rm -rf /tmp/test)"));
    }

    #[test]
    fn test_contains_rm_command_in_subshell() {
        assert!(contains_rm_command("(cd /tmp && rm -rf test)"));
    }

    #[test]
    fn test_contains_rm_command_with_env_wrapper() {
        assert!(contains_rm_command("env PATH=/usr/bin rm file.txt"));
    }

    #[test]
    fn test_contains_rm_command_with_nohup_wrapper() {
        assert!(contains_rm_command("nohup rm -rf /tmp/test &"));
    }

    // === ファクトリ関数テスト ===

    #[test]
    fn test_new_rm_filter_default_message() {
        let filter = make_filter(true, None);
        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "rm file.txt".to_string(),
                timeout: None,
            }),
            session_id: None,
        };
        match filter.execute(&input) {
            Decision::Block { message } => {
                assert!(
                    message.contains("rm/rmdir"),
                    "デフォルトメッセージに rm/rmdir が含まれるべき: {}",
                    message
                );
            }
            _ => panic!("Block判定が期待される"),
        }
    }

    #[test]
    fn test_new_rm_filter_priority() {
        let filter = make_filter(true, None);
        assert_eq!(filter.priority(), 20);
    }

    // === 偽陽性テスト ===

    #[test]
    fn test_no_false_positive_for_similar_commands() {
        // rm を部分文字列として含むが rm コマンドではないもの
        assert!(!contains_rm_command("chrome --headless"));
        assert!(!contains_rm_command("grep rm file.txt"));
        assert!(!contains_rm_command("farm build"));
        assert!(!contains_rm_command("format output.txt"));
    }

    #[test]
    fn test_rm_with_complex_flags() {
        assert!(contains_rm_command("rm --verbose --force -r dir/"));
        assert!(contains_rm_command("rm -i file.txt"));
        assert!(contains_rm_command("rm --preserve-root=all -rf /"));
    }

    #[test]
    fn test_rm_in_multiple_chained_commands() {
        assert!(contains_rm_command(
            "echo start && ls && rm -rf tmp && echo done"
        ));
    }

    #[test]
    fn test_rm_with_timeout_wrapper() {
        assert!(contains_rm_command("timeout 30 rm -rf /tmp/test"));
        assert!(contains_rm_command("timeout --signal TERM 10 rm file"));
    }

    #[test]
    fn test_rm_with_exec_wrapper() {
        assert!(contains_rm_command("exec rm -rf /tmp/test"));
    }

    #[test]
    fn test_rm_with_xargs_replace_flag() {
        assert!(contains_rm_command(
            "find . -name '*.tmp' | xargs -I {} rm -rf {}"
        ));
    }

    #[test]
    fn test_rm_with_xargs_shell_c() {
        assert!(contains_rm_command(
            "echo file | xargs sh -c 'rm -f \"$@\"' sh"
        ));
    }
}
