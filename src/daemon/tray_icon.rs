//! Nook's tray icon, drawn at runtime: a rounded frame with dots in the four corners
//! ("order comes from the corners"); an accent dot when a plan is waiting.

use anyhow::{Context, Result};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

pub fn draw(size: u32, pending: bool, accent: Color) -> Pixmap {
    let s = size as f32;
    let mut pm = Pixmap::new(size, size).unwrap();
    let mut p = Paint {
        anti_alias: true,
        ..Default::default()
    };
    p.set_color(Color::from_rgba8(240, 242, 248, 255));
    let r = s * 0.22;
    let (a, b) = (s * 0.12, s * 0.88);
    let mut pb = PathBuilder::new();
    pb.move_to(a + r, a);
    pb.line_to(b - r, a);
    pb.quad_to(b, a, b, a + r);
    pb.line_to(b, b - r);
    pb.quad_to(b, b, b - r, b);
    pb.line_to(a + r, b);
    pb.quad_to(a, b, a, b - r);
    pb.line_to(a, a + r);
    pb.quad_to(a, a, a + r, a);
    pb.close();
    let frame = pb.finish().unwrap();
    pm.stroke_path(
        &frame,
        &p,
        &Stroke {
            width: (s * 0.07).max(1.2),
            ..Default::default()
        },
        Transform::identity(),
        None,
    );
    let d = s * 0.085;
    for (x, y) in [(0.31, 0.31), (0.69, 0.31), (0.31, 0.69), (0.69, 0.69)] {
        let c = PathBuilder::from_circle(s * x, s * y, d).unwrap();
        pm.fill_path(&c, &p, FillRule::Winding, Transform::identity(), None);
    }
    if pending {
        let mut ap = Paint {
            anti_alias: true,
            ..Default::default()
        };
        ap.set_color(Color::from_rgba8(14, 16, 22, 255));
        let ring = PathBuilder::from_circle(s * 0.80, s * 0.20, s * 0.22).unwrap();
        pm.fill_path(&ring, &ap, FillRule::Winding, Transform::identity(), None);
        ap.set_color(accent);
        let dot = PathBuilder::from_circle(s * 0.80, s * 0.20, s * 0.16).unwrap();
        pm.fill_path(&dot, &ap, FillRule::Winding, Transform::identity(), None);
    }
    pm
}

/// Pixmap → HICON (32-bit with alpha). Caller destroys it with DestroyIcon.
pub fn to_hicon(pm: &Pixmap) -> Result<HICON> {
    let (w, h) = (pm.width() as i32, pm.height() as i32);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: DIB sized w*h*4; bitmaps deleted after the icon copies them.
    unsafe {
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            .context("CreateDIBSection")?;
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
        // Icons want straight (non-premultiplied) BGRA.
        for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(pm.pixels()) {
            let c = s.demultiply();
            *d = [c.blue(), c.green(), c.red(), c.alpha()];
        }
        let mask = CreateBitmap(w, h, 1, 1, None);
        let info = ICONINFO {
            fIcon: true.into(),
            hbmMask: mask,
            hbmColor: color,
            ..Default::default()
        };
        let icon = CreateIconIndirect(&info);
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        icon.context("CreateIconIndirect")
    }
}
