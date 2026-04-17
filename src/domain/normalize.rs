//! AI向け出力正規化ユーティリティ。
//!
//! lint/typecheck出力をトークン効率のために最適化し、
//! エラー情報は維持する。

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
/// - 共通の絶対パスプレフィックスを除去（例: `/home/user/GitHub/project/`）
/// - 各行の先頭・末尾の空白を除去
/// - 連続する空白（スペースとタブ）を1つのスペースに圧縮
/// - 連続する装飾文字（`.`, `=`, `-`, `─`, `━`）を1文字に圧縮
/// - 連続する空行を1行に圧縮
pub fn normalize_lint_output(output: &str) -> String {
    let stripped = strip_ansi_codes(output);
    let stripped = strip_common_path_prefix(&stripped);

    let mut lines = Vec::new();
    let mut prev_blank = false;

    for line in stripped.lines() {
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
        lines.push(collapse_repeated_chars(&collapsed));
    }

    // 末尾の空行を除去
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }

    lines.join("\n")
}

/// 同一文字が連続する装飾文字をトークン効率のために圧縮する。
/// 対象: `.`, `=`, `-`, `─`, `━`
/// ルール:
/// - `=`, `-`, `─`, `━` は4回以上の連続で1文字に圧縮
/// - `.` は4回以上の連続、または行末の3連続以上で1文字に圧縮
///
/// 例: `====` → `=`, `...............` → `.`, `text...` → `text.`
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
            let should_collapse = count >= 4 || (c == '.' && count >= 3 && chars.peek().is_none());
            if !should_collapse {
                for _ in 1..count {
                    result.push(c);
                }
            }
        }
    }
    result
}

/// 装飾文字かどうかを判定する。
fn is_decorative_char(c: char) -> bool {
    matches!(c, '.' | '=' | '-' | '─' | '━')
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

    // パスコンテキストでのみ置換: プレフィックス直後にパス文字が続く場合のみ
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(pos) = remaining.find(prefix) {
        result.push_str(&remaining[..pos]);
        let after = &remaining[pos + prefix.len()..];
        // パスコンテキスト: プレフィックス直後にファイル名文字（英数字、_、.、-）が続く場合のみ除去
        let is_path_context = after
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '-');
        if is_path_context {
            // プレフィックスを除去（直後のパス部分はそのまま保持）
            remaining = after;
        } else {
            // パスコンテキストでなければプレフィックスをそのまま保持
            result.push_str(prefix);
            remaining = after;
        }
    }
    result.push_str(remaining);
    result
}

/// lint出力テキストから絶対パスを抽出する。
/// `/path/file.rs:10:5`、`-->/path/file.rs:10:5`、`/path/file.ts(10,5)` 等の形式に対応。
fn extract_absolute_paths(text: &str) -> Vec<&str> {
    text.split_whitespace()
        .filter_map(|token| {
            let start = token.find('/')?;
            let rest = &token[start..];
            let end = rest.find([':', '(']).unwrap_or(rest.len());
            let path = &rest[..end];
            if path.matches('/').count() >= 2 && path.len() > 2 {
                Some(path)
            } else {
                None
            }
        })
        .collect()
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
        assert_eq!(collapse_repeated_chars("#####"), "#####");
        assert_eq!(collapse_repeated_chars("*****"), "*****");
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
}
