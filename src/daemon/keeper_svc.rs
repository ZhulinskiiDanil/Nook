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
    plan: &crate::plan::model::PlanInput,
) -> Result<()> {
    if state.desktops.is_sandbox() {
        return Ok(());
    }
    let scan = state.scan()?;
    let s = snap(state)?;
    let cells = s.cells();
    let by_id: HashMap<&str, &crate::scan::Item> =
        scan.items.iter().map(|i| (i.id.as_str(), i)).collect();
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
    let ks = KeeperState { items: kept };
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
    let _g = state.apply_lock.lock().unwrap();
    let s = snap(state)?;
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
fn place_in_inbox(state: &State, path: PathBuf) -> Result<()> {
    let key = path.to_string_lossy().to_lowercase();
    let deadline = Instant::now() + Duration::from_secs(3);
    let s = loop {
        let s = snap(state)?;
        if s.items.iter().any(|v| v.parsing.to_lowercase() == key) {
            break s;
        }
        if Instant::now() > deadline {
            anyhow::bail!(
                "{} did not appear in the desktop view within 3 s",
                path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    };
    let _g = state.apply_lock.lock().unwrap();
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

/// Watches user + public desktop folders; new arrivals go to the inbox corner.
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
    std::thread::Builder::new().name("inbox".into()).spawn(move || {
        let _keep_alive = watcher;
        let _com = crate::platform::ComApartment::sta();
        let mut arrivals = Arrivals::default();
        for res in rx {
            let Ok(ev) = res else { continue };
            let name = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().into_owned());
            let simple: Vec<(FsEvent, PathBuf)> = match ev.kind {
                EventKind::Create(_) => ev.paths.iter().filter_map(|p| Some((FsEvent::Created(name(p)?), p.clone()))).collect(),
                EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                    ev.paths.iter().filter_map(|p| Some((FsEvent::RenamedFrom(name(p)?), p.clone()))).collect()
                }
                EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                    ev.paths.iter().filter_map(|p| Some((FsEvent::RenamedTo(name(p)?), p.clone()))).collect()
                }
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
                if !state.config.inbox.enabled || state.pause_inbox.load(Ordering::Relaxed) {
                    tracing::info!(file = %n, "inbox: paused, leaving it where it is");
                    continue;
                }
                if state.desktops.is_sandbox() {
                    tracing::info!(file = %n, "inbox (sandbox): would place in the inbox corner");
                    continue;
                }
                let st = state.clone();
                // A plan apply moves files around on purpose; don't treat those as arrivals.
                if st.applying.load(Ordering::Relaxed) {
                    continue;
                }
                let _ = std::thread::Builder::new().name("inbox-place".into()).spawn(move || {
                    let _com = crate::platform::ComApartment::sta();
                    if let Err(e) = place_in_inbox(&st, path) {
                        tracing::warn!("inbox: {e:#}");
                    }
                });
            }
        }
    })?;
    Ok(())
}
