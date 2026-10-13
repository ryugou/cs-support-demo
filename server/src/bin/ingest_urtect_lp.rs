/// urtect 自社サイト（`https://urtect.cds2016.com/`）ランディングページの ingest CLI（Issue #69）。
///
/// 背景: 自社の料金・サービス内容を聞かれても材料が無く、CS が毎回取次になっていた。
/// 既存の ingest 対象は Google Sites マニュアル（`ingest_urtect`）と answers.alarm.com
/// （`ingest_alarmcom`）のみで、このランディングページは対象外だった。
///
/// サイト構造（調査済み、設計時点で確定）: ナビの 7 項目はすべてページ内アンカー
/// （`#about` / `#price` / `#product` / `#voice` / `#case` / `#flow` / `#faq`）で、下層ページも
/// sitemap も無い 1 ページ構成。したがって `ingest_alarmcom.rs` のような sitemap 駆動クロール・
/// リンク追跡は不要で、「1 回の取得 → 7 節に切る → 投入」で足りる。
///
/// 投入先は `urtect`（ManualSection。`ingest_urtect.rs` と同じノード型）のみを対象とする CLI。
/// 当初は homesec（`advisor_material`、`kind = own_product`）への同時投入も実装したが、
/// reviewer から Critical 指摘を受け撤回した: `advisor::materials::select_own_product_materials`
/// が `concern_category` で絞り込むため category を持たない LP 材料は concern 確定後のターンで
/// 必ず除外され目的（料金を答える）を達成せず、かつ concern が package_theft / stalking /
/// fire_disaster のとき own_product 側に一致が無くフォールバックで既存 7 件 + LP 7 件が無条件に
/// 注入され、`inject_guaranteed_own_products` が上限対象外のため後続の
/// `inject_category_materials` が `MAX_TOTAL_MATERIALS` 到達で事実上無効化される既存挙動の
/// 退行を伴っていた。homesec への投入設計そのものは別 Issue で検討する。
///
/// `manual::ingest_model::ManualSectionInput` / `build_section_graph` は再利用しない:
/// `fetched_at` を section 単位で保持するフィールドを持たず、かつ全フィールド列挙の
/// 構造体リテラルで `ingest_urtect.rs` / `ingest_alarmcom.rs` から直接構築されているため、
/// フィールド追加はその 2 バイナリの呼び出し箇所修正を強制する（本 Issue は既存バイナリの
/// 変更をしない）。そのため本バイナリは `GraphNode` を直接組み立てるローカル関数を持つ。
///
/// 料金節（`#price`）の本文構造化（Issue #96、design doc
/// `2026-10-07-partial-answer-with-handoff-design.md` §11 で追加）:
///
/// 当初は他の6節と同じ「ノイズ除去 → 空白正規化」の単一文字列化パイプラインで料金節も
/// 平坦化していたが、実 LLM スモーク（§10 run8）で「導入にどれくらいの費用がかかるの？」に
/// 対し下書きが1台分の月額（￥1,540）を書き落とす不具合が実測された。平坦化後の本文
/// 「Price 基本料金 設置初期費用 月額利用料金 (1台) ￥1,540 (税込) ~ …」では、
/// どのラベル（設置初期費用／月額利用料金）にどの金額が対応するかが文字列上あいまいで、
/// 取り次ぐ項目（設置初期費用）の金額を書かないという指示と衝突し、モデルが安全側に倒して
/// 月額分まで省いたと判断した。
///
/// 対策として、料金節だけは HTML のラベル要素（`.price-box-name` 等）と値要素
/// （`.price-box-price` 等）の DOM 対応を保ったまま「ラベル: 値」の行として本文を
/// 組み立て直す（`build_price_body` 以下）。値の無いラベル（設置初期費用）は
/// 「ラベル: 金額の記載なし」と機械的に表記する。これにより、ページに実際に書かれていない
/// 情報（「~」が何で変動するか・カメラ4台以上の月額）は従来どおり本文に出現しない一方、
/// 以前のように「書かれていないこと」をコードの固定文言（旧 `PRICE_DISCLAIMER`）として
/// 注記する必要が無くなった: 値の有無は DOM から導出されるため、ページの実際の構造を
/// 離れた文言をコードに持たずに済む。
///
/// ラベルと値の対応が DOM から一意に取れない場合（見出しが0個/2個以上、価格ボックスに
/// ラベルが無い、1つのラベルに値が2個以上ある、箇条書きの区切り記号「→」が無い等）は、
/// 従来の平坦化本文へフォールバックせず fail closed でエラーにする（料金ページの改変に
/// 気づくため。詳細は各抽出関数の doc コメントを参照）。
///
/// 既知の痩せた節（投入 OK、抽出失敗ではない）: `#about` は見出し「ユアテクトとは？」以外の
/// 本文テキストを持たない（実体は動画 `<video>` と画像のみで、説明文の `<p>` が無い）。
/// 抽出結果は 8〜9 文字程度になるが、これは fail-closed の「本文が空」には該当せず、ページの
/// 実際の構成を正しく反映した結果である。次にこのファイルを読む人が「#about の抽出に
/// 失敗している」と誤解しないよう明記しておく。og:description 等からの補完は本 Issue の
/// スコープ外（意図的に実装していない）。
use anyhow::{Context, Result};
use clap::Parser;
use cs_support_mcp::{
    manual::{
        crawl::{extract_text_excluding_noise, normalize_body},
        ingest_model::{build_document_node, content_hash},
        schema_ids::{manual_node_id, with_schema_name, KIND_DOC, KIND_SECTION},
        vectors::{embed_all, vector_entry, EMBED_CONCURRENCY},
    },
    model::{GraphBuild, GraphEdge, GraphNode},
    vegapunk::VegapunkClient,
};
use scraper::{ElementRef, Html, Node, Selector};
use serde_json::json;
use std::{env, fs, path::PathBuf, sync::OnceLock};
use url::Url;

/// 差分 ingest 用の安定 doc key。urtect（`doc-manual`）/ alarmcom（`doc-alarmcom`）とは
/// 別 document として共存する。
const DOC_KEY: &str = "doc-urtect-lp";
const DOC_TITLE: &str = "ユアテクト サービス紹介ページ";

/// section slug の namespace prefix。`ingest_alarmcom.rs` の `alarmcom-` prefix と同じ理由:
/// ManualSection ノード id は schema 内で slug だけから決まる（doc_keyではスコープされない）
/// ため、他ソース（doc-manual / doc-alarmcom）の slug と衝突しないよう明示的に分離する。
const LP_SLUG_PREFIX: &str = "urtectlp-";

/// アンカー id → 表示タイトル（ナビの文言に揃える、固定値）。ページ内見出しの表記ゆれ
/// （例: 「ユアテクトとは？」の「？」有無）に依存させないため、ナビ基準の固定ラベルを正とする。
/// 出現順はナビの列挙順（= `order` 属性の元）。
const ANCHORS: [(&str, &str); 7] = [
    ("about", "ユアテクトとは"),
    ("price", "基本料金"),
    ("product", "取り扱い機器"),
    ("voice", "お客様の声"),
    ("case", "活用事例"),
    ("flow", "導入までの流れ"),
    ("faq", "よくあるご質問"),
];

/// 値要素（`.price-box-price`）が無い価格ボックスの値として機械的に表記する文言。
const PRICE_VALUE_ABSENT: &str = "金額の記載なし";

/// 料金節の見出しブロックにある装飾ラベル（「Price」）。本文として使わないことを明示した
/// 唯一の無視対象で、これ以外の未消費テキストは `ensure_price_text_fully_consumed` が弾く。
const PRICE_DECORATIVE_LABEL_SELECTOR: &str = ".sub-ttl";

/// 料金節の抽出関数が実際に読む要素のセレクタ（`PRICE_DECORATIVE_LABEL_SELECTOR` は別扱い）。
const PRICE_CONSUMED_SELECTORS: [&str; 4] = [
    "h2",
    ".price-box > .price-box-contents .price-box-name",
    ".price-box > .price-box-contents .price-box-price",
    ".price-text > p",
];

/// 1 ページ分の抽出結果（ネットワーク非依存、テスト可能な中間表現）。
#[derive(Debug)]
struct LpSection {
    anchor: &'static str,
    title_ja: String,
    body: String,
}

#[derive(Debug, Parser)]
struct Args {
    #[arg(
        long,
        env = "VEGAPUNK_ENDPOINT",
        default_value = "http://vegapunk.local:6840"
    )]
    endpoint: String,
    /// 投入先: urtect（ManualSection）。
    #[arg(long, default_value = "urtect")]
    urtect_schema: String,
    #[arg(long, default_value = "VEGAPUNK_BEARER_TOKEN")]
    token_env: String,
    /// bearer token ファイル（主経路）。CLAUDE.md のローカル/GCE 起動手順の既定パスと揃える。
    /// ファイルが無い/読めない場合のみ --token-env にフォールバックする。
    #[arg(
        long,
        env = "VEGAPUNK_BEARER_TOKEN_FILE",
        default_value = "/private/tmp/vegapunk-bearer-token"
    )]
    token_file: Option<PathBuf>,
    #[arg(long, default_value = "../schema/cs-support.yml")]
    urtect_schema_file: PathBuf,
    #[arg(long, default_value = "https://urtect.cds2016.com/")]
    top_url: String,
    /// 取得・抽出・fail-closed 検査のみ行い、vegapunk への接続・投入を一切行わない
    /// （検証用。bearer token 不要）。
    #[arg(long)]
    dry_run: bool,
    /// embed / upsert_vectors を一切呼ばずスキップする。未指定時は embed 失敗を fail closed
    /// で扱う。
    #[arg(long)]
    no_vectors: bool,
}

/// token 解決: 既定は --token-file（CLAUDE.md のローカル/GCE 起動手順と同じ経路）。
/// ファイルが無い/読めない場合のみ --token-env にフォールバックする。
/// `ingest_urtect.rs` の `read_token` と同じ挙動（Args の型が異なるため関数は複製する。
/// 既存バイナリの慣例）。
fn read_token(args: &Args) -> Result<String> {
    if let Some(path) = &args.token_file {
        match fs::read_to_string(path) {
            Ok(body) => {
                let trimmed = body.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_string());
                }
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "token file unreadable; falling back to --token-env"
                );
            }
        }
    }
    let token = env::var(&args.token_env).with_context(|| {
        format!(
            "vegapunk bearer token not found (tried --token-file {:?} and env {})",
            args.token_file, args.token_env
        )
    })?;
    let trimmed = token.trim();
    if trimmed.is_empty() {
        anyhow::bail!("vegapunk bearer token env {} is empty", args.token_env);
    }
    Ok(trimmed.to_string())
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("non-success status for {url}"))?;
    resp.text()
        .await
        .with_context(|| format!("read response body {url}"))
}

/// アンカー div の CSS セレクタ（`div#about` 等）。anchor は `ANCHORS` 由来の ASCII 定数
/// のみを受け取るため parse は常に成功する（`.expect` は既存 ingest バイナリの慣例と同じ）。
fn anchor_selector(anchor: &str) -> Selector {
    Selector::parse(&format!("div#{anchor}"))
        .expect("valid selector: anchor ids are ASCII constants")
}

/// section slug（`urtectlp-` prefix で他ソースの slug と衝突させない）。
/// anchor id だけから決まるため、再実行で必ず同じ値になる（冪等）。
fn lp_slug(anchor: &str) -> String {
    format!("{LP_SLUG_PREFIX}{anchor}")
}

/// アンカー付き出典 URL（`{top_url}#{anchor}`）。文字列連結ではなく `Url::set_fragment` で
/// 組み立てる: `--top-url` に query 文字列や既存 fragment が付いていても壊れない
/// （文字列連結だと `https://host/?x=1` が `https://host/?x=1/#price` のように壊れる）。
/// 末尾スラッシュ有無も `Url::parse` 側で正規化済みの `top_url` を受け取るため、常に
/// `.../#anchor` の形になる。
fn anchored_url(top_url: &Url, anchor: &str) -> String {
    let mut url = top_url.clone();
    url.set_fragment(Some(anchor));
    url.to_string()
}

/// 数字と桁区切り「,」「.」の間に挟まった空白にマッチする（タグ分割由来。実サイトでは
/// `<span>1</span><span>,</span>540` のように金額の桁がスタイル用 `<span>` で分割されており、
/// `extract_text_excluding_noise` のテキストノード単位の抽出がそれぞれの間に空白を1つ
/// 挟んでしまう。例: 抽出直後 "1 , 540"）。`\s*` は0個以上なので、元から空白が無い
/// 正常な表記（"2,640" 等）にマッチしても置換後は同じ内容になり、冪等に働く。
fn digit_separator_spacing_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(\d)\s*([,.])\s*(\d)").expect("digit separator regex must compile")
    })
}

/// 通貨記号（￥/¥/$）と直後の数字の間に挟まった空白にマッチする（タグ分割由来。例:
/// `<span>￥</span>1540` → 抽出直後 "￥ 1540"）。
fn currency_digit_spacing_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"([￥¥$])\s*(\d)").expect("currency digit regex must compile")
    })
}

/// 数字と直後の「円」の間に挟まった空白にマッチする（タグ分割由来）。
///
/// 「円」以外の単位（台・名 等）は意図的に対象外にする。本サイトの実コンテンツで
/// タグ分割によって単位の前に空白が混入する箇所は金額表記（￥記号・桁区切りカンマ）だけで、
/// 「台」等の単位は常に1つのテキストノード内に空白なしで書かれている（例:「月額利用料金
/// (1台)」「カメラ2台」はどちらも分割されていない）。対象を「円」に限定せず単位全般に
/// 広げると、将来ページに「カメラ 2 台」のような意図的な分かち書きが追加された場合に
/// それを壊してしまう副作用があり、現時点でそれを正当化する実例が無いため、最小スコープ
/// （円のみ）に留める。
fn digit_yen_spacing_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(\d)\s*円").expect("digit yen regex must compile"))
}

/// タグ分割に由来する、金額表記内部の空白だけを除去する純関数。
///
/// 対象（タグ分割由来の空白のみ。文章としての通常の空白は対象にしない）:
/// 1. 数字と桁区切り「,」「.」の間の空白: `1 , 540` → `1,540`
/// 2. 通貨記号（￥/¥/$）と直後の数字の間の空白: `￥ 1,540` → `￥1,540`
/// 3. 数字と直後の「円」の間の空白: `1,540 円` → `1,540円`
///
/// 3パターンとも「数字 + 記号/単位」という狭い並びにしかマッチしないため、
/// 日本語文中の意図的な分かち書き（例:「カメラ 2 台」）には一切作用しない
/// （カンマ・ピリオド・円のいずれも隣接していないため）。背景・対象外とする単位の
/// 判断理由は `digit_yen_spacing_regex` の doc コメントを参照。
///
/// NBSP 等の Unicode 空白は、ここに到達する前に `normalize_body`（manual/crawl.rs:13、
/// `split_whitespace` ベース）が半角スペース 1 つへ畳むため、本関数では意識しなくてよい。
fn collapse_numeric_spacing(body: &str) -> String {
    let step1 = digit_separator_spacing_regex()
        .replace_all(body, "$1$2$3")
        .into_owned();
    let step2 = currency_digit_spacing_regex()
        .replace_all(&step1, "$1$2")
        .into_owned();
    digit_yen_spacing_regex()
        .replace_all(&step2, "$1円")
        .into_owned()
}

/// 料金節（`#price`）見出しの抽出。
///
/// `#price` コンテナ配下から `h2` 要素を探し、ちょうど 1 個であることを要求する
/// （価格ボックス群より上位にある見出しはこの 1 個のみ、という実ページの構造を前提にする）。
/// 0 個・2 個以上は fail closed（どれが見出しか一意に決められないため）。
fn extract_price_heading(container: ElementRef<'_>) -> Result<String> {
    let selector = Selector::parse("h2").expect("valid selector: static string \"h2\" must parse");
    let mut headings = container.select(&selector);
    let heading = headings.next().with_context(|| {
        "anchor #price: no h2 heading found; the site structure may have changed since this \
         ingest CLI was written — aborting without writing anything"
            .to_string()
    })?;
    if headings.next().is_some() {
        anyhow::bail!(
            "anchor #price: more than one h2 heading found; cannot determine which one titles \
             the price block — the site structure may have changed since this ingest CLI was \
             written — aborting without writing anything"
        );
    }
    let text = normalize_body(&heading.text().collect::<String>());
    if text.is_empty() {
        anyhow::bail!(
            "anchor #price: the h2 heading extracted to an empty text — aborting without \
             writing anything"
        );
    }
    Ok(text)
}

/// 価格ボックス（`.price-box-contents`）1 個分の「ラベル: 値」行を組み立てる。
///
/// ラベルは `.price-box-name` のテキストを `normalize_body` で正規化したもの（内部の空白は
/// 保持する。例:「月額利用料金」と「(1台)」の間の半角スペース）。`.price-box-name` が
/// 0 個・2 個以上なら fail closed（ラベルを一意に決められないため）。
///
/// 値は `.price-box-price` 配下の全テキストを `extract_text_excluding_noise` で集め、
/// `collapse_numeric_spacing` でタグ分割由来の数字周りの空白を潰したうえで、残る空白を
/// 全て除去する（価格表記はタグ境界ごとに空白が挟まるだけで、意味のある空白を内部に
/// 持たないため丸ごと詰めてよい）。`.price-box-price` が無ければ値は「金額の記載なし」。
/// 2 個以上あれば fail closed（どちらが正か決められないため）。
fn extract_price_box_line(box_el: ElementRef<'_>) -> Result<String> {
    let name_selector = Selector::parse(".price-box-name")
        .expect("valid selector: static string \".price-box-name\" must parse");
    let price_selector = Selector::parse(".price-box-price")
        .expect("valid selector: static string \".price-box-price\" must parse");

    let mut names = box_el.select(&name_selector);
    let name_el = names.next().with_context(|| {
        "anchor #price: a price box has no .price-box-name label — the site structure may have \
         changed since this ingest CLI was written — aborting without writing anything"
            .to_string()
    })?;
    if names.next().is_some() {
        anyhow::bail!(
            "anchor #price: a price box has more than one .price-box-name label — cannot \
             determine which one is authoritative — the site structure may have changed since \
             this ingest CLI was written — aborting without writing anything"
        );
    }
    let label = normalize_body(&name_el.text().collect::<String>());
    if label.is_empty() {
        anyhow::bail!(
            "anchor #price: a .price-box-name label extracted to an empty text — aborting \
             without writing anything"
        );
    }

    let mut prices = box_el.select(&price_selector);
    let value = match prices.next() {
        None => PRICE_VALUE_ABSENT.to_string(),
        Some(price_el) => {
            if prices.next().is_some() {
                anyhow::bail!(
                    "anchor #price: price box {label:?} has more than one .price-box-price \
                     value — cannot determine which one is authoritative — the site structure \
                     may have changed since this ingest CLI was written — aborting without \
                     writing anything"
                );
            }
            let raw = extract_text_excluding_noise(price_el);
            let collapsed = collapse_numeric_spacing(&raw);
            collapsed.chars().filter(|c| !c.is_whitespace()).collect()
        }
    };
    Ok(format!("{label}: {value}"))
}

/// `.price-box` の直接の子である `.price-box-contents` を出現順に処理し、価格ボックス行を
/// 組み立てる。1 個も無ければ fail closed（料金ブロックの構造自体が想定と異なるため）。
fn extract_price_box_lines(container: ElementRef<'_>) -> Result<Vec<String>> {
    let contents_selector = Selector::parse(".price-box > .price-box-contents")
        .expect("valid selector: static string \".price-box > .price-box-contents\" must parse");
    let mut lines = Vec::new();
    for box_el in container.select(&contents_selector) {
        lines.push(extract_price_box_line(box_el)?);
    }
    if lines.is_empty() {
        anyhow::bail!(
            "anchor #price: no .price-box-contents price boxes found under .price-box — the \
             site structure may have changed since this ingest CLI was written — aborting \
             without writing anything"
        );
    }
    Ok(lines)
}

/// `<p>` 要素の直接の子ノードを前から走査し、`<br>` が現れるたびにフラグメントを区切る
/// 関数（テキストの文字列分割ではなく DOM の子ノード境界で分割する。将来 `<br>` の数が
/// 変わっても追従できるようにするため）。テキストと `<br>` とコメント以外の子要素は
/// 想定外の構造なので fail closed（コメントは読み飛ばす）。
fn split_on_br(p: ElementRef<'_>) -> Result<Vec<String>> {
    let mut fragments = Vec::new();
    let mut current = String::new();
    for child in p.children() {
        match child.value() {
            Node::Element(el) if el.name() == "br" => {
                fragments.push(std::mem::take(&mut current));
            }
            Node::Text(text) => current.push_str(text),
            Node::Element(el) => anyhow::bail!(
                "anchor #price: a price bullet paragraph contains an unexpected <{}> element — \
                 the site structure may have changed since this ingest CLI was written — \
                 aborting without writing anything",
                el.name()
            ),
            _ => {}
        }
    }
    fragments.push(current);
    Ok(fragments)
}

/// 箇条書きフラグメント 1 個を「ラベル: 値」行へ変換する。
///
/// 先頭が全角中点「・」であることを確認して取り除き（無ければ fail closed）、残りを
/// 「→」（U+2192）でちょうど 1 回だけ分割する（0 回・2 回以上は fail closed。ラベルと値の
/// 対応を一意に決められないため）。前後は `trim` のみ行う（空白の正規化はせず、内部の
/// 空白をそのまま残す）。
fn price_bullet_line(fragment: &str) -> Result<String> {
    let trimmed = fragment.trim();
    let without_marker = trimmed.strip_prefix('・').with_context(|| {
        format!(
            "anchor #price: a price bullet fragment does not start with \"・\": {trimmed:?} — \
             the site structure may have changed since this ingest CLI was written — aborting \
             without writing anything"
        )
    })?;
    let arrow_count = without_marker.matches('→').count();
    if arrow_count != 1 {
        anyhow::bail!(
            "anchor #price: a price bullet fragment has {arrow_count} \"→\" separator(s), \
             expected exactly 1: {without_marker:?} — the site structure may have changed \
             since this ingest CLI was written — aborting without writing anything"
        );
    }
    let (label, value) = without_marker
        .split_once('→')
        .expect("arrow_count == 1 was just checked above");
    Ok(format!("{}: {}", label.trim(), value.trim()))
}

/// `.price-text` 配下の箇条書き行と末尾の注記行を組み立てる。
///
/// `.price-text > p` のうち最後の 1 個を注記（「※別途設置工事費用がかかります。」等。
/// そのままの文を 1 行として残す）、それより前を箇条書きの対象にする。`<p>` の数を固定
/// 個数に決め打ちしない（将来台数の例示が増減しても追従できるようにするため）。
/// `.price-text > p` が 1 個以下なら fail closed（箇条書きと注記の両方が揃わないため）。
fn extract_price_bullets_and_note(container: ElementRef<'_>) -> Result<(Vec<String>, String)> {
    let p_selector = Selector::parse(".price-text > p")
        .expect("valid selector: static string \".price-text > p\" must parse");
    let paragraphs: Vec<ElementRef<'_>> = container.select(&p_selector).collect();
    if paragraphs.len() <= 1 {
        anyhow::bail!(
            "anchor #price: .price-text has {} <p> element(s), need at least 2 (a bullet list \
             and a trailing note) — the site structure may have changed since this ingest CLI \
             was written — aborting without writing anything",
            paragraphs.len()
        );
    }
    let (bullet_paragraphs, note_paragraphs) = paragraphs.split_at(paragraphs.len() - 1);

    let note_line = normalize_body(&note_paragraphs[0].text().collect::<String>());
    if note_line.is_empty() {
        anyhow::bail!(
            "anchor #price: the trailing .price-text note paragraph extracted to an empty \
             text — aborting without writing anything"
        );
    }

    let mut bullet_lines = Vec::new();
    for p in bullet_paragraphs {
        for fragment in split_on_br(*p)? {
            bullet_lines.push(price_bullet_line(&fragment)?);
        }
    }
    if bullet_lines.is_empty() {
        anyhow::bail!(
            "anchor #price: no bullet fragments found under .price-text — aborting without \
             writing anything"
        );
    }
    Ok((bullet_lines, note_line))
}

/// 料金節（`#price`）専用の本文組み立て。
///
/// 他の 6 節と異なり、単一文字列への平坦化はしない: 見出し行・価格ボックス行・箇条書き行・
/// 注記行を個別に組み立て、`"\n"` で結合する（各行の中身だけを個別に正規化し、結合後に
/// `normalize_body` へ通さない。`normalize_body` は改行も半角スペース 1 個へ畳むため、
/// 通すと行構造が壊れる）。ファイル冒頭の doc コメントに設計の背景（Issue #96）を記載。
fn build_price_body(container: ElementRef<'_>) -> Result<String> {
    let mut lines = Vec::with_capacity(4);
    lines.push(extract_price_heading(container)?);
    lines.extend(extract_price_box_lines(container)?);
    let (bullet_lines, note_line) = extract_price_bullets_and_note(container)?;
    lines.extend(bullet_lines);
    lines.push(note_line);
    ensure_price_text_fully_consumed(container)?;
    Ok(lines.join("\n"))
}

/// `#price` 配下のテキストのうち、抽出関数が読む要素と装飾ラベルのどちらにも属さないものが
/// あれば fail closed にする（新しい金額・注記が静かに本文から落ちるのを防ぐ）。
/// 全体のテキストの語（空白区切り）の多重集合から、読んだ要素の語を引いた残りを未消費とする。
fn ensure_price_text_fully_consumed(container: ElementRef<'_>) -> Result<()> {
    let mut remaining: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for word in extract_text_excluding_noise(container).split_whitespace() {
        *remaining.entry(word.to_string()).or_insert(0) += 1;
    }
    let consumed_selectors = PRICE_CONSUMED_SELECTORS
        .iter()
        .copied()
        .chain(std::iter::once(PRICE_DECORATIVE_LABEL_SELECTOR));
    for selector_text in consumed_selectors {
        let selector = Selector::parse(selector_text)
            .expect("valid selector: price consumed selectors are static strings");
        for element in container.select(&selector) {
            for word in extract_text_excluding_noise(element).split_whitespace() {
                if let Some(count) = remaining.get_mut(word) {
                    *count = count.saturating_sub(1);
                }
            }
        }
    }
    let mut leftover: Vec<String> = remaining
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(word, _)| word)
        .collect();
    if leftover.is_empty() {
        return Ok(());
    }
    leftover.sort();
    anyhow::bail!(
        "anchor #price: unconsumed text not covered by the recognized price elements: {} — the \
         site structure may have changed since this ingest CLI was written — aborting without \
         writing anything",
        leftover.join(" ")
    )
}

/// 取得済み HTML から 7 節を切り出す純関数（ネットワーク非依存、テスト可能）。
///
/// fail closed: アンカーが 1 つでも見つからない、または本文が空になった節が 1 つでもあれば
/// 何も返さずエラーにする（サイト構造が変わった可能性があるため、部分的な投入はしない）。
fn extract_lp_sections(html: &str) -> Result<Vec<LpSection>> {
    let document = Html::parse_document(html);
    let mut sections = Vec::with_capacity(ANCHORS.len());
    for (anchor, title_ja) in ANCHORS {
        let selector = anchor_selector(anchor);
        let container = document.select(&selector).next().with_context(|| {
            format!(
                "anchor #{anchor} (div#{anchor}) not found in fetched HTML; the site structure \
                 may have changed since this ingest CLI was written against the 7-anchor layout \
                 (about/price/product/voice/case/flow/faq) — aborting without writing anything"
            )
        })?;
        let body = if anchor == "price" {
            // Issue #96: 料金節だけは DOM 構造を保った行組み立てにする（理由はファイル冒頭の
            // doc コメントと `build_price_body` の doc コメントを参照）。
            build_price_body(container)?
        } else {
            let raw = extract_text_excluding_noise(container);
            let normalized = normalize_body(&raw);
            // タグ分割（<span>等）由来で金額表記に紛れ込んだ空白をここで潰す。本 Issue の目的が
            // 「料金を正確に答えられるようにする」ことであり、`￥ 1 , 540` のような表記は
            // 桁の読み違いに直結するため、価格ボックス以外の節にも一律適用する（金額的な
            // フラグメントが混ざる可能性があり、対象パターン自体が狭いので害が無い）。
            //
            // CJK文字同士の間の空白一般まで対象を広げないこと: 一度実装し、実データで有害と
            // 判明して差し戻した（`extract_text_excluding_noise` が挟む空白の大半は語の途中の
            // 分割ではなく、箇条書き・段落・別セルの区切り。詰めると別項目が融合する）。意図的な
            // 設計判断であることは
            // `collapse_numeric_spacing_preserves_element_boundary_and_word_internal_cjk_spacing`
            // で固定している。
            collapse_numeric_spacing(&normalized)
        };
        if body.is_empty() {
            anyhow::bail!(
                "anchor #{anchor} extracted to an empty body after removing script/style/nav/\
                 header/footer noise; the section markup may have changed — aborting without \
                 writing anything"
            );
        }
        sections.push(LpSection {
            anchor,
            title_ja: title_ja.to_string(),
            body,
        });
    }
    Ok(sections)
}

/// ManualSection ノード（urtect 側）を直接組み立てる。
/// `manual::ingest_model::build_section_graph` を使わない理由はファイル冒頭コメントを参照
/// （`fetched_at` を section 単位で持たせるため、既存バイナリ非変更の制約下でローカル実装にした）。
fn build_lp_manual_section_node(
    schema: &str,
    slug: &str,
    title_ja: &str,
    body: &str,
    source_url: &str,
    order: i32,
    fetched_at: &str,
) -> GraphNode {
    GraphNode {
        id: manual_node_id(schema, KIND_SECTION, slug),
        node_type: KIND_SECTION.to_string(),
        attributes: vec![
            ("section_key".to_string(), slug.to_string()),
            ("doc_key".to_string(), DOC_KEY.to_string()),
            ("title".to_string(), title_ja.to_string()),
            ("body".to_string(), body.to_string()),
            ("source_url".to_string(), source_url.to_string()),
            // 1 ページ構成で階層が無いため breadcrumb は title と同じ
            // （`ingest_urtect.rs` の root section と同じ扱い）。
            ("breadcrumb".to_string(), title_ja.to_string()),
            ("section_no".to_string(), String::new()),
            ("order".to_string(), order.to_string()),
            ("source_lang".to_string(), "ja".to_string()),
            ("content_hash".to_string(), content_hash(body)),
            ("fetched_at".to_string(), fetched_at.to_string()),
        ],
    }
}

/// document → section の HAS_SECTION 辺。
fn build_lp_has_section_edge(schema: &str, slug: &str) -> GraphEdge {
    GraphEdge {
        from_id: manual_node_id(schema, KIND_DOC, DOC_KEY),
        to_id: manual_node_id(schema, KIND_SECTION, slug),
        edge_type: "HAS_SECTION".to_string(),
        attributes: Vec::new(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let top_url =
        Url::parse(&args.top_url).with_context(|| format!("parse top url {}", args.top_url))?;

    // redirect は top_url と同一 scheme/host のみ追従する（nav 由来ではなく単一 URL だが、
    // 既存 ingest バイナリと同じ防御方針: 意図しない外部ホストへの fetch/SSRF を防ぐ）。
    let allowed_scheme = top_url.scheme().to_string();
    let allowed_host = top_url.host_str().unwrap_or_default().to_string();
    let redirect_policy = reqwest::redirect::Policy::custom(move |attempt| {
        let same_origin = attempt.url().scheme() == allowed_scheme
            && attempt.url().host_str() == Some(allowed_host.as_str());
        if !same_origin {
            attempt.error("cross-origin redirect blocked (same-host policy)")
        } else if attempt.previous().len() >= 5 {
            attempt.error("too many redirects")
        } else {
            attempt.follow()
        }
    });
    let http = reqwest::Client::builder()
        .user_agent("cs-support-mcp/ingest_urtect_lp")
        .redirect(redirect_policy)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build http client")?;

    let html = fetch(&http, top_url.as_str())
        .await
        .with_context(|| format!("fetch top url {top_url}"))?;

    let sections = extract_lp_sections(&html)?;

    if args.dry_run {
        let summary: Vec<serde_json::Value> = sections
            .iter()
            .map(|s| {
                json!({
                    "anchor": s.anchor,
                    "title_ja": s.title_ja,
                    "body_chars": s.body.chars().count(),
                    // dry-run の目的は投入前に内容を目視確認することなので、文字数だけでなく
                    // 本文全文も出す（投入はしない。確認用）。
                    "body": s.body,
                    "source_url": anchored_url(&top_url, s.anchor),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "dry_run": true,
                "top_url": args.top_url,
                "section_count": sections.len(),
                "sections": summary,
            }))?
        );
        return Ok(());
    }

    let token = read_token(&args)?;
    let fetched_at = chrono::Utc::now().to_rfc3339();
    // vector metadata の timestamp_ms は run 開始時に 1 回だけ取得する（ingest_urtect.rs と
    // 同じ理由: entry ごとに now を取ると同一 run 内で値がばらつき決定性が失われる）。
    let ingest_timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before unix epoch; cannot compute vector metadata timestamp_ms")?
        .as_millis()
        .to_string();

    let urtect_schema_yaml = with_schema_name(
        &fs::read_to_string(&args.urtect_schema_file)
            .with_context(|| format!("read schema file {}", args.urtect_schema_file.display()))?,
        &args.urtect_schema,
    )?;

    let client = VegapunkClient::connect(&args.endpoint, &token).await?;
    client
        .create_or_update_schema(&args.urtect_schema, urtect_schema_yaml)
        .await?;

    // urtect 側: ManualDocument + 7 x ManualSection + 7 x HAS_SECTION。
    let mut urtect_nodes = vec![build_document_node(
        &args.urtect_schema,
        DOC_KEY,
        DOC_TITLE,
        top_url.as_str(),
        &fetched_at,
    )];
    let mut urtect_edges = Vec::with_capacity(sections.len());
    let mut embed_items: Vec<(String, String)> = Vec::with_capacity(sections.len());

    for (idx, section) in sections.iter().enumerate() {
        let slug = lp_slug(section.anchor);
        let source_url = anchored_url(&top_url, section.anchor);
        urtect_nodes.push(build_lp_manual_section_node(
            &args.urtect_schema,
            &slug,
            &section.title_ja,
            &section.body,
            &source_url,
            idx as i32,
            &fetched_at,
        ));
        urtect_edges.push(build_lp_has_section_edge(&args.urtect_schema, &slug));
        embed_items.push((format!("section {}", section.anchor), section.body.clone()));
    }

    // embed は node/edge upsert より先に実行する（`ingest_urtect.rs` と同じ fail-closed
    // 順序: embed 失敗時に中途半端な状態の node/edge を確定させない）。
    let vectors_skipped = args.no_vectors;
    let upserted_vectors = if vectors_skipped {
        0
    } else {
        let section_vectors = embed_all(&client, embed_items, EMBED_CONCURRENCY).await?;
        // embed_all は入力順を保つ契約（manual/vectors.rs::embed_all の doc コメント）なので、
        // sections と直接 zip して slug/text を組み立てられる（embed_slugs / embed_texts への
        // 複製が不要。slug は lp_slug が純関数・冪等なので再計算で十分で、本文の clone も
        // embed_items 分の1回だけになる）。
        let mut entries = Vec::with_capacity(section_vectors.len());
        for (section, vector) in sections.iter().zip(section_vectors) {
            let slug = lp_slug(section.anchor);
            let id = manual_node_id(&args.urtect_schema, KIND_SECTION, &slug);
            entries.push(vector_entry(
                id,
                vector,
                &section.body,
                KIND_SECTION,
                &ingest_timestamp_ms,
            ));
        }
        let entries_len = entries.len();
        client
            .upsert_vectors(entries)
            .await
            .with_context(|| format!("upsert vectors (entries={entries_len})"))?
    };

    let urtect_node_count = urtect_nodes.len();
    let urtect_edge_count = urtect_edges.len();
    let urtect_graph = GraphBuild {
        nodes: urtect_nodes,
        edges: urtect_edges,
    };
    let (urtect_upserted_nodes, urtect_upserted_edges) = client
        .upsert_graph_low_level(urtect_graph)
        .await
        .with_context(|| {
            format!(
                "upsert urtect graph (schema={}, nodes={}, edges={})",
                args.urtect_schema, urtect_node_count, urtect_edge_count
            )
        })?;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "dry_run": false,
            "urtect_schema": args.urtect_schema,
            "top_url": args.top_url,
            "fetched_at": fetched_at,
            "section_count": sections.len(),
            "urtect_upserted_nodes": urtect_upserted_nodes,
            "urtect_upserted_edges": urtect_upserted_edges,
            "urtect_upserted_vectors": upserted_vectors,
            "vectors_skipped": vectors_skipped,
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実サイトの構造を縮約したフィクスチャ（全文は巨大なため要旨を抜粋）。
    /// price 節だけは実際のマークアップ（span で分割された金額表記・閉じタグ省略を含む
    /// price-box-price02 等）をできる限り忠実に再現する。これが「書かれていないこと」の
    /// 明示テストの土台になる。
    const FIXTURE_HTML: &str = r##"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"></head>
<body>
<nav class="global-nav">
<ul>
<li><a href="#about">ユアテクトとは？</a></li>
<li><a href="#price">基本料金</a></li>
<li><a href="#product">取り扱い機器</a></li>
<li><a href="#voice">お客様の声</a></li>
<li><a href="#case">活用事例</a></li>
<li><a href="#flow">導入までの流れ</a></li>
<li><a href="#faq">よくあるご質問</a></li>
</ul>
</nav>
<div class="about wrap anchor" id="about">
    <h2>ユアテクト<span class="fs90">とは？</span></h2>
    <p>ユアテクトは福岡・北九州エリアを中心に、住宅・店舗向けの防犯カメラの導入・設置を行うサービスです。</p>
</div>
<div class="price wrap anchor" id="price">
        <div class="price-ttl">
            <div class="block-ttl block-ttl02 ttl-contents">
                <div class="sub-ttl">Price</div>
                <div class="ttl-line"></div>
                <h2>基本料金</h2>
            </div>
        </div>
        <div class="price-contents wrap-contents">
            <div class="price-box">
                <div class="price-box-contents price-box01 inview_re fadeIn is-show">
                    <div class="price-box-img">
                        <img class="" alt="" src="img/price-box-img01.png">
                    </div>
                    <div class="price-box-text">
                        <p class="price-box-name">設置初期費用</p>
                    </div>
                </div>
                <div class="price-box-contents price-box02 inview_re fadeIn is-show">
                    <div class="price-box-img">
                        <img class="" alt="" src="img/price-box-img02.png">
                    </div>
                    <div class="price-box-text">
                        <p class="price-box-name">月額利用料金 (1台)</p>
                        <div class="price-box-line"></div>
                        <div class="price-box-price">
                            <p class="price-box-price01"><span class="fs90">￥</span>1<span class="fs90">,</span>540
                            </p>
            <p class="price-box-price02">
                <span class="price-box-price02-01">(税込)</span>
            <span class="price-box-price02-02">~</span>
            </div>

                    </div>
                </div>

<img class="price-plus-icon" alt="" src="img/price-plus-icon.png">
            </div>
            <div class="price-text">
            <p>・カメラ2台 → 2,640円（税込）／月　<br class="brsp">・カメラ3台 → 3,960円（税込）／月</p>
            <p>※別途設置工事費用がかかります。</p>
            </div>
        </div>
    </div>
<div class="product wrap anchor" id="product">
    <h2>取り扱い機器</h2>
    <div class="product-box product01">
        <h3><span class="product-box-ttl-main">ADC-VC729P</span></h3>
        <div class="product-box-text">
            <p>ADC-VC729Pは投光ライトを搭載した屋外対応カメラです。ナイトビジョンと4K対応で昼夜を問わず確認できます。</p>
        </div>
    </div>
</div>
<div class="voice wrap anchor" id="voice">
    <h2><span>ユアテクトをご利用いただいた</span><div>お客様の声</div></h2>
    <div class="voice-text">
        <p>店舗の防犯対策として導入しました。導入後は、来店者がプッシュ通知でわかるので安心です。</p>
    </div>
</div>
<div class="case wrap anchor" id="case">
    <h2>様々な業種で<br>ご活用いただけます！</h2>
    <div class="case-text">
        <p class="case-text-ttl">接客の安心とスタッフ管理を同時に！</p>
    </div>
</div>
<div class="flow wrap anchor" id="flow">
    <h2>導入までの流れ</h2>
    <div class="flow-box flow-box01">
        <h3><b class="line">LINEからお問い合せ</b></h3>
        <p>LINE公式から簡単にお問い合わせできます！まずは資料請求だけでもOK。</p>
    </div>
</div>
<div class="faq wrap wrap-contents anchor" id="faq">
    <h2>よくあるご質問</h2>
    <div class="faq-box">
        <p class="q-text">導入までの流れについて教えてください。</p>
        <div class="a-text">
            <p>①お申し込み → ②現地確認 → ③お見積もり・ご案内 → ④設置工事 → ⑤ご利用開始という流れになります。</p>
        </div>
    </div>
</div>
</body>
</html>"##;

    #[test]
    fn extracts_exactly_seven_sections_in_nav_order() {
        let sections = extract_lp_sections(FIXTURE_HTML).expect("fixture must parse");
        assert_eq!(sections.len(), 7);
        let anchors: Vec<&str> = sections.iter().map(|s| s.anchor).collect();
        assert_eq!(
            anchors,
            vec!["about", "price", "product", "voice", "case", "flow", "faq"]
        );
        let titles: Vec<&str> = sections.iter().map(|s| s.title_ja.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "ユアテクトとは",
                "基本料金",
                "取り扱い機器",
                "お客様の声",
                "活用事例",
                "導入までの流れ",
                "よくあるご質問",
            ]
        );
    }

    #[test]
    fn every_section_body_is_non_empty() {
        let sections = extract_lp_sections(FIXTURE_HTML).expect("fixture must parse");
        for section in &sections {
            assert!(
                !section.body.trim().is_empty(),
                "anchor {} must have a non-empty body",
                section.anchor
            );
        }
    }

    #[test]
    fn about_section_does_not_leak_nav_text() {
        let sections = extract_lp_sections(FIXTURE_HTML).expect("fixture must parse");
        let about = sections.iter().find(|s| s.anchor == "about").unwrap();
        assert!(about.body.contains("防犯カメラの導入・設置"));
        assert!(!about.body.contains("よくあるご質問"));
    }

    #[test]
    fn missing_anchor_is_rejected() {
        // #price の id を剥がし、7 アンカーの1つが見つからない状態を模す。
        let broken = FIXTURE_HTML.replacen(
            r#"<div class="price wrap anchor" id="price">"#,
            r#"<div class="price wrap anchor" id="price-renamed">"#,
            1,
        );
        let err = extract_lp_sections(&broken).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("price") && message.contains("not found"),
            "expected a missing-anchor error naming price, got: {message}"
        );
    }

    #[test]
    fn empty_body_section_is_rejected() {
        // #faq を画像だけの空本文に差し替える。
        let broken = FIXTURE_HTML.replacen(
            r#"<div class="faq wrap wrap-contents anchor" id="faq">
    <h2>よくあるご質問</h2>
    <div class="faq-box">
        <p class="q-text">導入までの流れについて教えてください。</p>
        <div class="a-text">
            <p>①お申し込み → ②現地確認 → ③お見積もり・ご案内 → ④設置工事 → ⑤ご利用開始という流れになります。</p>
        </div>
    </div>
</div>"#,
            r#"<div class="faq wrap wrap-contents anchor" id="faq">
    <img alt="" src="img/faq.png">
</div>"#,
            1,
        );
        let err = extract_lp_sections(&broken).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("faq") && message.contains("empty"),
            "expected an empty-body error naming faq, got: {message}"
        );
    }

    #[test]
    fn price_section_amounts_are_not_broken_by_tag_splitting() {
        // 実サイトの price 節フィクスチャ（￥/1/,/540 がそれぞれ別 <span> に分割されたマークアップ）
        // から抽出した結果、金額がスペース混じりにならず、そのまま現れること。
        let sections = extract_lp_sections(FIXTURE_HTML).expect("fixture must parse");
        let price = sections.iter().find(|s| s.anchor == "price").unwrap();
        assert!(
            price.body.contains("￥1,540"),
            "expected the tag-split price figure to be joined into ￥1,540, got: {}",
            price.body
        );
        assert!(price.body.contains("2,640円"));
        assert!(price.body.contains("3,960円"));
        // 崩れた表記が残っていないことも確認する（読み違いの余地を残さない）。
        assert!(!price.body.contains("1 , 540"));
        assert!(!price.body.contains("￥ 1"));
    }

    // design doc `2026-10-07-partial-answer-with-handoff-design.md` §11.3「料金ブロックの
    // 整形」の受け入れ基準: フィクスチャから抽出した行構造を実ページの文字（半角/全角とも
    // 実ページの実際の表記に従う）でそのまま固定する。
    #[test]
    fn price_section_body_is_structured_as_label_value_lines() {
        let sections = extract_lp_sections(FIXTURE_HTML).expect("fixture must parse");
        let price = sections.iter().find(|s| s.anchor == "price").unwrap();
        assert_eq!(
            price.body,
            "基本料金\n\
             設置初期費用: 金額の記載なし\n\
             月額利用料金 (1台): ￥1,540(税込)~\n\
             カメラ2台: 2,640円（税込）／月\n\
             カメラ3台: 3,960円（税込）／月\n\
             ※別途設置工事費用がかかります。"
        );
    }

    #[test]
    fn price_section_rejects_unexpected_markup() {
        let name_label = r#"<p class="price-box-name">設置初期費用</p>"#;
        let bullet_tail = "・カメラ3台 → 3,960円（税込）／月</p>";
        let cases: &[(&str, &str, String, &str)] = &[
            (
                "zero h2",
                "<h2>基本料金</h2>",
                "<p>基本料金</p>".into(),
                "no h2 heading found",
            ),
            (
                "two h2",
                "<h2>基本料金</h2>",
                "<h2>基本料金</h2><h2>追加</h2>".into(),
                "more than one h2 heading",
            ),
            (
                "empty h2",
                "<h2>基本料金</h2>",
                "<h2> </h2>".into(),
                "the h2 heading extracted to an empty text",
            ),
            (
                "missing name label",
                name_label,
                r#"<p class="not-a-name-label">設置初期費用</p>"#.into(),
                "has no .price-box-name label",
            ),
            (
                "two name labels",
                name_label,
                format!(r#"{name_label}<p class="price-box-name">二重</p>"#),
                "more than one .price-box-name label",
            ),
            (
                "empty name label",
                name_label,
                r#"<p class="price-box-name"> </p>"#.into(),
                "a .price-box-name label extracted to an empty text",
            ),
            (
                "two price values",
                r#"<div class="price-box-price">"#,
                r#"<div class="price-box-price">1円</div><div class="price-box-price">"#.into(),
                "more than one .price-box-price value",
            ),
            (
                "no price box contents",
                r#"<div class="price-box">"#,
                r#"<div class="price-boxes">"#.into(),
                "no .price-box-contents price boxes found",
            ),
            (
                "missing trailing note paragraph",
                "<p>※別途設置工事費用がかかります。</p>",
                String::new(),
                "has 1 <p> element(s), need at least 2",
            ),
            (
                "bullet without marker",
                "・カメラ2台 → 2,640円",
                "カメラ2台 → 2,640円".into(),
                "does not start with",
            ),
            (
                "bullet without arrow",
                "・カメラ2台 → 2,640円（税込）／月",
                "・カメラ2台 2,640円（税込）／月".into(),
                "has 0 \"→\" separator(s), expected exactly 1",
            ),
            (
                "bullet with two arrows",
                "・カメラ2台 → 2,640円",
                "・カメラ2台 → → 2,640円".into(),
                "has 2 \"→\" separator(s), expected exactly 1",
            ),
            (
                "bullet paragraph with a child element",
                bullet_tail,
                "・カメラ3台 → 3,960円（税込）／月<b>強調</b></p>".into(),
                "unexpected <b> element",
            ),
            (
                "unconsumed text inside price-box-text",
                name_label,
                format!(r#"{name_label}<p class="price-box-note">4台目以降 +1,100円／月</p>"#),
                "4台目以降",
            ),
            (
                "unconsumed text directly under price-contents",
                r#"<div class="price-text">"#,
                r#"<p>追加料金 500円</p><div class="price-text">"#.into(),
                "追加料金",
            ),
        ];
        for (case, from, to, expected) in cases {
            let broken = FIXTURE_HTML.replacen(from, to, 1);
            assert_ne!(broken, FIXTURE_HTML, "case {case}: mutation did not apply");
            let err = extract_lp_sections(&broken)
                .expect_err(&format!("case {case}: markup must be rejected"));
            let message = format!("{err:#}");
            assert!(
                message.contains("anchor #price") && message.contains(expected),
                "case {case}: expected an error containing {expected:?}, got: {message}"
            );
        }
    }

    #[test]
    fn collapse_numeric_spacing_joins_currency_and_digit_grouping() {
        assert_eq!(
            collapse_numeric_spacing("￥ 1 , 540"),
            "￥1,540",
            "currency symbol and thousands-separator comma split across <span>s must be joined"
        );
    }

    #[test]
    fn collapse_numeric_spacing_joins_digit_and_yen_unit() {
        assert_eq!(collapse_numeric_spacing("2,640 円"), "2,640円");
    }

    #[test]
    fn collapse_numeric_spacing_is_idempotent_on_already_correct_amounts() {
        // 既に空白の無い正しい表記（スパン分割されていない通常のテキストノード由来）に
        // 適用しても内容が変わらないこと（誤爆が無いことの確認）。
        assert_eq!(collapse_numeric_spacing("2,640円"), "2,640円");
        assert_eq!(collapse_numeric_spacing("3,960円"), "3,960円");
    }

    #[test]
    fn collapse_numeric_spacing_preserves_ordinary_japanese_prose_spacing() {
        // 判断: 数字の前後に意図的な分かち書きがある文（例:「カメラ 2 台」）は、壊さない
        // （= 詰めない）設計にした。対象の3パターン（桁区切りカンマ/ピリオド・通貨記号・
        // 「円」）はいずれも「カメラ 2 台」に出現する文字（数字+空白+「台」）とは一致しない
        // ため、この判断はホワイトリストの狭さによって自然に実現されている（「台」を特別扱い
        // で除外しているわけではない）。理由: 本サイトの実コンテンツで単位がタグ分割されて
        // 空白混じりになる箇所は金額表記だけであり、「台」等まで対象を広げる実例が無い一方、
        // 広げた場合に将来の意図的な分かち書きを誤って壊すリスクがあるため、最小スコープに
        // 留める方を選んだ（`digit_yen_spacing_regex` の doc コメントと同じ判断）。
        assert_eq!(collapse_numeric_spacing("カメラ 2 台"), "カメラ 2 台");
        assert_eq!(
            collapse_numeric_spacing("月額利用料金 (1台)"),
            "月額利用料金 (1台)"
        );
    }

    #[test]
    fn collapse_numeric_spacing_joins_multiple_thousands_separators() {
        // 桁区切りが2つ以上あっても全て結合される（実データの金額は3桁区切りなので、
        // 1つ目のマッチ消費後も2つ目の区切りの左辺に未消費の数字が必ず残る。下の
        // `_known_limitation_` テストで、1桁グループしかない病的な入力に限って
        // この前提が崩れることを別途固定している）。
        assert_eq!(collapse_numeric_spacing("￥ 1 , 234 , 567"), "￥1,234,567");
    }

    #[test]
    fn collapse_numeric_spacing_known_limitation_does_not_join_the_second_separator_in_single_digit_groups(
    ) {
        // 既知の限界（正規表現は直さない: 実データの金額で崩れていないことは確認済みで、
        // ここを触る方がリスクが高い）。`digit_separator_spacing_regex`
        // (`(\d)\s*([,.])\s*(\d)`) は `replace_all` で左から非overlappingに消費されるため、
        // 1つ目のマッチが右端の数字まで使い切ってしまう。実在する金額は3桁区切り
        // （例: 1,234,567）なので、1つ目のマッチが消費する数字の直後に必ず2桁以上残り、
        // 次の区切りはその未消費の数字を左辺にして必ず結合される。1桁しかない桁グループ
        // （例: "1 , 2 , 3"）は実データに存在しないパターンであり、この限界が実害を
        // 及ぼす実例が無いため許容する。
        assert_eq!(collapse_numeric_spacing("1 , 2 , 3"), "1,2 , 3");
    }

    #[test]
    fn collapse_numeric_spacing_does_not_join_across_fullwidth_comma_or_period() {
        // digit_separator_spacing_regex の文字クラス `[,.]` は半角（ASCII）のみ。全角カンマ
        // 「，」(U+FF0C)・全角ピリオド「．」(U+FF0E) は意図的にスコープ外（実データの金額
        // 表記は半角カンマのみで書かれているため）。
        //
        // "￥ 1 ，540" では、￥と直後の数字の間の空白は別の独立した規則
        // (currency_digit_spacing_regex) で詰まる（￥→1 の結合は意図通り）が、全角カンマを
        // 挟む "1 ，540" 側は digit_separator_spacing_regex の対象外なので空白が残る。
        assert_eq!(collapse_numeric_spacing("￥ 1 ，540"), "￥1 ，540");
        // 通貨記号・円の影響を受けない単純な数字+全角カンマ/ピリオド+数字でも空白が保持
        // されることを確認する（全角記号そのものがスコープ外であることの直接確認）。
        assert_eq!(collapse_numeric_spacing("1 ，234"), "1 ，234");
        assert_eq!(collapse_numeric_spacing("1 ．234"), "1 ．234");
    }

    #[test]
    fn collapse_numeric_spacing_preserves_element_boundary_and_word_internal_cjk_spacing() {
        // `collapse_numeric_spacing` の対象を数字・桁区切り・通貨記号・「円」に限定している
        // のは意図的な設計判断である。CJK文字同士の間の空白一般まで対象を広げてはならない
        // （一度実装し、実データで有害と判明して差し戻した）。
        //
        // `extract_text_excluding_noise`（manual/crawl.rs）はテキストノードごとに空白を
        // 1つ挟むため、CJK同士の空白の大半は語の途中の分割ではなく、箇条書き・段落・
        // 別セルの区切り（要素境界）である。詰めると別項目が融合してしまう。実サイトの
        // --dry-run 出力からの実例（出典節を併記）:
        assert_eq!(
            collapse_numeric_spacing("基本料金 設置初期費用"), // price節: 見出しと別項目の区切り
            "基本料金 設置初期費用"
        );
        assert_eq!(
            collapse_numeric_spacing("BAAに対応 双方向Audio対応"), // product節: 機能リストの別項目
            "BAAに対応 双方向Audio対応"
        );
        assert_eq!(
            collapse_numeric_spacing("夜間の防犯、監視 工場 管理の見える化"), // case節: 別項目
            "夜間の防犯、監視 工場 管理の見える化"
        );
        assert_eq!(
            collapse_numeric_spacing("部門間モニタリング オフィス"), // case節: 別項目
            "部門間モニタリング オフィス"
        );

        // 一方、語中の分割に見える並び（"ユアテクト とは？"・"お客様 の 声"）もこの関数では
        // 詰めない。検索への影響は無い: `content_runs`（manual/retrieval.rs:93-97）は
        // カタカナ連続・漢字連続・ASCII英数字連続だけを内容語ランとして抽出し、ひらがな・
        // 記号・空白はラン区切りとして扱うため、"お客様の声" というクエリは
        // ["客様", "声"] のランに分解され、本文 "お客様 の 声" に対しても部分一致する
        // （run_term_frequency）。"ユアテクト とは？" も同様にラン ["ユアテクト"] が
        // 本文に一致する。
        assert_eq!(
            collapse_numeric_spacing("ユアテクト とは？"),
            "ユアテクト とは？"
        );
        assert_eq!(collapse_numeric_spacing("お客様 の 声"), "お客様 の 声");
    }

    #[test]
    fn lp_slug_is_stable_and_namespaced() {
        assert_eq!(lp_slug("price"), "urtectlp-price");
        // 同じ入力は常に同じ出力（冪等な upsert キー）。
        assert_eq!(lp_slug("price"), lp_slug("price"));
        // アンカーが違えば slug も違う。
        assert_ne!(lp_slug("price"), lp_slug("about"));
    }

    #[test]
    fn anchored_url_normalizes_trailing_slash() {
        assert_eq!(
            anchored_url(&Url::parse("https://urtect.cds2016.com/").unwrap(), "price"),
            "https://urtect.cds2016.com/#price"
        );
        assert_eq!(
            anchored_url(&Url::parse("https://urtect.cds2016.com").unwrap(), "price"),
            "https://urtect.cds2016.com/#price"
        );
    }

    #[test]
    fn anchored_url_places_fragment_after_existing_query_string() {
        // 文字列連結（旧実装）だと `https://host/?x=1` が `https://host/?x=1/#price` の
        // ように壊れる。`Url::set_fragment` ベースにしたことで query 文字列の後に
        // 正しく fragment が付くことを確認する。
        assert_eq!(
            anchored_url(
                &Url::parse("https://urtect.cds2016.com/?utm_source=x").unwrap(),
                "price"
            ),
            "https://urtect.cds2016.com/?utm_source=x#price"
        );
    }

    #[test]
    fn manual_section_node_has_required_attributes_and_fetched_at() {
        let node = build_lp_manual_section_node(
            "urtect",
            "urtectlp-price",
            "基本料金",
            "本文テキスト",
            "https://urtect.cds2016.com/#price",
            1,
            "2026-10-02T00:00:00+00:00",
        );
        assert_eq!(node.id, "urtect:gen1:ManualSection:urtectlp-price");
        assert_eq!(node.node_type, "ManualSection");
        let attrs: std::collections::HashMap<_, _> = node.attributes.into_iter().collect();
        assert_eq!(attrs.get("section_key").unwrap(), "urtectlp-price");
        assert_eq!(attrs.get("doc_key").unwrap(), "doc-urtect-lp");
        assert_eq!(attrs.get("title").unwrap(), "基本料金");
        assert_eq!(attrs.get("body").unwrap(), "本文テキスト");
        assert_eq!(
            attrs.get("source_url").unwrap(),
            "https://urtect.cds2016.com/#price"
        );
        assert_eq!(attrs.get("breadcrumb").unwrap(), "基本料金");
        assert_eq!(attrs.get("order").unwrap(), "1");
        assert_eq!(attrs.get("source_lang").unwrap(), "ja");
        assert_eq!(
            attrs.get("fetched_at").unwrap(),
            "2026-10-02T00:00:00+00:00"
        );
        assert!(!attrs.get("content_hash").unwrap().is_empty());
    }

    #[test]
    fn has_section_edge_points_from_doc_to_section() {
        let edge = build_lp_has_section_edge("urtect", "urtectlp-price");
        assert_eq!(edge.from_id, "urtect:gen1:ManualDocument:doc-urtect-lp");
        assert_eq!(edge.to_id, "urtect:gen1:ManualSection:urtectlp-price");
        assert_eq!(edge.edge_type, "HAS_SECTION");
    }

    #[test]
    fn building_the_same_section_twice_with_identical_inputs_is_deterministic() {
        // 決定性（same input → same output）の確認。本番の upsert 冪等性（fetched_at が
        // 毎回変わっても id・安定キーは変わらないこと）はこのテストでは証明できない
        // （下の `manual_section_node_id_and_stable_keys_are_unaffected_by_fetched_at` が
        // それを別に固定している。codex レビュー Warning 3: 同一呼び出しの比較は決定性の
        // 証明にしかならない）。
        let a = build_lp_manual_section_node(
            "urtect",
            "urtectlp-about",
            "ユアテクトとは",
            "本文",
            "https://urtect.cds2016.com/#about",
            0,
            "2026-10-02T00:00:00+00:00",
        );
        let b = build_lp_manual_section_node(
            "urtect",
            "urtectlp-about",
            "ユアテクトとは",
            "本文",
            "https://urtect.cds2016.com/#about",
            0,
            "2026-10-02T00:00:00+00:00",
        );
        assert_eq!(a, b);
    }

    #[test]
    fn manual_section_node_id_and_stable_keys_are_unaffected_by_fetched_at() {
        // 本番の upsert 冪等性の実体はこちら。本番は毎回 `fetched_at`（main 内
        // `chrono::Utc::now()`、167行目付近）が変わるため、同一入力の決定性だけでは
        // 冪等性を証明しない。「fetched_at が異なっても id・安定キー（section_key・
        // doc_key・content_hash・body・title・source_url・breadcrumb・order・
        // source_lang）は変わらず、差分は fetched_at の値のみ」であることを確認し、これが
        // vegapunk 側の upsert が同一ノードを更新する（新規ノードを作らない）ことの
        // 根拠になる。
        let a = build_lp_manual_section_node(
            "urtect",
            "urtectlp-about",
            "ユアテクトとは",
            "本文",
            "https://urtect.cds2016.com/#about",
            0,
            "2026-10-02T00:00:00+00:00",
        );
        let b = build_lp_manual_section_node(
            "urtect",
            "urtectlp-about",
            "ユアテクトとは",
            "本文",
            "https://urtect.cds2016.com/#about",
            0,
            "2026-10-03T12:34:56+00:00", // 異なる fetched_at（本番で毎回変わる値を模す）
        );
        assert_eq!(a.id, b.id);
        assert_eq!(a.node_type, b.node_type);

        let attrs_a: std::collections::BTreeMap<String, String> =
            a.attributes.into_iter().collect();
        let attrs_b: std::collections::BTreeMap<String, String> =
            b.attributes.into_iter().collect();

        // 属性キー集合そのものは一致する（fetched_at の値で増減しない）。
        let keys_a: std::collections::BTreeSet<&String> = attrs_a.keys().collect();
        let keys_b: std::collections::BTreeSet<&String> = attrs_b.keys().collect();
        assert_eq!(keys_a, keys_b);

        // 差分は fetched_at のみであること。
        let mut differing_keys: Vec<&str> = Vec::new();
        for (key, value_a) in &attrs_a {
            if attrs_b.get(key) != Some(value_a) {
                differing_keys.push(key.as_str());
            }
        }
        assert_eq!(
            differing_keys,
            vec!["fetched_at"],
            "fetched_at 以外の属性まで fetched_at に伴って変わってしまっている: \
             {differing_keys:?}"
        );
    }

    // Issue #69 reviewer指摘(Warning 2): build_lp_manual_section_node は
    // manual::ingest_model::build_section_graph を使わず GraphNode を直接組み立てている
    // (理由はファイル冒頭コメント)。vegapunk 側は未宣言属性の書き込みを拒否しうるため
    // (ingest_rules.rs の
    // both_schema_files_declare_every_attribute_written_for_escalation_rule_nodes、
    // harness/knowledge.rs の cs_support_yml_declares_every_support_case_attribute_that_code_writes
    // と同じ障害クラス)、この guard が無いと schema 側に required 属性が追加されたときに
    // サイレントに書き漏れる。以下1本は「書き込み属性 ⊆ 宣言属性」かつ「宣言された
    // required 属性が1つも欠落しない(値も空でない)」ことを固定し、将来 schema に required
    // 属性が追加されたらこのテストが落ちることを目的とする。

    #[test]
    fn manual_section_node_attributes_match_schema_cs_support_yml() {
        let node = build_lp_manual_section_node(
            "urtect",
            "urtectlp-price",
            "基本料金",
            "本文テキスト",
            "https://urtect.cds2016.com/#price",
            1,
            "2026-10-02T00:00:00+00:00",
        );
        let written: std::collections::BTreeMap<String, String> =
            node.attributes.into_iter().collect();

        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../schema/cs-support.yml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read schema file {path:?}: {e}"));
        let value: serde_yaml::Value = serde_yaml::from_str(&text)
            .unwrap_or_else(|e| panic!("parse schema file {path:?} as YAML: {e}"));
        let attributes = value["nodes"]["ManualSection"]["attributes"]
            .as_mapping()
            .unwrap_or_else(|| panic!("{path:?}: nodes.ManualSection.attributes is not a mapping"));

        let declared: std::collections::BTreeSet<&str> = attributes
            .keys()
            .filter_map(serde_yaml::Value::as_str)
            .collect();
        let written_keys: std::collections::BTreeSet<&str> =
            written.keys().map(String::as_str).collect();
        let undeclared: Vec<&&str> = written_keys.difference(&declared).collect();
        assert!(
            undeclared.is_empty(),
            "schema/cs-support.yml nodes.ManualSection.attributes is missing keys that \
             build_lp_manual_section_node writes: {undeclared:?}"
        );

        for (key, def) in attributes.iter() {
            let is_required =
                def.get("required").and_then(serde_yaml::Value::as_bool) == Some(true);
            if !is_required {
                continue;
            }
            let key = key
                .as_str()
                .unwrap_or_else(|| panic!("{path:?}: non-string attribute key"));
            let value = written.get(key).unwrap_or_else(|| {
                panic!(
                    "build_lp_manual_section_node does not write required attribute {key:?} \
                     declared in schema/cs-support.yml"
                )
            });
            assert!(
                !value.is_empty(),
                "required attribute {key:?} must not be written empty, got {value:?}"
            );
        }
    }
}
