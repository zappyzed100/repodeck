//! The Win32 window-movement surface `SwitchCoordinator` needs, abstracted
//! behind a trait so its orchestration logic is unit-testable without a real
//! desktop (PLAN.md §4.5, §9.3's "`application`は`domain`とtraitへ依存").
//!
//! `windowing::window_ops_impl::Win32WindowOps` is the real implementation;
//! it adds no new Win32 logic beyond what `windowing::placement` already has.

use crate::domain::placement::{PixelRect, SavedShowState};
use crate::windowing::win32_error::WindowError;

pub trait WindowOps {
    fn get_show_state(&self, hwnd: isize) -> Result<SavedShowState, WindowError>;
    fn get_normal_rect(&self, hwnd: isize) -> Result<PixelRect, WindowError>;
    fn restore(&self, hwnd: isize);
    fn maximize(&self, hwnd: isize);

    /// Places `hwnd` at `rect` (or maximized on `rect`'s monitor), reliably even
    /// when it is currently maximized elsewhere. When `fill` is set, `rect` is
    /// the desired *visible* area and the frame is expanded by the window's
    /// invisible DWM margins so the visible content fills it edge-to-edge (for
    /// parking cells); otherwise `rect` is the frame rect (restoring a saved main
    /// placement). See `windowing::placement::set_placement`.
    fn set_placement(&self, hwnd: isize, rect: PixelRect, maximized: bool, fill: bool);
    fn minimize(&self, hwnd: isize);

    /// Sends the browser's own full-screen keys (`F11` then `F`) to a parked
    /// window so a video fills the screen ("退避後に全画面表示"), momentarily
    /// focusing it and then handing the foreground back to `refocus`. Best-effort
    /// and asynchronous — see `windowing::key_input::send_fullscreen_keys`.
    fn send_fullscreen_keys(&self, hwnd: isize, refocus: Option<isize>);

    /// Exits a browser full-screen (reverse keys) and places `hwnd` at
    /// `restore_rect` (maximized there if `maximized`), for a full-screen-parked
    /// window returning to the main screen (`SW_RESTORE` can't undo a page/video
    /// full-screen). `fill` has the same meaning as in [`WindowOps::set_placement`]:
    /// `true` treats `restore_rect` as the desired *visible* area and expands it
    /// by the window's invisible DWM margins (parking cells); `false` uses the
    /// frame rect as-is (restoring a saved main placement). Best-effort and
    /// asynchronous — see `windowing::key_input::send_exit_fullscreen_keys`.
    fn exit_fullscreen(&self, hwnd: isize, restore_rect: PixelRect, maximized: bool, fill: bool);

    /// Atomic batch move (PLAN.md §4.5). Implementations fall back to
    /// per-window moves if the atomic path fails, per §4.5's documented
    /// policy; this returns `Err` only if that fallback also fails.
    fn batch_move(&self, moves: &[(isize, PixelRect)]) -> Result<(), WindowOpsError>;

    /// Z-order-only move, placing `hwnd` directly above `insert_after`
    /// (`None` leaves Z-order untouched).
    fn set_z_order_after(&self, hwnd: isize, insert_after: Option<isize>);

    /// Best-effort; never fatal to a switch (PLAN.md §3.8 step 8).
    fn set_foreground(&self, hwnd: isize);

    /// Whether `hwnd` still refers to a live top-level window — used to
    /// detect a window that closed mid-switch.
    fn is_window_alive(&self, hwnd: isize) -> bool;
}

#[derive(Debug, thiserror::Error)]
pub enum WindowOpsError {
    #[error("batch move failed for hwnd {hwnd}: {source}")]
    PerWindowFailed {
        hwnd: isize,
        #[source]
        source: WindowError,
    },
}

/// An in-memory `WindowOps` fake with failure-injection hooks, used by
/// `switch_coordinator`'s and `recovery_service`'s unit tests to exercise
/// paths (a mid-switch window close, an `EndDeferWindowPos` failure) that
/// aren't sanely reproducible against real Win32.
#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::{WindowOps, WindowOpsError};
    use crate::domain::placement::{PixelRect, SavedShowState};
    use crate::windowing::win32_error::WindowError;

    #[derive(Clone)]
    struct FakeState {
        rect: PixelRect,
        show_state: SavedShowState,
        alive: bool,
    }

    #[derive(Default)]
    pub(crate) struct FakeWindowOps {
        windows: RefCell<HashMap<isize, FakeState>>,
        fail_next_batch_move: RefCell<bool>,
        fail_per_window_fallback_for: RefCell<Vec<isize>>,
        foreground_history: RefCell<Vec<isize>>,
        /// 全画面変換キーを送った相手。切替のたびに撃ち直していないかを検証する。
        fullscreen_key_targets: RefCell<Vec<isize>>,
        /// 全画面解除を要求した相手。解除は配置と同じ結果になるので、rect だけでは
        /// 「解除キーを撃ったか」が区別できない。
        exit_fullscreen_targets: RefCell<Vec<isize>>,
        /// `set_z_order_after` の呼び出し履歴 `(hwnd, insert_after)`。
        /// `insert_after == None` は「最前面（HWND_TOP）へ持ち上げた」ことを示す。
        z_order_history: RefCell<Vec<(isize, Option<isize>)>>,
    }

    impl FakeWindowOps {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        pub(crate) fn seed_window(&self, hwnd: isize, rect: PixelRect, show_state: SavedShowState) {
            self.windows.borrow_mut().insert(
                hwnd,
                FakeState {
                    rect,
                    show_state,
                    alive: true,
                },
            );
        }

        /// The next `batch_move` call falls back to the per-window path,
        /// simulating an `EndDeferWindowPos` failure.
        pub(crate) fn fail_next_batch_move(&self) {
            *self.fail_next_batch_move.borrow_mut() = true;
        }

        pub(crate) fn rect_of(&self, hwnd: isize) -> Option<PixelRect> {
            self.windows.borrow().get(&hwnd).map(|s| s.rect)
        }

        pub(crate) fn show_state_of(&self, hwnd: isize) -> Option<SavedShowState> {
            self.windows.borrow().get(&hwnd).map(|s| s.show_state)
        }

        pub(crate) fn foreground_history(&self) -> Vec<isize> {
            self.foreground_history.borrow().clone()
        }

        /// 全画面変換キーを送った相手の履歴。
        pub(crate) fn fullscreen_key_targets(&self) -> Vec<isize> {
            self.fullscreen_key_targets.borrow().clone()
        }

        /// 全画面解除を要求した相手の履歴。
        pub(crate) fn exit_fullscreen_targets(&self) -> Vec<isize> {
            self.exit_fullscreen_targets.borrow().clone()
        }

        /// `set_z_order_after` の呼び出し履歴。
        pub(crate) fn z_order_history(&self) -> Vec<(isize, Option<isize>)> {
            self.z_order_history.borrow().clone()
        }
    }

    impl WindowOps for FakeWindowOps {
        fn get_show_state(&self, hwnd: isize) -> Result<SavedShowState, WindowError> {
            self.windows
                .borrow()
                .get(&hwnd)
                .map(|s| s.show_state)
                .ok_or_else(|| WindowError::no_detail("GetWindowPlacement"))
        }

        fn get_normal_rect(&self, hwnd: isize) -> Result<PixelRect, WindowError> {
            self.windows
                .borrow()
                .get(&hwnd)
                .map(|s| s.rect)
                .ok_or_else(|| WindowError::no_detail("GetWindowPlacement"))
        }

        fn restore(&self, hwnd: isize) {
            if let Some(state) = self.windows.borrow_mut().get_mut(&hwnd) {
                state.show_state = SavedShowState::Normal;
            }
        }

        fn maximize(&self, hwnd: isize) {
            if let Some(state) = self.windows.borrow_mut().get_mut(&hwnd) {
                state.show_state = SavedShowState::Maximized;
            }
        }

        fn set_placement(&self, hwnd: isize, rect: PixelRect, maximized: bool, _fill: bool) {
            if let Some(state) = self.windows.borrow_mut().get_mut(&hwnd) {
                state.rect = rect;
                state.show_state = if maximized {
                    SavedShowState::Maximized
                } else {
                    SavedShowState::Normal
                };
            }
        }

        fn send_fullscreen_keys(&self, hwnd: isize, _refocus: Option<isize>) {
            // キー合成自体は再現しないが、誰に送ったかは記録する。
            self.fullscreen_key_targets.borrow_mut().push(hwnd);
        }

        fn exit_fullscreen(
            &self,
            hwnd: isize,
            restore_rect: PixelRect,
            maximized: bool,
            fill: bool,
        ) {
            // Modeled as an immediate placement so restore tests still observe
            // the final state; the key synthesis itself isn't modeled.
            self.exit_fullscreen_targets.borrow_mut().push(hwnd);
            self.set_placement(hwnd, restore_rect, maximized, fill);
        }

        fn minimize(&self, hwnd: isize) {
            if let Some(state) = self.windows.borrow_mut().get_mut(&hwnd) {
                state.show_state = SavedShowState::Minimized;
            }
        }

        fn batch_move(&self, moves: &[(isize, PixelRect)]) -> Result<(), WindowOpsError> {
            let use_fallback = self.fail_next_batch_move.replace(false);

            if !use_fallback {
                // Models `BeginDeferWindowPos`/`EndDeferWindowPos`: nothing
                // takes effect until the whole batch commits.
                for &(hwnd, rect) in moves {
                    if let Some(state) = self.windows.borrow_mut().get_mut(&hwnd) {
                        state.rect = rect;
                    }
                }
                return Ok(());
            }

            // Fallback path: apply moves one at a time, stopping at the first
            // failure — mirrors the real per-window `SetWindowPos` loop, where
            // windows before the failing one have already moved.
            let fallback_failures = self.fail_per_window_fallback_for.borrow().clone();
            for &(hwnd, rect) in moves {
                if fallback_failures.contains(&hwnd) || !self.is_window_alive(hwnd) {
                    return Err(WindowOpsError::PerWindowFailed {
                        hwnd,
                        source: WindowError::no_detail("SetWindowPos"),
                    });
                }
                if let Some(state) = self.windows.borrow_mut().get_mut(&hwnd) {
                    state.rect = rect;
                }
            }
            Ok(())
        }

        fn set_z_order_after(&self, hwnd: isize, insert_after: Option<isize>) {
            // Z-order isn't visually modeled by the fake, but the *sequence* is
            // recorded so tests can assert which windows were raised to the top
            // (`None`) and in what order.
            self.z_order_history.borrow_mut().push((hwnd, insert_after));
        }

        fn set_foreground(&self, hwnd: isize) {
            self.foreground_history.borrow_mut().push(hwnd);
        }

        fn is_window_alive(&self, hwnd: isize) -> bool {
            self.windows.borrow().get(&hwnd).is_some_and(|s| s.alive)
        }
    }
}
