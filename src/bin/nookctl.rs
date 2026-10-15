//! `nookctl` — console companion: debugging commands now, MCP server from M2.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use nook::config::Config;
use nook::doctor::Report;
use nook::paths::{AppDirs, Desktops, SANDBOX_ENV};
use nook::platform::{ComApartment, enable_per_monitor_dpi};
use nook::scan::{Item, ScanResult, scan};

#[derive(Parser)]
#[command(name = "nookctl", version, about = "Nook control tool")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read-only report: Windows, monitor, desktop grid, Explorer settings, wallpaper, items.
    Doctor {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Also write one icon's current position back and read it again (needs `positions save`).
        #[arg(long)]
        roundtrip: bool,
    },
    /// Layout by rough heuristic groups (M3 check). Default: dry run, prints an ASCII grid.
    Layout {
        /// Print only; never move anything (default).
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        /// Move real desktop icons (positions only). Requires `positions save`.
        #[arg(long)]
        apply: bool,
    },
    /// Save or restore desktop icon positions.
    Positions {
        #[command(subcommand)]
        action: PositionsAction,
    },
    /// Read-only scan of all desktop items with hints.
    Dump {
        /// Print the full scan as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Look inside one item (by id or exact name). Saves the thumbnail as PNG.
    Peek {
        item: String,
        /// Where to write the thumbnail (default: %LOCALAPPDATA%\Nook\peek\<id>.png).
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// stdio MCP server for Claude Code (proxies to the daemon, starting it if needed).
    Mcp,
    /// Apply a pending plan (M4; from M5 the user applies from the preview).
    Apply { plan_id: String },
    /// Render a plan's preview frame to PNG (debug; the real preview is LCtrl+LAlt).
    Preview {
        plan_id: String,
        #[arg(long)]
        out: PathBuf,
    },
    /// Move shortcuts from the Public Desktop to yours (UAC prompt) so Nook can restyle them.
    /// `--undo` moves them back.
    AdoptPublic {
        /// Only these file names (default: all .lnk/.url on the Public Desktop).
        names: Vec<String>,
        #[arg(long)]
        undo: bool,
        /// Internal: the elevated half.
        #[arg(long, hide = true)]
        elevated: bool,
        #[arg(long, hide = true)]
        public: Option<PathBuf>,
        #[arg(long, hide = true)]
        user: Option<PathBuf>,
        #[arg(long, hide = true)]
        record: Option<PathBuf>,
        #[arg(long, hide = true)]
        report: Option<PathBuf>,
    },
    /// Undo the last applied plan, or the given one.
    Undo { plan_id: Option<String> },
    /// Applied / undone / failed plans.
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Quarantine: list or restore.
    Quarantine {
        #[command(subcommand)]
        action: QuarantineAction,
    },
    /// Call a daemon IPC method directly, e.g. `call status` or `call scan {"limit":5}`.
    Call {
        method: String,
        /// JSON params.
        params: Option<String>,
    },
    /// Development sandbox.
    Sandbox {
        #[command(subcommand)]
        action: SandboxAction,
    },
}

#[derive(Subcommand)]
enum QuarantineAction {
    List,
    Restore { ids: Vec<String> },
}

/// One IPC call to the daemon (starting it if needed); prints pretty JSON.
fn ipc(method: &str, params: serde_json::Value) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let v = rt.block_on(async {
        let mut c = nook::ipc::client::Client::connect_or_start().await?;
        c.call(method, params).await
    })?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

#[derive(Subcommand)]
enum PositionsAction {
    /// Snapshot all icon positions to %LOCALAPPDATA%\Nook\positions\ (+ latest.json).
    Save,
    /// Put icons back where a snapshot says (default: latest.json).
    Restore {
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Show which icons moved since the snapshot.
    Diff {
        #[arg(long)]
        file: Option<PathBuf>,
    },
}

/// Live desktop only: positions are never touched in sandbox mode (spec §10).
fn require_live(d: &Desktops) -> Result<()> {
    if d.is_sandbox() {
        bail!(
            "sandbox mode ({SANDBOX_ENV} is set): icon positions are never touched. Unset it to work with real icons."
        );
    }
    Ok(())
}

struct Plan {
    input: nook::layout::engine::Input,
    out: nook::layout::engine::Output,
    names: std::collections::HashMap<String, String>,
}

fn plan_layout(r: &ScanResult, cfg: &Config) -> Result<Plan> {
    use nook::layout::{engine, free_zones, heuristic};
    let mon = nook::platform::PrimaryMonitor::query()?;
    let g = r.grid.context("grid unknown (desktop view unavailable)")?;
    let (cols, rows) = nook::shell::desktop_view::grid_dims(
        mon.work.width(),
        mon.work.height(),
        g.step_x,
        g.step_y,
    );
    let input = engine::Input {
        cols,
        rows,
        blocked: free_zones::blocked_cells(cols as usize, rows as usize, &cfg.layout.keep_free),
        groups: heuristic::group(&r.items),
    };
    let out = engine::layout(&input, &cfg.layout);
    let names = r
        .items
        .iter()
        .map(|i| (i.id.clone(), i.name.clone()))
        .collect();
    Ok(Plan { input, out, names })
}

#[derive(Subcommand)]
enum SandboxAction {
    /// Mirror the real desktop (read-only) into NOOK_DESKTOP_ROOT or --root.
    Init {
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

fn flags(it: &Item) -> String {
    let h = &it.hints;
    let mut f = Vec::new();
    if !h.duplicate_of.is_empty() {
        f.push(format!("dup:{}", h.duplicate_of.join(",")));
    }
    if !h.same_target_as.is_empty() {
        f.push(format!("same-target:{}", h.same_target_as.join(",")));
    }
    for (on, name) in [
        (h.is_copy, "copy"),
        (h.is_backup, "backup"),
        (h.is_installer, "installer"),
        (h.is_temp, "temp"),
        (h.is_protected_os, "os-file"),
        (it.caption_hidden, "no-caption"),
        (it.attributes.contains(&"hidden"), "hidden"),
        (
            !it.writable && it.source != nook::scan::Source::Virtual,
            "read-only",
        ),
    ] {
        if on {
            f.push(name.to_string());
        }
    }
    if let Some(m) = &h.installed_match {
        f.push(format!("installed:\"{m}\""));
    }
    if let Some(s) = &it.shortcut {
        if s.pinned_to_taskbar {
            f.push("pinned".into());
        }
        if s.target_exists == Some(false) {
            f.push("broken".into());
        }
        if let Some(n) = s.launch_count {
            f.push(format!("runs:{n}"));
        }
        if let Some(t) = &s.last_launch {
            f.push(format!("last:{}", &t[..10]));
        }
        if let Some(u) = &s.uninstall_match {
            f.push(format!("app:\"{u}\""));
        }
    }
    if let Some(fo) = &it.folder {
        f.push(format!("{} entries", fo.entries));
    }
    f.push(format!("{}d", h.age_days));
    f.join(" ")
}

fn print_dump(r: &ScanResult) {
    println!(
        "{} items · {} · user: {} · public: {}",
        r.total,
        if r.sandbox {
            "SANDBOX"
        } else {
            "LIVE (read-only)"
        },
        r.user_desktop.display(),
        r.public_desktop.display()
    );
    println!(
        "{:<11} {:<7} {:<8} {:>7}  {:<40} hints",
        "id", "source", "kind", "cell", "name"
    );
    for it in &r.items {
        let cell = it
            .cell
            .map_or("-".into(), |c| format!("{},{}", c.col, c.row));
        let src = format!("{:?}", it.source).to_lowercase();
        let kind = format!("{:?}", it.kind).to_lowercase();
        println!(
            "{:<11} {:<7} {:<8} {:>7}  {:<40} {}",
            it.id,
            src,
            kind,
            cell,
            it.name,
            flags(it)
        );
    }
    for w in &r.warnings {
        println!("warning: {w}");
    }
}

fn find<'a>(r: &'a ScanResult, key: &str) -> Result<&'a Item> {
    r.items
        .iter()
        .find(|i| i.id == key)
        .or_else(|| r.items.iter().find(|i| i.name == key))
        .or_else(|| {
            r.items
                .iter()
                .find(|i| i.name.to_lowercase() == key.to_lowercase())
        })
        .with_context(|| format!("no item with id or name {key:?}"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Err(e) = enable_per_monitor_dpi() {
        eprintln!("warning: {e:#}");
    }
    let dirs = AppDirs::resolve()?;
    let _log = nook::logging::init(&dirs.logs_dir(), "nookctl")?;
    let config = Config::load_or_create(&dirs.config_file())?;
    let _com = ComApartment::sta()?;

    if let Command::Mcp = cli.command {
        tracing::info!("mcp server starting");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let r = rt.block_on(nook::mcp::serve_stdio(config.safety.allow_ai_apply));
        tracing::info!("mcp server stopped: {r:?}");
        return r;
    }
    match cli.command {
        Command::Mcp => unreachable!(),
        Command::Apply { plan_id } => ipc("apply_plan", serde_json::json!({ "id": plan_id }))?,
        Command::Undo { plan_id } => ipc("undo", serde_json::json!({ "plan_id": plan_id }))?,
        Command::AdoptPublic {
            names,
            undo,
            elevated,
            public,
            user,
            record,
            report,
        } => {
            if elevated {
                // Elevated half: paths come from the unelevated parent (never this profile's).
                let (record, report) = (record.context("--record")?, report.context("--report")?);
                let lines = if undo {
                    nook::adopt::give_back(&record)
                } else {
                    nook::adopt::adopt(
                        &public.context("--public")?,
                        &user.context("--user")?,
                        &record,
                        &names,
                    )
                };
                let text = match &lines {
                    Ok(l) => l.join("\n"),
                    Err(e) => format!("ERROR: {e:#}"),
                };
                std::fs::write(&report, text)?;
                std::process::exit(if lines.is_ok() { 0 } else { 2 });
            }
            let d = Desktops::resolve()?;
            require_live(&d)?;
            let record = dirs.data_dir.join("adopted.json");
            let report = dirs.data_dir.join("adopt-report.txt");
            let q = |p: &std::path::Path| format!("\"{}\"", p.display());
            let mut args = format!(
                "adopt-public --elevated --public {} --user {} --record {} --report {}",
                q(&d.public),
                q(&d.user),
                q(&record),
                q(&report)
            );
            if undo {
                args.push_str(" --undo");
            }
            for n in &names {
                args.push_str(&format!(" \"{n}\""));
            }
            println!("Asking for administrator rights (UAC)…");
            let code = nook::adopt::run_elevated(&args)?;
            print!("{}", std::fs::read_to_string(&report).unwrap_or_default());
            println!();
            if code != 0 {
                bail!("elevated step failed (exit code {code})");
            }
        }
        Command::Preview { plan_id, out } => {
            let store = nook::plan::store::Store::new(&dirs.data_dir);
            let sp = store.load(&plan_id)?;
            let r = scan(&Desktops::resolve()?)?;
            let view = nook::preview::frame::view_facts()?;
            let f = nook::preview::frame::build(&r, &sp, &config, &view)?;
            f.pixmap.save_png(&out)?;
            println!(
                "{}x{} frame in {} ms (live wallpaper: {}) → {}",
                f.width,
                f.height,
                f.build_ms,
                f.live_wallpaper,
                out.display()
            );
        }
        Command::History { limit } => ipc("history", serde_json::json!({ "limit": limit }))?,
        Command::Quarantine {
            action: QuarantineAction::List,
        } => ipc("quarantine_list", serde_json::Value::Null)?,
        Command::Quarantine {
            action: QuarantineAction::Restore { ids },
        } => ipc("quarantine_restore", serde_json::json!({ "ids": ids }))?,
        Command::Call { method, params } => {
            let params: serde_json::Value = match params {
                Some(p) => serde_json::from_str(&p).context("params must be JSON")?,
                None => serde_json::Value::Null,
            };
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let v = rt.block_on(async {
                let mut c = nook::ipc::client::Client::connect_or_start().await?;
                c.call(&method, params).await
            })?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Command::Doctor { json, roundtrip } => {
            tracing::info!("doctor");
            let report = Report::collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", report.render_text());
            }
            if roundtrip {
                require_live(&Desktops::resolve()?)?;
                nook::positions::require_saved(&dirs.data_dir)?;
                let view = nook::shell::desktop_view::DesktopView::open()?;
                let items = view.items()?;
                let it = items.first().context("desktop is empty")?;
                // Write the item's own position back: exercises the write path, no visible change.
                let targets = [(it.parsing.to_lowercase(), (it.x, it.y))]
                    .into_iter()
                    .collect();
                let moved = view.set_positions(&targets)?;
                let after = view.items()?;
                let again = after
                    .iter()
                    .find(|a| a.parsing == it.parsing)
                    .context("item vanished")?;
                let ok = moved == 1 && (again.x, again.y) == (it.x, it.y);
                println!(
                    "\n[roundtrip] {}: wrote ({}, {}), read ({}, {}) → {}",
                    it.display,
                    it.x,
                    it.y,
                    again.x,
                    again.y,
                    if ok { "OK" } else { "MISMATCH" }
                );
                if !ok {
                    bail!("position round-trip failed");
                }
            }
        }
        Command::Layout { dry_run: _, apply } => {
            let desktops = Desktops::resolve()?;
            let r = scan(&desktops)?;
            let p = plan_layout(&r, &config)?;
            println!(
                "grid {}x{} · {} items · A=apps P=projects I=inbox T=trash #=keep_free{}",
                p.input.cols,
                p.input.rows,
                r.total,
                if r.sandbox {
                    " · SANDBOX (fictitious grid)"
                } else {
                    ""
                }
            );
            print!("{}", nook::layout::engine::ascii(&p.input, &p.out));
            for (g, entries) in &p.input.groups {
                let names: Vec<&str> = entries.iter().map(|e| p.names[&e.id].as_str()).collect();
                println!("{g:?} ({}): {}", entries.len(), names.join(", "));
            }
            if !p.out.overflow.is_empty() {
                let names: Vec<&str> = p
                    .out
                    .overflow
                    .iter()
                    .map(|id| p.names[id].as_str())
                    .collect();
                println!(
                    "OVERFLOW ({}), not placed: {}",
                    names.len(),
                    names.join(", ")
                );
            }
            for w in &p.out.warnings {
                println!("warning: {w}");
            }
            if !desktops.is_sandbox()
                && let Some(g) = r.grid
            {
                // Sanity: cell → point must land exactly on current icons (no moves).
                let vitems = nook::shell::desktop_view::DesktopView::open()?.items()?;
                let origin = nook::shell::desktop_view::grid_origin(&vitems, g.step_x, g.step_y);
                let exact = vitems
                    .iter()
                    .filter(|v| {
                        let (c, rw) =
                            nook::shell::desktop_view::cell_of(v.x, v.y, g.step_x, g.step_y);
                        nook::shell::desktop_view::point_of(c, rw, g.step_x, g.step_y, origin)
                            == (v.x, v.y)
                    })
                    .count();
                println!(
                    "grid origin {origin:?}; {exact} of {} icons sit exactly on grid points",
                    vitems.len()
                );
            }
            if apply {
                require_live(&desktops)?;
                nook::positions::require_saved(&dirs.data_dir)?;
                let view = nook::shell::desktop_view::DesktopView::open()?;
                if view.info()?.flags.auto_arrange {
                    bail!(
                        "auto-arrange is on; turn it off first (a plan would include auto_arrange_off)"
                    );
                }
                let vitems = view.items()?;
                let g = r.grid.context("grid unknown")?;
                let origin = nook::shell::desktop_view::grid_origin(&vitems, g.step_x, g.step_y);
                let path_of: std::collections::HashMap<&str, String> = r
                    .items
                    .iter()
                    .map(|i| (i.id.as_str(), i.path.to_lowercase()))
                    .collect();
                let targets = p
                    .out
                    .cells
                    .iter()
                    .map(|(id, c)| {
                        let pt = nook::shell::desktop_view::point_of(
                            c.col, c.row, g.step_x, g.step_y, origin,
                        );
                        (path_of[id.as_str()].clone(), pt)
                    })
                    .collect();
                let moved = view.set_positions(&targets)?;
                println!("moved {moved} icons (positions only). Undo: nookctl positions restore");
            } else {
                println!("dry run: nothing moved.");
            }
        }
        Command::Positions { action } => {
            let desktops = Desktops::resolve()?;
            require_live(&desktops)?;
            let view = nook::shell::desktop_view::DesktopView::open()?;
            match action {
                PositionsAction::Save => {
                    let snap = nook::positions::Snapshot::from_view(&view.items()?);
                    let path = nook::positions::save(&dirs.data_dir, &snap)?;
                    println!("saved {} positions → {}", snap.items.len(), path.display());
                }
                PositionsAction::Restore { file } => {
                    let path = file.unwrap_or_else(|| nook::positions::latest(&dirs.data_dir));
                    let snap = nook::positions::load(&path)?;
                    let moved = view.set_positions(&snap.targets())?;
                    let left = snap.diff(&view.items()?);
                    println!(
                        "restored {moved} of {} icons from {}",
                        snap.items.len(),
                        path.display()
                    );
                    if !left.is_empty() {
                        println!("still differ ({}): {}", left.len(), left.join(", "));
                    }
                }
                PositionsAction::Diff { file } => {
                    let path = file.unwrap_or_else(|| nook::positions::latest(&dirs.data_dir));
                    let snap = nook::positions::load(&path)?;
                    let moved = snap.diff(&view.items()?);
                    println!(
                        "{} of {} icons differ from {}",
                        moved.len(),
                        snap.items.len(),
                        path.display()
                    );
                    for m in moved {
                        println!("  {m}");
                    }
                }
            }
        }
        Command::Dump { json } => {
            let r = scan(&Desktops::resolve()?)?;
            tracing::info!(items = r.total, sandbox = r.sandbox, "dump");
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print_dump(&r);
            }
        }
        Command::Peek { item, out, json } => {
            let r = scan(&Desktops::resolve()?)?;
            let it = find(&r, &item)?;
            let p = nook::intel::peek::peek(it);
            let saved = match &p.thumbnail_png {
                Some(png) => {
                    let path = out.unwrap_or_else(|| {
                        dirs.data_dir.join("peek").join(format!("{}.png", it.id))
                    });
                    nook::fsutil::write_atomic(&path, png)?;
                    Some(path)
                }
                None => None,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&p)?);
            } else {
                println!("{} ({})", p.name, p.id);
                if let (Some(path), Some((w, h))) = (&saved, p.thumbnail_size) {
                    println!("thumbnail: {w}x{h} → {}", path.display());
                }
                if let Some(v) = &p.version {
                    println!("version: {v:?}");
                }
                if let Some(e) = &p.entries {
                    println!(
                        "entries ({} of {}):",
                        e.len(),
                        p.entries_total.unwrap_or(e.len())
                    );
                    for x in e {
                        println!(
                            "  {}{}  {}",
                            x.name,
                            if x.is_dir { "\\" } else { "" },
                            x.size
                        );
                    }
                }
                if let Some(t) = &p.text {
                    println!(
                        "text ({:?}{}):\n{t}",
                        p.text_encoding.unwrap(),
                        if p.text_truncated { ", truncated" } else { "" }
                    );
                }
                for e in &p.errors {
                    println!("note: {e}");
                }
            }
        }
        Command::Sandbox {
            action: SandboxAction::Init { root },
        } => {
            let root = root
                .or_else(|| {
                    std::env::var_os(SANDBOX_ENV)
                        .filter(|v| !v.is_empty())
                        .map(PathBuf::from)
                })
                .unwrap_or_else(|| PathBuf::from(r"C:\nook-sandbox"));
            if !root.is_absolute() {
                bail!("sandbox root must be absolute: {}", root.display());
            }
            let rep = nook::sandbox::init(&root)?;
            println!(
                "sandbox {}: {} copied ({} KB), {} large-file stubs, {} folder stubs",
                rep.root.display(),
                rep.copied,
                rep.bytes / 1024,
                rep.stubs,
                rep.folders
            );
            for s in &rep.skipped {
                println!("skipped: {s}");
            }
            println!("use it: $env:{SANDBOX_ENV}='{}'", rep.root.display());
        }
    }
    Ok(())
}
