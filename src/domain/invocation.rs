//! シェルコマンド中のプログラム呼び出しの中間表現（IR）。
//!
//! [`crate::domain::parser::ShellParser::extract_invocations`] がコマンド文字列を解析し、
//! 実行されるプログラム呼び出しを 1 つずつ [`Invocation`] として返す。各語には
//! 「実行時の値が静的に決まるか」と「実行時に何個の引数になるか」を持たせる。
//!
//! command hooks（外部の判定器にプログラム呼び出しを渡す機能）がこの IR を使う。
//! 既存の名前抽出（`extract_commands`）とコマンド文字列抽出（`extract_command_strings`）は
//! まだ独自の走査を持つが、最終的にはこの IR から導出する（経路ごとに解析が食い違って
//! 検出漏れが生じる構造を無くすため）。移行中は、IR が既存の名前抽出の見つける
//! コマンドを取りこぼさないことをテストで保証する。

use super::parser::{ShellParser, command_key};

/// 1 つの語が実行時に何個の引数になるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    /// ちょうど 1 個の引数になる。
    One,
    /// 0 個以上の引数に展開され得る（非引用の展開・グロブ・ブレース展開、
    /// xargs が実行時に足す引数など）。
    ZeroOrMore,
}

impl Cardinality {
    /// 判定器へ渡す JSON での表記。
    pub fn as_str(self) -> &'static str {
        match self {
            Cardinality::One => "one",
            Cardinality::ZeroOrMore => "zero_or_more",
        }
    }
}

/// 呼び出しの解析の確度。
///
/// 値の大小は確度の低さを表す（`Complete` < `Uncertain` < `Speculative`）。
/// 複数の理由が重なったときは [`Analysis::max`] で低い方の確度に寄せる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Analysis {
    /// 構文として確定した呼び出し。
    Complete,
    /// 実在する呼び出しの候補だが、語の値の判定が不正確かもしれない
    /// （非静的な文字列の再評価から見つけた、構文エラーを含む解析から見つけた等）。
    Uncertain,
    /// 実在しない可能性がある過大近似（危険コマンド検出のための再解析が作る候補）。
    /// 名前の一覧には含めるが、外部の判定器には渡さない。
    Speculative,
}

impl Analysis {
    /// 判定器へ渡す JSON での表記。`Speculative` は判定器に渡さない前提だが、
    /// 表記が必要になった場合に備えて定義しておく。
    pub fn as_str(self) -> &'static str {
        match self {
            Analysis::Complete => "complete",
            Analysis::Uncertain => "uncertain",
            Analysis::Speculative => "speculative",
        }
    }
}

/// 呼び出しを構成する 1 つの語。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellWord {
    /// 書かれたままの語（クォートを保持）。実行時に足される引数を表す合成語では空文字列。
    pub raw: String,
    /// クォート除去後の語。展開は `$HOME` のように文字どおり残す
    /// （既存の名前抽出が使う形。`normalize_shell_word(raw)` と同じ）。
    pub text: String,
    /// 実行時の値が静的に確定していれば `Some`。
    pub value: Option<String>,
    /// 実行時に何個の引数になるか。
    pub cardinality: Cardinality,
}

impl ShellWord {
    /// 実行時の値が静的に確定しているか。
    pub fn is_static(&self) -> bool {
        self.value.is_some()
    }
}

/// 呼び出しの標準入力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdinSource {
    /// リダイレクトもパイプも無い（エージェントのシェルから継承する）。
    Inherited,
    /// ヒアドキュメント / here-string。本文が静的に確定していれば `Some`。
    Literal { value: Option<String> },
    /// パイプや `< file` など、中身の分からない入力が流れ込む。
    Other,
}

/// 実行されるプログラム呼び出し 1 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// argv。`words[0]` がプログラム名。
    pub words: Vec<ShellWord>,
    /// 標準入力。
    pub stdin: StdinSource,
    /// 解析の確度。
    pub analysis: Analysis,
}

impl Invocation {
    /// プログラム名を判定用のキー（basename・実行拡張子除去・小文字化）に正規化して返す。
    ///
    /// プログラム名が静的に確定しない（`$CMD args` のように展開で決まる）場合は `None`。
    /// その場合どのプログラムが起動するかは実行時まで分からない。
    pub fn command_key(&self) -> Option<String> {
        let name = self.words.first()?.value.as_deref()?;
        let key = command_key(name);
        (!key.is_empty()).then_some(key)
    }
}

/// コマンド文字列 1 つを解析した結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandLineAnalysis {
    /// 見つかった呼び出し（おおむねコマンド内の出現順）。
    pub invocations: Vec<Invocation>,
    /// 入力が長すぎる・深すぎるため解析を諦めたか。`true` のとき `invocations` は
    /// 不完全で、呼び出しを取りこぼしている可能性がある。
    pub pathological: bool,
}

// === 語の静的解析 ===
//
// AST 経路（語の境界は tree-sitter のノード）とフォールバック経路（語の境界は文字列の
// 分割）の両方が、書かれたままの語をここへ渡して値を求める。静的判定を 1 か所に
// 置くことで、経路ごとに「どの語を確定値とみなすか」が食い違わないようにする。

/// 書かれたままの語 1 つを [`ShellWord`] にする。
pub(crate) fn shell_word(raw: &str) -> ShellWord {
    let (value, cardinality) = analyze_word(raw);
    ShellWord {
        raw: raw.to_string(),
        text: ShellParser::normalize_shell_word(raw),
        value,
        cardinality,
    }
}

/// 実行時に足される引数（xargs が標準入力から読んだ要素など）を表す合成語。
pub(crate) fn runtime_arguments_word() -> ShellWord {
    ShellWord {
        raw: String::new(),
        text: String::new(),
        value: None,
        cardinality: Cardinality::ZeroOrMore,
    }
}

/// 書かれたままの語 1 つについて、実行時の値が静的に決まればその値と、実行時に
/// 何個の引数になるかを返す。
///
/// 静的とみなすのは、クォート除去だけで値が決まる語（単一引用・`$'...'`・展開を
/// 含まない二重引用・エスケープ）と、二重引用の中の「引数なしの `cat` と
/// ヒアドキュメント 1 つだけ」のコマンド置換（[`recognize_cat_heredoc`]）だけ。
/// 変数展開・コマンド置換・算術展開・プロセス置換・非引用のグロブ文字・ブレース展開・
/// 語頭のチルダを含む語は非静的にする。値が NUL を含む場合も非静的にする
/// （引数に NUL は渡せないため、実行時の値と一致しない）。
pub(crate) fn analyze_word(raw: &str) -> (Option<String>, Cardinality) {
    let chars: Vec<char> = raw.chars().collect();
    let mut scanner = WordScanner {
        chars: &chars,
        pos: 0,
        value: String::new(),
        is_static: true,
        cardinality: Cardinality::One,
    };
    scanner.scan_unquoted();
    let value = (scanner.is_static && !scanner.value.contains('\0')).then_some(scanner.value);
    (value, scanner.cardinality)
}

/// 語を 1 文字ずつ読んで値を組み立てる走査器。
struct WordScanner<'a> {
    chars: &'a [char],
    pos: usize,
    value: String,
    is_static: bool,
    cardinality: Cardinality,
}

impl WordScanner<'_> {
    /// 実行時に値が決まるが、引数の個数は 1 個のまま（二重引用の中の展開など）。
    fn dynamic_one(&mut self) {
        self.is_static = false;
    }

    /// 実行時に値も個数も決まる（非引用の展開は単語分割とパス名展開を受ける）。
    fn dynamic_many(&mut self) {
        self.is_static = false;
        self.cardinality = Cardinality::ZeroOrMore;
    }

    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    /// 引用の外を読む。
    fn scan_unquoted(&mut self) {
        while let Some(c) = self.peek(0) {
            match c {
                '\\' => match self.peek(1) {
                    // 行継続: バックスラッシュと改行は語から消える
                    Some('\n') => self.pos += 2,
                    Some(next) => {
                        self.value.push(next);
                        self.pos += 2;
                    }
                    None => {
                        self.value.push('\\');
                        self.pos += 1;
                    }
                },
                '\'' => {
                    let start = self.pos + 1;
                    match self.chars[start..].iter().position(|&ch| ch == '\'') {
                        Some(offset) => {
                            self.value.extend(&self.chars[start..start + offset]);
                            self.pos = start + offset + 1;
                        }
                        // 閉じない引用は値を確定できない
                        None => {
                            self.dynamic_one();
                            self.pos = self.chars.len();
                        }
                    }
                }
                '"' => {
                    self.pos += 1;
                    self.scan_double_quoted();
                }
                '$' => self.scan_dollar(false),
                '`' => {
                    self.skip_backtick();
                    self.dynamic_many();
                }
                // パス名展開: 一致するファイルがあれば複数の引数に置き換わる
                '*' | '?' | '[' => {
                    self.value.push(c);
                    self.pos += 1;
                    self.dynamic_many();
                }
                '{' => {
                    if is_brace_expansion(self.chars, self.pos) {
                        self.dynamic_many();
                    }
                    self.value.push(c);
                    self.pos += 1;
                }
                // 語頭のチルダはホームディレクトリへ展開される
                '~' if self.pos == 0 => {
                    self.value.push(c);
                    self.pos += 1;
                    self.dynamic_one();
                }
                // プロセス置換 `<(...)` / `>(...)` は /dev/fd/N のようなパスになる
                '<' | '>' if self.peek(1) == Some('(') => {
                    self.pos = find_paren_end(self.chars, self.pos + 1)
                        .map_or(self.chars.len(), |end| end + 1);
                    self.dynamic_one();
                }
                // 引用されない空白は語の中に現れない。現れたのは語の境界を取り違えた
                // 証拠（フォールバックの分割が崩れた等）なので、値を確定させない。
                ' ' | '\t' | '\n' | '\r' => {
                    self.value.push(c);
                    self.pos += 1;
                    self.dynamic_one();
                }
                _ => {
                    self.value.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// 二重引用の中を読む（開き `"` の直後から、閉じ `"` の直後まで進める）。
    fn scan_double_quoted(&mut self) {
        while let Some(c) = self.peek(0) {
            match c {
                '"' => {
                    self.pos += 1;
                    return;
                }
                '\\' => match self.peek(1) {
                    Some(next @ ('$' | '`' | '"' | '\\')) => {
                        self.value.push(next);
                        self.pos += 2;
                    }
                    Some('\n') => self.pos += 2,
                    Some(next) => {
                        self.value.push('\\');
                        self.value.push(next);
                        self.pos += 2;
                    }
                    None => {
                        self.value.push('\\');
                        self.pos += 1;
                    }
                },
                '$' => self.scan_dollar(true),
                '`' => {
                    self.skip_backtick();
                    self.dynamic_one();
                }
                _ => {
                    self.value.push(c);
                    self.pos += 1;
                }
            }
        }
        // 閉じない二重引用は値を確定できない
        self.dynamic_one();
    }

    /// `$` から始まる構文を読む。`in_double_quote` は二重引用の中かどうか。
    fn scan_dollar(&mut self, in_double_quote: bool) {
        let expanded = |scanner: &mut Self| {
            if in_double_quote {
                scanner.dynamic_one();
            } else {
                scanner.dynamic_many();
            }
        };
        match self.peek(1) {
            // ANSI-C 引用 `$'...'`（二重引用の中では `$` の後の `'` はただの文字）
            Some('\'') if !in_double_quote => {
                let start = self.pos + 2;
                let mut end = start;
                while end < self.chars.len() && self.chars[end] != '\'' {
                    end += if self.chars[end] == '\\' { 2 } else { 1 };
                }
                if end >= self.chars.len() {
                    self.dynamic_one();
                    self.pos = self.chars.len();
                    return;
                }
                let body: String = self.chars[start..end].iter().collect();
                self.value
                    .push_str(&ShellParser::normalize_shell_word(&format!("$'{body}'")));
                self.pos = end + 1;
            }
            // ロケール翻訳文字列 `$"..."`（翻訳が無ければ二重引用と同じ）
            Some('"') if !in_double_quote => {
                self.pos += 2;
                self.scan_double_quoted();
            }
            Some('(') => {
                if self.peek(2) == Some('(') {
                    // 算術展開 `$((...))` は数値 1 つになる
                    self.pos = find_paren_end(self.chars, self.pos + 1)
                        .map_or(self.chars.len(), |end| end + 1);
                    self.dynamic_one();
                    return;
                }
                if let Some((value, close)) = recognize_cat_heredoc(self.chars, self.pos + 2) {
                    self.pos = close + 1;
                    match (in_double_quote, value) {
                        (true, Some(value)) => self.value.push_str(&value),
                        (true, None) => self.dynamic_one(),
                        // 非引用のコマンド置換は単語分割とパス名展開を受ける
                        (false, _) => self.dynamic_many(),
                    }
                    return;
                }
                self.pos = find_paren_end(self.chars, self.pos + 1)
                    .map_or(self.chars.len(), |end| end + 1);
                expanded(self);
            }
            Some('{') => {
                let end = find_brace_end(self.chars, self.pos + 1);
                let body: String = self.chars
                    [self.pos + 2..end.unwrap_or(self.chars.len()).max(self.pos + 2)]
                    .iter()
                    .collect();
                self.pos = end.map_or(self.chars.len(), |end| end + 1);
                // `"${array[@]}"` は二重引用の中でも要素ごとの引数になる
                if body.contains("[@]") {
                    self.dynamic_many();
                } else {
                    expanded(self);
                }
            }
            Some(next) if next == '_' || next.is_ascii_alphabetic() => {
                self.pos += 1;
                while self
                    .peek(0)
                    .is_some_and(|ch| ch == '_' || ch.is_ascii_alphanumeric())
                {
                    self.pos += 1;
                }
                expanded(self);
            }
            // `"$@"` は二重引用の中でも位置パラメータごとの引数になる
            Some('@') => {
                self.pos += 2;
                self.dynamic_many();
            }
            Some(next)
                if next.is_ascii_digit() || matches!(next, '*' | '#' | '?' | '$' | '!' | '-') =>
            {
                self.pos += 2;
                expanded(self);
            }
            // 展開を始めない `$` はただの文字
            _ => {
                self.value.push('$');
                self.pos += 1;
            }
        }
    }

    /// バッククォートのコマンド置換を読み飛ばす（開き `` ` `` の位置から）。
    fn skip_backtick(&mut self) {
        let mut index = self.pos + 1;
        while index < self.chars.len() {
            match self.chars[index] {
                '\\' => index += 2,
                '`' => {
                    self.pos = index + 1;
                    return;
                }
                _ => index += 1,
            }
        }
        self.pos = self.chars.len();
    }
}

/// `chars[open] == '('` に対応する `)` の位置を返す。
///
/// 引用とエスケープの中の括弧は数えない。コマンド置換の中の `case` のパターン
/// （`a)`）のように括弧の対応が崩れる構文では位置を誤り得るが、呼び出し側は
/// その語を非静的として扱うため、値を誤って確定させることはない。
fn find_paren_end(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = open;
    let mut in_single = false;
    let mut in_double = false;
    while index < chars.len() {
        let c = chars[index];
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            index += 1;
            continue;
        }
        match c {
            '\\' => {
                index += 2;
                continue;
            }
            '\'' if !in_double => in_single = true,
            '"' => in_double = !in_double,
            '(' if !in_double => depth += 1,
            ')' if !in_double => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// `chars[open] == '{'` に対応する `}` の位置を返す（引用とエスケープを考慮）。
fn find_brace_end(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = open;
    let mut in_single = false;
    let mut in_double = false;
    while index < chars.len() {
        let c = chars[index];
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            index += 1;
            continue;
        }
        match c {
            '\\' => {
                index += 2;
                continue;
            }
            '\'' if !in_double => in_single = true,
            '"' => in_double = !in_double,
            '{' if !in_double => depth += 1,
            '}' if !in_double => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// `chars[open] == '{'` から始まる部分がブレース展開（`{a,b}` / `{1..3}`）か。
///
/// 最上位の引用されていないカンマか `..` を含み、対応する `}` で閉じる場合だけ
/// ブレース展開になる。`{}`（find の置換文字列）や `{ cmd; }` は展開されない。
fn is_brace_expansion(chars: &[char], open: usize) -> bool {
    let Some(close) = find_brace_end(chars, open) else {
        return false;
    };
    let mut depth = 0usize;
    let mut index = open + 1;
    let mut in_single = false;
    let mut in_double = false;
    while index < close {
        let c = chars[index];
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            index += 1;
            continue;
        }
        match c {
            '\\' => {
                index += 2;
                continue;
            }
            '\'' if !in_double => in_single = true,
            '"' => in_double = !in_double,
            '{' if !in_double => depth += 1,
            '}' if !in_double => depth = depth.saturating_sub(1),
            ',' if !in_double && depth == 0 => return true,
            '.' if !in_double && depth == 0 && chars.get(index + 1) == Some(&'.') => return true,
            _ => {}
        }
        index += 1;
    }
    false
}

/// `chars[index..]` の空白（スペース・タブ）を読み飛ばした位置を返す。
fn skip_blanks(chars: &[char], mut index: usize) -> usize {
    while chars.get(index).is_some_and(|c| matches!(c, ' ' | '\t')) {
        index += 1;
    }
    index
}

/// 語の終わり（引用の外の空白・改行・`)`・`;`・`|`・`&`・`<`・`>`）までを読む。
fn scan_word_end(chars: &[char], start: usize) -> usize {
    let mut index = start;
    let mut in_single = false;
    let mut in_double = false;
    while index < chars.len() {
        let c = chars[index];
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            index += 1;
            continue;
        }
        if in_double {
            match c {
                '\\' => index += 2,
                '"' => {
                    in_double = false;
                    index += 1;
                }
                _ => index += 1,
            }
            continue;
        }
        match c {
            '\\' => index += 2,
            '\'' => {
                in_single = true;
                index += 1;
            }
            '"' => {
                in_double = true;
                index += 1;
            }
            ' ' | '\t' | '\n' | ')' | ';' | '|' | '&' | '<' | '>' => break,
            _ => index += 1,
        }
    }
    index.min(chars.len())
}

/// コマンド置換の中身が「引数なしの `cat` とヒアドキュメント（または here-string）
/// 1 つだけ」なら、その出力（静的に決まれば値）と閉じ `)` の位置を返す。
///
/// `start` は `$(` の直後の位置。`--json "$(cat <<'EOF' ... EOF)"` の形で本文を
/// コマンドの引数へ渡すのはエージェントがよく使う書き方で、本文そのものが判定器の
/// 検査対象になる。これ以外の形（パイプ・追加の引数・別のリダイレクト）は
/// 認識しない（呼び出し側はコマンド置換を非静的として扱う）。
///
/// 値の規則:
/// - 区切りをクォートしたヒアドキュメント（`<<'EOF'` / `<<"EOF"` / `<<\EOF`）は本文をそのまま
/// - クォートしない区切りは、本文に展開（`$` / `` ` ``）が無い場合だけ確定する
///   （[`expand_unquoted_heredoc_body`]）
/// - `<<-` は本文と区切り行の先頭のタブを除く
/// - here-string（`cat <<< 'text'`）は語の値。here-string が足す末尾の改行は
///   コマンド置換が除く
/// - コマンド置換は出力末尾の改行をすべて除く
pub(crate) fn recognize_cat_heredoc(
    chars: &[char],
    start: usize,
) -> Option<(Option<String>, usize)> {
    let mut index = skip_blanks(chars, start);
    let name: String = chars.get(index..index + 3)?.iter().collect();
    if name != "cat"
        || !chars
            .get(index + 3)
            .is_some_and(|c| matches!(c, ' ' | '\t'))
    {
        return None;
    }
    index = skip_blanks(chars, index + 3);
    // `cat - <<EOF` も標準入力をそのまま出力する
    if chars.get(index) == Some(&'-')
        && chars
            .get(index + 1)
            .is_some_and(|c| matches!(c, ' ' | '\t'))
    {
        index = skip_blanks(chars, index + 1);
    }
    if chars.get(index..index + 2)? != ['<', '<'] {
        return None;
    }
    index += 2;

    if chars.get(index) == Some(&'<') {
        // here-string: `cat <<< WORD`
        let word_start = skip_blanks(chars, index + 1);
        let word_end = scan_word_end(chars, word_start);
        if word_end == word_start {
            return None;
        }
        let close = skip_whitespace(chars, word_end);
        if chars.get(close) != Some(&')') {
            return None;
        }
        let raw: String = chars[word_start..word_end].iter().collect();
        let (value, cardinality) = analyze_word(&raw);
        let value = value
            .filter(|_| cardinality == Cardinality::One)
            .map(|value| value.trim_end_matches('\n').to_string());
        return Some((value, close));
    }

    let strip_tabs = chars.get(index) == Some(&'-');
    if strip_tabs {
        index += 1;
    }
    index = skip_blanks(chars, index);
    let delimiter_end = scan_word_end(chars, index);
    if delimiter_end == index {
        return None;
    }
    let delimiter_raw: String = chars[index..delimiter_end].iter().collect();
    let quoted = delimiter_raw.contains(['\'', '"', '\\']);
    let delimiter = ShellParser::normalize_shell_word(&delimiter_raw);
    if delimiter.is_empty() {
        return None;
    }
    index = skip_blanks(chars, delimiter_end);
    if chars.get(index) != Some(&'\n') {
        return None;
    }
    index += 1;

    let mut body = String::new();
    loop {
        let line_end = chars[index..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(chars.len(), |offset| index + offset);
        let mut line = &chars[index..line_end];
        if strip_tabs {
            let tabs = line.iter().take_while(|&&c| c == '\t').count();
            line = &line[tabs..];
        }
        if line.iter().copied().eq(delimiter.chars()) {
            index = line_end;
            break;
        }
        if line_end >= chars.len() {
            // 区切り行が無いまま終わった
            return None;
        }
        body.extend(line);
        body.push('\n');
        index = line_end + 1;
    }

    let close = skip_whitespace(chars, index);
    if chars.get(close) != Some(&')') {
        return None;
    }
    let value = if quoted {
        Some(body)
    } else {
        expand_unquoted_heredoc_body(&body)
    };
    Some((
        value.map(|value| value.trim_end_matches('\n').to_string()),
        close,
    ))
}

/// 空白と改行を読み飛ばした位置を返す。
fn skip_whitespace(chars: &[char], mut index: usize) -> usize {
    while chars
        .get(index)
        .is_some_and(|c| matches!(c, ' ' | '\t' | '\n'))
    {
        index += 1;
    }
    index
}

/// クォートしない区切りのヒアドキュメント本文を、展開が無い場合だけ確定させる。
///
/// 本文ではバックスラッシュが `\`・`$`・`` ` `` と改行だけを引用する（`\"` は
/// そのまま残る）。引用されていない `$`（展開の開始）や `` ` `` があれば、
/// 値は実行時まで決まらないので `None` を返す。
pub(crate) fn expand_unquoted_heredoc_body(body: &str) -> Option<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut value = String::with_capacity(body.len());
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '\\' => match chars.get(index + 1) {
                Some('\n') => index += 2,
                Some(&next @ ('$' | '`' | '\\')) => {
                    value.push(next);
                    index += 2;
                }
                Some(&next) => {
                    value.push('\\');
                    value.push(next);
                    index += 2;
                }
                None => {
                    value.push('\\');
                    index += 1;
                }
            },
            '$' => match chars.get(index + 1) {
                Some(&next)
                    if next == '_'
                        || next.is_ascii_alphanumeric()
                        || matches!(next, '{' | '(' | '@' | '*' | '#' | '?' | '$' | '!' | '-') =>
                {
                    return None;
                }
                _ => {
                    value.push('$');
                    index += 1;
                }
            },
            '`' => return None,
            c => {
                value.push(c);
                index += 1;
            }
        }
    }
    Some(value)
}

/// `<<-` のヒアドキュメント本文から、各行の先頭のタブを除く。
///
/// 本文を構文木から取り出せる AST 経路だけが使う（フォールバックはヒアドキュメントの
/// 本文を行ごとの別のセグメントとして読むため、本文を取り出さない）。
#[cfg(feature = "ast-parser")]
pub(crate) fn strip_heredoc_tabs(body: &str) -> String {
    body.split('\n')
        .map(|line| line.trim_start_matches('\t'))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// AST 経路では確定、フォールバック経路では候補になる呼び出しの確度。
    fn parsed_analysis() -> Analysis {
        if cfg!(feature = "ast-parser") {
            Analysis::Complete
        } else {
            Analysis::Uncertain
        }
    }

    fn analyze(command: &str) -> CommandLineAnalysis {
        ShellParser::new().extract_invocations(command)
    }

    /// 指定したプログラム名の呼び出しを、判定器に渡すもの（Speculative 以外）だけ返す。
    fn invocations_of(command: &str, name: &str) -> Vec<Invocation> {
        analyze(command)
            .invocations
            .into_iter()
            .filter(|invocation| invocation.analysis != Analysis::Speculative)
            .filter(|invocation| invocation.command_key().as_deref() == Some(name))
            .collect()
    }

    /// 指定したプログラム名の呼び出しが 1 つだけあることを確かめて返す。
    fn only_invocation(command: &str, name: &str) -> Invocation {
        let found = invocations_of(command, name);
        assert_eq!(
            found.len(),
            1,
            "{command:?}: expected exactly one `{name}` invocation, got {found:#?}"
        );
        found.into_iter().next().unwrap()
    }

    fn values(invocation: &Invocation) -> Vec<Option<&str>> {
        invocation
            .words
            .iter()
            .map(|word| word.value.as_deref())
            .collect()
    }

    fn cardinalities(invocation: &Invocation) -> Vec<Cardinality> {
        invocation
            .words
            .iter()
            .map(|word| word.cardinality)
            .collect()
    }

    // --- 語の静的解析 ---

    #[test]
    fn test_analyze_word_static_forms() {
        let cases = [
            ("docs", "docs"),
            ("'a b'", "a b"),
            ("\"a b\"", "a b"),
            ("a\\ b", "a b"),
            ("$'a\\tb'", "a\tb"),
            ("'{\"title\":\"x\"}'", "{\"title\":\"x\"}"),
            ("--json='{}'", "--json={}"),
            ("\"q*\"", "q*"),
            ("{}", "{}"),
            ("a$", "a$"),
            ("\"\\$HOME\"", "$HOME"),
            ("\"a\\nb\"", "a\\nb"),
            ("$\"hello\"", "hello"),
            ("''", ""),
            ("x~y", "x~y"),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                analyze_word(raw),
                (Some(expected.to_string()), Cardinality::One),
                "raw word {raw:?}"
            );
        }
    }

    #[test]
    fn test_analyze_word_dynamic_forms() {
        // (語, 実行時の引数の個数)
        let cases = [
            ("$X", Cardinality::ZeroOrMore),
            ("\"$X\"", Cardinality::One),
            ("${X}", Cardinality::ZeroOrMore),
            ("\"${X:-a}\"", Cardinality::One),
            ("\"${arr[@]}\"", Cardinality::ZeroOrMore),
            ("\"$@\"", Cardinality::ZeroOrMore),
            ("$(date)", Cardinality::ZeroOrMore),
            ("\"$(date)\"", Cardinality::One),
            ("`date`", Cardinality::ZeroOrMore),
            ("\"`date`\"", Cardinality::One),
            ("$((1 + 2))", Cardinality::One),
            ("*.txt", Cardinality::ZeroOrMore),
            ("file?", Cardinality::ZeroOrMore),
            ("[ab]", Cardinality::ZeroOrMore),
            ("a{b,c}d", Cardinality::ZeroOrMore),
            ("{1..3}", Cardinality::ZeroOrMore),
            ("~/x", Cardinality::One),
            ("<(ls)", Cardinality::One),
            ("--json=$X", Cardinality::ZeroOrMore),
            ("'unterminated", Cardinality::One),
            ("\"unterminated", Cardinality::One),
            ("\"nul\\\\\u{0}\"", Cardinality::One),
        ];
        for (raw, cardinality) in cases {
            assert_eq!(analyze_word(raw), (None, cardinality), "raw word {raw:?}");
        }
    }

    #[test]
    fn test_analyze_word_heredoc_in_double_quoted_substitution() {
        let raw = "\"$(cat <<'EOF'\n{\"title\": \"テスト\", \"body\": \"$HOME は展開されない\"}\nEOF\n)\"";
        assert_eq!(
            analyze_word(raw).0.as_deref(),
            Some("{\"title\": \"テスト\", \"body\": \"$HOME は展開されない\"}")
        );

        // 区切りの引用の書き方はどれでもよい
        for delimiter in ["\"EOF\"", "\\EOF", "E'O'F"] {
            let raw = format!("\"$(cat <<{delimiter}\nbody $X\nEOF\n)\"");
            assert_eq!(analyze_word(&raw).0.as_deref(), Some("body $X"), "{raw:?}");
        }

        // 連結: `--json="$(cat <<'EOF' ... EOF)"`
        let raw = "--json=\"$(cat <<'EOF'\n[1, 2]\nEOF\n)\"";
        assert_eq!(analyze_word(raw).0.as_deref(), Some("--json=[1, 2]"));

        // 本文の括弧や引用の対応が崩れていても本文として扱う
        let raw = "\"$(cat <<'EOF'\nit's (unbalanced \"\nEOF\n)\"";
        assert_eq!(analyze_word(raw).0.as_deref(), Some("it's (unbalanced \""));

        // 末尾の改行はすべて除かれ、途中の空行は残る
        let raw = "\"$(cat <<'EOF'\na\n\nb\n\n\nEOF\n)\"";
        assert_eq!(analyze_word(raw).0.as_deref(), Some("a\n\nb"));

        // 空の本文
        let raw = "\"$(cat <<'EOF'\nEOF\n)\"";
        assert_eq!(analyze_word(raw).0.as_deref(), Some(""));

        // `cat -` も同じ
        let raw = "\"$(cat - <<'EOF'\nx\nEOF\n)\"";
        assert_eq!(analyze_word(raw).0.as_deref(), Some("x"));
    }

    #[test]
    fn test_analyze_word_unquoted_heredoc_delimiter() {
        // 展開の無い本文は確定する
        let raw = "\"$(cat <<EOF\nplain text\nEOF\n)\"";
        assert_eq!(analyze_word(raw).0.as_deref(), Some("plain text"));
        // 引用された `$` / `` ` `` / `\` は文字になり、それ以外のバックスラッシュは残る
        let raw = "\"$(cat <<EOF\ncost \\$5 \\`x\\` a\\\\b \\\"q\\\"\nEOF\n)\"";
        assert_eq!(
            analyze_word(raw).0.as_deref(),
            Some("cost $5 `x` a\\b \\\"q\\\"")
        );
        // 行継続は消える
        let raw = "\"$(cat <<EOF\none \\\ntwo\nEOF\n)\"";
        assert_eq!(analyze_word(raw).0.as_deref(), Some("one two"));
        // 展開のある本文は確定しない
        for body in ["$HOME", "${X}", "$(date)", "`date`", "$1"] {
            let raw = format!("\"$(cat <<EOF\nvalue {body}\nEOF\n)\"");
            assert_eq!(analyze_word(&raw), (None, Cardinality::One), "{raw:?}");
        }
        // 展開を始めない `$` はただの文字
        let raw = "\"$(cat <<EOF\nprice: 5$ / $ alone\nEOF\n)\"";
        assert_eq!(analyze_word(raw).0.as_deref(), Some("price: 5$ / $ alone"));
    }

    #[test]
    fn test_analyze_word_dash_heredoc_strips_tabs() {
        let raw = "\"$(cat <<-'EOF'\n\t{\n\t\t\"a\": 1\n\t}\n\tEOF\n)\"";
        assert_eq!(analyze_word(raw).0.as_deref(), Some("{\n\"a\": 1\n}"));
    }

    #[test]
    fn test_analyze_word_herestring_in_substitution() {
        let raw = "\"$(cat <<< '{\"a\": 1}')\"";
        assert_eq!(analyze_word(raw).0.as_deref(), Some("{\"a\": 1}"));
        let raw = "\"$(cat <<< \"$X\")\"";
        assert_eq!(analyze_word(raw), (None, Cardinality::One));
    }

    #[test]
    fn test_analyze_word_unrecognized_substitutions_stay_dynamic() {
        for raw in [
            // 非引用のコマンド置換は単語分割とパス名展開を受ける
            "$(cat <<'EOF'\nx\nEOF\n)",
            // パイプ・追加の引数・別のコマンド
            "\"$(cat <<'EOF' | tr a b\nx\nEOF\n)\"",
            "\"$(cat file <<'EOF'\nx\nEOF\n)\"",
            "\"$(printf '%s' <<'EOF'\nx\nEOF\n)\"",
            "\"$(tac <<'EOF'\nx\nEOF\n)\"",
            // 区切り行が無い
            "\"$(cat <<'EOF'\nx\n)\"",
            // 区切り行の後ろに別のコマンドがある
            "\"$(cat <<'EOF'\nx\nEOF\necho y)\"",
            // 区切り行に空白が付く（bash は区切りとみなさない）
            "\"$(cat <<'EOF'\nx\nEOF \n)\"",
        ] {
            assert_eq!(analyze_word(raw).0, None, "{raw:?}");
        }
    }

    // --- 呼び出しの抽出 ---

    #[test]
    fn test_extract_simple_invocation() {
        let invocation = only_invocation(
            "gws docs documents get --params '{\"documentId\": \"abc\"}'",
            "gws",
        );
        assert_eq!(
            values(&invocation),
            [
                Some("gws"),
                Some("docs"),
                Some("documents"),
                Some("get"),
                Some("--params"),
                Some("{\"documentId\": \"abc\"}"),
            ]
        );
        assert_eq!(invocation.stdin, StdinSource::Inherited);
        assert_eq!(invocation.analysis, parsed_analysis());
    }

    #[test]
    fn test_extract_issue_example_heredoc_json() {
        let command = "gws docs documents create --json \"$(cat <<'EOF'\n{\"title\": \"週報\", \"body\": \"本文 $HOME\"}\nEOF\n)\"";
        let invocation = only_invocation(command, "gws");
        assert_eq!(
            values(&invocation).last().copied().flatten(),
            Some("{\"title\": \"週報\", \"body\": \"本文 $HOME\"}")
        );
        assert!(invocation.words.iter().all(ShellWord::is_static));
        // コマンド置換の中の cat も呼び出しとして見つかる
        assert_eq!(invocations_of(command, "cat").len(), 1);
    }

    #[test]
    fn test_extract_dynamic_arguments() {
        let invocation = only_invocation("gws docs \"$TITLE\" $BODY *.md", "gws");
        assert_eq!(
            values(&invocation),
            [Some("gws"), Some("docs"), None, None, None]
        );
        assert_eq!(
            cardinalities(&invocation),
            [
                Cardinality::One,
                Cardinality::One,
                Cardinality::One,
                Cardinality::ZeroOrMore,
                Cardinality::ZeroOrMore,
            ]
        );
        assert_eq!(invocation.words[2].text, "$TITLE");
    }

    #[test]
    fn test_extract_normalizes_program_name() {
        for command in [
            "/opt/homebrew/bin/gws docs x",
            "g\\ws docs x",
            "'gws' docs x",
            "$'g\\x77s' docs x",
            "GWS docs x",
        ] {
            let invocation = only_invocation(command, "gws");
            assert_eq!(
                values(&invocation)[1..],
                [Some("docs"), Some("x")],
                "{command:?}"
            );
        }
    }

    #[test]
    fn test_extract_dynamic_program_name_has_no_key() {
        let analysis = analyze("$CMD docs x");
        assert!(!analysis.invocations.is_empty());
        assert!(
            analysis
                .invocations
                .iter()
                .all(|invocation| invocation.command_key().is_none())
        );
    }

    #[test]
    fn test_extract_through_wrappers() {
        for command in [
            "sudo -u me gws docs x",
            "timeout 5 gws docs x",
            "env A=1 B=2 gws docs x",
            "nohup nice -n 10 gws docs x",
            "command gws docs x",
            "sudo env A=1 timeout -s TERM 5 gws docs x",
            "A=1 gws docs x",
        ] {
            let invocation = only_invocation(command, "gws");
            assert_eq!(
                values(&invocation),
                [Some("gws"), Some("docs"), Some("x")],
                "{command:?}"
            );
        }
    }

    #[test]
    fn test_extract_sequences_pipelines_and_substitutions() {
        let command = "gws a && gws b | jq . ; echo \"$(gws c)\" || (gws d)";
        let names: Vec<Vec<Option<String>>> = invocations_of(command, "gws")
            .iter()
            .map(|invocation| {
                invocation
                    .words
                    .iter()
                    .map(|word| word.value.clone())
                    .collect()
            })
            .collect();
        for expected in ["a", "b", "c", "d"] {
            assert!(
                names.contains(&vec![Some("gws".to_string()), Some(expected.to_string())]),
                "{command:?}: missing gws {expected} in {names:?}"
            );
        }
    }

    #[test]
    fn test_extract_control_structures() {
        for command in [
            "if true; then gws docs x; fi",
            "for i in 1 2; do gws docs x; done",
            "while false; do gws docs x; done",
            "{ gws docs x; }",
            "time gws docs x",
        ] {
            let invocation = only_invocation(command, "gws");
            assert_eq!(
                values(&invocation),
                [Some("gws"), Some("docs"), Some("x")],
                "{command:?}"
            );
        }
    }

    #[test]
    fn test_extract_reevaluated_shell_strings() {
        // 静的な文字列の再評価は確定
        let invocation = only_invocation("bash -c 'gws docs \"a b\"'", "gws");
        assert_eq!(
            values(&invocation),
            [Some("gws"), Some("docs"), Some("a b")]
        );
        assert_eq!(invocation.analysis, parsed_analysis());

        for command in ["sh -c 'gws docs x'", "eval gws docs x", "eval 'gws docs x'"] {
            let invocation = only_invocation(command, "gws");
            assert_eq!(
                values(&invocation),
                [Some("gws"), Some("docs"), Some("x")],
                "{command:?}"
            );
        }

        // 非静的な文字列の再評価は候補。展開を含む語は値を確定させない
        let invocation = only_invocation("bash -c \"gws docs '$TITLE' plain\"", "gws");
        assert_eq!(invocation.analysis, Analysis::Uncertain);
        assert_eq!(
            values(&invocation),
            [Some("gws"), Some("docs"), None, Some("plain")]
        );
    }

    #[test]
    fn test_extract_xargs_and_find_exec() {
        let invocation = only_invocation("ls | xargs gws docs \"a b\"", "gws");
        assert_eq!(
            values(&invocation),
            [Some("gws"), Some("docs"), Some("a b"), None]
        );
        assert_eq!(
            invocation.words.last().map(|word| word.cardinality),
            Some(Cardinality::ZeroOrMore)
        );

        let invocation = only_invocation("ls | xargs -I{} gws docs --id {} --x", "gws");
        assert_eq!(
            values(&invocation),
            [Some("gws"), Some("docs"), Some("--id"), None, Some("--x")]
        );

        let invocation = only_invocation("find . -name '*.md' -exec gws docs {} \\;", "gws");
        assert_eq!(values(&invocation), [Some("gws"), Some("docs"), None]);
        assert_eq!(invocation.words[2].cardinality, Cardinality::One);

        let invocation = only_invocation("find . -exec gws docs {} +", "gws");
        assert_eq!(invocation.words[2].cardinality, Cardinality::ZeroOrMore);
    }

    #[test]
    fn test_extract_script_fed_to_shell() {
        let invocation = only_invocation("bash <<< 'gws docs x'", "gws");
        assert_eq!(values(&invocation), [Some("gws"), Some("docs"), Some("x")]);
        assert!(!invocations_of("bash <<'EOF'\ngws docs x\nEOF", "gws").is_empty());
        // シェル以外へ流し込んだ本文はコマンドではない
        if cfg!(feature = "ast-parser") {
            assert!(invocations_of("cat <<'EOF'\ngws docs x\nEOF", "gws").is_empty());
        }
    }

    #[test]
    fn test_extract_stdin_sources() {
        assert_eq!(
            only_invocation("echo a | gws docs", "gws").stdin,
            StdinSource::Other
        );
        assert_eq!(
            only_invocation("gws docs < body.json", "gws").stdin,
            StdinSource::Other
        );
        assert_eq!(
            only_invocation("gws docs <<< \"hi\"", "gws").stdin,
            StdinSource::Literal {
                value: Some("hi\n".to_string())
            }
        );
        assert_eq!(
            only_invocation("gws docs <<< \"$X\"", "gws").stdin,
            StdinSource::Literal { value: None }
        );
        let heredoc = only_invocation("gws docs <<'EOF'\nline 1\nline 2\nEOF", "gws").stdin;
        if cfg!(feature = "ast-parser") {
            assert_eq!(
                heredoc,
                StdinSource::Literal {
                    value: Some("line 1\nline 2\n".to_string())
                }
            );
        } else {
            assert_eq!(heredoc, StdinSource::Literal { value: None });
        }
        assert_eq!(
            only_invocation("gws docs 2>/dev/null", "gws").stdin,
            StdinSource::Inherited
        );
    }

    #[cfg(feature = "ast-parser")]
    #[test]
    fn test_extract_restores_words_tree_sitter_drops() {
        // ヒアドキュメント直前の単独の `-` が構文木から消える
        let invocation = only_invocation("gws docs - <<'EOF'\nbody\nEOF", "gws");
        assert_eq!(values(&invocation), [Some("gws"), Some("docs"), Some("-")]);
        let invocation = only_invocation("gws docs - 2>/dev/null <<-EOF\n\tbody\n\tEOF", "gws");
        assert_eq!(values(&invocation), [Some("gws"), Some("docs"), Some("-")]);
        assert_eq!(
            invocation.stdin,
            StdinSource::Literal {
                value: Some("body\n".to_string())
            }
        );

        // リダイレクトの後ろの語は destination に飲み込まれる
        let invocation = only_invocation("gws 2>/dev/null docs create > out.json --x", "gws");
        assert_eq!(
            values(&invocation),
            [Some("gws"), Some("docs"), Some("create"), Some("--x")]
        );

        // `0<` の `0` は引数ではなく fd
        let invocation = only_invocation("gws docs 0< in.json", "gws");
        assert_eq!(values(&invocation), [Some("gws"), Some("docs")]);
        assert_eq!(invocation.stdin, StdinSource::Other);
    }

    #[cfg(feature = "ast-parser")]
    #[test]
    fn test_extract_pipe_after_heredoc_line() {
        // `| gws x` は heredoc_redirect の子の pipeline になる
        let invocation = only_invocation("cat <<'EOF' | gws docs x\nbody\nEOF", "gws");
        assert_eq!(values(&invocation), [Some("gws"), Some("docs"), Some("x")]);
        assert_eq!(invocation.stdin, StdinSource::Other);
        let cat = only_invocation("cat <<'EOF' | gws docs x\nbody\nEOF", "cat");
        assert_eq!(
            cat.stdin,
            StdinSource::Literal {
                value: Some("body\n".to_string())
            }
        );
    }

    #[cfg(feature = "ast-parser")]
    #[test]
    fn test_extract_process_substitution_stdin() {
        let analysis = analyze("diff <(gws a) >(gws b)");
        let stdin_of = |argument: &str| {
            analysis
                .invocations
                .iter()
                .find(|invocation| {
                    invocation.command_key().as_deref() == Some("gws")
                        && invocation
                            .words
                            .get(1)
                            .and_then(|word| word.value.as_deref())
                            == Some(argument)
                })
                .map(|invocation| invocation.stdin.clone())
        };
        assert_eq!(stdin_of("a"), Some(StdinSource::Inherited));
        assert_eq!(stdin_of("b"), Some(StdinSource::Other));
    }

    #[cfg(feature = "ast-parser")]
    #[test]
    fn test_extract_brace_expansion_is_speculative() {
        let analysis = analyze("{gws,x} docs");
        let speculative: Vec<&Invocation> = analysis
            .invocations
            .iter()
            .filter(|invocation| invocation.command_key().as_deref() == Some("gws"))
            .collect();
        assert!(!speculative.is_empty());
        assert!(
            speculative
                .iter()
                .all(|invocation| invocation.analysis == Analysis::Speculative)
        );
        // 引数の JSON がブレース展開に見えても、判定器に渡す呼び出しは増えない
        // （再解析で見つけた呼び出しは Speculative になる）
        let command = "gws docs --json '{\"a\":1,\"b\":2}'";
        assert!(
            analyze(command)
                .invocations
                .iter()
                .any(|invocation| invocation.analysis == Analysis::Speculative)
        );
        let invocation = only_invocation(command, "gws");
        assert_eq!(
            values(&invocation).last().copied().flatten(),
            Some("{\"a\":1,\"b\":2}")
        );
    }

    #[cfg(feature = "ast-parser")]
    #[test]
    fn test_extract_words_after_compound_redirect_are_speculative() {
        // 複合文の後ろに続く語は bash では構文エラーだが、危険コマンドの検出用に候補として残す
        let analysis = analyze("(cd x) > out rm -rf /");
        let rm = analysis
            .invocations
            .iter()
            .find(|invocation| invocation.command_key().as_deref() == Some("rm"))
            .expect("rm should be found");
        assert_eq!(rm.analysis, Analysis::Speculative);
    }

    #[cfg(feature = "ast-parser")]
    #[test]
    fn test_extract_syntax_error_marks_uncertain() {
        let analysis = analyze("gws docs x )(");
        let found: Vec<&Invocation> = analysis
            .invocations
            .iter()
            .filter(|invocation| invocation.command_key().as_deref() == Some("gws"))
            .collect();
        assert!(!found.is_empty());
        assert!(
            found
                .iter()
                .all(|invocation| invocation.analysis >= Analysis::Uncertain)
        );
    }

    #[test]
    fn test_extract_pathological_input() {
        let long = format!("gws {}", "a ".repeat(40_000));
        assert!(analyze(&long).pathological);
        let deep = format!("{}gws x{}", "$(".repeat(200), ")".repeat(200));
        assert!(analyze(&deep).pathological);
    }

    #[test]
    fn test_extract_deep_wrapper_chain_is_pathological() {
        let command = format!("{}gws x", "sudo ".repeat(200));
        assert!(analyze(&command).pathological);
    }

    /// エージェントが実際に書く形のコマンド。
    #[test]
    fn test_extract_realistic_agent_commands() {
        let invocation = only_invocation(
            "cd /tmp && gws sheets values update --params '{\"range\":\"A1\"}' --json \"$(cat <<'EOF'\n{\"values\": [[\"テスト\"]]}\nEOF\n)\" | jq .",
            "gws",
        );
        assert_eq!(
            values(&invocation),
            [
                Some("gws"),
                Some("sheets"),
                Some("values"),
                Some("update"),
                Some("--params"),
                Some("{\"range\":\"A1\"}"),
                Some("--json"),
                Some("{\"values\": [[\"テスト\"]]}"),
            ]
        );
        assert_eq!(
            only_invocation("gws docs x | jq .", "jq").stdin,
            StdinSource::Other
        );

        let invocation = only_invocation(
            "sudo -u me bash -c 'gws docs x --json \"$(cat <<EOF\nhello\nEOF\n)\"'",
            "gws",
        );
        assert_eq!(
            values(&invocation),
            [
                Some("gws"),
                Some("docs"),
                Some("x"),
                Some("--json"),
                Some("hello")
            ]
        );

        let invocation = only_invocation("gws x \"a\"'b'$'c' > out 2>&1 &", "gws");
        assert_eq!(values(&invocation), [Some("gws"), Some("x"), Some("abc")]);
    }

    /// 本文の `'` や `(` はシェルの構文として数えない（ヒアドキュメントの本文は文字どおり）。
    #[cfg(feature = "ast-parser")]
    #[test]
    fn test_extract_heredoc_body_with_unbalanced_syntax() {
        let invocation = only_invocation(
            "gws docs --json \"$(cat <<'EOF'\nit's (fine)\nEOF\n)\" --params '{}' && echo done",
            "gws",
        );
        assert_eq!(
            values(&invocation),
            [
                Some("gws"),
                Some("docs"),
                Some("--json"),
                Some("it's (fine)"),
                Some("--params"),
                Some("{}"),
            ]
        );
        let invocation = only_invocation("gws docs x <<'EOF' > out.json\nbody\nEOF", "gws");
        assert_eq!(
            invocation.stdin,
            StdinSource::Literal {
                value: Some("body\n".to_string())
            }
        );
    }

    // --- 既存の名前抽出との同値性 ---

    /// 名前抽出（`extract_commands`）が見つけるコマンドを、IR が 1 つも取りこぼさない。
    ///
    /// 呼び出しの IR は、いずれ名前抽出とコマンド文字列抽出の唯一の情報源になる
    /// （経路ごとに解析が食い違う構造を無くすため）。移行の前提として、既存のテストに
    /// 現れる全ての文字列（コマンド・期待値・メッセージを区別せず集めたもの）と、
    /// IR 固有の構文を並べたコーパスで、名前の集合の包含関係を確かめる。
    #[test]
    fn test_invocations_cover_extract_commands() {
        let mut corpus: Vec<String> =
            serde_json::from_str(include_str!("testdata/shell_corpus.json")).unwrap();
        corpus.extend(
            [
                "gws docs documents create --json \"$(cat <<'EOF'\n{\"a\": 1}\nEOF\n)\"",
                "gws docs - <<'EOF'\nbody\nEOF",
                "gws 2>/dev/null docs create",
                "cat <<'EOF' | gws docs x\nbody\nEOF",
                "diff <(gws a) >(gws b)",
                "ls | xargs -I{} sh -c 'rm {}'",
                "find . -exec sudo rm {} +",
                "bash -c \"gws docs '$X'\"",
            ]
            .map(str::to_string),
        );

        let mut parser = ShellParser::new();
        let mut failures = Vec::new();
        for command in &corpus {
            let names = parser.extract_commands(command);
            let analysis = parser.extract_invocations(command);
            let mut covered: HashSet<String> = analysis
                .invocations
                .iter()
                .filter_map(|invocation| invocation.words.first())
                .map(|word| word.text.clone())
                .collect();
            if analysis.pathological {
                covered.extend(["rm", "kill", "dd"].map(str::to_string));
            }
            let missing: Vec<&String> = names
                .iter()
                .filter(|name| !covered.contains(*name))
                .collect();
            if !missing.is_empty() {
                failures.push(format!("{command:?}: {missing:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "extract_invocations missed names that extract_commands found:\n{}",
            failures.join("\n")
        );
    }
}
