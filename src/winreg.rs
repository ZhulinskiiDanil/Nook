//! Minimal read-only registry helpers (W APIs, UTF-16).

use windows::Win32::Foundation::ERROR_NO_MORE_ITEMS;
use windows::Win32::System::Registry::{
    HKEY, KEY_READ, KEY_WOW64_64KEY, REG_SAM_FLAGS, RRF_RT_REG_DWORD, RRF_RT_REG_EXPAND_SZ,
    RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};
use windows::core::{HSTRING, PWSTR};

/// An open registry key, closed on drop.
pub struct Key(HKEY);

impl Key {
    pub fn open(root: HKEY, path: &str) -> Option<Self> {
        Self::open_sam(root, path, KEY_READ | KEY_WOW64_64KEY)
    }

    pub fn open_sam(root: HKEY, path: &str, sam: REG_SAM_FLAGS) -> Option<Self> {
        let mut h = HKEY::default();
        // SAFETY: out-param is a local; handle closed on drop.
        unsafe { RegOpenKeyExW(root, &HSTRING::from(path), None, sam, &mut h) }
            .ok()
            .ok()?;
        Some(Self(h))
    }

    pub fn subkeys(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = [0u16; 512];
        for i in 0.. {
            let mut len = buf.len() as u32;
            // SAFETY: buffer/len describe the same allocation.
            let r = unsafe {
                RegEnumKeyExW(
                    self.0,
                    i,
                    Some(PWSTR(buf.as_mut_ptr())),
                    &mut len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if r == ERROR_NO_MORE_ITEMS {
                break;
            }
            if r.is_ok() {
                out.push(String::from_utf16_lossy(&buf[..len as usize]));
            }
        }
        out
    }

    /// REG_SZ / REG_EXPAND_SZ (expanded) value of `subkey` (use "" for this key).
    pub fn string(&self, subkey: &str, value: &str) -> Option<String> {
        let (sk, v) = (HSTRING::from(subkey), HSTRING::from(value));
        let mut size = 0u32;
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
        // SAFETY: size query first, then a buffer of that size.
        unsafe {
            RegGetValueW(self.0, &sk, &v, flags, None, None, Some(&mut size))
                .ok()
                .ok()?;
            let mut buf = vec![0u16; (size as usize).div_ceil(2)];
            RegGetValueW(
                self.0,
                &sk,
                &v,
                flags,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
            .ok()
            .ok()?;
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..len]))
        }
    }

    pub fn dword(&self, subkey: &str, value: &str) -> Option<u32> {
        let mut v = 0u32;
        let mut size = 4u32;
        // SAFETY: 4-byte buffer.
        unsafe {
            RegGetValueW(
                self.0,
                &HSTRING::from(subkey),
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
}

/// Sets an HKCU DWORD (creates the key if needed).
pub fn hkcu_set_dword(subkey: &str, value: &str, data: u32) -> anyhow::Result<()> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_DWORD, RegSetKeyValueW};
    // SAFETY: 4-byte buffer.
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(subkey),
            &HSTRING::from(value),
            REG_DWORD.0,
            Some((&data as *const u32).cast()),
            4,
        )
    };
    r.ok()
        .map_err(|e| anyhow::anyhow!("RegSetKeyValueW {subkey}\\{value}: {e}"))
}

/// Removes an HKCU value that Nook itself created (restoring "not set").
pub fn hkcu_remove_value(subkey: &str, value: &str) -> anyhow::Result<()> {
    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteKeyValueW};
    // SAFETY: plain call.
    let r = unsafe {
        RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(subkey),
            &HSTRING::from(value),
        )
    };
    if r.is_ok() || r == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "RegDeleteKeyValueW {subkey}\\{value}: {r:?}"
        ))
    }
}

/// Broadcasts `WM_SETTINGCHANGE("ShellState")` and `SHCNE_ASSOCCHANGED` so Explorer re-reads settings.
pub fn broadcast_shell_settings() {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };
    let s = HSTRING::from("ShellState");
    // SAFETY: broadcast with a valid string pointer; bounded by the timeout.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(s.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            1000,
            None,
        );
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// Sets an HKCU REG_SZ value (creates the key if needed).
pub fn hkcu_set_string(subkey: &str, value: &str, data: &str) -> anyhow::Result<()> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
    let w: Vec<u16> = data.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: buffer of w.len()*2 bytes including the terminator.
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(subkey),
            &HSTRING::from(value),
            REG_SZ.0,
            Some(w.as_ptr().cast()),
            (w.len() * 2) as u32,
        )
    };
    r.ok()
        .map_err(|e| anyhow::anyhow!("RegSetKeyValueW {value}: {e}"))
}
