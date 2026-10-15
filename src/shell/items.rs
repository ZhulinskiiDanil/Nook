//! Filesystem enumeration of desktop folders. M0: counts only (full scan arrives in M1).

use std::os::windows::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct FolderCounts {
    pub total: usize,
    pub files: usize,
    pub folders: usize,
    pub shortcuts: usize,
    pub hidden: usize,
    /// Hidden + System (e.g. desktop.ini).
    pub protected_os: usize,
}

impl FolderCounts {
    /// How many Explorer will show, given hidden/super-hidden settings.
    pub fn visible(&self, show_hidden: bool, show_super_hidden: bool) -> usize {
        let plain_hidden = self.hidden - self.protected_os;
        self.total
            - if show_hidden { 0 } else { plain_hidden }
            - if show_super_hidden {
                0
            } else {
                self.protected_os
            }
    }
}

/// Counts direct children. Never modifies anything.
pub fn count(dir: &Path) -> Result<FolderCounts> {
    let mut c = FolderCounts::default();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let attrs = meta.file_attributes();
        c.total += 1;
        if meta.is_dir() {
            c.folders += 1;
        } else {
            c.files += 1;
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if name.ends_with(".lnk") || name.ends_with(".url") {
                c.shortcuts += 1;
            }
        }
        if attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
            c.hidden += 1;
            if attrs & FILE_ATTRIBUTE_SYSTEM != 0 {
                c.protected_os += 1;
            }
        }
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_kinds() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("Steam.url"), "[InternetShortcut]").unwrap();
        std::fs::write(d.path().join("robot_03 копия 2.png"), b"x").unwrap();
        std::fs::create_dir(d.path().join("Семен КС конфиги")).unwrap();
        let c = count(d.path()).unwrap();
        assert_eq!((c.total, c.files, c.folders, c.shortcuts), (3, 2, 1, 1));
    }

    #[test]
    fn visibility_rules() {
        let c = FolderCounts {
            total: 10,
            hidden: 3,
            protected_os: 2,
            ..Default::default()
        };
        assert_eq!(c.visible(true, true), 10);
        assert_eq!(c.visible(true, false), 8);
        assert_eq!(c.visible(false, false), 7);
    }
}
