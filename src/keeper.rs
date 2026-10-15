//! Layout keeper and inbox rules (spec §6.4, §6.5). Pure decisions; the daemon does the I/O.
//!
//! - Explorer reset many icons at once (more than half moved) → restore our layout.
//! - The user dragged 1–2 icons → accept as their choice (pin) and never fight it.
//! - New files on the desktop go to the next free cell of the inbox corner.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::layout::engine::{Cell, Group};

/// One placed desktop item Nook keeps an eye on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    /// Stable scan id (FileId-based; survives renames).
    pub id: Option<String>,
    /// Lowercase path or `::{CLSID}` at the time of placement.
    pub key: String,
    pub group: Group,
    /// Where Nook put it (or where the user moved it).
    pub cell: Cell,
    /// The user moved it by hand: keep this cell through relayouts.
    #[serde(default)]
    pub pinned: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeeperState {
    pub items: Vec<Kept>,
    /// Grid the cells refer to: (step_x, step_y, cols, rows). Icon size / DPI / resolution
    /// changes alter it; then the layout is recomputed instead of restored.
    #[serde(default)]
    pub grid: Option<(i32, i32, i32, i32)>,
}

impl KeeperState {
    /// Cells no longer valid on `grid` (different step, or outside the area).
    pub fn stale_for(&self, grid: (i32, i32, i32, i32)) -> bool {
        self.grid.is_some_and(|g| g != grid)
            || self.items.iter().any(|k| {
                k.cell.col < 0 || k.cell.row < 0 || k.cell.col >= grid.2 || k.cell.row >= grid.3
            })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to do.
    Fine,
    /// Explorer reset positions: put everything back.
    Restore,
    /// The user moved a few icons: remember them as pins (index → new cell).
    Accept(Vec<(usize, Cell)>),
}

/// Compares kept cells with current ones (`None` = item not on the desktop any more).
pub fn judge(state: &KeeperState, current: &[Option<Cell>]) -> Verdict {
    let present: Vec<(usize, Cell, Cell)> = state
        .items
        .iter()
        .zip(current)
        .enumerate()
        .filter_map(|(i, (k, c))| c.map(|c| (i, k.cell, c)))
        .collect();
    let moved: Vec<(usize, Cell)> = present
        .iter()
        .filter(|(_, a, b)| a != b)
        .map(|&(i, _, b)| (i, b))
        .collect();
    if moved.is_empty() {
        Verdict::Fine
    } else if present.len() >= 4 && moved.len() * 2 > present.len() {
        Verdict::Restore
    } else {
        Verdict::Accept(moved)
    }
}

/// Case-insensitive glob with `*` and `?` (for `[inbox] ignore`).
pub fn glob(pattern: &str, name: &str) -> bool {
    fn m(p: &[char], n: &[char]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some('*'), _) => m(&p[1..], n) || (!n.is_empty() && m(p, &n[1..])),
            (Some('?'), Some(_)) => m(&p[1..], &n[1..]),
            (Some(a), Some(b)) => a == b && m(&p[1..], &n[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    m(&p, &n)
}

pub fn ignored(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| glob(p, name))
}

/// Temporary download names whose final rename means "a new file arrived".
pub fn is_partial(name: &str) -> bool {
    let l = name.to_lowercase();
    [
        ".crdownload",
        ".part",
        ".tmp",
        ".download",
        ".partial",
        ".!ut",
    ]
    .iter()
    .any(|e| l.ends_with(e))
}

/// File-system event, already simplified by the watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsEvent {
    Created(String),
    RenamedFrom(String),
    RenamedTo(String),
    Removed(String),
}

/// Decides which events mean "a new item landed on the desktop". Keeps the last
/// `RenamedFrom` to tell a download finishing (`x.crdownload` → `x.zip`) from a plain rename.
#[derive(Debug, Default)]
pub struct Arrivals {
    pending_from: Option<String>,
    /// Names already on the desktop (lowercase). Apps and Explorer often re-save an existing
    /// file via "write temp, rename over": that is not a new item.
    known: std::collections::HashSet<String>,
}

impl Arrivals {
    pub fn new(existing: impl IntoIterator<Item = String>) -> Self {
        Self {
            pending_from: None,
            known: existing.into_iter().map(|n| n.to_lowercase()).collect(),
        }
    }

    /// Returns the file name to place in the inbox, if any.
    pub fn feed(&mut self, ev: FsEvent, ignore: &[String]) -> Option<String> {
        match ev {
            FsEvent::Created(n) => {
                let new = self.known.insert(n.to_lowercase());
                (new && !ignored(&n, ignore) && !is_partial(&n)).then_some(n)
            }
            FsEvent::RenamedFrom(n) => {
                self.known.remove(&n.to_lowercase());
                self.pending_from = Some(n);
                None
            }
            FsEvent::RenamedTo(n) => {
                let from = self.pending_from.take();
                let new = self.known.insert(n.to_lowercase());
                let arrived = from
                    .as_deref()
                    .is_none_or(|f| is_partial(f) || ignored(f, ignore));
                (new && arrived && !ignored(&n, ignore) && !is_partial(&n)).then_some(n)
            }
            FsEvent::Removed(n) => {
                self.known.remove(&n.to_lowercase());
                None
            }
        }
    }
}

/// Group lists from kept items, in placement order (for relayout after a display change).
pub fn groups_of(state: &KeeperState) -> BTreeMap<Group, Vec<usize>> {
    let mut g: BTreeMap<Group, Vec<usize>> = BTreeMap::new();
    for (i, k) in state.items.iter().enumerate() {
        g.entry(k.group).or_default().push(i);
    }
    g
}

pub fn load(path: &Path) -> KeeperState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(path: &Path, s: &KeeperState) -> anyhow::Result<()> {
    crate::fsutil::write_atomic(path, &serde_json::to_vec_pretty(s)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(cells: &[(i32, i32)]) -> KeeperState {
        KeeperState {
            items: cells
                .iter()
                .enumerate()
                .map(|(i, &(col, row))| Kept {
                    id: None,
                    key: format!("k{i}"),
                    group: Group::Apps,
                    cell: Cell { col, row },
                    pinned: false,
                })
                .collect(),
            grid: None,
        }
    }
    fn c(col: i32, row: i32) -> Option<Cell> {
        Some(Cell { col, row })
    }

    #[test]
    fn grid_change_makes_cells_stale() {
        let mut s = st(&[(1, 1), (1, 15)]);
        // Old state without a recorded grid: row 15 doesn't exist on a 14-row grid.
        assert!(s.stale_for((75, 98, 34, 14)));
        assert!(!s.stale_for((75, 85, 34, 16)));
        // Icons got bigger: same cells, different step.
        s.grid = Some((75, 85, 34, 16));
        assert!(s.stale_for((75, 98, 34, 16)));
        assert!(!s.stale_for((75, 85, 34, 16)));
    }

    #[test]
    fn verdicts() {
        let s = st(&[(1, 1), (2, 1), (1, 2), (2, 2), (1, 3), (2, 3)]);
        assert_eq!(
            judge(&s, &[c(1, 1), c(2, 1), c(1, 2), c(2, 2), c(1, 3), c(2, 3)]),
            Verdict::Fine
        );
        // User dragged one icon.
        assert_eq!(
            judge(&s, &[c(1, 1), c(2, 1), c(1, 2), c(2, 2), c(1, 3), c(9, 9)]),
            Verdict::Accept(vec![(5, Cell { col: 9, row: 9 })])
        );
        // Explorer piled everything into the left column.
        assert_eq!(
            judge(&s, &[c(0, 0), c(0, 1), c(0, 2), c(0, 3), c(0, 4), c(2, 3)]),
            Verdict::Restore
        );
        // Removed items don't count.
        assert_eq!(
            judge(&s, &[None, None, None, None, c(1, 3), c(2, 3)]),
            Verdict::Fine
        );
    }

    #[test]
    fn globs() {
        assert!(glob("*.tmp", "Отчёт.TMP"));
        assert!(glob("~$*", "~$отчёт.docx"));
        assert!(glob("desktop.ini", "Desktop.ini"));
        assert!(glob("photo?.png", "photo1.png"));
        assert!(!glob("*.tmp", "tmp.txt"));
        assert!(!glob("photo?.png", "photo12.png"));
    }

    #[test]
    fn downloads_and_renames() {
        let ign: Vec<String> = ["*.tmp", "*.crdownload", "*.part", "~$*", "desktop.ini"]
            .map(String::from)
            .to_vec();
        let mut a = Arrivals::new(["robot_03.png".to_string()]);
        // Chrome: create .crdownload, then rename to the final name.
        assert_eq!(
            a.feed(FsEvent::Created("film.mkv.crdownload".into()), &ign),
            None
        );
        assert_eq!(
            a.feed(FsEvent::RenamedFrom("film.mkv.crdownload".into()), &ign),
            None
        );
        assert_eq!(
            a.feed(FsEvent::RenamedTo("film.mkv".into()), &ign),
            Some("film.mkv".into())
        );
        // Plain rename in place: not new.
        assert_eq!(a.feed(FsEvent::RenamedFrom("a.txt".into()), &ign), None);
        assert_eq!(a.feed(FsEvent::RenamedTo("b.txt".into()), &ign), None);
        // Saved directly / moved in from elsewhere.
        assert_eq!(
            a.feed(FsEvent::Created("Скриншот.png".into()), &ign),
            Some("Скриншот.png".into())
        );
        assert_eq!(
            a.feed(FsEvent::RenamedTo("moved-in.zip".into()), &ign),
            Some("moved-in.zip".into())
        );
        assert_eq!(a.feed(FsEvent::Created("desktop.ini".into()), &ign), None);
        // Explorer re-saves an existing shortcut: temp file, then rename over the old name.
        let mut b = Arrivals::new(["Spotify.lnk".to_string()]);
        assert_eq!(b.feed(FsEvent::Created("~shl1234.tmp".into()), &ign), None);
        assert_eq!(
            b.feed(FsEvent::RenamedFrom("~shl1234.tmp".into()), &ign),
            None
        );
        assert_eq!(
            b.feed(FsEvent::RenamedTo("Spotify.lnk".into()), &ign),
            None,
            "re-save is not new"
        );
        // But a file that was deleted and comes back counts again.
        assert_eq!(b.feed(FsEvent::Removed("Spotify.lnk".into()), &ign), None);
        assert_eq!(
            b.feed(FsEvent::Created("Spotify.lnk".into()), &ign),
            Some("Spotify.lnk".into())
        );
        assert_eq!(a.feed(FsEvent::Created("~$отчёт.docx".into()), &ign), None);
    }
}
