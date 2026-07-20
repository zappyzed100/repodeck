//! Global hotkey registration and its dedicated message-loop thread
//! (PLAN.md §9.1 "Hotkeyスレッド", §13 Phase 7).
//!
//! Win32 requires `RegisterHotKey`/`WM_HOTKEY` to be received by the thread
//! that registered the hotkey, via that thread's own message queue — hence a
//! dedicated OS thread with its own `GetMessageW` loop, entirely independent
//! of Slint's UI thread. This module has no Slint dependency; callers that
//! need to update UI state from `on_event` must marshal via
//! `slint::invoke_from_event_loop` themselves (mirroring the one existing
//! precedent for this in `src/app.rs`'s second-instance show-request thread).

use std::sync::mpsc;

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PostThreadMessageW, TranslateMessage, WM_APP, WM_HOTKEY,
};

use crate::domain::config::{HotkeyConfig, HotkeyModifier};

/// This app registers exactly one global hotkey at a time, so a single
/// constant id is enough (PLAN.md doesn't call for multiple simultaneous
/// hotkeys).
const HOTKEY_ID: i32 = 1;
const WM_APP_REBIND: u32 = WM_APP + 1;
const WM_APP_SHUTDOWN: u32 = WM_APP + 2;

/// Maps the app's own hotkey config to Win32's modifier flags + virtual-key
/// code. Always includes `MOD_NOREPEAT`: without it, holding the combo down
/// re-fires `WM_HOTKEY` on Windows' key-repeat cadence, which would rapidly
/// toggle the Quick Switcher open/closed while held.
pub fn to_win32(config: &HotkeyConfig) -> (HOT_KEY_MODIFIERS, u32) {
    let mut mods = MOD_NOREPEAT;
    for modifier in &config.modifiers {
        mods |= match modifier {
            HotkeyModifier::Alt => MOD_ALT,
            HotkeyModifier::Control => MOD_CONTROL,
            HotkeyModifier::Shift => MOD_SHIFT,
            HotkeyModifier::Win => MOD_WIN,
        };
    }
    (mods, config.virtual_key)
}

/// Win32 virtual-key code for a key label shown in the hotkey rebind UI
/// (`"A"`-`"Z"`, `"0"`-`"9"`, `"F1"`-`"F12"`). `A`-`Z`/`0`-`9` virtual-key
/// codes equal their ASCII codes per the Win32 API docs; `F1`-`F12` are
/// `0x70..=0x7B` (`VK_F1`..`VK_F12`).
pub fn key_label_to_virtual_key(label: &str) -> Option<u32> {
    if let Some(f_num) = label.strip_prefix('F').or_else(|| label.strip_prefix('f')) {
        let n: u32 = f_num.parse().ok()?;
        return (1..=12).contains(&n).then(|| 0x70 + (n - 1));
    }

    let mut chars = label.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    match c {
        'A'..='Z' | '0'..='9' => Some(u32::from(c as u8)),
        _ => None,
    }
}

/// Inverse of [`key_label_to_virtual_key`], for displaying the currently
/// saved hotkey in the rebind UI.
pub fn virtual_key_to_label(vk: u32) -> Option<String> {
    if (0x70..=0x7B).contains(&vk) {
        return Some(format!("F{}", vk - 0x70 + 1));
    }
    let in_letter_or_digit_range = (u32::from(b'A')..=u32::from(b'Z')).contains(&vk)
        || (u32::from(b'0')..=u32::from(b'9')).contains(&vk);
    if in_letter_or_digit_range {
        return char::from_u32(vk).map(|c| c.to_string());
    }
    None
}

/// A hotkey registration failure's classification (PLAN.md §13 completion
/// criterion "ホットキー衝突を検出").
#[derive(Debug)]
pub enum HotkeyRegisterError {
    AlreadyRegistered,
    Other(windows::core::Error),
}

#[derive(Debug)]
pub enum HotkeyEvent {
    /// The registered combo was pressed.
    Pressed,
    /// A rebind (or the initial registration) succeeded.
    Registered,
    /// A rebind failed; the thread has already re-registered its previous
    /// combo on its own, so the caller only needs to roll back *persisted*
    /// config to match, not retry registration itself.
    RegisterFailed(HotkeyRegisterError),
}

pub struct HotkeyThread {
    thread_id: u32,
    join_handle: Option<std::thread::JoinHandle<()>>,
}

impl HotkeyThread {
    /// Spawns the dedicated Win32 message-loop thread and attempts the
    /// initial registration.
    pub fn spawn(initial: HotkeyConfig, on_event: impl Fn(HotkeyEvent) + Send + 'static) -> Self {
        let (thread_id_tx, thread_id_rx) = mpsc::sync_channel(1);

        let join_handle = std::thread::spawn(move || {
            // SAFETY: called once, at the very start of this thread's life.
            let thread_id = unsafe { GetCurrentThreadId() };
            let _ = thread_id_tx.send(thread_id);

            let mut current = to_win32(&initial);
            match register_or_classify(current.0, current.1) {
                Ok(()) => on_event(HotkeyEvent::Registered),
                Err(err) => on_event(HotkeyEvent::RegisterFailed(err)),
            }

            run_message_loop(&mut current, &on_event);
            unregister();
        });

        let thread_id = thread_id_rx
            .recv()
            .expect("the hotkey thread must report its thread id before spawn() returns");

        Self {
            thread_id,
            join_handle: Some(join_handle),
        }
    }

    /// Requests a rebind. Delivered asynchronously via the hotkey thread's
    /// own message queue (`RegisterHotKey`/`UnregisterHotKey` must run on the
    /// thread that owns the registering message queue); the result arrives
    /// later as a `HotkeyEvent::Registered`/`RegisterFailed` passed to the
    /// closure given to `spawn`.
    pub fn rebind(&self, new_config: HotkeyConfig) {
        let (mods, vk) = to_win32(&new_config);
        // SAFETY: `self.thread_id` is this struct's own live hotkey thread;
        // wparam/lparam just carry the two plain values `to_win32` produced.
        let _ = unsafe {
            PostThreadMessageW(
                self.thread_id,
                WM_APP_REBIND,
                WPARAM(mods.0 as usize),
                LPARAM(vk as isize),
            )
        };
    }
}

impl Drop for HotkeyThread {
    fn drop(&mut self) {
        // SAFETY: see `rebind`.
        let _ =
            unsafe { PostThreadMessageW(self.thread_id, WM_APP_SHUTDOWN, WPARAM(0), LPARAM(0)) };
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_message_loop(
    current: &mut (HOT_KEY_MODIFIERS, u32),
    on_event: &(impl Fn(HotkeyEvent) + Send + 'static),
) {
    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a valid buffer; `None`/`0`/`0` requests every
        // message posted to this thread's queue (thread-message hotkeys have
        // no associated window).
        if !unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            break; // WM_QUIT or GetMessageW failed.
        }

        match msg.message {
            WM_HOTKEY if msg.wParam.0 == HOTKEY_ID as usize => on_event(HotkeyEvent::Pressed),
            WM_APP_REBIND => {
                let requested = (HOT_KEY_MODIFIERS(msg.wParam.0 as u32), msg.lParam.0 as u32);
                unregister();
                match register_or_classify(requested.0, requested.1) {
                    Ok(()) => {
                        *current = requested;
                        on_event(HotkeyEvent::Registered);
                    }
                    Err(err) => {
                        // Self-heal: re-register the last-known-good combo so
                        // the app never ends up with zero hotkeys registered.
                        let _ = register_or_classify(current.0, current.1);
                        on_event(HotkeyEvent::RegisterFailed(err));
                        on_event(HotkeyEvent::Registered);
                    }
                }
            }
            WM_APP_SHUTDOWN => break,
            _ => {}
        }

        // SAFETY: `msg` was just filled in by `GetMessageW` above.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Registers `(mods, vk)` as this thread's hotkey, classifying a conflict
/// with another app's hotkey separately from any other failure (PLAN.md §13
/// completion criterion "ホットキー衝突を検出"), following the same
/// `GetLastError`-based idiom `acquire_single_instance` (`src/app.rs`) uses.
fn register_or_classify(mods: HOT_KEY_MODIFIERS, vk: u32) -> Result<(), HotkeyRegisterError> {
    use windows::Win32::Foundation::{ERROR_HOTKEY_ALREADY_REGISTERED, GetLastError};

    // SAFETY: `hwnd: None` registers a thread-message hotkey delivered to
    // this thread's own queue; no window is involved.
    if unsafe { RegisterHotKey(None, HOTKEY_ID, mods, vk) }.is_ok() {
        return Ok(());
    }
    // SAFETY: reads thread-local state set by the immediately preceding call.
    if unsafe { GetLastError() } == ERROR_HOTKEY_ALREADY_REGISTERED {
        Err(HotkeyRegisterError::AlreadyRegistered)
    } else {
        Err(HotkeyRegisterError::Other(
            windows::core::Error::from_thread(),
        ))
    }
}

fn unregister() {
    // SAFETY: `hwnd: None` + `HOTKEY_ID` matches whatever `register_or_classify`
    // last successfully registered on this thread; unregistering a hotkey
    // that was never registered is a documented, harmless no-op failure.
    let _ = unsafe { UnregisterHotKey(None, HOTKEY_ID) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_win32_maps_modifiers_and_always_includes_no_repeat() {
        let config = HotkeyConfig {
            modifiers: vec![HotkeyModifier::Control, HotkeyModifier::Alt],
            virtual_key: u32::from(b'R'),
        };

        let (mods, vk) = to_win32(&config);

        assert!(mods.contains(MOD_CONTROL));
        assert!(mods.contains(MOD_ALT));
        assert!(mods.contains(MOD_NOREPEAT));
        assert!(!mods.contains(MOD_SHIFT));
        assert!(!mods.contains(MOD_WIN));
        assert_eq!(vk, u32::from(b'R'));
    }

    #[test]
    fn to_win32_with_no_modifiers_still_includes_no_repeat() {
        let config = HotkeyConfig {
            modifiers: vec![],
            virtual_key: u32::from(b'A'),
        };

        let (mods, _) = to_win32(&config);
        assert!(mods.contains(MOD_NOREPEAT));
    }

    #[test]
    fn key_label_round_trips_for_letters_digits_and_function_keys() {
        for label in ["A", "Z", "0", "9", "F1", "F12"] {
            let vk =
                key_label_to_virtual_key(label).unwrap_or_else(|| panic!("{label} should map"));
            assert_eq!(virtual_key_to_label(vk).as_deref(), Some(label));
        }
    }

    #[test]
    fn key_label_rejects_garbage_input() {
        assert_eq!(key_label_to_virtual_key(""), None);
        assert_eq!(key_label_to_virtual_key("Ctrl"), None);
        assert_eq!(key_label_to_virtual_key("F13"), None);
        assert_eq!(key_label_to_virtual_key("F0"), None);
        assert_eq!(key_label_to_virtual_key("AB"), None);
    }

    #[test]
    fn virtual_key_to_label_rejects_out_of_range_codes() {
        assert_eq!(virtual_key_to_label(0x01), None);
    }
}
