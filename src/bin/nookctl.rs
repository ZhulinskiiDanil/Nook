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
    /// Development sandbox.
    Sandbox {
        #[command(subcommand)]
        action: SandboxAction,
    },
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
    let _config = Config::load_or_create(&dirs.config_file())?;
    let _com = ComApartment::sta()?;

    match cli.command {
        Command::Doctor { json } => {
            tracing::info!("doctor");
            let report = Report::collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", report.render_text());
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
