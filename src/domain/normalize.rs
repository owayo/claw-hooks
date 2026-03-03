//! Output normalization utilities for AI consumption.
//!
//! Optimizes lint/typecheck output for token efficiency
//! while preserving error information.

/// Strip ANSI escape codes (colors, styles, cursor control) from text.
pub fn strip_ansi_codes(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI sequence: ESC [ ... final_byte (0x40-0x7e)
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

/// Normalize lint/typecheck output for AI consumption.
/// Optimizes for token efficiency while preserving error information:
/// - Strips ANSI escape codes (colors, styles)
/// - Strips common absolute path prefix (e.g., `/Users/owa/GitHub/project/`)
/// - Strips leading and trailing whitespace from each line
/// - Collapses consecutive whitespace (spaces and tabs) into a single space
/// - Collapses consecutive blank lines into one
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

    // Remove trailing blank lines
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }

    lines.join("\n")
}

/// Collapse consecutive whitespace (spaces and tabs) into a single space.
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

/// Strip the common directory prefix from absolute file paths in the text.
/// Requires 2+ path occurrences and a prefix at least 3 slashes deep (e.g., `/Users/owa/`).
fn strip_common_path_prefix(text: &str) -> String {
    let paths = extract_absolute_paths(text);
    if paths.len() < 2 {
        return text.to_string();
    }

    let prefix = common_directory_prefix(&paths);
    // Require at least 3 slashes (e.g., /Users/owa/) to avoid stripping too little
    if prefix.is_empty() || prefix.matches('/').count() < 3 {
        return text.to_string();
    }

    text.replace(prefix, "")
}

/// Extract absolute file paths from lint output text.
/// Handles formats like `/path/file.rs:10:5`, `-->/path/file.rs:10:5`, `/path/file.ts(10,5)`.
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

/// Compute the longest common directory prefix among paths.
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

    // === Path prefix stripping ===

    #[test]
    fn test_strip_common_path_prefix_multiple_files() {
        // Same directory → prefix includes /src/
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
        // Only 2 slashes per path - prefix too shallow to strip
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
        // End-to-end: different directories → prefix is project root
        let input = "/Users/owa/GitHub/project/src/App.tsx:10:5 error\n/Users/owa/GitHub/project/tests/index.test.tsx:20:3 warning";
        let result = normalize_lint_output(input);
        assert_eq!(
            result,
            "src/App.tsx:10:5 error\ntests/index.test.tsx:20:3 warning"
        );
    }

    // === Additional edge case tests ===

    #[test]
    fn test_common_directory_prefix_empty_input() {
        let paths: Vec<&str> = vec![];
        assert_eq!(common_directory_prefix(&paths), "");
    }

    #[test]
    fn test_common_directory_prefix_single_path() {
        let paths = vec!["/a/b/c/file.rs"];
        // Single path: last_slash tracks up to last '/' before end of iteration
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
        // Path with only 1 slash is excluded (needs >= 2)
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
        // ESC without '[' following
        let input = "text\x1bXmore";
        let result = strip_ansi_codes(input);
        // ESC consumed, 'X' is not a CSI, so 'X' is lost and 'more' remains
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
}
