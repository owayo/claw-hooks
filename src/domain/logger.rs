//! Logging system with daily rotation.

use anyhow::Result;
use std::fs;
use std::path::Path;
use time::macros::format_description;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
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

    // Create rolling file appender with daily rotation
    let file_appender = RollingFileAppender::new(Rotation::DAILY, &config.log_path, "claw-hooks");

    // Use local timezone for timestamps
    let time_format = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let timer = OffsetTime::new(local_offset, time_format);

    // Set up subscriber with file output
    let subscriber = tracing_subscriber::registry()
        .with(EnvFilter::from_default_env().add_directive(tracing::Level::DEBUG.into()))
        .with(
            fmt::layer()
                .with_writer(file_appender)
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(false)
                .with_file(true)
                .with_line_number(true)
                .with_timer(timer),
        );

    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| anyhow::anyhow!("Failed to set global subscriber: {}", e))?;

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
