//! Cells the layout never fills: `layout.keep_free` zones (fractions of the work area).
//! Pure: no Win32, no wallpaper.

use crate::config::Zone;

/// `blocked[row][col]` — true if the cell's center lies inside any zone.
pub fn blocked_cells(cols: usize, rows: usize, zones: &[Zone]) -> Vec<Vec<bool>> {
    let mut out = vec![vec![false; cols]; rows];
    for (r, row) in out.iter_mut().enumerate() {
        for (c, cell) in row.iter_mut().enumerate() {
            let cx = (c as f32 + 0.5) / cols as f32;
            let cy = (r as f32 + 0.5) / rows as f32;
            *cell = zones
                .iter()
                .any(|z| cx >= z.x && cx < z.x + z.w && cy >= z.y && cy < z.y + z.h);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ascii(b: &[Vec<bool>]) -> String {
        b.iter()
            .map(|r| {
                r.iter()
                    .map(|&x| if x { '#' } else { '.' })
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    #[test]
    fn default_center_column() {
        let z = [Zone {
            x: 0.25,
            y: 0.0,
            w: 0.5,
            h: 1.0,
        }];
        assert_eq!(ascii(&blocked_cells(8, 2, &z)), "..####..\n..####..\n");
    }

    #[test]
    fn several_zones_and_empty() {
        let z = [
            Zone {
                x: 0.0,
                y: 0.5,
                w: 0.5,
                h: 0.5,
            },
            Zone {
                x: 0.75,
                y: 0.0,
                w: 0.25,
                h: 0.25,
            },
        ];
        assert_eq!(ascii(&blocked_cells(4, 4, &z)), "...#\n....\n##..\n##..\n");
        assert_eq!(ascii(&blocked_cells(3, 1, &[])), "...\n");
    }
}
