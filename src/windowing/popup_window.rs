//! Raw-HWND popup-window tweaks that Slint's public API doesn't expose:
//! taskbar/Alt+Tab exclusion and outside-click detection (PLAN.md §3.3,
//! §13 Phase 7). Same `raw_window_handle::HasWindowHandle` pattern
//! `apply_glass_backdrop` (`src/app.rs`) already uses to reach a live Slint
//! window's real HWND.

use std::cell::RefCell;
use std::collections::HashMap;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GWL_EXSTYLE, GWLP_WNDPROC, GetWindowLongPtrW, SetWindowLongPtrW, WA_INACTIVE,
    WM_ACTIVATE, WM_DISPLAYCHANGE, WNDPROC, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

/// Best-effort forces real OS input focus onto `window` (PLAN.md §4.5's
/// `SetForegroundWindow` policy). Needed for a `WS_EX_TOOLWINDOW` popup like
/// the Quick Switcher: showing it alone doesn't reliably grab keyboard focus
/// away from whatever the user was previously typing into.
pub fn force_foreground(window: &slint::Window) {
    if let Some(hwnd) = hwnd_of(window) {
        crate::windowing::placement::set_foreground_best_effort(hwnd);
    }
}

fn hwnd_of(window: &slint::Window) -> Option<HWND> {
    let window_handle = window.window_handle();
    let handle = HasWindowHandle::window_handle(&window_handle).ok()?;
    let RawWindowHandle::Win32(win32_handle) = handle.as_raw() else {
        return None;
    };
    Some(HWND(isize::from(win32_handle.hwnd) as *mut std::ffi::c_void))
}

/// Sets `WS_EX_TOOLWINDOW` and clears `WS_EX_APPWINDOW` via `GWL_EXSTYLE` so
/// the window never gets a taskbar button and is excluded from Alt+Tab
/// (PLAN.md §3.3: タスクバー・Alt+Tabに表示しない). Must be called before the
/// window's first `.show()` — Explorer's taskbar often needs a hide/show
/// cycle to notice a change on an already-visible window, a well-known Win32
/// quirk with these two style bits.
pub fn exclude_from_taskbar_and_alt_tab(window: &slint::Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    // SAFETY: `hwnd` came from a live Slint window's raw handle; `GWL_EXSTYLE`
    // is a plain 32-bit style value even though `GetWindowLongPtrW` returns
    // it widened to `isize`.
    unsafe {
        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let new_style = (current & !WS_EX_APPWINDOW.0) | WS_EX_TOOLWINDOW.0;
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style as isize);
    }
}

/// A subclassed window's original WNDPROC (to chain to) and its
/// deactivation callback.
type Subclass = (WNDPROC, Box<dyn Fn()>);

thread_local! {
    /// Keyed by raw HWND value rather than `GWLP_USERDATA` (which winit's own
    /// window adapter may already occupy).
    static SUBCLASSES: RefCell<HashMap<isize, Subclass>> = RefCell::new(HashMap::new());
}

fn wndproc_from_isize(raw: isize) -> WNDPROC {
    if raw == 0 {
        return None;
    }
    // SAFETY: `raw` is a genuine WNDPROC previously returned by
    // `SetWindowLongPtrW`/`GetWindowLongPtrW` for `GWLP_WNDPROC`; both it and
    // the target function type are pointer-width.
    Some(unsafe {
        std::mem::transmute::<isize, unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>(
            raw,
        )
    })
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_ACTIVATE && (wparam.0 as u32 & 0xFFFF) == WA_INACTIVE {
        SUBCLASSES.with(|map| {
            if let Some((_, callback)) = map.borrow().get(&(hwnd.0 as isize)) {
                callback();
            }
        });
    }

    let original =
        SUBCLASSES.with(|map| map.borrow().get(&(hwnd.0 as isize)).and_then(|(p, _)| *p));
    match original {
        // SAFETY: `original` is the real previous WNDPROC captured in
        // `watch_deactivation`, and `hwnd`/`msg`/`wparam`/`lparam` are exactly
        // what this procedure itself was just called with.
        Some(_) => unsafe { CallWindowProcW(original, hwnd, msg, wparam, lparam) },
        None => LRESULT(0),
    }
}

/// Un-subclasses the window on drop, restoring its original WNDPROC.
pub struct DeactivationWatch {
    hwnd: HWND,
    original_raw: isize,
}

impl Drop for DeactivationWatch {
    fn drop(&mut self) {
        // SAFETY: `self.hwnd` is still a live window for the lifetime of this
        // watch (callers are expected to drop it no later than the window
        // itself is torn down); `self.original_raw` is the real WNDPROC this
        // subclass replaced.
        unsafe {
            let _ = SetWindowLongPtrW(self.hwnd, GWLP_WNDPROC, self.original_raw);
        }
        SUBCLASSES.with(|map| {
            map.borrow_mut().remove(&(self.hwnd.0 as isize));
        });
    }
}

/// Subclasses `window`'s WNDPROC to observe `WM_ACTIVATE(WA_INACTIVE)` and
/// invoke `on_deactivated` (PLAN.md §3.3's `close_on_focus_loss` setting,
/// which has no public Slint API to hook). `WM_ACTIVATE` is delivered on the
/// same thread that owns the window's message queue — the Slint UI thread
/// itself — so no cross-thread marshaling is needed here, unlike the hotkey
/// thread's `WM_HOTKEY` messages. Always forwards to the original WNDPROC via
/// `CallWindowProcW` so Slint's own window handling still runs.
pub fn watch_deactivation(
    window: &slint::Window,
    on_deactivated: impl Fn() + 'static,
) -> Option<DeactivationWatch> {
    let hwnd = hwnd_of(window)?;
    let key = hwnd.0 as isize;

    // SAFETY: `hwnd` is a live window's real handle; `subclass_proc` matches
    // the `WNDPROC` signature exactly.
    let original_raw =
        unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, subclass_proc as *const () as isize) };
    let original = wndproc_from_isize(original_raw);

    SUBCLASSES.with(|map| {
        map.borrow_mut()
            .insert(key, (original, Box::new(on_deactivated)));
    });

    Some(DeactivationWatch { hwnd, original_raw })
}

// A second, independent subclassing mechanism (same pattern as
// `watch_deactivation`/`SUBCLASSES`, kept as a parallel implementation
// rather than a shared one so this Phase 9 addition can't regress the
// already-shipping deactivation-watch behavior): observes
// `WM_DISPLAYCHANGE` on a *persistent* window (Phase 9's monitor-change
// recovery watches the Settings window, which exists for the whole process
// lifetime, unlike the Quick Switcher the user can close). Delivered on the
// same UI thread that owns the window's message queue, so `on_change` can
// capture `Rc`s directly.
thread_local! {
    static DISPLAY_SUBCLASSES: RefCell<HashMap<isize, Subclass>> = RefCell::new(HashMap::new());
}

unsafe extern "system" fn display_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_DISPLAYCHANGE {
        DISPLAY_SUBCLASSES.with(|map| {
            if let Some((_, callback)) = map.borrow().get(&(hwnd.0 as isize)) {
                callback();
            }
        });
    }

    let original =
        DISPLAY_SUBCLASSES.with(|map| map.borrow().get(&(hwnd.0 as isize)).and_then(|(p, _)| *p));
    match original {
        // SAFETY: `original` is the real previous WNDPROC captured in
        // `watch_display_changes`, and `hwnd`/`msg`/`wparam`/`lparam` are
        // exactly what this procedure itself was just called with.
        Some(_) => unsafe { CallWindowProcW(original, hwnd, msg, wparam, lparam) },
        None => LRESULT(0),
    }
}

/// Un-subclasses the window on drop, restoring its original WNDPROC.
pub struct DisplayChangeWatch {
    hwnd: HWND,
    original_raw: isize,
}

impl Drop for DisplayChangeWatch {
    fn drop(&mut self) {
        // SAFETY: see `DeactivationWatch::drop`'s identical reasoning.
        unsafe {
            let _ = SetWindowLongPtrW(self.hwnd, GWLP_WNDPROC, self.original_raw);
        }
        DISPLAY_SUBCLASSES.with(|map| {
            map.borrow_mut().remove(&(self.hwnd.0 as isize));
        });
    }
}

/// Subclasses `window`'s WNDPROC to observe `WM_DISPLAYCHANGE` and invoke
/// `on_change` (Phase 9's monitor-configuration-change recovery — no public
/// Slint API exposes this event). Always forwards to the original WNDPROC via
/// `CallWindowProcW` so Slint's own window handling still runs.
pub fn watch_display_changes(
    window: &slint::Window,
    on_change: impl Fn() + 'static,
) -> Option<DisplayChangeWatch> {
    let hwnd = hwnd_of(window)?;
    let key = hwnd.0 as isize;

    // SAFETY: `hwnd` is a live window's real handle; `display_subclass_proc`
    // matches the `WNDPROC` signature exactly.
    let original_raw = unsafe {
        SetWindowLongPtrW(
            hwnd,
            GWLP_WNDPROC,
            display_subclass_proc as *const () as isize,
        )
    };
    let original = wndproc_from_isize(original_raw);

    DISPLAY_SUBCLASSES.with(|map| {
        map.borrow_mut()
            .insert(key, (original, Box::new(on_change)));
    });

    Some(DisplayChangeWatch { hwnd, original_raw })
}
