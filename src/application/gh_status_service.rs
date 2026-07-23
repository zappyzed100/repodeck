//! Best-effort GitHub status for a repository — open pull requests and the
//! latest CI conclusion — read via the `gh` CLI. This is the "開いたままのPR"
//! and "CI失敗" data for the home-screen dashboard.
//!
//! Everything here is best-effort and must never block the UI: `gh` talks to
//! the network, so each call runs with a short timeout and any failure (gh not
//! installed, not authenticated, no network, not a GitHub remote) collapses to
//! an empty [`GhStatus`] with `gh_available == false`. Callers cache the result
//! and refresh it off the UI thread, exactly like [`git_status_service`].

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// `CREATE_NO_WINDOW` — keep `gh.exe` from flashing a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How long any single `gh` invocation may take before it's killed and treated
/// as unavailable. Network calls that hang must not stall a background refresh.
const GH_TIMEOUT: Duration = Duration::from_secs(5);

/// Latest CI conclusion for a branch (the dashboard's ✓/✕/●/— column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CiState {
    /// No workflow runs found (or CI not configured) — 未実行.
    #[default]
    None,
    /// Latest run passed — 成功.
    Success,
    /// Latest run failed / was cancelled / timed out — 失敗.
    Failure,
    /// Latest run is queued or in progress — 実行中.
    Running,
    /// A run exists but its conclusion isn't one we classify.
    Unknown,
}

impl CiState {
    /// The dashboard symbol + Japanese label (matches the agent-status glyphs).
    pub fn symbol_label(self) -> (&'static str, &'static str) {
        match self {
            CiState::Success => ("✓", "成功"),
            CiState::Failure => ("✕", "失敗"),
            CiState::Running => ("●", "実行中"),
            CiState::None => ("—", "未実行"),
            CiState::Unknown => ("?", "不明"),
        }
    }
}

/// A pull request reference (the current branch's open PR, if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRef {
    pub number: u32,
    pub title: String,
}

/// GitHub-side status for one repository. `gh_available == false` means `gh`
/// couldn't answer at all (not installed / not authed / not a GitHub repo /
/// timed out), in which case the counts are 0 and `ci == None`.
#[derive(Debug, Clone, Default)]
pub struct GhStatus {
    pub gh_available: bool,
    /// Number of open PRs in the repo (the "開いたままのPR" figure).
    pub open_pr_count: u32,
    /// The open PR whose head branch is the checked-out `branch`, if any.
    pub current_branch_pr: Option<PrRef>,
    /// Latest CI conclusion for `branch`.
    pub ci: CiState,
}

#[derive(Deserialize)]
struct GhPr {
    number: u32,
    title: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
}

#[derive(Deserialize)]
struct GhRun {
    status: String,
    conclusion: Option<String>,
}

/// Fetches open-PR and CI status for the repo at `dir`. `branch` is the current
/// branch (used to pick out its PR and scope the CI query); pass `None` to skip
/// branch-specific narrowing. Best-effort — see the module docs.
pub fn fetch(dir: &Path, branch: Option<&str>) -> GhStatus {
    // One probe doubles as the availability check: if listing PRs fails, `gh`
    // can't answer for this repo and there's nothing else worth asking.
    let Some(pr_json) = gh(
        dir,
        &[
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            "50",
            "--json",
            "number,title,headRefName",
        ],
    ) else {
        return GhStatus::default();
    };

    let prs: Vec<GhPr> = serde_json::from_str(pr_json.trim()).unwrap_or_default();
    let current_branch_pr = branch.and_then(|b| {
        prs.iter().find(|p| p.head_ref_name == b).map(|p| PrRef {
            number: p.number,
            title: p.title.clone(),
        })
    });

    GhStatus {
        gh_available: true,
        open_pr_count: prs.len().try_into().unwrap_or(u32::MAX),
        current_branch_pr,
        ci: fetch_ci(dir, branch),
    }
}

/// Latest CI conclusion for `branch` (or the repo's default if `branch` is
/// `None`), via `gh run list`.
fn fetch_ci(dir: &Path, branch: Option<&str>) -> CiState {
    let mut args = vec!["run", "list", "--limit", "1", "--json", "status,conclusion"];
    if let Some(b) = branch {
        args.push("--branch");
        args.push(b);
    }
    let Some(json) = gh(dir, &args) else {
        return CiState::None;
    };
    let runs: Vec<GhRun> = serde_json::from_str(json.trim()).unwrap_or_default();
    let Some(run) = runs.first() else {
        return CiState::None;
    };
    classify_ci(&run.status, run.conclusion.as_deref())
}

/// Maps a `gh run` `(status, conclusion)` to a [`CiState`].
fn classify_ci(status: &str, conclusion: Option<&str>) -> CiState {
    if status != "completed" {
        // queued / in_progress / requested / waiting
        return CiState::Running;
    }
    match conclusion {
        Some("success") => CiState::Success,
        Some("failure" | "timed_out" | "cancelled" | "action_required" | "startup_failure") => {
            CiState::Failure
        }
        _ => CiState::Unknown,
    }
}

/// Runs `gh args` in `dir`, returning trimmed stdout on a clean exit, or `None`
/// on any failure — including exceeding [`GH_TIMEOUT`], in which case the child
/// is killed. Output is small (a few PRs / one run as JSON), well within the
/// pipe buffer, so reading after exit can't deadlock.
fn gh(dir: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("gh");
    command
        .current_dir(dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().ok()?;

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return status.success().then_some(out);
            }
            Ok(None) => {
                if start.elapsed() > GH_TIMEOUT {
                    let _ = child.kill();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_ci_covers_the_states() {
        assert_eq!(classify_ci("in_progress", None), CiState::Running);
        assert_eq!(classify_ci("queued", None), CiState::Running);
        assert_eq!(classify_ci("completed", Some("success")), CiState::Success);
        assert_eq!(classify_ci("completed", Some("failure")), CiState::Failure);
        assert_eq!(
            classify_ci("completed", Some("timed_out")),
            CiState::Failure
        );
        assert_eq!(
            classify_ci("completed", Some("cancelled")),
            CiState::Failure
        );
        assert_eq!(classify_ci("completed", Some("skipped")), CiState::Unknown);
        assert_eq!(classify_ci("completed", None), CiState::Unknown);
    }

    #[test]
    fn ci_symbols_match_the_dashboard() {
        assert_eq!(CiState::Success.symbol_label(), ("✓", "成功"));
        assert_eq!(CiState::Failure.symbol_label(), ("✕", "失敗"));
        assert_eq!(CiState::Running.symbol_label(), ("●", "実行中"));
        assert_eq!(CiState::None.symbol_label(), ("—", "未実行"));
    }
}
