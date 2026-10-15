//! The desktop's `IFolderView2`, via Raymond Chen's recipe ("Manipulating the positions
//! of desktop icons"): IShellWindows → FindWindowSW(SWC_DESKTOP) → SID_STopLevelBrowser
//! → QueryActiveShellView. No WorkerW / SysListView32 hacks.

use anyhow::{Context, Result};
use serde::Serialize;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, IServiceProvider};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    CSIDL_DESKTOP, FOLDERFLAGS, FOLDERVIEWMODE, FVM_ICON, FWF_AUTOARRANGE, FWF_NOICONS,
    FWF_SNAPTOGRID, IFolderView2, ILFree, IShellBrowser, IShellItem, IShellWindows,
    SID_STopLevelBrowser, SIGDN, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH,
    SIGDN_NORMALDISPLAY, SVGIO_ALLVIEW, SVSI_POSITIONITEM, SWC_DESKTOP, SWFO_NEEDDISPATCH,
    ShellWindows,
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

/// One item as the desktop view sees it. Read-only snapshot.
#[derive(Debug, Clone)]
pub struct ViewItem {
    /// Filesystem path, or `::{CLSID}` for virtual items.
    pub parsing: String,
    pub is_filesystem: bool,
    pub display: String,
    /// Position in view client coordinates.
    pub x: i32,
    pub y: i32,
}

fn display_name(item: &IShellItem, sigdn: SIGDN) -> Option<String> {
    // SAFETY: returned string freed with CoTaskMemFree.
    unsafe {
        let p = item.GetDisplayName(sigdn).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

impl DesktopView {
    /// Names and positions of all items in the view. Never moves anything.
    pub fn items(&self) -> Result<Vec<ViewItem>> {
        let mut out = Vec::new();
        // SAFETY: PIDLs returned by Item() are freed with ILFree.
        unsafe {
            let n = self.view.ItemCount(SVGIO_ALLVIEW).context("ItemCount")?;
            for i in 0..n {
                let Ok(item) = self.view.GetItem::<IShellItem>(i) else {
                    continue;
                };
                let (parsing, is_filesystem) = match display_name(&item, SIGDN_FILESYSPATH) {
                    Some(p) => (p, true),
                    None => match display_name(&item, SIGDN_DESKTOPABSOLUTEPARSING) {
                        Some(p) => (p, false),
                        None => continue,
                    },
                };
                let display = display_name(&item, SIGDN_NORMALDISPLAY).unwrap_or_default();
                let Ok(pidl) = self.view.Item(i) else {
                    continue;
                };
                let pos = self.view.GetItemPosition(pidl);
                ILFree(Some(pidl));
                let Ok(pt) = pos else { continue };
                out.push(ViewItem {
                    parsing,
                    is_filesystem,
                    display,
                    x: pt.x,
                    y: pt.y,
                });
            }
        }
        Ok(out)
    }
}

impl DesktopView {
    /// Moves items (keyed by lowercase parsing name) to view points in ONE batch call,
    /// so the desktop doesn't flicker. Unknown keys are ignored. Returns how many moved.
    /// Touches icon positions only — never files.
    pub fn set_positions(
        &self,
        targets: &std::collections::HashMap<String, (i32, i32)>,
    ) -> Result<usize> {
        let mut pidls = Vec::new();
        let mut points = Vec::new();
        // SAFETY: PIDLs from Item() are freed with ILFree after the batch call.
        unsafe {
            let n = self.view.ItemCount(SVGIO_ALLVIEW).context("ItemCount")?;
            for i in 0..n {
                let Ok(item) = self.view.GetItem::<IShellItem>(i) else {
                    continue;
                };
                let key = display_name(&item, SIGDN_FILESYSPATH)
                    .or_else(|| display_name(&item, SIGDN_DESKTOPABSOLUTEPARSING))
                    .map(|s| s.to_lowercase());
                let Some(&(x, y)) = key.as_ref().and_then(|k| targets.get(k)) else {
                    continue;
                };
                let Ok(pidl) = self.view.Item(i) else {
                    continue;
                };
                pidls.push(pidl as *const _);
                points.push(POINT { x, y });
            }
            let r = if pidls.is_empty() {
                Ok(())
            } else {
                self.view.SelectAndPositionItems(
                    pidls.len() as u32,
                    pidls.as_ptr(),
                    Some(points.as_ptr()),
                    SVSI_POSITIONITEM.0 as u32,
                )
            };
            for p in &pidls {
                ILFree(Some(*p));
            }
            r.context("SelectAndPositionItems")?;
        }
        Ok(points.len())
    }
}

impl DesktopView {
    /// Turns the view's auto-arrange on or off.
    pub fn set_auto_arrange(&self, on: bool) -> Result<()> {
        let mask = FWF_AUTOARRANGE.0 as u32;
        // SAFETY: plain COM call on our view.
        unsafe {
            self.view
                .SetCurrentFolderFlags(mask, if on { mask } else { 0 })
        }
        .context("SetCurrentFolderFlags")
    }
}

/// Grid origin offset: the most common `x mod step` / `y mod step` among current items.
pub fn grid_origin(items: &[ViewItem], step_x: i32, step_y: i32) -> (i32, i32) {
    fn mode(vals: impl Iterator<Item = i32>) -> i32 {
        let mut counts = std::collections::BTreeMap::new();
        for v in vals {
            *counts.entry(v).or_insert(0usize) += 1;
        }
        counts
            .into_iter()
            .max_by_key(|&(v, n)| (n, std::cmp::Reverse(v)))
            .map_or(0, |(v, _)| v)
    }
    if step_x <= 0 || step_y <= 0 {
        return (0, 0);
    }
    (
        mode(items.iter().map(|i| i.x.rem_euclid(step_x))),
        mode(items.iter().map(|i| i.y.rem_euclid(step_y))),
    )
}

/// Grid cell → view point (top-left of the slot), inverse of [`cell_of`].
pub fn point_of(col: i32, row: i32, step_x: i32, step_y: i32, origin: (i32, i32)) -> (i32, i32) {
    (col * step_x + origin.0, row * step_y + origin.1)
}

/// View position → grid cell (nearest slot).
pub fn cell_of(x: i32, y: i32, step_x: i32, step_y: i32) -> (i32, i32) {
    if step_x <= 0 || step_y <= 0 {
        return (0, 0);
    }
    (
        (x + step_x / 2).div_euclid(step_x),
        (y + step_y / 2).div_euclid(step_y),
    )
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
