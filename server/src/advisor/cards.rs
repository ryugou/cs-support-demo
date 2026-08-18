//! 製品カードの添付判定(design doc `2026-08-17-homesec-advisor-design.md` §7.2)。
//!
//! **分担の境界**: `images_dir` はファイルシステム上の実ディレクトリ(存在確認用)であり、
//! URL ではない。返す [`ProductCard::image_url`] は**ホストを含まない相対パス**
//! (例 `/static/products/adc-v724.jpg`)にする。ホスト名を付与して完全 URL にするのは
//! Task 6(ハンドラ、`CS_SUPPORT_PUBLIC_DOMAIN` を知っている層)の責務であり、ここでは
//! 行わない。これにより [`select_cards`] は純粋・決定論のまま保たれ、tempdir を使った
//! ユニットテストだけで検証できる。

use crate::advisor::materials::AdvisorMaterial;
use crate::harness::knowledge::csv_list;
use std::path::Path;

/// design doc §3.3 の `product_cards` 1 件。
#[derive(Debug, Clone, PartialEq)]
pub struct ProductCard {
    pub material_key: String,
    pub title: String,
    pub description: String,
    pub image_url: Option<String>,
    pub button_text: String,
    pub button_message: String,
}

/// カルーセルのボタン文言(固定。dialogue-examples のカルーセル注記と一致させる)。
const BUTTON_TEXT: &str = "この製品について聞く";

/// design doc §7.2 の製品カード添付判定本体。
///
/// 手順:
/// 1. `injected` のうち `kind` が `own_product` / `partner_product`、かつ
///    `card_description.is_some()` の材料だけを候補にする(design doc §7.2 手順1は候補を
///    「own_product / partner_product 材料(`card_description` を持つもの)」と定めている。
///    `kind` 側の絞り込みが無いと `statistic` / `scenario` 材料でも `card_description` さえ
///    あればカード化されてしまい、`kind_priority` の「own_product 以外は一律 partner 相当」
///    という前提とも食い違う。`known_resolution_to_material` により `card_description = None`
///    の KR 由来材料は元々ここで除外される)。
/// 2. `final_text`(出口関門を通過した最終応答文、正規化後)に照合語が含まれるかを判定する:
///    `card_match_terms` があればその CSV 分割語のいずれか、無ければ `title_ja` または
///    `product_key` のいずれか(design doc §5.1「省略時は title_ja と product_key で照合する」)。
/// 3. `shown_csv`(この会話で既に表示済みの material_key の CSV)に含まれる候補を除外する
///    (再表示抑止)。
/// 4. `kind == "own_product"` を先に、`"partner_product"` をその後に安定ソートし、
///    先頭から最大3件を採用する。
/// 5. 採用した候補を [`ProductCard`] に変換する(画像は `images_dir` 上に実在するファイル
///    だけを URL 化する)。
pub fn select_cards(
    final_text: &str,
    injected: &[AdvisorMaterial],
    shown_csv: &str,
    images_dir: &Path,
) -> Vec<ProductCard> {
    let shown: std::collections::HashSet<String> = csv_list(shown_csv).into_iter().collect();

    let mut candidates: Vec<&AdvisorMaterial> = injected
        .iter()
        .filter(|m| matches!(m.kind.as_str(), "own_product" | "partner_product"))
        .filter(|m| m.card_description.is_some())
        .filter(|m| matches_final_text(m, final_text))
        .filter(|m| !shown.contains(&m.material_key))
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

/// ソートキー: `own_product` を 0、それ以外(`partner_product` 等)を 1 とする。
/// `Vec::sort_by_key` は安定ソートなので、同じキー内では元の順序が維持される。
fn kind_priority(kind: &str) -> u8 {
    if kind == "own_product" {
        0
    } else {
        1
    }
}

/// design doc §7.2 手順1 の照合判定。`card_match_terms` があればその CSV 分割語のいずれか、
/// 無ければ `title_ja` または `product_key` のいずれかが `final_text` に部分文字列として
/// 含まれるかを見る。
fn matches_final_text(material: &AdvisorMaterial, final_text: &str) -> bool {
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

/// [`AdvisorMaterial`] を [`ProductCard`] へ変換する(design doc §7.2 手順5)。
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

    ProductCard {
        material_key: material.material_key.clone(),
        title: material.title_ja.clone(),
        description,
        image_url: resolve_image_url(material, images_dir),
        button_text: BUTTON_TEXT.to_string(),
        button_message: format!("{}について詳しく教えて", material.title_ja),
    }
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

    if images_dir.join(&filename).exists() {
        Some(format!("/static/products/{filename}"))
    } else {
        None
    }
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
            card_match_terms: Some(format!("{product_key},屋外")),
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
        }
    }

    // --- 合致判定 ---

    #[test]
    fn matching_via_card_match_terms_produces_a_card() {
        let dir = TempImagesDir::new("match-terms");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards("ADC-V724がおすすめです", &[m], "", &dir.path);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].material_key, "own_product:adc-v724");
    }

    #[test]
    fn matching_via_title_ja_fallback_when_match_terms_is_absent() {
        let dir = TempImagesDir::new("title-fallback");
        let mut m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        m.card_match_terms = None;
        let cards = select_cards(
            "URTECT ADC-V724のカメラをご検討ください",
            &[m],
            "",
            &dir.path,
        );
        assert_eq!(cards.len(), 1);
    }

    #[test]
    fn matching_via_product_key_fallback_when_match_terms_is_absent() {
        let dir = TempImagesDir::new("product-key-fallback");
        let mut m = own_product("own_product:adc-v724", "屋外カメラ", "ADC-V724");
        m.card_match_terms = None;
        // final_text は title_ja を含まないが product_key を含む。
        let cards = select_cards("ADC-V724がおすすめです", &[m], "", &dir.path);
        assert_eq!(cards.len(), 1);
    }

    #[test]
    fn no_match_produces_no_card() {
        let dir = TempImagesDir::new("no-match");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards("窓の防犯フィルムが有効です", &[m], "", &dir.path);
        assert!(cards.is_empty());
    }

    #[test]
    fn material_without_card_description_is_never_a_candidate_even_if_it_matches() {
        let dir = TempImagesDir::new("no-card-description");
        let mut m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        m.card_description = None;
        let cards = select_cards("ADC-V724がおすすめです", &[m], "", &dir.path);
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
        };
        let cards = select_cards("無締りにご注意ください", &[m], "", &dir.path);
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
        };
        let cards = select_cards("賃貸一人暮らしの防犯対策です", &[m], "", &dir.path);
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
        let cards = select_cards(final_text, &materials, "", &dir.path);
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
        );
        assert!(cards.is_empty());
    }

    // --- 画像 URL 化 ---

    #[test]
    fn own_product_image_url_is_some_when_the_file_exists() {
        let dir = TempImagesDir::new("image-exists-own");
        dir.touch("adc-v724.jpg");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards("ADC-V724がおすすめです", &[m], "", &dir.path);
        assert_eq!(
            cards[0].image_url.as_deref(),
            Some("/static/products/adc-v724.jpg")
        );
    }

    #[test]
    fn image_url_is_none_when_the_file_does_not_exist_but_the_card_still_forms() {
        let dir = TempImagesDir::new("image-missing");
        let m = own_product("own_product:adc-v724", "URTECT ADC-V724", "ADC-V724");
        let cards = select_cards("ADC-V724がおすすめです", &[m], "", &dir.path);
        assert_eq!(cards.len(), 1, "card must still form without an image");
        assert_eq!(cards[0].image_url, None);
    }

    #[test]
    fn image_url_is_none_when_product_key_attempts_path_traversal() {
        // Warning E: product_key はシード投入由来で現状は信頼できるが、多層防御として
        // `../../etc/passwd` のような値が紛れ込んでも `image_url` に反映されないことを固定する。
        let dir = TempImagesDir::new("path-traversal-product-key");
        let m = own_product("own_product:evil", "怪しい製品", "../../etc/passwd");
        // card_match_terms は own_product() ヘルパーが product_key ベースで組むため、
        // 素の product_key 文字列を final_text にそのまま含めれば合致判定は通る。
        let final_text = "../../etc/passwdがおすすめです";
        let cards = select_cards(final_text, &[m], "", &dir.path);
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
        let cards = select_cards("ALSOK駆けつけがおすすめです", &[m], "", &dir.path);
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
        let cards = select_cards("ADC-V724がおすすめです", &[m], "", &dir.path);
        let card = &cards[0];
        assert_eq!(card.title, "URTECT ADC-V724");
        assert_eq!(card.description, "屋外対応・夜間撮影");
        assert_eq!(card.button_text, "この製品について聞く");
        assert_eq!(card.button_message, "URTECT ADC-V724について詳しく教えて");
    }
}
