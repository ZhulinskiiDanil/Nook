//! Main (STA) thread: hidden window, tray icon, low-level keyboard hook, preview overlay,
//! and a bridge so other threads can run closures here (desktop view access stays here).
//! Nothing is drawn over the desktop except the preview, and only while the keys are held.

use std::cell::{Cell, RefCell};
use std::sync::mpsc::{Receiver, Sender, channel, sync_channel};

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HGDIOBJ,
    ReleaseDC, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, QUNS_BUSY,
    QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DispatchMessageW, GetCursorPos, GetMessageW, HC_ACTION, HHOOK, HICON,
    HWND_TOPMOST, KBDLLHOOKSTRUCT, KillTimer, LLKHF_INJECTED, MF_CHECKED, MF_SEPARATOR, MF_STRING,
    MSG, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SPI_SETWORKAREA,
    SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow,
    SetTimer, SetWindowPos, SetWindowsHookExW, ShowWindow, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, ULW_ALPHA, UnhookWindowsHookEx,
    UpdateLayeredWindow, WH_KEYBOARD_LL, WINDOW_EX_STYLE, WM_APP, WM_CONTEXTMENU, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_DPICHANGED, WM_KEYDOWN, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_SYSKEYDOWN,
    WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_OVERLAPPED, WS_POPUP,
};
use windows::core::w;

use crate::preview::frame::Frame;
use crate::preview::hotkey::{Cmd, Ctx, Machine};

const WM_TRAY: u32 = WM_APP + 1;
const WM_RUN: u32 = WM_APP + 2;
const WM_HOTKEY_CMD: u32 = WM_APP + 3;
const WM_ARM: u32 = WM_APP + 4;
const TRAY_ID: u32 = 1;
const HOLD_TIMER: usize = 1;

const CMD_UNDO: usize = 1;
const CMD_PAUSE_INBOX: usize = 2;
const CMD_QUARANTINE: usize = 3;
const CMD_CONFIG: usize = 4;
const CMD_LOGS: usize = 5;
const CMD_EXIT: usize = 9;

/// What the UI thread asks the daemon to do (handled off the UI thread).
#[derive(Debug, Clone)]
pub enum UiEvent {
    Apply(String),
    Reject(String),
    Previewed(String),
    UndoLast,
    TogglePauseInbox(bool),
    OpenQuarantine,
    OpenConfig,
    OpenLogs,
    /// Resolution, monitor layout, DPI or work area changed.
    DisplayChanged,
    /// Explorer came back (TaskbarCreated).
    ExplorerRestarted,
}

/// Pending plan as the UI sees it.
#[derive(Debug, Clone, Default)]
pub struct PlanUi {
    pub id: Option<String>,
    pub summary: String,
    pub previewed: bool,
}

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

    /// Updates the pending plan (tray dot, tooltip, hotkey context).
    pub fn set_plan(&self, plan: PlanUi) -> Result<()> {
        self.call(move || set_plan_ui(plan))
    }

    /// Uploads a ready preview frame into the (hidden) overlay so showing is instant.
    pub fn set_frame(&self, frame: Frame) -> Result<()> {
        self.call(move || upload_frame(frame))?
    }
}

/// Marker: Nook hid desktop icons for a live preview. If present at startup (crash), icons are restored.
pub fn icons_marker() -> Option<std::path::PathBuf> {
    crate::paths::AppDirs::resolve()
        .ok()
        .map(|d| d.data_dir.join("icons_hidden.marker"))
}

/// Restores desktop icons if a previous run hid them and died. UI thread.
pub fn recover_hidden_icons() {
    if let Some(m) = icons_marker().filter(|m| m.exists()) {
        tracing::warn!("desktop icons were left hidden by a previous run; restoring");
        if crate::shell::desktop_view::DesktopView::open()
            .and_then(|v| v.set_icons_visible(true))
            .is_ok()
        {
            let _ = std::fs::remove_file(m);
        }
    }
}

struct Live {
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    hid_icons: bool,
}

struct UiState {
    hwnd: HWND,
    overlay: HWND,
    bridge: Option<UiBridge>,
    frame_layer: Option<std::sync::Arc<tiny_skia::Pixmap>>,
    frame_live: bool,
    live: Option<Live>,
    hook: Option<HHOOK>,
    machine: Machine,
    plan: PlanUi,
    frame_plan: Option<String>,
    suppressed: bool,
    pause_inbox: bool,
    accent: tiny_skia::Color,
    icon: Option<HICON>,
    events: Sender<UiEvent>,
}

thread_local! {
    static JOBS: RefCell<Option<Receiver<Job>>> = const { RefCell::new(None) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
    static STATE: RefCell<Option<UiState>> = const { RefCell::new(None) };
}

fn with_state<T>(f: impl FnOnce(&mut UiState) -> T) -> Option<T> {
    STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn ctx(s: &UiState) -> Ctx {
    Ctx {
        has_plan: s.plan.id.is_some(),
        previewed: s.plan.previewed,
        suppressed: s.suppressed,
    }
}

fn fullscreen_now() -> bool {
    // SAFETY: plain call.
    matches!(
        unsafe { SHQueryUserNotificationState() },
        Ok(QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE)
    )
}

// ---- tray ----

fn tray_data(s: &UiState) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: s.hwnd,
        uID: TRAY_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: s.icon.unwrap_or_default(),
        ..Default::default()
    };
    let tip = match &s.plan.id {
        Some(_) => format!("Nook — план ждёт: {}. Зажми LCtrl+LAlt", s.plan.summary),
        None => "Nook".to_string(),
    };
    let tip: Vec<u16> = tip.encode_utf16().take(nid.szTip.len() - 1).collect();
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    nid
}

fn refresh_icon(s: &mut UiState) {
    let pm = super::tray_icon::draw(32, s.plan.id.is_some(), s.accent);
    if let Ok(icon) = super::tray_icon::to_hicon(&pm)
        && let Some(old) = s.icon.replace(icon)
    {
        // SAFETY: we created it.
        let _ = unsafe { DestroyIcon(old) };
    }
}

fn tray(s: &UiState, msg: windows::Win32::UI::Shell::NOTIFY_ICON_MESSAGE) {
    // SAFETY: valid NOTIFYICONDATAW for our window.
    let ok = unsafe { Shell_NotifyIconW(msg, &tray_data(s)) }.as_bool();
    if !ok && msg == NIM_ADD {
        tracing::warn!("tray icon not added (Explorer not ready?)");
    }
}

fn set_plan_ui(plan: PlanUi) {
    with_state(|s| {
        let gone = plan.id.is_none() || plan.id != s.plan.id;
        s.plan = plan;
        if gone && s.machine.showing() {
            for c in s.machine.force_hide() {
                handle_cmd(s, c);
            }
        }
        refresh_icon(s);
        tray(s, NIM_MODIFY);
    });
}

fn show_menu(hwnd: HWND) {
    let pause = with_state(|s| s.pause_inbox).unwrap_or(false);
    // SAFETY: menu created and destroyed here; TPM_RETURNCMD returns the chosen id.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, CMD_UNDO, w!("Отменить последнее"));
        let flags = if pause {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let _ = AppendMenuW(menu, flags, CMD_PAUSE_INBOX, w!("Пауза входящих"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, CMD_QUARANTINE, w!("Открыть карантин"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_CONFIG, w!("Открыть конфиг"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_LOGS, w!("Логи"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
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
        let ev = match cmd.0 as usize {
            CMD_UNDO => Some(UiEvent::UndoLast),
            CMD_PAUSE_INBOX => with_state(|s| {
                s.pause_inbox = !s.pause_inbox;
                s.pause_inbox
            })
            .map(UiEvent::TogglePauseInbox),
            CMD_QUARANTINE => Some(UiEvent::OpenQuarantine),
            CMD_CONFIG => Some(UiEvent::OpenConfig),
            CMD_LOGS => Some(UiEvent::OpenLogs),
            CMD_EXIT => {
                tracing::info!("exit from tray");
                PostQuitMessage(0);
                None
            }
            _ => None,
        };
        if let Some(ev) = ev {
            with_state(|s| s.events.send(ev).ok());
        }
    }
}

// ---- preview overlay ----

fn upload_frame(frame: Frame) -> Result<()> {
    upload_bgra(frame.width, frame.height, &frame.bgra)?;
    with_state(|s| {
        s.frame_plan = Some(frame.plan_id.clone());
        s.frame_layer = Some(frame.layer.clone());
        s.frame_live = frame.live_wallpaper;
    });
    tracing::info!(plan = %frame.plan_id, ms = frame.build_ms as u64, live = frame.live_wallpaper, "preview frame ready");
    Ok(())
}

/// Starts the live wallpaper loop: hide desktop icons under the (already visible) overlay and
/// keep compositing fresh Wallpaper Engine frames under the icon layer.
fn start_live(s: &mut UiState) {
    let (Some(layer), Some(bridge)) = (s.frame_layer.clone(), s.bridge.clone()) else {
        return;
    };
    let Some(src) = crate::preview::sources::wallpaper_engine_layer() else {
        return;
    };
    let view = crate::shell::desktop_view::DesktopView::open();
    let shown = view
        .as_ref()
        .ok()
        .and_then(|v| v.info().ok())
        .is_some_and(|i| i.flags.icons_shown);
    let mut hid_icons = false;
    if shown && let Ok(v) = &view {
        if let Some(m) = icons_marker() {
            let _ = std::fs::write(m, b"nook");
        }
        hid_icons = v.set_icons_visible(false).is_ok();
    }
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = running.clone();
    let src_raw = src.0 as isize;
    let _ = std::thread::Builder::new()
        .name("live-wallpaper".into())
        .spawn(move || {
            use std::sync::atomic::Ordering;
            let Ok(mon) = crate::platform::PrimaryMonitor::query() else {
                return;
            };
            let mut first = true;
            while flag.load(Ordering::Relaxed) {
                let t = std::time::Instant::now();
                if !first || hid_icons {
                    // Give Explorer a moment to actually remove the icons before the first grab.
                    if first {
                        std::thread::sleep(std::time::Duration::from_millis(60));
                    }
                    match crate::preview::sources::capture_window(HWND(src_raw as *mut _), mon.work)
                    {
                        Ok(bg) => {
                            let bg = std::sync::Arc::new(bg);
                            if hid_icons {
                                crate::preview::frame::remember_clean(bg.clone());
                            }
                            let pm = crate::preview::render::flatten(&bg, &layer);
                            let bgra = crate::preview::render::to_bgra(&pm);
                            let (w, h) = (pm.width(), pm.height());
                            if !flag.load(Ordering::Relaxed)
                                || bridge.call(move || upload_bgra(w, h, &bgra)).is_err()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("live wallpaper capture: {e:#}");
                            break;
                        }
                    }
                }
                first = false;
                // ~12 fps is plenty for a peek and keeps CPU low.
                std::thread::sleep(
                    std::time::Duration::from_millis(80).saturating_sub(t.elapsed()),
                );
            }
        });
    s.live = Some(Live { running, hid_icons });
}

fn stop_live(s: &mut UiState) {
    if let Some(live) = s.live.take() {
        live.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if live.hid_icons {
            // Icons first, then the overlay goes away: the user never sees an empty desktop.
            match crate::shell::desktop_view::DesktopView::open()
                .and_then(|v| v.set_icons_visible(true))
            {
                Ok(()) => {
                    if let Some(m) = icons_marker() {
                        let _ = std::fs::remove_file(m);
                    }
                }
                Err(e) => tracing::error!("could not restore desktop icons: {e:#}"),
            }
        }
    }
}

fn upload_bgra(w: u32, h: u32, bgra: &[u8]) -> Result<()> {
    let overlay = with_state(|s| s.overlay).context("ui not ready")?;
    let (w, h) = (w as i32, h as i32);
    let mon = crate::platform::PrimaryMonitor::query()?;
    // SAFETY: GDI objects are created and released here; UpdateLayeredWindow copies the bits.
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
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
        let bmp = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        std::ptr::copy_nonoverlapping(
            bgra.as_ptr(),
            bits as *mut u8,
            bgra.len().min((w * h * 4) as usize),
        );
        let old = SelectObject(mem, HGDIOBJ(bmp.0));
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
            ..Default::default()
        };
        let dst = POINT {
            x: mon.work.left,
            y: mon.work.top,
        };
        let size = SIZE { cx: w, cy: h };
        let src = POINT::default();
        let r = UpdateLayeredWindow(
            overlay,
            Some(screen),
            Some(&dst),
            Some(&size),
            Some(mem),
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        r.context("UpdateLayeredWindow")?;
    }
    Ok(())
}

fn handle_cmd(s: &mut UiState, c: Cmd) {
    match c {
        Cmd::Show => {
            let ready = s.plan.id.is_some() && s.frame_plan == s.plan.id;
            if !ready {
                tracing::info!("preview requested but the frame is still being built");
                return;
            }
            // SAFETY: our overlay window.
            unsafe {
                let _ = SetWindowPos(
                    s.overlay,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                let _ = ShowWindow(s.overlay, SW_SHOWNOACTIVATE);
            }
            if s.frame_live {
                start_live(s);
            }
            if !s.plan.previewed
                && let Some(id) = &s.plan.id
            {
                s.plan.previewed = true;
                let _ = s.events.send(UiEvent::Previewed(id.clone()));
            }
        }
        Cmd::Hide => {
            stop_live(s);
            // SAFETY: our overlay window.
            let _ = unsafe { ShowWindow(s.overlay, SW_HIDE) };
        }
        Cmd::Apply => {
            if let Some(id) = s.plan.id.clone() {
                tracing::info!(plan = %id, "apply from preview");
                let _ = s.events.send(UiEvent::Apply(id));
            }
        }
        Cmd::Reject => {
            if let Some(id) = s.plan.id.clone() {
                tracing::info!(plan = %id, "rejected from preview");
                let _ = s.events.send(UiEvent::Reject(id));
            }
        }
    }
}

// ---- keyboard hook: minimal work, post everything ----

extern "system" fn keyboard_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for WH_KEYBOARD_LL lParam points at a KBDLLHOOKSTRUCT.
        let kb = unsafe { &*(lp.0 as *const KBDLLHOOKSTRUCT) };
        if kb.flags.0 & LLKHF_INJECTED.0 == 0 {
            let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let r = with_state(|s| {
                let (cmds, swallow) = s.machine.on_key(kb.vkCode, down, kb.time, ctx(s));
                for c in cmds {
                    // SAFETY: posting to our own window.
                    let _ = unsafe {
                        PostMessageW(Some(s.hwnd), WM_HOTKEY_CMD, WPARAM(c as usize), LPARAM(0))
                    };
                }
                if s.machine.combo_active() {
                    // SAFETY: as above.
                    let _ = unsafe { PostMessageW(Some(s.hwnd), WM_ARM, WPARAM(0), LPARAM(0)) };
                }
                swallow
            });
            if r == Some(true) {
                return LRESULT(1);
            }
        }
    }
    // SAFETY: pass the event on.
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

fn cmd_from(w: usize) -> Cmd {
    match w {
        x if x == Cmd::Show as usize => Cmd::Show,
        x if x == Cmd::Hide as usize => Cmd::Hide,
        x if x == Cmd::Apply as usize => Cmd::Apply,
        _ => Cmd::Reject,
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_RUN => {
            JOBS.with(|j| {
                let jobs: Vec<Job> = j
                    .borrow()
                    .as_ref()
                    .map(|rx| rx.try_iter().collect())
                    .unwrap_or_default();
                for job in jobs {
                    job();
                }
            });
            LRESULT(0)
        }
        WM_HOTKEY_CMD => {
            with_state(|s| handle_cmd(s, cmd_from(wp.0)));
            LRESULT(0)
        }
        WM_ARM => {
            with_state(|s| {
                s.suppressed = fullscreen_now();
                // SAFETY: our window.
                unsafe { SetTimer(Some(s.hwnd), HOLD_TIMER, 20, None) };
            });
            LRESULT(0)
        }
        WM_TIMER if wp.0 == HOLD_TIMER => {
            with_state(|s| {
                // SAFETY: plain call.
                let now = unsafe { GetTickCount() };
                let c = ctx(s);
                for cmd in s.machine.on_tick(now, c) {
                    handle_cmd(s, cmd);
                }
                if !s.machine.combo_active() {
                    // SAFETY: our timer.
                    let _ = unsafe { KillTimer(Some(s.hwnd), HOLD_TIMER) };
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
            with_state(|s| tray(s, NIM_DELETE));
            // SAFETY: plain call.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        m if m != 0 && TASKBAR_CREATED.get() == m => {
            tracing::info!("Explorer restarted; re-adding tray icon");
            with_state(|s| {
                tray(s, NIM_ADD);
                let _ = s.events.send(UiEvent::ExplorerRestarted);
            });
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            with_state(|s| s.events.send(UiEvent::DisplayChanged).ok());
            // SAFETY: default handling.
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_SETTINGCHANGE if wp.0 as u32 == SPI_SETWORKAREA.0 => {
            // Taskbar moved/resized: the work area (and grid) changed.
            with_state(|s| s.events.send(UiEvent::DisplayChanged).ok());
            LRESULT(0)
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: default handling; the overlay is click-through and never active.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

pub struct Ui;

impl Ui {
    /// Creates the hidden window, overlay, tray icon and keyboard hook. Call on the STA main thread.
    pub fn create(hold_ms: u32, accent: &str, events: Sender<UiEvent>) -> Result<(Self, UiBridge)> {
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

            let oclass = w!("NookPreview");
            let owc = WNDCLASSW {
                lpfnWndProc: Some(overlay_proc),
                hInstance: hinst.into(),
                lpszClassName: oclass,
                ..Default::default()
            };
            RegisterClassW(&owc);
            let mon = crate::platform::PrimaryMonitor::query()?;
            let overlay = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOPMOST,
                oclass,
                w!("Nook preview"),
                WS_POPUP,
                mon.work.left,
                mon.work.top,
                mon.work.width(),
                mon.work.height(),
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .context("create overlay")?;

            TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
            let (tx, rx) = channel();
            JOBS.with(|j| *j.borrow_mut() = Some(rx));
            let hook =
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), Some(hinst.into()), 0)
                    .map_err(|e| tracing::error!("keyboard hook failed: {e}"))
                    .ok();
            STATE.with(|s| {
                *s.borrow_mut() = Some(UiState {
                    hwnd,
                    overlay,
                    bridge: None,
                    frame_layer: None,
                    frame_live: false,
                    live: None,
                    hook,
                    machine: Machine::new(hold_ms),
                    plan: PlanUi::default(),
                    frame_plan: None,
                    suppressed: false,
                    pause_inbox: false,
                    accent: crate::preview::render::parse_hex(accent),
                    icon: None,
                    events,
                })
            });
            let bridge = UiBridge {
                tx,
                hwnd: hwnd.0 as isize,
            };
            with_state(|s| {
                s.bridge = Some(bridge.clone());
                refresh_icon(s);
                tray(s, NIM_ADD);
            });
            recover_hidden_icons();
            Ok((Self, bridge))
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
        with_state(|s| {
            stop_live(s);
            tray(s, NIM_DELETE);
            if let Some(h) = s.hook.take() {
                // SAFETY: our hook.
                let _ = unsafe { UnhookWindowsHookEx(h) };
            }
        });
    }
}
