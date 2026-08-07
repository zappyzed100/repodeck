//! Runs `git` against a workset's repository to populate a [`GitStatus`]
//! (branch / changed count / last-commit time). Called off the UI thread (the
//! Quick Switcher caches the result and refreshes it in the background), so a
//! slow or missing repository never blocks the switcher from opening.
//!
//! Parsing lives in `domain::git`; this module only owns the process calls and
//! their Windows-specific "don't flash a console window" flag.

use std::path::Path;
use std::process::Command;

use crate::domain::git::{GitStatus, clean_branch, parse_ahead_behind, parse_changed_count};

/// `CREATE_NO_WINDOW` — keeps `git.exe` from popping a console window each time
/// it runs (these calls happen silently in the background).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Runs `git` with `args` in `dir`, returning trimmed stdout on a clean exit,
/// or `None` if git is missing, the directory isn't a repo, or it exits
/// non-zero.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Reads `path`'s current git status. A non-repository (or unavailable git)
/// yields `GitStatus::default()` (`is_git == false`); an empty repository yields
/// `is_git == true` with `last_commit_at == None`.
pub fn fetch(path: &Path) -> GitStatus {
    // `rev-parse` doubles as the "is this a git work tree?" probe: if it fails,
    // there is nothing else worth asking.
    let Some(branch_raw) = git(path, &["rev-parse", "--abbrev-ref", "HEAD"]) else {
        return GitStatus::default();
    };

    let changed_count = git(path, &["status", "--porcelain"])
        .map(|out| parse_changed_count(&out))
        .unwrap_or(0);

    // `%cI` is strict ISO 8601 (committer date), which `time`'s RFC 3339 parser
    // accepts. Absent (empty repo) or unreadable → None.
    let last_commit_at = git(path, &["log", "-1", "--format=%cI"])
        .map(|out| out.trim().to_string())
        .filter(|s| !s.is_empty());

    // Unpushed / behind counts, only meaningful with an upstream. The symmetric
    // `--left-right --count @{upstream}...HEAD` fails (→ None) when the branch
    // has no upstream, which is exactly when there's nothing to report.
    let (has_upstream, ahead, behind) = match git(
        path,
        &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
    ) {
        Some(out) => {
            let (ahead, behind) = parse_ahead_behind(&out);
            (true, ahead, behind)
        }
        None => (false, 0, 0),
    };

    GitStatus {
        is_git: true,
        branch: clean_branch(&branch_raw),
        changed_count,
        last_commit_at,
        has_upstream,
        ahead,
        behind,
    }
}
