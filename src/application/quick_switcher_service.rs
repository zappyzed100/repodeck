//! Ordering and substring filtering for the Quick Switcher's workset list
//! (PLAN.md §3.3 "表示内容"/"キーボード操作").

use crate::domain::config::SortMode;
use crate::domain::workset::Workset;

/// Orders `worksets` per `sort_mode`, then filters by a case-insensitive
/// substring match against name or repository path.
///
/// `SortMode::Recent` has no backing data yet (neither `Workset` nor
/// `RuntimeState` records "last switched to"), so it's aliased to `Manual`
/// until a future phase adds a settings UI for `sort_mode` and a place to
/// persist activation timestamps.
pub fn sorted_and_filtered<'a>(
    worksets: &'a [Workset],
    sort_mode: SortMode,
    filter_text: &str,
) -> Vec<&'a Workset> {
    let mut sorted: Vec<&Workset> = worksets.iter().collect();
    match sort_mode {
        SortMode::Manual | SortMode::Recent => sorted.sort_by_key(|w| w.sort_order),
        SortMode::Name => sorted.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.sort_order.cmp(&b.sort_order))
        }),
    }

    let needle = filter_text.trim().to_lowercase();
    if needle.is_empty() {
        return sorted;
    }

    sorted
        .into_iter()
        .filter(|w| {
            w.name.to_lowercase().contains(&needle)
                || w.repository_path
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(&needle)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workset::{ParkingPolicy, RepositoryKind};

    fn workset(name: &str, repository_path: &str, sort_order: i32) -> Workset {
        Workset {
            id: uuid::Uuid::new_v4(),
            name: name.to_string(),
            repository_path: repository_path.into(),
            repository_kind: RepositoryKind::Git,
            color: "#000000".to_string(),
            sort_order,
            direct_hotkey: None,
            parking_policy: ParkingPolicy::Auto,
            fullscreen_when_parked: false,
            windows: Vec::new(),
            created_at: "2026-07-20T00:00:00Z".to_string(),
            updated_at: "2026-07-20T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn manual_order_respects_sort_order() {
        let b = workset("b-set", "C:\\repo-b", 1);
        let a = workset("a-set", "C:\\repo-a", 0);
        let worksets = vec![b.clone(), a.clone()];

        let result = sorted_and_filtered(&worksets, SortMode::Manual, "");

        assert_eq!(
            result.iter().map(|w| &w.name).collect::<Vec<_>>(),
            vec![&a.name, &b.name]
        );
    }

    #[test]
    fn name_order_is_case_insensitive() {
        let upper = workset("Zeta", "C:\\repo-z", 0);
        let lower = workset("alpha", "C:\\repo-a", 1);
        let worksets = vec![upper.clone(), lower.clone()];

        let result = sorted_and_filtered(&worksets, SortMode::Name, "");

        assert_eq!(
            result.iter().map(|w| &w.name).collect::<Vec<_>>(),
            vec![&lower.name, &upper.name]
        );
    }

    #[test]
    fn recent_is_aliased_to_manual() {
        let b = workset("b-set", "C:\\repo-b", 1);
        let a = workset("a-set", "C:\\repo-a", 0);
        let worksets = vec![b.clone(), a.clone()];

        let manual = sorted_and_filtered(&worksets, SortMode::Manual, "");
        let recent = sorted_and_filtered(&worksets, SortMode::Recent, "");

        assert_eq!(
            manual.iter().map(|w| w.id).collect::<Vec<_>>(),
            recent.iter().map(|w| w.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn filter_matches_name_substring_case_insensitively() {
        let repodeck = workset("repodeck", "C:\\code\\repodeck", 0);
        let other = workset("other-project", "C:\\code\\other", 1);
        let worksets = vec![repodeck.clone(), other.clone()];

        let result = sorted_and_filtered(&worksets, SortMode::Manual, "REPO");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, repodeck.id);
    }

    #[test]
    fn filter_matches_repository_path_substring() {
        let repodeck = workset("main", "C:\\code\\portfolio\\repodeck", 0);
        let other = workset("side", "C:\\code\\side-project", 1);
        let worksets = vec![repodeck.clone(), other.clone()];

        let result = sorted_and_filtered(&worksets, SortMode::Manual, "portfolio");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, repodeck.id);
    }

    #[test]
    fn empty_or_whitespace_filter_returns_everything() {
        let worksets = vec![workset("a", "C:\\a", 0), workset("b", "C:\\b", 1)];

        assert_eq!(
            sorted_and_filtered(&worksets, SortMode::Manual, "").len(),
            2
        );
        assert_eq!(
            sorted_and_filtered(&worksets, SortMode::Manual, "   ").len(),
            2
        );
    }
}
