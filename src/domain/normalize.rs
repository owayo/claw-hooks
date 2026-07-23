//! AI向け出力正規化ユーティリティ。
//!
//! lint/typecheck出力をトークン効率のために最適化し、
//! エラー情報は維持する。

use std::collections::HashSet;

/// デフォルトの出力最大長（文字数）。
pub const DEFAULT_OUTPUT_MAX_LENGTH: usize = 1000;

/// テキストを指定された最大長（文字数）に切り詰める。
/// サフィックスが収まる場合のみ省略メッセージ (`\n... (truncated)`) を末尾に付加する。
/// `max_length` が 0 の場合は無制限（切り詰めなし）。
pub fn truncate_output(output: &str, max_length: usize) -> String {
    // バイト長 ≤ max_length なら文字数も必ず ≤ max_length（各文字は1バイト以上）
    if max_length == 0 || output.len() <= max_length {
        return output.to_string();
    }
    // バイト長 > max_length の場合のみ、正確な文字数を算出する
    let char_count = output.chars().count();
    if char_count <= max_length {
        return output.to_string();
    }

    let suffix = "\n... (truncated)";
    let suffix_chars = suffix.chars().count();
    // max_length がサフィックス長以下の場合はサフィックスなしで切り詰める
    if max_length <= suffix_chars {
        let truncated: String = output.chars().take(max_length).collect();
        return truncated;
    }
    let keep = max_length - suffix_chars;
    let truncated: String = output.chars().take(keep).collect();

    format!("{}{}", truncated, suffix)
}

/// テキストからANSIエスケープコード（色、スタイル、カーソル制御、OSCハイパーリンク等）を除去する。
pub fn strip_ansi_codes(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next(); // '[' を消費
                    // CSIシーケンス: ESC [ ... 終端バイト (0x40-0x7e)
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next(); // ']' を消費
                    // OSCシーケンス: ESC ] ... (BEL または ESC \ で終端)
                    // 例: ハイパーリンク \x1b]8;;URL\x1b\\TEXT\x1b]8;;\x1b\\
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            // ST (ESC \) なら '\' まで消費する。
                            // それ以外のESCは不正なOSC終端とみなし、後続文字は
                            // 消費せず外側のループで通常文字として再処理する。
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next(); // ESC後の1文字を消費 (SS2/SS3等)
                }
                None => {}
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// lint/typecheck出力をAI向けに正規化する。
/// トークン効率を最適化しつつ、エラー情報は維持する:
/// - ANSIエスケープコード（色、スタイル）を除去
/// - キャリッジリターン（`\r`）で同一行を上書きする進捗表示を、端末で実際に
///   見える「最後の状態」だけに圧縮（Godot のヘッドレス import やダウンロード系
///   ツールが進捗バーを `\r` で繰り返し上書きするケースを集約）
/// - 共通の絶対パスプレフィックスを除去（例: `/home/user/GitHub/project/`）
/// - 各行の先頭・末尾の空白を除去
/// - 連続する空白（スペースとタブ）を1つのスペースに圧縮
/// - biome の重複行番号 `X X │ text` (X 同一) を `X │ text` に圧縮
///   - unchanged context line では old/new が同一整数になるため redundant
///   - old != new のときは情報として保持する
/// - 連続する装飾文字（`.`, `=`, `-`, `─`, `━`, `^`, `·`, `→`）を1文字に圧縮
///   - `^` は ruff / clippy / rust 等の lint 出力で範囲を示すマーカーとして使われる
///   - `·` (U+00B7) は biome 等が空白を視覚化するため diff 行に多用する
///   - `→` (U+2192) は biome 等がタブを視覚化するため diff 行に多用する
/// - 連続する空行を1行に圧縮
/// - 診断ブロックの枠線・キャレットのみの行（`|` / `│` / `^` と空白だけ）を除去
///   - ruff の `|` / `| ^`、biome の `│`、rustc の `   |` 等が該当
///   - 列位置は直前の `file:line:col` ヘッダに残るためエラー情報は失われない
///   - 英数字を含む行（見出し・ソース行・ラベル付きキャレット）は保持する
/// - 進捗系の単語（`Compiling` 等）で始まる行が4行以上連続する場合、4行目以降を集約
///   （診断行 `error:` / `warning:` 等は固有情報があるため対象外）
/// - lint ルールセットの非互換設定警告（ruff の `... are incompatible. Ignoring ...`）を除去
///   - 編集ごとに毎回同じ内容が出るコード非依存のノイズ
///   - コード診断の `warning:`（`warning: unused variable` 等）は保持する
/// - 逐語一致で繰り返される説明行（biome の `i ...`、ruff の `help: ...`、
///   rustc/clippy の `= help:` / `= note:` 等）の2回目以降を除去
///   - 同一ルールの違反が複数あると出現ごとに同じ説明文が繰り返されるため
pub fn normalize_lint_output(output: &str) -> String {
    let stripped = strip_ansi_codes(output);
    let stripped = strip_common_path_prefix(&stripped);

    let mut lines = Vec::new();
    let mut prev_blank = false;

    for line in stripped.lines() {
        // キャリッジリターンで上書きされた進捗表示は、端末で最後に見える状態
        // （最後の `\r` 以降の非空セグメント）だけを残す。途中経過と制御文字を捨てる。
        let line = collapse_carriage_return(line);
        let trimmed = line.trim();

        if trimmed.is_empty() {
            if !prev_blank && !lines.is_empty() {
                lines.push(String::new());
            }
            prev_blank = true;
            continue;
        }
        prev_blank = false;
        let collapsed = collapse_whitespace(trimmed);
        let collapsed = collapse_duplicate_diff_context_line_number(&collapsed);
        let collapsed = collapse_repeated_chars(&collapsed);
        let collapsed = collapse_space_separated_decorative(&collapsed);
        // 診断の枠線・キャレットのみで構成された行（診断テキストを含まない）は
        // トークン節約のため丸ごと除去する。指し示す列位置は直前の
        // `file:line:col` ヘッダに数値で残るため、エラー情報は失われない。
        if is_diagnostic_frame_line(&collapsed) {
            continue;
        }
        // lint ルールセットの非互換設定警告（ruff の `... are incompatible. Ignoring ...`）は
        // 編集ごとに毎回同じ内容が出るコード非依存のノイズなので除去する。
        if is_lint_ruleset_incompatibility_warning(&collapsed) {
            continue;
        }
        lines.push(collapsed);
    }

    // 末尾の空行を除去
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }

    let lines = dedup_repeated_explanation_lines(lines);
    let lines = collapse_repeated_prefix_lines(lines);

    lines.join("\n")
}

/// 逐語一致で繰り返される説明行の2回目以降を除去する。
///
/// 同一ルールの違反が複数あると、biome / ruff / rustc / clippy は違反ごとに
/// まったく同じ説明文を繰り返す（biome の `i any disables many type checking
/// rules. ...`、ruff の `help: Add missing whitespace`、clippy の
/// `= help: for further information visit https://rust-lang.github.io/...` 等）。
/// 説明は1度伝われば十分で、2回目以降は情報量ゼロのままトークンを消費するため
/// 除去する。`file:line:col` ヘッダやソース行は説明行の形式ではないため対象外
/// （位置情報・コード断片は出現ごとに保持される）。
///
/// biome は説明行の前後を空行で挟むため、除去すると空行が連続し得る。
/// 連続空行はここで1行に再圧縮し、末尾に残った空行も落とす。
fn dedup_repeated_explanation_lines(lines: Vec<String>) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut result: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        if is_explanation_line(&line) && !seen.insert(line.clone()) {
            continue;
        }
        if line.is_empty() && result.last().is_some_and(|l| l.is_empty()) {
            continue;
        }
        result.push(line);
    }
    while result.last().is_some_and(|l| l.is_empty()) {
        result.pop();
    }
    result
}

/// 診断の補足説明・ヘルプを表す行かどうかを判定する。
///
/// biome は `i <説明>`、ruff は `help: <説明>`、rustc/clippy は
/// `= help: <説明>` / `= note: <説明>` / `note: <説明>` の形式で補足説明を
/// 出力する（`collapse_whitespace` 適用後の行頭を前提とする）。これらは
/// 診断位置に依存しない一般説明のため、逐語一致すれば重複除去して安全。
/// ソース行（`3 │ code` / `3 | code`）や `file:line:col` ヘッダは
/// この形式に一致しないため対象にならない。
fn is_explanation_line(line: &str) -> bool {
    line.starts_with("i ")
        || line.starts_with("help: ")
        || line.starts_with("= help: ")
        || line.starts_with("= note: ")
        || line.starts_with("note: ")
}

/// 進捗系の単語で始まる行が連続する場合、4行目以降を集約する。
/// cargo の `Compiling foo v1.0` のような進捗ログが大量に並ぶケースで
/// AI に返すフィードバックのトークン量を削減する。
///
/// 集約対象は進捗系の行頭語（`leading_progress_prefix` のホワイトリスト）に
/// 限定する。`error:` / `warning:` などの診断行は各行に固有情報があるため、
/// 進捗語以外の行でランが分断され、集約されない（4 行目以降の情報欠落を防ぐ）。
///
/// 進捗語はいずれも診断価値のないノイズなので、`Compiling` と `Checking` が
/// 交互に出力される cargo の並行ビルドのように、異なる進捗語が混在していても
/// 1 つの連続ランとして扱う。同一語に限定すると交互出力ではランが 3 行以下に
/// 分断され、ほとんどの進捗ノイズが集約されずに残ってしまうため。
///
/// ルール:
/// - 各行の行頭語が進捗系ホワイトリストに含まれる連続ランを対象にする
/// - 4行以上連続した場合、最初の3行のみ残し、4行目以降を要約行で置換する
///   - ラン全体が同一語なら `... (and N more lines starting with "<word>")`
///   - 複数の進捗語が混在するなら `... (and N more progress lines)`
fn collapse_repeated_prefix_lines(lines: Vec<String>) -> Vec<String> {
    let mut result: Vec<String> = Vec::with_capacity(lines.len());
    let mut idx = 0;
    while idx < lines.len() {
        let Some(first_word) = leading_progress_prefix(&lines[idx]) else {
            result.push(lines[idx].clone());
            idx += 1;
            continue;
        };

        // 行頭語が進捗系ホワイトリストに含まれる行が続く限りランを伸ばす。
        // 進捗語が異なっても（Compiling/Checking など）同一ランに含める。
        let mut run_end = idx + 1;
        let mut single_word = true;
        while run_end < lines.len() {
            let Some(word) = leading_progress_prefix(&lines[run_end]) else {
                break;
            };
            if word != first_word {
                single_word = false;
            }
            run_end += 1;
        }

        let run_len = run_end - idx;
        if run_len >= 4 {
            // 最初の3行を残し、4行目以降を集約行で置換する。
            result.extend(lines[idx..idx + 3].iter().cloned());
            let extra = run_len - 3;
            if single_word {
                result.push(format!(
                    "... (and {} more lines starting with \"{}\")",
                    extra, first_word
                ));
            } else {
                result.push(format!("... (and {extra} more progress lines)"));
            }
        } else {
            result.extend(lines[idx..run_end].iter().cloned());
        }
        idx = run_end;
    }
    result
}

/// 行頭の集約対象となる「進捗系の単語」を返す。
///
/// 集約してよいのは cargo / npm 等が大量に出力する進捗ログ
/// （`Compiling ...` の連発など）に限る。`error:` / `warning:` / `help:` /
/// `note:` のような診断行は各行に固有の情報を持つため、たとえ同じ単語で
/// 始まっても集約してはならない（4 行目以降のエラー内容が失われる）。
/// そのため、進捗系ホワイトリストに一致する単語だけを集約対象として返す。
///
/// 単語の条件: ASCII 英字始まり、2文字以上、後に空白またはコロンが続き、
/// かつ進捗系ホワイトリストに含まれること。
fn leading_progress_prefix(line: &str) -> Option<&str> {
    /// cargo / npm 等が連続して大量に出力する進捗ログの行頭語。
    /// 診断系（error/warning/help/note など）は意図的に含めない。
    /// `Blocking` は cargo がロック待ち中に繰り返し出力する
    /// `Blocking waiting for file lock on ...` 行（診断価値なし）が対象。
    const PROGRESS_WORDS: &[&str] = &[
        "Blocking",
        "Building",
        "Checking",
        "Compiling",
        "Documenting",
        "Downloaded",
        "Downloading",
        "Finished",
        "Fresh",
        "Installing",
        "Running",
        "Updating",
    ];

    let mut end = 0;
    let bytes = line.as_bytes();
    while end < bytes.len() && bytes[end].is_ascii_alphabetic() {
        end += 1;
    }
    if end < 2 {
        return None;
    }
    // 単語の後に空白かコロンがある場合のみ集約候補とする。
    // （`Compiling foo` ✓、`fnname()` ✗、`error[E0001]` ✗）
    let next = bytes.get(end).copied();
    if !matches!(next, Some(b' ') | Some(b'\t') | Some(b':')) {
        return None;
    }
    let word = &line[..end];
    // 進捗系のみ集約。診断行は固有情報を持つため集約しない。
    if PROGRESS_WORDS.contains(&word) {
        Some(word)
    } else {
        None
    }
}

/// 同一文字が連続する装飾文字をトークン効率のために圧縮する。
/// 対象: `.`, `=`, `-`, `─`, `━`, `^`, `·`, `→`, `_`
/// ルール:
/// - `=`, `-`, `─`, `━`, `^`, `·`, `→`, `_` は4回以上の連続で1文字に圧縮
/// - `.` は4回以上の連続、または行末の3連続以上で1文字に圧縮
/// - `-` が2回以上連続して直後に `>` が続く場合は1文字に圧縮（`-->` → `->`、rustc の位置マーカー対策）
/// - `_` は rustc/ruff/biome のマルチライン span 下線 `| |_____^` の範囲マーカー対策。
///   snake_case 識別子の区切りは通常 1〜2 連続のため 4 連続以上の閾値では影響しない
///
/// 例: `====` → `=`, `...............` → `.`, `text...` → `text.`, `^^^^^^` → `^`,
///     `············` → `·`, `→→→→` → `→`, `-->` → `->`, `---->` → `->`,
///     `| |_______________^` → `| |_^`
fn collapse_repeated_chars(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        result.push(c);
        if is_decorative_char(c) {
            let mut count = 1u32;
            while chars.peek() == Some(&c) {
                chars.next();
                count += 1;
            }
            let should_collapse = count >= 4
                || (c == '.' && count >= 3 && chars.peek().is_none())
                || (c == '-' && count >= 2 && chars.peek() == Some(&'>'));
            if !should_collapse {
                for _ in 1..count {
                    result.push(c);
                }
            }
        }
    }
    result
}

/// Biome の diff 行に現れる重複行番号 `X X │ text` (X が同一の整数) を
/// `X │ text` に圧縮する。
///
/// Biome の diff フォーマット:
/// - `X Y │ text` (X==Y)          = unchanged context line (両側で同じ行)
/// - `X │ - text`                  = 削除行
/// - `Y │ + text`                  = 追加行
///
/// unchanged context line では old/new が必ず同じ整数になるため、片方は
/// 完全に冗長。`-`/`+` プレフィックスの有無で context と diff は判別可能なため、
/// 同一整数のときに限定して圧縮する。
/// old != new (差分前後で context 行番号がズレる稀なケース) では情報として
/// 保持する必要があるため圧縮しない。
fn collapse_duplicate_diff_context_line_number(line: &str) -> String {
    let (line_numbers, text, has_text_separator) =
        if let Some((line_numbers, text)) = line.split_once(" │ ") {
            (line_numbers, text, true)
        } else if let Some(line_numbers) = line.strip_suffix(" │") {
            // normalize_lint_output は行全体を trim するため、Biome の空 context 行
            // `42 42 │ ` はここに来る時点で `42 42 │` になっている。
            (line_numbers, "", false)
        } else {
            return line.to_string();
        };

    let mut parts = line_numbers.split(' ');
    let Some(old_line) = parts.next() else {
        return line.to_string();
    };
    let Some(new_line) = parts.next() else {
        return line.to_string();
    };

    if parts.next().is_none()
        && old_line == new_line
        && !old_line.is_empty()
        && old_line.bytes().all(|b| b.is_ascii_digit())
    {
        return if has_text_separator {
            format!("{old_line} │ {text}")
        } else {
            format!("{old_line} │")
        };
    }

    line.to_string()
}

/// `→ → → → → → →` や `· · · · · ·` のような「装飾文字＋単一スペース」の
/// 繰り返しパターンを 1 文字に圧縮する。biome 等が diff 行で
/// 連続する空白/タブを 1 文字ごとに視覚化したときに、
/// 同じ文字がスペースを挟んで大量に出現するためトークンが膨らむ。
///
/// 対象は `·` (U+00B7) と `→` (U+2192) のみ。
/// 直前にすでに `collapse_whitespace` が走っている前提で、
/// 区切りは ASCII の単一スペース 1 個に限定する。
///
/// ルール:
/// - パターン `c( c){3,}` (= c が 4 回以上、間に単一スペース 1 個) を `c` に置き換える
/// - 圧縮後に末尾がさらに別の文字に続く場合はスペースを 1 個残す
///   例: `→ → → → → → → Google` → `→ Google`
fn collapse_space_separated_decorative(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut result = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if (c == '→' || c == '·') && i + 2 < chars.len() && chars[i + 1] == ' ' && chars[i + 2] == c
        {
            // パターン `c( c)+` を最大まで走査する。
            // `last_c` は走査中に確認できている最後の `c` の位置を指す。
            let mut count = 1u32;
            let mut last_c = i;
            while last_c + 2 < chars.len() && chars[last_c + 1] == ' ' && chars[last_c + 2] == c {
                count += 1;
                last_c += 2;
            }
            if count >= 4 {
                // 圧縮: 最後の `c` の次の位置へジャンプする。
                // 末尾以外なら直後のスペースはそのまま残るため `→ Google` のような形になる。
                result.push(c);
                i = last_c + 1;
            } else {
                result.push(c);
                i += 1;
            }
        } else {
            result.push(c);
            i += 1;
        }
    }
    result
}

/// 装飾文字かどうかを判定する。
/// `·` (U+00B7) / `→` (U+2192) は biome 等が diff 行で空白・タブを視覚化する際に、
/// `_` (U+005F) は rustc/ruff/biome のマルチライン span 下線
/// (`| |_______________^` のような複数行スパンの範囲マーカー) として大量に出力するため、
/// 4 文字以上連続した場合のみ 1 文字に圧縮してトークン効率を高める。
/// 注: snake_case 識別子の区切りは通常 1〜2 文字連続のため、4 文字以上の閾値により影響しない。
fn is_decorative_char(c: char) -> bool {
    matches!(c, '.' | '=' | '-' | '─' | '━' | '^' | '·' | '→' | '_')
}

/// 行が診断ブロックの枠線・キャレットマーカーのみで構成されているか判定する。
///
/// 対象は ASCII パイプ `|`、キャレット `^`、アンダースコア `_`、Box Drawing 文字
/// (U+2500–U+257F: `│` `─` `╭` `╮` `╰` `╯` `┌` `└` 等の罫線素片全般)、
/// および空白だけからなる行。ruff / biome / rustc が診断ブロックでソース行の
/// 上下に出力する純粋な視覚装飾行（例: ruff の `|` / `| ^`、biome の `│`、
/// rustc の `   |`、rustc/clippy のマルチライン span 下線 `| |_____^` が
/// `collapse_repeated_chars` で `| |_^` に圧縮された後の残骸）、pnpm 等の通知
/// バナー枠（`╭─╮` / `╰─╯`）、biome の `─────` 区切り線が該当する。これらは
/// 診断テキスト（コード・メッセージ・`file:line:col`）を一切含まず、キャレットが
/// 指す列位置は直前の `file:line:col` ヘッダに数値で残るため、除去しても
/// エラー情報は失われない。
///
/// 英数字を含む行（`error:` / `warning:` 見出し、`10 │ let x = ...` のソース行、
/// `| ^ expected u32` のようなラベル付きキャレット行、`│ Update available! │`
/// のようなバナー本文行、`__init__` のような snake_case 識別子を含む行）は対象外。
/// アンダースコアは英数字ではないため許容集合に含めても、英数字を含む行は
/// 引き続き保持される。`| --- |` のような Markdown テーブル区切りも ASCII `-`
/// を含むため保持される。
fn is_diagnostic_frame_line(line: &str) -> bool {
    !line.is_empty()
        && line
            .chars()
            .all(|c| matches!(c, '|' | '^' | '_' | ' ' | '\u{2500}'..='\u{257F}'))
}

/// lint ルールセット同士の非互換を知らせる設定警告かどうかを判定する。
///
/// ruff は互いに衝突するルール（例: D203 と D211、D212 と D213）を同時に有効化すると
/// `warning: `X` (D203) and `Y` (D211) are incompatible. Ignoring `X`.` を **毎回**
/// 出力する。これはルールセット（設定）に関する警告で、編集対象コードの診断ではない。
/// ファイル編集ごとに同じ 2 行が繰り返されるため、AI へのフィードバックでは純粋な
/// ノイズになる。ruff 固有の言い回し（`are incompatible. Ignoring`）で始まる warning 行に
/// 限定して除去し、コード診断（`warning: unused variable` 等）は保持する。
fn is_lint_ruleset_incompatibility_warning(line: &str) -> bool {
    line.starts_with("warning:") && line.contains("are incompatible. Ignoring")
}

/// フォーマッター/リンターが成功時に出す「何もすることがなかった」定型メッセージ
/// だけで構成された出力かどうかを判定する。
///
/// `ruff format --check` の `1 file already formatted`、`ruff format` の
/// `1 file left unchanged`、`ruff check` の `All checks passed!`、biome の
/// `Checked 1 file in 53ms. No fixes applied.` は、編集のたびに毎回出力される
/// コード非依存の完了通知で、エージェントに返しても行動につながらない純粋な
/// ノイズのため、出力ごと破棄する対象とする。
///
/// 「ファイルが書き換えられた」ことを示すメッセージ（`1 file reformatted` /
/// biome の `Fixed 1 file.`）は、エージェントが編集内容を再読すべきシグナル
/// なので対象外。失敗時（exit code 非 0）は診断本文の一部であり得るため、
/// 呼び出し側でコマンド成功時に限って適用すること。
pub fn is_noop_success_output(output: &str) -> bool {
    let mut has_line = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        has_line = true;
        if !is_noop_success_line(trimmed) {
            return false;
        }
    }
    has_line
}

/// 1 行が no-op 完了メッセージかどうかを判定する。
fn is_noop_success_line(line: &str) -> bool {
    // ruff check: 問題なし
    if line == "All checks passed!" {
        return true;
    }
    // biome check: `Checked N file(s) in <時間>. No fixes applied.` に限定する。
    // 任意の `Checked ... No fixes applied.` を受理すると、利用者向けメッセージまで消えてしまう。
    let is_biome_noop = line
        .strip_prefix("Checked ")
        .and_then(counted_file_message_tail)
        .and_then(|tail| tail.strip_prefix("in "))
        .and_then(|tail| tail.strip_suffix(". No fixes applied."))
        .is_some_and(|duration| !duration.is_empty());
    if is_biome_noop {
        return true;
    }
    // ruff format / ruff format --check: 整形済み・変更なしを示す定型メッセージ
    counted_file_message_tail(line)
        .is_some_and(|tail| tail == "already formatted" || tail == "left unchanged")
}

/// `<数> file(s) ` で始まる定型メッセージから後続部分を取り出す。
fn counted_file_message_tail(line: &str) -> Option<&str> {
    let (count, rest) = line.split_once(' ')?;
    if count.is_empty() || !count.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    rest.strip_prefix("files ")
        .or_else(|| rest.strip_prefix("file "))
}

/// キャリッジリターン（`\r`）で上書きされた進捗表示を、端末で実際に表示される
/// 「最後の状態」だけに圧縮する。
///
/// Godot のヘッドレス import やダウンロード系ツールなどは、進捗バーを `\r` で
/// 同一行に何度も上書きする。これをパイプで捕捉すると、`str::lines()` は `\n` と
/// `\r\n` でしか行を分割しないため、単独の `\r` で区切られた途中経過がすべて
/// 1 行に残り、制御文字 `\r` ごとトークンを浪費する。
///
/// 端末は `\r` を「カーソルを行頭へ戻す」と解釈し、以降の出力で前の内容を
/// 上書きするため、人間に見えるのは最後のセグメントだけ。そこで `\r` で分割した
/// うち「末尾に最も近い非空セグメント」を採用し、途中経過を捨てる。行末が `\r`
/// （行をクリアした状態）の場合は直前の非空セグメントを残す。
///
/// 注: `\r\n`（Windows 改行）は呼び出し元の `str::lines()` が処理済みのため、
/// ここに渡る `\r` は常に単独の上書き用。lint テキスト本文に `\r` は現れない。
fn collapse_carriage_return(line: &str) -> &str {
    if !line.contains('\r') {
        return line;
    }
    line.rsplit('\r')
        .find(|segment| !segment.trim().is_empty())
        .unwrap_or("")
}

/// 連続する空白（スペースとタブ）を1つのスペースに圧縮する。
fn collapse_whitespace(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut prev_ws = false;
    for c in input.chars() {
        if c == ' ' || c == '\t' {
            if !prev_ws {
                result.push(' ');
            }
            prev_ws = true;
        } else {
            prev_ws = false;
            result.push(c);
        }
    }
    result
}

/// テキスト内の絶対パスから共通ディレクトリプレフィックスを除去する。
/// パスが2つ以上あり、プレフィックスがスラッシュ3つ以上の深さ（例: `/home/user/`）であることが条件。
/// パスコンテキスト（直前がスラッシュ以外またはトークン先頭）でのみ置換し、
/// エラーメッセージ本文中の偶発的な一致を除外する。
fn strip_common_path_prefix(text: &str) -> String {
    let paths = extract_absolute_paths(text);
    if paths.len() < 2 {
        return text.to_string();
    }

    let prefix = common_directory_prefix(&paths);
    // スラッシュ3つ未満だと除去が少なすぎるため、最低3つを要求
    if prefix.is_empty() || prefix.matches('/').count() < 3 {
        return text.to_string();
    }

    // パスコンテキストでのみ置換: プレフィックスの「前後」両方がパス境界条件を
    // 満たす場合にのみ除去する。後方のみを検査すると、非パス文字列に共通プレフィックスが
    // 偶然部分文字列として現れた場合（例: `foo/proj/src/...` のような文中の出現）に
    // 中身を破壊する。`extract_absolute_paths` のパス開始判定と対称な条件を採用する。
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;
    let mut consumed = 0usize;
    while let Some(pos) = remaining.find(prefix) {
        result.push_str(&remaining[..pos]);
        let abs_pos = consumed + pos;
        let after = &remaining[pos + prefix.len()..];
        // 直前文字: 行頭/空白/`-`/`>`/`=` のいずれかがパス開始位置として有効
        let prev = text[..abs_pos].chars().next_back();
        let is_prev_path_start = match prev {
            None => true,
            Some(c) if c.is_ascii_whitespace() => true,
            Some('-') | Some('>') | Some('=') => true,
            _ => false,
        };
        // 直後文字: ファイル名文字（英数字、_、.、-）が続く場合のみパス継続とみなす
        let is_next_path_char = after
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '-');
        if is_prev_path_start && is_next_path_char {
            // プレフィックスを除去（直後のパス部分はそのまま保持）
            remaining = after;
        } else {
            // パスコンテキストでなければプレフィックスをそのまま保持
            result.push_str(prefix);
            remaining = after;
        }
        consumed = abs_pos + prefix.len();
    }
    result.push_str(remaining);
    result
}

/// lint出力テキストから絶対パスを抽出する。
/// `/path/file.rs:10:5`、`-->/path/file.rs:10:5`、`/path/file.ts(10,5)` 等の形式に対応。
///
/// 行単位で最初の `/` を探し、`:` `(` まで（または改行）をパスとして扱うため、
/// `My Documents` のようにスペースを含むディレクトリパスも正しく抽出できる。
/// 直前文字が空白・行頭・`-` `>` `=` のいずれか（rustc の `--> file:line` 等を許容）の場合のみ
/// パスの開始位置として採用し、`http://` のような URL を誤抽出しないようにする。
fn extract_absolute_paths(text: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    for line in text.lines() {
        let mut search_from = 0;
        while let Some(rel_start) = line[search_from..].find('/') {
            let start = search_from + rel_start;
            let prev = line[..start].chars().next_back();
            let is_path_start = match prev {
                None => true,
                Some(c) if c.is_ascii_whitespace() => true,
                Some('-') | Some('>') | Some('=') => true,
                _ => false,
            };
            if !is_path_start {
                // パス開始位置として採用しない場合でも、続く `/` を探索する
                search_from = start + 1;
                continue;
            }
            let rest = &line[start..];
            let end = rest.find([':', '(']).unwrap_or(rest.len());
            let path = &rest[..end];
            if path.matches('/').count() >= 2 && path.len() > 2 {
                paths.push(path);
            }
            search_from = start + end.max(1);
        }
    }
    paths
}

/// パス群の最長共通ディレクトリプレフィックスを算出する。
fn common_directory_prefix<'a>(paths: &[&'a str]) -> &'a str {
    if paths.is_empty() {
        return "";
    }

    let first = paths[0];
    let mut last_slash = 0;

    for (i, byte) in first.bytes().enumerate() {
        if !paths.iter().all(|p| p.as_bytes().get(i) == Some(&byte)) {
            break;
        }
        if byte == b'/' {
            last_slash = i + 1;
        }
    }

    &first[..last_slash]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_ansi_codes_removes_colors() {
        let input = "\x1b[31merror\x1b[0m: something failed";
        assert_eq!(strip_ansi_codes(input), "error: something failed");
    }

    #[test]
    fn test_strip_ansi_codes_no_ansi() {
        let input = "plain text";
        assert_eq!(strip_ansi_codes(input), "plain text");
    }

    #[test]
    fn test_normalize_strips_ansi_and_indentation() {
        let input = "\x1b[31m  error: bad code\x1b[0m\n    at line 10";
        let result = normalize_lint_output(input);
        assert_eq!(result, "error: bad code\nat line 10");
    }

    #[test]
    fn test_normalize_strips_trailing_spaces() {
        let input = "error: bad code   \n  at line 10  ";
        let result = normalize_lint_output(input);
        assert_eq!(result, "error: bad code\nat line 10");
    }

    #[test]
    fn test_normalize_collapses_consecutive_spaces() {
        let input = "error:   type    mismatch\nexpected  `u32`,   got  `String`";
        let result = normalize_lint_output(input);
        assert_eq!(result, "error: type mismatch\nexpected `u32`, got `String`");
    }

    #[test]
    fn test_normalize_collapses_tabs() {
        let input = "error:\t\ttype\tmismatch";
        let result = normalize_lint_output(input);
        assert_eq!(result, "error: type mismatch");
    }

    #[test]
    fn test_normalize_collapses_mixed_whitespace() {
        let input = "error: \t type \t\t mismatch";
        let result = normalize_lint_output(input);
        assert_eq!(result, "error: type mismatch");
    }

    #[test]
    fn test_normalize_collapses_blank_lines() {
        let input = "line1\n\n\n\nline2";
        let result = normalize_lint_output(input);
        assert_eq!(result, "line1\n\nline2");
    }

    #[test]
    fn test_normalize_removes_trailing_blank_lines() {
        let input = "line1\nline2\n\n\n";
        let result = normalize_lint_output(input);
        assert_eq!(result, "line1\nline2");
    }

    #[test]
    fn test_normalize_empty_input() {
        assert_eq!(normalize_lint_output(""), "");
    }

    #[test]
    fn test_is_noop_success_output_recognizes_known_formatter_messages() {
        for output in [
            "All checks passed!",
            "1 file already formatted",
            "2 files already formatted",
            "1 file left unchanged",
            "3 files left unchanged",
            "Checked 1 file in 53ms. No fixes applied.",
        ] {
            assert!(is_noop_success_output(output), "未認識の出力: {output}");
        }
    }

    #[test]
    fn test_is_noop_success_output_rejects_empty_output() {
        assert!(!is_noop_success_output(""));
        assert!(!is_noop_success_output("\n  \n"));
    }

    #[test]
    fn test_is_noop_success_output_preserves_actionable_output() {
        for output in [
            "1 file reformatted",
            "Fixed 1 file.",
            "All checks passed!\nwarning: generated file changed",
            "Checked configuration. No fixes applied.",
        ] {
            assert!(
                !is_noop_success_output(output),
                "誤って抑制する出力: {output}"
            );
        }
    }

    #[test]
    fn test_collapse_carriage_return_keeps_last_segment() {
        // 進捗バーが \r で上書きされたケースは最後の状態だけを残す。
        assert_eq!(collapse_carriage_return("1%\r2%\r3%"), "3%");
        // 行末が \r（行クリア）なら直前の非空セグメントを残す。
        assert_eq!(
            collapse_carriage_return("Downloading 100%\r"),
            "Downloading 100%"
        );
        // \r を含まない行はそのまま返す（追加アロケーションなし）。
        assert_eq!(collapse_carriage_return("plain line"), "plain line");
        // \r のみ・空白のみは空文字列に圧縮される。
        assert_eq!(collapse_carriage_return("\r"), "");
        assert_eq!(collapse_carriage_return("done\r   "), "done");
    }

    #[test]
    fn test_normalize_collapses_carriage_return_progress() {
        // \r で同一行を上書きする進捗表示は最後の状態のみ残り、制御文字も消える。
        let input = "Importing: a.png\rImporting: b.png\rImporting: c.png\n";
        let result = normalize_lint_output(input);
        assert_eq!(result, "Importing: c.png");
        assert!(!result.contains('\r'));
    }

    #[test]
    fn test_normalize_carriage_return_then_dot_collapse() {
        // \r 圧縮後に進捗ドットの圧縮も適用される。
        let input = "task\r......................  63 / 212 ( 29%)\n";
        let result = normalize_lint_output(input);
        assert_eq!(result, ". 63 / 212 ( 29%)");
    }

    #[test]
    fn test_normalize_migration_progress_example() {
        // マイグレーション系ツールの装飾枠・末尾ドット・進捗ドットを圧縮する。
        let input = "Using attach strategy to execute scripts...\n\
                     ==== Starting migration for: 3.8.0.rc001 ====\n\
                     ...............................................................  63 / 212 ( 29%)";
        let result = normalize_lint_output(input);
        assert_eq!(
            result,
            "Using attach strategy to execute scripts.\n\
             = Starting migration for: 3.8.0.rc001 =\n\
             . 63 / 212 ( 29%)"
        );
    }

    #[test]
    fn test_normalize_carriage_return_preserves_following_error_line() {
        // \r で進捗を上書きした後、別行に続くエラーは保持されること（情報欠落防止）。
        let input = "Building...\rBuilt\nerror: type mismatch";
        let result = normalize_lint_output(input);
        assert_eq!(result, "Built\nerror: type mismatch");
    }

    #[test]
    fn test_normalize_carriage_return_mixed_with_crlf() {
        // CRLF（Windows 改行）は行として分割され、行内の単独 \r は上書きとして圧縮される。
        let input = "step1\r\n1%\r2%\r100%\r\nstep3";
        let result = normalize_lint_output(input);
        assert_eq!(result, "step1\n100%\nstep3");
    }

    #[test]
    fn test_normalize_carriage_return_without_trailing_newline() {
        // 末尾に改行が無い単独 \r 行でも最後の状態だけ残る。
        let input = "loading 1%\rloading 99%";
        let result = normalize_lint_output(input);
        assert_eq!(result, "loading 99%");
    }

    #[test]
    fn test_collapse_whitespace() {
        assert_eq!(collapse_whitespace("a  b"), "a b");
        assert_eq!(collapse_whitespace("a     b     c"), "a b c");
        assert_eq!(collapse_whitespace("no-extra-spaces"), "no-extra-spaces");
        assert_eq!(collapse_whitespace(""), "");
        assert_eq!(collapse_whitespace(" "), " ");
    }

    #[test]
    fn test_collapse_whitespace_tabs() {
        assert_eq!(collapse_whitespace("a\tb"), "a b");
        assert_eq!(collapse_whitespace("a\t\tb"), "a b");
        assert_eq!(collapse_whitespace("a \t b"), "a b");
        assert_eq!(collapse_whitespace("\t"), " ");
    }

    // === パスプレフィックス除去 ===

    #[test]
    fn test_strip_common_path_prefix_multiple_files() {
        // 同一ディレクトリ → プレフィックスに /src/ を含む
        let input = "/home/user/GitHub/project/src/main.rs:10 error\n/home/user/GitHub/project/src/lib.rs:20 warning";
        let result = strip_common_path_prefix(input);
        assert_eq!(result, "main.rs:10 error\nlib.rs:20 warning");
    }

    #[test]
    fn test_strip_common_path_prefix_different_dirs() {
        let input =
            "/home/user/GitHub/project/src/main.rs:10\n/home/user/GitHub/project/tests/test.rs:20";
        let result = strip_common_path_prefix(input);
        assert_eq!(result, "src/main.rs:10\ntests/test.rs:20");
    }

    #[test]
    fn test_strip_common_path_prefix_single_path_skipped() {
        let input = "/home/user/GitHub/project/src/main.rs:10 error";
        let result = strip_common_path_prefix(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_strip_common_path_prefix_no_paths() {
        let input = "error: type mismatch";
        let result = strip_common_path_prefix(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_strip_common_path_prefix_shallow_paths_skipped() {
        // パスごとにスラッシュ2つのみ - プレフィックスが浅すぎて除去不可
        let input = "/usr/file1:10\n/usr/file2:20";
        let result = strip_common_path_prefix(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_extract_absolute_paths() {
        let text = "error at /home/user/src/main.rs:10:5";
        let paths = extract_absolute_paths(text);
        assert_eq!(paths, vec!["/home/user/src/main.rs"]);
    }

    #[test]
    fn test_extract_absolute_paths_with_arrow() {
        let text = "-->/home/user/src/main.rs:10:5";
        let paths = extract_absolute_paths(text);
        assert_eq!(paths, vec!["/home/user/src/main.rs"]);
    }

    #[test]
    fn test_extract_absolute_paths_with_parens() {
        let text = "/home/user/src/main.ts(10,5): error";
        let paths = extract_absolute_paths(text);
        assert_eq!(paths, vec!["/home/user/src/main.ts"]);
    }

    #[test]
    fn test_common_directory_prefix() {
        let paths = vec!["/a/b/c/file1.rs", "/a/b/d/file2.rs"];
        assert_eq!(common_directory_prefix(&paths), "/a/b/");
    }

    #[test]
    fn test_common_directory_prefix_same_dir() {
        let paths = vec!["/a/b/c/file1.rs", "/a/b/c/file2.rs"];
        assert_eq!(common_directory_prefix(&paths), "/a/b/c/");
    }

    #[test]
    fn test_common_directory_prefix_no_common() {
        let paths = vec!["/a/b/file1.rs", "/c/d/file2.rs"];
        assert_eq!(common_directory_prefix(&paths), "/");
    }

    #[test]
    fn test_normalize_strips_path_prefix() {
        // E2E: 異なるディレクトリ → プレフィックスはプロジェクトルート
        let input = "/home/user/GitHub/project/src/App.tsx:10:5 error\n/home/user/GitHub/project/tests/index.test.tsx:20:3 warning";
        let result = normalize_lint_output(input);
        assert_eq!(
            result,
            "src/App.tsx:10:5 error\ntests/index.test.tsx:20:3 warning"
        );
    }

    // === エッジケースの追加テスト ===

    #[test]
    fn test_common_directory_prefix_empty_input() {
        let paths: Vec<&str> = vec![];
        assert_eq!(common_directory_prefix(&paths), "");
    }

    #[test]
    fn test_common_directory_prefix_single_path() {
        let paths = vec!["/a/b/c/file.rs"];
        // 単一パス: last_slashはイテレーション終了前の最後の '/' まで追跡
        assert_eq!(common_directory_prefix(&paths), "/a/b/c/");
    }

    #[test]
    fn test_common_directory_prefix_identical_paths() {
        let paths = vec!["/a/b/c/file.rs", "/a/b/c/file.rs"];
        assert_eq!(common_directory_prefix(&paths), "/a/b/c/");
    }

    #[test]
    fn test_common_directory_prefix_root_only() {
        let paths = vec!["/foo", "/bar"];
        assert_eq!(common_directory_prefix(&paths), "/");
    }

    #[test]
    fn test_extract_absolute_paths_no_paths() {
        let paths = extract_absolute_paths("no paths here");
        assert!(paths.is_empty());
    }

    #[test]
    fn test_extract_absolute_paths_shallow_path_excluded() {
        // スラッシュ1つのみのパスは除外（2つ以上が必要）
        let paths = extract_absolute_paths("/file:10");
        assert!(paths.is_empty());
    }

    #[test]
    fn test_strip_ansi_codes_multiple_sequences() {
        let input = "\x1b[1m\x1b[31merror\x1b[0m: \x1b[33mwarning\x1b[0m";
        assert_eq!(strip_ansi_codes(input), "error: warning");
    }

    #[test]
    fn test_strip_ansi_codes_incomplete_sequence() {
        // ESCの後に '[' が続かない場合（SS2/SS3等の2文字エスケープシーケンス）
        let input = "text\x1bXmore";
        let result = strip_ansi_codes(input);
        // ESCとそれに続く1文字はエスケープシーケンスとして除去される
        assert_eq!(result, "textmore");
    }

    #[test]
    fn test_strip_ansi_codes_osc_hyperlink() {
        // OSC-8 ハイパーリンク: \x1b]8;;URL\x1b\\TEXT\x1b]8;;\x1b\\
        let input = "\x1b]8;;https://example.com\x1b\\click here\x1b]8;;\x1b\\";
        let result = strip_ansi_codes(input);
        assert_eq!(result, "click here");
    }

    #[test]
    fn test_strip_ansi_codes_osc_bel_terminated() {
        // BEL (0x07) で終端する OSC シーケンス
        let input = "before\x1b]0;window title\x07after";
        let result = strip_ansi_codes(input);
        assert_eq!(result, "beforeafter");
    }

    #[test]
    fn test_strip_ansi_codes_mixed_csi_and_osc() {
        // CSI と OSC が混在するケース
        let input = "\x1b[31m\x1b]8;;https://doc.rust-lang.org\x1b\\E0308\x1b]8;;\x1b\\\x1b[0m";
        let result = strip_ansi_codes(input);
        assert_eq!(result, "E0308");
    }

    #[test]
    fn test_strip_ansi_codes_osc_invalid_escape_preserves_following_char() {
        // OSC内でESCの後に\以外の文字が来た場合、後続文字が消費されないことを検証
        let input = "before\x1b]0;window title\x1bXafter";
        let result = strip_ansi_codes(input);
        assert_eq!(result, "beforeXafter");
    }

    #[test]
    fn test_strip_ansi_codes_osc_unterminated() {
        // BELもSTも無いOSCシーケンスはEOFまで消費される
        let input = "text\x1b]0;unterminated";
        let result = strip_ansi_codes(input);
        assert_eq!(result, "text");
    }

    #[test]
    fn test_strip_ansi_codes_osc_followed_by_csi() {
        // OSC後にCSIが続く場合、OSCが終了しCSIも除去される
        // OSC内のESCは消費されるが、後続の[0mは通常文字として残る
        let input = "text\x1b]0;title\x1b[0mmore";
        let result = strip_ansi_codes(input);
        assert_eq!(result, "text[0mmore");
    }

    #[test]
    fn test_strip_ansi_codes_empty_input() {
        assert_eq!(strip_ansi_codes(""), "");
    }

    #[test]
    fn test_strip_ansi_codes_only_escape() {
        // ESCだけの入力
        let result = strip_ansi_codes("\x1b");
        assert_eq!(result, "");
    }

    #[test]
    fn test_normalize_only_blank_lines() {
        let input = "\n\n\n";
        let result = normalize_lint_output(input);
        assert_eq!(result, "");
    }

    #[test]
    fn test_normalize_leading_blank_lines_suppressed() {
        let input = "\n\nerror: foo";
        let result = normalize_lint_output(input);
        assert_eq!(result, "error: foo");
    }

    // === truncate_output テスト ===

    #[test]
    fn test_truncate_output_within_limit() {
        let input = "short text";
        assert_eq!(truncate_output(input, 1000), "short text");
    }

    #[test]
    fn test_truncate_output_exact_limit() {
        let input = "a".repeat(1000);
        assert_eq!(truncate_output(&input, 1000), input);
    }

    #[test]
    fn test_truncate_output_exceeds_limit() {
        let input = "a".repeat(1500);
        let result = truncate_output(&input, 1000);
        assert!(result.chars().count() <= 1000);
        assert!(result.ends_with("... (truncated)"));
    }

    #[test]
    fn test_truncate_output_zero_means_unlimited() {
        let input = "a".repeat(10000);
        assert_eq!(truncate_output(&input, 0), input);
    }

    #[test]
    fn test_truncate_output_utf8_chars() {
        // 日本語文字で文字数ベースの切り詰めを確認
        let input = "あ".repeat(500); // 500文字, 1500バイト
        let result = truncate_output(&input, 100);
        assert!(result.ends_with("... (truncated)"));
        assert!(result.chars().count() <= 100);
    }

    #[test]
    fn test_truncate_output_empty_input() {
        assert_eq!(truncate_output("", 1000), "");
    }

    // === truncate_output 境界ケーステスト ===

    #[test]
    fn test_truncate_output_max_length_equals_suffix_char_count() {
        // サフィックス文字数（16文字）ちょうどの場合はサフィックスなしで切り詰め
        let input = "abcdefghijklmnopqrstuvwxyz";
        let result = truncate_output(input, 16);
        assert_eq!(result, "abcdefghijklmnop");
        assert!(result.chars().count() <= 16);
    }

    #[test]
    fn test_truncate_output_max_length_less_than_suffix() {
        // サフィックス文字数未満の場合もサフィックスなしで切り詰め
        let input = "abcdefghijklmnopqrstuvwxyz";
        let result = truncate_output(input, 5);
        assert_eq!(result, "abcde");
        assert!(result.chars().count() <= 5);
    }

    #[test]
    fn test_truncate_output_max_length_one() {
        let result = truncate_output("hello", 1);
        assert_eq!(result, "h");
    }

    #[test]
    fn test_truncate_output_small_max_length_multibyte() {
        // 文字数ベースの切り詰め: 3文字まで許可 → 3文字残る
        assert_eq!(truncate_output("あいうえお", 3), "あいう");
        // 1文字まで
        assert_eq!(truncate_output("あいうえお", 1), "あ");
        // 4文字まで
        assert_eq!(truncate_output("あいうえお", 4), "あいうえ");
    }

    #[test]
    fn test_truncate_output_emoji_4byte() {
        // 4バイト絵文字でも文字数ベースで正しく切り詰められる
        let input = "🎉🎊🎈🎁🎂🎃🎄🎅🎆🎇🎋🎌🎍🎎🎏🎐🎑🎒🎓🎠";
        let result = truncate_output(input, 5);
        assert_eq!(result.chars().count(), 5);
        assert_eq!(result, "🎉🎊🎈🎁🎂");
    }

    #[test]
    fn test_truncate_output_mixed_ascii_multibyte() {
        // ASCII と日本語の混在
        let input = "error: 型の不一致が発生しました。expected `u32`, got `String`";
        let result = truncate_output(input, 20);
        assert!(result.chars().count() <= 20);
    }

    #[test]
    fn test_truncate_output_fastpath_ascii() {
        // ASCII のみの場合、バイト長チェックで早期リターンする
        let input = "a".repeat(999);
        let result = truncate_output(&input, 1000);
        assert_eq!(result, input);
    }

    // === truncate_output 追加テスト ===

    #[test]
    fn test_truncate_output_ascii_within_limit() {
        // 制限内のテキストはそのまま返される
        let input = "hello world";
        assert_eq!(truncate_output(input, 100), "hello world");
    }

    #[test]
    fn test_truncate_output_ascii_exceeds_limit() {
        // 制限超過時はサフィックス付きで切り詰められる
        let input = "a".repeat(50);
        let result = truncate_output(&input, 30);
        // サフィックス "\n... (truncated)" は16文字 → 30 - 16 = 14文字保持
        let expected = format!("{}\n... (truncated)", "a".repeat(14));
        assert_eq!(result, expected);
        assert_eq!(result.chars().count(), 30);
    }

    #[test]
    fn test_truncate_output_multibyte_chars() {
        // 日本語テキストが文字境界で正しく切り詰められる
        let input = "あいうえお"; // 5文字
        let result = truncate_output(input, 3);
        // max_length(3) <= suffix_chars(16) なのでサフィックスなし
        assert_eq!(result, "あいう");
    }

    #[test]
    fn test_truncate_output_emoji() {
        // 絵文字を含むテキストが文字数ベースで正しく切り詰められる
        let input = "hello 🎉🎊🎈"; // 9文字
        let result = truncate_output(input, 8);
        // max_length(8) <= suffix_chars(16) なのでサフィックスなし
        assert_eq!(result, "hello 🎉🎊");
        assert_eq!(result.chars().count(), 8);
    }

    #[test]
    fn test_truncate_output_zero_limit() {
        // max_length 0 は無制限を意味し、テキストはそのまま返される
        let input = "some text";
        assert_eq!(truncate_output(input, 0), "some text");
    }

    #[test]
    fn test_truncate_output_empty_input_returns_empty() {
        // 空文字列は空文字列を返す
        assert_eq!(truncate_output("", 10), "");
        assert_eq!(truncate_output("", 0), "");
    }

    // === strip_ansi_codes 追加テスト ===

    #[test]
    fn test_strip_ansi_codes_256_color() {
        // 256色エスケープシーケンスの除去
        let input = "\x1b[38;5;196mred\x1b[0m";
        assert_eq!(strip_ansi_codes(input), "red");
    }

    #[test]
    fn test_strip_ansi_codes_truecolor() {
        // 24ビットTrueColorエスケープシーケンスの除去
        let input = "\x1b[38;2;255;0;0mred\x1b[0m";
        assert_eq!(strip_ansi_codes(input), "red");
    }

    #[test]
    fn test_strip_ansi_codes_multiple_params() {
        // 複数パラメータ（太字+赤+下線）のCSIシーケンス除去
        let input = "\x1b[1;31;4mbold red underline\x1b[0m";
        assert_eq!(strip_ansi_codes(input), "bold red underline");
    }

    #[test]
    fn test_strip_ansi_codes_empty_params() {
        // パラメータなしのCSIシーケンス（ESC [ m）も正しく除去される
        let input = "\x1b[mtext";
        assert_eq!(strip_ansi_codes(input), "text");
    }

    // === normalize_lint_output 追加テスト ===

    #[test]
    fn test_normalize_leading_blank_lines_removed() {
        // 先頭の空行が除去されることを確認
        let input = "\n\n\n  error: something failed\n  at line 42";
        let result = normalize_lint_output(input);
        // 先頭に空行が残らない
        assert!(!result.starts_with('\n'));
        assert_eq!(result, "error: something failed\nat line 42");
    }

    // === common_directory_prefix マルチバイトパステスト ===

    #[test]
    fn test_common_directory_prefix_multibyte_directory() {
        // マルチバイト文字を含むディレクトリパスでも正しく動作する
        let paths = vec![
            "/Users/田中/project/src/main.rs",
            "/Users/田中/project/tests/test.rs",
        ];
        assert_eq!(common_directory_prefix(&paths), "/Users/田中/project/");
    }

    #[test]
    fn test_common_directory_prefix_multibyte_diverge_at_dir() {
        // マルチバイト文字のディレクトリ名が異なる場合
        let paths = vec!["/home/太郎/code/file.rs", "/home/花子/code/file.rs"];
        assert_eq!(common_directory_prefix(&paths), "/home/");
    }

    #[test]
    fn test_common_directory_prefix_emoji_directory() {
        // 絵文字を含むディレクトリ名
        let paths = vec!["/data/🎉project/src/a.rs", "/data/🎉project/src/b.rs"];
        assert_eq!(common_directory_prefix(&paths), "/data/🎉project/src/");
    }

    // === extract_absolute_paths 追加テスト ===

    #[test]
    fn test_extract_absolute_paths_multiple() {
        let text = "/usr/src/main.rs:10 and /usr/src/lib.rs:20";
        let paths = extract_absolute_paths(text);
        assert_eq!(paths, vec!["/usr/src/main.rs", "/usr/src/lib.rs"]);
    }

    #[test]
    fn test_extract_absolute_paths_no_deep_path() {
        // スラッシュ1つだけのパスは除外
        let text = "/tmp:10";
        let paths = extract_absolute_paths(text);
        assert!(paths.is_empty());
    }

    // === strip_ansi_codes CSI境界テスト ===

    #[test]
    fn test_strip_ansi_codes_csi_unterminated() {
        // CSIシーケンスが終端なしで入力が終わる場合
        let input = "text\x1b[31";
        let result = strip_ansi_codes(input);
        assert_eq!(result, "text");
    }

    #[test]
    fn test_strip_ansi_codes_csi_empty_sequence() {
        // CSI直後に終端文字が来る場合（ESC [ @）
        let input = "text\x1b[@more";
        let result = strip_ansi_codes(input);
        assert_eq!(result, "textmore");
    }

    // === normalize_lint_output E2E テスト ===

    #[test]
    fn test_normalize_complex_rust_output() {
        // Rust コンパイラ出力に似た複雑な入力
        let input = "\x1b[1;31merror[E0308]\x1b[0m: mismatched types\n  \x1b[1;34m-->\x1b[0m /Users/dev/project/src/main.rs:10:5\n   |\n10 |     let x: u32 = \"hello\";\n   |                  ^^^^^^^ expected `u32`, found `&str`";
        let result = normalize_lint_output(input);
        // ANSI除去されること
        assert!(!result.contains("\x1b["));
        // エラー情報が保持されること
        assert!(result.contains("error[E0308]"));
        assert!(result.contains("mismatched types"));
    }

    #[test]
    fn test_normalize_whitespace_only_lines() {
        // スペースのみの行は空行として扱われる
        let input = "line1\n   \n   \nline2";
        let result = normalize_lint_output(input);
        assert_eq!(result, "line1\n\nline2");
    }

    // === truncate_output サフィックス境界テスト ===

    #[test]
    fn test_truncate_output_suffix_exactly_fits() {
        // max_length がサフィックス文字数+1のとき、1文字+サフィックスで出力される
        let suffix = "\n... (truncated)"; // 16文字
        let input = "abcdefghijklmnopqrstuvwxyz"; // 26文字
        let result = truncate_output(input, 17);
        assert_eq!(result, format!("a{}", suffix));
        assert_eq!(result.chars().count(), 17);
    }

    // === truncate_output マルチバイト文字テスト ===

    #[test]
    fn test_truncate_output_multibyte_within_limit() {
        // マルチバイト文字（3バイト/文字）で文字数がmax_length以下なら切り詰めない
        let input = "あいうえお"; // 5文字, 15バイト
        let result = truncate_output(input, 10);
        assert_eq!(result, "あいうえお");
    }

    #[test]
    fn test_truncate_output_emoji_within_limit() {
        // エモジ（4バイト/文字）で文字数がmax_length以下なら切り詰めない
        let input = "😀😀😀"; // 3文字, 12バイト
        let result = truncate_output(input, 5);
        assert_eq!(result, "😀😀😀");
    }

    #[test]
    fn test_truncate_output_multibyte_exceeds_limit() {
        // マルチバイト文字で文字数がmax_lengthを超える場合は切り詰める
        // サフィックス "\n... (truncated)" は16文字なので、max_length=20 で確認
        let input = "あいうえおかきくけこさしすせそたちつてと"; // 20文字
        let result = truncate_output(input, 10);
        // max_length=10 < サフィックス長16 のため、サフィックスなしで10文字に切り詰め
        assert_eq!(result.chars().count(), 10);
        assert_eq!(result, "あいうえおかきくけこ");
    }

    #[test]
    fn test_truncate_output_max_length_equals_suffix_length() {
        // max_length がサフィックス長と同じ場合、サフィックスなしで切り詰め
        let input = "abcdefghijklmnopqrstuvwxyz"; // 26文字
        let suffix_len = "\n... (truncated)".chars().count(); // 16
        let result = truncate_output(input, suffix_len);
        assert_eq!(result.chars().count(), suffix_len);
        assert!(!result.contains("truncated"));
    }

    #[test]
    fn test_truncate_output_max_length_one_no_suffix() {
        // max_length = 1 の場合、サフィックスが収まらないので文字だけ切り詰め
        let input = "abcdef";
        let result = truncate_output(input, 1);
        assert_eq!(result, "a");
    }

    // === strip_ansi_codes 不正シーケンステスト ===

    #[test]
    fn test_strip_ansi_codes_unterminated_csi() {
        // 終端文字なしの CSI シーケンス（入力が途中で終了）
        let input = "text\x1b[31";
        let result = strip_ansi_codes(input);
        assert_eq!(result, "text");
    }

    #[test]
    fn test_strip_ansi_codes_unterminated_osc() {
        // BEL も ST もなしの OSC シーケンス（入力が途中で終了）
        let input = "text\x1b]8;;url";
        let result = strip_ansi_codes(input);
        assert_eq!(result, "text");
    }

    #[test]
    fn test_strip_ansi_codes_bare_escape_at_end() {
        // 文字列末尾の孤立 ESC
        let input = "text\x1b";
        let result = strip_ansi_codes(input);
        assert_eq!(result, "text");
    }

    #[test]
    fn test_strip_ansi_codes_consecutive_escape_sequences() {
        // 連続する複数の ANSI シーケンス
        let input = "\x1b[1m\x1b[31m\x1b[4mhello\x1b[0m";
        let result = strip_ansi_codes(input);
        assert_eq!(result, "hello");
    }

    // === normalize_lint_output エッジケース ===

    #[test]
    fn test_normalize_lint_output_empty_input() {
        assert_eq!(normalize_lint_output(""), "");
    }

    #[test]
    fn test_normalize_lint_output_only_whitespace() {
        assert_eq!(normalize_lint_output("   \n  \t  \n  "), "");
    }

    #[test]
    fn test_normalize_lint_output_single_path_no_stripping() {
        // 絶対パスが1つだけの場合はプレフィックス除去しない
        let input = "error: /Users/dev/project/src/main.rs:10: type error";
        let result = normalize_lint_output(input);
        assert!(result.contains("/Users/dev/project/src/main.rs"));
    }

    // === strip_ansi_codes の追加エッジケーステスト ===

    #[test]
    fn test_strip_ansi_codes_csi_only_bracket() {
        // ESC [ だけで入力が終了する場合
        let input = "before\x1b[";
        let result = strip_ansi_codes(input);
        assert_eq!(result, "before", "ESC [ のみでもパニックしないこと");
    }

    #[test]
    fn test_strip_ansi_codes_256color_sequence() {
        // 256色のCSIシーケンス（中間バイトが多い）
        let input = "\x1b[38;5;196mred text\x1b[0m";
        let result = strip_ansi_codes(input);
        assert_eq!(
            result, "red text",
            "256色CSIシーケンスが正しく除去されること"
        );
    }

    // === 繰り返し装飾文字の圧縮テスト ===

    #[test]
    fn test_collapse_repeated_chars_4_or_more() {
        assert_eq!(collapse_repeated_chars("===="), "=");
        assert_eq!(collapse_repeated_chars("...................."), ".");
        assert_eq!(collapse_repeated_chars("─────"), "─");
        assert_eq!(collapse_repeated_chars("━━━━━"), "━");
        assert_eq!(collapse_repeated_chars("------"), "-");
        assert_eq!(collapse_repeated_chars("······"), "·");
        assert_eq!(collapse_repeated_chars("→→→→→→"), "→");
        // ruff / clippy / rust の lint 出力で使われる範囲マーカー
        assert_eq!(collapse_repeated_chars("^^^^"), "^");
        assert_eq!(collapse_repeated_chars("^^^^^^^^^^^^"), "^");
    }

    #[test]
    fn test_collapse_repeated_chars_preserves_3_or_less() {
        // 行末以外の短い繰り返しは維持する
        assert_eq!(collapse_repeated_chars("Wait... what?"), "Wait... what?");
        assert_eq!(collapse_repeated_chars("---"), "---");
        assert_eq!(collapse_repeated_chars("=="), "==");
        assert_eq!(collapse_repeated_chars("text == value"), "text == value");
    }

    #[test]
    fn test_collapse_repeated_chars_compresses_trailing_ellipsis() {
        assert_eq!(
            collapse_repeated_chars("Using attach strategy..."),
            "Using attach strategy."
        );
        assert_eq!(collapse_repeated_chars("..."), ".");
    }

    #[test]
    fn test_collapse_repeated_chars_mixed_text() {
        assert_eq!(
            collapse_repeated_chars("= Starting migration ="),
            "= Starting migration ="
        );
        assert_eq!(
            collapse_repeated_chars("==== Starting migration ===="),
            "= Starting migration ="
        );
        assert_eq!(
            collapse_repeated_chars(".  63 / 212 ( 29%)"),
            ".  63 / 212 ( 29%)"
        );
    }

    #[test]
    fn test_collapse_repeated_chars_ignores_non_decorative() {
        // 装飾文字以外は対象外
        assert_eq!(collapse_repeated_chars("aaaaa"), "aaaaa");
        // `#` は markdown 見出し、`*` は markdown 強調と用法が重なるため装飾扱いしない
        assert_eq!(collapse_repeated_chars("#####"), "#####");
        assert_eq!(collapse_repeated_chars("*****"), "*****");
    }

    #[test]
    fn test_collapse_repeated_chars_arrow_marker() {
        // rustc の位置マーカー `-->` を `->` に圧縮
        assert_eq!(collapse_repeated_chars("-->"), "->");
        assert_eq!(collapse_repeated_chars("--->"), "->");
        assert_eq!(collapse_repeated_chars("---->"), "->");
        // 周囲に文字があっても矢印部分のみ圧縮
        assert_eq!(
            collapse_repeated_chars("  --> src/main.rs:10:5"),
            "  -> src/main.rs:10:5"
        );
    }

    #[test]
    fn test_collapse_repeated_chars_arrow_preserves_single_hyphen_arrow() {
        // 単一ハイフンの `->`（Rust の関数戻り型等）は圧縮対象外で保持
        assert_eq!(
            collapse_repeated_chars("fn foo() -> u32"),
            "fn foo() -> u32"
        );
        assert_eq!(collapse_repeated_chars("->"), "->");
    }

    #[test]
    fn test_collapse_repeated_chars_double_hyphen_not_followed_by_gt() {
        // `--` の直後が `>` でなければ通常ルール（4回以上で圧縮）が適用される
        assert_eq!(collapse_repeated_chars("--feature"), "--feature");
        assert_eq!(collapse_repeated_chars("a -- b"), "a -- b");
    }

    #[test]
    fn test_normalize_collapses_rustc_location_marker() {
        // E2E: rustc 出力に頻出する `--> path:line:col` が `-> path:line:col` になる
        let input = "error[E0308]: mismatched types\n  --> src/main.rs:10:5";
        let result = normalize_lint_output(input);
        assert_eq!(
            result,
            "error[E0308]: mismatched types\n-> src/main.rs:10:5"
        );
    }

    #[test]
    fn test_collapse_repeated_chars_caret_marker() {
        // 単体の caret や2-3個の連続は維持（XOR 演算子等の用法を破壊しない）
        assert_eq!(collapse_repeated_chars("a ^ b"), "a ^ b");
        assert_eq!(collapse_repeated_chars("^^^"), "^^^");
        // 行内の長い caret マーカーも圧縮対象
        assert_eq!(collapse_repeated_chars("| ^^^^^^"), "| ^");
    }

    #[test]
    fn test_collapse_repeated_chars_underscore_span_marker() {
        // rustc/ruff/biome のマルチライン span 下線 `| |_____^` を圧縮（範囲マーカー対策）
        assert_eq!(collapse_repeated_chars("| |_______^"), "| |_^");
        assert_eq!(
            collapse_repeated_chars("| |_______________________________^"),
            "| |_^"
        );
        // snake_case 識別子の区切り（1〜2 連続）は保持する
        assert_eq!(collapse_repeated_chars("MID_MONTH_DAY"), "MID_MONTH_DAY");
        assert_eq!(collapse_repeated_chars("__init__"), "__init__");
        // 3 連続以下は保持、4 連続以上で圧縮
        assert_eq!(collapse_repeated_chars("a___b"), "a___b");
        assert_eq!(collapse_repeated_chars("____"), "_");
    }

    #[test]
    fn test_collapse_repeated_chars_empty() {
        assert_eq!(collapse_repeated_chars(""), "");
    }

    #[test]
    fn test_normalize_collapses_long_decorative_chars() {
        // E2E: normalize_lint_output で装飾文字が圧縮される
        let input = "error\n============================\ntype mismatch\n....................";
        let result = normalize_lint_output(input);
        assert_eq!(result, "error\n=\ntype mismatch\n.");
    }

    #[test]
    fn test_normalize_progress_bar_compression() {
        let input = "Using attach strategy to execute scripts...\n==== Starting migration for: 3.8.0.rc001 ====\n...............................................................  63 / 212 ( 29%)";
        let result = normalize_lint_output(input);
        assert_eq!(
            result,
            "Using attach strategy to execute scripts.\n= Starting migration for: 3.8.0.rc001 =\n. 63 / 212 ( 29%)"
        );
    }

    #[test]
    fn test_normalize_biome_box_drawing_separator() {
        let input = "biome.jsonc:2:13 deserialize ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\n  i The configuration schema version does not match the CLI version 2.4.13";
        let result = normalize_lint_output(input);
        assert_eq!(
            result,
            "biome.jsonc:2:13 deserialize ━\n\ni The configuration schema version does not match the CLI version 2.4.13"
        );
    }

    // === 診断枠線（frame line）除去テスト ===

    #[test]
    fn test_is_diagnostic_frame_line_basic() {
        // 枠線・キャレットのみの行は frame line
        assert!(is_diagnostic_frame_line("|"));
        assert!(is_diagnostic_frame_line("│"));
        assert!(is_diagnostic_frame_line("| ^"));
        assert!(is_diagnostic_frame_line("│ ^"));
        assert!(is_diagnostic_frame_line("^"));
        assert!(is_diagnostic_frame_line("| | ^"));
        // Box Drawing 文字のみのバナー枠・区切り線も frame line
        assert!(is_diagnostic_frame_line("╭─╮"));
        assert!(is_diagnostic_frame_line("╰─╯"));
        assert!(is_diagnostic_frame_line("│ │"));
        assert!(is_diagnostic_frame_line("┌──┐"));
        assert!(is_diagnostic_frame_line("└──┘"));
        assert!(is_diagnostic_frame_line("─"));
        assert!(is_diagnostic_frame_line("━"));
        // rustc/clippy のマルチライン span 下線 `| |_____^` が圧縮された後の
        // `| |_^` のようなアンダースコア装飾行も frame line
        assert!(is_diagnostic_frame_line("| |_^"));
        assert!(is_diagnostic_frame_line("_"));
        // 英数字を含む行は frame line ではない
        assert!(!is_diagnostic_frame_line("| ^ expected u32"));
        assert!(!is_diagnostic_frame_line("10 | let x = 1;"));
        assert!(!is_diagnostic_frame_line("error: foo"));
        assert!(!is_diagnostic_frame_line("│ Update available! │"));
        // アンダースコアを含んでも英数字があれば frame line ではない（snake_case 等）
        assert!(!is_diagnostic_frame_line("__init__"));
        assert!(!is_diagnostic_frame_line("10 | foo_bar();"));
        // `-` を含む Markdown テーブル区切りは保持対象
        assert!(!is_diagnostic_frame_line("| --- |"));
        // 空文字列は対象外
        assert!(!is_diagnostic_frame_line(""));
    }

    #[test]
    fn test_normalize_removes_pnpm_update_banner_frame() {
        // pnpm の更新通知バナー。枠線行（╭─╮ / │ │ / ╰─╯）は除去され、
        // 本文行（│ Update available! ... │）は保持される。
        let input = "Stop hook failed: pnpm exec tsc --noEmit\n╭─────────────────────────────╮\n│                             │\n│ Update available! 11.5.2 → 11.5.3. │\n│ To update, run: corepack use pnpm@11.5.3 │\n│                             │\n╰─────────────────────────────╯\nsrc/app.ts(1,1): error TS2304";
        let result = normalize_lint_output(input);
        assert!(
            !result.contains('╭'),
            "top frame should be removed: {result}"
        );
        assert!(
            !result.contains('╰'),
            "bottom frame should be removed: {result}"
        );
        assert!(
            result.contains("Update available!"),
            "banner body should be kept: {result}"
        );
        assert!(
            result.contains("error TS2304"),
            "diagnostics should be kept: {result}"
        );
    }

    #[test]
    fn test_normalize_collapses_cargo_blocking_lines() {
        // cargo がロック待ち中に繰り返す Blocking 行は4行目以降が集約される
        let input = "Blocking waiting for file lock on package cache\nBlocking waiting for file lock on package cache\nBlocking waiting for file lock on package cache\nBlocking waiting for file lock on package cache\nBlocking waiting for file lock on build directory\nerror: build failed";
        let result = normalize_lint_output(input);
        let blocking_count = result.matches("Blocking waiting").count();
        assert_eq!(
            blocking_count, 3,
            "only first 3 Blocking lines kept: {result}"
        );
        assert!(
            result.contains("and 2 more lines starting with \"Blocking\""),
            "{result}"
        );
        assert!(result.contains("error: build failed"), "{result}");
    }

    #[test]
    fn test_normalize_removes_ruff_caret_frame_lines() {
        // ruff スタイルの診断ブロック。`|` 区切り行と `| ^` キャレット行は除去され、
        // 見出し・位置ヘッダ・コード行は保持される。
        let input = "F401 unused import\n--> src/foo.py:1:8\n|\n1 | import os\n| ^^^^^^^^^\n|";
        let result = normalize_lint_output(input);
        assert_eq!(
            result,
            "F401 unused import\n-> src/foo.py:1:8\n1 | import os"
        );
    }

    #[test]
    fn test_normalize_removes_biome_pipe_frame_lines() {
        // biome の `│` 単独行・キャレット行を除去する。
        let input = "src/main.ts:1:7 lint/correctness/noUnusedVariables\n│\n1 │ const x = 1;\n│ ^";
        let result = normalize_lint_output(input);
        assert_eq!(
            result,
            "src/main.ts:1:7 lint/correctness/noUnusedVariables\n1 │ const x = 1;"
        );
    }

    #[test]
    fn test_normalize_removes_ruff_ruleset_incompatibility_warning() {
        // ruff が毎回出力する「ルール同士が非互換」設定警告は除去し、
        // 実際のコード診断は保持する。
        let input = "warning: `incorrect-blank-line-before-class` (D203) and `no-blank-line-before-class` (D211) are incompatible. Ignoring `incorrect-blank-line-before-class`.\n\
warning: `multi-line-summary-first-line` (D212) and `multi-line-summary-second-line` (D213) are incompatible. Ignoring `multi-line-summary-second-line`.\n\
undocumented-public-module: Missing docstring in public module\n\
--> probe.py:1:1";
        let result = normalize_lint_output(input);
        assert!(
            !result.contains("are incompatible"),
            "ruleset incompatibility warnings should be removed: {result:?}"
        );
        assert!(
            result.contains("undocumented-public-module: Missing docstring in public module"),
            "code diagnostics must be preserved: {result:?}"
        );
    }

    #[test]
    fn test_normalize_preserves_real_code_warning() {
        // 「warning:」で始まっても、コード自体の診断（非互換設定警告ではない）は保持する。
        let input = "warning: unused variable: `x`\n--> src/main.rs:2:9";
        let result = normalize_lint_output(input);
        assert!(
            result.contains("warning: unused variable: `x`"),
            "real code warnings must be preserved: {result:?}"
        );
    }

    #[test]
    fn test_normalize_preserves_caret_line_with_label() {
        // ラベル付きキャレット行（rustc）はテキストを含むため保持する。
        let input = "error[E0308]: mismatched types\n--> src/main.rs:10:18\n|\n10 | let x: u32 = \"hi\";\n| ^^^^ expected u32, found &str";
        let result = normalize_lint_output(input);
        // `|` 単独行は除去、ラベル付きキャレット行は保持
        assert!(!result.contains("\n|\n"));
        assert!(result.contains("expected u32, found &str"));
        assert!(result.contains("10 | let x: u32 = \"hi\";"));
    }

    #[test]
    fn test_normalize_preserves_pipe_in_content_lines() {
        // パイプを含むが英数字もある行（テーブル・コード）は保持する。
        let input = "| Col A | Col B |\n| --- | --- |\nlet r = a | b;";
        let result = normalize_lint_output(input);
        assert!(result.contains("| Col A | Col B |"));
        assert!(result.contains("| --- | --- |"));
        assert!(result.contains("let r = a | b;"));
    }

    // === パスコンテキスト置換テスト ===

    #[test]
    fn test_strip_common_path_prefix_context_only() {
        // プレフィックス直後にパス文字が続く場合のみ除去
        let input =
            "/home/user/GitHub/project/src/main.rs:10\n/home/user/GitHub/project/src/lib.rs:20";
        let result = strip_common_path_prefix(input);
        assert_eq!(result, "main.rs:10\nlib.rs:20");
    }

    #[test]
    fn test_strip_common_path_prefix_non_path_context_preserved() {
        // プレフィックスの直後がスペースや改行等の場合は除去しない
        // このケースでは2パス抽出 → 共通プレフィックスは /home/user/GitHub/project/
        // 「see /home/user/GitHub/project/ 」の直後がスペースなので除去しない
        let input = "/home/user/GitHub/project/src/main.rs:10\n/home/user/GitHub/project/tests/test.rs:20\nsee /home/user/GitHub/project/ for details";
        let result = strip_common_path_prefix(input);
        assert!(result.contains("see /home/user/GitHub/project/ for details"));
        assert!(result.contains("src/main.rs:10"));
    }

    #[test]
    fn test_strip_common_path_prefix_embedded_substring_preserved() {
        // 共通プレフィックスが非パス文字列中の偶発的な部分文字列として現れた場合、
        // 中身を破壊してはいけない（直前が英数字なのでパス境界ではない）。
        // 回帰テスト: かつては直前文字を検査せず除去し、「foo/foo/bar/extra」が
        // 「fooextra」に破壊された。
        let input = "/foo/bar/main.rs:10\n/foo/bar/lib.rs:20\nfoo/foo/bar/extra";
        let result = strip_common_path_prefix(input);
        assert!(
            result.contains("foo/foo/bar/extra"),
            "非パス文脈の部分文字列が破壊された: {}",
            result
        );
        assert!(result.contains("main.rs:10"));
        assert!(result.contains("lib.rs:20"));
    }

    // === collapse_repeated_chars 追加の境界条件 ===

    #[test]
    fn test_collapse_repeated_chars_trailing_ellipsis_with_following_text_after_newline() {
        // 「行末」判定はあくまで chars.peek() == None なので、行が連続しても分割は呼び出し側で行う前提
        // collapse_repeated_chars 単体では \n を含む単一文字列を扱うことは想定しないが、防御的に確認
        let input = "first... second";
        // chars.peek() は次に space があるため、3点リーダは行末と判定されない
        assert_eq!(collapse_repeated_chars(input), "first... second");
    }

    #[test]
    fn test_collapse_repeated_chars_dots_at_start() {
        // 行頭の3点リーダは行末ではないので維持される
        assert_eq!(collapse_repeated_chars("...text"), "...text");
    }

    #[test]
    fn test_collapse_repeated_chars_only_dots_3() {
        // 単独で3点 → chars.peek() == None なので行末扱い
        assert_eq!(collapse_repeated_chars("..."), ".");
    }

    #[test]
    fn test_collapse_repeated_chars_two_dots_preserved() {
        // 2点は装飾文字判定の閾値（4以上 or 行末3以上）に達しないため維持
        assert_eq!(collapse_repeated_chars(".."), "..");
    }

    #[test]
    fn test_collapse_repeated_chars_horizontal_box_drawing() {
        // 罫線文字 ─ ━ の長い連続が圧縮される
        assert_eq!(collapse_repeated_chars("title\n─────"), "title\n─");
        assert_eq!(collapse_repeated_chars("━━━━━━━━━━ end"), "━ end");
    }

    // === normalize_lint_output: ANSI + パス + 装飾の複合 ===

    #[test]
    fn test_normalize_combines_ansi_path_and_decoration_compression() {
        // 実際の lint 出力に近い複合ケース
        let input = "\x1b[31m/home/user/GitHub/project/src/main.rs:10:5 error\x1b[0m\n\
            ===== summary =====\n\
            /home/user/GitHub/project/src/lib.rs:20 warning";
        let result = normalize_lint_output(input);
        // ANSI 除去
        assert!(!result.contains('\x1b'));
        // パスプレフィックス除去（共通部分は /home/user/GitHub/project/src/ なのでファイル名のみ残る）
        assert!(result.contains("main.rs:10:5"));
        assert!(result.contains("lib.rs:20"));
        assert!(!result.contains("/home/user/GitHub/"));
        // 装飾文字圧縮
        assert!(result.contains("= summary ="));
        assert!(!result.contains("====="));
    }

    // === truncate_output 追加: 既存のサフィックスを含む入力 ===

    #[test]
    fn test_truncate_output_input_already_contains_truncated_suffix() {
        // 既に切り詰めサフィックスを含む入力でも、長さ制限さえ守れば正しく動く
        let input = format!("{}\n... (truncated)", "a".repeat(2000));
        let result = truncate_output(&input, 100);
        assert!(result.chars().count() <= 100);
        assert!(result.ends_with("... (truncated)"));
    }

    // === strip_common_path_prefix 追加: マルチバイトパスでも文字境界を維持 ===

    #[test]
    fn test_strip_common_path_prefix_multibyte_path_preserves_char_boundary() {
        // マルチバイトのディレクトリ名を含む共通プレフィックスでも UTF-8 境界を壊さない
        let input = "/Users/田中/proj/src/main.rs:10 error\n/Users/田中/proj/src/lib.rs:20 warning";
        let result = strip_common_path_prefix(input);
        // 元の入力は有効な UTF-8 で、結果も有効な UTF-8 のまま
        assert!(result.is_ascii() || result.chars().count() > 0);
        assert!(result.contains("main.rs:10"));
        assert!(result.contains("lib.rs:20"));
    }

    #[test]
    fn test_strip_common_path_prefix_paths_with_spaces() {
        // スペースを含むディレクトリ名でもファイル単位の共通プレフィックスを除去する
        let input = "/Users/dev/My Project/src/main.rs:10 error\n/Users/dev/My Project/src/lib.rs:20 warning";
        let result = strip_common_path_prefix(input);
        assert_eq!(result, "main.rs:10 error\nlib.rs:20 warning");
    }

    #[test]
    fn test_extract_absolute_paths_ignores_urls() {
        // URL のスラッシュは絶対パスとして扱わない
        let input = "see https://example.com/a/b and /Users/dev/project/src/main.rs:10";
        let paths = extract_absolute_paths(input);
        assert_eq!(paths, vec!["/Users/dev/project/src/main.rs"]);
    }

    // === collapse_repeated_prefix_lines: 同じ単語で始まる行の集約 ===

    #[test]
    fn test_collapse_repeated_prefix_lines_compiling() {
        // cargo の Compiling 連発を 4 行目以降で集約する
        let lines = vec![
            "Compiling foo v1.0.0".to_string(),
            "Compiling bar v2.0.0".to_string(),
            "Compiling baz v3.0.0".to_string(),
            "Compiling qux v4.0.0".to_string(),
            "Compiling fred v5.0.0".to_string(),
            "error: failed".to_string(),
        ];
        let result = collapse_repeated_prefix_lines(lines);
        assert_eq!(result.len(), 5);
        assert_eq!(result[0], "Compiling foo v1.0.0");
        assert_eq!(result[1], "Compiling bar v2.0.0");
        assert_eq!(result[2], "Compiling baz v3.0.0");
        assert_eq!(
            result[3],
            "... (and 2 more lines starting with \"Compiling\")"
        );
        assert_eq!(result[4], "error: failed");
    }

    #[test]
    fn test_collapse_repeated_prefix_lines_three_or_fewer_preserved() {
        // 3行以下は集約せずそのまま保持する
        let lines = vec![
            "Compiling foo v1.0.0".to_string(),
            "Compiling bar v2.0.0".to_string(),
            "Compiling baz v3.0.0".to_string(),
            "error: failed".to_string(),
        ];
        let result = collapse_repeated_prefix_lines(lines.clone());
        assert_eq!(result, lines);
    }

    #[test]
    fn test_collapse_repeated_prefix_lines_mixed_progress_words_collapsed() {
        // 異なる進捗語（Compiling/Downloading/Checking 等）が混在しても、
        // 進捗語の連続ランとして 1 つにまとめて集約する。
        // cargo の並行ビルドは Compiling と Checking を交互に出力するため、
        // 同一語に限定すると交互出力がほとんど集約されずに残ってしまう。
        let lines = vec![
            "Compiling foo".to_string(),
            "Downloading bar".to_string(),
            "Checking baz".to_string(),
            "Compiling qux".to_string(),
            "Checking quux".to_string(),
            "error: failed".to_string(),
        ];
        let result = collapse_repeated_prefix_lines(lines);
        assert_eq!(result.len(), 5);
        assert_eq!(result[0], "Compiling foo");
        assert_eq!(result[1], "Downloading bar");
        assert_eq!(result[2], "Checking baz");
        // 混在ランは汎用メッセージで要約する
        assert_eq!(result[3], "... (and 2 more progress lines)");
        assert_eq!(result[4], "error: failed");
    }

    #[test]
    fn test_collapse_repeated_prefix_lines_mixed_three_or_fewer_preserved() {
        // 混在ランでも 3 行以下なら集約せず保持する
        let lines = vec![
            "Compiling foo".to_string(),
            "Downloading bar".to_string(),
            "Checking baz".to_string(),
        ];
        let result = collapse_repeated_prefix_lines(lines.clone());
        assert_eq!(result, lines);
    }

    #[test]
    fn test_collapse_repeated_prefix_lines_no_word_prefix_skipped() {
        // 行頭が記号や数字の場合は集約しない（誤検知防止）
        let lines = vec![
            "1. step one".to_string(),
            "2. step two".to_string(),
            "3. step three".to_string(),
            "4. step four".to_string(),
        ];
        let result = collapse_repeated_prefix_lines(lines.clone());
        assert_eq!(result, lines);
    }

    #[test]
    fn test_normalize_collapses_cargo_compiling_lines() {
        // 実際の cargo 出力に近い複合ケース
        let input = "Compiling tauri v2.10.3\n\
                     Compiling clang-sys v1.8.1\n\
                     Compiling prettyplease v0.2.37\n\
                     Compiling minimal-lexical v0.2.1\n\
                     Compiling libloading v0.8.9\n\
                     error: failed to run custom build command";
        let result = normalize_lint_output(input);
        // 最初の3行は維持
        assert!(result.contains("Compiling tauri"));
        assert!(result.contains("Compiling clang-sys"));
        assert!(result.contains("Compiling prettyplease"));
        // 4行目以降は集約
        assert!(result.contains("... (and 2 more lines starting with \"Compiling\")"));
        // エラー行は維持
        assert!(result.contains("error: failed to run custom build command"));
    }

    #[test]
    fn test_leading_progress_prefix_diagnostics_not_matched() {
        // 診断行（error:/warning:）は進捗系ではないため集約対象にしない（情報欠落防止）。
        assert_eq!(leading_progress_prefix("error: something"), None);
        assert_eq!(leading_progress_prefix("warning: bar"), None);
        // コロン区切りでも進捗系の単語なら認識する
        assert_eq!(leading_progress_prefix("Compiling foo"), Some("Compiling"));
    }

    #[test]
    fn test_leading_progress_prefix_single_char_skipped() {
        // 1文字の単語は集約しない
        assert_eq!(leading_progress_prefix("a foo"), None);
    }

    #[test]
    fn test_leading_progress_prefix_no_word() {
        // 行頭が英字でない場合は None
        assert_eq!(leading_progress_prefix("123 foo"), None);
        assert_eq!(leading_progress_prefix("--> file.rs"), None);
        assert_eq!(leading_progress_prefix(""), None);
    }

    // === collapse_space_separated_decorative テスト ===

    #[test]
    fn test_collapse_space_separated_arrow_compresses_long_run() {
        // 4回以上のスペース区切り `→` パターンが圧縮される
        assert_eq!(
            collapse_space_separated_decorative("→ → → → Google"),
            "→ Google"
        );
        assert_eq!(
            collapse_space_separated_decorative("→ → → → → → → Google"),
            "→ Google"
        );
    }

    #[test]
    fn test_collapse_space_separated_middot_compresses_long_run() {
        // `·` も同じく 4 回以上の連続で圧縮
        assert_eq!(
            collapse_space_separated_decorative("text · · · · · end"),
            "text · end"
        );
    }

    #[test]
    fn test_collapse_space_separated_preserves_short_runs() {
        // 3 個以下はそのまま残る
        assert_eq!(
            collapse_space_separated_decorative("→ → → end"),
            "→ → → end"
        );
        assert_eq!(collapse_space_separated_decorative("→ end"), "→ end");
        assert_eq!(collapse_space_separated_decorative("· · ·"), "· · ·");
    }

    #[test]
    fn test_collapse_space_separated_at_end_of_line() {
        // 圧縮対象が行末で終わる場合
        assert_eq!(collapse_space_separated_decorative("→ → → → →"), "→");
        assert_eq!(collapse_space_separated_decorative("a → → → →"), "a →");
    }

    #[test]
    fn test_collapse_space_separated_not_decorative_char_unchanged() {
        // 対象外の文字 (例: `-`) はこの関数では圧縮しない
        assert_eq!(
            collapse_space_separated_decorative("- - - - end"),
            "- - - - end"
        );
    }

    #[test]
    fn test_collapse_space_separated_multiple_spaces_not_collapsed() {
        // スペースが 2 個以上ある場合は別の関数の責任 (collapse_whitespace 後を想定)
        // この関数では単一スペースのパターンのみを対象とする
        let input = "→  →  →  →"; // 2スペース区切り
        // パターンが一致しないのでそのまま返る
        assert_eq!(collapse_space_separated_decorative(input), input);
    }

    #[test]
    fn test_normalize_collapses_biome_arrow_diff() {
        // biome の diff 行で `→ → → → → → →` のような可視化が圧縮される
        let input = "293 │ - → → → → → → → Google";
        let result = normalize_lint_output(input);
        assert_eq!(result, "293 │ - → Google");
    }

    #[test]
    fn test_normalize_collapses_biome_middot_diff() {
        // biome の diff 行で `·` の可視化が圧縮される
        let input = "5 │ - → → → → :·\"text-gray-400·dark:text-gray-500\"";
        let result = normalize_lint_output(input);
        // → の連続が圧縮される。·は単独・連続2つなので維持。
        assert!(result.contains("→ :·"));
        assert!(!result.contains("→ → → → :"));
    }

    #[test]
    fn test_collapse_space_separated_handles_empty_input() {
        assert_eq!(collapse_space_separated_decorative(""), "");
    }

    #[test]
    fn test_collapse_space_separated_exactly_four() {
        // ちょうど 4 個でも圧縮対象 (4 回以上)
        assert_eq!(collapse_space_separated_decorative("→ → → →"), "→");
        assert_eq!(collapse_space_separated_decorative("· · · ·"), "·");
    }

    #[test]
    fn test_collapse_space_separated_three_preserved() {
        // 3 個は閾値未満で維持
        assert_eq!(collapse_space_separated_decorative("→ → →"), "→ → →");
    }

    #[test]
    fn test_collapse_space_separated_pattern_inside_text() {
        // 前後にテキストがあっても圧縮対象の文字パターンが認識される
        assert_eq!(
            collapse_space_separated_decorative("text → → → → → more"),
            "text → more"
        );
    }

    #[test]
    fn test_collapse_space_separated_mixed_decorative() {
        // 異なる装飾文字が交互に並ぶ場合はそれぞれ独立して扱う
        let input = "→ → → → · · · · end";
        let result = collapse_space_separated_decorative(input);
        assert_eq!(result, "→ · end");
    }

    #[test]
    fn test_collapse_space_separated_pattern_followed_by_decorative() {
        // 圧縮後に直接同じ装飾文字が続かないケース
        let input = "→ → → → a";
        assert_eq!(collapse_space_separated_decorative(input), "→ a");
    }

    #[test]
    fn test_collapse_space_separated_unicode_safe() {
        // マルチバイト文字を含む文字列でも UTF-8 境界を壊さない
        let input = "あ → → → → い";
        let result = collapse_space_separated_decorative(input);
        // 「あ → い」の形になり、result も有効な UTF-8
        assert_eq!(result, "あ → い");
        assert!(result.is_char_boundary(0));
        assert!(result.is_char_boundary(result.len()));
    }

    #[test]
    fn test_collapse_space_separated_no_pattern_passthrough() {
        // パターンに一致しないテキストはそのまま
        assert_eq!(
            collapse_space_separated_decorative("hello world"),
            "hello world"
        );
        // 単一の装飾文字
        assert_eq!(collapse_space_separated_decorative("→"), "→");
        assert_eq!(collapse_space_separated_decorative("·"), "·");
    }

    // === 既存ロジックのエッジケース補強 ===

    #[test]
    fn test_collapse_repeated_chars_arrow_chain_long_prefix() {
        // `----->` のようなさらに長いプレフィックスも `->` に圧縮される
        assert_eq!(collapse_repeated_chars("------>"), "->");
        assert_eq!(collapse_repeated_chars("text ----> end"), "text -> end");
    }

    #[test]
    fn test_collapse_repeated_chars_only_dot_run_at_end() {
        // 行末 3 連続 `.` の圧縮 (`text...` → `text.`)
        assert_eq!(collapse_repeated_chars("text..."), "text.");
        // 4 連続 `.` も圧縮
        assert_eq!(collapse_repeated_chars("....more"), ".more");
    }

    #[test]
    fn test_normalize_keeps_short_decorative_lines() {
        // 装飾文字数が閾値未満の行はそのまま出力される
        let input = "title\n---\n=== sub ===\ntext";
        let result = normalize_lint_output(input);
        assert_eq!(result, "title\n---\n=== sub ===\ntext");
    }

    #[test]
    fn test_normalize_handles_carriage_return_lines() {
        // CR を含む行も `\n` ベースで処理される (split による分割)
        let input = "line1\r\nline2\r\nline3";
        let result = normalize_lint_output(input);
        // CR は通常文字として残るが、トリミングで除去される (\r は trim 対象)
        assert!(result.contains("line1"));
        assert!(result.contains("line2"));
        assert!(result.contains("line3"));
    }

    #[test]
    fn test_truncate_output_does_not_split_multibyte() {
        // マルチバイト文字境界で切り詰めても UTF-8 境界を壊さない
        let input = "あいうえおかきくけこ"; // 10文字
        let result = truncate_output(input, 5);
        // 5 文字以下に切り詰められ、UTF-8 として有効
        assert!(result.chars().count() <= 5);
        assert!(result.is_char_boundary(0));
        assert!(result.is_char_boundary(result.len()));
    }

    #[test]
    fn test_strip_common_path_prefix_paths_in_quotes() {
        // 引用符で囲まれた絶対パスでも、前置文字 (`"`、`'`) は path 開始位置として
        // 認識されないので元の入力どおりに維持される
        let input = "see \"/home/user/proj/main.rs\" and \"/home/user/proj/lib.rs\"";
        let result = strip_common_path_prefix(input);
        // パスは絶対パスとして抽出されないため、共通プレフィックスも除去されない
        assert_eq!(result, input);
    }

    #[test]
    fn test_collapse_repeated_prefix_lines_empty_input() {
        // 空入力でもパニックしない
        let lines: Vec<String> = vec![];
        let result = collapse_repeated_prefix_lines(lines);
        assert!(result.is_empty());
    }

    // === collapse_duplicate_diff_context_line_number: biome の重複行番号圧縮 ===

    #[test]
    fn test_collapse_duplicate_diff_context_line_number_same_numbers() {
        // X == Y のとき X に圧縮される
        assert_eq!(
            collapse_duplicate_diff_context_line_number("129 129 │ data"),
            "129 │ data"
        );
        assert_eq!(
            collapse_duplicate_diff_context_line_number("1 1 │ {"),
            "1 │ {"
        );
    }

    #[test]
    fn test_collapse_duplicate_diff_context_line_number_different_numbers_preserved() {
        // X != Y のときは情報として保持し圧縮しない
        assert_eq!(
            collapse_duplicate_diff_context_line_number("10 9 │ data"),
            "10 9 │ data"
        );
        assert_eq!(
            collapse_duplicate_diff_context_line_number("100 200 │ context"),
            "100 200 │ context"
        );
    }

    #[test]
    fn test_collapse_duplicate_diff_context_line_number_single_number_preserved() {
        // 行番号が片方のみ (削除/追加行) はそのまま保持
        assert_eq!(
            collapse_duplicate_diff_context_line_number("131 │ - text"),
            "131 │ - text"
        );
        assert_eq!(
            collapse_duplicate_diff_context_line_number("132 │ + text"),
            "132 │ + text"
        );
    }

    #[test]
    fn test_collapse_duplicate_diff_context_line_number_no_pipe_separator() {
        // `│` がない行はそのまま保持
        assert_eq!(
            collapse_duplicate_diff_context_line_number("129 129 data"),
            "129 129 data"
        );
        assert_eq!(
            collapse_duplicate_diff_context_line_number("plain text"),
            "plain text"
        );
    }

    #[test]
    fn test_collapse_duplicate_diff_context_line_number_non_digit_preserved() {
        // 数値以外のトークン (例: `a a │`) は誤検知防止のため圧縮しない
        assert_eq!(
            collapse_duplicate_diff_context_line_number("a a │ text"),
            "a a │ text"
        );
        assert_eq!(
            collapse_duplicate_diff_context_line_number("- - │ text"),
            "- - │ text"
        );
    }

    #[test]
    fn test_collapse_duplicate_diff_context_line_number_three_tokens_preserved() {
        // 数値が3つ以上ある場合 (想定外フォーマット) は圧縮しない
        assert_eq!(
            collapse_duplicate_diff_context_line_number("1 1 1 │ text"),
            "1 1 1 │ text"
        );
    }

    #[test]
    fn test_collapse_duplicate_diff_context_line_number_empty_text_preserved() {
        // 行末が `│ ` で終わるケース (text が空) も圧縮対象
        assert_eq!(
            collapse_duplicate_diff_context_line_number("42 42 │ "),
            "42 │ "
        );
        assert_eq!(
            collapse_duplicate_diff_context_line_number("42 42 │"),
            "42 │"
        );
    }

    #[test]
    fn test_normalize_collapses_biome_diff_context_lines_e2e() {
        // E2E: biome 形式の diff で context line のみ重複行番号が圧縮される
        let input = "131 │ - → log(\n132 │ + → log(\n129 129 │ data\n130 130 │ });\n131 131 │";
        let result = normalize_lint_output(input);
        assert_eq!(
            result,
            "131 │ - → log(\n132 │ + → log(\n129 │ data\n130 │ });\n131 │"
        );
    }

    #[test]
    fn test_collapse_repeated_prefix_lines_single_long_run() {
        // 同じ単語の長い連続のみ
        let lines = vec![
            "Building foo".to_string(),
            "Building bar".to_string(),
            "Building baz".to_string(),
            "Building qux".to_string(),
            "Building fred".to_string(),
            "Building waldo".to_string(),
        ];
        let result = collapse_repeated_prefix_lines(lines);
        assert_eq!(result.len(), 4);
        assert_eq!(
            result[3],
            "... (and 3 more lines starting with \"Building\")"
        );
    }

    #[test]
    fn test_collapse_repeated_prefix_lines_does_not_collapse_warnings() {
        // 診断行（warning:）は固有情報を持つため、同じ単語で始まり 4 行以上
        // 連続しても集約してはならない（4 行目以降のエラー内容の欠落を防ぐ）。
        let lines = vec![
            "warning: unused import foo".to_string(),
            "warning: unused import bar".to_string(),
            "warning: unused import baz".to_string(),
            "warning: unused import qux".to_string(),
            "warning: unused import quux".to_string(),
        ];
        let result = collapse_repeated_prefix_lines(lines.clone());
        // 全行が保持され、集約行は挿入されない
        assert_eq!(result, lines);
    }

    #[test]
    fn test_collapse_repeated_prefix_lines_does_not_collapse_errors() {
        // error: で始まる異なるエラーが連続しても集約しない（情報欠落防止）
        let lines = vec![
            "error: mismatched types".to_string(),
            "error: cannot find value x".to_string(),
            "error: borrow of moved value".to_string(),
            "error: unused variable y".to_string(),
        ];
        let result = collapse_repeated_prefix_lines(lines.clone());
        assert_eq!(result, lines);
    }

    #[test]
    fn test_collapse_repeated_prefix_lines_ignores_non_progress_words() {
        // 進捗系ホワイトリスト外の単語は集約対象にしない
        let lines = vec![
            "Deleting alpha".to_string(),
            "Deleting beta".to_string(),
            "Deleting gamma".to_string(),
            "Deleting delta".to_string(),
        ];
        let result = collapse_repeated_prefix_lines(lines.clone());
        assert_eq!(result, lines);
    }

    #[test]
    fn test_normalize_collapses_progress_lines_e2e() {
        // E2E: 進捗系（Compiling）が 4 行以上連続したら 4 行目以降を集約する
        let input = "Compiling a v1.0\nCompiling b v1.0\nCompiling c v1.0\nCompiling d v1.0\nCompiling e v1.0";
        let result = normalize_lint_output(input);
        assert!(result.contains("Compiling a v1.0"));
        assert!(result.contains("Compiling c v1.0"));
        assert!(result.contains("and 2 more lines starting with \"Compiling\""));
        // 4・5 行目の生テキストは集約される
        assert!(!result.contains("Compiling d v1.0"));
        assert!(!result.contains("Compiling e v1.0"));
    }

    #[test]
    fn test_normalize_collapses_interleaved_cargo_progress_e2e() {
        // E2E: cargo の並行ビルドは Compiling と Checking を交互に出力する。
        // 異なる進捗語が混在していても 1 つのランとして 4 行目以降を集約する。
        let input = "Compiling libc v0.2\nChecking cfg-if v1.0\nCompiling quote v1.0\nChecking bytes v1.0\nCompiling serde v1.0\nerror: build failed";
        let result = normalize_lint_output(input);
        // 最初の3行は維持
        assert!(result.contains("Compiling libc v0.2"));
        assert!(result.contains("Checking cfg-if v1.0"));
        assert!(result.contains("Compiling quote v1.0"));
        // 混在ランは汎用メッセージで要約
        assert!(result.contains("and 2 more progress lines"));
        // 4・5 行目の生テキストは集約される
        assert!(!result.contains("Checking bytes v1.0"));
        assert!(!result.contains("Compiling serde v1.0"));
        // 診断行は保持される
        assert!(result.contains("error: build failed"));
    }

    #[test]
    fn test_normalize_removes_compressed_multiline_span_underline() {
        // E2E: rustc/clippy のマルチライン span 下線 `| |_______^` は
        // `| |_^` に圧縮された後、枠線行として除去される。
        // 見出し・位置ヘッダ・ソース行・ラベル付き行は保持される。
        let input = "error[E0308]: mismatched types\n--> src/main.rs:10:18\n|\n10 | let x = foo(\n| |_______^\n| | expected u32";
        let result = normalize_lint_output(input);
        assert!(
            !result.contains("| |_^"),
            "span underline should be removed: {result}"
        );
        assert!(result.contains("error[E0308]: mismatched types"));
        assert!(result.contains("-> src/main.rs:10:18"));
        assert!(result.contains("10 | let x = foo("));
        // ラベル付きキャレット行（英数字を含む）は保持
        assert!(result.contains("expected u32"));
    }

    #[test]
    fn test_dedup_explanation_lines_biome_info() {
        // biome は同一ルールの違反ごとに同じ `i` 説明文を繰り返す。
        // 2回目以降は除去され、除去で生じた連続空行は1行に再圧縮される。
        let lines: Vec<String> = [
            "src/a.ts:1:18 lint/suspicious/noExplicitAny",
            "! Unexpected any. Specify a different type.",
            "",
            "i any disables many type checking rules. Its use should be avoided.",
            "",
            "src/a.ts:3:32 lint/suspicious/noExplicitAny",
            "! Unexpected any. Specify a different type.",
            "",
            "i any disables many type checking rules. Its use should be avoided.",
            "",
            "Found 2 warnings.",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let result = dedup_repeated_explanation_lines(lines);
        let joined = result.join("\n");
        assert_eq!(
            joined.matches("i any disables").count(),
            1,
            "duplicate biome info line should be removed: {joined}"
        );
        assert!(!joined.contains("\n\n\n"), "no double blank: {joined}");
        // 診断ヘッダと本文行は出現ごとに保持される
        assert_eq!(joined.matches("lint/suspicious/noExplicitAny").count(), 2);
        assert_eq!(joined.matches("! Unexpected any").count(), 2);
    }

    #[test]
    fn test_dedup_explanation_lines_ruff_help() {
        // ruff は同種違反ごとに同じ `help:` 行を繰り返す。逐語一致のみ除去し、
        // 内容が異なる help 行は保持する。
        let lines: Vec<String> = [
            "missing-whitespace-around-operator: [*] Missing whitespace around operator",
            "-> src/a.py:7:10",
            "help: Add missing whitespace",
            "missing-whitespace-around-operator: [*] Missing whitespace around operator",
            "-> src/a.py:9:10",
            "help: Add missing whitespace",
            "docstring-missing-returns: `return` is not documented in docstring",
            "-> src/a.py:2:5",
            "help: Add a \"Returns\" section to the docstring",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let result = dedup_repeated_explanation_lines(lines);
        let joined = result.join("\n");
        assert_eq!(joined.matches("help: Add missing whitespace").count(), 1);
        assert_eq!(joined.matches("help: Add a \"Returns\"").count(), 1);
        // 位置ヘッダは出現ごとに保持される
        assert!(joined.contains("-> src/a.py:7:10"));
        assert!(joined.contains("-> src/a.py:9:10"));
    }

    #[test]
    fn test_dedup_explanation_lines_clippy_help_url() {
        // clippy は lint 出現ごとに同じ `= help: for further information...` を繰り返す
        let lines: Vec<String> = [
            "warning: this expression creates a reference which is immediately dereferenced",
            "= help: for further information visit https://rust-lang.github.io/rust-clippy/master/index.html#needless_borrow",
            "warning: this expression creates a reference which is immediately dereferenced",
            "= help: for further information visit https://rust-lang.github.io/rust-clippy/master/index.html#needless_borrow",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let result = dedup_repeated_explanation_lines(lines);
        let joined = result.join("\n");
        assert_eq!(joined.matches("= help: for further information").count(), 1);
        // 診断見出し行は説明行ではないため重複していても保持される
        assert_eq!(joined.matches("warning: this expression").count(), 2);
    }

    #[test]
    fn test_dedup_explanation_lines_preserves_identical_source_lines() {
        // ソース行や位置ヘッダは説明行の形式ではないため、同一でも除去しない
        let lines: Vec<String> = [
            "> 1 │ var legacyValue: any = 42;",
            "3 │ export function compute(input: any): number {",
            "> 1 │ var legacyValue: any = 42;",
            "3 │ export function compute(input: any): number {",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let result = dedup_repeated_explanation_lines(lines);
        assert_eq!(result.len(), 4, "source lines must not be deduped");
    }

    #[test]
    fn test_dedup_explanation_lines_trailing_blank_removed() {
        // 末尾の説明行を除去した結果、末尾に空行が残る場合は落とす
        let lines: Vec<String> = [
            "i See MDN web docs for more details.",
            "body",
            "",
            "i See MDN web docs for more details.",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let result = dedup_repeated_explanation_lines(lines);
        assert_eq!(
            result,
            vec!["i See MDN web docs for more details.", "body"],
            "trailing blank after dedup should be removed"
        );
    }

    #[test]
    fn test_normalize_dedups_biome_repeated_info_e2e() {
        // E2E: biome check の実出力形状で、同じルールの `i` 説明が1回に集約される
        let input = "src/sample.ts:1:1 lint/suspicious/noVar  FIXABLE  ━━━━━━━━\n\n  ! Use let or const instead of var.\n\n  i A variable declared with var is accessible in the whole module.\n\n  i See MDN web docs for more details.\n\nsrc/sample.ts:4:3 lint/suspicious/noVar  FIXABLE  ━━━━━━━━\n\n  ! Use let or const instead of var.\n\n  i A variable declared with var is accessible in the whole module.\n\n  i See MDN web docs for more details.\n\nFound 2 warnings.";
        let result = normalize_lint_output(input);
        assert_eq!(
            result.matches("i A variable declared with var").count(),
            1,
            "repeated biome info should be deduped: {result}"
        );
        assert_eq!(result.matches("i See MDN web docs").count(), 1);
        // 診断ヘッダは位置情報を持つため出現ごとに保持される
        assert_eq!(result.matches("lint/suspicious/noVar").count(), 2);
        assert!(result.contains("Found 2 warnings."));
        assert!(!result.contains("\n\n\n"), "no double blanks: {result}");
    }
}
