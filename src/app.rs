use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use slint::Model;
use tracing_appender::non_blocking::WorkerGuard;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, INFINITE, WaitForSingleObject,
};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::core::HSTRING;

use crate::application::layout_service::{self, UndoEntry, UndoSnapshot};
use crate::diagnostics::logging;
use crate::domain::config::AppConfig;
use crate::domain::monitor::AutoSplit;
use crate::domain::placement::SavedShowState;
use crate::persistence::config_store;
use crate::windowing::enumerate::{self, TopLevelWindow};
use crate::windowing::monitor::{self, MonitorInfo};
use crate::windowing::placement as win_placement;

slint::include_modules!();

/// Local (session-scoped) mutex name used to detect a second RepoDeck instance.
const SINGLE_INSTANCE_MUTEX_NAME: &str = r"Local\RepoDeck.SingleInstance.v1";

/// Local (session-scoped) event a second launch signals to ask the already-running
/// instance to show its window, per PLAN.md §3.2 ("二重起動された場合、既存プロセスへ
/// 「クイックスイッチャー表示」を通知し、新プロセスは終了する"). MVP shows the main
/// window rather than the (not yet implemented) quick switcher.
const SHOW_REQUEST_EVENT_NAME: &str = r"Local\RepoDeck.ShowRequested.v1";

/// Identifies this process to Windows (taskbar grouping, notification/tray icon
/// identity) as distinct from other apps. Must be set before any window or tray
/// icon is created.
const APP_USER_MODEL_ID: &str = "RepoDeck.App";

/// Logical-pixel size assumed for the monitor canvas projection (PLAN.md §15's
/// "モニター図は実座標比率を維持"). This matches `layout-studio.slint`'s default
/// window size; it is not recomputed if the user resizes the window, so very
/// large/small windows will show the canvas letterboxed rather than filling it.
const CANVAS_WIDTH: f64 = 700.0;
const CANVAS_HEIGHT: f64 = 480.0;
const CANVAS_PADDING: f64 = 12.0;

/// A raw `HANDLE` isn't `Send` by default (it's a bare `*mut c_void`), but Win32
/// kernel object handles are safe to hand to another thread and wait on there —
/// that's exactly what `WaitForSingleObject` is for.
struct SendHandle(HANDLE);

// SAFETY: see the doc comment above; Win32 documents wait/signal handles as safe
// to use from any thread.
unsafe impl Send for SendHandle {}

struct InstanceMutex(HANDLE);

impl Drop for InstanceMutex {
    fn drop(&mut self) {
        // SAFETY: `self.0` was created by `CreateMutexW` in `acquire_single_instance`
        // and is not used anywhere else.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Returns `%LOCALAPPDATA%\RepoDeck`. Fails loudly if `LOCALAPPDATA` is unavailable
/// rather than silently falling back to the current directory (PLAN.md §7.1).
pub fn local_app_data_dir() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").context(
        "LOCALAPPDATA environment variable is not set; cannot locate RepoDeck's data directory",
    )?;
    Ok(PathBuf::from(base).join("RepoDeck"))
}

/// Attempts to become the single running RepoDeck instance.
///
/// Returns `Ok(None)` when another instance already holds the mutex, in which
/// case the caller must exit without performing any further startup work.
fn acquire_single_instance() -> Result<Option<InstanceMutex>> {
    let name = HSTRING::from(SINGLE_INSTANCE_MUTEX_NAME);

    // SAFETY: All arguments are valid for the duration of the call; `lpname`
    // is a well-formed, NUL-terminated wide string owned by `name`.
    let handle = unsafe { CreateMutexW(None, true, &name) }
        .context("failed to create the single-instance mutex")?;

    // SAFETY: `GetLastError` reads thread-local state set by the immediately
    // preceding `CreateMutexW` call.
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;

    if already_running {
        // SAFETY: `handle` is a valid, owned handle we are done with.
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Ok(None);
    }

    Ok(Some(InstanceMutex(handle)))
}

/// Creates (or, if it already exists, opens) the auto-reset event used to ask the
/// running instance to show its window. Same-user processes can both create and
/// signal it: `CreateEventW`'s default security attributes grant the creating
/// user full access, and a second `CreateEventW` call with the same name returns
/// a handle to the same kernel object rather than failing.
fn open_show_request_event() -> Result<HANDLE> {
    let name = HSTRING::from(SHOW_REQUEST_EVENT_NAME);

    // SAFETY: `name` is a valid, NUL-terminated wide string for the duration of
    // the call; `manual_reset = false` and `initial_state = false` are plain values.
    unsafe { CreateEventW(None, false, false, &name) }
        .context("failed to create or open the show-request event")
}

/// Signals the already-running RepoDeck instance (if any) to show its window.
/// Called by a second launch after `acquire_single_instance` finds the mutex
/// already held.
fn request_running_instance_to_show() -> Result<()> {
    use windows::Win32::System::Threading::SetEvent;

    let event = open_show_request_event()?;
    // SAFETY: `event` is a valid, owned handle for the duration of this call.
    let result = unsafe { SetEvent(event) };
    // SAFETY: `event` is a valid, owned handle we are done with.
    unsafe {
        let _ = CloseHandle(event);
    }
    result.context("failed to signal the show-request event")
}

fn set_app_user_model_id() {
    let id = HSTRING::from(APP_USER_MODEL_ID);
    // SAFETY: `id` is a valid, NUL-terminated wide string for the duration of the call.
    if let Err(err) = unsafe { SetCurrentProcessExplicitAppUserModelID(&id) } {
        tracing::warn!(error = %err, "failed to set the process AppUserModelID");
    }
}

fn load_or_default_config(data_dir: &Path) -> AppConfig {
    match config_store::load(data_dir) {
        Ok(Some(result)) => {
            if result.recovered_from_backup {
                tracing::warn!(
                    "config.json was unreadable; recovered settings from config.backup.json"
                );
            }
            result.config
        }
        Ok(None) => {
            tracing::info!("no config.json found; starting from defaults (first run)");
            AppConfig::new_empty()
        }
        Err(err) => {
            tracing::error!(error = %err, "failed to load config.json; starting from defaults");
            AppConfig::new_empty()
        }
    }
}

/// UI-only Layout Studio state that isn't part of the persisted [`AppConfig`]:
/// the last enumerated monitor list, in-progress main-selection edits, which
/// tile is selected, auto-split choices not yet saved, and the single rolling
/// undo snapshot for "メインを空にする" (PLAN.md §3.5).
struct LayoutStudioState {
    monitors: Vec<MonitorInfo>,
    auto_splits: HashMap<String, AutoSplit>,
    selecting_main: bool,
    pending_main_ids: Vec<String>,
    selected_index: Option<usize>,
    undo_snapshot: UndoSnapshot,
    pending_candidates: Vec<TopLevelWindow>,
}

impl LayoutStudioState {
    fn new() -> Self {
        Self {
            monitors: Vec::new(),
            auto_splits: HashMap::new(),
            selecting_main: false,
            pending_main_ids: Vec::new(),
            selected_index: None,
            undo_snapshot: UndoSnapshot::default(),
            pending_candidates: Vec::new(),
        }
    }

    fn auto_split_for(&self, config: &AppConfig, device_name: &str) -> AutoSplit {
        self.auto_splits
            .get(device_name)
            .copied()
            .or_else(|| {
                config
                    .monitors
                    .iter()
                    .find(|saved| saved.stable_id == device_name)
                    .map(|saved| saved.auto_split)
            })
            .unwrap_or(AutoSplit::One)
    }
}

fn auto_split_label(split: AutoSplit) -> &'static str {
    match split {
        AutoSplit::One => "AUTO (1分割)",
        AutoSplit::TwoColumns => "AUTO (2分割)",
        AutoSplit::FourGrid => "AUTO (4分割)",
    }
}

/// Rebuilds `layout`'s `monitor-tiles` model from a fresh Win32 monitor
/// enumeration, reflecting the in-progress main-selection (if any) and each
/// monitor's chosen auto-split. Called on open, on explicit refresh, and after
/// any interaction that changes what a tile should show.
fn refresh_monitor_tiles(layout: &LayoutStudio, config: &AppConfig, state: &mut LayoutStudioState) {
    let monitors = match monitor::enumerate_monitors() {
        Ok(monitors) => monitors,
        Err(err) => {
            layout.set_status_text(format!("モニター一覧の取得に失敗しました: {err}").into());
            layout.set_status_is_warning(true);
            return;
        }
    };

    let main_ids: &[String] = if state.selecting_main {
        &state.pending_main_ids
    } else {
        &config.main_monitor_ids
    };

    let bounds: Vec<_> = monitors.iter().map(|m| m.bounds_px).collect();
    let canvas_rects = layout_service::project_monitors_to_canvas(
        &bounds,
        CANVAS_WIDTH,
        CANVAS_HEIGHT,
        CANVAS_PADDING,
    );

    let tiles: Vec<MonitorTile> = monitors
        .iter()
        .zip(canvas_rects.iter())
        .enumerate()
        .map(|(index, (monitor, rect))| {
            let main_order = main_ids.iter().position(|id| id == &monitor.device_name);
            let split = state.auto_split_for(config, &monitor.device_name);
            let role_label = match main_order {
                Some(order) => format!("MAIN {}", order + 1),
                None => auto_split_label(split).to_string(),
            };
            let dpi_percent = monitor.dpi_x * 100 / 96;

            MonitorTile {
                x: rect.x as f32,
                y: rect.y as f32,
                width: rect.width as f32,
                height: rect.height as f32,
                device_name: monitor.device_name.clone().into(),
                detail_label: format!(
                    "{}×{}  {dpi_percent}%",
                    monitor.bounds_px.width, monitor.bounds_px.height
                )
                .into(),
                role_label: role_label.into(),
                is_main: main_order.is_some(),
                selected: state.selected_index == Some(index),
            }
        })
        .collect();

    layout.set_monitor_tiles(std::rc::Rc::new(slint::VecModel::from(tiles)).into());
    state.monitors = monitors;
}

/// Updates the right-hand property pane for the currently selected monitor.
fn refresh_selected_monitor_panel(
    layout: &LayoutStudio,
    config: &AppConfig,
    state: &LayoutStudioState,
) {
    let Some(index) = state.selected_index else {
        layout.set_selected_has_monitor(false);
        layout.set_selected_is_main(false);
        layout.set_selected_monitor_detail("モニターを選択してください".into());
        return;
    };
    let Some(monitor) = state.monitors.get(index) else {
        layout.set_selected_has_monitor(false);
        return;
    };

    let is_main = config
        .main_monitor_ids
        .iter()
        .any(|id| id == &monitor.device_name);
    let split = state.auto_split_for(config, &monitor.device_name);

    layout.set_selected_has_monitor(true);
    layout.set_selected_is_main(is_main);
    layout.set_selected_auto_split(match split {
        AutoSplit::One => 1,
        AutoSplit::TwoColumns => 2,
        AutoSplit::FourGrid => 4,
    });
    layout.set_selected_monitor_detail(
        format!(
            "{}\n{}×{} (作業領域 {}×{})\nDPI {}%{}",
            monitor.device_name,
            monitor.bounds_px.width,
            monitor.bounds_px.height,
            monitor.work_area_px.width,
            monitor.work_area_px.height,
            monitor.dpi_x * 100 / 96,
            if monitor.is_primary {
                "\nWindowsのプライマリモニター"
            } else {
                ""
            },
        )
        .into(),
    );
}

fn resolve_main_monitor_bounds(
    monitors: &[MonitorInfo],
    main_ids: &[String],
) -> Vec<crate::domain::placement::PixelRect> {
    main_ids
        .iter()
        .filter_map(|id| {
            monitors
                .iter()
                .find(|m| &m.device_name == id)
                .map(|m| m.bounds_px)
        })
        .collect()
}

/// Undoes the effect of a confirmed "メインを空にする" (PLAN.md §3.5): restores
/// each window's rectangle and, for windows that were maximized, re-maximizes
/// them after first placing them at their saved normal rectangle (mirroring
/// PLAN.md §4.2's restore order).
fn perform_undo(snapshot: &UndoSnapshot) {
    for entry in &snapshot.entries {
        let hwnd = HWND(entry.hwnd as *mut _);
        win_placement::restore(hwnd);
        if let Err(err) = win_placement::set_window_rect(hwnd, entry.before_rect) {
            tracing::warn!(error = %err, hwnd = entry.hwnd, "undo: failed to restore window rect");
            continue;
        }
        match entry.before_show_state {
            SavedShowState::Maximized => win_placement::maximize(hwnd),
            SavedShowState::Minimized => win_placement::minimize(hwnd),
            SavedShowState::Normal => {}
        }
    }
}

/// Wires every Layout Studio callback to `config`/`state`. Called once, right
/// after the window is created.
fn wire_layout_studio(
    layout: &LayoutStudio,
    data_dir: PathBuf,
    config: Rc<RefCell<AppConfig>>,
    state: Rc<RefCell<LayoutStudioState>>,
) {
    {
        let mut state = state.borrow_mut();
        refresh_monitor_tiles(layout, &config.borrow(), &mut state);
    }

    let l = layout.as_weak();
    let c = config.clone();
    let s = state.clone();
    layout.on_refresh_requested(move || {
        if let Some(layout) = l.upgrade() {
            let mut state = s.borrow_mut();
            refresh_monitor_tiles(&layout, &c.borrow(), &mut state);
            layout.set_status_text("モニター一覧を更新しました。".into());
            layout.set_status_is_warning(false);
        }
    });

    let l = layout.as_weak();
    let c = config.clone();
    let s = state.clone();
    layout.on_toggle_select_main_mode(move || {
        if let Some(layout) = l.upgrade() {
            let mut state = s.borrow_mut();
            let config = c.borrow();
            if state.selecting_main {
                state.selecting_main = false;
                layout.set_status_text("メイン画面選択をキャンセルしました。".into());
            } else {
                state.selecting_main = true;
                state.pending_main_ids = config.main_monitor_ids.clone();
                layout.set_status_text("メイン画面にするモニターをクリックしてください。".into());
            }
            layout.set_status_is_warning(false);
            layout.set_selecting_main(state.selecting_main);
            refresh_monitor_tiles(&layout, &config, &mut state);
        }
    });

    let l = layout.as_weak();
    let c = config.clone();
    let s = state.clone();
    layout.on_monitor_clicked(move |index| {
        let Some(layout) = l.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let mut state = s.borrow_mut();
        let config = c.borrow();

        if state.selecting_main {
            let Some(monitor) = state.monitors.get(index) else {
                return;
            };
            let device_name = monitor.device_name.clone();
            if let Some(pos) = state
                .pending_main_ids
                .iter()
                .position(|id| id == &device_name)
            {
                state.pending_main_ids.remove(pos);
            } else {
                state.pending_main_ids.push(device_name);
            }
        } else {
            state.selected_index = Some(index);
            refresh_selected_monitor_panel(&layout, &config, &state);
        }
        refresh_monitor_tiles(&layout, &config, &mut state);
    });

    let l = layout.as_weak();
    let c = config.clone();
    let s = state.clone();
    layout.on_apply_main_selection(move || {
        let Some(layout) = l.upgrade() else { return };
        let mut state = s.borrow_mut();
        let mut config = c.borrow_mut();

        if state.pending_main_ids.is_empty() {
            layout.set_status_text("メイン画面は1台以上選択してください。".into());
            layout.set_status_is_warning(true);
            return;
        }

        config.main_monitor_ids = state.pending_main_ids.clone();
        state.selecting_main = false;
        layout.set_selecting_main(false);
        layout
            .set_status_text("メイン画面を更新しました。「設定を保存」で確定してください。".into());
        layout.set_status_is_warning(false);
        refresh_monitor_tiles(&layout, &config, &mut state);
    });

    let l = layout.as_weak();
    let c = config.clone();
    let s = state.clone();
    layout.on_set_auto_split(move |monitor_index, split| {
        let Some(layout) = l.upgrade() else { return };
        let Ok(monitor_index) = usize::try_from(monitor_index) else {
            return;
        };
        let mut state = s.borrow_mut();
        let config = c.borrow();

        let Some(monitor) = state.monitors.get(monitor_index) else {
            return;
        };
        let device_name = monitor.device_name.clone();
        let split = match split {
            2 => AutoSplit::TwoColumns,
            4 => AutoSplit::FourGrid,
            _ => AutoSplit::One,
        };
        state.auto_splits.insert(device_name, split);

        refresh_selected_monitor_panel(&layout, &config, &state);
        refresh_monitor_tiles(&layout, &config, &mut state);
    });

    let l = layout.as_weak();
    let c = config.clone();
    let s = state.clone();
    let pid = std::process::id();
    layout.on_empty_main_screen_requested(move || {
        let Some(layout) = l.upgrade() else { return };
        let mut state = s.borrow_mut();
        let config = c.borrow();

        if config.main_monitor_ids.is_empty() {
            layout.set_status_text(
                "メイン画面が未設定です。先に「メイン画面を選択」してください。".into(),
            );
            layout.set_status_is_warning(true);
            return;
        }

        let main_bounds = resolve_main_monitor_bounds(&state.monitors, &config.main_monitor_ids);
        let windows = match enumerate::enumerate_top_level_windows(pid) {
            Ok(windows) => windows,
            Err(err) => {
                layout.set_status_text(format!("ウィンドウ一覧の取得に失敗しました: {err}").into());
                layout.set_status_is_warning(true);
                return;
            }
        };
        let candidates = layout_service::find_windows_on_main_screen(&windows, &main_bounds);

        if candidates.is_empty() {
            layout.set_status_text("メイン画面に対象ウィンドウはありませんでした。".into());
            layout.set_status_is_warning(false);
            return;
        }

        use crate::domain::config::UnknownWindowPolicy;
        match config.settings.unknown_window_policy {
            UnknownWindowPolicy::LeaveInPlace => {
                layout.set_status_text(
                    format!(
                        "{}個の未登録ウィンドウがありますが、設定方針によりそのままにしました。",
                        candidates.len()
                    )
                    .into(),
                );
                layout.set_status_is_warning(false);
            }
            UnknownWindowPolicy::Ask => {
                let model_items: Vec<MainScreenCandidate> = candidates
                    .iter()
                    .map(|w| MainScreenCandidate {
                        title: w.title.clone().into(),
                        process_name: w
                            .executable_path
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| w.window_class.clone())
                            .into(),
                        checked: true,
                    })
                    .collect();
                layout.set_main_screen_candidates(
                    std::rc::Rc::new(slint::VecModel::from(model_items)).into(),
                );
                state.pending_candidates = candidates;
                layout.set_empty_screen_panel_visible(true);
                layout.set_status_text("最小化するウィンドウを確認してください。".into());
                layout.set_status_is_warning(false);
            }
        }
    });

    let l = layout.as_weak();
    layout.on_candidate_toggled(move |index| {
        let Some(layout) = l.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let model = layout.get_main_screen_candidates();
        if let Some(mut row) = model.row_data(index) {
            row.checked = !row.checked;
            model.set_row_data(index, row);
        }
    });

    let l = layout.as_weak();
    let s = state.clone();
    layout.on_cancel_empty_main_screen(move || {
        let Some(layout) = l.upgrade() else { return };
        s.borrow_mut().pending_candidates.clear();
        layout.set_empty_screen_panel_visible(false);
        layout.set_status_text("キャンセルしました。".into());
        layout.set_status_is_warning(false);
    });

    let l = layout.as_weak();
    let s = state.clone();
    layout.on_confirm_empty_main_screen(move || {
        let Some(layout) = l.upgrade() else { return };
        let mut state = s.borrow_mut();

        let model = layout.get_main_screen_candidates();
        let mut entries = Vec::new();
        for (index, window) in state.pending_candidates.iter().enumerate() {
            let checked = model.row_data(index).map(|row| row.checked).unwrap_or(false);
            if !checked {
                continue;
            }

            let hwnd = HWND(window.hwnd as *mut _);
            let before_show_state = match win_placement::get_show_state(hwnd) {
                Ok(state) => state,
                Err(err) => {
                    tracing::warn!(error = %err, hwnd = window.hwnd, "skipping window: failed to read show state");
                    continue;
                }
            };
            let before_rect = match win_placement::get_normal_rect(hwnd) {
                Ok(rect) => rect,
                Err(err) => {
                    tracing::warn!(error = %err, hwnd = window.hwnd, "skipping window: failed to read normal rect");
                    continue;
                }
            };

            win_placement::minimize(hwnd);
            entries.push(UndoEntry { hwnd: window.hwnd, process_id: window.process_id, before_rect, before_show_state });
        }

        let minimized_count = entries.len();
        state.undo_snapshot = UndoSnapshot { entries };
        state.pending_candidates.clear();

        layout.set_undo_available(!state.undo_snapshot.is_empty());
        layout.set_empty_screen_panel_visible(false);
        layout.set_status_text(format!("{minimized_count}個のウィンドウを最小化しました。").into());
        layout.set_status_is_warning(false);
    });

    let l = layout.as_weak();
    let s = state.clone();
    layout.on_undo_requested(move || {
        let Some(layout) = l.upgrade() else { return };
        let mut state = s.borrow_mut();

        perform_undo(&state.undo_snapshot);
        let count = state.undo_snapshot.entries.len();
        state.undo_snapshot = UndoSnapshot::default();

        layout.set_undo_available(false);
        layout.set_status_text(format!("{count}個のウィンドウを元に戻しました。").into());
        layout.set_status_is_warning(false);
    });

    let l = layout.as_weak();
    let c = config.clone();
    let s = state.clone();
    layout.on_save_requested(move || {
        let Some(layout) = l.upgrade() else { return };
        let state = s.borrow();
        let mut config = c.borrow_mut();

        config.monitors = state
            .monitors
            .iter()
            .map(|monitor| crate::domain::monitor::SavedMonitor {
                stable_id: monitor.device_name.clone(),
                device_name: monitor.device_name.clone(),
                device_path: None,
                friendly_name: None,
                bounds_px: monitor.bounds_px,
                work_area_px: monitor.work_area_px,
                dpi_x: monitor.dpi_x,
                dpi_y: monitor.dpi_y,
                auto_split: state.auto_split_for(&config, &monitor.device_name),
            })
            .collect();

        let errors = config.validate();
        if !errors.is_empty() {
            let message = errors
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(" / ");
            layout.set_status_text(format!("保存できません: {message}").into());
            layout.set_status_is_warning(true);
            return;
        }

        match config_store::save(&data_dir, &config) {
            Ok(()) => {
                layout.set_status_text("設定を保存しました。".into());
                layout.set_status_is_warning(false);
            }
            Err(err) => {
                layout.set_status_text(format!("設定の保存に失敗しました: {err}").into());
                layout.set_status_is_warning(true);
            }
        }
    });
}

pub fn run() -> Result<()> {
    set_app_user_model_id();

    let data_dir = local_app_data_dir()?;
    std::fs::create_dir_all(&data_dir).with_context(|| {
        format!(
            "failed to create RepoDeck data directory {}",
            data_dir.display()
        )
    })?;

    let _log_guard: WorkerGuard = logging::init(&data_dir)?;

    let Some(_instance_mutex) = acquire_single_instance()? else {
        tracing::info!(
            "another RepoDeck instance is already running; asking it to show its window"
        );
        if let Err(err) = request_running_instance_to_show() {
            tracing::warn!(error = %err, "failed to signal the running RepoDeck instance");
        }
        return Ok(());
    };

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "RepoDeck starting");

    let config = Rc::new(RefCell::new(load_or_default_config(&data_dir)));
    let layout_studio_state = Rc::new(RefCell::new(LayoutStudioState::new()));

    let window = AppWindow::new().context("failed to create the RepoDeck main window")?;
    let tray = TrayIcon::new().context("failed to create the RepoDeck tray icon")?;
    let layout_studio = LayoutStudio::new().context("failed to create the Layout Studio window")?;

    wire_layout_studio(&layout_studio, data_dir, config, layout_studio_state);

    // Left-click on the tray icon or "RepoDeckを開く" in its menu (both wired to
    // `open-requested` in app-window.slint) bring the main window back. Closing
    // the window (the X button) only hides it by default
    // (`CloseRequestResponse::HideWindow`); the tray icon keeps the event loop
    // alive, so the process stays resident until "RepoDeckを終了" is chosen.
    let window_for_open = window.as_weak();
    tray.on_open_requested(move || {
        if let Some(window) = window_for_open.upgrade() {
            let _ = window.show();
        }
    });

    let layout_studio_for_open = layout_studio.as_weak();
    tray.on_layout_studio_requested(move || {
        if let Some(layout_studio) = layout_studio_for_open.upgrade() {
            let _ = layout_studio.show();
        }
    });

    // Re-launching repodeck.exe while an instance is already running (e.g. a
    // taskbar-pinned icon click) signals this event instead of starting a second
    // process (PLAN.md §3.2). A dedicated thread blocks on it and marshals the
    // show request onto the Slint UI thread.
    let show_request_event = SendHandle(open_show_request_event()?);
    let window_for_show_request = window.as_weak();
    std::thread::spawn(move || {
        // Force the whole `SendHandle` to be captured, not just its `.0` field
        // (Rust 2021 disjoint closure capture would otherwise capture the bare,
        // non-`Send` `HANDLE` field directly and defeat the wrapper).
        let show_request_event = show_request_event;
        let show_request_event = show_request_event.0;
        loop {
            // SAFETY: `show_request_event` is a valid, owned handle for the
            // lifetime of this thread (the process owns it until exit).
            let wait_result = unsafe { WaitForSingleObject(show_request_event, INFINITE) };
            if wait_result != WAIT_OBJECT_0 {
                break;
            }

            let window_for_show_request = window_for_show_request.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = window_for_show_request.upgrade() {
                    let _ = window.show();
                }
            });
        }
    });

    tray.on_quit_requested(|| {
        tracing::info!("quit requested from the tray menu");
        let _ = slint::quit_event_loop();
    });

    window
        .show()
        .context("failed to show the RepoDeck main window")?;
    tray.show()
        .context("failed to show the RepoDeck tray icon")?;
    if std::env::var_os("REPODECK_DEBUG_OPEN_LAYOUT_STUDIO").is_some() {
        layout_studio
            .show()
            .context("failed to show Layout Studio")?;
    }

    slint::run_event_loop().context("RepoDeck event loop failed")?;

    tracing::info!("RepoDeck exiting");
    Ok(())
}
