//! Persisted monitor/parking-grid domain types (PLAN.md §7.2).

use serde::{Deserialize, Serialize};

use crate::domain::placement::PixelRect;

/// How many parking cells a non-main monitor is split into (PLAN.md §2.5, §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoSplit {
    One,
    TwoColumns,
    FourGrid,
}

impl AutoSplit {
    /// Number of grid cells this split produces.
    pub fn cell_count(self) -> usize {
        match self {
            AutoSplit::One => 1,
            AutoSplit::TwoColumns => 2,
            AutoSplit::FourGrid => 4,
        }
    }
}

/// A monitor as last observed, persisted so it can be re-matched by `stable_id`
/// after a restart or a monitor configuration change (PLAN.md §4.6, §7.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedMonitor {
    pub stable_id: String,
    pub device_name: String,
    pub device_path: Option<String>,
    pub friendly_name: Option<String>,
    pub bounds_px: PixelRect,
    pub work_area_px: PixelRect,
    pub dpi_x: u32,
    pub dpi_y: u32,
    pub auto_split: AutoSplit,
}
