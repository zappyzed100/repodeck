//! Global low-level mouse hook detecting Ctrl+Shift+MouseWheel system-wide.
//!
//! Win32's `WH_MOUSE_LL` hook, like `RegisterHotKey`, requires the installing
//! thread to run a message loop — the OS pumps the hook callback only while
//! that thread services its queue via `GetMessage`/`DispatchMessage`. So this
//! mirrors [`win32_hotkey`](super::win32_hotkey): a dedicated OS thread that
//! installs the hook, runs a blocking `GetMessageW` loop, and unhooks on exit.
//! The hook is torn down by posting a shutdown thread-message from `Drop` and
//! joining the thread.
//!
//! Unlike `RegisterHotKey`, a low-level mouse hook sees *every* wheel event on
//! the system and can swallow it; this lets Ctrl+Shift+Wheel be repurposed
//! (returning a non-zero `LRESULT`) without the underlying window also
//! scrolling. Normal wheel scrolling is untouched — the callback fires and the
//! event is swallowed only when both Ctrl and Shift are physically held.
//!
//! This module has no Slint dependency; callers needing to update UI from
//! `on_event` must marshal via `slint::invoke_from_event_loop` themselves
//! (the same convention `win32_hotkey` documents).

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, HHOOK, MSG, MSLLHOOKSTRUCT,
    PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_MOUSE_LL,
    WM_APP, WM_MOUSEWHEEL,
};

/// Posted from [`MouseWheelHook::drop`] to break the hook thread's message
/// loop (mirrors `win32_hotkey`'s `WM_APP_SHUTDOWN` convention).
const WM_APP_SHUTDOWN: u32 = WM_APP + 2;

/// High bit of a `GetAsyncKeyState` return value: set while the key is down.
const KEY_DOWN_MASK: u16 = 0x8000;

/// The user-supplied callback, invoked on the hook thread for each
/// Ctrl+Shift+wheel tick. A plain `extern "system" fn` hook proc can't capture
/// environment, so the closure lives here. This is a `OnceLock` because only a
/// single [`MouseWheelHook`] is expected to exist at a time (the app spawns
/// one); a second `spawn` would silently keep the first callback. That
/// single-instance assumption is the module's contract.
static CALLBACK: OnceLock<Box<dyn Fn(bool) + Send + Sync>> = OnceLock::new();

thread_local! {
    /// The installed hook handle, kept on the hook thread so its message loop
    /// can `UnhookWindowsHookEx` cleanly on shutdown. Only ever touched by the
    /// one thread that installs the hook, hence `thread_local` rather than a
    /// shared static.
    static HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
}

/// A global Ctrl+Shift+MouseWheel detector backed by a dedicated Win32
/// message-loop thread. Dropping it tears the hook down and joins the thread.
pub struct MouseWheelHook {
    thread_id: u32,
    join_handle: Option<JoinHandle<()>>,
}

impl MouseWheelHook {
    /// Spawns the hook thread and installs the low-level mouse hook.
    ///
    /// `on_event(forward)` is invoked on the hook thread for each
    /// Ctrl+Shift+wheel tick: `forward == true` means the wheel scrolled
    /// DOWN / toward the user (advance / next), `false` means UP (previous).
    /// The originating wheel event is swallowed so the window under the cursor
    /// does not also scroll.
    ///
    /// Only one instance should exist at a time (see [`CALLBACK`]).
    pub fn spawn(on_event: impl Fn(bool) + Send + Sync + 'static) -> Self {
        // Store the callback before installing the hook, so the very first
        // wheel event the hook proc sees can already reach it. `set` fails only
        // if a previous `spawn` already populated it; the first callback wins.
        let _ = CALLBACK.set(Box::new(on_event));

        let (thread_id_tx, thread_id_rx) = mpsc::sync_channel(1);

        let join_handle = std::thread::spawn(move || {
            // SAFETY: called once, at the very start of this thread's life.
            let thread_id = unsafe { GetCurrentThreadId() };
            let _ = thread_id_tx.send(thread_id);

            if install_hook() {
                run_message_loop();
                uninstall_hook();
            }
        });

        let thread_id = thread_id_rx
            .recv()
            .expect("the mouse-wheel hook thread must report its thread id before spawn() returns");

        Self {
            thread_id,
            join_handle: Some(join_handle),
        }
    }
}

impl Drop for MouseWheelHook {
    fn drop(&mut self) {
        // SAFETY: `self.thread_id` is this struct's own live hook thread; the
        // message carries no payload.
        let _ =
            unsafe { PostThreadMessageW(self.thread_id, WM_APP_SHUTDOWN, WPARAM(0), LPARAM(0)) };
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Installs the `WH_MOUSE_LL` hook on the current (hook) thread and stashes the
/// handle in [`HOOK`]. Returns whether installation succeeded; on failure the
/// thread has nothing to pump and simply exits.
fn install_hook() -> bool {
    // SAFETY: `GetModuleHandleW(None)` returns this process's own module
    // handle, which `SetWindowsHookExW` accepts for a global low-level hook
    // whose proc lives in this module. A low-level hook takes thread id 0
    // (system-wide). Both calls are FFI with valid arguments.
    let result = unsafe {
        let hmod = match GetModuleHandleW(None) {
            Ok(hmod) => hmod,
            Err(_) => return false,
        };
        SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), Some(HINSTANCE(hmod.0)), 0)
    };

    match result {
        Ok(hhook) => {
            HOOK.with(|cell| cell.set(Some(hhook)));
            true
        }
        Err(_) => false,
    }
}

/// Removes the hook installed by [`install_hook`], if any. Runs on the hook
/// thread as its message loop exits.
fn uninstall_hook() {
    if let Some(hhook) = HOOK.with(|cell| cell.take()) {
        // SAFETY: `hhook` was returned by this thread's `SetWindowsHookExW` and
        // has not been unhooked yet (`take` clears it so this runs at most
        // once).
        let _ = unsafe { UnhookWindowsHookEx(hhook) };
    }
}

/// The dedicated hook thread's blocking message loop. `WH_MOUSE_LL` callbacks
/// fire only while this loop pumps the thread's queue, so it must run for the
/// hook to work at all. Exits on `WM_QUIT` or the [`WM_APP_SHUTDOWN`] posted
/// by [`MouseWheelHook::drop`].
fn run_message_loop() {
    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a valid buffer; `None`/`0`/`0` requests every
        // message posted to this thread's queue (the low-level hook has no
        // associated window).
        if !unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            break; // WM_QUIT or GetMessageW failed.
        }

        if msg.message == WM_APP_SHUTDOWN {
            break;
        }

        // SAFETY: `msg` was just filled in by `GetMessageW` above.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Whether `vk` is physically held down right now.
fn is_key_down(vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY) -> bool {
    // SAFETY: `GetAsyncKeyState` takes a virtual-key code and has no
    // preconditions; the high bit of its return marks the key as down.
    (unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & KEY_DOWN_MASK) != 0
}

/// The `WH_MOUSE_LL` hook procedure. Called by the OS on the hook thread for
/// every low-level mouse event while the message loop runs.
///
/// For a wheel event with both Ctrl and Shift physically down, it invokes the
/// stored callback and returns `LRESULT(1)` to swallow the event (so the
/// window under the cursor does not scroll). Every other event — including
/// plain wheel scrolling — is passed on unchanged via `CallNextHookEx`.
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && wparam.0 as u32 == WM_MOUSEWHEEL {
        // SAFETY: for `WM_MOUSEWHEEL` the OS guarantees `lparam` points at a
        // valid `MSLLHOOKSTRUCT` for the duration of this call.
        let msll = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
        // The wheel delta is the signed high word of `mouseData`.
        let delta = (msll.mouseData >> 16) as i16;

        if delta != 0 && is_key_down(VK_CONTROL) && is_key_down(VK_SHIFT) {
            // Wheel down (delta < 0) advances / goes to next.
            let forward = delta < 0;
            if let Some(callback) = CALLBACK.get() {
                callback(forward);
            }
            // Swallow the event so the underlying window does not scroll.
            return LRESULT(1);
        }
    }

    // SAFETY: forwarding to the next hook in the chain with the exact
    // parameters we were handed; `None` lets the OS resolve the chain.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
