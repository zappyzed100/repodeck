//! RepoDeck's normalized agent-event wire schema v1, and the Codex-hook-JSON
//! adapter (PLAN.md §6.3, §6.4, §11).
//!
//! This file is deliberately the *only* place that should need to change if
//! Codex's own hook JSON schema shifts upstream (PLAN.md §6.3's own
//! instruction) — [`NormalizedEvent`] is RepoDeck's stable internal shape;
//! [`CodexHookEvent`] and [`parse_and_adapt`] are the volatile adapter layer.
//! Stateful interpretation (e.g. "was this turn pending input") belongs in
//! `application::agent_status_service`, not here — this module stays a pure,
//! stateless mapping from one Codex hook invocation to one normalized event.

use serde::{Deserialize, Serialize};

use crate::persistence::clock;

/// Maximum accepted event size (PLAN.md §6.4 "最大入力1MiB", §11).
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;
pub const SCHEMA_VERSION: u32 = 1;

/// RepoDeck's own normalized wire schema v1 — the exact JSON shape sent over
/// the named pipe (PLAN.md §6.4's "転送JSON"). Must not change shape without
/// bumping [`SCHEMA_VERSION`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedEvent {
    pub schema_version: u32,
    pub source: String,
    pub event: NormalizedEventKind,
    pub session_id: String,
    pub turn_id: String,
    pub cwd: String,
    pub model: Option<String>,
    pub occurred_at: String,
}

/// One variant per Codex hook (PLAN.md §6.3's mapping table), 1:1. Whether a
/// `ToolUseObserved` becomes a real "resumed" transition depends on run-store
/// state this module doesn't have — see `application::agent_status_service`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedEventKind {
    RunStarted,
    NeedsInput,
    ToolUseObserved,
    RunCompleted,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("event exceeds the 1 MiB limit ({actual} bytes)")]
    TooLarge { actual: usize },
    #[error("malformed JSON: {0}")]
    MalformedJson(#[from] serde_json::Error),
    #[error("unrecognized hook_event_name: {0}")]
    UnrecognizedHookEvent(String),
}

/// Raw shape of Codex's own hook JSON (PLAN.md §6.3's field list). Kept
/// separate from [`NormalizedEvent`] since it's the volatile side of the
/// adapter. Deliberately has no `transcript_path` field at all — forwarding
/// prompt content is structurally impossible here, not just avoided by
/// convention (PLAN.md §6.3/§11's privacy requirement). Any other unknown
/// JSON fields (including `transcript_path`, if present) are silently
/// ignored by serde's default behavior, which PLAN.md §11 explicitly allows.
/// `permission_mode` (also listed in PLAN.md §6.3's consumed-fields list) is
/// deliberately not modeled here: nothing in §6.2's aggregation, §6.3's event
/// mapping, or §6.7's notifications ever branches on it, and §6.4's own wire
/// JSON example doesn't carry it either — same "ignored, not forwarded"
/// treatment as any other field this adapter doesn't need, via serde's
/// default unknown-field handling (PLAN.md §11).
#[derive(Debug, Deserialize)]
struct CodexHookEvent {
    session_id: String,
    turn_id: String,
    cwd: String,
    hook_event_name: String,
    model: Option<String>,
}

/// Parses one Codex hook invocation's JSON and adapts it to RepoDeck's
/// normalized schema (PLAN.md §6.3). Enforces the 1 MiB cap before parsing.
pub fn parse_and_adapt(raw: &[u8]) -> Result<NormalizedEvent, ProtocolError> {
    if raw.len() > MAX_EVENT_BYTES {
        return Err(ProtocolError::TooLarge { actual: raw.len() });
    }
    let hook: CodexHookEvent = serde_json::from_slice(raw)?;

    let event = match hook.hook_event_name.as_str() {
        "UserPromptSubmit" => NormalizedEventKind::RunStarted,
        "PermissionRequest" => NormalizedEventKind::NeedsInput,
        "PostToolUse" => NormalizedEventKind::ToolUseObserved,
        "Stop" => NormalizedEventKind::RunCompleted,
        other => return Err(ProtocolError::UnrecognizedHookEvent(other.to_string())),
    };

    Ok(NormalizedEvent {
        schema_version: SCHEMA_VERSION,
        source: "codex".to_string(),
        event,
        session_id: hook.session_id,
        turn_id: hook.turn_id,
        cwd: normalize_cwd(&hook.cwd),
        model: hook.model,
        occurred_at: clock::now_rfc3339(),
    })
}

/// Lexical `cwd` cleanup (PLAN.md §6.4 "cwdを正規化して送信"): resolves `.`/
/// `..` components and normalizes path separators. Deliberately does *not*
/// call `std::fs::canonicalize` — that requires the path to exist and
/// follows symlinks, and the hook process (a short-lived stdin-to-pipe
/// relay) shouldn't touch the filesystem beyond stdin/the pipe itself.
fn normalize_cwd(raw: &str) -> String {
    use std::path::{Component, Path};

    let mut normalized = std::path::PathBuf::new();
    for component in Path::new(raw).components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_json(hook_event_name: &str) -> Vec<u8> {
        serde_json::json!({
            "session_id": "sess-1",
            "turn_id": "turn-1",
            "cwd": r"C:\repo",
            "hook_event_name": hook_event_name,
            "model": "gpt-test",
            "permission_mode": "auto",
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn valid_event_maps_every_hook_name() {
        let cases = [
            ("UserPromptSubmit", NormalizedEventKind::RunStarted),
            ("PermissionRequest", NormalizedEventKind::NeedsInput),
            ("PostToolUse", NormalizedEventKind::ToolUseObserved),
            ("Stop", NormalizedEventKind::RunCompleted),
        ];
        for (hook_name, expected) in cases {
            let event = parse_and_adapt(&valid_json(hook_name)).unwrap();
            assert_eq!(event.event, expected);
            assert_eq!(event.schema_version, SCHEMA_VERSION);
            assert_eq!(event.session_id, "sess-1");
            assert_eq!(event.turn_id, "turn-1");
        }
    }

    #[test]
    fn oversized_input_is_rejected() {
        let raw = vec![b'a'; MAX_EVENT_BYTES + 1];
        assert!(matches!(
            parse_and_adapt(&raw),
            Err(ProtocolError::TooLarge { .. })
        ));
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(matches!(
            parse_and_adapt(b"not json"),
            Err(ProtocolError::MalformedJson(_))
        ));
    }

    #[test]
    fn missing_required_field_is_rejected() {
        let raw = serde_json::json!({"turn_id": "t", "cwd": "C:\\repo", "hook_event_name": "Stop"})
            .to_string()
            .into_bytes();
        assert!(matches!(
            parse_and_adapt(&raw),
            Err(ProtocolError::MalformedJson(_))
        ));
    }

    #[test]
    fn unrecognized_hook_event_name_is_rejected() {
        assert!(matches!(
            parse_and_adapt(&valid_json("SomeFutureHook")),
            Err(ProtocolError::UnrecognizedHookEvent(name)) if name == "SomeFutureHook"
        ));
    }

    #[test]
    fn transcript_path_field_is_ignored_not_forwarded() {
        let raw = serde_json::json!({
            "session_id": "s", "turn_id": "t", "cwd": "C:\\repo",
            "hook_event_name": "Stop", "transcript_path": "C:\\secret\\transcript.json",
        })
        .to_string()
        .into_bytes();
        // Must parse successfully (unknown fields ignored) and the resulting
        // NormalizedEvent has no field capable of carrying transcript_path.
        assert!(parse_and_adapt(&raw).is_ok());
    }

    #[test]
    fn cwd_is_lexically_normalized() {
        let raw = serde_json::json!({
            "session_id": "s", "turn_id": "t", "cwd": r"C:\repo\.\sub\..\final",
            "hook_event_name": "Stop",
        })
        .to_string()
        .into_bytes();
        let event = parse_and_adapt(&raw).unwrap();
        assert_eq!(event.cwd, r"C:\repo\final");
    }
}
