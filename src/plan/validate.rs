//! Plan validation (spec §5.2). Pure: works on scan items and resolved paths.
//! A plan is rejected as a whole with a readable list of errors.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use crate::plan::model::{Action, NEW_PREFIX, PlanInput};
use crate::scan::{Item, Kind};

pub struct Ctx<'a> {
    pub items: &'a [Item],
    /// User Desktop (sandboxed when in sandbox mode); `%DESKTOP%` expands to it.
    pub desktop: PathBuf,
    /// Expanded `safety.allowed_roots`.
    pub allowed_roots: Vec<PathBuf>,
    /// Explorer shows hidden files (needed for `dim`).
    pub shows_hidden: bool,
    /// Other env vars for `%NAME%` (USERPROFILE etc.).
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

/// A validated plan, with refs resolved to paths.
#[derive(Debug, Default)]
pub struct Checked {
    /// Folders to create on the desktop for `new:` refs.
    pub new_folders: Vec<PathBuf>,
    /// group name → ordered refs (ids or `new:` refs).
    pub groups: BTreeMap<&'static str, Vec<String>>,
    pub before: usize,
    pub after: usize,
    pub warnings: Vec<String>,
}

fn lower(p: &Path) -> String {
    p.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// True if `p` is `root` or inside it (case-insensitive, no `..`).
pub fn inside(p: &Path, root: &Path) -> bool {
    let (p, r) = (lower(p), lower(root));
    p == r || p.starts_with(&format!("{r}\\"))
}

fn has_dotdot(p: &Path) -> bool {
    p.components().any(|c| matches!(c, Component::ParentDir))
}

/// Windows file name rules: no reserved chars/names, no trailing dot/space.
pub fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 240 {
        return Err("empty or too long".into());
    }
    if let Some(c) = name
        .chars()
        .find(|c| r#"\/:*?"<>|"#.contains(*c) || (*c as u32) < 32)
    {
        return Err(format!("contains forbidden character {c:?}"));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err("Windows strips trailing dots/spaces".into());
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        return Err(format!("{stem} is a reserved device name"));
    }
    Ok(())
}

/// Expands `%DESKTOP%` and other `%VARS%` in a path from a plan.
pub fn expand(s: &str, ctx: &Ctx) -> Result<PathBuf, String> {
    crate::paths::expand_vars(s, &ctx.desktop, |n| (ctx.env)(n)).map_err(|e| format!("{e:#}"))
}

/// Resolves `new:%DESKTOP%\Name` to a direct child of the desktop.
pub fn resolve_new(r: &str, ctx: &Ctx) -> Result<PathBuf, String> {
    let raw = r
        .strip_prefix(NEW_PREFIX)
        .ok_or_else(|| format!("{r}: not a new: ref"))?;
    let p = expand(raw, ctx)?;
    if has_dotdot(&p) || p.parent().map(lower) != Some(lower(&ctx.desktop)) {
        return Err(format!(
            "{r}: new folders must be created directly on the desktop (%DESKTOP%\\Name)"
        ));
    }
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    valid_name(&name).map_err(|e| format!("{r}: {e}"))?;
    Ok(p)
}

pub fn validate(plan: &PlanInput, ctx: &Ctx) -> Result<Checked, Vec<String>> {
    let mut errors = Vec::new();
    let mut out = Checked {
        before: ctx.items.len(),
        ..Default::default()
    };
    let by_id: HashMap<&str, &Item> = ctx.items.iter().map(|i| (i.id.as_str(), i)).collect();
    let existing_names: HashSet<String> = ctx
        .items
        .iter()
        .filter(|i| i.source != crate::scan::Source::Virtual)
        .map(|i| i.name.to_lowercase())
        .collect();

    if plan.title.trim().is_empty() {
        errors.push("title is empty".into());
    }

    // Resolve a ref: existing item or new folder.
    let mut new_refs: HashMap<String, PathBuf> = HashMap::new();
    let check_ref = |r: &str,
                     errors: &mut Vec<String>,
                     new_refs: &mut HashMap<String, PathBuf>|
     -> Option<Option<&Item>> {
        if r.starts_with(NEW_PREFIX) {
            match resolve_new(r, ctx) {
                Ok(p) => {
                    let name = p.file_name().unwrap().to_string_lossy().to_lowercase();
                    if existing_names.contains(&name) {
                        errors.push(format!(
                            "{r}: already exists on the desktop — use its it_ id"
                        ));
                        return None;
                    }
                    new_refs.insert(r.to_string(), p);
                    Some(None)
                }
                Err(e) => {
                    errors.push(e);
                    None
                }
            }
        } else {
            match by_id.get(r) {
                Some(i) => Some(Some(*i)),
                None => {
                    errors.push(format!("{r}: unknown item id (rescan?)"));
                    None
                }
            }
        }
    };

    // Where each ref ends up.
    let mut placed: HashMap<String, Vec<String>> = HashMap::new();
    for (g, entries) in plan.layout.groups() {
        for e in entries {
            let r = e.item().to_string();
            if let Some(Some(item)) = check_ref(&r, &mut errors, &mut new_refs)
                && item.hints.is_protected_os
            {
                out.warnings.push(format!(
                    "{} is a protected OS file; prefer hide_protected_os_files",
                    item.name
                ));
            }
            placed
                .entry(r.clone())
                .or_default()
                .push(format!("layout.{g}"));
            out.groups.entry(g).or_default().push(r);
        }
    }

    let mut new_dests: HashSet<String> = HashSet::new();
    for r in new_refs.values() {
        new_dests.insert(lower(r));
    }

    for (n, a) in plan.actions.iter().enumerate() {
        let at = format!("actions[{n}] {}", a.op());
        if a.reason().trim().is_empty() {
            errors.push(format!("{at}: reason is required"));
        }
        let item = match a.item() {
            Some(r) => match check_ref(r, &mut errors, &mut new_refs) {
                Some(i) => i,
                None => continue,
            },
            None => None,
        };
        if a.removes_from_desktop()
            && let Some(r) = a.item()
        {
            placed.entry(r.to_string()).or_default().push(at.clone());
        }
        let name = item.map_or_else(|| a.item().unwrap_or("").to_string(), |i| i.name.clone());
        let need_writable = |errors: &mut Vec<String>| {
            if let Some(i) = item
                && !i.writable
            {
                errors.push(format!(
                    "{at}: {name} is not writable (Public Desktop or virtual) — layout only"
                ));
            }
        };
        let not_os = |errors: &mut Vec<String>| {
            if item.is_some_and(|i| i.hints.is_protected_os) {
                errors.push(format!("{at}: {name} is a protected OS file — do not move it, use hide_protected_os_files"));
            }
        };
        let need_item = |errors: &mut Vec<String>| {
            if item.is_none() {
                errors.push(format!("{at}: needs an existing item, not a new: folder"));
            }
        };
        match a {
            Action::MoveToFolder {
                dest, create_dirs, ..
            } => {
                need_item(&mut errors);
                need_writable(&mut errors);
                not_os(&mut errors);
                match expand(dest, ctx) {
                    Ok(d) => {
                        if has_dotdot(&d) {
                            errors.push(format!("{at}: dest must not contain '..'"));
                        } else if !ctx.allowed_roots.iter().any(|r| inside(&d, r)) {
                            errors.push(format!(
                                "{at}: dest {} is outside allowed_roots",
                                d.display()
                            ));
                        } else if lower(&d) == lower(&ctx.desktop) {
                            errors.push(format!(
                                "{at}: dest is the desktop itself — use layout instead"
                            ));
                        } else if item.is_some_and(|i| inside(&d, Path::new(&i.path))) {
                            errors.push(format!("{at}: cannot move {name} into itself"));
                        } else if !d.is_dir() && !create_dirs && !new_dests.contains(&lower(&d)) {
                            errors.push(format!(
                                "{at}: {} does not exist (set create_dirs: true)",
                                d.display()
                            ));
                        }
                    }
                    Err(e) => errors.push(format!("{at}: {e}")),
                }
            }
            Action::Quarantine { .. } => {
                need_item(&mut errors);
                need_writable(&mut errors);
                not_os(&mut errors);
            }
            Action::Rename { new_name, .. } => {
                need_item(&mut errors);
                need_writable(&mut errors);
                not_os(&mut errors);
                if let Err(e) = valid_name(new_name) {
                    errors.push(format!("{at}: new_name {new_name:?}: {e}"));
                }
            }
            Action::HideCaption { .. } => {
                need_item(&mut errors);
                need_writable(&mut errors);
                if item.is_some_and(|i| !matches!(i.kind, Kind::Shortcut | Kind::Url)) {
                    errors.push(format!("{at}: {name}: hide_caption is only for .lnk/.url"));
                }
            }
            Action::Dim { value, .. } => {
                need_item(&mut errors);
                need_writable(&mut errors);
                not_os(&mut errors);
                if *value && !ctx.shows_hidden {
                    errors.push(format!(
                        "{at}: Explorer hides hidden files, so dim would make {name} disappear (needs Hidden = 1)"
                    ));
                }
            }
            Action::FolderIcon { icon, .. } => {
                if let Some(i) = item {
                    need_writable(&mut errors);
                    if i.kind != Kind::Folder {
                        errors.push(format!("{at}: {name} is not a folder"));
                    }
                }
                if icon != "default" && !expand(icon, ctx).is_ok_and(|p| p.is_file()) {
                    errors.push(format!(
                        "{at}: icon must be \"default\" or an existing .ico"
                    ));
                }
            }
            Action::ExplorerSetting { .. } => {}
            Action::RestyleIcon { .. } => {
                errors.push(format!("{at}: restyle_icon arrives in M6 (icon styles)"));
            }
        }
    }

    // Every new: ref must be in a layout group.
    for r in new_refs.keys() {
        if !placed
            .get(r)
            .is_some_and(|v| v.iter().any(|w| w.starts_with("layout.")))
        {
            errors.push(format!(
                "{r}: folders created by the plan must be placed in a layout group"
            ));
        }
    }

    // Exactly-once rule over all real items (protected OS files are exempt).
    let mut missing = Vec::new();
    for i in ctx.items {
        match placed.get(&i.id) {
            None if i.hints.is_protected_os => {}
            None => missing.push(format!("{} ({})", i.name, i.id)),
            Some(v) if v.len() > 1 => errors.push(format!(
                "{} ({}) is in more than one place: {}",
                i.name,
                i.id,
                v.join(", ")
            )),
            _ => {}
        }
    }
    for (r, v) in &placed {
        if r.starts_with(NEW_PREFIX) && v.len() > 1 {
            errors.push(format!("{r} is in more than one place: {}", v.join(", ")));
        }
    }
    if !missing.is_empty() {
        missing.sort();
        errors.push(format!(
            "{} item(s) not placed anywhere: {}",
            missing.len(),
            missing.join(", ")
        ));
    }
    let hides_os = plan.actions.iter().any(|a| {
        matches!(
            a,
            Action::ExplorerSetting {
                key: crate::plan::model::ExplorerKey::HideProtectedOsFiles,
                value: true,
                ..
            }
        )
    });
    let os_files = ctx
        .items
        .iter()
        .filter(|i| i.hints.is_protected_os && !placed.contains_key(&i.id))
        .count();
    if os_files > 0 && !hides_os {
        out.warnings.push(format!("{os_files} protected OS file(s) (desktop.ini) stay visible; consider hide_protected_os_files"));
    }

    out.new_folders = new_refs.into_values().collect();
    out.new_folders.sort();
    out.after =
        out.groups.values().map(Vec::len).sum::<usize>() + if hides_os { 0 } else { os_files };
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

/// `"name (2).ext"`, `"name (3).ext"`… — first that is not taken. Never overwrite.
pub fn free_name(dir: &Path, name: &str, taken: &dyn Fn(&Path) -> bool) -> PathBuf {
    let first = dir.join(name);
    if !taken(&first) {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !taken(p))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::model::{Layout, LayoutEntry};
    use crate::scan::{Hints, Source};

    fn item(id: &str, name: &str, kind: Kind, source: Source) -> Item {
        Item {
            id: id.into(),
            name: name.into(),
            display_name: name.into(),
            path: format!(r"C:\sb\Desktop\{name}"),
            source,
            kind,
            ext: None,
            size: 1,
            created: None,
            modified: None,
            attributes: vec![],
            writable: source == Source::User,
            cell: None,
            group: None,
            caption_hidden: false,
            shortcut: None,
            folder: None,
            hints: Hints::default(),
            fingerprint: None,
        }
    }

    fn items() -> Vec<Item> {
        let mut ini = item("it_ini", "desktop.ini", Kind::File, Source::User);
        ini.hints.is_protected_os = true;
        vec![
            item("it_steam", "Steam.lnk", Kind::Shortcut, Source::Public),
            item("it_tg", "Telegram.lnk", Kind::Shortcut, Source::User),
            item("it_dev", "Dev", Kind::Folder, Source::User),
            item("it_r03", "robot_03 копия.png", Kind::File, Source::User),
            item("it_bin", "Корзина", Kind::Virtual, Source::Virtual),
            ini,
        ]
    }

    fn env(n: &str) -> Option<String> {
        (n == "USERPROFILE").then(|| r"C:\Users\X".to_string())
    }

    fn ctx(items: &[Item]) -> Ctx<'_> {
        Ctx {
            items,
            desktop: PathBuf::from(r"C:\sb\Desktop"),
            allowed_roots: vec![PathBuf::from(r"C:\sb\Desktop")],
            shows_hidden: true,
            env: &env,
        }
    }

    fn ids(v: &[&str]) -> Vec<LayoutEntry> {
        v.iter().map(|s| LayoutEntry::Id(s.to_string())).collect()
    }

    fn good() -> PlanInput {
        PlanInput {
            title: "Уборка".into(),
            layout: Layout {
                apps: ids(&["it_steam", "it_tg"]),
                projects: ids(&["it_dev", r"new:%DESKTOP%\Art"]),
                inbox: vec![],
                trash: ids(&["it_bin"]),
            },
            actions: vec![
                Action::Quarantine {
                    item: "it_r03".into(),
                    reason: "копия".into(),
                },
                Action::HideCaption {
                    item: "it_tg".into(),
                    reason: "без подписей".into(),
                },
            ],
        }
    }

    #[test]
    fn accepts_complete_plan() {
        let it = items();
        let c = validate(&good(), &ctx(&it)).unwrap();
        assert_eq!(c.new_folders, vec![PathBuf::from(r"C:\sb\Desktop\Art")]);
        assert_eq!((c.before, c.after), (6, 6)); // 5 placed + desktop.ini still visible
        assert!(
            c.warnings
                .iter()
                .any(|w| w.contains("desktop.ini") || w.contains("protected"))
        );
    }

    #[test]
    fn rejects_unplaced_items_with_names() {
        let it = items();
        let mut p = good();
        p.layout.apps.retain(|e| e.item() != "it_tg");
        p.actions.retain(|a| a.item() != Some("it_tg"));
        let e = validate(&p, &ctx(&it)).unwrap_err();
        assert!(
            e.iter()
                .any(|m| m.contains("not placed") && m.contains("Telegram.lnk")),
            "{e:?}"
        );
    }

    #[test]
    fn rejects_double_placement_and_unknown_ids() {
        let it = items();
        let mut p = good();
        p.layout.inbox = ids(&["it_r03", "it_nope"]);
        let e = validate(&p, &ctx(&it)).unwrap_err();
        assert!(e.iter().any(|m| m.contains("more than one place")), "{e:?}");
        assert!(e.iter().any(|m| m.contains("it_nope")), "{e:?}");
    }

    #[test]
    fn rejects_dest_outside_roots_and_traversal() {
        let it = items();
        for dest in [
            r"%USERPROFILE%\Documents",
            r"%DESKTOP%\..\Escape",
            r"C:\Windows",
            "%DESKTOP%",
        ] {
            let mut p = good();
            p.actions[0] = Action::MoveToFolder {
                item: "it_r03".into(),
                dest: dest.into(),
                create_dirs: true,
                reason: "r".into(),
            };
            assert!(validate(&p, &ctx(&it)).is_err(), "{dest} must be rejected");
        }
        let mut p = good();
        p.actions[0] = Action::MoveToFolder {
            item: "it_r03".into(),
            dest: r"%DESKTOP%\Art".into(),
            create_dirs: false,
            reason: "r".into(),
        };
        assert!(
            validate(&p, &ctx(&it)).is_ok(),
            "new: folder is a valid dest"
        );
    }

    #[test]
    fn enforces_action_rules() {
        let it = items();
        let cases: Vec<(Action, &str)> = vec![
            (
                Action::Quarantine {
                    item: "it_steam".into(),
                    reason: "r".into(),
                },
                "not writable",
            ),
            (
                Action::Quarantine {
                    item: "it_ini".into(),
                    reason: "r".into(),
                },
                "protected OS",
            ),
            (
                Action::HideCaption {
                    item: "it_dev".into(),
                    reason: "r".into(),
                },
                "only for .lnk",
            ),
            (
                Action::Rename {
                    item: "it_dev".into(),
                    new_name: "a:b".into(),
                    reason: "r".into(),
                },
                "forbidden",
            ),
            (
                Action::Rename {
                    item: "it_dev".into(),
                    new_name: "CON".into(),
                    reason: "r".into(),
                },
                "reserved",
            ),
            (
                Action::Rename {
                    item: "it_bin".into(),
                    new_name: "x".into(),
                    reason: "r".into(),
                },
                "not writable",
            ),
            (
                Action::Dim {
                    item: "it_dev".into(),
                    value: true,
                    reason: " ".into(),
                },
                "reason is required",
            ),
            (
                Action::FolderIcon {
                    item: "it_r03".into(),
                    icon: "default".into(),
                    reason: "r".into(),
                },
                "not a folder",
            ),
        ];
        for (a, want) in cases {
            let mut p = good();
            p.actions.push(a.clone());
            let e = validate(&p, &ctx(&it)).unwrap_err();
            assert!(
                e.iter().any(|m| m.contains(want)),
                "{a:?}: want {want:?} in {e:?}"
            );
        }
    }

    #[test]
    fn dim_needs_hidden_files_shown() {
        let it = items();
        let mut c = ctx(&it);
        c.shows_hidden = false;
        let mut p = good();
        p.actions.push(Action::Dim {
            item: "it_dev".into(),
            value: true,
            reason: "old".into(),
        });
        assert!(
            validate(&p, &c)
                .unwrap_err()
                .iter()
                .any(|m| m.contains("Hidden = 1"))
        );
    }

    #[test]
    fn new_folders_must_be_placed_and_unique() {
        let it = items();
        let mut p = good();
        p.layout.projects = ids(&["it_dev"]);
        p.actions.push(Action::FolderIcon {
            item: r"new:%DESKTOP%\Art".into(),
            icon: "default".into(),
            reason: "r".into(),
        });
        assert!(
            validate(&p, &ctx(&it))
                .unwrap_err()
                .iter()
                .any(|m| m.contains("must be placed"))
        );
        let mut p = good();
        p.layout
            .projects
            .push(LayoutEntry::Id(r"new:%DESKTOP%\Dev".into()));
        assert!(
            validate(&p, &ctx(&it))
                .unwrap_err()
                .iter()
                .any(|m| m.contains("already exists"))
        );
        let mut p = good();
        p.layout
            .projects
            .push(LayoutEntry::Id(r"new:%DESKTOP%\Sub\Deep".into()));
        assert!(validate(&p, &ctx(&it)).is_err());
    }

    #[test]
    fn free_names_never_overwrite() {
        let taken: HashSet<PathBuf> = [r"C:\d\robot.png", r"C:\d\robot (2).png", r"C:\d\Dev"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let t = |p: &Path| taken.contains(p);
        assert_eq!(
            free_name(Path::new(r"C:\d"), "robot.png", &t),
            PathBuf::from(r"C:\d\robot (3).png")
        );
        assert_eq!(
            free_name(Path::new(r"C:\d"), "Dev", &t),
            PathBuf::from(r"C:\d\Dev (2)")
        );
        assert_eq!(
            free_name(Path::new(r"C:\d"), "new.txt", &t),
            PathBuf::from(r"C:\d\new.txt")
        );
    }
}
