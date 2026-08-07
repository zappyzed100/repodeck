//! Detects resume-from-sleep to drive automatic display recovery (PLAN.md §4.6,
//! Phase 9 resilience).
//!
//! This uses the **callback** form of `RegisterSuspendResumeNotification`
//! (`DEVICE_NOTIFY_CALLBACK`) rather than subclassing a window's `WM_POWERBROADCAST`.
//! RepoDeck's Slint windows are created lazily by the winit backend *inside* the
//! event loop and stay hidden until the user opens one, so at startup there is no
//! `HWND` to subclass — a window-based power watch would silently never register.
//! The callback API needs no window at all: Windows invokes our function directly
//! on resume.
//!
//! ## Threading
//!
//! The callback runs on an arbitrary system thread, so the closure handed to
//! [`watch_power_resume`] must be `Send` and must NOT touch UI-thread `Rc` state
//! directly — it should marshal onto the UI thread via
//! `slint::invoke_from_event_loop` (the same pattern the hotkey and pipe-server
//! threads use in `src/app.rs`).

use std::ffi::c_void;
use std::ptr;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Power::{
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY, RegisterSuspendResumeNotification,
    UnregisterSuspendResumeNotification,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND,
};

/// The registered resume callback, boxed and type-erased. The raw pointer to this
/// (via [`Box::into_raw`]) is what we hand Windows as the callback `Context`, so it
/// must stay alive until the registration is torn down in [`PowerWatchGuard::drop`].
type ResumeCallback = Box<dyn Fn() + Send>;

/// Unregisters the suspend/resume notification and frees the boxed callback on drop,
/// tying the watch to this guard's lifetime. `app.rs` keeps it alive for the whole
/// process.
pub struct PowerWatchGuard {
    handle: HPOWERNOTIFY,
    /// The `Box::into_raw(Box<ResumeCallback>)` pointer we passed as `Context`.
    /// Reclaimed in `drop`, after unregistering guarantees the callback can't fire.
    context: *mut ResumeCallback,
}

impl Drop for PowerWatchGuard {
    fn drop(&mut self) {
        // SAFETY: `handle` came from a successful `RegisterSuspendResumeNotification`;
        // unregistering stops future callbacks.
        unsafe {
            let _ = UnregisterSuspendResumeNotification(self.handle);
        }
        // The boxed callback is intentionally NOT freed here: `power_callback` may
        // still be executing on a system thread at the moment we unregister
        // (Windows makes no "no callback in flight" guarantee), so reclaiming the
        // box could free memory the callback is mid-dereference of. This guard is
        // held for the whole process lifetime, so leaking one small box at teardown
        // is harmless and strictly safer than a potential use-after-free.
        let _ = self.context;
    }
}

/// The C callback Windows invokes on a power event. Fires the registered closure on
/// resume notifications and ignores everything else. Runs on a system thread.
unsafe extern "system" fn power_callback(
    context: *const c_void,
    event_type: u32,
    _setting: *const c_void,
) -> u32 {
    if (event_type == PBT_APMRESUMEAUTOMATIC || event_type == PBT_APMRESUMESUSPEND)
        && !context.is_null()
    {
        // SAFETY: `context` is the `*mut ResumeCallback` we registered; it stays
        // valid until `PowerWatchGuard::drop` unregisters this callback and only
        // then frees it, so it is live for the duration of this call.
        let callback = unsafe { &*(context as *const ResumeCallback) };
        callback();
    }
    0 // ERROR_SUCCESS
}

/// Registers `on_resume` to fire whenever the system resumes from sleep.
///
/// Returns a [`PowerWatchGuard`] that keeps the registration active; drop it to
/// unregister. Returns `None` (non-fatal) if the registration fails — auto-recovery
/// simply won't fire on resume, and the manual tray trigger still works.
///
/// `on_resume` runs on a system thread; see the module docs on marshaling to the UI
/// thread.
pub fn watch_power_resume<F: Fn() + Send + 'static>(on_resume: F) -> Option<PowerWatchGuard> {
    let boxed: ResumeCallback = Box::new(on_resume);
    // Double-box so the `Context` is a thin pointer (a `Box<dyn Fn>` is a fat
    // pointer and can't be round-tripped through `*mut c_void`).
    let context: *mut ResumeCallback = Box::into_raw(Box::new(boxed));

    let mut params = DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(power_callback),
        Context: context.cast(),
    };

    // SAFETY: with `DEVICE_NOTIFY_CALLBACK`, `hrecipient` points to our
    // `DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS`; Windows reads the callback+context out of
    // it during this call and does not retain the pointer, so a stack `params` is
    // fine.
    let result = unsafe {
        RegisterSuspendResumeNotification(
            HANDLE(ptr::from_mut(&mut params).cast()),
            DEVICE_NOTIFY_CALLBACK,
        )
    };

    match result {
        Ok(handle) => Some(PowerWatchGuard { handle, context }),
        Err(err) => {
            tracing::warn!(error = %err, "RegisterSuspendResumeNotification failed");
            // SAFETY: reclaim the box we just leaked; it was never shared.
            unsafe {
                drop(Box::from_raw(context));
            }
            None
        }
    }
}
