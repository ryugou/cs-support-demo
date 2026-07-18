# GCP Cloud Run デプロイ + マージ連動 CI/CD + データ登録運用 実装計画

**Goal:** 新 GCP プロジェクトで cs-support-mcp を Cloud Run 稼働させ、main マージで自動デプロイ、業務データ（rules/lexicon/NG）の PR マージで自動 ingest、手動トリガでマニュアル再クロール。実質 ¥0/月。

**確定済みトポロジ（recon 実測 2026-07-18）:**
- vegapunk gRPC backend = プロジェクト `punkrecord-egghead` / VPC `punkrecord-egghead-vpc`(10.10.0.0/24) / `10.10.0.2:6840`
- 前例: `gen-lang-client-0184763777` の default VPC が peering 済み・firewall `punkrecord-egghead-allow-internal-grpc`(tcp:6840,6841) に source 追加で開通
- 実行者権限: 課金 SIVIRA(018A13-74AFE5-C7BB32, open) / punkrecord-egghead に roles/owner

**Architecture:**
```text
GitHub (ryugou/cs-support-demo)
  └─ main merge → GitHub Actions (WIF keyless auth)
       ├─ docker build+push → Artifact Registry (asia-northeast1)
       ├─ gcloud run deploy cs-support-mcp（常時）
       ├─ server/data/** or schema/** 変更時 → Cloud Run Job ingest-rules 実行
       └─ workflow_dispatch → Cloud Run Job ingest-urtect（マニュアル再クロール）
新プロジェクト（ID は作成時確定・課金 SIVIRA）
  Cloud Run service cs-support-mcp: min=0/max=1, 512Mi-1Gi, port 8080, timeout 3600
    - Direct VPC egress → cs-support-vpc subnet(10.20.0.0/24, asia-northeast1)
    - VPC peering ⇄ punkrecord-egghead-vpc（firewall に 10.20.0.0/24 追加）
    - GCS bucket を /data にボリュームマウント（WORM 監査・検索改善キュー永続）
    - Secret Manager: vegapunk-bearer-token(env) / cs-support-llm-api-key(env) /
      cs-support-jwt-secret(file mount → CS_SUPPORT_JWT_SECRET_FILE)
    - 認証: Cloud Run 層は allow-unauthenticated、アプリ層 JWT 必須（default_actor なし）
  Cloud Run Jobs: ingest-rules / ingest-urtect（同一イメージ・args 違い・同 VPC/secrets）
```

## Global Constraints

- 費用: Cloud Run min-instances=0 / Artifact Registry は旧イメージ削除運用 / Serverless VPC Access connector は使わない（Direct VPC egress・無料）
- Python/TypeScript 禁止。secret を repo・イメージ・ログに置かない（.dockerignore 必須: .env / certs / audit.jsonl / target / .git）
- WORM の I5（provenance・hash chain）を Cloud Run でも維持: GCS マウント + append 毎 sync（インスタンス kill でもイベント単位で耐久）
- vegapunk リポジトリ・VM 内部には触れない（GCP ネットワーク設定のみ。既存パターンの複製）
- コミットは Conventional Commits。実装は kaneko → reviewer → Fable 受理

## Task 1（kaneko）: コンテナ・設定・WORM 耐久化

**Files:** `Dockerfile`, `.dockerignore`(新規), `server/config.cloudrun.toml`(新規), `server/src/harness/audit.rs`

1. **audit.rs**: `append()` の書き込み後に `file.sync_all()` を追加（GCS FUSE は close/fsync でアップロードされるため、fsync でイベント単位の耐久性を保証。ローカル運用でも durability 向上・デモ流量でコスト無視可）。既存テスト green 維持
2. **Dockerfile**: builder に `--bin ingest_rules --bin ingest_urtect` を追加し runtime へ COPY（Cloud Run Jobs で使用）。`server/config.cloudrun.toml` を COPY。既存 GCE 用行は維持
3. **.dockerignore**(新規): `server/target`, `server/certs`, `.env*`（`!.env.example`）, `**/audit.jsonl`, `.git`, `docs`, `.superpowers`
4. **config.cloudrun.toml**: `bind_addr = "0.0.0.0:8080"`（BIND_ADDR env 上書き可の既存機構を確認）/ `vegapunk_endpoint` は env `VEGAPUNK_ENDPOINT` 上書き（既存機構確認: config.gce.toml と同形）/ projects = `sivira-cs-demo`(legacy) + `urtect`(manual_v1, vector_route_enabled は harness 側 true) / `[auth]` default_actor **なし**・`jwt_secret_file` は env `CS_SUPPORT_JWT_SECRET_FILE` 参照（既存機構）/ `[[actors]]` op-001(operator)・sup-001(supervisor) allowed_schemas=両テナント / `[harness]` audit_log_path=`/data/audit/audit.jsonl`, queue=`/data/audit/search-improvement-queue.jsonl`, lexicon/ng = urtect 用パス（イメージ内 data/urtect/…）, `default_escalation_route="support_desk"`, `vector_route_enabled=true` / `[llm]` enabled=true（キーは env）/ thresholds・grading は現行値
5. 検証: `cargo test --lib`（audit 含め全 green）・`cargo check --bins`・fmt。Docker build はローカル環境依存のため CI/初回デプロイで検証（その旨報告）

## Task 2（kaneko）: GitHub Actions ワークフロー

**Files:** `.github/workflows/deploy.yml`(新規)

- 注意: グローバル CLAUDE.md は workflows 作成時 `github-actions-optimize` スキル使用を求めるが、本環境に同スキルは未導入 → 手書き + reviewer 検証で代替（ユーザ報告事項）
- `on: push: branches: [main]` + `workflow_dispatch:`（inputs: job = recrawl|rules）
- 認証: `google-github-actions/auth@v3`（Workload Identity Federation、キーレス）。`GCP_WIF_PROVIDER` / `GCP_SA_EMAIL` / `GCP_PROJECT_ID` / `GAR_REPO` / `RUN_REGION` は **GitHub Actions variables** 参照（インフラ側 Task 3 で値確定後に gh CLI で設定）
- job `build-deploy`: checkout → auth → docker buildx（`cache-from/to: gha`）→ AR push（tag = git sha + latest）→ `gcloud run deploy cs-support-mcp --image …`（設定変更はデプロイフラグでなくサービス側定義を維持: `--image` のみ更新）→ `gcloud run jobs update ingest-rules --image …` / `ingest-urtect --image …`
- job `auto-ingest`（needs: build-deploy）: `dorny/paths-filter`（SHA ピン）で `server/data/**` `schema/**` 変更検知 → `gcloud run jobs execute ingest-rules --wait`
- `workflow_dispatch` 実行時: input に応じ `gcloud run jobs execute ingest-urtect --wait`（再クロール）または ingest-rules
- タイムアウト・concurrency(group: deploy, cancel-in-progress: false) 設定。actions は SHA ピン

## Task 3（Fable・gcloud ops）: GCP プロビジョニング

1. プロジェクト作成（ID 候補 `sivira-cs-support`、衝突時サフィックス）→ 課金リンク(SIVIRA) → API 有効化（run/artifactregistry/secretmanager/compute/iamcredentials/storage/cloudbuild）
2. VPC `cs-support-vpc`(custom) + subnet `cs-support-subnet` 10.20.0.0/24 (asia-northeast1)
3. Peering 両側作成（新 ⇄ punkrecord-egghead-vpc）→ ACTIVE 確認
4. punkrecord-egghead の firewall `punkrecord-egghead-allow-internal-grpc` の sourceRanges に 10.20.0.0/24 を追加（既存 2 レンジ維持）
5. AR repo `cs-support`(docker) / GCS bucket（uniform・asia-northeast1）
6. Secrets 投入: vegapunk-bearer-token・cs-support-llm-api-key（ローカル .env から表示せずパイプ）・cs-support-jwt-secret（新規生成 openssl rand -hex 32）
7. SA: `run-cs-support`（runtime: secretAccessor + bucket objectAdmin）/ `deploy-cs-support`（run.admin + artifactregistry.writer + runtime SA への serviceAccountUser）+ WIF pool/provider（repo=ryugou/cs-support-demo 限定）
8. 初回イメージ: `gcloud builds submit`（ローカル docker 不要）→ Cloud Run service + Jobs 作成（Direct VPC egress・GCS ボリューム・secrets 配線・min0/max1・timeout3600・concurrency 既定）
9. gh CLI で Actions variables 設定

## Task 4（Fable）: 実機検証

- healthz 200 / JWT なし → 拒否・JWT あり initialize → 12 tools
- evaluate spot（A/C 群）→ vegapunk 到達（peering 越し）と LLM 抽出（mode=hybrid）確認
- ingest-rules Job 実行 → 成功ログ / ingest-urtect Job（재クロール）動作確認
- コールドスタート後の再接続（keepalive/再接続挙動）確認

## Task 5: docs + PR

- プロジェクト CLAUDE.md の GCE runbook 節を「旧構成（参考）」化し、新 Cloud Run runbook（URL・JWT 発行手順・データ更新フロー・再クロール手順）を追記。spec 実装状況更新
- PR 作成（push はユーザ）→ Copilot ループ

## データ登録の運用（成果物の使い方）

- 業務データ到着 → `server/data/urtect/*.json` を PR で編集 → レビュー・マージ → **自動でデプロイ + ingest-rules 実行**（完了）
- マニュアル改訂 → GitHub Actions の Run workflow ボタン（job=recrawl）→ ingest-urtect が差分クロール
- 語彙追加（lexicon）はイメージ内包のためマージ→デプロイで反映。MENTIONS_SIGNAL 再結線が必要な場合のみ recrawl 実行
