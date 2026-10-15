//! Taskbar pins: `%APPDATA%\Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar\*.lnk`.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::shell::shortcuts::{LinkInfo, read_lnk, target_key};

#[derive(Debug, Default)]
pub struct Pins {
    keys: HashSet<String>,
    aumids: HashSet<String>,
    pub count: usize,
}

impl Pins {
    pub fn dir() -> Option<PathBuf> {
        std::env::var_os("APPDATA").map(|a| {
            PathBuf::from(a).join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar")
        })
    }

    /// Requires COM. Missing folder → no pins.
    pub fn load() -> Self {
        let mut pins = Self::default();
        let Some(dir) = Self::dir() else { return pins };
        let Ok(rd) = std::fs::read_dir(dir) else {
            return pins;
        };
        for e in rd.flatten() {
            let p = e.path();
            if !p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")) {
                continue;
            }
            if let Ok(info) = read_lnk(&p) {
                pins.add(&info);
            }
        }
        pins
    }

    pub fn add(&mut self, info: &LinkInfo) {
        self.count += 1;
        if let Some(k) = target_key(info) {
            self.keys.insert(k);
        }
        if let Some(a) = &info.aumid {
            self.aumids.insert(a.to_lowercase());
        }
    }

    /// Same target + args, or the same AppUserModelID.
    pub fn contains(&self, info: &LinkInfo) -> bool {
        target_key(info).is_some_and(|k| self.keys.contains(&k))
            || info
                .aumid
                .as_ref()
                .is_some_and(|a| self.aumids.contains(&a.to_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_target_args_or_aumid() {
        let mut pins = Pins::default();
        pins.add(&LinkInfo {
            target: r"C:\D\Update.exe".into(),
            args: "--processStart Discord.exe".into(),
            ..Default::default()
        });
        pins.add(&LinkInfo {
            aumid: Some("Microsoft.WindowsTerminal_8wekyb3d8bbwe!App".into()),
            ..Default::default()
        });

        assert!(pins.contains(&LinkInfo {
            target: r"c:\d\update.exe".into(),
            args: "--processStart Discord.exe".into(),
            ..Default::default()
        }));
        assert!(!pins.contains(&LinkInfo {
            target: r"C:\D\Update.exe".into(),
            args: "--processStart Slack.exe".into(),
            ..Default::default()
        }));
        assert!(pins.contains(&LinkInfo {
            target: r"C:\Other\wt.exe".into(),
            aumid: Some("microsoft.windowsterminal_8wekyb3d8bbwe!app".into()),
            ..Default::default()
        }));
    }
}
