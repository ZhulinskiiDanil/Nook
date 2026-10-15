//! Preview frame (spec §7.3): wallpaper, ghosts of what leaves, final icons with captions,
//! one hint line on top. Composition is pure tiny-skia; inputs come from `sources`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use tiny_skia::{
    Color, FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Stroke, Transform,
};

use crate::config::Config;
use crate::layout::engine::{self, Cell, Entry, Group};
use crate::layout::free_zones::blocked_cells;
use crate::plan::model::{Action, NEW_PREFIX, PlanInput};
use crate::scan::Item;

/// Where the desktop view puts things, in frame (work area) pixels.
#[derive(Debug, Clone, Copy)]
pub struct Geom {
    pub step_x: i32,
    pub step_y: i32,
    pub icon: i32,
    pub cols: i32,
    pub rows: i32,
    /// Grid origin inside the view (see `grid_origin`).
    pub origin: (i32, i32),
    /// View client (0,0) relative to the work area's top-left.
    pub view_offset: (i32, i32),
}

impl Geom {
    /// Top-left of a cell's slot in frame pixels.
    pub fn slot(&self, c: Cell) -> (i32, i32) {
        (
            c.col * self.step_x + self.origin.0 + self.view_offset.0,
            c.row * self.step_y + self.origin.1 + self.view_offset.1,
        )
    }
    /// Top-left of the icon image. Explorer's item position is already the icon's left edge
    /// (the grid origin carries the centering), so only a small top inset is added.
    pub fn icon_pos(&self, c: Cell) -> (i32, i32) {
        let (x, y) = self.slot(c);
        (x, y + 2)
    }

    /// Caption box (x, width): one grid step wide, centered under the icon.
    pub fn caption_box(&self, c: Cell) -> (i32, i32) {
        let (ix, _) = self.icon_pos(c);
        let w = self.step_x - 2;
        (ix + self.icon / 2 - w / 2, w)
    }
}

#[derive(Clone)]
pub struct Sprite {
    pub icon: Arc<Pixmap>,
    pub x: i32,
    pub y: i32,
    pub size: i32,
    pub opacity: f32,
    pub red_outline: bool,
    pub underline: bool,
}

pub struct Caption {
    pub text: String,
    pub cell: Cell,
    pub dim: bool,
}

/// Everything the frame shows, before rasterizing text.
pub struct Scene {
    pub ghosts: Vec<Sprite>,
    pub finals: Vec<Sprite>,
    pub captions: Vec<Caption>,
    pub header: String,
    pub before: usize,
    pub after: usize,
}

/// What happens to each item under the plan (pure).
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// ref → final cell (ids and `new:` refs).
    pub cells: BTreeMap<String, Cell>,
    pub moved_out: HashSet<String>,
    pub quarantined: HashSet<String>,
    pub hidden_caption: HashSet<String>,
    pub dimmed: HashSet<String>,
    pub renamed: HashMap<String, String>,
}

pub fn outcome(plan: &PlanInput, cols: i32, rows: i32, config: &Config) -> Outcome {
    let mut o = Outcome::default();
    let mut groups: BTreeMap<Group, Vec<Entry>> = BTreeMap::new();
    for (g, entries) in [
        (Group::Apps, &plan.layout.apps),
        (Group::Projects, &plan.layout.projects),
        (Group::Inbox, &plan.layout.inbox),
        (Group::Trash, &plan.layout.trash),
    ] {
        groups
            .entry(g)
            .or_default()
            .extend(entries.iter().map(|e| Entry {
                id: e.item().into(),
                pin: e.pin().map(|p| Cell {
                    col: p.col,
                    row: p.row,
                }),
            }));
    }
    let input = engine::Input {
        cols,
        rows,
        blocked: blocked_cells(cols as usize, rows as usize, &config.layout.keep_free),
        groups,
    };
    o.cells = engine::layout(&input, &config.layout).cells;
    for a in &plan.actions {
        let Some(r) = a.item().map(String::from) else {
            continue;
        };
        match a {
            Action::MoveToFolder { .. } => {
                o.moved_out.insert(r);
            }
            Action::Quarantine { .. } => {
                o.quarantined.insert(r);
            }
            Action::HideCaption { .. } => {
                o.hidden_caption.insert(r);
            }
            Action::Dim { value: true, .. } => {
                o.dimmed.insert(r);
            }
            Action::Rename { new_name, .. } => {
                o.renamed.insert(r, new_name.clone());
            }
            _ => {}
        }
    }
    o
}

/// Builds the scene. `icon_of` returns an icon for an item path / `::{CLSID}`;
/// `folder_icon` is used for folders the plan creates.
pub fn scene(
    items: &[Item],
    plan: &PlanInput,
    o: &Outcome,
    g: &Geom,
    icon_of: &mut dyn FnMut(&str) -> Option<Arc<Pixmap>>,
    folder_icon: Option<Arc<Pixmap>>,
    (before, after): (usize, usize),
) -> Scene {
    let by_id: HashMap<&str, &Item> = items.iter().map(|i| (i.id.as_str(), i)).collect();
    let mut ghosts = Vec::new();
    for i in items {
        let (out, q) = (o.moved_out.contains(&i.id), o.quarantined.contains(&i.id));
        if !(out || q) {
            continue;
        }
        let (Some(c), Some(icon)) = (i.cell, icon_of(&i.path)) else {
            continue;
        };
        let (x, y) = g.icon_pos(Cell {
            col: c.col,
            row: c.row,
        });
        ghosts.push(Sprite {
            icon,
            x,
            y,
            size: g.icon,
            opacity: if q { 0.30 } else { 0.15 },
            red_outline: q,
            underline: false,
        });
    }
    let mut finals = Vec::new();
    let mut captions = Vec::new();
    for (r, cell) in &o.cells {
        let (icon, name, cur_cell, already_hidden) = match by_id.get(r.as_str()) {
            Some(i) => (
                icon_of(&i.path),
                i.display_name.clone(),
                i.cell,
                i.caption_hidden,
            ),
            None if r.starts_with(NEW_PREFIX) => (
                folder_icon.clone(),
                r.rsplit(['\\', '/']).next().unwrap_or("").to_string(),
                None,
                false,
            ),
            None => continue,
        };
        let Some(icon) = icon else { continue };
        let touched = plan.actions.iter().any(|a| a.item() == Some(r.as_str()));
        let moved = cur_cell.is_none_or(|c| c.col != cell.col || c.row != cell.row);
        let dim = o.dimmed.contains(r);
        let (x, y) = g.icon_pos(*cell);
        finals.push(Sprite {
            icon,
            x,
            y,
            size: g.icon,
            opacity: if dim { 0.5 } else { 1.0 },
            red_outline: false,
            underline: touched || moved,
        });
        if !already_hidden && !o.hidden_caption.contains(r) {
            let text = o.renamed.get(r).map_or(name, |n| {
                n.rsplit_once('.').map_or(n.clone(), |(s, _)| s.to_string())
            });
            captions.push(Caption {
                text,
                cell: *cell,
                dim,
            });
        }
    }
    let header =
        format!("{before} → {after}   ·   отпусти — как было   ·   Ctrl+Alt дважды — применить");
    Scene {
        ghosts,
        finals,
        captions,
        header,
        before,
        after,
    }
}

pub fn parse_hex(c: &str) -> Color {
    let h = c.trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).unwrap_or(0x8fb3ff);
    Color::from_rgba8((v >> 16) as u8, (v >> 8) as u8, v as u8, 255)
}

/// Blends a solid color through a coverage mask (0..255) at `alpha`, offset by (dx, dy).
fn blend_mask(pm: &mut Pixmap, mask: &[u8], (r, g, b): (u8, u8, u8), alpha: f32, dx: i32, dy: i32) {
    let (w, h) = (pm.width() as i32, pm.height() as i32);
    let data = pm.data_mut();
    for y in 0..h {
        let sy = y - dy;
        if sy < 0 || sy >= h {
            continue;
        }
        for x in 0..w {
            let sx = x - dx;
            if sx < 0 || sx >= w {
                continue;
            }
            let m = mask[(sy * w + sx) as usize];
            if m == 0 {
                continue;
            }
            let a = f32::from(m) / 255.0 * alpha;
            let i = ((y * w + x) * 4) as usize;
            for (k, c) in [r, g, b].into_iter().enumerate() {
                data[i + k] = (f32::from(c) * a + f32::from(data[i + k]) * (1.0 - a)).round() as u8;
            }
            data[i + 3] = (255.0 * a + f32::from(data[i + 3]) * (1.0 - a)).round() as u8;
        }
    }
}

fn draw_sprite(pm: &mut Pixmap, s: &Sprite, accent: Color) {
    let scale = s.size as f32 / s.icon.width().max(1) as f32;
    let paint = PixmapPaint {
        opacity: s.opacity,
        quality: tiny_skia::FilterQuality::Bicubic,
        ..Default::default()
    };
    pm.draw_pixmap(
        0,
        0,
        (*s.icon).as_ref(),
        &paint,
        Transform::from_row(scale, 0.0, 0.0, scale, s.x as f32, s.y as f32),
        None,
    );
    if s.red_outline {
        let r = Rect::from_xywh(
            s.x as f32 - 3.0,
            s.y as f32 - 3.0,
            s.size as f32 + 6.0,
            s.size as f32 + 6.0,
        )
        .unwrap();
        let mut p = Paint::default();
        p.set_color(Color::from_rgba8(0xff, 0x6b, 0x6b, 200));
        p.anti_alias = true;
        pm.stroke_path(
            &PathBuilder::from_rect(r),
            &p,
            &Stroke {
                width: 1.2,
                ..Default::default()
            },
            Transform::identity(),
            None,
        );
    }
    if s.underline {
        let w = (s.size as f32 * 0.75).round();
        let r = Rect::from_xywh(
            s.x as f32 + (s.size as f32 - w) / 2.0,
            (s.y + s.size + 1) as f32,
            w,
            2.0,
        )
        .unwrap();
        let mut p = Paint::default();
        p.set_color(accent);
        p.anti_alias = true;
        pm.fill_path(
            &PathBuilder::from_rect(r),
            &p,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// Final composition. `masks` = (normal text, dim text) coverage, frame-sized.
pub fn compose(background: &Pixmap, scene: &Scene, masks: (&[u8], &[u8]), accent: Color) -> Pixmap {
    flatten(
        background,
        &layer(
            background.width(),
            background.height(),
            scene,
            masks,
            accent,
        ),
    )
}

/// Background + icon layer (premultiplied, source-over).
pub fn flatten(background: &Pixmap, layer: &Pixmap) -> Pixmap {
    let mut pm = background.clone();
    pm.draw_pixmap(
        0,
        0,
        layer.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    pm
}

/// Everything except the wallpaper, on transparent: re-used over live wallpaper frames.
pub fn layer(w: u32, h: u32, scene: &Scene, masks: (&[u8], &[u8]), accent: Color) -> Pixmap {
    let mut pm = Pixmap::new(w, h).unwrap();
    for s in &scene.ghosts {
        draw_sprite(&mut pm, s, accent);
    }
    for s in &scene.finals {
        draw_sprite(&mut pm, s, accent);
    }
    // Explorer-like captions: soft dark shadow, then white.
    for (mask, a) in [(masks.0, 1.0f32), (masks.1, 0.5)] {
        blend_mask(&mut pm, mask, (0, 0, 0), 0.75 * a, 1, 1);
        blend_mask(&mut pm, mask, (255, 255, 255), a, 0, 0);
    }
    pm
}

/// tiny-skia premultiplied RGBA → premultiplied BGRA for UpdateLayeredWindow.
pub fn to_bgra(pm: &Pixmap) -> Vec<u8> {
    let mut v = pm.data().to_vec();
    for p in v.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::model::{Layout, LayoutEntry};

    fn plan() -> PlanInput {
        let ids = |v: &[&str]| v.iter().map(|s| LayoutEntry::Id(s.to_string())).collect();
        PlanInput {
            title: "t".into(),
            layout: Layout {
                apps: ids(&["a1", "a2"]),
                projects: ids(&[r"new:%DESKTOP%\Art"]),
                inbox: vec![],
                trash: ids(&["bin"]),
            },
            actions: vec![
                Action::Quarantine {
                    item: "q".into(),
                    reason: "r".into(),
                },
                Action::MoveToFolder {
                    item: "m".into(),
                    dest: r"%DESKTOP%\Art".into(),
                    create_dirs: false,
                    reason: "r".into(),
                },
                Action::HideCaption {
                    item: "a1".into(),
                    reason: "r".into(),
                },
                Action::Dim {
                    item: "a2".into(),
                    value: true,
                    reason: "r".into(),
                },
            ],
        }
    }

    #[test]
    fn outcome_classifies_items() {
        let o = outcome(&plan(), 10, 6, &Config::default());
        assert_eq!(o.cells.len(), 4);
        assert!(o.quarantined.contains("q") && o.moved_out.contains("m"));
        assert!(o.hidden_caption.contains("a1") && o.dimmed.contains("a2"));
        assert_eq!(o.cells["a1"], Cell { col: 1, row: 1 });
    }

    #[test]
    fn compose_draws_text_and_icons() {
        let bg = Pixmap::new(40, 20).unwrap();
        let mut icon = Pixmap::new(4, 4).unwrap();
        icon.fill(Color::from_rgba8(255, 0, 0, 255));
        let scene = Scene {
            ghosts: vec![],
            finals: vec![Sprite {
                icon: Arc::new(icon),
                x: 2,
                y: 2,
                size: 4,
                opacity: 1.0,
                red_outline: false,
                underline: false,
            }],
            captions: vec![],
            header: String::new(),
            before: 0,
            after: 0,
        };
        let mut mask = vec![0u8; 800];
        mask[10 * 40 + 20] = 255;
        let out = compose(&bg, &scene, (&mask, &vec![0u8; 800]), Color::WHITE);
        let px = |x: usize, y: usize| out.pixel(x as u32, y as u32).unwrap();
        assert_eq!((px(3, 3).red(), px(3, 3).green()), (255, 0), "icon drawn");
        assert_eq!(px(20, 10).red(), 255, "white text");
        assert!(
            px(21, 11).alpha() > 150 && px(21, 11).red() < 30,
            "dark shadow offset by 1px"
        );
        let bgra = to_bgra(&out);
        assert_eq!(
            &bgra[(3 * 40 + 3) * 4..(3 * 40 + 3) * 4 + 4],
            &[0, 0, 255, 255]
        );
    }
}
