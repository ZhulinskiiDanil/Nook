//! Shell thumbnails (images, video, PSD, PDF — whatever handlers are installed) → PNG.

use std::path::Path;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HGDIOBJ,
};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_RESIZETOFIT,
};
use windows::core::HSTRING;

pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Shell image for `path`, at most `max` px on the long side. Requires COM.
pub fn shell_image(path: &Path, max: i32) -> Result<Rgba> {
    // SAFETY: GDI objects are released; DIB buffer sized from BITMAP dims.
    unsafe {
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None)
                .context("SHCreateItemFromParsingName")?;
        let hbm = get_image_retrying(
            &factory,
            SIZE { cx: max, cy: max },
            SIIGBF_RESIZETOFIT | SIIGBF_BIGGERSIZEOK,
        )?;
        hbitmap_to_rgba(hbm)
    }
}

/// Shell *icon* (never a thumbnail) for a path or `::{CLSID}` at `size` px. Requires COM.
pub fn shell_icon_rgba(parsing: &str, size: i32) -> Result<Rgba> {
    use windows::Win32::UI::Shell::{SIIGBF_ICONONLY, SIIGBF_SCALEUP};
    // SAFETY: COM call; the bitmap is freed in hbitmap_to_rgba.
    unsafe {
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(&HSTRING::from(parsing), None)
                .with_context(|| format!("SHCreateItemFromParsingName {parsing}"))?;
        let hbm = get_image_retrying(
            &factory,
            SIZE { cx: size, cy: size },
            // SCALEUP: small-only icons are scaled up instead of framed in a 256 px tile.
            SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK | SIIGBF_SCALEUP,
        )?;
        hbitmap_to_rgba(hbm)
    }
}

/// `GetImage` may answer E_PENDING while the shell's icon cache is still extracting; retry briefly.
fn get_image_retrying(
    f: &IShellItemImageFactory,
    size: SIZE,
    flags: windows::Win32::UI::Shell::SIIGBF,
) -> Result<windows::Win32::Graphics::Gdi::HBITMAP> {
    const E_PENDING: windows::core::HRESULT = windows::core::HRESULT(0x8000_000Au32 as i32);
    let mut last = None;
    for _ in 0..40 {
        // SAFETY: plain COM call.
        match unsafe { f.GetImage(size, flags) } {
            Ok(h) => return Ok(h),
            Err(e) if e.code() == E_PENDING => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => {
                return Err(anyhow::Error::from(e).context("IShellItemImageFactory::GetImage"));
            }
        }
    }
    Err(anyhow::Error::from(last.unwrap())
        .context("IShellItemImageFactory::GetImage: still pending after 1 s"))
}

/// Converts (and frees) a shell HBITMAP to straight RGBA.
///
/// # Safety
/// `hbm` must be a valid bitmap handle the caller owns; it is deleted here.
pub unsafe fn hbitmap_to_rgba(hbm: windows::Win32::Graphics::Gdi::HBITMAP) -> Result<Rgba> {
    // SAFETY: per the function contract; GDI objects are released.
    unsafe {
        let mut bm = BITMAP::default();
        let got = GetObjectW(
            HGDIOBJ(hbm.0),
            size_of::<BITMAP>() as i32,
            Some((&mut bm as *mut BITMAP).cast()),
        );
        if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
            let _ = DeleteObject(HGDIOBJ(hbm.0));
            bail!("empty bitmap");
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; (w * h * 4) as usize];
        let dc = CreateCompatibleDC(None);
        let lines = GetDIBits(
            dc,
            hbm,
            0,
            h as u32,
            Some(px.as_mut_ptr().cast()),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        if lines == 0 {
            bail!("GetDIBits failed");
        }
        Ok(Rgba {
            width: w as u32,
            height: h as u32,
            pixels: bgra_to_rgba(px),
        })
    }
}

/// BGRA (premultiplied or with unused alpha) → straight RGBA.
pub fn bgra_to_rgba(mut px: Vec<u8>) -> Vec<u8> {
    let no_alpha = px.as_chunks::<4>().0.iter().all(|p| p[3] == 0);
    for p in px.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
        if no_alpha {
            p[3] = 255;
        } else if p[3] != 0 && p[3] != 255 {
            let a = u32::from(p[3]);
            for c in &mut p[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    px
}

pub fn encode_png(img: &Rgba) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.width, img.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header()?;
        w.write_image_data(&img.pixels)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_channels() {
        assert_eq!(
            bgra_to_rgba(vec![1, 2, 3, 0, 4, 5, 6, 0]),
            vec![3, 2, 1, 255, 6, 5, 4, 255]
        );
        // premultiplied half-alpha red: B=0 G=0 R=64 A=128 → R≈128
        assert_eq!(
            bgra_to_rgba(vec![0, 0, 64, 128, 0, 0, 0, 255]),
            vec![128, 0, 0, 128, 0, 0, 0, 255]
        );
    }

    #[test]
    fn png_roundtrip_header() {
        let img = Rgba {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 255, 0, 255, 0, 255],
        };
        let png = encode_png(&img).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }
}
