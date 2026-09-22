//! Jev（TypeSafe System One）判定クライアント。
//!
//! **配線先は `server/src/api.rs::reply_handler` の 1 箇所のみ。** Issue #58 で、第1層の
//! エスカレーションルールのうち、ヒアリング契約 `product_and_symptom`
//! （`harness::rules::HearingContract::ProductAndSymptom`。実データでは `warranty-failure` だけが
//! 宣言）を宣言したルールにマッチしたターンに限り、`api.rs::resolve_jev_has_enough_info` が
//! このクライアントの `evaluate` を呼び、応答の `has_enough_info`（`noul` 値）だけを聞き返し
//! （Clarify）vs 即エスカレーション（EscalationReply）の判定に使う
//! （`api.rs::decide_jev_hearing_action`）。他の質問への回答は判定に使わない。この経路以外
//! （宣言の無い第1層ルール（`contract-billing` を含む）・第1層 mandatory・第2層・第3層・MCP
//! `evaluate_answerability`・`advisor/` 配下）へは配線しないこと（design doc §7「対象外」）。
//! 判別を binding（advisory か）ではなく宣言で行う理由は `api.rs::is_product_and_symptom_hearing_turn`
//! の doc を参照（`has_enough_info` は製品と症状の両方が分かるかを測るので、型番も症状も
//! 関係ない契約・請求の問い合わせには当てはまらない）。
//!
//! Issue #56 設計にあった「fire-and-forget の shadow ログ記録（全質問の結果を
//! `tracing::info!` で 1 行残す、`main.rs` / `mcp.rs` 等からの並行呼び出し）」は依然として
//! 未実装のまま。詳細と不変条件は design doc §1〜§3・§7 を参照。
//!
//! API キーは env `TYPESAFE_API_KEY` のみから解決する。`llm.rs` の `AnthropicClient` と違い
//! ファイルフォールバック（`*_key_file` 設定）は無い（design doc に記載が無いため追加していない）。
//! 鍵はログに出さないこと。`Debug` は手動実装し `api_key` を redact する。
//!
//! 正本: `docs/superpowers/specs/2026-09-21-jev-shadow-design.md` §1〜§2。

use crate::config::JevConfig;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

const API_KEY_ENV: &str = "TYPESAFE_API_KEY";

/// Jev 応答本文の読み込み上限。実測では 1 応答あたり出力 385 トークン程度
/// (design doc §1 実測値)で、JSON のオーバーヘッドを踏まえても数 KB に収まる。
/// 1 MiB は誤設定・エンドポイント異常時に無制限のメモリ確保を避けるための
/// 安全マージンで、通常応答を拒否するリスクは無い。
///
/// **上限の実効的な境界は `evaluate` の逐次読み込み(`response.chunk()` のループ)が担う。**
/// Content-Length ヘッダによる事前チェックは早期拒否の追加でしかなく、ヘッダが無い
/// (chunked)応答では素通りする。かつて `response.bytes()` で一括読み込みしていたときは、
/// 事後チェックの前に本文全体がメモリへ確保されており、実測で 64 MiB の chunked 応答が
/// 丸ごとバッファされて上限が機能していなかった。
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Jev の 1 問に対する回答。`type` フィールドの値で 3 種類に分岐する（design doc §1）。
///
/// `probabilities: HashMap<String, f64>` / `legend: HashMap<String, String>` という形は
/// design doc に明記が無いため実装者判断（Issue #56 spec に明記済み）。
#[derive(Debug, Clone, PartialEq)]
pub enum JevAnswer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        confidence: f64,
        probabilities: HashMap<String, f64>,
    },
    Score {
        score: f64,
        confidence: f64,
        probabilities: HashMap<String, f64>,
        legend: HashMap<String, String>,
    },
}

impl JevAnswer {
    /// 回答の種別名。語彙は Jev の応答 JSON の `type` フィールドと同一（`noul` / `choice` /
    /// `score`。`parse_one_answer` と揃えてあり、テストで固定している）。
    ///
    /// **値を一切含まない**ことが目的。`choice` の選択値・`confidence`・`probabilities`・
    /// `legend`・`score` はモデル生成値で、顧客由来のデータを含みうる。ログには種別名だけを
    /// 出したい呼び出し側（`api.rs::query_jev_has_enough_info`）が使う。`Debug` 出力
    /// （`{:?}`）は実値を全部含むため、ログ用途では使わないこと。
    pub fn kind(&self) -> &'static str {
        match self {
            JevAnswer::Noul { .. } => "noul",
            JevAnswer::Choice { .. } => "choice",
            JevAnswer::Score { .. } => "score",
        }
    }
}

/// Anthropic 同様の入出力トークン数（課金・較正の実測用。design doc §1 の実測値参照）。
#[derive(Debug, Clone, PartialEq)]
pub struct JevUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// `evaluate` の戻り値。呼び出し側は `server/src/api.rs::query_jev_has_enough_info`
/// （`resolve_jev_has_enough_info` 経由、Issue #58）で、`answers` のうち `has_enough_info` の
/// `noul` 回答だけを読む。
#[derive(Debug, Clone, PartialEq)]
pub struct JevOutcome {
    pub answers: HashMap<String, JevAnswer>,
    pub usage: JevUsage,
}

/// Jev（`POST {endpoint}`）を叩くクライアント。`enabled = false` の設定からは構築されない
/// （`from_config` が `Ok(None)` を返す）。
pub struct JevClient {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: String,
    /// `[jev] timeout_secs` の実値（`http` に設定したものと同一）。`evaluate` がタイムアウトを
    /// 報告する文言へ埋め込み、運用者が「設定値を上げるべきか」を warn 1 行で判断できるようにする。
    timeout: Duration,
    /// 構築時に 1 度だけ読み込み、以後は `evaluate` のリクエストへそのまま埋め込む
    /// （毎呼び出しでファイルを再読み込みしない）。
    questions: serde_json::Value,
}

impl std::fmt::Debug for JevClient {
    /// `api_key` を redact した Debug 出力（誤ってログに流れても鍵が漏れないようにする。
    /// `AnthropicClient` の Debug 実装を踏襲）。`questions` は本文ではなく件数のみ出す
    /// （デバッグに有用な情報を残しつつ、ログを冗長にしない）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevClient")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field(
                "questions_count",
                &self.questions.as_object().map(|o| o.len()),
            )
            .finish()
    }
}

impl JevClient {
    /// `JevConfig` からクライアントを構築する。
    ///
    /// `config_dir` は起動時に読み込んだ config ファイルのディレクトリ。`questions_path` が
    /// 相対パスならこれを基準に解決する（絶対パスならそのまま使う）。この規約は
    /// `Harness::build`（`harness/mod.rs`）の `resolve_path` クロージャと同一で、
    /// `[harness]` 配下の各 `*_path` 群にもすでに適用されている既存規約に揃えている。
    /// `questions_path` の既定値は相対パスかつ（`enabled = true` なら）必須で読むため、
    /// ここがプロセス CWD 基準のままだと `[jev] enabled = true` にした瞬間に起動が
    /// working directory 依存になり、CWD が config ファイルの場所と異なる運用（例:
    /// リポジトリルートから `cargo run --manifest-path server/Cargo.toml` する等）で
    /// 起動失敗する。
    ///
    /// - `enabled = false` → `Ok(None)`。**この時点で questions_path のファイル読み込みも
    ///   reqwest クライアント構築も一切行わない**（shadow 機能が無効な環境で
    ///   questions_path が存在しなくても起動を妨げない）。
    /// - `enabled = true` かつ鍵が解決できない → `Err`（`AnthropicClient::from_config` と同じ
    ///   fail-closed 方針。有効化したつもりで鍵を忘れる事故を防ぐ）。
    pub fn from_config(cfg: &JevConfig, config_dir: &Path) -> Result<Option<Self>> {
        if !cfg.enabled {
            return Ok(None);
        }
        let api_key = resolve_api_key(env::var(API_KEY_ENV).ok())?;
        Self::build(api_key, cfg, config_dir).map(Some)
    }

    /// 鍵解決を除いた構築処理（questions_path の解決・読み込み・reqwest クライアント構築）。
    ///
    /// `from_config` から分離しているのは、テストが env `TYPESAFE_API_KEY` を書き換えずに
    /// 「鍵は解決できたが questions_path が無効」という状態を再現するため。
    /// `TYPESAFE_API_KEY` はプロセス全体の環境変数であり、`cargo test` は同一プロセス内で
    /// テストを並列実行するため、素朴に `env::set_var` すると他のテスト（鍵未設定を確認する
    /// テスト）の判定と競合し flaky になる。鍵を引数で受けることでこの経路を丸ごと避ける。
    ///
    /// `pub(crate)`（Issue #58）: `api.rs` の聞き返し判定（`resolve_jev_has_enough_info` 等）を
    /// wiremock 相手の統合テストで検証するために、このモジュール外からも同じ「env を汚染しない」
    /// 構築経路が要る。crate 外へは公開しない。
    pub(crate) fn build(api_key: String, cfg: &JevConfig, config_dir: &Path) -> Result<Self> {
        validate_endpoint(&cfg.endpoint)?;
        validate_enough_info_threshold(cfg.enough_info_threshold)?;
        let questions_path = resolve_relative_to(config_dir, &cfg.questions_path);
        let raw = fs::read_to_string(&questions_path)
            .with_context(|| format!("read jev.questions_path {}", questions_path.display()))?;
        let questions: serde_json::Value = serde_json::from_str(&raw).with_context(|| {
            format!(
                "parse jev.questions_path {} as json",
                questions_path.display()
            )
        })?;
        // 「有効化したつもりで設定ミスに気づかず起動してしまう事故を防ぐ」fail-closed 方針
        // （このメソッドの doc）を、JSON として妥当かどうかだけでなく構造にも適用する。
        // `null` / 配列 / 空文字列 / 空 object はいずれも JSON としては妥当にパースできて
        // しまうため、パース成功だけでは「使える質問定義が入っている」ことの保証にならない。
        if questions.as_object().is_none_or(|o| o.is_empty()) {
            bail!(
                "jev.questions_path {} must contain a non-empty JSON object \
                 (e.g. {{\"question_id\": {{\"type\": \"noul\", ...}}, ...}}), got {}",
                questions_path.display(),
                json_shape_label(&questions)
            );
        }
        let timeout = Duration::from_secs(cfg.timeout_secs);
        let http = reqwest::Client::builder()
            .timeout(timeout)
            // リダイレクトを一切追わない。`validate_endpoint` は構築時に設定された
            // endpoint の URL だけを検証しており、リダイレクト先の scheme までは見ない。
            // 307/308 は元の POST 本文(顧客発話 `state` を含む)をそのまま再送する仕様なので、
            // 追従を許すと平文 http:// へ本文が送られうる。さらに reqwest の
            // `remove_sensitive_headers` は host と実効ポートが変わらない限り `Authorization`
            // を除去しない(実測: `https://example.com/api` → `http://example.com:443/api2`
            // では host も実効ポートも同じ扱いになり、API キーが平文送信されたまま残る)。
            // Jev の endpoint はリダイレクトを使う契約になっていないため、全面禁止でよい
            // (`ingest_urtect.rs` / `ingest_alarmcom.rs` は同一 origin に限定した custom policy
            // だが、Jev にはそもそも追従を許す理由が無いので `none()` にしている)。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build reqwest client for JevClient")?;
        Ok(Self {
            http,
            endpoint: cfg.endpoint.clone(),
            model: cfg.model.clone(),
            api_key,
            timeout,
            questions,
        })
    }

    /// 顧客発話（`state`）を Jev へ送り、質問定義に対する回答を得る。
    ///
    /// **呼び出し側の責務はユースケースにより異なる。呼び出し元ごとに design doc の該当節を
    /// 確認すること:**
    ///
    /// - **Issue #58 の、ヒアリング契約 `product_and_symptom` を宣言した第1層ルール
    ///   （`warranty-failure`）の聞き返し判定（design doc §7、実装済み）**:
    ///   `api.rs::resolve_jev_has_enough_info` が**同期 `.await`** で呼び出す。結果
    ///   （`has_enough_info`）をそのターンの `Clarify` / `EscalationReply` 判定にそのまま使い、
    ///   対象ターンの応答時間は Jev の応答時間ぶん実際に延びる。失敗・タイムアウトは
    ///   `None` にフォールバックし（`query_jev_has_enough_info`）、`missing` ベースの既存判定
    ///   （`decide_reply_action`）へ委ねる。fire-and-forget ではない。
    /// - **design doc §1〜§3 の shadow-only 用途（未実装）**: `tokio::spawn` で
    ///   fire-and-forget にし、失敗・タイムアウトを応答内容・応答時間に一切波及させないことが
    ///   前提だった（§3 不変条件 1・2）。この経路は Issue #57 時点では未配線のままで、Issue #58
    ///   でも配線していない（モジュール doc 冒頭「配線先は…の1箇所のみ」参照）。
    ///
    /// このメソッド自体はどちらの用途でも同期的に `Result` を返すだけで、fire-and-forget化・
    /// リトライ・タイムアウト後の劣化判断はすべて呼び出し側の責務。
    ///
    /// `request_id` は**ログの突合専用**。応答の要素をパースできず捨てるときの warn
    /// （`parse_answers`）へ載せるためだけに使い、Jev へ送るリクエスト本文には含めない
    /// （社内の相関 ID を外部サービスへ渡さない。`evaluate_sends_state_model_and_questions_in_request_body`
    /// が固定）。呼び出し側（`api.rs::query_jev_has_enough_info`）の warn も同じ値を持つため、
    /// Cloud Run で `/api/reply` が並行処理されていても、2 つの warn を `request_id` で結べる。
    ///
    /// `send()` の失敗は、タイムアウトなら `jev evaluate api timed out after <timeout_secs>s`
    /// （こちらが所有する安定した文言。`describe_send_error` 参照）、それ以外は
    /// `call jev evaluate api` を context に持つ。原因は context の下のチェーンにあるので、
    /// ログへ出す側は `{err:#}` を使うこと（`{err}` だと最外層しか出ない）。
    pub async fn evaluate(&self, state: &str, request_id: &str) -> Result<JevOutcome> {
        let payload = serde_json::json!({
            "state": state,
            "model": self.model,
            "questions": self.questions,
        });
        let body = serde_json::to_vec(&payload).context("serialize jev evaluate request body")?;

        let mut response = self
            .http
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|err| describe_send_error(err, self.timeout))?;

        // status は本文を読む前に確認する。サイズ上限チェックを先にすると、Jev が非成功
        // status とともに大きなエラーページを返した場合、本来の診断情報である HTTP status が
        // 「サイズ超過」に隠れて障害原因の特定を妨げる。レスポンス本文はエラーに含めない
        // （llm.rs と同じログ衛生方針）。status のみ残し、非成功時は本文を一切読まない。
        let status = response.status();
        if !status.is_success() {
            bail!("jev evaluate api returned {status}");
        }

        // Content-Length ヘッダによる事前チェック（宣言されていれば読み込み前に早期拒否できる）。
        // ヘッダが無い（chunked）応答ではここを素通りするため、上限の実効的な境界にはならない
        // （MAX_RESPONSE_BYTES のコメント参照）。
        if let Some(len) = response.content_length() {
            if len > MAX_RESPONSE_BYTES as u64 {
                bail!(
                    "jev evaluate api response declared content-length {len} bytes, \
                     exceeding the {MAX_RESPONSE_BYTES} byte cap"
                );
            }
        }
        // 逐次読み込みで累積サイズを監視し、上限を超えた時点で即座に打ち切る（それ以上読まない・
        // 確保しない）。`response.bytes()` は本文全体を読み切ってからでないとサイズを判定できず、
        // chunked 応答では上限が効かなかった（MAX_RESPONSE_BYTES のコメント参照）。
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("read jev evaluate api response body")?
        {
            // 追加前に判定する（追加後だと `Vec` が一時的に `MAX_RESPONSE_BYTES + chunk.len()`
            // まで伸びてしまい、コメントが主張する「1 MiB が上限」と実挙動がずれる）。
            // `saturating_add` は `bytes.len() + chunk.len()` が `usize` を溢れる病的入力
            // （現実的には起きないが、事前チェックのオーバーフローで上限判定自体が無効化される
            // 事故を避ける）でも安全に判定できるようにするため。
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                bail!(
                    "jev evaluate api response body exceeded the {MAX_RESPONSE_BYTES} byte cap \
                     while streaming the response body"
                );
            }
            bytes.extend_from_slice(&chunk);
        }

        let text = std::str::from_utf8(&bytes)
            .context("decode jev evaluate api response body as utf-8")?;
        let parsed: RawJevResponse =
            serde_json::from_str(text).context("parse jev evaluate api response as json")?;
        Ok(JevOutcome {
            answers: parse_answers(parsed.answers, request_id),
            usage: JevUsage {
                input_tokens: parsed.usage.input_tokens,
                output_tokens: parsed.usage.output_tokens,
            },
        })
    }
}

/// `send()` が返した `reqwest::Error` に、運用者が原因を判別できる context を付ける。
///
/// - タイムアウト（`is_timeout()`）: `jev evaluate api timed out after <timeout>s`。
///   `<timeout>` は `[jev] timeout_secs` の実値（`3` → `3s`、サブ秒なら `0.05s`）。
/// - それ以外（接続拒否・DNS 失敗・TLS 失敗など）: 従来どおり `call jev evaluate api`。
///   具体的な原因は、context の下に残る reqwest のエラーチェーンが持つ。
///
/// **タイムアウトの文言は、こちらが所有する安定した文字列**であり、判別にもテストにも
/// これを使う。reqwest / hyper の文言（`operation timed out` 等）は依存更新で変わりうるため、
/// 依存しない。anyhow の `Display` は最外層の context しか出さないので、呼び出し側は原因まで
/// 出すために `{err:#}` を使うこと（`api.rs::query_jev_has_enough_info`）。
///
/// `send()` の失敗だけが対象。応答本文の読み込み（`response.chunk()`）中のタイムアウトは、
/// 従来どおり `read jev evaluate api response body` の context に原因チェーンが付く。
fn describe_send_error(err: reqwest::Error, timeout: Duration) -> anyhow::Error {
    if err.is_timeout() {
        anyhow::Error::new(err).context(format!(
            "jev evaluate api timed out after {}s",
            timeout.as_secs_f64()
        ))
    } else {
        anyhow::Error::new(err).context("call jev evaluate api")
    }
}

/// env `TYPESAFE_API_KEY` の値（`raw`）を trim して解決する。空/未設定は運用者が次に
/// 何をすべきか分かるメッセージで `Err`（`llm.rs` の `resolve_api_key` 呼び出し元と同じ文体）。
///
/// 値を `Option<String>` で受け取るのは、テストが実プロセスの env（`env::set_var` /
/// `env::remove_var`）を一切書き換えずに「未設定」「空文字」「空白のみ」「正常値」の
/// 4 状態を決定論的に再現するため。`cargo test` は同一プロセス内でテストを並列実行するため、
/// 素朴に env を書き換えるテストは他のテストの判定と競合し flaky になる（呼び出し元
/// `from_config` は `env::var(API_KEY_ENV).ok()` を渡す）。
fn resolve_api_key(raw: Option<String>) -> Result<String> {
    let trimmed = raw.unwrap_or_default();
    let trimmed = trimmed.trim();
    if trimmed.is_empty() {
        bail!(
            "jev.enabled = true ですが API キーを解決できません。\
             env {API_KEY_ENV} を設定してください \
             (dev では jev.enabled = false にしてください)"
        );
    }
    Ok(trimmed.to_string())
}

/// 相対パスは `config_dir` 基準、絶対パスはそのまま解決する。
/// `Harness::build`（`harness/mod.rs`）の `resolve_path` クロージャと同一の規約。
fn resolve_relative_to(config_dir: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_dir.join(path)
    }
}

/// Jev endpoint の起動時検証（fail closed）。scheme・userinfo・クエリ文字列・フラグメントを見る。
///
/// 検査順序は parse → userinfo 拒否 → クエリ拒否 → フラグメント拒否 → scheme 拒否。
///
/// - **scheme**: API キーと顧客発話(state)を平文で送信しないため、既定では https のみを
///   許可する。テスト用スタブサーバ(wiremock の MockServer は 127.0.0.1 の動的ポートで起動する)
///   だけ、ローカルループバックへの http を例外として許可する。
/// - **userinfo（`user:pass@`）**: 拒否する。失敗時の warn（`api.rs::query_jev_has_enough_info`）は
///   原因チェーンを出し、reqwest のエラー文言は `for url ({url})` と endpoint をそのまま含むため、
///   userinfo があると資格情報がアプリケーションログへ載る。認証は `Authorization` ヘッダ
///   （env `TYPESAFE_API_KEY`）で行うので、URL に資格情報を入れる理由は無い。
/// - **クエリ文字列(`?key=value`)**: 拒否する。userinfo と同じ経路(reqwest のエラー文言
///   `for url ({url})`)でログへ載るため、`?api_key=...` のような値を置けること自体が漏洩経路に
///   なる。かつては正当な用途(マルチテナント識別用の `?tenant=...` 等)を壊さないよう doc の警告
///   だけに留めていたが、現時点でそのような用途は存在せず、「資格情報をログに載せない」という
///   この検証の目的を doc に委ねるのは弱いので fail closed に倒した(本番の endpoint
///   `https://api.typesafe.ai/v1/systemone` はクエリを持たないため影響しない)。
///   空クエリ(`?` のみ)も、`url::Url::query()` が `Some("")` を返すため拒否する。資格情報は
///   運べないが、「クエリを一切持たない」という単純な規則にして運用者への説明を一文で済ませる
///   ためで、末尾の `?` は設定の打ち間違いとしても検出する価値がある。
/// - **フラグメント(`#token=...`)**: 拒否する。クエリと同じ漏洩経路で、空フラグメント(`#` のみ)も
///   `url::Url::fragment()` が `Some("")` を返すため、クエリと同じ理由(「一切持たない」という
///   単純な規則)で拒否する。**reqwest がフラグメントをエラー文言へ実際に露出するかは実測して
///   いないが、露出するかどうかに関係なく拒否する**: `url::Url::as_str()` が fragment を保持する
///   以上、URL を出すログ経路が将来 1 つ増えるだけで漏れるため。ここで拒否するのは userinfo・
///   クエリ・フラグメントの 3 つだけで、**パスは検証対象外**（正当な endpoint のパスと資格情報を
///   機械的に区別できないため）。パスに資格情報を置くと reqwest のエラー文言 `for url ({url})` と
///   下記の scheme 拒否メッセージの両方にそのまま載るので、パスへ資格情報を置かないことは
///   引き続き運用側の約束（このコードでは強制できない）として残る。
///
/// **エラーメッセージに endpoint の値を出してよいのは、userinfo・クエリ・フラグメントのいずれも
/// 無いと確認できた後だけ**（＝現状 scheme 拒否のメッセージだけ）。値を出すと、塞ごうとしている
/// 漏洩を起動時エラーで再現してしまう。parse に失敗した値はそれらの有無を判定できないので、
/// parse 失敗・userinfo 拒否・クエリ拒否・フラグメント拒否のメッセージは値を含めない。
fn validate_endpoint(endpoint: &str) -> Result<()> {
    let url = url::Url::parse(endpoint).context(
        "parse jev.endpoint as a URL (the value is omitted from this message because it may \
         contain credentials; [jev] endpoint を確認してください)",
    )?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!(
            "jev.endpoint must not contain userinfo (a username or password before the host): \
             failure logs include the endpoint URL, so credentials in it would leak into the logs. \
             The API key is sent via env {API_KEY_ENV}, never in the URL \
             ([jev] endpoint を確認してください)"
        );
    }
    if url.query().is_some() {
        bail!(
            "jev.endpoint must not contain a query string (?key=value): failure logs include the \
             endpoint URL, so a credential placed in the query would leak into the logs. \
             The API key is sent via env {API_KEY_ENV}, never in the URL \
             ([jev] endpoint を確認してください。値はクエリに資格情報が含まれうるため \
             このメッセージには出しません)"
        );
    }
    if url.fragment().is_some() {
        bail!(
            "jev.endpoint must not contain a fragment (#...): failure logs include the endpoint \
             URL, so a credential placed in the fragment would leak into the logs. \
             The API key is sent via env {API_KEY_ENV}, never in the URL \
             ([jev] endpoint を確認してください。値はフラグメントに資格情報が含まれうるため \
             このメッセージには出しません)"
        );
    }
    // ここから先は userinfo・クエリ・フラグメントのいずれも無いと確認済みなので、endpoint の値を
    // メッセージに出してよい。
    let is_loopback_http =
        url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"));
    if url.scheme() != "https" && !is_loopback_http {
        bail!(
            "jev.endpoint {endpoint} must use https:// (http:// is allowed only for \
             http://127.0.0.1 or http://localhost, used by local stub servers in tests); \
             refusing to send the API key and customer utterances in plaintext"
        );
    }
    Ok(())
}

/// `[jev] enough_info_threshold` の範囲検証（reviewer 指摘、Issue #58 第2ラウンド）。
///
/// `api.rs::decide_jev_hearing_action` / `is_reply_clarify_exhausted` の判定式
/// `has_enough_info < enough_info_threshold` のうち、`has_enough_info` 側は `is_valid_noul`
/// により `[0.0, 1.0]` の有限値であることが保証されているが、`threshold` 側は config から
/// 読んだ値をそのまま比較に使っており、これまで無検証だった。無検証のままだと次の誤設定が
/// **警告も出さずに**通ってしまう:
///
/// - `[0.0, 1.0]` の範囲外（負値・`1.0` 超。例: `0.5` の打ち間違いで `5` と書いた場合。TOML の
///   整数は f64 へそのまま入る）
/// - `NaN`（TOML では `nan` リテラルを持つ）・無限大: あらゆる比較が false になり判定式の
///   意味が失われる
///
/// この2種類はこのリポジトリの既存の流儀（`CS_SUPPORT_OAUTH_SIGNING_KEY` の長さ検査、
/// `[llm] enabled = true` 時の鍵解決失敗での起動失敗、`validate_endpoint` 自身）に
/// 揃え、起動時に fail closed で拒否する。
///
/// 一方、境界値ちょうどの `0.0` / `1.0` は `has_enough_info` 側の `is_valid_noul` が inclusive
/// に扱っているのと揃えて**受理する**（確率の有効な境界値であり拒否しない）。ただし運用上は
/// どちらも極端な設定になることを踏まえて使うこと:
///
/// - `0.0`: `has_enough_info < 0.0` がどの有効な noul でも成立しないため、常に
///   `EscalationReply`（＝聞き返しゼロで即エスカレーション。Issue #58 が直したはずの不具合を
///   無言で再発させる誤設定になりうる）
/// - `1.0`: `has_enough_info < 1.0` が `has_enough_info == 1.0` 以外すべてで成立するため、
///   ほぼ常に `Clarify`（聞き返し予算を必ず使い切ってからエスカレーションする）
///
/// `enabled = false` の構成では呼び出されない（`build` は `enabled = true` のときにしか
/// 呼ばれないため。無効な機能の設定値で起動を妨げない規律は `from_config` の doc を参照）。
fn validate_enough_info_threshold(value: f64) -> Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        bail!(
            "jev.enough_info_threshold must be a finite value in the range 0.0..=1.0, \
             but the configured value was {value} ([jev] enough_info_threshold を確認してください)"
        );
    }
    Ok(())
}

/// `questions` の構造検証エラーメッセージ用に、JSON の種類を人間可読なラベルへ変換する。
fn json_shape_label(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(o) if o.is_empty() => "an empty object",
        serde_json::Value::Object(_) => "a non-empty object",
    }
}

#[derive(Debug, Deserialize)]
struct RawJevResponse {
    /// 欠落（フィールド省略）を「回答なし」として許容する。トップレベルの JSON 自体が
    /// 壊れている場合（このフィールドの有無に関わらずパース不能）は別途 `Err` になる。
    #[serde(default)]
    answers: HashMap<String, serde_json::Value>,
    usage: RawJevUsage,
}

#[derive(Debug, Deserialize)]
struct RawJevUsage {
    input_tokens: u64,
    output_tokens: u64,
}

/// `answers` を要素単位で検証しながら `JevAnswer` へ変換する（2 段階パース）。
///
/// 素朴な `#[serde(tag = "type")]` enum は未知の `type` 値が 1 件でもあると `answers`
/// 全体が `Err` になる。Jev の質問セットは運用中に増減するため、未知の質問 ID・未知の
/// `type` を 1 件の欠落として扱い、他の回答は失わない設計にする。
///
/// `request_id` は、捨てた要素の warn を呼び出し側（`api.rs::query_jev_has_enough_info`）の
/// warn と結ぶためだけに使う（`evaluate` の doc 参照）。
fn parse_answers(
    raw: HashMap<String, serde_json::Value>,
    request_id: &str,
) -> HashMap<String, JevAnswer> {
    raw.into_iter()
        .filter_map(|(id, value)| {
            let type_field = value
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("<missing>")
                .to_string();
            match parse_one_answer(&value) {
                Ok(answer) => Some((id, answer)),
                Err(reason) => {
                    // 顧客発話の本文はこのレイヤーに無いが、ログ衛生の規律は他箇所と揃え、
                    // id / type / 理由のみを出す（value そのものは出さない）。例外は `reason` に
                    // 載る数値 `noul` の範囲エラーの実値だけで、診断のため意図的に出している
                    // （f64 は顧客由来のテキストを運べない）。
                    tracing::warn!(
                        request_id = %request_id,
                        answer_id = %id,
                        answer_type = %type_field,
                        reason = %reason,
                        "jev evaluate response contained an answer entry that could not be \
                         parsed; discarding this entry only (other entries still parse)"
                    );
                    None
                }
            }
        })
        .collect()
}

/// `noul` は #58 で閾値判定に使うため、[0.0, 1.0] の範囲外・NaN・無限大を通さない
/// (Issue #56 codex レビュー指摘)。
fn is_valid_noul(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

/// `answers` の要素 1 件を検証する。未知の `type`、または既知の `type` だが必須フィールドが
/// 欠落・型不一致の場合は `Err(理由)` を返す（呼び出し側 `parse_answers` が warn して捨てる）。
fn parse_one_answer(value: &serde_json::Value) -> std::result::Result<JevAnswer, String> {
    let type_field = value
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing or non-string 'type' field".to_string())?;
    match type_field {
        "noul" => {
            let noul = value
                .get("noul")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| "missing or non-numeric 'noul' field".to_string())?;
            if !is_valid_noul(noul) {
                // 実値を理由へ含めるのは診断のための意図的な例外（f64 は顧客由来のテキストを
                // 運べないためログ衛生上の問題にならない。`evaluate_discards_out_of_range_noul_...`
                // が「実値が WARN に出ること」を固定している）。
                return Err(format!(
                    "noul value {noul} is out of the valid [0.0, 1.0] range or not finite"
                ));
            }
            Ok(JevAnswer::Noul { noul })
        }
        "choice" => {
            let choice = value
                .get("choice")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "missing or non-string 'choice' field".to_string())?
                .to_string();
            let confidence = value
                .get("confidence")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| "missing or non-numeric 'confidence' field".to_string())?;
            let probabilities = parse_probabilities(value.get("probabilities"))
                .ok_or_else(|| "missing or malformed 'probabilities' field".to_string())?;
            Ok(JevAnswer::Choice {
                choice,
                confidence,
                probabilities,
            })
        }
        "score" => {
            let score = value
                .get("score")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| "missing or non-numeric 'score' field".to_string())?;
            let confidence = value
                .get("confidence")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| "missing or non-numeric 'confidence' field".to_string())?;
            let probabilities = parse_probabilities(value.get("probabilities"))
                .ok_or_else(|| "missing or malformed 'probabilities' field".to_string())?;
            let legend = parse_legend(value.get("legend"))
                .ok_or_else(|| "missing or malformed 'legend' field".to_string())?;
            Ok(JevAnswer::Score {
                score,
                confidence,
                probabilities,
                legend,
            })
        }
        other => Err(format!("unknown answer type '{other}'")),
    }
}

fn parse_probabilities(value: Option<&serde_json::Value>) -> Option<HashMap<String, f64>> {
    let obj = value?.as_object()?;
    let mut out = HashMap::with_capacity(obj.len());
    for (k, v) in obj {
        out.insert(k.clone(), v.as_f64()?);
    }
    Some(out)
}

fn parse_legend(value: Option<&serde_json::Value>) -> Option<HashMap<String, String>> {
    let obj = value?.as_object()?;
    let mut out = HashMap::with_capacity(obj.len());
    for (k, v) in obj {
        out.insert(k.clone(), v.as_str()?.to_string());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{capture_logs_async, filter_warn_and_error_lines};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// `evaluate` の `request_id` 引数用。ログ突合専用の値で、応答パースの挙動には影響しない
    /// テスト（大半）はこれを渡す。値そのものを assert するテストは専用の値を使う。
    const TEST_REQUEST_ID: &str = "req-test";

    fn temp_dir() -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cs-support-jev-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_questions_file(contents: &str) -> String {
        let dir = temp_dir();
        let path = dir.join("questions.json");
        std::fs::write(&path, contents).expect("write questions file");
        path.to_string_lossy().to_string()
    }

    fn valid_questions_path() -> String {
        write_questions_file(r#"{"is_emergency": {"type": "noul"}}"#)
    }

    // ---- JevAnswer::kind（ログ用の値を含まない種別名） ----

    #[test]
    fn kind_names_each_variant_without_carrying_any_value() {
        assert_eq!(JevAnswer::Noul { noul: 0.42 }.kind(), "noul");
        assert_eq!(
            JevAnswer::Choice {
                choice: "SENTINEL-CHOICE".to_string(),
                confidence: 0.9,
                probabilities: HashMap::new(),
            }
            .kind(),
            "choice"
        );
        assert_eq!(
            JevAnswer::Score {
                score: 3.0,
                confidence: 0.9,
                probabilities: HashMap::new(),
                legend: HashMap::new(),
            }
            .kind(),
            "score"
        );
    }

    #[test]
    fn kind_uses_the_same_vocabulary_as_the_wire_type_field() {
        // 固定するのは「Jev の応答 JSON の `type` 文字列 = パース後の variant = `kind()` が返す
        // 語彙」の 3 者の一致。ずれると、運用者が warn の `answer_kind` と Jev の応答 JSON を
        // 突き合わせられなくなる。`kind()` 自体は網羅 match のため、variant を足せば本体側が
        // コンパイルエラーで止まる（それはこのテストの役割ではない）。
        let wire_answers = [
            serde_json::json!({"type": "noul", "noul": 0.5}),
            serde_json::json!({
                "type": "choice", "choice": "a", "confidence": 0.9, "probabilities": {"a": 0.9}
            }),
            serde_json::json!({
                "type": "score", "score": 1.0, "confidence": 0.7,
                "probabilities": {"1": 0.7}, "legend": {"1": "中"}
            }),
        ];
        for wire in wire_answers {
            let parsed = parse_one_answer(&wire).expect("well-formed answer must parse");
            // `_ =>` を置かない網羅 match。JevAnswer に variant を足すとテスト側もここで
            // コンパイルエラーになり、`wire_answers` へ新 variant の wire 例を足す判断を促す
            // （配列は variant を列挙できないため、足し忘れの検出はこの網羅性に頼る）。
            let expected_wire_type = match &parsed {
                JevAnswer::Noul { .. } => "noul",
                JevAnswer::Choice { .. } => "choice",
                JevAnswer::Score { .. } => "score",
            };
            assert_eq!(
                wire["type"].as_str(),
                Some(expected_wire_type),
                "wire example {wire} must parse into the variant whose wire type is \
                 {expected_wire_type}"
            );
            assert_eq!(
                parsed.kind(),
                expected_wire_type,
                "kind() must equal the wire `type` for {wire}"
            );
        }
    }

    /// `from_config` / `build` の `config_dir` 引数用。このテストファイルの `questions_path`
    /// はすべて絶対パス（`write_questions_file` か明示的な `/nonexistent/...`）なので
    /// `resolve_relative_to` はこの値を無視する。実在しないディレクトリを使うことで、
    /// 万一 `config_dir` 側が使われてしまった場合にテストが誤って通らないようにする
    /// （ディレクトリを作らないので存在確認すれば必ず「無い」と分かる）。
    fn unused_config_dir() -> std::path::PathBuf {
        std::path::PathBuf::from("/config-dir-not-used-because-questions-path-is-absolute")
    }

    /// テスト専用クライアント（`stub_client` in llm.rs と同じ役割）。timeout を明示指定するのは、
    /// 無いと stub が応答しなかったときテストがハングし、CI がジョブ timeout まで気づけないため。
    /// `build()` と同じ `redirect(Policy::none())` を設定するのは、`evaluate_does_not_follow_redirects`
    /// が本番と同じクライアント設定を検証するため（このヘルパーは `build()` を経由せず構造体を
    /// 直接組み立てるので、`build()` 側だけ直しても test_client 経由のテストには反映されない）。
    fn test_client(endpoint: String, timeout: Duration) -> JevClient {
        JevClient {
            http: reqwest::Client::builder()
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            endpoint,
            model: "jev-test".to_string(),
            api_key: "test-key".to_string(),
            timeout,
            questions: serde_json::json!({"is_emergency": {"type": "noul"}}),
        }
    }

    // ---- from_config / build ----

    #[test]
    fn from_config_disabled_returns_none_without_reading_questions_file() {
        let cfg = JevConfig {
            enabled: false,
            questions_path: "/nonexistent/does-not-exist.json".to_string(),
            ..Default::default()
        };
        let client =
            JevClient::from_config(&cfg, &unused_config_dir()).expect("disabled must not error");
        assert!(
            client.is_none(),
            "enabled=false must yield Ok(None) without touching questions_path \
             (nonexistent path proves the file was never opened)"
        );
    }

    // env `TYPESAFE_API_KEY` の有無で結果が変わるため、CI 環境にたまたま同変数が設定されて
    // いても誤って壊れたテストが通らないよう、変数が既に設定されている場合はスキップする
    // （llm.rs の `from_config_enabled_without_any_key_errs` と同じガード）。
    // `env::remove_var` はプロセス全体に効くため他の並行テストと競合し flaky になる —
    // ここでは変数を書き換えず、存在チェックのみで分岐する。
    #[test]
    fn from_config_enabled_without_key_errs() {
        if env::var(API_KEY_ENV).is_ok() {
            eprintln!(
                "skip: {API_KEY_ENV} is set in this environment; \
                 cannot exercise the no-key fail-closed path"
            );
            return;
        }
        let cfg = JevConfig {
            enabled: true,
            questions_path: valid_questions_path(),
            ..Default::default()
        };
        let err = JevClient::from_config(&cfg, &unused_config_dir())
            .expect_err("enabled=true with no TYPESAFE_API_KEY must fail closed");
        assert!(
            err.to_string().contains(API_KEY_ENV),
            "error must name the env var operators need to set, got: {err}"
        );
    }

    #[test]
    fn build_with_missing_questions_file_errs() {
        let cfg = JevConfig {
            enabled: true,
            questions_path: "/nonexistent/does-not-exist.json".to_string(),
            ..Default::default()
        };
        let err = JevClient::build("test-key".to_string(), &cfg, &unused_config_dir())
            .expect_err("a missing questions_path must fail closed");
        assert!(
            err.to_string().contains("questions_path"),
            "error must identify which setting failed to read, got: {err}"
        );
    }

    #[test]
    fn build_with_invalid_questions_json_errs() {
        let cfg = JevConfig {
            enabled: true,
            questions_path: write_questions_file("{ not valid json"),
            ..Default::default()
        };
        let err = JevClient::build("test-key".to_string(), &cfg, &unused_config_dir())
            .expect_err("malformed questions JSON must fail closed");
        assert!(
            err.to_string().contains("questions_path"),
            "error must identify which setting failed to parse, got: {err}"
        );
    }

    #[test]
    fn build_with_non_loopback_http_endpoint_errs() {
        let cfg = JevConfig {
            enabled: true,
            endpoint: "http://example.com/v1/systemone".to_string(),
            questions_path: valid_questions_path(),
            ..Default::default()
        };
        let err = JevClient::build("test-key".to_string(), &cfg, &unused_config_dir()).expect_err(
            "a non-loopback http:// endpoint must be rejected to avoid sending the \
                         api key and customer utterances in plaintext",
        );
        let message = err.to_string();
        assert!(
            message.contains("http://example.com/v1/systemone") || message.contains("https"),
            "error must name the endpoint or require https, got: {message}"
        );
    }

    #[test]
    fn build_with_loopback_http_127_0_0_1_endpoint_succeeds() {
        let cfg = JevConfig {
            enabled: true,
            endpoint: "http://127.0.0.1:9/v1/systemone".to_string(),
            questions_path: valid_questions_path(),
            ..Default::default()
        };
        let client = JevClient::build("test-key".to_string(), &cfg, &unused_config_dir());
        assert!(
            client.is_ok(),
            "http://127.0.0.1 must be allowed as an exception for local stub servers \
             used in tests, got: {:?}",
            client.err()
        );
    }

    #[test]
    fn build_with_loopback_http_localhost_endpoint_succeeds() {
        let cfg = JevConfig {
            enabled: true,
            endpoint: "http://localhost:9/v1/systemone".to_string(),
            questions_path: valid_questions_path(),
            ..Default::default()
        };
        let client = JevClient::build("test-key".to_string(), &cfg, &unused_config_dir());
        assert!(
            client.is_ok(),
            "http://localhost must be allowed as an exception for local stub servers \
             used in tests, got: {:?}",
            client.err()
        );
    }

    // ---- endpoint の userinfo 拒否（失敗時の warn が endpoint URL を出すため） ----
    //
    // `from_config` は鍵解決（env）の後に `build` を呼ぶだけなので、env を書き換えずに済む
    // `build` で検証する（`build` の doc 参照）。センチネル値は、エラーメッセージ自身の文言
    // （"password" 等）と偶然一致しないよう、固有の文字列にしている。

    const USERNAME_SENTINEL: &str = "USERNAME-SENTINEL";
    const PASSWORD_SENTINEL: &str = "PASSWORD-SENTINEL";

    fn build_with_endpoint(endpoint: &str) -> Result<JevClient> {
        let cfg = JevConfig {
            enabled: true,
            endpoint: endpoint.to_string(),
            questions_path: valid_questions_path(),
            ..Default::default()
        };
        JevClient::build("test-key".to_string(), &cfg, &unused_config_dir())
    }

    #[test]
    fn build_rejects_an_endpoint_with_userinfo_without_echoing_the_credentials() {
        // 失敗時の warn は reqwest のエラー（`for url ({url})`）を通じて endpoint をそのまま出す。
        // userinfo があると資格情報がログに載るため、起動時に fail closed で拒否する。
        // 拒否メッセージにも値を出さない（出すと塞ぎたい漏洩を起動時エラーで再現してしまう）。
        let endpoints = [
            format!("https://{USERNAME_SENTINEL}:{PASSWORD_SENTINEL}@api.typesafe.ai/v1/systemone"),
            format!("https://{USERNAME_SENTINEL}@api.typesafe.ai/v1/systemone"),
            format!("https://:{PASSWORD_SENTINEL}@api.typesafe.ai/v1/systemone"),
            // https 以外でも、scheme 拒否のメッセージ（endpoint を出す）より先に userinfo を
            // 拒否し、値を出さないこと。
            format!("http://{USERNAME_SENTINEL}:{PASSWORD_SENTINEL}@example.com/v1/systemone"),
        ];
        for endpoint in endpoints {
            let err = build_with_endpoint(&endpoint)
                .expect_err("an endpoint with userinfo must fail closed at startup");
            let message = format!("{err:#}");
            assert!(
                message.contains("userinfo"),
                "the error must tell operators what to fix, got: {message}"
            );
            for secret in [USERNAME_SENTINEL, PASSWORD_SENTINEL] {
                assert!(
                    !message.contains(secret),
                    "the error message must not echo the credentials ({secret}), got: {message}"
                );
            }
        }
    }

    #[test]
    fn build_rejects_an_endpoint_with_percent_encoded_userinfo_without_echoing_the_credentials() {
        // codex 指摘(Issue #58 レビュー、reviewer 実測): `url::Url` の `username()` / `password()`
        // は percent-decode しない(実測: `https://%55SER:%50ASS@host/` → username() ==
        // "%55SER"、password() == Some("%50ASS"))。単純な `is_empty()` / `is_some()` 判定は
        // decode の有無に依存しないため percent-encoded な userinfo も素通りしない。さらに、
        // `url::Url` は percent-encoded な userinfo を**シリアライズ結果にもそのまま保持する**
        // (空 userinfo のような正規化落ちが起きない)ため、拒否しなければ reqwest が
        // `for url ({url})` で出すエラーに percent-encoded な資格情報がそのまま載る。
        const PERCENT_ENCODED_USERNAME_SENTINEL: &str = "%55SER-PCT-SENTINEL";
        const PERCENT_ENCODED_PASSWORD_SENTINEL: &str = "%50ASS-PCT-SENTINEL";
        let endpoint = format!(
            "https://{PERCENT_ENCODED_USERNAME_SENTINEL}:{PERCENT_ENCODED_PASSWORD_SENTINEL}\
             @api.typesafe.ai/v1/systemone"
        );
        let err = build_with_endpoint(&endpoint).expect_err(
            "percent-encoded userinfo must fail closed at startup, the same as plain userinfo",
        );
        let message = format!("{err:#}");
        assert!(
            message.contains("userinfo"),
            "the error must tell operators what to fix, got: {message}"
        );
        for secret in [
            PERCENT_ENCODED_USERNAME_SENTINEL,
            PERCENT_ENCODED_PASSWORD_SENTINEL,
        ] {
            assert!(
                !message.contains(secret),
                "the error message must not echo the percent-encoded credentials ({secret}), \
                 got: {message}"
            );
        }
    }

    #[test]
    fn build_with_an_unparseable_endpoint_errs_without_echoing_it() {
        // parse に失敗した値は userinfo の有無を判定できないため、値を出さない。
        let endpoint = format!("https://{USERNAME_SENTINEL}:{PASSWORD_SENTINEL}@exa mple.com/");
        let err = build_with_endpoint(&endpoint)
            .expect_err("an unparseable endpoint must fail closed at startup");
        let message = format!("{err:#}");
        assert!(
            message.contains("jev.endpoint"),
            "the error must identify which setting failed to parse, got: {message}"
        );
        for secret in [USERNAME_SENTINEL, PASSWORD_SENTINEL] {
            assert!(
                !message.contains(secret),
                "the error message must not echo the endpoint ({secret}), got: {message}"
            );
        }
    }

    // ---- endpoint のクエリ文字列拒否（Copilot レビュー High、Issue #58） ----
    //
    // 失敗時の warn（`api.rs::query_jev_has_enough_info`）は原因チェーンを出し、reqwest の
    // エラー文言は `for url ({url})` と endpoint をそのまま含む。クエリに資格情報
    // （`?api_key=...` 等）を置けると userinfo と同じ経路でログへ載るため、起動時に拒否する。

    const QUERY_SENTINEL: &str = "QUERY-SENTINEL";

    #[test]
    fn build_rejects_an_endpoint_with_a_query_string_without_echoing_it() {
        let endpoints = [
            format!("https://api.typesafe.ai/v1/systemone?api_key={QUERY_SENTINEL}"),
            // キー無しのクエリ（値だけを置く形）
            format!("https://api.typesafe.ai/v1/systemone?{QUERY_SENTINEL}"),
            // 旧仕様が「正当なクエリ」として許可していた形（`@` を含むクエリ）も拒否する
            format!("https://api.typesafe.ai:8443/v1/systemone?tenant=a@{QUERY_SENTINEL}"),
            // percent-encoded な値もそのまま拒否する（decode の有無に依存しない）
            format!("https://api.typesafe.ai/v1/systemone?token=%41{QUERY_SENTINEL}"),
            // https 以外でも、scheme 拒否のメッセージ（endpoint を出す）より先にクエリを拒否し、
            // 値を出さないこと。検査順序（クエリ拒否 → scheme 拒否）を固定する。
            format!("http://example.com/v1/systemone?api_key={QUERY_SENTINEL}"),
            // テスト用スタブ向けの loopback http 例外の対象でも、クエリは拒否する。
            format!("http://127.0.0.1:8080/v1/systemone?api_key={QUERY_SENTINEL}"),
        ];
        for endpoint in endpoints {
            let err = build_with_endpoint(&endpoint)
                .expect_err("an endpoint with a query string must fail closed at startup");
            let message = format!("{err:#}");
            assert!(
                message.contains("query string"),
                "the error must tell operators what to fix, got: {message}"
            );
            assert!(
                message.contains(API_KEY_ENV),
                "the error must point operators at where the credential belongs, got: {message}"
            );
            assert!(
                !message.contains(QUERY_SENTINEL),
                "the error message must not echo the query ({QUERY_SENTINEL}), got: {message}"
            );
        }
    }

    #[test]
    fn build_rejects_an_empty_query_marker_as_well() {
        // `url::Url` は空クエリ `?` を `Some("")` として保持し、`as_str()` にも `?` が残る
        // （実測。下の前提アサーションで固定する）。空クエリは資格情報を運べないが、
        // 「クエリを一切持たない」という単純な規則にして運用者への説明を一文で済ませるため、
        // 存在するクエリはすべて拒否する。末尾の `?` は設定の打ち間違いとしても妥当な検出対象。
        let endpoint = "https://api.typesafe.ai/v1/systemone?";
        let parsed = url::Url::parse(endpoint).expect("endpoint is a valid URL");
        assert_eq!(
            parsed.query(),
            Some(""),
            "premise: the url crate keeps an empty query as Some(\"\")"
        );
        assert_eq!(
            parsed.as_str(),
            "https://api.typesafe.ai/v1/systemone?",
            "premise: url::Url::as_str() keeps the trailing '?', so any log path that prints \
             the URL would leak it"
        );
        let err = build_with_endpoint(endpoint)
            .expect_err("an empty query marker must fail closed like any other query");
        let message = format!("{err:#}");
        assert!(
            message.contains("query string"),
            "the error must tell operators what to fix, got: {message}"
        );
    }

    // ---- endpoint のフラグメント拒否（Issue #58） ----
    //
    // フラグメント（`#token=...`）はクエリと同じ漏洩経路。`url::Url::as_str()` が fragment を
    // 保持する（各テストの前提アサーションで固定）ため、その URL をエラー文言へ出す経路が 1 つでも
    // あれば資格情報がログへ載る。reqwest が実際に fragment をエラー文言へ露出するかは実測して
    // いないが、露出するかどうかに関係なく拒否する（将来ログ経路が 1 つ増えるだけで漏れるため）。

    const FRAGMENT_SENTINEL: &str = "FRAGMENT-SENTINEL";

    #[test]
    fn build_rejects_an_endpoint_with_a_fragment_without_echoing_it() {
        let endpoints = [
            format!("https://api.typesafe.ai/v1/systemone#token={FRAGMENT_SENTINEL}"),
            // キー無しのフラグメント（値だけを置く形）
            format!("https://api.typesafe.ai/v1/systemone#{FRAGMENT_SENTINEL}"),
            // percent-encoded な値もそのまま拒否する（decode の有無に依存しない）
            format!("https://api.typesafe.ai/v1/systemone#tok=%41{FRAGMENT_SENTINEL}"),
            // https 以外でも、scheme 拒否のメッセージ（endpoint を出す）より先にフラグメントを
            // 拒否し、値を出さないこと。検査順序（フラグメント拒否 → scheme 拒否）を固定する。
            format!("http://example.com/v1/systemone#token={FRAGMENT_SENTINEL}"),
            // テスト用スタブ向けの loopback http 例外の対象でも、フラグメントは拒否する。
            format!("http://127.0.0.1:8080/v1/systemone#token={FRAGMENT_SENTINEL}"),
        ];
        for endpoint in endpoints {
            let parsed = url::Url::parse(&endpoint).expect("endpoint is a valid URL");
            assert!(
                parsed.as_str().contains(FRAGMENT_SENTINEL),
                "premise: url::Url::as_str() keeps the fragment, so any log path that prints \
                 the URL would leak it, got: {}",
                parsed.as_str()
            );
            let err = build_with_endpoint(&endpoint)
                .expect_err("an endpoint with a fragment must fail closed at startup");
            let message = format!("{err:#}");
            assert!(
                message.contains("fragment"),
                "the error must tell operators what to fix, got: {message}"
            );
            assert!(
                message.contains(API_KEY_ENV),
                "the error must point operators at where the credential belongs, got: {message}"
            );
            assert!(
                !message.contains(FRAGMENT_SENTINEL),
                "the error message must not echo the fragment ({FRAGMENT_SENTINEL}), got: \
                 {message}"
            );
        }
    }

    #[test]
    fn build_rejects_an_empty_fragment_marker_as_well() {
        // `url::Url` は空フラグメント `#` を `Some("")` として保持し、`as_str()` にも `#` が残る
        // （実測。下の前提アサーションで固定する）。空フラグメントは資格情報を運べないが、クエリと
        // 同じ理由（「フラグメントを一切持たない」という単純な規則にして運用者への説明を一文で
        // 済ませる）で、存在するフラグメントはすべて拒否する。
        let endpoint = "https://api.typesafe.ai/v1/systemone#";
        let parsed = url::Url::parse(endpoint).expect("endpoint is a valid URL");
        assert_eq!(
            parsed.fragment(),
            Some(""),
            "premise: the url crate keeps an empty fragment as Some(\"\")"
        );
        assert_eq!(
            parsed.as_str(),
            "https://api.typesafe.ai/v1/systemone#",
            "premise: url::Url::as_str() keeps the trailing '#', so any log path that prints \
             the URL would leak it"
        );
        let err = build_with_endpoint(endpoint)
            .expect_err("an empty fragment marker must fail closed like any other fragment");
        let message = format!("{err:#}");
        assert!(
            message.contains("fragment"),
            "the error must tell operators what to fix, got: {message}"
        );
    }

    #[test]
    fn build_accepts_https_endpoints_without_userinfo_query_or_fragment() {
        // 本番の endpoint と同じ形。`@` がパスに現れても userinfo ではないので通る
        // （拒否の判定は文字列中の `@` ではなく、パース結果の username / password で行う）。
        // クエリ・フラグメントを持つ形は上の `build_rejects_an_endpoint_with_a_query_string_...` /
        // `build_rejects_an_endpoint_with_a_fragment_...` が拒否を固定する。
        for endpoint in [
            "https://api.typesafe.ai/v1/systemone",
            "https://api.typesafe.ai:8443/v1/systemone",
            "https://api.typesafe.ai/v1/user@systemone",
        ] {
            let client = build_with_endpoint(endpoint);
            assert!(
                client.is_ok(),
                "{endpoint} has no userinfo, query or fragment and must be accepted, got: {:?}",
                client.err()
            );
        }
    }

    #[test]
    fn build_accepts_empty_userinfo_because_url_normalization_drops_it_before_serialization() {
        // codex 指摘(Issue #58 レビュー、reviewer 実測): 空の userinfo(`https://@host/` /
        // `https://:@host/`)は username() が ""、password() が None になり build() は受理する。
        // 「`@` が文字列にあるから危険」ではなく「パース結果の username / password で判定する」
        // という validate_endpoint の設計が正しいことを示すため、受理してよい理由まで固定する:
        // url::Url はこれらの空 userinfo を正規化の過程で `@` ごと落とすため、reqwest が失敗時
        // ログへ出す `for url ({url})` の URL には資格情報どころか `@` 自体が一切残らない
        // (percent-encoded な userinfo が正規化されずそのまま残るのと対照的。上の
        // build_rejects_an_endpoint_with_percent_encoded_userinfo_... 参照)。
        for endpoint in [
            "https://@api.typesafe.ai/v1/systemone",
            "https://:@api.typesafe.ai/v1/systemone",
        ] {
            let client = build_with_endpoint(endpoint);
            assert!(
                client.is_ok(),
                "{endpoint} has an empty userinfo that url normalization drops entirely and \
                 must be accepted, got: {:?}",
                client.err()
            );

            let parsed = url::Url::parse(endpoint).expect("endpoint is a valid URL");
            assert!(
                !parsed.as_str().contains('@'),
                "url normalization must drop the empty userinfo (including the '@') so that no \
                 credential marker survives into the URL reqwest would log on failure, got: {}",
                parsed.as_str()
            );
        }
    }

    // ---- enough_info_threshold validation (reviewer 指摘、Issue #58 第2ラウンド) ----

    #[test]
    fn validate_enough_info_threshold_accepts_in_range_values_including_boundaries() {
        for value in [0.0, 0.5, 1.0] {
            assert!(
                validate_enough_info_threshold(value).is_ok(),
                "{value} is within [0.0, 1.0] and must be accepted \
                 (boundaries are inclusive, matching is_valid_noul)"
            );
        }
    }

    #[test]
    fn validate_enough_info_threshold_rejects_out_of_range_nan_and_infinite() {
        for value in [-0.1, 1.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = validate_enough_info_threshold(value)
                .expect_err(&format!("{value} must be rejected"));
            let message = err.to_string();
            assert!(
                message.contains(&value.to_string()),
                "error message must include the configured value so operators can see what \
                 was actually set, got: {message}"
            );
            assert!(
                message.contains("enough_info_threshold"),
                "error message must name the offending config key, got: {message}"
            );
        }
    }

    #[test]
    fn build_with_boundary_enough_info_threshold_succeeds() {
        // 境界値 `0.0` / `1.0` は `has_enough_info` 側の `is_valid_noul` が inclusive に扱うのと
        // 揃えて許容する。判定式は `has_enough_info < enough_info_threshold` なので、
        // `0.0` は「常に EscalationReply」（`x < 0.0` はどの有効な noul でも成立しない＝
        // 聞き返しゼロ）、`1.0` は「ほぼ常に Clarify」（`x < 1.0` は `x == 1.0` 以外すべてで
        // 成立）という極端な運用設定になる。値そのものは有効な確率境界であるため拒否しない。
        for threshold in [0.0, 1.0] {
            let cfg = JevConfig {
                enabled: true,
                questions_path: valid_questions_path(),
                enough_info_threshold: threshold,
                ..Default::default()
            };
            let client = JevClient::build("test-key".to_string(), &cfg, &unused_config_dir());
            assert!(
                client.is_ok(),
                "enough_info_threshold = {threshold} is an inclusive boundary and must be \
                 accepted, got: {:?}",
                client.err()
            );
        }
    }

    #[test]
    fn build_with_negative_enough_info_threshold_errs() {
        // 負値は `has_enough_info < threshold` を常に false にし、Issue #58 が直したはずの
        // 「聞かずに即エスカレーション」へ無言で退行させる。起動時に fail closed する。
        let cfg = JevConfig {
            enabled: true,
            questions_path: valid_questions_path(),
            enough_info_threshold: -0.1,
            ..Default::default()
        };
        let err = JevClient::build("test-key".to_string(), &cfg, &unused_config_dir())
            .expect_err("enough_info_threshold = -0.1 must fail closed at startup");
        assert!(
            err.to_string().contains("enough_info_threshold"),
            "error must identify which setting failed validation, got: {err}"
        );
    }

    #[test]
    fn build_with_out_of_range_enough_info_threshold_errs() {
        // `1.0` 超（TOML の整数打ち間違い、例: `0.5` のつもりで `5` と書いた場合）も
        // 同じ理由で fail closed する。
        let cfg = JevConfig {
            enabled: true,
            questions_path: valid_questions_path(),
            enough_info_threshold: 5.0,
            ..Default::default()
        };
        let err = JevClient::build("test-key".to_string(), &cfg, &unused_config_dir())
            .expect_err("enough_info_threshold = 5.0 must fail closed at startup");
        assert!(
            err.to_string().contains("enough_info_threshold"),
            "error must identify which setting failed validation, got: {err}"
        );
    }

    #[test]
    fn from_config_disabled_ignores_out_of_range_enough_info_threshold() {
        // `enabled = false` の構成は、無効な機能の設定値で起動を妨げない既存規律
        // （`from_config_disabled_returns_none_without_reading_questions_file` と同じ）を
        // enough_info_threshold にも適用する。
        let cfg = JevConfig {
            enabled: false,
            enough_info_threshold: f64::NAN,
            ..Default::default()
        };
        let client = JevClient::from_config(&cfg, &unused_config_dir())
            .expect("enabled=false must not validate enough_info_threshold");
        assert!(client.is_none());
    }

    #[test]
    fn debug_output_redacts_api_key() {
        let cfg = JevConfig {
            enabled: true,
            questions_path: valid_questions_path(),
            ..Default::default()
        };
        let client =
            JevClient::build("super-secret-key".to_string(), &cfg, &unused_config_dir()).unwrap();
        let debug = format!("{client:?}");
        assert!(
            !debug.contains("super-secret-key"),
            "Debug output must not leak the api_key: {debug}"
        );
        assert!(debug.contains("<redacted>"));
    }

    // ---- evaluate: parsing ----

    #[tokio::test]
    async fn evaluate_parses_noul_choice_and_score_answers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "jev-latest",
                "answers": {
                    "is_emergency": {"type": "noul", "noul": 0.12},
                    "product": {
                        "type": "choice",
                        "choice": "ADC-V724",
                        "confidence": 0.84,
                        "probabilities": {"ADC-V724": 0.84, "ADC-V523": 0.05}
                    },
                    "urgency": {
                        "type": "score",
                        "score": 1.0,
                        "confidence": 0.7,
                        "probabilities": {"0": 0.1, "1": 0.7, "2": 0.2},
                        "legend": {"0": "低", "1": "中", "2": "高"}
                    }
                },
                "usage": {"input_tokens": 945, "output_tokens": 220}
            })))
            .mount(&server)
            .await;

        let client = test_client(
            format!("{}/v1/systemone", server.uri()),
            Duration::from_secs(5),
        );
        let outcome = client
            .evaluate("電源が入りません", TEST_REQUEST_ID)
            .await
            .expect("well-formed response must parse");

        assert_eq!(outcome.usage.input_tokens, 945);
        assert_eq!(outcome.usage.output_tokens, 220);
        assert_eq!(outcome.answers.len(), 3);
        assert_eq!(
            outcome.answers.get("is_emergency"),
            Some(&JevAnswer::Noul { noul: 0.12 })
        );
        assert_eq!(
            outcome.answers.get("product"),
            Some(&JevAnswer::Choice {
                choice: "ADC-V724".to_string(),
                confidence: 0.84,
                probabilities: HashMap::from([
                    ("ADC-V724".to_string(), 0.84),
                    ("ADC-V523".to_string(), 0.05),
                ]),
            })
        );
        assert_eq!(
            outcome.answers.get("urgency"),
            Some(&JevAnswer::Score {
                score: 1.0,
                confidence: 0.7,
                probabilities: HashMap::from([
                    ("0".to_string(), 0.1),
                    ("1".to_string(), 0.7),
                    ("2".to_string(), 0.2),
                ]),
                legend: HashMap::from([
                    ("0".to_string(), "低".to_string()),
                    ("1".to_string(), "中".to_string()),
                    ("2".to_string(), "高".to_string()),
                ]),
            })
        );
    }

    #[tokio::test]
    async fn evaluate_sends_state_model_and_questions_in_request_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "answers": {},
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .mount(&server)
            .await;

        let client = test_client(
            format!("{}/v1/systemone", server.uri()),
            Duration::from_secs(5),
        );
        client
            .evaluate("STATE-MARKER", TEST_REQUEST_ID)
            .await
            .expect("stub response must parse");

        let requests = server.received_requests().await.expect("recording enabled");
        assert_eq!(requests.len(), 1);
        let sent: serde_json::Value = requests[0].body_json().expect("request body is json");
        assert_eq!(sent["state"], "STATE-MARKER");
        assert_eq!(sent["model"], "jev-test");
        assert_eq!(sent["questions"]["is_emergency"]["type"], "noul");
        // 本文のトップレベルキーは design doc §1 のリクエスト契約と完全一致させる。
        // 個別キーの値だけを見ていると、キーが 1 つ増えても（社内 ID や追加のメタデータが
        // 外部サービスへ流れ始めても）気付けない。
        let sent_keys: std::collections::BTreeSet<&str> = sent
            .as_object()
            .expect("request body is a json object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            sent_keys,
            std::collections::BTreeSet::from(["model", "questions", "state"]),
            "the request body must contain exactly the documented top-level keys, got: {sent}"
        );
    }

    #[tokio::test]
    async fn evaluate_never_sends_the_request_id_to_jev_in_body_headers_or_url() {
        // `request_id` はログ突合専用。社内の相関 ID を外部サービスへ渡さない。送信面は本文・
        // HTTP ヘッダ・URL（クエリ）の 3 つあり、design doc §2 の「Jev へは送信しない」は
        // そのどれにも当てはまる。どの面へ足されても落ちるよう、それぞれで「キー名（ヘッダ名）
        // にも値にも現れない」ことを固定する。
        const SENTINEL: &str = "req-must-not-be-sent-to-jev";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "answers": {},
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .mount(&server)
            .await;
        let client = test_client(
            format!("{}/v1/systemone", server.uri()),
            Duration::from_secs(5),
        );

        client
            .evaluate("state", SENTINEL)
            .await
            .expect("stub response must parse");

        let requests = server.received_requests().await.expect("recording enabled");
        assert_eq!(requests.len(), 1);
        let request = &requests[0];

        // 本文: 入れ子を含む全体にキー名も値も現れない。
        let body_text = String::from_utf8_lossy(&request.body);
        assert!(
            !body_text.contains("request_id") && !body_text.contains(SENTINEL),
            "request_id must not appear in the request body, got: {body_text}"
        );
        // ヘッダ: 名前は小文字化済みで、`_` と `-` の表記ゆれ（`request_id` / `x-request-id` /
        // `request-id`）を `-` へ寄せて判定する。
        for (name, value) in &request.headers {
            let normalized_name = name.as_str().replace('_', "-");
            let value_text = String::from_utf8_lossy(value.as_bytes());
            assert!(
                !normalized_name.contains("request-id"),
                "request_id must not be sent as an HTTP header, got header name: {name}"
            );
            assert!(
                !value_text.contains(SENTINEL),
                "request_id must not be sent as an HTTP header value, got: {name}: {value_text}"
            );
        }
        // URL（クエリを含む）。
        let url = request.url.as_str();
        assert!(
            !url.contains("request_id") && !url.contains(SENTINEL),
            "request_id must not be sent in the request URL, got: {url}"
        );
    }

    #[tokio::test]
    async fn evaluate_discards_unknown_answer_type_but_keeps_others_and_warns() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "answers": {
                    "is_emergency": {"type": "noul", "noul": 0.9},
                    "future_field": {"type": "vector", "vector": [1, 2, 3]}
                },
                "usage": {"input_tokens": 10, "output_tokens": 5}
            })))
            .mount(&server)
            .await;

        let client = test_client(server.uri(), Duration::from_secs(5));
        let (result, logs) = capture_logs_async(client.evaluate("state", TEST_REQUEST_ID)).await;
        let outcome = result.expect("unknown answer type must not fail the whole response");

        assert_eq!(
            outcome.answers.len(),
            1,
            "only the known answer must survive"
        );
        assert_eq!(
            outcome.answers.get("is_emergency"),
            Some(&JevAnswer::Noul { noul: 0.9 })
        );
        assert!(
            !outcome.answers.contains_key("future_field"),
            "the unknown-type answer must not appear in the outcome"
        );

        let warnings = filter_warn_and_error_lines(&logs);
        assert!(
            warnings.contains("future_field") && warnings.contains("vector"),
            "a WARN line must name the discarded answer id and its type, got: {warnings}"
        );
    }

    #[tokio::test]
    async fn evaluate_discards_answer_with_missing_required_field_but_keeps_others() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "answers": {
                    "is_emergency": {"type": "noul", "noul": 0.9},
                    "product": {"type": "choice", "confidence": 0.5}
                },
                "usage": {"input_tokens": 10, "output_tokens": 5}
            })))
            .mount(&server)
            .await;

        let client = test_client(server.uri(), Duration::from_secs(5));
        let (result, logs) = capture_logs_async(client.evaluate("state", TEST_REQUEST_ID)).await;
        let outcome = result.expect("a malformed answer entry must not fail the whole response");

        assert_eq!(outcome.answers.len(), 1);
        assert!(!outcome.answers.contains_key("product"));
        let warnings = filter_warn_and_error_lines(&logs);
        assert!(
            warnings.contains("product") && warnings.contains("choice"),
            "got: {warnings}"
        );
    }

    #[tokio::test]
    async fn evaluate_parse_failure_warn_carries_the_request_id() {
        // `/api/reply` は並行処理される。捨てた回答の理由（この warn）と、呼び出し側
        // （`api.rs`）の `answer_kind=missing` warn を結ぶ相関 ID がここに無いと、運用者は
        // 時刻近傍で当て推量するしかなく、他リクエストのパース失敗を取り違える。
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "answers": {
                    "is_emergency": {"type": "noul", "noul": 0.9},
                    "future_field": {"type": "vector", "vector": [1, 2, 3]}
                },
                "usage": {"input_tokens": 10, "output_tokens": 5}
            })))
            .mount(&server)
            .await;

        let client = test_client(server.uri(), Duration::from_secs(5));
        let (result, logs) = capture_logs_async(client.evaluate("state", "req-jev-unit-7")).await;
        result.expect("an unparseable answer entry must not fail the whole response");

        let warnings = filter_warn_and_error_lines(&logs);
        let discard_line = warnings
            .lines()
            .find(|line| line.contains("answer_id=future_field"))
            .unwrap_or_else(|| {
                panic!("the discarded entry must be reported by a WARN line, got: {warnings}")
            });
        assert!(
            discard_line.contains("request_id=req-jev-unit-7"),
            "the parse-failure WARN must carry the request_id so it can be joined with the \
             caller's warn, got: {discard_line}"
        );
    }

    // `noul` は #58 で閾値判定に使うため、範囲外の値を静かに通すと誤判定に直結する。
    // NaN は JSON リテラルとして表現できない(wiremock 経由の HTTP レスポンスでは再現不能)
    // ため、純粋関数として直接ユニットテストする。
    #[test]
    fn is_valid_noul_rejects_out_of_range_nan_and_infinite() {
        assert!(!is_valid_noul(-0.1), "below the [0.0, 1.0] range");
        assert!(!is_valid_noul(1.5), "above the [0.0, 1.0] range");
        assert!(!is_valid_noul(f64::NAN), "NaN must not compare as in-range");
        assert!(!is_valid_noul(f64::INFINITY), "+inf must be rejected");
        assert!(!is_valid_noul(f64::NEG_INFINITY), "-inf must be rejected");
        assert!(is_valid_noul(0.0), "lower bound is inclusive");
        assert!(is_valid_noul(1.0), "upper bound is inclusive");
        assert!(is_valid_noul(0.5), "a typical mid-range value");
    }

    #[tokio::test]
    async fn evaluate_discards_out_of_range_noul_but_keeps_others_and_warns() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "answers": {
                    "is_emergency": {"type": "noul", "noul": 0.9},
                    "out_of_range": {"type": "noul", "noul": 1.5}
                },
                "usage": {"input_tokens": 10, "output_tokens": 5}
            })))
            .mount(&server)
            .await;

        let client = test_client(server.uri(), Duration::from_secs(5));
        let (result, logs) = capture_logs_async(client.evaluate("state", TEST_REQUEST_ID)).await;
        let outcome = result.expect("an out-of-range noul must not fail the whole response");

        assert_eq!(
            outcome.answers.len(),
            1,
            "only the in-range answer must survive"
        );
        assert_eq!(
            outcome.answers.get("is_emergency"),
            Some(&JevAnswer::Noul { noul: 0.9 })
        );
        assert!(
            !outcome.answers.contains_key("out_of_range"),
            "the out-of-range noul answer must not appear in the outcome"
        );

        let warnings = filter_warn_and_error_lines(&logs);
        assert!(
            warnings.contains("out_of_range") && warnings.contains("1.5"),
            "a WARN line must name the discarded answer id and its out-of-range value, \
             got: {warnings}"
        );
    }

    #[tokio::test]
    async fn evaluate_errs_on_malformed_response_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{ not valid json"))
            .mount(&server)
            .await;

        let client = test_client(server.uri(), Duration::from_secs(5));
        let err = client
            .evaluate("state", TEST_REQUEST_ID)
            .await
            .expect_err("malformed top-level JSON must be an error, not a panic");
        assert!(err.to_string().contains("parse"));
    }

    #[tokio::test]
    async fn evaluate_errs_on_oversized_response_declared_by_content_length() {
        let server = MockServer::start().await;
        // wiremock は固定長 body にしか対応せず、`set_body_string` は必ず content-length を
        // 付けてしまう(実測: `content-length: 1048577`)。したがってこのテストが検証できるのは
        // content-length による事前チェックだけ。content-length の無い chunked 応答での
        // 逐次読み込み側の上限は `evaluate_errs_on_oversized_chunked_response_without_content_length`
        // で別途検証する(このテストを緩い assert のまま残すと、逐次読み込み側の上限チェックを
        // 削除しても検出できない = mutation-blind になる)。
        let oversized_body = "x".repeat(MAX_RESPONSE_BYTES + 1);
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(oversized_body))
            .mount(&server)
            .await;

        let client = test_client(server.uri(), Duration::from_secs(5));
        let err = client
            .evaluate("state", TEST_REQUEST_ID)
            .await
            .expect_err("a response body over the size cap must be rejected, not parsed");
        let message = err.to_string();
        assert!(
            message.contains("content-length"),
            "this response always carries a content-length header (wiremock fixed-length \
             body), so the error must come from the content-length precheck specifically, \
             got: {message}"
        );
    }

    /// `evaluate_errs_on_oversized_chunked_response_without_content_length` 専用のスタブ。
    /// wiremock は固定長 body にしか対応せず必ず `content-length` を付けてしまうため
    /// (上のテストのコメント参照)、content-length の無い `Transfer-Encoding: chunked` 応答を
    /// 作るにはこの生 TCP スタブが要る。リクエストを読み切ってから応答する手法は
    /// `llm.rs::test_support::spawn_messages_stub` と同じ(読み切る前に書き始めると、client が
    /// まだ送信中の接続をこちらから切ることになる)。
    async fn spawn_oversized_chunked_stub() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub listener");
        let addr = listener.local_addr().expect("stub listener local addr");
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
                if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            if stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            // MAX_RESPONSE_BYTES ちょうどのチャンクを 2 回書く(content-length が無いので事前
            // チェックは素通りし、合計が上限を超えるのは 2 チャンク目の途中)。client は上限超過を
            // 検出した時点で読み込みをやめて接続を切るはずなので、2 回目の書き込みが失敗するのは
            // このテストが検証したい挙動そのもの — panic させず黙って抜ける。
            let chunk = vec![b'x'; MAX_RESPONSE_BYTES];
            for _ in 0..2 {
                let mut framed = format!("{:x}\r\n", chunk.len()).into_bytes();
                framed.extend_from_slice(&chunk);
                framed.extend_from_slice(b"\r\n");
                if stream.write_all(&framed).await.is_err() {
                    return;
                }
            }
            // 終端チャンクを送る。無いと、逐次上限判定を削除する mutation を入れたときに
            // このテストが落ちる理由が「巨大な正常 body を読み切って JSON parse で失敗」では
            // なく「chunked body が終端されないまま接続が閉じた」不完全 body エラーになり、
            // 何を検出したテストなのか因果関係が不明瞭になる。書き込み失敗時に panic させない
            // のは、client が上限超過を検出して先に切断するのがこのテストの期待挙動そのもの
            // だから（上のコメント参照）。
            let _ = stream.write_all(b"0\r\n\r\n").await;
        });
        format!("http://{addr}/v1/systemone")
    }

    #[tokio::test]
    async fn evaluate_errs_on_oversized_chunked_response_without_content_length() {
        let endpoint = spawn_oversized_chunked_stub().await;
        let client = test_client(endpoint, Duration::from_secs(5));
        let err = client.evaluate("state", TEST_REQUEST_ID).await.expect_err(
            "a chunked response with no content-length header must still be capped by the \
             incremental reader, not buffered in full before checking",
        );
        let message = err.to_string();
        assert!(
            !message.contains("content-length"),
            "this response has no content-length header, so the content-length precheck \
             must not be the one reporting this, got: {message}"
        );
        assert!(
            message.contains("while streaming"),
            "error must come from the incremental-read cap wording, not a generic parse \
             failure (which is what you'd get if the cap check were silently removed and the \
             stub's abrupt disconnect were mistaken for something else), got: {message}"
        );
    }

    /// `evaluate_accepts_response_body_exactly_at_the_byte_cap` 専用のスタブ。上限ちょうどの
    /// バイト数を単一 chunk として送り、`0\r\n\r\n` で正しく終端する（content-length が無いのは
    /// `spawn_oversized_chunked_stub` と同じ理由）。本文は妥当な JSON ではないため後段の
    /// JSON parse では失敗するが、それはサイズ上限チェックとは別のエラーであり、
    /// ちょうど `MAX_RESPONSE_BYTES` の本文がサイズ上限では拒否されないことの証拠になる。
    async fn spawn_exact_cap_chunked_stub() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub listener");
        let addr = listener.local_addr().expect("stub listener local addr");
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
                if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            if stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            let chunk = vec![b'x'; MAX_RESPONSE_BYTES];
            let mut framed = format!("{:x}\r\n", chunk.len()).into_bytes();
            framed.extend_from_slice(&chunk);
            framed.extend_from_slice(b"\r\n0\r\n\r\n");
            let _ = stream.write_all(&framed).await;
        });
        format!("http://{addr}/v1/systemone")
    }

    #[tokio::test]
    async fn evaluate_accepts_response_body_exactly_at_the_byte_cap() {
        // 修正2の境界確認: 「追加前判定」に変えても、ちょうど MAX_RESPONSE_BYTES の本文は
        // 引き続き受理されること（1バイトでも境界がずれていないこと）を検証する。
        let endpoint = spawn_exact_cap_chunked_stub().await;
        let client = test_client(endpoint, Duration::from_secs(5));
        let err = client.evaluate("state", TEST_REQUEST_ID).await.expect_err(
            "the stub body is not valid JSON, so evaluate must still fail overall — but the \
             failure must come from JSON parsing, not the size cap, which is what proves a \
             body of exactly MAX_RESPONSE_BYTES is not rejected by the pre-append size check",
        );
        let message = err.to_string();
        assert!(
            !message.contains("byte cap"),
            "a body of exactly MAX_RESPONSE_BYTES must not be rejected by the size cap, \
             got: {message}"
        );
        assert!(
            message.contains("parse"),
            "the only expected failure at exactly the cap is JSON parsing of the (intentionally \
             invalid) stub body, got: {message}"
        );
    }

    #[tokio::test]
    async fn evaluate_does_not_follow_redirects() {
        let redirect_target = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "answers": {},
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .mount(&redirect_target)
            .await;

        let redirector = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(307).insert_header(
                "Location",
                format!("{}/v1/systemone", redirect_target.uri()),
            ))
            .mount(&redirector)
            .await;

        let client = test_client(redirector.uri(), Duration::from_secs(5));
        let err = client.evaluate("state", TEST_REQUEST_ID).await.expect_err(
            "a 307 redirect must not be followed transparently: validate_endpoint only \
             checks the configured endpoint, not the redirect target, and 307 resends the POST \
             body (customer utterances) to wherever Location points",
        );
        assert!(
            err.to_string().contains("307"),
            "the 307 itself must surface as the error status, got: {err}"
        );

        let redirected_requests = redirect_target
            .received_requests()
            .await
            .expect("recording enabled");
        assert!(
            redirected_requests.is_empty(),
            "the redirect target must never receive a request; if it does, the client is \
             following redirects and both the api key and the customer utterance can leak to \
             an uncontrolled destination"
        );
    }

    /// `evaluate_does_not_follow_redirects` は `test_client()` が組み立てたクライアントを検証
    /// している。`test_client()` は `build()` を経由せず構造体を直接組み立て、自前で
    /// `.redirect(Policy::none())` を設定している（`test_client` の doc 参照）ため、将来誰かが
    /// `build()` から `.redirect(Policy::none())` だけを削除しても、上のテストは
    /// `test_client` 側の設定が生きているので気づかずに通ってしまう。このテストは本番の
    /// `build()` を実際に通して生成した `JevClient` でリダイレクト非追従を検証することで、
    /// その退行を検出できるようにする。
    ///
    /// `validate_endpoint` は `http://127.0.0.1` を許可するため、wiremock の
    /// `server.uri()`（`http://127.0.0.1:<port>`）をそのまま `JevConfig.endpoint` に渡して
    /// `build()` を通せる。
    #[tokio::test]
    async fn evaluate_from_built_client_does_not_follow_redirects() {
        let redirect_target = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "answers": {},
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .mount(&redirect_target)
            .await;

        let redirector = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(307).insert_header(
                "Location",
                format!("{}/v1/systemone", redirect_target.uri()),
            ))
            .mount(&redirector)
            .await;

        let cfg = JevConfig {
            enabled: true,
            endpoint: redirector.uri(),
            questions_path: valid_questions_path(),
            ..Default::default()
        };
        let client = JevClient::build("test-key".to_string(), &cfg, &unused_config_dir())
            .expect("build() must succeed for a loopback http endpoint");

        let err = client.evaluate("state", TEST_REQUEST_ID).await.expect_err(
            "a 307 redirect must not be followed transparently by a client built through the \
             production build() path",
        );
        assert!(
            err.to_string().contains("307"),
            "the 307 itself must surface as the error status, got: {err}"
        );

        let redirected_requests = redirect_target
            .received_requests()
            .await
            .expect("recording enabled");
        assert!(
            redirected_requests.is_empty(),
            "the redirect target must never receive a request; if build() ever drops \
             .redirect(Policy::none()), this is the test that must catch it (test_client() \
             cannot, since it sets the policy itself independently of build())"
        );
    }

    #[tokio::test]
    async fn evaluate_errs_on_http_401() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let client = test_client(server.uri(), Duration::from_secs(5));
        let err = client
            .evaluate("state", TEST_REQUEST_ID)
            .await
            .expect_err("HTTP 401 must be an error");
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn evaluate_errs_on_http_429() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let client = test_client(server.uri(), Duration::from_secs(5));
        let err = client
            .evaluate("state", TEST_REQUEST_ID)
            .await
            .expect_err("HTTP 429 must be an error");
        assert!(err.to_string().contains("429"));
    }

    #[tokio::test]
    async fn evaluate_errs_on_http_529() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(529))
            .mount(&server)
            .await;

        let client = test_client(server.uri(), Duration::from_secs(5));
        let err = client
            .evaluate("state", TEST_REQUEST_ID)
            .await
            .expect_err("HTTP 529 must be an error");
        assert!(err.to_string().contains("529"));
    }

    #[tokio::test]
    async fn evaluate_errs_on_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(500)))
            .mount(&server)
            .await;

        // per-request timeout をサーバの遅延より短くして、確実にタイムアウト経路を通す。
        let client = test_client(server.uri(), Duration::from_millis(50));
        let err = client
            .evaluate("state", TEST_REQUEST_ID)
            .await
            .expect_err("a response slower than the client timeout must be an error");
        // タイムアウトは、こちらが所有する安定した文言（設定値入り）で判別できること。
        // reqwest / hyper の文言（`operation timed out` 等）には依存しない。
        assert_eq!(
            err.to_string(),
            "jev evaluate api timed out after 0.05s",
            "a timeout must be labelled with the stable phrase and the configured timeout"
        );
        // 原因（reqwest のエラー）は context の下にチェーンとして残る。呼び出し側が
        // `{err:#}` で出せるよう、握りつぶさないことを固定する（文言には立ち入らない）。
        assert!(
            format!("{err:#}").starts_with("jev evaluate api timed out after 0.05s: "),
            "the underlying cause must remain in the error chain, got: {err:#}"
        );
    }

    #[tokio::test]
    async fn evaluate_keeps_the_generic_context_for_a_send_error_that_is_not_a_timeout() {
        // タイムアウト以外の送出エラー（ここでは接続拒否）は従来の context のままで、
        // 「タイムアウトした」と誤って名乗らない（運用者が原因を取り違えないため）。
        // 空きポートを確保してすぐ閉じることで、接続拒否を決定論的に再現する。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("read the bound address");
        drop(listener);

        let client = test_client(format!("http://{addr}"), Duration::from_secs(5));
        let err = client
            .evaluate("state", TEST_REQUEST_ID)
            .await
            .expect_err("connecting to a closed port must be an error");

        assert_eq!(err.to_string(), "call jev evaluate api");
        assert!(
            !format!("{err:#}").contains("timed out after"),
            "a non-timeout failure must not be labelled as a timeout, got: {err:#}"
        );
    }
}
