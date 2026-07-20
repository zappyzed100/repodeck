//! Schema-version gatekeeping and migrations for `config.json` (PLAN.md §7.6).

use serde_json::Value;

use crate::domain::config::CURRENT_SCHEMA_VERSION;
use crate::persistence::config_store::ConfigStoreError;

/// Rejects config files from a schema version newer than this build understands
/// (PLAN.md §7.6: "未知の新しいschemaは開かず、更新を促す").
pub fn ensure_supported(found_version: u32) -> Result<(), ConfigStoreError> {
    if found_version > CURRENT_SCHEMA_VERSION {
        return Err(ConfigStoreError::UnsupportedSchemaVersion {
            found: found_version,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }
    Ok(())
}

/// Migrates a raw config JSON value from `from_version` up to
/// [`CURRENT_SCHEMA_VERSION`]. MVP ships only schema version 1, so there are no
/// migration steps yet; the first schema bump adds a step here rather than
/// restructuring the load path (PLAN.md §7.6: "古いschemaはmigrationsモジュールで段階移行").
///
/// `from_version < CURRENT_SCHEMA_VERSION` (including a missing/zero
/// `schema_version` field) is rejected rather than guessed at, since MVP has no
/// migration path to run for it yet.
pub fn migrate(value: Value, from_version: u32) -> Result<Value, ConfigStoreError> {
    if from_version == CURRENT_SCHEMA_VERSION {
        return Ok(value);
    }

    // Example for the future, once schema_version 2 exists:
    // if from_version == 1 { return migrate_v1_to_v2(value).and_then(|v| migrate(v, 2)); }

    Err(ConfigStoreError::UnsupportedSchemaVersion {
        found: from_version,
        supported: CURRENT_SCHEMA_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn future_schema_version_is_rejected() {
        let result = ensure_supported(CURRENT_SCHEMA_VERSION + 1);
        assert!(matches!(
            result,
            Err(ConfigStoreError::UnsupportedSchemaVersion { .. })
        ));
    }

    #[test]
    fn current_schema_version_is_accepted() {
        assert!(ensure_supported(CURRENT_SCHEMA_VERSION).is_ok());
    }

    #[test]
    fn migrating_from_current_version_is_a_no_op() {
        let value = serde_json::json!({"schema_version": CURRENT_SCHEMA_VERSION});
        let migrated = migrate(value.clone(), CURRENT_SCHEMA_VERSION).unwrap();
        assert_eq!(migrated, value);
    }
}
