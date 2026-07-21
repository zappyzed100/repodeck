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

/// The primary Quick Switcher toggle hotkey.
const HOTKEY_ID: i32 = 1;
/// Alt+Tab-style "hold the modifiers, tap an arrow to cycle" hotkeys: the
/// same modifiers as [`HOTKEY_ID`] combined with Down/Up. Registered
/// best-effort (only when the main hotkey has a modifier), so a conflict with
/// another app's Ctrl+Alt+Arrow just disables cycling rather than failing.
const CYCLE_NEXT_HOTKEY_ID: i32 = 2;
const CYCLE_PREV_HOTKEY_ID: i32 = 3;

/// `VK_UP` / `VK_DOWN` — the arrow keys the cycle hotkeys bind to.
const VK_UP: u32 = 0x26;
const VK_DOWN: u32 = 0x28;

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

/// `(Win32 virtual-key, display label, Slint key text)` for every special
/// (non-alphanumeric, non-function) key the hotkey capture UI supports.
/// Slint delivers special keys as single characters — the codepoints of its
/// `Key.*` constants (the macOS `NSEvent` function-key private-use area,
/// e.g. `Key.UpArrow` = `'\u{F700}'`, plus a few ASCII control codes). A
/// unit test below cross-checks every entry against `slint::platform::Key`
/// so this table can't silently drift from the toolkit.
const SPECIAL_KEYS: &[(u32, &str, char)] = &[
    (0x09, "Tab", '\u{0009}'),      // VK_TAB / Key.Tab
    (0x20, "Space", '\u{0020}'),    // VK_SPACE / Key.Space
    (0x21, "PageUp", '\u{F72C}'),   // VK_PRIOR / Key.PageUp
    (0x22, "PageDown", '\u{F72D}'), // VK_NEXT / Key.PageDown
    (0x23, "End", '\u{F72B}'),      // VK_END / Key.End
    (0x24, "Home", '\u{F729}'),     // VK_HOME / Key.Home
    (0x25, "Left", '\u{F702}'),     // VK_LEFT / Key.LeftArrow
    (0x26, "Up", '\u{F700}'),       // VK_UP / Key.UpArrow
    (0x27, "Right", '\u{F703}'),    // VK_RIGHT / Key.RightArrow
    (0x28, "Down", '\u{F701}'),     // VK_DOWN / Key.DownArrow
    (0x2D, "Insert", '\u{F727}'),   // VK_INSERT / Key.Insert
    (0x2E, "Delete", '\u{007F}'),   // VK_DELETE / Key.Delete
];

/// Slint's `Key.F1`..`Key.F24` codepoints (`i-slint-common`'s
/// `for_each_keys!` table) and the matching `VK_F1`..`VK_F24` range.
const SLINT_F1: char = '\u{F704}';
const SLINT_F24: char = '\u{F71B}';
const VK_F1: u32 = 0x70;
const VK_F24: u32 = 0x87;

/// Win32 virtual-key code for the `event.text` of a Slint `KeyEvent`, as
/// captured by the settings window's press-to-record hotkey UI (PLAN.md §13
/// Phase 7, "ホットキー設定UI"). Handles letters (case-insensitively —
/// Slint reports `"a"` unshifted, `"A"` shifted), digits, `F1`-`F24`, and
/// the special keys in [`SPECIAL_KEYS`]; anything else (Enter, punctuation,
/// IME output, …) maps to `None` and is rejected by the capture UI.
pub fn slint_key_text_to_virtual_key(text: &str) -> Option<u32> {
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }

    if let Some(&(vk, _, _)) = SPECIAL_KEYS.iter().find(|(_, _, slint)| *slint == c) {
        return Some(vk);
    }
    if (SLINT_F1..=SLINT_F24).contains(&c) {
        return Some(VK_F1 + (c as u32 - SLINT_F1 as u32));
    }
    match c {
        'a'..='z' => Some(u32::from(c as u8 - b'a' + b'A')),
        'A'..='Z' | '0'..='9' => Some(u32::from(c as u8)),
        _ => None,
    }
}

/// Win32 virtual-key code for a human-readable key label as produced by
/// [`virtual_key_to_label`] (`"A"`-`"Z"`, `"0"`-`"9"`, `"F1"`-`"F24"`,
/// `"Up"`, `"Space"`, …). `A`-`Z`/`0`-`9` virtual-key codes equal their
/// ASCII codes per the Win32 API docs; `F1`-`F24` are `0x70..=0x87`
/// (`VK_F1`..`VK_F24`).
pub fn key_label_to_virtual_key(label: &str) -> Option<u32> {
    if let Some(&(vk, _, _)) = SPECIAL_KEYS.iter().find(|(_, name, _)| *name == label) {
        return Some(vk);
    }
    if let Some(f_num) = label.strip_prefix('F').or_else(|| label.strip_prefix('f'))
        && let Ok(n) = f_num.parse::<u32>()
    {
        return (1..=24).contains(&n).then(|| VK_F1 + (n - 1));
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
/// saved hotkey in the settings window (e.g. the `"Up"` in `"Ctrl+Alt+Up"`).
pub fn virtual_key_to_label(vk: u32) -> Option<String> {
    if (VK_F1..=VK_F24).contains(&vk) {
        return Some(format!("F{}", vk - VK_F1 + 1));
    }
    if let Some(&(_, name, _)) = SPECIAL_KEYS
        .iter()
        .find(|(candidate, _, _)| *candidate == vk)
    {
        return Some(name.to_string());
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
    /// A cycle hotkey (main modifiers + Down/Up) was pressed. `forward` is
    /// true for Down (next), false for Up (previous). The caller shows the
    /// Quick Switcher, moves the selection, and commits it once the modifiers
    /// are released (that release is detected UI-side, not here).
    CyclePressed { forward: bool },
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
            match register_all(current.0, current.1) {
                Ok(()) => on_event(HotkeyEvent::Registered),
                Err(err) => on_event(HotkeyEvent::RegisterFailed(err)),
            }

            run_message_loop(&mut current, &on_event);
            unregister_all();
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
            WM_HOTKEY => match msg.wParam.0 as i32 {
                HOTKEY_ID => on_event(HotkeyEvent::Pressed),
                CYCLE_NEXT_HOTKEY_ID => on_event(HotkeyEvent::CyclePressed { forward: true }),
                CYCLE_PREV_HOTKEY_ID => on_event(HotkeyEvent::CyclePressed { forward: false }),
                _ => {}
            },
            WM_APP_REBIND => {
                let requested = (HOT_KEY_MODIFIERS(msg.wParam.0 as u32), msg.lParam.0 as u32);
                unregister_all();
                match register_all(requested.0, requested.1) {
                    Ok(()) => {
                        *current = requested;
                        on_event(HotkeyEvent::Registered);
                    }
                    Err(err) => {
                        // Self-heal: re-register the last-known-good combo so
                        // the app never ends up with zero hotkeys registered.
                        let _ = register_all(current.0, current.1);
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

/// Whether `mods` carries a real modifier (ignoring `MOD_NOREPEAT`), which is
/// what makes an Alt+Tab-style "hold modifiers, tap arrow" cycle possible.
fn has_real_modifier(mods: HOT_KEY_MODIFIERS) -> bool {
    (mods & (MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN)).0 != 0
}

/// Registers the main hotkey plus, best-effort, the two cycle hotkeys
/// (main modifiers + Down/Up). Only the main hotkey's failure is reported —
/// the cycle hotkeys sharing the same modifiers may legitimately collide with
/// another app (e.g. a GPU driver's Ctrl+Alt+Arrow), in which case cycling is
/// simply unavailable this session.
fn register_all(mods: HOT_KEY_MODIFIERS, vk: u32) -> Result<(), HotkeyRegisterError> {
    register_or_classify(mods, vk)?;
    if has_real_modifier(mods) {
        // Skip the arrow whose combo would duplicate the main hotkey itself.
        if vk != VK_DOWN {
            // SAFETY: thread-message hotkey on this thread's own queue.
            let _ = unsafe { RegisterHotKey(None, CYCLE_NEXT_HOTKEY_ID, mods, VK_DOWN) };
        }
        if vk != VK_UP {
            // SAFETY: as above.
            let _ = unsafe { RegisterHotKey(None, CYCLE_PREV_HOTKEY_ID, mods, VK_UP) };
        }
    }
    Ok(())
}

fn unregister_all() {
    // SAFETY: `hwnd: None` + each id matches whatever `register_all` last
    // registered on this thread; unregistering an id that was never
    // registered is a documented, harmless no-op failure.
    for id in [HOTKEY_ID, CYCLE_NEXT_HOTKEY_ID, CYCLE_PREV_HOTKEY_ID] {
        let _ = unsafe { UnregisterHotKey(None, id) };
    }
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
        for label in ["A", "F", "Z", "0", "9", "F1", "F12", "F13", "F24"] {
            let vk =
                key_label_to_virtual_key(label).unwrap_or_else(|| panic!("{label} should map"));
            assert_eq!(virtual_key_to_label(vk).as_deref(), Some(label));
        }
    }

    #[test]
    fn key_label_round_trips_for_special_keys() {
        for label in [
            "Up", "Down", "Left", "Right", "Space", "Tab", "Home", "End", "PageUp", "PageDown",
            "Insert", "Delete",
        ] {
            let vk =
                key_label_to_virtual_key(label).unwrap_or_else(|| panic!("{label} should map"));
            assert_eq!(virtual_key_to_label(vk).as_deref(), Some(label));
        }
    }

    #[test]
    fn key_label_rejects_garbage_input() {
        assert_eq!(key_label_to_virtual_key(""), None);
        assert_eq!(key_label_to_virtual_key("Ctrl"), None);
        assert_eq!(key_label_to_virtual_key("F25"), None);
        assert_eq!(key_label_to_virtual_key("F0"), None);
        assert_eq!(key_label_to_virtual_key("AB"), None);
    }

    #[test]
    fn virtual_key_to_label_rejects_out_of_range_codes() {
        assert_eq!(virtual_key_to_label(0x01), None);
    }

    #[test]
    fn slint_key_text_maps_letters_case_insensitively_and_digits() {
        assert_eq!(slint_key_text_to_virtual_key("a"), Some(u32::from(b'A')));
        assert_eq!(slint_key_text_to_virtual_key("A"), Some(u32::from(b'A')));
        assert_eq!(slint_key_text_to_virtual_key("z"), Some(u32::from(b'Z')));
        assert_eq!(slint_key_text_to_virtual_key("0"), Some(u32::from(b'0')));
        assert_eq!(slint_key_text_to_virtual_key("9"), Some(u32::from(b'9')));
    }

    /// Guards [`SPECIAL_KEYS`]' and the F-key range's hard-coded codepoints
    /// against drift from the toolkit: builds each `event.text` exactly the
    /// way Slint would (from `slint::platform::Key`) and checks it maps to
    /// the intended Win32 virtual-key code.
    #[test]
    fn slint_key_text_maps_special_keys_matching_slint_key_constants() {
        use slint::platform::Key;

        let cases: [(Key, u32, &str); 16] = [
            (Key::Tab, 0x09, "Tab"),
            (Key::Space, 0x20, "Space"),
            (Key::PageUp, 0x21, "PageUp"),
            (Key::PageDown, 0x22, "PageDown"),
            (Key::End, 0x23, "End"),
            (Key::Home, 0x24, "Home"),
            (Key::LeftArrow, 0x25, "Left"),
            (Key::UpArrow, 0x26, "Up"),
            (Key::RightArrow, 0x27, "Right"),
            (Key::DownArrow, 0x28, "Down"),
            (Key::Insert, 0x2D, "Insert"),
            (Key::Delete, 0x2E, "Delete"),
            (Key::F1, 0x70, "F1"),
            (Key::F12, 0x7B, "F12"),
            (Key::F13, 0x7C, "F13"),
            (Key::F24, 0x87, "F24"),
        ];
        for (key, vk, label) in cases {
            let text = char::from(key).to_string();
            assert_eq!(
                slint_key_text_to_virtual_key(&text),
                Some(vk),
                "{label} should map to VK 0x{vk:02X}"
            );
            assert_eq!(virtual_key_to_label(vk).as_deref(), Some(label));
        }
    }

    #[test]
    fn slint_key_text_rejects_unmappable_keys() {
        use slint::platform::Key;

        for key in [Key::Return, Key::Escape, Key::Backspace, Key::Shift] {
            let text = char::from(key).to_string();
            assert_eq!(slint_key_text_to_virtual_key(&text), None, "{key:?}");
        }
        assert_eq!(slint_key_text_to_virtual_key(""), None);
        assert_eq!(slint_key_text_to_virtual_key("ab"), None);
        assert_eq!(slint_key_text_to_virtual_key("!"), None);
    }
}
