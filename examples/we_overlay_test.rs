//! Debug spike: does PrintWindow(WorkerW) include a topmost layered window above it?
//! Shows a red full-screen click-through window for ~300 ms, captures, reports. Read-only.

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    HGDIOBJ, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumChildWindows,
    FindWindowW, GetClassNameW, GetParent, GetWindowRect, LWA_ALPHA, MSG, PM_REMOVE, PeekMessageW,
    RegisterClassW, SW_SHOWNOACTIVATE, SetLayeredWindowAttributes, ShowWindow, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{BOOL, w};

static mut FOUND: Option<HWND> = None;
extern "system" fn find(h: HWND, _: LPARAM) -> BOOL {
    let mut b = [0u16; 64];
    let n = unsafe { GetClassNameW(h, &mut b) };
    if String::from_utf16_lossy(&b[..n as usize]) == "WPEDesktopDX11Window" {
        unsafe { FOUND = Some(h) };
        return false.into();
    }
    true.into()
}

extern "system" fn proc_(
    h: HWND,
    m: u32,
    w: windows::Win32::Foundation::WPARAM,
    l: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    unsafe { DefWindowProcW(h, m, w, l) }
}

fn red_share(h: HWND) -> f64 {
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
        SelectObject(mem, HGDIOBJ(bmp.0));
        let _ = PrintWindow(h, mem, PRINT_WINDOW_FLAGS(2));
        let px = std::slice::from_raw_parts(bits as *const u8, (w * hh * 4) as usize);
        let red = px
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[2] > 200 && p[1] < 40 && p[0] < 40)
            .count();
        red as f64 * 100.0 / (w * hh) as f64
    }
}

fn main() {
    unsafe {
        let progman = FindWindowW(w!("Progman"), None).unwrap();
        let _ = EnumChildWindows(Some(progman), Some(find), LPARAM(0));
        let worker = GetParent(FOUND.expect("no Wallpaper Engine")).unwrap();
        println!("red before overlay: {:.1}%", red_share(worker));

        let hinst = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc_),
            hInstance: hinst.into(),
            lpszClassName: w!("NookRedTest"),
            hbrBackground: windows::Win32::Graphics::Gdi::CreateSolidBrush(COLORREF(0x0000FF)),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let red = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            w!("NookRedTest"),
            w!(""),
            WS_POPUP,
            0,
            0,
            2560,
            1440,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .unwrap();
        SetLayeredWindowAttributes(red, COLORREF(0), 255, LWA_ALPHA).unwrap();
        let _ = ShowWindow(red, SW_SHOWNOACTIVATE);
        let t = std::time::Instant::now();
        let mut msg = MSG::default();
        while t.elapsed().as_millis() < 250 {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        println!("red with overlay:   {:.1}%", red_share(worker));
        let _ = DestroyWindow(red);
    }
}
