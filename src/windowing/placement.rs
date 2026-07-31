//! Reading and changing a single window's placement, plus batched moves
//! (PLAN.md §4.5 `BeginDeferWindowPos`/`DeferWindowPos`/`EndDeferWindowPos`).

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, PROCESS_TERMINATE, TerminateProcess,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, BringWindowToTop, DeferWindowPos, EndDeferWindowPos, GetForegroundWindow,
    GetWindowPlacement, GetWindowRect, GetWindowThreadProcessId, HDWP, HWND_TOP, PostMessageW,
    SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow,
    SetWindowPos, ShowWindow, WINDOWPLACEMENT, WM_CLOSE,
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

/// The window's *visible* on-screen bounds — `GetWindowRect` minus the
/// invisible DWM frame (drop-shadow / resize border, ~7px on Chromium/most
/// apps). `None` if the window is gone or DWM has no answer.
fn visible_bounds(hwnd: HWND) -> Option<PixelRect> {
    let mut r = RECT::default();
    // SAFETY: `hwnd` may be stale; the call just fails then.
    if unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            std::ptr::from_mut(&mut r).cast(),
            u32::try_from(std::mem::size_of::<RECT>()).unwrap_or(0),
        )
    }
    .is_err()
    {
        return None;
    }
    Some(PixelRect::new(
        r.left,
        r.top,
        r.right - r.left,
        r.bottom - r.top,
    ))
}

/// The invisible DWM frame margins `(left, top, right, bottom)` of `hwnd`: how
/// far the real window frame (`GetWindowRect`) extends *beyond* its visible
/// bounds on each edge. To make a window's *visible* area exactly fill a cell,
/// expand the cell by these margins before `SetWindowPos` — otherwise adjacent
/// flush cells leave a ~2×border gap (verified against a hand-snapped layout,
/// 2026-07-23: a "half" cell of 960 needs a 974 frame, offset −7/+7).
fn frame_margins(hwnd: HWND) -> (i32, i32, i32, i32) {
    let mut frame = RECT::default();
    // SAFETY: `hwnd` may be stale; the call just fails then.
    if unsafe { GetWindowRect(hwnd, &mut frame) }.is_err() {
        return (0, 0, 0, 0);
    }
    let Some(v) = visible_bounds(hwnd) else {
        return (0, 0, 0, 0);
    };
    (
        v.x - frame.left,
        v.y - frame.top,
        frame.right - v.right(),
        frame.bottom - v.bottom(),
    )
}

/// Moves `hwnd` to `rect` or maximizes it on `rect`'s monitor, reliably even
/// when the window is currently maximized on another monitor. When `fill` is
/// set, `rect` is treated as the desired *visible* area and the window's frame
/// is expanded by its invisible DWM margins so the visible content fills `rect`
/// edge-to-edge (used for parking cells); otherwise `rect` is the frame rect
/// (used to restore a saved main placement).
///
/// Implementation notes (empirically verified, 2026-07-23): `SetWindowPlacement`
/// with a new `rcNormalPosition` is *ignored* for a currently-maximized window,
/// so the working sequence is un-maximize (without activating) → `SetWindowPos`
/// → optionally re-maximize. Some apps (Chromium: VS Code, Brave) re-assert
/// their own remembered bounds asynchronously, so a background thread re-applies
/// until it sticks.
pub fn set_placement(hwnd: HWND, rect: PixelRect, maximized: bool, fill: bool) {
    apply_placement(hwnd, rect, maximized, fill);

    let raw = hwnd.0 as isize;
    // Each placement of a window gets a fresh generation number. The re-assert
    // loop below stops the instant a newer `set_placement` for the same window
    // supersedes it — otherwise a previous switch's ~6.5s re-assert thread keeps
    // re-applying its (now stale) rect and fights the new switch's placement,
    // leaving windows at the wrong size / overlapping and making the parking
    // monitors flicker (2026-07-23).
    let my_generation = {
        let mut map = PLACEMENT_GENERATIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let g = map.entry(raw).or_insert(0);
        *g = g.wrapping_add(1);
        *g
    };
    std::thread::spawn(move || {
        // Long tail (≈6.5s cumulative): a browser leaving F11 full-screen
        // restores its own remembered bounds noticeably after our first apply,
        // so keep winning the race until it stops re-asserting.
        for delay_ms in [300u64, 500, 700, 1000, 1500, 2500] {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            // Superseded by a newer placement of this window? Stop.
            let current = PLACEMENT_GENERATIONS
                .lock()
                .ok()
                .and_then(|m| m.get(&raw).copied());
            if current != Some(my_generation) {
                break;
            }
            let hwnd = HWND(raw as *mut _);
            match placement_settled(hwnd, rect, maximized, fill) {
                None => break, // window is gone; nothing left to do
                Some(true) => break,
                Some(false) => {
                    // App re-asserted its own bounds after our apply; re-apply
                    // to keep winning the race until it settles.
                    apply_placement(hwnd, rect, maximized, fill);
                }
            }
        }
    });
}

/// Per-window placement generation. Bumped on every `set_placement`; a re-assert
/// loop aborts once its generation is no longer the latest for that window.
static PLACEMENT_GENERATIONS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<isize, u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// One pass of the working sequence: leave maximized/minimized state without
/// stealing focus, apply the rectangle (expanded to fill if `fill`), then
/// re-maximize if asked.
fn apply_placement(hwnd: HWND, rect: PixelRect, maximized: bool, fill: bool) {
    if get_show_state(hwnd).is_ok_and(|s| s != SavedShowState::Normal) {
        // SW_SHOWNOACTIVATE leaves maximized/minimized for the normal state
        // without activating (SW_RESTORE would steal focus on every re-apply).
        // SAFETY: `hwnd` is a live handle.
        let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    }
    let target = if fill && !maximized {
        let (ml, mt, mr, mb) = frame_margins(hwnd);
        PixelRect::new(
            rect.x - ml,
            rect.y - mt,
            rect.width + ml + mr,
            rect.height + mt + mb,
        )
    } else {
        rect
    };
    let _ = set_window_rect(hwnd, target);
    if maximized {
        // SAFETY: `hwnd` is a live handle.
        let _ = unsafe { ShowWindow(hwnd, SW_MAXIMIZE) };
    }
}

/// Whether the window currently matches the requested placement. `None` when
/// the window no longer exists.
fn placement_settled(hwnd: HWND, rect: PixelRect, maximized: bool, fill: bool) -> Option<bool> {
    let mut frame = RECT::default();
    // SAFETY: `hwnd` may be stale; GetWindowRect just fails in that case.
    if unsafe { GetWindowRect(hwnd, &mut frame) }.is_err() {
        return None;
    }
    if maximized {
        // Close enough = maximized on the right monitor (centers roughly agree).
        let (cx, cy) = (
            (frame.left + frame.right) / 2,
            (frame.top + frame.bottom) / 2,
        );
        let (tx, ty) = (rect.x + rect.width / 2, rect.y + rect.height / 2);
        let on_target_monitor = (cx - tx).abs() < 800 && (cy - ty).abs() < 600;
        Some(
            on_target_monitor && get_show_state(hwnd).is_ok_and(|s| s == SavedShowState::Maximized),
        )
    } else {
        // `fill` compares the *visible* bounds against the cell; otherwise the
        // frame rect against the saved rect. Tolerances absorb rounding.
        let actual = if fill {
            match visible_bounds(hwnd) {
                Some(v) => v,
                None => return Some(false),
            }
        } else {
            PixelRect::new(
                frame.left,
                frame.top,
                frame.right - frame.left,
                frame.bottom - frame.top,
            )
        };
        Some(
            (actual.x - rect.x).abs() < 24
                && (actual.y - rect.y).abs() < 24
                && (actual.width - rect.width).abs() < 48
                && (actual.height - rect.height).abs() < 48,
        )
    }
}

/// Restores `hwnd` from maximized/minimized to its normal state, without activating it.
pub fn restore(hwnd: HWND) {
    // SAFETY: `hwnd` is a live handle. `ShowWindow`'s return value reports the
    // window's *previous* visibility, not success; there is nothing actionable
    // to do with it here.
    let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
}

/// Maximizes `hwnd` on whichever monitor its restored position currently sits.
pub fn maximize(hwnd: HWND) {
    // SAFETY: see `restore`.
    let _ = unsafe { ShowWindow(hwnd, SW_MAXIMIZE) };
}

/// Minimizes `hwnd`.
pub fn minimize(hwnd: HWND) {
    // SAFETY: see `restore`.
    let _ = unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
}

/// Asks `hwnd` to close, exactly as its title-bar × would.
///
/// `WM_CLOSE` is *posted*, not sent: the app decides what to do with it, and may
/// well put up a "save changes?" prompt and stay open. That is the intended
/// behaviour — RepoDeck never kills a process, so nothing the user hasn't
/// confirmed is ever lost. Returns false if the message couldn't be posted (the
/// window is already gone, or belongs to a higher-integrity process).
pub fn close_window(hwnd: HWND) -> bool {
    // SAFETY: `hwnd` is a live handle; `WM_CLOSE` carries no pointer payload.
    unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }.is_ok()
}

/// `hwnd` を持つプロセスを終了させる。
///
/// リモートデスクトップ（mstsc.exe）のように `WM_CLOSE` では畳めないアプリ専用の
/// 逃げ道。mstsc はセッションを保持したまま居座り、次にセットを開き直しても新しい
/// 接続が張れない。編集中の文書を持つ種類のアプリではないので、終了させて問題ない。
/// 対象の判定は [`crate::application::launch_service::closes_only_by_kill`]。
pub fn kill_window_process(hwnd: HWND) -> bool {
    let mut process_id = 0u32;
    // SAFETY: `hwnd` is a live handle; `process_id` is a valid out-pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if process_id == 0 {
        return false;
    }
    // SAFETY: `process_id` came from `GetWindowThreadProcessId`.
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_TERMINATE, false, process_id) }) else {
        return false;
    };
    // SAFETY: `handle` is a valid, owned process handle opened for termination.
    let terminated = unsafe { TerminateProcess(handle, 0) }.is_ok();
    // SAFETY: `handle` is a valid, owned handle we are done with.
    unsafe {
        let _ = CloseHandle(handle);
    }
    terminated
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

/// 指定ウィンドウを強制的にフォアグラウンドへ出す（PLAN.md §3.8 step 8, §4.5）。
///
/// 素の `SetForegroundWindow` は、別プロセス（ChatGPT や Brave など）がフォア
/// グラウンドを握っていると拒否され、タスクバーを光らせるだけで窓は前面に
/// 出ない。切り替えでは移動も Z 順も `SWP_NOACTIVATE` でアクティベートを伴わず、
/// 前面化はこの1回だけが頼みの綱なので、単発で弾かれるとターゲット窓はメインに
/// 正しく置かれ Z 順も整ったまま他アプリの窓の下に埋まり、二度目の選択が必要に
/// なっていた（実機 2026-07-31: ChatGPT がメインに居るとき VSCode が上に出ない）。
///
/// `AttachThreadInput` で自スレッドの入力キューを「現在のフォアグラウンド窓の
/// スレッド」と「対象窓のスレッド」へ一時的に接続し、この呼び出しがアクティブな
/// 入力文脈から来たものとして扱わせる——`key_input` で実績のある標準の手法。
pub fn set_foreground_best_effort(hwnd: HWND) {
    force_foreground(hwnd);
}

/// `set_foreground_best_effort` の本体。`key_input` からも再利用するため分離。
///
/// SAFETY: 全ハンドル/スレッドID は各呼び出し前に非0・非自己を検証し、成功した
/// `AttachThreadInput(.., true)` はすべて対応する detach と対になる。
pub fn force_foreground(hwnd: HWND) {
    unsafe {
        let cur = GetCurrentThreadId();
        let fg = GetForegroundWindow();
        let fg_thread = if fg.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, None)
        };
        let tgt_thread = GetWindowThreadProcessId(hwnd, None);

        let att_fg =
            fg_thread != 0 && fg_thread != cur && AttachThreadInput(cur, fg_thread, true).as_bool();
        let att_tgt = tgt_thread != 0
            && tgt_thread != cur
            && tgt_thread != fg_thread
            && AttachThreadInput(cur, tgt_thread, true).as_bool();

        let _ = SetForegroundWindow(hwnd);
        let _ = BringWindowToTop(hwnd);
        let _ = SetFocus(Some(hwnd));

        if att_tgt {
            let _ = AttachThreadInput(cur, tgt_thread, false);
        }
        if att_fg {
            let _ = AttachThreadInput(cur, fg_thread, false);
        }
    }
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
