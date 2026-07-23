//! Enumerates Start Menu shortcuts into (name, target executable) pairs, so an
//! app can be registered as a launch target *without its window being open*.
//!
//! This is what makes tray-resident apps registrable at all: CodexBar Desktop
//! keeps its real window hidden until summoned, so there is nothing to pick from
//! the live-window list — but its Start Menu entry names it and points at its
//! executable. The same path covers "I know the app, I just don't have it open".
//!
//! Shortcut targets are resolved with `IShellLinkW`, the documented way to read
//! a `.lnk`; parsing the binary format by hand would be both fragile and
//! unnecessary. Everything is best-effort: an unreadable or non-executable
//! shortcut is skipped rather than failing the whole enumeration.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::MAX_PATH;
use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile,
    STGM_READ,
};
use windows::Win32::UI::Shell::{IShellLinkW, SLGP_UNCPRIORITY, ShellLink};
use windows::core::{HSTRING, Interface};

/// One Start Menu entry that resolves to a runnable executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartMenuApp {
    /// The shortcut's display name (its file stem) — e.g. "CodexBar Desktop".
    pub name: String,
    /// The executable the shortcut points at.
    pub target: PathBuf,
    /// The shortcut's own arguments, if any (kept so a wrapper shortcut that
    /// passes flags still launches the way the user expects).
    pub args: String,
}

/// The two Start Menu roots: the current user's and the all-users one.
fn start_menu_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(
            PathBuf::from(appdata)
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs"),
        );
    }
    if let Some(program_data) = std::env::var_os("ProgramData") {
        roots.push(
            PathBuf::from(program_data)
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs"),
        );
    }
    roots
}

/// Every Start Menu shortcut that resolves to an existing `.exe`, de-duplicated
/// by target and sorted by name. Uninstallers and other noise are filtered out.
pub fn enumerate() -> Vec<StartMenuApp> {
    let mut shortcuts = Vec::new();
    for root in start_menu_roots() {
        collect_shortcuts(&root, 0, &mut shortcuts);
    }

    let mut apps: Vec<StartMenuApp> = shortcuts
        .iter()
        .filter_map(|link| resolve_shortcut(link))
        .filter(|app| !is_noise(&app.name))
        .collect();

    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps.dedup_by(|a, b| a.target == b.target && a.args == b.args);
    apps
}

/// Recursively collects `*.lnk` paths under `dir`. Depth-limited: Start Menu
/// trees are shallow, and a bounded walk keeps a pathological directory from
/// stalling the UI thread's caller.
fn collect_shortcuts(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    const MAX_DEPTH: usize = 4;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_shortcuts(&path, depth + 1, out);
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"))
        {
            out.push(path);
        }
    }
}

/// Shortcut names that are never useful as a managed app.
fn is_noise(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("uninstall")
        || lower.contains("アンインストール")
        || lower.contains("readme")
        || lower.contains("release notes")
}

/// Resolves one `.lnk` to its target executable via `IShellLinkW`. `None` when
/// the shortcut can't be read, doesn't point at an `.exe`, or the target is
/// missing (a stale shortcut).
fn resolve_shortcut(link_path: &Path) -> Option<StartMenuApp> {
    ensure_com();

    // SAFETY: `ShellLink` is a documented in-proc COM class; the returned
    // interface is dropped (released) at the end of this function.
    let link: IShellLinkW =
        unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }.ok()?;

    // SAFETY: `link` implements `IPersistFile`; the path is a valid wide string.
    let persist: IPersistFile = link.cast().ok()?;
    unsafe { persist.Load(&HSTRING::from(link_path.as_os_str()), STGM_READ) }.ok()?;

    let mut buf = [0u16; MAX_PATH as usize];
    let mut find_data = WIN32_FIND_DATAW::default();
    // SAFETY: `buf` is a valid, correctly-sized out buffer; `find_data` is a
    // fully-initialised out parameter the call fills in.
    unsafe { link.GetPath(&mut buf, &mut find_data, SLGP_UNCPRIORITY.0 as u32) }.ok()?;
    let target = PathBuf::from(wide_to_string(&buf));

    if !target
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        || !target.exists()
    {
        return None;
    }

    let mut arg_buf = [0u16; 1024];
    // SAFETY: same contract as `GetPath` — a valid, sized out buffer.
    let args = if unsafe { link.GetArguments(&mut arg_buf) }.is_ok() {
        wide_to_string(&arg_buf)
    } else {
        String::new()
    };

    Some(StartMenuApp {
        name: link_path.file_stem()?.to_string_lossy().into_owned(),
        target,
        args,
    })
}

/// Truncates a null-terminated wide buffer and converts it to a `String`.
fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Initialises COM for the calling thread, tolerating "already initialised"
/// (including a different apartment) — same best-effort contract as
/// `windowing::browser_url`.
fn ensure_com() {
    // SAFETY: no preconditions; a failed/duplicate initialisation is ignored
    // because the caller only reads shell objects afterwards.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_filter_drops_uninstallers_and_docs() {
        assert!(is_noise("LINE アンインストール"));
        assert!(is_noise("Uninstall Foo"));
        assert!(is_noise("ReadMe"));
        assert!(!is_noise("CodexBar Desktop"));
        assert!(!is_noise("LibreHardwareMonitor HUD"));
    }

    #[test]
    fn wide_to_string_stops_at_the_null_terminator() {
        let mut buf = [0u16; 8];
        for (i, c) in "abc".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(wide_to_string(&buf), "abc");
        assert_eq!(wide_to_string(&[0u16; 4]), "");
    }

    #[test]
    fn start_menu_roots_are_under_start_menu_programs() {
        for root in start_menu_roots() {
            assert!(root.ends_with(Path::new("Start Menu").join("Programs")));
        }
    }

    /// Environment-dependent smoke test: enumeration must not panic and, on a
    /// normal Windows install, finds at least one app.
    #[test]
    fn enumerate_does_not_panic() {
        let apps = enumerate();
        for app in &apps {
            assert!(!app.name.is_empty());
            assert!(app.target.extension().is_some());
        }
    }
}
