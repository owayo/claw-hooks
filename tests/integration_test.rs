//! Integration tests for claw-hooks CLI.

use std::io::Write;
use std::process::{Command, Stdio};

/// Helper to run claw-hooks with JSON input and return (stdout, stderr, exit_code).
fn run_hook(json_input: &str) -> (String, String, i32) {
    run_hook_with_format(json_input, "claude")
}

/// Helper to run claw-hooks with JSON input and specific format.
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
        stdout.contains(r#""decision":"approve""#),
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
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
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
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block"
    );
}

#[test]
fn test_block_killall_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"killall python"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "killall command should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block"
    );
}

#[test]
fn test_block_rm_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "rm command should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block"
    );
    // Note: block message is configurable via rm_block_message in config
}

#[test]
fn test_block_rmdir_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rmdir old_folder"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "rmdir command should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block"
    );
}

#[test]
fn test_piped_command_with_kill() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ps aux | grep node | xargs kill"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "Piped command with kill should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block"
    );
}

#[test]
fn test_chained_command_with_rm() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cd /tmp && rm -rf test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 2, "Chained command with rm should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block"
    );
}

#[test]
fn test_allow_file_read_operation() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Read","tool_input":{"file_path":"/tmp/test.txt"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "Read operation should be allowed");
    assert!(
        stdout.contains(r#""decision":"approve""#),
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
        stdout.contains(r#""decision":"approve""#),
        "Output should indicate allow"
    );
}

#[test]
fn test_non_bash_tool_allowed() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"WebSearch","tool_input":{"query":"rust programming"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "Non-bash tool should be allowed");
    assert!(
        stdout.contains(r#""decision":"approve""#),
        "Output should indicate allow"
    );
}

#[test]
fn test_post_tool_use_event() {
    let input = r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"kill 1234"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    // PostToolUse events should be allowed (monitoring only, no blocking)
    assert_eq!(exit_code, 0, "PostToolUse should be allowed");
    assert!(
        stdout.contains(r#""decision":"approve""#),
        "Output should indicate allow"
    );
}

#[test]
fn test_stop_event() {
    // Stop events have no tool_name or tool_input
    let input = r#"{"hook_event_name":"Stop","stop_hook_active":true}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "Stop event should be allowed");
    assert!(
        stdout.contains(r#""decision":"approve""#),
        "Output should indicate allow"
    );
}

#[test]
fn test_init_command_creates_config() {
    use std::env;
    use std::fs;

    // Create a temporary directory for the test
    let temp_dir = env::temp_dir().join(format!("claw-hooks-test-{}", std::process::id()));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    let config_path = temp_dir.join("claw-hooks.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_claw-hooks"))
        .arg("init")
        .arg("--path")
        .arg(&config_path)
        .output()
        .expect("Failed to run init command");

    assert!(output.status.success(), "init command should succeed");
    assert!(config_path.exists(), "Config file should be created");

    let content = fs::read_to_string(&config_path).expect("Failed to read config");
    assert!(
        content.contains("kill_block"),
        "Config should contain kill_block"
    );
    assert!(
        content.contains("rm_block"),
        "Config should contain rm_block"
    );

    // Cleanup
    fs::remove_dir_all(&temp_dir).ok();
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
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
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
    let input = r#"{"command":"rm -rf /tmp/test","cwd":"/path/to/project"}"#;
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
    let input = r#"{"command":"kill -9 1234","cwd":"/path/to/project"}"#;
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
    // Cursor's afterFileEdit hook provides file_path
    let input = r#"{"file_path":"/path/to/file.rs"}"#;
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
    // Cursor might use camelCase filePath
    let input = r#"{"filePath":"/path/to/component.tsx"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(exit_code, 0, "afterFileEdit should be allowed");
    assert!(
        stdout.contains(r#""permission":"allow""#),
        "Cursor output should indicate allow: {}",
        stdout
    );
}

// === Windsurf Format Tests ===

#[test]
fn test_windsurf_format_allow_safe_command() {
    let input = r#"{"agent_action_name":"pre_run_command","tool_info":{"command_line":"git status","cwd":"/path/to/project"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "windsurf");

    assert_eq!(exit_code, 0, "Safe command should be allowed");
    assert!(
        stdout.contains(r#""decision":"approve""#),
        "Windsurf output should indicate allow: {}",
        stdout
    );
}

#[test]
fn test_windsurf_format_block_rm_command() {
    let input = r#"{"agent_action_name":"pre_run_command","tool_info":{"command_line":"rm -rf /tmp/test","cwd":"/path/to/project"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "windsurf");

    assert_eq!(exit_code, 2, "rm command should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Windsurf output should indicate block: {}",
        stdout
    );
    // Note: block message is configurable via rm_block_message in config
}

#[test]
fn test_windsurf_format_block_kill_command() {
    let input = r#"{"agent_action_name":"pre_run_command","tool_info":{"command_line":"pkill node","cwd":"/path/to/project"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "windsurf");

    assert_eq!(exit_code, 2, "pkill command should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Windsurf output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_windsurf_format_post_write_code() {
    let input =
        r#"{"agent_action_name":"post_write_code","tool_info":{"file_path":"/path/to/file.rs"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "windsurf");

    // PostToolUse events should be allowed (monitoring only)
    assert_eq!(exit_code, 0, "post_write_code should be allowed");
    assert!(
        stdout.contains(r#""decision":"approve""#),
        "Windsurf output should indicate allow: {}",
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
    // Cursor's stop hook with error status
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
    // Windsurf's post_cascade_response (equivalent to Stop event)
    let input = r#"{"agent_action_name":"post_cascade_response","tool_info":{"response":"Task completed successfully."}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "windsurf");

    // Stop events should be allowed (monitoring only)
    assert_eq!(exit_code, 0, "post_cascade_response should be allowed");
    assert!(
        stdout.contains(r#""decision":"approve""#),
        "Windsurf output should indicate allow: {}",
        stdout
    );
}

// === Custom Filter Tests ===

/// Helper to run claw-hooks with custom config file.
fn run_hook_with_config(json_input: &str, config_path: &std::path::Path) -> (String, String, i32) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_claw-hooks"))
        .arg("run")
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

/// Create a test config file with custom filters.
fn create_custom_filter_config() -> std::path::PathBuf {
    use std::env;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Use a unique directory for each test to avoid conflicts when running in parallel
    let unique_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let temp_dir = env::temp_dir().join(format!(
        "claw-hooks-custom-filter-test-{}-{}",
        std::process::id(),
        unique_id
    ));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    let config_path = temp_dir.join("config.toml");
    let config_content = r#"
# Disable default filters for isolated testing
rm_block = false
kill_block = false
dd_block = false

[[custom_filters]]
command = "yarn"
message = "Use pnpm instead of yarn"
"#;

    fs::write(&config_path, config_content).expect("Failed to write config");
    config_path
}

#[test]
fn test_custom_filter_blocks_yarn_after_semicolon() {
    // Test: echo "install"; yarn install
    // yarn is a command after semicolon, should be blocked
    let config_path = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo \"install\"; yarn install"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn command should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
        stdout
    );
    assert!(
        stdout.contains("pnpm"),
        "Block message should suggest pnpm: {}",
        stdout
    );

    // Cleanup
    std::fs::remove_dir_all(config_path.parent().unwrap()).ok();
}

#[test]
fn test_custom_filter_allows_yarn_in_quotes() {
    // Test: echo "not yarn install"; pnpm install
    // yarn is inside quotes (argument), pnpm is the actual command, should be allowed
    let config_path = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo \"not yarn install\"; pnpm install"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(
        exit_code, 0,
        "Command with yarn in quotes should be allowed"
    );
    assert!(
        stdout.contains(r#""decision":"approve""#),
        "Output should indicate approve: {}",
        stdout
    );

    // Cleanup
    std::fs::remove_dir_all(config_path.parent().unwrap()).ok();
}

#[test]
fn test_custom_filter_blocks_direct_yarn_command() {
    // Test: yarn install
    // Direct yarn command should be blocked
    let config_path = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"yarn install"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "Direct yarn command should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
        stdout
    );

    // Cleanup
    std::fs::remove_dir_all(config_path.parent().unwrap()).ok();
}

#[test]
fn test_custom_filter_blocks_yarn_in_chained_commands() {
    // Test: cd project && yarn add react
    // yarn after && should be detected and blocked
    let config_path = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cd project && yarn add react"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn in chained command should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
        stdout
    );

    // Cleanup
    std::fs::remove_dir_all(config_path.parent().unwrap()).ok();
}

#[test]
fn test_custom_filter_blocks_yarn_after_pipe() {
    // Test: cat package.json | yarn install
    // yarn after pipe should be detected and blocked
    let config_path = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cat package.json | yarn install"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn after pipe should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
        stdout
    );

    // Cleanup
    std::fs::remove_dir_all(config_path.parent().unwrap()).ok();
}

#[test]
fn test_custom_filter_blocks_yarn_in_sh_c() {
    // Test: sh -c "yarn install"
    // yarn inside sh -c should be detected and blocked
    let config_path = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"sh -c \"yarn install\""}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn in sh -c should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
        stdout
    );

    // Cleanup
    std::fs::remove_dir_all(config_path.parent().unwrap()).ok();
}

#[test]
fn test_custom_filter_blocks_yarn_in_bash_c() {
    // Test: bash -c "yarn add react"
    // yarn inside bash -c should be detected and blocked
    let config_path = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"bash -c \"yarn add react\""}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn in bash -c should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
        stdout
    );

    // Cleanup
    std::fs::remove_dir_all(config_path.parent().unwrap()).ok();
}

#[test]
fn test_custom_filter_blocks_yarn_in_subshell() {
    // Test: (cd project && yarn install)
    // yarn in subshell should be detected and blocked
    let config_path = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"(cd project && yarn install)"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn in subshell should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
        stdout
    );

    // Cleanup
    std::fs::remove_dir_all(config_path.parent().unwrap()).ok();
}

#[test]
fn test_custom_filter_blocks_yarn_in_command_substitution() {
    // Test: echo $(yarn --version)
    // yarn in command substitution should be detected and blocked
    let config_path = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo $(yarn --version)"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(
        exit_code, 2,
        "yarn in command substitution should be blocked"
    );
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
        stdout
    );

    // Cleanup
    std::fs::remove_dir_all(config_path.parent().unwrap()).ok();
}

#[test]
fn test_custom_filter_allows_yarn_string_in_pipe() {
    // Test: echo "yarn" | grep yarn
    // yarn is just a string argument, not a command, should be allowed
    let config_path = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo \"yarn\" | grep yarn"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 0, "yarn as string argument should be allowed");
    assert!(
        stdout.contains(r#""decision":"approve""#),
        "Output should indicate approve: {}",
        stdout
    );

    // Cleanup
    std::fs::remove_dir_all(config_path.parent().unwrap()).ok();
}

#[test]
fn test_custom_filter_blocks_yarn_in_complex_pipeline() {
    // Test: cat package.json | jq '.dependencies' | yarn install
    // yarn at end of complex pipeline should be blocked
    let config_path = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cat package.json | jq '.dependencies' | yarn install"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn in complex pipeline should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
        stdout
    );

    // Cleanup
    std::fs::remove_dir_all(config_path.parent().unwrap()).ok();
}

#[test]
fn test_custom_filter_blocks_yarn_with_env_prefix() {
    // Test: NODE_ENV=production yarn build
    // yarn with environment variable prefix should be blocked
    let config_path = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"NODE_ENV=production yarn build"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 2, "yarn with env prefix should be blocked");
    assert!(
        stdout.contains(r#""decision":"block""#),
        "Output should indicate block: {}",
        stdout
    );

    // Cleanup
    std::fs::remove_dir_all(config_path.parent().unwrap()).ok();
}
