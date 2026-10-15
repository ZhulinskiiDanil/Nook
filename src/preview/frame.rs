//! Builds the full preview frame for a plan (worker thread, needs COM).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tiny_skia::Pixmap;

use super::render::{self, Geom};
use super::sources::{self, TextJob};
use crate::config::Config;
use crate::plan::model::StoredPlan;
use crate::platform::PrimaryMonitor;
use crate::scan::ScanResult;
use crate::shell::desktop_view::{ViewInfo, ViewItem, grid_dims, grid_origin};

/// Desktop view facts gathered on the UI thread.
#[derive(Debug, Clone)]
pub struct ViewFacts {
    pub info: ViewInfo,
    pub items: Vec<ViewItem>,
    /// Screen position of the view's client (0,0).
    pub client_origin: (i32, i32),
}

pub struct Frame {
    pub plan_id: String,
    pub width: u32,
    pub height: u32,
    /// Premultiplied BGRA, top-down.
    pub bgra: Vec<u8>,
    pub pixmap: Pixmap,
    /// Icons + captions on transparent: drawn over live wallpaper frames while showing.
    pub layer: Arc<Pixmap>,
    /// Wallpaper Engine is running: the background is animated and refreshed live.
    pub live_wallpaper: bool,
    pub build_ms: u128,
}

/// Last wallpaper frame captured while desktop icons were hidden (clean).
static CLEAN_LIVE: std::sync::Mutex<Option<Arc<Pixmap>>> = std::sync::Mutex::new(None);

pub fn remember_clean(p: Arc<Pixmap>) {
    *CLEAN_LIVE.lock().unwrap() = Some(p);
}

/// Background as on screen: Wallpaper Engine (its desktop layer) if running, else the static wallpaper.
/// With Wallpaper Engine, the capture includes desktop icons until the live loop starts (icons are
/// hidden only while the preview covers the screen), so a remembered clean frame is preferred.
pub fn background(mon: &PrimaryMonitor) -> (Pixmap, bool) {
    if let Some(layer) = sources::wallpaper_engine_layer() {
        if let Some(clean) = CLEAN_LIVE.lock().unwrap().clone()
            && clean.width() == mon.work.width() as u32
            && clean.height() == mon.work.height() as u32
        {
            return ((*clean).clone(), true);
        }
        match sources::capture_window(layer, mon.work) {
            Ok(p) => return (p, true),
            Err(e) => {
                tracing::warn!("Wallpaper Engine capture failed, using static wallpaper: {e:#}")
            }
        }
    }
    let stat = crate::shell::wallpaper::query(mon.monitor)
        .and_then(|w| sources::static_wallpaper(&w, mon.monitor, mon.work));
    match stat {
        Ok(p) => (p, false),
        Err(e) => {
            tracing::warn!("wallpaper unavailable: {e:#}");
            let mut p = Pixmap::new(
                mon.work.width().max(1) as u32,
                mon.work.height().max(1) as u32,
            )
            .unwrap();
            p.fill(tiny_skia::Color::from_rgba8(12, 14, 20, 255));
            (p, false)
        }
    }
}

pub fn build(
    scan: &ScanResult,
    sp: &StoredPlan,
    config: &Config,
    view: &ViewFacts,
) -> Result<Frame> {
    let t0 = Instant::now();
    let mon = PrimaryMonitor::query()?;
    let (sx, sy) = (view.info.spacing_x, view.info.spacing_y);
    let (cols, rows) = grid_dims(mon.work.width(), mon.work.height(), sx, sy);
    let geom = Geom {
        step_x: sx,
        step_y: sy,
        icon: view.info.icon_size.max(16),
        cols,
        rows,
        origin: grid_origin(&view.items, sx, sy),
        view_offset: (
            view.client_origin.0 - mon.work.left,
            view.client_origin.1 - mon.work.top,
        ),
    };
    let (bg, live) = background(&mon);

    let outcome = render::outcome(&sp.plan, cols, rows, config);
    let mut cache: HashMap<String, Option<Arc<Pixmap>>> = HashMap::new();
    // Icons the plan restyles: show the new look in the preview, not the old icon.
    let styled = styled_icons(scan, sp, config);
    let new_folder_style = styled.get("new:").cloned();
    for (path, pm) in styled {
        cache.insert(path, Some(pm));
    }
    let size = geom.icon;
    let mut icon_of = |p: &str| -> Option<Arc<Pixmap>> {
        cache
            .entry(p.to_lowercase())
            .or_insert_with(|| {
                sources::shell_icon(p, size)
                    .map(Arc::new)
                    .map_err(|e| tracing::debug!("icon {p}: {e:#}"))
                    .ok()
            })
            .clone()
    };
    let folder = new_folder_style.or_else(|| {
        sources::shell_icon(&std::env::temp_dir().to_string_lossy(), size)
            .ok()
            .map(Arc::new)
    });
    let scene = render::scene(
        &scan.items,
        &sp.plan,
        &outcome,
        &geom,
        &mut icon_of,
        folder,
        (sp.before, sp.after),
    );

    let font = sources::caption_font(mon.dpi);
    let line = sources::caption_line_height(&font);
    let job = |c: &render::Caption| {
        let (x, w) = geom.caption_box(c.cell);
        let (_, iy) = geom.icon_pos(c.cell);
        TextJob {
            text: c.text.clone(),
            x,
            y: iy + geom.icon + 4,
            w,
            max_lines: 2,
            single_line: false,
        }
    };
    let (w, h) = (bg.width() as i32, bg.height() as i32);
    let mut normal: Vec<TextJob> = scene.captions.iter().filter(|c| !c.dim).map(job).collect();
    normal.push(TextJob {
        text: scene.header.clone(),
        x: 0,
        y: 10 + line / 2,
        w,
        max_lines: 1,
        single_line: true,
    });
    let dim: Vec<TextJob> = scene.captions.iter().filter(|c| c.dim).map(job).collect();
    let m1 = sources::text_mask(w, h, &font, &normal).context("caption text")?;
    let m2 = sources::text_mask(w, h, &font, &dim).context("dim caption text")?;

    let layer = render::layer(
        bg.width(),
        bg.height(),
        &scene,
        (&m1, &m2),
        render::parse_hex(&config.preview.accent),
    );
    let pm = render::flatten(&bg, &layer);
    Ok(Frame {
        plan_id: sp.id.clone(),
        width: pm.width(),
        height: pm.height(),
        bgra: render::to_bgra(&pm),
        pixmap: pm,
        layer: Arc::new(layer),
        live_wallpaper: live,
        build_ms: t0.elapsed().as_millis(),
    })
}

/// Styled icon per item path (lowercase) for every restyle/dim the plan makes; key `"new:"`
/// holds the look for folders the plan creates. Same choices as apply (`look_for`, `base_from`),
/// so the preview shows exactly what the desktop will get. Best effort.
fn styled_icons(
    scan: &ScanResult,
    sp: &StoredPlan,
    config: &Config,
) -> HashMap<String, Arc<Pixmap>> {
    use crate::plan::apply::{base_from, look_for};
    use crate::plan::model::NEW_PREFIX;
    let mut out = HashMap::new();
    if !config.icons.enabled {
        return out;
    }
    let accent = render::parse_hex(&config.icons.accent);
    let journal = crate::paths::AppDirs::resolve()
        .map(|d| d.data_dir.join("journal"))
        .unwrap_or_default();
    let by_id: HashMap<&str, &crate::scan::Item> =
        scan.items.iter().map(|i| (i.id.as_str(), i)).collect();
    for a in &sp.plan.actions {
        let Some(r) = a.item() else { continue };
        let Some(look) = look_for(&sp.plan, r, a, config) else {
            continue;
        };
        let (key, base) = match by_id.get(r) {
            Some(i) if crate::plan::validate::styleable(i.kind) && i.writable => (
                i.path.to_lowercase(),
                base_from(r, std::path::Path::new(&i.path), i.kind, &by_id, &journal),
            ),
            None if r.starts_with(NEW_PREFIX) => (
                "new:".to_string(),
                std::env::temp_dir().to_string_lossy().into_owned(),
            ),
            _ => continue,
        };
        match crate::icons::base_icon(&base) {
            Ok(b) => {
                let mut pm = crate::icons::render(look.style, look.shape, &b, accent);
                if look.opacity < 1.0 {
                    pm = crate::icons::faded(&pm, look.opacity);
                }
                out.insert(key, Arc::new(pm));
            }
            Err(e) => tracing::debug!("preview style for {r}: {e:#}"),
        }
    }
    out
}

/// Gathers [`ViewFacts`]. STA/UI thread only.
pub fn view_facts() -> Result<ViewFacts> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    let v = crate::shell::desktop_view::DesktopView::open()?;
    let mut pt = POINT::default();
    // SAFETY: valid window handle from the shell view.
    let _ = unsafe { ClientToScreen(v.hwnd, &mut pt) };
    Ok(ViewFacts {
        info: v.info()?,
        items: v.items()?,
        client_origin: (pt.x, pt.y),
    })
}
