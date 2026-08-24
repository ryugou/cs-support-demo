//! LLM Call #2 の材料(advisor_material 検索 + known_resolution 照合)。
//!
//! design doc `2026-08-17-homesec-advisor-design.md` §5.1(`advisor_material` スキーマ)・
//! §6 手順6(累積条件 + 相談要旨での検索、KR 照合)・§4.4(KR signal 表現の正本)・
//! §8(vegapunk 検索失敗時の挙動)の実装。
//!
//! パターンは `harness::product_gate::ProductAllowlist::from_nodes` の「欠落ノードは warn して
//! 除外し、パイプライン全体は止めない」寛容スキップ規律に倣う。material 1 件の欠陥で検索全体を
//! `Err` にすると、seed データの 1 件のタイプミスが会話全体を fallback へ落とすことになり、
//! 被害の範囲が原因に対して不釣り合いに大きい。

use crate::advisor::understand::ConditionKey;
use crate::harness::rules::{match_known_resolution, KnownResolution, KrMatch};
use crate::harness::signal::{Signal, SignalSet};
use crate::manual::retrieval::kind_marker;
use crate::proto::graphrag::SearchResultItem;
use crate::vegapunk::VegapunkClient;
use std::collections::HashMap;

/// vegapunk 検索の top_k(design doc §6 手順6)。
const MATERIAL_SEARCH_TOP_K: i32 = 5;

/// `advisor_material` の `query_nodes` 取得上限。design doc §5.2 の現行データ規模(統計20-30 +
/// 自社7 + 他社12以上 + シナリオ5 ≒ 40-55件)に十分な余裕を持たせた固定値。件数がこれに近づいた
/// らページング(`query_nodes_paged`)へ切り替える必要があるが、それは将来の最適化でありこの
/// タスクの範囲外。
const MATERIAL_QUERY_NODES_LIMIT: i32 = 200;

/// LLM Call #2 に渡す 1 件の材料。design doc §5.1 の `advisor_material` 属性そのものに加え、
/// `kind == "known_resolution"` は ingest 由来ではなく KR 照合ヒット時に
/// [`known_resolution_to_material`] が合成する(design doc §4.4)。
#[derive(Debug, Clone, PartialEq)]
pub struct AdvisorMaterial {
    pub material_key: String,
    pub kind: String,
    pub title_ja: String,
    pub body_ja: String,
    pub source_url: Option<String>,
    pub category: Option<String>,
    pub product_key: Option<String>,
    pub price_band: Option<String>,
    pub card_description: Option<String>,
    pub card_match_terms: Option<String>,
    /// カードの「商品ページを見る」ボタンの遷移先(design doc §5.1、Issue #34 カルーセル→Flex
    /// 移行)。`own_product` / `partner_product` のみ意味を持つ。無ければボタンを出さない
    /// (`line_adapter.rs::build_flex_message` 側の判定)。
    pub product_page_url: Option<String>,
}

/// 属性値を trim し、空文字列なら `None` として返す。`ingest_homesec.rs` は未設定の optional
/// 属性を欠落キーではなく空文字列として書き込むため、欠落キーと空文字列値の両方をここで
/// 同じ扱い(`None`)にする。
fn trimmed_non_empty(attrs: &HashMap<String, String>, key: &str) -> Option<String> {
    attrs
        .get(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// design doc §5.1 の `kind` 語彙(4値)。この配列がそのまま vocabulary の正本。
const VALID_MATERIAL_KINDS: [&str; 4] = ["statistic", "own_product", "partner_product", "scenario"];

/// design doc §5.1「`source_url` は `statistic` と `partner_product` は required」の判定。
fn kind_requires_source_url(kind: &str) -> bool {
    matches!(kind, "statistic" | "partner_product")
}

/// レビュー3巡目 Warning 是正: 任意の値が `url::Url` としてパース可能な絶対 https URL かを
/// 検証する。parse 成功・scheme が https・userinfo 不在・host 存在を**すべて**満たす場合のみ
/// true。`source_url`(§5.1「公式サイト」)と `product_page_url`(§5.1 カードの「商品ページを
/// 見る」ボタン遷移先)の両方に使う汎用検証(Issue #47 Critical指摘2: 両フィールドとも
/// カード/応答本文を通じて顧客のブラウザへ渡る遷移先である以上、同じ強度の整形検証を課す)。
///
/// userinfo チェックは `username()`/`password()` だけでは不十分: `https://@host` のように
/// `@` はあるが中身が空の userinfo は、url crate が正規化時に userinfo を完全に消し去って
/// しまい(実測: パース後は `https://host/` と区別が付かず、`username()`/`password()` は
/// どちらも「userinfo 無し」を返す)、アクセサだけで見ると素通ししてしまう。生文字列の
/// authority 部分(スキーム "://" の直後から最初の `/`・`?`・`#` まで)に生の `@` が
/// 含まれるかを別途見ることで、意味上は無害でも「userinfo らしき記法」自体を一律拒否する
/// (拒否範囲を広げるだけなので、正規の https URL を誤って弾くことはない)。
pub fn is_well_formed_https_url(raw: &str) -> bool {
    if authority_contains_raw_userinfo_marker(raw) {
        return false;
    }
    match url::Url::parse(raw) {
        Ok(parsed) => {
            parsed.scheme() == "https"
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.host_str().is_some_and(|host| !host.is_empty())
        }
        Err(_) => false,
    }
}

/// scheme "://" の直後から authority の終端(最初の `/`・`?`・`#`、無ければ文字列末尾)までに
/// 生の `@` が含まれるかを見る。[`is_well_formed_https_url`] のコメント参照。
fn authority_contains_raw_userinfo_marker(raw: &str) -> bool {
    let Some((_, after_scheme)) = raw.split_once("://") else {
        return false;
    };
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    after_scheme[..authority_end].contains('@')
}

impl AdvisorMaterial {
    /// `query_nodes` が返す `advisor_material` ノードの属性 map から組み立てる。
    ///
    /// `material_key` / `kind` / `title_ja` / `body_ja` の 4 つは trim 後非空が必須。1つでも
    /// 欠落・空なら `tracing::warn!` して `None` を返す(呼び出し側は件数が 1 件減るだけで、
    /// 検索パイプライン全体は止めない)。他 6 フィールドは trim 後空文字列(または欠落キー)なら
    /// `None`、非空なら `Some(値)` にする(design doc §5.1 の optional 属性)。
    ///
    /// Warning D 是正: 上記の非空チェックだけでは `kind` が design doc §5.1 の4値
    /// (`statistic` / `own_product` / `partner_product` / `scenario`)以外でも通過し、
    /// `statistic` / `partner_product` が `source_url` 無しで通過していた(接地材料として
    /// LLM に渡る事実主張の出典が曖昧になる)。この2点も同じ寛容スキップ方式(warn + `None`)
    /// で追加検証する。
    pub fn from_attributes(attrs: &HashMap<String, String>) -> Option<Self> {
        let material_key = trimmed_non_empty(attrs, "material_key");
        let kind = trimmed_non_empty(attrs, "kind");
        let title_ja = trimmed_non_empty(attrs, "title_ja");
        let body_ja = trimmed_non_empty(attrs, "body_ja");

        if material_key.is_none() || kind.is_none() || title_ja.is_none() || body_ja.is_none() {
            let missing: Vec<&str> = [
                ("material_key", material_key.is_none()),
                ("kind", kind.is_none()),
                ("title_ja", title_ja.is_none()),
                ("body_ja", body_ja.is_none()),
            ]
            .into_iter()
            .filter_map(|(name, is_missing)| is_missing.then_some(name))
            .collect();
            tracing::warn!(
                material_key = material_key.as_deref().unwrap_or("<missing>"),
                missing_fields = ?missing,
                "advisor_material node is missing a required attribute (material_key/kind/\
                 title_ja/body_ja); skipping this material and continuing with the rest of the \
                 search result set"
            );
            return None;
        }

        let material_key = material_key.expect("checked above");
        let kind = kind.expect("checked above");
        let title_ja = title_ja.expect("checked above");
        let body_ja = body_ja.expect("checked above");

        if !VALID_MATERIAL_KINDS.contains(&kind.as_str()) {
            tracing::warn!(
                material_key = %material_key,
                kind = %kind,
                valid_kinds = ?VALID_MATERIAL_KINDS,
                "advisor_material node has a kind outside the design doc §5.1 vocabulary; \
                 skipping this material. Fix the source row in server/data/homesec/materials.json \
                 (or its ingest source) so kind matches the vocabulary"
            );
            return None;
        }

        let source_url = trimmed_non_empty(attrs, "source_url");
        if kind_requires_source_url(&kind) && source_url.is_none() {
            tracing::warn!(
                material_key = %material_key,
                kind = %kind,
                "advisor_material node is missing source_url, which design doc §5.1 requires \
                 for kind=statistic/partner_product (without it this material would be handed \
                 to the LLM as a factual grounding source with no traceable origin); skipping \
                 this material. Fix server/data/homesec/materials.json"
            );
            return None;
        }
        // レビュー2巡目 Warning A 是正: ここまでは source_url の非空しか見ていなかったため、
        // `javascript:alert(1)` のような値も material として通過し、そのまま
        // `draftgen.rs` の `allowed_urls`(URL allowlist gate の許可集合)へ混入しうる状態
        // だった。出口関門は「材料データは安全である」ことを暗黙に前提しており、その前提を
        // ここで担保する。design doc §5.1 の `source_url` は「公式サイト」なので https 限定
        // (http・スキーム無しの相対パス・javascript: 等は全て拒否)で問題ない。
        //
        // レビュー3巡目 Warning 是正: `starts_with("https://")` は文字列の接頭辞しか見ない
        // ため、"https://"(ホスト無し)や "https://example.com@evil.example/path"
        // (userinfo による接続先偽装)のような値も通過していた。これがそのまま
        // `draftgen.rs` の URL allowlist gate の許可集合(`allowed_urls`)に入ると、
        // allowlist 自体に偽装ホストを許可する値が混入することになる。`url::Url` でパースし、
        // scheme・userinfo 不在・host 存在を検証する([`is_well_formed_https_url`] 参照)。
        if let Some(url) = &source_url {
            if !is_well_formed_https_url(url) {
                tracing::warn!(
                    material_key = %material_key,
                    kind = %kind,
                    source_url = %url,
                    "advisor_material node has a source_url that is not a well-formed absolute \
                     https:// URL (parse failure, non-https scheme, embedded/empty userinfo, or \
                     missing host); skipping this material rather than letting an unvetted or \
                     host-ambiguous value reach the URL allowlist gate. Fix \
                     server/data/homesec/materials.json"
                );
                return None;
            }
        }

        // Issue #47 Critical指摘2: `product_page_url` はカードの「商品ページを見る」ボタンの
        // 遷移先(`cards.rs::build_card_buttons`)としてそのまま顧客のブラウザへ渡る。
        // `source_url` と同じ入口を通らないため出口関門(URL allowlist gate)の対象外であり、
        // ここで同等の整形検証を課さないと未検証の値がボタンの URL にまで届く。
        //
        // 4次 codex レビュー 指摘3 是正: `source_url` の検証失敗は材料全体(本文・タイトル等)を
        // 落とす必要がある(design doc 上 `source_url` は根拠の出典として required なフィールド
        // であり、欠陥のある材料を接地材料として使わせないため)。一方 `product_page_url` は
        // カードの URI ボタンという1フィールドの装飾に過ぎない optional 属性であり、他の
        // フィールドに欠陥は無い。検証失敗時に材料全体を `None` で握りつぶすと、本文・
        // card_description まで失われ、実害に対して被害が不釣り合いに大きい。ここでは
        // `product_page_url` だけを `None` に落とし(ボタンが省略されるだけ)、材料自体は
        // 残す非対称な扱いにする。
        let product_page_url = trimmed_non_empty(attrs, "product_page_url").and_then(|url| {
            if is_well_formed_https_url(&url) {
                Some(url)
            } else {
                tracing::warn!(
                    material_key = %material_key,
                    kind = %kind,
                    product_page_url = %url,
                    "advisor_material node has a product_page_url that is not a well-formed absolute \
                     https:// URL (parse failure, non-https scheme, embedded/empty userinfo, or \
                     missing host); dropping only the product_page_url (the card's URI button will be \
                     omitted) rather than discarding the whole material. Fix \
                     server/data/homesec/materials.json"
                );
                None
            }
        });

        Some(Self {
            material_key,
            kind,
            title_ja,
            body_ja,
            source_url,
            category: trimmed_non_empty(attrs, "category"),
            product_key: trimmed_non_empty(attrs, "product_key"),
            price_band: trimmed_non_empty(attrs, "price_band"),
            card_description: trimmed_non_empty(attrs, "card_description"),
            card_match_terms: trimmed_non_empty(attrs, "card_match_terms"),
            product_page_url,
        })
    }
}

/// design doc §6 手順6「累積条件 + 相談要旨で検索」の検索文組み立て。`query` の後ろへ、各条件の
/// value(`ConditionKey` 自体ではなく正規化済みの値の文字列)を半角スペース区切りで連結する。
fn build_material_search_text(query: &str, conditions: &[(ConditionKey, String)]) -> String {
    if conditions.is_empty() {
        return query.to_string();
    }
    let mut parts: Vec<&str> = Vec::with_capacity(1 + conditions.len());
    parts.push(query);
    for (_, value) in conditions {
        parts.push(value.as_str());
    }
    parts.join(" ")
}

/// design doc §6 手順6「category 合致材料」の対象 kind(own_product を除く3種)。
const CATEGORY_POOL_KINDS: [&str; 3] = ["statistic", "partner_product", "scenario"];

/// `vp.search()` のヒットから `advisor_material` 以外(`ConversationTurn` / `support_case` 等)を
/// 除外し、`query_nodes` の属性で [`AdvisorMaterial`] へ変換する純関数。ネットワーク呼び出しを
/// 含まないため、`SearchResultItem` / 属性 map を手組みしたフィクスチャで検証できる。
///
/// ヒット順序を保つ(検索の順位はそのまま材料の優先順位になる。design doc §6 手順6・§7.2
/// カード決定の「検索ヒット順」の前提)。
fn select_materials_from_hits(
    hits: &[SearchResultItem],
    node_attrs: &HashMap<String, HashMap<String, String>>,
) -> Vec<AdvisorMaterial> {
    let marker = kind_marker("advisor_material");
    hits.iter()
        .filter_map(|hit| {
            let id = hit.id.as_deref()?;
            if !id.contains(&marker) {
                return None;
            }
            match node_attrs.get(id) {
                Some(attrs) => AdvisorMaterial::from_attributes(attrs),
                None => {
                    tracing::warn!(
                        node_id = id,
                        "a search hit id carries the advisor_material marker but was not found \
                         in the query_nodes attribute map (possible race between search index \
                         and node store, or a node deleted between the two calls); skipping"
                    );
                    None
                }
            }
        })
        .collect()
}

/// design doc §6 手順6 の材料検索本体。`vp.search` と `vp.query_nodes` を両方 await し、両方の
/// 結果を見る。実際の合成ロジックは [`materials_from_results`](純関数)に委譲する
/// (Warning B 是正: 「失敗しても `Err` を伝播しない」契約はネットワーク呼び出しと同じ関数に
/// インラインで書かれていたため、この契約自体を固定するテストが書けなかった)。
///
/// 戻り値は `(searched, own_products, category_pool)` の3要素タプル。`searched` は従来どおり
/// 検索ヒット由来の材料、`own_products` は schema 内の全 `own_product` 材料(design doc §6
/// 手順6「own_product の保証注入」用)、`category_pool` は `own_product` を除く3種
/// (`statistic` / `partner_product` / `scenario`)の全材料(design doc §6 手順6「category
/// 合致材料」の入力集合)。いずれも `query_nodes` で既に取得済みの全 advisor_material から
/// 抽出するため、追加のネットワーク呼び出しは発生しない。
pub async fn gather_materials(
    vp: &VegapunkClient,
    schema: &str,
    query: &str,
    conditions: &[(ConditionKey, String)],
) -> (
    Vec<AdvisorMaterial>,
    Vec<AdvisorMaterial>,
    Vec<AdvisorMaterial>,
) {
    let search_text = build_material_search_text(query, conditions);
    let (search_result, nodes_result) = tokio::join!(
        vp.search(schema, &search_text, MATERIAL_SEARCH_TOP_K),
        vp.query_nodes(
            schema,
            "advisor_material",
            Vec::new(),
            MATERIAL_QUERY_NODES_LIMIT
        ),
    );
    materials_from_results(search_result, nodes_result, schema)
}

/// [`gather_materials`] から `tokio::join!` の結果を受け取ってから先の合成ロジックを切り出した
/// 純関数。ネットワーク呼び出しを含まないため、`Ok` / `Err` を手組みしたフィクスチャで
/// 検証できる。
///
/// **この関数は絶対に `Err` を伝播しない**(design doc §8: 「vegapunk 検索失敗: warn ログ。
/// 材料ゼロで Call#2 を実行(接地規則により事実主張なしの一般助言になる)。応答は止めない」)。
/// どちらか一方でも失敗したら `tracing::warn!` して両方とも空 `Vec` を返す。呼び出し元
/// (`draftgen.rs`)は材料が空でも安全に動く前提で設計してある。
///
/// 戻り値の2・3つ目の要素は design doc §6 手順6 の別枠保証注入用に、`nodes_result` から
/// kind ごとに [`select_materials_of_kind`] / [`select_materials_of_kinds`] で抽出する:
/// `own_products` は `kind == "own_product"`(「own_product の保証注入」用)、
/// `category_pool` は `kind` が `statistic` / `partner_product` / `scenario` のいずれか
/// (own_product を除く。「category 合致材料」の入力集合)。
fn materials_from_results(
    search_result: anyhow::Result<Vec<SearchResultItem>>,
    nodes_result: anyhow::Result<Vec<crate::proto::graphrag::NodeResult>>,
    schema: &str,
) -> (
    Vec<AdvisorMaterial>,
    Vec<AdvisorMaterial>,
    Vec<AdvisorMaterial>,
) {
    let hits = match search_result {
        Ok(hits) => hits,
        Err(error) => {
            tracing::warn!(
                route = "advisor_gather_materials",
                schema,
                error = %error,
                "vegapunk search for advisor materials failed; returning zero materials so \
                 Call#2 still runs (it degrades to general advice without factual grounding, \
                 design doc §8). Response is not blocked"
            );
            return (Vec::new(), Vec::new(), Vec::new());
        }
    };
    let nodes = match nodes_result {
        Ok(nodes) => nodes,
        Err(error) => {
            tracing::warn!(
                route = "advisor_gather_materials",
                schema,
                error = %error,
                "vegapunk query_nodes for advisor_material attributes failed; returning zero \
                 materials so Call#2 still runs (it degrades to general advice without factual \
                 grounding, design doc §8). Response is not blocked"
            );
            return (Vec::new(), Vec::new(), Vec::new());
        }
    };

    let node_attrs: HashMap<String, HashMap<String, String>> = nodes
        .into_iter()
        .map(|n| (n.node_id, n.attributes))
        .collect();

    let searched = select_materials_from_hits(&hits, &node_attrs);
    let own_products = select_materials_of_kind(&node_attrs, "own_product");
    let category_pool = select_materials_of_kinds(&node_attrs, &CATEGORY_POOL_KINDS);
    (searched, own_products, category_pool)
}

/// `node_attrs`(query_nodes が返した全 advisor_material の属性 map)から `kind` が一致する
/// 材料だけを [`AdvisorMaterial`] へ変換する。[`select_materials_of_kinds`] の単一 kind 版。
fn select_materials_of_kind(
    node_attrs: &HashMap<String, HashMap<String, String>>,
    kind: &str,
) -> Vec<AdvisorMaterial> {
    select_materials_of_kinds(node_attrs, &[kind])
}

/// `node_attrs`(query_nodes が返した全 advisor_material の属性 map)から `kind` が `kinds` の
/// いずれかに一致する材料だけを [`AdvisorMaterial`] へ変換する。material_key の昇順で
/// ソートし、`HashMap` の反復順序に依存しない決定論的な順序にする(design doc §6 手順6)。
fn select_materials_of_kinds(
    node_attrs: &HashMap<String, HashMap<String, String>>,
    kinds: &[&str],
) -> Vec<AdvisorMaterial> {
    let mut materials: Vec<AdvisorMaterial> = node_attrs
        .values()
        .filter_map(AdvisorMaterial::from_attributes)
        .filter(|m| kinds.contains(&m.kind.as_str()))
        .collect();
    materials.sort_by(|a, b| a.material_key.cmp(&b.material_key));
    materials
}

/// advisor の known_resolution signal 表現の**正本**。各累積条件を `"{key}:{value}"`
/// (例 `concern:intrusion`)へ変換した [`SignalSet`] を返す。
///
/// 将来 admin から homesec 向け known_resolution を登録する側(このタスクのスコープ外)も、
/// ここで定義した `"{key}:{value}"` 形式で `signal_set` を組む契約になる。この関数自体は
/// その契約を定義するだけで、admin 側の実装は行わない。
pub fn conditions_to_signal_set(conditions: &[(ConditionKey, String)]) -> SignalSet {
    conditions
        .iter()
        .map(|(key, value)| Signal::new(format!("{}:{}", key.as_str(), value)))
        .collect()
}

/// design doc §4.4・§6 手順6 の KR 照合。[`conditions_to_signal_set`] で組んだ SignalSet を
/// `harness::rules::match_known_resolution` に通し、`KrMatch::Applicable` のときだけ材料化候補を
/// 返す。
///
/// `KrMatch::BlockedByAddedSignal`(条件のサブセットは一致したが未知の追加条件が残っている状態。
/// CS の「学習の入口」フローに対応する)は homesec のスコープ外なので `None` と同様に扱う
/// (advisor はこの状態から新規 KR を提案する導線を持たない)。
pub fn match_advisor_known_resolution<'a>(
    resolutions: &'a [KnownResolution],
    conditions: &[(ConditionKey, String)],
) -> Option<&'a KnownResolution> {
    let signal_set = conditions_to_signal_set(conditions);
    match match_known_resolution(resolutions, &signal_set) {
        KrMatch::Applicable(kr) => Some(kr),
        KrMatch::BlockedByAddedSignal { .. } | KrMatch::None => None,
    }
}

/// KR がヒットしたときに材料リストの先頭へ注入する合成材料(design doc §4.4)。
///
/// `card_description` は必ず `None` にする — KR 由来の材料はカード化対象にしない設計であり、
/// `cards::select_cards` は `card_description.is_some()` の材料だけを候補にするため、ここで
/// `None` にしておけば `cards.rs` 側で KR を特別扱いするコードが不要になる(除外は構造で保証し、
/// 分岐で保証しない)。
pub fn known_resolution_to_material(kr: &KnownResolution) -> AdvisorMaterial {
    AdvisorMaterial {
        material_key: format!("known_resolution:{}", kr.id),
        kind: "known_resolution".to_string(),
        title_ja: "過去の相談から".to_string(),
        body_ja: kr.answer.clone(),
        source_url: None,
        category: None,
        product_key: None,
        price_band: None,
        card_description: None,
        card_match_terms: None,
        product_page_url: None,
    }
}

/// design doc §6 手順6・§4.4 の KR 照合結果を材料リストへ合成する純関数(レビュー2巡目
/// Warning B 是正: `match_advisor_known_resolution` と `known_resolution_to_material` は
/// 存在していたが、両者を「材料リストの先頭へ注入する」合成自体がどこにも実装されておらず、
/// `known_resolution_to_material` の doc コメントだけがその契約を主張している状態だった)。
///
/// `kr` が `Some` なら [`known_resolution_to_material`] の結果を先頭に置き、`searched`
/// (design doc §6 手順6 の vegapunk 検索結果、検索ヒット順)をその後ろへ順序そのまま並べる。
/// `kr` が `None` なら `searched` をそのまま返す。
///
/// 呼び出し元(Task 6 のハンドラ)はまだ存在しない。この関数はその合成ロジックを単体テスト
/// 可能な純関数として先に用意するところまでがこのタスクの範囲。
pub fn compose_materials(
    kr: Option<&KnownResolution>,
    searched: Vec<AdvisorMaterial>,
) -> Vec<AdvisorMaterial> {
    match kr {
        Some(kr) => {
            let mut composed = Vec::with_capacity(1 + searched.len());
            composed.push(known_resolution_to_material(kr));
            composed.extend(searched);
            composed
        }
        None => searched,
    }
}

/// own_product 保証注入(design doc §6 手順6)のトリガー判定: 理解結果が製品・機器の
/// 導入意図を含む、または累積条件に `concern` があるとき true。
///
/// 検索が own_product を引けず接地規則が製品提案を封じる本番実害(背景 (b))への決定論対処。
/// 検索順位に依存せず、この2条件のどちらかを満たせば own_product 材料を別枠で必ず注入する。
pub fn should_guarantee_own_products(
    product_intent: bool,
    conditions: &[(ConditionKey, String)],
) -> bool {
    product_intent || conditions.iter().any(|(k, _)| *k == ConditionKey::Concern)
}

/// `concern_category` に合致する own_product を優先し、合致が無ければ全件を返す
/// (design doc §6 手順6)。
pub fn select_own_product_materials(
    own_products: &[AdvisorMaterial],
    concern_category: Option<&str>,
) -> Vec<AdvisorMaterial> {
    if let Some(cat) = concern_category {
        let matched: Vec<AdvisorMaterial> = own_products
            .iter()
            .filter(|m| m.category.as_deref() == Some(cat))
            .cloned()
            .collect();
        if !matched.is_empty() {
            return matched;
        }
    }
    own_products.to_vec()
}

/// 累積条件から own_product 保証注入の category 優先キー(design doc §6 手順6)を取り出す。
///
/// `decide::merge_conditions` は同一キーを上書きする実装のため、`conditions` に
/// `ConditionKey::Concern` は最大1件しか含まれない。この前提が崩れると `.find()` が
/// 先頭要素だけを見る現在の意味(=先頭がそのまま「唯一の」concern である)が崩れるため、
/// ここに明記しておく。
pub fn concern_category(conditions: &[(ConditionKey, String)]) -> Option<&str> {
    conditions
        .iter()
        .find(|(k, _)| *k == ConditionKey::Concern)
        .map(|(_, v)| v.as_str())
}

/// `guaranteed` のうち `composed` に既に存在する material_key と重複するものを除いて
/// 末尾へ追加する(design doc §6 手順6)。`composed` 側との重複だけでなく、`guaranteed`
/// 自身の内部で material_key が重複している場合(`guaranteed` は `node_id` をキーにした
/// `HashMap` 由来のため、別 `node_id` が同じ `material_key` を持つと起こりうる)も1件しか
/// 追加しない。
///
/// codex レビュー指摘是正: 以前は `existing_keys` を `composed` の初期状態から一度だけ
/// 作り、push 後に更新していなかったため、`guaranteed` 内部の重複は素通りして両方とも
/// 追加されていた。`HashSet::insert` の戻り値(新規追加なら `true`)で判定することで、
/// 追加したキーがその場で `existing_keys` へ反映され、`guaranteed` 内部の重複も除去する。
pub fn inject_guaranteed_own_products(
    mut composed: Vec<AdvisorMaterial>,
    guaranteed: Vec<AdvisorMaterial>,
) -> Vec<AdvisorMaterial> {
    let mut existing_keys: std::collections::HashSet<String> =
        composed.iter().map(|m| m.material_key.clone()).collect();
    for m in guaranteed {
        if existing_keys.insert(m.material_key.clone()) {
            composed.push(m);
        }
    }
    composed
}

/// 材料合成リストの総数上限(design doc §6 手順6「検索ヒットと保証注入を合わせた総数は
/// 最大12件」)。own_product 保証注入([`inject_guaranteed_own_products`])はこの上限の
/// 対象外(既存挙動を変更しない指示のため)で、category 合致材料の注入([`inject_category_materials`])
/// にだけ適用する。
const MAX_TOTAL_MATERIALS: usize = 12;

/// `category_pool` の各上限(design doc §6 手順6「category 合致材料」)。kind ごとに
/// material_key 昇順で先頭からこの件数だけ選び、この順(statistic → partner_product →
/// scenario)で連結する。
const CATEGORY_MATERIAL_LIMITS: [(&str, usize); 3] =
    [("statistic", 2), ("partner_product", 2), ("scenario", 1)];

/// `category_pool`(design doc §6 手順6「category 合致材料」の入力集合。own_product を除く
/// 3種)から、累積条件の `concern` と同じ `category` を持つ材料を kind ごとの上限
/// ([`CATEGORY_MATERIAL_LIMITS`])まで抽出する。`concern_category` が `None`(累積条件に
/// `concern` が無い)なら、合致対象自体が無いので空 `Vec` を返す。
///
/// own_product 保証注入([`select_own_product_materials`])と異なり、合致が無いときの
/// 「全件返す」フォールバックは無い(design doc §6 手順6の文言どおり、category 合致材料は
/// 合致するものだけを注入する別枠)。
pub fn select_category_materials(
    category_pool: &[AdvisorMaterial],
    concern_category: Option<&str>,
) -> Vec<AdvisorMaterial> {
    let Some(cat) = concern_category else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for (kind, limit) in CATEGORY_MATERIAL_LIMITS {
        let mut matched: Vec<AdvisorMaterial> = category_pool
            .iter()
            .filter(|m| m.kind == kind && m.category.as_deref() == Some(cat))
            .cloned()
            .collect();
        matched.sort_by(|a, b| a.material_key.cmp(&b.material_key));
        matched.truncate(limit);
        result.extend(matched);
    }
    result
}

/// `candidates` のうち `composed` に既に存在する material_key と重複するものを除いて末尾へ
/// 追加する(design doc §6 手順6「category 合致材料」)。`composed` 側との重複だけでなく、
/// `candidates` 自身の内部で material_key が重複している場合も1件しか追加しない(実装は
/// [`inject_guaranteed_own_products`] と同じ `HashSet::insert` 戻り値判定パターン)。
///
/// 合計 [`MAX_TOTAL_MATERIALS`] 件に達した時点で、以降の `candidates` は重複判定より先に
/// 打ち切る(design doc §6 手順6「検索ヒットと保証注入を合わせた総数は最大12件」)。
pub fn inject_category_materials(
    mut composed: Vec<AdvisorMaterial>,
    candidates: Vec<AdvisorMaterial>,
) -> Vec<AdvisorMaterial> {
    let mut existing_keys: std::collections::HashSet<String> =
        composed.iter().map(|m| m.material_key.clone()).collect();
    for m in candidates {
        if composed.len() >= MAX_TOTAL_MATERIALS {
            break;
        }
        if existing_keys.insert(m.material_key.clone()) {
            composed.push(m);
        }
    }
    composed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::rules::{Binding, Grade, RootCause, SourceAuthority};
    use crate::test_support::capture_logs;

    fn attrs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn full_attrs() -> HashMap<String, String> {
        attrs(&[
            ("material_key", "own_product:adc-v724"),
            ("kind", "own_product"),
            ("title_ja", "URTECT ADC-V724"),
            ("body_ja", "屋外対応の防犯カメラです。"),
            ("source_url", ""),
            ("category", "monitoring"),
            ("product_key", "ADC-V724"),
            ("price_band", ""),
            ("card_description", "屋外対応・夜間撮影。スマホから映像確認"),
            ("card_match_terms", "ADC-V724,屋外,カメラ"),
            ("product_page_url", "https://example.com/products/adc-v724"),
        ])
    }

    // --- AdvisorMaterial::from_attributes ---

    #[test]
    fn from_attributes_parses_a_well_formed_node() {
        let material =
            AdvisorMaterial::from_attributes(&full_attrs()).expect("well-formed attrs must parse");
        assert_eq!(material.material_key, "own_product:adc-v724");
        assert_eq!(material.kind, "own_product");
        assert_eq!(material.title_ja, "URTECT ADC-V724");
        assert_eq!(material.body_ja, "屋外対応の防犯カメラです。");
        assert_eq!(material.category.as_deref(), Some("monitoring"));
        assert_eq!(material.product_key.as_deref(), Some("ADC-V724"));
        assert_eq!(
            material.card_match_terms.as_deref(),
            Some("ADC-V724,屋外,カメラ")
        );
    }

    #[test]
    fn from_attributes_treats_blank_optional_attribute_as_none() {
        let material = AdvisorMaterial::from_attributes(&full_attrs()).expect("must parse");
        // full_attrs() で source_url / price_band を空文字列にしている(ingest 側の未設定表現)。
        assert_eq!(material.source_url, None);
        assert_eq!(material.price_band, None);
    }

    #[test]
    fn from_attributes_treats_missing_optional_key_as_none() {
        let mut a = full_attrs();
        a.remove("card_description");
        let material = AdvisorMaterial::from_attributes(&a).expect("must parse");
        assert_eq!(material.card_description, None);
    }

    // --- Issue #34: product_page_url(design doc §5.1、カルーセル→Flex 移行) ---

    #[test]
    fn from_attributes_parses_product_page_url_when_present() {
        let material = AdvisorMaterial::from_attributes(&full_attrs()).expect("must parse");
        assert_eq!(
            material.product_page_url.as_deref(),
            Some("https://example.com/products/adc-v724")
        );
    }

    #[test]
    fn from_attributes_treats_missing_product_page_url_as_none() {
        let mut a = full_attrs();
        a.remove("product_page_url");
        let material = AdvisorMaterial::from_attributes(&a).expect("must parse");
        assert_eq!(material.product_page_url, None);
    }

    #[test]
    fn from_attributes_treats_blank_product_page_url_as_none() {
        let mut a = full_attrs();
        a.insert("product_page_url".to_string(), "   ".to_string());
        let material = AdvisorMaterial::from_attributes(&a).expect("must parse");
        assert_eq!(material.product_page_url, None);
    }

    // --- Issue #47 Critical指摘2: product_page_url に source_url と同等の https 整形検証 ---

    #[test]
    fn from_attributes_drops_a_plain_http_product_page_url_but_keeps_the_material_and_warns() {
        // 4次 codex レビュー 指摘3 是正: product_page_url はカードの URI ボタン用の1フィールド
        // に過ぎず、検証失敗を理由に材料全体(本文・タイトル等)まで捨てるのは過剰。ここでは
        // product_page_url だけが None になり、他フィールドは full_attrs() 由来のまま残ること
        // を固定する。
        let mut a = full_attrs();
        a.insert(
            "product_page_url".to_string(),
            "http://example.com/products/adc-v724".to_string(),
        );
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        let material = result.expect(
            "product_page_url must be https, but an invalid product_page_url alone must not \
             discard the whole material",
        );
        assert_eq!(material.product_page_url, None);
        assert_eq!(material.material_key, "own_product:adc-v724");
        assert_eq!(material.title_ja, "URTECT ADC-V724");
        assert_eq!(material.body_ja, "屋外対応の防犯カメラです。");
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn from_attributes_drops_a_hostless_https_product_page_url_but_keeps_the_material_and_warns() {
        let mut a = full_attrs();
        a.insert("product_page_url".to_string(), "https://".to_string());
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        let material = result.expect(
            "a bare https:// with no host must be dropped from product_page_url before it \
             reaches the card's URI button, but must not discard the whole material",
        );
        assert_eq!(material.product_page_url, None);
        assert_eq!(material.material_key, "own_product:adc-v724");
        assert_eq!(material.title_ja, "URTECT ADC-V724");
        assert_eq!(material.body_ja, "屋外対応の防犯カメラです。");
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn from_attributes_drops_a_product_page_url_with_userinfo_impersonating_a_host_but_keeps_the_material_and_warns(
    ) {
        let mut a = full_attrs();
        a.insert(
            "product_page_url".to_string(),
            "https://example.com@evil.example/path".to_string(),
        );
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        let material = result.expect(
            "a product_page_url embedding a real-looking host as userinfo must be dropped, but \
             must not discard the whole material",
        );
        assert_eq!(material.product_page_url, None);
        assert_eq!(material.material_key, "own_product:adc-v724");
        assert_eq!(material.title_ja, "URTECT ADC-V724");
        assert_eq!(material.body_ja, "屋外対応の防犯カメラです。");
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn from_attributes_rejects_missing_material_key_and_warns() {
        let mut a = full_attrs();
        a.remove("material_key");
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        assert_eq!(result, None);
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn from_attributes_rejects_blank_kind_and_warns() {
        let mut a = full_attrs();
        a.insert("kind".to_string(), "   ".to_string());
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        assert_eq!(result, None);
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn from_attributes_rejects_missing_title_ja_and_warns() {
        let mut a = full_attrs();
        a.remove("title_ja");
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        assert_eq!(result, None);
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn from_attributes_rejects_missing_body_ja_and_warns() {
        let mut a = full_attrs();
        a.remove("body_ja");
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        assert_eq!(result, None);
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    // --- レビュー2巡目 Warning A: source_url のスキーム検証 ---

    #[test]
    fn from_attributes_rejects_a_javascript_scheme_source_url_and_warns() {
        let mut a = full_attrs();
        a.insert("source_url".to_string(), "javascript:alert(1)".to_string());
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        assert_eq!(
            result, None,
            "a javascript: source_url must be rejected before it can reach the URL allowlist"
        );
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn from_attributes_rejects_a_plain_http_source_url_and_warns() {
        let mut a = full_attrs();
        a.insert("source_url".to_string(), "http://example.com/a".to_string());
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        assert_eq!(
            result, None,
            "source_url must be https (design doc §5.1: source_url is the official site)"
        );
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn from_attributes_rejects_a_relative_path_source_url_and_warns() {
        let mut a = full_attrs();
        a.insert("source_url".to_string(), "/relative/path".to_string());
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        assert_eq!(result, None);
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn from_attributes_accepts_a_well_formed_https_source_url() {
        let mut a = full_attrs();
        a.insert(
            "source_url".to_string(),
            "https://example.com/a".to_string(),
        );
        let material = AdvisorMaterial::from_attributes(&a).expect("https source_url must parse");
        assert_eq!(
            material.source_url.as_deref(),
            Some("https://example.com/a")
        );
    }

    // --- レビュー3巡目 Warning: source_url を url::Url でパース検証すること ---
    // (starts_with("https://") は "https://"(ホスト無し)や userinfo による接続先偽装を
    // 素通ししていた。draftgen.rs の URL allowlist の許可集合はこの source_url から
    // 組み立てられるため、ここで拒否しないとそのまま allowlist に混入する)

    #[test]
    fn from_attributes_rejects_a_hostless_https_source_url_and_warns() {
        let mut a = full_attrs();
        a.insert("source_url".to_string(), "https://".to_string());
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        assert_eq!(
            result, None,
            "a bare https:// with no host must be rejected before it reaches the URL allowlist"
        );
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn from_attributes_rejects_a_source_url_with_empty_userinfo_and_warns() {
        // "https://@evil.example" は username()/password() だけを見ると url crate が
        // 正規化時に userinfo を完全に消し去るため「userinfo 無し」に見えてしまう
        // (実測: パース後は "https://evil.example/" と区別が付かない)。生文字列の
        // authority 部分に "@" が含まれるかを別途見て拒否する。
        let mut a = full_attrs();
        a.insert(
            "source_url".to_string(),
            "https://@evil.example".to_string(),
        );
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        assert_eq!(result, None);
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn from_attributes_rejects_a_source_url_with_userinfo_impersonating_a_host_and_warns() {
        let mut a = full_attrs();
        a.insert(
            "source_url".to_string(),
            "https://example.com@evil.example/path".to_string(),
        );
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        assert_eq!(
            result, None,
            "a source_url embedding a real-looking host as userinfo must be rejected"
        );
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    // --- Warning D: kind 語彙検証・source_url required 検証 ---

    #[test]
    fn from_attributes_rejects_kind_outside_the_vocabulary_and_warns() {
        let mut a = full_attrs();
        a.insert("kind".to_string(), "arbitrary_value".to_string());
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        assert_eq!(
            result, None,
            "kind outside statistic/own_product/partner_product/scenario must be rejected"
        );
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn from_attributes_rejects_statistic_without_source_url_and_warns() {
        let mut a = full_attrs();
        a.insert("kind".to_string(), "statistic".to_string());
        a.remove("source_url");
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        assert_eq!(
            result, None,
            "kind=statistic is required to carry source_url (design doc §5.1)"
        );
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn from_attributes_rejects_partner_product_with_blank_source_url_and_warns() {
        let mut a = full_attrs();
        a.insert("kind".to_string(), "partner_product".to_string());
        a.insert("source_url".to_string(), "".to_string());
        let (result, logs) = capture_logs(|| AdvisorMaterial::from_attributes(&a));
        assert_eq!(
            result, None,
            "kind=partner_product is required to carry source_url (design doc §5.1)"
        );
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn from_attributes_accepts_own_product_without_source_url() {
        let mut a = full_attrs();
        a.insert("kind".to_string(), "own_product".to_string());
        a.remove("source_url");
        let material =
            AdvisorMaterial::from_attributes(&a).expect("own_product's source_url is optional");
        assert_eq!(material.source_url, None);
    }

    #[test]
    fn from_attributes_accepts_scenario_without_source_url() {
        let mut a = full_attrs();
        a.insert("kind".to_string(), "scenario".to_string());
        a.remove("source_url");
        let material =
            AdvisorMaterial::from_attributes(&a).expect("scenario's source_url is optional");
        assert_eq!(material.source_url, None);
    }

    // --- build_material_search_text ---

    #[test]
    fn build_material_search_text_uses_query_alone_when_no_conditions() {
        assert_eq!(
            build_material_search_text("玄関の防犯が心配です", &[]),
            "玄関の防犯が心配です"
        );
    }

    #[test]
    fn build_material_search_text_appends_condition_values_space_separated() {
        let conditions = vec![
            (ConditionKey::Housing, "apartment_rented".to_string()),
            (ConditionKey::Concern, "intrusion".to_string()),
        ];
        assert_eq!(
            build_material_search_text("玄関の防犯が心配です", &conditions),
            "玄関の防犯が心配です apartment_rented intrusion"
        );
    }

    // --- select_materials_of_kind ---

    fn own_product_attrs(material_key: &str, category: &str) -> HashMap<String, String> {
        attrs(&[
            ("material_key", material_key),
            ("kind", "own_product"),
            ("title_ja", "URTECT製品"),
            ("body_ja", "本文"),
            ("source_url", ""),
            ("category", category),
            ("product_key", ""),
            ("price_band", ""),
            ("card_description", ""),
            ("card_match_terms", ""),
        ])
    }

    #[test]
    fn select_materials_of_kind_returns_only_the_matching_kind() {
        let mut node_attrs = HashMap::new();
        node_attrs.insert(
            "id-own".to_string(),
            own_product_attrs("own_product:adc-v724", "monitoring"),
        );
        node_attrs.insert(
            "id-statistic".to_string(),
            attrs(&[
                ("material_key", "statistic:musimari-46percent"),
                ("kind", "statistic"),
                ("title_ja", "統計"),
                ("body_ja", "本文"),
                ("source_url", "https://example.com/a"),
                ("category", "intrusion"),
                ("product_key", ""),
                ("price_band", ""),
                ("card_description", ""),
                ("card_match_terms", ""),
            ]),
        );

        let materials = select_materials_of_kind(&node_attrs, "own_product");
        assert_eq!(materials.len(), 1);
        assert_eq!(materials[0].material_key, "own_product:adc-v724");
    }

    #[test]
    fn select_materials_of_kind_sorts_by_material_key_ascending_deterministically() {
        let mut node_attrs = HashMap::new();
        node_attrs.insert(
            "id-c".to_string(),
            own_product_attrs("own_product:c", "intrusion"),
        );
        node_attrs.insert(
            "id-a".to_string(),
            own_product_attrs("own_product:a", "intrusion"),
        );
        node_attrs.insert(
            "id-b".to_string(),
            own_product_attrs("own_product:b", "intrusion"),
        );

        let materials = select_materials_of_kind(&node_attrs, "own_product");
        let keys: Vec<&str> = materials.iter().map(|m| m.material_key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["own_product:a", "own_product:b", "own_product:c"]
        );
    }

    // --- select_materials_from_hits ---

    fn hit(id: &str) -> SearchResultItem {
        SearchResultItem {
            r#type: "node".to_string(),
            id: Some(id.to_string()),
            score: Some(0.9),
            ..Default::default()
        }
    }

    #[test]
    fn select_materials_from_hits_converts_advisor_material_hits_in_order() {
        let id1 = "homesec:gen1:advisor_material:own_product:adc-v724";
        let id2 = "homesec:gen1:advisor_material:statistic:musimari-46percent";
        let hits = vec![hit(id1), hit(id2)];
        let mut node_attrs = HashMap::new();
        node_attrs.insert(id1.to_string(), full_attrs());
        node_attrs.insert(
            id2.to_string(),
            attrs(&[
                ("material_key", "statistic:musimari-46percent"),
                ("kind", "statistic"),
                ("title_ja", "無締りは侵入手段の半数近く"),
                ("body_ja", "本文"),
                ("source_url", "https://example.com/a"),
                ("category", "intrusion"),
                ("product_key", ""),
                ("price_band", ""),
                ("card_description", ""),
                ("card_match_terms", ""),
            ]),
        );

        let materials = select_materials_from_hits(&hits, &node_attrs);
        assert_eq!(materials.len(), 2);
        assert_eq!(materials[0].material_key, "own_product:adc-v724");
        assert_eq!(materials[1].material_key, "statistic:musimari-46percent");
    }

    /// 検索非汚染(design doc 不変条件7)の固定テスト: `ConversationTurn` / `support_case` の
    /// id が hits に混ざっても、返り値に一切現れない。
    #[test]
    fn select_materials_from_hits_excludes_conversation_turn_and_support_case_ids() {
        let material_id = "homesec:gen1:advisor_material:own_product:adc-v724";
        let hits = vec![
            hit("homesec:gen1:ConversationTurn:turn-1"),
            hit(material_id),
            hit("homesec:gen1:support_case:case-1"),
        ];
        let mut node_attrs = HashMap::new();
        node_attrs.insert(material_id.to_string(), full_attrs());
        // 万一 ConversationTurn / support_case の id が node_attrs に載っていても、
        // marker フィルタの時点で弾かれ from_attributes にすら渡らないことを確認するため、
        // ダミーの attrs を仕込んでおく(もし marker フィルタが機能していなければ、これが
        // 誤って AdvisorMaterial 化されてテストが失敗する)。
        node_attrs.insert(
            "homesec:gen1:ConversationTurn:turn-1".to_string(),
            full_attrs(),
        );
        node_attrs.insert("homesec:gen1:support_case:case-1".to_string(), full_attrs());

        let materials = select_materials_from_hits(&hits, &node_attrs);
        assert_eq!(materials.len(), 1);
        assert_eq!(materials[0].material_key, "own_product:adc-v724");
    }

    #[test]
    fn select_materials_from_hits_skips_and_warns_when_id_not_in_node_attrs() {
        let material_id = "homesec:gen1:advisor_material:own_product:adc-v724";
        let hits = vec![hit(material_id)];
        let node_attrs = HashMap::new();

        let (materials, logs) =
            crate::test_support::capture_logs(|| select_materials_from_hits(&hits, &node_attrs));
        assert!(materials.is_empty());
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn select_materials_from_hits_skips_hit_without_id() {
        let hit_without_id = SearchResultItem {
            r#type: "node".to_string(),
            id: None,
            ..Default::default()
        };
        let materials = select_materials_from_hits(&[hit_without_id], &HashMap::new());
        assert!(materials.is_empty());
    }

    // --- materials_from_results (Warning B: 失敗しても Err を伝播しない契約の固定) ---

    #[test]
    fn materials_from_results_returns_empty_and_warns_when_search_fails() {
        let ((searched, own_products, category_pool), logs) = capture_logs(|| {
            materials_from_results(
                Err(anyhow::anyhow!("vegapunk search unavailable (test)")),
                Ok(Vec::new()),
                "homesec",
            )
        });
        assert!(
            searched.is_empty(),
            "a failed search must degrade to zero materials, not propagate Err"
        );
        assert!(own_products.is_empty());
        assert!(category_pool.is_empty());
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn materials_from_results_returns_empty_and_warns_when_query_nodes_fails() {
        let ((searched, own_products, category_pool), logs) = capture_logs(|| {
            materials_from_results(
                Ok(Vec::new()),
                Err(anyhow::anyhow!("vegapunk query_nodes unavailable (test)")),
                "homesec",
            )
        });
        assert!(
            searched.is_empty(),
            "a failed query_nodes must degrade to zero materials, not propagate Err"
        );
        assert!(own_products.is_empty());
        assert!(category_pool.is_empty());
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn materials_from_results_matches_select_materials_from_hits_when_both_succeed() {
        let id = "homesec:gen1:advisor_material:own_product:adc-v724";
        let hits = vec![hit(id)];
        let mut node_attrs = HashMap::new();
        node_attrs.insert(id.to_string(), full_attrs());
        let nodes = vec![crate::proto::graphrag::NodeResult {
            node_id: id.to_string(),
            node_type: "advisor_material".to_string(),
            attributes: full_attrs(),
        }];

        let expected_searched = select_materials_from_hits(&hits, &node_attrs);
        let (actual_searched, _actual_own_products, _actual_category_pool) =
            materials_from_results(Ok(hits), Ok(nodes), "homesec");
        assert_eq!(actual_searched, expected_searched);
        assert_eq!(actual_searched.len(), 1);
    }

    #[test]
    fn materials_from_results_second_element_contains_only_own_product_kind_nodes() {
        // Task 2b: query_nodes が返す全 advisor_material のうち kind == "own_product" の
        // ものだけが own_products 側に入り、他 kind(statistic 等)は混ざらないことを固定する。
        let own_id = "homesec:gen1:advisor_material:own_product:adc-v724";
        let statistic_id = "homesec:gen1:advisor_material:statistic:musimari-46percent";
        let statistic_attrs = attrs(&[
            ("material_key", "statistic:musimari-46percent"),
            ("kind", "statistic"),
            ("title_ja", "統計"),
            ("body_ja", "本文"),
            ("source_url", "https://example.com/a"),
            ("category", "intrusion"),
            ("product_key", ""),
            ("price_band", ""),
            ("card_description", ""),
            ("card_match_terms", ""),
        ]);
        let nodes = vec![
            crate::proto::graphrag::NodeResult {
                node_id: own_id.to_string(),
                node_type: "advisor_material".to_string(),
                attributes: full_attrs(),
            },
            crate::proto::graphrag::NodeResult {
                node_id: statistic_id.to_string(),
                node_type: "advisor_material".to_string(),
                attributes: statistic_attrs,
            },
        ];

        let (_searched, own_products, _category_pool) =
            materials_from_results(Ok(Vec::new()), Ok(nodes), "homesec");
        assert_eq!(own_products.len(), 1);
        assert_eq!(own_products[0].material_key, "own_product:adc-v724");
        assert_eq!(own_products[0].kind, "own_product");
    }

    /// タスク1: `category_pool`(3つ目の要素)には kind が statistic/partner_product/scenario
    /// の material だけが入り、own_product は入らないことを固定する(design doc §6 手順6
    /// 「category 合致材料」の入力集合)。
    #[test]
    fn materials_from_results_third_element_contains_statistic_partner_product_and_scenario_but_not_own_product(
    ) {
        let own_id = "homesec:gen1:advisor_material:own_product:adc-v724";
        let statistic_id = "homesec:gen1:advisor_material:statistic:musimari-46percent";
        let partner_id = "homesec:gen1:advisor_material:partner_product:sensor-light";
        let scenario_id = "homesec:gen1:advisor_material:scenario:elderly-watch";
        let statistic_attrs = attrs(&[
            ("material_key", "statistic:musimari-46percent"),
            ("kind", "statistic"),
            ("title_ja", "統計"),
            ("body_ja", "本文"),
            ("source_url", "https://example.com/a"),
            ("category", "intrusion"),
            ("product_key", ""),
            ("price_band", ""),
            ("card_description", ""),
            ("card_match_terms", ""),
        ]);
        let partner_attrs = attrs(&[
            ("material_key", "partner_product:sensor-light"),
            ("kind", "partner_product"),
            ("title_ja", "センサーライト"),
            ("body_ja", "本文"),
            ("source_url", "https://example.com/b"),
            ("category", "intrusion"),
            ("product_key", ""),
            ("price_band", ""),
            ("card_description", ""),
            ("card_match_terms", ""),
        ]);
        let scenario_attrs = attrs(&[
            ("material_key", "scenario:elderly-watch"),
            ("kind", "scenario"),
            ("title_ja", "見守りの例"),
            ("body_ja", "本文"),
            ("source_url", ""),
            ("category", "intrusion"),
            ("product_key", ""),
            ("price_band", ""),
            ("card_description", ""),
            ("card_match_terms", ""),
        ]);
        let nodes = vec![
            crate::proto::graphrag::NodeResult {
                node_id: own_id.to_string(),
                node_type: "advisor_material".to_string(),
                attributes: full_attrs(),
            },
            crate::proto::graphrag::NodeResult {
                node_id: statistic_id.to_string(),
                node_type: "advisor_material".to_string(),
                attributes: statistic_attrs,
            },
            crate::proto::graphrag::NodeResult {
                node_id: partner_id.to_string(),
                node_type: "advisor_material".to_string(),
                attributes: partner_attrs,
            },
            crate::proto::graphrag::NodeResult {
                node_id: scenario_id.to_string(),
                node_type: "advisor_material".to_string(),
                attributes: scenario_attrs,
            },
        ];

        let (_searched, _own_products, category_pool) =
            materials_from_results(Ok(Vec::new()), Ok(nodes), "homesec");
        let mut keys: Vec<&str> = category_pool
            .iter()
            .map(|m| m.material_key.as_str())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "partner_product:sensor-light",
                "scenario:elderly-watch",
                "statistic:musimari-46percent",
            ]
        );
        assert!(
            category_pool.iter().all(|m| m.kind != "own_product"),
            "own_product must never appear in the category_pool: {category_pool:?}"
        );
    }

    // --- conditions_to_signal_set / match_advisor_known_resolution ---

    #[test]
    fn conditions_to_signal_set_formats_key_colon_value() {
        let conditions = vec![
            (ConditionKey::Concern, "intrusion".to_string()),
            (ConditionKey::Housing, "apartment_rented".to_string()),
        ];
        let set = conditions_to_signal_set(&conditions);
        assert!(set.contains(&Signal::new("concern:intrusion")));
        assert!(set.contains(&Signal::new("housing:apartment_rented")));
        assert_eq!(set.len(), 2);
    }

    fn kr(id: &str, set: &[&str], answer: &str) -> KnownResolution {
        KnownResolution {
            id: id.to_string(),
            signal_set: set.iter().map(|s| Signal::new(*s)).collect(),
            applicability: "".to_string(),
            answer: answer.to_string(),
            source_authority: SourceAuthority::Authoritative,
            root_cause: RootCause::KnowledgeError,
            grade: Grade::ApprovalRequired,
            approval_count: 0,
            rejection_count: 0,
            approver_set: Vec::new(),
            origin: "test".to_string(),
            binding: Binding::Advisory,
            registration_trigger: "single_ruling".to_string(),
            knowledge_class: "commercial".to_string(),
            outcome_ref: Vec::new(),
        }
    }

    #[test]
    fn match_advisor_known_resolution_returns_some_on_applicable_match() {
        let resolutions = vec![kr(
            "kr-1",
            &["concern:intrusion", "housing:apartment_rented"],
            "賃貸の玄関防犯にはこの案内が有効です。",
        )];
        let conditions = vec![
            (ConditionKey::Concern, "intrusion".to_string()),
            (ConditionKey::Housing, "apartment_rented".to_string()),
        ];
        let matched = match_advisor_known_resolution(&resolutions, &conditions);
        assert_eq!(matched.map(|kr| kr.id.as_str()), Some("kr-1"));
    }

    #[test]
    fn match_advisor_known_resolution_returns_none_when_no_kr_is_a_subset() {
        let resolutions = vec![kr("kr-1", &["concern:fire_disaster"], "本文")];
        let conditions = vec![(ConditionKey::Concern, "intrusion".to_string())];
        assert!(match_advisor_known_resolution(&resolutions, &conditions).is_none());
    }

    #[test]
    fn match_advisor_known_resolution_returns_none_when_blocked_by_added_signal() {
        // KR の signal_set が質問の signal に対して真のスーパーセットになっている(質問側に
        // 無い concern:stalking を要求する)ため、KrMatch::BlockedByAddedSignal になり、
        // homesec スコープ外として None を返す。
        let resolutions = vec![kr(
            "kr-1",
            &["concern:intrusion", "concern:stalking"],
            "本文",
        )];
        let conditions = vec![(ConditionKey::Concern, "intrusion".to_string())];
        assert!(match_advisor_known_resolution(&resolutions, &conditions).is_none());
    }

    // --- known_resolution_to_material ---

    #[test]
    fn known_resolution_to_material_never_sets_card_description() {
        let resolution = kr("kr-1", &["concern:intrusion"], "承認済みの回答本文");
        let material = known_resolution_to_material(&resolution);
        assert_eq!(material.material_key, "known_resolution:kr-1");
        assert_eq!(material.kind, "known_resolution");
        assert_eq!(material.body_ja, "承認済みの回答本文");
        assert_eq!(
            material.card_description, None,
            "KR-derived materials must never be card candidates"
        );
    }

    // --- レビュー2巡目 Warning B: compose_materials(KR を材料先頭へ注入する合成) ---

    fn searched_material(material_key: &str) -> AdvisorMaterial {
        AdvisorMaterial {
            material_key: material_key.to_string(),
            kind: "statistic".to_string(),
            title_ja: "タイトル".to_string(),
            body_ja: "本文".to_string(),
            source_url: Some("https://example.com/a".to_string()),
            category: None,
            product_key: None,
            price_band: None,
            card_description: None,
            card_match_terms: None,
            product_page_url: None,
        }
    }

    #[test]
    fn compose_materials_puts_the_known_resolution_first_and_keeps_searched_order() {
        let resolution = kr("kr-1", &["concern:intrusion"], "承認済みの回答本文");
        let searched = vec![
            searched_material("statistic:a"),
            searched_material("statistic:b"),
        ];

        let composed = compose_materials(Some(&resolution), searched);

        assert_eq!(composed.len(), 3);
        assert_eq!(composed[0].kind, "known_resolution");
        assert_eq!(composed[0].material_key, "known_resolution:kr-1");
        assert_eq!(composed[1].material_key, "statistic:a");
        assert_eq!(composed[2].material_key, "statistic:b");
    }

    #[test]
    fn compose_materials_returns_searched_as_is_when_no_known_resolution() {
        let searched = vec![
            searched_material("statistic:a"),
            searched_material("statistic:b"),
        ];

        let composed = compose_materials(None, searched.clone());

        assert_eq!(composed, searched);
    }

    #[test]
    fn compose_materials_injected_known_resolution_has_no_card_description() {
        let resolution = kr("kr-1", &["concern:intrusion"], "承認済みの回答本文");

        let composed = compose_materials(Some(&resolution), Vec::new());

        assert_eq!(composed.len(), 1);
        assert_eq!(
            composed[0].card_description, None,
            "the KR-derived material at the front must never be a card candidate"
        );
    }

    // --- concern_category (design doc §6 手順6。reviewer 指摘3是正: `draft_with_materials`
    // にインラインで書かれ、ネットワーク呼び出しを含むためテスト対象外だった判断を
    // 純関数として切り出した) ---

    #[test]
    fn concern_category_returns_the_concern_value_when_present() {
        let conditions = vec![
            (ConditionKey::Housing, "apartment_rented".to_string()),
            (ConditionKey::Concern, "intrusion".to_string()),
        ];
        assert_eq!(concern_category(&conditions), Some("intrusion"));
    }

    #[test]
    fn concern_category_returns_none_when_conditions_are_empty() {
        assert_eq!(concern_category(&[]), None);
    }

    #[test]
    fn concern_category_returns_none_when_only_non_concern_conditions_are_present() {
        let conditions = vec![
            (ConditionKey::Housing, "apartment_rented".to_string()),
            (ConditionKey::Budget, "under_10k".to_string()),
        ];
        assert_eq!(concern_category(&conditions), None);
    }

    // --- should_guarantee_own_products (design doc §6 手順6) ---

    #[test]
    fn should_guarantee_own_products_true_when_product_intent_is_true() {
        assert!(should_guarantee_own_products(true, &[]));
    }

    #[test]
    fn should_guarantee_own_products_true_when_conditions_contain_concern() {
        let conditions = vec![(ConditionKey::Concern, "intrusion".to_string())];
        assert!(should_guarantee_own_products(false, &conditions));
    }

    #[test]
    fn should_guarantee_own_products_false_when_neither_condition_holds() {
        assert!(!should_guarantee_own_products(false, &[]));
    }

    #[test]
    fn should_guarantee_own_products_false_when_conditions_present_but_no_concern() {
        let conditions = vec![(ConditionKey::Housing, "apartment_rented".to_string())];
        assert!(!should_guarantee_own_products(false, &conditions));
    }

    // --- select_own_product_materials (design doc §6 手順6) ---

    fn own_product_material(material_key: &str, category: Option<&str>) -> AdvisorMaterial {
        AdvisorMaterial {
            material_key: material_key.to_string(),
            kind: "own_product".to_string(),
            title_ja: "URTECT製品".to_string(),
            body_ja: "本文".to_string(),
            source_url: None,
            category: category.map(str::to_string),
            product_key: None,
            price_band: None,
            card_description: Some("説明".to_string()),
            card_match_terms: None,
            product_page_url: None,
        }
    }

    #[test]
    fn select_own_product_materials_prefers_matching_category_and_excludes_others() {
        let own_products = vec![
            own_product_material("own_product:a", Some("monitoring")),
            own_product_material("own_product:b", Some("intrusion")),
        ];
        let selected = select_own_product_materials(&own_products, Some("intrusion"));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].material_key, "own_product:b");
    }

    #[test]
    fn select_own_product_materials_returns_all_when_no_category_matches() {
        let own_products = vec![
            own_product_material("own_product:a", Some("monitoring")),
            own_product_material("own_product:b", Some("package_theft")),
        ];
        let selected = select_own_product_materials(&own_products, Some("intrusion"));
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn select_own_product_materials_returns_all_when_concern_category_is_none() {
        let own_products = vec![
            own_product_material("own_product:a", Some("monitoring")),
            own_product_material("own_product:b", Some("intrusion")),
        ];
        let selected = select_own_product_materials(&own_products, None);
        assert_eq!(selected.len(), 2);
    }

    // --- inject_guaranteed_own_products (design doc §6 手順6) ---

    #[test]
    fn inject_guaranteed_own_products_appends_materials_absent_from_composed() {
        let composed = vec![searched_material("statistic:a")];
        let guaranteed = vec![own_product_material("own_product:adc-v724", None)];

        let result = inject_guaranteed_own_products(composed, guaranteed);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].material_key, "statistic:a");
        assert_eq!(result[1].material_key, "own_product:adc-v724");
    }

    #[test]
    fn inject_guaranteed_own_products_deduplicates_material_keys_already_in_composed() {
        let composed = vec![
            searched_material("statistic:a"),
            own_product_material("own_product:adc-v724", None),
        ];
        let guaranteed = vec![own_product_material("own_product:adc-v724", None)];

        let result = inject_guaranteed_own_products(composed, guaranteed);

        assert_eq!(
            result.len(),
            2,
            "a guaranteed material whose key already exists in composed must not be duplicated"
        );
    }

    #[test]
    fn inject_guaranteed_own_products_deduplicates_a_key_that_repeats_within_guaranteed_itself() {
        // codex レビュー指摘の回帰: `existing_keys` を初期状態から一度だけ作って push 後に
        // 更新しないと、`guaranteed` 内部に同一 material_key が2件あった場合(別 node_id が
        // 同じ material_key を持つケース)両方とも追加されてしまう。
        let composed = vec![searched_material("statistic:a")];
        let guaranteed = vec![
            own_product_material("own_product:adc-v724", None),
            own_product_material("own_product:adc-v724", None),
        ];

        let result = inject_guaranteed_own_products(composed, guaranteed);

        assert_eq!(
            result.len(),
            2,
            "a material_key that repeats within guaranteed itself must be added only once"
        );
    }

    // --- select_category_materials / inject_category_materials (design doc §6 手順6
    // 「category 合致材料」。own_product 保証注入とは別枠の kind ごとの決定論的な保証注入) ---

    fn category_material(material_key: &str, kind: &str, category: &str) -> AdvisorMaterial {
        AdvisorMaterial {
            material_key: material_key.to_string(),
            kind: kind.to_string(),
            title_ja: "タイトル".to_string(),
            body_ja: "本文".to_string(),
            source_url: Some("https://example.com/a".to_string()),
            category: Some(category.to_string()),
            product_key: None,
            price_band: None,
            card_description: None,
            card_match_terms: None,
            product_page_url: None,
        }
    }

    #[test]
    fn select_category_materials_returns_empty_when_concern_category_is_none() {
        let pool = vec![category_material("statistic:a", "statistic", "intrusion")];
        assert!(select_category_materials(&pool, None).is_empty());
    }

    #[test]
    fn select_category_materials_returns_only_matching_category_and_kind() {
        let pool = vec![
            category_material("statistic:a", "statistic", "intrusion"),
            // category が違うので対象外。
            category_material("statistic:b", "statistic", "package_theft"),
            // kind が対象3種の範囲外なので対象外(category_pool には本来own_productは
            // 混ざらないが、フィルタ自体が kind を見て除外できることも固定しておく)。
            category_material("own_product:c", "own_product", "intrusion"),
            category_material("partner_product:d", "partner_product", "intrusion"),
        ];
        let selected = select_category_materials(&pool, Some("intrusion"));
        let keys: Vec<&str> = selected.iter().map(|m| m.material_key.as_str()).collect();
        assert_eq!(keys, vec!["statistic:a", "partner_product:d"]);
    }

    #[test]
    fn select_category_materials_limits_statistic_to_two_ascending() {
        let pool = vec![
            category_material("statistic:c", "statistic", "intrusion"),
            category_material("statistic:a", "statistic", "intrusion"),
            category_material("statistic:b", "statistic", "intrusion"),
        ];
        let selected = select_category_materials(&pool, Some("intrusion"));
        let keys: Vec<&str> = selected.iter().map(|m| m.material_key.as_str()).collect();
        assert_eq!(keys, vec!["statistic:a", "statistic:b"]);
    }

    #[test]
    fn select_category_materials_limits_partner_product_to_two() {
        let pool = vec![
            category_material("partner_product:c", "partner_product", "intrusion"),
            category_material("partner_product:a", "partner_product", "intrusion"),
            category_material("partner_product:b", "partner_product", "intrusion"),
        ];
        let selected = select_category_materials(&pool, Some("intrusion"));
        let keys: Vec<&str> = selected.iter().map(|m| m.material_key.as_str()).collect();
        assert_eq!(keys, vec!["partner_product:a", "partner_product:b"]);
    }

    #[test]
    fn select_category_materials_limits_scenario_to_one() {
        let pool = vec![
            category_material("scenario:b", "scenario", "intrusion"),
            category_material("scenario:a", "scenario", "intrusion"),
        ];
        let selected = select_category_materials(&pool, Some("intrusion"));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].material_key, "scenario:a");
    }

    #[test]
    fn select_category_materials_orders_statistic_then_partner_product_then_scenario() {
        // design doc §6 手順6「この順(statistic → partner_product → scenario)で連結」の固定。
        let pool = vec![
            category_material("scenario:s", "scenario", "intrusion"),
            category_material("partner_product:p", "partner_product", "intrusion"),
            category_material("statistic:t", "statistic", "intrusion"),
        ];
        let selected = select_category_materials(&pool, Some("intrusion"));
        let keys: Vec<&str> = selected.iter().map(|m| m.material_key.as_str()).collect();
        assert_eq!(keys, vec!["statistic:t", "partner_product:p", "scenario:s"]);
    }

    #[test]
    fn inject_category_materials_appends_candidates_absent_from_composed() {
        let composed = vec![searched_material("statistic:existing")];
        let candidates = vec![category_material(
            "partner_product:new",
            "partner_product",
            "intrusion",
        )];

        let result = inject_category_materials(composed, candidates);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].material_key, "statistic:existing");
        assert_eq!(result[1].material_key, "partner_product:new");
    }

    #[test]
    fn inject_category_materials_deduplicates_material_keys_already_in_composed() {
        let composed = vec![
            searched_material("statistic:a"),
            category_material("partner_product:existing", "partner_product", "intrusion"),
        ];
        let candidates = vec![category_material(
            "partner_product:existing",
            "partner_product",
            "intrusion",
        )];

        let result = inject_category_materials(composed, candidates);

        assert_eq!(
            result.len(),
            2,
            "a candidate whose key already exists in composed must not be duplicated"
        );
    }

    #[test]
    fn inject_category_materials_deduplicates_a_key_that_repeats_within_candidates_itself() {
        let composed = vec![searched_material("statistic:a")];
        let candidates = vec![
            category_material("scenario:dup", "scenario", "intrusion"),
            category_material("scenario:dup", "scenario", "intrusion"),
        ];

        let result = inject_category_materials(composed, candidates);

        assert_eq!(
            result.len(),
            2,
            "a material_key that repeats within candidates itself must be added only once"
        );
    }

    #[test]
    fn inject_category_materials_adds_nothing_when_composed_already_at_the_total_cap() {
        let composed: Vec<AdvisorMaterial> = (0..12)
            .map(|i| searched_material(&format!("statistic:existing-{i}")))
            .collect();
        let candidates = vec![category_material("scenario:new", "scenario", "intrusion")];

        let result = inject_category_materials(composed.clone(), candidates);

        assert_eq!(
            result, composed,
            "composed already holds MAX_TOTAL_MATERIALS (12) items; nothing more may be added"
        );
    }

    #[test]
    fn inject_category_materials_stops_at_the_total_cap_mid_candidates() {
        let composed: Vec<AdvisorMaterial> = (0..10)
            .map(|i| searched_material(&format!("statistic:existing-{i}")))
            .collect();
        let candidates = vec![
            category_material("partner_product:a", "partner_product", "intrusion"),
            category_material("partner_product:b", "partner_product", "intrusion"),
            category_material("scenario:c", "scenario", "intrusion"),
        ];

        let result = inject_category_materials(composed, candidates);

        assert_eq!(
            result.len(),
            12,
            "10 existing + 3 candidates must stop at the 12-item cap, not reach 13"
        );
    }
}
