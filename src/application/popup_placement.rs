//! Where the Quick Switcher popup should appear (PLAN.md §3.3 "表示位置").

use crate::application::monitor_resolution::find_live_monitor_by_stable_id;
use crate::domain::config::PopupLocation;
use crate::windowing::monitor::MonitorInfo;

/// Resolves the top-left position for a `popup_size`-sized window so it's
/// centered on the monitor implied by `location`. Falls back to the primary
/// monitor, then the first available monitor, if the target monitor is gone
/// (PLAN.md §3.3: "対象モニターが消失した場合は、プライマリモニター中央へ
/// フォールバックする"). Returns `(0, 0)` only if `live_monitors` is empty.
pub fn resolve_popup_position(
    location: PopupLocation,
    cursor_pos: (i32, i32),
    live_monitors: &[MonitorInfo],
    main_monitor_ids: &[String],
    popup_size: (i32, i32),
) -> (i32, i32) {
    let target = match location {
        PopupLocation::CursorMonitorCenter => live_monitors
            .iter()
            .find(|m| m.bounds_px.contains_point(cursor_pos.0, cursor_pos.1)),
        PopupLocation::MainMonitorCenter => main_monitor_ids
            .first()
            .and_then(|id| find_live_monitor_by_stable_id(live_monitors, id)),
    }
    .or_else(|| live_monitors.iter().find(|m| m.is_primary))
    .or_else(|| live_monitors.first());

    let Some(monitor) = target else {
        return (0, 0);
    };

    let (center_x, center_y) = monitor.bounds_px.center();
    (center_x - popup_size.0 / 2, center_y - popup_size.1 / 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::placement::PixelRect;

    fn monitor(device_name: &str, x: i32, is_primary: bool) -> MonitorInfo {
        MonitorInfo {
            handle: 0,
            device_name: device_name.to_string(),
            bounds_px: PixelRect::new(x, 0, 1920, 1080),
            work_area_px: PixelRect::new(x, 0, 1920, 1032),
            dpi_x: 96,
            dpi_y: 96,
            is_primary,
        }
    }

    #[test]
    fn centers_on_the_monitor_containing_the_cursor() {
        let monitors = vec![monitor("MAIN", 0, true), monitor("SIDE", 1920, false)];

        let (x, y) = resolve_popup_position(
            PopupLocation::CursorMonitorCenter,
            (2500, 100), // inside SIDE
            &monitors,
            &["MAIN".to_string()],
            (480, 560),
        );

        // SIDE's center is (1920 + 960, 540) = (2880, 540).
        assert_eq!((x, y), (2880 - 240, 540 - 280));
    }

    #[test]
    fn falls_back_to_primary_when_cursor_monitor_is_gone() {
        let monitors = vec![monitor("MAIN", 0, true)];

        let (x, y) = resolve_popup_position(
            PopupLocation::CursorMonitorCenter,
            (5000, 100), // not on any monitor
            &monitors,
            &["MAIN".to_string()],
            (480, 560),
        );

        assert_eq!((x, y), (960 - 240, 540 - 280));
    }

    #[test]
    fn main_monitor_center_falls_back_to_primary_when_main_is_gone() {
        let monitors = vec![monitor("OTHER", 0, true)];

        let (x, y) = resolve_popup_position(
            PopupLocation::MainMonitorCenter,
            (0, 0),
            &monitors,
            &["MAIN-GONE".to_string()],
            (480, 560),
        );

        assert_eq!((x, y), (960 - 240, 540 - 280));
    }

    #[test]
    fn empty_monitor_list_does_not_panic() {
        assert_eq!(
            resolve_popup_position(
                PopupLocation::CursorMonitorCenter,
                (0, 0),
                &[],
                &[],
                (480, 560)
            ),
            (0, 0)
        );
    }
}
