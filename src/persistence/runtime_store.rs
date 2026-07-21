//! `runtime.json`: re-buildable runtime state, not user configuration (PLAN.md §7.3).

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::agent::AgentRun;
use crate::persistence::clock;

const RUNTIME_FILE: &str = "runtime.json";
const TMP_FILE: &str = "runtime.json.tmp";

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeState {
    pub schema_version: u32,
    pub current_workset_id: Option<Uuid>,
    /// Workset id (as a string, per JSON object-key rules) -> assigned slot key.
    /// The slot-key encoding is owned by the Phase 6 parking allocator.
    pub auto_slot_assignments: HashMap<String, String>,
    /// Tracked Codex agent-run records (PLAN.md §6).
    pub agent_runs: Vec<AgentRun>,
    pub last_seen_monitor_fingerprint: Option<String>,
    pub last_clean_shutdown: bool,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            current_workset_id: None,
            auto_slot_assignments: HashMap::new(),
            agent_runs: Vec::new(),
            last_seen_monitor_fingerprint: None,
            last_clean_shutdown: true,
        }
    }
}

#[derive(Debug, Error)]
pub enum RuntimeStoreError {
    #[error("failed to write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Loads `runtime.json`, or a fresh default if it is missing, unreadable, or
/// fails to parse. Runtime state is reconstructible by design (PLAN.md §7.3), so
/// a corrupt file is quarantined and logged rather than treated as fatal.
pub fn load(dir: &Path) -> RuntimeState {
    let path = dir.join(RUNTIME_FILE);
    if !path.exists() {
        return RuntimeState::default();
    }

    match fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<RuntimeState>(&text).ok())
    {
        Some(state) => state,
        None => {
            tracing::warn!(path = %path.display(), "runtime.json unreadable; starting from a fresh state");
            let timestamp = clock::now_rfc3339().replace([':', '.'], "-");
            let quarantined = path.with_file_name(format!("runtime.corrupt.{timestamp}.json"));
            let _ = fs::rename(&path, quarantined);
            RuntimeState::default()
        }
    }
}

/// Atomically saves `state` to `runtime.json` (write to a temp file, then rename).
pub fn save(dir: &Path, state: &RuntimeState) -> Result<(), RuntimeStoreError> {
    let primary = dir.join(RUNTIME_FILE);
    let tmp = dir.join(TMP_FILE);

    let json = serde_json::to_string_pretty(state).map_err(|source| RuntimeStoreError::Write {
        path: tmp.clone(),
        source: io::Error::other(source),
    })?;

    {
        let mut file = fs::File::create(&tmp).map_err(|source| RuntimeStoreError::Write {
            path: tmp.clone(),
            source,
        })?;
        file.write_all(json.as_bytes())
            .map_err(|source| RuntimeStoreError::Write {
                path: tmp.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| RuntimeStoreError::Write {
            path: tmp.clone(),
            source,
        })?;
    }

    fs::rename(&tmp, &primary).map_err(|source| RuntimeStoreError::Write {
        path: primary,
        source,
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn missing_runtime_file_loads_default() {
        let dir = tempdir().unwrap();
        let state = load(dir.path());
        assert!(state.current_workset_id.is_none());
        assert!(state.last_clean_shutdown);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempdir().unwrap();
        let state = RuntimeState {
            current_workset_id: Some(Uuid::new_v4()),
            last_clean_shutdown: false,
            ..RuntimeState::default()
        };

        save(dir.path(), &state).unwrap();
        let loaded = load(dir.path());

        assert_eq!(loaded.current_workset_id, state.current_workset_id);
        assert_eq!(loaded.last_clean_shutdown, state.last_clean_shutdown);
    }

    #[test]
    fn corrupt_runtime_file_is_quarantined_and_defaults_are_returned() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(RUNTIME_FILE), b"not json").unwrap();

        let state = load(dir.path());
        assert!(state.current_workset_id.is_none());
        assert!(!dir.path().join(RUNTIME_FILE).exists());
    }
}
