//! Installed programs from `...\CurrentVersion\Uninstall` (HKLM, HKLM\WOW6432Node, HKCU).

use crate::intel::heuristics::words;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub display_name: String,
    /// Lowercase, no trailing backslash.
    pub install_location: Option<String>,
    /// Lowercase exe path from `DisplayIcon` (index and quotes stripped).
    pub icon_exe: Option<String>,
}

fn norm_dir(s: &str) -> Option<String> {
    let t = s
        .trim()
        .trim_matches('"')
        .trim_end_matches('\\')
        .to_lowercase();
    (!t.is_empty()).then_some(t)
}

/// `"C:\x\app.exe",0` → `c:\x\app.exe`.
pub fn icon_path(s: &str) -> Option<String> {
    let t = s.trim();
    let t = match t.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(""),
        None => t
            .rsplit_once(',')
            .filter(|(_, idx)| idx.trim().parse::<i32>().is_ok())
            .map_or(t, |(p, _)| p),
    };
    norm_dir(t)
}

impl Program {
    pub fn new(
        display_name: &str,
        install_location: Option<&str>,
        display_icon: Option<&str>,
    ) -> Self {
        Self {
            display_name: display_name.trim().to_string(),
            install_location: install_location.and_then(norm_dir),
            icon_exe: display_icon.and_then(icon_path),
        }
    }
}

#[derive(Debug, Default)]
pub struct Programs(pub Vec<Program>);

impl Programs {
    /// Program owning a shortcut target: target inside InstallLocation or equal to/next to DisplayIcon.
    pub fn match_target(&self, target: &str) -> Option<&Program> {
        let t = target.trim().to_lowercase();
        if t.is_empty() {
            return None;
        }
        let parent = t.rsplit_once('\\').map(|(p, _)| p);
        self.0
            .iter()
            .find(|p| {
                p.install_location
                    .as_ref()
                    .is_some_and(|loc| t.starts_with(&format!("{loc}\\")))
                    || p.icon_exe.as_ref().is_some_and(|i| *i == t)
            })
            .or_else(|| {
                self.0.iter().find(|p| {
                    p.icon_exe
                        .as_ref()
                        .and_then(|i| i.rsplit_once('\\').map(|(d, _)| d.to_string()))
                        .as_deref()
                        == parent
                        && parent.is_some()
                })
            })
    }

    /// Program an installer file name probably belongs to: first meaningful word matches.
    pub fn match_installer(&self, file_name: &str) -> Option<&Program> {
        let stem = file_name
            .rsplit_once('.')
            .map_or(file_name, |(s, _)| s)
            .to_lowercase();
        let cleaned = ["installer", "install", "setup", "установщик", "установка"]
            .iter()
            .fold(stem, |s, k| s.replace(k, " "));
        let first = words(&cleaned).into_iter().next()?;
        self.0
            .iter()
            .find(|p| words(&p.display_name).first() == Some(&first))
    }
}

#[cfg(windows)]
pub fn load() -> Programs {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    use crate::winreg::Key;

    let roots = [
        (
            HKEY_LOCAL_MACHINE,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
        (
            HKEY_LOCAL_MACHINE,
            r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
        (
            HKEY_CURRENT_USER,
            r"Software\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
    ];
    let mut out = Vec::new();
    for (root, path) in roots {
        let Some(key) = Key::open(root, path) else {
            continue;
        };
        for sub in key.subkeys() {
            let Some(name) = key
                .string(&sub, "DisplayName")
                .filter(|n| !n.trim().is_empty())
            else {
                continue;
            };
            if key.dword(&sub, "SystemComponent") == Some(1) {
                continue;
            }
            out.push(Program::new(
                &name,
                key.string(&sub, "InstallLocation").as_deref(),
                key.string(&sub, "DisplayIcon").as_deref(),
            ));
        }
    }
    Programs(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progs() -> Programs {
        Programs(vec![
            Program::new(
                "Steam",
                Some(r"C:\Program Files (x86)\Steam\"),
                Some(r"C:\Program Files (x86)\Steam\steam.exe"),
            ),
            Program::new(
                "Blender 4.2",
                Some(r"C:\Program Files\Blender Foundation\Blender 4.2"),
                None,
            ),
            Program::new(
                "Discord",
                None,
                Some(r#""C:\Users\X\AppData\Local\Discord\app.ico",0"#),
            ),
            Program::new(
                "Telegram Desktop",
                None,
                Some(r"C:\Users\X\AppData\Roaming\Telegram Desktop\Telegram.exe,0"),
            ),
        ])
    }

    #[test]
    fn icon_paths() {
        assert_eq!(
            icon_path(r#""C:\A B\x.exe",0"#).as_deref(),
            Some(r"c:\a b\x.exe")
        );
        assert_eq!(
            icon_path(r"C:\A\x.exe,-101").as_deref(),
            Some(r"c:\a\x.exe")
        );
        assert_eq!(icon_path(r"C:\A,B\x.exe").as_deref(), Some(r"c:\a,b\x.exe"));
    }

    #[test]
    fn targets() {
        let p = progs();
        assert_eq!(
            p.match_target(r"C:\Program Files (x86)\Steam\steam.exe")
                .unwrap()
                .display_name,
            "Steam"
        );
        assert_eq!(
            p.match_target(r"C:\Program Files\Blender Foundation\Blender 4.2\blender.exe")
                .unwrap()
                .display_name,
            "Blender 4.2"
        );
        assert_eq!(
            p.match_target(r"C:\Users\X\AppData\Local\Discord\Update.exe")
                .unwrap()
                .display_name,
            "Discord"
        );
        assert_eq!(
            p.match_target(r"C:\Users\X\AppData\Roaming\Telegram Desktop\Telegram.exe")
                .unwrap()
                .display_name,
            "Telegram Desktop"
        );
        assert!(p.match_target(r"D:\Games\other.exe").is_none());
    }

    #[test]
    fn installers() {
        let p = progs();
        assert_eq!(
            p.match_installer("SteamSetup.exe").unwrap().display_name,
            "Steam"
        );
        assert_eq!(
            p.match_installer("blender-4.2.1-windows-x64.msi")
                .unwrap()
                .display_name,
            "Blender 4.2"
        );
        assert_eq!(
            p.match_installer("tsetup-x64.5.2.exe")
                .map(|x| x.display_name.as_str()),
            None
        );
        assert!(p.match_installer("Photoshop_Setup.exe").is_none());
    }
}
