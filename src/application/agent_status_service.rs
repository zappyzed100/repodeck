//! Stateful orchestration of Codex agent events (PLAN.md §6.2, §6.3, §6.6,
//! §3.3's "ready確認処理"). `ipc::protocol` stays a stateless per-event
//! adapter; everything here that depends on *previous* events — matching a
//! `cwd` to a registered workset, updating an existing run vs. creating a
//! new one, "only resume from `NeedsInput`" — lives in this module instead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::application::workset_service::{self, find_git_root};
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
/// subdirectory of a workset's match path" — checked against the original
/// `event_cwd`, not just the resolved git root, so a `RepositoryKind::
/// Directory` workset (no `.git` of its own) still matches. Both branches
/// compare against `resolve_match_path`, not the raw `repository_path`,
/// since a `RepositoryKind::Workspace` workset's `repository_path` points at
/// its `.code-workspace` file rather than a directory a `cwd` could ever be
/// "under".
pub fn map_event_to_workset(worksets: &[Workset], event_cwd: &Path) -> Option<Uuid> {
    if let Some(git_root) = find_git_root(event_cwd)
        && let Some(workset) = worksets.iter().find(|w| {
            workset_service::resolve_match_path(&w.repository_path, w.repository_kind) == git_root
        })
    {
        return Some(workset.id);
    }

    worksets
        .iter()
        .find(|w| {
            let match_path =
                workset_service::resolve_match_path(&w.repository_path, w.repository_kind);
            // `Path::starts_with("")` は常に true なので、`repository_path` が
            // 空のセット（ブラウザやデスクトップアプリだけを束ねた、リポジトリ
            // を持たないセット）を弾かないと、そのセットが *あらゆる* cwd の
            // イベントを総取りしてしまう。登録順で最初に現れた空パスのセットに
            // 無関係なエージェントの実行状態が表示される
            // （ユーザー報告 2026-07-24）。空パスは「照合先を持たない」の意。
            !match_path.as_os_str().is_empty() && event_cwd.starts_with(&match_path)
        })
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
        // どのセットにも結びつかないイベントは無音で捨てられていた。
        // 「実装中なのにバッジが出ない/消えた」を追うには、少なくとも
        // 「イベントは来たが cwd がどのセットにも一致しなかった」ことが
        // 見えている必要がある(セットの repository_path 設定漏れの典型)。
        tracing::info!(
            target: "agents",
            source = %event.source,
            event = ?event.event,
            cwd = %event.cwd,
            session = %short_id(&event.session_id),
            "agent event did not match any workset (cwd not under any registered set)"
        );
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
            // ツール使用は「また動いている」の合図。`NeedsInput`（入力待ちから
            // 再開）に加えて `Ready` からも `Running` へ戻す。Claude Code は
            // ターン終了ごとに `Stop` を撃つため、一度 `Ready`（成功）になった後も
            // サブエージェント待ちや自律的な複数ターン作業で活動が続く。ここで
            // `Ready` から戻さないと、実際は動いているのに「成功」のまま張り付き、
            // 次のユーザープロンプトまで直らない（ユーザー報告 2026-07-24）。
            // 対応する run が無い野良 `PostToolUse` や、既に `Running` のものは
            // 従来どおり何もしない。
            if let Some(run) =
                find_run_mut(&mut state.agent_runs, &event.session_id, &event.turn_id)
                && matches!(run.state, AgentState::NeedsInput | AgentState::Ready)
            {
                // `Ready` からの復帰は「完了」が早すぎただけで、作業自体は
                // 途切れていない。ここで `last_transition_at`（UI の経過時間の
                // 起点）を打ち直すと、実行中の表示が何度も 0 に戻る——PC が
                // スリープから復帰した直後に届く `PostToolUse` で、それまでの
                // 計算時間が丸ごと消える（ユーザー報告 2026-07-24）。
                // 一方 `NeedsInput` からの復帰は、人間の返事を待っていた区間が
                // 終わった合図なので、そこからを計算時間として数え直す。
                if run.state == AgentState::NeedsInput {
                    run.last_transition_at = event.occurred_at.clone();
                }
                run.state = AgentState::Running;
                run.confirmed = false;
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

    // 適用後のこの run の状態を残す。バッジは run.state と confirmed から
    // 決まるので、この2つが分かればバッジの挙動を後追いできる。
    let outcome = find_run_mut(&mut state.agent_runs, &event.session_id, &event.turn_id)
        .map(|run| (run.state, run.confirmed));
    match outcome {
        Some((run_state, confirmed)) => tracing::info!(
            target: "agents",
            source = %event.source,
            event = ?event.event,
            session = %short_id(&event.session_id),
            run_state = ?run_state,
            confirmed,
            "agent event applied"
        ),
        None => tracing::info!(
            target: "agents",
            source = %event.source,
            event = ?event.event,
            session = %short_id(&event.session_id),
            "agent event matched a workset but changed no run (e.g. a stray PostToolUse)"
        ),
    }

    runtime_store::save(data_dir, &state)?;
    Ok(Some(workset_id))
}

/// ログを読みやすくするための session id の短縮 (先頭8文字)。
fn short_id(session_id: &str) -> &str {
    session_id.get(..8).unwrap_or(session_id)
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
        event_at(kind, cwd, session_id, turn_id, "2026-07-21T00:00:00Z")
    }

    fn event_at(
        kind: NormalizedEventKind,
        cwd: &Path,
        session_id: &str,
        turn_id: &str,
        occurred_at: &str,
    ) -> NormalizedEvent {
        NormalizedEvent {
            schema_version: 1,
            source: "codex".to_string(),
            event: kind,
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            cwd: cwd.to_string_lossy().into_owned(),
            model: None,
            occurred_at: occurred_at.to_string(),
        }
    }

    #[test]
    fn map_event_to_workset_matches_exact_repository_path() {
        let repo = PathBuf::from(r"D:\repos\a");
        let worksets = [workset_at(&repo)];
        assert_eq!(map_event_to_workset(&worksets, &repo), Some(worksets[0].id));
    }

    #[test]
    fn a_workset_without_a_repository_path_never_matches_an_event() {
        // リポジトリを持たないセット（ブラウザだけのセット等）は照合先が無い。
        // 空パスを許すと `starts_with("")` が全部 true になり、無関係な
        // エージェントの実行状態がそのセットに出てしまう。
        let mut browser_only = workset_at(&PathBuf::from(r"D:\repos\a"));
        browser_only.repository_path = PathBuf::new();
        browser_only.repository_kind = RepositoryKind::Directory;
        let worksets = [browser_only];

        assert_eq!(
            map_event_to_workset(&worksets, &PathBuf::from(r"C:\code\unrelated")),
            None
        );
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

    /// A `RepositoryKind::Workspace` workset's `repository_path` is the
    /// `.code-workspace` file itself, not a directory — this guards against
    /// regressing back to comparing `event_cwd` against that literal file
    /// path (which could never match, since a directory can't "start with" a
    /// file — the bug this test was added to catch).
    #[test]
    fn map_event_to_workset_matches_a_workspace_kind_via_its_underlying_folder() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let ws_path = dir.path().join("proj.code-workspace");
        std::fs::write(&ws_path, r#"{"folders": [{"path": "repo"}]}"#).unwrap();

        let workset = crate::application::workset_service::build_workset(
            "test".to_string(),
            "#fff".to_string(),
            ws_path,
            RepositoryKind::Workspace,
            0,
            Vec::new(),
        );
        let sub = repo.join("src");

        assert_eq!(
            map_event_to_workset(std::slice::from_ref(&workset), &repo),
            Some(workset.id)
        );
        assert_eq!(
            map_event_to_workset(std::slice::from_ref(&workset), &sub),
            Some(workset.id)
        );
    }

    #[test]
    fn map_event_to_workset_ignores_a_workspace_kind_whose_file_is_gone() {
        let dir = tempdir().unwrap();
        let ws_path = dir.path().join("gone.code-workspace");
        let workset = crate::application::workset_service::build_workset(
            "test".to_string(),
            "#fff".to_string(),
            ws_path.clone(),
            RepositoryKind::Workspace,
            0,
            Vec::new(),
        );

        // Falls back to matching against the literal (non-directory) file
        // path, which no real cwd can ever be "under" — so this degrades to
        // "never matches" rather than panicking or matching everything.
        assert_eq!(
            map_event_to_workset(&[workset], &dir.path().join("repo")),
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
    fn tool_use_after_ready_resumes_running() {
        // Claude Codeはターン終了ごとにStopを撃つ。一度Ready（成功）になった後、
        // サブエージェント処理やツール実行で活動が続くなら、PostToolUseで
        // Runningへ戻り「成功のまま張り付く」ことがないようにする。
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
            event(NormalizedEventKind::RunCompleted, &repo, "s1", "t1"),
        )
        .unwrap();
        assert_eq!(
            runtime_store::load(dir.path()).agent_runs[0].state,
            AgentState::Ready
        );

        apply_event(
            dir.path(),
            &worksets,
            &mut unmatched,
            event(NormalizedEventKind::ToolUseObserved, &repo, "s1", "t1"),
        )
        .unwrap();
        let state = runtime_store::load(dir.path());
        assert_eq!(state.agent_runs[0].state, AgentState::Running);
        assert!(!state.agent_runs[0].confirmed);
    }

    #[test]
    fn resuming_from_ready_keeps_the_elapsed_time_origin() {
        // スリープ復帰直後に届く `PostToolUse` で計算時間が 0 に戻らないこと。
        // 作業は Ready を挟んで continuous なので、起点は動かさない。
        let dir = tempdir().unwrap();
        let repo = PathBuf::from(r"D:\repos\a");
        let worksets = [workset_at(&repo)];
        let mut unmatched = Vec::new();

        for kind in [
            NormalizedEventKind::RunStarted,
            NormalizedEventKind::RunCompleted,
        ] {
            apply_event(
                dir.path(),
                &worksets,
                &mut unmatched,
                event_at(kind, &repo, "s1", "t1", "2026-07-24T00:00:00Z"),
            )
            .unwrap();
        }

        apply_event(
            dir.path(),
            &worksets,
            &mut unmatched,
            event_at(
                NormalizedEventKind::ToolUseObserved,
                &repo,
                "s1",
                "t1",
                "2026-07-24T03:45:59Z",
            ),
        )
        .unwrap();

        let run = &runtime_store::load(dir.path()).agent_runs[0];
        assert_eq!(run.state, AgentState::Running);
        assert_eq!(run.last_transition_at, "2026-07-24T00:00:00Z");
    }

    #[test]
    fn resuming_from_needs_input_restarts_the_elapsed_time_origin() {
        // 人間の返事待ちが終わってからが「計算時間」なので、ここは数え直す。
        let dir = tempdir().unwrap();
        let repo = PathBuf::from(r"D:\repos\a");
        let worksets = [workset_at(&repo)];
        let mut unmatched = Vec::new();

        for kind in [
            NormalizedEventKind::RunStarted,
            NormalizedEventKind::NeedsInput,
        ] {
            apply_event(
                dir.path(),
                &worksets,
                &mut unmatched,
                event_at(kind, &repo, "s1", "t1", "2026-07-24T00:00:00Z"),
            )
            .unwrap();
        }

        apply_event(
            dir.path(),
            &worksets,
            &mut unmatched,
            event_at(
                NormalizedEventKind::ToolUseObserved,
                &repo,
                "s1",
                "t1",
                "2026-07-24T03:45:59Z",
            ),
        )
        .unwrap();

        let run = &runtime_store::load(dir.path()).agent_runs[0];
        assert_eq!(run.state, AgentState::Running);
        assert_eq!(run.last_transition_at, "2026-07-24T03:45:59Z");
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
