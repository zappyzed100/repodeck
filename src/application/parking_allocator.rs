//! Parking-slot identity and the auto-slot allocation algorithm (PLAN.md §3.7, §4.4).

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::application::layout_service::auto_split_cells;
use crate::application::monitor_resolution::{
    find_live_monitor_by_stable_id, sort_monitors_reading_order,
};
use crate::application::switch_coordinator::{screen_capacity, subdivide_for_count};
use crate::domain::monitor::{AutoSplit, SavedMonitor};
use crate::domain::placement::PixelRect;
use crate::domain::workset::{FixedParkingSlot, ParkingPolicy, Workset};
use crate::windowing::monitor::MonitorInfo;

/// A parking cell's stable identity: which monitor, which grid cell on it.
/// Distinct from `layout_service::auto_split_cells`'s output, which is the
/// cell's pixel geometry, not its identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParkingSlotId {
    pub monitor_id: String,
    pub cell_index: usize,
}

impl ParkingSlotId {
    /// Encoding used for `RuntimeState::auto_slot_assignments` values.
    /// `"::"` is used as the separator and decoding splits from the right,
    /// so a monitor id containing `"::"` (Win32 device names like
    /// `\\.\DISPLAY1` never do) would still round-trip correctly.
    pub fn encode(&self) -> String {
        format!("{}::{}", self.monitor_id, self.cell_index)
    }

    pub fn decode(s: &str) -> Option<Self> {
        let (monitor_id, cell_index) = s.rsplit_once("::")?;
        Some(Self {
            monitor_id: monitor_id.to_string(),
            cell_index: cell_index.parse().ok()?,
        })
    }
}

/// Where a non-current workset ends up (PLAN.md §3.7).
#[derive(Debug, Clone, PartialEq)]
pub enum ParkAssignment {
    AutoSlot {
        slot: ParkingSlotId,
        rect: PixelRect,
    },
    FixedSlot {
        slot: ParkingSlotId,
        rect: PixelRect,
    },
    Minimized,
}

pub struct AllocationInput<'a> {
    pub worksets: &'a [Workset],
    /// The workset that is (or will be) current; excluded from parking entirely.
    pub current_workset_id: Option<Uuid>,
    pub fixed_slots: &'a [FixedParkingSlot],
    pub main_monitor_ids: &'a [String],
    pub live_monitors: &'a [MonitorInfo],
    pub saved_monitors: &'a [SavedMonitor],
    /// Device names of every monitor a sub-screen (退避先エリア) uses. These are
    /// reserved for their designated worksets and excluded from the auto pool,
    /// so a non-designated workset never parks onto a sub-screen (確定仕様
    /// 2026-07-22: 「サブ以外の退避先の画面に詰める」).
    pub sub_screen_monitor_ids: &'a [String],
    /// Decoded from `RuntimeState.auto_slot_assignments`.
    pub previous_assignments: &'a HashMap<Uuid, ParkingSlotId>,
    /// Ids of worksets that currently have at least one live window on screen.
    /// Only these reserve an auto parking cell — a set whose apps are all closed
    /// must not consume space and shrink the live windows' cells (2026-07-23).
    pub worksets_with_windows: &'a HashSet<Uuid>,
}

pub struct AllocationResult {
    pub assignments: HashMap<Uuid, ParkAssignment>,
    /// Only the `AutoSlot` subset of `assignments`, keyed for persistence —
    /// this is exactly what gets re-encoded into `RuntimeState.auto_slot_assignments`.
    pub new_auto_slot_assignments: HashMap<Uuid, ParkingSlotId>,
}

/// Decides where every non-current workset parks (PLAN.md §3.7, §4.4).
///
/// Fixed-policy worksets are resolved independently of the auto pool: they
/// always go to their designated slot, or are minimized if that slot's
/// monitor is currently missing (PLAN.md §4.6's "固定先モニター切断"). Every
/// fixed slot is excluded from the auto pool unconditionally, whether or not
/// its owning workset is actually parked right now.
///
/// Auto-policy worksets are assigned deterministically: monitors are visited
/// in reading order, cells within a monitor in `auto_split_cells`'s own
/// stable reading order, worksets in `sort_order` order. A workset keeps its
/// previous cell if it is still available; only unassigned worksets consume
/// fresh cells via First Fit. Anything left over is minimized.
pub fn allocate_parking(input: &AllocationInput) -> AllocationResult {
    let mut assignments = HashMap::new();
    let mut fixed_cells: HashSet<ParkingSlotId> = HashSet::new();

    // Fixed-slot pass (PLAN.md §3.7): independent of, and takes priority
    // over, the auto pool.
    for workset in input.worksets {
        if Some(workset.id) == input.current_workset_id {
            continue;
        }
        let ParkingPolicy::Fixed { slot_id } = &workset.parking_policy else {
            continue;
        };
        let Some(slot) = input.fixed_slots.iter().find(|s| s.id == *slot_id) else {
            continue;
        };
        let parking_slot_id = ParkingSlotId {
            monitor_id: slot.monitor_id.clone(),
            cell_index: slot.cell_index,
        };
        fixed_cells.insert(parking_slot_id.clone());

        // An excluded monitor is treated like a missing one (PLAN.md §4.6's
        // 「固定先モニター切断」): the workset is minimized, never parked there.
        let slot_monitor_excluded = input
            .saved_monitors
            .iter()
            .any(|s| s.stable_id == slot.monitor_id && s.excluded);
        let assignment = match find_live_monitor_by_stable_id(input.live_monitors, &slot.monitor_id)
        {
            _ if slot_monitor_excluded => ParkAssignment::Minimized,
            None => ParkAssignment::Minimized,
            Some(monitor) => {
                let cells = auto_split_cells(monitor.work_area_px, slot.grid);
                match cells.get(slot.cell_index) {
                    Some(&rect) => ParkAssignment::FixedSlot {
                        slot: parking_slot_id,
                        rect,
                    },
                    None => ParkAssignment::Minimized,
                }
            }
        };
        assignments.insert(workset.id, assignment);
    }

    // Auto-pool worksets (§4.4 step 4), deterministic order. A set with no live
    // window is skipped entirely: it has nothing to park, so reserving a cell
    // for it would only shrink the sets that do have windows.
    let mut auto_pool: Vec<&Workset> = input
        .worksets
        .iter()
        .filter(|w| {
            Some(w.id) != input.current_workset_id
                && w.parking_policy == ParkingPolicy::Auto
                && input.worksets_with_windows.contains(&w.id)
        })
        .collect();
    auto_pool.sort_by_key(|w| (w.sort_order, w.id));

    // Monitors available to the auto pool: non-main, non-excluded, non-sub, and
    // not already carrying a fixed cell (a monitor shared with a fixed slot is
    // left to that slot rather than mixed with dynamic subdivision), in reading
    // order for deterministic tie-breaks.
    let fixed_monitor_ids: HashSet<&str> =
        fixed_cells.iter().map(|c| c.monitor_id.as_str()).collect();
    let mut auto_monitors: Vec<MonitorInfo> = input
        .live_monitors
        .iter()
        .filter(|m| !input.main_monitor_ids.iter().any(|id| id == &m.device_name))
        .filter(|m| {
            !input
                .saved_monitors
                .iter()
                .any(|s| s.stable_id == m.device_name && s.excluded)
        })
        .filter(|m| {
            !input
                .sub_screen_monitor_ids
                .iter()
                .any(|id| id == &m.device_name)
        })
        .filter(|m| !fixed_monitor_ids.contains(m.device_name.as_str()))
        .cloned()
        .collect();
    sort_monitors_reading_order(&mut auto_monitors);

    // A monitor's parking capacity: its saved `auto_split` read as a *maximum*
    // (One→1, TwoColumns→2, FourGrid→4), or the size-based default (QHD holds a
    // 3×2 of six, smaller screens four) when left on 「自動」. Crucially this is
    // only the cap — a monitor is subdivided by the number of windows that
    // *actually* park on it, so a lone window fills the whole screen instead of
    // being wedged into a fixed quarter while other screens sit empty
    // (2026-07-23 bug fix: 「空いてる退避画面があるのに1/4で詰め込まれる」).
    let capacity = |m: &MonitorInfo| -> usize {
        match input
            .saved_monitors
            .iter()
            .find(|s| s.stable_id == m.device_name)
            .and_then(|s| s.auto_split)
        {
            Some(AutoSplit::One) => 1,
            Some(AutoSplit::TwoColumns) => 2,
            Some(AutoSplit::FourGrid) => 4,
            None => screen_capacity(m.work_area_px),
        }
    };

    // Distribute worksets across monitors so total parked area is maximised:
    // light up the largest empty monitor first (never shrink a window while a
    // screen is empty), then the monitor whose next cell stays largest
    // (`area/(count+1)`). Same rule as `switch_coordinator::distribute_parking`.
    let index_by_id: HashMap<&str, usize> = auto_monitors
        .iter()
        .enumerate()
        .map(|(i, m)| (m.device_name.as_str(), i))
        .collect();
    let mut occupants: Vec<Vec<&Workset>> = vec![Vec::new(); auto_monitors.len()];
    let mut placed: HashSet<Uuid> = HashSet::new();

    // Stability (§4.4 step 5): keep a workset on its previous monitor when that
    // monitor is still an auto target with spare capacity, so windows don't
    // needlessly jump screens between switches.
    for workset in &auto_pool {
        if let Some(previous) = input.previous_assignments.get(&workset.id)
            && let Some(&i) = index_by_id.get(previous.monitor_id.as_str())
            && occupants[i].len() < capacity(&auto_monitors[i])
        {
            occupants[i].push(workset);
            placed.insert(workset.id);
        }
    }

    for workset in &auto_pool {
        if placed.contains(&workset.id) {
            continue;
        }
        let best = (0..auto_monitors.len())
            .filter(|&i| occupants[i].len() < capacity(&auto_monitors[i]))
            .max_by_key(|&i| {
                let area = i64::from(auto_monitors[i].work_area_px.width)
                    * i64::from(auto_monitors[i].work_area_px.height);
                let count = occupants[i].len() as i64;
                let lights_up = count == 0;
                let metric = if lights_up { area } else { area / (count + 1) };
                (lights_up, metric, -(i as i64))
            });
        // No monitor with room → left unplaced, minimized below.
        if let Some(i) = best {
            occupants[i].push(workset);
            placed.insert(workset.id);
        }
    }

    tracing::info!(
        target: "parking",
        auto_worksets = auto_pool.len(),
        monitors = ?auto_monitors
            .iter()
            .map(|m| (m.device_name.clone(), capacity(m), m.work_area_px.width, m.work_area_px.height))
            .collect::<Vec<_>>(),
        "auto-park: distributing across available monitors"
    );

    // Assign each monitor's occupants to cells subdivided by their *actual*
    // count (1→whole, 2→halves, up to the QHD 3×2 of six).
    let mut new_auto_slot_assignments = HashMap::new();
    for (i, monitor) in auto_monitors.iter().enumerate() {
        if occupants[i].is_empty() {
            continue;
        }
        let cells = subdivide_for_count(monitor.work_area_px, occupants[i].len());
        for (cell_index, workset) in occupants[i].iter().enumerate() {
            let rect = cells.get(cell_index).copied().unwrap_or(monitor.work_area_px);
            let slot = ParkingSlotId {
                monitor_id: monitor.device_name.clone(),
                cell_index,
            };
            tracing::info!(
                target: "parking",
                workset = %workset.id,
                name = %workset.name,
                monitor = %monitor.device_name,
                cell = cell_index,
                of = occupants[i].len(),
                rect = ?rect,
                "auto-park: assigned cell"
            );
            new_auto_slot_assignments.insert(workset.id, slot.clone());
            assignments.insert(workset.id, ParkAssignment::AutoSlot { slot, rect });
        }
    }

    // Overflow (§4.4 step 7): worksets that found no monitor with room.
    for workset in &auto_pool {
        if !placed.contains(&workset.id) {
            tracing::info!(
                target: "parking",
                workset = %workset.id,
                name = %workset.name,
                "auto-park: no monitor had room, minimizing"
            );
            assignments.insert(workset.id, ParkAssignment::Minimized);
        }
    }

    AllocationResult {
        assignments,
        new_auto_slot_assignments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::monitor::AutoSplit;

    fn monitor(device_name: &str, x: i32, work_width: i32) -> MonitorInfo {
        MonitorInfo {
            handle: 0,
            device_name: device_name.to_string(),
            bounds_px: PixelRect::new(x, 0, work_width, 1080),
            work_area_px: PixelRect::new(x, 0, work_width, 1080),
            dpi_x: 96,
            dpi_y: 96,
            is_primary: x == 0,
        }
    }

    fn saved_monitor(stable_id: &str, split: AutoSplit) -> SavedMonitor {
        SavedMonitor {
            stable_id: stable_id.to_string(),
            device_name: stable_id.to_string(),
            device_path: None,
            friendly_name: None,
            bounds_px: PixelRect::new(0, 0, 1920, 1080),
            work_area_px: PixelRect::new(0, 0, 1920, 1080),
            dpi_x: 96,
            dpi_y: 96,
            auto_split: Some(split),
            excluded: false,
        }
    }

    fn auto_workset(sort_order: i32) -> Workset {
        use crate::domain::workset::RepositoryKind;
        Workset {
            id: Uuid::new_v4(),
            name: format!("workset-{sort_order}"),
            repository_path: "C:\\repo".into(),
            repository_kind: RepositoryKind::Git,
            color: "#000000".to_string(),
            sort_order,
            direct_hotkey: None,
            parking_policy: ParkingPolicy::Auto,
            fullscreen_when_parked: false,
            windows: Vec::new(),
            created_at: "2026-07-20T00:00:00Z".to_string(),
            updated_at: "2026-07-20T00:00:00Z".to_string(),
        }
    }

    fn fixed_workset(sort_order: i32, slot_id: Uuid) -> Workset {
        let mut w = auto_workset(sort_order);
        w.parking_policy = ParkingPolicy::Fixed { slot_id };
        w
    }

    /// Test default: treat every workset as having live windows (the filter is
    /// exercised on its own where it matters).
    fn all_ids(worksets: &[Workset]) -> HashSet<Uuid> {
        worksets.iter().map(|w| w.id).collect()
    }

    #[test]
    fn two_monitors_no_parking_room_minimizes_overflow() {
        let live = vec![monitor("MAIN", 0, 1920), monitor("SIDE", 1920, 1920)];
        let saved = vec![saved_monitor("SIDE", AutoSplit::One)];
        let a = auto_workset(0);
        let b = auto_workset(1);
        let worksets = vec![a.clone(), b.clone()];

        let result = allocate_parking(&AllocationInput {
            worksets: &worksets,
            current_workset_id: None,
            fixed_slots: &[],
            main_monitor_ids: &["MAIN".to_string()],
            live_monitors: &live,
            saved_monitors: &saved,
            sub_screen_monitor_ids: &[],
            previous_assignments: &HashMap::new(),
            worksets_with_windows: &all_ids(&worksets),
        });

        assert!(matches!(
            result.assignments[&a.id],
            ParkAssignment::AutoSlot { .. }
        ));
        assert_eq!(result.assignments[&b.id], ParkAssignment::Minimized);
    }

    #[test]
    fn sub_screen_monitors_are_excluded_from_the_auto_pool() {
        // The only non-main monitor is reserved by a sub-screen, so an auto
        // workset has nowhere to park and is minimized rather than dumped onto
        // the sub (確定仕様 2026-07-22).
        let live = vec![monitor("MAIN", 0, 1920), monitor("SUB", 1920, 1920)];
        let saved = vec![saved_monitor("SUB", AutoSplit::One)];
        let a = auto_workset(0);
        let worksets = vec![a.clone()];

        let result = allocate_parking(&AllocationInput {
            worksets: &worksets,
            current_workset_id: None,
            fixed_slots: &[],
            main_monitor_ids: &["MAIN".to_string()],
            live_monitors: &live,
            saved_monitors: &saved,
            sub_screen_monitor_ids: &["SUB".to_string()],
            previous_assignments: &HashMap::new(),
            worksets_with_windows: &all_ids(&worksets),
        });

        assert_eq!(result.assignments[&a.id], ParkAssignment::Minimized);
    }

    #[test]
    fn few_worksets_are_not_packed_into_quarters_while_a_screen_is_empty() {
        // Regression (2026-07-23): two monitors both configured FourGrid, only
        // two worksets to park. The old fixed-grid First-Fit wedged both into
        // SIDE1's quarters (960×540) and left SIDE2 empty. They must instead
        // spread one-per-monitor at full size, subdivided by actual count.
        let live = vec![
            monitor("MAIN", 0, 1920),
            monitor("SIDE1", 1920, 1920),
            monitor("SIDE2", 3840, 1920),
        ];
        let saved = vec![
            saved_monitor("SIDE1", AutoSplit::FourGrid),
            saved_monitor("SIDE2", AutoSplit::FourGrid),
        ];
        let a = auto_workset(0);
        let b = auto_workset(1);
        let worksets = vec![a.clone(), b.clone()];

        let result = allocate_parking(&AllocationInput {
            worksets: &worksets,
            current_workset_id: None,
            fixed_slots: &[],
            main_monitor_ids: &["MAIN".to_string()],
            live_monitors: &live,
            saved_monitors: &saved,
            sub_screen_monitor_ids: &[],
            previous_assignments: &HashMap::new(),
            worksets_with_windows: &all_ids(&worksets),
        });

        let placed = |w: &Workset| match &result.assignments[&w.id] {
            ParkAssignment::AutoSlot { rect, slot } => (*rect, slot.monitor_id.clone()),
            other => panic!("expected AutoSlot, got {other:?}"),
        };
        let (rect_a, mon_a) = placed(&a);
        let (rect_b, mon_b) = placed(&b);
        assert_ne!(mon_a, mon_b, "the two worksets spread onto different monitors");
        // Full monitor size (1920×1080), not a 960×540 quarter.
        assert_eq!((rect_a.width, rect_a.height), (1920, 1080));
        assert_eq!((rect_b.width, rect_b.height), (1920, 1080));
    }

    #[test]
    fn a_set_with_no_live_windows_does_not_reserve_a_cell() {
        // Regression (2026-07-23): a set whose apps are all closed must not
        // reserve a parking cell — otherwise it shrinks the live sets. Here only
        // `a` has live windows, so it gets a whole monitor and `b` gets nothing.
        let live = vec![
            monitor("MAIN", 0, 1920),
            monitor("SIDE1", 1920, 1920),
            monitor("SIDE2", 3840, 1920),
        ];
        let a = auto_workset(0);
        let b = auto_workset(1);
        let worksets = vec![a.clone(), b.clone()];
        let only_a: HashSet<Uuid> = [a.id].into_iter().collect();

        let result = allocate_parking(&AllocationInput {
            worksets: &worksets,
            current_workset_id: None,
            fixed_slots: &[],
            main_monitor_ids: &["MAIN".to_string()],
            live_monitors: &live,
            saved_monitors: &[],
            sub_screen_monitor_ids: &[],
            previous_assignments: &HashMap::new(),
            worksets_with_windows: &only_a,
        });

        match &result.assignments[&a.id] {
            ParkAssignment::AutoSlot { rect, .. } => {
                assert_eq!((rect.width, rect.height), (1920, 1080), "live set fills a whole monitor");
            }
            other => panic!("expected AutoSlot whole monitor, got {other:?}"),
        }
        assert!(
            !result.assignments.contains_key(&b.id),
            "a set with no live windows must not be assigned a cell"
        );
    }

    #[test]
    fn four_monitors_multiple_auto_slots_are_distinct_and_stable() {
        let live = vec![
            monitor("MAIN", 0, 1920),
            monitor("SIDE1", 1920, 1920),
            monitor("SIDE2", 3840, 1920),
            monitor("SIDE3", 5760, 1920),
        ];
        let saved = vec![
            saved_monitor("SIDE1", AutoSplit::TwoColumns),
            saved_monitor("SIDE2", AutoSplit::FourGrid),
            saved_monitor("SIDE3", AutoSplit::One),
        ];
        let worksets: Vec<Workset> = (0..5).map(auto_workset).collect();
        let main_monitor_ids = vec!["MAIN".to_string()];

        let first = allocate_parking(&AllocationInput {
            worksets: &worksets,
            current_workset_id: None,
            fixed_slots: &[],
            main_monitor_ids: &main_monitor_ids,
            live_monitors: &live,
            saved_monitors: &saved,
            sub_screen_monitor_ids: &[],
            previous_assignments: &HashMap::new(),
            worksets_with_windows: &all_ids(&worksets),
        });

        let mut assigned_slots: Vec<&ParkingSlotId> = Vec::new();
        for workset in &worksets {
            if let ParkAssignment::AutoSlot { slot, .. } = &first.assignments[&workset.id] {
                assigned_slots.push(slot);
            }
        }
        let distinct: HashSet<&ParkingSlotId> = assigned_slots.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            assigned_slots.len(),
            "no two worksets share a cell"
        );
        assert_eq!(
            assigned_slots.len(),
            5,
            "1+2+4+... covers all 5 worksets across 2+4+1=7 cells"
        );

        // Determinism/stability: feeding the result back in yields an identical allocation.
        let second = allocate_parking(&AllocationInput {
            worksets: &worksets,
            current_workset_id: None,
            fixed_slots: &[],
            main_monitor_ids: &main_monitor_ids,
            live_monitors: &live,
            saved_monitors: &saved,
            sub_screen_monitor_ids: &[],
            previous_assignments: &first.new_auto_slot_assignments,
            worksets_with_windows: &all_ids(&worksets),
        });
        assert_eq!(second.assignments, first.assignments);
    }

    #[test]
    fn eight_monitors_fixed_and_auto_mixed_never_collide() {
        let mut live: Vec<MonitorInfo> = vec![monitor("MAIN", 0, 1920)];
        for i in 1..8 {
            live.push(monitor(&format!("SIDE{i}"), 1920 * i, 1920));
        }
        let saved: Vec<SavedMonitor> = (1..8)
            .map(|i| saved_monitor(&format!("SIDE{i}"), AutoSplit::TwoColumns))
            .collect();

        let fixed_slot_id = Uuid::new_v4();
        let fixed_slots = vec![FixedParkingSlot {
            id: fixed_slot_id,
            monitor_id: "SIDE1".to_string(),
            grid: AutoSplit::TwoColumns,
            cell_index: 0,
            assigned_workset_id: Uuid::nil(),
        }];

        let fixed = fixed_workset(0, fixed_slot_id);
        let autos: Vec<Workset> = (1..6).map(auto_workset).collect();
        let mut worksets = vec![fixed.clone()];
        worksets.extend(autos.iter().cloned());
        let main_monitor_ids = vec!["MAIN".to_string()];

        let result = allocate_parking(&AllocationInput {
            worksets: &worksets,
            current_workset_id: None,
            fixed_slots: &fixed_slots,
            main_monitor_ids: &main_monitor_ids,
            live_monitors: &live,
            saved_monitors: &saved,
            sub_screen_monitor_ids: &[],
            previous_assignments: &HashMap::new(),
            worksets_with_windows: &all_ids(&worksets),
        });

        let fixed_target = ParkingSlotId {
            monitor_id: "SIDE1".to_string(),
            cell_index: 0,
        };
        assert_eq!(
            result.assignments[&fixed.id],
            ParkAssignment::FixedSlot {
                slot: fixed_target.clone(),
                rect: auto_split_cells(
                    live.iter()
                        .find(|m| m.device_name == "SIDE1")
                        .unwrap()
                        .work_area_px,
                    AutoSplit::TwoColumns
                )[0]
            }
        );
        for workset in &autos {
            if let ParkAssignment::AutoSlot { slot, .. } = &result.assignments[&workset.id] {
                assert_ne!(
                    slot, &fixed_target,
                    "auto slot never collides with the fixed cell"
                );
            }
        }
    }

    #[test]
    fn fixed_slot_monitor_missing_is_minimized() {
        let live = vec![monitor("MAIN", 0, 1920)];
        let fixed_slot_id = Uuid::new_v4();
        let fixed_slots = vec![FixedParkingSlot {
            id: fixed_slot_id,
            monitor_id: "GONE".to_string(),
            grid: AutoSplit::One,
            cell_index: 0,
            assigned_workset_id: Uuid::nil(),
        }];
        let workset = fixed_workset(0, fixed_slot_id);
        let worksets = vec![workset.clone()];

        let result = allocate_parking(&AllocationInput {
            worksets: &worksets,
            current_workset_id: None,
            fixed_slots: &fixed_slots,
            main_monitor_ids: &["MAIN".to_string()],
            live_monitors: &live,
            saved_monitors: &[],
            sub_screen_monitor_ids: &[],
            previous_assignments: &HashMap::new(),
            worksets_with_windows: &all_ids(&worksets),
        });

        assert_eq!(result.assignments[&workset.id], ParkAssignment::Minimized);
    }

    #[test]
    fn stable_previous_assignment_is_kept_over_a_different_first_fit_choice() {
        let live = vec![monitor("MAIN", 0, 1920), monitor("SIDE", 1920, 1920)];
        let saved = vec![saved_monitor("SIDE", AutoSplit::TwoColumns)];
        let a = auto_workset(0);
        let b = auto_workset(1);
        let worksets = vec![a.clone(), b.clone()];
        let main_monitor_ids = vec!["MAIN".to_string()];

        // `b` previously held cell 0 — the cell plain First-Fit (which visits
        // worksets in `sort_order`, so `a` before `b`) would otherwise hand to
        // `a`. Stability must override that and keep `b` on cell 0, pushing
        // `a` onto cell 1 instead.
        let mut previous = HashMap::new();
        previous.insert(
            b.id,
            ParkingSlotId {
                monitor_id: "SIDE".to_string(),
                cell_index: 0,
            },
        );

        let result = allocate_parking(&AllocationInput {
            worksets: &worksets,
            current_workset_id: None,
            fixed_slots: &[],
            main_monitor_ids: &main_monitor_ids,
            live_monitors: &live,
            saved_monitors: &saved,
            sub_screen_monitor_ids: &[],
            previous_assignments: &previous,
            worksets_with_windows: &all_ids(&worksets),
        });

        let side_cells = auto_split_cells(
            live.iter()
                .find(|m| m.device_name == "SIDE")
                .unwrap()
                .work_area_px,
            AutoSplit::TwoColumns,
        );
        assert_eq!(
            result.assignments[&b.id],
            ParkAssignment::AutoSlot {
                slot: ParkingSlotId {
                    monitor_id: "SIDE".to_string(),
                    cell_index: 0
                },
                rect: side_cells[0]
            }
        );
        assert_eq!(
            result.assignments[&a.id],
            ParkAssignment::AutoSlot {
                slot: ParkingSlotId {
                    monitor_id: "SIDE".to_string(),
                    cell_index: 1
                },
                rect: side_cells[1]
            }
        );
    }

    #[test]
    fn parking_slot_id_encode_decode_round_trips() {
        let id = ParkingSlotId {
            monitor_id: r"\\.\DISPLAY1".to_string(),
            cell_index: 3,
        };
        let encoded = id.encode();
        assert_eq!(ParkingSlotId::decode(&encoded), Some(id));
    }
}
