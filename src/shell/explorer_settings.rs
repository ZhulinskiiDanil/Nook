//! Explorer settings under `HKCU\...\Explorer\Advanced` and desktop virtual-icon visibility.
//! Read-only in M0.

use serde::Serialize;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::core::HSTRING;

const ADVANCED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced";
const HIDE_DESKTOP_ICONS: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel";

pub fn hkcu_dword(key: &str, value: &str) -> Option<u32> {
    let mut v = 0u32;
    let mut size = 4u32;
    // SAFETY: v is a 4-byte buffer matching `size`.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
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

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ExplorerSettings {
    /// `Hidden`: 1 = show hidden files, 2 = don't. Required for `dim`.
    pub hidden: Option<u32>,
    /// `ShowSuperHidden`: 1 = show protected OS files (e.g. desktop.ini).
    pub show_super_hidden: Option<u32>,
    /// `HideFileExt`: 1 = hide known extensions.
    pub hide_file_ext: Option<u32>,
}

impl ExplorerSettings {
    pub fn read() -> Self {
        Self {
            hidden: hkcu_dword(ADVANCED, "Hidden"),
            show_super_hidden: hkcu_dword(ADVANCED, "ShowSuperHidden"),
            hide_file_ext: hkcu_dword(ADVANCED, "HideFileExt"),
        }
    }

    pub fn shows_hidden(&self) -> bool {
        self.hidden == Some(1)
    }

    pub fn shows_super_hidden(&self) -> bool {
        self.show_super_hidden == Some(1)
    }
}

/// Windows "Track app launches" (privacy setting). Missing value means enabled.
pub fn launch_tracking_enabled() -> bool {
    hkcu_dword(ADVANCED, "Start_TrackProgs") != Some(0)
}

/// Virtual desktop icons Nook knows about.
pub const VIRTUAL_ICONS: &[(&str, &str, bool)] = &[
    // (name, CLSID, shown by default)
    (
        "Recycle Bin",
        "{645FF040-5081-101B-9F08-00AA002F954E}",
        true,
    ),
    ("This PC", "{20D04FE0-3AEA-1069-A2D8-08002B30309D}", false),
    (
        "User's Files",
        "{59031a47-3f72-44a7-89c5-5595fe6b30ee}",
        false,
    ),
    ("Network", "{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}", false),
    (
        "Control Panel",
        "{5399E694-6CE5-4D6C-8FCE-1D8870FDCBA0}",
        false,
    ),
];

/// Virtual icons currently enabled on the desktop (value 0 = shown, 1 = hidden).
pub fn visible_virtual_icons() -> Vec<&'static str> {
    VIRTUAL_ICONS
        .iter()
        .filter(|(_, clsid, default)| {
            hkcu_dword(HIDE_DESKTOP_ICONS, clsid).map_or(*default, |v| v == 0)
        })
        .map(|(name, _, _)| *name)
        .collect()
}
