//! File primitives for plans. Moves never overwrite and nothing is ever deleted.

use std::path::Path;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{
    ERROR_LOCK_VIOLATION, ERROR_NOT_SAME_DEVICE, ERROR_SHARING_VIOLATION, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAGS_AND_ATTRIBUTES, GetFileAttributesW, INVALID_FILE_ATTRIBUTES, MOVE_FILE_FLAGS,
    MoveFileExW, SetFileAttributesW,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::UI::Shell::{
    FOF_NO_UI, FileOperation, IFileOperation, IShellItem, SHCNE_UPDATEDIR, SHCNE_UPDATEITEM,
    SHCNF_FLUSH, SHCNF_PATHW, SHChangeNotify, SHCreateItemFromParsingName,
};
use windows::core::HSTRING;

/// File is open elsewhere (Photoshop etc.): skip with a warning rather than fail the plan.
#[derive(Debug)]
pub struct Busy;

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("file is in use by another program")
    }
}
impl std::error::Error for Busy {}

pub fn is_busy(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Busy>().is_some()
}

fn win32_code(e: &windows::core::Error) -> WIN32_ERROR {
    WIN32_ERROR::from_error(e).unwrap_or_default()
}

/// Same-volume move via `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`;
/// cross-volume via `IFileOperation` (`FOF_NO_UI`). Fails if `to` exists.
pub fn move_path(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        bail!("destination exists: {}", to.display());
    }
    // SAFETY: plain Win32 call with owned wide strings.
    let r = unsafe {
        MoveFileExW(
            &HSTRING::from(from.as_os_str()),
            &HSTRING::from(to.as_os_str()),
            MOVE_FILE_FLAGS(0),
        )
    };
    match r {
        Ok(()) => Ok(()),
        Err(e)
            if matches!(
                win32_code(&e),
                ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION
            ) =>
        {
            Err(Busy.into())
        }
        Err(e) if win32_code(&e) == ERROR_NOT_SAME_DEVICE => move_cross_volume(from, to),
        Err(e) => Err(anyhow::Error::from(e).context(format!(
            "move {} -> {}",
            from.display(),
            to.display()
        ))),
    }
}

fn move_cross_volume(from: &Path, to: &Path) -> Result<()> {
    let dir = to.parent().context("dest has no parent")?;
    let name = to.file_name().context("dest has no name")?;
    // SAFETY: COM on the calling thread (callers run with an apartment).
    unsafe {
        let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
        op.SetOperationFlags(FOF_NO_UI)?;
        let src: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(from.as_os_str()), None)?;
        let dst: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(dir.as_os_str()), None)?;
        op.MoveItem(&src, &dst, &HSTRING::from(name), None)?;
        op.PerformOperations()
            .context("IFileOperation::PerformOperations")?;
        if op.GetAnyOperationsAborted()?.as_bool() {
            bail!("cross-volume move aborted: {}", from.display());
        }
    }
    Ok(())
}

pub fn get_attrs(path: &Path) -> Result<u32> {
    // SAFETY: plain call.
    let a = unsafe { GetFileAttributesW(&HSTRING::from(path.as_os_str())) };
    if a == INVALID_FILE_ATTRIBUTES {
        bail!("GetFileAttributesW failed for {}", path.display());
    }
    Ok(a)
}

pub fn set_attrs(path: &Path, attrs: u32) -> Result<()> {
    // SAFETY: plain call.
    unsafe {
        SetFileAttributesW(
            &HSTRING::from(path.as_os_str()),
            FILE_FLAGS_AND_ATTRIBUTES(attrs),
        )
    }
    .with_context(|| format!("SetFileAttributesW {}", path.display()))
}

pub const ATTR_READONLY: u32 = 0x1;
pub const ATTR_HIDDEN: u32 = 0x2;
pub const ATTR_SYSTEM: u32 = 0x4;
pub const ATTR_NORMAL: u32 = 0x80;

/// Tell the shell an item or folder changed so the desktop view refreshes.
pub fn notify_item(path: &Path) {
    let w = HSTRING::from(path.as_os_str());
    // SAFETY: SHCNF_PATHW with a valid null-terminated wide string.
    unsafe {
        SHChangeNotify(
            SHCNE_UPDATEITEM,
            SHCNF_PATHW | SHCNF_FLUSH,
            Some(w.as_ptr().cast()),
            None,
        )
    };
}

/// After icons changed (desktop.ini / .lnk / .url): Explorer keeps showing cached pictures
/// until its icon cache is refreshed. `ie4uinit -show` is the stock way to do that; then
/// re-touch the changed files so the view re-reads them (`.url` icons are cached separately).
pub fn refresh_icons(changed: &[std::path::PathBuf]) {
    let sys = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let _ = std::process::Command::new(format!(r"{sys}\System32\ie4uinit.exe"))
        .arg("-show")
        .status();
    for p in changed {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(p) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
        notify_item(p);
    }
    // SAFETY: plain call; tells the shell file associations/icons changed.
    unsafe {
        windows::Win32::UI::Shell::SHChangeNotify(
            windows::Win32::UI::Shell::SHCNE_ASSOCCHANGED,
            windows::Win32::UI::Shell::SHCNF_IDLIST,
            None,
            None,
        )
    };
}

pub fn notify_dir(path: &Path) {
    let w = HSTRING::from(path.as_os_str());
    // SAFETY: as above.
    unsafe {
        SHChangeNotify(
            SHCNE_UPDATEDIR,
            SHCNF_PATHW | SHCNF_FLUSH,
            Some(w.as_ptr().cast()),
            None,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a.txt"), d.path().join("b.txt"));
        std::fs::write(&a, "A").unwrap();
        std::fs::write(&b, "B").unwrap();
        assert!(move_path(&a, &b).is_err());
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "B");
        let c = d.path().join("Семен КС").join("c.txt");
        std::fs::create_dir(c.parent().unwrap()).unwrap();
        move_path(&a, &c).unwrap();
        assert_eq!(std::fs::read_to_string(&c).unwrap(), "A");
        assert!(!a.exists());
    }

    #[test]
    fn busy_file_is_reported_as_busy() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("locked.psd");
        std::fs::write(&a, "x").unwrap();
        // share_mode(0): nobody else may open it, like an app holding the file.
        let _hold = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&a)
            .unwrap();
        let e = move_path(&a, &d.path().join("moved.psd")).unwrap_err();
        assert!(is_busy(&e), "{e:#}");
        assert!(a.exists());
    }

    #[test]
    fn attrs_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("x");
        std::fs::write(&a, "x").unwrap();
        let old = get_attrs(&a).unwrap();
        set_attrs(&a, old | ATTR_HIDDEN).unwrap();
        assert!(get_attrs(&a).unwrap() & ATTR_HIDDEN != 0);
        set_attrs(&a, old).unwrap();
        assert_eq!(get_attrs(&a).unwrap(), old);
    }
}
