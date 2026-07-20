//! Atomic load/save for `config.json`, with backup fallback and quarantine of
//! unreadable files (PLAN.md §7.1, §7.5, §7.6, §10.4).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::domain::config::AppConfig;
use crate::persistence::{clock, migrations};

const CONFIG_FILE: &str = "config.json";
const BACKUP_FILE: &str = "config.backup.json";
const TMP_FILE: &str = "config.json.tmp";

#[derive(Debug, Error)]
pub enum ConfigStoreError {
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

    #[error(
        "config schema_version {found} is newer than this build supports ({supported}); please update RepoDeck"
    )]
    UnsupportedSchemaVersion { found: u32, supported: u32 },

    #[error(
        "both config.json and config.backup.json are unreadable; the broken file was quarantined"
    )]
    BothCorrupt,
}

pub struct LoadResult {
    pub config: AppConfig,
    pub recovered_from_backup: bool,
}

/// Loads `config.json`, falling back to `config.backup.json` if the primary file
/// is missing, unreadable, or fails to parse (PLAN.md §10.4).
///
/// Returns `Ok(None)` when neither file exists yet (first run).
pub fn load(dir: &Path) -> Result<Option<LoadResult>, ConfigStoreError> {
    let primary = dir.join(CONFIG_FILE);
    let backup = dir.join(BACKUP_FILE);

    if !primary.exists() && !backup.exists() {
        return Ok(None);
    }

    if primary.exists() {
        match read_and_parse(&primary) {
            Ok(config) => {
                return Ok(Some(LoadResult {
                    config,
                    recovered_from_backup: false,
                }));
            }
            Err(ConfigStoreError::UnsupportedSchemaVersion { found, supported }) => {
                return Err(ConfigStoreError::UnsupportedSchemaVersion { found, supported });
            }
            Err(primary_err) => {
                tracing::warn!(error = %primary_err, "config.json unreadable, trying config.backup.json");
            }
        }
    }

    match read_and_parse(&backup) {
        Ok(config) => Ok(Some(LoadResult {
            config,
            recovered_from_backup: true,
        })),
        Err(_) => {
            quarantine(&primary)?;
            Err(ConfigStoreError::BothCorrupt)
        }
    }
}

fn read_and_parse(path: &Path) -> Result<AppConfig, ConfigStoreError> {
    let text = fs::read_to_string(path).map_err(|source| ConfigStoreError::Read {
        path: path.to_path_buf(),
        source,
    })?;

    let raw: serde_json::Value =
        serde_json::from_str(&text).map_err(|source| ConfigStoreError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

    let schema_version = raw
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    migrations::ensure_supported(schema_version)?;
    let migrated = migrations::migrate(raw, schema_version)?;

    serde_json::from_value(migrated).map_err(|source| ConfigStoreError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// Moves an unreadable file aside with a timestamp suffix rather than discarding it
/// (PLAN.md §10.4: "データを無言で破棄しない").
fn quarantine(path: &Path) -> Result<(), ConfigStoreError> {
    if !path.exists() {
        return Ok(());
    }

    let timestamp = clock::now_rfc3339().replace([':', '.'], "-");
    let quarantined = path.with_file_name(format!("config.corrupt.{timestamp}.json"));

    fs::rename(path, &quarantined).map_err(|source| ConfigStoreError::Write {
        path: quarantined,
        source,
    })
}

/// Atomically saves `config` to `config.json` (PLAN.md §7.5): back up the current
/// file, write a temp file, flush + `sync_all`, re-parse the temp file to verify
/// it, then rename into place. On any failure before the final rename,
/// `config.json` is left untouched.
pub fn save(dir: &Path, config: &AppConfig) -> Result<(), ConfigStoreError> {
    let primary = dir.join(CONFIG_FILE);
    let backup = dir.join(BACKUP_FILE);
    let tmp = dir.join(TMP_FILE);

    if primary.exists() {
        fs::copy(&primary, &backup).map_err(|source| ConfigStoreError::Write {
            path: backup,
            source,
        })?;
    }

    let json = serde_json::to_string_pretty(config).map_err(|source| ConfigStoreError::Parse {
        path: tmp.clone(),
        source,
    })?;

    {
        let mut file = fs::File::create(&tmp).map_err(|source| ConfigStoreError::Write {
            path: tmp.clone(),
            source,
        })?;
        file.write_all(json.as_bytes())
            .map_err(|source| ConfigStoreError::Write {
                path: tmp.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| ConfigStoreError::Write {
            path: tmp.clone(),
            source,
        })?;
    }

    // Verify the temp file round-trips before it ever becomes the primary file.
    let verify_text = fs::read_to_string(&tmp).map_err(|source| ConfigStoreError::Read {
        path: tmp.clone(),
        source,
    })?;
    let _: AppConfig =
        serde_json::from_str(&verify_text).map_err(|source| ConfigStoreError::Parse {
            path: tmp.clone(),
            source,
        })?;

    fs::rename(&tmp, &primary).map_err(|source| ConfigStoreError::Write {
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
    fn missing_config_is_first_run() {
        let dir = tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn save_then_load_round_trips_exactly() {
        let dir = tempdir().unwrap();
        let mut config = AppConfig::new_empty();
        config.main_monitor_ids.push("\\\\.\\DISPLAY1".to_string());

        save(dir.path(), &config).unwrap();

        let loaded = load(dir.path()).unwrap().unwrap();
        assert!(!loaded.recovered_from_backup);
        assert_eq!(loaded.config.main_monitor_ids, config.main_monitor_ids);
        assert_eq!(loaded.config.schema_version, config.schema_version);
    }

    #[test]
    fn save_leaves_original_untouched_if_interrupted_before_rename() {
        let dir = tempdir().unwrap();
        let config = AppConfig::new_empty();
        save(dir.path(), &config).unwrap();

        // Simulate a crash mid-write: a truncated/corrupt temp file exists, but
        // config.json itself must still be the last good version.
        fs::write(dir.path().join(TMP_FILE), b"{not valid json").unwrap();

        let loaded = load(dir.path()).unwrap().unwrap();
        assert!(!loaded.recovered_from_backup);
        assert_eq!(loaded.config.schema_version, config.schema_version);
    }

    #[test]
    fn corrupt_primary_recovers_from_backup() {
        let dir = tempdir().unwrap();
        let mut config = AppConfig::new_empty();
        config.main_monitor_ids.push("\\\\.\\DISPLAY1".to_string());
        save(dir.path(), &config).unwrap();

        // A second save creates config.backup.json from the first save's content.
        config.app_version = "0.1.1".to_string();
        save(dir.path(), &config).unwrap();

        // Corrupt the primary file only.
        fs::write(dir.path().join(CONFIG_FILE), b"{ this is not json").unwrap();

        let loaded = load(dir.path()).unwrap().unwrap();
        assert!(loaded.recovered_from_backup);
    }

    #[test]
    fn both_files_corrupt_quarantines_and_errors() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(CONFIG_FILE), b"not json").unwrap();
        fs::write(dir.path().join(BACKUP_FILE), b"also not json").unwrap();

        let result = load(dir.path());
        assert!(matches!(result, Err(ConfigStoreError::BothCorrupt)));

        // The broken primary was moved aside, not deleted.
        assert!(!dir.path().join(CONFIG_FILE).exists());
        let quarantined: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("config.corrupt.")
            })
            .collect();
        assert_eq!(quarantined.len(), 1);
    }

    #[test]
    fn future_schema_version_is_rejected_without_touching_files() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(CONFIG_FILE),
            r#"{"schema_version": 999999}"#,
        )
        .unwrap();

        let result = load(dir.path());
        assert!(matches!(
            result,
            Err(ConfigStoreError::UnsupportedSchemaVersion { .. })
        ));
        assert!(dir.path().join(CONFIG_FILE).exists());
    }
}
