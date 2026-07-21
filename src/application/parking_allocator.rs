//! Parking-slot identity and the auto-slot allocation algorithm (PLAN.md §3.7, §4.4).

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::application::layout_service::auto_split_cells;
use crate::application::monitor_resolution::{
    find_live_monitor_by_stable_id, sort_monitors_reading_order,
};
use crate::domain::monitor::SavedMonitor;
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
    /// Decoded from `RuntimeState.auto_slot_assignments`.
    pub previous_assignments: &'a HashMap<Uuid, ParkingSlotId>,
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

    // Cell enumeration (§4.4 steps 1-2): non-main, non-excluded monitors,
    // reading order.
    let mut non_main_monitors: Vec<MonitorInfo> = input
        .live_monitors
        .iter()
        .filter(|m| !input.main_monitor_ids.iter().any(|id| id == &m.device_name))
        .filter(|m| {
            !input
                .saved_monitors
                .iter()
                .any(|s| s.stable_id == m.device_name && s.excluded)
        })
        .cloned()
        .collect();
    sort_monitors_reading_order(&mut non_main_monitors);

    let mut ordered_cells: Vec<(ParkingSlotId, PixelRect)> = Vec::new();
    for monitor in &non_main_monitors {
        let split = input
            .saved_monitors
            .iter()
            .find(|s| s.stable_id == monitor.device_name)
            .and_then(|s| s.auto_split)
            .unwrap_or_else(|| {
                crate::application::layout_service::resolve_auto_split(monitor.work_area_px)
            });
        for (cell_index, rect) in auto_split_cells(monitor.work_area_px, split)
            .into_iter()
            .enumerate()
        {
            ordered_cells.push((
                ParkingSlotId {
                    monitor_id: monitor.device_name.clone(),
                    cell_index,
                },
                rect,
            ));
        }
    }

    // Exclude fixed cells (§4.4 step 3), unconditionally.
    let available_cells: Vec<(ParkingSlotId, PixelRect)> = ordered_cells
        .into_iter()
        .filter(|(id, _)| !fixed_cells.contains(id))
        .collect();
    let cell_rects: HashMap<&ParkingSlotId, PixelRect> = available_cells
        .iter()
        .map(|(id, rect)| (id, *rect))
        .collect();

    // Auto-pool worksets (§4.4 step 4), deterministic order.
    let mut auto_pool: Vec<&Workset> = input
        .worksets
        .iter()
        .filter(|w| {
            Some(w.id) != input.current_workset_id && w.parking_policy == ParkingPolicy::Auto
        })
        .collect();
    auto_pool.sort_by_key(|w| (w.sort_order, w.id));

    let mut claimed: HashSet<&ParkingSlotId> = HashSet::new();
    let mut kept: HashMap<Uuid, &ParkingSlotId> = HashMap::new();

    // Stability pass (§4.4 step 5): keep a valid previous assignment.
    for workset in &auto_pool {
        let Some(previous) = input.previous_assignments.get(&workset.id) else {
            continue;
        };
        let Some((slot_id, _)) = available_cells.iter().find(|(id, _)| id == previous) else {
            continue;
        };
        if claimed.insert(slot_id) {
            kept.insert(workset.id, slot_id);
        }
    }

    // First Fit (§4.4 step 6): remaining worksets take the next free cell.
    let mut new_auto_slot_assignments = HashMap::new();
    for workset in &auto_pool {
        let slot_id = if let Some(&kept_slot) = kept.get(&workset.id) {
            Some(kept_slot)
        } else {
            available_cells
                .iter()
                .map(|(id, _)| id)
                .find(|id| claimed.insert(id))
        };

        let assignment = match slot_id {
            Some(id) => {
                let rect = cell_rects[id];
                new_auto_slot_assignments.insert(workset.id, id.clone());
                ParkAssignment::AutoSlot {
                    slot: id.clone(),
                    rect,
                }
            }
            // Overflow (§4.4 step 7): no cell left.
            None => ParkAssignment::Minimized,
        };
        assignments.insert(workset.id, assignment);
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
            previous_assignments: &HashMap::new(),
        });

        assert!(matches!(
            result.assignments[&a.id],
            ParkAssignment::AutoSlot { .. }
        ));
        assert_eq!(result.assignments[&b.id], ParkAssignment::Minimized);
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
            previous_assignments: &HashMap::new(),
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
            previous_assignments: &first.new_auto_slot_assignments,
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
            previous_assignments: &HashMap::new(),
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
            previous_assignments: &HashMap::new(),
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
            previous_assignments: &previous,
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
