//! The desktop's `IFolderView2`, via Raymond Chen's recipe ("Manipulating the positions
//! of desktop icons"): IShellWindows → FindWindowSW(SWC_DESKTOP) → SID_STopLevelBrowser
//! → QueryActiveShellView. No WorkerW / SysListView32 hacks.

use anyhow::{Context, Result};
use serde::Serialize;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, IServiceProvider};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    CSIDL_DESKTOP, FOLDERFLAGS, FOLDERVIEWMODE, FVM_ICON, FWF_AUTOARRANGE, FWF_NOICONS,
    FWF_SNAPTOGRID, IFolderView2, IShellBrowser, IShellWindows, SID_STopLevelBrowser,
    SVGIO_ALLVIEW, SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
};
use windows::core::Interface;

pub struct DesktopView {
    pub view: IFolderView2,
    pub hwnd: HWND,
}

impl DesktopView {
    /// Must be called on an STA thread with COM initialised.
    pub fn open() -> Result<Self> {
        // SAFETY: standard COM calls on interfaces we own.
        unsafe {
            let windows: IShellWindows =
                CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).context("ShellWindows")?;
            let loc = VARIANT::from(CSIDL_DESKTOP as i32);
            let empty = VARIANT::default();
            let mut hwnd = 0i32;
            let disp = windows
                .FindWindowSW(&loc, &empty, SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH)
                .context("FindWindowSW(SWC_DESKTOP)")?;
            let sp: IServiceProvider = disp.cast().context("IServiceProvider")?;
            let browser: IShellBrowser = sp
                .QueryService(&SID_STopLevelBrowser)
                .context("SID_STopLevelBrowser")?;
            let shell_view = browser
                .QueryActiveShellView()
                .context("QueryActiveShellView")?;
            let view_hwnd = shell_view.GetWindow().context("IShellView::GetWindow")?;
            let view: IFolderView2 = shell_view.cast().context("IFolderView2")?;
            Ok(Self {
                view,
                hwnd: view_hwnd,
            })
        }
    }

    pub fn info(&self) -> Result<ViewInfo> {
        // SAFETY: out-params are valid locals.
        unsafe {
            let mut spacing = POINT::default();
            self.view.GetSpacing(&mut spacing).context("GetSpacing")?;
            let mut mode = FOLDERVIEWMODE::default();
            let mut icon_size = 0i32;
            self.view
                .GetViewModeAndIconSize(&mut mode, &mut icon_size)
                .context("GetViewModeAndIconSize")?;
            let flags = self
                .view
                .GetCurrentFolderFlags()
                .context("GetCurrentFolderFlags")?;
            let item_count = self.view.ItemCount(SVGIO_ALLVIEW).context("ItemCount")?;
            Ok(ViewInfo {
                spacing_x: spacing.x,
                spacing_y: spacing.y,
                icon_size,
                icon_mode: mode == FVM_ICON,
                flags: ViewFlags::from_bits(flags),
                item_count,
            })
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ViewFlags {
    pub auto_arrange: bool,
    pub snap_to_grid: bool,
    pub icons_shown: bool,
    pub raw: u32,
}

impl ViewFlags {
    pub fn from_bits(raw: u32) -> Self {
        let has = |f: FOLDERFLAGS| raw & f.0 as u32 != 0;
        Self {
            auto_arrange: has(FWF_AUTOARRANGE),
            snap_to_grid: has(FWF_SNAPTOGRID),
            icons_shown: !has(FWF_NOICONS),
            raw,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ViewInfo {
    /// Grid step in view client pixels.
    pub spacing_x: i32,
    pub spacing_y: i32,
    pub icon_size: i32,
    pub icon_mode: bool,
    pub flags: ViewFlags,
    /// Items currently shown in the view (respects hidden/system file settings).
    pub item_count: i32,
}

/// Grid of the work area for a given step. Pure, for reports and the layout engine.
pub fn grid_dims(work_w: i32, work_h: i32, step_x: i32, step_y: i32) -> (i32, i32) {
    if step_x <= 0 || step_y <= 0 {
        return (0, 0);
    }
    (work_w / step_x, work_h / step_y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_decode() {
        let f = ViewFlags::from_bits((FWF_AUTOARRANGE.0 | FWF_SNAPTOGRID.0) as u32);
        assert!(f.auto_arrange && f.snap_to_grid && f.icons_shown);
        assert!(!ViewFlags::from_bits(FWF_NOICONS.0 as u32).icons_shown);
    }

    #[test]
    fn grid() {
        assert_eq!(grid_dims(2560, 1392, 76, 100), (33, 13));
        assert_eq!(grid_dims(100, 100, 0, 10), (0, 0));
    }
}
