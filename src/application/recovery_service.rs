//! "Recover all windows": an emergency operation that brings every
//! registered window back onto the main screen, ignoring workset boundaries
//! (PLAN.md §10.2).

use uuid::Uuid;

use crate::application::monitor_resolution::find_live_monitor_by_stable_id;
use crate::application::window_ops::WindowOps;
use crate::application::workset_service;
use crate::domain::placement::PixelRect;
use crate::domain::workset::Workset;
use crate::windowing::enumerate::TopLevelWindow;
use crate::windowing::matcher::MatchDecision;
use crate::windowing::monitor::MonitorInfo;

#[derive(Debug, Clone)]
pub struct RecoveredWindow {
    pub managed_window_id: Uuid,
    pub hwnd: isize,
    pub placed_rect: Option<PixelRect>,
    pub skip_reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RecoveryReport {
    pub recovered: Vec<RecoveredWindow>,
    pub skipped: Vec<RecoveredWindow>,
}

/// PLAN.md §10.2 step 4's tiling, made concrete: 1xN up to 4 windows,
/// otherwise a 2-row grid (the spec's own wording, "対象数に応じ", isn't
/// numerically specific). Every returned rect is fully inside `work_area` by
/// construction, structurally satisfying "never off-screen" (PLAN.md §2.7).
pub fn plan_recovery_tiles(count: usize, work_area: PixelRect) -> Vec<PixelRect> {
    if count == 0 {
        return Vec::new();
    }
    let (cols, rows): (usize, i32) = if count <= 4 {
        (count, 1)
    } else {
        (count.div_ceil(2), 2)
    };
    let cell_w = work_area.width / cols as i32;
    let cell_h = work_area.height / rows;

    (0..count)
        .map(|i| {
            let col = (i % cols) as i32;
            let row = (i / cols) as i32;
            PixelRect::new(
                work_area.x + col * cell_w,
                work_area.y + row * cell_h,
                cell_w,
                cell_h,
            )
        })
        .collect()
}

/// PLAN.md §10.2 steps 1-5, 7. Step 6 (`current_workset_id = None`) is the
/// caller's responsibility — `SwitchCoordinator::recover_all_windows` owns
/// `runtime.json` persistence, matching how it owns it for `switch_to`.
pub fn recover_all_windows<W: WindowOps>(
    window_ops: &W,
    worksets: &[Workset],
    live_windows: &[TopLevelWindow],
    live_monitors: &[MonitorInfo],
    main_monitor_ids: &[String],
) -> RecoveryReport {
    let decisions = workset_service::resolve_all_matches(worksets, live_windows); // step 1

    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    for workset in worksets {
        for managed in &workset.windows {
            match decisions.get(&managed.id) {
                Some(MatchDecision::AutoRebind { hwnd }) => {
                    candidates.push((workset.sort_order, managed, *hwnd));
                }
                _ => skipped.push(RecoveredWindow {
                    managed_window_id: managed.id,
                    hwnd: 0,
                    placed_rect: None,
                    skip_reason: Some("window could not be re-resolved".to_string()),
                }),
            }
        }
    }
    // Deterministic tiling order: workset registration order, then z_order, then hwnd.
    candidates.sort_by_key(|(sort_order, managed, hwnd)| (*sort_order, managed.z_order, *hwnd));

    let Some(work_area) = main_monitor_ids
        .iter()
        .find_map(|id| find_live_monitor_by_stable_id(live_monitors, id))
        .map(|m| m.work_area_px)
    else {
        skipped.extend(
            candidates
                .into_iter()
                .map(|(_, managed, hwnd)| RecoveredWindow {
                    managed_window_id: managed.id,
                    hwnd,
                    placed_rect: None,
                    skip_reason: Some("no live main monitor".to_string()),
                }),
        );
        return RecoveryReport {
            recovered: Vec::new(),
            skipped,
        };
    };

    for (_, _, hwnd) in &candidates {
        window_ops.restore(*hwnd); // step 3
    }
    let tiles = plan_recovery_tiles(candidates.len(), work_area); // step 4
    let moves: Vec<(isize, PixelRect)> = candidates
        .iter()
        .zip(&tiles)
        .map(|((_, _, hwnd), rect)| (*hwnd, *rect))
        .collect();

    let mut recovered = Vec::new();
    match window_ops.batch_move(&moves) {
        Ok(()) => {
            for ((_, managed, hwnd), rect) in candidates.iter().zip(&tiles) {
                recovered.push(RecoveredWindow {
                    managed_window_id: managed.id,
                    hwnd: *hwnd,
                    placed_rect: Some(*rect),
                    skip_reason: None,
                });
            }
        }
        Err(_) => {
            for (_, managed, hwnd) in candidates {
                skipped.push(RecoveredWindow {
                    managed_window_id: managed.id,
                    hwnd,
                    placed_rect: None,
                    skip_reason: Some("batch move failed".to_string()),
                });
            }
        }
    }

    RecoveryReport { recovered, skipped }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_up_to_four_windows_in_a_single_row() {
        let work_area = PixelRect::new(0, 0, 1920, 1080);
        let tiles = plan_recovery_tiles(3, work_area);
        assert_eq!(tiles.len(), 3);
        for tile in &tiles {
            assert!(tile.right() <= work_area.right());
            assert!(tile.bottom() <= work_area.bottom());
        }
        assert_eq!(tiles[0].y, tiles[1].y, "single row");
    }

    #[test]
    fn tiles_more_than_four_windows_into_two_rows() {
        let work_area = PixelRect::new(0, 0, 1920, 1080);
        let tiles = plan_recovery_tiles(6, work_area);
        assert_eq!(tiles.len(), 6);
        let distinct_rows: std::collections::HashSet<i32> = tiles.iter().map(|t| t.y).collect();
        assert_eq!(distinct_rows.len(), 2);
        for tile in &tiles {
            assert!(tile.right() <= work_area.right());
            assert!(tile.bottom() <= work_area.bottom());
        }
    }

    #[test]
    fn zero_windows_yields_no_tiles() {
        assert!(plan_recovery_tiles(0, PixelRect::new(0, 0, 1920, 1080)).is_empty());
    }
}
