//! Shared monitor-matching helpers used by the parking allocator and main-
//! placement restore (PLAN.md §4.2, §4.4, §4.6).

use crate::domain::placement::PixelRect;
use crate::windowing::monitor::MonitorInfo;

/// Matches a persisted monitor identity against the live monitor list.
///
/// `stable_id` is currently just a monitor's `device_name` (see the save path
/// in `src/app.rs`), so this is plain equality against `MonitorInfo::device_name`
/// today; centralizing it here means callers don't repeat that assumption.
pub fn find_live_monitor_by_stable_id<'a>(
    live: &'a [MonitorInfo],
    stable_id: &str,
) -> Option<&'a MonitorInfo> {
    live.iter().find(|m| m.device_name == stable_id)
}

/// Sort key for top-left-to-bottom-right reading order (PLAN.md §4.4 step 1).
fn reading_order_key(bounds: PixelRect) -> (i32, i32) {
    (bounds.y, bounds.x)
}

/// Sorts `monitors` in place into reading order. `enumerate_monitors`'s own
/// doc comment says Windows' enumeration order does not already guarantee
/// this, so callers that need spatial ordering (PLAN.md §4.4) must sort explicitly.
pub fn sort_monitors_reading_order(monitors: &mut [MonitorInfo]) {
    monitors.sort_by_key(|m| reading_order_key(m.bounds_px));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(device_name: &str, x: i32, y: i32) -> MonitorInfo {
        MonitorInfo {
            handle: 0,
            device_name: device_name.to_string(),
            bounds_px: PixelRect::new(x, y, 1920, 1080),
            work_area_px: PixelRect::new(x, y, 1920, 1080),
            dpi_x: 96,
            dpi_y: 96,
            is_primary: false,
        }
    }

    #[test]
    fn find_by_stable_id_matches_device_name() {
        let live = vec![monitor("A", 0, 0), monitor("B", 1920, 0)];
        assert_eq!(
            find_live_monitor_by_stable_id(&live, "B").map(|m| &m.device_name),
            Some(&"B".to_string())
        );
        assert!(find_live_monitor_by_stable_id(&live, "C").is_none());
    }

    #[test]
    fn sorts_scrambled_monitors_into_reading_order() {
        let mut monitors = vec![
            monitor("bottom-right", 1920, 1080),
            monitor("top-left", 0, 0),
            monitor("top-right", 1920, 0),
            monitor("bottom-left", 0, 1080),
        ];

        sort_monitors_reading_order(&mut monitors);

        let order: Vec<&str> = monitors.iter().map(|m| m.device_name.as_str()).collect();
        assert_eq!(
            order,
            vec!["top-left", "top-right", "bottom-left", "bottom-right"]
        );
    }

    #[test]
    fn sorts_negative_virtual_screen_coordinates() {
        // Mirrors a monitor above-left of the primary at negative coordinates
        // (same real-machine layout used in layout_service's own tests).
        let mut monitors = vec![monitor("primary", 0, 0), monitor("secondary", -1920, -1080)];

        sort_monitors_reading_order(&mut monitors);

        let order: Vec<&str> = monitors.iter().map(|m| m.device_name.as_str()).collect();
        assert_eq!(order, vec!["secondary", "primary"]);
    }
}
