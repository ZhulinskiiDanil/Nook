//! Where things live: known folders, Nook's own dirs, and the dev sandbox.
//!
//! Sandbox (spec §10): when `NOOK_DESKTOP_ROOT` is set, all desktop file operations are
//! redirected to `<root>\Desktop` (user) and `<root>\Public` (public desktop).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_PublicDesktop,
    FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::core::GUID;

pub const SANDBOX_ENV: &str = "NOOK_DESKTOP_ROOT";

fn known_folder(id: &GUID) -> Result<PathBuf> {
    // SAFETY: the returned buffer is owned by us and freed with CoTaskMemFree.
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).context("SHGetKnownFolderPath")?;
        let s = p.to_string();
        CoTaskMemFree(Some(p.0 as *const _));
        Ok(PathBuf::from(s?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopMode {
    Live,
    Sandbox(PathBuf),
}

/// The two filesystem desktops Nook manages.
#[derive(Debug, Clone)]
pub struct Desktops {
    pub mode: DesktopMode,
    pub user: PathBuf,
    pub public: PathBuf,
}

impl Desktops {
    pub fn resolve() -> Result<Self> {
        match std::env::var_os(SANDBOX_ENV).filter(|v| !v.is_empty()) {
            Some(root) => Ok(Self::sandbox(PathBuf::from(root))),
            None => Ok(Self {
                mode: DesktopMode::Live,
                user: known_folder(&FOLDERID_Desktop)?,
                public: known_folder(&FOLDERID_PublicDesktop)?,
            }),
        }
    }

    pub fn sandbox(root: PathBuf) -> Self {
        Self {
            user: root.join("Desktop"),
            public: root.join("Public"),
            mode: DesktopMode::Sandbox(root),
        }
    }

    pub fn is_sandbox(&self) -> bool {
        matches!(self.mode, DesktopMode::Sandbox(_))
    }

    /// Real (live) desktop folders, regardless of sandbox mode. Read-only use only.
    pub fn live() -> Result<(PathBuf, PathBuf)> {
        Ok((
            known_folder(&FOLDERID_Desktop)?,
            known_folder(&FOLDERID_PublicDesktop)?,
        ))
    }
}

/// Nook's own directories.
#[derive(Debug, Clone)]
pub struct AppDirs {
    /// `%APPDATA%\Nook`
    pub config_dir: PathBuf,
    /// `%LOCALAPPDATA%\Nook`
    pub data_dir: PathBuf,
}

impl AppDirs {
    pub fn resolve() -> Result<Self> {
        Ok(Self {
            config_dir: known_folder(&FOLDERID_RoamingAppData)?.join("Nook"),
            data_dir: known_folder(&FOLDERID_LocalAppData)?.join("Nook"),
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }
}

pub fn user_profile() -> Result<PathBuf> {
    known_folder(&FOLDERID_Profile)
}

/// Expands `%NAME%` tokens. `%DESKTOP%` maps to the (possibly sandboxed) user desktop;
/// other names come from `lookup`. Unknown names are an error, `%%` is a literal `%`.
pub fn expand_vars(
    template: &str,
    desktop: &Path,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<PathBuf> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            bail!("unclosed % in {template:?}")
        };
        let name = &after[..end];
        if name.is_empty() {
            out.push('%');
        } else if name.eq_ignore_ascii_case("DESKTOP") {
            out.push_str(&desktop.to_string_lossy());
        } else {
            match lookup(name) {
                Some(v) => out.push_str(&v),
                None => bail!("unknown variable %{name}% in {template:?}"),
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(PathBuf::from(out))
}

/// [`expand_vars`] backed by the process environment.
pub fn expand_env(template: &str, desktop: &Path) -> Result<PathBuf> {
    expand_vars(template, desktop, |n| std::env::var(n).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(n: &str) -> Option<String> {
        (n.eq_ignore_ascii_case("USERPROFILE")).then(|| r"C:\Users\Семен".to_string())
    }

    #[test]
    fn expands_desktop_and_env() {
        let d = Path::new(r"D:\OneDrive\Рабочий стол");
        assert_eq!(
            expand_vars(r"%DESKTOP%\Art", d, lookup).unwrap(),
            PathBuf::from(r"D:\OneDrive\Рабочий стол\Art")
        );
        assert_eq!(
            expand_vars(r"%USERPROFILE%\.nook\quarantine", d, lookup).unwrap(),
            PathBuf::from(r"C:\Users\Семен\.nook\quarantine")
        );
        assert_eq!(
            expand_vars("100%%", d, lookup).unwrap(),
            PathBuf::from("100%")
        );
    }

    #[test]
    fn rejects_unknown_and_unclosed() {
        let d = Path::new(r"C:\D");
        assert!(expand_vars("%NOPE%", d, lookup).is_err());
        assert!(expand_vars("%DESKTOP", d, lookup).is_err());
    }

    #[test]
    fn sandbox_layout() {
        let s = Desktops::sandbox(PathBuf::from(r"C:\nook-sandbox"));
        assert!(s.is_sandbox());
        assert_eq!(s.user, PathBuf::from(r"C:\nook-sandbox\Desktop"));
        assert_eq!(s.public, PathBuf::from(r"C:\nook-sandbox\Public"));
    }
}
