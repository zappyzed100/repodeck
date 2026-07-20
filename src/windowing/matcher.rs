//! Re-matches a registered [`WindowMatcher`] against freshly enumerated windows
//! (PLAN.md §5.3-§5.6). Pure comparison logic; no Win32 calls of its own — it
//! only reads [`TopLevelWindow`] values that `enumerate` already collected.

use std::collections::HashSet;

use crate::domain::workset::WindowMatcher;
use crate::windowing::enumerate::TopLevelWindow;

const AUTO_REBIND_THRESHOLD: i32 = 75;
const AUTO_REBIND_MARGIN: i32 = 20;
const TITLE_SIMILARITY_THRESHOLD: f64 = 0.8;
const VSCODE_TITLE_SUFFIX: &str = "- Visual Studio Code";

/// Normalizes a window title for comparison (PLAN.md §5.5):
/// strips VS Code's unsaved-changes marker and its dynamic `- Visual Studio
/// Code` suffix, collapses whitespace, and lowercases the result.
pub fn normalize_title(title: &str) -> String {
    let without_marker = title.replace('●', "");
    let trimmed = without_marker.trim();
    let without_suffix = trimmed
        .strip_suffix(VSCODE_TITLE_SUFFIX)
        .map_or(trimmed, str::trim_end);

    without_suffix
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();

    let mut previous_row: Vec<usize> = (0..=b.len()).collect();
    let mut current_row = vec![0usize; b.len() + 1];

    for (i, &ca) in a.iter().enumerate() {
        current_row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let substitution_cost = usize::from(ca != cb);
            current_row[j + 1] = (previous_row[j + 1] + 1)
                .min(current_row[j] + 1)
                .min(previous_row[j] + substitution_cost);
        }
        std::mem::swap(&mut previous_row, &mut current_row);
    }

    previous_row[b.len()]
}

/// Normalized similarity between two already-[`normalize_title`]d strings, in
/// `0.0..=1.0` (1.0 = identical). PLAN.md §5.5's "登録時タイトルとの正規化類似度".
pub fn title_similarity(a: &str, b: &str) -> f64 {
    let max_len = a.chars().count().max(b.chars().count());
    if max_len == 0 {
        return 1.0;
    }
    1.0 - (levenshtein_distance(a, b) as f64 / max_len as f64)
}

/// Scores how well `candidate` matches `matcher`, per PLAN.md §5.5's table.
/// "Exact registered title" and "80%+ normalized similarity" are mutually
/// exclusive tiers (an exact match does not also collect the similarity bonus).
pub fn score_candidate(matcher: &WindowMatcher, candidate: &TopLevelWindow) -> i32 {
    let mut score = 0;

    if candidate.executable_path.as_ref() == Some(&matcher.executable_path) {
        score += 50;
    }
    if candidate.window_class == matcher.window_class {
        score += 25;
    }
    if matcher
        .title_contains
        .as_deref()
        .is_some_and(|needle| !needle.is_empty() && candidate.title.contains(needle))
    {
        score += 30;
    }
    if let Some(pattern) = &matcher.title_regex
        && let Ok(re) = regex::Regex::new(pattern)
        && re.is_match(&candidate.title)
    {
        score += 30;
    }

    if candidate.title == matcher.registered_title {
        score += 20;
    } else if title_similarity(
        &normalize_title(&matcher.registered_title),
        &normalize_title(&candidate.title),
    ) >= TITLE_SIMILARITY_THRESHOLD
    {
        score += 10;
    }

    score
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchDecision {
    /// A single candidate is confidently the right window.
    AutoRebind { hwnd: isize },
    /// One or more candidates scored high enough to be plausible, but not
    /// confidently enough to pick automatically; the user must confirm.
    Ambiguous { candidates: Vec<isize> },
    /// Nothing scored high enough to be worth showing as a match.
    Unresolved,
}

/// Re-matches `matcher` against `candidates` (PLAN.md §5.5-§5.6).
///
/// `bound_elsewhere` lists HWNDs already bound to a *different* registration
/// (from an earlier candidate in the same re-bind pass) and excludes them
/// entirely, per §5.5's "候補が別ワークセットへ既にバインド済み: 除外".
pub fn resolve_best_match(
    matcher: &WindowMatcher,
    candidates: &[TopLevelWindow],
    bound_elsewhere: &HashSet<isize>,
) -> MatchDecision {
    let mut scored: Vec<(isize, i32, bool)> = candidates
        .iter()
        .filter(|candidate| !bound_elsewhere.contains(&candidate.hwnd))
        .map(|candidate| {
            (
                candidate.hwnd,
                score_candidate(matcher, candidate),
                candidate.executable_path.is_some(),
            )
        })
        .collect();

    scored.sort_by_key(|&(_, score, _)| std::cmp::Reverse(score));

    let Some(&(best_hwnd, best_score, best_has_exe_path)) = scored.first() else {
        return MatchDecision::Unresolved;
    };

    if best_score < AUTO_REBIND_THRESHOLD {
        return MatchDecision::Unresolved;
    }

    let margin = scored.get(1).map_or(best_score, |&(_, second_score, _)| {
        best_score - second_score
    });

    // §5.6: a candidate whose executable path couldn't be read is never
    // auto-rebound, even at high confidence — it falls back to asking the user.
    if margin >= AUTO_REBIND_MARGIN && best_has_exe_path {
        MatchDecision::AutoRebind { hwnd: best_hwnd }
    } else {
        let candidates = scored
            .into_iter()
            .filter(|&(_, score, _)| score >= AUTO_REBIND_THRESHOLD)
            .map(|(hwnd, _, _)| hwnd)
            .collect();
        MatchDecision::Ambiguous { candidates }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn matcher() -> WindowMatcher {
        WindowMatcher {
            executable_path: PathBuf::from(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            process_name: "Code.exe".to_string(),
            window_class: "Chrome_WidgetWin_1".to_string(),
            registered_title: "main.rs - repodeck - Visual Studio Code".to_string(),
            title_contains: None,
            title_regex: None,
        }
    }

    fn candidate(hwnd: isize, exe: Option<&str>, class: &str, title: &str) -> TopLevelWindow {
        TopLevelWindow {
            hwnd,
            process_id: 1000,
            executable_path: exe.map(PathBuf::from),
            window_class: class.to_string(),
            title: title.to_string(),
            rect_px: crate::domain::placement::PixelRect::new(0, 0, 800, 600),
        }
    }

    #[test]
    fn normalize_title_strips_marker_suffix_and_case() {
        assert_eq!(
            normalize_title("● main.rs - repodeck - Visual Studio Code"),
            "main.rs - repodeck"
        );
        assert_eq!(normalize_title("  Foo   Bar  "), "foo bar");
    }

    #[test]
    fn exact_match_on_every_field_auto_rebinds() {
        let m = matcher();
        let c = candidate(
            1,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "main.rs - repodeck - Visual Studio Code",
        );

        assert_eq!(
            resolve_best_match(&m, &[c], &HashSet::new()),
            MatchDecision::AutoRebind { hwnd: 1 }
        );
    }

    #[test]
    fn title_only_change_still_auto_rebinds_via_similarity() {
        let m = matcher();
        // exe path (+50) + class (+25) + similarity (+10) = 85, no other candidate.
        let c = candidate(
            1,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "lib.rs - repodeck - Visual Studio Code",
        );

        assert_eq!(
            resolve_best_match(&m, &[c], &HashSet::new()),
            MatchDecision::AutoRebind { hwnd: 1 }
        );
    }

    #[test]
    fn two_similar_candidates_are_ambiguous() {
        let m = matcher();
        let c1 = candidate(
            1,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "main.rs - repodeck - Visual Studio Code",
        );
        let c2 = candidate(
            2,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "main.rs - repodeck - Visual Studio Code",
        );

        let decision = resolve_best_match(&m, &[c1, c2], &HashSet::new());
        assert_eq!(
            decision,
            MatchDecision::Ambiguous {
                candidates: vec![1, 2]
            }
        );
    }

    #[test]
    fn low_score_candidate_is_unresolved() {
        let m = matcher();
        let c = candidate(
            1,
            Some(r"C:\other\app.exe"),
            "SomeOtherClass",
            "Completely different title",
        );

        assert_eq!(
            resolve_best_match(&m, &[c], &HashSet::new()),
            MatchDecision::Unresolved
        );
    }

    #[test]
    fn candidate_already_bound_elsewhere_is_excluded() {
        let m = matcher();
        let c = candidate(
            1,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "main.rs - repodeck - Visual Studio Code",
        );

        let mut bound = HashSet::new();
        bound.insert(1isize);

        assert_eq!(
            resolve_best_match(&m, &[c], &bound),
            MatchDecision::Unresolved
        );
    }

    #[test]
    fn missing_executable_path_never_auto_rebinds_even_at_high_confidence() {
        let m = matcher();
        // class (+25) + exact title (+20) = 45... not enough alone; add title_contains too.
        let mut m = m;
        m.title_contains = Some("main.rs".to_string());
        let c = candidate(
            1,
            None,
            "Chrome_WidgetWin_1",
            "main.rs - repodeck - Visual Studio Code",
        );
        // class(25) + contains(30) + exact title(20) = 75, well above threshold, single candidate.

        assert_eq!(
            resolve_best_match(&m, &[c], &HashSet::new()),
            MatchDecision::Ambiguous {
                candidates: vec![1]
            }
        );
    }

    #[test]
    fn title_contains_and_regex_both_contribute() {
        let mut m = matcher();
        m.title_contains = Some("repodeck".to_string());
        m.title_regex = Some(r"^main\.rs".to_string());
        let c = candidate(1, None, "SomeClass", "main.rs - repodeck");
        // contains(30) + regex(30) + similarity(10, since normalized titles differ by the VS Code suffix only, still >=0.8) = 70 -> unresolved.
        let score = score_candidate(&m, &c);
        assert_eq!(score, 70);
    }

    #[test]
    fn invalid_regex_is_ignored_rather_than_panicking() {
        let mut m = matcher();
        m.title_regex = Some("(unclosed".to_string());
        let c = candidate(1, None, "x", "y");
        // Should not panic; invalid regex simply contributes no score.
        assert_eq!(score_candidate(&m, &c), 0);
    }
}
