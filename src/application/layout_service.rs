//! Pure logic behind the Layout Studio screen: projecting monitors onto a
//! canvas, computing auto-split grid cells, and finding which windows sit on
//! the main screen (PLAN.md §3.5, §4.4, Phase 4).
//!
//! Functions here take already-fetched data (monitor lists, enumerated
//! windows) rather than calling into `windowing` themselves, so the actual
//! decision logic is testable without a real Win32 environment. The one
//! pragmatic exception is [`TopLevelWindow`] itself: reusing it here (instead
//! of a duplicate `application`-local struct) avoids ceremony for no real
//! benefit, since nothing in this module needs to substitute a fake
//! implementation of window enumeration — only the enumerated *data*.

use crate::domain::monitor::AutoSplit;
use crate::domain::placement::{NormalizedRect, PixelRect, SavedShowState, bounding_rect};
use crate::windowing::enumerate::TopLevelWindow;

/// A monitor's bounds mapped onto a canvas, in logical pixels, preserving the
/// monitor's real aspect ratio (PLAN.md §15: "モニター図は実座標比率を維持").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Projects `monitor_bounds` (physical pixels, may include negative
/// coordinates) onto a `canvas_width` x `canvas_height` logical-pixel canvas,
/// uniformly scaled (never stretched) and centered within `padding`.
///
/// Returns an empty vector for an empty input; the output is parallel to
/// `monitor_bounds` (same length, same order).
pub fn project_monitors_to_canvas(
    monitor_bounds: &[PixelRect],
    canvas_width: f64,
    canvas_height: f64,
    padding: f64,
) -> Vec<CanvasRect> {
    let Some(virtual_bounds) = bounding_rect(monitor_bounds) else {
        return Vec::new();
    };

    let available_w = (canvas_width - 2.0 * padding).max(1.0);
    let available_h = (canvas_height - 2.0 * padding).max(1.0);

    let scale = (available_w / f64::from(virtual_bounds.width))
        .min(available_h / f64::from(virtual_bounds.height));

    let scaled_w = f64::from(virtual_bounds.width) * scale;
    let scaled_h = f64::from(virtual_bounds.height) * scale;
    let offset_x = padding + (available_w - scaled_w) / 2.0;
    let offset_y = padding + (available_h - scaled_h) / 2.0;

    monitor_bounds
        .iter()
        .map(|m| CanvasRect {
            x: offset_x + f64::from(m.x - virtual_bounds.x) * scale,
            y: offset_y + f64::from(m.y - virtual_bounds.y) * scale,
            width: f64::from(m.width) * scale,
            height: f64::from(m.height) * scale,
        })
        .collect()
}

/// Picks a concrete split for a monitor whose `auto_split` is unset —
/// 「自動」, the default for newly seen monitors: 4K-class work areas take
/// four parked windows comfortably, wide QHD-class areas two columns, and
/// anything smaller (or portrait) stays whole.
pub fn resolve_auto_split(work_area: PixelRect) -> AutoSplit {
    if work_area.width >= 3400 && work_area.height >= 1700 {
        AutoSplit::FourGrid
    } else if work_area.width >= 2200 && work_area.width > work_area.height {
        AutoSplit::TwoColumns
    } else {
        AutoSplit::One
    }
}

/// Splits `work_area` into the grid cells implied by `split` (PLAN.md §4.4):
/// `One` is the whole area, `TwoColumns` splits left/right, `FourGrid` splits
/// into four quadrants. Cell order is stable (reading order: left-to-right,
/// top-to-bottom) so a `cell_index` persisted in a [`FixedParkingSlot`]
/// (`crate::domain::workset::FixedParkingSlot`) round-trips correctly.
pub fn auto_split_cells(work_area: PixelRect, split: AutoSplit) -> Vec<PixelRect> {
    match split {
        AutoSplit::One => vec![work_area],
        AutoSplit::TwoColumns => {
            let left_width = work_area.width / 2;
            vec![
                PixelRect::new(work_area.x, work_area.y, left_width, work_area.height),
                PixelRect::new(
                    work_area.x + left_width,
                    work_area.y,
                    work_area.width - left_width,
                    work_area.height,
                ),
            ]
        }
        AutoSplit::FourGrid => {
            let left_width = work_area.width / 2;
            let top_height = work_area.height / 2;
            vec![
                PixelRect::new(work_area.x, work_area.y, left_width, top_height),
                PixelRect::new(
                    work_area.x + left_width,
                    work_area.y,
                    work_area.width - left_width,
                    top_height,
                ),
                PixelRect::new(
                    work_area.x,
                    work_area.y + top_height,
                    left_width,
                    work_area.height - top_height,
                ),
                PixelRect::new(
                    work_area.x + left_width,
                    work_area.y + top_height,
                    work_area.width - left_width,
                    work_area.height - top_height,
                ),
            ]
        }
    }
}

/// The same grid cell as [`auto_split_cells`], but expressed as a fraction of
/// the monitor's work area rather than pixels.
///
/// This is what lets a workset declare "this app opens on the left half of
/// DISPLAY1" without knowing that monitor's resolution: the fraction is stored
/// in `SavedPlacement::normalized_rect` and resolved against whatever work area
/// the monitor has at switch time. Cell order matches `auto_split_cells`
/// exactly (reading order), so a `cell_index` means the same thing in both.
/// An out-of-range `cell_index` falls back to the whole area.
pub fn normalized_split_cell(split: AutoSplit, cell_index: usize) -> NormalizedRect {
    let whole = NormalizedRect {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    };
    if cell_index >= split.cell_count() {
        return whole;
    }
    match split {
        AutoSplit::One => whole,
        AutoSplit::TwoColumns => NormalizedRect {
            x: if cell_index == 0 { 0.0 } else { 0.5 },
            y: 0.0,
            width: 0.5,
            height: 1.0,
        },
        AutoSplit::FourGrid => NormalizedRect {
            x: if cell_index.is_multiple_of(2) {
                0.0
            } else {
                0.5
            },
            y: if cell_index < 2 { 0.0 } else { 0.5 },
            width: 0.5,
            height: 0.5,
        },
    }
}

/// The inverse of [`normalized_split_cell`]: which split and cell a saved
/// normalized rectangle describes.
///
/// Editing a set has to show the 分割 / 位置 the set was registered with, and
/// all that is persisted is the rectangle. Anything that isn't recognisably one
/// of the grid cells (a rectangle captured from a hand-dragged window, from
/// before sets were declared) reads as whole-monitor, which is the least
/// surprising thing to re-save. The tolerance absorbs the rounding in
/// `auto_split_cells`' integer pixel division.
pub fn split_cell_from_normalized(rect: NormalizedRect) -> (AutoSplit, usize) {
    const TOLERANCE: f64 = 0.02;
    for split in [AutoSplit::FourGrid, AutoSplit::TwoColumns, AutoSplit::One] {
        for cell_index in 0..split.cell_count() {
            let candidate = normalized_split_cell(split, cell_index);
            if (candidate.x - rect.x).abs() < TOLERANCE
                && (candidate.y - rect.y).abs() < TOLERANCE
                && (candidate.width - rect.width).abs() < TOLERANCE
                && (candidate.height - rect.height).abs() < TOLERANCE
            {
                return (split, cell_index);
            }
        }
    }
    (AutoSplit::One, 0)
}

/// Selects the windows from `windows` whose center point falls on one of
/// `main_monitor_bounds` (PLAN.md §3.6's candidate rule, reused here for
/// "メインを空にする"'s "メイン画面と交差するトップレベルウィンドウを列挙").
pub fn find_windows_on_main_screen(
    windows: &[TopLevelWindow],
    main_monitor_bounds: &[PixelRect],
) -> Vec<TopLevelWindow> {
    windows
        .iter()
        .filter(|window| {
            let (cx, cy) = window.rect_px.center();
            main_monitor_bounds
                .iter()
                .any(|bounds| bounds.contains_point(cx, cy))
        })
        .cloned()
        .collect()
}

/// One window's placement before a destructive Layout Studio action, kept so
/// [`UndoSnapshot`] can restore it (PLAN.md §3.5: "操作前配置をUndoスナップショット
/// として保存").
#[derive(Debug, Clone)]
pub struct UndoEntry {
    pub hwnd: isize,
    pub process_id: u32,
    pub before_rect: PixelRect,
    pub before_show_state: SavedShowState,
}

/// A single rolling undo slot for "メインを空にする" (PLAN.md §3.5:
/// "Undoスナップショットは次の破壊的でない操作まで保持する" — one snapshot, not a
/// full history stack).
#[derive(Debug, Clone, Default)]
pub struct UndoSnapshot {
    pub entries: Vec<UndoEntry>,
}

impl UndoSnapshot {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The declared cell (workset registration) and the parking cell (Layout
    /// Studio) must describe the same rectangle, or "左半分" would mean two
    /// different things in the two screens.
    #[test]
    fn normalized_split_cells_match_auto_split_cells() {
        let work_area = PixelRect::new(100, 200, 1920, 1080);
        for split in [AutoSplit::One, AutoSplit::TwoColumns, AutoSplit::FourGrid] {
            let pixel_cells = auto_split_cells(work_area, split);
            for (index, cell) in pixel_cells.iter().enumerate() {
                let normalized = normalized_split_cell(split, index);
                let expected_x = (cell.x - work_area.x) as f64 / work_area.width as f64;
                let expected_y = (cell.y - work_area.y) as f64 / work_area.height as f64;
                let expected_w = cell.width as f64 / work_area.width as f64;
                let expected_h = cell.height as f64 / work_area.height as f64;
                assert!(
                    (normalized.x - expected_x).abs() < 0.001
                        && (normalized.y - expected_y).abs() < 0.001
                        && (normalized.width - expected_w).abs() < 0.001
                        && (normalized.height - expected_h).abs() < 0.001,
                    "{split:?} cell {index}: {normalized:?} != {cell:?}"
                );
            }
        }
    }

    /// Registering a placement and then editing the set must show back the same
    /// 分割 / 位置 the user picked.
    #[test]
    fn split_and_cell_round_trip_through_a_normalized_rect() {
        for split in [AutoSplit::One, AutoSplit::TwoColumns, AutoSplit::FourGrid] {
            for cell_index in 0..split.cell_count() {
                let rect = normalized_split_cell(split, cell_index);
                assert_eq!(split_cell_from_normalized(rect), (split, cell_index));
            }
        }
    }

    #[test]
    fn an_unrecognised_rect_reads_as_the_whole_monitor() {
        let odd = NormalizedRect {
            x: 0.13,
            y: 0.27,
            width: 0.41,
            height: 0.62,
        };
        assert_eq!(split_cell_from_normalized(odd), (AutoSplit::One, 0));
    }

    #[test]
    fn out_of_range_cell_falls_back_to_the_whole_monitor() {
        let cell = normalized_split_cell(AutoSplit::TwoColumns, 7);
        assert_eq!(cell.width, 1.0);
        assert_eq!(cell.height, 1.0);
    }

    #[test]
    fn project_single_monitor_fills_canvas_minus_padding() {
        let monitors = [PixelRect::new(0, 0, 1920, 1080)];
        let projected = project_monitors_to_canvas(&monitors, 400.0, 300.0, 10.0);

        assert_eq!(projected.len(), 1);
        let r = projected[0];
        // 1920x1080 into a 380x280 box, uniform scale -> width-limited.
        assert!((r.width - 380.0).abs() < 0.01, "width={}", r.width);
        assert!(r.height < 280.0);
        assert!((r.x - 10.0).abs() < 0.01);
    }

    #[test]
    fn project_preserves_relative_position_with_negative_coordinates() {
        // Mirrors this machine's real layout: a monitor above-left of the primary,
        // at negative coordinates.
        let monitors = [
            PixelRect::new(0, 0, 1920, 1080),
            PixelRect::new(-1920, -1080, 1920, 1080),
        ];
        let projected = project_monitors_to_canvas(&monitors, 800.0, 600.0, 20.0);

        let primary = projected[0];
        let secondary = projected[1];

        // The secondary monitor must render strictly above and to the left of
        // the primary, matching its real relative position.
        assert!(secondary.x < primary.x);
        assert!(secondary.y < primary.y);
        // Same physical size -> same canvas size (uniform scale).
        assert!((secondary.width - primary.width).abs() < 0.01);
        assert!((secondary.height - primary.height).abs() < 0.01);
    }

    #[test]
    fn project_empty_input_returns_empty() {
        assert!(project_monitors_to_canvas(&[], 400.0, 300.0, 10.0).is_empty());
    }

    #[test]
    fn auto_split_one_is_the_whole_area() {
        let area = PixelRect::new(1920, 0, 1920, 1080);
        assert_eq!(auto_split_cells(area, AutoSplit::One), vec![area]);
    }

    #[test]
    fn auto_split_two_columns_splits_left_right_without_gaps_or_overlap() {
        let area = PixelRect::new(1920, 0, 1921, 1080);
        let cells = auto_split_cells(area, AutoSplit::TwoColumns);

        assert_eq!(cells.len(), 2);
        assert_eq!(
            cells[0].right(),
            cells[1].x,
            "no gap or overlap between columns"
        );
        assert_eq!(cells[0].x, area.x);
        assert_eq!(
            cells[1].right(),
            area.right(),
            "odd width fully covered, remainder on the right cell"
        );
        assert_eq!(cells[0].height, area.height);
        assert_eq!(cells[1].height, area.height);
    }

    #[test]
    fn auto_split_four_grid_covers_area_in_four_quadrants() {
        let area = PixelRect::new(0, 0, 2561, 1441);
        let cells = auto_split_cells(area, AutoSplit::FourGrid);

        assert_eq!(cells.len(), 4);
        let bounds = bounding_rect(&cells).unwrap();
        assert_eq!(bounds, area, "quadrants exactly tile the source area");
    }

    #[test]
    fn resolve_auto_split_picks_split_by_work_area_size() {
        let cases = [
            (PixelRect::new(0, 0, 3840, 2160), AutoSplit::FourGrid),
            (PixelRect::new(0, 0, 2560, 1440), AutoSplit::TwoColumns),
            (PixelRect::new(0, 0, 1920, 1080), AutoSplit::One),
            // Portrait stays whole even at high resolution.
            (PixelRect::new(0, 0, 1440, 2560), AutoSplit::One),
        ];
        for (area, expected) in cases {
            assert_eq!(
                resolve_auto_split(area),
                expected,
                "work area {}x{}",
                area.width,
                area.height
            );
        }
    }

    #[test]
    fn find_windows_on_main_screen_filters_by_center_point() {
        let main_bounds = [PixelRect::new(0, 0, 1920, 1080)];

        let on_main = TopLevelWindow {
            hwnd: 1,
            process_id: 100,
            executable_path: None,
            window_class: "Notepad".to_string(),
            title: "on main".to_string(),
            rect_px: PixelRect::new(100, 100, 400, 300),
        };
        let off_main = TopLevelWindow {
            hwnd: 2,
            process_id: 200,
            executable_path: None,
            window_class: "Notepad".to_string(),
            title: "off main".to_string(),
            rect_px: PixelRect::new(2000, 100, 400, 300),
        };

        let found = find_windows_on_main_screen(&[on_main.clone(), off_main], &main_bounds);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].hwnd, on_main.hwnd);
    }
}
