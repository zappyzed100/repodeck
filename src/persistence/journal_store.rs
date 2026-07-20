//! `switch-journal.json`: written before a workset switch begins, so a crash
//! mid-switch can be recovered on the next startup (PLAN.md §7.4, §10.3).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::placement::{PixelRect, SavedShowState};

const JOURNAL_FILE: &str = "switch-journal.json";
const TMP_FILE: &str = "switch-journal.json.tmp";

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalStatus {
    Started,
    Committed,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchJournal {
    pub schema_version: u32,
    pub transaction_id: Uuid,
    pub status: JournalStatus,
    pub from_workset_id: Option<Uuid>,
    pub to_workset_id: Uuid,
    /// RFC 3339 UTC timestamp.
    pub created_at: String,
    pub windows: Vec<JournalWindowEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalWindowEntry {
    pub managed_window_id: Uuid,
    /// Valid only within this transaction (PLAN.md §7.4): after a restart, HWNDs
    /// are re-validated by process id and current window attributes, never trusted
    /// on their own.
    pub hwnd: isize,
    pub process_id: u32,
    pub before: JournalWindowState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalWindowState {
    pub rect: PixelRect,
    pub show_state: SavedShowState,
}

#[derive(Debug, Error)]
pub enum JournalStoreError {
    #[error("failed to read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Loads the journal left behind by an interrupted switch, if any (PLAN.md §10.3).
pub fn load(dir: &Path) -> Result<Option<SwitchJournal>, JournalStoreError> {
    let path = dir.join(JOURNAL_FILE);
    if !path.exists() {
        return Ok(None);
    }

    let text = fs::read_to_string(&path).map_err(|source| JournalStoreError::Read {
        path: path.clone(),
        source,
    })?;
    let journal = serde_json::from_str(&text).map_err(|source| JournalStoreError::Parse {
        path: path.clone(),
        source,
    })?;
    Ok(Some(journal))
}

/// Writes `journal` before a switch begins (PLAN.md §3.8 step 4).
pub fn save(dir: &Path, journal: &SwitchJournal) -> Result<(), JournalStoreError> {
    let primary = dir.join(JOURNAL_FILE);
    let tmp = dir.join(TMP_FILE);

    let json =
        serde_json::to_string_pretty(journal).map_err(|source| JournalStoreError::Write {
            path: tmp.clone(),
            source: io::Error::other(source),
        })?;

    {
        let mut file = fs::File::create(&tmp).map_err(|source| JournalStoreError::Write {
            path: tmp.clone(),
            source,
        })?;
        file.write_all(json.as_bytes())
            .map_err(|source| JournalStoreError::Write {
                path: tmp.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| JournalStoreError::Write {
            path: tmp.clone(),
            source,
        })?;
    }

    fs::rename(&tmp, &primary).map_err(|source| JournalStoreError::Write {
        path: primary,
        source,
    })?;

    Ok(())
}

/// Deletes the journal after a switch commits or a rollback finishes resolving
/// (PLAN.md §3.8 step 11, §10.3 step 5).
pub fn clear(dir: &Path) -> Result<(), JournalStoreError> {
    let path = dir.join(JOURNAL_FILE);
    if !path.exists() {
        return Ok(());
    }
    fs::remove_file(&path).map_err(|source| JournalStoreError::Write { path, source })
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn sample_journal() -> SwitchJournal {
        SwitchJournal {
            schema_version: CURRENT_SCHEMA_VERSION,
            transaction_id: Uuid::new_v4(),
            status: JournalStatus::Started,
            from_workset_id: None,
            to_workset_id: Uuid::new_v4(),
            created_at: "2026-07-20T00:00:00Z".to_string(),
            windows: vec![JournalWindowEntry {
                managed_window_id: Uuid::new_v4(),
                hwnd: 123_456,
                process_id: 1000,
                before: JournalWindowState {
                    rect: PixelRect::new(0, 0, 1000, 800),
                    show_state: SavedShowState::Normal,
                },
            }],
        }
    }

    #[test]
    fn no_journal_file_means_no_interrupted_switch() {
        let dir = tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempdir().unwrap();
        let journal = sample_journal();

        save(dir.path(), &journal).unwrap();
        let loaded = load(dir.path()).unwrap().unwrap();

        assert_eq!(loaded.transaction_id, journal.transaction_id);
        assert_eq!(loaded.status, JournalStatus::Started);
        assert_eq!(loaded.windows.len(), 1);
    }

    #[test]
    fn clear_removes_the_journal() {
        let dir = tempdir().unwrap();
        save(dir.path(), &sample_journal()).unwrap();

        clear(dir.path()).unwrap();

        assert!(load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn clear_on_missing_journal_is_a_no_op() {
        let dir = tempdir().unwrap();
        assert!(clear(dir.path()).is_ok());
    }
}
