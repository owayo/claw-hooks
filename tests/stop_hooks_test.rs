//! Stop フックの stage の打ち切り（gate）と、再停止での 1 回だけの再試行の統合テスト。
//!
//! claw-hooks の実バイナリを起動し、エージェントへ返す JSON と、フックが実際に動いた回数を
//! 確かめる。回数は「検査の実行回数」「後続（副作用）の起動回数」を別々のマーカーファイルの
//! 行数で数え、判定（Block の有無）・ユーザー向け通知（`systemMessage`）の有無とは分けて検証する。
//!
//! 再試行の予定はユーザーのキャッシュディレクトリ（`dirs::cache_dir()/claw-hooks`）に置かれる
//! ため、子プロセスの `HOME` / `XDG_CACHE_HOME` を一時ディレクトリへ向けて隔離する。Windows は
//! 環境変数でキャッシュの場所を変えられないので、このファイルのテストは Unix 限定にする。
#![cfg(unix)]

use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 再試行を予定できたときに Block の要約の 3 行目に入る文。
const RETRY_SCHEDULED_LINE: &str = "One retry is scheduled: at the next stop, the reported hooks \
     up to stage 1 run again, and the stages that were not run start if they pass.";

/// テスト 1 つ分の作業場所（設定・マーカー・フックのプログラム・状態の置き場所）。
struct Fixture {
    dir: tempfile::TempDir,
    /// 対照のマーカーの通し番号（確認のたびに別のマーカーを使う）。
    control_seq: Cell<usize>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::Builder::new()
                .prefix("claw-hooks-stop-test")
                .tempdir()
                .expect("Failed to create temp dir"),
            control_seq: Cell::new(0),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// `/bin/sh` を `name` の名前で指すシンボリックリンクを作り、そのパスを返す。
    ///
    /// Block の文面のラベルは実行ファイル名なので、検査と副作用を別の名前にして見分ける。
    /// 書いた直後のスクリプトを直接実行すると、並列のテストが fork した子に書き込み中の
    /// ファイルを握られて ETXTBSY で起動に失敗し得るため、既存の `/bin/sh` を別名で指す。
    fn program(&self, name: &str) -> PathBuf {
        let path = self.path(name);
        if std::fs::symlink_metadata(&path).is_err() {
            std::os::unix::fs::symlink("/bin/sh", &path).expect("Failed to link a program");
        }
        path
    }

    /// `program` を名前にした `sh -c '<script>'` のコマンドを TOML の文字列にする。
    fn command(&self, program: &str, script: &str) -> String {
        toml_string(&format!(
            "{} -c '{}'",
            self.program(program).display(),
            script
        ))
    }

    /// Stop フックの設定を書く。NanoBuddy はテストでデスクトップ通知を出さないよう切る。
    fn write_config(&self, name: &str, stop_hooks: &str) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, format!("nano_buddy = false\n\n{stop_hooks}"))
            .expect("Failed to write config");
        path
    }

    /// stage 1 の検査（report = true。`ok` が無ければ失敗）と、stage 5 の副作用
    /// （report = false = detached。git の自動コミットに相当）の 2 段構成の設定を書く。
    fn gate_config(&self, check_failure_output: &str) -> PathBuf {
        let check = self.command(
            "gate-check",
            &format!(
                "echo run >> {}; test -f {} || {{ {}; exit 1; }}",
                self.path("checks.log").display(),
                self.path("ok").display(),
                check_failure_output
            ),
        );
        let effect = self.command(
            "side-effect",
            &format!("echo run >> {}", self.path("effects.log").display()),
        );
        self.write_config(
            "config.toml",
            &format!(
                "[[stop_hooks]]\ncommands = [{check}]\nstage = 1\nreport = true\n\n\
                 [[stop_hooks]]\ncommands = [{effect}]\nstage = 5\n"
            ),
        )
    }

    /// 検査が通るようにする（エージェントが問題を直したことに相当）。
    fn fix(&self) {
        std::fs::write(self.path("ok"), "").expect("Failed to write ok marker");
    }

    /// 検査の実行回数。
    fn checks(&self) -> usize {
        count_lines(&self.path("checks.log"))
    }

    /// 副作用の起動を `expected` 回まで待つ。
    fn wait_for_effects(&self, expected: usize) -> bool {
        wait_for_lines(&self.path("effects.log"), expected)
    }

    /// 副作用（stage 5 の detached のフック）の起動回数が `expected` のまま増えないことを確かめる。
    ///
    /// 起動しなかったことは「待っても現れない」でしか確かめられず、固定の時間だけ待つと遅い
    /// マシンでは確認が空振りする。同じ経路（claw-hooks が detached で起動する stage 5 のフック）
    /// で対照のマーカーを作らせ、対照が現れてからさらに猶予を置いて数える。
    fn assert_effects_settled(&self, expected: usize) {
        let seq = self.control_seq.get();
        self.control_seq.set(seq + 1);
        let control = self.path(&format!("control-{seq}.log"));
        let command = self.command(
            "control-effect",
            &format!("echo run >> {}", control.display()),
        );
        let config = self.write_config(
            &format!("control-{seq}.toml"),
            &format!("[[stop_hooks]]\ncommands = [{command}]\nstage = 5\n"),
        );
        let (stdout, _stderr, code) = self.run(
            "claude",
            &config,
            &claude_stop("control-session", false),
            &[],
        );
        assert_eq!((code, stdout.trim()), (0, "{}"), "対照の停止");
        assert!(
            wait_for_lines(&control, 1),
            "対照の detached フックが起動しなかった"
        );
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            count_lines(&self.path("effects.log")),
            expected,
            "副作用の起動回数"
        );
    }

    /// 再試行の予定の記録が置かれるディレクトリ（macOS は `$HOME/Library/Caches`、
    /// Linux は `$XDG_CACHE_HOME` の下）。
    fn retry_state_dir(&self) -> PathBuf {
        let home = self.path("home");
        if cfg!(target_os = "macos") {
            home.join("Library/Caches/claw-hooks/stop-retry")
        } else {
            home.join("cache/claw-hooks/stop-retry")
        }
    }

    /// 再試行の予定の記録（中身）の一覧。
    fn retry_records(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.retry_state_dir()) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
            .collect()
    }

    /// claw-hooks を実行し、`(stdout, stderr, exit_code)` を返す。
    ///
    /// 状態の置き場所を作業場所の `home` へ向け、同じテストの中の呼び出しで共有する。
    /// cwd も作業場所にし、リポジトリの `.claw-hooks.toml` などを読まないようにする。
    fn run(
        &self,
        format: &str,
        config: &Path,
        input: &str,
        envs: &[(&str, &str)],
    ) -> (String, String, i32) {
        let home = self.path("home");
        let mut command = Command::new(env!("CARGO_BIN_EXE_claw-hooks"));
        command
            .arg("run")
            .arg("--format")
            .arg(format)
            .arg("--config")
            .arg(config)
            .current_dir(self.dir.path())
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", home.join("cache"))
            // テスト自体が claw-hooks の Stop フックの中で走っていても、判定を飛ばさない
            .env_remove("CLAW_HOOKS_STOP_ACTIVE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("Failed to spawn claw-hooks");
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(input.as_bytes())
            .expect("Failed to write stdin");
        let output = child.wait_with_output().expect("Failed to read output");
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            output.status.code().unwrap_or(-1),
        )
    }
}

/// 文字列を TOML の基本文字列にする（JSON の文字列表記は TOML の基本文字列としても読める）。
fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string is always serializable")
}

/// マーカーの行数（実行回数）。マーカーが無ければ 0。
fn count_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|contents| contents.lines().count())
        .unwrap_or(0)
}

/// マーカーの行数が `expected` に届くまで待つ（detached のフックの起動待ち）。
fn wait_for_lines(path: &Path, expected: usize) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if count_lines(path) >= expected {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    count_lines(path) >= expected
}

/// Claude Code の Stop 入力。
fn claude_stop(session_id: &str, stop_hook_active: bool) -> String {
    serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": session_id,
        "stop_hook_active": stop_hook_active,
        "last_assistant_message": "Done.",
    })
    .to_string()
}

/// Codex CLI の Stop 入力。
fn codex_stop(session_id: &str, stop_hook_active: bool) -> String {
    serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": session_id,
        "turn_id": "turn-1",
        "transcript_path": null,
        "cwd": "/tmp",
        "model": "gpt-5.4",
        "permission_mode": "default",
        "stop_hook_active": stop_hook_active,
        "last_assistant_message": "Done.",
    })
    .to_string()
}

/// Cursor の stop 入力。
fn cursor_stop(conversation_id: &str, loop_count: u32) -> String {
    serde_json::json!({
        "hook_event_name": "stop",
        "conversation_id": conversation_id,
        "status": "completed",
        "loop_count": loop_count,
    })
    .to_string()
}

/// stdout の JSON を読む。
fn parse(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("invalid JSON ({e}): {stdout}"))
}

/// 初回の停止で返る Block の文面の先頭（要約の 3 行と空行、1 件目の失敗の見出し）。
fn expected_block_head() -> String {
    format!(
        "Stop hooks failed: stage 1 [gate-check].\n\
         Not run because a stage 1 hook failed: stage 5 [side-effect].\n\
         {RETRY_SCHEDULED_LINE}\n\
         \n\
         Stop hook failed: gate-check\n"
    )
}

#[test]
fn test_claude_gate_failure_blocks_and_retry_after_fix_starts_skipped_stage() {
    let fx = Fixture::new();
    let config = fx.gate_config("echo gate-check-failed >&2");

    // 初回の停止: 検査が失敗 → Block（先頭に要約）。stage 5 の副作用は起動しない
    let (stdout, _stderr, code) = fx.run("claude", &config, &claude_stop("session-a", false), &[]);
    assert_eq!(code, 0, "{stdout}");
    let output = parse(&stdout);
    assert_eq!(output["decision"], "block", "{stdout}");
    assert_eq!(
        output["reason"].as_str().unwrap(),
        format!("{}gate-check-failed", expected_block_head())
    );
    assert_eq!(fx.checks(), 1);
    fx.assert_effects_settled(0);

    // 予定の記録は隔離した HOME の下に置かれ、セッション ID やコマンドを含まない
    let records = fx.retry_records();
    assert_eq!(records.len(), 1, "再試行の予定が 1 件記録される");
    let record = parse(&records[0]);
    assert_eq!(record["version"], 1);
    assert_eq!(record["failed_stage"], 1);
    assert!(!records[0].contains("session-a"), "{}", records[0]);
    assert!(!records[0].contains("gate-check"), "{}", records[0]);

    // 直した後の継続中の停止: 検査をやり直して通れば副作用を起動し、停止を許す（通知なし）
    fx.fix();
    let (stdout, _stderr, code) = fx.run("claude", &config, &claude_stop("session-a", true), &[]);
    assert_eq!(code, 0);
    assert_eq!(parse(&stdout), serde_json::json!({}), "{stdout}");
    assert_eq!(fx.checks(), 2, "検査をやり直す");
    assert!(fx.wait_for_effects(1), "実行しなかった stage を起動する");
    assert!(fx.retry_records().is_empty(), "予定は消費される");

    // 再試行は 1 回だけ: 次の継続中の停止では何も実行しない
    let (stdout, _stderr, code) = fx.run("claude", &config, &claude_stop("session-a", true), &[]);
    assert_eq!(code, 0);
    assert_eq!(parse(&stdout), serde_json::json!({}), "{stdout}");
    assert_eq!(fx.checks(), 2);
    fx.assert_effects_settled(1);
}

#[test]
fn test_claude_retry_failure_returns_system_message_without_block() {
    let fx = Fixture::new();
    let config = fx.gate_config("echo gate-check-failed >&2");

    let (stdout, _stderr, _code) = fx.run("claude", &config, &claude_stop("session-b", false), &[]);
    assert_eq!(parse(&stdout)["decision"], "block", "{stdout}");

    // 直さないまま継続中の停止: 検査が再び失敗 → 副作用は起動せず、Block もせず、
    // ユーザー向けの systemMessage で知らせる（作業を続けさせるフィールドは付けない）
    let (stdout, _stderr, code) = fx.run("claude", &config, &claude_stop("session-b", true), &[]);
    assert_eq!(code, 0);
    assert_eq!(
        parse(&stdout),
        serde_json::json!({
            "systemMessage": "claw-hooks: stop hook retry failed at stage 1 [gate-check]. \
                              Not run: stage 5 [side-effect]. No further retry is scheduled."
        }),
        "{stdout}"
    );
    assert_eq!(fx.checks(), 2, "検査をやり直す");
    fx.assert_effects_settled(0);

    // 再試行は 1 回だけ: 次の継続中の停止では何も実行せず、通知もしない
    let (stdout, _stderr, _code) = fx.run("claude", &config, &claude_stop("session-b", true), &[]);
    assert_eq!(parse(&stdout), serde_json::json!({}), "{stdout}");
    assert_eq!(fx.checks(), 2);
}

#[test]
fn test_claude_long_failure_output_keeps_summary_after_truncation() {
    // 長い失敗出力でも、adapter で既定の 1000 文字に切り詰めた後に先頭の要約が残る
    let fx = Fixture::new();
    let config = fx.gate_config("yes x | head -n 2500 >&2");

    let (stdout, _stderr, code) = fx.run("claude", &config, &claude_stop("session-c", false), &[]);
    assert_eq!(code, 0);
    let output = parse(&stdout);
    assert_eq!(output["decision"], "block", "{stdout}");
    let reason = output["reason"].as_str().unwrap();
    assert!(
        reason.chars().count() <= 1000,
        "既定の上限で切り詰められる: {} chars",
        reason.chars().count()
    );
    assert!(reason.ends_with("... (truncated)"), "{reason}");
    assert!(reason.starts_with(&expected_block_head()), "{reason}");
    fx.assert_effects_settled(0);
}

#[test]
fn test_stop_active_env_skips_all_stop_hooks() {
    // claw-hooks の Stop フックが起動したプロセスの中の停止（CLAW_HOOKS_STOP_ACTIVE=1）は、
    // 失敗する検査があっても何も実行しない（クロスプロセスのループ防止。再試行より優先）
    let fx = Fixture::new();
    let config = fx.gate_config("echo gate-check-failed >&2");

    let (stdout, _stderr, code) = fx.run(
        "claude",
        &config,
        &claude_stop("session-d", false),
        &[("CLAW_HOOKS_STOP_ACTIVE", "1")],
    );
    assert_eq!(code, 0);
    assert_eq!(parse(&stdout), serde_json::json!({}), "{stdout}");
    assert_eq!(fx.checks(), 0);
    fx.assert_effects_settled(0);
}

#[test]
fn test_codex_retry_failure_returns_system_message_without_block() {
    let fx = Fixture::new();
    let config = fx.gate_config("echo gate-check-failed >&2");

    let (stdout, _stderr, code) = fx.run("codex", &config, &codex_stop("codex-1", false), &[]);
    assert_eq!(code, 0);
    let output = parse(&stdout);
    assert_eq!(output["decision"], "block", "{stdout}");
    assert!(
        output["reason"]
            .as_str()
            .unwrap()
            .starts_with(&expected_block_head()),
        "{stdout}"
    );

    let (stdout, _stderr, code) = fx.run("codex", &config, &codex_stop("codex-1", true), &[]);
    assert_eq!(code, 0);
    assert_eq!(
        parse(&stdout),
        serde_json::json!({
            "systemMessage": "claw-hooks: stop hook retry failed at stage 1 [gate-check]. \
                              Not run: stage 5 [side-effect]. No further retry is scheduled."
        }),
        "{stdout}"
    );
    assert_eq!(fx.checks(), 2);
    fx.assert_effects_settled(0);
}

#[test]
fn test_cursor_retries_once_at_loop_count_one() {
    // Cursor: loop_count = 1 + 予定あり → 再試行、loop_count = 2 → 何も実行しない
    let fx = Fixture::new();
    let config = fx.gate_config("echo gate-check-failed >&2");

    let (stdout, _stderr, code) = fx.run("cursor", &config, &cursor_stop("conv-1", 0), &[]);
    assert_eq!(code, 0);
    let output = parse(&stdout);
    assert!(
        output["followup_message"]
            .as_str()
            .unwrap()
            .starts_with(&expected_block_head()),
        "{stdout}"
    );
    fx.assert_effects_settled(0);

    fx.fix();
    let (stdout, _stderr, code) = fx.run("cursor", &config, &cursor_stop("conv-1", 1), &[]);
    assert_eq!(code, 0);
    assert_eq!(parse(&stdout), serde_json::json!({}), "{stdout}");
    assert_eq!(fx.checks(), 2, "検査をやり直す");
    assert!(fx.wait_for_effects(1), "実行しなかった stage を起動する");

    let (stdout, _stderr, code) = fx.run("cursor", &config, &cursor_stop("conv-1", 2), &[]);
    assert_eq!(code, 0);
    assert_eq!(parse(&stdout), serde_json::json!({}), "{stdout}");
    assert_eq!(fx.checks(), 2);
    fx.assert_effects_settled(1);
}

#[test]
fn test_windsurf_gate_failure_does_not_start_later_stage() {
    // Windsurf は Stop の Block がエージェントへ届かない（事後フック）ので再試行を予定しないが、
    // 後続の stage を止める動作は同じく効く
    let fx = Fixture::new();
    let config = fx.gate_config("echo gate-check-failed >&2");

    let input = r#"{"agent_action_name":"post_cascade_response","tool_info":{"response":"Done."}}"#;
    let (stdout, _stderr, code) = fx.run("windsurf", &config, input, &[]);
    assert_eq!(code, 0);
    assert_eq!(stdout.trim(), "{}", "Windsurf の Stop はベストエフォート");
    assert_eq!(fx.checks(), 1);
    fx.assert_effects_settled(0);
    assert!(fx.retry_records().is_empty(), "再試行を予定しない");
}
