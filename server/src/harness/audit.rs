use crate::harness::scope::AccessScope;
use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 監査イベントの入力（Harness が組み立てる）。
#[derive(Debug, Clone)]
pub struct AuditDraft {
    pub request_id: String,
    pub schema: String,
    pub actor: String,
    pub used_scope: AccessScope,
    pub retrieved_node_ids: Vec<String>,
    pub decision: String,
    pub route: Option<String>,
    pub governing_norm_ids: Vec<String>,
}

/// WORM に書かれる 1 行（I5: provenance キー付き構造化レコード）。
#[derive(Debug, Serialize)]
struct AuditEvent<'a> {
    event_id: &'a str,
    timestamp: &'a str,
    request_id: &'a str,
    schema: &'a str,
    /// PunkRecord generation。Step 1 では node_id に gen prefix が含まれるため None。
    generation: Option<i64>,
    actor: &'a str,
    used_scope: &'a AccessScope,
    retrieved_node_ids: &'a [String],
    decision: &'a str,
    route: Option<&'a str>,
    governing_norm_ids: &'a [String],
    /// 将来 A / traceable_pairs へ結線するための予約（駆動は後段）。
    graph_provenance_linked: bool,
    prev_hash: &'a str,
    hash: &'a str,
}

/// 別建て WORM ストア（S1-2 / S1-8 条件 8）。append-only JSONL + hash chain。
/// 削除・更新 API は存在しない。
pub struct WormAuditLog {
    path: PathBuf,
    state: Mutex<(File, String)>, // (append-only file, prev_hash)
}

impl WormAuditLog {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create audit dir {}", parent.display()))?;
        }
        // 既存ログは全行の hash chain を検証してから継続する（fail closed）。
        // 破損・改ざん・切り詰めを黙って新チェーンで上書きしない。
        // ログは長期運用で巨大化し得るため、一括読み込みでなく 1 行ずつストリーム検証する。
        let prev_hash = match File::open(path) {
            Ok(existing) => verify_chain(std::io::BufReader::new(existing))
                .with_context(|| format!("audit log {} failed integrity check", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => genesis_hash(),
            Err(err) => {
                return Err(err).with_context(|| format!("read audit log {}", path.display()))
            }
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open audit log {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            state: Mutex::new((file, prev_hash)),
        })
    }

    /// イベントを追記し event_id を返す。
    pub fn append(&self, draft: AuditDraft) -> Result<String> {
        let event_id = uuid::Uuid::new_v4().to_string();
        let timestamp = chrono::Utc::now().to_rfc3339();
        let mut guard = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("audit log mutex poisoned"))?;
        let (file, prev_hash) = &mut *guard;
        // hash = SHA256(prev_hash + 本文 JSON) — 改竄検知用チェーン
        let payload = serde_json::json!({
            "event_id": event_id,
            "timestamp": timestamp,
            "request_id": draft.request_id,
            "schema": draft.schema,
            "generation": serde_json::Value::Null,
            "actor": draft.actor,
            "used_scope": draft.used_scope,
            "retrieved_node_ids": draft.retrieved_node_ids,
            "decision": draft.decision,
            "route": draft.route,
            "governing_norm_ids": draft.governing_norm_ids,
            "graph_provenance_linked": false,
        });
        let payload_text = serde_json::to_string(&payload)?;
        let mut hasher = Sha256::new();
        hasher.update(prev_hash.as_bytes());
        hasher.update(payload_text.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        let event = AuditEvent {
            event_id: &event_id,
            timestamp: &timestamp,
            request_id: &draft.request_id,
            schema: &draft.schema,
            generation: None,
            actor: &draft.actor,
            used_scope: &draft.used_scope,
            retrieved_node_ids: &draft.retrieved_node_ids,
            decision: &draft.decision,
            route: draft.route.as_deref(),
            governing_norm_ids: &draft.governing_norm_ids,
            graph_provenance_linked: false,
            prev_hash,
            hash: &hash,
        };
        let line = serde_json::to_string(&event)?;
        writeln!(file, "{line}")
            .with_context(|| format!("append audit log {}", self.path.display()))?;
        file.flush().context("flush audit log")?;
        *prev_hash = hash;
        Ok(event_id)
    }
}

fn genesis_hash() -> String {
    format!("{:x}", Sha256::digest(b"cs-support-mcp-worm-genesis"))
}

/// 既存ログ全行の hash chain をストリームで検証し、最後の hash を返す（空なら genesis）。
/// 1 行でも JSON 不正・チェーン断絶・hash 不一致があれば Err（fail closed）。
fn verify_chain(reader: impl std::io::BufRead) -> Result<String> {
    let mut prev = genesis_hash();
    for (index, line) in reader.lines().enumerate() {
        let line_no = index + 1;
        let line = line.with_context(|| format!("line {line_no}: read failed"))?;
        let line = line.as_str();
        if line.trim().is_empty() {
            anyhow::bail!("line {line_no}: empty line in append-only log");
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("line {line_no}: not valid json"))?;
        let serde_json::Value::Object(mut map) = value else {
            anyhow::bail!("line {line_no}: not a json object");
        };
        let line_prev = map
            .remove("prev_hash")
            .and_then(|v| v.as_str().map(ToString::to_string))
            .ok_or_else(|| anyhow::anyhow!("line {line_no}: missing prev_hash"))?;
        let line_hash = map
            .remove("hash")
            .and_then(|v| v.as_str().map(ToString::to_string))
            .ok_or_else(|| anyhow::anyhow!("line {line_no}: missing hash"))?;
        if line_prev != prev {
            anyhow::bail!("line {line_no}: hash chain is broken (prev_hash mismatch)");
        }
        // append 時の payload は serde_json::Value（キーはソート済み）を to_string したもの。
        // 行からも同じ正規形（prev_hash / hash を除いた sorted-key JSON）を再構成して照合する。
        let payload_text = serde_json::to_string(&serde_json::Value::Object(map))?;
        let mut hasher = Sha256::new();
        hasher.update(prev.as_bytes());
        hasher.update(payload_text.as_bytes());
        let expected = format!("{:x}", hasher.finalize());
        if expected != line_hash {
            anyhow::bail!("line {line_no}: hash mismatch (tampered or corrupted)");
        }
        prev = line_hash;
    }
    Ok(prev)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::scope::AccessScope;

    fn scope() -> AccessScope {
        AccessScope {
            allowed_schemas: vec!["sivira-cs-demo".to_string()],
            max_sensitivity: None,
            label_allowlist: None,
        }
    }

    fn draft(request_id: &str, decision: &str) -> AuditDraft {
        AuditDraft {
            request_id: request_id.to_string(),
            schema: "sivira-cs-demo".to_string(),
            actor: "op-001".to_string(),
            used_scope: scope(),
            retrieved_node_ids: vec!["sivira-cs-demo#gen1/section:doc-1#storage".to_string()],
            decision: decision.to_string(),
            route: None,
            governing_norm_ids: Vec::new(),
        }
    }

    #[test]
    fn append_writes_provenance_keyed_jsonl_with_hash_chain() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let log = WormAuditLog::open(&path).expect("open worm log");
        let id1 = log.append(draft("req-1", "allowed")).expect("append 1");
        let id2 = log.append(draft("req-2", "escalate")).expect("append 2");
        assert_ne!(id1, id2);

        let body = std::fs::read_to_string(&path).expect("read log");
        let lines: Vec<serde_json::Value> = body
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is json"))
            .collect();
        assert_eq!(lines.len(), 2);
        // provenance キーが構造化されている（I5、opaque blob でない）
        for line in &lines {
            for key in [
                "event_id",
                "timestamp",
                "request_id",
                "schema",
                "actor",
                "used_scope",
                "retrieved_node_ids",
                "decision",
                "governing_norm_ids",
                "graph_provenance_linked",
                "prev_hash",
                "hash",
            ] {
                assert!(line.get(key).is_some(), "missing key {key}");
            }
        }
        // hash chain: 2 行目の prev_hash は 1 行目の hash
        assert_eq!(lines[1]["prev_hash"], lines[0]["hash"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_rejects_tampered_log() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        {
            let log = WormAuditLog::open(&path).expect("open");
            log.append(draft("req-1", "allowed")).expect("append 1");
            log.append(draft("req-2", "escalate")).expect("append 2");
        }
        // 1 行目の decision を書き換える（改ざん）→ 再オープンは失敗する
        let body = std::fs::read_to_string(&path).unwrap();
        let tampered = body.replacen("\"decision\":\"allowed\"", "\"decision\":\"denied!\"", 1);
        assert_ne!(body, tampered, "tamper must change the file");
        std::fs::write(&path, tampered).unwrap();
        assert!(WormAuditLog::open(&path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_rejects_corrupted_tail() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        {
            let log = WormAuditLog::open(&path).expect("open");
            log.append(draft("req-1", "allowed")).expect("append");
        }
        // 途中で切れた行（クラッシュ・破損相当）→ genesis に黙って戻らず失敗する
        let mut body = std::fs::read_to_string(&path).unwrap();
        body.push_str("{\"broken\":");
        std::fs::write(&path, body).unwrap();
        assert!(WormAuditLog::open(&path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_continues_hash_chain() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let first_hash;
        {
            let log = WormAuditLog::open(&path).expect("open");
            log.append(draft("req-1", "allowed")).expect("append");
            let body = std::fs::read_to_string(&path).unwrap();
            let v: serde_json::Value = serde_json::from_str(body.lines().last().unwrap()).unwrap();
            first_hash = v["hash"].as_str().unwrap().to_string();
        }
        // 再オープン（プロセス再起動相当）でもチェーンが繋がる
        let log = WormAuditLog::open(&path).expect("reopen");
        log.append(draft("req-2", "escalate")).expect("append");
        let body = std::fs::read_to_string(&path).unwrap();
        let last: serde_json::Value = serde_json::from_str(body.lines().last().unwrap()).unwrap();
        assert_eq!(last["prev_hash"].as_str().unwrap(), first_hash);
        std::fs::remove_dir_all(&dir).ok();
    }
}
