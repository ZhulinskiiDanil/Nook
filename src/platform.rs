//! Process-level Win32 plumbing: DPI, COM apartments, single instance, user SID,
//! Windows version, primary monitor.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, LocalFree, POINT, RECT,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

/// Opts the process into Per-Monitor-V2 DPI awareness. Must run before any window is created.
pub fn enable_per_monitor_dpi() -> Result<()> {
    // SAFETY: plain Win32 call with a constant argument.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        .context("SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)")
}

/// STA COM apartment for the current thread; uninitialised on drop.
pub struct ComApartment(std::marker::PhantomData<*const ()>);

impl ComApartment {
    pub fn sta() -> Result<Self> {
        // SAFETY: balanced by CoUninitialize in Drop on the same thread (the raw-pointer marker keeps it !Send).
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }
            .ok()
            .context("CoInitializeEx(STA)")?;
        Ok(Self(std::marker::PhantomData))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: paired with the successful CoInitializeEx in `sta`.
        unsafe { CoUninitialize() };
    }
}

/// Current user's SID as a string (`S-1-5-21-...`).
pub fn current_user_sid() -> Result<String> {
    // SAFETY: token handle is closed; buffers are sized per GetTokenInformation; the
    // string SID is freed with LocalFree.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .context("OpenProcessToken")?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let res = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        );
        let _ = CloseHandle(token);
        res.context("GetTokenInformation(TokenUser)")?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s).context("ConvertSidToStringSidW")?;
        let out = s.to_string();
        LocalFree(Some(HLOCAL(s.0.cast())));
        Ok(out?)
    }
}

/// Named mutex held for the lifetime of the daemon.
pub struct SingleInstance(HANDLE);

impl SingleInstance {
    /// `Ok(None)` when another instance already holds the mutex.
    pub fn acquire(name: &str) -> Result<Option<Self>> {
        // SAFETY: CreateMutexW with a valid name; handle closed on drop or immediately if not ours.
        unsafe {
            let h = CreateMutexW(None, true, &HSTRING::from(name)).context("CreateMutexW")?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(h);
                return Ok(None);
            }
            Ok(Some(Self(h)))
        }
    }

    pub fn daemon_mutex_name() -> Result<String> {
        Ok(format!(r"Local\Nook-{}", current_user_sid()?))
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn reg_string(key: &str, value: &str) -> Option<String> {
    let mut buf = [0u16; 256];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: buffer and size describe the same allocation.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(key),
            &HSTRING::from(value),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    let len = (size as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn reg_dword_hklm(key: &str, value: &str) -> Option<u32> {
    let mut v = 0u32;
    let mut size = 4u32;
    // SAFETY: v is a 4-byte buffer.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(key),
            &HSTRING::from(value),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut v as *mut u32).cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    Some(v)
}

#[derive(Debug, Clone, Serialize)]
pub struct WindowsVersion {
    pub product: String,
    pub edition: String,
    pub display_version: String,
    pub build: u32,
    pub ubr: u32,
}

impl WindowsVersion {
    pub fn query() -> Result<Self> {
        const KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
        let build: u32 = reg_string(KEY, "CurrentBuildNumber")
            .and_then(|s| s.parse().ok())
            .context("CurrentBuildNumber")?;
        // ProductName still says "Windows 10" on Windows 11; derive from build.
        let product = if build >= 22000 {
            "Windows 11"
        } else {
            "Windows 10"
        }
        .to_string();
        Ok(Self {
            product,
            edition: reg_string(KEY, "EditionID").unwrap_or_default(),
            display_version: reg_string(KEY, "DisplayVersion").unwrap_or_default(),
            build,
            ubr: reg_dword_hklm(KEY, "UBR").unwrap_or(0),
        })
    }
}

impl std::fmt::Display for WindowsVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} {} (build {}.{})",
            self.product, self.edition, self.display_version, self.build, self.ubr
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

impl From<RECT> for Rect {
    fn from(r: RECT) -> Self {
        Self {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
}

impl std::fmt::Display for Rect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}x{} @ ({}, {})",
            self.width(),
            self.height(),
            self.left,
            self.top
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PrimaryMonitor {
    pub monitor: Rect,
    pub work: Rect,
    pub dpi: u32,
    pub scale_percent: u32,
}

impl PrimaryMonitor {
    /// Requires the process to be Per-Monitor-V2 aware to get physical pixels.
    pub fn query() -> Result<Self> {
        // SAFETY: MONITORINFO is sized correctly; HMONITOR from MonitorFromPoint is valid.
        unsafe {
            let hmon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let mut mi = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
                bail!("GetMonitorInfoW failed");
            }
            let (mut dx, mut dy) = (0u32, 0u32);
            GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy)
                .context("GetDpiForMonitor")?;
            Ok(Self {
                monitor: mi.rcMonitor.into(),
                work: mi.rcWork.into(),
                dpi: dx,
                scale_percent: dx * 100 / 96,
            })
        }
    }
}

/// Null-terminated UTF-16 for APIs taking `PCWSTR`. Keep the Vec alive while the pointer is used.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn pcwstr(v: &[u16]) -> PCWSTR {
    PCWSTR(v.as_ptr())
}
