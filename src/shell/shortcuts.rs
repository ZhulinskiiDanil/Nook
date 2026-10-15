//! `.lnk` (IShellLinkW + property store) and `.url` (INI) shortcuts. Read-only.

use std::path::Path;

use anyhow::{Context, Result};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IPersistFile, STGM_READ,
};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{IShellLinkW, SLGP_RAWPATH, ShellLink};
use windows::core::{HSTRING, Interface};

use crate::intel::text::decode_prefix;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkInfo {
    /// Target path with environment variables expanded; empty for shell-namespace targets.
    pub target: String,
    pub args: String,
    pub aumid: Option<String>,
    pub url: Option<String>,
}

fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn expand_env_vars(s: &str) -> String {
    // Expand %VAR% using the process environment; leave unknown ones as-is.
    let mut out = String::new();
    let mut rest = s;
    while let Some(a) = rest.find('%') {
        let Some(b) = rest[a + 1..].find('%') else {
            break;
        };
        let name = &rest[a + 1..a + 1 + b];
        out.push_str(&rest[..a]);
        match std::env::var(name) {
            Ok(v) if !name.is_empty() => out.push_str(&v),
            _ => out.push_str(&rest[a..a + b + 2]),
        }
        rest = &rest[a + b + 2..];
    }
    out.push_str(rest);
    out
}

/// Reads a `.lnk`. Requires COM on the calling thread.
pub fn read_lnk(path: &Path) -> Result<LinkInfo> {
    // SAFETY: COM objects owned locally; buffers sized for the calls.
    unsafe {
        let link: IShellLinkW =
            CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).context("ShellLink")?;
        let pf: IPersistFile = link.cast()?;
        pf.Load(&HSTRING::from(path.as_os_str()), STGM_READ)
            .context("IPersistFile::Load")?;

        let mut buf = vec![0u16; 1024];
        let mut fd = WIN32_FIND_DATAW::default();
        let target = match link.GetPath(&mut buf, &mut fd, SLGP_RAWPATH.0 as u32) {
            Ok(()) => expand_env_vars(&from_wide(&buf)),
            Err(_) => String::new(),
        };
        let mut args = vec![0u16; 4096];
        let args = match link.GetArguments(&mut args) {
            Ok(()) => from_wide(&args),
            Err(_) => String::new(),
        };
        let aumid = link.cast::<IPropertyStore>().ok().and_then(|ps| {
            let v = ps.GetValue(&PKEY_AppUserModel_ID).ok()?;
            let p = PropVariantToStringAlloc(&v).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            s.filter(|s| !s.is_empty())
        });
        Ok(LinkInfo {
            target,
            args,
            aumid,
            url: None,
        })
    }
}

/// Points a `.lnk` at a new icon (`SetIconLocation` + save). Requires COM.
pub fn set_lnk_icon(path: &Path, ico: &Path) -> Result<()> {
    use windows::Win32::System::Com::STGM_READWRITE;
    // SAFETY: COM objects owned locally.
    unsafe {
        let link: IShellLinkW =
            CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).context("ShellLink")?;
        let pf: IPersistFile = link.cast()?;
        let p = HSTRING::from(path.as_os_str());
        pf.Load(&p, STGM_READWRITE).context("IPersistFile::Load")?;
        link.SetIconLocation(&HSTRING::from(ico.as_os_str()), 0)
            .context("SetIconLocation")?;
        pf.Save(&p, true).context("IPersistFile::Save")?;
    }
    Ok(())
}

/// `.url` text with `IconFile`/`IconIndex` set in `[InternetShortcut]` (other lines kept).
pub fn set_url_icon(bytes: &[u8], ico: &str) -> Vec<u8> {
    let text = decode_prefix(bytes, 1 << 20)
        .map(|(t, _)| t)
        .unwrap_or_default();
    let mut out = Vec::new();
    let mut in_section = false;
    let mut done = false;
    let flush = |out: &mut Vec<String>, done: &mut bool| {
        if !*done {
            out.push(format!("IconFile={ico}"));
            out.push("IconIndex=0".into());
            *done = true;
        }
    };
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if in_section {
                flush(&mut out, &mut done);
            }
            in_section = t.eq_ignore_ascii_case("[InternetShortcut]");
            out.push(line.to_string());
            continue;
        }
        if in_section
            && let Some((k, _)) = t.split_once('=')
            && (k.trim().eq_ignore_ascii_case("IconFile")
                || k.trim().eq_ignore_ascii_case("IconIndex"))
        {
            continue;
        }
        out.push(line.to_string());
    }
    if in_section {
        flush(&mut out, &mut done);
    } else if !done {
        out.push("[InternetShortcut]".into());
        flush(&mut out, &mut done);
    }
    let mut s = out.join("\r\n");
    s.push_str("\r\n");
    s.into_bytes()
}

/// Parses `.url` content: `[InternetShortcut]` → `URL=...`.
pub fn parse_url_file(bytes: &[u8]) -> Option<String> {
    let (text, _) = decode_prefix(bytes, 64 * 1024)?;
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case("[InternetShortcut]");
        } else if in_section
            && let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case("URL")
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

pub fn read_url(path: &Path) -> Result<LinkInfo> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(LinkInfo {
        url: parse_url_file(&bytes),
        ..Default::default()
    })
}

/// Normalised key for "same target" comparisons: target + args, case-insensitive, or AUMID.
pub fn target_key(info: &LinkInfo) -> Option<String> {
    if let Some(u) = &info.url {
        return Some(format!("url:{}", u.to_lowercase()));
    }
    if !info.target.is_empty() {
        return Some(format!(
            "path:{}|{}",
            info.target.to_lowercase(),
            info.args.trim().to_lowercase()
        ));
    }
    info.aumid
        .as_ref()
        .map(|a| format!("aumid:{}", a.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_file() {
        let steam = b"[{000214A0-0000-0000-C000-000000000046}]\r\nProp3=19,0\r\n[InternetShortcut]\r\nIDList=\r\nIconIndex=0\r\nURL=steam://rungameid/322170\r\nIconFile=C:\\Steam\\icon.ico\r\n";
        assert_eq!(
            parse_url_file(steam).as_deref(),
            Some("steam://rungameid/322170")
        );
        assert_eq!(parse_url_file(b"[Other]\nURL=x\n"), None);
        assert_eq!(parse_url_file(b"\x00\x01\x02\x03\x04\x05\x06\x07"), None);
    }

    #[test]
    fn url_icon_is_replaced_and_url_kept() {
        let src = b"[{000214A0-0000-0000-C000-000000000046}]\r\nProp3=19,0\r\n[InternetShortcut]\r\nURL=steam://rungameid/322170\r\nIconFile=C:\\Steam\\icon.ico\r\nIconIndex=3\r\n";
        let out = String::from_utf8(set_url_icon(src, r"C:\N\a.ico")).unwrap();
        assert!(
            out.contains("IconFile=C:\\N\\a.ico\r\nIconIndex=0"),
            "{out}"
        );
        assert_eq!(out.matches("IconFile").count(), 1);
        assert!(out.contains("Prop3=19,0"));
        assert_eq!(
            parse_url_file(out.as_bytes()).as_deref(),
            Some("steam://rungameid/322170")
        );
    }

    #[test]
    fn keys() {
        let squirrel = |args: &str| LinkInfo {
            target: r"C:\Users\X\AppData\Local\Discord\Update.exe".into(),
            args: args.into(),
            ..Default::default()
        };
        assert_ne!(
            target_key(&squirrel("--processStart Discord.exe")),
            target_key(&squirrel("--processStart Slack.exe"))
        );
        assert_eq!(
            target_key(&squirrel("--processStart Discord.exe")),
            target_key(&squirrel(" --processStart discord.exe"))
        );
        assert_eq!(target_key(&LinkInfo::default()), None);
    }

    #[test]
    fn env_expansion() {
        // SAFETY: test-only, single-threaded use of a unique variable name.
        unsafe { std::env::set_var("NOOK_TEST_DIR", r"C:\Prog") };
        assert_eq!(
            expand_env_vars(r"%NOOK_TEST_DIR%\app.exe"),
            r"C:\Prog\app.exe"
        );
        assert_eq!(expand_env_vars(r"%NOPE_NOT_SET%\a"), r"%NOPE_NOT_SET%\a");
    }
}
