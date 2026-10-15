//! Nook — keeps the Windows desktop clean. Corners give order; Claude Code decides
//! what goes where; Nook previews, applies and undoes.

pub mod config;
pub mod daemon;
pub mod doctor;
pub mod fsutil;
pub mod intel;
pub mod ipc;
pub mod layout;
pub mod logging;
pub mod mcp;
pub mod paths;
pub mod plan;
pub mod platform;
pub mod positions;
pub mod preview;
pub mod sandbox;
pub mod scan;
pub mod shell;
pub mod status;
pub mod timefmt;
pub mod winreg;
