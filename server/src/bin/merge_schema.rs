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
    /// Merge を実行せず観測だけ行う（実行前の状態確認、B2 検討時の再観測)。
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
        assert_eq!(
            classify_hit(Some("urtect:gen1:CommunitySummary:3")),
            HitKind::Other
        );
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
