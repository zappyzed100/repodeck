<!-- PLAN.md — RepoDeck の全体計画・アーキテクチャ・技術選定理由の正本 -->
# PLAN.md — RepoDeck 全体計画

## 目的

Windowsで複数リポジトリを並行開発する開発者向けの常駐GUIアプリ。VS Code・ブラウザ・ターミナルなどの
トップレベルウィンドウ群を「リポジトリ単位のワークセット」として記憶し、Codexなどのエージェント状態を
見ながら、ホットキー一発で選んだセットをメイン画面へ瞬時に呼び出す。日常操作を
「ホットキーを押す→ワークセットを選ぶ→メイン画面に復元される」の3操作へ短縮することが目的であり、
Windows仮想デスクトップやブラウザタブ管理、アプリの自動終了などは意図的にスコープ外とする。

## アーキテクチャ

```text
repodeck/
├─ ui/                      Slint UIファイル（クイックスイッチャー・レイアウトスタジオ・設定 等）
├─ src/
│  ├─ main.rs / lib.rs / app.rs   起動・単一インスタンス・配線
│  ├─ domain/               Win32/Slintに依存しない正本データ型（config・workset・placement・monitor）
│  ├─ application/          SwitchCoordinator・WorksetService・LayoutService 等のユースケース層
│  ├─ windowing/            Win32実装（列挙・照合・配置・DPI・一括移動）
│  ├─ hotkey/               RegisterHotKeyスレッド
│  ├─ ipc/                  Codexフック用の名前付きパイプサーバー
│  ├─ persistence/          config/runtime/journalの原子的保存・スキーマ移行
│  ├─ diagnostics/          ロギング
│  └─ bin/repodeck-hook.rs  Codexフックから状態を転送する小型コンソールアプリ
├─ tests/                   geometry・allocator・matcher・config・agent_state・windows_e2e
└─ test-harness/            E2E検証用の合成トップレベルウィンドウ生成ツール
```

依存方向は `ui → application → domain` の一方向とし、`domain`はWin32にもSlintにも依存しない。
`windowing`はWin32実装を提供し、`persistence`は`domain`型を保存するだけで判断ロジックを持たない。
禁止依存・不変条件・詳細なモジュール構成は `docs/plans/development-plan.md` §2, §9 を正本とする。

## 技術選定理由

- **Rust 2024 + `windows` crate 0.62**: Win32 APIを直接・安全に呼ぶため。C#/WPF、Electron、Tauriは
  いずれも不採用（ネイティブなウィンドウ移動・DPI制御が必須で、WebViewや.NETランタイムは不要）。
  `windows` crateのfeatureフラグはコンパイルエラーに従って必要なものだけを有効化する。
- **Slint 1.17**: 軽量なネイティブGUIツールキットで、`system-tray` featureが標準搭載。Royalty-free
  Desktop Licenseの下で利用し、`About`画面とREADME・`THIRD_PARTY_NOTICES.md`に帰属表示を行う。
- **serde / serde_json**: `config.json` / `runtime.json` / `switch-journal.json` の正本フォーマット。
  schema_versionを必須にし、原子的保存（tmp書込→検証→rename）で破損を防ぐ。
- **同期プリミティブのみ・Tokio不採用**: 名前付きパイプ（Codex連携）とホットキー受信は専用の
  ブロッキングスレッドで十分であり、非同期ランタイムを持ち込む理由がない。
- **Windows仮想デスクトップAPI不採用**: 製品方針として仮想デスクトップを使わず、物理モニター内の
  メイン画面／退避枠の切替のみでワークセットを実現する。

## 実装原則（安全性の優先順位）

判断が衝突した場合は次の順で優先する。

1. ウィンドウを失わないこと
2. 誤ったウィンドウを移動しないこと
3. 切替処理を途中状態で終わらせないこと
4. UIが応答し続けること
5. 状態表示の正確性
6. 切替速度
7. 見た目とアニメーション

対象アプリを閉じない・画面外に残さない・曖昧な照合は自動選択しない、といった実装者が常に守るべき
12項目と、システム全体で常に成立させる不変条件（現在セットは最大1個、1ウィンドウは最大1ワークセット
に属する 等）は `docs/plans/development-plan.md` §0, §2.7 を正本とする。

## 運用

- 各Phaseの終了時に `cargo fmt --check` / `cargo clippy --all-targets --all-features -- -D warnings` /
  `cargo test --all-targets` / `cargo build --release` を実行し、warning 0・failure 0 を維持する。
- `repodeck.exe` は単一インスタンスで常駐し、明示的な「RepoDeckを終了」でのみ終了する。
- 完全ローカル動作・ネットワーク通信なし・テレメトリーなし・管理者権限を要求しない。
- MVP配布物は `RepoDeck-v0.1.0-windows-x64.zip`（`repodeck.exe` / `repodeck-hook.exe` /
  `README.txt` / `LICENSE` / `THIRD_PARTY_NOTICES.md`）とSHA-256チェックサム。
- 最終確認コマンドと手動スモークテスト手順は `docs/plans/development-plan.md` §19 を正本とする。

## ロードマップ

実装はPhase順に縦切りで進め、各Phaseの完了条件を満たしてから次へ進む。詳細な実装手順・テスト・
完了条件は `docs/plans/development-plan.md` §13（実装Phase）を正本とする。

1. **Phase 1 — プロジェクト基盤**（完了）: Cargoプロジェクト、Slint最小ウィンドウ、ロギング、
   単一インスタンスmutex、Windowsマニフェスト、LICENSE / THIRD_PARTY_NOTICES。
2. **Phase 2 — Win32ウィンドウ・モニター基盤**（完了）: モニター列挙、トップレベルウィンドウ列挙と
   除外フィルター、配置取得・変更、`BeginDeferWindowPos`一括移動、geometry純粋関数の分離。
3. **Phase 3 — 設定とドメインモデル**（完了）: domain型、config schema v1、原子的保存とbackup復旧、
   runtime store、switch journal、バリデーション、テストfixture。
4. **Phase 4 — レイアウトスタジオ**（完了）: モニター縮小図、メイン画面選択、自動分割、
   メインを空にする＋Undo、設定保存。固定枠割当はワークセットが存在しないため表示のみで
   Phase 5待ち（詳細は`docs/plans/development-plan.md`冒頭の実装メモを参照）。
5. **Phase 5 — ワークセット登録・照合**（完了）: フォルダーピッカー、Gitルート検出、候補検出、
   matcher scoring、セット管理画面、再バインド（自動＋手動「このウィンドウを再登録」）。
   曖昧候補の対話的な選択UIは簡略化し、最高スコア候補への手動再登録のみを提供
   （詳細は`docs/plans/development-plan.md`冒頭の実装メモを参照）。
6. **Phase 6 — 退避割当・切替Coordinator**（完了）: 自動枠割当（First Fit＋前回割当維持）、
   固定枠優先、枠内縮小配置、枠不足時の最小化、`SwitchCoordinator`（12ステップ切替・
   ジャーナル・ロールバック）、全ウィンドウ回収。UI（クイックスイッチャー・ホットキー）は
   Phase 7待ちのため、バックエンドAPIとして実装（詳細は`docs/plans/development-plan.md`
   冒頭の実装メモを参照）。
7. **Phase 7 — タスクトレイ・ホットキー・クイックスイッチャー**（完了）: `RegisterHotKey`用の
   専用スレッド（衝突検出・自己修復・設定画面からの再設定）、クイックスイッチャー
   （検索・矢印/数字キー/Enter・タスクバー/Alt+Tab非表示・アウトフォーカスで閉じる）、
   トレイメニュー刷新、`SwitchCoordinator`への実配線。エージェント状態表示はPhase 8待ち
   （詳細は`docs/plans/development-plan.md`冒頭の実装メモを参照）。
8. **Phase 8 — Codex連携**（完了）: 名前付きパイプ（`\\.\pipe\RepoDeck.AgentEvents.v1`、
   所有者限定ACL）、`repodeck-hook.exe`（標準入力→正規化JSON→単発送信、常に終了コード0）、
   エージェント状態集約（`domain::agent`の6段階優先順位）、クイックスイッチャーの状態
   バッジ・トレイアイコン色連動、設定画面「Codex連携」セクション（hook検出・hooks.json
   スニペット生成＋コピー・設定フォルダーを開く・実プロセスによるテストイベント送信）。
   詳細は`docs/plans/development-plan.md`冒頭の実装メモを参照。
9. **Phase 9 — 回復性・仕上げ**（完了）: モニター構成変更の監視（`WM_DISPLAYCHANGE`検知
   →画面外ウィンドウのみ最小化）、起動時クラッシュ復旧（`switch-journal.json`残存時に
   3択ダイアログ）、ログ保持（7日／50MB上限）、起動時自動実行設定、設定画面「バージョン
   情報」セクション、配布一式（`.github/workflows/ci.yml`、`scripts/package.ps1`、
   `README.md`/`README.en.md`）。config backup復旧はPhase 3で既に実装済みと確認。
   詳細は`docs/plans/development-plan.md`冒頭の実装メモを参照。

MVP受け入れ基準25項目は `docs/plans/development-plan.md` §18 を正本とする。

## タスク（機械可読 — Phase進捗を正規表現で読める記法）

書式:
- `- [ ] タイトル` … 未着手。行末に `` `状態タグ` `` が無ければ `backlog` 扱い
- `- [x] タイトル` … 完了。行末にタグが無ければ `done` 扱い
- 状態を明示したい時だけ行末にタグを付ける: `` `next` `` / `` `in_progress` `` / `` `blocked` ``
- 各タスクの詳細な実装手順・完了条件は `docs/plans/development-plan.md` §13 の対応するPhaseを参照

現状タスク（このリポジトリの実装状況）:

- [x] Phase 1: プロジェクト基盤
- [x] Phase 2: Win32ウィンドウ・モニター基盤
- [x] Phase 3: 設定とドメインモデル
- [x] Phase 4: レイアウトスタジオ
- [x] Phase 5: ワークセット登録・照合
- [x] Phase 6: 退避割当・切替Coordinator
- [x] Phase 7: タスクトレイ・ホットキー・クイックスイッチャー
- [x] Phase 8: Codex連携
- [x] Phase 9: 回復性・仕上げ
