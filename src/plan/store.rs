//! Plan storage: `%LOCALAPPDATA%\Nook\plans\<id>.json` (atomic writes), journals in `journal\`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::fsutil::write_atomic;
use crate::plan::model::{PlanState, StoredPlan};

#[derive(Debug, Clone)]
pub struct Store {
    pub plans: PathBuf,
    pub journal: PathBuf,
    pub positions: PathBuf,
}

impl Store {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            plans: data_dir.join("plans"),
            journal: data_dir.join("journal"),
            positions: data_dir.join("positions"),
        }
    }

    pub fn new_id() -> String {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let h = blake3::hash(&n.to_le_bytes());
        format!("pl_{}", &crate::scan::make_id(h.as_bytes())[3..])
    }

    fn file(&self, id: &str) -> PathBuf {
        self.plans.join(format!("{id}.json"))
    }

    pub fn save(&self, p: &StoredPlan) -> Result<()> {
        write_atomic(&self.file(&p.id), &serde_json::to_vec_pretty(p)?)
    }

    pub fn load(&self, id: &str) -> Result<StoredPlan> {
        if !id.starts_with("pl_") || id.contains(['\\', '/', '.']) {
            anyhow::bail!("bad plan id {id:?}");
        }
        let t = std::fs::read_to_string(self.file(id)).with_context(|| format!("no plan {id}"))?;
        Ok(serde_json::from_str(&t)?)
    }

    /// All plans, newest first.
    pub fn list(&self) -> Vec<StoredPlan> {
        let mut v: Vec<StoredPlan> = std::fs::read_dir(&self.plans)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|t| serde_json::from_str(&t).ok())
            .collect();
        v.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| b.id.cmp(&a.id)));
        v
    }

    pub fn pending(&self) -> Option<StoredPlan> {
        self.list()
            .into_iter()
            .find(|p| p.state == PlanState::Pending)
    }

    /// Latest applied (not undone) plan, optionally for one mode only.
    pub fn last_applied(&self, sandbox: bool) -> Option<StoredPlan> {
        self.list()
            .into_iter()
            .find(|p| p.state == PlanState::Applied && p.sandbox == sandbox)
    }

    pub fn journal_file(&self, id: &str) -> PathBuf {
        self.journal.join(format!("{id}.jsonl"))
    }
}
