//! One-off UAC operation (spec §13): move shortcuts from the Public Desktop to the user's
//! Desktop so Nook can restyle/rename them. Journaled in `adopted.json`; `--undo` moves them
//! back. Never deletes or overwrites.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcessToken, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::{HSTRING, PCWSTR};

use crate::plan::fsops::move_path;
use crate::plan::validate::free_name;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Adopted {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Record {
    pub items: Vec<Adopted>,
}

pub fn load(path: &Path) -> Record {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save(path: &Path, r: &Record) -> Result<()> {
    crate::fsutil::write_atomic(path, &serde_json::to_vec_pretty(r)?)
}

pub fn is_elevated() -> bool {
    // SAFETY: token handle closed; TOKEN_ELEVATION sized correctly.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut e = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut e as *mut TOKEN_ELEVATION).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && e.TokenIsElevated != 0
    }
}

/// Runs this exe elevated with `args` (UAC prompt) and waits. Returns its exit code.
pub fn run_elevated(args: &str) -> Result<u32> {
    let exe = std::env::current_exe()?;
    let file = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from(args);
    let verb = HSTRING::from("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: strings outlive the call; process handle closed below.
    unsafe {
        ShellExecuteExW(&mut info)
            .context("UAC was declined or the elevated process failed to start")?;
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        Ok(code)
    }
}

fn is_shortcut(p: &Path) -> bool {
    p.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("lnk") || e.eq_ignore_ascii_case("url"))
}

/// A moved file keeps the Public Desktop's ACL (admin-only writes). Reset it so it inherits
/// from the user's Desktop like any file created there. Same as `icacls <file> /reset`.
fn inherit_acl(p: &Path) -> Result<()> {
    let out = std::process::Command::new("icacls")
        .arg(p)
        .arg("/reset")
        .arg("/Q")
        .output()
        .context("icacls")?;
    if !out.status.success() {
        bail!(
            "icacls /reset failed for {}: {}",
            p.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Elevated side: moves shortcuts from `public` to `user` (all, or only `names`).
/// Also repairs the ACL of everything adopted earlier (idempotent).
pub fn adopt(public: &Path, user: &Path, record: &Path, names: &[String]) -> Result<Vec<String>> {
    if !is_elevated() {
        bail!("needs administrator rights");
    }
    let mut rec = load(record);
    let mut report = Vec::new();
    for a in &rec.items {
        if a.to.exists() {
            match inherit_acl(&a.to) {
                Ok(()) => report.push(format!("{}: permissions fixed", a.to.display())),
                Err(e) => report.push(format!("{e:#}")),
            }
        }
    }
    for e in std::fs::read_dir(public)?.flatten() {
        let from = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if !is_shortcut(&from)
            || (!names.is_empty() && !names.iter().any(|n| n.eq_ignore_ascii_case(&name)))
        {
            continue;
        }
        let to = free_name(user, &name, &|p| p.exists());
        match move_path(&from, &to) {
            Ok(()) => {
                rec.items.push(Adopted {
                    from: from.clone(),
                    to: to.clone(),
                });
                save(record, &rec)?; // after every move: a crash never loses track
                let acl = inherit_acl(&to)
                    .map_or_else(|e| format!(" (permissions: {e:#})"), |_| String::new());
                report.push(format!("{name} → {}{acl}", to.display()));
            }
            Err(err) => report.push(format!("{name}: {err:#}")),
        }
    }
    Ok(report)
}

/// Elevated side: moves adopted shortcuts back to the Public Desktop.
pub fn give_back(record: &Path) -> Result<Vec<String>> {
    if !is_elevated() {
        bail!("needs administrator rights");
    }
    let mut rec = load(record);
    let mut report = Vec::new();
    let mut left = Vec::new();
    for a in rec.items.drain(..) {
        if !a.to.exists() {
            report.push(format!(
                "{}: not on your desktop any more (moved by a plan?) — kept in the record",
                a.to.display()
            ));
            left.push(a);
            continue;
        }
        let dir = a.from.parent().context("no parent")?;
        let back = free_name(dir, &a.from.file_name().unwrap().to_string_lossy(), &|p| {
            p.exists()
        });
        match move_path(&a.to, &back) {
            Ok(()) => report.push(format!("{} → {}", a.to.display(), back.display())),
            Err(e) => {
                report.push(format!("{}: {e:#}", a.to.display()));
                left.push(a);
            }
        }
    }
    rec.items = left;
    save(record, &rec)?;
    Ok(report)
}
