//! カスタムコマンドフィルターの実装。

use regex::Regex;

use super::Filter;
use crate::domain::parser::ShellParser;
use crate::domain::{Decision, HookEvent, HookInput, ToolInput};

/// カスタムコマンドマッチングのフィルターモード。
enum FilterMode {
    /// 正規表現ベースのパターンマッチング（command フィールドが正規表現）
    Regex(Regex),
    /// 正規表現コマンド名 + 引数マッチング
    Args { command: Regex, args: Vec<String> },
}

/// カスタムコマンドパターンのフィルター。
///
/// 2つのモードをサポート:
/// 1. 正規表現モード: `command` のみ指定時、正規表現パターンとして扱う
/// 2. 引数モード: `command` と `args` 両方指定時、正規表現コマンド + いずれかの引数でマッチ
pub struct CustomCommandFilter {
    mode: FilterMode,
    message: String,
}

impl CustomCommandFilter {
    /// 正規表現パターンで新しい CustomCommandFilter を作成する。
    ///
    /// パターンはコマンド文字列の先頭に自動アンカーされ、
    /// 引数ではなくコマンド名にマッチすることを保証する。
    /// 例: パターン "yarn" は "yarn install" にマッチするが "grep yarn" にはマッチしない。
    ///
    /// # Errors
    ///
    /// パターンが有効な正規表現でない場合エラーを返す。
    pub fn new(pattern: &str, message: String) -> Result<Self, regex::Error> {
        // コマンド名にマッチするよう先頭にアンカーする
        let anchored_pattern = if pattern.starts_with('^') {
            pattern.to_string()
        } else {
            format!("^{}", pattern)
        };
        let regex = Regex::new(&anchored_pattern)?;
        Ok(Self {
            mode: FilterMode::Regex(regex),
            message,
        })
    }

    /// 正規表現コマンド + 引数マッチングで新しい CustomCommandFilter を作成する。
    ///
    /// コマンド名が `command` 正規表現にマッチし、かつ `args` のいずれかが
    /// 最初の引数として存在する場合にマッチする。
    ///
    /// # 例
    ///
    /// ```ignore
    /// let filter = CustomCommandFilter::with_args("npm", vec!["install", "i", "add"], "msg")?;
    /// // マッチする: npm install, npm i, npm add package
    /// // マッチしない: npm run, npm test
    ///
    /// let filter = CustomCommandFilter::with_args("pip3?", vec!["install"], "msg")?;
    /// // マッチする: pip install, pip3 install
    /// ```
    ///
    /// # Errors
    ///
    /// コマンドパターンが有効な正規表現でない場合エラーを返す。
    pub fn with_args(
        command: &str,
        args: Vec<String>,
        message: String,
    ) -> Result<Self, regex::Error> {
        // コマンド名全体にマッチするようアンカー付きで正規表現をコンパイル
        let anchored = format!("^{}$", command);
        let regex = Regex::new(&anchored)?;
        Ok(Self {
            mode: FilterMode::Args {
                command: regex,
                args,
            },
            message,
        })
    }

    /// パターンマッチング用にコマンド文字列からクォートされた内容を除去する。
    /// `echo "yarn"` のような誤検知を防止する。
    fn strip_quoted_content(s: &str) -> String {
        let mut result = String::new();
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let mut chars = s.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '\\' && !in_single_quote {
                // エスケープされた文字をスキップ
                chars.next();
                continue;
            }

            if c == '\'' && !in_double_quote {
                in_single_quote = !in_single_quote;
                continue;
            }

            if c == '"' && !in_single_quote {
                in_double_quote = !in_double_quote;
                continue;
            }

            if !in_single_quote && !in_double_quote {
                result.push(c);
            }
        }

        result
    }

    /// 正規表現モードでコマンド文字列がマッチするか判定する。
    fn matches_regex(&self, command: &str, pattern: &Regex) -> bool {
        let mut parser = ShellParser::new();
        let command_strings = parser.extract_command_strings(command);

        command_strings
            .iter()
            .any(|cmd| pattern.is_match(&Self::strip_quoted_content(cmd)))
    }

    /// 引数モードでコマンド文字列がマッチするか判定する。
    fn matches_args(
        &self,
        input_command: &str,
        target_cmd: &Regex,
        target_args: &[String],
    ) -> bool {
        let mut parser = ShellParser::new();
        let command_strings = parser.extract_command_strings(input_command);

        for cmd_str in command_strings {
            let stripped = Self::strip_quoted_content(&cmd_str);
            let parts: Vec<&str> = stripped.split_whitespace().collect();

            if parts.is_empty() {
                continue;
            }

            // コマンド名が正規表現にマッチするか判定
            if !target_cmd.is_match(parts[0]) {
                continue;
            }

            // 引数未指定の場合、コマンドの使用すべてにマッチ
            if target_args.is_empty() {
                return true;
            }

            // 対象の引数が存在するか判定
            if parts.len() > 1 && target_args.iter().any(|arg| parts[1] == arg) {
                return true;
            }
        }

        false
    }

    /// コマンド文字列がフィルターにマッチするか判定する。
    fn matches(&self, command: &str) -> bool {
        match &self.mode {
            FilterMode::Regex(pattern) => self.matches_regex(command, pattern),
            FilterMode::Args { command: cmd, args } => self.matches_args(command, cmd, args),
        }
    }
}

impl Filter for CustomCommandFilter {
    fn applies_to(&self, input: &HookInput) -> bool {
        // BeforeCommand イベントの Bash ツールにのみ適用
        if input.event != HookEvent::BeforeCommand || input.tool_name != "Bash" {
            return false;
        }

        if let ToolInput::Bash(bash) = &input.tool_input {
            return self.matches(&bash.command);
        }

        false
    }

    fn execute(&self, _input: &HookInput) -> Decision {
        Decision::Block {
            message: self.message.clone(),
        }
    }

    fn priority(&self) -> u32 {
        50 // 中優先度
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regex mode tests
    #[test]
    fn test_custom_filter_regex() {
        let filter = CustomCommandFilter::new("python", "Use uv instead".to_string()).unwrap();
        assert!(filter.matches("python script.py"));
        assert!(filter.matches("python"));
        assert!(!filter.matches("ls"));
    }

    #[test]
    fn test_custom_filter_regex_with_semicolon() {
        let filter = CustomCommandFilter::new("yarn", "Use pnpm instead".to_string()).unwrap();

        // yarn after semicolon should be detected
        assert!(filter.matches("echo \"install\"; yarn install"));

        // yarn in quotes should NOT trigger (it's not a command)
        assert!(!filter.matches("echo \"not yarn install\"; pnpm install"));

        // Direct yarn command
        assert!(filter.matches("yarn install"));
        assert!(filter.matches("yarn add react"));

        // pnpm should pass
        assert!(!filter.matches("pnpm install"));
    }

    #[test]
    fn test_custom_filter_regex_with_chained_commands() {
        let filter = CustomCommandFilter::new("python", "Use uv instead".to_string()).unwrap();

        // python in chained commands
        assert!(filter.matches("cd /app && python script.py"));
        assert!(filter.matches("echo done; python main.py"));
        assert!(filter.matches("ls | python filter.py"));

        // python in quotes should NOT trigger
        assert!(!filter.matches("echo \"python is great\""));
    }

    // Args mode tests
    #[test]
    fn test_custom_filter_args_basic() {
        let filter = CustomCommandFilter::with_args(
            "npm",
            vec!["install".to_string(), "i".to_string(), "add".to_string()],
            "Use pnpm instead".to_string(),
        )
        .unwrap();

        // Should match
        assert!(filter.matches("npm install"));
        assert!(filter.matches("npm i"));
        assert!(filter.matches("npm add react"));
        assert!(filter.matches("npm install lodash"));

        // Should not match (different subcommand)
        assert!(!filter.matches("npm run build"));
        assert!(!filter.matches("npm test"));
        assert!(!filter.matches("npm --version"));

        // Should not match (different command)
        assert!(!filter.matches("pnpm install"));
        assert!(!filter.matches("yarn add"));
    }

    #[test]
    fn test_custom_filter_args_in_chained_commands() {
        let filter = CustomCommandFilter::with_args(
            "npm",
            vec!["install".to_string(), "i".to_string()],
            "Use pnpm instead".to_string(),
        )
        .unwrap();

        // Should match in chained commands
        assert!(filter.matches("echo done; npm install"));
        assert!(filter.matches("cd /app && npm i lodash"));

        // Should not match when in quotes
        assert!(!filter.matches("echo \"npm install\""));

        // Should not match different subcommand in chain
        assert!(!filter.matches("npm run build && echo done"));
    }

    #[test]
    fn test_custom_filter_args_empty_args() {
        // Empty args means match any usage of the command
        let filter =
            CustomCommandFilter::with_args("yarn", vec![], "Use pnpm instead".to_string()).unwrap();

        // Should match all yarn commands
        assert!(filter.matches("yarn"));
        assert!(filter.matches("yarn install"));
        assert!(filter.matches("yarn add react"));
        assert!(filter.matches("yarn run build"));

        // Should not match other commands
        assert!(!filter.matches("npm install"));
    }

    #[test]
    fn test_custom_filter_args_with_flags() {
        let filter = CustomCommandFilter::with_args(
            "hoge",
            vec!["--fuga".to_string(), "-f".to_string()],
            "Block!!!".to_string(),
        )
        .unwrap();

        // Should match
        assert!(filter.matches("hoge --fuga"));
        assert!(filter.matches("hoge -f value"));

        // Should not match
        assert!(!filter.matches("hoge --other"));
        assert!(!filter.matches("hoge run"));
    }

    #[test]
    fn test_custom_filter_args_with_regex_command() {
        // Test regex pattern in command field with args mode
        let filter = CustomCommandFilter::with_args(
            "pip3?",
            vec!["install".to_string(), "uninstall".to_string()],
            "Use uv pip instead".to_string(),
        )
        .unwrap();

        // Should match both pip and pip3
        assert!(filter.matches("pip install requests"));
        assert!(filter.matches("pip3 install requests"));
        assert!(filter.matches("pip uninstall requests"));
        assert!(filter.matches("pip3 uninstall requests"));

        // Should not match other subcommands
        assert!(!filter.matches("pip list"));
        assert!(!filter.matches("pip3 --version"));

        // Should not match other commands
        assert!(!filter.matches("python install"));
    }

    // === Edge Case Tests ===

    #[test]
    fn test_custom_filter_invalid_regex_returns_error() {
        // Invalid regex pattern should return error
        assert!(CustomCommandFilter::new("[", "msg".to_string()).is_err());
        assert!(CustomCommandFilter::new("(unclosed", "msg".to_string()).is_err());
    }

    #[test]
    fn test_custom_filter_args_ignores_flags_before_arg() {
        // args mode matches first argument, flags before should be ignored
        let filter = CustomCommandFilter::with_args(
            "npm",
            vec!["install".to_string()],
            "Use pnpm instead".to_string(),
        )
        .unwrap();

        // Should match when install is the first non-flag argument
        assert!(filter.matches("npm install"));
        assert!(filter.matches("npm install lodash"));

        // Should not match when install is not first argument
        // (--silent is not in args, so this is checking first arg is not "install")
        assert!(!filter.matches("npm --silent install"));
    }

    #[test]
    fn test_custom_filter_with_sudo_wrapper() {
        // Note: CustomFilter uses extract_command_strings which focuses on direct command matching.
        // sudo/env wrappers are NOT unwrapped for custom filters (unlike built-in rm/kill filters).
        // This is a known limitation - custom filters match on command name at start of string.
        let filter = CustomCommandFilter::new("npm", "Use pnpm instead".to_string()).unwrap();

        // Direct npm commands match
        assert!(filter.matches("npm install"));

        // env with VAR=value prefix: npm is still the command
        // This works because extract_command_strings handles env prefix
        assert!(filter.matches("NODE_ENV=prod npm install"));

        // sudo wrapper: Currently does NOT match (known limitation)
        // The command is "sudo", not "npm" from CustomFilter's perspective
        assert!(!filter.matches("sudo npm install"));
    }

    #[test]
    fn test_custom_filter_with_bash_c_subshell() {
        let filter = CustomCommandFilter::new("npm", "Use pnpm instead".to_string()).unwrap();

        // Should match npm in bash -c
        assert!(filter.matches("bash -c 'npm install'"));
        assert!(filter.matches("sh -c \"npm install\""));
    }

    // === strip_quoted_content edge cases ===

    #[test]
    fn test_strip_quoted_content_nested_quotes() {
        // Single quotes inside double quotes: inner content stripped
        let result = CustomCommandFilter::strip_quoted_content(r#"echo "it's fine""#);
        assert_eq!(result, "echo ");
    }

    #[test]
    fn test_strip_quoted_content_escaped_chars() {
        // \" is an escaped quote, not a real quote, so "hello" remains visible
        let result = CustomCommandFilter::strip_quoted_content(r#"echo \"hello\""#);
        assert_eq!(result, "echo hello");
    }

    #[test]
    fn test_strip_quoted_content_empty_quotes() {
        let result = CustomCommandFilter::strip_quoted_content("echo '' \"\"");
        assert_eq!(result, "echo  ");
    }

    #[test]
    fn test_strip_quoted_content_no_quotes() {
        let result = CustomCommandFilter::strip_quoted_content("ls -la /tmp");
        assert_eq!(result, "ls -la /tmp");
    }

    #[test]
    fn test_strip_quoted_content_backslash_at_end() {
        // Backslash at end of string (no char to escape)
        let result = CustomCommandFilter::strip_quoted_content("echo \\");
        assert_eq!(result, "echo ");
    }

    // === Priority and Filter trait tests ===

    #[test]
    fn test_custom_filter_priority() {
        let filter = CustomCommandFilter::new("test", "msg".to_string()).unwrap();
        assert_eq!(filter.priority(), 50);
    }

    #[test]
    fn test_custom_filter_does_not_apply_to_non_bash_tool() {
        let filter = CustomCommandFilter::new("npm", "msg".to_string()).unwrap();
        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(crate::domain::FileOperationInput {
                file_path: "/tmp/npm.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };
        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_custom_filter_does_not_apply_to_after_file_edit() {
        let filter = CustomCommandFilter::new("npm", "msg".to_string()).unwrap();
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "npm install".to_string(),
                timeout: None,
            }),
            session_id: None,
        };
        assert!(!filter.applies_to(&input));
    }

    #[test]
    fn test_custom_filter_execute_returns_block_with_message() {
        let filter = CustomCommandFilter::new("npm", "Use pnpm instead".to_string()).unwrap();
        let input = HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(crate::domain::BashInput {
                command: "npm install".to_string(),
                timeout: None,
            }),
            session_id: None,
        };
        match filter.execute(&input) {
            Decision::Block { message } => {
                assert_eq!(message, "Use pnpm instead");
            }
            _ => panic!("Expected Block"),
        }
    }
}
