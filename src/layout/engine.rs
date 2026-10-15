//! Layout engine: (grid, free zones, groups, pins) → cells. Pure and deterministic.
//!
//! Each group grows from its corner (configurable). Fill patterns, relative to the corner:
//! - apps: bands `apps_columns` wide; inside a band row by row away from the corner,
//!   then the next band further from the edge;
//! - projects: a column away from the corner, then the next column toward the center;
//! - inbox, trash: a row along the edge away from the corner, then the next row inward.
//!
//! Blocked cells and cells taken by other groups are skipped. Placement order:
//! pins, then trash, inbox, projects, apps.

use std::collections::{BTreeMap, HashSet};

use serde::Serialize;

use crate::config::{Corner, LayoutConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct Cell {
    pub col: i32,
    pub row: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Group {
    Apps,
    Projects,
    Inbox,
    Trash,
}

impl Group {
    pub fn letter(self) -> char {
        match self {
            Group::Apps => 'A',
            Group::Projects => 'P',
            Group::Inbox => 'I',
            Group::Trash => 'T',
        }
    }

    fn corner(self, cfg: &LayoutConfig) -> Corner {
        match self {
            Group::Apps => cfg.corners.apps,
            Group::Projects => cfg.corners.projects,
            Group::Inbox => cfg.corners.inbox,
            Group::Trash => cfg.corners.trash,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub pin: Option<Cell>,
}

#[derive(Debug, Clone, Default)]
pub struct Input {
    pub cols: i32,
    pub rows: i32,
    /// `blocked[row][col]`
    pub blocked: Vec<Vec<bool>>,
    pub groups: BTreeMap<Group, Vec<Entry>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Output {
    pub cells: BTreeMap<String, Cell>,
    pub group_of: BTreeMap<String, Group>,
    /// Items that did not fit (or whose pin was invalid and no cell was left).
    pub overflow: Vec<String>,
    pub warnings: Vec<String>,
}

struct Taken<'a> {
    input: &'a Input,
    used: HashSet<Cell>,
}

impl Taken<'_> {
    fn free(&self, c: Cell) -> bool {
        c.col >= 0
            && c.row >= 0
            && c.col < self.input.cols
            && c.row < self.input.rows
            && !self.used.contains(&c)
            && !self
                .input
                .blocked
                .get(c.row as usize)
                .and_then(|r| r.get(c.col as usize))
                .copied()
                .unwrap_or(false)
    }
}

/// Cells in fill order for a group, starting at its corner (with margin).
/// A group stays in its own half of the screen and never jumps over a blocked zone:
/// a row/column ends at the first blocked cell.
fn order(group: Group, corner: Corner, cfg: &LayoutConfig, input: &Input) -> Vec<Cell> {
    let (cols, rows) = (input.cols, input.rows);
    let m = cfg.margin_cells as i32;
    let left = matches!(corner, Corner::TopLeft | Corner::BottomLeft);
    let top = matches!(corner, Corner::TopLeft | Corner::TopRight);
    // Map "distance from corner" (dx, dy) to grid coordinates.
    let at = |dx: i32, dy: i32| Cell {
        col: if left { m + dx } else { cols - 1 - m - dx },
        row: if top { m + dy } else { rows - 1 - m - dy },
    };
    let blocked = |c: Cell| {
        input
            .blocked
            .get(c.row as usize)
            .and_then(|r| r.get(c.col as usize))
            .copied()
            .unwrap_or(false)
    };
    let span_x = ((cols + 1) / 2 - m).max(0);
    let span_y = (rows - 2 * m).max(0);
    let mut out = Vec::new();
    match group {
        Group::Apps => {
            let band = (cfg.apps_columns as i32).max(1);
            let mut b0 = 0;
            while b0 < span_x && !blocked(at(b0, 0)) {
                for dy in 0..span_y {
                    for dx in b0..(b0 + band).min(span_x) {
                        if blocked(at(dx, dy)) {
                            break;
                        }
                        out.push(at(dx, dy));
                    }
                }
                b0 += band;
            }
        }
        Group::Projects => {
            for dx in 0..span_x {
                if blocked(at(dx, 0)) {
                    break;
                }
                for dy in 0..span_y {
                    if blocked(at(dx, dy)) {
                        break;
                    }
                    out.push(at(dx, dy));
                }
            }
        }
        Group::Inbox | Group::Trash => {
            for dy in 0..span_y {
                for dx in 0..span_x {
                    if blocked(at(dx, dy)) {
                        break;
                    }
                    out.push(at(dx, dy));
                }
            }
        }
    }
    out
}

pub fn layout(input: &Input, cfg: &LayoutConfig) -> Output {
    let mut out = Output::default();
    let mut taken = Taken {
        input,
        used: HashSet::new(),
    };

    // 1. Pins.
    for (g, entries) in &input.groups {
        for e in entries {
            out.group_of.insert(e.id.clone(), *g);
            if let Some(p) = e.pin {
                if taken.free(p) {
                    taken.used.insert(p);
                    out.cells.insert(e.id.clone(), p);
                } else {
                    out.warnings.push(format!(
                        "pin {},{} for {} is blocked or taken; placing normally",
                        p.col, p.row, e.id
                    ));
                }
            }
        }
    }
    // 2. Groups in a fixed order.
    for g in [Group::Trash, Group::Inbox, Group::Projects, Group::Apps] {
        let Some(entries) = input.groups.get(&g) else {
            continue;
        };
        let mut cells = order(g, g.corner(cfg), cfg, input).into_iter();
        for e in entries {
            if out.cells.contains_key(&e.id) {
                continue;
            }
            match cells.by_ref().find(|c| taken.free(*c)) {
                Some(c) => {
                    taken.used.insert(c);
                    out.cells.insert(e.id.clone(), c);
                }
                None => out.overflow.push(e.id.clone()),
            }
        }
    }
    out
}

/// ASCII picture: `#` blocked, `.` free, group letters for placed items.
pub fn ascii(input: &Input, out: &Output) -> String {
    let mut g = vec![vec!['.'; input.cols.max(0) as usize]; input.rows.max(0) as usize];
    for (r, row) in g.iter_mut().enumerate() {
        for (c, ch) in row.iter_mut().enumerate() {
            if input
                .blocked
                .get(r)
                .and_then(|b| b.get(c))
                .copied()
                .unwrap_or(false)
            {
                *ch = '#';
            }
        }
    }
    for (id, cell) in &out.cells {
        let letter = out.group_of.get(id).map_or('?', |g| g.letter());
        if let Some(ch) = g
            .get_mut(cell.row as usize)
            .and_then(|r| r.get_mut(cell.col as usize))
        {
            *ch = letter;
        }
    }
    g.into_iter()
        .map(|r| r.into_iter().collect::<String>() + "\n")
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Corners, Zone};
    use crate::layout::free_zones::blocked_cells;

    fn entries(prefix: &str, n: usize) -> Vec<Entry> {
        (0..n)
            .map(|i| Entry {
                id: format!("{prefix}{i:02}"),
                pin: None,
            })
            .collect()
    }

    fn input(cols: i32, rows: i32, zones: &[Zone], g: &[(Group, usize)]) -> Input {
        Input {
            cols,
            rows,
            blocked: blocked_cells(cols as usize, rows as usize, zones),
            groups: g
                .iter()
                .map(|(grp, n)| (*grp, entries(&format!("{grp:?}"), *n)))
                .collect(),
        }
    }

    fn cfg() -> LayoutConfig {
        LayoutConfig {
            keep_free: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn golden_default_corners() {
        let zones = [Zone {
            x: 0.25,
            y: 0.0,
            w: 0.5,
            h: 1.0,
        }];
        let inp = input(
            16,
            8,
            &zones,
            &[
                (Group::Apps, 7),
                (Group::Projects, 5),
                (Group::Inbox, 5),
                (Group::Trash, 1),
            ],
        );
        let out = layout(&inp, &cfg());
        assert!(out.overflow.is_empty());
        assert_eq!(
            ascii(&inp, &out),
            "\
....########....
.AA.########.PP.
.AA.########..P.
.AA.########..P.
.A..########..P.
....########.II.
.T..########III.
....########....
"
        );
    }

    #[test]
    fn golden_overflow_wraps_and_skips_zones() {
        // Groups never cross the center zone; what doesn't fit goes to `overflow`.
        let zones = [Zone {
            x: 0.25,
            y: 0.0,
            w: 0.5,
            h: 1.0,
        }];
        let inp = input(
            12,
            5,
            &zones,
            &[(Group::Apps, 9), (Group::Inbox, 9), (Group::Trash, 1)],
        );
        let out = layout(&inp, &cfg());
        assert_eq!(
            ascii(&inp, &out),
            "\
...######...
.AA######II.
.AA######II.
.TA######II.
...######...
"
        );
        // apps: 5 cells left of the zone (trash took one); inbox: 6 cells on the right.
        assert_eq!(
            out.overflow.len(),
            (9 - 5) + (9 - 6),
            "overflow: {:?}",
            out.overflow
        );
    }

    #[test]
    fn corners_are_configurable() {
        let mut c = cfg();
        c.corners = Corners {
            apps: Corner::TopRight,
            projects: Corner::TopLeft,
            inbox: Corner::BottomLeft,
            trash: Corner::BottomRight,
        };
        let inp = input(
            8,
            5,
            &[],
            &[
                (Group::Apps, 3),
                (Group::Projects, 2),
                (Group::Inbox, 2),
                (Group::Trash, 1),
            ],
        );
        let out = layout(&inp, &c);
        // Mirrored: from the top-right corner the edge column fills first.
        assert_eq!(
            ascii(&inp, &out),
            "........\n.P...AA.\n.P....A.\n.II...T.\n........\n"
        );
    }

    #[test]
    fn pins_win_and_bad_pins_fall_back() {
        let mut inp = input(8, 5, &[], &[(Group::Apps, 2)]);
        inp.groups.get_mut(&Group::Apps).unwrap()[1].pin = Some(Cell { col: 6, row: 3 });
        inp.groups.insert(
            Group::Trash,
            vec![Entry {
                id: "bin".into(),
                pin: Some(Cell { col: 99, row: 0 }),
            }],
        );
        let out = layout(&inp, &cfg());
        assert_eq!(out.cells["Apps01"], Cell { col: 6, row: 3 });
        assert_eq!(out.cells["Apps00"], Cell { col: 1, row: 1 });
        assert_eq!(out.cells["bin"], Cell { col: 1, row: 3 });
        assert_eq!(out.warnings.len(), 1);
    }

    #[test]
    fn deterministic() {
        let inp = input(
            34,
            16,
            &[Zone {
                x: 0.25,
                y: 0.0,
                w: 0.5,
                h: 1.0,
            }],
            &[(Group::Apps, 20), (Group::Inbox, 12)],
        );
        assert_eq!(layout(&inp, &cfg()).cells, layout(&inp, &cfg()).cells);
    }
}
