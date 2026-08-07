//! A coarse "OS boot session" token, used to decide whether persisted window
//! HWND bindings are still meaningful (PLAN.md §5.4 extension).
//!
//! HWND values are only stable for the lifetime of the OS session: after a
//! reboot Windows reassigns them, so a persisted HWND from a previous boot may
//! point at a different window or none. Tagging the bindings with a boot token
//! lets RepoDeck keep them across its *own* restart (the browser/VS Code windows
//! survive) but discard them after a *system* reboot.

use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::System::SystemInformation::GetTickCount64;

/// A token identifying the current OS boot session: the approximate boot
/// instant in Unix milliseconds (`now - uptime`). Constant across a session
/// (modulo small clock drift), and jumps far after a reboot.
pub fn boot_session_token() -> i64 {
    // SAFETY: `GetTickCount64` takes no arguments and only reads a system counter.
    let uptime_ms = unsafe { GetTickCount64() } as i64;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    now_ms - uptime_ms
}

/// Whether `stored` was captured in the current boot session. The 90-second
/// tolerance absorbs ordinary clock drift/NTP adjustments within a session,
/// while a reboot shifts the computed boot instant by far more than that.
pub fn is_same_session(stored: Option<i64>) -> bool {
    stored.is_some_and(|s| (s - boot_session_token()).abs() <= 90_000)
}
