//! Write-ahead journal: `journal\<plan-id>.jsonl`. Before each step an `intent` line is
//! written, after it a `done`/`failed`/`skipped` line. Undo replays `done` steps backwards.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A reversible step. Everything needed to undo it is recorded here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    /// Folder created by the plan. Undo moves it (empty) into quarantine — never deletes.
    CreateDir {
        path: PathBuf,
        park: PathBuf,
    },
    /// Move/rename. `item` is the scan id when known.
    Move {
        item: Option<String>,
        from: PathBuf,
        to: PathBuf,
        why: String,
    },
    SetAttrs {
        path: PathBuf,
        old: u32,
        new: u32,
    },
    /// File written by the plan (desktop.ini). `backup` holds the previous content if any;
    /// `park` is where the new file goes on undo when there was no previous one.
    WriteFile {
        path: PathBuf,
        backup: Option<PathBuf>,
        park: PathBuf,
        old_attrs: Option<u32>,
    },
    /// HKCU DWORD. `old: None` = value did not exist.
    Registry {
        key: String,
        value: String,
        old: Option<u32>,
        new: u32,
        simulated: bool,
    },
    /// Desktop view auto-arrange flag.
    AutoArrange {
        old: bool,
        new: bool,
        simulated: bool,
    },
    /// Icon positions before apply (live desktop only).
    Positions {
        snapshot: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mark {
    Intent,
    Done,
    Failed,
    Skipped,
    Undone,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Line {
    pub seq: usize,
    pub mark: Mark,
    pub step: Step,
    #[serde(default)]
    pub note: String,
}

pub struct Journal {
    path: PathBuf,
    file: File,
    next: usize,
}

impl Journal {
    pub fn open(dir: &Path, plan_id: &str) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{plan_id}.jsonl"));
        let next = read(&path)
            .map(|l| l.iter().map(|x| x.seq + 1).max().unwrap_or(0))
            .unwrap_or(0);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        // After a crash the last line may be torn; start on a fresh line so new entries parse.
        if std::fs::read(&path).is_ok_and(|b| b.last().is_some_and(|&c| c != b'\n')) {
            file.write_all(b"\n")?;
        }
        Ok(Self { path, file, next })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write(&mut self, line: &Line) -> Result<()> {
        let mut s = serde_json::to_vec(line)?;
        s.push(b'\n');
        self.file.write_all(&s)?;
        self.file.sync_data()?; // write-ahead: must hit disk before the action
        Ok(())
    }

    /// Records the intent; returns the sequence number to close it with.
    pub fn intent(&mut self, step: &Step) -> Result<usize> {
        let seq = self.next;
        self.next += 1;
        self.write(&Line {
            seq,
            mark: Mark::Intent,
            step: step.clone(),
            note: String::new(),
        })?;
        Ok(seq)
    }

    pub fn close(
        &mut self,
        seq: usize,
        mark: Mark,
        step: &Step,
        note: impl Into<String>,
    ) -> Result<()> {
        self.write(&Line {
            seq,
            mark,
            step: step.clone(),
            note: note.into(),
        })
    }
}

pub fn read(path: &Path) -> Result<Vec<Line>> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut out = Vec::new();
    for l in BufReader::new(f).lines() {
        let l = l?;
        if l.trim().is_empty() {
            continue;
        }
        // A torn last line (crash mid-write) is ignored.
        if let Ok(x) = serde_json::from_str::<Line>(&l) {
            out.push(x);
        }
    }
    Ok(out)
}

/// Final mark per seq: steps whose last mark is `Done` (still to be undone), latest first.
pub fn done_steps(lines: &[Line]) -> Vec<(usize, Step)> {
    let mut last: std::collections::BTreeMap<usize, (Mark, Step)> =
        std::collections::BTreeMap::new();
    for l in lines {
        last.insert(l.seq, (l.mark, l.step.clone()));
    }
    last.into_iter()
        .rev()
        .filter(|(_, (m, _))| *m == Mark::Done)
        .map(|(s, (_, st))| (s, st))
        .collect()
}

/// Intents never closed: a crash happened mid-step.
pub fn dangling(lines: &[Line]) -> Vec<(usize, Step)> {
    let mut last: std::collections::BTreeMap<usize, (Mark, Step)> =
        std::collections::BTreeMap::new();
    for l in lines {
        last.insert(l.seq, (l.mark, l.step.clone()));
    }
    last.into_iter()
        .filter(|(_, (m, _))| *m == Mark::Intent)
        .map(|(s, (_, st))| (s, st))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_ahead_and_replay_order() {
        let d = tempfile::tempdir().unwrap();
        let mv = |n: &str| Step::Move {
            item: None,
            from: n.into(),
            to: format!("{n}.x").into(),
            why: "t".into(),
        };
        let mut j = Journal::open(d.path(), "pl_test").unwrap();
        let a = j.intent(&mv("a")).unwrap();
        j.close(a, Mark::Done, &mv("a"), "").unwrap();
        let b = j.intent(&mv("b")).unwrap();
        j.close(b, Mark::Skipped, &mv("b"), "busy").unwrap();
        let c = j.intent(&mv("c")).unwrap();
        j.close(c, Mark::Done, &mv("c"), "").unwrap();
        let _d = j.intent(&mv("d")).unwrap(); // crash: never closed
        drop(j);

        // Torn tail line must not break reading.
        std::fs::OpenOptions::new()
            .append(true)
            .open(d.path().join("pl_test.jsonl"))
            .unwrap()
            .write_all(b"{\"seq\":9,\"ma")
            .unwrap();
        let lines = read(&d.path().join("pl_test.jsonl")).unwrap();
        let done: Vec<usize> = done_steps(&lines).into_iter().map(|(s, _)| s).collect();
        assert_eq!(done, vec![c, a], "latest first, skipped excluded");
        assert_eq!(dangling(&lines).len(), 1);

        // Reopen continues numbering.
        let mut j = Journal::open(d.path(), "pl_test").unwrap();
        assert_eq!(j.intent(&mv("e")).unwrap(), 4);
    }
}
