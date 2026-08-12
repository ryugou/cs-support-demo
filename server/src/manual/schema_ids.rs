pub const KIND_DOC: &str = "ManualDocument";
pub const KIND_SECTION: &str = "ManualSection";
pub const KIND_PRODUCT: &str = "Product";
/// Issue #8 v2: answers.alarm.com の概念クエリ・記事横断 join の拠り所。
pub const KIND_CONCEPT: &str = "Concept";

/// vegapunk read API は generation prefix でスコープする。新規スキーマは gen1 始まり。
pub fn manual_node_id(schema: &str, kind: &str, key: &str) -> String {
    format!("{schema}:gen1:{kind}:{key}")
}

/// URL の全パスセグメントから安定 slug（再 ingest の冪等 upsert キー）。
///
/// 末尾セグメントだけを使うと、異なる階層下に同名の末尾セグメントを持つページ
/// （例: `/setup/sd-card` と `/troubleshooting/sd-card`）が同一 slug に衝突し、
/// 一方が他方を upsert で上書きしてしまう。これを防ぐため、ホスト以降のパス全体を
/// 対象に kebab 正規化する（`/` は他の非英数字と同様に区切りとして `-` に落ちるため、
/// パスをセグメント単位でループする必要はない）。
///
/// 注意（upsert キー破壊的変更）: この slug 方式を変更すると upsert キーが変わる。
/// 既存テナントへの再 ingest 後は旧キーのノードが孤児として残る（backend に公開 delete が
/// 無いため）。クリーンな再 ingest が必要な運用では、必ずスキーマ/テナントを作り直すこと。
pub fn section_slug(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let path_only = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    // ホスト部を落とし、残りのパス全体（複数セグメント）を対象にする。
    let path = path_only.split_once('/').map(|(_, p)| p).unwrap_or("");
    let kebab: String = path
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let squeezed = kebab
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    // パスが空 / 非 ASCII のみで kebab 化が全て落ちた場合、"sec-" が衝突・不安定キーになる。
    // 決定論フォールバックとして URL 全体の sha256 先頭 12 hex を使う（同一 URL → 同一 slug）。
    if squeezed.is_empty() {
        use sha2::{Digest, Sha256};
        let digest = format!("{:x}", Sha256::digest(trimmed.as_bytes()));
        return format!("sec-{}", &digest[..12]);
    }
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
    fn slug_from_full_url_path_is_stable_and_ascii_kebab() {
        assert_eq!(
            section_slug("https://sites.google.com/view/urtect-manual/1-4/sd-not-recognized"),
            "sec-view-urtect-manual-1-4-sd-not-recognized"
        );
        // 同一 URL は同一 slug（冪等キー）
        assert_eq!(
            section_slug("https://x/a/b/"),
            section_slug("https://x/a/b")
        );
    }

    #[test]
    fn empty_or_non_ascii_path_gets_deterministic_hash_slug() {
        // パスなし・非 ASCII のみのパスでも "sec-" 単独にならず、URL ごとに一意で安定
        let a = section_slug("https://x/");
        let b = section_slug("https://x/日本語のみ");
        let c = section_slug("https://x/日本語のみ");
        assert_ne!(a, "sec-");
        assert_ne!(b, "sec-");
        assert_ne!(a, b);
        assert_eq!(b, c); // 決定論
        assert!(b.starts_with("sec-"));
    }

    #[test]
    fn slug_does_not_collide_across_different_parent_paths() {
        // 末尾セグメントだけを見ると "sd-card" 同士で衝突していた（Critical fix）。
        // 全パスセグメントを使うことで親パスが異なれば slug も異なる。
        assert_ne!(
            section_slug("https://x/setup/sd-card"),
            section_slug("https://x/troubleshooting/sd-card")
        );
    }
}
