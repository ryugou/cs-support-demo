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
    /// 正規化後の発話からこの語形の出現箇所を取り除いてから surface_forms を照合する
    /// （Issue #75: 「契約前に料金を知りたい」のように、個別案件語の部分文字列を含むだけの
    /// 一般的な質問が誤って signal を立てるのを防ぐ）。省略時は空配列＝抑止なし、従来どおり
    /// surface_forms をそのまま照合する。詳細は
    /// `docs/superpowers/specs/2026-10-05-lexicon-suppress-forms-design.md`。
    #[serde(default)]
    suppress_forms: Vec<String>,
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
    /// 正規化済みの抑止語形。空なら抑止なし（現行どおり normalized_forms を直接照合する）。
    suppress_forms: Vec<String>,
}

/// 抑止語形を取り除いた跡に埋める区切り文字。`normalize_key`（`server/src/resolve.rs`）は
/// `char::is_alphanumeric` を満たす文字しか出力しないため、非英数の制御文字を選べば
/// 正規化後の発話に意図せず出現することがない。空文字ではなくこの1文字に置換するのは、
/// 除去跡の前後の文字が連結して別の surface_form に誤って再マッチするのを防ぐため
/// （`docs/superpowers/specs/2026-10-05-lexicon-suppress-forms-design.md` §2.3）。
const SUPPRESS_REMOVAL_MARKER: char = '\u{0}';

/// surface_forms を正規化し、空文字になるもの（記号のみ等）を除く。
/// 照合のコンパイルと suppress_forms の検証が同じ基準を使うための共通経路
/// （空文字は contains("") が常に真になり、検証をすり抜けさせるため）。
fn normalize_surface_forms(surface_forms: &[String]) -> Vec<String> {
    surface_forms
        .iter()
        .map(|form| normalize_key(form))
        .filter(|form| !form.is_empty())
        .collect()
}

/// 正規化済み発話に対して、全抑止語形の一致範囲（同じ語形の重なり合う出現を含む）を
/// 先に収集し、重なり・隣接する範囲を統合してから、各範囲を `SUPPRESS_REMOVAL_MARKER`
/// 1 文字に置き換えた文字列を返す。
///
/// 置換を順に重ねると、先に置いた区切り文字が別の抑止語形の一致を壊す（"abc" を置換した後は
/// 元の "bcd" が検索できない）ため、必ず元の文字列に対して一度に範囲を確定する。
/// 範囲は `str` の検索 API が返すバイト位置で、常に UTF-8 の文字境界に乗る。
fn mask_suppressed(normalized: &str, suppress_forms: &[String]) -> String {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for form in suppress_forms {
        let mut search_from = 0;
        while let Some(offset) = normalized[search_from..].find(form.as_str()) {
            let start = search_from + offset;
            ranges.push((start, start + form.len()));
            // match_indices は重ならない出現しか返さないため、1 文字ずつ進めて探す。
            let first_char_len = normalized[start..].chars().next().map_or(1, char::len_utf8);
            search_from = start + first_char_len;
        }
    }
    ranges.sort_unstable();

    let mut masked = String::with_capacity(normalized.len());
    let mut cursor = 0;
    let mut ranges = ranges.into_iter().peekable();
    while let Some((start, mut end)) = ranges.next() {
        // 重なる範囲・隣接する範囲（next.start <= end）を 1 つに統合する。
        while let Some(&(next_start, next_end)) = ranges.peek() {
            if next_start > end {
                break;
            }
            end = end.max(next_end);
            ranges.next();
        }
        masked.push_str(&normalized[cursor..start]);
        masked.push(SUPPRESS_REMOVAL_MARKER);
        cursor = end;
    }
    masked.push_str(&normalized[cursor..]);
    masked
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
    /// シグナル名の lexicon 宣言順（ファイルの `signals` 配列順。`llm_only` も含む全件）。
    /// `SignalSet`（`BTreeSet`）はアルファベット順になるため、宣言順を必要とする呼び出し側
    /// （`handoff_items::derive_handoff_items`、design doc
    /// `2026-10-07-partial-answer-with-handoff-design.md` §2）がこれを使って並べ替える。
    declaration_order: Vec<String>,
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
        let declaration_order: Vec<String> = file
            .signals
            .iter()
            .map(|entry| entry.signal.clone())
            .collect();
        // suppress_forms の検証は llm_only でのフィルタ前、全エントリに対して行う
        // （llm_only エントリが suppress_forms を宣言すること自体を拒否する必要があるため）。
        for entry in &file.signals {
            if entry.llm_only && !entry.suppress_forms.is_empty() {
                anyhow::bail!(
                    "signal {} is llm_only and must not declare suppress_forms {:?} (string \
                     matching is not performed for llm_only signals)",
                    entry.signal,
                    entry.suppress_forms
                );
            }
            let normalized_surface_forms = normalize_surface_forms(&entry.surface_forms);
            for suppress_form in &entry.suppress_forms {
                let normalized_suppress = normalize_key(suppress_form);
                if normalized_suppress.is_empty() {
                    anyhow::bail!(
                        "signal {} has a suppress_forms entry {:?} that normalizes to an empty \
                         string",
                        entry.signal,
                        suppress_form
                    );
                }
                if !normalized_surface_forms
                    .iter()
                    .any(|surface_form| normalized_suppress.contains(surface_form.as_str()))
                {
                    anyhow::bail!(
                        "signal {} has a suppress_forms entry {:?} that does not contain any of \
                         its surface_forms, so suppressing it would never change the match \
                         result",
                        entry.signal,
                        suppress_form
                    );
                }
            }
        }
        let entries = file
            .signals
            .into_iter()
            // llm_only の signal は文字列照合の対象にしない（classes / vocabulary には登録済み）。
            .filter(|entry| !entry.llm_only)
            .map(|entry| {
                let normalized_forms = normalize_surface_forms(&entry.surface_forms);
                // 有効な surface form が 1 つも無い signal は「存在するのに決して
                // 抽出されない」サイレント never-match になるため、ロード時に拒否する。
                if normalized_forms.is_empty() {
                    anyhow::bail!(
                        "signal {} has no usable surface forms after normalization",
                        entry.signal
                    );
                }
                let suppress_forms: Vec<String> = entry
                    .suppress_forms
                    .iter()
                    .map(|form| normalize_key(form))
                    .collect();
                Ok(CompiledEntry {
                    signal: entry.signal,
                    normalized_forms,
                    suppress_forms,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            entries,
            classes,
            customer_labels,
            vocabulary,
            declaration_order,
        })
    }

    /// シグナル名の lexicon 宣言順（`llm_only` を含む全件）。`SignalSet` 自体はアルファベット順
    /// （`BTreeSet`）になるため、宣言順で並べ替えたい呼び出し側がこれを使う。
    pub(crate) fn declared_signal_order(&self) -> &[String] {
        &self.declaration_order
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
                if entry.suppress_forms.is_empty() {
                    entry
                        .normalized_forms
                        .iter()
                        .any(|form| normalized.contains(form))
                } else {
                    let masked = mask_suppressed(&normalized, &entry.suppress_forms);
                    entry
                        .normalized_forms
                        .iter()
                        .any(|form| masked.contains(form))
                }
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

    // --- Issue #75: suppress_forms（抑止語形）---
    //
    // 設計書 docs/superpowers/specs/2026-10-05-lexicon-suppress-forms-design.md §6 のテスト表に
    // 対応する。手組みの lexicon で照合規則そのものを固定し、bundled lexicon との結合は
    // decision.rs 側の回帰テストで固定する。

    #[test]
    fn suppress_forms_hide_a_signal_only_when_no_other_surface_form_remains() {
        let lex = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "contract_billing_question", "class": "context",
                  "surface_forms": ["契約", "解約"], "suppress_forms": ["契約前"] },
                { "signal": "mold", "class": "hazard", "surface_forms": ["カビ"] }
            ] }"#,
        )
        .expect("lexicon with suppress_forms parses");

        // 抑止語形に一致する箇所しかない発話では signal が立たない。
        assert!(!lex
            .normalize("契約前に料金を知りたいです")
            .contains(&Signal::new("contract_billing_question")));

        // 抑止語形と、別の箇所の surface_forms（解約）の両方を含む発話では立つ。
        assert!(lex
            .normalize("契約前ですが解約金はいくらですか")
            .contains(&Signal::new("contract_billing_question")));

        // suppress_forms を持たないエントリ（mold）は影響を受けない。
        assert!(lex.normalize("カビが生えた").contains(&Signal::new("mold")));
    }

    #[test]
    fn suppress_forms_removal_does_not_join_adjacent_characters_into_a_false_match() {
        // "ab" が surface_form、"xaby" が suppress_form。空文字へ置換すると
        // "za" + "xaby" + "bz" から "xaby" を抜いた残り "za" + "bz" = "zabz" に
        // 新たな "ab" が生まれてしまう（前後の連結）。区切り文字へ置換していれば
        // "za\0bz" になり "ab" は生まれない。
        let lex = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "concat_test", "class": "context", "surface_forms": ["ab"],
                  "suppress_forms": ["xaby"] }
            ] }"#,
        )
        .expect("lexicon with suppress_forms parses");

        assert!(!lex
            .normalize("zaxabybz")
            .contains(&Signal::new("concat_test")));
    }

    #[test]
    fn suppress_forms_mask_both_ranges_when_two_forms_share_a_prefix() {
        // "契約する" は "契約する前" の接頭辞を共有する短い語形。どちらの範囲も
        // マスクされなければ "前"（別の surface_form）が残って誤って signal が立つ。
        let lex = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "contract_billing_question", "class": "context",
                  "surface_forms": ["契約", "前"],
                  "suppress_forms": ["契約する前", "契約する"] }
            ] }"#,
        )
        .expect("lexicon with suppress_forms parses");

        assert!(!lex
            .normalize("契約する前です")
            .contains(&Signal::new("contract_billing_question")));
    }

    fn signal_fires(surface_forms: &str, suppress_forms: &str, utterance: &str) -> bool {
        let json = format!(
            r#"{{ "signals": [ {{ "signal": "s", "class": "context",
                "surface_forms": {surface_forms}, "suppress_forms": {suppress_forms} }} ] }}"#
        );
        LexiconNormalizer::from_json(&json)
            .expect("lexicon parses")
            .normalize(utterance)
            .contains(&Signal::new("s"))
    }

    #[test]
    fn suppress_forms_mask_overlapping_distinct_forms() {
        // "abc" と "bcd" は "abcd" の中で重なる。順に置換すると "abc" を置換した後
        // "bcd" が検索できず、元は "bcd" の一部だった "d" が残って signal が立つ。
        assert!(!signal_fires(r#"["b", "d"]"#, r#"["abc", "bcd"]"#, "abcd"));
    }

    #[test]
    fn suppress_forms_mask_overlapping_occurrences_of_the_same_form() {
        // 語形 "aa" は "aaa" の位置 0 と 1 の両方に出現する（重なり合う出現）。
        assert!(!signal_fires(r#"["a"]"#, r#"["aa"]"#, "aaa"));
    }

    #[test]
    fn suppress_forms_keep_a_surface_form_outside_every_suppressed_range() {
        assert!(signal_fires(r#"["b", "d"]"#, r#"["abc", "bcd"]"#, "abcdxd"));
        assert!(signal_fires(r#"["a"]"#, r#"["aa"]"#, "aaaba"));
    }

    #[test]
    fn suppress_forms_mask_overlapping_multibyte_forms() {
        // "契約前" と "前に" は "契約前に" の中で "前" を共有して重なる。surface_form の
        // "契約"・"に" はどちらの範囲にも含まれるため、全体がマスクされて signal は立たない。
        assert!(!signal_fires(
            r#"["契約", "に"]"#,
            r#"["契約前", "前に"]"#,
            "契約前に"
        ));
        // 抑止範囲の外（"です"）に surface_form があれば立つ。
        assert!(signal_fires(
            r#"["契約", "に", "です"]"#,
            r#"["契約前", "前に"]"#,
            "契約前にです"
        ));
    }

    #[test]
    fn mask_suppressed_replaces_each_merged_range_with_one_marker() {
        let masked = mask_suppressed("xabcdy", &["abc".to_string(), "bcd".to_string()]);
        assert_eq!(masked, format!("x{SUPPRESS_REMOVAL_MARKER}y"));
        // 隣接する範囲も統合して 1 文字にする。
        let adjacent = mask_suppressed("abcd", &["ab".to_string(), "cd".to_string()]);
        assert_eq!(adjacent, SUPPRESS_REMOVAL_MARKER.to_string());
    }

    #[test]
    fn from_json_rejects_a_suppress_form_that_normalizes_to_an_empty_string() {
        let result = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "blank_suppress_test", "class": "context",
                  "surface_forms": ["カビ"], "suppress_forms": ["!!!"] }
            ] }"#,
        );
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("a suppress_forms entry that normalizes to empty must be rejected"),
        };
        assert!(err.to_string().contains("blank_suppress_test"));
    }

    #[test]
    fn from_json_rejects_a_suppress_form_that_does_not_contain_any_surface_form() {
        let result = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "mismatched_suppress_test", "class": "context",
                  "surface_forms": ["解約"], "suppress_forms": ["転居"] }
            ] }"#,
        );
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!(
                "a suppress_forms entry that does not contain any surface_form must be rejected"
            ),
        };
        assert!(err.to_string().contains("mismatched_suppress_test"));
    }

    #[test]
    fn from_json_ignores_blank_surface_forms_when_validating_suppress_forms() {
        // "!!!" は正規化後に空文字になり contains("") が常に真になるため、判定対象から除く。
        let result = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "blank_surface_mixed_test", "class": "context",
                  "surface_forms": ["!!!", "解約"], "suppress_forms": ["転居"] }
            ] }"#,
        );
        let err = match result {
            Err(e) => e,
            Ok(_) => {
                panic!("a suppress_form unrelated to the usable surface_forms must be rejected")
            }
        };
        let message = err.to_string();
        assert!(message.contains("blank_surface_mixed_test"));
        assert!(message.contains("転居"));
    }

    #[test]
    fn from_json_accepts_a_suppress_form_containing_a_usable_surface_form_next_to_a_blank_one() {
        let result = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "blank_surface_ok_test", "class": "context",
                  "surface_forms": ["!!!", "解約"], "suppress_forms": ["解約前"] }
            ] }"#,
        );
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn from_json_rejects_suppress_forms_when_every_surface_form_is_blank() {
        let result = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "all_blank_surface_test", "class": "context",
                  "surface_forms": ["!!!"], "suppress_forms": ["転居"] }
            ] }"#,
        );
        assert!(
            result.is_err(),
            "an entry with no usable surface_form must be rejected"
        );
    }

    #[test]
    fn from_json_rejects_an_llm_only_entry_that_declares_suppress_forms() {
        let result = LexiconNormalizer::from_json(
            r#"{ "signals": [
                { "signal": "llm_only_suppress_test", "class": "hazard",
                  "surface_forms": [], "llm_only": true, "suppress_forms": ["foo"] }
            ] }"#,
        );
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("an llm_only entry with suppress_forms must be rejected"),
        };
        let message = err.to_string();
        assert!(message.contains("llm_only_suppress_test"));
        assert!(message.contains("foo"));
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
