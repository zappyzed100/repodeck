//! Reading and changing a single window's placement, plus batched moves
//! (PLAN.md §4.5 `BeginDeferWindowPos`/`DeferWindowPos`/`EndDeferWindowPos`).

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GetWindowPlacement, HDWP, HWND_TOP,
    SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetWindowPos,
    ShowWindow, WINDOWPLACEMENT,
};

use crate::domain::placement::{PixelRect, SavedShowState};
use crate::windowing::win32_error::WindowError;

/// Reads the current show state (normal/maximized/minimized) of `hwnd`.
pub fn get_show_state(hwnd: HWND) -> Result<SavedShowState, WindowError> {
    let placement = get_window_placement(hwnd)?;

    Ok(if placement.showCmd == SW_SHOWMAXIMIZED.0 as u32 {
        SavedShowState::Maximized
    } else if placement.showCmd == SW_SHOWMINIMIZED.0 as u32 {
        SavedShowState::Minimized
    } else {
        SavedShowState::Normal
    })
}

/// Reads `hwnd`'s restored (non-maximized, non-minimized) rectangle, in physical
/// screen coordinates, even while the window is currently maximized or minimized.
pub fn get_normal_rect(hwnd: HWND) -> Result<PixelRect, WindowError> {
    let placement = get_window_placement(hwnd)?;
    let r = placement.rcNormalPosition;
    Ok(PixelRect::new(
        r.left,
        r.top,
        r.right - r.left,
        r.bottom - r.top,
    ))
}

fn get_window_placement(hwnd: HWND) -> Result<WINDOWPLACEMENT, WindowError> {
    let mut placement = WINDOWPLACEMENT {
        length: u32::try_from(std::mem::size_of::<WINDOWPLACEMENT>()).unwrap(),
        ..Default::default()
    };

    // SAFETY: `hwnd` is a live handle; `placement.length` is set to the struct's
    // real size as `GetWindowPlacement` requires.
    unsafe { GetWindowPlacement(hwnd, &mut placement) }
        .map_err(|e| WindowError::win32("GetWindowPlacement", e))?;

    Ok(placement)
}

/// Moves and resizes `hwnd` to `rect` without activating it or changing its Z order
/// (PLAN.md §4.5). Callers must restore from maximized state first; this only sets
/// the restored-position rectangle.
pub fn set_window_rect(hwnd: HWND, rect: PixelRect) -> Result<(), WindowError> {
    // SAFETY: `hwnd` is a live handle; the remaining arguments are plain integers.
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        )
    }
    .map_err(|e| WindowError::win32("SetWindowPos", e))
}

/// Restores `hwnd` from maximized/minimized to its normal state, without activating it.
pub fn restore(hwnd: HWND) {
    // SAFETY: `hwnd` is a live handle. `ShowWindow`'s return value reports the
    // window's *previous* visibility, not success; there is nothing actionable
    // to do with it here.
    let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
}

/// Maximizes `hwnd`.
pub fn maximize(hwnd: HWND) {
    // SAFETY: see `restore`.
    let _ = unsafe { ShowWindow(hwnd, SW_MAXIMIZE) };
}

/// Minimizes `hwnd`.
pub fn minimize(hwnd: HWND) {
    // SAFETY: see `restore`.
    let _ = unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
}

/// Moves `hwnd` directly above `insert_after` in Z order without moving or
/// resizing it, or activating it (PLAN.md §3.8 step 7). `None` places it at
/// the top of its own Z-order group (`HWND_TOP`).
pub fn set_z_order_after(hwnd: HWND, insert_after: Option<HWND>) {
    // SAFETY: `hwnd` and `insert_after` (when present) are live handles;
    // `SWP_NOMOVE | SWP_NOSIZE` makes the position/size arguments ignored.
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            Some(insert_after.unwrap_or(HWND_TOP)),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };
}

/// Best-effort foreground focus (PLAN.md §3.8 step 8, §4.5: "フォーカスは
/// `SetForegroundWindow`のベストエフォートとする"). Win32 may refuse this
/// request depending on foreground-lock rules; there is nothing actionable
/// to do with a failure here.
pub fn set_foreground_best_effort(hwnd: HWND) {
    // SAFETY: `hwnd` is a live handle.
    let _ = unsafe { SetForegroundWindow(hwnd) };
}

/// A single `BeginDeferWindowPos`/`DeferWindowPos*`/`EndDeferWindowPos` batch
/// (PLAN.md §4.5). Every window queued via [`BatchMove::defer`] is moved
/// atomically once [`BatchMove::commit`] runs.
pub struct BatchMove {
    hdwp: HDWP,
}

impl BatchMove {
    pub fn begin(window_count: usize) -> Result<Self, WindowError> {
        let count = i32::try_from(window_count).unwrap_or(i32::MAX);
        // SAFETY: no preconditions beyond a valid `count`.
        let hdwp = unsafe { BeginDeferWindowPos(count) }
            .map_err(|e| WindowError::win32("BeginDeferWindowPos", e))?;
        Ok(Self { hdwp })
    }

    /// Queues `hwnd` to move to `rect` without activating it or changing Z order.
    #[must_use = "DeferWindowPos returns a new HDWP that must replace the previous one"]
    pub fn defer(mut self, hwnd: HWND, rect: PixelRect) -> Result<Self, WindowError> {
        // SAFETY: `self.hdwp` is the live handle from the most recent
        // `BeginDeferWindowPos`/`DeferWindowPos` call; `hwnd` is a live window handle.
        let hdwp = unsafe {
            DeferWindowPos(
                self.hdwp,
                hwnd,
                None,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            )
        }
        .map_err(|e| WindowError::win32("DeferWindowPos", e))?;

        self.hdwp = hdwp;
        Ok(self)
    }

    /// Commits every queued move as a single batch (PLAN.md §4.5).
    pub fn commit(self) -> Result<(), WindowError> {
        // SAFETY: `self.hdwp` is the live handle from the most recent
        // `BeginDeferWindowPos`/`DeferWindowPos` call.
        unsafe { EndDeferWindowPos(self.hdwp) }
            .map_err(|e| WindowError::win32("EndDeferWindowPos", e))
    }
}
