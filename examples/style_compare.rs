//! Compare icon treatments at real desktop sizes (32 and 48 px), magnified ×3 without smoothing.
//! Rows: current tile · original (no arrow) · muted original · adaptive squircle (native size).
//! Read-only. Usage: cargo run --example style_compare -- <out.png> <paths...>

use anyhow::Result;
use nook::config::IconStyle;
use nook::icons::{Shape, base_icon, dominant, muted, render};
use nook::platform::ComApartment;
use tiny_skia::{
    Color, FillRule, FilterQuality, Mask, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Transform,
};

fn squircle(x: f32, y: f32, w: f32, r: f32) -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r * 0.45, y, x + w, y + r * 0.45, x + w, y + r);
    pb.line_to(x + w, y + w - r);
    pb.cubic_to(
        x + w,
        y + w - r * 0.45,
        x + w - r * 0.45,
        y + w,
        x + w - r,
        y + w,
    );
    pb.line_to(x + r, y + w);
    pb.cubic_to(x + r * 0.45, y + w, x, y + w - r * 0.45, x, y + w - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r * 0.45, x + r * 0.45, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

fn draw(dst: &mut Pixmap, src: &Pixmap, x: f32, y: f32, size: f32, mask: Option<&Mask>) {
    let s = size / src.width().max(src.height()) as f32;
    let p = PixmapPaint {
        quality: FilterQuality::Bicubic,
        ..Default::default()
    };
    dst.draw_pixmap(
        0,
        0,
        src.as_ref(),
        &p,
        Transform::from_row(s, 0.0, 0.0, s, x, y),
        mask,
    );
}

/// Adaptive: the app's own art fills a squircle drawn at the target size; transparent areas
/// get the icon's own (slightly darkened) color. No rim, glyph ~84 %.
fn adaptive(base: &Pixmap, n: u32) -> Pixmap {
    let nf = n as f32;
    let mut out = Pixmap::new(n, n).unwrap();
    let path = squircle(0.5, 0.5, nf - 1.0, nf * 0.30);
    let (r, g, b) = dominant(base);
    let mut fill = Paint {
        anti_alias: true,
        ..Default::default()
    };
    fill.set_color(Color::from_rgba8(
        (r * 0.55) as u8,
        (g * 0.55) as u8,
        (b * 0.55) as u8,
        255,
    ));
    out.fill_path(&path, &fill, FillRule::Winding, Transform::identity(), None);
    let mut mask = Mask::new(n, n).unwrap();
    mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
    let k = 0.84;
    draw(
        &mut out,
        base,
        nf * (1.0 - k) / 2.0,
        nf * (1.0 - k) / 2.0,
        nf * k,
        Some(&mask),
    );
    out
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let _com = ComApartment::sta()?;
    let accent = Color::from_rgba8(0x8f, 0xb3, 0xff, 255);
    let bases: Vec<Pixmap> = args[1..].iter().filter_map(|p| base_icon(p).ok()).collect();
    const Z: f32 = 3.0; // magnification
    let sizes = [32u32, 48];
    let rows = 4;
    let cell = 56.0;
    let w = (bases.len() as f32 * cell * 2.0 + 40.0) * Z;
    let h = (rows as f32 * cell + 20.0) * Z;
    let mut canvas = Pixmap::new(w as u32, h as u32).unwrap();
    canvas.fill(Color::from_rgba8(16, 14, 28, 255));
    for (si, &n) in sizes.iter().enumerate() {
        for (i, base) in bases.iter().enumerate() {
            let x0 =
                (si as f32 * bases.len() as f32 * cell + i as f32 * cell + 10.0 + si as f32 * 20.0)
                    * Z;
            let variants: Vec<Pixmap> = vec![
                {
                    // Current tile, rendered at 256 and downscaled (what Nook does now).
                    let big = render(IconStyle::Tile, Shape::Squircle, base, accent);
                    let mut p = Pixmap::new(n, n).unwrap();
                    draw(&mut p, &big, 0.0, 0.0, n as f32, None);
                    p
                },
                {
                    let mut p = Pixmap::new(n, n).unwrap();
                    draw(&mut p, base, 0.0, 0.0, n as f32, None);
                    p
                },
                {
                    let m = muted(base, accent);
                    let mut p = Pixmap::new(n, n).unwrap();
                    draw(&mut p, &m, 0.0, 0.0, n as f32, None);
                    p
                },
                adaptive(base, n),
            ];
            for (ri, v) in variants.iter().enumerate() {
                let y0 = (ri as f32 * cell + 10.0) * Z;
                // Nearest-neighbour magnification: shows the real pixels.
                let p = PixmapPaint {
                    quality: FilterQuality::Nearest,
                    ..Default::default()
                };
                canvas.draw_pixmap(
                    0,
                    0,
                    v.as_ref(),
                    &p,
                    Transform::from_row(Z, 0.0, 0.0, Z, x0, y0),
                    None,
                );
            }
        }
    }
    // Separator between the 32 px and 48 px halves.
    let sep = bases.len() as f32 * cell * Z + 15.0 * Z;
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(80, 80, 110, 255));
    canvas.fill_rect(
        Rect::from_xywh(sep, 0.0, 2.0, h).unwrap(),
        &paint,
        Transform::identity(),
        None,
    );
    canvas.save_png(&args[0])?;
    println!("rows: current tile | original | muted | adaptive; left 32 px, right 48 px (×3)");
    Ok(())
}
