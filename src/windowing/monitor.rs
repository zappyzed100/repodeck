//! Monitor enumeration: physical bounds, work area, DPI, device name (PLAN.md §2.2, Phase 2).

use windows::Win32::Foundation::{LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, MONITORINFOF_PRIMARY};
use windows::core::BOOL;

use crate::domain::placement::PixelRect;
use crate::windowing::win32_error::WindowError;

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorInfo {
    /// Raw `HMONITOR` value. Not persisted; monitors are re-enumerated on every
    /// startup and re-matched by `device_name` (PLAN.md §7.2 `SavedMonitor::stable_id`).
    pub handle: isize,
    pub device_name: String,
    pub bounds_px: PixelRect,
    pub work_area_px: PixelRect,
    pub dpi_x: u32,
    pub dpi_y: u32,
    pub is_primary: bool,
}

/// Enumerates all currently connected monitors, ordered by Windows' internal order
/// (top-left to bottom-right is not guaranteed here; callers that need spatial
/// ordering per PLAN.md §4.4 step 1 must sort explicitly).
pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>, WindowError> {
    let mut monitors: Vec<MonitorInfo> = Vec::new();

    // SAFETY: `enum_monitor_proc` only touches `monitors` through the raw pointer
    // passed via `lparam`, and `EnumDisplayMonitors` calls it synchronously on this
    // thread before returning, so the borrow below does not outlive this call.
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitor_proc),
            LPARAM(std::ptr::from_mut(&mut monitors) as isize),
        )
    };

    if !ok.as_bool() {
        return Err(WindowError::no_detail("EnumDisplayMonitors"));
    }

    Ok(monitors)
}

unsafe extern "system" fn enum_monitor_proc(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    // SAFETY: `lparam` was set by `enumerate_monitors` to a live `&mut Vec<MonitorInfo>`
    // for the duration of the `EnumDisplayMonitors` call.
    let monitors = unsafe { &mut *(lparam.0 as *mut Vec<MonitorInfo>) };

    if let Some(info) = describe_monitor(hmonitor) {
        monitors.push(info);
    }

    BOOL(1)
}

fn describe_monitor(hmonitor: HMONITOR) -> Option<MonitorInfo> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = u32::try_from(std::mem::size_of::<MONITORINFOEXW>()).ok()?;

    // SAFETY: `info` is a valid, correctly-sized `MONITORINFOEXW` buffer.
    let ok = unsafe { GetMonitorInfoW(hmonitor, std::ptr::from_mut(&mut info).cast()) };
    if !ok.as_bool() {
        return None;
    }

    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    // SAFETY: `hmonitor` came from `EnumDisplayMonitors` and is valid for this call;
    // failure just leaves the 96 DPI fallback in place.
    let _ = unsafe { GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };

    let nul_pos = info
        .szDevice
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(info.szDevice.len());
    let device_name = String::from_utf16_lossy(&info.szDevice[..nul_pos]);

    Some(MonitorInfo {
        handle: hmonitor.0 as isize,
        device_name,
        bounds_px: rect_to_pixel_rect(info.monitorInfo.rcMonitor),
        work_area_px: rect_to_pixel_rect(info.monitorInfo.rcWork),
        dpi_x,
        dpi_y,
        is_primary: (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
    })
}

/// Reads the current physical-pixel cursor position (PLAN.md §3.3's
/// `CursorMonitorCenter` popup placement).
pub fn cursor_position() -> Result<(i32, i32), WindowError> {
    let mut point = POINT::default();
    // SAFETY: `point` is a valid, correctly-sized `POINT` buffer.
    unsafe { GetCursorPos(&mut point) }.map_err(|e| WindowError::win32("GetCursorPos", e))?;
    Ok((point.x, point.y))
}

fn rect_to_pixel_rect(rect: RECT) -> PixelRect {
    PixelRect::new(
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_monitors_finds_at_least_one_monitor_with_positive_size() {
        let monitors = enumerate_monitors().expect("EnumDisplayMonitors should succeed");

        assert!(!monitors.is_empty(), "expected at least one monitor");
        assert!(
            monitors.iter().filter(|m| m.is_primary).count() == 1,
            "expected exactly one primary monitor"
        );
        for monitor in &monitors {
            assert!(monitor.bounds_px.width > 0 && monitor.bounds_px.height > 0);
            assert!(monitor.work_area_px.width > 0 && monitor.work_area_px.height > 0);
            assert!(monitor.dpi_x > 0 && monitor.dpi_y > 0);
            assert!(!monitor.device_name.is_empty());
        }
    }
}
