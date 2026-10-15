//! Live-desktop `Desk`: every view call runs on the daemon's UI (STA) thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;

use super::ui::UiBridge;
use crate::layout::engine::Cell;
use crate::plan::apply::Desk;
use crate::positions::{self, Snapshot};
use crate::shell::desktop_view::{DesktopView, grid_dims, grid_origin, point_of};

pub struct LiveDesk {
    pub ui: UiBridge,
    pub data_dir: PathBuf,
}

impl Desk for LiveDesk {
    fn live(&self) -> bool {
        true
    }

    fn snapshot_positions(&self) -> Result<Option<PathBuf>> {
        let items = self.ui.call(|| DesktopView::open()?.items())??;
        let snap = Snapshot::from_view(&items);
        let name = format!(
            "plan-{}.json",
            crate::timefmt::rfc3339(SystemTime::now()).replace(':', "-")
        );
        Ok(Some(positions::save_as(&self.data_dir, &name, &snap)?))
    }

    fn restore_positions(&self, file: &Path) -> Result<()> {
        let targets = positions::load(file)?.targets();
        self.ui
            .call(move || DesktopView::open()?.set_positions(&targets))??;
        Ok(())
    }

    fn grid(&self) -> Option<(i32, i32)> {
        let info = self.ui.call(|| DesktopView::open()?.info()).ok()?.ok()?;
        let mon = crate::platform::PrimaryMonitor::query().ok()?;
        Some(grid_dims(
            mon.work.width(),
            mon.work.height(),
            info.spacing_x,
            info.spacing_y,
        ))
    }

    fn place(&self, cells: &HashMap<String, Cell>) -> Result<usize> {
        let cells = cells.clone();
        self.ui.call(move || {
            let v = DesktopView::open()?;
            let info = v.info()?;
            let origin = grid_origin(&v.items()?, info.spacing_x, info.spacing_y);
            let targets = cells
                .into_iter()
                .map(|(k, c)| {
                    (
                        k,
                        point_of(c.col, c.row, info.spacing_x, info.spacing_y, origin),
                    )
                })
                .collect();
            v.set_positions(&targets)
        })?
    }

    fn auto_arrange(&self) -> Result<bool> {
        Ok(self
            .ui
            .call(|| DesktopView::open()?.info())??
            .flags
            .auto_arrange)
    }

    fn set_auto_arrange(&self, on: bool) -> Result<()> {
        self.ui
            .call(move || DesktopView::open()?.set_auto_arrange(on))?
    }
}
