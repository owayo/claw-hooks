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
/// 値を取る xargs オプションを読み飛ばし、実際に実行されるコマンドだけを検査する。
fn contains_xargs_kill(command: &str) -> bool {
    for segment in command.split('|') {
        let parts: Vec<&str> = segment.split_whitespace().collect();
        // セグメント先頭トークンが厳密に `xargs` のときだけ検査する。
        // `starts_with("xargs")` だと `xargs-wrapper` / `xargsfoo` のような
        // 別コマンドを前方一致で xargs と誤判定し、過剰ブロックになる。
        if parts.first() == Some(&"xargs") {
            if let Some(cmd) = find_xargs_command(&parts[1..]) {
                if KILL_COMMANDS.contains(&cmd) {
                    return true;
                }
            }
        }
    }
    false
}

fn find_xargs_command<'a>(args: &[&'a str]) -> Option<&'a str> {
    let mut index = 0;

    while let Some(arg) = args.get(index) {
        if *arg == "--" {
            index += 1;
            break;
        }

        if !arg.starts_with('-') {
            break;
        }

        index += if xargs_flag_takes_separate_value(arg) {
            2
        } else {
            1
        };
    }

    args.get(index).copied()
}

fn xargs_flag_takes_separate_value(flag: &str) -> bool {
    matches!(
        flag,
        "-a" | "--arg-file"
            | "-d"
            | "--delimiter"
            | "-E"
            | "-e"
            | "--eof"
            | "-I"
            | "-i"
            | "--replace"
            | "-L"
            | "--max-lines"
            | "-n"
            | "--max-args"
            | "-P"
            | "--max-procs"
            | "--process-slot-var"
    )
}

/// kill 関連コマンドをブロックするフィルターを作成する。
pub fn new_kill_filter(enabled: bool, custom_message: Option<String>) -> BuiltinCommandFilter {
    BuiltinCommandFilter::new(
        enabled,
        custom_message,
        DEFAULT_KILL_MESSAGE,
        KILL_COMMANDS,
        super::priority::KILL,
        Some(contains_xargs_kill),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::filters::Filter;
    use crate::domain::test_helpers::make_bash_input;
    use crate::domain::{Decision, HookEvent, HookInput, ToolInput};

    fn make_filter(enabled: bool, msg: Option<String>) -> BuiltinCommandFilter {
        new_kill_filter(enabled, msg)
    }

    fn contains_kill_command(command: &str) -> bool {
        let filter = make_filter(true, None);
        let input = make_bash_input(command);
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
    fn test_xargs_kill_does_not_false_positive_on_similar_command() {
        // `xargs-wrapper` / `xargsfoo` は `xargs` で始まる別コマンドであり、
        // xargs 経由の kill ではないため過剰ブロックしないこと（前方一致の誤検知防止）。
        assert!(!contains_xargs_kill("ps | xargs-wrapper kill"));
        assert!(!contains_xargs_kill("ps | xargsfoo kill"));
        // 本物の xargs kill は引き続き検出する（回帰防止）。
        assert!(contains_xargs_kill("ps aux | xargs kill"));
        assert!(contains_xargs_kill("pgrep node | xargs kill -9"));
        // 値取りフラグを挟んでも検出する。
        assert!(contains_xargs_kill("ps | xargs -r kill"));
    }

    #[test]
    fn test_applies_to_before_command_bash_kill() {
        let filter = make_filter(true, None);
        let input = make_bash_input("kill -9 1234");
        assert!(filter.applies_to(&input));
    }

    #[test]
    fn test_does_not_apply_when_disabled() {
        let filter = make_filter(false, None);
        let input = make_bash_input("kill -9 1234");
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
        let input = make_bash_input("kill -9 1234");
        let decision = filter.execute(&input);
        match decision {
            Decision::Block { message } => {
                assert_eq!(message, "Custom kill block message");
            }
            _ => panic!("Expected Block decision"),
        }
    }

    // === エッジケーステスト ===

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
    fn test_contains_kill_command_with_xargs_replace_flag() {
        assert!(contains_kill_command("pgrep node | xargs -I {} kill -9 {}"));
    }

    #[test]
    fn test_contains_kill_command_with_xargs_shell_c() {
        assert!(contains_kill_command(
            "pgrep node | xargs sh -c 'kill -9 \"$@\"' sh"
        ));
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
        let input = make_bash_input("kill 1234");
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
        assert!(contains_xargs_kill("ps | xargs -I {} kill -9 {}"));
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
