//! タイムアウト対応のコマンド実行ユーティリティ。

use std::io::Read as _;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt as _;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt as _;
use tracing::warn;

/// タイムアウトによりコマンドが終了された場合の終了コード。
pub(crate) const TIMEOUT_EXIT_CODE: i32 = 124;

/// タイムアウト時にstderrに付加されるプレフィックス。
const TIMEOUT_STDERR_PREFIX: &str = "[Command timed out after";

/// Unix で子プロセスを新しいプロセスグループに配置する。
/// `Command::process_group(0)` 相当の挙動を `pre_exec` 経由で安定 API のみで実現する。
/// （`process_group` は Rust 1.64 以降で安定化済みだが、明示的な意図を残すため pre_exec を使う）
///
/// 効果: プロセスグループID == 子プロセスPID となるため、子の孫プロセス
/// （例: `sh -c 'sleep 600'` の `sleep`）も同じプロセスグループに属し、
/// `killpg(pid, SIGKILL)` でグループ全体を停止できる。
#[cfg(unix)]
fn configure_unix_process_group(cmd: &mut Command) {
    // Safety: pre_exec で呼ぶ関数は async-signal-safe である必要がある。
    // setpgid(0, 0) は POSIX の async-signal-safe 関数として規定されている。
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

/// Unix で子プロセスのプロセスグループ全体を SIGKILL で停止する。
/// `child.kill()` は子プロセスのみを対象とするため、`sh -c 'sleep'` のような
/// シェル経由のケースで孫プロセスがゾンビ/孤児として残るのを防ぐ。
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let pgid = pid as i32;
    // killpg は EPERM/ESRCH 等の失敗もあり得るが、フェイルセーフに留める。
    // 戻り値は意図的に無視する（後続で SIGKILL 直送 + wait で確実に回収する）。
    unsafe {
        let _ = libc::killpg(pgid, libc::SIGKILL);
    }
}

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

/// リーダースレッドを deadline まで待って join する。
///
/// 子プロセスが正常終了していても、バックグラウンドの孫プロセスが stdout/stderr
/// のパイプを継承して保持し続けると `read_to_end` が EOF を受け取れず `join` が
/// 無期限にブロックし、設定したタイムアウトが無効化される。これを防ぐため、
/// deadline 超過時は join を諦め、取得済み（空の可能性あり）バッファを返す。
///
/// 子は既に回収済みで PID が再利用される恐れがあるため、ここで killpg はしない。
/// 残った孫プロセスはプロセス終了時に OS がクリーンアップする。
fn join_reader_before_deadline(
    handle: std::thread::JoinHandle<Vec<u8>>,
    deadline: Instant,
) -> Vec<u8> {
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return Vec::new();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    handle.join().unwrap_or_default()
}

/// タイムアウト付きでコマンドを実行し、タイムアウトメタデータを返す。
///
/// `Output` のみが必要な場合は `run_with_timeout` を使用する。
pub fn run_with_timeout_tracked(
    mut child: std::process::Child,
    timeout_secs: u64,
    command_desc: &str,
) -> Result<TimedOutput, String> {
    let deadline = match Instant::now().checked_add(Duration::from_secs(timeout_secs)) {
        Some(deadline) => deadline,
        None => {
            #[cfg(unix)]
            kill_process_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "Invalid timeout {}s for command '{}': deadline is out of range",
                timeout_secs, command_desc
            ));
        }
    };

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
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status, false),
            Ok(None) => {
                if Instant::now() >= deadline {
                    warn!(
                        "⏰ Command timed out after {}s: {}",
                        timeout_secs, command_desc
                    );
                    // Unix では子プロセスのプロセスグループ全体を SIGKILL する。
                    // `child.kill()` は直接の子のみを対象とするため、
                    // `sh -c 'sleep'` のような孫プロセスが孤児として残ってしまう。
                    // configure_unix_process_group で setpgid 済みのため
                    // 子の PID == プロセスグループID で killpg 可能。
                    #[cfg(unix)]
                    kill_process_group(child.id());
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

    // リーダースレッドから出力を収集する（正常終了後は通常すぐ完了する）。
    // ただし子が早期終了し、バックグラウンドの孫プロセスがパイプを保持し続けると
    // join が無期限ブロックしタイムアウトが無効化されるため、deadline までで諦める。
    let stdout = join_reader_before_deadline(stdout_thread, deadline);
    let stderr = join_reader_before_deadline(stderr_thread, deadline);

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

/// パイプ接続されたstdout/stderrと追加の環境変数でコマンドを起動する。
/// ストップフックがループ防止用の環境変数を子プロセスに伝播するために使用。
///
/// Unix では子プロセスを新しいプロセスグループに配置し、タイムアウト時に
/// プロセスグループ全体を確実に停止できるようにする。
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
    #[cfg(unix)]
    configure_unix_process_group(&mut cmd);
    cmd.spawn()
        .map_err(|e| format!("Failed to execute '{}': {}", program, e))
}

/// stdout/stderr/stdin を切り離してコマンドを起動する。
///
/// `report=false` の Stop フック用。親プロセスは子を待たないため、Hook 応答を
/// コマンド完了まで遅延させない。出力は破棄されるので、必要なログはコマンド側で
/// 明示的にファイル等へ書き出すこと。
pub fn spawn_detached_with_env(
    program: &str,
    args: &[String],
    envs: &[(&str, &str)],
) -> Result<u32, String> {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.arg("/c").arg(program).args(args);
        c
    } else {
        let mut c = Command::new(program);
        c.args(args);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for &(key, value) in envs {
        cmd.env(key, value);
    }
    #[cfg(unix)]
    configure_unix_process_group(&mut cmd);
    cmd.spawn()
        .map(|child| child.id())
        .map_err(|e| format!("Failed to execute '{}': {}", program, e))
}

/// 任意の `Command` ビルダーに対して、Unix では新しいプロセスグループに配置する設定を施す。
/// `Command::new(...)` を直接組み立てるパス（例: extension hook）から再利用できる。
///
/// Windows ではノーオペレーション。`#[cfg(unix)]` 制約のない呼び出し側で安全に使えるよう
/// 公開する。
pub fn configure_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    configure_unix_process_group(cmd);
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テストヘルパー：追加の環境変数なしでspawnする。
    fn spawn_piped(program: &str, args: &[String]) -> Result<std::process::Child, String> {
        spawn_piped_with_env(program, args, &[])
    }

    fn wait_for_path(path: &std::path::Path, timeout: std::time::Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if path.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        path.exists()
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

    #[test]
    fn test_run_with_timeout_rejects_unrepresentable_deadline() {
        let child = spawn_piped("sleep", &["30".to_string()]).unwrap();
        let result = run_with_timeout(child, u64::MAX, "sleep 30");
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("Invalid timeout"),
            "表現できない期限はエラーとして返すべき"
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

    #[test]
    fn test_spawn_detached_with_env_returns_without_waiting() {
        let marker = std::env::temp_dir().join(format!(
            "claw-hooks-detached-command-{}",
            std::process::id()
        ));
        let marker_path = marker.to_string_lossy().replace('\'', "'\\''");
        let _ = std::fs::remove_file(&marker);

        let start = Instant::now();
        let pid = spawn_detached_with_env(
            "sh",
            &[
                "-c".to_string(),
                format!("sleep 1; echo detached > '{}'", marker_path),
            ],
            &[],
        )
        .unwrap();

        assert!(pid > 0);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "デタッチ起動は子プロセス完了を待たないべき"
        );
        assert!(!marker.exists());
        assert!(wait_for_path(&marker, Duration::from_secs(3)));
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn test_spawn_detached_with_env_passes_env_vars() {
        let marker =
            std::env::temp_dir().join(format!("claw-hooks-detached-env-{}", std::process::id()));
        let marker_path = marker.to_string_lossy().replace('\'', "'\\''");
        let _ = std::fs::remove_file(&marker);

        spawn_detached_with_env(
            "sh",
            &[
                "-c".to_string(),
                format!("printf %s \"$DETACHED_VAR\" > '{}'", marker_path),
            ],
            &[("DETACHED_VAR", "detached-env-ok")],
        )
        .unwrap();

        assert!(wait_for_path(&marker, Duration::from_secs(3)));
        let content = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(content, "detached-env-ok");
        let _ = std::fs::remove_file(marker);
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

    /// Unix で `sh -c 'sleep'` のような孫プロセスがタイムアウト時に
    /// 確実に停止することを確認する。
    /// プロセスグループ kill が機能していなければ、`sh` だけ kill されて
    /// `sleep` が孤児プロセスとして残ってしまう。
    #[cfg(unix)]
    #[test]
    fn test_run_with_timeout_kills_grandchild_process() {
        // 一時マーカーファイルを設定: sleep が完走したらマーカーを書き込む。
        // タイムアウトで sleep が殺されればマーカーは書き込まれない。
        let marker = std::env::temp_dir().join(format!(
            "claw-hooks-grandchild-marker-{}",
            std::process::id()
        ));
        let marker_path = marker.to_str().unwrap();
        // 既存マーカーを掃除
        let _ = std::fs::remove_file(&marker);

        // 30秒スリープ後にマーカー作成。1秒タイムアウトで kill。
        let cmd = format!("sleep 30 && touch {}", marker_path);
        let child = spawn_piped("sh", &["-c".to_string(), cmd.clone()]).unwrap();
        let result = run_with_timeout(child, 1, &cmd);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().status.code(), Some(TIMEOUT_EXIT_CODE));

        // killpg が孫プロセスを停止できたなら、しばらく待ってもマーカーは作成されない。
        // 念のため余裕を持って待機する。
        std::thread::sleep(Duration::from_secs(2));
        assert!(
            !marker.exists(),
            "孫プロセスのスリープがタイムアウトで停止しなかった: {} が残存",
            marker_path
        );
    }
}
