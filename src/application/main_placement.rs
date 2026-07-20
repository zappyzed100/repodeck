//! Restoring a window to its main-screen placement (PLAN.md §4.2).

use crate::application::monitor_resolution::find_live_monitor_by_stable_id;
use crate::domain::placement::{PixelRect, SavedPlacement, SavedShowState, denormalize};
use crate::windowing::monitor::MonitorInfo;

/// Minimum size a restored main-screen window is allowed to be (PLAN.md §4.2 step 4).
pub const MIN_WINDOW_WIDTH: i32 = 160;
pub const MIN_WINDOW_HEIGHT: i32 = 90;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RestoreOutcome {
    pub rect: PixelRect,
    /// Only ever `Normal` or `Maximized` (PLAN.md §4.2 step 7: a window is
    /// never left minimized when restored to the main screen).
    pub show_state: SavedShowState,
}

/// Computes where `saved` should land on the current screen configuration
/// (PLAN.md §4.2). Returns `None` only when not a single main monitor id
/// resolves to a live monitor (total main-screen loss); an out-of-range
/// `main_monitor_index` on its own still falls back correctly to the first
/// live main monitor.
pub fn resolve_main_restore(
    saved: &SavedPlacement,
    live_monitors: &[MonitorInfo],
    main_monitor_ids: &[String],
) -> Option<RestoreOutcome> {
    let work_area = main_monitor_ids
        .get(saved.main_monitor_index)
        .and_then(|id| find_live_monitor_by_stable_id(live_monitors, id))
        // Step 2 fallback: the first configured main monitor that's currently live.
        .or_else(|| {
            main_monitor_ids
                .iter()
                .find_map(|id| find_live_monitor_by_stable_id(live_monitors, id))
        })
        .map(|m| m.work_area_px)?;

    let mut rect = denormalize(saved.normalized_rect, work_area); // step 3
    rect.width = rect.width.max(MIN_WINDOW_WIDTH); // step 4
    rect.height = rect.height.max(MIN_WINDOW_HEIGHT);
    rect = clamp_not_fully_outside(rect, work_area); // step 5

    let show_state = match saved.show_state {
        SavedShowState::Maximized => SavedShowState::Maximized, // step 6
        SavedShowState::Minimized | SavedShowState::Normal => SavedShowState::Normal, // step 7
    };

    Some(RestoreOutcome { rect, show_state })
}

/// Nudges `rect` back on-screen if it falls entirely outside `work_area`
/// along an axis, without otherwise resizing or repositioning it.
fn clamp_not_fully_outside(mut rect: PixelRect, work_area: PixelRect) -> PixelRect {
    if rect.right() <= work_area.x {
        rect.x = work_area.x;
    }
    if rect.x >= work_area.right() {
        rect.x = work_area.right() - rect.width;
    }
    if rect.bottom() <= work_area.y {
        rect.y = work_area.y;
    }
    if rect.y >= work_area.bottom() {
        rect.y = work_area.bottom() - rect.height;
    }
    rect
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::placement::NormalizedRect;

    fn monitor(device_name: &str, x: i32) -> MonitorInfo {
        MonitorInfo {
            handle: 0,
            device_name: device_name.to_string(),
            bounds_px: PixelRect::new(x, 0, 1920, 1080),
            work_area_px: PixelRect::new(x, 0, 1920, 1032),
            dpi_x: 96,
            dpi_y: 96,
            is_primary: x == 0,
        }
    }

    fn saved(
        main_monitor_index: usize,
        rect: NormalizedRect,
        show_state: SavedShowState,
    ) -> SavedPlacement {
        SavedPlacement {
            monitor_id: "MAIN1".to_string(),
            main_monitor_index,
            normalized_rect: rect,
            physical_rect_at_capture: PixelRect::new(0, 0, 800, 600),
            show_state,
        }
    }

    #[test]
    fn out_of_range_index_falls_back_to_first_live_main_monitor() {
        let live = vec![monitor("MAIN1", 0)];
        let main_ids = vec!["MAIN1".to_string()];
        let placement = saved(
            5, // out of range
            NormalizedRect {
                x: 0.1,
                y: 0.1,
                width: 0.5,
                height: 0.5,
            },
            SavedShowState::Normal,
        );

        let outcome = resolve_main_restore(&placement, &live, &main_ids).unwrap();
        assert_eq!(outcome.rect.x, 192);
    }

    #[test]
    fn below_minimum_size_is_bumped_up() {
        let live = vec![monitor("MAIN1", 0)];
        let main_ids = vec!["MAIN1".to_string()];
        let placement = saved(
            0,
            NormalizedRect {
                x: 0.0,
                y: 0.0,
                width: 0.01,
                height: 0.01,
            },
            SavedShowState::Normal,
        );

        let outcome = resolve_main_restore(&placement, &live, &main_ids).unwrap();
        assert_eq!(outcome.rect.width, MIN_WINDOW_WIDTH);
        assert_eq!(outcome.rect.height, MIN_WINDOW_HEIGHT);
    }

    #[test]
    fn fully_offscreen_saved_rect_is_clamped_back_on_screen() {
        let live = vec![monitor("MAIN1", 0)];
        let main_ids = vec!["MAIN1".to_string()];
        // Normalized x=1.5 means the saved rect starts entirely past the
        // current work area's right edge.
        let placement = saved(
            0,
            NormalizedRect {
                x: 1.5,
                y: 0.0,
                width: 0.3,
                height: 0.3,
            },
            SavedShowState::Normal,
        );

        let outcome = resolve_main_restore(&placement, &live, &main_ids).unwrap();
        assert!(outcome.rect.x < live[0].work_area_px.right());
    }

    #[test]
    fn minimized_show_state_restores_as_normal() {
        let live = vec![monitor("MAIN1", 0)];
        let main_ids = vec!["MAIN1".to_string()];
        let placement = saved(
            0,
            NormalizedRect {
                x: 0.1,
                y: 0.1,
                width: 0.3,
                height: 0.3,
            },
            SavedShowState::Minimized,
        );

        let outcome = resolve_main_restore(&placement, &live, &main_ids).unwrap();
        assert_eq!(outcome.show_state, SavedShowState::Normal);
    }

    #[test]
    fn maximized_show_state_stays_maximized() {
        let live = vec![monitor("MAIN1", 0)];
        let main_ids = vec!["MAIN1".to_string()];
        let placement = saved(
            0,
            NormalizedRect {
                x: 0.1,
                y: 0.1,
                width: 0.3,
                height: 0.3,
            },
            SavedShowState::Maximized,
        );

        let outcome = resolve_main_restore(&placement, &live, &main_ids).unwrap();
        assert_eq!(outcome.show_state, SavedShowState::Maximized);
    }

    #[test]
    fn total_main_monitor_loss_returns_none() {
        let live = vec![monitor("OTHER", 0)];
        let main_ids = vec!["MAIN1".to_string()];
        let placement = saved(
            0,
            NormalizedRect {
                x: 0.1,
                y: 0.1,
                width: 0.3,
                height: 0.3,
            },
            SavedShowState::Normal,
        );

        assert!(resolve_main_restore(&placement, &live, &main_ids).is_none());
    }
}
