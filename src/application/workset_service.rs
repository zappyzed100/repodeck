//! Workset registration logic: resolving a repository folder, building
//! [`WindowMatcher`]/[`ManagedWindow`] entries from live windows, and
//! preventing duplicate registrations (PLAN.md §3.6).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use uuid::Uuid;

use crate::domain::placement::{PixelRect, SavedPlacement, SavedShowState, normalize};
use crate::domain::workset::{
    ManagedWindow, ParkingPolicy, RepositoryKind, WindowMatcher, Workset,
};
use crate::persistence::clock;
use crate::windowing::enumerate::TopLevelWindow;
use crate::windowing::matcher::{self, MatchDecision, TitlelessMatch};
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

/// Just the piece of a VS Code `.code-workspace` file (its multi-root
/// workspace format) this module needs: the listed folders' `path` entries.
/// Other top-level keys (`settings`, `extensions`, ...) are ignored by
/// serde's default behavior.
#[derive(Deserialize)]
struct CodeWorkspaceFile {
    folders: Vec<CodeWorkspaceFolder>,
}

#[derive(Deserialize)]
struct CodeWorkspaceFolder {
    path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceFileError {
    #[error("ワークスペースファイルを読み込めません")]
    ReadFailed,
    #[error("ワークスペースファイルの形式が不正です（.code-workspaceのJSONとして解釈できません）")]
    ParseFailed,
    #[error("ワークスペースファイルにフォルダーが1つも定義されていません")]
    NoFolders,
}

/// Resolves a `.code-workspace` file's first listed folder to an absolute
/// path (PLAN.md §3.6 workspace-file support). A relative `path` entry is
/// resolved against the workspace file's own parent directory, matching VS
/// Code's own resolution rule. Multi-root workspaces with more than one
/// folder use only the first — a workset tracks one repository, not a set.
pub fn resolve_workspace_file(workspace_path: &Path) -> Result<PathBuf, WorkspaceFileError> {
    let contents =
        std::fs::read_to_string(workspace_path).map_err(|_| WorkspaceFileError::ReadFailed)?;
    let parsed: CodeWorkspaceFile =
        serde_json::from_str(&contents).map_err(|_| WorkspaceFileError::ParseFailed)?;
    let first = parsed
        .folders
        .first()
        .ok_or(WorkspaceFileError::NoFolders)?;
    let folder_path = PathBuf::from(&first.path);
    if folder_path.is_absolute() {
        Ok(folder_path)
    } else {
        let parent = workspace_path.parent().unwrap_or_else(|| Path::new("."));
        Ok(parent.join(folder_path))
    }
}

/// Resolves the directory a workset should be matched against for agent
/// cwd-correlation (`agent_status_service::map_event_to_workset`,
/// PLAN.md §6.6). A `RepositoryKind::Workspace` workset's `repository_path`
/// points at the `.code-workspace` file itself (not a directory it could be
/// compared against a hook's `cwd`), so its first folder entry is parsed out
/// here. If the file can no longer be read (moved/deleted since
/// registration), falls back to the literal `repository_path` — matching
/// then simply never succeeds rather than panicking.
pub fn resolve_match_path(repository_path: &Path, repository_kind: RepositoryKind) -> PathBuf {
    if repository_kind == RepositoryKind::Workspace
        && let Ok(folder) = resolve_workspace_file(repository_path)
    {
        return folder;
    }
    repository_path.to_path_buf()
}

/// Whether a workset contains the OpenAI ChatGPT / Codex desktop app.
///
/// This is RepoDeck's marker for 「このセットでは Codex を回す」: such a set
/// groups the ChatGPT app with the browser (and no VS Code repo), so it has no
/// `repository_path` to correlate agent events against. `app.rs` uses this to
/// auto-adopt an otherwise-unmatched Codex event's cwd into the active set the
/// first time one arrives, so the set starts reflecting the run's status
/// without the user typing a path.
pub fn contains_chatgpt_codex_app(workset: &Workset) -> bool {
    workset.windows.iter().any(|w| {
        crate::application::launch_service::is_chatgpt_codex_app(
            &w.matcher.executable_path,
            &w.matcher.process_name,
        )
    })
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
        // Filled in by the caller (registration), which knows the workset's
        // repository path and can read a browser's URL from its live HWND.
        launch_spec: None,
        minimize_when_parked: false,
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
        fullscreen_when_parked: false,
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
    resolve_all_matches_with_bindings(worksets, live_windows, &HashMap::new(), None)
}

/// Whether a session HWND binding is still trustworthy: the live window at that
/// HWND must belong to the same executable and window class the managed window
/// was registered with (guards against Windows recycling the HWND for an
/// unrelated window). Title/URL are deliberately NOT checked — that volatility
/// is the whole reason bindings exist.
///
/// クラスは照合しない。登録アプリから宣言したウィンドウはそもそもクラスを持たず
/// （宣言時点でアプリが起動しているとは限らない）、学習したクラスも WinForms の
/// `WindowsForms10.Window.8.app.0.21b46d2_r3_ad1` のように実行ごとに変わりうる。
/// 一致を要求すると、そうしたウィンドウは永久にバインドを保持できない。HWND の
/// 使い回しに対しては実行ファイルの一致で十分に守れている。
/// 起動引数そのものでウィンドウを特定する。ブラウザを `--user-data-dir` で用途別に
/// 分けている場合だけ働く（Chromium 系はデータ領域が違えば独立プロセスになるので、
/// そのプロセスのコマンドラインに引数が残る）。
///
/// タイトルもクラスも同じで見分けようのないブラウザを、確実に「この窓だ」と
/// 特定できる唯一の手掛かり。該当しないウィンドウでは何もしない（コマンドライン
/// 読み出しはプロセスを開くので、必要なときだけ行う）。
fn find_by_launch_identity(
    window: &ManagedWindow,
    live_windows: &[TopLevelWindow],
    bound: &HashSet<isize>,
) -> Option<isize> {
    let spec = window.launch_spec.as_ref()?;
    let identity = crate::application::launch_service::browser_identity_arg(&spec.args)?;
    let needle = identity
        .trim_start_matches("--user-data-dir=")
        .trim_matches('"');
    if needle.is_empty() {
        return None;
    }

    live_windows
        .iter()
        .filter(|w| !bound.contains(&w.hwnd))
        .filter(|w| {
            w.executable_path
                .as_deref()
                .is_some_and(|p| matcher::same_executable(p, &window.matcher.executable_path))
        })
        .find(|w| {
            crate::windowing::process_info::read_process_command_line(w.process_id)
                .is_some_and(|cmd| cmd.to_lowercase().contains(&needle.to_lowercase()))
        })
        .map(|w| w.hwnd)
}

/// Resolves a script-host registration to the single visible UI window it
/// launched in another process (for example `wscript.exe` → Brave).
///
/// This bridge is deliberately narrower than the normal matcher: it only
/// applies to known launcher executables, requires a title rooted in the
/// registered app name, excludes windows already claimed by another entry,
/// and refuses to guess when more than one browser window qualifies.
fn launcher_handoff_decision(
    managed: &ManagedWindow,
    live_windows: &[TopLevelWindow],
    bound: &HashSet<isize>,
) -> Option<MatchDecision> {
    if !matcher::is_launcher_executable(&managed.matcher.executable_path) {
        return None;
    }

    let candidates: Vec<isize> = live_windows
        .iter()
        .filter(|window| !bound.contains(&window.hwnd))
        .filter(|window| {
            window.executable_path.as_deref().is_some_and(|path| {
                !matcher::same_executable(path, &managed.matcher.executable_path)
            })
        })
        .filter(|window| matcher::launcher_title_matches(&managed.matcher, window))
        .map(|window| window.hwnd)
        .collect();

    match candidates.as_slice() {
        [] => None,
        [hwnd] => Some(MatchDecision::AutoRebind { hwnd: *hwnd }),
        _ => Some(MatchDecision::Ambiguous { candidates }),
    }
}

fn binding_still_valid(matcher: &WindowMatcher, live: &TopLevelWindow) -> bool {
    let same_app = live
        .executable_path
        .as_deref()
        .is_some_and(|p| matcher::same_executable(p, &matcher.executable_path));
    if !same_app {
        return matcher::launcher_title_matches(matcher, live);
    }

    // 実行ファイルだけでは「そのアプリの窓」しか言えない。登録が自分の窓を
    // 見分ける手掛かり（`title_contains` / `title_regex`）を持っているなら、
    // それが今も成り立っていることまで確かめる。
    //
    // ここを実行ファイルだけで通していたせいで、一度ついた誤りが二度と直らな
    // かった。実機では `repodeck` セットの VS Code 登録が「02_求解・高速化」の
    // 窓を握り、正しい `repodeck` の窓が後から現れても乗り換えなかった
    // （2026-07-29）。手掛かりを持たない登録は今までどおり実行ファイルだけで
    // 判断する——それ以上の材料が無く、要求すると永久にバインドを保持できない。
    !matcher::has_discriminator(matcher) || matcher::has_title_evidence(matcher, live)
}

/// 実行ファイルごとの登録数。同じ実行ファイルを名乗る登録が2つ以上あると、
/// 実行ファイル＋クラスの75点だけでは互いを見分けられない。
fn registrations_per_executable(worksets: &[Workset]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for window in worksets.iter().flat_map(|w| w.windows.iter()) {
        let key = window
            .matcher
            .executable_path
            .as_os_str()
            .to_string_lossy()
            .to_ascii_lowercase();
        *counts.entry(key).or_default() += 1;
    }
    counts
}

/// タイトルの根拠が無い候補を、この登録に限って自動バインドしてよいか。
fn titleless_policy(counts: &HashMap<String, usize>, window: &ManagedWindow) -> TitlelessMatch {
    let key = window
        .matcher
        .executable_path
        .as_os_str()
        .to_string_lossy()
        .to_ascii_lowercase();
    if counts.get(&key).copied().unwrap_or(0) > 1 {
        TitlelessMatch::Reject
    } else {
        TitlelessMatch::Accept
    }
}

/// Like [`resolve_all_matches`], but first tries each managed window's tracked
/// session HWND binding (PLAN.md §5.4 extension): if the bound HWND is still
/// live and passes [`binding_still_valid`], it's used directly, bypassing
/// content matching. This lets a browser window whose title/URL constantly
/// change (a video tab) still be re-found. Anything without a usable binding
/// falls back to the normal scoring matcher.
///
/// `priority_workset_id`, when given, is resolved first so that a window shared
/// by several worksets binds to it: on a switch this is the target set, so the
/// shared window lands on the main screen (with the activated set) instead of
/// being claimed and parked by another set. The same physical window may be
/// registered in multiple worksets — that is allowed and simply means the
/// window follows whichever set is active.
pub fn resolve_all_matches_with_bindings(
    worksets: &[Workset],
    live_windows: &[TopLevelWindow],
    bindings: &HashMap<Uuid, isize>,
    priority_workset_id: Option<Uuid>,
) -> HashMap<Uuid, MatchDecision> {
    let mut bound: HashSet<isize> = HashSet::new();
    let mut results = HashMap::new();
    let per_exe = registrations_per_executable(worksets);

    // The priority workset first, then the rest in their original order.
    let ordered = priority_workset_id
        .and_then(|id| worksets.iter().find(|w| w.id == id))
        .into_iter()
        .chain(
            worksets
                .iter()
                .filter(|w| Some(w.id) != priority_workset_id),
        );
    for workset in ordered {
        for window in &workset.windows {
            if let Some(&hwnd) = bindings.get(&window.id) {
                match live_windows.iter().find(|w| w.hwnd == hwnd) {
                    Some(live)
                        if !bound.contains(&hwnd) && binding_still_valid(&window.matcher, live) =>
                    {
                        bound.insert(hwnd);
                        results.insert(window.id, MatchDecision::AutoRebind { hwnd });
                        continue;
                    }
                    // バインド先のウィンドウがもう存在しない＝このエントリは閉じ
                    // られた。内容マッチでの再発見はまだ許す（開き直した VS Code
                    // はタイトルに同じフォルダ名を持つ）が、「同じアプリの別窓」
                    // で妥協させてはいけない。妥協すると、閉じた Brave が別の
                    // Brave ウィンドウを勝手に取り込み、セットは開いているように
                    // 見え、開き直すが「対象なし」と言う。
                    None => {
                        // ブラウザを用途ごとに `--user-data-dir` で分けている場合、
                        // その窓は独立プロセスなのでコマンドラインで確実に特定できる。
                        // タイトルでは見分けられないブラウザでも、これなら正しい窓を
                        // 取り戻せる。
                        if let Some(hwnd) = find_by_launch_identity(window, live_windows, &bound) {
                            bound.insert(hwnd);
                            results.insert(window.id, MatchDecision::AutoRebind { hwnd });
                            continue;
                        }
                        if let Some(decision) =
                            launcher_handoff_decision(window, live_windows, &bound)
                        {
                            if let MatchDecision::AutoRebind { hwnd } = &decision {
                                bound.insert(*hwnd);
                            }
                            results.insert(window.id, decision);
                            continue;
                        }
                        // バインドが指す窓が消えている＝閉じられた。再発見は許すが
                        // 「同じアプリの別窓」で妥協させてはいけないので、登録数に
                        // 関わらずタイトルの根拠を要求する。
                        let decision = matcher::resolve_best_match(
                            &window.matcher,
                            live_windows,
                            &bound,
                            TitlelessMatch::Reject,
                        );
                        if let MatchDecision::AutoRebind { hwnd } = &decision {
                            bound.insert(*hwnd);
                        }
                        results.insert(window.id, decision);
                        continue;
                    }
                    // 生きてはいるが先のセットが確保済み、または HWND が無関係な
                    // ウィンドウに再利用された場合は内容マッチへ委ねる。
                    _ => {}
                }
            }

            let decision =
                launcher_handoff_decision(window, live_windows, &bound).unwrap_or_else(|| {
                    matcher::resolve_best_match(
                        &window.matcher,
                        live_windows,
                        &bound,
                        titleless_policy(&per_exe, window),
                    )
                });
            if let MatchDecision::AutoRebind { hwnd } = &decision {
                bound.insert(*hwnd);
            }
            results.insert(window.id, decision);
        }
    }

    results
}

/// Extracts the fresh `managed_window_id → HWND` bindings from a resolution
/// result, so the caller can persist them for next time.
pub fn bindings_from_decisions(decisions: &HashMap<Uuid, MatchDecision>) -> HashMap<Uuid, isize> {
    decisions
        .iter()
        .filter_map(|(id, decision)| match decision {
            MatchDecision::AutoRebind { hwnd } => Some((*id, *hwnd)),
            _ => None,
        })
        .collect()
}

/// 紐づけ表から、**もう存在しない登録**のぶんを落とす。落とした件数を返す。
///
/// `runtime.json` の `window_bindings` は書き足す一方だった。現役のエントリは自分の
/// 項目を上書きするだけなので増えない——増えるのは、セットやウィンドウを削除しても
/// その `managed_window_id` の項目が残り続けるからで、これが唯一の漏れ口。実機では
/// 190件（現役の窓は22個）まで溜まっていた（2026-07-26）。
///
/// 残骸が害になるのは HWND を OS が再利用するからで、「現役の窓はもう別のエントリが
/// 握っている」と主張してくる。実機の ChatGPT の窓は、存在しない6つの id に握られた
/// 状態になり、セットが永久にバインドできなくなっていた。
///
/// **窓が既に消えている紐づけは落とさない。** あれは残骸ではなく情報を持っている:
/// [`resolve_all_matches_with_bindings`] は「紐づけがあるのに窓が無い＝このエントリは
/// 閉じられた」と読み、再発見に
/// [`matcher::has_title_evidence`] の厳しい条件を課す。項目そのものを消すと、その
/// 条件のない素の照合へ落ちてしまい、「閉じた Brave が無関係な別の Brave 窓を取り込む」
/// 挙動が戻ってくる。生きている HWND が再利用されていた場合は `binding_still_valid`
/// が弾くので、現役エントリの側はこれで足りている。
pub fn prune_window_bindings(bindings: &mut HashMap<Uuid, isize>, worksets: &[Workset]) -> usize {
    let known: HashSet<Uuid> = worksets
        .iter()
        .flat_map(|w| w.windows.iter())
        .map(|w| w.id)
        .collect();
    let before = bindings.len();
    bindings.retain(|id, _| known.contains(id));
    before - bindings.len()
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

    fn workset_with_process(process_name: &str) -> Workset {
        use crate::domain::placement::{NormalizedRect, PixelRect};
        let mut ws = build_workset(
            "s".into(),
            "#fff".to_string(),
            PathBuf::new(),
            RepositoryKind::Directory,
            0,
            Vec::new(),
        );
        ws.windows.push(ManagedWindow {
            id: Uuid::new_v4(),
            matcher: WindowMatcher {
                executable_path: PathBuf::from(r"C:\app.exe"),
                process_name: process_name.to_string(),
                window_class: "X".to_string(),
                registered_title: "t".to_string(),
                title_contains: None,
                title_regex: None,
            },
            main_placement: SavedPlacement {
                monitor_id: "A".to_string(),
                main_monitor_index: 0,
                normalized_rect: NormalizedRect {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                },
                physical_rect_at_capture: PixelRect::new(0, 0, 800, 600),
                show_state: SavedShowState::Normal,
            },
            z_order: 0,
            launch_spec: None,
            minimize_when_parked: false,
        });
        ws
    }

    #[test]
    fn contains_chatgpt_codex_app_detects_the_app_by_process_name() {
        assert!(contains_chatgpt_codex_app(&workset_with_process(
            "ChatGPT.exe"
        )));
        assert!(!contains_chatgpt_codex_app(&workset_with_process(
            "brave.exe"
        )));
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

    #[test]
    fn resolve_workspace_file_reads_the_first_folders_absolute_path() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let ws_path = dir.path().join("proj.code-workspace");
        std::fs::write(
            &ws_path,
            format!(
                r#"{{"folders": [{{"name": "proj", "path": "{}"}}], "settings": {{}}}}"#,
                repo.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();

        assert_eq!(resolve_workspace_file(&ws_path).unwrap(), repo);
    }

    #[test]
    fn resolve_workspace_file_resolves_a_relative_path_against_its_own_parent() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let ws_path = dir.path().join("proj.code-workspace");
        std::fs::write(&ws_path, r#"{"folders": [{"path": "repo"}]}"#).unwrap();

        assert_eq!(resolve_workspace_file(&ws_path).unwrap(), repo);
    }

    #[test]
    fn resolve_workspace_file_uses_only_the_first_of_multiple_folders() {
        let dir = tempdir().unwrap();
        let ws_path = dir.path().join("proj.code-workspace");
        std::fs::write(
            &ws_path,
            r#"{"folders": [{"path": "first"}, {"path": "second"}]}"#,
        )
        .unwrap();

        assert_eq!(
            resolve_workspace_file(&ws_path).unwrap(),
            dir.path().join("first")
        );
    }

    #[test]
    fn resolve_workspace_file_rejects_a_workspace_with_no_folders() {
        let dir = tempdir().unwrap();
        let ws_path = dir.path().join("empty.code-workspace");
        std::fs::write(&ws_path, r#"{"folders": []}"#).unwrap();

        assert_eq!(
            resolve_workspace_file(&ws_path),
            Err(WorkspaceFileError::NoFolders)
        );
    }

    #[test]
    fn resolve_workspace_file_rejects_invalid_json() {
        let dir = tempdir().unwrap();
        let ws_path = dir.path().join("broken.code-workspace");
        std::fs::write(&ws_path, "not json at all").unwrap();

        assert_eq!(
            resolve_workspace_file(&ws_path),
            Err(WorkspaceFileError::ParseFailed)
        );
    }

    #[test]
    fn resolve_workspace_file_rejects_a_missing_file() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope.code-workspace");

        assert_eq!(
            resolve_workspace_file(&missing),
            Err(WorkspaceFileError::ReadFailed)
        );
    }

    #[test]
    fn resolve_match_path_resolves_a_workspace_kind_to_its_underlying_folder() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let ws_path = dir.path().join("proj.code-workspace");
        std::fs::write(&ws_path, r#"{"folders": [{"path": "repo"}]}"#).unwrap();

        assert_eq!(
            resolve_match_path(&ws_path, RepositoryKind::Workspace),
            repo
        );
    }

    #[test]
    fn resolve_match_path_falls_back_to_the_literal_path_when_the_workspace_file_is_gone() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("gone.code-workspace");

        assert_eq!(
            resolve_match_path(&missing, RepositoryKind::Workspace),
            missing
        );
    }

    #[test]
    fn resolve_match_path_returns_non_workspace_kinds_unchanged() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");

        assert_eq!(resolve_match_path(&repo, RepositoryKind::Git), repo);
        assert_eq!(resolve_match_path(&repo, RepositoryKind::Directory), repo);
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
            launch_spec: None,
            minimize_when_parked: false,
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

    #[test]
    fn priority_workset_claims_a_shared_window_before_earlier_order() {
        // Both worksets register the *same* window (shared): identical matcher.
        let matcher = WindowMatcher {
            executable_path: PathBuf::from(r"C:\code.exe"),
            process_name: "code.exe".to_string(),
            window_class: "Chrome_WidgetWin_1".to_string(),
            registered_title: "repodeck".to_string(),
            title_contains: None,
            title_regex: None,
        };
        let managed_a = ManagedWindow {
            id: Uuid::new_v4(),
            matcher: matcher.clone(),
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
            launch_spec: None,
            minimize_when_parked: false,
        };
        let mut managed_b = managed_a.clone();
        managed_b.id = Uuid::new_v4();

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
        let b_id = workset_b.id;
        let live = [window_with(
            1,
            r"C:\code.exe",
            "Chrome_WidgetWin_1",
            "repodeck",
        )];

        // B is the priority (e.g. the switch target), so it claims the shared
        // window even though A comes first in order.
        let decisions = resolve_all_matches_with_bindings(
            &[workset_a, workset_b],
            &live,
            &HashMap::new(),
            Some(b_id),
        );
        assert_eq!(
            decisions[&managed_b.id],
            MatchDecision::AutoRebind { hwnd: 1 }
        );
        assert_ne!(
            decisions[&managed_a.id],
            MatchDecision::AutoRebind { hwnd: 1 }
        );
    }

    /// 登録アプリから宣言したエントリ（クラス未学習、タイトルはアプリ名だけ）。
    fn declared_window(exe: &str, app_name: &str) -> ManagedWindow {
        ManagedWindow {
            id: Uuid::new_v4(),
            matcher: WindowMatcher {
                executable_path: PathBuf::from(exe),
                process_name: "app.exe".to_string(),
                window_class: String::new(),
                registered_title: app_name.to_string(),
                title_contains: Some(app_name.to_string()),
                title_regex: None,
            },
            main_placement: SavedPlacement {
                monitor_id: "A".to_string(),
                main_monitor_index: 0,
                normalized_rect: crate::domain::placement::NormalizedRect {
                    x: 0.0,
                    y: 0.0,
                    width: 0.5,
                    height: 1.0,
                },
                physical_rect_at_capture: PixelRect::new(0, 0, 0, 0),
                show_state: SavedShowState::Normal,
            },
            z_order: 0,
            launch_spec: None,
            minimize_when_parked: false,
        }
    }

    fn single_set(windows: Vec<ManagedWindow>) -> Workset {
        build_workset(
            "S".to_string(),
            "#fff".to_string(),
            PathBuf::from(r"D:\s"),
            RepositoryKind::Git,
            0,
            windows,
        )
    }

    /// 実機で190件（現役の窓は22個）まで溜まっていた（2026-07-26）。残骸が現役の窓を
    /// 握っているように見え、ChatGPT セットが永久にバインドできなくなっていた。
    #[test]
    fn bindings_of_deleted_registrations_are_pruned() {
        let live_window = declared_window(r"C:\x\app.exe", "App");
        let live_id = live_window.id;
        let set = single_set(vec![live_window]);
        let deleted_a = Uuid::new_v4();
        let deleted_b = Uuid::new_v4();
        let mut bindings: HashMap<Uuid, isize> =
            [(live_id, 11), (deleted_a, 265198), (deleted_b, 265198)]
                .into_iter()
                .collect();

        let pruned = prune_window_bindings(&mut bindings, std::slice::from_ref(&set));

        assert_eq!(pruned, 2);
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings.get(&live_id), Some(&11));
    }

    /// 窓が消えている紐づけは残骸ではない。あれがあると
    /// `resolve_all_matches_with_bindings` は「閉じられたエントリ」と読んで、再発見に
    /// `has_title_evidence` の厳しい条件を課す。消すと素の照合へ落ちて、閉じた
    /// ブラウザが無関係な別の窓を取り込む挙動が戻る。
    #[test]
    fn a_binding_whose_window_is_gone_is_kept_because_it_still_carries_meaning() {
        let managed = declared_window(r"C:\x\app.exe", "App");
        let id = managed.id;
        let set = single_set(vec![managed]);
        // 999 はもう存在しない窓。それでも登録は生きている。
        let mut bindings: HashMap<Uuid, isize> = [(id, 999)].into_iter().collect();

        let pruned = prune_window_bindings(&mut bindings, std::slice::from_ref(&set));

        assert_eq!(pruned, 0);
        assert_eq!(bindings.get(&id), Some(&999));
    }

    #[test]
    fn a_declared_window_holds_its_binding_even_though_no_class_was_registered() {
        // クラス一致を要求していたため、宣言アプリはバインドを一切保持できず、
        // 開き直しても紐づかなかった（VS Code / LibreHardwareMonitor / RDP）。
        let managed = declared_window(r"C:\tool\LHM.exe", "Libre Hardware Monitor");
        let set = single_set(vec![managed.clone()]);
        let live = [window_with(
            42,
            r"C:\tool\LHM.exe",
            "WindowsForms10.Window.8.app.0.1",
            "Libre Hardware Monitor",
        )];
        let bindings: HashMap<Uuid, isize> = [(managed.id, 42)].into_iter().collect();

        let decisions = resolve_all_matches_with_bindings(&[set], &live, &bindings, None);
        assert_eq!(
            decisions[&managed.id],
            MatchDecision::AutoRebind { hwnd: 42 }
        );
    }

    #[test]
    fn a_closed_browser_does_not_adopt_another_window_of_the_same_browser() {
        // Brave を閉じたのに、別の Brave ウィンドウへ紐づき直してしまい
        // 「開き直せる閉じたアプリはありませんでした」になっていた。
        let managed = declared_window(r"C:\brave\brave.exe", "Brave");
        let set = single_set(vec![managed.clone()]);
        // このエントリが結びついていた 10 は消え、無関係な Brave の窓 11 だけが残る。
        let live = [window_with(
            11,
            r"C:\brave\brave.exe",
            "Chrome_WidgetWin_1",
            "GitHub - Brave",
        )];
        let bindings: HashMap<Uuid, isize> = [(managed.id, 10)].into_iter().collect();

        let decisions = resolve_all_matches_with_bindings(&[set], &live, &bindings, None);
        assert_eq!(decisions[&managed.id], MatchDecision::Unresolved);
    }

    #[test]
    fn a_script_launcher_adopts_the_browser_window_it_started() {
        let managed = declared_window(
            r"C:\Windows\System32\wscript.exe",
            "DeepSeek Harness Web UI",
        );
        let set = single_set(vec![managed.clone()]);
        let live = [window_with(
            31,
            r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe",
            "Chrome_WidgetWin_1",
            "テスト — DeepSeek Harness - Brave",
        )];

        let decisions = resolve_all_matches(&[set], &live);

        assert_eq!(
            decisions[&managed.id],
            MatchDecision::AutoRebind { hwnd: 31 }
        );
    }

    #[test]
    fn a_closed_vscode_window_is_still_re_found_by_its_folder_in_the_title() {
        // 一方、タイトルで自分だと分かるものは内容マッチでの再発見を許す：
        // ユーザーが同じフォルダを開き直した VS Code は取り込んでよい。
        let mut managed = declared_window(r"C:\VS\Code.exe", "Visual Studio Code");
        managed.matcher.title_contains = Some("repo01".to_string());
        let set = single_set(vec![managed.clone()]);
        let live = [
            window_with(
                11,
                r"C:\VS\Code.exe",
                "Chrome_WidgetWin_1",
                "repo01 - Visual Studio Code",
            ),
            window_with(
                12,
                r"C:\VS\Code.exe",
                "Chrome_WidgetWin_1",
                "repo07 - Visual Studio Code",
            ),
        ];
        let bindings: HashMap<Uuid, isize> = [(managed.id, 10)].into_iter().collect();

        let decisions = resolve_all_matches_with_bindings(&[set], &live, &bindings, None);
        assert_eq!(
            decisions[&managed.id],
            MatchDecision::AutoRebind { hwnd: 11 }
        );
    }

    #[test]
    fn without_any_binding_a_declared_entry_still_adopts_a_running_window() {
        // バインドが無い＝まだ一度も結びついていない（再起動後など）。この段階では
        // 既に起動している窓を取り込めたほうが便利なので、従来どおり内容マッチ。
        let managed = declared_window(r"C:\brave\brave.exe", "Brave");
        let set = single_set(vec![managed.clone()]);
        let live = [window_with(
            11,
            r"C:\brave\brave.exe",
            "Chrome_WidgetWin_1",
            "GitHub - Brave",
        )];

        let decisions = resolve_all_matches_with_bindings(&[set], &live, &HashMap::new(), None);
        assert_eq!(
            decisions[&managed.id],
            MatchDecision::AutoRebind { hwnd: 11 }
        );
    }
}
