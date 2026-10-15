//! `nook.exe` — the daemon. M0: single instance, DPI, config, logging, STA message loop.
#![windows_subsystem = "windows"]

use anyhow::{Context, Result};
use nook::config::Config;
use nook::paths::AppDirs;
use nook::platform::{ComApartment, SingleInstance, enable_per_monitor_dpi};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, TranslateMessage,
};

fn main() -> Result<()> {
    enable_per_monitor_dpi().ok();
    let dirs = AppDirs::resolve()?;
    let _log = nook::logging::init(&dirs.logs_dir(), "nook")?;

    let Some(_instance) = SingleInstance::acquire(&SingleInstance::daemon_mutex_name()?)? else {
        tracing::info!("another instance is running; exiting");
        return Ok(());
    };
    let config = Config::load_or_create(&dirs.config_file()).context("config")?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), ?config.layout, "nook started");

    let _com = ComApartment::sta()?;
    // Tray, hotkey, IPC and watchers attach to this loop in later milestones.
    // SAFETY: standard Win32 message pump on the thread that owns the apartment.
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    tracing::info!("nook exiting");
    Ok(())
}
