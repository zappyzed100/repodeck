use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

const RETENTION_DAYS: u64 = 7;
const MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024;

/// Where `init` writes daily-rotating log files, so callers (e.g. the tray
/// menu's "ログフォルダーを開く") don't repeat the `"logs"` literal.
pub fn log_dir(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("logs")
}

/// Initializes daily-rotating file logging under `<data_dir>/logs`.
///
/// The returned guard must be kept alive for the lifetime of the process;
/// dropping it flushes and stops the background writer thread.
pub fn init(data_dir: &Path) -> anyhow::Result<WorkerGuard> {
    let log_dir = log_dir(data_dir);
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create log directory {}", log_dir.display()))?;

    let file_appender = tracing_appender::rolling::daily(&log_dir, "repodeck");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_env("REPODECK_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();

    Ok(guard)
}

/// Deletes `repodeck.*` log files older than [`RETENTION_DAYS`], then, if the
/// directory's total size is still over [`MAX_TOTAL_BYTES`], deletes the
/// oldest remaining files until it isn't (development-plan.md §16: daily
/// rotation is already handled by `tracing_appender`; this owns the
/// retention/size cap that rotation alone doesn't provide). Best-effort:
/// individual file errors (e.g. one still open elsewhere) are skipped rather
/// than aborting the whole sweep.
pub fn enforce_retention(log_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return;
    };

    let now = SystemTime::now();
    let max_age = Duration::from_secs(RETENTION_DAYS * 24 * 60 * 60);

    let mut remaining: Vec<(std::path::PathBuf, SystemTime, u64)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let is_log_file = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("repodeck."));
        if !is_log_file {
            continue;
        }

        let modified = metadata.modified().unwrap_or(now);
        let age = now.duration_since(modified).unwrap_or_default();
        if age > max_age {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        remaining.push((path, modified, metadata.len()));
    }

    let mut total_bytes: u64 = remaining.iter().map(|(_, _, len)| *len).sum();
    if total_bytes <= MAX_TOTAL_BYTES {
        return;
    }

    // Oldest-modified first, so the newest (most likely still-useful) logs
    // are the last ones considered for deletion.
    remaining.sort_by_key(|(_, modified, _)| *modified);
    for (path, _, len) in remaining {
        if total_bytes <= MAX_TOTAL_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total_bytes = total_bytes.saturating_sub(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn write_file(dir: &Path, name: &str, size: usize) {
        std::fs::write(dir.join(name), vec![b'x'; size]).unwrap();
    }

    fn set_mtime(path: &Path, age: Duration) {
        let modified = SystemTime::now() - age;
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(modified).unwrap();
    }

    #[test]
    fn deletes_files_older_than_the_retention_window() {
        let dir = tempdir().unwrap();
        write_file(dir.path(), "repodeck.2020-01-01", 10);
        set_mtime(
            &dir.path().join("repodeck.2020-01-01"),
            Duration::from_secs(30 * 24 * 60 * 60),
        );
        write_file(dir.path(), "repodeck.2026-07-21", 10);

        enforce_retention(dir.path());

        assert!(!dir.path().join("repodeck.2020-01-01").exists());
        assert!(dir.path().join("repodeck.2026-07-21").exists());
    }

    #[test]
    fn leaves_non_log_files_untouched() {
        let dir = tempdir().unwrap();
        write_file(dir.path(), "unrelated.txt", 10);
        set_mtime(
            &dir.path().join("unrelated.txt"),
            Duration::from_secs(30 * 24 * 60 * 60),
        );

        enforce_retention(dir.path());

        assert!(dir.path().join("unrelated.txt").exists());
    }

    #[test]
    fn deletes_oldest_first_when_over_the_size_cap() {
        let dir = tempdir().unwrap();
        let big = (MAX_TOTAL_BYTES / 2 + 1) as usize;
        write_file(dir.path(), "repodeck.a", big);
        set_mtime(
            &dir.path().join("repodeck.a"),
            Duration::from_secs(3 * 24 * 60 * 60),
        );
        write_file(dir.path(), "repodeck.b", big);
        set_mtime(
            &dir.path().join("repodeck.b"),
            Duration::from_secs(2 * 24 * 60 * 60),
        );
        write_file(dir.path(), "repodeck.c", big);
        set_mtime(
            &dir.path().join("repodeck.c"),
            Duration::from_secs(24 * 60 * 60),
        );

        enforce_retention(dir.path());

        // Oldest (`a`) should go first; the newest (`c`) must survive.
        assert!(!dir.path().join("repodeck.a").exists());
        assert!(dir.path().join("repodeck.c").exists());
    }

    #[test]
    fn missing_log_directory_is_a_no_op() {
        let dir = tempdir().unwrap();
        enforce_retention(&dir.path().join("does-not-exist"));
    }
}
