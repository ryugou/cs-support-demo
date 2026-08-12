//! マニュアルクローラ共通ヘルパ。
//!
//! `ingest_urtect`（Google Sites）と `ingest_alarmcom`（answers.alarm.com / MindTouch）の
//! 両方が使う、URL 構造・サイト構造に依存しない純関数を集約する。サイト固有の
//! セレクタ選択（urtect は `<main>`、alarmcom は `#elm-main-content`）やコンテナ選択は
//! 呼び出し側に残し、ここには「選ばれたコンテナからノイズを除外してテキストを集める」
//! ような共通ロジックだけを置く（コピペ増殖を避けつつ、意味論の混線も避ける）。

use crate::proto::graphrag::NodeResult;
use std::collections::{HashMap, HashSet};

/// 空白（改行・タブ・全角スペース含む）を単一の半角スペースへ正規化する純関数。
pub fn normalize_body(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 与えられたコンテナ要素の配下から本文テキストを収集する純関数。
///
/// `script`/`style`/`noscript`/`nav`/`header`/`footer` 配下のテキストは本文でないため
/// 除外する（`.text()` をそのまま使うと inline JS/CSS を拾い、Google Sites では本文が
/// JS で汚染される）。テキストノードは空白区切りで連結して返す（空白正規化は行わない。
/// `normalize_body` の責務）。
///
/// コンテナの選択（CSS セレクタ）は呼び出し側が行う: urtect は `<main>`、alarmcom は
/// `#elm-main-content` と異なるため、ここでは選択済みの `ElementRef` を受け取る。
pub fn extract_text_excluding_noise(container: scraper::ElementRef) -> String {
    let mut raw = String::new();
    for node in container.descendants() {
        let scraper::Node::Text(text) = node.value() else {
            continue;
        };
        let under_noise = node.ancestors().any(|anc| {
            matches!(anc.value(), scraper::Node::Element(el)
                if matches!(el.name(), "script" | "style" | "noscript" | "nav" | "header" | "footer"))
        });
        if !under_noise {
            let chunk: &str = text;
            raw.push_str(chunk);
            raw.push(' ');
        }
    }
    raw
}

/// needle が haystack 中に「英数字境界で」出現するか（前後が英数字でない位置のみ一致とみなす）。
/// "ADC-V724" が "ADC-V724X" の内部に前方一致してしまう誤爆を防ぐために使う。
pub fn contains_as_token(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack.match_indices(needle).any(|(pos, _)| {
        let end = pos + needle.len();
        let before_ok = haystack[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        before_ok && after_ok
    })
}

/// vegapunk から取得した Product ノード一覧を検出語彙（表層形 → product_key）に変換する。
/// 表層形は `model` 属性自身と `aliases` 属性（カンマ区切り・trim・空要素除去）の両方を含み、
/// 大文字化して保持する（detect_product_models 側は body を大文字化するだけで比較できる）。
/// `model` 属性が空/欠落の Product ノードは検出語彙に加えない（fail closed にはしない。
/// ingest_products 側の投入時点バリデーションで既に弾かれているはずだが、直接 vegapunk に
/// 投入された不正データで crawl 全体を止めないための防御的スキップ）。
pub fn build_product_lexicon(products: &[NodeResult]) -> HashMap<String, String> {
    let mut lexicon = HashMap::new();
    for product in products {
        let model = match product.attributes.get("model") {
            Some(m) if !m.trim().is_empty() => m.clone(),
            _ => continue,
        };
        lexicon.insert(model.to_uppercase(), model.clone());
        if let Some(aliases) = product.attributes.get("aliases") {
            for alias in aliases.split(',') {
                let trimmed = alias.trim();
                if !trimmed.is_empty() {
                    lexicon.insert(trimmed.to_uppercase(), model.clone());
                }
            }
        }
    }
    lexicon
}

/// body に出現する型番を検出する（DESCRIBES 辺のもとになる）。`lexicon` は
/// `build_product_lexicon` が組み立てた表層形（大文字）→ product_key の対応表。
/// 同一 product が model と alias の両方でヒットしても 1 回だけ返す。戻り値の順序は
/// product_key の文字列昇順で決定論的にする（差分 ingest のハッシュ計算に混ぜるため
/// 安定した順序が必須）。
pub fn detect_product_models(body: &str, lexicon: &HashMap<String, String>) -> Vec<String> {
    let upper = body.to_uppercase();
    let mut hit_keys: HashSet<String> = HashSet::new();
    for (surface_form, product_key) in lexicon {
        if contains_as_token(&upper, surface_form) {
            hit_keys.insert(product_key.clone());
        }
    }
    let mut result: Vec<String> = hit_keys.into_iter().collect();
    result.sort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::{Html, Selector};

    #[test]
    fn normalize_body_squeezes_all_whitespace_to_single_spaces() {
        // 改行・タブ・全角スペース・連続空白がすべて単一の半角スペースに畳まれる。
        let normalized = normalize_body("  a\n\tb\u{3000}c   d  ");
        assert_eq!(normalized, "a b c d");
    }

    #[test]
    fn extract_text_excludes_script_style_and_structural_noise() {
        // 選択済みコンテナ配下から本文テキストのみ集め、script/style/nav/header/footer を除外する。
        let html = r#"<html><body><main>
            <nav>メニュー</nav>
            <header>ヘッダ</header>
            <h1>タイトル</h1>
            <p>本文テキスト</p>
            <style>.x{color:red}</style>
            <script>var noise = 42;</script>
            <footer>フッタ</footer>
        </main></body></html>"#;
        let document = Html::parse_document(html);
        let selector = Selector::parse("main").expect("valid selector");
        let container = document.select(&selector).next().expect("main present");
        let text = normalize_body(&extract_text_excluding_noise(container));
        assert!(text.contains("タイトル"));
        assert!(text.contains("本文テキスト"));
        assert!(!text.contains("メニュー"));
        assert!(!text.contains("ヘッダ"));
        assert!(!text.contains("フッタ"));
        assert!(!text.contains("noise"));
        assert!(!text.contains("color:red"));
    }

    /// テスト用の型番検出語彙: ADC-V724 / ADC-V724X / ADC-VC727P の 3 型番を、
    /// それぞれ自分自身を表層形として登録する（現行 products.json の seed と同じ形）。
    fn sample_lexicon() -> HashMap<String, String> {
        let mut lexicon = HashMap::new();
        lexicon.insert("ADC-V724".to_string(), "ADC-V724".to_string());
        lexicon.insert("ADC-V724X".to_string(), "ADC-V724X".to_string());
        lexicon.insert("ADC-VC727P".to_string(), "ADC-VC727P".to_string());
        lexicon
    }

    #[test]
    fn detects_model_without_false_positive_on_prefix() {
        // "ADC-V724X" は "ADC-V724" の前方一致だが、V724 単体としては誤検出しない。
        let models = detect_product_models("この設定は ADC-V724X 専用です。", &sample_lexicon());
        assert_eq!(models, vec!["ADC-V724X".to_string()]);
    }

    #[test]
    fn detects_multiple_models_when_both_mentioned() {
        let models = detect_product_models(
            "ADC-V724 と ADC-V724X の両方に対応します。",
            &sample_lexicon(),
        );
        assert_eq!(
            models,
            vec!["ADC-V724".to_string(), "ADC-V724X".to_string()]
        );
    }

    #[test]
    fn detects_model_directly_via_exact_match() {
        let models = detect_product_models("ADC-VC727P の設定手順です。", &sample_lexicon());
        assert_eq!(models, vec!["ADC-VC727P".to_string()]);
    }

    #[test]
    fn resolves_alias_hit_to_product_key() {
        // alias は product_key（= model）と異なる表層形になり得る。ヒットは alias の
        // 文字列ではなく product_key（対応表の値）で返す。
        let mut lexicon = HashMap::new();
        lexicon.insert("ADC-V724".to_string(), "ADC-V724".to_string());
        lexicon.insert("V724 PRO".to_string(), "ADC-V724".to_string());
        let models = detect_product_models("V724 PRO の設定について。", &lexicon);
        assert_eq!(models, vec!["ADC-V724".to_string()]);
    }

    #[test]
    fn dedupes_when_model_and_alias_both_hit_same_product() {
        // 本文中に model と alias の両方が出現しても、同一 product は 1 回だけ返す。
        let mut lexicon = HashMap::new();
        lexicon.insert("ADC-V724".to_string(), "ADC-V724".to_string());
        lexicon.insert("V724 PRO".to_string(), "ADC-V724".to_string());
        let models = detect_product_models("ADC-V724（別名 V724 PRO）です。", &lexicon);
        assert_eq!(models, vec!["ADC-V724".to_string()]);
    }

    #[test]
    fn detection_is_case_insensitive() {
        // body 側だけ大文字化して比較するため、小文字表記の本文でもヒットする。
        let models = detect_product_models("adc-v724 は防水です。", &sample_lexicon());
        assert_eq!(models, vec!["ADC-V724".to_string()]);
    }

    fn node_result(model: &str, aliases: &str) -> NodeResult {
        let mut attributes = HashMap::new();
        attributes.insert("model".to_string(), model.to_string());
        attributes.insert("aliases".to_string(), aliases.to_string());
        NodeResult {
            node_id: format!("urtect:gen1:Product:{model}"),
            node_type: "Product".to_string(),
            attributes,
        }
    }

    #[test]
    fn lexicon_includes_model_and_aliases_uppercased() {
        let products = vec![node_result("ADC-V724", "V724 Pro, 旧型番V724")];
        let lexicon = build_product_lexicon(&products);
        assert_eq!(lexicon.get("ADC-V724"), Some(&"ADC-V724".to_string()));
        assert_eq!(lexicon.get("V724 PRO"), Some(&"ADC-V724".to_string()));
        assert_eq!(lexicon.get("旧型番V724"), Some(&"ADC-V724".to_string()));
    }

    #[test]
    fn lexicon_skips_empty_alias_segments() {
        // "A,,B" のような空要素混じりの aliases でも trim・空要素除去して安全に扱う。
        let products = vec![node_result("ADC-V724", " , ,")];
        let lexicon = build_product_lexicon(&products);
        // aliases 側は全部空なので、model 自身のキーしか登録されない。
        assert_eq!(lexicon.len(), 1);
        assert_eq!(lexicon.get("ADC-V724"), Some(&"ADC-V724".to_string()));
    }

    #[test]
    fn lexicon_skips_product_with_empty_model_attribute() {
        // model 属性が空/欠落の Product ノードは検出語彙に加えない（防御的スキップ）。
        let products = vec![node_result("", "")];
        let lexicon = build_product_lexicon(&products);
        assert!(lexicon.is_empty());
    }
}
