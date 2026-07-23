//! Workset domain types: repositories, managed windows, and parking policy
//! (PLAN.md §2.1, §7.2).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::config::HotkeyConfig;
use crate::domain::placement::SavedPlacement;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryKind {
    Git,
    Directory,
    /// A VS Code multi-root workspace file (`*.code-workspace`); the
    /// `repository_path` points at the file itself.
    Workspace,
}

/// One repository/project's set of managed top-level windows (PLAN.md §2.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workset {
    pub id: Uuid,
    pub name: String,
    pub repository_path: PathBuf,
    pub repository_kind: RepositoryKind,
    pub color: String,
    pub sort_order: i32,
    pub direct_hotkey: Option<HotkeyConfig>,
    pub parking_policy: ParkingPolicy,
    /// When `true`, this workset's windows are maximized on their parking
    /// monitor after being parked (PLAN.md §2.4 extension) — e.g. a video kept
    /// full-screen on a secondary monitor while another set is on the main
    /// screen. `#[serde(default)]` keeps pre-existing configs loadable.
    #[serde(default)]
    pub fullscreen_when_parked: bool,
    pub windows: Vec<ManagedWindow>,
    /// RFC 3339 UTC timestamp.
    pub created_at: String,
    /// RFC 3339 UTC timestamp.
    pub updated_at: String,
}

/// One top-level window belonging to a [`Workset`] (PLAN.md §5.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagedWindow {
    pub id: Uuid,
    pub matcher: WindowMatcher,
    pub main_placement: SavedPlacement,
    pub z_order: i32,
    /// How to re-launch this window's app if it has been closed (the
    /// "閉じたアプリを開き直す" feature). `None` for windows registered before
    /// this field existed, or apps whose launch couldn't be determined.
    #[serde(default)]
    pub launch_spec: Option<LaunchSpec>,
}

/// An app the user has registered as a launch candidate (the "起動候補" list).
///
/// Registration has three routes, all of which end here: type a name and a path,
/// browse for the executable, or pick a Start Menu entry (which fills both from
/// the shortcut — see `windowing::start_menu`). A workset then *references* one
/// of these instead of naming an executable itself, so the same app can be
/// attached to several sets and re-pointed in one place when it moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchApp {
    pub id: Uuid,
    /// Display name, and the title hint used to match the app's window.
    pub name: String,
    /// The executable to run.
    pub program: PathBuf,
    /// Default arguments (e.g. those carried by a Start Menu shortcut).
    #[serde(default)]
    pub args: String,
}

/// How to relaunch a managed window's application after it was closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSpec {
    /// The executable to run (the window's recorded `executable_path`).
    pub program: PathBuf,
    /// Arguments: the repository folder/`.code-workspace` for VS Code, the URL
    /// for a browser, empty otherwise.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub kind: LaunchKind,
    /// Application User Model ID for a packaged (MSIX/Store) app, e.g.
    /// `OpenAI.Codex_2p2nqsd0c76g0!App`. Preferred over [`Self::program`] at
    /// launch time because a Store app's install path contains its version and
    /// therefore changes on every update; `program` stays as the fallback.
    #[serde(default)]
    pub aumid: Option<String>,
}

/// The app family a [`LaunchSpec`] targets, deciding how its args are built at
/// registration and how it's relaunched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LaunchKind {
    #[default]
    Generic,
    VsCode,
    Browser,
}

/// Where a [`Workset`] goes when it is not the current (main-screen) set (PLAN.md §2.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParkingPolicy {
    Auto,
    Fixed {
        slot_id: Uuid,
    },
    /// Park onto a named [`SubScreen`](crate::domain::config::SubScreen).
    SubScreen {
        sub_screen_id: Uuid,
    },
}

/// A non-main monitor's grid cell reserved for one specific workset (PLAN.md §2.5, §3.7).
///
/// `monitor_id` matches a [`SavedMonitor::stable_id`](crate::domain::monitor::SavedMonitor).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixedParkingSlot {
    pub id: Uuid,
    pub monitor_id: String,
    pub grid: crate::domain::monitor::AutoSplit,
    pub cell_index: usize,
    pub assigned_workset_id: Uuid,
}

/// Re-matches a re-enumerated window against its registration (PLAN.md §5.3-§5.5).
///
/// Never persists an HWND: identity is executable path, class, and title heuristics,
/// re-validated every time a window is bound (PLAN.md §5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowMatcher {
    pub executable_path: PathBuf,
    pub process_name: String,
    pub window_class: String,
    pub registered_title: String,
    pub title_contains: Option<String>,
    pub title_regex: Option<String>,
}
