//! Workset registration logic: resolving a repository folder, building
//! [`WindowMatcher`]/[`ManagedWindow`] entries from live windows, and
//! preventing duplicate registrations (PLAN.md §3.6).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::domain::placement::{PixelRect, SavedPlacement, SavedShowState, normalize};
use crate::domain::workset::{
    ManagedWindow, ParkingPolicy, RepositoryKind, WindowMatcher, Workset,
};
use crate::persistence::clock;
use crate::windowing::enumerate::TopLevelWindow;
use crate::windowing::matcher::{self, MatchDecision};
use crate::windowing::monitor::MonitorInfo;

/// Searches `start` and its ancestors for a `.git` entry (PLAN.md §3.6:
/// "選択フォルダーから親方向へ`.git`を探索"). Returns the first directory that
/// has one, or `None` if the search reaches the filesystem root without
/// finding one — the folder is then registered as a plain directory set.
pub fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// Resolves `repository_kind`/`repository_path` from a user-picked folder:
/// walks up for a `.git` root, falling back to the picked folder itself as a
/// plain directory set if none is found.
pub fn resolve_repository(picked_folder: &Path) -> (PathBuf, RepositoryKind) {
    match find_git_root(picked_folder) {
        Some(git_root) => (git_root, RepositoryKind::Git),
        None => (picked_folder.to_path_buf(), RepositoryKind::Directory),
    }
}

/// Whether `candidate_path` is already registered under an existing workset
/// (PLAN.md §3.6: "同一正規パスの重複登録は禁止"). Both sides are compared as
/// given; callers should pass already-canonicalized paths.
pub fn is_duplicate_repository(worksets: &[Workset], candidate_path: &Path) -> bool {
    worksets
        .iter()
        .any(|workset| workset.repository_path == candidate_path)
}

/// Builds a [`WindowMatcher`] from a live window's current attributes
/// (PLAN.md §3.6 "登録時に保存する情報"). `title_contains`/`title_regex` are
/// left unset; the registration UI may add them afterward.
pub fn build_matcher(window: &TopLevelWindow) -> WindowMatcher {
    let process_name = window
        .executable_path
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| window.window_class.clone());

    WindowMatcher {
        executable_path: window.executable_path.clone().unwrap_or_default(),
        process_name,
        window_class: window.window_class.clone(),
        registered_title: window.title.clone(),
        title_contains: None,
        title_regex: None,
    }
}

/// Finds which main monitor (by index into `main_monitor_ids`, matching
/// [`crate::domain::placement::SavedPlacement::main_monitor_index`]) contains
/// `point`. Pure and independent of any specific window, so it is reused by
/// both registration and, later, Layout Studio-style diagnostics.
pub fn resolve_main_monitor_index(
    point: (i32, i32),
    monitors: &[MonitorInfo],
    main_monitor_ids: &[String],
) -> Option<usize> {
    main_monitor_ids.iter().position(|id| {
        monitors.iter().any(|monitor| {
            &monitor.device_name == id && monitor.bounds_px.contains_point(point.0, point.1)
        })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ManagedWindowBuildError {
    #[error("window is not on any configured main monitor")]
    NotOnMainMonitor,
}

/// Builds a [`ManagedWindow`] from a live window's captured geometry. The
/// caller supplies `rect`/`show_state` (typically from
/// `windowing::placement::get_normal_rect`/`get_show_state`) so this function
/// stays pure and testable without a real window.
pub fn build_managed_window(
    window: &TopLevelWindow,
    rect: PixelRect,
    show_state: SavedShowState,
    z_order: i32,
    monitors: &[MonitorInfo],
    main_monitor_ids: &[String],
) -> Result<ManagedWindow, ManagedWindowBuildError> {
    let center = window.rect_px.center();
    let monitor_index = resolve_main_monitor_index(center, monitors, main_monitor_ids)
        .ok_or(ManagedWindowBuildError::NotOnMainMonitor)?;
    let monitor_id = &main_monitor_ids[monitor_index];
    let monitor = monitors
        .iter()
        .find(|m| &m.device_name == monitor_id)
        .ok_or(ManagedWindowBuildError::NotOnMainMonitor)?;

    Ok(ManagedWindow {
        id: Uuid::new_v4(),
        matcher: build_matcher(window),
        main_placement: SavedPlacement {
            monitor_id: monitor_id.clone(),
            main_monitor_index: monitor_index,
            normalized_rect: normalize(rect, monitor.work_area_px),
            physical_rect_at_capture: rect,
            show_state,
        },
        z_order,
    })
}

/// Assembles a new [`Workset`] from already-built [`ManagedWindow`]s. Does not
/// touch [`crate::domain::config::AppConfig`] or validate uniqueness — callers
/// run [`is_duplicate_repository`] first and append the result themselves.
pub fn build_workset(
    name: String,
    color: String,
    repository_path: PathBuf,
    repository_kind: RepositoryKind,
    sort_order: i32,
    windows: Vec<ManagedWindow>,
) -> Workset {
    let now = clock::now_rfc3339();
    Workset {
        id: Uuid::new_v4(),
        name,
        repository_path,
        repository_kind,
        color,
        sort_order,
        direct_hotkey: None,
        parking_policy: ParkingPolicy::Auto,
        windows,
        created_at: now.clone(),
        updated_at: now,
    }
}

/// Re-matches every registered [`ManagedWindow`] across every `workset`
/// against `live_windows`, in order, greedily claiming each confidently
/// auto-rebound HWND so a later window can't also claim it (PLAN.md §5.5's
/// "候補が別ワークセットへ既にバインド済み: 除外" applied across the whole
/// registration, not just within one workset).
pub fn resolve_all_matches(
    worksets: &[Workset],
    live_windows: &[TopLevelWindow],
) -> HashMap<Uuid, MatchDecision> {
    let mut bound: HashSet<isize> = HashSet::new();
    let mut results = HashMap::new();

    for workset in worksets {
        for window in &workset.windows {
            let decision = matcher::resolve_best_match(&window.matcher, live_windows, &bound);
            if let MatchDecision::AutoRebind { hwnd } = &decision {
                bound.insert(*hwnd);
            }
            results.insert(window.id, decision);
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn find_git_root_walks_up_from_a_subdirectory() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        let nested = repo_root.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(repo_root.join(".git")).unwrap();

        assert_eq!(find_git_root(&nested), Some(repo_root));
    }

    #[test]
    fn find_git_root_returns_none_without_a_git_directory() {
        let dir = tempdir().unwrap();
        let plain = dir.path().join("just-a-folder");
        std::fs::create_dir_all(&plain).unwrap();

        assert_eq!(find_git_root(&plain), None);
    }

    #[test]
    fn resolve_repository_prefers_git_root_over_picked_folder() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        let nested = repo_root.join("crates").join("app");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(repo_root.join(".git")).unwrap();

        let (path, kind) = resolve_repository(&nested);
        assert_eq!(path, repo_root);
        assert_eq!(kind, RepositoryKind::Git);
    }

    #[test]
    fn resolve_repository_falls_back_to_directory_kind() {
        let dir = tempdir().unwrap();
        let plain = dir.path().join("just-a-folder");
        std::fs::create_dir_all(&plain).unwrap();

        let (path, kind) = resolve_repository(&plain);
        assert_eq!(path, plain);
        assert_eq!(kind, RepositoryKind::Directory);
    }

    fn monitor(device_name: &str, bounds: PixelRect) -> MonitorInfo {
        MonitorInfo {
            handle: 0,
            device_name: device_name.to_string(),
            bounds_px: bounds,
            work_area_px: bounds,
            dpi_x: 96,
            dpi_y: 96,
            is_primary: false,
        }
    }

    #[test]
    fn resolve_main_monitor_index_finds_the_monitor_containing_the_point() {
        let monitors = [
            monitor("A", PixelRect::new(0, 0, 1920, 1080)),
            monitor("B", PixelRect::new(1920, 0, 1920, 1080)),
        ];
        let main_ids = vec!["A".to_string(), "B".to_string()];

        assert_eq!(
            resolve_main_monitor_index((100, 100), &monitors, &main_ids),
            Some(0)
        );
        assert_eq!(
            resolve_main_monitor_index((2000, 100), &monitors, &main_ids),
            Some(1)
        );
        assert_eq!(
            resolve_main_monitor_index((5000, 100), &monitors, &main_ids),
            None
        );
    }

    #[test]
    fn build_managed_window_normalizes_against_the_resolved_monitor() {
        let monitors = [monitor("A", PixelRect::new(0, 0, 1920, 1080))];
        let main_ids = vec!["A".to_string()];

        let window = TopLevelWindow {
            hwnd: 42,
            process_id: 100,
            executable_path: Some(PathBuf::from(r"C:\tools\code.exe")),
            window_class: "Chrome_WidgetWin_1".to_string(),
            title: "main.rs".to_string(),
            rect_px: PixelRect::new(100, 100, 800, 600),
        };

        let managed = build_managed_window(
            &window,
            PixelRect::new(100, 100, 800, 600),
            SavedShowState::Normal,
            0,
            &monitors,
            &main_ids,
        )
        .unwrap();

        assert_eq!(managed.main_placement.main_monitor_index, 0);
        assert_eq!(managed.main_placement.monitor_id, "A");
        assert!((managed.main_placement.normalized_rect.x - 100.0 / 1920.0).abs() < 1e-9);
        assert_eq!(
            managed.matcher.executable_path,
            PathBuf::from(r"C:\tools\code.exe")
        );
        assert_eq!(managed.matcher.registered_title, "main.rs");
    }

    #[test]
    fn build_managed_window_fails_when_window_is_off_all_main_monitors() {
        let monitors = [monitor("A", PixelRect::new(0, 0, 1920, 1080))];
        let main_ids = vec!["A".to_string()];

        let window = TopLevelWindow {
            hwnd: 42,
            process_id: 100,
            executable_path: None,
            window_class: "X".to_string(),
            title: "Y".to_string(),
            rect_px: PixelRect::new(5000, 5000, 800, 600),
        };

        let result = build_managed_window(
            &window,
            window.rect_px,
            SavedShowState::Normal,
            0,
            &monitors,
            &main_ids,
        );
        assert_eq!(result, Err(ManagedWindowBuildError::NotOnMainMonitor));
    }

    #[test]
    fn is_duplicate_repository_matches_exact_path_only() {
        let workset = build_workset(
            "A".to_string(),
            "#fff".to_string(),
            PathBuf::from(r"D:\repos\a"),
            RepositoryKind::Git,
            0,
            Vec::new(),
        );

        assert!(is_duplicate_repository(
            std::slice::from_ref(&workset),
            &PathBuf::from(r"D:\repos\a")
        ));
        assert!(!is_duplicate_repository(
            &[workset],
            &PathBuf::from(r"D:\repos\b")
        ));
    }

    fn window_with(hwnd: isize, exe: &str, class: &str, title: &str) -> TopLevelWindow {
        TopLevelWindow {
            hwnd,
            process_id: 1,
            executable_path: Some(PathBuf::from(exe)),
            window_class: class.to_string(),
            title: title.to_string(),
            rect_px: PixelRect::new(0, 0, 800, 600),
        }
    }

    #[test]
    fn resolve_all_matches_does_not_let_two_worksets_claim_the_same_window() {
        let matcher_a = WindowMatcher {
            executable_path: PathBuf::from(r"C:\code.exe"),
            process_name: "code.exe".to_string(),
            window_class: "Chrome_WidgetWin_1".to_string(),
            registered_title: "repo-a - Visual Studio Code".to_string(),
            title_contains: None,
            title_regex: None,
        };
        let mut matcher_b = matcher_a.clone();
        matcher_b.registered_title = "repo-b - Visual Studio Code".to_string();

        let managed_a = ManagedWindow {
            id: Uuid::new_v4(),
            matcher: matcher_a,
            main_placement: SavedPlacement {
                monitor_id: "A".to_string(),
                main_monitor_index: 0,
                normalized_rect: crate::domain::placement::NormalizedRect {
                    x: 0.0,
                    y: 0.0,
                    width: 0.1,
                    height: 0.1,
                },
                physical_rect_at_capture: PixelRect::new(0, 0, 800, 600),
                show_state: SavedShowState::Normal,
            },
            z_order: 0,
        };
        let mut managed_b = managed_a.clone();
        managed_b.id = Uuid::new_v4();
        managed_b.matcher = matcher_b;

        let workset_a = build_workset(
            "A".to_string(),
            "#fff".to_string(),
            PathBuf::from(r"D:\a"),
            RepositoryKind::Git,
            0,
            vec![managed_a.clone()],
        );
        let workset_b = build_workset(
            "B".to_string(),
            "#fff".to_string(),
            PathBuf::from(r"D:\b"),
            RepositoryKind::Git,
            1,
            vec![managed_b.clone()],
        );

        // Only one live VS Code window exists; both worksets' matchers would
        // score identically well against it (same exe/class, similar title).
        let live = [window_with(
            1,
            r"C:\code.exe",
            "Chrome_WidgetWin_1",
            "repo-a - Visual Studio Code",
        )];

        let decisions = resolve_all_matches(&[workset_a, workset_b], &live);

        let decision_a = decisions.get(&managed_a.id).unwrap();
        let decision_b = decisions.get(&managed_b.id).unwrap();

        // Workset A is processed first and claims hwnd 1; workset B must not
        // also claim it (the shared HWND cannot belong to two AutoRebinds).
        assert_eq!(*decision_a, MatchDecision::AutoRebind { hwnd: 1 });
        assert_ne!(*decision_b, MatchDecision::AutoRebind { hwnd: 1 });
    }
}
