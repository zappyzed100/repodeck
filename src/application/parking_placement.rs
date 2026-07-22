//! Shrinking a workset's main-placement layout into a parking slot, or
//! deciding the whole workset must be minimized instead (PLAN.md §4.3).

use crate::domain::placement::{PixelRect, affine_map, bounding_rect};

/// Minimum displayed size for a parked window (PLAN.md §4.3).
pub const MIN_PARKED_WIDTH: i32 = 120;
pub const MIN_PARKED_HEIGHT: i32 = 68;
/// Inset applied to a parking slot's rect before mapping windows into it.
/// Zero since 2026-07-23: with per-window subdivided cells, an 8px inset put a
/// 16px gutter between neighbouring cells and the user read the gaps as the
/// split "not working" — cells now tile flush.
pub const PARKING_SLOT_INSET_PX: i32 = 0;

#[derive(Debug, Clone, PartialEq)]
pub enum ParkPlan {
    /// Parallel to the input slice: one target rect per window.
    ShrinkToFit(Vec<PixelRect>),
    MinimizeWhole,
}

/// Maps `main_rects` (each window's resolved main-placement rect) into
/// `slot_rect` (the parking cell's raw rect, before inset), scaling X and Y
/// independently (PLAN.md §4.3). If any window would end up below the
/// minimum displayed size, the whole workset is minimized instead — never a
/// partial mix of parked and minimized windows for one workset.
pub fn plan_park_into_slot(main_rects: &[PixelRect], slot_rect: PixelRect) -> ParkPlan {
    let Some(source_bounds) = bounding_rect(main_rects) else {
        return ParkPlan::ShrinkToFit(Vec::new());
    };
    let target_bounds = inset(slot_rect, PARKING_SLOT_INSET_PX);

    let mapped: Vec<PixelRect> = main_rects
        .iter()
        .map(|&rect| affine_map(rect, source_bounds, target_bounds))
        .collect();

    if mapped
        .iter()
        .any(|r| r.width < MIN_PARKED_WIDTH || r.height < MIN_PARKED_HEIGHT)
    {
        ParkPlan::MinimizeWhole
    } else {
        ParkPlan::ShrinkToFit(mapped)
    }
}

fn inset(rect: PixelRect, px: i32) -> PixelRect {
    PixelRect::new(
        rect.x + px,
        rect.y + px,
        (rect.width - 2 * px).max(0),
        (rect.height - 2 * px).max(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrinks_two_side_by_side_windows_into_one_slot() {
        let main_rects = vec![
            PixelRect::new(0, 0, 960, 1032),
            PixelRect::new(960, 0, 960, 1032),
        ];
        let slot_rect = PixelRect::new(2000, 100, 408, 266);

        let plan = plan_park_into_slot(&main_rects, slot_rect);

        let ParkPlan::ShrinkToFit(rects) = plan else {
            panic!("expected ShrinkToFit");
        };
        assert_eq!(rects.len(), 2);
        for rect in &rects {
            assert!(rect.width >= MIN_PARKED_WIDTH);
            assert!(rect.height >= MIN_PARKED_HEIGHT);
        }
        // Left window stays left of the right window after mapping.
        assert!(rects[0].x < rects[1].x);
    }

    #[test]
    fn below_minimum_size_minimizes_the_whole_workset() {
        let main_rects = vec![
            PixelRect::new(0, 0, 1920, 1080),
            PixelRect::new(1920, 0, 100, 100),
        ];
        // Tiny slot: mapping shrinks everything well under 120x68.
        let slot_rect = PixelRect::new(0, 0, 40, 30);

        let plan = plan_park_into_slot(&main_rects, slot_rect);

        assert_eq!(plan, ParkPlan::MinimizeWhole);
    }

    #[test]
    fn empty_main_rects_yields_an_empty_plan() {
        assert_eq!(
            plan_park_into_slot(&[], PixelRect::new(0, 0, 400, 300)),
            ParkPlan::ShrinkToFit(Vec::new())
        );
    }
}
