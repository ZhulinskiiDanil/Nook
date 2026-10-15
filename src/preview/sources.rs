//! Win32 inputs for the preview frame: the wallpaper as it is on screen (static via WIC,
//! or a live Wallpaper Engine window), shell icons, and Explorer-style caption text masks.

use std::path::Path;

use anyhow::{Context, Result, bail};
use tiny_skia::{FilterQuality, IntSize, Pixmap, PixmapPaint, Transform};
use windows::Win32::Foundation::{GENERIC_READ, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC,
    CreateDIBSection, CreateFontIndirectW, DIB_RGB_COLORS, DT_CENTER, DT_EDITCONTROL,
    DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_WORDBREAK, DeleteDC, DeleteObject, DrawTextW,
    GetTextMetricsW, HGDIOBJ, LOGFONTW, SelectObject, SetBkMode, SetTextColor, TEXTMETRICW,
    TRANSPARENT,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPRGBA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::HiDpi::SystemParametersInfoForDpi;
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClassNameW, GetWindowRect, GetWindowThreadProcessId,
    SPI_GETICONTITLELOGFONT,
};
use windows::core::{BOOL, HSTRING};

use crate::platform::Rect;
use crate::shell::thumbs::Rgba;
use crate::shell::wallpaper::{WallpaperInfo, WallpaperMode};

/// Straight RGBA → tiny-skia (premultiplied).
pub fn to_pixmap(img: &Rgba) -> Option<Pixmap> {
    let mut data = img.pixels.clone();
    for p in data.as_chunks_mut::<4>().0 {
        let a = u16::from(p[3]);
        for c in &mut p[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
    Pixmap::from_vec(data, IntSize::from_wh(img.width, img.height)?)
}

/// Decodes any image WIC can read (jpg, png, bmp, heic with codec, TranscodedWallpaper).
pub fn decode_image(path: &Path) -> Result<Pixmap> {
    // SAFETY: COM objects are local; the buffer is sized from GetSize.
    unsafe {
        let f: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let dec = f
            .CreateDecoderFromFilename(
                &HSTRING::from(path.as_os_str()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .with_context(|| format!("decode {}", path.display()))?;
        let frame = dec.GetFrame(0)?;
        let conv = f.CreateFormatConverter()?;
        conv.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppPRGBA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let (mut w, mut h) = (0u32, 0u32);
        conv.GetSize(&mut w, &mut h)?;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        conv.CopyPixels(std::ptr::null(), w * 4, &mut buf)?;
        Pixmap::from_vec(buf, IntSize::from_wh(w, h).context("empty image")?).context("pixmap")
    }
}

/// Lays the wallpaper out like Windows does for `mode`, on the monitor, cropped to the work area.
pub fn place_wallpaper(
    img: &Pixmap,
    mode: WallpaperMode,
    bg: u32,
    monitor: Rect,
    work: Rect,
) -> Pixmap {
    let (w, h) = (work.width().max(1) as u32, work.height().max(1) as u32);
    let mut out = Pixmap::new(w, h).unwrap();
    out.fill(tiny_skia::Color::from_rgba8(
        (bg >> 16) as u8,
        (bg >> 8) as u8,
        bg as u8,
        255,
    ));
    // Monitor origin relative to the work area.
    let (ox, oy) = (
        (monitor.left - work.left) as f32,
        (monitor.top - work.top) as f32,
    );
    let (mw, mh) = (monitor.width() as f32, monitor.height() as f32);
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let paint = PixmapPaint {
        quality: FilterQuality::Bicubic,
        ..Default::default()
    };
    let draw = |out: &mut Pixmap, sx: f32, sy: f32, tx: f32, ty: f32| {
        out.draw_pixmap(
            0,
            0,
            img.as_ref(),
            &paint,
            Transform::from_row(sx, 0.0, 0.0, sy, ox + tx, oy + ty),
            None,
        );
    };
    match mode {
        WallpaperMode::Stretch => draw(&mut out, mw / iw, mh / ih, 0.0, 0.0),
        WallpaperMode::Fit => {
            let s = (mw / iw).min(mh / ih);
            draw(&mut out, s, s, (mw - iw * s) / 2.0, (mh - ih * s) / 2.0);
        }
        WallpaperMode::Center => draw(
            &mut out,
            1.0,
            1.0,
            ((mw - iw) / 2.0).round(),
            ((mh - ih) / 2.0).round(),
        ),
        WallpaperMode::Tile => {
            let mut y = 0.0;
            while y < mh {
                let mut x = 0.0;
                while x < mw {
                    draw(&mut out, 1.0, 1.0, x, y);
                    x += iw;
                }
                y += ih;
            }
        }
        // Fill (and Span/unknown on a single monitor): cover, centered.
        _ => {
            let s = (mw / iw).max(mh / ih);
            draw(&mut out, s, s, (mw - iw * s) / 2.0, (mh - ih * s) / 2.0);
        }
    }
    out
}

/// The static wallpaper image (reported path, else TranscodedWallpaper).
pub fn static_wallpaper(info: &WallpaperInfo, monitor: Rect, work: Rect) -> Result<Pixmap> {
    let path = info.effective_path().context("no wallpaper image")?;
    let img = decode_image(path)?;
    Ok(place_wallpaper(
        &img,
        info.mode,
        info.background_color,
        monitor,
        work,
    ))
}

/// Finds the Wallpaper Engine render window (a child of the desktop WorkerW/Progman
/// owned by wallpaper32/64.exe). `None` if Wallpaper Engine isn't running.
pub fn wallpaper_engine_window() -> Option<HWND> {
    fn process_name(hwnd: HWND) -> Option<String> {
        let mut pid = 0u32;
        // SAFETY: plain calls; handle closed.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            use windows::Win32::System::Threading::{
                OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
                QueryFullProcessImageNameW,
            };
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 512];
            let mut len = buf.len() as u32;
            let r = QueryFullProcessImageNameW(
                h,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut len,
            );
            let _ = windows::Win32::Foundation::CloseHandle(h);
            r.ok()?;
            let s = String::from_utf16_lossy(&buf[..len as usize]);
            Some(s.rsplit('\\').next()?.to_lowercase())
        }
    }
    fn class(hwnd: HWND) -> String {
        let mut buf = [0u16; 64];
        // SAFETY: buffer is ours.
        let n = unsafe { GetClassNameW(hwnd, &mut buf) };
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
    struct Search(Option<HWND>);
    extern "system" fn child(hwnd: HWND, lp: LPARAM) -> BOOL {
        // SAFETY: lp points at our Search for the duration of the enumeration.
        let s = unsafe { &mut *(lp.0 as *mut Search) };
        if process_name(hwnd)
            .is_some_and(|n| n.starts_with("wallpaper32") || n.starts_with("wallpaper64"))
        {
            s.0 = Some(hwnd);
            return false.into();
        }
        true.into()
    }
    extern "system" fn top(hwnd: HWND, lp: LPARAM) -> BOOL {
        let c = class(hwnd);
        if c == "WorkerW" || c == "Progman" {
            // SAFETY: same Search pointer.
            unsafe {
                let _ = EnumChildWindows(Some(hwnd), Some(child), lp);
                if (*(lp.0 as *mut Search)).0.is_some() {
                    return false.into();
                }
            }
        }
        true.into()
    }
    let mut s = Search(None);
    // SAFETY: callbacks only touch `s` through the LPARAM during this call.
    unsafe {
        let _ = EnumWindows(Some(top), LPARAM(&mut s as *mut Search as isize));
    }
    s.0
}

/// The window to capture for Wallpaper Engine: the WorkerW that hosts its render window.
/// (The DX11 window itself returns black; its host renders the composed wallpaper.)
pub fn wallpaper_engine_layer() -> Option<HWND> {
    let we = wallpaper_engine_window()?;
    // SAFETY: plain call.
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetParent(we) }
        .ok()
        .or(Some(we))
}

/// Grabs the current frame of a (possibly DirectX) window via PrintWindow(PW_RENDERFULLCONTENT).
pub fn capture_window(hwnd: HWND, work: Rect) -> Result<Pixmap> {
    let mut r = RECT::default();
    // SAFETY: GDI objects created and released here.
    unsafe {
        GetWindowRect(hwnd, &mut r)?;
        let (w, h) = ((r.right - r.left).max(1), (r.bottom - r.top).max(1));
        let (dc, bmp, bits) = dib(w, h)?;
        let old = SelectObject(dc, HGDIOBJ(bmp.0));
        let ok = PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(2)).as_bool();
        let px = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(dc);
        if !ok {
            bail!("PrintWindow failed");
        }
        // BGRX → RGBA opaque; an all-black grab means the capture didn't work.
        let mut rgba = px;
        let mut lit = 0usize;
        for p in rgba.as_chunks_mut::<4>().0 {
            p.swap(0, 2);
            p[3] = 255;
            lit += usize::from(p[0] > 8 || p[1] > 8 || p[2] > 8);
        }
        if lit < (w * h) as usize / 100 {
            bail!("capture is black (window not capturable)");
        }
        let img = Pixmap::from_vec(rgba, IntSize::from_wh(w as u32, h as u32).unwrap())
            .context("pixmap")?;
        let mon = Rect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        };
        Ok(place_wallpaper(&img, WallpaperMode::Stretch, 0, mon, work))
    }
}

/// Top-down 32bpp DIB section + memory DC.
unsafe fn dib(
    w: i32,
    h: i32,
) -> Result<(
    windows::Win32::Graphics::Gdi::HDC,
    windows::Win32::Graphics::Gdi::HBITMAP,
    *mut core::ffi::c_void,
)> {
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
    let mut bits = std::ptr::null_mut();
    // SAFETY: caller releases the DC and bitmap.
    unsafe {
        let dc = CreateCompatibleDC(None);
        let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        Ok((dc, bmp, bits))
    }
}

/// Shell icon (no thumbnail) at `size` px for a path or `::{CLSID}`.
pub fn shell_icon(parsing: &str, size: i32) -> Result<Pixmap> {
    // SAFETY: COM + GDI handled inside thumbs; here only the factory call differs.
    unsafe {
        let f: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(parsing), None)?;
        let hbm = f.GetImage(
            windows::Win32::Foundation::SIZE { cx: size, cy: size },
            SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
        )?;
        let rgba = crate::shell::thumbs::hbitmap_to_rgba(hbm)?;
        to_pixmap(&rgba).context("icon pixmap")
    }
}

/// Explorer's icon caption font at `dpi`.
pub fn caption_font(dpi: u32) -> LOGFONTW {
    let mut lf = LOGFONTW::default();
    // SAFETY: LOGFONTW out-param of the right size.
    let _ = unsafe {
        SystemParametersInfoForDpi(
            SPI_GETICONTITLELOGFONT.0,
            size_of::<LOGFONTW>() as u32,
            Some((&mut lf as *mut LOGFONTW).cast()),
            0,
            dpi,
        )
    };
    lf.lfQuality = ANTIALIASED_QUALITY;
    lf
}

pub struct TextJob {
    pub text: String,
    /// Box in frame pixels.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub max_lines: i32,
    pub single_line: bool,
}

/// Renders all captions into one coverage mask (0..255 per pixel), GDI-antialiased.
pub fn text_mask(w: i32, h: i32, font: &LOGFONTW, jobs: &[TextJob]) -> Result<Vec<u8>> {
    // SAFETY: GDI objects created and released here; bits valid while the DIB lives.
    unsafe {
        let (dc, bmp, bits) = dib(w, h)?;
        let old_bmp = SelectObject(dc, HGDIOBJ(bmp.0));
        let hfont = CreateFontIndirectW(font);
        let old_font = SelectObject(dc, HGDIOBJ(hfont.0));
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, windows::Win32::Foundation::COLORREF(0x00FF_FFFF));
        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(dc, &mut tm);
        let line_h = tm.tmHeight;
        for j in jobs {
            let mut buf: Vec<u16> = j.text.encode_utf16().collect();
            let mut rc = RECT {
                left: j.x,
                top: j.y,
                right: j.x + j.w,
                bottom: j.y + line_h * j.max_lines.max(1),
            };
            let fmt = if j.single_line {
                DT_CENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS
            } else {
                DT_CENTER | DT_WORDBREAK | DT_EDITCONTROL | DT_END_ELLIPSIS | DT_NOPREFIX
            };
            DrawTextW(dc, &mut buf, &mut rc, fmt);
        }
        let px = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
        let mask = px
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[0].max(p[1]).max(p[2]))
            .collect();
        SelectObject(dc, old_font);
        SelectObject(dc, old_bmp);
        let _ = DeleteObject(HGDIOBJ(hfont.0));
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(dc);
        Ok(mask)
    }
}

/// Line height of the caption font (for layout of the 2-line box).
pub fn caption_line_height(font: &LOGFONTW) -> i32 {
    (font.lfHeight.abs() as f32 * 1.35).round() as i32
}
