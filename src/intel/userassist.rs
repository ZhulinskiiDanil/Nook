//! UserAssist launch counters (spec §13).
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\UserAssist\{GUID}\Count`:
//! value names are ROT13, may start with a known-folder GUID; data is 72 bytes:
//! run count u32 @4, last run FILETIME @60. Parser tolerates garbage.

use std::time::SystemTime;

use crate::timefmt::filetime_to_system;

pub const GUID_EXE: &str = "{CEBFF5CD-ACE2-4F4F-9178-9926F41749EA}";
pub const GUID_LNK: &str = "{F4E57C4B-2036-45F0-A9AB-443BCFE33D9F}";

/// ROT13 over ASCII letters only (Cyrillic and others pass through).
pub fn rot13(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' => (((c as u8 - b'a') + 13) % 26 + b'a') as char,
            'A'..='Z' => (((c as u8 - b'A') + 13) % 26 + b'A') as char,
            _ => c,
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Decoded name; a leading `{GUID}` is kept for [`resolve_known_folder`].
    pub name: String,
    pub count: u32,
    pub last_run: Option<SystemTime>,
}

/// Parses one value. `None` for too-short data; zero counters are kept.
pub fn parse_value(encoded_name: &str, data: &[u8]) -> Option<Entry> {
    if data.len() < 68 {
        return None;
    }
    let count = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let ft = u64::from_le_bytes(data[60..68].try_into().ok()?);
    Some(Entry {
        name: rot13(encoded_name),
        count,
        last_run: (ft != 0).then(|| filetime_to_system(ft)).flatten(),
    })
}

/// Splits `"{GUID}\rest"` into the GUID and the remainder.
pub fn split_known_folder(name: &str) -> Option<(&str, &str)> {
    if !name.starts_with('{') {
        return None;
    }
    let end = name.find('}')?;
    let rest = name[end + 1..].trim_start_matches('\\');
    Some((&name[..=end], rest))
}

#[cfg(windows)]
pub use reader::read_all;

#[cfg(windows)]
mod reader {
    use super::*;
    use windows::Win32::Foundation::ERROR_NO_MORE_ITEMS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegEnumValueW, RegOpenKeyExW,
    };
    use windows::core::{GUID, HSTRING, PWSTR};

    /// Resolves the `{GUID}` prefix of a known folder; leaves the name as-is otherwise.
    fn resolve_known_folder(name: &str) -> String {
        let Some((guid, rest)) = split_known_folder(name) else {
            return name.to_string();
        };
        let Ok(g) = GUID::try_from(guid.trim_matches(|c| c == '{' || c == '}')) else {
            return name.to_string();
        };
        // SAFETY: buffer freed with CoTaskMemFree.
        unsafe {
            use windows::Win32::System::Com::CoTaskMemFree;
            use windows::Win32::UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath};
            match SHGetKnownFolderPath(&g, KF_FLAG_DEFAULT, None) {
                Ok(p) => {
                    let base = p.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(p.0 as *const _));
                    format!(r"{base}\{rest}")
                }
                Err(_) => name.to_string(),
            }
        }
    }

    /// All entries from both GUIDs, names resolved to paths where possible.
    /// Missing keys (tracking disabled) give an empty list.
    pub fn read_all() -> Vec<Entry> {
        let mut out = Vec::new();
        for g in [GUID_EXE, GUID_LNK] {
            let key =
                format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\UserAssist\{g}\Count");
            let mut hkey = HKEY::default();
            // SAFETY: key handle closed below; buffers sized per call.
            unsafe {
                if RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    &HSTRING::from(key),
                    None,
                    KEY_READ,
                    &mut hkey,
                )
                .is_err()
                {
                    continue;
                }
                let mut name = vec![0u16; 2048];
                let mut data = vec![0u8; 256];
                for i in 0.. {
                    let mut name_len = name.len() as u32;
                    let mut data_len = data.len() as u32;
                    let mut ty = 0u32;
                    let r = RegEnumValueW(
                        hkey,
                        i,
                        Some(PWSTR(name.as_mut_ptr())),
                        &mut name_len,
                        None,
                        Some(&mut ty),
                        Some(data.as_mut_ptr()),
                        Some(&mut data_len),
                    );
                    if r == ERROR_NO_MORE_ITEMS {
                        break;
                    }
                    if r.is_err() {
                        continue;
                    }
                    let n = String::from_utf16_lossy(&name[..name_len as usize]);
                    if let Some(mut e) = parse_value(&n, &data[..data_len as usize]) {
                        e.name = resolve_known_folder(&e.name);
                        out.push(e);
                    }
                }
                let _ = RegCloseKey(hkey);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(count: u32, ft: u64) -> Vec<u8> {
        let mut v = vec![0u8; 72];
        v[4..8].copy_from_slice(&count.to_le_bytes());
        v[60..68].copy_from_slice(&ft.to_le_bytes());
        v
    }

    #[test]
    fn rot13_roundtrip_keeps_cyrillic() {
        assert_eq!(rot13("Fgrnz.yax"), "Steam.lnk");
        assert_eq!(
            rot13(&rot13("Семен КС конфиги.lnk")),
            "Семен КС конфиги.lnk"
        );
        assert_eq!(
            rot13("{N77S5Q77-2R2O-44P3-N6N2-NON601054N51}"),
            "{A77F5D77-2E2B-44C3-A6A2-ABA601054A51}"
        );
    }

    #[test]
    fn parses_fixture() {
        // 2026-09-30T12:01:00Z as FILETIME.
        let ft = 134_352_432_600_000_000u64;
        let e = parse_value("Q:\\Tnzrf\\fgrnz.rkr", &value(42, ft)).unwrap();
        assert_eq!(e.name, "D:\\Games\\steam.exe");
        assert_eq!(e.count, 42);
        assert_eq!(
            crate::timefmt::rfc3339(e.last_run.unwrap()),
            "2026-09-30T12:01:00Z"
        );
    }

    #[test]
    fn tolerates_garbage() {
        assert!(parse_value("x", &[1, 2, 3]).is_none());
        assert!(parse_value("x", &[]).is_none());
        let e = parse_value("URZR_PGYFRFFVBA", &value(0, 0)).unwrap();
        assert_eq!((e.count, e.last_run), (0, None));
        // Huge FILETIME must not panic.
        assert!(parse_value("x", &value(1, u64::MAX)).is_some());
    }

    #[test]
    fn known_folder_prefix() {
        assert_eq!(
            split_known_folder(r"{B4BFCC3A-DB2C-424C-B029-7FE99A87C641}\Steam.lnk"),
            Some(("{B4BFCC3A-DB2C-424C-B029-7FE99A87C641}", "Steam.lnk"))
        );
        assert_eq!(split_known_folder(r"C:\x.exe"), None);
    }
}
