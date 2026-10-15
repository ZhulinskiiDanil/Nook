//! `nook.exe` — the daemon: tray, IPC, desktop view access (later: hotkey, preview, inbox).
#![windows_subsystem = "windows"]

use anyhow::{Context, Result};
use nook::config::Config;
use nook::paths::AppDirs;
use nook::platform::{SingleInstance, enable_per_monitor_dpi};

fn main() -> Result<()> {
    enable_per_monitor_dpi().ok();
    let dirs = AppDirs::resolve()?;
    let _log = nook::logging::init(&dirs.logs_dir(), "nook")?;

    let Some(_instance) = SingleInstance::acquire(&SingleInstance::daemon_mutex_name()?)? else {
        tracing::info!("another instance is running; exiting");
        return Ok(());
    };
    let config = Config::load_or_create(&dirs.config_file()).context("config")?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "nook started");

    let r = nook::daemon::run(config);
    if let Err(e) = &r {
        tracing::error!("daemon failed: {e:#}");
    }
    tracing::info!("nook exiting");
    r
}
