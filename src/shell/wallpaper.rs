//! Wallpaper as it is on screen, via `IDesktopWallpaper`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::{
    DESKTOP_WALLPAPER_POSITION, DWPOS_CENTER, DWPOS_FILL, DWPOS_FIT, DWPOS_SPAN, DWPOS_STRETCH,
    DWPOS_TILE, DesktopWallpaper, IDesktopWallpaper,
};
use windows::core::{PCWSTR, PWSTR};

use crate::platform::Rect;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WallpaperMode {
    Center,
    Tile,
    Stretch,
    Fit,
    Fill,
    Span,
    Unknown,
}

impl From<DESKTOP_WALLPAPER_POSITION> for WallpaperMode {
    fn from(p: DESKTOP_WALLPAPER_POSITION) -> Self {
        match p {
            DWPOS_CENTER => Self::Center,
            DWPOS_TILE => Self::Tile,
            DWPOS_STRETCH => Self::Stretch,
            DWPOS_FIT => Self::Fit,
            DWPOS_FILL => Self::Fill,
            DWPOS_SPAN => Self::Span,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WallpaperInfo {
    /// Path reported by Windows; may be empty or missing (slideshow, Spotlight).
    pub path: Option<PathBuf>,
    pub path_exists: bool,
    /// `%APPDATA%\Microsoft\Windows\Themes\TranscodedWallpaper` fallback.
    pub transcoded: Option<PathBuf>,
    pub mode: WallpaperMode,
    /// 0xRRGGBB
    pub background_color: u32,
    pub monitor: Option<Rect>,
}

impl WallpaperInfo {
    /// The image file to actually read: reported path, else the transcoded copy.
    pub fn effective_path(&self) -> Option<&PathBuf> {
        self.path
            .as_ref()
            .filter(|_| self.path_exists)
            .or(self.transcoded.as_ref())
    }
}

fn take_pwstr(p: PWSTR) -> String {
    // SAFETY: p was allocated by the callee with CoTaskMemAlloc.
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// Wallpaper for the monitor whose rect matches `primary`. STA or MTA.
pub fn query(primary: Rect) -> Result<WallpaperInfo> {
    // SAFETY: COM calls on an interface we own; returned strings are freed.
    unsafe {
        let dw: IDesktopWallpaper =
            CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL).context("DesktopWallpaper")?;
        let mut path = None;
        let mut monitor = None;
        let count = dw
            .GetMonitorDevicePathCount()
            .context("GetMonitorDevicePathCount")?;
        for i in 0..count {
            let Ok(id) = dw.GetMonitorDevicePathAt(i) else {
                continue;
            };
            let id_str = take_pwstr(id);
            let id_w: Vec<u16> = id_str.encode_utf16().chain(Some(0)).collect();
            let Ok(r) = dw.GetMonitorRECT(PCWSTR(id_w.as_ptr())) else {
                continue;
            };
            let r = Rect::from(r);
            if r == primary {
                monitor = Some(r);
                if let Ok(p) = dw.GetWallpaper(PCWSTR(id_w.as_ptr())) {
                    path = Some(take_pwstr(p));
                }
                break;
            }
        }
        if path.is_none() {
            // Same wallpaper on all monitors: NULL monitor id returns it.
            path = dw.GetWallpaper(PCWSTR::null()).ok().map(take_pwstr);
        }
        let path = path.filter(|p| !p.is_empty()).map(PathBuf::from);
        let path_exists = path.as_ref().is_some_and(|p| p.is_file());
        let mode = dw
            .GetPosition()
            .map(WallpaperMode::from)
            .unwrap_or(WallpaperMode::Unknown);
        let bgr = dw.GetBackgroundColor().map(|c| c.0).unwrap_or(0);
        let background_color = ((bgr & 0xff) << 16) | (bgr & 0xff00) | ((bgr >> 16) & 0xff);
        let transcoded = std::env::var_os("APPDATA")
            .map(|a| PathBuf::from(a).join(r"Microsoft\Windows\Themes\TranscodedWallpaper"))
            .filter(|p| p.is_file());
        Ok(WallpaperInfo {
            path,
            path_exists,
            transcoded,
            mode,
            background_color,
            monitor,
        })
    }
}
