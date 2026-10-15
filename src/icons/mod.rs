//! Nook icon styles (spec §5.4): `tile` (colored squircle), `orb` (round glass),
//! `muted` (accent-tinted grayscale for inactive items). Generates multi-size `.ico` files
//! (16–256, PNG inside) named by content hash so Explorer's icon cache never shows a stale one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tiny_skia::{
    Color, FillRule, FilterQuality, IntSize, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke,
    Transform,
};

use crate::config::IconStyle;

pub const SIZES: [u32; 7] = [256, 128, 64, 48, 32, 24, 16];
const BASE: f32 = 256.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Squircle,
    Circle,
}

fn squircle(x: f32, y: f32, w: f32, r: f32) -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + w - r);
    pb.quad_to(x + w, y + w, x + w - r, y + w);
    pb.line_to(x + r, y + w);
    pb.quad_to(x, y + w, x, y + w - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

fn shape_path(shape: Shape) -> tiny_skia::Path {
    // A little inset so the rim isn't clipped at small sizes.
    let (m, w) = (BASE * 0.04, BASE * 0.92);
    match shape {
        Shape::Squircle => squircle(m, m, w, w * 0.28),
        Shape::Circle => PathBuilder::from_circle(BASE / 2.0, BASE / 2.0, w / 2.0).unwrap(),
    }
}

/// Average color of saturated opaque pixels (straight RGB, 0..255).
pub fn dominant(src: &Pixmap) -> (f32, f32, f32) {
    let (mut r, mut g, mut b, mut n) = (0f32, 0f32, 0f32, 0f32);
    for p in src.pixels() {
        if p.alpha() < 200 {
            continue;
        }
        let c = p.demultiply();
        let (pr, pg, pb) = (
            f32::from(c.red()),
            f32::from(c.green()),
            f32::from(c.blue()),
        );
        let w = (pr.max(pg).max(pb) - pr.min(pg).min(pb)) / 255.0 + 0.05;
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

/// Accent-tinted grayscale copy (premultiplied preserved).
pub fn muted(src: &Pixmap, accent: Color) -> Pixmap {
    let mut out = src.clone();
    let (ar, ag, ab) = (accent.red(), accent.green(), accent.blue());
    for px in out.pixels_mut() {
        let c = px.demultiply();
        let l = (0.299 * f32::from(c.red())
            + 0.587 * f32::from(c.green())
            + 0.114 * f32::from(c.blue()))
            / 255.0;
        let l = l.powf(0.8);
        let ch = |a: f32| ((l * (0.55 + 0.45 * a)) * 255.0 + 25.0).min(255.0) as u8;
        *px = tiny_skia::ColorU8::from_rgba(ch(ar), ch(ag), ch(ab), c.alpha()).premultiply();
    }
    out
}

fn draw_centered(dst: &mut Pixmap, src: &Pixmap, size: f32) {
    let s = size / src.width().max(src.height()) as f32;
    let tx = (BASE - src.width() as f32 * s) / 2.0;
    let ty = (BASE - src.height() as f32 * s) / 2.0;
    let paint = PixmapPaint {
        quality: FilterQuality::Bicubic,
        ..Default::default()
    };
    dst.draw_pixmap(
        0,
        0,
        src.as_ref(),
        &paint,
        Transform::from_row(s, 0.0, 0.0, s, tx, ty),
        None,
    );
}

/// Recolors to the accent hue but keeps the icon's own shading (a light tint, not flat).
pub fn tint(src: &Pixmap, accent: Color) -> Pixmap {
    let mut out = src.clone();
    let (ar, ag, ab) = (accent.red(), accent.green(), accent.blue());
    for px in out.pixels_mut() {
        let c = px.demultiply();
        let l = (0.299 * f32::from(c.red())
            + 0.587 * f32::from(c.green())
            + 0.114 * f32::from(c.blue()))
            / 255.0;
        let ch = |a: f32| ((a * (0.35 + 0.75 * l)).min(1.0) * 255.0) as u8;
        *px = tiny_skia::ColorU8::from_rgba(ch(ar), ch(ag), ch(ab), c.alpha()).premultiply();
    }
    out
}

/// Multiplies opacity into the pixels (premultiplied stays premultiplied).
pub fn faded(src: &Pixmap, opacity: f32) -> Pixmap {
    let mut out = Pixmap::new(src.width(), src.height()).unwrap();
    let p = PixmapPaint {
        opacity,
        ..Default::default()
    };
    out.draw_pixmap(0, 0, src.as_ref(), &p, Transform::identity(), None);
    out
}

/// Separable box blur of the alpha channel, run twice (≈ gaussian). Returns alpha only.
fn blur_alpha(src: &Pixmap, r: usize) -> Vec<u8> {
    let (w, h) = (src.width() as usize, src.height() as usize);
    let mut a: Vec<u32> = src.pixels().iter().map(|p| u32::from(p.alpha())).collect();
    let mut tmp = vec![0u32; w * h];
    let n = (2 * r + 1) as u32;
    for _ in 0..2 {
        for y in 0..h {
            for x in 0..w {
                let (lo, hi) = (x.saturating_sub(r), (x + r).min(w - 1));
                tmp[y * w + x] = (lo..=hi).map(|i| a[y * w + i]).sum::<u32>() / n;
            }
        }
        for y in 0..h {
            for x in 0..w {
                let (lo, hi) = (y.saturating_sub(r), (y + r).min(h - 1));
                a[y * w + x] = (lo..=hi).map(|i| tmp[i * w + x]).sum::<u32>() / n;
            }
        }
    }
    a.into_iter().map(|v| v.min(255) as u8).collect()
}

/// Full-size glyph on a transparent 256 master (no plate).
fn full(src: &Pixmap, k: f32) -> Pixmap {
    let mut pm = Pixmap::new(BASE as u32, BASE as u32).unwrap();
    draw_centered(&mut pm, src, BASE * k);
    pm
}

/// Original icon with a soft drop shadow under it.
fn lifted(base: &Pixmap) -> Pixmap {
    let k = 0.90; // leave a little room for the shadow
    let glyph = full(base, k);
    let shadow_a = blur_alpha(&glyph, 7);
    let mut pm = Pixmap::new(BASE as u32, BASE as u32).unwrap();
    let dy = (BASE * 0.03) as usize;
    let w = BASE as usize;
    for (i, px) in pm.pixels_mut().iter_mut().enumerate() {
        let (x, y) = (i % w, i / w);
        if y < dy {
            continue;
        }
        let a = (f32::from(shadow_a[(y - dy) * w + x]) * 0.55) as u8;
        *px = tiny_skia::ColorU8::from_rgba(0, 0, 0, a).premultiply();
    }
    pm.draw_pixmap(
        0,
        0,
        glyph.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    pm
}

/// Renders the 256 px master for a style. `base` is the item's own icon (any size, ~256 ideal).
pub fn render(style: IconStyle, shape: Shape, base: &Pixmap, accent: Color) -> Pixmap {
    match style {
        IconStyle::Lifted => return lifted(base),
        IconStyle::Tint => return full(&tint(base, accent), 1.0),
        IconStyle::Muted => return full(&muted(base, accent), 1.0),
        _ => {}
    }
    let mut pm = Pixmap::new(BASE as u32, BASE as u32).unwrap();
    let path = shape_path(shape);
    let mut fill = Paint {
        anti_alias: true,
        ..Default::default()
    };
    let mut rim = Paint {
        anti_alias: true,
        ..Default::default()
    };
    let stroke = Stroke {
        width: BASE * 0.014,
        ..Default::default()
    };
    let glyph = match style {
        IconStyle::Tile => {
            let (r, g, b) = dominant(base);
            fill.set_color(Color::from_rgba8(
                (r * 0.45) as u8,
                (g * 0.45) as u8,
                (b * 0.45) as u8,
                240,
            ));
            rim.set_color(Color::from_rgba8(r as u8, g as u8, b as u8, 140));
            base.clone()
        }
        IconStyle::Orb => {
            fill.set_color(Color::from_rgba8(255, 255, 255, 34));
            rim.set_color(Color::from_rgba8(255, 255, 255, 90));
            base.clone()
        }
        IconStyle::Muted | IconStyle::Lifted | IconStyle::Tint => unreachable!("handled above"),
        IconStyle::None => return base.clone(),
    };
    pm.fill_path(&path, &fill, FillRule::Winding, Transform::identity(), None);
    pm.stroke_path(&path, &rim, &stroke, Transform::identity(), None);
    let k = if shape == Shape::Circle { 0.56 } else { 0.60 };
    draw_centered(&mut pm, &glyph, BASE * k);
    pm
}

fn resized(src: &Pixmap, size: u32) -> Pixmap {
    let mut out = Pixmap::new(size, size).unwrap();
    let s = size as f32 / src.width() as f32;
    let paint = PixmapPaint {
        quality: FilterQuality::Bicubic,
        ..Default::default()
    };
    out.draw_pixmap(
        0,
        0,
        src.as_ref(),
        &paint,
        Transform::from_scale(s, s),
        None,
    );
    out
}

/// `.ico` with PNG-compressed images at every size in [`SIZES`].
pub fn ico_bytes(master: &Pixmap) -> Result<Vec<u8>> {
    let images: Vec<(u32, Vec<u8>)> = SIZES
        .iter()
        .map(|&s| Ok((s, resized(master, s).encode_png()?)))
        .collect::<Result<_>>()?;
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (s, png) in &images {
        let d = if *s >= 256 { 0 } else { *s as u8 };
        out.extend_from_slice(&[d, d, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bpp
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in images {
        out.extend_from_slice(&png);
    }
    Ok(out)
}

/// Writes the icon under `dir` as `<hash>.ico` (content-addressed; existing file is reused).
pub fn store(dir: &Path, ico: &[u8]) -> Result<PathBuf> {
    let name = format!("{}.ico", &blake3::hash(ico).to_hex()[..16]);
    let path = dir.join(name);
    if !path.is_file() {
        crate::fsutil::write_atomic(&path, ico)?;
    }
    Ok(path)
}

/// Shell image of `parsing` at 256 px as a premultiplied pixmap. Requires COM.
pub fn base_icon(parsing: &str) -> Result<Pixmap> {
    let mut rgba = crate::shell::thumbs::shell_icon_rgba(parsing, 256)?;
    // Apps without a 256 px icon come back as a tiny icon framed in a 256 tile. Then the
    // honest 48 px version is the better source (desktop icons are 32–48 px anyway).
    let opaque = rgba.pixels.as_chunks::<4>().0.iter().filter(|p| p[3] > 128).count();
    if (opaque as f32) < (rgba.width * rgba.height) as f32 * 0.06
        && let Ok(small) = crate::shell::thumbs::shell_icon_rgba(parsing, 48)
    {
        rgba = small;
    }
    let mut data = rgba.pixels;
    for p in data.as_chunks_mut::<4>().0 {
        let a = u16::from(p[3]);
        for c in &mut p[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
    Pixmap::from_vec(
        data,
        IntSize::from_wh(rgba.width, rgba.height).context("empty icon")?,
    )
    .context("pixmap")
}

/// Generates and stores the styled icon for an item. Returns the `.ico` path.
pub fn make(
    icons_dir: &Path,
    base_from: &str,
    style: IconStyle,
    shape: Shape,
    accent: Color,
    opacity: f32,
) -> Result<PathBuf> {
    let base = base_icon(base_from)?;
    let mut master = render(style, shape, &base, accent);
    if opacity < 1.0 {
        master = faded(&master, opacity);
    }
    store(icons_dir, &ico_bytes(&master)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Pixmap {
        let mut p = Pixmap::new(64, 64).unwrap();
        p.fill(Color::from_rgba8(30, 200, 90, 255)); // Spotify-ish green
        p
    }

    #[test]
    fn tile_uses_icon_color_orb_is_glass_muted_is_gray_blue() {
        let accent = Color::from_rgba8(0x8f, 0xb3, 0xff, 255);
        let tile = render(IconStyle::Tile, Shape::Squircle, &sample(), accent);
        let corner_in = tile.pixel(40, 128).unwrap().demultiply(); // inside plate, outside glyph
        assert!(
            corner_in.green() > corner_in.red() && corner_in.green() > corner_in.blue(),
            "tile tinted by icon color"
        );

        let orb = render(IconStyle::Orb, Shape::Circle, &sample(), accent);
        assert_eq!(
            orb.pixel(2, 2).unwrap().alpha(),
            0,
            "outside the circle is transparent"
        );

        let m = render(IconStyle::Muted, Shape::Squircle, &sample(), accent);
        let c = m.pixel(128, 128).unwrap().demultiply();
        assert!(
            c.blue() >= c.green() && c.green() >= c.red(),
            "muted glyph leans to the accent: {c:?}"
        );
    }

    #[test]
    fn ico_structure() {
        let ico = ico_bytes(&render(
            IconStyle::Orb,
            Shape::Circle,
            &sample(),
            Color::WHITE,
        ))
        .unwrap();
        assert_eq!(&ico[..4], &[0, 0, 1, 0]);
        assert_eq!(u16::from_le_bytes([ico[4], ico[5]]) as usize, SIZES.len());
        // First entry: 256 px stored as 0, PNG payload at the recorded offset.
        assert_eq!(ico[6], 0);
        let off = u32::from_le_bytes(ico[18..22].try_into().unwrap()) as usize;
        assert_eq!(&ico[off..off + 8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn store_is_content_addressed() {
        let d = tempfile::tempdir().unwrap();
        let a = store(d.path(), b"abc").unwrap();
        let b = store(d.path(), b"abc").unwrap();
        let c = store(d.path(), b"abd").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
