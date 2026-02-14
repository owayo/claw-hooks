//! Timeout-aware command execution utility.

use std::io::Read as _;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt as _;
use tracing::warn;

/// Execute a command with a timeout.
///
/// Spawns the command with piped stdout/stderr, reads output in separate threads
/// (to prevent pipe buffer deadlock), and polls `try_wait` until the child exits
/// or the deadline is reached. On timeout, the child process is killed (SIGKILL)
/// and reaped.
pub fn run_with_timeout(
    mut child: std::process::Child,
    timeout_secs: u64,
    command_desc: &str,
) -> Result<Output, String> {
    // Take ownership of stdout/stderr handles for reading in threads
    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    // Read stdout in a thread (prevents pipe buffer deadlock)
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stdout) = stdout_handle {
            stdout.read_to_end(&mut buf).ok();
        }
        buf
    });

    // Read stderr in a thread
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stderr) = stderr_handle {
            stderr.read_to_end(&mut buf).ok();
        }
        buf
    });

    // Wait for child with timeout using try_wait polling
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
                    // Kill the child process (sends SIGKILL on Unix)
                    let _ = child.kill();
                    // Reap the zombie process
                    let _ = child.wait();
                    warn!("💀 Process killed (SIGKILL): {}", command_desc);
                    // Treat timeout as successful completion with collected output
                    #[cfg(unix)]
                    let status = ExitStatus::from_raw(0);
                    #[cfg(not(unix))]
                    let status = {
                        // Fallback: spawn a trivially-successful process to obtain ExitStatus(0)
                        Command::new("cmd")
                            .args(["/C", "exit 0"])
                            .output()
                            .map(|o| o.status)
                            .unwrap_or_else(|_| {
                                // Last resort: use the killed process status
                                child.wait().unwrap_or_else(|_| ExitStatus::from_raw(1))
                            })
                    };
                    break (status, true);
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
        // Don't join reader threads: child subprocesses (e.g. sh → sleep) may still
        // hold pipe handles open, causing join() to block indefinitely.
        // The threads will be cleaned up when the process exits.
        let msg = format!(
            "[Command timed out after {}s: {}]\n",
            timeout_secs, command_desc
        );
        return Ok(Output {
            status,
            stdout: Vec::new(),
            stderr: msg.into_bytes(),
        });
    }

    // Collect output from reader threads (finish quickly after normal process exit)
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Spawn a command with piped stdout/stderr and additional environment variables.
/// Used by stop hooks to propagate loop prevention env vars to child processes.
pub fn spawn_piped_with_env(
    program: &str,
    args: &[String],
    envs: &[(&str, &str)],
) -> Result<std::process::Child, String> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    for &(key, value) in envs {
        cmd.env(key, value);
    }
    cmd.spawn()
        .map_err(|e| format!("Failed to execute '{}': {}", program, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: spawn without extra env vars.
    fn spawn_piped(program: &str, args: &[String]) -> Result<std::process::Child, String> {
        spawn_piped_with_env(program, args, &[])
    }

    // === spawn_piped tests ===

    #[test]
    fn test_spawn_piped_valid_command() {
        let child = spawn_piped("echo", &["hello".to_string()]);
        assert!(child.is_ok(), "Should spawn valid command");
        // Reap the child
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

    // === run_with_timeout tests ===

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

        // Timeout is treated as successful completion
        assert!(result.is_ok(), "Timeout should return Ok");
        let output = result.unwrap();
        assert!(
            output.status.success(),
            "Timeout should be treated as success"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("timed out"),
            "Stderr should contain timeout notice: {}",
            stderr
        );
        // stdout is empty on timeout (reader threads are not joined)
        assert!(
            output.stdout.is_empty(),
            "Stdout should be empty on timeout"
        );
        assert!(
            elapsed.as_secs() < 5,
            "Should timeout quickly, took {:?}",
            elapsed
        );

        // Verify process is actually dead by checking /proc (Linux) or kill -0 via shell
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
        // Generate >64KB of output (typical pipe buffer size) to verify no deadlock
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
}
