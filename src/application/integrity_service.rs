//! セットとウィンドウの紐づけが壊れていないかを点検し、直せるものはその場で直す。
//!
//! 紐づけは「一度ついたら正しい」と扱われる。生きている HWND を指していて実行
//! ファイルさえ合っていれば、内容の照合は丸ごと省かれる——それが紐づけの存在理由
//! （タイトルも URL も変わる窓を追い続けるため）だからだ。ところがその前提は、
//! **紐づけが最初から間違っていた**場合には牙を剥く。誤りは検証されないので、
//! 正しい窓が後から現れても乗り換えず、永久に固定される。
//!
//! 実機で起きたこと（2026-07-29、OS 再起動直後）:
//!
//! - 再起動で HWND の紐づけは全消しされる（正しい挙動）
//! - 復元中はまだ数枚しか窓が立っていない
//! - 貪欲な先勝ちで候補が1枚に減ると、実行ファイル＋クラスの75点だけで
//!   「自信を持って」誤バインドが確定する
//! - それが `runtime.json` に焼き付き、`repodeck` セットの VS Code 登録が
//!   「02_求解・高速化」の窓を、`エンジン` セットの登録が「repodeck」の窓を
//!   握ったまま戻らなくなった。Brave に至っては1枚の窓を4セットが同時に握った
//!
//! 照合側（`windowing::matcher`）はこの誤りが**新しく生まれない**ように直した。
//! こちらは、**既に生まれてしまった**誤りを見つけて捨てる担当。捨てれば次の
//! 解決で正しい窓に付き直すか、見つからなければセット自身のアプリを起動し直す。
//!
//! 判定は純粋関数なので実機なしでテストできる。

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::domain::workset::{ManagedWindow, Workset};
use crate::windowing::enumerate::TopLevelWindow;
use crate::windowing::matcher;

/// 点検で見つかった異常。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anomaly {
    /// もう存在しない登録が残していた紐づけ。**直せる**——外す。
    ///
    /// セットやウィンドウを消しても項目は残り続けるので、放っておくと溜まる
    /// （実機で190件、現役の窓は22個だった）。HWND は OS が再利用するため、
    /// 残骸が現役の窓を握っているように見えるのが害。
    StaleBinding { entry: Uuid },
    /// 紐づけ先の窓は生きているが、その登録自身の手掛かり（`title_contains` /
    /// `title_regex`）を満たしていない。**直せる**——捨てて付け直させる。
    ContradictedBinding {
        entry: Uuid,
        workset: String,
        hwnd: isize,
        /// 登録が期待している手掛かり。ログ用。
        expected: String,
        /// 実際に握っている窓のタイトル。
        actual: String,
    },
    /// 1枚の窓を複数の登録が握っているが、そのうちのいくつかは「この窓だ」と
    /// 言える根拠を持たない。**直せる**——根拠の無い側を捨てる。
    ///
    /// 根拠を持つ登録どうしが同じ窓を共有するのは異常ではない（同じ窓を複数の
    /// セットに入れる使い方は意図的に許されている）。
    UnjustifiedSharing {
        hwnd: isize,
        /// 根拠を持たないまま握っていた登録。これらを捨てる。
        entries: Vec<Uuid>,
        worksets: Vec<String>,
    },
    /// 同じ実行ファイルを名乗る登録が複数あるのに、どれも自分の窓を見分ける
    /// 手掛かりを持っていない。**直せない**——設定の問題なので報告だけ。
    ///
    /// この状態の登録は、実行ファイル＋クラスの75点しか出せない。照合側は
    /// 「そのアプリの窓」を掴むことを拒むので、セットは切り替えのたびに自分の
    /// アプリを起動し直すことになる。`title_contains` を足せば解消する。
    IndistinguishableRegistrations {
        executable: String,
        entries: Vec<Uuid>,
        worksets: Vec<String>,
    },
}

impl Anomaly {
    /// 修正器（[`repair`]）がこの異常を直せるか。直せないものは設定の問題で、
    /// 報告するしかない。
    pub fn is_repairable(&self) -> bool {
        !matches!(self, Anomaly::IndistinguishableRegistrations { .. })
    }

    /// この異常を解消するために外す紐づけ。
    ///
    /// 修正器が知っていることはこれだけ——**外す**。付け直しはしない。付け直しは
    /// 照合器の仕事で、こちらがそこへ手を出すと「間違った紐づけを別の間違った
    /// 紐づけに置き換える」余地が生まれる。
    fn bindings_to_drop(&self) -> &[Uuid] {
        match self {
            Anomaly::StaleBinding { entry } | Anomaly::ContradictedBinding { entry, .. } => {
                std::slice::from_ref(entry)
            }
            Anomaly::UnjustifiedSharing { entries, .. } => entries,
            Anomaly::IndistinguishableRegistrations { .. } => &[],
        }
    }
}

/// 修正器が実際に行ったこと。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RepairOutcome {
    /// 正当化できないとして外した紐づけの件数。
    pub dropped_bindings: usize,
    /// もう存在しない登録の残骸として外した件数。
    pub pruned_stale: usize,
}

impl RepairOutcome {
    pub fn touched_anything(&self) -> bool {
        self.dropped_bindings > 0 || self.pruned_stale > 0
    }
}

/// 検出器と修正器を1回ずつ回した結果。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IntegrityReport {
    /// 検出器が見つけたもの。修正器が動いたかどうかに関わらず全部載る。
    pub anomalies: Vec<Anomaly>,
    pub repair: RepairOutcome,
}

impl IntegrityReport {
    pub fn is_clean(&self) -> bool {
        self.anomalies.is_empty() && !self.repair.touched_anything()
    }

    /// 検出器が反応したか。
    pub fn detected_anything(&self) -> bool {
        !self.anomalies.is_empty()
    }

    pub fn dropped_bindings(&self) -> usize {
        self.repair.dropped_bindings
    }

    pub fn pruned_stale(&self) -> usize {
        self.repair.pruned_stale
    }

    /// 直せない異常（設定を直さないと解消しないもの）。
    pub fn unrepairable(&self) -> impl Iterator<Item = &Anomaly> {
        self.anomalies.iter().filter(|a| !a.is_repairable())
    }
}

struct Registration<'a> {
    workset: &'a str,
    window: &'a ManagedWindow,
}

fn index_registrations(worksets: &[Workset]) -> HashMap<Uuid, Registration<'_>> {
    worksets
        .iter()
        .flat_map(|ws| {
            ws.windows.iter().map(move |w| {
                (
                    w.id,
                    Registration {
                        workset: ws.name.as_str(),
                        window: w,
                    },
                )
            })
        })
        .collect()
}

fn executable_key(window: &ManagedWindow) -> String {
    window
        .matcher
        .executable_path
        .as_os_str()
        .to_string_lossy()
        .to_ascii_lowercase()
}

/// **検出器。** 紐づけ表と現在のウィンドウを読むだけで、何も書き換えない。
///
/// Win32 は一切呼ばない（`live_windows` は呼び出し側が既に列挙したもの）。
/// 切り替えのたびに回しても実質タダなので、遠慮なく回してよい。
pub fn detect(
    worksets: &[Workset],
    live_windows: &[TopLevelWindow],
    bindings: &HashMap<Uuid, isize>,
) -> Vec<Anomaly> {
    let registrations = index_registrations(worksets);
    let live_by_hwnd: HashMap<isize, &TopLevelWindow> =
        live_windows.iter().map(|w| (w.hwnd, w)).collect();
    let mut found = Vec::new();

    // 走査順を安定させる。`HashMap` の順は不定で、そのままだとログの並びも
    // 「どの登録が先に doomed 入りするか」も実行ごとに変わる。
    let mut rows: Vec<(Uuid, isize)> = bindings.iter().map(|(&id, &hwnd)| (id, hwnd)).collect();
    rows.sort();

    // 1. もう存在しない登録の残骸。
    for &(entry, _) in &rows {
        if !registrations.contains_key(&entry) {
            found.push(Anomaly::StaleBinding { entry });
        }
    }

    // 2. 登録自身の手掛かりに反する紐づけ。
    //
    // **窓がもう存在しない紐づけは触らない。** あれは残骸ではなく「このエントリは
    // 閉じられた」という情報で、落とすと再発見の条件が緩んでしまう
    // （`workset_service::prune_window_bindings` の説明を参照）。
    let mut doomed: HashSet<Uuid> = HashSet::new();
    for &(entry, hwnd) in &rows {
        let (Some(reg), Some(live)) = (registrations.get(&entry), live_by_hwnd.get(&hwnd)) else {
            continue;
        };
        if !matcher::has_discriminator(&reg.window.matcher)
            || matcher::has_title_evidence(&reg.window.matcher, live)
        {
            continue;
        }
        found.push(Anomaly::ContradictedBinding {
            entry,
            workset: reg.workset.to_string(),
            hwnd,
            expected: reg
                .window
                .matcher
                .title_contains
                .clone()
                .or_else(|| reg.window.matcher.title_regex.clone())
                .unwrap_or_default(),
            actual: live.title.clone(),
        });
        doomed.insert(entry);
    }

    // 3. 根拠なしに1枚の窓を分け合っている紐づけ。
    let mut holders: Vec<(isize, Vec<Uuid>)> = Vec::new();
    for &(entry, hwnd) in &rows {
        if doomed.contains(&entry) || !registrations.contains_key(&entry) {
            continue;
        }
        match holders.iter_mut().find(|(h, _)| *h == hwnd) {
            Some((_, list)) => list.push(entry),
            None => holders.push((hwnd, vec![entry])),
        }
    }
    for (hwnd, entries) in holders {
        if entries.len() < 2 {
            continue;
        }
        let Some(live) = live_by_hwnd.get(&hwnd) else {
            continue;
        };
        let unjustified: Vec<Uuid> = entries
            .into_iter()
            .filter(|entry| {
                registrations
                    .get(entry)
                    .is_some_and(|reg| !matcher::has_title_evidence(&reg.window.matcher, live))
            })
            .collect();
        if unjustified.is_empty() {
            continue;
        }
        found.push(Anomaly::UnjustifiedSharing {
            hwnd,
            worksets: unjustified
                .iter()
                .filter_map(|e| registrations.get(e).map(|r| r.workset.to_string()))
                .collect(),
            entries: unjustified,
        });
    }

    // 4. 設定そのものの問題（直せない）。
    found.extend(indistinguishable(worksets));
    found
}

/// **修正器。** 検出器が挙げた異常のうち、直せるものを直す。
///
/// やることは「正当化できない紐づけを外す」だけで、付け直しはしない。付け直しは
/// 照合器（`workset_service::resolve_all_matches_with_bindings`）の仕事で、外し
/// さえすれば、根拠のある窓を見つければそちらに付き直し、見つからなければ
/// 「開いていない」と判断してセット自身のアプリを起動し直す。どちらも、誤った
/// 紐づけを抱えたままよりは必ず正しい。
pub fn repair(anomalies: &[Anomaly], bindings: &mut HashMap<Uuid, isize>) -> RepairOutcome {
    let mut outcome = RepairOutcome::default();
    for anomaly in anomalies {
        let stale = matches!(anomaly, Anomaly::StaleBinding { .. });
        for entry in anomaly.bindings_to_drop() {
            if bindings.remove(entry).is_none() {
                continue;
            }
            if stale {
                outcome.pruned_stale += 1;
            } else {
                outcome.dropped_bindings += 1;
            }
        }
    }
    outcome
}

/// 検出器を走らせ、**反応したら修正器を呼ぶ**。
///
/// 起動時・各切り替えの直前・トレイの「紐づけを点検して修復」から呼ばれる、
/// 唯一の入口。異常が無ければ `bindings` には指一本触れない。
pub fn check_and_repair(
    worksets: &[Workset],
    live_windows: &[TopLevelWindow],
    bindings: &mut HashMap<Uuid, isize>,
) -> IntegrityReport {
    let anomalies = detect(worksets, live_windows, bindings);
    let repair = if anomalies.iter().any(Anomaly::is_repairable) {
        repair(&anomalies, bindings)
    } else {
        RepairOutcome::default()
    };
    IntegrityReport { anomalies, repair }
}

/// 同じ実行ファイルを名乗るのに、どれも手掛かりを持たない登録の組。
///
/// [`check_and_repair`] からも、設定を保存した直後の点検からも使える。
pub fn indistinguishable(worksets: &[Workset]) -> Vec<Anomaly> {
    let mut by_exe: HashMap<String, Vec<(Uuid, String)>> = HashMap::new();
    for ws in worksets {
        for window in &ws.windows {
            if matcher::has_discriminator(&window.matcher) {
                continue;
            }
            by_exe
                .entry(executable_key(window))
                .or_default()
                .push((window.id, ws.name.clone()));
        }
    }

    let mut found: Vec<Anomaly> = by_exe
        .into_iter()
        .filter(|(_, group)| group.len() > 1)
        .map(
            |(executable, group)| Anomaly::IndistinguishableRegistrations {
                executable,
                entries: group.iter().map(|(id, _)| *id).collect(),
                worksets: group.into_iter().map(|(_, name)| name).collect(),
            },
        )
        .collect();
    // `HashMap` の走査順は不定。ログと表示を安定させる。
    found.sort_by(|a, b| match (a, b) {
        (
            Anomaly::IndistinguishableRegistrations { executable: a, .. },
            Anomaly::IndistinguishableRegistrations { executable: b, .. },
        ) => a.cmp(b),
        _ => std::cmp::Ordering::Equal,
    });
    found
}

/// 点検結果をログへ書き出す。異常が無ければ1行だけ。
pub fn log_report(report: &IntegrityReport, reason: &str) {
    if report.is_clean() {
        tracing::info!(target: "integrity", reason, "紐づけの点検: 異常なし");
        return;
    }

    for anomaly in &report.anomalies {
        match anomaly {
            Anomaly::StaleBinding { entry } => tracing::info!(
                target: "integrity", reason, %entry,
                "もう存在しない登録が残していた紐づけを外した"
            ),
            Anomaly::ContradictedBinding {
                workset,
                hwnd,
                expected,
                actual,
                ..
            } => tracing::warn!(
                target: "integrity", reason, %workset, hwnd, %expected, %actual,
                "紐づけ先の窓が登録の手掛かりに反している。捨てて付け直させる"
            ),
            Anomaly::UnjustifiedSharing {
                hwnd,
                entries,
                worksets,
            } => tracing::warn!(
                target: "integrity", reason, hwnd, count = entries.len(), ?worksets,
                "根拠なしに1枚の窓を分け合っていた。根拠の無い側を捨てる"
            ),
            Anomaly::IndistinguishableRegistrations {
                executable,
                entries,
                worksets,
            } => tracing::warn!(
                target: "integrity", reason, %executable, count = entries.len(), ?worksets,
                "同じアプリの登録が互いに区別できない。title_contains を足さないと、毎回起動し直すことになる"
            ),
        }
    }

    tracing::info!(
        target: "integrity", reason,
        dropped = report.dropped_bindings(), pruned = report.pruned_stale(),
        anomalies = report.anomalies.len(),
        "紐づけの点検が完了した"
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::domain::placement::{NormalizedRect, PixelRect, SavedPlacement, SavedShowState};
    use crate::domain::workset::{ParkingPolicy, RepositoryKind, WindowMatcher};

    fn managed(id: Uuid, exe: &str, needle: Option<&str>) -> ManagedWindow {
        ManagedWindow {
            id,
            matcher: WindowMatcher {
                executable_path: PathBuf::from(exe),
                process_name: "x.exe".to_string(),
                window_class: "Chrome_WidgetWin_1".to_string(),
                registered_title: "App".to_string(),
                title_contains: needle.map(str::to_string),
                title_regex: None,
            },
            main_placement: SavedPlacement {
                monitor_id: "MAIN".to_string(),
                main_monitor_index: 0,
                normalized_rect: NormalizedRect {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                },
                physical_rect_at_capture: PixelRect::new(0, 0, 800, 600),
                show_state: SavedShowState::Normal,
            },
            z_order: 0,
            launch_spec: None,
            minimize_when_parked: false,
        }
    }

    fn workset(name: &str, windows: Vec<ManagedWindow>) -> Workset {
        Workset {
            id: Uuid::new_v4(),
            name: name.to_string(),
            repository_path: r"C:\repo".into(),
            repository_kind: RepositoryKind::Git,
            color: "#000000".to_string(),
            sort_order: 0,
            direct_hotkey: None,
            parking_policy: ParkingPolicy::Auto,
            fullscreen_when_parked: false,
            windows,
            created_at: "2026-07-29T00:00:00Z".to_string(),
            updated_at: "2026-07-29T00:00:00Z".to_string(),
        }
    }

    fn live(hwnd: isize, exe: &str, title: &str) -> TopLevelWindow {
        TopLevelWindow {
            hwnd,
            process_id: 1,
            executable_path: Some(PathBuf::from(exe)),
            window_class: "Chrome_WidgetWin_1".to_string(),
            title: title.to_string(),
            rect_px: PixelRect::new(0, 0, 800, 600),
        }
    }

    const CODE: &str = r"C:\Code\Code.exe";
    const BRAVE: &str = r"C:\Brave\brave.exe";

    /// 実機の再現: `repodeck` セットの登録が「02_求解・高速化」の窓を握っていた。
    #[test]
    fn a_binding_that_contradicts_its_own_needle_is_dropped() {
        let entry = Uuid::new_v4();
        let worksets = vec![workset(
            "repodeck",
            vec![managed(entry, CODE, Some("repodeck"))],
        )];
        let live_windows = vec![live(67630, CODE, "02_求解・高速化 (ワークスペース)")];
        let mut bindings = HashMap::from([(entry, 67630)]);

        let report = check_and_repair(&worksets, &live_windows, &mut bindings);

        assert_eq!(report.dropped_bindings(), 1);
        assert!(bindings.is_empty(), "捨てて付け直させる");
        assert!(matches!(
            report.anomalies.first(),
            Some(Anomaly::ContradictedBinding { hwnd: 67630, .. })
        ));
    }

    #[test]
    fn a_binding_that_still_satisfies_its_needle_is_left_alone() {
        let entry = Uuid::new_v4();
        let worksets = vec![workset(
            "repodeck",
            vec![managed(entry, CODE, Some("repodeck"))],
        )];
        let live_windows = vec![live(264584, CODE, "main.rs - repodeck")];
        let mut bindings = HashMap::from([(entry, 264584)]);

        let report = check_and_repair(&worksets, &live_windows, &mut bindings);

        assert!(report.anomalies.iter().all(|a| !a.is_repairable()));
        assert_eq!(bindings.get(&entry), Some(&264584));
    }

    /// 実機の再現: 1枚の Brave を4セットが同時に握っていた。
    #[test]
    fn sharing_one_window_without_evidence_drops_every_unjustified_holder() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let worksets = vec![
            workset("動画", vec![managed(a, BRAVE, None)]),
            workset("repodeck", vec![managed(b, BRAVE, None)]),
        ];
        let live_windows = vec![live(131970, BRAVE, "再生 | U-NEXT - Brave")];
        let mut bindings = HashMap::from([(a, 131970), (b, 131970)]);

        let report = check_and_repair(&worksets, &live_windows, &mut bindings);

        assert_eq!(report.dropped_bindings(), 2);
        assert!(bindings.is_empty());
        assert!(
            report
                .anomalies
                .iter()
                .any(|x| matches!(x, Anomaly::UnjustifiedSharing { hwnd: 131970, .. }))
        );
    }

    /// 同じ窓を複数セットで使うのは意図的に許されている。根拠を持っている
    /// 登録どうしの共有まで壊してはいけない。
    #[test]
    fn justified_sharing_of_one_window_is_not_an_anomaly() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let worksets = vec![
            workset("A", vec![managed(a, CODE, Some("repodeck"))]),
            workset("B", vec![managed(b, CODE, Some("repodeck"))]),
        ];
        let live_windows = vec![live(42, CODE, "main.rs - repodeck")];
        let mut bindings = HashMap::from([(a, 42), (b, 42)]);

        let report = check_and_repair(&worksets, &live_windows, &mut bindings);

        assert_eq!(report.dropped_bindings(), 0);
        assert_eq!(bindings.len(), 2);
    }

    /// 1つの登録が根拠なしに握っているのは異常ではない。起動して掴んだ窓は
    /// タイトルの根拠を持たないことがある（URL 未確定のブラウザなど）。
    #[test]
    fn a_lone_holder_without_evidence_is_left_alone() {
        let entry = Uuid::new_v4();
        let worksets = vec![workset("動画", vec![managed(entry, BRAVE, None)])];
        let live_windows = vec![live(131970, BRAVE, "再生 | U-NEXT - Brave")];
        let mut bindings = HashMap::from([(entry, 131970)]);

        let report = check_and_repair(&worksets, &live_windows, &mut bindings);

        assert_eq!(report.dropped_bindings(), 0);
        assert_eq!(bindings.get(&entry), Some(&131970));
    }

    #[test]
    fn a_binding_whose_window_is_gone_is_kept_as_the_closed_signal() {
        let entry = Uuid::new_v4();
        let worksets = vec![workset(
            "repodeck",
            vec![managed(entry, CODE, Some("repodeck"))],
        )];
        let mut bindings = HashMap::from([(entry, 999)]);

        let report = check_and_repair(&worksets, &[], &mut bindings);

        assert_eq!(report.dropped_bindings(), 0);
        assert_eq!(bindings.get(&entry), Some(&999));
    }

    /// 検出器は読むだけ。修正器を呼ばない限り紐づけ表は動かない。
    #[test]
    fn the_detector_never_mutates_the_bindings() {
        let entry = Uuid::new_v4();
        let worksets = vec![workset(
            "repodeck",
            vec![managed(entry, CODE, Some("repodeck"))],
        )];
        let live_windows = vec![live(67630, CODE, "02_求解・高速化 (ワークスペース)")];
        let bindings = HashMap::from([(entry, 67630)]);

        let anomalies = detect(&worksets, &live_windows, &bindings);

        assert_eq!(anomalies.len(), 1);
        assert_eq!(bindings.get(&entry), Some(&67630), "検出器は書き換えない");
    }

    /// 修正器は検出器が挙げたものしか触らない。
    #[test]
    fn the_repairer_only_touches_what_the_detector_reported() {
        let (bad, good) = (Uuid::new_v4(), Uuid::new_v4());
        let mut bindings = HashMap::from([(bad, 1), (good, 2)]);

        let outcome = repair(
            &[Anomaly::ContradictedBinding {
                entry: bad,
                workset: "repodeck".to_string(),
                hwnd: 1,
                expected: "repodeck".to_string(),
                actual: "別のフォルダ".to_string(),
            }],
            &mut bindings,
        );

        assert_eq!(outcome.dropped_bindings, 1);
        assert_eq!(outcome.pruned_stale, 0);
        assert_eq!(bindings.get(&good), Some(&2), "挙がっていない側は残る");
        assert!(!bindings.contains_key(&bad));
    }

    /// 直せない異常しか無ければ、修正器は呼ばれず紐づけ表も動かない。
    #[test]
    fn an_unrepairable_only_report_leaves_the_bindings_untouched() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let worksets = vec![
            workset("動画", vec![managed(a, BRAVE, None)]),
            workset("repodeck", vec![managed(b, BRAVE, None)]),
        ];
        // それぞれ別の窓を握っているので、共有の異常は起きない。
        let live_windows = vec![live(1, BRAVE, "A - Brave"), live(2, BRAVE, "B - Brave")];
        let mut bindings = HashMap::from([(a, 1), (b, 2)]);

        let report = check_and_repair(&worksets, &live_windows, &mut bindings);

        assert!(report.detected_anything(), "区別不能な登録は報告される");
        assert!(!report.repair.touched_anything());
        assert_eq!(bindings.len(), 2);
    }

    #[test]
    fn bindings_of_deleted_registrations_are_pruned() {
        let mut bindings = HashMap::from([(Uuid::new_v4(), 1), (Uuid::new_v4(), 2)]);

        let report = check_and_repair(&[], &[], &mut bindings);

        assert_eq!(report.pruned_stale(), 2);
        assert!(bindings.is_empty());
    }

    /// Brave の登録14個のように、互いに見分けようのない登録は直せない。
    /// 黙って壊れるのではなく、設定の問題として報告する。
    #[test]
    fn indistinguishable_registrations_are_reported_but_not_repaired() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let worksets = vec![
            workset("動画", vec![managed(a, BRAVE, None)]),
            workset("repodeck", vec![managed(b, BRAVE, None)]),
        ];

        let found = indistinguishable(&worksets);

        assert_eq!(found.len(), 1);
        let Anomaly::IndistinguishableRegistrations { entries, .. } = &found[0] else {
            panic!(
                "expected IndistinguishableRegistrations, got {:?}",
                found[0]
            );
        };
        assert_eq!(entries.len(), 2);
        assert!(!found[0].is_repairable());
    }

    #[test]
    fn a_registration_with_a_needle_is_never_reported_as_indistinguishable() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let worksets = vec![
            workset("A", vec![managed(a, CODE, Some("repodeck"))]),
            workset("B", vec![managed(b, CODE, Some("sourcecast"))]),
        ];

        assert!(indistinguishable(&worksets).is_empty());
    }
}
