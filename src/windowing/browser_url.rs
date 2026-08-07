//! Best-effort address-bar URL extraction from a browser window via UI
//! Automation (UIA). This lets RepoDeck recognise which repository/site a
//! Chromium- or Firefox-family browser window is showing without any
//! browser-specific extension or debugging protocol.
//!
//! The whole module is side-effect free: it only *reads* UIA properties and
//! never mutates the target window. COM is initialised once per calling thread
//! (see [`read_browser_url`]) and intentionally never uninitialised — RepoDeck's
//! UI thread stays COM-initialised for the process lifetime.

use std::ffi::c_void;

use windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Variant::{VARIANT, VT_I4};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationValuePattern, TreeScope_Descendants,
    UIA_ControlTypePropertyId, UIA_EditControlTypeId, UIA_ValuePatternId,
};

/// Reads the address-bar URL of a Chromium/Firefox browser window via UI
/// Automation. Returns `None` if the window isn't a supported browser, the
/// address bar can't be found, or UIA fails. Best-effort and side-effect free.
///
/// The returned string is the *raw* address-bar text. Chromium browsers often
/// strip the `https://` scheme (showing e.g. `example.com/path`); this function
/// deliberately does not repair that — normalisation is the caller's job.
pub fn read_browser_url(hwnd: isize) -> Option<String> {
    ensure_com_initialized();

    // SAFETY: `CoCreateInstance` is a standard COM activation call. `CUIAutomation`
    // is the well-known CLSID for the UI Automation core object and `IUIAutomation`
    // is the matching interface, so the returned pointer (if `Ok`) is a valid,
    // ref-counted COM object owned by the returned wrapper (released on drop).
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.ok()?;

    let hwnd = HWND(hwnd as *mut c_void);

    // SAFETY: `ElementFromHandle` accepts any window handle; an invalid or
    // non-UIA handle simply yields an `Err`, which we map to `None`. The
    // returned element is owned by the `IUIAutomationElement` wrapper.
    let root = unsafe { automation.ElementFromHandle(hwnd) }.ok()?;

    // A VT_I4 VARIANT holding the Edit control-type id, used to build a
    // property condition matching every edit/address-bar-like descendant.
    let control_type_value = variant_i4(UIA_EditControlTypeId.0);

    // SAFETY: `CreatePropertyCondition` reads `control_type_value` by reference
    // for the duration of the call and does not take ownership of it. The VARIANT
    // holds a plain `i32` (no COM/BSTR resource), so no VARIANT clear is needed.
    let condition = unsafe {
        automation.CreatePropertyCondition(UIA_ControlTypePropertyId, &control_type_value)
    }
    .ok()?;

    // SAFETY: `FindAll` walks the subtree rooted at `root` for elements matching
    // `condition`. It returns an owned `IUIAutomationElementArray` on success.
    let edits = unsafe { root.FindAll(TreeScope_Descendants, &condition) }.ok()?;

    // SAFETY: `Length` reads the array's element count.
    let count = unsafe { edits.Length() }.ok()?;

    for index in 0..count {
        // SAFETY: `index` is bounded by `[0, count)`, the valid range reported by
        // `Length`, so `GetElement` yields an owned element on success.
        let Ok(element) = (unsafe { edits.GetElement(index) }) else {
            continue;
        };

        // SAFETY: queries the Value pattern on `element`. Controls that do not
        // support the pattern return `Err`, which we skip.
        let Ok(value_pattern) = (unsafe {
            element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
        }) else {
            continue;
        };

        // SAFETY: `CurrentValue` returns an owned BSTR wrapper (freed on drop);
        // `.to_string()` copies it into an owned Rust `String`.
        let Ok(value) = (unsafe { value_pattern.CurrentValue() }) else {
            continue;
        };

        let text = value.to_string();
        let trimmed = text.trim();
        if !trimmed.is_empty() && looks_like_url(trimmed) {
            return Some(trimmed.to_string());
        }
    }

    None
}

/// Ensures COM is initialised (apartment-threaded) on the current thread.
///
/// `RPC_E_CHANGED_MODE` means the thread is already COM-initialised with a
/// different concurrency model, which is fine for our read-only UIA use — we
/// treat it as success. We never call `CoUninitialize`: RepoDeck's UI thread
/// stays COM-initialised for the lifetime of the process.
fn ensure_com_initialized() {
    // SAFETY: `CoInitializeEx` is safe to call repeatedly on a thread; it returns
    // an `HRESULT` we inspect rather than a resource we must free. We intentionally
    // do not pair it with `CoUninitialize` (see the function doc comment).
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    debug_assert!(
        hr.is_ok() || hr == RPC_E_CHANGED_MODE,
        "unexpected CoInitializeEx failure: {hr:?}"
    );
}

/// Builds a `VT_I4` VARIANT wrapping `value`. The VARIANT owns no COM resource,
/// so callers may drop it without a `VariantClear`.
fn variant_i4(value: i32) -> VARIANT {
    let mut variant = VARIANT::default();
    // SAFETY: `VARIANT::default()` is a zeroed union; we tag it `VT_I4` and write
    // the `i32` payload into the matching `lVal` arm. Reading it back through the
    // same arm (as the UIA marshaller does for a VT_I4) is therefore well-defined.
    unsafe {
        let inner = &mut variant.Anonymous.Anonymous;
        inner.vt = VT_I4;
        inner.Anonymous.lVal = value;
    }
    variant
}

/// Simple heuristic: does `text` plausibly look like a browser address-bar URL?
/// Chromium may drop the scheme, so we accept anything with a `.` or `/`, plus
/// the explicit `http` prefix.
fn looks_like_url(text: &str) -> bool {
    text.starts_with("http") || text.contains('.') || text.contains('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_url_accepts_common_forms() {
        assert!(looks_like_url("https://example.com/path"));
        assert!(looks_like_url("example.com"));
        assert!(looks_like_url("localhost/dashboard"));
        assert!(!looks_like_url("Search or type a command"));
        assert!(!looks_like_url(""));
    }

    /// An invalid HWND (0 / null) must never panic and must yield `None`:
    /// `ElementFromHandle` fails cleanly for a non-window handle.
    #[test]
    fn invalid_hwnd_returns_none_without_panicking() {
        assert_eq!(read_browser_url(0), None);
    }

    /// Manual smoke test (mirrors the `tests/windows_e2e.rs` convention): needs a
    /// real, visible browser window, so it is `#[ignore]`d by default. Run with an
    /// actual browser HWND substituted below via:
    ///
    /// ```powershell
    /// cargo test --lib windowing::browser_url -- --ignored --nocapture
    /// ```
    ///
    /// Obtain a live browser HWND (e.g. from `enumerate::enumerate_top_level_windows`
    /// filtered to `chrome.exe`/`firefox.exe`) and pass it here to observe the
    /// address-bar URL printed to stdout.
    #[test]
    #[ignore = "requires an interactive desktop session with a real browser window"]
    fn reads_url_from_live_browser_window() {
        // Replace `0` with a real browser HWND when running this manually.
        let hwnd: isize = 0;
        match read_browser_url(hwnd) {
            Some(url) => println!("address-bar URL: {url}"),
            None => println!("no URL read (not a browser window, or UIA unavailable)"),
        }
    }
}
