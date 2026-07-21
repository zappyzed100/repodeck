//! Manual, local-only Windows E2E checks (PLAN.md §14.2, §14.5).
//!
//! These tests drive real top-level windows and are intentionally excluded from
//! the default `cargo test` gate (`#[ignore]`) since they require an interactive
//! desktop session and are not meant to run in CI. Run explicitly with:
//!
//! ```powershell
//! cargo test --test windows_e2e -- --ignored --test-threads=1
//! ```

use std::collections::HashSet;
use std::process::Command;
use std::time::{Duration, Instant};

use repodeck::application::switch_coordinator::{SwitchCoordinator, SwitchRequest};
use repodeck::application::workset_service;
use repodeck::domain::placement::{PixelRect, SavedShowState};
use repodeck::domain::workset::{ParkingPolicy, RepositoryKind};
use repodeck::windowing::window_ops_impl::Win32WindowOps;
use repodeck::windowing::{enumerate, monitor, placement};
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

/// Force-closes the real window-owning process by PID (PLAN.md's "never close the
/// target app" rule governs RepoDeck's production behavior on the *user's* windows;
/// it does not apply to throwaway windows this test harness spawns and must clean
/// up itself, matching the synthetic-window test harness described in PLAN.md §14.2).
struct SpawnedWindowGuard {
    process_id: u32,
}

impl Drop for SpawnedWindowGuard {
    fn drop(&mut self) {
        // SAFETY: `self.process_id` was read from a live `EnumWindows` result when
        // this guard was created; a process that has already exited is a benign,
        // expected `OpenProcess` failure here, not a safety issue.
        unsafe {
            if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, self.process_id) {
                let _ = TerminateProcess(handle, 0);
                let _ = CloseHandle(handle);
            }
        }
    }
}

/// Windows 11's packaged Notepad is launched through an App Execution Alias:
/// `Command::new("notepad.exe").spawn()`'s PID does not match the real
/// window-owning process, so this diffs the top-level window list before/after
/// spawning and identifies the new window by class/title instead of PID.
///
/// The spawned `Child` is intentionally never `wait()`-ed on: it is the alias
/// stub, not the real window-owning process, and `SpawnedWindowGuard::drop`
/// terminates the real process by the PID discovered via `EnumWindows`.
#[allow(clippy::zombie_processes)]
fn spawn_notepad_hwnd() -> (SpawnedWindowGuard, HWND) {
    let before: HashSet<isize> = enumerate::enumerate_top_level_windows(0)
        .expect("EnumWindows should succeed")
        .into_iter()
        .map(|w| w.hwnd)
        .collect();

    let _ = Command::new("notepad.exe")
        .spawn()
        .expect("failed to spawn notepad.exe");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let windows =
            enumerate::enumerate_top_level_windows(0).expect("EnumWindows should succeed");
        let found = windows.into_iter().find(|w| {
            !before.contains(&w.hwnd)
                && (w.window_class.to_ascii_lowercase().contains("notepad")
                    || w.title.contains("Notepad")
                    || w.title.contains("メモ帳"))
        });

        if let Some(window) = found {
            return (
                SpawnedWindowGuard {
                    process_id: window.process_id,
                },
                HWND(window.hwnd as *mut _),
            );
        }

        assert!(
            Instant::now() < deadline,
            "no new Notepad window appeared within 10s"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// PLAN.md Phase 2 completion condition: move a real window between two placements
/// 100 times and confirm cumulative drift stays within 2px.
#[test]
#[ignore = "drives a real desktop window; run manually, not in CI"]
fn round_trip_a_real_window_100_times_within_2px_drift() {
    let (_guard, hwnd) = spawn_notepad_hwnd();

    let rect_a = PixelRect::new(50, 50, 640, 480);
    let rect_b = PixelRect::new(300, 200, 800, 600);

    placement::restore(hwnd);
    std::thread::sleep(Duration::from_millis(100));

    for i in 0..100 {
        let target = if i % 2 == 0 { rect_a } else { rect_b };
        placement::set_window_rect(hwnd, target).expect("set_window_rect should succeed");
    }

    std::thread::sleep(Duration::from_millis(100));
    let final_rect = placement::get_normal_rect(hwnd).expect("get_normal_rect should succeed");

    // 100 iterations end on i=99 (odd) -> rect_b.
    assert!(
        (final_rect.x - rect_b.x).abs() <= 2,
        "x drift: {} vs {}",
        final_rect.x,
        rect_b.x
    );
    assert!(
        (final_rect.y - rect_b.y).abs() <= 2,
        "y drift: {} vs {}",
        final_rect.y,
        rect_b.y
    );
    assert!(
        (final_rect.width - rect_b.width).abs() <= 2,
        "width drift: {} vs {}",
        final_rect.width,
        rect_b.width
    );
    assert!(
        (final_rect.height - rect_b.height).abs() <= 2,
        "height drift: {} vs {}",
        final_rect.height,
        rect_b.height
    );
}

/// PLAN.md Phase 2 test: a maximized window's *normal* placement can still be read.
#[test]
#[ignore = "drives a real desktop window; run manually, not in CI"]
fn normal_rect_is_readable_while_window_is_maximized() {
    let (_guard, hwnd) = spawn_notepad_hwnd();

    let rect = PixelRect::new(80, 80, 700, 500);
    placement::restore(hwnd);
    std::thread::sleep(Duration::from_millis(100));
    placement::set_window_rect(hwnd, rect).expect("set_window_rect should succeed");
    std::thread::sleep(Duration::from_millis(100));

    placement::maximize(hwnd);
    std::thread::sleep(Duration::from_millis(200));

    let state = placement::get_show_state(hwnd).expect("get_show_state should succeed");
    assert_eq!(
        state,
        repodeck::domain::placement::SavedShowState::Maximized
    );

    let normal_rect = placement::get_normal_rect(hwnd).expect("get_normal_rect should succeed");
    assert!((normal_rect.x - rect.x).abs() <= 2);
    assert!((normal_rect.y - rect.y).abs() <= 2);
    assert!((normal_rect.width - rect.width).abs() <= 2);
    assert!((normal_rect.height - rect.height).abs() <= 2);
}

/// PLAN.md §4.5: a batch of windows moves atomically via `BeginDeferWindowPos`.
#[test]
#[ignore = "drives real desktop windows; run manually, not in CI"]
fn batch_move_places_two_windows_in_one_commit() {
    let (_guard_a, hwnd_a) = spawn_notepad_hwnd();
    let (_guard_b, hwnd_b) = spawn_notepad_hwnd();

    placement::restore(hwnd_a);
    placement::restore(hwnd_b);
    std::thread::sleep(Duration::from_millis(100));

    let rect_a = PixelRect::new(40, 40, 500, 400);
    let rect_b = PixelRect::new(600, 40, 500, 400);

    placement::BatchMove::begin(2)
        .expect("BeginDeferWindowPos should succeed")
        .defer(hwnd_a, rect_a)
        .expect("DeferWindowPos(a) should succeed")
        .defer(hwnd_b, rect_b)
        .expect("DeferWindowPos(b) should succeed")
        .commit()
        .expect("EndDeferWindowPos should succeed");

    std::thread::sleep(Duration::from_millis(100));

    let final_a = placement::get_normal_rect(hwnd_a).unwrap();
    let final_b = placement::get_normal_rect(hwnd_b).unwrap();

    assert!((final_a.x - rect_a.x).abs() <= 2 && (final_a.y - rect_a.y).abs() <= 2);
    assert!((final_b.x - rect_b.x).abs() <= 2 && (final_b.y - rect_b.y).abs() <= 2);
}

/// PLAN.md §3.8 end-to-end: switching to a second real workset minimizes the
/// one that stops being current. Deliberately restricted to a single
/// (synthetic single-element) monitor list so the outcome is deterministic
/// regardless of how many real monitors this machine actually has — real
/// multi-monitor parking scenarios are covered by `switch_coordinator`'s own
/// unit tests (`FakeWindowOps`), not here (PLAN.md §14.2).
#[test]
#[ignore = "drives real desktop windows; run manually, not in CI"]
fn switch_between_two_real_worksets_minimizes_the_non_current_one() {
    let (_guard_a, hwnd_a) = spawn_notepad_hwnd();
    let (_guard_b, hwnd_b) = spawn_notepad_hwnd();

    placement::restore(hwnd_a);
    placement::restore(hwnd_b);
    placement::set_window_rect(hwnd_a, PixelRect::new(50, 50, 640, 480))
        .expect("set_window_rect(a) should succeed");
    placement::set_window_rect(hwnd_b, PixelRect::new(50, 50, 640, 480))
        .expect("set_window_rect(b) should succeed");
    std::thread::sleep(Duration::from_millis(200));

    let all_monitors = monitor::enumerate_monitors().expect("enumerate_monitors should succeed");
    let primary = all_monitors
        .iter()
        .find(|m| m.is_primary)
        .or_else(|| all_monitors.first())
        .cloned()
        .expect("at least one monitor must be present");
    let live_monitors = vec![primary.clone()];
    let main_monitor_ids = vec![primary.device_name.clone()];

    let live_windows =
        enumerate::enumerate_top_level_windows(0).expect("EnumWindows should succeed");
    let window_a = live_windows
        .iter()
        .find(|w| w.hwnd == hwnd_a.0 as isize)
        .expect("window a must be enumerable")
        .clone();
    let window_b = live_windows
        .iter()
        .find(|w| w.hwnd == hwnd_b.0 as isize)
        .expect("window b must be enumerable")
        .clone();

    let managed_a = workset_service::build_managed_window(
        &window_a,
        window_a.rect_px,
        SavedShowState::Normal,
        0,
        &live_monitors,
        &main_monitor_ids,
    )
    .expect("window a should resolve onto the (synthetic) main monitor");
    let managed_b = workset_service::build_managed_window(
        &window_b,
        window_b.rect_px,
        SavedShowState::Normal,
        0,
        &live_monitors,
        &main_monitor_ids,
    )
    .expect("window b should resolve onto the (synthetic) main monitor");

    let mut workset_a = workset_service::build_workset(
        "e2e-a".to_string(),
        "#000000".to_string(),
        std::path::PathBuf::from("C:\\repodeck-e2e-a"),
        RepositoryKind::Directory,
        0,
        vec![managed_a],
    );
    workset_a.parking_policy = ParkingPolicy::Auto;
    let mut workset_b = workset_service::build_workset(
        "e2e-b".to_string(),
        "#000000".to_string(),
        std::path::PathBuf::from("C:\\repodeck-e2e-b"),
        RepositoryKind::Directory,
        1,
        vec![managed_b],
    );
    workset_b.parking_policy = ParkingPolicy::Auto;
    let worksets = vec![workset_a.clone(), workset_b.clone()];

    let data_dir = tempfile::tempdir().expect("tempdir should succeed");
    let coordinator = SwitchCoordinator::new(Win32WindowOps, data_dir.path().to_path_buf());

    coordinator
        .switch_to(SwitchRequest {
            worksets: &worksets,
            fixed_slots: &[],
            saved_monitors: &[],
            main_monitor_ids: &main_monitor_ids,
            live_monitors: &live_monitors,
            live_windows: &live_windows,
            target_workset_id: workset_a.id,
        })
        .expect("switch to workset a should succeed");
    std::thread::sleep(Duration::from_millis(200));

    coordinator
        .switch_to(SwitchRequest {
            worksets: &worksets,
            fixed_slots: &[],
            saved_monitors: &[],
            main_monitor_ids: &main_monitor_ids,
            live_monitors: &live_monitors,
            live_windows: &live_windows,
            target_workset_id: workset_b.id,
        })
        .expect("switch to workset b should succeed");
    std::thread::sleep(Duration::from_millis(200));

    // No non-main monitor is available (only the primary is in `live_monitors`),
    // so `a` (now non-current) has nowhere to auto-park and must be minimized.
    let state_a = placement::get_show_state(hwnd_a).expect("get_show_state(a) should succeed");
    assert_eq!(state_a, SavedShowState::Minimized);

    let state_b = placement::get_show_state(hwnd_b).expect("get_show_state(b) should succeed");
    assert_eq!(state_b, SavedShowState::Normal);
}

/// PLAN.md §13 Phase 7 completion criterion "ホットキー衝突を検出": occupies a
/// combo on this thread first, then confirms `HotkeyThread` (registering the
/// same combo on its own dedicated thread) observes
/// `RegisterFailed(AlreadyRegistered)` — `RegisterHotKey` conflicts are
/// system-wide regardless of which thread/process registered first, so this
/// needs no Slint window at all, unlike the other tests in this file.
#[test]
#[ignore = "registers a real global hotkey; run manually, not in CI"]
fn hotkey_thread_reports_already_registered_when_the_combo_is_taken() {
    use std::sync::{Arc, Mutex};

    use repodeck::domain::config::{HotkeyConfig, HotkeyModifier};
    use repodeck::hotkey::win32_hotkey::{HotkeyEvent, HotkeyRegisterError, HotkeyThread};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
    };

    const OCCUPYING_HOTKEY_ID: i32 = 999;
    // An obscure combo (all four modifiers + F24) rather than RepoDeck's own
    // default Ctrl+Alt+R: a real running RepoDeck instance (or some other
    // unrelated app) may already hold Ctrl+Alt+R on this machine, which would
    // make this test's own "occupy the combo" setup step fail before it gets
    // to the thing being tested.
    const TEST_VK: u32 = 0x87; // VK_F24
    let config = HotkeyConfig {
        modifiers: vec![
            HotkeyModifier::Control,
            HotkeyModifier::Alt,
            HotkeyModifier::Shift,
            HotkeyModifier::Win,
        ],
        virtual_key: TEST_VK,
    };

    // Occupy the combo on this (test-runner) thread first.
    let occupying_mods = MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_WIN | MOD_NOREPEAT;
    unsafe { RegisterHotKey(None, OCCUPYING_HOTKEY_ID, occupying_mods, TEST_VK) }
        .expect("failed to occupy the test hotkey combo");

    let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_thread = events.clone();
    let _hotkey_thread = HotkeyThread::spawn(config, move |event| {
        let label = match event {
            HotkeyEvent::Pressed => "pressed",
            HotkeyEvent::Registered => "registered",
            HotkeyEvent::RegisterFailed(HotkeyRegisterError::AlreadyRegistered) => {
                "already_registered"
            }
            HotkeyEvent::RegisterFailed(HotkeyRegisterError::Other(_)) => "other_error",
        };
        events_for_thread.lock().unwrap().push(label);
    });

    std::thread::sleep(Duration::from_millis(300));

    // SAFETY: `OCCUPYING_HOTKEY_ID` was registered on this same thread above.
    let _ = unsafe { UnregisterHotKey(None, OCCUPYING_HOTKEY_ID) };

    let events = events.lock().unwrap();
    assert!(
        events.contains(&"already_registered"),
        "expected an already_registered event, got {events:?}"
    );
}

/// PLAN.md §6.4's "RepoDeck未起動でも200ms以内に終了コード0": with no
/// `NamedPipeServer` listening at all, `repodeck-hook.exe` must still exit 0
/// quickly (`CreateFileW(OPEN_EXISTING, ...)` fails immediately with no
/// `WaitNamedPipeW` retry, per its own doc comment).
#[test]
#[ignore = "spawns a real subprocess; run manually, not in CI"]
fn repodeck_hook_exits_zero_when_no_pipe_server_is_listening() {
    use std::io::Write;
    use std::process::Stdio;

    let payload = serde_json::json!({
        "session_id": "e2e-no-server",
        "turn_id": "t",
        "cwd": r"C:\repo",
        "hook_event_name": "Stop",
    })
    .to_string();

    let mut child = Command::new(env!("CARGO_BIN_EXE_repodeck-hook"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn repodeck-hook.exe");
    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(payload.as_bytes())
        .expect("failed to write to repodeck-hook.exe's stdin");

    let start = Instant::now();
    let status = child.wait().expect("failed to wait on repodeck-hook.exe");
    let elapsed = start.elapsed();

    assert!(status.success(), "expected exit code 0, got {status:?}");
    assert!(
        elapsed < Duration::from_secs(2),
        "repodeck-hook.exe took too long with no server listening: {elapsed:?}"
    );
}

/// Full pipe round-trip (PLAN.md §6.4/§9.1): a real `NamedPipeServer`
/// receives a real `repodeck-hook.exe` subprocess's forwarded message, and
/// the bytes deserialize to the expected `NormalizedEvent`.
#[test]
#[ignore = "spawns a real subprocess and binds a real named pipe; run manually, not in CI"]
fn named_pipe_server_receives_a_real_hook_process_event() {
    use std::io::Write;
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};

    use repodeck::ipc::named_pipe::{NamedPipeServer, PipeServerEvent};
    use repodeck::ipc::protocol::{NormalizedEvent, NormalizedEventKind};

    let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let received_for_server = received.clone();
    let _server = NamedPipeServer::spawn(move |event| {
        if let PipeServerEvent::MessageReceived(bytes) = event {
            received_for_server.lock().unwrap().push(bytes);
        }
    })
    .expect("failed to start the named pipe server");

    // Give the server a moment to reach its first `ConnectNamedPipe` wait.
    std::thread::sleep(Duration::from_millis(100));

    let payload = serde_json::json!({
        "session_id": "e2e-round-trip",
        "turn_id": "t1",
        "cwd": r"C:\repo\guardrails-kit",
        "hook_event_name": "Stop",
        "model": "gpt-e2e",
    })
    .to_string();

    let mut child = Command::new(env!("CARGO_BIN_EXE_repodeck-hook"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn repodeck-hook.exe");
    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(payload.as_bytes())
        .expect("failed to write to repodeck-hook.exe's stdin");
    let status = child.wait().expect("failed to wait on repodeck-hook.exe");
    assert!(status.success(), "expected exit code 0, got {status:?}");

    let deadline = Instant::now() + Duration::from_secs(5);
    let bytes = loop {
        if let Some(bytes) = received.lock().unwrap().first().cloned() {
            break bytes;
        }
        assert!(
            Instant::now() < deadline,
            "named pipe server never received the hook process's message"
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    let event: NormalizedEvent = serde_json::from_slice(&bytes)
        .expect("received bytes should deserialize as NormalizedEvent");
    assert_eq!(event.event, NormalizedEventKind::RunCompleted);
    assert_eq!(event.session_id, "e2e-round-trip");
    assert_eq!(event.turn_id, "t1");
    assert_eq!(event.model.as_deref(), Some("gpt-e2e"));
    assert!(event.cwd.ends_with("guardrails-kit"));
}

/// Cleans up the real `RepoDeck` autostart registry value on drop, so a
/// failed assertion mid-test never leaves a stray entry on the dev machine
/// (PLAN.md §11: "管理者権限を要求しない" — this touches `HKEY_CURRENT_USER`
/// only, no elevation involved).
struct AutostartGuard;

impl Drop for AutostartGuard {
    fn drop(&mut self) {
        let _ = repodeck::windowing::autostart::set_enabled(false);
    }
}

/// PLAN.md §13 Phase 9's "起動時自動実行設定": a real round-trip against the
/// per-user `Run` registry key.
#[test]
#[ignore = "writes a real registry value under HKEY_CURRENT_USER; run manually, not in CI"]
fn autostart_enable_disable_round_trips_against_the_real_registry() {
    use repodeck::windowing::autostart;

    let _guard = AutostartGuard;

    // Start from a known state in case a previous failed run left a value.
    autostart::set_enabled(false).expect("failed to clear any pre-existing autostart value");
    assert!(!autostart::is_enabled().expect("is_enabled should succeed"));

    autostart::set_enabled(true).expect("failed to enable autostart");
    assert!(autostart::is_enabled().expect("is_enabled should succeed"));

    autostart::set_enabled(false).expect("failed to disable autostart");
    assert!(!autostart::is_enabled().expect("is_enabled should succeed"));
}
