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

use repodeck::domain::placement::PixelRect;
use repodeck::windowing::{enumerate, placement};
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
