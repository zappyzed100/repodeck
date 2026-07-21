//! Pure decision logic for automatic display re-detection (PLAN.md §4.6 モニター
//! 再マッチ, Phase 9 resilience).
//!
//! When the PC resumes from sleep — or a monitor otherwise drops off the bus — the
//! live display topology can end up missing monitors that the user has saved into
//! their [`AppConfig`](crate::domain::config::AppConfig). This module answers the
//! *policy* questions ("should we attempt a software re-detect? which saved
//! monitors are missing? have we spent our retry budget?") as a single [`decide_recovery`]
//! function over plain data, so it can be unit-tested with no display hardware and
//! no Win32 at all. The actual side effect — the software "unplug/replug" that asks
//! Windows to re-apply the topology — lives in
//! [`crate::windowing::display_reset`]. This mirrors how the rest of the
//! `application` layer keeps decisions pure and pushes I/O to the edges.

use crate::domain::monitor::SavedMonitor;

/// How many software re-detect attempts we make before giving up and logging.
///
/// The automatic path must NEVER loop indefinitely (PLAN.md §10 レジリエンス):
/// each resume event resets the counter to zero, we retry at most this many times
/// with a settle delay between attempts, and then stop until the next resume.
pub const MAX_RECOVERY_ATTEMPTS: u32 = 3;

/// The outcome of [`decide_recovery`]: what the caller should do next.
///
/// The caller (in `app.rs`) matches on this and either fires the
/// [`crate::windowing::display_reset`] side effect or does nothing. Every variant
/// is logged via `tracing` so a support log makes the decision auditable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// The user has turned auto-recovery off ([`UserSettings::auto_display_recovery`](crate::domain::config::UserSettings)).
    /// Do nothing automatically; the manual tray trigger still works.
    Disabled,
    /// No monitors have been saved yet (first run, before Layout Studio has been
    /// used). There is no topology to compare against, so do nothing.
    NoSavedTopology,
    /// The live topology already contains every saved monitor. Nothing to fix —
    /// this is the common case and must be a no-op (PLAN.md §4.6: do not disturb a
    /// healthy configuration).
    UpToDate,
    /// One or more saved monitors are missing from the live set and we still have
    /// retry budget left. The caller should invoke the display-reset side effect.
    Recover {
        /// `stable_id`s of the saved monitors that are absent from the live set.
        missing: Vec<String>,
        /// 1-based attempt number this recovery represents (`attempts_made + 1`).
        attempt: u32,
    },
    /// Monitors are still missing but the retry budget is exhausted. The caller
    /// logs and stops until the next resume event.
    GiveUp {
        /// `stable_id`s still missing at the point we gave up.
        missing: Vec<String>,
    },
}

/// Returns the `stable_id` of every saved monitor that is absent from the live
/// display set.
///
/// A saved monitor counts as *present* when either its `stable_id` or its
/// `device_name` matches a live device name — matching the two keys PLAN.md §7.2
/// documents for [`SavedMonitor`], and tolerating the fact that on this build the
/// two are usually equal (see `wire_layout_studio`'s save path in `app.rs`).
pub fn missing_saved_monitors(saved: &[SavedMonitor], live_device_names: &[String]) -> Vec<String> {
    saved
        .iter()
        .filter(|m| {
            !live_device_names
                .iter()
                .any(|live| live == &m.stable_id || live == &m.device_name)
        })
        .map(|m| m.stable_id.clone())
        .collect()
}

/// Decides whether an automatic software re-detect should run, given the saved
/// topology, the currently-live device names, how many attempts have already been
/// made in this resume window, and whether the feature is enabled.
///
/// This is intentionally total and side-effect-free: same inputs always yield the
/// same [`RecoveryDecision`]. The gating rules (PLAN.md §4.6, §10 レジリエンス):
///
/// 1. Off switch wins: disabled → [`RecoveryDecision::Disabled`].
/// 2. Nothing saved → [`RecoveryDecision::NoSavedTopology`] (never mutate blindly).
/// 3. No missing monitors → [`RecoveryDecision::UpToDate`] (never disturb a healthy
///    topology; this is what makes the automatic path safe to fire on every resume).
/// 4. Missing monitors with budget left → [`RecoveryDecision::Recover`].
/// 5. Missing monitors but budget spent → [`RecoveryDecision::GiveUp`].
pub fn decide_recovery(
    saved_monitors: &[SavedMonitor],
    live_device_names: &[String],
    attempts_made: u32,
    auto_recovery_enabled: bool,
) -> RecoveryDecision {
    if !auto_recovery_enabled {
        return RecoveryDecision::Disabled;
    }
    if saved_monitors.is_empty() {
        return RecoveryDecision::NoSavedTopology;
    }

    let missing = missing_saved_monitors(saved_monitors, live_device_names);
    if missing.is_empty() {
        return RecoveryDecision::UpToDate;
    }

    if attempts_made >= MAX_RECOVERY_ATTEMPTS {
        return RecoveryDecision::GiveUp { missing };
    }

    RecoveryDecision::Recover {
        missing,
        attempt: attempts_made + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::monitor::AutoSplit;
    use crate::domain::placement::PixelRect;

    fn saved(id: &str) -> SavedMonitor {
        SavedMonitor {
            stable_id: id.to_string(),
            device_name: id.to_string(),
            device_path: None,
            friendly_name: None,
            bounds_px: PixelRect::new(0, 0, 1920, 1080),
            work_area_px: PixelRect::new(0, 0, 1920, 1040),
            dpi_x: 96,
            dpi_y: 96,
            auto_split: Some(AutoSplit::One),
            excluded: false,
        }
    }

    fn names(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn missing_detection_flags_only_absent_monitors() {
        let saved = vec![saved(r"\\.\DISPLAY1"), saved(r"\\.\DISPLAY2")];
        let live = names(&[r"\\.\DISPLAY1"]);
        assert_eq!(
            missing_saved_monitors(&saved, &live),
            vec![r"\\.\DISPLAY2".to_string()]
        );
    }

    #[test]
    fn recover_when_a_saved_monitor_is_missing() {
        let saved = vec![saved(r"\\.\DISPLAY1"), saved(r"\\.\DISPLAY2")];
        let live = names(&[r"\\.\DISPLAY1"]);
        assert_eq!(
            decide_recovery(&saved, &live, 0, true),
            RecoveryDecision::Recover {
                missing: vec![r"\\.\DISPLAY2".to_string()],
                attempt: 1,
            }
        );
    }

    #[test]
    fn no_op_when_topology_matches() {
        let saved = vec![saved(r"\\.\DISPLAY1"), saved(r"\\.\DISPLAY2")];
        let live = names(&[r"\\.\DISPLAY2", r"\\.\DISPLAY1"]);
        assert_eq!(
            decide_recovery(&saved, &live, 0, true),
            RecoveryDecision::UpToDate
        );
    }

    #[test]
    fn extra_live_monitors_do_not_trigger_recovery() {
        // A monitor the user has NOT saved being present is fine — recovery only
        // cares about saved monitors going missing, never the reverse.
        let saved = vec![saved(r"\\.\DISPLAY1")];
        let live = names(&[r"\\.\DISPLAY1", r"\\.\DISPLAY2", r"\\.\DISPLAY3"]);
        assert_eq!(
            decide_recovery(&saved, &live, 0, true),
            RecoveryDecision::UpToDate
        );
    }

    #[test]
    fn retry_budget_exhaustion_gives_up() {
        let saved = vec![saved(r"\\.\DISPLAY1"), saved(r"\\.\DISPLAY2")];
        let live = names(&[r"\\.\DISPLAY1"]);
        assert_eq!(
            decide_recovery(&saved, &live, MAX_RECOVERY_ATTEMPTS, true),
            RecoveryDecision::GiveUp {
                missing: vec![r"\\.\DISPLAY2".to_string()],
            }
        );
    }

    #[test]
    fn last_attempt_within_budget_still_recovers() {
        let saved = vec![saved(r"\\.\DISPLAY1"), saved(r"\\.\DISPLAY2")];
        let live = names(&[r"\\.\DISPLAY1"]);
        assert_eq!(
            decide_recovery(&saved, &live, MAX_RECOVERY_ATTEMPTS - 1, true),
            RecoveryDecision::Recover {
                missing: vec![r"\\.\DISPLAY2".to_string()],
                attempt: MAX_RECOVERY_ATTEMPTS,
            }
        );
    }

    #[test]
    fn disabled_setting_short_circuits_even_with_missing_monitors() {
        let saved = vec![saved(r"\\.\DISPLAY1"), saved(r"\\.\DISPLAY2")];
        let live = names(&[r"\\.\DISPLAY1"]);
        assert_eq!(
            decide_recovery(&saved, &live, 0, false),
            RecoveryDecision::Disabled
        );
    }

    #[test]
    fn no_saved_topology_is_a_no_op() {
        let live = names(&[r"\\.\DISPLAY1"]);
        assert_eq!(
            decide_recovery(&[], &live, 0, true),
            RecoveryDecision::NoSavedTopology
        );
    }
}
