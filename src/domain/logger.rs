//! Logging system with daily rotation using local timezone.

use anyhow::Result;
use logroller::{LogRollerBuilder, Rotation, RotationAge, TimeZone};
use std::fs;
use std::path::Path;
use time::macros::format_description;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::time::OffsetTime;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

/// Initialize the logging system.
pub fn init(config: &Config) -> Result<()> {
    // Create log directory if needed
    if !config.log_path.exists() {
        fs::create_dir_all(&config.log_path)?;
    }

    // Clean up old logs
    cleanup_old_logs(&config.log_path)?;

    // Create rolling file appender with daily rotation using local timezone
    // File naming: claw-hooks.YYYY-MM-DD (e.g., claw-hooks.2026-02-05)
    let appender = LogRollerBuilder::new(config.log_path.as_path(), Path::new("claw-hooks"))
        .rotation(Rotation::AgeBased(RotationAge::Daily))
        .time_zone(TimeZone::Local) // Use system local timezone for rotation
        .max_keep_files(3)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create log roller: {}", e))?;

    let (non_blocking, _guard) = tracing_appender::non_blocking(appender);

    // Use local timezone for timestamps
    let time_format = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let timer = OffsetTime::new(local_offset, time_format);

    // Set up subscriber with file output
    let subscriber = tracing_subscriber::registry()
        .with(EnvFilter::from_default_env().add_directive(tracing::Level::DEBUG.into()))
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(false)
                .with_file(true)
                .with_line_number(true)
                .with_timer(timer),
        );

    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| anyhow::anyhow!("Failed to set global subscriber: {}", e))?;

    // Keep the guard alive for the duration of the program
    // Note: In a real application, this guard should be stored somewhere
    // to prevent it from being dropped, which would stop the logging thread.
    std::mem::forget(_guard);

    Ok(())
}

/// Clean up log files older than 2 days.
pub fn cleanup_old_logs(log_path: &Path) -> Result<()> {
    use std::time::{Duration, SystemTime};

    let two_days = Duration::from_secs(2 * 24 * 60 * 60);
    let cutoff = SystemTime::now() - two_days;

    if !log_path.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(log_path)? {
        let entry = entry?;
        let path = entry.path();

        // Only process log files
        if !path.is_file() {
            continue;
        }

        let filename = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        // Check if it's a claw-hooks log file
        if !filename.starts_with("claw-hooks") {
            continue;
        }

        // Check modification time
        if let Ok(metadata) = entry.metadata() {
            if let Ok(modified) = metadata.modified() {
                if modified < cutoff {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    fn set_file_modified_time(path: &Path, time: SystemTime) -> std::io::Result<()> {
        let since_epoch = time.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let secs = since_epoch.as_secs();
        let atime = libc::timespec {
            tv_sec: secs as libc::time_t,
            tv_nsec: 0,
        };
        let mtime = libc::timespec {
            tv_sec: secs as libc::time_t,
            tv_nsec: 0,
        };
        let times = [atime, mtime];
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        let ret = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
        if ret == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[test]
    fn test_cleanup_old_logs_removes_old_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path();

        // Create an old log file (3 days ago)
        let old_file = log_path.join("claw-hooks.2020-01-01");
        fs::write(&old_file, "old log").unwrap();
        let three_days_ago = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        set_file_modified_time(&old_file, three_days_ago).unwrap();

        // Create a recent log file
        let recent_file = log_path.join("claw-hooks.2026-02-12");
        fs::write(&recent_file, "recent log").unwrap();

        cleanup_old_logs(log_path).unwrap();

        assert!(!old_file.exists(), "Old log file should be deleted");
        assert!(recent_file.exists(), "Recent log file should be kept");
    }

    #[test]
    fn test_cleanup_old_logs_ignores_non_claw_hooks_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path();

        // Create an old file that is NOT a claw-hooks log
        let other_file = log_path.join("other-app.log");
        fs::write(&other_file, "other log").unwrap();
        let three_days_ago = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        set_file_modified_time(&other_file, three_days_ago).unwrap();

        cleanup_old_logs(log_path).unwrap();

        assert!(other_file.exists(), "Non-claw-hooks file should be kept");
    }

    #[test]
    fn test_cleanup_old_logs_nonexistent_dir() {
        let result = cleanup_old_logs(Path::new("/tmp/nonexistent_claw_hooks_test_dir"));
        assert!(result.is_ok(), "Should return Ok for nonexistent directory");
    }

    #[test]
    fn test_cleanup_old_logs_empty_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let result = cleanup_old_logs(dir.path());
        assert!(result.is_ok(), "Should return Ok for empty directory");
    }

    #[test]
    fn test_cleanup_old_logs_ignores_subdirectories() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path();

        // Create a subdirectory named like a log file
        let subdir = log_path.join("claw-hooks.subdir");
        fs::create_dir(&subdir).unwrap();

        cleanup_old_logs(log_path).unwrap();

        assert!(subdir.exists(), "Subdirectory should not be deleted");
    }
}
