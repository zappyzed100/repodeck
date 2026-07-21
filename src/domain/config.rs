//! `config.json`'s domain model, defaults, and validation (PLAN.md §3.1, §7.2, Phase 3).

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::monitor::SavedMonitor;
use crate::domain::workset::{FixedParkingSlot, ParkingPolicy, Workset};

/// `config.json`'s current schema version (PLAN.md §7.6). Bump this, and add a
/// migration in `persistence::migrations`, whenever a field is added or changed.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub schema_version: u32,
    pub app_version: String,
    pub settings: UserSettings,
    pub monitors: Vec<SavedMonitor>,
    pub main_monitor_ids: Vec<String>,
    pub worksets: Vec<Workset>,
    pub fixed_slots: Vec<FixedParkingSlot>,
}

impl AppConfig {
    /// A fresh config as produced by the first-run wizard before the user has
    /// selected any main monitors or registered any worksets (PLAN.md §3.1).
    pub fn new_empty() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            settings: UserSettings::default(),
            monitors: Vec::new(),
            main_monitor_ids: Vec::new(),
            worksets: Vec::new(),
            fixed_slots: Vec::new(),
        }
    }

    /// Runs every rule from PLAN.md §3.5/§Phase 3 バリデーション, returning every
    /// violation found (not just the first) so the UI can show a full warning list.
    pub fn validate(&self) -> Vec<ConfigValidationError> {
        let mut errors = Vec::new();

        if self.main_monitor_ids.is_empty() {
            errors.push(ConfigValidationError::NoMainMonitor);
        }

        if self.settings.quick_switcher_hotkey.modifiers.is_empty()
            && !self.settings.quick_switcher_hotkey.allows_empty_modifiers()
        {
            errors.push(ConfigValidationError::HotkeyMissingModifier {
                context: "settings.quick_switcher_hotkey".to_string(),
            });
        }

        let mut seen_workset_ids: HashSet<Uuid> = HashSet::new();
        let mut seen_window_ids: HashSet<Uuid> = HashSet::new();
        let mut seen_matchers: HashSet<(PathBuf, String, String)> = HashSet::new();

        for workset in &self.worksets {
            if !seen_workset_ids.insert(workset.id) {
                errors.push(ConfigValidationError::DuplicateWorksetId { id: workset.id });
            }

            let name_len = workset.name.chars().count();
            if !(1..=80).contains(&name_len) {
                errors.push(ConfigValidationError::WorksetNameLength {
                    name: workset.name.clone(),
                });
            }

            // An empty path means "no repository" (worksets can be registered
            // without one); only a non-empty path must be absolute.
            if !workset.repository_path.as_os_str().is_empty()
                && !workset.repository_path.is_absolute()
            {
                errors.push(ConfigValidationError::RepositoryPathNotAbsolute {
                    path: workset.repository_path.clone(),
                });
            }

            if let Some(hotkey) = &workset.direct_hotkey
                && hotkey.modifiers.is_empty()
                && !hotkey.allows_empty_modifiers()
            {
                errors.push(ConfigValidationError::HotkeyMissingModifier {
                    context: format!("workset {} direct_hotkey", workset.id),
                });
            }

            if let ParkingPolicy::Fixed { slot_id } = &workset.parking_policy {
                match self.fixed_slots.iter().find(|slot| slot.id == *slot_id) {
                    None => errors.push(ConfigValidationError::MissingFixedSlot {
                        workset_id: workset.id,
                        slot_id: *slot_id,
                    }),
                    Some(slot) if slot.assigned_workset_id != workset.id => {
                        errors.push(ConfigValidationError::InconsistentFixedAssignment {
                            slot_id: *slot_id,
                            assigned: slot.assigned_workset_id,
                            workset_id: workset.id,
                        });
                    }
                    Some(_) => {}
                }
            }

            for window in &workset.windows {
                if !seen_window_ids.insert(window.id) {
                    errors.push(ConfigValidationError::DuplicateManagedWindowId { id: window.id });
                }

                let matcher_key = (
                    window.matcher.executable_path.clone(),
                    window.matcher.window_class.clone(),
                    window.matcher.registered_title.clone(),
                );
                if !seen_matchers.insert(matcher_key) {
                    errors.push(ConfigValidationError::DuplicateWindowMatcher {
                        executable_path: window.matcher.executable_path.display().to_string(),
                        window_class: window.matcher.window_class.clone(),
                        registered_title: window.matcher.registered_title.clone(),
                    });
                }

                if let Some(pattern) = &window.matcher.title_regex
                    && let Err(source) = regex::Regex::new(pattern)
                {
                    errors.push(ConfigValidationError::InvalidTitleRegex {
                        workset_id: workset.id,
                        window_id: window.id,
                        source,
                    });
                }
            }
        }

        let mut seen_slots: HashSet<(String, usize)> = HashSet::new();
        for slot in &self.fixed_slots {
            if !seen_slots.insert((slot.monitor_id.clone(), slot.cell_index)) {
                errors.push(ConfigValidationError::DuplicateFixedSlot {
                    monitor_id: slot.monitor_id.clone(),
                    grid: slot.grid,
                    cell_index: slot.cell_index,
                });
            }

            if !seen_workset_ids.contains(&slot.assigned_workset_id) {
                errors.push(ConfigValidationError::MissingAssignedWorkset {
                    slot_id: slot.id,
                    workset_id: slot.assigned_workset_id,
                });
            }
        }

        errors
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSettings {
    pub quick_switcher_hotkey: HotkeyConfig,
    pub popup_location: PopupLocation,
    pub close_on_focus_loss: bool,
    pub close_after_switch: bool,
    pub unknown_window_policy: UnknownWindowPolicy,
    pub sort_mode: SortMode,
    pub notify_needs_input: bool,
    pub notify_ready: bool,
    pub start_with_windows: bool,
    /// When `true` (default), RepoDeck automatically attempts a software display
    /// re-detect after a resume-from-sleep if saved monitors are missing from the
    /// live topology (PLAN.md §4.6, Phase 9 resilience). The manual tray trigger
    /// ("モニターを再検出") works regardless of this flag. `#[serde(default)]` keeps
    /// configs written before this field was added loadable.
    #[serde(default = "default_auto_display_recovery")]
    pub auto_display_recovery: bool,
}

/// serde default for [`UserSettings::auto_display_recovery`]: auto-recovery is ON
/// unless a config explicitly disables it.
fn default_auto_display_recovery() -> bool {
    true
}

impl Default for UserSettings {
    /// Defaults from PLAN.md §3.1.
    fn default() -> Self {
        Self {
            quick_switcher_hotkey: HotkeyConfig {
                modifiers: vec![HotkeyModifier::Control, HotkeyModifier::Alt],
                virtual_key: u32::from(b'W'),
            },
            popup_location: PopupLocation::CursorMonitorCenter,
            close_on_focus_loss: true,
            close_after_switch: true,
            unknown_window_policy: UnknownWindowPolicy::Ask,
            sort_mode: SortMode::Manual,
            notify_needs_input: true,
            notify_ready: true,
            start_with_windows: false,
            auto_display_recovery: default_auto_display_recovery(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyConfig {
    pub modifiers: Vec<HotkeyModifier>,
    pub virtual_key: u32,
}

impl HotkeyConfig {
    /// Whether this hotkey is valid with an empty `modifiers` list. Only
    /// function keys (`VK_F1`..`VK_F24`, `0x70..=0x87`) qualify: registering
    /// a bare letter/digit/arrow/space system-wide would steal that key from
    /// normal typing in every application, while F13-F24 (and unused
    /// F1-F12) exist precisely for dedicated bindings.
    pub fn allows_empty_modifiers(&self) -> bool {
        (0x70..=0x87).contains(&self.virtual_key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyModifier {
    Alt,
    Control,
    Shift,
    Win,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PopupLocation {
    CursorMonitorCenter,
    MainMonitorCenter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownWindowPolicy {
    Ask,
    LeaveInPlace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortMode {
    Manual,
    Name,
    Recent,
}

#[derive(Debug, Error)]
pub enum ConfigValidationError {
    #[error("workset name must be 1-80 characters: {name:?}")]
    WorksetNameLength { name: String },
    #[error("repository path must be absolute: {}", path.display())]
    RepositoryPathNotAbsolute { path: PathBuf },
    #[error("duplicate workset id: {id}")]
    DuplicateWorksetId { id: Uuid },
    #[error("duplicate managed window id: {id}")]
    DuplicateManagedWindowId { id: Uuid },
    #[error(
        "window matcher registered to more than one workset: {executable_path} / {window_class} / {registered_title}"
    )]
    DuplicateWindowMatcher {
        executable_path: String,
        window_class: String,
        registered_title: String,
    },
    #[error("duplicate fixed parking slot: monitor {monitor_id} grid {grid:?} cell {cell_index}")]
    DuplicateFixedSlot {
        monitor_id: String,
        grid: crate::domain::monitor::AutoSplit,
        cell_index: usize,
    },
    #[error("workset {workset_id} references missing fixed parking slot {slot_id}")]
    MissingFixedSlot { workset_id: Uuid, slot_id: Uuid },
    #[error("fixed parking slot {slot_id} is assigned to missing workset {workset_id}")]
    MissingAssignedWorkset { slot_id: Uuid, workset_id: Uuid },
    #[error(
        "fixed parking slot {slot_id} assigned_workset_id ({assigned}) does not match workset {workset_id}'s parking_policy"
    )]
    InconsistentFixedAssignment {
        slot_id: Uuid,
        assigned: Uuid,
        workset_id: Uuid,
    },
    #[error("at least one main monitor is required")]
    NoMainMonitor,
    #[error("invalid title_regex on workset {workset_id} window {window_id}: {source}")]
    InvalidTitleRegex {
        workset_id: Uuid,
        window_id: Uuid,
        #[source]
        source: regex::Error,
    },
    #[error("hotkey must have at least one modifier unless the key is a function key ({context})")]
    HotkeyMissingModifier { context: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_missing_a_main_monitor() {
        let config = AppConfig::new_empty();
        let errors = config.validate();
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, ConfigValidationError::NoMainMonitor))
        );
    }

    #[test]
    fn config_with_main_monitor_and_default_settings_has_no_errors() {
        let mut config = AppConfig::new_empty();
        config.main_monitor_ids.push("\\\\.\\DISPLAY1".to_string());
        assert!(config.validate().is_empty());
    }

    #[test]
    fn hotkey_without_modifiers_is_rejected() {
        let mut config = AppConfig::new_empty();
        config.main_monitor_ids.push("\\\\.\\DISPLAY1".to_string());
        config.settings.quick_switcher_hotkey.modifiers.clear();

        let errors = config.validate();
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, ConfigValidationError::HotkeyMissingModifier { .. }))
        );
    }

    #[test]
    fn function_key_hotkey_without_modifiers_is_accepted() {
        let mut config = AppConfig::new_empty();
        config.main_monitor_ids.push("\\\\.\\DISPLAY1".to_string());
        config.settings.quick_switcher_hotkey.modifiers.clear();

        for vk in [0x70, 0x7B, 0x7C, 0x87] {
            // VK_F1, VK_F12, VK_F13, VK_F24.
            config.settings.quick_switcher_hotkey.virtual_key = vk;
            assert!(
                config.validate().is_empty(),
                "VK 0x{vk:02X} should be registrable without modifiers"
            );
        }
    }

    #[test]
    fn non_function_key_hotkey_without_modifiers_is_rejected() {
        let mut config = AppConfig::new_empty();
        config.main_monitor_ids.push("\\\\.\\DISPLAY1".to_string());
        config.settings.quick_switcher_hotkey.modifiers.clear();

        for vk in [0x26, 0x6F, 0x88] {
            // VK_UP, VK_DIVIDE (just below VK_F1), one past VK_F24.
            config.settings.quick_switcher_hotkey.virtual_key = vk;
            let errors = config.validate();
            assert!(
                errors
                    .iter()
                    .any(|e| matches!(e, ConfigValidationError::HotkeyMissingModifier { .. })),
                "VK 0x{vk:02X} should require a modifier"
            );
        }
    }
}
