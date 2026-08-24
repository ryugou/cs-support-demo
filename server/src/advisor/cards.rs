//! 製品カードの添付判定(design doc `2026-08-17-homesec-advisor-design.md` §7.2)。
//!
//! **分担の境界**: `images_dir` はファイルシステム上の実ディレクトリ(存在確認用)であり、
//! URL ではない。返す [`ProductCard::image_url`] は**ホストを含まない相対パス**
//! (例 `/static/products/adc-v724.jpg`)にする。ホスト名を付与して完全 URL にするのは
//! Task 6(ハンドラ、`CS_SUPPORT_PUBLIC_DOMAIN` を知っている層)の責務であり、ここでは
//! 行わない。これにより [`select_cards`] は純粋・決定論のまま保たれ、tempdir を使った
//! ユニットテストだけで検証できる。

use crate::advisor::draftgen::{ClosingKind, DraftMeta};
use crate::advisor::materials::{is_well_formed_https_url, AdvisorMaterial};
use crate::harness::egress::{egress_gate, EgressVerdict, EmitChannel, EmitContext, NgDictionary};
use crate::harness::knowledge::csv_list;
use crate::harness::product_gate::extract_model_tokens;
use crate::harness::prompt_input::to_plain_text;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// カードのボタン1件(design doc §3.3、2026-08-21 conversation-rhythm-implementation §要件4)。
///
/// タップ後に会話が行き止まりになる問題(本番実害 (c))への是正: 旧実装は
/// `button_text`/`button_message` の単一ペア(常に「詳しく聞く」相当の1ボタン)しか持てず、
/// 特に他社製品(partner_product)のカードから「入手方法・頼み方」へ会話を繋げられなかった。
/// `kind` タグで LINE Messaging API の action 種別(`uri` / `message`)をそのまま表現する。
///
/// JSON 表現(`#[serde(tag = "kind", rename_all = "snake_case")]`):
/// `{"kind":"uri","label":"商品ページを見る","url":"https://..."}`
/// `{"kind":"message","label":"詳しく聞く","message":"ADC-V724について詳しく教えて"}`
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CardButton {
    Uri { label: String, url: String },
    Message { label: String, message: String },
}

/// design doc §3.3 の `product_cards` 1 件。
///
/// `Serialize` を derive する(Task 6): フィールド名は design doc §3.3 の JSON 例
/// (`material_key` / `title` / `description` / `image_url` / `buttons`)と完全一致している
/// ため `#[serde(rename = ...)]` は不要。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProductCard {
    pub material_key: String,
    pub title: String,
    pub description: String,
    pub image_url: Option<String>,
    /// このカードのボタン列(design doc §3.3、要件4)。組み立て規則は [`build_card_buttons`]
    /// の doc comment を参照。
    pub buttons: Vec<CardButton>,
}

/// own_product の「詳しく聞く」ボタンの文言(固定。ユーザー指定)。
const DETAIL_BUTTON_LABEL: &str = "詳しく聞く";
/// own_product の「導入を相談する」ボタンの文言(固定。ユーザー指定)。
const CONSULT_BUTTON_LABEL: &str = "導入を相談する";
/// partner_product の「選び方を聞く」ボタンの文言(固定。ユーザー指定)。
const CHOOSING_BUTTON_LABEL: &str = "選び方を聞く";
/// `product_page_url` がある場合に先頭へ加える URI ボタンの文言(固定)。
const PRODUCT_PAGE_BUTTON_LABEL: &str = "商品ページを見る";

/// design doc §7.2 の製品カード添付判定本体。2026-08-21 conversation-rhythm-implementation
/// §要件3 により、候補の起点を LLM Call#2 のメタ(`meta.featured`)へ変更した。
///
/// 手順:
/// 0. `meta` が `None`、または `meta.closing != ClosingKind::Proposal` なら、この応答は
///    「提案」で締めていない(質問で締めた、または Call#2 がメタ付きで成功しなかった)ため
///    カードは一切出さない(空配列)。これが最優先のゲートで、以下の手順1〜5は
///    `closing == Proposal` のときだけ評価する(本番実害 (a):「特に心配な場所は?」と
///    質問で締めたターンに商品カードが付き会話が噛み合わない、への是正)。
/// 1. `injected` のうち `kind` が `own_product` / `partner_product`、かつ
///    `card_description.is_some()`、かつ `meta.featured` に `material_key` が列挙されている
///    材料だけを候補にする(`meta.featured` に無いものは「今回主役として提案していない」と
///    みなし破棄する。本番実害 (b): 助言の一要素(防犯フィルム等、提案の主役でない商品)まで
///    カード化される、への是正)。`kind` 側の絞り込みが無いと `statistic` / `scenario` 材料でも
///    `card_description` さえあればカード化されてしまい、`kind_priority` の「own_product
///    以外は一律 partner 相当」という前提とも食い違う。`known_resolution_to_material` により
///    `card_description = None` の KR 由来材料は元々ここで除外される。`meta.featured` に
///    列挙されていても `injected` に実在しない `material_key`(存在しない材料への言及)は、
///    この filter が `injected` を起点に回る以上、自然に無視される(カードを捏造しない)。
/// 2. `final_text`(出口関門を通過した最終応答文、正規化後)に照合語が含まれるかを判定する
///    (`matches_final_text`。手順1の featured フィルタとは独立の第2ゲート — LLM がメタの
///    featured に挙げても、本文中で実際に言及していなければカード化しない):
///    `kind == "own_product"` は**型番明示のみ**(`product_key` を
///    [`extract_model_tokens`] で正規化したトークンが `final_text` の型番トークン集合に
///    含まれる場合だけ合致。`card_match_terms` の汎用語では合致させない — 本番実害
///    「応答文に型番が無い own_product までカード化される」への是正、design doc §7.2 手順1)。
///    それ以外の kind(`partner_product` 等)は従来どおり: `card_match_terms` があればその
///    CSV 分割語のいずれか、無ければ `title_ja` または `product_key` のいずれか。design doc
///    §5.1 の実文言は「省略時は `title_ja` で照合する」であり、`product_key` フォールバック
///    (下記 [`matches_final_text`] 参照)は spec には無い実装側の防御的な追加(reviewer 一次
///    レビュー Suggestion 1 是正: 以前は spec の引用がこの追加を含むかのように誤記していた)。
/// 3. `shown_csv`(この会話で既に表示済みの material_key の CSV)に含まれる候補を除外する
///    (再表示抑止)。
/// 4. カードの `title_ja` / `card_description` を、応答本文と同じ NG辞書 + plain-text正規化
///    出口関門に通す([`card_text_passes_ng_gate`])。落ちた候補は warn してそのカードだけ除外し、
///    残りの候補には影響しない(Issue #47 Critical指摘2: 多層防御。手順5 の `take(3)` より前に
///    置くことで、1件落ちても後続候補が自然に繰り上がる)。
/// 5. `kind == "own_product"` を先に、`"partner_product"` をその後に安定ソートし、
///    先頭から最大3件を採用する。
/// 6. 採用した候補を [`ProductCard`] に変換する(画像は `images_dir` 上に実在するファイル
///    だけを URL 化する)。
pub fn select_cards(
    final_text: &str,
    injected: &[AdvisorMaterial],
    shown_csv: &str,
    images_dir: &Path,
    meta: Option<&DraftMeta>,
    ng: &NgDictionary,
) -> Vec<ProductCard> {
    let Some(meta) = meta else {
        return Vec::new();
    };
    if meta.closing != ClosingKind::Proposal {
        return Vec::new();
    }

    let shown: std::collections::HashSet<String> = csv_list(shown_csv).into_iter().collect();
    let featured: std::collections::HashSet<&str> =
        meta.featured.iter().map(String::as_str).collect();

    // featured フィルタまでを一旦区切って持つ(reviewer 一次レビュー Major 3 是正)。以前は
    // `own_product`/`partner_product` の materials.json 側で `card_description` が
    // 欠けているだけでも本番でカードが完全に死んでいた(draftgen.rs のコメント参照)のと
    // 同じクラスの静かな失敗が、LLM が featured に返す material_key の側でも起こりうる
    // (資料タグの値をそのままコピーしなかった・型番から推測した等)。このゲートを通過した
    // 候補が0件の場合だけ warn する。以降の `matches_final_text` / `shown` フィルタは
    // 独立した別ゲートであり、そちらの0件は「本文に言及していない」「既出」という別の
    // 正当な理由がありうるため対象にしない。
    let after_featured_filter: Vec<&AdvisorMaterial> = injected
        .iter()
        .filter(|m| matches!(m.kind.as_str(), "own_product" | "partner_product"))
        .filter(|m| m.card_description.is_some())
        .filter(|m| featured.contains(m.material_key.as_str()))
        .collect();

    if !meta.featured.is_empty() && after_featured_filter.is_empty() {
        tracing::warn!(
            route = "advisor_cards",
            featured_len = meta.featured.len(),
            injected_len = injected.len(),
            "none of the material_keys the LLM returned in featured matched any injected \
             own_product/partner_product material; the LLM may not have copied the \
             material_key verbatim from the material tag. Check the material tags built by \
             build_advisor_user_message and the material_key values in \
             server/data/homesec/materials.json"
        );
    }

    let mut candidates: Vec<&AdvisorMaterial> = after_featured_filter
        .into_iter()
        .filter(|m| matches_final_text(m, final_text))
        .filter(|m| !shown.contains(&m.material_key))
        .filter(|m| card_text_passes_ng_gate(m, ng))
        .collect();

    // stable sort: own_product を先に、partner_product をその後に。同じ kind 内では
    // `injected` に現れた元の順序(≒検索ヒット順)を維持する。
    candidates.sort_by_key(|m| kind_priority(&m.kind));

    candidates
        .into_iter()
        .take(3)
        .map(|m| to_product_card(m, images_dir))
        .collect()
}

/// カードの title_ja / card_description を、応答本文と同じ NG辞書・plain-text正規化に通す
/// 多層防御(Issue #47)。キュレーション材料は現状信頼できるソースだが、材料が将来増える
/// 前提のため、応答本文の出口関門(draftgen.rs::apply_advisor_output_gates)と同じ検証を
/// カードにも適用する。
///
/// title と description は**それぞれ独立に** [`egress_gate`] へ通す(Issue #47 レビュー
/// 指摘F4 是正)。以前は `\n` で連結してから1回だけ通していたが、`egress_gate` が使う
/// `normalize_key`([`crate::resolve::normalize_key`])は英数字以外の文字をすべて捨てる
/// ため区切りの `\n` も消え、「title の末尾 + description の先頭」が連結されて NG 語に
/// 一致しうる(どちらのフィールド単独では一致しないのに、境界をまたいで偽陽性でカードが
/// 黙って落ちる)。
fn card_text_passes_ng_gate(material: &AdvisorMaterial, ng: &NgDictionary) -> bool {
    let title = to_plain_text(&material.title_ja);
    let description = material
        .card_description
        .as_deref()
        .map(to_plain_text)
        .unwrap_or_default();
    let ctx = EmitContext {
        channel: EmitChannel::CustomerChat,
    };
    field_passes_ng_gate(material, "title_ja", &title, &ctx, ng)
        && field_passes_ng_gate(material, "card_description", &description, &ctx, ng)
}

/// [`card_text_passes_ng_gate`] が1フィールド分の検証に使う共通ヘルパー。`field` は
/// warn ログへそのまま出す(どちらのフィールドで一致したかを運用者が即座に特定できるように
/// するため、Issue #47 レビュー指摘F4)。
fn field_passes_ng_gate(
    material: &AdvisorMaterial,
    field: &str,
    text: &str,
    ctx: &EmitContext,
    ng: &NgDictionary,
) -> bool {
    match egress_gate(text, ctx, ng) {
        EgressVerdict::Pass => true,
        EgressVerdict::Block { term } | EgressVerdict::Abstain { term } => {
            tracing::warn!(
                route = "advisor_cards",
                material_key = %material.material_key,
                field,
                term = %term,
                "advisor_material card field matched the NG dictionary after plain-text \
                 normalization; dropping this card candidate rather than showing it to the \
                 customer. Fix server/data/homesec/materials.json"
            );
            false
        }
    }
}

/// ソートキー: `own_product` を 0、それ以外(`partner_product` 等)を 1 とする。
/// `Vec::sort_by_key` は安定ソートなので、同じキー内では元の順序が維持される。
fn kind_priority(kind: &str) -> u8 {
    if kind == "own_product" {
        0
    } else {
        1
    }
}

/// design doc §7.2 手順1 の照合判定。`own_product` とそれ以外で判定方法を分ける
/// (本番実害是正: own_product は汎用語で合致させない)。
fn matches_final_text(material: &AdvisorMaterial, final_text: &str) -> bool {
    if material.kind == "own_product" {
        return matches_own_product_by_model_token(material, final_text);
    }
    match &material.card_match_terms {
        Some(terms) => csv_list(terms).iter().any(|term| final_text.contains(term)),
        None => {
            final_text.contains(&material.title_ja)
                || material
                    .product_key
                    .as_deref()
                    .is_some_and(|pk| final_text.contains(pk))
        }
    }
}

/// `own_product` の合致判定(design doc §7.2 手順1「own_product の合致は常に `product_key`
/// の型番明示のみ」)。`card_match_terms` は一切見ない — own_product に汎用語(「防犯カメラ」等)
/// を CSV で持たせても、それだけでは合致しない。
///
/// `product_gate::extract_model_tokens` を「質問側ゲート・材料選別・応答側ゲートの3箇所が
/// 共通で使う唯一の抽出経路」の規律どおりそのまま再利用する(独自の型番正規化を持たない)。
/// `material.product_key`(例 `"ADC-V724"`)自体を通せば正規化済みトークンが1個得られるので、
/// それと `final_text` の型番トークン集合を突き合わせるだけでよい。
fn matches_own_product_by_model_token(material: &AdvisorMaterial, final_text: &str) -> bool {
    let Some(product_key) = material.product_key.as_deref() else {
        tracing::warn!(
            material_key = %material.material_key,
            kind = %material.kind,
            "advisor_material has kind=own_product but no product_key; this material can \
             never be card-matched (own_product matching requires a model token extracted \
             from product_key). Fix server/data/homesec/materials.json"
        );
        return false;
    };
    let Some(expected_token) = extract_model_tokens(product_key).into_iter().next() else {
        tracing::warn!(
            material_key = %material.material_key,
            kind = %material.kind,
            product_key,
            "advisor_material's product_key does not contain a recognizable model token \
             (extract_model_tokens returned none); this own_product material can never be \
             card-matched. Fix server/data/homesec/materials.json"
        );
        return false;
    };
    extract_model_tokens(final_text)
        .iter()
        .any(|token| *token == expected_token)
}

/// [`AdvisorMaterial`] を [`ProductCard`] へ変換する(design doc §7.2 手順5)。
///
/// `title` / `description` は [`to_plain_text`] で正規化する(Issue #47: `card_text_passes_ng_gate`
/// がNG判定に使う正規化と揃え、Markdown 記法がそのまま顧客のカード表示へ漏れないようにする)。
fn to_product_card(material: &AdvisorMaterial, images_dir: &Path) -> ProductCard {
    // `card_description` は候補選定(`select_cards` の filter)で `is_some()` を保証済み。
    let description = material.card_description.clone().unwrap_or_else(|| {
        tracing::warn!(
            material_key = %material.material_key,
            "select_cards produced a card candidate without card_description; this should \
             be unreachable because candidates are filtered by card_description.is_some() \
             upstream. Falling back to an empty description rather than panicking"
        );
        String::new()
    });

    // Issue #47 レビュー指摘F2: title_ja の plain-text 正規化を1箇所(ここ)だけで行い、
    // カード見出しとボタン発話(build_card_buttons)の両方に同じ値を渡す。以前は
    // build_card_buttons が生の material.title_ja を使っていたため、見出しは正規化済みなのに
    // タップ時に顧客の発話として表示されるボタンの message だけ Markdown 記法が残っていた。
    let title = to_plain_text(&material.title_ja);

    ProductCard {
        material_key: material.material_key.clone(),
        description: to_plain_text(&description),
        image_url: resolve_image_url(material, images_dir),
        buttons: build_card_buttons(material, &title),
        title,
    }
}

/// [`ProductCard::buttons`] の組み立て規則(design doc §3.3、要件4)。
///
/// - `product_page_url` があれば、先頭に [`CardButton::Uri`](「商品ページを見る」)を置く。
/// - `kind == "own_product"` の材料は「詳しく聞く」→「導入を相談する」の順で
///   [`CardButton::Message`] を2つ加える。
/// - `kind == "partner_product"` の材料は「選び方を聞く」の [`CardButton::Message`] を1つだけ
///   加える(他社製品は「詳しく聞く」の行き止まりではなく、入手方法・頼み方へ会話を繋げる誘導、
///   本番実害 (c) の是正)。
/// - それ以外の `kind`(このターゲットには `select_cards` の filter により到達しない)は
///   `product_page_url` ボタン以外を追加しない。
///
/// `title`(呼び出し元 [`to_product_card`] が正規化済みの `material.title_ja`)をボタンの
/// メッセージ文言に使う(Issue #47 レビュー指摘F2: カード見出しと同じ正規化済みの値を使うため、
/// ここで生の `material.title_ja` を読み直さない)。
fn build_card_buttons(material: &AdvisorMaterial, title: &str) -> Vec<CardButton> {
    let mut buttons = Vec::with_capacity(3);
    if let Some(url) = material.product_page_url.clone() {
        // Issue #47 Critical指摘2: `AdvisorMaterial::from_attributes` 経由なら既に
        // `is_well_formed_https_url` を通過しているはずだが、テスト等で直接構造体リテラルを
        // 組み立てる経路もあるため、ここでも多層防御として再検証する(検証に落ちたらボタンを
        // 追加しないだけで、カード自体は他のボタンを維持したまま成立させる。
        // `resolve_image_url` の「画像だけ落とす」既存方針と同じ粒度)。
        if is_well_formed_https_url(&url) {
            buttons.push(CardButton::Uri {
                label: PRODUCT_PAGE_BUTTON_LABEL.to_string(),
                url,
            });
        } else {
            tracing::warn!(
                material_key = %material.material_key,
                product_page_url = %url,
                "advisor_material's product_page_url is not a well-formed absolute https:// URL \
                 (defense in depth; this should be unreachable because \
                 AdvisorMaterial::from_attributes already validated it); omitting the uri button \
                 rather than sending an unvetted url to the customer's browser"
            );
        }
    }
    match material.kind.as_str() {
        "own_product" => {
            buttons.push(CardButton::Message {
                label: DETAIL_BUTTON_LABEL.to_string(),
                message: format!("{title}について詳しく教えて"),
            });
            buttons.push(CardButton::Message {
                label: CONSULT_BUTTON_LABEL.to_string(),
                message: format!("{title}の導入を相談したい"),
            });
        }
        "partner_product" => {
            buttons.push(CardButton::Message {
                label: CHOOSING_BUTTON_LABEL.to_string(),
                message: format!("{title}の選び方を教えて"),
            });
        }
        other => {
            tracing::warn!(
                material_key = %material.material_key,
                kind = other,
                "select_cards produced a card candidate whose kind is neither own_product nor \
                 partner_product; this should be unreachable because candidates are filtered \
                 by kind upstream. Producing a card with only the product_page_url button (if \
                 any), matching the pre-Issue-34 no-message-button-for-unknown-kind behavior"
            );
        }
    }
    buttons
}

/// design doc §7.2 手順4「画像は存在するファイルのみ URL 化」の実装。ファイル名の決定規則:
/// - `kind == "own_product"` かつ `product_key` が `Some` → `{product_key を小文字化}.jpg`
/// - `kind == "partner_product"` かつ `category` が `Some` → `{category}.jpg`
///   (`category` は既に `intrusion` / `monitoring` 等の小文字英数字なのでそのまま使える)
/// - それ以外はファイル名なし → `None`
///
/// ファイル名がある場合、まず [`is_safe_filename_component`] で許可文字集合のみで構成されて
/// いるかを検証する(Warning E 是正: `product_key` / `category` は seed データ由来で現状は
/// 信頼できるが、多層防御として検証する。例えば `product_key = "../../etc/passwd"` のような
/// 値が紛れ込むと、検証が無い場合 `image_url` が `/static/products/../../etc/passwd.jpg` に
/// なり、静的配信と組み合わさると任意ファイル読み出しに繋がりうる)。検証を通ったら
/// `images_dir` 上に実在するかを確認し、実在すれば `/static/products/{filename}` を返す
/// (ホスト名は付与しない。分担はモジュール doc 参照)。検証に落ちた場合は画像なしのカードとして
/// 成立させる(warn。カード自体を握りつぶさない — 画像の欠落は致命的ではない)。
fn resolve_image_url(material: &AdvisorMaterial, images_dir: &Path) -> Option<String> {
    let filename = match material.kind.as_str() {
        "own_product" => material
            .product_key
            .as_deref()
            .map(|pk| format!("{}.jpg", pk.to_lowercase())),
        "partner_product" => material.category.as_deref().map(|c| format!("{c}.jpg")),
        _ => None,
    }?;

    if !is_safe_filename_component(&filename) {
        tracing::warn!(
            material_key = %material.material_key,
            filename,
            "advisor_material product_key/category produced an image filename outside the \
             allowed [a-z0-9._-] charset (defense in depth against path traversal from seed \
             data); showing the card without an image. Check server/data/homesec/materials.json"
        );
        return None;
    }

    if !images_dir.join(&filename).exists() {
        return None;
    }

    let url = format!("/static/products/{filename}");
    if is_trusted_static_image_url(&url) {
        Some(url)
    } else {
        // 到達しないはず(直前で is_safe_filename_component を通した filename から組み立てた
        // 固定プレフィックス付き文字列のため)。多層防御として、念のための最終チェックが
        // 落ちた場合も画像なしのカードとして成立させる(warn。カード自体を握りつぶさない)。
        tracing::warn!(
            material_key = %material.material_key,
            url,
            "resolve_image_url built a URL that failed the final self-host static-path check \
             (defense in depth; this should be unreachable because is_safe_filename_component \
             already validated the filename); showing the card without an image"
        );
        None
    }
}

/// [`resolve_image_url`] の最終防御チェック: 組み立てた URL が自ホストの
/// `/static/products/{safe filename}` の形になっているかを再確認する。`is_safe_filename_component`
/// は既に path traversal 等を防いでいるため、これは多層防御的な再確認であり通常は必ず true になる。
fn is_trusted_static_image_url(url: &str) -> bool {
    const PREFIX: &str = "/static/products/";
    url.starts_with(PREFIX) && is_safe_filename_component(&url[PREFIX.len()..])
}

/// [`resolve_image_url`] が組み立てたファイル名が、`[a-z0-9._-]` のみで構成されているかを
/// 検証する。空文字列は拒否する。`/` はこの許可文字集合に含まれないため経路混入(ディレクトリ
/// トラバーサル)は charset チェックだけで塞げるが、`..` の連続はスラッシュ無しでも OS の
/// パス解決規則上は特別扱いされうるため、念のため明示的にも拒否する。
fn is_safe_filename_component(filename: &str) -> bool {
    !filename.is_empty()
        && !filename.contains("..")
        && filename
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// closing = Proposal の [`DraftMeta`] を組み立てるテストヘルパー(2026-08-21
    /// conversation-rhythm-implementation §要件3)。`featured` に渡した material_key だけが
    /// 候補になる。
    fn proposal_meta(featured: &[&str]) -> DraftMeta {
        DraftMeta {
            featured: featured.iter().map(|s| s.to_string()).collect(),
            closing: ClosingKind::Proposal,
            choices: Vec::new(),
        }
    }

    /// NG辞書が空(block_terms/abstain_terms とも空)の `select_cards` 用テストヘルパー
    /// (Issue #47)。既存テストは NG 判定を検証対象にしていないため、この空辞書を渡せば新しい
    /// ゲートは発火せず、既存の期待値・挙動を一切変えない。
    fn permissive_ng() -> NgDictionary {
        NgDictionary::from_json(r#"{"block_terms":[],"abstain_terms":[]}"#).unwrap()
    }

    /// `tempfile` crate は `server/Cargo.toml` に無いため、`std::env::temp_dir()` 配下に
    /// テストごとの一意なサブディレクトリを作って手動で作成・掃除する。`Drop` で確実に
    /// 掃除することで、テスト間の残骸(前回テストが作った画像ファイル)が別テストの
    /// 「画像なし」判定を汚染しないようにする。
    struct TempImagesDir {
        path: std::path::PathBuf,
    }

    impl TempImagesDir {
        fn new(unique_name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "cs-support-mcp-cards-test-{unique_name}-{}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temp images dir");
            Self { path }
        }

        fn touch(&self, filename: &str) {
            fs::write(self.path.join(filename), b"stub").expect("write stub image file");
        }
    }

    impl Drop for TempImagesDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn own_product(material_key: &str, title_ja: &str, product_key: &str) -> AdvisorMaterial {
        AdvisorMaterial {
            material_key: material_key.to_string(),
            kind: "own_product".to_string(),
            title_ja: title_ja.to_string(),
            body_ja: "本文".to_string(),
            source_url: None,
            category: Some("monitoring".to_string()),
            product_key: Some(product_key.to_string()),
            price_band: None,
            card_description: Some("屋外対応・夜間撮影".to_string()),
            // 汎用語("屋外")をあえて含める: own_product はこの CSV を一切見ないことを
            // `own_product_does_not_match_via_a_generic_card_match_term` で固定する。
            card_match_terms: Some(format!("{product_key},屋外")),
            product_page_url: None,
        }
    }

    fn partner_product(material_key: &str, title_ja: &str, category: &str) -> AdvisorMaterial {
        AdvisorMaterial {
            material_key: material_key.to_string(),
            kind: "partner_product".to_string(),
            title_ja: title_ja.to_string(),
            body_ja: "本文".to_string(),
            source_url: Some("https://example.com/partner".to_string()),
            category: Some(category.to_string()),
            product_key: None,
            price_band: None,
            card_description: Some("駆けつけ対応付きサービス".to_string()),
            card_match_terms: Some("ALSOK,駆けつけ".to_string()),
            product_page_url: None,
        }
    }

    // --- own_product の合致判定(design doc §7.2 手順1: 型番明示のみ。本番実害是正) ---

    #[test]
    fn own_product_does_not_match_via_a_generic_card_match_term() {
        // 必須テスト1: card_match_terms に汎用語("屋外")が含まれていても、final_text に
        // 型番("ADC-V724")が無ければ own_product は合致しない。
        let dir = TempImagesDir::new("own-product-generic-term-no-match");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards(
            "屋外でも使える防犯対策をご検討ください",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert!(
            cards.is_empty(),
            "own_product must never match on a generic card_match_terms word alone"
        );
    }

    #[test]
    fn own_product_matches_via_explicit_model_token_in_final_text() {
        // 必須テスト2: final_text に型番が明示されていれば合致する。
        let dir = TempImagesDir::new("own-product-model-token-match");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].material_key, "own_product:adc-v724");
    }

    #[test]
    fn own_product_matches_via_model_token_even_without_card_match_terms() {
        // card_match_terms が無くても型番トークン抽出だけで合致することを固定する
        // (card_match_terms の有無に判定が依存しない)。
        let dir = TempImagesDir::new("own-product-model-token-no-terms");
        let mut m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        m.card_match_terms = None;
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert_eq!(cards.len(), 1);
    }

    #[test]
    fn own_product_does_not_fall_back_to_title_ja_matching() {
        // 旧実装は card_match_terms が無い場合 title_ja 部分一致にフォールバックしていたが、
        // own_product はそのフォールバックを持たない(型番明示のみ)。final_text は title_ja を
        // 含むが型番は含まない。
        let dir = TempImagesDir::new("own-product-no-title-ja-fallback");
        let mut m = own_product("own_product:adc-v724", "屋外カメラ", "ADC-V724");
        m.card_match_terms = None;
        let cards = select_cards(
            "屋外カメラをご検討ください",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert!(
            cards.is_empty(),
            "own_product must not fall back to title_ja matching"
        );
    }

    #[test]
    fn own_product_without_product_key_never_matches() {
        let dir = TempImagesDir::new("own-product-no-product-key");
        let mut m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        m.product_key = None;
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert!(cards.is_empty());
    }

    #[test]
    fn own_product_with_a_non_model_shaped_product_key_never_matches() {
        // reviewer 一次レビュー Warning 4: product_key が型番形式でない(extract_model_tokens
        // が1件もトークンを返さない)場合も、product_key が None の場合と同様に恒久的に
        // カード化されない。final_text に product_key の生文字列を含めても
        // own_product は型番トークン以外では合致しないため、この材料は決して選ばれない。
        let dir = TempImagesDir::new("own-product-non-model-shaped-key");
        let m = own_product("own_product:mystery", "謎の製品", "PLAIN-MODEL-123");
        let cards = select_cards(
            "PLAIN-MODEL-123がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:mystery"])),
            &permissive_ng(),
        );
        assert!(
            cards.is_empty(),
            "a product_key with no extractable model token must never produce a card"
        );
    }

    #[test]
    fn own_product_no_match_produces_no_card() {
        let dir = TempImagesDir::new("own-product-no-match");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards(
            "窓の防犯フィルムが有効です",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert!(cards.is_empty());
    }

    // --- partner_product の合致判定(design doc §7.2 手順1: 従来どおり) ---

    #[test]
    fn partner_product_matches_via_card_match_terms() {
        let dir = TempImagesDir::new("partner-match-terms");
        let m = partner_product(
            "partner_product:alsok",
            "ALSOKホームセキュリティ",
            "intrusion",
        );
        let cards = select_cards(
            "ALSOK駆けつけがおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["partner_product:alsok"])),
            &permissive_ng(),
        );
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].material_key, "partner_product:alsok");
    }

    #[test]
    fn partner_product_matches_via_title_ja_fallback_when_match_terms_is_absent() {
        let dir = TempImagesDir::new("partner-title-fallback");
        let mut m = partner_product(
            "partner_product:alsok",
            "ALSOKホームセキュリティ",
            "intrusion",
        );
        m.card_match_terms = None;
        let cards = select_cards(
            "ALSOKホームセキュリティがおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["partner_product:alsok"])),
            &permissive_ng(),
        );
        assert_eq!(cards.len(), 1);
    }

    #[test]
    fn partner_product_matches_via_product_key_fallback_when_match_terms_is_absent() {
        let dir = TempImagesDir::new("partner-product-key-fallback");
        let mut m = partner_product("partner_product:alsok", "駆けつけサービス", "intrusion");
        m.card_match_terms = None;
        m.product_key = Some("ALSOK-PLAN-A".to_string());
        // final_text は title_ja を含まないが product_key を含む。
        let cards = select_cards(
            "ALSOK-PLAN-Aがおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["partner_product:alsok"])),
            &permissive_ng(),
        );
        assert_eq!(cards.len(), 1);
    }

    #[test]
    fn partner_product_no_match_produces_no_card() {
        let dir = TempImagesDir::new("partner-no-match");
        let m = partner_product(
            "partner_product:alsok",
            "ALSOKホームセキュリティ",
            "intrusion",
        );
        let cards = select_cards(
            "窓の防犯フィルムが有効です",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["partner_product:alsok"])),
            &permissive_ng(),
        );
        assert!(cards.is_empty());
    }

    #[test]
    fn material_without_card_description_is_never_a_candidate_even_if_it_matches() {
        let dir = TempImagesDir::new("no-card-description");
        let mut m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        m.card_description = None;
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert!(cards.is_empty());
    }

    // --- Warning C: 候補は own_product / partner_product に限定する ---

    #[test]
    fn a_statistic_material_with_card_description_never_becomes_a_card() {
        let dir = TempImagesDir::new("kind-statistic-not-a-card");
        let m = AdvisorMaterial {
            material_key: "statistic:mujimari".to_string(),
            kind: "statistic".to_string(),
            title_ja: "無締り統計".to_string(),
            body_ja: "無締りが侵入手口の最多です".to_string(),
            source_url: Some("https://example.com/stat".to_string()),
            category: Some("intrusion".to_string()),
            product_key: None,
            price_band: None,
            // design doc §5.1 の想定外だが、行儀の悪いデータが混入した場合の防御を確認する
            // (design doc §7.2 手順1は候補を own_product / partner_product 限定と定めている)。
            card_description: Some("無締り対策の統計データ".to_string()),
            card_match_terms: Some("無締り".to_string()),
            product_page_url: None,
        };
        let cards = select_cards(
            "無締りにご注意ください",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["statistic:mujimari"])),
            &permissive_ng(),
        );
        assert!(
            cards.is_empty(),
            "kind=statistic must never become a card candidate even with card_description set"
        );
    }

    #[test]
    fn a_scenario_material_with_card_description_never_becomes_a_card() {
        let dir = TempImagesDir::new("kind-scenario-not-a-card");
        let m = AdvisorMaterial {
            material_key: "scenario:rental-single".to_string(),
            kind: "scenario".to_string(),
            title_ja: "賃貸一人暮らし".to_string(),
            body_ja: "賃貸一人暮らしのシナリオです".to_string(),
            source_url: None,
            category: None,
            product_key: None,
            price_band: None,
            card_description: Some("賃貸一人暮らし向けの案内".to_string()),
            card_match_terms: Some("賃貸一人暮らし".to_string()),
            product_page_url: None,
        };
        let cards = select_cards(
            "賃貸一人暮らしの防犯対策です",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["scenario:rental-single"])),
            &permissive_ng(),
        );
        assert!(
            cards.is_empty(),
            "kind=scenario must never become a card candidate even with card_description set"
        );
    }

    // --- own 優先・3件上限 ---

    #[test]
    fn own_product_is_ordered_before_partner_product_when_both_match() {
        let dir = TempImagesDir::new("own-before-partner");
        let partner = partner_product(
            "partner_product:alsok",
            "ALSOKホームセキュリティ",
            "intrusion",
        );
        let own = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        // injected の並び順はあえて partner を先に置く(順序ではなく kind で並び替わることの確認)。
        let cards = select_cards(
            "ADC-V724とALSOK駆けつけの両方が候補です",
            &[partner, own],
            "",
            &dir.path,
            Some(&proposal_meta(&[
                "own_product:adc-v724",
                "partner_product:alsok",
            ])),
            &permissive_ng(),
        );
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].material_key, "own_product:adc-v724");
        assert_eq!(cards[1].material_key, "partner_product:alsok");
    }

    #[test]
    fn more_than_three_matches_are_truncated_to_three_preserving_own_first_order() {
        let dir = TempImagesDir::new("truncate-to-three");
        let mut partner_a = partner_product("partner_product:a", "A社サービス", "intrusion");
        partner_a.card_match_terms = None; // title_ja フォールバックで照合させる
        let mut partner_b = partner_product("partner_product:b", "B社サービス", "monitoring");
        partner_b.card_match_terms = None;
        let materials = vec![
            partner_a,
            own_product("own_product:adc-v523", "URTECT ADC-V523", "ADC-V523"),
            partner_b,
            own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724"),
        ];
        let final_text = "ADC-V523 ADC-V724 A社サービス B社サービス が候補です";
        let meta = proposal_meta(&[
            "partner_product:a",
            "own_product:adc-v523",
            "partner_product:b",
            "own_product:adc-v724",
        ]);
        let cards = select_cards(
            final_text,
            &materials,
            "",
            &dir.path,
            Some(&meta),
            &permissive_ng(),
        );
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].material_key, "own_product:adc-v523");
        assert_eq!(cards[1].material_key, "own_product:adc-v724");
        assert_eq!(cards[2].material_key, "partner_product:a");
    }

    // --- 再表示抑止 ---

    #[test]
    fn a_material_key_already_in_shown_csv_is_not_shown_again() {
        let dir = TempImagesDir::new("shown-suppressed");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "own_product:adc-v724",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert!(cards.is_empty());
    }

    // --- 画像 URL 化 ---

    #[test]
    fn own_product_image_url_is_some_when_the_file_exists() {
        let dir = TempImagesDir::new("image-exists-own");
        dir.touch("adc-v724.jpg");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert_eq!(
            cards[0].image_url.as_deref(),
            Some("/static/products/adc-v724.jpg")
        );
    }

    #[test]
    fn image_url_is_none_when_the_file_does_not_exist_but_the_card_still_forms() {
        let dir = TempImagesDir::new("image-missing");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert_eq!(cards.len(), 1, "card must still form without an image");
        assert_eq!(cards[0].image_url, None);
    }

    #[test]
    fn image_url_is_none_when_product_key_attempts_path_traversal() {
        // Warning E: product_key はシード投入由来で現状は信頼できるが、多層防御として
        // `resolve_image_url` は正規化前の生の `product_key` を使う(合致判定側の
        // `extract_model_tokens` は正規化済みトークンしか見ない)。型番トークン抽出を通過
        // しつつ経路混入を狙う値("ADC-V724/../../etc/passwd" のように型番の後ろへ経路混入
        // 文字列を続けた値)が紛れ込んでも `image_url` に反映されないことを固定する。
        let dir = TempImagesDir::new("path-traversal-product-key");
        let m = own_product(
            "own_product:evil",
            "怪しい製品",
            "ADC-V724/../../etc/passwd",
        );
        let final_text = "ADC-V724がおすすめです";
        let cards = select_cards(
            final_text,
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:evil"])),
            &permissive_ng(),
        );
        assert_eq!(
            cards.len(),
            1,
            "the card must still form without an image: {cards:?}"
        );
        assert_eq!(
            cards[0].image_url, None,
            "a path-traversal-shaped product_key must never produce an image_url"
        );
    }

    #[test]
    fn partner_product_image_url_uses_the_category_filename_when_the_file_exists() {
        let dir = TempImagesDir::new("image-exists-partner");
        dir.touch("intrusion.jpg");
        let m = partner_product(
            "partner_product:alsok",
            "ALSOKホームセキュリティ",
            "intrusion",
        );
        let cards = select_cards(
            "ALSOK駆けつけがおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["partner_product:alsok"])),
            &permissive_ng(),
        );
        assert_eq!(
            cards[0].image_url.as_deref(),
            Some("/static/products/intrusion.jpg")
        );
    }

    // --- カード内容 ---

    #[test]
    fn card_fields_are_built_from_the_material() {
        let dir = TempImagesDir::new("card-fields");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        let card = &cards[0];
        assert_eq!(card.title, "URTECT ADC-V724");
        assert_eq!(card.description, "屋外対応・夜間撮影");
    }

    // --- buttons 組み立て(design doc §3.3・要件4、必須テスト4) ---

    #[test]
    fn own_product_card_has_two_message_buttons_in_order() {
        let dir = TempImagesDir::new("buttons-own-product");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert_eq!(
            cards[0].buttons,
            vec![
                CardButton::Message {
                    label: "詳しく聞く".to_string(),
                    message: "URTECT ADC-V724について詳しく教えて".to_string(),
                },
                CardButton::Message {
                    label: "導入を相談する".to_string(),
                    message: "URTECT ADC-V724の導入を相談したい".to_string(),
                },
            ]
        );
    }

    #[test]
    fn partner_product_card_has_exactly_one_message_button() {
        let dir = TempImagesDir::new("buttons-partner-product");
        let m = partner_product(
            "partner_product:alsok",
            "ALSOKホームセキュリティ",
            "intrusion",
        );
        let cards = select_cards(
            "ALSOK駆けつけがおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["partner_product:alsok"])),
            &permissive_ng(),
        );
        assert_eq!(
            cards[0].buttons,
            vec![CardButton::Message {
                label: "選び方を聞く".to_string(),
                message: "ALSOKホームセキュリティの選び方を教えて".to_string(),
            }]
        );
    }

    #[test]
    fn product_page_url_present_prepends_a_uri_button_before_the_message_buttons() {
        let dir = TempImagesDir::new("buttons-product-page-url-present");
        let mut m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        m.product_page_url = Some("https://example.com/products/adc-v724".to_string());
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert_eq!(
            cards[0].buttons,
            vec![
                CardButton::Uri {
                    label: "商品ページを見る".to_string(),
                    url: "https://example.com/products/adc-v724".to_string(),
                },
                CardButton::Message {
                    label: "詳しく聞く".to_string(),
                    message: "URTECT ADC-V724について詳しく教えて".to_string(),
                },
                CardButton::Message {
                    label: "導入を相談する".to_string(),
                    message: "URTECT ADC-V724の導入を相談したい".to_string(),
                },
            ]
        );
    }

    #[test]
    fn product_page_url_absent_omits_the_uri_button() {
        let dir = TempImagesDir::new("buttons-product-page-url-absent");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert!(
            cards[0]
                .buttons
                .iter()
                .all(|b| !matches!(b, CardButton::Uri { .. })),
            "no product_page_url means no uri button: {:?}",
            cards[0].buttons
        );
    }

    // --- 必須テスト3: カードが1件のみ合致する場合でも非空の Vec が返ること
    // (api.rs 側の `Option<Vec<ProductCard>>` への wrap は `selected_cards.is_empty()` の
    // 単純な if/else であり、この Vec が非空である限り `Some(vec![1件])` になる) ---

    #[test]
    fn select_cards_returns_exactly_one_card_when_only_one_material_matches() {
        let dir = TempImagesDir::new("single-match");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert_eq!(
            cards.len(),
            1,
            "a single matching material must yield a non-empty Vec of exactly one card, so \
             api.rs's is_empty()-guarded Option wrap yields Some(vec![1 card]), not None"
        );
    }

    // --- 必須テスト3: closing 別の出し分け(カード側)。2026-08-21
    // conversation-rhythm-implementation §要件3: closing != Proposal(meta が None の場合を
    // 含む)ならカードは一切出さない。本番実害 (a):「特に心配な場所は?」と質問で締めた
    // ターンに商品カードが付き会話が噛み合わない、への是正。 ---

    #[test]
    fn no_cards_when_meta_is_none_even_if_the_material_would_otherwise_match() {
        let dir = TempImagesDir::new("meta-none-no-cards");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            None,
            &permissive_ng(),
        );
        assert!(
            cards.is_empty(),
            "meta = None must suppress cards even when the material would otherwise match"
        );
    }

    #[test]
    fn no_cards_when_closing_is_question_choice() {
        let dir = TempImagesDir::new("closing-question-choice-no-cards");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let meta = DraftMeta {
            featured: vec!["own_product:adc-v724".to_string()],
            closing: ClosingKind::QuestionChoice,
            choices: vec!["侵入が心配".to_string()],
        };
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&meta),
            &permissive_ng(),
        );
        assert!(
            cards.is_empty(),
            "closing = QuestionChoice must suppress cards even when featured lists the material"
        );
    }

    #[test]
    fn no_cards_when_closing_is_question_open() {
        let dir = TempImagesDir::new("closing-question-open-no-cards");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let meta = DraftMeta {
            featured: vec!["own_product:adc-v724".to_string()],
            closing: ClosingKind::QuestionOpen,
            choices: Vec::new(),
        };
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&meta),
            &permissive_ng(),
        );
        assert!(
            cards.is_empty(),
            "closing = QuestionOpen must suppress cards even when featured lists the material"
        );
    }

    #[test]
    fn cards_are_shown_when_closing_is_proposal() {
        // proposal_meta の positive path は上の各テストで既に固定済みだが、closing の
        // 出し分けを1箇所にまとめて確認する回帰テスト。
        let dir = TempImagesDir::new("closing-proposal-shows-cards");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert_eq!(cards.len(), 1);
    }

    // --- 必須テスト2: featured 検証。2026-08-21 conversation-rhythm-implementation §要件2
    // (要件3 実装箇所での検証)。 ---

    #[test]
    fn a_featured_material_key_absent_from_injected_materials_is_silently_discarded() {
        // meta.featured が注入材料に存在しない material_key を挙げていても、injected の
        // 中に実在する候補(このケースでは無し)しかカードにならない。捏造しない。
        //
        // reviewer 一次レビュー Major 3 是正: 戻り値が空になる判定ロジック自体はこのテストの
        // 名前どおり変わっていないが、この経路は `tracing::warn!`(route="advisor_cards")を
        // 出すようになった(運用者が「なぜカードが出なかったか」を判断できるようにするため)。
        // 呼び出し側から見える戻り値は変わらないため、ここでは warn 出力自体はアサートしない。
        let dir = TempImagesDir::new("featured-key-not-in-injected");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:does-not-exist"])),
            &permissive_ng(),
        );
        assert!(
            cards.is_empty(),
            "a featured material_key with no matching injected material must not produce a card, \
             and must not cause the (unrelated, non-featured) injected material to be shown either"
        );
    }

    #[test]
    fn an_injected_material_key_not_listed_in_featured_is_discarded_even_if_mentioned_in_text() {
        // injected に実在し、final_text でも言及されているが、meta.featured に挙げられて
        // いない material_key はカードにならない(このターンの主役として提案していない、
        // 本番実害 (b) の是正)。
        let dir = TempImagesDir::new("injected-key-not-in-featured");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let meta = proposal_meta(&["own_product:adc-v523"]); // 別の material_key だけを featured にする
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&meta),
            &permissive_ng(),
        );
        assert!(
            cards.is_empty(),
            "an injected material not listed in meta.featured must be discarded even when the \
             final text mentions it"
        );
    }

    // --- Issue #47 Critical指摘2: カードの title_ja / card_description も出口関門を通す ---

    #[test]
    fn card_is_dropped_and_warns_when_title_ja_matches_a_block_term() {
        // own_product の合致判定は product_key の型番トークンのみを見る(matches_final_text)
        // ため、title_ja の中身は合致判定に影響しない。ここでは合致は満たしつつ、
        // title_ja に NG block_term を仕込んでカードだけが落ちることを確認する。
        let dir = TempImagesDir::new("ng-gate-title-block-term");
        let m = own_product("own_product:adc-v724", "絶対に安全なカメラ", "ADC-V724");
        let ng = NgDictionary::from_json(r#"{"block_terms":["絶対に安全"],"abstain_terms":[]}"#)
            .expect("ng dictionary parses");
        let (cards, logs) = crate::test_support::capture_logs(|| {
            select_cards(
                "ADC-V724がおすすめです",
                &[m],
                "",
                &dir.path,
                Some(&proposal_meta(&["own_product:adc-v724"])),
                &ng,
            )
        });
        assert!(
            cards.is_empty(),
            "a card whose title_ja matches an NG block_term must never be shown"
        );
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn card_is_dropped_and_warns_when_card_description_matches_an_abstain_term() {
        let dir = TempImagesDir::new("ng-gate-description-abstain-term");
        let mut m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        m.card_description = Some("これで必ず治ると言われています".to_string());
        let ng = NgDictionary::from_json(r#"{"block_terms":[],"abstain_terms":["治る"]}"#)
            .expect("ng dictionary parses");
        let (cards, logs) = crate::test_support::capture_logs(|| {
            select_cards(
                "ADC-V724がおすすめです",
                &[m],
                "",
                &dir.path,
                Some(&proposal_meta(&["own_product:adc-v724"])),
                &ng,
            )
        });
        assert!(
            cards.is_empty(),
            "a card whose card_description matches an NG abstain_term must never be shown"
        );
        assert!(logs.contains("WARN"), "logs: {logs}");
    }

    #[test]
    fn card_survives_ng_gate_when_the_term_only_spans_the_title_description_boundary() {
        // Issue #47 レビュー指摘F4: card_text_passes_ng_gate は以前 title と description を
        // `\n` で連結してから1回だけ egress_gate に通していたが、normalize_key は英数字以外を
        // すべて捨てる(`\n` も消える)ため、正規化後は「title の末尾 + description の先頭」が
        // 連結されて NG 語に一致しうる(偽陽性)。ここでは title 単体にも description 単体にも
        // NG block_term が含まれないが、連結すると一致してしまう組み合わせを使う。
        let dir = TempImagesDir::new("ng-gate-title-description-boundary");
        let mut m = own_product("own_product:adc-v724", "とても絶対に", "ADC-V724");
        m.card_description = Some("安全な製品です".to_string());
        let ng = NgDictionary::from_json(r#"{"block_terms":["絶対に安全"],"abstain_terms":[]}"#)
            .expect("ng dictionary parses");
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &ng,
        );
        assert_eq!(
            cards.len(),
            1,
            "neither title_ja nor card_description alone matches the NG term, so the card must \
             not be dropped just because concatenating them happens to match"
        );
    }

    #[test]
    fn card_title_and_description_strip_markdown_via_plain_text_normalization() {
        let dir = TempImagesDir::new("markdown-normalization");
        let mut m = own_product("own_product:adc-v724", "**URTECT ADC-V724**", "ADC-V724");
        m.card_description = Some("**屋外対応**・夜間撮影".to_string());
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert_eq!(cards.len(), 1);
        assert!(
            !cards[0].title.contains("**"),
            "title must not retain markdown bold markers: {:?}",
            cards[0].title
        );
        assert!(
            !cards[0].description.contains("**"),
            "description must not retain markdown bold markers: {:?}",
            cards[0].description
        );
    }

    #[test]
    fn card_button_messages_use_the_same_plain_text_normalized_title_as_the_card_heading() {
        // Issue #47 レビュー指摘F2: build_card_buttons が生の material.title_ja を使うと、
        // カード見出し(title)は to_plain_text で正規化されるのに、タップ時に顧客の発話として
        // チャットへ表示される CardButton::Message.message だけ Markdown 記法が残っていた。
        let dir = TempImagesDir::new("button-message-markdown-normalization");
        let m = own_product("own_product:adc-v724", "**URTECT ADC-V724**", "ADC-V724");
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert_eq!(cards.len(), 1);
        for button in &cards[0].buttons {
            if let CardButton::Message { message, .. } = button {
                assert!(
                    !message.contains("**"),
                    "button message must not retain markdown bold markers: {message:?}"
                );
            }
        }
    }

    #[test]
    fn product_page_url_button_is_omitted_when_the_material_carries_a_not_well_formed_url() {
        // AdvisorMaterial::from_attributes は不正な product_page_url を持つ材料自体を除外する
        // (Issue #47 修正(a))が、この材料は from_attributes を経由せず直接構造体リテラルで
        // 組み立てているため、その入口検証を素通りしている。cards.rs 自身の多層防御
        // (build_card_buttons の再検証)を単体で確認する
        // (image_url_is_none_when_product_key_attempts_path_traversal と同じ「多層防御を直接
        // 構造体リテラルで検証する」パターン)。
        let dir = TempImagesDir::new("product-page-url-not-well-formed");
        let mut m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        m.product_page_url = Some("http://example.com/products/adc-v724".to_string());
        let cards = select_cards(
            "ADC-V724がおすすめです",
            &[m],
            "",
            &dir.path,
            Some(&proposal_meta(&["own_product:adc-v724"])),
            &permissive_ng(),
        );
        assert_eq!(
            cards.len(),
            1,
            "the card must still form without the uri button"
        );
        assert!(
            cards[0]
                .buttons
                .iter()
                .all(|b| !matches!(b, CardButton::Uri { .. })),
            "a not-well-formed product_page_url must never produce a uri button: {:?}",
            cards[0].buttons
        );
        assert!(
            cards[0]
                .buttons
                .iter()
                .any(|b| matches!(b, CardButton::Message { .. })),
            "other buttons must still be present: {:?}",
            cards[0].buttons
        );
    }
}
