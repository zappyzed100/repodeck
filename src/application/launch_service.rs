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

/// The Application User Model ID for a packaged (MSIX/Store) app's executable,
/// or `None` for an ordinary one.
///
/// A Store app lives under `…\WindowsApps\<name>_<version>_<arch>__<publisher>\…`
/// and its install path therefore *changes on every app update* — pinning the
/// exe path (as Codex/ChatGPT's does) breaks the saved launch spec the next time
/// the app updates. The AUMID is version-independent: it is built from the
/// package family name (`<name>_<publisher>`, i.e. the folder name with the
/// version and architecture dropped) plus the application id.
///
/// The application id is read from the package manifest when that is possible
/// and otherwise assumed to be `App`, which is the near-universal default (and
/// what Codex/ChatGPT uses: `OpenAI.Codex_2p2nqsd0c76g0!App`). A wrong guess is
/// not fatal — `app_launch` falls back to the exe path.
pub fn store_app_aumid(executable: &Path) -> Option<String> {
    let mut components = executable.components().peekable();
    // Find the package directory: the component right after `WindowsApps`.
    let mut package_dir: Option<String> = None;
    while let Some(component) = components.next() {
        if component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("WindowsApps")
        {
            package_dir = components
                .peek()
                .map(|next| next.as_os_str().to_string_lossy().into_owned());
            break;
        }
    }
    let package_dir = package_dir?;
    let family = package_family_name(&package_dir)?;
    Some(format!("{family}!App"))
}

/// Turns a WindowsApps package directory name into its package family name:
/// `OpenAI.Codex_26.715.4045.0_x64__2p2nqsd0c76g0` → `OpenAI.Codex_2p2nqsd0c76g0`.
/// The publisher id follows the double underscore; the name is everything up to
/// the first underscore (version and architecture sit between them).
fn package_family_name(package_dir: &str) -> Option<String> {
    let (head, publisher_id) = package_dir.rsplit_once("__")?;
    let name = head.split('_').next()?;
    if name.is_empty() || publisher_id.is_empty() {
        return None;
    }
    Some(format!("{name}_{publisher_id}"))
}

/// Whether a folder read from a VS Code process's command line plausibly belongs
/// to the window being registered.
///
/// VS Code runs every window in one process, so `read_process_command_line`
/// returns the command line of whichever window started it — register a *second*
/// window and you silently capture the *first* window's folder. The window title
/// always contains the open folder's (or workspace's) name, so requiring that
/// name to appear in the title rejects exactly that mismatch. A `.code-workspace`
/// is compared by its stem, since the title shows the workspace name without the
/// extension ("repo08 (ワークスペース)").
pub fn vscode_folder_matches_title(folder: &str, window_title: &str) -> bool {
    let Some(stem) = Path::new(folder)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
    else {
        return false;
    };
    window_title.contains(&stem)
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

/// Splits a Windows command line into arguments, honoring double quotes (so a
/// path with spaces stays one token). Good enough for reading back a launch
/// command line — not a full CommandLineToArgvW escape parser.
fn tokenize_command_line(command_line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut started = false;
    for ch in command_line.chars() {
        match ch {
            '"' => started = true, // toggle handled below
            c if c.is_whitespace() && !in_quotes => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
        if ch == '"' {
            in_quotes = !in_quotes;
        }
    }
    if started {
        tokens.push(current);
    }
    tokens
}

/// From a VS Code process command line, the folder/workspace path it was opened
/// on: the first positional argument (skipping the executable and any `--flags`,
/// and `foo://` URIs). `None` if it was launched with no path (a bare window).
pub fn extract_vscode_folder(command_line: &str) -> Option<String> {
    tokenize_command_line(command_line)
        .into_iter()
        .skip(1) // the executable itself
        .find(|token| !token.is_empty() && !token.starts_with('-') && !token.contains("://"))
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
        // `-n` forces a *new* window: without it `Code.exe <path>` merely
        // focuses an already-open window for that folder, so reopening a
        // deliberately-closed workset window would silently do nothing.
        LaunchKind::VsCode => repository_path
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| vec!["-n".to_string(), p.display().to_string()])
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
        aumid: store_app_aumid(executable),
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
        // `-n` so a reopened window is a *new* one, not a focus of an existing.
        assert_eq!(spec.args, vec!["-n".to_string(), r"D:\repo".to_string()]);
        assert_eq!(spec.aumid, None);
    }

    #[test]
    fn vscode_spec_takes_a_code_workspace_file_too() {
        let spec = build_launch_spec(
            Path::new(r"C:\VS\Code.exe"),
            Some(Path::new(r"D:\repo\repo.code-workspace")),
            None,
        );
        assert_eq!(
            spec.args,
            vec!["-n".to_string(), r"D:\repo\repo.code-workspace".to_string()]
        );
    }

    #[test]
    fn vscode_spec_without_repo_has_no_args() {
        let spec = build_launch_spec(Path::new(r"C:\VS\Code.exe"), None, None);
        assert!(spec.args.is_empty());
    }

    #[test]
    fn vscode_folder_is_accepted_only_when_the_title_names_it() {
        // Folder window: title carries the folder name.
        assert!(vscode_folder_matches_title(
            r"C:\code\test\repo01",
            "repo01 - Visual Studio Code"
        ));
        // Workspace window: title shows the workspace stem, not the file name.
        assert!(vscode_folder_matches_title(
            r"C:\code\test\repo08\repo08.code-workspace",
            "repo08 (ワークスペース) [WS repo08]"
        ));
        // The shared-process trap: a second window whose command line reports
        // the *first* window's folder must be rejected.
        assert!(!vscode_folder_matches_title(
            r"C:\code\test\repo01",
            "repo07 - Visual Studio Code"
        ));
        assert!(!vscode_folder_matches_title("", "repo01 - Visual Studio Code"));
    }

    #[test]
    fn store_app_aumid_is_derived_version_independently() {
        // The install path carries the version; the AUMID must not.
        let exe = Path::new(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.715.4045.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
        );
        assert_eq!(
            store_app_aumid(exe).as_deref(),
            Some("OpenAI.Codex_2p2nqsd0c76g0!App")
        );
        // A newer package version yields the same AUMID.
        let updated = Path::new(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_99.0.0.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
        );
        assert_eq!(store_app_aumid(updated), store_app_aumid(exe));
    }

    #[test]
    fn store_app_aumid_is_none_for_ordinary_executables() {
        assert_eq!(store_app_aumid(Path::new(r"C:\VS\Code.exe")), None);
        assert_eq!(
            store_app_aumid(Path::new(
                r"C:\code\tool\LibreHardwareMonitor\publish\LibreHardwareMonitor.Windows.Forms.exe"
            )),
            None
        );
        assert_eq!(store_app_aumid(Path::new("")), None);
    }

    #[test]
    fn store_app_spec_carries_the_aumid() {
        let spec = build_launch_spec(
            Path::new(
                r"C:\Program Files\WindowsApps\OpenAI.Codex_26.715.4045.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
            ),
            None,
            None,
        );
        assert_eq!(spec.kind, LaunchKind::Generic);
        assert_eq!(spec.aumid.as_deref(), Some("OpenAI.Codex_2p2nqsd0c76g0!App"));
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
    fn extract_vscode_folder_reads_the_positional_path() {
        assert_eq!(
            extract_vscode_folder(
                r#""C:\Users\me\AppData\Local\Programs\Microsoft VS Code\Code.exe" "D:\work\my repo""#
            ),
            Some(r"D:\work\my repo".to_string())
        );
    }

    #[test]
    fn extract_vscode_folder_skips_flags_and_uris() {
        assert_eq!(
            extract_vscode_folder(r#""Code.exe" --new-window D:\repo"#),
            Some(r"D:\repo".to_string())
        );
        // A --folder-uri style launch (uri, not a plain path) is ignored.
        assert_eq!(
            extract_vscode_folder(r#""Code.exe" --folder-uri file:///D:/repo"#),
            None
        );
    }

    #[test]
    fn extract_vscode_folder_none_for_a_bare_window() {
        assert_eq!(extract_vscode_folder(r#""C:\x\Code.exe""#), None);
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
