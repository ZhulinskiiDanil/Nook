//! Icon position snapshots: `%LOCALAPPDATA%\Nook\positions\<time>.json` + `latest.json`.
//! Positions only — never files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::fsutil::write_atomic;
use crate::shell::desktop_view::ViewItem;
use crate::timefmt::rfc3339;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Saved {
    /// Lowercase filesystem path or `::{CLSID}`.
    pub key: String,
    pub display: String,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub created: String,
    pub items: Vec<Saved>,
}

impl Snapshot {
    pub fn from_view(items: &[ViewItem]) -> Self {
        Self {
            created: rfc3339(SystemTime::now()),
            items: items
                .iter()
                .map(|i| Saved {
                    key: i.parsing.to_lowercase(),
                    display: i.display.clone(),
                    x: i.x,
                    y: i.y,
                })
                .collect(),
        }
    }

    pub fn targets(&self) -> HashMap<String, (i32, i32)> {
        self.items
            .iter()
            .map(|s| (s.key.clone(), (s.x, s.y)))
            .collect()
    }

    /// Items whose current position differs from the snapshot.
    pub fn diff(&self, now: &[ViewItem]) -> Vec<String> {
        let cur: HashMap<String, (i32, i32)> = now
            .iter()
            .map(|i| (i.parsing.to_lowercase(), (i.x, i.y)))
            .collect();
        self.items
            .iter()
            .filter(|s| cur.get(&s.key).is_some_and(|&p| p != (s.x, s.y)))
            .map(|s| s.display.clone())
            .collect()
    }
}

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("positions")
}

pub fn latest(data_dir: &Path) -> PathBuf {
    dir(data_dir).join("latest.json")
}

/// Writes a timestamped snapshot and updates `latest.json`. Returns the timestamped path.
pub fn save(data_dir: &Path, snap: &Snapshot) -> Result<PathBuf> {
    let json = serde_json::to_vec_pretty(snap)?;
    let stamp = snap.created.replace(':', "-");
    let path = dir(data_dir).join(format!("{stamp}.json"));
    write_atomic(&path, &json)?;
    write_atomic(&latest(data_dir), &json)?;
    Ok(path)
}

pub fn load(path: &Path) -> Result<Snapshot> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// Guard for anything that moves real icons (spec §10): a saved snapshot must exist.
pub fn require_saved(data_dir: &Path) -> Result<Snapshot> {
    let p = latest(data_dir);
    if !p.is_file() {
        bail!("no saved icon positions. Run `nookctl positions save` first (spec §10).");
    }
    load(&p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vi(p: &str, x: i32, y: i32) -> ViewItem {
        ViewItem {
            parsing: p.into(),
            is_filesystem: true,
            display: p.into(),
            x,
            y,
        }
    }

    #[test]
    fn save_load_diff() {
        let d = tempfile::tempdir().unwrap();
        let snap = Snapshot::from_view(&[
            vi(r"C:\D\Steam.lnk", 20, 2),
            vi("::{645FF040-5081-101B-9F08-00AA002F954E}", 20, 1277),
        ]);
        let p = save(d.path(), &snap).unwrap();
        assert!(p.is_file());
        let back = require_saved(d.path()).unwrap();
        assert_eq!(back.items, snap.items);
        assert_eq!(back.targets()[r"c:\d\steam.lnk"], (20, 2));
        let moved = back.diff(&[
            vi(r"C:\D\Steam.lnk", 95, 2),
            vi("::{645FF040-5081-101B-9F08-00AA002F954E}", 20, 1277),
        ]);
        assert_eq!(moved, vec![r"C:\D\Steam.lnk".to_string()]);
    }

    #[test]
    fn guard_without_snapshot() {
        let d = tempfile::tempdir().unwrap();
        assert!(require_saved(d.path()).is_err());
    }
}
