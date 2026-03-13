//! AI向け出力正規化ユーティリティ。
//!
//! lint/typecheck出力をトークン効率のために最適化し、
//! エラー情報は維持する。

/// デフォルトの出力最大長（文字数）。
pub const DEFAULT_OUTPUT_MAX_LENGTH: usize = 1000;

/// テキストを指定された最大長に切り詰める。
/// 切り詰めが発生した場合、末尾に省略メッセージを付加する。
/// `max_length` が 0 の場合は無制限（切り詰めなし）。
pub fn truncate_output(output: &str, max_length: usize) -> String {
    if max_length == 0 || output.len() <= max_length {
        return output.to_string();
    }

    let suffix = "\n... (truncated)";
    let keep = max_length.saturating_sub(suffix.len());

    // UTF-8境界で安全に切り詰める
    let truncated = if output.is_char_boundary(keep) {
        &output[..keep]
    } else {
        let mut end = keep;
        while end > 0 && !output.is_char_boundary(end) {
            end -= 1;
        }
        &output[..end]
    };

    format!("{}{}", truncated, suffix)
}

/// テキストからANSIエスケープコード（色、スタイル、カーソル制御）を除去する。
pub fn strip_ansi_codes(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSIシーケンス: ESC [ ... 終端バイト (0x40-0x7e)
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
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
/// - 共通の絶対パスプレフィックスを除去（例: `/Users/owa/GitHub/project/`）
/// - 各行の先頭・末尾の空白を除去
/// - 連続する空白（スペースとタブ）を1つのスペースに圧縮
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
        lines.push(collapse_whitespace(trimmed));
    }

    // 末尾の空行を除去
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }

    lines.join("\n")
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
/// パスが2つ以上あり、プレフィックスがスラッシュ3つ以上の深さ（例: `/Users/owa/`）であることが条件。
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

    text.replace(prefix, "")
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
        let input = "/Users/owa/GitHub/project/src/main.rs:10 error\n/Users/owa/GitHub/project/src/lib.rs:20 warning";
        let result = strip_common_path_prefix(input);
        assert_eq!(result, "main.rs:10 error\nlib.rs:20 warning");
    }

    #[test]
    fn test_strip_common_path_prefix_different_dirs() {
        let input =
            "/Users/owa/GitHub/project/src/main.rs:10\n/Users/owa/GitHub/project/tests/test.rs:20";
        let result = strip_common_path_prefix(input);
        assert_eq!(result, "src/main.rs:10\ntests/test.rs:20");
    }

    #[test]
    fn test_strip_common_path_prefix_single_path_skipped() {
        let input = "/Users/owa/GitHub/project/src/main.rs:10 error";
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
        let text = "error at /Users/owa/src/main.rs:10:5";
        let paths = extract_absolute_paths(text);
        assert_eq!(paths, vec!["/Users/owa/src/main.rs"]);
    }

    #[test]
    fn test_extract_absolute_paths_with_arrow() {
        let text = "-->/Users/owa/src/main.rs:10:5";
        let paths = extract_absolute_paths(text);
        assert_eq!(paths, vec!["/Users/owa/src/main.rs"]);
    }

    #[test]
    fn test_extract_absolute_paths_with_parens() {
        let text = "/Users/owa/src/main.ts(10,5): error";
        let paths = extract_absolute_paths(text);
        assert_eq!(paths, vec!["/Users/owa/src/main.ts"]);
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
        let input = "/Users/owa/GitHub/project/src/App.tsx:10:5 error\n/Users/owa/GitHub/project/tests/index.test.tsx:20:3 warning";
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
        // ESCの後に '[' が続かない場合
        let input = "text\x1bXmore";
        let result = strip_ansi_codes(input);
        // ESCは消費され、'X'はCSIではないため'X'は失われ、'more'が残る
        assert_eq!(result, "textmore");
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
        assert!(result.len() <= 1000);
        assert!(result.ends_with("... (truncated)"));
    }

    #[test]
    fn test_truncate_output_zero_means_unlimited() {
        let input = "a".repeat(10000);
        assert_eq!(truncate_output(&input, 0), input);
    }

    #[test]
    fn test_truncate_output_utf8_boundary() {
        // 日本語文字（3バイト each）でUTF-8境界を正しく処理することを確認
        let input = "あ".repeat(500); // 1500 bytes
        let result = truncate_output(&input, 100);
        assert!(result.ends_with("... (truncated)"));
        // 不正なUTF-8にならないことを確認
        assert!(result.is_char_boundary(result.len()));
    }

    #[test]
    fn test_truncate_output_empty_input() {
        assert_eq!(truncate_output("", 1000), "");
    }
}
