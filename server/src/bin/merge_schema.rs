//! vegapunk `Merge` RPC の実行と、その前後の観測を行う CLI（Issue #8 Phase B1）。
//!
//! Merge は **schema 全体の再計算・同期実行・admin ロール必須・同一 schema で同時 1 本のみ**で、
//! 応答は空（進捗もジョブ ID も返らない）。したがって「実行したか / 効いたか」は
//! `GetStats.community_count` の前後差で確認する。
//!
//! さらに **Phase C で `mode=hybrid` への切替（別記事 join）の可否を判断するための実測**を
//! Merge の前後で取る（`MENTIONS_CONCEPT` を辿る自前 concept-expansion は B2 として既に
//! 実装対象が確定しており、この実測の結果を待たない）。global / hybrid の返却物を JSON で出す。
//! 統合仕様書は global を「コミュニティ要約を検索し代表メンバーを返す」と書いているが、
//! proto の `SearchResultItem` にメンバー一覧フィールドは無く、ManualSection の node_id が
//! 返るかは実測しないと確定しない。B1 では `readiness.global` が READY にならず
//! （node2vec の job timeout で Merge が abort）、この判定自体が未実施のまま残っている。
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
    proto::graphrag::{
        ComponentReadiness, JobInfo, ListJobsResponse, ReadinessState, SearchReadiness,
    },
    vegapunk::{GrpcLimits, VegapunkClient},
};
use serde_json::{json, Map, Value};
use std::{
    env, fs,
    path::PathBuf,
    time::{Duration, Instant},
};

/// probe に使う日本語クエリ。**複数記事にまたがって答えが散る問い合わせ**を選ぶ
/// （single article で閉じるクエリだと、community 由来のヒットが出ても差が見えない）。
const PROBE_QUERIES: &[&str] = &[
    "カメラが夜だけ映らないのはなぜですか",
    "通知が届かないときに確認することは何ですか",
    "Wi-Fi を変更したあとに機器を再接続する手順を教えてください",
    "センサーの電池を交換する方法を教えてください",
];

/// probe で叩く検索 mode。`local` は基準線（Merge の影響を受けない）、
/// `global` は Merge 前だと FAILED_PRECONDITION が正常、`hybrid` が Phase C の hybrid
/// 切替可否判断の本命。
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
    /// Merge を実行せず観測だけ行う（実行前の状態確認、Phase C 検討時の再観測)。
    #[arg(long)]
    probe_only: bool,
    /// probe の top-k。
    #[arg(long, default_value_t = 10)]
    top_k: i32,
    /// Merge 失敗時・`--probe-only` 時に `ListJobs` で取得する直近ジョブの件数上限。
    /// vegapunk 側の上限（proto コメント: max 500）に合わせて検証する。
    #[arg(long, default_value_t = 50)]
    jobs_limit: i32,
    /// `ListJobs` を絞る時間窓（時間単位、既定 24）。`ListJobs` は schema を絞る手段が
    /// proto に無く cross-schema・job_type 無フィルタで返るため、窓を切って「見た期間」を
    /// 明示できるようにする。
    ///
    /// **窓を狭めても `--jobs-limit` の打ち切りは減らない。** `ListJobs` は `created_at DESC`
    /// でソートしてから `limit` を適用するので、目的のジョブを押し出せるのは**それより新しい
    /// ジョブだけ**であり、下限（`since_ms`）を上げても新しい側の競合は 1 件も減らない。
    /// 打ち切りが起きたかどうかは `recent_jobs` の `total_count` と `jobs` の長さの比較で見る。
    ///
    /// `0` を指定すると窓なし（全期間）。**インシデントから既定の 24 時間以上が経っている
    /// 場合は `0` か経過時間より大きい値を指定すること**（既定のままだと目的のジョブが窓外に
    /// 落ち、`jobs` が空なのを「失敗ジョブは無い」と誤読する）。
    #[arg(long, default_value_t = 24)]
    jobs_since_hours: u64,
}

/// probe で返ったヒットの種別。Phase C の hybrid 切替可否判断はこの内訳だけで決まる。
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
    /// この probe の `SearchExecution.degraded`。`execution` が返らなかった probe（サーバが
    /// 送ってこなかった場合）は `None`（「degraded ではない」と断定できないため、既定値
    /// `false` で埋めない）。`probe_counts_delta` の `degraded_pairs` はここが `Some(true)`
    /// の probe だけを数える。
    degraded: Option<bool>,
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

    /// 1 実行分（全クエリ × 全 mode）の合算。`degraded` はここでは合算しない
    /// （observation flag であって計数ではないため、複数 probe 分を足し合わせる意味を持たない）。
    /// 合算後の `self.degraded` は不定値として扱うこと。`to_json` がこのフィールドを
    /// 出力しない限りは無害だが、将来 `to_json` に足すときは合算不能である点に注意する。
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

/// probe 内訳の before→after 差分（**1 つの mode 内**）。`manual_section` が減って
/// `other` が増えるなら community 由来の item に top_k を食われている、という
/// Phase C（hybrid 切替可否判断）の判断材料。
/// `compared_pairs` は差分の母数（その mode で両側そろった `(query, mode)` の数）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ProbeCountsDelta {
    manual_section: i64,
    concept: i64,
    other: i64,
    compared_pairs: usize,
    /// `compared_pairs` のうち、before / after のどちらか片側でも
    /// `SearchExecution.degraded == true` だった件数。分母は `compared_pairs`。
    ///
    /// `docs/superpowers/specs/2026-07-27-phase-b-community-merge-design.md` の
    /// 「B1 実測 JSON の判読手順」節（`probe_after.entries[].execution` の `degraded` を
    /// 先に確認せず `probe_counts_delta.hybrid` の数値だけで判断すると生じる、と同節が
    /// 名指しする「最も危険な誤読」）が指すのは、`hybrid` が一度も本来の形（degrade せず）で
    /// 動いていないのに `probe_counts_delta.hybrid` の数値だけを見て「安全」と読むことだった。
    /// 以前はこれを確認するのに `probe_after.entries[].execution` を個別に開く必要があったが、
    /// この値を summary に併記することで、mode 別 delta を読むだけでその確認を完結できる。
    degraded_pairs: usize,
    /// `compared_pairs` のうち、`degraded_pairs` に数えなかった（＝ before/after のどちらも
    /// `Some(true)` ではなかった）ペアのうち、片側以上が `None`（execution 情報が無い）
    /// だったもの。
    ///
    /// **`degraded_pairs == 0` だけを見ると「degraded は無かった」と断定できてしまう。**
    /// vegapunk が `execution` を返さない実行では全 probe が `degraded: None` になり、
    /// `degraded_pairs` は必ず 0 になる。この 0 は「degraded ではないと確認できた」のではなく
    /// 「degraded かどうか一度も分からなかった」であり、`Option<bool>`（`ProbeCounts::degraded`）
    /// を導入した意図（「不明を false で埋めない」）が `to_json` の出力段で `0` という断定に
    /// 潰れてしまう。summary だけを読む運用者がこれを「hybrid は degrade せず動いた」と誤読する
    /// のは、spec の「B1 実測 JSON の判読手順」（3 番目の項目）が名指しする最も危険な誤読と
    /// 同じ構図である。`degraded_unknown_pairs` を併記することで、
    /// `compared_pairs = degraded_pairs + degraded_unknown_pairs + (判明していて degraded で
    /// はない残り)` の内訳が summary JSON だけで閉じるようにする。
    degraded_unknown_pairs: usize,
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
            "degraded_pairs": self.degraded_pairs,
            "degraded_unknown_pairs": self.degraded_unknown_pairs,
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

/// mode 別の before→after 差分。キーは `PROBE_MODES` の値で、その mode に比較可能ペアが
/// 1 つも無ければ `None`（JSON では `null`）。
///
/// **mode を横断して合算しない。** 3 つの mode は性質が違う: `local` は Merge の影響を
/// 受けない基準線、`global` は Merge 前が FAILED_PRECONDITION で全欠測（B1 初回実行では必ず
/// こうなる）、Phase C（hybrid 切替可否判断）の一次シグナルは `hybrid` の `manual_section`
/// 減少（community 由来の item に top_k を食われた）である。合算すると、基準線 `local` の
/// 偶然の増減が `hybrid` のシグナルを打ち消して隠しうる。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeCountsDeltaByMode {
    /// `PROBE_MODES` と同じ並びの `(mode, その mode の差分)`。
    per_mode: Vec<(&'static str, Option<ProbeCountsDelta>)>,
}

impl ProbeCountsDeltaByMode {
    fn to_json(&self) -> Value {
        let mut map = Map::with_capacity(self.per_mode.len());
        for (mode, delta) in &self.per_mode {
            // 比較不能な mode もキーごと消さない。キーが無いと読み手は
            // 「その mode を測っていない」のか「キー名を間違えた」のか区別できない。
            map.insert(
                (*mode).to_string(),
                delta.map_or(Value::Null, ProbeCountsDelta::to_json),
            );
        }
        Value::Object(map)
    }
}

/// probe 内訳の before→after 差分を、**同じ `(query, mode)` で両側とも成功したペアだけ**から
/// 取り、**mode ごとに分けて**返す。
///
/// `(query, mode)` のグリッドは `PROBE_QUERIES × PROBE_MODES` で固定（query-major /
/// mode-minor）なので、`per_probe` の index はそのまま probe の同一性を意味し、
/// `index % modes.len()` がその probe の mode になる。ここで index を捨てて合算どうしを引くと
/// **母数の異なる集計の引き算**になる。これは例外ケースではなく、B1 で最初に回す Merge 実行で
/// 確実に起きる: Merge 前の `global` は FAILED_PRECONDITION が正常なので before は 8 probe 分、
/// after は 12 probe 分の合算になり、差は「Merge の効果」ではなく「probe が 4 本増えた」を映す。
///
/// `--probe-only`（after 側が無い）、または index 対応が崩れている場合は、関数全体で
/// `None`（JSON では `probe_counts_delta` キー自体が `null`）を返す。
///
/// 一方、**mode 一覧・グリッドの形そのものは正常だが、どの mode にも両側そろったペアが
/// 1 つも無い**場合は、関数レベルの `None` にはしない。`Some(ProbeCountsDeltaByMode)` を
/// 返し、各 mode の値だけ `null` にする（`ProbeCountsDeltaByMode::to_json` が担う）。
/// 以前はここも関数レベルの `None` にしており、`summary.probe_counts_delta` 全体が
/// `null` になって mode キーごと消えていた。読み手はそれを「測っていない」のか
/// 「キー名を間違えた」のか JSON だけでは区別できない。mode キーを常に残すことで、
/// 「mode 一覧は分かっているが、比較可能なペアが無かった」ことを明示する。
fn probe_counts_delta(
    modes: &[&'static str],
    before: &[Option<ProbeCounts>],
    after: Option<&[Option<ProbeCounts>]>,
) -> Option<ProbeCountsDeltaByMode> {
    let after = after?;
    if modes.is_empty() {
        // 定数 PROBE_MODES が空になることは無いが、剰余演算が成立しない入力で
        // panic させない（判定は純関数として他所からも呼べる形にしてある）。
        tracing::error!("probe mode list is empty; cannot map a probe index to its mode");
        return None;
    }
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
    if before.len() % modes.len() != 0 {
        // グリッドが mode 数の倍数でないなら index → mode の写像が決まらない。
        // 適当に割り当てると「hybrid の差分」と称して別 mode の値を見せることになる。
        tracing::error!(
            grid_len = before.len(),
            mode_count = modes.len(),
            "probe grid is not a whole number of mode rows; refusing to attribute probes to modes"
        );
        return None;
    }
    let mut per_mode: Vec<(&'static str, Option<ProbeCountsDelta>)> =
        modes.iter().map(|mode| (*mode, None)).collect();
    for (index, (before, after)) in before.iter().zip(after.iter()).enumerate() {
        let (Some(before), Some(after)) = (before, after) else {
            continue;
        };
        // 最初の比較可能ペアが出た時点で None → Some に切り替える。ペアが 1 つも無い mode を
        // 差分 0 と区別するため、既定値で先に埋めておかない。
        let delta = per_mode[index % modes.len()]
            .1
            .get_or_insert_with(ProbeCountsDelta::default);
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
        // どちらか片側でも degraded なら数える。execution 情報が無い（None）probe は
        // 「degraded ではない」と断定できないため加算しない（過大にも過小にも倒さない）。
        if before.degraded == Some(true) || after.degraded == Some(true) {
            delta.degraded_pairs += 1;
        } else if before.degraded.is_none() || after.degraded.is_none() {
            // 「degraded ではないと判明した」のではなく「判定できなかった」。この件数を
            // `degraded_pairs` に混ぜない・かつ黙って消さない（`ProbeCountsDelta::degraded_unknown_pairs`
            // のコメント参照）。
            delta.degraded_unknown_pairs += 1;
        }
    }
    // 上の guard（modes 空 / グリッド長不一致 / grid が mode 数の倍数でない）をすべて
    // 通過した以上、per_mode は常に意味のある形（mode ごとに 1 エントリ）になっている。
    // 比較可能なペアが 1 つも無くても、mode キー一覧を持つ `Some` を返す。
    Some(ProbeCountsDeltaByMode { per_mode })
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

impl MergeStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Skipped => "skipped",
            Self::Ok => "ok",
            Self::Failed => "failed",
        }
    }
}

/// Merge 実行パスの結果。**判定・出力 JSON・exit 時のエラーをすべてここから作る**。
/// 経路ごとに JSON を手書きすると、キー構成が経路依存になり（`{"ok":true}` /
/// `{"ok":false,"error":..}` / `{"skipped":true}`）、後段ツールがキーの有無で分岐する羽目になる。
/// verdict と JSON がずれる余地も残る。
struct MergeReport {
    status: MergeStatus,
    /// Merge RPC が返した原エラー。`Failed` のときだけ `Some`。
    /// gRPC code 由来のヒントを exit 時のエラー行にも残すため、文字列化せず保持する。
    error: Option<anyhow::Error>,
    /// Merge を実際に呼んだときの所要秒。`Skipped` では `None`（0.0 と区別する）。
    elapsed_secs: Option<f64>,
}

impl MergeReport {
    fn skipped() -> Self {
        Self {
            status: MergeStatus::Skipped,
            error: None,
            elapsed_secs: None,
        }
    }

    fn ok(elapsed_secs: f64) -> Self {
        Self {
            status: MergeStatus::Ok,
            error: None,
            elapsed_secs: Some(elapsed_secs),
        }
    }

    fn failed(error: anyhow::Error, elapsed_secs: f64) -> Self {
        Self {
            status: MergeStatus::Failed,
            error: Some(error),
            elapsed_secs: Some(elapsed_secs),
        }
    }

    /// どの経路でも同じキー構成（`status` / `error`）にする。
    fn to_json(&self) -> Value {
        json!({
            "status": self.status.as_str(),
            "error": self.error.as_ref().map(|err| format!("{err:#}")),
        })
    }
}

/// Fatal 時に `main` が返すエラー。Merge 由来の原エラーがあるなら、**それを保持したまま**
/// verdict の説明を context として被せる。定型文へ置き換えると、Cloud Run job の失敗
/// サマリ（最終エラー行しか見えないことがある）に gRPC code 由来のヒントが届かない。
fn fatal_error(failure: RunFailure, merge_error: Option<anyhow::Error>) -> anyhow::Error {
    match merge_error {
        Some(err) => err.context(failure.message()),
        None => anyhow::anyhow!("{}", failure.message()),
    }
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
                "before 側の probe が 1 件も成功しなかった。この実行からは Phase C\
                 （hybrid 切替可否判断）の材料が何も得られていない。出力 JSON の\
                 probe_before の error（接続 / 権限 / schema 名）を確認する"
            }
            Self::MergeFailed => {
                "Merge の実行自体が失敗した。after 側の stats / probe は意図して取得していない\
                 （Merge 失敗直後の再呼び出しは避けている）。出力 JSON の merge.error（gRPC code\
                 由来のヒント込み）を確認する。stats_before / probe_before は取得済みなので\
                 Merge 前の状態把握には使える"
            }
            Self::ProbeAfterAllFailed => {
                "Merge は成功したのに after 側 probe が 1 件も成功しなかった。global/hybrid の\
                 実測 JSON が空で、Phase C の hybrid 切替可否判断（mode=hybrid だけで別記事 join が\
                 成立するか）に使える情報が無い。出力 JSON の probe_after の error を確認する。\
                 stats_after の community_count で Merge 自体の効果は別途確認できる"
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
    // 3. Merge は成功したのに after 側 probe が全滅。Phase C の実測 JSON が空になる致命的な欠落。
    if observation.probe_after.all_failed() {
        return RunVerdict::Fatal(RunFailure::ProbeAfterAllFailed);
    }
    // 4. stats が両側とも取れず、Merge の成否を一切確認できない。
    if observation.community_before.is_error() && observation.community_after.is_error() {
        return RunVerdict::Fatal(RunFailure::StatsUnavailable);
    }
    // 5. 真の空振り: **Merge を実行した**のに community が 0 件。before の観測状態は問わない。
    //    Merge を回していない実行（--probe-only）に当てると、観測専用実行が非 0 終了に化ける。
    if observation.merge == MergeStatus::Ok && observation.community_after.value() == Some(0) {
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

/// probe 1 周分の出力 JSON。合算のキー名は `totals_of_succeeded_probes` で、母数
/// （`succeeded` / `attempted`）を同じオブジェクトに並べる。**単に `totals` とすると
/// JSON の読み手に母数が見えず**、before/after で母数が違うこと（初回実行では
/// before 8 probe 分・after 12 probe 分）に気づかないまま
/// `.probe_after.totals - .probe_before.totals` を計算できてしまう。
/// ソースコメントの注意書きは JSON の読み手には届かない。mode 別の正しい差分は
/// `summary.probe_counts_delta` にある。
fn probe_summary_json(
    label: &str,
    entries: Vec<Value>,
    per_probe: &[Option<ProbeCounts>],
) -> Value {
    json!({
        "label": label,
        "succeeded": probe_succeeded(per_probe),
        "attempted": per_probe.len(),
        "totals_of_succeeded_probes": probe_totals(per_probe).to_json(),
        "entries": entries,
    })
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

/// `probe()` が叩く `(query, mode)` の実行グリッド。**query-major / mode-minor**
/// （同じ query に対して `PROBE_MODES` を全部回してから次の query へ進む）順で並べる。
///
/// `probe_counts_delta` の index → mode 写像（`index % modes.len()`）はこの並びを
/// 前提にしている。生成をここ 1 箇所に閉じることで、`probe()` 本体とテスト
/// （`probe_grid_matches_query_major_mode_minor_order`）の両方が同じグリッドを見る。
/// 以前は `probe()` 内の nested for loop が唯一の実装で、テストは
/// `(a*b) % b == 0` という順序に依存しない恒真式しか検証しておらず、
/// このループの入れ替え（mode-major への変更等）を検出できなかった。
fn probe_grid() -> Vec<(&'static str, &'static str)> {
    PROBE_QUERIES
        .iter()
        .flat_map(|query| PROBE_MODES.iter().map(move |mode| (*query, *mode)))
        .collect()
}

/// 全 PROBE_QUERIES × PROBE_MODES を叩き、ヒットの種別内訳・上位サンプル・SearchExecution を
/// JSON に残す。**エラー（Merge 前の global = FAILED_PRECONDITION 等）は記録して次へ進む**
/// （前後差を取るのが目的で、片方のエラーで観測全体を落とさない）。
async fn probe(client: &VegapunkClient, args: &Args, label: &str) -> ProbeOutcome {
    let mut entries = Vec::new();
    // グリッドの 1 マスにつき必ず 1 要素 push する（成功は Some、失敗は None）。
    // index が (query, mode) の同一性を担保するので、途中で push を飛ばさないこと。
    let mut per_probe: Vec<Option<ProbeCounts>> = Vec::new();
    for (query, mode) in probe_grid() {
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
                let mut counts = ProbeCounts::from_kinds(&kinds);
                // execution が返らなかった probe は degraded を「不明」のままにする
                // （`ProbeCounts::degraded` のコメント参照。既定値 false で埋めない）。
                counts.degraded = outcome
                    .execution
                    .as_ref()
                    .map(|execution| execution.degraded);
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
    let json = probe_summary_json(label, entries, &per_probe);
    ProbeOutcome { json, per_probe }
}

/// 診断優先度の 2 段キー。第 1 キー: `error` が非空か（非空が最優先）。第 2 キー:
/// `status` が `completed` 以外か（`completed` 以外が次点）。タプルは辞書式に比較されるため、
/// このタプルの昇順ソートがそのまま 2 段階の優先順位になる（`false < true`）。
///
/// 1 段キー（「error 非空 OR status != completed」を 1 bit に潰す）だと、他 schema で
/// ingest が並走しているときの running/pending ジョブ（error 無し）が、より古い
/// failed（error あり）と同じ優先度タイルに入る。安定ソートはタイル内で
/// `created_at DESC` を保つため、新しい running ジョブが古い failed より先頭に来て
/// しまい、本当に見るべき失敗ジョブが埋もれる。error の有無を独立した第 1 キーにすることで
/// この事故を防ぐ。
fn diagnostic_priority(job: &JobInfo) -> (bool, bool) {
    let has_error = job.error.as_deref().is_some_and(|error| !error.is_empty());
    (!has_error, job.status == "completed")
}

/// `ListJobs` が返した順（vegapunk 申告: `created_at` DESC）を保ったまま、診断で先に
/// 見るべきジョブを先頭に寄せる。[`diagnostic_priority`] の 2 段キーでグループを作り、
/// `Vec::sort_by_key` の安定性でグループ内の順序（= created_at DESC）を保つ。
fn sort_jobs_diagnostic_first(mut jobs: Vec<JobInfo>) -> Vec<JobInfo> {
    jobs.sort_by_key(diagnostic_priority);
    jobs
}

/// 出力 JSON に載せる `JobInfo` の射影。`msg_id` は運用者が Merge 失敗の原因特定に使う
/// 情報（job_id / job_type / status / error / created_at / completed_at / retry_count）に
/// 含まれないため意図して落とす。
fn job_info_json(job: &JobInfo) -> Value {
    json!({
        "job_id": job.job_id,
        "job_type": job.job_type,
        "status": job.status,
        "error": job.error,
        "created_at": job.created_at,
        "completed_at": job.completed_at,
        "retry_count": job.retry_count,
    })
}

/// ログに個別出力する失敗ジョブの上限。`--probe-only` は日常的な観測コマンドなので、
/// 実行のたびに過去の failed ジョブ（最大 `--jobs-limit` 件、既定 50・上限 500）が
/// 丸ごと ERROR ログへ出ると Cloud Run のエラー集計を汚し、「今回の失敗」と
/// 「以前から残っている失敗」の区別が付かなくなる。先頭 5 件だけ個別に出し、
/// 残りは件数のサマリ行にする。
const FAILED_JOB_LOG_LIMIT: usize = 5;

/// `jobs` から「error が非空」なものだけを抜き出し、ログに個別出力する先頭
/// [`FAILED_JOB_LOG_LIMIT`] 件と、そこから溢れた残数に分ける。ログ出力そのもの
/// （`tracing::warn!` / `tracing::error!`）と分離することで、5 件キャップと残数計算を
/// トレーシング基盤なしに単体テストできる。
fn split_failed_jobs_for_logging(jobs: &[JobInfo]) -> (Vec<&JobInfo>, usize) {
    let failed: Vec<&JobInfo> = jobs
        .iter()
        .filter(|job| job.error.as_deref().is_some_and(|error| !error.is_empty()))
        .collect();
    let remaining = failed.len().saturating_sub(FAILED_JOB_LOG_LIMIT);
    let head = failed.into_iter().take(FAILED_JOB_LOG_LIMIT).collect();
    (head, remaining)
}

/// 失敗ジョブ（`error` が非空）をログに出す。`routine` が `true`（`--probe-only`、日常観測）
/// なら `warn!`、`false`（Merge 失敗直後の診断）なら従来どおり `error!` にする。
/// 先頭 [`FAILED_JOB_LOG_LIMIT`] 件だけ個別に出し、残りは件数だけのサマリ行にする。
fn log_failed_jobs(jobs: &[JobInfo], routine: bool) {
    let (head, remaining) = split_failed_jobs_for_logging(jobs);
    for job in &head {
        let error = job.error.as_deref().unwrap_or_default();
        if routine {
            tracing::warn!(job_id = %job.job_id, error = %error, "vegapunk job reported an error");
        } else {
            tracing::error!(job_id = %job.job_id, error = %error, "vegapunk job reported an error");
        }
    }
    if remaining > 0 {
        if routine {
            tracing::warn!(
                remaining,
                "... and {remaining} more failed jobs; see recent_jobs in the summary JSON"
            );
        } else {
            tracing::error!(
                remaining,
                "... and {remaining} more failed jobs; see recent_jobs in the summary JSON"
            );
        }
    }
}

/// `list_jobs` の `Result` から `recent_jobs` フィールドの中身（JSON オブジェクト）を組み立てる。
/// 経路（成功 / RPC 失敗 / タイムアウト）によらず**同じキー集合**を返す — この CLI の規約
/// （`merge_report_json_keeps_the_same_shape_on_every_path` と同じ）に、この関数追加時点では
/// `recent_jobs` だけが違反していた（成功時は配列、失敗時はオブジェクト）ため揃える:
///
/// - 成功時: `{"jobs": [...], "error": null, "total_count": N, "since_ms": <適用した窓 or null>}`
/// - 失敗時（RPC エラー・タイムアウトとも同じ形）:
///   `{"jobs": [], "error": "...", "total_count": null, "since_ms": <適用した窓 or null>}`
///
/// `total_count`（post-filter・pre-pagination の全件数）を残すのは、「`jobs`（`limit` 件で
/// 打ち切り）に収まりきらなかった件数がどれだけあるか」を見るため。**ただし `ListJobs` は
/// schema を絞る手段が proto に無く cross-schema・job_type 無フィルタのままなので、
/// `total_count` は「その時間窓に入った全 schema のジョブ数」であり、「目的のジョブが
/// 窓外に落ちたか」までは分からない**（それを見分けるには対象 schema 専用の絞り込みが要る
/// が proto に無い）。`since_ms` を時間窓として渡すことで、少なくとも「どの期間を見て
/// どの期間を見ていないか」は summary JSON だけで分かるようにする（呼び出し元の
/// `--jobs-since-hours`、既定 24h・`0` で窓なし）。
///
/// `routine` は呼び出し文脈（`true` = `--probe-only` の日常観測、`false` = Merge 失敗直後の
/// 診断）。成功時は失敗ジョブのログレベル（[`log_failed_jobs`] 参照）、失敗時は
/// `list_jobs` 自体の失敗ログのレベルを、どちらもこのフラグで切り替える。`--probe-only` は
/// 毎回の日常観測なので `list_jobs` の失敗（権限不足・タイムアウト等）を `warn!` に留め、
/// Merge 失敗直後の診断では引き続き `error!` にする。
fn recent_jobs_value(
    result: Result<ListJobsResponse>,
    since_ms: Option<i64>,
    routine: bool,
) -> Value {
    match result {
        Ok(resp) => {
            let jobs = sort_jobs_diagnostic_first(resp.jobs);
            log_failed_jobs(&jobs, routine);
            json!({
                "jobs": Value::Array(jobs.iter().map(job_info_json).collect()),
                "error": null,
                "total_count": resp.total_count,
                "since_ms": since_ms,
            })
        }
        Err(err) => {
            let error = format!("{err:#}");
            if routine {
                tracing::warn!(
                    error = %error,
                    "list_jobs failed; recent_jobs diagnostics unavailable"
                );
            } else {
                tracing::error!(
                    error = %error,
                    "list_jobs failed; recent_jobs diagnostics unavailable"
                );
            }
            json!({
                "jobs": [],
                "error": error,
                "total_count": null,
                "since_ms": since_ms,
            })
        }
    }
}

/// `--jobs-since-hours` を `ListJobs.since_ms` の下限（epoch ms）に変換する純関数。
/// `jobs_since_hours == 0` は「窓なし」（`None` = 全期間、旧来の挙動）。
/// `now_ms` を引数として受け取ることで、`SystemTime::now()` をこの判定ロジックに
/// 埋め込まずに済み、決定論的にテストできる（呼び出し元は [`now_epoch_ms`] を渡す）。
fn since_ms_from_hours(jobs_since_hours: u64, now_ms: i64) -> Option<i64> {
    if jobs_since_hours == 0 {
        return None;
    }
    const MS_PER_HOUR: i64 = 3_600_000;
    let window_ms = i64::try_from(jobs_since_hours)
        .unwrap_or(i64::MAX)
        .saturating_mul(MS_PER_HOUR);
    Some(now_ms.saturating_sub(window_ms))
}

/// 現在時刻を epoch ms で返す薄いラッパ。`SystemTime::now()` の評価をここ 1 箇所に閉じ、
/// 判定ロジック（[`since_ms_from_hours`]）を純関数のまま保つ。
fn now_epoch_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// `recent_jobs` フィールドの中身を組み立てる。Merge 失敗時・`--probe-only` 時にだけ呼ぶ
/// （呼び出し側の判断）。`list_jobs` 自体が失敗しても Merge の判定・exit code は変えない
/// ため、ここでは `Result` を返さず [`recent_jobs_value`] の失敗形に畳んで続行する
/// （診断が取れないことを黙って隠さない）。
///
/// `list_jobs` 呼び出しには `merge_cli_limits` の per-request timeout（既定 6h）とは独立に
/// 60 秒の上限を設ける。`list_jobs` は本来ミリ秒級の read で、6h を継承すると
/// 「診断呼び出しがサーバ無応答で最長 6 時間ブロックし、summary JSON も verdict も
/// exit code も出ないまま job の task-timeout に食われる」事故になる
/// （Merge が既に失敗している経路で、取得済みの stats_before / probe_before ごと
/// 全観測を失う）。
async fn recent_jobs_json(
    client: &VegapunkClient,
    limit: i32,
    since_ms: Option<i64>,
    routine: bool,
) -> Value {
    const LIST_JOBS_TIMEOUT: Duration = Duration::from_secs(60);
    let result = match tokio::time::timeout(
        LIST_JOBS_TIMEOUT,
        client.list_jobs(None, since_ms, limit),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "list_jobs timed out after {}s (this diagnostic call is bounded independently of \
             --timeout-secs / merge_cli_limits' long Merge timeout; vegapunk did not respond in time)",
            LIST_JOBS_TIMEOUT.as_secs()
        )),
    };
    recent_jobs_value(result, since_ms, routine)
}

/// この CLI 専用の gRPC 上限。**h2 PING keepalive を無効にする**のがここの本質。
///
/// なぜ無効にするか: Merge は schema 全体の同期再計算（Leiden + LLM 要約 + Node2Vec）で、
/// その間サーバは h2 PING に応答できない。常駐サーバ向けの既定
/// （interval 30s + timeout 10s）のままだと、**正常に走っている Merge を 40 秒で切断する**。
/// 本番 Cloud Run job での実測:
/// `Unavailable, message: "http2 error", ... keep-alive timed out` / `elapsed_secs=40.0`。
/// このとき Merge が失敗したのではなく、こちら側が接続を切っていた。
/// 一発の長時間 RPC を投げるだけの CLI に、常駐サーバ向けの死活検知は要らない。
///
/// トレードオフ: 接続が本当に死んだ場合、この CLI は per-request timeout
/// （`--timeout-secs`、既定 6h）まで気づけない。Cloud Run job の `--task-timeout` を 7h
/// （= `--timeout-secs` より長く）に取ってある前提で、ハングは job 側で検出できる。
/// TCP keepalive（60s）は `build_endpoint` 側で有効なままなので、OS 層の検知は残る。
/// ただし検知までの遅延は keepalive の設定値（60s）そのものではない。h2 PING keepalive
/// （無効化前）なら約 40s で死活に気づけていたのに対し、TCP keepalive は Linux では
/// `tcp_keepalive(60s)` が `TCP_KEEPIDLE=60` を設定するだけで、既定の
/// `tcp_keepalive_probes=9` / `tcp_keepalive_intvl=75s` と合わせると
/// 死判定は `60 + 9 * 75 = 735s`（約 12 分）後になる。「60 秒で気づける」という
/// 読みは誤りで、実際は h2 なら 40 秒、TCP keepalive では約 12 分。
fn merge_cli_limits(timeout_secs: u64) -> GrpcLimits {
    GrpcLimits {
        timeout_secs,
        keep_alive: None,
        ..GrpcLimits::default()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    anyhow::ensure!(
        (1..=500).contains(&args.jobs_limit),
        "--jobs-limit must be in 1..=500 (vegapunk ListJobs upper bound), got {}",
        args.jobs_limit
    );
    let token = read_token(&args)?;
    let limits = merge_cli_limits(args.timeout_secs);
    let client = VegapunkClient::connect_with_limits(&args.endpoint, &token, limits)
        .await
        .context("connect vegapunk")?;

    let stats_before = stats_json(&client, &args.schema, "before").await;
    let probe_before = probe(&client, &args, "before").await;

    // `ListJobs` に適用する時間窓。呼び出し直前ではなく実行開始時点の 1 回だけ評価する
    // （Merge 自体が長時間かかるため、診断時に「now」を取り直すと窓の意味がぶれる）。
    let jobs_since_ms = since_ms_from_hours(args.jobs_since_hours, now_epoch_ms());

    // after 側（stats + probe）を取るかどうかは Merge の結果と一体。判断を 1 箇所に閉じ、
    // `--probe-only` や Merge 失敗時に after を観測してしまう分岐漏れを防ぐ。
    // **出力 JSON の形はどのパスでも同じにする**（後段ツールが `.summary.verdict` を読むときに
    // パスによってスキーマが変わらないようにする。以前は Merge 失敗の早期 return が
    // 別スキーマの JSON を出しており、`stats_after` 等が丸ごと欠落していた）。
    // recent_jobs は Merge 失敗時・--probe-only 時にだけ取得する（診断が要る場面に限る）。
    // Merge が成功した通常実行では取得せず null にする（`stats_after` 等と同じ
    // 「経路で JSON の形は揃えるが、値は意図して null にする」規約）。
    let (merge_report, stats_after, probe_after, recent_jobs) = if args.probe_only {
        // Merge だけでなく after 側の観測もまとめてスキップする。同一条件の probe を
        // 2 周させても本番 vegapunk に無駄な負荷をかけるだけで、JSON の読み手には
        // 「before/after に差がある」と誤読させる材料にしかならない。
        tracing::info!("--probe-only: skipping Merge and the after-side observation");
        // routine = true: --probe-only は日常的な観測コマンドなので、失敗ジョブは warn に
        // 留める（recent_jobs_json 参照）。
        let recent_jobs = recent_jobs_json(&client, args.jobs_limit, jobs_since_ms, true).await;
        (MergeReport::skipped(), None, None, Some(recent_jobs))
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
                    MergeReport::ok(elapsed),
                    Some(stats_after),
                    Some(probe_after),
                    None,
                )
            }
            Err(err) => {
                // after 側の stats/probe は意図して観測しない（FAILED_PRECONDITION は
                // 「同一 schema で Merge が同時実行中」の可能性があり、失敗直後に追い打ちで
                // 叩くのはノイズにしかならない）。一方 recent_jobs は「なぜ落ちたか」を知る
                // 唯一の経路なので、こちらは取得する。
                tracing::error!(error = %format!("{err:#}"), elapsed_secs = elapsed, "Merge failed");
                // routine = false: Merge 失敗直後の診断なので、失敗ジョブは error のままにする。
                let recent_jobs =
                    recent_jobs_json(&client, args.jobs_limit, jobs_since_ms, false).await;
                (
                    MergeReport::failed(err, elapsed),
                    None,
                    None,
                    Some(recent_jobs),
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
        merge: merge_report.status,
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
        "merge": merge_report.to_json(),
        "merge_elapsed_secs": merge_report.elapsed_secs,
        "probe_before": probe_before.json,
        "probe_after": probe_after.as_ref().map(|probe| probe.json.clone()),
        // Merge 失敗時・--probe-only 時にだけ取得する診断情報（vegapunk ListJobs）。
        // 通常の Merge 成功パスでは null（意図した省略。取得失敗ではない）。
        "recent_jobs": recent_jobs,
        "summary": {
            "community_count_delta": community_count_delta(
                observation.community_before,
                observation.community_after,
            ),
            // 同じ (query, mode) で両側とも成功したペアだけの差分を、mode ごとに分けて出す。
            // 母数は各 mode の compared_pairs として同じオブジェクトに入る。
            // mode 横断で合算すると、Merge 非感受の local の増減が hybrid のシグナルを隠す。
            "probe_counts_delta": probe_counts_delta(
                PROBE_MODES,
                &probe_before.per_probe,
                probe_after.as_ref().map(|probe| probe.per_probe.as_slice()),
            )
            .map(|by_mode| by_mode.to_json()),
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
            Err(fatal_error(failure, merge_report.error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_cli_limits_disable_h2_keepalive_and_honor_timeout_arg() {
        // Merge は schema 全体の同期再計算で、その間サーバは h2 PING に応答できない。
        // 既定の keepalive（interval 30s + timeout 10s）のままだと、正常に走っている
        // Merge を 40 秒で切断する（本番実測）。この CLI では必ず無効であること。
        let limits = merge_cli_limits(21_600);
        assert!(
            limits.keep_alive.is_none(),
            "merge_schema CLI は h2 PING keepalive を張らない"
        );
        assert_eq!(
            limits.timeout_secs, 21_600,
            "--timeout-secs がそのまま per-request timeout になる"
        );
        assert_eq!(
            limits.max_decode_bytes,
            GrpcLimits::default().max_decode_bytes,
            "decode 上限は既定のまま（keepalive 以外を触らない）"
        );
    }

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
    /// `degraded` は「不明」（`None`）で埋める。degraded_pairs を検証したいテストは
    /// [`hit_degraded`] を使う。
    fn hit(manual_section: usize, concept: usize, other: usize) -> Option<ProbeCounts> {
        Some(ProbeCounts {
            manual_section,
            concept,
            other,
            ..Default::default()
        })
    }

    /// [`hit`] に `SearchExecution.degraded` の観測値を足した版。
    /// `degraded_pairs`（before/after のどちらか片側でも degraded だったペア数）の
    /// テスト専用。
    fn hit_degraded(
        manual_section: usize,
        concept: usize,
        other: usize,
        degraded: bool,
    ) -> Option<ProbeCounts> {
        Some(ProbeCounts {
            manual_section,
            concept,
            other,
            degraded: Some(degraded),
        })
    }

    #[test]
    fn probe_summary_json_labels_totals_with_their_denominator() {
        // 合算は「成功した probe だけ」の値で、母数は実行ごとに変わる（初回実行では
        // before 8 / after 12）。キー名に母数を書いておかないと、読み手が
        // `.probe_after.totals - .probe_before.totals` のワンライナーで母数の違う
        // 集計どうしを引き算し、Merge の効果と称した捏造値を作れてしまう。
        let per_probe = vec![hit(3, 1, 0), None, hit(2, 0, 4)];
        let rendered = probe_summary_json("before", Vec::new(), &per_probe);

        assert_eq!(rendered["succeeded"], 2);
        assert_eq!(rendered["attempted"], 3);
        assert_eq!(rendered["totals_of_succeeded_probes"]["manual_section"], 5);
        assert_eq!(rendered["totals_of_succeeded_probes"]["concept"], 1);
        assert_eq!(rendered["totals_of_succeeded_probes"]["other"], 4);
        assert_eq!(
            rendered["totals"],
            Value::Null,
            "母数の読めない旧キーを残さない（両方あると読み手が古い方を使える）"
        );
    }

    #[test]
    fn probe_totals_sums_only_successful_probes() {
        // 欠測（None）は 0 として足さない。母数は succeeded 側で別に出す。
        let totals = probe_totals(&[hit(3, 1, 0), None, hit(2, 0, 4)]);
        assert_eq!(totals.manual_section, 5);
        assert_eq!(totals.concept, 1);
        assert_eq!(totals.other, 4);
    }

    /// テスト用の mode グリッド。実際の `PROBE_MODES` と同じ並び（probe が
    /// query-major / mode-minor で回すので、index % modes.len() が mode を決める）。
    const TEST_MODES: &[&str] = &["local", "hybrid", "global"];

    /// mode 別 delta を名前で引くヘルパ。「そのキーが存在すること」も同時に確かめる
    /// （キーが落ちていれば `expect` で落ちる）。
    fn delta_of(by_mode: &ProbeCountsDeltaByMode, mode: &str) -> Option<ProbeCountsDelta> {
        by_mode
            .per_mode
            .iter()
            .find(|(name, _)| *name == mode)
            .map(|(_, delta)| *delta)
            .unwrap_or_else(|| panic!("mode {mode} のキーが無い: {by_mode:?}"))
    }

    #[test]
    fn probe_counts_delta_keys_each_mode_separately() {
        // B1 初回実行の形（2 クエリ × 3 mode）: Merge 前の global は FAILED_PRECONDITION で
        // 欠測し、Merge 後だけ成功する。mode を横断合算すると、Merge 非感受の local の
        // 増減が hybrid の manual_section 減少（Phase C の一次シグナル）を打ち消しうる。
        let before = vec![
            // query 1: local, hybrid, global
            hit(5, 0, 0),
            hit(5, 0, 0),
            None,
            // query 2: local, hybrid, global
            hit(4, 1, 0),
            hit(4, 1, 0),
            None,
        ];
        let after = vec![
            hit(5, 0, 0),
            hit(3, 0, 2),
            hit(6, 0, 1),
            hit(4, 1, 0),
            hit(2, 1, 3),
            hit(6, 0, 1),
        ];

        let by_mode = probe_counts_delta(TEST_MODES, &before, Some(&after))
            .expect("成功ペアがあるので比較できる");

        let local = delta_of(&by_mode, "local").expect("local は両側そろっている");
        assert_eq!(local.compared_pairs, 2);
        assert_eq!(
            local.manual_section, 0,
            "local は Merge の影響を受けない基準線"
        );
        assert_eq!(local.other, 0);
        assert_eq!(
            local.degraded_pairs, 0,
            "hit() は degraded を観測していない（None）ので加算されない"
        );
        assert_eq!(
            local.degraded_unknown_pairs, 2,
            "両側とも hit() = degraded 不明。「degraded ではない」と断定できないので不明分に計上する"
        );

        let hybrid = delta_of(&by_mode, "hybrid").expect("hybrid は両側そろっている");
        assert_eq!(hybrid.compared_pairs, 2);
        assert_eq!(hybrid.manual_section, -4, "(3-5) + (2-4)");
        assert_eq!(hybrid.concept, 0);
        assert_eq!(hybrid.other, 5, "(2-0) + (3-0)");
        assert_eq!(hybrid.degraded_pairs, 0);
        assert_eq!(hybrid.degraded_unknown_pairs, 2);

        assert_eq!(
            delta_of(&by_mode, "global"),
            None,
            "before 側が全欠測の global は比較不能（0 として見せない）"
        );
    }

    #[test]
    fn probe_counts_delta_degraded_pairs_counts_either_side_degraded() {
        // spec の「最も危険な誤読」対策: hybrid が degrade したまま動いていたことを
        // summary だけで確認できるようにする。before/after のどちらか片側でも degraded なら
        // 数え、両側とも degraded ではない（Some(false)）、または不明（None）なら数えない。
        let before = vec![
            hit_degraded(5, 0, 0, false), // local: 両側 not degraded（判明）
            hit_degraded(5, 0, 0, true),  // hybrid: before だけ degraded
            hit(1, 0, 0),                 // global: degraded 不明（None）のまま
        ];
        let after = vec![
            hit_degraded(5, 0, 0, false),
            hit_degraded(3, 0, 2, false), // hybrid: after は degraded 解消
            hit(1, 0, 0),
        ];

        let by_mode = probe_counts_delta(TEST_MODES, &before, Some(&after))
            .expect("全 mode で比較可能ペアがある");

        let local = delta_of(&by_mode, "local").unwrap();
        assert_eq!(
            local.degraded_pairs, 0,
            "両側とも degraded=false なら加算しない"
        );
        assert_eq!(
            local.degraded_unknown_pairs, 0,
            "両側とも degraded の値が判明している（Some(false)）ので不明分にも入らない"
        );

        let hybrid = delta_of(&by_mode, "hybrid").unwrap();
        assert_eq!(
            hybrid.degraded_pairs, 1,
            "before 側だけでも degraded=true なら片側分として数える"
        );
        assert_eq!(
            hybrid.degraded_unknown_pairs, 0,
            "degraded=true と判明している以上、不明分ではない"
        );

        let global = delta_of(&by_mode, "global").unwrap();
        assert_eq!(
            global.degraded_pairs, 0,
            "degraded が不明（None）は degraded=true と断定しないので加算しない"
        );
        assert_eq!(
            global.degraded_unknown_pairs, 1,
            "W1: degraded_pairs=0 だけでは「degraded 無し」と「一度も分からなかった」を\
             summary JSON だけで区別できない。この不明分を可視化するのが degraded_unknown_pairs"
        );
    }

    #[test]
    fn probe_counts_delta_json_lists_every_mode_with_null_for_incomparable_ones() {
        // 読み手が「どの mode の話か」と「その mode の母数」を JSON だけで確認できること。
        let before = vec![hit(5, 0, 0), hit(5, 0, 0), None];
        let after = vec![hit(5, 0, 0), hit(3, 0, 2), hit(6, 0, 1)];
        let rendered = probe_counts_delta(TEST_MODES, &before, Some(&after))
            .expect("成功ペアがある")
            .to_json();

        assert_eq!(rendered["local"]["manual_section"], 0);
        assert_eq!(rendered["local"]["compared_pairs"], 1);
        assert_eq!(rendered["local"]["degraded_pairs"], 0);
        assert_eq!(rendered["local"]["degraded_unknown_pairs"], 1);
        assert_eq!(rendered["hybrid"]["manual_section"], -2);
        assert_eq!(rendered["hybrid"]["other"], 2);
        assert_eq!(rendered["hybrid"]["compared_pairs"], 1);
        assert_eq!(rendered["hybrid"]["degraded_pairs"], 0);
        assert_eq!(rendered["hybrid"]["degraded_unknown_pairs"], 1);
        assert_eq!(
            rendered["global"],
            Value::Null,
            "比較不能な mode はキーごと消さず null で残す"
        );
    }

    #[test]
    fn probe_counts_delta_json_renders_degraded_pairs_and_degraded_unknown_pairs_distinctly() {
        // reviewer 指摘の再発防止: degraded_pairs と degraded_unknown_pairs はキー名が近く、
        // struct レベルのテストだけだと JSON 化の際にキーの取り違え（お互いの値を入れ違える）
        // や typo を見逃しうる。両方を非ゼロにして JSON 経由で個別に検証する。
        //
        // 1 mode に 3 ペアを集約させ、`compared_pairs = degraded_pairs + degraded_unknown_pairs
        // + (判明していて degraded ではない残り)` の内訳が summary JSON だけで閉じることも
        // 併せて確認する（W1: compared_pairs だけでは「判明」と「不明」の内訳が読めない）。
        const ONE_MODE: &[&str] = &["only"];
        let before = vec![
            hit_degraded(1, 0, 0, true), // pair 1: before が degraded=true → 既知の degraded
            hit(1, 0, 0),                // pair 2: 両側とも execution 情報が無い → 不明
            hit_degraded(1, 0, 0, false), // pair 3: 両側とも degraded=false → 判明していて安全
        ];
        let after = vec![
            hit_degraded(1, 0, 0, false),
            hit(1, 0, 0),
            hit_degraded(1, 0, 0, false),
        ];

        let rendered = probe_counts_delta(ONE_MODE, &before, Some(&after))
            .expect("比較可能ペアがある")
            .to_json();

        assert_eq!(rendered["only"]["compared_pairs"], 3);
        assert_eq!(
            rendered["only"]["degraded_pairs"], 1,
            "pair 1 だけが既知の degraded"
        );
        assert_eq!(
            rendered["only"]["degraded_unknown_pairs"], 1,
            "pair 2 は両側とも execution 情報が無く degraded かどうか不明"
        );
        // pair 3（判明していて degraded ではない残り 1 件）は compared_pairs から
        // degraded_pairs / degraded_unknown_pairs を引いた差分として summary JSON だけで
        // 復元できる。専用フィールドを持たないのは意図的（W1 の対応範囲は「不明」の可視化）。
        let compared = rendered["only"]["compared_pairs"].as_u64().unwrap();
        let degraded = rendered["only"]["degraded_pairs"].as_u64().unwrap();
        let unknown = rendered["only"]["degraded_unknown_pairs"].as_u64().unwrap();
        assert_eq!(
            compared - degraded - unknown,
            1,
            "残り 1 件（pair 3）は判明していて degraded ではない"
        );
    }

    #[test]
    fn probe_counts_delta_is_none_without_after_side() {
        assert_eq!(
            probe_counts_delta(TEST_MODES, &[hit(10, 2, 0), None, None], None),
            None
        );
    }

    #[test]
    fn probe_counts_delta_lists_null_per_mode_when_no_pair_is_comparable() {
        // 欠測が互い違いで、同じ (query, mode) で両側そろったマスが 1 つも無い。
        // 「比較可能ペアが 0」であって「mode 一覧が分からない」わけではないので、関数レベルの
        // `None`（JSON では probe_counts_delta 全体が null）ではなく、mode キーは残したまま
        // 各値だけ null にする。以前は前者だった。読み手は「測っていない」のか
        // 「キー名を間違えた」のか JSON だけでは区別できず、
        // `docs/superpowers/specs/2026-07-27-phase-b-community-merge-design.md`
        // （Phase B 全体 / B1 の正本、「merge_schema の未実装事項」節）が
        // 禁じる読み違いの温床になる。
        let before = vec![hit(10, 2, 0), None, hit(1, 0, 0)];
        let after = vec![None, hit(1, 1, 8), None];
        let by_mode = probe_counts_delta(TEST_MODES, &before, Some(&after))
            .expect("mode 一覧そのものは常に返る（グリッドの形は正常なため）");
        for mode in TEST_MODES {
            assert_eq!(
                delta_of(&by_mode, mode),
                None,
                "mode {mode} は比較可能ペアが無いので値は null"
            );
        }
        let rendered = by_mode.to_json();
        for mode in TEST_MODES {
            assert_eq!(
                rendered[mode],
                Value::Null,
                "mode {mode} のキーは残り、値だけ null"
            );
        }

        // 片側が全滅した場合も同じ（旧実装が succeeded == 0 で弾いていたケース）。
        let by_mode_after_all_failed =
            probe_counts_delta(TEST_MODES, &before, Some(&[None, None, None]))
                .expect("after 側が全滅でも mode 一覧は返る");
        for mode in TEST_MODES {
            assert_eq!(delta_of(&by_mode_after_all_failed, mode), None);
        }
    }

    #[test]
    fn probe_counts_delta_is_none_when_grids_are_misaligned() {
        // グリッドは固定なので長さは常に一致するはずだが、一致しないなら index 対応が
        // 崩れている。前方一致で辻褄を合わせず比較不能にする。
        let before = vec![hit(5, 0, 0), hit(4, 1, 0), hit(1, 0, 0)];
        let after = vec![hit(3, 0, 2), hit(2, 1, 3)];
        assert_eq!(probe_counts_delta(TEST_MODES, &before, Some(&after)), None);
    }

    #[test]
    fn probe_counts_delta_is_none_when_grid_is_not_a_whole_number_of_mode_rows() {
        // 長さが mode 数の倍数でないなら index → mode の対応が決まらない。
        // 適当に割り当てると「local の差分」と称して別 mode の値を見せることになる。
        let before = vec![hit(5, 0, 0), hit(4, 1, 0)];
        let after = vec![hit(3, 0, 2), hit(2, 1, 3)];
        assert_eq!(probe_counts_delta(TEST_MODES, &before, Some(&after)), None);
    }

    #[test]
    fn probe_counts_delta_is_none_when_mode_list_is_empty() {
        // mode が 0 件だと index → mode の写像が定義できない（剰余演算も成立しない）。
        assert_eq!(probe_counts_delta(&[], &[], Some(&[])), None);
    }

    #[test]
    fn probe_grid_matches_query_major_mode_minor_order() {
        // `probe_grid()` は probe() 本体が実際に使う唯一の生成元。ここでは index →
        // (query, mode) の写像そのものを検証する。
        //
        // 以前のテスト（`(a*b) % b == 0`）は並び順に一切依存しない恒真式で、probe() の
        // ループ順を mode-major に入れ替えても常に成功していた（実際に入れ替えて
        // `cargo test` を実行し、旧テストが通ったまま新テストだけ落ちることを確認した）。
        // この形なら、index % modes.len() が実際の mode と一致するという
        // `probe_counts_delta` の前提が崩れた場合に検出できる。
        let grid = probe_grid();
        assert_eq!(
            grid.len(),
            PROBE_QUERIES.len() * PROBE_MODES.len(),
            "1 query あたり PROBE_MODES 件、必ず全マスを埋める"
        );
        assert!(!PROBE_MODES.is_empty());
        for (index, (query, mode)) in grid.iter().enumerate() {
            assert_eq!(
                *mode,
                PROBE_MODES[index % PROBE_MODES.len()],
                "index {index}: query-major/mode-minor なら mode は index % PROBE_MODES.len() の位置と一致するはず"
            );
            assert_eq!(
                *query,
                PROBE_QUERIES[index / PROBE_MODES.len()],
                "index {index}: query-major/mode-minor なら query は index / PROBE_MODES.len() の位置と一致するはず"
            );
        }
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
    fn probe_side_observation_counts_are_none_when_skipped() {
        // Skipped は `Some(0)` ではなく `None`。0 に潰すと --probe-only 実行の JSON が
        // 「after 側 probe を回して 1 件も成功しなかった」と読める（実際には回していない）。
        assert_eq!(ProbeSideObservation::Skipped.attempted_count(), None);
        assert_eq!(ProbeSideObservation::Skipped.succeeded_count(), None);
        let attempted = ProbeSideObservation::Attempted {
            attempted: 12,
            succeeded: 8,
        };
        assert_eq!(attempted.attempted_count(), Some(12));
        assert_eq!(attempted.succeeded_count(), Some(8));
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
    fn merge_report_json_keeps_the_same_shape_on_every_path() {
        // 経路ごとにキーが変わる（skipped / ok / error）と、後段ツールは
        // 「キーの有無」で分岐せざるを得ず、新しい経路が増えるたびに壊れる。
        let keys = |value: &Value| {
            value
                .as_object()
                .expect("merge は object")
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        };
        let skipped = MergeReport::skipped().to_json();
        let ok = MergeReport::ok(12.5).to_json();
        let failed = MergeReport::failed(
            anyhow::anyhow!("permission denied").context("merge schema urtect"),
            3.0,
        )
        .to_json();

        assert_eq!(keys(&skipped), keys(&ok));
        assert_eq!(keys(&skipped), keys(&failed));
        assert_eq!(skipped["status"], "skipped");
        assert_eq!(skipped["error"], Value::Null);
        assert_eq!(ok["status"], "ok");
        assert_eq!(ok["error"], Value::Null);
        assert_eq!(failed["status"], "failed");
        assert_eq!(
            failed["error"],
            Value::from("merge schema urtect: permission denied"),
            "gRPC code 由来のヒントを含む context 連鎖を落とさない"
        );
    }

    #[test]
    fn fatal_error_keeps_the_original_merge_error_in_the_chain() {
        // Cloud Run job の失敗サマリでは最終エラー行しか見ないことがある。定型文で
        // 置き換えると、Merge が返した gRPC 由来のヒントが運用者へ届かなくなる。
        let err = fatal_error(
            RunFailure::MergeFailed,
            Some(anyhow::anyhow!("Merge は admin ロール必須").context("merge schema urtect")),
        );
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains(RunFailure::MergeFailed.message()),
            "verdict の説明が先頭に出る: {rendered}"
        );
        assert!(
            rendered.contains("admin ロール必須"),
            "原エラーが連鎖に残る: {rendered}"
        );
    }

    #[test]
    fn fatal_error_falls_back_to_the_verdict_message_without_a_merge_error() {
        // Merge 以外の Fatal（probe 全滅など）では原エラーが無い。verdict の説明だけを返す。
        let err = fatal_error(RunFailure::ProbeAllFailed, None);
        assert_eq!(
            format!("{err:#}"),
            RunFailure::ProbeAllFailed.message().to_string()
        );
    }

    #[test]
    fn evaluate_run_does_not_blame_merge_for_zero_communities_when_merge_did_not_run() {
        // 「Merge を実行したのに 0 件」が空振りの定義なので、Merge を回していない実行に
        // この判定を当てない。RunObservation は merge: Skipped と community_after: Value(0) を
        // 同時に持てる値であり（--probe-only でも after stats を取る変更を入れれば実際に成立する）、
        // merge を条件に含めないと観測専用実行が非 0 終了に化ける。
        let full = PROBE_QUERIES.len() * PROBE_MODES.len();
        let verdict = evaluate_run(RunObservation {
            community_before: StatsObservation::Value(0),
            community_after: StatsObservation::Value(0),
            merge: MergeStatus::Skipped,
            probe_before_attempted: full,
            probe_before_succeeded: full,
            probe_after: ProbeSideObservation::Skipped,
        });
        assert_eq!(
            verdict,
            RunVerdict::Warn(RunWarning::CommunityCountUnchanged)
        );
    }

    #[test]
    fn evaluate_run_prefers_probe_after_all_failed_over_no_communities() {
        // Merge 成功 / community 0 件 / after probe 全滅が同時成立するケース。
        // 先に出すのは「after 側の観測が何も取れていない」方。観測経路そのもの（接続・権限）が
        // 死んでいる可能性がある状態で「コミュニティが作れていない」と読ませると、
        // vegapunk 側のログを追う誤った調査に運用者を送り込む。どちらも非 0 終了だが、
        // 先頭に出るメッセージが調査の入口を決めるので優先順位を固定する。
        let full = PROBE_QUERIES.len() * PROBE_MODES.len();
        let verdict = evaluate_run(RunObservation {
            community_before: StatsObservation::Value(0),
            community_after: StatsObservation::Value(0),
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
        // 読んで ok にすると、Phase C の hybrid 切替可否判断に使う実測 JSON が空のまま見逃される。
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

    /// テスト用の `JobInfo` 組み立てヘルパ。`msg_id` は診断出力（`job_info_json`）に
    /// 使わない固定値で埋め、テストの意図（status / error / 並び順）を読みやすくする。
    fn job_info(
        job_id: &str,
        status: &str,
        error: Option<&str>,
        created_at: i64,
        completed_at: Option<i64>,
        retry_count: i32,
    ) -> JobInfo {
        JobInfo {
            job_id: job_id.to_string(),
            job_type: "merge".to_string(),
            status: status.to_string(),
            error: error.map(str::to_string),
            created_at,
            completed_at,
            msg_id: Some("msg-1".to_string()),
            retry_count,
        }
    }

    #[test]
    fn diagnostic_priority_ranks_error_above_non_completed_above_completed() {
        let has_error =
            diagnostic_priority(&job_info("j1", "completed", Some("boom"), 1, Some(2), 0));
        let non_completed_no_error =
            diagnostic_priority(&job_info("j2", "running", None, 1, None, 0));
        let failed_no_error = diagnostic_priority(&job_info("j3", "failed", None, 1, None, 3));
        let completed_no_error =
            diagnostic_priority(&job_info("j4", "completed", None, 1, Some(2), 0));
        let empty_error_completed =
            diagnostic_priority(&job_info("j5", "completed", Some(""), 1, Some(2), 0));

        assert!(
            has_error < non_completed_no_error,
            "error 非空が最優先（第 1 キー）: {has_error:?} vs {non_completed_no_error:?}"
        );
        assert!(
            non_completed_no_error < completed_no_error,
            "status != completed が次点（第 2 キー）: {non_completed_no_error:?} vs {completed_no_error:?}"
        );
        assert_eq!(
            non_completed_no_error, failed_no_error,
            "completed 以外はどの status でも同じ優先度: {non_completed_no_error:?} vs {failed_no_error:?}"
        );
        assert_eq!(
            empty_error_completed, completed_no_error,
            "error が空文字は非空ではないので error 無し扱い"
        );
    }

    #[test]
    fn sort_jobs_diagnostic_first_moves_flagged_jobs_to_front_preserving_relative_order() {
        // vegapunk 申告順（created_at DESC）を模した並び:
        // completed, failed(error あり), completed, running(error 無し)。
        let jobs = vec![
            job_info("ok-1", "completed", None, 400, Some(410), 0),
            job_info(
                "failed-1",
                "failed",
                Some("node2vec failed"),
                300,
                Some(305),
                5,
            ),
            job_info("ok-2", "completed", None, 200, Some(210), 0),
            job_info("running-1", "running", None, 100, None, 0),
        ];

        let sorted = sort_jobs_diagnostic_first(jobs);
        let ids: Vec<&str> = sorted.iter().map(|j| j.job_id.as_str()).collect();

        // 診断対象（failed-1, running-1）が先頭に来て、かつ各グループ内では元の並び
        // （created_at DESC）が保たれる（安定ソートであることの回帰テスト）。
        assert_eq!(ids, vec!["failed-1", "running-1", "ok-1", "ok-2"]);
    }

    #[test]
    fn sort_jobs_diagnostic_first_keeps_error_jobs_ahead_of_newer_error_free_jobs_from_other_schemas(
    ) {
        // 他 schema の ingest/merge が並走していると、error 無しの running ジョブが
        // failed よりずっと新しい created_at で並ぶ。1 段キー（error 非空 OR
        // status != completed を 1 bit に潰す）だと両者が同じ優先度タイルに入り、安定ソート
        // が created_at DESC を保つ結果、running が failed より先頭に来ていた
        // （この回帰テストが無い状態だと検出できないバグ）。2 段キーでは error の有無を
        // 独立した第 1 キーにするため、failed が常に先に来る。
        let jobs = vec![
            job_info("running-other-schema", "running", None, 500, None, 0),
            job_info(
                "failed-1",
                "failed",
                Some("node2vec failed"),
                300,
                Some(305),
                5,
            ),
            job_info("ok-1", "completed", None, 200, Some(210), 0),
        ];

        let sorted = sort_jobs_diagnostic_first(jobs);
        let ids: Vec<&str> = sorted.iter().map(|j| j.job_id.as_str()).collect();

        assert_eq!(ids, vec!["failed-1", "running-other-schema", "ok-1"]);
    }

    #[test]
    fn since_ms_from_hours_zero_means_no_window() {
        assert_eq!(since_ms_from_hours(0, 1_753_000_000_000), None);
    }

    #[test]
    fn since_ms_from_hours_subtracts_the_window_from_now() {
        let now_ms = 1_753_000_000_000;
        // 24h = 86_400_000ms
        assert_eq!(since_ms_from_hours(24, now_ms), Some(now_ms - 86_400_000));
        // 1h = 3_600_000ms
        assert_eq!(since_ms_from_hours(1, now_ms), Some(now_ms - 3_600_000));
    }

    #[test]
    fn since_ms_from_hours_saturates_instead_of_overflowing_on_huge_windows() {
        // u64::MAX 時間を渡しても panic せず飽和する
        // （`--jobs-since-hours` はユーザ入力なので、桁あふれで CLI が落ちないことを保証する）。
        // jobs_since_hours -> i64::MAX（try_from 失敗時の unwrap_or）-> *3_600_000 は
        // saturating_mul で i64::MAX に飽和し、`0 - i64::MAX` は i64 の範囲内（i64::MIN より
        // 1 大きい）なのでこちらは飽和しない。
        let result = since_ms_from_hours(u64::MAX, 0);
        assert_eq!(result, Some(-i64::MAX));
    }

    #[test]
    fn job_info_json_includes_diagnostic_fields_and_excludes_msg_id() {
        let job = job_info(
            "j1",
            "failed",
            Some("node2vec failed"),
            1_753_000_000_000,
            Some(1_753_000_060_000),
            5,
        );
        let rendered = job_info_json(&job);

        assert_eq!(rendered["job_id"], "j1");
        assert_eq!(rendered["job_type"], "merge");
        assert_eq!(rendered["status"], "failed");
        assert_eq!(rendered["error"], "node2vec failed");
        assert_eq!(rendered["created_at"], 1_753_000_000_000i64);
        assert_eq!(rendered["completed_at"], 1_753_000_060_000i64);
        assert_eq!(rendered["retry_count"], 5);
        assert!(
            rendered.get("msg_id").is_none(),
            "msg_id は完了条件のフィールド一覧に無いので出力しない: {rendered}"
        );
    }

    /// `recent_jobs_value` の成功 / 失敗パスが同じキー集合を返すことの回帰テスト。
    /// `merge_report_json_keeps_the_same_shape_on_every_path` と同じ趣旨: キーの有無で
    /// 後段ツールが分岐する事態を防ぐ。
    #[test]
    fn recent_jobs_value_ok_and_err_share_the_same_key_set() {
        let keys = |value: &Value| {
            value
                .as_object()
                .expect("recent_jobs は object")
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
        };
        let ok = recent_jobs_value(
            Ok(ListJobsResponse {
                jobs: Vec::new(),
                total_count: 0,
            }),
            Some(1_753_000_000_000),
            false,
        );
        let err = recent_jobs_value(
            Err(anyhow::anyhow!("list jobs: boom")),
            Some(1_753_000_000_000),
            false,
        );

        assert_eq!(keys(&ok), keys(&err));
        assert_eq!(
            keys(&ok),
            ["jobs", "error", "total_count", "since_ms"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }

    #[test]
    fn recent_jobs_value_ok_carries_sorted_jobs_total_count_and_applied_window() {
        // total_count は ListJobs がフィルタ後・ページング前に申告する全件数。jobs（limit で
        // 打ち切られる）と切り離して残さないと、フィルタ後の全件が見えているかどうかが
        // 分からなくなる。since_ms は「どの時間窓を見たか」を summary JSON だけで判別できる
        // ようにするために残す（ListJobs は schema を絞れず cross-schema のままなので、
        // total_count だけでは対象 schema のジョブが漏れたかまでは分からない）。
        let resp = ListJobsResponse {
            jobs: vec![
                job_info("ok-1", "completed", None, 200, Some(210), 0),
                job_info(
                    "failed-1",
                    "failed",
                    Some("node2vec failed"),
                    100,
                    Some(110),
                    3,
                ),
            ],
            total_count: 9_999,
        };
        let rendered = recent_jobs_value(Ok(resp), Some(1_753_000_000_000), false);

        assert_eq!(rendered["error"], Value::Null);
        assert_eq!(rendered["total_count"], 9_999);
        assert_eq!(rendered["since_ms"], 1_753_000_000_000i64);
        let jobs = rendered["jobs"].as_array().expect("jobs は array");
        assert_eq!(jobs.len(), 2);
        assert_eq!(
            jobs[0]["job_id"], "failed-1",
            "診断対象（error あり）が先頭に来る: {jobs:?}"
        );
    }

    #[test]
    fn recent_jobs_value_ok_reports_null_since_ms_when_window_is_disabled() {
        // `--jobs-since-hours 0` で窓なしにした場合、since_ms は None のまま JSON の null に
        // 落ちる（「窓を掛けていない」ことを summary JSON だけで判別できるようにする）。
        let rendered = recent_jobs_value(
            Ok(ListJobsResponse {
                jobs: Vec::new(),
                total_count: 0,
            }),
            None,
            false,
        );

        assert_eq!(rendered["since_ms"], Value::Null);
    }

    #[test]
    fn recent_jobs_value_err_reports_error_with_empty_jobs_null_total_count_and_the_applied_window()
    {
        let rendered = recent_jobs_value(
            Err(anyhow::anyhow!("permission denied")),
            Some(1_753_000_000_000),
            false,
        );

        assert_eq!(rendered["jobs"], Value::Array(Vec::new()));
        assert_eq!(rendered["total_count"], Value::Null);
        assert_eq!(rendered["error"], "permission denied");
        assert_eq!(rendered["since_ms"], 1_753_000_000_000i64);
    }

    #[test]
    fn split_failed_jobs_for_logging_caps_at_five_and_reports_remaining() {
        // 7 件の失敗ジョブ + 完了ジョブ 1 件。先頭 5 件だけ個別ログ対象になり、
        // 残り 2 件は件数だけのサマリに回ることを保証する
        // （--probe-only の日常観測で過去の failed ジョブが ERROR ログを埋め尽くす事故対策）。
        let mut jobs: Vec<JobInfo> = (0..7)
            .map(|i| {
                job_info(
                    &format!("failed-{i}"),
                    "failed",
                    Some("node2vec failed"),
                    100 + i,
                    None,
                    1,
                )
            })
            .collect();
        jobs.push(job_info("ok-1", "completed", None, 1, Some(2), 0));

        let (head, remaining) = split_failed_jobs_for_logging(&jobs);

        assert_eq!(head.len(), 5, "先頭 5 件だけ個別ログ対象: {head:?}");
        assert!(
            head.iter().all(|job| job.job_id.starts_with("failed-")),
            "完了ジョブ（error 無し）は個別ログ対象に混ざらない: {head:?}"
        );
        assert_eq!(remaining, 2, "5 件を超えた分は残数として報告する");
    }

    #[test]
    fn split_failed_jobs_for_logging_ignores_jobs_without_error() {
        let jobs = vec![
            job_info("ok-1", "completed", None, 1, Some(2), 0),
            job_info("running-1", "running", None, 2, None, 0),
        ];

        let (head, remaining) = split_failed_jobs_for_logging(&jobs);

        assert!(
            head.is_empty(),
            "error が無いジョブ（completed も running も）はログ対象にしない: {head:?}"
        );
        assert_eq!(remaining, 0);
    }
}
