//! Rough grouping used only to exercise the layout in M3 (`layout --dry-run`).
//! Real sorting is Claude's job via plans (M4).

use std::collections::BTreeMap;

use crate::layout::engine::{Entry, Group};
use crate::scan::{Item, Kind};

pub const MAX_PROJECTS: usize = 5;

pub fn group(items: &[Item]) -> BTreeMap<Group, Vec<Entry>> {
    let mut g: BTreeMap<Group, Vec<Entry>> = BTreeMap::new();
    let entry = |i: &Item| Entry {
        id: i.id.clone(),
        pin: None,
    };

    let mut folders: Vec<&Item> = items.iter().filter(|i| i.kind == Kind::Folder).collect();
    // Most recently changed first.
    folders.sort_by(|a, b| {
        let key = |i: &Item| {
            i.folder
                .as_ref()
                .and_then(|f| f.newest_modified.clone())
                .or_else(|| i.modified.clone())
        };
        key(b).cmp(&key(a)).then_with(|| a.name.cmp(&b.name))
    });
    let projects: Vec<&str> = folders
        .iter()
        .take(MAX_PROJECTS)
        .map(|i| i.id.as_str())
        .collect();

    for i in items {
        let grp = match i.kind {
            Kind::Virtual
                if i.path
                    .eq_ignore_ascii_case("::{645FF040-5081-101B-9F08-00AA002F954E}") =>
            {
                Group::Trash
            }
            Kind::Folder if projects.contains(&i.id.as_str()) => Group::Projects,
            Kind::Shortcut | Kind::Url
                if i.shortcut
                    .as_ref()
                    .is_none_or(|s| s.target_exists != Some(false)) =>
            {
                Group::Apps
            }
            _ => Group::Inbox,
        };
        g.entry(grp).or_default().push(entry(i));
    }
    g
}
