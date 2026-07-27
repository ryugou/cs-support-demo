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
    proto::graphrag::{ComponentReadiness, ReadinessState, SearchReadiness},
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

    /// 1 実行分（全クエリ × 全 mode）の合算。
    fn add(&mut self, other: Self) {
        self.manual_section += other.manual_section;
        self.concept += other.concept;
        self.other += other.other;
    }

    fn to_json(self) -> Value {
        json!({
            "manual_section": self.manual_section,
            "concept": self.concept,
            "other": self.other,
        })
    }
}

/// probe 内訳の before→after 差分。`manual_section` が減って `other` が増えるなら
/// community 由来の item に top_k を食われている、という B2 の判断材料になる。
/// `compared_pairs` は差分の母数（両側そろった `(query, mode)` の数）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ProbeCountsDelta {
    manual_section: i64,
    concept: i64,
    other: i64,
    compared_pairs: usize,
}

impl ProbeCountsDelta {
    fn to_json(self) -> Value {
        json!({
            "manual_section": self.manual_section,
            "concept": self.concept,
            "other": self.other,
            // 母数を出さないと、読み手は「12 probe 分の差」なのか「2 probe 分の差」なのかを
            // 検証できない。delta 単体では意味が決まらないので必ず併記する。
            "compared_pairs": self.compared_pairs,
        })
    }
}

/// probe のヒット件数を符号付き差分にする。件数は
/// `top_k × PROBE_QUERIES × PROBE_MODES`（既定で最大 120 件）で `i64` に収まるが、
/// 万一の桁溢れでも黙って符号が反転しないよう飽和変換する。
fn count_delta(before: usize, after: usize) -> i64 {
    let before = i64::try_from(before).unwrap_or(i64::MAX);
    let after = i64::try_from(after).unwrap_or(i64::MAX);
    after.saturating_sub(before)
}

/// probe 内訳の before→after 差分を、**同じ `(query, mode)` で両側とも成功したペアだけ**から取る。
///
/// `(query, mode)` のグリッドは `PROBE_QUERIES × PROBE_MODES` で固定なので、`per_probe` の
/// index はそのまま probe の同一性を意味する。ここで index を捨てて合算どうしを引くと
/// **母数の異なる集計の引き算**になる。これは例外ケースではなく、B1 で最初に回す Merge 実行で
/// 確実に起きる: Merge 前の `global` は FAILED_PRECONDITION が正常なので before は 8 probe 分、
/// after は 12 probe 分の合算になり、差は「Merge の効果」ではなく「probe が 4 本増えた」を映す。
///
/// `--probe-only`（after 側が無い）と、両側そろったペアが 1 つも無い場合は「差分 0」ではなく
/// 「比較不能（`None` → JSON では `null`）」を返す。測れていないものを 0 として見せない。
fn probe_counts_delta(
    before: &[Option<ProbeCounts>],
    after: Option<&[Option<ProbeCounts>]>,
) -> Option<ProbeCountsDelta> {
    let after = after?;
    if before.len() != after.len() {
        // 同じ固定グリッドから作る以上ここは起きない。起きたなら index 対応が崩れており、
        // 前方一致で辻褄を合わせると別の probe どうしを引き算することになる。比較不能で返す。
        tracing::error!(
            before_len = before.len(),
            after_len = after.len(),
            "probe grid size mismatch; refusing to compute a delta from misaligned probes"
        );
        return None;
    }
    let mut delta = ProbeCountsDelta::default();
    for (before, after) in before.iter().zip(after.iter()) {
        let (Some(before), Some(after)) = (before, after) else {
            continue;
        };
        delta.manual_section = delta
            .manual_section
            .saturating_add(count_delta(before.manual_section, after.manual_section));
        delta.concept = delta
            .concept
            .saturating_add(count_delta(before.concept, after.concept));
        delta.other = delta
            .other
            .saturating_add(count_delta(before.other, after.other));
        delta.compared_pairs += 1;
    }
    (delta.compared_pairs > 0).then_some(delta)
}

/// `GetStats.community_count` の観測結果。**`Skipped`（`--probe-only` による意図した省略）と
/// `Error`（取得できなかった）を型で区別する**。両者を同じ「値なし」に潰すと、
/// 意図した省略まで異常として fail させるか、逆に取得失敗を見逃すかのどちらかになる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatsObservation {
    Value(i64),
    Error,
    Skipped,
}

impl StatsObservation {
    fn value(self) -> Option<i64> {
        match self {
            Self::Value(count) => Some(count),
            Self::Error | Self::Skipped => None,
        }
    }

    fn is_error(self) -> bool {
        matches!(self, Self::Error)
    }
}

/// 両側とも観測できたときだけ増分を返す。`--probe-only` や stats 取得失敗では `None`。
/// 桁溢れは飽和させる（`community_count` が i64 域を跨ぐ現実解は無いが、
/// 万一のとき符号が反転した数値を運用者に見せない）。
fn community_count_delta(before: StatsObservation, after: StatsObservation) -> Option<i64> {
    Some(after.value()?.saturating_sub(before.value()?))
}

/// Merge 呼び出し自体の結果。`Skipped` は `--probe-only` による意図した省略、
/// `Failed` は Merge RPC がエラーを返した（after 側は意図して観測していない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeStatus {
    Skipped,
    Ok,
    Failed,
}

/// after 側 probe の観測結果。`StatsObservation::{Value, Error, Skipped}` と同じ
/// 「意図した省略」と「実行したが失敗した」の型分離を probe 側にも適用する。
/// before 側は `--probe-only` でも常に実行するため区別が要らず、素の usize で持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeSideObservation {
    /// Merge を実行し、after 側 probe も実行した（`attempted` 件中 `succeeded` 件成功）。
    Attempted { attempted: usize, succeeded: usize },
    /// `--probe-only`、または Merge 失敗により after 側 probe を実行しなかった。
    Skipped,
}

impl ProbeSideObservation {
    /// 実行はしたが 1 件も成功しなかった（= この実行から after 側の情報が何も得られていない）。
    fn all_failed(self) -> bool {
        matches!(
            self,
            Self::Attempted {
                attempted,
                succeeded: 0
            } if attempted > 0
        )
    }

    fn attempted_count(self) -> Option<usize> {
        match self {
            Self::Attempted { attempted, .. } => Some(attempted),
            Self::Skipped => None,
        }
    }

    fn succeeded_count(self) -> Option<usize> {
        match self {
            Self::Attempted { succeeded, .. } => Some(succeeded),
            Self::Skipped => None,
        }
    }
}

/// exit code 判定の入力。ネットワーク I/O の結果をここへ畳んでから純関数で判定する
/// （判定ロジックを vegapunk 到達性から切り離してテストするため）。
#[derive(Debug, Clone, Copy)]
struct RunObservation {
    community_before: StatsObservation,
    community_after: StatsObservation,
    merge: MergeStatus,
    probe_before_attempted: usize,
    probe_before_succeeded: usize,
    probe_after: ProbeSideObservation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunVerdict {
    Ok,
    /// exit 0 のまま warn する（想定内だが読み手に伝えるべき状態）。
    Warn(RunWarning),
    /// 非 0 終了する（この実行の目的が達成できていない）。
    Fatal(RunFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunWarning {
    CommunityCountUnchanged,
    StatsPartiallyUnavailable,
}

impl RunWarning {
    fn code(self) -> &'static str {
        match self {
            Self::CommunityCountUnchanged => "community_count_unchanged",
            Self::StatsPartiallyUnavailable => "stats_partially_unavailable",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::CommunityCountUnchanged => {
                "community_count が Merge の前後で変化していない。前回 Merge 以降グラフが\
                 変わっていなければ正常。ingest 直後にこれが出た場合は投入内容を確認する"
            }
            Self::StatsPartiallyUnavailable => {
                "GetStats が片側だけ取得できず、community_count の前後比較が成立していない。\
                 出力 JSON の stats_before / stats_after の error を確認する"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunFailure {
    ProbeAllFailed,
    MergeFailed,
    ProbeAfterAllFailed,
    StatsUnavailable,
    NoCommunities,
}

impl RunFailure {
    fn code(self) -> &'static str {
        match self {
            Self::ProbeAllFailed => "probe_all_failed",
            Self::MergeFailed => "merge_failed",
            Self::ProbeAfterAllFailed => "probe_after_all_failed",
            Self::StatsUnavailable => "stats_unavailable",
            Self::NoCommunities => "no_communities",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::ProbeAllFailed => {
                "before 側の probe が 1 件も成功しなかった。この実行からは B2 の判断材料が\
                 何も得られていない。出力 JSON の probe_before の error（接続 / 権限 / schema 名）\
                 を確認する"
            }
            Self::MergeFailed => {
                "Merge の実行自体が失敗した。after 側の stats / probe は意図して取得していない\
                 （Merge 失敗直後の再呼び出しは避けている）。出力 JSON の merge.error（gRPC code\
                 由来のヒント込み）を確認する。stats_before / probe_before は取得済みなので\
                 Merge 前の状態把握には使える"
            }
            Self::ProbeAfterAllFailed => {
                "Merge は成功したのに after 側 probe が 1 件も成功しなかった。global/hybrid の\
                 実測 JSON が空で、B2 の分岐判断（mode=hybrid 切替か自前 concept-expansion か）に\
                 使える情報が無い。出力 JSON の probe_after の error を確認する。stats_after の\
                 community_count で Merge 自体の効果は別途確認できる"
            }
            Self::StatsUnavailable => {
                "GetStats が before / after の両方で失敗し、Merge が効いたかを確認できていない。\
                 出力 JSON の stats_before / stats_after の error を確認する"
            }
            Self::NoCommunities => {
                "Merge 後も community_count が 0。Leiden コミュニティ検出が 1 件も作れていない\
                 （グラフが空、または embedding / LLM 設定の前提未達）。vegapunk 側のログを確認する"
            }
        }
    }
}

/// 実行の成否を判定する。**優先順位は上から順で、先に一致したものが勝つ**
/// （before 観測ゼロ > Merge 失敗 > after 観測ゼロ > 成否不明 > 空振り > 増分ゼロ の順。
/// 順序を入れ替えると、例えば after 側の probe 全滅を「community_count 増分あり＝正常」で
/// 覆い隠してしまう）。
fn evaluate_run(observation: RunObservation) -> RunVerdict {
    // 1. before 側の観測がゼロ。Merge の成否によらず、この実行は目的を果たしていない。
    if observation.probe_before_attempted > 0 && observation.probe_before_succeeded == 0 {
        return RunVerdict::Fatal(RunFailure::ProbeAllFailed);
    }
    // 2. Merge 自体が失敗。after 側は意図して観測していないため、これ以降は判断材料が無い。
    if observation.merge == MergeStatus::Failed {
        return RunVerdict::Fatal(RunFailure::MergeFailed);
    }
    // 3. Merge は成功したのに after 側 probe が全滅。B2 の実測 JSON が空になる致命的な欠落。
    if observation.probe_after.all_failed() {
        return RunVerdict::Fatal(RunFailure::ProbeAfterAllFailed);
    }
    // 4. stats が両側とも取れず、Merge の成否を一切確認できない。
    if observation.community_before.is_error() && observation.community_after.is_error() {
        return RunVerdict::Fatal(RunFailure::StatsUnavailable);
    }
    // 5. 真の空振り: Merge 後の community が 0 件。before の観測状態は問わない。
    if observation.community_after.value() == Some(0) {
        return RunVerdict::Fatal(RunFailure::NoCommunities);
    }
    // 6-7. 両側そろったので増分で判断する。0 は「グラフ不変の schema への再 Merge」で
    //      起こりうるため warn 止まり。
    if let Some(delta) =
        community_count_delta(observation.community_before, observation.community_after)
    {
        return if delta == 0 {
            RunVerdict::Warn(RunWarning::CommunityCountUnchanged)
        } else {
            RunVerdict::Ok
        };
    }
    // 8. 残りは致命ではない。ただし片側だけ「エラー」なら比較が不完全なので伝える
    //    （`--probe-only` による Skipped は意図した省略なので黙って通す）。
    if observation.community_before.is_error() || observation.community_after.is_error() {
        RunVerdict::Warn(RunWarning::StatsPartiallyUnavailable)
    } else {
        RunVerdict::Ok
    }
}

/// `SearchReadiness` を JSON にする。B1 の目的は「global が使える状態か」の実測なので、
/// readiness は degradations と並ぶ一次証跡。**サーバが送ってこなかった場合は `null`** にして、
/// 「readiness が来ていない」と「全コンポーネントが READY」を読み手が区別できるようにする。
fn readiness_json(readiness: Option<&SearchReadiness>) -> Value {
    let Some(readiness) = readiness else {
        return Value::Null;
    };
    json!({
        "local": component_readiness_json(readiness.local.as_ref()),
        "global": component_readiness_json(readiness.global.as_ref()),
        "community_summary": component_readiness_json(readiness.community_summary.as_ref()),
        "structural_vectors": component_readiness_json(readiness.structural_vectors.as_ref()),
        "similar_patterns": component_readiness_json(readiness.similar_patterns.as_ref()),
    })
}

/// `ComponentReadiness` 1 件分。`state` は prost の i32 なので、既知値は名前、
/// 未知値は `UNKNOWN({n})`（`degradation_summary` と同じ流儀）で数値を残す。
fn component_readiness_json(component: Option<&ComponentReadiness>) -> Value {
    let Some(component) = component else {
        return Value::Null;
    };
    let state = ReadinessState::try_from(component.state)
        .map(|state| state.as_str_name().to_string())
        .unwrap_or_else(|_| format!("UNKNOWN({})", component.state));
    json!({
        "state": state,
        "reason": component.reason,
        "revision": component.revision,
        "ready_at_ms": component.ready_at_ms,
    })
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
                // 読めたが空。Secret Manager のマウント漏れ等で起きるので、
                // 黙って env に落ちず「なぜ主経路を使わなかったか」を残す。
                tracing::warn!(
                    path = %path.display(),
                    "token file is empty; falling back to --token-env"
                );
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

/// stats の観測結果: そのまま出力に載せる JSON と、成否判定に使う `community_count`。
struct StatsSnapshot {
    json: Value,
    community_count: StatsObservation,
}

/// stats を取って JSON にする。**1 回の取得失敗だけでは即座に落とさない**
/// （前後どちらかが取れていれば判断材料になるため）。最終的な成否は `evaluate_run` が決める。
async fn stats_json(client: &VegapunkClient, schema: &str, label: &str) -> StatsSnapshot {
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
            StatsSnapshot {
                json: json!({
                    "node_count": stats.node_count,
                    "edge_count": stats.edge_count,
                    "vector_count": stats.vector_count,
                    "community_count": stats.community_count,
                }),
                community_count: StatsObservation::Value(stats.community_count),
            }
        }
        Err(err) => {
            tracing::error!(label, error = %format!("{err:#}"), "stats unavailable");
            StatsSnapshot {
                json: json!({ "error": format!("{err:#}") }),
                community_count: StatsObservation::Error,
            }
        }
    }
}

/// probe 1 周分の結果: 出力 JSON と、差分・成否判定に使う per-probe 集計。
struct ProbeOutcome {
    json: Value,
    /// `PROBE_QUERIES × PROBE_MODES` のグリッドと **index 対応が取れた** 集計。
    /// 失敗した probe は `None` にする（`ProbeCounts::default()` で埋めると
    /// 「ヒット 0 件だった」と「そもそも測れていない」が区別できなくなる）。
    /// before/after の差分はこの index 対応でのみ取る（`probe_counts_delta`）。
    per_probe: Vec<Option<ProbeCounts>>,
}

impl ProbeOutcome {
    fn attempted(&self) -> usize {
        self.per_probe.len()
    }

    fn succeeded(&self) -> usize {
        probe_succeeded(&self.per_probe)
    }
}

/// 成功した `(query, mode)` の件数。0 なら「この実行から観測が何も得られていない」。
fn probe_succeeded(per_probe: &[Option<ProbeCounts>]) -> usize {
    per_probe.iter().filter(|counts| counts.is_some()).count()
}

/// 成功した probe だけを合算した内訳。母数が違えば比較できないので、読むときは必ず
/// `succeeded` と併せて見ること（before/after 差分は必ず `probe_counts_delta` を通す）。
fn probe_totals(per_probe: &[Option<ProbeCounts>]) -> ProbeCounts {
    let mut totals = ProbeCounts::default();
    for counts in per_probe.iter().flatten() {
        totals.add(*counts);
    }
    totals
}

/// 全 PROBE_QUERIES × PROBE_MODES を叩き、ヒットの種別内訳・上位サンプル・SearchExecution を
/// JSON に残す。**エラー（Merge 前の global = FAILED_PRECONDITION 等）は記録して次へ進む**
/// （前後差を取るのが目的で、片方のエラーで観測全体を落とさない）。
async fn probe(client: &VegapunkClient, args: &Args, label: &str) -> ProbeOutcome {
    let mut entries = Vec::new();
    // グリッドの 1 マスにつき必ず 1 要素 push する（成功は Some、失敗は None）。
    // index が (query, mode) の同一性を担保するので、途中で push を飛ばさないこと。
    let mut per_probe: Vec<Option<ProbeCounts>> = Vec::new();
    for query in PROBE_QUERIES {
        for mode in PROBE_MODES {
            let (entry, counts) = match client
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
                            "readiness": readiness_json(execution.readiness.as_ref()),
                        })
                    });
                    (
                        json!({
                            "query": query,
                            "mode": mode,
                            "hit_count": outcome.results.len(),
                            "counts": counts.to_json(),
                            "samples": samples,
                            "execution": execution,
                        }),
                        Some(counts),
                    )
                }
                Err(err) => {
                    // Merge 前の global は FAILED_PRECONDITION が正常。異常ではないので error にしない。
                    tracing::warn!(query, mode, error = %format!("{err:#}"), "probe query failed");
                    (
                        json!({ "query": query, "mode": mode, "error": format!("{err:#}") }),
                        None,
                    )
                }
            };
            entries.push(entry);
            per_probe.push(counts);
        }
    }
    let totals = probe_totals(&per_probe);
    let attempted = per_probe.len();
    let succeeded = probe_succeeded(&per_probe);
    tracing::info!(
        label,
        succeeded,
        attempted,
        manual_section = totals.manual_section,
        concept = totals.concept,
        other = totals.other,
        "probe finished"
    );
    ProbeOutcome {
        json: json!({
            "label": label,
            "succeeded": succeeded,
            "attempted": attempted,
            // 成功した probe だけの合算。before/after で母数が違いうるので、
            // 単独で引き算しないこと（差分は summary.probe_counts_delta を見る）。
            "totals": totals.to_json(),
            "entries": entries,
        }),
        per_probe,
    }
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

    // after 側（stats + probe）を取るかどうかは Merge の結果と一体。判断を 1 箇所に閉じ、
    // `--probe-only` や Merge 失敗時に after を観測してしまう分岐漏れを防ぐ。
    // **出力 JSON の形はどのパスでも同じにする**（後段ツールが `.summary.verdict` を読むときに
    // パスによってスキーマが変わらないようにする。以前は Merge 失敗の早期 return が
    // 別スキーマの JSON を出しており、`stats_after` 等が丸ごと欠落していた）。
    let (merge_status, merge_json, elapsed_secs, stats_after, probe_after) = if args.probe_only {
        // Merge だけでなく after 側の観測もまとめてスキップする。同一条件の probe を
        // 2 周させても本番 vegapunk に無駄な負荷をかけるだけで、JSON の読み手には
        // 「before/after に差がある」と誤読させる材料にしかならない。
        tracing::info!("--probe-only: skipping Merge and the after-side observation");
        (
            MergeStatus::Skipped,
            json!({ "skipped": true }),
            None,
            None,
            None,
        )
    } else {
        tracing::info!(schema = %args.schema, "starting Merge (synchronous, whole-schema recompute)");
        let started = Instant::now();
        let outcome = client.merge(&args.schema).await;
        let elapsed = started.elapsed().as_secs_f64();
        match outcome {
            Ok(()) => {
                tracing::info!(elapsed_secs = elapsed, "Merge completed");
                let stats_after = stats_json(&client, &args.schema, "after").await;
                let probe_after = probe(&client, &args, "after").await;
                (
                    MergeStatus::Ok,
                    json!({ "ok": true }),
                    Some(elapsed),
                    Some(stats_after),
                    Some(probe_after),
                )
            }
            Err(err) => {
                // after 側は意図して観測しない。FAILED_PRECONDITION は「同一 schema で
                // Merge が同時実行中」の可能性があり、失敗直後に追い打ちで stats/probe を
                // 叩くのはノイズにしかならない。JSON のスキーマは成功パスと揃え、
                // after 側は null（意図した省略）で表す。
                tracing::error!(error = %format!("{err:#}"), elapsed_secs = elapsed, "Merge failed");
                (
                    MergeStatus::Failed,
                    json!({ "ok": false, "error": format!("{err:#}") }),
                    Some(elapsed),
                    None,
                    None,
                )
            }
        }
    };

    let probe_after_observation = match &probe_after {
        Some(outcome) => ProbeSideObservation::Attempted {
            attempted: outcome.attempted(),
            succeeded: outcome.succeeded(),
        },
        None => ProbeSideObservation::Skipped,
    };

    let observation = RunObservation {
        community_before: stats_before.community_count,
        community_after: stats_after
            .as_ref()
            .map_or(StatsObservation::Skipped, |stats| stats.community_count),
        merge: merge_status,
        probe_before_attempted: probe_before.attempted(),
        probe_before_succeeded: probe_before.succeeded(),
        probe_after: probe_after_observation,
    };
    let verdict = evaluate_run(observation);
    let (verdict_label, verdict_code, verdict_message) = match verdict {
        RunVerdict::Ok => ("ok", None, None),
        RunVerdict::Warn(warning) => ("warn", Some(warning.code()), Some(warning.message())),
        RunVerdict::Fatal(failure) => ("fatal", Some(failure.code()), Some(failure.message())),
    };

    let summary = json!({
        "schema": args.schema,
        "probe_only": args.probe_only,
        "stats_before": stats_before.json,
        "stats_after": stats_after.as_ref().map(|stats| stats.json.clone()),
        "merge": merge_json,
        "merge_elapsed_secs": elapsed_secs,
        "probe_before": probe_before.json,
        "probe_after": probe_after.as_ref().map(|probe| probe.json.clone()),
        "summary": {
            "community_count_delta": community_count_delta(
                observation.community_before,
                observation.community_after,
            ),
            // 同じ (query, mode) で両側とも成功したペアだけの差分。母数は
            // compared_pairs として同じオブジェクトに入る。
            "probe_counts_delta": probe_counts_delta(
                &probe_before.per_probe,
                probe_after.as_ref().map(|probe| probe.per_probe.as_slice()),
            )
            .map(ProbeCountsDelta::to_json),
            "probe_before_attempted": observation.probe_before_attempted,
            "probe_before_succeeded": observation.probe_before_succeeded,
            "probe_after_attempted": observation.probe_after.attempted_count(),
            "probe_after_succeeded": observation.probe_after.succeeded_count(),
            "verdict": verdict_label,
            "verdict_code": verdict_code,
            "verdict_message": verdict_message,
        },
    });
    // fail closed でも観測結果は捨てない。判定・exit code に関わらず同じ 1 箇所で出す
    // （Merge 失敗の早期 return を廃止し、出力経路を 1 本に統一した）。
    println!("{}", serde_json::to_string_pretty(&summary)?);

    match verdict {
        RunVerdict::Ok => Ok(()),
        RunVerdict::Warn(warning) => {
            tracing::warn!(verdict = warning.code(), "{}", warning.message());
            Ok(())
        }
        RunVerdict::Fatal(failure) => {
            tracing::error!(verdict = failure.code(), "{}", failure.message());
            Err(anyhow::anyhow!("{}", failure.message()))
        }
    }
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

    /// probe が全件成功した通常実行（Merge あり）の観測を組み立てるヘルパ。
    /// 各テストは「何を変えた結果その判定になるか」だけを書きたいので、
    /// 変えない軸（Merge 成功・probe 前後とも全件成功）をここに固定する。
    fn observation(before: StatsObservation, after: StatsObservation) -> RunObservation {
        let full = PROBE_QUERIES.len() * PROBE_MODES.len();
        RunObservation {
            community_before: before,
            community_after: after,
            merge: MergeStatus::Ok,
            probe_before_attempted: full,
            probe_before_succeeded: full,
            probe_after: ProbeSideObservation::Attempted {
                attempted: full,
                succeeded: full,
            },
        }
    }

    #[test]
    fn community_count_delta_needs_both_sides_observed() {
        assert_eq!(
            community_count_delta(StatsObservation::Value(0), StatsObservation::Value(42)),
            Some(42)
        );
        // 片側でも観測できていなければ「差分 0」ではなく「比較不能」。
        assert_eq!(
            community_count_delta(StatsObservation::Error, StatsObservation::Value(42)),
            None
        );
        assert_eq!(
            community_count_delta(StatsObservation::Value(42), StatsObservation::Skipped),
            None
        );
    }

    /// per-probe 集計の 1 マス。テストの意図（どのマスが欠測か）を読みやすくするヘルパ。
    fn hit(manual_section: usize, concept: usize, other: usize) -> Option<ProbeCounts> {
        Some(ProbeCounts {
            manual_section,
            concept,
            other,
        })
    }

    #[test]
    fn probe_totals_sums_only_successful_probes() {
        // 欠測（None）は 0 として足さない。母数は succeeded 側で別に出す。
        let totals = probe_totals(&[hit(3, 1, 0), None, hit(2, 0, 4)]);
        assert_eq!(totals.manual_section, 5);
        assert_eq!(totals.concept, 1);
        assert_eq!(totals.other, 4);
    }

    #[test]
    fn probe_counts_delta_compares_only_pairs_succeeded_on_both_sides() {
        // B1 初回実行の形: Merge 前の global（index 2）は FAILED_PRECONDITION で欠測し、
        // Merge 後は成功する。合算どうしを引くと「probe が 1 本増えた」分まで delta に
        // 混ざる（この例では manual_section が naive 集計だと +2 になり、符号すら反転する）。
        let before = vec![hit(5, 0, 0), hit(4, 1, 0), None];
        let after = vec![hit(3, 0, 2), hit(2, 1, 3), hit(6, 0, 1)];

        let delta =
            probe_counts_delta(&before, Some(&after)).expect("成功ペアがあるので比較できる");

        assert_eq!(
            delta.compared_pairs, 2,
            "before 側が欠測の global は母数に入れない"
        );
        assert_eq!(delta.manual_section, -4, "(3-5) + (2-4)");
        assert_eq!(delta.concept, 0);
        assert_eq!(delta.other, 5, "(2-0) + (3-0)");
        assert_eq!(
            delta.to_json()["compared_pairs"],
            2,
            "読み手が母数を検証できるよう JSON にも出す"
        );
    }

    #[test]
    fn probe_counts_delta_is_none_without_after_side() {
        assert_eq!(probe_counts_delta(&[hit(10, 2, 0)], None), None);
    }

    #[test]
    fn probe_counts_delta_is_none_when_no_pair_succeeded_on_both_sides() {
        // 欠測が互い違いで、同じ (query, mode) で両側そろったマスが 1 つも無い。
        // 合算どうしなら数字が出てしまうが、比較可能なペアが無い以上「差分 0」ではなく比較不能。
        let before = vec![hit(10, 2, 0), None];
        let after = vec![None, hit(1, 1, 8)];
        assert_eq!(probe_counts_delta(&before, Some(&after)), None);
        // 片側が全滅した場合も同じ（旧実装が succeeded == 0 で弾いていたケース）。
        assert_eq!(
            probe_counts_delta(&before, Some(&[None, None])),
            None,
            "after 側が全滅なら delta は null"
        );
    }

    #[test]
    fn probe_counts_delta_is_none_when_grids_are_misaligned() {
        // グリッドは固定なので長さは常に一致するはずだが、一致しないなら index 対応が
        // 崩れている。前方一致で辻褄を合わせず比較不能にする。
        let before = vec![hit(5, 0, 0), hit(4, 1, 0), hit(1, 0, 0)];
        let after = vec![hit(3, 0, 2), hit(2, 1, 3)];
        assert_eq!(probe_counts_delta(&before, Some(&after)), None);
    }

    #[test]
    fn probe_side_observation_all_failed_only_when_attempted_and_zero_succeeded() {
        assert!(ProbeSideObservation::Attempted {
            attempted: 5,
            succeeded: 0,
        }
        .all_failed());
        assert!(!ProbeSideObservation::Attempted {
            attempted: 5,
            succeeded: 1,
        }
        .all_failed());
        // attempted 0 は「そもそも回していない」であって「全滅」ではない。
        assert!(!ProbeSideObservation::Attempted {
            attempted: 0,
            succeeded: 0,
        }
        .all_failed());
        assert!(!ProbeSideObservation::Skipped.all_failed());
    }

    #[test]
    fn evaluate_run_accepts_community_growth() {
        let verdict = evaluate_run(observation(
            StatsObservation::Value(0),
            StatsObservation::Value(37),
        ));
        assert_eq!(verdict, RunVerdict::Ok);
    }

    #[test]
    fn evaluate_run_warns_when_community_count_did_not_move() {
        // グラフ不変の schema への再 Merge では増分 0 が正常。fail にはしない。
        let verdict = evaluate_run(observation(
            StatsObservation::Value(37),
            StatsObservation::Value(37),
        ));
        assert_eq!(
            verdict,
            RunVerdict::Warn(RunWarning::CommunityCountUnchanged)
        );
    }

    #[test]
    fn evaluate_run_fails_when_no_community_exists_after_merge() {
        // Merge を実行したのに 0 件＝真の空振り。before の観測状態によらず fail。
        assert_eq!(
            evaluate_run(observation(
                StatsObservation::Value(0),
                StatsObservation::Value(0)
            )),
            RunVerdict::Fatal(RunFailure::NoCommunities)
        );
        assert_eq!(
            evaluate_run(observation(
                StatsObservation::Error,
                StatsObservation::Value(0)
            )),
            RunVerdict::Fatal(RunFailure::NoCommunities)
        );
    }

    #[test]
    fn evaluate_run_fails_when_stats_unavailable_on_both_sides() {
        // Merge の成否を一切確認できていない状態を成功として返さない。
        assert_eq!(
            evaluate_run(observation(
                StatsObservation::Error,
                StatsObservation::Error
            )),
            RunVerdict::Fatal(RunFailure::StatsUnavailable)
        );
    }

    #[test]
    fn evaluate_run_warns_when_only_one_side_of_stats_failed() {
        assert_eq!(
            evaluate_run(observation(
                StatsObservation::Error,
                StatsObservation::Value(37)
            )),
            RunVerdict::Warn(RunWarning::StatsPartiallyUnavailable)
        );
        assert_eq!(
            evaluate_run(observation(
                StatsObservation::Value(37),
                StatsObservation::Error
            )),
            RunVerdict::Warn(RunWarning::StatsPartiallyUnavailable)
        );
    }

    #[test]
    fn evaluate_run_fails_when_before_side_probe_all_failed() {
        // stats が正常でも「before 側の観測ゼロ」は最優先で fail
        // （B1 の目的は実測 JSON を得ること。何も観測できていない実行を ok にしない）。
        let full = PROBE_QUERIES.len() * PROBE_MODES.len();
        let verdict = evaluate_run(RunObservation {
            community_before: StatsObservation::Value(0),
            community_after: StatsObservation::Value(37),
            merge: MergeStatus::Ok,
            probe_before_attempted: full,
            probe_before_succeeded: 0,
            probe_after: ProbeSideObservation::Attempted {
                attempted: full,
                succeeded: full,
            },
        });
        assert_eq!(verdict, RunVerdict::Fatal(RunFailure::ProbeAllFailed));
    }

    #[test]
    fn evaluate_run_fails_when_after_side_probe_all_failed_despite_merge_success() {
        // レビュー指摘の核心: Merge は成功し community_count も増えているのに、
        // after 側 probe が全滅している。これを before/after 合算で「succeeded > 0」と
        // 読んで ok にすると、B2 の分岐判断に使う実測 JSON が空のまま見逃される。
        let full = PROBE_QUERIES.len() * PROBE_MODES.len();
        let verdict = evaluate_run(RunObservation {
            community_before: StatsObservation::Value(0),
            community_after: StatsObservation::Value(37),
            merge: MergeStatus::Ok,
            probe_before_attempted: full,
            probe_before_succeeded: full,
            probe_after: ProbeSideObservation::Attempted {
                attempted: full,
                succeeded: 0,
            },
        });
        assert_eq!(verdict, RunVerdict::Fatal(RunFailure::ProbeAfterAllFailed));
    }

    #[test]
    fn evaluate_run_fails_when_merge_itself_failed() {
        // Merge RPC 自体がエラーを返した実行は、stats/probe の値によらず Fatal。
        // after 側は意図して観測していない（Skipped）。
        let full = PROBE_QUERIES.len() * PROBE_MODES.len();
        let verdict = evaluate_run(RunObservation {
            community_before: StatsObservation::Value(37),
            community_after: StatsObservation::Skipped,
            merge: MergeStatus::Failed,
            probe_before_attempted: full,
            probe_before_succeeded: full,
            probe_after: ProbeSideObservation::Skipped,
        });
        assert_eq!(verdict, RunVerdict::Fatal(RunFailure::MergeFailed));
    }

    #[test]
    fn evaluate_run_prefers_probe_all_failed_over_stats_unavailable_when_both_apply() {
        // before 側 probe が全滅、かつ stats が両側ともエラーという 2 条件が同時に成立する
        // ケース。判定順を入れ替えると StatsUnavailable が返り、「そもそも何も観測できて
        // いない」という一次原因が隠れてしまう。優先順位を固定するための回帰テスト。
        let full = PROBE_QUERIES.len() * PROBE_MODES.len();
        let verdict = evaluate_run(RunObservation {
            community_before: StatsObservation::Error,
            community_after: StatsObservation::Error,
            merge: MergeStatus::Ok,
            probe_before_attempted: full,
            probe_before_succeeded: 0,
            probe_after: ProbeSideObservation::Attempted {
                attempted: full,
                succeeded: full,
            },
        });
        assert_eq!(verdict, RunVerdict::Fatal(RunFailure::ProbeAllFailed));
    }

    #[test]
    fn evaluate_run_accepts_probe_only_run_without_after_side() {
        // --probe-only は after を「意図して省略」しただけで、エラーではない。
        let full = PROBE_QUERIES.len() * PROBE_MODES.len();
        let verdict = evaluate_run(RunObservation {
            community_before: StatsObservation::Value(37),
            community_after: StatsObservation::Skipped,
            merge: MergeStatus::Skipped,
            probe_before_attempted: full,
            probe_before_succeeded: full,
            probe_after: ProbeSideObservation::Skipped,
        });
        assert_eq!(verdict, RunVerdict::Ok);
    }

    #[test]
    fn evaluate_run_fails_probe_only_run_when_all_probes_failed() {
        // after のスキップより「before 観測ゼロ」の方が優先される。
        let full = PROBE_QUERIES.len() * PROBE_MODES.len();
        let verdict = evaluate_run(RunObservation {
            community_before: StatsObservation::Value(37),
            community_after: StatsObservation::Skipped,
            merge: MergeStatus::Skipped,
            probe_before_attempted: full,
            probe_before_succeeded: 0,
            probe_after: ProbeSideObservation::Skipped,
        });
        assert_eq!(verdict, RunVerdict::Fatal(RunFailure::ProbeAllFailed));
    }

    #[test]
    fn readiness_json_is_null_when_server_sent_none() {
        // 「readiness が来ていない」と「全 READY」を読み手が区別できるようにする。
        assert_eq!(readiness_json(None), Value::Null);
        assert_eq!(component_readiness_json(None), Value::Null);
    }

    #[test]
    fn readiness_json_names_known_states_and_keeps_unknown_numeric() {
        let readiness = SearchReadiness {
            local: Some(ComponentReadiness {
                state: ReadinessState::Ready as i32,
                reason: String::new(),
                revision: 7,
                ready_at_ms: 1_753_000_000_000,
            }),
            global: Some(ComponentReadiness {
                // proto に state が増えても数値を残して握りつぶさない。
                state: 9999,
                reason: "unknown to this build".to_string(),
                revision: 0,
                ready_at_ms: 0,
            }),
            community_summary: None,
            structural_vectors: None,
            similar_patterns: None,
        };
        let rendered = readiness_json(Some(&readiness));
        assert_eq!(rendered["local"]["state"], "READINESS_STATE_READY");
        assert_eq!(rendered["local"]["revision"], 7);
        assert_eq!(rendered["global"]["state"], "UNKNOWN(9999)");
        assert_eq!(
            rendered["global"]["reason"],
            Value::from("unknown to this build")
        );
        assert_eq!(rendered["community_summary"], Value::Null);
    }
}
