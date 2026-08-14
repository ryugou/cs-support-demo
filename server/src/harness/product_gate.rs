//! 取扱製品スコープ（Issue #28）。
//!
//! 正本は `docs/superpowers/specs/2026-08-14-product-scope-design.md`。「当社が取り扱う製品以外
//! には答えない」を会話全体（質問側ゲート・材料選別・聞き返し/回答プロンプト・応答側ゲート）の
//! 前提として注入するための共通部品を、ここに集約する。
//!
//! **命名の注意**: `harness::scope` は AuthZ の [`crate::harness::scope::AccessScope`] を定義して
//! おり無関係。ここでは「取扱製品スコープ」だけを扱うため、モジュール名は `product_gate` とし
//! `scope` を使わない（衝突・混同を避ける）。
//!
//! 型番の allowlist（正本は vegapunk の `Product` ノード）はコードへハードコードしない。
//! `ProductGate::allowlist` が schema 単位に TTL 10 分でキャッシュしつつ都度解決する。

use crate::manual::schema_ids::KIND_PRODUCT;
use crate::vegapunk::VegapunkClient;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use unicode_normalization::UnicodeNormalization;

/// 製品マスタ（Product ノード一覧）の TTL キャッシュ既定値（design doc §2）。
const PRODUCT_ALLOWLIST_TTL: Duration = Duration::from_secs(600);

/// 型番トークンの抽出パターン（design doc §3.1 で確定した正規表現そのもの）。
/// `(?i)` で大文字小文字を無視するため、`[A-Z0-9]` は `a-z0-9` にもマッチする。
fn model_token_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)ADC-[A-Z0-9][A-Z0-9-]*").expect("model token regex must compile")
    })
}

/// 型番トークン比較用の正規化: NFKC（全角英数・全角ハイフンを半角へ）してから大文字化する
/// （design doc §3.1）。
fn normalize_model_token(raw: &str) -> String {
    raw.nfkc().collect::<String>().to_uppercase()
}

/// テキストから型番トークンを検出順（出現順）に抽出する。抽出前に文字列全体を NFKC 正規化し、
/// マッチした断片を大文字化して返す（比較は常にこの正規化後の形で行う）。
///
/// `質問側ゲート`（§3.1）・`材料選別`（§3.2）・`応答側ゲート`（§3.5）の 3 箇所が共通で使う
/// 唯一の抽出経路（重複実装しない）。
pub fn extract_model_tokens(text: &str) -> Vec<String> {
    let normalized: String = text.nfkc().collect();
    model_token_regex()
        .find_iter(&normalized)
        .map(|m| {
            // `[A-Z0-9-]*` は貪欲マッチのため、顧客が区切りに打つハイフンや原文の折返し由来の
            // 末尾ハイフンまで取り込む（例:「ADC-V724-の設定」→ `ADC-V724-`）。これを trim せず
            // allowlist と比較すると、取扱内の製品が一致せず誤って「取り扱いがございません」と
            // 断ってしまう。正規表現は `ADC-` の直後に `[A-Z0-9]` を必ず 1 文字要求するため、
            // 末尾ハイフンを落としても空文字にはならない。語中のハイフン（多段ハイフン型番、
            // 例 `ADC-SEM100-ADT`）は末尾ではないため保持される。
            m.as_str().to_uppercase().trim_end_matches('-').to_string()
        })
        .collect()
}

/// 取扱製品の allowlist（1 schema 分のスナップショット）。
///
/// 正規化済み型番の集合（大文字比較用）と、顧客向け表示文字列（ソート済みで「、」連結）の
/// 両方を持つ。生成は [`ProductAllowlist::from_models`] / [`ProductGate::allowlist`] 経由のみ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductAllowlist {
    normalized: HashSet<String>,
    display: String,
}

impl ProductAllowlist {
    /// 製品マスタの `model` 属性値一覧（vegapunk 応答順。順序は問わない）から allowlist を
    /// 組み立てる。表示文字列（design doc §4）はここで一度だけ組み立て、以降は使い回す。
    ///
    /// 各値は trim してから使う。trim 後に空文字になった値（空白のみの `model` 属性）は、
    /// 表示文字列を「ADC-V724、、ADC-V523」のように壊さないため除外し、`from_nodes` の
    /// 「`model` 属性欠落ノードを除外する」warn と同じ思想で `tracing::warn!` を出す
    /// （運用者が allowlist から消えた理由をログだけで追えるように）。
    ///
    /// 表示文字列はソートして組み立てる。vegapunk（`query_nodes`）の応答順は安定性が保証
    /// されないため、ソートしないと Merge・再インデックス等のたびに顧客向け断り文の型番順が
    /// 変わりうる（design doc §2/§4 は組み立て方法のみを規定し、マスタ順の維持は要求していない）。
    pub fn from_models(models: Vec<String>) -> Self {
        let mut usable: Vec<String> = Vec::with_capacity(models.len());
        for model in models {
            let trimmed = model.trim();
            if trimmed.is_empty() {
                tracing::warn!(
                    "product master 'model' value is blank after trimming; excluding it from \
                     the product scope allowlist"
                );
                continue;
            }
            usable.push(trimmed.to_string());
        }
        usable.sort();
        let normalized = usable.iter().map(|m| normalize_model_token(m)).collect();
        let display = usable.join("、");
        Self {
            normalized,
            display,
        }
    }

    /// allowlist に実際に使える型番が 1 件も無いか（`ProductGate::fetch` の fail-closed 判定
    /// [`validate_allowlist_not_empty`] に使う）。
    fn is_empty(&self) -> bool {
        self.normalized.is_empty()
    }

    /// `query_nodes` の `NodeResult` 一覧から allowlist を組み立てる。`model` 属性が欠けた
    /// ノードはスキーマ上あり得ないはずだが、fail-closed で黙って落とさず warn して除外する
    /// （運用者が「なぜ allowlist から消えたか」をログだけで追えるように）。
    pub(crate) fn from_nodes(nodes: Vec<crate::proto::graphrag::NodeResult>) -> Self {
        let models: Vec<String> = nodes
            .into_iter()
            .filter_map(|n| match n.attributes.get("model") {
                Some(model) => Some(model.clone()),
                None => {
                    tracing::warn!(
                        node_id = %n.node_id,
                        "product node is missing the required 'model' attribute; excluding it \
                         from the product scope allowlist"
                    );
                    None
                }
            })
            .collect();
        Self::from_models(models)
    }

    /// 正規化済みトークンが allowlist 内か。
    pub fn is_in_scope(&self, normalized_token: &str) -> bool {
        self.normalized.contains(normalized_token)
    }

    /// `text` から検出した型番のうち、allowlist 外の最初の 1 件を返す（§3.1 の質問側ゲート・
    /// §4 の定型応答の `{検出型番}` に使う）。
    pub fn first_out_of_scope_token(&self, text: &str) -> Option<String> {
        extract_model_tokens(text)
            .into_iter()
            .find(|t| !self.is_in_scope(t))
    }

    /// `text` に allowlist 外の型番言及が 1 つでもあるか（§3.5 応答側ゲート用）。
    pub fn has_out_of_scope_mention(&self, text: &str) -> bool {
        extract_model_tokens(text)
            .iter()
            .any(|t| !self.is_in_scope(t))
    }

    /// §3.2 材料選別の判定: `text`（材料の生本文。truncate 前）が allowlist 外の型番だけを
    /// 言及し、allowlist 内の言及が 1 つも無い場合に、除外すべき理由（検出した型番）を返す。
    /// 型番言及が無い材料、allowlist 内言及を含む材料は `None`（除外しない）。
    pub fn out_of_scope_material_exclusion(&self, text: &str) -> Option<String> {
        let tokens = extract_model_tokens(text);
        if tokens.is_empty() {
            return None;
        }
        if tokens.iter().any(|t| self.is_in_scope(t)) {
            return None;
        }
        tokens.into_iter().find(|t| !self.is_in_scope(t))
    }

    /// 顧客向け表示用の一覧文字列（例: `ADC-V523、ADC-V523X、...`。design doc §4）。
    pub fn display_list(&self) -> &str {
        &self.display
    }
}

/// §4 の取扱外定型応答をコードで組み立てる（LLM 不使用・プレーンテキスト）。
pub fn build_out_of_scope_reply(detected_model: &str, allowlist: &ProductAllowlist) -> String {
    format!(
        "申し訳ありません。{detected_model} は当社では取り扱いがございません。\
         当社で取り扱っている製品は {} です。\
         その他の Alarm.com 製品につきましては、ご購入元または Alarm.com 社へお問い合わせください。",
        allowlist.display_list()
    )
}

struct CachedAllowlist {
    stored_at: Instant,
    allowlist: Arc<ProductAllowlist>,
}

/// 製品マスタ（Product ノード）から取扱 allowlist を取得し、schema 単位に TTL キャッシュする
/// コンポーネント（design doc §2）。
///
/// 取得失敗時の挙動: キャッシュがあれば期限切れでも使う（stale 許容、`tracing::warn!` で
/// 可視化する）。キャッシュが一度も無い状態で失敗した場合はエラーをそのまま返す
/// （呼び出し側が `classify_evaluate_error` 等で 503 `upstream_unavailable` に分類する。
/// vegapunk 不達時は検索も成立しないため既存のエラー意味論と一致する）。
pub struct ProductGate {
    client: Arc<VegapunkClient>,
    cache: Mutex<HashMap<String, CachedAllowlist>>,
    ttl: Duration,
}

impl ProductGate {
    pub fn new(client: Arc<VegapunkClient>) -> Self {
        Self::with_ttl(client, PRODUCT_ALLOWLIST_TTL)
    }

    pub fn with_ttl(client: Arc<VegapunkClient>, ttl: Duration) -> Self {
        Self {
            client,
            cache: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    pub async fn allowlist(&self, schema: &str) -> Result<Arc<ProductAllowlist>> {
        if let Some(fresh) = self.cached(schema, true) {
            return Ok(fresh);
        }
        match self.fetch(schema).await {
            Ok(allowlist) => {
                let allowlist = Arc::new(allowlist);
                self.store(schema, allowlist.clone());
                Ok(allowlist)
            }
            Err(err) => {
                if let Some(stale) = self.cached(schema, false) {
                    tracing::warn!(
                        schema,
                        error = ?err,
                        "product allowlist refresh failed; serving the stale cached allowlist \
                         instead of failing the request (product scope master may be out of \
                         date until vegapunk recovers)"
                    );
                    return Ok(stale);
                }
                Err(err)
            }
        }
    }

    /// `require_fresh = true` なら TTL 内のエントリだけを返す。`false` なら年齢を問わず返す
    /// （stale フォールバック用）。
    fn cached(&self, schema: &str, require_fresh: bool) -> Option<Arc<ProductAllowlist>> {
        let guard = self.cache.lock().expect("product allowlist cache poisoned");
        guard
            .get(schema)
            .filter(|c| !require_fresh || is_fresh(c.stored_at.elapsed(), self.ttl))
            .map(|c| c.allowlist.clone())
    }

    fn store(&self, schema: &str, allowlist: Arc<ProductAllowlist>) {
        let mut guard = self.cache.lock().expect("product allowlist cache poisoned");
        guard.insert(
            schema.to_string(),
            CachedAllowlist {
                stored_at: Instant::now(),
                allowlist,
            },
        );
    }

    async fn fetch(&self, schema: &str) -> Result<ProductAllowlist> {
        let nodes = self
            .client
            .query_nodes(schema, KIND_PRODUCT, Vec::new(), PRODUCT_QUERY_LIMIT)
            .await
            .context("query product allowlist (Product nodes) from vegapunk")?;
        validate_product_nodes(&nodes, schema, PRODUCT_QUERY_LIMIT)?;
        let allowlist = ProductAllowlist::from_nodes(nodes);
        validate_allowlist_not_empty(&allowlist, schema)?;
        Ok(allowlist)
    }
}

/// 製品マスタ（Product ノード）取得の問い合わせ上限。`ingest_urtect.rs` の同名の製品マスタ取得
/// （`PRODUCT_QUERY_LIMIT`）と値・意味を揃える。
const PRODUCT_QUERY_LIMIT: i32 = 1000;

/// `ProductGate::fetch` が取得した Product ノード一覧の健全性を検証する純関数（Issue #28
/// Critical 1）。
///
/// 空 allowlist は「全型番が取扱外」を意味し、これを黙って `Ok` にすると:
/// - §3.1 質問側ゲートが取扱製品の質問にまで「取扱一覧が空欄」の誤った断り文を返す
/// - §3.2 材料選別が型番を含む材料を全除外し、§3.5 応答側ゲートが型番に言及する下書きを
///   全破棄する（実質「全件エスカレーション」に化ける）
/// - しかもエラーにならないため運用者が気づけない（サイレント）
///
/// 発生条件は現実的（全体リセット後に `ingest_products` を流す前、schema 取り違え等）なので、
/// fail-closed で `bail!` する。`ingest_urtect.rs` が同じ Product ノード取得に対して採用して
/// いる 2 段の規律（0 件 bail / limit ちょうど bail）をここでも踏襲し、文言もできる限り揃える
/// （重複実装が意味的に drift するのを防ぐ）。
///
/// 「limit ちょうどなら bail」を選び、`query_nodes_paged` へのページング切り替えは選ばなかった
/// 理由: 製品マスタは数十 SKU 規模で、`ingest_urtect.rs` の同じ判断根拠（1000 件規模に近づく
/// 想定が無い）がそのまま成り立つ。ページング実装を追加する価値より、`ingest_urtect.rs` と
/// 実装・文言を完全に揃えて重複コードの意味的ドリフトを防ぐ価値を優先した。
fn validate_product_nodes(
    nodes: &[crate::proto::graphrag::NodeResult],
    schema: &str,
    limit: i32,
) -> Result<()> {
    if nodes.is_empty() {
        anyhow::bail!(
            "no Product nodes found in schema {schema}; the product master is empty — run \
             `ingest_products` first to seed it before serving the product scope gate"
        );
    }
    if nodes.len() as i32 == limit {
        anyhow::bail!(
            "Product node query returned exactly the limit ({limit}) in schema {schema}; this \
             may indicate silent truncation and an incomplete product master — aborting (raise \
             the limit or investigate the product count)"
        );
    }
    Ok(())
}

/// `ProductGate::fetch` が `ProductAllowlist::from_nodes` で組み立てた allowlist を検証する
/// 純関数（Issue #28 Warning 2）。
///
/// `validate_product_nodes` は **ノード数**しか見ないため、「Product ノードは N 件あるが全件
/// `model` 属性欠落（または空白のみ）」だと検証を素通りし、実際に使える型番が 0 件の allowlist
/// が出来てしまう。これは `validate_product_nodes` の 0 件チェックと同じ障害モード
/// （§3.1 が取扱一覧空欄の断り文を返し、§3.2/§3.5 が型番付き材料・下書きを全破棄する＝
/// サイレント全件エスカレーション）なので、ノード数ではなく実際に使える型番数で fail-closed
/// 判定する。
fn validate_allowlist_not_empty(allowlist: &ProductAllowlist, schema: &str) -> Result<()> {
    if allowlist.is_empty() {
        anyhow::bail!(
            "product allowlist for schema {schema} has zero usable models: Product nodes were \
             fetched but none had a non-empty 'model' attribute — check the `ingest_products` \
             input and the 'model' attribute on Product nodes in schema {schema} before serving \
             the product scope gate"
        );
    }
    Ok(())
}

/// TTL キャッシュの鮮度判定。境界（`age == ttl`）は stale 扱い（`corpus.rs::is_fresh` と同じ規律）。
fn is_fresh(age: Duration, ttl: Duration) -> bool {
    age < ttl
}

#[cfg(test)]
impl ProductGate {
    /// テスト専用: 実ネットワークに繋がず、`schema` に `allowlist` を新鮮なキャッシュとして
    /// 埋め込んだ状態で構築する。`harness::mod` のテスト（`draft_customer_reply_via_stub` 等、
    /// VegapunkClient の実接続を張れない同期/擬似テスト環境）が、`Harness::product_gate` を
    /// 経由するコードパス（§3.2/§3.4 のプロンプト注入）を検証するために使う。
    ///
    /// `connect_lazy` は tonic の内部リアクタが Tokio ランタイム下での呼び出しを要求するため、
    /// このメソッドを呼ぶテストは `#[tokio::test]` にすること。
    pub fn seeded_for_test(schema: &str, allowlist: ProductAllowlist) -> Self {
        let client = Arc::new(
            VegapunkClient::connect_lazy("http://127.0.0.1:1", "test")
                .expect("connect_lazy must succeed for a lazy (non-connecting) channel"),
        );
        let gate = Self::new(client);
        gate.store(schema, Arc::new(allowlist));
        gate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- extract_model_tokens（design doc §6: 半角/全角・大小文字・ハイフン揺れ） ----

    #[test]
    fn extract_model_tokens_finds_halfwidth_ascii_token() {
        assert_eq!(
            extract_model_tokens("ADC-V724の設定を教えてください"),
            vec!["ADC-V724"]
        );
    }

    #[test]
    fn extract_model_tokens_normalizes_lowercase_to_uppercase() {
        assert_eq!(
            extract_model_tokens("adc-v724が動きません"),
            vec!["ADC-V724"]
        );
    }

    #[test]
    fn extract_model_tokens_normalizes_fullwidth_alnum_and_hyphen() {
        // "ＡＤＣ－Ｖ７２４" は全角英数字+全角ハイフン。NFKC で半角化してから抽出する。
        assert_eq!(
            extract_model_tokens("ＡＤＣ－Ｖ７２４が反応しません"),
            vec!["ADC-V724"]
        );
    }

    #[test]
    fn extract_model_tokens_finds_multiple_tokens_in_order() {
        assert_eq!(
            extract_model_tokens("ADC-V724とADC-VDB101の違いは何ですか"),
            vec!["ADC-V724", "ADC-VDB101"]
        );
    }

    #[test]
    fn extract_model_tokens_returns_empty_when_no_model_mentioned() {
        assert!(extract_model_tokens("カメラの映像が映りません").is_empty());
    }

    // ---- extract_model_tokens: 末尾ハイフンの trim（Issue #28 Stage1 Warning 1） ----

    #[test]
    fn extract_model_tokens_trims_a_trailing_hyphen_from_the_match() {
        // 顧客が区切りにハイフンを打つ・原文の折返し由来などで型番直後にハイフンが続くことが
        // ある。trim しないと "ADC-V724-" になり、allowlist の "ADC-V724" と一致しない。
        assert_eq!(
            extract_model_tokens("ADC-V724-の設定を教えてください"),
            vec!["ADC-V724"]
        );
    }

    #[test]
    fn extract_model_tokens_trims_multiple_trailing_hyphens() {
        assert_eq!(extract_model_tokens("ADC-V724--"), vec!["ADC-V724"]);
    }

    #[test]
    fn extract_model_tokens_keeps_interior_hyphens_of_multi_segment_models() {
        // "ADC-SEM100-ADT" のような多段ハイフン型番は、末尾ではなく語中のハイフンなので保持する
        // （trim_end_matches は末尾からしか削らない）。
        assert_eq!(
            extract_model_tokens("ADC-SEM100-ADTの互換品ですか"),
            vec!["ADC-SEM100-ADT"]
        );
    }

    // ---- ProductAllowlist ----

    fn fixture_allowlist() -> ProductAllowlist {
        ProductAllowlist::from_models(vec![
            "ADC-V523".to_string(),
            "ADC-V523X".to_string(),
            "ADC-V724".to_string(),
            "ADC-V724X".to_string(),
            "ADC-VC729P".to_string(),
            "ADC-VC727P".to_string(),
            "ADC-VC827P".to_string(),
        ])
    }

    // ---- ProductAllowlist::from_models（Stage1 Suggestion 4: trim / Suggestion 5: sort） ----

    #[test]
    fn from_models_trims_whitespace_around_each_model_value() {
        let allow =
            ProductAllowlist::from_models(vec![" ADC-V724 ".to_string(), "ADC-V523".to_string()]);
        assert!(allow.is_in_scope("ADC-V724"));
        assert_eq!(allow.display_list(), "ADC-V523、ADC-V724");
    }

    #[test]
    fn from_models_excludes_a_value_that_is_blank_after_trimming_and_warns() {
        let (allow, logs) = capture_warnings_sync(|| {
            ProductAllowlist::from_models(vec!["ADC-V724".to_string(), "   ".to_string()])
        });
        // 表示文字列が「ADC-V724、」のように壊れず、空要素を含まない。
        assert_eq!(allow.display_list(), "ADC-V724");
        assert!(
            logs.contains("WARN"),
            "a blank model value must be warned so operators can see why it was dropped: {logs}"
        );
    }

    #[test]
    fn from_models_sorts_the_display_list_regardless_of_input_order() {
        let allow = ProductAllowlist::from_models(vec![
            "ADC-VC827P".to_string(),
            "ADC-V523".to_string(),
            "ADC-VC727P".to_string(),
        ]);
        assert_eq!(allow.display_list(), "ADC-V523、ADC-VC727P、ADC-VC827P");
    }

    #[test]
    fn is_in_scope_accepts_normalized_in_scope_token_and_rejects_out_of_scope() {
        let allow = fixture_allowlist();
        assert!(allow.is_in_scope("ADC-V724"));
        assert!(!allow.is_in_scope("ADC-VDB101"));
    }

    #[test]
    fn first_out_of_scope_token_is_none_when_only_in_scope_models_mentioned() {
        let allow = fixture_allowlist();
        assert_eq!(allow.first_out_of_scope_token("ADC-V724の設定"), None);
    }

    #[test]
    fn first_out_of_scope_token_is_none_when_no_model_mentioned() {
        let allow = fixture_allowlist();
        assert_eq!(allow.first_out_of_scope_token("映像が映りません"), None);
    }

    #[test]
    fn first_out_of_scope_token_returns_the_first_out_of_scope_model() {
        let allow = fixture_allowlist();
        assert_eq!(
            allow.first_out_of_scope_token("ADC-VDB101とADC-V724の違いは"),
            Some("ADC-VDB101".to_string())
        );
    }

    #[test]
    fn first_out_of_scope_token_is_none_for_an_in_scope_model_followed_by_a_trailing_hyphen() {
        // Issue #28 Stage1 Warning 1 の境界の実効テスト: 末尾ハイフンを trim しないと
        // "ADC-V724-" が allowlist の "ADC-V724" と一致せず、取扱内の製品を誤って
        // 「取り扱いがございません」と断ってしまう。fixture_allowlist は ADC-V724 を含む。
        let allow = fixture_allowlist();
        assert_eq!(
            allow.first_out_of_scope_token("ADC-V724-の設定を教えてください"),
            None
        );
    }

    #[test]
    fn has_out_of_scope_mention_true_for_out_of_scope_and_false_for_in_scope_only() {
        let allow = fixture_allowlist();
        assert!(allow.has_out_of_scope_mention("ADC-VDB101はどうですか"));
        assert!(!allow.has_out_of_scope_mention("ADC-V724はどうですか"));
        assert!(!allow.has_out_of_scope_mention("型番の記載はありません"));
    }

    // ---- out_of_scope_material_exclusion（design doc §3.2 / §6） ----

    #[test]
    fn material_exclusion_excludes_when_only_out_of_scope_models_are_mentioned() {
        let allow = fixture_allowlist();
        assert_eq!(
            allow.out_of_scope_material_exclusion("ADC-VDB101の初期設定手順です"),
            Some("ADC-VDB101".to_string())
        );
    }

    #[test]
    fn material_exclusion_keeps_material_that_also_mentions_an_in_scope_model() {
        let allow = fixture_allowlist();
        // ADC-VDB101 も出てくるが ADC-V724 の言及もあるので除外しない。
        assert_eq!(
            allow.out_of_scope_material_exclusion("ADC-V724とADC-VDB101は共通の手順です"),
            None
        );
    }

    #[test]
    fn material_exclusion_keeps_material_with_no_model_mention() {
        let allow = fixture_allowlist();
        assert_eq!(
            allow.out_of_scope_material_exclusion("Wi-Fiの再接続手順です"),
            None
        );
    }

    // ---- build_out_of_scope_reply（design doc §4） ----

    #[test]
    fn out_of_scope_reply_contains_detected_model_and_sorted_full_allowlist() {
        // fixture_allowlist は "ADC-VC729P", "ADC-VC727P", "ADC-VC827P" の順（マスタ順を模した
        // 非ソート順）で渡しているが、`from_models` がソートするため、vegapunk の応答順が
        // 変わっても顧客向け文面の型番順は常にこの並びで安定する（Issue #28 Stage1 Suggestion 5）。
        let allow = fixture_allowlist();
        let text = build_out_of_scope_reply("ADC-VDB101", &allow);
        assert!(text.contains("ADC-VDB101"));
        assert!(text.contains("当社では取り扱いがございません"));
        assert!(text.contains(
            "ADC-V523、ADC-V523X、ADC-V724、ADC-V724X、ADC-VC727P、ADC-VC729P、ADC-VC827P"
        ));
        assert!(text.contains("Alarm.com 社へお問い合わせください"));
    }

    // ---- ProductGate（design doc §2 / §6: キャッシュ利用・stale 利用・初回失敗→エラー） ----

    /// 実ネットワークに繋がらない dummy client。`ManualStore` のテスト
    /// （`manual/retrieval.rs` の `dummy_store`）と同じパターンで、`127.0.0.1:1` は
    /// 到達不能ポートのため、実際に RPC を発行すればほぼ即座に接続拒否で失敗する。
    fn unreachable_client() -> Arc<VegapunkClient> {
        Arc::new(VegapunkClient::connect_lazy("http://127.0.0.1:1", "test").expect("connect_lazy"))
    }

    /// `tracing` の warn を捕まえるテスト用ライタ（`harness::reply` の `CapturedLogs` と
    /// 同型。dev-dependency を増やさず、テスト内で完結する最小の subscriber を組む）。
    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn text(&self) -> String {
            let buf = self.0.lock().expect("log buffer mutex poisoned");
            String::from_utf8(buf.clone()).expect("tracing fmt writes utf-8")
        }
    }

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer mutex poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for CapturedLogs {
        type Writer = Self;
        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    /// `fut` の実行中に出た WARN 以上のログを文字列で返す（`harness::reply` の
    /// `capture_warnings` の非同期版）。`tracing::subscriber::set_default` はスレッドローカル
    /// なので、`#[tokio::test]` の既定（current-thread）ランタイムで `.await` をまたいでも
    /// 同じスレッド上で実行される限り有効に保たれる。
    async fn capture_warnings<Fut, T>(fut: Fut) -> (T, String)
    where
        Fut: std::future::Future<Output = T>,
    {
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(logs.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let result = fut.await;
        (result, logs.text())
    }

    /// `capture_warnings` の同期版。`ProductAllowlist::from_models` のような同期関数（Stage1
    /// Suggestion 4 の trim 除外 warn）を検証するために使う。
    fn capture_warnings_sync<F, T>(f: F) -> (T, String)
    where
        F: FnOnce() -> T,
    {
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(logs.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let result = f();
        (result, logs.text())
    }

    #[tokio::test]
    async fn allowlist_uses_fresh_cache_without_warning() {
        let gate = ProductGate::with_ttl(unreachable_client(), Duration::from_secs(600));
        gate.store("urtect", Arc::new(fixture_allowlist()));

        let (result, logs) = capture_warnings(gate.allowlist("urtect")).await;
        let allowlist = result.expect("fresh cache hit must not error");
        assert!(allowlist.is_in_scope("ADC-V724"));
        assert!(
            logs.is_empty(),
            "a fresh cache hit must never attempt (and thus never warn about) a refetch: {logs}"
        );
    }

    #[tokio::test]
    async fn allowlist_serves_stale_cache_and_warns_when_refresh_fails() {
        // TTL=0 -> 直後にストアしたエントリも常に stale 扱いになる。
        let gate = ProductGate::with_ttl(unreachable_client(), Duration::from_secs(0));
        gate.store("urtect", Arc::new(fixture_allowlist()));

        let (result, logs) = capture_warnings(gate.allowlist("urtect")).await;
        let allowlist = result.expect("stale cache must still be served, not an error");
        assert!(allowlist.is_in_scope("ADC-V724"));
        assert!(
            logs.contains("WARN"),
            "stale fallback must be warned so operators can see the master is out of date: {logs}"
        );
        assert!(logs.contains("urtect"), "{logs}");
    }

    #[tokio::test]
    async fn allowlist_fails_when_first_fetch_fails_and_no_cache_exists() {
        let gate = ProductGate::with_ttl(unreachable_client(), Duration::from_secs(600));
        let result = gate.allowlist("urtect").await;
        assert!(
            result.is_err(),
            "no cache and a failing fetch must surface an error (caller maps this to 503)"
        );
    }

    // ---- validate_product_nodes（Critical 1: 空 / limit ちょうどの fail-closed） ----

    fn product_node(model: &str) -> crate::proto::graphrag::NodeResult {
        let mut attributes = HashMap::new();
        attributes.insert("model".to_string(), model.to_string());
        crate::proto::graphrag::NodeResult {
            node_id: format!("urtect:gen1:Product:{model}"),
            node_type: KIND_PRODUCT.to_string(),
            attributes,
        }
    }

    #[test]
    fn validate_product_nodes_rejects_empty_master_with_an_actionable_message() {
        let err = validate_product_nodes(&[], "urtect", 1000)
            .expect_err("an empty product master must fail closed, not silently succeed");
        let message = err.to_string();
        assert!(
            message.contains("urtect"),
            "operators need the schema name to know which project is affected: {message}"
        );
        assert!(
            message.contains("ingest_products"),
            "operators need the next action (run ingest_products) spelled out: {message}"
        );
    }

    #[test]
    fn validate_product_nodes_rejects_a_count_exactly_at_the_limit_as_suspected_truncation() {
        let nodes: Vec<_> = (0..3).map(|i| product_node(&format!("ADC-V{i}"))).collect();
        let err = validate_product_nodes(&nodes, "urtect", 3).expect_err(
            "a count exactly at the query limit must be treated as suspected \
                         silent truncation",
        );
        assert!(err.to_string().contains("urtect"));
    }

    #[test]
    fn validate_product_nodes_accepts_a_normal_count_below_the_limit() {
        let nodes = vec![product_node("ADC-V724"), product_node("ADC-V523")];
        validate_product_nodes(&nodes, "urtect", 1000)
            .expect("a normal, below-limit product count must pass validation");
    }

    // ---- validate_allowlist_not_empty（Issue #28 Stage1 Warning 2） ----

    #[test]
    fn validate_allowlist_not_empty_rejects_when_every_node_is_missing_the_model_attribute() {
        // `validate_product_nodes` はノード数しか見ないため 2 件あれば素通りするが、両方とも
        // `model` 属性が無く `from_nodes` が全除外するため、実際に使える型番は 0 件になる。
        let nodes = vec![
            crate::proto::graphrag::NodeResult {
                node_id: "urtect:gen1:Product:1".to_string(),
                node_type: KIND_PRODUCT.to_string(),
                attributes: HashMap::new(),
            },
            crate::proto::graphrag::NodeResult {
                node_id: "urtect:gen1:Product:2".to_string(),
                node_type: KIND_PRODUCT.to_string(),
                attributes: HashMap::new(),
            },
        ];
        validate_product_nodes(&nodes, "urtect", 1000)
            .expect("node-count validation alone must not catch this; it only checks count");
        let allowlist = ProductAllowlist::from_nodes(nodes);

        let err = validate_allowlist_not_empty(&allowlist, "urtect").expect_err(
            "an allowlist with zero usable models must fail closed, not serve an empty allowlist \
             (which would make the out-of-scope reply show a blank product list and reject \
             every in-scope question)",
        );
        let message = err.to_string();
        assert!(
            message.contains("urtect"),
            "operators need the schema name to know which project is affected: {message}"
        );
        assert!(
            message.contains("model"),
            "operators need to know the 'model' attribute is the cause: {message}"
        );
        assert!(
            message.contains("ingest_products"),
            "operators need the next action spelled out: {message}"
        );
    }

    #[test]
    fn validate_allowlist_not_empty_accepts_when_usable_models_exist() {
        let allowlist = ProductAllowlist::from_nodes(vec![product_node("ADC-V724")]);
        validate_allowlist_not_empty(&allowlist, "urtect")
            .expect("an allowlist with at least one usable model must pass validation");
    }
}
