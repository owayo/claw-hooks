//! Shell command parser.
//!
//! Provides functionality to extract commands from shell command strings.
//! Uses tree-sitter-bash for accurate AST-based parsing when the `ast-parser` feature is enabled.

#[cfg(feature = "ast-parser")]
use tree_sitter::{Node, Parser};

/// Wrappers that execute another command
const COMMAND_WRAPPERS: &[&str] = &[
    "sudo", "env", "nohup", "nice", "ionice", "time", "timeout", "strace", "ltrace", "doas",
];

/// Shells that can execute command strings via -c flag
const SHELL_COMMANDS: &[&str] = &["bash", "sh", "zsh", "ksh", "csh", "tcsh", "fish", "dash"];

/// Shell command parser using tree-sitter-bash for AST-based analysis.
pub struct ShellParser {
    #[cfg(feature = "ast-parser")]
    parser: Parser,
}

impl ShellParser {
    /// Create a new ShellParser.
    pub fn new() -> Self {
        #[cfg(feature = "ast-parser")]
        {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_bash::LANGUAGE.into())
                .expect("Failed to load tree-sitter-bash grammar");
            Self { parser }
        }
        #[cfg(not(feature = "ast-parser"))]
        {
            Self {}
        }
    }

    /// Extract commands from a shell command string.
    ///
    /// Handles:
    /// - Pipelines (|)
    /// - Logical operators (&&, ||)
    /// - Semicolons (;)
    /// - Command wrappers (sudo, env, nohup, etc.)
    /// - Subshells (bash -c, sh -c, etc.)
    /// - xargs with commands
    #[cfg(feature = "ast-parser")]
    pub fn extract_commands(&mut self, command: &str) -> Vec<String> {
        let tree = match self.parser.parse(command, None) {
            Some(tree) => tree,
            None => return self.extract_commands_fallback(command),
        };

        let root = tree.root_node();
        let mut commands = Vec::new();
        // Now handles wrappers and subshells directly within extract_commands_from_node
        // using AST-based argument extraction instead of string search
        self.extract_commands_from_node(root, command, &mut commands);

        commands
    }

    #[cfg(not(feature = "ast-parser"))]
    pub fn extract_commands(&self, command: &str) -> Vec<String> {
        self.extract_commands_fallback(command)
    }

    /// Extract commands from AST node recursively
    #[cfg(feature = "ast-parser")]
    fn extract_commands_from_node(&mut self, node: Node, source: &str, commands: &mut Vec<String>) {
        match node.kind() {
            "command" | "simple_command" => {
                // Find the command_name child
                if let Some(cmd_name) = self.get_command_name(node, source) {
                    if !cmd_name.is_empty() {
                        commands.push(cmd_name.clone());
                    }

                    // Get arguments for further processing
                    let args = self.get_command_arguments(node, source);

                    // Handle command wrappers at AST level (sudo, env, etc.)
                    if COMMAND_WRAPPERS.contains(&cmd_name.as_str()) {
                        self.process_wrapper_args(&args, commands);
                    }

                    // Handle shell -c "command" at AST level
                    if SHELL_COMMANDS.contains(&cmd_name.as_str()) {
                        if let Some(shell_cmd) = Self::extract_shell_c_from_args(&args) {
                            let nested = self.extract_commands(&shell_cmd);
                            for nested_cmd in nested {
                                if !commands.contains(&nested_cmd) {
                                    commands.push(nested_cmd);
                                }
                            }
                        }
                    }

                    // Handle xargs at AST level
                    if cmd_name == "xargs" {
                        if let Some(xargs_cmd) = Self::extract_xargs_from_args(&args) {
                            if !commands.contains(&xargs_cmd) {
                                commands.push(xargs_cmd);
                            }
                        }
                    }
                }
                // Also recurse into children to find command substitutions in arguments
                // e.g., echo $(yarn --version) - need to find yarn inside $()
                for child in node.children(&mut node.walk()) {
                    self.extract_commands_from_node(child, source, commands);
                }
            }
            "subshell" | "command_substitution" => {
                // Parse contents of subshell/command substitution
                for child in node.children(&mut node.walk()) {
                    self.extract_commands_from_node(child, source, commands);
                }
            }
            _ => {
                // Recurse into children
                for child in node.children(&mut node.walk()) {
                    self.extract_commands_from_node(child, source, commands);
                }
            }
        }
    }

    /// Get command arguments from AST node (excludes the command name itself)
    #[cfg(feature = "ast-parser")]
    fn get_command_arguments(&self, node: Node, source: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut found_command_name = false;

        for child in node.children(&mut node.walk()) {
            match child.kind() {
                "command_name" => {
                    found_command_name = true;
                }
                "word" | "string" | "raw_string" | "simple_expansion" | "expansion"
                | "concatenation" => {
                    if found_command_name {
                        let text = source[child.byte_range()]
                            .trim_matches(|c| c == '"' || c == '\'')
                            .to_string();
                        args.push(text);
                    }
                }
                _ => {}
            }
        }

        args
    }

    /// Extract command from shell -c arguments
    #[cfg(feature = "ast-parser")]
    fn extract_shell_c_from_args(args: &[String]) -> Option<String> {
        for (i, arg) in args.iter().enumerate() {
            if arg == "-c" && i + 1 < args.len() {
                return Some(args[i + 1].clone());
            }
        }
        None
    }

    /// Extract command from xargs arguments
    #[cfg(feature = "ast-parser")]
    fn extract_xargs_from_args(args: &[String]) -> Option<String> {
        args.iter().find(|arg| !arg.starts_with('-')).cloned()
    }

    /// Get command name from a command node
    #[cfg(feature = "ast-parser")]
    fn get_command_name(&self, node: Node, source: &str) -> Option<String> {
        for child in node.children(&mut node.walk()) {
            match child.kind() {
                "command_name" => {
                    // Get the actual word inside command_name
                    for inner in child.children(&mut child.walk()) {
                        if inner.kind() == "word" {
                            return Some(
                                source[inner.byte_range()]
                                    .trim_matches(|c| c == '"' || c == '\'')
                                    .to_string(),
                            );
                        }
                    }
                    // Fallback: use the command_name text directly
                    return Some(
                        source[child.byte_range()]
                            .trim_matches(|c| c == '"' || c == '\'')
                            .to_string(),
                    );
                }
                "word" => {
                    // First word in simple_command might be the command
                    let text = source[child.byte_range()]
                        .trim_matches(|c| c == '"' || c == '\'')
                        .to_string();
                    if !text.starts_with('-') && !text.contains('=') {
                        return Some(text);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Process wrapper arguments to find the actual command
    /// Recursively handles nested wrappers (e.g., sudo bash -c 'rm')
    #[cfg(feature = "ast-parser")]
    fn process_wrapper_args(&mut self, args: &[String], commands: &mut Vec<String>) {
        let mut skip_next = false;
        for (i, arg) in args.iter().enumerate() {
            if skip_next {
                skip_next = false;
                continue;
            }
            if arg.starts_with('-') {
                if Self::flag_takes_arg(arg) {
                    skip_next = true;
                }
                continue;
            }
            if arg.contains('=') {
                continue;
            }
            // Found the actual command
            if !commands.contains(arg) {
                commands.push(arg.clone());
            }

            // Get remaining arguments after this command
            let remaining_args: Vec<String> = args[i + 1..].to_vec();

            // If the found command is a shell, check for -c argument
            if SHELL_COMMANDS.contains(&arg.as_str()) {
                if let Some(shell_cmd) = Self::extract_shell_c_from_args(&remaining_args) {
                    let nested = self.extract_commands(&shell_cmd);
                    for nested_cmd in nested {
                        if !commands.contains(&nested_cmd) {
                            commands.push(nested_cmd);
                        }
                    }
                }
            }

            // If the found command is also a wrapper, process its remaining args
            if COMMAND_WRAPPERS.contains(&arg.as_str()) {
                self.process_wrapper_args(&remaining_args, commands);
            }

            break;
        }
    }

    /// Flags that take an argument (value) for common wrappers
    const FLAGS_WITH_ARGS: &[&str] = &[
        // sudo flags
        "-u", "-g", "-C", "-D", "-R", "-T", "-h", "-p", "-r", "-t", "-U", // env flags
        "-S", // timeout flags
        "-k", "-s", // nice/ionice flags
        "-n", "-c",
    ];

    /// Check if a flag takes an argument
    fn flag_takes_arg(flag: &str) -> bool {
        if flag.contains('=') {
            return false;
        }
        Self::FLAGS_WITH_ARGS.contains(&flag)
    }

    /// Fallback parser using string manipulation
    fn extract_commands_fallback(&self, command: &str) -> Vec<String> {
        let mut commands = Vec::new();

        for segment in command.split(';') {
            for part in Self::split_by_logical_ops(segment.trim()) {
                for pipe_part in part.split('|') {
                    let cmd = pipe_part.trim();
                    if !cmd.is_empty() {
                        commands.extend(self.extract_commands_from_segment_fallback(cmd));
                    }
                }
            }
        }

        commands
    }

    /// Extract commands from a single segment (fallback)
    fn extract_commands_from_segment_fallback(&self, segment: &str) -> Vec<String> {
        let mut commands = Vec::new();
        let (cmd, args) = self.extract_command_with_args_fallback(segment);

        if cmd.is_empty() {
            return commands;
        }

        commands.push(cmd.clone());

        // Handle command wrappers
        if COMMAND_WRAPPERS.contains(&cmd.as_str()) {
            let mut skip_next = false;
            for (i, arg) in args.iter().enumerate() {
                if skip_next {
                    skip_next = false;
                    continue;
                }
                if arg.starts_with('-') {
                    if Self::flag_takes_arg(arg) {
                        skip_next = true;
                    }
                    continue;
                }
                if cmd == "env" && arg.contains('=') {
                    continue;
                }
                commands.push(arg.clone());
                let remaining: Vec<String> = args[i..].to_vec();
                if !remaining.is_empty() {
                    let remaining_str = remaining.join(" ");
                    commands.extend(self.extract_commands_from_segment_fallback(&remaining_str));
                }
                break;
            }
        }

        // Handle shell -c "command"
        if SHELL_COMMANDS.contains(&cmd.as_str()) {
            for (i, arg) in args.iter().enumerate() {
                if arg == "-c" && i + 1 < args.len() {
                    let shell_cmd = &args[i + 1];
                    commands.extend(self.extract_commands_fallback(shell_cmd));
                    break;
                }
            }
        }

        // Handle xargs
        if cmd == "xargs" {
            for arg in &args {
                if arg.starts_with('-') {
                    continue;
                }
                commands.push(arg.clone());
                break;
            }
        }

        commands
    }

    /// Split by && and || operators
    fn split_by_logical_ops(s: &str) -> Vec<&str> {
        let mut result = Vec::new();
        let mut current_start = 0;
        let chars: Vec<char> = s.chars().collect();
        let len = chars.len();
        let mut i = 0;

        while i < len {
            if i + 1 < len {
                let two_chars: String = chars[i..=i + 1].iter().collect();
                if two_chars == "&&" || two_chars == "||" {
                    let part = &s[current_start..i];
                    if !part.trim().is_empty() {
                        result.push(part.trim());
                    }
                    current_start = i + 2;
                    i += 2;
                    continue;
                }
            }
            i += 1;
        }

        let remaining = &s[current_start..];
        if !remaining.trim().is_empty() {
            result.push(remaining.trim());
        }

        result
    }

    /// Extract command with its arguments (fallback string-based parser).
    fn extract_command_with_args_fallback(&self, command: &str) -> (String, Vec<String>) {
        let mut parts = Vec::new();
        let mut current = String::new();
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let mut escape_next = false;

        for c in command.trim().chars() {
            if escape_next {
                current.push(c);
                escape_next = false;
                continue;
            }

            match c {
                '\\' if !in_single_quote => {
                    escape_next = true;
                }
                '\'' if !in_double_quote => {
                    in_single_quote = !in_single_quote;
                }
                '"' if !in_single_quote => {
                    in_double_quote = !in_double_quote;
                }
                ' ' | '\t' if !in_single_quote && !in_double_quote => {
                    if !current.is_empty() {
                        parts.push(current.clone());
                        current.clear();
                    }
                }
                _ => {
                    current.push(c);
                }
            }
        }

        if !current.is_empty() {
            parts.push(current);
        }

        if parts.is_empty() {
            return (String::new(), Vec::new());
        }

        let cmd = parts.remove(0);
        (cmd, parts)
    }

    /// Extract command with its arguments (public API).
    #[allow(dead_code)]
    pub fn extract_command_with_args(&self, command: &str) -> (String, Vec<String>) {
        self.extract_command_with_args_fallback(command)
    }
}

impl Default for ShellParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a command string into tokens, respecting shell quoting rules.
/// This is a standalone function that can be used without creating a ShellParser.
///
/// # Examples
/// ```
/// let tokens = parse_shell_tokens("echo 'hello world'");
/// assert_eq!(tokens, vec!["echo", "hello world"]);
/// ```
pub fn parse_shell_tokens(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escape_next = false;

    for c in command.trim().chars() {
        if escape_next {
            current.push(c);
            escape_next = false;
            continue;
        }

        match c {
            '\\' if !in_single_quote => {
                escape_next = true;
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
            }
            ' ' | '\t' if !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    parts.push(current.clone());
                    current.clear();
                }
            }
            _ => {
                current.push(c);
            }
        }
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_simple_command() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("ls -la");
        assert!(commands.contains(&"ls".to_string()));
    }

    #[test]
    fn test_extract_piped_commands() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("cat file.txt | grep error | wc -l");
        assert!(commands.contains(&"cat".to_string()));
        assert!(commands.contains(&"grep".to_string()));
        assert!(commands.contains(&"wc".to_string()));
    }

    #[test]
    fn test_extract_logical_ops() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("mkdir -p dir && cd dir && ls");
        assert!(commands.contains(&"mkdir".to_string()));
        assert!(commands.contains(&"cd".to_string()));
        assert!(commands.contains(&"ls".to_string()));
    }

    #[test]
    fn test_extract_semicolon() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo hello; echo world");
        assert!(commands.iter().filter(|c| *c == "echo").count() >= 2);
    }

    #[test]
    fn test_extract_command_with_args() {
        let parser = ShellParser::new();
        let (cmd, args) = parser.extract_command_with_args("git commit -m \"Hello world\"");
        assert_eq!(cmd, "git");
        assert_eq!(args, vec!["commit", "-m", "Hello world"]);
    }

    #[test]
    fn test_extract_command_with_single_quotes() {
        let parser = ShellParser::new();
        let (cmd, args) = parser.extract_command_with_args("echo 'hello world'");
        assert_eq!(cmd, "echo");
        assert_eq!(args, vec!["hello world"]);
    }

    // === Wrapper and subshell detection tests ===

    #[test]
    fn test_extract_sudo_wrapper() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo rm -rf /tmp/test");
        assert!(commands.contains(&"sudo".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_sudo_with_flags() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo -u root rm -rf /tmp/test");
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_env_wrapper() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("env PATH=/usr/bin rm file.txt");
        assert!(commands.contains(&"env".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_bash_c_subshell() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("bash -c 'rm -rf /tmp/test'");
        assert!(commands.contains(&"bash".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_sh_c_subshell() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sh -c \"kill -9 1234\"");
        assert!(commands.contains(&"sh".to_string()));
        assert!(commands.contains(&"kill".to_string()));
    }

    #[test]
    fn test_extract_xargs_command() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("find . -name '*.tmp' | xargs rm");
        assert!(commands.contains(&"find".to_string()));
        assert!(commands.contains(&"xargs".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_xargs_with_flags() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("pgrep node | xargs -r kill -9");
        assert!(commands.contains(&"xargs".to_string()));
        assert!(commands.contains(&"kill".to_string()));
    }

    #[test]
    fn test_extract_nested_wrappers() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo bash -c 'rm -rf /'");
        assert!(commands.contains(&"sudo".to_string()));
        assert!(commands.contains(&"bash".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_nohup_wrapper() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("nohup kill -9 1234 &");
        assert!(commands.contains(&"nohup".to_string()));
        assert!(commands.contains(&"kill".to_string()));
    }

    #[test]
    fn test_extract_semicolon_with_yarn() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo \"install\"; yarn install");
        assert!(commands.contains(&"echo".to_string()));
        assert!(commands.contains(&"yarn".to_string()));
    }

    #[test]
    fn test_extract_semicolon_with_pnpm() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo \"not yarn install\"; pnpm install");
        assert!(commands.contains(&"echo".to_string()));
        assert!(commands.contains(&"pnpm".to_string()));
        // Should NOT contain yarn from the quoted string
        assert!(!commands.contains(&"yarn".to_string()));
    }

    #[test]
    fn test_extract_commands_in_quotes_not_executed() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo 'rm -rf /'");
        assert!(commands.contains(&"echo".to_string()));
        // rm should not be extracted since it's inside quotes (an argument)
        assert!(!commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_command_substitution() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo $(yarn --version)");
        assert!(commands.contains(&"echo".to_string()));
        // yarn inside $() should be extracted as a command
        assert!(
            commands.contains(&"yarn".to_string()),
            "yarn should be extracted from command substitution: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_command_substitution_backticks() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo `yarn --version`");
        assert!(commands.contains(&"echo".to_string()));
        // yarn inside backticks should be extracted as a command
        assert!(
            commands.contains(&"yarn".to_string()),
            "yarn should be extracted from backtick command substitution: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_subshell() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("(cd project && yarn install)");
        assert!(commands.contains(&"cd".to_string()));
        assert!(commands.contains(&"yarn".to_string()));
    }
}
