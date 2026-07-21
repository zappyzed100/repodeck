//! The software "unplug/replug" side effect: ask Windows to re-detect and re-apply
//! the display topology after a resume/reconnect leaves it wrong (PLAN.md §4.6,
//! Phase 9 resilience).
//!
//! This is deliberately *not* a physical action and never permanently detaches a
//! display. It uses the Connecting and Configuring Displays (CCD) APIs from
//! [`windows::Win32::Devices::Display`]:
//!
//! * [`QueryDisplayConfig`] captures the current active configuration *before* any
//!   mutation, so we can always put things back (SAFETY CONSTRAINT: never leave the
//!   user with fewer displays than they started with).
//! * [`SetDisplayConfig`] with `SDC_APPLY | SDC_TOPOLOGY_EXTEND` asks Windows to
//!   re-apply the persisted extend topology across whatever is currently connected.
//!   This is the "replug": it forces a fresh detect+apply pass and pulls a monitor
//!   that dropped off the bus back into the desktop, and Windows itself picks a safe
//!   mode. It works **without elevation** — which is the whole point, since RepoDeck
//!   ships `asInvoker` (see `build.rs` / the embedded manifest).
//!
//! ## The privilege branch (see [`reapply_display_topology`])
//!
//! `SetDisplayConfig` returns a `WIN32_ERROR` code. On a locked-down machine the
//! topology-reapply can come back `ERROR_ACCESS_DENIED` or
//! `ERROR_PRIVILEGE_NOT_HELD`. Because the app is `asInvoker` by design, we do
//! **not** try to elevate or relaunch as admin. Instead we log a clear Japanese
//! warning telling the user automatic re-detection needs a manual step, and fall
//! back to re-applying the *captured* configuration (`SDC_USE_SUPPLIED_DISPLAY_CONFIG`),
//! which only restores what was already active and therefore cannot black out the
//! screens. [`is_process_elevated`] is available if a caller wants to log the token
//! state, but we never change behaviour to *require* elevation.

use std::mem::size_of;
use std::ptr;

use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, GetDisplayConfigBufferSizes,
    QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig, SDC_APPLY, SDC_TOPOLOGY_EXTEND,
    SDC_USE_SUPPLIED_DISPLAY_CONFIG, SetDisplayConfig,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_PRIVILEGE_NOT_HELD, ERROR_SUCCESS, HANDLE, WIN32_ERROR,
};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::windowing::monitor;
use crate::windowing::win32_error::WindowError;

/// What [`reapply_display_topology`] actually did, so the caller can log a precise,
/// auditable outcome (PLAN.md §10 ログ).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayResetOutcome {
    /// The non-elevated `SDC_APPLY | SDC_TOPOLOGY_EXTEND` re-apply succeeded and the
    /// active-monitor count did not drop. This is the normal success path.
    Reapplied,
    /// The topology re-apply failed with a privilege error. We did NOT elevate; we
    /// fell back to re-applying the captured configuration so the user keeps every
    /// display that was already active.
    PrivilegeFallback,
    /// The re-apply reported success but the active-monitor count dropped, so we
    /// restored the captured configuration to honour the "never fewer displays"
    /// safety constraint.
    RestoredCaptured,
}

/// A snapshot of the active display configuration, captured before any mutation so
/// it can be re-applied verbatim as a safety net.
struct CapturedConfig {
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
    /// Number of monitors [`monitor::enumerate_monitors`] saw at capture time. The
    /// post-mutation count must never fall below this.
    active_monitor_count: usize,
}

/// Captures the current active display configuration (paths + modes) via the CCD
/// query APIs. Used both as the restore point and to detect a monitor-count drop.
fn capture_active_config() -> Result<CapturedConfig, WindowError> {
    let mut path_count: u32 = 0;
    let mut mode_count: u32 = 0;

    // SAFETY: both out-params are valid `u32` locations; the call only writes the
    // required buffer sizes for the active-paths query.
    let status = unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
    };
    if status != ERROR_SUCCESS {
        return Err(win32_error_from("GetDisplayConfigBufferSizes", status));
    }

    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];

    // SAFETY: `paths`/`modes` are sized to the counts just returned; the count
    // out-params are valid and are updated in place with the number actually filled.
    let status = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(win32_error_from("QueryDisplayConfig", status));
    }

    // Windows may report fewer entries than the buffer sizes; trim to what it filled.
    paths.truncate(path_count as usize);
    modes.truncate(mode_count as usize);

    let active_monitor_count = monitor::enumerate_monitors().map(|m| m.len()).unwrap_or(0);

    Ok(CapturedConfig {
        paths,
        modes,
        active_monitor_count,
    })
}

/// Re-applies a previously [`captured`](capture_active_config) configuration exactly
/// as it was. Uses `SDC_USE_SUPPLIED_DISPLAY_CONFIG`, so it only restores what was
/// already active and can never reduce the display count.
fn reapply_captured(captured: &CapturedConfig) -> Result<(), WindowError> {
    // SAFETY: the slices come straight from a successful `QueryDisplayConfig`, so
    // they are a self-consistent config of the sizes Windows itself returned.
    let status = unsafe {
        SetDisplayConfig(
            Some(&captured.paths),
            Some(&captured.modes),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG,
        )
    };
    if status == ERROR_SUCCESS.0 as i32 {
        Ok(())
    } else {
        Err(win32_error_from(
            "SetDisplayConfig(restore captured)",
            WIN32_ERROR(status as u32),
        ))
    }
}

/// Performs the conservative, non-elevated software re-detect.
///
/// Sequence (mirrors the module-level safety notes):
/// 1. Capture the current active config as a restore point.
/// 2. `SetDisplayConfig(None, None, SDC_APPLY | SDC_TOPOLOGY_EXTEND)` — the replug.
/// 3. If that returns a privilege error (`ERROR_ACCESS_DENIED` /
///    `ERROR_PRIVILEGE_NOT_HELD`): log a Japanese warning, restore the captured
///    config, and report [`DisplayResetOutcome::PrivilegeFallback`]. We never elevate.
/// 4. On any other failure: restore the captured config and return the error.
/// 5. On success: if the active-monitor count dropped, restore the captured config
///    and report [`DisplayResetOutcome::RestoredCaptured`]; otherwise
///    [`DisplayResetOutcome::Reapplied`].
pub fn reapply_display_topology() -> Result<DisplayResetOutcome, WindowError> {
    let captured = capture_active_config()?;

    // SAFETY: passing `None`/`None` with the topology flags asks Windows to re-apply
    // the persisted extend topology across the currently-connected displays; no
    // caller-supplied buffers are dereferenced.
    let status = unsafe { SetDisplayConfig(None, None, SDC_APPLY | SDC_TOPOLOGY_EXTEND) };

    // `SetDisplayConfig` returns a LONG WIN32 error code (0 == success).
    let status = WIN32_ERROR(status as u32);

    if status == ERROR_ACCESS_DENIED || status == ERROR_PRIVILEGE_NOT_HELD {
        // The privileged branch: do NOT attempt to elevate or relaunch as admin.
        tracing::warn!(
            error_code = status.0,
            elevated = is_process_elevated(),
            "ディスプレイの自動再検出には管理者権限が必要でした。RepoDeckは昇格せず、\
             既存の表示構成を維持します。手動でモニターを再接続するか、Windowsの\
             「ディスプレイ設定」から再検出してください。"
        );
        reapply_captured(&captured)?;
        return Ok(DisplayResetOutcome::PrivilegeFallback);
    }

    if status != ERROR_SUCCESS {
        // Any other failure: put the captured config back before surfacing the error
        // so we never leave the desktop in a half-applied state.
        tracing::warn!(
            error_code = status.0,
            "SetDisplayConfig(トポロジ再適用)が失敗しました。取得済みの構成に復元します。"
        );
        reapply_captured(&captured)?;
        return Err(win32_error_from(
            "SetDisplayConfig(SDC_TOPOLOGY_EXTEND)",
            status,
        ));
    }

    // Success — but honour the "never fewer displays" constraint.
    let after = monitor::enumerate_monitors().map(|m| m.len()).unwrap_or(0);
    if after < captured.active_monitor_count {
        tracing::warn!(
            before = captured.active_monitor_count,
            after,
            "トポロジ再適用後にモニター数が減少しました。取得済みの構成に復元します。"
        );
        reapply_captured(&captured)?;
        return Ok(DisplayResetOutcome::RestoredCaptured);
    }

    tracing::info!(
        monitors = after,
        "ディスプレイトポロジを再適用しました（ソフトウェア再検出）。"
    );
    Ok(DisplayResetOutcome::Reapplied)
}

/// Returns whether the current process token is elevated.
///
/// Purely informational: RepoDeck is `asInvoker` and never *requires* elevation.
/// The privilege branch in [`reapply_display_topology`] logs this so a support log
/// can distinguish "denied while non-elevated" from other failures. Returns `false`
/// on any query failure (the safe assumption for an `asInvoker` app).
pub fn is_process_elevated() -> bool {
    let mut token = HANDLE::default();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle valid for this call, and
    // `token` is a valid out-location for the opened token handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }

    let mut elevation = TOKEN_ELEVATION::default();
    let mut ret_len: u32 = 0;
    // SAFETY: `elevation` is a correctly-sized `TOKEN_ELEVATION` buffer and `token`
    // is the handle just opened; `ret_len` receives the bytes written.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(ptr::from_mut(&mut elevation).cast()),
            u32::try_from(size_of::<TOKEN_ELEVATION>()).unwrap_or(0),
            &mut ret_len,
        )
    };

    // SAFETY: `token` is a valid, owned handle we are finished with.
    unsafe {
        let _ = CloseHandle(token);
    }

    ok.is_ok() && elevation.TokenIsElevated != 0
}

fn win32_error_from(context: &'static str, code: WIN32_ERROR) -> WindowError {
    WindowError::win32(context, windows::core::Error::from(code.to_hresult()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The CCD side effects mutate the live display configuration, so they cannot run
    // in CI (mirrors `tests/windows_e2e.rs`'s `#[ignore]` convention). This test is
    // ignored by default and only exercised on real hardware.
    #[test]
    #[ignore = "mutates the live display configuration; run manually on hardware"]
    fn reapply_topology_on_real_hardware() {
        let outcome = reapply_display_topology().expect("re-apply should not error");
        // Any outcome is acceptable; the invariant is that it returns without leaving
        // the desktop with fewer displays (enforced inside the function).
        println!("display reset outcome: {outcome:?}");
    }

    #[test]
    fn is_process_elevated_does_not_panic() {
        // We can't assert the value (depends on how CI runs), only that querying the
        // token is safe and total.
        let _ = is_process_elevated();
    }
}
