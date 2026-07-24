//! Pure logic for the "閉じたアプリを開き直す" feature: deciding how a managed
//! window's app should be relaunched (PLAN.md §5.3 extension).
//!
//! Kept Win32-free and side-effect-free so it can be unit-tested without a
//! desktop. The actual process spawn lives in
//! [`crate::windowing::app_launch`], and the browser-URL capture (which does
//! need UI Automation) in [`crate::windowing::browser_url`].

use std::collections::HashSet;
use std::path::Path;

use crate::domain::workset::{LaunchKind, LaunchSpec};
use crate::windowing::enumerate::TopLevelWindow;

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

/// 「アプリを閉じる」で `WM_CLOSE` ではなくプロセス終了を使うアプリか。
///
/// リモートデスクトップ（mstsc.exe）は `WM_CLOSE` で切断確認を出して居座り、
/// セッションを掴んだままなので開き直しても新しい接続を張れない。編集中の文書を
/// 抱える種類のアプリではないため、終了させて失われるものはない。既定は
/// `WM_CLOSE`——プロセスを殺すのは、ユーザーの確認なしに何かを失いうる操作なので、
/// ここに挙げたものだけの例外に留める。
pub fn closes_only_by_kill(executable: &Path) -> bool {
    executable
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|n| n == "mstsc.exe")
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

/// 起動候補に登録された既定引数（スタートメニューのショートカット由来、または
/// 手入力）をトークンへ分解する。引用符を尊重するので、空白を含むパスが1つの
/// 引数として保たれる（`--user-data-dir="D:\Brave Data\開発"` など）。
pub fn split_registered_args(raw: &str) -> Vec<String> {
    tokenize_command_line(raw)
}

/// ブラウザのウィンドウを「そのアプリの他の窓」と区別できる引数か。
///
/// Chromium 系は `--user-data-dir` を変えると**別プロセスの独立したブラウザ**に
/// なるので、ウィンドウのプロセスのコマンドラインから確実に判別できる。用途ごとに
/// データ領域を分けておけば、Brave のウィンドウ1つ1つを別物として扱える。
///
/// `--profile-directory` は同じ `--user-data-dir` の既存プロセスが窓を開くため、
/// 窓の持ち主プロセスのコマンドラインには現れないことがある。判別には使わない。
pub fn browser_identity_arg(args: &[String]) -> Option<&String> {
    args.iter()
        .find(|a| a.starts_with("--user-data-dir"))
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

/// 登録アプリから宣言したウィンドウの `title_contains`：起動入力のうち、実際に
/// ウィンドウタイトルへ現れ、かつそのアプリの他のウィンドウと区別できる部分。
///
/// VS Code のタイトルは開いているフォルダ（またはワークスペース）名を含むので
/// その stem が使える。ブラウザのタイトルは *ページ* のもので起動 URL とは無関係、
/// 汎用アプリの引数もタイトルではない。どちらも `None` にする——アプリのどの
/// ウィンドウにも一致してしまう針を入れるくらいなら、針なしのほうがよい。
pub fn declared_title_needle(kind: LaunchKind, input: &str) -> Option<String> {
    if kind != LaunchKind::VsCode {
        return None;
    }
    Path::new(input.trim())
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
}

/// 既存セットの VS Code エントリの `title_contains` を、アプリ名から開いている
/// フォルダ名へ入れ替える。入れ替えた数を返す。
///
/// 登録アプリからの宣言は当初アプリ名をそのまま針にしていた。これだと 1 セットに
/// VS Code が 2 つあっても——別セットの VS Code とすら——見分けがつかない。起動
/// 引数には開くフォルダが入っているので、そこから本来の針を復元できる。
///
/// ブラウザや汎用アプリの針は触らない。タイトルから復元できる識別子が無く、
/// 針を外すと点数が閾値に届かず「起動中の窓を取り込む」動作まで失われるため。
/// そちらの取り違えは、バインドが切れたときに
/// [`crate::windowing::matcher::has_title_evidence`] が弾く。
pub fn retitle_declared_vscode_windows(worksets: &mut [crate::domain::workset::Workset]) -> usize {
    let mut changed = 0;
    for workset in worksets.iter_mut() {
        for window in &mut workset.windows {
            let matcher = &window.matcher;
            // アプリ名がそのまま針になっているもの＝宣言時の既定値のままのもの。
            if matcher.title_contains.as_deref() != Some(matcher.registered_title.as_str()) {
                continue;
            }
            let Some(spec) = &window.launch_spec else {
                continue;
            };
            if spec.kind != LaunchKind::VsCode {
                continue;
            }
            let Some(folder) = spec.args.iter().find(|a| !a.starts_with('-')) else {
                continue;
            };
            let Some(needle) = declared_title_needle(LaunchKind::VsCode, folder) else {
                continue;
            };
            window.matcher.title_contains = Some(needle);
            changed += 1;
        }
    }
    changed
}

/// 再起動したエントリが結びつくべき、新しく現れたウィンドウを選ぶ。
///
/// 登録アプリから宣言したセットはウィンドウクラスを持たない（宣言時点でアプリが
/// 起動している——どころかインストールされている——とは限らない）。よって空の
/// `class` は「未知」とし、実行ファイルだけで決める。クラス一致を要求していた
/// ため、宣言アプリは *一つも* 紐づかなかった：VS Code、LibreHardwareMonitor、
/// リモートデスクトップはいずれも起動だけして未バインドのまま残っていた。
///
/// 実行ファイルはまずフルパス、次にファイル名で比較する。ストアアプリは AUMID
/// 経由で起動され、そのウィンドウのプロセスは *バージョン付き* の `WindowsApps`
/// ディレクトリに居るので、アプリが更新された途端フルパスは登録値と一致しなくなる。
///
/// クラスは絞り込みの *ヒント* であって条件ではない。WinForms のクラス名は
/// `WindowsForms10.Window.8.app.0.21b46d2_r3_ad1` のように実行ごとに変わりうる
/// 部分を含むため、前回学習したクラスに固執すると次回また紐づかなくなる。
pub fn find_launched_window<'a>(
    after: &'a [TopLevelWindow],
    before: &HashSet<isize>,
    claimed: &HashSet<isize>,
    exe: &Path,
    class: &str,
) -> Option<&'a TopLevelWindow> {
    let fresh = |w: &&TopLevelWindow| !before.contains(&w.hwnd) && !claimed.contains(&w.hwnd);
    let same_exe = |w: &&TopLevelWindow| {
        w.executable_path
            .as_deref()
            .is_some_and(|p| crate::windowing::matcher::same_executable(p, exe))
    };
    let file_name = |p: &Path| {
        p.file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
    };
    let same_name = |w: &&TopLevelWindow| {
        file_name(exe).is_some_and(|wanted| {
            w.executable_path
                .as_deref()
                .and_then(file_name)
                .is_some_and(|got| got == wanted)
        })
    };

    after
        .iter()
        .find(|w| fresh(w) && same_exe(w) && !class.is_empty() && w.window_class == class)
        .or_else(|| after.iter().find(|w| fresh(w) && same_exe(w)))
        .or_else(|| after.iter().find(|w| fresh(w) && same_name(w)))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::domain::placement::PixelRect;

    fn window(hwnd: isize, exe: &str, class: &str) -> TopLevelWindow {
        TopLevelWindow {
            hwnd,
            process_id: 100,
            executable_path: Some(PathBuf::from(exe)),
            window_class: class.to_string(),
            title: "t".to_string(),
            rect_px: PixelRect::new(0, 0, 800, 600),
        }
    }

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
        assert!(!vscode_folder_matches_title(
            "",
            "repo01 - Visual Studio Code"
        ));
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
        assert_eq!(
            spec.aumid.as_deref(),
            Some("OpenAI.Codex_2p2nqsd0c76g0!App")
        );
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
    fn declared_window_with_no_class_binds_on_the_executable_alone() {
        // 登録アプリ由来のエントリはクラスが空。クラス一致を要求していたため
        // LibreHardwareMonitor もリモートデスクトップも起動後に紐づかなかった。
        let exe = r"C:\tool\LibreHardwareMonitor.Windows.Forms.exe";
        let after = vec![window(11, exe, "WindowsForms10.Window.8.app.0.1")];
        let found =
            find_launched_window(&after, &HashSet::new(), &HashSet::new(), Path::new(exe), "");
        assert_eq!(found.map(|w| w.hwnd), Some(11));
    }

    #[test]
    fn only_a_newly_appeared_unclaimed_window_is_taken() {
        let exe = r"C:\x\mstsc.exe";
        let after = vec![
            window(1, exe, "TscShellContainerClass"),
            window(2, exe, "TscShellContainerClass"),
        ];
        let before: HashSet<isize> = [1].into_iter().collect();
        // 1 は起動前から居たので対象外、2 が選ばれる。
        let first = find_launched_window(&after, &before, &HashSet::new(), Path::new(exe), "");
        assert_eq!(first.map(|w| w.hwnd), Some(2));
        // 2 を別のエントリが確保済みなら、もう渡せる窓はない。
        let claimed: HashSet<isize> = [2].into_iter().collect();
        assert!(find_launched_window(&after, &before, &claimed, Path::new(exe), "").is_none());
    }

    #[test]
    fn a_known_class_narrows_the_choice_but_does_not_gate_it() {
        let exe = r"C:\VS\Code.exe";
        let after = vec![
            window(1, exe, "OtherClass"),
            window(2, exe, "Chrome_WidgetWin_1"),
        ];
        let found = find_launched_window(
            &after,
            &HashSet::new(),
            &HashSet::new(),
            Path::new(exe),
            "Chrome_WidgetWin_1",
        );
        assert_eq!(found.map(|w| w.hwnd), Some(2));

        // WinForms のクラス名は実行ごとに変わりうる。学習済みのクラスと一致する
        // 窓が無くても、実行ファイルが合っていれば結びつける。
        let stale = vec![window(
            3,
            r"C:\tool\LHM.exe",
            "WindowsForms10.Window.8.app.0.21b46d2_r3_ad1",
        )];
        let found = find_launched_window(
            &stale,
            &HashSet::new(),
            &HashSet::new(),
            Path::new(r"C:\tool\LHM.exe"),
            "WindowsForms10.Window.8.app.0.aabbcc_r6_ad1",
        );
        assert_eq!(found.map(|w| w.hwnd), Some(3));
    }

    #[test]
    fn store_app_falls_back_to_the_file_name_when_the_version_directory_moved() {
        // AUMID 起動なので、窓のプロセスは更新後の versioned な WindowsApps に居る。
        let registered = r"C:\Program Files\WindowsApps\OpenAI.Codex_26.715.4045.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe";
        let running = r"C:\Program Files\WindowsApps\OpenAI.Codex_99.0.0.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe";
        let after = vec![window(7, running, "WinUIDesktopWin32WindowClass")];
        let found = find_launched_window(
            &after,
            &HashSet::new(),
            &HashSet::new(),
            Path::new(registered),
            "",
        );
        assert_eq!(found.map(|w| w.hwnd), Some(7));
    }

    #[test]
    fn migration_swaps_the_app_name_needle_for_the_open_folder() {
        use crate::domain::workset::{ManagedWindow, RepositoryKind, WindowMatcher};

        fn declared(app_name: &str, spec: Option<LaunchSpec>) -> ManagedWindow {
            ManagedWindow {
                id: uuid::Uuid::new_v4(),
                matcher: WindowMatcher {
                    executable_path: PathBuf::from(r"C:\x\app.exe"),
                    process_name: "app.exe".to_string(),
                    window_class: String::new(),
                    registered_title: app_name.to_string(),
                    title_contains: Some(app_name.to_string()),
                    title_regex: None,
                },
                main_placement: crate::domain::placement::SavedPlacement {
                    monitor_id: "A".to_string(),
                    main_monitor_index: 0,
                    normalized_rect: crate::domain::placement::NormalizedRect {
                        x: 0.0,
                        y: 0.0,
                        width: 1.0,
                        height: 1.0,
                    },
                    physical_rect_at_capture: crate::domain::placement::PixelRect::new(0, 0, 0, 0),
                    show_state: crate::domain::placement::SavedShowState::Normal,
                },
                z_order: 0,
                launch_spec: spec,
                minimize_when_parked: false,
            }
        }

        let code = declared(
            "Visual Studio Code",
            Some(build_launch_spec(
                Path::new(r"C:\VS\Code.exe"),
                Some(Path::new(r"C:\code\portfolio\repodeck")),
                None,
            )),
        );
        let workspace = declared(
            "Visual Studio Code",
            Some(build_launch_spec(
                Path::new(r"C:\VS\Code.exe"),
                Some(Path::new(r"C:\ws\02_資料.code-workspace")),
                None,
            )),
        );
        let brave = declared(
            "Brave",
            Some(build_launch_spec(
                Path::new(r"C:\brave\brave.exe"),
                None,
                Some("https://www.youtube.com/"),
            )),
        );
        let mut worksets = vec![crate::application::workset_service::build_workset(
            "S".to_string(),
            "#fff".to_string(),
            PathBuf::from(r"D:\s"),
            RepositoryKind::Git,
            0,
            vec![code.clone(), workspace.clone(), brave.clone()],
        )];

        assert_eq!(retitle_declared_vscode_windows(&mut worksets), 2);
        let windows = &worksets[0].windows;
        assert_eq!(
            windows[0].matcher.title_contains.as_deref(),
            Some("repodeck")
        );
        assert_eq!(
            windows[1].matcher.title_contains.as_deref(),
            Some("02_資料")
        );
        // ブラウザの針は据え置き（外すと起動中の窓を取り込めなくなる）。
        assert_eq!(windows[2].matcher.title_contains.as_deref(), Some("Brave"));

        // 二度目は何も変えない（移行済みの針を上書きしない）。
        assert_eq!(retitle_declared_vscode_windows(&mut worksets), 0);
    }

    #[test]
    fn only_remote_desktop_is_closed_by_killing_its_process() {
        assert!(closes_only_by_kill(Path::new(
            r"C:\Windows\System32\mstsc.exe"
        )));
        assert!(closes_only_by_kill(Path::new(
            r"C:\Windows\System32\MSTSC.EXE"
        )));
        // 既定は WM_CLOSE。編集中の内容を持ちうるアプリを勝手に殺さない。
        assert!(!closes_only_by_kill(Path::new(r"C:\VS\Code.exe")));
        assert!(!closes_only_by_kill(Path::new(r"C:\brave\brave.exe")));
        assert!(!closes_only_by_kill(Path::new("")));
    }

    #[test]
    fn registered_args_are_split_respecting_quotes() {
        assert_eq!(
            split_registered_args(r#"--user-data-dir="D:\Brave Data\開発""#),
            vec![r"--user-data-dir=D:\Brave Data\開発".to_string()]
        );
        assert_eq!(
            split_registered_args("--incognito --new-window"),
            vec!["--incognito".to_string(), "--new-window".to_string()]
        );
        assert!(split_registered_args("").is_empty());
    }

    #[test]
    fn only_user_data_dir_identifies_a_browser_window() {
        // `--user-data-dir` は独立プロセスになるのでコマンドラインで特定できる。
        let args = vec![
            r"--user-data-dir=D:\BraveData\dev".to_string(),
            "--new-window".to_string(),
        ];
        assert_eq!(
            browser_identity_arg(&args).map(String::as_str),
            Some(r"--user-data-dir=D:\BraveData\dev")
        );
        // `--profile-directory` は既存プロセスが窓を開くため判別に使えない。
        let profile_only = vec![
            "--profile-directory=Profile 1".to_string(),
            "--new-window".to_string(),
        ];
        assert_eq!(browser_identity_arg(&profile_only), None);
        assert_eq!(browser_identity_arg(&["--new-window".to_string()]), None);
    }

    #[test]
    fn declared_title_needle_is_the_vscode_folder_and_nothing_else() {
        assert_eq!(
            declared_title_needle(LaunchKind::VsCode, r"C:\code\test\repo01"),
            Some("repo01".to_string())
        );
        // ワークスペースはタイトルに拡張子なしで出る。
        assert_eq!(
            declared_title_needle(LaunchKind::VsCode, r"C:\code\repo08\repo08.code-workspace"),
            Some("repo08".to_string())
        );
        assert_eq!(declared_title_needle(LaunchKind::VsCode, ""), None);
        // ブラウザのタイトルはページのもので URL とは無関係。汎用アプリの引数も同様。
        assert_eq!(
            declared_title_needle(LaunchKind::Browser, "https://example.com"),
            None
        );
        assert_eq!(declared_title_needle(LaunchKind::Generic, "--hud"), None);
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
