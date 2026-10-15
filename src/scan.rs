//! `scan`: every desktop item (user, public, virtual) with hints (spec §5.1). Read-only.

use std::collections::HashMap;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use serde::Serialize;

use crate::intel::heuristics::{caption_is_blank, is_backup, is_copy, is_installer_name, is_temp};
use crate::intel::taskbar_pins::Pins;
use crate::intel::uninstall::{self, Programs};
use crate::intel::{hashing, userassist};
use crate::paths::Desktops;
use crate::shell::desktop_view::{DesktopView, cell_of};
use crate::shell::explorer_settings::{VIRTUAL_ICONS, visible_virtual_icons};
use crate::shell::fileinfo::{file_id, version_strings};
use crate::shell::shortcuts::{LinkInfo, read_lnk, read_url, target_key};
use crate::timefmt::{age_days, rfc3339};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    User,
    Public,
    Virtual,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Folder,
    Shortcut,
    Url,
    Virtual,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Cell {
    pub col: i32,
    pub row: i32,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ShortcutInfo {
    pub target: Option<String>,
    pub args: Option<String>,
    pub aumid: Option<String>,
    pub url: Option<String>,
    /// `None` when not applicable (URLs, shell-namespace targets).
    pub target_exists: Option<bool>,
    pub pinned_to_taskbar: bool,
    pub launch_count: Option<u32>,
    pub last_launch: Option<String>,
    pub uninstall_match: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct FolderInfo {
    pub entries: usize,
    pub top_entries: Vec<String>,
    pub total_size: u64,
    /// False if the 2 s walk budget ran out.
    pub total_size_complete: bool,
    pub newest_modified: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Hints {
    pub duplicate_of: Vec<String>,
    pub same_target_as: Vec<String>,
    pub is_copy: bool,
    pub is_backup: bool,
    pub is_installer: bool,
    pub is_temp: bool,
    /// Hidden+System file such as desktop.ini: never move, hide via `hide_protected_os_files`.
    pub is_protected_os: bool,
    /// For installers: installed program it most likely belongs to.
    pub installed_match: Option<String>,
    pub age_days: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub path: String,
    pub source: Source,
    pub kind: Kind,
    pub ext: Option<String>,
    pub size: u64,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub attributes: Vec<&'static str>,
    pub writable: bool,
    pub cell: Option<Cell>,
    pub group: Option<String>,
    pub caption_hidden: bool,
    pub shortcut: Option<ShortcutInfo>,
    pub folder: Option<FolderInfo>,
    pub hints: Hints,
    /// Stable identity for change detection (not part of the public schema).
    #[serde(skip)]
    pub fingerprint: Option<Fingerprint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub file_id: [u8; 24],
    pub size: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Serialize)]
pub struct ScanResult {
    pub sandbox: bool,
    pub user_desktop: PathBuf,
    pub public_desktop: PathBuf,
    pub grid: Option<GridInfo>,
    pub total: usize,
    pub items: Vec<Item>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct GridInfo {
    pub step_x: i32,
    pub step_y: i32,
    /// True in sandbox mode: cells are assigned, not read from the view.
    pub fictitious: bool,
}

const ATTRS: &[(u32, &str)] = &[
    (0x1, "readonly"),
    (0x2, "hidden"),
    (0x4, "system"),
    (0x20, "archive"),
    (0x400, "reparse_point"),
    (0x1000, "offline"),
    (0x400000, "recall_on_data_access"),
];

/// Stable short id: `it_` + 8 base32 chars of blake3(seed).
pub fn make_id(seed: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let h = blake3::hash(seed);
    let mut bits = u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap());
    let mut s = String::from("it_");
    for _ in 0..8 {
        s.push(ALPHABET[(bits & 31) as usize] as char);
        bits >>= 5;
    }
    s
}

fn folder_info(dir: &Path, budget: Duration) -> FolderInfo {
    let mut info = FolderInfo {
        total_size_complete: true,
        ..Default::default()
    };
    let mut names: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
    }
    info.entries = names.len();
    names.sort_by_key(|n| n.to_lowercase());
    info.top_entries = names.into_iter().take(20).collect();

    let deadline = Instant::now() + budget;
    let mut newest: Option<SystemTime> = None;
    let mut stack = vec![dir.to_path_buf()];
    'walk: while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            if Instant::now() > deadline {
                info.total_size_complete = false;
                break 'walk;
            }
            let Ok(m) = e.metadata() else { continue };
            if let Ok(t) = m.modified() {
                newest = newest.max(Some(t));
            }
            // Don't follow junctions/symlinks.
            if m.is_dir() && m.file_attributes() & 0x400 == 0 {
                stack.push(e.path());
            } else if m.is_file() {
                info.total_size += m.len();
            }
        }
    }
    info.newest_modified = newest.map(rfc3339);
    info
}

struct Raw {
    path: PathBuf,
    source: Source,
    meta: std::fs::Metadata,
    link: Option<LinkInfo>,
}

/// Collects all items. Requires an STA thread with COM initialised.
pub fn scan(desktops: &Desktops) -> Result<ScanResult> {
    let now = SystemTime::now();
    let mut warnings = Vec::new();

    // Filesystem items.
    let mut raws = Vec::new();
    for (dir, source) in [
        (&desktops.user, Source::User),
        (&desktops.public, Source::Public),
    ] {
        match std::fs::read_dir(dir) {
            Ok(rd) => {
                for e in rd.flatten() {
                    match std::fs::symlink_metadata(e.path()) {
                        Ok(meta) => raws.push(Raw {
                            path: e.path(),
                            source,
                            meta,
                            link: None,
                        }),
                        Err(err) => warnings.push(format!("{}: {err}", e.path().display())),
                    }
                }
            }
            Err(err) => warnings.push(format!("cannot read {}: {err}", dir.display())),
        }
    }
    raws.sort_by_key(|r| {
        (
            r.source == Source::Public,
            r.path
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase()),
        )
    });

    for r in &mut raws {
        let ext = r
            .path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase());
        r.link = match ext.as_deref() {
            Some("lnk") if r.meta.is_file() => read_lnk(&r.path).ok(),
            Some("url") if r.meta.is_file() => read_url(&r.path).ok(),
            _ => None,
        };
    }

    // Context sources (each optional).
    let t0 = Instant::now();
    let pins = Pins::load();
    tracing::debug!(ms = t0.elapsed().as_millis() as u64, "scan: lnk+pins");
    let programs: Programs = uninstall::load();
    tracing::debug!(ms = t0.elapsed().as_millis() as u64, "scan: uninstall");
    let mut launches: HashMap<String, (u32, Option<SystemTime>)> = HashMap::new();
    for e in userassist::read_all() {
        let slot = launches.entry(e.name.to_lowercase()).or_default();
        slot.0 = slot.0.max(e.count);
        slot.1 = slot.1.max(e.last_run);
    }
    let live = Desktops::live().ok();

    tracing::debug!(ms = t0.elapsed().as_millis() as u64, "scan: userassist");
    // Cells.
    let mut cells: HashMap<String, (Cell, String)> = HashMap::new();
    let mut grid = None;
    if desktops.is_sandbox() {
        grid = Some(GridInfo {
            step_x: 75,
            step_y: 85,
            fictitious: true,
        });
    } else {
        match DesktopView::open().and_then(|v| Ok((v.info()?, v.items()?))) {
            Ok((info, view_items)) => {
                grid = Some(GridInfo {
                    step_x: info.spacing_x,
                    step_y: info.spacing_y,
                    fictitious: false,
                });
                for vi in view_items {
                    let (col, row) = cell_of(vi.x, vi.y, info.spacing_x, info.spacing_y);
                    cells.insert(vi.parsing.to_lowercase(), (Cell { col, row }, vi.display));
                }
            }
            Err(e) => warnings.push(format!("desktop view unavailable, cells unknown: {e:#}")),
        }
    }

    tracing::debug!(ms = t0.elapsed().as_millis() as u64, "scan: view");
    // Folder summaries walk in parallel; each has its own 2 s budget.
    let mut folders: HashMap<PathBuf, FolderInfo> = std::thread::scope(|s| {
        let handles: Vec<_> = raws
            .iter()
            .filter(|r| r.meta.is_dir())
            .map(|r| {
                (
                    r.path.clone(),
                    s.spawn(|| folder_info(&r.path, Duration::from_secs(2))),
                )
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|(p, h)| Some((p, h.join().ok()?)))
            .collect()
    });
    let mut items = Vec::new();
    for r in &raws {
        let name = r
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = r
            .path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .filter(|_| r.meta.is_file());
        let attrs = r.meta.file_attributes();
        let kind = match (r.meta.is_dir(), ext.as_deref()) {
            (true, _) => Kind::Folder,
            (false, Some("lnk")) => Kind::Shortcut,
            (false, Some("url")) => Kind::Url,
            _ => Kind::File,
        };
        let fid = file_id(&r.path).ok();
        let id = match fid {
            Some(f) => make_id(&f.to_bytes()),
            None => make_id(r.path.to_string_lossy().to_lowercase().as_bytes()),
        };
        let stem = match kind {
            Kind::Shortcut | Kind::Url => r
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            _ => name.clone(),
        };
        let path_str = r.path.to_string_lossy().into_owned();
        let view = cells.get(&path_str.to_lowercase());
        let display_name = view
            .map(|v| v.1.clone())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| {
                if kind == Kind::File {
                    r.path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                } else {
                    stem.clone()
                }
            });

        let created = r.meta.created().ok();
        let modified = r.meta.modified().ok();
        let newest = created.max(modified);

        let shortcut = r.link.as_ref().map(|l| {
            let mut s = ShortcutInfo {
                target: (!l.target.is_empty()).then(|| l.target.clone()),
                args: (!l.args.is_empty()).then(|| l.args.clone()),
                aumid: l.aumid.clone(),
                url: l.url.clone(),
                target_exists: (!l.target.is_empty()).then(|| Path::new(&l.target).exists()),
                pinned_to_taskbar: pins.contains(l),
                uninstall_match: programs
                    .match_target(&l.target)
                    .map(|p| p.display_name.clone()),
                ..Default::default()
            };
            // UserAssist: the shortcut itself (live path, also when sandboxed) and its target exe.
            let mut keys = vec![path_str.to_lowercase()];
            if let Some((lu, lp)) = &live {
                let base = if r.source == Source::Public { lp } else { lu };
                keys.push(base.join(&name).to_string_lossy().to_lowercase());
            }
            if !l.target.is_empty() {
                keys.push(l.target.to_lowercase());
            }
            for k in keys {
                if let Some((count, last)) = launches.get(&k) {
                    s.launch_count = Some(s.launch_count.unwrap_or(0).max(*count));
                    if let Some(t) = last {
                        let cur = s.last_launch.clone();
                        let t = rfc3339(*t);
                        if cur.as_ref().is_none_or(|c| *c < t) {
                            s.last_launch = Some(t);
                        }
                    }
                }
            }
            s
        });

        let is_exe = ext.as_deref() == Some("exe");
        let installer =
            is_installer_name(&name) || (is_exe && version_strings(&r.path).looks_like_installer());
        let hints = Hints {
            is_copy: is_copy(&name),
            is_backup: is_backup(&name),
            is_installer: installer,
            is_temp: is_temp(&name),
            is_protected_os: attrs & 0x6 == 0x6,
            installed_match: installer
                .then(|| {
                    programs
                        .match_installer(&name)
                        .map(|p| p.display_name.clone())
                })
                .flatten(),
            age_days: newest.map_or(0, |t| age_days(t, now)),
            ..Default::default()
        };

        items.push(Item {
            id,
            name: name.clone(),
            display_name,
            path: path_str,
            source: r.source,
            kind,
            ext,
            size: if r.meta.is_file() { r.meta.len() } else { 0 },
            created: created.map(rfc3339),
            modified: modified.map(rfc3339),
            attributes: ATTRS
                .iter()
                .filter(|(b, _)| attrs & b != 0)
                .map(|(_, n)| *n)
                .collect(),
            writable: r.source == Source::User && attrs & 0x1 == 0,
            cell: view.map(|v| v.0),
            group: None,
            caption_hidden: matches!(kind, Kind::Shortcut | Kind::Url) && caption_is_blank(&stem),
            shortcut,
            folder: folders.remove(&r.path),
            hints,
            fingerprint: fid.map(|f| Fingerprint {
                file_id: f.to_bytes(),
                size: r.meta.len(),
                modified,
            }),
        });
    }

    tracing::debug!(ms = t0.elapsed().as_millis() as u64, "scan: items");
    // Virtual items.
    for name in visible_virtual_icons() {
        let clsid = VIRTUAL_ICONS
            .iter()
            .find(|v| v.0 == name)
            .map(|v| v.1)
            .unwrap_or_default();
        let parsing = format!("::{clsid}");
        let view = cells.get(&parsing.to_lowercase());
        items.push(Item {
            id: make_id(clsid.to_lowercase().as_bytes()),
            name: view
                .map(|v| v.1.clone())
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| name.to_string()),
            display_name: view
                .map(|v| v.1.clone())
                .unwrap_or_else(|| name.to_string()),
            path: parsing,
            source: Source::Virtual,
            kind: Kind::Virtual,
            ext: None,
            size: 0,
            created: None,
            modified: None,
            attributes: vec![],
            writable: false,
            cell: view.map(|v| v.0),
            group: None,
            caption_hidden: false,
            shortcut: None,
            folder: None,
            hints: Hints::default(),
            fingerprint: None,
        });
    }

    // Fictitious sandbox grid: column-major, 12 rows.
    if desktops.is_sandbox() {
        for (i, it) in items.iter_mut().enumerate() {
            it.cell = Some(Cell {
                col: (i / 12) as i32,
                row: (i % 12) as i32,
            });
        }
    }

    link_hints(&mut items);
    tracing::debug!(ms = t0.elapsed().as_millis() as u64, "scan: hints");
    Ok(ScanResult {
        sandbox: desktops.is_sandbox(),
        user_desktop: desktops.user.clone(),
        public_desktop: desktops.public.clone(),
        grid,
        total: items.len(),
        items,
        warnings,
    })
}

/// Cross-item hints: duplicates by content hash, shortcuts with the same target.
fn link_hints(items: &mut [Item]) {
    let files: Vec<(usize, PathBuf, u64)> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| it.kind == Kind::File && !it.hints.is_protected_os)
        .map(|(i, it)| (i, PathBuf::from(&it.path), it.size))
        .collect();
    for group in hashing::find_duplicates(&files) {
        for &i in &group {
            items[i].hints.duplicate_of = group
                .iter()
                .filter(|&&j| j != i)
                .map(|&j| items[j].id.clone())
                .collect();
        }
    }

    let mut by_target: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        if let Some(s) = &it.shortcut {
            let info = LinkInfo {
                target: s.target.clone().unwrap_or_default(),
                args: s.args.clone().unwrap_or_default(),
                aumid: s.aumid.clone(),
                url: s.url.clone(),
            };
            if let Some(k) = target_key(&info) {
                by_target.entry(k).or_default().push(i);
            }
        }
    }
    for group in by_target.into_values().filter(|g| g.len() > 1) {
        for &i in &group {
            items[i].hints.same_target_as = group
                .iter()
                .filter(|&&j| j != i)
                .map(|&j| items[j].id.clone())
                .collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_shaped() {
        let a = make_id(b"volume+fileid");
        assert_eq!(a, make_id(b"volume+fileid"));
        assert_ne!(a, make_id(b"other"));
        assert_eq!(a.len(), 11);
        assert!(a.starts_with("it_"));
    }

    #[test]
    fn folder_summary() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("a.txt"), b"12345").unwrap();
        std::fs::write(d.path().join("sub").join("b.bin"), b"123").unwrap();
        let f = folder_info(d.path(), Duration::from_secs(2));
        assert_eq!(f.entries, 2);
        assert_eq!(f.top_entries, vec!["a.txt", "sub"]);
        assert_eq!(f.total_size, 8);
        assert!(f.total_size_complete);
        assert!(f.newest_modified.is_some());
    }
}
