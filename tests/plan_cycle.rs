//! Full plan cycle in a throwaway sandbox: propose → apply → undo, rollback on failure,
//! stale plans, name conflicts, quarantine. Never touches the real desktop.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use nook::config::Config;
use nook::paths::Desktops;
use nook::plan::apply::{self, Env, NoDesk};
use nook::plan::model::{PlanInput, PlanState};
use nook::plan::store::Store;
use nook::platform::ComApartment;
use nook::scan::{ScanResult, scan};

struct Sb {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    desk: Desktops,
    store: Store,
    config: Config,
}

impl Sb {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sb");
        let desk = Desktops::sandbox(root.clone());
        let d = &desk.user;
        std::fs::create_dir_all(d.join("Dev")).unwrap();
        std::fs::create_dir_all(&desk.public).unwrap();
        std::fs::write(d.join("Dev").join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(d.join("blitzkrieg.psd"), "PSD").unwrap();
        std::fs::write(d.join("robot_03.png"), "PNG-A").unwrap();
        std::fs::write(d.join("robot_03 копия.png"), "PNG-A").unwrap();
        std::fs::write(d.join("Семен КС конфиги.txt"), "sens 2.1").unwrap();
        std::fs::write(
            d.join("Steam.url"),
            "[InternetShortcut]\r\nURL=steam://open/main\r\n",
        )
        .unwrap();
        let store = Store::new(&tmp.path().join("appdata"));
        Sb {
            _tmp: tmp,
            root,
            desk,
            store,
            config: Config::default(),
        }
    }

    fn env(&self) -> Env<'_> {
        Env {
            desktops: &self.desk,
            config: &self.config,
            store: &self.store,
            desk: &NoDesk,
            shows_hidden: true,
            fail_at_step: None,
            icons_dir: self._tmp.path().join("icons"),
        }
    }

    fn scan(&self) -> ScanResult {
        scan(&self.desk).unwrap()
    }

    fn id(&self, s: &ScanResult, name: &str) -> String {
        s.items
            .iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("no {name}"))
            .id
            .clone()
    }

    /// Everything under the sandbox desktops: (relative path, attrs, content) — "scan equality".
    fn state(&self) -> BTreeSet<(String, u32, Vec<u8>)> {
        fn walk(base: &Path, p: &Path, out: &mut BTreeSet<(String, u32, Vec<u8>)>) {
            for e in std::fs::read_dir(p).unwrap().flatten() {
                let path = e.path();
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let attrs = nook::plan::fsops::get_attrs(&path).unwrap() & 0x7; // RO/H/S
                if path.is_dir() {
                    out.insert((rel, attrs, vec![]));
                    walk(base, &path, out);
                } else {
                    out.insert((rel, attrs, std::fs::read(&path).unwrap()));
                }
            }
        }
        let mut out = BTreeSet::new();
        walk(&self.root, &self.desk.user, &mut out);
        walk(&self.root, &self.desk.public, &mut out);
        out
    }

    fn plan(&self, s: &ScanResult) -> PlanInput {
        let id = |n: &str| self.id(s, n);
        let bin = s
            .items
            .iter()
            .find(|i| i.kind == nook::scan::Kind::Virtual)
            .map(|i| i.id.clone());
        let mut json = serde_json::json!({
            "title": "Тест",
            "layout": {
                "apps": [id("Steam.url")],
                "projects": [id("Dev"), "new:%DESKTOP%\\Art"],
                "inbox": [id("Семен КС конфиги.txt"), id("robot_03.png")],
                "trash": []
            },
            "actions": [
                { "op": "move_to_folder", "item": id("blitzkrieg.psd"), "dest": "%DESKTOP%\\Art", "reason": "исходники" },
                { "op": "quarantine", "item": id("robot_03 копия.png"), "reason": "дубль robot_03.png" },
                { "op": "hide_caption", "item": id("Steam.url"), "reason": "без подписей" },
                { "op": "dim", "item": id("Dev"), "value": true, "reason": "давно не менялась" },
                { "op": "folder_icon", "item": id("Dev"), "icon": "default", "reason": "единый стиль" },
                { "op": "explorer_setting", "key": "hide_protected_os_files", "value": true, "reason": "desktop.ini" }
            ]
        });
        if let Some(v) = bin {
            json["layout"]["trash"] = serde_json::json!([v]);
        }
        serde_json::from_value(json).unwrap()
    }
}

#[test]
fn propose_apply_undo_restores_everything() {
    let _com = ComApartment::sta().unwrap();
    let sb = Sb::new();
    let before = sb.state();
    let s = sb.scan();
    let p = apply::propose(sb.plan(&s), &s, &sb.env())
        .unwrap()
        .expect("plan must validate");
    assert!(p.sandbox);

    let done = apply::apply(&p.plan_id, &sb.scan(), &sb.env()).unwrap();
    assert_eq!(done.state, PlanState::Applied, "{:#?}", done.results);
    let d = &sb.desk.user;
    assert!(d.join("Art").join("blitzkrieg.psd").is_file());
    assert!(!d.join("robot_03 копия.png").exists());
    assert!(
        sb.root
            .join("quarantine")
            .join(&p.plan_id)
            .join("robot_03 копия.png")
            .is_file()
    );
    assert!(d.join("\u{2800}.url").is_file(), "caption hidden");
    assert!(d.join("Dev").join("desktop.ini").is_file());
    // Dim on a folder with icon styles on = muted icon via desktop.ini, not the Hidden attribute.
    assert_eq!(
        nook::plan::fsops::get_attrs(&d.join("Dev")).unwrap() & 0x2,
        0,
        "not hidden"
    );
    assert!(
        done.results
            .iter()
            .any(|r| r.op == "explorer_setting" && r.detail.contains("simulated"))
    );

    let q = apply::quarantine_list(&sb.env()).unwrap();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].reason, "дубль robot_03.png");

    let undone = apply::undo(None, &sb.env()).unwrap();
    assert_eq!(undone.state, PlanState::Undone);
    assert_eq!(
        sb.state(),
        before,
        "desktop must be exactly as before the plan"
    );
    // Nothing was deleted: the created folder is parked in the plan's quarantine.
    assert!(
        sb.root
            .join("quarantine")
            .join(&p.plan_id)
            .join("_undo")
            .join("Art")
            .is_dir()
    );
}

#[test]
fn icon_styles_apply_and_undo_exactly() {
    let _com = ComApartment::sta().unwrap();
    let sb = Sb::new();
    let before = sb.state();
    let s = sb.scan();
    let mut plan = sb.plan(&s);
    plan.actions.retain(|a| {
        !matches!(
            a,
            nook::plan::model::Action::Dim { .. } | nook::plan::model::Action::FolderIcon { .. }
        )
    });
    let id = |n: &str| sb.id(&s, n);
    plan.actions.extend(
        serde_json::from_value::<Vec<nook::plan::model::Action>>(serde_json::json!([
            { "op": "restyle_icon", "item": id("Steam.url"), "style": "tile", "reason": "apps" },
            { "op": "restyle_icon", "item": "new:%DESKTOP%\\Art", "style": "orb", "reason": "projects" },
            { "op": "dim", "item": id("Dev"), "value": true, "reason": "inactive" }
        ]))
        .unwrap(),
    );
    let p = apply::propose(plan, &s, &sb.env()).unwrap().expect("valid");
    let done = apply::apply(&p.plan_id, &sb.scan(), &sb.env()).unwrap();
    assert_eq!(done.state, PlanState::Applied, "{:#?}", done.results);

    let url = std::fs::read_to_string(sb.desk.user.join("\u{2800}.url")).unwrap();
    assert!(
        url.contains("IconFile=") && url.contains("steam://open/main"),
        "{url}"
    );
    let art_ini = std::fs::read(sb.desk.user.join("Art").join("desktop.ini")).unwrap();
    assert!(
        String::from_utf16_lossy(
            &art_ini[2..]
                .chunks(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<_>>()
        )
        .contains(".ico,0")
    );
    // Dim on a styled folder: muted icon, no Hidden attribute.
    assert_eq!(
        nook::plan::fsops::get_attrs(&sb.desk.user.join("Dev")).unwrap() & 0x2,
        0
    );
    assert!(sb.desk.user.join("Dev").join("desktop.ini").is_file());
    let icos = std::fs::read_dir(sb._tmp.path().join("icons"))
        .unwrap()
        .count();
    assert_eq!(icos, 3, "tile, orb, muted");

    let steam_id = sb.id(&s, "Steam.url");
    apply::undo(None, &sb.env()).unwrap();
    assert_eq!(
        sb.id(&sb.scan(), "Steam.url"),
        steam_id,
        "undo must keep the file identity (FileId)"
    );
    assert_eq!(sb.state(), before);
}

#[test]
fn failure_mid_plan_rolls_back() {
    let _com = ComApartment::sta().unwrap();
    let sb = Sb::new();
    let before = sb.state();
    let s = sb.scan();
    let p = apply::propose(sb.plan(&s), &s, &sb.env()).unwrap().unwrap();
    let mut env = sb.env();
    env.fail_at_step = Some(5); // after the new folder, a rename and attributes are done
    let r = apply::apply(&p.plan_id, &sb.scan(), &env).unwrap();
    assert_eq!(r.state, PlanState::Failed);
    assert!(r.results.iter().any(|x| x.status == "failed"));
    assert!(
        r.results
            .iter()
            .any(|x| x.op == "rollback" && x.status == "done"),
        "{:#?}",
        r.results
    );
    assert_eq!(sb.state(), before);
}

#[test]
fn stale_plan_is_refused() {
    let _com = ComApartment::sta().unwrap();
    let sb = Sb::new();
    let s = sb.scan();
    let p = apply::propose(sb.plan(&s), &s, &sb.env()).unwrap().unwrap();
    std::fs::write(
        sb.desk.user.join("blitzkrieg.psd"),
        "PSD edited in Photoshop",
    )
    .unwrap();
    let e = apply::apply(&p.plan_id, &sb.scan(), &sb.env()).unwrap_err();
    assert!(format!("{e:#}").contains("rescan"), "{e:#}");
}

#[test]
fn new_plan_supersedes_pending_and_unplaced_is_rejected() {
    let _com = ComApartment::sta().unwrap();
    let sb = Sb::new();
    let s = sb.scan();
    let first = apply::propose(sb.plan(&s), &s, &sb.env()).unwrap().unwrap();
    let second = apply::propose(sb.plan(&s), &s, &sb.env()).unwrap().unwrap();
    assert_eq!(second.superseded.as_deref(), Some(first.plan_id.as_str()));
    assert_eq!(
        sb.store.load(&first.plan_id).unwrap().state,
        PlanState::Superseded
    );

    let mut bad = sb.plan(&s);
    bad.layout.inbox.clear();
    let errors = apply::propose(bad, &s, &sb.env()).unwrap().unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.contains("not placed") && e.contains("robot_03.png")),
        "{errors:?}"
    );
}

#[test]
fn name_conflicts_get_suffix_and_quarantine_restores() {
    let _com = ComApartment::sta().unwrap();
    let sb = Sb::new();
    std::fs::create_dir(sb.desk.user.join("Art")).unwrap();
    std::fs::write(sb.desk.user.join("Art").join("blitzkrieg.psd"), "OLDER PSD").unwrap();
    let s = sb.scan();
    let mut plan = sb.plan(&s);
    // Art exists now: use its id instead of new:.
    let art = sb.id(&s, "Art");
    plan.layout.projects[1] = nook::plan::model::LayoutEntry::Id(art);
    let p = apply::propose(plan, &s, &sb.env()).unwrap().unwrap();
    let done = apply::apply(&p.plan_id, &sb.scan(), &sb.env()).unwrap();
    assert_eq!(done.state, PlanState::Applied, "{:#?}", done.results);
    let art = sb.desk.user.join("Art");
    assert_eq!(
        std::fs::read_to_string(art.join("blitzkrieg.psd")).unwrap(),
        "OLDER PSD",
        "never overwrite"
    );
    assert_eq!(
        std::fs::read_to_string(art.join("blitzkrieg (2).psd")).unwrap(),
        "PSD"
    );

    // Restore from quarantine while a file with the same name came back meanwhile.
    std::fs::write(sb.desk.user.join("robot_03 копия.png"), "NEW").unwrap();
    let q = apply::quarantine_list(&sb.env()).unwrap();
    let rep = apply::quarantine_restore(&[q[0].id.clone()], &sb.env()).unwrap();
    assert!(rep[0].contains("restored"), "{rep:?}");
    assert_eq!(
        std::fs::read_to_string(sb.desk.user.join("robot_03 копия (2).png")).unwrap(),
        "PNG-A"
    );
    assert_eq!(
        std::fs::read_to_string(sb.desk.user.join("robot_03 копия.png")).unwrap(),
        "NEW"
    );
    assert!(apply::quarantine_list(&sb.env()).unwrap().is_empty());
}
