//! シェルコマンドパーサー。
//!
//! シェルコマンド文字列からコマンドを抽出する機能を提供する。
//! `ast-parser` フィーチャーが有効な場合、tree-sitter-bash による正確な AST ベースの解析を使用する。

#[cfg(feature = "ast-parser")]
use tree_sitter::{Node, Parser};

/// 実コマンドを実行するラッパーコマンド
const COMMAND_WRAPPERS: &[&str] = &[
    "sudo", "env", "nohup", "nice", "ionice", "time", "timeout", "strace", "ltrace", "doas",
    "command", "exec",
];

/// -c フラグでコマンド文字列を実行できるシェル
const SHELL_COMMANDS: &[&str] = &[
    "bash", "sh", "zsh", "ksh", "csh", "tcsh", "fish", "dash", "cmd",
];

/// find で後続引数をコマンドとして実行する述語
const FIND_EXEC_PREDICATES: &[&str] = &["-exec", "-execdir"];

/// コマンド名を判定用のキーに正規化する。
///
/// 以下を行う:
/// - basename 抽出（`/bin/rm`、`./rm`、`C:\Windows\rm.exe` → `rm`）
/// - 実行可能ファイル拡張子の除去（`.exe`、`.cmd`、`.bat`、`.com`、大文字小文字を問わない）
/// - 小文字化（`DEL`、`Rm` → `del`、`rm`）
///
/// これにより `/bin/rm`・`cmd.exe`・`DEL` などのパス・拡張子・大文字経由のバイパスを防ぐ。
pub(crate) fn command_key(command: &str) -> String {
    let leaf = command
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(command);
    let lower = leaf.to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".com"] {
        if let Some(stripped) = lower.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    lower
}

/// SHELL_COMMANDS と一致するかを正規化キーで判定する。
fn is_shell_command(name: &str) -> bool {
    let key = command_key(name);
    SHELL_COMMANDS.iter().any(|s| *s == key)
}

/// COMMAND_WRAPPERS と一致するかを正規化キーで判定する。
fn is_command_wrapper(name: &str) -> bool {
    let key = command_key(name);
    COMMAND_WRAPPERS.iter().any(|s| *s == key)
}

/// 与えられた cmd_name が target コマンド（例: "xargs"、"eval"、"find"）かを
/// パス・拡張子・大文字小文字を考慮して判定する。
fn matches_command(cmd_name: &str, target: &str) -> bool {
    command_key(cmd_name) == target
}

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
    pub fn extract_commands(&mut self, command: &str) -> Vec<String> {
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
    pub fn extract_command_strings(&mut self, command: &str) -> Vec<String> {
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
                        if is_shell_command(&cmd_name) {
                            let args = self.get_command_arguments(node, source);
                            if let Some(shell_cmd) = Self::extract_shell_c_from_args(&args) {
                                let nested = self.extract_command_strings(&shell_cmd);
                                command_strings.extend(nested);
                            }
                        }

                        // xargs を処理 - 実行されるコマンドを抽出
                        if matches_command(&cmd_name, "xargs") {
                            let args = self.get_command_arguments(node, source);
                            if let Some(xargs_cmd) =
                                Self::extract_xargs_command_string_from_args(&args)
                            {
                                command_strings.push(xargs_cmd.clone());
                                command_strings.extend(self.extract_command_strings(&xargs_cmd));
                            }
                        }

                        // eval は引数をシェルとして再評価するため、内側の文字列も解析する
                        if matches_command(&cmd_name, "eval") {
                            let args = self.get_command_arguments(node, source);
                            if let Some(eval_cmd) = Self::join_eval_args(&args) {
                                command_strings.extend(self.extract_command_strings(&eval_cmd));
                            }
                        }

                        // find -exec/-execdir は後続の引数をコマンドとして実行する
                        if matches_command(&cmd_name, "find") {
                            let args = self.get_command_arguments(node, source);
                            for exec_cmd in Self::extract_find_exec_commands(&args) {
                                command_strings.extend(self.extract_command_strings(&exec_cmd));
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

        for segment in Self::split_respecting_quotes(command, ';') {
            for part in Self::split_by_logical_ops(segment) {
                for pipe_part in Self::split_respecting_quotes(part, '|') {
                    if !pipe_part.is_empty() {
                        command_strings
                            .extend(self.extract_command_strings_from_segment_fallback(pipe_part));
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
        if is_shell_command(&cmd_name) {
            if let Some(shell_cmd) = Self::extract_shell_c_from_args(&args) {
                command_strings.extend(self.extract_command_strings_fallback(&shell_cmd));
            }
        }

        // xargs 対象コマンドを処理
        if matches_command(&cmd_name, "xargs") {
            if let Some(xargs_cmd) = Self::extract_xargs_command_string_from_args(&args) {
                command_strings.push(xargs_cmd.clone());
                command_strings.extend(self.extract_command_strings_fallback(&xargs_cmd));
            }
        }

        // eval は引数をシェルとして再評価するため、内側の文字列も解析する。
        if matches_command(&cmd_name, "eval") {
            if let Some(eval_cmd) = Self::join_eval_args(&args) {
                command_strings.extend(self.extract_command_strings_fallback(&eval_cmd));
            }
        }

        // find -exec/-execdir は後続の引数をコマンドとして実行する。
        if matches_command(&cmd_name, "find") {
            for exec_cmd in Self::extract_find_exec_commands(&args) {
                command_strings.extend(self.extract_command_strings_fallback(&exec_cmd));
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
                    if is_command_wrapper(&cmd_name) {
                        self.process_wrapper_args(&cmd_name, &args, commands);
                    }

                    // AST レベルで shell -c "command" を処理
                    if is_shell_command(&cmd_name) {
                        if let Some(shell_cmd) = Self::extract_shell_c_from_args(&args) {
                            let nested = self.extract_commands(&shell_cmd);
                            for nested_cmd in nested {
                                if !commands.contains(&nested_cmd) {
                                    commands.push(nested_cmd);
                                }
                            }
                        }
                    }

                    // AST レベルで xargs を処理
                    if matches_command(&cmd_name, "xargs") {
                        if let Some(xargs_cmd) = Self::extract_xargs_command_string_from_args(&args)
                        {
                            for nested_cmd in self.extract_commands(&xargs_cmd) {
                                Self::push_unique_command(commands, &nested_cmd);
                            }
                        }
                    }

                    // AST レベルで eval を処理
                    if matches_command(&cmd_name, "eval") {
                        if let Some(eval_cmd) = Self::join_eval_args(&args) {
                            for nested_cmd in self.extract_commands(&eval_cmd) {
                                Self::push_unique_command(commands, &nested_cmd);
                            }
                        }
                    }

                    // AST レベルで find -exec/-execdir を処理
                    if matches_command(&cmd_name, "find") {
                        for exec_cmd in Self::extract_find_exec_commands(&args) {
                            for nested_cmd in self.extract_commands(&exec_cmd) {
                                Self::push_unique_command(commands, &nested_cmd);
                            }
                        }
                    }
                }
                // 引数内のコマンド置換を拾うために子ノードも再帰的に探索する。
                // 例: echo $(yarn --version) から yarn を抽出する。
                for child in node.children(&mut node.walk()) {
                    self.extract_commands_from_node(child, source, commands);
                }
            }
            "subshell" | "command_substitution" => {
                // サブシェル/コマンド置換の中身を再帰解析する。
                for child in node.children(&mut node.walk()) {
                    self.extract_commands_from_node(child, source, commands);
                }
            }
            _ => {
                // 子ノードを再帰走査する。
                for child in node.children(&mut node.walk()) {
                    self.extract_commands_from_node(child, source, commands);
                }
            }
        }
    }

    /// AST ノードから引数を取得する（コマンド名自体は除外）。
    /// 内部処理用にクォートを除去する。
    #[cfg(feature = "ast-parser")]
    fn get_command_arguments(&self, node: Node, source: &str) -> Vec<String> {
        self.get_command_arguments_impl(node, source, true)
    }

    /// パターンマッチ用途でクォートを保持したまま引数を取得する。
    #[cfg(feature = "ast-parser")]
    fn get_command_arguments_raw(&self, node: Node, source: &str) -> Vec<String> {
        self.get_command_arguments_impl(node, source, false)
    }

    /// 実装本体: クォート除去有無を切り替えて引数を取得する。
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
                // `number`（裸の整数）も引数に含める。これを欠落させると
                // `xargs -n 1 rm` / `sudo -u 1000 rm` / `nice -n 10 rm` のような
                // 「値を取るフラグ + 数値」で数値が脱落し、フラグが後続のコマンド
                // (rm 等) を値として消費して危険コマンド検出が漏れる。
                "word" | "string" | "raw_string" | "simple_expansion" | "expansion"
                | "concatenation" | "number"
                    if found_command_name =>
                {
                    let raw = &source[child.byte_range()];
                    let text = if strip_quotes {
                        Self::normalize_shell_word(raw)
                    } else {
                        raw.to_string()
                    };
                    args.push(text);
                }
                _ => {}
            }
        }

        args
    }

    /// shell -c 形式の引数から実行文字列を取り出す。
    fn extract_shell_c_from_args(args: &[String]) -> Option<String> {
        for (i, arg) in args.iter().enumerate() {
            let lower = arg.to_ascii_lowercase();
            if matches!(lower.as_str(), "/c" | "/k") && i + 1 < args.len() {
                return Some(args[i + 1..].join(" "));
            }

            let has_shell_c = arg == "-c"
                || (arg.starts_with('-')
                    && !arg.starts_with("--")
                    && arg.chars().skip(1).any(|c| c == 'c'));
            if has_shell_c {
                // `bash -c -- 'script'` のように -c とスクリプトの間に `--`
                // （オプション終端）が挟まる場合は読み飛ばす。読み飛ばさないと
                // `--` をスクリプト本体とみなし、内側の rm 等を取りこぼす。
                let mut script_idx = i + 1;
                if args.get(script_idx).map(String::as_str) == Some("--") {
                    script_idx += 1;
                }
                if script_idx < args.len() {
                    return Some(args[script_idx].clone());
                }
            }
        }
        None
    }

    /// xargs の引数から実行コマンド文字列を取り出す。
    fn extract_xargs_command_string_from_args(args: &[String]) -> Option<String> {
        Self::find_xargs_command_index(args).map(|index| args[index..].join(" "))
    }

    /// xargs のオプションを読み飛ばし、実行対象コマンドの位置を返す。
    fn find_xargs_command_index(args: &[String]) -> Option<usize> {
        let mut i = 0usize;
        while i < args.len() {
            let arg = &args[i];

            if arg == "--" {
                return (i + 1 < args.len()).then_some(i + 1);
            }

            if arg.starts_with('-') {
                if Self::xargs_flag_takes_arg(arg) {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }

            return Some(i);
        }

        None
    }

    /// xargs で次トークンを値として消費するオプションか判定する。
    fn xargs_flag_takes_arg(flag: &str) -> bool {
        let (base_flag, has_inline_value) = match flag.split_once('=') {
            Some((base, _)) => (base, true),
            None => (flag, false),
        };

        if has_inline_value {
            return false;
        }

        if base_flag.starts_with("--") {
            return matches!(
                base_flag,
                "--arg-file"
                    | "--delimiter"
                    | "--eof"
                    | "--max-args"
                    | "--max-chars"
                    | "--max-lines"
                    | "--max-procs"
                    | "--process-slot-var"
                    | "--replace"
            );
        }

        matches!(
            base_flag,
            "-a" | "-d" | "-E" | "-I" | "-L" | "-n" | "-P" | "-s"
        )
    }

    /// eval の引数を再解析用のコマンド文字列へ戻す。
    fn join_eval_args(args: &[String]) -> Option<String> {
        (!args.is_empty()).then(|| args.join(" "))
    }

    /// find の -exec/-execdir 述語から実行コマンド文字列を取り出す。
    fn extract_find_exec_commands(args: &[String]) -> Vec<String> {
        let mut commands = Vec::new();
        let mut i = 0;

        while i < args.len() {
            if FIND_EXEC_PREDICATES.contains(&args[i].as_str()) {
                let start = i + 1;
                let mut end = start;
                while end < args.len() && !Self::is_find_exec_terminator(&args[end]) {
                    end += 1;
                }
                if start < end {
                    commands.push(args[start..end].join(" "));
                }
                i = end;
            }
            i += 1;
        }

        commands
    }

    /// find -exec の終端記号かどうかを判定する。
    fn is_find_exec_terminator(arg: &str) -> bool {
        matches!(arg, ";" | r"\;" | "+")
    }

    /// command ノードからコマンド名を取得する。
    #[cfg(feature = "ast-parser")]
    fn get_command_name(&self, node: Node, source: &str) -> Option<String> {
        for child in node.children(&mut node.walk()) {
            match child.kind() {
                "command_name" => {
                    // command_name 全体を正規化して、`r\m` や `r''m` のような
                    // シェルの quote removal 後に実行されるコマンド名で判定する。
                    return Some(Self::normalize_shell_word(&source[child.byte_range()]));
                }
                "word" => {
                    // simple_command の先頭 word がコマンド名である場合に拾う。
                    let text = Self::normalize_shell_word(&source[child.byte_range()]);
                    if !text.starts_with('-') && !text.contains('=') {
                        return Some(text);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// シェルの quote removal に近い形で1トークンを正規化する。
    ///
    /// tree-sitter-bash は `r\m` や `r''m` の raw 表記を保持するため、
    /// 危険コマンド判定ではシェルが実際に実行するコマンド名へ寄せる必要がある。
    fn normalize_shell_word(word: &str) -> String {
        let mut result = String::with_capacity(word.len());
        let mut chars = word.chars().peekable();
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let preserve_windows_separators = word.len() >= 3
            && word.as_bytes()[0].is_ascii_alphabetic()
            && word.as_bytes()[1] == b':'
            && word.as_bytes()[2] == b'\\';

        while let Some(c) = chars.next() {
            match c {
                '$' if !in_single_quote && !in_double_quote => {
                    if chars.peek() == Some(&'\'') {
                        // Bash の ANSI-C quoting: $'r\x6d' は quote removal 後に `rm`。
                        let _ = chars.next();
                        result.push_str(&Self::read_ansi_c_quoted(&mut chars));
                    } else if chars.peek() == Some(&'"') {
                        // Bash の $"..." はロケール翻訳文字列。未翻訳時は通常の二重引用と同じ。
                        // `$` 自体は quote removal 後のコマンド名に残らないため捨てる。
                    } else {
                        result.push(c);
                    }
                }
                '\\' if !in_single_quote => {
                    if let Some(next) = chars.next() {
                        if preserve_windows_separators && !in_double_quote {
                            result.push('\\');
                            result.push(next);
                            continue;
                        }
                        if in_double_quote && !matches!(next, '$' | '`' | '"' | '\\' | '\n' | '\r')
                        {
                            result.push('\\');
                        }
                        if next != '\n' && next != '\r' {
                            result.push(next);
                        }
                    } else {
                        result.push(c);
                    }
                }
                '\'' if !in_double_quote => {
                    in_single_quote = !in_single_quote;
                }
                '"' if !in_single_quote => {
                    in_double_quote = !in_double_quote;
                }
                _ => result.push(c),
            }
        }

        result
    }

    /// Bash の ANSI-C quoted string (`$'...'`) をコマンド名比較用に展開する。
    fn read_ansi_c_quoted(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
        let mut result = String::new();

        while let Some(c) = chars.next() {
            match c {
                '\'' => break,
                '\\' => {
                    if let Some(decoded) = Self::read_ansi_c_escape(chars) {
                        result.push(decoded);
                    }
                }
                _ => result.push(c),
            }
        }

        result
    }

    /// ANSI-C quoted string 内のバックスラッシュエスケープを1文字読む。
    fn read_ansi_c_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<char> {
        let escaped = chars.next()?;
        match escaped {
            'a' => Some('\x07'),
            'b' => Some('\x08'),
            'e' | 'E' => Some('\x1b'),
            'f' => Some('\x0c'),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            'v' => Some('\x0b'),
            '\\' => Some('\\'),
            '\'' => Some('\''),
            '"' => Some('"'),
            '\n' | '\r' => None,
            'x' => Self::read_radix_escape(chars, 2, 16),
            'u' => Self::read_radix_escape(chars, 4, 16),
            'U' => Self::read_radix_escape(chars, 8, 16),
            '0'..='7' => {
                let mut digits = String::from(escaped);
                digits.push_str(&Self::take_while_limited(chars, 2, |c| {
                    matches!(c, '0'..='7')
                }));
                u32::from_str_radix(&digits, 8)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ => Some(escaped),
        }
    }

    /// 最大桁数付きの基数エスケープを読む。
    fn read_radix_escape(
        chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
        max_digits: usize,
        radix: u32,
    ) -> Option<char> {
        let digits = Self::take_while_limited(chars, max_digits, |c| c.is_digit(radix));
        if digits.is_empty() {
            return None;
        }
        u32::from_str_radix(&digits, radix)
            .ok()
            .and_then(char::from_u32)
    }

    /// 条件に合う文字を最大 `limit` 文字まで取り出す。
    fn take_while_limited<F>(
        chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
        limit: usize,
        mut predicate: F,
    ) -> String
    where
        F: FnMut(char) -> bool,
    {
        let mut result = String::new();
        while result.len() < limit {
            let Some(&next) = chars.peek() else {
                break;
            };
            if !predicate(next) {
                break;
            }
            result.push(next);
            let _ = chars.next();
        }
        result
    }

    /// ラッパー引数を処理して実行対象コマンドを見つける
    /// 入れ子ラッパーも再帰的に処理する（例: sudo bash -c 'rm'）
    #[cfg(feature = "ast-parser")]
    fn process_wrapper_args(&mut self, wrapper: &str, args: &[String], commands: &mut Vec<String>) {
        if let Some(command_index) = Self::find_wrapped_command_index(wrapper, args) {
            let command_name = &args[command_index];

            if !commands.contains(command_name) {
                commands.push(command_name.clone());
            }

            // このコマンド以降の残り引数
            let remaining_args: Vec<String> = args[command_index + 1..].to_vec();

            // shell -c の場合は内側のコマンドを抽出
            if is_shell_command(command_name) {
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
            if is_command_wrapper(command_name) {
                self.process_wrapper_args(command_name, &remaining_args, commands);
            }
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
    const EXEC_FLAGS_WITH_ARGS: &[&str] = &["-a"];
    const SUDO_LONG_FLAGS_WITH_ARGS: &[&str] = &[
        "--user",
        "--group",
        "--host",
        "--chdir",
        "--prompt",
        "--other-user",
    ];
    const ENV_LONG_FLAGS_WITH_ARGS: &[&str] = &[
        "--unset",
        "--chdir",
        "--argv0",
        "--split-string",
        "--block-signal",
        "--default-signal",
        "--ignore-signal",
    ];
    const TIMEOUT_LONG_FLAGS_WITH_ARGS: &[&str] = &["--signal", "--kill-after"];
    const NICE_LONG_FLAGS_WITH_ARGS: &[&str] = &["--adjustment"];
    const IONICE_LONG_FLAGS_WITH_ARGS: &[&str] = &["--class", "--classdata"];

    /// 指定ラッパーで「値を次トークンから取る」フラグかを判定する。
    fn wrapper_flag_takes_arg(wrapper: &str, flag: &str) -> bool {
        if !flag.starts_with('-') || flag == "-" || flag == "--" {
            return false;
        }

        let (base_flag, has_inline_value) = match flag.split_once('=') {
            Some((base, _)) => (base, true),
            None => (flag, false),
        };

        if base_flag.starts_with("--") {
            if has_inline_value {
                return false;
            }
            return Self::wrapper_long_flags_with_args(wrapper).contains(&base_flag);
        }

        // 短縮フラグの cluster（例: `-nu`、`-uroot`）を解釈する。
        // - `-uroot`（先頭が値取得フラグで以降が値）→ 追加トークン不要
        // - `-nu`（cluster の末尾だけが値取得フラグ）→ 追加トークンが必要
        // - `-nv`（値取得フラグを含まない）→ 追加トークン不要
        if base_flag.len() > 2 {
            let cluster = &base_flag[1..];
            let value_flags = Self::wrapper_short_flags_with_args(wrapper);
            for (idx, ch) in cluster.char_indices() {
                let opt = format!("-{}", ch);
                if value_flags.contains(&opt.as_str()) {
                    // cluster 末尾が値取得フラグなら追加トークン必要、
                    // それ以前の位置にあれば残りが inline 値とみなされる。
                    let has_inline_value = idx + ch.len_utf8() < cluster.len();
                    return !has_inline_value;
                }
            }
            return false;
        }

        Self::wrapper_short_flags_with_args(wrapper).contains(&base_flag)
    }

    fn wrapper_short_flags_with_args(wrapper: &str) -> &'static [&'static str] {
        let key = command_key(wrapper);
        match key.as_str() {
            "sudo" => Self::SUDO_FLAGS_WITH_ARGS,
            "env" => Self::ENV_FLAGS_WITH_ARGS,
            "timeout" => Self::TIMEOUT_FLAGS_WITH_ARGS,
            "nice" => Self::NICE_FLAGS_WITH_ARGS,
            "ionice" => Self::IONICE_FLAGS_WITH_ARGS,
            "doas" => Self::DOAS_FLAGS_WITH_ARGS,
            "exec" => Self::EXEC_FLAGS_WITH_ARGS,
            _ => &[],
        }
    }

    fn wrapper_long_flags_with_args(wrapper: &str) -> &'static [&'static str] {
        let key = command_key(wrapper);
        match key.as_str() {
            "sudo" => Self::SUDO_LONG_FLAGS_WITH_ARGS,
            "env" => Self::ENV_LONG_FLAGS_WITH_ARGS,
            "timeout" => Self::TIMEOUT_LONG_FLAGS_WITH_ARGS,
            "nice" => Self::NICE_LONG_FLAGS_WITH_ARGS,
            "ionice" => Self::IONICE_LONG_FLAGS_WITH_ARGS,
            _ => &[],
        }
    }

    /// GNU timeout の制限時間トークンかを判定する（例: 10, 0.5, 30s, 5m）。
    fn is_timeout_duration_token(token: &str) -> bool {
        if token.is_empty() {
            return false;
        }

        let trimmed = token.trim();
        let value = match trimmed.chars().last() {
            Some('s' | 'm' | 'h' | 'd') => &trimmed[..trimmed.len() - 1],
            _ => trimmed,
        };

        if value.is_empty() {
            return false;
        }

        value
            .parse::<f64>()
            .is_ok_and(|n| n.is_finite() && n >= 0.0)
    }

    /// ラッパー引数列から実際に実行されるコマンド位置を返す。
    fn find_wrapped_command_index(wrapper: &str, args: &[String]) -> Option<usize> {
        let mut i = 0usize;
        let mut timeout_duration_consumed = false;
        let wrapper_key = command_key(wrapper);

        while i < args.len() {
            let arg = &args[i];

            if arg == "--" {
                return (i + 1 < args.len()).then_some(i + 1);
            }

            if wrapper_key == "env" && Self::is_env_assignment_token(arg) {
                i += 1;
                continue;
            }

            // sudo は `sudo VAR=value command` 形式でコマンド直前の環境変数代入を受け付ける。
            // ここを実行対象コマンドとして扱うと、後続の rm/kill/dd を見落とす。
            if wrapper_key == "sudo" && Self::is_env_assignment_token(arg) {
                i += 1;
                continue;
            }

            if arg.starts_with('-') {
                if Self::wrapper_flag_takes_arg(&wrapper_key, arg) {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }

            if wrapper_key == "timeout"
                && !timeout_duration_consumed
                && Self::is_timeout_duration_token(arg)
            {
                timeout_duration_consumed = true;
                i += 1;
                continue;
            }

            return Some(i);
        }

        None
    }

    /// シェル形式の環境変数代入（例: KEY=value）かを判定する。
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

    /// トークン列から実際に実行されるコマンドを取り出す。
    /// 先頭の環境変数代入は読み飛ばす。
    fn parse_effective_command(tokens: &[String]) -> Option<(String, Vec<String>)> {
        if tokens.is_empty() {
            return None;
        }

        let start = tokens
            .iter()
            .position(|token| !Self::is_env_assignment_token(token))?;

        Some((tokens[start].clone(), tokens[start + 1..].to_vec()))
    }

    /// セグメント全体が最上位の `( ... )` で包まれていれば中身を返す。
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
                    // 最外周の括弧は末尾で閉じている必要がある。
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

    /// コマンド置換（`$(...)`, `` `...` ``）から内部断片を抽出する。
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

            // `$(...)` 形式
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

            // `` `...` `` 形式
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

    /// 文字列処理ベースのフォールバックパーサー。
    fn extract_commands_fallback(&self, command: &str) -> Vec<String> {
        let mut commands = Vec::new();

        for segment in Self::split_top_level_terminators(command) {
            for part in Self::split_by_logical_ops(segment) {
                for pipe_part in Self::split_respecting_quotes(part, '|') {
                    if !pipe_part.is_empty() {
                        commands.extend(self.extract_commands_from_segment_fallback(pipe_part));
                    }
                }
            }
        }

        commands
    }

    /// 単一セグメントからコマンドを抽出する（フォールバック）。
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
        if is_command_wrapper(&cmd) {
            self.expand_wrapper_commands_fallback(&cmd, &args, &mut commands);
        }

        // shell -c "command" を処理
        if is_shell_command(&cmd) {
            if let Some(shell_cmd) = Self::extract_shell_c_from_args(&args) {
                commands.extend(self.extract_commands_fallback(&shell_cmd));
            }
        }

        // xargs を処理
        if matches_command(&cmd, "xargs") {
            if let Some(xargs_cmd) = Self::extract_xargs_command_string_from_args(&args) {
                commands.extend(self.extract_commands_fallback(&xargs_cmd));
            }
        }

        // eval は引数をシェルとして再評価するため、内側の文字列も解析する。
        if matches_command(&cmd, "eval") {
            if let Some(eval_cmd) = Self::join_eval_args(&args) {
                commands.extend(self.extract_commands_fallback(&eval_cmd));
            }
        }

        // find -exec/-execdir は後続の引数をコマンドとして実行する。
        if matches_command(&cmd, "find") {
            for exec_cmd in Self::extract_find_exec_commands(&args) {
                commands.extend(self.extract_commands_fallback(&exec_cmd));
            }
        }

        // 引数中のコマンド置換を処理（例: echo $(rm -rf /tmp)）。
        for nested in Self::extract_nested_command_fragments(trimmed) {
            commands.extend(self.extract_commands_fallback(&nested));
        }

        commands
    }

    /// フォールバックパーサーでラッパーコマンドの実行対象を展開する。
    /// `sudo -u root bash -c 'rm -rf /'` のように wrapper → shell -c が
    /// 連続する場合でも、シェルに渡される内側のコマンドを落とさない。
    fn expand_wrapper_commands_fallback(
        &self,
        wrapper: &str,
        args: &[String],
        commands: &mut Vec<String>,
    ) {
        let Some(command_index) = Self::find_wrapped_command_index(wrapper, args) else {
            return;
        };

        let command_name = &args[command_index];
        Self::push_unique_command(commands, command_name);

        let remaining = &args[command_index + 1..];

        if is_shell_command(command_name) {
            if let Some(shell_cmd) = Self::extract_shell_c_from_args(remaining) {
                commands.extend(self.extract_commands_fallback(&shell_cmd));
            }
        }

        if is_command_wrapper(command_name) {
            self.expand_wrapper_commands_fallback(command_name, remaining, commands);
        }
    }

    /// 同じコマンド名を重複して追加しない。
    fn push_unique_command(commands: &mut Vec<String>, command: &str) {
        if !commands.iter().any(|existing| existing == command) {
            commands.push(command.to_string());
        }
    }

    /// トップレベルのコマンド終端子で分割する。
    /// `;`、改行 (`\n`)、単独の `&` (バックグラウンド実行) を区切りとして扱う。
    /// 引用符内、サブシェル `(...)` の内側、エスケープされた文字は分割しない。
    /// `&&` や `||` はここでは分割せず、後続の split_by_logical_ops に委ねる。
    fn split_top_level_terminators(s: &str) -> Vec<&str> {
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
                ';' | '\n' if !in_single_quote && !in_double_quote && paren_depth == 0 => {
                    let part = &s[current_start..idx];
                    if !part.trim().is_empty() {
                        result.push(part.trim());
                    }
                    current_start = idx + c.len_utf8();
                }
                '&' if !in_single_quote && !in_double_quote && paren_depth == 0 => {
                    let next_is_amp = chars.peek().is_some_and(|(_, nc)| *nc == '&');
                    if next_is_amp {
                        // `&&` (論理 AND) は後続の split_by_logical_ops に委ねる。
                        // ここで2文字目の `&` を消費しておかないと、次のループ反復で
                        // 単独 `&` として誤って分割されてしまう。
                        let _ = chars.next();
                    } else {
                        // 単独 `&` (バックグラウンド実行) はここで分割する。
                        let part = &s[current_start..idx];
                        if !part.trim().is_empty() {
                            result.push(part.trim());
                        }
                        current_start = idx + c.len_utf8();
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

    /// クォート（`'` `"`）と括弧 `()` を考慮して、単一文字 `sep` で分割する。
    /// クォート/括弧の内側の `sep` は区切りとして扱わない。
    /// `|` をシェルの構造を保ったまま分割するために使う。
    fn split_respecting_quotes(s: &str, sep: char) -> Vec<&str> {
        let mut result = Vec::new();
        let mut current_start = 0;
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let mut escape_next = false;
        let mut paren_depth = 0usize;

        for (idx, c) in s.char_indices() {
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
                ch if ch == sep && !in_single_quote && !in_double_quote && paren_depth == 0 => {
                    let part = &s[current_start..idx];
                    if !part.trim().is_empty() {
                        result.push(part.trim());
                    }
                    current_start = idx + ch.len_utf8();
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

    /// `&&` と `||` で分割する。
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
                            // 2文字目の演算子も消費し、次の開始位置を更新する。
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

    /// コマンドと引数を抽出する（文字列ベースのフォールバック）。
    fn extract_command_with_args_fallback(&self, command: &str) -> (String, Vec<String>) {
        let mut parts = parse_shell_tokens(command);
        if parts.is_empty() {
            return (String::new(), Vec::new());
        }

        let cmd = parts.remove(0);
        (cmd, parts)
    }

    /// コマンドと引数を抽出する（公開 API）。
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

/// シェルのクォート規則を考慮してコマンド文字列をトークン化する。
/// `ShellParser` を生成せずに使える独立関数。
///
/// # 使用例
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
            // 分割判定ではエスケープを考慮しつつ、quote removal は後段に任せる。
            current.push(c);
            escape_next = false;
            continue;
        }

        match c {
            '\\' if !in_single_quote => {
                current.push(c);
                escape_next = true;
            }
            '\'' if !in_double_quote => {
                current.push(c);
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                current.push(c);
                in_double_quote = !in_double_quote;
            }
            ' ' | '\t' | '\n' | '\r' if !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    parts.push(ShellParser::normalize_shell_word(&current));
                    current.clear();
                }
            }
            _ => {
                current.push(c);
            }
        }
    }

    if !current.is_empty() {
        parts.push(ShellParser::normalize_shell_word(&current));
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
    fn test_extract_background_amp_separator() {
        // 単独の `&` (バックグラウンド実行) でもコマンドが分割されること。
        // 後続コマンドが見落とされると危険コマンドブロックが回避される。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo ok & rm -rf /tmp/dummy");
        assert!(commands.contains(&"echo".to_string()));
        assert!(
            commands.contains(&"rm".to_string()),
            "single `&` should split commands; got {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_newline_separator() {
        // 改行で区切られた複数コマンドも個別に抽出できること。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo ok\nrm -rf /tmp/dummy");
        assert!(commands.contains(&"echo".to_string()));
        assert!(
            commands.contains(&"rm".to_string()),
            "newline should split commands; got {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_amp_inside_quotes_not_split() {
        // 引用符内の `&` はコマンド分離子として扱わない。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo 'a & b' && ls");
        assert!(commands.contains(&"echo".to_string()));
        assert!(commands.contains(&"ls".to_string()));
        assert!(!commands.contains(&"b".to_string()));
    }

    #[test]
    fn test_extract_backslash_escaped_command_name() {
        // シェルは `r\m` を quote removal 後に `rm` として実行する。
        // raw 表記のままだと危険コマンドブロックを回避できてしまう。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(r"r\m -rf /tmp/dummy");
        assert!(
            commands.contains(&"rm".to_string()),
            "escaped command name should normalize to rm; got {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_quoted_concatenated_command_name() {
        // シェルでは `r''m` も quote removal 後に `rm` として実行される。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("r''m -rf /tmp/dummy");
        assert!(
            commands.contains(&"rm".to_string()),
            "quoted command name should normalize to rm; got {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_ansi_c_quoted_command_name() {
        // Bash の $'...' 形式も quote removal 後のコマンド名で判定する。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(r"$'r\x6d' -rf /tmp/dummy");
        assert!(
            commands.contains(&"rm".to_string()),
            "ANSI-C quoted command name should normalize to rm; got {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_locale_quoted_command_name() {
        // Bash の $"..." は未翻訳なら通常の二重引用と同じコマンド名になる。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(r#"$"rm" -rf /tmp/dummy"#);
        assert!(
            commands.contains(&"rm".to_string()),
            "locale quoted command name should normalize to rm; got {:?}",
            commands
        );
    }

    #[test]
    fn test_split_top_level_terminators_basic() {
        // セミコロン、改行、単独 `&` で分割される。
        let result = ShellParser::split_top_level_terminators("a; b\nc & d");
        assert_eq!(result, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn test_split_top_level_terminators_double_amp_preserved() {
        // `&&` は分割しない（後続の split_by_logical_ops に委ねる）。
        let result = ShellParser::split_top_level_terminators("a && b");
        assert_eq!(result, vec!["a && b"]);
    }

    #[test]
    fn test_split_top_level_terminators_quoted_separators_kept() {
        // 引用符内・サブシェル内の分離子は無視する。
        let result = ShellParser::split_top_level_terminators("echo 'a;b'");
        assert_eq!(result, vec!["echo 'a;b'"]);
        let result = ShellParser::split_top_level_terminators("echo \"a;b\"");
        assert_eq!(result, vec!["echo \"a;b\""]);
        let result = ShellParser::split_top_level_terminators("(a; b); c");
        assert_eq!(result, vec!["(a; b)", "c"]);
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
    fn test_command_key_basename() {
        // basename 抽出: /bin/rm → rm
        assert_eq!(command_key("/bin/rm"), "rm");
        assert_eq!(command_key("/usr/bin/rm"), "rm");
        assert_eq!(command_key("./rm"), "rm");
    }

    #[test]
    fn test_command_key_windows_path() {
        // Windows パス区切り `\` も basename として扱う
        assert_eq!(command_key("C:\\Windows\\rm.exe"), "rm");
    }

    #[test]
    fn test_command_key_extension_strip() {
        // 実行ファイル拡張子の除去
        assert_eq!(command_key("rm.exe"), "rm");
        assert_eq!(command_key("rm.CMD"), "rm");
        assert_eq!(command_key("rm.Bat"), "rm");
        assert_eq!(command_key("rm.com"), "rm");
    }

    #[test]
    fn test_command_key_lowercase() {
        // 大文字小文字を正規化
        assert_eq!(command_key("DEL"), "del");
        assert_eq!(command_key("Rm"), "rm");
        assert_eq!(command_key("TASKKILL.EXE"), "taskkill");
    }

    #[test]
    fn test_command_key_already_normalized() {
        // 既に正規化されている文字列はそのまま
        assert_eq!(command_key("rm"), "rm");
        assert_eq!(command_key("bash"), "bash");
    }

    #[test]
    fn test_is_shell_command_recognizes_cmd_exe() {
        // cmd.exe を cmd と認識する
        assert!(is_shell_command("cmd.exe"));
        assert!(is_shell_command("cmd"));
        assert!(is_shell_command("CMD.EXE"));
    }

    #[test]
    fn test_is_shell_command_recognizes_path_prefixed_bash() {
        // /bin/bash も bash として認識する
        assert!(is_shell_command("/bin/bash"));
        assert!(is_shell_command("/usr/bin/sh"));
    }

    #[test]
    fn test_is_command_wrapper_recognizes_path_prefixed() {
        // /usr/bin/sudo も sudo として認識する
        assert!(is_command_wrapper("/usr/bin/sudo"));
        assert!(is_command_wrapper("sudo"));
    }

    #[test]
    fn test_extract_commands_absolute_path_rm() {
        // /bin/rm の絶対パス指定でもコマンドが抽出される
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("/bin/rm -rf /tmp/test");
        assert!(commands.iter().any(|c| command_key(c) == "rm"));
    }

    #[test]
    fn test_extract_commands_cmd_exe_del() {
        // cmd.exe /c del を経由しても del が抽出される
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("cmd.exe /c del C:\\tmp\\file.txt");
        assert!(commands.iter().any(|c| command_key(c) == "del"));
    }

    #[test]
    fn test_extract_commands_windows_path_command() {
        // Windows ドライブパスの `\` はコマンド名ではパス区切りとして扱う
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("C:\\Windows\\rm.exe C:\\tmp\\file.txt");
        assert!(commands.iter().any(|c| command_key(c) == "rm"));
    }

    #[test]
    fn test_extract_commands_sudo_nu_cluster() {
        // sudo -nu root rm の cluster short option でも rm が抽出される
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo -nu root rm -rf /tmp/test");
        assert!(commands.iter().any(|c| command_key(c) == "rm"));
    }

    #[test]
    fn test_extract_commands_path_prefixed_sudo_with_value_option() {
        // /usr/bin/sudo のようにラッパーがパス付きでも -u の値を読み飛ばし rm を抽出する
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("/usr/bin/sudo -u root rm -rf /tmp/test");
        assert!(commands.iter().any(|c| command_key(c) == "rm"));
    }

    #[test]
    fn test_extract_commands_path_prefixed_timeout_with_value_option() {
        // /usr/bin/timeout のようにラッパーがパス付きでも -s と duration を読み飛ばす
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("/usr/bin/timeout -s TERM 10 rm -f /tmp/test");
        assert!(commands.iter().any(|c| command_key(c) == "rm"));
    }

    #[test]
    fn test_extract_commands_timeout_vs_cluster() {
        // timeout -vs TERM 10 rm の cluster short option でも rm が抽出される
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("timeout -vs TERM 10 rm -f /tmp/test");
        assert!(commands.iter().any(|c| command_key(c) == "rm"));
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

    #[test]
    fn test_extract_command_strings_from_eval() {
        let mut parser = ShellParser::new();
        let command_strings = parser.extract_command_strings("eval 'npm install'");
        assert!(
            command_strings.iter().any(|s| s == "npm install"),
            "eval の内側のコマンド文字列を抽出すべき: {:?}",
            command_strings
        );
    }

    #[test]
    fn test_extract_command_strings_from_find_exec() {
        let mut parser = ShellParser::new();
        let command_strings =
            parser.extract_command_strings(r"find . -name package.json -exec npm install \;");
        assert!(
            command_strings.iter().any(|s| s == "npm install"),
            "find -exec の実行コマンド文字列を抽出すべき: {:?}",
            command_strings
        );
    }

    // === ラッパー・サブシェル検出テスト ===

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
    fn test_extract_sudo_with_long_user_flag() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo --user root rm -rf /tmp/test");
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_sudo_with_non_interactive_flag() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo -n rm -rf /tmp/test");
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_timeout_with_long_signal_option() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("timeout --signal TERM 10 rm -rf /tmp/test");
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
    fn test_extract_exec_wrapper() {
        // exec は現在のシェルを指定コマンドで置き換えて実行する。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("exec rm -rf /tmp/test");
        assert!(commands.contains(&"exec".to_string()));
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
    fn test_extract_bash_combined_c_flag() {
        // bash -lc のように -c が他フラグと結合されても内側のコマンドを抽出する。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("bash -lc 'rm -rf /tmp/test'");
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
    fn test_extract_cmd_c_shell() {
        // Windows の cmd /c 経由で del が実行される場合も検出する。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("cmd /c del C:\\tmp\\file.txt");
        assert!(commands.contains(&"cmd".to_string()));
        assert!(commands.contains(&"del".to_string()));
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
    fn test_extract_xargs_with_replace_flag() {
        // -I は次トークンを置換文字列として消費するため、その後の rm が実行対象。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("find . -name '*.tmp' | xargs -I {} rm -rf {}");
        assert!(commands.contains(&"xargs".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_xargs_shell_c_command() {
        // xargs が sh -c を実行する場合は、シェル内のコマンドまで再帰的に抽出する。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo file | xargs sh -c 'rm -f \"$@\"' sh");
        assert!(commands.contains(&"xargs".to_string()));
        assert!(commands.contains(&"sh".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_eval_inner_command() {
        // eval の引数はシェルとして再評価されるため、内側のコマンドも抽出する
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("eval 'rm -rf /tmp/test'");
        assert!(commands.contains(&"eval".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_find_exec_command() {
        // find -exec/-execdir は後続の引数をコマンドとして実行する
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(r"find . -name '*.tmp' -exec rm -rf {} \;");
        assert!(commands.contains(&"find".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_find_execdir_command_with_plus_terminator() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("find . -type f -execdir kill -9 {} +");
        assert!(commands.contains(&"find".to_string()));
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
        // クォート内の文字列から yarn は抽出しない。
        assert!(!commands.contains(&"yarn".to_string()));
    }

    #[test]
    fn test_extract_commands_in_quotes_not_executed() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo 'rm -rf /'");
        assert!(commands.contains(&"echo".to_string()));
        // rm は引数内クォートなので抽出しない。
        assert!(!commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_extract_command_substitution() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo $(yarn --version)");
        assert!(commands.contains(&"echo".to_string()));
        // $() 内の yarn は実行コマンドとして抽出する。
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
        // バッククォート内の yarn は実行コマンドとして抽出する。
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

    // === 境界条件テスト ===

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
        // 先頭が演算子でも ls は抽出する。
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
        // "b" はクォート内部なので抽出しない。
        assert!(!commands.contains(&"b".to_string()));
    }

    #[test]
    fn test_extract_commands_newline_separated() {
        let mut parser = ShellParser::new();
        // セミコロン区切りのコマンドを抽出する（改行はシェル側で解釈）。
        let commands = parser.extract_commands("ls; echo hello");
        assert!(commands.contains(&"ls".to_string()));
        assert!(commands.contains(&"echo".to_string()));
    }

    // === is_env_assignment_token のテスト ===

    #[test]
    fn test_is_env_assignment_valid() {
        assert!(ShellParser::is_env_assignment_token("FOO=bar"));
        assert!(ShellParser::is_env_assignment_token("A_B_C=value"));
        assert!(ShellParser::is_env_assignment_token("_VAR=1"));
        assert!(ShellParser::is_env_assignment_token("X="));
    }

    #[test]
    fn test_is_env_assignment_invalid() {
        assert!(!ShellParser::is_env_assignment_token("=nokey"));
        assert!(!ShellParser::is_env_assignment_token("123=bad"));
        assert!(!ShellParser::is_env_assignment_token(""));
        assert!(!ShellParser::is_env_assignment_token("noequals"));
        assert!(!ShellParser::is_env_assignment_token("a-b=val"));
    }

    // === is_timeout_duration_token のテスト ===

    #[test]
    fn test_is_timeout_duration_valid() {
        assert!(ShellParser::is_timeout_duration_token("10"));
        assert!(ShellParser::is_timeout_duration_token("3.5"));
        assert!(ShellParser::is_timeout_duration_token("30s"));
        assert!(ShellParser::is_timeout_duration_token("5m"));
        assert!(ShellParser::is_timeout_duration_token("1h"));
        assert!(ShellParser::is_timeout_duration_token("2d"));
        assert!(ShellParser::is_timeout_duration_token("0"));
        assert!(ShellParser::is_timeout_duration_token("0.5"));
    }

    #[test]
    fn test_is_timeout_duration_invalid() {
        assert!(!ShellParser::is_timeout_duration_token(""));
        assert!(!ShellParser::is_timeout_duration_token("abc"));
        assert!(!ShellParser::is_timeout_duration_token("10x"));
        assert!(!ShellParser::is_timeout_duration_token("s"));
        assert!(!ShellParser::is_timeout_duration_token("-1"));
        assert!(!ShellParser::is_timeout_duration_token("NaN"));
    }

    // === unwrap_subshell のテスト ===

    #[test]
    fn test_unwrap_subshell_simple() {
        assert_eq!(ShellParser::unwrap_subshell("(inner)"), Some("inner"));
    }

    #[test]
    fn test_unwrap_subshell_with_spaces() {
        assert_eq!(
            ShellParser::unwrap_subshell("  (echo hello)  "),
            Some("echo hello")
        );
    }

    #[test]
    fn test_unwrap_subshell_nested() {
        assert_eq!(
            ShellParser::unwrap_subshell("(echo $(pwd))"),
            Some("echo $(pwd)")
        );
    }

    #[test]
    fn test_unwrap_subshell_not_wrapped() {
        assert_eq!(ShellParser::unwrap_subshell("echo hello"), None);
    }

    #[test]
    fn test_unwrap_subshell_unbalanced() {
        assert_eq!(ShellParser::unwrap_subshell("(no close"), None);
    }

    #[test]
    fn test_unwrap_subshell_middle_close() {
        // 最外周の括弧が途中で閉じるケース → None
        assert_eq!(ShellParser::unwrap_subshell("(a)(b)"), None);
    }

    // === extract_nested_command_fragments のテスト ===

    #[test]
    fn test_extract_nested_dollar_paren() {
        let frags = ShellParser::extract_nested_command_fragments("echo $(rm -rf /tmp)");
        assert_eq!(frags, vec!["rm -rf /tmp"]);
    }

    #[test]
    fn test_extract_nested_backticks() {
        let frags = ShellParser::extract_nested_command_fragments("echo `ls -la`");
        assert_eq!(frags, vec!["ls -la"]);
    }

    #[test]
    fn test_extract_nested_mixed() {
        let frags = ShellParser::extract_nested_command_fragments("$(cmd1) and `cmd2`");
        assert_eq!(frags, vec!["cmd1", "cmd2"]);
    }

    #[test]
    fn test_extract_nested_none() {
        let frags = ShellParser::extract_nested_command_fragments("plain text");
        assert!(frags.is_empty());
    }

    #[test]
    fn test_extract_nested_in_single_quotes_ignored() {
        // シングルクォート内のコマンド置換は無視される
        let frags = ShellParser::extract_nested_command_fragments("echo '$(dangerous)'");
        assert!(frags.is_empty());
    }

    #[test]
    fn test_extract_nested_dollar_paren_deep() {
        let frags = ShellParser::extract_nested_command_fragments("$(echo $(inner))");
        // 最外周の $() のみ抽出
        assert_eq!(frags, vec!["echo $(inner)"]);
    }

    // === find_wrapped_command_index のテスト ===

    #[test]
    fn test_find_wrapped_sudo_user() {
        let args: Vec<String> = vec!["-u", "root", "rm", "-rf", "/tmp"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("sudo", &args),
            Some(2)
        );
    }

    #[test]
    fn test_find_wrapped_timeout_duration() {
        let args: Vec<String> = vec!["10", "rm", "-rf", "/tmp"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("timeout", &args),
            Some(1)
        );
    }

    #[test]
    fn test_find_wrapped_timeout_with_signal() {
        let args: Vec<String> = vec!["--signal", "TERM", "10", "dd"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("timeout", &args),
            Some(3)
        );
    }

    #[test]
    fn test_find_wrapped_env_with_assignments() {
        let args: Vec<String> = vec!["VAR=x", "FOO=y", "rm", "-rf"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("env", &args),
            Some(2)
        );
    }

    #[test]
    fn test_find_wrapped_double_dash() {
        let args: Vec<String> = vec!["--", "rm", "-rf"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("sudo", &args),
            Some(1)
        );
    }

    #[test]
    fn test_find_wrapped_empty_args() {
        let args: Vec<String> = vec![];
        assert_eq!(ShellParser::find_wrapped_command_index("sudo", &args), None);
    }

    #[test]
    fn test_find_wrapped_only_flags() {
        let args: Vec<String> = vec!["-n".to_string()];
        assert_eq!(ShellParser::find_wrapped_command_index("sudo", &args), None);
    }

    // === parse_effective_command のテスト ===

    #[test]
    fn test_parse_effective_command_with_env_prefix() {
        let tokens: Vec<String> = vec!["VAR=x", "FOO=y", "rm", "-rf", "/tmp"]
            .into_iter()
            .map(String::from)
            .collect();
        let (cmd, args) = ShellParser::parse_effective_command(&tokens).unwrap();
        assert_eq!(cmd, "rm");
        assert_eq!(args, vec!["-rf", "/tmp"]);
    }

    #[test]
    fn test_parse_effective_command_no_env() {
        let tokens: Vec<String> = vec!["ls", "-la"].into_iter().map(String::from).collect();
        let (cmd, args) = ShellParser::parse_effective_command(&tokens).unwrap();
        assert_eq!(cmd, "ls");
        assert_eq!(args, vec!["-la"]);
    }

    #[test]
    fn test_parse_effective_command_all_env() {
        let tokens: Vec<String> = vec!["A=1", "B=2"].into_iter().map(String::from).collect();
        assert!(ShellParser::parse_effective_command(&tokens).is_none());
    }

    #[test]
    fn test_parse_effective_command_empty() {
        let tokens: Vec<String> = vec![];
        assert!(ShellParser::parse_effective_command(&tokens).is_none());
    }

    // === parse_shell_tokens の追加テスト ===

    #[test]
    fn test_parse_shell_tokens_empty_input() {
        // 空文字列は空のベクタを返す
        let tokens = parse_shell_tokens("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_parse_shell_tokens_whitespace_only_mixed() {
        // スペースとタブのみの入力は空のベクタを返す
        let tokens = parse_shell_tokens("  \t  ");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_parse_shell_tokens_single_quotes() {
        // シングルクォート内のスペースはトークンを分割しない
        let tokens = parse_shell_tokens("echo 'hello world'");
        assert_eq!(tokens, vec!["echo", "hello world"]);
    }

    #[test]
    fn test_parse_shell_tokens_double_quotes() {
        // ダブルクォート内のスペースはトークンを分割しない
        let tokens = parse_shell_tokens("echo \"hello world\"");
        assert_eq!(tokens, vec!["echo", "hello world"]);
    }

    #[test]
    fn test_parse_shell_tokens_escaped_space_in_token() {
        // バックスラッシュでエスケープされたスペースはトークンを分割しない
        let tokens = parse_shell_tokens("echo hello\\ world");
        assert_eq!(tokens, vec!["echo", "hello world"]);
    }

    #[test]
    fn test_parse_shell_tokens_mixed_quotes_separate() {
        // シングルクォートとダブルクォートが混在する場合それぞれ独立したトークンになる
        let tokens = parse_shell_tokens("echo 'hello' \"world\"");
        assert_eq!(tokens, vec!["echo", "hello", "world"]);
    }

    #[test]
    fn test_parse_shell_tokens_unmatched_single_quote() {
        // 閉じられていないシングルクォートでも残りの文字列をトークンとして返す
        let tokens = parse_shell_tokens("echo 'hello");
        assert_eq!(tokens, vec!["echo", "hello"]);
    }

    #[test]
    fn test_parse_shell_tokens_unmatched_double_quote() {
        // 閉じられていないダブルクォートでも残りの文字列をトークンとして返す
        let tokens = parse_shell_tokens("echo \"hello");
        assert_eq!(tokens, vec!["echo", "hello"]);
    }

    #[test]
    fn test_parse_shell_tokens_unicode() {
        // Unicode文字（日本語）を正しくトークン分割する
        let tokens = parse_shell_tokens("echo こんにちは 世界");
        assert_eq!(tokens, vec!["echo", "こんにちは", "世界"]);
    }

    #[test]
    fn test_parse_shell_tokens_consecutive_spaces() {
        // 連続するスペースは無視してトークンを正しく分割する
        let tokens = parse_shell_tokens("echo   hello    world");
        assert_eq!(tokens, vec!["echo", "hello", "world"]);
    }

    // ========================================================================
    // wrapper_flag_takes_arg のテスト
    // ========================================================================

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_u_takes_arg() {
        // sudo -u は値を取るフラグ
        assert!(ShellParser::wrapper_flag_takes_arg("sudo", "-u"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_n_no_arg() {
        // sudo -n は値を取らないフラグ
        assert!(!ShellParser::wrapper_flag_takes_arg("sudo", "-n"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_long_user() {
        // sudo --user は値を取るロングフラグ
        assert!(ShellParser::wrapper_flag_takes_arg("sudo", "--user"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_long_user_inline() {
        // --user=root はインライン値なので追加引数不要（false）
        assert!(!ShellParser::wrapper_flag_takes_arg("sudo", "--user=root"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_uroot_concatenated() {
        // -uroot は値が連結されているので追加引数不要（false）
        assert!(!ShellParser::wrapper_flag_takes_arg("sudo", "-uroot"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_nu_cluster_takes_arg() {
        // -nu の cluster: 末尾 `u` が値取得フラグ、`n` は値取らない
        // → 次トークン (`root`) を値として消費すべき（true）
        assert!(ShellParser::wrapper_flag_takes_arg("sudo", "-nu"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_path_prefixed_sudo() {
        // パス付きラッパーでも sudo の値付きオプションとして扱う
        assert!(ShellParser::wrapper_flag_takes_arg("/usr/bin/sudo", "-u"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_timeout_vs_cluster_takes_arg() {
        // timeout -vs の cluster: 末尾 `s` が値取得フラグ
        // → 次トークン (`TERM`) を値として消費すべき（true）
        assert!(ShellParser::wrapper_flag_takes_arg("timeout", "-vs"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_nv_cluster_no_value_flag() {
        // -nv の cluster: 両方とも値取らないフラグ → 追加トークン不要（false）
        assert!(!ShellParser::wrapper_flag_takes_arg("sudo", "-nv"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_sudo_unroot_inline_value() {
        // -unroot の cluster: 先頭 `u` が値取得フラグで以降 `nroot` が inline 値
        // → 追加トークン不要（false）
        assert!(!ShellParser::wrapper_flag_takes_arg("sudo", "-unroot"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_timeout_signal() {
        // timeout --signal は値を取る
        assert!(ShellParser::wrapper_flag_takes_arg("timeout", "--signal"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_timeout_kill_after() {
        // timeout --kill-after は値を取る
        assert!(ShellParser::wrapper_flag_takes_arg(
            "timeout",
            "--kill-after"
        ));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_env_u() {
        // env -u は値を取る
        assert!(ShellParser::wrapper_flag_takes_arg("env", "-u"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_doas_u() {
        // doas -u は値を取る
        assert!(ShellParser::wrapper_flag_takes_arg("doas", "-u"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_nice_n() {
        // nice -n は値を取る
        assert!(ShellParser::wrapper_flag_takes_arg("nice", "-n"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_unknown_wrapper() {
        // 未知のラッパーはフラグを返さない
        assert!(!ShellParser::wrapper_flag_takes_arg("unknown", "-u"));
        assert!(!ShellParser::wrapper_flag_takes_arg("unknown", "--flag"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_double_dash() {
        // -- はフラグとして扱わない
        assert!(!ShellParser::wrapper_flag_takes_arg("sudo", "--"));
    }

    #[test]
    fn test_wrapper_flag_takes_arg_single_dash() {
        // - はフラグとして扱わない
        assert!(!ShellParser::wrapper_flag_takes_arg("sudo", "-"));
    }

    // ========================================================================
    // find_wrapped_command_index のテスト
    // ========================================================================

    #[test]
    fn test_find_wrapped_command_index_sudo_rm() {
        // sudo rm -rf / → コマンド位置は 0 ("rm")
        let args: Vec<String> = vec!["rm", "-rf", "/"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("sudo", &args),
            Some(0)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_sudo_u_root_rm() {
        // sudo -u root rm -rf / → コマンド位置は 2 ("rm")
        let args: Vec<String> = vec!["-u", "root", "rm", "-rf", "/"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("sudo", &args),
            Some(2)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_sudo_env_assignment_rm() {
        // sudo VAR=value rm -rf / → sudo が受け取る環境変数代入をスキップ
        let args: Vec<String> = vec!["VAR=value", "rm", "-rf", "/"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("sudo", &args),
            Some(1)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_sudo_double_dash() {
        // sudo -- rm → -- 後の位置
        let args: Vec<String> = vec!["--", "rm"].into_iter().map(String::from).collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("sudo", &args),
            Some(1)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_env_assignment() {
        // env VAR=value rm → 環境変数代入をスキップ
        let args: Vec<String> = vec!["VAR=value", "rm"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("env", &args),
            Some(1)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_timeout_numeric() {
        // timeout 10 rm → 制限時間トークンをスキップ
        let args: Vec<String> = vec!["10", "rm"].into_iter().map(String::from).collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("timeout", &args),
            Some(1)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_timeout_with_suffix() {
        // timeout 30s rm → サフィックス付き制限時間
        let args: Vec<String> = vec!["30s", "rm"].into_iter().map(String::from).collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("timeout", &args),
            Some(1)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_timeout_signal_and_duration() {
        // timeout --signal TERM 10 rm → フラグ+制限時間をスキップ
        let args: Vec<String> = vec!["--signal", "TERM", "10", "rm"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            ShellParser::find_wrapped_command_index("timeout", &args),
            Some(3)
        );
    }

    #[test]
    fn test_find_wrapped_command_index_empty_args() {
        // 引数がない場合 → None
        let args: Vec<String> = vec![];
        assert_eq!(ShellParser::find_wrapped_command_index("sudo", &args), None);
    }

    // ========================================================================
    // is_timeout_duration_token のテスト
    // ========================================================================

    #[test]
    fn test_is_timeout_duration_token_integer() {
        // 整数の制限時間
        assert!(ShellParser::is_timeout_duration_token("10"));
    }

    #[test]
    fn test_is_timeout_duration_token_float() {
        // 小数の制限時間
        assert!(ShellParser::is_timeout_duration_token("0.5"));
    }

    #[test]
    fn test_is_timeout_duration_token_suffix_s() {
        // 秒サフィックス
        assert!(ShellParser::is_timeout_duration_token("30s"));
    }

    #[test]
    fn test_is_timeout_duration_token_suffix_m() {
        // 分サフィックス
        assert!(ShellParser::is_timeout_duration_token("5m"));
    }

    #[test]
    fn test_is_timeout_duration_token_suffix_h() {
        // 時間サフィックス
        assert!(ShellParser::is_timeout_duration_token("2h"));
    }

    #[test]
    fn test_is_timeout_duration_token_suffix_d() {
        // 日サフィックス
        assert!(ShellParser::is_timeout_duration_token("1d"));
    }

    #[test]
    fn test_is_timeout_duration_token_empty() {
        // 空文字列は無効
        assert!(!ShellParser::is_timeout_duration_token(""));
    }

    #[test]
    fn test_is_timeout_duration_token_alphabetic() {
        // アルファベット文字列は無効
        assert!(!ShellParser::is_timeout_duration_token("abc"));
    }

    #[test]
    fn test_is_timeout_duration_token_negative() {
        // 負数は無効
        assert!(!ShellParser::is_timeout_duration_token("-1"));
    }

    #[test]
    fn test_is_timeout_duration_token_suffix_only() {
        // サフィックスのみは無効
        assert!(!ShellParser::is_timeout_duration_token("s"));
    }

    // ========================================================================
    // is_env_assignment_token のテスト
    // ========================================================================

    #[test]
    fn test_is_env_assignment_token_valid() {
        // 正常な環境変数代入
        assert!(ShellParser::is_env_assignment_token("VAR=value"));
    }

    #[test]
    fn test_is_env_assignment_token_underscore_prefix() {
        // アンダースコアで始まる変数名
        assert!(ShellParser::is_env_assignment_token("_VAR=value"));
    }

    #[test]
    fn test_is_env_assignment_token_alphanumeric() {
        // 英数字の変数名
        assert!(ShellParser::is_env_assignment_token("A123=x"));
    }

    #[test]
    fn test_is_env_assignment_token_empty_name() {
        // 空の変数名は無効
        assert!(!ShellParser::is_env_assignment_token("=value"));
    }

    #[test]
    fn test_is_env_assignment_token_digit_start() {
        // 数字で始まる変数名は無効
        assert!(!ShellParser::is_env_assignment_token("123=value"));
    }

    #[test]
    fn test_is_env_assignment_token_no_equals() {
        // = がない場合は無効
        assert!(!ShellParser::is_env_assignment_token("rm"));
    }

    // ========================================================================
    // unwrap_subshell のテスト
    // ========================================================================

    #[test]
    fn test_unwrap_subshell_wrapped_command() {
        // 単純なサブシェル
        assert_eq!(ShellParser::unwrap_subshell("(ls -la)"), Some("ls -la"));
    }

    #[test]
    fn test_unwrap_subshell_plain_command() {
        // サブシェルでない場合
        assert_eq!(ShellParser::unwrap_subshell("ls -la"), None);
    }

    #[test]
    fn test_unwrap_subshell_multiple_groups() {
        // 複数の括弧グループがある場合
        assert_eq!(ShellParser::unwrap_subshell("(a) && (b)"), None);
    }

    #[test]
    fn test_unwrap_subshell_unclosed() {
        // 閉じられていない括弧
        assert_eq!(ShellParser::unwrap_subshell("(unclosed"), None);
    }

    #[test]
    fn test_unwrap_subshell_quoted_paren() {
        // クォート内の括弧は無視される
        let result = ShellParser::unwrap_subshell("(ls 'a)' -la)");
        assert!(result.is_some());
    }

    // ========================================================================
    // extract_nested_command_fragments のテスト
    // ========================================================================

    #[test]
    fn test_extract_nested_fragments_dollar_paren() {
        // $() 形式のコマンド置換
        let fragments = ShellParser::extract_nested_command_fragments("echo $(rm -rf /)");
        assert_eq!(fragments, vec!["rm -rf /"]);
    }

    #[test]
    fn test_extract_nested_fragments_backtick() {
        // バッククォート形式のコマンド置換
        let fragments = ShellParser::extract_nested_command_fragments("echo `rm -rf /`");
        assert_eq!(fragments, vec!["rm -rf /"]);
    }

    #[test]
    fn test_extract_nested_fragments_no_substitution() {
        // コマンド置換なし
        let fragments = ShellParser::extract_nested_command_fragments("echo hello");
        assert!(fragments.is_empty());
    }

    #[test]
    fn test_extract_nested_fragments_empty_substitution() {
        // 空のコマンド置換
        let fragments = ShellParser::extract_nested_command_fragments("echo $()");
        assert!(fragments.is_empty());
    }

    // ========================================================================
    // process_wrapper_args の統合テスト（extract_commands 経由）
    // ========================================================================

    #[test]
    fn test_process_wrapper_sudo_rm() {
        // sudo rm -rf / → "sudo" と "rm" の両方を含む
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo rm -rf /");
        assert!(commands.contains(&"sudo".to_string()));
        assert!(commands.contains(&"rm".to_string()));
    }

    #[test]
    fn test_process_wrapper_escaped_sudo_rm() {
        // `s\udo` は quote removal 後に `sudo` なので、内側の rm も抽出する。
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(r"s\udo rm -rf /");
        assert!(
            commands.contains(&"rm".to_string()),
            "escaped sudo wrapper should expose wrapped rm; got {:?}",
            commands
        );
    }

    #[test]
    fn test_process_wrapper_sudo_u_root_bash_c() {
        // sudo -u root bash -c 'rm -rf /' → "rm" を含む（ネストラッパー + shell -c）
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("sudo -u root bash -c 'rm -rf /'");
        assert!(
            commands.contains(&"rm".to_string()),
            "commands should contain 'rm', got: {:?}",
            commands
        );
    }

    #[test]
    fn test_process_wrapper_timeout_signal_rm() {
        // timeout --signal TERM 10 rm -f /tmp/file → "rm" を含む
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("timeout --signal TERM 10 rm -f /tmp/file");
        assert!(
            commands.contains(&"rm".to_string()),
            "commands should contain 'rm', got: {:?}",
            commands
        );
    }

    #[test]
    fn test_process_wrapper_env_command_rm() {
        // env VAR=value command rm -f → "rm" を含む
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("env VAR=value command rm -f");
        assert!(
            commands.contains(&"rm".to_string()),
            "commands should contain 'rm', got: {:?}",
            commands
        );
    }

    // === エッジケーステスト ===

    #[test]
    fn test_extract_commands_empty_string() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("");
        assert!(
            commands.is_empty(),
            "空文字列からはコマンドが抽出されないべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_commands_only_whitespace_and_tabs() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("   \t  ");
        assert!(
            commands.is_empty(),
            "空白のみからはコマンドが抽出されないべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_commands_deeply_nested_subshells() {
        // 3層のネストされたサブシェル
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("bash -c 'sh -c \"bash -c rm\"'");
        assert!(
            commands.contains(&"rm".to_string()),
            "深いネストでも rm を検出すべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_commands_null_byte_in_command() {
        // NULL バイトを含むコマンド（パーサーがクラッシュしないこと）
        let mut parser = ShellParser::new();
        let _commands = parser.extract_commands("echo hello\0world");
        // パニックしなければ成功
    }

    #[test]
    fn test_extract_commands_very_long_pipeline() {
        // 長いパイプライン
        let mut parser = ShellParser::new();
        let cmd = (0..50)
            .map(|i| format!("cmd{}", i))
            .collect::<Vec<_>>()
            .join(" | ");
        let commands = parser.extract_commands(&cmd);
        assert!(
            !commands.is_empty(),
            "長いパイプラインからもコマンドを抽出すべき"
        );
    }

    #[test]
    fn test_extract_commands_mixed_quote_styles() {
        // シングルクォートとダブルクォートの混在
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands(r#"echo "hello 'world'" && rm -f test"#);
        assert!(
            commands.contains(&"rm".to_string()),
            "混合クォートでも rm を検出すべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_commands_consecutive_semicolons() {
        // 連続するセミコロン
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo a ;; echo b ; rm file");
        assert!(
            commands.contains(&"rm".to_string()),
            "連続セミコロンでも rm を検出すべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_commands_heredoc() {
        // ヒアドキュメント内のコマンドは実行されない
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("cat <<EOF\nrm -rf /\nEOF");
        // ヒアドキュメント内の rm は実際のコマンドではない
        // cat のみが抽出されるのが理想
        assert!(
            commands.contains(&"cat".to_string()),
            "ヒアドキュメントで cat を検出すべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_commands_comment_ignored() {
        // コメント後のコマンドは無視されるべき
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("echo hello # rm -rf /");
        assert!(
            !commands.contains(&"rm".to_string()),
            "コメント内の rm は検出すべきでない: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_commands_backgrounded_command() {
        // バックグラウンド実行コマンド
        let mut parser = ShellParser::new();
        let commands = parser.extract_commands("rm -f file &");
        assert!(
            commands.contains(&"rm".to_string()),
            "バックグラウンド実行でも rm を検出すべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_command_strings_empty() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_command_strings("");
        assert!(
            commands.is_empty(),
            "空文字列からはコマンド文字列が抽出されないべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_extract_command_strings_with_args() {
        let mut parser = ShellParser::new();
        let commands = parser.extract_command_strings("npm install lodash");
        assert!(
            commands
                .iter()
                .any(|c| c.contains("npm") && c.contains("install")),
            "npm install を含むコマンド文字列が抽出されるべき: {:?}",
            commands
        );
    }

    // --- フォールバックパーサー: ラッパーコマンド展開のテスト ---

    #[test]
    fn test_fallback_wrapper_no_duplicate_command() {
        // ラッパー展開でコマンドが二重追加されないことを確認
        let parser = ShellParser::new();
        let commands = parser.extract_commands_from_segment_fallback("sudo rm -rf /tmp");
        let rm_count = commands.iter().filter(|c| *c == "rm").count();
        assert_eq!(
            rm_count, 1,
            "rm は1回だけ抽出されるべき（二重追加バグの回帰テスト）: {:?}",
            commands
        );
    }

    #[test]
    fn test_fallback_nested_wrappers_no_duplicate() {
        // 二重ラッパーでもコマンドが重複しない
        let parser = ShellParser::new();
        let commands = parser.extract_commands_from_segment_fallback("timeout 10 sudo rm -rf /tmp");
        let rm_count = commands.iter().filter(|c| *c == "rm").count();
        assert_eq!(rm_count, 1, "二重ラッパーでも rm は1回だけ: {:?}", commands);
    }

    #[test]
    fn test_fallback_wrapper_shell_c_extracts_inner_command() {
        // wrapper の実行対象が shell -c の場合、内側の危険コマンドまで抽出する
        let parser = ShellParser::new();
        let commands =
            parser.extract_commands_from_segment_fallback("sudo -u root bash -c 'rm -rf /tmp'");
        assert!(
            commands.contains(&"rm".to_string()),
            "sudo 経由の bash -c から rm が抽出されるべき: {:?}",
            commands
        );
        let rm_count = commands.iter().filter(|c| *c == "rm").count();
        assert_eq!(rm_count, 1, "rm は1回だけ: {:?}", commands);
    }

    #[test]
    fn test_fallback_wrapper_command_only_no_args() {
        // ラッパーの直後にコマンドだけで引数なし（境界ケース）
        let parser = ShellParser::new();
        let commands = parser.extract_commands_from_segment_fallback("sudo rm");
        assert!(
            commands.contains(&"rm".to_string()),
            "sudo rm から rm が抽出されるべき: {:?}",
            commands
        );
        let rm_count = commands.iter().filter(|c| *c == "rm").count();
        assert_eq!(rm_count, 1, "rm は1回だけ: {:?}", commands);
    }

    #[test]
    fn test_fallback_env_wrapper_with_assignment() {
        // env VAR=x を経由したコマンド抽出
        let parser = ShellParser::new();
        let commands =
            parser.extract_commands_from_segment_fallback("env HOME=/tmp rm -rf /var/data");
        assert!(
            commands.contains(&"rm".to_string()),
            "env ラッパー経由で rm が抽出されるべき: {:?}",
            commands
        );
    }

    #[test]
    fn test_fallback_wrapper_with_remaining_args() {
        // ラッパー展開後の残り引数が正しく処理される
        let parser = ShellParser::new();
        let commands = parser.extract_commands_from_segment_fallback("sudo -u root rm -rf /var");
        assert!(
            commands.contains(&"rm".to_string()),
            "sudo -u root 経由で rm が抽出されるべき: {:?}",
            commands
        );
        let rm_count = commands.iter().filter(|c| *c == "rm").count();
        assert_eq!(rm_count, 1, "rm は1回だけ: {:?}", commands);
    }

    // === parse_shell_tokens の未閉じクォートエッジケース ===

    #[test]
    fn test_parse_shell_tokens_unclosed_single_quote_no_panic() {
        // 未閉じシングルクォートでパニックしないことを確認
        let tokens = parse_shell_tokens("echo 'hello world");
        assert!(!tokens.is_empty(), "未閉じクォートでもトークンが返るべき");
        assert_eq!(tokens[0], "echo");
    }

    #[test]
    fn test_parse_shell_tokens_unclosed_double_quote_no_panic() {
        // 未閉じダブルクォートでパニックしないことを確認
        let tokens = parse_shell_tokens("rm \"unterminated arg");
        assert_eq!(tokens[0], "rm");
    }

    // === split_by_logical_ops のエッジケース ===

    #[test]
    fn test_split_by_logical_ops_preserves_quoted_operators() {
        // クォート内の && はコマンド区切りとして扱わない
        let result = ShellParser::split_by_logical_ops(r#"echo "a && b" && rm file"#);
        assert_eq!(result.len(), 2, "クォート内の && は分割すべきでない");
        assert_eq!(result[1].trim(), "rm file");
    }

    #[test]
    fn test_split_by_logical_ops_single_pipe_not_split() {
        // 単一の | はパイプであり論理演算子ではないため分割しない
        let result = ShellParser::split_by_logical_ops("cat file | grep error || echo fail");
        assert_eq!(result.len(), 2, "|| で分割されるべき");
        assert!(result[0].contains("cat file | grep error"));
    }

    // === split_respecting_quotes: クォート内の区切り文字を保護 ===

    #[test]
    fn test_split_respecting_quotes_semicolon_in_single_quote() {
        // シングルクォート内のセミコロンは区切りとして扱わない
        let result = ShellParser::split_respecting_quotes("printf 'hello; world'", ';');
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "printf 'hello; world'");
    }

    #[test]
    fn test_split_respecting_quotes_pipe_in_double_quote() {
        // ダブルクォート内のパイプは区切りとして扱わない
        let result = ShellParser::split_respecting_quotes(r#"echo "safe | text""#, '|');
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], r#"echo "safe | text""#);
    }

    #[test]
    fn test_split_respecting_quotes_separator_outside_quotes() {
        // クォート外の区切り文字は通常通り分割する
        let result = ShellParser::split_respecting_quotes("a; b; c", ';');
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_split_respecting_quotes_paren_protection() {
        // 括弧内のセミコロンは区切らない（サブシェル内のコマンド連鎖を維持）
        let result = ShellParser::split_respecting_quotes("(a; b); c", ';');
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "(a; b)");
        assert_eq!(result[1], "c");
    }

    #[test]
    fn test_extract_commands_does_not_split_inside_single_quote_for_rm() {
        // 偽陽性回帰テスト: シングルクォート内に rm 文字列があっても誤ブロックしない。
        // AST/フォールバックの両方で printf が最初のコマンドとして検出され、rm は漏れない。
        let mut parser = ShellParser::new();
        let cmds = parser.extract_commands("printf 'hello; rm -rf /'");
        assert!(
            cmds.iter().any(|c| c == "printf"),
            "printf should be detected, got: {:?}",
            cmds
        );
        assert!(
            !cmds.iter().any(|c| c == "rm"),
            "rm should NOT leak from quoted string, got: {:?}",
            cmds
        );
    }

    #[test]
    fn test_extract_commands_does_not_split_inside_double_quote_for_rm() {
        // 偽陽性回帰テスト: ダブルクォート内のパイプ + rm を誤検出しない。
        let mut parser = ShellParser::new();
        let cmds = parser.extract_commands(r#"echo "safe | rm -rf /""#);
        assert!(
            cmds.iter().any(|c| c == "echo"),
            "echo should be detected, got: {:?}",
            cmds
        );
        assert!(
            !cmds.iter().any(|c| c == "rm"),
            "rm should NOT leak from quoted string, got: {:?}",
            cmds
        );
    }
}
