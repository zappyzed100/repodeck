//! Ordering and substring filtering for the Quick Switcher's workset list
//! (PLAN.md §3.3 "表示内容"/"キーボード操作").

use uuid::Uuid;

use crate::domain::config::SortMode;
use crate::domain::workset::Workset;

/// セットを手動順で1つ動かす向き。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MoveDirection {
    Up,
    Down,
}

/// 手動順（`sort_order` 昇順）で `workset_id` を1つ上/下へ動かし、全セットの
/// `sort_order` を 0..n の連番へ振り直す。動いたら `true`、端で動けない・対象が
/// 無いときは `false`（`sort_order` は触らない）。
///
/// クイックスイッチャーの ▲▼ から呼ぶ。連番へ振り直すのは、既存データに同じ
/// `sort_order` の重複（タイ）があっても順序が確定するようにするため。
pub fn move_workset(worksets: &mut [Workset], workset_id: Uuid, direction: MoveDirection) -> bool {
    // 現在の手動順でのインデックス列（安定ソートで既存順を保つ）。
    let mut order: Vec<usize> = (0..worksets.len()).collect();
    order.sort_by_key(|&i| worksets[i].sort_order);

    let Some(pos) = order.iter().position(|&i| worksets[i].id == workset_id) else {
        return false;
    };
    let target = match direction {
        MoveDirection::Up if pos > 0 => pos - 1,
        MoveDirection::Down if pos + 1 < order.len() => pos + 1,
        _ => return false,
    };
    order.swap(pos, target);
    for (new_sort, &i) in order.iter().enumerate() {
        worksets[i].sort_order = i32::try_from(new_sort).unwrap_or(0);
    }
    true
}

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

    fn manual_names(worksets: &[Workset]) -> Vec<String> {
        sorted_and_filtered(worksets, SortMode::Manual, "")
            .iter()
            .map(|w| w.name.clone())
            .collect()
    }

    #[test]
    fn move_down_then_up_returns_to_the_original_order() {
        let mut worksets = vec![
            workset("a", "C:\\a", 0),
            workset("b", "C:\\b", 1),
            workset("c", "C:\\c", 2),
        ];
        let a_id = worksets[0].id;
        assert_eq!(manual_names(&worksets), ["a", "b", "c"]);

        assert!(move_workset(&mut worksets, a_id, MoveDirection::Down));
        assert_eq!(manual_names(&worksets), ["b", "a", "c"]);

        assert!(move_workset(&mut worksets, a_id, MoveDirection::Up));
        assert_eq!(manual_names(&worksets), ["a", "b", "c"]);
    }

    #[test]
    fn moving_the_top_up_or_the_bottom_down_is_a_no_op() {
        let mut worksets = vec![workset("a", "C:\\a", 0), workset("b", "C:\\b", 1)];
        let a_id = worksets[0].id;
        let b_id = worksets[1].id;

        assert!(!move_workset(&mut worksets, a_id, MoveDirection::Up));
        assert!(!move_workset(&mut worksets, b_id, MoveDirection::Down));
        assert_eq!(manual_names(&worksets), ["a", "b"]);
    }

    #[test]
    fn move_reassigns_sequential_sort_order_even_with_ties() {
        // 既存データに同じ sort_order（タイ）があっても順序を確定できる。
        let mut worksets = vec![
            workset("a", "C:\\a", 2),
            workset("b", "C:\\b", 2),
            workset("c", "C:\\c", 2),
        ];
        let c_id = worksets[2].id;
        // タイなので挿入順 a,b,c が現在の手動順。c を上へ → a,c,b。
        assert!(move_workset(&mut worksets, c_id, MoveDirection::Up));
        assert_eq!(manual_names(&worksets), ["a", "c", "b"]);
        let mut orders: Vec<i32> = worksets.iter().map(|w| w.sort_order).collect();
        orders.sort();
        assert_eq!(orders, [0, 1, 2], "sort_order must be a 0..n permutation");
    }
}
