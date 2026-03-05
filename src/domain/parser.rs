//! シェルコマンドパーサー。
//!
//! シェルコマンド文字列からコマンドを抽出する機能を提供する。
//! `ast-parser` フィーチャーが有効な場合、tree-sitter-bash による正確な AST ベースの解析を使用する。

#[cfg(feature = "ast-parser")]
use tree_sitter::{Node, Parser};

/// 実コマンドを実行するラッパーコマンド
const COMMAND_WRAPPERS: &[&str] = &[
    "sudo", "env", "nohup", "nice", "ionice", "time", "timeout", "strace", "ltrace", "doas",
    "command",
];

/// -c フラグでコマンド文字列を実行できるシェル
const SHELL_COMMANDS: &[&str] = &["bash", "sh", "zsh", "ksh", "csh", "tcsh", "fish", "dash"];

/// tree-sitter-bash を使用した AST ベースのシェルコマンドパーサー。
pub struct ShellParser {
    #[cfg(feature = "ast-parser")]
    parser: Parser,
}

impl ShellParser {
    /// 新しい ShellParser を作成する。
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

    /// シェルコマンド文字列からコマンドを抽出する。
    ///
    /// 対応する構文:
    /// - パイプライン (|)
    /// - 論理演算子 (&&, ||)
    /// - セミコロン (;)
    /// - ラッパーコマンド (sudo, env, nohup 等)
    /// - サブシェル (bash -c, sh -c 等)
    /// - xargs 付きコマンド
    #[cfg(feature = "ast-parser")]
    pub fn extract_commands(&mut self, command: &str) -> Vec<String> {
        let tree = match self.parser.parse(command, None) {
            Some(tree) => tree,
            None => return self.extract_commands_fallback(command),
        };

        let root = tree.root_node();
        let mut commands = Vec::new();
        // 文字列検索の代わりに AST ベースの引数抽出を使用して
        // ラッパーとサブシェルを extract_commands_from_node 内で直接処理する
        self.extract_commands_from_node(root, command, &mut commands);

        commands
    }

    #[cfg(not(feature = "ast-parser"))]
    pub fn extract_commands(&self, command: &str) -> Vec<String> {
        self.extract_commands_fallback(command)
    }

    /// シェルコマンド文字列から完全なコマンド文字列（コマンド名 + 引数）を抽出する。
    /// "npm install" のようなパターンをマッチするためにカスタムフィルターで使用される。
    #[cfg(feature = "ast-parser")]
    pub fn extract_command_strings(&mut self, command: &str) -> Vec<String> {
        let tree = match self.parser.parse(command, None) {
            Some(tree) => tree,
            None => return self.extract_command_strings_fallback(command),
        };

        let root = tree.root_node();
        let mut command_strings = Vec::new();
        self.extract_command_strings_from_node(root, command, &mut command_strings);

        command_strings
    }

    #[cfg(not(feature = "ast-parser"))]
    pub fn extract_command_strings(&self, command: &str) -> Vec<String> {
        self.extract_command_strings_fallback(command)
    }

    /// AST ノードから完全なコマンド文字列を再帰的に抽出する。
    /// 正確なパターンマッチングのためにクォートを保持した生の引数を使用する。
    #[cfg(feature = "ast-parser")]
    fn extract_command_strings_from_node(
        &mut self,
        node: Node,
        source: &str,
        command_strings: &mut Vec<String>,
    ) {
        match node.kind() {
            "command" | "simple_command" => {
                if let Some(cmd_name) = self.get_command_name(node, source) {
                    if !cmd_name.is_empty() {
                        // 完全なコマンド文字列を構築: コマンド + 引数（クォート保持）
                        let args_raw = self.get_command_arguments_raw(node, source);
                        let full_cmd = if args_raw.is_empty() {
                            cmd_name.clone()
                        } else {
                            format!("{} {}", cmd_name, args_raw.join(" "))
                        };
                        command_strings.push(full_cmd);

                        // shell -c "command" を処理 - ネストされたコマンド文字列を抽出
                        // シェルコマンド抽出にはクォート除去済み引数を使用
                        if SHELL_COMMANDS.contains(&cmd_name.as_str()) {
                            let args = self.get_command_arguments(node, source);
                            if let Some(shell_cmd) = Self::extract_shell_c_from_args(&args) {
                                let nested = self.extract_command_strings(&shell_cmd);
                                command_strings.extend(nested);
                            }
                        }

                        // xargs を処理 - 実行されるコマンドを抽出
                        if cmd_name == "xargs" {
                            let args = self.get_command_arguments(node, source);
                            // xargs 対象コマンド文字列を構築
                            let xargs_args: Vec<_> =
                                args.iter().filter(|a| !a.starts_with('-')).collect();
                            if !xargs_args.is_empty() {
                                let xargs_cmd = xargs_args
                                    .iter()
                                    .map(|s| s.as_str())
                                    .collect::<Vec<_>>()
                                    .join(" ");
                                command_strings.push(xargs_cmd);
                            }
                        }
                    }
                }
                // コマンド置換のために子ノードに再帰
                for child in node.children(&mut node.walk()) {
                    self.extract_command_strings_from_node(child, source, command_strings);
                }
            }
            "subshell" | "command_substitution" => {
                for child in node.children(&mut node.walk()) {
                    self.extract_command_strings_from_node(child, source, command_strings);
                }
            }
            _ => {
                for child in node.children(&mut node.walk()) {
                    self.extract_command_strings_from_node(child, source, command_strings);
                }
            }
        }
    }

    /// extract_command_strings のフォールバックパーサー
    fn extract_command_strings_fallback(&self, command: &str) -> Vec<String> {
        let mut command_strings = Vec::new();

        for segment in command.split(';') {
            for part in Self::split_by_logical_ops(segment.trim()) {
                for pipe_part in part.split('|') {
                    let cmd = pipe_part.trim();
                    if !cmd.is_empty() {
                        command_strings
                            .extend(self.extract_command_strings_from_segment_fallback(cmd));
                    }
                }
            }
        }

        command_strings
    }

    /// 単一セグメントから完全なコマンド文字列を抽出する（フォールバック）。
    fn extract_command_strings_from_segment_fallback(&self, segment: &str) -> Vec<String> {
        let mut command_strings = Vec::new();
        let trimmed = segment.trim();

        if trimmed.is_empty() {
            return command_strings;
        }

        if let Some(inner) = Self::unwrap_subshell(trimmed) {
            return self.extract_command_strings_fallback(inner);
        }

        let tokens = parse_shell_tokens(trimmed);
        let Some((cmd_name, args)) = Self::parse_effective_command(&tokens) else {
            return command_strings;
        };

        let full_cmd = if args.is_empty() {
            cmd_name.clone()
        } else {
            format!("{} {}", cmd_name, args.join(" "))
        };
        command_strings.push(full_cmd);

        // shell -c "command" を処理
        if SHELL_COMMANDS.contains(&cmd_name.as_str()) {
            if let Some(shell_cmd) = Self::extract_shell_c_from_args(&args) {
                command_strings.extend(self.extract_command_strings_fallback(&shell_cmd));
            }
        }

        // xargs 対象コマンドを処理
        if cmd_name == "xargs" {
            let xargs_args: Vec<_> = args.iter().filter(|a| !a.starts_with('-')).collect();
            if !xargs_args.is_empty() {
                command_strings.push(
                    xargs_args
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                );
            }
        }

        // 引数内のコマンド置換を処理。
        for nested in Self::extract_nested_command_fragments(trimmed) {
            command_strings.extend(self.extract_command_strings_fallback(&nested));
        }

        command_strings
    }

    /// ASTノードを再帰的に走査してコマンドを抽出する
    #[cfg(feature = "ast-parser")]
    fn extract_commands_from_node(&mut self, node: Node, source: &str, commands: &mut Vec<String>) {
        match node.kind() {
            "command" | "simple_command" => {
                // command_name の子ノードを取得
                if let Some(cmd_name) = self.get_command_name(node, source) {
                    if !cmd_name.is_empty() {
                        commands.push(cmd_name.clone());
                    }

                    // 後続解析のため引数を取得
                    let args = self.get_command_arguments(node, source);

                    // ASTレベルでラッパーコマンドを展開（sudo, env, command など）
                    if COMMAND_WRAPPERS.contains(&cmd_name.as_str()) {
                        self.process_wrapper_args(&cmd_name, &args, commands);
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
    /// Strips quotes from arguments for internal processing.
    #[cfg(feature = "ast-parser")]
    fn get_command_arguments(&self, node: Node, source: &str) -> Vec<String> {
        self.get_command_arguments_impl(node, source, true)
    }

    /// Get command arguments with quotes preserved for pattern matching.
    #[cfg(feature = "ast-parser")]
    fn get_command_arguments_raw(&self, node: Node, source: &str) -> Vec<String> {
        self.get_command_arguments_impl(node, source, false)
    }

    /// Implementation: Get command arguments with optional quote stripping.
    #[cfg(feature = "ast-parser")]
    fn get_command_arguments_impl(
        &self,
        node: Node,
        source: &str,
        strip_quotes: bool,
    ) -> Vec<String> {
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
                        let text = if strip_quotes {
                            source[child.byte_range()]
                                .trim_matches(|c| c == '"' || c == '\'')
                                .to_string()
                        } else {
                            source[child.byte_range()].to_string()
                        };
                        args.push(text);
                    }
                }
                _ => {}
            }
        }

        args
    }

    /// Extract command from shell -c arguments
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

    /// ラッパー引数を処理して実行対象コマンドを見つける
    /// 入れ子ラッパーも再帰的に処理する（例: sudo bash -c 'rm'）
    #[cfg(feature = "ast-parser")]
    fn process_wrapper_args(&mut self, wrapper: &str, args: &[String], commands: &mut Vec<String>) {
        let mut skip_next = false;
        for (i, arg) in args.iter().enumerate() {
            if skip_next {
                skip_next = false;
                continue;
            }
            if arg.starts_with('-') {
                if Self::wrapper_flag_takes_arg(wrapper, arg) {
                    skip_next = true;
                }
                continue;
            }
            if wrapper == "env" && Self::is_env_assignment_token(arg) {
                continue;
            }
            // 実行されるコマンドを検出
            if !commands.contains(arg) {
                commands.push(arg.clone());
            }

            // このコマンド以降の残り引数
            let remaining_args: Vec<String> = args[i + 1..].to_vec();

            // shell -c の場合は内側のコマンドを抽出
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

            // 次のコマンドもラッパーなら再帰的に処理
            if COMMAND_WRAPPERS.contains(&arg.as_str()) {
                self.process_wrapper_args(arg, &remaining_args, commands);
            }

            break;
        }
    }

    /// ラッパーごとに「値を取る」ことが確定している短縮フラグ
    const SUDO_FLAGS_WITH_ARGS: &[&str] = &[
        "-u", "-g", "-C", "-D", "-R", "-T", "-h", "-p", "-r", "-t", "-U",
    ];
    const ENV_FLAGS_WITH_ARGS: &[&str] = &["-u", "-C", "-S"];
    const TIMEOUT_FLAGS_WITH_ARGS: &[&str] = &["-k", "-s"];
    const NICE_FLAGS_WITH_ARGS: &[&str] = &["-n"];
    const IONICE_FLAGS_WITH_ARGS: &[&str] = &["-c", "-n"];
    const DOAS_FLAGS_WITH_ARGS: &[&str] = &["-u"];

    /// 指定ラッパー上で、フラグが値を取るかを判定する
    fn wrapper_flag_takes_arg(wrapper: &str, flag: &str) -> bool {
        if flag.contains('=') {
            return false;
        }
        match wrapper {
            "sudo" => Self::SUDO_FLAGS_WITH_ARGS.contains(&flag),
            "env" => Self::ENV_FLAGS_WITH_ARGS.contains(&flag),
            "timeout" => Self::TIMEOUT_FLAGS_WITH_ARGS.contains(&flag),
            "nice" => Self::NICE_FLAGS_WITH_ARGS.contains(&flag),
            "ionice" => Self::IONICE_FLAGS_WITH_ARGS.contains(&flag),
            "doas" => Self::DOAS_FLAGS_WITH_ARGS.contains(&flag),
            _ => false,
        }
    }

    /// Returns true for shell-style env assignments (e.g., KEY=value).
    fn is_env_assignment_token(token: &str) -> bool {
        let Some((name, _value)) = token.split_once('=') else {
            return false;
        };
        if name.is_empty() {
            return false;
        }
        let mut chars = name.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !(first == '_' || first.is_ascii_alphabetic()) {
            return false;
        }
        chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
    }

    /// Parse the effective command from a tokenized segment,
    /// skipping leading environment assignments.
    fn parse_effective_command(tokens: &[String]) -> Option<(String, Vec<String>)> {
        if tokens.is_empty() {
            return None;
        }

        let start = tokens
            .iter()
            .position(|token| !Self::is_env_assignment_token(token))?;

        Some((tokens[start].clone(), tokens[start + 1..].to_vec()))
    }

    /// If the segment is exactly wrapped by a single top-level `( ... )`, return inner text.
    fn unwrap_subshell(segment: &str) -> Option<&str> {
        let trimmed = segment.trim();
        if !(trimmed.starts_with('(') && trimmed.ends_with(')')) {
            return None;
        }

        let last_index = trimmed.char_indices().last()?.0;
        let mut depth = 0usize;
        let mut in_single = false;
        let mut in_double = false;
        let mut escape = false;

        for (idx, ch) in trimmed.char_indices() {
            if escape {
                escape = false;
                continue;
            }

            if ch == '\\' && !in_single {
                escape = true;
                continue;
            }
            if ch == '\'' && !in_double {
                in_single = !in_single;
                continue;
            }
            if ch == '"' && !in_single {
                in_double = !in_double;
                continue;
            }
            if in_single || in_double {
                continue;
            }

            match ch {
                '(' => depth += 1,
                ')' => {
                    if depth == 0 {
                        return None;
                    }
                    depth -= 1;
                    // Outer-most pair must close at the final character.
                    if depth == 0 && idx != last_index {
                        return None;
                    }
                }
                _ => {}
            }
        }

        if depth == 0 {
            Some(&trimmed[1..trimmed.len() - 1])
        } else {
            None
        }
    }

    /// Extract nested command fragments from command substitutions ($(...), `...`).
    fn extract_nested_command_fragments(segment: &str) -> Vec<String> {
        let chars: Vec<char> = segment.chars().collect();
        let len = chars.len();
        let mut fragments = Vec::new();
        let mut i = 0usize;
        let mut in_single = false;
        let mut in_double = false;
        let mut escape = false;

        while i < len {
            let ch = chars[i];

            if escape {
                escape = false;
                i += 1;
                continue;
            }

            if ch == '\\' && !in_single {
                escape = true;
                i += 1;
                continue;
            }
            if ch == '\'' && !in_double {
                in_single = !in_single;
                i += 1;
                continue;
            }
            if ch == '"' && !in_single {
                in_double = !in_double;
                i += 1;
                continue;
            }

            // $(...)
            if !in_single && ch == '$' && i + 1 < len && chars[i + 1] == '(' {
                let start = i + 2;
                let mut j = start;
                let mut depth = 1usize;
                let mut sub_in_single = false;
                let mut sub_in_double = false;
                let mut sub_escape = false;

                while j < len {
                    let sub = chars[j];

                    if sub_escape {
                        sub_escape = false;
                        j += 1;
                        continue;
                    }

                    if sub == '\\' && !sub_in_single {
                        sub_escape = true;
                        j += 1;
                        continue;
                    }
                    if sub == '\'' && !sub_in_double {
                        sub_in_single = !sub_in_single;
                        j += 1;
                        continue;
                    }
                    if sub == '"' && !sub_in_single {
                        sub_in_double = !sub_in_double;
                        j += 1;
                        continue;
                    }
                    if sub_in_single || sub_in_double {
                        j += 1;
                        continue;
                    }

                    if sub == '(' {
                        depth += 1;
                    } else if sub == ')' {
                        depth -= 1;
                        if depth == 0 {
                            let inner: String = chars[start..j].iter().collect();
                            if !inner.trim().is_empty() {
                                fragments.push(inner);
                            }
                            i = j + 1;
                            break;
                        }
                    }

                    j += 1;
                }

                if depth == 0 {
                    continue;
                }
            }

            // `...`
            if !in_single && ch == '`' {
                let start = i + 1;
                let mut j = start;
                let mut sub_escape = false;
                let mut found = false;

                while j < len {
                    let sub = chars[j];
                    if sub_escape {
                        sub_escape = false;
                        j += 1;
                        continue;
                    }
                    if sub == '\\' {
                        sub_escape = true;
                        j += 1;
                        continue;
                    }
                    if sub == '`' {
                        let inner: String = chars[start..j].iter().collect();
                        if !inner.trim().is_empty() {
                            fragments.push(inner);
                        }
                        i = j + 1;
                        found = true;
                        break;
                    }
                    j += 1;
                }

                if found {
                    continue;
                }
            }

            i += 1;
        }

        fragments
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
        let trimmed = segment.trim();

        if trimmed.is_empty() {
            return commands;
        }

        if let Some(inner) = Self::unwrap_subshell(trimmed) {
            return self.extract_commands_fallback(inner);
        }

        let tokens = parse_shell_tokens(trimmed);
        let Some((cmd, args)) = Self::parse_effective_command(&tokens) else {
            return commands;
        };

        commands.push(cmd.clone());

        // ラッパーコマンドを展開
        if COMMAND_WRAPPERS.contains(&cmd.as_str()) {
            let mut skip_next = false;
            for (i, arg) in args.iter().enumerate() {
                if skip_next {
                    skip_next = false;
                    continue;
                }
                if arg.starts_with('-') {
                    if Self::wrapper_flag_takes_arg(&cmd, arg) {
                        skip_next = true;
                    }
                    continue;
                }
                if cmd == "env" && Self::is_env_assignment_token(arg) {
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

        // Handle command substitutions in arguments (e.g., echo $(rm -rf /tmp)).
        for nested in Self::extract_nested_command_fragments(trimmed) {
            commands.extend(self.extract_commands_fallback(&nested));
        }

        commands
    }

    /// Split by && and || operators
    fn split_by_logical_ops(s: &str) -> Vec<&str> {
        let mut result = Vec::new();
        let mut current_start = 0;
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let mut escape_next = false;
        let mut paren_depth = 0usize;
        let mut chars = s.char_indices().peekable();

        while let Some((idx, c)) = chars.next() {
            if escape_next {
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
                '(' if !in_single_quote && !in_double_quote => {
                    paren_depth += 1;
                }
                ')' if !in_single_quote && !in_double_quote => {
                    paren_depth = paren_depth.saturating_sub(1);
                }
                '&' | '|' if !in_single_quote && !in_double_quote && paren_depth == 0 => {
                    if let Some(&(next_idx, next_c)) = chars.peek() {
                        if next_c == c {
                            let part = &s[current_start..idx];
                            if !part.trim().is_empty() {
                                result.push(part.trim());
                            }
                            // Consume the second operator char and move start after it.
                            let _ = chars.next();
                            current_start = next_idx + next_c.len_utf8();
                        }
                    }
                }
                _ => {}
            }
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

    #[test]
    fn test_extract_commands_with_env_assignment_prefix() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("NODE_ENV=prod npm install");
        assert!(commands.contains(&"npm".to_string()));
    }

    #[test]
    fn test_extract_command_strings_with_bash_c_subshell() {
        let mut parser = ShellParser::new();
        let command_strings = parser.extract_command_strings("bash -c 'npm install'");
        assert!(command_strings.iter().any(|s| s == "npm install"));
    }

    #[test]
    fn test_extract_command_strings_from_command_substitution() {
        let mut parser = ShellParser::new();
        let command_strings = parser.extract_command_strings("echo $(npm install)");
        assert!(command_strings.iter().any(|s| s == "npm install"));
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
    fn test_extract_sudo_with_non_interactive_flag() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo -n rm -rf /tmp/test");
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_command_wrapper() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("command rm -rf /tmp/test");
        assert!(commands.contains(&"command".to_string()));
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

    // === Boundary Condition Tests ===

    #[test]
    fn test_extract_commands_empty_input() {
        let mut parser = ShellParser::new();
        assert!(parser.extract_commands("").is_empty());
    }

    #[test]
    fn test_extract_commands_whitespace_only() {
        let mut parser = ShellParser::new();
        assert!(parser.extract_commands("   ").is_empty());
        assert!(parser.extract_commands("\t\n").is_empty());
    }

    #[test]
    fn test_extract_commands_trailing_operator() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("ls &&");
        assert!(commands.contains(&"ls".to_string()));
    }

    #[test]
    fn test_extract_commands_leading_operator() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("&& ls");
        // Should still extract ls even with leading operator
        assert!(commands.contains(&"ls".to_string()));
    }

    #[test]
    fn test_parse_shell_tokens_empty() {
        let tokens = parse_shell_tokens("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_parse_shell_tokens_whitespace_only() {
        let tokens = parse_shell_tokens("   ");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_parse_shell_tokens_escaped_space() {
        let tokens = parse_shell_tokens("echo foo\\ bar");
        assert_eq!(tokens, vec!["echo", "foo bar"]);
    }

    #[test]
    fn test_parse_shell_tokens_escaped_quote() {
        let tokens = parse_shell_tokens("echo \"hello\\\"world\"");
        assert_eq!(tokens, vec!["echo", "hello\"world"]);
    }

    #[test]
    fn test_parse_shell_tokens_mixed_quotes() {
        let tokens = parse_shell_tokens("echo 'single' \"double\"");
        assert_eq!(tokens, vec!["echo", "single", "double"]);
    }

    #[cfg(feature = "ast-parser")]
    #[test]
    fn test_extract_commands_ignores_operators_in_quotes() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo \"a && b\" && rm -rf /tmp/test");
        assert!(commands.contains(&"echo".to_string()));
        assert!(commands.contains(&"rm".to_string()));
        // "b" should NOT be extracted as it's inside quotes
        assert!(!commands.contains(&"b".to_string()));
    }

    #[test]
    fn test_extract_commands_newline_separated() {
        let mut parser = ShellParser::new();
        // Commands separated by semicolon (newlines handled by shell)
        let commands = parser.extract_commands("ls; echo hello");
        assert!(commands.contains(&"ls".to_string()));
        assert!(commands.contains(&"echo".to_string()));
    }
}
