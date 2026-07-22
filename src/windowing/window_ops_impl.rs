//! The real `WindowOps` implementation, delegating to `windowing::placement`
//! (PLAN.md §4.5). Adds no new Win32 logic of its own beyond the documented
//! `EndDeferWindowPos`-failure fallback (PLAN.md §4.5).

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

use crate::application::window_ops::{WindowOps, WindowOpsError};
use crate::domain::placement::{PixelRect, SavedShowState};
use crate::windowing::placement;
use crate::windowing::win32_error::WindowError;

pub struct Win32WindowOps;

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut _)
}

impl WindowOps for Win32WindowOps {
    fn get_show_state(&self, hwnd: isize) -> Result<SavedShowState, WindowError> {
        placement::get_show_state(hwnd_of(hwnd))
    }

    fn get_normal_rect(&self, hwnd: isize) -> Result<PixelRect, WindowError> {
        placement::get_normal_rect(hwnd_of(hwnd))
    }

    fn restore(&self, hwnd: isize) {
        placement::restore(hwnd_of(hwnd));
    }

    fn maximize(&self, hwnd: isize) {
        placement::maximize(hwnd_of(hwnd));
    }

    fn set_placement(&self, hwnd: isize, rect: PixelRect, maximized: bool, fill: bool) {
        placement::set_placement(hwnd_of(hwnd), rect, maximized, fill);
    }

    fn send_fullscreen_keys(&self, hwnd: isize, refocus: Option<isize>) {
        crate::windowing::key_input::send_fullscreen_keys(hwnd_of(hwnd), refocus.map(hwnd_of));
    }

    fn exit_fullscreen(&self, hwnd: isize, restore_rect: PixelRect, maximized: bool) {
        crate::windowing::key_input::send_exit_fullscreen_keys(
            hwnd_of(hwnd),
            restore_rect,
            maximized,
        );
    }

    fn minimize(&self, hwnd: isize) {
        placement::minimize(hwnd_of(hwnd));
    }

    fn batch_move(&self, moves: &[(isize, PixelRect)]) -> Result<(), WindowOpsError> {
        let attempt = (|| -> Result<(), WindowError> {
            let mut batch = placement::BatchMove::begin(moves.len())?;
            for &(hwnd, rect) in moves {
                batch = batch.defer(hwnd_of(hwnd), rect)?;
            }
            batch.commit()
        })();

        // PLAN.md §4.5: fall back to per-window `SetWindowPos` if the atomic
        // batch fails at any point. A single window that cannot be moved — most
        // commonly one owned by an elevated process, where `SetWindowPos`
        // returns ERROR_ACCESS_DENIED under UIPI, or a window that just closed —
        // is skipped (logged) rather than aborting the whole switch. Otherwise
        // one admin-privileged app (e.g. Libre Hardware Monitor) in a workset
        // would roll the entire switch back and nothing would move.
        if attempt.is_err() {
            for &(hwnd, rect) in moves {
                if let Err(source) = placement::set_window_rect(hwnd_of(hwnd), rect) {
                    tracing::warn!(
                        error = %source,
                        hwnd,
                        "batch move: skipping a window that could not be moved \
                         (likely an elevated process — access denied — or a \
                         window that closed mid-switch)"
                    );
                }
            }
        }
        Ok(())
    }

    fn set_z_order_after(&self, hwnd: isize, insert_after: Option<isize>) {
        placement::set_z_order_after(hwnd_of(hwnd), insert_after.map(hwnd_of));
    }

    fn set_foreground(&self, hwnd: isize) {
        placement::set_foreground_best_effort(hwnd_of(hwnd));
    }

    fn is_window_alive(&self, hwnd: isize) -> bool {
        // SAFETY: `IsWindow` accepts any value, including stale/invalid
        // handles, and simply reports whether it's currently a valid window.
        unsafe { IsWindow(Some(hwnd_of(hwnd))) }.as_bool()
    }
}
