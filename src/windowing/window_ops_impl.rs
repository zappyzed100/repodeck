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
        // batch fails at any point.
        if attempt.is_err() {
            for &(hwnd, rect) in moves {
                placement::set_window_rect(hwnd_of(hwnd), rect)
                    .map_err(|source| WindowOpsError::PerWindowFailed { hwnd, source })?;
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
