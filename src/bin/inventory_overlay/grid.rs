//! Inventory grid geometry: turns the tunable grid parameters into the 28
//! concrete slot rectangles, and persists the resolved coordinates.
//!
//! The default parameters are seeded from the known-good RuneLite measurements.
//! Calibrate them to the Elorin client live, then press S to write the resolved
//! boxes to `inventory_slots.json`.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::state;

/// Tunable inventory-grid parameters, in client-relative pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GridParams {
    pub base_x: i32,
    pub base_y: i32,
    pub cols: i32,
    pub rows: i32,
    pub slot_w: i32,
    pub slot_h: i32,
    pub step_x: i32,
    pub step_y: i32,
}

impl GridParams {
    /// RuneLite reference measurements (exact, verified): top-left slot
    /// (567,241,602,272), bottom-right (693,457,728,488).
    pub const DEFAULT: GridParams = GridParams {
        base_x: 567,
        base_y: 241,
        cols: 4,
        rows: 7,
        slot_w: 35,
        slot_h: 31,
        step_x: 42,
        step_y: 36,
    };
}

/// One inventory slot bounding box + centre, in client coordinates.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Slot {
    pub index: i32,
    pub row: i32,
    pub col: i32,
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    pub cx: i32,
    pub cy: i32,
}

/// Build all slots in row-major order, `(0,0)` .. `(cols-1, rows-1)`.
pub fn slots(p: &GridParams) -> Vec<Slot> {
    let cols = p.cols.max(0);
    let rows = p.rows.max(0);
    let mut out = Vec::with_capacity((cols * rows) as usize);
    for row in 0..rows {
        for col in 0..cols {
            let x1 = p.base_x + col * p.step_x;
            let y1 = p.base_y + row * p.step_y;
            let x2 = x1 + p.slot_w;
            let y2 = y1 + p.slot_h;
            out.push(Slot {
                index: row * cols + col,
                row,
                col,
                x1,
                y1,
                x2,
                y2,
                cx: x1 + p.slot_w / 2,
                cy: y1 + p.slot_h / 2,
            });
        }
    }
    out
}

#[derive(Serialize)]
struct Snapshot<'a> {
    window_title: &'a str,
    grid: GridParams,
    slots: Vec<Slot>,
}

/// Write `inventory_slots.json` (the canonical output for future projects) and
/// rewrite `overlay.toml` with the current parameters. Returns the paths written.
pub fn save_snapshot(params: &GridParams, title: &str, hud_on: bool) -> Result<Vec<PathBuf>> {
    let dir = state::OUT_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."));

    let doc = Snapshot {
        window_title: title,
        grid: *params,
        slots: slots(params),
    };
    let json = serde_json::to_string_pretty(&doc)?;
    let json_path = dir.join("inventory_slots.json");
    std::fs::write(&json_path, json)?;

    // Rewrite the config too, so the tuned values survive a restart. Re-read the
    // file first so any hand-edited fields are preserved.
    if let Some(path) = state::CONFIG_PATH.get().cloned() {
        let mut cfg = crate::config::OverlayConfig::load_from(&path)
            .unwrap_or_else(|_| crate::config::OverlayConfig::default_config());
        cfg.grid = *params;
        cfg.overlay.show_hud = hud_on;
        cfg.window.title_contains = title.to_string();
        cfg.save_to(&path)?;
        Ok(vec![json_path, path])
    } else {
        Ok(vec![json_path])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_grid_matches_reference_measurements() {
        let s = slots(&GridParams::DEFAULT);
        assert_eq!(s.len(), 28);
        assert_eq!((s[0].x1, s[0].y1, s[0].x2, s[0].y2), (567, 241, 602, 272));
        assert_eq!((s[1].x1, s[1].y1, s[1].x2, s[1].y2), (609, 241, 644, 272));
        assert_eq!((s[4].x1, s[4].y1, s[4].x2, s[4].y2), (567, 277, 602, 308));
        assert_eq!((s[27].x1, s[27].y1, s[27].x2, s[27].y2), (693, 457, 728, 488));
    }

    #[test]
    fn indices_are_row_major() {
        let s = slots(&GridParams::DEFAULT);
        for (i, slot) in s.iter().enumerate() {
            assert_eq!(slot.index as usize, i);
            assert_eq!(slot.row, (i / 4) as i32);
            assert_eq!(slot.col, (i % 4) as i32);
        }
    }
}
