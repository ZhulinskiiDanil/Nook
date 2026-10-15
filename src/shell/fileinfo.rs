//! Per-file Win32 facts: NTFS FileId and PE version strings. Read-only.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use windows::Win32::Foundation::{CloseHandle, GENERIC_ACCESS_RIGHTS};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo,
    GetFileInformationByHandleEx, GetFileVersionInfoSizeW, GetFileVersionInfoW, OPEN_EXISTING,
    VerQueryValueW,
};
use windows::core::HSTRING;

/// Volume serial + 128-bit file id. Survives renames and same-volume moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct FileId {
    pub volume: u64,
    pub id: [u8; 16],
}

impl FileId {
    pub fn to_bytes(self) -> [u8; 24] {
        let mut b = [0u8; 24];
        b[..8].copy_from_slice(&self.volume.to_le_bytes());
        b[8..].copy_from_slice(&self.id);
        b
    }
}

/// Opens with minimal rights and full sharing (never blocks other apps), reads FileIdInfo.
pub fn file_id(path: &Path) -> Result<FileId> {
    // SAFETY: handle closed before return; FILE_ID_INFO is sized correctly.
    unsafe {
        let h = CreateFileW(
            &HSTRING::from(path.as_os_str()),
            GENERIC_ACCESS_RIGHTS(FILE_READ_ATTRIBUTES.0).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
        .with_context(|| format!("open {}", path.display()))?;
        let mut info = FILE_ID_INFO::default();
        let r = GetFileInformationByHandleEx(
            h,
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        );
        let _ = CloseHandle(h);
        r.context("GetFileInformationByHandleEx(FileIdInfo)")?;
        Ok(FileId {
            volume: info.VolumeSerialNumber,
            id: info.FileId.Identifier,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct VersionStrings {
    pub file_description: Option<String>,
    pub product_name: Option<String>,
    pub company_name: Option<String>,
    pub original_filename: Option<String>,
}

impl VersionStrings {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// PE metadata that marks an installer.
    pub fn looks_like_installer(&self) -> bool {
        [
            &self.file_description,
            &self.original_filename,
            &self.product_name,
        ]
        .iter()
        .filter_map(|s| s.as_deref())
        .map(str::to_lowercase)
        .any(|s| {
            s.contains("setup")
                || s.contains("install")
                || s.contains("установ")
                || s.contains("bootstrapper")
        })
    }
}

/// `FileDescription`, `ProductName`, `CompanyName`, `OriginalFilename` from the PE version resource.
pub fn version_strings(path: &Path) -> VersionStrings {
    let name = HSTRING::from(path.as_os_str());
    // SAFETY: buffer sized by GetFileVersionInfoSizeW; VerQueryValueW returns pointers into it.
    unsafe {
        let size = GetFileVersionInfoSizeW(&name, None);
        if size == 0 {
            return VersionStrings::default();
        }
        let mut buf = vec![0u8; size as usize];
        if GetFileVersionInfoW(&name, None, size, buf.as_mut_ptr().cast()).is_err() {
            return VersionStrings::default();
        }
        let query = |sub: &str| -> Option<(*mut core::ffi::c_void, u32)> {
            let mut p = std::ptr::null_mut();
            let mut len = 0u32;
            VerQueryValueW(buf.as_ptr().cast(), &HSTRING::from(sub), &mut p, &mut len)
                .as_bool()
                .then_some((p, len))
                .filter(|(p, len)| !p.is_null() && *len > 0)
        };
        let mut langs: Vec<(u16, u16)> = Vec::new();
        if let Some((p, len)) = query(r"\VarFileInfo\Translation") {
            let pairs = std::slice::from_raw_parts(p as *const u16, (len / 2) as usize);
            langs.extend(pairs.as_chunks::<2>().0.iter().map(|c| (c[0], c[1])));
        }
        langs.extend([
            (0x0409, 0x04B0),
            (0x0409, 0x04E4),
            (0x0419, 0x04B0),
            (0x0000, 0x04B0),
        ]);
        let get = |field: &str| -> Option<String> {
            langs.iter().find_map(|(l, cp)| {
                let (p, len) = query(&format!(r"\StringFileInfo\{l:04x}{cp:04x}\{field}"))?;
                let s = std::slice::from_raw_parts(p as *const u16, len as usize);
                let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
                let v = String::from_utf16_lossy(&s[..end]).trim().to_string();
                (!v.is_empty()).then_some(v)
            })
        };
        VersionStrings {
            file_description: get("FileDescription"),
            product_name: get("ProductName"),
            company_name: get("CompanyName"),
            original_filename: get("OriginalFilename"),
        }
    }
}
