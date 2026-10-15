//! File logging under `%LOCALAPPDATA%\Nook\logs`. Never writes to stdout
//! (stdout is reserved for MCP JSON-RPC in `nookctl mcp`).

use std::path::Path;

use anyhow::Result;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// Initialises logging to `<logs_dir>/<prefix>.YYYY-MM-DD.log`. Keep the guard alive.
/// Level comes from `NOOK_LOG` (default `info`).
pub fn init(logs_dir: &Path, prefix: &str) -> Result<WorkerGuard> {
    std::fs::create_dir_all(logs_dir)?;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(prefix)
        .filename_suffix("log")
        .max_log_files(14)
        .build(logs_dir)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("NOOK_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Ok(guard)
}
