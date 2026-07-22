//! Synthesises keystrokes into another process's window — used to trigger a
//! browser's own full-screen for a parked video ("退避後に全画面表示").
//!
//! Only YouTube's `F` (video-element full-screen) is sent — *not* `F11`
//! (browser page full-screen). Sending both nested a page-full-screen inside a
//! video-full-screen, and Chromium/Brave then leaves the window stuck
//! borderless and un-resizable on exit (a known browser bug). With just `F`,
//! toggling it off returns the window to a normal state cleanly (問題2 案A,
//! 2026-07-23). The key is handled by the *focused* window, so the target is
//! brought to the foreground first — on a short-lived background thread so the
//! switch itself never blocks.

use std::time::Duration;

use crate::domain::placement::PixelRect;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, SetFocus, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
};

const VK_F: u16 = 0x46;

/// Forces `hwnd` to the foreground, bypassing Windows' foreground-lock (which
/// otherwise makes `SetForegroundWindow` merely flash the taskbar when another
/// app is active). It temporarily attaches our thread's input queue to both the
/// current foreground window's thread and the target's, so the calls are treated
/// as coming from the active input context — the standard AttachThreadInput
/// trick. Required so the synthetic `F11`/`F` actually reach the browser
/// (問題1 案C, 2026-07-23).
fn force_foreground(hwnd: HWND) {
    // SAFETY: all handles/ids are validated (nonzero, not self) before each
    // call, and every successful `AttachThreadInput(.., true)` is paired with a
    // matching detach.
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

/// Presses and releases a single virtual key via the system input stream, which
/// delivers it to whatever window currently has keyboard focus.
fn tap(vk: u16) {
    let events = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: Default::default(),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];
    // SAFETY: `events` is a valid slice of two fully-initialised INPUT records;
    // the size argument is the exact struct size the API requires.
    unsafe {
        let _ = SendInput(&events, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Focuses `hwnd`, then sends `F` so a YouTube video goes full-screen. Runs on
/// a detached thread with small delays so the switch that triggered it isn't
/// blocked and so the key lands after the window has actually taken the
/// foreground. If `refocus` is given, the foreground is handed back to it
/// afterwards (the workset just switched to), so stealing focus for the
/// key-send is only momentary.
pub fn send_fullscreen_keys(hwnd: HWND, refocus: Option<HWND>) {
    let raw = hwnd.0 as isize;
    let refocus_raw = refocus.map(|h| h.0 as isize);
    std::thread::spawn(move || {
        let hwnd = HWND(raw as *mut _);
        // Let the switch settle (parking move, target focus) before we grab
        // focus for the key-send.
        std::thread::sleep(Duration::from_millis(250));
        force_foreground(hwnd);
        std::thread::sleep(Duration::from_millis(90));
        tap(VK_F); // YouTube video-element full-screen (F11 avoided — see module docs)

        if let Some(refocus_raw) = refocus_raw {
            std::thread::sleep(Duration::from_millis(120));
            let refocus = HWND(refocus_raw as *mut _);
            // SAFETY: `refocus` is a live handle; best-effort foreground.
            unsafe {
                let _ = SetForegroundWindow(refocus);
            }
        }
    });
}

/// Exits a YouTube video full-screen (the reverse of [`send_fullscreen_keys`]:
/// `F` again) and then places the window at its main-screen position
/// (`restore_rect`, maximized there if `maximized`). Used when a
/// full-screen-parked window is switched back to the main screen: a video
/// full-screen is not a Win32 maximized state, so `SW_RESTORE` cannot undo it —
/// the window would otherwise keep its (larger) parking-monitor size. Runs on a
/// detached thread: focus, toggle full-screen off, then apply the target
/// placement once the browser has returned to a normal window. The placement is
/// re-asserted (see `placement::set_placement`) because the browser restores its
/// own remembered bounds when leaving full-screen, racing this resize.
pub fn send_exit_fullscreen_keys(hwnd: HWND, restore_rect: PixelRect, maximized: bool) {
    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(raw as *mut _);
        std::thread::sleep(Duration::from_millis(250));
        force_foreground(hwnd);
        std::thread::sleep(Duration::from_millis(90));
        tap(VK_F); // toggle the YouTube video full-screen back off
        // Give the browser time to actually leave full-screen before placing;
        // `set_placement`'s own re-assert loop then outlasts the browser's
        // asynchronous restore of its remembered bounds.
        std::thread::sleep(Duration::from_millis(500));
        // Now that it's a normal window again, place it on the main screen
        // (restore to the saved main frame rect, not a fill cell).
        crate::windowing::placement::set_placement(hwnd, restore_rect, maximized, false);
    });
}
