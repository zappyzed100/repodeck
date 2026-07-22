use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use slint::{Model, Timer, TimerMode};
use tracing_appender::non_blocking::WorkerGuard;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, INFINITE, WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::Win32::UI::WindowsAndMessaging::{
    IDNO, IDYES, MB_ICONWARNING, MB_YESNO, MB_YESNOCANCEL, MessageBoxW,
};
use windows::core::HSTRING;

use crate::application::agent_status_service;
use crate::application::crash_recovery::{self, JournalRecoveryChoice};
use crate::application::display_recovery_service::{self, RecoveryDecision};
use crate::application::launch_service;
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
use crate::domain::workset::ParkingPolicy;
use crate::hotkey::mouse_wheel_hook::MouseWheelHook;
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
use crate::windowing::{display_reset, power_watch};

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

/// Whether Windows' "Transparency effects" personalization setting
/// (設定 > 個人用設定 > 色) is currently on. Gates the Quick Switcher's
/// translucent backdrop: with the setting off, DWM doesn't blur behind the
/// window, and any backdrop alpha < 1.0 shows other windows' text sharply
/// through the popup (see `glass-base` in `ui/theme.slint`). Checked once at
/// startup; toggling the Windows setting takes effect on the next launch.
fn transparency_effects_enabled() -> bool {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};

    let path = HSTRING::from(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let name = HSTRING::from("EnableTransparency");
    let mut value: u32 = 0;
    let mut size = u32::try_from(std::mem::size_of::<u32>()).unwrap();
    // SAFETY: `path`/`name` are valid, NUL-terminated wide strings for the
    // duration of the call; `value`/`size` describe a DWORD-sized buffer,
    // matching `RRF_RT_REG_DWORD`.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &path,
            &name,
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::from_mut(&mut value).cast()),
            Some(&mut size),
        )
    };
    status == ERROR_SUCCESS && value == 1
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
    /// Unsaved per-monitor split edits; the value is `None` for 「自動」.
    auto_splits: HashMap<String, Option<AutoSplit>>,
    /// Unsaved per-monitor 「操作対象にしない」 edits.
    excluded_overrides: HashMap<String, bool>,
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
            excluded_overrides: HashMap::new(),
            selected_index: None,
            undo_snapshot: UndoSnapshot::default(),
            pending_candidates: Vec::new(),
            canvas_width: CANVAS_WIDTH,
            canvas_height: CANVAS_HEIGHT,
        }
    }

    /// The monitor's effective split choice: unsaved edit first, then the
    /// saved config; `None` means 「自動」 (also the default for monitors
    /// never configured).
    fn auto_split_for(&self, config: &AppConfig, device_name: &str) -> Option<AutoSplit> {
        if let Some(&choice) = self.auto_splits.get(device_name) {
            return choice;
        }
        config
            .monitors
            .iter()
            .find(|saved| saved.stable_id == device_name)
            .and_then(|saved| saved.auto_split)
    }

    /// The monitor's effective 「操作対象にしない」 state, unsaved edit first.
    fn excluded_for(&self, config: &AppConfig, device_name: &str) -> bool {
        if let Some(&excluded) = self.excluded_overrides.get(device_name) {
            return excluded;
        }
        config
            .monitors
            .iter()
            .find(|saved| saved.stable_id == device_name)
            .is_some_and(|saved| saved.excluded)
    }
}

/// Tile caption for a non-main monitor's split: 「自動」 shows what it
/// currently resolves to so the choice is never a black box.
fn auto_split_label(
    split: Option<AutoSplit>,
    work_area: crate::domain::placement::PixelRect,
) -> String {
    match split {
        None => format!(
            "自動 ({}分割)",
            layout_service::resolve_auto_split(work_area).cell_count()
        ),
        Some(split) => format!("{}分割", split.cell_count()),
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

    let main_ids: &[String] = &config.main_monitor_ids;

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
            let excluded = state.excluded_for(config, &monitor.device_name);
            let sub = config
                .sub_screens
                .iter()
                .find(|s| s.monitor_ids.iter().any(|id| id == &monitor.device_name));
            let split = state.auto_split_for(config, &monitor.device_name);
            let role_label = match main_order {
                Some(order) => format!("MAIN {}", order + 1),
                None if excluded => "対象外".to_string(),
                None => match sub {
                    Some(s) => format!("サブ: {}", s.name),
                    None => auto_split_label(split, monitor.work_area_px),
                },
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
                is_sub: sub.is_some(),
                is_excluded: excluded,
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
    layout.set_selected_excluded(state.excluded_for(config, &monitor.device_name));
    layout.set_selected_auto_split(match split {
        None => 0,
        Some(AutoSplit::One) => 1,
        Some(AutoSplit::TwoColumns) => 2,
        Some(AutoSplit::FourGrid) => 4,
    });
    layout.set_selected_monitor_index(i32::try_from(index).unwrap_or(-1));
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

    refresh_sub_screen_rows(layout, config, monitor.device_name.as_str());
}

/// Removes `device_name` from every sub-screen. Used to keep a monitor's role
/// exclusive (a monitor is main, a sub-screen member, excluded, or normal — not
/// several at once).
fn remove_monitor_from_all_sub_screens(config: &mut AppConfig, device_name: &str) {
    for sub in &mut config.sub_screens {
        sub.monitor_ids.retain(|id| id != device_name);
    }
}

/// Rebuilds the Layout Studio's sub-screen list, marking which ones contain
/// `device_name` (the selected monitor).
/// The sub-screen "region" options the region button cycles through: whole
/// area, left/right half, and the four quarters, as `(split, cell, label)`.
const SUB_REGIONS: [(AutoSplit, usize, &str); 7] = [
    (AutoSplit::One, 0, "全体"),
    (AutoSplit::TwoColumns, 0, "左半分"),
    (AutoSplit::TwoColumns, 1, "右半分"),
    (AutoSplit::FourGrid, 0, "左上¼"),
    (AutoSplit::FourGrid, 1, "右上¼"),
    (AutoSplit::FourGrid, 2, "左下¼"),
    (AutoSplit::FourGrid, 3, "右下¼"),
];

fn sub_region_label(split: AutoSplit, cell: usize) -> &'static str {
    SUB_REGIONS
        .iter()
        .find(|(s, c, _)| *s == split && *c == cell)
        .map_or("全体", |(_, _, label)| label)
}

/// The next `(split, cell)` in the region cycle after the given one.
fn next_sub_region(split: AutoSplit, cell: usize) -> (AutoSplit, usize) {
    let idx = SUB_REGIONS
        .iter()
        .position(|(s, c, _)| *s == split && *c == cell)
        .unwrap_or(0);
    let (s, c, _) = SUB_REGIONS[(idx + 1) % SUB_REGIONS.len()];
    (s, c)
}

fn refresh_sub_screen_rows(layout: &LayoutStudio, config: &AppConfig, device_name: &str) {
    let rows: Vec<SubScreenRow> = config
        .sub_screens
        .iter()
        .map(|s| SubScreenRow {
            name: s.name.clone().into(),
            assigned: s.monitor_ids.iter().any(|id| id == device_name),
            region_label: sub_region_label(s.split, s.cell_index).into(),
            fullscreen: s.fullscreen,
        })
        .collect();
    layout.set_sub_screen_rows(std::rc::Rc::new(slint::VecModel::from(rows)).into());
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
    layout.on_monitor_clicked(move |index| {
        let Some(layout) = l.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let mut state = s.borrow_mut();
        let config = c.borrow();
        state.selected_index = Some(index);
        refresh_selected_monitor_panel(&layout, &config, &state);
        refresh_monitor_tiles(&layout, &config, &mut state);
    });

    let l = layout.as_weak();
    let c = config.clone();
    let s = state.clone();
    layout.on_toggle_main_requested(move || {
        let Some(layout) = l.upgrade() else { return };
        let mut state = s.borrow_mut();
        let mut config = c.borrow_mut();
        let Some(monitor) = state.selected_index.and_then(|i| state.monitors.get(i)) else {
            return;
        };
        let device_name = monitor.device_name.clone();

        if let Some(pos) = config
            .main_monitor_ids
            .iter()
            .position(|id| id == &device_name)
        {
            config.main_monitor_ids.remove(pos);
            layout.set_status_text(
                "メイン画面から外しました。「設定を保存」で確定してください。".into(),
            );
        } else {
            config.main_monitor_ids.push(device_name.clone());
            // Main is mutually exclusive with 対象外 and sub-screen membership.
            state.excluded_overrides.insert(device_name.clone(), false);
            remove_monitor_from_all_sub_screens(&mut config, &device_name);
            layout.set_status_text(
                "メイン画面に登録しました。「設定を保存」で確定してください。".into(),
            );
        }
        layout.set_status_is_warning(false);
        refresh_selected_monitor_panel(&layout, &config, &state);
        refresh_monitor_tiles(&layout, &config, &mut state);
    });

    let l = layout.as_weak();
    let c = config.clone();
    let s = state.clone();
    layout.on_toggle_excluded_requested(move || {
        let Some(layout) = l.upgrade() else { return };
        let mut state = s.borrow_mut();
        let mut config = c.borrow_mut();
        let Some(monitor) = state.selected_index.and_then(|i| state.monitors.get(i)) else {
            return;
        };
        let device_name = monitor.device_name.clone();

        let excluded = !state.excluded_for(&config, &device_name);
        state
            .excluded_overrides
            .insert(device_name.clone(), excluded);
        if excluded {
            // 対象外 is mutually exclusive with main and sub-screen membership.
            if let Some(pos) = config
                .main_monitor_ids
                .iter()
                .position(|id| id == &device_name)
            {
                config.main_monitor_ids.remove(pos);
            }
            remove_monitor_from_all_sub_screens(&mut config, &device_name);
            layout.set_status_text(
                "このモニターを操作対象から外しました。「設定を保存」で確定してください。".into(),
            );
        } else {
            layout.set_status_text(
                "このモニターを操作対象に戻しました。「設定を保存」で確定してください。".into(),
            );
        }
        layout.set_status_is_warning(false);
        refresh_selected_monitor_panel(&layout, &config, &state);
        refresh_monitor_tiles(&layout, &config, &mut state);
    });

    // --- Sub-screen (退避先) management ---
    let l = layout.as_weak();
    let c = config.clone();
    let s = state.clone();
    layout.on_sub_screen_add(move |name| {
        let Some(layout) = l.upgrade() else { return };
        let name = name.trim().to_string();
        if name.is_empty() {
            layout.set_status_text("サブ画面の名前を入力してください。".into());
            layout.set_status_is_warning(true);
            return;
        }
        let mut state = s.borrow_mut();
        let mut config = c.borrow_mut();
        if config.sub_screens.iter().any(|sub| sub.name == name) {
            layout.set_status_text("同じ名前のサブ画面が既にあります。".into());
            layout.set_status_is_warning(true);
            return;
        }
        config.sub_screens.push(crate::domain::config::SubScreen {
            id: uuid::Uuid::new_v4(),
            name: name.clone(),
            monitor_ids: Vec::new(),
            split: AutoSplit::One,
            cell_index: 0,
            fullscreen: false,
        });
        layout.set_status_text(
            format!("サブ画面「{name}」を追加しました。モニターを割り当ててください。").into(),
        );
        layout.set_status_is_warning(false);
        refresh_selected_monitor_panel(&layout, &config, &state);
        refresh_monitor_tiles(&layout, &config, &mut state);
    });

    let l = layout.as_weak();
    let c = config.clone();
    let s = state.clone();
    let dir = data_dir.clone();
    layout.on_sub_screen_delete(move |index| {
        let Some(layout) = l.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let mut state = s.borrow_mut();
        let mut config = c.borrow_mut();
        if index >= config.sub_screens.len() {
            return;
        }
        let removed = config.sub_screens.remove(index);
        // Any workset that parked to this sub-screen falls back to 自動.
        for workset in &mut config.worksets {
            if workset.parking_policy
                == (crate::domain::workset::ParkingPolicy::SubScreen {
                    sub_screen_id: removed.id,
                })
            {
                workset.parking_policy = crate::domain::workset::ParkingPolicy::Auto;
            }
        }
        let _ = config_store::save(&dir, &config);
        layout.set_status_text(format!("サブ画面「{}」を削除しました。", removed.name).into());
        layout.set_status_is_warning(false);
        refresh_selected_monitor_panel(&layout, &config, &state);
        refresh_monitor_tiles(&layout, &config, &mut state);
    });

    let l = layout.as_weak();
    let c = config.clone();
    let s = state.clone();
    layout.on_sub_screen_toggle_monitor(move |index| {
        let Some(layout) = l.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let mut state = s.borrow_mut();
        let mut config = c.borrow_mut();
        let Some(device_name) = state
            .selected_index
            .and_then(|i| state.monitors.get(i))
            .map(|m| m.device_name.clone())
        else {
            return;
        };
        // A monitor can only belong to one sub-screen; assigning here removes it
        // from any other, from main, and from 対象外.
        let already = config
            .sub_screens
            .get(index)
            .is_some_and(|sub| sub.monitor_ids.iter().any(|id| id == &device_name));
        remove_monitor_from_all_sub_screens(&mut config, &device_name);
        if !already {
            if let Some(pos) = config
                .main_monitor_ids
                .iter()
                .position(|id| id == &device_name)
            {
                config.main_monitor_ids.remove(pos);
            }
            state.excluded_overrides.insert(device_name.clone(), false);
            if let Some(sub) = config.sub_screens.get_mut(index) {
                sub.monitor_ids.push(device_name);
            }
        }
        layout.set_status_text(
            "サブ画面の割当を更新しました。「設定を保存」で確定してください。".into(),
        );
        layout.set_status_is_warning(false);
        refresh_selected_monitor_panel(&layout, &config, &state);
        refresh_monitor_tiles(&layout, &config, &mut state);
    });

    let l = layout.as_weak();
    let c = config.clone();
    let s = state.clone();
    layout.on_sub_screen_cycle_region(move |index| {
        let Some(layout) = l.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let mut state = s.borrow_mut();
        let mut config = c.borrow_mut();
        if let Some(sub) = config.sub_screens.get_mut(index) {
            let (split, cell) = next_sub_region(sub.split, sub.cell_index);
            sub.split = split;
            sub.cell_index = cell;
        }
        layout.set_status_text(
            "サブ画面の領域を変更しました。「設定を保存」で確定してください。".into(),
        );
        layout.set_status_is_warning(false);
        refresh_selected_monitor_panel(&layout, &config, &state);
        refresh_monitor_tiles(&layout, &config, &mut state);
    });

    let l = layout.as_weak();
    let c = config.clone();
    let s = state.clone();
    layout.on_sub_screen_toggle_fullscreen(move |index| {
        let Some(layout) = l.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let state = s.borrow();
        let mut config = c.borrow_mut();
        if let Some(sub) = config.sub_screens.get_mut(index) {
            sub.fullscreen = !sub.fullscreen;
        }
        layout.set_status_text(
            "サブ画面の全画面設定を変更しました。「設定を保存」で確定してください。".into(),
        );
        layout.set_status_is_warning(false);
        refresh_selected_monitor_panel(&layout, &config, &state);
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
            1 => Some(AutoSplit::One),
            2 => Some(AutoSplit::TwoColumns),
            4 => Some(AutoSplit::FourGrid),
            _ => None, // 自動
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
                "メイン画面が未設定です。先にモニターを選択して「メイン画面に登録する」を押してください。".into(),
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
                excluded: state.excluded_for(&config, &monitor.device_name),
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
    fullscreen_when_parked: bool,
    /// "メイン画面を空にする" candidates awaiting the user's minimize choice.
    pending_empty_candidates: Vec<TopLevelWindow>,
    /// Rolling undo snapshot for the last "メイン画面を空にする" action.
    empty_undo_snapshot: UndoSnapshot,
    /// 退避先 selection for the workset being registered: -1 = 自動, otherwise
    /// an index into `AppConfig.sub_screens`.
    parking_selected_sub: i32,
    /// When set, the registration view is editing (re-registering) this
    /// existing workset rather than creating a new one.
    editing_workset_id: Option<uuid::Uuid>,
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
            fullscreen_when_parked: false,
            pending_empty_candidates: Vec::new(),
            empty_undo_snapshot: UndoSnapshot::default(),
            parking_selected_sub: -1,
            editing_workset_id: None,
        }
    }
}

/// Distinct workset colors offered at registration. A workset's color is its
/// at-a-glance identity, so already-used ones are filtered out of the choices.
const WORKSET_PALETTE: [&str; 16] = [
    "#2563eb", "#16a34a", "#d97706", "#dc2626", "#7c3aed", "#0891b2", "#db2777", "#65a30d",
    "#ea580c", "#4f46e5", "#0d9488", "#9333ea", "#ca8a04", "#e11d48", "#0ea5e9", "#f43f5e",
];

/// Sets the registration color swatches to the palette colors not already used
/// by an existing workset, and selects the first available one. Falls back to
/// the full palette if every color is somehow taken.
fn refresh_color_choices(
    manager: &WorksetManager,
    config: &AppConfig,
    state: &mut WorksetManagerState,
) {
    let used: std::collections::HashSet<String> = config
        .worksets
        .iter()
        .map(|w| w.color.to_ascii_lowercase())
        .collect();
    let available: Vec<&str> = WORKSET_PALETTE
        .iter()
        .copied()
        .filter(|c| !used.contains(&c.to_ascii_lowercase()))
        .collect();
    let list: Vec<&str> = if available.is_empty() {
        WORKSET_PALETTE.to_vec()
    } else {
        available
    };
    if let Some(first) = list.first() {
        state.selected_color = hex_to_color(first);
    }
    let colors: Vec<slint::Color> = list.iter().map(|c| hex_to_color(c)).collect();
    manager.set_color_choices(std::rc::Rc::new(slint::VecModel::from(colors)).into());
    manager.set_selected_color(state.selected_color);
}

/// Builds the relaunch spec for a window being registered: VS Code gets the
/// workset's repository path, a browser gets its live address-bar URL (read via
/// UI Automation), and anything else just relaunches its bare exe. Returns
/// `None` if the window has no known executable path.
fn capture_launch_spec(
    window: &TopLevelWindow,
    repository_path: &Path,
) -> Option<crate::domain::workset::LaunchSpec> {
    use crate::domain::workset::LaunchKind;
    let exe = window.executable_path.as_ref()?;
    let kind = launch_service::classify(exe);
    let browser_url = if kind == LaunchKind::Browser {
        crate::windowing::browser_url::read_browser_url(window.hwnd)
    } else {
        None
    };
    // For VS Code, prefer the folder/workspace the window actually has open (read
    // from its process command line) over the workset's `repository_path` — most
    // worksets have no repository_path, so relying on it left VS Code relaunching
    // with no folder (2026-07-23). Fall back to repository_path if the capture
    // fails (protected process, or a bare window with no path argument).
    let captured_vscode_folder = if kind == LaunchKind::VsCode {
        let folder = crate::windowing::process_info::read_process_command_line(window.process_id)
            .and_then(|cl| launch_service::extract_vscode_folder(&cl));
        tracing::info!(target: "launch", pid = window.process_id, folder = ?folder, "capture: VS Code open folder from command line");
        folder
    } else {
        None
    };
    let repo_folder = captured_vscode_folder
        .as_deref()
        .map(Path::new)
        .or_else(|| (!repository_path.as_os_str().is_empty()).then_some(repository_path));
    Some(launch_service::build_launch_spec(
        exe,
        repo_folder,
        browser_url.as_deref(),
    ))
}

/// Japanese label for a repository kind, shown in the registration screen.
fn repository_kind_label(kind: crate::domain::workset::RepositoryKind) -> &'static str {
    use crate::domain::workset::RepositoryKind;
    match kind {
        RepositoryKind::Git => "Gitリポジトリ",
        RepositoryKind::Directory => "通常フォルダー",
        RepositoryKind::Workspace => "ワークスペース",
    }
}

/// Populates the registration screen's 退避先 sub-screen chips from the current
/// config, clamping the selection back to 自動 if it's out of range.
fn refresh_parking_subs(
    manager: &WorksetManager,
    config: &AppConfig,
    state: &mut WorksetManagerState,
) {
    let names: Vec<slint::SharedString> = config
        .sub_screens
        .iter()
        .map(|s| s.name.clone().into())
        .collect();
    if state.parking_selected_sub >= i32::try_from(names.len()).unwrap_or(0) {
        state.parking_selected_sub = -1;
    }
    manager.set_parking_subs(std::rc::Rc::new(slint::VecModel::from(names)).into());
    manager.set_parking_selected_sub(state.parking_selected_sub);
}

/// Re-enumerates the top-level windows currently on the main screen and pushes
/// them into the registration candidate list. Shared by "現在の配置をセットとして
/// 登録" (start) and the "候補を更新" button. Returns whether any were found.
fn refresh_registration_candidates(
    manager: &WorksetManager,
    config: &AppConfig,
    state: &mut WorksetManagerState,
) -> bool {
    state.monitors = monitor::enumerate_monitors().unwrap_or_default();
    let main_bounds = resolve_main_monitor_bounds(&state.monitors, &config.main_monitor_ids);
    let live_windows =
        enumerate::enumerate_top_level_windows(std::process::id()).unwrap_or_default();
    let candidates = layout_service::find_windows_on_main_screen(&live_windows, &main_bounds);

    let model_items: Vec<RegistrationCandidate> = candidates
        .iter()
        .map(|w| RegistrationCandidate {
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
    manager.set_candidates(std::rc::Rc::new(slint::VecModel::from(model_items)).into());
    let found = !candidates.is_empty();
    state.registration_candidates = candidates;
    found
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
                repository_path: if workset.repository_path.as_os_str().is_empty() {
                    "（リポジトリなし）".into()
                } else {
                    workset.repository_path.display().to_string().into()
                },
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
    data_dir: &Path,
) {
    // Keep the Slint `selected-workset-index` in sync so the detail-pane
    // buttons (which enable on `>= 0`) and the row highlight reflect the
    // selection.
    manager.set_selected_workset_index(
        state
            .selected_workset_index
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or(-1),
    );
    let Some(index) = state.selected_workset_index else {
        manager.set_selected_workset_windows(
            std::rc::Rc::new(slint::VecModel::from(Vec::<ManagedWindowSummary>::new())).into(),
        );
        manager.set_selected_missing_count(0);
        return;
    };
    let Some(workset) = config.worksets.get(index) else {
        return;
    };

    let live_windows =
        enumerate::enumerate_top_level_windows(std::process::id()).unwrap_or_default();
    // Use the session HWND bindings (same as a real switch) so a window pinned
    // to a specific HWND shows as resolved, not "ambiguous", even when another
    // same-app window (e.g. a second browser) exists elsewhere.
    let bindings = runtime_store::load(data_dir).window_bindings;
    let decisions = workset_service::resolve_all_matches_with_bindings(
        &config.worksets,
        &live_windows,
        &bindings,
        // Resolve the set being inspected first, so a window it shares with
        // another set still shows as resolved here.
        Some(workset.id),
    );

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

    let missing = count_reopenable_missing(workset, &decisions);
    manager.set_selected_missing_count(i32::try_from(missing).unwrap_or(0));
    manager.set_selected_workset_windows(std::rc::Rc::new(slint::VecModel::from(rows)).into());
}

/// Counts a workset's windows that are currently closed (no confident live
/// match) AND carry a `launch_spec`, i.e. can be reopened.
fn count_reopenable_missing(
    workset: &crate::domain::workset::Workset,
    decisions: &HashMap<uuid::Uuid, MatchDecision>,
) -> usize {
    workset
        .windows
        .iter()
        .filter(|w| {
            w.launch_spec.is_some()
                && !matches!(decisions.get(&w.id), Some(MatchDecision::AutoRebind { .. }))
        })
        .count()
}

thread_local! {
    /// Keeps the single-shot "reopen closed apps → settle → place" timer alive
    /// between the launch and the deferred placement.
    static REOPEN_TIMER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
}

/// Switches to `workset_id` so its (now reopened) windows get matched and
/// placed. If it is already the current workset, the current id is first
/// cleared so the switch performs a full restore rather than a no-op.
fn reopen_place_workset(
    workset_id: uuid::Uuid,
    config: &Rc<RefCell<AppConfig>>,
    data_dir: &Path,
    coordinator: &SwitchCoordinator<Win32WindowOps>,
) {
    let mut runtime = runtime_store::load(data_dir);
    if runtime.current_workset_id == Some(workset_id) {
        runtime.current_workset_id = None;
        let _ = runtime_store::save(data_dir, &runtime);
    }
    let cfg = config.borrow();
    let live_monitors = monitor::enumerate_monitors().unwrap_or_default();
    let live_windows =
        enumerate::enumerate_top_level_windows(std::process::id()).unwrap_or_default();
    if let Err(err) = coordinator.switch_to(SwitchRequest {
        worksets: &cfg.worksets,
        fixed_slots: &cfg.fixed_slots,
        sub_screens: &cfg.sub_screens,
        saved_monitors: &cfg.monitors,
        main_monitor_ids: &cfg.main_monitor_ids,
        live_monitors: &live_monitors,
        live_windows: &live_windows,
        target_workset_id: workset_id,
    }) {
        tracing::warn!(error = %err, "reopen: failed to place workset after relaunch");
    }
}

/// What to acquire after launching a switch target's closed windows: the
/// pre-launch HWND set and, per launched window, its managed id + expected
/// executable/class so the newly-appeared window can be bound to it.
struct PendingAcquire {
    workset_id: uuid::Uuid,
    before: std::collections::HashSet<isize>,
    dead: Vec<(uuid::Uuid, PathBuf, String)>,
    /// Relaunched browsers whose launch spec carried no URL — they open a blank
    /// window with the address bar focused, so they are minimized once bound so
    /// stray keystrokes during the switch can't land in the omnibox (2026-07-23).
    blank_browsers: std::collections::HashSet<uuid::Uuid>,
}

/// Launches the target workset's closed (unresolved) windows that carry a
/// launch spec, returning what to acquire afterwards — or `None` if every
/// window is already open (nothing to launch). Browsers open a fresh window;
/// VS Code / Codex are only "dead" here if content matching already failed to
/// find their window, so relaunching them won't duplicate an open one.
fn launch_missing_for_switch(
    config: &AppConfig,
    bindings: &std::collections::HashMap<uuid::Uuid, isize>,
    live_windows: &[TopLevelWindow],
    target_id: uuid::Uuid,
) -> Option<PendingAcquire> {
    let workset = config.worksets.iter().find(|w| w.id == target_id)?;
    let decisions = workset_service::resolve_all_matches_with_bindings(
        &config.worksets,
        live_windows,
        bindings,
        Some(target_id),
    );

    let mut dead = Vec::new();
    let mut blank_browsers = std::collections::HashSet::new();
    for w in &workset.windows {
        let alive = matches!(decisions.get(&w.id), Some(MatchDecision::AutoRebind { .. }));
        if !alive && let Some(spec) = &w.launch_spec {
            match crate::windowing::app_launch::launch(spec) {
                Ok(()) => {
                    let no_url = spec.kind == crate::domain::workset::LaunchKind::Browser
                        && !spec.args.iter().any(|a| a.contains("://"));
                    tracing::info!(
                        target: "launch", managed = %w.id, program = %spec.program.display(),
                        class = %w.matcher.window_class, no_url,
                        "switch: relaunching closed app via set"
                    );
                    if no_url {
                        blank_browsers.insert(w.id);
                    }
                    dead.push((w.id, spec.program.clone(), w.matcher.window_class.clone()));
                }
                Err(err) => {
                    tracing::warn!(target: "launch", error = %err, program = %spec.program.display(), "switch: relaunch failed");
                }
            }
        }
    }

    if dead.is_empty() {
        return None;
    }
    tracing::info!(target: "launch", workset = %workset.name, relaunched = dead.len(), "switch: awaiting relaunched windows to bind");
    Some(PendingAcquire {
        workset_id: target_id,
        before: live_windows.iter().map(|w| w.hwnd).collect(),
        dead,
        blank_browsers,
    })
}

/// After launched windows have had time to appear, binds each newly-appeared
/// window (matched by executable + class, in launch order) to the managed
/// window it was launched for, then places the workset so they move to main.
fn acquire_launched_and_place(
    pending: PendingAcquire,
    config: &Rc<RefCell<AppConfig>>,
    data_dir: &Path,
    coordinator: &SwitchCoordinator<Win32WindowOps>,
) {
    let after = enumerate::enumerate_top_level_windows(std::process::id()).unwrap_or_default();
    let mut claimed: std::collections::HashSet<isize> = std::collections::HashSet::new();
    let mut blank_browser_hwnds: Vec<isize> = Vec::new();
    let mut runtime = runtime_store::load(data_dir);
    for (id, exe, class) in &pending.dead {
        if let Some(w) = after.iter().find(|w| {
            !pending.before.contains(&w.hwnd)
                && !claimed.contains(&w.hwnd)
                && w.window_class == *class
                && w.executable_path.as_ref() == Some(exe)
        }) {
            tracing::info!(target: "launch", managed = %id, hwnd = w.hwnd, class = %class, "switch: bound relaunched window to its set");
            runtime.window_bindings.insert(*id, w.hwnd);
            claimed.insert(w.hwnd);
            if pending.blank_browsers.contains(id) {
                blank_browser_hwnds.push(w.hwnd);
            }
        } else {
            tracing::warn!(target: "launch", managed = %id, program = %exe.display(), class = %class, "switch: relaunched app did not appear in time to bind");
        }
    }
    let _ = runtime_store::save(data_dir, &runtime);
    reopen_place_workset(pending.workset_id, config, data_dir, coordinator);

    // A blank browser (relaunched with no captured URL) opens with its address
    // bar focused; minimize it after placement so stray keystrokes during the
    // switch can't be typed into the omnibox.
    for hwnd in blank_browser_hwnds {
        tracing::info!(target: "launch", hwnd, "minimizing relaunched blank browser (no captured URL)");
        Win32WindowOps.minimize(hwnd);
    }
}

fn wire_workset_manager(
    manager: &WorksetManager,
    data_dir: PathBuf,
    config: Rc<RefCell<AppConfig>>,
    state: Rc<RefCell<WorksetManagerState>>,
    coordinator: Rc<SwitchCoordinator<Win32WindowOps>>,
) {
    {
        let mut state = state.borrow_mut();
        refresh_workset_summaries(manager, &config.borrow(), &mut state);
        refresh_color_choices(manager, &config.borrow(), &mut state);
    }

    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    let d = data_dir.clone();
    manager.on_refresh_requested(move || {
        let Some(manager) = m.upgrade() else { return };
        let mut state = s.borrow_mut();
        refresh_workset_summaries(&manager, &c.borrow(), &mut state);
        refresh_selected_workset_detail(&manager, &c.borrow(), &state, &d);
    });

    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    let d = data_dir.clone();
    manager.on_workset_selected(move |index| {
        let Some(manager) = m.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let mut state = s.borrow_mut();
        if index < c.borrow().worksets.len() {
            state.selected_workset_index = Some(index);
        }
        refresh_selected_workset_detail(&manager, &c.borrow(), &state, &d);
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
        refresh_selected_workset_detail(&manager, &c.borrow(), &state, &dir);
    });

    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    let dir = data_dir.clone();
    let coord = coordinator.clone();
    manager.on_reopen_closed_requested(move || {
        let Some(manager) = m.upgrade() else { return };
        let (workset_id, specs) = {
            let state = s.borrow();
            let config = c.borrow();
            let Some(workset) = state
                .selected_workset_index
                .and_then(|i| config.worksets.get(i))
            else {
                return;
            };
            let live_windows =
                enumerate::enumerate_top_level_windows(std::process::id()).unwrap_or_default();
            let decisions = workset_service::resolve_all_matches(&config.worksets, &live_windows);
            let specs: Vec<crate::domain::workset::LaunchSpec> = workset
                .windows
                .iter()
                .filter(|w| {
                    !matches!(decisions.get(&w.id), Some(MatchDecision::AutoRebind { .. }))
                })
                .filter_map(|w| w.launch_spec.clone())
                .collect();
            (workset.id, specs)
        };

        if specs.is_empty() {
            manager.set_status_text("開き直せる閉じたアプリはありませんでした。".into());
            manager.set_status_is_warning(false);
            return;
        }

        let mut launched = 0;
        for spec in &specs {
            match crate::windowing::app_launch::launch(spec) {
                Ok(()) => launched += 1,
                Err(err) => {
                    tracing::warn!(error = %err, program = %spec.program.display(), "reopen: launch failed");
                }
            }
        }
        manager.set_status_text(
            format!(
                "{launched}個のアプリを起動しました。配置を復元しています… （復元完了まで、同じアプリを手動で起動しないでください）"
            )
            .into(),
        );
        manager.set_status_is_warning(false);

        // Give the apps time to create their windows, then switch to the workset
        // so the reopened windows get matched and placed.
        let c2 = c.clone();
        let dir2 = dir.clone();
        let coord2 = coord.clone();
        let m2 = m.clone();
        let s2 = s.clone();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(2500),
            move || {
                reopen_place_workset(workset_id, &c2, &dir2, &coord2);
                if let Some(manager) = m2.upgrade() {
                    refresh_selected_workset_detail(&manager, &c2.borrow(), &s2.borrow(), &dir2);
                    manager.set_status_text("配置を復元しました。".into());
                    manager.set_status_is_warning(false);
                }
            },
        );
        REOPEN_TIMER.with(|t| *t.borrow_mut() = Some(timer));
    });

    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    manager.on_start_registration(move || {
        let Some(manager) = m.upgrade() else { return };
        let mut state = s.borrow_mut();
        let config = c.borrow();

        let found = refresh_registration_candidates(&manager, &config, &mut state);
        state.picked_repository = None;
        state.editing_workset_id = None;
        manager.set_registration_editing(false);
        manager.set_registration_name("".into());
        state.fullscreen_when_parked = false;
        manager.set_fullscreen_when_parked(false);
        // Reset the 退避先 picker to 自動 and refresh the sub-screen chips.
        state.parking_selected_sub = -1;
        refresh_parking_subs(&manager, &config, &mut state);
        // Offer only colors not already taken by an existing workset.
        refresh_color_choices(&manager, &config, &mut state);
        manager.set_picked_folder_label("（未選択）".into());
        manager.set_resolved_repo_label("".into());
        manager.set_registering(true);
        if found {
            manager.set_status_text(
                "登録するウィンドウを選び、名前を入力してください。リポジトリは任意です。".into(),
            );
            manager.set_status_is_warning(false);
        } else {
            manager.set_status_text(
                "メイン画面に候補ウィンドウがありません。ウィンドウを配置して「候補を更新」を押してください。".into(),
            );
            manager.set_status_is_warning(false);
        }
    });

    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    manager.on_refresh_candidates_requested(move || {
        let Some(manager) = m.upgrade() else { return };
        let mut state = s.borrow_mut();
        let found = refresh_registration_candidates(&manager, &c.borrow(), &mut state);
        manager.set_status_text(
            if found {
                "候補ウィンドウを更新しました。"
            } else {
                "メイン画面に候補ウィンドウが見つかりませんでした。"
            }
            .into(),
        );
        manager.set_status_is_warning(false);
    });

    let m = manager.as_weak();
    let s = state.clone();
    manager.on_fullscreen_toggled(move || {
        let Some(manager) = m.upgrade() else { return };
        let mut state = s.borrow_mut();
        state.fullscreen_when_parked = !state.fullscreen_when_parked;
        manager.set_fullscreen_when_parked(state.fullscreen_when_parked);
    });

    // --- 退避先 (自動 / サブ画面) selection ---
    let m = manager.as_weak();
    let s = state.clone();
    manager.on_parking_sub_selected(move |index| {
        let Some(manager) = m.upgrade() else { return };
        s.borrow_mut().parking_selected_sub = index;
        manager.set_parking_selected_sub(index);
    });

    // --- "メイン画面を空にする" (moved here from Layout Studio) ---
    // Runs directly, with no confirmation panel: registered windows on the main
    // screen are parked to their workset's destination; everything else on main
    // is minimized.
    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    let d = data_dir.clone();
    manager.on_empty_main_screen_requested(move || {
        let Some(manager) = m.upgrade() else { return };
        let mut state = s.borrow_mut();
        let config = c.borrow();

        if config.main_monitor_ids.is_empty() {
            manager.set_status_text(
                "メイン画面が未設定です。先にレイアウトスタジオでメイン画面を登録してください。"
                    .into(),
            );
            manager.set_status_is_warning(true);
            return;
        }

        let live_monitors = monitor::enumerate_monitors().unwrap_or_default();
        state.monitors = live_monitors.clone();
        let main_bounds = resolve_main_monitor_bounds(&live_monitors, &config.main_monitor_ids);
        let windows =
            enumerate::enumerate_top_level_windows(std::process::id()).unwrap_or_default();
        let candidates = layout_service::find_windows_on_main_screen(&windows, &main_bounds);

        if candidates.is_empty() {
            manager.set_status_text("メイン画面に対象のウィンドウはありませんでした。".into());
            manager.set_status_is_warning(false);
            return;
        }

        let (owner_park_rect, owner_fullscreen) =
            compute_empty_main_destinations(&config, &d, &live_monitors, &windows);

        let mut entries = Vec::new();
        let mut parked = 0usize;
        let mut minimized = 0usize;
        for window in &candidates {
            let hwnd = HWND(window.hwnd as *mut _);
            let before_show_state = match win_placement::get_show_state(hwnd) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let before_rect = match win_placement::get_normal_rect(hwnd) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if let Some(rect) = owner_park_rect.get(&window.hwnd) {
                // Un-maximize + fill the cell (visible bounds, so the invisible
                // DWM border doesn't leave a gutter between windows).
                win_placement::set_placement(hwnd, *rect, false, true);
                if owner_fullscreen.contains(&window.hwnd) {
                    crate::windowing::key_input::send_fullscreen_keys(hwnd, None);
                }
                parked += 1;
            } else {
                win_placement::minimize(hwnd);
                minimized += 1;
            }
            entries.push(UndoEntry {
                hwnd: window.hwnd,
                process_id: window.process_id,
                before_rect,
                before_show_state,
            });
        }

        state.empty_undo_snapshot = UndoSnapshot { entries };
        manager.set_empty_undo_available(!state.empty_undo_snapshot.is_empty());
        manager.set_empty_screen_panel_visible(false);
        manager
            .set_status_text(format!("{parked}個を退避、{minimized}個を最小化しました。").into());
        manager.set_status_is_warning(false);
    });

    let m = manager.as_weak();
    manager.on_empty_candidate_toggled(move |index| {
        let Some(manager) = m.upgrade() else { return };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let model = manager.get_main_screen_candidates();
        if let Some(mut row) = model.row_data(index) {
            row.checked = !row.checked;
            model.set_row_data(index, row);
        }
    });

    let m = manager.as_weak();
    let s = state.clone();
    manager.on_cancel_empty_main_screen(move || {
        let Some(manager) = m.upgrade() else { return };
        s.borrow_mut().pending_empty_candidates.clear();
        manager.set_empty_screen_panel_visible(false);
        manager.set_status_text("キャンセルしました。".into());
        manager.set_status_is_warning(false);
    });

    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    let d = data_dir.clone();
    manager.on_confirm_empty_main_screen(move || {
        let Some(manager) = m.upgrade() else { return };
        let mut state = s.borrow_mut();

        // Map each live HWND to the workset that owns it (if any) and, for
        // worksets with a sub-screen destination, the rect to park it into —
        // so a window registered in another set is sent to its parking area
        // instead of being minimized.
        let live_monitors = monitor::enumerate_monitors().unwrap_or_default();
        let (owner_park_rect, owner_fullscreen): (
            std::collections::HashMap<isize, crate::domain::placement::PixelRect>,
            std::collections::HashSet<isize>,
        ) = {
            let config = c.borrow();
            let live_windows =
                enumerate::enumerate_top_level_windows(std::process::id()).unwrap_or_default();
            let runtime = runtime_store::load(&d);
            let decisions = workset_service::resolve_all_matches_with_bindings(
                &config.worksets,
                &live_windows,
                &runtime.window_bindings,
                None,
            );

            // Auto/Fixed worksets park to allocator-assigned cells; SubScreen
            // worksets to their sub cell. `current_workset_id: None` so the
            // *active* workset (whose windows are on main) is parked too — the
            // whole point of emptying the main screen.
            use crate::application::parking_allocator::{
                AllocationInput, ParkAssignment, ParkingSlotId, allocate_parking,
            };
            let previous: std::collections::HashMap<uuid::Uuid, ParkingSlotId> = runtime
                .auto_slot_assignments
                .iter()
                .filter_map(|(k, v)| {
                    Some((uuid::Uuid::parse_str(k).ok()?, ParkingSlotId::decode(v)?))
                })
                .collect();
            let sub_screen_monitor_ids: Vec<String> = config
                .sub_screens
                .iter()
                .flat_map(|s| s.monitor_ids.iter().cloned())
                .collect();
            // Only sets with live windows reserve a cell (see allocate_parking).
            let worksets_with_windows: std::collections::HashSet<uuid::Uuid> = config
                .worksets
                .iter()
                .filter(|w| {
                    w.windows.iter().any(|mw| {
                        matches!(decisions.get(&mw.id), Some(MatchDecision::AutoRebind { .. }))
                    })
                })
                .map(|w| w.id)
                .collect();
            let allocation = allocate_parking(&AllocationInput {
                worksets: &config.worksets,
                current_workset_id: None,
                fixed_slots: &config.fixed_slots,
                main_monitor_ids: &config.main_monitor_ids,
                live_monitors: &live_monitors,
                saved_monitors: &config.monitors,
                sub_screen_monitor_ids: &sub_screen_monitor_ids,
                previous_assignments: &previous,
                worksets_with_windows: &worksets_with_windows,
            });

            let mut rects = std::collections::HashMap::new();
            let mut fullscreen = std::collections::HashSet::new();
            for workset in &config.worksets {
                use crate::domain::workset::ParkingPolicy;
                // Where this workset's windows should park, and whether to send
                // full-screen keys. `None` → its windows fall through to minimize.
                let dest: Option<(crate::domain::placement::PixelRect, bool)> =
                    match &workset.parking_policy {
                        ParkingPolicy::SubScreen { sub_screen_id } => config
                            .sub_screens
                            .iter()
                            .find(|s| s.id == *sub_screen_id)
                            .and_then(|sub| {
                                let slot =
                                    crate::application::switch_coordinator::sub_screen_slot_rect(
                                        sub,
                                        &config.worksets,
                                        workset.id,
                                        &live_monitors,
                                    )?;
                                // Full-screen only for a sole occupant — a shared
                                // sub-screen can't have overlapping full windows.
                                let sole = crate::application::switch_coordinator::sub_screen_sharer_count(
                                    &config.worksets,
                                    *sub_screen_id,
                                ) <= 1;
                                Some((slot, sole && (workset.fullscreen_when_parked || sub.fullscreen)))
                            }),
                        _ => match allocation.assignments.get(&workset.id) {
                            Some(ParkAssignment::AutoSlot { rect, .. })
                            | Some(ParkAssignment::FixedSlot { rect, .. }) => {
                                Some((*rect, workset.fullscreen_when_parked))
                            }
                            _ => None,
                        },
                    };
                let Some((slot, want_fullscreen)) = dest else {
                    continue;
                };
                // Resolve this workset's live windows and their main-screen rects
                // so they can be shrunk into the cell preserving relative layout.
                let mut hwnds = Vec::new();
                let mut main_rects = Vec::new();
                for w in &workset.windows {
                    if let Some(MatchDecision::AutoRebind { hwnd }) = decisions.get(&w.id)
                        && let Some(outcome) =
                            crate::application::main_placement::resolve_main_restore(
                                &w.main_placement,
                                &live_monitors,
                                &config.main_monitor_ids,
                            )
                    {
                        hwnds.push(*hwnd);
                        main_rects.push(outcome.rect);
                    }
                }
                if hwnds.is_empty() {
                    continue;
                }
                match crate::application::parking_placement::plan_park_into_slot(&main_rects, slot) {
                    crate::application::parking_placement::ParkPlan::ShrinkToFit(mapped) => {
                        for (hwnd, rect) in hwnds.iter().zip(mapped) {
                            rects.insert(*hwnd, rect);
                            if want_fullscreen {
                                fullscreen.insert(*hwnd);
                            }
                        }
                    }
                    // Windows too small to park → left out, minimized below.
                    crate::application::parking_placement::ParkPlan::MinimizeWhole => {}
                }
            }
            (rects, fullscreen)
        };

        let model = manager.get_main_screen_candidates();
        let mut entries = Vec::new();
        let mut parked = 0usize;
        let mut minimized = 0usize;
        for (index, window) in state.pending_empty_candidates.iter().enumerate() {
            let checked = model.row_data(index).map(|row| row.checked).unwrap_or(false);
            if !checked {
                continue;
            }
            let hwnd = HWND(window.hwnd as *mut _);
            let before_show_state = match win_placement::get_show_state(hwnd) {
                Ok(s) => s,
                Err(err) => {
                    tracing::warn!(error = %err, hwnd = window.hwnd, "empty-main: skipping window, failed to read show state");
                    continue;
                }
            };
            let before_rect = match win_placement::get_normal_rect(hwnd) {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!(error = %err, hwnd = window.hwnd, "empty-main: skipping window, failed to read normal rect");
                    continue;
                }
            };

            if let Some(rect) = owner_park_rect.get(&window.hwnd) {
                // Registered in another set with a sub-screen: park it there.
                win_placement::restore(hwnd);
                if win_placement::set_window_rect(hwnd, *rect).is_ok() {
                    let want_fs = owner_fullscreen.contains(&window.hwnd);
                    if want_fs {
                        // Send the browser's own full-screen key (F) so a parked
                        // video fills the sub-screen. No refocus target here — the
                        // main screen is being emptied.
                        crate::windowing::key_input::send_fullscreen_keys(hwnd, None);
                    }
                    parked += 1;
                } else {
                    win_placement::minimize(hwnd);
                    minimized += 1;
                }
            } else {
                win_placement::minimize(hwnd);
                minimized += 1;
            }
            entries.push(UndoEntry {
                hwnd: window.hwnd,
                process_id: window.process_id,
                before_rect,
                before_show_state,
            });
        }

        state.empty_undo_snapshot = UndoSnapshot { entries };
        state.pending_empty_candidates.clear();

        manager.set_empty_undo_available(!state.empty_undo_snapshot.is_empty());
        manager.set_empty_screen_panel_visible(false);
        manager.set_status_text(
            format!("{parked}個を退避、{minimized}個を最小化しました。").into(),
        );
        manager.set_status_is_warning(false);
    });

    let m = manager.as_weak();
    let s = state.clone();
    manager.on_undo_empty_main_screen(move || {
        let Some(manager) = m.upgrade() else { return };
        let mut state = s.borrow_mut();
        perform_undo(&state.empty_undo_snapshot);
        let count = state.empty_undo_snapshot.entries.len();
        state.empty_undo_snapshot = UndoSnapshot::default();
        manager.set_empty_undo_available(false);
        manager.set_status_text(format!("{count}個のウィンドウを元に戻しました。").into());
        manager.set_status_is_warning(false);
    });

    let m = manager.as_weak();
    let s = state.clone();
    manager.on_cancel_registration(move || {
        let Some(manager) = m.upgrade() else { return };
        s.borrow_mut().editing_workset_id = None;
        manager.set_registration_editing(false);
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
        manager.set_resolved_repo_label(
            format!(
                "{}として登録されます: {}",
                repository_kind_label(repository_kind),
                repository_path.display()
            )
            .into(),
        );
        s.borrow_mut().picked_repository = Some((repository_path, repository_kind));
        manager.set_status_text("".into());
        manager.set_status_is_warning(false);
    });

    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    manager.on_pick_workspace_requested(move || {
        let Some(manager) = m.upgrade() else { return };
        let Some(file) = rfd::FileDialog::new()
            .add_filter("VS Code ワークスペース", &["code-workspace"])
            .pick_file()
        else {
            return;
        };

        // Parsed eagerly (not just stored) so a malformed/empty workspace
        // file is rejected at registration time — otherwise agent cwd
        // matching (`agent_status_service::map_event_to_workset`) would
        // silently never succeed for this workset instead of failing loudly
        // here.
        let folder = match workset_service::resolve_workspace_file(&file) {
            Ok(folder) => folder,
            Err(err) => {
                manager.set_status_text(err.to_string().into());
                manager.set_status_is_warning(true);
                return;
            }
        };

        let repository_kind = crate::domain::workset::RepositoryKind::Workspace;
        if workset_service::is_duplicate_repository(&c.borrow().worksets, &file) {
            manager.set_status_text("このワークスペースは既に登録されています。".into());
            manager.set_status_is_warning(true);
            return;
        }

        manager.set_picked_folder_label(file.display().to_string().into());
        manager.set_resolved_repo_label(
            format!(
                "{}として登録されます: {}（フォルダー: {}）",
                repository_kind_label(repository_kind),
                file.display(),
                folder.display()
            )
            .into(),
        );
        s.borrow_mut().picked_repository = Some((file, repository_kind));
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
        // Repository is optional: with no folder picked, register with an empty
        // path (validation and de-dup both skip empty paths).
        let (repository_path, repository_kind) = state.picked_repository.clone().unwrap_or_else(|| {
            (
                PathBuf::new(),
                crate::domain::workset::RepositoryKind::Directory,
            )
        });

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
        // The authoritative HWND for each newly registered window: the user
        // picked it explicitly, so seed the session binding from it.
        let mut seed_bindings: Vec<(uuid::Uuid, isize)> = Vec::new();
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
                Ok(mut managed_window) => {
                    managed_window.launch_spec =
                        capture_launch_spec(window, repository_path.as_path());
                    seed_bindings.push((managed_window.id, window.hwnd));
                    managed_windows.push(managed_window);
                }
                Err(err) => tracing::warn!(error = %err, hwnd = window.hwnd, "registration: skipping window not on a main monitor"),
            }
        }

        if managed_windows.is_empty() {
            manager.set_status_text("登録できるウィンドウがありませんでした。".into());
            manager.set_status_is_warning(true);
            return;
        }

        let color_hex = color_to_hex(state.selected_color);
        let editing_id = state.editing_workset_id;
        let save_result = {
            let mut config = c.borrow_mut();
            let sort_order = i32::try_from(config.worksets.len()).unwrap_or(i32::MAX);
            let mut workset = workset_service::build_workset(name, color_hex, repository_path, repository_kind, sort_order, managed_windows);
            workset.fullscreen_when_parked = state.fullscreen_when_parked;

            // 退避先: -1 = 自動, otherwise the sub-screen at that index.
            if state.parking_selected_sub >= 0
                && let Some(sub) = usize::try_from(state.parking_selected_sub)
                    .ok()
                    .and_then(|i| config.sub_screens.get(i))
            {
                workset.parking_policy = crate::domain::workset::ParkingPolicy::SubScreen {
                    sub_screen_id: sub.id,
                };
            }

            // Editing ("配置を再登録") replaces the existing set's fields in
            // place (keeping its id/created_at/sort_order/hotkey); a fresh
            // registration appends a new set. Snapshot for rollback on error.
            let backup = editing_id.and_then(|id| {
                config
                    .worksets
                    .iter()
                    .find(|w| w.id == id)
                    .map(|w| (w.id, w.clone()))
            });
            if let Some((existing_id, _)) = &backup
                && let Some(existing) = config.worksets.iter_mut().find(|w| &w.id == existing_id)
            {
                existing.name = workset.name.clone();
                existing.repository_path = workset.repository_path.clone();
                existing.repository_kind = workset.repository_kind;
                existing.color = workset.color.clone();
                existing.parking_policy = workset.parking_policy.clone();
                existing.fullscreen_when_parked = workset.fullscreen_when_parked;
                existing.windows = workset.windows.clone();
                existing.updated_at = clock::now_rfc3339();
            } else {
                config.worksets.push(workset);
            }

            let errors = config.validate();
            // Registering the same window in more than one workset is allowed —
            // at switch time a window simply resolves to whichever workset claims
            // it first — so it is not a fatal error, only the rest are.
            let fatal: Vec<String> = errors
                .iter()
                .filter(|e| {
                    !matches!(
                        e,
                        crate::domain::config::ConfigValidationError::DuplicateWindowMatcher { .. }
                    )
                })
                .map(config_validation_message)
                .collect();
            let restore = |config: &mut AppConfig| match &backup {
                Some((id, original)) => {
                    if let Some(w) = config.worksets.iter_mut().find(|w| &w.id == id) {
                        *w = original.clone();
                    }
                }
                None => {
                    config.worksets.pop();
                }
            };
            if !fatal.is_empty() {
                restore(&mut config);
                Err(fatal.join(" / "))
            } else {
                match config_store::save(&dir, &config) {
                    Ok(()) => Ok(()),
                    Err(err) => {
                        restore(&mut config);
                        Err(err.to_string())
                    }
                }
            }
        };

        match save_result {
            Ok(()) => {
                // Seed session HWND bindings from the exact windows the user
                // picked, so switches re-find them even as titles/URLs change.
                let mut runtime = runtime_store::load(&dir);
                for (id, hwnd) in seed_bindings {
                    runtime.window_bindings.insert(id, hwnd);
                }
                let _ = runtime_store::save(&dir, &runtime);

                state.registering = false;
                let was_editing = state.editing_workset_id.is_some();
                state.editing_workset_id = None;
                manager.set_registering(false);
                manager.set_registration_editing(false);
                manager.set_status_text(
                    if was_editing {
                        "セットを更新しました。"
                    } else {
                        "ワークセットを登録しました。"
                    }
                    .into(),
                );
                manager.set_status_is_warning(false);
                refresh_workset_summaries(&manager, &c.borrow(), &mut state);
                refresh_selected_workset_detail(&manager, &c.borrow(), &state, &dir);
            }
            Err(message) => {
                manager.set_status_text(format!("登録できません: {message}").into());
                manager.set_status_is_warning(true);
            }
        }
    });

    // "配置を再登録": re-enter the same registration flow as a first
    // registration, but scoped to the selected set — detect the windows now on
    // the main screen, let the user re-pick and adjust, then update the set.
    let m = manager.as_weak();
    let c = config.clone();
    let s = state.clone();
    manager.on_recapture_placement_requested(move || {
        let Some(manager) = m.upgrade() else { return };
        let mut state = s.borrow_mut();
        let config = c.borrow();
        let Some(workset) = state
            .selected_workset_index
            .and_then(|i| config.worksets.get(i))
        else {
            return;
        };

        // Pre-fill the registration view from the existing set.
        state.editing_workset_id = Some(workset.id);
        let repo = workset.repository_path.clone();
        state.picked_repository = if repo.as_os_str().is_empty() {
            None
        } else {
            Some((repo, workset.repository_kind))
        };
        state.selected_color = hex_to_color(&workset.color);
        state.fullscreen_when_parked = workset.fullscreen_when_parked;
        state.parking_selected_sub = match &workset.parking_policy {
            crate::domain::workset::ParkingPolicy::SubScreen { sub_screen_id } => config
                .sub_screens
                .iter()
                .position(|sub| sub.id == *sub_screen_id)
                .and_then(|i| i32::try_from(i).ok())
                .unwrap_or(-1),
            _ => -1,
        };
        manager.set_registration_name(workset.name.clone().into());
        manager.set_registration_editing(true);
        manager.set_fullscreen_when_parked(state.fullscreen_when_parked);

        let found = refresh_registration_candidates(&manager, &config, &mut state);
        refresh_parking_subs(&manager, &config, &mut state);
        // Show the full palette (including this set's current color) when editing.
        manager.set_color_choices(
            std::rc::Rc::new(slint::VecModel::from(
                WORKSET_PALETTE.iter().map(|c| hex_to_color(c)).collect::<Vec<_>>(),
            ))
            .into(),
        );
        manager.set_selected_color(state.selected_color);

        let repo_label = state
            .picked_repository
            .as_ref()
            .map_or_else(|| "（リポジトリなし）".to_string(), |(p, _)| p.display().to_string());
        manager.set_picked_folder_label(repo_label.into());
        manager.set_resolved_repo_label("".into());
        manager.set_registering(true);
        manager.set_status_text(
            if found {
                "登録し直すウィンドウを選び直してください。"
            } else {
                "メイン画面に候補ウィンドウがありません。配置してから「候補を更新」を押してください。"
            }
            .into(),
        );
        manager.set_status_is_warning(false);
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
            let (_, agent_elapsed_text) =
                agent_row_status(&state.agent_runs, workset.id, agent_state);
            let (agent_symbol, agent_status_label) = agent_symbol_and_label(agent_state);
            let git = cached_git_status(&workset.repository_path);
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
                agent_symbol: agent_symbol.into(),
                agent_status_label: agent_status_label.into(),
                agent_elapsed_text: agent_elapsed_text.into(),
                branch_text: git.branch.clone().unwrap_or_default().into(),
                changed_text: if git.is_git {
                    format!("変更{}", git.changed_count).into()
                } else {
                    slint::SharedString::new()
                },
                last_commit_text: git
                    .last_commit_at
                    .as_deref()
                    .map(format_commit_relative)
                    .unwrap_or_default()
                    .into(),
            }
        })
        .collect();

    let row_count = i32::try_from(rows.len()).unwrap_or(i32::MAX);
    switcher.set_rows(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    // The slot at index == row_count is the "empty the main screen" action, so
    // it is a valid selection even when it sits one past the last workset row.
    if switcher.get_selected_index() > row_count {
        switcher.set_selected_index(row_count);
    } else if switcher.get_selected_index() < 0 {
        switcher.set_selected_index(0);
    }
}

/// Quick Switcher status cell for an aggregate agent state: a mark plus a
/// Japanese word, always shown (including 未実行 for Idle/Unknown, unlike the
/// tray tooltip). `×` is U+00D7, which the UI font renders (unlike U+2715);
/// the colour (`state_color`) carries the success/failure meaning alongside.
fn agent_symbol_and_label(state: AgentState) -> (&'static str, &'static str) {
    match state {
        AgentState::Running => ("●", "実行中"),
        AgentState::NeedsInput => ("●", "入力待ち"),
        AgentState::Ready => ("✓", "成功"),
        AgentState::Blocked => ("×", "失敗"),
        AgentState::Idle | AgentState::Unknown => ("—", "未実行"),
    }
}

/// Relative "最終commit" text (「18分前」) from a commit's RFC 3339 timestamp,
/// computed fresh at display time. Empty when the timestamp can't be parsed.
fn format_commit_relative(timestamp: &str) -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    let Ok(parsed) = OffsetDateTime::parse(timestamp, &Rfc3339) else {
        return String::new();
    };
    let minutes = (OffsetDateTime::now_utc() - parsed).whole_minutes();
    crate::domain::git::relative_label(minutes)
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
    // Kick a background git refresh; rows re-render when it completes. The
    // switcher shows the cached (possibly stale/empty) values immediately.
    spawn_git_refresh(workset_repo_paths(config));

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
        // Hiding aborts any in-flight hold-to-cycle session (e.g. the user taps
        // the main hotkey key while still holding the cycle modifiers): the
        // release timer must not survive to commit an unconfirmed switch.
        cancel_cycle_timer();
        let _ = switcher.hide();
    } else {
        show_quick_switcher_at_cursor(switcher, config, data_dir);
    }
}

/// Concise Japanese message for a config validation error, so the registration
/// banner stays readable rather than dumping the `Display` form (full exe paths
/// and window classes, which overflow the dialog).
fn config_validation_message(error: &crate::domain::config::ConfigValidationError) -> String {
    use crate::domain::config::ConfigValidationError as E;
    match error {
        E::WorksetNameLength { .. } => "セット名は1〜80文字にしてください。".to_string(),
        E::DuplicateWindowMatcher {
            registered_title, ..
        } => format!("「{registered_title}」は既に別のセットに登録されています。"),
        other => other.to_string(),
    }
}

/// Maps each registered window's live HWND to the rect it should park into when
/// the main screen is emptied, plus the set that additionally wants the browser
/// full-screen keys. Two tracks (development-plan.md「退避の優先順位ルール」):
///
/// - **Sub-screen-designated** worksets go to their own sub cell (subdivided
///   1→2→4 among worksets sharing that sub); full-screen when sole occupant.
/// - **Everything else** (Auto/Fixed worksets' windows) is distributed across
///   the *non-sub* parking screens by `distribute_parking`: emptiest screen
///   first, each screen subdivided by its window count, never overlapping. What
///   doesn't fit anywhere is absent here (→ minimized by the caller).
fn compute_empty_main_destinations(
    config: &AppConfig,
    data_dir: &std::path::Path,
    live_monitors: &[MonitorInfo],
    live_windows: &[TopLevelWindow],
) -> (
    std::collections::HashMap<isize, crate::domain::placement::PixelRect>,
    std::collections::HashSet<isize>,
) {
    use crate::application::main_placement::resolve_main_restore;
    use crate::application::parking_placement::{ParkPlan, plan_park_into_slot};
    use crate::application::switch_coordinator as sc;
    use crate::domain::placement::PixelRect;
    use crate::domain::workset::ParkingPolicy;

    let runtime = runtime_store::load(data_dir);
    let decisions = workset_service::resolve_all_matches_with_bindings(
        &config.worksets,
        live_windows,
        &runtime.window_bindings,
        None,
    );

    let mut rects = std::collections::HashMap::new();
    let mut fullscreen = std::collections::HashSet::new();
    // Non-designated registered windows, collected for distribution: (hwnd, its
    // main-screen rect for aspect-preserving shrink).
    let mut to_distribute: Vec<(isize, PixelRect)> = Vec::new();
    // Sub-screens that actually receive a designated window this pass. Only these
    // are withheld from general parking; an *empty* sub-screen is fair game as a
    // parking target (「サブにウィンドウが入っているときは入れない」, 2026-07-23).
    let mut occupied_sub_ids: std::collections::HashSet<uuid::Uuid> =
        std::collections::HashSet::new();

    for workset in &config.worksets {
        // Resolve this workset's live on-main windows once.
        let mut resolved: Vec<(isize, PixelRect)> = Vec::new();
        for w in &workset.windows {
            if let Some(MatchDecision::AutoRebind { hwnd }) = decisions.get(&w.id)
                && let Some(outcome) =
                    resolve_main_restore(&w.main_placement, live_monitors, &config.main_monitor_ids)
            {
                resolved.push((*hwnd, outcome.rect));
            }
        }
        if resolved.is_empty() {
            continue;
        }

        let ParkingPolicy::SubScreen { sub_screen_id } = &workset.parking_policy else {
            // Non-designated → distribute across the non-sub parking screens.
            to_distribute.extend(resolved);
            continue;
        };
        // Designated → its own sub cell.
        let dest = config
            .sub_screens
            .iter()
            .find(|s| s.id == *sub_screen_id)
            .and_then(|sub| {
                let slot =
                    sc::sub_screen_slot_rect(sub, &config.worksets, workset.id, live_monitors)?;
                let sole = sc::sub_screen_sharer_count(&config.worksets, *sub_screen_id) <= 1;
                Some((
                    slot,
                    sole && (workset.fullscreen_when_parked || sub.fullscreen),
                ))
            });
        let Some((slot, want_fullscreen)) = dest else {
            continue;
        };
        // Same algorithm as a normal switch: subdivide the sub cell among the
        // workset's windows so each fills its own flush cell (projecting the
        // whole main layout kept the original gaps and looked broken).
        let cells = sc::subdivide_for_count(slot, resolved.len());
        for (i, (hwnd, main_rect)) in resolved.iter().enumerate() {
            let Some(&cell) = cells.get(i) else {
                continue; // beyond capacity → minimized by the caller
            };
            if let ParkPlan::ShrinkToFit(mapped) =
                plan_park_into_slot(std::slice::from_ref(main_rect), cell)
                && let Some(&rect) = mapped.first()
            {
                rects.insert(*hwnd, rect);
                occupied_sub_ids.insert(*sub_screen_id);
                if want_fullscreen {
                    fullscreen.insert(*hwnd);
                }
            }
        }
    }

    // Parking screens = live monitors that are non-main, non-excluded, and not
    // an *occupied* sub-screen. A sub-screen only counts as reserved when a
    // designated workset actually parked a window into it this pass; an empty
    // sub-screen is usable as a general parking target (2026-07-23).
    let occupied_sub_monitor_ids: std::collections::HashSet<&str> = config
        .sub_screens
        .iter()
        .filter(|s| occupied_sub_ids.contains(&s.id))
        .flat_map(|s| s.monitor_ids.iter().map(String::as_str))
        .collect();
    let parking_screens: Vec<PixelRect> = live_monitors
        .iter()
        .filter(|m| {
            !config
                .main_monitor_ids
                .iter()
                .any(|id| id == &m.device_name)
        })
        .filter(|m| !occupied_sub_monitor_ids.contains(m.device_name.as_str()))
        .filter(|m| {
            !config
                .monitors
                .iter()
                .any(|s| s.stable_id == m.device_name && s.excluded)
        })
        .map(|m| m.work_area_px)
        .collect();

    let hwnds: Vec<isize> = to_distribute.iter().map(|(h, _)| *h).collect();
    let main_rect_of: std::collections::HashMap<isize, PixelRect> =
        to_distribute.into_iter().collect();
    let (placements, _overflow) = sc::distribute_parking(&parking_screens, &hwnds);
    for (hwnd, cell) in placements {
        // Shrink the single window into its cell, preserving aspect; too small
        // → left out (minimized).
        if let Some(main_rect) = main_rect_of.get(&hwnd)
            && let ParkPlan::ShrinkToFit(mapped) = plan_park_into_slot(&[*main_rect], cell)
            && let Some(rect) = mapped.first()
        {
            rects.insert(hwnd, *rect);
        }
    }

    (rects, fullscreen)
}

/// Single entry point for "メイン画面を空にする" shared by the tray menu, the
/// Quick Switcher, and the Workset Manager: bring the manager to the front and
/// trigger its empty-main modal (which runs the one real handler in
/// `wire_workset_manager`).
fn open_empty_main_screen(workset_manager: &WorksetManager) {
    let _ = workset_manager.show();
    popup_window::restore_and_foreground(workset_manager.window());
    workset_manager.invoke_empty_main_screen_requested();
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
    switcher.on_filter_changed(move |_text| {
        let Some(switcher) = s.upgrade() else { return };
        // `filter-text` is two-way bound to the TextInput, so it already holds
        // the new value (including IME-composed text); just re-filter.
        refresh_quick_switcher_rows(&switcher, &c.borrow(), &d);
    });

    let s = switcher.as_weak();
    let c = config.clone();
    let d = data_dir.clone();
    let coord = coordinator.clone();
    switcher.on_switch_requested(move |workset_id| {
        let Some(switcher) = s.upgrade() else { return };
        // A direct switch (Enter/click/number, or a hold-to-cycle commit)
        // ends any hold session; a stale release timer must not re-fire.
        cancel_cycle_timer();
        let Ok(target_workset_id) = uuid::Uuid::parse_str(&workset_id) else {
            return;
        };

        // Pre-launch snapshot, then relaunch any closed windows of the target
        // (switch = alive windows move, dead windows get launched).
        let live_windows =
            enumerate::enumerate_top_level_windows(std::process::id()).unwrap_or_default();
        let pending_acquire = {
            let config = c.borrow();
            let runtime = runtime_store::load(&d);
            launch_missing_for_switch(
                &config,
                &runtime.window_bindings,
                &live_windows,
                target_workset_id,
            )
        };

        let close_after_switch = {
            let config = c.borrow();
            let live_monitors = monitor::enumerate_monitors().unwrap_or_default();

            match coord.switch_to(SwitchRequest {
                worksets: &config.worksets,
                fixed_slots: &config.fixed_slots,
                sub_screens: &config.sub_screens,
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
        // A switch can change branch/uncommitted state (e.g. worktrees) — refresh
        // git in the background so the next open shows current values.
        spawn_git_refresh(workset_repo_paths(&c.borrow()));
        if close_after_switch {
            let _ = switcher.hide();
        }

        // If we relaunched closed windows, acquire and place them once they've
        // had time to appear.
        if let Some(pending) = pending_acquire {
            let c2 = c.clone();
            let d2 = d.clone();
            let coord2 = coord.clone();
            let pending = RefCell::new(Some(pending));
            let timer = slint::Timer::default();
            timer.start(
                slint::TimerMode::SingleShot,
                Duration::from_millis(2500),
                move || {
                    if let Some(p) = pending.borrow_mut().take() {
                        acquire_launched_and_place(p, &c2, &d2, &coord2);
                    }
                },
            );
            REOPEN_TIMER.with(|t| *t.borrow_mut() = Some(timer));
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
        cancel_cycle_timer();
        if let Some(switcher) = s.upgrade() {
            let _ = switcher.hide();
        }
    });
}

/// Renders a hotkey as the settings window's read-only display string
/// (e.g. `"Ctrl+Alt+W"`, `"Ctrl+Alt+Up"`), in canonical Ctrl/Alt/Shift/Win
/// order regardless of the stored `modifiers` order.
/// Full combo label for a cycle key, e.g. `"Ctrl+Alt+Down"` — the cycle key
/// combined with the main hotkey's modifiers (which must be held for it).
fn cycle_combo_label(main: &HotkeyConfig, cycle_vk: u32) -> String {
    if win32_hotkey::virtual_key_to_label(cycle_vk).is_some() {
        let combo = HotkeyConfig {
            modifiers: main.modifiers.clone(),
            virtual_key: cycle_vk,
        };
        hotkey_display_label(&combo)
    } else {
        format!("0x{cycle_vk:02X}")
    }
}

fn hotkey_display_label(hotkey: &HotkeyConfig) -> String {
    let mut parts: Vec<String> = [
        (HotkeyModifier::Control, "Ctrl"),
        (HotkeyModifier::Alt, "Alt"),
        (HotkeyModifier::Shift, "Shift"),
        (HotkeyModifier::Win, "Win"),
    ]
    .iter()
    .filter(|(modifier, _)| hotkey.modifiers.contains(modifier))
    .map(|(_, label)| (*label).to_string())
    .collect();
    parts.push(
        win32_hotkey::virtual_key_to_label(hotkey.virtual_key)
            .unwrap_or_else(|| format!("VK 0x{:02X}", hotkey.virtual_key)),
    );
    parts.join("+")
}

/// Populates the hotkey display and wires the press-to-record capture flow
/// (PLAN.md §13 Phase 7 checklist item 4, "ホットキー設定UI"): the Slint
/// side captures the next non-modifier key press plus whatever modifiers are
/// held, and `on_hotkey_captured` feeds it through the same mutate →
/// `validate()` → `config_store::save()` → rollback-on-failure template used
/// by every other config-saving handler in this file.
fn wire_settings(
    window: &AppWindow,
    data_dir: PathBuf,
    config: Rc<RefCell<AppConfig>>,
    hotkey_thread: Rc<HotkeyThread>,
) {
    window.set_hotkey_display(
        hotkey_display_label(&config.borrow().settings.quick_switcher_hotkey).into(),
    );
    {
        let cfg = config.borrow();
        let main = &cfg.settings.quick_switcher_hotkey;
        window.set_cycle_next_display(cycle_combo_label(main, cfg.settings.cycle_next_key).into());
        window.set_cycle_prev_display(cycle_combo_label(main, cfg.settings.cycle_prev_key).into());
    }

    {
        let w = window.as_weak();
        window.on_hotkey_capture_started(move || {
            let Some(window) = w.upgrade() else { return };
            window.set_hotkey_status_text("".into());
            window.set_hotkey_status_is_warning(false);
        });
    }

    {
        let w = window.as_weak();
        window.on_cycle_capture_started(move |_which| {
            let Some(window) = w.upgrade() else { return };
            window.set_hotkey_status_text("".into());
            window.set_hotkey_status_is_warning(false);
        });
    }

    // Capturing a cycle key: only the key matters (its modifiers are inherited
    // from the main hotkey), so map the pressed key, persist, and rebind.
    {
        let w = window.as_weak();
        let c = config.clone();
        let d = data_dir.clone();
        let ht = hotkey_thread.clone();
        window.on_cycle_captured(move |which, key_text| {
            let Some(window) = w.upgrade() else { return };
            let Some(vk) = win32_hotkey::slint_key_text_to_virtual_key(&key_text) else {
                window.set_hotkey_status_text(
                    "このキーは登録できません。別のキーを押してください。".into(),
                );
                window.set_hotkey_status_is_warning(true);
                return;
            };

            let (spec, cn, cp) = {
                let mut cfg = c.borrow_mut();
                if which == 1 {
                    cfg.settings.cycle_next_key = vk;
                } else {
                    cfg.settings.cycle_prev_key = vk;
                }
                (
                    cfg.settings.quick_switcher_hotkey.clone(),
                    cfg.settings.cycle_next_key,
                    cfg.settings.cycle_prev_key,
                )
            };

            match config_store::save(&d, &c.borrow()) {
                Ok(()) => {
                    ht.rebind(spec.clone(), cn, cp);
                    window.set_cycle_next_display(cycle_combo_label(&spec, cn).into());
                    window.set_cycle_prev_display(cycle_combo_label(&spec, cp).into());
                    window.set_cycle_capturing(0);
                    window.set_hotkey_status_text(
                        format!(
                            "次のセット={} / 前のセット={} を保存しました。",
                            cycle_combo_label(&spec, cn),
                            cycle_combo_label(&spec, cp)
                        )
                        .into(),
                    );
                    window.set_hotkey_status_is_warning(false);
                }
                Err(err) => {
                    window
                        .set_hotkey_status_text(format!("設定の保存に失敗しました: {err}").into());
                    window.set_hotkey_status_is_warning(true);
                }
            }
        });
    }

    let w = window.as_weak();
    let c = config.clone();
    let d = data_dir.clone();
    let ht = hotkey_thread;
    window.on_hotkey_captured(move |ctrl, alt, shift, meta, key_text| {
        let Some(window) = w.upgrade() else { return };

        // Rejections below deliberately leave `hotkey-capturing` true so the
        // user can immediately press another combination.
        let Some(virtual_key) = win32_hotkey::slint_key_text_to_virtual_key(&key_text) else {
            window.set_hotkey_status_text(
                "このキーは登録できません。別のキーを押してください。".into(),
            );
            window.set_hotkey_status_is_warning(true);
            return;
        };

        let mut modifiers = Vec::new();
        if ctrl {
            modifiers.push(HotkeyModifier::Control);
        }
        if alt {
            modifiers.push(HotkeyModifier::Alt);
        }
        if shift {
            modifiers.push(HotkeyModifier::Shift);
        }
        if meta {
            modifiers.push(HotkeyModifier::Win);
        }

        let candidate = HotkeyConfig {
            modifiers,
            virtual_key,
        };
        if candidate.modifiers.is_empty() && !candidate.allows_empty_modifiers() {
            window.set_hotkey_status_text(
                "修飾キーと組み合わせてください（ファンクションキーのみ単独登録可）".into(),
            );
            window.set_hotkey_status_is_warning(true);
            return;
        }

        window.set_hotkey_capturing(false);
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
                window.set_hotkey_display(hotkey_display_label(&candidate).into());
                // The cycle combos inherit the main modifiers, so re-render them.
                window.set_cycle_next_display(
                    cycle_combo_label(&candidate, cfg.settings.cycle_next_key).into(),
                );
                window.set_cycle_prev_display(
                    cycle_combo_label(&candidate, cfg.settings.cycle_prev_key).into(),
                );
                window.set_hotkey_status_text("保存しました。反映を確認しています…".into());
                window.set_hotkey_status_is_warning(false);
                ht.rebind(
                    candidate,
                    cfg.settings.cycle_next_key,
                    cfg.settings.cycle_prev_key,
                );
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
    /// A hold-to-cycle arrow was pressed (main modifiers + Down/Up).
    Cycle {
        forward: bool,
    },
    /// A Ctrl+Shift+MouseWheel tick — drives the switcher like `Cycle` but the
    /// release-to-commit watches Ctrl+Shift rather than the configured hotkey.
    WheelCycle {
        forward: bool,
    },
    Registered,
    RegisterFailed(String),
}

thread_local! {
    /// The repeating timer that watches for the cycle modifiers being
    /// released (Alt+Tab-style commit). Held in an `Rc<RefCell<..>>` so the
    /// timer's own callback can drop it — the standard Slint self-stopping
    /// timer idiom. `None` whenever no hold-to-cycle session is in flight.
    static CYCLE_TIMER: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));

    /// The virtual-key codes whose release commits the in-flight cycle session.
    /// Set per gesture — the configured hotkey's modifiers for the keyboard
    /// cycle, or Ctrl+Shift for the mouse-wheel gesture — so one release timer
    /// serves both.
    static CYCLE_WATCH_VKS: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
}

/// The Win32 virtual-key codes to poll for each configured hotkey modifier.
/// The Win key contributes both `VK_LWIN` and `VK_RWIN` (either counts as
/// held), so "all released" means every code here reads up.
fn hotkey_modifier_vks(hotkey: &HotkeyConfig) -> Vec<i32> {
    let mut vks = Vec::new();
    for modifier in &hotkey.modifiers {
        match modifier {
            HotkeyModifier::Control => vks.push(0x11), // VK_CONTROL
            HotkeyModifier::Alt => vks.push(0x12),     // VK_MENU
            HotkeyModifier::Shift => vks.push(0x10),   // VK_SHIFT
            HotkeyModifier::Win => {
                vks.push(0x5B); // VK_LWIN
                vks.push(0x5C); // VK_RWIN
            }
        }
    }
    vks
}

fn key_is_down(vk: i32) -> bool {
    // SAFETY: `GetAsyncKeyState` is always safe to call; the high bit of the
    // returned SHORT means the key is currently down.
    (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
}

/// True once every watched modifier of the in-flight cycle session has been
/// released — the moment it commits. The watched set is whatever the gesture
/// stored in `CYCLE_WATCH_VKS` (hotkey modifiers, or Ctrl+Shift for the wheel).
fn cycle_watch_released() -> bool {
    CYCLE_WATCH_VKS.with(|vks| {
        let vks = vks.borrow();
        // An empty set can't be "held"; treat as never-released so no phantom
        // commit fires.
        !vks.is_empty() && vks.iter().all(|&vk| !key_is_down(vk))
    })
}

/// Moves the Quick Switcher selection one row, wrapping at either end. The slot
/// one past the last row (index == row_count) is the "empty the main screen"
/// action, so the cycle includes it.
fn cycle_move_selection(switcher: &QuickSwitcher, forward: bool) {
    let len = switcher.get_rows().row_count() + 1;
    let current = usize::try_from(switcher.get_selected_index().max(0)).unwrap_or(0) % len;
    let next = if forward {
        (current + 1) % len
    } else {
        (current + len - 1) % len
    };
    switcher.set_selected_index(i32::try_from(next).unwrap_or(0));
}

/// Cancels any in-flight hold-to-cycle release timer (e.g. the user pressed
/// Escape, clicked a row, or the popup lost focus before releasing).
fn cancel_cycle_timer() {
    CYCLE_TIMER.with(|holder| holder.borrow_mut().take());
}

/// Switches to whatever row the hold-to-cycle session left selected, then
/// closes the switcher — the Alt+Tab-style "release to commit" action.
fn commit_cycle_selection(ctx: &CrossThreadUiContext) {
    if let Some(switcher) = ctx.quick_switcher.upgrade() {
        let rows = switcher.get_rows();
        let idx = switcher.get_selected_index();
        let row_count = rows.row_count();
        if idx >= 0 && (idx as usize) >= row_count {
            // The slot past the last row is the "empty the main screen" action.
            switcher.invoke_empty_main_requested();
        } else if idx >= 0
            && let Some(row) = rows.row_data(idx as usize)
        {
            // Reuses the fully-wired switch path (journal, rollback, hide).
            switcher.invoke_switch_requested(row.workset_id);
        }
        let _ = switcher.hide();
    }
}

/// Starts the release-watching timer for a hold-to-cycle session, watching
/// `watch_vks` for release. Updates the watched set even if a timer is already
/// running (the latest gesture defines the commit condition).
fn start_cycle_release_timer(watch_vks: Vec<i32>) {
    CYCLE_WATCH_VKS.with(|c| *c.borrow_mut() = watch_vks);
    if CYCLE_TIMER.with(|holder| holder.borrow().is_some()) {
        return;
    }
    let holder = CYCLE_TIMER.with(std::clone::Clone::clone);
    let holder_for_cb = holder.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(40),
        move || {
            let Some(ctx) = UI_CONTEXT.with(|cell| cell.borrow().clone()) else {
                return;
            };
            if cycle_watch_released() {
                // Drop the timer first so it can't re-fire while the switch runs.
                holder_for_cb.borrow_mut().take();
                commit_cycle_selection(&ctx);
            }
        },
    );
    *holder.borrow_mut() = Some(timer);
}

/// Shared body of the keyboard-cycle and wheel-cycle gestures: shows the
/// switcher (preselecting the current workset on first open), moves the
/// selection, and arms the release timer for `watch_vks`.
fn drive_cycle(ctx: &CrossThreadUiContext, forward: bool, watch_vks: Vec<i32>) {
    if let Some(switcher) = ctx.quick_switcher.upgrade() {
        if !switcher.window().is_visible() {
            show_quick_switcher_at_cursor(&switcher, &ctx.config.borrow(), &ctx.data_dir);
            // Start from the current workset so the first tap lands on its
            // neighbour, exactly like Alt+Tab starting on the next window.
            let state = runtime_store::load(&ctx.data_dir);
            if let Some(id) = state.current_workset_id {
                select_row_for_workset(&switcher, id);
            }
        }
        cycle_move_selection(&switcher, forward);
        start_cycle_release_timer(watch_vks);
    }
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
        HotkeyUiEvent::Cycle { forward } => {
            let watch = hotkey_modifier_vks(&ctx.config.borrow().settings.quick_switcher_hotkey);
            drive_cycle(&ctx, forward, watch);
        }
        HotkeyUiEvent::WheelCycle { forward } => {
            // Ctrl+Shift held during the wheel gesture — release either to commit.
            drive_cycle(&ctx, forward, vec![0x11, 0x10]); // VK_CONTROL, VK_SHIFT
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
                // Re-render the display from the (possibly just rolled-back)
                // config so it never keeps showing a combo that failed to
                // register.
                settings.set_hotkey_display(
                    hotkey_display_label(&ctx.config.borrow().settings.quick_switcher_hotkey)
                        .into(),
                );
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

/// Per-repository git status, refreshed in the background (`spawn_git_refresh`)
/// and read when building Quick Switcher rows. A process-wide cache rather than
/// `Rc` state so the background thread can write it; the UI thread reads it.
static GIT_CACHE: std::sync::LazyLock<
    std::sync::Mutex<HashMap<PathBuf, crate::domain::git::GitStatus>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// The cached git status for `path`, or the empty default until the first
/// background refresh has populated it.
fn cached_git_status(path: &Path) -> crate::domain::git::GitStatus {
    GIT_CACHE
        .lock()
        .ok()
        .and_then(|cache| cache.get(path).cloned())
        .unwrap_or_default()
}

/// Fetches each repository's git status on a background thread (so `git` never
/// blocks the switcher from opening), stores it in `GIT_CACHE`, then re-renders
/// the Quick Switcher on the UI thread. Called when the switcher opens and after
/// a switch — the moments its branch / change-count / last-commit can differ.
fn spawn_git_refresh(paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for path in paths {
            let status = crate::application::git_status_service::fetch(&path);
            if let Ok(mut cache) = GIT_CACHE.lock() {
                cache.insert(path, status);
            }
        }
        let _ = slint::invoke_from_event_loop(refresh_quick_switcher_from_context);
    });
}

/// The distinct repository paths of all worksets — the work list for
/// `spawn_git_refresh`.
fn workset_repo_paths(config: &AppConfig) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = config
        .worksets
        .iter()
        .map(|w| w.repository_path.clone())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Re-renders the Quick Switcher's rows from the UI-thread context (used as the
/// background git refresh's completion callback).
fn refresh_quick_switcher_from_context() {
    UI_CONTEXT.with(|cell| {
        if let Some(ctx) = &*cell.borrow()
            && let Some(switcher) = ctx.quick_switcher.upgrade()
        {
            refresh_quick_switcher_rows(&switcher, &ctx.config.borrow(), &ctx.data_dir);
        }
    });
}

/// How long to let the display topology settle before re-checking whether a
/// software re-detect actually brought the missing monitors back (PLAN.md §4.6).
/// The retry cadence uses a [`slint::Timer`] so the UI thread is never blocked.
const RECOVERY_SETTLE_DELAY: Duration = Duration::from_secs(3);

/// Snapshots the live monitors' `device_name`s — the plain-data input the pure
/// [`display_recovery_service`] compares against the saved topology.
fn live_device_names() -> Vec<String> {
    monitor::enumerate_monitors()
        .map(|monitors| monitors.into_iter().map(|m| m.device_name).collect())
        .unwrap_or_default()
}

/// Runs the [`display_reset`] side effect once and logs its outcome. Shared by the
/// manual tray trigger ("モニターを再検出") and the automatic resume path.
fn run_display_reset(reason: &str) {
    match display_reset::reapply_display_topology() {
        Ok(outcome) => {
            tracing::info!(?outcome, reason, "display re-detect completed");
        }
        Err(err) => {
            tracing::warn!(error = %err, reason, "display re-detect failed");
        }
    }
}

/// One automatic-recovery cycle: read the saved topology and the setting, ask the
/// pure [`display_recovery_service::decide_recovery`] what to do, and act on it.
///
/// On [`RecoveryDecision::Recover`] it fires the side effect and schedules a
/// re-check after [`RECOVERY_SETTLE_DELAY`] via `timer`, forming a bounded retry
/// loop (the pure decision function enforces the retry budget, so this can never
/// spin forever). All other decisions just log and stop.
fn attempt_auto_recovery(
    config: &Rc<RefCell<AppConfig>>,
    attempts: &Rc<Cell<u32>>,
    timer: &Rc<Timer>,
) {
    let (saved_monitors, enabled) = {
        let config = config.borrow();
        (
            config.monitors.clone(),
            config.settings.auto_display_recovery,
        )
    };
    let live = live_device_names();

    match display_recovery_service::decide_recovery(&saved_monitors, &live, attempts.get(), enabled)
    {
        RecoveryDecision::Recover { missing, attempt } => {
            tracing::warn!(
                ?missing,
                attempt,
                "resume: saved monitors missing from the live topology; attempting software re-detect"
            );
            attempts.set(attempt);
            run_display_reset("resume auto-recovery");

            // Re-check after the display settles; if monitors are still missing and
            // budget remains, this reschedules, otherwise the next decision stops it.
            let config = config.clone();
            let attempts = attempts.clone();
            let timer_for_retry = timer.clone();
            timer.start(TimerMode::SingleShot, RECOVERY_SETTLE_DELAY, move || {
                attempt_auto_recovery(&config, &attempts, &timer_for_retry);
            });
        }
        RecoveryDecision::GiveUp { missing } => {
            tracing::warn!(
                ?missing,
                attempts = attempts.get(),
                "resume: retry budget exhausted; giving up on display re-detect until next resume"
            );
        }
        RecoveryDecision::UpToDate => {
            tracing::info!(
                "resume: live display topology already matches saved monitors; no recovery needed"
            );
        }
        RecoveryDecision::Disabled => {
            tracing::debug!("resume: automatic display recovery is disabled by the user setting");
        }
        RecoveryDecision::NoSavedTopology => {
            tracing::debug!("resume: no saved monitors yet; skipping display recovery");
        }
    }
}

thread_local! {
    /// Per-resume retry counter and the settle-delay timer for automatic display
    /// recovery. UI-thread-only (the suspend/resume callback marshals here before
    /// touching them), so plain `thread_local` `Rc`s need no synchronization.
    static RECOVERY_ATTEMPTS: Rc<Cell<u32>> = Rc::new(Cell::new(0));
    static RECOVERY_TIMER: Rc<Timer> = Rc::new(Timer::default());
}

/// UI-thread handler for a resume-from-sleep event (marshaled from the
/// suspend/resume callback). Resets the retry budget and kicks off one recovery
/// cycle against the current saved topology.
fn handle_resume_ui_event() {
    let Some(ctx) = UI_CONTEXT.with(|cell| cell.borrow().clone()) else {
        return;
    };
    tracing::info!("system resume detected; evaluating display recovery");
    let attempts = RECOVERY_ATTEMPTS.with(std::clone::Clone::clone);
    let timer = RECOVERY_TIMER.with(std::clone::Clone::clone);
    // Each resume starts a fresh retry budget.
    attempts.set(0);
    attempt_auto_recovery(&ctx.config, &attempts, &timer);
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
        // Discard session HWND bindings from a previous OS boot: after a reboot
        // those HWND numbers are reassigned and would point at unrelated
        // windows (PLAN.md §5.4 extension). Bindings survive a RepoDeck-only
        // restart (same boot session), so switching still re-finds windows.
        if !crate::windowing::session::is_same_session(runtime.window_binding_session) {
            runtime.window_bindings.clear();
            runtime.window_binding_session = Some(crate::windowing::session::boot_session_token());
        }
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
            tracing::debug!(target: "monitors", "WM_DISPLAYCHANGE with unchanged topology (e.g. DPI-only); ignoring");
            return; // e.g. a DPI-only change also fires WM_DISPLAYCHANGE
        }

        // A real topology change — including a Remote Desktop connect/disconnect,
        // which swaps the physical monitors for the RDP virtual display and back.
        // Log the before/after so a "windows went weird over RDP" report can be
        // traced to exactly which monitors appeared/disappeared and which windows
        // it stranded.
        tracing::info!(
            target: "monitors",
            previous = runtime.last_seen_monitor_fingerprint.as_deref().unwrap_or("(none)"),
            live = ?live_monitors
                .iter()
                .map(|m| (m.device_name.clone(), m.bounds_px.x, m.bounds_px.y, m.bounds_px.width, m.bounds_px.height))
                .collect::<Vec<_>>(),
            "monitor topology changed (WM_DISPLAYCHANGE)"
        );

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
                target: "monitors",
                count = offscreen.len(),
                "minimizing windows left offscreen by a monitor configuration change"
            );
        }
        for hwnd in &offscreen {
            let rect = live_windows
                .iter()
                .find(|w| w.hwnd == *hwnd)
                .map(|w| (w.rect_px.x, w.rect_px.y, w.rect_px.width, w.rect_px.height));
            let title = live_windows
                .iter()
                .find(|w| w.hwnd == *hwnd)
                .map(|w| w.title.clone())
                .unwrap_or_default();
            tracing::info!(target: "monitors", hwnd, ?rect, %title, "minimizing offscreen window");
            Win32WindowOps.minimize(*hwnd);
        }

        runtime.last_seen_monitor_fingerprint = Some(fingerprint);
        let _ = runtime_store::save(&data_dir_for_display, &runtime);
    });

    // Automatic display recovery: register a window-free suspend/resume callback
    // (see `power_watch` for why a window subclass can't work at startup) and, on
    // each resume, run a bounded, self-healing software re-detect if (and only if)
    // saved monitors have gone missing (PLAN.md §4.6, Phase 9). The callback fires
    // on a system thread, so it only marshals onto the UI thread; all the real
    // work — and all the "should we?" logic in the pure `display_recovery_service`
    // — happens there via `handle_resume_ui_event`.
    let _power_watch = power_watch::watch_power_resume(|| {
        let _ = slint::invoke_from_event_loop(handle_resume_ui_event);
    });
    if _power_watch.is_none() {
        tracing::warn!(
            "could not register the suspend/resume notification; automatic display recovery on resume is disabled"
        );
    }

    let tray = TrayIcon::new().context("failed to create the RepoDeck tray icon")?;
    let layout_studio = LayoutStudio::new().context("failed to create the Layout Studio window")?;
    apply_glass_backdrop(layout_studio.window());
    let workset_manager =
        WorksetManager::new().context("failed to create the Workset Manager window")?;
    apply_glass_backdrop(workset_manager.window());
    let quick_switcher =
        QuickSwitcher::new().context("failed to create the Quick Switcher window")?;
    apply_glass_backdrop(quick_switcher.window());
    quick_switcher.set_translucent(transparency_effects_enabled());
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
        coordinator.clone(),
    );

    wire_quick_switcher(
        &quick_switcher,
        data_dir.clone(),
        config.clone(),
        coordinator.clone(),
        window.as_weak(),
    );

    // "メイン画面を空にする" from the Quick Switcher: hide it, open the Workset
    // Manager and trigger its empty-main modal.
    let workset_manager_for_qs_empty = workset_manager.as_weak();
    let switcher_for_qs_empty = quick_switcher.as_weak();
    quick_switcher.on_empty_main_requested(move || {
        if let Some(switcher) = switcher_for_qs_empty.upgrade() {
            let _ = switcher.hide();
        }
        if let Some(workset_manager) = workset_manager_for_qs_empty.upgrade() {
            open_empty_main_screen(&workset_manager);
        }
    });

    // "セット管理" from the Quick Switcher: hide it and bring the Workset Manager
    // to the front (the standard place to register a set and launch its apps).
    let workset_manager_for_qs_manage = workset_manager.as_weak();
    let switcher_for_qs_manage = quick_switcher.as_weak();
    quick_switcher.on_manager_requested(move || {
        if let Some(switcher) = switcher_for_qs_manage.upgrade() {
            let _ = switcher.hide();
        }
        if let Some(workset_manager) = workset_manager_for_qs_manage.upgrade() {
            let _ = workset_manager.show();
            popup_window::restore_and_foreground(workset_manager.window());
        }
    });

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
        config.borrow().settings.cycle_next_key,
        config.borrow().settings.cycle_prev_key,
        move |event| {
            let ui_event = match event {
                HotkeyEvent::Pressed => HotkeyUiEvent::Pressed,
                HotkeyEvent::CyclePressed { forward } => HotkeyUiEvent::Cycle { forward },
                HotkeyEvent::Registered => HotkeyUiEvent::Registered,
                HotkeyEvent::RegisterFailed(err) => {
                    HotkeyUiEvent::RegisterFailed(hotkey_register_error_message(err))
                }
            };
            let _ = slint::invoke_from_event_loop(move || handle_hotkey_ui_event(ui_event));
        },
    ));

    // Ctrl+Shift+MouseWheel opens and drives the Quick Switcher system-wide.
    // Kept alive for the process lifetime; its `Drop` unhooks and stops the
    // thread. Each wheel tick marshals onto the UI thread like the hotkey.
    let _mouse_wheel_hook = MouseWheelHook::spawn(move |forward| {
        let _ = slint::invoke_from_event_loop(move || {
            handle_hotkey_ui_event(HotkeyUiEvent::WheelCycle { forward });
        });
    });

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
            // Focus loss also aborts a hold-to-cycle session mid-flight.
            cancel_cycle_timer();
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
            popup_window::restore_and_foreground(layout_studio.window());
        }
    });

    let workset_manager_for_register = workset_manager.as_weak();
    tray.on_register_workset_requested(move || {
        if let Some(workset_manager) = workset_manager_for_register.upgrade() {
            let _ = workset_manager.show();
            popup_window::restore_and_foreground(workset_manager.window());
        }
    });

    // "メイン画面を空にする" now lives in the Workset Manager's registration
    // flow: open it, start registration, and trigger the empty-main modal.
    let workset_manager_for_empty = workset_manager.as_weak();
    tray.on_empty_main_screen_requested(move || {
        if let Some(workset_manager) = workset_manager_for_empty.upgrade() {
            open_empty_main_screen(&workset_manager);
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

    // Manual trigger: run the software display re-detect unconditionally, exactly as
    // the user asked (no gating on the saved-vs-live comparison).
    tray.on_reconnect_monitors_requested(|| {
        tracing::info!("manual monitor re-detect requested from the tray menu");
        run_display_reset("manual tray trigger");
    });

    let window_for_settings = window.as_weak();
    let config_for_settings = config.clone();
    // Installed on first show: the Settings window's HWND only exists once
    // shown, and suppressing Alt menu-mode there lets Alt-based hotkeys be
    // captured (see `popup_window::suppress_system_menu_key`).
    let sysmenu_suppression: RefCell<Option<popup_window::SysMenuSuppression>> = RefCell::new(None);
    tray.on_settings_requested(move || {
        if let Some(window) = window_for_settings.upgrade() {
            refresh_codex_settings_state(&window, &config_for_settings.borrow());
            let _ = window.show();
            popup_window::restore_and_foreground(window.window());
            if sysmenu_suppression.borrow().is_none() {
                *sysmenu_suppression.borrow_mut() =
                    popup_window::suppress_system_menu_key(window.window());
            }
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
            popup_window::restore_and_foreground(layout_studio.window());
        }
    });
    let workset_manager_for_main_window = workset_manager.as_weak();
    window.on_open_workset_manager_requested(move || {
        if let Some(workset_manager) = workset_manager_for_main_window.upgrade() {
            let _ = workset_manager.show();
            popup_window::restore_and_foreground(workset_manager.window());
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
