//! Timeout-aware command execution utility.

use std::io::Read as _;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

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
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
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
                    break Err(format!(
                        "Command timed out after {}s: {}",
                        timeout_secs, command_desc
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                break Err(format!(
                    "Failed to wait for command '{}': {}",
                    command_desc, e
                ));
            }
        }
    };

    let status = status?;

    // Collect output from reader threads (these will finish quickly after process exits or is killed)
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Spawn a command with piped stdout/stderr, suitable for use with `run_with_timeout`.
pub fn spawn_piped(program: &str, args: &[String]) -> Result<std::process::Child, String> {
    Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to execute '{}': {}", program, e))
}
