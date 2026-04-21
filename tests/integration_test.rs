//! claw-hooks CLI の統合テスト。

use std::io::Write;
use std::process::{Command, Stdio};

/// JSON入力で claw-hooks を実行し、`(stdout, stderr, exit_code)` を返す。
fn run_hook(json_input: &str) -> (String, String, i32) {
    run_hook_with_format(json_input, "claude")
}

/// JSON入力で指定フォーマットの claw-hooks を実行する。
fn run_hook_with_format(json_input: &str, format: &str) -> (String, String, i32) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_claw-hooks"))
        .arg("run")
        .arg("--format")
        .arg(format)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn claw-hooks");

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(json_input.as_bytes()).unwrap();
    }

    let output = child.wait_with_output().expect("Failed to read output");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    (stdout, stderr, exit_code)
}

#[test]
fn test_allow_safe_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"git status"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "Safe command should be allowed");
    assert!(
        stdout.contains(r#""permissionDecision":"allow""#),
        "Output should indicate allow: {}",
        stdout
    );
}

#[test]
fn test_block_kill_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"kill -9 1234"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "Kill command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny: {}",
        stdout
    );
    // Note: block message is configurable via kill_block_message in config
}

#[test]
fn test_block_pkill_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"pkill node"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "pkill command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny"
    );
}

#[test]
fn test_block_killall_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"killall python"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "killall command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny"
    );
}

#[test]
fn test_block_rm_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "rm command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny"
    );
    // Note: block message is configurable via rm_block_message in config
}

#[test]
fn test_block_rmdir_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rmdir old_folder"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "rmdir command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny"
    );
}

#[test]
fn test_piped_command_with_kill() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ps aux | grep node | xargs kill"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "Piped command with kill should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny"
    );
}

#[test]
fn test_chained_command_with_rm() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cd /tmp && rm -rf test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "Chained command with rm should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny"
    );
}

#[test]
fn test_allow_file_read_operation() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Read","tool_input":{"file_path":"/tmp/test.txt"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "Read operation should be allowed");
    assert!(
        stdout.contains(r#""permissionDecision":"allow""#),
        "Output should indicate allow"
    );
}

#[test]
fn test_allow_file_write_operation() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Write","tool_input":{"file_path":"/tmp/test.rs"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    // Without extension hooks configured, write should be allowed
    assert_eq!(exit_code, 0, "Write operation should be allowed");
    assert!(
        stdout.contains(r#""permissionDecision":"allow""#),
        "Output should indicate allow"
    );
}

#[test]
fn test_non_bash_tool_allowed() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"WebSearch","tool_input":{"query":"rust programming"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "Non-bash tool should be allowed");
    assert!(
        stdout.contains(r#""permissionDecision":"allow""#),
        "Output should indicate allow"
    );
}

#[test]
fn test_post_tool_use_event() {
    let input = r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"kill 1234"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    // Claude の PostToolUse + Bash は監視用途のためブロックしない
    assert_eq!(exit_code, 0, "PostToolUse should be allowed");
    // PostToolUse Allow: 空 JSON（decision 省略）
    assert_eq!(
        stdout.trim(),
        "{}",
        "PostToolUse Allow should return empty JSON"
    );
}

#[test]
fn test_stop_event() {
    // Stop イベントには tool_name / tool_input がない
    let input = r#"{"hook_event_name":"Stop","stop_hook_active":true}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "Stop event should be allowed");
    // Stop Allow は decision を省略（空 JSON を返す）
    assert!(
        !stdout.contains(r#""decision""#),
        "Stop Allow should not contain decision field: {}",
        stdout
    );
}

#[test]
fn test_init_command_creates_config() {
    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let config_path = temp_dir.path().join("claw-hooks.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_claw-hooks"))
        .arg("init")
        .arg("--path")
        .arg(&config_path)
        .output()
        .expect("Failed to run init command");

    assert!(output.status.success(), "init command should succeed");
    assert!(config_path.exists(), "Config file should be created");

    let content = std::fs::read_to_string(&config_path).expect("Failed to read config");
    assert!(
        content.contains("kill_block"),
        "Config should contain kill_block"
    );
    assert!(
        content.contains("rm_block"),
        "Config should contain rm_block"
    );
}

#[test]
fn test_help_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_claw-hooks"))
        .arg("--help")
        .output()
        .expect("Failed to run help command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "Help should succeed");
    assert!(
        stdout.contains("claw-hooks"),
        "Help should mention program name"
    );
    assert!(stdout.contains("hook"), "Help should mention hook command");
    assert!(stdout.contains("init"), "Help should mention init command");
}

#[test]
fn test_version_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_claw-hooks"))
        .arg("--version")
        .output()
        .expect("Failed to run version command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "Version should succeed");
    assert!(
        stdout.contains("claw-hooks"),
        "Version should mention program name"
    );
}

#[test]
fn test_block_dd_command_by_default() {
    // dd_block is true by default, so dd commands should be blocked
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"dd if=/dev/zero of=test.img bs=1M count=1"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "dd command should be blocked by default");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny: {}",
        stdout
    );
}

#[test]
fn test_invalid_json_input() {
    let input = "not valid json";
    let (stdout, _stderr, exit_code) = run_hook(input);

    // Invalid JSON should result in error (exit code 1)
    assert_ne!(exit_code, 0, "Invalid JSON should fail");
    assert!(
        stdout.contains("Failed to parse"),
        "Should indicate parsing failure: {}",
        stdout
    );
}

// === Cursor Format Tests ===

#[test]
fn test_cursor_format_allow_safe_command() {
    let input = r#"{"command":"git status","cwd":"/path/to/project"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(exit_code, 0, "Safe command should be allowed");
    assert!(
        stdout.contains(r#""permission":"allow""#),
        "Cursor output should indicate allow: {}",
        stdout
    );
}

#[test]
fn test_cursor_format_block_rm_command() {
    let input = r#"{"hook_event_name":"beforeShellExecution","command":"rm -rf /tmp/test","cwd":"/path/to/project"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(exit_code, 2, "rm command should be blocked");
    assert!(
        stdout.contains(r#""permission":"deny""#),
        "Cursor output should indicate deny: {}",
        stdout
    );
    // Note: block message is configurable via rm_block_message in config
}

#[test]
fn test_cursor_format_block_kill_command() {
    let input = r#"{"hook_event_name":"beforeShellExecution","command":"kill -9 1234","cwd":"/path/to/project"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(exit_code, 2, "kill command should be blocked");
    assert!(
        stdout.contains(r#""permission":"deny""#),
        "Cursor output should indicate deny: {}",
        stdout
    );
}

#[test]
fn test_cursor_format_after_file_edit() {
    // Cursor の afterFileEdit フックは file_path を提供する
    let input = r#"{"hook_event_name":"afterFileEdit","file_path":"/path/to/file.rs"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    // afterFileEdit maps to PostToolUse which always allows
    assert_eq!(exit_code, 0, "afterFileEdit should be allowed");
    assert!(
        stdout.contains(r#""permission":"allow""#),
        "Cursor output should indicate allow: {}",
        stdout
    );
}

#[test]
fn test_cursor_format_after_file_edit_camel_case() {
    // Cursor は camelCase の filePath を送る場合もある
    let input = r#"{"hook_event_name":"afterFileEdit","filePath":"/path/to/component.tsx"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(exit_code, 0, "afterFileEdit should be allowed");
    assert!(
        stdout.contains(r#""permission":"allow""#),
        "Cursor output should indicate allow: {}",
        stdout
    );
}

#[test]
fn test_cursor_format_unsupported_event_passthrough() {
    // afterShellExecution 等の未対応イベントはパススルー（allow）
    let input =
        r#"{"hook_event_name":"afterShellExecution","command":"echo test","output":"test"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(exit_code, 0, "未対応イベントは allow として処理されるべき");
    assert!(
        stdout.contains(r#""permission":"allow""#),
        "未対応イベントは allow: {}",
        stdout
    );
}

#[test]
fn test_cursor_format_pre_tool_use_passthrough() {
    // preToolUse は claw-hooks の対象外でパススルー
    let input = r#"{"hook_event_name":"preToolUse","tool_name":"Shell","tool_input":{"command":"rm -rf /"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(
        exit_code, 0,
        "preToolUse は claw-hooks の対象外でパススルー"
    );
    assert!(
        stdout.contains(r#""permission":"allow""#),
        "preToolUse は allow: {}",
        stdout
    );
}

// === Windsurf Format Tests ===

#[test]
fn test_windsurf_format_allow_safe_command() {
    let input = r#"{"agent_action_name":"pre_run_command","tool_info":{"command_line":"git status","cwd":"/path/to/project"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "windsurf");

    assert_eq!(exit_code, 0, "Safe command should be allowed");
    // Windsurf Allow: 空 JSON（decision 省略）
    assert_eq!(
        stdout.trim(),
        "{}",
        "Windsurf allow should return empty JSON: {}",
        stdout
    );
}

#[test]
fn test_windsurf_format_block_rm_command() {
    let input = r#"{"agent_action_name":"pre_run_command","tool_info":{"command_line":"rm -rf /tmp/test","cwd":"/path/to/project"}}"#;
    let (_stdout, stderr, exit_code) = run_hook_with_format(input, "windsurf");

    assert_eq!(exit_code, 2, "rm command should be blocked");
    // Windsurf は exit code 2 でブロック時、stderr からエラーメッセージを読み取る
    assert!(
        stderr.contains(r#""decision":"block""#),
        "Windsurf block message should be on stderr: {}",
        stderr
    );
}

#[test]
fn test_windsurf_format_block_kill_command() {
    let input = r#"{"agent_action_name":"pre_run_command","tool_info":{"command_line":"pkill node","cwd":"/path/to/project"}}"#;
    let (_stdout, stderr, exit_code) = run_hook_with_format(input, "windsurf");

    assert_eq!(exit_code, 2, "pkill command should be blocked");
    // Windsurf は exit code 2 でブロック時、stderr からエラーメッセージを読み取る
    assert!(
        stderr.contains(r#""decision":"block""#),
        "Windsurf block message should be on stderr: {}",
        stderr
    );
}

#[test]
fn test_windsurf_format_post_write_code() {
    let input =
        r#"{"agent_action_name":"post_write_code","tool_info":{"file_path":"/path/to/file.rs"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "windsurf");

    // PostToolUse events should be allowed (monitoring only)
    assert_eq!(exit_code, 0, "post_write_code should be allowed");
    // Windsurf Allow: 空 JSON（decision 省略）
    assert_eq!(
        stdout.trim(),
        "{}",
        "Windsurf allow should return empty JSON: {}",
        stdout
    );
}

// === Stop Event Tests ===

#[test]
fn test_cursor_format_stop_completed() {
    // Cursor's stop hook with completed status
    let input = r#"{"status":"completed","loop_count":2}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    // Stop events should be allowed (monitoring only)
    assert_eq!(exit_code, 0, "stop event should be allowed");
    assert!(
        stdout.contains(r#""permission":"allow""#),
        "Cursor output should indicate allow: {}",
        stdout
    );
}

#[test]
fn test_cursor_format_stop_aborted() {
    // Cursor's stop hook with aborted status
    let input = r#"{"status":"aborted"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(exit_code, 0, "stop event should be allowed");
    assert!(
        stdout.contains(r#""permission":"allow""#),
        "Cursor output should indicate allow: {}",
        stdout
    );
}

#[test]
fn test_cursor_format_stop_error() {
    // Cursor の Stop フックで status=error を受け取るケース
    let input = r#"{"status":"error","loop_count":0}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(exit_code, 0, "stop event should be allowed");
    assert!(
        stdout.contains(r#""permission":"allow""#),
        "Cursor output should indicate allow: {}",
        stdout
    );
}

#[test]
fn test_windsurf_format_post_cascade_response() {
    // Windsurf の post_cascade_response（Stop 相当イベント）
    let input = r#"{"agent_action_name":"post_cascade_response","tool_info":{"response":"Task completed successfully."}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "windsurf");

    // Stop は監視用途のため常に許可される
    assert_eq!(exit_code, 0, "post_cascade_response should be allowed");
    // Windsurf Stop Allow: 空 JSON（decision 省略）
    assert_eq!(
        stdout.trim(),
        "{}",
        "Windsurf stop allow should return empty JSON: {}",
        stdout
    );
}

#[test]
fn test_windsurf_format_unsupported_event_passthrough() {
    let input = r#"{"agent_action_name":"pre_user_prompt","prompt":"hello"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "windsurf");

    assert_eq!(exit_code, 0, "未対応の Windsurf イベントは透過させるべき");
    assert_eq!(stdout.trim(), "{}", "未対応イベントでも allow を返すべき");
}

#[test]
fn test_windsurf_stop_hook_failure_is_best_effort() {
    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let config_path = temp_dir.path().join("config.toml");
    let config_content = r#"
[[stop_hooks]]
commands = ["sh -c 'echo lint failed 1>&2; exit 1'"]
report = true
"#;
    std::fs::write(&config_path, config_content).expect("Failed to write config");

    let input = r#"{"agent_action_name":"post_cascade_response","tool_info":{"response":"Task completed successfully."}}"#;
    let (stdout, stderr, exit_code) =
        run_hook_with_config_and_format(input, "windsurf", &config_path);

    assert_eq!(exit_code, 0, "Windsurf stop hooks must remain best-effort");
    assert_eq!(
        stdout.trim(),
        "{}",
        "Windsurf stop hook failure should still return empty JSON: {}",
        stdout
    );
    assert!(
        !stderr.contains("lint failed"),
        "Stop hook failure must not be surfaced to Windsurf stderr: {}",
        stderr
    );
}

// === カスタムフィルターのテスト ===

/// JSON入力・フォーマット・設定ファイルを指定して claw-hooks を実行する。
fn run_hook_with_config_and_format(
    json_input: &str,
    format: &str,
    config_path: &std::path::Path,
) -> (String, String, i32) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_claw-hooks"))
        .arg("run")
        .arg("--format")
        .arg(format)
        .arg("--config")
        .arg(config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn claw-hooks");

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(json_input.as_bytes()).unwrap();
    }

    let output = child.wait_with_output().expect("Failed to read output");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    (stdout, stderr, exit_code)
}

/// 設定ファイルを指定して claw-hooks を実行する。
fn run_hook_with_config(json_input: &str, config_path: &std::path::Path) -> (String, String, i32) {
    run_hook_with_config_and_format(json_input, "claude", config_path)
}

/// カスタムフィルター用のテスト設定ファイルを作成する。
/// 戻り値は `(config_path, _temp_dir)`。RAII による後始末のため `_temp_dir` を保持する。
fn create_custom_filter_config() -> (std::path::PathBuf, tempfile::TempDir) {
    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let config_path = temp_dir.path().join("config.toml");
    let config_content = r#"
# 単体で検証できるよう既定フィルターを無効化
rm_block = false
kill_block = false
dd_block = false

[[custom_filters]]
command = "yarn"
message = "Use pnpm instead of yarn"
"#;

    std::fs::write(&config_path, config_content).expect("Failed to write config");
    (config_path, temp_dir)
}

#[test]
fn test_custom_filter_blocks_yarn_after_semicolon() {
    // `echo "install"; yarn install` の形でも、
    // セミコロン後の `yarn` をコマンドとして検出してブロックする
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo \"install\"; yarn install"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
    assert!(
        stdout.contains("pnpm"),
        "Block message should suggest pnpm: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_allows_yarn_in_quotes() {
    // Test: echo "not yarn install"; pnpm install
    // yarn is inside quotes (argument), pnpm is the actual command, should be allowed
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo \"not yarn install\"; pnpm install"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(
        exit_code, 0,
        "Command with yarn in quotes should be allowed"
    );
    assert!(
        stdout.contains(r#""permissionDecision":"allow""#),
        "Output should indicate approve: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_direct_yarn_command() {
    // Test: yarn install
    // Direct yarn command should be blocked
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"yarn install"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "Direct yarn command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_in_chained_commands() {
    // Test: cd project && yarn add react
    // yarn after && should be detected and blocked
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cd project && yarn add react"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn in chained command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_after_pipe() {
    // Test: cat package.json | yarn install
    // yarn after pipe should be detected and blocked
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cat package.json | yarn install"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn after pipe should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_in_sh_c() {
    // Test: sh -c "yarn install"
    // yarn inside sh -c should be detected and blocked
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"sh -c \"yarn install\""}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn in sh -c should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_in_bash_c() {
    // Test: bash -c "yarn add react"
    // yarn inside bash -c should be detected and blocked
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"bash -c \"yarn add react\""}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn in bash -c should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_in_subshell() {
    // Test: (cd project && yarn install)
    // yarn in subshell should be detected and blocked
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"(cd project && yarn install)"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn in subshell should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_in_command_substitution() {
    // Test: echo $(yarn --version)
    // yarn in command substitution should be detected and blocked
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo $(yarn --version)"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(
        exit_code, 2,
        "yarn in command substitution should be blocked"
    );
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_allows_yarn_string_in_pipe() {
    // Test: echo "yarn" | grep yarn
    // yarn is just a string argument, not a command, should be allowed
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo \"yarn\" | grep yarn"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 0, "yarn as string argument should be allowed");
    assert!(
        stdout.contains(r#""permissionDecision":"allow""#),
        "Output should indicate approve: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_in_complex_pipeline() {
    // Test: cat package.json | jq '.dependencies' | yarn install
    // yarn at end of complex pipeline should be blocked
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cat package.json | jq '.dependencies' | yarn install"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn in complex pipeline should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_with_env_prefix() {
    // Test: NODE_ENV=production yarn build
    // yarn with environment variable prefix should be blocked
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"NODE_ENV=production yarn build"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn with env prefix should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

// === Gemini CLI Format Tests ===

#[test]
fn test_gemini_format_allow_safe_command() {
    // Use official Gemini CLI tool name: run_shell_command
    let input = r#"{"hook_event_name":"BeforeTool","tool_name":"run_shell_command","tool_input":{"command":"git status"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "gemini");

    assert_eq!(exit_code, 0, "Safe command should be allowed");
    assert!(
        stdout.contains(r#""decision":"allow""#),
        "Gemini output should indicate allow: {}",
        stdout
    );
    // Gemini should not have reason field for allow
    assert!(
        !stdout.contains("reason"),
        "Allow output should not have reason: {}",
        stdout
    );
}

#[test]
fn test_gemini_format_block_rm_command() {
    // Use official Gemini CLI tool name: run_shell_command
    // Gemini CLI expects exit code 0 for all decisions (deny is communicated via JSON)
    let input = r#"{"hook_event_name":"BeforeTool","tool_name":"run_shell_command","tool_input":{"command":"rm -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "gemini");

    assert_eq!(exit_code, 0, "Gemini uses exit 0 for all decisions");
    assert!(
        stdout.contains(r#""decision":"deny""#),
        "Gemini output should indicate deny: {}",
        stdout
    );
    assert!(
        stdout.contains("reason"),
        "Deny output should have reason: {}",
        stdout
    );
}

#[test]
fn test_gemini_format_block_kill_command() {
    // Use official Gemini CLI tool name: run_shell_command
    // Gemini CLI expects exit code 0 for all decisions (deny is communicated via JSON)
    let input = r#"{"hook_event_name":"BeforeTool","tool_name":"run_shell_command","tool_input":{"command":"pkill node"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "gemini");

    assert_eq!(exit_code, 0, "Gemini uses exit 0 for all decisions");
    assert!(
        stdout.contains(r#""decision":"deny""#),
        "Gemini output should indicate deny: {}",
        stdout
    );
}

#[test]
fn test_gemini_format_after_tool() {
    // Use official Gemini CLI tool name: write_file
    let input = r#"{"hook_event_name":"AfterTool","tool_name":"write_file","tool_input":{"file_path":"/path/to/file.rs"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "gemini");

    // PostToolUse events should be allowed (monitoring only)
    assert_eq!(exit_code, 0, "AfterTool should be allowed");
    assert!(
        stdout.contains(r#""decision":"allow""#),
        "Gemini output should indicate allow: {}",
        stdout
    );
}

#[test]
fn test_gemini_format_after_agent() {
    let input = r#"{"hook_event_name":"AfterAgent"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "gemini");

    // Stop events should be allowed (monitoring only)
    assert_eq!(exit_code, 0, "AfterAgent should be allowed");
    assert!(
        stdout.contains(r#""decision":"allow""#),
        "Gemini output should indicate allow: {}",
        stdout
    );
}

#[test]
fn test_gemini_format_with_event_alias() {
    // Use official Gemini CLI tool name: run_shell_command
    let input = r#"{"event":"BeforeTool","tool_name":"run_shell_command","tool_input":{"command":"echo hello"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "gemini");

    assert_eq!(exit_code, 0, "Safe command should be allowed");
    assert!(
        stdout.contains(r#""decision":"allow""#),
        "Gemini output should indicate allow: {}",
        stdout
    );
}

#[test]
fn test_gemini_format_session_start_passthrough() {
    let input = r#"{"hook_event_name":"SessionStart","source":"startup"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "gemini");

    assert_eq!(exit_code, 0, "未対応の Gemini イベントは透過させるべき");
    assert!(
        stdout.contains(r#""decision":"allow""#),
        "未対応イベントでも allow を返すべき: {}",
        stdout
    );
}

// === Fail-Closed Error Handling Tests ===

#[test]
fn test_empty_input_is_fail_closed() {
    let (stdout, _stderr, exit_code) = run_hook("");

    assert_eq!(
        exit_code, 2,
        "Empty input should result in block (fail-closed)"
    );
    assert!(
        stdout.contains("fail-closed"),
        "Output should indicate fail-closed: {}",
        stdout
    );
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_malformed_json_is_fail_closed() {
    let (stdout, _stderr, exit_code) = run_hook("{");

    assert_eq!(
        exit_code, 2,
        "Malformed JSON should result in block (fail-closed)"
    );
    assert!(
        stdout.contains("fail-closed"),
        "Output should indicate fail-closed: {}",
        stdout
    );
}

#[test]
fn test_unknown_event_is_fail_closed() {
    let input = r#"{"hook_event_name":"Bogus","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(
        exit_code, 2,
        "Unknown event should result in block (fail-closed)"
    );
    assert!(
        stdout.contains("fail-closed"),
        "Output should indicate fail-closed: {}",
        stdout
    );
}

#[test]
fn test_cursor_empty_input_is_fail_closed() {
    let (stdout, _stderr, exit_code) = run_hook_with_format("", "cursor");

    assert_eq!(
        exit_code, 2,
        "Empty input should result in deny (fail-closed)"
    );
    assert!(
        stdout.contains("fail-closed"),
        "Output should indicate fail-closed: {}",
        stdout
    );
    assert!(
        stdout.contains(r#""permission":"deny""#),
        "Output should indicate deny: {}",
        stdout
    );
}

#[test]
fn test_windsurf_empty_input_is_fail_closed() {
    let (_stdout, stderr, exit_code) = run_hook_with_format("", "windsurf");

    assert_eq!(
        exit_code, 2,
        "Empty input should result in block (fail-closed)"
    );
    // Windsurf はブロック時に stderr からメッセージを読むため、
    // フェイルクローズパスでも stderr に出力される
    assert!(
        stderr.contains("fail-closed"),
        "stderr should indicate fail-closed: {}",
        stderr
    );
}

#[test]
fn test_gemini_empty_input_is_fail_closed() {
    let (stdout, _stderr, exit_code) = run_hook_with_format("", "gemini");

    // Gemini: 非0終了コードはフック失敗扱いで判定が無視されるため、
    // エラー時も0を返し、deny判定はJSON内で伝達する
    assert_eq!(
        exit_code, 0,
        "Gemini empty input should exit 0 with deny in JSON"
    );
    assert!(
        stdout.contains("fail-closed"),
        "Output should indicate fail-closed: {}",
        stdout
    );
}

#[test]
fn test_codex_empty_input_is_fail_closed() {
    let (stdout, _stderr, exit_code) = run_hook_with_format("", "codex");

    // Codex CLI: 非0終了コードはフック失敗扱いで判定が無視されるため、
    // エラー時も0を返し、block判定はJSON内で伝達する
    assert_eq!(
        exit_code, 0,
        "Codex empty input should exit 0 with block in JSON"
    );
    assert!(
        stdout.contains("fail-closed"),
        "Output should indicate fail-closed: {}",
        stdout
    );
}

#[test]
fn test_codex_missing_hook_event_name_is_fail_closed() {
    let input = r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "codex");

    assert_eq!(
        exit_code, 0,
        "Codex missing hook_event_name should exit 0 with block in JSON"
    );
    assert!(
        stdout.contains("Missing hook_event_name field"),
        "Output should mention the missing required field: {}",
        stdout
    );
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_codex_missing_tool_input_is_fail_closed() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "codex");

    assert_eq!(
        exit_code, 0,
        "Codex missing tool_input should exit 0 with block in JSON"
    );
    assert!(
        stdout.contains("Missing tool_input field"),
        "Output should mention the missing required field: {}",
        stdout
    );
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
        stdout
    );
}

// === ラッパー・サブシェル検出テスト ===

#[test]
fn test_block_sudo_rm() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"sudo rm -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "sudo rm should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_block_sudo_non_interactive_rm() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"sudo -n rm -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "sudo -n rm should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_block_sudo_long_option_rm() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"sudo --user root rm -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "sudo --user root rm should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_block_timeout_long_option_rm() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"timeout --signal TERM 10 rm -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(
        exit_code, 2,
        "timeout --signal TERM 10 rm should be blocked"
    );
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_block_command_wrapper_rm() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"command rm -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "command rm should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_block_bash_c_rm() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"bash -c 'rm -rf /tmp/test'"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "bash -c rm should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_block_sudo_kill() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"sudo kill -9 1234"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "sudo kill should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_block_sudo_dd() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"sudo dd if=/dev/zero of=/dev/sda"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "sudo dd should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_block_xargs_kill() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"pgrep node | xargs kill -9"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "xargs kill should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_block_rm_in_subshell() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"(cd /tmp && rm -rf test)"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "rm in subshell should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_block_rm_in_command_substitution() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo $(rm -rf /tmp/test)"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "rm in command substitution should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

// =========================================================================
// Codex フォーマット統合テスト
// =========================================================================

#[test]
fn test_codex_format_allow_safe_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"git status"},"session_id":"test-session","cwd":"/tmp"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "codex");

    assert_eq!(exit_code, 0, "Safe command should be allowed");
    // Codex Allow は空 JSON {} を返す
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({}),
        "Codex Allow should return empty JSON"
    );
}

#[test]
fn test_codex_format_block_rm_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf /"},"session_id":"test-session","cwd":"/tmp"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "codex");

    // Codex: 非0終了コードはフック失敗扱いのため exit 0 + JSON で block を伝達
    assert_eq!(exit_code, 0, "Codex block should still exit 0");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should contain block decision: {}",
        stdout
    );
    assert!(
        stdout.contains(r#""reason""#),
        "Output should contain reason field: {}",
        stdout
    );
}

#[test]
fn test_codex_format_block_kill_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"kill -9 1234"},"session_id":"test-session","cwd":"/tmp"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "codex");

    assert_eq!(exit_code, 0, "Codex block should still exit 0");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should contain block decision: {}",
        stdout
    );
}

#[test]
fn test_codex_format_empty_input_is_fail_closed() {
    let (stdout, _stderr, exit_code) = run_hook_with_format("", "codex");

    // Codex: 非0終了コードはフック失敗扱いのため、フェイルクローズでも exit 0
    assert_eq!(
        exit_code, 0,
        "Codex empty input should exit 0 with block in JSON"
    );
    assert!(
        stdout.contains("fail-closed"),
        "Output should indicate fail-closed: {}",
        stdout
    );
}

#[test]
fn test_codex_format_invalid_json_is_fail_closed() {
    let (stdout, _stderr, exit_code) = run_hook_with_format("{invalid json}", "codex");

    assert_eq!(
        exit_code, 0,
        "Codex invalid JSON should exit 0 with block in JSON"
    );
    assert!(
        stdout.contains("fail-closed"),
        "Output should indicate fail-closed: {}",
        stdout
    );
}

#[test]
fn test_codex_format_missing_required_fields_is_fail_closed() {
    // hook_event_name が欠落: Codex はフェイルクローズすべき
    let input = r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "codex");

    assert_eq!(
        exit_code, 0,
        "Codex missing fields should exit 0 with block in JSON"
    );
    assert!(
        stdout.contains("fail-closed"),
        "Output should indicate fail-closed: {}",
        stdout
    );
}

#[test]
fn test_codex_format_stop_event() {
    let input = r#"{"hook_event_name":"Stop","session_id":"test-session","cwd":"/tmp","stop_hook_active":false}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "codex");

    assert_eq!(exit_code, 0, "Codex Stop should exit 0");
    // Stop Allow は空 JSON {} を返す
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({}),
        "Codex Stop Allow should return empty JSON"
    );
}

#[test]
fn test_codex_format_post_tool_use_bash_passthrough() {
    let input = r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"echo done"},"session_id":"test-session","cwd":"/tmp"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "codex");

    // 現行の Codex PostToolUse は Bash 出力のパススルーとして扱う
    assert_eq!(exit_code, 0, "Codex PostToolUse should exit 0");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({}),
        "Codex PostToolUse passthrough should return empty JSON"
    );
}

// =========================================================================
// Windsurf フォーマット追加テスト
// =========================================================================

#[test]
fn test_windsurf_invalid_json_is_fail_closed_stderr() {
    let (_stdout, stderr, exit_code) = run_hook_with_format("{invalid}", "windsurf");

    assert_eq!(
        exit_code, 2,
        "Windsurf invalid JSON should result in block (fail-closed)"
    );
    // Windsurf はブロック時に stderr からメッセージを読む
    assert!(
        stderr.contains("fail-closed"),
        "stderr should indicate fail-closed: {}",
        stderr
    );
}

// =========================================================================
// stop_hook_active ループ防止テスト
// =========================================================================

#[test]
fn test_stop_hook_active_true_allows_stop() {
    // stop_hook_active=true の場合、Stop フックの無限ループを防止するため
    // 無条件で Allow を返すべき
    let input = r#"{"hook_event_name":"Stop","stop_hook_active":true}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(
        exit_code, 0,
        "Stop with stop_hook_active=true should be allowed"
    );
    // Stop Allow は decision を省略
    assert!(
        !stdout.contains(r#""permissionDecision":"deny""#),
        "Stop with stop_hook_active should not block: {}",
        stdout
    );
}

#[test]
fn test_stop_hook_active_false_processes_normally() {
    // stop_hook_active=false の場合は通常通り処理される
    let input = r#"{"hook_event_name":"Stop","stop_hook_active":false}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(
        exit_code, 0,
        "Stop with stop_hook_active=false should be allowed (no stop hooks configured)"
    );
    assert!(
        !stdout.contains(r#""permissionDecision":"deny""#),
        "Stop should not block without stop hooks: {}",
        stdout
    );
}

// =========================================================================
// Codex フォーマット パススルーイベントテスト
// =========================================================================

#[test]
fn test_codex_session_start_passthrough() {
    let input = r#"{"hook_event_name":"SessionStart","session_id":"test-session","cwd":"/tmp","source":"startup"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "codex");

    assert_eq!(
        exit_code, 0,
        "Codex SessionStart should be allowed (passthrough)"
    );
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({}),
        "Codex passthrough should return empty JSON"
    );
}

#[test]
fn test_codex_user_prompt_submit_passthrough() {
    let input = r#"{"hook_event_name":"UserPromptSubmit","session_id":"test-session","cwd":"/tmp","prompt":"hello"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "codex");

    assert_eq!(
        exit_code, 0,
        "Codex UserPromptSubmit should be allowed (passthrough)"
    );
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({}),
        "Codex passthrough should return empty JSON"
    );
}
