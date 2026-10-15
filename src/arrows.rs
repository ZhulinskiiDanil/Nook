//! Shortcut arrow overlay on/off (system-wide, HKLM; needs elevation). Reversible: the previous
//! value of `Shell Icons\29` is recorded in `arrows.json` and restored by `--on`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
};
use windows::core::HSTRING;

const KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\Shell Icons";
const VALUE: &str = "29";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Record {
    /// What `29` was before Nook changed it (`None` = not set).
    pub previous: Option<String>,
    pub nook_set: bool,
}

/// Fully transparent icon for the overlay slot, in a place every user can read.
pub fn blank_icon_path() -> PathBuf {
    let pd = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    pd.join("Nook").join("blank.ico")
}

fn current() -> Option<String> {
    crate::winreg::Key::open(HKEY_LOCAL_MACHINE, KEY).and_then(|k| k.string("", VALUE))
}

fn set(v: &str) -> Result<()> {
    let w: Vec<u16> = v.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: buffer of w.len()*2 bytes including the terminator.
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(KEY),
            &HSTRING::from(VALUE),
            REG_SZ.0,
            Some(w.as_ptr().cast()),
            (w.len() * 2) as u32,
        )
    };
    r.ok()
        .map_err(|e| anyhow::anyhow!("HKLM {KEY}\\{VALUE}: {e}"))
}

fn remove() -> Result<()> {
    // SAFETY: plain call.
    let r = unsafe {
        RegDeleteKeyValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(KEY),
            &HSTRING::from(VALUE),
        )
    };
    if r.is_ok() || r == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        bail!("delete {KEY}\\{VALUE}: {r:?}")
    }
}

/// Elevated side. `off = true` hides arrows; `false` restores what was there before.
pub fn apply(off: bool, record: &Path) -> Result<String> {
    if !crate::adopt::is_elevated() {
        bail!("needs administrator rights");
    }
    let mut rec: Record = std::fs::read_to_string(record)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if off {
        let blank = blank_icon_path();
        if !blank.is_file() {
            let empty = tiny_skia::Pixmap::new(256, 256).context("pixmap")?;
            std::fs::create_dir_all(blank.parent().unwrap())?;
            std::fs::write(&blank, crate::icons::ico_bytes(&empty)?)?;
        }
        let want = format!("{},0", blank.display());
        if !rec.nook_set {
            rec.previous = current();
        }
        set(&want)?;
        rec.nook_set = true;
        crate::fsutil::write_atomic(record, &serde_json::to_vec_pretty(&rec)?)?;
        Ok(format!("shortcut arrows off ({KEY}\\{VALUE} = {want})"))
    } else {
        if !rec.nook_set {
            return Ok("Nook never changed the arrows; nothing to restore".into());
        }
        match &rec.previous {
            Some(p) => set(p)?,
            None => remove()?,
        }
        rec = Record::default();
        crate::fsutil::write_atomic(record, &serde_json::to_vec_pretty(&rec)?)?;
        Ok("shortcut arrows restored".into())
    }
}

/// Restarts Explorer so the overlay change shows (icon positions are kept by Explorer/keeper).
pub fn restart_explorer() -> Result<()> {
    let _ = std::process::Command::new("taskkill")
        .args(["/f", "/im", "explorer.exe"])
        .output();
    std::thread::sleep(std::time::Duration::from_millis(800));
    std::process::Command::new("explorer.exe")
        .spawn()
        .context("start explorer")?;
    Ok(())
}
