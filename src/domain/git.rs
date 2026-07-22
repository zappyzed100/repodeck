//! Git working-tree status for a workset's repository, surfaced in the Quick
//! Switcher (branch / changed-file count / last-commit age). Pure domain types
//! and parsers only — the actual `git` invocation lives in
//! `application::git_status_service`, so everything here is unit-testable
//! without a real repository.

/// A repository's last-observed state. `is_git == false` means the path is not
/// a git working tree (or git was unavailable); the other fields are then their
/// defaults and the Quick Switcher shows no git columns for that row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitStatus {
    pub is_git: bool,
    /// Current branch name, or `None` when detached / unreadable.
    pub branch: Option<String>,
    /// Number of changed entries (`git status --porcelain` lines) — staged,
    /// unstaged, and untracked all count once.
    pub changed_count: u32,
    /// The last commit's committer date (RFC 3339 / ISO 8601, `git log %cI`),
    /// or `None` on an empty repository. The relative "最終commit" text is
    /// computed from this at display time, not stored.
    pub last_commit_at: Option<String>,
}

/// Counts changed entries from `git status --porcelain` output: one per
/// non-empty line (each line is a single path's status). Blank lines — e.g. a
/// trailing newline — are ignored.
pub fn parse_changed_count(porcelain: &str) -> u32 {
    porcelain
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

/// Normalizes `git rev-parse --abbrev-ref HEAD` output into a branch name, or
/// `None` for a detached HEAD (git prints the literal `HEAD`) or empty output.
pub fn clean_branch(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "HEAD" {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Japanese relative-age label from a whole-minute delta: `たった今` under a
/// minute, then `N分前` / `N時間前` / `N日前`. A negative delta (clock skew, a
/// commit dated slightly in the future) is treated as "now".
pub fn relative_label(minutes: i64) -> String {
    if minutes < 1 {
        "たった今".to_string()
    } else if minutes < 60 {
        format!("{minutes}分前")
    } else if minutes < 60 * 24 {
        format!("{}時間前", minutes / 60)
    } else {
        format!("{}日前", minutes / (60 * 24))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_count_ignores_blank_lines() {
        let porcelain = " M src/a.rs\n?? new.txt\nA  staged.rs\n";
        assert_eq!(parse_changed_count(porcelain), 3);
        assert_eq!(parse_changed_count(""), 0);
        assert_eq!(parse_changed_count("\n\n"), 0);
    }

    #[test]
    fn branch_is_none_when_detached_or_empty() {
        assert_eq!(clean_branch("main\n"), Some("main".to_string()));
        assert_eq!(clean_branch("  feature/ui  "), Some("feature/ui".to_string()));
        assert_eq!(clean_branch("HEAD\n"), None);
        assert_eq!(clean_branch(""), None);
    }

    #[test]
    fn relative_label_buckets_by_unit() {
        assert_eq!(relative_label(0), "たった今");
        assert_eq!(relative_label(-5), "たった今");
        assert_eq!(relative_label(18), "18分前");
        assert_eq!(relative_label(59), "59分前");
        assert_eq!(relative_label(60), "1時間前");
        assert_eq!(relative_label(150), "2時間前");
        assert_eq!(relative_label(60 * 24), "1日前");
        assert_eq!(relative_label(60 * 24 * 3), "3日前");
    }
}
