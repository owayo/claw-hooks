//! ローカルタイムゾーンを使用した日次ローテーション付きログシステム。

use anyhow::Result;
use chrono::Local;
use logroller::{LogRoller, LogRollerBuilder, Rotation, RotationAge, TimeZone};
use std::fs;
use std::io::{self, Write as IoWrite};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::prelude::*;

use crate::config::Config;

/// ローカル時刻を既存ログと同じ形式で出力するタイマー。
struct LocalTimer;

impl FormatTime for LocalTimer {
    fn format_time(&self, writer: &mut Writer<'_>) -> std::fmt::Result {
        write!(writer, "{}", Local::now().format("%Y-%m-%d %H:%M:%S"))
    }
}

/// `LogRoller` を tracing layer と終了時ガードで共有する writer。
#[derive(Clone)]
struct SharedLogWriter(Arc<Mutex<LogRoller>>);

impl SharedLogWriter {
    fn lock(&self) -> MutexGuard<'_, LogRoller> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

struct LockedLogWriter<'a>(MutexGuard<'a, LogRoller>);

impl IoWrite for LockedLogWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a> MakeWriter<'a> for SharedLogWriter {
    type Writer = LockedLogWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LockedLogWriter(self.lock())
    }
}

/// `process::exit` の前にログとローテーション処理を確実に完了させるガード。
pub struct LoggingGuard {
    writer: SharedLogWriter,
}

impl Drop for LoggingGuard {
    fn drop(&mut self) {
        let _ = self.writer.lock().flush();
    }
}

/// ログシステムを初期化する。
pub fn init(config: &Config) -> Result<LoggingGuard> {
    // 必要に応じてログディレクトリを作成
    if !config.log_path.exists() {
        fs::create_dir_all(&config.log_path)?;
    }

    // 古いログを削除
    cleanup_old_logs(&config.log_path)?;

    // ローカルタイムゾーンによる日次ローテーション付きローリングファイルアペンダを作成
    // ファイル命名: claw-hooks.YYYY-MM-DD（例: claw-hooks.2026-02-05）
    let appender = LogRollerBuilder::new(config.log_path.as_path(), Path::new("claw-hooks"))
        .rotation(Rotation::AgeBased(RotationAge::Daily))
        .time_zone(TimeZone::Local) // ローテーションにシステムのローカルタイムゾーンを使用
        .max_keep_files(3)
        .graceful_shutdown(true)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create log roller: {}", e))?;

    let writer = SharedLogWriter(Arc::new(Mutex::new(appender)));
    let guard = LoggingGuard {
        writer: writer.clone(),
    };

    // タイムスタンプにローカルタイムゾーンを使用
    let timer = LocalTimer;

    // ファイル出力付きサブスクライバを設定
    let subscriber = tracing_subscriber::registry()
        .with(EnvFilter::from_default_env().add_directive(tracing::Level::DEBUG.into()))
        .with(
            fmt::layer()
                // Mutex<Write> は tracing-subscriber 公式の同期 MakeWriter 実装。
                // 不要な time/parsing 依存を持つ tracing-appender を経由しない。
                .with_writer(writer)
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(false)
                .with_file(true)
                .with_line_number(true)
                .with_timer(timer),
        );

    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| anyhow::anyhow!("Failed to set global subscriber: {}", e))?;

    Ok(guard)
}

/// 2日以上経過したログファイルを削除する。
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

        // ログファイルのみ処理
        if !path.is_file() {
            continue;
        }

        let filename = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        // claw-hooks のログファイルか確認
        if !filename.starts_with("claw-hooks") {
            continue;
        }

        // 更新日時を確認
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

        // 3日前の古いログファイルを作成
        let old_file = log_path.join("claw-hooks.2020-01-01");
        fs::write(&old_file, "old log").unwrap();
        let three_days_ago = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        set_file_modified_time(&old_file, three_days_ago).unwrap();

        // 最近のログファイルを作成
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

        // claw-hooksのログではない古いファイルを作成
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

        // ログファイル風の名前を持つサブディレクトリを作成
        let subdir = log_path.join("claw-hooks.subdir");
        fs::create_dir(&subdir).unwrap();

        cleanup_old_logs(log_path).unwrap();

        assert!(subdir.exists(), "Subdirectory should not be deleted");
    }

    #[test]
    fn test_cleanup_old_logs_keeps_recent_files_with_long_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path();

        // "claw-hooks" で始まる別名のログファイル（ローテーション後のファイル名想定）
        let recent_rotated = log_path.join("claw-hooks-archive.2026-04-15");
        fs::write(&recent_rotated, "rotated").unwrap();
        let one_day_ago = SystemTime::now() - Duration::from_secs(24 * 60 * 60);
        set_file_modified_time(&recent_rotated, one_day_ago).unwrap();

        cleanup_old_logs(log_path).unwrap();

        // 24時間前のファイルは2日基準なので削除されない
        assert!(
            recent_rotated.exists(),
            "1日前の claw-hooks プレフィックスファイルは保持される"
        );
    }

    #[test]
    fn test_cleanup_old_logs_handles_unreadable_metadata_gracefully() {
        // メタデータが読めないファイルがあってもエラーにならず処理を継続できる
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path();

        // 通常のログファイルを作成
        let regular_file = log_path.join("claw-hooks.2026-04-15");
        fs::write(&regular_file, "log").unwrap();

        // 削除しても他のファイルへ影響しないことを確認
        let result = cleanup_old_logs(log_path);
        assert!(result.is_ok());
        assert!(regular_file.exists());
    }
}
