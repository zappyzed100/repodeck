use std::path::Path;

use anyhow::Context;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

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
