//! Placement domain types and pure geometry math, independent of Win32 and Slint
//! (PLAN.md §4.1, §7.2, §9.3).

use serde::{Deserialize, Serialize};

/// A window's normal/maximized/minimized state, as saved in a placement (PLAN.md §7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedShowState {
    Normal,
    Maximized,
    Minimized,
}

/// A rectangle in physical pixel coordinates, as returned by Win32 APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PixelRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl PixelRect {
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> i32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.height
    }
}

/// A rectangle normalized against a monitor's work area, per PLAN.md §4.1.
///
/// Values are stored as-captured and are not mechanically clamped to `0.0..=1.0`;
/// a window that extends slightly past its work area at capture time yields a
/// value outside that range so the discrepancy remains diagnosable. Callers that
/// restore a placement onto real screen geometry are responsible for clamping.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NormalizedRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Converts `window` into coordinates normalized against `work_area` (PLAN.md §4.1).
pub fn normalize(window: PixelRect, work_area: PixelRect) -> NormalizedRect {
    NormalizedRect {
        x: (window.x - work_area.x) as f64 / work_area.width as f64,
        y: (window.y - work_area.y) as f64 / work_area.height as f64,
        width: window.width as f64 / work_area.width as f64,
        height: window.height as f64 / work_area.height as f64,
    }
}

/// Converts a normalized rect back into physical pixel coordinates against `work_area`.
///
/// This is the mechanical inverse of [`normalize`]; it performs no clamping or
/// minimum-size enforcement. Restoring a saved main placement onto live screen
/// geometry (PLAN.md §4.2) applies those rules on top of this function.
pub fn denormalize(rect: NormalizedRect, work_area: PixelRect) -> PixelRect {
    PixelRect {
        x: work_area.x + (rect.x * work_area.width as f64).round() as i32,
        y: work_area.y + (rect.y * work_area.height as f64).round() as i32,
        width: (rect.width * work_area.width as f64).round() as i32,
        height: (rect.height * work_area.height as f64).round() as i32,
    }
}

/// Smallest axis-aligned rectangle containing all of `rects` (PLAN.md §4.3 `source_bounds`).
///
/// Returns `None` for an empty slice; there is no meaningful bounding rectangle.
pub fn bounding_rect(rects: &[PixelRect]) -> Option<PixelRect> {
    let first = *rects.first()?;
    let mut left = first.x;
    let mut top = first.y;
    let mut right = first.right();
    let mut bottom = first.bottom();

    for rect in &rects[1..] {
        left = left.min(rect.x);
        top = top.min(rect.y);
        right = right.max(rect.right());
        bottom = bottom.max(rect.bottom());
    }

    Some(PixelRect::new(left, top, right - left, bottom - top))
}

/// A `ManagedWindow`'s placement while its workset is the main (foreground) set
/// (PLAN.md §2.3, §7.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedPlacement {
    pub monitor_id: String,
    pub main_monitor_index: usize,
    pub normalized_rect: NormalizedRect,
    pub physical_rect_at_capture: PixelRect,
    pub show_state: SavedShowState,
}

/// Maps `window` from the `source` bounding rectangle into the `target` rectangle,
/// scaling X and Y independently without preserving aspect ratio (PLAN.md §4.3).
pub fn affine_map(window: PixelRect, source: PixelRect, target: PixelRect) -> PixelRect {
    let scale_x = target.width as f64 / source.width as f64;
    let scale_y = target.height as f64 / source.height as f64;

    PixelRect {
        x: target.x + ((window.x - source.x) as f64 * scale_x).round() as i32,
        y: target.y + ((window.y - source.y) as f64 * scale_y).round() as i32,
        width: (window.width as f64 * scale_x).round() as i32,
        height: (window.height as f64 * scale_y).round() as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_round_trip_within_1px(window: PixelRect, work_area: PixelRect) {
        let normalized = normalize(window, work_area);
        let restored = denormalize(normalized, work_area);

        assert!(
            (restored.x - window.x).abs() <= 1,
            "x drifted: {} -> {}",
            window.x,
            restored.x
        );
        assert!(
            (restored.y - window.y).abs() <= 1,
            "y drifted: {} -> {}",
            window.y,
            restored.y
        );
        assert!(
            (restored.width - window.width).abs() <= 1,
            "width drifted: {} -> {}",
            window.width,
            restored.width
        );
        assert!(
            (restored.height - window.height).abs() <= 1,
            "height drifted: {} -> {}",
            window.height,
            restored.height
        );
    }

    #[test]
    fn round_trip_positive_coordinates() {
        assert_round_trip_within_1px(
            PixelRect::new(100, 50, 800, 600),
            PixelRect::new(0, 0, 1920, 1032),
        );
    }

    #[test]
    fn round_trip_negative_virtual_screen_coordinates() {
        assert_round_trip_within_1px(
            PixelRect::new(-2400, -1000, 640, 480),
            PixelRect::new(-2560, -1429, 2560, 1392),
        );
    }

    #[test]
    fn round_trip_high_dpi_work_area() {
        assert_round_trip_within_1px(
            PixelRect::new(3900, 40, 1200, 900),
            PixelRect::new(3840, 0, 2560, 1392),
        );
    }

    #[test]
    fn bounding_rect_of_two_side_by_side_windows() {
        let left = PixelRect::new(0, 0, 960, 1032);
        let right = PixelRect::new(960, 0, 960, 1032);

        let bounds = bounding_rect(&[left, right]).unwrap();

        assert_eq!(bounds, PixelRect::new(0, 0, 1920, 1032));
    }

    #[test]
    fn bounding_rect_of_empty_slice_is_none() {
        assert!(bounding_rect(&[]).is_none());
    }

    #[test]
    fn affine_map_shrinks_two_windows_independently_in_x_and_y() {
        let source = PixelRect::new(0, 0, 1920, 1032);
        let target = PixelRect::new(2000, 100, 400, 250);

        let left = PixelRect::new(0, 0, 960, 1032);
        let mapped_left = affine_map(left, source, target);
        assert_eq!(mapped_left, PixelRect::new(2000, 100, 200, 250));

        let right = PixelRect::new(960, 0, 960, 1032);
        let mapped_right = affine_map(right, source, target);
        assert_eq!(mapped_right, PixelRect::new(2200, 100, 200, 250));
    }
}
