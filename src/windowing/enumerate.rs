//! Top-level window enumeration with the exclusion filters from PLAN.md §5.1-§5.2.

use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, MAX_PATH};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GA_ROOT, GWL_EXSTYLE, GetAncestor, GetClassNameW, GetWindowLongPtrW,
    GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};
use windows::core::BOOL;

use crate::domain::placement::PixelRect;
use crate::windowing::win32_error::WindowError;

/// Minimum size (physical pixels) for a window to be a management candidate (PLAN.md §5.2).
pub const MIN_CANDIDATE_WIDTH: i32 = 80;
pub const MIN_CANDIDATE_HEIGHT: i32 = 60;

const EXCLUDED_CLASSES: &[&str] = &["Shell_TrayWnd", "Progman", "WorkerW"];

#[derive(Debug, Clone)]
pub struct TopLevelWindow {
    pub hwnd: isize,
    pub process_id: u32,
    pub executable_path: Option<PathBuf>,
    pub window_class: String,
    pub title: String,
    pub rect_px: PixelRect,
}

/// Enumerates visible top-level windows, applying the structural exclusion rules
/// from PLAN.md §5.2 that do not depend on registered workset data (self-process
/// exclusion, non-root windows, tool windows, known shell classes, minimum size,
/// cloaked windows, empty title). Callers apply any remaining, registration-aware
/// filtering (e.g. RepoDeck's own process id) on top of this.
pub fn enumerate_top_level_windows(
    exclude_process_id: u32,
) -> Result<Vec<TopLevelWindow>, WindowError> {
    let mut windows: Vec<TopLevelWindow> = Vec::new();
    let mut ctx = EnumContext {
        windows: &mut windows,
        exclude_process_id,
    };

    // SAFETY: `enum_windows_proc` only touches `*ctx` through the raw pointer
    // passed via `lparam`, and `EnumWindows` calls it synchronously on this
    // thread before returning, so the borrow does not outlive this call.
    let ok = unsafe {
        EnumWindows(
            Some(enum_windows_proc),
            LPARAM(std::ptr::from_mut(&mut ctx) as isize),
        )
    };

    if let Err(source) = ok {
        return Err(WindowError::win32("EnumWindows", source));
    }

    Ok(windows)
}

struct EnumContext<'a> {
    windows: &'a mut Vec<TopLevelWindow>,
    exclude_process_id: u32,
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` was set by `enumerate_top_level_windows` to a live
    // `&mut EnumContext` for the duration of the `EnumWindows` call.
    let ctx = unsafe { &mut *(lparam.0 as *mut EnumContext) };

    if let Some(window) = describe_candidate(hwnd, ctx.exclude_process_id) {
        ctx.windows.push(window);
    }

    BOOL(1)
}

fn describe_candidate(hwnd: HWND, exclude_process_id: u32) -> Option<TopLevelWindow> {
    // SAFETY: `hwnd` is a live handle supplied by `EnumWindows` for this call.
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return None;
    }

    // SAFETY: `hwnd` is a live handle; `GA_ROOT` requires no additional invariants.
    if unsafe { GetAncestor(hwnd, GA_ROOT) } != hwnd {
        return None;
    }

    // SAFETY: `hwnd` is a live handle.
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let is_tool_window = (ex_style & WS_EX_TOOLWINDOW.0) != 0;
    let is_app_window = (ex_style & WS_EX_APPWINDOW.0) != 0;
    if is_tool_window && !is_app_window {
        return None;
    }

    let window_class = get_class_name(hwnd);
    if EXCLUDED_CLASSES.contains(&window_class.as_str()) {
        return None;
    }

    if is_cloaked(hwnd) {
        return None;
    }

    let title = get_window_text(hwnd);
    if title.is_empty() {
        return None;
    }

    // A minimized window has a tiny off-screen frame (≈ -32000). Keep it in the
    // list anyway — otherwise a window parked-to-minimized by an earlier switch
    // can never be matched and brought back when its own set is activated
    // (2026-07-23). Use its restore rect so it isn't dropped as "too small".
    let minimized = unsafe { IsIconic(hwnd) }.as_bool();
    let rect_px = if minimized {
        crate::windowing::placement::get_normal_rect(hwnd)
            .ok()
            .or_else(|| get_window_rect(hwnd))?
    } else {
        get_window_rect(hwnd)?
    };
    if !minimized && (rect_px.width < MIN_CANDIDATE_WIDTH || rect_px.height < MIN_CANDIDATE_HEIGHT)
    {
        return None;
    }

    let mut process_id = 0u32;
    // SAFETY: `hwnd` is a live handle; `process_id` is a valid out-pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if process_id == 0 || process_id == exclude_process_id {
        return None;
    }

    let executable_path = get_executable_path(process_id);

    Some(TopLevelWindow {
        hwnd: hwnd.0 as isize,
        process_id,
        executable_path,
        window_class,
        title,
        rect_px,
    })
}

fn get_class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: `hwnd` is a live handle; `buf` is a valid, correctly-sized buffer.
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

fn get_window_text(hwnd: HWND) -> String {
    // SAFETY: `hwnd` is a live handle.
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }

    let mut buf = vec![0u16; len as usize + 1];
    // SAFETY: `hwnd` is a live handle; `buf` is sized to hold `len + 1` code units.
    let copied = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..copied.max(0) as usize])
}

fn get_window_rect(hwnd: HWND) -> Option<PixelRect> {
    let mut rect = windows::Win32::Foundation::RECT::default();
    // SAFETY: `hwnd` is a live handle; `rect` is a valid out-pointer.
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(PixelRect::new(
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    ))
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    // SAFETY: `hwnd` is a live handle; `cloaked` is a valid, correctly-sized out-pointer.
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            std::ptr::from_mut(&mut cloaked).cast(),
            u32::try_from(std::mem::size_of::<u32>()).unwrap(),
        )
    };
    result.is_ok() && cloaked != 0
}

fn get_executable_path(process_id: u32) -> Option<PathBuf> {
    // SAFETY: `process_id` is a valid process id obtained from `GetWindowThreadProcessId`.
    let handle =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;

    let mut buf = [0u16; MAX_PATH as usize];
    let mut len = buf.len() as u32;
    // SAFETY: `handle` is a valid, owned process handle; `buf`/`len` describe a
    // valid, correctly-sized buffer.
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    };

    // SAFETY: `handle` is a valid, owned handle we are done with.
    unsafe {
        let _ = CloseHandle(handle);
    }

    result.ok()?;
    Some(PathBuf::from(String::from_utf16_lossy(
        &buf[..len as usize],
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_excludes_known_shell_classes_and_undersized_windows() {
        let windows = enumerate_top_level_windows(0).expect("EnumWindows should succeed");

        for window in &windows {
            assert!(!EXCLUDED_CLASSES.contains(&window.window_class.as_str()));
            assert!(window.rect_px.width >= MIN_CANDIDATE_WIDTH);
            assert!(window.rect_px.height >= MIN_CANDIDATE_HEIGHT);
            assert!(!window.title.is_empty());
        }
    }

    #[test]
    fn enumeration_excludes_current_process() {
        let current_pid = std::process::id();
        let windows = enumerate_top_level_windows(current_pid).expect("EnumWindows should succeed");

        assert!(windows.iter().all(|w| w.process_id != current_pid));
    }
}
