//! Codex agent lifecycle state (PLAN.md §6.1-§6.2, §6.7). Pure domain types
//! only — no Slint, no Win32, no `ipc`-layer types (this module sits below
//! `application`/`ipc` in the dependency order, PLAN.md §9.3).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One Codex session/turn's last-known lifecycle state (PLAN.md §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Running,
    NeedsInput,
    Ready,
    Blocked,
    Unknown,
}

/// One Codex session+turn's tracked run (PLAN.md §6.2, §6.3, §6.7). Not
/// specified as a concrete shape anywhere in PLAN.md beyond `AgentState`
/// itself; this is Phase 8's own design, built to satisfy §6.2's aggregation
/// and §3.3/§6.7's "ready確認処理" (ready-confirmation) requirements.
///
/// `workset_id` is never `Option<Uuid>`: an event that doesn't resolve to a
/// registered workset never becomes an `AgentRun` at all (PLAN.md §6.6 —
/// "一致しないイベントを適当なセットへ割り当てない") — it becomes an
/// `application::agent_status_service::UnmatchedAgentEvent` instead, which
/// never gets promoted into an `AgentRun` later even if a matching workset
/// is registered afterward.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRun {
    pub session_id: String,
    pub turn_id: String,
    pub workset_id: Uuid,
    pub state: AgentState,
    pub model: Option<String>,
    /// Set once a `Ready` run has been surfaced to the user by successfully
    /// switching to its workset (PLAN.md §3.3 "選択後"). A `Ready` run with
    /// `confirmed == false` is what makes [`aggregate_state`] return `Ready`;
    /// once confirmed it behaves like `Idle` in aggregation without actually
    /// rewriting `state`, so the run's own history ("this was a completion,
    /// not just idle from the start") stays inspectable.
    pub confirmed: bool,
    /// RFC 3339 UTC, set once at `run_started`, never mutated afterward.
    pub started_at: String,
    /// RFC 3339 UTC, updated on every event this run receives. The Quick
    /// Switcher's "経過時間" column is computed from this at display time,
    /// not stored as a duration.
    pub last_transition_at: String,
}

/// PLAN.md §6.2's 6-step aggregation priority, applied to one workset's
/// runs. Pure and allocation-free; callers pre-filter `runs` to one
/// workset's own records.
pub fn aggregate_state(runs: &[&AgentRun]) -> AgentState {
    if runs.is_empty() {
        return AgentState::Unknown; // step 6: no information at all.
    }
    if runs.iter().any(|r| r.state == AgentState::NeedsInput) {
        return AgentState::NeedsInput; // step 1
    }
    if runs.iter().any(|r| r.state == AgentState::Blocked) {
        return AgentState::Blocked; // step 2
    }
    if runs.iter().any(|r| r.state == AgentState::Running) {
        return AgentState::Running; // step 3
    }
    if runs
        .iter()
        .any(|r| r.state == AgentState::Ready && !r.confirmed)
    {
        return AgentState::Ready; // step 4
    }
    AgentState::Idle // step 5: everything left is confirmed-Ready or Idle.
}

/// Hex color for a state's badge/tray-icon-variant lookup (PLAN.md §6.1's
/// color table).
pub fn state_color(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "#8e8e93",
        AgentState::Running => "#0a84ff",
        AgentState::NeedsInput => "#ffd60a",
        AgentState::Ready => "#30d158",
        AgentState::Blocked => "#ff453a",
        AgentState::Unknown => "#f5f5f7",
    }
}

/// PLAN.md §3.3's row-display priority (needs_input > ready > blocked >
/// running > idle > unknown). Higher number sorts first. Deliberately a
/// *different* function from [`tray_priority`] — the doc states both orders
/// verbatim, and `Ready`/`Blocked` swap position between them; conflating
/// the two would silently violate one or the other.
pub fn row_display_priority(state: AgentState) -> u8 {
    match state {
        AgentState::NeedsInput => 5,
        AgentState::Ready => 4,
        AgentState::Blocked => 3,
        AgentState::Running => 2,
        AgentState::Idle => 1,
        AgentState::Unknown => 0,
    }
}

/// PLAN.md §6.7's tray-icon-color priority (needs_input > blocked > ready >
/// running > idle). Higher number wins. See [`row_display_priority`]'s doc
/// comment for why this isn't the same function.
pub fn tray_priority(state: AgentState) -> u8 {
    match state {
        AgentState::NeedsInput => 5,
        AgentState::Blocked => 4,
        AgentState::Ready => 3,
        AgentState::Running => 2,
        AgentState::Idle => 1,
        AgentState::Unknown => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(state: AgentState, confirmed: bool) -> AgentRun {
        AgentRun {
            session_id: "s".to_string(),
            turn_id: "t".to_string(),
            workset_id: Uuid::new_v4(),
            state,
            model: None,
            confirmed,
            started_at: "2026-07-20T00:00:00Z".to_string(),
            last_transition_at: "2026-07-20T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn empty_runs_aggregate_to_unknown() {
        assert_eq!(aggregate_state(&[]), AgentState::Unknown);
    }

    #[test]
    fn needs_input_wins_over_everything_else() {
        let a = run(AgentState::Blocked, false);
        let b = run(AgentState::NeedsInput, false);
        let c = run(AgentState::Running, false);
        assert_eq!(aggregate_state(&[&a, &b, &c]), AgentState::NeedsInput);
    }

    #[test]
    fn blocked_wins_over_running_and_ready() {
        let a = run(AgentState::Running, false);
        let b = run(AgentState::Blocked, false);
        assert_eq!(aggregate_state(&[&a, &b]), AgentState::Blocked);
    }

    #[test]
    fn running_wins_over_unconfirmed_ready() {
        let a = run(AgentState::Ready, false);
        let b = run(AgentState::Running, false);
        assert_eq!(aggregate_state(&[&a, &b]), AgentState::Running);
    }

    #[test]
    fn unconfirmed_ready_wins_over_idle() {
        let a = run(AgentState::Idle, true);
        let b = run(AgentState::Ready, false);
        assert_eq!(aggregate_state(&[&a, &b]), AgentState::Ready);
    }

    #[test]
    fn confirmed_ready_counts_as_idle() {
        let a = run(AgentState::Ready, true);
        assert_eq!(aggregate_state(&[&a]), AgentState::Idle);
    }

    #[test]
    fn all_idle_aggregates_to_idle() {
        let a = run(AgentState::Idle, true);
        let b = run(AgentState::Idle, true);
        assert_eq!(aggregate_state(&[&a, &b]), AgentState::Idle);
    }

    #[test]
    fn row_and_tray_priority_disagree_on_ready_vs_blocked() {
        assert!(
            row_display_priority(AgentState::Ready) > row_display_priority(AgentState::Blocked)
        );
        assert!(tray_priority(AgentState::Blocked) > tray_priority(AgentState::Ready));
    }
}
