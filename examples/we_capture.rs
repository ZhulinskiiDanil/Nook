//! Debug spike: can we grab the Wallpaper Engine layer without desktop icons? Read-only.
//! Tries PrintWindow(PW_RENDERFULLCONTENT) and BitBlt on the WE window and its WorkerW parent.

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, GetDC, HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, FindWindowW, GetClassNameW, GetParent, GetWindowRect,
};
use windows::core::{BOOL, w};

fn class(h: HWND) -> String {
    let mut b = [0u16; 64];
    let n = unsafe { GetClassNameW(h, &mut b) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

static mut FOUND: Option<HWND> = None;
extern "system" fn find(h: HWND, _: LPARAM) -> BOOL {
    if class(h) == "WPEDesktopDX11Window" {
        unsafe { FOUND = Some(h) };
        return false.into();
    }
    true.into()
}

fn grab(h: HWND, how: &str) -> (usize, usize) {
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r).unwrap() };
    let (w, hh) = (r.right - r.left, r.bottom - r.top);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -hh,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    unsafe {
        let mut bits = std::ptr::null_mut();
        let mem = CreateCompatibleDC(None);
        let bmp = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
        let old = SelectObject(mem, HGDIOBJ(bmp.0));
        match how {
            "print2" => {
                let _ = PrintWindow(h, mem, PRINT_WINDOW_FLAGS(2));
            }
            "print0" => {
                let _ = PrintWindow(h, mem, PRINT_WINDOW_FLAGS(0));
            }
            _ => {
                let src = GetDC(Some(h));
                let _ = BitBlt(mem, 0, 0, w, hh, Some(src), 0, 0, SRCCOPY);
                ReleaseDC(Some(h), src);
            }
        }
        let px = std::slice::from_raw_parts(bits as *const u8, (w * hh * 4) as usize);
        let lit = px
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] > 8 || p[1] > 8 || p[2] > 8)
            .count();
        if how == "print2" && class(h) == "WorkerW" {
            let mut rgba = px.to_vec();
            for p in rgba.as_chunks_mut::<4>().0 {
                p.swap(0, 2);
                p[3] = 255;
            }
            let pm = tiny_skia::Pixmap::from_vec(
                rgba,
                tiny_skia::IntSize::from_wh(w as u32, hh as u32).unwrap(),
            )
            .unwrap();
            pm.save_png(std::env::args().nth(1).unwrap_or("we.png".into()))
                .unwrap();
        }
        SelectObject(mem, old);
        let _ = DeleteDC(mem);
        (lit, (w * hh) as usize)
    }
}

fn main() {
    unsafe {
        let progman = FindWindowW(w!("Progman"), None).unwrap();
        let _ = EnumChildWindows(Some(progman), Some(find), LPARAM(0));
        let Some(we) = FOUND else {
            println!("Wallpaper Engine window not found");
            return;
        };
        let worker = GetParent(we).unwrap();
        for (name, h) in [("WPEDesktopDX11Window", we), ("WorkerW", worker)] {
            for how in ["print2", "print0", "bitblt"] {
                let (lit, all) = grab(h, how);
                println!(
                    "{name:22} {how:7} lit {:5.1}%",
                    lit as f64 * 100.0 / all as f64
                );
            }
        }
    }
}
