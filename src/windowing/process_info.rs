//! Reads another process's full command line, used at registration to learn
//! which folder/workspace a VS Code window has open so the "セット経由で再起動"
//! relaunch can reopen it — even when the workset has no `repository_path`.
//!
//! Uses `NtQueryInformationProcess(ProcessCommandLineInformation)` (Windows 8.1+),
//! which returns the command line in one call without walking the PEB. Entirely
//! best-effort: any failure (a protected process, access denied, an unexpected
//! layout) yields `None` and the caller falls back to `repository_path`.

use windows::Wdk::System::Threading::{NtQueryInformationProcess, PROCESSINFOCLASS};
use windows::Win32::Foundation::{CloseHandle, UNICODE_STRING};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

/// `ProcessCommandLineInformation` — not in the `windows` enum, so constructed
/// by its documented ordinal.
const PROCESS_COMMAND_LINE_INFORMATION: PROCESSINFOCLASS = PROCESSINFOCLASS(60);

/// The full command line of process `pid` (`"C:\...\Code.exe" "D:\repo"`), or
/// `None` if it can't be read. Note: VS Code shares one main process across all
/// its windows, so for a second folder-window opened into an already-running
/// instance this returns the *first* window's command line — the caller treats
/// the result as a best-effort hint and logs it.
pub fn read_process_command_line(pid: u32) -> Option<String> {
    // SAFETY: `OpenProcess` is called with a concrete pid; the returned handle
    // is closed on every path. The two `NtQueryInformationProcess` calls use a
    // correctly-sized buffer (the first sizes it), and the `UNICODE_STRING` is
    // read only within the bytes the call reported writing.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

        let mut needed = 0u32;
        // First call sizes the buffer (returns STATUS_INFO_LENGTH_MISMATCH).
        let _ = NtQueryInformationProcess(
            handle,
            PROCESS_COMMAND_LINE_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
        if needed == 0 || needed > 64 * 1024 {
            let _ = CloseHandle(handle);
            return None;
        }

        let mut buf = vec![0u8; needed as usize];
        let status = NtQueryInformationProcess(
            handle,
            PROCESS_COMMAND_LINE_INFORMATION,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        );
        let _ = CloseHandle(handle);
        if status.is_err() {
            return None;
        }

        // The buffer begins with a UNICODE_STRING whose Buffer points just past
        // it, into the same allocation.
        let unicode = &*buf.as_ptr().cast::<UNICODE_STRING>();
        if unicode.Buffer.is_null() || unicode.Length == 0 {
            return None;
        }
        let len_u16 = (unicode.Length / 2) as usize;
        let chars = std::slice::from_raw_parts(unicode.Buffer.0, len_u16);
        Some(String::from_utf16_lossy(chars))
    }
}
