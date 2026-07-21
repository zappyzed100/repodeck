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
use windows::Win32::UI::WindowsAndMessaging::{
    IDNO, IDYES, MB_ICONWARNING, MB_YESNO, MB_YESNOCANCEL, MessageBoxW,
};
use windows::core::HSTRING;

use crate::application::agent_status_service;
use crate::application::crash_recovery::{self, JournalRecoveryChoice};
use crate::application::layout_service::{self, UndoEntry, UndoSnapshot};
use crate::application::monitor_watch_service;
use crate::application::popup_placement;
use crate::application::quick_switcher_service;
use crate::application::switch_coordinator::{SwitchCoordinator, SwitchRequest};
use crate::application::window_ops::WindowOps;
use crate::application::workset_service;
use crate::diagnostics::logging;
use crate::domain::agent::{
    AgentRun, AgentState, row_display_priority, state_color, tray_priority,
};
use crate::domain::config::{AppConfig, HotkeyConfig, HotkeyModifier, SortMode};
use crate::domain::monitor::AutoSplit;
use crate::domain::placement::SavedShowState;
use crate::domain::workset::{ManagedWindow, ParkingPolicy};
use crate::hotkey::win32_hotkey::{self, HotkeyEvent, HotkeyRegisterError, HotkeyThread};
use crate::ipc::named_pipe::{NamedPipeServer, PipeServerEvent};
use crate::ipc::protocol;
use crate::persistence::{clock, config_store, journal_store, runtime_store};
use crate::windowing::autostart;
use crate::windowing::enumerate::{self, TopLevelWindow};
use crate::windowing::matcher::MatchDecision;
use crate::windowing::monitor::{self, MonitorInfo};
use crate::windowing::placement as win_placement;
use crate::windowing::popup_window;
use crate::windowing::window_ops_impl::Win32WindowOps;

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

/// Fallback logical-pixel size for the monitor canvas projection (PLAN.md
/// §15's "モニター図は実座標比率を維持"), used only for the very first tile
/// refresh — before the window has ever been shown, `canvas-frame` (the card
/// the tiles are drawn into) hasn't been laid out yet, so its real size isn't
/// known. Once `layout-studio.slint`'s `canvas-resized` callback reports the
/// card's actual size, `LayoutStudioState::canvas_width/height` are updated to
/// match and every subsequent projection uses the real bounds instead of this
/// guess.
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

/// Applies a dark title bar and a Windows 11 system backdrop (Mica/Acrylic
/// family) to `window`, so its transparent Slint-rendered background shows
/// the real desktop through it — the "glass" look every RepoDeck window uses
/// (see `ui/theme.slint`). Best-effort: silently does nothing on Windows
/// versions or handle types that don't support it. Whether the backdrop
/// renders blurred or perfectly clear depends on the user's own Windows
/// "transparency effects" setting (Settings > Personalization > Colors) —
/// RepoDeck doesn't change that setting itself.
fn apply_glass_backdrop(window: &slint::Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Graphics::Dwm::{
        DWM_SYSTEMBACKDROP_TYPE, DWMSBT_TRANSIENTWINDOW, DWMWA_SYSTEMBACKDROP_TYPE,
        DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute,
    };

    let window_handle = window.window_handle();
    let Ok(handle) = HasWindowHandle::window_handle(&window_handle) else {
        return;
    };
    let RawWindowHandle::Win32(win32_handle) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(isize::from(win32_handle.hwnd) as *mut std::ffi::c_void);

    // SAFETY: `hwnd` came from a live Slint window's raw handle; the attribute
    // buffers match the size Win32 expects for each attribute.
    unsafe {
        let dark_mode = windows::core::BOOL(1);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            std::ptr::from_ref(&dark_mode).cast(),
            u32::try_from(std::mem::size_of_val(&dark_mode)).unwrap(),
        );

        let backdrop = DWMSBT_TRANSIENTWINDOW;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            std::ptr::from_ref(&backdrop).cast(),
            u32::try_from(std::mem::size_of::<DWM_SYSTEMBACKDROP_TYPE>()).unwrap(),
        );
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
    /// The canvas card's real on-screen size, kept in sync with
    /// `canvas-resized` (see `CANVAS_WIDTH`/`CANVAS_HEIGHT`'s doc comment).
    canvas_width: f64,
    canvas_height: f64,
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
            canvas_width: CANVAS_WIDTH,
            canvas_height: CANVAS_HEIGHT,
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
        state.canvas_width,
        state.canvas_height,
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
    layout.on_canvas_resized(move |width, height| {
        let (width, height) = (width as f64, height as f64);
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        let Some(layout) = l.upgrade() else { return };
        let mut state = s.borrow_mut();
        state.canvas_width = width;
        state.canvas_height = height;
        refresh_monitor_tiles(&layout, &c.borrow(), &mut state);
    });

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

fn hex_to_color(hex: &str) -> slint::Color {
    let hex = hex.trim_start_matches('#');
    let r = u8::from_str_radix(hex.get(0..2).unwrap_or("00"), 16).unwrap_or(0);
    let g = u8::from_str_radix(hex.get(2..4).unwrap_or("00"), 16).unwrap_or(0);
    let b = u8::from_str_radix(hex.get(4..6).unwrap_or("00"), 16).unwrap_or(0);
    slint::Color::from_rgb_u8(r, g, b)
}

fn color_to_hex(color: slint::Color) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        color.red(),
        color.green(),
        color.blue()
    )
}

fn match_status_label(decision: Option<&MatchDecision>) -> (String, bool) {
    match decision {
        Some(MatchDecision::AutoRebind { .. }) => ("自動再バインド済み".to_string(), false),
        Some(MatchDecision::Ambiguous { candidates }) => {
            (format!("曖昧: 候補{}件", candidates.len()), true)
        }
        Some(MatchDecision::Unresolved) | None => ("未解決".to_string(), true),
    }
}

/// UI-only Workset Manager state: the last enumerated monitor list (needed to
/// resolve a candidate's main-monitor index at registration time), the
/// in-progress registration form, and which workset is selected in the list.
struct WorksetManagerState {
    monitors: Vec<MonitorInfo>,
    registering: bool,
    picked_repository: Option<(PathBuf, crate::domain::workset::RepositoryKind)>,
    registration_candidates: Vec<TopLevelWindow>,
    selected_color: slint::Color,
    selected_workset_index: Option<usize>,
}

impl WorksetManagerState {
    fn new() -> Self {
        Self {
            monitors: Vec::new(),
            registering: false,
            picked_repository: None,
            registration_candidates: Vec::new(),
            selected_color: hex_to_color("#2563eb"),
            selected_workset_index: None,
        }
    }
}

fn refresh_workset_summaries(
    manager: &WorksetManager,
    config: &AppConfig,
    state: &mut WorksetManagerState,
) {
    state.monitors = monitor::enumerate_monitors().unwrap_or_default();
    let live_windows =
        enumerate::enumerate_top_level_windows(std::process::id()).unwrap_or_default();
    let decisions = workset_service::resolve_all_matches(&config.worksets, &live_windows);

    let summaries: Vec<WorksetSummary> = config
        .worksets
        .iter()
        .map(|workset| {
            let (auto, needs_attention) =
                workset
                    .windows
                    .iter()
                    .fold((0u32, 0u32), |(auto, needs), window| {
                        match decisions.get(&window.id) {
                            Some(MatchDecision::AutoRebind { .. }) => (auto + 1, needs),
                            _ => (auto, needs + 1),
                        }
                    });
            let status_label = if needs_attention == 0 {
                format!("{auto}/{}件 自動再バインド", workset.windows.len())
            } else {
                format!(
                    "{auto}/{}件 自動再バインド・{needs_attention}件要確認",
                    workset.windows.len()
                )
            };

            WorksetSummary {
                name: workset.name.clone().into(),
                repository_path: workset.repository_path.display().to_string().into(),
                window_count: workset.windows.len() as i32,
                color: hex_to_color(&workset.color),
                status_label: status_label.into(),
            }
        })
        .collect();

    manager.set_worksets(std::rc::Rc::new(slint::VecModel::from(summaries)).into());
}

fn refresh_selected_workset_detail(
    manager: &WorksetManager,
    config: &AppConfig,
    state: &WorksetManagerState,
) {
    let Some(index) = state.selected_workset_index else {
        manager.set_selected_workset_windows(
            std::rc::Rc::new(slint::VecModel::from(Vec::<ManagedWindowSummary>::new())).into(),
        );
        return;
    };
    let Some(workset) = config.worksets.get(index) else {
        return;
    };

    let live_windows =
        enumerate::enumerate_top_level_windows(std::process::id()).unwrap_or_default();
    let decisions = workset_service::resolve_all_matches(&config.worksets, &live_windows);

    let rows: Vec<ManagedWindowSummary> = workset
        .windows
        .iter()
        .map(|window| {
            let (label, warn) = match_status_label(decisions.get(&window.id));
            ManagedWindowSummary {
                title: window.matcher.registered_title.clone().into(),
                status_label: label.into(),
                status_is_warning: warn,
            }
        })
        .collect();

    manager.set_selected_workset_windows(std::rc::Rc::new(slint::VecModel::from(rows)).into());
}

fn wire_workset_manager(
    manager: &WorksetManager,
    data_dir: PathBuf,
    config: Rc<RefCell<AppConfig>>,
    state: Rc<RefCell<WorksetManagerState>>,
) {
    {
        let mut state = state.borrow_mut();
        refresh_workset_summaries(manager, &config.borrow(), &mut state);
        manager.set_color_choices(
            std::rc::Rc::new(slint::VecModel::from(
                [
                    "#2563eb", "#16a34a", "#d97706", "#dc2626", "#7c3aed", "#0891b2",
                ]
                .map(hex_to_color)
                .to_vec(),
            ))
            .into(),
        );
        manager.set_selected_color(state.selected_color);
    }

    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    manager.on_refresh_requested(move || {
        let Some(manager) = m.upgrade() else { return };
        let mut state = s.borrow_mut();
        refresh_workset_summaries(&manager, &c.borrow(), &mut state);
        refresh_selected_workset_detail(&manager, &c.borrow(), &state);
    });

    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    manager.on_workset_selected(move |index| {
        let Some(manager) = m.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let mut state = s.borrow_mut();
        if index < c.borrow().worksets.len() {
            state.selected_workset_index = Some(index);
        }
        refresh_selected_workset_detail(&manager, &c.borrow(), &state);
    });

    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    let dir = data_dir.clone();
    manager.on_delete_workset_requested(move || {
        let Some(manager) = m.upgrade() else { return };
        let mut state = s.borrow_mut();
        let Some(index) = state.selected_workset_index else {
            return;
        };

        {
            let mut config = c.borrow_mut();
            if index >= config.worksets.len() {
                return;
            }
            config.worksets.remove(index);
        }
        state.selected_workset_index = None;

        match config_store::save(&dir, &c.borrow()) {
            Ok(()) => {
                manager.set_status_text("セットを削除しました。".into());
                manager.set_status_is_warning(false);
            }
            Err(err) => {
                manager.set_status_text(format!("削除の保存に失敗しました: {err}").into());
                manager.set_status_is_warning(true);
            }
        }
        refresh_workset_summaries(&manager, &c.borrow(), &mut state);
        refresh_selected_workset_detail(&manager, &c.borrow(), &state);
    });

    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    let pid = std::process::id();
    manager.on_start_registration(move || {
        let Some(manager) = m.upgrade() else { return };
        let mut state = s.borrow_mut();
        let config = c.borrow();

        if config.main_monitor_ids.is_empty() {
            manager.set_status_text("メイン画面が未設定です。先にレイアウトスタジオで「メイン画面を選択」してください。".into());
            manager.set_status_is_warning(true);
            return;
        }

        state.monitors = monitor::enumerate_monitors().unwrap_or_default();
        let main_bounds = resolve_main_monitor_bounds(&state.monitors, &config.main_monitor_ids);
        let live_windows = match enumerate::enumerate_top_level_windows(pid) {
            Ok(windows) => windows,
            Err(err) => {
                manager.set_status_text(format!("ウィンドウ一覧の取得に失敗しました: {err}").into());
                manager.set_status_is_warning(true);
                return;
            }
        };
        let candidates = layout_service::find_windows_on_main_screen(&live_windows, &main_bounds);

        if candidates.is_empty() {
            manager.set_status_text("メイン画面に候補ウィンドウがありません。登録したいウィンドウをメイン画面へ配置してください。".into());
            manager.set_status_is_warning(true);
            return;
        }

        let model_items: Vec<RegistrationCandidate> = candidates
            .iter()
            .map(|w| RegistrationCandidate {
                title: w.title.clone().into(),
                process_name: w.executable_path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| w.window_class.clone()).into(),
                checked: true,
            })
            .collect();
        manager.set_candidates(std::rc::Rc::new(slint::VecModel::from(model_items)).into());
        state.registration_candidates = candidates;
        state.picked_repository = None;
        manager.set_picked_folder_label("（未選択）".into());
        manager.set_resolved_repo_label("".into());
        manager.set_registering(true);
        manager.set_status_text("リポジトリフォルダーを選択し、登録するウィンドウを確認してください。".into());
        manager.set_status_is_warning(false);
    });

    let m = manager.as_weak();
    manager.on_cancel_registration(move || {
        let Some(manager) = m.upgrade() else { return };
        manager.set_registering(false);
    });

    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    manager.on_pick_folder_requested(move || {
        let Some(manager) = m.upgrade() else { return };
        let Some(folder) = rfd::FileDialog::new().pick_folder() else {
            return;
        };

        let (repository_path, repository_kind) = workset_service::resolve_repository(&folder);
        if workset_service::is_duplicate_repository(&c.borrow().worksets, &repository_path) {
            manager.set_status_text("このリポジトリは既に登録されています。".into());
            manager.set_status_is_warning(true);
            return;
        }

        manager.set_picked_folder_label(folder.display().to_string().into());
        let kind_label = match repository_kind {
            crate::domain::workset::RepositoryKind::Git => "Gitリポジトリ",
            crate::domain::workset::RepositoryKind::Directory => "通常フォルダー",
        };
        manager.set_resolved_repo_label(
            format!(
                "{kind_label}として登録されます: {}",
                repository_path.display()
            )
            .into(),
        );
        s.borrow_mut().picked_repository = Some((repository_path, repository_kind));
        manager.set_status_text("".into());
        manager.set_status_is_warning(false);
    });

    let m = manager.as_weak();
    manager.on_candidate_toggled(move |index| {
        let Some(manager) = m.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let model = manager.get_candidates();
        if let Some(mut row) = model.row_data(index) {
            row.checked = !row.checked;
            model.set_row_data(index, row);
        }
    });

    let m = manager.as_weak();
    let s = state.clone();
    manager.on_color_selected(move |color| {
        let Some(manager) = m.upgrade() else { return };
        s.borrow_mut().selected_color = color;
        manager.set_selected_color(color);
    });

    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    let dir = data_dir.clone();
    manager.on_register_requested(move |name| {
        let Some(manager) = m.upgrade() else { return };
        let name = name.trim().to_string();
        if name.is_empty() {
            manager.set_status_text("名前を入力してください。".into());
            manager.set_status_is_warning(true);
            return;
        }

        let mut state = s.borrow_mut();
        let Some((repository_path, repository_kind)) = state.picked_repository.clone() else {
            manager.set_status_text("リポジトリフォルダーを選択してください。".into());
            manager.set_status_is_warning(true);
            return;
        };

        let model = manager.get_candidates();
        let checked_windows: Vec<TopLevelWindow> = state
            .registration_candidates
            .iter()
            .enumerate()
            .filter(|(i, _)| model.row_data(*i).is_some_and(|row| row.checked))
            .map(|(_, w)| w.clone())
            .collect();
        if checked_windows.is_empty() {
            manager.set_status_text("ウィンドウを1つ以上選択してください。".into());
            manager.set_status_is_warning(true);
            return;
        }

        let mut managed_windows = Vec::new();
        for (z_order, window) in checked_windows.iter().enumerate() {
            let hwnd = HWND(window.hwnd as *mut _);
            let show_state = match win_placement::get_show_state(hwnd) {
                Ok(show_state) => show_state,
                Err(err) => {
                    tracing::warn!(error = %err, hwnd = window.hwnd, "registration: skipping window, failed to read show state");
                    continue;
                }
            };
            let rect = match win_placement::get_normal_rect(hwnd) {
                Ok(rect) => rect,
                Err(err) => {
                    tracing::warn!(error = %err, hwnd = window.hwnd, "registration: skipping window, failed to read normal rect");
                    continue;
                }
            };
            match workset_service::build_managed_window(window, rect, show_state, i32::try_from(z_order).unwrap_or(i32::MAX), &state.monitors, &c.borrow().main_monitor_ids) {
                Ok(managed_window) => managed_windows.push(managed_window),
                Err(err) => tracing::warn!(error = %err, hwnd = window.hwnd, "registration: skipping window not on a main monitor"),
            }
        }

        if managed_windows.is_empty() {
            manager.set_status_text("登録できるウィンドウがありませんでした。".into());
            manager.set_status_is_warning(true);
            return;
        }

        let color_hex = color_to_hex(state.selected_color);
        let save_result = {
            let mut config = c.borrow_mut();
            let sort_order = i32::try_from(config.worksets.len()).unwrap_or(i32::MAX);
            let workset = workset_service::build_workset(name, color_hex, repository_path, repository_kind, sort_order, managed_windows);
            config.worksets.push(workset);

            let errors = config.validate();
            if !errors.is_empty() {
                config.worksets.pop();
                Err(errors.iter().map(std::string::ToString::to_string).collect::<Vec<_>>().join(" / "))
            } else {
                match config_store::save(&dir, &config) {
                    Ok(()) => Ok(()),
                    Err(err) => {
                        config.worksets.pop();
                        Err(err.to_string())
                    }
                }
            }
        };

        match save_result {
            Ok(()) => {
                state.registering = false;
                manager.set_registering(false);
                manager.set_status_text("ワークセットを登録しました。".into());
                manager.set_status_is_warning(false);
                refresh_workset_summaries(&manager, &c.borrow(), &mut state);
            }
            Err(message) => {
                manager.set_status_text(format!("登録できません: {message}").into());
                manager.set_status_is_warning(true);
            }
        }
    });

    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    let dir = data_dir;
    let pid = std::process::id();
    manager.on_rebind_requested(move |window_index| {
        let Some(manager) = m.upgrade() else { return };
        let state = s.borrow();
        let Some(ws_index) = state.selected_workset_index else {
            return;
        };
        let Ok(window_index) = usize::try_from(window_index) else {
            return;
        };

        let managed_window_id_and_matcher = {
            let config = c.borrow();
            config
                .worksets
                .get(ws_index)
                .and_then(|w| w.windows.get(window_index))
                .map(|w| (w.id, w.matcher.clone(), w.z_order))
        };
        let Some((managed_window_id, window_matcher, z_order)) = managed_window_id_and_matcher
        else {
            return;
        };

        let live_windows = match enumerate::enumerate_top_level_windows(pid) {
            Ok(windows) => windows,
            Err(err) => {
                manager
                    .set_status_text(format!("ウィンドウ一覧の取得に失敗しました: {err}").into());
                manager.set_status_is_warning(true);
                return;
            }
        };

        let bound_elsewhere: std::collections::HashSet<isize> = {
            let config = c.borrow();
            let decisions = workset_service::resolve_all_matches(&config.worksets, &live_windows);
            decisions
                .iter()
                .filter(|&(id, _)| *id != managed_window_id)
                .filter_map(|(_, decision)| {
                    if let MatchDecision::AutoRebind { hwnd } = decision {
                        Some(*hwnd)
                    } else {
                        None
                    }
                })
                .collect()
        };

        let decision = crate::windowing::matcher::resolve_best_match(
            &window_matcher,
            &live_windows,
            &bound_elsewhere,
        );
        let target_hwnd = match decision {
            MatchDecision::AutoRebind { hwnd } => Some(hwnd),
            MatchDecision::Ambiguous { candidates } => candidates.into_iter().next(),
            MatchDecision::Unresolved => live_windows
                .iter()
                .filter(|w| !bound_elsewhere.contains(&w.hwnd))
                .max_by_key(|w| crate::windowing::matcher::score_candidate(&window_matcher, w))
                .map(|w| w.hwnd),
        };

        let Some(target_hwnd) = target_hwnd else {
            manager.set_status_text("再登録できる候補が見つかりませんでした。".into());
            manager.set_status_is_warning(true);
            return;
        };
        let Some(target_window) = live_windows.iter().find(|w| w.hwnd == target_hwnd) else {
            return;
        };

        let hwnd = HWND(target_hwnd as *mut _);
        let show_state = match win_placement::get_show_state(hwnd) {
            Ok(s) => s,
            Err(err) => {
                manager.set_status_text(format!("状態の取得に失敗しました: {err}").into());
                manager.set_status_is_warning(true);
                return;
            }
        };
        let rect = match win_placement::get_normal_rect(hwnd) {
            Ok(r) => r,
            Err(err) => {
                manager.set_status_text(format!("配置の取得に失敗しました: {err}").into());
                manager.set_status_is_warning(true);
                return;
            }
        };

        let mut state = s.borrow_mut();
        let rebuilt = workset_service::build_managed_window(
            target_window,
            rect,
            show_state,
            z_order,
            &state.monitors,
            &c.borrow().main_monitor_ids,
        );
        match rebuilt {
            Ok(new_managed_window) => {
                let mut config = c.borrow_mut();
                if let Some(workset) = config.worksets.get_mut(ws_index)
                    && let Some(slot) = workset.windows.get_mut(window_index)
                {
                    *slot = ManagedWindow {
                        id: managed_window_id,
                        ..new_managed_window
                    };
                    workset.updated_at = clock::now_rfc3339();
                }
                drop(config);

                if let Err(err) = config_store::save(&dir, &c.borrow()) {
                    manager.set_status_text(format!("保存に失敗しました: {err}").into());
                    manager.set_status_is_warning(true);
                    return;
                }

                manager.set_status_text("再登録しました。".into());
                manager.set_status_is_warning(false);
                refresh_workset_summaries(&manager, &c.borrow(), &mut state);
                refresh_selected_workset_detail(&manager, &c.borrow(), &state);
            }
            Err(err) => {
                manager.set_status_text(format!("再登録できません: {err}").into());
                manager.set_status_is_warning(true);
            }
        }
    });
}

/// Rebuilds the Quick Switcher's row list from `config.worksets` (PLAN.md
/// §3.3 "表示内容"), mirroring `refresh_workset_summaries`'s shape, plus each
/// row's Codex agent-status badge (PLAN.md §6, Phase 8). When `sort_mode`
/// isn't `Manual`, status priority becomes the primary sort key (highest
/// first), with the existing `SortMode` comparator only breaking ties within
/// a status group — `Manual` bypasses status grouping entirely (badges still
/// show, order never changes).
fn refresh_quick_switcher_rows(switcher: &QuickSwitcher, config: &AppConfig, data_dir: &Path) {
    let state = runtime_store::load(data_dir);
    let current_workset_id = state.current_workset_id;
    let filter_text = switcher.get_filter_text().to_string();
    let mut ordered = quick_switcher_service::sorted_and_filtered(
        &config.worksets,
        config.settings.sort_mode,
        &filter_text,
    );

    let aggregates = agent_status_service::aggregate_all(&config.worksets, &state.agent_runs);
    if config.settings.sort_mode != SortMode::Manual {
        ordered.sort_by_key(|w| {
            std::cmp::Reverse(row_display_priority(
                aggregates
                    .get(&w.id)
                    .copied()
                    .unwrap_or(AgentState::Unknown),
            ))
        });
    }

    let rows: Vec<QuickSwitcherRow> = ordered
        .iter()
        .enumerate()
        .map(|(i, workset)| {
            let agent_state = aggregates
                .get(&workset.id)
                .copied()
                .unwrap_or(AgentState::Unknown);
            let (agent_status_label, agent_elapsed_text) =
                agent_row_status(&state.agent_runs, workset.id, agent_state);
            QuickSwitcherRow {
                workset_id: workset.id.to_string().into(),
                name: workset.name.clone().into(),
                repository_name: workset
                    .repository_path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| workset.repository_path.display().to_string())
                    .into(),
                color: hex_to_color(&workset.color),
                is_current: Some(workset.id) == current_workset_id,
                is_parking_target: matches!(workset.parking_policy, ParkingPolicy::Fixed { .. }),
                number_hint: if i < 9 {
                    i32::try_from(i + 1).unwrap_or(0)
                } else {
                    0
                },
                agent_status_color: hex_to_color(state_color(agent_state)),
                agent_status_label: agent_status_label.into(),
                agent_elapsed_text: agent_elapsed_text.into(),
            }
        })
        .collect();

    let row_count = i32::try_from(rows.len()).unwrap_or(i32::MAX);
    switcher.set_rows(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    if row_count == 0 {
        switcher.set_selected_index(0);
    } else if switcher.get_selected_index() >= row_count {
        switcher.set_selected_index(row_count - 1);
    }
}

/// Japanese label for an aggregate agent state, shared by the Quick
/// Switcher's row badge and the tray tooltip. Empty for `Idle`/`Unknown` —
/// the common case shouldn't carry visual noise.
fn agent_status_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Running => "実行中",
        AgentState::NeedsInput => "入力待ち",
        AgentState::Ready => "完了",
        AgentState::Blocked => "エラー",
        AgentState::Idle | AgentState::Unknown => "",
    }
}

/// Finds the run(s) that actually drive a workset's aggregate `state`
/// (matching `domain::agent::aggregate_state`'s own per-state criteria — in
/// particular, `Ready` only counts unconfirmed runs) and formats how long
/// ago the most recent one transitioned. Empty for `Idle`/`Unknown`, where
/// there's no single "since when" moment worth surfacing.
fn agent_row_status(
    runs: &[AgentRun],
    workset_id: uuid::Uuid,
    state: AgentState,
) -> (&'static str, String) {
    if matches!(state, AgentState::Idle | AgentState::Unknown) {
        return (agent_status_label(state), String::new());
    }

    let driving_run = runs
        .iter()
        .filter(|r| {
            r.workset_id == workset_id
                && match state {
                    AgentState::Ready => r.state == AgentState::Ready && !r.confirmed,
                    other => r.state == other,
                }
        })
        .max_by(|a, b| a.last_transition_at.cmp(&b.last_transition_at));

    let elapsed = driving_run
        .map(|r| format_elapsed(&r.last_transition_at))
        .unwrap_or_default();
    (agent_status_label(state), elapsed)
}

/// "3分" / "1分未満" style elapsed-time text from an RFC 3339 timestamp,
/// computed fresh at display time rather than stored as a duration (PLAN.md
/// §3.3's "経過時間" column).
fn format_elapsed(timestamp: &str) -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    let Ok(parsed) = OffsetDateTime::parse(timestamp, &Rfc3339) else {
        return String::new();
    };
    let minutes = (OffsetDateTime::now_utc() - parsed).whole_minutes();
    if minutes < 1 {
        "1分未満".to_string()
    } else {
        format!("{minutes}分")
    }
}

/// The workset with the highest `tray_priority` aggregate state among those
/// that aren't `Idle`/`Unknown` (PLAN.md §6.7's tray-color priority),
/// shared by the tray-icon refresh and the tray-click row-selection logic.
fn highest_priority_agent_workset(
    config: &AppConfig,
    agent_runs: &[AgentRun],
) -> Option<(uuid::Uuid, AgentState)> {
    let aggregates = agent_status_service::aggregate_all(&config.worksets, agent_runs);
    config
        .worksets
        .iter()
        .filter_map(|w| aggregates.get(&w.id).map(|s| (w.id, *s)))
        .filter(|(_, s)| !matches!(s, AgentState::Idle | AgentState::Unknown))
        .max_by_key(|(_, s)| tray_priority(*s))
}

/// Selects the row matching `workset_id`, if one is currently displayed
/// (PLAN.md §6.7: a tray left-click that opens the Quick Switcher selects
/// the highest-priority workset's row).
fn select_row_for_workset(switcher: &QuickSwitcher, workset_id: uuid::Uuid) {
    let target = workset_id.to_string();
    let rows = switcher.get_rows();
    for i in 0..rows.row_count() {
        if let Some(row) = rows.row_data(i)
            && row.workset_id == target
        {
            switcher.set_selected_index(i32::try_from(i).unwrap_or(0));
            return;
        }
    }
}

/// Positions and shows the Quick Switcher at the configured popup location
/// (PLAN.md §3.3), refreshing its rows first and then forcing real OS input
/// focus onto it (needed for a `WS_EX_TOOLWINDOW` popup, which doesn't
/// reliably grab keyboard focus on `.show()` alone).
fn show_quick_switcher_at_cursor(switcher: &QuickSwitcher, config: &AppConfig, data_dir: &Path) {
    refresh_quick_switcher_rows(switcher, config, data_dir);

    let live_monitors = monitor::enumerate_monitors().unwrap_or_default();
    let cursor = monitor::cursor_position().unwrap_or((0, 0));
    let size = switcher.window().size();
    let popup_size = (
        i32::try_from(size.width).unwrap_or(480),
        i32::try_from(size.height).unwrap_or(560),
    );
    let (x, y) = popup_placement::resolve_popup_position(
        config.settings.popup_location,
        cursor,
        &live_monitors,
        &config.main_monitor_ids,
        popup_size,
    );
    switcher
        .window()
        .set_position(slint::WindowPosition::Physical(
            slint::PhysicalPosition::new(x, y),
        ));
    let _ = switcher.show();
    // Re-applied after every `.show()`, not just once at window creation:
    // empirically, winit's own show-window path resets `WS_EX_APPWINDOW`
    // back on regardless of what was set beforehand, so the exclusion only
    // sticks if it's the *last* thing touching the style bits.
    popup_window::exclude_from_taskbar_and_alt_tab(switcher.window());
    popup_window::force_foreground(switcher.window());
}

/// PLAN.md §3.3/§3.4: the hotkey and tray-icon left-click both *toggle*
/// visibility, while the tray menu's own "クイックスイッチャーを開く" always
/// shows it fresh — see `TrayIcon`'s doc comment on `clicked` in
/// `ui/app-window.slint`.
fn toggle_quick_switcher(switcher: &QuickSwitcher, config: &AppConfig, data_dir: &Path) {
    if switcher.window().is_visible() {
        let _ = switcher.hide();
    } else {
        show_quick_switcher_at_cursor(switcher, config, data_dir);
    }
}

fn wire_quick_switcher(
    switcher: &QuickSwitcher,
    data_dir: PathBuf,
    config: Rc<RefCell<AppConfig>>,
    coordinator: Rc<SwitchCoordinator<Win32WindowOps>>,
    settings_window: slint::Weak<AppWindow>,
) {
    refresh_quick_switcher_rows(switcher, &config.borrow(), &data_dir);

    let s = switcher.as_weak();
    let c = config.clone();
    let d = data_dir.clone();
    switcher.on_key_text_input(move |text| {
        let Some(switcher) = s.upgrade() else { return };
        let mut filter = switcher.get_filter_text().to_string();
        filter.push_str(&text);
        switcher.set_filter_text(filter.into());
        refresh_quick_switcher_rows(&switcher, &c.borrow(), &d);
    });

    let s = switcher.as_weak();
    let c = config.clone();
    let d = data_dir.clone();
    switcher.on_filter_backspace_requested(move || {
        let Some(switcher) = s.upgrade() else { return };
        let mut filter = switcher.get_filter_text().to_string();
        filter.pop();
        switcher.set_filter_text(filter.into());
        refresh_quick_switcher_rows(&switcher, &c.borrow(), &d);
    });

    let s = switcher.as_weak();
    let c = config.clone();
    let d = data_dir.clone();
    let coord = coordinator.clone();
    switcher.on_switch_requested(move |workset_id| {
        let Some(switcher) = s.upgrade() else { return };
        let Ok(target_workset_id) = uuid::Uuid::parse_str(&workset_id) else {
            return;
        };

        let close_after_switch = {
            let config = c.borrow();
            let live_monitors = monitor::enumerate_monitors().unwrap_or_default();
            let live_windows =
                enumerate::enumerate_top_level_windows(std::process::id()).unwrap_or_default();

            match coord.switch_to(SwitchRequest {
                worksets: &config.worksets,
                fixed_slots: &config.fixed_slots,
                saved_monitors: &config.monitors,
                main_monitor_ids: &config.main_monitor_ids,
                live_monitors: &live_monitors,
                live_windows: &live_windows,
                target_workset_id,
            }) {
                Ok(_) => {
                    switcher.set_status_text("".into());
                    switcher.set_status_is_warning(false);
                    // PLAN.md §3.3 "選択後" / ready確認処理: switching to a
                    // workset counts as the user having seen its completed
                    // agent runs.
                    if let Err(err) =
                        agent_status_service::confirm_ready_and_recompute(&d, target_workset_id)
                    {
                        tracing::warn!(error = %err, "failed to confirm ready agent runs after switching");
                    }
                    UI_CONTEXT.with(|cell| {
                        if let Some(ctx) = &*cell.borrow() {
                            refresh_tray_status(ctx);
                        }
                    });
                    config.settings.close_after_switch
                }
                Err(err) => {
                    switcher.set_status_text(format!("切替に失敗しました: {err}").into());
                    switcher.set_status_is_warning(true);
                    false
                }
            }
        };

        refresh_quick_switcher_rows(&switcher, &c.borrow(), &d);
        if close_after_switch {
            let _ = switcher.hide();
        }
    });

    let s = switcher.as_weak();
    let c = config.clone();
    let coord = coordinator.clone();
    switcher.on_recover_requested(move || {
        let Some(switcher) = s.upgrade() else { return };
        let config = c.borrow();
        let live_monitors = monitor::enumerate_monitors().unwrap_or_default();
        let live_windows =
            enumerate::enumerate_top_level_windows(std::process::id()).unwrap_or_default();
        match coord.recover_all_windows(
            &config.worksets,
            &live_monitors,
            &config.main_monitor_ids,
            &live_windows,
        ) {
            Ok(report) => {
                switcher.set_status_text(
                    format!(
                        "{}件のウィンドウをメイン画面へ回収しました。",
                        report.recovered.len()
                    )
                    .into(),
                );
                switcher.set_status_is_warning(false);
            }
            Err(err) => {
                switcher.set_status_text(format!("回収に失敗しました: {err}").into());
                switcher.set_status_is_warning(true);
            }
        }
    });

    let s = switcher.as_weak();
    let settings_for_open = settings_window;
    let c = config.clone();
    switcher.on_settings_requested(move || {
        if let Some(switcher) = s.upgrade() {
            let _ = switcher.hide();
        }
        if let Some(settings) = settings_for_open.upgrade() {
            refresh_codex_settings_state(&settings, &c.borrow());
            let _ = settings.show();
        }
    });

    let s = switcher.as_weak();
    switcher.on_close_requested(move || {
        if let Some(switcher) = s.upgrade() {
            let _ = switcher.hide();
        }
    });
}

/// Populates the hotkey rebind form and wires its save handler, following
/// the same mutate → `validate()` → `config_store::save()` →
/// rollback-on-failure template used by every other config-saving handler in
/// this file (PLAN.md §13 Phase 7 checklist item 4, "ホットキー設定UI").
fn wire_settings(
    window: &AppWindow,
    data_dir: PathBuf,
    config: Rc<RefCell<AppConfig>>,
    hotkey_thread: Rc<HotkeyThread>,
) {
    let key_labels: Vec<String> = ('A'..='Z')
        .map(|c| c.to_string())
        .chain((0..=9).map(|n| n.to_string()))
        .chain((1..=12).map(|n| format!("F{n}")))
        .collect();
    let key_choices: Vec<slint::SharedString> =
        key_labels.iter().cloned().map(Into::into).collect();
    window.set_hotkey_key_choices(std::rc::Rc::new(slint::VecModel::from(key_choices)).into());

    {
        let cfg = config.borrow();
        let hotkey = &cfg.settings.quick_switcher_hotkey;
        window.set_hotkey_ctrl(hotkey.modifiers.contains(&HotkeyModifier::Control));
        window.set_hotkey_alt(hotkey.modifiers.contains(&HotkeyModifier::Alt));
        window.set_hotkey_shift(hotkey.modifiers.contains(&HotkeyModifier::Shift));
        window.set_hotkey_win(hotkey.modifiers.contains(&HotkeyModifier::Win));
        if let Some(label) = win32_hotkey::virtual_key_to_label(hotkey.virtual_key) {
            if let Some(index) = key_labels.iter().position(|l| l == &label) {
                window.set_hotkey_key_index(i32::try_from(index).unwrap_or(0));
            }
            window.set_hotkey_key_choice(label.into());
        }
    }

    let w = window.as_weak();
    let c = config.clone();
    let d = data_dir.clone();
    let ht = hotkey_thread;
    window.on_hotkey_rebind_requested(move || {
        let Some(window) = w.upgrade() else { return };

        let mut modifiers = Vec::new();
        if window.get_hotkey_ctrl() {
            modifiers.push(HotkeyModifier::Control);
        }
        if window.get_hotkey_alt() {
            modifiers.push(HotkeyModifier::Alt);
        }
        if window.get_hotkey_shift() {
            modifiers.push(HotkeyModifier::Shift);
        }
        if window.get_hotkey_win() {
            modifiers.push(HotkeyModifier::Win);
        }

        if modifiers.is_empty() {
            window.set_hotkey_status_text("修飾キーを1つ以上選択してください。".into());
            window.set_hotkey_status_is_warning(true);
            return;
        }
        let Some(virtual_key) =
            win32_hotkey::key_label_to_virtual_key(&window.get_hotkey_key_choice())
        else {
            window.set_hotkey_status_text("キーを選択してください。".into());
            window.set_hotkey_status_is_warning(true);
            return;
        };

        let candidate = HotkeyConfig {
            modifiers,
            virtual_key,
        };
        let mut cfg = c.borrow_mut();
        let previous = cfg.settings.quick_switcher_hotkey.clone();
        if previous == candidate {
            drop(cfg);
            window.set_hotkey_status_text("変更はありません。".into());
            window.set_hotkey_status_is_warning(false);
            return;
        }
        cfg.settings.quick_switcher_hotkey = candidate.clone();

        let errors = cfg.validate();
        if !errors.is_empty() {
            cfg.settings.quick_switcher_hotkey = previous;
            let message = errors
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(" / ");
            window.set_hotkey_status_text(format!("保存できません: {message}").into());
            window.set_hotkey_status_is_warning(true);
            return;
        }

        match config_store::save(&d, &cfg) {
            Ok(()) => {
                UI_CONTEXT.with(|cell| {
                    if let Some(ctx) = &*cell.borrow() {
                        *ctx.pending_hotkey_rollback.borrow_mut() = Some(previous);
                    }
                });
                window.set_hotkey_status_text("保存しました。反映を確認しています…".into());
                window.set_hotkey_status_is_warning(false);
                ht.rebind(candidate);
            }
            Err(err) => {
                cfg.settings.quick_switcher_hotkey = previous;
                window.set_hotkey_status_text(format!("設定の保存に失敗しました: {err}").into());
                window.set_hotkey_status_is_warning(true);
            }
        }
    });
}

/// Wires the settings window's "Codex連携" section (PLAN.md §6.5). Unlike
/// `wire_settings`, nothing here is persisted to `AppConfig` — the hook
/// path, snippet, and test-event flow are all derived fresh each time from
/// the filesystem/process environment, not stored settings.
fn wire_codex_settings(window: &AppWindow, config: Rc<RefCell<AppConfig>>) {
    refresh_codex_settings_state(window, &config.borrow());

    let w = window.as_weak();
    window.on_codex_check_hook_exe_requested(move || {
        if let Some(window) = w.upgrade() {
            check_hook_exe(&window);
        }
    });

    let w = window.as_weak();
    window.on_codex_copy_snippet_requested(move || {
        let Some(window) = w.upgrade() else { return };
        let Some(hook_path) = hook_exe_path() else {
            window.set_codex_status_text(
                "repodeck-hook.exeが見つかりません。インストール先を確認してください。".into(),
            );
            window.set_codex_status_is_warning(true);
            return;
        };

        let snippet = build_hooks_json_snippet(&hook_path);
        window.set_codex_hook_snippet(snippet.clone().into());
        match copy_text_to_clipboard(&snippet) {
            Ok(()) => {
                window.set_codex_status_text("クリップボードへコピーしました。".into());
                window.set_codex_status_is_warning(false);
            }
            Err(err) => {
                window.set_codex_status_text(format!("コピーに失敗しました: {err}").into());
                window.set_codex_status_is_warning(true);
            }
        }
    });

    window.on_codex_open_config_folder_requested(move || {
        let folder = codex_config_folder();
        if let Err(err) = std::process::Command::new("explorer.exe")
            .arg(&folder)
            .spawn()
        {
            tracing::warn!(error = %err, path = %folder.display(), "failed to open the Codex config folder");
        }
    });

    let w = window.as_weak();
    let c = config;
    window.on_codex_send_test_event_requested(move || {
        let Some(window) = w.upgrade() else { return };
        let Some(repository_path) = c
            .borrow()
            .worksets
            .first()
            .map(|w| w.repository_path.clone())
        else {
            window.set_codex_status_text("先にセットを登録してください。".into());
            window.set_codex_status_is_warning(true);
            return;
        };
        let Some(hook_path) = hook_exe_path() else {
            window.set_codex_status_text("repodeck-hook.exeが見つかりません。".into());
            window.set_codex_status_is_warning(true);
            return;
        };

        window.set_codex_status_text(
            "テストイベントを送信中… クイックスイッチャーのバッジを確認してください。".into(),
        );
        window.set_codex_status_is_warning(false);
        // Runs entirely on a background thread: the real feedback is the
        // Quick Switcher badge / tray icon transitions that arrive back
        // through the actual named pipe, exactly like a real Codex hook —
        // this thread never touches UI state directly.
        std::thread::spawn(move || {
            if let Err(err) = send_test_event(&hook_path, &repository_path) {
                tracing::warn!(error = %err, "failed to send the Codex test event sequence");
            }
        });
    });
}

fn refresh_codex_settings_state(window: &AppWindow, config: &AppConfig) {
    check_hook_exe(window);
    window.set_codex_send_test_event_enabled(!config.worksets.is_empty());
}

fn check_hook_exe(window: &AppWindow) {
    window.set_codex_hook_exe_found(hook_exe_path().is_some());
}

/// `repodeck-hook.exe` is expected to sit next to `repodeck.exe` (same
/// install directory) — PLAN.md doesn't specify an installer, so this is
/// the only location that's true in both a dev build (`target/debug/`) and
/// a plain xcopy-style install.
fn hook_exe_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let hook_path = exe.parent()?.join("repodeck-hook.exe");
    hook_path.exists().then_some(hook_path)
}

/// Builds the `hooks.json` fragment for all 4 Codex hooks (PLAN.md §6.5),
/// via `serde_json::json!`/`to_string_pretty` rather than string templating
/// so every one of §6.5's validation conditions holds by construction: one
/// group per hook, one `command`-type entry per group, a 2-second timeout,
/// and `commandWindows` wrapping the absolute path in literal quotes (the
/// shell-level quoting a path containing spaces needs, distinct from the
/// JSON string's own quoting).
fn build_hooks_json_snippet(hook_path: &Path) -> String {
    let command = format!("\"{}\"", hook_path.display());
    let hook_group = || {
        serde_json::json!({
            "hooks": [{
                "type": "command",
                "commandWindows": command,
                "timeout": 2,
            }]
        })
    };

    let value = serde_json::json!({
        "hooks": {
            "UserPromptSubmit": [hook_group()],
            "PermissionRequest": [hook_group()],
            "PostToolUse": [hook_group()],
            "Stop": [hook_group()],
        }
    });

    serde_json::to_string_pretty(&value).unwrap_or_default()
}

/// `%USERPROFILE%\.codex`, falling back to bare `%USERPROFILE%` if Codex
/// hasn't created its config folder yet.
fn codex_config_folder() -> PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let codex_dir = base.join(".codex");
    if codex_dir.exists() { codex_dir } else { base }
}

/// Fires the real `repodeck-hook.exe` through its full 4-hook lifecycle
/// sequence (`UserPromptSubmit` → `PermissionRequest` → `PostToolUse` →
/// `Stop`) against `repository_path`, with a short pause between each so the
/// Quick Switcher badge/tray icon visibly transition through
/// running→needs_input→running→ready (Phase 8 design decision 2) — this
/// exercises the full pipe→ACL→adapter→aggregation→UI path, not an
/// in-process shortcut.
fn send_test_event(hook_path: &Path, repository_path: &Path) -> Result<(), String> {
    let session_id = format!("repodeck-test-{}", uuid::Uuid::new_v4());
    for hook_event_name in [
        "UserPromptSubmit",
        "PermissionRequest",
        "PostToolUse",
        "Stop",
    ] {
        send_one_test_hook_event(hook_path, repository_path, &session_id, hook_event_name)?;
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
    Ok(())
}

fn send_one_test_hook_event(
    hook_path: &Path,
    repository_path: &Path,
    session_id: &str,
    hook_event_name: &str,
) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let payload = serde_json::json!({
        "session_id": session_id,
        "turn_id": "repodeck-test-turn",
        "cwd": repository_path.to_string_lossy(),
        "hook_event_name": hook_event_name,
        "model": "repodeck-test",
    })
    .to_string();

    let mut child = Command::new(hook_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;

    child
        .stdin
        .take()
        .ok_or_else(|| "failed to open the hook process's stdin".to_string())?
        .write_all(payload.as_bytes())
        .map_err(|e| e.to_string())?;

    child.wait().map_err(|e| e.to_string())?;
    Ok(())
}

/// Copies `text` to the clipboard as `CF_UNICODETEXT` (hardcoded as `13`
/// rather than pulling in `Win32_System_Ole` for the constant). Ownership of
/// the `GlobalAlloc`'d memory transfers to the clipboard on a *successful*
/// `SetClipboardData` — it must not be freed in that case, only on failure,
/// or the clipboard is left holding a dangling handle.
fn copy_text_to_clipboard(text: &str) -> windows::core::Result<()> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

    const CF_UNICODETEXT: u32 = 13;

    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let byte_len = wide.len() * std::mem::size_of::<u16>();

    // SAFETY: `hwndnewowner: None` associates the clipboard with the current
    // task, sufficient for a one-shot copy.
    unsafe { OpenClipboard(None) }?;

    let result: windows::core::Result<()> = (|| {
        // SAFETY: the clipboard is open (just above); this discards whatever
        // was previously on it, which is the point of "copy".
        unsafe { EmptyClipboard() }?;

        // SAFETY: `byte_len` is nonzero (at least the NUL terminator).
        let hglobal: HGLOBAL = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) }?;

        // SAFETY: `hglobal` was just allocated above with `byte_len` bytes.
        let ptr = unsafe { GlobalLock(hglobal) };
        if ptr.is_null() {
            // SAFETY: `hglobal` is still owned by this function; the lock
            // above failed, so the clipboard never took ownership of it.
            unsafe {
                let _ = GlobalFree(Some(hglobal));
            }
            return Err(windows::core::Error::from_thread());
        }
        // SAFETY: `ptr` is a writable buffer of `byte_len` bytes for as long
        // as the lock above holds; unlocked immediately below.
        unsafe {
            std::ptr::copy_nonoverlapping(wide.as_ptr().cast::<u8>(), ptr.cast::<u8>(), byte_len);
        }
        // SAFETY: unlocks the same handle locked above; a "failure" here
        // (lock count reaching zero) is the documented normal case, not a
        // real error, so the result is discarded.
        let _ = unsafe { GlobalUnlock(hglobal) };

        // SAFETY: `hglobal` is a valid `GMEM_MOVEABLE` handle. On success the
        // clipboard now owns it and must not be freed here (see this
        // function's doc comment); on failure it's still ours to free.
        match unsafe { SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hglobal.0))) } {
            Ok(_) => Ok(()),
            Err(err) => {
                unsafe {
                    let _ = GlobalFree(Some(hglobal));
                }
                Err(err)
            }
        }
    })();

    // SAFETY: closes the clipboard opened above, regardless of the inner
    // result.
    unsafe {
        let _ = CloseClipboard();
    }

    result
}

/// Wires the settings window's "バージョン情報"/"起動設定" sections (Phase 9).
/// The registry (`windowing::autostart`) is the source of truth for whether
/// autostart is actually enabled — `AppConfig.settings.start_with_windows` is
/// only kept in sync as a secondary record, updated on every successful
/// toggle here, never read to decide the checkbox's initial state.
fn wire_about_and_autostart(window: &AppWindow, config: Rc<RefCell<AppConfig>>, data_dir: PathBuf) {
    window.set_app_version(env!("CARGO_PKG_VERSION").into());
    window.set_start_with_windows(autostart::is_enabled().unwrap_or(false));

    window.on_open_third_party_notices_requested(|| {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let Some(dir) = exe.parent() else { return };
        let path = dir.join("THIRD_PARTY_NOTICES.md");
        if !path.exists() {
            tracing::warn!(path = %path.display(), "THIRD_PARTY_NOTICES.md not found next to the executable");
            return;
        }
        if let Err(err) = std::process::Command::new("explorer.exe").arg(&path).spawn() {
            tracing::warn!(error = %err, path = %path.display(), "failed to open THIRD_PARTY_NOTICES.md");
        }
    });

    let w = window.as_weak();
    let c = config;
    window.on_start_with_windows_toggled(move |enabled| {
        let Some(window) = w.upgrade() else { return };
        if let Err(err) = autostart::set_enabled(enabled) {
            window.set_start_with_windows(!enabled); // revert the checkbox
            window.set_autostart_status_text(format!("自動起動の設定に失敗しました: {err}").into());
            window.set_autostart_status_is_warning(true);
            return;
        }

        let mut cfg = c.borrow_mut();
        cfg.settings.start_with_windows = enabled;
        match config_store::save(&data_dir, &cfg) {
            Ok(()) => {
                window.set_autostart_status_text(
                    if enabled {
                        "Windows起動時の自動起動を有効にしました。"
                    } else {
                        "自動起動を無効にしました。"
                    }
                    .into(),
                );
                window.set_autostart_status_is_warning(false);
            }
            Err(err) => {
                window.set_autostart_status_text(format!("設定の保存に失敗しました: {err}").into());
                window.set_autostart_status_is_warning(true);
            }
        }
    });
}

/// Native `MessageBoxW` confirmation for the tray's "全管理ウィンドウを回収" —
/// a single click would otherwise move every window across every workset at
/// once with no chance to back out.
fn confirm_recover_all() -> bool {
    let text = HSTRING::from("登録されている全ウィンドウをメイン画面へ回収しますか?");
    let caption = HSTRING::from("RepoDeck");
    // SAFETY: `text`/`caption` are valid, NUL-terminated wide strings for the
    // duration of this call; `hwnd: None` shows an owner-less dialog.
    let result = unsafe { MessageBoxW(None, &text, &caption, MB_YESNO | MB_ICONWARNING) };
    result == IDYES
}

/// PLAN.md §10.3 steps 3-4: a leftover `switch-journal.json` at startup means
/// the previous switch never finished. A single `MB_YESNOCANCEL` dialog maps
/// onto the spec's three named choices — Yes/No/Cancel button labels are
/// fixed by Win32, so the body text spells out what each one does.
fn crash_recovery_dialog(affected_window_count: usize) -> JournalRecoveryChoice {
    let text = HSTRING::from(format!(
        "前回の切替が中断されました({affected_window_count}件のウィンドウに影響)。\n\n\
         [はい] 元の配置に戻す\n\
         [いいえ] 全ウィンドウをメイン画面へ回収\n\
         [キャンセル] 何もしない(次回起動時に再度確認します)"
    ));
    let caption = HSTRING::from("RepoDeck — 切替の復旧");
    // SAFETY: `text`/`caption` are valid, NUL-terminated wide strings for the
    // duration of this call; `hwnd: None` shows an owner-less dialog.
    let result = unsafe { MessageBoxW(None, &text, &caption, MB_YESNOCANCEL | MB_ICONWARNING) };
    match result {
        IDYES => JournalRecoveryChoice::RestoreOriginalPlacement,
        IDNO => JournalRecoveryChoice::RecoverAllToMain,
        _ => JournalRecoveryChoice::DoNothing,
    }
}

fn hotkey_register_error_message(err: HotkeyRegisterError) -> String {
    match err {
        HotkeyRegisterError::AlreadyRegistered => {
            "このホットキーは他のアプリと競合しています。元のホットキーに戻しました。".to_string()
        }
        HotkeyRegisterError::Other(e) => format!("ホットキーの登録に失敗しました: {e}"),
    }
}

/// Send-safe projection of `hotkey::win32_hotkey::HotkeyEvent`, built on the
/// hotkey thread and handled on the UI thread via
/// `slint::invoke_from_event_loop`. Its `Rc`-based state (`AppConfig`, the
/// pending hotkey rollback) can't cross threads directly — `Rc` isn't
/// `Send` — so it lives in `UI_CONTEXT`, a UI-thread-local populated once in
/// `run()` and only ever read back from a closure Slint guarantees runs on
/// that same thread.
enum HotkeyUiEvent {
    Pressed,
    Registered,
    RegisterFailed(String),
}

struct CrossThreadUiContext {
    config: Rc<RefCell<AppConfig>>,
    data_dir: PathBuf,
    pending_hotkey_rollback: RefCell<Option<HotkeyConfig>>,
    settings_window: slint::Weak<AppWindow>,
    quick_switcher: slint::Weak<QuickSwitcher>,
    tray: slint::Weak<TrayIcon>,
    tray_icons: TrayIcons,
    /// Codex events whose `cwd` didn't match any registered workset (PLAN.md
    /// §6.6). In-memory only, never persisted.
    unmatched_agent_events: RefCell<Vec<agent_status_service::UnmatchedAgentEvent>>,
}

thread_local! {
    static UI_CONTEXT: RefCell<Option<Rc<CrossThreadUiContext>>> = const { RefCell::new(None) };
}

fn handle_hotkey_ui_event(event: HotkeyUiEvent) {
    let Some(ctx) = UI_CONTEXT.with(|cell| cell.borrow().clone()) else {
        return;
    };
    match event {
        HotkeyUiEvent::Pressed => {
            if let Some(switcher) = ctx.quick_switcher.upgrade() {
                toggle_quick_switcher(&switcher, &ctx.config.borrow(), &ctx.data_dir);
            }
        }
        HotkeyUiEvent::Registered => {
            ctx.pending_hotkey_rollback.borrow_mut().take();
            if let Some(settings) = ctx.settings_window.upgrade() {
                settings.set_hotkey_status_text("ホットキーを保存しました。".into());
                settings.set_hotkey_status_is_warning(false);
            }
        }
        HotkeyUiEvent::RegisterFailed(message) => {
            if let Some(previous) = ctx.pending_hotkey_rollback.borrow_mut().take() {
                let mut cfg = ctx.config.borrow_mut();
                cfg.settings.quick_switcher_hotkey = previous;
                let _ = config_store::save(&ctx.data_dir, &cfg);
            }
            if let Some(settings) = ctx.settings_window.upgrade() {
                settings.set_hotkey_status_text(message.into());
                settings.set_hotkey_status_is_warning(true);
            }
        }
    }
}

/// Send-safe projection of `ipc::named_pipe::PipeServerEvent::MessageReceived`,
/// built on the pipe server's accept thread and handled on the UI thread via
/// `slint::invoke_from_event_loop` — same shape as `HotkeyUiEvent`. Carries
/// only the raw bytes (trivially `Send`); parsing and all `Rc`-based state
/// access happen after the marshal, in `handle_agent_ui_event`.
enum AgentUiEvent {
    MessageReceived(Vec<u8>),
}

fn handle_agent_ui_event(event: AgentUiEvent) {
    let Some(ctx) = UI_CONTEXT.with(|cell| cell.borrow().clone()) else {
        return;
    };
    let AgentUiEvent::MessageReceived(bytes) = event;

    // `repodeck-hook.exe` already ran `ipc::protocol::parse_and_adapt` on the
    // raw Codex hook JSON before sending it over the pipe (PLAN.md §6.4's
    // "転送JSON" *is* the normalized wire schema) — deserializing straight to
    // `NormalizedEvent` here, not re-adapting, is what the wire format is.
    let normalized: protocol::NormalizedEvent = match serde_json::from_slice(&bytes) {
        Ok(normalized) => normalized,
        Err(err) => {
            tracing::warn!(error = %err, "dropping malformed Codex agent event");
            return;
        }
    };

    {
        let config = ctx.config.borrow();
        let mut unmatched = ctx.unmatched_agent_events.borrow_mut();
        if let Err(err) = agent_status_service::apply_event(
            &ctx.data_dir,
            &config.worksets,
            &mut unmatched,
            normalized,
        ) {
            tracing::warn!(error = %err, "failed to persist Codex agent status update");
        }
    }

    refresh_tray_status(&ctx);
    if let Some(switcher) = ctx.quick_switcher.upgrade() {
        refresh_quick_switcher_rows(&switcher, &ctx.config.borrow(), &ctx.data_dir);
    }
}

/// Recomputes the tray icon color/tooltip from every registered workset's
/// aggregate agent state (PLAN.md §6.7): the highest-`tray_priority` active
/// workset wins, or the idle icon/plain tooltip if none are active.
fn refresh_tray_status(ctx: &CrossThreadUiContext) {
    let Some(tray) = ctx.tray.upgrade() else {
        return;
    };
    let config = ctx.config.borrow();
    let state = runtime_store::load(&ctx.data_dir);

    match highest_priority_agent_workset(&config, &state.agent_runs) {
        Some((workset_id, agent_state)) => {
            let name = config
                .worksets
                .iter()
                .find(|w| w.id == workset_id)
                .map(|w| w.name.as_str())
                .unwrap_or("");
            tray.set_icon_source(ctx.tray_icons.get(agent_state).clone());
            tray.set_tooltip_text(
                format!("RepoDeck — {name}: {}", agent_status_label(agent_state)).into(),
            );
        }
        None => {
            tray.set_icon_source(ctx.tray_icons.get(AgentState::Idle).clone());
            tray.set_tooltip_text("RepoDeck".into());
        }
    }
}

/// The 6 agent-state tray-icon variants (PLAN.md §6.7), rendered once at
/// startup: the bundled app icon with a colored status-dot badge painted in
/// the corner. Rendered in memory via `resvg`/`tiny_skia` (already a
/// dependency for `build.rs`'s own icon generation) and handed to Slint as
/// `Image::from_rgba8_premultiplied` — no asset files or install-path
/// lookups needed at runtime.
struct TrayIcons {
    idle: slint::Image,
    running: slint::Image,
    needs_input: slint::Image,
    ready: slint::Image,
    blocked: slint::Image,
    unknown: slint::Image,
}

impl TrayIcons {
    fn render() -> Self {
        Self {
            idle: render_tray_icon(state_color(AgentState::Idle)),
            running: render_tray_icon(state_color(AgentState::Running)),
            needs_input: render_tray_icon(state_color(AgentState::NeedsInput)),
            ready: render_tray_icon(state_color(AgentState::Ready)),
            blocked: render_tray_icon(state_color(AgentState::Blocked)),
            unknown: render_tray_icon(state_color(AgentState::Unknown)),
        }
    }

    fn get(&self, state: AgentState) -> &slint::Image {
        match state {
            AgentState::Idle => &self.idle,
            AgentState::Running => &self.running,
            AgentState::NeedsInput => &self.needs_input,
            AgentState::Ready => &self.ready,
            AgentState::Blocked => &self.blocked,
            AgentState::Unknown => &self.unknown,
        }
    }
}

const TRAY_ICON_SVG: &[u8] = include_bytes!("../assets/repodeck-icon.svg");
const TRAY_ICON_SIZE: u32 = 32;
const TRAY_BADGE_RADIUS: f32 = 9.0;

fn render_tray_icon(hex_color: &str) -> slint::Image {
    let tree = resvg::usvg::Tree::from_data(TRAY_ICON_SVG, &resvg::usvg::Options::default())
        .expect("bundled repodeck-icon.svg must parse");
    let source_size = tree.size();

    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(TRAY_ICON_SIZE, TRAY_ICON_SIZE).expect("nonzero icon size");
    let scale_x = TRAY_ICON_SIZE as f32 / source_size.width();
    let scale_y = TRAY_ICON_SIZE as f32 / source_size.height();
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale_x, scale_y),
        &mut pixmap.as_mut(),
    );

    draw_status_badge(&mut pixmap, hex_color);

    let mut buffer =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(TRAY_ICON_SIZE, TRAY_ICON_SIZE);
    buffer.make_mut_bytes().copy_from_slice(pixmap.data());
    slint::Image::from_rgba8_premultiplied(buffer)
}

/// Paints a filled circle badge (with a thin dark ring for contrast against
/// light taskbars) in the bottom-right corner, in the state's hex color.
fn draw_status_badge(pixmap: &mut resvg::tiny_skia::Pixmap, hex_color: &str) {
    let cx = TRAY_ICON_SIZE as f32 - TRAY_BADGE_RADIUS - 1.0;
    let cy = TRAY_ICON_SIZE as f32 - TRAY_BADGE_RADIUS - 1.0;

    let mut ring_paint = resvg::tiny_skia::Paint::default();
    ring_paint.set_color(resvg::tiny_skia::Color::from_rgba8(15, 23, 42, 255));
    let ring_path = resvg::tiny_skia::PathBuilder::from_circle(cx, cy, TRAY_BADGE_RADIUS + 1.5)
        .expect("nonzero radius");
    pixmap.fill_path(
        &ring_path,
        &ring_paint,
        resvg::tiny_skia::FillRule::Winding,
        resvg::tiny_skia::Transform::identity(),
        None,
    );

    let mut fill_paint = resvg::tiny_skia::Paint::default();
    fill_paint.set_color(hex_to_tiny_skia_color(hex_color));
    let fill_path = resvg::tiny_skia::PathBuilder::from_circle(cx, cy, TRAY_BADGE_RADIUS)
        .expect("nonzero radius");
    pixmap.fill_path(
        &fill_path,
        &fill_paint,
        resvg::tiny_skia::FillRule::Winding,
        resvg::tiny_skia::Transform::identity(),
        None,
    );
}

fn hex_to_tiny_skia_color(hex: &str) -> resvg::tiny_skia::Color {
    let hex = hex.trim_start_matches('#');
    let r = u8::from_str_radix(hex.get(0..2).unwrap_or("00"), 16).unwrap_or(0);
    let g = u8::from_str_radix(hex.get(2..4).unwrap_or("00"), 16).unwrap_or(0);
    let b = u8::from_str_radix(hex.get(4..6).unwrap_or("00"), 16).unwrap_or(0);
    resvg::tiny_skia::Color::from_rgba8(r, g, b, 255)
}

/// Shows the Quick Switcher in response to a second `repodeck.exe` launch
/// (PLAN.md §3.2), via the same `UI_CONTEXT` thread-local `handle_hotkey_ui_event`
/// uses — the listener thread that calls this (via `invoke_from_event_loop`)
/// is, like the hotkey thread, not the UI thread.
fn show_quick_switcher_from_context() {
    UI_CONTEXT.with(|cell| {
        if let Some(ctx) = &*cell.borrow()
            && let Some(switcher) = ctx.quick_switcher.upgrade()
        {
            show_quick_switcher_at_cursor(&switcher, &ctx.config.borrow(), &ctx.data_dir);
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
    logging::enforce_retention(&logging::log_dir(&data_dir));

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
    let workset_manager_state = Rc::new(RefCell::new(WorksetManagerState::new()));

    // Constructed here (rather than just before `wire_quick_switcher`, as in
    // earlier phases) so the startup crash-recovery pass below can also use
    // it for the "全てメインへ回収" choice.
    let coordinator = Rc::new(SwitchCoordinator::new(Win32WindowOps, data_dir.clone()));

    // PLAN.md §10.3: a leftover `switch-journal.json` means the previous
    // switch never finished (RepoDeck crashed or was killed mid-switch).
    // Runs before any window is shown.
    if let Some(journal) = journal_store::load(&data_dir).unwrap_or(None) {
        let runtime = runtime_store::load(&data_dir);
        if crash_recovery::already_succeeded(&journal, runtime.current_workset_id) {
            // The switch itself completed; only the journal-clear step was
            // interrupted. Nothing to recover, nothing to ask the user.
            let _ = journal_store::clear(&data_dir);
        } else {
            match crash_recovery_dialog(journal.windows.len()) {
                JournalRecoveryChoice::RestoreOriginalPlacement => {
                    let live_windows = enumerate::enumerate_top_level_windows(std::process::id())
                        .unwrap_or_default();
                    let report = crash_recovery::restore_original_placement(
                        &Win32WindowOps,
                        &config.borrow().worksets,
                        &live_windows,
                        &journal,
                    );
                    tracing::info!(
                        recovered = report.recovered.len(),
                        skipped = report.skipped.len(),
                        "restored original placement after an interrupted switch"
                    );
                    let _ = journal_store::clear(&data_dir);
                }
                JournalRecoveryChoice::RecoverAllToMain => {
                    let live_monitors = monitor::enumerate_monitors().unwrap_or_default();
                    let live_windows = enumerate::enumerate_top_level_windows(std::process::id())
                        .unwrap_or_default();
                    let cfg = config.borrow();
                    match coordinator.recover_all_windows(
                        &cfg.worksets,
                        &live_monitors,
                        &cfg.main_monitor_ids,
                        &live_windows,
                    ) {
                        Ok(report) => tracing::info!(
                            recovered = report.recovered.len(),
                            skipped = report.skipped.len(),
                            "recovered all windows after an interrupted switch"
                        ),
                        Err(err) => tracing::warn!(
                            error = %err,
                            "failed to persist runtime state during startup recovery"
                        ),
                    }
                    drop(cfg);
                    let _ = journal_store::clear(&data_dir);
                }
                JournalRecoveryChoice::DoNothing => {
                    // Leave the journal in place; the same dialog reappears
                    // next launch (PLAN.md §10.3 step 5: only a real
                    // selection resolves it).
                }
            }
        }
    }

    // Diagnostic bookkeeping only — the journal's presence, not this flag,
    // is what gates recovery above, so a plain crash outside any switch
    // never triggers a false recovery prompt.
    {
        let mut runtime = runtime_store::load(&data_dir);
        runtime.last_clean_shutdown = false;
        let _ = runtime_store::save(&data_dir, &runtime);
    }

    // `window` is the repurposed settings surface (PLAN.md §13 Phase 7
    // checklist item 4) — see the doc comment on `AppWindow` in
    // `ui/app-window.slint` for why.
    let window = AppWindow::new().context("failed to create the RepoDeck settings window")?;
    apply_glass_backdrop(window.window());

    // Monitor-change recovery (Phase 9): watches the Settings window's
    // WNDPROC because, unlike the Quick Switcher, it exists for the whole
    // process lifetime even while hidden — `WM_DISPLAYCHANGE` needs a
    // persistent window to subclass.
    let config_for_display = config.clone();
    let data_dir_for_display = data_dir.clone();
    let _display_change_watch = popup_window::watch_display_changes(window.window(), move || {
        let live_monitors = monitor::enumerate_monitors().unwrap_or_default();
        let fingerprint = monitor_watch_service::compute_fingerprint(&live_monitors);

        let mut runtime = runtime_store::load(&data_dir_for_display);
        if runtime.last_seen_monitor_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            return; // e.g. a DPI-only change also fires WM_DISPLAYCHANGE
        }

        let live_windows =
            enumerate::enumerate_top_level_windows(std::process::id()).unwrap_or_default();
        let offscreen = {
            let config = config_for_display.borrow();
            monitor_watch_service::find_now_offscreen_windows(
                &config.worksets,
                &live_windows,
                &live_monitors,
            )
        };
        if !offscreen.is_empty() {
            tracing::info!(
                count = offscreen.len(),
                "minimizing windows left offscreen by a monitor configuration change"
            );
        }
        for hwnd in offscreen {
            Win32WindowOps.minimize(hwnd);
        }

        runtime.last_seen_monitor_fingerprint = Some(fingerprint);
        let _ = runtime_store::save(&data_dir_for_display, &runtime);
    });

    let tray = TrayIcon::new().context("failed to create the RepoDeck tray icon")?;
    let layout_studio = LayoutStudio::new().context("failed to create the Layout Studio window")?;
    apply_glass_backdrop(layout_studio.window());
    let workset_manager =
        WorksetManager::new().context("failed to create the Workset Manager window")?;
    apply_glass_backdrop(workset_manager.window());
    let quick_switcher =
        QuickSwitcher::new().context("failed to create the Quick Switcher window")?;
    apply_glass_backdrop(quick_switcher.window());
    // Must run before the window's first `.show()` — Explorer's taskbar
    // often needs a hide/show cycle to notice a style change otherwise.
    popup_window::exclude_from_taskbar_and_alt_tab(quick_switcher.window());

    wire_layout_studio(
        &layout_studio,
        data_dir.clone(),
        config.clone(),
        layout_studio_state,
    );
    wire_workset_manager(
        &workset_manager,
        data_dir.clone(),
        config.clone(),
        workset_manager_state,
    );

    wire_quick_switcher(
        &quick_switcher,
        data_dir.clone(),
        config.clone(),
        coordinator.clone(),
        window.as_weak(),
    );

    // Populates the cross-thread context the hotkey thread, the pipe-server
    // thread, and the second-instance listener thread reach
    // `config`/`quick_switcher`/`tray` through: none of them can capture an
    // `Rc` directly (it isn't `Send`), so each only ever calls a plain-fn/
    // zero-capture closure via `slint::invoke_from_event_loop`, which looks
    // the real state up here — safe because Slint guarantees that closure
    // runs on this same (UI) thread that populated it.
    UI_CONTEXT.with(|cell| {
        *cell.borrow_mut() = Some(Rc::new(CrossThreadUiContext {
            config: config.clone(),
            data_dir: data_dir.clone(),
            pending_hotkey_rollback: RefCell::new(None),
            settings_window: window.as_weak(),
            quick_switcher: quick_switcher.as_weak(),
            tray: tray.as_weak(),
            tray_icons: TrayIcons::render(),
            unmatched_agent_events: RefCell::new(Vec::new()),
        }));
    });

    // Codex agent-event named pipe server (PLAN.md §6.4, §9.1). A failure to
    // bind (e.g. another process already squatting the pipe name) disables
    // Codex integration for this session rather than the whole app.
    let _pipe_server = match NamedPipeServer::spawn(|event| match event {
        PipeServerEvent::MessageReceived(bytes) => {
            let _ = slint::invoke_from_event_loop(move || {
                handle_agent_ui_event(AgentUiEvent::MessageReceived(bytes));
            });
        }
        PipeServerEvent::MessageTooLarge => {
            tracing::warn!("dropped an oversized Codex agent event");
        }
        PipeServerEvent::ConnectionError(message) => {
            tracing::warn!(error = %message, "Codex agent-event pipe connection error");
        }
    }) {
        Ok(server) => Some(server),
        Err(err) => {
            tracing::warn!(error = %err, "failed to start the Codex agent-event named pipe server");
            None
        }
    };

    let hotkey_thread = Rc::new(HotkeyThread::spawn(
        config.borrow().settings.quick_switcher_hotkey.clone(),
        move |event| {
            let ui_event = match event {
                HotkeyEvent::Pressed => HotkeyUiEvent::Pressed,
                HotkeyEvent::Registered => HotkeyUiEvent::Registered,
                HotkeyEvent::RegisterFailed(err) => {
                    HotkeyUiEvent::RegisterFailed(hotkey_register_error_message(err))
                }
            };
            let _ = slint::invoke_from_event_loop(move || handle_hotkey_ui_event(ui_event));
        },
    ));
    wire_settings(&window, data_dir.clone(), config.clone(), hotkey_thread);
    wire_codex_settings(&window, config.clone());
    wire_about_and_autostart(&window, config.clone(), data_dir.clone());

    // PLAN.md §3.3's `close_on_focus_loss` setting: Slint has no public API
    // for window-deactivation, so this reaches the popup's raw HWND
    // directly. `WM_ACTIVATE` runs on the UI thread itself (unlike
    // `WM_HOTKEY`), so this closure can capture `Rc`s normally.
    let c = config.clone();
    let s = quick_switcher.as_weak();
    let _deactivation_watch =
        popup_window::watch_deactivation(quick_switcher.window(), move || {
            if !c.borrow().settings.close_on_focus_loss {
                return;
            }
            if let Some(switcher) = s.upgrade() {
                let _ = switcher.hide();
            }
        });

    // --- Tray wiring (PLAN.md §3.4) ---
    let switcher_for_tray = quick_switcher.as_weak();
    let config_for_tray = config.clone();
    let data_dir_for_tray = data_dir.clone();
    tray.on_toggle_quick_switcher_requested(move || {
        let Some(switcher) = switcher_for_tray.upgrade() else {
            return;
        };
        let was_visible = switcher.window().is_visible();
        let config = config_for_tray.borrow();
        toggle_quick_switcher(&switcher, &config, &data_dir_for_tray);
        // PLAN.md §6.7: opening (not closing) via the tray selects the
        // highest-priority agent workset's row, if any is active.
        if !was_visible {
            let state = runtime_store::load(&data_dir_for_tray);
            if let Some((workset_id, _)) =
                highest_priority_agent_workset(&config, &state.agent_runs)
            {
                select_row_for_workset(&switcher, workset_id);
            }
        }
    });

    let switcher_for_menu = quick_switcher.as_weak();
    let config_for_menu = config.clone();
    let data_dir_for_menu = data_dir.clone();
    tray.on_quick_switcher_requested(move || {
        if let Some(switcher) = switcher_for_menu.upgrade() {
            show_quick_switcher_at_cursor(&switcher, &config_for_menu.borrow(), &data_dir_for_menu);
        }
    });

    let layout_studio_for_open = layout_studio.as_weak();
    tray.on_layout_studio_requested(move || {
        if let Some(layout_studio) = layout_studio_for_open.upgrade() {
            let _ = layout_studio.show();
        }
    });

    let workset_manager_for_register = workset_manager.as_weak();
    tray.on_register_workset_requested(move || {
        if let Some(workset_manager) = workset_manager_for_register.upgrade() {
            let _ = workset_manager.show();
            workset_manager.invoke_start_registration();
        }
    });

    let layout_studio_for_empty = layout_studio.as_weak();
    tray.on_empty_main_screen_requested(move || {
        if let Some(layout_studio) = layout_studio_for_empty.upgrade() {
            let _ = layout_studio.show();
            layout_studio.invoke_empty_main_screen_requested();
        }
    });

    let config_for_recover = config.clone();
    let coordinator_for_recover = coordinator.clone();
    tray.on_recover_all_requested(move || {
        if !confirm_recover_all() {
            return;
        }
        let config = config_for_recover.borrow();
        let live_monitors = monitor::enumerate_monitors().unwrap_or_default();
        let live_windows =
            enumerate::enumerate_top_level_windows(std::process::id()).unwrap_or_default();
        match coordinator_for_recover.recover_all_windows(
            &config.worksets,
            &live_monitors,
            &config.main_monitor_ids,
            &live_windows,
        ) {
            Ok(report) => tracing::info!(
                recovered = report.recovered.len(),
                skipped = report.skipped.len(),
                "recovered all managed windows from the tray menu"
            ),
            Err(err) => {
                tracing::warn!(error = %err, "failed to persist runtime state after recovering all windows");
            }
        }
    });

    let window_for_settings = window.as_weak();
    let config_for_settings = config.clone();
    tray.on_settings_requested(move || {
        if let Some(window) = window_for_settings.upgrade() {
            refresh_codex_settings_state(&window, &config_for_settings.borrow());
            let _ = window.show();
        }
    });

    let data_dir_for_log = data_dir.clone();
    tray.on_open_log_folder_requested(move || {
        let log_dir = logging::log_dir(&data_dir_for_log);
        if let Err(err) = std::process::Command::new("explorer.exe")
            .arg(&log_dir)
            .spawn()
        {
            tracing::warn!(error = %err, path = %log_dir.display(), "failed to open the log folder");
        }
    });

    // The settings window also carries its own shortcuts to the two
    // secondary windows, mirroring the tray menu entries.
    let layout_studio_for_main_window = layout_studio.as_weak();
    window.on_open_layout_studio_requested(move || {
        if let Some(layout_studio) = layout_studio_for_main_window.upgrade() {
            let _ = layout_studio.show();
        }
    });
    let workset_manager_for_main_window = workset_manager.as_weak();
    window.on_open_workset_manager_requested(move || {
        if let Some(workset_manager) = workset_manager_for_main_window.upgrade() {
            let _ = workset_manager.show();
        }
    });

    // Re-launching repodeck.exe while an instance is already running (e.g. a
    // taskbar-pinned icon click) signals this event instead of starting a second
    // process (PLAN.md §3.2). A dedicated thread blocks on it and marshals the
    // show request onto the Slint UI thread via the same `UI_CONTEXT` the
    // hotkey thread uses.
    let show_request_event = SendHandle(open_show_request_event()?);
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

            let _ = slint::invoke_from_event_loop(show_quick_switcher_from_context);
        }
    });

    tray.on_quit_requested(|| {
        tracing::info!("quit requested from the tray menu");
        let _ = slint::quit_event_loop();
    });

    // Unlike earlier phases, startup no longer force-shows any window (PLAN.md
    // §13 Phase 7 completion condition "GUI非表示でもプロセス継続") — the tray
    // icon alone keeps the process resident.
    tray.show()
        .context("failed to show the RepoDeck tray icon")?;
    if std::env::var_os("REPODECK_DEBUG_OPEN_LAYOUT_STUDIO").is_some() {
        layout_studio
            .show()
            .context("failed to show Layout Studio")?;
    }
    if std::env::var_os("REPODECK_DEBUG_OPEN_WORKSET_MANAGER").is_some() {
        workset_manager
            .show()
            .context("failed to show Workset Manager")?;
    }
    if std::env::var_os("REPODECK_DEBUG_OPEN_QUICK_SWITCHER").is_some() {
        quick_switcher
            .show()
            .context("failed to show the Quick Switcher")?;
    }
    if std::env::var_os("REPODECK_DEBUG_OPEN_SETTINGS").is_some() {
        window.show().context("failed to show Settings")?;
    }

    slint::run_event_loop().context("RepoDeck event loop failed")?;

    {
        let mut runtime = runtime_store::load(&data_dir);
        runtime.last_clean_shutdown = true;
        let _ = runtime_store::save(&data_dir, &runtime);
    }

    tracing::info!("RepoDeck exiting");
    Ok(())
}
