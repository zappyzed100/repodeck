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
            // Target set has priority, so a window shared with another set lands
            // on the main screen with the set being switched to.
            Some(request.target_workset_id),
        );
        for (id, hwnd) in workset_service::bindings_from_decisions(&decisions) {
            runtime.window_bindings.insert(id, hwnd);
        }

        // Step 2: switching to the already-current workset is a no-op focus.
        if runtime.current_workset_id == Some(target.id) {
            let resolved = resolved_windows(target, &decisions, request.live_windows);
            let focused_hwnd = frontmost(&resolved).map(|w| w.hwnd);
            tracing::info!(
                target: "switch",
                to = %target.name,
                windows = resolved.len(),
                focus = ?focused_hwnd,
                "switch: already current, focus-only no-op"
            );
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

        // Every non-target workset that has live windows. ALL of them are parked
        // (re-placed) on each switch — not just the outgoing one — so the parking
        // layout always matches the current balanced allocation. Otherwise a set
        // parked by an earlier switch keeps a stale cell and overlaps the freshly
        // parked ones at the wrong size (2026-07-23).
        let parking: Vec<(&Workset, Vec<ResolvedWindow>)> = request
            .worksets
            .iter()
            .filter(|w| w.id != target.id)
            .filter_map(|w| {
                let resolved = resolved_windows(w, &decisions, request.live_windows);
                (!resolved.is_empty()).then_some((w, resolved))
            })
            .collect();

        tracing::info!(
            target: "switch",
            from = current.map(|w| w.name.as_str()).unwrap_or("(none)"),
            to = %target.name,
            current_windows = current_resolved.len(),
            target_windows = target_resolved.len(),
            live_windows = request.live_windows.len(),
            live_monitors = request.live_monitors.len(),
            "switch: begin"
        );
        let title_of = |hwnd: isize| -> String {
            request
                .live_windows
                .iter()
                .find(|lw| lw.hwnd == hwnd)
                .map(|lw| lw.title.clone())
                .unwrap_or_default()
        };
        for w in &current_resolved {
            tracing::info!(
                target: "switch", role = "current(park)", hwnd = w.hwnd,
                managed = %w.managed.id, z = w.managed.z_order, title = %title_of(w.hwnd),
                "switch: resolved window"
            );
        }
        for w in &target_resolved {
            tracing::info!(
                target: "switch", role = "target(main)", hwnd = w.hwnd,
                managed = %w.managed.id, z = w.managed.z_order, title = %title_of(w.hwnd),
                "switch: resolved window"
            );
        }

        // If the outgoing workset parks into a sub-screen cell that a stale
        // window from another set still occupies, that window is evacuated to
        // general parking first (below). Resolve it now so it is journaled too.
        let sub_evictees = self.resolve_sub_evictees(
            current,
            &current_resolved,
            &target_resolved,
            &decisions,
            &request,
        );

        // Step 4: journal every affected window's pre-switch placement.
        let transaction_id = Uuid::new_v4();
        let mut journal_windows = Vec::new();
        for resolved in parking
            .iter()
            .flat_map(|(_, r)| r.iter())
            .chain(target_resolved.iter())
            .chain(sub_evictees.iter())
        {
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

        // Step 5: park the outgoing current workset, if there is one. Any
        // windows that want the browser full-screen keys are sent them at the
        // very end (after the target is focused), so the keys land on the parked
        // video and the foreground still ends up on the new workset.
        // Order (確定 2026-07-23): FIRST bring the target set to the main screen
        // (a brief overlap with the still-parked outgoing set is fine), and only
        // THEN park everything else. The set you asked for appears immediately
        // and the parked windows settle behind it.
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
        // Was the target set parked in browser full-screen (F11/F)? That's not a
        // Win32 maximized state, so plain placement can't shrink it back — those
        // windows get the exit-keys sequence, which owns the whole restore.
        let target_was_fullscreen = match &target.parking_policy {
            ParkingPolicy::SubScreen { sub_screen_id } => {
                let sole = sub_screen_sharer_count(request.worksets, *sub_screen_id) <= 1;
                let sub_fs = request
                    .sub_screens
                    .iter()
                    .find(|s| s.id == *sub_screen_id)
                    .is_some_and(|s| s.fullscreen);
                sole && (target.fullscreen_when_parked || sub_fs)
            }
            _ => target.fullscreen_when_parked,
        };
        // Restore each target window to its saved main rect and show state.
        // If two target windows' saved placements overlap — e.g. a main monitor
        // is gone and several fall back to the same one — tile the target windows
        // across the available main monitors instead, so they never stack
        // (spec §1.2 / 2026-07-23: 「画面の上に一画面」防止).
        let main_screens: Vec<PixelRect> = request
            .main_monitor_ids
            .iter()
            .filter_map(|id| {
                request
                    .live_monitors
                    .iter()
                    .find(|m| &m.device_name == id)
                    .map(|m| m.work_area_px)
            })
            .collect();
        let any_overlap = outcomes.iter().enumerate().any(|(i, o)| {
            outcomes[i + 1..].iter().any(|p| {
                o.rect.x < p.rect.right()
                    && p.rect.x < o.rect.right()
                    && o.rect.y < p.rect.bottom()
                    && p.rect.y < o.rect.bottom()
            })
        });
        let tiled: std::collections::HashMap<isize, PixelRect> =
            if any_overlap && !main_screens.is_empty() && !target_was_fullscreen {
                let hwnds: Vec<isize> = target_resolved.iter().map(|w| w.hwnd).collect();
                distribute_parking(&main_screens, &hwnds).0.into_iter().collect()
            } else {
                std::collections::HashMap::new()
            };

        for (resolved, outcome) in target_resolved.iter().zip(&outcomes) {
            if let Some(cell) = tiled.get(&resolved.hwnd) {
                tracing::info!(
                    target: "switch", hwnd = resolved.hwnd, tiled = ?cell,
                    "switch: main placements overlapped — tiling target across main"
                );
                self.window_ops.set_placement(resolved.hwnd, *cell, false, true);
                continue;
            }
            let maximized = outcome.show_state == SavedShowState::Maximized;
            tracing::info!(
                target: "switch", hwnd = resolved.hwnd, to_rect = ?outcome.rect,
                maximized, exit_fullscreen = target_was_fullscreen,
                "switch: restore target window to main"
            );
            if target_was_fullscreen {
                self.window_ops
                    .exit_fullscreen(resolved.hwnd, outcome.rect, maximized);
            } else {
                self.window_ops
                    .set_placement(resolved.hwnd, outcome.rect, maximized, false);
            }
        }

        // Now park the outgoing set and every other non-target live set.
        let mut fullscreen_hwnds: Vec<isize> = Vec::new();
        let worksets_with_windows: std::collections::HashSet<Uuid> =
            parking.iter().map(|(w, _)| w.id).collect();
        // Clear any stale foreign window out of the outgoing set's sub cell first.
        self.park_evictees(&sub_evictees, &request);
        // Auto sets park by window (spec §2): pool all their windows and
        // distribute them across the general parking monitors together. Sub/Fixed
        // sets park into their designated area per-set. All non-target live sets
        // are re-placed every switch, so nothing lingers at a stale size.
        let mut auto_windows: Vec<isize> = Vec::new();
        for (w, resolved) in &parking {
            if matches!(w.parking_policy, ParkingPolicy::Auto) {
                auto_windows.extend(resolved.iter().map(|r| r.hwnd));
                continue;
            }
            tracing::info!(
                target: "switch", workset = %w.name, windows = resolved.len(),
                policy = ?w.parking_policy, "switch: parking set"
            );
            match self.park_workset(
                w,
                resolved,
                &request,
                &mut runtime.auto_slot_assignments,
                &worksets_with_windows,
            ) {
                Ok(hwnds) => fullscreen_hwnds.extend(hwnds),
                Err(reason) => return Err(self.rollback(journal, reason)),
            }
        }
        self.park_auto_windows(&auto_windows, &request);

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

        // "退避後に全画面表示": now that the target is focused, send the browser's
        // own full-screen keys (F11 then F) to each parked window that wants
        // them, handing the foreground back to the target afterwards.
        for hwnd in fullscreen_hwnds {
            self.window_ops.send_fullscreen_keys(hwnd, focused_hwnd);
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

        tracing::info!(
            target: "switch", to = %target.name, transaction = %transaction_id,
            focused = ?focused_hwnd, "switch: completed"
        );
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
        worksets_with_windows: &std::collections::HashSet<Uuid>,
    ) -> Result<Vec<isize>, String> {
        // Sub-screen policy bypasses the auto-cell allocator: the workset parks
        // into its own non-overlapping cell of the named area. Worksets sharing
        // a sub-screen subdivide it 1→2→4; a 5th+ sharer gets no cell (→ None,
        // minimized), preserving the workset's main-screen relative layout.
        if let ParkingPolicy::SubScreen { sub_screen_id } = &workset.parking_policy {
            let sub = request.sub_screens.iter().find(|s| s.id == *sub_screen_id);
            let target = sub.and_then(|s| {
                sub_screen_slot_rect(
                    s,
                    request.worksets,
                    workset.id,
                    request.live_monitors,
                    worksets_with_windows,
                )
            });
            // Full-screen only when this workset is the sole sharer *parking now*
            // — a sub shared with another currently-parked set can't have
            // overlapping full-screen windows.
            let parked_sharers = request
                .worksets
                .iter()
                .filter(|w| {
                    matches!(&w.parking_policy, ParkingPolicy::SubScreen { sub_screen_id: id } if id == sub_screen_id)
                })
                .filter(|w| worksets_with_windows.contains(&w.id))
                .count();
            let sole_occupant = parked_sharers <= 1;
            let fullscreen = sole_occupant
                && (workset.fullscreen_when_parked || sub.is_some_and(|s| s.fullscreen));
            return match target {
                Some(rect) => self.place_workset_into_rect(resolved, request, rect, fullscreen),
                None => {
                    for w in resolved {
                        self.window_ops.minimize(w.hwnd);
                    }
                    Ok(Vec::new())
                }
            };
        }

        let previous = decode_assignments(auto_slot_assignments);
        let sub_screen_monitor_ids: Vec<String> = request
            .sub_screens
            .iter()
            .flat_map(|s| s.monitor_ids.iter().cloned())
            .collect();
        let allocation = allocate_parking(&AllocationInput {
            worksets: request.worksets,
            current_workset_id: Some(request.target_workset_id),
            fixed_slots: request.fixed_slots,
            main_monitor_ids: request.main_monitor_ids,
            live_monitors: request.live_monitors,
            saved_monitors: request.saved_monitors,
            sub_screen_monitor_ids: &sub_screen_monitor_ids,
            previous_assignments: &previous,
            worksets_with_windows,
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
                Ok(Vec::new())
            }
            ParkAssignment::AutoSlot { rect, .. } | ParkAssignment::FixedSlot { rect, .. } => self
                .place_workset_into_rect(resolved, request, rect, workset.fullscreen_when_parked),
        }
    }

    /// Places `resolved` into `target_rect` by **subdividing the slot among the
    /// workset's windows** (`subdivide_for_count`): each window fills its own
    /// cell, resized to the destination — never stacked, and not shrunk-to-fit
    /// as one bounding box (which produced awkward, uneven sizes on a
    /// differently-sized monitor). A window whose cell would be below the
    /// minimum displayed size, or one beyond the slot's capacity, is minimized.
    /// Returns the hwnds that should be sent the browser full-screen keys
    /// afterwards (empty unless `fullscreen` and the window was actually parked).
    /// Shared by auto/fixed cells and sub-screen areas.
    fn place_workset_into_rect(
        &self,
        resolved: &[ResolvedWindow],
        request: &SwitchRequest,
        target_rect: PixelRect,
        fullscreen: bool,
    ) -> Result<Vec<isize>, String> {
        if resolved.is_empty() {
            return Ok(Vec::new());
        }
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

        let cells = subdivide_for_count(target_rect, resolved.len());
        let mut moves: Vec<(isize, PixelRect)> = Vec::new();
        for (i, w) in resolved.iter().enumerate() {
            let Some(&cell) = cells.get(i) else {
                // Beyond the slot's capacity → minimize this window.
                self.window_ops.minimize(w.hwnd);
                continue;
            };
            match plan_park_into_slot(std::slice::from_ref(&main_rects[i]), cell) {
                ParkPlan::ShrinkToFit(mapped) => {
                    if let Some(&rect) = mapped.first() {
                        moves.push((w.hwnd, rect));
                    }
                }
                ParkPlan::MinimizeWhole => self.window_ops.minimize(w.hwnd),
            }
        }

        // `SetWindowPlacement` (via `set_placement`) atomically un-maximizes and
        // sizes each window to its cell — a plain `SetWindowPos` won't resize a
        // still-maximized window (問題2, 2026-07-23).
        for (hwnd, rect) in &moves {
            tracing::info!(
                target: "switch", hwnd = *hwnd, cell = ?rect, of = moves.len(),
                "switch: park window into cell"
            );
            // `fill`: `rect` is the desired visible cell, expanded by the
            // window's invisible DWM border so it fills flush (no gutter).
            self.window_ops.set_placement(*hwnd, *rect, false, true);
        }

        // "退避後に全画面表示": the browser's own full-screen keys are sent after
        // the switch settles (see `switch_to`). Report the placed windows.
        if fullscreen {
            Ok(moves.iter().map(|(hwnd, _)| *hwnd).collect())
        } else {
            Ok(Vec::new())
        }
    }

    /// Windows that must be evacuated from the outgoing workset's sub-screen cell
    /// before it is parked there. Empty unless `current` is sub-designated and a
    /// managed window from *another* (inactive) workset is still sitting in that
    /// exact cell from an earlier switch. These are journaled by the caller (so a
    /// rollback restores them) and then moved by `park_evictees`.
    fn resolve_sub_evictees<'a>(
        &self,
        current: Option<&Workset>,
        current_resolved: &[ResolvedWindow<'a>],
        target_resolved: &[ResolvedWindow<'a>],
        decisions: &HashMap<Uuid, MatchDecision>,
        request: &SwitchRequest<'a>,
    ) -> Vec<ResolvedWindow<'a>> {
        let Some(current) = current else {
            return Vec::new();
        };
        let ParkingPolicy::SubScreen { sub_screen_id } = &current.parking_policy else {
            return Vec::new();
        };
        let Some(sub) = request.sub_screens.iter().find(|s| s.id == *sub_screen_id) else {
            return Vec::new();
        };
        // Same sharer set the actual park uses (non-target sets with live
        // windows), so the eviction cell matches where `current` will land.
        let parking_sharers: std::collections::HashSet<Uuid> = request
            .worksets
            .iter()
            .filter(|w| w.id != request.target_workset_id)
            .filter(|w| {
                w.windows.iter().any(|mw| {
                    matches!(decisions.get(&mw.id), Some(MatchDecision::AutoRebind { .. }))
                })
            })
            .map(|w| w.id)
            .collect();
        let Some(cell) = sub_screen_slot_rect(
            sub,
            request.worksets,
            current.id,
            request.live_monitors,
            &parking_sharers,
        ) else {
            return Vec::new();
        };

        // Every managed live window and its current rect, keyed by hwnd so a
        // window shared across worksets is considered once. The `ManagedWindow`
        // (any owning one) supplies the id/process for journaling.
        let mut managed_by_hwnd: HashMap<isize, (&ManagedWindow, u32, PixelRect)> = HashMap::new();
        for ws in request.worksets {
            for mw in &ws.windows {
                if let Some(MatchDecision::AutoRebind { hwnd }) = decisions.get(&mw.id)
                    && let Some(live) = request.live_windows.iter().find(|w| w.hwnd == *hwnd)
                {
                    managed_by_hwnd
                        .entry(*hwnd)
                        .or_insert((mw, live.process_id, live.rect_px));
                }
            }
        }

        let incoming: std::collections::HashSet<isize> = current_resolved
            .iter()
            .chain(target_resolved)
            .map(|w| w.hwnd)
            .collect();
        let managed: Vec<(isize, PixelRect)> = managed_by_hwnd
            .iter()
            .map(|(hwnd, (_, _, rect))| (*hwnd, *rect))
            .collect();

        stale_sub_occupants(cell, &incoming, &managed)
            .into_iter()
            .filter_map(|hwnd| {
                managed_by_hwnd
                    .get(&hwnd)
                    .map(|(mw, pid, _)| ResolvedWindow {
                        managed: mw,
                        hwnd,
                        process_id: *pid,
                    })
            })
            .collect()
    }

    /// The general parking regions: every live monitor that is not a main
    /// monitor and not excluded. A monitor a sub-screen only *partly* uses (e.g.
    /// a 2-column sub occupying the right half) contributes its **unused cells**
    /// as parking regions, so the free half isn't wasted (spec §3 / 2026-07-23).
    /// A monitor a sub fully uses (split = One) contributes nothing.
    fn general_parking_screens(&self, request: &SwitchRequest) -> Vec<(PixelRect, usize)> {
        use crate::application::layout_service::auto_split_cells;
        let mut screens = Vec::new();
        for m in request.live_monitors {
            if request.main_monitor_ids.iter().any(|id| id == &m.device_name) {
                continue;
            }
            if request
                .saved_monitors
                .iter()
                .any(|s| s.stable_id == m.device_name && s.excluded)
            {
                continue;
            }
            let wa = m.work_area_px;
            let subs_here: Vec<&SubScreen> = request
                .sub_screens
                .iter()
                .filter(|s| s.monitor_ids.iter().any(|id| id == &m.device_name))
                .collect();
            if subs_here.is_empty() {
                screens.push((wa, screen_capacity(wa)));
                continue;
            }
            // Free = the cells of this monitor's sub grid that no sub occupies.
            // Each free cell's capacity is capped so its cells never fall below
            // the *whole* monitor's smallest cell (a free half of a QHD holds 2,
            // not 4 → no 1/8, spec §2 「サブは例外」).
            let split = subs_here[0].split;
            let used: std::collections::HashSet<usize> = subs_here
                .iter()
                .filter(|s| s.split == split)
                .map(|s| s.cell_index)
                .collect();
            for (i, cell) in auto_split_cells(wa, split).into_iter().enumerate() {
                if !used.contains(&i) {
                    screens.push((cell, capped_capacity(cell, wa)));
                }
            }
        }
        screens
    }

    /// Parks Auto-policy windows **by window** (spec §2): pool every auto set's
    /// windows and distribute them across the general parking monitors, each
    /// monitor subdivided by how many windows land on it (`distribute_parking`
    /// picks the monitor that keeps each window largest). Each window fills its
    /// cell; anything past capacity (HD 4 / QHD 6 per monitor) is minimized.
    fn park_auto_windows(&self, hwnds: &[isize], request: &SwitchRequest) {
        if hwnds.is_empty() {
            return;
        }
        let screens = self.general_parking_screens(request);
        let (placements, overflow) = distribute_parking_capped(&screens, hwnds);
        for (hwnd, cell) in placements {
            // Re-place every window every switch (no "already there → skip"): a
            // window can drift after its placement (its own resize, a display
            // event) once the 6.5s re-assert has ended, and only a fresh placement
            // brings it back. The per-window placement generation stops the old
            // and new re-assert threads from fighting, so this doesn't flicker
            // (2026-07-23).
            tracing::info!(
                target: "parking", hwnd, cell = ?cell, windows = hwnds.len(),
                "auto-park: place window into cell"
            );
            self.window_ops.set_placement(hwnd, cell, false, true);
        }
        for hwnd in overflow {
            tracing::info!(target: "parking", hwnd, "auto-park: no room, minimizing");
            self.window_ops.minimize(hwnd);
        }
    }

    /// Moves `evictees` off a sub-screen cell into general parking. A window that
    /// fits nowhere is minimized. Called before an outgoing set parks into a sub.
    fn park_evictees(&self, evictees: &[ResolvedWindow], request: &SwitchRequest) {
        if evictees.is_empty() {
            return;
        }
        let screens = self.general_parking_screens(request);
        let hwnds: Vec<isize> = evictees.iter().map(|w| w.hwnd).collect();
        let (placements, overflow) = distribute_parking_capped(&screens, &hwnds);
        for (hwnd, cell) in placements {
            self.window_ops.set_placement(hwnd, cell, false, true);
        }
        for hwnd in overflow {
            self.window_ops.minimize(hwnd);
        }
    }

    /// PLAN.md §3.8 failure path: restores every journaled window to its
    /// pre-switch placement. `current_workset_id` is never touched here.
    fn rollback(&self, mut journal: SwitchJournal, reason: String) -> SwitchError {
        tracing::warn!(
            target: "switch", %reason, windows = journal.windows.len(),
            "switch: FAILED, rolling back to pre-switch layout"
        );
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

/// The target rect a sub-screen parks into, or `None` if none of its monitors
/// are connected (→ the workset is minimized instead). `split == One` uses the
/// union of all live monitors' work areas (may span several); a `TwoColumns`/
/// `FourGrid` sub-screen uses the chosen half/quarter cell of its first live
/// monitor.
pub fn sub_screen_target_rect(sub: &SubScreen, live_monitors: &[MonitorInfo]) -> Option<PixelRect> {
    use crate::domain::monitor::AutoSplit;

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
    if work_areas.is_empty() {
        return None;
    }
    if sub.split == AutoSplit::One {
        return bounding_rect(&work_areas);
    }
    let cells = crate::application::layout_service::auto_split_cells(work_areas[0], sub.split);
    cells
        .get(sub.cell_index)
        .copied()
        .or_else(|| bounding_rect(&work_areas))
}

/// A `cols × rows` grid of `rect` in row-major (reading) order. Boundaries are
/// computed from exact fractional positions so the cells tile `rect` with no
/// gaps or overlap even when the size doesn't divide evenly.
fn grid(rect: PixelRect, cols: i32, rows: i32) -> Vec<PixelRect> {
    let mut cells = Vec::with_capacity((cols * rows) as usize);
    for r in 0..rows {
        for c in 0..cols {
            let x0 = rect.x + rect.width * c / cols;
            let x1 = rect.x + rect.width * (c + 1) / cols;
            let y0 = rect.y + rect.height * r / rows;
            let y1 = rect.y + rect.height * (r + 1) / rows;
            cells.push(PixelRect::new(x0, y0, x1 - x0, y1 - y0));
        }
    }
    cells
}

/// Max windows a parking screen holds before overflowing to the next screen:
/// a QHD-class (2560-wide) or larger monitor fits a 3×2 grid (6); a smaller
/// monitor (e.g. 1920×1080) a 2×2 grid (4). (確定仕様 2026-07-23.)
pub fn screen_capacity(rect: PixelRect) -> usize {
    if rect.width.max(rect.height) >= 2400 {
        6
    } else {
        4
    }
}

/// Cells for `count` windows on `rect`, using **equal-size grid cells** and
/// filling the first `count` of them — the layout progression 確定 2026-07-23
/// (`screen-layout-spec.md` §2.2/§2.3):
/// 1→whole; 2→halves along the long axis; 3–4→a 2×2 grid (3 fills three cells,
/// one left empty); 5–6→a 3×2 grid (5 fills five). Every returned cell is the
/// same size, and the split is kept as square as possible (2 columns, 2×2,
/// 3×2). So e.g. HD 1920×1080 gives 1920×1080 / 960×1080 / 960×540 / 960×540 for
/// 1..4 windows. Beyond six a window gets no cell and is minimized by the caller.
pub fn subdivide_for_count(rect: PixelRect, count: usize) -> Vec<PixelRect> {
    let wide = rect.width >= rect.height;
    let full = match count {
        0 | 1 => vec![rect],
        2 => {
            if wide {
                grid(rect, 2, 1)
            } else {
                grid(rect, 1, 2)
            }
        }
        3 | 4 => grid(rect, 2, 2),
        _ => {
            if wide {
                grid(rect, 3, 2)
            } else {
                grid(rect, 2, 3)
            }
        }
    };
    // Fill only the first `count` cells; the rest of the grid stays empty (e.g.
    // 3 windows on a 2×2 grid → three equal cells, one empty).
    let used = count.clamp(1, full.len());
    full.into_iter().take(used).collect()
}

/// The area of the *smallest* cell a region is cut into for `count` windows.
/// Used to pick a parking screen by how large its worst cell would stay after
/// adding one more window — the basis for a balanced distribution.
pub fn smallest_cell_area(rect: PixelRect, count: usize) -> i64 {
    subdivide_for_count(rect, count)
        .iter()
        .map(|r| i64::from(r.width) * i64::from(r.height))
        .min()
        .unwrap_or(0)
}

/// Distributes `windows` across the available parking `screens` so cells stay as
/// large and even as possible, subdividing each screen by how many windows it
/// ends up holding (see `subdivide_for_count`), never stacking. Each screen's
/// capacity depends on its size (`screen_capacity` — 4 for a 1080p-class monitor,
/// 6 for QHD-class); any window beyond every screen's capacity is returned in the
/// overflow list (to be minimized).
///
/// Each window goes to the screen whose **smallest resulting cell would be
/// largest** (`smallest_cell_area` after adding it). An empty screen's smallest
/// cell is the whole screen, so empty screens fill first, largest first; once
/// every screen holds one, the next window goes to whichever keeps the biggest
/// cell — which *balances* the load rather than piling onto the largest screen
/// and leaving a window at 1/4 while another screen still has a free half
/// (2026-07-23).
pub fn distribute_parking(
    screens: &[PixelRect],
    windows: &[isize],
) -> (Vec<(isize, PixelRect)>, Vec<isize>) {
    let capped: Vec<(PixelRect, usize)> =
        screens.iter().map(|s| (*s, screen_capacity(*s))).collect();
    distribute_parking_capped(&capped, windows)
}

/// The largest number of windows a *partial* parking region (a free cell of a
/// monitor a sub only partly uses) may hold before its cells would fall below
/// the parent monitor's own smallest cell (1/6 of a QHD, 1/4 of an HD). Without
/// this, a free half of a QHD would be split into quarters — i.e. 1/8 of the
/// whole monitor — which is smaller than the 1/6 minimum (spec §2: サブは例外,
/// 2026-07-23). A full monitor (`region == monitor`) yields its `screen_capacity`.
pub fn capped_capacity(region: PixelRect, monitor: PixelRect) -> usize {
    let full_min = smallest_cell_area(monitor, screen_capacity(monitor));
    let mut cap = 1;
    for n in 1..=screen_capacity(region) {
        if smallest_cell_area(region, n) >= full_min {
            cap = n;
        } else {
            break;
        }
    }
    cap
}

/// Distributes `windows` across `(region, capacity)` parking screens, balanced
/// so each window stays as large as possible (`smallest_cell_area`), never
/// exceeding a screen's given capacity. See `distribute_parking`; this variant
/// lets a partial region carry a reduced capacity (`capped_capacity`).
pub fn distribute_parking_capped(
    screens: &[(PixelRect, usize)],
    windows: &[isize],
) -> (Vec<(isize, PixelRect)>, Vec<isize>) {
    let mut occupants: Vec<Vec<isize>> = vec![Vec::new(); screens.len()];
    let mut overflow = Vec::new();
    for &hwnd in windows {
        let best = (0..screens.len())
            .filter(|&i| occupants[i].len() < screens[i].1)
            .max_by_key(|&i| (smallest_cell_area(screens[i].0, occupants[i].len() + 1), -(i as i64)));
        match best {
            Some(i) => occupants[i].push(hwnd),
            None => overflow.push(hwnd),
        }
    }
    let mut placements = Vec::new();
    for (i, occ) in occupants.iter().enumerate() {
        for (hwnd, cell) in occ.iter().zip(subdivide_for_count(screens[i].0, occ.len())) {
            placements.push((*hwnd, cell));
        }
    }
    (placements, overflow)
}

/// hwnds of managed windows that still occupy `cell` (a sub-screen cell about to
/// receive an incoming window) but are **not** part of this switch — stale
/// occupants left there by an earlier switch. They must be evacuated to general
/// parking before the incoming window is placed, otherwise it would be stacked
/// on top of them (「サブに入った画面を他に退避させてからメインをサブに入れる」,
/// 2026-07-23). `managed` is `(hwnd, current rect)` for every RepoDeck-managed
/// live window; `incoming` is the hwnds of the current + target worksets, which
/// are never evicted. The result is sorted and de-duplicated for determinism.
pub fn stale_sub_occupants(
    cell: PixelRect,
    incoming: &std::collections::HashSet<isize>,
    managed: &[(isize, PixelRect)],
) -> Vec<isize> {
    let mut out: Vec<isize> = managed
        .iter()
        .filter(|(hwnd, _)| !incoming.contains(hwnd))
        .filter(|(_, rect)| rect.overlaps(&cell))
        .map(|(hwnd, _)| *hwnd)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Counts how many worksets park onto the sub-screen `sub_screen_id`.
pub fn sub_screen_sharer_count(worksets: &[Workset], sub_screen_id: Uuid) -> usize {
    worksets
        .iter()
        .filter(|w| {
            matches!(
                &w.parking_policy,
                ParkingPolicy::SubScreen { sub_screen_id: id } if *id == sub_screen_id
            )
        })
        .count()
}

/// The specific, non-overlapping cell of a sub-screen that `workset_id` parks
/// into. The region is divided among only the sharers **actually parking now**
/// (`parking_sharers` — those with live windows that aren't the active set), not
/// every set that merely designates the sub. So a lone parking sharer uses the
/// whole sub (its windows then split it), and two split it in half, etc. (spec
/// §2/§3, 2026-07-23). Ordered by position in `worksets` for a stable cell.
/// Returns `None` when the sub's monitors are offline, `workset_id` isn't
/// parking here, or it exceeds capacity (→ caller minimizes it).
pub fn sub_screen_slot_rect(
    sub: &SubScreen,
    worksets: &[Workset],
    workset_id: Uuid,
    live_monitors: &[MonitorInfo],
    parking_sharers: &std::collections::HashSet<Uuid>,
) -> Option<PixelRect> {
    let region = sub_screen_target_rect(sub, live_monitors)?;
    let sharers: Vec<Uuid> = worksets
        .iter()
        .filter(|w| {
            matches!(
                &w.parking_policy,
                ParkingPolicy::SubScreen { sub_screen_id } if *sub_screen_id == sub.id
            )
        })
        .filter(|w| parking_sharers.contains(&w.id))
        .map(|w| w.id)
        .collect();
    let index = sharers.iter().position(|id| *id == workset_id)?;
    let cap = screen_capacity(region);
    if index >= cap {
        return None;
    }
    subdivide_for_count(region, sharers.len().min(cap))
        .into_iter()
        .nth(index)
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

    #[test]
    fn subdivide_for_count_tiles_without_overlap() {
        let region = PixelRect::new(0, 0, 1000, 400);
        assert_eq!(subdivide_for_count(region, 1), vec![region]);

        // Wide region → two columns.
        let two = subdivide_for_count(region, 2);
        assert_eq!(two.len(), 2);
        assert_eq!(two[0], PixelRect::new(0, 0, 500, 400));
        assert_eq!(two[1], PixelRect::new(500, 0, 500, 400));

        // Tall region → two rows.
        let tall = subdivide_for_count(PixelRect::new(0, 0, 400, 1000), 2);
        assert_eq!(tall[0], PixelRect::new(0, 0, 400, 500));
        assert_eq!(tall[1], PixelRect::new(0, 500, 400, 500));

        // Three: a 2×2 grid with the first three (equal) cells filled, one left
        // empty — every cell the same size (spec §2.2/§2.3).
        let three = subdivide_for_count(region, 3);
        assert_eq!(three.len(), 3);
        assert_eq!(three[0], PixelRect::new(0, 0, 500, 200));
        assert_eq!(three[1], PixelRect::new(500, 0, 500, 200));
        assert_eq!(three[2], PixelRect::new(0, 200, 500, 200));
        // All three cells are the same size.
        assert!(three.iter().all(|c| c.width == 500 && c.height == 200));

        // Three or four sharers → quarters; every cell disjoint.
        let quad = subdivide_for_count(region, 4);
        assert_eq!(quad.len(), 4);
        for (i, a) in quad.iter().enumerate() {
            for b in &quad[i + 1..] {
                let overlap =
                    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom();
                assert!(!overlap, "cells {a:?} and {b:?} overlap");
            }
        }
    }

    #[test]
    fn distribute_parking_balances_across_screens_and_subdivides() {
        let screens = vec![
            PixelRect::new(0, 0, 1000, 800),
            PixelRect::new(1000, 0, 1000, 800),
        ];
        // Five windows over two screens → emptiest-first round-robin gives
        // screen0 three (quarters) and screen1 two (halves).
        let (placements, overflow) = distribute_parking(&screens, &[1, 2, 3, 4, 5]);
        assert!(overflow.is_empty());
        assert_eq!(placements.len(), 5);
        let on0 = placements.iter().filter(|(_, r)| r.x < 1000).count();
        let on1 = placements.iter().filter(|(_, r)| r.x >= 1000).count();
        assert_eq!(on0, 3);
        assert_eq!(on1, 2);
        // No two parked windows overlap.
        for (i, (_, a)) in placements.iter().enumerate() {
            for (_, b) in &placements[i + 1..] {
                let overlap =
                    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom();
                assert!(!overlap, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn distribute_parking_minimizes_overflow_past_four_per_screen() {
        let screens = vec![PixelRect::new(0, 0, 800, 600)];
        let (placements, overflow) = distribute_parking(&screens, &[1, 2, 3, 4, 5, 6]);
        assert_eq!(placements.len(), 4); // capped at four per screen
        assert_eq!(overflow, vec![5, 6]);
    }

    #[test]
    fn distribute_parking_uses_a_3x2_grid_on_a_qhd_screen() {
        // A single QHD monitor holds six windows (3×2), not four.
        let screens = vec![PixelRect::new(0, 0, 2560, 1440)];
        let (placements, overflow) = distribute_parking(&screens, &[1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(placements.len(), 6);
        assert_eq!(overflow, vec![7]); // the 7th overflows
        // The six cells tile the screen without overlap.
        for (i, (_, a)) in placements.iter().enumerate() {
            for (_, b) in &placements[i + 1..] {
                let overlap =
                    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom();
                assert!(!overlap, "{a:?} overlaps {b:?}");
            }
        }
        // Cells are QHD-thirds/halves, i.e. ~853×720.
        assert!(
            placements
                .iter()
                .all(|(_, r)| r.width < 1000 && r.height < 800)
        );
    }

    #[test]
    fn three_windows_on_one_monitor_are_three_equal_quarters() {
        // 3 windows forced onto a single HD monitor use the 2×2 grid's first
        // three cells — all equal quarters (960×540). The third is NOT full size.
        let hd = PixelRect::new(0, 0, 1920, 1080);
        let (placements, overflow) = distribute_parking(&[hd], &[1, 2, 3]);
        assert!(overflow.is_empty());
        assert_eq!(placements.len(), 3);
        for (_, r) in &placements {
            assert_eq!(
                (r.width, r.height),
                (960, 540),
                "a cell of 3-on-one-monitor is {r:?}, not a quarter"
            );
        }
        for (i, (_, a)) in placements.iter().enumerate() {
            for (_, b) in &placements[i + 1..] {
                let overlap =
                    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom();
                assert!(!overlap, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn capped_capacity_keeps_partial_regions_above_the_full_min_cell() {
        let qhd = PixelRect::new(0, 0, 2560, 1440);
        let hd = PixelRect::new(0, 0, 1920, 1080);
        // A full monitor keeps its own capacity.
        assert_eq!(capped_capacity(qhd, qhd), 6);
        assert_eq!(capped_capacity(hd, hd), 4);
        // A free HALF of a QHD holds 2, not 4 — otherwise its cells would be a
        // quarter of the half = 1/8 of the monitor, below the 1/6 minimum.
        assert_eq!(capped_capacity(PixelRect::new(0, 0, 1280, 1440), qhd), 2);
        // A free half of an HD holds 2 (each 1/4 of the monitor, the HD minimum).
        assert_eq!(capped_capacity(PixelRect::new(0, 0, 960, 1080), hd), 2);
    }

    #[test]
    fn distribute_parking_lights_the_largest_screen_first() {
        // A smaller screen listed BEFORE a larger one. A single window must land
        // on the larger screen (maximising total parked area), not the first
        // one in the list.
        let screens = vec![
            PixelRect::new(0, 0, 1920, 1080),    // HD, listed first
            PixelRect::new(1920, 0, 2560, 1440), // QHD, larger — listed second
        ];
        let (placements, overflow) = distribute_parking(&screens, &[1]);
        assert!(overflow.is_empty());
        assert_eq!(placements.len(), 1);
        assert!(
            placements[0].1.x >= 1920,
            "the single window should land on the larger QHD screen, got {:?}",
            placements[0].1
        );
    }

    #[test]
    fn distribute_parking_keeps_windows_largest_when_screens_are_mixed() {
        // HD (listed first) + QHD, three windows. Once both screens are lit, the
        // third window goes where its cell stays largest: the QHD (a QHD half is
        // bigger than an HD half), so QHD holds 2 and HD holds 1.
        let screens = vec![
            PixelRect::new(0, 0, 1920, 1080),    // HD
            PixelRect::new(1920, 0, 2560, 1440), // QHD
        ];
        let (placements, overflow) = distribute_parking(&screens, &[1, 2, 3]);
        assert!(overflow.is_empty());
        let on_qhd = placements.iter().filter(|(_, r)| r.x >= 1920).count();
        let on_hd = placements.iter().filter(|(_, r)| r.x < 1920).count();
        assert_eq!(on_qhd, 2, "QHD should take the extra window");
        assert_eq!(on_hd, 1);
    }

    #[test]
    fn stale_sub_occupants_flags_only_foreign_windows_overlapping_the_cell() {
        let cell = PixelRect::new(1920, 0, 1280, 1440); // a sub-screen half
        let incoming: std::collections::HashSet<isize> = [10, 11].into_iter().collect();
        let managed = vec![
            (10, PixelRect::new(1920, 0, 1280, 1440)), // incoming → never evicted
            (20, PixelRect::new(1920, 0, 1280, 1440)), // foreign, on the cell → evict
            (30, PixelRect::new(0, 0, 1920, 1080)),    // foreign, on the main → keep
            (40, PixelRect::new(2000, 100, 400, 300)), // foreign, partly on cell → evict
        ];
        let evictees = stale_sub_occupants(cell, &incoming, &managed);
        assert_eq!(evictees, vec![20, 40]);
    }

    #[test]
    fn active_windows_tile_and_never_overlap_with_one_effective_main_monitor() {
        // main_monitor_ids lists two monitors, but only MAIN0 is actually live.
        // A 2-window set whose windows both map to main (the second falls back to
        // MAIN0) must TILE MAIN0 into halves, not stack two full-size windows —
        // otherwise one is drawn on top of the other (2026-07-23).
        let monitors = vec![
            mon_wh("MAIN0", 0, 0, 1920, 1080),
            mon_wh("PARK1", -1920, 0, 1920, 1080),
        ];
        let main_ids = vec!["MAIN0".to_string(), "MAIN1".to_string()]; // MAIN1 offline
        let full = NormalizedRect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 };
        let s = workset(
            0,
            ParkingPolicy::Auto,
            vec![
                managed_window("wa", 0, full, SavedShowState::Maximized, 0),
                managed_window("wb", 1, full, SavedShowState::Maximized, 1),
            ],
        );
        let other = workset(
            1,
            ParkingPolicy::Auto,
            vec![managed_window("oc", 0, full, SavedShowState::Maximized, 0)],
        );
        let worksets = vec![s.clone(), other];
        let wins = [(100isize, "wa"), (101, "wb"), (102, "oc")];

        let fake = FakeWindowOps::new();
        for (h, _) in &wins {
            fake.seed_window(*h, PixelRect::new(0, 0, 800, 600), SavedShowState::Normal);
        }
        let dir = tempdir().unwrap();
        let coord = SwitchCoordinator::new(fake, dir.path().to_path_buf());
        let live: Vec<TopLevelWindow> = wins
            .iter()
            .map(|(h, e)| {
                let mut lw = live_window(*h, e);
                lw.rect_px = coord.window_ops.rect_of(*h).unwrap();
                lw
            })
            .collect();

        coord
            .switch_to(SwitchRequest {
                worksets: &worksets,
                fixed_slots: &[],
                sub_screens: &[],
                saved_monitors: &[],
                main_monitor_ids: &main_ids,
                live_monitors: &monitors,
                live_windows: &live,
                target_workset_id: s.id,
            })
            .unwrap();

        let ra = coord.window_ops.rect_of(100).unwrap();
        let rb = coord.window_ops.rect_of(101).unwrap();
        let overlap =
            ra.x < rb.right() && rb.x < ra.right() && ra.y < rb.bottom() && rb.y < ra.bottom();
        assert!(!overlap, "active windows stack on the single main monitor: {ra:?} vs {rb:?}");
        // Both are on MAIN0 (its work area == bounds in this fake topology).
        let main0 = PixelRect::new(0, 0, 1920, 1080);
        for r in [ra, rb] {
            assert!(
                r.x >= main0.x && r.right() <= main0.right() && r.y >= main0.y && r.bottom() <= main0.bottom(),
                "window {r:?} is not within the single main monitor"
            );
        }
    }

    fn mon_wh(device_name: &str, x: i32, y: i32, w: i32, h: i32) -> MonitorInfo {
        MonitorInfo {
            handle: 0,
            device_name: device_name.to_string(),
            bounds_px: PixelRect::new(x, y, w, h),
            work_area_px: PixelRect::new(x, y, w, h),
            dpi_x: 96,
            dpi_y: 96,
            is_primary: x == 0 && y == 0,
        }
    }

    /// Drives the *real* `switch_to` through thousands of random set switches on
    /// a fake desktop and asserts, after every switch, that the layout is valid:
    /// no two visible windows overlap, the set switched to actually has its
    /// windows on-screen (not stranded/minimized), and nothing lands off every
    /// monitor. Deterministic PRNG so any failure reproduces from the seed.
    #[test]
    fn simulation_thousands_of_random_switches_keep_the_layout_valid() {
        use crate::domain::config::SubScreen;

        // 2 main + 5 parking (2 HD, 3 QHD) + 1 sub monitor. Enough parking that
        // the auto windows usually have slack, so the optimality check below is
        // actually exercised (not just "everything is full").
        let monitors = vec![
            mon_wh("MAIN0", 0, 0, 1920, 1080),
            mon_wh("MAIN1", 1920, 0, 1920, 1080),
            mon_wh("PARK1", -1920, 0, 1920, 1080),
            mon_wh("PARK2", -3840, 0, 1920, 1080),
            mon_wh("PARK3", 3840, 0, 2560, 1440),
            mon_wh("PARK4", 3840, 1440, 2560, 1440),
            mon_wh("PARK5", 3840, 2880, 2560, 1440),
            mon_wh("SUBMON", 6400, 0, 1920, 1080),
        ];
        let main_ids = vec!["MAIN0".to_string(), "MAIN1".to_string()];
        let sub = SubScreen {
            id: Uuid::new_v4(),
            name: "S".to_string(),
            monitor_ids: vec!["SUBMON".to_string()],
            split: AutoSplit::One,
            cell_index: 0,
            fullscreen: false,
        };
        let full = NormalizedRect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 };

        // 15 sets × 2 windows; sets 0,1 park onto the sub, the rest are Auto.
        // Each window has a unique exe/title so it matches exactly one entry.
        let mut worksets: Vec<Workset> = Vec::new();
        let mut wins: Vec<(isize, String)> = Vec::new();
        let mut hwnd = 100isize;
        for s in 0..15 {
            let policy = if s < 2 {
                ParkingPolicy::SubScreen { sub_screen_id: sub.id }
            } else {
                ParkingPolicy::Auto
            };
            let mut mws = Vec::new();
            for (idx, ab) in ["a", "b"].iter().enumerate() {
                let exe = format!("s{s:02}{ab}");
                mws.push(managed_window(&exe, idx, full, SavedShowState::Normal, idx as i32));
                wins.push((hwnd, exe));
                hwnd += 1;
            }
            worksets.push(workset(s, policy, mws));
        }

        let fake = FakeWindowOps::new();
        for (h, _) in &wins {
            fake.seed_window(*h, PixelRect::new(0, 0, 800, 600), SavedShowState::Normal);
        }
        let dir = tempdir().unwrap();
        let coord = SwitchCoordinator::new(fake, dir.path().to_path_buf());
        let mon_rects: Vec<PixelRect> = monitors.iter().map(|m| m.bounds_px).collect();

        let mut seed: u64 = 0x1234_5678_9abc_def0;
        for iter in 0..3000u32 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let target_idx = ((seed >> 33) as usize) % worksets.len();

            let live: Vec<TopLevelWindow> = wins
                .iter()
                .map(|(h, exe)| {
                    let mut lw = live_window(*h, exe);
                    lw.rect_px = coord.window_ops.rect_of(*h).unwrap();
                    lw
                })
                .collect();

            coord
                .switch_to(SwitchRequest {
                    worksets: &worksets,
                    fixed_slots: &[],
                    sub_screens: std::slice::from_ref(&sub),
                    saved_monitors: &[],
                    main_monitor_ids: &main_ids,
                    live_monitors: &monitors,
                    live_windows: &live,
                    target_workset_id: worksets[target_idx].id,
                })
                .unwrap_or_else(|e| panic!("switch #{iter} to set {target_idx} failed: {e:?}"));

            // The set switched to must have both its windows visible (on main).
            for k in 0..2 {
                let h = wins[target_idx * 2 + k].0;
                assert_ne!(
                    coord.window_ops.show_state_of(h),
                    Some(SavedShowState::Minimized),
                    "switch #{iter} to set {target_idx}: its window {h} was left minimized"
                );
            }

            // Collect every non-minimized window's rect.
            let placed: Vec<(isize, PixelRect)> = wins
                .iter()
                .filter(|(h, _)| {
                    coord.window_ops.show_state_of(*h) != Some(SavedShowState::Minimized)
                })
                .map(|(h, _)| (*h, coord.window_ops.rect_of(*h).unwrap()))
                .collect();

            // No two visible windows overlap.
            for i in 0..placed.len() {
                for j in (i + 1)..placed.len() {
                    let (ha, a) = placed[i];
                    let (hb, b) = placed[j];
                    let overlap =
                        a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom();
                    assert!(
                        !overlap,
                        "switch #{iter} to set {target_idx}: {ha} {a:?} overlaps {hb} {b:?}"
                    );
                }
            }
            // Every visible window is fully on some monitor.
            for (h, r) in &placed {
                assert!(
                    on_some_monitor(*r, &mon_rects),
                    "switch #{iter} to set {target_idx}: window {h} {r:?} is off every monitor"
                );
            }

            // The active set's windows are on a MAIN monitor.
            let main_rects = [mon_rects[0], mon_rects[1]];
            for k in 0..2 {
                let h = wins[target_idx * 2 + k].0;
                let r = coord.window_ops.rect_of(h).unwrap();
                assert!(
                    on_some_monitor(r, &main_rects),
                    "switch #{iter}: active set {target_idx} window {h} {r:?} is not on a main monitor"
                );
            }
            // No non-active visible window sits on a main monitor.
            for si in 0..worksets.len() {
                if si == target_idx {
                    continue;
                }
                for k in 0..2 {
                    let h = wins[si * 2 + k].0;
                    if coord.window_ops.show_state_of(h) == Some(SavedShowState::Minimized) {
                        continue;
                    }
                    let r = coord.window_ops.rect_of(h).unwrap();
                    assert!(
                        !on_some_monitor(r, &main_rects),
                        "switch #{iter}: non-active set {si} window {h} {r:?} is on a main monitor"
                    );
                }
            }

            // OPTIMALITY: the parked auto windows are at a local maximin optimum —
            // no single window can move to another parking monitor and make the
            // smallest parked cell larger (spec §2.5: each window as large as the
            // layout allows). This is what catches a *suboptimal* (not merely
            // invalid) placement.
            let park = [
                mon_rects[2], mon_rects[3], mon_rects[4], mon_rects[5], mon_rects[6],
            ];
            let caps: Vec<usize> = park.iter().map(|m| screen_capacity(*m)).collect();
            let mut occ = [0usize; 5];
            for si in 2..worksets.len() {
                if si == target_idx {
                    continue;
                }
                for k in 0..2 {
                    let h = wins[si * 2 + k].0;
                    if coord.window_ops.show_state_of(h) == Some(SavedShowState::Minimized) {
                        continue;
                    }
                    let r = coord.window_ops.rect_of(h).unwrap();
                    for (mi, pm) in park.iter().enumerate() {
                        if r.x >= pm.x
                            && r.right() <= pm.right()
                            && r.y >= pm.y
                            && r.bottom() <= pm.bottom()
                        {
                            occ[mi] += 1;
                            break;
                        }
                    }
                }
            }
            let cell = |mi: usize, count: usize| -> i64 {
                if count == 0 {
                    i64::MAX
                } else {
                    smallest_cell_area(park[mi], count)
                }
            };
            let cur_min = (0..5)
                .filter(|&mi| occ[mi] > 0)
                .map(|mi| cell(mi, occ[mi]))
                .min()
                .unwrap_or(i64::MAX);
            for a in 0..5 {
                if occ[a] == 0 {
                    continue;
                }
                for b in 0..5 {
                    if a == b || occ[b] >= caps[b] {
                        continue;
                    }
                    let mut n = occ;
                    n[a] -= 1;
                    n[b] += 1;
                    let new_min = (0..5)
                        .filter(|&mi| n[mi] > 0)
                        .map(|mi| cell(mi, n[mi]))
                        .min()
                        .unwrap_or(i64::MAX);
                    assert!(
                        new_min <= cur_min,
                        "switch #{iter} to set {target_idx}: SUBOPTIMAL parking — moving a window {a}->{b} raises smallest cell {cur_min}->{new_min} (occ {occ:?} caps {caps:?})"
                    );
                }
            }
        }
    }

    #[test]
    fn simulation_sub_screen_sharers_tile_the_region_without_gaps() {
        use crate::domain::config::SubScreen;

        let sub = SubScreen {
            id: Uuid::new_v4(),
            name: "サブ".to_string(),
            monitor_ids: vec!["SUB".to_string()],
            split: AutoSplit::One,
            cell_index: 0,
            fullscreen: false,
        };
        let policy = ParkingPolicy::SubScreen {
            sub_screen_id: sub.id,
        };
        let live = vec![monitor("SUB", 1920)];
        let region = sub_screen_target_rect(&sub, &live).unwrap();
        let region_area = i64::from(region.width) * i64::from(region.height);

        // Sharers each get a distinct, non-overlapping, equal-size cell within
        // the sub region. Counts that fill a whole grid (1, 2, 4) cover the region
        // exactly; 3 uses three of a 2×2 grid, leaving one cell empty (spec §2.3).
        for n in 1..=4i32 {
            let worksets: Vec<Workset> =
                (0..n).map(|i| workset(i, policy.clone(), vec![])).collect();
            let all: std::collections::HashSet<Uuid> = worksets.iter().map(|w| w.id).collect();
            let cells: Vec<PixelRect> = worksets
                .iter()
                .map(|w| sub_screen_slot_rect(&sub, &worksets, w.id, &live, &all).unwrap())
                .collect();
            for (i, a) in cells.iter().enumerate() {
                for b in &cells[i + 1..] {
                    let overlap =
                        a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom();
                    assert!(!overlap, "{n} sharers: {a:?} overlaps {b:?}");
                }
            }
            // Cells are all the same size and stay inside the region.
            let first = cells[0];
            assert!(cells.iter().all(|c| c.width == first.width && c.height == first.height));
            let covered: i64 = cells
                .iter()
                .map(|r| i64::from(r.width) * i64::from(r.height))
                .sum();
            let expected = if n == 3 { region_area * 3 / 4 } else { region_area };
            // Allow ±1px rounding across the grid boundaries.
            assert!(
                (covered - expected).abs() <= i64::from(region.width) + i64::from(region.height),
                "{n} sharers coverage {covered} vs expected ~{expected}"
            );
        }
    }

    #[test]
    fn sub_screen_slot_rect_gives_each_sharer_a_distinct_cell() {
        use crate::domain::config::SubScreen;

        let sub = SubScreen {
            id: Uuid::new_v4(),
            name: "サブ".to_string(),
            monitor_ids: vec!["SUB".to_string()],
            split: AutoSplit::One,
            cell_index: 0,
            fullscreen: false,
        };
        let policy = ParkingPolicy::SubScreen {
            sub_screen_id: sub.id,
        };
        // Five worksets all park onto the same sub-screen.
        let worksets: Vec<Workset> = (0..5).map(|i| workset(i, policy.clone(), vec![])).collect();
        // A non-main monitor at x=1920 so the sub has a live work area.
        let live = vec![monitor("SUB", 1920)];
        let all: std::collections::HashSet<Uuid> = worksets.iter().map(|w| w.id).collect();

        let cells: Vec<Option<PixelRect>> = worksets
            .iter()
            .map(|w| sub_screen_slot_rect(&sub, &worksets, w.id, &live, &all))
            .collect();

        // First four get a cell; the fifth overflows to None (→ minimized).
        assert!(cells[0].is_some());
        assert!(cells[3].is_some());
        assert_eq!(cells[4], None);
        // The four cells are pairwise disjoint.
        let rects: Vec<PixelRect> = cells[..4].iter().map(|c| c.unwrap()).collect();
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                let overlap =
                    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom();
                assert!(!overlap, "sharer cells {a:?} and {b:?} overlap");
            }
        }
    }
}
