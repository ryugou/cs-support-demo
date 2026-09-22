use anyhow::{Context, Result};
use clap::Parser;
use cs_support_mcp::{
    harness::{
        knowledge::harness_node_id,
        rules::{Binding, HearingContract},
    },
    model::{GraphBuild, GraphNode},
    vegapunk::VegapunkClient,
};
use serde::Deserialize;
use serde_json::json;
use std::{env, fs, path::PathBuf};

/// 第1層 escalation_rule / 第2層 prohibited_domain の初期データ投入と
/// Step 1 加算スキーマの登録を行う CLI（spec S1-2 / S1-9）。
#[derive(Debug, Parser)]
struct Args {
    #[arg(
        long,
        env = "VEGAPUNK_ENDPOINT",
        default_value = "http://vegapunk.local:6840"
    )]
    endpoint: String,
    #[arg(long, default_value = "sivira-cs-demo")]
    schema: String,
    #[arg(long, default_value = "VEGAPUNK_BEARER_TOKEN")]
    token_env: String,
    #[arg(long, env = "VEGAPUNK_BEARER_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    #[arg(long, default_value = "../schema/cs-schema.yml")]
    schema_file: PathBuf,
    #[arg(long, default_value = "data/rules.sample.json")]
    rules_file: PathBuf,
}

// 3 つの入力構造体はすべて `deny_unknown_fields`（理由は `parse_rules_file` の doc）。
// フィールドを足すときは、この構造体へ足さない限り rules ファイルに書けない（投入前に落ちる）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    escalation_rules: Vec<RuleInput>,
    prohibited_domains: Vec<DomainInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleInput {
    rule_id: String,
    condition: Vec<String>,
    owner: Option<String>,
    route: String,
    /// **必須**（`resolve_binding` が検証する）。`Option` にしているのは、欠落を serde の
    /// `missing field` ではなく rule_id と有効値の一覧を名指しするエラーにするため
    /// （serde の欠落エラーは配列の何番目の要素かを名指ししない）。
    binding: Option<String>,
    /// このルールにマッチしたターンで、どの情報契約で聞き返し（ヒアリング）するかの宣言
    /// （`HearingContract` の識別子。例: `product_and_symptom`）。省略は「宣言なし」。
    /// 詳細は `HearingContract` の doc コメント。
    hearing: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DomainInput {
    domain_id: String,
    #[serde(default)]
    domain_signals: Vec<String>,
    #[serde(default)]
    pattern: Vec<String>,
    route: String,
    /// **必須**（`resolve_binding` が検証する。`Option` の理由は `RuleInput.binding` と同じ）。
    binding: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let token = read_token(&args)?;

    // 1. 第1層ルール・第2層領域をノードへ変換する（判定は載せない・材料のみ、I4）。
    // ここまではすべてローカルな読み込み・検証であり、vegapunk へは一切接続していない。
    // 検証（未知フィールド名・未知の binding 値・未知の hearing 契約・mandatory への宣言）は
    // ここで落とす。vegapunk へ接続する前に落ちるため、設定ミスがあってもスキーマ登録・
    // ノード投入のどちらも発生しない。
    let rules_body = fs::read_to_string(&args.rules_file)
        .with_context(|| format!("read rules file {}", args.rules_file.display()))?;
    let rules = parse_rules_file(&rules_body)
        .with_context(|| format!("rules file {}", args.rules_file.display()))?;
    let mut nodes = build_escalation_rule_nodes(&args.schema, &rules.escalation_rules)?;
    nodes.extend(build_prohibited_domain_nodes(
        &args.schema,
        &rules.prohibited_domains,
    )?);

    // 2. 加算スキーマ登録（既存 schema に node/edge type を足す）。ここから先で初めて
    // vegapunk へ接続する。
    // 汎用テンプレを別テナントに登録する場合に備え name をリクエスト schema 名へ揃える
    // （同名なら no-op。vegapunk は create_schema 時に YAML name と登録名の一致を要求）。
    let schema_yaml = cs_support_mcp::manual::schema_ids::with_schema_name(
        &fs::read_to_string(&args.schema_file)
            .with_context(|| format!("read schema file {}", args.schema_file.display()))?,
        &args.schema,
    )?;
    let client = VegapunkClient::connect(&args.endpoint, &token).await?;
    client
        .create_or_update_schema(&args.schema, schema_yaml)
        .await?;
    tracing::info!(schema = %args.schema, "schema updated (additive)");

    // 3. 手順 1 で組み立てた第1層ルール・第2層領域ノードを投入する。
    let build = GraphBuild {
        nodes,
        edges: Vec::new(),
    };
    let (node_count, edge_count) = client.upsert_graph_low_level(build).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": args.schema,
            "upserted_nodes": node_count,
            "upserted_edges": edge_count,
        }))?
    );
    Ok(())
}

/// rules ファイルのパース。`main` とテストが同じ経路を通る。
///
/// 入力構造体（`RulesFile` / `RuleInput` / `DomainInput`）はすべて `deny_unknown_fields`。
/// rules ファイルの各フィールドは判定の挙動を左右し、serde の既定（未知フィールドを黙って
/// 捨てる）だと綴りミスが「宣言なし」として通ってしまうため（例: `hearing` の綴りミスで本番の
/// Jev が永久に呼ばれない、`domain_signals` の綴りミスで禁止領域が signal で止まらなくなる。
/// どちらもエラーもログも出ない）。未知フィールドは、serde が有効なフィールド名の一覧と
/// 一緒に名指しするエラーで投入前に落とす（`main` の終了時に原因チェーンとして表示される）。
fn parse_rules_file(body: &str) -> Result<RulesFile> {
    serde_json::from_str(body).context("parse rules file")
}

/// `binding` を**必須**かつ**完全一致**で検証して `Binding` にする。既定値は無い。
///
/// - 省略（キーなし・`null`）: `rule <id>: missing binding ...` でエラー
/// - 未知の値（綴りミス `"manadatory"`・大文字違い `"Mandatory"`・前後空白・空文字）:
///   `rule <id>: unknown binding ...` でエラー
///
/// どちらも rule_id / domain_id と有効な値の一覧を名指しする（`hearing` の未知値と同じ規律）。
///
/// なぜ既定値を持たせないか: 実行時のローダ（`knowledge::parse_binding`）は未知の値・属性なしを
/// advisory に倒す（fail-back。fail closed にすると 1 件のエラーで第1層・第2層の全件が読めなく
/// なるため）。したがって、書き込み側のここで落とさないと、mandatory の綴りミスや `binding` の
/// 書き忘れが黙って advisory になる。その場合 `match_layer1` の mandatory 優先・`decide` の
/// `missing` 抑止・`hearing` の mandatory ガード（この CLI の検証と実行時の
/// `hearing_contract`）の 3 つが同時に破れ、`human-handoff`（聞き返しループからの脱出手段）の
/// ような mandatory が Jev のヒアリングに吸収されうる。CLI は fail-closed、ローダは fail-back
/// という非対称は意図した設計。
///
/// 旧仕様は省略を許し、第1層は advisory・第2層は mandatory を既定にしていた（層ごとに逆の
/// 既定値で、ローダの「属性なしは advisory」とも食い違っていた）。必須化でこの省略経路ごと消えた。
/// 現行の `rules.json` / `rules.sample.json` は全件明示している。
///
/// `label` はエラー文言用（`rule` / `domain`）。
fn resolve_binding(label: &str, id: &str, value: Option<&str>) -> Result<Binding> {
    let valid_values = || Binding::ALL.map(Binding::as_str).join(", ");
    let Some(value) = value else {
        anyhow::bail!(
            "{label} {id}: missing binding; binding is required (there is no default: an omitted \
             binding must not silently weaken a {label}); expected one of: {}",
            valid_values()
        );
    };
    Binding::parse(value).with_context(|| {
        format!(
            "{label} {id}: unknown binding {value:?}; expected one of: {}",
            valid_values()
        )
    })
}

/// 第2層領域（`prohibited_domains`）を vegapunk の `ProhibitedDomain` ノードへ変換する。
///
/// `binding` は必須で、[`resolve_binding`] が完全一致で検証する（省略は拒否。既定値は無い）。
fn build_prohibited_domain_nodes(schema: &str, domains: &[DomainInput]) -> Result<Vec<GraphNode>> {
    domains
        .iter()
        .map(|domain| {
            let binding = resolve_binding("domain", &domain.domain_id, domain.binding.as_deref())?;
            Ok(GraphNode {
                id: harness_node_id(schema, "ProhibitedDomain", &domain.domain_id),
                node_type: "ProhibitedDomain".to_string(),
                attributes: vec![
                    ("domain_id".to_string(), domain.domain_id.clone()),
                    (
                        "domain_signals".to_string(),
                        domain.domain_signals.join(","),
                    ),
                    ("pattern".to_string(), domain.pattern.join(",")),
                    ("route".to_string(), domain.route.clone()),
                    ("binding".to_string(), binding.as_str().to_string()),
                ],
            })
        })
        .collect()
}

/// 第1層ルール（`escalation_rules`）を vegapunk の `EscalationRule` ノードへ変換する。
///
/// 書き込む属性は実行時のローダ（`harness::knowledge::escalation_rule_from_attributes`）が読む
/// ものと対になる。`hearing` は宣言の有無に関わらず**常に書く**（宣言なしは空文字）。属性を
/// 省略すると、vegapunk の `UpsertNodes` が部分マージの場合に「宣言を消した rules.json の再投入」
/// が旧い宣言を残したままになりうるため（`owner` が空文字を書くのと同じ規律）。
///
/// 次の設定ミスは、`main` がこの関数を vegapunk への接続（schema 登録・ノード投入）より前に
/// 呼ぶことで、**vegapunk へ接続する前に**エラーにする（実行時のローダは未知の値を未宣言・
/// advisory に倒すため、ここで落とさないと綴りミスがサイレントに「Jev が永久に呼ばれない」
/// 「mandatory が advisory になる」になる）:
/// - `binding` の省略、および未知の `binding` 値（[`resolve_binding`]。**`binding` は必須**で
///   既定値は無い。書き忘れが黙って advisory になるのを防ぐ）
/// - 未知の `hearing` 識別子（綴りミス・大文字・前後空白）
/// - `binding = mandatory` のルールへの `hearing` 宣言（mandatory は情報の有無を問わず即
///   エスカレーションする拘束度そのもの。実行時にも `EscalationRule::hearing_contract` が無効化する）
fn build_escalation_rule_nodes(schema: &str, rules: &[RuleInput]) -> Result<Vec<GraphNode>> {
    rules
        .iter()
        .map(|rule| {
            let binding = resolve_binding("rule", &rule.rule_id, rule.binding.as_deref())?;
            let hearing = match rule.hearing.as_deref() {
                None => "",
                Some(value) => {
                    let contract = HearingContract::parse(value).with_context(|| {
                        format!(
                            "rule {}: unknown hearing contract {value:?}; expected one of: {}",
                            rule.rule_id,
                            HearingContract::ALL.map(HearingContract::as_str).join(", ")
                        )
                    })?;
                    if binding == Binding::Mandatory {
                        anyhow::bail!(
                            "rule {}: binding=mandatory must not declare a hearing contract \
                             ({value:?}); mandatory escalates unconditionally, so remove `hearing` \
                             or change the binding to advisory",
                            rule.rule_id
                        );
                    }
                    contract.as_str()
                }
            };
            Ok(GraphNode {
                id: harness_node_id(schema, "EscalationRule", &rule.rule_id),
                node_type: "EscalationRule".to_string(),
                attributes: vec![
                    ("rule_id".to_string(), rule.rule_id.clone()),
                    ("condition".to_string(), rule.condition.join(",")),
                    ("owner".to_string(), rule.owner.clone().unwrap_or_default()),
                    ("route".to_string(), rule.route.clone()),
                    ("binding".to_string(), binding.as_str().to_string()),
                    ("hearing".to_string(), hearing.to_string()),
                ],
            })
        })
        .collect()
}

fn read_token(args: &Args) -> Result<String> {
    if let Ok(token) = env::var(&args.token_env) {
        if !token.trim().is_empty() {
            return Ok(token.trim().to_string());
        }
    }
    if let Some(path) = &args.token_file {
        return fs::read_to_string(path)
            .with_context(|| format!("read token file {}", path.display()))
            .map(|s| s.trim().to_string());
    }
    anyhow::bail!(
        "vegapunk bearer token is required (env {} or VEGAPUNK_BEARER_TOKEN_FILE)",
        args.token_env
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cs_support_mcp::harness::{
        knowledge::escalation_rule_from_attributes, rules::HearingContract,
    };
    use std::collections::HashMap;

    /// 本番の `ingest-rules` job が投入する image 内ファイルそのもの（`server/data/urtect/rules.json`）。
    /// 手組みの JSON ではなく実データを CLI のパース経路へ通し、新フィールドで壊れないことを固定する。
    const BUNDLED_URTECT_RULES: &str = include_str!("../../data/urtect/rules.json");
    /// `hearing` を持たない従来形式のファイル（後方互換の確認用）。
    const SAMPLE_RULES: &str = include_str!("../../data/rules.sample.json");

    /// CLI と同じパース経路（`parse_rules_file`）で読む。未知フィールドの拒否もここで効く。
    fn parse_rules(body: &str) -> RulesFile {
        parse_rules_file(body).expect("rules file must parse through the CLI's RulesFile")
    }

    /// エラーの原因チェーン全体（`{:#}`）。`main` が `Result` を返して終了するとき、運用者が
    /// 目にするのはこのチェーン（context の下の serde の原因まで）なので、名指しの検証もこちらで行う。
    fn error_chain(err: &anyhow::Error) -> String {
        format!("{err:#}")
    }

    fn attribute<'a>(node: &'a GraphNode, key: &str) -> Option<&'a str> {
        node.attributes
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn rule_json(binding: Option<&str>, hearing: Option<&str>) -> RulesFile {
        let mut rule = json!({
            "rule_id": "probe-rule",
            "condition": ["probe_signal"],
            "route": "support_desk",
        });
        if let Some(binding) = binding {
            rule["binding"] = json!(binding);
        }
        if let Some(hearing) = hearing {
            rule["hearing"] = json!(hearing);
        }
        parse_rules(&json!({ "escalation_rules": [rule], "prohibited_domains": [] }).to_string())
    }

    #[test]
    fn bundled_urtect_rules_parse_and_only_warranty_failure_writes_a_hearing_attribute() {
        let rules = parse_rules(BUNDLED_URTECT_RULES);
        let nodes = build_escalation_rule_nodes("urtect", &rules.escalation_rules)
            .expect("the bundled rules.json must build into graph nodes");
        assert_eq!(nodes.len(), rules.escalation_rules.len());

        for node in &nodes {
            let rule_id = attribute(node, "rule_id").expect("every node carries rule_id");
            // 宣言の無いルールは「属性なし」ではなく**空文字**を明示的に書く。vegapunk の
            // UpsertNodes が部分マージでも、宣言を消した再投入が確実に反映されるようにするため。
            let expected = if rule_id == "warranty-failure" {
                "product_and_symptom"
            } else {
                ""
            };
            assert_eq!(
                attribute(node, "hearing"),
                Some(expected),
                "rule {rule_id}: the hearing attribute the CLI writes to vegapunk"
            );
        }
    }

    // CLI が書いた属性を、本番サービスが実際に使うローダ（`escalation_rule_from_attributes`）へ
    // 通して、宣言が Jev 呼び出し可否まで届くことを固定する（CLI が書かない・ローダが読まない、
    // どちらの退行も「本番で Jev が永久に呼ばれない」サイレント never-match になる）。
    #[test]
    fn bundled_urtect_rules_round_trip_through_the_runtime_loader() {
        let rules = parse_rules(BUNDLED_URTECT_RULES);
        let nodes = build_escalation_rule_nodes("urtect", &rules.escalation_rules)
            .expect("the bundled rules.json must build into graph nodes");

        let declared: Vec<(String, HearingContract)> = nodes
            .iter()
            .map(|node| {
                let attrs: HashMap<String, String> = node.attributes.iter().cloned().collect();
                escalation_rule_from_attributes(&attrs)
                    .expect("the runtime loader must accept every node the CLI writes")
            })
            .filter_map(|rule| {
                rule.hearing_contract()
                    .map(|contract| (rule.id.clone(), contract))
            })
            .collect();

        assert_eq!(
            declared,
            vec![(
                "warranty-failure".to_string(),
                HearingContract::ProductAndSymptom
            )]
        );
    }

    #[test]
    fn rules_file_without_any_hearing_field_still_parses_and_writes_empty_declarations() {
        let rules = parse_rules(SAMPLE_RULES);
        let nodes = build_escalation_rule_nodes("sivira-cs-demo", &rules.escalation_rules)
            .expect("a pre-existing rules file without hearing must keep working");
        assert!(!nodes.is_empty());
        for node in &nodes {
            assert_eq!(attribute(node, "hearing"), Some(""));
        }
    }

    #[test]
    fn an_advisory_rule_may_declare_a_hearing() {
        let rules = rule_json(Some("advisory"), Some("product_and_symptom"));
        let nodes = build_escalation_rule_nodes("urtect", &rules.escalation_rules)
            .expect("an advisory rule may declare a hearing");
        assert_eq!(attribute(&nodes[0], "binding"), Some("advisory"));
        assert_eq!(attribute(&nodes[0], "hearing"), Some("product_and_symptom"));
    }

    #[test]
    fn unknown_hearing_contract_is_rejected_naming_the_rule_the_value_and_the_valid_ones() {
        // 綴りミスの宣言を黙って「宣言なし」として投入すると、warranty-failure でも Jev が永久に
        // 呼ばれなくなる。投入前（CI の ingest-rules job）で落として運用者に見える形にする。
        let rules = rule_json(Some("advisory"), Some("product_and_sympton"));
        let err = build_escalation_rule_nodes("urtect", &rules.escalation_rules)
            .expect_err("a typo'd hearing contract must be rejected")
            .to_string();
        assert!(err.contains("probe-rule"), "must name the rule, got: {err}");
        assert!(
            err.contains("product_and_sympton"),
            "must echo the offending value, got: {err}"
        );
        assert!(
            err.contains("product_and_symptom"),
            "must list the valid identifiers so the operator knows the next action, got: {err}"
        );
    }

    #[test]
    fn hearing_declared_on_a_mandatory_rule_is_rejected() {
        // mandatory は情報の有無を問わず問答無用で即エスカレーションする拘束度そのもの。
        // 宣言を付けても実行時は無視される（`EscalationRule::hearing_contract`）が、設定ミスは
        // 投入時に落として気付かせる。
        let rules = rule_json(Some("mandatory"), Some("product_and_symptom"));
        let err = build_escalation_rule_nodes("urtect", &rules.escalation_rules)
            .expect_err("a mandatory rule must not declare a hearing")
            .to_string();
        assert!(err.contains("probe-rule"), "must name the rule, got: {err}");
        assert!(
            err.contains("mandatory"),
            "must explain the binding conflict, got: {err}"
        );
    }

    // --- binding の値の検証（Issue #58 reviewer Warning 1） ---
    //
    // `binding` の値の綴りミス（"manadatory" / "Mandatory"）を黙って advisory 扱いにすると、
    // mandatory の即時エスカレーション契約が同時に破れる: `match_layer1` の mandatory 優先が
    // 効かず、`decide` が積まないはずの `missing` を積み、`hearing` の mandatory ガード（この CLI の
    // 検証と実行時の `hearing_contract`）も通らない。`human-handoff`（聞き返しループからの脱出手段）
    // が Jev のヒアリングに吸収されうる。実行時のローダは fail-back（advisory に倒して warn）なので、
    // 書き込み側のここで完全一致の検証をして、投入前に落とす。

    fn domain_json(binding: Option<&str>) -> RulesFile {
        let mut domain = json!({
            "domain_id": "probe-domain",
            "domain_signals": ["probe_signal"],
            "pattern": [],
            "route": "support_desk",
        });
        if let Some(binding) = binding {
            domain["binding"] = json!(binding);
        }
        parse_rules(&json!({ "escalation_rules": [], "prohibited_domains": [domain] }).to_string())
    }

    #[test]
    fn an_unknown_binding_on_an_escalation_rule_is_rejected_naming_the_rule_the_value_and_the_valid_ones(
    ) {
        let rules = rule_json(Some("manadatory"), None);
        let err = build_escalation_rule_nodes("urtect", &rules.escalation_rules)
            .expect_err("a typo'd binding must be rejected")
            .to_string();
        assert!(err.contains("probe-rule"), "must name the rule, got: {err}");
        assert!(
            err.contains("manadatory"),
            "must echo the offending value, got: {err}"
        );
        assert!(
            err.contains("mandatory, advisory"),
            "must list the valid values so the operator knows the next action, got: {err}"
        );
    }

    #[test]
    fn an_unknown_binding_on_a_prohibited_domain_is_rejected_naming_the_domain_the_value_and_the_valid_ones(
    ) {
        let rules = domain_json(Some("manadatory"));
        let err = build_prohibited_domain_nodes("urtect", &rules.prohibited_domains)
            .expect_err("a typo'd binding must be rejected")
            .to_string();
        assert!(
            err.contains("probe-domain"),
            "must name the domain, got: {err}"
        );
        assert!(
            err.contains("manadatory"),
            "must echo the offending value, got: {err}"
        );
        assert!(
            err.contains("mandatory, advisory"),
            "must list the valid values, got: {err}"
        );
    }

    #[test]
    fn binding_matching_is_exact_so_case_variants_and_empty_are_rejected() {
        // 大文字違い・前後空白・空文字を黙って通すと、ローダ側で advisory に倒れる（=拘束度が
        // 下がる）。完全一致であることを、escalation_rules と prohibited_domains の両方で固定する。
        for value in ["Mandatory", "ADVISORY", " mandatory", "advisory ", ""] {
            let rules = rule_json(Some(value), None);
            assert!(
                build_escalation_rule_nodes("urtect", &rules.escalation_rules).is_err(),
                "escalation rule binding {value:?} must be rejected"
            );
            let domains = domain_json(Some(value));
            assert!(
                build_prohibited_domain_nodes("urtect", &domains.prohibited_domains).is_err(),
                "prohibited domain binding {value:?} must be rejected"
            );
        }
    }

    #[test]
    fn a_misspelled_binding_can_not_slip_a_hearing_declaration_past_the_mandatory_guard() {
        // 検証が無かった頃は、"manadatory" は `binding == "mandatory"` の判定を通らず、typo した
        // mandatory ルールが `hearing` を宣言できてしまった（CLI・実行時・decide の 3 つの
        // mandatory 防御が同時に破れる）。binding の値の検証がその入口を塞ぐ。
        let rules = rule_json(Some("manadatory"), Some("product_and_symptom"));
        let err = build_escalation_rule_nodes("urtect", &rules.escalation_rules)
            .expect_err("must be rejected before any hearing declaration is accepted")
            .to_string();
        assert!(
            err.contains("binding") && err.contains("manadatory"),
            "the error must point at the misspelled binding, got: {err}"
        );
    }

    // --- binding は必須（省略は投入前に拒否する） ---
    //
    // 値の綴りミスを拒否しても、キーごと書き忘れると黙って既定値（第1層は advisory）になる。
    // `human-handoff` の `binding` を書き忘れれば、担当者取次が聞き返しループに吸収される
    // （綴りミスと同じ帰結）。入力形式を狭めるのは意図した fail-closed。既定値を持たないので、
    // 第1層（旧既定 advisory）と第2層（旧既定 mandatory）の既定値の非対称も無い。

    #[test]
    fn an_omitted_binding_on_an_escalation_rule_is_rejected_naming_the_rule_and_the_valid_values() {
        let rules = rule_json(None, None);
        let err = build_escalation_rule_nodes("urtect", &rules.escalation_rules)
            .expect_err("an omitted binding must be rejected")
            .to_string();
        assert!(err.contains("probe-rule"), "must name the rule, got: {err}");
        assert!(
            err.contains("missing binding"),
            "must say that the binding is missing, got: {err}"
        );
        assert!(
            err.contains("mandatory, advisory"),
            "must list the valid values so the operator knows the next action, got: {err}"
        );
    }

    #[test]
    fn an_omitted_binding_on_a_prohibited_domain_is_rejected_naming_the_domain_and_the_valid_values(
    ) {
        let rules = domain_json(None);
        let err = build_prohibited_domain_nodes("urtect", &rules.prohibited_domains)
            .expect_err("an omitted binding must be rejected")
            .to_string();
        assert!(
            err.contains("probe-domain"),
            "must name the domain, got: {err}"
        );
        assert!(
            err.contains("missing binding"),
            "must say that the binding is missing, got: {err}"
        );
        assert!(
            err.contains("mandatory, advisory"),
            "must list the valid values, got: {err}"
        );
    }

    #[test]
    fn a_null_binding_is_rejected_like_an_omitted_one() {
        // `"binding": null` は serde では `None`（省略と同じ）。同じ規律で拒否する。
        let body = json!({
            "escalation_rules": [{
                "rule_id": "null-binding-rule",
                "condition": ["probe_signal"],
                "route": "support_desk",
                "binding": null,
            }],
            "prohibited_domains": [{
                "domain_id": "null-binding-domain",
                "domain_signals": ["probe_signal"],
                "pattern": [],
                "route": "support_desk",
                "binding": null,
            }],
        })
        .to_string();
        let rules = parse_rules(&body);
        let rule_err = build_escalation_rule_nodes("urtect", &rules.escalation_rules)
            .expect_err("a null binding must be rejected")
            .to_string();
        assert!(rule_err.contains("null-binding-rule"), "got: {rule_err}");
        let domain_err = build_prohibited_domain_nodes("urtect", &rules.prohibited_domains)
            .expect_err("a null binding must be rejected")
            .to_string();
        assert!(
            domain_err.contains("null-binding-domain"),
            "got: {domain_err}"
        );
    }

    #[test]
    fn forgetting_the_binding_on_a_bundled_rule_or_domain_is_rejected_naming_it() {
        // 実データから 1 件だけ `binding` を落として、書き忘れた当のルールが名指しされること
        // （`human-handoff` の書き忘れが黙って advisory になるのが、この必須化が塞ぐ経路）。
        let mut value: serde_json::Value =
            serde_json::from_str(BUNDLED_URTECT_RULES).expect("bundled rules.json is valid JSON");
        value["escalation_rules"]
            .as_array_mut()
            .expect("escalation_rules is an array")
            .iter_mut()
            .find(|rule| rule["rule_id"] == "human-handoff")
            .expect("bundled rules.json defines human-handoff")
            .as_object_mut()
            .expect("a rule is an object")
            .remove("binding")
            .expect("human-handoff declares a binding today");
        value["prohibited_domains"]
            .as_array_mut()
            .expect("prohibited_domains is an array")
            .iter_mut()
            .find(|domain| domain["domain_id"] == "legal-privacy")
            .expect("bundled rules.json defines legal-privacy")
            .as_object_mut()
            .expect("a domain is an object")
            .remove("binding")
            .expect("legal-privacy declares a binding today");
        let rules = parse_rules(&value.to_string());

        let rule_err = build_escalation_rule_nodes("urtect", &rules.escalation_rules)
            .expect_err("a rule without binding must be rejected")
            .to_string();
        assert!(
            rule_err.contains("human-handoff") && rule_err.contains("missing binding"),
            "must name the rule that lost its binding, got: {rule_err}"
        );
        let domain_err = build_prohibited_domain_nodes("urtect", &rules.prohibited_domains)
            .expect_err("a domain without binding must be rejected")
            .to_string();
        assert!(
            domain_err.contains("legal-privacy") && domain_err.contains("missing binding"),
            "must name the domain that lost its binding, got: {domain_err}"
        );
    }

    #[test]
    fn bundled_and_sample_rules_files_build_every_node_with_the_binding_they_declare() {
        // 値の検証を足しても、現行の rules ファイルはそのまま通り、宣言どおりの binding が
        // 書かれる（勝手に書き換わらない）こと。
        for (name, body) in [
            ("server/data/urtect/rules.json", BUNDLED_URTECT_RULES),
            ("server/data/rules.sample.json", SAMPLE_RULES),
        ] {
            let rules = parse_rules(body);
            let rule_nodes = build_escalation_rule_nodes("urtect", &rules.escalation_rules)
                .unwrap_or_else(|e| panic!("{name}: escalation_rules must build: {e:#}"));
            let domain_nodes = build_prohibited_domain_nodes("urtect", &rules.prohibited_domains)
                .unwrap_or_else(|e| panic!("{name}: prohibited_domains must build: {e:#}"));
            assert_eq!(rule_nodes.len(), rules.escalation_rules.len(), "{name}");
            assert_eq!(domain_nodes.len(), rules.prohibited_domains.len(), "{name}");

            for (input, node) in rules.escalation_rules.iter().zip(&rule_nodes) {
                assert_eq!(
                    attribute(node, "binding"),
                    input.binding.as_deref(),
                    "{name}: rule {}",
                    input.rule_id
                );
            }
            for (input, node) in rules.prohibited_domains.iter().zip(&domain_nodes) {
                assert_eq!(
                    attribute(node, "binding"),
                    input.binding.as_deref(),
                    "{name}: domain {}",
                    input.domain_id
                );
            }
        }
    }

    // --- 未知フィールドの拒否（`deny_unknown_fields`） ---
    //
    // rules ファイルの各フィールドは判定の挙動を左右する（`hearing` を書き間違えれば本番で Jev が
    // 永久に呼ばれず、`domain_signals` を書き間違えれば禁止領域が signal で止まらなくなる）。
    // serde の既定は未知フィールドを黙って捨てるため、綴りミスが「宣言なし」として通ってしまう。
    // 投入前に、間違えたフィールド名を名指しして落とす。

    #[test]
    fn a_misspelled_field_on_an_escalation_rule_is_rejected_naming_the_field() {
        let body = json!({
            "escalation_rules": [{
                "rule_id": "warranty-failure",
                "condition": ["warranty_hardware_failure"],
                "route": "support_desk",
                "binding": "advisory",
                "hearng": "product_and_symptom",
            }],
            "prohibited_domains": [],
        })
        .to_string();
        let err = parse_rules_file(&body).expect_err("a misspelled field must be rejected");
        let chain = error_chain(&err);
        assert!(
            chain.contains("hearng"),
            "the error must name the unknown field so the operator can fix the typo, got: {chain}"
        );
        assert!(
            chain.contains("hearing"),
            "the error should list the valid fields (serde does), got: {chain}"
        );
    }

    #[test]
    fn a_misspelled_field_on_a_prohibited_domain_is_rejected_naming_the_field() {
        // `domain_signals` は `#[serde(default)]` なので、綴りミスは黙って空になりうる。
        let body = json!({
            "escalation_rules": [],
            "prohibited_domains": [{
                "domain_id": "legal-privacy",
                "domain_signal": ["legal_privacy_question"],
                "pattern": [],
                "route": "support_desk",
                "binding": "mandatory",
            }],
        })
        .to_string();
        let err = parse_rules_file(&body).expect_err("a misspelled field must be rejected");
        let chain = error_chain(&err);
        assert!(
            chain.contains("domain_signal"),
            "the error must name the unknown field, got: {chain}"
        );
    }

    #[test]
    fn an_unknown_top_level_key_is_rejected_naming_the_key() {
        let body = json!({
            "escalation_rules": [],
            "prohibited_domains": [],
            "escalation_rule": [],
        })
        .to_string();
        let err = parse_rules_file(&body).expect_err("an unknown top-level key must be rejected");
        let chain = error_chain(&err);
        assert!(
            chain.contains("escalation_rule`") || chain.contains("escalation_rule\""),
            "the error must name the unknown key, got: {chain}"
        );
    }

    // vegapunk 側が未宣言属性の書き込みを拒否しうる（`knowledge.rs` の support_case 属性の一致
    // テストと同じ障害クラス）ため、CLI が EscalationRule ノードへ書く全属性が、この CLI が
    // 登録する schema ファイル（既定は cs-schema.yml、他の ingest CLI は cs-support.yml）の
    // どちらにも宣言されていることを固定する。
    #[test]
    fn both_schema_files_declare_every_attribute_written_for_escalation_rule_nodes() {
        let rules = parse_rules(BUNDLED_URTECT_RULES);
        let nodes = build_escalation_rule_nodes("urtect", &rules.escalation_rules)
            .expect("the bundled rules.json must build into graph nodes");
        let written: std::collections::BTreeSet<&str> = nodes
            .iter()
            .flat_map(|node| node.attributes.iter().map(|(name, _)| name.as_str()))
            .collect();
        assert!(
            written.contains("hearing"),
            "test premise: hearing is written"
        );

        for relative in ["../schema/cs-schema.yml", "../schema/cs-support.yml"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read schema file {path:?}: {e}"));
            let value: serde_yaml::Value = serde_yaml::from_str(&text)
                .unwrap_or_else(|e| panic!("parse schema file {path:?} as YAML: {e}"));
            let declared: std::collections::BTreeSet<&str> = value["nodes"]["EscalationRule"]
                ["attributes"]
                .as_mapping()
                .unwrap_or_else(|| {
                    panic!("{path:?}: nodes.EscalationRule.attributes is not a mapping")
                })
                .keys()
                .filter_map(serde_yaml::Value::as_str)
                .collect();
            let missing: Vec<&&str> = written.difference(&declared).collect();
            assert!(
                missing.is_empty(),
                "{relative} nodes.EscalationRule.attributes is missing keys that ingest_rules \
                 writes: {missing:?}"
            );
        }
    }
}
