use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, INFINITE, WaitForSingleObject,
};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::core::HSTRING;

use crate::diagnostics::logging;

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

    let window = AppWindow::new().context("failed to create the RepoDeck main window")?;
    let tray = TrayIcon::new().context("failed to create the RepoDeck tray icon")?;

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

    slint::run_event_loop().context("RepoDeck event loop failed")?;

    tracing::info!("RepoDeck exiting");
    Ok(())
}
