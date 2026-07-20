<!-- PLAN.md — Progress Proof の全体計画・アーキテクチャ・技術選定理由の正本 -->
# PLAN.md — Progress Proof 全体計画

## 目的

自分専用の開発活動ダッシュボード。複数の public / private GitHub リポジトリを横断集計し、
「今日進んだこと」と「次にやること」がトップ画面を開いて5秒以内に分かる状態を作る。
Jira のような高密度 UI にはしない。

## アーキテクチャ

```text
src/                       ブラウザUI（React + Vite）
  ↓ public/data/*.json のみを読む
public/data/               公開可能な生成済みJSON（today / projects / tasks / history / metadata）
  ↑
scripts/report/            公開JSON・日次レポート生成（DTO変換・Schema検証・原子的反映）
  ↑
scripts/analysis/          活動時間・成果・タスク・人日の分析（決定的。LLMはFake差し替え可能）
  ↑
scripts/normalize/         GitHub由来データの共通形式化（安定ソート・安定ID）
  ↑
scripts/collectors/        GitHub API・gh・リポジトリファイルからの収集
  ↓
data/raw/                  非公開の生データ（.gitignore）
data/normalized/           非公開の正規化済みデータ（.gitignore）
schemas/                   各層を接続する JSON Schema（データ契約の正本）
scripts/lib/               外部I/Oアダプター（GitHub・Gemini・fs・Clock・設定）と共通型・ロガー
```

依存方向は上記の一方向のみ。禁止依存と機械強制は AGENTS.md §5 と
`.guardrails/GUARDRAILS.md` 末尾「progress-proof 固有の決定事項」が正本。

## 技術選定理由

- **TypeScript + React + Vite**: UI とデータ生成 CLI を単一言語で書き、型を公開JSON契約の
  第1強制層にする。Vite は静的サイト出力（Cloudflare Pages）と preview が軽い。
- **JSON ファイル保存（MVP）**: 初期データ保存は JSON。日次スナップショットで十分な規模であり、
  スキーマは `schemas/` に固定してあるため、将来 Cloudflare D1 へ移行する場合も
  normalize 層以降の入れ替えで済む構造にする（MVP では D1 を導入しない）。
- **Gemini API**: 成果・タスク・人日・称号・コメントの分析。モデル名は設定値
  （`scripts/lib/config.ts`。初期値: Gemini 3.1 Flash Lite）。出力は提案であり正本ではない
  （evidence 必須・Schema 検証・grounding 検査——INV-03）。LLM 停止時・API 上限時でも
  GitHub の機械的集計部分は生成可能で、失敗は `analysisStatus: failed` として明示する。
- **Cloudflare Pages + Access**: 静的サイトを GitHub Actions 経由でデプロイし、本人以外の
  閲覧を Access で拒否する。ブラウザへはいかなるトークンも渡さない（INV-10）。
- **tsx**: Node 上で TS の CLI をビルドなしで実行する（データ生成パイプラインの実行系）。

## 評価の方針（スコア設計の原則）

- 行数やコミット数だけで成果を評価しない。
- lockfile・自動生成ファイル・整形だけの差分を過大評価しない。
- 活動が少ない日を否定的に評価しない。スコアは他人との比較に使用しない。
- GitHub イベントから算出した時間は「GitHub から推定した活動時間」、人日は「一般的な開発者が
  逐次作業した場合の概算」として扱い、実測値と区別する（INV-06）。

## 運用

- GitHub Actions の定期実行は 12:00 JST / 23:00 JST（cron は UTC 03:00 / 14:00）＋手動実行。
  夜の回は schedule の発火遅延が JST 日付境界をまたがないよう 0 時から余裕を取る。
- 日次集計のタイムゾーンは Asia/Tokyo（INV-05。Clock 注入・`PROGRESS_PROOF_NOW` で凍結可能）。
- 公開JSONは一時ディレクトリで全生成・Schema 検証・publish-safety 検査後に一括反映（INV-09）。

## ロードマップ

1. **MVP（本ブートストラップ）**: fixture 駆動で全公開JSONを決定的に生成し、Today 画面を表示。
   ガードレール（.guardrails/GUARDRAILS.md §11 Step 0〜10）を完備。
2. 本番収集（`generate`）を GitHub Actions の日次 workflow で運用開始。
3. 履歴の蓄積と history チャートの拡充。
4. 必要になった時点で Cloudflare D1 への保存層移行を検討（スキーマは schemas/ を正本に維持）。

タスク粒度の設計根拠は `docs/plans/` に置く（AGENTS.md §4 — feat⇔plan 対）。
機能計画・フェーズ・画面仕様の正本は `docs/plans/development-plan.md`（Progress Proof 開発計画）。

## タスク（機械可読 — 将来の収集ステップが Gemini を使わず正規表現で読む想定）

Gemini の呼び出しは1日1回のまま増やさない方針（docs/plans/development-plan.md §8）。
そのため各リポジトリの `PLAN.md` ルートにこの節と同じ記法でタスクを書けば、
決定的なパースだけで状態を取り出せる——LLM に解釈させない。

書式:
- `- [ ] タイトル` … 未完了。行末に `` `状態タグ` `` が無ければ `backlog` 扱い
- `- [x] タイトル` … 完了。行末にタグが無ければ `done` 扱い
- 状態を明示したい時だけ行末にタグを付ける: `` `next` `` / `` `in_progress` `` /
  `` `blocked` `` / `` `cancelled` ``（`done`/`backlog` はチェック状態で表せるため省略可）
- `unknown` はこの記法では書かない——収集側が行を解釈できなかった場合にのみ付与する
- タスクの id はこのリポジトリ名とタイトルから収集側が導出する（このファイルには書かない）

例（このリポジトリの現状タスク）:

- [x] History画面の推移グラフに欠損日を0分のバーとして表示する
- [ ] plan.md タスク記法の雛形を作る `in_progress`
- [ ] plan.md からタスクを自動収集する仕組みを実装する（`scripts/collectors/collect.ts` 拡張） `next`
- [ ] 各追跡リポジトリの PLAN.md に本記法でタスクを書く `backlog`
- [ ] 誤って推定されたタスクを却下できるUI（docs/plans/development-plan.md §19） `backlog`
