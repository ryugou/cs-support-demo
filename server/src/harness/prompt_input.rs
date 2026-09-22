//! LLM プロンプトへ渡す前の入力整形を集約する共通モジュール（Warning 4 の集約先）。
//!
//! `reply.rs` / `clarify.rs` / `escalation_reply.rs` の 3 モジュールはいずれも「顧客本文などの
//! 信頼できない入力を LLM プロンプトへ埋め込む」という同じ問題を扱う。以前は
//! `neutralize_delimiters`・問い合わせ本文の切り詰め・egress gate フォールバックの分岐が
//! 3 箇所へ複製されており、片方だけ強化したときに他が黙って取り残される構造だった。
//! ここへ集約し、3 モジュールはこのモジュールの関数を呼ぶだけにする。
//!
//! （集約前の `clarify.rs` の doc コメントには「`pub(crate)` が本モジュールを横断できないため
//! ここに複製する」と書かれていたが、これは誤り。`pub(crate)` はクレート全体・兄弟モジュール
//! から見える。複製の実際の理由は単に「まだ集約していなかった」だけである。）

use crate::harness::egress::{self, EgressVerdict, EmitContext, NgDictionary};

/// 顧客からの問い合わせ本文（質問文）の最大文字数。
///
/// `harness::reply::MAX_EXCERPT_CHARS` で資料側（マニュアル抜粋・known_resolution 本文）を
/// 切っているのに、より信用できない入力である問い合わせ本文が無制限なのは筋が通らない。
/// 注入面積・コスト・レイテンシに効く。**具体値をここ以外に書かないのは、片方を変えたときに
/// 他方の doc が腐るため。**
pub(crate) const MAX_QUESTION_CHARS: usize = 2000;

/// 会話継続時（`is_continuation == true`）に聞き返し・受け止め文・回答下書きプロンプトへ
/// 追加する system prompt 用の 1 行（会話フロー v1.1 design doc §3）。`clarify.rs` /
/// `escalation_reply.rs` / `reply.rs`（顧客向け回答下書き）の 3 モジュールが使う。以前この
/// 種の文字列が複数モジュールへ複製され drift した経緯（本モジュール冒頭のdoc参照）を
/// 繰り返さないためここに集約する。
pub(crate) const CONTINUATION_OPENER_RULE: &str =
    "- 継続中の会話です。挨拶・感謝・謝罪の定型オープナー（「いつもご利用いただき〜」\
     「ご不便をおかけして〜」等）は書かず、直前のやり取りを受けて本題から書き始める。\n";

/// 会話終了を示唆する定型文言の説明句（会話フロー v1.1 design doc §3 の (2) および
/// 「クローザーの扱い」）。`clarify.rs`（聞き返し、常時禁止）と `reply.rs`
/// （回答下書き Answer 分岐、常時禁止 + 継続を誘う一文への差し替え指示）が、
/// それぞれの文脈の文へこの語句を埋め込む。
pub(crate) const CLOSER_BAN_PHRASE: &str =
    "感謝・締め・「何かあればお申し付けください」等、会話の終了を示唆する文言";

/// 生成プロンプトへ Markdown 記法の禁止を伝える共通ルール（Issue #27: LINE は Markdown を
/// 描画しないため、生成物に `**太字**` 等が混じるとそのまま記号として顧客に表示される）。
/// `clarify.rs` / `escalation_reply.rs` / `reply.rs`（顧客向け回答下書き）の 3 モジュールが、
/// それぞれの system prompt を組み立てる関数の「共通ルール」ブロック（`is_continuation` 分岐
/// より前、常に実行される位置）へ差し込む。
///
/// **これはプロンプト側の抑止であり、保証ではない**。実際の保証は
/// `/api/reply` の応答確定点（`api.rs::to_plain_text` 呼び出し）側のコードによる正規化が担う
/// （design doc `2026-08-11-answer-api-line-adapter-design.md` §2 の「二段構え」）。ここが
/// 抜けても出口側の正規化で救えるが、モデルが素直に Markdown を書かなくなる分だけ
/// `truncate_chars` 等の文字数カウントとの齟齬（`**` がそのまま字数を消費する等）も減るため、
/// 両方を維持する。
pub(crate) const MARKDOWN_BAN_RULE: &str =
    "- Markdown 記法（太字・見出し・箇条書き記号・コードブロック・リンク記法）を使わない。\
     プレーンテキストのみで書く。強調したい語は記号ではなく語順と文で表現する。\n";

/// 制御文字（改行・復帰等）を半角スペースへ潰し、Unicode 空白（`char::is_whitespace()` が
/// 拾う U+2028 / U+2029 等を含む）で連続分割してから半角スペース 1 つで再結合し、trim する。
/// 「1 値 = 必ず 1 行」という不変条件を、呼び出し側ごとに個別実装させず共通化するための関数。
///
/// 切り詰めは行わない。長さの制約が必要な呼び出し側は、この関数の戻り値に対して
/// [`truncate_chars`] 等を別途重ねること。
///
/// `api::normalize_customer_turn_to_single_line`（顧客発話。改行で偽の箇条書き行を
/// 注入されないため）、`api::select_customer_history_for_jev` / `api::build_jev_state`
/// （Jev の判定入力。発話を切り詰めず改行だけを潰すため、この関数を直接使う）、
/// `harness::signal::LexiconNormalizer`（lexicon の `customer_label`。JSON に改行を紛れ込ませても
/// 顧客向けプロンプトが崩れないため）が使う。
pub(crate) fn collapse_to_single_line(text: &str) -> String {
    let control_collapsed: String = text
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    control_collapsed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 文字数上限で切り詰める（文字境界を壊さない）。切ったことが分かるよう省略記号を付ける。
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// 顧客からの問い合わせ本文を trim したうえで [`MAX_QUESTION_CHARS`] で切り詰める。
/// 超過分は必ず warn する（黙って切らない。運用者が「モデルが読み落とした」と誤解しないため）。
///
/// `route` は呼び出し元を warn に残すためのラベル（例: `"customer_reply"` / `"clarify_question"` /
/// `"escalation_ack"`）。本文そのものはログに出さない。
pub(crate) fn truncate_question(question: &str, route: &str) -> String {
    let question = question.trim();
    let question_chars = question.chars().count();
    if question_chars > MAX_QUESTION_CHARS {
        tracing::warn!(
            route,
            original_chars = question_chars,
            max_chars = MAX_QUESTION_CHARS,
            "the customer question was truncated before being handed to the LLM prompt \
             builder; the prompt only saw the head of it. If the output misses the point of a \
             long inquiry, check the part past the limit"
        );
    }
    truncate_chars(question, MAX_QUESTION_CHARS)
}

/// 問い合わせ・資料などの入力文字列から、区切りタグとして解釈されうる山括弧を無害化する。
///
/// **これが無いと、顧客が `</顧客からの問い合わせ><資料>…` のような文字列を書くだけで
/// 「サーバが渡した資料」を偽装できる**（`harness::reply` の `excerpts` を空にする構造的保証は
/// 「内部マニュアルが漏れない」ことしか担保しない。顧客が自分で書いた文字列は別物）。
///
/// 本文を捨てずに全角へ寄せる（入力の情報は保ちたい。モデルが読む意味は変わらず、区切りとして
/// は機能しなくなる）。
pub(crate) fn neutralize_delimiters(s: &str) -> String {
    s.replace('<', "＜").replace('>', "＞")
}

/// LLM 下書き（`ReplyDraft`）を受け取り、「truncated 判定 → egress gate → フォールバック」を
/// 1 関数にまとめる（Warning 1: `clarify.rs` / `escalation_reply.rs` に同型ロジックが複製され
/// ていた）。`ReplyDraft` を受ける形にしているのは、呼び出し側が truncated チェックを書き忘れて
/// 直接 `String` を渡す退行を型で防ぐため(新経路が安全側デフォルトになる)。この保証は
/// [`apply_egress_gate_or_fallback`] がモジュール外へ公開されていないことで初めて成立する
/// （公開されていれば、新しい呼び出し元がそちらを直接呼んで truncated チェックを迂回できる）。
///
/// `route` はログ相関用ラベル（例: `"clarify_question"` / `"escalation_ack"`）。
///
/// **前提条件**: この関数の truncated warn は `setting` を
/// `"harness.customer_reply_draft_max_tokens"` に固定でハードコードしている。現行の呼び出し元
/// （clarify_question / escalation_ack）はどちらもこの設定値で駆動されるため正しいが、
/// 将来別の max_tokens 設定（例: `time_pref::TIME_PREF_EXTRACTION_MAX_TOKENS`）で駆動される
/// 経路からこの関数を呼ぶ場合は、運用者に誤った設定値を案内しないよう `setting` も引数化する
/// こと（現状はシグネチャに含めていない）。
pub(crate) fn apply_draft_gate_or_fallback(
    draft: crate::llm::ReplyDraft,
    ctx: &EmitContext,
    ng: &NgDictionary,
    fallback: &str,
    fallback_name: &str,
    route: &str,
    inspect_hint: &str,
) -> String {
    // 生成上限で途中切断された下書きは、切れ目次第で完成文に見えることがあり、egress gate
    // （NG 語のブロックリストマッチ）では検知できない。egress gate に通す前にここで
    // フォールバックへ倒す。
    if draft.truncated {
        tracing::warn!(
            route,
            setting = "harness.customer_reply_draft_max_tokens",
            draft_chars = draft.text.chars().count(),
            "draft hit max_tokens and is cut off; it could look like a complete sentence \
             depending on where it was cut, so the egress gate (which only matches NG terms, \
             not sentence completeness) cannot catch it. Falling back to {fallback_name}. Raise \
             harness.customer_reply_draft_max_tokens if this recurs (shared by the customer \
             reply / clarify question / escalation ack routes; raising it also raises the \
             customer reply route's output cap, cost, and latency)"
        );
        return fallback.to_string();
    }
    apply_egress_gate_or_fallback(draft.text, ctx, ng, fallback, fallback_name, inspect_hint)
}

/// 生成結果を egress gate に通し、block/abstain 時は warn してフォールバック文字列へ倒す。
/// gate 判定 → フォールバック分岐の純粋ロジックだけを独立させ、実際の Anthropic API 呼び出しを
/// 伴わずにテストできるようにする。
///
/// `fallback` はフォールバック先の文字列そのもの、`fallback_name` はログに残す定数名
/// （例: `"FALLBACK_CLARIFY_TEXT"`）、`inspect_hint` は「何を調べればよいか」（例:
/// `"the question/missing material"`）。呼び出し元ごとに異なるこの 3 つだけを引数化し、
/// warn の情報量（verdict / term / draft_chars / 次のアクション）は落とさない。
fn apply_egress_gate_or_fallback(
    text: String,
    ctx: &EmitContext,
    ng: &NgDictionary,
    fallback: &str,
    fallback_name: &str,
    inspect_hint: &str,
) -> String {
    match egress::egress_gate(&text, ctx, ng) {
        EgressVerdict::Pass => text,
        ref blocked => {
            let term = match blocked {
                EgressVerdict::Block { term } | EgressVerdict::Abstain { term } => term.as_str(),
                EgressVerdict::Pass => "",
            };
            tracing::warn!(
                verdict = blocked.label(),
                term,
                draft_chars = text.chars().count(),
                "draft was blocked by the egress gate; falling back to {fallback_name}. \
                 Inspect {inspect_hint} behind this generation — the draft contained a term \
                 the NG dictionary rejects"
            );
            fallback.to_string()
        }
    }
}

/// LLM が生成した応答文からプレーンテキストへ正規化する（Issue #27）。
///
/// `/{project_id}/api/reply` の `reply_text` は design doc
/// `2026-08-11-answer-api-line-adapter-design.md` §2 で「必ずプレーンテキスト」と定めている
/// （LINE は Markdown を描画しないため、`**太字**` のような記法がそのまま記号として顧客に
/// 見えてしまう）。[`MARKDOWN_BAN_RULE`] で生成プロンプト側にも禁止を伝えるが、モデルが
/// 指示を無視して Markdown を書く場合に備え、応答確定点でコードによる正規化を必ず通す
/// （二段構えのうち保証を担う方）。**MCP 経路（CS 担当が検分する下書き）には適用しない**
/// （design doc 同節）。
///
/// 対応する規則（design doc §2 のリスト）:
/// - 太字/斜体マーカー `**` / `__` / `*` / `_` の対除去（対にならない・英数字に挟まれた
///   単語内部のマーカーは除去しない。型番 `URT_2_A` 等を壊さないため）
/// - 未閉じの `**` / `__`（対にならない太字マーカー）も最終的には除去される（顧客に記号を
///   見せないため。`strip_double_marker` 単体は残すが、後段の `strip_single_marker` が同じ
///   文字を単独マーカー 2 個として拾う。詳細は `strip_double_marker` の doc コメントを参照）
/// - 行頭 `#`（1〜6 個 + 半角スペース 1 個以上）の見出し記号を除去
/// - インラインコード `` `code` `` の対除去、コードフェンス行（\`\`\` 始まり）の除去
/// - `[text](url)` → `text（url）`
/// - 行頭の `-`/`*` 箇条書きマーカーを「・」へ
/// - 改行は保持し、番号付きリストはそのまま残す
///
/// この変換のためだけに正規表現クレートを足すのは割に合わないと判断し、標準ライブラリの
/// 文字列処理だけで行単位に処理し、`\n` で再結合する（仕様がこれを要求しているわけではない）。
pub(crate) fn to_plain_text(text: &str) -> String {
    // コードフェンス行（\`\`\` 始まり）は出力から除去するだけで、開閉状態は保持しない
    // （design doc §2: フェンス行の除去。フェンスの「内側」を特別扱いする規則は無い）。
    // 以前は `in_fence` フラグで開閉を追跡し、内側の行を transform_line に通さずそのまま
    // 出力していたため、フェンス内の `**太字**` 等がプレーンテキスト化されずに顧客へ届く
    // 不具合があった（Issue #27 reviewer 指摘）。加えて閉じないフェンスが 1 行でも出ると
    // `in_fence` が true のまま残り、それ以降のメッセージ全体が正規化されなくなっていた。
    // 状態を持たない実装にすることで、この経路自体を構造的に無くす。
    text.split('\n')
        .filter(|line| !line.trim().starts_with("```"))
        .map(transform_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// 1 行分の Markdown 記法を除去・変換する。呼び出し順序は「行構造の判定（見出し・箇条書き）
/// → インライン記法（強調・コード・リンク）」。行構造の判定を先に行うのは、箇条書きマーカー
/// `* ` が後続の斜体除去に誤って拾われないようにするため（`convert_list_marker` が `* ` を
/// 「・」へ置き換えた後は、斜体除去の対象になる単独の `*` が残らない）。
fn transform_line(line: &str) -> String {
    let line = strip_heading_marker(line);
    let line = convert_list_marker(&line);
    // 太字（2 文字マーカー）を先に処理する。単独マーカー除去を先に走らせると、
    // `**text**` の 4 つの `*` がそれぞれ独立した対として誤って処理されうるため、
    // 「対にならないマーカーは除去しない」規則を明示的に守るには 2 文字マーカーを
    // 独立した規則として先に消費するほうが読んで意図が分かる。
    let line = strip_double_marker(&line, '*');
    let line = strip_double_marker(&line, '_');
    let line = strip_single_marker(&line, '*');
    let line = strip_single_marker(&line, '_');
    let line = strip_inline_code(&line);
    convert_links(&line)
}

/// 行頭（先頭の空白を除いた位置）の `#` 見出し記号（1〜6 個 + 半角スペース 1 個以上）を除去し、
/// 見出しテキストだけを残す。`#` の直後に半角スペースが無い場合（Markdown の見出しとして
/// 機能しない）は変更しない。
fn strip_heading_marker(line: &str) -> String {
    let leading_ws_len: usize = line
        .chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| c.len_utf8())
        .sum();
    let rest = &line[leading_ws_len..];
    let hash_count = rest.chars().take_while(|&c| c == '#').count();
    if hash_count == 0 || hash_count > 6 {
        return line.to_string();
    }
    let after_hashes = &rest[hash_count..];
    let space_count = after_hashes.chars().take_while(|&c| c == ' ').count();
    if space_count == 0 {
        return line.to_string();
    }
    let heading_text = &after_hashes[space_count..];
    format!("{}{heading_text}", &line[..leading_ws_len])
}

/// 行頭（先頭の空白を除いた位置）の `- ` または `* ` 箇条書きマーカーを「・」へ置き換える。
/// 行頭空白・後続本文はそのまま保持する。番号付きリスト（`1. foo` 等）はこの規則に該当せず
/// 変更されない。
fn convert_list_marker(line: &str) -> String {
    let leading_ws_len: usize = line
        .chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| c.len_utf8())
        .sum();
    let leading = &line[..leading_ws_len];
    let rest = &line[leading_ws_len..];
    if let Some(remainder) = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")) {
        return format!("{leading}・{remainder}");
    }
    line.to_string()
}

/// `**...**` / `__...__` のように 2 文字連続したマーカーの対を探し、マーカーだけを除去して
/// 中身を残す。閉じ側が見つからない（対にならない）場合は、それ以降を変更せずそのまま残す
/// （「対にならないマーカーは除去しない」規則）。**これはこの関数単体の責務の説明であり、
/// `to_plain_text` 全体の最終的な挙動を保証するものではない。** `transform_line` はこの関数の
/// 直後に `strip_single_marker(&line, marker)` を走らせており、そちらは単独マーカーを出現順に
/// 2 個ずつ対にして除去する。この関数が残した未閉じの `**` / `__` は、`strip_single_marker` から
/// 見ると単に同じ文字が 2 個連続しているだけなので、単独マーカー 2 個の対として拾われ除去される
/// （例: `to_plain_text("**未閉じ")` は `"未閉じ"` になる）。結果として、`to_plain_text` 全体では
/// 未閉じの `**` / `__` も最終的に除去される。詳細は `to_plain_text` の doc コメントと
/// `to_plain_text_removes_unclosed_double_markers` テストを参照。
fn strip_double_marker(line: &str, marker: char) -> String {
    let double: String = [marker, marker].into_iter().collect();
    let mut result = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        let Some(open_rel) = rest.find(&double) else {
            result.push_str(rest);
            break;
        };
        let after_open = &rest[open_rel + double.len()..];
        let Some(close_rel) = after_open.find(&double) else {
            // 開きマーカーの後に閉じマーカーが無い。ここから先は変更せず残す。
            result.push_str(rest);
            break;
        };
        result.push_str(&rest[..open_rel]);
        result.push_str(&after_open[..close_rel]);
        rest = &after_open[close_rel + double.len()..];
    }
    result
}

/// 単独の `*` / `_` マーカーの対を探して除去する。CommonMark のワード内強調禁止規則に倣い、
/// マーカーの直前直後が「単語の一部」とみなせる文字（`_` は ASCII 英数字またはアンダースコア、
/// `*` は ASCII 英数字）に両側とも挟まれている場合は、そのマーカーを対の候補にしない
/// （`URT_2_A` や `snake_case_name` のような型番・識別子を壊さないため）。
///
/// 加えて、マーカーの直前直後が両方とも空白文字（`char::is_whitespace()`）の場合も対の候補に
/// しない（CommonMark: 両側が空白のマーカーは開始子にも終了子にもなり得ない。`5 * 3` の乗算
/// 記号のような用法を誤って除去しないため。reviewer 指摘 Warning）。行頭・行末で隣接文字が
/// 存在しない側は空白扱いにしない（既存の境界挙動を変えないため。`is_word_flank` 側の
/// `i > 0` / `i + 1 < chars.len()` ガードと同じ考え方）。
///
/// 候補として残ったマーカー出現位置を出現順に 2 個ずつ対にして除去する。奇数個残った場合、
/// 最後の 1 個は対にならないため除去しない。
fn strip_single_marker(line: &str, marker: char) -> String {
    let chars: Vec<char> = line.chars().collect();
    let is_word_flank = |c: char| -> bool {
        if marker == '_' {
            c.is_ascii_alphanumeric() || c == '_'
        } else {
            c.is_ascii_alphanumeric()
        }
    };
    let mut candidates: Vec<usize> = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        if c != marker {
            continue;
        }
        let prev_blocks = i > 0 && is_word_flank(chars[i - 1]);
        let next_blocks = i + 1 < chars.len() && is_word_flank(chars[i + 1]);
        if prev_blocks && next_blocks {
            continue; // 単語内部のマーカー。対の候補にしない。
        }
        let prev_space = i > 0 && chars[i - 1].is_whitespace();
        let next_space = i + 1 < chars.len() && chars[i + 1].is_whitespace();
        if prev_space && next_space {
            continue; // 両側が空白のマーカー。開始子にも終了子にもなり得ない。対の候補にしない。
        }
        candidates.push(i);
    }
    let mut remove = vec![false; chars.len()];
    let mut pairs = candidates.chunks_exact(2);
    for pair in &mut pairs {
        remove[pair[0]] = true;
        remove[pair[1]] = true;
    }
    // `chunks_exact` は余り（奇数個目の孤立候補）を自動的に無視するため、
    // 最後の 1 個は `remove` に立てられず除去されない（対にならないマーカーの保持）。
    chars
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !remove[*i])
        .map(|(_, c)| c)
        .collect()
}

/// インラインコード `` `code` `` の対を探し、バッククォートだけを除去して中身を残す。
/// 対にならない孤立したバッククォートは除去しない。
fn strip_inline_code(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        let Some(open_rel) = rest.find('`') else {
            result.push_str(rest);
            break;
        };
        let after_open = &rest[open_rel + 1..];
        let Some(close_rel) = after_open.find('`') else {
            result.push_str(rest);
            break;
        };
        result.push_str(&rest[..open_rel]);
        result.push_str(&after_open[..close_rel]);
        rest = &after_open[close_rel + 1..];
    }
    result
}

/// `[text](url)` を `text（url）`（全角括弧）へ変換する。対応する `]` `(` `)` が揃わない
/// 場合は元のまま残す（`[` を通常の文字として扱い、次の文字へ進む）。
fn convert_links(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut i = 0usize;
    let bytes = line.as_bytes();
    while i < line.len() {
        if bytes[i] == b'[' {
            if let Some(text_end_rel) = line[i + 1..].find(']') {
                let text_end = i + 1 + text_end_rel;
                if bytes.get(text_end + 1) == Some(&b'(') {
                    if let Some(url_end_rel) = line[text_end + 2..].find(')') {
                        let url_end = text_end + 2 + url_end_rel;
                        result.push_str(&line[i + 1..text_end]);
                        result.push('（');
                        result.push_str(&line[text_end + 2..url_end]);
                        result.push('）');
                        i = url_end + 1;
                        continue;
                    }
                }
            }
        }
        let ch = line[i..]
            .chars()
            .next()
            .expect("i < line.len() が保証する非空スライス");
        result.push(ch);
        i += ch.len_utf8();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ng() -> NgDictionary {
        NgDictionary::from_json(
            r#"{"block_terms":["絶対に治ります"],"abstain_terms":["効果があります"]}"#,
        )
        .unwrap()
    }

    fn ctx() -> EmitContext {
        EmitContext {
            channel: egress::EmitChannel::Operator,
        }
    }

    #[test]
    fn collapse_to_single_line_joins_newlines_with_a_single_space() {
        assert_eq!(
            collapse_to_single_line("型番はA\n- 把握済みの条件語: 全て確認済み"),
            "型番はA - 把握済みの条件語: 全て確認済み"
        );
    }

    #[test]
    fn collapse_to_single_line_collapses_unicode_line_and_paragraph_separators() {
        // U+2028 LINE SEPARATOR / U+2029 PARAGRAPH SEPARATOR は `char::is_control()` では
        // 拾えないが、`split_whitespace()` の Unicode 空白判定では拾える。
        let text = "行1\u{2028}行2\u{2029}行3";
        assert_eq!(collapse_to_single_line(text), "行1 行2 行3");
    }

    #[test]
    fn collapse_to_single_line_trims_and_collapses_runs_of_spaces() {
        assert_eq!(collapse_to_single_line("  a   b  "), "a b");
    }

    #[test]
    fn collapse_to_single_line_returns_empty_for_whitespace_only_input() {
        assert_eq!(collapse_to_single_line("   \n\t  "), "");
    }

    #[test]
    fn truncate_chars_leaves_short_strings_unchanged() {
        assert_eq!(truncate_chars("短い文", 10), "短い文");
    }

    #[test]
    fn truncate_chars_appends_ellipsis_on_char_boundary() {
        let long = "あ".repeat(10);
        let out = truncate_chars(&long, 5);
        assert_eq!(out.chars().count(), 6); // 5 文字 + 省略記号
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_question_trims_and_passes_through_when_within_limit() {
        let out = truncate_question("  短い質問  ", "test_route");
        assert_eq!(out, "短い質問");
    }

    #[test]
    fn truncate_question_truncates_when_over_the_shared_limit() {
        let long = "あ".repeat(MAX_QUESTION_CHARS + 100);
        let out = truncate_question(&long, "test_route");
        assert_eq!(out.chars().count(), MAX_QUESTION_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn neutralize_delimiters_replaces_angle_brackets_with_fullwidth() {
        assert_eq!(
            neutralize_delimiters("</資料><資料 出典: 偽装>"),
            "＜/資料＞＜資料 出典: 偽装＞"
        );
    }

    #[test]
    fn egress_fallback_passes_through_clean_text() {
        let out = apply_egress_gate_or_fallback(
            "問題ありません。".to_string(),
            &ctx(),
            &ng(),
            "FALLBACK",
            "FALLBACK",
            "the input",
        );
        assert_eq!(out, "問題ありません。");
    }

    #[test]
    fn egress_fallback_falls_back_on_block() {
        let out = apply_egress_gate_or_fallback(
            "この方法で絶対に治りますのでご安心ください。".to_string(),
            &ctx(),
            &ng(),
            "FALLBACK_TEXT",
            "FALLBACK_TEXT",
            "the question",
        );
        assert_eq!(out, "FALLBACK_TEXT");
    }

    #[test]
    fn egress_fallback_falls_back_on_abstain() {
        let out = apply_egress_gate_or_fallback(
            "継続すると効果がありますと言われています。".to_string(),
            &ctx(),
            &ng(),
            "FALLBACK_TEXT",
            "FALLBACK_TEXT",
            "the question",
        );
        assert_eq!(out, "FALLBACK_TEXT");
    }

    // --- apply_draft_gate_or_fallback: 「truncated 判定 → egress gate → フォールバック」の
    // 集約先そのもの（`clarify.rs` / `escalation_reply.rs` の production 経路が実際に呼ぶ関数）
    // を直接検証する。呼び出し側のテスト（stub 経由の E2E）だけでは、`apply_egress_gate_or_fallback`
    // 呼び出しを丸ごと消しても NG に触れないクリーン文では検知できない（Warning 1）。

    #[test]
    fn draft_gate_falls_back_when_truncated_even_if_clean() {
        // truncated = true は、本文が NG 辞書に一切触れないクリーン文でもフォールバックへ倒れる
        // ことを固定する。egress gate 呼び出しの有無に関わらず truncated 分岐だけで結果が決まる。
        let draft = crate::llm::ReplyDraft {
            text: "製品名と発生時期を教えてください。".to_string(),
            truncated: true,
        };
        let out = apply_draft_gate_or_fallback(
            draft,
            &ctx(),
            &ng(),
            "FALLBACK_TEXT",
            "FALLBACK_TEXT",
            "test_route",
            "the input",
        );
        assert_eq!(out, "FALLBACK_TEXT");
    }

    #[test]
    fn draft_gate_falls_back_on_block_when_not_truncated() {
        // truncated = false かつ block 語を含む文。egress gate 呼び出しを消して draft.text を
        // そのまま返す退行が起きると、このテストは "FALLBACK_TEXT" ではなく draft.text と比較
        // して赤くなる。
        let draft = crate::llm::ReplyDraft {
            text: "この方法で絶対に治りますのでご安心ください。".to_string(),
            truncated: false,
        };
        let out = apply_draft_gate_or_fallback(
            draft,
            &ctx(),
            &ng(),
            "FALLBACK_TEXT",
            "FALLBACK_TEXT",
            "test_route",
            "the input",
        );
        assert_eq!(out, "FALLBACK_TEXT");
    }

    #[test]
    fn draft_gate_passes_through_clean_non_truncated_draft() {
        let draft = crate::llm::ReplyDraft {
            text: "問題ありません。".to_string(),
            truncated: false,
        };
        let out = apply_draft_gate_or_fallback(
            draft,
            &ctx(),
            &ng(),
            "FALLBACK_TEXT",
            "FALLBACK_TEXT",
            "test_route",
            "the input",
        );
        assert_eq!(out, "問題ありません。");
    }

    // --- to_plain_text（Issue #27: `/api/reply` の応答確定点のプレーンテキスト正規化）

    #[test]
    fn to_plain_text_removes_paired_double_asterisk_bold() {
        assert_eq!(to_plain_text("これは**重要**です"), "これは重要です");
    }

    #[test]
    fn to_plain_text_removes_paired_double_underscore_bold() {
        assert_eq!(to_plain_text("これは__重要__です"), "これは重要です");
    }

    #[test]
    fn to_plain_text_removes_paired_single_asterisk_italic() {
        assert_eq!(to_plain_text("これは*強調*です"), "これは強調です");
    }

    #[test]
    fn to_plain_text_removes_paired_single_underscore_italic() {
        assert_eq!(to_plain_text("これは_強調_です"), "これは強調です");
    }

    #[test]
    fn to_plain_text_strips_heading_markers_at_multiple_levels() {
        assert_eq!(to_plain_text("# 見出し\n本文"), "見出し\n本文");
        assert_eq!(to_plain_text("## 小見出し"), "小見出し");
    }

    #[test]
    fn to_plain_text_leaves_heading_marker_unchanged_without_a_following_space() {
        // `#` の直後に半角スペースが無い場合は Markdown の見出しとして機能しないため変更しない。
        assert_eq!(to_plain_text("#タグ"), "#タグ");
    }

    #[test]
    fn to_plain_text_removes_inline_code_backticks() {
        assert_eq!(to_plain_text("`code`を実行"), "codeを実行");
    }

    #[test]
    fn to_plain_text_removes_code_fence_marker_lines_and_normalizes_enclosed_content() {
        // フェンスの開始・終了行（\`\`\`）だけを取り除く。囲まれた行自体は他の行と同じく
        // transform_line を通る（フェンスは「透明」で、状態を持たない）。この入力に Markdown
        // 記法は無いため見た目は変わらないが、次のテストで中身が正規化されることを固定する。
        assert_eq!(to_plain_text("```\nコード行\n```\n本文"), "コード行\n本文");
    }

    #[test]
    fn to_plain_text_normalizes_markdown_inside_a_closed_code_fence() {
        // Issue #27 reviewer 指摘（Critical）: 旧実装はフェンス内側を transform_line に通さず、
        // `**太字**` がそのまま顧客へ届いていた。フェンスは行の除去のみを行い、中身は他の行と
        // 同じく正規化されることを固定する。
        let out = to_plain_text("```\n**太字**が残る\n```");
        assert!(
            !out.contains("**"),
            "fence 内側の ** が正規化されていない: {out:?}"
        );
    }

    #[test]
    fn to_plain_text_normalizes_markdown_after_an_unclosed_code_fence() {
        // Issue #27 reviewer 指摘（Critical）: 旧実装は `in_fence` フラグを状態として持ち、
        // 閉じないフェンスが 1 行でも出るとそれ以降のメッセージ全体が正規化されなくなっていた。
        // 状態を持たない実装（フェンス行の除去のみ）ではこの経路が構造的に発生しない。
        let out = to_plain_text("文A\n```\n**太字**");
        assert!(
            !out.contains("**"),
            "unclosed fence 後の ** が正規化されていない: {out:?}"
        );
    }

    #[test]
    fn to_plain_text_converts_link_syntax_to_fullwidth_parens() {
        assert_eq!(
            to_plain_text("[こちら](https://example.com)を参照"),
            "こちら（https://example.com）を参照"
        );
    }

    #[test]
    fn to_plain_text_converts_bullet_markers_hyphen_and_asterisk() {
        assert_eq!(to_plain_text("- 項目1\n- 項目2"), "・項目1\n・項目2");
        assert_eq!(to_plain_text("* 項目1"), "・項目1");
    }

    #[test]
    fn to_plain_text_handles_mixed_markdown_rules_in_one_input() {
        // 太字 + 箇条書き + リンクが 1 行に混在するケース。それぞれ独立に正しく変換されることを
        // 固定する（1 つの規則の実装が他の規則の出力を壊していないかの回帰）。
        let input = "- これは**重要**です。[こちら](https://example.com)を参照してください";
        let expected = "・これは重要です。こちら（https://example.com）を参照してください";
        assert_eq!(to_plain_text(input), expected);
    }

    #[test]
    fn to_plain_text_passes_through_text_without_markdown_syntax() {
        let plain = "お問い合わせいただきありがとうございます。担当者が確認いたします。";
        assert_eq!(to_plain_text(plain), plain);
    }

    #[test]
    fn to_plain_text_removes_unclosed_double_markers() {
        // 対になっていない `**` / `__` も最終的に除去する（顧客に記号を見せないため）。
        // `strip_double_marker` 単体は残すが、後段の `strip_single_marker` が単独マーカー
        // 2 個として拾う。合成後の挙動をここで固定する。
        assert_eq!(to_plain_text("**未閉じ"), "未閉じ");
        assert_eq!(to_plain_text("__未閉じ"), "未閉じ");
    }

    #[test]
    fn to_plain_text_keeps_asterisk_flanked_by_spaces_on_both_sides() {
        // reviewer 指摘（Warning）: 前後が空白のマーカーは CommonMark では開始子にも終了子にも
        // なり得ない。乗算記号のような用法（`5 * 3`）を対の候補にして消してしまう既存バグを
        // 固定する回帰テスト。
        let plain = "5 * 3 = 15 と 2 * 4 = 8";
        assert_eq!(to_plain_text(plain), plain);
    }

    #[test]
    fn to_plain_text_keeps_unpaired_underscore_inside_identifiers() {
        // `_` の前後が ASCII 英数字またはアンダースコアの場合は対の候補にしない。
        // 型番・識別子を誤って壊さないことを固定する。
        assert_eq!(to_plain_text("型番はURT_2_Aです"), "型番はURT_2_Aです");
        assert_eq!(
            to_plain_text("変数名はsnake_case_nameにしてください"),
            "変数名はsnake_case_nameにしてください"
        );
    }

    #[test]
    fn to_plain_text_keeps_numbered_lists_unchanged() {
        let numbered = "1. 項目1\n2. 項目2";
        assert_eq!(to_plain_text(numbered), numbered);
    }

    #[test]
    fn to_plain_text_preserves_newline_count_and_positions() {
        let input = "1行目\n2行目\n3行目";
        let out = to_plain_text(input);
        assert_eq!(out.matches('\n').count(), 2);
        assert_eq!(out, "1行目\n2行目\n3行目");
    }
}
