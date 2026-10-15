//! Main (STA) thread: hidden window, tray icon, and a bridge so other threads can run
//! closures here (desktop view access must stay on this thread).

use std::cell::RefCell;
use std::sync::mpsc::{Receiver, Sender, channel, sync_channel};

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, GetMessageW, IDI_APPLICATION, LoadIconW, MF_STRING, MSG, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WM_APP,
    WM_CONTEXTMENU, WM_DESTROY, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::w;

const WM_TRAY: u32 = WM_APP + 1;
const WM_RUN: u32 = WM_APP + 2;
const TRAY_ID: u32 = 1;
const CMD_EXIT: usize = 1;

type Job = Box<dyn FnOnce() + Send>;

/// Handle other threads use to run work on the UI thread.
#[derive(Clone)]
pub struct UiBridge {
    tx: Sender<Job>,
    hwnd: isize,
}

impl UiBridge {
    /// Runs `f` on the UI thread and waits for its result. Call from worker threads only.
    pub fn call<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
        let (rtx, rrx) = sync_channel(1);
        self.tx
            .send(Box::new(move || {
                let _ = rtx.send(f());
            }))
            .map_err(|_| anyhow!("UI thread is gone"))?;
        // SAFETY: posting to our own window; failure only if it was destroyed.
        unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_RUN,
                WPARAM(0),
                LPARAM(0),
            )
        }
        .context("PostMessageW")?;
        rrx.recv().map_err(|_| anyhow!("UI job dropped"))
    }
}

thread_local! {
    static JOBS: RefCell<Option<Receiver<Job>>> = const { RefCell::new(None) };
    static TASKBAR_CREATED: RefCell<u32> = const { RefCell::new(0) };
}

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    };
    // SAFETY: stock icon, no module needed.
    nid.hIcon = unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default();
    let tip: Vec<u16> = "Nook".encode_utf16().collect();
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    nid
}

fn add_tray(hwnd: HWND) {
    // SAFETY: valid NOTIFYICONDATAW for our window.
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &tray_data(hwnd)) }.as_bool() {
        tracing::warn!("tray icon not added (Explorer not ready?)");
    }
}

fn remove_tray(hwnd: HWND) {
    // SAFETY: as above.
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &tray_data(hwnd)) };
}

fn show_menu(hwnd: HWND) {
    // SAFETY: menu created and destroyed here; TPM_RETURNCMD returns the chosen id.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT, w!("Выход"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        if cmd.0 as usize == CMD_EXIT {
            tracing::info!("exit from tray");
            remove_tray(hwnd);
            PostQuitMessage(0);
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_RUN => {
            JOBS.with(|j| {
                if let Some(rx) = j.borrow().as_ref() {
                    while let Ok(job) = rx.try_recv() {
                        job();
                    }
                }
            });
            LRESULT(0)
        }
        WM_TRAY => {
            let event = (lp.0 & 0xffff) as u32;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                show_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_tray(hwnd);
            // SAFETY: plain call.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        m if m != 0 && TASKBAR_CREATED.with(|t| *t.borrow()) == m => {
            tracing::info!("Explorer restarted; re-adding tray icon");
            add_tray(hwnd);
            LRESULT(0)
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

pub struct Ui {
    hwnd: HWND,
}

impl Ui {
    /// Creates the hidden window and tray icon. Call on the STA main thread.
    pub fn create() -> Result<(Self, UiBridge)> {
        // SAFETY: standard window class/window creation on this thread.
        unsafe {
            let hinst = GetModuleHandleW(None)?;
            let class = w!("NookHiddenWindow");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinst.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);
            // Hidden top-level window (not message-only) so it receives the TaskbarCreated broadcast.
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("Nook"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .context("CreateWindowExW")?;
            TASKBAR_CREATED
                .with(|t| *t.borrow_mut() = RegisterWindowMessageW(w!("TaskbarCreated")));
            let (tx, rx) = channel();
            JOBS.with(|j| *j.borrow_mut() = Some(rx));
            add_tray(hwnd);
            Ok((
                Self { hwnd },
                UiBridge {
                    tx,
                    hwnd: hwnd.0 as isize,
                },
            ))
        }
    }

    /// Message loop until "Выход" or WM_QUIT.
    pub fn run(&self) {
        // SAFETY: standard pump on the owning thread.
        unsafe {
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        remove_tray(self.hwnd);
    }
}
