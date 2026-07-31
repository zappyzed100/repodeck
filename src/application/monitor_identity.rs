//! 物理モニターの同一性を `\\.\DISPLAYn` から切り離すための再マッピング。
//!
//! Windows のデバイス名 `\\.\DISPLAYn` は、GPU がディスプレイを再列挙すると
//! (スリープ復帰・モニターの電源断・ケーブルの抜き差し) 物理モニターとの
//! 対応が入れ替わることがある。デスクトップの解像度も配置も変わらないので
//! `WM_DISPLAYCHANGE` すら飛ばず、[`display_recovery_service`] の
//! 「保存済みモニターが live から消えたか」判定も——名前は7枚とも揃った
//! ままなので——素通りする。それでも保存済みレイアウトは `stable_id`
//! (= デバイス名) をキーにしているため、メイン画面・退避先・ウィンドウの
//! 復元先がまとめて別のモニターを指す
//! (ユーザー報告 2026-07-24: 復帰したら退避したウィンドウがメイン画面に出た)。
//!
//! そこでこのモジュールは「同じ物理モニターが別の名前で戻ってきた」ことを
//! 検知し、保存済み設定のデバイス名参照を丸ごと付け替える計画を立てる。
//! 判定は純粋関数なので実機なしでテストできる。副作用 (Win32 の列挙と
//! 保存) は `app.rs` 側が持つ。
//!
//! [`display_recovery_service`]: crate::application::display_recovery_service

use std::collections::{HashMap, HashSet};

use crate::domain::config::AppConfig;
use crate::domain::monitor::SavedMonitor;
use crate::domain::placement::PixelRect;

/// 再マッピング判定に必要なライブモニターの情報だけを抜き出したもの。
/// `windowing::monitor` の `MonitorInfo` をそのまま使わないのは、この層を
/// Win32 から独立させてテスト可能に保つため。
#[derive(Debug, Clone, PartialEq)]
pub struct LiveMonitor {
    pub device_name: String,
    /// `EnumDisplayDevicesW(EDD_GET_DEVICE_INTERFACE_NAME)` が返すデバイス
    /// インターフェース名 (`\\?\DISPLAY#HKC2496#5&2da23&0&UID4357#{GUID}`)。
    /// EDID とコネクタ由来なので再列挙をまたいでも同じ物理モニターを指す。
    /// 取得できなければ `None`。
    pub device_path: Option<String>,
    pub bounds_px: PixelRect,
}

/// 旧デバイス名 → 新デバイス名。空なら付け替え不要。
pub type RenameMap = HashMap<String, String>;

fn bounds_key(r: PixelRect) -> (i32, i32, i32, i32) {
    (r.x, r.y, r.width, r.height)
}

/// 一度しか現れない bounds の集合。ミラーリング等で座標が完全に重なる
/// モニターがあると bounds では区別できないため、そういう bounds は
/// 対応付けの根拠にしない。
fn unique_bounds<I: Iterator<Item = PixelRect>>(bounds: I) -> HashSet<(i32, i32, i32, i32)> {
    let mut seen: HashMap<(i32, i32, i32, i32), usize> = HashMap::new();
    for b in bounds {
        *seen.entry(bounds_key(b)).or_insert(0) += 1;
    }
    seen.into_iter()
        .filter(|(_, count)| *count == 1)
        .map(|(key, _)| key)
        .collect()
}

/// 保存済みモニターと現在のライブモニターを突き合わせ、デバイス名の
/// 付け替え計画を返す。付け替え不要なら空。
///
/// 対応付けの優先順位:
///
/// 1. `device_path` の一致。唯一の本当に安定な同一性なので最優先。
/// 2. bounds の完全一致。`device_path` を保存していない古い設定
///    (このフィールドは長らく `None` のまま書かれていた) を救うための
///    フォールバック。座標が重複する bounds は根拠にしない。
///
/// 安全側に倒すための不変条件:
///
/// - 対応付けは 1:1。1つのライブモニターが2つの保存モニターに割り当たらない。
/// - 対応の付かなかった保存モニター (本当に外されたモニター) の名前が
///   付け替え先になる場合は、衝突するので計画全体を捨てる。
/// - ユーザーがモニターを物理的に並べ替えた場合は 1 も 2 も一致しないので、
///   何も起きない (勝手に設定を書き換えない)。
pub fn plan_remap(saved: &[SavedMonitor], live: &[LiveMonitor]) -> RenameMap {
    let mut pairs: Vec<(&SavedMonitor, &LiveMonitor)> = Vec::new();
    let mut taken_live: HashSet<&str> = HashSet::new();
    let mut matched_saved: HashSet<&str> = HashSet::new();

    // 1. device_path 一致
    for s in saved {
        let Some(path) = s.device_path.as_deref() else {
            continue;
        };
        let hit = live.iter().find(|l| {
            l.device_path.as_deref() == Some(path) && !taken_live.contains(l.device_name.as_str())
        });
        if let Some(l) = hit {
            taken_live.insert(l.device_name.as_str());
            matched_saved.insert(s.stable_id.as_str());
            pairs.push((s, l));
        }
    }

    // 2. bounds 一致でフォールバック
    let saved_unique = unique_bounds(saved.iter().map(|s| s.bounds_px));
    let live_unique = unique_bounds(live.iter().map(|l| l.bounds_px));
    for s in saved {
        if matched_saved.contains(s.stable_id.as_str())
            || !saved_unique.contains(&bounds_key(s.bounds_px))
        {
            continue;
        }
        let hit = live.iter().find(|l| {
            l.bounds_px == s.bounds_px
                && live_unique.contains(&bounds_key(l.bounds_px))
                && !taken_live.contains(l.device_name.as_str())
        });
        if let Some(l) = hit {
            taken_live.insert(l.device_name.as_str());
            matched_saved.insert(s.stable_id.as_str());
            pairs.push((s, l));
        }
    }

    let renames: RenameMap = pairs
        .iter()
        .filter(|(s, l)| s.stable_id != l.device_name)
        .map(|(s, l)| (s.stable_id.clone(), l.device_name.clone()))
        .collect();

    if renames.is_empty() {
        return RenameMap::new();
    }

    // 対応の付かなかった保存モニターの名前へ付け替えようとしていたら、
    // 同じ `stable_id` が2つできてしまうので計画ごと捨てる。
    let collides = saved.iter().any(|s| {
        !matched_saved.contains(s.stable_id.as_str())
            && renames.values().any(|new| new == &s.stable_id)
    });
    if collides {
        return RenameMap::new();
    }

    renames
}

fn rename(id: &mut String, renames: &RenameMap, changed: &mut usize) {
    if let Some(new) = renames.get(id.as_str()) {
        *id = new.clone();
        *changed += 1;
    }
}

/// [`plan_remap`] の計画を、デバイス名を持つ保存データすべてに適用する。
/// 書き換えた参照の数を返す。
///
/// 参照の在り処を1か所に集めておくのが目的なので、新しくデバイス名を
/// 保持するフィールドが増えたらここにも足すこと。
pub fn apply_remap(
    config: &mut AppConfig,
    auto_slot_assignments: &mut HashMap<String, String>,
    last_seen_monitor_fingerprint: &mut Option<String>,
    renames: &RenameMap,
) -> usize {
    let mut changed = 0;

    for monitor in &mut config.monitors {
        rename(&mut monitor.stable_id, renames, &mut changed);
        if let Some(new) = renames.get(monitor.device_name.as_str()) {
            monitor.device_name = new.clone();
        }
    }
    for id in &mut config.main_monitor_ids {
        rename(id, renames, &mut changed);
    }
    for sub in &mut config.sub_screens {
        for id in &mut sub.monitor_ids {
            rename(id, renames, &mut changed);
        }
    }
    for slot in &mut config.fixed_slots {
        rename(&mut slot.monitor_id, renames, &mut changed);
    }
    for workset in &mut config.worksets {
        for window in &mut workset.windows {
            rename(&mut window.main_placement.monitor_id, renames, &mut changed);
        }
    }

    // `auto_slot_assignments` の値は parking_allocator の `<device_name>::<cell>` 形式。
    for slot in auto_slot_assignments.values_mut() {
        let Some((name, cell)) = slot.rsplit_once("::") else {
            continue;
        };
        if let Some(new) = renames.get(name) {
            *slot = format!("{new}::{cell}");
            changed += 1;
        }
    }

    if let Some(fingerprint) = last_seen_monitor_fingerprint
        && let Some(rewritten) = rename_fingerprint(fingerprint, renames)
    {
        *fingerprint = rewritten;
        changed += 1;
    }

    changed
}

/// `monitor_watch_service::compute_fingerprint` が作る
/// `<device_name>:<x>,<y>,<w>,<h>` を `|` で連ねた文字列のデバイス名を付け替える。
/// 何も変わらなければ `None`。
///
/// ここを忘れていたせいで、付け替えの直後は指紋だけが旧名のまま残っていた。
/// 次に `WM_DISPLAYCHANGE` が来ると同値判定が必ず外れ、実際には何も変わって
/// いないのに「トポロジが変わった」として画面外ウィンドウの掃除が走る
/// （実機で確認、2026-07-29）。
fn rename_fingerprint(fingerprint: &str, renames: &RenameMap) -> Option<String> {
    let mut touched = false;
    let mut parts: Vec<String> = fingerprint
        .split('|')
        .map(|part| {
            let Some((name, rect)) = part.split_once(':') else {
                return part.to_string();
            };
            match renames.get(name) {
                Some(new) => {
                    touched = true;
                    format!("{new}:{rect}")
                }
                None => part.to_string(),
            }
        })
        .collect();
    if !touched {
        return None;
    }
    // `compute_fingerprint` は並べ替えてから連結する。付け替えで順序が変わりうる
    // ので、比較相手と同じ正規形に戻す。
    parts.sort();
    Some(parts.join("|"))
}

/// 現在のライブモニターから `device_path` を保存済みモニターへ焼き直す。
///
/// これを書いておかないと次回の入れ替わりを bounds でしか判定できない。
/// 何か変わったら `true`。
///
/// **既に別の `device_path` が入っている行は上書きしない。** 対応付けは
/// `stable_id == device_name` という「入れ替わっているかもしれない前提」で
/// 取っているので、[`plan_remap`] が衝突や対応不能で計画を捨てた直後にここを
/// 素通しすると、旧 `stable_id` に**別の物理モニター**の path を焼き付けて
/// しまう。そうなると次回からはその捻れを `device_path` では検知できなくなり、
/// 唯一の安定な同一性を自分で壊すことになる。食い違いは呼び出し側が警告する。
pub fn refresh_device_paths(config: &mut AppConfig, live: &[LiveMonitor]) -> RefreshOutcome {
    let mut outcome = RefreshOutcome::default();
    for monitor in &mut config.monitors {
        let Some(l) = live.iter().find(|l| l.device_name == monitor.stable_id) else {
            continue;
        };
        let Some(live_path) = l.device_path.as_deref() else {
            continue;
        };
        match monitor.device_path.as_deref() {
            Some(saved) if saved == live_path => {}
            // 空欄への初回書き込みだけが安全。
            None => {
                monitor.device_path = Some(live_path.to_string());
                outcome.filled += 1;
            }
            Some(_) => outcome.conflicting.push(monitor.stable_id.clone()),
        }
    }
    outcome
}

/// [`refresh_device_paths`] の結果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RefreshOutcome {
    /// `device_path` が空だった行を埋めた件数。
    pub filled: usize,
    /// 保存済みの `device_path` と、同じ名前のライブモニターの path が食い違った
    /// 行の `stable_id`。名前と物理モニターの対応がねじれている証拠なので、
    /// 上書きせずそのまま残してある。
    pub conflicting: Vec<String>,
}

impl RefreshOutcome {
    pub fn changed(&self) -> bool {
        self.filled > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::monitor::AutoSplit;

    fn saved(id: &str, path: Option<&str>, x: i32, y: i32) -> SavedMonitor {
        SavedMonitor {
            stable_id: id.to_string(),
            device_name: id.to_string(),
            device_path: path.map(str::to_string),
            friendly_name: None,
            bounds_px: PixelRect::new(x, y, 1920, 1080),
            work_area_px: PixelRect::new(x, y, 1920, 1040),
            dpi_x: 96,
            dpi_y: 96,
            auto_split: Some(AutoSplit::One),
            excluded: false,
        }
    }

    fn live(name: &str, path: Option<&str>, x: i32, y: i32) -> LiveMonitor {
        LiveMonitor {
            device_name: name.to_string(),
            device_path: path.map(str::to_string),
            bounds_px: PixelRect::new(x, y, 1920, 1080),
        }
    }

    #[test]
    fn no_renames_when_names_still_match() {
        let saved = [
            saved("A", Some("p1"), 0, 0),
            saved("B", Some("p2"), 1920, 0),
        ];
        let live = [live("A", Some("p1"), 0, 0), live("B", Some("p2"), 1920, 0)];
        assert!(plan_remap(&saved, &live).is_empty());
    }

    #[test]
    fn device_path_wins_over_position() {
        // 2枚が入れ替わり、ついでに配置も左右逆になった。device_path を
        // 持っているので座標ではなくハードウェアで追跡できる。
        let saved = [
            saved("A", Some("p1"), 0, 0),
            saved("B", Some("p2"), 1920, 0),
        ];
        let live = [live("B", Some("p1"), 1920, 0), live("A", Some("p2"), 0, 0)];

        let renames = plan_remap(&saved, &live);
        assert_eq!(renames.get("A"), Some(&"B".to_string()));
        assert_eq!(renames.get("B"), Some(&"A".to_string()));
    }

    #[test]
    fn falls_back_to_bounds_when_no_device_path_was_saved() {
        // 2026-07-24 に実際に起きた形: device_path 未保存の設定で名前だけ巡回。
        let saved = [
            saved("\\\\.\\DISPLAY1", None, 0, 0),
            saved("\\\\.\\DISPLAY2", None, 1920, 0),
        ];
        let live = [
            live("\\\\.\\DISPLAY2", Some("p1"), 0, 0),
            live("\\\\.\\DISPLAY1", Some("p2"), 1920, 0),
        ];

        let renames = plan_remap(&saved, &live);
        assert_eq!(renames.len(), 2);
        assert_eq!(
            renames.get("\\\\.\\DISPLAY1"),
            Some(&"\\\\.\\DISPLAY2".to_string())
        );
        assert_eq!(
            renames.get("\\\\.\\DISPLAY2"),
            Some(&"\\\\.\\DISPLAY1".to_string())
        );
    }

    #[test]
    fn rearranged_monitors_are_left_alone() {
        // ユーザーが物理的に並べ替えただけ: device_path も bounds も一致しない。
        // 勝手に付け替えず、Layout Studio での保存し直しに任せる。
        let saved = [saved("A", None, 0, 0), saved("B", None, 1920, 0)];
        let live = [live("A", None, 0, 1080), live("B", None, 1920, 1080)];
        assert!(plan_remap(&saved, &live).is_empty());
    }

    #[test]
    fn duplicated_bounds_are_never_matched_by_position() {
        // ミラーリングで2枚が完全に重なっている場合、bounds では区別できない。
        let saved = [saved("A", None, 0, 0), saved("B", None, 0, 0)];
        let live = [live("B", None, 0, 0), live("A", None, 0, 0)];
        assert!(plan_remap(&saved, &live).is_empty());
    }

    #[test]
    fn a_disconnected_monitor_does_not_block_the_others() {
        let saved = [
            saved("A", Some("p1"), 0, 0),
            saved("B", Some("p2"), 1920, 0),
            saved("C", Some("p3"), 3840, 0),
        ];
        // C は外された。A と B は名前が入れ替わって戻ってきた。
        let live = [live("B", Some("p1"), 0, 0), live("A", Some("p2"), 1920, 0)];

        let renames = plan_remap(&saved, &live);
        assert_eq!(renames.get("A"), Some(&"B".to_string()));
        assert_eq!(renames.get("B"), Some(&"A".to_string()));
        assert!(!renames.contains_key("C"));
    }

    #[test]
    fn a_rename_colliding_with_a_disconnected_monitor_is_refused() {
        // A を C へ付け替えると、外れている C と stable_id が衝突する。
        let saved = [
            saved("A", Some("p1"), 0, 0),
            saved("C", Some("p3"), 3840, 0),
        ];
        let live = [live("C", Some("p1"), 0, 0)];
        assert!(plan_remap(&saved, &live).is_empty());
    }

    #[test]
    fn apply_remap_rewrites_every_place_a_device_name_is_stored() {
        use crate::domain::config::SubScreen;
        use crate::domain::placement::{NormalizedRect, SavedPlacement, SavedShowState};
        use crate::domain::workset::{
            FixedParkingSlot, ManagedWindow, ParkingPolicy, RepositoryKind, WindowMatcher,
        };
        use uuid::Uuid;

        let mut config = AppConfig::new_empty();
        config.monitors = vec![saved("A", None, 0, 0)];
        config.main_monitor_ids = vec!["A".to_string()];
        config.sub_screens = vec![SubScreen {
            id: Uuid::new_v4(),
            name: "sub".to_string(),
            monitor_ids: vec!["A".to_string()],
            split: AutoSplit::One,
            cell_index: 0,
            fullscreen: false,
        }];
        config.fixed_slots = vec![FixedParkingSlot {
            id: Uuid::new_v4(),
            monitor_id: "A".to_string(),
            grid: AutoSplit::One,
            cell_index: 0,
            assigned_workset_id: Uuid::new_v4(),
        }];
        config.worksets = vec![crate::domain::workset::Workset {
            id: Uuid::new_v4(),
            name: "ws".to_string(),
            repository_path: std::path::PathBuf::from(r"D:\repo"),
            repository_kind: RepositoryKind::Git,
            color: "#fff".to_string(),
            sort_order: 0,
            direct_hotkey: None,
            parking_policy: ParkingPolicy::Auto,
            fullscreen_when_parked: false,
            windows: vec![ManagedWindow {
                id: Uuid::new_v4(),
                matcher: WindowMatcher {
                    executable_path: std::path::PathBuf::from(r"C:\code.exe"),
                    process_name: "code.exe".to_string(),
                    window_class: "X".to_string(),
                    registered_title: "t".to_string(),
                    title_contains: None,
                    title_regex: None,
                },
                main_placement: SavedPlacement {
                    monitor_id: "A".to_string(),
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
            }],
            created_at: "2026-07-24T00:00:00Z".to_string(),
            updated_at: "2026-07-24T00:00:00Z".to_string(),
        }];
        let mut slots = HashMap::from([("ws".to_string(), "A::2".to_string())]);

        let mut fingerprint = Some("A:0,0,1920,1080|C:1920,0,1920,1080".to_string());

        let renames = RenameMap::from([("A".to_string(), "B".to_string())]);
        let changed = apply_remap(&mut config, &mut slots, &mut fingerprint, &renames);

        assert_eq!(changed, 7, "7か所すべてが書き換わること");
        assert_eq!(config.monitors[0].stable_id, "B");
        assert_eq!(config.monitors[0].device_name, "B");
        assert_eq!(config.main_monitor_ids, vec!["B".to_string()]);
        assert_eq!(config.sub_screens[0].monitor_ids, vec!["B".to_string()]);
        assert_eq!(config.fixed_slots[0].monitor_id, "B");
        assert_eq!(config.worksets[0].windows[0].main_placement.monitor_id, "B");
        assert_eq!(slots.get("ws"), Some(&"B::2".to_string()));
        assert_eq!(
            fingerprint.as_deref(),
            Some("B:0,0,1920,1080|C:1920,0,1920,1080"),
            "指紋を置き去りにすると、次の WM_DISPLAYCHANGE で必ず誤検知する"
        );
    }

    /// 付け替えで名前の辞書順が変わっても、`compute_fingerprint` と同じ
    /// 並べ替え済みの形に戻さないと比較が一致しない。
    #[test]
    fn a_renamed_fingerprint_is_re_sorted_into_the_canonical_form() {
        let mut config = AppConfig::new_empty();
        let mut slots = HashMap::new();
        let mut fingerprint = Some("A:0,0,100,100|B:100,0,100,100".to_string());
        let renames = RenameMap::from([("A".to_string(), "Z".to_string())]);

        apply_remap(&mut config, &mut slots, &mut fingerprint, &renames);

        assert_eq!(
            fingerprint.as_deref(),
            Some("B:100,0,100,100|Z:0,0,100,100")
        );
    }

    #[test]
    fn refresh_device_paths_backfills_missing_paths() {
        let mut config = AppConfig::new_empty();
        config.monitors = vec![saved("A", None, 0, 0)];
        let live = [live("A", Some("p1"), 0, 0)];

        assert!(refresh_device_paths(&mut config, &live).changed());
        assert_eq!(config.monitors[0].device_path.as_deref(), Some("p1"));
        assert!(!refresh_device_paths(&mut config, &live).changed());
    }

    /// 付け替えを断念した直後にここを素通しすると、旧 `stable_id` に別の物理
    /// モニターの path を焼き付け、以後その捻れを検知できなくなる。
    #[test]
    fn refresh_device_paths_never_overwrites_a_conflicting_path() {
        let mut config = AppConfig::new_empty();
        config.monitors = vec![saved("A", Some("p_old"), 0, 0)];
        let live = [live("A", Some("p_new"), 0, 0)];

        let outcome = refresh_device_paths(&mut config, &live);

        assert_eq!(config.monitors[0].device_path.as_deref(), Some("p_old"));
        assert!(!outcome.changed());
        assert_eq!(outcome.conflicting, vec!["A".to_string()]);
    }
}
