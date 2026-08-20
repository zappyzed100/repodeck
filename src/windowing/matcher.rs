//! Re-matches a registered [`WindowMatcher`] against freshly enumerated windows
//! (PLAN.md §5.3-§5.6). Pure comparison logic; no Win32 calls of its own — it
//! only reads [`TopLevelWindow`] values that `enumerate` already collected.

use std::collections::HashSet;
use std::path::Path;

use crate::domain::workset::WindowMatcher;
use crate::windowing::enumerate::TopLevelWindow;

/// 2 つの実行ファイルパスが同じものを指すか。
///
/// Windows のパスは大文字小文字を区別しない。登録値はスタートメニューの
/// ショートカットから来ることが多く `C:\WINDOWS\system32\mstsc.exe`、実行中の
/// ウィンドウから読める値は OS 正規の `C:\Windows\System32\mstsc.exe` になる。
/// 素の `PathBuf` 比較はこれを別物と見なすので、リモートデスクトップのバインドが
/// 検証で落ち、「閉じる」の対象からも外れていた。
pub fn same_executable(a: &Path, b: &Path) -> bool {
    let key = |p: &Path| p.as_os_str().to_string_lossy().to_ascii_lowercase();
    if key(a) == key(b) {
        return true;
    }

    // Packaged (MSIX/Store) applications live below a versioned directory:
    // `WindowsApps\OpenAI.Codex_26.810.7004.0_x64__...\app\ChatGPT.exe`.
    // The directory changes on every update, while the package family and the
    // path inside the package stay stable. Treat those two stable parts as the
    // executable identity so a saved Codex window survives an app update.
    match (store_app_identity(a), store_app_identity(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Returns the version-independent identity of an executable inside a
/// `WindowsApps` package: package family plus the path within that package.
fn store_app_identity(path: &Path) -> Option<(String, String)> {
    let mut components = path.components();
    while let Some(component) = components.next() {
        if !component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("WindowsApps")
        {
            continue;
        }

        let package_dir = components.next()?.as_os_str().to_string_lossy();
        let (name_and_version, publisher_id) = package_dir.rsplit_once("__")?;
        let package_name = name_and_version.split('_').next()?;
        if package_name.is_empty() || publisher_id.is_empty() {
            return None;
        }

        let relative_path = components
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\\")
            .to_ascii_lowercase();
        if relative_path.is_empty() {
            return None;
        }

        return Some((
            format!("{package_name}_{publisher_id}").to_ascii_lowercase(),
            relative_path,
        ));
    }

    None
}

/// Windows のスクリプトホストは、起動したアプリの窓を自分では持たず、
/// Brave/Chrome など別プロセスへ UI を引き渡すことがある。
pub fn is_launcher_executable(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    matches!(
        file_name.to_ascii_lowercase().as_str(),
        "wscript.exe" | "cscript.exe" | "cmd.exe" | "powershell.exe" | "pwsh.exe"
    )
}

const GENERIC_LAUNCHER_TITLE_TOKENS: &[&str] = &[
    "web",
    "ui",
    "app",
    "application",
    "desktop",
    "launcher",
    "windows",
];

/// VS Code 系はスクリプトホストが開く UI ではなく、同じリポジトリ名をタイトルに
/// 出すだけなので、ランチャー引き継ぎの対象にしない。
fn is_editor_executable(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "code.exe" | "code - insiders.exe" | "codium.exe" | "cursor.exe"
            )
        })
}

/// 登録タイトルから汎用語を除いた、照合に使う連続フレーズ。
///
/// 単語ごとにバラすと `deepseek-harness - Cursor` が `DeepSeek Harness Web UI`
/// に一致してしまう。空白区切りのフレーズとして残し、ハイフン連結のフォルダ名は
/// 別物とみなす。
fn meaningful_title_phrase(title: &str) -> Option<String> {
    let words: Vec<String> = title
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-')
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .filter(|word| {
            !GENERIC_LAUNCHER_TITLE_TOKENS
                .iter()
                .any(|generic| word == generic)
        })
        .collect();
    if words.len() < 2 {
        return None;
    }
    Some(words.join(" "))
}

/// ランチャーが作った別プロセスの UI を、登録タイトルを根拠に引き継げるか。
///
/// `registered_title` だけでなく、明示的な title needle/regex も利用する。
/// フォールバックは汎用語を除いた登録タイトルの連続フレーズが候補タイトルに
/// 含まれるときだけ。エディタ窓は対象外。
pub fn launcher_title_matches(matcher: &WindowMatcher, candidate: &TopLevelWindow) -> bool {
    if !is_launcher_executable(&matcher.executable_path) {
        return false;
    }
    if candidate
        .executable_path
        .as_deref()
        .is_some_and(is_editor_executable)
    {
        return false;
    }

    let contains_ok = matcher
        .title_contains
        .as_deref()
        .is_some_and(|needle| !needle.is_empty() && candidate.title.contains(needle));
    let regex_ok = matcher.title_regex.as_deref().is_some_and(|pattern| {
        !pattern.is_empty()
            && regex::Regex::new(pattern).is_ok_and(|re| re.is_match(&candidate.title))
    });
    if contains_ok || regex_ok {
        return true;
    }

    let Some(phrase) = meaningful_title_phrase(&matcher.registered_title) else {
        return false;
    };
    candidate.title.to_lowercase().contains(&phrase)
}

const AUTO_REBIND_THRESHOLD: i32 = 75;
const AUTO_REBIND_MARGIN: i32 = 20;
const TITLE_SIMILARITY_THRESHOLD: f64 = 0.8;
const EDITOR_TITLE_SUFFIXES: &[&str] = &["- Visual Studio Code", "- Cursor"];

/// Normalizes a window title for comparison (PLAN.md §5.5):
/// strips the unsaved-changes marker and the dynamic editor suffix
/// (`- Visual Studio Code` / `- Cursor`), collapses whitespace, and
/// lowercases the result.
pub fn normalize_title(title: &str) -> String {
    let without_marker = title.replace('●', "");
    let trimmed = without_marker.trim();
    let without_suffix = EDITOR_TITLE_SUFFIXES
        .iter()
        .find_map(|suffix| trimmed.strip_suffix(suffix).map(str::trim_end))
        .unwrap_or(trimmed);

    without_suffix
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();

    let mut previous_row: Vec<usize> = (0..=b.len()).collect();
    let mut current_row = vec![0usize; b.len() + 1];

    for (i, &ca) in a.iter().enumerate() {
        current_row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let substitution_cost = usize::from(ca != cb);
            current_row[j + 1] = (previous_row[j + 1] + 1)
                .min(current_row[j] + 1)
                .min(previous_row[j] + substitution_cost);
        }
        std::mem::swap(&mut previous_row, &mut current_row);
    }

    previous_row[b.len()]
}

/// Normalized similarity between two already-[`normalize_title`]d strings, in
/// `0.0..=1.0` (1.0 = identical). PLAN.md §5.5's "登録時タイトルとの正規化類似度".
pub fn title_similarity(a: &str, b: &str) -> f64 {
    let max_len = a.chars().count().max(b.chars().count());
    if max_len == 0 {
        return 1.0;
    }
    1.0 - (levenshtein_distance(a, b) as f64 / max_len as f64)
}

/// Scores how well `candidate` matches `matcher`, per PLAN.md §5.5's table.
/// "Exact registered title" and "80%+ normalized similarity" are mutually
/// exclusive tiers (an exact match does not also collect the similarity bonus).
pub fn score_candidate(matcher: &WindowMatcher, candidate: &TopLevelWindow) -> i32 {
    let mut score = 0;

    if candidate
        .executable_path
        .as_deref()
        .is_some_and(|p| same_executable(p, &matcher.executable_path))
    {
        score += 50;
    }
    if candidate.window_class == matcher.window_class {
        score += 25;
    }

    score + title_score(matcher, candidate)
}

/// [`score_candidate`] のうちタイトル由来の分。アプリ同一性（実行ファイル＋
/// ウィンドウクラス）以外のすべて。
fn title_score(matcher: &WindowMatcher, candidate: &TopLevelWindow) -> i32 {
    let mut score = 0;

    if matcher
        .title_contains
        .as_deref()
        .is_some_and(|needle| !needle.is_empty() && candidate.title.contains(needle))
    {
        score += 30;
    }
    if let Some(pattern) = &matcher.title_regex
        && let Ok(re) = regex::Regex::new(pattern)
        && re.is_match(&candidate.title)
    {
        score += 30;
    }

    if candidate.title == matcher.registered_title {
        score += 20;
    } else if title_similarity(
        &normalize_title(&matcher.registered_title),
        &normalize_title(&candidate.title),
    ) >= TITLE_SIMILARITY_THRESHOLD
    {
        score += 10;
    }

    score
}

/// この登録が「同じアプリの別窓」と自分の窓を見分けるための手掛かりを持っているか。
///
/// アプリ名そのものを入れた `title_contains`（登録アプリから宣言したセットは当初
/// これを保存していた）は手掛かりではない。移行はせず、ここで無視する。
pub fn has_discriminator(matcher: &WindowMatcher) -> bool {
    matcher
        .title_regex
        .as_deref()
        .is_some_and(|pattern| !pattern.is_empty())
        || matcher
            .title_contains
            .as_deref()
            .is_some_and(|needle| !needle.is_empty() && needle != matcher.registered_title)
}

/// [`has_discriminator`] の手掛かりが `candidate` に対して成り立つか。
fn discriminator_matches(matcher: &WindowMatcher, candidate: &TopLevelWindow) -> bool {
    let contains_ok = matcher.title_contains.as_deref().is_some_and(|needle| {
        !needle.is_empty() && needle != matcher.registered_title && candidate.title.contains(needle)
    });
    let regex_ok = matcher.title_regex.as_deref().is_some_and(|pattern| {
        !pattern.is_empty()
            && regex::Regex::new(pattern).is_ok_and(|re| re.is_match(&candidate.title))
    });
    contains_ok || regex_ok
}

/// `candidate` が「そのアプリの他のウィンドウ」ではなく *この* ウィンドウだと
/// 言える根拠を持つか。
///
/// 実行ファイル＋クラスは「そのアプリの窓」としか言っていない（Brave のどの
/// ウィンドウもどの Brave 登録にも一致する）。同一アプリの二窓を取り違えられない
/// 呼び出し側が、本物の再発見とそっくりさんを見分けるために使う。
///
/// 手掛かり（[`has_discriminator`]）を持つ登録は、**その手掛かりだけで**判断する。
/// 登録時タイトルとの完全一致・類似は根拠に数えない。VS Code の登録はどれも
/// `registered_title` が汎用の `"Visual Studio Code"` で、これは**起動直後の
/// フォルダ未読込のウィンドウが一時的に名乗るタイトル**でもある。数えてしまうと、
/// OS 再起動でセッション復元中の VS Code の窓が、どのセットの登録に対しても
/// 「根拠あり」になり、最初に評価されたセットが無関係な窓を掴む（2026-07-29）。
pub fn has_title_evidence(matcher: &WindowMatcher, candidate: &TopLevelWindow) -> bool {
    if has_discriminator(matcher) {
        return discriminator_matches(matcher, candidate);
    }
    // 手掛かりが無い登録は、登録時タイトルとの一致・類似だけが頼り。アプリ名
    // そのものを入れた `title_contains` は根拠にしない（上記のとおり）。
    let neutralized = WindowMatcher {
        title_contains: None,
        ..matcher.clone()
    };
    title_score(&neutralized, candidate) > 0
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchDecision {
    /// A single candidate is confidently the right window.
    AutoRebind { hwnd: isize },
    /// One or more candidates scored high enough to be plausible, but not
    /// confidently enough to pick automatically; the user must confirm.
    Ambiguous { candidates: Vec<isize> },
    /// Nothing scored high enough to be worth showing as a match.
    Unresolved,
}

/// タイトルの根拠を1点も持たない候補を、それでも自動バインドしてよいか。
///
/// 実行ファイル(50)＋ウィンドウクラス(25)＝**75** は [`AUTO_REBIND_THRESHOLD`] に
/// ちょうど届く。つまりタイトル由来の点がゼロでも自動バインドが成立してしまう。
/// この 75 点が言っているのは「そのアプリの窓だ」だけで「*この*窓だ」ではないので、
/// 同じ実行ファイルを名乗る登録が他にもあると、どの登録も同じ窓を自分のものだと
/// 主張する（実機で Brave の登録14個が1枚の窓を奪い合った、2026-07-29）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitlelessMatch {
    /// 許す。その実行ファイルを名乗る登録が他に無いので、取り違える相手が居ない。
    Accept,
    /// 拒む。同点になる登録が他にもある。根拠なしに掴ませてはいけない。
    Reject,
}

struct Scored {
    hwnd: isize,
    score: i32,
    has_exe_path: bool,
    has_title_evidence: bool,
    /// 先のセットが既に確保済み。選べないが、**居ないわけではない**。
    claimed: bool,
}

/// Re-matches `matcher` against `candidates` (PLAN.md §5.5-§5.6).
///
/// `bound_elsewhere` lists HWNDs already bound to a *different* registration
/// (from an earlier candidate in the same re-bind pass); they can't be selected,
/// per §5.5's "候補が別ワークセットへ既にバインド済み: 除外".
///
/// `titleless` は、タイトルの根拠を持つ候補が1つも無かったときの逃げ道を開けるか
/// どうか。[`TitlelessMatch::Reject`] のときは [`MatchDecision::Unresolved`]
/// ——「このセットの窓は開いていない」——を返す。`Ambiguous` ではなく `Unresolved`
/// なのは意図的で、呼び出し側はこれを見て**そのセット自身のアプリを起動し直す**。
/// 他セットの窓で妥協するより、正しい窓を開き直すほうが常に正しい。
pub fn resolve_best_match(
    matcher: &WindowMatcher,
    candidates: &[TopLevelWindow],
    bound_elsewhere: &HashSet<isize>,
    titleless: TitlelessMatch,
) -> MatchDecision {
    // 得点は**全候補**ぶん出す。`bound_elsewhere` を先に振り落とすと、貪欲な
    // 先勝ちで候補が1枚に減ったときに2位が消え、下の `margin` が `best_score`
    // まで跳ね上がって、いちばん自信を持ってはいけない場面で自信を持つ。
    let mut scored: Vec<Scored> = candidates
        .iter()
        .map(|candidate| Scored {
            hwnd: candidate.hwnd,
            score: score_candidate(matcher, candidate),
            has_exe_path: candidate.executable_path.is_some(),
            has_title_evidence: has_title_evidence(matcher, candidate),
            claimed: bound_elsewhere.contains(&candidate.hwnd),
        })
        .collect();

    scored.sort_by_key(|s| std::cmp::Reverse(s.score));

    let Some(best) = scored.iter().find(|s| !s.claimed) else {
        return MatchDecision::Unresolved;
    };

    if best.score < AUTO_REBIND_THRESHOLD {
        return MatchDecision::Unresolved;
    }

    // 選ぼうとしている窓が「そのアプリの窓」としか言えない＝このセットの窓かどうか
    // 分からない。他セットの窓を掴むより、開いていない扱いにして開き直させる。
    if titleless == TitlelessMatch::Reject && !best.has_title_evidence {
        return MatchDecision::Unresolved;
    }

    // 2位は**確保済みも数える**。より良い候補が他セットに取られているなら、
    // 残り物を掴むのは「一番手が居なかった」ときの自動バインドとは違う。
    let margin = scored
        .iter()
        .find(|s| s.hwnd != best.hwnd)
        .map_or(best.score, |s| best.score - s.score);

    // §5.6: a candidate whose executable path couldn't be read is never
    // auto-rebound, even at high confidence — it falls back to asking the user.
    if margin >= AUTO_REBIND_MARGIN && best.has_exe_path {
        MatchDecision::AutoRebind { hwnd: best.hwnd }
    } else {
        let candidates = scored
            .iter()
            .filter(|s| !s.claimed && s.score >= AUTO_REBIND_THRESHOLD)
            .map(|s| s.hwnd)
            .collect();
        MatchDecision::Ambiguous { candidates }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn matcher() -> WindowMatcher {
        WindowMatcher {
            executable_path: PathBuf::from(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            process_name: "Code.exe".to_string(),
            window_class: "Chrome_WidgetWin_1".to_string(),
            registered_title: "main.rs - repodeck - Visual Studio Code".to_string(),
            title_contains: None,
            title_regex: None,
        }
    }

    fn candidate(hwnd: isize, exe: Option<&str>, class: &str, title: &str) -> TopLevelWindow {
        TopLevelWindow {
            hwnd,
            process_id: 1000,
            executable_path: exe.map(PathBuf::from),
            window_class: class.to_string(),
            title: title.to_string(),
            rect_px: crate::domain::placement::PixelRect::new(0, 0, 800, 600),
        }
    }

    fn launcher_matcher() -> WindowMatcher {
        WindowMatcher {
            executable_path: PathBuf::from(r"C:\Windows\System32\wscript.exe"),
            process_name: "wscript.exe".to_string(),
            window_class: String::new(),
            registered_title: "DeepSeek Harness Web UI".to_string(),
            title_contains: None,
            title_regex: None,
        }
    }

    #[test]
    fn normalize_title_strips_marker_suffix_and_case() {
        assert_eq!(
            normalize_title("● main.rs - repodeck - Visual Studio Code"),
            "main.rs - repodeck"
        );
        assert_eq!(
            normalize_title("● main.rs - repodeck - Cursor"),
            "main.rs - repodeck"
        );
        assert_eq!(normalize_title("  Foo   Bar  "), "foo bar");
    }

    #[test]
    fn launcher_can_handoff_to_a_browser_window_with_the_app_title() {
        let m = launcher_matcher();
        let c = candidate(
            1,
            Some(r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe"),
            "Chrome_WidgetWin_1",
            "テスト — DeepSeek Harness - Brave",
        );

        assert!(launcher_title_matches(&m, &c));
    }

    #[test]
    fn launcher_handoff_rejects_unrelated_titles_and_normal_apps() {
        let m = launcher_matcher();
        let unrelated = candidate(
            1,
            Some(r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe"),
            "Chrome_WidgetWin_1",
            "GitHub - Brave",
        );
        assert!(!launcher_title_matches(&m, &unrelated));

        let mut normal = m;
        normal.executable_path = PathBuf::from(r"C:\Tools\deepseek-harness.exe");
        assert!(!launcher_title_matches(&normal, &unrelated));
    }

    #[test]
    fn launcher_handoff_does_not_steal_an_editor_window_for_the_same_repo() {
        let m = launcher_matcher();
        let cursor = candidate(
            1,
            Some(r"C:\Users\me\AppData\Local\Programs\cursor\Cursor.exe"),
            "Chrome_WidgetWin_1",
            "deepseek-harness - Cursor",
        );
        assert!(!launcher_title_matches(&m, &cursor));

        let hyphenated_browser = candidate(
            2,
            Some(r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe"),
            "Chrome_WidgetWin_1",
            "deepseek-harness - Brave",
        );
        assert!(
            !launcher_title_matches(&m, &hyphenated_browser),
            "hyphenated folder names are not the spaced app title"
        );
    }

    #[test]
    fn exact_match_on_every_field_auto_rebinds() {
        let m = matcher();
        let c = candidate(
            1,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "main.rs - repodeck - Visual Studio Code",
        );

        assert_eq!(
            resolve_best_match(&m, &[c], &HashSet::new(), TitlelessMatch::Accept),
            MatchDecision::AutoRebind { hwnd: 1 }
        );
    }

    #[test]
    fn title_only_change_still_auto_rebinds_via_similarity() {
        let m = matcher();
        // exe path (+50) + class (+25) + similarity (+10) = 85, no other candidate.
        let c = candidate(
            1,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "lib.rs - repodeck - Visual Studio Code",
        );

        assert_eq!(
            resolve_best_match(&m, &[c], &HashSet::new(), TitlelessMatch::Accept),
            MatchDecision::AutoRebind { hwnd: 1 }
        );
    }

    #[test]
    fn two_similar_candidates_are_ambiguous() {
        let m = matcher();
        let c1 = candidate(
            1,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "main.rs - repodeck - Visual Studio Code",
        );
        let c2 = candidate(
            2,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "main.rs - repodeck - Visual Studio Code",
        );

        let decision = resolve_best_match(&m, &[c1, c2], &HashSet::new(), TitlelessMatch::Accept);
        assert_eq!(
            decision,
            MatchDecision::Ambiguous {
                candidates: vec![1, 2]
            }
        );
    }

    #[test]
    fn executable_paths_are_compared_case_insensitively() {
        // 登録値はショートカット由来（C:\WINDOWS\system32\…）、実行中の窓から
        // 読める値は OS 正規（C:\Windows\System32\…）。同じ実行ファイルである。
        assert!(same_executable(
            Path::new(r"C:\WINDOWS\system32\mstsc.exe"),
            Path::new(r"C:\Windows\System32\mstsc.exe")
        ));
        assert!(!same_executable(
            Path::new(r"C:\Windows\System32\mstsc.exe"),
            Path::new(r"C:\Windows\System32\notepad.exe")
        ));

        let mut m = matcher();
        m.executable_path = PathBuf::from(r"C:\WINDOWS\system32\mstsc.exe");
        m.window_class = "TscShellContainerClass".to_string();
        m.registered_title = "Remote Desktop Connection".to_string();
        let c = candidate(
            1,
            Some(r"C:\Windows\System32\mstsc.exe"),
            "TscShellContainerClass",
            "perkypat100 - リモート デスクトップ接続",
        );
        // 実行ファイル 50 + クラス 25 で自動再バインドの閾値に届く。
        assert_eq!(score_candidate(&m, &c), 75);
    }

    #[test]
    fn store_app_paths_match_across_package_updates() {
        let registered = Path::new(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.810.7004.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
        );
        let running = Path::new(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.818.2441.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
        );
        assert!(same_executable(registered, running));
        assert!(!same_executable(
            registered,
            Path::new(
                r"C:\Program Files\WindowsApps\OpenAI.Codex_26.818.2441.0_x64__different\app\ChatGPT.exe",
            )
        ));
        assert!(!same_executable(
            registered,
            Path::new(
                r"C:\Program Files\WindowsApps\OpenAI.Codex_26.818.2441.0_x64__2p2nqsd0c76g0\other\ChatGPT.exe",
            )
        ));
    }

    #[test]
    fn codex_window_rebinds_when_the_store_package_version_changed() {
        let m = WindowMatcher {
            executable_path: PathBuf::from(
                r"C:\Program Files\WindowsApps\OpenAI.Codex_26.810.7004.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe",
            ),
            process_name: "ChatGPT.exe".to_string(),
            window_class: "Chrome_WidgetWin_1".to_string(),
            registered_title: "ChatGPT".to_string(),
            title_contains: None,
            title_regex: None,
        };
        let running_path = r"C:\Program Files\WindowsApps\OpenAI.Codex_26.818.2441.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe";
        let window = candidate(1, Some(running_path), "Chrome_WidgetWin_1", "ChatGPT");

        assert_eq!(score_candidate(&m, &window), 95);
        assert_eq!(
            resolve_best_match(&m, &[window], &HashSet::new(), TitlelessMatch::Accept),
            MatchDecision::AutoRebind { hwnd: 1 }
        );
    }

    #[test]
    fn low_score_candidate_is_unresolved() {
        let m = matcher();
        let c = candidate(
            1,
            Some(r"C:\other\app.exe"),
            "SomeOtherClass",
            "Completely different title",
        );

        assert_eq!(
            resolve_best_match(&m, &[c], &HashSet::new(), TitlelessMatch::Accept),
            MatchDecision::Unresolved
        );
    }

    #[test]
    fn candidate_already_bound_elsewhere_is_excluded() {
        let m = matcher();
        let c = candidate(
            1,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "main.rs - repodeck - Visual Studio Code",
        );

        let mut bound = HashSet::new();
        bound.insert(1isize);

        assert_eq!(
            resolve_best_match(&m, &[c], &bound, TitlelessMatch::Accept),
            MatchDecision::Unresolved
        );
    }

    #[test]
    fn missing_executable_path_never_auto_rebinds_even_at_high_confidence() {
        let m = matcher();
        // class (+25) + exact title (+20) = 45... not enough alone; add title_contains too.
        let mut m = m;
        m.title_contains = Some("main.rs".to_string());
        let c = candidate(
            1,
            None,
            "Chrome_WidgetWin_1",
            "main.rs - repodeck - Visual Studio Code",
        );
        // class(25) + contains(30) + exact title(20) = 75, well above threshold, single candidate.

        assert_eq!(
            resolve_best_match(&m, &[c], &HashSet::new(), TitlelessMatch::Accept),
            MatchDecision::Ambiguous {
                candidates: vec![1]
            }
        );
    }

    #[test]
    fn title_contains_and_regex_both_contribute() {
        let mut m = matcher();
        m.title_contains = Some("repodeck".to_string());
        m.title_regex = Some(r"^main\.rs".to_string());
        let c = candidate(1, None, "SomeClass", "main.rs - repodeck");
        // contains(30) + regex(30) + similarity(10, since normalized titles differ by the VS Code suffix only, still >=0.8) = 70 -> unresolved.
        let score = score_candidate(&m, &c);
        assert_eq!(score, 70);
    }

    #[test]
    fn the_apps_own_name_is_not_evidence_that_this_is_the_right_window() {
        // 登録アプリから宣言したエントリ：登録タイトルも針もアプリ名そのもの。
        let m = WindowMatcher {
            executable_path: PathBuf::from(r"C:\brave\brave.exe"),
            process_name: "brave.exe".to_string(),
            window_class: "Chrome_WidgetWin_1".to_string(),
            registered_title: "Brave".to_string(),
            title_contains: Some("Brave".to_string()),
            title_regex: None,
        };
        let other_window = candidate(
            1,
            Some(r"C:\brave\brave.exe"),
            "Chrome_WidgetWin_1",
            "GitHub - Brave",
        );
        assert!(!has_title_evidence(&m, &other_window));
    }

    #[test]
    fn a_discriminating_needle_is_evidence() {
        let mut m = matcher();
        m.title_contains = Some("repo01".to_string());
        let mine = candidate(
            1,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "repo01 - Visual Studio Code",
        );
        let theirs = candidate(
            2,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "repo07 - Visual Studio Code",
        );
        assert!(has_title_evidence(&m, &mine));
        assert!(!has_title_evidence(&m, &theirs));
    }

    #[test]
    fn a_captured_windows_own_title_is_evidence() {
        // キャプチャ由来の登録は本物のウィンドウタイトルを持つので、
        // タイトルが多少変わっても類似度で自分だと分かる。
        let m = matcher();
        let same = candidate(
            1,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "lib.rs - repodeck - Visual Studio Code",
        );
        assert!(has_title_evidence(&m, &same));
    }

    /// 実機（2026-07-29）: Brave の登録14個はどれも `registered_title="Brave"`、
    /// 針も正規表現も無し。実行ファイル50＋クラス25＝ちょうど75で閾値に届くので、
    /// 生き残った1枚の Brave をどの登録も自分のものだと主張していた。
    #[test]
    fn an_app_only_match_is_refused_when_other_registrations_look_identical() {
        let m = WindowMatcher {
            executable_path: PathBuf::from(r"C:\brave\brave.exe"),
            process_name: "brave.exe".to_string(),
            window_class: "Chrome_WidgetWin_1".to_string(),
            registered_title: "Brave".to_string(),
            title_contains: None,
            title_regex: None,
        };
        let someone_elses = candidate(
            1,
            Some(r"C:\brave\brave.exe"),
            "Chrome_WidgetWin_1",
            "再生 | U-NEXT - Brave",
        );
        assert_eq!(score_candidate(&m, &someone_elses), 75);

        assert_eq!(
            resolve_best_match(
                &m,
                std::slice::from_ref(&someone_elses),
                &HashSet::new(),
                TitlelessMatch::Reject
            ),
            MatchDecision::Unresolved,
            "根拠なしに他セットの窓を掴んではいけない"
        );
        assert_eq!(
            resolve_best_match(
                &m,
                &[someone_elses],
                &HashSet::new(),
                TitlelessMatch::Accept
            ),
            MatchDecision::AutoRebind { hwnd: 1 },
            "その実行ファイルの登録が1つだけなら取り違える相手が居ない"
        );
    }

    /// 貪欲な先勝ちで候補が1枚に減ると、2位が消えて margin が best_score まで
    /// 跳ね上がり、いちばん自信を持ってはいけない場面で自信を持っていた。
    #[test]
    fn the_leftover_candidate_is_not_confidently_bound_when_a_better_one_is_taken() {
        let mut m = matcher();
        m.title_contains = Some("repodeck".to_string());
        let mine = candidate(
            1,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "repodeck",
        );
        let someone_elses = candidate(
            2,
            Some(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "02_求解・高速化 (ワークスペース)",
        );

        // 自分の窓が別セットに確保済み。残るのは無関係な窓1枚だけ。
        let claimed: HashSet<isize> = [1].into_iter().collect();
        assert_eq!(
            resolve_best_match(&m, &[mine, someone_elses], &claimed, TitlelessMatch::Accept),
            MatchDecision::Ambiguous {
                candidates: vec![2]
            },
            "取られた一番手より低い残り物を、自信満々に掴んではいけない"
        );
    }

    /// VS Code の登録はどれも `registered_title` が汎用の "Visual Studio Code"。
    /// これは**フォルダ読込前のウィンドウが一時的に名乗るタイトル**でもあるので、
    /// OS 再起動でセッション復元中の窓がどの登録にも「根拠あり」になっていた。
    #[test]
    fn the_generic_app_title_is_not_evidence_for_a_registration_that_has_a_needle() {
        let m = WindowMatcher {
            executable_path: PathBuf::from(r"C:\Code\Code.exe"),
            process_name: "Code.exe".to_string(),
            window_class: "Chrome_WidgetWin_1".to_string(),
            registered_title: "Visual Studio Code".to_string(),
            title_contains: Some("repodeck".to_string()),
            title_regex: None,
        };
        let still_loading = candidate(
            1,
            Some(r"C:\Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "Visual Studio Code",
        );
        let mine = candidate(
            2,
            Some(r"C:\Code\Code.exe"),
            "Chrome_WidgetWin_1",
            "repodeck",
        );
        assert!(!has_title_evidence(&m, &still_loading));
        assert!(has_title_evidence(&m, &mine));
    }

    #[test]
    fn invalid_regex_is_ignored_rather_than_panicking() {
        let mut m = matcher();
        m.title_regex = Some("(unclosed".to_string());
        let c = candidate(1, None, "x", "y");
        // Should not panic; invalid regex simply contributes no score.
        assert_eq!(score_candidate(&m, &c), 0);
    }
}
