//! Plan as Claude sends it (spec §5.2) and as Nook stores it.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Reference to a desktop item: `it_xxxxxxxx`, or `new:%DESKTOP%\Name` for a folder the plan creates.
pub type ItemRef = String;

pub const NEW_PREFIX: &str = "new:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Pin {
    pub col: i32,
    pub row: i32,
}

/// `"it_x"` or `{ "item": "it_x", "pin": { "col": 1, "row": 1 } }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum LayoutEntry {
    Id(ItemRef),
    Pinned { item: ItemRef, pin: Option<Pin> },
}

impl LayoutEntry {
    pub fn item(&self) -> &str {
        match self {
            LayoutEntry::Id(s) => s,
            LayoutEntry::Pinned { item, .. } => item,
        }
    }
    pub fn pin(&self) -> Option<Pin> {
        match self {
            LayoutEntry::Id(_) => None,
            LayoutEntry::Pinned { pin, .. } => *pin,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    #[serde(default)]
    pub apps: Vec<LayoutEntry>,
    #[serde(default)]
    pub projects: Vec<LayoutEntry>,
    #[serde(default)]
    pub inbox: Vec<LayoutEntry>,
    #[serde(default)]
    pub trash: Vec<LayoutEntry>,
}

impl Layout {
    pub fn groups(&self) -> [(&'static str, &Vec<LayoutEntry>); 4] {
        [
            ("apps", &self.apps),
            ("projects", &self.projects),
            ("inbox", &self.inbox),
            ("trash", &self.trash),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExplorerKey {
    HideProtectedOsFiles,
    AutoArrangeOff,
    HideThisPcIcon,
}

/// Whitelisted actions (spec §5.3). Every action needs a non-empty `reason`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    MoveToFolder {
        item: ItemRef,
        /// Destination folder, e.g. `%DESKTOP%\Art`.
        dest: String,
        #[serde(default)]
        create_dirs: bool,
        reason: String,
    },
    Quarantine {
        item: ItemRef,
        reason: String,
    },
    Rename {
        item: ItemRef,
        new_name: String,
        reason: String,
    },
    HideCaption {
        item: ItemRef,
        reason: String,
    },
    Dim {
        item: ItemRef,
        #[serde(default = "yes")]
        value: bool,
        reason: String,
    },
    FolderIcon {
        item: ItemRef,
        /// `"default"` or a path to an `.ico`.
        icon: String,
        reason: String,
    },
    ExplorerSetting {
        key: ExplorerKey,
        value: bool,
        reason: String,
    },
    RestyleIcon {
        item: ItemRef,
        style: String,
        reason: String,
    },
}

fn yes() -> bool {
    true
}

impl Action {
    pub fn op(&self) -> &'static str {
        match self {
            Action::MoveToFolder { .. } => "move_to_folder",
            Action::Quarantine { .. } => "quarantine",
            Action::Rename { .. } => "rename",
            Action::HideCaption { .. } => "hide_caption",
            Action::Dim { .. } => "dim",
            Action::FolderIcon { .. } => "folder_icon",
            Action::ExplorerSetting { .. } => "explorer_setting",
            Action::RestyleIcon { .. } => "restyle_icon",
        }
    }

    pub fn item(&self) -> Option<&str> {
        match self {
            Action::MoveToFolder { item, .. }
            | Action::Quarantine { item, .. }
            | Action::Rename { item, .. }
            | Action::HideCaption { item, .. }
            | Action::Dim { item, .. }
            | Action::FolderIcon { item, .. }
            | Action::RestyleIcon { item, .. } => Some(item),
            Action::ExplorerSetting { .. } => None,
        }
    }

    pub fn reason(&self) -> &str {
        match self {
            Action::MoveToFolder { reason, .. }
            | Action::Quarantine { reason, .. }
            | Action::Rename { reason, .. }
            | Action::HideCaption { reason, .. }
            | Action::Dim { reason, .. }
            | Action::FolderIcon { reason, .. }
            | Action::ExplorerSetting { reason, .. }
            | Action::RestyleIcon { reason, .. } => reason,
        }
    }

    /// Takes the item off the desktop.
    pub fn removes_from_desktop(&self) -> bool {
        matches!(
            self,
            Action::MoveToFolder { .. } | Action::Quarantine { .. }
        )
    }
}

/// What Claude sends to `propose_plan`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanInput {
    pub title: String,
    #[serde(default)]
    pub layout: Layout,
    #[serde(default)]
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    Pending,
    Applied,
    Rejected,
    Superseded,
    Failed,
    Cancelled,
    Undone,
}

/// Identity of an item at proposal time; apply refuses if it changed ("rescan").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    pub path: String,
    pub size: u64,
    pub modified: Option<String>,
    pub file_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub step: usize,
    pub op: String,
    pub item: Option<String>,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPlan {
    pub id: String,
    pub created: String,
    pub state: PlanState,
    pub sandbox: bool,
    pub plan: PlanInput,
    /// "62 → 17"
    pub summary: String,
    pub before: usize,
    pub after: usize,
    pub warnings: Vec<String>,
    /// item id → identity at proposal.
    pub seen: BTreeMap<String, Seen>,
    #[serde(default)]
    pub results: Vec<StepResult>,
    #[serde(default)]
    pub applied_at: Option<String>,
    #[serde(default)]
    pub previewed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spec_example() {
        let json = r#"{
          "title": "Первая уборка",
          "layout": {
            "apps":     [{ "item": "it_steam", "pin": { "col": 1, "row": 1 } }, "it_spotify", "it_blender"],
            "projects": ["new:%DESKTOP%\\Dev", "it_bkillustr", "new:%DESKTOP%\\GD"],
            "inbox":    ["it_robot02"],
            "trash":    ["it_recyclebin"]
          },
          "actions": [
            { "op": "move_to_folder", "item": "it_lexpsd", "dest": "%DESKTOP%\\Art", "create_dirs": true, "reason": "исходники blitzkrieg" },
            { "op": "quarantine", "item": "it_r03c2", "reason": "копия robot_03.png, тот же хеш" },
            { "op": "hide_caption", "item": "it_steam", "reason": "приложения без подписей" },
            { "op": "dim", "item": "it_learnhtml", "value": true, "reason": "не менялась 5 месяцев" },
            { "op": "folder_icon", "item": "new:%DESKTOP%\\Dev", "icon": "default", "reason": "единый стиль" },
            { "op": "explorer_setting", "key": "hide_protected_os_files", "value": true, "reason": "спрятать desktop.ini" }
          ]
        }"#;
        let p: PlanInput = serde_json::from_str(json).unwrap();
        assert_eq!(p.layout.apps[0].item(), "it_steam");
        assert_eq!(p.layout.apps[0].pin(), Some(Pin { col: 1, row: 1 }));
        assert_eq!(p.layout.projects[0].item(), r"new:%DESKTOP%\Dev");
        assert_eq!(p.actions.len(), 6);
        assert_eq!(p.actions[1].op(), "quarantine");
        assert!(p.actions[0].removes_from_desktop());
    }

    #[test]
    fn unknown_op_and_fields_rejected() {
        assert!(
            serde_json::from_str::<Action>(r#"{"op":"delete","item":"x","reason":"y"}"#).is_err()
        );
        assert!(
            serde_json::from_str::<Action>(
                r#"{"op":"quarantine","item":"x","reason":"y","force":true}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<PlanInput>(r#"{"title":"t","layout":{"desk":[]}}"#).is_err()
        );
    }
}
