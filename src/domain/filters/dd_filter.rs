//! dd コマンドフィルターの実装。

use super::builtin_filter::BuiltinCommandFilter;

/// dd ブロック時のデフォルトメッセージ。
const DEFAULT_DD_MESSAGE: &str = "🚫 dd command is blocked for safety. Use cp or rsync for file operations. If you need dd specifically, use safe-dd or request explicit permission.";

/// dd コマンドパターン
const DD_COMMANDS: &[&str] = &[
    "dd", // Unix ディスクダンプコマンド
];

/// dd コマンドをブロックするフィルターを作成する。
pub fn new_dd_filter(enabled: bool, custom_message: Option<String>) -> BuiltinCommandFilter {
    BuiltinCommandFilter::new(
        enabled,
        custom_message,
        DEFAULT_DD_MESSAGE,
        DD_COMMANDS,
        super::priority::DD,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::filters::Filter;
    use crate::domain::test_helpers::make_bash_input;
    use crate::domain::{Decision, HookEvent, HookInput, ToolInput};

    fn make_filter(enabled: bool, msg: Option<String>) -> BuiltinCommandFilter {
        new_dd_filter(enabled, msg)
    }

    fn contains_dd_command(command: &str) -> bool {
        let filter = make_filter(true, None);
        let input = make_bash_input(command);
        filter.applies_to(&input)
    }

    #[test]
    fn test_contains_dd_command() {
        // 基本的な dd コマンド
        assert!(contains_dd_command("dd if=/dev/zero of=/dev/sda"));
        assert!(contains_dd_command("dd if=input.img of=output.img bs=4M"));
        assert!(!contains_dd_command("ls -la"));
        assert!(!contains_dd_command("echo dd"));

        // パイプコマンド
        assert!(contains_dd_command("cat file | dd of=output.img"));

        // チェーンコマンド
        assert!(contains_dd_command("sync && dd if=/dev/sda of=backup.img"));
    }

    #[test]
    fn test_applies_to_before_command_bash_dd() {
        let filter = make_filter(true, None);
        let input = make_bash_input("dd if=/dev/zero of=/dev/sda");
        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_when_disabled() {
        let filter = make_filter(false, None);
        let input = make_bash_input("dd if=/dev/zero of=/dev/sda");
        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_to_after_file_edit() {
        let filter = make_filter(true, None);

        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "dd if=/dev/zero of=/dev/sda".to_string(),
                timeout: None,
            }),
            session_id: None,
        };

        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_execute_returns_block() {
        let filter = make_filter(true, Some("Custom dd block message".to_string()));
        let input = make_bash_input("dd if=/dev/zero of=/dev/sda");
        let decision = filter.execute(&input);
        match decision {
            Decision::Block { message } => {
                assert_eq!(message, "Custom dd block message");
            }
            _ => panic!("Expected Block decision"),
        }
    }

    // === Edge Case Tests ===

    #[test]
    fn test_contains_dd_command_with_sudo_wrapper() {
        assert!(contains_dd_command("sudo dd if=/dev/zero of=/dev/sda"));
        assert!(contains_dd_command(
            "sudo -u root dd if=/dev/zero of=/dev/sda"
        ));
    }

    #[test]
    fn test_contains_dd_command_with_bash_c_subshell() {
        assert!(contains_dd_command("bash -c 'dd if=/dev/zero of=/dev/sda'"));
        assert!(contains_dd_command("sh -c \"dd if=/dev/zero of=/dev/sda\""));
    }

    #[test]
    fn test_contains_dd_command_in_command_substitution() {
        assert!(contains_dd_command("echo $(dd if=/dev/zero of=/dev/sda)"));
    }

    #[test]
    fn test_contains_dd_command_in_subshell() {
        assert!(contains_dd_command("(cd /dev && dd if=zero of=sda)"));
    }

    // === ファクトリ関数テスト ===

    #[test]
    fn test_new_dd_filter_default_message() {
        let filter = make_filter(true, None);
        let input = make_bash_input("dd if=/dev/zero of=/dev/sda");
        match filter.execute(&input) {
            Decision::Block { message } => {
                assert!(
                    message.contains("dd"),
                    "デフォルトメッセージに dd が含まれるべき: {}",
                    message
                );
            }
            _ => panic!("Block判定が期待される"),
        }
    }

    #[test]
    fn test_new_dd_filter_priority() {
        let filter = make_filter(true, None);
        assert_eq!(filter.priority(), 15);
    }

    // === 偽陽性テスト ===

    #[test]
    fn test_no_false_positive_for_similar_commands() {
        // dd を部分文字列として含むが dd コマンドではないもの
        assert!(!contains_dd_command("add file.txt"));
        assert!(!contains_dd_command("address 192.168.1.1"));
        assert!(!contains_dd_command("oddly enough"));
    }

    // === 追加のバリエーションテスト ===

    #[test]
    fn test_dd_with_various_options() {
        assert!(contains_dd_command(
            "dd status=progress if=/dev/zero of=/dev/sda"
        ));
        assert!(contains_dd_command(
            "dd conv=fsync if=input.img of=output.img"
        ));
        assert!(contains_dd_command(
            "dd bs=1M count=100 if=/dev/urandom of=/tmp/data"
        ));
    }

    #[test]
    fn test_dd_in_multiple_chained_commands() {
        assert!(contains_dd_command(
            "dd if=/dev/zero of=a && dd if=/dev/zero of=b"
        ));
    }

    #[test]
    fn test_dd_with_timeout_wrapper() {
        assert!(contains_dd_command(
            "timeout 60 dd if=/dev/zero of=/dev/sda bs=4M"
        ));
    }
}
