//! `status`: version, Windows, monitor, grid, view flags, Explorer settings, plans, quarantine.

use std::path::Path;

use serde::Serialize;

use crate::paths::{Desktops, expand_env};
use crate::platform::{PrimaryMonitor, WindowsVersion};
use crate::shell::desktop_view::{ViewInfo, grid_dims};
use crate::shell::explorer_settings::ExplorerSettings;

#[derive(Debug, Serialize)]
pub struct Grid {
    pub cols: i32,
    pub rows: i32,
    pub step_x: i32,
    pub step_y: i32,
    pub fictitious: bool,
}

#[derive(Debug, Serialize)]
pub struct Quarantine {
    pub path: String,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub nook_version: &'static str,
    pub windows: Option<WindowsVersion>,
    pub sandbox: bool,
    pub user_desktop: String,
    pub public_desktop: String,
    pub monitor: Option<PrimaryMonitor>,
    pub grid: Option<Grid>,
    pub icon_size: Option<i32>,
    pub view: Option<ViewInfo>,
    pub explorer: ExplorerSettings,
    /// Plans arrive in M4.
    pub pending_plan: Option<String>,
    pub last_plan: Option<String>,
    pub quarantine: Option<Quarantine>,
    pub errors: Vec<String>,
}

fn dir_totals(dir: &Path) -> (u64, u64) {
    let mut stack = vec![dir.to_path_buf()];
    let (mut files, mut bytes) = (0, 0);
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => stack.push(e.path()),
                Ok(m) => {
                    files += 1;
                    bytes += m.len();
                }
                Err(_) => {}
            }
        }
    }
    (files, bytes)
}

/// `view` comes from the UI thread (live) or is `None` in sandbox mode.
pub fn collect(
    desktops: &Desktops,
    quarantine_template: &str,
    view: Option<anyhow::Result<ViewInfo>>,
) -> Status {
    let mut errors = Vec::new();
    let monitor = PrimaryMonitor::query()
        .map_err(|e| errors.push(format!("monitor: {e:#}")))
        .ok();
    let view = match view {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => {
            errors.push(format!("desktop view: {e:#}"));
            None
        }
        None => None,
    };
    let grid = match (&monitor, &view, desktops.is_sandbox()) {
        (Some(m), Some(v), false) => {
            let (cols, rows) = grid_dims(m.work.width(), m.work.height(), v.spacing_x, v.spacing_y);
            Some(Grid {
                cols,
                rows,
                step_x: v.spacing_x,
                step_y: v.spacing_y,
                fictitious: false,
            })
        }
        (_, _, true) => Some(Grid {
            cols: 34,
            rows: 16,
            step_x: 75,
            step_y: 85,
            fictitious: true,
        }),
        _ => None,
    };
    let quarantine = match expand_env(quarantine_template, &desktops.user) {
        Ok(p) => {
            let (files, bytes) = dir_totals(&p);
            Some(Quarantine {
                path: p.display().to_string(),
                files,
                bytes,
            })
        }
        Err(e) => {
            errors.push(format!("quarantine: {e:#}"));
            None
        }
    };
    Status {
        nook_version: env!("CARGO_PKG_VERSION"),
        windows: WindowsVersion::query().ok(),
        sandbox: desktops.is_sandbox(),
        user_desktop: desktops.user.display().to_string(),
        public_desktop: desktops.public.display().to_string(),
        monitor,
        icon_size: view.as_ref().map(|v| v.icon_size),
        grid,
        view,
        explorer: ExplorerSettings::read(),
        pending_plan: None,
        last_plan: None,
        quarantine,
        errors,
    }
}
