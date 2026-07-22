//! Process elevation: run the main app with administrator rights so it can
//! manage windows owned by elevated processes (e.g. Libre Hardware Monitor,
//! which needs admin to read sensors). A non-elevated process cannot call
//! `SetWindowPos` on an elevated window — Windows' UIPI returns
//! ERROR_ACCESS_DENIED — so those windows can't be parked or restored.
//!
//! This is done at runtime (self-elevation) rather than via the embedded
//! manifest's `requestedExecutionLevel`, because the single build script embeds
//! one manifest into *both* binaries; a `requireAdministrator` manifest would
//! also force `repodeck-hook.exe` — which Codex spawns non-elevated — to prompt
//! for UAC and break the agent-event pipe. Self-elevating only in
//! `repodeck.exe`'s `main` keeps the hook untouched.

use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

/// Whether the current process is running with an elevated (administrator)
/// token.
pub fn is_elevated() -> bool {
    // SAFETY: `OpenProcessToken` on the current pseudo-handle, then a fixed-size
    // `TOKEN_ELEVATION` query. Both handles are validated before use and the
    // token handle is closed on every path.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(std::ptr::from_mut(&mut elevation).cast()),
            u32::try_from(std::mem::size_of::<TOKEN_ELEVATION>()).unwrap_or(0),
            &mut returned,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// If the process is not already elevated, relaunches this same executable with
/// the `runas` verb (triggering the UAC prompt) and returns `true` so `main`
/// can exit and let the elevated instance take over. Returns `false` when
/// already elevated, or when the user declines / the relaunch fails — in which
/// case the caller continues running non-elevated (still usable, just unable to
/// move elevated windows).
#[must_use]
pub fn relaunch_as_admin_if_needed() -> bool {
    if is_elevated() {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe_w: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `w!("runas")` is a static null-terminated wide string; `exe_w` is
    // a null-terminated wide string that outlives the call.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            PCWSTR(exe_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a value greater than 32 on success.
    result.0 as isize > 32
}
