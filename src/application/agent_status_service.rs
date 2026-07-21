//! Stateful orchestration of Codex agent events (PLAN.md §6.2, §6.3, §6.6,
//! §3.3's "ready確認処理"). `ipc::protocol` stays a stateless per-event
//! adapter; everything here that depends on *previous* events — matching a
//! `cwd` to a registered workset, updating an existing run vs. creating a
//! new one, "only resume from `NeedsInput`" — lives in this module instead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::application::workset_service::find_git_root;
use crate::domain::agent::{AgentRun, AgentState, aggregate_state};
use crate::domain::workset::Workset;
use crate::ipc::protocol::{NormalizedEvent, NormalizedEventKind};
use crate::persistence::runtime_store::{self, RuntimeStoreError};

/// How long an event that didn't resolve to any registered workset stays
/// around in memory before being dropped (PLAN.md §6.6: never guess a
/// workset for it, not even later if a matching one gets registered).
const UNMATCHED_RETENTION: Duration = Duration::from_secs(30 * 60);

/// An agent event whose `cwd` didn't map to any registered workset
/// (PLAN.md §6.6). In-memory only — never persisted, and never promoted
/// into an `AgentRun` even if a matching workset is registered afterward.
#[derive(Debug, Clone)]
pub struct UnmatchedAgentEvent {
    pub cwd: PathBuf,
    pub event: NormalizedEventKind,
    pub received_at: Instant,
}

/// Resolves a Codex hook event's `cwd` to a registered workset (PLAN.md
/// §6.6): an exact match on the git root wins first, then "`event_cwd` is a
/// subdirectory of a workset's `repository_path`" — checked against the
/// original `event_cwd`, not just the resolved git root, so a
/// `RepositoryKind::Directory` workset (no `.git` of its own) still matches.
pub fn map_event_to_workset(worksets: &[Workset], event_cwd: &Path) -> Option<Uuid> {
    if let Some(git_root) = find_git_root(event_cwd)
        && let Some(workset) = worksets.iter().find(|w| w.repository_path == git_root)
    {
        return Some(workset.id);
    }

    worksets
        .iter()
        .find(|w| event_cwd.starts_with(&w.repository_path))
        .map(|w| w.id)
}

/// Applies one normalized event to persisted runtime state (PLAN.md §6.3's
/// state machine), returning the affected workset id, or `Ok(None)` if the
/// event's `cwd` didn't match any registered workset (pushed into
/// `unmatched` instead).
pub fn apply_event(
    data_dir: &Path,
    worksets: &[Workset],
    unmatched: &mut Vec<UnmatchedAgentEvent>,
    event: NormalizedEvent,
) -> Result<Option<Uuid>, RuntimeStoreError> {
    prune_expired_unmatched(unmatched);

    let event_cwd = PathBuf::from(&event.cwd);
    let Some(workset_id) = map_event_to_workset(worksets, &event_cwd) else {
        unmatched.push(UnmatchedAgentEvent {
            cwd: event_cwd,
            event: event.event,
            received_at: Instant::now(),
        });
        return Ok(None);
    };

    let mut state = runtime_store::load(data_dir);

    match event.event {
        NormalizedEventKind::RunStarted => {
            upsert_run(
                &mut state.agent_runs,
                workset_id,
                &event,
                AgentState::Running,
            );
        }
        NormalizedEventKind::NeedsInput => {
            upsert_run(
                &mut state.agent_runs,
                workset_id,
                &event,
                AgentState::NeedsInput,
            );
        }
        NormalizedEventKind::ToolUseObserved => {
            // Only a resume from a pending-input state counts as a real
            // transition (PLAN.md §6.3); a stray `PostToolUse` with no
            // matching run, or one that isn't currently `NeedsInput`,
            // silently no-ops.
            if let Some(run) =
                find_run_mut(&mut state.agent_runs, &event.session_id, &event.turn_id)
                && run.state == AgentState::NeedsInput
            {
                run.state = AgentState::Running;
                run.last_transition_at = event.occurred_at.clone();
            }
        }
        NormalizedEventKind::RunCompleted => {
            upsert_run(&mut state.agent_runs, workset_id, &event, AgentState::Ready);
            if let Some(run) =
                find_run_mut(&mut state.agent_runs, &event.session_id, &event.turn_id)
            {
                run.confirmed = false;
            }
        }
    }

    runtime_store::save(data_dir, &state)?;
    Ok(Some(workset_id))
}

/// PLAN.md §3.3's "選択後" / "ready確認処理": marks every unconfirmed
/// `Ready` run belonging to `workset_id` as confirmed. Called after a
/// successful switch to that workset; the caller's next UI refresh
/// re-aggregates from scratch, so no special-casing is needed here for
/// "another agent is still running in the same repo".
pub fn confirm_ready_and_recompute(
    data_dir: &Path,
    workset_id: Uuid,
) -> Result<(), RuntimeStoreError> {
    let mut state = runtime_store::load(data_dir);
    for run in &mut state.agent_runs {
        if run.workset_id == workset_id && run.state == AgentState::Ready && !run.confirmed {
            run.confirmed = true;
        }
    }
    runtime_store::save(data_dir, &state)
}

/// Groups `runs` by workset once, then aggregates each registered workset's
/// state (defaulting to `Unknown` for a workset with zero runs). Used by
/// both the Quick Switcher row refresh and the tray-state computation to
/// avoid an O(worksets × runs) rescan.
pub fn aggregate_all(worksets: &[Workset], runs: &[AgentRun]) -> HashMap<Uuid, AgentState> {
    let mut by_workset: HashMap<Uuid, Vec<&AgentRun>> = HashMap::new();
    for run in runs {
        by_workset.entry(run.workset_id).or_default().push(run);
    }

    worksets
        .iter()
        .map(|w| {
            let workset_runs = by_workset.get(&w.id).map(Vec::as_slice).unwrap_or(&[]);
            (w.id, aggregate_state(workset_runs))
        })
        .collect()
}

fn find_run_mut<'a>(
    runs: &'a mut [AgentRun],
    session_id: &str,
    turn_id: &str,
) -> Option<&'a mut AgentRun> {
    runs.iter_mut()
        .find(|r| r.session_id == session_id && r.turn_id == turn_id)
}

/// Idempotent on repeated `session_id`+`turn_id`: refreshes the existing
/// run's state/timestamp/model instead of duplicating it.
fn upsert_run(
    runs: &mut Vec<AgentRun>,
    workset_id: Uuid,
    event: &NormalizedEvent,
    state: AgentState,
) {
    if let Some(run) = find_run_mut(runs, &event.session_id, &event.turn_id) {
        run.state = state;
        run.last_transition_at = event.occurred_at.clone();
        if event.model.is_some() {
            run.model = event.model.clone();
        }
    } else {
        runs.push(AgentRun {
            session_id: event.session_id.clone(),
            turn_id: event.turn_id.clone(),
            workset_id,
            state,
            model: event.model.clone(),
            confirmed: false,
            started_at: event.occurred_at.clone(),
            last_transition_at: event.occurred_at.clone(),
        });
    }
}

fn prune_expired_unmatched(unmatched: &mut Vec<UnmatchedAgentEvent>) {
    unmatched.retain(|u| u.received_at.elapsed() < UNMATCHED_RETENTION);
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::domain::workset::RepositoryKind;

    fn workset_at(repository_path: &Path) -> Workset {
        crate::application::workset_service::build_workset(
            "test".to_string(),
            "#fff".to_string(),
            repository_path.to_path_buf(),
            RepositoryKind::Directory,
            0,
            Vec::new(),
        )
    }

    fn event(
        kind: NormalizedEventKind,
        cwd: &Path,
        session_id: &str,
        turn_id: &str,
    ) -> NormalizedEvent {
        NormalizedEvent {
            schema_version: 1,
            source: "codex".to_string(),
            event: kind,
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            cwd: cwd.to_string_lossy().into_owned(),
            model: None,
            occurred_at: "2026-07-21T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn map_event_to_workset_matches_exact_repository_path() {
        let repo = PathBuf::from(r"D:\repos\a");
        let worksets = [workset_at(&repo)];
        assert_eq!(map_event_to_workset(&worksets, &repo), Some(worksets[0].id));
    }

    #[test]
    fn map_event_to_workset_matches_a_subdirectory() {
        let repo = PathBuf::from(r"D:\repos\a");
        let worksets = [workset_at(&repo)];
        let sub = repo.join("src");
        assert_eq!(map_event_to_workset(&worksets, &sub), Some(worksets[0].id));
    }

    #[test]
    fn map_event_to_workset_returns_none_for_an_unregistered_cwd() {
        let worksets = [workset_at(&PathBuf::from(r"D:\repos\a"))];
        assert_eq!(
            map_event_to_workset(&worksets, &PathBuf::from(r"D:\repos\b")),
            None
        );
    }

    #[test]
    fn apply_event_with_unmatched_cwd_is_recorded_and_not_persisted() {
        let dir = tempdir().unwrap();
        let worksets = [workset_at(&PathBuf::from(r"D:\repos\a"))];
        let mut unmatched = Vec::new();

        let result = apply_event(
            dir.path(),
            &worksets,
            &mut unmatched,
            event(
                NormalizedEventKind::RunStarted,
                &PathBuf::from(r"D:\repos\other"),
                "s1",
                "t1",
            ),
        )
        .unwrap();

        assert_eq!(result, None);
        assert_eq!(unmatched.len(), 1);
        assert!(runtime_store::load(dir.path()).agent_runs.is_empty());
    }

    #[test]
    fn duplicate_run_started_events_do_not_duplicate_the_run() {
        let dir = tempdir().unwrap();
        let repo = PathBuf::from(r"D:\repos\a");
        let worksets = [workset_at(&repo)];
        let mut unmatched = Vec::new();

        for _ in 0..2 {
            apply_event(
                dir.path(),
                &worksets,
                &mut unmatched,
                event(NormalizedEventKind::RunStarted, &repo, "s1", "t1"),
            )
            .unwrap();
        }

        let state = runtime_store::load(dir.path());
        assert_eq!(state.agent_runs.len(), 1);
        assert_eq!(state.agent_runs[0].state, AgentState::Running);
    }

    #[test]
    fn full_lifecycle_needs_input_to_running_to_ready_to_confirmed() {
        let dir = tempdir().unwrap();
        let repo = PathBuf::from(r"D:\repos\a");
        let worksets = [workset_at(&repo)];
        let mut unmatched = Vec::new();

        let workset_id = apply_event(
            dir.path(),
            &worksets,
            &mut unmatched,
            event(NormalizedEventKind::RunStarted, &repo, "s1", "t1"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            runtime_store::load(dir.path()).agent_runs[0].state,
            AgentState::Running
        );

        apply_event(
            dir.path(),
            &worksets,
            &mut unmatched,
            event(NormalizedEventKind::NeedsInput, &repo, "s1", "t1"),
        )
        .unwrap();
        assert_eq!(
            runtime_store::load(dir.path()).agent_runs[0].state,
            AgentState::NeedsInput
        );

        // A stray PostToolUse while NOT NeedsInput must no-op; verify the
        // resume path only fires from NeedsInput by first confirming the
        // transition below actually happens.
        apply_event(
            dir.path(),
            &worksets,
            &mut unmatched,
            event(NormalizedEventKind::ToolUseObserved, &repo, "s1", "t1"),
        )
        .unwrap();
        assert_eq!(
            runtime_store::load(dir.path()).agent_runs[0].state,
            AgentState::Running
        );

        apply_event(
            dir.path(),
            &worksets,
            &mut unmatched,
            event(NormalizedEventKind::RunCompleted, &repo, "s1", "t1"),
        )
        .unwrap();
        let state = runtime_store::load(dir.path());
        assert_eq!(state.agent_runs[0].state, AgentState::Ready);
        assert!(!state.agent_runs[0].confirmed);
        assert_eq!(
            aggregate_all(&worksets, &state.agent_runs)[&workset_id],
            AgentState::Ready
        );

        confirm_ready_and_recompute(dir.path(), workset_id).unwrap();
        let state = runtime_store::load(dir.path());
        assert!(state.agent_runs[0].confirmed);
        assert_eq!(
            aggregate_all(&worksets, &state.agent_runs)[&workset_id],
            AgentState::Idle
        );
    }

    #[test]
    fn tool_use_observed_is_a_no_op_when_not_pending_input() {
        let dir = tempdir().unwrap();
        let repo = PathBuf::from(r"D:\repos\a");
        let worksets = [workset_at(&repo)];
        let mut unmatched = Vec::new();

        apply_event(
            dir.path(),
            &worksets,
            &mut unmatched,
            event(NormalizedEventKind::RunStarted, &repo, "s1", "t1"),
        )
        .unwrap();
        apply_event(
            dir.path(),
            &worksets,
            &mut unmatched,
            event(NormalizedEventKind::ToolUseObserved, &repo, "s1", "t1"),
        )
        .unwrap();

        assert_eq!(
            runtime_store::load(dir.path()).agent_runs[0].state,
            AgentState::Running
        );
    }

    #[test]
    fn aggregate_all_defaults_worksets_with_no_runs_to_unknown() {
        let worksets = [workset_at(&PathBuf::from(r"D:\repos\a"))];
        let aggregates = aggregate_all(&worksets, &[]);
        assert_eq!(aggregates[&worksets[0].id], AgentState::Unknown);
    }
}
