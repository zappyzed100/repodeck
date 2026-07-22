//! Pure logic for the "閉じたアプリを開き直す" feature: deciding how a managed
//! window's app should be relaunched (PLAN.md §5.3 extension).
//!
//! Kept Win32-free and side-effect-free so it can be unit-tested without a
//! desktop. The actual process spawn lives in
//! [`crate::windowing::app_launch`], and the browser-URL capture (which does
//! need UI Automation) in [`crate::windowing::browser_url`].

use std::path::Path;

use crate::domain::workset::{LaunchKind, LaunchSpec};

/// Classifies an executable by file name into the app family we build launch
/// args for. Case-insensitive; unknown apps are [`LaunchKind::Generic`].
pub fn classify(executable: &Path) -> LaunchKind {
    let Some(name) = executable
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
    else {
        return LaunchKind::Generic;
    };
    match name.as_str() {
        "code.exe" | "code - insiders.exe" | "codium.exe" => LaunchKind::VsCode,
        "chrome.exe" | "msedge.exe" | "firefox.exe" | "brave.exe" | "opera.exe" | "vivaldi.exe" => {
            LaunchKind::Browser
        }
        _ => LaunchKind::Generic,
    }
}

/// Whether `executable` is Firefox (which uses `-new-window` rather than the
/// Chromium `--new-window`).
fn is_firefox(executable: &Path) -> bool {
    executable
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|n| n == "firefox.exe")
}

/// Normalizes an address-bar string into something a browser will accept as a
/// launch argument: if it carries no scheme, assume `https://`. Chrome/Edge
/// strip the scheme in the address bar, so this restores it.
pub fn normalize_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

/// Builds the relaunch spec for a window given its executable, the owning
/// workset's repository path (for VS Code), and its captured browser URL (for
/// browsers). Returns `None` for a generic app with nothing to relaunch
/// usefully beyond the bare exe — actually, we still return a spec so the exe
/// can be relaunched; only a missing executable yields `None`.
pub fn build_launch_spec(
    executable: &Path,
    repository_path: Option<&Path>,
    browser_url: Option<&str>,
) -> LaunchSpec {
    let kind = classify(executable);
    let args = match kind {
        LaunchKind::VsCode => repository_path
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| vec![p.display().to_string()])
            .unwrap_or_default(),
        LaunchKind::Browser => {
            // Always open a fresh window (not a tab in an existing one), so each
            // reopened browser entry is its own window — Firefox uses a single
            // dash, Chromium a double dash.
            let new_window_flag = if is_firefox(executable) {
                "-new-window"
            } else {
                "--new-window"
            };
            let mut args = vec![new_window_flag.to_string()];
            if let Some(url) = browser_url.filter(|u| !u.trim().is_empty()) {
                args.push(normalize_url(url));
            }
            args
        }
        LaunchKind::Generic => Vec::new(),
    };
    LaunchSpec {
        program: executable.to_path_buf(),
        args,
        kind,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn classify_detects_vscode_and_browsers_case_insensitively() {
        assert_eq!(classify(Path::new(r"C:\VS\Code.exe")), LaunchKind::VsCode);
        assert_eq!(
            classify(Path::new(r"C:\ch\CHROME.EXE")),
            LaunchKind::Browser
        );
        assert_eq!(classify(Path::new(r"C:\e\msedge.exe")), LaunchKind::Browser);
        assert_eq!(
            classify(Path::new(r"C:\x\notepad.exe")),
            LaunchKind::Generic
        );
        assert_eq!(classify(Path::new("")), LaunchKind::Generic);
    }

    #[test]
    fn normalize_url_adds_https_when_scheme_missing() {
        assert_eq!(normalize_url("example.com/x"), "https://example.com/x");
        assert_eq!(normalize_url("  example.com  "), "https://example.com");
        assert_eq!(normalize_url("http://a.b"), "http://a.b");
        assert_eq!(normalize_url("https://a.b/c"), "https://a.b/c");
    }

    #[test]
    fn vscode_spec_uses_repository_path_as_arg() {
        let spec = build_launch_spec(
            Path::new(r"C:\VS\Code.exe"),
            Some(Path::new(r"D:\repo")),
            None,
        );
        assert_eq!(spec.kind, LaunchKind::VsCode);
        assert_eq!(spec.program, PathBuf::from(r"C:\VS\Code.exe"));
        assert_eq!(spec.args, vec![r"D:\repo".to_string()]);
    }

    #[test]
    fn vscode_spec_without_repo_has_no_args() {
        let spec = build_launch_spec(Path::new(r"C:\VS\Code.exe"), None, None);
        assert!(spec.args.is_empty());
    }

    #[test]
    fn browser_spec_normalizes_the_captured_url() {
        let spec = build_launch_spec(
            Path::new(r"C:\ch\chrome.exe"),
            None,
            Some("example.com/path"),
        );
        assert_eq!(spec.kind, LaunchKind::Browser);
        assert_eq!(
            spec.args,
            vec![
                "--new-window".to_string(),
                "https://example.com/path".to_string()
            ]
        );
    }

    #[test]
    fn firefox_uses_single_dash_new_window_flag() {
        let spec = build_launch_spec(Path::new(r"C:\ff\firefox.exe"), None, Some("https://a.b"));
        assert_eq!(
            spec.args,
            vec!["-new-window".to_string(), "https://a.b".to_string()]
        );
    }

    #[test]
    fn generic_spec_relaunches_the_bare_exe() {
        let spec = build_launch_spec(
            Path::new(r"C:\x\notepad.exe"),
            Some(Path::new(r"D:\r")),
            None,
        );
        assert_eq!(spec.kind, LaunchKind::Generic);
        assert!(spec.args.is_empty());
    }
}
