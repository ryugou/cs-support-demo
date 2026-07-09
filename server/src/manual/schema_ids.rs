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

/// テンプレ YAML 先頭の `name:` をテナント schema 名に差し替える。
/// vegapunk は create_schema 時に YAML の name と登録名の一致を要求するため、
/// 1 つの汎用テンプレを複数テナント（schema）に登録するにはこの差し替えが要る。
pub fn with_schema_name(yaml: &str, schema: &str) -> String {
    let mut out = String::with_capacity(yaml.len() + schema.len());
    let mut replaced = false;
    for line in yaml.lines() {
        if !replaced
            && line.trim_start().starts_with("name:")
            && !line.starts_with(char::is_whitespace)
        {
            out.push_str("name: ");
            out.push_str(schema);
            replaced = true;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
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
        let out = with_schema_name(yaml, "urtect");
        assert!(out.starts_with("name: urtect\n"));
        // ネストした `name:`（インデント付き）は変えない
        assert!(out.contains("      name: { type: string }"));
        // top-level name は 1 つだけ差し替わる
        assert_eq!(out.matches("name: urtect").count(), 1);
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
