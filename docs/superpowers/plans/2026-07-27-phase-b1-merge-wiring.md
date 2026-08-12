# Phase B1: Merge 配線と community 実測 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** vegapunk の `Merge` RPC を配線し、本番 schema `urtect` に対して実行したうえで、global 検索が何を返すかを実測できる CLI と Cloud Run job を用意する。

**Architecture:** `VegapunkClient` に `merge` / `stats` を足し、`search` の戻りに `SearchExecution` を露出させる（既存呼び出し元は互換ヘルパで無改修）。新 bin `merge_schema` が「stats → probe → Merge → stats → probe」を 1 実行で回し、B2 の分岐（hybrid 切替で足りるか、自前 concept-expansion が要るか）を決める実測 JSON を出す。gRPC 境界のコードは薄く保ち、判定・分類・整形は純関数に切り出して単体テストする。

**Tech Stack:** Rust / tonic + prost（gRPC）/ clap（CLI）/ tracing / anyhow / serde_json

**Spec:** [`docs/superpowers/specs/2026-07-27-phase-b-community-merge-design.md`](../specs/2026-07-27-phase-b-community-merge-design.md)

## Global Constraints

- Python / TypeScript を使わない。ファイルも作らない
- 検証コマンドは必ず `--manifest-path server/Cargo.toml` を付ける。`cd` / `git -C` を使わない
- エラーを握りつぶさない。degrade・skip は理由付きでログする
- 認証情報をハードコードしない。bearer token は `--token-file`（主経路）→ `--token-env`（フォールバック）
- 既存 CLI（`server/src/bin/verify_alarmcom.rs` / `ingest_alarmcom.rs`）の引数・token 解決の流儀を踏襲する
- B1 ではグラフを書き換えない。retrieval 経路の検索 mode も変えない（`local` のまま）
- Conventional Commits。commit は可、push・PR 作成は指示があるまで禁止
- 対象 schema の既定値は `urtect`

---

### Task 1: `VegapunkClient::merge` と `stats`

**Files:**
- Modify: `server/src/vegapunk.rs`（`use` の proto import 追加、`impl VegapunkClient` にメソッド追加、末尾の `mod tests` にテスト追加）

**Interfaces:**
- Consumes: 既存の `VegapunkClient::call`（private ヘルパ）、`GrpcLimits`
- Produces:
  - `pub async fn merge(&self, schema: &str) -> anyhow::Result<()>`
  - `pub async fn stats(&self, schema: &str) -> anyhow::Result<crate::proto::graphrag::GetStatsResponse>`
  - `pub fn merge_error_hint(code: tonic::Code) -> Option<&'static str>`（Task 3 では使わないが、エラー文言のテスト対象）

- [ ] **Step 1: 失敗するテストを書く**

`server/src/vegapunk.rs` の既存 `mod tests` に追加する（`mod tests` が無ければファイル末尾に作る）:

```rust
#[test]
fn merge_error_hint_maps_operational_codes() {
    // 運用者が「次に何をすればよいか」を判断できる文言であること。
    let precondition = merge_error_hint(Code::FailedPrecondition).expect("hint");
    assert!(precondition.contains("同時実行"));
    let denied = merge_error_hint(Code::PermissionDenied).expect("hint");
    assert!(denied.contains("admin"));
    let deadline = merge_error_hint(Code::DeadlineExceeded).expect("hint");
    assert!(deadline.contains("--timeout-secs"));
    // 未分類の code はヒント無し（元の Status をそのまま見せる）
    assert!(merge_error_hint(Code::Internal).is_none());
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test --manifest-path server/Cargo.toml merge_error_hint`
Expected: FAIL（`merge_error_hint` が未定義でコンパイルエラー）

- [ ] **Step 3: 最小実装を書く**

`server/src/vegapunk.rs` の proto import に `GetStatsRequest, GetStatsResponse, MergeRequest` を追加し、`impl VegapunkClient` に次を足す:

```rust
    /// Leiden コミュニティ検出 + CommunitySummary 生成 + Node2Vec を schema 全体に対して
    /// 実行する（vegapunk `Merge` RPC）。**admin ロール必須・同期実行・同一 schema で同時 1 本のみ。**
    /// 応答は空（`MergeResponse {}`）で進捗もジョブ ID も返らないため、成否の確認は
    /// `stats()` の `community_count` で行う。10 万ノード規模では長時間化するので、
    /// 呼び出し側は `GrpcLimits.timeout_secs` を十分長く張り替えてから使うこと。
    pub async fn merge(&self, schema: &str) -> Result<()> {
        let req = MergeRequest {
            schema: schema.to_string(),
        };
        self.call(
            |mut client, request| async move {
                client.merge(request).await.map(|resp| resp.into_inner())
            },
            req,
        )
        .await
        .map(|_| ())
        .map_err(|err| annotate_grpc_error(err, &format!("merge schema {schema}")))
    }

    /// schema のノード / エッジ / ベクトル / コミュニティ件数。`community_count` は
    /// Merge を実行したかどうかの一次証跡になる（Merge 前は 0 のはず）。
    pub async fn stats(&self, schema: &str) -> Result<crate::proto::graphrag::GetStatsResponse> {
        let req = GetStatsRequest {
            schema: Some(schema.to_string()),
            node_type: None,
            filters: Vec::new(),
        };
        self.call(
            |mut client, request| async move {
                client.get_stats(request).await.map(|resp| resp.into_inner())
            },
            req,
        )
        .await
        .map_err(|err| annotate_grpc_error(err, &format!("get stats for schema {schema}")))
    }
```

同ファイルの free function 群（`page_is_last` の近く）に:

```rust
/// gRPC の status code を、運用者が次のアクションを判断できる文言に写像する。
/// 分類できない code は `None` を返し、元の `Status` の message をそのまま見せる
/// （当てずっぽうの説明を足して原因を誤誘導しない）。
pub fn merge_error_hint(code: Code) -> Option<&'static str> {
    match code {
        Code::FailedPrecondition => Some(
            "同一 schema で Merge が同時実行中か、サーバ側の前提未達（embedding / LLM 設定等）。\
             実行中なら完了を待つ。リトライでは解決しない",
        ),
        Code::PermissionDenied => Some(
            "Merge は admin ロール必須。使用中の bearer token の権限を確認する",
        ),
        Code::DeadlineExceeded => Some(
            "クライアント側 timeout。--timeout-secs を引き上げる。\
             サーバ側では Merge が継続している可能性があるため、再実行前に stats の community_count を確認する",
        ),
        _ => None,
    }
}

/// `call` が返す anyhow エラーに、gRPC code 由来の運用ヒントを付ける。
/// `call` は `tonic::Status` を `Into` で anyhow 化しているので downcast で code を取り出す。
fn annotate_grpc_error(err: anyhow::Error, context: &str) -> anyhow::Error {
    let hint = err
        .downcast_ref::<tonic::Status>()
        .and_then(|status| merge_error_hint(status.code()));
    match hint {
        Some(hint) => err.context(format!("{context}: {hint}")),
        None => err.context(context.to_string()),
    }
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test --manifest-path server/Cargo.toml merge_error_hint`
Expected: PASS

Run: `cargo check --manifest-path server/Cargo.toml --all-targets`
Expected: 警告なしで成功（`GetStatsResponse` / `MergeRequest` の import 漏れがないこと）

- [ ] **Step 5: commit**

```bash
git add server/src/vegapunk.rs
git commit -m "feat(vegapunk): add Merge and GetStats client methods (#8 Phase B1)"
```

---

### Task 2: `search` に `SearchExecution` を露出し degrade を可視化する

**Files:**
- Modify: `server/src/vegapunk.rs`（`search` の書き換え、`SearchOutcome` 追加、degrade ログ、テスト追加）

**Interfaces:**
- Consumes: Task 1 の `annotate_grpc_error`
- Produces:
  - `pub struct SearchOutcome { pub results: Vec<SearchResultItem>, pub execution: Option<SearchExecution> }`
  - `pub async fn search_with_mode(&self, schema: &str, query: &str, top_k: i32, mode: &str) -> anyhow::Result<SearchOutcome>`
  - `pub async fn search(&self, schema: &str, query: &str, top_k: i32) -> anyhow::Result<Vec<SearchResultItem>>`（**シグネチャ不変**。内部で `search_with_mode(.., "local")` を呼ぶ互換ヘルパ）
  - `pub fn degradation_summary(d: &SearchDegradation) -> String`

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn degradation_summary_renders_component_and_reason() {
    use crate::proto::graphrag::{SearchComponent, SearchDegradedReason, SearchDegradation};
    let degradation = SearchDegradation {
        component: SearchComponent::Global as i32,
        reason: SearchDegradedReason::NotReady as i32,
        message: "No community summaries found".to_string(),
    };
    let rendered = degradation_summary(&degradation);
    assert!(rendered.contains("GLOBAL"), "component が読める: {rendered}");
    assert!(rendered.contains("NOT_READY"), "reason が読める: {rendered}");
    assert!(
        rendered.contains("No community summaries found"),
        "サーバの message を落とさない: {rendered}"
    );
}

#[test]
fn degradation_summary_keeps_unknown_enum_values_visible() {
    use crate::proto::graphrag::SearchDegradation;
    // 未知の enum 値（proto 追加時）でも数値を残し、握りつぶさない。
    let degradation = SearchDegradation {
        component: 9999,
        reason: 8888,
        message: String::new(),
    };
    let rendered = degradation_summary(&degradation);
    assert!(rendered.contains("9999"), "未知 component を数値で残す: {rendered}");
    assert!(rendered.contains("8888"), "未知 reason を数値で残す: {rendered}");
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test --manifest-path server/Cargo.toml degradation_summary`
Expected: FAIL（`degradation_summary` 未定義）

- [ ] **Step 3: 最小実装を書く**

proto import に `SearchDegradation, SearchExecution, SearchResultItem` を追加（未 import のもののみ）。free function に:

```rust
/// `SearchDegradation` を 1 行のログ文字列にする。prost の enum は i32 なので、
/// 既知値は名前、未知値は数値のまま残す（proto にフィールドが増えても情報を落とさない）。
pub fn degradation_summary(degradation: &SearchDegradation) -> String {
    use crate::proto::graphrag::{SearchComponent, SearchDegradedReason};
    let component = SearchComponent::try_from(degradation.component)
        .map(|c| c.as_str_name().to_string())
        .unwrap_or_else(|_| format!("UNKNOWN({})", degradation.component));
    let reason = SearchDegradedReason::try_from(degradation.reason)
        .map(|r| r.as_str_name().to_string())
        .unwrap_or_else(|_| format!("UNKNOWN({})", degradation.reason));
    format!("{component}/{reason}: {}", degradation.message)
}
```

> 実装メモ: `as_str_name()` が返す文字列は prost の生成規則で `SEARCH_COMPONENT_GLOBAL` /
> `SEARCH_DEGRADED_REASON_NOT_READY` のような prefix 付きになる。テストの `contains("GLOBAL")` /
> `contains("NOT_READY")` はこの prefix 付きでも通る。prefix を剥がす整形を足すなら
> テストの期待値も揃えること。

`search` を次に置き換える:

```rust
/// `Search` の応答から、プロダクトが使う 2 つを取り出したもの。
/// `execution` は degrade（Merge 未実行で global が落ちた等）の可視化に使う。
pub struct SearchOutcome {
    pub results: Vec<SearchResultItem>,
    pub execution: Option<SearchExecution>,
}

// impl VegapunkClient 内
    /// 既存呼び出し元互換の local 検索。retrieval / verify CLI はこちらを使う。
    pub async fn search(
        &self,
        schema: &str,
        query: &str,
        top_k: i32,
    ) -> Result<Vec<SearchResultItem>> {
        Ok(self
            .search_with_mode(schema, query, top_k, "local")
            .await?
            .results)
    }

    /// mode を明示する検索。`global` は Merge 未実行だと FAILED_PRECONDITION、
    /// `hybrid` は global 部分だけ local へ degrade する（落ちない）。
    /// degrade したときは warn で理由を出す（黙って degrade させない）。
    pub async fn search_with_mode(
        &self,
        schema: &str,
        query: &str,
        top_k: i32,
        mode: &str,
    ) -> Result<SearchOutcome> {
        let req = SearchRequest {
            text: query.to_string(),
            filter: None,
            depth: Some(1),
            top_k: Some(top_k),
            format: None,
            mode: Some(mode.to_string()),
            schema: schema.to_string(),
            offset: Some(0),
            limit: Some(top_k),
            structural_weight: Some(0.0),
        };
        let resp = self
            .call(
                |mut client, request| async move {
                    client.search(request).await.map(|resp| resp.into_inner())
                },
                req,
            )
            .await
            .map_err(|err| annotate_grpc_error(err, &format!("search schema {schema}")))?;
        if let Some(execution) = resp.execution.as_ref() {
            if execution.degraded {
                tracing::warn!(
                    schema,
                    requested_mode = %execution.requested_mode,
                    effective_mode = %execution.effective_mode,
                    degradations = %execution
                        .degradations
                        .iter()
                        .map(degradation_summary)
                        .collect::<Vec<_>>()
                        .join("; "),
                    "vegapunk search degraded"
                );
            }
        }
        Ok(SearchOutcome {
            results: resp.results,
            execution: resp.execution,
        })
    }
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test --manifest-path server/Cargo.toml --lib`
Expected: PASS（既存テストも含めて全通過。`search` のシグネチャを変えていないので retrieval 側の改修は不要）

Run: `cargo check --manifest-path server/Cargo.toml --all-targets`
Expected: 成功

- [ ] **Step 5: commit**

```bash
git add server/src/vegapunk.rs
git commit -m "feat(vegapunk): expose SearchExecution and warn on degraded search (#8 Phase B1)"
```

---

### Task 3: `merge_schema` bin

**Files:**
- Create: `server/src/bin/merge_schema.rs`
- Modify: `server/Cargo.toml`（`[[bin]]` 追加）
- Modify: `server/src/manual/retrieval.rs:6`（`fn kind_marker` を `pub fn kind_marker` にする。bin から node_id 分類に使うため）
- Modify: `Dockerfile`（`cargo build --release` の `--bin` 列挙と `COPY --from=builder` 列挙の両方に `merge_schema` を追加する。両方に入れないと Cloud Run job `merge-schema` のイメージにバイナリが存在せず、job 実行時に初めて失敗する）

**Interfaces:**
- Consumes: Task 1 の `merge` / `stats`、Task 2 の `search_with_mode` / `SearchOutcome`、既存 `GrpcLimits` / `VegapunkClient::connect_with_limits`、`cs_support_mcp::manual::schema_ids::{KIND_SECTION, KIND_CONCEPT, KIND_DOC, KIND_PRODUCT}`、`cs_support_mcp::manual::retrieval::kind_marker`
- Produces: 実行バイナリ `merge_schema`（他タスクから参照されない終端）

- [ ] **Step 1: 失敗するテストを書く**

`server/src/bin/merge_schema.rs` の末尾に置く（実装より先に書く）:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_hit_recognizes_manual_section() {
        assert_eq!(
            classify_hit(Some("urtect:gen1:ManualSection:sec-video-devices-adc-v724")),
            HitKind::ManualSection
        );
    }

    #[test]
    fn classify_hit_recognizes_concept() {
        assert_eq!(
            classify_hit(Some("urtect:gen1:Concept:motion-detection")),
            HitKind::Concept
        );
    }

    #[test]
    fn classify_hit_marks_community_summary_as_other() {
        // community summary の id 形は未知。ManualSection でも Concept でもない、が要点。
        assert_eq!(classify_hit(Some("urtect:gen1:CommunitySummary:3")), HitKind::Other);
        assert_eq!(classify_hit(None), HitKind::Other);
    }

    #[test]
    fn probe_counts_group_hits_by_kind() {
        let counts = ProbeCounts::from_kinds(&[
            HitKind::ManualSection,
            HitKind::ManualSection,
            HitKind::Concept,
            HitKind::Other,
        ]);
        assert_eq!(counts.manual_section, 2);
        assert_eq!(counts.concept, 1);
        assert_eq!(counts.other, 1);
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test --manifest-path server/Cargo.toml --bin merge_schema`
Expected: FAIL（`classify_hit` / `HitKind` / `ProbeCounts` が未定義。`[[bin]]` 未登録なら「no bin target」でも可）

- [ ] **Step 3: 最小実装を書く**

`server/Cargo.toml` の `[[bin]]` 群の末尾に:

```toml
[[bin]]
name = "merge_schema"
path = "src/bin/merge_schema.rs"
```

`server/src/manual/retrieval.rs:6` を `pub fn kind_marker(kind: &str) -> String {` に変更する。

`server/src/bin/merge_schema.rs` を作る:

```rust
//! vegapunk `Merge` RPC の実行と、その前後の観測を行う CLI（Issue #8 Phase B1）。
//!
//! Merge は **schema 全体の再計算・同期実行・admin ロール必須・同一 schema で同時 1 本のみ**で、
//! 応答は空（進捗もジョブ ID も返らない）。したがって「実行したか / 効いたか」は
//! `GetStats.community_count` の前後差で確認する。
//!
//! さらに Phase B2 の分岐（`mode=hybrid` への切替だけで別記事 join が成立するか、
//! `MENTIONS_CONCEPT` を辿る自前 concept-expansion が必要か）を決めるため、Merge の前後で
//! **global / hybrid の返却物を実測**して JSON で出す。統合仕様書は global を
//! 「コミュニティ要約を検索し代表メンバーを返す」と書いているが、proto の `SearchResultItem` に
//! メンバー一覧フィールドは無く、ManualSection の node_id が返るかは実測しないと確定しない。
//!
//! VPC 内 Cloud Run job として実行する前提（本番 vegapunk は VPC 内部限定）。
//! ネットワーク非依存の分類・集計は純関数として単体テストがある。
use anyhow::{Context, Result};
use clap::Parser;
use cs_support_mcp::{
    manual::{
        retrieval::kind_marker,
        schema_ids::{KIND_CONCEPT, KIND_SECTION},
    },
    vegapunk::{GrpcLimits, VegapunkClient},
};
use serde_json::{json, Value};
use std::{env, fs, path::PathBuf, time::Instant};

/// probe に使う日本語クエリ。**複数記事にまたがって答えが散る問い合わせ**を選ぶ
/// （single article で閉じるクエリだと、community 由来のヒットが出ても差が見えない）。
const PROBE_QUERIES: &[&str] = &[
    "カメラが夜だけ映らないのはなぜですか",
    "通知が届かないときに確認することは何ですか",
    "Wi-Fi を変更したあとに機器を再接続する手順を教えてください",
    "センサーの電池を交換する方法を教えてください",
];

/// probe で叩く検索 mode。`local` は基準線（Merge の影響を受けない）、
/// `global` は Merge 前だと FAILED_PRECONDITION が正常、`hybrid` が B2 の本命。
const PROBE_MODES: &[&str] = &["local", "hybrid", "global"];

#[derive(Debug, Parser)]
struct Args {
    #[arg(
        long,
        env = "VEGAPUNK_ENDPOINT",
        default_value = "http://vegapunk.local:6840"
    )]
    endpoint: String,
    #[arg(long, default_value = "urtect")]
    schema: String,
    #[arg(long, default_value = "VEGAPUNK_BEARER_TOKEN")]
    token_env: String,
    /// bearer token ファイル（主経路）。無い/読めない場合のみ --token-env にフォールバックする。
    #[arg(
        long,
        env = "VEGAPUNK_BEARER_TOKEN_FILE",
        default_value = "/private/tmp/vegapunk-bearer-token"
    )]
    token_file: Option<PathBuf>,
    /// gRPC 呼び出しの上限秒。既定 6h。Merge は schema 全体の再計算 + 全コミュニティの
    /// LLM 要約で、既定の 120s ではまず足りない。
    #[arg(long, default_value_t = 21_600)]
    timeout_secs: u64,
    /// Merge を実行せず観測だけ行う（実行前の状態確認、B2 検討時の再観測）。
    #[arg(long)]
    probe_only: bool,
    /// probe の top-k。
    #[arg(long, default_value_t = 10)]
    top_k: i32,
}

/// probe で返ったヒットの種別。B2 の分岐はこの内訳だけで決まる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HitKind {
    ManualSection,
    Concept,
    Other,
}

/// node_id の kind marker（`:{kind}:`）で分類する。id が無いヒットは Other。
fn classify_hit(id: Option<&str>) -> HitKind {
    let Some(id) = id else {
        return HitKind::Other;
    };
    if id.contains(&kind_marker(KIND_SECTION)) {
        HitKind::ManualSection
    } else if id.contains(&kind_marker(KIND_CONCEPT)) {
        HitKind::Concept
    } else {
        HitKind::Other
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct ProbeCounts {
    manual_section: usize,
    concept: usize,
    other: usize,
}

impl ProbeCounts {
    fn from_kinds(kinds: &[HitKind]) -> Self {
        let mut counts = Self::default();
        for kind in kinds {
            match kind {
                HitKind::ManualSection => counts.manual_section += 1,
                HitKind::Concept => counts.concept += 1,
                HitKind::Other => counts.other += 1,
            }
        }
        counts
    }
}

/// token 解決: 既定は --token-file、ファイルが無い/読めない場合のみ --token-env。
/// `verify_alarmcom.rs` / `ingest_alarmcom.rs` の同名関数と同じ挙動（Args 型が異なるため複製）。
fn read_token(args: &Args) -> Result<String> {
    if let Some(path) = &args.token_file {
        match fs::read_to_string(path) {
            Ok(body) => {
                let trimmed = body.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_string());
                }
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "token file unreadable; falling back to --token-env"
                );
            }
        }
    }
    let token = env::var(&args.token_env).with_context(|| {
        format!(
            "vegapunk bearer token not found (tried --token-file {:?} and env {})",
            args.token_file, args.token_env
        )
    })?;
    let trimmed = token.trim();
    if trimmed.is_empty() {
        anyhow::bail!("vegapunk bearer token env {} is empty", args.token_env);
    }
    Ok(trimmed.to_string())
}

/// stats を取って JSON にする。**取得失敗は致命ではない**（Merge 本体の成否とは別軸）ので
/// エラーを JSON に残して続行する。握りつぶさず理由を出す。
async fn stats_json(client: &VegapunkClient, schema: &str, label: &str) -> Value {
    match client.stats(schema).await {
        Ok(stats) => {
            tracing::info!(
                label,
                node_count = stats.node_count,
                edge_count = stats.edge_count,
                vector_count = stats.vector_count,
                community_count = stats.community_count,
                "vegapunk stats"
            );
            json!({
                "node_count": stats.node_count,
                "edge_count": stats.edge_count,
                "vector_count": stats.vector_count,
                "community_count": stats.community_count,
            })
        }
        Err(err) => {
            tracing::error!(label, error = %format!("{err:#}"), "stats unavailable");
            json!({ "error": format!("{err:#}") })
        }
    }
}

/// 全 PROBE_QUERIES × PROBE_MODES を叩き、ヒットの種別内訳・上位サンプル・SearchExecution を
/// JSON に残す。**エラー（Merge 前の global = FAILED_PRECONDITION 等）は記録して次へ進む**
/// （前後差を取るのが目的で、片方のエラーで観測全体を落とさない）。
async fn probe(client: &VegapunkClient, args: &Args, label: &str) -> Value {
    let mut entries = Vec::new();
    for query in PROBE_QUERIES {
        for mode in PROBE_MODES {
            let entry = match client
                .search_with_mode(&args.schema, query, args.top_k, mode)
                .await
            {
                Ok(outcome) => {
                    let kinds: Vec<HitKind> = outcome
                        .results
                        .iter()
                        .map(|item| classify_hit(item.id.as_deref()))
                        .collect();
                    let counts = ProbeCounts::from_kinds(&kinds);
                    let samples: Vec<Value> = outcome
                        .results
                        .iter()
                        .take(5)
                        .map(|item| {
                            json!({
                                "type": item.r#type,
                                "id": item.id,
                                "score": item.score,
                                // text は先頭だけ（ログ肥大を避ける）。返却の「形」が分かればよい。
                                "text_head": item
                                    .text
                                    .as_deref()
                                    .map(|t| t.chars().take(120).collect::<String>()),
                            })
                        })
                        .collect();
                    let execution = outcome.execution.as_ref().map(|execution| {
                        json!({
                            "requested_mode": execution.requested_mode,
                            "effective_mode": execution.effective_mode,
                            "degraded": execution.degraded,
                            "degradations": execution
                                .degradations
                                .iter()
                                .map(cs_support_mcp::vegapunk::degradation_summary)
                                .collect::<Vec<_>>(),
                        })
                    });
                    json!({
                        "query": query,
                        "mode": mode,
                        "hit_count": outcome.results.len(),
                        "counts": {
                            "manual_section": counts.manual_section,
                            "concept": counts.concept,
                            "other": counts.other,
                        },
                        "samples": samples,
                        "execution": execution,
                    })
                }
                Err(err) => {
                    // Merge 前の global は FAILED_PRECONDITION が正常。異常ではないので error にしない。
                    tracing::warn!(query, mode, error = %format!("{err:#}"), "probe query failed");
                    json!({ "query": query, "mode": mode, "error": format!("{err:#}") })
                }
            };
            entries.push(entry);
        }
    }
    json!({ "label": label, "entries": entries })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let token = read_token(&args)?;
    let limits = GrpcLimits {
        timeout_secs: args.timeout_secs,
        ..GrpcLimits::default()
    };
    let client = VegapunkClient::connect_with_limits(&args.endpoint, &token, limits)
        .await
        .context("connect vegapunk")?;

    let stats_before = stats_json(&client, &args.schema, "before").await;
    let probe_before = probe(&client, &args, "before").await;

    let (merge_result, elapsed_secs) = if args.probe_only {
        tracing::info!("--probe-only: skipping Merge");
        (json!({ "skipped": true }), 0.0)
    } else {
        tracing::info!(schema = %args.schema, "starting Merge (synchronous, whole-schema recompute)");
        let started = Instant::now();
        let outcome = client.merge(&args.schema).await;
        let elapsed = started.elapsed().as_secs_f64();
        match outcome {
            Ok(()) => {
                tracing::info!(elapsed_secs = elapsed, "Merge completed");
                (json!({ "ok": true }), elapsed)
            }
            Err(err) => {
                // fail closed: サマリを出してから非 0 終了する（観測結果は捨てない）。
                tracing::error!(error = %format!("{err:#}"), elapsed_secs = elapsed, "Merge failed");
                let summary = json!({
                    "schema": args.schema,
                    "stats_before": stats_before,
                    "probe_before": probe_before,
                    "merge": { "ok": false, "error": format!("{err:#}") },
                    "merge_elapsed_secs": elapsed,
                });
                println!("{}", serde_json::to_string_pretty(&summary)?);
                return Err(err);
            }
        }
    };

    let stats_after = stats_json(&client, &args.schema, "after").await;
    let probe_after = probe(&client, &args, "after").await;

    let summary = json!({
        "schema": args.schema,
        "probe_only": args.probe_only,
        "stats_before": stats_before,
        "stats_after": stats_after,
        "merge": merge_result,
        "merge_elapsed_secs": elapsed_secs,
        "probe_before": probe_before,
        "probe_after": probe_after,
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}
```

> 実装メモ: `tracing_subscriber` / `clap` / `serde_json` / `tokio` は既存 CLI が使っている依存で、
> `server/Cargo.toml` に追加は不要。もし `merge_schema` のビルドで未解決になったら、
> **依存を足す前に既存 bin（`verify_alarmcom.rs`）の use と Cargo.toml の feature を確認**すること。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test --manifest-path server/Cargo.toml --bin merge_schema`
Expected: PASS（4 テスト）

Run: `cargo check --manifest-path server/Cargo.toml --all-targets`
Expected: 成功

Run: `cargo fmt --manifest-path server/Cargo.toml -- --check`
Expected: 差分なし

- [ ] **Step 5: commit**

```bash
git add server/src/bin/merge_schema.rs server/Cargo.toml server/src/manual/retrieval.rs
git commit -m "feat(cli): add merge_schema CLI to run Merge and probe community search (#8 Phase B1)"
```

---

### Task 4: 運用ドキュメント（Cloud Run job `merge-schema`）

**Files:**
- Modify: `CLAUDE.md`（Cloud Run デプロイ手順の節）

**Interfaces:**
- Consumes: Task 3 の bin 名 `merge_schema`
- Produces: なし（終端）

- [ ] **Step 1: CLAUDE.md の Cloud Run 節を更新**

`Cloud Run jobs（service と同一イメージ）: ingest-rules, ingest-urtect` の行に `merge-schema` を追記し、ビルド & デプロイのコマンド群に次を足す（`<tag>` は service / 全 job で同一の git short SHA）:

```sh
gcloud run jobs update merge-schema --project sivira-cs-support --region asia-northeast1 --image asia-northeast1-docker.pkg.dev/sivira-cs-support/cs-support/cs-support-mcp:<tag>
```

同じ節に、job の役割と注意点を散文で追記する（箇条書き 4 点）:

- `merge-schema` は vegapunk の `Merge` RPC（Leiden コミュニティ検出 + CommunitySummary + Node2Vec）を schema `urtect` に対して実行し、**前後の `GetStats` と global/hybrid 検索の返却物を JSON で出す**
- Merge は **schema 全体の同期再計算で、同一 schema では同時 1 本しか走らない**。実行中に再実行すると `FAILED_PRECONDITION` で弾かれる
- job の `--task-timeout` は CLI の `--timeout-secs`（既定 6h）以上に取ること。短いと Merge の途中で task が殺され、サーバ側だけ処理が続く状態になる
- ingest とは独立した job にしてある。`ingest_alarmcom` は実測約 6 時間かかるため、その末尾に Merge を積むと Merge だけの再実行ができない

- [ ] **Step 2: 記述が実装と一致していることを確認**

Run: `grep -n "merge-schema" CLAUDE.md`
Expected: job 一覧・デプロイコマンド・注意点の 3 箇所以上にヒットする

- [ ] **Step 3: commit**

```bash
git add CLAUDE.md
git commit -m "docs: document merge-schema Cloud Run job (#8 Phase B1)"
```

---

## 実装後の受け渡し（実装者のスコープ外・オーケストレータが行う）

- `gcloud run jobs create merge-schema`（初回のみ）+ 既存 job と同一 tag のイメージ反映
- job 実行 → 出力 JSON を evidence として Phase B2 の分岐を決定（spec に追記）
- `verify_alarmcom` による retrieval 品質の before/after 比較は B2 で行う

## 検証（全タスク完了時にまとめて実行し、出力を evidence として提出する）

- `cargo fmt --manifest-path server/Cargo.toml -- --check`
- `cargo check --manifest-path server/Cargo.toml --all-targets`
- `cargo test --manifest-path server/Cargo.toml`

実 vegapunk への接続を伴う検証（実 Merge / 実 probe）は VPC 内 Cloud Run job でしか実行できないため、**実装者は実行しない**。未実行であることを最終報告に明記する。
