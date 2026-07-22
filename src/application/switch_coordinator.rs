//! Orchestrates a workset switch end-to-end (PLAN.md §3.8), and the
//! emergency "recover all windows" operation (PLAN.md §10.2).
//!
//! Implemented as a synchronous, directly-callable API guarded by a simple
//! exclusive-lock flag (§3.8 step 1) rather than a dedicated OS thread: the
//! only real cross-thread callers (the Hotkey thread, the Quick Switcher UI)
//! don't exist until Phase 7, so the thread/channel wiring described in
//! PLAN.md §9.1 is deferred until there is a real caller that needs it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use uuid::Uuid;

use crate::application::main_placement::resolve_main_restore;
use crate::application::parking_allocator::{
    AllocationInput, ParkAssignment, ParkingSlotId, allocate_parking,
};
use crate::application::parking_placement::{ParkPlan, plan_park_into_slot};
use crate::application::recovery_service::{self, RecoveryReport};
use crate::application::window_ops::WindowOps;
use crate::application::workset_service;
use crate::domain::config::SubScreen;
use crate::domain::monitor::SavedMonitor;
use crate::domain::placement::{PixelRect, SavedShowState, bounding_rect};
use crate::domain::workset::{FixedParkingSlot, ManagedWindow, ParkingPolicy, Workset};
use crate::persistence::clock;
use crate::persistence::journal_store::{
    self, JournalStatus, JournalStoreError, JournalWindowEntry, JournalWindowState, SwitchJournal,
};
use crate::persistence::runtime_store::{self, RuntimeStoreError};
use crate::windowing::enumerate::TopLevelWindow;
use crate::windowing::matcher::MatchDecision;
use crate::windowing::monitor::MonitorInfo;

pub struct SwitchCoordinator<W: WindowOps> {
    window_ops: W,
    data_dir: PathBuf,
    switching: AtomicBool,
}

pub struct SwitchRequest<'a> {
    pub worksets: &'a [Workset],
    pub fixed_slots: &'a [FixedParkingSlot],
    pub sub_screens: &'a [SubScreen],
    pub saved_monitors: &'a [SavedMonitor],
    pub main_monitor_ids: &'a [String],
    pub live_monitors: &'a [MonitorInfo],
    pub live_windows: &'a [TopLevelWindow],
    pub target_workset_id: Uuid,
}

#[derive(Debug, Clone)]
pub struct SwitchOutcome {
    pub transaction_id: Uuid,
    pub new_current_workset_id: Uuid,
    pub focused_hwnd: Option<isize>,
}

#[derive(Debug, thiserror::Error)]
pub enum SwitchError {
    #[error("a switch is already in progress")]
    AlreadyInProgress,
    #[error("target workset {0} was not found")]
    TargetNotFound(Uuid),
    #[error("no live main monitor is available")]
    NoMainMonitor,
    #[error("switch journal I/O failed: {0}")]
    Journal(#[from] JournalStoreError),
    #[error("failed to capture pre-switch state for hwnd {hwnd}")]
    CaptureFailed { hwnd: isize },
    #[error("failed to persist runtime state: {0}")]
    Runtime(#[from] RuntimeStoreError),
    #[error("switch failed ({reason}) and was rolled back to the pre-switch layout")]
    RolledBack {
        reason: String,
        unrecoverable_hwnds: Vec<isize>,
    },
    #[error("switch failed ({reason}) and rollback ALSO could not restore every window")]
    RollbackFailed {
        reason: String,
        unrecoverable_hwnds: Vec<isize>,
    },
}

/// Releases `flag` back to `false` when dropped, so every early `?` return
/// from `switch_to` still releases the exclusive lock.
struct SwitchGuard<'a>(&'a AtomicBool);

impl Drop for SwitchGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A `ManagedWindow` successfully re-resolved to a live hwnd this switch.
struct ResolvedWindow<'a> {
    managed: &'a ManagedWindow,
    hwnd: isize,
    process_id: u32,
}

impl<W: WindowOps> SwitchCoordinator<W> {
    pub fn new(window_ops: W, data_dir: PathBuf) -> Self {
        Self {
            window_ops,
            data_dir,
            switching: AtomicBool::new(false),
        }
    }

    /// Test-only seam: forces the exclusive-switch lock held, so a test can
    /// exercise `switch_to`'s `AlreadyInProgress` path and
    /// `recover_all_windows`'s "always available" guarantee without needing
    /// real concurrency.
    #[cfg(test)]
    pub(crate) fn force_lock_for_test(&self) {
        self.switching.store(true, Ordering::Release);
    }

    /// PLAN.md §3.8, steps 1-12.
    pub fn switch_to(&self, request: SwitchRequest) -> Result<SwitchOutcome, SwitchError> {
        if self.switching.swap(true, Ordering::Acquire) {
            return Err(SwitchError::AlreadyInProgress); // step 1
        }
        let _guard = SwitchGuard(&self.switching);

        let Some(target) = request
            .worksets
            .iter()
            .find(|w| w.id == request.target_workset_id)
        else {
            return Err(SwitchError::TargetNotFound(request.target_workset_id));
        };

        let mut runtime = runtime_store::load(&self.data_dir);
        // Step 3: resolve, preferring each window's tracked session HWND binding
        // over volatile title/URL matching, then learn/refresh those bindings.
        let decisions = workset_service::resolve_all_matches_with_bindings(
            request.worksets,
            request.live_windows,
            &runtime.window_bindings,
        );
        for (id, hwnd) in workset_service::bindings_from_decisions(&decisions) {
            runtime.window_bindings.insert(id, hwnd);
        }

        // Step 2: switching to the already-current workset is a no-op focus.
        if runtime.current_workset_id == Some(target.id) {
            let resolved = resolved_windows(target, &decisions, request.live_windows);
            let focused_hwnd = frontmost(&resolved).map(|w| w.hwnd);
            if let Some(hwnd) = focused_hwnd {
                self.window_ops.set_foreground(hwnd);
            }
            return Ok(SwitchOutcome {
                transaction_id: Uuid::new_v4(),
                new_current_workset_id: target.id,
                focused_hwnd,
            });
        }

        let current = runtime
            .current_workset_id
            .and_then(|id| request.worksets.iter().find(|w| w.id == id));
        let current_resolved = current
            .map(|w| resolved_windows(w, &decisions, request.live_windows))
            .unwrap_or_default();
        let target_resolved = resolved_windows(target, &decisions, request.live_windows);

        // Step 4: journal every affected window's pre-switch placement.
        let transaction_id = Uuid::new_v4();
        let mut journal_windows = Vec::new();
        for resolved in current_resolved.iter().chain(target_resolved.iter()) {
            journal_windows.push(capture_journal_entry(&self.window_ops, resolved)?);
        }
        let journal = SwitchJournal {
            schema_version: journal_store::CURRENT_SCHEMA_VERSION,
            transaction_id,
            status: JournalStatus::Started,
            from_workset_id: runtime.current_workset_id,
            to_workset_id: target.id,
            created_at: clock::now_rfc3339(),
            windows: journal_windows,
        };
        journal_store::save(&self.data_dir, &journal)?;

        // Step 5: park the outgoing current workset, if there is one.
        if let Some(current) = current
            && let Err(reason) = self.park_workset(
                current,
                &current_resolved,
                &request,
                &mut runtime.auto_slot_assignments,
            )
        {
            return Err(self.rollback(journal, reason));
        }

        // Step 6: restore the target workset to its main placement.
        let mut outcomes = Vec::with_capacity(target_resolved.len());
        for resolved in &target_resolved {
            match resolve_main_restore(
                &resolved.managed.main_placement,
                request.live_monitors,
                request.main_monitor_ids,
            ) {
                Some(outcome) => outcomes.push(outcome),
                None => {
                    return Err(self.rollback(journal, "no live main monitor".to_string()));
                }
            }
        }
        for resolved in &target_resolved {
            self.window_ops.restore(resolved.hwnd);
        }
        let moves: Vec<(isize, crate::domain::placement::PixelRect)> = target_resolved
            .iter()
            .zip(&outcomes)
            .map(|(resolved, outcome)| (resolved.hwnd, outcome.rect))
            .collect();
        if let Err(err) = self.window_ops.batch_move(&moves) {
            return Err(self.rollback(journal, err.to_string()));
        }
        for (resolved, outcome) in target_resolved.iter().zip(&outcomes) {
            if outcome.show_state == SavedShowState::Maximized {
                self.window_ops.maximize(resolved.hwnd);
            }
        }

        // Step 7: restore Z-order (ascending `z_order`, frontmost last).
        let mut z_ordered: Vec<&ResolvedWindow> = target_resolved.iter().collect();
        z_ordered.sort_by_key(|w| w.managed.z_order);
        for pair in z_ordered.windows(2) {
            self.window_ops
                .set_z_order_after(pair[1].hwnd, Some(pair[0].hwnd));
        }

        // Step 8: best-effort focus the topmost window.
        let focused_hwnd = z_ordered.last().map(|w| w.hwnd);
        if let Some(hwnd) = focused_hwnd {
            self.window_ops.set_foreground(hwnd);
        }

        // Step 9: update current workset. Step 11: persist, then clear the
        // journal — in that order, so a crash between the two leaves a stale
        // `Started` journal whose `to_workset_id` already matches
        // `runtime.current_workset_id`, an easy "already succeeded" case for
        // a future crash-recovery pass (Phase 9) to special-case, rather than
        // the reverse (screen already switched, but runtime.json still
        // pointing at the old workset).
        runtime.current_workset_id = Some(target.id);
        runtime_store::save(&self.data_dir, &runtime)?;
        journal_store::clear(&self.data_dir)?;
        // Step 10 (ready-confirmed) is a no-op placeholder until Phase 8 hooks
        // agent-status confirmation here.

        Ok(SwitchOutcome {
            transaction_id,
            new_current_workset_id: target.id,
            focused_hwnd,
        }) // step 12: this return value is the notification.
    }

    /// PLAN.md §10.2. Deliberately does not take `self.switching` — recovery
    /// must always be callable, even if the switch lock is somehow stuck.
    pub fn recover_all_windows(
        &self,
        worksets: &[Workset],
        live_monitors: &[MonitorInfo],
        main_monitor_ids: &[String],
        live_windows: &[TopLevelWindow],
    ) -> Result<RecoveryReport, RuntimeStoreError> {
        let report = recovery_service::recover_all_windows(
            &self.window_ops,
            worksets,
            live_windows,
            live_monitors,
            main_monitor_ids,
        );
        let mut runtime = runtime_store::load(&self.data_dir);
        runtime.current_workset_id = None;
        runtime_store::save(&self.data_dir, &runtime)?;
        Ok(report)
    }

    /// PLAN.md §4.3/§4.4: decides and applies where `workset` parks.
    /// Updates `auto_slot_assignments` in place with the allocator's fresh
    /// result on success.
    fn park_workset(
        &self,
        workset: &Workset,
        resolved: &[ResolvedWindow],
        request: &SwitchRequest,
        auto_slot_assignments: &mut HashMap<String, String>,
    ) -> Result<(), String> {
        // Sub-screen policy bypasses the auto-cell allocator: the workset parks
        // onto its named area's monitors (the union work-area rect), shrunk to
        // preserve its main-screen relative layout.
        if let ParkingPolicy::SubScreen { sub_screen_id } = &workset.parking_policy {
            let target = request
                .sub_screens
                .iter()
                .find(|s| s.id == *sub_screen_id)
                .and_then(|s| sub_screen_target_rect(s, request.live_monitors));
            return match target {
                Some(rect) => self.place_workset_into_rect(workset, resolved, request, rect),
                None => {
                    for w in resolved {
                        self.window_ops.minimize(w.hwnd);
                    }
                    Ok(())
                }
            };
        }

        let previous = decode_assignments(auto_slot_assignments);
        let allocation = allocate_parking(&AllocationInput {
            worksets: request.worksets,
            current_workset_id: Some(request.target_workset_id),
            fixed_slots: request.fixed_slots,
            main_monitor_ids: request.main_monitor_ids,
            live_monitors: request.live_monitors,
            saved_monitors: request.saved_monitors,
            previous_assignments: &previous,
        });
        *auto_slot_assignments = encode_assignments(&allocation.new_auto_slot_assignments);

        let assignment = allocation
            .assignments
            .get(&workset.id)
            .cloned()
            .unwrap_or(ParkAssignment::Minimized);

        match assignment {
            ParkAssignment::Minimized => {
                for w in resolved {
                    self.window_ops.minimize(w.hwnd);
                }
                Ok(())
            }
            ParkAssignment::AutoSlot { rect, .. } | ParkAssignment::FixedSlot { rect, .. } => {
                self.place_workset_into_rect(workset, resolved, request, rect)
            }
        }
    }

    /// Places `resolved` into `target_rect`, shrinking to preserve the workset's
    /// main-screen relative layout (via `plan_park_into_slot`), then maximizing
    /// if `fullscreen_when_parked` is set. Shared by auto/fixed cells and
    /// sub-screen areas.
    fn place_workset_into_rect(
        &self,
        workset: &Workset,
        resolved: &[ResolvedWindow],
        request: &SwitchRequest,
        target_rect: PixelRect,
    ) -> Result<(), String> {
        let mut main_rects = Vec::with_capacity(resolved.len());
        for w in resolved {
            let outcome = resolve_main_restore(
                &w.managed.main_placement,
                request.live_monitors,
                request.main_monitor_ids,
            )
            .ok_or_else(|| "no live main monitor for parking source bounds".to_string())?;
            main_rects.push(outcome.rect);
        }
        match plan_park_into_slot(&main_rects, target_rect) {
            ParkPlan::MinimizeWhole => {
                for w in resolved {
                    self.window_ops.minimize(w.hwnd);
                }
                Ok(())
            }
            ParkPlan::ShrinkToFit(rects) => {
                for w in resolved {
                    self.window_ops.restore(w.hwnd);
                }
                let moves: Vec<(isize, PixelRect)> = resolved
                    .iter()
                    .zip(rects)
                    .map(|(w, rect)| (w.hwnd, rect))
                    .collect();
                self.window_ops
                    .batch_move(&moves)
                    .map_err(|e| e.to_string())?;
                // "退避後に全画面表示": maximize each window on the parking monitor
                // it was just placed on (PLAN.md §2.4 extension).
                if workset.fullscreen_when_parked {
                    for w in resolved {
                        self.window_ops.maximize(w.hwnd);
                    }
                }
                Ok(())
            }
        }
    }

    /// PLAN.md §3.8 failure path: restores every journaled window to its
    /// pre-switch placement. `current_workset_id` is never touched here.
    fn rollback(&self, mut journal: SwitchJournal, reason: String) -> SwitchError {
        let mut unrecoverable = Vec::new();

        for entry in &journal.windows {
            if !self.window_ops.is_window_alive(entry.hwnd) {
                unrecoverable.push(entry.hwnd);
                continue;
            }
            self.window_ops.restore(entry.hwnd);
            if self
                .window_ops
                .batch_move(&[(entry.hwnd, entry.before.rect)])
                .is_err()
            {
                unrecoverable.push(entry.hwnd);
                continue;
            }
            match entry.before.show_state {
                SavedShowState::Maximized => self.window_ops.maximize(entry.hwnd),
                SavedShowState::Minimized => self.window_ops.minimize(entry.hwnd),
                SavedShowState::Normal => {}
            }
        }

        journal.status = JournalStatus::RolledBack;
        let _ = journal_store::save(&self.data_dir, &journal);

        if unrecoverable.is_empty() {
            let _ = journal_store::clear(&self.data_dir);
            SwitchError::RolledBack {
                reason,
                unrecoverable_hwnds: unrecoverable,
            }
        } else {
            SwitchError::RollbackFailed {
                reason,
                unrecoverable_hwnds: unrecoverable,
            }
        }
    }
}

fn resolved_windows<'a>(
    workset: &'a Workset,
    decisions: &HashMap<Uuid, MatchDecision>,
    live_windows: &[TopLevelWindow],
) -> Vec<ResolvedWindow<'a>> {
    workset
        .windows
        .iter()
        .filter_map(|managed| match decisions.get(&managed.id) {
            Some(MatchDecision::AutoRebind { hwnd }) => {
                let process_id = live_windows
                    .iter()
                    .find(|w| w.hwnd == *hwnd)
                    .map(|w| w.process_id)
                    .unwrap_or(0);
                Some(ResolvedWindow {
                    managed,
                    hwnd: *hwnd,
                    process_id,
                })
            }
            _ => None,
        })
        .collect()
}

/// The highest-`z_order` resolved window, treated as the intended frontmost
/// window within its own workset (PLAN.md §3.8 step 8; `z_order`'s exact
/// front/back convention isn't specified beyond this).
fn frontmost<'a, 'b>(resolved: &'a [ResolvedWindow<'b>]) -> Option<&'a ResolvedWindow<'b>> {
    resolved.iter().max_by_key(|w| w.managed.z_order)
}

fn capture_journal_entry<W: WindowOps>(
    window_ops: &W,
    resolved: &ResolvedWindow,
) -> Result<JournalWindowEntry, SwitchError> {
    let rect =
        window_ops
            .get_normal_rect(resolved.hwnd)
            .map_err(|_| SwitchError::CaptureFailed {
                hwnd: resolved.hwnd,
            })?;
    let show_state =
        window_ops
            .get_show_state(resolved.hwnd)
            .map_err(|_| SwitchError::CaptureFailed {
                hwnd: resolved.hwnd,
            })?;
    Ok(JournalWindowEntry {
        managed_window_id: resolved.managed.id,
        hwnd: resolved.hwnd,
        process_id: resolved.process_id,
        before: JournalWindowState { rect, show_state },
    })
}

fn decode_assignments(map: &HashMap<String, String>) -> HashMap<Uuid, ParkingSlotId> {
    map.iter()
        .filter_map(|(id, slot)| Some((Uuid::parse_str(id).ok()?, ParkingSlotId::decode(slot)?)))
        .collect()
}

fn encode_assignments(map: &HashMap<Uuid, ParkingSlotId>) -> HashMap<String, String> {
    map.iter()
        .map(|(id, slot)| (id.to_string(), slot.encode()))
        .collect()
}

/// The union work-area rect of a sub-screen's currently-live monitors, or
/// `None` if none of them are connected (→ the workset is minimized instead).
fn sub_screen_target_rect(sub: &SubScreen, live_monitors: &[MonitorInfo]) -> Option<PixelRect> {
    let work_areas: Vec<PixelRect> = sub
        .monitor_ids
        .iter()
        .filter_map(|id| {
            live_monitors
                .iter()
                .find(|m| &m.device_name == id)
                .map(|m| m.work_area_px)
        })
        .collect();
    bounding_rect(&work_areas)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::application::window_ops::fake::FakeWindowOps;
    use crate::domain::monitor::AutoSplit;
    use crate::domain::placement::{NormalizedRect, PixelRect, SavedPlacement};
    use crate::domain::workset::{ParkingPolicy, RepositoryKind, WindowMatcher};

    fn matcher_for(exe: &str, title: &str) -> WindowMatcher {
        WindowMatcher {
            executable_path: exe.into(),
            process_name: exe.to_string(),
            window_class: format!("{exe}-class"),
            registered_title: title.to_string(),
            title_contains: None,
            title_regex: None,
        }
    }

    fn managed_window(
        exe: &str,
        main_monitor_index: usize,
        rect: NormalizedRect,
        show_state: SavedShowState,
        z_order: i32,
    ) -> ManagedWindow {
        ManagedWindow {
            id: Uuid::new_v4(),
            matcher: matcher_for(exe, exe),
            main_placement: SavedPlacement {
                monitor_id: "MAIN".to_string(),
                main_monitor_index,
                normalized_rect: rect,
                physical_rect_at_capture: PixelRect::new(0, 0, 800, 600),
                show_state,
            },
            z_order,
            launch_spec: None,
        }
    }

    fn workset(
        sort_order: i32,
        parking_policy: ParkingPolicy,
        windows: Vec<ManagedWindow>,
    ) -> Workset {
        Workset {
            id: Uuid::new_v4(),
            name: format!("workset-{sort_order}"),
            repository_path: "C:\\repo".into(),
            repository_kind: RepositoryKind::Git,
            color: "#000000".to_string(),
            sort_order,
            direct_hotkey: None,
            parking_policy,
            fullscreen_when_parked: false,
            windows,
            created_at: "2026-07-20T00:00:00Z".to_string(),
            updated_at: "2026-07-20T00:00:00Z".to_string(),
        }
    }

    fn live_window(hwnd: isize, exe: &str) -> TopLevelWindow {
        TopLevelWindow {
            hwnd,
            process_id: hwnd as u32,
            executable_path: Some(exe.into()),
            window_class: format!("{exe}-class"),
            title: exe.to_string(),
            rect_px: PixelRect::new(0, 0, 800, 600),
        }
    }

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

    fn on_some_monitor(rect: PixelRect, monitors: &[PixelRect]) -> bool {
        monitors.iter().any(|m| {
            rect.x >= m.x
                && rect.right() <= m.right()
                && rect.y >= m.y
                && rect.bottom() <= m.bottom()
        })
    }

    fn assert_never_offscreen(fake: &FakeWindowOps, hwnd: isize, monitors: &[PixelRect]) {
        if fake.show_state_of(hwnd) == Some(SavedShowState::Minimized) {
            return;
        }
        let rect = fake.rect_of(hwnd).expect("window exists");
        assert!(
            on_some_monitor(rect, monitors),
            "hwnd {hwnd} rect {rect:?} is not fully within any monitor"
        );
    }

    #[test]
    fn switching_between_three_worksets_100_times_leaves_nothing_offscreen() {
        let dir = tempdir().unwrap();
        let main_rect = NormalizedRect {
            x: 0.1,
            y: 0.1,
            width: 0.3,
            height: 0.3,
        };
        let a = workset(
            0,
            ParkingPolicy::Auto,
            vec![managed_window(
                "app-a",
                0,
                main_rect,
                SavedShowState::Normal,
                0,
            )],
        );
        let b = workset(
            1,
            ParkingPolicy::Auto,
            vec![managed_window(
                "app-b",
                0,
                main_rect,
                SavedShowState::Normal,
                0,
            )],
        );
        let c = workset(
            2,
            ParkingPolicy::Auto,
            vec![managed_window(
                "app-c",
                0,
                main_rect,
                SavedShowState::Normal,
                0,
            )],
        );
        let worksets = vec![a.clone(), b.clone(), c.clone()];

        let live_windows = vec![
            live_window(1, "app-a"),
            live_window(2, "app-b"),
            live_window(3, "app-c"),
        ];
        let main = monitor("MAIN", 0);
        let side = monitor("SIDE", 1920);
        let live_monitors = vec![main.clone(), side.clone()];
        let saved_monitors = vec![crate::domain::monitor::SavedMonitor {
            stable_id: "SIDE".to_string(),
            device_name: "SIDE".to_string(),
            device_path: None,
            friendly_name: None,
            bounds_px: side.bounds_px,
            work_area_px: side.work_area_px,
            dpi_x: 96,
            dpi_y: 96,
            auto_split: Some(AutoSplit::One),
            excluded: false,
        }];
        let main_monitor_ids = vec!["MAIN".to_string()];

        let fake = FakeWindowOps::new();
        for hwnd in [1isize, 2, 3] {
            fake.seed_window(
                hwnd,
                PixelRect::new(100, 100, 400, 300),
                SavedShowState::Normal,
            );
        }
        let coordinator = SwitchCoordinator::new(fake, dir.path().to_path_buf());

        let ids = [a.id, b.id, c.id];
        for i in 0..100 {
            let target = ids[i % 3];
            coordinator
                .switch_to(SwitchRequest {
                    worksets: &worksets,
                    fixed_slots: &[],
                    sub_screens: &[],
                    saved_monitors: &saved_monitors,
                    main_monitor_ids: &main_monitor_ids,
                    live_monitors: &live_monitors,
                    live_windows: &live_windows,
                    target_workset_id: target,
                })
                .unwrap_or_else(|e| panic!("switch {i} to {target} failed: {e}"));

            for hwnd in [1isize, 2, 3] {
                assert_never_offscreen(
                    &coordinator.window_ops,
                    hwnd,
                    &[main.work_area_px, side.work_area_px],
                );
            }
        }
    }

    #[test]
    fn batch_move_fallback_recovers_from_a_simulated_batch_failure() {
        let dir = tempdir().unwrap();
        let rect = NormalizedRect {
            x: 0.1,
            y: 0.1,
            width: 0.3,
            height: 0.3,
        };
        let a = workset(
            0,
            ParkingPolicy::Auto,
            vec![managed_window("app-a", 0, rect, SavedShowState::Normal, 0)],
        );
        let worksets = vec![a.clone()];
        let live_windows = vec![live_window(1, "app-a")];
        let main = monitor("MAIN", 0);
        let live_monitors = vec![main.clone()];
        let main_monitor_ids = vec!["MAIN".to_string()];

        let fake = FakeWindowOps::new();
        fake.seed_window(1, PixelRect::new(0, 0, 100, 100), SavedShowState::Normal);
        fake.fail_next_batch_move();
        let coordinator = SwitchCoordinator::new(fake, dir.path().to_path_buf());

        let outcome = coordinator
            .switch_to(SwitchRequest {
                worksets: &worksets,
                fixed_slots: &[],
                sub_screens: &[],
                saved_monitors: &[],
                main_monitor_ids: &main_monitor_ids,
                live_monitors: &live_monitors,
                live_windows: &live_windows,
                target_workset_id: a.id,
            })
            .expect("fallback should recover from the simulated batch failure");

        assert_eq!(outcome.new_current_workset_id, a.id);
        let final_rect = coordinator.window_ops.rect_of(1).unwrap();
        assert!(on_some_monitor(final_rect, &[main.work_area_px]));
    }

    #[test]
    fn successful_switch_focuses_the_frontmost_window() {
        let dir = tempdir().unwrap();
        let rect = NormalizedRect {
            x: 0.1,
            y: 0.1,
            width: 0.2,
            height: 0.2,
        };
        let a = workset(
            0,
            ParkingPolicy::Auto,
            vec![
                managed_window("app-a1", 0, rect, SavedShowState::Normal, 0),
                managed_window("app-a2", 0, rect, SavedShowState::Normal, 5), // higher z_order = frontmost
            ],
        );
        let worksets = vec![a.clone()];
        let live_windows = vec![live_window(1, "app-a1"), live_window(2, "app-a2")];
        let main = monitor("MAIN", 0);
        let live_monitors = vec![main.clone()];
        let main_monitor_ids = vec!["MAIN".to_string()];

        let fake = FakeWindowOps::new();
        fake.seed_window(1, PixelRect::new(0, 0, 100, 100), SavedShowState::Normal);
        fake.seed_window(2, PixelRect::new(0, 0, 100, 100), SavedShowState::Normal);
        let coordinator = SwitchCoordinator::new(fake, dir.path().to_path_buf());

        let outcome = coordinator
            .switch_to(SwitchRequest {
                worksets: &worksets,
                fixed_slots: &[],
                sub_screens: &[],
                saved_monitors: &[],
                main_monitor_ids: &main_monitor_ids,
                live_monitors: &live_monitors,
                live_windows: &live_windows,
                target_workset_id: a.id,
            })
            .unwrap();

        assert_eq!(outcome.focused_hwnd, Some(2));
        assert_eq!(coordinator.window_ops.foreground_history(), vec![2]);
    }

    #[test]
    fn per_window_fallback_failure_on_a_live_window_still_fully_rolls_back() {
        let dir = tempdir().unwrap();
        let rect = NormalizedRect {
            x: 0.1,
            y: 0.1,
            width: 0.2,
            height: 0.2,
        };
        let a = workset(
            0,
            ParkingPolicy::Auto,
            vec![managed_window("app-a", 0, rect, SavedShowState::Normal, 0)],
        );
        let worksets = vec![a.clone()];
        let live_windows = vec![live_window(1, "app-a")];
        let main = monitor("MAIN", 0);
        let live_monitors = vec![main.clone()];
        let main_monitor_ids = vec!["MAIN".to_string()];

        let fake = FakeWindowOps::new();
        let original_rect = PixelRect::new(10, 10, 200, 150);
        fake.seed_window(1, original_rect, SavedShowState::Normal);
        // Simulate `EndDeferWindowPos` failing AND the per-window
        // `SetWindowPos` fallback also failing for this (still alive) window.
        fake.fail_next_batch_move();
        fake.fail_per_window_fallback_for(1);
        let coordinator = SwitchCoordinator::new(fake, dir.path().to_path_buf());

        let result = coordinator.switch_to(SwitchRequest {
            worksets: &worksets,
            fixed_slots: &[],
            sub_screens: &[],
            saved_monitors: &[],
            main_monitor_ids: &main_monitor_ids,
            live_monitors: &live_monitors,
            live_windows: &live_windows,
            target_workset_id: a.id,
        });

        match result {
            Err(SwitchError::RolledBack {
                unrecoverable_hwnds,
                ..
            }) => {
                assert!(
                    unrecoverable_hwnds.is_empty(),
                    "the window is alive, so rollback itself must succeed"
                );
            }
            other => panic!("expected RolledBack, got {other:?}"),
        }
        assert_eq!(coordinator.window_ops.rect_of(1), Some(original_rect));
    }

    #[test]
    fn window_dying_mid_switch_rolls_back_the_surviving_window() {
        let dir = tempdir().unwrap();
        let rect = NormalizedRect {
            x: 0.1,
            y: 0.1,
            width: 0.2,
            height: 0.2,
        };
        let a = workset(
            0,
            ParkingPolicy::Auto,
            vec![managed_window("app-a", 0, rect, SavedShowState::Normal, 0)],
        );
        let b = workset(
            1,
            ParkingPolicy::Auto,
            vec![
                managed_window("app-b1", 0, rect, SavedShowState::Normal, 0),
                managed_window("app-b2", 0, rect, SavedShowState::Normal, 1),
            ],
        );
        let worksets = vec![a.clone(), b.clone()];
        let live_windows = vec![
            live_window(1, "app-a"),
            live_window(2, "app-b1"),
            live_window(3, "app-b2"),
        ];
        let main = monitor("MAIN", 0);
        let live_monitors = vec![main.clone()]; // no non-main monitor: `a` will be minimized when parked.
        let main_monitor_ids = vec!["MAIN".to_string()];

        let fake = FakeWindowOps::new();
        let a_original_rect = PixelRect::new(10, 10, 200, 150);
        let b1_original_rect = PixelRect::new(300, 10, 200, 150);
        fake.seed_window(1, a_original_rect, SavedShowState::Normal);
        fake.seed_window(2, b1_original_rect, SavedShowState::Normal);
        fake.seed_window(3, PixelRect::new(500, 10, 200, 150), SavedShowState::Normal);

        // First, make `a` the current workset with no failures injected.
        let coordinator = SwitchCoordinator::new(fake, dir.path().to_path_buf());
        coordinator
            .switch_to(SwitchRequest {
                worksets: &worksets,
                fixed_slots: &[],
                sub_screens: &[],
                saved_monitors: &[],
                main_monitor_ids: &main_monitor_ids,
                live_monitors: &live_monitors,
                live_windows: &live_windows,
                target_workset_id: a.id,
            })
            .unwrap();
        // `a`'s pre-switch-2 state: restored onto the main screen by switch 1
        // above, not its original fake-seeded position — rollback must
        // restore to *this*, the state right before the failing switch.
        let a_rect_before_switch_2 = coordinator.window_ops.rect_of(1).unwrap();

        // Now switch to `b`, but `app-b2` dies mid-switch and the atomic
        // batch move is forced to fall back to the per-window path, where it
        // discovers the dead window and fails.
        coordinator.window_ops.kill_window(3);
        coordinator.window_ops.fail_next_batch_move();

        let result = coordinator.switch_to(SwitchRequest {
            worksets: &worksets,
            fixed_slots: &[],
            sub_screens: &[],
            saved_monitors: &[],
            main_monitor_ids: &main_monitor_ids,
            live_monitors: &live_monitors,
            live_windows: &live_windows,
            target_workset_id: b.id,
        });

        match result {
            Err(SwitchError::RollbackFailed {
                unrecoverable_hwnds,
                ..
            }) => {
                assert_eq!(unrecoverable_hwnds, vec![3]);
            }
            other => panic!("expected RollbackFailed, got {other:?}"),
        }

        // `a` (minimized then rolled back) and `b1` (moved then rolled back)
        // both end up restored to their pre-switch-2 state.
        assert_eq!(
            coordinator.window_ops.show_state_of(1),
            Some(SavedShowState::Normal)
        );
        assert_eq!(
            coordinator.window_ops.rect_of(1),
            Some(a_rect_before_switch_2)
        );
        assert_eq!(coordinator.window_ops.rect_of(2), Some(b1_original_rect));
    }

    #[test]
    fn fixed_workset_returns_to_the_same_slot_every_time() {
        let dir = tempdir().unwrap();
        let rect = NormalizedRect {
            x: 0.1,
            y: 0.1,
            width: 0.2,
            height: 0.2,
        };
        let slot_id = Uuid::new_v4();
        let fixed = workset(
            0,
            ParkingPolicy::Fixed { slot_id },
            vec![managed_window(
                "app-fixed",
                0,
                rect,
                SavedShowState::Normal,
                0,
            )],
        );
        let auto = workset(
            1,
            ParkingPolicy::Auto,
            vec![managed_window(
                "app-auto",
                0,
                rect,
                SavedShowState::Normal,
                0,
            )],
        );
        let worksets = vec![fixed.clone(), auto.clone()];
        let fixed_slots = vec![FixedParkingSlot {
            id: slot_id,
            monitor_id: "SIDE".to_string(),
            grid: AutoSplit::TwoColumns,
            cell_index: 0,
            assigned_workset_id: fixed.id,
        }];
        let live_windows = vec![live_window(1, "app-fixed"), live_window(2, "app-auto")];
        let main = monitor("MAIN", 0);
        let side = monitor("SIDE", 1920);
        let live_monitors = vec![main.clone(), side.clone()];
        let saved_monitors = vec![crate::domain::monitor::SavedMonitor {
            stable_id: "SIDE".to_string(),
            device_name: "SIDE".to_string(),
            device_path: None,
            friendly_name: None,
            bounds_px: side.bounds_px,
            work_area_px: side.work_area_px,
            dpi_x: 96,
            dpi_y: 96,
            auto_split: Some(AutoSplit::TwoColumns),
            excluded: false,
        }];
        let main_monitor_ids = vec!["MAIN".to_string()];

        let fake = FakeWindowOps::new();
        fake.seed_window(1, PixelRect::new(0, 0, 400, 300), SavedShowState::Normal);
        fake.seed_window(2, PixelRect::new(0, 0, 400, 300), SavedShowState::Normal);
        let coordinator = SwitchCoordinator::new(fake, dir.path().to_path_buf());

        let request = |target: Uuid| SwitchRequest {
            worksets: &worksets,
            fixed_slots: &fixed_slots,
            sub_screens: &[],
            saved_monitors: &saved_monitors,
            main_monitor_ids: &main_monitor_ids,
            live_monitors: &live_monitors,
            live_windows: &live_windows,
            target_workset_id: target,
        };

        coordinator.switch_to(request(fixed.id)).unwrap(); // fixed becomes current
        coordinator.switch_to(request(auto.id)).unwrap(); // fixed parks at its slot
        let first_park_rect = coordinator.window_ops.rect_of(1).unwrap();

        coordinator.switch_to(request(fixed.id)).unwrap(); // fixed becomes current again
        coordinator.switch_to(request(auto.id)).unwrap(); // fixed parks again
        let second_park_rect = coordinator.window_ops.rect_of(1).unwrap();

        assert_eq!(first_park_rect, second_park_rect);
    }

    #[test]
    fn recover_all_windows_bypasses_the_switch_lock() {
        let dir = tempdir().unwrap();
        let fake = FakeWindowOps::new();
        let coordinator = SwitchCoordinator::new(fake, dir.path().to_path_buf());
        coordinator.force_lock_for_test();

        let report = coordinator
            .recover_all_windows(&[], &[], &[], &[])
            .expect("recovery must always be callable, even with the switch lock held");

        assert!(report.recovered.is_empty());
    }

    #[test]
    fn switch_to_reports_already_in_progress_when_reentered() {
        let dir = tempdir().unwrap();
        let fake = FakeWindowOps::new();
        let coordinator = SwitchCoordinator::new(fake, dir.path().to_path_buf());
        coordinator.force_lock_for_test();

        let result = coordinator.switch_to(SwitchRequest {
            worksets: &[],
            fixed_slots: &[],
            sub_screens: &[],
            saved_monitors: &[],
            main_monitor_ids: &[],
            live_monitors: &[],
            live_windows: &[],
            target_workset_id: Uuid::new_v4(),
        });

        assert!(matches!(result, Err(SwitchError::AlreadyInProgress)));
    }
}
