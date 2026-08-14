use crate::harness::prompt_input::collapse_to_single_line;
use crate::resolve::normalize_key;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// 標準化された条件語（specs/signal-vocabulary.md の語彙）。
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct Signal(String);

impl Signal {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub type SignalSet = BTreeSet<Signal>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalClass {
    Hazard,
    Context,
}

/// 軸1（表現ゆれの吸収）。Step 1 は決定論 lexicon、将来 LLM 実装が同 trait に載る（S1-11）。
pub trait SignalNormalizer: Send + Sync {
    fn normalize(&self, text: &str) -> SignalSet;
}

#[derive(Debug, Deserialize)]
struct LexiconEntry {
    signal: String,
    class: SignalClass,
    surface_forms: Vec<String>,
    /// LLM 抽出専用の signal。文字列照合（surface_forms）には使わず、
    /// 分類（classes マップ）と vocabulary_for_prompt にのみ登録する。
    #[serde(default)]
    llm_only: bool,
    /// LLM プロンプト向けの語義。vocabulary_for_prompt に埋め込まれる。**内部専用**
    /// （部署名・`mandatory エスカレーション対象` 等の社内運用語を含みうる）。顧客向け表示には
    /// 使わない。顧客向けには必ず `customer_label` を使うこと。
    #[serde(default)]
    description: String,
    /// 顧客提示専用ラベル（[`LexiconNormalizer::customer_label_of`] 専用。Critical 修正）。
    /// 部署名・`mandatory`・「エスカレーション」等の内部運用語・英語スラッグ・識別子を
    /// 含めないこと。この値は `description` と違い、顧客向け自動送信文の材料になる。
    #[serde(default)]
    customer_label: String,
}

#[derive(Debug, Deserialize)]
struct LexiconFile {
    signals: Vec<LexiconEntry>,
}

/// ロード時に正規化済みの照合エントリ（hot path で normalize_key を再計算しない）。
struct CompiledEntry {
    signal: String,
    normalized_forms: Vec<String>,
}

/// vocabulary_for_prompt 向けに保持する語彙 1 件分（signal, class, description）。
struct VocabularyEntry {
    signal: String,
    class: SignalClass,
    description: String,
}

pub struct LexiconNormalizer {
    entries: Vec<CompiledEntry>,
    classes: std::collections::HashMap<String, SignalClass>,
    /// signal → 顧客提示用ラベルの直引き（[`LexiconNormalizer::customer_label_of`] 専用）。
    /// ロード時に [`collapse_to_single_line`] で単一行へ正規化し、正規化後に空文字になる
    /// エントリ（未指定・空文字・空白のみ）は登録しない（`customer_label_of` が `None` を
    /// 返す契約をこのマップの不在で表現するため。Suggestion: 空白のみの値を有効扱いしない）。
    customer_labels: std::collections::HashMap<String, String>,
    vocabulary: Vec<VocabularyEntry>,
}

impl LexiconNormalizer {
    pub fn from_path(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("read signal lexicon {}", path.display()))?;
        Self::from_json(&body)
    }

    pub fn from_json(body: &str) -> Result<Self> {
        let file: LexiconFile = serde_json::from_str(body).context("parse signal lexicon json")?;
        let classes = file
            .signals
            .iter()
            .map(|entry| (entry.signal.clone(), entry.class))
            .collect();
        let customer_labels = file
            .signals
            .iter()
            .filter_map(|entry| {
                // 改行混入（JSON 経由で仕込まれても）が顧客向けプロンプトを崩さないよう、
                // ロード時に単一行へ正規化する。正規化後に空文字なら未指定として扱い、
                // description へは fallback しない（Critical: 内部語彙の漏洩防止）。
                let normalized = collapse_to_single_line(&entry.customer_label);
                if normalized.is_empty() {
                    None
                } else {
                    Some((entry.signal.clone(), normalized))
                }
            })
            .collect();
        let vocabulary = file
            .signals
            .iter()
            .map(|entry| VocabularyEntry {
                signal: entry.signal.clone(),
                class: entry.class,
                description: entry.description.clone(),
            })
            .collect();
        let entries = file
            .signals
            .into_iter()
            // llm_only の signal は文字列照合の対象にしない（classes / vocabulary には登録済み）。
            .filter(|entry| !entry.llm_only)
            .map(|entry| {
                let normalized_forms: Vec<String> = entry
                    .surface_forms
                    .iter()
                    .map(|form| normalize_key(form))
                    .filter(|form| !form.is_empty())
                    .collect();
                // 有効な surface form が 1 つも無い signal は「存在するのに決して
                // 抽出されない」サイレント never-match になるため、ロード時に拒否する。
                if normalized_forms.is_empty() {
                    anyhow::bail!(
                        "signal {} has no usable surface forms after normalization",
                        entry.signal
                    );
                }
                Ok(CompiledEntry {
                    signal: entry.signal,
                    normalized_forms,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            entries,
            classes,
            customer_labels,
            vocabulary,
        })
    }

    pub fn class_of(&self, signal: &Signal) -> Option<SignalClass> {
        self.classes.get(signal.as_str()).copied()
    }

    /// signal がこの lexicon に登録されているか（llm_only も含む）。
    pub fn contains_signal(&self, name: &str) -> bool {
        self.classes.contains_key(name)
    }

    /// 登録済み signal の一意な件数（`llm_only` を含む）。テストの退行ガード専用の
    /// introspection（Warning 2: JSON の入力件数とローダーの認識件数を突き合わせるため）。
    #[cfg(test)]
    pub(crate) fn signal_count(&self) -> usize {
        self.classes.len()
    }

    /// signal の顧客提示用ラベルを返す（顧客向けプロンプトへ英語スラッグや内部運用語
    /// （部署名・`mandatory`・「エスカレーション」等）をそのまま出さないための唯一の
    /// 変換経路。`harness::api::build_known_facts` が使う）。
    ///
    /// **これは顧客提示専用。** 内部用途（LLM 分類プロンプト等）には
    /// [`Self::vocabulary_for_prompt`] が使う `description` を使うこと。取り違えると、
    /// `description` に含まれる部署名・`mandatory エスカレーション対象` のような社内語彙が
    /// 顧客向け自動送信文に混入する（実際に到達可能だった Critical。第 3 層グレーゾーンの
    /// 聞き返し経路は escalation_rules を経由しないため、そこでしか止まらない）。
    ///
    /// lexicon に signal 自体が未登録、または `customer_label` が未指定・空白のみの場合は
    /// `None` を返す。**呼び出し側はこれを「顧客向けに言い換えられない signal」として扱い、
    /// 生スラッグや `description` へ fallback せず行ごと除外する契約**（fallback すると
    /// 上記 Critical が再発するため。`description` へのフォールバックは意図的に実装しない）。
    pub fn customer_label_of(&self, signal: &Signal) -> Option<&str> {
        self.customer_labels
            .get(signal.as_str())
            .map(String::as_str)
    }

    /// LLM 分類プロンプトに埋め込む語彙一覧。1 signal 1 行、
    /// `signal (class): description` 形式（description は空文字の場合あり）。
    pub fn vocabulary_for_prompt(&self) -> String {
        self.vocabulary
            .iter()
            .map(|entry| {
                let class = match entry.class {
                    SignalClass::Hazard => "hazard",
                    SignalClass::Context => "context",
                };
                format!("{} ({}): {}", entry.signal, class, entry.description)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl SignalNormalizer for LexiconNormalizer {
    fn normalize(&self, text: &str) -> SignalSet {
        let normalized = normalize_key(text);
        self.entries
            .iter()
            .filter(|entry| {
                entry
                    .normalized_forms
                    .iter()
                    .any(|form| normalized.contains(form))
            })
            .map(|entry| Signal::new(&entry.signal))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lexicon() -> LexiconNormalizer {
        LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "discoloration", "class": "hazard", "surface_forms": ["変色", "色が変わ"] },
                { "signal": "mold", "class": "hazard", "surface_forms": ["カビ", "かび"] },
                { "signal": "continue_use_question", "class": "context", "surface_forms": ["食べてもいい"] }
            ] }"#,
        )
        .expect("lexicon parses")
    }

    #[test]
    fn normalize_absorbs_surface_variation() {
        let n = lexicon();
        let a = n.normalize("商品が変色しています");
        let b = n.normalize("色が変わってしまった");
        assert_eq!(a, b);
        assert!(a.contains(&Signal::new("discoloration")));
    }

    #[test]
    fn normalize_extracts_multiple_signals() {
        let n = lexicon();
        let s = n.normalize("変色していてカビも生えているが食べてもいいか");
        let expected: Vec<&str> = vec!["continue_use_question", "discoloration", "mold"];
        assert_eq!(s.iter().map(Signal::as_str).collect::<Vec<_>>(), expected);
    }

    #[test]
    fn normalize_returns_empty_for_unrelated_text() {
        let n = lexicon();
        assert!(n.normalize("送料はいくらですか").is_empty());
    }

    #[test]
    fn class_of_distinguishes_hazard_and_context() {
        let n = lexicon();
        assert_eq!(n.class_of(&Signal::new("mold")), Some(SignalClass::Hazard));
        assert_eq!(
            n.class_of(&Signal::new("continue_use_question")),
            Some(SignalClass::Context)
        );
        assert_eq!(n.class_of(&Signal::new("unknown")), None);
    }

    #[test]
    fn rejects_signal_with_no_usable_surface_forms() {
        // 記号のみ（正規化で空になる）の signal はサイレント never-match になるため拒否
        let result = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "broken", "class": "hazard", "surface_forms": ["!!!", "…"] }
            ] }"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn llm_only_entry_allows_empty_surface_forms_and_is_not_string_matched() {
        let lex = LexiconNormalizer::from_json(
            r#"{ "signals": [
            { "signal": "unclassified_risk", "class": "hazard", "surface_forms": [], "llm_only": true,
              "description": "既存のどの signal にも分類できないが、安全・契約・法務上の不安がある発話" },
            { "signal": "mold", "class": "hazard", "surface_forms": ["カビ"] }
        ] }"#,
        )
        .unwrap();
        assert!(lex
            .normalize("カビが生えた unclassified_risk")
            .iter()
            .all(|s| s.as_str() != "unclassified_risk"));
        assert!(lex.contains_signal("unclassified_risk"));
        assert!(lex.class_of(&Signal::new("unclassified_risk")).is_some());
        let prompt = lex.vocabulary_for_prompt();
        assert!(prompt.contains("unclassified_risk") && prompt.contains("分類できない"));
    }

    #[test]
    fn customer_label_of_returns_the_label_for_a_registered_signal() {
        let lex = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "mold", "class": "hazard", "surface_forms": ["カビ"], "customer_label": "カビの発生" }
            ] }"#,
        )
        .unwrap();
        assert_eq!(
            lex.customer_label_of(&Signal::new("mold")),
            Some("カビの発生")
        );
    }

    #[test]
    fn customer_label_of_returns_none_for_an_unregistered_signal() {
        let lex = lexicon();
        assert_eq!(lex.customer_label_of(&Signal::new("totally_unknown")), None);
    }

    #[test]
    fn customer_label_of_returns_none_when_customer_label_is_unset() {
        // customer_label を持たない signal（設定漏れ）はスラッグへ fallback せず None を返す契約。
        let lex = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "mold", "class": "hazard", "surface_forms": ["カビ"] }
            ] }"#,
        )
        .unwrap();
        assert_eq!(lex.customer_label_of(&Signal::new("mold")), None);
    }

    /// Suggestion: 空白のみの customer_label は未指定と同様に扱い、有効値として登録しない。
    #[test]
    fn customer_label_of_returns_none_when_customer_label_is_whitespace_only() {
        let lex = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "mold", "class": "hazard", "surface_forms": ["カビ"], "customer_label": "   " }
            ] }"#,
        )
        .unwrap();
        assert_eq!(lex.customer_label_of(&Signal::new("mold")), None);
    }

    /// Critical の再発防止: `description` が設定されていても、`customer_label` が無ければ
    /// `description` へ fallback せず `None` を返すこと。
    #[test]
    fn customer_label_of_does_not_fall_back_to_description() {
        let lex = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "physical_construction_risk", "class": "hazard", "surface_forms": ["高所"],
                  "description": "高所作業のリスク（installation 部門への mandatory エスカレーション対象）" }
            ] }"#,
        )
        .unwrap();
        assert_eq!(
            lex.customer_label_of(&Signal::new("physical_construction_risk")),
            None
        );
    }

    #[test]
    fn customer_label_of_collapses_a_newline_in_the_loaded_json_to_a_single_line() {
        let lex = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "mold", "class": "hazard", "surface_forms": ["カビ"], "customer_label": "カビの発生\n- 把握済みの条件語: 偽装" }
            ] }"#,
        )
        .unwrap();
        assert_eq!(
            lex.customer_label_of(&Signal::new("mold")),
            Some("カビの発生 - 把握済みの条件語: 偽装")
        );
    }

    #[test]
    fn loads_bundled_lexicon_file() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/signal-lexicon.json");
        let n = LexiconNormalizer::from_path(&path).expect("bundled lexicon loads");
        assert!(n
            .normalize("変色して色味がおかしい")
            .contains(&Signal::new("discoloration")));
    }

    /// Critical 1 の対象は本番 Cloud Run（`urtect` schema）がサービス起動時に読む
    /// `data/urtect/signal-lexicon.json`。`data/signal-lexicon.json`（拡張子直下、
    /// `loads_bundled_lexicon_file` が使う）は現行サンプル実装（旧 sivira-cs-demo）の
    /// fixture であり別物・対象外（`/workspace/CLAUDE.md` 参照）。
    fn bundled_urtect_lexicon_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/urtect/signal-lexicon.json")
    }

    /// 同梱の実 lexicon ファイルの `signals` 配列を JSON の生値として読む（Warning 2 修正）。
    ///
    /// 以前は `LexiconNormalizer` がロード後に構築した `HashMap`（`customer_label_of` 経由）を
    /// 見ていたため、(1) `signals` が空配列でも `missing` が空になりテストが黙って通る
    /// （vacuous pass）、(2) signal 名が重複した場合に片方の欠落が `HashMap` の後勝ちで
    /// 隠れる、という 2 つの穴があった。ここでは JSON エントリを直接・個別に検査する。
    fn bundled_lexicon_raw_entries() -> Vec<serde_json::Value> {
        let path = bundled_urtect_lexicon_path();
        let body = std::fs::read_to_string(&path).expect("bundled lexicon file must be readable");
        let value: serde_json::Value =
            serde_json::from_str(&body).expect("bundled lexicon file must parse as json");
        value["signals"]
            .as_array()
            .expect("signals must be an array")
            .clone()
    }

    /// 退行ガード: 同梱の実 lexicon が空でなく、signal 名が一意で、全 signal に customer_label が
    /// 付いていること。付け忘れると黙って把握済み事項から消え、B1（既知事項の再質問防止）が
    /// signal 単位で無効化される。
    #[test]
    fn bundled_lexicon_has_a_customer_label_for_every_signal() {
        let entries = bundled_lexicon_raw_entries();
        assert!(
            !entries.is_empty(),
            "bundled lexicon must not be empty (an empty list would make the checks below vacuously pass)"
        );

        let mut seen = std::collections::HashSet::new();
        let mut duplicates = Vec::new();
        let mut missing = Vec::new();
        for entry in &entries {
            let name = entry["signal"]
                .as_str()
                .expect("each signal entry must have a signal name")
                .to_string();
            if !seen.insert(name.clone()) {
                duplicates.push(name.clone());
            }
            let raw_label = entry["customer_label"].as_str().unwrap_or("");
            if collapse_to_single_line(raw_label).is_empty() {
                missing.push(name);
            }
        }
        assert!(
            duplicates.is_empty(),
            "duplicate signal names would hide a missing customer_label behind HashMap dedup: {duplicates:?}"
        );
        assert!(
            missing.is_empty(),
            "these signals have no usable customer_label in the bundled lexicon: {missing:?}"
        );

        // エントリ単位で一意性・非空を確認した上で、ローダーが同じ件数を認識しているかを
        // 突き合わせる（ローダー側の取りこぼしに対する退行ガード）。
        let n = LexiconNormalizer::from_path(&bundled_urtect_lexicon_path())
            .expect("bundled lexicon loads");
        assert_eq!(
            n.signal_count(),
            entries.len(),
            "LexiconNormalizer must recognize exactly as many distinct signals as the json input"
        );
    }

    /// `text` に ASCII 英字（大文字・小文字を区別しない）またはアンダースコアの連続 4 文字
    /// 以上が含まれるかを判定する（内部運用スラッグ `support_desk` / `SECURITY_TEAM` /
    /// `Support_Desk` 等の漏洩検知専用ヘルパー。Warning 3 修正）。
    ///
    /// 閾値を 3 ではなく 4 にしているのは、大文字を大小区別なく数えると `ADC-V724`（`ADC`）・
    /// `iOS`（`iOS`）・`Web`（`Web`）のような顧客向けに正当な 3 文字の英字表記まで
    /// 誤検知するため。`support_desk`・`installation`・`security`・`hardware`・`mandatory` は
    /// いずれも 4 文字以上の連続する英字を含むため、この調整後も検出できる。ハイフンは区切り
    /// 文字として run をリセットするが、`support-desk` のように分割後の各語がそれ自体で
    /// 4 文字以上あれば検出される（`support` の 7 文字で検出）。
    fn contains_ascii_slug(text: &str) -> bool {
        let mut run = 0usize;
        for c in text.chars() {
            if c.is_ascii_alphabetic() || c == '_' {
                run += 1;
                if run >= 4 {
                    return true;
                }
            } else {
                run = 0;
            }
        }
        false
    }

    #[test]
    fn contains_ascii_slug_detects_snake_case_and_uppercase_forms_but_not_short_brand_terms() {
        assert!(contains_ascii_slug("support_desk"));
        assert!(contains_ascii_slug("installation"));
        assert!(contains_ascii_slug("SECURITY_TEAM"));
        assert!(contains_ascii_slug("Support_Desk"));
        assert!(contains_ascii_slug("support-desk"));
        assert!(!contains_ascii_slug("SD"));
        assert!(!contains_ascii_slug("Wi-Fi"));
        assert!(!contains_ascii_slug("ADC-V724"));
        assert!(!contains_ascii_slug("iOS"));
        assert!(!contains_ascii_slug("Web"));
    }

    /// customer_label 1 件が内部運用語彙（禁止語・ASCII スラッグ）を含まないかを検査し、
    /// 違反があれば理由の文字列を返す（Warning 3）。同梱の実 lexicon への統合テストと、
    /// 合成入力での単体テストの両方から呼べるよう純関数として切り出す。
    ///
    /// 禁止語の部分一致は大小文字を無視して比較する（`label.to_lowercase()` /
    /// `term.to_lowercase()` の双方を小文字化してから比較）。
    fn customer_label_vocabulary_violations(signal_name: &str, label: &str) -> Vec<String> {
        const FORBIDDEN_SUBSTRINGS: &[&str] = &[
            "部門",
            "mandatory",
            "エスカレーション",
            "support_desk",
            "installation",
            "security",
            "hardware",
        ];
        let mut violations = Vec::new();
        let label_lower = label.to_lowercase();
        for term in FORBIDDEN_SUBSTRINGS {
            if label_lower.contains(&term.to_lowercase()) {
                violations.push(format!("{signal_name}: contains forbidden term {term:?}"));
            }
        }
        if contains_ascii_slug(label) {
            violations.push(format!(
                "{signal_name}: customer_label {label:?} contains an ascii slug"
            ));
        }
        violations
    }

    #[test]
    fn customer_label_vocabulary_violations_detects_uppercase_and_mixed_case_slugs() {
        assert!(!customer_label_vocabulary_violations("x", "SECURITY_TEAMへの取次").is_empty());
        assert!(!customer_label_vocabulary_violations("x", "Support_Deskへの取次").is_empty());
        assert!(!customer_label_vocabulary_violations("x", "support-desk 経由のご案内").is_empty());
    }

    #[test]
    fn customer_label_vocabulary_violations_detects_forbidden_terms_regardless_of_case() {
        assert!(!customer_label_vocabulary_violations("x", "Mandatoryのご案内").is_empty());
    }

    #[test]
    fn customer_label_vocabulary_violations_allows_short_customer_facing_terms() {
        assert!(customer_label_vocabulary_violations("x", "SDカードの初期化").is_empty());
        assert!(customer_label_vocabulary_violations("x", "Wi-Fi接続の不具合").is_empty());
        assert!(
            customer_label_vocabulary_violations("x", "型番 ADC-V724 に関するお問い合わせ")
                .is_empty()
        );
        assert!(customer_label_vocabulary_violations("x", "iOSアプリでのご利用").is_empty());
        assert!(customer_label_vocabulary_violations("x", "Webブラウザでのご利用").is_empty());
    }

    /// Critical の中核となる退行ガード: 同梱の実 lexicon の全 customer_label に、内部運用語
    /// （部署名・`mandatory`・「エスカレーション」等）や ASCII スラッグが含まれないこと。
    /// `customer_label` フィールドを追加する前（= `description` を顧客向けに使っていた状態）
    /// では、hazard 系 5 signal がこの禁止語を含むため必ず赤くなる。
    #[test]
    fn bundled_lexicon_customer_labels_do_not_leak_internal_vocabulary() {
        let mut violations: Vec<String> = Vec::new();
        for entry in bundled_lexicon_raw_entries() {
            let name = entry["signal"].as_str().unwrap_or("<unknown>").to_string();
            let raw_label = entry["customer_label"].as_str().unwrap_or("");
            let label = collapse_to_single_line(raw_label);
            if label.is_empty() {
                continue; // 未登録は別テスト（has_a_customer_label_for_every_signal）の責務。
            }
            violations.extend(customer_label_vocabulary_violations(&name, &label));
        }
        assert!(
            violations.is_empty(),
            "internal vocabulary leaked into customer-facing labels: {violations:?}"
        );
    }
}
