# RepoDeck 開発計画（詳細仕様） — Phase・データモデル・アルゴリズム正本

> `PLAN.md`（全体計画・アーキテクチャ・技術選定理由の正本）から参照される詳細ドキュメント。
> 用語・不変条件・ユーザー操作仕様・配置アルゴリズム・ウィンドウ識別仕様・AIエージェント連携・
> データモデル・技術構成・アーキテクチャ詳細・エラー復旧・セキュリティ・性能要件・Phaseごとの
> 実装手順とテストと完了条件・UI受け入れ仕様・ログ仕様・配布仕様・MVP受け入れ基準・最終確認コマンドの
> 正本はこのファイルとする。`PLAN.md`はここへのポインタと概要のみを持つ。

- 文書版：2.0
- 対象アプリ版：0.1.0 MVP
- 対象OS：Windows 11 x64
- 最終更新：2026-07-20
- 実装言語：Rust 2024 Edition
- UI：Slint 1.17

---

## 現在の実装状況（2026-07-20時点）

- **Phase 1（プロジェクト基盤）: 完了・検証済み。** Cargoプロジェクト、`ui/app-window.slint`最小ウィンドウ、
  `tracing`による日次ローテーションログ、`CreateMutexW`ベースの単一インスタンス検出、
  `embed-manifest`によるPerMonitorV2 DPI・asInvoker実行レベルのWindowsマニフェスト埋め込み、
  LICENSE（MIT）・THIRD_PARTY_NOTICES.mdを実装。実機でexe起動・二重起動時の即時終了・
  ログ出力を確認済み。
- **Phase 2（Win32ウィンドウ・モニター基盤）: 完了・検証済み。** `src/windowing/`に
  `monitor.rs`（`EnumDisplayMonitors`＋DPI取得）、`enumerate.rs`（`EnumWindows`＋§5.2除外フィルター）、
  `placement.rs`（`GetWindowPlacement`／`SetWindowPos`／`BeginDeferWindowPos`一括移動）、
  `dpi.rs`、`win32_error.rs`を実装。`src/domain/placement.rs`に`PixelRect`／`NormalizedRect`／
  `SavedShowState`と正規化⇔物理座標変換・外接矩形・アフィン変換の純粋関数を実装。
  実機7画面（負座標・異解像度を含む）でのモニター列挙、および実Notepadウィンドウを使った
  100往復配置・最大化中の通常配置取得・`BeginDeferWindowPos`一括移動の手動E2Eテスト
  （`tests/windows_e2e.rs`、`#[ignore]`）で実際に成功を確認済み。
- **Phase 3（設定とドメインモデル）: 完了・検証済み。** `src/domain/{config,workset,monitor}.rs`に
  §7.2のRust型を実装し、`AppConfig::validate()`でワークセット名長・絶対パス・ID重複・固定枠重複・
  未参照固定枠・main monitor必須・正規表現コンパイル可否・hotkey修飾キー必須を検証。
  `src/persistence/{config_store,runtime_store,journal_store,migrations}.rs`で
  §7.5の原子的保存（backup→tmp書込→flush/sync→再検証→rename）、§10.4のbackup復旧・
  破損ファイルの日時付き退避、§7.6のスキーマバージョン必須化と未来バージョン拒否を実装。
  30件の自動テストが実データ（実モニター構成・実ファイルシステム）に対して green。
- **Phase 4（レイアウトスタジオ）: 完了・検証済み。** `ui/layout-studio.slint`＋
  `src/application/layout_service.rs`（正規化座標変換とは別の、モニター一覧をキャンバス上へ
  縦横比を保って投影する純粋関数、1/2/4自動分割のセル計算、メイン画面上のウィンドウ検出、
  Undoスナップショット型）と`src/app.rs`の`wire_layout_studio`で実装。
  タスクトレイに「レイアウトスタジオを開く」を追加し、`AppConfig`は起動時に
  `config_store::load`で読み込み・`Rc<RefCell<AppConfig>>`で共有するようにした。
  実機7画面（負座標含む）に対して、`PrintWindow`によるスクリーンショットと
  `SetForegroundWindow`／合成マウスクリックを使った実際のUI操作で
  「モニター縮小図が正しい相対位置・縦横比で表示される」「メイン画面をクリックで選択し
  MAIN 1/MAIN 2として適用できる」「設定を保存するとconfig.jsonへ正しく書き込まれる」
  「プロセス再起動後に選択状態が復元される」ことを確認済み。
  「メインを空にする」機能自体は実装済みだが、実行中の実ウィンドウ（VS Code等）を
  最小化してしまうリスクがあるため対話的な実機テストは意図的に見送り、内部で使う
  Win32操作（`get_show_state`/`get_normal_rect`/`minimize`/`restore`/`maximize`）は
  Phase 2のNotepad実機E2Eテストで既に検証済みのものを再利用している。
  固定枠割当（§3.7）は`FixedParkingSlot.assigned_workset_id`が必須のUuidであり
  ワークセットが1件も存在しない現時点では作成不能なため、右パネルには
  「ワークセット登録後に利用できます」という案内のみを表示し、実際の割当UIはPhase 5以降に
  持ち越した。デバッグ用に環境変数`REPODECK_DEBUG_OPEN_LAYOUT_STUDIO=1`を設定すると
  起動時にLayout Studioを自動表示できる（`src/app.rs`）。
- **タスクトレイ・単一インスタンス関連（Phase 7の一部を先行実装）**: `SystemTrayIcon`
  （開く／レイアウトスタジオを開く／終了のメニュー、左クリックで再表示）、ウィンドウを閉じても
  トレイに常駐する既定動作（`CloseRequestResponse::HideWindow`）、`SetCurrentProcessExplicitAppUserModelID`
  によるプロセス識別、`.ico`をビルド時に生成して実行ファイルへ埋め込みタスクバーピン留め時の
  アイコン欠落を解消、`#![windows_subsystem = "windows"]`でコンソール非表示化、常駐中に
  `repodeck.exe`を再度起動すると名前付きイベントで通知して既存ウィンドウを再表示、を実装・
  実機確認済み。グローバルホットキー登録とクイックスイッチャーUI自体はまだ未実装。
- **Phase 5（ワークセット登録・照合）: 完了・検証済み。** `src/windowing/matcher.rs`（タイトル正規化、
  Levenshtein類似度、§5.5のスコアリング表通り、75点かつ次点との差20点以上で自動再バインド、
  実行ファイルパス取得失敗時は高スコアでも自動再バインドしない）と
  `src/application/workset_service.rs`（`.git`探索、`WindowMatcher`/`ManagedWindow`構築、
  重複登録検出、複数ワークセットにまたがる貪欲な排他バインド`resolve_all_matches`）を実装。
  `ui/workset-dialog.slint`の`WorksetManager`ウィンドウ（一覧＋登録＋再登録）を
  `src/app.rs`の`wire_workset_manager`で配線し、タスクトレイに「セット管理を開く」を追加。
  フォルダー選択は`rfd`クレート（ネイティブIFileDialog）を利用。
  実機で「+現在の配置をセットとして登録」→実フォルダー(`C:\code\portfolio\repodeck`)を
  ネイティブダイアログで選択→Gitリポジトリとして正しく解決→実候補ウィンドウ8個
  （Brave・VS Code・ChatGPT・Chrome・エクスプローラー）を検出→登録→`config.json`への
  正しい書き込み→一覧で「8/8件自動再バインド」表示、を対話的なマウス操作で最後まで確認済み
  （検証後、テスト用ワークセットは削除済み）。曖昧候補（複数候補が近いスコア）を選ばせる
  専用UIは実装しておらず、「このウィンドウを再登録」ボタンは常に最高スコアの候補へ
  再バインドする簡略実装（§5.6の「ユーザーが明示選択できる」という要件を完全には満たさない）。
  デバッグ用に環境変数`REPODECK_DEBUG_OPEN_WORKSET_MANAGER=1`で起動時にセット管理を
  自動表示できる。
  また、Slintの`if`条件分岐で表示切替する最上位パネルに明示的な`width`/`preferred-width`が
  無いと、切り替え時にOSウィンドウ全体が狭い方のブランチの最小幅まで縮小してしまう実バグを
  発見・修正した（`LayoutStudio`／`WorksetManager`とも`preferred-width`ではなく固定`width`/
  `height`をWindow直下に指定することで解消。`width: parent.width`はVerticalLayout内で
  バインディングループを起こすため使用不可）。
- **UI全面刷新（「ダサい」フィードバックを受けての再設計）: 完了・実機検証済み。** ユーザー指定の
  参考リポジトリ [zappyzed100/guardrails-workbench](https://github.com/zappyzed100/guardrails-workbench)
  自体にはUIコードが無く、同README記載の設計参照元3点を実際に取得して適用した:
  1. [voltagent/awesome-design-md](https://github.com/voltagent/awesome-design-md) の
     74ブランドDESIGN.mdから「透過度が高いUI」に最も一致する`apple`（backdrop-filter/blur/
     frosted/glass/vibrancy/translucentの言及数で他ブランドを比較し選定）を採用。
  2. [emilkowalski/skills](https://github.com/emilkowalski/skills) の`apple-design`
     （マテリアル階層・ヴァイブランシー・タイポグラフィのトラッキング等）と
     `emil-design-eng`（押下フィードバック・easing・「頻繁に使うUIほどアニメーションさせない」
     というRaycast由来の原則）を取得し適用。
  3. `ui-ux-pro-max-skill`は具体的な追加知見が無かったため今回は不使用。
  実装は`ui/theme.slint`に共有デザイントークン（Apple System Blue `#0a84ff`、ガラス面の
  半透明色階層、pill形ボタン、ヒアラインボーダー、Segoe UI Variable、110–170msの
  press feedback）と共有コンポーネント`GlassButton`/`GlassCheckbox`を集約し、
  `AppWindow`／`LayoutStudio`／`WorksetManager`の3ウィンドウ全てに適用。
  Rust側では`src/app.rs`の`apply_glass_backdrop()`が各ウィンドウの生HWNDを
  （`slint`クレートの`raw-window-handle-06` feature経由で）取得し、
  `DwmSetWindowAttribute`で`DWMWA_USE_IMMERSIVE_DARK_MODE`と
  `DWMWA_SYSTEMBACKDROP_TYPE = DWMSBT_TRANSIENTWINDOW`を設定、Slint側は
  `background: transparent`としてWindows 11ネイティブの背景コンポジット（Mica/Acrylic系）を
  そのまま活かす構成にした。`CopyFromScreen`による実画面キャプチャで、背後のVS Codeが
  実際に透けて見えることを確認済み。
  なお実機のWindows設定で「透明効果」(`HKCU\...\Themes\Personalize!EnableTransparency`)が
  `0`（オフ）だったため、ぼかし(blur)は掛からずクリアな透過として見える。この設定はOS全体に
  影響するためRepoDeck側からは変更していない。ユーザーがWindowsの設定でオンにすれば、
  同じコードのままDWMがぼかし付きのAcrylic/Micaとして描画するはず（未検証）。
  この修正の過程で、Slintの`Cargo.toml`に`raw-window-handle-06` feature（Phase 1で
  「存在しない」と誤判定し外していたもの）を追加した。
- **Phase 6（退避割当・切替Coordinator）: バックエンド完了・単体テスト検証済み。** 既存の
  純粋な幾何計算（`domain::placement::{normalize, denormalize, bounding_rect, affine_map}`）と
  永続化層（`journal_store`／`runtime_store`、いずれもPhase 2/3で実装済み）を土台に、
  新規オーケストレーション層のみを追加した（スキーマ変更なし）。
  - `application::parking_allocator`（§4.4／§3.7）: `ParkingSlotId`（`"{monitor_id}::{cell_index}"`
    で`RuntimeState.auto_slot_assignments`にエンコード）、固定枠を独立に先処理（対象モニター
    消失時は最小化）、非メインモニターを`monitor_resolution::sort_monitors_reading_order`で
    読み順に並べ`layout_service::auto_split_cells`のセルを列挙、固定枠を除外した上で
    `sort_order`順のワークセットに対し「前回割当を維持→残りをFirst Fit→溢れは最小化」を実装。
  - `application::parking_placement`（§4.3）: `bounding_rect`/`affine_map`を組み合わせ、
    8px内側余白・最小120×68pxを下回る場合はセット全体を最小化（部分的な退避＋最小化の
    混在は作らない）。
  - `application::main_placement`（§4.2）: `denormalize`を土台に、インデックス範囲外→
    先頭メインモニターへのフォールバック、160×90px下限、画面外クランプ、
    最大化／最小化の復元規則を実装。
  - `application::window_ops::WindowOps`トレイト（§9.3の「applicationはdomainとtraitへ依存」
    方針に対応）と、実装を`windowing::window_ops_impl::Win32WindowOps`
    （既存`windowing::placement`への薄いラッパー）、テスト専用の`fake::FakeWindowOps`
    （`EndDeferWindowPos`失敗・ウィンドウ消失を注入可能なインメモリ実装）の2系統に分離。
    `windowing::placement`には`set_z_order_after`／`set_foreground_best_effort`を追加。
  - `application::switch_coordinator::SwitchCoordinator<W: WindowOps>`: §3.8の12ステップ
    （排他ロック→現在セットなら再フォーカスのみ→`workset_service::resolve_all_matches`で
    再解決→ジャーナル保存→現在セット退避→対象セットのメイン復元→Z順復元→フォーカス→
    `current_workset_id`更新→ジャーナルclear）を実装。失敗時はジャーナルから
    ロールバックし、生存確認できないウィンドウを`unrecoverable_hwnds`として返す。
    `current_workset_id`は成功時のみ更新。全ウィンドウ回収（§10.2）用に
    `recover_all_windows`（`application::recovery_service`、退避ロックを取らず常時呼び出し可能）
    も実装。
  - **スレッド化は意図的に後回し**: §9.1の「Coordinatorスレッド」は、実際に別スレッドから
    呼ぶ相手（ホットキースレッド・クイックスイッチャーUI）がPhase 7まで存在しないため、
    今回は同期的に直接呼べるAPI＋`AtomicBool`の排他ガードのみを実装し、スレッド／
    チャネル配線はPhase 7に持ち越した。
  - テスト: `parking_allocator`／`parking_placement`／`main_placement`／`monitor_resolution`／
    `recovery_service`の純粋ロジック単体テストに加え、`switch_coordinator`は
    `FakeWindowOps`を使い「3セットを100回切替えて画面外ウィンドウ0」「固定セットが
    常に指定枠へ戻る」「`EndDeferWindowPos`相当の失敗からフォールバックで復帰」
    「フォールバックも失敗した場合のロールバック」「切替途中でウィンドウが消えた場合の
    部分ロールバック」を実機なしで決定的に検証。`tests/windows_e2e.rs`に
    `switch_between_two_real_worksets_minimizes_the_non_current_one`
    （`#[ignore]`、実Notepad2枚を使い、非メインモニターを持たない構成に絞って
    実際に最小化されることを確認）を追加。
  - UI・ホットキー・クイックスイッチャーからの呼び出し経路は未配線（Phase 7の対象）。
- **Phase 7（タスクトレイ・ホットキー・クイックスイッチャー）: 完了・実機検証済み。**
  Phase 6で作った`SwitchCoordinator`を実際に呼び出す経路（グローバルホットキー・
  クイックスイッチャーUI・トレイメニュー）を実装した。
  - `hotkey::win32_hotkey`: `RegisterHotKey`/`UnregisterHotKey`を専用スレッド
    （自前の`GetMessageW`ループ、Slint UIスレッドとは独立）で扱う`HotkeyThread`。
    衝突検出は`acquire_single_instance`と同じ`GetLastError() == ERROR_HOTKEY_ALREADY_REGISTERED`
    方式。リバインド失敗時は直前の組み合わせへ自己修復（呼び出し側は永続化済み設定の
    ロールバックだけ行えばよい）。`MOD_NOREPEAT`を常時付与（押しっぱなしで
    連続トグルするのを防止）。
  - `windowing::popup_window`: Slintに公開APIが無い2点を生HWNDで補う——
    `WS_EX_TOOLWINDOW`付与でタスクバー・Alt+Tabから除外（`exclude_from_taskbar_and_alt_tab`）、
    `WM_ACTIVATE(WA_INACTIVE)`をWNDPROCサブクラス化で監視して`close_on_focus_loss`を
    実装（`watch_deactivation`）。
  - `application::popup_placement`／`application::quick_switcher_service`:
    ポップアップ位置解決（カーソル/メインモニター中心＋フォールバック）と
    ワークセット一覧の並び替え・絞り込みを、Win32/Slintに依存しない純粋関数として実装
    （`main_placement`/`monitor_resolution`と同じ設計）。`SortMode::Recent`は
    「最後に切り替えた時刻」を記録する場所がまだ無いため`Manual`と同一に扱う
    （`sort_mode`自体のUIもまだ無い）。
  - `ui/quick-switcher.slint`: 新規`QuickSwitcher`ウィンドウ（`no-frame`＋`always-on-top`）。
    ↑/↓/Enter/Esc/Ctrl+,/数字キー1-9即切替/文字入力絞り込みを`FocusScope`で実装。
    **実装上の発見**: `key-pressed`コールバック本文に単純な`if { ... return accept; }`を
    12個前後並べただけで、このツールチェーンの`slint-build`（コンパイル時）が
    スタックオーバーフローで異常終了する実バグを踏んだ（`else if`チェーンでも同様）。
    回避策として、条件の後半を`function`に分割し1つのコールバック/関数あたりの
    連続`if`文数を減らして解消（`ui/quick-switcher.slint`のコメント参照）。
  - `ui/app-window.slint`の`AppWindow`を「設定」画面に転用（トレイ左クリックと
    二重起動時の表示先が両方クイックスイッチャーに変わり、元の簡易ウィンドウが
    どこからも開かれなくなったため）。ホットキー再設定UI（Ctrl/Alt/Shift/Winの
    チェックボックス＋キー選択のComboBox）を追加。
  - `src/app.rs`: `SwitchCoordinator<Win32WindowOps>`を`Rc`で保持し、クイックスイッチャーの
    行クリック／Enter／数字キーから`switch_to`を実際に呼び出す。ホットキースレッドと
    二重起動シグナル用スレッドは`Rc`を跨げない（`Rc`は`Send`ではない）ため、
    `UI_CONTEXT`というUIスレッド専用の`thread_local!`にconfigの`Rc`を置き、
    各スレッドは`slint::invoke_from_event_loop`経由でSend安全な小さいイベント値
    （`HotkeyUiEvent`等）だけを渡してからUIスレッド側でその`thread_local`越しに
    実体へアクセスする設計にした。起動時にウィンドウを強制表示しないよう変更
    （完了条件「GUI非表示でもプロセス継続」）。「全管理ウィンドウを回収」は
    `MessageBoxW`のYes/No確認を挟んでから実行。
  - 実機確認で2件の実バグを発見・修正済み: (1) `ComboBox`の`current-value`を
    Rustから`set_hotkey_key_choice(...)`で設定しても、`current-index`（既定0）由来の
    表示と食い違い、保存済みのキー（例:「R」）ではなく`model[0]`（「A」）が
    表示されてしまう問題 — `current-index`も明示的に同期する`hotkey-key-index`
    プロパティを追加して解消。(2) `exclude_from_taskbar_and_alt_tab`を
    ウィンドウ生成直後（初回`.show()`より前）に1度呼ぶだけでは、winit側の
    `.show()`処理が`WS_EX_APPWINDOW`を再度付与してしまい`WS_EX_TOOLWINDOW`が
    効かない — `.show()`のたびに再適用するよう修正して解消。両方とも
    `PrintWindow`によるスクリーンショットと合成キー入力／マウスクリックによる
    実機操作で発見・確認した。
  - 手動確認: 起動直後は無表示でトレイのみ常駐／設定画面でのホットキー再設定
    （実機に既存の競合と衝突→自動ロールバックのメッセージを実際に確認、
    別の組み合わせへの再設定→成功）／新しいホットキーでクイックスイッチャーが
    カーソルのあるモニター中央に正しく表示・同じホットキーで非表示（トグル）／
    タスクバー・Alt+Tab非表示（`WS_EX_TOOLWINDOW`のビット確認）を実機で確認済み。
    ワークセット未登録のため実際の切替・Esc閉じる・アウトフォーカスで閉じる・
    トレイメニュー各項目のクリックは自動化テストと単体テストの範囲でのみ検証
    （手動QAチェックリストとして残し、実機での網羅確認は次回以降）。
  - エージェント状態表示・全設定画面（ホットキー以外）・初回セットアップ
    ウィザードはPhase 7のチェックリスト外として意図的に対象外（Phase 8以降）。

### 実装メモ・既知の齟齬

- **`PopupLocation`の記法齟齬**: §3.3（クイックスイッチャー表示位置のUI仕様）は
  `CursorMonitorCenter` / `ForegroundMonitorCenter` / `FixedMonitor`の3種類を挙げているが、
  §7.2（domain型の正本）で定義される`PopupLocation` enumは`CursorMonitorCenter` /
  `MainMonitorCenter`の2種類のみで、`ForegroundMonitorCenter`と`FixedMonitor`が存在しない。
  §7.2は「次の型をdomain層の正本とする」と明記されているため、Phase 3の実装は§7.2の2種類を
  正本として`src/domain/config.rs`に実装した。
  **Phase 7で再確認済み**: `ForegroundMonitorCenter`/`FixedMonitor`は追加しないと決定した
  （§13 Phase 7のチェックリストにこれらを要求する項目が無く、`popup_placement::resolve_popup_position`
  は既存の2種類のみを実装）。将来追加する場合は§7.2の型定義とschema_versionの更新が必要。
- **Windows 11パッケージ版Notepadのプロセス間接性**: `tests/windows_e2e.rs`実装中に判明。
  Windows 11では`notepad.exe`はApp Execution Aliasで、`std::process::Command::spawn()`が返す
  PIDは実際にウィンドウを所有するプロセスのPIDと一致しない（別プロセスへ委譲される）。
  対策として、spawn前後のトップレベルウィンドウ一覧の差分とウィンドウクラス／タイトルの
  部分一致でウィンドウを特定し、終了処理も`Child::kill()`ではなく発見した実PIDへの
  `OpenProcess(PROCESS_TERMINATE)`＋`TerminateProcess`で行う実装にした。
  §14.2の`test-harness`（自前で生成する合成トップレベルウィンドウ）はこの問題を根本的に
  回避できる設計なので、Phase 9で`test-harness`を実装する際はこの間接性を踏まえずに
  直接HWNDを扱える前提のままでよい。

---

## 0. この計画書の使い方

この文書は、実装を担当するLLMまたは開発者が追加の製品判断をせず、上から順に実装できることを目的とする。ここに書かれた要件、状態遷移、データ構造、例外処理、検証方法を正本とする。

実装者は次を守ること。

1. 不明点を独自の機能追加で補わない。
2. MVP外の機能を先回りして実装しない。
3. 各Phaseの完了条件を満たしてから次へ進む。
4. `cargo fmt`、`cargo clippy`、`cargo test`を各Phaseの終了時に実行する。
5. Windows仮想デスクトップを使用しない。
6. Editor、Browser、Terminalなどのウィンドウ役割を設けない。
7. ブラウザのタブを管理しない。トップレベルウィンドウだけを管理する。
8. AIエージェント完了時に自動でワークセットを切り替えない。
9. 対象アプリを閉じたり終了したりしない。
10. ウィンドウ照合に確信がない場合、勝手に移動しない。
11. 仮実装、空のハンドラー、常時成功するテスト、`TODO`を完成扱いしない。
12. OS操作に失敗しても、ユーザーのウィンドウを画面外に残さない。

### 0.1 優先順位

判断が衝突した場合は、次の順で優先する。

1. ウィンドウを失わないこと
2. 誤ったウィンドウを移動しないこと
3. 切替処理を途中状態で終わらせないこと
4. UIが応答し続けること
5. 状態表示の正確性
6. 切替速度
7. 見た目とアニメーション

### 0.2 完成物

最終的に次を生成する。

- `repodeck.exe`：常駐GUIアプリ
- `repodeck-hook.exe`：Codexフックから状態を転送する小型コンソールアプリ
- `README.md`：日本語と英語の概要、導入、GIF、制約
- `LICENSE`：MIT License
- `THIRD_PARTY_NOTICES.md`：Slintなど依存ライブラリの表示
- `assets/`：アイコン、スクリーンショット用素材
- GitHub Release用のportable ZIP
- SHA-256チェックサム

---

## 1. 製品定義

### 1.1 アプリ名

**RepoDeck（リポデッキ）**

短い説明：

> RepoDeckは、VS Code、ブラウザ、ターミナルなどのウィンドウをリポジトリ単位の「ワークセット」として記憶し、AIエージェントの状態を見ながら、選択したセットをメイン画面へ瞬時に呼び出すWindowsアプリである。

### 1.2 解決する問題

複数のリポジトリをCodexなどで並行開発すると、次が起きる。

- どのVS Codeウィンドウがどのリポジトリか分からなくなる
- 対応するブラウザ、ターミナル、資料を毎回探す
- 開発対象を変えるたびに複数画面へ並べ直す
- AIエージェントの処理完了や入力待ちを見落とす
- 多画面環境でも空き画面を待機領域として活用できない

RepoDeckは、アプリの種類ではなく「一緒に使うウィンドウの集合」と配置を記憶し、ワークセット単位でメイン画面と待機領域を切り替える。

### 1.3 MVPの価値

ユーザーの日常操作を次の3操作へ短縮する。

1. グローバルショートカットを押す
2. ワークセットを選ぶ
3. 選択したセットがメイン画面に復元される

### 1.4 対象ユーザー

- Windowsで複数リポジトリを並行開発する個人開発者
- VS Codeとブラウザを複数ウィンドウ開く開発者
- Codex CLIまたはCodex VS Code拡張を使用する開発者
- 2画面以上、特に4～8画面を使用する開発者
- AIエージェントを複数同時実行する開発者

### 1.5 MVP対象外

- Windows 10、macOS、Linuxの正式対応
- Windows仮想デスクトップ
- ブラウザタブ単位の管理
- アプリの自動終了
- Git worktreeの作成・管理
- Gitブランチ操作
- コード差分レビュー
- GitHub Copilot Chatの内部状態監視
- ChatGPT Work Modeの完了状態監視
- クラウド同期
- チーム共有
- DWMライブサムネイル
- 音声操作
- モバイルアプリ
- AIエージェントの自動起動
- AI完了を契機とする自動ワークセット切替

---

## 2. 用語と不変条件

### 2.1 ワークセット

1つのリポジトリまたは開発案件に属するトップレベルウィンドウの集合。アプリ種類の役割は付けない。

例：

- `shift-solver-demo`のVS Code
- 同リポジトリの動作確認用Braveウィンドウ
- 同リポジトリのWindows Terminal
- 仕様書を開いたPDFビューアー

### 2.2 メイン画面

現在選択中のワークセットを大きく表示する、1台以上の物理モニター。モニター内にEditor用、Browser用などの役割は設けない。

メイン画面に登録されるのはモニターの順序だけである。各ウィンドウの位置はワークセット登録時の実配置から記憶する。

### 2.3 メイン配置

ワークセットがメイン画面にあるときの各ウィンドウの位置、サイズ、表示状態。

### 2.4 退避先

非選択ワークセットを置く場所。

退避方式は次の2種類をMVPで実装する。

- `Auto`：RepoDeckが非メイン画面の空き枠を割り当てる
- `Fixed`：ユーザーが特定モニターの特定枠を固定割り当てする

空き枠がない場合は最小化する。

### 2.5 退避枠

非メインモニター上の矩形領域。1つのワークセット全体を縮小配置するコンテナとして扱う。

退避枠内には、登録時のウィンドウ相対配置を縮小して再現する。

### 2.6 現在セット

メイン画面に表示中のワークセット。常に0または1個とする。

### 2.7 不変条件

実装中、常に次を成立させる。

- 現在セットは最大1個
- 1ウィンドウは最大1ワークセットに属する
- 1固定退避枠は最大1ワークセットに属する
- 固定退避枠同士は重複しない
- メイン画面と退避枠は重複しない
- RepoDeck自身は管理対象ウィンドウに含めない
- ウィンドウを画面外座標へ退避しない
- ウィンドウを閉じない
- 切替失敗時は可能な限り切替前配置へ戻す

---

## 3. ユーザー操作仕様

## 3.1 初回起動

初回起動時はセットアップウィザードを表示する。

手順：

1. 使用条件と「RepoDeckはウィンドウを移動・リサイズする」説明を表示
2. 検出した全モニターを縮小図で表示
3. ユーザーがメイン画面を1台以上選ぶ
4. 非メイン画面の自動退避分割数を選ぶ
5. クイックスイッチャー用ホットキーを登録
6. Codex連携は「後で設定」を許可
7. 設定保存後、レイアウトスタジオを開く

既定値：

- クイックスイッチャー：`Ctrl+Alt+R`
- 非メイン画面の最大自動分割：4
- UI表示位置：マウスカーソルのあるモニター中央
- UI外クリックで閉じる：有効
- セット選択後に閉じる：有効
- 未登録ウィンドウのメイン退避方針：毎回確認
- AI完了通知：有効
- AI完了時の自動切替：無効かつMVPでは変更不可

ホットキー登録に失敗した場合は、その場で別キーを求め、失敗した組み合わせを保存しない。

## 3.2 通常起動

- Windowsログイン時自動起動はユーザーが設定した場合のみ行う
- `repodeck.exe`は単一インスタンスとする
- 二重起動された場合、既存プロセスへ「クイックスイッチャー表示」を通知し、新プロセスは終了する
- GUIを閉じてもプロセスは終了せず、タスクトレイに残る
- 明示的な「RepoDeckを終了」でのみ終了する

## 3.3 クイックスイッチャー

### 表示

- 登録済みグローバルホットキーで表示する
- 表示中に同じホットキーを押すと非表示にする
- `Esc`で非表示にする
- UI外クリックで非表示にする設定を持つ
- トレイアイコン左クリックでも表示／非表示を切り替える
- タスクバーの通常アプリ一覧には表示しない
- `Alt+Tab`一覧には原則表示しない
- 他ウィンドウより前面に表示する

### 表示位置

設定値は次の列挙型とする。

- `CursorMonitorCenter`：カーソルのあるモニター中央。既定
- `ForegroundMonitorCenter`：現在のフォアグラウンドウィンドウがあるモニター中央
- `FixedMonitor`：ユーザー指定モニター中央

対象モニターが消失した場合は、プライマリモニター中央へフォールバックする。

> 実装メモ：本ファイル冒頭「実装メモ・既知の齟齬」の通り、§7.2のdomain型は
> `CursorMonitorCenter`／`MainMonitorCenter`の2種類のみを正本として実装済み（Phase 3）。
> `ForegroundMonitorCenter`／`FixedMonitor`をPhase 7で実装するかは着手時に再確認すること。

### 表示内容

各ワークセットを1行またはカードで表示する。

- 色
- 名前
- リポジトリ名
- AI状態
- AI状態になってからの経過時間
- 現在セット表示
- 退避先表示
- 数字キー表示

状態の表示順：

1. `needs_input`
2. `ready`
3. `blocked`
4. `running`
5. `idle`
6. `unknown`

ユーザーが固定順を選んだ場合は保存順を使用する。MVPでは設定ファイルの`sort_mode`で切り替え、UI設定も用意する。

### キーボード操作

- `↑`／`↓`：選択移動
- `Enter`：選択セットへ切替
- `1`～`9`：表示中の該当セットへ即切替
- 文字入力：名前とリポジトリパスの部分一致検索
- `Ctrl+,`：設定画面を開く
- `Esc`：閉じる

### 選択後

- 切替成功時はクイックスイッチャーを閉じる
- 切替失敗時は閉じず、失敗理由と復旧操作を表示する
- `ready`のセットを正常にメイン表示できた時点で、未確認フラグを解除して`idle`へ遷移する。ただし同一セットで別のエージェントが実行中なら集約状態を再計算する

## 3.4 タスクトレイ

Slint 1.17の`SystemTrayIcon`を使用する。

左クリック：

- クイックスイッチャーを表示／非表示

右クリックメニュー：

1. クイックスイッチャーを開く
2. レイアウトスタジオを開く
3. 新しいセットを登録
4. メイン画面を空にする
5. 全管理ウィンドウを回収
6. 設定
7. ログフォルダーを開く
8. RepoDeckを終了

トレイツールチップ：

```text
RepoDeck — 2 running / 1 needs input / 1 ready
```

## 3.5 レイアウトスタジオ

セット作成と画面設定を行う常設設定UI。クイックスイッチャーとは別ウィンドウにする。

### 画面構成

上部ツールバー：

- `メイン画面を選択`
- `メイン画面を空にする`
- `現在の配置をセットとして登録`
- `セット管理`
- `退避枠を編集`
- `元に戻す`
- `設定を保存`

中央：

- Windowsの実モニター配置を相対位置どおりに縮小表示
- 各モニターに番号、デバイス名、解像度、DPI、メイン／退避の区分を表示
- 各退避枠に割当セット名と状態色を表示
- メイン画面には`MAIN`表示だけを行い、内部役割は表示しない

右ペイン：

- 選択中モニターまたは退避枠のプロパティ
- 自動分割数：1、2、4
- 固定割当ワークセット
- 枠の座標とサイズ
- 割当解除

下部：

- 操作結果
- 警告
- Undo可能な操作

### メイン画面選択

1. `メイン画面を選択`を押す
2. モニター図を1台以上クリックする
3. 選択順に`MAIN 1`、`MAIN 2`と番号を付ける
4. `適用`を押す
5. 既存メイン配置がある場合、変更後の配置に変換可能か検証する
6. 解決不能なセットは`要再登録`にする

メイン画面内でEditor／Browserなどの役割指定は行わない。

### メイン画面を空にする

この操作はアプリを閉じず、メイン画面上の対象ウィンドウを移動または最小化する。

処理順：

1. メイン画面と交差するトップレベルウィンドウを列挙
2. RepoDeck自身、システムUI、固定除外ウィンドウを除外
3. 登録済みワークセットのウィンドウを各セットの退避先へ移動
4. 未登録ウィンドウを検出
5. 未登録ウィンドウ方針を適用
6. 操作前配置をUndoスナップショットとして保存
7. 結果を表示

未登録ウィンドウ方針：

- `Ask`：一覧を表示し、最小化するものをチェックさせる。既定
- `Minimize`：最小化する
- `Leave`：そのまま残す

絶対に行わないこと：

- ウィンドウを閉じる
- 画面外へ移動する
- 未登録ウィンドウを別セットへ自動所属させる

処理後に次を表示する。

```text
6個のウィンドウを退避し、2個を最小化しました。［元に戻す］
```

Undoスナップショットは次の破壊的でない操作まで保持する。

## 3.6 ワークセット登録

推奨操作：

1. `メイン画面を空にする`
2. 登録したいウィンドウをメイン画面へ手動配置
3. `現在の配置をセットとして登録`
4. リポジトリフォルダーを選択
5. 候補ウィンドウを確認
6. 名前、色、ホットキー、退避方針を設定
7. 保存

### リポジトリ選択

- フォルダーピッカーで選択
- 選択フォルダーから親方向へ`.git`を探索
- Gitルートが見つかった場合はそこを正規パスとして使用
- Gitルートが見つからなくても登録可能。その場合は通常フォルダーセットとして扱う
- 同一正規パスの重複登録は禁止
- 既存登録がある場合は編集画面へ誘導

### ウィンドウ候補検出

候補条件：

- 可視トップレベルウィンドウ
- ウィンドウ中心点がメイン画面のいずれかに存在
- 最小サイズが80×60物理ピクセル以上
- 除外対象でない

候補一覧に表示する情報：

- アプリアイコン
- プロセス名
- 現在のタイトル
- 実行ファイルパス
- ウィンドウクラス
- 現在のモニター
- チェックボックス

既定では候補をすべて選択する。ただしRepoDeck自身とシステムUIは一覧にも出さない。

### 登録時に保存する情報

各ウィンドウについて次を保存する。

- 実行ファイルパス
- プロセス名
- ウィンドウクラス
- 登録時タイトル
- ユーザー指定の`title_contains`。任意
- ユーザー指定の`title_regex`。任意
- 現在のHWND。ランタイムキャッシュのみ
- メイン画面番号
- メイン画面内の正規化座標
- 登録時の物理座標
- 通常／最大化／最小化状態
- Z順序の相対順位

役割名は保存しない。

### メイン配置の上書き

現在セットがメイン画面にある状態で`現在の配置を保存`を押すと、全所属ウィンドウのメイン配置を上書きする。

- 見つからないウィンドウは上書きしない
- 新しい未登録ウィンドウがメインにある場合は追加候補として確認する
- 既存ウィンドウをセットから外す操作は確認を必要とする

## 3.7 退避先設定

### 自動退避

ワークセット作成時の既定値。

- 非メインモニターの退避可能領域を使用
- 各モニターの最大自動分割数は1、2、4から選択
- 固定枠を除外
- ワークセットに安定した自動枠を割り当てる
- 枠不足時は最小化

### 固定退避

1. レイアウトスタジオで非メインモニターを選択
2. 1、2、4分割プリセットを適用
3. 枠を選択
4. `固定割当`からワークセットを選択
5. 保存

MVPでは固定枠をグリッドセル単位に制限する。自由描画はMVP外とする。これにより重複判定と自動割当を決定的にする。

固定枠の規則：

- 1セットに固定枠は最大1つ
- 1枠に固定できるセットは最大1つ
- セットがメイン表示中は固定枠が空く
- セットが非アクティブになったら自分の固定枠へ戻る
- 固定枠のあるモニターが消えたら最小化
- モニター復帰時に固定枠へ自動復帰

## 3.8 ワークセット切替

切替要求の入口を`SwitchCoordinator`へ一本化する。ホットキー、UI、トレイから直接Win32移動処理を呼ばない。

処理：

1. 多重切替を排他ロックで防止
2. 対象が現在セットなら、フォーカスだけ行って成功終了
3. 現在セットと対象セットのウィンドウを再解決
4. 切替前の全対象配置をジャーナルへ保存
5. 現在セットを退避
6. 対象セットをメイン配置へ復元
7. Z順序を復元
8. 最上位ウィンドウへベストエフォートでフォーカス
9. 現在セットIDを更新
10. `ready`確認済み処理
11. ジャーナルを`committed`にする
12. UIへ結果通知

失敗時：

1. 失敗理由を記録
2. 切替前ジャーナルからロールバック
3. ロールバックできないウィンドウを列挙
4. クイックスイッチャーに復旧ボタンを表示
5. 現在セットIDを成功した状態にのみ更新

---

## 4. 配置アルゴリズム

## 4.1 座標系

- 外部ウィンドウは物理ピクセルで扱う
- RepoDeckプロセスはPer-Monitor DPI Awareness V2とする
- Slint UI内部はSlintの論理座標へ任せる
- 永続化する配置はモニター内正規化座標を正本とする
- 物理座標は診断と同一構成復元の補助として保存する

正規化矩形：

```text
x = (window.left - monitor.work_area.left) / monitor.work_area.width
y = (window.top  - monitor.work_area.top)  / monitor.work_area.height
w = window.width  / monitor.work_area.width
h = window.height / monitor.work_area.height
```

値は`f64`で保存し、0.0～1.0へ機械的に丸めない。わずかな画面外配置を診断できるよう、保存時は-0.1～1.1を許可し、復元時に作業領域へ収める。

## 4.2 メイン配置復元

各ウィンドウは`main_monitor_index`を持つ。

1. 現在のメインモニター配列から該当インデックスを取得
2. 見つからない場合はプライマリメイン画面へフォールバック
3. 正規化矩形を現在の作業領域へ変換
4. 最小幅160px、最小高さ90pxを保証
5. 作業領域から完全に外れないようクランプ
6. 保存状態が最大化なら、通常配置を設定してから最大化
7. 保存状態が最小化でも、メイン表示時は通常または最大化へ復元する

## 4.3 退避枠への縮小配置

ワークセット内の全ウィンドウのメイン配置矩形の外接矩形を`source_bounds`とする。

退避枠から8物理pxの内側余白を引いた矩形を`target_bounds`とする。

各ウィンドウについて、`source_bounds`から`target_bounds`へのアフィン変換を行う。

```text
scale_x = target.width  / source.width
scale_y = target.height / source.height

target_x = target.left + (window.left - source.left) * scale_x
target_y = target.top  + (window.top  - source.top)  * scale_y
target_w = window.width  * scale_x
target_h = window.height * scale_y
```

縦横比を保つ必要はない。メインで左右2画面に配置された2ウィンドウを1つの小さな枠へ左右配置するため、XとYは独立に縮小する。

最小サイズを下回る場合：

- 各ウィンドウの最小表示サイズを120×68pxとする
- すべてを枠内に収められない場合、ワークセット全体を最小化する
- 一部だけ物理退避し、一部だけ最小化する状態は作らない

退避時は最大化を解除し、通常状態へしたうえで縮小配置する。

## 4.4 自動退避枠生成

各非メインモニターに`auto_split`を持つ。

- 1：1×1
- 2：2×1。左右分割
- 4：2×2

自動割当アルゴリズム：

1. 接続中の非メインモニターをWindows上の左上から右下順に並べる
2. 各モニターのグリッドセルを列挙
3. 固定枠として予約されたセルを除外
4. 非アクティブかつ固定枠のないセットを`sort_order`順に並べる
5. 前回割当セルが利用可能なら維持
6. 未割当セットを先頭空きセルからFirst Fitで割り当て
7. 空きセルが尽きたら残りを最小化

自動割当は同一画面構成と同一セット順で決定的になること。

## 4.5 一括ウィンドウ移動

次を使用する。

- `BeginDeferWindowPos`
- `DeferWindowPos`
- `EndDeferWindowPos`

一括移動前に各ウィンドウの最大化を解除する。`EndDeferWindowPos`失敗時は個別`SetWindowPos`へフォールバックし、それでも失敗した場合は切替失敗とする。

フラグ方針：

- アクティブ化しない退避処理：`SWP_NOACTIVATE`
- 通常配置：`SWP_NOOWNERZORDER`
- 最終フォーカス対象以外はZ順を不要に変更しない
- `AttachThreadInput`を通常経路で使用しない
- フォーカスは`SetForegroundWindow`のベストエフォートとする

## 4.6 モニター消失

モニター構成変更を検出したら次を行う。

1. 切替中なら完了またはロールバックまで待つ
2. モニター一覧を再取得
3. メインモニターが消えた場合、残存プライマリを暫定メインにする
4. 固定退避先を失ったセットを最小化
5. 画面外になった管理対象ウィンドウを残存メインへ回収
6. UIに`画面構成が変わりました`警告を表示
7. 元のモニター復帰時は保存識別子で再照合

---

## 5. ウィンドウ検出・識別仕様

## 5.1 列挙API

Windows API：

- `EnumWindows`
- `IsWindowVisible`
- `GetAncestor(..., GA_ROOT)`
- `GetWindowLongPtrW`
- `GetWindowThreadProcessId`
- `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`
- `QueryFullProcessImageNameW`
- `GetWindowTextLengthW`
- `GetWindowTextW`
- `GetClassNameW`
- `GetWindowRect`
- `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)`

## 5.2 除外条件

次を管理候補から除外する。

- RepoDeck自身のプロセス
- 不可視ウィンドウ
- `GetAncestor(hwnd, GA_ROOT) != hwnd`
- `WS_EX_TOOLWINDOW`かつ`WS_EX_APPWINDOW`でないもの
- タイトルなしで、かつ既知の管理対象でないもの
- `Shell_TrayWnd`
- `Progman`
- `WorkerW`
- `ApplicationFrameWindow`のうちサイズ0または不可視のもの
- 幅80px未満または高さ60px未満
- cloaked状態のウィンドウ。ただし既存登録の再解決時は候補として診断表示可能

## 5.3 永続識別子

HWNDは永続識別子にしない。登録情報は次で構成する。

```rust
struct WindowMatcher {
    executable_path: PathBuf,
    process_name: String,
    window_class: String,
    registered_title: String,
    title_contains: Option<String>,
    title_regex: Option<String>,
}
```

## 5.4 ランタイムキャッシュ

```rust
struct BoundWindow {
    hwnd: isize,
    process_id: u32,
    matched_at_utc: String,
    confidence: MatchConfidence,
}
```

`matched_at_utc`はRFC 3339 UTC文字列とする。`BoundWindow`はメモリー上だけで使い、`runtime.json`へHWNDを永続化しない。

HWND利用前に必ず次を再検証する。

- `IsWindow(hwnd)`
- 現在のプロセスID
- 現在の実行ファイルパス
- 現在のクラス名

## 5.5 再照合スコア

候補スコア：

| 条件 | 点数 |
|---|---:|
| 実行ファイルパス完全一致 | +50 |
| ウィンドウクラス完全一致 | +25 |
| `title_contains`一致 | +30 |
| `title_regex`一致 | +30 |
| 登録時タイトル完全一致 | +20 |
| 登録時タイトルとの正規化類似度80%以上 | +10 |
| 候補が別ワークセットへ既にバインド済み | 除外 |

判定：

- 75点以上かつ次点との差20点以上：自動再バインド
- 75点以上だが次点との差20点未満：曖昧。ユーザー確認
- 75点未満：未解決

タイトル正規化：

- 前後空白除去
- 連続空白を1つへ
- VS Codeの変更マーク`●`等を除去
- 大文字小文字を無視
- 動的な末尾`- Visual Studio Code`を比較時に分離

正規表現が不正な場合は設定保存を拒否する。

## 5.6 誤移動防止

- 曖昧候補を自動選択しない
- 同一候補を複数登録へ割り当てない
- 実行ファイルパス取得に失敗した候補を自動再バインドしない
- 管理者権限差で操作できない場合は理由を表示する
- ユーザーが`このウィンドウを再登録`で明示選択できるようにする

---

## 6. AIエージェント状態連携

## 6.1 状態

```rust
enum AgentState {
    Idle,
    Running,
    NeedsInput,
    Ready,
    Blocked,
    Unknown,
}
```

表示色：

| 状態 | 色 | 意味 |
|---|---|---|
| `Idle` | 灰 | 待機または確認済み |
| `Running` | 青 | エージェント処理中 |
| `NeedsInput` | 黄 | 承認、回答、判断待ち |
| `Ready` | 緑 | 完了したが未確認 |
| `Blocked` | 赤 | エラーまたは中断 |
| `Unknown` | 白 | 状態不明 |

## 6.2 複数実行の集約

1リポジトリ内で複数セッション／ターンが動くことを許可する。

集約優先順位：

1. 1件でも`NeedsInput`なら`NeedsInput`
2. それ以外で1件でも`Blocked`なら`Blocked`
3. それ以外で1件でも`Running`なら`Running`
4. それ以外で未確認完了が1件でもあれば`Ready`
5. 全件確認済みなら`Idle`
6. 情報がなければ`Unknown`

## 6.3 Codexフック対応

対象：

- Codex CLI
- Codex VS Code拡張

Codex CLIとIDE拡張は同じ設定レイヤーを共有するため、MVPではCodex公式のライフサイクルフックを唯一の連携経路とする。RepoDeckがCodex App Serverを起動したり、既存のCodexセッションへ接続したりはしない。

イベント対応：

| Codexイベント | RepoDeckイベント |
|---|---|
| `UserPromptSubmit` | `run_started` |
| `PermissionRequest` | `needs_input` |
| `PostToolUse` | `run_resumed`。同一turnが入力待ちだった場合のみ |
| `Stop` | `run_completed` |
| フック転送エラー | Codexへ影響させずログのみ |

CodexフックのJSONから使用するフィールド：

- `session_id`
- `turn_id`
- `cwd`
- `hook_event_name`
- `model`
- `permission_mode`

`transcript_path`は保存・解析しない。

実装基準は2026-07-20時点のCodex公式Hooks仕様とする。実装開始時に <https://developers.openai.com/codex/hooks> を再確認し、イベント名または入力スキーマが変わっていた場合は`ipc/protocol.rs`のCodex入力アダプターだけを更新する。RepoDeck内部の正規化イベントschema v1は変更しない。

## 6.4 repodeck-hook.exe

Codexから標準入力で受け取ったJSONを、RepoDeck本体の名前付きパイプへ転送する。

要件：

- コンソールへ通常出力しない
- JSONが不正でも終了コード0
- RepoDeck未起動でも200ms以内に終了コード0
- プロンプト本文を保存しない
- 名前付きパイプ名は`\\.\pipe\RepoDeck.AgentEvents.v1`
- 最大入力1MiB
- 送信前に必要フィールドだけへ正規化
- `cwd`を正規化して送信

転送JSON：

```json
{
  "schema_version": 1,
  "source": "codex",
  "event": "run_completed",
  "session_id": "...",
  "turn_id": "...",
  "cwd": "D:\\repos\\guardrails-kit",
  "model": "...",
  "occurred_at": "2026-07-20T12:34:56.000Z"
}
```

## 6.5 フック設定

RepoDeck設定画面に`Codex連携`ページを設ける。

MVP操作：

1. インストール先の`repodeck-hook.exe`存在確認
2. 必要な`hooks.json`断片を生成
3. クリップボードへコピー
4. Codex設定フォルダーを開く
5. ユーザーが反映後、Codex CLIの`/hooks`で4フックをレビューして信頼する
6. Codexを再起動または設定再読込する
7. RepoDeckからテストイベントを送信

MVPでは既存の`hooks.json`を無断編集しない。自動インストールは将来機能とする。

生成例：

```json
{
  "hooks": {
    "UserPromptSubmit": [{
      "hooks": [{
        "type": "command",
        "commandWindows": "\"C:\\Program Files\\RepoDeck\\repodeck-hook.exe\"",
        "timeout": 2
      }]
    }],
    "PermissionRequest": [{
      "hooks": [{
        "type": "command",
        "commandWindows": "\"C:\\Program Files\\RepoDeck\\repodeck-hook.exe\"",
        "timeout": 2
      }]
    }],
    "PostToolUse": [{
      "hooks": [{
        "type": "command",
        "commandWindows": "\"C:\\Program Files\\RepoDeck\\repodeck-hook.exe\"",
        "timeout": 2
      }]
    }],
    "Stop": [{
      "hooks": [{
        "type": "command",
        "commandWindows": "\"C:\\Program Files\\RepoDeck\\repodeck-hook.exe\"",
        "timeout": 2
      }]
    }]
  }
}
```

実際の出力は現在のCodexスキーマに合わせ、インストール先を正しくエスケープする。

生成後の検証条件：

- JSONとして再読込できる
- `UserPromptSubmit`、`PermissionRequest`、`PostToolUse`、`Stop`が各1グループある
- 各グループの`hooks`が1件で、`type`が`command`
- `commandWindows`が絶対パスを引用符で囲む
- `timeout`を2秒に設定し、RepoDeck停止中でもCodexを長時間待たせない
- ユーザーが`/hooks`で信頼するまではイベントが来ないことを設定画面に表示する

## 6.6 リポジトリ対応付け

イベントの`cwd`から親方向へ`.git`を探索し、正規化したGitルートを求める。

照合順：

1. ワークセットの`repository_path`完全一致
2. `cwd`がワークセットパス配下
3. 見つからなければ`unmatched agent event`として30分保持

一致しないイベントを適当なセットへ割り当てない。

## 6.7 通知

- `NeedsInput`遷移時：トレイアイコンを黄へ変え、カードに黄バッジを表示
- `Ready`遷移時：トレイアイコンを緑へ変え、カードに緑バッジを表示
- `Running`遷移時：トレイアイコンを青へ変え、カードに青バッジを表示
- 複数セットに状態がある場合、トレイ色は`NeedsInput > Blocked > Ready > Running > Idle`の優先順位で決める
- トレイアイコンを左クリックすると、最優先状態のセットを選択したクイックスイッチャーを開く
- 自動切替はしない

MVPの通知はクイックスイッチャーのバッジとトレイアイコン変化で実装する。Windowsトースト通知はv0.2対象とし、MVPコードには未完成の通知経路を入れない。

---

## 7. データモデル

## 7.1 設定保存場所

```text
%LOCALAPPDATA%\RepoDeck\
├─ config.json
├─ runtime.json
├─ switch-journal.json
├─ config.backup.json
└─ logs\
   ├─ repodeck.YYYY-MM-DD.log
   └─ ...
```

`LOCALAPPDATA`が取得できない場合は起動を中止し、明確なエラーを表示する。カレントディレクトリへ勝手に保存しない。

## 7.2 Rust型

次の型をdomain層の正本とする。Win32固有型はこの層へ持ち込まない。シリアライズされるenumは`snake_case`、構造体フィールドはRust名のまま保存する。

```rust
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppConfig {
    schema_version: u32,
    app_version: String,
    settings: UserSettings,
    monitors: Vec<SavedMonitor>,
    main_monitor_ids: Vec<String>,
    worksets: Vec<Workset>,
    fixed_slots: Vec<FixedParkingSlot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UserSettings {
    quick_switcher_hotkey: HotkeyConfig,
    popup_location: PopupLocation,
    close_on_focus_loss: bool,
    close_after_switch: bool,
    unknown_window_policy: UnknownWindowPolicy,
    sort_mode: SortMode,
    notify_needs_input: bool,
    notify_ready: bool,
    start_with_windows: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Workset {
    id: Uuid,
    name: String,
    repository_path: PathBuf,
    repository_kind: RepositoryKind,
    color: String,
    sort_order: i32,
    direct_hotkey: Option<HotkeyConfig>,
    parking_policy: ParkingPolicy,
    windows: Vec<ManagedWindow>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedWindow {
    id: Uuid,
    matcher: WindowMatcher,
    main_placement: SavedPlacement,
    z_order: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedPlacement {
    monitor_id: String,
    main_monitor_index: usize,
    normalized_rect: NormalizedRect,
    physical_rect_at_capture: PixelRect,
    show_state: SavedShowState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ParkingPolicy {
    Auto,
    Fixed { slot_id: Uuid },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedMonitor {
    stable_id: String,
    device_name: String,
    device_path: Option<String>,
    friendly_name: Option<String>,
    bounds_px: PixelRect,
    work_area_px: PixelRect,
    dpi_x: u32,
    dpi_y: u32,
    auto_split: AutoSplit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FixedParkingSlot {
    id: Uuid,
    monitor_id: String,
    grid: AutoSplit,
    cell_index: usize,
    assigned_workset_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HotkeyConfig {
    modifiers: Vec<HotkeyModifier>,
    virtual_key: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HotkeyModifier {
    Alt,
    Control,
    Shift,
    Win,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PopupLocation {
    CursorMonitorCenter,
    MainMonitorCenter,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UnknownWindowPolicy {
    Ask,
    LeaveInPlace,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SortMode {
    Manual,
    Name,
    Recent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RepositoryKind {
    Git,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WindowMatcher {
    executable_path: PathBuf,
    process_name: String,
    window_class: String,
    registered_title: String,
    title_contains: Option<String>,
    title_regex: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct NormalizedRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct PixelRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SavedShowState {
    Normal,
    Maximized,
    Minimized,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AutoSplit {
    One,
    TwoColumns,
    FourGrid,
}
```

追加してよいのは、上記型を変更しない内部用newtype、ViewModel、Win32変換型だけとする。設定schemaへフィールドを追加する場合はschema versionを上げ、移行処理とfixtureを同じコミットで追加する。

> 実装メモ：Phase 3では`PixelRect`／`NormalizedRect`／`SavedShowState`／`SavedPlacement`を
> `src/domain/placement.rs`に、`AutoSplit`／`SavedMonitor`を`src/domain/monitor.rs`に、
> `Workset`／`ManagedWindow`／`ParkingPolicy`／`WindowMatcher`／`FixedParkingSlot`／
> `RepositoryKind`を`src/domain/workset.rs`に、残りを`src/domain/config.rs`に配置した
> （§9.4のディレクトリ構成`domain/placement.rs`・`domain/monitor.rs`・`domain/workset.rs`・
> `domain/config.rs`に対応させたモジュール分割）。フィールド名・型はすべて上記定義のまま。

## 7.3 runtime.json

永続設定ではなく、再起動時に復元可能なランタイム状態を保存する。

```json
{
  "schema_version": 1,
  "current_workset_id": null,
  "auto_slot_assignments": {},
  "agent_runs": [],
  "last_seen_monitor_fingerprint": "...",
  "last_clean_shutdown": true
}
```

### 保存タイミング

- 現在セット変更時
- 自動枠割当変更時
- Agent状態変更時。最大500msでデバウンス
- 正常終了時

## 7.4 switch-journal.json

切替開始前に書き、成功時に削除または`committed`化する。

```json
{
  "schema_version": 1,
  "transaction_id": "uuid",
  "status": "started",
  "from_workset_id": "uuid-or-null",
  "to_workset_id": "uuid",
  "created_at": "...",
  "windows": [
    {
      "managed_window_id": "uuid",
      "hwnd": 123456,
      "process_id": 1000,
      "before": {
        "rect": {"x": 0, "y": 0, "width": 1000, "height": 800},
        "show_state": "normal"
      }
    }
  ]
}
```

HWNDは当該トランザクション内だけで使用し、再起動後はプロセスIDと現在属性を再検証する。

## 7.5 原子的保存

設定保存：

1. 現在ファイルを`config.backup.json`へコピー
2. `config.json.tmp`へ全量書込
3. `flush`と`sync_all`
4. JSONを再読込してデシリアライズ検証
5. Windows上で置換
6. 失敗時は元ファイルを保持

不完全なJSONを正本へ上書きしない。

## 7.6 スキーマ移行

- `schema_version`を必須にする
- 未知の新しいschemaは開かず、更新を促す
- 古いschemaは`migrations`モジュールで段階移行
- 移行前ファイルをバックアップ
- MVP初期schemaは1

---

## 8. 技術構成

## 8.1 採用技術

| 項目 | 採用 |
|---|---|
| 言語 | Rust 2024 Edition |
| MSRV | Rust 1.85 |
| UI | Slint 1.17.x |
| Windows API | `windows` crate 0.62.x |
| シリアライズ | `serde`、`serde_json` |
| ID | `uuid` |
| 正規表現 | `regex` |
| エラー | `thiserror`、アプリ境界のみ`anyhow`可 |
| ログ | `tracing`、`tracing-subscriber`、`tracing-appender` |
| 同期 | 標準同期プリミティブまたは`parking_lot` |
| ビルド | Cargo＋`build.rs` |
| UIテスト | 純粋ViewModelテスト＋Windows E2Eハーネス |

非採用：

- Tokio：MVPでは不要。名前付きパイプとホットキーは専用ブロッキングスレッドで実装
- Tauri：WebView不要
- WPF／C#：不採用
- Electron：不採用
- 仮想デスクトップAPI：不採用

## 8.2 Cargo.toml方針

バージョンは次の互換範囲を開始点とし、最初の成功ビルドで`Cargo.lock`をコミットする。

```toml
[package]
name = "repodeck"
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "MIT"
build = "build.rs"

[dependencies]
slint = { version = "1.17", features = ["system-tray", "raw-window-handle-06"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
uuid = { version = "1", features = ["v4", "serde"] }
regex = "1"
thiserror = "2"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tracing-appender = "0.2"
parking_lot = "0.12"
raw-window-handle = "0.6"

[dependencies.windows]
version = "0.62"
features = [
  "Win32_Foundation",
  "Win32_Graphics_Dwm",
  "Win32_Graphics_Gdi",
  "Win32_Security",
  "Win32_Storage_FileSystem",
  "Win32_System_Diagnostics_ToolHelp",
  "Win32_System_Pipes",
  "Win32_System_Threading",
  "Win32_UI_HiDpi",
  "Win32_UI_Input_KeyboardAndMouse",
  "Win32_UI_Shell",
  "Win32_UI_WindowsAndMessaging"
]

[build-dependencies]
slint-build = "1.17"

[dev-dependencies]
tempfile = "3"
pretty_assertions = "1"
```

不足するWindows featureはコンパイルエラーに従い最小限追加する。`windows`の全feature有効化は禁止する。

> 実装メモ：実際にビルドして判明した差分（Phase 1〜3）は次の通り。
> - `slint`の`raw-window-handle-06` featureはSlint 1.17.1に存在しない（`system-tray`のみ有効化）。
> - `rust-version`はSlint 1.17.1自体が`1.92`を要求するため`1.85`ではなく`1.92`とした。
> - `windows` crateの`BOOL`は`windows::core::BOOL`から、`CreateMutexW`は`Win32_Security`
>   feature追加後に、`PROCESS_NAME_WIN32`は`Win32_Storage_FileSystem`ではなく
>   `Win32_System_Threading`から、`MONITORINFOF_PRIMARY`は`Win32_Graphics_Gdi`ではなく
>   `Win32_UI_WindowsAndMessaging`から、それぞれ解決した。
> - Windowsマニフェスト埋め込みは`slint-build`だけでなく`embed-manifest` crateを
>   build-dependenciesへ追加して実装した（§8.4のPerMonitorV2／asInvoker要件を
>   `embed_manifest::new_manifest()`の既定値がそのまま満たす）。
> - RFC 3339タイムスタンプ生成のため`time`（`formatting`・`parsing` feature）を
>   直接依存へ追加した（`tracing-appender`経由で既に推移的依存に含まれていたもの）。
> 実際の`Cargo.toml`が正本の最新状態であり、本ブロックはあくまで開始点である。

## 8.3 Slintライセンス

RepoDeck本体はMITで公開する。SlintはRoyalty-free Desktop Licenseを利用し、次を実施する。

- 設定の`About`画面に`AboutSlint`表示または同等の帰属表示
- READMEの依存技術欄にSlintを記載
- `THIRD_PARTY_NOTICES.md`にライセンスとURLを記載
- Slint単体を再配布しない

## 8.4 Windowsマニフェスト

実行ファイルへ次を含める。

- `PerMonitorV2` DPI awareness
- Windows 11対応OS宣言
- requestedExecutionLevelは`asInvoker`
- 管理者権限を要求しない
- UTF-8／長いパス対応に必要な設定

RepoDeckを常時管理者起動する設計は禁止する。高権限ウィンドウを操作できない場合は明示する。

---

## 9. アーキテクチャ

## 9.1 スレッドモデル

### UIスレッド

- Slintイベントループ
- UIモデル更新
- UIコールバック
- Win32操作は短時間のものだけ呼出可能
- 長い列挙、設定I/O、切替処理を直接実行しない

### Coordinatorスレッド

- 切替要求の直列処理
- ウィンドウ再解決
- 配置ジャーナル
- ロールバック
- モニター変更処理

### Hotkeyスレッド

- `RegisterHotKey`
- Win32メッセージループ
- ホットキーイベントをCoordinator／UIへ送信

### IPCスレッド

- 名前付きパイプ待受
- サイズ制限
- JSON検証
- Agent状態ストアへ転送

### Window Watcherスレッド

- 2秒間隔で登録ウィンドウの生存確認
- モニター構成変更の検出
- アイドル時に高頻度ポーリングしない

Slint UI更新は必ず`slint::invoke_from_event_loop()`経由で行う。

## 9.2 コンポーネント

```text
UI callbacks
    │
    ▼
AppController
    ├── SwitchCoordinator ── WindowingService ── Win32
    ├── WorksetService ───── ConfigStore
    ├── LayoutService ────── MonitorService
    ├── AgentStatusService ─ NamedPipeServer
    ├── HotkeyService ────── Win32 message loop
    └── RecoveryService ──── SwitchJournal
```

## 9.3 依存方向

- `ui`は`application`の公開コマンドだけを呼ぶ
- `application`は`domain`とtraitへ依存
- `windowing`はWin32実装を提供する
- `persistence`はdomain型を保存する
- `agent`はイベントをdomain状態へ変換する
- `domain`はSlintとWin32へ依存しない

## 9.4 ディレクトリ構成

```text
repodeck/
├─ Cargo.toml
├─ Cargo.lock
├─ build.rs
├─ LICENSE
├─ README.md
├─ THIRD_PARTY_NOTICES.md
├─ rustfmt.toml
├─ clippy.toml
├─ assets/
│  ├─ repodeck.ico
│  ├─ tray-idle.png
│  ├─ tray-running.png
│  └─ tray-needs-input.png
├─ ui/
│  ├─ app-window.slint
│  ├─ quick-switcher.slint
│  ├─ layout-studio.slint
│  ├─ workset-dialog.slint
│  ├─ settings.slint
│  ├─ codex-integration.slint
│  ├─ about.slint
│  ├─ components/
│  │  ├─ workset-card.slint
│  │  ├─ monitor-canvas.slint
│  │  ├─ parking-slot.slint
│  │  └─ status-badge.slint
│  └─ theme.slint
├─ src/
│  ├─ main.rs
│  ├─ app.rs
│  ├─ commands.rs
│  ├─ domain/
│  │  ├─ mod.rs
│  │  ├─ config.rs
│  │  ├─ workset.rs
│  │  ├─ placement.rs
│  │  ├─ monitor.rs
│  │  └─ agent.rs
│  ├─ application/
│  │  ├─ mod.rs
│  │  ├─ switch_coordinator.rs
│  │  ├─ workset_service.rs
│  │  ├─ layout_service.rs
│  │  ├─ agent_status_service.rs
│  │  └─ recovery_service.rs
│  ├─ windowing/
│  │  ├─ mod.rs
│  │  ├─ enumerate.rs
│  │  ├─ matcher.rs
│  │  ├─ placement.rs
│  │  ├─ monitor.rs
│  │  ├─ dpi.rs
│  │  ├─ focus.rs
│  │  └─ win32_error.rs
│  ├─ ui_bridge/
│  │  ├─ mod.rs
│  │  ├─ models.rs
│  │  └─ callbacks.rs
│  ├─ hotkey/
│  │  ├─ mod.rs
│  │  └─ win32_hotkey.rs
│  ├─ ipc/
│  │  ├─ mod.rs
│  │  ├─ named_pipe.rs
│  │  └─ protocol.rs
│  ├─ persistence/
│  │  ├─ mod.rs
│  │  ├─ config_store.rs
│  │  ├─ runtime_store.rs
│  │  ├─ journal_store.rs
│  │  └─ migrations.rs
│  ├─ diagnostics/
│  │  ├─ mod.rs
│  │  └─ logging.rs
│  └─ bin/
│     └─ repodeck-hook.rs
├─ tests/
│  ├─ geometry_tests.rs
│  ├─ allocator_tests.rs
│  ├─ matcher_tests.rs
│  ├─ config_tests.rs
│  ├─ agent_state_tests.rs
│  └─ windows_e2e.rs
├─ test-harness/
│  └─ src/main.rs
└─ .github/workflows/ci.yml
```

> 実装メモ：Phase 1〜3では`agent.rs`（`AgentState`はPhase 8で導入）、`application/`、
> `ui_bridge/`、`hotkey/`、`ipc/`、`commands.rs`、`assets/`、Slintの追加画面はまだ作成していない。
> `geometry_tests.rs`・`allocator_tests.rs`・`matcher_tests.rs`・`config_tests.rs`相当のテストは
> 現時点では対応する各モジュール内の`#[cfg(test)] mod tests`にインラインで実装しており
> （`domain::placement`・`domain::config`・`windowing::monitor`・`windowing::enumerate`・
> `persistence::*`）、`tests/windows_e2e.rs`のみ本構成どおり独立ファイルとして存在する。
> Phase 4以降でファイルが増えるにつれ、`tests/`配下への切り出しを検討する。

---

## 10. エラーと復旧

## 10.1 エラー分類

```rust
struct WindowCandidateSummary {
    hwnd: isize,
    process_id: u32,
    executable_path: Option<PathBuf>,
    window_class: String,
    title: String,
    score: i32,
}

enum AppError {
    Config(ConfigError),
    WindowEnumeration(WindowError),
    WindowNotFound { managed_window_id: Uuid },
    WindowAmbiguous {
        managed_window_id: Uuid,
        candidates: Vec<WindowCandidateSummary>,
    },
    WindowAccessDenied { hwnd: isize },
    MonitorUnavailable { monitor_id: String },
    HotkeyConflict { hotkey: HotkeyConfig },
    SwitchFailed { transaction_id: Uuid, cause: String },
    Ipc(IpcError),
}
```

ユーザー向け文言とログ詳細を分ける。Win32エラーコードはログへ残すが、UIでは操作方法を示す。

## 10.2 全ウィンドウ回収

トレイと設定画面から実行できる。

処理：

1. 全登録ウィンドウを再解決
2. 接続中のメイン画面を取得
3. 全ウィンドウを通常状態へ戻す
4. メイン画面の作業領域へタイル配置
5. 画面外ウィンドウを必ず回収
6. 現在セットを`None`へ
7. 結果一覧を表示

回収配置は登録メイン配置ではなく、安全なグリッド配置を使う。対象数に応じ1×Nまたは2列で配置する。

## 10.3 起動時クラッシュ復旧

`switch-journal.json`が`started`のまま残っている場合：

1. UI表示前に内容を検証
2. 対象HWNDを再検証
3. `前回の切替が中断されました`ダイアログ
4. `元の配置へ戻す`、`全てメインへ回収`、`何もしない`を提示
5. 選択後ジャーナルを解決済みにする

## 10.4 設定破損

1. `config.json`読込失敗
2. `config.backup.json`読込を試す
3. 成功した場合は復旧したことを通知
4. 両方失敗した場合、壊れたファイルを日時付きで退避
5. 初回セットアップを起動

データを無言で破棄しない。

---

## 11. セキュリティ・プライバシー

- 完全ローカル動作
- ネットワーク通信なし
- テレメトリーなし
- プロンプト本文を保存しない
- Codex transcriptを読まない
- リポジトリ内容を読まない。`.git`存在確認とパス解決のみ
- APIキーを扱わない
- 名前付きパイプは現在ユーザーだけが接続できるACLを設定
- IPC入力上限1MiB
- JSONの未知フィールドは無視可能だが、必須フィールド欠落は破棄
- ログにはパスが含まれ得るため、ログ送付前に確認する説明を出す
- 管理者権限を要求しない

---

## 12. 性能要件

- ホットキーからUI表示開始：100ms以内目標
- 切替要求から移動開始：150ms以内目標
- 5ウィンドウの通常切替完了：500ms以内目標
- アイドルCPU：平均0.5%未満
- アイドルメモリ：150MB未満
- ウィンドウ監視間隔：2秒以上
- UIスレッドを50ms以上ブロックしない
- IPCイベント反映：受信から500ms以内
- 設定保存：1秒以内

性能目標未達を理由に安全チェックを削らない。

---

## 13. 実装Phase

以下を順に実施する。各Phaseの成果物、実装手順、テスト、完了条件を満たすこと。

## Phase 1：プロジェクト基盤

### 実装

1. CargoプロジェクトをRust 2024で作成
2. 上記ディレクトリを作成
3. Slint `build.rs`を設定
4. 最小ウィンドウを表示
5. Windowsマニフェストを埋め込む
6. ロギング初期化
7. `%LOCALAPPDATA%\RepoDeck`を作成
8. 単一インスタンスmutexを実装
9. MIT、Third Party Noticesを追加

### 検証

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
```

### 完了条件

- Windows 11で起動する
- DPI 100%、150%でUIが表示される
- 二重起動時に2プロセス常駐しない
- release buildが成功する
- warningが0

## Phase 2：Win32ウィンドウ・モニター基盤

### 実装

1. モニター列挙
2. 物理／作業領域、DPI、デバイス名取得
3. トップレベルウィンドウ列挙
4. 除外フィルター
5. 実行ファイルパス、クラス、タイトル取得
6. ウィンドウ配置取得
7. ウィンドウ通常化、移動、最大化、最小化
8. 一括移動
9. geometry純粋関数をWin32から分離

### テスト

- 正規化→物理座標→正規化の往復誤差1px以内
- 負の仮想スクリーン座標
- 縦置きモニター
- 異なるDPI
- 最大化ウィンドウの通常配置取得
- 除外対象を列挙しない

### 完了条件

- VS CodeとBraveを指定矩形へ100往復させ、累積ずれ2px以内
- タスクバー領域へ重ならない
- 管理者ウィンドウ操作失敗を検出できる
- DisplayLinkモニターを列挙できる

## Phase 3：設定とドメインモデル

### 実装

1. domain型
2. config schema v1
3. atomic save
4. backup restore
5. runtime store
6. switch journal
7. config validation
8. test fixtures

### バリデーション

- ワークセット名は1～80文字
- リポジトリパスは絶対パス
- ID重複禁止
- 1ウィンドウの多重所属禁止
- 固定枠重複禁止
- 存在しない固定枠参照禁止
- main monitorは1件以上
- 正規表現はコンパイル可能
- hotkey modifierは1つ以上

### 完了条件

- 保存後再起動で完全一致
- 保存途中停止でも元configが読める
- 壊れたconfigからbackup復旧できる
- 未知schemaを安全に拒否する

## Phase 4：レイアウトスタジオ

### 実装

1. モニター縮小図
2. メイン画面複数選択
3. 選択順表示
4. 自動分割1／2／4
5. 固定枠割当
6. メインを空にする
7. 未登録ウィンドウ確認ダイアログ
8. Undo
9. 設定保存

### UI検証

- 2、4、8画面相当のfixtureを表示
- 負座標モニターが図上で正しい位置
- メインと退避枠の重複を保存できない
- 固定枠重複を保存できない
- UIを別画面から操作できる

### 完了条件

- コード編集なしでメイン画面と退避枠を設定できる
- `メインを空にする`後にUndoできる
- アプリを一つも閉じない

## Phase 5：ワークセット登録・照合

### 実装

1. フォルダーピッカー
2. Gitルート検出
3. メイン上候補ウィンドウ検出
4. チェックリスト
5. matcher保存
6. main placement保存
7. matcher scoring
8. 曖昧候補確認UI
9. 再バインドUI
10. 配置上書き保存

### テスト

- 同じVS Codeプロセスの複数ウィンドウ
- Brave複数ウィンドウ
- タイトル変化
- HWND再生成
- 既に他セットへ所属したウィンドウ
- 不正regex

### 完了条件

- VS Code＋Brave＋Terminalのセットを3つ登録可能
- 再起動後、明確な候補は自動再バインド
- 曖昧候補は勝手に移動しない
- 役割指定UIが存在しない

## Phase 6：退避割当・切替Coordinator

### 実装

1. 自動枠生成
2. First Fit＋前回割当維持
3. 固定枠優先
4. 枠内縮小配置
5. 枠不足時のセット単位最小化
6. SwitchCoordinator
7. 一括配置
8. ジャーナル
9. ロールバック
10. 現在セット管理
11. 全ウィンドウ回収

### テスト

- 2画面で退避枠なし→最小化
- 4画面で複数自動枠
- 8画面で固定＋自動混在
- 固定先モニター切断
- 切替連打
- 途中でウィンドウ終了
- `EndDeferWindowPos`失敗fixture

### 完了条件

- 3セットを100回切替えて画面外ウィンドウ0
- 固定セットが常に指定枠へ戻る
- 自動割当が同条件で安定
- 失敗注入時に切替前へロールバック
- 全回収が必ず利用可能

## Phase 7：タスクトレイ・ホットキー・クイックスイッチャー

### 実装

1. Slint SystemTrayIcon
2. 右クリックメニュー
3. Win32 RegisterHotKeyスレッド
4. ホットキー設定UI
5. クイックスイッチャー
6. 同一ホットキーtoggle
7. Esc／外クリックclose
8. カーソル画面中央表示
9. キーボード操作
10. セット選択後切替

### 完了条件

- GUI非表示でもプロセス継続
- ホットキーで100回表示／非表示できる
- ホットキー衝突を検出
- タスクバーとAlt+Tabを不必要に占有しない
- マウス、数字キー、矢印＋Enterで切替可能

## Phase 8：Codex連携

### 実装

1. 名前付きパイプサーバー
2. ユーザー限定ACL
3. `repodeck-hook.exe`
4. イベントprotocol v1
5. repo path mapping
6. agent run store
7. aggregate state
8. クイックスイッチャー状態表示
9. hook snippet generator
10. test event button
11. ready確認処理

### テスト

- 正常イベント
- 1MiB超過
- 不正JSON
- 必須フィールド欠落
- RepoDeck未起動時hook
- 同一イベント重複
- 複数session／turn
- cwd不一致
- NeedsInput→PostToolUse→Running→Stop→Ready→表示→Idle

### 完了条件

- Codex実行中にカードが青
- 承認待ちで黄
- 完了で緑
- 該当セット表示後に確認済み
- フック障害がCodexの終了コードへ影響しない
- プロンプト本文を保存しない

## Phase 9：回復性・仕上げ

### 実装

1. モニター変更監視
2. 起動時ジャーナル復旧
3. config backup復旧
4. ログローテーション
5. About／Slint帰属
6. 起動時自動実行設定
7. portable ZIP作成
8. SHA-256
9. README日本語／英語
10. 30秒デモ手順

### 完了条件

- モニター切断後に画面外ウィンドウ0
- 異常終了後に復旧UI表示
- clean Windows環境でZIPから起動
- READMEだけで第三者がセット登録・切替・Codex連携できる

---

## 14. テスト戦略

## 14.1 単体テスト必須領域

- PixelRect演算
- 正規化座標変換
- 外接矩形
- 枠へのアフィン変換
- 自動分割
- First Fit割当
- 固定枠除外
- matcher score
- title normalization
- agent aggregate state
- repository path mapping
- config validation
- schema migration
- hotkey parse／serialize

## 14.2 Windows E2Eハーネス

`test-harness`は次のトップレベルウィンドウを生成する。

- 固定タイトルウィンドウ
- 1秒ごとにタイトルが変わるウィンドウ
- 最大化ウィンドウ
- 最小化ウィンドウ
- 同一クラス／同一exeの複数ウィンドウ
- 切替途中で終了するウィンドウ

E2EテストはハーネスのHWNDを取得し、実座標を検証する。

> 実装メモ：`test-harness`自体はまだ実装していない。Phase 2の完了条件にある実機E2E検証は、
> 暫定的に`tests/windows_e2e.rs`（`#[ignore]`、実Notepadウィンドウを起動して駆動）で
> 満たした。Windows 11のパッケージ版Notepadはプロセス間接性があるため
> （本ファイル冒頭「実装メモ・既知の齟齬」参照）、`test-harness`を実装すればこの問題を
> 回避しつつ、タイトルが変わるウィンドウや切替途中で終了するウィンドウなどより広い
> シナリオを決定的に検証できる。

## 14.3 手動テスト構成

| 構成 | 必須確認 |
|---|---|
| 2画面・同DPI | 非選択セット最小化、切替復元 |
| 2画面・異DPI | 配置ずれなし |
| 4画面 | 自動退避、固定枠 |
| 8画面＋DisplayLink | 列挙、切断、復帰 |
| 縦置き混在 | 正規化復元 |
| モニター負座標 | UI図と配置 |
| RDP接続／切断 | 構成変更と回収 |

## 14.4 実アプリテスト

- VS Code複数ウィンドウ
- Brave複数ウィンドウ
- Windows Terminal
- PowerShell 7
- エクスプローラー
- Excel
- PDFビューアー
- 管理者起動PowerShell。操作拒否確認

## 14.5 CI

`.github/workflows/ci.yml`：

- runner：`windows-latest`
- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-targets`
- `cargo build --release`
- release binaryをartifact化
- 同一ブランチの古い実行をcancelする`concurrency`設定
- READMEのみ変更時は重いrelease buildを省略可能

E2Eの実モニターテストはCIに依存せず、ローカル手動チェックリストも正本とする。

---

## 15. UI受け入れ仕様

### クイックスイッチャー

- 800×480を基準とする
- 最小幅560、最大幅900
- 1画面に最大9セット、超過時スクロール
- 現在セットを明確に表示
- 状態色だけに依存せず、アイコンと文字を併記
- フォントサイズ14px以上相当
- キーボードフォーカスが視認可能
- 表示アニメーションは150ms以下、無効化可能

### レイアウトスタジオ

- 1100×720基準
- モニター図は実座標比率を維持
- 物理モニター番号とWindowsデバイス名を両方表示
- MAIN、AUTO、FIXEDを文字と色で区別
- 保存前に警告一覧を表示
- 未保存変更がある状態で閉じる場合は確認

### ダーク／ライト

- OS設定追従を既定
- Light、Dark、Systemを設定可能
- 状態色は両テーマでWCAG AA相当の視認性を目標

---

## 16. ログ仕様

ログレベル：

- `ERROR`：切替失敗、設定保存失敗、IPC起動不能
- `WARN`：曖昧照合、モニター消失、ウィンドウ操作拒否
- `INFO`：起動、終了、セット登録、切替開始／成功、Agent状態遷移
- `DEBUG`：候補スコア、座標、Win32詳細

既定は`INFO`。ログには次を保存しない。

- Codexプロンプト
- Codex応答
- ファイル内容
- APIキー
- 環境変数全量

ログ保持：

- 日次ローテーション
- 7日
- 合計50MB上限

---

## 17. 配布仕様

### MVP

- `RepoDeck-v0.1.0-windows-x64.zip`
- `repodeck.exe`
- `repodeck-hook.exe`
- `README.txt`
- `THIRD_PARTY_NOTICES.md`
- `LICENSE`

### GitHub Release

- ZIP
- SHA-256
- 変更点
- 既知の制約
- 30秒GIF
- VirusTotalへ第三者が検証可能な固定ハッシュ

### v0.2以降

- 署名付きインストーラー
- WinGet登録
- 自動更新は別Phase。MVPに入れない

---

## 18. MVP受け入れ基準

次をすべて満たした場合のみMVP完成とする。

1. Rust＋Slintで構成され、C#／WPF依存がない
2. Windows仮想デスクトップを使用していない
3. メイン画面を1台以上選択できる
4. メイン画面内の役割指定が存在しない
5. メイン画面を空にでき、Undoできる
6. 現在配置からワークセットを登録できる
7. 同じアプリの複数ウィンドウを個別登録できる
8. 自動退避と固定退避を選べる
9. 固定退避枠を1／2／4分割セルから指定できる
10. 枠がなければセット単位で最小化する
11. 3セット以上をクイックスイッチャーから切替できる
12. ユーザー指定グローバルホットキーでUIをtoggleできる
13. トレイ常駐できる
14. 位置、サイズ、最大化状態を復元できる
15. 切替失敗時にロールバックできる
16. 全ウィンドウ回収機能がある
17. CodexのRunning、NeedsInput、Readyを表示できる
18. AI完了時に自動切替しない
19. 再起動後に設定を復元できる
20. モニター切断後に画面外ウィンドウを残さない
21. `cargo fmt --check`成功
22. `cargo clippy --all-targets --all-features -- -D warnings`成功
23. `cargo test --all-targets`成功
24. `cargo build --release`成功
25. READMEだけで第三者が利用開始できる

---

## 19. 実装完了時の最終確認コマンド

```powershell
rustc --version
cargo --version
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo build --workspace --release
```

生成物確認：

```powershell
Get-Item .\target\release\repodeck.exe
Get-Item .\target\release\repodeck-hook.exe
Get-FileHash .\target\release\repodeck.exe -Algorithm SHA256
Get-FileHash .\target\release\repodeck-hook.exe -Algorithm SHA256
```

手動スモークテスト：

1. RepoDeck起動
2. メイン画面選択
3. メインを空にする
4. VS Code＋Braveを配置
5. セットA登録
6. 別のVS Code＋BraveでセットB登録
7. 固定退避枠をセットAへ割当
8. ホットキーでクイックスイッチャー表示
9. 同じホットキーで非表示
10. セットA／Bを10回切替
11. Codexテストイベントで状態変化
12. モニター1台を切断
13. 全ウィンドウが残存画面内にあることを確認
14. RepoDeck再起動
15. 設定と状態が復元されることを確認

---

## 20. 実装者への最終指示

最初から全UIを作らず、Phase順に動く縦切りを完成させること。最初にWin32のウィンドウ移動、DPI、DisplayLink、Slintトレイ常駐を検証し、成立を確認してからUIを広げる。

MVPの中心は次の4点である。

1. ウィンドウ集合をリポジトリ単位で確実に記憶する
2. メイン画面と実物理退避枠の間を安全に切り替える
3. ホットキーで瞬時に呼び出せる
4. Codexの状態をセットごとに表示する

これ以外の機能は、上記4点の安定性を損なう場合は実装しない。
