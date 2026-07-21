//! Startup crash recovery from a leftover `switch-journal.json` (PLAN.md
//! §10.3): a `SwitchCoordinator::switch_to` call that never reached its final
//! `journal_store::clear` (RepoDeck crashed, or was killed, mid-switch) leaves
//! a journal describing every affected window's pre-switch placement.
//!
//! Unlike the in-process `SwitchCoordinator::rollback` (which trusts the
//! journal's raw `hwnd`s because they were captured moments earlier in the
//! same process run), this module re-resolves each entry's `managed_window_id`
//! through the same `WindowMatcher`-based mechanism `switch_to`/
//! `recover_all_windows` already use (PLAN.md §7.4: "HWNDは当該トランザクション
//!内だけで使用し、再起動後はプロセスIDと現在属性を再検証する") — a window that
//! was closed and reopened between the crash and this startup is still found
//! and restored correctly, since its identity is the matcher, not the stale
//! hwnd.

use uuid::Uuid;

use crate::application::recovery_service::{RecoveredWindow, RecoveryReport};
use crate::application::window_ops::WindowOps;
use crate::application::workset_service;
use crate::domain::placement::SavedShowState;
use crate::domain::workset::Workset;
use crate::persistence::journal_store::{JournalStatus, SwitchJournal};
use crate::windowing::enumerate::TopLevelWindow;
use crate::windowing::matcher::MatchDecision;

/// The three choices PLAN.md §10.3 step 4 specifies for a leftover journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalRecoveryChoice {
    RestoreOriginalPlacement,
    RecoverAllToMain,
    DoNothing,
}

/// The exact "already succeeded" case `SwitchCoordinator::switch_to` itself
/// documents: a crash between persisting `runtime.current_workset_id` and
/// clearing the journal leaves a stale `Started` journal whose `to_workset_id`
/// already matches the current workset — the switch itself is done, nothing
/// needs recovering, and showing a dialog here would be pure noise.
pub fn already_succeeded(journal: &SwitchJournal, current_workset_id: Option<Uuid>) -> bool {
    journal.status == JournalStatus::Started && Some(journal.to_workset_id) == current_workset_id
}

/// PLAN.md §10.3 step 4's "元の配置へ戻す": restores every journaled window to
/// its captured pre-switch `rect`/`show_state`, re-resolving each one's live
/// hwnd fresh rather than trusting the journal's stale value.
pub fn restore_original_placement<W: WindowOps>(
    window_ops: &W,
    worksets: &[Workset],
    live_windows: &[TopLevelWindow],
    journal: &SwitchJournal,
) -> RecoveryReport {
    let decisions = workset_service::resolve_all_matches(worksets, live_windows);

    let mut recovered = Vec::new();
    let mut skipped = Vec::new();

    for entry in &journal.windows {
        let hwnd = match decisions.get(&entry.managed_window_id) {
            Some(MatchDecision::AutoRebind { hwnd }) if window_ops.is_window_alive(*hwnd) => *hwnd,
            _ => {
                skipped.push(RecoveredWindow {
                    managed_window_id: entry.managed_window_id,
                    hwnd: entry.hwnd,
                    placed_rect: None,
                    skip_reason: Some("window could not be re-resolved".to_string()),
                });
                continue;
            }
        };

        window_ops.restore(hwnd);
        if window_ops.batch_move(&[(hwnd, entry.before.rect)]).is_err() {
            skipped.push(RecoveredWindow {
                managed_window_id: entry.managed_window_id,
                hwnd,
                placed_rect: None,
                skip_reason: Some("failed to move window".to_string()),
            });
            continue;
        }
        match entry.before.show_state {
            SavedShowState::Maximized => window_ops.maximize(hwnd),
            SavedShowState::Minimized => window_ops.minimize(hwnd),
            SavedShowState::Normal => {}
        }

        recovered.push(RecoveredWindow {
            managed_window_id: entry.managed_window_id,
            hwnd,
            placed_rect: Some(entry.before.rect),
            skip_reason: None,
        });
    }

    RecoveryReport { recovered, skipped }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::application::window_ops::fake::FakeWindowOps;
    use crate::domain::placement::{
        NormalizedRect, PixelRect, SavedPlacement, SavedShowState as PlacementShowState,
    };
    use crate::domain::workset::{ManagedWindow, ParkingPolicy, RepositoryKind, WindowMatcher};
    use crate::persistence::journal_store::{JournalWindowEntry, JournalWindowState};

    fn journal(
        status: JournalStatus,
        to_workset_id: Uuid,
        windows: Vec<JournalWindowEntry>,
    ) -> SwitchJournal {
        SwitchJournal {
            schema_version: 1,
            transaction_id: Uuid::new_v4(),
            status,
            from_workset_id: None,
            to_workset_id,
            created_at: "2026-07-21T00:00:00Z".to_string(),
            windows,
        }
    }

    #[test]
    fn already_succeeded_when_started_and_current_matches_target() {
        let target = Uuid::new_v4();
        let j = journal(JournalStatus::Started, target, Vec::new());
        assert!(already_succeeded(&j, Some(target)));
    }

    #[test]
    fn not_already_succeeded_when_current_differs_from_target() {
        let target = Uuid::new_v4();
        let other = Uuid::new_v4();
        let j = journal(JournalStatus::Started, target, Vec::new());
        assert!(!already_succeeded(&j, Some(other)));
        assert!(!already_succeeded(&j, None));
    }

    #[test]
    fn not_already_succeeded_when_status_is_rolled_back() {
        let target = Uuid::new_v4();
        let j = journal(JournalStatus::RolledBack, target, Vec::new());
        assert!(!already_succeeded(&j, Some(target)));
    }

    fn matcher(title: &str) -> WindowMatcher {
        WindowMatcher {
            executable_path: PathBuf::from(r"C:\code.exe"),
            process_name: "code.exe".to_string(),
            window_class: "Chrome_WidgetWin_1".to_string(),
            registered_title: title.to_string(),
            title_contains: None,
            title_regex: None,
        }
    }

    fn managed_window(id: Uuid, title: &str) -> ManagedWindow {
        ManagedWindow {
            id,
            matcher: matcher(title),
            main_placement: SavedPlacement {
                monitor_id: "A".to_string(),
                main_monitor_index: 0,
                normalized_rect: NormalizedRect {
                    x: 0.0,
                    y: 0.0,
                    width: 0.5,
                    height: 0.5,
                },
                physical_rect_at_capture: PixelRect::new(0, 0, 800, 600),
                show_state: PlacementShowState::Normal,
            },
            z_order: 0,
        }
    }

    fn workset_with(windows: Vec<ManagedWindow>) -> Workset {
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
            windows,
            created_at: "2026-07-21T00:00:00Z".to_string(),
            updated_at: "2026-07-21T00:00:00Z".to_string(),
        }
    }

    fn live_window(hwnd: isize, title: &str) -> TopLevelWindow {
        TopLevelWindow {
            hwnd,
            process_id: 1,
            executable_path: Some(PathBuf::from(r"C:\code.exe")),
            window_class: "Chrome_WidgetWin_1".to_string(),
            title: title.to_string(),
            rect_px: PixelRect::new(500, 500, 800, 600),
        }
    }

    #[test]
    fn restores_a_matched_window_to_its_journaled_placement() {
        let managed_id = Uuid::new_v4();
        let worksets = [workset_with(vec![managed_window(
            managed_id,
            "repo - Visual Studio Code",
        )])];
        let live = [live_window(42, "repo - Visual Studio Code")];

        let ops = FakeWindowOps::new();
        ops.seed_window(
            42,
            PixelRect::new(500, 500, 800, 600),
            PlacementShowState::Maximized,
        );

        let before_rect = PixelRect::new(10, 10, 640, 480);
        let entry = JournalWindowEntry {
            managed_window_id: managed_id,
            hwnd: 999, // stale hwnd from the crashed session; must not be trusted
            process_id: 1,
            before: JournalWindowState {
                rect: before_rect,
                show_state: PlacementShowState::Normal,
            },
        };
        let j = journal(JournalStatus::Started, Uuid::new_v4(), vec![entry]);

        let report = restore_original_placement(&ops, &worksets, &live, &j);

        assert_eq!(report.recovered.len(), 1);
        assert!(report.skipped.is_empty());
        assert_eq!(report.recovered[0].hwnd, 42);
        assert_eq!(ops.rect_of(42), Some(before_rect));
        assert_eq!(ops.show_state_of(42), Some(PlacementShowState::Normal));
    }

    #[test]
    fn skips_an_entry_whose_window_no_longer_exists() {
        let managed_id = Uuid::new_v4();
        let worksets = [workset_with(vec![managed_window(
            managed_id,
            "repo - Visual Studio Code",
        )])];
        let live: [TopLevelWindow; 0] = []; // the window is gone

        let ops = FakeWindowOps::new();
        let entry = JournalWindowEntry {
            managed_window_id: managed_id,
            hwnd: 999,
            process_id: 1,
            before: JournalWindowState {
                rect: PixelRect::new(0, 0, 100, 100),
                show_state: PlacementShowState::Normal,
            },
        };
        let j = journal(JournalStatus::Started, Uuid::new_v4(), vec![entry]);

        let report = restore_original_placement(&ops, &worksets, &live, &j);

        assert!(report.recovered.is_empty());
        assert_eq!(report.skipped.len(), 1);
    }
}
