//! The RepoDeck-side named-pipe server that `repodeck-hook.exe` writes
//! normalized Codex hook events to (PLAN.md §6.4, §9.1).
//!
//! `ConnectNamedPipe`/`ReadFile` block synchronously here (`lpoverlapped:
//! None` throughout) — confirmed against the vendored `windows` crate that
//! this needs no IOCP/overlapped I/O, matching this codebase's existing
//! plain-blocking-thread style (see `hotkey::win32_hotkey`). This module has
//! no Slint dependency; callers marshal `on_event` into the UI thread
//! themselves via `slint::invoke_from_event_loop`, same as the hotkey thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_MORE_DATA, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND, ReadFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_WAIT,
};
use windows::core::HSTRING;

use super::protocol::MAX_EVENT_BYTES;

/// Exact pipe name Codex hooks / `repodeck-hook.exe` target (PLAN.md §6.4).
pub const PIPE_NAME: &str = r"\\.\pipe\RepoDeck.AgentEvents.v1";

/// Owner-only DACL: one Allow ACE, generic-all, to the Owner Rights
/// placeholder SID — no Everyone, no other users, no built-in groups. A
/// process running as SYSTEM/an admin can still bypass any DACL; that's a
/// Windows platform constant, not a gap in this design.
const PIPE_SDDL: &str = "D:P(A;;GA;;;OW)";

const MAX_PIPE_INSTANCES: u32 = 16;
const PIPE_BUFFER_SIZE: u32 = 64 * 1024;

/// A raw `HANDLE` isn't `Send` by default (it's a bare `*mut c_void`), but a
/// pipe-instance handle is safe to hand to its own dedicated accept-loop
/// thread — nothing else touches it concurrently (mirrors `app.rs`'s
/// `SendHandle` for the same reason).
struct SendHandle(HANDLE);

// SAFETY: see the doc comment above.
unsafe impl Send for SendHandle {}

#[derive(Debug)]
pub enum PipeServerEvent {
    MessageReceived(Vec<u8>),
    MessageTooLarge,
    ConnectionError(String),
}

pub struct NamedPipeServer {
    shutdown_flag: Arc<AtomicBool>,
}

impl NamedPipeServer {
    /// Spawns the dedicated accept-loop thread, having already created (and
    /// validated) the first pipe instance synchronously so a name collision
    /// or ACL failure surfaces as an `Err` from `spawn` itself, matching
    /// `hotkey::win32_hotkey::HotkeyThread::spawn`'s API shape (though that
    /// one can't fail at spawn time — this one can, since pipe creation is
    /// itself fallible in a way hotkey registration's retry loop isn't).
    pub fn spawn(on_event: impl Fn(PipeServerEvent) + Send + 'static) -> std::io::Result<Self> {
        let first_handle = SendHandle(create_pipe_instance(true).map_err(to_io_error)?);

        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let thread_shutdown_flag = Arc::clone(&shutdown_flag);

        std::thread::spawn(move || {
            let first_handle = first_handle; // captured whole, see `SendHandle`'s doc comment.
            run_accept_loop(first_handle.0, &thread_shutdown_flag, &on_event);
        });

        Ok(Self { shutdown_flag })
    }
}

impl Drop for NamedPipeServer {
    fn drop(&mut self) {
        // Best-effort only: a thread currently blocked in `ConnectNamedPipe`
        // with no client connecting has no equivalent to
        // `PostThreadMessageW` to unblock it. This flag only takes effect the
        // next time the loop reaches a safe checkpoint (after a connection
        // completes); normal process exit tears the OS handle/thread down
        // regardless, so no join is attempted here.
        self.shutdown_flag.store(true, Ordering::Relaxed);
    }
}

fn run_accept_loop(
    first_handle: HANDLE,
    shutdown_flag: &Arc<AtomicBool>,
    on_event: &(impl Fn(PipeServerEvent) + Send + 'static),
) {
    let mut handle = first_handle;

    loop {
        if shutdown_flag.load(Ordering::Relaxed) {
            // SAFETY: `handle` is a live pipe-instance handle owned by this
            // loop, never closed elsewhere.
            unsafe {
                let _ = CloseHandle(handle);
            }
            break;
        }

        // SAFETY: `handle` is a freshly created, unconnected pipe instance;
        // `None` requests fully synchronous, blocking behavior.
        let connect_result = unsafe { ConnectNamedPipe(handle, None) };
        let connected = match connect_result {
            Ok(()) => true,
            // A client connecting between `CreateNamedPipeW` and this call is
            // a documented Win32 race, not a real failure.
            Err(_) => (unsafe { GetLastError() } == ERROR_PIPE_CONNECTED),
        };

        if connected {
            on_event(read_one_message(handle));
        } else {
            on_event(PipeServerEvent::ConnectionError(
                "ConnectNamedPipe failed".to_string(),
            ));
        }

        // SAFETY: `handle` is this loop's own pipe instance; disconnecting
        // an instance that never fully connected is a documented no-op.
        unsafe {
            let _ = DisconnectNamedPipe(handle);
            let _ = CloseHandle(handle);
        }

        match create_pipe_instance(false) {
            Ok(next) => handle = next,
            Err(err) => {
                on_event(PipeServerEvent::ConnectionError(err.to_string()));
                break;
            }
        }
    }
}

/// Reads exactly one message. The buffer is sized to `MAX_EVENT_BYTES + 1`
/// so a message that fits is always read in a single `ReadFile` call in
/// message mode (no `ERROR_MORE_DATA` chunking needed for the common case),
/// and a message that doesn't fit is unambiguously reported as too large
/// rather than silently truncated.
fn read_one_message(handle: HANDLE) -> PipeServerEvent {
    let mut buf = vec![0u8; MAX_EVENT_BYTES + 1];
    let mut bytes_read: u32 = 0;

    // SAFETY: `buf` is a valid, uniquely-owned buffer for the duration of
    // this synchronous call; `handle` is connected and owned by the caller.
    let result = unsafe { ReadFile(handle, Some(&mut buf), Some(&mut bytes_read), None) };

    match result {
        Ok(()) => {
            let bytes_read = bytes_read as usize;
            if bytes_read > MAX_EVENT_BYTES {
                PipeServerEvent::MessageTooLarge
            } else {
                buf.truncate(bytes_read);
                PipeServerEvent::MessageReceived(buf)
            }
        }
        Err(e) => {
            // SAFETY: reads thread-local state set by the immediately
            // preceding failed call.
            if unsafe { GetLastError() } == ERROR_MORE_DATA {
                PipeServerEvent::MessageTooLarge
            } else {
                PipeServerEvent::ConnectionError(e.to_string())
            }
        }
    }
}

/// Builds the owner-only `SECURITY_ATTRIBUTES` from [`PIPE_SDDL`], creates
/// one pipe instance, and frees the security descriptor immediately after
/// (`CreateNamedPipeW` copies what it needs from it at creation time).
/// `first` gates `FILE_FLAG_FIRST_PIPE_INSTANCE`, which fails the call if a
/// pipe of this name already exists anywhere on the system — a defense
/// against another process squatting on this name before RepoDeck starts,
/// only meaningful (and only valid to pass) on the very first instance.
fn create_pipe_instance(first: bool) -> windows::core::Result<HANDLE> {
    let sddl = HSTRING::from(PIPE_SDDL);
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `sddl` is a valid, null-terminated wide string; `psd` receives
    // an OS-allocated descriptor this function frees below via `LocalFree`.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut psd, None)
    }?;

    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd.0,
        bInheritHandle: windows::core::BOOL(0),
    };

    let mut open_mode = PIPE_ACCESS_INBOUND;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }

    let name = HSTRING::from(PIPE_NAME);
    // SAFETY: `name` is a valid wide string; `sa` is a fully-initialized,
    // stack-local `SECURITY_ATTRIBUTES` whose descriptor stays valid for the
    // duration of this call (freed only after it returns).
    let handle = unsafe {
        CreateNamedPipeW(
            &name,
            open_mode,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
            MAX_PIPE_INSTANCES,
            PIPE_BUFFER_SIZE,
            PIPE_BUFFER_SIZE,
            0,
            Some(&sa as *const SECURITY_ATTRIBUTES),
        )
    };

    // SAFETY: `psd.0` was allocated by `ConvertStringSecurityDescriptorTo...`
    // above and is no longer needed once `CreateNamedPipeW` has returned.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(psd.0)));
    }

    if handle.is_invalid() {
        return Err(windows::core::Error::from_thread());
    }
    Ok(handle)
}

fn to_io_error(err: windows::core::Error) -> std::io::Error {
    std::io::Error::other(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_matches_the_exact_spec_string() {
        assert_eq!(PIPE_NAME, r"\\.\pipe\RepoDeck.AgentEvents.v1");
    }
}
