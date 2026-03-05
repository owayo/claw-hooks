//! タイムアウト対応のコマンド実行ユーティリティ。

use std::io::Read as _;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt as _;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt as _;
use tracing::warn;

/// タイムアウトによりコマンドが終了された場合の終了コード。
pub(crate) const TIMEOUT_EXIT_CODE: i32 = 124;

/// タイムアウト時にstderrに付加されるプレフィックス。
const TIMEOUT_STDERR_PREFIX: &str = "[Command timed out after";

/// タイムアウトメタデータ付きのコマンド出力。
pub struct TimedOutput {
    /// キャプチャされたプロセス出力。
    pub output: Output,
    /// `run_with_timeout_tracked` によりタイムアウトでプロセスが強制終了された場合にtrue。
    pub timed_out: bool,
}

#[cfg(unix)]
fn timeout_exit_status() -> ExitStatus {
    // Unixのwaitステータスは上位バイトに終了コードをエンコードする。
    ExitStatus::from_raw(TIMEOUT_EXIT_CODE << 8)
}

#[cfg(windows)]
fn timeout_exit_status() -> ExitStatus {
    ExitStatus::from_raw(TIMEOUT_EXIT_CODE as u32)
}

#[cfg(test)]
/// `run_with_timeout` によるタイムアウト出力かどうかを判定する。
pub fn is_timeout_output(output: &Output) -> bool {
    output.status.code() == Some(TIMEOUT_EXIT_CODE)
        && output.stderr.starts_with(TIMEOUT_STDERR_PREFIX.as_bytes())
}

/// タイムアウト付きでコマンドを実行し、タイムアウトメタデータを返す。
///
/// `Output` のみが必要な場合は `run_with_timeout` を使用する。
pub fn run_with_timeout_tracked(
    mut child: std::process::Child,
    timeout_secs: u64,
    command_desc: &str,
) -> Result<TimedOutput, String> {
    // スレッドで読み取るためにstdout/stderrハンドルの所有権を取得
    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    // スレッドでstdoutを読み取る（パイプバッファのデッドロックを防止）
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stdout) = stdout_handle {
            stdout.read_to_end(&mut buf).ok();
        }
        buf
    });

    // スレッドでstderrを読み取る
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stderr) = stderr_handle {
            stderr.read_to_end(&mut buf).ok();
        }
        buf
    });

    // try_waitポーリングでタイムアウト付きの子プロセス待機
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status, false),
            Ok(None) => {
                if Instant::now() >= deadline {
                    warn!(
                        "⏰ Command timed out after {}s: {}",
                        timeout_secs, command_desc
                    );
                    // 子プロセスを強制終了（Unixの場合SIGKILLを送信）
                    let _ = child.kill();
                    // ゾンビプロセスを回収
                    let _ = child.wait();
                    warn!("💀 Process killed (SIGKILL): {}", command_desc);
                    break (timeout_exit_status(), true);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                return Err(format!(
                    "Failed to wait for command '{}': {}",
                    command_desc, e
                ));
            }
        }
    };

    if timed_out {
        // リーダースレッドをjoinしない：子サブプロセス（例：sh → sleep）が
        // パイプハンドルを保持し続ける可能性があり、join()が無期限にブロックされるため。
        // スレッドはプロセス終了時にクリーンアップされる。
        let msg = format!(
            "{} {}s: {}]\n",
            TIMEOUT_STDERR_PREFIX, timeout_secs, command_desc
        );
        return Ok(TimedOutput {
            output: Output {
                status,
                stdout: Vec::new(),
                stderr: msg.into_bytes(),
            },
            timed_out: true,
        });
    }

    // リーダースレッドから出力を収集（正常なプロセス終了後すぐに完了する）
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    Ok(TimedOutput {
        output: Output {
            status,
            stdout,
            stderr,
        },
        timed_out: false,
    })
}

/// タイムアウト付きでコマンドを実行する。
///
/// パイプ接続されたstdout/stderrでコマンドを起動し、別スレッドで出力を読み取り
/// （パイプバッファのデッドロック防止）、子プロセスの終了またはデッドライン到達まで
/// `try_wait` でポーリングする。タイムアウト時は子プロセスをSIGKILLで強制終了し
/// 回収する。タイムアウト時の終了コードは124。
pub fn run_with_timeout(
    child: std::process::Child,
    timeout_secs: u64,
    command_desc: &str,
) -> Result<Output, String> {
    run_with_timeout_tracked(child, timeout_secs, command_desc).map(|result| result.output)
}

/// stdout/stderrを `/dev/null` に接続し、追加の環境変数でコマンドをデタッチ起動する。
/// fire-and-forget のストップフック用。親プロセス終了後も子プロセスは孤立プロセスとして存続する。
pub fn spawn_detached_with_env(
    program: &str,
    args: &[String],
    envs: &[(&str, &str)],
) -> Result<(), String> {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.arg("/c").arg(program).args(args);
        c
    } else {
        let mut c = Command::new(program);
        c.args(args);
        c
    };
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    for &(key, value) in envs {
        cmd.env(key, value);
    }
    cmd.spawn()
        .map_err(|e| format!("Failed to execute '{}': {}", program, e))?;
    // Child をドロップ — 子プロセスは OS によりデタッチされる
    Ok(())
}

/// パイプ接続されたstdout/stderrと追加の環境変数でコマンドを起動する。
/// ストップフックがループ防止用の環境変数を子プロセスに伝播するために使用。
pub fn spawn_piped_with_env(
    program: &str,
    args: &[String],
    envs: &[(&str, &str)],
) -> Result<std::process::Child, String> {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.arg("/c").arg(program).args(args);
        c
    } else {
        let mut c = Command::new(program);
        c.args(args);
        c
    };
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    for &(key, value) in envs {
        cmd.env(key, value);
    }
    cmd.spawn()
        .map_err(|e| format!("Failed to execute '{}': {}", program, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テストヘルパー：追加の環境変数なしでspawnする。
    fn spawn_piped(program: &str, args: &[String]) -> Result<std::process::Child, String> {
        spawn_piped_with_env(program, args, &[])
    }

    // === spawn_piped テスト ===

    #[test]
    fn test_spawn_piped_valid_command() {
        let child = spawn_piped("echo", &["hello".to_string()]);
        assert!(child.is_ok(), "Should spawn valid command");
        // 子プロセスを回収
        let _ = child.unwrap().wait();
    }

    #[test]
    fn test_spawn_piped_nonexistent_command() {
        let child = spawn_piped("nonexistent-command-xyz-abc-999", &[]);
        assert!(child.is_err(), "Should fail for nonexistent command");
        assert!(
            child.unwrap_err().contains("Failed to execute"),
            "Error should indicate execution failure"
        );
    }

    // === run_with_timeout テスト ===

    #[test]
    fn test_run_with_timeout_captures_stdout() {
        let child = spawn_piped("echo", &["hello-stdout".to_string()]).unwrap();
        let output = run_with_timeout(child, 10, "echo hello-stdout").unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("hello-stdout"),
            "Should capture stdout, got: {}",
            stdout
        );
    }

    #[test]
    fn test_run_with_timeout_captures_stderr() {
        let child = spawn_piped(
            "sh",
            &["-c".to_string(), "echo hello-stderr >&2".to_string()],
        )
        .unwrap();
        let output = run_with_timeout(child, 10, "echo stderr").unwrap();

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("hello-stderr"),
            "Should capture stderr, got: {}",
            stderr
        );
    }

    #[test]
    fn test_run_with_timeout_captures_both_stdout_and_stderr() {
        let child = spawn_piped(
            "sh",
            &[
                "-c".to_string(),
                "echo out-data; echo err-data >&2".to_string(),
            ],
        )
        .unwrap();
        let output = run_with_timeout(child, 10, "both streams").unwrap();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("out-data"), "stdout: {}", stdout);
        assert!(stderr.contains("err-data"), "stderr: {}", stderr);
    }

    #[test]
    fn test_run_with_timeout_nonzero_exit_code() {
        let child = spawn_piped("sh", &["-c".to_string(), "exit 42".to_string()]).unwrap();
        let output = run_with_timeout(child, 10, "exit 42").unwrap();

        assert!(!output.status.success());
        assert_eq!(output.status.code(), Some(42));
    }

    #[test]
    fn test_run_with_timeout_kills_on_timeout() {
        let child = spawn_piped("sleep", &["30".to_string()]).unwrap();
        let pid = child.id();

        let start = Instant::now();
        let result = run_with_timeout(child, 1, "sleep 30");
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "Timeout should return Ok");
        let output = result.unwrap();
        assert!(
            !output.status.success(),
            "Timeout should be treated as failure"
        );
        assert_eq!(
            output.status.code(),
            Some(TIMEOUT_EXIT_CODE),
            "Timeout should use exit code 124"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("timed out"),
            "Stderr should contain timeout notice: {}",
            stderr
        );
        // タイムアウト時はstdoutが空（リーダースレッドはjoinされない）
        assert!(
            output.stdout.is_empty(),
            "Stdout should be empty on timeout"
        );
        assert!(
            elapsed.as_secs() < 5,
            "Should timeout quickly, took {:?}",
            elapsed
        );
        assert!(is_timeout_output(&output));

        // /proc（Linux）またはkill -0でプロセスが実際に終了したことを確認
        #[cfg(unix)]
        {
            let check = Command::new("kill").args(["-0", &pid.to_string()]).output();
            if let Ok(output) = check {
                assert!(
                    !output.status.success(),
                    "Process {} should be dead after timeout kill",
                    pid
                );
            }
        }
    }

    #[test]
    fn test_run_with_timeout_large_output_no_deadlock() {
        // デッドロックが発生しないことを確認するために64KB超の出力を生成（典型的なパイプバッファサイズ）
        let child = spawn_piped(
            "sh",
            &[
                "-c".to_string(),
                "dd if=/dev/zero bs=1024 count=128 2>/dev/null | tr '\\0' 'A'".to_string(),
            ],
        )
        .unwrap();

        let start = Instant::now();
        let output = run_with_timeout(child, 10, "large output");
        let elapsed = start.elapsed();

        assert!(output.is_ok(), "Should not deadlock on large output");
        assert!(
            elapsed.as_secs() < 10,
            "Should complete quickly, took {:?}",
            elapsed
        );
        let out = output.unwrap();
        assert!(
            out.stdout.len() >= 128 * 1024,
            "Should capture all output: {} bytes",
            out.stdout.len()
        );
    }

    #[test]
    fn test_run_with_timeout_fast_command_under_timeout() {
        let child = spawn_piped("true", &[]).unwrap();
        let start = Instant::now();
        let result = run_with_timeout(child, 60, "true");
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        assert!(result.unwrap().status.success());
        assert!(
            elapsed.as_secs() < 2,
            "Fast command should return quickly: {:?}",
            elapsed
        );
    }

    // === spawn_piped_with_env テスト ===

    #[test]
    fn test_spawn_piped_with_env_passes_env_vars() {
        let child = spawn_piped_with_env(
            "sh",
            &["-c".to_string(), "echo $TEST_VAR_123".to_string()],
            &[("TEST_VAR_123", "hello_from_env")],
        )
        .unwrap();
        let output = run_with_timeout(child, 10, "env test").unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("hello_from_env"),
            "環境変数が子プロセスに渡されるべき: {}",
            stdout
        );
    }

    #[test]
    fn test_spawn_piped_with_env_multiple_vars() {
        let child = spawn_piped_with_env(
            "sh",
            &["-c".to_string(), "echo ${VAR_A}_${VAR_B}".to_string()],
            &[("VAR_A", "alpha"), ("VAR_B", "beta")],
        )
        .unwrap();
        let output = run_with_timeout(child, 10, "multi env test").unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("alpha_beta"),
            "複数の環境変数が渡されるべき: {}",
            stdout
        );
    }

    #[test]
    fn test_spawn_piped_with_env_empty_envs() {
        let child = spawn_piped_with_env("echo", &["no-env".to_string()], &[]).unwrap();
        let output = run_with_timeout(child, 10, "empty env").unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("no-env"));
    }

    // === run_with_timeout_tracked テスト ===

    #[test]
    fn test_run_with_timeout_tracked_success_not_timed_out() {
        let child = spawn_piped("echo", &["tracked".to_string()]).unwrap();
        let result = run_with_timeout_tracked(child, 10, "echo tracked").unwrap();
        assert!(!result.timed_out, "正常終了時は timed_out=false");
        assert!(result.output.status.success());
    }

    #[test]
    fn test_run_with_timeout_tracked_timeout_sets_flag() {
        let child = spawn_piped("sleep", &["30".to_string()]).unwrap();
        let result = run_with_timeout_tracked(child, 1, "sleep 30").unwrap();
        assert!(result.timed_out, "タイムアウト時は timed_out=true");
        assert_eq!(result.output.status.code(), Some(TIMEOUT_EXIT_CODE));
    }

    #[test]
    fn test_run_with_timeout_nonzero_exit_preserves_stderr() {
        let child = spawn_piped(
            "sh",
            &[
                "-c".to_string(),
                "echo error-detail >&2; exit 1".to_string(),
            ],
        )
        .unwrap();
        let output = run_with_timeout(child, 10, "exit 1 with stderr").unwrap();
        assert!(!output.status.success());
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("error-detail"),
            "非ゼロ終了時もstderrをキャプチャすべき: {}",
            stderr
        );
    }

    #[test]
    fn test_is_timeout_output_false_for_normal_exit_124() {
        let child = spawn_piped(
            "sh",
            &[
                "-c".to_string(),
                "echo normal-error >&2; exit 124".to_string(),
            ],
        )
        .unwrap();
        let output = run_with_timeout(child, 10, "exit 124").unwrap();
        assert!(!is_timeout_output(&output));
        assert_eq!(output.status.code(), Some(TIMEOUT_EXIT_CODE));
    }
}
