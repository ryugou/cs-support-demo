pub const KIND_DOC: &str = "ManualDocument";
pub const KIND_SECTION: &str = "ManualSection";
pub const KIND_PRODUCT: &str = "Product";

/// vegapunk read API は generation prefix でスコープする。新規スキーマは gen1 始まり。
pub fn manual_node_id(schema: &str, kind: &str, key: &str) -> String {
    format!("{schema}:gen1:{kind}:{key}")
}

/// URL 末尾セグメントから安定 slug（再 ingest の冪等 upsert キー）。
pub fn section_slug(url: &str) -> String {
    let tail = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_lowercase();
    let kebab: String = tail
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let squeezed = kebab
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    format!("sec-{squeezed}")
}

/// テンプレ YAML のトップレベル `name` をテナント schema 名に差し替える。
/// vegapunk は create_schema 時に YAML の name と登録名の一致を要求するため、
/// 1 つの汎用テンプレを複数テナント（schema）に登録するにはこの差し替えが要る。
/// 文字列パッチでなく YAML として parse/set/serialize する（quoted 形式・flow style 等に頑健。
/// コメントは登録用コピーからは落ちるが、リポジトリのテンプレファイル自体は変更しない）。
pub fn with_schema_name(yaml: &str, schema: &str) -> anyhow::Result<String> {
    let mut value: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|e| anyhow::anyhow!("parse schema template: {e}"))?;
    let mapping = value
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("schema template root is not a mapping"))?;
    mapping.insert(
        serde_yaml::Value::String("name".to_string()),
        serde_yaml::Value::String(schema.to_string()),
    );
    serde_yaml::to_string(&value).map_err(|e| anyhow::anyhow!("serialize schema template: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn node_id_has_generation_prefix() {
        assert_eq!(
            manual_node_id("urtect", KIND_SECTION, "sec-1-4-sd"),
            "urtect:gen1:ManualSection:sec-1-4-sd"
        );
    }
    #[test]
    fn with_schema_name_replaces_only_top_level_name() {
        let yaml = "name: cs-support-manual\nversion: 1\nnodes:\n  Product:\n    attributes:\n      name: { type: string }\n";
        let out = with_schema_name(yaml, "urtect").unwrap();
        // 構造で検証する（再シリアライズで表記スタイルは変わりうるため）
        let v: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(v["name"].as_str(), Some("urtect"));
        // ネストした `name:` は変えない
        assert_eq!(
            v["nodes"]["Product"]["attributes"]["name"]["type"].as_str(),
            Some("string")
        );
        assert_eq!(v["version"].as_i64(), Some(1));
    }
    #[test]
    fn slug_from_url_tail_is_stable_and_ascii_kebab() {
        assert_eq!(
            section_slug("https://sites.google.com/view/urtect-manual/1-4/sd-not-recognized"),
            "sec-sd-not-recognized"
        );
        // 同一 URL は同一 slug（冪等キー）
        assert_eq!(
            section_slug("https://x/a/b/"),
            section_slug("https://x/a/b")
        );
    }
}
