//! Debug: prints the desktop window tree (Progman / WorkerW and children). Read-only.

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClassNameW, GetParent, GetWindowRect,
    GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::BOOL;

fn class(h: HWND) -> String {
    let mut b = [0u16; 64];
    let n = unsafe { GetClassNameW(h, &mut b) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

fn info(h: HWND) -> String {
    let mut pid = 0;
    let mut r = RECT::default();
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut pid));
        let _ = GetWindowRect(h, &mut r);
    }
    let parent = unsafe { GetParent(h) }.map(class).unwrap_or_default();
    format!(
        "{} vis={} pid={pid} parent={parent} rect={}x{}@{},{}",
        class(h),
        unsafe { IsWindowVisible(h) }.as_bool(),
        r.right - r.left,
        r.bottom - r.top,
        r.left,
        r.top
    )
}

extern "system" fn child(h: HWND, _: LPARAM) -> BOOL {
    println!("    {}", info(h));
    true.into()
}

extern "system" fn top(h: HWND, _: LPARAM) -> BOOL {
    let c = class(h);
    if c == "Progman" || c == "WorkerW" {
        println!("TOP {}", info(h));
        unsafe {
            let _ = EnumChildWindows(Some(h), Some(child), LPARAM(0));
        }
    }
    true.into()
}

fn main() {
    unsafe {
        let _ = EnumWindows(Some(top), LPARAM(0));
    }
}
