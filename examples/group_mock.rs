//! Mock-up of "clean originals + light group styling" at real size (48 px) over the wallpaper.
//! apps: original + soft shadow · projects: accent-tinted folder · inbox: muted original.
//! Read-only. Usage: group_mock <out.png> <wallpaper.png> --apps <exe...> --inbox <exe...>

use anyhow::{Context, Result};
use nook::icons::{base_icon, muted};
use nook::platform::ComApartment;
use tiny_skia::{Color, FilterQuality, IntSize, Pixmap, PixmapPaint, Transform};

const ICON: f32 = 48.0;
const SX: f32 = 75.0;
const SY: f32 = 98.0;

/// Soft drop shadow: the icon's alpha, darkened, smeared over a few offsets (cheap blur).
fn shadow(src: &Pixmap) -> Pixmap {
    let mut sil = src.clone();
    for p in sil.pixels_mut() {
        let a = p.alpha();
        *p = tiny_skia::ColorU8::from_rgba(0, 0, 0, a).premultiply();
    }
    let mut out = Pixmap::new(src.width() + 16, src.height() + 16).unwrap();
    let paint = PixmapPaint {
        opacity: 0.06,
        quality: FilterQuality::Bilinear,
        ..Default::default()
    };
    for dy in -3..=5 {
        for dx in -4..=4 {
            if dx * dx + (dy - 1) * (dy - 1) <= 18 {
                out.draw_pixmap(
                    8 + dx,
                    8 + dy + 2,
                    sil.as_ref(),
                    &paint,
                    Transform::identity(),
                    None,
                );
            }
        }
    }
    out
}

/// Recolors an icon to the accent hue, keeping its shading (light tint, not flat).
fn tint(src: &Pixmap, accent: Color) -> Pixmap {
    let mut out = src.clone();
    for p in out.pixels_mut() {
        let c = p.demultiply();
        let l = (0.299 * f32::from(c.red())
            + 0.587 * f32::from(c.green())
            + 0.114 * f32::from(c.blue()))
            / 255.0;
        let mix = |a: f32| ((a * (0.35 + 0.75 * l)).min(1.0) * 255.0) as u8;
        *p = tiny_skia::ColorU8::from_rgba(
            mix(accent.red()),
            mix(accent.green()),
            mix(accent.blue()),
            c.alpha(),
        )
        .premultiply();
    }
    out
}

fn put(canvas: &mut Pixmap, icon: &Pixmap, x: f32, y: f32, opacity: f32) {
    let s = ICON / icon.width() as f32;
    let p = PixmapPaint {
        opacity,
        quality: FilterQuality::Bicubic,
        ..Default::default()
    };
    canvas.draw_pixmap(
        0,
        0,
        icon.as_ref(),
        &p,
        Transform::from_row(s, 0.0, 0.0, s, x, y),
        None,
    );
}

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let _com = ComApartment::sta()?;
    let out = &a[0];
    let wall = Pixmap::load_png(&a[1]).context("wallpaper png")?;
    let split = |flag: &str| -> Vec<String> {
        let i = a.iter().position(|x| x == flag).unwrap_or(a.len());
        a[i + 1..]
            .iter()
            .take_while(|x| !x.starts_with("--"))
            .cloned()
            .collect()
    };
    let apps: Vec<Pixmap> = split("--apps")
        .iter()
        .filter_map(|p| base_icon(p).ok())
        .collect();
    let inbox: Vec<Pixmap> = split("--inbox")
        .iter()
        .filter_map(|p| base_icon(p).ok())
        .collect();
    let folder = base_icon(&std::env::temp_dir().to_string_lossy())?;
    let accent = Color::from_rgba8(0x8f, 0xb3, 0xff, 255);

    // A 2560-wide strip of the wallpaper (top and bottom bands), scaled down 1:1 regions.
    let (w, h) = (900u32, 520u32);
    let mut c = Pixmap::new(w, h).unwrap();
    let src_x = 230.0;
    c.draw_pixmap(
        0,
        0,
        wall.as_ref(),
        &PixmapPaint::default(),
        Transform::from_translate(-src_x, -60.0),
        None,
    );

    // ↖ apps: 2 columns, originals with a soft shadow.
    for (i, ic) in apps.iter().enumerate() {
        let (col, row) = ((i % 2) as f32, (i / 2) as f32);
        let (x, y) = (30.0 + col * SX, 20.0 + row * SY * 0.82);
        let sh = shadow(ic);
        let s = ICON / ic.width() as f32;
        let p = PixmapPaint {
            quality: FilterQuality::Bicubic,
            ..Default::default()
        };
        c.draw_pixmap(
            0,
            0,
            sh.as_ref(),
            &p,
            Transform::from_row(s, 0.0, 0.0, s, x - 8.0 * s, y - 8.0 * s),
            None,
        );
        put(&mut c, ic, x, y, 1.0);
    }
    // ↗ projects: one accent-tinted folder look.
    let tf = tint(&folder, accent);
    for i in 0..4 {
        put(
            &mut c,
            &tf,
            w as f32 - 90.0,
            20.0 + i as f32 * SY * 0.82,
            if i == 3 { 0.5 } else { 1.0 },
        );
    }
    // ↘ inbox: muted originals, rows right to left.
    for (i, ic) in inbox.iter().enumerate() {
        let (col, row) = ((i % 5) as f32, (i / 5) as f32);
        put(
            &mut c,
            &muted(ic, accent),
            w as f32 - 90.0 - col * SX,
            h as f32 - 80.0 - row * SY * 0.82,
            0.9,
        );
    }
    // Magnify ×2 (nearest) so it's visible at real pixel size.
    let mut big = Pixmap::new(w * 2, h * 2).unwrap();
    let p = PixmapPaint {
        quality: FilterQuality::Nearest,
        ..Default::default()
    };
    big.draw_pixmap(0, 0, c.as_ref(), &p, Transform::from_scale(2.0, 2.0), None);
    let _ = IntSize::from_wh(1, 1);
    big.save_png(out)?;
    Ok(())
}
