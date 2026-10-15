//! Propose, apply (write-ahead journal, rollback on failure), undo, quarantine.
//! Nothing is ever deleted: undo "parks" things Nook created in the plan's quarantine folder.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use crate::config::{Config, IconStyle};
use crate::icons::Shape;
use crate::layout::engine::{self, Cell, Entry, Group};
use crate::layout::free_zones::blocked_cells;
use crate::paths::Desktops;
use crate::plan::fsops::{
    ATTR_HIDDEN, ATTR_NORMAL, ATTR_READONLY, ATTR_SYSTEM, get_attrs, is_busy, move_path,
    notify_dir, notify_item, set_attrs,
};
use crate::plan::journal::{self, Journal, Mark, Step};
use crate::plan::model::{
    Action, ExplorerKey, NEW_PREFIX, PlanInput, PlanState, Seen, StepResult, StoredPlan,
};
use crate::plan::store::Store;
use crate::plan::validate::{Checked, Ctx, free_name, validate};
use crate::scan::{Item, ScanResult};
use crate::timefmt::rfc3339;

/// Desktop view operations (icon positions, auto-arrange). No-ops in sandbox mode.
pub trait Desk: Send + Sync {
    fn live(&self) -> bool;
    /// Saves current positions; returns the snapshot file.
    fn snapshot_positions(&self) -> Result<Option<PathBuf>>;
    fn restore_positions(&self, file: &Path) -> Result<()>;
    /// Grid columns × rows of the work area.
    fn grid(&self) -> Option<(i32, i32)>;
    /// Places items (lowercase path or `::{CLSID}` → cell).
    fn place(&self, cells: &HashMap<String, Cell>) -> Result<usize>;
    fn auto_arrange(&self) -> Result<bool>;
    fn set_auto_arrange(&self, on: bool) -> Result<()>;
}

/// Sandbox: positions are never touched (spec §10).
pub struct NoDesk;

impl Desk for NoDesk {
    fn live(&self) -> bool {
        false
    }
    fn snapshot_positions(&self) -> Result<Option<PathBuf>> {
        Ok(None)
    }
    fn restore_positions(&self, _: &Path) -> Result<()> {
        Ok(())
    }
    fn grid(&self) -> Option<(i32, i32)> {
        None
    }
    fn place(&self, _: &HashMap<String, Cell>) -> Result<usize> {
        Ok(0)
    }
    fn auto_arrange(&self) -> Result<bool> {
        Ok(false)
    }
    fn set_auto_arrange(&self, _: bool) -> Result<()> {
        Ok(())
    }
}

pub struct Env<'a> {
    pub desktops: &'a Desktops,
    pub config: &'a Config,
    pub store: &'a Store,
    pub desk: &'a dyn Desk,
    /// Explorer shows hidden files.
    pub shows_hidden: bool,
    /// Test hook: fail at this step number (rollback test).
    pub fail_at_step: Option<usize>,
    /// Where generated `.ico` files go (`%LOCALAPPDATA%\Nook\icons`).
    pub icons_dir: PathBuf,
}

impl Env<'_> {
    pub fn quarantine_root(&self) -> Result<PathBuf> {
        match &self.desktops.mode {
            crate::paths::DesktopMode::Sandbox(root) => Ok(root.join("quarantine")),
            crate::paths::DesktopMode::Live => {
                crate::paths::expand_env(&self.config.safety.quarantine_dir, &self.desktops.user)
            }
        }
    }

    fn ctx<'b>(
        &'b self,
        items: &'b [Item],
        env: &'b dyn Fn(&str) -> Option<String>,
    ) -> Result<Ctx<'b>> {
        let allowed_roots = self
            .config
            .safety
            .allowed_roots
            .iter()
            .map(|r| crate::paths::expand_env(r, &self.desktops.user))
            .collect::<Result<Vec<_>>>()?;
        Ok(Ctx {
            items,
            desktop: self.desktops.user.clone(),
            allowed_roots,
            shows_hidden: self.shows_hidden,
            icons_enabled: self.config.icons.enabled,
            env,
        })
    }
}

fn os_env(n: &str) -> Option<String> {
    std::env::var(n).ok()
}

fn seen_of(i: &Item) -> Seen {
    Seen {
        path: i.path.clone(),
        size: i.size,
        modified: i.modified.clone(),
        file_id: i
            .fingerprint
            .map(|f| f.file_id.iter().map(|b| format!("{b:02x}")).collect()),
    }
}

#[derive(Debug, Serialize)]
pub struct Proposal {
    pub plan_id: String,
    pub summary: String,
    pub warnings: Vec<String>,
    pub superseded: Option<String>,
    pub sandbox: bool,
}

/// Validates and stores a plan as pending; a previous pending plan becomes `superseded`.
pub fn propose(
    input: PlanInput,
    scan: &ScanResult,
    env: &Env,
) -> Result<std::result::Result<Proposal, Vec<String>>> {
    let ctx = env.ctx(&scan.items, &os_env)?;
    let checked = match validate(&input, &ctx) {
        Ok(c) => c,
        Err(errors) => return Ok(Err(errors)),
    };
    let mut superseded = None;
    if let Some(mut old) = env.store.pending() {
        old.state = PlanState::Superseded;
        env.store.save(&old)?;
        superseded = Some(old.id);
    }
    let referenced: HashSet<&str> = input
        .layout
        .groups()
        .iter()
        .flat_map(|(_, v)| v.iter().map(|e| e.item()))
        .chain(input.actions.iter().filter_map(|a| a.item()))
        .collect();
    let seen = scan
        .items
        .iter()
        .filter(|i| referenced.contains(i.id.as_str()))
        .map(|i| (i.id.clone(), seen_of(i)))
        .collect();
    let id = Store::new_id();
    let summary = format!("{} → {}", checked.before, checked.after);
    let sp = StoredPlan {
        id: id.clone(),
        created: rfc3339(SystemTime::now()),
        state: PlanState::Pending,
        sandbox: env.desktops.is_sandbox(),
        plan: input,
        summary: summary.clone(),
        before: checked.before,
        after: checked.after,
        warnings: checked.warnings.clone(),
        seen,
        results: vec![],
        applied_at: None,
        previewed: false,
    };
    env.store.save(&sp)?;
    Ok(Ok(Proposal {
        plan_id: id,
        summary,
        warnings: checked.warnings,
        superseded,
        sandbox: sp.sandbox,
    }))
}

/// Invisible name for `hide_caption`: N × U+2800 + extension; N picks a free name.
fn invisible_name(dir: &Path, public: &Path, ext: &str, taken: &HashSet<String>) -> PathBuf {
    (1..)
        .map(|n| format!("{}.{ext}", "\u{2800}".repeat(n)))
        .find(|name| {
            let l = name.to_lowercase();
            !taken.contains(&l) && !dir.join(name).exists() && !public.join(name).exists()
        })
        .map(|n| dir.join(n))
        .unwrap()
}

struct Run<'a> {
    env: &'a Env<'a>,
    journal: Journal,
    results: Vec<StepResult>,
    step_no: usize,
    park: PathBuf,
}

enum Outcome {
    Done,
    Skipped(String),
}

impl Run<'_> {
    /// Journals intent → runs → journals result. Busy files are skipped, other errors abort.
    fn step(
        &mut self,
        op: &str,
        item: Option<&str>,
        step: Step,
        f: impl FnOnce() -> Result<Outcome>,
    ) -> Result<bool> {
        self.step_no += 1;
        let seq = self.journal.intent(&step)?;
        let r = if self.env.fail_at_step == Some(self.step_no) {
            Err(anyhow!("injected failure (test)"))
        } else {
            f()
        };
        let (mark, status, detail, ok) = match r {
            Ok(Outcome::Done) => (Mark::Done, "done", String::new(), true),
            Ok(Outcome::Skipped(why)) => (Mark::Skipped, "skipped", why, false),
            Err(e) if is_busy(&e) => (Mark::Skipped, "skipped", format!("{e:#}"), false),
            Err(e) => {
                self.journal
                    .close(seq, Mark::Failed, &step, format!("{e:#}"))?;
                self.results.push(StepResult {
                    step: self.step_no,
                    op: op.into(),
                    item: item.map(String::from),
                    status: "failed".into(),
                    detail: format!("{e:#}"),
                });
                return Err(e);
            }
        };
        self.journal.close(seq, mark, &step, detail.clone())?;
        self.results.push(StepResult {
            step: self.step_no,
            op: op.into(),
            item: item.map(String::from),
            status: status.into(),
            detail,
        });
        Ok(ok)
    }
}

fn explorer_reg(key: ExplorerKey, value: bool) -> Option<(&'static str, &'static str, u32)> {
    match key {
        ExplorerKey::HideProtectedOsFiles => Some((
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced",
            "ShowSuperHidden",
            u32::from(!value),
        )),
        ExplorerKey::HideThisPcIcon => Some((
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel",
            "{20D04FE0-3AEA-1069-A2D8-08002B30309D}",
            u32::from(value),
        )),
        ExplorerKey::AutoArrangeOff => None,
    }
}

fn desktop_ini(icon: &str) -> Vec<u8> {
    let text = format!("[.ShellClassInfo]\r\nIconResource={icon}\r\n");
    // UTF-16LE with BOM: Explorer reads Unicode desktop.ini (Cyrillic paths).
    let mut out = vec![0xFF, 0xFE];
    out.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    out
}

/// Applies a pending plan. Returns the stored plan with results (state Applied or Failed).
pub fn apply(id: &str, scan: &ScanResult, env: &Env) -> Result<StoredPlan> {
    let mut sp = env.store.load(id)?;
    if sp.state != PlanState::Pending {
        bail!("plan {id} is {:?}, not pending", sp.state);
    }
    if sp.sandbox != env.desktops.is_sandbox() {
        bail!(
            "plan {id} was made for {} mode",
            if sp.sandbox { "sandbox" } else { "live" }
        );
    }
    // Items must not have changed since the proposal.
    let now: HashMap<&str, &Item> = scan.items.iter().map(|i| (i.id.as_str(), i)).collect();
    let mut stale = Vec::new();
    for (iid, s) in &sp.seen {
        match now.get(iid.as_str()) {
            Some(i) if seen_of(i) == *s => {}
            Some(i) => stale.push(format!("{} changed", i.name)),
            None => stale.push(format!("{} is gone", s.path)),
        }
    }
    if !stale.is_empty() {
        bail!(
            "desktop changed since the plan was made — rescan and propose again: {}",
            stale.join("; ")
        );
    }
    // Re-validate against the current state.
    let ctx = env.ctx(&scan.items, &os_env)?;
    let checked =
        validate(&sp.plan, &ctx).map_err(|e| anyhow!("plan no longer valid: {}", e.join("; ")))?;

    let qroot = env.quarantine_root()?;
    let park = qroot.join(&sp.id).join("_undo");
    let mut run = Run {
        env,
        journal: Journal::open(&env.store.journal, &sp.id)?,
        results: vec![],
        step_no: 0,
        park,
    };
    let res = execute(&sp, &checked, scan, &mut run, &qroot);
    sp.results = std::mem::take(&mut run.results);
    match res {
        Ok(()) => {
            sp.state = PlanState::Applied;
            sp.applied_at = Some(rfc3339(SystemTime::now()));
            env.store.save(&sp)?;
            Ok(sp)
        }
        Err(e) => {
            tracing::error!(plan = %sp.id, "apply failed, rolling back: {e:#}");
            let rb = rollback(&sp.id, env);
            sp.state = PlanState::Failed;
            sp.results.push(StepResult {
                step: 0,
                op: "rollback".into(),
                item: None,
                status: if rb.is_ok() {
                    "done".into()
                } else {
                    "failed".into()
                },
                detail: format!(
                    "cause: {e:#}; rollback: {}",
                    rb.as_ref()
                        .map_or_else(|e| format!("{e:#}"), |n| format!("{n} step(s) undone"))
                ),
            });
            env.store.save(&sp)?;
            Ok(sp)
        }
    }
}

fn execute(
    sp: &StoredPlan,
    checked: &Checked,
    scan: &ScanResult,
    run: &mut Run,
    qroot: &Path,
) -> Result<()> {
    let env = run.env;
    let by_id: HashMap<&str, &Item> = scan.items.iter().map(|i| (i.id.as_str(), i)).collect();
    // Current path per ref (changes with renames/moves).
    let mut cur: HashMap<String, PathBuf> = scan
        .items
        .iter()
        .map(|i| (i.id.clone(), PathBuf::from(&i.path)))
        .collect();
    let mut names: HashSet<String> = scan.items.iter().map(|i| i.name.to_lowercase()).collect();
    let mut touched_dirs: HashSet<PathBuf> = HashSet::new();

    // 1. Positions snapshot (live only).
    if env.desk.live()
        && let Some(snap) = env.desk.snapshot_positions()?
    {
        run.step(
            "positions",
            None,
            Step::Positions { snapshot: snap },
            || Ok(Outcome::Done),
        )?;
    }

    // 2. New folders on the desktop.
    for (r, path) in new_refs(&sp.plan, checked) {
        let park = run.park.join(path.file_name().unwrap());
        run.step(
            "create_folder",
            Some(&r),
            Step::CreateDir {
                path: path.clone(),
                park,
            },
            || {
                std::fs::create_dir(&path).with_context(|| format!("create {}", path.display()))?;
                Ok(Outcome::Done)
            },
        )?;
        names.insert(path.file_name().unwrap().to_string_lossy().to_lowercase());
        cur.insert(r, path);
    }

    // 3. Explorer settings.
    let mut settings_changed = false;
    for a in &sp.plan.actions {
        let Action::ExplorerSetting { key, value, .. } = a else {
            continue;
        };
        let sim = !env.desk.live();
        match explorer_reg(*key, *value) {
            Some((k, v, new)) => {
                let old = crate::shell::explorer_settings::hkcu_dword(k, v);
                let step = Step::Registry {
                    key: k.into(),
                    value: v.into(),
                    old,
                    new,
                    simulated: sim,
                };
                run.step("explorer_setting", None, step, || {
                    if sim {
                        return Ok(Outcome::Skipped(
                            "sandbox: registry not touched (simulated)".into(),
                        ));
                    }
                    crate::winreg::hkcu_set_dword(k, v, new)?;
                    Ok(Outcome::Done)
                })?;
                settings_changed |= !sim;
            }
            None => {
                let on = !*value;
                let old = env.desk.auto_arrange()?;
                run.step(
                    "explorer_setting",
                    None,
                    Step::AutoArrange {
                        old,
                        new: on,
                        simulated: sim,
                    },
                    || {
                        if sim {
                            return Ok(Outcome::Skipped(
                                "sandbox: view not touched (simulated)".into(),
                            ));
                        }
                        env.desk.set_auto_arrange(on)?;
                        Ok(Outcome::Done)
                    },
                )?;
            }
        }
    }
    if settings_changed {
        crate::winreg::broadcast_shell_settings();
    }

    // 4. In-place actions first (rename, hide_caption, dim, icons), then moves.
    let (in_place, moves): (Vec<&Action>, Vec<&Action>) = sp
        .plan
        .actions
        .iter()
        .filter(|a| !matches!(a, Action::ExplorerSetting { .. }))
        .partition(|a| !a.removes_from_desktop());
    for a in in_place.into_iter().chain(moves) {
        let r = a.item().unwrap_or_default().to_string();
        let Some(path) = cur.get(&r).cloned() else {
            bail!("{r}: path unknown")
        };
        let dir = path.parent().context("no parent")?.to_path_buf();
        match a {
            Action::Rename { new_name, .. } => {
                let to = free_name(&dir, new_name, &|p| p.exists());
                let step = Step::Move {
                    item: Some(r.clone()),
                    from: path.clone(),
                    to: to.clone(),
                    why: "rename".into(),
                };
                if run.step("rename", Some(&r), step, || {
                    move_path(&path, &to).map(|_| Outcome::Done)
                })? {
                    names.insert(to.file_name().unwrap().to_string_lossy().to_lowercase());
                    cur.insert(r, to);
                }
            }
            Action::HideCaption { .. } => {
                let ext = path
                    .extension()
                    .map(|e| e.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "lnk".into());
                let to = invisible_name(&dir, &env.desktops.public, &ext, &names);
                let step = Step::Move {
                    item: Some(r.clone()),
                    from: path.clone(),
                    to: to.clone(),
                    why: "hide_caption".into(),
                };
                if run.step("hide_caption", Some(&r), step, || {
                    move_path(&path, &to).map(|_| Outcome::Done)
                })? {
                    names.insert(to.file_name().unwrap().to_string_lossy().to_lowercase());
                    cur.insert(r, to);
                }
            }
            Action::Dim { value: true, .. }
                if env.config.icons.enabled && styled_kind(&r, &by_id).is_some() =>
            {
                // Styled items dim via their icon (no Hidden attribute needed).
                let kind = styled_kind(&r, &by_id).unwrap();
                let look = look_for(&sp.plan, &r, a, env.config).context("no style")?;
                let base = base_from(&r, &path, kind, &by_id, &run.env.store.journal);
                restyle(run, &r, &path, kind, look, &base)?;
            }
            Action::Dim { value, .. } => {
                let old = get_attrs(&path)?;
                let new = if *value {
                    old | ATTR_HIDDEN
                } else {
                    old & !ATTR_HIDDEN
                };
                run.step(
                    "dim",
                    Some(&r),
                    Step::SetAttrs {
                        path: path.clone(),
                        old,
                        new,
                    },
                    || {
                        set_attrs(&path, new)?;
                        notify_item(&path);
                        Ok(Outcome::Done)
                    },
                )?;
            }
            Action::RestyleIcon { .. } => {
                let kind = styled_kind(&r, &by_id).context("item cannot be restyled")?;
                let look = look_for(&sp.plan, &r, a, env.config).context("unknown style")?;
                let base = base_from(&r, &path, kind, &by_id, &run.env.store.journal);
                restyle(run, &r, &path, kind, look, &base)?;
            }
            Action::FolderIcon { icon, .. } => {
                let icon_res = if icon == "default" {
                    let sys = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
                    format!(r"{sys}\System32\shell32.dll,4")
                } else {
                    format!(
                        "{},0",
                        crate::paths::expand_env(icon, &env.desktops.user)?.display()
                    )
                };
                set_folder_icon(run, &r, &path, &icon_res)?;
            }
            Action::MoveToFolder { dest, .. } => {
                let d = crate::paths::expand_env(dest, &env.desktops.user)?;
                // Create missing folders one level at a time (journaled, parked on undo).
                let mut missing = Vec::new();
                let mut p = d.clone();
                while !p.exists() {
                    missing.push(p.clone());
                    p = p
                        .parent()
                        .context("dest has no existing ancestor")?
                        .to_path_buf();
                }
                for m in missing.into_iter().rev() {
                    let park = run.park.join(m.file_name().unwrap());
                    run.step(
                        "create_folder",
                        Some(&r),
                        Step::CreateDir {
                            path: m.clone(),
                            park,
                        },
                        || {
                            std::fs::create_dir(&m)?;
                            Ok(Outcome::Done)
                        },
                    )?;
                }
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let to = free_name(&d, &name, &|p| p.exists());
                let step = Step::Move {
                    item: Some(r.clone()),
                    from: path.clone(),
                    to: to.clone(),
                    why: "move_to_folder".into(),
                };
                if run.step("move_to_folder", Some(&r), step, || {
                    move_path(&path, &to).map(|_| Outcome::Done)
                })? {
                    touched_dirs.insert(d);
                    cur.remove(&r);
                }
            }
            Action::Quarantine { .. } => {
                let qdir = qroot.join(&sp.id);
                std::fs::create_dir_all(&qdir)
                    .with_context(|| format!("create {}", qdir.display()))?;
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let to = free_name(&qdir, &name, &|p| p.exists());
                let step = Step::Move {
                    item: Some(r.clone()),
                    from: path.clone(),
                    to: to.clone(),
                    why: "quarantine".into(),
                };
                if run.step("quarantine", Some(&r), step, || {
                    move_path(&path, &to).map(|_| Outcome::Done)
                })? {
                    cur.remove(&r);
                }
            }
            Action::ExplorerSetting { .. } => {}
        }
        touched_dirs.insert(dir);
    }

    // 5. Layout (live only): engine over the post-plan desktop.
    if env.desk.live()
        && let Some((cols, rows)) = env.desk.grid()
    {
        let cells = plan_cells(&sp.plan, checked, &by_id, &cur, cols, rows, env.config);
        if !cells.is_empty() {
            let placed = env.desk.place(&cells)?;
            run.step(
                "layout",
                None,
                Step::Positions {
                    snapshot: PathBuf::new(),
                },
                || Ok(Outcome::Done),
            )?;
            tracing::info!(placed, "layout applied");
        }
    }
    for d in touched_dirs {
        notify_dir(&d);
    }
    // New icons only show once Explorer's icon cache lets go of the old pictures.
    let icon_paths: Vec<PathBuf> = sp
        .plan
        .actions
        .iter()
        .filter(|a| {
            matches!(
                a,
                Action::RestyleIcon { .. } | Action::FolderIcon { .. } | Action::Dim { .. }
            )
        })
        .filter_map(|a| cur.get(a.item()?).cloned())
        .collect();
    if !icon_paths.is_empty() && env.desk.live() {
        crate::plan::fsops::refresh_icons(&icon_paths);
    }
    Ok(())
}

fn new_refs(plan: &PlanInput, checked: &Checked) -> Vec<(String, PathBuf)> {
    let refs: Vec<String> = plan
        .layout
        .groups()
        .iter()
        .flat_map(|(_, v)| v.iter().map(|e| e.item().to_string()))
        .filter(|r| r.starts_with(NEW_PREFIX))
        .collect();
    refs.into_iter()
        .filter_map(|r| {
            let name = r.rsplit(['\\', '/']).next()?.to_lowercase();
            let p = checked.new_folders.iter().find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().to_lowercase() == name)
            })?;
            Some((r, p.clone()))
        })
        .collect()
}

/// Layout cells for the post-plan desktop, keyed by lowercase path / `::{CLSID}`.
pub fn plan_cells(
    plan: &PlanInput,
    checked: &Checked,
    by_id: &HashMap<&str, &Item>,
    cur: &HashMap<String, PathBuf>,
    cols: i32,
    rows: i32,
    config: &Config,
) -> HashMap<String, Cell> {
    let _ = checked;
    let mut groups: BTreeMap<Group, Vec<Entry>> = BTreeMap::new();
    for (g, entries) in [
        (Group::Apps, &plan.layout.apps),
        (Group::Projects, &plan.layout.projects),
        (Group::Inbox, &plan.layout.inbox),
        (Group::Trash, &plan.layout.trash),
    ] {
        for e in entries {
            groups.entry(g).or_default().push(Entry {
                id: e.item().to_string(),
                pin: e.pin().map(|p| Cell {
                    col: p.col,
                    row: p.row,
                }),
            });
        }
    }
    let input = engine::Input {
        cols,
        rows,
        blocked: blocked_cells(cols as usize, rows as usize, &config.layout.keep_free),
        groups,
    };
    let out = engine::layout(&input, &config.layout);
    out.cells
        .into_iter()
        .filter_map(|(r, c)| {
            let key = match by_id.get(r.as_str()) {
                Some(i) if i.kind == crate::scan::Kind::Virtual => i.path.to_lowercase(),
                _ => cur.get(&r)?.to_string_lossy().to_lowercase(),
            };
            Some((key, c))
        })
        .collect()
}

/// Reverts one journaled step.
fn revert(step: &Step, env: &Env) -> Result<String> {
    match step {
        Step::CreateDir { path, park } => {
            if !path.exists() {
                return Ok("already gone".into());
            }
            if std::fs::read_dir(path)?.next().is_some() {
                return Ok(format!("{} is not empty; left in place", path.display()));
            }
            std::fs::create_dir_all(park.parent().unwrap())?;
            let to = free_name(
                park.parent().unwrap(),
                &park.file_name().unwrap().to_string_lossy(),
                &|p| p.exists(),
            );
            move_path(path, &to)?;
            Ok(format!("parked in {}", to.display()))
        }
        Step::Move { from, to, .. } => {
            if !to.exists() {
                return Ok(format!(
                    "{} no longer exists (restored earlier?)",
                    to.display()
                ));
            }
            let back = free_name(
                from.parent().unwrap(),
                &from.file_name().unwrap().to_string_lossy(),
                &|p| p.exists(),
            );
            move_path(to, &back)?;
            notify_dir(from.parent().unwrap());
            Ok(String::new())
        }
        Step::SetAttrs { path, old, .. } => {
            set_attrs(path, *old)?;
            notify_item(path);
            Ok(String::new())
        }
        Step::WriteFile {
            path,
            backup,
            park,
            old_attrs,
        } => {
            if path.exists() {
                std::fs::create_dir_all(park.parent().unwrap())?;
                set_attrs(path, ATTR_NORMAL)?;
                let to = free_name(
                    park.parent().unwrap(),
                    &park.file_name().unwrap().to_string_lossy(),
                    &|p| p.exists(),
                );
                move_path(path, &to)?;
            }
            if let Some(b) = backup
                && b.exists()
            {
                move_path(b, path)?;
                if let Some(a) = old_attrs {
                    set_attrs(path, *a)?;
                }
            }
            notify_item(path.parent().unwrap());
            Ok(String::new())
        }
        Step::ModifyFile { path, backup, park } => {
            if !backup.exists() {
                return Ok("no backup (step never ran)".into());
            }
            if !path.exists() {
                move_path(backup, path)?;
                notify_item(path);
                return Ok("file was gone; original put back".into());
            }
            // Write the original bytes back INTO the same file: its FileId (Nook's identity
            // for the item) stays the same. The changed version is kept in the park folder.
            let original = std::fs::read(backup)?;
            let meta = std::fs::metadata(backup)?;
            std::fs::create_dir_all(park.parent().unwrap())?;
            let keep = free_name(
                park.parent().unwrap(),
                &park.file_name().unwrap().to_string_lossy(),
                &|p| p.exists(),
            );
            std::fs::copy(path, &keep)?;
            {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(path)?;
                f.write_all(&original)?;
                let mut t = std::fs::FileTimes::new();
                if let Ok(m) = meta.modified() {
                    t = t.set_modified(m);
                }
                let _ = f.set_times(t);
            }
            notify_item(path);
            Ok(String::new())
        }
        Step::Registry {
            key,
            value,
            old,
            simulated,
            ..
        } => {
            if *simulated {
                return Ok("simulated".into());
            }
            match old {
                Some(v) => crate::winreg::hkcu_set_dword(key, value, *v)?,
                None => crate::winreg::hkcu_remove_value(key, value)?,
            }
            crate::winreg::broadcast_shell_settings();
            Ok(String::new())
        }
        Step::AutoArrange { old, simulated, .. } => {
            if !*simulated {
                env.desk.set_auto_arrange(*old)?;
            }
            Ok(String::new())
        }
        Step::Positions { snapshot } => {
            if !snapshot.as_os_str().is_empty() {
                env.desk.restore_positions(snapshot)?;
            }
            Ok(String::new())
        }
    }
}

/// Undoes every `done` step of a plan, newest first. Returns how many were undone.
fn rollback(id: &str, env: &Env) -> Result<usize> {
    let jpath = env.store.journal_file(id);
    let lines = journal::read(&jpath)?;
    let mut j = Journal::open(&env.store.journal, id)?;
    let mut n = 0;
    let mut errors = Vec::new();
    // Positions last: files first, then put icons back where they were.
    let (pos, rest): (Vec<_>, Vec<_>) = journal::done_steps(&lines)
        .into_iter()
        .partition(|(_, s)| matches!(s, Step::Positions { .. }));
    let mut icon_paths = Vec::new();
    for (seq, step) in rest.into_iter().chain(pos) {
        match revert(&step, env) {
            Ok(note) => {
                match &step {
                    Step::ModifyFile { path, .. } => icon_paths.push(path.clone()),
                    Step::WriteFile { path, .. } => {
                        icon_paths.extend(path.parent().map(Path::to_path_buf))
                    }
                    _ => {}
                }
                j.close(seq, Mark::Undone, &step, note)?;
                n += 1;
            }
            Err(e) => errors.push(format!("step {seq}: {e:#}")),
        }
    }
    if !icon_paths.is_empty() && !env.desktops.is_sandbox() {
        crate::plan::fsops::refresh_icons(&icon_paths);
    }
    if errors.is_empty() {
        Ok(n)
    } else {
        Err(anyhow!(
            "{} step(s) could not be undone: {}",
            errors.len(),
            errors.join("; ")
        ))
    }
}

/// `undo(plan_id?)`: the given plan or the last applied one in the current mode.
pub fn undo(id: Option<&str>, env: &Env) -> Result<StoredPlan> {
    let mut sp = match id {
        Some(id) => env.store.load(id)?,
        None => env
            .store
            .last_applied(env.desktops.is_sandbox())
            .context("nothing to undo")?,
    };
    if sp.state != PlanState::Applied {
        bail!(
            "plan {} is {:?}; only applied plans can be undone",
            sp.id,
            sp.state
        );
    }
    if sp.sandbox != env.desktops.is_sandbox() {
        bail!(
            "plan {} belongs to {} mode",
            sp.id,
            if sp.sandbox { "sandbox" } else { "live" }
        );
    }
    let n = rollback(&sp.id, env)?;
    sp.state = PlanState::Undone;
    sp.results.push(StepResult {
        step: 0,
        op: "undo".into(),
        item: None,
        status: "done".into(),
        detail: format!("{n} step(s) undone"),
    });
    env.store.save(&sp)?;
    notify_dir(&env.desktops.user);
    Ok(sp)
}

#[derive(Debug, Serialize, Clone)]
pub struct QuarantineEntry {
    pub id: String,
    pub plan_id: String,
    pub name: String,
    pub original_path: PathBuf,
    pub quarantined_path: PathBuf,
    pub size: u64,
    pub reason: String,
}

/// Everything currently sitting in quarantine (from journals; files that still exist there).
pub fn quarantine_list(env: &Env) -> Result<Vec<QuarantineEntry>> {
    let mut out = Vec::new();
    for sp in env.store.list() {
        if sp.sandbox != env.desktops.is_sandbox() || !matches!(sp.state, PlanState::Applied) {
            continue;
        }
        let Ok(lines) = journal::read(&env.store.journal_file(&sp.id)) else {
            continue;
        };
        for (seq, step) in journal::done_steps(&lines) {
            let Step::Move {
                item,
                from,
                to,
                why,
            } = step
            else {
                continue;
            };
            if why != "quarantine" || !to.exists() {
                continue;
            }
            let reason = sp
                .plan
                .actions
                .iter()
                .find(|a| matches!(a, Action::Quarantine { .. }) && a.item() == item.as_deref())
                .map(|a| a.reason().to_string())
                .unwrap_or_default();
            out.push(QuarantineEntry {
                id: format!("q_{}_{seq}", &sp.id[3..]),
                plan_id: sp.id.clone(),
                name: from
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                size: std::fs::metadata(&to).map(|m| m.len()).unwrap_or(0),
                original_path: from,
                quarantined_path: to,
                reason,
            });
        }
    }
    Ok(out)
}

/// Moves quarantined items back to where they came from (name conflicts → " (2)").
pub fn quarantine_restore(ids: &[String], env: &Env) -> Result<Vec<String>> {
    let all = quarantine_list(env)?;
    let mut report = Vec::new();
    for id in ids {
        let Some(q) = all.iter().find(|q| &q.id == id) else {
            report.push(format!("{id}: not in quarantine"));
            continue;
        };
        let dir = q.original_path.parent().context("no parent")?;
        std::fs::create_dir_all(dir)?;
        let to = free_name(dir, &q.name, &|p| p.exists());
        move_path(&q.quarantined_path, &to)?;
        notify_dir(dir);
        report.push(format!("{id}: restored to {}", to.display()));
    }
    Ok(report)
}

/// Rejects / cancels a pending plan.
pub fn set_state(id: &str, state: PlanState, store: &Store) -> Result<StoredPlan> {
    let mut sp = store.load(id)?;
    if sp.state != PlanState::Pending {
        bail!("plan {id} is {:?}, not pending", sp.state);
    }
    sp.state = state;
    store.save(&sp)?;
    Ok(sp)
}

/// Kind of a ref for icon styling: existing styleable item, or a `new:` folder.
fn styled_kind(r: &str, by_id: &HashMap<&str, &Item>) -> Option<crate::scan::Kind> {
    match by_id.get(r) {
        Some(i) if crate::plan::validate::styleable(i.kind) && i.writable => Some(i.kind),
        Some(_) => None,
        None if r.starts_with(NEW_PREFIX) => Some(crate::scan::Kind::Folder),
        None => None,
    }
}

fn in_projects(plan: &PlanInput, r: &str) -> bool {
    plan.layout.projects.iter().any(|e| e.item() == r)
}

/// How an item should look (spec §5.4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Look {
    pub style: IconStyle,
    pub shape: Shape,
    /// Baked-in opacity (dimmed projects keep their tint at 50 %).
    pub opacity: f32,
}

/// Style for a `restyle_icon` or styled `dim` action on ref `r`.
pub fn look_for(plan: &PlanInput, r: &str, a: &Action, config: &Config) -> Option<Look> {
    let projects = in_projects(plan, r);
    let (style, opacity) = match a {
        Action::RestyleIcon { style, .. } => (IconStyle::parse(style)?, 1.0),
        // A dimmed project keeps the project look, half transparent; others go muted.
        Action::Dim { value: true, .. } if projects => (config.icons.projects, 0.5),
        Action::Dim { value: true, .. } => (config.icons.dim, 1.0),
        _ => return None,
    };
    let shape = if style == IconStyle::Orb {
        Shape::Circle
    } else {
        Shape::Squircle
    };
    Some(Look {
        style,
        shape,
        opacity,
    })
}

/// Where the base picture comes from — always the *original* art, never an icon Nook made:
/// folders → the stock folder icon (projects share one look); shortcuts → their target's icon;
/// a `.url` already pointing at a Nook icon → its pre-Nook copy from a plan journal backup.
pub fn base_from(
    r: &str,
    path: &Path,
    kind: crate::scan::Kind,
    by_id: &HashMap<&str, &Item>,
    journal_dir: &Path,
) -> String {
    let _ = (r, by_id);
    if kind == crate::scan::Kind::Folder {
        return std::env::temp_dir().to_string_lossy().into_owned();
    }
    // The shortcut itself carries its real icon (custom game icons, browser profiles…), unless
    // Nook already pointed it at one of its own icons: then use the pre-Nook copy.
    let me = path.to_string_lossy().into_owned();
    if !std::fs::read(path).is_ok_and(|b| refers_to_nook_icon(&b)) {
        return me;
    }
    original_backup(path, journal_dir)
        .and_then(|b| {
            // The shell needs the real extension to read the icon.
            let tmp = std::env::temp_dir()
                .join(format!("nook-orig-{}", path.file_name()?.to_string_lossy()));
            std::fs::copy(&b, &tmp).ok()?;
            Some(tmp.to_string_lossy().into_owned())
        })
        .unwrap_or(me)
}

/// Shortcut bytes reference `…\Nook\icons\…` (`.url` is text, `.lnk` stores UTF-16).
pub fn refers_to_nook_icon(bytes: &[u8]) -> bool {
    let needle = r"\nook\icons\";
    if String::from_utf8_lossy(bytes)
        .to_lowercase()
        .contains(needle)
    {
        return true;
    }
    let decode = |b: &[u8]| -> String {
        let w: Vec<u16> = b
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&w).to_lowercase()
    };
    decode(bytes).contains(needle) || bytes.get(1..).is_some_and(|b| decode(b).contains(needle))
}

/// A journaled pre-change copy of `path` that does not itself point at a Nook icon.
fn original_backup(path: &Path, journal_dir: &Path) -> Option<PathBuf> {
    let want = path.to_string_lossy().to_lowercase();
    for e in std::fs::read_dir(journal_dir).ok()?.flatten() {
        let Ok(lines) = journal::read(&e.path()) else {
            continue;
        };
        for l in lines {
            if let Step::ModifyFile {
                path: p, backup, ..
            } = l.step
                && p.to_string_lossy().to_lowercase() == want
                && std::fs::read(&backup).is_ok_and(|b| !refers_to_nook_icon(&b))
            {
                return Some(backup);
            }
        }
    }
    None
}

/// `desktop.ini` with `IconResource` + ReadOnly on the folder (PathMakeSystemFolder equivalent).
fn set_folder_icon(run: &mut Run, r: &str, path: &Path, icon_res: &str) -> Result<()> {
    let ini = path.join("desktop.ini");
    let existed = ini.exists();
    let backup = existed.then(|| free_name(&run.park, "desktop.ini.bak", &|p| p.exists()));
    let old_attrs = existed.then(|| get_attrs(&ini)).transpose()?;
    let park = free_name(&run.park, "desktop.ini", &|p| p.exists());
    let step = Step::WriteFile {
        path: ini.clone(),
        backup: backup.clone(),
        park,
        old_attrs,
    };
    let pk = run.park.clone();
    let res = icon_res.to_string();
    run.step("folder_icon", Some(r), step, || {
        std::fs::create_dir_all(&pk)?;
        if let Some(b) = &backup {
            // Keep the previous desktop.ini: move it aside (never delete).
            set_attrs(&ini, ATTR_NORMAL)?;
            move_path(&ini, b)?;
        }
        std::fs::write(&ini, desktop_ini(&res))?;
        set_attrs(&ini, ATTR_HIDDEN | ATTR_SYSTEM)?;
        Ok(Outcome::Done)
    })?;
    let old = get_attrs(path)?;
    if old & (ATTR_READONLY | ATTR_SYSTEM) == 0 {
        let new = old | ATTR_READONLY;
        run.step(
            "folder_icon",
            Some(r),
            Step::SetAttrs {
                path: path.to_path_buf(),
                old,
                new,
            },
            || {
                set_attrs(path, new)?;
                Ok(Outcome::Done)
            },
        )?;
    }
    notify_item(path);
    Ok(())
}

/// Generates the styled `.ico` and assigns it (spec §5.4).
fn restyle(
    run: &mut Run,
    r: &str,
    path: &Path,
    kind: crate::scan::Kind,
    look: Look,
    base: &str,
) -> Result<()> {
    let accent = crate::preview::render::parse_hex(&run.env.config.icons.accent);
    let ico = crate::icons::make(
        &run.env.icons_dir,
        base,
        look.style,
        look.shape,
        accent,
        look.opacity,
    )?;
    match kind {
        crate::scan::Kind::Folder => set_folder_icon(run, r, path, &format!("{},0", ico.display())),
        _ => {
            let backup = free_name(
                &run.park,
                &format!("{}.bak", path.file_name().unwrap().to_string_lossy()),
                &|p| p.exists(),
            );
            let park = free_name(
                &run.park,
                &path.file_name().unwrap().to_string_lossy(),
                &|p| p.exists(),
            );
            let step = Step::ModifyFile {
                path: path.to_path_buf(),
                backup: backup.clone(),
                park,
            };
            let pk = run.park.clone();
            run.step("restyle_icon", Some(r), step, || {
                std::fs::create_dir_all(&pk)?;
                // Keep the original bytes; undo puts them back.
                std::fs::copy(path, &backup)?;
                if kind == crate::scan::Kind::Url {
                    let text = std::fs::read(path)?;
                    std::fs::write(
                        path,
                        crate::shell::shortcuts::set_url_icon(&text, &ico.to_string_lossy()),
                    )?;
                } else {
                    crate::shell::shortcuts::set_lnk_icon(path, &ico)?;
                }
                notify_item(path);
                Ok(Outcome::Done)
            })?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::refers_to_nook_icon;

    #[test]
    fn detects_nook_icon_in_url_and_lnk() {
        let url = r"[InternetShortcut]
IconFile=C:\Users\X\AppData\Local\Nook\icons\ab.ico";
        assert!(refers_to_nook_icon(url.as_bytes()));
        let lnk: Vec<u8> = "\u{1}\u{2}"
            .encode_utf16()
            .chain(r"C:\Users\X\AppData\Local\Nook\icons\ab.ico".encode_utf16())
            .flat_map(u16::to_le_bytes)
            .collect();
        assert!(refers_to_nook_icon(&lnk));
        let mut shifted = vec![0u8];
        shifted.extend(&lnk);
        assert!(refers_to_nook_icon(&shifted), "odd byte offset");
        assert!(!refers_to_nook_icon(
            r"IconFile=C:\Steam\game.ico".as_bytes()
        ));
    }
}
