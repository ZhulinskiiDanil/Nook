//! `%APPDATA%\Nook\config.toml`: created with defaults on first run.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::fsutil::write_atomic;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomRight,
    BottomLeft,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Corners {
    pub apps: Corner,
    pub projects: Corner,
    pub inbox: Corner,
    pub trash: Corner,
}

impl Default for Corners {
    fn default() -> Self {
        Self {
            apps: Corner::TopLeft,
            projects: Corner::TopRight,
            inbox: Corner::BottomRight,
            trash: Corner::BottomLeft,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutConfig {
    pub margin_cells: u32,
    pub apps_columns: u32,
    /// Areas the layout never fills, as fractions of the work area.
    pub keep_free: Vec<Zone>,
    pub corners: Corners,
}

/// Rectangle in fractions (0..=1) of the work area.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Zone {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            margin_cells: 1,
            apps_columns: 2,
            keep_free: vec![Zone {
                x: 0.25,
                y: 0.0,
                w: 0.5,
                h: 1.0,
            }],
            corners: Corners::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreviewConfig {
    pub hold: String,
    pub hold_ms: u32,
    pub apply: Vec<String>,
    pub accent: String,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            hold: "LCtrl+LAlt".into(),
            hold_ms: 150,
            apply: vec!["double-tap".into(), "Enter".into()],
            accent: "#8fb3ff".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InboxConfig {
    pub enabled: bool,
    pub ignore: Vec<String>,
}

impl Default for InboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ignore: ["*.tmp", "*.crdownload", "*.part", "~$*", "desktop.ini"]
                .map(String::from)
                .to_vec(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    pub allowed_roots: Vec<String>,
    pub quarantine_dir: String,
    pub allow_ai_apply: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            allowed_roots: vec!["%DESKTOP%".into()],
            quarantine_dir: r"%USERPROFILE%\.nook\quarantine".into(),
            allow_ai_apply: false,
        }
    }
}

/// Nook icon styles (spec §5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconStyle {
    /// Original icon, full size, with a soft shadow (apps).
    Lifted,
    /// Original shape recolored to the accent, keeping its shading (projects).
    Tint,
    /// Original, full size, in accent-tinted grayscale (inbox, inactive).
    Muted,
    /// Rounded square in the icon's own (darkened) color. Kept for configs that ask for it.
    Tile,
    /// Round glass plate. Kept for configs that ask for it.
    Orb,
    /// Put back the icon the item had before Nook ever touched it (from journal backups).
    Original,
    /// Keep the stock icon.
    None,
}

impl IconStyle {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "lifted" => Self::Lifted,
            "tint" => Self::Tint,
            "muted" => Self::Muted,
            "tile" => Self::Tile,
            "orb" => Self::Orb,
            "none" => Self::None,
            "original" => Self::Original,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IconsConfig {
    pub enabled: bool,
    pub apps: IconStyle,
    pub projects: IconStyle,
    pub dim: IconStyle,
    pub accent: String,
}

impl Default for IconsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            apps: IconStyle::Lifted,
            projects: IconStyle::Tint,
            dim: IconStyle::Muted,
            accent: "#8fb3ff".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Start with Windows (`HKCU\...\Run`).
    pub autostart: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self { autostart: true }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub layout: LayoutConfig,
    pub preview: PreviewConfig,
    pub inbox: InboxConfig,
    pub safety: SafetyConfig,
    pub icons: IconsConfig,
}

/// Text written on first run; mirrors appendix B of the spec.
pub const DEFAULT_CONFIG_TOML: &str = r##"# Nook config. Missing keys fall back to defaults.

[general]
autostart = true

[layout]
margin_cells = 1
apps_columns = 2
# Areas never filled, as fractions of the work area (default: center column, full height).
keep_free = [{ x = 0.25, y = 0.0, w = 0.5, h = 1.0 }]
corners = { apps = "top-left", projects = "top-right", inbox = "bottom-right", trash = "bottom-left" }

[preview]
hold = "LCtrl+LAlt"
hold_ms = 150
apply = ["double-tap", "Enter"]
accent = "#8fb3ff"

[inbox]
enabled = true
ignore = ["*.tmp", "*.crdownload", "*.part", "~$*", "desktop.ini"]

[safety]
allowed_roots = ["%DESKTOP%"]
quarantine_dir = "%USERPROFILE%\\.nook\\quarantine"
allow_ai_apply = false

[icons]
enabled = true
apps = "lifted"      # lifted | tint | muted | tile | orb | none
projects = "tint"
dim = "muted"
accent = "#8fb3ff"   # background of muted icons
"##;

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }

    /// Loads the config, creating it with defaults if absent. A malformed file is an error
    /// and is never overwritten.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                Self::parse(&text).with_context(|| format!("invalid config {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                write_atomic(path, DEFAULT_CONFIG_TOML.as_bytes())?;
                Ok(Self::default())
            }
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_text_matches_default_struct() {
        assert_eq!(
            Config::parse(DEFAULT_CONFIG_TOML).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn partial_config_fills_defaults() {
        let c =
            Config::parse("[layout]\napps_columns = 3\n[layout.corners]\ninbox = \"top-left\"\n")
                .unwrap();
        assert_eq!(c.layout.apps_columns, 3);
        assert_eq!(c.layout.margin_cells, 1);
        assert_eq!(c.layout.corners.inbox, Corner::TopLeft);
        assert_eq!(c.layout.corners.apps, Corner::TopLeft);
        assert_eq!(c.preview, PreviewConfig::default());
    }

    #[test]
    fn bad_corner_is_rejected() {
        assert!(Config::parse("[layout.corners]\napps = \"middle\"\n").is_err());
    }

    #[test]
    fn creates_once_and_never_overwrites_broken() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Nook").join("config.toml");
        assert_eq!(Config::load_or_create(&p).unwrap(), Config::default());
        assert!(p.exists());

        std::fs::write(&p, "[layout\n").unwrap();
        assert!(Config::load_or_create(&p).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "[layout\n");
    }
}
