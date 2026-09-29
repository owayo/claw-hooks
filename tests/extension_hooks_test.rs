//! 拡張子フック（保存後の formatter / linter）の統合テスト。
//!
//! 見つからないコマンドの通知を「セッションにつき 1 回」にする記録は、フック呼び出しをまたいで
//! ユーザーのキャッシュディレクトリ（`dirs::cache_dir()/claw-hooks`）に残る。子プロセスの `HOME` と
//! `XDG_CACHE_HOME` を一時ディレクトリへ向け、開発者の実キャッシュを汚さず、テスト同士で記録を
//! 共有しないようにする（同じセッションとして続けて呼ぶテストだけが、同じ一時ディレクトリを渡す）。
//!
//! Unix に限る。Windows は環境変数でキャッシュの場所を変えられない。また拡張子フックを `cmd /c`
//! 経由で起動するので、プログラムが無くても起動自体は成功し（cmd が 9009 で終わる）、見つからない
//! コマンドの通知の対象にならない。
#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// PATH に無いプログラム名。
const MISSING_TOOL: &str = "claw-hooks-test-missing-tool";

/// 見つからないコマンドの通知を初めて返すときに付く一文。
const NOT_REPEATED: &str = ". This notice is not repeated in this session.";

/// PATH に無いプログラムの通知の文面（後ろの一文なし）。
fn missing_tool_notice() -> String {
    format!("[{MISSING_TOOL}] not started: command not found in PATH")
}

/// 1 つのテストで使う一時ディレクトリ。設定・編集対象のファイル・状態の置き場所を置く。
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            dir: tempfile::Builder::new()
                .prefix("claw-hooks-ext-test")
                .tempdir()
                .expect("Failed to create temp dir"),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// 状態の置き場所に使う `HOME`。同じ Sandbox の呼び出し同士で記録を共有する。
    fn home(&self) -> PathBuf {
        self.path().join("home")
    }

    /// 設定ファイルを書いてパスを返す。
    fn write_config(&self, contents: &str) -> PathBuf {
        let path = self.path().join("config.toml");
        std::fs::write(&path, contents).expect("Failed to write config");
        path
    }

    /// 編集対象のファイルを作ってパスを返す。
    fn write_source(&self, relative: &str) -> PathBuf {
        let path = self.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "x").unwrap();
        path
    }

    /// Claude Code の PostToolUse（Write）を送り、`(stdout, stderr, exit_code)` を返す。
    ///
    /// フックプロセスの作業ディレクトリはこの一時ディレクトリにする（リポジトリの
    /// `.claw-hooks.toml` を拾わないため）。
    fn run_write_hook(
        &self,
        config: &Path,
        file_path: &Path,
        session_id: Option<&str>,
    ) -> (String, String, i32) {
        let mut input = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": file_path.to_str().unwrap(), "content": "x"},
        });
        if let Some(session_id) = session_id {
            input["session_id"] = serde_json::json!(session_id);
        }

        let home = self.home();
        let mut child = Command::new(env!("CARGO_BIN_EXE_claw-hooks"))
            .arg("hook")
            .arg("--format")
            .arg("claude")
            .arg("--config")
            .arg(config)
            .current_dir(self.path())
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", home.join("cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn claw-hooks");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().expect("Failed to read output");
        (
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
            output.status.code().unwrap_or(-1),
        )
    }

    /// PostToolUse を送り、エージェントへ返る追加コンテキストを返す（無ければ None）。
    fn edit(&self, config: &Path, file_path: &Path, session_id: Option<&str>) -> Option<String> {
        let (stdout, stderr, exit_code) = self.run_write_hook(config, file_path, session_id);
        assert_eq!(exit_code, 0, "stdout={stdout} stderr={stderr}");
        let parsed: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout}"));
        parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .map(str::to_string)
    }
}

/// `path` に `contents` を書き、`mode`（`chmod` の 8 進表記）の権限にする。
///
/// ファイルは子プロセス（`sh`）に書かせる。このテストプロセスが書き込み用に開くと、並列の
/// テストがその間に起動した子プロセスが fd の写しを exec まで持ち、Linux ではこのファイルの
/// exec が ETXTBSY（Text file busy）で失敗することがある。
fn write_file_via_child(path: &Path, contents: &str, mode: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let status = Command::new("sh")
        .arg("-c")
        .arg("printf '%s' \"$1\" > \"$0\" && chmod \"$2\" \"$0\"")
        .arg(path)
        .arg(contents)
        .arg(mode)
        .status()
        .expect("Failed to run sh");
    assert!(status.success(), "テスト用のファイルを書けなかった");
}

#[test]
fn test_missing_command_is_named_and_reported_once_per_session() {
    // 依頼の再現: formatter は動き、linter だけが PATH に無い
    let sandbox = Sandbox::new();
    let source = sandbox.write_source("main.zzz");
    let config = sandbox.write_config(&format!(
        "[extension_hooks]\n\".zzz\" = [\"sh -c 'echo FORMATTER-RAN' {{file}}\", \"{MISSING_TOOL} run {{file}}\"]\n"
    ));

    // 動いたフックの出力と、見つからないフックの名前が区別できること。
    // 動かせなかったコマンドの知らせは、出力の切り詰めで消えないよう先頭に置く
    let first = sandbox
        .edit(&config, &source, Some("session-1"))
        .expect("1 回目は通知が返る");
    assert_eq!(
        first,
        format!(
            "{}{NOT_REPEATED}\n[sh] FORMATTER-RAN",
            missing_tool_notice()
        )
    );

    // 同じセッションの 2 回目の編集: コマンドは毎回動かすが、見つからない通知は返さない
    let second = sandbox
        .edit(&config, &source, Some("session-1"))
        .expect("formatter の出力は返る");
    assert_eq!(second, "[sh] FORMATTER-RAN");

    // 別のセッションでは再び知らせる
    let other_session = sandbox
        .edit(&config, &source, Some("session-2"))
        .expect("別のセッションの 1 回目は通知が返る");
    assert!(
        other_session.starts_with(&format!("{}{NOT_REPEATED}", missing_tool_notice())),
        "{other_session}"
    );
}

#[test]
fn test_missing_command_notice_survives_output_truncation() {
    // 追加コンテキストは既定で 1000 文字に切り詰められる（先頭を残す）。見つからない通知は
    // 返した時点で「知らせた」と記録するので、長い lint 出力の後ろで切り落とされると、
    // そのセッションでは二度と届かない。長い出力があっても通知が届くこと
    let sandbox = Sandbox::new();
    let source = sandbox.write_source("main.zzz");
    let config = sandbox.write_config(&format!(
        "[extension_hooks]\n\".zzz\" = [\"sh -c 'seq 1 500 | sed s/^/finding-/' {{file}}\", \"{MISSING_TOOL} run {{file}}\"]\n"
    ));

    let context = sandbox
        .edit(&config, &source, Some("session-1"))
        .expect("出力と通知が返る");

    assert!(
        context.ends_with("... (truncated)"),
        "前提: 出力が切り詰められている: {context}"
    );
    assert!(
        context.starts_with(&format!("{}{NOT_REPEATED}", missing_tool_notice())),
        "{context}"
    );
}

#[test]
fn test_missing_command_notice_repeats_without_session_id() {
    let sandbox = Sandbox::new();
    let source = sandbox.write_source("main.zzz");
    let config = sandbox.write_config(&format!(
        "[extension_hooks]\n\".zzz\" = [\"{MISSING_TOOL} run {{file}}\"]\n"
    ));

    // セッションを識別できなければ記録を残せないので、毎回知らせる（後ろの一文は付けない）
    for _ in 0..2 {
        assert_eq!(
            sandbox.edit(&config, &source, None),
            Some(missing_tool_notice())
        );
    }
}

#[test]
fn test_missing_command_notice_repeats_when_state_directory_is_unusable() {
    let sandbox = Sandbox::new();
    let source = sandbox.write_source("main.zzz");
    let config = sandbox.write_config(&format!(
        "[extension_hooks]\n\".zzz\" = [\"{MISSING_TOOL} run {{file}}\"]\n"
    ));
    // 状態の置き場所（macOS: $HOME/Library/Caches/claw-hooks、Linux: $XDG_CACHE_HOME/claw-hooks）に
    // 通常のファイルを置き、ディレクトリを作れなくする
    for cache_dir in [
        sandbox.home().join("Library").join("Caches"),
        sandbox.home().join("cache"),
    ] {
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("claw-hooks"), "").unwrap();
    }

    // 記録を残せないので、セッション ID があっても毎回知らせる
    for _ in 0..2 {
        assert_eq!(
            sandbox.edit(&config, &source, Some("session-1")),
            Some(missing_tool_notice())
        );
    }
}

#[test]
fn test_start_failures_name_the_command_and_the_cause() {
    let sandbox = Sandbox::new();
    let source = sandbox.write_source("main.zzz");
    let missing = sandbox.path().join("not-installed").join("formatter");
    let needs_interpreter = sandbox.path().join("bin").join("needs-interpreter");
    write_file_via_child(&needs_interpreter, "#!/nonexistent/interpreter\n", "755");
    let not_executable = sandbox.path().join("bin").join("not-executable");
    write_file_via_child(&not_executable, "#!/bin/sh\n", "644");
    let config = sandbox.write_config(&format!(
        "[extension_hooks]\n\".zzz\" = [\"'{}' {{file}}\", \"'{}' {{file}}\", \"'{}' {{file}}\"]\n",
        missing.display(),
        needs_interpreter.display(),
        not_executable.display(),
    ));

    let first = sandbox
        .edit(&config, &source, Some("session-1"))
        .expect("起動の失敗は返る");
    assert_eq!(
        first.lines().collect::<Vec<_>>(),
        [
            format!(
                "[formatter] not started: command not found at the configured path{NOT_REPEATED}"
            )
            .as_str(),
            "[needs-interpreter] not started: executable or required interpreter not found",
            "[not-executable] not started: permission denied",
        ],
        "{first}"
    );
    assert!(
        !first.contains(sandbox.path().to_str().unwrap()),
        "プログラムのディレクトリは文面に出さない: {first}"
    );

    // 同じセッションの 2 回目: 「見つからない」だけを抑え、直すまで毎回起きる失敗は返し続ける
    let second = sandbox
        .edit(&config, &source, Some("session-1"))
        .expect("起動の失敗は返る");
    assert_eq!(
        second.lines().collect::<Vec<_>>(),
        [
            "[needs-interpreter] not started: executable or required interpreter not found",
            "[not-executable] not started: permission denied",
        ],
        "{second}"
    );
}

#[test]
fn test_conditional_entries_run_only_when_their_condition_holds() {
    let sandbox = Sandbox::new();
    let source = sandbox.write_source("main.zzz");
    // 通常のファイルの下のパスは有無を確かめられない（ENOTDIR）ので、条件の評価エラーになる
    let plain_file = sandbox.path().join("plain-file");
    std::fs::write(&plain_file, "").unwrap();
    let unknowable = plain_file.join("child");
    let config = sandbox.write_config(&format!(
        r#"[extension_hooks]
".zzz" = [
  {{ command = "sh -c 'echo SKIPPED-BY-MISSING-COMMAND' {{file}}", condition = {{ command_exists = "{MISSING_TOOL}" }} }},
  {{ command = "sh -c 'echo RAN-WITH-CONDITION' {{file}}", condition = {{ command_exists = "sh" }} }},
  {{ command = "sh -c 'echo SKIPPED-BY-CONDITION-ERROR' {{file}}", condition = {{ file_exists = "{}" }} }},
  {{ command = "{MISSING_TOOL} run {{file}}", condition = {{ command_exists = "{MISSING_TOOL}" }} }},
]
"#,
        unknowable.display()
    ));

    let context = sandbox.edit(&config, &source, Some("session-1"));

    assert_eq!(
        context.as_deref(),
        Some("[sh] RAN-WITH-CONDITION"),
        "条件を満たすコマンドだけが動き、条件で守った見つからないコマンドは通知も出さない"
    );
}

#[test]
fn test_logs_and_agent_context_do_not_contain_paths() {
    // 永続ログ（debug = true）にも、エージェントへ返す文面にも、編集したファイルのパス・
    // プログラムのディレクトリ・条件のパスを残さない
    let sandbox = Sandbox::new();
    let source = sandbox.write_source("private-src-dir/private-file-name.zzz");
    let missing_at_path = sandbox.path().join("private-bin-dir").join("formatter");
    let plain_file = sandbox.path().join("private-condition-file");
    std::fs::write(&plain_file, "").unwrap();
    let log_dir = sandbox.path().join("logs");
    let config = sandbox.write_config(&format!(
        r#"debug = true
log_path = '{}'

[extension_hooks]
".zzz" = [
  "sh -c 'echo FORMATTER-RAN' {{file}}",
  "{MISSING_TOOL} run {{file}}",
  "'{}' {{file}}",
  {{ command = "sh -c 'echo SKIPPED' {{file}}", condition = {{ file_exists = "{}" }} }},
]
"#,
        log_dir.display(),
        missing_at_path.display(),
        plain_file.join("child").display(),
    ));

    let context = sandbox
        .edit(&config, &source, Some("session-1"))
        .expect("出力と通知が返る");

    let mut logs = String::new();
    for entry in std::fs::read_dir(&log_dir).expect("ログが書かれること") {
        logs.push_str(&std::fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    // ログが実際に書かれ、失敗と条件の評価エラーを記録していること
    assert!(
        logs.contains(&format!(
            "Extension hook [{MISSING_TOOL}] not started: kind=NotFound"
        )),
        "{logs}"
    );
    assert!(
        logs.contains("Extension hook [sh] skipped: could not evaluate condition.file_exists"),
        "{logs}"
    );
    for private in [
        "private-src-dir",
        "private-file-name",
        "private-bin-dir",
        "private-condition-file",
    ] {
        assert!(
            !logs.contains(private),
            "永続ログに {private} を残さない: {logs}"
        );
        assert!(
            !context.contains(private),
            "エージェントへの文面に {private} を含めない: {context}"
        );
    }
}
