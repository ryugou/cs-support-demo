# Runbook: vegapunk community 対象に ManualSection / Concept を追加

cs-support の Concept/community 機能（Issue #8 Phase B）を効かせるための **vegapunk サーバ側**設定変更手順。cs-support のコードでは変えられない（vegapunk のグローバル config）。

適用日: 2026-07-22（目視確認で適用済み扱い。機能確認は #8 Phase B の Merge 実行時に行う）。

## 何をするか（1 行）

vegapunk サーバの `config.yml` の `community.target_node_types` を、**既定 8 型 + 新規 2 型の計 10 型**に書き換えて**再起動**する。

## 前提となる仕様（vegapunk コードで確認済み）

- 設定キー: `community.target_node_types`（YAML トップレベル `community:` 配下）。フォーマットは **YAML**
- **env で上書き不可**（`VEGAPUNK_*` の env 上書きは auth/server/embedding/llm/worker のみ。community 系は無い）。**必ず config.yml に書く**
- **書くと既定リストを丸ごと置換する**（serde default はキー不在時のみ有効）。既存 8 型を省くと会話ドメイン schema の community を壊す
- **ホットリロード無し**。config は起動時 1 回読むだけ。**変更後は vegapunk プロセス再起動が必須**
- 追加は**加算的で安全**。対象型を持たない schema の Merge には一切影響しない（両端が対象型でない辺はスキップ。エラー・warning なし）

## 手順

### 1. サーバが読む config.yml を特定
起動コマンドに `--config <path>` があればそのパス。無ければ既定 `~/.config/vegapunk/config.yml`（Linux）。GCP VM `punkrecord-egghead-server` 上で確認:

```
gcloud compute ssh punkrecord-egghead-server --project <vegapunk-project> --zone <zone> \
  --command 'ps -o pid,lstart,args -C vegapunk'
```

### 2. `community.target_node_types` を 10 型に設定
既存 `community:` ブロックがあれば `target_node_types` の行だけ置換。無ければブロックごと追加（他 community フィールドは既定のままでよい）:

```yaml
community:
  target_node_types:
    - Person
    - Decision
    - Rationale
    - Alternative
    - Specification
    - Topic
    - Project
    - Task
    - ManualSection   # 追加（cs-support）
    - Concept         # 追加（cs-support）
```

> `resolution` / `seed` / `max_levels` / `retain_generations` を既にカスタム設定している場合は残す（`target_node_types` の行だけ変える）。

### 3. vegapunk プロセスを再起動
（ホットリロード無しのため必須。）

### 4. 反映確認（機能確認は Phase B）
- **今すぐの目視確認**: config.yml に 10 型が揃い、プロセス `lstart` が編集時刻より後
- **機能確認（Phase B）**: cs-support の `ingest_alarmcom` が ManualSection/Concept を投入 → `Merge` RPC 実行後に
  - `GetStats(schema=<cs-support schema>).community_count` が 0 から増える
  - `GetGraphSnapshot` で `node_type=ManualSection`/`Concept` のノードが `community` id（非 null）を持つ
  - これらは VPC 内の Cloud Run job からのみ叩ける

## ロールバック

`target_node_types` を既定 8 型に戻して再起動するだけ。データ破壊は無い（次回 Merge で cs-support schema の community が作られなくなるのみ）。
