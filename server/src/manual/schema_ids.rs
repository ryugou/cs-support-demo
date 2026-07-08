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
