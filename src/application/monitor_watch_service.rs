//! Detects a runtime monitor-configuration change (Phase 9) and finds which
//! currently-managed windows it left fully off every live monitor. Deliberately
//! *not* a blanket "recover everything to the main screen" — only windows that
//! actually ended up unreachable get touched, consistent with Phase 6's own
//! "minimize when no good placement exists" convention (`parking_allocator`'s
//! `Minimized` fallback).

use crate::application::workset_service;
use crate::domain::workset::Workset;
use crate::windowing::enumerate::TopLevelWindow;
use crate::windowing::matcher::MatchDecision;
use crate::windowing::monitor::MonitorInfo;

/// A stable, order-independent snapshot of the live monitor topology, cheap to
/// compare against `RuntimeState.last_seen_monitor_fingerprint` to decide
/// whether anything actually changed (a `WM_DISPLAYCHANGE` message doesn't by
/// itself mean the set of monitors changed — e.g. a DPI-only change fires it
/// too).
pub fn compute_fingerprint(monitors: &[MonitorInfo]) -> String {
    let mut parts: Vec<String> = monitors
        .iter()
        .map(|m| {
            format!(
                "{}:{},{},{},{}",
                m.device_name, m.bounds_px.x, m.bounds_px.y, m.bounds_px.width, m.bounds_px.height
            )
        })
        .collect();
    parts.sort();
    parts.join("|")
}

/// Resolves every registered `ManagedWindow` to its live hwnd, then returns the
/// hwnds whose *current* rect (from `live_windows`, not the stored placement)
/// overlaps no live monitor at all. A window merely shrunk or partially
/// clipped by a lost monitor, but still reachable elsewhere, is left alone.
pub fn find_now_offscreen_windows(
    worksets: &[Workset],
    live_windows: &[TopLevelWindow],
    live_monitors: &[MonitorInfo],
) -> Vec<isize> {
    let decisions = workset_service::resolve_all_matches(worksets, live_windows);

    worksets
        .iter()
        .flat_map(|workset| &workset.windows)
        .filter_map(|managed| match decisions.get(&managed.id) {
            Some(MatchDecision::AutoRebind { hwnd }) => {
                live_windows.iter().find(|w| w.hwnd == *hwnd)
            }
            _ => None,
        })
        .filter(|window| {
            !live_monitors
                .iter()
                .any(|monitor| window.rect_px.overlaps(&monitor.bounds_px))
        })
        .map(|window| window.hwnd)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use uuid::Uuid;

    use super::*;
    use crate::domain::placement::{NormalizedRect, PixelRect, SavedPlacement, SavedShowState};
    use crate::domain::workset::{ManagedWindow, ParkingPolicy, RepositoryKind, WindowMatcher};

    fn monitor(device_name: &str, bounds: PixelRect) -> MonitorInfo {
        MonitorInfo {
            handle: 0,
            device_name: device_name.to_string(),
            bounds_px: bounds,
            work_area_px: bounds,
            dpi_x: 96,
            dpi_y: 96,
            is_primary: false,
        }
    }

    #[test]
    fn fingerprint_is_stable_regardless_of_input_order() {
        let a = monitor("A", PixelRect::new(0, 0, 1920, 1080));
        let b = monitor("B", PixelRect::new(1920, 0, 1920, 1080));

        assert_eq!(
            compute_fingerprint(&[a.clone(), b.clone()]),
            compute_fingerprint(&[b, a])
        );
    }

    #[test]
    fn fingerprint_changes_when_bounds_change() {
        let a = monitor("A", PixelRect::new(0, 0, 1920, 1080));
        let a_moved = monitor("A", PixelRect::new(0, 0, 2560, 1440));

        assert_ne!(compute_fingerprint(&[a]), compute_fingerprint(&[a_moved]));
    }

    fn window_with(
        hwnd: isize,
        exe: &str,
        class: &str,
        title: &str,
        rect: PixelRect,
    ) -> TopLevelWindow {
        TopLevelWindow {
            hwnd,
            process_id: 1,
            executable_path: Some(PathBuf::from(exe)),
            window_class: class.to_string(),
            title: title.to_string(),
            rect_px: rect,
        }
    }

    fn workset_with_window(rect: PixelRect) -> Workset {
        let matcher = WindowMatcher {
            executable_path: PathBuf::from(r"C:\code.exe"),
            process_name: "code.exe".to_string(),
            window_class: "Chrome_WidgetWin_1".to_string(),
            registered_title: "repo - Visual Studio Code".to_string(),
            title_contains: None,
            title_regex: None,
        };
        let managed = ManagedWindow {
            id: Uuid::new_v4(),
            matcher,
            main_placement: SavedPlacement {
                monitor_id: "A".to_string(),
                main_monitor_index: 0,
                normalized_rect: NormalizedRect {
                    x: 0.0,
                    y: 0.0,
                    width: 0.5,
                    height: 0.5,
                },
                physical_rect_at_capture: rect,
                show_state: SavedShowState::Normal,
            },
            z_order: 0,
            launch_spec: None,
        };
        Workset {
            id: Uuid::new_v4(),
            name: "test".to_string(),
            repository_path: PathBuf::from(r"D:\repo"),
            repository_kind: RepositoryKind::Git,
            color: "#fff".to_string(),
            sort_order: 0,
            direct_hotkey: None,
            parking_policy: ParkingPolicy::Auto,
            fullscreen_when_parked: false,
            windows: vec![managed],
            created_at: "2026-07-21T00:00:00Z".to_string(),
            updated_at: "2026-07-21T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn window_fully_inside_a_live_monitor_is_not_offscreen() {
        let rect = PixelRect::new(100, 100, 800, 600);
        let workset = workset_with_window(rect);
        let live = [window_with(
            1,
            r"C:\code.exe",
            "Chrome_WidgetWin_1",
            "repo - Visual Studio Code",
            rect,
        )];
        let monitors = [monitor("A", PixelRect::new(0, 0, 1920, 1080))];

        assert!(find_now_offscreen_windows(&[workset], &live, &monitors).is_empty());
    }

    #[test]
    fn window_fully_outside_every_live_monitor_is_offscreen() {
        let rect = PixelRect::new(5000, 5000, 800, 600);
        let workset = workset_with_window(rect);
        let live = [window_with(
            1,
            r"C:\code.exe",
            "Chrome_WidgetWin_1",
            "repo - Visual Studio Code",
            rect,
        )];
        let monitors = [monitor("A", PixelRect::new(0, 0, 1920, 1080))];

        assert_eq!(
            find_now_offscreen_windows(&[workset], &live, &monitors),
            vec![1]
        );
    }

    #[test]
    fn window_straddling_a_monitor_edge_is_not_offscreen() {
        let rect = PixelRect::new(1800, 0, 400, 400);
        let workset = workset_with_window(rect);
        let live = [window_with(
            1,
            r"C:\code.exe",
            "Chrome_WidgetWin_1",
            "repo - Visual Studio Code",
            rect,
        )];
        let monitors = [monitor("A", PixelRect::new(0, 0, 1920, 1080))];

        assert!(find_now_offscreen_windows(&[workset], &live, &monitors).is_empty());
    }
}
