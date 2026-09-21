//! Jev（TypeSafe System One）shadow 判定クライアント。
//!
//! **shadow 専用で、どの呼び出し経路にも配線されていない。** Issue #56 の範囲はこの
//! クライアントと config のみで、`evaluate` を呼ぶ側（`POST /{project_id}/api/reply` からの
//! fire-and-forget 起動、結果の記録）は Issue #57 で別途実装する。このファイルの型・関数は
//! 現時点でどこからも参照されない（`main.rs` / `api.rs` / `mcp.rs` / `harness/` 配下のどこにも
//! 配線しないこと）。判定・分岐・応答生成に一切使わない設計（design doc §1〜§3 の不変条件）。
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

/// Anthropic 同様の入出力トークン数（課金・較正の実測用。design doc §1 の実測値参照）。
#[derive(Debug, Clone, PartialEq)]
pub struct JevUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// `evaluate` の戻り値。**現時点でどの呼び出し側も存在しない**（Issue #57 で配線される）。
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
    fn build(api_key: String, cfg: &JevConfig, config_dir: &Path) -> Result<Self> {
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
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build()
            .context("build reqwest client for JevClient")?;
        Ok(Self {
            http,
            endpoint: cfg.endpoint.clone(),
            model: cfg.model.clone(),
            api_key,
            questions,
        })
    }

    /// 顧客発話（`state`）を Jev へ送り、質問定義に対する回答を得る。
    ///
    /// **呼び出し側の責務（design doc §2・§3、Issue #57 で実装）**: `tokio::spawn` で
    /// fire-and-forget にし、失敗・タイムアウトを応答内容・応答時間に一切波及させないこと。
    /// このメソッド自体は同期的に `Result` を返すだけで、リトライ・タイムアウト後の劣化判断は
    /// 呼び出し側の責務。
    pub async fn evaluate(&self, state: &str) -> Result<JevOutcome> {
        let payload = serde_json::json!({
            "state": state,
            "model": self.model,
            "questions": self.questions,
        });
        let body = serde_json::to_vec(&payload).context("serialize jev evaluate request body")?;

        let response = self
            .http
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .context("call jev evaluate api")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("read jev evaluate api response body")?;
        // レスポンス本文はエラーに含めない（llm.rs と同じログ衛生方針）。status のみ残す。
        if !status.is_success() {
            bail!("jev evaluate api returned {status}");
        }

        let parsed: RawJevResponse =
            serde_json::from_str(&text).context("parse jev evaluate api response as json")?;
        Ok(JevOutcome {
            answers: parse_answers(parsed.answers),
            usage: JevUsage {
                input_tokens: parsed.usage.input_tokens,
                output_tokens: parsed.usage.output_tokens,
            },
        })
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
fn parse_answers(raw: HashMap<String, serde_json::Value>) -> HashMap<String, JevAnswer> {
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
                    // id / type / 理由のみを出す（value そのものは出さない）。
                    tracing::warn!(
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
    fn test_client(endpoint: String, timeout: Duration) -> JevClient {
        JevClient {
            http: reqwest::Client::builder().timeout(timeout).build().unwrap(),
            endpoint,
            model: "jev-test".to_string(),
            api_key: "test-key".to_string(),
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
            .evaluate("電源が入りません")
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
            .evaluate("STATE-MARKER")
            .await
            .expect("stub response must parse");

        let requests = server.received_requests().await.expect("recording enabled");
        assert_eq!(requests.len(), 1);
        let sent: serde_json::Value = requests[0].body_json().expect("request body is json");
        assert_eq!(sent["state"], "STATE-MARKER");
        assert_eq!(sent["model"], "jev-test");
        assert_eq!(sent["questions"]["is_emergency"]["type"], "noul");
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
        let (result, logs) = capture_logs_async(client.evaluate("state")).await;
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
        let (result, logs) = capture_logs_async(client.evaluate("state")).await;
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
    async fn evaluate_errs_on_malformed_response_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{ not valid json"))
            .mount(&server)
            .await;

        let client = test_client(server.uri(), Duration::from_secs(5));
        let err = client
            .evaluate("state")
            .await
            .expect_err("malformed top-level JSON must be an error, not a panic");
        assert!(err.to_string().contains("parse"));
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
            .evaluate("state")
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
            .evaluate("state")
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
            .evaluate("state")
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
            .evaluate("state")
            .await
            .expect_err("a response slower than the client timeout must be an error");
        assert!(err.to_string().contains("call jev evaluate api"));
    }
}
