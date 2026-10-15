//! Style mock-up: renders real desktop shortcut icons in several candidate styles.
//! Read-only: icons are only extracted, nothing on the desktop changes.
//! Usage: cargo run --example icon_styles -- <out.png> <shortcut paths...>

use std::path::Path;

use anyhow::{Context, Result};
use nook::platform::ComApartment;
use nook::shell::thumbs::{Rgba, shell_image};
use tiny_skia::{
    Color, FillRule, FilterQuality, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Shader, Stroke,
    Transform,
};

const TILE: f32 = 112.0;
const GAP: f32 = 28.0;
const STYLES: usize = 4;

fn to_pixmap(img: &Rgba) -> Pixmap {
    let mut data = img.pixels.clone();
    for p in data.as_chunks_mut::<4>().0 {
        let a = u16::from(p[3]);
        for c in &mut p[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
    Pixmap::from_vec(
        data,
        tiny_skia::IntSize::from_wh(img.width, img.height).unwrap(),
    )
    .unwrap()
}

fn rounded(x: f32, y: f32, w: f32, h: f32, r: f32) -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

fn circle(cx: f32, cy: f32, r: f32) -> tiny_skia::Path {
    PathBuilder::from_circle(cx, cy, r).unwrap()
}

fn fill(pm: &mut Pixmap, path: &tiny_skia::Path, c: Color) {
    let mut p = Paint::default();
    p.set_color(c);
    p.anti_alias = true;
    pm.fill_path(path, &p, FillRule::Winding, Transform::identity(), None);
}

fn stroke(pm: &mut Pixmap, path: &tiny_skia::Path, c: Color, w: f32) {
    let mut p = Paint::default();
    p.set_color(c);
    p.anti_alias = true;
    pm.stroke_path(
        path,
        &p,
        &Stroke {
            width: w,
            ..Default::default()
        },
        Transform::identity(),
        None,
    );
}

/// Average saturated color of the icon (for the "colored tile" style).
fn dominant(img: &Rgba) -> (f32, f32, f32) {
    let (mut r, mut g, mut b, mut n) = (0f32, 0f32, 0f32, 0f32);
    for p in img.pixels.as_chunks::<4>().0 {
        if p[3] < 200 {
            continue;
        }
        let (pr, pg, pb) = (f32::from(p[0]), f32::from(p[1]), f32::from(p[2]));
        let sat = pr.max(pg).max(pb) - pr.min(pg).min(pb);
        let w = sat / 255.0 + 0.05;
        r += pr * w;
        g += pg * w;
        b += pb * w;
        n += w;
    }
    if n == 0.0 {
        (90.0, 100.0, 120.0)
    } else {
        (r / n, g / n, b / n)
    }
}

/// Muted: luminance tinted toward the accent (#8fb3ff), keeps the glyph readable.
fn muted(img: &Rgba) -> Rgba {
    let mut px = img.pixels.clone();
    for p in px.as_chunks_mut::<4>().0 {
        let l = 0.299 * f32::from(p[0]) + 0.587 * f32::from(p[1]) + 0.114 * f32::from(p[2]);
        let l = (l / 255.0).powf(0.8);
        p[0] = (l * 0xb8 as f32 + 30.0).min(255.0) as u8;
        p[1] = (l * 0xcc as f32 + 30.0).min(255.0) as u8;
        p[2] = (l * 0xff as f32 + 20.0).min(255.0) as u8;
    }
    Rgba {
        width: img.width,
        height: img.height,
        pixels: px,
    }
}

fn draw_icon(pm: &mut Pixmap, icon: &Rgba, cx: f32, cy: f32, size: f32, opacity: f32) {
    let src = to_pixmap(icon);
    let s = size / icon.width.max(icon.height) as f32;
    let tx = cx - icon.width as f32 * s / 2.0;
    let ty = cy - icon.height as f32 * s / 2.0;
    let paint = PixmapPaint {
        opacity,
        quality: FilterQuality::Bicubic,
        ..Default::default()
    };
    pm.draw_pixmap(
        0,
        0,
        src.as_ref(),
        &paint,
        Transform::from_row(s, 0.0, 0.0, s, tx, ty),
        None,
    );
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = args
        .first()
        .context("usage: icon_styles <out.png> <paths...>")?;
    let _com = ComApartment::sta()?;
    let icons: Vec<Rgba> = args[1..]
        .iter()
        .filter_map(|p| {
            shell_image(Path::new(p), 256)
                .map_err(|e| eprintln!("{p}: {e:#}"))
                .ok()
        })
        .collect();

    let cols = icons.len() as f32;
    let w = GAP + cols * (TILE + GAP);
    let h = GAP + STYLES as f32 * (TILE + GAP);
    let mut pm = Pixmap::new(w as u32, h as u32).unwrap();
    // Dark "night sky" backdrop with a soft gradient, like a dark wallpaper.
    let bg = Paint {
        shader: tiny_skia::LinearGradient::new(
            tiny_skia::Point::from_xy(0.0, 0.0),
            tiny_skia::Point::from_xy(w, h),
            vec![
                tiny_skia::GradientStop::new(0.0, Color::from_rgba8(14, 18, 32, 255)),
                tiny_skia::GradientStop::new(1.0, Color::from_rgba8(36, 24, 52, 255)),
            ],
            tiny_skia::SpreadMode::Pad,
            Transform::identity(),
        )
        .unwrap_or(Shader::SolidColor(Color::BLACK)),
        ..Default::default()
    };
    pm.fill_rect(
        Rect::from_xywh(0.0, 0.0, w, h).unwrap(),
        &bg,
        Transform::identity(),
        None,
    );

    for (i, icon) in icons.iter().enumerate() {
        let x = GAP + i as f32 * (TILE + GAP);
        for s in 0..STYLES {
            let y = GAP + s as f32 * (TILE + GAP);
            let (cx, cy) = (x + TILE / 2.0, y + TILE / 2.0);
            match s {
                // 1. Glass tile: dark translucent squircle, thin light rim, original icon.
                0 => {
                    let p = rounded(x, y, TILE, TILE, TILE * 0.28);
                    fill(&mut pm, &p, Color::from_rgba8(255, 255, 255, 22));
                    stroke(&mut pm, &p, Color::from_rgba8(255, 255, 255, 46), 1.5);
                    draw_icon(&mut pm, icon, cx, cy, TILE * 0.60, 1.0);
                }
                // 2. Muted: accent squircle, icon in accent-tinted grayscale.
                1 => {
                    let p = rounded(x, y, TILE, TILE, TILE * 0.28);
                    fill(&mut pm, &p, Color::from_rgba8(0x8f, 0xb3, 0xff, 60));
                    stroke(&mut pm, &p, Color::from_rgba8(0x8f, 0xb3, 0xff, 140), 1.5);
                    draw_icon(&mut pm, &muted(icon), cx, cy, TILE * 0.60, 1.0);
                }
                // 3. Colored tile: squircle in the icon's own color (darkened), original icon.
                2 => {
                    let (r, g, b) = dominant(icon);
                    let p = rounded(x, y, TILE, TILE, TILE * 0.28);
                    fill(
                        &mut pm,
                        &p,
                        Color::from_rgba8(
                            (r * 0.45) as u8,
                            (g * 0.45) as u8,
                            (b * 0.45) as u8,
                            235,
                        ),
                    );
                    stroke(
                        &mut pm,
                        &p,
                        Color::from_rgba8(r as u8, g as u8, b as u8, 120),
                        1.5,
                    );
                    draw_icon(&mut pm, icon, cx, cy, TILE * 0.60, 1.0);
                }
                // 4. Orb: round glass, original icon.
                _ => {
                    let p = circle(cx, cy, TILE / 2.0);
                    fill(&mut pm, &p, Color::from_rgba8(255, 255, 255, 18));
                    stroke(&mut pm, &p, Color::from_rgba8(255, 255, 255, 40), 1.5);
                    draw_icon(&mut pm, icon, cx, cy, TILE * 0.56, 1.0);
                }
            }
        }
    }
    pm.save_png(out)?;
    println!("{} icons x {STYLES} styles → {out}", icons.len());
    Ok(())
}
