//! `nookctl doctor`: read-only environment report. Each section fails independently.

use std::fmt::Write as _;
use std::path::PathBuf;

use serde::Serialize;

use crate::paths::{AppDirs, Desktops};
use crate::platform::{PrimaryMonitor, WindowsVersion};
use crate::shell::desktop_view::{DesktopView, ViewInfo, grid_dims};
use crate::shell::explorer_settings::{ExplorerSettings, visible_virtual_icons};
use crate::shell::items::{FolderCounts, count};
use crate::shell::wallpaper::{self, WallpaperInfo};

type Section<T> = Result<T, String>;

fn sec<T>(r: anyhow::Result<T>) -> Section<T> {
    r.map_err(|e| format!("{e:#}"))
}

#[derive(Debug, Serialize)]
pub struct DesktopFolder {
    pub path: PathBuf,
    pub counts: Section<FolderCounts>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub nook_version: &'static str,
    pub windows: Section<WindowsVersion>,
    pub sandbox: Option<PathBuf>,
    pub config_file: Section<PathBuf>,
    pub monitor: Section<PrimaryMonitor>,
    pub view: Section<ViewInfo>,
    /// Columns × rows of the work area at the view's grid step.
    pub grid: Option<(i32, i32)>,
    pub explorer: ExplorerSettings,
    pub wallpaper: Section<WallpaperInfo>,
    pub user_desktop: Section<DesktopFolder>,
    pub public_desktop: Section<DesktopFolder>,
    pub virtual_items: Vec<&'static str>,
    /// Items Explorer should show: files per hidden settings + virtual icons.
    pub expected_visible: Option<usize>,
    pub warnings: Vec<String>,
}

impl Report {
    /// Requires an STA thread with COM initialised and Per-Monitor-V2 DPI awareness.
    pub fn collect() -> Self {
        let desktops = Desktops::resolve();
        let monitor = sec(PrimaryMonitor::query());
        let view = sec(DesktopView::open().and_then(|v| v.info()));
        let explorer = ExplorerSettings::read();
        let wallpaper = match &monitor {
            Ok(m) => sec(wallpaper::query(m.monitor)),
            Err(e) => Err(format!("no monitor: {e}")),
        };
        let folder = |path: PathBuf| DesktopFolder {
            counts: sec(count(&path)),
            path,
        };
        let (user_desktop, public_desktop, sandbox) = match &desktops {
            Ok(d) => (
                Ok(folder(d.user.clone())),
                Ok(folder(d.public.clone())),
                match &d.mode {
                    crate::paths::DesktopMode::Sandbox(root) => Some(root.clone()),
                    crate::paths::DesktopMode::Live => None,
                },
            ),
            Err(e) => (Err(format!("{e:#}")), Err(format!("{e:#}")), None),
        };
        let grid = match (&monitor, &view) {
            (Ok(m), Ok(v)) => Some(grid_dims(
                m.work.width(),
                m.work.height(),
                v.spacing_x,
                v.spacing_y,
            )),
            _ => None,
        };
        let virtual_items = visible_virtual_icons();

        let vis = |f: &Section<DesktopFolder>| -> Option<usize> {
            let c = f.as_ref().ok()?.counts.as_ref().ok()?;
            Some(c.visible(explorer.shows_hidden(), explorer.shows_super_hidden()))
        };
        let expected_visible = vis(&user_desktop)
            .zip(vis(&public_desktop))
            .map(|(u, p)| u + p + virtual_items.len());

        let mut warnings = Vec::new();
        if let Ok(v) = &view {
            if v.flags.auto_arrange {
                warnings.push(
                    "Auto-arrange is ON: a plan will need `explorer_setting auto_arrange_off`."
                        .into(),
                );
            }
            if !v.flags.icons_shown {
                warnings.push("Desktop icons are hidden (View → Show desktop icons).".into());
            }
            if !v.icon_mode {
                warnings.push("Desktop view is not in icon mode.".into());
            }
            if sandbox.is_none()
                && let Some(e) = expected_visible.filter(|&e| e as i32 != v.item_count)
            {
                warnings.push(format!(
                    "View shows {} items but folders suggest {e}; differences are usually extra shell items.",
                    v.item_count
                ));
            }
        }
        if !explorer.shows_hidden() {
            warnings.push(
                "Explorer hides hidden files: `dim` will be unavailable (needs Hidden = 1).".into(),
            );
        }
        if let Ok(w) = &wallpaper {
            if w.effective_path().is_none() {
                warnings.push(
                    "Wallpaper image not found (neither reported path nor TranscodedWallpaper)."
                        .into(),
                );
            }
            if w.mode == wallpaper::WallpaperMode::Span {
                warnings.push("Span wallpaper is not supported in the MVP.".into());
            }
        }
        if sandbox.is_some() {
            warnings.push(
                "Sandbox mode: counts are from the sandbox; icon positions are never touched."
                    .into(),
            );
        }

        Self {
            nook_version: env!("CARGO_PKG_VERSION"),
            windows: sec(WindowsVersion::query()),
            sandbox,
            config_file: sec(AppDirs::resolve().map(|d| d.config_file())),
            monitor,
            view,
            grid,
            explorer,
            wallpaper,
            user_desktop,
            public_desktop,
            virtual_items,
            expected_visible,
            warnings,
        }
    }

    pub fn render_text(&self) -> String {
        let mut o = String::new();
        let line = |o: &mut String, k: &str, v: String| {
            let _ = writeln!(o, "{k:<18} {v}");
        };
        let or_err = |r: Result<String, &String>| r.unwrap_or_else(|e| format!("ERROR: {e}"));

        let _ = writeln!(o, "Nook doctor v{}\n", self.nook_version);
        line(
            &mut o,
            "Windows",
            or_err(self.windows.as_ref().map(|w| w.to_string())),
        );
        line(
            &mut o,
            "Mode",
            self.sandbox
                .as_ref()
                .map_or("LIVE desktop (read-only)".into(), |r| {
                    format!("SANDBOX {}", r.display())
                }),
        );
        line(
            &mut o,
            "Config",
            or_err(self.config_file.as_ref().map(|p| p.display().to_string())),
        );

        let _ = writeln!(o, "\n[monitor]");
        match &self.monitor {
            Ok(m) => {
                line(&mut o, "Monitor", m.monitor.to_string());
                line(&mut o, "Work area", m.work.to_string());
                line(&mut o, "DPI", format!("{} ({}%)", m.dpi, m.scale_percent));
            }
            Err(e) => line(&mut o, "Monitor", format!("ERROR: {e}")),
        }

        let _ = writeln!(o, "\n[desktop view]");
        match &self.view {
            Ok(v) => {
                line(
                    &mut o,
                    "Grid step",
                    format!("{} x {} px", v.spacing_x, v.spacing_y),
                );
                if let Some((c, r)) = self.grid {
                    line(&mut o, "Grid", format!("{c} cols x {r} rows"));
                }
                line(
                    &mut o,
                    "Icon size",
                    format!(
                        "{} px{}",
                        v.icon_size,
                        if v.icon_mode { "" } else { " (not icon mode)" }
                    ),
                );
                line(&mut o, "Auto-arrange", yes_no(v.flags.auto_arrange));
                line(&mut o, "Snap to grid", yes_no(v.flags.snap_to_grid));
                line(&mut o, "Show icons", yes_no(v.flags.icons_shown));
                line(&mut o, "Items in view", v.item_count.to_string());
            }
            Err(e) => line(&mut o, "View", format!("ERROR: {e}")),
        }

        let _ = writeln!(o, "\n[explorer]");
        let reg = |v: Option<u32>| v.map_or("(not set)".into(), |v| v.to_string());
        line(
            &mut o,
            "Hidden",
            format!(
                "{} ({})",
                reg(self.explorer.hidden),
                if self.explorer.shows_hidden() {
                    "shown"
                } else {
                    "not shown"
                }
            ),
        );
        line(
            &mut o,
            "ShowSuperHidden",
            format!(
                "{} ({})",
                reg(self.explorer.show_super_hidden),
                if self.explorer.shows_super_hidden() {
                    "shown"
                } else {
                    "not shown"
                }
            ),
        );
        line(&mut o, "HideFileExt", reg(self.explorer.hide_file_ext));

        let _ = writeln!(o, "\n[wallpaper]");
        match &self.wallpaper {
            Ok(w) => {
                line(
                    &mut o,
                    "Path",
                    match &w.path {
                        Some(p) => format!(
                            "{}{}",
                            p.display(),
                            if w.path_exists { "" } else { "  (missing)" }
                        ),
                        None => "(none reported)".into(),
                    },
                );
                if let Some(t) = &w.transcoded {
                    line(&mut o, "Transcoded", t.display().to_string());
                }
                line(&mut o, "Mode", format!("{:?}", w.mode).to_lowercase());
                line(&mut o, "Background", format!("#{:06x}", w.background_color));
            }
            Err(e) => line(&mut o, "Wallpaper", format!("ERROR: {e}")),
        }

        let _ = writeln!(o, "\n[items]");
        for (label, f) in [
            ("User desktop", &self.user_desktop),
            ("Public desktop", &self.public_desktop),
        ] {
            match f {
                Ok(f) => {
                    line(&mut o, label, f.path.display().to_string());
                    match &f.counts {
                        Ok(c) => line(
                            &mut o,
                            "",
                            format!(
                                "{} total: {} files ({} shortcuts), {} folders; {} hidden, {} protected OS",
                                c.total, c.files, c.shortcuts, c.folders, c.hidden, c.protected_os
                            ),
                        ),
                        Err(e) => line(&mut o, "", format!("ERROR: {e}")),
                    }
                }
                Err(e) => line(&mut o, label, format!("ERROR: {e}")),
            }
        }
        line(
            &mut o,
            "Virtual",
            format!(
                "{} ({})",
                self.virtual_items.len(),
                self.virtual_items.join(", ")
            ),
        );
        if let Some(e) = self.expected_visible {
            line(&mut o, "Expected visible", e.to_string());
        }

        if !self.warnings.is_empty() {
            let _ = writeln!(o, "\n[warnings]");
            for w in &self.warnings {
                let _ = writeln!(o, "- {w}");
            }
        }
        o
    }
}

fn yes_no(b: bool) -> String {
    if b { "yes" } else { "no" }.into()
}
