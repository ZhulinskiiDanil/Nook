//! Inbox watcher and layout keeper (spec §6.4–6.5), live desktop only.
//! Positions are never touched in sandbox mode; there the watcher only logs.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode, Watcher};

use super::State;
use crate::keeper::{self, Arrivals, FsEvent, KeeperState, Kept, Verdict};
use crate::layout::engine::{self, Cell, Entry, Group};
use crate::layout::free_zones::blocked_cells;
use crate::shell::desktop_view::{
    DesktopView, ViewItem, cell_of, grid_dims, grid_origin, point_of,
};

pub(super) fn state_file(state: &State) -> PathBuf {
    state.data_dir.join("state.json")
}

/// View snapshot from the UI thread: grid step, origin, cols/rows and items.
struct Snap {
    step: (i32, i32),
    origin: (i32, i32),
    cols: i32,
    rows: i32,
    items: Vec<ViewItem>,
}

fn snap(state: &State) -> Result<Snap> {
    let (info, items) = state.ui.call(|| -> Result<_> {
        let v = DesktopView::open()?;
        Ok((v.info()?, v.items()?))
    })??;
    let mon = crate::platform::PrimaryMonitor::query()?;
    let (cols, rows) = grid_dims(
        mon.work.width(),
        mon.work.height(),
        info.spacing_x,
        info.spacing_y,
    );
    let origin = grid_origin(&items, info.spacing_x, info.spacing_y);
    Ok(Snap {
        step: (info.spacing_x, info.spacing_y),
        origin,
        cols,
        rows,
        items,
    })
}

impl Snap {
    fn grid(&self) -> (i32, i32, i32, i32) {
        (self.step.0, self.step.1, self.cols, self.rows)
    }
    fn cell(&self, v: &ViewItem) -> Cell {
        let (c, r) = cell_of(
            v.x - self.origin.0,
            v.y - self.origin.1,
            self.step.0,
            self.step.1,
        );
        Cell { col: c, row: r }
    }
    fn cells(&self) -> HashMap<String, Cell> {
        self.items
            .iter()
            .map(|v| (v.parsing.to_lowercase(), self.cell(v)))
            .collect()
    }
    fn point(&self, c: Cell) -> (i32, i32) {
        point_of(c.col, c.row, self.step.0, self.step.1, self.origin)
    }
}

fn place(state: &State, targets: HashMap<String, (i32, i32)>) -> Result<usize> {
    state
        .ui
        .call(move || DesktopView::open()?.set_positions(&targets))?
}

// ---------------------------------------------------------------- after apply / undo

/// Remembers the layout Nook just applied so the keeper can hold it.
pub(super) fn remember_applied(
    state: &Arc<State>,
    sp: &crate::plan::model::StoredPlan,
) -> Result<()> {
    if state.desktops.is_sandbox() {
        return Ok(());
    }
    let plan = &sp.plan;
    let scan = state.scan()?;
    let s = snap(state)?;
    let cells = s.cells();
    let mut by_id: HashMap<&str, &crate::scan::Item> =
        scan.items.iter().map(|i| (i.id.as_str(), i)).collect();
    // An id can change if a file was replaced since (e.g. re-created by its app): fall back to
    // the file name the plan saw, if exactly one desktop item still has it.
    for (id, seen) in &sp.seen {
        if by_id.contains_key(id.as_str()) {
            continue;
        }
        let Some(name) = std::path::Path::new(&seen.path)
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
        else {
            continue;
        };
        let matches: Vec<&crate::scan::Item> = scan
            .items
            .iter()
            .filter(|i| i.name.to_lowercase() == name)
            .collect();
        if let [one] = matches.as_slice() {
            by_id.insert(id.as_str(), one);
        }
    }
    let mut kept = Vec::new();
    for (g, entries) in [
        (Group::Apps, &plan.layout.apps),
        (Group::Projects, &plan.layout.projects),
        (Group::Inbox, &plan.layout.inbox),
        (Group::Trash, &plan.layout.trash),
    ] {
        for e in entries {
            let key = match by_id.get(e.item()) {
                Some(i) => i.path.to_lowercase(),
                None => match e.item().strip_prefix(crate::plan::model::NEW_PREFIX) {
                    Some(raw) => crate::paths::expand_env(raw, &state.desktops.user)?
                        .to_string_lossy()
                        .to_lowercase(),
                    None => continue,
                },
            };
            let Some(cell) = cells.get(&key).copied() else {
                continue;
            };
            let id = by_id.get(e.item()).map(|i| i.id.clone());
            kept.push(Kept {
                id,
                key,
                group: g,
                cell,
                pinned: e.pin().is_some(),
            });
        }
    }
    // Anything on the desktop the plan didn't mention (arrived later) belongs to the inbox,
    // so relayouts never hand its cell to someone else.
    let known: HashSet<String> = kept.iter().map(|k| k.key.clone()).collect();
    for v in &s.items {
        let key = v.parsing.to_lowercase();
        if !known.contains(&key) {
            kept.push(Kept {
                id: None,
                key,
                group: Group::Inbox,
                cell: s.cell(v),
                pinned: false,
            });
        }
    }
    let ks = KeeperState {
        items: kept,
        grid: Some(s.grid()),
    };
    keeper::save(&state_file(state), &ks)?;
    *state.keeper.lock().unwrap() = ks;
    Ok(())
}

pub(super) fn forget(state: &State) {
    let ks = KeeperState::default();
    let _ = keeper::save(&state_file(state), &ks);
    *state.keeper.lock().unwrap() = ks;
}

// ---------------------------------------------------------------- keeper

/// One keeper pass: restore after a mass reset, accept the user's manual moves.
pub(super) fn check(state: &State) -> Result<()> {
    if state.desktops.is_sandbox() || state.keeper.lock().unwrap().items.is_empty() {
        return Ok(());
    }
    // Icon size, DPI or resolution changed: old cells mean nothing on the new grid.
    let s = snap(state)?;
    if state.keeper.lock().unwrap().stale_for(s.grid()) {
        tracing::info!(grid = ?s.grid(), "keeper: grid changed (icon size / DPI / resolution)");
        return relayout(state);
    }
    let _g = state.apply_lock.lock().unwrap();
    let cells = s.cells();
    let mut ks = state.keeper.lock().unwrap().clone();
    let current: Vec<Option<Cell>> = ks
        .items
        .iter()
        .map(|k| cells.get(&k.key).copied())
        .collect();
    match keeper::judge(&ks, &current) {
        Verdict::Fine => {}
        Verdict::Restore => {
            let targets = ks
                .items
                .iter()
                .map(|k| (k.key.clone(), s.point(k.cell)))
                .collect();
            let n = place(state, targets)?;
            tracing::info!(
                restored = n,
                "keeper: Explorer reset icon positions; restored the layout"
            );
        }
        Verdict::Accept(moved) => {
            for (i, cell) in moved {
                tracing::info!(item = %ks.items[i].key, ?cell, "keeper: user moved an icon; keeping it there");
                ks.items[i].cell = cell;
                ks.items[i].pinned = true;
            }
            keeper::save(&state_file(state), &ks)?;
            *state.keeper.lock().unwrap() = ks;
        }
    }
    Ok(())
}

/// Resolution / monitor / DPI changed: recompute the layout on the new grid and re-place.
pub(super) fn relayout(state: &State) -> Result<()> {
    if state.desktops.is_sandbox() || state.keeper.lock().unwrap().items.is_empty() {
        return Ok(());
    }
    let _g = state.apply_lock.lock().unwrap();
    let s = snap(state)?;
    let mut ks = state.keeper.lock().unwrap().clone();
    let present: HashSet<String> = s.items.iter().map(|v| v.parsing.to_lowercase()).collect();
    let mut groups: std::collections::BTreeMap<Group, Vec<Entry>> =
        std::collections::BTreeMap::new();
    for (g, idx) in keeper::groups_of(&ks) {
        for i in idx {
            let k = &ks.items[i];
            if present.contains(&k.key) {
                let pin =
                    (k.pinned && k.cell.col < s.cols && k.cell.row < s.rows).then_some(k.cell);
                groups.entry(g).or_default().push(Entry {
                    id: i.to_string(),
                    pin,
                });
            }
        }
    }
    let input = engine::Input {
        cols: s.cols,
        rows: s.rows,
        blocked: blocked_cells(
            s.cols as usize,
            s.rows as usize,
            &state.config.layout.keep_free,
        ),
        groups,
    };
    let out = engine::layout(&input, &state.config.layout);
    let mut targets = HashMap::new();
    for (i, cell) in &out.cells {
        let i: usize = i.parse()?;
        ks.items[i].cell = *cell;
        targets.insert(ks.items[i].key.clone(), s.point(*cell));
    }
    let n = place(state, targets)?;
    tracing::info!(
        placed = n,
        cols = s.cols,
        rows = s.rows,
        "keeper: display changed; layout recomputed"
    );
    ks.grid = Some(s.grid());
    keeper::save(&state_file(state), &ks)?;
    *state.keeper.lock().unwrap() = ks;
    Ok(())
}

/// Periodic keeper (cheap: one view query every 15 s).
pub(super) fn spawn_keeper(state: Arc<State>) {
    let _ = std::thread::Builder::new()
        .name("keeper".into())
        .spawn(move || {
            let _com = crate::platform::ComApartment::sta();
            loop {
                std::thread::sleep(Duration::from_secs(15));
                if let Err(e) = check(&state) {
                    tracing::debug!("keeper check: {e:#}");
                }
            }
        });
}

// ---------------------------------------------------------------- inbox

/// Puts a newly arrived item into the next free inbox cell once it shows up in the view.
/// Runs serially (one placement at a time) and looks at the desktop under the apply lock,
/// so two arrivals never get the same cell.
fn place_in_inbox(state: &State, path: PathBuf) -> Result<()> {
    let key = path.to_string_lossy().to_lowercase();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if snap(state)?
            .items
            .iter()
            .any(|v| v.parsing.to_lowercase() == key)
        {
            break;
        }
        if Instant::now() > deadline {
            anyhow::bail!(
                "{} did not appear in the desktop view within 3 s",
                path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let _g = state.apply_lock.lock().unwrap();
    let s = snap(state)?;
    let occupied: HashSet<Cell> = s
        .items
        .iter()
        .filter(|v| v.parsing.to_lowercase() != key)
        .map(|v| s.cell(v))
        .collect();
    let input = engine::Input {
        cols: s.cols,
        rows: s.rows,
        blocked: blocked_cells(
            s.cols as usize,
            s.rows as usize,
            &state.config.layout.keep_free,
        ),
        groups: Default::default(),
    };
    let corner = Group::Inbox.corner(&state.config.layout);
    let cell = engine::order(Group::Inbox, corner, &state.config.layout, &input)
        .into_iter()
        .find(|c| !occupied.contains(c))
        .context("inbox is full")?;
    place(state, [(key.clone(), s.point(cell))].into())?;
    tracing::info!(file = %path.display(), ?cell, "inbox: placed new item");
    let mut ks = state.keeper.lock().unwrap().clone();
    if !ks.items.is_empty() {
        ks.items.retain(|k| k.key != key);
        ks.items.push(Kept {
            id: None,
            key,
            group: Group::Inbox,
            cell,
            pinned: false,
        });
        keeper::save(&state_file(state), &ks)?;
        *state.keeper.lock().unwrap() = ks;
    }
    Ok(())
}

fn desktop_names(state: &State) -> Vec<String> {
    [&state.desktops.user, &state.desktops.public]
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Watches user + public desktop folders; genuinely new items go to the inbox corner.
pub(super) fn spawn_inbox(state: Arc<State>) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx)?;
    for d in [&state.desktops.user, &state.desktops.public] {
        if d.is_dir() {
            watcher
                .watch(d, RecursiveMode::NonRecursive)
                .with_context(|| format!("watch {}", d.display()))?;
        }
    }
    // One placement worker: arrivals are placed strictly one after another.
    let (place_tx, place_rx) = std::sync::mpsc::channel::<PathBuf>();
    {
        let st = state.clone();
        std::thread::Builder::new()
            .name("inbox-place".into())
            .spawn(move || {
                let _com = crate::platform::ComApartment::sta();
                for path in place_rx {
                    if let Err(e) = place_in_inbox(&st, path) {
                        tracing::warn!("inbox: {e:#}");
                    }
                }
            })?;
    }
    std::thread::Builder::new().name("inbox".into()).spawn(move || {
        let _keep_alive = watcher;
        let mut arrivals = Arrivals::new(desktop_names(&state));
        for res in rx {
            let Ok(ev) = res else { continue };
            let name = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().into_owned());
            let each = |f: fn(String) -> FsEvent| -> Vec<(FsEvent, PathBuf)> {
                ev.paths.iter().filter_map(|p| Some((f(name(p)?), p.clone()))).collect()
            };
            let simple: Vec<(FsEvent, PathBuf)> = match ev.kind {
                EventKind::Create(_) => each(FsEvent::Created),
                EventKind::Remove(_) => each(FsEvent::Removed),
                EventKind::Modify(ModifyKind::Name(RenameMode::From)) => each(FsEvent::RenamedFrom),
                EventKind::Modify(ModifyKind::Name(RenameMode::To)) => each(FsEvent::RenamedTo),
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if ev.paths.len() == 2 => {
                    let (a, b) = (&ev.paths[0], &ev.paths[1]);
                    match (name(a), name(b)) {
                        (Some(x), Some(y)) => vec![(FsEvent::RenamedFrom(x), a.clone()), (FsEvent::RenamedTo(y), b.clone())],
                        _ => vec![],
                    }
                }
                _ => vec![],
            };
            for (fe, path) in simple {
                let Some(n) = arrivals.feed(fe, &state.config.inbox.ignore) else { continue };
                // A plan apply moves/renames files on purpose; those are not arrivals.
                if state.applying.load(Ordering::Relaxed) {
                    continue;
                }
                if !state.config.inbox.enabled || state.pause_inbox.load(Ordering::Relaxed) {
                    tracing::info!(file = %n, "inbox: paused, leaving it where it is");
                    continue;
                }
                if state.desktops.is_sandbox() {
                    tracing::info!(file = %n, "inbox (sandbox): would place in the inbox corner");
                    continue;
                }
                let _ = place_tx.send(path);
            }
        }
    })?;
    Ok(())
}
