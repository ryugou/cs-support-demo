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

/// 型番トークンの抽出パターン（2026-08-14 裁定。全面 case-insensitive・貪欲マッチ）。`(?i)` で
/// 大文字小文字を無視するため、`[A-Z]` は `a-z` にもマッチする。
///
/// 構造: "ADC-" の後、英数字とハイフンを貪欲に読み進める。末尾サフィックスの文字数やマッチ
/// 終端直後の文字種を一切制限しない。
///
/// # なぜこの単純な式に戻したか（旧方式のfail-open）
///
/// 一時期、末尾サフィックスを「高々1文字の大文字」に制限し（`[A-Z]?`）、かつ
/// `extract_model_tokens` 側でマッチ終端直後が ASCII 英数字なら**マッチ全体を丸ごと破棄**する
/// 境界チェックを重ねる方式を採用していた。狙いは「ADC-V724camera」のような型番直後に英字が
/// 続く語を型番として誤検出しない（fail-closed 側の偽陽性を避ける）ことだった。
///
/// しかしこれはユーザーにより棄却された。顧客は型番を `adc-v521ir` のように**小文字**で、かつ
/// 2文字以上のサフィックス（"ir" 等）付きで入力するのが普通である。旧方式では `[A-Z]?` が
/// 高々1文字しか許さないため正規表現が `adc-v521i` までしかマッチできず、続く `r` が ASCII
/// 英数字であるため境界チェックがマッチ全体を丸ごと破棄していた。結果、型番トークンが一切
/// 検出されず `extract_model_tokens` が空配列を返す。これは **fail-open**（取扱外型番への
/// 言及が「言及なし」として §3.1 質問側ゲートを素通りする）であり、reviewer が Critical (C-1)
/// として FAIL 判定した。
///
/// 2026-08-14 裁定: 検出は全面 case-insensitive・貪欲マッチへ戻す。これにより「型番直後に
/// 区切りなしで英字が続く語」（例:「ADC-V724camera」）は「ADC-V724CAMERA」として一体抽出され、
/// allowlist（型番のみ）と不一致になり取扱外扱いになる、という稀な偽陽性が生じる
/// （`extract_model_tokens` の doc コメントの Accepted Risk を参照）。これは fail-closed 側
/// （実際には取扱内の製品を誤って「取扱外」と断る）であり、日本語の顧客文で型番に英単語が
/// 直結する書き方は実質発生しないため、Accepted Risk として受容する。
fn model_token_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)ADC-[A-Z0-9][A-Z0-9-]*").expect("model token regex must compile")
    })
}

/// Unicode のハイフン様記号（U+2010〜U+2015 の各種ダッシュ、U+2212 数学マイナス記号、
/// 全角ハイフンマイナス U+FF0D）を ASCII `'-'` へ変換し、U+00AD SOFT HYPHEN を除去する
/// （Issue #28 codex レビュー採用3）。
///
/// 全角ハイフンマイナスは NFKC でも半角化されるが、それ以外（U+2010 HYPHEN、U+2212 MINUS SIGN
/// 等）は正準/互換分解が定義されておらず NFKC の対象外なので、NFKC の前段でここで明示変換
/// しないと、顧客が ASCII ハイフン以外の文字で型番を書いた場合に型番トークンとして検出されない
/// （§3.1 の質問側ゲートが素通りし false negative になる）。
///
/// U+00AD SOFT HYPHEN はゼロ幅の書式制御文字（本来はソフトウェアによる行末ハイフネーション
/// 挿入位置を示す）で、正準分解が定義されておらず NFKC でも消えない。除去しないと、コピペや
/// 自動整形の過程で型番の途中に紛れ込んだ場合に、そのままでは正規表現がマッチせず検出漏れ
/// （false negative）になる。ハイフンとして扱う（`-` へ変換する）のではなく除去するのは、
/// SOFT HYPHEN が視覚的には非表示であり、`-` に変換すると「ADC-V724」が「ADC--V724」のように
/// 余分なハイフンを持つ形になってしまうため。
fn normalize_hyphens(s: &str) -> String {
    s.chars()
        .filter(|&c| c != '\u{00AD}')
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' | '\u{2212}' | '\u{FF0D}' => '-',
            other => other,
        })
        .collect()
}

/// 型番トークン比較用の正規化: Unicode ハイフン類を ASCII `'-'` へ変換し、NFKC（全角英数を
/// 半角へ）してから大文字化する（design doc §3.1、Issue #28 codex レビュー採用3）。
fn normalize_model_token(raw: &str) -> String {
    normalize_hyphens(raw)
        .nfkc()
        .collect::<String>()
        .to_uppercase()
}

/// 型番断片（"ADC-" 接頭辞を含まない）抽出パターン: 英字1文字以上 + 数字2桁以上 +
/// 任意の英数字。allowlist の型番はすべて "ADC-" の後に同じ形（例: "V724", "VC729P"）が
/// 続くため、これを allowlist 側のサフィックスと突合して veto に使う（Issue #28 W1-b）。
fn bare_fragment_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)[A-Z]+[0-9]{2,}[A-Z0-9]*")
            .expect("bare fragment regex must compile")
    })
}

fn extract_bare_fragments(surface: &str) -> Vec<String> {
    let normalized: String = surface.nfkc().collect();
    bare_fragment_regex()
        .find_iter(&normalized)
        .map(|m| m.as_str().to_uppercase())
        .collect()
}

/// テキストから型番トークンを検出順（出現順）に抽出する。抽出前に文字列全体の Unicode
/// ハイフン類を ASCII `'-'` へ変換（SOFT HYPHEN は除去）してから NFKC 正規化し、マッチした
/// 断片を大文字化して返す（比較は常にこの正規化後の形で行う）。
///
/// 境界チェックは行わない（2026-08-14 裁定。`model_token_regex` の doc コメント参照）。
///
/// # Accepted Risk（2026-08-14 裁定）
///
/// 全面貪欲マッチのため、型番直後に区切りなしで英字が続く語（例:「ADC-V724camera」）は
/// 「ADC-V724CAMERA」として一体抽出される。これは allowlist（型番のみ）と一致せず、実際には
/// 取扱内の「ADC-V724」について聞いている顧客を誤って「取扱外」と断ってしまう偽陽性になる。
/// 日本語の顧客文で型番に英単語が直結する書き方（スペースや助詞を挟まない）は実質発生しない
/// ため、fail-open（旧方式が引き起こしていた検出漏れ）よりましな fail-closed 側の代償として
/// 受容する。回帰テスト `accepted_risk_...` で固定化している。
///
/// `質問側ゲート`（§3.1）・`材料選別`（§3.2）・`応答側ゲート`（§3.5）の 3 箇所が共通で使う
/// 唯一の抽出経路（重複実装しない）。
pub fn extract_model_tokens(text: &str) -> Vec<String> {
    let normalized: String = normalize_hyphens(text).nfkc().collect();
    model_token_regex()
        .find_iter(&normalized)
        .map(|m| {
            // 正規表現の `[A-Z0-9-]*` は末尾ハイフンも貪欲に飲み込むため、顧客が区切りに打つ
            // ハイフンや原文の折返し由来の末尾ハイフン（例:「ADC-V724-の設定」）が型番の一部に
            // なってしまう。trim して allowlist の "ADC-V724" と一致させる。
            m.as_str().to_uppercase().trim_end_matches('-').to_string()
        })
        .collect()
}

/// [`ProductAllowlist::matches_in_scope_model`] の条件 2（surface 全体の allowlist 型番への
/// サフィックス一致）が対象にする断片の最小文字数（Issue #28 codex レビュー Warning 是正、
/// 修正2）。
///
/// 1〜2文字の断片は allowlist 型番の末尾数文字と偶然一致しやすく、真の foreign 参照を誤って
/// veto してしまうため 3 文字未満は対象外とする。例えば取扱 7 型番の実在する 2 文字末尾
/// （`"23"`（ADC-V523）・`"24"`（ADC-V724）・`"7P"`（ADC-VC727P / ADC-VC827P）等）は、
/// `ends_with` 実装下では `MIN_FRAGMENT_VETO_CHARS` が無ければそのまま偶然一致して veto される
/// （回帰テスト `confirmed_foreign_reference_still_fires_when_surface_is_a_two_char_fragment_matching_an_in_scope_suffix`
/// が、この定数を下げると red になることを固定している）。
///
/// **この定数は空文字ガードも兼ねている**（`"...".ends_with("")` は常に true になるため、
/// `MIN_FRAGMENT_VETO_CHARS` が 0 まで下がると空文字 surface が無条件で veto される）。ただし
/// Issue #28 W-1 是正でこの定数に依存しない明示的な空文字ガードを
/// [`ProductAllowlist::matches_in_scope_model`] 内に別途置いたため、この定数を変更しても
/// 空文字の扱いは変わらない（多層防御）。
const MIN_FRAGMENT_VETO_CHARS: usize = 3;

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
    ///
    /// allowlist は `model` 属性のみで構築する（`name` は併合しない。W-3 是正）。以前は `name`
    /// 属性も正規化して一致判定（`normalized`）へマージしていたが、`extract_model_tokens` は
    /// 常に「ADC-」で始まる型番形式のトークンしか生成しないため、`name`（現行データでは
    /// `name == model`、design doc §1）由来のマージは実利用上到達しない不要な複雑さだった。
    /// 単純化のため撤去する。
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

    /// `surface`（LLM が抽出した表層表記、型番形式とは限らない自由記述）が、決定論的に見て
    /// 取扱内製品を指していると言えるかを判定する（Issue #28 codex Stage2 Warning 4 是正）。
    ///
    /// `confirmed_foreign_reference` が LLM の `foreign` 分類を採用する前の veto に使う:
    /// LLM が製品マスタと矛盾する分類（取扱内製品を `foreign` と誤答）を返しても、決定論の
    /// 製品マスタ（allowlist）の方を正とし、誤答を採用しない。`normalized` が private のため
    /// このモジュール内（`ProductAllowlist` の impl）に置く。呼び出し側は `surface` にも
    /// `matched_model` にも同じこのメソッドを使う（Issue #28 W1-a 修正1で対称化した）。
    ///
    /// 3 つの判定を or で combine する:
    /// 1. `surface` から `extract_model_tokens` で型番トークンを抽出した結果に、allowlist 内の
    ///    型番が 1 つでも含まれる（surface が「ADC-V724について」のような文でも拾える）。
    /// 2. `surface` 全体を `normalize_model_token` で正規化した文字列が、allowlist 内の
    ///    いずれかの型番の**サフィックス**である（surface が型番の断片や略記、例えば
    ///    `"V724"` のような場合でも、それが取扱内型番 `"ADC-V724"` の末尾でしかないケースを
    ///    捕まえて veto する）。[`MIN_FRAGMENT_VETO_CHARS`] 文字未満の断片は対象外（下記
    ///    doc comment 参照）。
    /// 3. `surface` から `extract_bare_fragments` で "ADC-" 接頭辞の無い型番断片
    ///    （例: `"V724"`）を抽出し、allowlist 内のいずれかの型番がその断片を
    ///    `"-{fragment}"` サフィックスとして持つ（Issue #28 W1-b）。条件 2 は
    ///    「surface 全体」を 1 つの文字列としてサフィックス判定するため、`"V724ドアベル"` の
    ///    ように型番断片の後に文字が続く surface では一致しない（正規化後の文字列が
    ///    allowlist のどの型番よりも長くなり、サフィックス関係が成立しない）。この条件は
    ///    それを捕まえる。
    ///
    /// # veto は Accepted Risk であり「安全側」ではない（W-3 是正で書き直し）
    ///
    /// 条件 2・3 はどちらも偶然の部分一致で誤って veto しうる（例: 3 文字ちょうどの断片
    /// `"724"` は無関係な他社製品の型番の一部であっても `"ADC-V724"` とサフィックス一致して
    /// veto される）。これは §3.1 質問側ゲートの **false negative**（本来 foreign と扱うべき
    /// 参照を見逃す）である。
    ///
    /// 以前の版は「§3.2 材料選別・§3.5 応答側ゲートが別途防ぐので安全性の逆方向の失敗は
    /// 起きない」と断言していたが、これは成立しない。§3.2 (`out_of_scope_material_exclusion`) も
    /// §3.5 (`api.rs` の `gate_generated_text` / `gate_customer_reply_draft`) も、どちらも
    /// [`extract_model_tokens`]（正規表現 `(?i)ADC-[A-Z0-9][A-Z0-9-]*`）に依存しており、
    /// **`ADC-` 接頭辞付きの型番の文字列出現**しか検出できない。したがって次のいずれかに
    /// 該当すると、§3.2 / §3.5 はこの veto の見逃しを回収できない:
    /// - 顧客の質問が型番ではなく非型番の名称（例:「Ring のドアベル」）で取扱外製品を指して
    ///   おり、検索材料・生成文のどちらにも `ADC-` 型番の文字列が現れない
    /// - 生成された応答文が質問中の型番をそのまま再掲しない（§3.5 は「生成文に取扱外型番を
    ///   書かない」ための防衛線であり、「取扱外の質問に答えない」ことは保証しない）
    ///
    /// この場合、決定論のゲート（§3.1 / §3.2 / §3.5）では完全には回復できず、残る防衛線は
    /// §3.3 / §3.4 のプロンプト注入（LLM が聞き返し・下書きの内容を自制することに期待する、
    /// 非決定論の防衛線）だけになる。したがって条件 2・3 の偶然一致は「安全側だから問題ない」
    /// のではなく、**受容した残余リスク（Accepted Risk）**として扱う。
    ///
    /// # 既知の限界: 末尾を欠いた断片は veto されない（W-4 是正で追記）
    ///
    /// 逆方向（取扱内顧客を誤って断ってしまう側）の既知の限界もある。接頭辞側を残して末尾を
    /// 欠いた断片（例: 取扱内 `ADC-VC729P` の末尾 `P` を落とした `"VC729"`）は、条件 2
    /// （surface 全体が allowlist 型番の**サフィックス**か）にも条件 3（"ADC-" 無し断片の
    /// `"-{fragment}"` サフィックス一致）にも一致しない（`"VC729"` は `"ADC-VC729P"` の
    /// サフィックスではなくプレフィックス寄りの部分文字列であるため）。この場合 veto されず
    /// `confirmed_foreign_reference` が発火し、取扱内製品を持つ顧客を誤って「取扱外」と断って
    /// しまう。`ADC-VC727P` / `ADC-VC827P` も同様に末尾 `P` を落とすと同じ限界に当たる。
    /// spec §3.1 (d) の「サフィックス一致で照合する」規則に忠実な結果であり、照合規則を
    /// 広げるには spec 側の更新と `VC727` / `VC827` との衝突分析が必要なため、現状は受容する
    /// （characterization test
    /// `confirmed_foreign_reference_still_fires_for_a_prefix_only_fragment_of_an_in_scope_model`
    /// で固定）。
    pub(crate) fn matches_in_scope_model(&self, surface: &str) -> bool {
        let trimmed = surface.trim();
        if trimmed.is_empty() {
            // 空文字は allowlist の全型番の suffix になってしまう（`"...".ends_with("")` は
            // 常に true）ため明示的に拒否する。MIN_FRAGMENT_VETO_CHARS の暗黙の副作用にだけ
            // 頼らない（Issue #28 W-1 是正: この定数が将来引き下げられても、この不変条件は
            // ここで独立に守られる）。
            return false;
        }
        if extract_model_tokens(trimmed)
            .iter()
            .any(|token| self.is_in_scope(token))
        {
            return true;
        }
        let normalized_surface = normalize_model_token(trimmed);
        if normalized_surface.chars().count() >= MIN_FRAGMENT_VETO_CHARS
            && self
                .normalized
                .iter()
                .any(|model| model.ends_with(&normalized_surface))
        {
            return true;
        }
        extract_bare_fragments(trimmed).iter().any(|fragment| {
            let suffix = format!("-{fragment}");
            self.normalized.iter().any(|model| model.ends_with(&suffix))
        })
    }

    /// `text` から検出した型番のうち、allowlist 外の最初の 1 件を返す（§3.1 の質問側ゲート・
    /// §4 の定型応答の `{検出型番}` に使う）。
    pub fn first_out_of_scope_token(&self, text: &str) -> Option<String> {
        extract_model_tokens(text)
            .into_iter()
            .find(|t| !self.is_in_scope(t))
    }

    /// `text` から検出した allowlist 外の型番をすべて返す（重複除去・出現順）。Issue #28 codex
    /// レビュー採用5: §3.5 応答側ゲートの warn ログに「何が検出されたか」を残すために使う
    /// （従来は `has_out_of_scope_mention` の真偽値しか無く、運用者がログだけでは検出型番を
    /// 特定できなかった）。
    pub fn out_of_scope_mentions(&self, text: &str) -> Vec<String> {
        let mut seen = HashSet::new();
        extract_model_tokens(text)
            .into_iter()
            .filter(|t| !self.is_in_scope(t))
            .filter(|t| seen.insert(t.clone()))
            .collect()
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

/// LLM（signal 抽出の同乗呼び出し）が抽出した 1 件の製品参照（Issue #28 §3.1 二段目）。
///
/// `surface` は発話中の表層表記（型番そのままとは限らない。略記・俗称・カテゴリ的言及もありうる）、
/// `resolution` は取扱一覧との関係、`matched_model` は `resolution == Matched` のときにモデルが
/// 添える正規型番（コード側では検証せずそのまま保持する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductReference {
    pub surface: String,
    pub resolution: ProductReferenceResolution,
    pub matched_model: Option<String>,
}

/// `ProductReference::resolution` の 3 値（design doc §3.1 二段目）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductReferenceResolution {
    Matched,
    Ambiguous,
    Foreign,
}

/// `surface` 実在チェック専用の正規化: `normalize_model_token` と同じハイフン正規化を共有した
/// うえで、NFKC + 小文字化する（Issue #28 Suggestion 1）。ハイフン正規化は共有するが、
/// それ以外は `normalize_model_token`（「ハイフン正規化 + 大文字化」）とは別物のまま残す。
/// ここでは型番形式を前提にしない自由テキストの表層表記（俗称・カテゴリ言及を含む）を、
/// そのままメッセージ本文と部分一致させたいだけなので、大文字化までは行わない。
///
/// ハイフン正規化を共有しないと、`surface` と `message` の一方だけが Unicode ハイフン類
/// （例: U+2212 MINUS SIGN）で書かれ、もう一方が ASCII ハイフンで書かれている場合に
/// 部分一致が成立せず、`confirmed_foreign_reference` の幻覚ガードが誤って発火しない
/// （本来なら実在するはずの surface を「発話に無い」と判定してしまう）。
fn normalize_for_presence_check(s: &str) -> String {
    normalize_hyphens(s)
        .nfkc()
        .collect::<String>()
        .to_lowercase()
}

/// 顧客向け定型文・ログへ `surface` をそのまま反映（反射）しても安全とみなせる上限文字数
/// （Issue #28 codex Stage2 Warning 3 是正）。
///
/// 根拠: allowlist の実際の型番は概ね 10 文字前後（例:「ADC-VC729P」）で、俗称・略記を含めても
/// 自然な製品言及がこれを大きく超えることはない。64 文字は実際の型番長に対して十分な余裕を
/// 持たせつつ、`surface` に長大な自由記述（発話全体の丸写しに近いもの）が紛れ込んだ場合を
/// 弾くための値として選んだ。`/api/reply` の入力上限（5,000 文字）に対しては十分小さく、
/// 「当社では取り扱いがございません」以降の定型文が切り詰めで消える事態を防ぐ。
pub(crate) const MAX_REFLECTABLE_SURFACE_CHARS: usize = 64;

/// `surface`（trim 済み）を顧客向け定型文・ログへ反射しても安全か（Issue #28 codex Stage2
/// Warning 3 是正）。
///
/// `surface` は LLM 応答から検証なしに deserialize される（`llm.rs::parse_one_product_reference`
/// は非空文字列であることしか検証しない）。ここで長さと制御文字を検査しないと、その値が
/// そのまま `build_out_of_scope_reply` の `{detected_model}` 位置へ入る。長すぎる・改行や
/// 制御文字を含む `surface` を採用すると、定型文の「当社では取り扱いがございません」以降が
/// クライアント側の長さ制限（例: LINE アダプタの 4,900 文字切り詰め）で失われ、顧客の入力が
/// そのまま返るだけの応答になりうる。
///
/// 拒否は Unicode 一般カテゴリ Cc（制御文字）・Cf（書式制御）・Zl（LINE SEPARATOR）・
/// Zp（PARAGRAPH SEPARATOR）をまとめた正規表現クラス [`reflection_unsafe_char_regex`] で行う
/// （Issue #28 codex レビュー是正: 列挙方式から Unicode 一般カテゴリの網羅方式へ変更。`regex`
/// crate は既存依存のため新規 crate 追加は不要）。
///
/// U+00AD SOFT HYPHEN・U+180E MONGOLIAN VOWEL SEPARATOR は、旧列挙リストが拒否していた文字の
/// うち `regex` crate 同梱の Unicode テーブルで `\p{Cf}` にマッチするため、この正規表現クラス
/// でカバーされる（reviewer 実測で確認済み）。
///
/// ただし旧列挙リストの `'\u{2060}'..='\u{2069}'` 範囲に含まれていた **U+2065 は例外**で、
/// Unicode 一般カテゴリが Cn（未割り当て）のため `\p{Cf}` にマッチせず、このクラスだけでは
/// 拾えない。U+2065 は Default_Ignorable_Code_Point（将来の書式制御文字用に予約された不可視
/// 領域）であり、準拠レンダラでは不可視になる。設計 spec
/// （`docs/superpowers/specs/2026-08-14-product-scope-design.md`）が規定する反射安全性のクラス
/// は「Cc / Cf / Zl / Zp・ゼロ幅を含む」であり、U+2065 の明示はその明文からは外れるが、旧実装
/// との後方互換のために [`reflection_unsafe_char_regex`] のパターンへ直接追加して保持している。
/// これ以外に旧列挙リストからの退行は無いことを reviewer が U+0000〜U+10FFFF 全スカラ値の
/// スイープで確認済み。
fn is_safe_to_reflect(surface: &str) -> bool {
    let char_count = surface.chars().count();
    char_count <= MAX_REFLECTABLE_SURFACE_CHARS && !reflection_unsafe_char_regex().is_match(surface)
}

/// [`is_safe_to_reflect`] が拒否する文字クラス: Unicode 一般カテゴリ Cc（制御文字）・
/// Cf（書式制御）・Zl（LINE SEPARATOR）・Zp（PARAGRAPH SEPARATOR）に加え、旧列挙方式との
/// 後方互換のため U+2065（Cn・Default_Ignorable、カテゴリ指定では拾えない）を明示追加している
/// （Issue #28 codex レビュー是正、reviewer 実測指摘によるフォローアップ）。
fn reflection_unsafe_char_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"[\p{Cc}\p{Cf}\p{Zl}\p{Zp}\u{2065}]")
            .expect("reflection unsafe char regex must compile")
    })
}

/// Issue #28 §3.1 二段目のコード判定本体: LLM が `foreign` と分類した参照のうち、
/// **matched_model の非矛盾**（`matched_model` が決定論の allowlist 上で取扱内製品を指して
/// いない。resolution=foreign と matched_model=取扱内型番という自己矛盾出力の veto。W1-a
/// 是正。`surface` 側と同じ [`ProductAllowlist::matches_in_scope_model`] を使うため、"ADC-"
/// 接頭辞を欠く型番断片一致も matched_model 側で捕まる。修正1でこの対称性を導入した）、
/// **幻覚ガード**（`surface` が正規化後の `message` 中に実在する）、**最小長**
/// （`surface` が trim 後 2 文字以上）、**反射安全性**（trim 後 64 文字以下・制御文字・
/// 書式制御/ゼロ幅文字なし。Warning 3 / W2 是正）、**製品マスタとの非矛盾**（`surface` が
/// 決定論の allowlist 上で取扱内製品を指していない。Warning 4 / W1-b 是正）をすべて満たす
/// 最初の 1 件を返す。
///
/// 「解釈は LLM、判定はコード」の規律により、LLM が `foreign` と言っただけでは取扱外応答へは
/// 倒さない。LLM が発話に無い表層をでっち上げた場合（幻覚）、1 文字のようなノイズに近い
/// 表層、顧客向け応答に反射すると危険な表層、そして製品マスタと矛盾する誤分類（取扱内製品を
/// `foreign` と誤答するケース）のいずれからも安全側（＝取扱外応答を返さず、以降の通常フローへ
/// 委ねる）に倒す。
pub fn confirmed_foreign_reference<'a>(
    refs: &'a [ProductReference],
    message: &str,
    allowlist: &ProductAllowlist,
) -> Option<&'a ProductReference> {
    let normalized_message = normalize_for_presence_check(message);
    refs.iter().find(|r| {
        if r.resolution != ProductReferenceResolution::Foreign {
            return false;
        }
        if let Some(matched_model) = r.matched_model.as_deref() {
            if allowlist.matches_in_scope_model(matched_model.trim()) {
                // LLM が resolution=foreign としながら matched_model に取扱内型番を入れる
                // 自己矛盾出力を返した場合の veto（Issue #28 W1-a）。surface 側の veto と同じ
                // `matches_in_scope_model` を使うため、"ADC-" 接頭辞を欠く型番断片
                // （例: "V724"）のサフィックス一致も matched_model 側で捕まる（Issue #28 W1-b
                // 是正の対称化）。
                tracing::warn!(
                    matched_model_chars = matched_model.trim().chars().count(),
                    surface_chars = r.surface.trim().chars().count(),
                    "llm classified a product reference as foreign but supplied a matched_model \
                     that matches an in-scope product master model (self-contradictory output); \
                     vetoing"
                );
                return false;
            }
        }
        let surface = r.surface.trim();
        if surface.chars().count() < 2 {
            return false;
        }
        if !normalized_message.contains(&normalize_for_presence_check(surface)) {
            return false;
        }
        if !is_safe_to_reflect(surface) {
            return false;
        }
        if allowlist.matches_in_scope_model(surface) {
            // LLM が製品マスタ（決定論の正本）と矛盾する分類を返した。運用上の兆候として
            // warn するが、surface の生値は出さない（Warning 3 の反射安全性の懸念と同じ理由）。
            tracing::warn!(
                surface_chars = surface.chars().count(),
                "llm classified a product reference as foreign (out of scope) but the surface \
                 matches an in-scope product master model; vetoing this reference and falling \
                 through to the normal flow (the deterministic product master overrides the \
                 llm's interpretation)"
            );
            return false;
        }
        true
    })
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

    // ---- extract_model_tokens: 全面 case-insensitive・貪欲マッチ（2026-08-14 裁定、C-1回帰） ----

    #[test]
    fn extract_model_tokens_finds_the_full_lowercase_token_with_a_multi_char_suffix() {
        // C-1 本体の回帰テスト。旧方式（末尾サフィックス高々1文字の大文字 + 境界チェック）は
        // 顧客が普段書く「adc-v521ir」（小文字・2文字以上のサフィックス "ir"）を
        // "adc-v521i" までしかマッチできず、続く "r" が ASCII 英数字であるため境界チェックが
        // マッチ全体を丸ごと破棄していた。fail-open（型番トークンが1つも検出されず §3.1
        // 質問側ゲートを素通りする）だったため、全面貪欲マッチへ戻した。
        assert_eq!(
            extract_model_tokens("adc-v521irが動きません"),
            vec!["ADC-V521IR"]
        );
    }

    #[test]
    fn extract_model_tokens_finds_the_full_uppercase_token_with_a_multi_char_suffix() {
        assert_eq!(
            extract_model_tokens("ADC-V521IRの調子が悪いです"),
            vec!["ADC-V521IR"]
        );
        assert_eq!(
            extract_model_tokens("ADC-V522IRの調子が悪いです"),
            vec!["ADC-V522IR"]
        );
    }

    #[test]
    fn extract_model_tokens_still_extracts_the_token_when_followed_by_japanese_text() {
        // "の設定" のような通常の日本語直後でも型番は問題なく検出される。
        assert_eq!(
            extract_model_tokens("ADC-V724の設定を教えてください"),
            vec!["ADC-V724"]
        );
    }

    #[test]
    fn extract_model_tokens_soft_hyphen_is_removed_so_the_full_token_is_still_detected() {
        // U+00AD SOFT HYPHEN はゼロ幅の書式制御文字で、コピペ・自動整形の過程で型番の途中に
        // 紛れ込むことがある。NFKC でも消えないため、除去しないと検出漏れになる。
        assert_eq!(
            extract_model_tokens("ADC-V521\u{00AD}IRの設定を教えてください"),
            vec!["ADC-V521IR"]
        );
    }

    #[test]
    fn accepted_risk_a_trailing_ascii_word_is_absorbed_into_the_token_and_becomes_out_of_scope() {
        // Accepted Risk（2026-08-14 裁定）の明文化。「ADC-V724camera」は全面貪欲マッチにより
        // "ADC-V724CAMERA" として一体抽出され、allowlist（型番のみ）と不一致になるため
        // 「取扱外」扱いになる。これは実際には取扱内の ADC-V724 について聞いている顧客を
        // 誤って断ってしまう fail-closed 側の偽陽性だが、fail-open（型番検出漏れ）より優先して
        // 意図的に受容した結果である。fixture_allowlist は ADC-V724 を含む。
        let allow = fixture_allowlist();
        assert_eq!(
            allow.first_out_of_scope_token("ADC-V724cameraの調子が悪いです"),
            Some("ADC-V724CAMERA".to_string())
        );
    }

    // ---- extract_model_tokens: Unicode ハイフン類の正規化（Issue #28 codex レビュー採用3） ----

    #[test]
    fn extract_model_tokens_normalizes_u2010_hyphen_to_ascii_hyphen() {
        // U+2010 HYPHEN。ASCII '-' (U+002D) とは異なるコードポイントで、NFKC でも半角化
        // されない。顧客がこの文字で型番を書いた場合に検出漏れ(false negative)にならないこと。
        assert_eq!(
            extract_model_tokens("ADC\u{2010}V724の設定を教えてください"),
            vec!["ADC-V724"]
        );
    }

    #[test]
    fn extract_model_tokens_normalizes_u2212_minus_sign_to_ascii_hyphen() {
        // U+2212 MINUS SIGN（数学記号）。同じく NFKC の対象外。
        assert_eq!(
            extract_model_tokens("ADC\u{2212}V724の設定を教えてください"),
            vec!["ADC-V724"]
        );
    }

    // ---- confirmed_foreign_reference（Issue #28 §3.1 二段目のコード判定） ----

    fn foreign_ref(surface: &str) -> ProductReference {
        ProductReference {
            surface: surface.to_string(),
            resolution: ProductReferenceResolution::Foreign,
            matched_model: None,
        }
    }

    #[test]
    fn confirmed_foreign_reference_returns_some_when_foreign_surface_is_present_in_message() {
        let refs = vec![foreign_ref("ADC-VDB101")];
        assert_eq!(
            confirmed_foreign_reference(
                &refs,
                "ADC-VDB101について教えてください",
                &fixture_allowlist()
            ),
            Some(&refs[0])
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_foreign_surface_is_not_in_the_message() {
        // LLM の幻覚ガード: 発話に無い表層をモデルが作り出した場合は取扱外へ倒さない。
        let refs = vec![foreign_ref("ADC-VDB101")];
        assert_eq!(
            confirmed_foreign_reference(&refs, "映像が映りません", &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_for_ambiguous_only() {
        let refs = vec![ProductReference {
            surface: "ドアベル".to_string(),
            resolution: ProductReferenceResolution::Ambiguous,
            matched_model: None,
        }];
        assert_eq!(
            confirmed_foreign_reference(&refs, "ドアベルの設定は?", &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_for_matched_only() {
        let refs = vec![ProductReference {
            surface: "ADC-V724".to_string(),
            resolution: ProductReferenceResolution::Matched,
            matched_model: Some("ADC-V724".to_string()),
        }];
        assert_eq!(
            confirmed_foreign_reference(&refs, "ADC-V724の設定は?", &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_is_shorter_than_two_chars() {
        let refs = vec![foreign_ref("V")];
        assert_eq!(
            confirmed_foreign_reference(&refs, "Vについて教えてください", &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_returns_the_first_qualifying_reference_among_several() {
        let refs = vec![
            // ambiguous は対象外
            ProductReference {
                surface: "ドアベル".to_string(),
                resolution: ProductReferenceResolution::Ambiguous,
                matched_model: None,
            },
            // 発話に存在しない foreign（幻覚）は対象外
            foreign_ref("ADC-XYZ999"),
            // 条件を満たす最初の1件
            foreign_ref("ADC-VDB101"),
            // 条件を満たす2件目（返らないことを確認する対照）
            foreign_ref("ADC-VDB201"),
        ];
        let message = "ADC-VDB101とADC-VDB201について教えてください";
        assert_eq!(
            confirmed_foreign_reference(&refs, message, &fixture_allowlist()),
            Some(&refs[2])
        );
    }

    #[test]
    fn confirmed_foreign_reference_matches_across_fullwidth_and_case_differences() {
        // surface "vdb101"（allowlist 外の型番）が message 中の全角 "ＶＤＢ１０１" に一致する
        // （NFKC + 小文字化）。以前は "v724" を使っていたが、Warning 4 是正で
        // `matches_in_scope_model` が追加され、"v724" は allowlist 内 "ADC-V724" のサフィックス
        // として veto される（別テスト `..._is_none_when_surface_is_a_suffix_of_an_in_scope_model`
        // で固定）ため、全半角/大小文字の一致ロジック自体は allowlist 外の型番で検証する。
        let refs = vec![foreign_ref("vdb101")];
        assert_eq!(
            confirmed_foreign_reference(
                &refs,
                "ＶＤＢ１０１について教えてください",
                &fixture_allowlist()
            ),
            Some(&refs[0])
        );
    }

    // ---- confirmed_foreign_reference: 実在チェックのハイフン正規化共有（Issue #28 Suggestion 1）----

    #[test]
    fn confirmed_foreign_reference_matches_when_surface_and_message_use_different_hyphen_variants()
    {
        // surface は Unicode 数学マイナス記号（U+2212、NFKC の対象外）、message は ASCII
        // ハイフンで同じ型番を書いている。`normalize_for_presence_check` がハイフン正規化を
        // 共有していないと、両者の実在チェックが一致せず、幻覚ガードで誤って None になる
        // （本来は実在する参照が見えなくなる false negative）。
        let refs = vec![foreign_ref("ADC\u{2212}VDB101")];
        assert_eq!(
            confirmed_foreign_reference(
                &refs,
                "ADC-VDB101について教えてください",
                &fixture_allowlist()
            ),
            Some(&refs[0])
        );
    }

    // ---- confirmed_foreign_reference: 反射安全性（Issue #28 codex Stage2 Warning 3 是正） ----

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_exceeds_the_max_reflectable_length() {
        // 65文字（上限64文字を1文字超える）の surface。message にも実在させ、長さ以外の条件は
        // 満たした状態で「長さだけ」が理由で発火しないことを確認する。
        let long_surface = "A".repeat(65);
        let message = format!("{long_surface}について教えてください");
        let refs = vec![foreign_ref(&long_surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_a_control_character() {
        // 改行を含む surface。message にも実在させる。
        let surface = "ADC-VDB101\nX";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_fires_when_surface_is_exactly_the_max_reflectable_length() {
        // 境界の固定: ちょうど64文字なら発火する。
        let surface = "A".repeat(64);
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(&surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            Some(&refs[0])
        );
    }

    // ---- confirmed_foreign_reference: 反射安全性の拡張（Issue #28 W2 是正） ----

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_u2028_line_separator() {
        let surface = "ADC-VDB101\u{2028}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_u2029_paragraph_separator() {
        let surface = "ADC-VDB101\u{2029}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_u200b_zero_width_space() {
        let surface = "ADC-VDB101\u{200B}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_ufeff_bom() {
        let surface = "ADC-VDB101\u{FEFF}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_u202a_left_to_right_embedding() {
        let surface = "ADC-VDB101\u{202A}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    // ---- confirmed_foreign_reference: 反射安全性の拒否対象への Cf 文字追加（修正3） ----

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_u061c_arabic_letter_mark() {
        let surface = "ADC-VDB101\u{061C}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_u180e_mongolian_vowel_separator() {
        let surface = "ADC-VDB101\u{180E}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_u206a_range_start() {
        let surface = "ADC-VDB101\u{206A}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_u206f_range_end() {
        let surface = "ADC-VDB101\u{206F}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_ufff9_range_start() {
        let surface = "ADC-VDB101\u{FFF9}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_ufffb_range_end() {
        let surface = "ADC-VDB101\u{FFFB}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    // ---- reflection_unsafe_char_regex: 拒否クラスへの追加（Issue #28 codex レビュー Critical/修正3） ----

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_u00ad_soft_hyphen() {
        // `normalize_hyphens` は U+00AD SOFT HYPHEN（ゼロ幅の書式制御文字、Cf）を除去するが、
        // `is_safe_to_reflect` は正規化前の生 surface に対して呼ばれるため、その除去には依存
        // できない。現行実装では U+00AD は `\p{Cf}` に含まれるため、個別の列挙なしにカテゴリ
        // 判定で拒否される。このテストはその挙動を固定する。
        let surface = "ADC-VDB101\u{00AD}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_ue0001_tags_block_start() {
        // U+E0001 LANGUAGE TAG（Cf、Tags ブロックの開始付近）。不可視テキスト埋め込みの定番
        // ベクタ。現行パターンが拒否するのは Tags ブロック（U+E0000〜U+E007F）のうち割り当て済み
        // Cf のコードポイント（U+E0001 と U+E0020〜U+E007F）のみで、U+E0000・U+E0002〜U+E001F は
        // 未割り当て（Cn）のため素通りする（reviewer 実測）。素通り分も Default_Ignorable では
        // あり不可視だが、現行 spec 規定（Cc/Cf/Zl/Zp）の範囲外の既知の限界として受容している
        // （塞ぐ判断は今回のスコープ外）。
        let surface = "ADC-VDB101\u{E0001}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_ue007f_tags_block_end() {
        // U+E007F CANCEL TAG（Tags ブロックの終端）。
        let surface = "ADC-VDB101\u{E007F}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_u0600_arabic_number_sign_not_in_old_enum_list(
    ) {
        // U+0600 ARABIC NUMBER SIGN（Cf）。旧列挙方式の拒否リストには含まれていなかった文字で、
        // 正規表現クラス方式（Issue #28 codex レビュー是正）が Unicode 一般カテゴリ Cf を網羅的に
        // 拒否することを確認する回帰テスト。
        let surface = "ADC-VDB101\u{0600}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_contains_u2065_unassigned_default_ignorable(
    ) {
        // U+2065 は Cn（未割り当て）だが Default_Ignorable であり、カテゴリ指定では拾えないため
        // 正規表現に明示追加している。この明示を外すとこのテストが red になる
        // （reviewer 実測指摘: 旧列挙方式の '\u{2060}'..='\u{2069}' 範囲からの退行是正）。
        let surface = "ADC-VDB101\u{2065}X";
        let message = format!("{surface}について教えてください");
        let refs = vec![foreign_ref(surface)];
        assert_eq!(
            confirmed_foreign_reference(&refs, &message, &fixture_allowlist()),
            None
        );
    }

    // ---- confirmed_foreign_reference: 製品マスタとの非矛盾（Issue #28 codex Stage2 Warning 4）----

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_is_an_in_scope_model_misclassified_as_foreign(
    ) {
        // LLM が allowlist 内の型番そのものを foreign と誤答しても、決定論の製品マスタが
        // 優先され veto される。
        let refs = vec![foreign_ref("ADC-V724")];
        assert_eq!(
            confirmed_foreign_reference(&refs, "ADC-V724の調子が悪いです", &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_is_a_lowercase_in_scope_model() {
        let refs = vec![foreign_ref("adc-v724")];
        assert_eq!(
            confirmed_foreign_reference(&refs, "adc-v724の調子が悪いです", &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_is_a_suffix_of_an_in_scope_model() {
        // "V724" は allowlist 内 "ADC-V724" の末尾（"ADC-" 接頭辞を省いたサフィックス）に
        // すぎない。誤って取扱外と断らないよう veto する（S-1 是正: `ends_with` 実装での
        // 実際の一致関係に合わせて名前・コメントを訂正。中間部分文字列の一致ではない）。
        let refs = vec![foreign_ref("V724")];
        assert_eq!(
            confirmed_foreign_reference(&refs, "V724の調子が悪いです", &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_still_fires_for_a_prefix_only_fragment_of_an_in_scope_model() {
        // W-4 是正: characterization test（既知の限界の固定）。"VC729" は取扱内
        // "ADC-VC729P" の末尾 "P" を欠いた断片で、条件2（surface 全体が allowlist 型番の
        // サフィックスか）にも条件3（"ADC-" 無し断片の "-{fragment}" サフィックス一致）にも
        // 一致しない（"VC729" は "ADC-VC729P" のサフィックスではない）。これは望ましい挙動
        // ではなく、取扱内顧客を誤って「取扱外」と断ってしまう方向の既知の限界を固定する
        // （spec §3.1 (d) のサフィックス一致規則に忠実であるため現状は受容する。
        // `ADC-VC727P` / `ADC-VC827P` も末尾 "P" を落とすと同じ限界に当たる）。
        let refs = vec![foreign_ref("VC729")];
        assert_eq!(
            confirmed_foreign_reference(&refs, "VC729について教えてください", &fixture_allowlist()),
            Some(&refs[0])
        );
    }

    #[test]
    fn confirmed_foreign_reference_still_fires_for_a_genuinely_out_of_scope_model() {
        // allowlist に無い型番は veto の対象外。従来どおり発火する（回帰）。
        let refs = vec![foreign_ref("ADC-VDB101")];
        assert_eq!(
            confirmed_foreign_reference(
                &refs,
                "ADC-VDB101について教えてください",
                &fixture_allowlist()
            ),
            Some(&refs[0])
        );
    }

    #[test]
    fn confirmed_foreign_reference_still_fires_for_free_text_unrelated_to_any_in_scope_model() {
        // 型番形式でない自由テキストで、取扱内型番と無関係なもの。veto されず従来どおり発火する。
        let refs = vec![foreign_ref("Ringのドアベル")];
        assert_eq!(
            confirmed_foreign_reference(
                &refs,
                "Ringのドアベルについて教えてください",
                &fixture_allowlist()
            ),
            Some(&refs[0])
        );
    }

    // ---- confirmed_foreign_reference: matched_model veto（Issue #28 W1-a 是正） ----

    #[test]
    fn confirmed_foreign_reference_is_none_when_matched_model_is_an_in_scope_model_even_if_surface_is_unrelated(
    ) {
        // surface 自体は allowlist と無関係だが、matched_model が取扱内型番を指す
        // 自己矛盾出力。matched_model の値だけで veto される（surface の実在チェック等の
        // 他条件は経由しない）。
        let refs = vec![ProductReference {
            surface: "Ringのドアベル".to_string(),
            resolution: ProductReferenceResolution::Foreign,
            matched_model: Some("ADC-V724".to_string()),
        }];
        assert_eq!(
            confirmed_foreign_reference(
                &refs,
                "Ringのドアベルについて教えてください",
                &fixture_allowlist()
            ),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_matched_model_is_a_bare_fragment_without_the_adc_prefix(
    ) {
        // matched_model が "ADC-" 接頭辞を欠く断片（"V724"）でも、allowlist の "ADC-V724" と
        // サフィックス一致するため veto される（修正1: surface と同じ matches_in_scope_model を
        // matched_model にも適用）。
        let refs = vec![ProductReference {
            surface: "Ringのドアベル".to_string(),
            resolution: ProductReferenceResolution::Foreign,
            matched_model: Some("V724".to_string()),
        }];
        assert_eq!(
            confirmed_foreign_reference(
                &refs,
                "Ringのドアベルについて教えてください",
                &fixture_allowlist()
            ),
            None
        );
    }

    // ---- matches_in_scope_model / confirmed_foreign_reference: 空文字ガード（Issue #28 W-1 是正） ----
    //
    // `"...".ends_with("")` は常に true になるため、空文字 surface / matched_model は無条件で
    // veto されてしまいうる。`llm.rs::parse_one_product_reference` 側で `matched_model` の
    // 空文字/空白は `None` へ落とすようにしたが（多層防御の1層目）、ここでは
    // `ProductAllowlist::matches_in_scope_model` 自身が持つ明示的なガード（2層目）を、
    // llm.rs の parse 層を経由しない直接構築の `ProductReference` で独立に固定する。

    #[test]
    fn matches_in_scope_model_is_false_for_an_empty_string() {
        assert!(!fixture_allowlist().matches_in_scope_model(""));
    }

    #[test]
    fn matches_in_scope_model_is_false_for_a_whitespace_only_string() {
        assert!(!fixture_allowlist().matches_in_scope_model("   "));
    }

    #[test]
    fn confirmed_foreign_reference_still_fires_when_matched_model_is_an_empty_string() {
        // llm.rs の trim-and-filter をバイパスして直接構築している点に注意（そちら側の
        // 検証は llm.rs::tests::parse_treats_a_blank_matched_model_as_none が別途固定する）。
        let refs = vec![ProductReference {
            surface: "Ringのドアベル".to_string(),
            resolution: ProductReferenceResolution::Foreign,
            matched_model: Some(String::new()),
        }];
        assert_eq!(
            confirmed_foreign_reference(
                &refs,
                "Ringのドアベルについて教えてください",
                &fixture_allowlist()
            ),
            Some(&refs[0])
        );
    }

    #[test]
    fn confirmed_foreign_reference_still_fires_when_matched_model_is_whitespace_only() {
        let refs = vec![ProductReference {
            surface: "Ringのドアベル".to_string(),
            resolution: ProductReferenceResolution::Foreign,
            matched_model: Some("   ".to_string()),
        }];
        assert_eq!(
            confirmed_foreign_reference(
                &refs,
                "Ringのドアベルについて教えてください",
                &fixture_allowlist()
            ),
            Some(&refs[0])
        );
    }

    // ---- confirmed_foreign_reference: ADC-無し型番断片の suffix veto（Issue #28 W1-b 是正） ----

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_is_a_bare_model_fragment_followed_by_text()
    {
        // "V724ドアベル" は「型番断片 + 後続テキスト」で、surface 全体のサフィックス
        // 一致判定（正規化した surface 全体が allowlist の型番の末尾か）では
        // 捕まえられない（正規化後の文字列が allowlist のどの型番よりも長くなるため）。
        // 新設の suffix veto（`extract_bare_fragments` + `ends_with("-{fragment}")`）で
        // 捕まえる。
        let refs = vec![foreign_ref("V724ドアベル")];
        assert_eq!(
            confirmed_foreign_reference(
                &refs,
                "V724ドアベルについて教えてください",
                &fixture_allowlist()
            ),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_is_an_in_scope_model_with_adc_prefix_followed_by_text(
    ) {
        // "ADC-V724カメラ" は `model_token_regex` の文字クラス（`[A-Z0-9-]`）が非ASCII文字
        // （カタカナ）で止まるため、`extract_model_tokens` は "ADC-V724" だけを抽出し、これは
        // 既存の条件1（allowlist 内トークン抽出）で veto される想定（reviewer 指示に明記された
        // 回帰）。この suffix veto（条件3）自体の新規挙動ではないが、明示的に固定する。
        let refs = vec![foreign_ref("ADC-V724カメラ")];
        assert_eq!(
            confirmed_foreign_reference(
                &refs,
                "ADC-V724カメラの調子が悪いです",
                &fixture_allowlist()
            ),
            None
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

    // ---- matches_in_scope_model: ADC-無し型番断片の suffix veto（Issue #28 W1-b 是正） ----

    #[test]
    fn matches_in_scope_model_true_for_a_bare_fragment_without_the_adc_prefix() {
        assert!(fixture_allowlist().matches_in_scope_model("V724"));
    }

    // ---- matches_in_scope_model: 断片照合の厳格化（contains → suffix, 3文字未満は不発） ----

    #[test]
    fn confirmed_foreign_reference_is_none_when_surface_is_a_three_char_fragment_matching_an_in_scope_suffix(
    ) {
        // "724" は3文字ちょうどでMIN_FRAGMENT_VETO_CHARSを満たし、"ADC-V724" とサフィックス一致
        // するため veto される（偶然一致の受容: doc comment 参照）。
        let refs = vec![foreign_ref("724")];
        assert_eq!(
            confirmed_foreign_reference(&refs, "724について教えてください", &fixture_allowlist()),
            None
        );
    }

    #[test]
    fn confirmed_foreign_reference_still_fires_for_a_non_suffix_two_char_fragment_v7() {
        // "V7" は "ADC-V724" のどの型番の末尾2文字でもない（実在する2文字末尾は "23"/"3X"/
        // "24"/"4X"/"9P"/"7P"）。`ends_with` 実装では最初からサフィックス不一致のため veto
        // されない。旧 `contains` 実装では "ADC-V724" の中間 "V7" に偶然一致して誤 veto して
        // いたが、`ends_with` への厳格化で是正され、真の foreign 参照として発火する
        // （W-2 是正: MIN_FRAGMENT_VETO_CHARS 未満という理由付けは誤りだったため訂正）。
        let refs = vec![foreign_ref("V7")];
        assert_eq!(
            confirmed_foreign_reference(&refs, "V7について教えてください", &fixture_allowlist()),
            Some(&refs[0])
        );
    }

    #[test]
    fn confirmed_foreign_reference_still_fires_for_a_non_suffix_two_char_fragment_ad() {
        // "AD" も同様にどの型番の末尾でもない。旧 `contains` 実装では "ADC-V724" の先頭 "AD" に
        // 偶然一致していたが、`ends_with` では先頭一致は無関係になるため veto されない
        // （W-2 是正: コメント訂正）。
        let refs = vec![foreign_ref("AD")];
        assert_eq!(
            confirmed_foreign_reference(&refs, "ADについて教えてください", &fixture_allowlist()),
            Some(&refs[0])
        );
    }

    #[test]
    fn confirmed_foreign_reference_still_fires_when_surface_is_a_two_char_fragment_matching_an_in_scope_suffix(
    ) {
        // MIN_FRAGMENT_VETO_CHARS そのものを検証する回帰（W-2 是正）。"24" は実在する
        // "ADC-V724" の末尾2文字（真のサフィックス）だが、2文字は MIN_FRAGMENT_VETO_CHARS(3)
        // 未満のため対象外となり veto されず発火する。この定数を 2 以下へ下げると
        // "ADC-V724".ends_with("24") が条件2で拾われて veto され、このテストは red になる
        // （実装を一時的に MIN_FRAGMENT_VETO_CHARS = 2 へ書き換えて red を確認済み。復元済み）。
        let refs = vec![foreign_ref("24")];
        assert_eq!(
            confirmed_foreign_reference(&refs, "24について教えてください", &fixture_allowlist()),
            Some(&refs[0])
        );
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
    fn first_out_of_scope_token_is_none_for_a_mixed_case_in_scope_model() {
        // 全面 case-insensitive マッチの実効テスト: 大文字小文字混在の型番表記
        // （"Adc-V724x"）でも正規化後に allowlist の "ADC-V724X" と一致する。
        // fixture_allowlist は ADC-V724X を含む。
        let allow = fixture_allowlist();
        assert_eq!(
            allow.first_out_of_scope_token("Adc-V724xの調子はどうですか"),
            None
        );
    }

    #[test]
    fn first_out_of_scope_token_c1_regression_at_the_gate_composition_level() {
        // これは C-1（fail-open）の回帰テストであり、抽出レベル
        // （`extract_model_tokens_finds_the_full_lowercase_token_with_a_multi_char_suffix`）だけ
        // でなく、実際に本番で壊れた層（allowlist との突合を含むゲート合成）でも固定する。
        // 顧客が小文字・複数文字サフィックスで書いた型番（"adc-v521ir"）が §3.1 質問側ゲートを
        // 素通りせず、取扱外として検出されること。fixture_allowlist は ADC-V521IR を含まない。
        let allow = fixture_allowlist();
        assert_eq!(
            allow.first_out_of_scope_token("adc-v521irが動きません"),
            Some("ADC-V521IR".to_string())
        );
    }

    // ---- out_of_scope_mentions（Issue #28 codex レビュー採用5） ----

    #[test]
    fn out_of_scope_mentions_returns_all_distinct_out_of_scope_models_in_order() {
        let allow = fixture_allowlist();
        assert_eq!(
            allow.out_of_scope_mentions("ADC-VDB101とADC-VDB201のどちらでも使えますか"),
            vec!["ADC-VDB101".to_string(), "ADC-VDB201".to_string()]
        );
    }

    #[test]
    fn out_of_scope_mentions_is_empty_when_only_in_scope_models_are_mentioned() {
        let allow = fixture_allowlist();
        assert!(allow
            .out_of_scope_mentions("ADC-V724とADC-V523の違いは何ですか")
            .is_empty());
    }

    #[test]
    fn out_of_scope_mentions_is_empty_when_no_model_is_mentioned() {
        // 修正2で撤去した `has_out_of_scope_mention` の単体テストが検証していた3述語のうち
        // 「型番なしのとき偽（＝ここでは空）になる」ケースを移植する（テストの意味を失わせない）。
        let allow = fixture_allowlist();
        assert!(allow
            .out_of_scope_mentions("型番の記載はありません")
            .is_empty());
    }

    #[test]
    fn out_of_scope_mentions_deduplicates_repeated_mentions() {
        let allow = fixture_allowlist();
        assert_eq!(
            allow.out_of_scope_mentions("ADC-VDB101について。もう一度、ADC-VDB101について教えて"),
            vec!["ADC-VDB101".to_string()]
        );
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

    /// `fut` の実行中に出た WARN 以上のログを文字列で返す（`harness::reply` の
    /// `capture_warnings` の非同期版）。capture 機構本体（グローバル subscriber の 1 回
    /// インストール + スレッドローカルバッファ）は `test_support` を参照。Dispatch を
    /// 差し替える旧方式は、capture 機構を使わないテストが先に無介入で同じコールサイトを
    /// 叩くと interest cache が「無効」に確定し手遅れになる問題があったため廃止した
    /// （詳細は `test_support` の doc コメント）。`#[tokio::test]` の既定（current-thread）
    /// ランタイムで `.await` をまたいでも同じスレッド上で実行される限り有効。
    async fn capture_warnings<Fut, T>(fut: Fut) -> (T, String)
    where
        Fut: std::future::Future<Output = T>,
    {
        crate::test_support::capture_logs_async(fut).await
    }

    /// `capture_warnings` の同期版。`ProductAllowlist::from_models` のような同期関数（Stage1
    /// Suggestion 4 の trim 除外 warn）を検証するために使う。
    fn capture_warnings_sync<F, T>(f: F) -> (T, String)
    where
        F: FnOnce() -> T,
    {
        crate::test_support::capture_logs(f)
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

    /// `model` に加えて `name` 属性も持つ Product ノード（W-3 是正のテスト用: name が
    /// 一致判定・表示のどちらにも一切現れないことを検証するために使う）。
    fn product_node_with_name(model: &str, name: &str) -> crate::proto::graphrag::NodeResult {
        let mut attributes = HashMap::new();
        attributes.insert("model".to_string(), model.to_string());
        attributes.insert("name".to_string(), name.to_string());
        crate::proto::graphrag::NodeResult {
            node_id: format!("urtect:gen1:Product:{model}"),
            node_type: KIND_PRODUCT.to_string(),
            attributes,
        }
    }

    // ---- ProductAllowlist::from_nodes: name は一致判定に含めない（W-3 是正） ----

    #[test]
    fn from_nodes_ignores_the_name_attribute_and_builds_the_allowlist_from_model_only() {
        // 以前は name 属性も正規化して一致判定（`normalized`）へマージしていたが、W-3 是正で
        // 撤去した（`extract_model_tokens` は常に「ADC-」形式のトークンしか生成しないため、
        // name 由来のマージは実利用上到達しない不要な複雑さだった）。意図的に model と異なる
        // name を使い、name 由来の値が一致判定にも表示文字列にも一切現れないことを証明する。
        let allowlist = ProductAllowlist::from_nodes(vec![product_node_with_name(
            "ADC-V724",
            "V724 Outdoor Camera",
        )]);
        assert!(
            !allowlist.is_in_scope(&normalize_model_token("V724 Outdoor Camera")),
            "the 'name' attribute must not be merged into the in-scope match set"
        );
        assert_eq!(
            allowlist.display_list(),
            "ADC-V724",
            "the display string must be built from 'model' only, never from 'name': {}",
            allowlist.display_list()
        );
    }

    #[test]
    fn from_nodes_allowlist_gates_in_scope_and_out_of_scope_models_via_the_real_call_path() {
        // W-3 の置き換えテスト: `from_nodes` で構築した allowlist を、実際の呼び出し経路
        // （`first_out_of_scope_token`）に通して end-to-end で検証する。
        let allowlist = ProductAllowlist::from_nodes(vec![product_node("ADC-V724")]);
        assert_eq!(
            allowlist.first_out_of_scope_token("ADC-V724の調子はどうですか"),
            None
        );
        assert_eq!(
            allowlist.first_out_of_scope_token("ADC-V999の調子はどうですか"),
            Some("ADC-V999".to_string())
        );
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

    // ---- reflection_unsafe_char_regex / is_safe_to_reflect: 直接テスト（Issue #28 codex
    // レビュー2巡目 Suggestion 採用）。上の "reflection_unsafe_char_regex: 拒否クラスへの追加"
    // セクションは `confirmed_foreign_reference` 越しの間接テストで、しかも危険文字を含む
    // surface が拒否される異常系のみを固定していた。ここでは正規表現本体と
    // `is_safe_to_reflect` を直接呼び、(a) 通常 surface が安全と判定される正常系、
    // (b) パターンを構成するリテラル文字自体は拒否されないこと、(c) 拒否対象の各カテゴリの
    // 代表がマッチすること、(d) 長さ境界を固定する。もしパターンからバックスラッシュが
    // 欠落して Unicode property escape ではなくただの文字集合に退化しても、(a)/(b) がこの
    // セクションの中で直接 red になる。

    #[test]
    fn is_safe_to_reflect_accepts_ordinary_surfaces() {
        // これはパターンが Unicode property escape として解釈されていることの直接的な検証で
        // ある。もしバックスラッシュが落ちてただの文字集合（`[pCcpCfpZlpZpu2065]` 相当）に
        // なると、'C' や 'p' や '{' を含む通常の surface が誤って危険文字扱いされ、この
        // テストが red になる。
        assert!(is_safe_to_reflect("ADC-VDB101X"));
        assert!(is_safe_to_reflect("ADC-V724"));
        assert!(is_safe_to_reflect("Ringのドアベル"));
        assert!(is_safe_to_reflect("ＡＤＣ－Ｖ７２４"));
    }

    #[test]
    fn reflection_unsafe_char_regex_does_not_match_the_literal_characters_of_its_own_pattern() {
        // パターンを構成する文字（p, {, }, C, c, f, Z, l, u, 数字）自体はリテラルとしては
        // 拒否対象ではない。バックスラッシュ欠落による退化（Unicode property escape → ただの
        // 文字集合）を直接検出する回帰テストである。
        assert!(!reflection_unsafe_char_regex().is_match("p{Cc}p{Cf}p{Zl}p{Zp}u{2065}"));
    }

    #[test]
    fn reflection_unsafe_char_regex_matches_each_rejected_category() {
        assert!(reflection_unsafe_char_regex().is_match("\u{0009}")); // Cc（TAB）
        assert!(reflection_unsafe_char_regex().is_match("\u{00AD}")); // Cf（SOFT HYPHEN）
        assert!(reflection_unsafe_char_regex().is_match("\u{0600}")); // Cf（ARABIC NUMBER SIGN）
        assert!(reflection_unsafe_char_regex().is_match("\u{2028}")); // Zl（LINE SEPARATOR）
        assert!(reflection_unsafe_char_regex().is_match("\u{2029}")); // Zp（PARAGRAPH SEPARATOR）
        assert!(reflection_unsafe_char_regex().is_match("\u{2065}")); // Cn だが明示追加（Default_Ignorable）
    }

    #[test]
    fn is_safe_to_reflect_rejects_surfaces_longer_than_the_reflectable_limit() {
        assert!(is_safe_to_reflect(
            &"あ".repeat(MAX_REFLECTABLE_SURFACE_CHARS)
        ));
        assert!(!is_safe_to_reflect(
            &"あ".repeat(MAX_REFLECTABLE_SURFACE_CHARS + 1)
        ));
    }
}
