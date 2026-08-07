//! Start-with-Windows via the per-user `Run` registry key (PLAN.md §11:
//! "管理者権限を要求しない" — `HKEY_CURRENT_USER`, not `HKEY_LOCAL_MACHINE`,
//! needs no elevation).

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::HSTRING;

const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "RepoDeck";

#[derive(Debug, thiserror::Error)]
pub enum AutostartError {
    #[error("failed to determine the running executable's path: {0}")]
    CurrentExe(#[source] std::io::Error),
    #[error("registry operation failed: {0}")]
    Registry(#[from] windows::core::Error),
}

/// Whether the `RepoDeck` value currently exists under the per-user `Run` key.
pub fn is_enabled() -> Result<bool, AutostartError> {
    let mut hkey = HKEY::default();
    let path = HSTRING::from(RUN_KEY_PATH);
    // SAFETY: `path` is a valid, NUL-terminated wide string; `hkey` receives
    // an owned key handle this function closes before returning.
    let open_result =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &path, Some(0), KEY_READ, &mut hkey) };
    if open_result == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    check(open_result)?;

    let value_name = HSTRING::from(VALUE_NAME);
    // SAFETY: `hkey` was just successfully opened above; no data buffer is
    // requested, only whether the value exists.
    let query_result = unsafe { RegQueryValueExW(hkey, &value_name, None, None, None, None) };

    // SAFETY: `hkey` is a valid, open key handle owned by this function.
    unsafe {
        let _ = RegCloseKey(hkey);
    }

    if query_result == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    check(query_result)?;
    Ok(true)
}

/// Creates or removes the `RepoDeck` autostart value, pointing at the
/// currently running `repodeck.exe`'s absolute path (quoted, in case it sits
/// under a path containing spaces).
pub fn set_enabled(enabled: bool) -> Result<(), AutostartError> {
    if enabled {
        create_value()
    } else {
        delete_value()
    }
}

fn create_value() -> Result<(), AutostartError> {
    let exe = std::env::current_exe().map_err(AutostartError::CurrentExe)?;
    let command = format!("\"{}\"", exe.display());

    let mut hkey = HKEY::default();
    let path = HSTRING::from(RUN_KEY_PATH);
    // SAFETY: `path` is a valid wide string; no security attributes/class are
    // needed for a per-user key under `HKEY_CURRENT_USER`.
    let create_result = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &path,
            None,
            windows::core::PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
    };
    check(create_result)?;

    let value_name = HSTRING::from(VALUE_NAME);
    let data = wide_null_terminated_bytes(&command);
    // SAFETY: `hkey` was just created/opened above; `data` is a valid,
    // NUL-terminated UTF-16LE byte buffer matching `REG_SZ`'s expected shape.
    let set_result = unsafe { RegSetValueExW(hkey, &value_name, Some(0), REG_SZ, Some(&data)) };

    // SAFETY: `hkey` is a valid, open key handle owned by this function.
    unsafe {
        let _ = RegCloseKey(hkey);
    }

    check(set_result)?;
    Ok(())
}

fn delete_value() -> Result<(), AutostartError> {
    let mut hkey = HKEY::default();
    let path = HSTRING::from(RUN_KEY_PATH);
    // SAFETY: `path` is a valid wide string.
    let open_result =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &path, Some(0), KEY_WRITE, &mut hkey) };
    if open_result == ERROR_FILE_NOT_FOUND {
        return Ok(()); // already absent
    }
    check(open_result)?;

    let value_name = HSTRING::from(VALUE_NAME);
    // SAFETY: `hkey` was just successfully opened above.
    let delete_result = unsafe { RegDeleteValueW(hkey, &value_name) };

    // SAFETY: `hkey` is a valid, open key handle owned by this function.
    unsafe {
        let _ = RegCloseKey(hkey);
    }

    if delete_result == ERROR_FILE_NOT_FOUND {
        return Ok(()); // already absent
    }
    check(delete_result)?;
    Ok(())
}

fn wide_null_terminated_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(std::iter::once(0u16))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn check(result: WIN32_ERROR) -> windows::core::Result<()> {
    if result == ERROR_SUCCESS_CODE {
        Ok(())
    } else {
        Err(windows::core::Error::from_hresult(hresult_from_win32(
            result.0,
        )))
    }
}

const ERROR_SUCCESS_CODE: WIN32_ERROR = WIN32_ERROR(0);

/// The standard `HRESULT_FROM_WIN32` conversion — these registry functions
/// return a raw `WIN32_ERROR` directly rather than setting the thread-local
/// last-error (so `windows::core::Error::from_thread()` doesn't apply here).
fn hresult_from_win32(code: u32) -> windows::core::HRESULT {
    windows::core::HRESULT(if code == 0 {
        0
    } else {
        ((code & 0x0000_FFFF) | (7 << 16) | 0x8000_0000) as i32
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hresult_from_win32_success_is_zero() {
        assert_eq!(hresult_from_win32(0).0, 0);
    }

    #[test]
    fn hresult_from_win32_matches_the_documented_formula() {
        // ERROR_FILE_NOT_FOUND (2) -> a well-known HRESULT constant.
        assert_eq!(hresult_from_win32(2).0 as u32, 0x8007_0002);
    }

    #[test]
    fn wide_bytes_are_null_terminated_utf16le() {
        let bytes = wide_null_terminated_bytes("AB");
        assert_eq!(bytes, vec![b'A', 0, b'B', 0, 0, 0]);
    }
}
