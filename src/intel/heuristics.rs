//! Name-based hints (spec §8). Pure, no Win32. Hints are suggestions; Claude decides.

/// Lowercased file stem and extension (`ext` without dot, lowercase).
fn split(name: &str) -> (String, String) {
    let lower = name.to_lowercase();
    match lower.rfind('.') {
        Some(i) if i > 0 => (lower[..i].to_string(), lower[i + 1..].to_string()),
        _ => (lower, String::new()),
    }
}

/// « - копия», « копия», « - Copy», «Copy of», « (2)» and numbered variants
/// like «копия 2», « - Copy (3)», « (12)».
pub fn is_copy(name: &str) -> bool {
    let (stem, _) = split(name);
    let stem = stem.trim_end();
    if stem.starts_with("copy of ") || stem.starts_with("копия ") {
        return true;
    }
    // Strip a trailing " (N)" or " N" counter.
    let without_counter = strip_counter(stem);
    if without_counter.len() != stem.len() && stem.ends_with(')') {
        return true; // "name (2)"
    }
    let s = without_counter.trim_end();
    s.ends_with(" - копия")
        || s.ends_with(" копия")
        || s.ends_with(" - copy")
        || s.ends_with(" copy")
}

fn strip_counter(s: &str) -> &str {
    let t = s.trim_end();
    if let Some(inner) = t.strip_suffix(')')
        && let Some(open) = inner.rfind(" (")
        && !inner[open + 2..].is_empty()
        && inner[open + 2..].chars().all(|c| c.is_ascii_digit())
    {
        return &t[..open];
    }
    if let Some(sp) = t.rfind(' ')
        && !t[sp + 1..].is_empty()
        && t[sp + 1..].chars().all(|c| c.is_ascii_digit())
    {
        return &t[..sp];
    }
    t
}

/// `backup`, `бэкап`, `.bak`.
pub fn is_backup(name: &str) -> bool {
    let (stem, ext) = split(name);
    ext == "bak" || stem.contains("backup") || stem.contains("бэкап") || stem.contains("бекап")
}

/// `.log`, `.tmp`, `~$*`, `.crdownload`, `.part`.
pub fn is_temp(name: &str) -> bool {
    let (_, ext) = split(name);
    name.starts_with("~$") || matches!(ext.as_str(), "log" | "tmp" | "crdownload" | "part" | "temp")
}

/// Name contains `setup|install|installer|установ`, or the file is an `.msi`.
/// The PE version-info check lives in `peek`/`scan` (needs Win32).
pub fn is_installer_name(name: &str) -> bool {
    let (stem, ext) = split(name);
    if ext == "msi" || ext == "msix" || ext == "msixbundle" || ext == "appx" {
        return true;
    }
    if !matches!(ext.as_str(), "exe" | "zip" | "7z") {
        return false;
    }
    ["setup", "install", "установ"]
        .iter()
        .any(|k| stem.contains(k))
}

/// True if a display name renders as empty (invisible-caption shortcut, spec §13).
pub fn caption_is_blank(stem: &str) -> bool {
    !stem.is_empty()
        && stem
            .chars()
            .all(|c| c == '\u{2800}' || c == '\u{00A0}' || c.is_whitespace())
}

/// Lowercase alphanumeric words, used to match installer names to installed programs.
pub fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !w.chars().all(|c| c.is_ascii_digit()))
        .filter(|w| {
            !matches!(
                *w,
                "setup"
                    | "install"
                    | "installer"
                    | "x64"
                    | "x86"
                    | "win"
                    | "win64"
                    | "win32"
                    | "amd64"
                    | "windows"
                    | "release"
                    | "full"
            )
        })
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies() {
        for n in [
            "robot_03 копия 2.png",
            "robot_03 - копия.png",
            "robot_03 копия.png",
            "report - Copy.docx",
            "report - Copy (3).docx",
            "Copy of report.docx",
            "Копия отчёт.docx",
            "photo (2).jpg",
            "photo (12).jpg",
        ] {
            assert!(is_copy(n), "{n}");
        }
        for n in [
            "robot_03.png",
            "копир.png",
            "Семен КС конфиги",
            "copycat.png",
            "v2 (beta).txt",
            "track 2.mp3",
        ] {
            assert!(!is_copy(n), "{n}");
        }
    }

    #[test]
    fn backups_and_temp() {
        assert!(is_backup("save.bak"));
        assert!(is_backup("Бэкап конфига.zip"));
        assert!(is_backup("db_backup_2026.sql"));
        assert!(!is_backup("backpack.png"));
        assert!(is_temp("debug.log"));
        assert!(is_temp("~$отчёт.docx"));
        assert!(is_temp("film.mkv.crdownload"));
        assert!(is_temp("file.part"));
        assert!(!is_temp("catalog.txt"));
    }

    #[test]
    fn installers() {
        assert!(is_installer_name("SteamSetup.exe"));
        assert!(is_installer_name("Blender-4.2-windows-x64.msi"));
        assert!(is_installer_name("Установщик Photoshop.exe"));
        assert!(is_installer_name("discord_installer.exe"));
        assert!(!is_installer_name("setup.txt"));
        assert!(!is_installer_name("game.exe"));
    }

    #[test]
    fn blank_captions() {
        assert!(caption_is_blank("\u{2800}"));
        assert!(caption_is_blank("\u{2800}\u{2800}"));
        assert!(caption_is_blank("\u{00A0}"));
        assert!(!caption_is_blank(""));
        assert!(!caption_is_blank("Steam"));
    }

    #[test]
    fn word_split() {
        assert_eq!(
            words("Blender-4.2.1-windows-x64.msi"),
            vec!["blender", "msi"]
        );
        assert_eq!(words("OBS Studio"), vec!["obs", "studio"]);
    }
}
