//! claw-hooks CLI の統合テスト。

use std::io::Write;
use std::process::{Command, Stdio};

/// JSON入力で claw-hooks を実行し、`(stdout, stderr, exit_code)` を返す。
fn run_hook(json_input: &str) -> (String, String, i32) {
    run_hook_with_format(json_input, "claude")
}

/// JSON入力で指定フォーマットの claw-hooks を実行する。
///
/// テストをユーザのグローバル設定（`~/.config/claw-hooks/config.toml`）から
/// 隔離するため、空の設定ファイルを `--config` で渡す。これにより Stop フック等が
/// テスト実行マシンの設定に依存して実際に実行される（cargo/git 等の副作用や
/// 非決定的な出力）のを防ぎ、組み込みの既定動作のみを検証する。
/// 設定値に依存するテストは `run_hook_with_config_and_format` を使うこと。
fn run_hook_with_format(json_input: &str, format: &str) -> (String, String, i32) {
    let empty_config = tempfile::Builder::new()
        .prefix("claw-hooks-test-empty")
        .suffix(".toml")
        .tempfile()
        .expect("Failed to create temp config");

    run_hook_with_config_and_format(json_input, format, empty_config.path())
}

fn run_hook_with_format_and_event(
    json_input: &str,
    format: &str,
    event: &str,
) -> (String, String, i32) {
    let empty_config = tempfile::Builder::new()
        .prefix("claw-hooks-test-empty")
        .suffix(".toml")
        .tempfile()
        .expect("Failed to create temp config");
    run_hook_with_config_format_and_event(json_input, format, empty_config.path(), Some(event))
}

/// 統合テスト用の Codex 入力に、公式仕様の必須メタデータを補完する。
fn complete_codex_input(input: &str) -> String {
    fn insert_default(
        object: &mut serde_json::Map<String, serde_json::Value>,
        field: &str,
        value: serde_json::Value,
    ) {
        object.entry(field.to_string()).or_insert(value);
    }

    let mut value: serde_json::Value = serde_json::from_str(input).unwrap();
    let object = value.as_object_mut().unwrap();
    let event = object
        .get("hook_event_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();

    insert_default(object, "session_id", serde_json::json!("test-session"));
    insert_default(object, "transcript_path", serde_json::Value::Null);
    insert_default(object, "cwd", serde_json::json!("/tmp/project"));
    insert_default(object, "model", serde_json::json!("gpt-5.4"));

    match event.as_str() {
        "SessionStart" => {
            insert_default(object, "permission_mode", serde_json::json!("default"));
        }
        "PreToolUse" => {
            insert_default(object, "turn_id", serde_json::json!("turn-1"));
            insert_default(object, "permission_mode", serde_json::json!("default"));
            insert_default(object, "tool_use_id", serde_json::json!("tool-1"));
        }
        "PermissionRequest" => {
            insert_default(object, "turn_id", serde_json::json!("turn-1"));
            insert_default(object, "permission_mode", serde_json::json!("default"));
        }
        "PostToolUse" => {
            insert_default(object, "turn_id", serde_json::json!("turn-1"));
            insert_default(object, "permission_mode", serde_json::json!("default"));
            insert_default(object, "tool_use_id", serde_json::json!("tool-1"));
            insert_default(object, "tool_response", serde_json::Value::Null);
        }
        "UserPromptSubmit" | "SubagentStart" | "SubagentStop" | "Stop" => {
            insert_default(object, "turn_id", serde_json::json!("turn-1"));
            insert_default(object, "permission_mode", serde_json::json!("default"));
        }
        _ => {}
    }

    serde_json::to_string(&value).unwrap()
}

#[test]
fn test_allow_safe_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"git status"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "Safe command should be allowed");
    // PreToolUse の非ブロック時は判定を返さず {} にする（Claude 本来の権限フローに委ねる）。
    // permissionDecision "allow" は権限プロンプトをスキップしてしまう。
    assert_eq!(stdout.trim(), "{}", "Output should abstain: {}", stdout);
}

#[test]
fn test_block_kill_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"kill -9 1234"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "Kill command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny: {}",
        stdout
    );
    // ブロックメッセージは config の kill_block_message で変更できる
}

#[test]
fn test_block_pkill_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"pkill node"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "pkill command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny"
    );
}

#[test]
fn test_block_killall_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"killall python"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "killall command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny"
    );
}

#[test]
fn test_block_rm_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "rm command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny"
    );
    // ブロックメッセージは config の rm_block_message で変更できる
}

#[test]
fn test_block_backslash_escaped_rm_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"r\\m -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(
        exit_code, 0,
        "シェルの quote removal 後に rm になるコマンドはブロックされるべき"
    );
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "出力は deny を示すべき: {}",
        stdout
    );
}

#[test]
fn test_block_quoted_concatenated_rm_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"r''m -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(
        exit_code, 0,
        "引用符連結により rm になるコマンドはブロックされるべき"
    );
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "出力は deny を示すべき: {}",
        stdout
    );
}

#[test]
fn test_block_ansi_c_quoted_rm_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"$'r\\x6d' -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(
        exit_code, 0,
        "Bash の ANSI-C quoting で rm になるコマンドはブロックされるべき"
    );
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "出力は deny を示すべき: {}",
        stdout
    );
}

#[test]
fn test_block_escaped_sudo_rm_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"s\\udo rm -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(
        exit_code, 0,
        "quote removal 後に sudo になるラッパー経由の rm もブロックされるべき"
    );
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "出力は deny を示すべき: {}",
        stdout
    );
}

#[test]
fn test_block_sudo_rm_with_env_assignment() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"sudo FOO=bar rm -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(
        exit_code, 0,
        "sudo の環境変数代入を挟んだ rm もブロックされるべき"
    );
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "出力は deny を示すべき"
    );
}

#[test]
fn test_block_rmdir_command() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rmdir old_folder"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "rmdir command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny"
    );
}

#[test]
fn test_piped_command_with_kill() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ps aux | grep node | xargs kill"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "Piped command with kill should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny"
    );
}

#[test]
fn test_chained_command_with_rm() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cd /tmp && rm -rf test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "Chained command with rm should be blocked");
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
    assert_eq!(stdout.trim(), "{}", "Output should abstain: {}", stdout);
}

#[test]
fn test_allow_file_write_operation() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Write","tool_input":{"file_path":"/tmp/test.rs"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    // extension_hooks 未設定なら書き込みは許可される
    assert_eq!(exit_code, 0, "Write operation should be allowed");
    assert_eq!(stdout.trim(), "{}", "Output should abstain: {}", stdout);
}

#[test]
fn test_non_bash_tool_allowed() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"WebSearch","tool_input":{"query":"rust programming"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "Non-bash tool should be allowed");
    assert_eq!(stdout.trim(), "{}", "Output should abstain: {}", stdout);
}

#[test]
fn test_powershell_tool_dangerous_command_blocked() {
    // Windows で Git Bash が無い環境では Claude Code が Bash ツールを登録せず
    // PowerShell が主シェルになる。公式仕様も `Bash|PowerShell` の両方に
    // マッチさせるよう明記しており、ここを拾わないと全ブロックが無効化される。
    for command in [
        "rm -rf /tmp/x",
        "Remove-Item -Recurse -Force C:/build",
        "Stop-Process -Id 123",
        "del C:/tmp/x",
    ] {
        let input = format!(
            r#"{{"hook_event_name":"PreToolUse","tool_name":"PowerShell","tool_input":{{"command":"{}"}}}}"#,
            command
        );
        let (stdout, _stderr, _exit_code) = run_hook(&input);
        assert!(
            stdout.contains("\"permissionDecision\":\"deny\""),
            "PowerShell の危険コマンドはブロックされるべき ({command}): {stdout}"
        );
    }
}

#[test]
fn test_powershell_tool_safe_command_allowed() {
    for command in ["Get-ChildItem -Recurse", "ri Array", "ls"] {
        let input = format!(
            r#"{{"hook_event_name":"PreToolUse","tool_name":"PowerShell","tool_input":{{"command":"{}"}}}}"#,
            command
        );
        let (stdout, _stderr, exit_code) = run_hook(&input);
        assert_eq!(exit_code, 0, "安全なコマンドは許可されるべき ({command})");
        assert_eq!(
            stdout.trim(),
            "{}",
            "安全なコマンドは判定を返さない ({command}): {stdout}"
        );
    }
}

#[test]
fn test_execution_wrapper_bypasses_are_blocked() {
    // 実行委譲ラッパー経由でも危険コマンドを検出する。
    // arch は macOS 標準、systemd-run は Linux、script は両者に存在する。
    for command in [
        "arch -arm64 rm -rf /tmp/x",
        "arch -arch x86_64 rm -rf /tmp/x",
        "systemd-run --uid 0 rm -rf /tmp/x",
        "script -q /dev/null rm -rf /tmp/x",
        "sudo arch -arm64 rm -rf /tmp/x",
    ] {
        let input = format!(
            r#"{{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{{"command":"{}"}}}}"#,
            command
        );
        let (stdout, _stderr, _exit_code) = run_hook(&input);
        assert!(
            stdout.contains("\"permissionDecision\":\"deny\""),
            "ラッパー経由の危険コマンドはブロックされるべき ({command}): {stdout}"
        );
    }
}

#[test]
fn test_execution_wrapper_safe_commands_not_overblocked() {
    for command in [
        "arch -arm64 ls -la",
        "script /tmp/typescript",
        "systemd-run --uid 0 ls",
    ] {
        let input = format!(
            r#"{{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{{"command":"{}"}}}}"#,
            command
        );
        let (stdout, _stderr, exit_code) = run_hook(&input);
        assert_eq!(exit_code, 0, "過剰ブロックしてはいけない ({command})");
        assert_eq!(
            stdout.trim(),
            "{}",
            "過剰ブロックしてはいけない ({command}): {stdout}"
        );
    }
}

#[test]
fn test_truncated_stop_payload_allows_stop_instead_of_looping() {
    // Stop のブロックは「停止させず reason を継続プロンプトにする」意味なので、
    // 壊れたペイロードに返すと Stop 再発火の無限ループになる。
    let truncated = r#"{"hook_event_name":"Stop","stop_hook_active":"#;
    let (stdout, _stderr, exit_code) = run_hook(truncated);

    assert_eq!(exit_code, 0, "停止を許可すべき: {stdout}");
    assert_eq!(stdout.trim(), "{}", "停止を許可すべき: {stdout}");
}

#[test]
fn test_truncated_pre_tool_use_payload_still_fails_closed() {
    // 実行前ゲートは壊れた入力でもブロックを維持する。
    let truncated =
        r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"#;
    let (_stdout, stderr, exit_code) = run_hook(truncated);

    assert_eq!(exit_code, 2, "フェイルクローズドを維持すべき: {stderr}");
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
fn test_claude_stop_without_loop_guard_allows_stop() {
    // stop_hook_active が欠落した壊れた入力では Stop hooks を実行しない。
    // Stop のブロックは継続指示になるため、フェイルクローズすると自己維持ループを招く。
    let input = r#"{"hook_event_name":"Stop"}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "Malformed Stop must be allowed to terminate");
    let body = stdout.trim();
    assert!(
        body.is_empty() || body == "{}",
        "Malformed Stop must not carry a continuation decision: {stdout}"
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
fn test_init_command_does_not_load_invalid_config() {
    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let invalid_config = temp_dir.path().join("invalid.toml");
    let generated_config = temp_dir.path().join("generated.toml");
    std::fs::write(&invalid_config, "invalid = [").expect("Failed to create invalid config");

    let output = Command::new(env!("CARGO_BIN_EXE_claw-hooks"))
        .arg("--config")
        .arg(&invalid_config)
        .arg("init")
        .arg("--path")
        .arg(&generated_config)
        .output()
        .expect("Failed to run init command");

    assert!(
        output.status.success(),
        "init should work even when the existing config is invalid: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        generated_config.exists(),
        "init should create the requested config"
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
fn test_version_subcommand_does_not_load_invalid_config() {
    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let invalid_config = temp_dir.path().join("invalid.toml");
    std::fs::write(&invalid_config, "invalid = [").expect("Failed to create invalid config");

    let output = Command::new(env!("CARGO_BIN_EXE_claw-hooks"))
        .arg("--config")
        .arg(&invalid_config)
        .arg("version")
        .output()
        .expect("Failed to run version subcommand");

    assert!(
        output.status.success(),
        "version should work even when the config is invalid: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("claw-hooks"));
}

#[test]
fn test_block_dd_command_by_default() {
    // dd_block はデフォルトで true のため dd コマンドはブロックされる
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"dd if=/dev/zero of=test.img bs=1M count=1"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "dd command should be blocked by default");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate deny: {}",
        stdout
    );
}

#[test]
fn test_invalid_json_input() {
    let input = "not valid json";
    let (_stdout, stderr, exit_code) = run_hook(input);

    // 不正な JSON はエラーとして扱われる
    assert_ne!(exit_code, 0, "Invalid JSON should fail");
    assert!(
        stderr.contains("Failed to parse"),
        "Should indicate parsing failure: {}",
        stderr
    );
}

// === Cursor フォーマットテスト ===

#[test]
fn test_cursor_format_allow_safe_command() {
    let input = r#"{"hook_event_name":"beforeShellExecution","command":"git status","cwd":"/path/to/project"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(exit_code, 0, "Safe command should be allowed");
    assert_eq!(
        stdout.trim(),
        "{}",
        "Cursor output should indicate allow (abstain): {}",
        stdout
    );
}

#[test]
fn test_cursor_format_block_rm_command() {
    let input = r#"{"hook_event_name":"beforeShellExecution","command":"rm -rf /tmp/test","cwd":"/path/to/project"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    // Cursor は exit 0 のときだけ stdout の JSON を使う。deny は JSON 契約で伝える
    // （exit 2 だと user_message の代替案がエージェントに届かない）。
    assert_eq!(exit_code, 0, "rm command should be blocked via deny JSON");
    assert!(
        stdout.contains(r#""permission":"deny""#),
        "Cursor output should indicate deny: {}",
        stdout
    );
    // ブロックメッセージは config の rm_block_message で変更できる
}

#[test]
fn test_cursor_format_block_kill_command() {
    let input = r#"{"hook_event_name":"beforeShellExecution","command":"kill -9 1234","cwd":"/path/to/project"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(exit_code, 0, "kill command should be blocked via deny JSON");
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

    // afterFileEdit は常に許可される PostToolUse 相当として扱う。
    // claw-hooks は deny-only ポリシーなので、許可時は明示 allow ではなく {} を返し、
    // Cursor 本来の権限設定と他フックの判定を上書きしない。
    assert_eq!(exit_code, 0, "afterFileEdit should be allowed");
    assert_eq!(
        stdout.trim(),
        "{}",
        "Cursor output should abstain: {}",
        stdout
    );
}

#[test]
fn test_cursor_format_after_file_edit_camel_case() {
    // Cursor は camelCase の filePath を送る場合もある
    let input = r#"{"hook_event_name":"afterFileEdit","filePath":"/path/to/component.tsx"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(exit_code, 0, "afterFileEdit should be allowed");
    assert_eq!(
        stdout.trim(),
        "{}",
        "Cursor output should indicate allow (abstain): {}",
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
    assert_eq!(
        stdout.trim(),
        "{}",
        "未対応イベントは allow (abstain): {}",
        stdout
    );
}

#[test]
fn test_cursor_format_pre_tool_use_shell_blocks_rm() {
    // Cursor の preToolUse Shell 経路でも危険コマンドをブロックする
    let input = r#"{"hook_event_name":"preToolUse","tool_name":"Shell","tool_input":{"command":"rm -rf /"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(
        exit_code, 0,
        "preToolUse Shell の rm は deny JSON でブロックされるべき"
    );
    assert!(
        stdout.contains(r#""permission":"deny""#),
        "preToolUse Shell は deny: {}",
        stdout
    );
}

#[test]
fn test_cursor_format_pre_tool_use_non_shell_passthrough() {
    // Shell 以外の preToolUse は claw-hooks の対象外でパススルー
    let input =
        r#"{"hook_event_name":"preToolUse","tool_name":"Read","tool_input":{"path":"README.md"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(exit_code, 0, "preToolUse Read はパススルーされるべき");
    assert_eq!(
        stdout.trim(),
        "{}",
        "preToolUse Read は allow (abstain): {}",
        stdout
    );
}

// === Windsurf フォーマットテスト ===

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
    // Windsurf は exit code 2 でブロック時、stderr からプレーンテキストのメッセージを読み取る。
    // メッセージ本文は設定（rm_block_message）でカスタマイズ可能なため、ここでは
    // 「生 JSON ではないプレーンテキストのメッセージが存在すること」を検証する。
    assert!(
        !stderr.trim().is_empty(),
        "Windsurf block message should be present on stderr: {}",
        stderr
    );
    assert!(
        !stderr.contains(r#""decision""#),
        "Windsurf block message should be plain text, not raw JSON: {}",
        stderr
    );
}

#[test]
fn test_windsurf_format_block_kill_command() {
    let input = r#"{"agent_action_name":"pre_run_command","tool_info":{"command_line":"pkill node","cwd":"/path/to/project"}}"#;
    let (_stdout, stderr, exit_code) = run_hook_with_format(input, "windsurf");

    assert_eq!(exit_code, 2, "pkill command should be blocked");
    // Windsurf は exit code 2 でブロック時、stderr からプレーンテキストのメッセージを読み取る。
    // メッセージ本文は設定（kill_block_message）でカスタマイズ可能なため、ここでは
    // 「生 JSON ではないプレーンテキストのメッセージが存在すること」を検証する。
    assert!(
        !stderr.trim().is_empty(),
        "Windsurf block message should be present on stderr: {}",
        stderr
    );
    assert!(
        !stderr.contains(r#""decision""#),
        "Windsurf block message should be plain text, not raw JSON: {}",
        stderr
    );
}

#[test]
fn test_windsurf_format_post_write_code() {
    let input =
        r#"{"agent_action_name":"post_write_code","tool_info":{"file_path":"/path/to/file.rs"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "windsurf");

    // PostToolUse 相当イベントは監視用途のため許可される
    assert_eq!(exit_code, 0, "post_write_code should be allowed");
    // Windsurf Allow: 空 JSON（decision 省略）
    assert_eq!(
        stdout.trim(),
        "{}",
        "Windsurf allow should return empty JSON: {}",
        stdout
    );
}

// === Stop イベントテスト ===

#[test]
fn test_cursor_format_stop_completed() {
    // Cursor の stop フックで status=completed を受け取るケース
    // hook_event_name を含めて実際の stop ディスパッチ経路（CursorStopInput）を通す
    let input = r#"{"hook_event_name":"stop","status":"completed","loop_count":2}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    // Stop イベントは監視用途のため許可される。Stop+Allow は空オブジェクトを返す（permission は出さない）
    assert_eq!(exit_code, 0, "stop event should be allowed");
    assert_eq!(
        stdout.trim(),
        "{}",
        "Cursor stop allow should return empty JSON: {}",
        stdout
    );
}

#[test]
fn test_cursor_format_stop_aborted() {
    // Cursor の stop フックで status=aborted を受け取るケース
    // hook_event_name を含めて実際の stop ディスパッチ経路（CursorStopInput）を通す
    let input = r#"{"hook_event_name":"stop","status":"aborted"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(exit_code, 0, "stop event should be allowed");
    assert_eq!(
        stdout.trim(),
        "{}",
        "Cursor stop allow should return empty JSON: {}",
        stdout
    );
}

#[test]
fn test_cursor_format_stop_error() {
    // Cursor の Stop フックで status=error を受け取るケース
    // hook_event_name を含めて実際の stop ディスパッチ経路（CursorStopInput）を通す
    let input = r#"{"hook_event_name":"stop","status":"error","loop_count":0}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "cursor");

    assert_eq!(exit_code, 0, "stop event should be allowed");
    assert_eq!(
        stdout.trim(),
        "{}",
        "Cursor stop allow should return empty JSON: {}",
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
    run_hook_with_config_format_and_event(json_input, format, config_path, None)
}

fn run_hook_with_config_format_and_event(
    json_input: &str,
    format: &str,
    config_path: &std::path::Path,
    event: Option<&str>,
) -> (String, String, i32) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_claw-hooks"));
    command
        .arg("run")
        .arg("--format")
        .arg(format)
        .arg("--config")
        .arg(config_path);
    if let Some(event) = event {
        command.arg("--event").arg(event);
    }

    let mut child = command
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

/// 生のバイト列（不正な UTF-8 を含み得る）を stdin に渡して実行し、
/// `(stdout, exit_code)` を返す。
fn run_hook_raw_bytes(input: &[u8], format: &str, config_path: &std::path::Path) -> (Vec<u8>, i32) {
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
        stdin.write_all(input).unwrap();
    }

    let output = child.wait_with_output().expect("Failed to read output");
    (output.stdout, output.status.code().unwrap_or(-1))
}

#[test]
fn test_invalid_utf8_input_is_fail_closed_codex() {
    // 不正な UTF-8 バイトが混入しても、フェイルクローズ（ブロック）になること。
    // 以前は read_to_string が `?` で即時エラー終了（exit 1・stdout 空）に倒れ、
    // Codex/Gemini では「フック失敗＝判定無視」と解釈され rm が素通りしていた。
    let empty_config = tempfile::Builder::new()
        .prefix("claw-hooks-test-utf8")
        .suffix(".toml")
        .tempfile()
        .expect("Failed to create temp config");
    let mut input = br#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf /tmp/x"}}"#.to_vec();
    input.push(0xff); // 不正な UTF-8 バイト

    let (stdout, exit_code) = run_hook_raw_bytes(&input, "codex", empty_config.path());
    let stdout = String::from_utf8_lossy(&stdout);

    // Codex のフェイルクローズは exit 0 + stdout に block/deny を含む JSON。
    assert_eq!(
        exit_code, 0,
        "Codex fail-closed should exit 0, stdout: {stdout}"
    );
    assert!(
        stdout.contains("block") || stdout.contains("deny"),
        "Invalid UTF-8 must fail closed (block/deny), got: {stdout}"
    );
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

    assert_eq!(exit_code, 0, "yarn command should be blocked");
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
    // テスト入力: echo "not yarn install"; pnpm install
    // yarn はクォート内の引数で、実際のコマンドは pnpm のため許可される
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo \"not yarn install\"; pnpm install"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(
        exit_code, 0,
        "Command with yarn in quotes should be allowed"
    );
    assert_eq!(stdout.trim(), "{}", "Output should abstain: {}", stdout);
}

#[test]
fn test_custom_filter_blocks_direct_yarn_command() {
    // テスト入力: yarn install
    // 直接の yarn コマンドはブロックされる
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"yarn install"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 0, "Direct yarn command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_in_chained_commands() {
    // テスト入力: cd project && yarn add react
    // && の後ろの yarn を検出してブロックする
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cd project && yarn add react"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 0, "yarn in chained command should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_after_pipe() {
    // テスト入力: cat package.json | yarn install
    // パイプ後の yarn を検出してブロックする
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cat package.json | yarn install"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 0, "yarn after pipe should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_in_sh_c() {
    // テスト入力: sh -c "yarn install"
    // sh -c 内の yarn を検出してブロックする
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"sh -c \"yarn install\""}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 0, "yarn in sh -c should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_in_bash_c() {
    // テスト入力: bash -c "yarn add react"
    // bash -c 内の yarn を検出してブロックする
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"bash -c \"yarn add react\""}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 0, "yarn in bash -c should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_in_subshell() {
    // テスト入力: (cd project && yarn install)
    // サブシェル内の yarn を検出してブロックする
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"(cd project && yarn install)"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 0, "yarn in subshell should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_in_command_substitution() {
    // テスト入力: echo $(yarn --version)
    // コマンド置換内の yarn を検出してブロックする
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo $(yarn --version)"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(
        exit_code, 0,
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
    // テスト入力: echo "yarn" | grep yarn
    // yarn は文字列引数でコマンドではないため許可される
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo \"yarn\" | grep yarn"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 0, "yarn as string argument should be allowed");
    assert_eq!(stdout.trim(), "{}", "Output should abstain: {}", stdout);
}

#[test]
fn test_custom_filter_blocks_yarn_in_complex_pipeline() {
    // テスト入力: cat package.json | jq '.dependencies' | yarn install
    // 複雑なパイプライン末尾の yarn を検出してブロックする
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cat package.json | jq '.dependencies' | yarn install"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 0, "yarn in complex pipeline should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_custom_filter_blocks_yarn_with_env_prefix() {
    // テスト入力: NODE_ENV=production yarn build
    // 環境変数プレフィックス付きの yarn を検出してブロックする
    let (config_path, _temp_dir) = create_custom_filter_config();
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"NODE_ENV=production yarn build"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_config(input, &config_path);

    assert_eq!(exit_code, 0, "yarn with env prefix should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

// === フェイルクローズのエラーハンドリングテスト ===

#[test]
fn test_empty_input_is_fail_closed() {
    let (_stdout, stderr, exit_code) = run_hook("");

    assert_eq!(
        exit_code, 2,
        "Empty input should result in block (fail-closed)"
    );
    assert!(
        stderr.contains("fail-closed"),
        "Output should indicate fail-closed: {}",
        stderr
    );
    // Claude は exit 2 のとき stderr 本文をプレーンテキストのエラーとして扱うため、JSON ではなく本文を出す
    assert!(
        !stderr.contains(r#""decision":"block""#),
        "Claude fail-closed should be plain text, not JSON: {}",
        stderr
    );
}

#[test]
fn test_malformed_json_is_fail_closed() {
    let (_stdout, stderr, exit_code) = run_hook("{");

    assert_eq!(
        exit_code, 2,
        "Malformed JSON should result in block (fail-closed)"
    );
    assert!(
        stderr.contains("fail-closed"),
        "Output should indicate fail-closed: {}",
        stderr
    );
}

#[test]
fn test_unknown_event_is_passthrough() {
    // 未対応イベントは claw-hooks のスコープ外なのでパススルーで Allow を返す。
    // Cursor / Codex / Antigravity と同じ挙動に揃える。
    let input = r#"{"hook_event_name":"Bogus","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(
        exit_code, 0,
        "Unknown event should be allowed (pass-through), got: {}",
        stdout
    );
    assert!(
        stdout.contains("\"permissionDecision\":\"allow\"") || stdout.trim() == "{}",
        "Output should indicate allow: {}",
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
    let input = complete_codex_input(r#"{"hook_event_name":"PreToolUse","tool_name":"Bash"}"#);
    let (stdout, _stderr, exit_code) = run_hook_with_format(&input, "codex");

    assert_eq!(
        exit_code, 0,
        "Codex missing tool_input should exit 0 with block in JSON"
    );
    assert!(
        stdout.contains("Missing tool_input field"),
        "Output should mention the missing required field: {}",
        stdout
    );
    // PreToolUse のパースエラーは推奨形式 hookSpecificOutput.permissionDecision="deny" で返す
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed["hookSpecificOutput"]["hookEventName"], "PreToolUse",
        "Output should use PreToolUse hookSpecificOutput: {}",
        stdout
    );
    assert_eq!(
        parsed["hookSpecificOutput"]["permissionDecision"], "deny",
        "Output should indicate deny: {}",
        stdout
    );
}

#[test]
fn test_codex_missing_used_field_is_fail_closed() {
    // 判定に使わない共通メタデータ（model 等）の欠落は許容する。
    // 一方、危険コマンド検出の入力そのもの（tool_input）が欠けていれば fail-closed。
    let complete = complete_codex_input(
        r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"git status"}}"#,
    );

    let mut tolerated: serde_json::Value = serde_json::from_str(&complete).unwrap();
    tolerated.as_object_mut().unwrap().remove("model");
    let (stdout, _stderr, exit_code) = run_hook_with_format(&tolerated.to_string(), "codex");
    assert_eq!(exit_code, 0, "model 欠落は許容されるべき: {}", stdout);
    assert_eq!(
        stdout.trim(),
        "{}",
        "安全なコマンドは判定なしで素通しされるべき: {}",
        stdout
    );

    let mut missing: serde_json::Value = serde_json::from_str(&complete).unwrap();
    missing.as_object_mut().unwrap().remove("tool_input");
    let (stdout, _stderr, exit_code) = run_hook_with_format(&missing.to_string(), "codex");

    assert_eq!(
        exit_code, 0,
        "Codex fail-closed should exit 0 with deny in JSON"
    );
    assert!(
        stdout.contains("Missing tool_input field"),
        "Output should mention the missing field: {}",
        stdout
    );
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed["hookSpecificOutput"]["hookEventName"], "PreToolUse",
        "Output should use PreToolUse hookSpecificOutput: {}",
        stdout
    );
    assert_eq!(
        parsed["hookSpecificOutput"]["permissionDecision"], "deny",
        "Output should indicate deny: {}",
        stdout
    );
}

// === ラッパー・サブシェル検出テスト ===

#[test]
fn test_block_sudo_rm() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"sudo rm -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "sudo rm should be blocked");
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

    assert_eq!(exit_code, 0, "sudo -n rm should be blocked");
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

    assert_eq!(exit_code, 0, "sudo --user root rm should be blocked");
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
        exit_code, 0,
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

    assert_eq!(exit_code, 0, "command rm should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_block_wrapper_wrapped_reevaluation_rm() {
    // ラッパー（sudo/timeout）配下に再評価系コマンド（eval/xargs/find -exec）が
    // 来ても内側の rm を検出してブロックする。以前はラッパー展開が shell -c しか
    // 再評価せず、これらが素通り（fail-open）していた。
    for command in [
        "sudo eval 'rm -rf /tmp/test'",
        "sudo xargs rm",
        "sudo find . -exec rm {} +",
        "timeout 10 eval 'rm -rf /tmp/test'",
    ] {
        let input = format!(
            r#"{{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{{"command":"{}"}}}}"#,
            command.replace('\\', "\\\\").replace('"', "\\\"")
        );
        let (stdout, _stderr, exit_code) = run_hook(&input);

        assert_eq!(exit_code, 0, "{command} should be processed");
        assert!(
            stdout.contains(r#""permissionDecision":"deny""#),
            "wrapper+re-eval should be blocked for {command:?}: {stdout}"
        );
    }
}

#[test]
fn test_allow_wrapper_echo_with_dangerous_string_arg() {
    // ラッパー配下の echo の引数にコマンド区切り文字と危険コマンド名を含む文字列が
    // あっても、コマンド位置ではないので誤ってブロックしない（false positive 防止）。
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"sudo echo '; rm -rf /tmp/test'"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(
        exit_code, 0,
        "sudo echo with quoted string should be allowed"
    );
    assert_eq!(
        stdout.trim(),
        "{}",
        "quoted argument must not be treated as a command: {stdout}"
    );
}

#[test]
fn test_block_bash_c_rm() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"bash -c 'rm -rf /tmp/test'"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "bash -c rm should be blocked");
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

    assert_eq!(exit_code, 0, "sudo kill should be blocked");
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

    assert_eq!(exit_code, 0, "sudo dd should be blocked");
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

    assert_eq!(exit_code, 0, "xargs kill should be blocked");
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

    assert_eq!(exit_code, 0, "rm in subshell should be blocked");
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

    assert_eq!(exit_code, 0, "rm in command substitution should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_block_rm_in_eval() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"eval 'rm -rf /tmp/test'"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "rm in eval should be blocked");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "Output should indicate block: {}",
        stdout
    );
}

#[test]
fn test_block_rm_in_find_exec() {
    let input = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"find . -name '*.tmp' -exec rm -rf {} \\;"}}"#;
    let (stdout, _stderr, exit_code) = run_hook(input);

    assert_eq!(exit_code, 0, "rm in find -exec should be blocked");
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
    let input = complete_codex_input(
        r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"git status"},"session_id":"test-session","cwd":"/tmp"}"#,
    );
    let (stdout, _stderr, exit_code) = run_hook_with_format(&input, "codex");

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
    let input = complete_codex_input(
        r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf /"},"session_id":"test-session","cwd":"/tmp"}"#,
    );
    let (stdout, _stderr, exit_code) = run_hook_with_format(&input, "codex");

    // Codex: 非0終了コードはフック失敗扱いのため exit 0 + JSON で block を伝達
    assert_eq!(exit_code, 0, "Codex block should still exit 0");
    // PreToolUse の Block は推奨形式 hookSpecificOutput.permissionDecision="deny"
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed["hookSpecificOutput"]["hookEventName"], "PreToolUse",
        "Output should use PreToolUse hookSpecificOutput: {}",
        stdout
    );
    assert_eq!(
        parsed["hookSpecificOutput"]["permissionDecision"], "deny",
        "Output should contain deny decision: {}",
        stdout
    );
    assert!(
        parsed["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .is_some_and(|r| !r.is_empty()),
        "Output should contain non-empty reason: {}",
        stdout
    );
}

#[test]
fn test_codex_format_permission_request_allows_safe_command() {
    let input = complete_codex_input(
        r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"git status","description":"inspect repository"},"session_id":"test-session","cwd":"/tmp","model":"gpt-5.4"}"#,
    );
    let (stdout, _stderr, exit_code) = run_hook_with_format(&input, "codex");

    assert_eq!(exit_code, 0, "Codex PermissionRequest allow should exit 0");
    // 安全な PermissionRequest では承認を代行せず、通常の承認フローへ委ねる。
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({}),
        "Safe PermissionRequest should return empty JSON"
    );
}

#[test]
fn test_codex_format_permission_request_blocks_rm_command() {
    // rm ブロックメッセージは設定でカスタマイズできる。ユーザのグローバル設定に
    // 依存しないよう、メッセージを明示した設定ファイルを渡して検証する。
    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let config_path = temp_dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "rm_block = true\nrm_block_message = \"🚫 Use safe-rm instead of rm\"\n",
    )
    .expect("Failed to write config");

    let input = complete_codex_input(
        r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"rm -rf /","description":"cleanup"},"session_id":"test-session","cwd":"/tmp","model":"gpt-5.4"}"#,
    );
    let (stdout, _stderr, exit_code) =
        run_hook_with_config_and_format(&input, "codex", &config_path);

    assert_eq!(exit_code, 0, "Codex PermissionRequest block should exit 0");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed["hookSpecificOutput"]["hookEventName"],
        "PermissionRequest"
    );
    assert_eq!(parsed["hookSpecificOutput"]["decision"]["behavior"], "deny");
    assert!(
        parsed["hookSpecificOutput"]["decision"]["message"]
            .as_str()
            .unwrap()
            .contains("safe-rm"),
        "PermissionRequest should return the configured rm block message: {}",
        stdout
    );
}

#[test]
fn test_codex_format_block_kill_command() {
    let input = complete_codex_input(
        r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"kill -9 1234"},"session_id":"test-session","cwd":"/tmp"}"#,
    );
    let (stdout, _stderr, exit_code) = run_hook_with_format(&input, "codex");

    assert_eq!(exit_code, 0, "Codex block should still exit 0");
    // PreToolUse の Block は推奨形式 hookSpecificOutput.permissionDecision="deny"
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed["hookSpecificOutput"]["permissionDecision"], "deny",
        "Output should contain deny decision: {}",
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
    let input = complete_codex_input(
        r#"{"hook_event_name":"Stop","session_id":"test-session","cwd":"/tmp","stop_hook_active":false,"last_assistant_message":null}"#,
    );
    let (stdout, _stderr, exit_code) = run_hook_with_format(&input, "codex");

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
    let input = complete_codex_input(
        r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"echo done"},"session_id":"test-session","cwd":"/tmp"}"#,
    );
    let (stdout, _stderr, exit_code) = run_hook_with_format(&input, "codex");

    // Codex PostToolUse + Bash はコマンド出力確認用のパススルーとして扱う
    assert_eq!(exit_code, 0, "Codex PostToolUse should exit 0");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({}),
        "Codex PostToolUse passthrough should return empty JSON"
    );
}

#[test]
fn test_codex_format_post_tool_use_apply_patch_allows() {
    let input = complete_codex_input(
        r#"{"hook_event_name":"PostToolUse","tool_name":"apply_patch","tool_input":{"command":"*** Begin Patch\n*** Update File: src/main.rs\n@@\n-old\n+new\n*** End Patch\n"},"session_id":"test-session","cwd":"/tmp"}"#,
    );
    let (stdout, _stderr, exit_code) = run_hook_with_format(&input, "codex");

    assert_eq!(exit_code, 0, "Codex apply_patch PostToolUse should exit 0");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({}),
        "Codex apply_patch without configured extension hooks should allow"
    );
}

#[test]
fn test_codex_format_subagent_start_allows() {
    let input = complete_codex_input(
        r#"{"hook_event_name":"SubagentStart","session_id":"test-session","turn_id":"turn-1","agent_id":"agent-1","agent_type":"Explore","permission_mode":"default"}"#,
    );
    let (stdout, _stderr, exit_code) = run_hook_with_format(&input, "codex");

    assert_eq!(exit_code, 0, "Codex SubagentStart should exit 0");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({}),
        "Codex SubagentStart should return empty JSON"
    );
}

#[test]
fn test_codex_format_subagent_stop_allows() {
    let input = complete_codex_input(
        r#"{"hook_event_name":"SubagentStop","session_id":"test-session","turn_id":"turn-1","agent_id":"agent-1","agent_type":"Plan","agent_transcript_path":"/tmp/subagent.jsonl","stop_hook_active":false,"last_assistant_message":"Done"}"#,
    );
    let (stdout, _stderr, exit_code) = run_hook_with_format(&input, "codex");

    assert_eq!(exit_code, 0, "Codex SubagentStop should exit 0");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({}),
        "Codex SubagentStop should return empty JSON"
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
    let input = complete_codex_input(
        r#"{"hook_event_name":"SessionStart","session_id":"test-session","cwd":"/tmp","source":"startup"}"#,
    );
    let (stdout, _stderr, exit_code) = run_hook_with_format(&input, "codex");

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
    let input = complete_codex_input(
        r#"{"hook_event_name":"UserPromptSubmit","session_id":"test-session","cwd":"/tmp","prompt":"hello"}"#,
    );
    let (stdout, _stderr, exit_code) = run_hook_with_format(&input, "codex");

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

// === Antigravity CLI フォーマット ===

#[test]
fn test_agy_format_block_rm_command() {
    let input = r#"{
        "toolCall": {
            "name": "run_command",
            "args": {"CommandLine": "rm -rf /tmp/test"}
        },
        "stepIdx": 1,
        "conversationId": "test-conversation",
        "workspacePaths": ["/tmp"],
        "transcriptPath": "/tmp/transcript.json",
        "artifactDirectoryPath": "/tmp/artifacts",
        "modelName": "test-model"
    }"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format_and_event(input, "agy", "PreToolUse");

    assert_eq!(exit_code, 0);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["decision"], "deny");
    assert!(!parsed["reason"].as_str().unwrap().trim().is_empty());
}

#[test]
fn test_agy_format_allow_safe_command() {
    let input = r#"{
        "toolCall": {
            "name": "run_command",
            "args": {"CommandLine": "echo safe"}
        },
        "stepIdx": 1,
        "conversationId": "test-conversation"
    }"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format_and_event(input, "agy", "PreToolUse");

    assert_eq!(exit_code, 0);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["decision"], "allow");
}

#[test]
fn test_agy_stop_event_uses_required_decision() {
    let input = r#"{
        "fullyIdle": true,
        "executionNum": 1,
        "terminationReason": "success",
        "conversationId": "test-conversation",
        "workspacePaths": ["/tmp"],
        "transcriptPath": "/tmp/transcript.json",
        "artifactDirectoryPath": "/tmp/artifacts",
        "modelName": "test-model"
    }"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format_and_event(input, "agy", "Stop");

    assert_eq!(exit_code, 0);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["decision"], "stop");
}

#[test]
fn test_agy_malformed_stop_event_uses_required_decision() {
    let input = r#"{"fullyIdle":"invalid"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format_and_event(input, "agy", "Stop");

    assert_eq!(exit_code, 0);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["decision"], "stop");
}

// === Grok CLI フォーマット ===

#[test]
fn test_grok_format_block_rm_command() {
    let input = r#"{"hookEventName":"PreToolUse","sessionId":"s","cwd":"/tmp","workspaceRoot":"/tmp","toolName":"Bash","toolInput":{"command":"rm -rf /tmp/test"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "grok");

    // Grok は「exit 0 = 許可、exit 2 = 拒否、それ以外はフェイルオープン」。
    // deny JSON と exit 2 の両方で拒否を表明し、どちらの解釈でもブロックを成立させる。
    assert_eq!(exit_code, 2, "rm command should be denied: {}", stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["decision"], "deny");
    // 本文は設定（rm_block_message）でカスタマイズ可能なため、
    // ここでは理由が空でないことだけを検証する。
    assert!(
        !parsed["reason"].as_str().unwrap().trim().is_empty(),
        "reason should explain the block: {}",
        stdout
    );
}

#[test]
fn test_grok_format_allow_safe_command() {
    let input = r#"{"hookEventName":"PreToolUse","sessionId":"s","toolName":"Bash","toolInput":{"command":"git status"}}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "grok");

    assert_eq!(exit_code, 0, "safe command should be allowed");
    assert_eq!(stdout.trim(), "{}", "allow should be an empty object");
}

#[test]
fn test_grok_format_unsupported_event_passthrough() {
    let input = r#"{"hookEventName":"SessionStart","sessionId":"s"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "grok");

    assert_eq!(exit_code, 0, "out-of-scope event should pass through");
    assert_eq!(stdout.trim(), "{}");
}

#[test]
fn test_grok_empty_input_is_fail_closed() {
    let (stdout, _stderr, exit_code) = run_hook_with_format("", "grok");

    assert_eq!(exit_code, 2, "empty input should be denied");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["decision"], "deny");
}

#[test]
fn test_grok_invalid_json_is_fail_closed() {
    let (stdout, _stderr, exit_code) = run_hook_with_format("{not json", "grok");

    assert_eq!(exit_code, 2, "malformed input should be denied");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["decision"], "deny");
    assert!(parsed["reason"].as_str().unwrap().contains("fail-closed"));
}

#[test]
fn test_grok_stop_event_allows_stop() {
    let input = r#"{"hookEventName":"Stop","sessionId":"s"}"#;
    let (stdout, _stderr, exit_code) = run_hook_with_format(input, "grok");

    assert_eq!(exit_code, 0, "Stop is a post-hook and cannot be blocked");
    assert_eq!(stdout.trim(), "{}");
}

// === 設定エラー時のフェイルクローズ ===

/// 壊れた TOML の設定ファイルを作成する。
fn create_broken_config() -> (std::path::PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("Failed to create temp dir");
    let path = dir.path().join("broken.toml");
    std::fs::write(&path, "this is not = valid toml [[[\n").expect("Failed to write config");
    (path, dir)
}

#[test]
fn test_config_error_still_blocks_dangerous_command() {
    // 設定エラーを exit 1 + stdout 空で終了させると、Codex / Antigravity では
    // 「フック失敗＝判定を無視」と解釈され、TOML のタイポ 1 つで危険コマンドの
    // ブロックが全て無効化される（フェイルオープン）。全フォーマットでブロックを維持する。
    let (config_path, _dir) = create_broken_config();
    let cases = [
        (
            "claude",
            r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf /tmp/x"}}"#,
        ),
        (
            "codex",
            r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf /tmp/x"}}"#,
        ),
        (
            "cursor",
            r#"{"hook_event_name":"beforeShellExecution","command":"rm -rf /tmp/x"}"#,
        ),
        (
            "windsurf",
            r#"{"agent_action_name":"pre_run_command","tool_info":{"command_line":"rm -rf /tmp/x"}}"#,
        ),
        (
            "agy",
            r#"{"stepIdx":1,"toolCall":{"name":"run_command","args":{"CommandLine":"rm -rf /tmp/x"}}}"#,
        ),
        (
            "grok",
            r#"{"hookEventName":"PreToolUse","toolName":"Bash","toolInput":{"command":"rm -rf /tmp/x"}}"#,
        ),
    ];

    for (format, input) in cases {
        let (stdout, stderr, exit_code) =
            run_hook_with_config_and_format(input, format, &config_path);

        // 設定エラーの詳細は stderr に出し、エージェントへ返す本文には含めない
        assert!(
            stderr.contains("configuration error"),
            "{format}: config error should be reported on stderr: {stderr}"
        );

        let blocked = match format {
            // exit 2 + stderr 本文でブロックを表現するフォーマット
            "claude" | "windsurf" => exit_code == 2,
            // stdout の JSON でブロックを表現するフォーマット
            _ => {
                let json: serde_json::Value = serde_json::from_str(stdout.trim())
                    .unwrap_or_else(|e| panic!("{format}: stdout is not JSON ({e}): {stdout}"));
                json["decision"] == "deny"
                    || json["permission"] == "deny"
                    || json["hookSpecificOutput"]["permissionDecision"] == "deny"
            }
        };
        assert!(
            blocked,
            "{format}: dangerous command must stay blocked on config error \
             (stdout={stdout}, exit={exit_code})"
        );
    }
}

#[test]
fn test_config_error_does_not_loop_stop_event() {
    // Stop 系で「ブロック」は多くのエージェントで継続指示を意味するため、
    // 設定エラーで返すと Stop 再発火 → 同じ失敗 → … の自己維持ループになる。
    // ループ防止層はパース成功後にしか働かないので、停止を許可する。
    let (config_path, _dir) = create_broken_config();
    let cases = [
        (
            "claude",
            r#"{"hook_event_name":"Stop","stop_hook_active":false}"#,
        ),
        (
            "codex",
            r#"{"hook_event_name":"Stop","stop_hook_active":false}"#,
        ),
        (
            "cursor",
            r#"{"hook_event_name":"stop","status":"completed","loop_count":0}"#,
        ),
        (
            "windsurf",
            r#"{"agent_action_name":"post_cascade_response","tool_info":{"response":"done"}}"#,
        ),
        (
            "agy",
            r#"{"executionNum":1,"terminationReason":"model_stop","fullyIdle":true}"#,
        ),
        ("grok", r#"{"hookEventName":"Stop","sessionId":"s"}"#),
    ];

    for (format, input) in cases {
        let (stdout, _stderr, exit_code) =
            run_hook_with_config_and_format(input, format, &config_path);

        assert_eq!(
            exit_code, 0,
            "{format}: Stop must not be blocked on config error (stdout={stdout})"
        );
        // Antigravity は必須 decision、他形式は空出力または空オブジェクトで停止を許可する。
        let body = stdout.trim();
        if format == "agy" {
            let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(parsed, serde_json::json!({"decision": "stop"}));
        } else {
            assert!(
                body.is_empty() || body == "{}",
                "{format}: Stop response must not carry a continue directive: {stdout}"
            );
        }
    }
}
