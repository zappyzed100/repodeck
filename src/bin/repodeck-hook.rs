//! Tiny helper Codex's own lifecycle hooks invoke directly (PLAN.md §6.4,
//! §9.4). Reads one JSON payload from stdin, adapts it to RepoDeck's
//! normalized wire schema, and makes a single best-effort attempt to deliver
//! it over the named pipe `repodeck` listens on — then always exits 0.
//! Every failure path (RepoDeck not running, malformed JSON, oversized
//! input, a busy pipe) is swallowed rather than surfaced: Codex's hook
//! runner treats a non-zero exit as a real error, and a passive status
//! observer must never be able to interrupt or slow down the user's actual
//! Codex session.

use std::io::Read;

use repodeck::ipc::named_pipe::PIPE_NAME;
use repodeck::ipc::protocol::{self, MAX_EVENT_BYTES};
use windows::Win32::Foundation::GENERIC_WRITE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_NONE, OPEN_EXISTING, WriteFile,
};
use windows::core::HSTRING;

fn main() {
    let _ = run();
    std::process::exit(0);
}

fn run() -> Option<()> {
    let raw = read_capped(std::io::stdin().lock(), MAX_EVENT_BYTES).ok()?;
    let payload = build_wire_payload(&raw)?;
    try_send_once(PIPE_NAME, &payload).ok()
}

/// Reads all of `reader` capped at `max_bytes`. Returns an empty `Vec`
/// (never a partial read) if the actual input exceeds the cap — defensive
/// size enforcement on the reading side, independent of `ipc::protocol`'s
/// own cap, per PLAN.md §6.4/§11. Generic over `Read` so the cap logic is
/// unit-testable without real stdin.
fn read_capped(mut reader: impl Read, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader
        .by_ref()
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut buf)?;
    if buf.len() > max_bytes {
        buf.clear();
    }
    Ok(buf)
}

/// Adapts raw Codex hook JSON to RepoDeck's normalized wire schema and
/// re-serializes it, collapsing any failure (malformed JSON, unrecognized
/// hook name, oversized input) to `None`.
fn build_wire_payload(raw: &[u8]) -> Option<Vec<u8>> {
    let event = protocol::parse_and_adapt(raw).ok()?;
    serde_json::to_vec(&event).ok()
}

/// A single, non-retrying delivery attempt: `CreateFileW(OPEN_EXISTING, ...)`
/// fails immediately (`ERROR_FILE_NOT_FOUND`/`ERROR_PIPE_BUSY`) if RepoDeck
/// isn't listening, with no `WaitNamedPipeW` wait — that immediacy is what
/// keeps this hook fast even when RepoDeck isn't running, structurally, with
/// no explicit timeout needed.
fn try_send_once(pipe_name: &str, payload: &[u8]) -> windows::core::Result<()> {
    let name = HSTRING::from(pipe_name);
    // SAFETY: `name` is a valid wide string; no security attributes/template
    // handle are needed for a client-side pipe connection.
    let handle = unsafe {
        CreateFileW(
            &name,
            GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }?;

    // SAFETY: `handle` was just opened above and is closed via `HANDLE`'s
    // own `Drop`/`Free` impl once it goes out of scope.
    let result = unsafe { WriteFile(handle, Some(payload), None, None) };

    // SAFETY: `handle` is a valid, open handle owned by this function.
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_wire_payload_adapts_a_valid_event() {
        let raw = serde_json::json!({
            "session_id": "s", "turn_id": "t", "cwd": r"C:\repo",
            "hook_event_name": "UserPromptSubmit",
        })
        .to_string()
        .into_bytes();

        let payload = build_wire_payload(&raw).expect("valid event should adapt");
        let value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(value["event"], "run_started");
        assert_eq!(value["session_id"], "s");
    }

    #[test]
    fn build_wire_payload_rejects_malformed_json() {
        assert!(build_wire_payload(b"not json").is_none());
    }

    #[test]
    fn build_wire_payload_rejects_missing_required_field() {
        let raw = serde_json::json!({"turn_id": "t", "cwd": "C:\\repo", "hook_event_name": "Stop"})
            .to_string()
            .into_bytes();
        assert!(build_wire_payload(&raw).is_none());
    }

    #[test]
    fn build_wire_payload_rejects_unrecognized_hook_name() {
        let raw = serde_json::json!({
            "session_id": "s", "turn_id": "t", "cwd": r"C:\repo",
            "hook_event_name": "SomeFutureHook",
        })
        .to_string()
        .into_bytes();
        assert!(build_wire_payload(&raw).is_none());
    }

    #[test]
    fn read_capped_passes_through_input_within_the_cap() {
        let input = b"hello".as_slice();
        let result = read_capped(input, 10).unwrap();
        assert_eq!(result, b"hello");
    }

    #[test]
    fn read_capped_truncates_to_empty_when_input_exceeds_the_cap() {
        let input = b"hello world".as_slice();
        let result = read_capped(input, 5).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn read_capped_passes_through_input_exactly_at_the_cap() {
        let input = b"hello".as_slice();
        let result = read_capped(input, 5).unwrap();
        assert_eq!(result, b"hello");
    }
}
