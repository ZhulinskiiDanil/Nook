//! `nookctl` — console companion: debugging commands now, MCP server from M2.

use anyhow::Result;
use clap::{Parser, Subcommand};
use nook::config::Config;
use nook::doctor::Report;
use nook::paths::AppDirs;
use nook::platform::{ComApartment, enable_per_monitor_dpi};

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
    }
    Ok(())
}
